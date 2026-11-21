//! The Linux device-mode key store: the agent key as a systemd credential (design 6.4, ADR-459
//! Decision item 4, ADR-498).
//!
//! The administrator provisions the key (`ngr-agent provision-key`, [`super::provision`]) as an
//! encrypted file, which the unit's `LoadCredentialEncrypted=` decrypts for the service before it
//! starts and exposes as a file in the directory `$CREDENTIALS_DIRECTORY` names. This store reads
//! that one file, never anything else, and never writes: a key the agent could create itself
//! would be one the administrator cannot account for (ADR-498 item 2).
//!
//! The file is opened without following a symlink and without blocking on a FIFO, and must be a
//! regular file. A missing file in an existing directory is the only absent answer; a missing
//! directory, a refused open and every other I/O error are unavailable. A file longer than
//! [`MAX_LEN`] is not a key and is undecryptable. Error sources are fixed text or an
//! [`std::io::ErrorKind`], never a path or bytes.

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use shared_crypto::Zeroizing;

use super::DEVICE_CREDENTIAL_NAME;
use super::store::{SecretName, SecretStore, StoreError, StoreErrorKind};

/// The longest credential file the store reads: a P-256 PKCS#8 key is about 140 bytes, so
/// anything near this is not the agent's key.
pub const MAX_LEN: usize = 16 * 1024;

/// The store over the credential directory `dir`.
pub struct SystemdCredential {
    dir: PathBuf,
}

impl SystemdCredential {
    /// A store reading `dir`, the absolute directory `$CREDENTIALS_DIRECTORY` names.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

fn unavailable(reason: &'static str) -> StoreError {
    StoreError::with_source(StoreErrorKind::Unavailable, reason)
}

fn io_unavailable(error: &std::io::Error) -> StoreError {
    StoreError::with_source(StoreErrorKind::Unavailable, format!("{:?}", error.kind()))
}

impl SecretStore for SystemdCredential {
    fn get(&self, name: &SecretName) -> Result<Zeroizing<Vec<u8>>, StoreError> {
        // The store holds the one credential the unit provides and no other name.
        if name.as_str() != DEVICE_CREDENTIAL_NAME {
            return Err(StoreError::new(StoreErrorKind::Absent));
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
        let file = match options.open(self.dir.join(name.as_str())) {
            Ok(file) => file,
            // Absent only when the directory is there: a missing directory means the unit
            // passed no credentials at all.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return match fs::metadata(&self.dir) {
                    Ok(meta) if meta.is_dir() => Err(StoreError::new(StoreErrorKind::Absent)),
                    Ok(_) => Err(unavailable("the credentials directory is not a directory")),
                    Err(error) => Err(io_unavailable(&error)),
                };
            }
            Err(error) => return Err(io_unavailable(&error)),
        };
        let meta = file.metadata().map_err(|error| io_unavailable(&error))?;
        if !meta.file_type().is_file() {
            return Err(unavailable("the credential is not a regular file"));
        }
        // Room for the cap and one byte more, so the read never reallocates (a copy of the key
        // would stay behind) and a longer file shows.
        let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_LEN + 1));
        file.take(MAX_LEN as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| io_unavailable(&error))?;
        if bytes.len() > MAX_LEN {
            return Err(StoreError::with_source(
                StoreErrorKind::Undecryptable,
                "the credential is longer than a key can be",
            ));
        }
        Ok(bytes)
    }

    fn put_new(&self, _name: &SecretName, _secret: &[u8]) -> Result<(), StoreError> {
        Err(StoreError::new(StoreErrorKind::NotCreatable))
    }

    fn can_create(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use worker_host::service::OperatingMode;

    use super::*;

    fn name() -> SecretName {
        SecretName::agent_key(OperatingMode::Device)
    }

    fn kind_of(store: &SystemdCredential) -> StoreErrorKind {
        store.get(&name()).unwrap_err().kind()
    }

    fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// T1 (A3, A4): an empty directory is absent; a planted file is read as it is; a name
    /// other than the credential's is absent and reads nothing.
    #[test]
    fn absent_then_present() {
        let temp = tempfile::tempdir().unwrap();
        let store = SystemdCredential::new(temp.path().to_owned());
        assert_eq!(kind_of(&store), StoreErrorKind::Absent);
        fs::write(temp.path().join(DEVICE_CREDENTIAL_NAME), b"bytes").unwrap();
        assert_eq!(&**store.get(&name()).unwrap(), b"bytes");
        fs::write(temp.path().join("other"), b"x").unwrap();
        let other = SecretName::new("other").unwrap();
        assert_eq!(
            store.get(&other).unwrap_err().kind(),
            StoreErrorKind::Absent
        );
    }

    /// An empty file is returned empty (the key decoder then refuses it, A5); one of exactly the
    /// cap is read, and one byte over is undecryptable (A7).
    #[test]
    fn empty_cap_and_over_the_cap() {
        let temp = tempfile::tempdir().unwrap();
        let store = SystemdCredential::new(temp.path().to_owned());
        let file = temp.path().join(DEVICE_CREDENTIAL_NAME);
        fs::write(&file, b"").unwrap();
        assert!(store.get(&name()).unwrap().is_empty());
        fs::write(&file, vec![7u8; MAX_LEN]).unwrap();
        assert_eq!(store.get(&name()).unwrap().len(), MAX_LEN);
        fs::write(&file, vec![7u8; MAX_LEN + 1]).unwrap();
        let error = store.get(&name()).unwrap_err();
        assert_eq!(error.kind(), StoreErrorKind::Undecryptable);
        assert!(!error.to_string().contains(temp.path().to_str().unwrap()));
    }

    /// A2: a missing directory, and a regular file where the directory belongs, are unavailable,
    /// never absent.
    #[test]
    fn a_missing_or_wrong_directory_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let missing = SystemdCredential::new(temp.path().join("gone"));
        assert_eq!(kind_of(&missing), StoreErrorKind::Unavailable);
        let file = temp.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert_eq!(
            kind_of(&SystemdCredential::new(file)),
            StoreErrorKind::Unavailable
        );
    }

    /// A8: a file the account cannot read is unavailable (skipped as root, for whom mode bits
    /// do not apply).
    #[test]
    fn an_unreadable_file_is_unavailable() {
        if is_root() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join(DEVICE_CREDENTIAL_NAME);
        fs::write(&file, b"key").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        let store = SystemdCredential::new(temp.path().to_owned());
        let error = store.get(&name()).unwrap_err();
        assert_eq!(error.kind(), StoreErrorKind::Unavailable);
        assert!(error.to_string().contains("PermissionDenied"));
    }

    /// A9: a symlink is not followed, to a valid file or to nothing.
    #[test]
    fn a_symlink_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("creds");
        fs::create_dir(&dir).unwrap();
        let target = temp.path().join("real");
        fs::write(&target, b"valid").unwrap();
        let link = dir.join(DEVICE_CREDENTIAL_NAME);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let store = SystemdCredential::new(dir.clone());
        assert_eq!(kind_of(&store), StoreErrorKind::Unavailable);
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(temp.path().join("nothing"), &link).unwrap();
        assert_eq!(kind_of(&store), StoreErrorKind::Unavailable);
    }

    /// A10: a directory or a FIFO at the name is unavailable, and the FIFO open does not hang.
    #[test]
    fn a_directory_or_a_fifo_is_unavailable_without_hanging() {
        let temp = tempfile::tempdir().unwrap();
        let store = SystemdCredential::new(temp.path().to_owned());
        let path = temp.path().join(DEVICE_CREDENTIAL_NAME);
        fs::create_dir(&path).unwrap();
        assert_eq!(kind_of(&store), StoreErrorKind::Unavailable);
        fs::remove_dir(&path).unwrap();
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: c_path is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert_eq!(kind_of(&store), StoreErrorKind::Unavailable);
    }

    /// A16: `put_new` cannot create and changes nothing; the store says so up front.
    #[test]
    fn put_new_is_not_creatable_and_touches_nothing() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("keep"), b"x").unwrap();
        let store = SystemdCredential::new(temp.path().to_owned());
        assert!(!store.can_create());
        let error = store.put_new(&name(), b"secret").unwrap_err();
        assert_eq!(error.kind(), StoreErrorKind::NotCreatable);
        assert_eq!(listing(temp.path()), ["keep"]);
        assert_eq!(kind_of(&store), StoreErrorKind::Absent);
    }
}
