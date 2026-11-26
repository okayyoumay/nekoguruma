//! The TLS key and certificate of the bundled object server (design 13.3, ADR-505): a P-256 key
//! and a self-signed certificate for `127.0.0.1`, each in an owner-only file directly in
//! `object_server.credentials_dir`, which the server makes at its first start under ADR-462's
//! file rules, and the pin of the certificate ([`CertPin`]).
//!
//! The key is made first and read back, then the certificate is made from the key as read back,
//! each by `private_files::read_or_create` (a temporary file, a hard link and a directory sync),
//! so a file appears complete or not at all and, of servers that start together, one link wins
//! and the others load its file. The server then reads both files back and checks that they are
//! one certificate, one PKCS#8 P-256 key and a pair ([`identity::pin_of_pair`]); the pin comes from that
//! check, at every start.
//!
//! A file holds one PEM section and only white space around it. A start after a crash between the
//! two links finds the key alone and makes the certificate from it. A crash between a temporary
//! file's write and its link, or between a link and the removal of the temporary file, leaves a
//! `.object-server-tls-*.tmp` file, which nothing reads; it holds a copy of the key or the
//! certificate, so it is safe to delete, and it is the kind of stray file the credentials
//! directory must not keep. Every other file that is not a good pair ends the start and is never rewritten: a
//! certificate without a key (nothing is created), a certificate and key that do not match, a
//! file that is empty, oversize, not PEM or of another label or key kind, and a file that is a
//! symbolic link, a FIFO, a directory, owned by another account or open to group or others
//! (Unix). The directory is never created. There is no rotation: to make a new pair, stop the
//! server, delete both files and start it again.
//!
//! The directory holds nothing else of the object server's than the credential files and these
//! two (and no leftover temporary file): the object server reads its TLS directory as a
//! namespace. Errors name the file in words
//! and never its content.

use std::io;
use std::path::Path;

use ngr_pinned_tls::CertPin;
use ngr_pinned_tls::identity::{self, IdentityError};
use shared_crypto::Zeroizing;

use super::private_files::{self, Fault};
use crate::fs_guard::{DirectoryFault, FileFault, check_directory};

/// The private key's file name in `credentials_dir`; the object server reads this name.
pub const KEY_FILE: &str = "rustfs_key.pem";
/// The certificate's file name in `credentials_dir`; the object server reads this name.
pub const CERTIFICATE_FILE: &str = "rustfs_cert.pem";

/// The most bytes read from either file; a made file is well under it.
const MAX_FILE_LEN: u64 = 4096;

/// Why the key and certificate could not be made ready.
#[derive(Debug, thiserror::Error)]
pub enum TlsFilesError {
    #[error("object_server.credentials_dir must be an absolute path of an existing directory")]
    Directory,
    #[error("cannot {op} an object server TLS file: {kind}")]
    Io {
        op: &'static str,
        kind: io::ErrorKind,
    },
    #[error(
        "the object server's TLS {which} path is not a regular file (a symbolic link is refused)"
    )]
    NotRegularFile { which: &'static str },
    #[error("the object server's TLS {which} file is not owned by the account the server runs as")]
    FileNotOwned { which: &'static str },
    #[error(
        "the object server's TLS {which} file is accessible to group or others; it must be \
         readable and writable by the server's account only (mode 0600)"
    )]
    FileTooOpen { which: &'static str },
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
        "the object server's TLS certificate file exists without its key file; neither is \
         changed: delete the certificate to have a new pair made"
    )]
    CertificateWithoutKey,
    #[error(
        "the object server's TLS {which} file is too long or not text; it is never rewritten: \
         fix or delete it"
    )]
    Corrupt { which: &'static str },
    #[error(
        "the object server's TLS key and certificate files are not a good pair ({0}); they are \
         never rewritten: fix or delete both"
    )]
    NotAPair(IdentityError),
    #[error("operating-system randomness is unavailable")]
    Random,
}

/// The key and certificate files of the object server and the pin of the certificate.
#[derive(Debug)]
pub struct TlsFiles {
    pin: CertPin,
    created: Option<Created>,
}

/// What a call to [`TlsFiles::load_or_create`] made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Created {
    KeyAndCertificate,
    /// The key was there (after a crash between the two links, or made by a start at the same
    /// time).
    CertificateOnly,
    /// A start at the same time made the certificate from the key this call made.
    KeyOnly,
}

impl Created {
    /// The files made, as words for a log line.
    pub fn what(self) -> &'static str {
        match self {
            Created::KeyAndCertificate => "key and certificate",
            Created::CertificateOnly => "certificate",
            Created::KeyOnly => "key",
        }
    }
}

impl TlsFiles {
    /// Reads the two files in `dir`, creating what is absent as the module's documentation says.
    /// `dir` must exist and, on Unix, belong to the server's account or root and not be writable
    /// by group or others.
    pub fn load_or_create(dir: &Path) -> Result<TlsFiles, TlsFilesError> {
        if !dir.is_absolute() {
            return Err(TlsFilesError::Directory);
        }
        check_directory(dir).map_err(directory_error)?;
        let key_path = dir.join(KEY_FILE);
        let cert_path = dir.join(CERTIFICATE_FILE);

        // The certificate is looked at before the key: it is made after the key, so a certificate
        // seen first has a key that is already there, even when another start is making both.
        let certificate_exists = private_files::read_existing(&cert_path, MAX_FILE_LEN)
            .map_err(|fault| fault_error(fault, "certificate"))?
            .is_some();
        let key_exists = private_files::read_existing(&key_path, MAX_FILE_LEN)
            .map_err(|fault| fault_error(fault, "key"))?
            .is_some();
        if certificate_exists && !key_exists {
            return Err(TlsFilesError::CertificateWithoutKey);
        }

        let key = private_files::read_or_create(
            dir,
            &key_path,
            ".object-server-tls-key-",
            MAX_FILE_LEN,
            || {
                let key = identity::generate_key().map_err(|_| Fault::Random)?;
                Ok(Zeroizing::new(key.as_bytes().to_vec()))
            },
        )
        .map_err(|fault: Fault| fault_error(fault, "key"))?;
        let key_text = text(&key.bytes, "key")?;
        let certificate = private_files::read_or_create(
            dir,
            &cert_path,
            ".object-server-tls-cert-",
            MAX_FILE_LEN,
            || make_certificate(key_text),
        )
        .map_err(|step| match step {
            Step::Fault(fault) => fault_error(fault, "certificate"),
            Step::Pair(error) => TlsFilesError::NotAPair(error),
        })?;
        let certificate_text = text(&certificate.bytes, "certificate")?;
        let pin =
            identity::pin_of_pair(certificate_text, key_text).map_err(TlsFilesError::NotAPair)?;
        Ok(TlsFiles {
            pin,
            created: match (key.created, certificate.created) {
                (true, true) => Some(Created::KeyAndCertificate),
                (false, true) => Some(Created::CertificateOnly),
                (true, false) => Some(Created::KeyOnly),
                (false, false) => None,
            },
        })
    }

    /// The pin of the certificate in the file.
    pub fn pin(&self) -> &CertPin {
        &self.pin
    }

    /// Which files this call created, if any.
    pub fn created(&self) -> Option<Created> {
        self.created
    }
}

/// What ends the making of the certificate.
enum Step {
    Fault(Fault),
    /// The key read back is not one this module signs with.
    Pair(IdentityError),
}

impl From<Fault> for Step {
    fn from(fault: Fault) -> Self {
        Step::Fault(fault)
    }
}

/// The certificate made from `key_text`, checked against it before anything is written: a key
/// of another kind or one that is not a key makes no certificate.
fn make_certificate(key_text: &str) -> Result<Zeroizing<Vec<u8>>, Step> {
    let certificate = identity::self_signed(key_text).map_err(Step::Pair)?;
    identity::pin_of_pair(&certificate, key_text).map_err(Step::Pair)?;
    Ok(Zeroizing::new(certificate.into_bytes()))
}

fn text<'a>(bytes: &'a [u8], which: &'static str) -> Result<&'a str, TlsFilesError> {
    std::str::from_utf8(bytes).map_err(|_| TlsFilesError::Corrupt { which })
}

fn directory_error(fault: DirectoryFault) -> TlsFilesError {
    match fault {
        DirectoryFault::Io(kind) => TlsFilesError::Io {
            op: "inspect the directory of",
            kind,
        },
        DirectoryFault::NotADirectory => TlsFilesError::Directory,
        DirectoryFault::NotOwned => TlsFilesError::DirectoryNotOwned,
        DirectoryFault::Writable => TlsFilesError::DirectoryWritable,
    }
}

fn fault_error(fault: Fault, which: &'static str) -> TlsFilesError {
    match fault {
        Fault::File(FileFault::Io { op, kind }) => TlsFilesError::Io { op, kind },
        Fault::File(FileFault::NotRegular) => TlsFilesError::NotRegularFile { which },
        Fault::File(FileFault::NotOwned) => TlsFilesError::FileNotOwned { which },
        Fault::File(FileFault::TooOpen) => TlsFilesError::FileTooOpen { which },
        Fault::Random => TlsFilesError::Random,
        Fault::TooLong => TlsFilesError::Corrupt { which },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn key_path(dir: &Path) -> PathBuf {
        dir.join(KEY_FILE)
    }

    fn cert_path(dir: &Path) -> PathBuf {
        dir.join(CERTIFICATE_FILE)
    }

    fn write_private(path: &Path, content: &[u8]) {
        std::fs::write(path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    /// A new key and its certificate, as PEM.
    fn pair() -> (String, String) {
        let key = identity::generate_key().unwrap().to_string();
        let cert = identity::self_signed(&key).unwrap();
        (key, cert)
    }

    fn p384_key() -> String {
        ngr_pinned_tls::test_server::Identity::generate(ngr_pinned_tls::test_server::Key::P384)
            .key_pem()
            .to_owned()
    }

    fn files(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut found: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
        found.sort();
        found
    }

    #[test]
    fn absent_files_are_created_and_loaded_again_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let first = TlsFiles::load_or_create(dir.path()).unwrap();
        assert_eq!(first.created(), Some(Created::KeyAndCertificate));
        let made = files(dir.path());
        assert_eq!(
            made.iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            [CERTIFICATE_FILE, KEY_FILE],
            "the two files and no temporary file"
        );
        let key = std::fs::read_to_string(key_path(dir.path())).unwrap();
        let cert = std::fs::read_to_string(cert_path(dir.path())).unwrap();
        assert_eq!(identity::pin_of_pair(&cert, &key).as_ref(), Ok(first.pin()));
        #[cfg(unix)]
        for path in [key_path(dir.path()), cert_path(dir.path())] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let before = std::fs::metadata(key_path(dir.path()))
            .unwrap()
            .modified()
            .unwrap();
        // A second start loads the files and rewrites nothing.
        let again = TlsFiles::load_or_create(dir.path()).unwrap();
        assert_eq!(again.created(), None);
        assert_eq!(again.pin(), first.pin());
        assert_eq!(files(dir.path()), made);
        assert_eq!(
            std::fs::metadata(key_path(dir.path()))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        // Another directory makes another pair.
        let other = tempfile::tempdir().unwrap();
        let second = TlsFiles::load_or_create(other.path()).unwrap();
        assert_ne!(second.pin(), first.pin());
    }

    #[test]
    fn a_key_alone_gets_a_certificate_made_from_it() {
        let dir = tempfile::tempdir().unwrap();
        let (key, _) = pair();
        write_private(&key_path(dir.path()), key.as_bytes());
        let loaded = TlsFiles::load_or_create(dir.path()).unwrap();
        assert_eq!(loaded.created(), Some(Created::CertificateOnly));
        assert_eq!(std::fs::read_to_string(key_path(dir.path())).unwrap(), key);
        let cert = std::fs::read_to_string(cert_path(dir.path())).unwrap();
        assert_eq!(
            identity::pin_of_pair(&cert, &key).as_ref(),
            Ok(loaded.pin())
        );
    }

    #[test]
    fn a_certificate_alone_is_refused_and_nothing_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let (_, cert) = pair();
        write_private(&cert_path(dir.path()), cert.as_bytes());
        assert!(matches!(
            TlsFiles::load_or_create(dir.path()),
            Err(TlsFilesError::CertificateWithoutKey)
        ));
        assert_eq!(
            files(dir.path()),
            [(CERTIFICATE_FILE.to_owned(), cert.into_bytes())]
        );
    }

    #[test]
    fn a_pair_that_does_not_match_is_refused_and_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let (key, _) = pair();
        let (_, other_cert) = pair();
        write_private(&key_path(dir.path()), key.as_bytes());
        write_private(&cert_path(dir.path()), other_cert.as_bytes());
        let before = files(dir.path());
        assert!(matches!(
            TlsFiles::load_or_create(dir.path()),
            Err(TlsFilesError::NotAPair(IdentityError::Mismatch))
        ));
        assert_eq!(files(dir.path()), before);
    }

    #[test]
    fn a_corrupt_file_is_refused_and_never_rewritten() {
        let (key, cert) = pair();
        let (other_key, other_cert) = pair();
        let cases: Vec<(&str, Vec<u8>, Vec<u8>)> = vec![
            ("empty", Vec::new(), Vec::new()),
            (
                "two sections",
                format!("{key}{other_key}").into_bytes(),
                format!("{cert}{other_cert}").into_bytes(),
            ),
            (
                "wrong label",
                key.replace("PRIVATE KEY", "EC PRIVATE KEY").into_bytes(),
                cert.replace("CERTIFICATE", "TRUSTED CERTIFICATE")
                    .into_bytes(),
            ),
            ("garbage", b"not pem".to_vec(), b"not pem".to_vec()),
            (
                "oversize",
                [key.as_bytes(), &vec![b'\n'; 10_000]].concat(),
                [cert.as_bytes(), &vec![b'\n'; 10_000]].concat(),
            ),
            ("not text", vec![0xff; 40], vec![0xff; 40]),
            (
                "a key of another curve",
                p384_key().into_bytes(),
                cert.clone().into_bytes(),
            ),
        ];
        for (name, bad_key, bad_cert) in cases {
            // A bad key, with the certificate present and with it absent.
            for cert_present in [true, false] {
                let dir = tempfile::tempdir().unwrap();
                write_private(&key_path(dir.path()), &bad_key);
                if cert_present {
                    write_private(&cert_path(dir.path()), cert.as_bytes());
                }
                let before = files(dir.path());
                let error = TlsFiles::load_or_create(dir.path()).unwrap_err();
                assert!(
                    matches!(
                        error,
                        TlsFilesError::NotAPair(_) | TlsFilesError::Corrupt { which: "key" }
                    ),
                    "{name}, certificate present {cert_present}: {error}"
                );
                assert_eq!(files(dir.path()), before, "{name}");
            }
            // A bad certificate beside a good key.
            if name == "a key of another curve" {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            write_private(&key_path(dir.path()), key.as_bytes());
            write_private(&cert_path(dir.path()), &bad_cert);
            let before = files(dir.path());
            let error = TlsFiles::load_or_create(dir.path()).unwrap_err();
            assert!(
                matches!(
                    error,
                    TlsFilesError::NotAPair(_)
                        | TlsFilesError::Corrupt {
                            which: "certificate"
                        }
                ),
                "{name}: {error}"
            );
            assert_eq!(files(dir.path()), before, "{name}");
        }
    }

    #[test]
    fn a_directory_in_place_of_a_file_is_refused() {
        for name in [KEY_FILE, CERTIFICATE_FILE] {
            let dir = tempfile::tempdir().unwrap();
            if name == CERTIFICATE_FILE {
                let (key, _) = pair();
                write_private(&key_path(dir.path()), key.as_bytes());
            }
            std::fs::create_dir(dir.path().join(name)).unwrap();
            let error = TlsFiles::load_or_create(dir.path()).unwrap_err();
            // Windows opens a file without backup semantics, so a directory fails there with an
            // access error instead of being recognized as not a file; both are refusals.
            #[cfg(unix)]
            assert!(
                matches!(error, TlsFilesError::NotRegularFile { .. }),
                "{name}: {error}"
            );
            #[cfg(windows)]
            assert!(
                matches!(
                    error,
                    TlsFilesError::Io {
                        kind: io::ErrorKind::PermissionDenied,
                        ..
                    }
                ),
                "{name}: {error}"
            );
            assert!(dir.path().join(name).is_dir(), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_others_can_write_to_is_refused_and_gets_no_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o770, 0o707, 0o777] {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(
                TlsFiles::load_or_create(dir.path()),
                Err(TlsFilesError::DirectoryWritable)
            ));
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn a_leftover_temporary_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (key, cert) = pair();
        // A crash after the certificate's temporary file was written and before its link.
        write_private(&key_path(dir.path()), key.as_bytes());
        let leftover = dir.path().join(".object-server-tls-cert-00.tmp");
        write_private(&leftover, cert.as_bytes());
        let loaded = TlsFiles::load_or_create(dir.path()).unwrap();
        assert_eq!(loaded.created(), Some(Created::CertificateOnly));
        assert!(leftover.exists());
        assert!(cert_path(dir.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    #[expect(
        unsafe_code,
        reason = "libc FFI with no safe std equivalent: `mkfifo` creates a FIFO (ADR-502)"
    )]
    fn a_symbolic_link_and_a_fifo_are_refused_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let (key, cert) = pair();
        for name in [KEY_FILE, CERTIFICATE_FILE] {
            let dir = tempfile::tempdir().unwrap();
            write_private(&key_path(dir.path()), key.as_bytes());
            write_private(&cert_path(dir.path()), cert.as_bytes());
            let target = dir.path().join("target");
            let link = dir.path().join(name);
            std::fs::rename(&link, &target).unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(
                matches!(
                    TlsFiles::load_or_create(dir.path()),
                    Err(TlsFilesError::NotRegularFile { .. })
                ),
                "{name}"
            );
            // A dangling link is refused too, not read as absent.
            std::fs::remove_file(&target).unwrap();
            assert!(
                matches!(
                    TlsFiles::load_or_create(dir.path()),
                    Err(TlsFilesError::NotRegularFile { .. })
                ),
                "{name}, dangling"
            );
            std::fs::remove_file(&link).unwrap();
            let fifo = std::ffi::CString::new(link.as_os_str().as_bytes()).unwrap();
            // SAFETY: `fifo` is a valid NUL-terminated path for the duration of the call.
            if unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) } != 0 {
                continue; // no FIFOs on this file system
            }
            let path = dir.path().to_owned();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(matches!(
                    TlsFiles::load_or_create(&path),
                    Err(TlsFilesError::NotRegularFile { .. })
                ));
            });
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(10)),
                Ok(true),
                "{name}, FIFO"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_file_with_a_group_or_other_bit_is_refused_and_left_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        for name in [KEY_FILE, CERTIFICATE_FILE] {
            let dir = tempfile::tempdir().unwrap();
            TlsFiles::load_or_create(dir.path()).unwrap();
            let before = files(dir.path());
            let path = dir.path().join(name);
            for mode in [0o640, 0o604, 0o660, 0o666, 0o644, 0o601] {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
                assert!(
                    matches!(
                        TlsFiles::load_or_create(dir.path()),
                        Err(TlsFilesError::FileTooOpen { .. })
                    ),
                    "{name} {mode:o}"
                );
                assert_eq!(files(dir.path()), before);
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
            TlsFiles::load_or_create(dir.path()).unwrap();
        }
    }

    // Runs only as root: a chown to another account needs it (as for the credential files).
    #[cfg(unix)]
    #[test]
    fn a_file_or_directory_of_another_account_is_refused() {
        if crate::fs_guard::euid() != 0 {
            return; // chown to another account needs root
        }
        let other = 4_242_424;
        let dir = tempfile::tempdir().unwrap();
        TlsFiles::load_or_create(dir.path()).unwrap();
        for path in [key_path(dir.path()), cert_path(dir.path())] {
            std::os::unix::fs::chown(&path, Some(other), None).unwrap();
            assert!(matches!(
                TlsFiles::load_or_create(dir.path()),
                Err(TlsFilesError::FileNotOwned { .. })
            ));
            std::os::unix::fs::chown(&path, Some(0), None).unwrap();
        }
        TlsFiles::load_or_create(dir.path()).unwrap();
        std::os::unix::fs::chown(dir.path(), Some(other), None).unwrap();
        assert!(matches!(
            TlsFiles::load_or_create(dir.path()),
            Err(TlsFilesError::DirectoryNotOwned)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        if crate::fs_guard::euid() == 0 {
            return; // root reads a file of mode 0
        }
        let dir = tempfile::tempdir().unwrap();
        TlsFiles::load_or_create(dir.path()).unwrap();
        std::fs::set_permissions(key_path(dir.path()), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        assert!(matches!(
            TlsFiles::load_or_create(dir.path()),
            Err(TlsFilesError::Io {
                kind: io::ErrorKind::PermissionDenied,
                ..
            })
        ));
    }

    #[test]
    fn an_absent_or_relative_directory_is_refused_and_never_created() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(matches!(
            TlsFiles::load_or_create(&missing),
            Err(TlsFilesError::Io {
                kind: io::ErrorKind::NotFound,
                ..
            })
        ));
        assert!(!missing.exists());
        assert!(matches!(
            TlsFiles::load_or_create(Path::new("relative")),
            Err(TlsFilesError::Directory)
        ));
    }

    #[test]
    fn sixteen_first_starts_at_once_agree_on_one_pair() {
        let dir = tempfile::tempdir().unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let (path, start) = (dir.path().to_owned(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    TlsFiles::load_or_create(&path).unwrap().pin().to_string()
                })
            })
            .collect();
        let pins: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(pins.iter().all(|pin| *pin == pins[0]));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        let key = std::fs::read_to_string(key_path(dir.path())).unwrap();
        let cert = std::fs::read_to_string(cert_path(dir.path())).unwrap();
        assert_eq!(
            identity::pin_of_pair(&cert, &key).unwrap().to_string(),
            pins[0]
        );
    }

    #[test]
    fn errors_show_nothing_of_a_file() {
        let dir = tempfile::tempdir().unwrap();
        write_private(&key_path(dir.path()), b"s3cret-value");
        let error = TlsFiles::load_or_create(dir.path()).unwrap_err();
        assert!(!format!("{error} / {error:?}").contains("s3cret-value"));
        let dir = tempfile::tempdir().unwrap();
        TlsFiles::load_or_create(dir.path()).unwrap();
        let loaded = TlsFiles::load_or_create(dir.path()).unwrap();
        let shown = format!("{loaded:?}");
        let key = std::fs::read_to_string(key_path(dir.path())).unwrap();
        let body = key.lines().nth(1).unwrap();
        assert!(!body.is_empty() && !shown.contains(body), "{shown}");
    }
}
