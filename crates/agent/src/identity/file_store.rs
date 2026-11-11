//! A key store that keeps each secret in a plain file, for tests only (ADR-459 Decision item 4,
//! ADR-460 Decision item 6). This module is compiled in debug builds only: a release build has no
//! store that writes a key unprotected.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;

use shared_crypto::Zeroizing;

use super::store::{SecretName, SecretStore, StoreError, StoreErrorKind};
use crate::journal::{CreateLinkedError, create_linked};

/// Secrets as files `<name>.pk8` in one directory. A file appears complete or not at all, made by the
/// same routine as the journal (`create_linked`).
pub struct DebugFileStore {
    dir: PathBuf,
}

impl DebugFileStore {
    /// A store in `dir`, which `put_new` expects to exist.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, name: &SecretName) -> PathBuf {
        self.dir.join(format!("{name}.pk8"))
    }
}

fn unavailable(error: io::Error) -> StoreError {
    StoreError::with_source(StoreErrorKind::Unavailable, format!("{:?}", error.kind()))
}

impl SecretStore for DebugFileStore {
    fn get(&self, name: &SecretName) -> Result<Zeroizing<Vec<u8>>, StoreError> {
        match fs::read(self.path(name)) {
            Ok(bytes) => Ok(Zeroizing::new(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Err(StoreError::new(StoreErrorKind::Absent))
            }
            Err(error) => Err(unavailable(error)),
        }
    }

    fn put_new(&self, name: &SecretName, secret: &[u8]) -> Result<(), StoreError> {
        let path = self.path(name);
        let tmp = path.with_extension("pk8.tmp");
        match create_linked(&self.dir, &path, &tmp, secret, private_options()) {
            Ok(_) => Ok(()),
            Err(CreateLinkedError::AlreadyExists) => {
                Err(StoreError::new(StoreErrorKind::AlreadyExists))
            }
            Err(CreateLinkedError::Io(error)) => Err(unavailable(error)),
        }
    }
}

/// Options that give the new file to its owner only on Unix.
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name() -> SecretName {
        SecretName::new("k.v1").unwrap()
    }

    #[test]
    fn creates_reads_and_never_replaces() {
        let temp = tempfile::tempdir().unwrap();
        let store = DebugFileStore::new(temp.path().to_owned());
        assert_eq!(
            store.get(&name()).unwrap_err().kind(),
            StoreErrorKind::Absent
        );
        store.put_new(&name(), b"one").unwrap();
        assert_eq!(&**store.get(&name()).unwrap(), b"one");
        assert_eq!(
            store.put_new(&name(), b"two").unwrap_err().kind(),
            StoreErrorKind::AlreadyExists
        );
        assert_eq!(&**store.get(&name()).unwrap(), b"one");
        assert!(!temp.path().join("k.v1.pk8.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(temp.path().join("k.v1.pk8"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// S6: a crash left a temporary file and no final name; the store reads Absent, and the
    /// next create removes the temporary file.
    #[test]
    fn a_stale_temporary_file_is_absent_and_is_removed_by_the_next_create() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("k.v1.pk8.tmp"), b"half").unwrap();
        let store = DebugFileStore::new(temp.path().to_owned());
        assert_eq!(
            store.get(&name()).unwrap_err().kind(),
            StoreErrorKind::Absent
        );
        store.put_new(&name(), b"whole").unwrap();
        assert_eq!(&**store.get(&name()).unwrap(), b"whole");
        assert!(!temp.path().join("k.v1.pk8.tmp").exists());
    }

    /// A read that fails for another reason than a missing file is not Absent (S3).
    #[test]
    fn an_unreadable_name_is_unavailable_not_absent() {
        let temp = tempfile::tempdir().unwrap();
        // A directory under the final name cannot be read as a file.
        fs::create_dir(temp.path().join("k.v1.pk8")).unwrap();
        let store = DebugFileStore::new(temp.path().to_owned());
        assert_eq!(
            store.get(&name()).unwrap_err().kind(),
            StoreErrorKind::Unavailable
        );
    }

    /// S12: a store whose directory is gone fails to create and leaves no final name.
    #[test]
    fn a_create_in_a_missing_directory_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let store = DebugFileStore::new(temp.path().join("gone"));
        assert_eq!(
            store.put_new(&name(), b"x").unwrap_err().kind(),
            StoreErrorKind::Unavailable
        );
    }
}
