//! Restart guards (design 8.8, 8.8.1; ADR-229 item 2 step 1, ADR-256).
//!
//! On one device, every job holds the lock of the VCI it uses (per-VCI lock), and a job that
//! reprograms also holds:
//! - the device's single reprogramming slot, since only one ECU is reprogrammed at a time per
//!   device. A job that only reads ([`JobGuards::take_vci_only`]) does not take it.
//!
//! The per-vehicle lock of design 8.8's two-stage locking is not here: its lock would have to
//! be named after the vehicle without keeping its VIN on the device (ADR-256 item 6).
//!
//! Each is an exclusive OS lock (`File::try_lock`) on its own file in a lock directory the
//! caller names. The OS releases a lock with its file handle, also when the process dies, so a
//! crashed agent never blocks the restart that follows it. The files are never deleted: a
//! process that locked a recreated file would not exclude one still holding the old one.
//!
//! Locks are always taken in the same order (VCI, then slot when wanted), so two jobs that wait on each
//! other cannot both hold what the other needs. Waiting polls `try_lock` and stops when the job
//! is cancelled.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Where the guards of a job live and which VCI it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardSetup {
    /// The device's lock directory, shared by every job on the device.
    pub dir: PathBuf,
    /// The VCI's name, as the device names it (for example the name `ngr-agent run --vci`
    /// takes), of 1 to [`MAX_VCI_NAME`] bytes. Two jobs on one VCI must give the same name.
    pub vci: String,
}

/// The longest VCI name a lock file can carry: hex-encoded, with its prefix and extension, it
/// stays within the 255-byte file name limit common file systems have.
pub const MAX_VCI_NAME: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("the job was cancelled while it waited for its guards")]
    Cancelled,
    #[error("a guard's lock file failed: {0}")]
    Io(#[from] io::Error),
    #[error("the VCI name {0:?} cannot name a lock: it must have 1 to {MAX_VCI_NAME} bytes")]
    InvalidVci(String),
}

/// The guards one job holds: the per-VCI lock, and for a job that reprograms also the
/// reprogramming slot. Dropping it releases them. It is not `Clone`, so one set of guards serves
/// one run at a time.
#[derive(Debug)]
pub struct JobGuards {
    _vci: LockFile,
    _slot: Option<LockFile>,
}

impl JobGuards {
    /// Takes the per-VCI lock of `setup.vci`, then the reprogramming slot, waiting while
    /// another job holds either and checking `cancelled` every `poll`. Creates the lock
    /// directory if needed.
    pub fn take(
        setup: &GuardSetup,
        poll: Duration,
        cancelled: &AtomicBool,
    ) -> Result<Self, GuardError> {
        let vci = Self::take_vci(setup, poll, cancelled)?;
        let slot = LockFile::wait(&setup.dir.join("reprogramming.lock"), poll, cancelled)?;
        Ok(Self {
            _vci: vci,
            _slot: Some(slot),
        })
    }

    /// Takes only the per-VCI lock of `setup.vci`, for a job that does not reprogram. Waits and
    /// validates like [`JobGuards::take`].
    pub fn take_vci_only(
        setup: &GuardSetup,
        poll: Duration,
        cancelled: &AtomicBool,
    ) -> Result<Self, GuardError> {
        Ok(Self {
            _vci: Self::take_vci(setup, poll, cancelled)?,
            _slot: None,
        })
    }

    /// Whether these guards hold the device's reprogramming slot.
    pub fn holds_slot(&self) -> bool {
        self._slot.is_some()
    }

    fn take_vci(
        setup: &GuardSetup,
        poll: Duration,
        cancelled: &AtomicBool,
    ) -> Result<LockFile, GuardError> {
        if setup.vci.is_empty() || setup.vci.len() > MAX_VCI_NAME {
            return Err(GuardError::InvalidVci(setup.vci.clone()));
        }
        fs::create_dir_all(&setup.dir)?;
        LockFile::wait(&vci_path(&setup.dir, &setup.vci), poll, cancelled)
    }
}

/// A file whose exclusive OS lock this process holds while the value lives.
#[derive(Debug)]
struct LockFile(#[expect(dead_code, reason = "held only for its lock")] File);

impl LockFile {
    /// Locks `path`, waiting `poll` (at least 1 ms) between tries while another handle holds it.
    fn wait(path: &Path, poll: Duration, cancelled: &AtomicBool) -> Result<Self, GuardError> {
        let poll = poll.max(Duration::from_millis(1));
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(GuardError::Cancelled);
            }
            match file.try_lock() {
                Ok(()) => return Ok(Self(file)),
                Err(fs::TryLockError::WouldBlock) => std::thread::sleep(poll),
                Err(fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }
}

/// Names are hex-encoded, so any VCI name gives a valid, distinct file name.
fn hex(name: &str) -> String {
    name.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn vci_path(dir: &Path, vci: &str) -> PathBuf {
    dir.join(format!("vci-{}.lock", hex(vci)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;

    const POLL: Duration = Duration::from_millis(5);

    fn dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ngr-guards-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn setup(dir: &Path, vci: &str) -> GuardSetup {
        GuardSetup {
            dir: dir.to_owned(),
            vci: vci.to_owned(),
        }
    }

    /// Takes the guards on another thread and reports how long it waited.
    fn take_on_thread(
        setup: GuardSetup,
        cancelled: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<(Result<JobGuards, GuardError>, Duration)> {
        let start = Instant::now();
        std::thread::spawn(move || {
            let taken = JobGuards::take(&setup, POLL, &cancelled);
            (taken, start.elapsed())
        })
    }

    #[test]
    fn a_job_waits_for_the_guards_another_job_holds() {
        let dir = dir("wait");
        let held = JobGuards::take(&setup(&dir, "VCI-1"), POLL, &AtomicBool::new(false))
            .expect("first job");
        let waiter = take_on_thread(setup(&dir, "VCI-1"), Arc::new(AtomicBool::new(false)));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "the second job must wait");
        drop(held);
        let (taken, waited) = waiter.join().unwrap();
        taken.expect("taken once the first job let go");
        assert!(waited >= Duration::from_millis(100));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The slot is per device: a job on another VCI waits for it too.
    #[test]
    fn the_reprogramming_slot_is_one_per_device() {
        let dir = dir("slot");
        let _held = JobGuards::take(&setup(&dir, "VCI-1"), POLL, &AtomicBool::new(false))
            .expect("first job");
        let cancelled = Arc::new(AtomicBool::new(false));
        let waiter = take_on_thread(setup(&dir, "VCI-2"), Arc::clone(&cancelled));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "a second VCI waits for the slot");
        cancelled.store(true, Ordering::Relaxed);
        let (taken, _) = waiter.join().unwrap();
        assert!(matches!(taken, Err(GuardError::Cancelled)), "{taken:?}");
        // The cancelled job let go of VCI-2's lock it had taken.
        let vci2 = File::options()
            .write(true)
            .open(vci_path(&dir, "VCI-2"))
            .unwrap();
        vci2.try_lock().expect("VCI-2's lock is free again");
        drop((vci2, _held));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The VCI-only guards hold the VCI lock: a second job on the VCI waits for them.
    #[test]
    fn vci_only_guards_hold_the_vci_lock() {
        let dir = dir("vci-only-wait");
        let held = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &AtomicBool::new(false))
            .expect("first job");
        let cancelled = Arc::new(AtomicBool::new(false));
        let other = setup(&dir, "VCI-1");
        let waiter = {
            let cancelled = Arc::clone(&cancelled);
            std::thread::spawn(move || JobGuards::take_vci_only(&other, POLL, &cancelled))
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "the second job must wait");
        cancelled.store(true, Ordering::Relaxed);
        let taken = waiter.join().unwrap();
        assert!(matches!(taken, Err(GuardError::Cancelled)), "{taken:?}");

        let waiter = {
            let other = setup(&dir, "VCI-1");
            std::thread::spawn(move || {
                JobGuards::take_vci_only(&other, POLL, &AtomicBool::new(false))
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "still waiting");
        drop(held);
        waiter.join().unwrap().expect("taken once released");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// VCI-only guards leave the reprogramming slot free for a job on another VCI.
    #[test]
    fn vci_only_guards_leave_the_slot_free() {
        let dir = dir("vci-only-slot");
        let _held = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &AtomicBool::new(false))
            .expect("read job");
        let writer = JobGuards::take(&setup(&dir, "VCI-2"), POLL, &AtomicBool::new(false))
            .expect("the slot is free");
        assert!(writer.holds_slot());
        drop((writer, _held));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_full_take_holds_the_slot() {
        let dir = dir("holds-slot");
        let never = AtomicBool::new(false);
        let full = JobGuards::take(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        assert!(full.holds_slot());
        drop(full);
        let vci_only = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        assert!(!vci_only.holds_slot());
        assert!(matches!(
            JobGuards::take_vci_only(&setup(&dir, ""), POLL, &never),
            Err(GuardError::InvalidVci(_))
        ));
        drop(vci_only);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A cancelled job takes nothing, and leaves the guards free for the next one.
    #[test]
    fn a_cancelled_job_takes_no_guard() {
        let dir = dir("cancel");
        let taken = JobGuards::take(&setup(&dir, "VCI-1"), POLL, &AtomicBool::new(true));
        assert!(matches!(taken, Err(GuardError::Cancelled)), "{taken:?}");
        JobGuards::take(&setup(&dir, "VCI-1"), POLL, &AtomicBool::new(false))
            .expect("nothing is held");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A VCI name too long for a lock file is refused before any file is touched.
    #[test]
    fn a_vci_name_must_fit_a_file_name() {
        let dir = dir("vci-name");
        let longest = "V".repeat(MAX_VCI_NAME);
        let guards = JobGuards::take(&setup(&dir, &longest), POLL, &AtomicBool::new(false))
            .expect("the longest name fits");
        drop(guards);
        for name in [String::new(), "V".repeat(MAX_VCI_NAME + 1)] {
            assert!(matches!(
                JobGuards::take(&setup(&dir, &name), POLL, &AtomicBool::new(false)),
                Err(GuardError::InvalidVci(_))
            ));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_are_distinct_file_names() {
        let dir = Path::new("locks");
        assert_ne!(vci_path(dir, "a/b"), vci_path(dir, "a_b"));
        assert_eq!(vci_path(dir, "A"), dir.join("vci-41.lock"));
        assert!(!vci_path(dir, "../x").to_string_lossy().contains(".."));
    }
}
