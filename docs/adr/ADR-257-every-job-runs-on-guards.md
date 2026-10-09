# ADR-257: Every Job Runs on Guards It Holds

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/guards.rs`, `src/runner.rs`, `src/policy.rs`, `src/main.rs`), `j2534-0404-service` (tests), ADR-256

## Context

Design 8.8 asks every job on a device to hold the per-VCI lock before it uses the VCI. Design
8.8.1 limits ECU reprogramming to one job at a time per device. ADR-256 implemented both locks
(`JobGuards`) but only the resume of an interrupted write job took them. `run_program` and
`run_program_journaled`, the first-run entries, opened the link without either. So a first run
and a resume of another job could share a VCI, and two writing first runs could reprogram at
once. `ngr-agent run` took no lock either.

The guards of a job that only reads cannot include the slot. Otherwise a read on one VCI would
wait for a reprogramming job on another, which design 8.8.1 allows to run side by side. The
runner also needs a way to know which kind of job it runs.

## Decision

1. **Two kinds of guards.**
   - `JobGuards::take` takes the per-VCI lock and then the slot, as ADR-256 item 4 orders.
   - `JobGuards::take_vci_only` takes the per-VCI lock alone.
   - `JobGuards::holds_slot` tells them apart.
2. **A job writes when the read-only policy refuses it.** `policy::writes(program)` is true when
   `policy::check_program` at `Permission::ReadOnly` finds an instruction it would refuse. Such
   a program needs the slot. Instructions that are always refused, such as `SecurityAccess`,
   also count as writes, which errs on the side of the slot.
3. **Every entry point requires guards and gives them back.**
   - All three entry points take a `JobGuards` by value and return it with the result, as
     ADR-256 item 5 does for the resume: `run_program`, `run_program_journaled` and
     `resume_program_journaled`.
   - The caller takes them before it calls, so a job waits for its guards before anything
     goes through the VCI.
   - The runner keeps its handle until after the link is closed, for every job.
   - A writing program run on guards without the slot ends in
     `JobError::NoReprogrammingSlot` before anything opens.
4. **`ngr-agent run` takes the guards itself.** It reads the lock directory from `--locks
   <dir>`, by default a `locks` directory next to the executable, as `--workers` does. It names
   the VCI by its `--vci` argument and takes `take` or `take_vci_only` by `policy::writes`. It
   holds the guards for the run.

## Consequences

- ADR-256's consequence that first runs open the link without the per-VCI lock no longer holds.
- `run_program` and `run_program_journaled` change their signatures; their callers in the
  repository (the CLI and the `j2534-0404-service` tests) take guards.
- The default lock directory next to the executable must be writable by every agent user on
  the device (ADR-256 item 1). An installation where it is not passes `--locks`.
- Tests that run jobs in parallel use separate lock directories, since the slot is per device.
