//! The bodies of the hardware checks, as functions over guarded handles and the profile, so the
//! parts that need no hardware run in CI against a mock (design 13.5). The `hw_` tests that run
//! them against a device live next to the worker binaries they launch.

pub mod t0_loopback;
pub mod t2_t3_read;
