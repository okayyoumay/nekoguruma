# ADR-258: A Failed Link Close Keeps the Job Guards

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/guards.rs`, `src/runner.rs`, `src/link.rs`, `src/main.rs`), `worker-host` (`WorkerProcess::stop`), ADR-256 (the failed-close consequence)

## Context

Every job runs on guards it holds and gets them back with its result (ADR-256 item 5,
ADR-257). The runner closes the job's link at the end of the run, but a close that failed was
only logged. Examples are DisconnectComLogicalLink past its deadline, a worker that died
mid-teardown, or a panic during the close. The caller then got the guards back and released
them while the worker might still hold the logical link or the module connection, so the next
job could take the guards and open a link on a module still connected. A panic after the link
opened but outside the runner's inner `catch_unwind` skipped the close altogether.

`ngr-agent run` stops its worker before it releases the guards. `WorkerProcess::stop` could
not tell a worker that ignored the stop request but was then killed and reaped from one whose
kill failed: both were errors. Design 8.8 asks the same-PC layer to detect conflicts and fail
safe.

## Decision

1. **The guards carry an unconfirmed-link state.** The runner marks the guards when the close
   of the job's link fails or panics. It also marks them when an open fails partway and the
   open's own cleanup of what it had opened fails too, or when the open panics: the worker may
   then still hold the module or the link. A close that succeeds is confirmed, also when it
   runs during an unwind. A run on guards that are already
   marked is refused before anything opens (`JobError::LinkUnconfirmed`). A flag in the
   returned value was rejected: a caller that ignored it would drop the guards, which is the
   bug itself. Keeping the guards inside the runner until the worker restarts was rejected
   too: the runner only has the worker's client, and the worker process is the caller's.
2. **Unconfirmed guards keep their locks when dropped.** Dropping marked guards forgets the
   lock files instead of closing them, so the OS keeps the locks until the agent process exits,
   and logs an error. `JobGuards::worker_gone` clears the state. Calling it is the caller's
   statement that the worker process that held the link has exited. Rust cannot forbid a drop,
   so the fail-safe is at run time: a caller that forgets `worker_gone` sees a VCI lock that is
   never released and an error in the log, not a VCI two jobs share.
3. **A reaped worker process confirms the link closed.** The logical link and the module
   connection are objects of the worker process. Once it is reaped, the OS has closed the vendor
   library's handles. A vendor library that hands the device to a separate device-server
   process can keep it claimed for a while after its client is gone; that is item 6's
   residual. `WorkerProcess::stop` returns `Stopped::Exited` or `Stopped::Killed`.
   It returns an error only when waiting for or killing the child failed, which leaves the
   child's state unknown. The stop request is answered within the worker's request timeout,
   and the grace period starts after it. A failed or timed-out stop request followed by an exit or a kill is
   no longer an error.
4. **`ngr-agent run` stops the worker, then releases the guards.** On `Ok` from `stop` it calls
   `worker_gone` and drops the guards; a killed worker is reported on stderr. When `stop` fails
   and the link was not confirmed closed, the run fails with "the worker may still hold the
   VCI", and the locks stay until the process exits.
5. **One owner closes the link on every path.** Right after the link opens, the runner wraps it
   in a guard that closes it exactly once, on a normal return, on an error and during an unwind.
   A panic after the open, for example while the host is built, therefore still closes the link
   or marks the guards.
6. **Some releases stay residual.** An agent killed without running its exit path has its
   locks released by the OS (ADR-256 item 1) while its orphaned worker tears the link down on
   stdin EOF. The same happens when `ngr-agent run` ends after a failed stop with an
   unconfirmed link (item 4), and when a vendor device-server process outlives the worker
   (item 3). This is accepted:
   - The window is bounded by that teardown.
   - A second open of a device still held usually fails, so the next job detects it
     (`JobError::Link`), as design 8.8 asks of the layers it cannot prevent.
   - Letting the worker inherit the lock descriptors would work on Unix only, since Windows
     ties a lock to the process that took it. It would also split the one mechanism ADR-256
     item 1 chose.
   - Having the worker take a lock of its own would need a shared and exclusive lock protocol,
     and the worker would have to learn the lock directory.

## Consequences

- ADR-256's consequence that a failed close is only logged, and that the caller may release
  the guards while the worker holds the link, no longer holds.
- `WorkerProcess::stop` changes its return type; its callers in the agent CLI and the
  `j2534-0404-service` tests change with it.
- A long-lived caller has to stop the worker and call `worker_gone` after a run whose link was
  not confirmed closed, before the guards serve another run or are released. A job that
  survives a worker crash (ADR-256 item 5) does the same before its next run.
- Ctrl-C ends `ngr-agent run` without its exit path, which makes item 6's residual its common
  case. Handling the signal so that the job is cancelled, the link closed and the worker
  stopped before the process exits is left to later work.
