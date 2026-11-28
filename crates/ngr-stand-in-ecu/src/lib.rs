//! Stand-in ECU of the optional hardware checks (design 13.5, target class T1, ADR-275): serves
//! `sim-ecu` to a UDS client over a Linux SocketCAN interface, with `sim-ecu`'s fault injection
//! and the persistent state file of `sim-vci`.
//!
//! The kernel's ISO-TP sockets (`CAN_ISOTP`) do the segmentation and flow control of ISO 15765-2
//! (2024) clauses 9 and 10, so each message the server reads is a whole UDS message and each
//! message it writes goes out as one. One socket takes physical requests (receive on the request
//! ID, send on the response ID), another functional ones (receive on the functional ID; it never
//! sends). Every response goes out on the physical socket: functional addressing is single-frame
//! only (ISO 15765-2 (2024) 8.3.2.4), so a segmented response is a physical transfer and the
//! tester's flow control goes to the request ID. The kernel cannot restrict the functional
//! socket to single frames, so it accepts a segmented functional request. The crate builds on every platform, but only Linux has the sockets.
//! `crates/sim-ecu/docs/simulated-ecu.md` describes how to start it.

pub mod args;
#[cfg(target_os = "linux")]
#[expect(
    unsafe_code,
    reason = "the ISO-TP socket and the stop signals are built on libc socket, bind, read, write, poll and signal calls that std does not offer (ADR-502); the module exists on Linux only"
)]
pub mod isotp;
pub mod serve;
