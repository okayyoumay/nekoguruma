use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use shared_proto::JobId;

use std::fs::OpenOptions;

use super::*;

const ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const OTHER: &str = "0194f0a2-7c3e-7d11-8a52-000000000001";

fn jk(n: u8) -> JobKey {
    JobKey {
        job_id: JobId(format!("job-{n}")),
        generation: 1,
    }
}

fn file_of(dir: &Path) -> PathBuf {
    dir.join(ID)
}

fn objects_of(dir: &Path) -> PathBuf {
    dir.join(format!("{ID}{OBJECTS_SUFFIX}"))
}

fn tmp_of(dir: &Path) -> PathBuf {
    dir.join(format!("{ID}{TMP_SUFFIX}"))
}

fn seqs(ledger: &UploadLedger) -> Vec<u64> {
    ledger.pending().map(|entry| entry.seq).collect()
}

fn put_result(ledger: &mut UploadLedger, n: u8) -> u64 {
    ledger
        .enqueue_result(&jk(n), format!("result {n}").as_bytes())
        .unwrap()
}

/// A root that makes a path absolute on this platform (on Windows a path needs a drive).
const ROOT: &str = if cfg!(windows) { r"C:\" } else { "/" };

/// `rest` under [`ROOT`], as the text an entry holds.
fn rooted(rest: &str) -> String {
    format!("{ROOT}{rest}")
}

fn comm_path(dir: &Path, n: u8) -> PathBuf {
    dir.join(format!("comm-{n}.log"))
}

fn put_comm(ledger: &mut UploadLedger, dir: &Path, n: u8) -> u64 {
    let path = comm_path(dir, n);
    fs::write(&path, format!("log {n}")).unwrap();
    let digest = digest_file(&path).unwrap();
    ledger.enqueue_comm_log(&jk(n), &path, digest).unwrap()
}

fn fake_digest(length: u64, tag: u8) -> Digest {
    Digest {
        length,
        sha256: [tag; 32],
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

/// A probe that records every step.
fn recording() -> (Probe, Arc<Mutex<Vec<Step>>>) {
    let steps = Arc::new(Mutex::new(Vec::new()));
    let seen = steps.clone();
    let probe: Probe = Box::new(move |step| {
        seen.lock().unwrap().push(step);
        Ok(())
    });
    (probe, steps)
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn result_entry(seq: u64, n: u8) -> Entry {
    Entry {
        seq,
        key: jk(n),
        kind: Kind::Result,
        source: Source::Stored,
        length: 4,
        sha256: [1; 32],
    }
}

fn comm_entry(seq: u64, n: u8) -> Entry {
    Entry {
        seq,
        key: jk(n),
        kind: Kind::CommLog,
        source: Source::File(rooted("comm.log")),
        length: 9,
        sha256: [2; 32],
    }
}

/// A file with the header given by the arguments, then the entries' frames in order.
fn raw(
    id: &str,
    version: u32,
    protection: u32,
    highest: u64,
    retained: u32,
    entries: &[Entry],
) -> Vec<u8> {
    let header = Header {
        agent_id: id.to_owned(),
        protection,
        highest,
        retained,
    };
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&version.to_le_bytes());
    push_frame(&mut out, &postcard::to_allocvec(&header).unwrap()).unwrap();
    for entry in entries {
        out.extend_from_slice(&encode_frame(entry).unwrap());
    }
    out
}

fn file(highest: u64, retained: u32, entries: &[Entry]) -> Vec<u8> {
    raw(ID, FORMAT_VERSION, 0, highest, retained, entries)
}

fn frame_starts(bytes: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut offset = PREAMBLE;
    while offset < bytes.len() {
        starts.push(offset);
        match read_frame(bytes, offset) {
            FrameRead::Ok { next, .. } => offset = next,
            _ => break,
        }
    }
    starts
}

fn header_of(bytes: &[u8]) -> Header {
    let FrameRead::Ok { payload, .. } = read_frame(bytes, PREAMBLE) else {
        panic!("header");
    };
    postcard::from_bytes(payload).unwrap()
}

/// Every C row: the open says corrupt and changes nothing (O16), whether or not the objects
/// directory exists.
fn assert_corrupt(name: &str, bytes: &[u8]) {
    for with_objects in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(file_of(dir.path()), bytes).unwrap();
        fs::write(tmp_of(dir.path()), b"stale").unwrap();
        let objects = objects_of(dir.path());
        if with_objects {
            fs::create_dir(&objects).unwrap();
            fs::write(objects.join("1"), b"one").unwrap();
            fs::write(objects.join("99"), b"stray").unwrap();
        }
        let result = UploadLedger::open_in(dir.path(), ID);
        assert!(
            matches!(result, Err(LedgerError::Corrupt(_))),
            "{name}: {:?}",
            result.as_ref().err()
        );
        drop(result);
        assert_eq!(fs::read(file_of(dir.path())).unwrap(), bytes, "{name}");
        assert_eq!(fs::read(tmp_of(dir.path())).unwrap(), b"stale", "{name}");
        if with_objects {
            assert_eq!(names(&objects), ["1", "99"], "{name}");
            assert_eq!(fs::read(objects.join("99")).unwrap(), b"stray", "{name}");
        } else {
            assert!(!objects.exists(), "{name}: the objects directory was made");
        }
        assert!(
            WriterLock::at(&dir.path().join(LOCK_FILE)).is_ok(),
            "{name}: the lock is held"
        );
    }
}

// O1
#[test]
fn a_missing_ledger_is_created_empty_with_a_private_objects_directory() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    let bytes = fs::read(file_of(dir.path())).unwrap();
    let header = header_of(&bytes);
    assert_eq!(
        (header.highest, header.retained, header.protection),
        (0, 0, 0)
    );
    assert_eq!(parse(&bytes, ID).unwrap().len, bytes.len());
    assert_eq!(ledger.highest(), 0);
    assert!(ledger.next().is_none());
    assert!(objects_of(dir.path()).is_dir());
    assert!(!tmp_of(dir.path()).exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(objects_of(dir.path())), 0o700);
        assert_eq!(mode(file_of(dir.path())), 0o600);
    }
}

// O1
#[test]
fn a_failure_while_creating_the_ledger_fails_the_open() {
    for step in [Step::BeforeRename, Step::AfterRename] {
        let dir = tempfile::tempdir().unwrap();
        let armed = Arc::new(AtomicBool::new(true));
        let result = UploadLedger::open_with(dir.path(), ID, failing_at(step, &armed));
        assert!(matches!(result, Err(LedgerError::Io(_))), "{step:?}");
        assert!(!tmp_of(dir.path()).exists() || step == Step::AfterRename);
        if step == Step::BeforeRename {
            assert!(!file_of(dir.path()).exists());
        }
    }
}

// O2
#[test]
fn a_stale_temporary_file_is_removed_before_the_ledger_is_created() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(tmp_of(dir.path()), b"half a rewrite").unwrap();
    drop(UploadLedger::open_in(dir.path(), ID).unwrap());
    assert!(!tmp_of(dir.path()).exists());
    assert!(file_of(dir.path()).exists());
    // And one beside an existing file goes at the next open.
    fs::write(tmp_of(dir.path()), b"half a rewrite").unwrap();
    drop(UploadLedger::open_in(dir.path(), ID).unwrap());
    assert!(!tmp_of(dir.path()).exists());
}

// O3
#[test]
fn objects_beside_a_missing_ledger_are_all_swept() {
    let dir = tempfile::tempdir().unwrap();
    let objects = objects_of(dir.path());
    fs::create_dir(&objects).unwrap();
    fs::write(objects.join("1"), b"a").unwrap();
    fs::write(objects.join("junk"), b"b").unwrap();
    let ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert!(names(&objects).is_empty());
    // The sweep reports how many it deleted (the open logs it as one warning).
    fs::write(objects.join("5"), b"a").unwrap();
    fs::write(objects.join("6"), b"b").unwrap();
    assert_eq!(ledger.sweep(), 2);
}

// O4
#[test]
fn a_header_only_ledger_is_empty_and_the_counter_continues_from_its_highest() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(file_of(dir.path()), file(7, 0, &[])).unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert!(ledger.next().is_none());
    assert_eq!(ledger.highest(), 7);
    assert_eq!(put_result(&mut ledger, 1), 8);
}

// O5
#[test]
fn retained_and_appended_entries_are_read_back_in_order_and_made_durable_first() {
    let dir = tempfile::tempdir().unwrap();
    let entries = [
        result_entry(2, 1),
        comm_entry(5, 2),
        result_entry(10, 3),
        comm_entry(11, 4),
    ];
    fs::write(file_of(dir.path()), file(9, 2, &entries)).unwrap();
    let (probe, steps) = recording();
    let mut ledger = UploadLedger::open_with(dir.path(), ID, probe).unwrap();
    assert_eq!(seqs(&ledger), [2, 10, 5, 11]);
    assert_eq!(ledger.highest(), 11);
    assert_eq!(ledger.stored_bytes, 8);
    assert_eq!(*steps.lock().unwrap(), [Step::AfterRename], "the dir sync");
    assert_eq!(put_result(&mut ledger, 5), 12);
}

// O6
#[test]
fn a_torn_last_appended_frame_is_cut_off_synced_and_its_number_and_object_reused() {
    for cut in [1, 5, 12] {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
        for n in 1..=3 {
            put_result(&mut ledger, n);
        }
        drop(ledger);
        let path = file_of(dir.path());
        let len = fs::metadata(&path).unwrap().len();
        let whole = fs::read(&path).unwrap();
        let last = *frame_starts(&whole).last().unwrap() as u64;
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(len - cut)
            .unwrap();
        let (probe, steps) = recording();
        let mut ledger = UploadLedger::open_with(dir.path(), ID, probe).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), last, "cut {cut}");
        assert_eq!(seqs(&ledger), [1, 2], "cut {cut}");
        assert!(!objects_of(dir.path()).join("3").exists(), "cut {cut}");
        let steps = steps.lock().unwrap().clone();
        let at = |step| steps.iter().position(|s| *s == step).unwrap();
        assert!(at(Step::AfterCut) < at(Step::AfterRename), "cut {cut}");
        assert!(
            at(Step::AfterRename) < at(Step::BeforeObjectDelete),
            "cut {cut}"
        );
        assert_eq!(put_result(&mut ledger, 3), 3, "cut {cut}");
        drop(ledger);
        assert_eq!(
            seqs(&UploadLedger::open_in(dir.path(), ID).unwrap()),
            [1, 2, 3]
        );
    }
}

// O7
#[test]
fn a_torn_frame_with_whole_entries_after_it_is_corrupt() {
    let mut bytes = file(
        9,
        0,
        &[
            result_entry(10, 1),
            result_entry(11, 2),
            result_entry(12, 3),
        ],
    );
    let second = frame_starts(&bytes)[2];
    bytes[second..second + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_corrupt("damaged length", &bytes);
}

// O8
#[test]
fn an_unreadable_tail_longer_than_any_entry_is_corrupt() {
    let mut bytes = file(9, 0, &[result_entry(10, 1)]);
    bytes.extend(std::iter::repeat_n(0u8, MAX_TORN + 1));
    assert_corrupt("long tail", &bytes);
}

// O9
#[test]
fn a_bad_checksum_before_the_end_is_corrupt() {
    let mut bytes = file(
        0,
        0,
        &[result_entry(1, 1), result_entry(2, 2), result_entry(3, 3)],
    );
    let second = frame_starts(&bytes)[2];
    bytes[second + FRAME_HEADER + 2] ^= 0xFF;
    assert_corrupt("checksum", &bytes);
}

// O10
#[test]
fn damage_among_the_retained_entries_is_corrupt_the_last_one_too() {
    let entries = [
        result_entry(2, 1),
        result_entry(5, 2),
        result_entry(7, 3),
        result_entry(9, 4),
    ];
    let good = file(10, 4, &entries);
    let fine = tempfile::tempdir().unwrap();
    fs::write(file_of(fine.path()), &good).unwrap();
    assert_eq!(
        seqs(&UploadLedger::open_in(fine.path(), ID).unwrap()),
        [2, 5, 7, 9]
    );
    let starts = frame_starts(&good);
    let mut zeroed = good.clone();
    zeroed[starts[2]..starts[2] + 4].copy_from_slice(&0u32.to_le_bytes());
    assert_corrupt("middle length zeroed", &zeroed);
    let mut flipped = good.clone();
    flipped[starts[2] + FRAME_HEADER + 1] ^= 0xFF;
    assert_corrupt("middle checksum", &flipped);
    // The last retained entry, at the end of the file.
    let mut short = good.clone();
    short.truncate(good.len() - 3);
    assert_corrupt("last torn", &short);
    let mut last_bad = good.clone();
    last_bad[starts[4] + FRAME_HEADER + 1] ^= 0xFF;
    assert_corrupt("last checksum", &last_bad);
    // The last retained entry with an appended one after it.
    let mut appended = file(10, 4, &[&entries[..], &[result_entry(11, 5)]].concat());
    appended[starts[4] + FRAME_HEADER + 1] ^= 0xFF;
    assert_corrupt("last checksum, appended after", &appended);
}

// O11
#[test]
fn fewer_entries_than_retained_is_corrupt() {
    assert_corrupt(
        "fewer",
        &file(5, 3, &[result_entry(1, 1), result_entry(2, 2)]),
    );
    assert_corrupt("none", &file(5, 1, &[]));
}

// O12
#[test]
fn an_empty_short_or_foreign_file_is_corrupt() {
    assert_corrupt("empty", b"");
    assert_corrupt("short", b"NGRUP");
    assert_corrupt("magic", b"NGROUTB\0....");
}

// O13
#[test]
fn an_unknown_version_or_protection_or_a_bad_header_is_corrupt() {
    assert_corrupt("version", &raw(ID, 2, 0, 0, 0, &[]));
    assert_corrupt("protection", &raw(ID, 1, 1, 0, 0, &[]));
    let good = file(0, 0, &[]);
    assert_corrupt("header torn", &good[..PREAMBLE + 3]);
    let mut undecodable = Vec::new();
    undecodable.extend_from_slice(&MAGIC);
    undecodable.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    push_frame(&mut undecodable, &[0xFF; 5]).unwrap();
    assert_corrupt("header undecodable", &undecodable);
}

// O14
#[test]
fn a_file_of_another_agent_id_is_corrupt() {
    assert_corrupt("other ID", &raw(OTHER, 1, 0, 1, 1, &[result_entry(1, 1)]));
}

// O15
#[test]
fn entries_that_break_the_rules_are_corrupt() {
    let mut garbage = file(0, 0, &[result_entry(1, 1)]);
    push_frame(&mut garbage, &[0xFF; 7]).unwrap();
    assert_corrupt("undecodable", &garbage);
    let mut trailing = Vec::new();
    let mut payload = postcard::to_allocvec(&Wire::from(&result_entry(1, 1))).unwrap();
    payload.push(0);
    trailing.extend_from_slice(&file(0, 0, &[]));
    push_frame(&mut trailing, &payload).unwrap();
    assert_corrupt("trailing bytes", &trailing);
    assert_corrupt("number 0", &file(5, 1, &[result_entry(0, 1)]));
    assert_corrupt(
        "retained not increasing",
        &file(5, 2, &[result_entry(3, 1), result_entry(2, 2)]),
    );
    assert_corrupt(
        "retained repeated",
        &file(5, 2, &[result_entry(3, 1), result_entry(3, 2)]),
    );
    assert_corrupt("retained above highest", &file(5, 1, &[result_entry(6, 1)]));
    assert_corrupt("appended gap", &file(5, 0, &[result_entry(7, 1)]));
    assert_corrupt(
        "appended repeats retained",
        &file(5, 1, &[result_entry(5, 1), result_entry(5, 2)]),
    );
    assert_corrupt(
        "duplicate key",
        &file(5, 2, &[result_entry(1, 1), result_entry(2, 1)]),
    );
    assert_corrupt(
        "duplicate key appended",
        &file(0, 0, &[result_entry(1, 1), result_entry(2, 1)]),
    );
    let mut bad_job = result_entry(1, 1);
    bad_job.key.job_id = JobId("../x".to_owned());
    assert_corrupt("job ID", &file(1, 1, &[bad_job]));
    let mut relative = comm_entry(1, 1);
    relative.source = Source::File("comm.log".to_owned());
    assert_corrupt("relative path", &file(1, 1, &[relative]));
    let mut long = comm_entry(1, 1);
    long.source = Source::File(rooted(&"x".repeat(MAX_PATH_BYTES + 1 - ROOT.len())));
    assert_corrupt("long path", &file(1, 1, &[long]));
    assert_corrupt("counter at the top", &file(u64::MAX, 0, &[]));
    assert_corrupt(
        "counter overflows",
        &file(
            u64::MAX - 2,
            0,
            &[result_entry(u64::MAX - 1, 1), result_entry(u64::MAX, 2)],
        ),
    );
}

#[test]
fn the_longest_entry_fits_the_torn_tail_bound() {
    let longest = Entry {
        seq: u64::MAX,
        key: JobKey {
            job_id: JobId("a".repeat(MAX_JOB_ID)),
            generation: u64::MAX,
        },
        kind: Kind::CommLog,
        source: Source::File(rooted(&"x".repeat(MAX_PATH_BYTES - ROOT.len()))),
        length: u64::MAX,
        sha256: [0xFF; 32],
    };
    let frame = encode_frame(&longest).unwrap();
    assert!(frame.len() <= MAX_TORN, "{} of {MAX_TORN}", frame.len());
    assert!(frame.len() > MAX_TORN - 8);
}

// O17
#[test]
fn objects_no_entry_names_are_deleted_after_the_durability_steps() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    put_result(&mut ledger, 1);
    drop(ledger);
    let objects = objects_of(dir.path());
    fs::write(objects.join("2"), b"stray").unwrap();
    fs::write(objects.join("x.tmp"), b"stray").unwrap();
    fs::create_dir(objects.join("sub")).unwrap();
    let (probe, steps) = recording();
    let ledger = UploadLedger::open_with(dir.path(), ID, probe).unwrap();
    assert_eq!(names(&objects), ["1", "sub"]);
    assert_eq!(seqs(&ledger), [1]);
    let steps = steps.lock().unwrap().clone();
    let after = steps.iter().position(|s| *s == Step::AfterRename).unwrap();
    let first = steps
        .iter()
        .position(|s| *s == Step::BeforeObjectDelete)
        .unwrap();
    assert!(after < first);
    assert_eq!(
        steps
            .iter()
            .filter(|s| **s == Step::BeforeObjectDelete)
            .count(),
        2
    );
}

// O17
#[test]
fn a_failed_delete_at_open_is_warned_and_the_next_open_repeats_it() {
    let dir = tempfile::tempdir().unwrap();
    drop(UploadLedger::open_in(dir.path(), ID).unwrap());
    let objects = objects_of(dir.path());
    fs::write(objects.join("4"), b"stray").unwrap();
    let armed = Arc::new(AtomicBool::new(true));
    let ledger =
        UploadLedger::open_with(dir.path(), ID, failing_at(Step::BeforeObjectDelete, &armed))
            .unwrap();
    assert_eq!(names(&objects), ["4"]);
    drop(ledger);
    drop(UploadLedger::open_in(dir.path(), ID).unwrap());
    assert!(names(&objects).is_empty());
}

// O18
#[test]
fn an_entry_whose_bytes_are_missing_is_still_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    put_result(&mut ledger, 1);
    put_comm(&mut ledger, dir.path(), 2);
    drop(ledger);
    fs::remove_file(objects_of(dir.path()).join("1")).unwrap();
    fs::remove_file(comm_path(dir.path(), 2)).unwrap();
    let ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert_eq!(seqs(&ledger), [1, 2]);
    assert!(!ledger.object_path(ledger.next().unwrap()).exists());
}

// O19
#[test]
fn an_objects_path_that_is_a_file_fails_the_open_with_a_state_error() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(objects_of(dir.path()), b"not a directory").unwrap();
    let result = UploadLedger::open_in(dir.path(), ID);
    assert!(matches!(result, Err(LedgerError::State(_))));
    assert!(!file_of(dir.path()).exists());
}

// O19
#[cfg(unix)]
#[test]
fn an_objects_directory_writable_by_others_fails_the_open_with_a_state_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    drop(UploadLedger::open_in(dir.path(), ID).unwrap());
    fs::set_permissions(objects_of(dir.path()), fs::Permissions::from_mode(0o770)).unwrap();
    assert!(matches!(
        UploadLedger::open_in(dir.path(), ID),
        Err(LedgerError::State(_))
    ));
}

// O20
#[test]
fn a_second_opener_gets_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let first = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert!(matches!(
        UploadLedger::open_in(dir.path(), ID),
        Err(LedgerError::InUse)
    ));
    drop(first);
    assert!(UploadLedger::open_in(dir.path(), ID).is_ok());
}

// O21
#[test]
fn an_agent_id_that_cannot_name_a_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    for id in ["", "../x", "a/b", "a.b", &"x".repeat(65)] {
        assert!(matches!(
            UploadLedger::open_in(dir.path(), id),
            Err(LedgerError::BadAgentId)
        ));
    }
}

// O22
#[test]
fn the_files_and_objects_of_other_agent_ids_are_left_alone_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let mut other = UploadLedger::open_in(dir.path(), OTHER).unwrap();
    put_result(&mut other, 1);
    drop(other);
    let theirs = dir.path().join(OTHER);
    let objects = dir.path().join(format!("{OTHER}{OBJECTS_SUFFIX}"));
    let before = (
        fs::read(&theirs).unwrap(),
        fs::read(objects.join("1")).unwrap(),
    );
    // Their file, their objects directory and a stray file.
    fs::write(dir.path().join("garbage"), b"x").unwrap();
    assert_eq!(other_entries(dir.path(), ID), 3);
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert_eq!(put_result(&mut ledger, 1), 1);
    assert_eq!(fs::read(&theirs).unwrap(), before.0);
    assert_eq!(fs::read(objects.join("1")).unwrap(), before.1);
    assert_eq!(fs::read(dir.path().join("garbage")).unwrap(), b"x");
}

// O23
#[test]
fn a_failed_sync_at_open_fails_the_open_and_sweeps_nothing() {
    for step in [Step::AfterCut, Step::AfterRename] {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
        put_result(&mut ledger, 1);
        put_result(&mut ledger, 2);
        drop(ledger);
        let path = file_of(dir.path());
        let len = fs::metadata(&path).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(len - 2)
            .unwrap();
        let armed = Arc::new(AtomicBool::new(true));
        let result = UploadLedger::open_with(dir.path(), ID, failing_at(step, &armed));
        assert!(matches!(result, Err(LedgerError::Io(_))), "{step:?}");
        // The torn entry's object is unnamed, but nothing was swept.
        assert_eq!(names(&objects_of(dir.path())), ["1", "2"], "{step:?}");
    }
}

// E1
#[test]
fn a_result_is_stored_and_synced_before_its_frame_is_appended() {
    let dir = tempfile::tempdir().unwrap();
    let (probe, steps) = recording();
    let mut ledger = UploadLedger::open_with(dir.path(), ID, probe).unwrap();
    steps.lock().unwrap().clear();
    let seq = ledger.enqueue_result(&jk(1), b"the result").unwrap();
    assert_eq!(seq, 1);
    assert_eq!(
        *steps.lock().unwrap(),
        [
            Step::BeforeObjectWrite,
            Step::AfterObjectWrite,
            Step::AfterObjectSync,
            Step::BeforeWrite,
            Step::AfterWrite,
            Step::AfterSync
        ]
    );
    let entry = ledger.next().unwrap().clone();
    assert_eq!(
        (entry.seq, entry.kind, &entry.source, entry.length),
        (1, Kind::Result, &Source::Stored, 10)
    );
    assert_eq!(
        entry.sha256,
        <[u8; 32]>::from(Sha256::digest(b"the result"))
    );
    let object = ledger.object_path(&entry);
    assert_eq!(object, objects_of(dir.path()).join("1"));
    assert_eq!(fs::read(object).unwrap(), b"the result");
    let bytes = fs::read(file_of(dir.path())).unwrap();
    let parsed = parse(&bytes, ID).unwrap();
    assert_eq!(parsed.entries, [entry]);
    assert_eq!(parsed.len as u64, ledger.file_len);
    assert_eq!(ledger.stored_bytes, 10);
}

// E2
#[test]
fn a_comm_log_is_a_frame_only_and_its_file_is_not_opened() {
    let dir = tempfile::tempdir().unwrap();
    let (probe, steps) = recording();
    let mut ledger = UploadLedger::open_with(dir.path(), ID, probe).unwrap();
    steps.lock().unwrap().clear();
    let path = comm_path(dir.path(), 1);
    let seq = ledger
        .enqueue_comm_log(&jk(1), &path, fake_digest(123, 7))
        .unwrap();
    assert_eq!(seq, 1, "the file does not exist and is not needed");
    assert_eq!(
        *steps.lock().unwrap(),
        [Step::BeforeWrite, Step::AfterWrite, Step::AfterSync]
    );
    assert!(names(&objects_of(dir.path())).is_empty());
    let entry = ledger.next().unwrap();
    assert_eq!(
        entry.source,
        Source::File(path.to_str().unwrap().to_owned())
    );
    assert_eq!((entry.length, entry.sha256), (123, [7; 32]));
    assert_eq!(ledger.object_path(entry), path);
    assert_eq!(ledger.stored_bytes, 0);
}

// E3
#[test]
fn the_same_job_and_bytes_again_return_the_existing_number_and_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (probe, steps) = recording();
    let mut ledger = UploadLedger::open_with(dir.path(), ID, probe).unwrap();
    let r = ledger.enqueue_result(&jk(1), b"bytes").unwrap();
    let c = put_comm(&mut ledger, dir.path(), 2);
    let before = fs::read(file_of(dir.path())).unwrap();
    steps.lock().unwrap().clear();
    assert_eq!(ledger.enqueue_result(&jk(1), b"bytes").unwrap(), r);
    let path = comm_path(dir.path(), 2);
    let digest = digest_file(&path).unwrap();
    assert_eq!(ledger.enqueue_comm_log(&jk(2), &path, digest).unwrap(), c);
    // Also when the ledger is full.
    ledger.file_len = MAX_LEDGER_BYTES;
    ledger.stored_bytes = MAX_STORED_BYTES;
    assert_eq!(ledger.enqueue_result(&jk(1), b"bytes").unwrap(), r);
    assert_eq!(ledger.enqueue_comm_log(&jk(2), &path, digest).unwrap(), c);
    assert!(steps.lock().unwrap().is_empty());
    assert_eq!(fs::read(file_of(dir.path())).unwrap(), before);
    assert_eq!(ledger.highest(), 2);
}

// E4
#[test]
fn the_same_job_with_other_bytes_is_a_conflict_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    ledger.enqueue_result(&jk(1), b"bytes").unwrap();
    let path = comm_path(dir.path(), 2);
    ledger
        .enqueue_comm_log(&jk(2), &path, fake_digest(10, 1))
        .unwrap();
    let before = fs::read(file_of(dir.path())).unwrap();
    assert!(matches!(
        ledger.enqueue_result(&jk(1), b"other"),
        Err(LedgerError::Conflict)
    ));
    assert!(matches!(
        ledger.enqueue_result(&jk(1), b"longer bytes"),
        Err(LedgerError::Conflict)
    ));
    assert!(matches!(
        ledger.enqueue_comm_log(&jk(2), &path, fake_digest(11, 1)),
        Err(LedgerError::Conflict)
    ));
    assert!(matches!(
        ledger.enqueue_comm_log(&jk(2), &path, fake_digest(10, 2)),
        Err(LedgerError::Conflict)
    ));
    assert_eq!(fs::read(file_of(dir.path())).unwrap(), before);
    assert_eq!(names(&objects_of(dir.path())), ["1"]);
    assert_eq!(
        fs::read(objects_of(dir.path()).join("1")).unwrap(),
        b"bytes"
    );
    assert_eq!(ledger.highest(), 2);
    assert!(!ledger.is_poisoned());
}

// E5
#[test]
fn another_kind_or_generation_of_the_same_job_is_an_independent_entry() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    let path = comm_path(dir.path(), 1);
    assert_eq!(ledger.enqueue_result(&jk(1), b"a").unwrap(), 1);
    assert_eq!(
        ledger
            .enqueue_comm_log(&jk(1), &path, fake_digest(3, 1))
            .unwrap(),
        2
    );
    let mut next_generation = jk(1);
    next_generation.generation = 2;
    assert_eq!(ledger.enqueue_result(&next_generation, b"b").unwrap(), 3);
    assert_eq!(
        ledger
            .enqueue_comm_log(&next_generation, &path, fake_digest(3, 1))
            .unwrap(),
        4
    );
    assert_eq!(seqs(&ledger), [1, 3, 2, 4]);
    // Read back, the result and the communication log of the same job are both there.
    ledger.reopen().unwrap();
    assert_eq!(seqs(&ledger), [1, 3, 2, 4]);
    let kinds_of_first: Vec<(u64, Kind)> = ledger
        .pending()
        .filter(|entry| entry.key == jk(1))
        .map(|entry| (entry.seq, entry.kind))
        .collect();
    assert_eq!(kinds_of_first, [(1, Kind::Result), (2, Kind::CommLog)]);
}

// E7a
#[test]
fn a_file_reaching_exactly_the_cap_and_stored_bytes_reaching_exactly_the_cap_are_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    let path = comm_path(dir.path(), 1);
    let entry = Entry {
        seq: 1,
        key: jk(1),
        kind: Kind::CommLog,
        source: Source::File(path.to_str().unwrap().to_owned()),
        length: 1,
        sha256: [1; 32],
    };
    let frame_len = encode_frame(&entry).unwrap().len() as u64;
    ledger.file_len = MAX_LEDGER_BYTES - frame_len + 1;
    assert!(matches!(
        ledger.enqueue_comm_log(&jk(1), &path, fake_digest(1, 1)),
        Err(LedgerError::Full)
    ));
    ledger.file_len = MAX_LEDGER_BYTES - frame_len;
    assert_eq!(
        ledger
            .enqueue_comm_log(&jk(1), &path, fake_digest(1, 1))
            .unwrap(),
        1
    );
    assert_eq!(ledger.file_len, MAX_LEDGER_BYTES);
    assert_eq!(
        fs::metadata(file_of(dir.path())).unwrap().len(),
        MAX_LEDGER_BYTES
    );

    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    ledger.stored_bytes = MAX_STORED_BYTES - 1;
    assert_eq!(ledger.enqueue_result(&jk(1), b"a").unwrap(), 1);
    assert_eq!(ledger.stored_bytes, MAX_STORED_BYTES);
    assert!(matches!(
        ledger.enqueue_result(&jk(2), b"a"),
        Err(LedgerError::Full)
    ));
}

// O8
#[test]
fn a_torn_tail_of_exactly_the_longest_unfinished_frame_is_cut() {
    let mut bytes = file(1, 1, &[result_entry(1, 1)]);
    let whole = bytes.len();
    bytes.extend(std::iter::repeat_n(0u8, MAX_TORN));
    assert_eq!(parse(&bytes, ID).unwrap().len, whole);
}

// E6
#[test]
fn a_key_delivered_earlier_gets_a_new_entry() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert_eq!(ledger.enqueue_result(&jk(1), b"a").unwrap(), 1);
    ledger.finished(1).unwrap();
    assert_eq!(ledger.enqueue_result(&jk(1), b"other").unwrap(), 2);
    assert_eq!(
        fs::read(objects_of(dir.path()).join("2")).unwrap(),
        b"other"
    );
}

// E7
#[test]
fn a_full_ledger_refuses_without_an_object_or_a_number() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    let real_len = ledger.file_len;
    ledger.file_len = MAX_LEDGER_BYTES - 1;
    assert!(matches!(
        ledger.enqueue_result(&jk(1), b"a"),
        Err(LedgerError::Full)
    ));
    assert!(matches!(
        ledger.enqueue_comm_log(&jk(1), &comm_path(dir.path(), 1), fake_digest(1, 1)),
        Err(LedgerError::Full)
    ));
    assert!(names(&objects_of(dir.path())).is_empty());
    assert_eq!(ledger.highest(), 0);
    ledger.file_len = real_len;
    // The stored cap refuses a result and still takes a communication log.
    ledger.stored_bytes = MAX_STORED_BYTES;
    assert!(matches!(
        ledger.enqueue_result(&jk(1), b"a"),
        Err(LedgerError::Full)
    ));
    assert!(names(&objects_of(dir.path())).is_empty());
    assert_eq!(
        ledger
            .enqueue_comm_log(&jk(1), &comm_path(dir.path(), 1), fake_digest(1, 1))
            .unwrap(),
        1
    );
    ledger.stored_bytes = 0;
    assert_eq!(ledger.enqueue_result(&jk(2), b"a").unwrap(), 2);
}

// E8
#[test]
fn a_result_over_the_cap_or_a_comm_log_over_four_gib_is_too_large() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    let big = vec![0u8; MAX_RESULT_BYTES as usize + 1];
    assert!(matches!(
        ledger.enqueue_result(&jk(1), &big),
        Err(LedgerError::TooLarge)
    ));
    let path = comm_path(dir.path(), 1);
    assert!(matches!(
        ledger.enqueue_comm_log(&jk(1), &path, fake_digest(MAX_COMM_LOG_BYTES + 1, 1)),
        Err(LedgerError::TooLarge)
    ));
    assert_eq!(ledger.highest(), 0);
    assert!(names(&objects_of(dir.path())).is_empty());
    assert_eq!(MAX_COMM_LOG_BYTES, 4 << 30);
    assert_eq!(
        ledger
            .enqueue_comm_log(&jk(1), &path, fake_digest(MAX_COMM_LOG_BYTES, 1))
            .unwrap(),
        1
    );
}

// E9
#[test]
fn a_bad_source_path_or_job_id_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    let digest = fake_digest(1, 1);
    let long = PathBuf::from(rooted(&"x".repeat(MAX_PATH_BYTES + 1 - ROOT.len())));
    for path in [PathBuf::from("comm.log"), PathBuf::from(""), long] {
        assert!(
            matches!(
                ledger.enqueue_comm_log(&jk(1), &path, digest),
                Err(LedgerError::BadSource)
            ),
            "{path:?}"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff.log"));
        assert!(matches!(
            ledger.enqueue_comm_log(&jk(1), &path, digest),
            Err(LedgerError::BadSource)
        ));
    }
    let exactly = PathBuf::from(rooted(&"x".repeat(MAX_PATH_BYTES - ROOT.len())));
    assert!(ledger.enqueue_comm_log(&jk(1), &exactly, digest).is_ok());
    for id in ["", "../x", "a/b", "a.b", &"x".repeat(65)] {
        let key = JobKey {
            job_id: JobId(id.to_owned()),
            generation: 1,
        };
        assert!(matches!(
            ledger.enqueue_result(&key, b"a"),
            Err(LedgerError::BadJobId)
        ));
        assert!(matches!(
            ledger.enqueue_comm_log(&key, &comm_path(dir.path(), 1), digest),
            Err(LedgerError::BadJobId)
        ));
    }
    assert_eq!(ledger.highest(), 1);
}

// E10
#[test]
fn a_result_or_log_of_length_zero_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert_eq!(ledger.enqueue_result(&jk(1), b"").unwrap(), 1);
    assert_eq!(fs::read(objects_of(dir.path()).join("1")).unwrap(), b"");
    let path = comm_path(dir.path(), 2);
    fs::write(&path, b"").unwrap();
    let digest = digest_file(&path).unwrap();
    assert_eq!(digest.length(), 0);
    assert_eq!(ledger.enqueue_comm_log(&jk(2), &path, digest).unwrap(), 2);
    drop(ledger);
    assert_eq!(
        seqs(&UploadLedger::open_in(dir.path(), ID).unwrap()),
        [1, 2]
    );
}

// E11
#[test]
fn a_poisoned_ledger_refuses_every_enqueue_even_a_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let mut ledger =
        UploadLedger::open_with(dir.path(), ID, failing_at(Step::AfterWrite, &armed)).unwrap();
    put_result(&mut ledger, 1);
    armed.store(true, Ordering::SeqCst);
    assert!(ledger.enqueue_result(&jk(2), b"x").is_err());
    armed.store(false, Ordering::SeqCst);
    assert!(ledger.is_poisoned());
    assert!(matches!(
        ledger.enqueue_result(&jk(1), b"result 1"),
        Err(LedgerError::Poisoned)
    ));
    assert!(matches!(
        ledger.enqueue_result(&jk(3), b"x"),
        Err(LedgerError::Poisoned)
    ));
    assert!(matches!(
        ledger.enqueue_comm_log(&jk(3), &comm_path(dir.path(), 3), fake_digest(1, 1)),
        Err(LedgerError::Poisoned)
    ));
}

// E12
#[test]
fn a_failed_object_write_removes_it_and_leaves_the_ledger_usable() {
    for step in [Step::BeforeObjectWrite, Step::AfterObjectWrite] {
        let dir = tempfile::tempdir().unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let mut ledger = UploadLedger::open_with(dir.path(), ID, failing_at(step, &armed)).unwrap();
        put_result(&mut ledger, 1);
        let before = fs::read(file_of(dir.path())).unwrap();
        armed.store(true, Ordering::SeqCst);
        assert!(
            matches!(
                ledger.enqueue_result(&jk(2), b"second"),
                Err(LedgerError::Io(_))
            ),
            "{step:?}"
        );
        assert_eq!(names(&objects_of(dir.path())), ["1"], "{step:?}");
        assert_eq!(fs::read(file_of(dir.path())).unwrap(), before);
        assert!(!ledger.is_poisoned());
        assert_eq!(ledger.highest(), 1);
        armed.store(false, Ordering::SeqCst);
        assert_eq!(
            ledger.enqueue_result(&jk(2), b"second").unwrap(),
            2,
            "{step:?}"
        );
    }
}

// E13
#[test]
fn a_leftover_file_with_the_fresh_name_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    fs::write(objects_of(dir.path()).join("1"), b"old leftover, longer").unwrap();
    assert_eq!(ledger.enqueue_result(&jk(1), b"new").unwrap(), 1);
    assert_eq!(fs::read(objects_of(dir.path()).join("1")).unwrap(), b"new");
}

// E13
#[test]
fn a_leftover_that_cannot_be_removed_fails_the_enqueue_without_poisoning() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    fs::create_dir(objects_of(dir.path()).join("1")).unwrap();
    assert!(matches!(
        ledger.enqueue_result(&jk(1), b"new"),
        Err(LedgerError::Io(_))
    ));
    assert!(!ledger.is_poisoned());
    assert_eq!(ledger.highest(), 0);
    assert!(ledger.next().is_none());
}

// E14
#[test]
fn a_failed_append_before_the_write_poisons_and_the_reopen_sweeps_the_object() {
    let dir = tempfile::tempdir().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let mut ledger =
        UploadLedger::open_with(dir.path(), ID, failing_at(Step::BeforeWrite, &armed)).unwrap();
    armed.store(true, Ordering::SeqCst);
    assert!(matches!(
        ledger.enqueue_result(&jk(1), b"bytes"),
        Err(LedgerError::Io(_))
    ));
    assert!(ledger.is_poisoned());
    assert_eq!(names(&objects_of(dir.path())), ["1"], "the object is kept");
    assert_eq!(ledger.highest(), 0);
    armed.store(false, Ordering::SeqCst);
    ledger.reopen().unwrap();
    assert!(ledger.next().is_none());
    assert!(names(&objects_of(dir.path())).is_empty());
    assert_eq!(ledger.enqueue_result(&jk(1), b"bytes").unwrap(), 1);
}

// E15
#[test]
fn a_failed_append_after_the_write_keeps_the_object_and_the_entry_is_pending_after_reopen() {
    for step in [Step::AfterWrite, Step::AfterSync] {
        let dir = tempfile::tempdir().unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let mut ledger = UploadLedger::open_with(dir.path(), ID, failing_at(step, &armed)).unwrap();
        armed.store(true, Ordering::SeqCst);
        assert!(matches!(
            ledger.enqueue_result(&jk(1), b"bytes"),
            Err(LedgerError::Io(_))
        ));
        assert!(ledger.is_poisoned(), "{step:?}");
        assert_eq!(names(&objects_of(dir.path())), ["1"]);
        armed.store(false, Ordering::SeqCst);
        ledger.reopen().unwrap();
        let entry = ledger.next().unwrap().clone();
        assert_eq!(entry.seq, 1, "{step:?}");
        let object = fs::read(ledger.object_path(&entry)).unwrap();
        assert_eq!(object, b"bytes");
        assert_eq!(<[u8; 32]>::from(Sha256::digest(&object)), entry.sha256);
        // A retried enqueue finds the entry.
        assert_eq!(ledger.enqueue_result(&jk(1), b"bytes").unwrap(), 1);
        assert_eq!(ledger.highest(), 1);
    }
}

// E14a
#[test]
fn a_reopen_that_finds_the_file_gone_creates_nothing_sweeps_nothing_and_stays_poisoned() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    put_result(&mut ledger, 1);
    ledger.poisoned = true;
    fs::remove_file(file_of(dir.path())).unwrap();
    assert!(matches!(
        ledger.reopen(),
        Err(LedgerError::Corrupt("the ledger file is gone"))
    ));
    assert!(!file_of(dir.path()).exists(), "no file was created");
    assert_eq!(names(&objects_of(dir.path())), ["1"]);
    assert_eq!(seqs(&ledger), [1]);
    assert_eq!(ledger.highest(), 1);
    assert!(ledger.is_poisoned());
    assert!(matches!(
        ledger.enqueue_result(&jk(2), b"x"),
        Err(LedgerError::Poisoned)
    ));
}

// E14b
#[test]
fn a_reopen_that_finds_the_file_corrupt_keeps_memory_sweeps_nothing_and_stays_poisoned() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    put_result(&mut ledger, 1);
    put_comm(&mut ledger, dir.path(), 2);
    fs::write(objects_of(dir.path()).join("99"), b"stray").unwrap();
    ledger.poisoned = true;
    let (file_len, stored) = (ledger.file_len, ledger.stored_bytes);
    fs::write(file_of(dir.path()), b"not a ledger").unwrap();
    assert!(matches!(ledger.reopen(), Err(LedgerError::Corrupt(_))));
    assert!(ledger.is_poisoned());
    assert_eq!(seqs(&ledger), [1, 2]);
    assert_eq!(
        (ledger.highest(), ledger.file_len, ledger.stored_bytes),
        (2, file_len, stored)
    );
    assert_eq!(names(&objects_of(dir.path())), ["1", "99"]);
    assert_eq!(fs::read(file_of(dir.path())).unwrap(), b"not a ledger");
    assert!(matches!(
        ledger.enqueue_result(&jk(3), b"x"),
        Err(LedgerError::Poisoned)
    ));
}

// E16
#[test]
fn an_enqueue_after_a_failed_reopen_is_poisoned_and_the_entries_stay_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let mut ledger =
        UploadLedger::open_with(dir.path(), ID, failing_at(Step::AfterRename, &armed)).unwrap();
    put_result(&mut ledger, 1);
    put_comm(&mut ledger, dir.path(), 2);
    let (file_len, stored) = (ledger.file_len, ledger.stored_bytes);
    armed.store(true, Ordering::SeqCst);
    assert!(matches!(ledger.reopen(), Err(LedgerError::Io(_))));
    assert!(ledger.is_poisoned());
    assert!(matches!(
        ledger.enqueue_result(&jk(3), b"x"),
        Err(LedgerError::Poisoned)
    ));
    assert!(matches!(ledger.finished(1), Err(LedgerError::Poisoned)));
    assert_eq!(seqs(&ledger), [1, 2]);
    assert_eq!(
        (ledger.highest(), ledger.file_len, ledger.stored_bytes),
        (2, file_len, stored)
    );
    // Another opener may take the lock the failed reopen released; this one reopens after.
    armed.store(false, Ordering::SeqCst);
    ledger.reopen().unwrap();
    assert_eq!(put_result(&mut ledger, 3), 3);
}

// F1
#[test]
fn finished_rewrites_the_file_then_memory_then_deletes_the_object() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_of(dir.path());
    let on_disk = Arc::new(Mutex::new(Vec::new()));
    let (seen, disk) = (on_disk.clone(), path.clone());
    let steps = Arc::new(Mutex::new(Vec::new()));
    let log = steps.clone();
    let probe: Probe = Box::new(move |step| {
        log.lock().unwrap().push(step);
        if step == Step::BeforeObjectDelete {
            let bytes = fs::read(&disk).unwrap();
            seen.lock()
                .unwrap()
                .push(parse(&bytes, ID).unwrap().entries.len());
        }
        Ok(())
    });
    let mut ledger = UploadLedger::open_with(dir.path(), ID, probe).unwrap();
    for n in 1..=3 {
        put_result(&mut ledger, n);
    }
    steps.lock().unwrap().clear();
    ledger.finished(2).unwrap();
    assert_eq!(
        *steps.lock().unwrap(),
        [
            Step::BeforeRename,
            Step::AfterRename,
            Step::AfterRenameSync,
            Step::BeforeObjectDelete
        ]
    );
    assert_eq!(
        *on_disk.lock().unwrap(),
        [2],
        "the file was rewritten first"
    );
    let bytes = fs::read(&path).unwrap();
    let header = header_of(&bytes);
    assert_eq!((header.highest, header.retained), (3, 2));
    assert_eq!(seqs(&ledger), [1, 3]);
    assert_eq!(names(&objects_of(dir.path())), ["1", "3"]);
    assert_eq!(ledger.file_len, bytes.len() as u64);
    assert_eq!(ledger.stored_bytes, 16);
    assert!(!tmp_of(dir.path()).exists());
    // The first and the last entries too.
    ledger.finished(1).unwrap();
    ledger.finished(3).unwrap();
    assert!(ledger.next().is_none());
}

// F2
#[test]
fn removing_the_last_entry_leaves_a_header_only_file_that_keeps_the_counter() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    put_result(&mut ledger, 1);
    put_comm(&mut ledger, dir.path(), 2);
    ledger.finished(1).unwrap();
    ledger.finished(2).unwrap();
    let bytes = fs::read(file_of(dir.path())).unwrap();
    let header = header_of(&bytes);
    assert_eq!((header.highest, header.retained), (2, 0));
    assert_eq!(frame_starts(&bytes).len(), 1);
    drop(ledger);
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert_eq!(ledger.highest(), 2);
    assert_eq!(put_result(&mut ledger, 3), 3);
}

// F3
#[test]
fn finishing_a_number_that_is_not_pending_is_ok_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (probe, steps) = recording();
    let mut ledger = UploadLedger::open_with(dir.path(), ID, probe).unwrap();
    for n in 1..=3 {
        put_result(&mut ledger, n);
    }
    ledger.finished(2).unwrap();
    let before = fs::read(file_of(dir.path())).unwrap();
    steps.lock().unwrap().clear();
    ledger.finished(2).unwrap();
    ledger.finished(0).unwrap();
    assert!(steps.lock().unwrap().is_empty());
    assert_eq!(fs::read(file_of(dir.path())).unwrap(), before);
    assert_eq!(seqs(&ledger), [1, 3]);
}

// F4
#[test]
fn finishing_a_number_above_the_highest_is_out_of_range_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (probe, steps) = recording();
    let mut ledger = UploadLedger::open_with(dir.path(), ID, probe).unwrap();
    put_result(&mut ledger, 1);
    let before = fs::read(file_of(dir.path())).unwrap();
    steps.lock().unwrap().clear();
    assert!(matches!(
        ledger.finished(2),
        Err(LedgerError::OutOfRange { seq: 2, highest: 1 })
    ));
    assert!(steps.lock().unwrap().is_empty());
    assert_eq!(fs::read(file_of(dir.path())).unwrap(), before);
    assert!(!ledger.is_poisoned());
}

// F5
#[test]
fn finishing_on_a_poisoned_ledger_says_poisoned_before_anything_else() {
    let dir = tempfile::tempdir().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let mut ledger =
        UploadLedger::open_with(dir.path(), ID, failing_at(Step::BeforeWrite, &armed)).unwrap();
    put_result(&mut ledger, 1);
    armed.store(true, Ordering::SeqCst);
    assert!(ledger.enqueue_result(&jk(2), b"x").is_err());
    assert!(matches!(ledger.finished(1), Err(LedgerError::Poisoned)));
    assert!(matches!(ledger.finished(99), Err(LedgerError::Poisoned)));
    assert_eq!(seqs(&ledger), [1]);
}

// F6
#[test]
fn a_rewrite_that_fails_before_the_rename_keeps_everything_and_may_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let mut ledger =
        UploadLedger::open_with(dir.path(), ID, failing_at(Step::BeforeRename, &armed)).unwrap();
    put_result(&mut ledger, 1);
    put_result(&mut ledger, 2);
    let before = fs::read(file_of(dir.path())).unwrap();
    armed.store(true, Ordering::SeqCst);
    assert!(matches!(ledger.finished(1), Err(LedgerError::Io(_))));
    assert_eq!(fs::read(file_of(dir.path())).unwrap(), before);
    assert!(!tmp_of(dir.path()).exists());
    assert_eq!(seqs(&ledger), [1, 2]);
    assert_eq!(names(&objects_of(dir.path())), ["1", "2"]);
    assert!(!ledger.is_poisoned());
    assert_eq!(put_result(&mut ledger, 3), 3, "still writable");
    armed.store(false, Ordering::SeqCst);
    ledger.finished(1).unwrap();
    assert_eq!(seqs(&ledger), [2, 3]);
}

// F7
#[test]
fn a_failed_rename_poisons_only_when_the_old_file_cannot_be_reopened() {
    // The probe removes the temporary file, so the rename fails with the old file in place; for
    // the second ledger it removes the old file as well.
    for (remove_old, poisoned) in [(false, false), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let (flag, tmp, path) = (armed.clone(), tmp_of(dir.path()), file_of(dir.path()));
        let probe: Probe = Box::new(move |step| {
            if step == Step::BeforeRename && flag.load(Ordering::SeqCst) {
                fs::remove_file(&tmp)?;
                if remove_old {
                    fs::remove_file(&path)?;
                }
            }
            Ok(())
        });
        let mut ledger = UploadLedger::open_with(dir.path(), ID, probe).unwrap();
        put_result(&mut ledger, 1);
        let before = fs::read(file_of(dir.path())).unwrap();
        armed.store(true, Ordering::SeqCst);
        assert!(matches!(ledger.finished(1), Err(LedgerError::Io(_))));
        assert_eq!(ledger.is_poisoned(), poisoned);
        assert_eq!(seqs(&ledger), [1]);
        if !poisoned {
            assert_eq!(fs::read(file_of(dir.path())).unwrap(), before);
            assert_eq!(put_result(&mut ledger, 2), 2);
        }
    }
}

// F8
#[test]
fn a_failed_sync_after_the_rename_poisons_and_the_reopen_sweeps_the_object() {
    let dir = tempfile::tempdir().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let mut ledger =
        UploadLedger::open_with(dir.path(), ID, failing_at(Step::AfterRename, &armed)).unwrap();
    put_result(&mut ledger, 1);
    put_result(&mut ledger, 2);
    armed.store(true, Ordering::SeqCst);
    assert!(matches!(ledger.finished(1), Err(LedgerError::Io(_))));
    assert!(ledger.is_poisoned());
    assert_eq!(seqs(&ledger), [2], "the entry is gone from memory");
    assert_eq!(
        names(&objects_of(dir.path())),
        ["1", "2"],
        "the object stays"
    );
    armed.store(false, Ordering::SeqCst);
    ledger.reopen().unwrap();
    assert_eq!(seqs(&ledger), [2]);
    let before = fs::read(file_of(dir.path())).unwrap();
    ledger.finished(1).unwrap();
    assert_eq!(fs::read(file_of(dir.path())).unwrap(), before);
    assert_eq!(names(&objects_of(dir.path())), ["2"]);
}

// F9
#[test]
fn a_failed_object_delete_is_ok_and_the_next_open_sweeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let mut ledger =
        UploadLedger::open_with(dir.path(), ID, failing_at(Step::BeforeObjectDelete, &armed))
            .unwrap();
    put_result(&mut ledger, 1);
    armed.store(true, Ordering::SeqCst);
    ledger.finished(1).unwrap();
    assert!(!ledger.is_poisoned());
    assert!(ledger.next().is_none());
    assert_eq!(names(&objects_of(dir.path())), ["1"]);
    drop(ledger);
    drop(UploadLedger::open_in(dir.path(), ID).unwrap());
    assert!(names(&objects_of(dir.path())).is_empty());
}

// F10
#[test]
fn finishing_a_comm_log_never_touches_its_file() {
    let dir = tempfile::tempdir().unwrap();
    let (probe, steps) = recording();
    let mut ledger = UploadLedger::open_with(dir.path(), ID, probe).unwrap();
    let seq = put_comm(&mut ledger, dir.path(), 1);
    let path = comm_path(dir.path(), 1);
    let before = fs::metadata(&path).unwrap().modified().unwrap();
    steps.lock().unwrap().clear();
    ledger.finished(seq).unwrap();
    assert!(!steps.lock().unwrap().contains(&Step::BeforeObjectDelete));
    assert_eq!(fs::read(&path).unwrap(), b"log 1");
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
    drop(ledger);
    drop(UploadLedger::open_in(dir.path(), ID).unwrap());
    assert!(path.exists(), "a sweep does not reach it either");
}

// F11
#[test]
fn an_enqueue_after_a_removal_never_reuses_a_removed_number() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    for n in 1..=3 {
        put_result(&mut ledger, n);
    }
    ledger.finished(3).unwrap();
    assert_eq!(put_result(&mut ledger, 4), 4);
    ledger.finished(2).unwrap();
    assert_eq!(put_result(&mut ledger, 5), 5);
    drop(ledger);
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert_eq!(seqs(&ledger), [1, 4, 5]);
    assert_eq!(put_result(&mut ledger, 6), 6);
}

#[test]
fn the_counter_stops_below_the_largest_number_the_file_can_read_back() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(file_of(dir.path()), file(u64::MAX - 3, 0, &[])).unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert_eq!(put_result(&mut ledger, 1), u64::MAX - 2);
    assert_eq!(put_result(&mut ledger, 2), u64::MAX - 1);
    assert!(matches!(
        ledger.enqueue_result(&jk(3), b"x"),
        Err(LedgerError::Corrupt(_))
    ));
    drop(ledger);
    let ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert_eq!(ledger.highest(), u64::MAX - 1);
}

// N1
#[test]
fn an_empty_ledger_has_no_next_entry() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert!(ledger.next().is_none());
    assert_eq!(ledger.pending().count(), 0);
}

// N2
#[test]
fn comm_logs_alone_upload_in_number_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    put_comm(&mut ledger, dir.path(), 1);
    put_comm(&mut ledger, dir.path(), 2);
    assert_eq!(ledger.next().unwrap().seq, 1);
    assert_eq!(seqs(&ledger), [1, 2]);
}

// N3
#[test]
fn results_upload_before_comm_logs_each_in_number_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    put_comm(&mut ledger, dir.path(), 1);
    put_result(&mut ledger, 2);
    put_result(&mut ledger, 3);
    put_comm(&mut ledger, dir.path(), 4);
    assert_eq!(seqs(&ledger), [2, 3, 1, 4]);
    let mut order = Vec::new();
    while let Some(entry) = ledger.next() {
        let seq = entry.seq;
        order.push(seq);
        ledger.finished(seq).unwrap();
    }
    assert_eq!(order, [2, 3, 1, 4]);
}

// N4
#[test]
fn a_result_enqueued_while_a_comm_log_is_held_is_next_and_the_held_one_still_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    let held = put_comm(&mut ledger, dir.path(), 1);
    assert_eq!(ledger.next().unwrap().seq, held);
    let result = put_result(&mut ledger, 2);
    assert_eq!(ledger.next().unwrap().seq, result);
    ledger.finished(held).unwrap();
    assert_eq!(seqs(&ledger), [result]);
}

// N5
#[test]
fn the_order_after_a_reopen_comes_from_the_file_alone() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    put_comm(&mut ledger, dir.path(), 1);
    put_result(&mut ledger, 2);
    put_comm(&mut ledger, dir.path(), 3);
    put_result(&mut ledger, 4);
    ledger.finished(2).unwrap();
    let before: Vec<Entry> = ledger.pending().cloned().collect();
    drop(ledger);
    let mut ledger = UploadLedger::open_in(dir.path(), ID).unwrap();
    assert_eq!(ledger.pending().cloned().collect::<Vec<_>>(), before);
    assert_eq!(seqs(&ledger), [4, 1, 3]);
    ledger.reopen().unwrap();
    assert_eq!(seqs(&ledger), [4, 1, 3]);
}

// N6
#[test]
fn a_poisoned_ledger_still_serves_its_entries_from_memory() {
    let dir = tempfile::tempdir().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let mut ledger =
        UploadLedger::open_with(dir.path(), ID, failing_at(Step::AfterSync, &armed)).unwrap();
    put_comm(&mut ledger, dir.path(), 1);
    put_result(&mut ledger, 2);
    armed.store(true, Ordering::SeqCst);
    assert!(ledger.enqueue_result(&jk(3), b"x").is_err());
    assert!(ledger.is_poisoned());
    assert_eq!(seqs(&ledger), [2, 1]);
    assert_eq!(ledger.next().unwrap().seq, 2);
}

#[test]
fn a_file_is_digested_through_one_handle_to_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("abc");
    fs::write(&path, b"abc").unwrap();
    let digest = digest_file(&path).unwrap();
    assert_eq!(digest.length(), 3);
    assert_eq!(
        digest.sha256()[..4],
        [0xba, 0x78, 0x16, 0xbf],
        "SHA-256 of \"abc\""
    );
    // Longer than the read buffer.
    let big = vec![7u8; (64 << 10) * 2 + 5];
    fs::write(&path, &big).unwrap();
    let digest = digest_file(&path).unwrap();
    assert_eq!(digest.length(), big.len() as u64);
    assert_eq!(*digest.sha256(), <[u8; 32]>::from(Sha256::digest(&big)));
    assert!(digest_file(&dir.path().join("missing")).is_err());
}
