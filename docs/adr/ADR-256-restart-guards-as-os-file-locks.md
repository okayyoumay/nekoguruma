# ADR-256: Restart Guards as OS Locks on Files in a Device Lock Directory

**Date:** 2026-10-09
**Status:** Accepted (the first-run consequence and item 1's write access to lock files superseded by ADR-257)
**Affects:** `agent` (`src/guards.rs`, `src/runner.rs`, `src/lib.rs`, `src/restart.rs`), ADR-255 item 5 and its duplicate-resume consequence

## Context

Design 8.8 asks for OS-level exclusive locks on one device: a per-VCI lock, then a per-vehicle
lock once the VIN is known. Design 8.8.1 adds a limit of one ECU reprogramming at a time per
device. ADR-229 item 2 step 1 orders a restart after an agent crash, or after a loss of the
device's own power, to take the per-VCI lock and the reprogramming slot again before anything
goes through the VCI, and to wait while another job holds them. A job that survived a worker
crash, a VCI disconnect or a loss of the vehicle's supply alone still holds them, and keeps
them. Nothing in `crates/` implemented any of these locks. `resume_program_journaled`
(ADR-255) opened the link without them.

Several things are not settled by the design:
- which OS primitive the locks use, and where they live;
- how the locks are named;
- how a job waits for a lock, and in what order the locks are taken;
- who owns the guards, so that a job that survives a worker crash keeps them.

## Decision

1. **The OS primitive is a file lock in a lock directory.** Each guard is an exclusive OS
   lock (`File::try_lock`, the primitive the journal's writer lock already uses, ADR-255
   item 7) on its own file in a lock directory the caller names, one per device and shared
   by every job on it. The files are:
   - `vci-{hex name}.lock` for each VCI, whose name has 1 to 100 bytes so that the file name
     stays within the 255-byte limit of common file systems (`GuardError::InvalidVci`
     otherwise);
   - `reprogramming.lock` for the slot.
   - **Why a file lock.** The kernel releases the lock with the handle, also when the process
     dies, so a crashed run never blocks its restart. One mechanism serves Linux and Windows
     without platform code. A named mutex (`Global\`) would need Windows-only
     code and has no Linux counterpart.
   - **Hex names.** The VCI's name is hex-encoded into the file name, so any name within the
     length limit gives a valid and distinct file.
   - **Never deleted.** A process that locked a recreated file would not exclude one still
     holding the old one.
   - **One directory per device.** Every agent process on the device, whichever user it runs
     as, must use the same directory and be able to open its files for writing. A per-user
     directory would not exclude another user's agent, and a file another user's agent cannot
     open fails the job (`GuardError::Io`) instead of making it wait.
2. **The caller names the VCI.** `LinkConfig` does not identify the VCI; the worker the job
   talks to does. `GuardSetup { dir, vci }` carries the name the device uses for it, for
   example the one `ngr-agent run --vci` takes. Two jobs on one VCI must give the same name.
3. **Waiting polls, and a cancel stops it.** `JobGuards::take` tries each lock, sleeps for the
   given poll (at least 1 ms, so a zero never spins), and tries again while another job holds
   it. A cancel ends the wait with nothing taken, and the caller gets `GuardError::Cancelled`.
   A blocking lock call could not be cancelled. A standalone agent has no start deadline, so
   the wait has no other end (ADR-255 item 4).
4. **Fixed lock order: VCI, then slot.** A job that takes guards (today, a resumed write job)
   takes them in this order and holds them until it ends. A job therefore never waits for a lock that comes before one it holds,
   and two jobs cannot each hold what the other waits for.
5. **The caller owns the guards; a run borrows them by value.** `resume_program_journaled`
   takes a `JobGuards` and returns it with the run's result. A resume therefore cannot run
   without holding the guards, they are held before anything goes through the VCI, and two
   runs cannot use one set at once: the type is not `Clone`, and a shared handle such as an
   `Arc` would let two overlapping runs pass the same proof.
   - After an agent crash or a loss of the device's power, the new run takes them with
     `JobGuards::take` before it calls the runner, waiting while another job holds them. A
     duplicate resume of the same job therefore waits there, without opening a link. The
     journal's writer lock (ADR-255 item 7) stays as the second line.
   - While the job runs, the guards sit in a slot that the job thread also holds. A caller
     whose future is dropped therefore releases them only after the job thread has ended
     and closed the link. A job that survived a worker crash, a VCI disconnect or a loss of
     the vehicle's supply alone gets the guards back, still held, and passes them to its next
     run without waiting on itself.
   - Taking them inside the runner was rejected: the runner's return would release them, so
     a worker crash would hand the slot to another job while this ECU may still be in its
     programming session.
6. **The per-vehicle lock is left to the restart's identity steps.** Its lock file would be
   named after the vehicle and, like the others, kept on the device. A name that holds the VIN,
   even hex-encoded, would keep VINs on the device outside the retention and deletion rules
   that apply to them (design 5.5, 16.2). The per-vehicle lock therefore comes with restart
   steps 2 and 3, which promote to it at the first VIN match (ADR-229 item 2), together with a
   file name from which the VIN cannot be recovered. It is taken after the slot, keeping the
   order of item 4.

## Consequences

- ADR-255 item 5's statement that the per-VCI lock and the reprogramming slot are not taken,
  and its consequence that a duplicate resume opens the link before the journal lock refuses
  it, no longer hold for `resume_program_journaled`.
- `resume_program_journaled` gains a required parameter, so its callers change. None exist in
  the repository.
- First runs (`run_program`, `run_program_journaled`) still open the link without the
  per-VCI lock. A first run and a resume of another job can therefore share a VCI. A job that
  only reads needs the per-VCI lock without the slot, which `JobGuards::take` does not offer.
- `run_job` holds its handle on the guard slot until after the link is closed, so even a
  dropped future never releases the guards while the link is still being torn down. A close
  that fails is only logged, though: the caller still gets the guards back and may release them
  while the worker holds the link.
- Lock files stay in the lock directory: one per VCI the device has seen, plus the slot.
- A lock directory on a file system without OS file locks fails every job that takes a
  guard.
- The locks only work between jobs on one device that use the same lock directory. That is
  what design 8.8 asks of the same-PC layer: detection on the vehicle and the server's
  soft lock cover the rest.
