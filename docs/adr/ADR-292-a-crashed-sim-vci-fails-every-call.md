# ADR-292: A Crashed Simulated VCI Fails Every Call with `ERR_FAILED`

**Date:** 2026-10-14
**Status:** Accepted
**Affects:** `crates/sim-vci` (`crash_vci`, `restart_vci`), `crates/j2534-0404-service/tests/agent_restart_end_to_end.rs`;
ADR-238

## Context

Design 13.4 asks the VCI-side simulator to inject crashes as well as disconnects. ADR-238 added
the disconnect: a pulled cable, after which the device is lost and every call returns
`ERR_DEVICE_NOT_CONNECTED` until the application closes it (J2534-1 6.10.1). It left the crash
out until a test needed behaviour that differs from a disconnect.

A crash is a VCI whose firmware stops working while it is still plugged in. The standard defines
a status for a device that went away, but none for one that is still there and does not answer:
a vendor library then reports a failure it cannot name more precisely. Two simulations were
considered:
- calls succeed but no response ever arrives. To the worker and the agent this looks like an ECU
  that stays silent, which `sim_ecu::Fault::PowerLoss` already simulates;
- calls fail with the generic `ERR_FAILED`. The worker sees an error it cannot attribute to a lost
  device, which no other fault produces.

## Decision

1. **`crash_vci` makes every J2534 call except `PassThruGetLastError` return `ERR_FAILED`**,
   before the call checks its arguments, as a lost device does with its own status. A read that
   waits for a response returns at once, and a write takes no message. `PassThruOpen` fails while
   the VCI is crashed, whether or not a device is open.
2. **`PassThruClose` on the open device still releases it** and reports `ERR_FAILED`: the
   application gives the device up, and the next open, once the VCI answers again, gets a new
   device ID, as after a loss.
3. **`restart_vci` ends the crash.** A device the application still holds open across the
   restart is lost: the restarted VCI has forgotten it, so every call returns
   `ERR_DEVICE_NOT_CONNECTED` until the device is closed, as for a disconnect (J2534-1 6.10.1).
   `restart_vci` on a VCI that did not crash changes nothing.
4. **The ECU behind the VCI keeps its state**, as it does through a disconnect.
5. **The crash belongs to the process that applied it**, like a disconnect. A worker started
   afterwards loads a VCI that answers.

The worker's handling is not changed. Its receive poll treats the failing read as it treats a lost
device and takes the link offline, so a primitive started afterwards finds no connected link. The
end-to-end test records this.

## Consequences

- A test can crash the VCI through the control directory and see the run end on the next
  primitive, with the link not confirmed closed. `a_vci_crash_mid_transfer_is_restarted_once_the_vci_answers_again`
  shows a flash job interrupted this way restarting on a new worker and completing.
- To the agent, a crashed VCI looks like a disconnected one: the next request fails as a call on
  an offline link (`HostError::Rpc` with `FailedPrecondition`). Telling the two apart would need
  the worker to say why the link went offline, as for a VCI lost between two primitives
  (ADR-289 Consequences).
- The choice is the simulator's own and can change if a vendor library is found that reports a
  crash differently; the commands' names stay.
