//! The credentials of the bundled object server (design 13.3, ADR-491): an access key and a secret
//! key, each in an owner-only file in `object_server.credentials_dir`, which the server creates at
//! its first start with the rules of the instruction key's file (ADR-462).
//!
//! The files are created by a temporary file, a hard link and a directory sync, so a file appears
//! complete or not at all and, of two servers that start together, one link wins and the other
//! loads its file. A file that exists is never rewritten or regenerated: one that is corrupt (not
//! the shape this module writes), a symbolic link, a FIFO, a directory, owned by another account or
//! open to group or others (Unix) ends the start. The directory is never created. A crash between
//! a link and the removal of the temporary file leaves a `.tmp` file, which nothing reads; delete
//! it. There is no rotation: to reset the credentials, stop the server, delete the two files and
//! start it again (the object server's data stays, so it may refuse the new keys).
//!
//! The values are never in the configuration, the environment or a command line: the object server
//! is given the files' paths and reads them itself. Errors name the file in words and never a
//! value, and [`Credentials`]' `Debug` shows nothing of either key.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use shared_crypto::{Zeroizing, random_32};

use crate::fs_guard::{self, DirectoryFault, FileFault, LinkFault, check_directory};

/// The access key's file name in `credentials_dir`.
pub const ACCESS_KEY_FILE: &str = "object-server-access-key";
/// The secret key's file name in `credentials_dir`.
pub const SECRET_KEY_FILE: &str = "object-server-secret-key";

/// Access key: 20 characters, upper-case hexadecimal (80 bits).
const ACCESS_LEN: usize = 20;
/// Secret key: 40 characters, lower-case hexadecimal (160 bits).
const SECRET_LEN: usize = 40;
/// The most bytes read from a credential file; both shapes are shorter.
const MAX_FILE_LEN: u64 = 64;

/// Why the credentials could not be made ready.
#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error("object_server.credentials_dir must be an absolute path of an existing directory")]
    Directory,
    #[error("cannot {op} an object server credential file: {kind}")]
    Io {
        op: &'static str,
        kind: io::ErrorKind,
    },
    #[error("an object server credential path is not a regular file (a symbolic link is refused)")]
    NotRegularFile,
    #[error("an object server credential file is not owned by the account the server runs as")]
    FileNotOwned,
    #[error(
        "an object server credential file is accessible to group or others; it must be readable \
         and writable by the server's account only (mode 0600)"
    )]
    FileTooOpen,
    #[error(
        "the object server credentials directory is owned by neither the account the server runs \
         as nor root"
    )]
    DirectoryNotOwned,
    #[error(
        "the object server credentials directory is writable by group or others; it must be \
         writable by the server's account only"
    )]
    DirectoryWritable,
    #[error(
        "the object server's {which} file is not what the server writes; it is never rewritten: \
         fix or delete it (the object server's data may then refuse new keys)"
    )]
    Corrupt { which: &'static str },
    #[error("operating-system randomness is unavailable")]
    Random,
}

/// The access key and the secret key. `Debug` shows neither; the secret is zeroed when dropped.
pub struct Credentials {
    access_key: String,
    secret_key: Zeroizing<String>,
    access_path: PathBuf,
    secret_path: PathBuf,
    created: bool,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials(..)")
    }
}

impl Credentials {
    /// Reads the two files in `dir`, creating the ones that are absent. `dir` must exist and, on
    /// Unix, belong to the server's account or root and not be writable by group or others.
    pub fn load_or_create(dir: &Path) -> Result<Credentials, CredentialsError> {
        if !dir.is_absolute() {
            return Err(CredentialsError::Directory);
        }
        check_directory(dir).map_err(directory_error)?;
        let access_path = dir.join(ACCESS_KEY_FILE);
        let secret_path = dir.join(SECRET_KEY_FILE);
        let (access, created_access) = load_or_create_one(
            dir,
            &access_path,
            "access key",
            ACCESS_LEN,
            |c| c.is_ascii_digit() || (b'A'..=b'F').contains(&c),
            || generate(10, true),
        )?;
        let (secret, created_secret) = load_or_create_one(
            dir,
            &secret_path,
            "secret key",
            SECRET_LEN,
            |c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c),
            || generate(20, false),
        )?;
        Ok(Credentials {
            access_key: access.to_string(),
            secret_key: secret,
            access_path,
            secret_path,
            created: created_access || created_secret,
        })
    }

    /// The access key (an identifier, not a secret).
    pub fn access_key(&self) -> &str {
        &self.access_key
    }

    /// The secret key. Callers must not log it or put it in an argument or the environment.
    pub fn expose_secret(&self) -> &str {
        &self.secret_key
    }

    /// The access key's file, which the object server is told to read.
    pub fn access_key_path(&self) -> &Path {
        &self.access_path
    }

    /// The secret key's file, which the object server is told to read.
    pub fn secret_key_path(&self) -> &Path {
        &self.secret_path
    }

    /// Whether this call created a file.
    pub fn created(&self) -> bool {
        self.created
    }
}

fn directory_error(fault: DirectoryFault) -> CredentialsError {
    match fault {
        DirectoryFault::Io(kind) => CredentialsError::Io {
            op: "inspect the directory of",
            kind,
        },
        DirectoryFault::NotADirectory => CredentialsError::Directory,
        DirectoryFault::NotOwned => CredentialsError::DirectoryNotOwned,
        DirectoryFault::Writable => CredentialsError::DirectoryWritable,
    }
}

fn file_error(fault: FileFault) -> CredentialsError {
    match fault {
        FileFault::Io { op, kind } => CredentialsError::Io { op, kind },
        FileFault::NotRegular => CredentialsError::NotRegularFile,
        FileFault::NotOwned => CredentialsError::FileNotOwned,
        FileFault::TooOpen => CredentialsError::FileTooOpen,
    }
}

/// `bytes` random bytes as hexadecimal text, upper case when `upper`.
fn generate(bytes: usize, upper: bool) -> Result<Zeroizing<String>, CredentialsError> {
    let random = Zeroizing::new(random_32().map_err(|_| CredentialsError::Random)?);
    let mut text = Zeroizing::new(String::with_capacity(bytes * 2));
    for b in random.iter().take(bytes) {
        text.push_str(&if upper {
            format!("{b:02X}")
        } else {
            format!("{b:02x}")
        });
    }
    Ok(text)
}

/// Reads one file, creating it first when it is absent.
fn load_or_create_one(
    dir: &Path,
    path: &Path,
    which: &'static str,
    len: usize,
    allowed: impl Fn(u8) -> bool,
    generate: impl FnOnce() -> Result<Zeroizing<String>, CredentialsError>,
) -> Result<(Zeroizing<String>, bool), CredentialsError> {
    let mut created = false;
    let file = match fs_guard::open_private(path).map_err(file_error)? {
        Some(file) => file,
        None => {
            let value = generate()?;
            created = match fs_guard::create_by_link(
                dir,
                path,
                ".object-server-credential-",
                value.as_bytes(),
            ) {
                Ok(created) => created,
                Err(LinkFault::Random) => return Err(CredentialsError::Random),
                Err(LinkFault::Io { op, error }) => {
                    return Err(CredentialsError::Io {
                        op,
                        kind: error.kind(),
                    });
                }
            };
            // What the file holds is read back, as at every start: the winner's value, which is
            // not necessarily this call's.
            fs_guard::open_private(path)
                .map_err(file_error)?
                .ok_or(CredentialsError::Io {
                    op: "read back",
                    kind: io::ErrorKind::NotFound,
                })?
        }
    };
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_FILE_LEN + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| CredentialsError::Io {
            op: "read",
            kind: error.kind(),
        })?;
    let valid = bytes.len() == len && bytes.iter().all(|&c| allowed(c));
    if !valid {
        return Err(CredentialsError::Corrupt { which });
    }
    let text = String::from_utf8(std::mem::take(&mut *bytes))
        .map_err(|_| CredentialsError::Corrupt { which })?;
    Ok((Zeroizing::new(text), created))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(dir: &Path) -> PathBuf {
        dir.join(ACCESS_KEY_FILE)
    }

    fn secret(dir: &Path) -> PathBuf {
        dir.join(SECRET_KEY_FILE)
    }

    fn write_private(path: &Path, content: &[u8]) {
        std::fs::write(path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn absent_files_are_created_in_the_documented_shape_and_loaded_again_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let first = Credentials::load_or_create(dir.path()).unwrap();
        assert!(first.created());
        assert_eq!(first.access_key().len(), ACCESS_LEN);
        assert_eq!(first.expose_secret().len(), SECRET_LEN);
        assert!(
            first
                .access_key()
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'A'..=b'F').contains(&c))
        );
        assert_eq!(
            std::fs::read_to_string(access(dir.path())).unwrap(),
            first.access_key()
        );
        assert_eq!(
            std::fs::read_to_string(secret(dir.path())).unwrap(),
            first.expose_secret()
        );
        assert_eq!(first.access_key_path(), access(dir.path()));
        assert_eq!(first.secret_key_path(), secret(dir.path()));
        // A second start loads them and rewrites nothing.
        let again = Credentials::load_or_create(dir.path()).unwrap();
        assert!(!again.created());
        assert_eq!(again.access_key(), first.access_key());
        assert_eq!(again.expose_secret(), first.expose_secret());
        // Two creations draw different values.
        let other = tempfile::tempdir().unwrap();
        let second = Credentials::load_or_create(other.path()).unwrap();
        assert_ne!(second.expose_secret(), first.expose_secret());
        // No temporary file is left.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn created_files_are_readable_by_their_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Credentials::load_or_create(dir.path()).unwrap();
        for path in [access(dir.path()), secret(dir.path())] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    // K3
    #[test]
    fn a_corrupt_file_ends_the_start_and_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let good_access = "0123456789ABCDEF0123";
        let good_secret = "0123456789abcdef0123456789abcdef01234567";
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("truncated", good_secret.as_bytes()[..39].to_vec()),
            ("extra line", format!("{good_secret}\n").into_bytes()),
            (
                "two lines",
                format!("{good_secret}\n{good_secret}").into_bytes(),
            ),
            ("wrong alphabet", good_secret.to_uppercase().into_bytes()),
            ("non-hex", "g".repeat(40).into_bytes()),
            ("oversize", vec![b'a'; 10_000]),
            ("not utf-8", vec![0xff; 40]),
        ];
        for (name, content) in cases {
            write_private(&access(dir.path()), good_access.as_bytes());
            write_private(&secret(dir.path()), &content);
            let error = Credentials::load_or_create(dir.path()).unwrap_err();
            assert!(
                matches!(
                    error,
                    CredentialsError::Corrupt {
                        which: "secret key"
                    }
                ),
                "{name}: {error}"
            );
            assert_eq!(
                std::fs::read(secret(dir.path())).unwrap(),
                content,
                "{name}"
            );
            // The access key is judged by its own shape.
            write_private(&secret(dir.path()), good_secret.as_bytes());
            write_private(&access(dir.path()), &content);
            let error = Credentials::load_or_create(dir.path()).unwrap_err();
            assert!(
                matches!(
                    error,
                    CredentialsError::Corrupt {
                        which: "access key"
                    }
                ),
                "{name}: {error}"
            );
            assert_eq!(
                std::fs::read(access(dir.path())).unwrap(),
                content,
                "{name}"
            );
        }
    }

    // K4
    #[cfg(unix)]
    #[test]
    fn a_file_with_a_group_or_other_bit_is_refused_and_left_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Credentials::load_or_create(dir.path()).unwrap();
        let bytes = std::fs::read(secret(dir.path())).unwrap();
        for mode in [0o640, 0o604, 0o660, 0o666, 0o644, 0o601] {
            std::fs::set_permissions(secret(dir.path()), std::fs::Permissions::from_mode(mode))
                .unwrap();
            let error = Credentials::load_or_create(dir.path()).unwrap_err();
            assert!(matches!(error, CredentialsError::FileTooOpen), "{mode:o}");
            assert_eq!(std::fs::read(secret(dir.path())).unwrap(), bytes);
        }
        std::fs::set_permissions(secret(dir.path()), std::fs::Permissions::from_mode(0o400))
            .unwrap();
        assert!(Credentials::load_or_create(dir.path()).is_ok());
    }

    // K5
    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_a_fifo_and_a_directory_are_refused_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        write_private(&target, b"0123456789ABCDEF0123");
        let link = access(dir.path());
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            Credentials::load_or_create(dir.path()),
            Err(CredentialsError::NotRegularFile)
        ));
        std::fs::remove_file(&link).unwrap();
        // A dangling link is refused too, not read as absent.
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &link).unwrap();
        assert!(matches!(
            Credentials::load_or_create(dir.path()),
            Err(CredentialsError::NotRegularFile)
        ));
        std::fs::remove_file(&link).unwrap();
        std::fs::create_dir(&link).unwrap();
        assert!(matches!(
            Credentials::load_or_create(dir.path()),
            Err(CredentialsError::NotRegularFile)
        ));
        std::fs::remove_dir(&link).unwrap();
        let name = std::ffi::CString::new(link.as_os_str().as_bytes()).unwrap();
        // SAFETY: `name` is a valid NUL-terminated path for the duration of the call.
        if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
            return; // no FIFOs on this file system
        }
        let path = dir.path().to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(matches!(
                Credentials::load_or_create(&path),
                Err(CredentialsError::NotRegularFile)
            ));
        });
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(true)
        );
    }

    // K6
    #[cfg(unix)]
    #[test]
    fn a_file_or_directory_of_another_account_is_refused() {
        if fs_guard::euid() != 0 {
            return; // chown to another account needs root
        }
        let other = 4_242_424;
        let dir = tempfile::tempdir().unwrap();
        Credentials::load_or_create(dir.path()).unwrap();
        std::os::unix::fs::chown(secret(dir.path()), Some(other), None).unwrap();
        assert!(matches!(
            Credentials::load_or_create(dir.path()),
            Err(CredentialsError::FileNotOwned)
        ));
        std::os::unix::fs::chown(secret(dir.path()), Some(0), None).unwrap();
        assert!(Credentials::load_or_create(dir.path()).is_ok());
        std::os::unix::fs::chown(dir.path(), Some(other), None).unwrap();
        assert!(matches!(
            Credentials::load_or_create(dir.path()),
            Err(CredentialsError::DirectoryNotOwned)
        ));
    }

    // K7
    #[test]
    fn an_absent_or_relative_directory_is_refused_and_never_created() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let error = Credentials::load_or_create(&missing).unwrap_err();
        assert!(
            matches!(
                error,
                CredentialsError::Io {
                    kind: io::ErrorKind::NotFound,
                    ..
                }
            ),
            "{error}"
        );
        assert!(!missing.exists());
        assert!(matches!(
            Credentials::load_or_create(Path::new("relative")),
            Err(CredentialsError::Directory)
        ));
        // A file where the directory should be.
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(matches!(
            Credentials::load_or_create(&file),
            Err(CredentialsError::Directory)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_others_can_write_to_is_refused_and_gets_no_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o770, 0o707, 0o777] {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(
                Credentials::load_or_create(dir.path()),
                Err(CredentialsError::DirectoryWritable)
            ));
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
    }

    // K8: sixteen first starts at once end with one value in each file, which every one loaded.
    #[test]
    fn sixteen_first_starts_at_once_agree_on_one_pair() {
        let dir = tempfile::tempdir().unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let (path, start) = (dir.path().to_owned(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    let c = Credentials::load_or_create(&path).unwrap();
                    (c.access_key().to_owned(), c.expose_secret().to_owned())
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(results.iter().all(|r| *r == results[0]));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        assert_eq!(
            std::fs::read_to_string(secret(dir.path())).unwrap(),
            results[0].1
        );
    }

    // K9: a temporary file left by a crash between the link and its removal is ignored.
    #[test]
    fn a_leftover_temporary_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write_private(
            &dir.path().join(".object-server-credential-00.tmp"),
            b"leftover",
        );
        let c = Credentials::load_or_create(dir.path()).unwrap();
        assert!(c.created());
        assert!(dir.path().join(".object-server-credential-00.tmp").exists());
    }

    // L6: the type's Debug shows no value.
    #[test]
    fn debug_and_errors_show_no_value() {
        let dir = tempfile::tempdir().unwrap();
        let c = Credentials::load_or_create(dir.path()).unwrap();
        let shown = format!("{c:?} {c:#?}");
        assert!(!shown.contains(c.expose_secret()), "{shown}");
        assert!(!shown.contains(c.access_key()), "{shown}");
        write_private(&secret(dir.path()), b"s3cret-value");
        let error = Credentials::load_or_create(dir.path()).unwrap_err();
        assert!(!format!("{error} / {error:?}").contains("s3cret-value"));
    }
}
