# ADR-393: A Run Continues After Its VCI Comes Back

**Date:** 2026-10-30
**Status:** Accepted
**Affects:** `agent` host (`HostError::resend_class`, `WorkerHost::reopen`), runner (`run_vm`,
`JobLimits::vci_return_wait`, `reopen_poll`, `max_link_reopens`), restart (`repeat_decision`),
communication log (`settle_loss`, `vci_connected`, `link_opened`); design 8.2.5, 8.11;
`crates/agent/docs/ngr-agent.md`; ADR-258 item 1; ADR-289 item 4; ADR-294 item 4; ADR-335 item 3; ADR-373
item 3; ADR-392 item 6

## Context

Design 8.2.5 names a lost VCI and a crashed worker as resume origins that keep the VM state:
the job goes on from the diagnostic primitive. The runner ended such a run instead (ADR-392
item 6), and only a later run resumed it from the journal, without the VM state. ADR-335 item 3
already says when a procedure may go on without re-running what came before: everything still
ahead is safe to repeat, the point is an interruptible one, and the vehicle is the one the job
names. The agent does not track the ECU's diagnostic session; a `Safe` primitive is by
definition correct in any session the procedure can leave the ECU in (ADR-335 item 8).

## Decision

1. **One judgement names a lost path.** A failure in which the VCI was lost, the event stream
   ended, the worker did not answer, or the worker's transport failed is a lost path. The repeat
   decision of ADR-392 is still the one that decides what follows.
2. **The run reopens its link and waits for the VCI.** Outside every flash plan, a run whose
   path was lost opens its link again through the worker it has, every `reopen_poll` (1 s),
   until `vci_return_wait` (60 s), taking a cancel while it waits as during a wait, at most
   `max_link_reopens` (2) times per run. A worker that crashed is not restarted yet, so such a
   run ends as before. Before it opens the link again, the run closes the lost one. On this path
   alone, a successful module disconnect confirms the link closed even when the link's own
   disconnect failed because the device was gone: disconnecting the module releases every link
   on it. Every other failed close stays unconfirmed (ADR-258).
3. **It continues only where ADR-335 item 3 would.** At the current instruction, everything
   ahead must be safe to repeat with no flash plan and no recovery-required section ahead, the
   point must be interruptible, and a VIN the job names must read back the same. No diagnostic
   session is entered again. Otherwise the run ends as before, and a failure of a primitive not
   safe to repeat ends as ADR-392 item 3 says, without a reopen.
4. **The failed primitive is decided as a lost answer.** After the reopen, the repeat decision
   takes the failed primitive as one whose answer was lost, or, when the VCI was lost before it
   was sent, as one not sent, which is sent again whatever its idempotency.
5. **A continuation is a resume.** It counts against the plain procedure's resume limit before
   its first request. It starts no new run record and keeps the run's step budget.
6. **The run settles its own loss.** The communication log settles the loss within the run on
   the agent's clock, then records the VCI connected again, with new clock epochs, and the link
   opened again. No loss is left for a later run to settle.
7. **What stays.** A failure inside a flash plan, an uninterruptible section or a recovery-required
   section, and a reopen that does not succeed in time end the run as before. Reopening the
   links of several targets in the order of ADR-325 item 5 and running the ambiguity guard of
   design 8.9.1 again are not done, since neither exists in the agent yet; the reopen is where
   they join.

The maintainer chose this split, these conditions and bounds, no session re-entry, and the rest
of this decision (2026-10-30).

## Consequences

- A read job whose VCI drops for less than a minute outside a flash plan finishes in the same
  run instead of ending and resuming from its last checkpoint.
- A VCI that drops again and again is bounded by the reopen limit and by the plain procedure's
  resume limit.
- A run with no journal re-reads no VIN after a reopen, since it names none.
- A crashed worker still ends the run until the agent restarts workers within a run (backlog).
- Design 8.11 no longer says that the agent does not reconnect within a run.
