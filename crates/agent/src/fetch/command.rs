//! `ngr-agent debug fetch`: one fetch for one job's instruction and one digest, so that a test
//! can drive the download leg end to end (design 13.4, ADR-497). It exists in a debug build only:
//! this module is not compiled in a release build, and the binary's parser then does not know
//! the command.
//!
//! It reads the registration record without the control channel's locks (the cache has its own
//! lock), verifies the envelope file against the record's instruction key and the agent's ID,
//! loads the agent key, opens the cache and runs [`Fetcher::fetch_retrying`]. It prints the
//! cached file's path and length and nothing the server sent.

use std::path::PathBuf;

use worker_host::service::OperatingMode;

use super::store::ObjectStore;
use super::{FetchError, Fetched, Fetcher, Kind};
use crate::identity::agent_key;
use crate::instruction;
use crate::registration::origin::ServerOrigin;
use crate::registration::record::{self, Record};
use crate::state;

/// The most bytes read from the envelope file: an envelope holds at most a 64 KiB payload.
const MAX_ENVELOPE_FILE: u64 = 128 * 1024;

/// What the command takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub mode: OperatingMode,
    /// A file holding one signed instruction envelope, as the server delivered it.
    pub instruction: PathBuf,
    pub kind: Kind,
    pub sha256: [u8; 32],
    /// The most attempts, with the back-off between them.
    pub attempts: u32,
}

/// Why the command failed: the exit status and a fixed text.
#[derive(Debug)]
pub struct CommandError {
    pub exit: u8,
    pub message: String,
}

pub(crate) fn fail(exit: u8, message: impl Into<String>) -> CommandError {
    CommandError {
        exit,
        message: message.into(),
    }
}

/// The registration record of `mode`, read without the control channel's locks, which must name
/// `installed`. Exit 1: not registered, or a record that cannot be used, or another server's; exit
/// 3: the state directory is unusable. Shared with `ngr-agent debug enqueue`.
pub(crate) fn installed_record(
    mode: OperatingMode,
    installed: &ServerOrigin,
) -> Result<Record, CommandError> {
    let dir = state::private_subdir(mode, record::DIR_NAME)
        .map_err(|error| fail(3, format!("the state directory is unusable: {error}")))?;
    let record = Record::read(&dir.join(record::RECORD_FILE)).map_err(|_| {
        fail(
            1,
            "not registered, or the registration record cannot be used",
        )
    })?;
    if record.server_origin != installed.as_str() {
        return Err(fail(
            1,
            "the registration record names another server than the installation file",
        ));
    }
    Ok(record)
}

/// Runs one fetch. Exit 1: it cannot as asked (not registered, an envelope that does not verify,
/// an object the instruction does not name or the server refuses, a digest that did not match
/// twice in a row); 3: it may work later (no answer, an exchange refused, an answer the server did not prove, the disk, the cache
/// locked, the key store), which includes a single digest mismatch when `--attempts` leaves no
/// second attempt.
pub async fn run(args: Args, installed: &ServerOrigin) -> Result<Fetched, CommandError> {
    let mode = args.mode;
    let record = installed_record(mode, installed)?;
    let bytes = read_envelope(&args.instruction)?;
    let trusted = shared_crypto::PublicKey::from_spki_pem(&record.instruction_key_pem)
        .map_err(|_| fail(1, "the registration record's instruction key is not valid"))?;
    let verified = instruction::open(&bytes, &trusted, &record.agent_id)
        .map_err(|reason| fail(1, format!("the instruction was refused: {reason:?}")))?;
    let key = tokio::task::spawn_blocking(move || agent_key(mode))
        .await
        .map_err(|_| fail(1, "the key load failed unexpectedly"))?
        .map_err(|error| {
            let exit = if error.retry_class().is_some() { 3 } else { 1 };
            fail(exit, error.to_string())
        })?;
    if key.created() {
        return Err(fail(1, "the agent key was just created; register again"));
    }
    let store = ObjectStore::open(mode).map_err(|error| fail(3, error.to_string()))?;
    let fetcher = Fetcher::new(&record, &key, store)
        .map_err(|_| fail(1, "the agent key is not the registered one"))?;
    fetcher
        .fetch_retrying(&verified, args.kind, &args.sha256, args.attempts)
        .await
        .map_err(|error| {
            let exit = if error.may_retry() || matches!(error, FetchError::Local { .. }) {
                3
            } else {
                1
            };
            fail(exit, format!("the fetch failed: {}", error.class()))
        })
}

fn read_envelope(path: &std::path::Path) -> Result<Vec<u8>, CommandError> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_ENVELOPE_FILE + 1).read_to_end(&mut bytes))
        .map_err(|error| {
            fail(
                1,
                format!("the instruction file cannot be read: {:?}", error.kind()),
            )
        })?;
    if bytes.len() as u64 > MAX_ENVELOPE_FILE {
        return Err(fail(1, "the instruction file is too large"));
    }
    Ok(bytes)
}
