//! The outbox: the agent's durable queue of events the server must receive (design 5.3,
//! ADR-478).
//!
//! One file per agent ID, `<state>/outbox/<agent-id>.outbox`, holds the events from the first one
//! the server has not confirmed on. Layout: the magic, the format version (`u32` LE), then frames
//! in the journal's format (`len`, CRC-32, payload; [`crate::journal`]). The first frame is the
//! header (format of the file, the agent ID and the floor: the number of the last event the file
//! no longer holds); each later frame is one event, its number (`u64` LE) followed by the exact
//! text of the control frame that carries it, so a resend is byte-identical.
//!
//! [`Outbox::append`] gives an event the next number inside the synced write: the number is the
//! highest assigned plus one, so numbers are at least 1 and contiguous under one agent ID, and an
//! event is pending only once its frame is durable. A frame that a crash tore is cut off at the
//! next open and its number is used again. Acknowledgements ([`Outbox::ack`]) change memory only;
//! the acknowledged prefix leaves the file by a rewrite (a temporary file, synced, renamed over
//! the old one, the directory synced) when nothing is pending or 64 KiB are acknowledged, and
//! the header's floor keeps the counter across an empty file. After a restart an event that was
//! acknowledged but not yet rewritten away is pending again and is sent again; the server
//! deduplicates by number.
//!
//! The file is capped; nothing is ever dropped to make room. A full outbox refuses the append
//! ([`OutboxError::Full`]) and the producer keeps the fact in its own journal. After a failed
//! write the outbox refuses appends until it is opened again, since a failed sync can lose data
//! the OS had accepted. A file that does not read back is [`OutboxError::Corrupt`] and is never
//! repaired or set aside.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shared_proto::MAX_CONTROL_MESSAGE_LEN;
use worker_host::service::OperatingMode;

use crate::journal::{
    FRAME_HEADER, FrameRead, JournalError, WriterLock, push_frame, read_frame, record_follows,
    sync_after_rename,
};
use crate::state::{StateDirError, private_subdir};

/// The outbox format this build writes and reads.
pub const FORMAT_VERSION: u32 = 1;
/// The largest the file may grow, in bytes. An append that would pass it is refused.
pub const MAX_OUTBOX_BYTES: u64 = 16 << 20;
/// Above this size a `received` event is refused, which keeps the room above it for the events
/// that finish jobs already accepted.
pub const ADMISSION_BYTES: u64 = 8 << 20;
/// The longest event text, in bytes: a control message is at most this long (ADR-466).
pub const MAX_EVENT_TEXT: usize = MAX_CONTROL_MESSAGE_LEN;
/// Acknowledged bytes at which the file is rewritten without them.
const REWRITE_BYTES: u64 = 64 << 10;

const MAGIC: [u8; 8] = *b"NGROUTB\0";
/// Magic and format version.
const PREAMBLE: usize = MAGIC.len() + 4;
/// The number in front of an event's text.
const SEQ_LEN: usize = 8;
const DIR_NAME: &str = "outbox";
const EXTENSION: &str = ".outbox";
const LOCK_FILE: &str = "outbox.lock";
/// The longest a frame that a crash left unfinished can be.
const MAX_TORN: usize = FRAME_HEADER + SEQ_LEN + MAX_EVENT_TEXT;

/// What an event is, for the room it may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A `received` event: new work, refused above [`ADMISSION_BYTES`].
    Received,
    /// Any other event: refused only at [`MAX_OUTBOX_BYTES`].
    Other,
}

/// A point of the write path, for fault injection in tests (a failure the hook returns is
/// handled as one of the file system's; a hook that aborts models a crash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    BeforeWrite,
    AfterWrite,
    AfterSync,
    BeforeRename,
    AfterRename,
}

/// The hook called at each [`Step`].
pub type Probe = Box<dyn Fn(Step) -> io::Result<()> + Send>;

#[derive(Debug, thiserror::Error)]
pub enum OutboxError {
    #[error("outbox i/o: {0}")]
    Io(#[from] io::Error),
    #[error("the outbox directory: {0}")]
    State(#[from] StateDirError),
    #[error("the outbox file is corrupt: {0}")]
    Corrupt(&'static str),
    /// Another outbox of this directory is open for writing, in this process or another.
    #[error("the outbox is open for writing elsewhere")]
    InUse,
    #[error("the agent ID cannot name an outbox file")]
    BadAgentId,
    #[error("the outbox is full")]
    Full,
    #[error("the event text is longer than a control message")]
    TooLarge,
    /// An earlier write failed; open the outbox again.
    #[error("an earlier write to the outbox failed")]
    Poisoned,
    /// An acknowledgement above the highest number assigned.
    #[error("acknowledged {acked}, but the highest event is {highest}")]
    OutOfRange { acked: u64, highest: u64 },
}

#[derive(Serialize, Deserialize)]
struct Header {
    agent_id: String,
    protection: u32,
    floor: u64,
}

struct Event {
    seq: u64,
    text: String,
}

impl Event {
    fn frame_len(&self) -> u64 {
        (FRAME_HEADER + SEQ_LEN + self.text.len()) as u64
    }
}

/// An open outbox, the only writer of its agent ID's file.
pub struct Outbox {
    dir: PathBuf,
    path: PathBuf,
    agent_id: String,
    /// `None` only while the file is being replaced, or after a failure to reopen it.
    file: Option<File>,
    _lock: WriterLock,
    /// The highest number acknowledged; the header's floor after a rewrite.
    acked: u64,
    /// The highest number assigned.
    last: u64,
    /// The events above `acked`, in order.
    pending: VecDeque<Event>,
    file_len: u64,
    /// The bytes of acknowledged events still in the file.
    acked_bytes: u64,
    poisoned: bool,
    probe: Probe,
}

impl Outbox {
    /// Opens the outbox of `agent_id` in the agent's state directory.
    pub fn open(mode: OperatingMode, agent_id: &str) -> Result<Self, OutboxError> {
        Self::open_in(&private_subdir(mode, DIR_NAME)?, agent_id)
    }

    /// Opens the outbox of `agent_id` in `dir`, creating its file when it has none.
    pub fn open_in(dir: &Path, agent_id: &str) -> Result<Self, OutboxError> {
        Self::open_with(dir, agent_id, Box::new(|_| Ok(())))
    }

    /// [`Outbox::open_in`] with a hook on the write path, for tests.
    #[doc(hidden)]
    pub fn open_with(dir: &Path, agent_id: &str, probe: Probe) -> Result<Self, OutboxError> {
        if !valid_id(agent_id) {
            return Err(OutboxError::BadAgentId);
        }
        let lock = match WriterLock::at(&dir.join(LOCK_FILE)) {
            Ok(lock) => lock,
            Err(JournalError::InUse) => return Err(OutboxError::InUse),
            Err(JournalError::Io(error)) => return Err(error.into()),
            Err(other) => return Err(io::Error::other(other.to_string()).into()),
        };
        let path = dir.join(format!("{agent_id}{EXTENSION}"));
        let others = other_outboxes(dir, &path);
        if others > 0 {
            tracing::warn!(
                count = others,
                "the outbox directory holds outboxes of other agent IDs; leaving them alone"
            );
        }
        let mut outbox = Self {
            dir: dir.to_owned(),
            path,
            agent_id: agent_id.to_owned(),
            file: None,
            _lock: lock,
            acked: 0,
            last: 0,
            pending: VecDeque::new(),
            file_len: 0,
            acked_bytes: 0,
            poisoned: false,
            probe,
        };
        match fs::read(&outbox.path) {
            Ok(bytes) => outbox.load(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => outbox.install()?,
            Err(error) => return Err(error.into()),
        }
        Ok(outbox)
    }

    fn load(&mut self, bytes: &[u8]) -> Result<(), OutboxError> {
        let (floor, events, len) = parse(bytes, &self.agent_id)?;
        let file = open_rw(&self.path)?;
        if len < bytes.len() {
            tracing::warn!(
                cut = bytes.len() - len,
                "the outbox ends in a frame its append did not finish; cutting it off"
            );
            file.set_len(len as u64)?;
        }
        // A frame that landed after a failed sync may be only in the OS's cache; syncing here,
        // before the frames count as pending, narrows the window in which a restart sends an
        // event that a power loss would still take back. It also makes the cut durable.
        file.sync_all()?;
        match fs::remove_file(self.tmp_path()) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        self.file = Some(file);
        self.acked = floor;
        self.last = u64::try_from(events.len())
            .ok()
            .and_then(|count| floor.checked_add(count))
            .ok_or(OutboxError::Corrupt("the event counter overflows"))?;
        self.pending = events.into();
        self.file_len = len as u64;
        Ok(())
    }

    fn tmp_path(&self) -> PathBuf {
        self.dir.join(format!("{}{EXTENSION}.tmp", self.agent_id))
    }

    /// Gives the event the next number, asks `text` for the exact text of its control frame with
    /// that number in it, and makes it durable. Blocking (a file sync): the channel calls it on a
    /// blocking task. Returns the number; the event is pending from then on. A refusal
    /// ([`OutboxError::Full`], [`OutboxError::TooLarge`]) writes nothing and uses no number.
    pub fn append(
        &mut self,
        kind: Kind,
        text: impl FnOnce(u64) -> String,
    ) -> Result<u64, OutboxError> {
        if self.poisoned {
            return Err(OutboxError::Poisoned);
        }
        let seq = self
            .last
            .checked_add(1)
            .ok_or(OutboxError::Corrupt("the event counter overflows"))?;
        let text = text(seq);
        if text.len() > MAX_EVENT_TEXT {
            return Err(OutboxError::TooLarge);
        }
        let mut payload = Vec::with_capacity(SEQ_LEN + text.len());
        payload.extend_from_slice(&seq.to_le_bytes());
        payload.extend_from_slice(text.as_bytes());
        let mut frame = Vec::with_capacity(FRAME_HEADER + payload.len());
        push_frame(&mut frame, &payload).map_err(|_| OutboxError::TooLarge)?;
        let limit = match kind {
            Kind::Received => ADMISSION_BYTES,
            Kind::Other => MAX_OUTBOX_BYTES,
        };
        if self.file_len + frame.len() as u64 > limit {
            return Err(OutboxError::Full);
        }
        if let Err(error) = self.write(&frame) {
            self.poisoned = true;
            return Err(error.into());
        }
        self.last = seq;
        self.file_len += frame.len() as u64;
        self.pending.push_back(Event { seq, text });
        Ok(seq)
    }

    fn write(&mut self, frame: &[u8]) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("the outbox file is closed"))?;
        (self.probe)(Step::BeforeWrite)?;
        file.seek(SeekFrom::Start(self.file_len))?;
        file.write_all(frame)?;
        (self.probe)(Step::AfterWrite)?;
        file.sync_all()?;
        (self.probe)(Step::AfterSync)
    }

    /// The events not yet acknowledged, in order, each with its number and its exact text.
    pub fn pending(&self) -> impl Iterator<Item = (u64, &str)> + '_ {
        self.pending.iter().map(|e| (e.seq, e.text.as_str()))
    }

    /// The highest number assigned, 0 when none has been.
    pub fn highest(&self) -> u64 {
        self.last
    }

    /// Takes the server's cumulative acknowledgement of every event up to `n`: they leave the
    /// pending events. A number at or below the one already acknowledged changes nothing; one
    /// above [`Outbox::highest`] is [`OutboxError::OutOfRange`] and drops nothing. The file is
    /// rewritten without the acknowledged prefix when nothing is pending or 64 KiB are
    /// acknowledged; a rewrite that fails leaves the old file in place, is logged and is tried
    /// again at a later acknowledgement.
    pub fn ack(&mut self, n: u64) -> Result<(), OutboxError> {
        if n > self.last {
            return Err(OutboxError::OutOfRange {
                acked: n,
                highest: self.last,
            });
        }
        if n <= self.acked {
            return Ok(());
        }
        while self.pending.front().is_some_and(|event| event.seq <= n) {
            if let Some(event) = self.pending.pop_front() {
                self.acked_bytes += event.frame_len();
            }
        }
        self.acked = n;
        if !self.poisoned
            && (self.pending.is_empty() || self.acked_bytes >= REWRITE_BYTES)
            && let Err(error) = self.install()
        {
            tracing::warn!(kind = ?error.kind(), "rewriting the outbox failed; keeping the old file");
        }
        Ok(())
    }

    /// Writes the file whole (header with the floor `acked`, then the pending events) under a
    /// temporary name, syncs it and renames it over the outbox's file; the same routine creates
    /// the file. The old handle is closed before the rename, which Windows needs. A failure
    /// before the rename leaves the old file as it was. After the rename the outbox is poisoned
    /// when the handle cannot be reopened (the same holds if the rename itself failed and the
    /// old file cannot be reopened) or when the sync after the rename fails (a rewrite only; the
    /// creation of the file returns the error and the open fails).
    fn install(&mut self) -> io::Result<()> {
        let tmp = self.tmp_path();
        let bytes = encode_file(&self.agent_id, self.acked, self.pending.iter())?;
        let prepared = (|| {
            match fs::remove_file(&tmp) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            (self.probe)(Step::BeforeRename)
        })();
        if let Err(error) = prepared {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        let had_file = self.file.take().is_some();
        if let Err(error) = fs::rename(&tmp, &self.path) {
            let _ = fs::remove_file(&tmp);
            if had_file {
                self.file = open_rw(&self.path).ok();
                self.poisoned = self.file.is_none();
            }
            return Err(error);
        }
        let _ = (self.probe)(Step::AfterRename);
        let synced = sync_after_rename(&self.dir, &self.path);
        self.file_len = bytes.len() as u64;
        self.acked_bytes = 0;
        match open_rw(&self.path) {
            Ok(file) => self.file = Some(file),
            Err(error) => {
                self.poisoned = true;
                return Err(error);
            }
        }
        if let Err(error) = synced {
            // The new file is in place but may not survive a power loss. Creating the file fails
            // the open; after a rewrite the outbox refuses appends until it is opened again.
            self.poisoned = had_file;
            return Err(error);
        }
        Ok(())
    }
}

fn open_rw(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

/// An agent ID names a file, so it is a short run of letters, digits and hyphens.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// How many outbox files of other agent IDs `dir` holds.
fn other_outboxes(dir: &Path, own: &Path) -> usize {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|e| e.path() != own && e.file_name().to_string_lossy().ends_with(EXTENSION))
            .count()
    })
}

fn encode_file<'a>(
    agent_id: &str,
    floor: u64,
    events: impl Iterator<Item = &'a Event>,
) -> io::Result<Vec<u8>> {
    let header = Header {
        agent_id: agent_id.to_owned(),
        protection: 0,
        floor,
    };
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    let too_large = |_| io::Error::other("an outbox frame is too large");
    let header = postcard::to_allocvec(&header).map_err(io::Error::other)?;
    push_frame(&mut out, &header).map_err(too_large)?;
    for event in events {
        let mut payload = event.seq.to_le_bytes().to_vec();
        payload.extend_from_slice(event.text.as_bytes());
        push_frame(&mut out, &payload).map_err(too_large)?;
    }
    Ok(out)
}

/// Reads a whole file: the floor, the events, and the length up to the end of the last whole
/// frame. Anything that is not one unfinished append at the end is corrupt.
fn parse(bytes: &[u8], agent_id: &str) -> Result<(u64, Vec<Event>, usize), OutboxError> {
    let corrupt = OutboxError::Corrupt;
    if bytes.len() < PREAMBLE || bytes[..MAGIC.len()] != MAGIC {
        return Err(corrupt("not an outbox"));
    }
    let version = u32::from_le_bytes(bytes[MAGIC.len()..PREAMBLE].try_into().expect("four bytes"));
    if version != FORMAT_VERSION {
        return Err(corrupt("unknown format version"));
    }
    let FrameRead::Ok { payload, next } = read_frame(bytes, PREAMBLE) else {
        return Err(corrupt("the header does not read back"));
    };
    let header: Header =
        postcard::from_bytes(payload).map_err(|_| corrupt("the header does not decode"))?;
    if header.agent_id != agent_id {
        return Err(corrupt("the file belongs to another agent ID"));
    }
    if header.protection != 0 {
        return Err(corrupt("unknown protection"));
    }
    let seq_of = |payload: &[u8]| {
        payload
            .first_chunk::<SEQ_LEN>()
            .map(|seq| u64::from_le_bytes(*seq))
    };
    let mut events = Vec::new();
    let mut expected = header
        .floor
        .checked_add(1)
        .ok_or(corrupt("the event counter overflows"))?;
    let mut offset = next;
    while offset < bytes.len() {
        match read_frame(bytes, offset) {
            FrameRead::Ok { payload, next } => {
                let (seq, text) = payload
                    .split_first_chunk::<SEQ_LEN>()
                    .ok_or(corrupt("an event has no number"))?;
                if u64::from_le_bytes(*seq) != expected {
                    return Err(corrupt("event numbers are not contiguous"));
                }
                let text = String::from_utf8(text.to_vec())
                    .map_err(|_| corrupt("an event text is not UTF-8"))?;
                events.push(Event {
                    seq: expected,
                    text,
                });
                expected = expected
                    .checked_add(1)
                    .ok_or(corrupt("the event counter overflows"))?;
                offset = next;
            }
            // One unfinished append leaves at most one frame and no event after it; a damaged
            // length field or a zeroed block followed by whole events is damage.
            FrameRead::Torn
                if bytes.len() - offset <= MAX_TORN
                    && !record_follows(bytes, offset, expected, seq_of) =>
            {
                break;
            }
            FrameRead::Torn => return Err(corrupt("an event does not read back")),
            FrameRead::Bad => return Err(corrupt("an event fails its checksum")),
        }
    }
    Ok((header.floor, events, offset))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    const ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
    const OTHER: &str = "0194f0a2-7c3e-7d11-8a52-000000000001";

    fn text(seq: u64) -> String {
        format!(r#"{{"type":"agentEvent","seq":{seq}}}"#)
    }

    fn big(seq: u64, len: usize) -> String {
        format!("{seq}:{}", "x".repeat(len))
    }

    fn file_of(dir: &Path) -> PathBuf {
        dir.join(format!("{ID}{EXTENSION}"))
    }

    fn pending(outbox: &Outbox) -> Vec<(u64, String)> {
        outbox.pending().map(|(s, t)| (s, t.to_owned())).collect()
    }

    fn put(outbox: &mut Outbox, count: u64) {
        for _ in 0..count {
            outbox.append(Kind::Other, text).unwrap();
        }
    }

    /// A probe that fails at `step` once `armed` is set.
    fn failing_at(step: Step, armed: &Arc<AtomicBool>) -> Probe {
        let armed = armed.clone();
        Box::new(move |at| {
            if at == step && armed.load(Ordering::SeqCst) {
                Err(io::Error::other("injected"))
            } else {
                Ok(())
            }
        })
    }

    // S1
    #[test]
    fn a_new_outbox_starts_at_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        let bytes = fs::read(file_of(dir.path())).unwrap();
        assert_eq!(parse(&bytes, ID).unwrap().0, 0);
        assert_eq!(outbox.highest(), 0);
        assert_eq!(outbox.append(Kind::Other, text).unwrap(), 1);
        assert_eq!(outbox.append(Kind::Other, text).unwrap(), 2);
    }

    // S2
    #[test]
    fn an_event_is_pending_only_after_its_frame_is_synced_and_holds_its_number() {
        let dir = tempfile::tempdir().unwrap();
        let steps = Arc::new(Mutex::new(Vec::new()));
        let seen = steps.clone();
        let probe: Probe = Box::new(move |step| {
            seen.lock().unwrap().push(step);
            Ok(())
        });
        let mut outbox = Outbox::open_with(dir.path(), ID, probe).unwrap();
        steps.lock().unwrap().clear();
        let mut given = 0;
        let seq = outbox
            .append(Kind::Other, |seq| {
                given = seq;
                text(seq)
            })
            .unwrap();
        assert_eq!((seq, given), (1, 1));
        assert_eq!(
            *steps.lock().unwrap(),
            [Step::BeforeWrite, Step::AfterWrite, Step::AfterSync]
        );
        assert_eq!(pending(&outbox), [(1, text(1))]);
        // The number is in the stored text and in the frame.
        let bytes = fs::read(file_of(dir.path())).unwrap();
        let (_, events, _) = parse(&bytes, ID).unwrap();
        assert_eq!(
            (events[0].seq, events[0].text.as_str()),
            (1, text(1).as_str())
        );
    }

    // S3
    #[test]
    fn a_reopened_outbox_has_the_same_events_and_continues_the_counter() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        put(&mut outbox, 3);
        let before = pending(&outbox);
        drop(outbox);
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        assert_eq!(pending(&outbox), before);
        assert_eq!(outbox.append(Kind::Other, text).unwrap(), 4);
    }

    // S4 (the crash cases are in tests/outbox_crash.rs)
    #[test]
    fn a_torn_last_frame_is_cut_off_and_its_number_used_again() {
        for cut in [1, 5, 12] {
            let dir = tempfile::tempdir().unwrap();
            let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
            put(&mut outbox, 3);
            drop(outbox);
            let path = file_of(dir.path());
            let len = fs::metadata(&path).unwrap().len();
            OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(len - cut)
                .unwrap();
            let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
            let cut_len = fs::metadata(&path).unwrap().len();
            assert_eq!(
                cut_len,
                len - Event {
                    seq: 3,
                    text: text(3)
                }
                .frame_len(),
                "cut {cut}"
            );
            assert_eq!(outbox.pending().map(|(s, _)| s).collect::<Vec<_>>(), [1, 2]);
            assert_eq!(outbox.append(Kind::Other, text).unwrap(), 3);
            drop(outbox);
            let outbox = Outbox::open_in(dir.path(), ID).unwrap();
            assert_eq!(pending(&outbox)[2], (3, text(3)), "cut {cut}");
        }
    }

    // S5
    #[test]
    fn a_failed_write_poisons_the_outbox_until_it_is_opened_again() {
        for (step, lands) in [
            (Step::BeforeWrite, false),
            (Step::AfterWrite, true),
            (Step::AfterSync, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let armed = Arc::new(AtomicBool::new(false));
            let mut outbox = Outbox::open_with(dir.path(), ID, failing_at(step, &armed)).unwrap();
            put(&mut outbox, 1);
            armed.store(true, Ordering::SeqCst);
            assert!(matches!(
                outbox.append(Kind::Other, text),
                Err(OutboxError::Io(_))
            ));
            assert_eq!(pending(&outbox), [(1, text(1))], "{step:?}");
            armed.store(false, Ordering::SeqCst);
            assert!(matches!(
                outbox.append(Kind::Other, text),
                Err(OutboxError::Poisoned)
            ));
            drop(outbox);
            // A frame that landed anyway is pending after the reopen.
            let outbox = Outbox::open_in(dir.path(), ID).unwrap();
            assert_eq!(outbox.highest(), if lands { 2 } else { 1 }, "{step:?}");
        }
    }

    // S6, S7, S8
    #[test]
    fn an_acknowledgement_is_cumulative_idempotent_and_bounded_by_the_highest() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        put(&mut outbox, 4);
        let file = fs::read(file_of(dir.path())).unwrap();
        outbox.ack(2).unwrap();
        assert_eq!(outbox.pending().map(|(s, _)| s).collect::<Vec<_>>(), [3, 4]);
        assert_eq!(fs::read(file_of(dir.path())).unwrap(), file, "no rewrite");
        // An old or repeated number changes nothing.
        outbox.ack(2).unwrap();
        outbox.ack(1).unwrap();
        outbox.ack(0).unwrap();
        assert_eq!(outbox.pending().count(), 2);
        // A number above the highest is refused and drops nothing.
        assert!(matches!(
            outbox.ack(5),
            Err(OutboxError::OutOfRange {
                acked: 5,
                highest: 4
            })
        ));
        assert_eq!(outbox.pending().count(), 2);
        assert_eq!(outbox.append(Kind::Other, text).unwrap(), 5);
    }

    // S9
    #[test]
    fn the_counter_survives_an_empty_outbox() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        put(&mut outbox, 3);
        outbox.ack(3).unwrap();
        assert_eq!(outbox.pending().count(), 0);
        let bytes = fs::read(file_of(dir.path())).unwrap();
        let (floor, events, len) = parse(&bytes, ID).unwrap();
        assert_eq!((floor, events.len(), len), (3, 0, bytes.len()));
        drop(outbox);
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        assert_eq!(outbox.append(Kind::Other, text).unwrap(), 4);
    }

    // S9
    #[test]
    fn sixty_four_kib_acknowledged_rewrite_the_file_without_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        for _ in 0..3 {
            outbox
                .append(Kind::Other, |seq| big(seq, 40 << 10))
                .unwrap();
        }
        let full = fs::metadata(file_of(dir.path())).unwrap().len();
        outbox.ack(1).unwrap();
        assert_eq!(fs::metadata(file_of(dir.path())).unwrap().len(), full);
        outbox.ack(2).unwrap();
        let rewritten = fs::metadata(file_of(dir.path())).unwrap().len();
        assert!(rewritten < full / 2 + 64, "{rewritten} of {full}");
        assert_eq!(outbox.append(Kind::Other, text).unwrap(), 4);
        drop(outbox);
        let outbox = Outbox::open_in(dir.path(), ID).unwrap();
        assert_eq!(outbox.pending().map(|(s, _)| s).collect::<Vec<_>>(), [3, 4]);
    }

    // S10
    #[test]
    fn a_rewrite_that_fails_before_the_rename_keeps_the_old_file_and_every_event() {
        let dir = tempfile::tempdir().unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let mut outbox =
            Outbox::open_with(dir.path(), ID, failing_at(Step::BeforeRename, &armed)).unwrap();
        put(&mut outbox, 3);
        let file = fs::read(file_of(dir.path())).unwrap();
        armed.store(true, Ordering::SeqCst);
        outbox.ack(3).unwrap();
        assert_eq!(fs::read(file_of(dir.path())).unwrap(), file);
        assert!(!outbox.tmp_path().exists(), "the temporary file is removed");
        // The outbox still works, and a later acknowledgement rewrites again.
        armed.store(false, Ordering::SeqCst);
        assert_eq!(outbox.append(Kind::Other, text).unwrap(), 4);
        outbox.ack(4).unwrap();
        drop(outbox);
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        assert_eq!(outbox.pending().count(), 0);
        assert_eq!(outbox.append(Kind::Other, text).unwrap(), 5);
    }

    // S10
    #[test]
    fn a_stale_temporary_file_is_removed_at_open() {
        let dir = tempfile::tempdir().unwrap();
        drop(Outbox::open_in(dir.path(), ID).unwrap());
        let tmp = dir.path().join(format!("{ID}{EXTENSION}.tmp"));
        fs::write(&tmp, b"half a rewrite").unwrap();
        drop(Outbox::open_in(dir.path(), ID).unwrap());
        assert!(!tmp.exists());
    }

    // S11
    #[test]
    fn acknowledgements_not_yet_rewritten_away_are_pending_again_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        put(&mut outbox, 3);
        outbox.ack(2).unwrap();
        drop(outbox);
        let outbox = Outbox::open_in(dir.path(), ID).unwrap();
        assert_eq!(
            outbox.pending().map(|(s, _)| s).collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    // S12
    #[test]
    fn appends_racing_acknowledgements_stay_contiguous_and_match_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = Arc::new(Mutex::new(Outbox::open_in(dir.path(), ID).unwrap()));
        let writer = {
            let outbox = outbox.clone();
            std::thread::spawn(move || {
                for _ in 0..150 {
                    outbox
                        .lock()
                        .unwrap()
                        .append(Kind::Other, |seq| big(seq, 2000))
                        .unwrap();
                }
            })
        };
        let acker = {
            let outbox = outbox.clone();
            std::thread::spawn(move || {
                for _ in 0..150 {
                    let mut outbox = outbox.lock().unwrap();
                    let highest = outbox.highest();
                    outbox.ack(highest).unwrap();
                }
            })
        };
        writer.join().unwrap();
        acker.join().unwrap();
        let live = Arc::try_unwrap(outbox).ok().unwrap().into_inner().unwrap();
        assert_eq!(live.highest(), 150);
        let live_pending = pending(&live);
        drop(live);
        let reopened = Outbox::open_in(dir.path(), ID).unwrap();
        assert_eq!(reopened.highest(), 150);
        let after = pending(&reopened);
        assert!(after.ends_with(&live_pending), "memory is the file's tail");
        let numbers: Vec<u64> = after.iter().map(|(s, _)| *s).collect();
        assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1));
    }

    // S13
    #[test]
    fn a_full_outbox_refuses_and_a_received_event_is_refused_earlier() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        let chunk = MAX_EVENT_TEXT;
        let mut count = 0;
        while outbox
            .append(Kind::Received, |seq| big(seq, chunk - 20))
            .is_ok()
        {
            count += 1;
        }
        assert!(outbox.file_len <= ADMISSION_BYTES);
        let len = fs::metadata(file_of(dir.path())).unwrap().len();
        let highest = outbox.highest();
        assert!(matches!(
            outbox.append(Kind::Received, |seq| big(seq, chunk - 20)),
            Err(OutboxError::Full)
        ));
        assert_eq!(fs::metadata(file_of(dir.path())).unwrap().len(), len);
        assert_eq!(outbox.highest(), highest, "no number was used");
        // Another kind still has room, up to the cap.
        while outbox
            .append(Kind::Other, |seq| big(seq, chunk - 20))
            .is_ok()
        {
            count += 1;
        }
        assert!(outbox.file_len <= MAX_OUTBOX_BYTES);
        assert!(count > 100);
        // Acknowledgements and the rewrite make room again.
        let highest = outbox.highest();
        outbox.ack(highest).unwrap();
        assert!(outbox.append(Kind::Received, text).is_ok());
    }

    // S14
    #[test]
    fn an_event_over_a_control_message_is_too_large_and_uses_no_number() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        assert!(matches!(
            outbox.append(Kind::Other, |_| "x".repeat(MAX_EVENT_TEXT + 1)),
            Err(OutboxError::TooLarge)
        ));
        assert_eq!(outbox.highest(), 0);
        let seq = outbox
            .append(Kind::Other, |_| "x".repeat(MAX_EVENT_TEXT))
            .unwrap();
        assert_eq!(seq, 1);
    }

    // S15
    #[test]
    fn the_files_of_other_agent_ids_are_left_alone_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let mut other = Outbox::open_in(dir.path(), OTHER).unwrap();
        put(&mut other, 2);
        drop(other);
        let theirs = dir.path().join(format!("{OTHER}{EXTENSION}"));
        let junk = dir.path().join(format!("garbage{EXTENSION}"));
        fs::write(&junk, b"not an outbox").unwrap();
        let before = (fs::read(&theirs).unwrap(), fs::read(&junk).unwrap());
        assert_eq!(other_outboxes(dir.path(), &file_of(dir.path())), 2);
        let mut outbox = Outbox::open_in(dir.path(), ID).unwrap();
        assert_eq!(outbox.pending().count(), 0);
        assert_eq!(outbox.append(Kind::Other, text).unwrap(), 1);
        assert_eq!(fs::read(&theirs).unwrap(), before.0);
        assert_eq!(fs::read(&junk).unwrap(), before.1);
    }

    // S16
    #[test]
    fn a_file_that_does_not_read_back_is_corrupt_and_nothing_is_touched() {
        let event = |seq: u64| Event {
            seq,
            text: text(seq),
        };
        let header = |id: &str, protection: u32| Header {
            agent_id: id.to_owned(),
            protection,
            floor: 0,
        };
        let build = |version: u32, header: &Header, events: &[Event]| {
            let mut out = Vec::new();
            out.extend_from_slice(&MAGIC);
            out.extend_from_slice(&version.to_le_bytes());
            push_frame(&mut out, &postcard::to_allocvec(header).unwrap()).unwrap();
            for e in events {
                let mut payload = e.seq.to_le_bytes().to_vec();
                payload.extend_from_slice(e.text.as_bytes());
                push_frame(&mut out, &payload).unwrap();
            }
            out
        };
        let good = build(1, &header(ID, 0), &[event(1), event(2), event(3)]);
        let mut flipped = good.clone();
        let at = PREAMBLE
            + FRAME_HEADER
            + postcard::to_allocvec(&header(ID, 0)).unwrap().len()
            + FRAME_HEADER
            + 9;
        flipped[at] ^= 0xFF;
        // A damaged length field in the middle, with whole events after it, is not a torn tail.
        let mut damaged = good.clone();
        let second = PREAMBLE
            + FRAME_HEADER
            + postcard::to_allocvec(&header(ID, 0)).unwrap().len()
            + FRAME_HEADER
            + SEQ_LEN
            + text(1).len();
        damaged[second..second + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let cases = [
            ("damaged length", damaged),
            ("another ID", build(1, &header(OTHER, 0), &[event(1)])),
            ("version", build(2, &header(ID, 0), &[event(1)])),
            ("protection", build(1, &header(ID, 1), &[event(1)])),
            ("checksum", flipped),
            ("gap", build(1, &header(ID, 0), &[event(1), event(3)])),
            ("start", build(1, &header(ID, 0), &[event(2)])),
            ("magic", b"NGRJRNL\0....".to_vec()),
        ];
        for (name, bytes) in cases {
            let dir = tempfile::tempdir().unwrap();
            fs::write(file_of(dir.path()), &bytes).unwrap();
            let result = Outbox::open_in(dir.path(), ID);
            assert!(matches!(result, Err(OutboxError::Corrupt(_))), "{name}");
            assert_eq!(fs::read(file_of(dir.path())).unwrap(), bytes, "{name}");
            let names: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert!(names.len() <= 2, "{name}: {names:?}");
        }
        // The good file reads back.
        let dir = tempfile::tempdir().unwrap();
        fs::write(file_of(dir.path()), &good).unwrap();
        assert_eq!(Outbox::open_in(dir.path(), ID).unwrap().highest(), 3);
    }

    // S17
    #[test]
    fn a_second_opener_gets_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let first = Outbox::open_in(dir.path(), ID).unwrap();
        assert!(matches!(
            Outbox::open_in(dir.path(), ID),
            Err(OutboxError::InUse)
        ));
        drop(first);
        assert!(Outbox::open_in(dir.path(), ID).is_ok());
    }

    #[test]
    fn an_agent_id_that_cannot_name_a_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["", "../x", "a/b", "a.b"] {
            assert!(matches!(
                Outbox::open_in(dir.path(), id),
                Err(OutboxError::BadAgentId)
            ));
        }
    }
}
