//! Restart guards (design 8.8, 8.8.1; ADR-229 item 2 step 1, ADR-256).
//!
//! On one device, a job holds:
//! - the lock of the VCI it uses (per-VCI lock);
//! - the device's single reprogramming slot, since only one ECU is reprogrammed at a time per
//!   device;
//! - once a VIN read matches the job's VIN, the lock of that vehicle as well (the promotion of
//!   design 8.8's two-stage locking).
//!
//! Each is an exclusive OS lock (`File::try_lock`) on its own file in a lock directory the
//! caller names. The OS releases a lock with its file handle, also when the process dies, so a
//! crashed agent never blocks the restart that follows it. The files are never deleted: a
//! process that locked a recreated file would not exclude one still holding the old one.
//!
//! Locks are always taken in the same order (VCI, slot, vehicle), so two jobs that wait on each
//! other cannot both hold what the other needs. Waiting polls `try_lock` and stops when the job
//! is cancelled.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Where the guards of a job live and which VCI it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardSetup {
    /// The device's lock directory, shared by every job on the device.
    pub dir: PathBuf,
    /// The VCI's name, as the device names it (for example the name `ngr-agent run --vci`
    /// takes). Two jobs on one VCI must give the same name.
    pub vci: String,
}

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("the job was cancelled while it waited for its guards")]
    Cancelled,
    #[error("a guard's lock file failed: {0}")]
    Io(#[from] io::Error),
    #[error("the VIN {0:?} cannot name a vehicle lock")]
    InvalidVin(String),
    #[error("the job already holds the lock of vehicle {held:?}, not {asked:?}")]
    OtherVehicle { held: String, asked: String },
}

/// The guards one job holds: the per-VCI lock and the reprogramming slot, and the per-vehicle
/// lock once [`JobGuards::promote`] took it. Dropping it releases them all.
#[derive(Debug)]
pub struct JobGuards {
    dir: PathBuf,
    _vci: LockFile,
    _slot: LockFile,
    vehicle: Mutex<Option<(String, LockFile)>>,
    /// Held for a whole promotion, so promotions run one at a time: a second one waits for the
    /// first and then finds its result, rather than waiting on the first one's file lock.
    promoting: Mutex<()>,
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
        fs::create_dir_all(&setup.dir)?;
        let vci = LockFile::wait(&vci_path(&setup.dir, &setup.vci), poll, cancelled)?;
        let slot = LockFile::wait(&setup.dir.join("reprogramming.lock"), poll, cancelled)?;
        Ok(Self {
            dir: setup.dir.clone(),
            _vci: vci,
            _slot: slot,
            vehicle: Mutex::new(None),
            promoting: Mutex::new(()),
        })
    }

    /// Promotes to the per-vehicle lock of `vin` once a VIN read first matched the job's VIN
    /// (design 8.8; ADR-229 item 2 steps 2 and 3), waiting while another job holds it. A second
    /// call with the same VIN does nothing; one with another VIN is refused. Promotions run one
    /// at a time: a call made while another one waits blocks until that one ends, whatever its
    /// own `cancelled` says, and then answers from its result.
    pub fn promote(
        &self,
        vin: &str,
        poll: Duration,
        cancelled: &AtomicBool,
    ) -> Result<(), GuardError> {
        if vin.is_empty() || vin.len() > 64 {
            return Err(GuardError::InvalidVin(vin.to_owned()));
        }
        let _promoting = self.promoting.lock().unwrap_or_else(|e| e.into_inner());
        if self.check_vehicle(vin)? {
            return Ok(());
        }
        // Waited for without the `vehicle` mutex, so `vehicle()` stays answerable meanwhile.
        // Only this call promotes now, so nothing else can set `vehicle` before it does.
        let lock = LockFile::wait(&vehicle_path(&self.dir, vin), poll, cancelled)?;
        *self.vehicle.lock().unwrap_or_else(|e| e.into_inner()) = Some((vin.to_owned(), lock));
        Ok(())
    }

    /// Whether the job already holds `vin`'s lock; an error if it holds another vehicle's.
    fn check_vehicle(&self, vin: &str) -> Result<bool, GuardError> {
        let vehicle = self.vehicle.lock().unwrap_or_else(|e| e.into_inner());
        match &*vehicle {
            Some((held, _)) if held == vin => Ok(true),
            Some((held, _)) => Err(GuardError::OtherVehicle {
                held: held.clone(),
                asked: vin.to_owned(),
            }),
            None => Ok(false),
        }
    }

    /// The VIN whose per-vehicle lock the job holds, if it was promoted.
    pub fn vehicle(&self) -> Option<String> {
        let vehicle = self.vehicle.lock().unwrap_or_else(|e| e.into_inner());
        vehicle.as_ref().map(|(vin, _)| vin.clone())
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

/// Names are hex-encoded, so any VCI name or VIN gives a valid, distinct file name.
fn hex(name: &str) -> String {
    name.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn vci_path(dir: &Path, vci: &str) -> PathBuf {
    dir.join(format!("vci-{}.lock", hex(vci)))
}

fn vehicle_path(dir: &Path, vin: &str) -> PathBuf {
    dir.join(format!("vehicle-{}.lock", hex(vin)))
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

    #[test]
    fn the_per_vehicle_lock_is_taken_once_and_held_until_the_job_ends() {
        let dir = dir("vehicle");
        let guards =
            JobGuards::take(&setup(&dir, "VCI-1"), POLL, &AtomicBool::new(false)).expect("guards");
        assert_eq!(guards.vehicle(), None);
        let vin = "WVWZZZ1JZXW000001";
        guards
            .promote(vin, POLL, &AtomicBool::new(false))
            .expect("promoted");
        guards
            .promote(vin, POLL, &AtomicBool::new(false))
            .expect("again, same VIN");
        assert_eq!(guards.vehicle().as_deref(), Some(vin));
        assert!(matches!(
            guards.promote("WVWZZZ1JZXW000002", POLL, &AtomicBool::new(false)),
            Err(GuardError::OtherVehicle { .. })
        ));
        assert!(matches!(
            guards.promote("", POLL, &AtomicBool::new(false)),
            Err(GuardError::InvalidVin(_))
        ));

        // Another holder of the same vehicle waits until the job lets go.
        let path = vehicle_path(&dir, vin);
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            LockFile::wait(&path, POLL, &cancelled),
            Err(GuardError::Cancelled)
        ));
        let waiter = std::thread::spawn(move || {
            LockFile::wait(&path, POLL, &AtomicBool::new(false)).map(drop)
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished());
        drop(guards);
        waiter.join().unwrap().expect("free once the job ended");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Two promotions of one VIN at once both succeed: the second waits for the first rather
    /// than for the first one's file lock.
    #[test]
    fn concurrent_promotions_of_one_vin_both_succeed() {
        let dir = dir("promote-race");
        let guards = Arc::new(
            JobGuards::take(&setup(&dir, "VCI-1"), POLL, &AtomicBool::new(false)).expect("guards"),
        );
        let vin = "WVWZZZ1JZXW000001";
        let promotions: Vec<_> = (0..4)
            .map(|_| {
                let guards = Arc::clone(&guards);
                std::thread::spawn(move || guards.promote(vin, POLL, &AtomicBool::new(false)))
            })
            .collect();
        for promotion in promotions {
            promotion.join().unwrap().expect("promoted");
        }
        assert_eq!(guards.vehicle().as_deref(), Some(vin));
        drop(guards);
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
