//! The issue-time mark's and the clock's cases (ADR-486; the case table's letter and number begin
//! each name). The control channel's end-to-end cases are in `control::tests`.

use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use super::*;

const AGENT: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const OTHER_AGENT: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d50";

struct Fixed(u64);

impl Clock for Fixed {
    fn now_unix(&self) -> u64 {
        self.0
    }
}

fn file_in(dir: &Path) -> PathBuf {
    path_of(dir, AGENT)
}

fn json(value: u64) -> String {
    format!(r#"{{"version":1,"agent_id":"{AGENT}","highest_issued_at":{value}}}"#)
}

fn mark_in(dir: &Path) -> IssueMark {
    IssueMark::open_in(dir, AGENT).unwrap()
}

/// A probe that counts the writes that begin and fails at `at` while `armed` is set.
fn failing_at(at: Step, armed: &Arc<AtomicBool>, writes: &Arc<AtomicUsize>) -> Probe {
    let (armed, writes) = (armed.clone(), writes.clone());
    Box::new(move |step| {
        if step == Step::BeforeWrite {
            writes.fetch_add(1, Ordering::SeqCst);
        }
        if step == at && armed.load(Ordering::SeqCst) {
            return Err(io::Error::other("scripted"));
        }
        Ok(())
    })
}

// T1 to T4, T6, T7, T11: expired iff the later of the clock and the mark reaches the deadline.
#[test]
fn t_the_later_of_the_clock_and_the_mark_is_compared_with_the_deadline_at_equality() {
    let dir = tempfile::tempdir().unwrap();
    let marks = mark_in(dir.path());
    let judge = |clock: u64, deadline: u64| marks.judge(&Fixed(clock), deadline);
    // The mark is 0: the clock alone decides, at the boundary exactly.
    assert_eq!(judge(1_999, 2_000), Verdict::Open);
    assert_eq!(judge(2_000, 2_000), Verdict::Expired { by_mark: false });
    assert_eq!(judge(2_001, 2_000), Verdict::Expired { by_mark: false });
    // T6: the mark is above the clock and below the deadline: open.
    marks.raise(1_500);
    assert_eq!(judge(500, 2_000), Verdict::Open);
    // T5, T7: the mark reaches the deadline while the clock is behind: the mark decides.
    assert_eq!(judge(500, 1_500), Verdict::Expired { by_mark: true });
    assert_eq!(judge(500, 1_000), Verdict::Expired { by_mark: true });
    // T12: a clock before the epoch reads as 0 (below), so the mark decides.
    assert_eq!(judge(0, 1_500), Verdict::Expired { by_mark: true });
}

// T12
#[test]
fn t12_a_time_before_the_epoch_is_zero_seconds() {
    assert_eq!(unix_secs(UNIX_EPOCH - Duration::from_secs(5)), 0);
    assert_eq!(unix_secs(UNIX_EPOCH + Duration::from_millis(7_999)), 7);
    assert!(SystemClock.now_unix() > 1_700_000_000, "a real clock");
}

// W7: the only change is a maximum.
#[test]
fn w7_raise_never_lowers_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let marks = mark_in(dir.path());
    marks.raise(2_000);
    marks.raise(1_000);
    assert_eq!(marks.highest(), 2_000);
    assert!(!file_in(dir.path()).exists(), "raise writes nothing");
    marks.raise(u64::MAX);
    marks.raise(5);
    assert_eq!(marks.highest(), u64::MAX, "no cap (T13)");
}

// F1
#[test]
fn f1_an_absent_file_is_zero_is_not_created_by_open_and_is_created_by_the_first_sync_0600() {
    let dir = tempfile::tempdir().unwrap();
    let marks = mark_in(dir.path());
    assert_eq!(marks.highest(), 0);
    assert!(!marks.owes_write());
    assert!(!file_in(dir.path()).exists(), "the reader creates nothing");
    marks.sync().unwrap();
    assert!(
        !file_in(dir.path()).exists(),
        "nothing owed, nothing written"
    );
    marks.raise(1_000);
    assert!(marks.owes_write());
    marks.sync().unwrap();
    assert!(!marks.owes_write());
    assert_eq!(
        fs::read_to_string(file_in(dir.path())).unwrap(),
        format!("{}\n", json(1_000))
    );
    assert!(
        !dir.path()
            .join(format!("{AGENT}{FILE_SUFFIX}.tmp"))
            .exists()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(file_in(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

// F2
#[test]
fn f2_a_present_file_is_read_and_a_lower_issue_time_writes_nothing_a_higher_one_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(file_in(dir.path()), json(5_000)).unwrap();
    let writes = Arc::new(AtomicUsize::new(0));
    let probe = failing_at(
        Step::BeforeWrite,
        &Arc::new(AtomicBool::new(false)),
        &writes,
    );
    let marks = IssueMark::open_with(dir.path(), AGENT, probe).unwrap();
    assert_eq!(marks.highest(), 5_000);
    assert!(!marks.owes_write());
    marks.raise(4_000);
    marks.sync().unwrap();
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    marks.raise(6_000);
    marks.sync().unwrap();
    marks.sync().unwrap();
    assert_eq!(
        writes.load(Ordering::SeqCst),
        1,
        "one write, however often synced"
    );
    drop(marks);
    assert_eq!(
        fs::read_to_string(file_in(dir.path())).unwrap(),
        format!("{}\n", json(6_000))
    );
}

// F3
#[test]
fn f3_a_file_that_is_not_the_writers_object_is_corrupt_and_never_read_as_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path());
    let good = json(7);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("not json", b"mark".to_vec()),
        ("an array", b"[]".to_vec()),
        (
            "an unknown field",
            good.replace('}', r#","extra":1}"#).into_bytes(),
        ),
        (
            "a missing field",
            format!(r#"{{"version":1,"agent_id":"{AGENT}"}}"#).into_bytes(),
        ),
        (
            "another version",
            good.replace("\"version\":1", "\"version\":2").into_bytes(),
        ),
        (
            "another agent ID",
            good.replace(AGENT, OTHER_AGENT).into_bytes(),
        ),
        (
            "a negative value",
            good.replace("\"highest_issued_at\":7", "\"highest_issued_at\":-1")
                .into_bytes(),
        ),
        (
            "a value over u64",
            good.replace(
                "\"highest_issued_at\":7",
                "\"highest_issued_at\":18446744073709551616",
            )
            .into_bytes(),
        ),
        ("a fraction", good.replace(":7}", ":7.5}").into_bytes()),
        ("oversize", vec![b' '; MAX_FILE as usize + 1]),
    ];
    for (name, bytes) in cases {
        fs::write(&path, &bytes).unwrap();
        match IssueMark::open_in(dir.path(), AGENT) {
            Err(MarkError::Corrupt(..)) => {}
            other => panic!("{name}: {:?}", other.map(|_| ())),
        }
        assert_eq!(fs::read(&path).unwrap(), bytes, "{name}: never repaired");
    }
    // A content that is not the expected object keeps the parser's error.
    fs::write(&path, b"[]").unwrap();
    let error = IssueMark::open_in(dir.path(), AGENT).err().unwrap();
    assert!(std::error::Error::source(&error).is_some(), "{error}");
    // The largest value reads back whole.
    fs::write(&path, json(u64::MAX)).unwrap();
    assert_eq!(mark_in(dir.path()).highest(), u64::MAX);
}

// F4
#[test]
fn f4_a_directory_or_a_link_where_the_file_belongs_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(file_in(dir.path())).unwrap();
    assert!(matches!(
        IssueMark::open_in(dir.path(), AGENT),
        Err(MarkError::Corrupt(..))
    ));
    fs::remove_dir(file_in(dir.path())).unwrap();
    #[cfg(unix)]
    {
        let real = dir.path().join("real.json");
        fs::write(&real, json(7)).unwrap();
        std::os::unix::fs::symlink(&real, file_in(dir.path())).unwrap();
        assert!(matches!(
            IssueMark::open_in(dir.path(), AGENT),
            Err(MarkError::Corrupt(..))
        ));
    }
}

// F5
#[test]
fn f5_a_directory_or_a_file_that_cannot_be_read_is_an_io_error_not_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let not_a_dir = dir.path().join("plain");
    fs::write(&not_a_dir, b"x").unwrap();
    assert!(matches!(
        IssueMark::open_in(&not_a_dir, AGENT),
        Err(MarkError::Io(_))
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = file_in(dir.path());
        fs::write(&path, json(7)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // An account that can read it anyway (root) has nothing to refuse.
        if fs::File::open(&path).is_err() {
            assert!(matches!(
                IssueMark::open_in(dir.path(), AGENT),
                Err(MarkError::Io(io::ErrorKind::PermissionDenied))
            ));
        }
    }
}

// The agent ID names the file: one that cannot is refused before anything is touched.
#[test]
fn an_agent_id_that_is_not_a_canonical_uuid_cannot_name_the_file() {
    let dir = tempfile::tempdir().unwrap();
    for id in ["", "../x", "ABCDEF00-0000-0000-0000-000000000000", "agent"] {
        assert!(matches!(
            IssueMark::open_in(dir.path(), id),
            Err(MarkError::BadAgentId)
        ));
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

// F6
#[cfg(unix)]
#[test]
fn f6_a_wider_mode_is_not_checked_and_the_next_write_leaves_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path());
    fs::write(&path, json(10)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let marks = mark_in(dir.path());
    assert_eq!(marks.highest(), 10);
    marks.raise(11);
    marks.sync().unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

// F7
#[test]
fn f7_a_stale_temp_file_is_removed_by_the_next_write_and_one_that_cannot_be_is_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().join(format!("{AGENT}{FILE_SUFFIX}.tmp"));
    fs::write(&tmp, b"stale").unwrap();
    let marks = mark_in(dir.path());
    marks.raise(3);
    marks.sync().unwrap();
    assert!(!tmp.exists());
    assert_eq!(read(&file_in(dir.path()), AGENT).unwrap(), 3);
    // A non-empty directory in its place cannot be removed.
    fs::create_dir(&tmp).unwrap();
    fs::write(tmp.join("x"), b"x").unwrap();
    marks.raise(4);
    assert!(marks.sync().is_err());
    assert!(marks.owes_write());
}

// F8, F9, F13
#[test]
fn f8_f9_f13_a_failure_at_any_step_leaves_the_known_value_and_the_next_sync_writes_again() {
    for at in [
        Step::BeforeWrite,
        Step::AfterWrite,
        Step::AfterSync,
        Step::BeforeRename,
        Step::AfterRename,
    ] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(file_in(dir.path()), json(10)).unwrap();
        // Armed after the open, whose own directory sync passes the same step.
        let armed = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(AtomicUsize::new(0));
        let marks =
            IssueMark::open_with(dir.path(), AGENT, failing_at(at, &armed, &writes)).unwrap();
        armed.store(true, Ordering::SeqCst);
        marks.raise(20);
        assert!(marks.sync().is_err(), "{at:?}");
        assert!(marks.owes_write(), "{at:?}: the known value is unchanged");
        assert_eq!(marks.highest(), 20, "{at:?}: the mark stays raised");
        let tmp = dir.path().join(format!("{AGENT}{FILE_SUFFIX}.tmp"));
        assert!(!tmp.exists(), "{at:?}: no temp file is left");
        let in_file = read(&file_in(dir.path()), AGENT).unwrap();
        // Before the rename the old value stands (F13); after it the new file is in place (F9).
        assert_eq!(
            in_file,
            if at == Step::AfterRename { 20 } else { 10 },
            "{at:?}"
        );
        armed.store(false, Ordering::SeqCst);
        marks.sync().unwrap();
        assert!(!marks.owes_write());
        assert_eq!(read(&file_in(dir.path()), AGENT).unwrap(), 20, "{at:?}");
        assert_eq!(writes.load(Ordering::SeqCst), 2, "{at:?}: written again");
    }
}

// F8: a rename that fails (a directory where the file belongs, planted after the open).
#[test]
fn f8_a_failed_rename_keeps_the_old_state_and_no_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let marks = mark_in(dir.path());
    fs::create_dir(file_in(dir.path())).unwrap();
    marks.raise(9);
    assert!(matches!(marks.sync(), Err(MarkError::Io(_))));
    assert!(marks.owes_write());
    assert!(
        !dir.path()
            .join(format!("{AGENT}{FILE_SUFFIX}.tmp"))
            .exists()
    );
    assert!(file_in(dir.path()).is_dir());
}

// F14
#[test]
fn f14_another_agent_id_has_its_own_file_and_starts_at_zero_and_the_old_file_stays() {
    let dir = tempfile::tempdir().unwrap();
    {
        let marks = mark_in(dir.path());
        marks.raise(8_000);
        marks.sync().unwrap();
    }
    let other = IssueMark::open_in(dir.path(), OTHER_AGENT).unwrap();
    assert_eq!(other.highest(), 0);
    other.raise(5);
    other.sync().unwrap();
    assert_eq!(read(&file_in(dir.path()), AGENT).unwrap(), 8_000);
    assert_eq!(
        read(&path_of(dir.path(), OTHER_AGENT), OTHER_AGENT).unwrap(),
        5
    );
}

// F16
#[test]
fn f16_a_second_store_is_in_use_until_the_first_and_every_clone_of_it_are_gone() {
    let dir = tempfile::tempdir().unwrap();
    let first = mark_in(dir.path());
    assert!(matches!(
        IssueMark::open_in(dir.path(), AGENT),
        Err(MarkError::InUse)
    ));
    // A write on a blocking task holds a clone: the lock outlives the owner's handle.
    let held = first.clone();
    drop(first);
    assert!(matches!(
        IssueMark::open_in(dir.path(), AGENT),
        Err(MarkError::InUse)
    ));
    drop(held);
    IssueMark::open_in(dir.path(), AGENT).unwrap();
}

// Codex review: a file read at open counts as synced only once its directory entry is durable.
#[test]
fn an_open_makes_a_present_file_durable_and_fails_when_that_sync_fails() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(file_in(dir.path()), json(1_500)).unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let probe = |armed: bool, seen: &Arc<AtomicUsize>| -> Probe {
        let seen = seen.clone();
        Box::new(move |step| {
            if step == Step::AfterRename {
                seen.fetch_add(1, Ordering::SeqCst);
                if armed {
                    return Err(io::Error::other("scripted"));
                }
            }
            Ok(())
        })
    };
    let mark = IssueMark::open_with(dir.path(), AGENT, probe(false, &seen)).unwrap();
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the open syncs the file's directory entry"
    );
    assert_eq!(mark.highest(), 1_500);
    drop(mark);
    let failed = IssueMark::open_with(dir.path(), AGENT, probe(true, &seen));
    assert!(
        matches!(failed, Err(MarkError::Io(_))),
        "{:?}",
        failed.err()
    );
    // An absent file has nothing to make durable.
    let empty = tempfile::tempdir().unwrap();
    let before = seen.load(Ordering::SeqCst);
    drop(IssueMark::open_with(empty.path(), AGENT, probe(true, &seen)).unwrap());
    assert_eq!(seen.load(Ordering::SeqCst), before);
}
