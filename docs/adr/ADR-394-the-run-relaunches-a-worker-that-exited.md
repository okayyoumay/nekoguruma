# ADR-394: The Run Starts a Worker That Exited Again

**Date:** 2026-10-30
**Status:** Accepted
**Affects:** `agent` source of the worker (`WorkerSource::status`, `end`, `pid`;
`launch::RelaunchingWorker`, `LaunchRecipe`), host (`WorkerHost::try_reopen`, `Relink`,
`HostError::WorkerStart`), runner (`run_job`'s end), `main.rs`; `worker-host`
(`WorkerProcess::try_wait`, `kill`); communication log (`worker_exit`, `worker_restart`);
design 8.2.5, 8.11; `crates/agent/docs/ngr-agent.md`; ADR-258 item 3; ADR-392; ADR-393 item 2

## Context

ADR-393 lets a run open its link again through the worker it has, and left a worker that
crashed ending the run: nothing started a new one. A crashed worker reaches the runner as the
same failures a lost VCI does (a call that cannot reach it, an event stream that ended, a
close that gets no answer), which ADR-393 already treats as a lost path. What tells the two
apart is the worker's process, which the agent owns, never the error. A dead worker also
holds no link, so a close made through it fails, and the guards stay marked for a worker that
no longer exists (ADR-258).

## Decision

1. **The process, not the error, tells a crash from a lost VCI.** The run asks its source
   whether the worker's process has exited. A source that does not own the process (a bare
   client) says nothing, and the run behaves as under ADR-393. The repeat decision of ADR-392
   and the conditions of ADR-393 item 3 are unchanged: a crash is a lost path, so it is
   continued only where a lost VCI would be.
2. **A worker that exited is started again.** When the run opens its link again and the
   worker's process has exited, the agent does not close the link through it. The link counts
   as closed and the guards' mark is cleared, since a reaped process holds no link (ADR-258
   item 3, which a reap within the run confirms as well). The source starts a new worker by
   the launch recipe of the first (binary, service kind, VCI, mode, launch and connect
   options), and the link opens on it as in ADR-393.
3. **A worker that is alive and does not answer is ended.** When the close of the lost link
   gets no answer from a worker whose process is alive, the agent ends the process, which is
   reaped, and goes on as in item 2. If it cannot end it, the guards are marked and the run
   ends in the host error as before. A close the worker answers with a failure does not end
   it: ADR-258 and ADR-393 decide that case.
4. **A start that cannot happen ends the run; one that fails may pass.** When the system
   cannot create the process, the attempt cannot succeed and the run ends in the original host
   error. When the process starts but does not become ready, or the agent cannot connect to
   it, the attempt is "not yet": the run polls until `vci_return_wait` as it does for a VCI
   that is away.
5. **The bound is shared.** A start of a worker belongs to a reopen, and the reopen counts
   against `JobLimits::max_link_reopens` with the reopens a lost VCI causes. Several starts
   within one reopen (a worker that is not ready yet is tried again at each poll) count once.
6. **A run that ends reaps a dead worker too.** A run that ends after its path was lost, on
   site or in the host error, asks for the worker's exit before it closes its link. A worker
   that exited is reaped there: the log gets its exit, the final close through it is not made,
   and the guards are not marked. The process stays in a slot the caller shares with the
   source; `ngr-agent run` stops whatever process is in it at the end.
7. **The log tells it in two events.** `worker_exit` names the process ID, how it ended (an
   exit code, a signal, or ended by the agent) and the exchanges that were in flight, and
   `worker_restart` names the new process. A worker's exit makes no `vci_loss` record and
   settles nothing: the exchanges keep the classification they were written with. The new
   connection settles a loss only when a lost VCI preceded the crash in the run (ADR-393
   item 6).

The maintainer chose these rules, the shared bound, the treatment of a failed start and the
reaping at the run's end (2026-10-30).

## Consequences

- A read job whose worker crashes outside a flash plan finishes in the same run when
  everything ahead is safe to repeat, instead of ending and resuming from its last checkpoint.
- A worker that crashes inside a plan, or under a primitive that is not safe to repeat, ends
  the run as before, but its guards are no longer left marked for a process that is gone.
- ADR-393 item 2's "a worker that crashed is not restarted" no longer holds; the rest of the
  item stands.
- A crash and a lost VCI in one run are both counted by `max_link_reopens`, which stays 2.
- A cancel is taken at each poll, so one start of a worker can delay it by up to the launch's
  startup timeout plus the connect timeout.
- A worker's exit is told by its process, which can be reaped a moment after its sockets
  closed. A close that got no answer from a worker still reported alive therefore asks again
  for up to one second before the worker is taken as alive; one that is alive and does not
  answer is then ended, the kill's reap bounded by 5 s.
- The agent starts a new worker only after it found the exit (or ended the worker) itself, so
  every exit is in the log; the first client of a run never starts one.
- A worker the agent cannot tell is alive (a source that cannot say) is closed through, as
  before. A worker that is alive but whose connection fails with a status of its own
  (not a timeout) keeps ADR-258's mark and ends the run.
