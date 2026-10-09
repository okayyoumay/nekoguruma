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
   a program needs the slot. A program with a flash recovery plan always writes: a valid plan
   has a RequestDownload and a RequestTransferExit.
3. **Every entry point requires guards and gives them back.**
   - All three entry points take a `JobGuards` by value and return it with the result, as
     ADR-256 item 5 does for the resume: `run_program`, `run_program_journaled` and
     `resume_program_journaled`.
   - The caller takes them before it calls, so a job waits for its guards before anything
     goes through the VCI.
   - The runner keeps its handle until after the link is closed, for every job.
   - A writing program that the build's ceiling allows, run on guards without the slot,
     ends in `JobError::NoReprogrammingSlot` before anything opens. The ceiling check comes
     first, so a release build, which allows no write, refuses such a program as
     `JobError::Refused` instead.
4. **`ngr-agent run` takes the guards itself.**
   - **Where.** It reads the lock directory from `--locks <dir>`, by default a `locks`
     directory next to the executable, as `--workers` does.
   - **Absolute only.** A relative `--locks` is refused. Resolved against each run's working
     directory, it would name a different lock set from each one.
   - **When.** It takes the guards right after the program check, before it resolves the VCI. It names
   the VCI by its `--vci` argument and takes `take` or `take_vci_only` by `policy::writes`. It
   holds the guards for the run.

## Consequences

- ADR-256's consequence that first runs open the link without the per-VCI lock no longer holds.
- `run_program` and `run_program_journaled` change their signatures; their callers in the
  repository (the CLI and the `j2534-0404-service` tests) take guards.
- Lock files are opened read-only, and created only when missing, atomically (`create_new`;
  a file another process created first is opened read-only). This replaces ADR-256 item 1's
  requirement that every agent process open the lock files for writing. An OS lock needs no write
  access, so a file one user created, with that user's default permissions, still excludes and
  is excluded by every other user who can read it.
- The lock directory must let every agent user on the device create files in it, and must
  keep users from deleting or replacing each other's lock files (ADR-256 item 1). On Unix that
  is a directory with the sticky bit, writable by all agent users, like `/tmp`; on Windows an
  ACL that grants create but not delete on others' files. The agent creates a missing
  directory with the process's default permissions, which grant neither. On a device whose
  agents run as several users, the installation therefore prepares the directory and names
  it with `--locks` or places it next to the executable. A user who cannot create or read a
  lock file fails at once instead of waiting.
- A job that only reads can still wait for another VCI's reprogramming, because of the lock
  order of ADR-256 item 4: a writer on VCI-1 takes VCI-1's lock and then waits for the slot,
  which a writer on VCI-2 holds, and a read on VCI-1 then waits for VCI-1's lock. The wait
  ends with that reprogramming. Taking the slot first would make every writer hold the
  device's slot while it waits for its VCI, which costs more.
- The guards exclude jobs by the VCI name their caller gives. A caller that drives
  `link::open` and `WorkerHost` directly, or names a VCI differently, is not excluded.
- Tests that run jobs in parallel use separate lock directories, since the slot is per device.
