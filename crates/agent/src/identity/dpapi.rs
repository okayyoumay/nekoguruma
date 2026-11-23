//! The Windows key store's operating-system calls (design 6.4, ADR-459 Decision item 4,
//! ADR-507): DPAPI for the encryption and `GetProfileType` for the kind of profile. The policy
//! is [`BlobStore`] in `blob_store`, which this module feeds with [`Dpapi`] and [`Profile`].
//!
//! DPAPI is called with `CRYPTPROTECT_UI_FORBIDDEN` and no other flag, so a blob belongs to the
//! calling account and not to the machine, and a call that would need a prompt fails instead.
//! No description and no prompt structure are passed. The buffer DPAPI allocates for a result is
//! owned by a guard that wipes it before it frees it, and a decrypted secret is copied once, to a
//! buffer of exactly its size that is wiped when dropped.

use std::io;
use std::path::PathBuf;
use std::ptr;
use std::slice;
use std::sync::atomic::{Ordering, compiler_fence};

use shared_crypto::Zeroizing;
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, LocalFree};
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows_sys::Win32::UI::Shell::GetProfileType;

use super::blob_store::{BlobStore, OsCode, ProfileProbe, Protect};

/// The store over the account's DPAPI and profile, for the state root `root`.
pub type DpapiStore = BlobStore<Dpapi, Profile>;

/// The Windows key store whose files are in `<root>/identity`.
pub fn open(root: PathBuf) -> DpapiStore {
    BlobStore::new(root, Dpapi, Profile)
}

/// `CryptProtectData` and `CryptUnprotectData` under the calling account.
#[derive(Debug)]
pub struct Dpapi;

/// `GetProfileType` of the calling account.
#[derive(Debug)]
pub struct Profile;

/// The error of the call that just failed on this thread.
fn last_error() -> OsCode {
    let code = io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or_default();
    OsCode(u32::from_ne_bytes(code.to_ne_bytes()))
}

/// `bytes` as the input structure DPAPI takes, or a refusal when it is too long to describe.
fn input(bytes: &[u8]) -> Result<CRYPT_INTEGER_BLOB, OsCode> {
    Ok(CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len()).map_err(|_| OsCode(ERROR_INVALID_PARAMETER))?,
        pbData: bytes.as_ptr().cast_mut(),
    })
}

/// The buffer DPAPI allocates for a result. Dropping it wipes the bytes and frees the memory.
struct OsBuffer(CRYPT_INTEGER_BLOB);

impl OsBuffer {
    fn empty() -> Self {
        Self(CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        })
    }

    #[expect(
        unsafe_code,
        reason = "views the buffer DPAPI allocated: pointer and length come from a successful call (ADR-502)"
    )]
    fn bytes(&self) -> &[u8] {
        if self.0.pbData.is_null() {
            return &[];
        }
        // SAFETY: after a successful DPAPI call `pbData` points to `cbData` initialized bytes
        // that this guard owns until it is dropped.
        unsafe { slice::from_raw_parts(self.0.pbData, self.0.cbData as usize) }
    }
}

impl Drop for OsBuffer {
    #[expect(
        unsafe_code,
        reason = "wipes and frees the buffer DPAPI allocated with LocalAlloc (ADR-502)"
    )]
    fn drop(&mut self) {
        if self.0.pbData.is_null() {
            return;
        }
        // SAFETY: `pbData` is a `LocalAlloc` allocation of `cbData` bytes that nothing else
        // references; it is written before it is freed, once.
        unsafe {
            for offset in 0..self.0.cbData as usize {
                self.0.pbData.add(offset).write_volatile(0);
            }
            compiler_fence(Ordering::SeqCst);
            LocalFree(self.0.pbData.cast());
        }
    }
}

#[expect(
    unsafe_code,
    reason = "CryptProtectData is a Win32 call with no safe wrapper in std (ADR-502)"
)]
fn protect_call(
    plain: &CRYPT_INTEGER_BLOB,
    entropy: &CRYPT_INTEGER_BLOB,
    out: &mut OsBuffer,
) -> bool {
    // SAFETY: both input structures describe slices that outlive the call; the description,
    // reserved and prompt arguments are null as the API allows; `out` receives an allocation it
    // then owns.
    unsafe {
        CryptProtectData(
            plain,
            ptr::null(),
            entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out.0,
        ) != 0
    }
}

#[expect(
    unsafe_code,
    reason = "CryptUnprotectData is a Win32 call with no safe wrapper in std (ADR-502)"
)]
fn unprotect_call(
    blob: &CRYPT_INTEGER_BLOB,
    entropy: &CRYPT_INTEGER_BLOB,
    out: &mut OsBuffer,
) -> bool {
    // SAFETY: as in `protect_call`; the description output is null, so none is allocated.
    unsafe {
        CryptUnprotectData(
            blob,
            ptr::null_mut(),
            entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out.0,
        ) != 0
    }
}

impl Protect for Dpapi {
    fn protect(&self, plain: &[u8], entropy: &[u8]) -> Result<Vec<u8>, OsCode> {
        let (plain, entropy) = (input(plain)?, input(entropy)?);
        let mut out = OsBuffer::empty();
        if !protect_call(&plain, &entropy, &mut out) {
            return Err(last_error());
        }
        Ok(out.bytes().to_vec())
    }

    fn unprotect(&self, blob: &[u8], entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>, OsCode> {
        let (blob, entropy) = (input(blob)?, input(entropy)?);
        let mut out = OsBuffer::empty();
        if !unprotect_call(&blob, &entropy, &mut out) {
            return Err(last_error());
        }
        let secret = out.bytes();
        // Exactly the secret's size, so the copy never reallocates and leaves no stray copy.
        let mut copy = Zeroizing::new(Vec::with_capacity(secret.len()));
        copy.extend_from_slice(secret);
        Ok(copy)
    }
}

#[expect(
    unsafe_code,
    reason = "GetProfileType is a Win32 call with no safe wrapper in std (ADR-502)"
)]
fn profile_type(flags: &mut u32) -> bool {
    // SAFETY: `flags` is a valid pointer to a `u32` for the call.
    unsafe { GetProfileType(flags) != 0 }
}

impl ProfileProbe for Profile {
    fn profile_flags(&self) -> Result<u32, OsCode> {
        let mut flags = 0;
        if profile_type(&mut flags) {
            Ok(flags)
        } else {
            Err(last_error())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use worker_host::service::OperatingMode;

    use super::*;
    use crate::identity::blob_store::UNDECRYPTABLE_CODES;
    use crate::identity::store::{SecretName, SecretStore, StoreErrorKind};

    fn name() -> SecretName {
        SecretName::agent_key(OperatingMode::User)
    }

    /// A state root in a temporary directory, with its `identity` directory made.
    fn state() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let dir = root.join("identity");
        fs::create_dir_all(&dir).unwrap();
        (temp, root, dir)
    }

    fn file(dir: &Path, name: &SecretName) -> PathBuf {
        dir.join(format!("{name}.dpapi"))
    }

    fn kind<T: std::fmt::Debug>(
        result: Result<T, crate::identity::store::StoreError>,
    ) -> StoreErrorKind {
        result.unwrap_err().kind()
    }

    /// The real DPAPI round trip: the secret comes back, the entropy is part of the binding and
    /// the refusal for a wrong one is a code the store reads as undecryptable.
    #[test]
    fn dpapi_round_trips_and_binds_the_entropy() {
        let blob = Dpapi.protect(b"secret", b"entropy").unwrap();
        assert!(!blob.windows(6).any(|window| window == b"secret"));
        assert_eq!(&**Dpapi.unprotect(&blob, b"entropy").unwrap(), b"secret");
        let refused = Dpapi.unprotect(&blob, b"another").unwrap_err();
        assert!(
            UNDECRYPTABLE_CODES.contains(&refused.0),
            "wrong entropy gave {:#x}",
            refused.0
        );
        let mut tampered = blob;
        let middle = tampered.len() / 2;
        tampered[middle] ^= 1;
        let refused = Dpapi.unprotect(&tampered, b"entropy").unwrap_err();
        assert!(
            UNDECRYPTABLE_CODES.contains(&refused.0),
            "a changed blob gave {:#x}",
            refused.0
        );
    }

    /// The account running the tests has a profile the store accepts.
    #[test]
    fn the_test_account_has_a_persistent_profile() {
        let flags = Profile.profile_flags().unwrap();
        assert_eq!(flags & 0x5, 0, "{flags:#x}");
    }

    /// Created, read by a store made afterwards, never replaced; the file starts with the magic
    /// and the format byte and does not hold the secret.
    #[test]
    fn a_key_is_created_once_and_read_back_through_dpapi() {
        let (_temp, root, dir) = state();
        let store = open(root.clone());
        assert_eq!(kind(store.get(&name())), StoreErrorKind::Absent);
        let secret = b"a secret of some length to look for";
        store.put_new(&name(), secret).unwrap();
        assert_eq!(&**open(root).get(&name()).unwrap(), secret);
        assert_eq!(
            kind(store.put_new(&name(), b"another")),
            StoreErrorKind::AlreadyExists
        );
        let bytes = fs::read(file(&dir, &name())).unwrap();
        assert_eq!(&bytes[..9], b"NGRDPAPI\x01");
        assert!(!bytes.windows(secret.len()).any(|window| window == secret));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    /// A changed byte and a blob copied to another name are undecryptable, and the file is not
    /// touched.
    #[test]
    fn a_changed_or_copied_blob_is_undecryptable() {
        let (_temp, root, dir) = state();
        let store = open(root);
        store.put_new(&name(), b"secret").unwrap();
        let original = fs::read(file(&dir, &name())).unwrap();
        let other = SecretName::agent_key(OperatingMode::Device);
        fs::write(file(&dir, &other), &original).unwrap();
        assert_eq!(kind(store.get(&other)), StoreErrorKind::Undecryptable);
        assert_eq!(fs::read(file(&dir, &other)).unwrap(), original);
        let mut tampered = original;
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        fs::write(file(&dir, &name()), &tampered).unwrap();
        assert_eq!(kind(store.get(&name())), StoreErrorKind::Undecryptable);
        assert_eq!(fs::read(file(&dir, &name())).unwrap(), tampered);
    }

    /// A symbolic link at the name is not followed. Creating one needs a privilege the account
    /// may lack (error 1314), in which case the test has nothing to check.
    #[test]
    fn a_symbolic_link_at_the_name_is_unavailable() {
        let (temp, root, dir) = state();
        let target = temp.path().join("target");
        fs::write(&target, b"NGRDPAPI\x01whatever").unwrap();
        if let Err(error) = std::os::windows::fs::symlink_file(&target, file(&dir, &name())) {
            assert_eq!(error.raw_os_error(), Some(1314), "{error}");
            return;
        }
        assert_eq!(kind(open(root).get(&name())), StoreErrorKind::Unavailable);
    }
}
