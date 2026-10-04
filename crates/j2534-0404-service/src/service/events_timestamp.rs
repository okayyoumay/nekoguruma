/// Backing store for [`module_timestamp_us`]/[`reset_module_clock`]: the
/// `Instant` each is measured relative to. `RwLock` rather than `Mutex`
/// because `module_timestamp_us()` (many concurrent readers, on every
/// synthetic timestamp/event) vastly outnumbers `reset_module_clock()`
/// (one writer, only on `PDU_IOCTL_RESET`); both paths are synchronous, so
/// no `.await` is ever held across the lock.
static CLOCK_START: std::sync::OnceLock<std::sync::RwLock<std::time::Instant>> =
    std::sync::OnceLock::new();

/// Microseconds elapsed since this service process started, or since the
/// last [`reset_module_clock`] call, whichever is more recent (ISO 22900-2
/// §9.1.6.1: unit is microseconds, time base resets at boot and within
/// `PDU_IOCTL_RESET`). Monotonic (`Instant`), not wall-clock, so the time
/// base can never step backward. Wraps at 2^32 µs (~71.6 min); §9.1.6.1
/// states that the D-PDU API offers no way to detect a timestamp wrap and
/// leaves handling it to the application -- this is a spec-accepted
/// client-side responsibility, not a bug. Shared by every
/// synthetic timestamp source in this service (status/error events here,
/// `GetTimestamp` in rpc_module.rs, and the synthetic status/error timestamp
/// helper formerly duplicated in rpc_primitive.rs) so they can't
/// independently re-diverge (ADR-120). `PDU_IOCTL_RESET`
/// (`rpc_misc.rs::ioctl_reset`) rebases this clock to (approximately) zero
/// by calling [`reset_module_clock`].
pub(in crate::service) fn module_timestamp_us() -> u32 {
    let start = CLOCK_START.get_or_init(|| std::sync::RwLock::new(std::time::Instant::now()));
    start
        .read()
        .expect("module clock RwLock poisoned")
        .elapsed()
        .as_micros() as u32
}

/// Eagerly captures [`module_timestamp_us`]'s epoch so it is genuinely
/// service-start-relative, not first-use-relative. Must be called once,
/// early in `J2534Service::new` -- without this, `module_timestamp_us()`'s
/// own lazy `CLOCK_START.get_or_init` would otherwise capture the epoch at
/// whatever moment the first synthetic timestamp/event/`GetTimestamp` call
/// happens to land (e.g. a client's first `GetTimestamp` call, possibly
/// long after the service actually started, especially before any
/// `ModuleConnect`), silently narrowing "microseconds since service start"
/// to "microseconds since this was first read" (Codex review, PR #129).
/// Idempotent: a second call is a no-op, since `get_or_init` only runs its
/// closure once.
pub(in crate::service) fn init_module_clock() {
    CLOCK_START.get_or_init(|| std::sync::RwLock::new(std::time::Instant::now()));
}

/// Rebases [`module_timestamp_us`] to (approximately) zero by overwriting
/// the stored start `Instant` with a fresh one. Called from
/// `rpc_misc.rs::ioctl_reset` to satisfy ISO 22900-2 §9.1.6.1's requirement
/// that the time base resets "within the `PDU_IOCTL_RESET` function," the
/// half of that requirement `module_timestamp_us()`'s original
/// process-start-only capture did not implement (ADR-120 amendment).
pub(in crate::service) fn reset_module_clock() {
    let lock = CLOCK_START.get_or_init(|| std::sync::RwLock::new(std::time::Instant::now()));
    *lock.write().expect("module clock RwLock poisoned") = std::time::Instant::now();
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    /// Conformance-audit fix A2-26 (ADR-120) follow-up: `module_timestamp_us`
    /// is documented as monotonic (backed by `Instant`, never wall-clock),
    /// but nothing exercised that contract -- `tests/grpc_mock/lifecycle.rs`'s
    /// `GetTimestamp` coverage only asserts `timestamp > 0`, true by
    /// construction the instant `CLOCK_START` is initialized regardless of
    /// whether the value could ever decrease. Two back-to-back calls in the
    /// same process share one `CLOCK_START` (`OnceLock`), so the second must
    /// never read a smaller elapsed duration than the first.
    ///
    /// `#[serial]` (bare, joining `events_module_clock_reset_tests.rs`'s own
    /// default-group `#[serial]` tests, per that file's own doc comment on
    /// why its two tests share it): without this, `reset_module_clock_
    /// rebases_timestamp_backward` running concurrently could land its own
    /// `reset_module_clock()` call between this test's two reads, rebasing
    /// the same process-global `CLOCK_START` and making `second < first`
    /// despite correct production behavior -- a spurious failure unrelated
    /// to this test's own logic (Codex review finding, PR #88).
    #[test]
    #[serial]
    fn module_timestamp_us_is_monotonic_across_back_to_back_calls() {
        let first = module_timestamp_us();
        let second = module_timestamp_us();
        assert!(
            second >= first,
            "module_timestamp_us must never decrease across back-to-back calls, got {first} \
             then {second}"
        );
    }
}
