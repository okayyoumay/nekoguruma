# ADR-241: Simulated ECU State Kept in a File Across Worker Processes

**Date:** 2026-10-07
**Status:** Accepted
**Affects:** `crates/sim-ecu` (`SimEcu::snapshot`, `SimEcu::restore`, `EcuSnapshot`), `crates/sim-vci` (`NGR_SIM_ECU_STATE`), `crates/j2534-0404-service/tests/sim_vci_restart.rs`

## Context

`sim-vci` is a cdylib that the worker service loads like a vendor J2534 library, and the
simulated ECU (`sim-ecu`) lives inside it. A worker crash or restart therefore also destroyed the
vehicle: the next worker found a fresh ECU with no flash progress. Recovery after a worker crash
(design 5.6, ADR-229) cannot be tested that way, because on a real vehicle the ECU keeps its
state, and even its session until tS3_Server runs out, while the tester side restarts.

Two ways out were open:

1. Run the ECU in its own process, with `sim-vci` forwarding requests to it.
2. Keep the ECU in the process that loads `sim-vci`, and persist its state so the next process
   continues from it.

## Decision

1. **Persist, don't move.** The ECU stays in the loading process. When `NGR_SIM_ECU_STATE`
   names a file, `sim-vci` writes the ECU's complete state there after every change: each
   request the ECU handles, each control command that touches it, and its creation. The first
   process that needs the ECU continues from that file if it exists, and otherwise starts from
   `NGR_SIM_ECU_CONFIG`. A second process would add a lifecycle (who starts it, who stops it,
   how a test waits for it) and a transport, and the path under test, worker to vendor library,
   would no longer be the one a vendor library takes. A file needs neither.
2. **The whole state, not only the flash.** The snapshot holds everything `SimEcu` keeps: the
   configuration as it stands (a fired `drop_at_block` stays cleared), session, security and
   attempt counters, flash phase, the download and the image, software versions, DTCs, armed
   faults and the power-cycle count. A worker crash is not a power cycle, so the ECU must come
   back exactly as it was, including a running session.
3. **Timers as time left, plus wall-clock time between processes.** The ECU's clock ends with
   the process, so the snapshot records each running timer (tS3_Server, the security delay, a
   response still going out) as the time left. The file carries the wall-clock time of the
   write; the next process subtracts the time since then, so a session times out on schedule
   while no process has the library loaded. A wall clock set back counts as no time passed.
4. **Opt-in, one process at a time.** Without the variable nothing is written and the ECU lives
   as long as the process, as before. The file is written to a temporary name and renamed over
   the old one, so a process killed while writing leaves the previous state. Two processes using
   the same file at once is not supported.
5. **A failed write fails the call.** If the state cannot be written, the J2534 call that
   changed the ECU returns `ERR_FAILED`, and a request's response is not delivered, so a test
   cannot go on past a state the next process would not see.
6. **Encoding.** postcard, with a format version; a file of another version, or one that cannot
   be decoded, makes `PassThruOpen` fail with `ERR_FAILED`.

## Consequences

- A test simulates a worker crash by ending the process that loaded `sim-vci` and loading the
  library again with the same state file. `tests/sim_vci_restart.rs` does this with child
  processes and reads DID FD00 and F186 before and after tS3_Server.
- VCI-side state is not persisted: open channels, filters, responses not yet read, the battery
  voltage set by a control command, and the device ID start fresh, as they would when a worker
  reloads a vendor library.
- Every request rewrites the file, image included (up to the 1 MiB flash window). That is
  acceptable for tests; a full-size download with persistence on writes the image once per
  block.
- Timer accuracy across processes depends on the wall clock; a test that relies on it should
  leave a margin.
