//! The machine's boot as the communication log identifies it, so that a later run of the agent
//! in the same boot can measure the time since an earlier run's agent time (design 8.11
//! `vci_loss_settled`, ADR-373): the boot's identifier and a clock that runs from the boot,
//! through suspend. On Linux these are the kernel's boot id and `CLOCK_BOOTTIME`; elsewhere the
//! log has neither, and a gap across runs stays unmeasured.

/// The boot's identifier, when the system gives one.
pub(crate) fn id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The time since the boot in microseconds, suspend included, when the system gives it.
pub(crate) fn now_us() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `clock_gettime` writes one `timespec` through a pointer to a live, writable
        // one, and reads nothing else.
        let read = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) };
        if read != 0 {
            return None;
        }
        let secs = u64::try_from(time.tv_sec).ok()?;
        let nanos = u64::try_from(time.tv_nsec).ok()?;
        secs.checked_mul(1_000_000)?.checked_add(nanos / 1_000)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn linux_gives_a_boot_id_and_a_boot_clock_that_runs() {
        assert!(id().is_some_and(|id| id.len() >= 32), "{:?}", id());
        let first = now_us().expect("CLOCK_BOOTTIME");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = now_us().expect("CLOCK_BOOTTIME");
        assert!(second >= first + 1_000, "{first} {second}");
    }
}
