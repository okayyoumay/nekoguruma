//! The server's job instruction key (design 11.1, 16.3; ADR-462): the private key that signs
//! every job instruction, one per deployment.
//!
//! The private key is a PKCS#8 file at the absolute path of `[instruction_key]` in the
//! configuration; the database records the public half in `instruction_keys`, and that row is
//! what makes the key one per deployment (two servers, or a restart with another file, cannot
//! end up signing under two identities). [`InstructionKey::open`] runs at start, after the
//! migrations and before the listener binds.
//!
//! The outcomes of `open`:
//!
//! | File | Active row | Result |
//! |---|---|---|
//! | absent | none | the file is created, the key is recorded |
//! | valid | same key | start |
//! | valid | none | the key is recorded |
//! | absent | present | refused |
//! | unreadable, not a P-256 PKCS#8 key, or too open | any | refused, the file is never touched |
//! | valid | another key | refused |
//!
//! Errors are fixed text and, for a failed file operation, the [`std::io::ErrorKind`]: never key
//! bytes and never the file's contents.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use shared_crypto::{PublicKey, Signer, SigningKey, Zeroizing, random_32};
use shared_proto::{InstructionError, JobInstruction};
use sqlx::PgPool;

use crate::config::InstructionKeyConfig;

/// The most bytes read from the key file; a P-256 PKCS#8 document is about a hundred.
const MAX_KEY_FILE_LEN: u64 = 4096;

/// Why the instruction key could not be made ready.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("instruction_key.path must be an absolute path naming a file in an existing directory")]
    Path,
    #[error("cannot {op} the instruction key file: {kind}")]
    Io {
        op: &'static str,
        kind: io::ErrorKind,
    },
    #[error("the instruction key path is not a regular file")]
    NotRegularFile,
    #[error(
        "the instruction key file is accessible to group or others; it must be readable and \
         writable by the server's account only (mode 0600)"
    )]
    FileTooOpen,
    #[error(
        "the instruction key file's directory is writable by group or others; it must be \
         writable by the server's account only"
    )]
    DirectoryWritable,
    #[error("the instruction key file is not a PKCS#8 P-256 private key")]
    NotAKey,
    #[error("operating-system randomness is unavailable")]
    Random,
    #[error(
        "the database records an active instruction key but its file is absent; restore the \
         file, or retire the record (ADR-462) to start a new deployment key"
    )]
    FileMissing,
    #[error("the instruction key file holds a different key from the one the database records")]
    Differs,
    #[error(
        "the database holds only a retired record of this instruction key; the file is \
         not used again (remove it to create a new key)"
    )]
    Retired,
    #[error("cannot record or read the instruction key in the database: {0}")]
    Database(#[from] sqlx::Error),
}

/// Why [`InstructionKey::seal`] failed.
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error(transparent)]
    Instruction(#[from] InstructionError),
    #[error(transparent)]
    Seal(#[from] shared_crypto::instruction::SealError),
}

/// The key that signs job instructions. Cheap to clone behind an [`Arc`].
///
/// It holds a [`Signer`], not key bytes, so that a signing service or a hardware module can
/// stand in for the file later. `seal` is `async` for the same reason: a signing service is
/// network I/O, and the callers (the job queue) then never change.
pub struct InstructionKey {
    signer: Arc<dyn Signer + Send + Sync>,
}

impl std::fmt::Debug for InstructionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InstructionKey({})", self.key_id())
    }
}

impl InstructionKey {
    /// Wraps a signer. Tests, and the signing service later.
    ///
    /// `shared_crypto::instruction::seal` asks the signer for its public key on every call, so a
    /// signer that reaches a module or a network must cache it.
    pub fn from_signer(signer: Arc<dyn Signer + Send + Sync>) -> InstructionKey {
        InstructionKey { signer }
    }

    /// The public key that verifies what this key signs: what registration hands to an agent.
    pub fn public_key(&self) -> PublicKey {
        self.signer.public_key()
    }

    /// The key's ID: 64 lowercase hex digits, the SHA-256 of its SPKI document. The value of
    /// `jobs.signing_key_id` and of `instruction_keys.key_id`.
    pub fn key_id(&self) -> String {
        self.public_key().fingerprint_hex()
    }

    /// Encodes `instruction` and signs it into the envelope the server delivers (ADR-461).
    pub async fn seal(&self, instruction: &JobInstruction) -> Result<Vec<u8>, SealError> {
        let payload = instruction.encode()?;
        Ok(shared_crypto::instruction::seal(
            self.signer.as_ref(),
            &payload,
        )?)
    }

    /// Makes the key ready at start: reads or creates the file and checks it against the
    /// database (the table in the module's documentation). The pool must be migrated.
    pub async fn open(
        config: &InstructionKeyConfig,
        pool: &PgPool,
    ) -> Result<InstructionKey, KeyError> {
        let path = config.path.as_path();
        let key = match read_key(path)? {
            Some(key) => key,
            None => {
                // Absent. A recorded key whose file is gone is never replaced silently. A
                // creator links the file before it records the key, so a row that appeared
                // since the first read means the file exists now: read it again before
                // refusing.
                if active(pool).await?.is_some() {
                    match read_key(path)? {
                        Some(key) => key,
                        None => return Err(KeyError::FileMissing),
                    }
                } else {
                    let (key, created) = create_or_load(path)?;
                    if created {
                        tracing::info!("instruction key file created");
                    }
                    key
                }
            }
        };
        pin(pool, &key.public_key()).await?;
        let key = InstructionKey::from_signer(Arc::new(key));
        tracing::info!(key_id = %key.key_id(), "instruction key ready");
        Ok(key)
    }
}

/// The active row: its key ID and SPKI.
async fn active(pool: &PgPool) -> Result<Option<(String, Vec<u8>)>, KeyError> {
    Ok(sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT key_id, public_key FROM instruction_keys WHERE retired_at IS NULL",
    )
    .fetch_optional(pool)
    .await?)
}

/// Records `public` as the active key unless the database already has one, then reads the active
/// row back and requires it to be this key. The partial unique index on the active row decides a
/// race, so the loser finds the winner's key here and is refused.
async fn pin(pool: &PgPool, public: &PublicKey) -> Result<(), KeyError> {
    let id = public.fingerprint_hex();
    let der = public.to_spki_der();
    sqlx::query(
        "INSERT INTO instruction_keys (key_id, public_key) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(&id)
    .bind(&der)
    .execute(pool)
    .await?;
    match active(pool).await? {
        Some((key_id, stored)) if key_id == id && stored == der => Ok(()),
        Some(_) => Err(KeyError::Differs),
        None => Err(KeyError::Retired),
    }
}

/// The key file's directory, after checking that the path can name a file.
fn directory(path: &Path) -> Result<&Path, KeyError> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(KeyError::Path);
    }
    path.parent().ok_or(KeyError::Path)
}

fn io_error(op: &'static str) -> impl FnOnce(io::Error) -> KeyError {
    move |error| KeyError::Io {
        op,
        kind: error.kind(),
    }
}

/// Refuses a directory that group or others can write to (Unix). On Windows the directory's ACL,
/// set when the service is installed, is the protection.
#[cfg(unix)]
fn check_directory(dir: &Path) -> Result<(), KeyError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(dir).map_err(io_error("inspect the directory of"))?;
    if !meta.is_dir() {
        return Err(KeyError::Path);
    }
    if meta.permissions().mode() & 0o022 != 0 {
        return Err(KeyError::DirectoryWritable);
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_directory(dir: &Path) -> Result<(), KeyError> {
    let meta = std::fs::metadata(dir).map_err(io_error("inspect the directory of"))?;
    if !meta.is_dir() {
        return Err(KeyError::Path);
    }
    Ok(())
}

/// Refuses an opened file that is not a regular file or, on Unix, that has any group or other
/// permission bit. Judged on the open handle, so the file cannot change between check and read.
#[cfg(unix)]
fn check_file(meta: &std::fs::Metadata) -> Result<(), KeyError> {
    use std::os::unix::fs::PermissionsExt;
    if !meta.is_file() {
        return Err(KeyError::NotRegularFile);
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(KeyError::FileTooOpen);
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_file(meta: &std::fs::Metadata) -> Result<(), KeyError> {
    if !meta.is_file() {
        return Err(KeyError::NotRegularFile);
    }
    Ok(())
}

/// Reads the key file: `None` when it is absent. Anything else that stops a read is an error;
/// a file is never overwritten here.
pub(crate) fn read_key(path: &Path) -> Result<Option<SigningKey>, KeyError> {
    check_directory(directory(path)?)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("open")(error)),
    };
    check_file(&file.metadata().map_err(io_error("inspect"))?)?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_KEY_FILE_LEN + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error("read"))?;
    if bytes.len() as u64 > MAX_KEY_FILE_LEN {
        return Err(KeyError::NotAKey);
    }
    SigningKey::from_pkcs8_der(&bytes)
        .map(Some)
        .map_err(|_| KeyError::NotAKey)
}

/// Creates the key file if it is still absent and returns the key that ends up in it, and whether
/// this call created it. The bytes go to a temporary file of a name no other attempt has (mode
/// 0600 on Unix), are synced, and are then hard-linked to the final name, which fails when the
/// name exists: so the file appears complete or not at all, and of any number of creators one
/// wins and the others load its key.
pub(crate) fn create_or_load(path: &Path) -> Result<(SigningKey, bool), KeyError> {
    let dir = directory(path)?;
    check_directory(dir)?;
    let key = SigningKey::generate().map_err(|_| KeyError::Random)?;
    let bytes = key.to_pkcs8_der();
    let suffix: String = random_32()
        .map_err(|_| KeyError::Random)?
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect();
    let tmp = dir.join(format!(".instruction-key-{suffix}.tmp"));

    #[cfg(unix)]
    let options = {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        options
    };
    #[cfg(not(unix))]
    let options = {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        options
    };

    let written = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_error("write")(error));
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {
            sync_directory(dir).map_err(io_error("sync the directory of"))?;
            // What the file holds is what was just written; read it back as every start does.
            match read_key(path)? {
                Some(read) => Ok((read, true)),
                None => Err(KeyError::Io {
                    op: "read back",
                    kind: io::ErrorKind::NotFound,
                }),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // Someone else created it first: theirs is the key.
            match read_key(path)? {
                Some(read) => Ok((read, false)),
                None => Err(KeyError::Io {
                    op: "read",
                    kind: io::ErrorKind::NotFound,
                }),
            }
        }
        Err(error) => Err(io_error("link")(error)),
    }
}

/// Makes the new directory entry durable. Windows cannot sync a directory; the file's own sync
/// covers its entry there.
#[cfg(unix)]
fn sync_directory(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_crypto::instruction::{Envelope, verify};
    use shared_proto::{AgentId, INSTRUCTION_VERSION, JobId, JobKind, VciId};

    fn instruction() -> JobInstruction {
        JobInstruction {
            version: INSTRUCTION_VERSION,
            job_id: JobId("0190f5a0-1b2c-7d3e-8f40-516273849a0b".to_owned()),
            ownership_generation: 3,
            agent_id: AgentId("agent-1".to_owned()),
            operator_id: "operator-1".to_owned(),
            kind: JobKind::Acquire,
            vci: VciId("vci-a".to_owned()),
            vin: None,
            document_hashes: vec![random_32().unwrap()],
            artifact_hash: None,
            plan_hash: None,
            issued_at: 1_000,
            start_deadline: 2_000,
        }
    }

    fn key_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("instruction.key")
    }

    #[test]
    fn a_created_key_is_read_back_as_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&dir);
        assert!(read_key(&path).unwrap().is_none());
        let (key, created) = create_or_load(&path).unwrap();
        assert!(created);
        let again = read_key(&path).unwrap().unwrap();
        assert_eq!(again.public_key(), key.public_key());
        // A second creator finds the file and takes its key.
        let (other, created) = create_or_load(&path).unwrap();
        assert!(!created);
        assert_eq!(other.public_key(), key.public_key());
        // No temporary file is left beside the key.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("instruction.key")]);
    }

    #[cfg(unix)]
    #[test]
    fn the_created_file_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&dir);
        create_or_load(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_sealed_instruction_splits_verifies_and_decodes() {
        let key = InstructionKey::from_signer(Arc::new(SigningKey::generate().unwrap()));
        let body = instruction();
        let blob = key.seal(&body).await.unwrap();
        let envelope = Envelope::split(&blob).unwrap();
        assert_eq!(
            hex(envelope.fingerprint()),
            key.key_id(),
            "the envelope names the key by its ID"
        );
        verify(
            &key.public_key(),
            envelope.payload(),
            &envelope.signature().unwrap(),
        )
        .unwrap();
        assert_eq!(JobInstruction::decode(envelope.payload()).unwrap(), body);
    }

    #[tokio::test]
    async fn another_key_does_not_verify_what_was_sealed() {
        let key = InstructionKey::from_signer(Arc::new(SigningKey::generate().unwrap()));
        let other = SigningKey::generate().unwrap().public_key();
        let blob = key.seal(&instruction()).await.unwrap();
        let envelope = Envelope::split(&blob).unwrap();
        assert!(verify(&other, envelope.payload(), &envelope.signature().unwrap()).is_err());
    }

    #[tokio::test]
    async fn an_instruction_the_layout_refuses_is_not_sealed() {
        let key = InstructionKey::from_signer(Arc::new(SigningKey::generate().unwrap()));
        let mut body = instruction();
        body.version += 1;
        assert!(matches!(
            key.seal(&body).await,
            Err(SealError::Instruction(_))
        ));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn garbage_and_an_oversize_file_are_refused_and_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&dir);
        for content in [
            random_32().unwrap().to_vec(),
            Vec::new(),
            vec![0x30; 10_000],
        ] {
            write_private(&path, &content);
            assert!(matches!(read_key(&path), Err(KeyError::NotAKey)));
            assert_eq!(std::fs::read(&path).unwrap(), content);
            // Creating never replaces what is there: the creator loads it, and it is refused.
            assert!(matches!(create_or_load(&path), Err(KeyError::NotAKey)));
            assert_eq!(std::fs::read(&path).unwrap(), content);
            std::fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn a_public_key_document_is_not_a_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&dir);
        let public = SigningKey::generate().unwrap().public_key().to_spki_der();
        write_private(&path, &public);
        assert!(matches!(read_key(&path), Err(KeyError::NotAKey)));
    }

    #[test]
    fn a_directory_at_the_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&dir);
        std::fs::create_dir(&path).unwrap();
        assert!(read_key(&path).is_err());
        assert!(create_or_load(&path).is_err());
        assert!(path.is_dir());
    }

    #[test]
    fn a_relative_path_or_a_missing_directory_is_refused() {
        assert!(matches!(
            read_key(Path::new("instruction.key")),
            Err(KeyError::Path)
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("instruction.key");
        assert!(matches!(
            read_key(&path),
            Err(KeyError::Io {
                kind: io::ErrorKind::NotFound,
                ..
            })
        ));
        assert!(create_or_load(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_file_with_a_group_or_other_bit_is_refused_and_left_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&dir);
        let (key, _) = create_or_load(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        for mode in [0o640, 0o604, 0o660, 0o666, 0o644, 0o601] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                matches!(read_key(&path), Err(KeyError::FileTooOpen)),
                "{mode:o}"
            );
            assert!(matches!(create_or_load(&path), Err(KeyError::FileTooOpen)));
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert_eq!(
            read_key(&path).unwrap().unwrap().public_key(),
            key.public_key()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_others_can_write_to_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&dir);
        for mode in [0o770, 0o707, 0o777] {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(read_key(&path), Err(KeyError::DirectoryWritable)));
            assert!(matches!(
                create_or_load(&path),
                Err(KeyError::DirectoryWritable)
            ));
            assert!(!path.exists());
        }
        // Readable by others is fine; only writability matters.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(create_or_load(&path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_regular_private_file_is_followed_and_to_a_directory_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.key");
        let (key, _) = create_or_load(&target).unwrap();
        let link = key_path(&dir);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            read_key(&link).unwrap().unwrap().public_key(),
            key.public_key()
        );
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        assert!(matches!(read_key(&link), Err(KeyError::NotRegularFile)));
    }

    #[test]
    fn sixteen_creators_at_one_path_get_one_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&dir);
        let start = Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let (path, start) = (path.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    let (key, created) = create_or_load(&path).unwrap();
                    (key.public_key(), created)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let first = results[0].0;
        assert!(results.iter().all(|(key, _)| *key == first));
        assert_eq!(results.iter().filter(|(_, created)| *created).count(), 1);
        assert_eq!(read_key(&path).unwrap().unwrap().public_key(), first);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn debug_output_shows_the_key_id_and_no_secret() {
        let signing = SigningKey::generate().unwrap();
        let secret = hex(&signing.to_pkcs8_der());
        let key = InstructionKey::from_signer(Arc::new(signing));
        let shown = format!("{key:?} {key:#?}");
        assert!(shown.contains(&key.key_id()));
        assert!(!shown.contains(&secret[secret.len() - 64..]), "{shown}");
        assert_eq!(key.key_id().len(), 64);
        let error = KeyError::NotAKey.to_string();
        assert!(!error.contains(&secret[..16]));
    }

    /// Writes `content` to `path` readable by the owner only.
    fn write_private(path: &Path, content: &[u8]) {
        std::fs::write(path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}
