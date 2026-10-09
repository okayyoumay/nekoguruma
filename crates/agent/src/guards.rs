//! Job guards (design 8.8, 8.8.1; ADR-229 item 2 step 1, ADR-256, ADR-257, ADR-262).
//!
//! On one device, every job holds the lock of the VCI it uses (per-VCI lock), and a job that
//! reprograms also holds:
//! - the device's single reprogramming slot, since only one ECU is reprogrammed at a time per
//!   device. A job that only reads ([`JobGuards::take_vci_only`]) does not take it.
//!
//! The per-vehicle lock of design 8.8's two-stage locking ([`JobGuards::take_vehicle`], ADR-262) is
//! named without keeping the VIN on the device (ADR-256 item 6): the VIN is hashed into one of 4096
//! fixed buckets (the low 12 bits of the first two bytes of the SHA-256 digest, read big-endian),
//! `vehicle-{k:03x}.lock`, so two VINs may share a bucket and then exclude each other, which is
//! safe. The files are created by a sweep over all 4096, empty, which a `take_vehicle` runs when
//! the directory holds fewer than 4096 files named exactly `vehicle-` and three lower-case hex
//! digits and `.lock`. Whether a sweep runs depends only on the directory's state, never on the
//! VIN, so the listing says nothing about the vehicles seen; no write-ordering guarantee of the
//! file system is relied on, and the Unix directory sync at the end of a sweep is best-effort
//! durability. The directory must therefore be listable by every agent user (ADR-262 item 2). A
//! job holds one vehicle (a well-formed VIN): taking that VIN again succeeds without touching the
//! file, and any other VIN, also one of the same bucket, is refused.
//!
//! Each is an exclusive OS lock (`File::try_lock`) on its own file in a lock directory the
//! caller names. The OS releases a lock with its file handle, also when the process dies, so a
//! crashed agent never blocks the restart that follows it. The files are never deleted: a
//! process that locked a recreated file would not exclude one still holding the old one. A lock
//! file must be a regular file: an entry that is a symlink, FIFO, device or (Windows) reparse
//! point fails the take ([`GuardError::NotAFile`] or the open's error) instead of being followed
//! or blocking the open (ADR-262 item 6).
//!
//! On Unix the lock directory must stop users deleting or replacing each other's lock files: a
//! directory that group or others may write must have the sticky bit, or the guards refuse it
//! (ADR-257). A directory the guards create is writable by its owner only.
//!
//! A job whose link could not be confirmed closed marks its guards ([`JobGuards::link_unconfirmed`],
//! ADR-258): the worker may still hold the VCI, so dropping them would let another job in on a
//! busy device. Marked guards keep their OS locks when dropped, until the process exits or
//! [`JobGuards::worker_gone`] clears the mark.
//!
//! Locks are always taken in the same order (VCI, then slot when wanted, then vehicle), so two jobs
//! that wait on each other cannot both hold what the other needs. Waiting polls `try_lock` and stops when the job
//! is cancelled.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::journal::Vin;

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
    #[error(
        "the lock directory {0} lets other users delete or replace lock files: \
         give it the sticky bit, or make it writable by its owner only"
    )]
    UnsafeDir(PathBuf),
    #[error("the guards already hold the lock of another vehicle: a job serves one vehicle")]
    OtherVehicleHeld,
    #[error("the VIN is not well-formed, so it cannot name a vehicle lock")]
    InvalidVin,
    #[error("the guards' link is unconfirmed closed, so they take no new lock (ADR-258)")]
    LinkUnconfirmed,
    #[error("the lock file {0} is not a regular file")]
    NotAFile(PathBuf),
    /// A vehicle lock file that is not a regular file. It carries no path: the name is the
    /// VIN-derived bucket, which stays out of messages (ADR-262).
    #[error("a vehicle lock file is not a regular file")]
    NotAVehicleFile,
}

/// How many vehicle lock files a lock directory has (ADR-262).
const VEHICLE_BUCKETS: u32 = 0x1000;

/// The guards one job holds: the per-VCI lock, for a job that reprograms also the
/// reprogramming slot, and once [`JobGuards::take_vehicle`] ran the vehicle's lock. Dropping it releases them, unless the link of the job that used them is
/// unconfirmed (see [`JobGuards::link_unconfirmed`]). It is not `Clone`, so one set of guards
/// serves one run at a time.
pub struct JobGuards {
    _vci: LockFile,
    _slot: Option<LockFile>,
    /// The lock directory, for the vehicle lock taken later.
    dir: PathBuf,
    /// The vehicle, its bucket and its lock (ADR-262). `Vin`'s `Debug` is redacted.
    _vehicle: Option<(Vin, u16, LockFile)>,
    link_unconfirmed: bool,
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
            dir: setup.dir.clone(),
            _vehicle: None,
            link_unconfirmed: false,
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
            dir: setup.dir.clone(),
            _vehicle: None,
            link_unconfirmed: false,
        })
    }

    /// Whether these guards hold the device's reprogramming slot.
    pub fn holds_slot(&self) -> bool {
        self._slot.is_some()
    }

    /// Takes the lock of the vehicle `vin` names, last in the lock order (VCI, slot, vehicle),
    /// waiting and cancelling like [`JobGuards::take`]. Allowed with or without the slot. A
    /// cancel returns [`GuardError::Cancelled`] and leaves the locks already held as they were.
    /// A VIN that is not well-formed gives [`GuardError::InvalidVin`], and guards marked
    /// unconfirmed (ADR-258) [`GuardError::LinkUnconfirmed`]. Taking the VIN the guards already
    /// hold succeeds at once, without touching the file (a second handle in this process would
    /// wait for the first for ever); any other VIN, also one of the same bucket, is refused with
    /// [`GuardError::OtherVehicleHeld`], since a job serves one vehicle (ADR-262).
    pub fn take_vehicle(
        &mut self,
        vin: &Vin,
        poll: Duration,
        cancelled: &AtomicBool,
    ) -> Result<(), GuardError> {
        if !vin.is_well_formed() {
            return Err(GuardError::InvalidVin);
        }
        if self.link_unconfirmed {
            return Err(GuardError::LinkUnconfirmed);
        }
        if let Some((held, _, _)) = &self._vehicle {
            return if held == vin {
                Ok(())
            } else {
                Err(GuardError::OtherVehicleHeld)
            };
        }
        // bucket -> wait -> attach
        let bucket = vehicle_bucket(vin);
        let lock = self.wait_vehicle(bucket, poll, cancelled)?;
        self._vehicle = Some((vin.clone(), bucket, lock));
        Ok(())
    }

    fn wait_vehicle(
        &self,
        bucket: u16,
        poll: Duration,
        cancelled: &AtomicBool,
    ) -> Result<LockFile, GuardError> {
        let path = vehicle_path(&self.dir, bucket);
        if cancelled.load(Ordering::Relaxed) {
            return Err(GuardError::Cancelled);
        }
        if count_vehicle_files(&self.dir)? < VEHICLE_BUCKETS as usize {
            create_vehicle_files(&self.dir)?;
        }
        LockFile::wait_existing(&path, poll, cancelled)
    }

    /// Whether these guards hold a vehicle's lock.
    pub fn holds_vehicle(&self) -> bool {
        self._vehicle.is_some()
    }

    /// Whether the job that used these guards could not confirm its link closed (ADR-258): the
    /// worker may still hold the VCI. Such guards are not released when dropped, and no run
    /// takes them, until [`JobGuards::worker_gone`].
    pub fn link_unconfirmed(&self) -> bool {
        self.link_unconfirmed
    }

    /// Records that the link was not confirmed closed. Sticky: only [`JobGuards::worker_gone`]
    /// clears it.
    pub(crate) fn mark_link_unconfirmed(&mut self) {
        self.link_unconfirmed = true;
    }

    /// Clears the unconfirmed-link mark, so dropping the guards releases the locks again. Call it
    /// only once the worker process that held the link has exited, for example after
    /// `WorkerProcess::stop` returned `Ok`. That process no longer holds the link then; it does
    /// not prove the VCI free, since a vendor device-server process can keep the device claimed
    /// a while longer (ADR-258 items 3 and 6), which the next open detects.
    pub fn worker_gone(&mut self) {
        self.link_unconfirmed = false;
    }

    fn take_vci(
        setup: &GuardSetup,
        poll: Duration,
        cancelled: &AtomicBool,
    ) -> Result<LockFile, GuardError> {
        if setup.vci.is_empty() || setup.vci.len() > MAX_VCI_NAME {
            return Err(GuardError::InvalidVci(setup.vci.clone()));
        }
        prepare_dir(&setup.dir)?;
        LockFile::wait(&vci_path(&setup.dir, &setup.vci), poll, cancelled)
    }
}

/// Reports only what is held: the directory, the bucket, the VIN and file paths stay out.
impl std::fmt::Debug for JobGuards {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobGuards")
            .field("vci", &true)
            .field("slot", &self.holds_slot())
            .field("vehicle", &self.holds_vehicle())
            .field("link_unconfirmed", &self.link_unconfirmed)
            .finish()
    }
}

impl Drop for JobGuards {
    fn drop(&mut self) {
        if self.link_unconfirmed {
            // Closing the files would release the locks (ADR-258); the OS releases them when
            // this process exits.
            tracing::error!(
                "dropping job guards whose link was not confirmed closed: the job's locks \
                 stay held until the agent exits"
            );
            self._vci.leak();
            if let Some(slot) = &mut self._slot {
                slot.leak();
            }
            if let Some((_, _, vehicle)) = &mut self._vehicle {
                vehicle.leak();
            }
        }
    }
}

/// A file whose exclusive OS lock this process holds while the value lives.
#[derive(Debug)]
struct LockFile(Option<File>);

impl LockFile {
    /// Locks `path`, waiting `poll` (at least 1 ms) between tries while another handle holds it.
    fn wait(path: &Path, poll: Duration, cancelled: &AtomicBool) -> Result<Self, GuardError> {
        Self::lock(Self::open(path)?, poll, cancelled)
    }

    /// Like [`LockFile::wait`], for a file that must exist: it is never created here.
    fn wait_existing(
        path: &Path,
        poll: Duration,
        cancelled: &AtomicBool,
    ) -> Result<Self, GuardError> {
        let file = open_read_only(path).map_err(|error| match error {
            GuardError::NotAFile(_) => GuardError::NotAVehicleFile,
            other => other,
        })?;
        Self::lock(file, poll, cancelled)
    }

    fn lock(file: File, poll: Duration, cancelled: &AtomicBool) -> Result<Self, GuardError> {
        let poll = poll.max(Duration::from_millis(1));
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(GuardError::Cancelled);
            }
            match file.try_lock() {
                Ok(()) => return Ok(Self(Some(file))),
                Err(fs::TryLockError::WouldBlock) => std::thread::sleep(poll),
                Err(fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }
}

impl LockFile {
    /// Keeps the lock past the value's life: the file is never closed by this process.
    fn leak(&mut self) {
        if let Some(file) = self.0.take() {
            std::mem::forget(file);
        }
    }

    /// Opens the lock file read-only, so a file another user created (with that user's default
    /// permissions) can be locked as long as it can be read: an OS lock needs no write access.
    /// Only a missing file is created, atomically: when another process creates it first, the
    /// read-only open is tried again rather than opening that file for writing.
    fn open(path: &Path) -> Result<File, GuardError> {
        let mut tries = 0;
        loop {
            match open_read_only(path) {
                Err(GuardError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
                opened => return opened,
            }
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)
            {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && tries < 3 => {
                    tries += 1;
                }
                created => return Ok(created?),
            }
        }
    }
}

/// Opens an existing lock file read-only without following a symlink or blocking on a FIFO, and
/// requires a regular file (ADR-262 item 6). `O_NONBLOCK` and `O_NOCTTY` keep a FIFO or a
/// terminal device from stalling or capturing the open; the type check then refuses them.
fn open_read_only(path: &Path) -> Result<File, GuardError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open a reparse point itself, not its target.
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = false;
    if reparse || !metadata.file_type().is_file() {
        return Err(GuardError::NotAFile(path.to_owned()));
    }
    Ok(file)
}

/// Creates the lock directory if it is missing, writable by its owner only, and refuses one in
/// which another user could delete or replace a lock file another job holds: such a file's path
/// would then name a new file, which a second job could lock at the same time. A class that may
/// write the directory must also be able to read it, so every user can count the vehicle files.
#[cfg(unix)]
fn prepare_dir(dir: &Path) -> Result<(), GuardError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)?;
    let mode = fs::metadata(dir)?.permissions().mode();
    let shared = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    // Every agent user must be able to list the directory (the vehicle files are counted), so a
    // class that may write it must also read it.
    let unreadable =
        (mode & 0o020 != 0 && mode & 0o040 == 0) || (mode & 0o002 != 0 && mode & 0o004 == 0);
    if (shared && !sticky) || unreadable {
        return Err(GuardError::UnsafeDir(dir.to_owned()));
    }
    Ok(())
}

/// Creates the lock directory if it is missing. Its ACL is the installation's (ADR-257).
#[cfg(not(unix))]
fn prepare_dir(dir: &Path) -> Result<(), GuardError> {
    fs::create_dir_all(dir)?;
    Ok(())
}

/// Counts the entries of `dir` named exactly like a vehicle lock file: `vehicle-`, three
/// lower-case hex digits, `.lock`. Other names (other case, other length, `vci-*`) are ignored.
fn count_vehicle_files(dir: &Path) -> Result<usize, GuardError> {
    let mut count = 0;
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let is_bucket = name
            .to_str()
            .and_then(|name| name.strip_prefix("vehicle-"))
            .and_then(|rest| rest.strip_suffix(".lock"))
            .is_some_and(|digits| {
                digits.len() == 3
                    && digits
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            });
        count += usize::from(is_bucket);
    }
    Ok(count)
}

/// Creates the empty vehicle lock files, all 4096 in bucket order whichever VIN asked, then
/// syncs the directory (Unix only, best-effort durability; none on Windows). Whether it runs
/// depends only on the directory's state (the count of bucket files), and no ordering guarantee
/// of the file system is relied on (ADR-262 item 2). A file that exists already is left as it is.
fn create_vehicle_files(dir: &Path) -> Result<(), GuardError> {
    for bucket in 0..VEHICLE_BUCKETS {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(vehicle_path(dir, bucket as u16))
        {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// The bucket of a VIN: the low 12 bits of the first two bytes of its SHA-256 digest, read
/// big-endian (a digest starting 84 b1 gives 0x4b1), so the device keeps no VIN (ADR-262).
fn vehicle_bucket(vin: &Vin) -> u16 {
    let digest = Sha256::digest(vin.as_str().as_bytes());
    u16::from_be_bytes([digest[0], digest[1]]) & 0x0fff
}

fn vehicle_path(dir: &Path, bucket: u16) -> PathBuf {
    dir.join(format!("vehicle-{bucket:03x}.lock"))
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

    fn vin(text: &str) -> Vin {
        Vin::new(text.to_owned())
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

    /// Guards marked unconfirmed keep their locks when dropped, until `worker_gone` (ADR-258).
    #[test]
    fn unconfirmed_guards_keep_their_locks_until_the_worker_is_gone() {
        let dir = dir("unconfirmed");
        let never = AtomicBool::new(false);
        let mut guards =
            JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).expect("first job");
        assert!(!guards.link_unconfirmed());
        guards.mark_link_unconfirmed();
        assert!(guards.link_unconfirmed());
        let other = setup(&dir, "VCI-1");
        let waiter = std::thread::spawn(move || {
            JobGuards::take_vci_only(&other, POLL, &AtomicBool::new(false))
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(!waiter.is_finished(), "marked guards still exclude");
        guards.worker_gone();
        assert!(!guards.link_unconfirmed());
        drop(guards);
        let taken = waiter
            .join()
            .unwrap()
            .expect("taken once the worker is gone");
        drop(taken);
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

    /// A directory group or others may write needs the sticky bit; one the guards create is the
    /// owner's alone (ADR-257).
    #[cfg(unix)]
    #[test]
    fn a_shared_lock_directory_needs_the_sticky_bit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = dir("sticky");
        let never = AtomicBool::new(false);
        let take = || JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never);
        drop(take().expect("a directory the guards create is safe"));
        let created = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(created & 0o022, 0, "{created:o}");
        for mode in [0o777, 0o775, 0o757] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                matches!(take(), Err(GuardError::UnsafeDir(_))),
                "{mode:o} is refused"
            );
        }
        for mode in [0o1777, 0o1770, 0o755, 0o700] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
            drop(take().unwrap_or_else(|error| panic!("{mode:o} is safe: {error}")));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Marked guards that are dropped by mistake keep their locks: the files are never closed,
    /// so another handle cannot lock them while this process lives (ADR-258).
    #[test]
    fn dropped_unconfirmed_guards_keep_their_locks() {
        let dir = dir("dropped-unconfirmed");
        let never = AtomicBool::new(false);
        let mut guards = JobGuards::take(&setup(&dir, "VCI-1"), POLL, &never).expect("first job");
        guards
            .take_vehicle(&vin("1HGCM82633A004352"), POLL, &never)
            .unwrap();
        guards.mark_link_unconfirmed();
        drop(guards);
        for path in [
            vci_path(&dir, "VCI-1"),
            dir.join("reprogramming.lock"),
            vehicle_path(&dir, vehicle_bucket(&vin("1HGCM82633A004352"))),
        ] {
            let other = File::open(&path).unwrap();
            assert!(
                matches!(other.try_lock(), Err(fs::TryLockError::WouldBlock)),
                "{} is still locked",
                path.display()
            );
        }
        // The leaked handles stay open until the test process exits, so the directory may not
        // be removable yet (Windows).
        let _ = fs::remove_dir_all(&dir);
    }

    /// A lock file another user created, which this one may only read, still locks (ADR-257).
    #[cfg(unix)]
    #[test]
    fn a_read_only_lock_file_still_locks() {
        use std::os::unix::fs::PermissionsExt;

        let dir = dir("read-only");
        fs::create_dir_all(&dir).unwrap();
        let path = vci_path(&dir, "VCI-1");
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        let held = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &AtomicBool::new(false))
            .expect("a readable lock file locks");
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            LockFile::wait(&path, POLL, &cancelled),
            Err(GuardError::Cancelled)
        ));
        let other = File::open(&path).unwrap();
        assert!(matches!(
            other.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        drop((held, other));
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

    const VIN_A: &str = "1HGCM82633A004352";
    const VIN_B: &str = "WVWZZZ1JZ3W386752";

    /// Names of the files in `dir`, sorted.
    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// The directory looks the same whichever vehicles were seen, and holds no VIN (ADR-262).
    #[test]
    fn the_lock_directory_does_not_depend_on_the_vehicles() {
        let never = AtomicBool::new(false);
        let mut listings = Vec::new();
        for (tag, vehicle) in [("listing-a", VIN_A), ("listing-b", VIN_B)] {
            let dir = dir(tag);
            let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
            guards.take_vehicle(&vin(vehicle), POLL, &never).unwrap();
            drop(guards);
            let names = listing(&dir);
            for name in &names {
                if name.starts_with("vehicle-") {
                    assert_eq!(fs::metadata(dir.join(name)).unwrap().len(), 0, "{name}");
                }
                assert!(!name.contains(vehicle), "{name}");
                assert!(!name.contains(&hex(vehicle)), "{name}");
            }
            assert_eq!(
                names.iter().filter(|n| n.starts_with("vehicle-")).count(),
                VEHICLE_BUCKETS as usize
            );
            listings.push(names);
            fs::remove_dir_all(&dir).unwrap();
        }
        assert_eq!(listings[0], listings[1]);
    }

    /// Taking the held vehicle again returns at once; the cancel from another thread turns a
    /// self-deadlock into a failure instead of a hang.
    #[test]
    fn taking_the_same_vehicle_twice_is_idempotent() {
        let dir = dir("vehicle-twice");
        let never = AtomicBool::new(false);
        let mut guards = JobGuards::take(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        guards.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        assert!(guards.holds_vehicle());
        let cancelled = Arc::new(AtomicBool::new(false));
        let timer = {
            let cancelled = Arc::clone(&cancelled);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                cancelled.store(true, Ordering::Relaxed);
            })
        };
        let again = guards.take_vehicle(&vin(VIN_A), POLL, &cancelled);
        assert!(again.is_ok(), "{again:?}");
        timer.join().unwrap();
        drop(guards);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A job on another VCI waits for the vehicle, and gets it once the first guards drop.
    #[test]
    fn a_job_on_another_vci_waits_for_the_vehicle() {
        let dir = dir("vehicle-wait");
        let never = AtomicBool::new(false);
        let mut first = JobGuards::take(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        first.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        let other = setup(&dir, "VCI-2");
        let waiter = std::thread::spawn(move || {
            let never = AtomicBool::new(false);
            let mut guards = JobGuards::take_vci_only(&other, POLL, &never)?;
            guards.take_vehicle(&vin(VIN_A), POLL, &never)?;
            Ok::<_, GuardError>(guards)
        });
        std::thread::sleep(Duration::from_millis(150));
        assert!(!waiter.is_finished(), "the vehicle is held");
        drop(first);
        let second = waiter.join().unwrap().expect("taken once released");
        assert!(second.holds_vehicle());
        drop(second);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A cancel while waiting for the vehicle keeps the VCI and the slot, and takes no vehicle.
    #[test]
    fn a_cancel_while_waiting_for_the_vehicle_keeps_the_other_guards() {
        let dir = dir("vehicle-cancel");
        let never = AtomicBool::new(false);
        let mut first = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        first.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        let mut second = JobGuards::take(&setup(&dir, "VCI-2"), POLL, &never).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let timer = {
            let cancelled = Arc::clone(&cancelled);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                cancelled.store(true, Ordering::Relaxed);
            })
        };
        let result = second.take_vehicle(&vin(VIN_A), POLL, &cancelled);
        assert!(matches!(result, Err(GuardError::Cancelled)), "{result:?}");
        timer.join().unwrap();
        assert!(!second.holds_vehicle());
        assert!(second.holds_slot());
        for path in [vci_path(&dir, "VCI-2"), dir.join("reprogramming.lock")] {
            let other = File::open(&path).unwrap();
            assert!(
                matches!(other.try_lock(), Err(fs::TryLockError::WouldBlock)),
                "{} is still held",
                path.display()
            );
        }
        drop(first);
        second
            .take_vehicle(&vin(VIN_A), POLL, &never)
            .expect("free after the first let go");
        drop(second);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A read-only job can hold a vehicle, and a writer on another VCI waits for it.
    #[test]
    fn a_vci_only_job_can_hold_a_vehicle() {
        let dir = dir("vehicle-vci-only");
        let never = AtomicBool::new(false);
        let mut reader = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        reader.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        assert!(reader.holds_vehicle() && !reader.holds_slot());
        let mut writer = JobGuards::take(&setup(&dir, "VCI-2"), POLL, &never).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let timer = {
            let cancelled = Arc::clone(&cancelled);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                cancelled.store(true, Ordering::Relaxed);
            })
        };
        let result = writer.take_vehicle(&vin(VIN_A), POLL, &cancelled);
        assert!(matches!(result, Err(GuardError::Cancelled)), "{result:?}");
        timer.join().unwrap();
        drop(reader);
        writer.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        drop(writer);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// One vehicle per job: a VIN in another bucket is refused, and the held one stays held.
    #[test]
    fn a_job_serves_one_vehicle() {
        assert_ne!(vehicle_bucket(&vin(VIN_A)), vehicle_bucket(&vin(VIN_B)));
        let dir = dir("vehicle-other");
        let never = AtomicBool::new(false);
        let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        guards.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        let refused = guards.take_vehicle(&vin(VIN_B), POLL, &never);
        let Err(error @ GuardError::OtherVehicleHeld) = refused else {
            panic!("{refused:?}");
        };
        assert!(!error.to_string().contains(VIN_B));
        assert!(guards.holds_vehicle());
        guards.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        drop(guards);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The bucket is the low 12 bits of the digest's first two bytes, big-endian; a changed hash or slice fails this.
    #[test]
    fn the_vehicle_bucket_is_fixed() {
        let digest = Sha256::digest(VIN_A.as_bytes());
        let expected = u16::from_be_bytes([digest[0], digest[1]]) & 0x0fff;
        assert_eq!(vehicle_bucket(&vin(VIN_A)), expected);
        assert_eq!(vehicle_bucket(&vin(VIN_A)), 0x4b1);
        assert!(u32::from(vehicle_bucket(&vin(VIN_B))) < VEHICLE_BUCKETS);
        assert_eq!(
            vehicle_path(Path::new("locks"), 0x0a5),
            Path::new("locks").join("vehicle-0a5.lock")
        );
    }

    /// Two VINs of one bucket, found by a small search.
    fn same_bucket_pair() -> (Vin, Vin) {
        let make = |n: u32| vin(&format!("1HGCM826{:09}", n));
        let mut seen = std::collections::HashMap::new();
        for n in 0.. {
            let candidate = make(n);
            if let Some(first) = seen.insert(vehicle_bucket(&candidate), n) {
                return (make(first), candidate);
            }
        }
        unreachable!()
    }

    fn vehicle_files(dir: &Path) -> usize {
        listing(dir)
            .iter()
            .filter(|name| name.starts_with("vehicle-"))
            .count()
    }

    #[test]
    fn a_partial_set_is_completed_before_the_bucket_is_locked() {
        let never = AtomicBool::new(false);
        let bucket = vehicle_bucket(&vin(VIN_A));
        for (tag, present) in [("partial-low", 0..0x800u16), ("partial-fff", 0xfff..0x1000)] {
            let dir = dir(tag);
            fs::create_dir_all(&dir).unwrap();
            for k in present {
                if k != bucket {
                    fs::write(vehicle_path(&dir, k), b"").unwrap();
                }
            }
            let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
            assert!(!vehicle_path(&dir, bucket).exists());
            guards.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
            assert_eq!(vehicle_files(&dir), VEHICLE_BUCKETS as usize);
            drop(guards);
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn take_alone_creates_no_vehicle_files() {
        let dir = dir("no-vehicle-files");
        let never = AtomicBool::new(false);
        drop(JobGuards::take(&setup(&dir, "VCI-1"), POLL, &never).unwrap());
        drop(JobGuards::take_vci_only(&setup(&dir, "VCI-2"), POLL, &never).unwrap());
        assert_eq!(vehicle_files(&dir), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_first_takes_on_a_fresh_directory_both_succeed() {
        let dir = dir("vehicle-race");
        let threads: Vec<_> = [("VCI-1", VIN_A), ("VCI-2", VIN_B)]
            .into_iter()
            .map(|(vci, vehicle)| {
                let setup = setup(&dir, vci);
                std::thread::spawn(move || {
                    let never = AtomicBool::new(false);
                    let mut guards = JobGuards::take_vci_only(&setup, POLL, &never)?;
                    guards.take_vehicle(&vin(vehicle), POLL, &never)?;
                    Ok::<_, GuardError>(guards)
                })
            })
            .collect();
        let guards: Vec<_> = threads
            .into_iter()
            .map(|t| t.join().unwrap().expect("taken"))
            .collect();
        assert_eq!(vehicle_files(&dir), VEHICLE_BUCKETS as usize);
        drop(guards);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_gone_guards_release_the_vehicle_on_drop() {
        let dir = dir("vehicle-worker-gone");
        let never = AtomicBool::new(false);
        let mut first = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        first.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        first.mark_link_unconfirmed();
        first.worker_gone();
        drop(first);
        let mut second = JobGuards::take_vci_only(&setup(&dir, "VCI-2"), POLL, &never).unwrap();
        second.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        drop(second);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_vin_must_be_well_formed() {
        let dir = dir("vehicle-invalid-vin");
        let never = AtomicBool::new(false);
        let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        for bad in ["1hgcm82633a004352", "1HGCM826", "1HGCM82633A00435I", ""] {
            let result = guards.take_vehicle(&vin(bad), POLL, &never);
            let Err(error @ GuardError::InvalidVin) = result else {
                panic!("{bad:?}: {result:?}");
            };
            assert!(!error.to_string().contains(bad) || bad.is_empty());
        }
        assert!(!guards.holds_vehicle());
        assert_eq!(vehicle_files(&dir), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_vin_of_the_same_bucket_is_refused() {
        let (first, second) = same_bucket_pair();
        assert_ne!(first, second);
        assert_eq!(vehicle_bucket(&first), vehicle_bucket(&second));
        let dir = dir("vehicle-same-bucket");
        let never = AtomicBool::new(false);
        let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        guards.take_vehicle(&first, POLL, &never).unwrap();
        assert!(matches!(
            guards.take_vehicle(&second, POLL, &never),
            Err(GuardError::OtherVehicleHeld)
        ));
        let shown = format!("{guards:?}");
        let bucket = format!("{:x}", vehicle_bucket(&first));
        for hidden in [first.as_str(), bucket.as_str(), "vehicle-"] {
            assert!(!shown.contains(hidden), "{hidden}: {shown}");
        }
        drop(guards);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unconfirmed_guards_take_no_vehicle() {
        let dir = dir("vehicle-unconfirmed");
        let never = AtomicBool::new(false);
        let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        guards.mark_link_unconfirmed();
        assert!(matches!(
            guards.take_vehicle(&vin(VIN_A), POLL, &never),
            Err(GuardError::LinkUnconfirmed)
        ));
        assert!(!guards.holds_vehicle());
        guards.worker_gone();
        drop(guards);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_take_creates_no_vehicle_files() {
        let dir = dir("vehicle-cancel-early");
        let never = AtomicBool::new(false);
        let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        let result = guards.take_vehicle(&vin(VIN_A), POLL, &AtomicBool::new(true));
        assert!(matches!(result, Err(GuardError::Cancelled)), "{result:?}");
        assert_eq!(vehicle_files(&dir), 0);
        drop(guards);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hole_in_the_set_is_refilled_by_a_take_for_another_bucket() {
        let dir = dir("vehicle-hole");
        let never = AtomicBool::new(false);
        let mut first = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        first.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        drop(first);
        let hole = (0..VEHICLE_BUCKETS as u16)
            .find(|&k| k != vehicle_bucket(&vin(VIN_A)) && k != vehicle_bucket(&vin(VIN_B)))
            .unwrap();
        fs::remove_file(vehicle_path(&dir, hole)).unwrap();
        let mut second = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        second.take_vehicle(&vin(VIN_B), POLL, &never).unwrap();
        assert_eq!(vehicle_files(&dir), VEHICLE_BUCKETS as usize);
        assert!(vehicle_path(&dir, hole).exists());
        drop(second);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_complete_set_is_left_as_it_is() {
        let dir = dir("vehicle-complete");
        let never = AtomicBool::new(false);
        let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        guards.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        drop(guards);
        let before = listing(&dir);
        let mut again = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        again.take_vehicle(&vin(VIN_B), POLL, &never).unwrap();
        assert_eq!(listing(&dir), before);
        drop(again);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Names that are not exactly `vehicle-` + three lower-case hex digits + `.lock` do not count.
    #[test]
    fn the_count_ignores_names_that_are_not_buckets() {
        let dir = dir("vehicle-decoys");
        let never = AtomicBool::new(false);
        let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        guards.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        drop(guards);
        let hole = 0x123;
        fs::remove_file(vehicle_path(&dir, hole)).unwrap();
        for decoy in ["vehicle-ABC.lock", "vehicle-12.lock", "vehicle-1234.lock"] {
            fs::write(dir.join(decoy), b"").unwrap();
        }
        assert_eq!(
            count_vehicle_files(&dir).unwrap(),
            VEHICLE_BUCKETS as usize - 1
        );
        let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        guards.take_vehicle(&vin(VIN_A), POLL, &never).unwrap();
        assert!(vehicle_path(&dir, hole).exists());
        assert_eq!(count_vehicle_files(&dir).unwrap(), VEHICLE_BUCKETS as usize);
        drop(guards);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_writable_but_unreadable_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let dir = dir("unreadable");
        let never = AtomicBool::new(false);
        drop(JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap());
        for mode in [0o1733, 0o1722, 0o1702, 0o1730] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                matches!(
                    JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never),
                    Err(GuardError::UnsafeDir(_))
                ),
                "{mode:o} is refused"
            );
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_not_followed_as_a_lock_file() {
        let dir = dir("symlink");
        let never = AtomicBool::new(false);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        fs::write(&target, b"").unwrap();
        let bucket = vehicle_bucket(&vin(VIN_A));
        std::os::unix::fs::symlink(&target, vehicle_path(&dir, bucket)).unwrap();
        std::os::unix::fs::symlink(&target, vci_path(&dir, "VCI-1")).unwrap();
        assert!(JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).is_err());
        let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-2"), POLL, &never).unwrap();
        // The sweep leaves the symlink alone and creates the rest; only this bucket fails.
        assert!(guards.take_vehicle(&vin(VIN_A), POLL, &never).is_err());
        assert!(
            fs::symlink_metadata(vehicle_path(&dir, bucket))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(vehicle_files(&dir), VEHICLE_BUCKETS as usize);
        guards.take_vehicle(&vin(VIN_B), POLL, &never).unwrap();
        drop(guards);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_lock_file_fails_the_take_promptly() {
        let dir = dir("fifo");
        let never = AtomicBool::new(false);
        fs::create_dir_all(&dir).unwrap();
        let path = vehicle_path(&dir, vehicle_bucket(&vin(VIN_A)));
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let timer = {
            let cancelled = Arc::clone(&cancelled);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                cancelled.store(true, Ordering::Relaxed);
            })
        };
        let result = guards.take_vehicle(&vin(VIN_A), POLL, &cancelled);
        assert!(
            matches!(result, Err(GuardError::NotAVehicleFile)),
            "{result:?}"
        );
        cancelled.store(true, Ordering::Relaxed);
        timer.join().unwrap();
        drop(guards);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A file symlink at a lock file's path is refused, not followed (ADR-262 item 6). Creating
    /// a symlink needs a privilege (or Developer Mode); when the OS says it is not held (error
    /// 1314), the test reports that and returns, since the runners that enforce it run as admin.
    #[cfg(windows)]
    #[test]
    fn a_windows_symlink_is_not_followed_as_a_lock_file() {
        let dir = dir("win-symlink");
        let never = AtomicBool::new(false);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        fs::write(&target, b"").unwrap();
        let bucket = vehicle_bucket(&vin(VIN_A));
        for link in [vci_path(&dir, "VCI-1"), vehicle_path(&dir, bucket)] {
            match std::os::windows::fs::symlink_file(&target, &link) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(1314) => {
                    eprintln!("skipped: no privilege to create symlinks: {error}");
                    let _ = fs::remove_dir_all(&dir);
                    return;
                }
                Err(error) => panic!("symlink_file failed: {error}"),
            }
        }
        assert!(JobGuards::take_vci_only(&setup(&dir, "VCI-1"), POLL, &never).is_err());
        let mut guards = JobGuards::take_vci_only(&setup(&dir, "VCI-2"), POLL, &never).unwrap();
        assert!(guards.take_vehicle(&vin(VIN_A), POLL, &never).is_err());
        assert!(!guards.holds_vehicle());
        // Neither refused take locked the target the links point at.
        let other = File::open(&target).unwrap();
        other.try_lock().expect("the target was not locked");
        drop((other, guards));
        let _ = fs::remove_dir_all(&dir);
    }
}
