# ADR-238: sim-vci Control Commands and Device Loss

**Date:** 2026-10-07
**Status:** Accepted
**Affects:** `crates/sim-vci` (control export, control directory, `ERR_DEVICE_NOT_CONNECTED`), `crates/sim-ecu` (`Fault` serialization), `crates/j2534-0404-service/tests/sim_vci_control.rs`

## Context

Design 13.4 asks the VCI-side simulator to inject delays, disconnects, crashes and write
failures. `sim-ecu` already had `SimEcu::inject` and `SimEcu::reconnect`, but only `sim-vci`'s own
unit tests could call them, and the VCI side had no disconnect at all. The interrupted-write
tests of design 5.3 / 5.6 (ADR-229) need both, from tests that drive a worker over gRPC.

`sim-vci` runs inside the worker process that loaded it, not in the test process. A C function
exported from the library can be called only by code in that process, and the worker calls only
the J2534 API, as it would on a vendor library. So a test needs a channel into another process.

## Decision

1. **Commands as JSON.** A control command is a JSON object tagged by `command`:
   `inject_fault` (with a `sim_ecu::Fault` in snake case, such as `"power_loss"` or
   `{"delay_response": {"ms": 500}}`), `reconnect_ecu`, `disconnect_vci` and `connect_vci`.
   Unknown commands and unknown fields are rejected.
2. **Two ways in.** The extra export `NgrSimVciControl(const char *command)` applies one command,
   for a test that loads the library into its own process. For a worker process,
   `NGR_SIM_VCI_CONTROL_DIR` names a directory: at the start of every J2534 call, and every
   20 ms while `PassThruReadMsgs` waits, `sim-vci` applies the `*.json` files there in file-name
   order and deletes each one; a file it cannot read, parse or apply is renamed to
   `*.rejected`. A test writes each file under another name and renames it, so a half-written
   file is never read.
   Files rather than a socket keep the library free of threads of its own (a thread would have
   to stop before the library is unloaded) and need nothing beyond `std`. A command takes effect
   at the next J2534 call, which is what a test needs: the call it makes next through the
   worker is the first to see the fault.
3. **Device loss follows J2534-1 6.10.1.** While the VCI is unplugged, every function except
   `PassThruGetLastError` returns `ERR_DEVICE_NOT_CONNECTED`, and a waiting read ends with it. If
   the device was open when the VCI went away, the device stays lost after the VCI is plugged
   back in: every call keeps returning the error until `PassThruClose` on that device, which
   releases it and still reports the error. The next `PassThruOpen` then returns a new device
   ID. A device that was closed when the VCI went away only fails to open until it is back.
   The ECU behind the VCI keeps its state through all of this, as a vehicle does when the
   tester's cable is pulled.

## Consequences

- A test can arm any `sim_ecu::Fault`, reconnect the ECU and unplug the VCI through the worker;
  `tests/sim_vci_control.rs` in `j2534-0404-service` does each through an agent job.
- The control directory is read on every J2534 call while the variable is set. That costs one
  directory listing per call, which only tests pay.
- A VCI crash (firmware failure, as opposed to a pulled cable) is not simulated; it would need
  its own command once a test needs behaviour that differs from a disconnect.
- The ECU still lives in the worker process, so a worker crash loses it; the control channel does
  not change that.
