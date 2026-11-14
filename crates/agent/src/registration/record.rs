//! The registration record: the one file `ngr-agent register` leaves behind (design 6.4,
//! ADR-470), `<state>/registration/registration.v1.json`.
//!
//! It holds what the agent needs to trust the server and to prove itself to it later: the agent
//! ID, owner ID, preset and remote use the server assigned (the server signed them), the
//! server's canonical origin (the audience the connection proof signs, ADR-466 Decision item 4),
//! the instruction key's PEM and ID (the key the instruction gate verifies jobs with, ADR-461
//! Decision item 11) and the fingerprint of the agent key it was registered with. The record
//! holds no secret. The mode is not in it: the state directory belongs to a mode.
//!
//! [`Record::read`] is the strict reader for the control channel session library: a field that
//! is unknown or missing, or a value that breaks the rules the writer's checks enforce, is an
//! error. [`PendingRecord`] is the writer: it makes the temporary file before the request is
//! sent, and after the answer it writes, syncs and renames it over the final name, so the old
//! record is touched by nothing but that rename.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shared_crypto::PublicKey;
use shared_crypto::proof::Audience;
use shared_crypto::registration::{MAX_FIELD_LEN, Preset};

use super::origin::ServerOrigin;

/// The sub-directory of the state directory that holds the registration files.
pub const DIR_NAME: &str = "registration";
/// The record's file name.
pub const RECORD_FILE: &str = "registration.v1.json";
/// The temporary file the record is written to before it replaces the old one.
pub const TEMP_FILE: &str = "registration.v1.json.tmp";
/// The lock file: held for the whole `register` command, and never deleted.
pub const LOCK_FILE: &str = "registration.lock";

/// The largest record the reader takes, in bytes.
const MAX_RECORD: u64 = 16 * 1024;

/// The record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub agent_id: String,
    pub owner_id: String,
    /// The preset's name in the Web API (`personal`, `personalRemote`, `shared`).
    pub preset: String,
    pub allow_remote: bool,
    /// The server's canonical origin.
    pub server_origin: String,
    /// The instruction key, as an SPKI PEM.
    pub instruction_key_pem: String,
    /// The instruction key's ID: the lowercase hex of its fingerprint.
    pub instruction_key_id: String,
    /// The fingerprint of the agent key, in lowercase hex.
    pub agent_key_fingerprint: String,
}

/// Why a record cannot be read.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("the registration record cannot be read: {0:?}")]
    Io(io::ErrorKind),
    #[error("the registration record is not a regular file or is too large")]
    NotARecord,
    #[error("the registration record is not the expected JSON object")]
    Json,
    #[error("the registration record's {0} is not valid")]
    Field(&'static str),
}

impl Record {
    /// Reads and checks the record at `path`. A symbolic link, a directory or a file over the
    /// limit is refused.
    pub fn read(path: &Path) -> Result<Record, RecordError> {
        let meta = fs::symlink_metadata(path).map_err(|e| RecordError::Io(e.kind()))?;
        if !meta.is_file() || meta.len() > MAX_RECORD {
            return Err(RecordError::NotARecord);
        }
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|file| file.take(MAX_RECORD + 1).read_to_end(&mut bytes))
            .map_err(|e| RecordError::Io(e.kind()))?;
        if bytes.len() as u64 > MAX_RECORD {
            return Err(RecordError::NotARecord);
        }
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| RecordError::Json)?;
        record.check()?;
        Ok(record)
    }

    /// The rules every field of a record holds to.
    fn check(&self) -> Result<(), RecordError> {
        if !shared_proto::is_canonical_uuid(&self.agent_id) {
            return Err(RecordError::Field("agent_id"));
        }
        if self.owner_id.is_empty() || self.owner_id.len() > MAX_FIELD_LEN {
            return Err(RecordError::Field("owner_id"));
        }
        if Preset::parse(&self.preset).is_none() {
            return Err(RecordError::Field("preset"));
        }
        let origin = Audience::parse_canonical(&self.server_origin)
            .ok()
            .and_then(|audience| ServerOrigin::parse(audience.as_str()).ok());
        if origin.is_none_or(|origin| origin.as_str() != self.server_origin) {
            return Err(RecordError::Field("server_origin"));
        }
        let key = PublicKey::from_spki_pem(&self.instruction_key_pem)
            .ok()
            .filter(|key| key.to_spki_pem() == self.instruction_key_pem)
            .ok_or(RecordError::Field("instruction_key_pem"))?;
        if self.instruction_key_id != key.fingerprint_hex() {
            return Err(RecordError::Field("instruction_key_id"));
        }
        let fingerprint = &self.agent_key_fingerprint;
        if fingerprint.len() != 64
            || !fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(RecordError::Field("agent_key_fingerprint"));
        }
        Ok(())
    }
}

/// How firmly a stored record is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// The file, its name and (on Unix) the directory were synced.
    Synced,
    /// The record is in place, but syncing the directory failed after the rename.
    DirectoryNotSynced,
}

/// The temporary file of a record about to be written, made before anything is sent. It is
/// removed unless [`PendingRecord::commit`] renames it, so no outcome leaves it behind.
pub struct PendingRecord {
    dir: PathBuf,
    file: Option<File>,
    armed: bool,
}

impl PendingRecord {
    /// Removes a stale temporary file and creates a new one with mode 0600 (on Windows the
    /// profile's inherited access list stands in for that). An entry that cannot be removed or
    /// created fails here, before the request.
    pub fn prepare(dir: &Path) -> io::Result<PendingRecord> {
        let tmp = dir.join(TEMP_FILE);
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let file = create_private(&tmp)?;
        Ok(PendingRecord {
            dir: dir.to_owned(),
            file: Some(file),
            armed: true,
        })
    }

    /// Writes `record`, syncs it, renames it over the final name and syncs the directory (on
    /// Windows, reopens the final file and syncs it). An error before or at the rename leaves the
    /// old record intact and removes the temporary file. A directory sync that fails after the
    /// rename is reported as [`Durability::DirectoryNotSynced`], since the record is in place.
    pub fn commit(mut self, record: &Record) -> io::Result<Durability> {
        let mut file = self
            .file
            .take()
            .expect("a pending record is committed once");
        let mut bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(self.dir.join(TEMP_FILE), self.dir.join(RECORD_FILE))?;
        self.armed = false;
        Ok(
            match crate::journal::sync_after_rename(&self.dir, &self.dir.join(RECORD_FILE)) {
                Ok(()) => Durability::Synced,
                Err(_) => Durability::DirectoryNotSynced,
            },
        )
    }
}

impl Drop for PendingRecord {
    fn drop(&mut self) {
        if self.armed {
            self.file = None;
            let _ = fs::remove_file(self.dir.join(TEMP_FILE));
        }
    }
}

#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_crypto::{Signer, SigningKey};

    fn sample() -> Record {
        let instruction = SigningKey::generate().unwrap().public_key();
        let agent = SigningKey::generate().unwrap().public_key();
        Record {
            agent_id: "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e".to_owned(),
            owner_id: "7b1d0c2e-4a5f-4c6d-9e8f-0a1b2c3d4e5f".to_owned(),
            preset: "personal".to_owned(),
            allow_remote: false,
            server_origin: "http://127.0.0.1:8080".to_owned(),
            instruction_key_pem: instruction.to_spki_pem(),
            instruction_key_id: instruction.fingerprint_hex(),
            agent_key_fingerprint: agent.fingerprint_hex(),
        }
    }

    fn write(dir: &Path, record: &Record) -> PathBuf {
        let path = dir.join(RECORD_FILE);
        fs::write(&path, serde_json::to_vec(record).unwrap()).unwrap();
        path
    }

    #[test]
    fn a_written_record_is_read_back_and_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let record = sample();
        let pending = PendingRecord::prepare(dir.path()).unwrap();
        assert!(dir.path().join(TEMP_FILE).exists());
        assert_eq!(pending.commit(&record).unwrap(), Durability::Synced);
        assert!(!dir.path().join(TEMP_FILE).exists());
        assert_eq!(Record::read(&dir.path().join(RECORD_FILE)).unwrap(), record);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(RECORD_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// The record has the fields of ADR-470 and no mode.
    #[test]
    fn the_record_has_exactly_its_eight_fields() {
        let json = serde_json::to_value(sample()).unwrap();
        let mut names: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        names.sort();
        assert_eq!(
            names,
            [
                "agent_id",
                "agent_key_fingerprint",
                "allow_remote",
                "instruction_key_id",
                "instruction_key_pem",
                "owner_id",
                "preset",
                "server_origin"
            ]
        );
    }

    #[test]
    fn a_stale_temp_is_replaced_and_a_dropped_pending_record_removes_its_temp() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(TEMP_FILE), b"stale").unwrap();
        let pending = PendingRecord::prepare(dir.path()).unwrap();
        assert_eq!(fs::metadata(dir.path().join(TEMP_FILE)).unwrap().len(), 0);
        drop(pending);
        assert!(!dir.path().join(TEMP_FILE).exists());
    }

    /// R5: a temp that cannot be removed fails the pre-flight.
    #[test]
    fn a_temp_that_is_a_non_empty_directory_fails_the_preflight() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(TEMP_FILE)).unwrap();
        fs::write(dir.path().join(TEMP_FILE).join("x"), b"x").unwrap();
        assert!(PendingRecord::prepare(dir.path()).is_err());
    }

    /// W3: a rename that fails leaves the old record and no temp.
    #[test]
    fn a_failed_rename_leaves_the_old_record_and_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the record belongs: the rename of a file over it fails.
        fs::create_dir(dir.path().join(RECORD_FILE)).unwrap();
        let pending = PendingRecord::prepare(dir.path()).unwrap();
        assert!(pending.commit(&sample()).is_err());
        assert!(!dir.path().join(TEMP_FILE).exists());
        assert!(dir.path().join(RECORD_FILE).is_dir());
    }

    #[test]
    fn a_new_record_replaces_the_old_one_whole() {
        let dir = tempfile::tempdir().unwrap();
        let first = sample();
        let path = write(dir.path(), &first);
        let second = sample();
        PendingRecord::prepare(dir.path())
            .unwrap()
            .commit(&second)
            .unwrap();
        assert_eq!(Record::read(&path).unwrap(), second);
    }

    #[test]
    fn the_reader_refuses_unknown_missing_and_invalid_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RECORD_FILE);
        let mut json = serde_json::to_value(sample()).unwrap();
        json["mode"] = "user".into();
        fs::write(&path, json.to_string()).unwrap();
        assert!(matches!(Record::read(&path), Err(RecordError::Json)));
        let mut json = serde_json::to_value(sample()).unwrap();
        json.as_object_mut().unwrap().remove("allow_remote");
        fs::write(&path, json.to_string()).unwrap();
        assert!(matches!(Record::read(&path), Err(RecordError::Json)));
        fs::write(&path, b"[]").unwrap();
        assert!(matches!(Record::read(&path), Err(RecordError::Json)));

        type Break = (&'static str, fn(&mut Record));
        let breaks: [Break; 7] = [
            ("agent_id", |r| r.agent_id = r.agent_id.to_uppercase()),
            ("owner_id", |r| r.owner_id.clear()),
            ("preset", |r| r.preset = "other".to_owned()),
            ("server_origin", |r| {
                r.server_origin = "http://localhost:1".to_owned()
            }),
            ("instruction_key_pem", |r| r.instruction_key_pem.push('\n')),
            ("instruction_key_id", |r| {
                r.instruction_key_id = "0".repeat(64)
            }),
            ("agent_key_fingerprint", |r| {
                r.agent_key_fingerprint = r.agent_key_fingerprint.to_uppercase()
            }),
        ];
        for (field, change) in breaks {
            let mut record = sample();
            change(&mut record);
            write(dir.path(), &record);
            match Record::read(&path) {
                Err(RecordError::Field(name)) => assert_eq!(name, field),
                other => panic!("{field}: {other:?}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_reader_refuses_a_symlink_a_directory_and_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = write(dir.path(), &sample());
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(Record::read(&link), Err(RecordError::NotARecord)));
        assert!(matches!(
            Record::read(dir.path()),
            Err(RecordError::NotARecord)
        ));
        assert!(matches!(
            Record::read(&dir.path().join("none")),
            Err(RecordError::Io(io::ErrorKind::NotFound))
        ));
    }
}
