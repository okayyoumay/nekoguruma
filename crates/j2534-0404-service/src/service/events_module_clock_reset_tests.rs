use serial_test::serial;

use super::*;

/// `reset_module_clock()` rebases `module_timestamp_us()` backward: a
/// reading taken after a reset that follows a nonzero elapsed interval
/// must be smaller than the reading taken just before the reset.
///
/// The first `module_timestamp_us()` call only guarantees the shared
/// clock is initialized -- since this is a process-global `OnceLock`,
/// it may already have been running for a while by the time this test
/// runs alongside others in the same test binary, or it may be
/// initialized by this very call with ~0 elapsed so far. Either way,
/// the *second* reading, taken after the sleep and immediately before
/// the reset, is guaranteed to reflect at least the sleep's elapsed
/// time -- that is the reading compared against the post-reset one.
///
/// `#[serial]` (shared with `init_module_clock_is_idempotent` below):
/// both tests mutate the same process-global `CLOCK_START`, so running
/// them concurrently could have one test's `reset_module_clock()` call
/// land in the middle of the other's before/after reading pair,
/// producing a spurious failure unrelated to either test's own logic.
#[test]
#[serial]
fn reset_module_clock_rebases_timestamp_backward() {
    let _ = module_timestamp_us(); // ensure the shared clock is initialized
    std::thread::sleep(std::time::Duration::from_millis(5));
    let before_reset = module_timestamp_us();
    reset_module_clock();
    let after_reset = module_timestamp_us();
    assert!(
        after_reset < before_reset,
        "expected post-reset timestamp ({after_reset}) to be smaller than \
             pre-reset timestamp ({before_reset})"
    );
}

/// `init_module_clock()` (ADR-120 Amendment 2, Codex review PR #129
/// round 3) must be a no-op after the clock is already initialized --
/// unlike `reset_module_clock()`, a second call must NOT rebase the
/// clock backward. Asserted by reading the clock, calling
/// `init_module_clock()` again, and confirming the reading afterward
/// did not go backward (only `>=`, not asserted strictly increasing,
/// since two reads a few instructions apart can tie at this clock's
/// resolution).
#[test]
#[serial]
fn init_module_clock_is_idempotent() {
    init_module_clock(); // guarantee initialized, whether or not this is the first call anywhere
    std::thread::sleep(std::time::Duration::from_millis(5));
    let before_second_init = module_timestamp_us();
    init_module_clock(); // must be a no-op: the clock must not rebase
    let after_second_init = module_timestamp_us();
    assert!(
        after_second_init >= before_second_init,
        "expected a second init_module_clock() call to leave the clock running \
             forward, not rebase it: before={before_second_init}, after={after_second_init}"
    );
}
