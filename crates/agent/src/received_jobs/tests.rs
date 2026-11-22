//! The received-jobs ledger's cases (ADR-489; the row's letter and number of the case table begin
//! each test's name where there is one).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

use super::*;

const ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const OTHER: &str = "0194f0a2-7c3e-7d11-8a52-000000000001";

/// A job ID, one per `n`.
fn job(n: u32) -> String {
    format!("0194f0a2-7c3e-7d11-8a52-{n:012x}")
}

/// An envelope-like byte string of `len` bytes, different for each `n`.
fn envelope(n: u32, len: usize) -> Vec<u8> {
    (0..len).map(|k| (k as u32 ^ n) as u8 | 1).collect()
}

fn file_of(dir: &Path) -> PathBuf {
    dir.join(format!("{ID}{FILE_SUFFIX}"))
}

fn tmp_of(dir: &Path) -> PathBuf {
    dir.join(format!("{ID}{FILE_SUFFIX}.tmp"))
}

fn open(dir: &Path) -> Result<ReceivedJobs, LedgerError> {
    ReceivedJobs::open_in(dir, ID)
}

fn record(ledger: &ReceivedJobs, n: u32, generation: u64) {
    ledger
        .record(&job(n), generation, &envelope(n, 200))
        .unwrap();
}

/// A probe that fails at `step` while `armed` is set.
fn failing_at(step: Step, armed: &Arc<AtomicBool>) -> Probe {
    let armed = armed.clone();
    Box::new(move |at| {
        if at == step && armed.load(Ordering::SeqCst) {
            return Err(io::Error::other("a scripted failure"));
        }
        Ok(())
    })
}

/// A file as the writer makes it, built from `entries` for the agent `agent_id`.
fn crafted(agent_id: &str, entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    let header = Header {
        agent_id: agent_id.to_owned(),
        protection: 0,
    };
    push_frame(&mut out, &postcard::to_allocvec(&header).unwrap()).unwrap();
    for entry in entries {
        push_frame(&mut out, &postcard::to_allocvec(entry).unwrap()).unwrap();
    }
    out
}

fn entry(seq: u64, job_id: &str, generation: u64, envelope: Vec<u8>) -> Entry {
    Entry {
        seq,
        job_id: job_id.to_owned(),
        generation,
        envelope,
    }
}

/// The length of the file after each of `count` records, found by writing them one by one.
fn lengths_after(dir: &Path, count: u32) -> Vec<u64> {
    let ledger = open(dir).unwrap();
    (1..=count)
        .map(|n| {
            record(&ledger, n, 1);
            fs::metadata(file_of(dir)).unwrap().len()
        })
        .collect()
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

// D1
#[test]
fn d1_an_absent_file_is_an_empty_ledger_and_the_open_creates_none_the_first_record_does() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = open(dir.path()).unwrap();
    assert_eq!(ledger.highest(&job(1)), None);
    assert_eq!(ledger.jobs(), 0);
    assert!(!file_of(dir.path()).exists(), "the open creates nothing");
    record(&ledger, 1, 7);
    assert!(file_of(dir.path()).exists());
    assert!(!tmp_of(dir.path()).exists(), "no temporary file is left");
    assert_eq!(ledger.highest(&job(1)), Some(7));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(file_of(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "owner-only");
    }
}

// D2, G2
#[test]
fn d2_a_valid_file_gives_the_highest_generation_per_job_and_the_numbers_continue() {
    let dir = tempfile::tempdir().unwrap();
    {
        let ledger = open(dir.path()).unwrap();
        record(&ledger, 1, 7);
        record(&ledger, 2, 1);
        record(&ledger, 1, 9);
        record(&ledger, 1, 8);
        assert_eq!(ledger.highest(&job(1)), Some(9), "a maximum, not the last");
    }
    let ledger = open(dir.path()).unwrap();
    assert_eq!(ledger.highest(&job(1)), Some(9));
    assert_eq!(ledger.highest(&job(2)), Some(1));
    assert_eq!(ledger.highest(&job(3)), None);
    assert_eq!(ledger.jobs(), 2);
    record(&ledger, 3, 1);
    drop(ledger);
    let ledger = open(dir.path()).unwrap();
    assert_eq!(
        ledger.jobs(),
        3,
        "the fifth record was numbered 4 and reads back"
    );
}

// D2: a record is counted only once it is durable (E2)
#[test]
fn e2_the_index_changes_only_after_the_sync_and_a_failed_write_poisons_the_ledger() {
    for step in [
        Step::BeforeWrite,
        Step::AfterWrite,
        Step::AfterSync,
        Step::BeforeRename,
        Step::AfterRename,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let ledger = ReceivedJobs::open_with(dir.path(), ID, failing_at(step, &armed)).unwrap();
        // The first record creates the file, the second appends: both paths are tried.
        for n in 1..=2 {
            // An append has no rename: the two steps around it belong to the creation only.
            if n == 2 && matches!(step, Step::BeforeRename | Step::AfterRename) {
                continue;
            }
            armed.store(true, Ordering::SeqCst);
            let error = ledger.record(&job(n), 1, &envelope(n, 50)).unwrap_err();
            assert!(matches!(error, LedgerError::Io(_)), "{step:?} {error}");
            assert_eq!(ledger.highest(&job(n)), None, "{step:?}: index unchanged");
            assert!(ledger.is_poisoned());
            let error = ledger.record(&job(n), 1, &envelope(n, 50)).unwrap_err();
            assert!(matches!(error, LedgerError::Poisoned), "{step:?}");
            armed.store(false, Ordering::SeqCst);
            assert!(ledger.reopen_if_poisoned().unwrap());
            assert!(!ledger.is_poisoned());
            // A file that was put in place, or a frame that was written, counts at the reopen;
            // otherwise the job is new and its record is made now.
            let counted = (n == 1 && step == Step::AfterRename)
                || (n == 2 && matches!(step, Step::AfterWrite | Step::AfterSync));
            assert_eq!(
                ledger.highest(&job(n)),
                counted.then_some(1),
                "{step:?} {n}: the reopen rebuilt the index from the file"
            );
            if ledger.highest(&job(n)).is_none() {
                ledger.record(&job(n), 1, &envelope(n, 50)).unwrap();
            }
            assert_eq!(ledger.highest(&job(n)), Some(1), "{step:?}");
        }
        let jobs = ledger.jobs();
        drop(ledger);
        assert_eq!(open(dir.path()).unwrap().jobs(), jobs, "{step:?}");
    }
}

// E5
#[test]
fn e5_a_reopen_that_fails_keeps_the_ledger_poisoned_and_its_index_and_a_later_one_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let write_fails = Arc::new(AtomicBool::new(false));
    let open_fails = Arc::new(AtomicBool::new(false));
    let (write_flag, open_flag) = (write_fails.clone(), open_fails.clone());
    let ledger = ReceivedJobs::open_with(
        dir.path(),
        ID,
        Box::new(move |step| {
            let failing = match step {
                Step::BeforeWrite => write_flag.load(Ordering::SeqCst),
                Step::AfterRename => open_flag.load(Ordering::SeqCst),
                _ => false,
            };
            if failing {
                return Err(io::Error::other("a scripted failure"));
            }
            Ok(())
        }),
    )
    .unwrap();
    record(&ledger, 1, 4);
    write_fails.store(true, Ordering::SeqCst);
    open_fails.store(true, Ordering::SeqCst);
    assert!(ledger.record(&job(2), 1, &envelope(2, 50)).is_err());
    let error = ledger.reopen_if_poisoned().unwrap_err();
    assert!(matches!(error, LedgerError::Io(_)), "{error}");
    assert!(ledger.is_poisoned());
    assert_eq!(ledger.highest(&job(1)), Some(4), "the index is kept");
    // Neither the lock nor the file is held: another store can open the directory.
    drop(open(dir.path()).unwrap());
    write_fails.store(false, Ordering::SeqCst);
    open_fails.store(false, Ordering::SeqCst);
    assert!(ledger.reopen_if_poisoned().unwrap());
    record(&ledger, 2, 1);
    assert!(
        !ledger.reopen_if_poisoned().unwrap(),
        "healthy: nothing to do"
    );
}

// B10
#[test]
fn b10_another_agent_id_has_an_empty_ledger_and_the_old_file_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    {
        let ledger = open(dir.path()).unwrap();
        record(&ledger, 1, 7);
    }
    let before = fs::read(file_of(dir.path())).unwrap();
    let other = ReceivedJobs::open_in(dir.path(), OTHER).unwrap();
    assert_eq!(other.jobs(), 0);
    assert_eq!(other.highest(&job(1)), None);
    drop(other);
    assert_eq!(fs::read(file_of(dir.path())).unwrap(), before);
    assert_eq!(open(dir.path()).unwrap().highest(&job(1)), Some(7));
}

// D3
#[test]
fn d3_a_torn_last_frame_is_cut_off_and_its_job_is_new_again() {
    let dir = tempfile::tempdir().unwrap();
    let lengths = lengths_after(dir.path(), 3);
    let whole = fs::read(file_of(dir.path())).unwrap();
    let (start, end) = (lengths[1] as usize, lengths[2] as usize);
    // The last frame cut short at several places, and damaged in its checksum and its body.
    let mut tails: Vec<Vec<u8>> = [1, 4, 7, 8, 9, 40, end - start - 1]
        .iter()
        .map(|keep| whole[..start + keep].to_vec())
        .collect();
    for at in [start + 5, end - 1] {
        let mut damaged = whole.clone();
        damaged[at] ^= 0xFF;
        tails.push(damaged);
    }
    for (k, bytes) in tails.into_iter().enumerate() {
        fs::write(file_of(dir.path()), &bytes).unwrap();
        let log = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        let ledger = tracing::subscriber::with_default(subscriber, || open(dir.path()).unwrap());
        assert_eq!(ledger.highest(&job(2)), Some(1), "tail {k}");
        assert_eq!(
            ledger.highest(&job(3)),
            None,
            "tail {k}: the job is new again"
        );
        assert_eq!(
            fs::metadata(file_of(dir.path())).unwrap().len(),
            lengths[1],
            "tail {k}: cut"
        );
        let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.matches("WARN").count(), 1, "tail {k}: {text}");
        let cut = bytes.len() - lengths[1] as usize;
        assert!(text.contains(&format!("cut={cut}")), "{text}");
        assert!(!text.contains(&job(3)), "{text}");
        // The next record continues the numbers.
        record(&ledger, 3, 1);
        drop(ledger);
        assert_eq!(open(dir.path()).unwrap().jobs(), 3, "tail {k}");
        // Back to the whole three for the next tail.
        fs::write(file_of(dir.path()), &whole).unwrap();
    }
}

// D4
#[test]
fn d4_a_file_that_does_not_read_back_is_corrupt_and_is_never_touched() {
    let good = |n: u64| entry(n, &job(n as u32 + 1), 1, envelope(n as u32, 80));
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    let valid = crafted(ID, &[good(0), good(1), good(2)]);
    cases.push(("empty file", Vec::new()));
    cases.push(("short file", valid[..6].to_vec()));
    let mut bytes = valid.clone();
    bytes[0] ^= 1;
    cases.push(("magic", bytes));
    let mut bytes = valid.clone();
    bytes[MAGIC.len()] = 2;
    cases.push(("version", bytes));
    let mut bytes = valid.clone();
    bytes[PREAMBLE + FRAME_HEADER] ^= 1;
    cases.push(("header checksum", bytes));
    cases.push(("another agent ID", crafted(OTHER, &[good(0)])));
    // The second of three records damaged in its body: a bad checksum before the last frame.
    let first_end = crafted(ID, &[good(0)]).len();
    let mut bytes = valid.clone();
    bytes[first_end + FRAME_HEADER + 3] ^= 1;
    cases.push(("checksum before the last frame", bytes));
    // The second record's length field destroyed with whole records after it.
    let mut bytes = valid;
    bytes[first_end..first_end + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    cases.push(("damage followed by a whole record", bytes));
    cases.push((
        "numbers not contiguous",
        crafted(ID, &[good(0), entry(2, &job(9), 1, envelope(9, 50))]),
    ));
    let mut junk = crafted(ID, &[]);
    push_frame(&mut junk, &[0xFF; 5]).unwrap();
    cases.push(("a record that does not decode", junk));
    for bad_job in ["not-a-uuid", "0194F0A2-7C3E-7D11-8A52-3F6F9B1C2D4F", ""] {
        cases.push((
            "a job ID that is not a canonical UUID",
            crafted(ID, &[entry(0, bad_job, 1, envelope(1, 50))]),
        ));
    }
    cases.push((
        "an empty envelope",
        crafted(ID, &[entry(0, &job(1), 1, Vec::new())]),
    ));
    cases.push((
        "an envelope over the limit",
        crafted(ID, &[entry(0, &job(1), 1, vec![1; MAX_ENVELOPE + 1])]),
    ));
    for (what, bytes) in cases {
        let dir = tempfile::tempdir().unwrap();
        fs::write(file_of(dir.path()), &bytes).unwrap();
        let error = open(dir.path())
            .err()
            .unwrap_or_else(|| panic!("{what} opened"));
        assert!(matches!(error, LedgerError::Corrupt(_)), "{what}: {error}");
        assert_eq!(
            fs::read(file_of(dir.path())).unwrap(),
            bytes,
            "{what}: untouched"
        );
    }
}

// D5
#[test]
fn d5_a_path_that_is_not_a_regular_file_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(file_of(dir.path())).unwrap();
    let error = open(dir.path()).expect_err("a directory does not open");
    assert!(matches!(error, LedgerError::Corrupt(_)), "{error}");
}

// D7
#[test]
fn d7_a_second_store_of_the_directory_is_in_use_until_the_first_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let first = open(dir.path()).unwrap();
    let error = open(dir.path()).expect_err("in use");
    assert!(matches!(error, LedgerError::InUse), "{error}");
    drop(first);
    open(dir.path()).unwrap();
}

// D6
#[test]
fn d6_a_directory_that_cannot_be_used_is_an_io_error_not_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let error = open(&dir.path().join("missing")).expect_err("no directory");
    assert!(matches!(error, LedgerError::Io(_)), "{error}");
}

// D8
#[test]
fn d8_an_agent_id_that_is_not_a_canonical_uuid_cannot_name_the_file() {
    let dir = tempfile::tempdir().unwrap();
    for id in ["", "agent", "../x", &ID.to_uppercase(), &format!("{ID}0")] {
        let error = ReceivedJobs::open_in(dir.path(), id).expect_err(id);
        assert!(matches!(error, LedgerError::BadAgentId), "{id}: {error}");
    }
}

// D10
#[test]
fn d10_a_stale_temporary_file_is_ignored_by_the_open_and_removed_by_the_creation() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(tmp_of(dir.path()), b"left by a crash").unwrap();
    let ledger = open(dir.path()).unwrap();
    assert!(tmp_of(dir.path()).exists(), "the open leaves it");
    record(&ledger, 1, 1);
    assert!(!tmp_of(dir.path()).exists(), "the creation removed it");
    drop(ledger);
    assert_eq!(open(dir.path()).unwrap().jobs(), 1);
}

#[cfg(unix)]
#[test]
fn d10_the_mode_of_an_existing_file_is_not_checked() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    drop({
        let ledger = open(dir.path()).unwrap();
        record(&ledger, 1, 1);
        ledger
    });
    fs::set_permissions(file_of(dir.path()), fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(open(dir.path()).unwrap().jobs(), 1);
}

// E1
#[test]
fn e1_an_envelope_at_the_limit_is_recorded_and_one_over_is_refused_without_poisoning() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = open(dir.path()).unwrap();
    ledger.record(&job(1), 1, &vec![7; MAX_ENVELOPE]).unwrap();
    let over = ledger.record(&job(2), 1, &vec![7; MAX_ENVELOPE + 1]);
    assert!(matches!(over, Err(LedgerError::Refused)));
    assert!(matches!(
        ledger.record(&job(2), 1, &[]),
        Err(LedgerError::Refused)
    ));
    assert!(matches!(
        ledger.record("not-a-uuid", 1, &[1]),
        Err(LedgerError::Refused)
    ));
    assert!(!ledger.is_poisoned());
    assert_eq!(ledger.highest(&job(2)), None);
    drop(ledger);
    assert_eq!(open(dir.path()).unwrap().highest(&job(1)), Some(1));
}

// D9
#[test]
fn d9_many_records_read_back_in_one_pass_and_memory_keeps_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let entries: Vec<Entry> = (0..20_000u64)
        .map(|n| entry(n, &job(n as u32 % 5_000), n, vec![3; 64]))
        .collect();
    fs::write(file_of(dir.path()), crafted(ID, &entries)).unwrap();
    let ledger = open(dir.path()).unwrap();
    assert_eq!(ledger.jobs(), 5_000);
    assert_eq!(ledger.highest(&job(0)), Some(15_000));
    assert_eq!(ledger.highest(&job(4_999)), Some(19_999));
    record(&ledger, 7, 99_999);
    assert_eq!(ledger.highest(&job(7)), Some(99_999));
}
