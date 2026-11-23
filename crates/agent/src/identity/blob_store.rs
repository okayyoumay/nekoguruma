//! The Windows key store's policy: the agent key as a file holding a DPAPI blob (design 6.4,
//! ADR-459 Decision item 4, ADR-507).
//!
//! [`BlobStore`] implements [`SecretStore`] over two seams, [`Protect`] (the encryption of a
//! secret under the account's DPAPI master key, `dpapi::Dpapi` on Windows) and [`ProfileProbe`]
//! (the kind of the account's profile, `dpapi::Profile`). Both are traits so that this module,
//! which is plain `std`, compiles and is tested on every platform with fakes.
//!
//! The file is `<root>/identity/<name>.dpapi`: the 8 bytes `NGRDPAPI`, one format byte (1), then
//! the DPAPI blob to the end of the file. [`MAX_LEN`] bounds what is read. The blob is bound to
//! the secret's name through the entropy [`entropy`] builds, a domain separator and not a
//! secret, so a blob copied to another name or made by another program does not open.
//!
//! Reading judges the file from an open handle that does not follow a reparse point
//! (`guards::open_read_only`):
//!
//! | What is found | Answer |
//! |---|---|
//! | no file, the root's parent is a directory, the profile is local or roaming | absent |
//! | no file otherwise (parent missing, profile temporary or mandatory, profile unreadable) | unavailable |
//! | a link, a directory, any other I/O error | unavailable |
//! | longer than [`MAX_LEN`], shorter than a header and a blob, wrong magic, unknown format | undecryptable |
//! | the operating system refuses the blob with a code in [`UNDECRYPTABLE_CODES`] | undecryptable |
//! | the operating system refuses the blob with any other code | unavailable |
//!
//! A profile that is temporary or mandatory is thrown away at logoff, so a key made in one would
//! be lost: the profile gates the absent answer and the creation, never a key that is present.
//! Creation (`put_new`) checks the profile first, protects the secret, then creates the file with
//! `journal::create_linked`: written and synced under `<name>.dpapi.tmp`, hard-linked into place
//! without replacing and the temporary removed. A crash leaves at most the temporary, which reads
//! as absent and is removed by the next create. Error sources are fixed text, an
//! [`io::ErrorKind`], or fixed text with an operating-system code in hexadecimal; never a path or
//! bytes.

use std::io::{self, Read};
use std::path::PathBuf;

use shared_crypto::Zeroizing;

use super::store::{SecretName, SecretStore, StoreError, StoreErrorKind};
use crate::guards::{GuardError, open_read_only};
use crate::journal::{CreateLinkedError, create_linked};

/// The longest file the store reads: a P-256 PKCS#8 key sealed by DPAPI is a few hundred bytes,
/// so anything near this is not the agent's key.
pub const MAX_LEN: usize = 16 * 1024;

/// The file's first 8 bytes.
const MAGIC: &[u8; 8] = b"NGRDPAPI";

/// The format byte after the magic.
const FORMAT: u8 = 1;

/// The bytes before the blob.
const HEADER_LEN: usize = MAGIC.len() + 1;

/// The start of the entropy; the secret's name follows it.
const ENTROPY_PREFIX: &[u8] = b"nekoguruma-secret-store-v1\0";

/// `GetProfileType`'s flag for a profile deleted at logoff.
pub const PT_TEMPORARY: u32 = 0x1;

/// `GetProfileType`'s flag for a profile that cannot be changed by its user.
pub const PT_MANDATORY: u32 = 0x4;

/// Win32 `ERROR_INVALID_DATA`: the blob is not a valid one for this account and entropy.
pub const ERROR_INVALID_DATA: u32 = 13;

/// `NTE_BAD_KEY_STATE`: the master key that protected the blob is not usable by this account.
pub const NTE_BAD_KEY_STATE: u32 = 0x8009_000B;

/// The operating-system codes that mean "this blob cannot be opened by this account", not "the
/// service is unreachable". A code outside the list is unavailable, so an unknown failure is
/// retried and never reported as a lost key.
pub const UNDECRYPTABLE_CODES: [u32; 2] = [ERROR_INVALID_DATA, NTE_BAD_KEY_STATE];

/// An operating-system error code (a Win32 error or an `NTE_` status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsCode(pub u32);

/// The encryption of a secret under the account's key.
pub trait Protect: Send + Sync {
    /// The blob that `unprotect` opens for the same `entropy` and account.
    fn protect(&self, plain: &[u8], entropy: &[u8]) -> Result<Vec<u8>, OsCode>;

    /// The secret in `blob`, in a buffer that is wiped when dropped.
    fn unprotect(&self, blob: &[u8], entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>, OsCode>;
}

/// The kind of the account's profile.
pub trait ProfileProbe: Send + Sync {
    /// The `GetProfileType` flags, or the code of the failure to read them.
    fn profile_flags(&self) -> Result<u32, OsCode>;
}

/// The key store over the blob files in `<root>/identity`.
#[derive(Debug)]
pub struct BlobStore<P, F> {
    root: PathBuf,
    protect: P,
    profile: F,
}

impl<P: Protect, F: ProfileProbe> BlobStore<P, F> {
    /// A store whose files are in `<root>/identity`. It resolves nothing and makes nothing: the
    /// directory is made by the caller (`state::private_subdir`) before the first create.
    pub fn new(root: PathBuf, protect: P, profile: F) -> Self {
        Self {
            root,
            protect,
            profile,
        }
    }

    fn path(&self, name: &SecretName) -> PathBuf {
        self.root.join("identity").join(format!("{name}.dpapi"))
    }

    /// Fails unless the profile is one whose files survive the account's next logoff.
    fn require_persistent_profile(&self) -> Result<(), StoreError> {
        match self.profile.profile_flags() {
            Ok(flags) if flags & (PT_TEMPORARY | PT_MANDATORY) == 0 => Ok(()),
            Ok(_) => Err(unavailable(
                "the account's profile is temporary or mandatory",
            )),
            Err(code) => Err(os_unavailable(
                "the account's profile type cannot be read",
                code,
            )),
        }
    }

    /// The answer for a name with no file: absent only when the state's parent directory is
    /// there (a first run) and the profile is persistent; a missing parent is a drive or profile
    /// that is not mounted.
    fn answer_for_no_file(&self) -> StoreError {
        let parent_is_dir = self
            .root
            .parent()
            .is_some_and(|parent| parent.metadata().is_ok_and(|meta| meta.is_dir()));
        if !parent_is_dir {
            return unavailable("the directory that holds the state is not there");
        }
        match self.require_persistent_profile() {
            Ok(()) => StoreError::new(StoreErrorKind::Absent),
            Err(error) => error,
        }
    }
}

impl<P: Protect, F: ProfileProbe> SecretStore for BlobStore<P, F> {
    fn get(&self, name: &SecretName) -> Result<Zeroizing<Vec<u8>>, StoreError> {
        let file = match open_read_only(&self.path(name)) {
            Ok(file) => file,
            Err(GuardError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Err(self.answer_for_no_file());
            }
            Err(GuardError::Io(error)) => return Err(io_unavailable(&error)),
            Err(_) => return Err(unavailable("the key file is not a regular file")),
        };
        // One byte more than the cap, so a longer file shows.
        let mut bytes = Vec::with_capacity(MAX_LEN + 1);
        file.take(MAX_LEN as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| io_unavailable(&error))?;
        let blob = blob_of(&bytes)?;
        self.protect
            .unprotect(blob, &entropy(name))
            .map_err(|code| {
                let kind = if UNDECRYPTABLE_CODES.contains(&code.0) {
                    StoreErrorKind::Undecryptable
                } else {
                    StoreErrorKind::Unavailable
                };
                StoreError::with_source(
                    kind,
                    code_text("the operating system refused the blob", code),
                )
            })
    }

    /// Creates the key file without replacing one. Callers hold the creation lock
    /// (`<root>\identity\<name>.lock`): unlocked creators share one temporary name, so one could
    /// link a file another is still writing.
    fn put_new(&self, name: &SecretName, secret: &[u8]) -> Result<(), StoreError> {
        self.require_persistent_profile()?;
        let blob = self
            .protect
            .protect(secret, &entropy(name))
            .map_err(|code| {
                os_unavailable("the operating system could not protect the key", code)
            })?;
        let mut contents = Vec::with_capacity(HEADER_LEN + blob.len());
        contents.extend_from_slice(MAGIC);
        contents.push(FORMAT);
        contents.extend_from_slice(&blob);
        let path = self.path(name);
        let dir = self.root.join("identity");
        let tmp = path.with_extension("dpapi.tmp");
        match create_linked(&dir, &path, &tmp, &contents, std::fs::OpenOptions::new()) {
            Ok(_) => Ok(()),
            Err(CreateLinkedError::AlreadyExists) => {
                Err(StoreError::new(StoreErrorKind::AlreadyExists))
            }
            Err(CreateLinkedError::Io(error)) => Err(io_unavailable(&error)),
        }
    }
}

/// The DPAPI blob of a file's bytes, or the reason the file is no key file.
fn blob_of(bytes: &[u8]) -> Result<&[u8], StoreError> {
    let undecryptable =
        |why: &'static str| StoreError::with_source(StoreErrorKind::Undecryptable, why);
    if bytes.len() > MAX_LEN {
        return Err(undecryptable("the key file is longer than a key can be"));
    }
    let Some((header, blob)) = bytes.split_at_checked(HEADER_LEN) else {
        return Err(undecryptable("the key file is shorter than its header"));
    };
    if &header[..MAGIC.len()] != MAGIC {
        return Err(undecryptable("the key file does not start with the magic"));
    }
    if header[MAGIC.len()] != FORMAT {
        return Err(undecryptable("the key file has an unknown format"));
    }
    if blob.is_empty() {
        return Err(undecryptable("the key file holds no blob"));
    }
    Ok(blob)
}

/// What binds a blob to its name: the store's domain separator and the name.
fn entropy(name: &SecretName) -> Vec<u8> {
    let mut entropy = Vec::with_capacity(ENTROPY_PREFIX.len() + name.as_str().len());
    entropy.extend_from_slice(ENTROPY_PREFIX);
    entropy.extend_from_slice(name.as_str().as_bytes());
    entropy
}

fn unavailable(reason: &'static str) -> StoreError {
    StoreError::with_source(StoreErrorKind::Unavailable, reason)
}

fn io_unavailable(error: &io::Error) -> StoreError {
    StoreError::with_source(StoreErrorKind::Unavailable, format!("{:?}", error.kind()))
}

fn os_unavailable(what: &str, code: OsCode) -> StoreError {
    StoreError::with_source(StoreErrorKind::Unavailable, code_text(what, code))
}

fn code_text(what: &str, code: OsCode) -> String {
    format!("{what} (code {:#010x})", code.0)
}

#[cfg(test)]
mod tests;
