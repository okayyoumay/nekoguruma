# ADR-287: The Job's Step Limit Applies to Each Run

**Date:** 2026-10-13
**Status:** Accepted
**Affects:** `agent` (`src/runner.rs` `JobLimits::max_steps`, `run_vm` and `JobError::StepLimit`,
`docs/ngr-agent.md`); ADR-253 item 3, ADR-255 and ADR-276 consequences

## Context

`JobLimits::max_steps` stops a job whose procedure never ends. The runner compared it with the
VM's step count (`VmState::steps`). That count does not start again at a restart: ADR-253 item 3
sets a restart's state to one step after the journal's last record, because the journal orders
its records by step count and refuses one that does not come after the last. A plain start on an
existing journal continues the same way. The limit therefore counted every run of the job
together.

Since ADR-276 a restart goes on from the erase, redoing the whole transfer. A redo that started
near the limit could reach it after the erase and end in `JobError::StepLimit`, leaving the ECU
erased; each further restart began nearer the limit, until the resume limit stopped the job.
Nothing about the redo was wrong, only the steps of earlier runs counted against it.

Two readings were open:

- the limit applies per run, with the journal's count kept for ordering only;
- the restart checks before the erase that the plan's remaining steps fit within the limit.

## Decision

1. **Each run has its own budget.** A run is a first run, a plain start on an existing journal,
   or a restart (its replay, the redo from the erase and the continuation after a read-back
   verification together). The runner stops a run once the steps it ran itself reach
   `max_steps`, counted from the step count it started at: the first run's 0, a plain start's or
   a restart's one past the journal's last record. `VmState::steps` keeps counting across runs
   for the journal, and `JobError::StepLimit` still reports that cumulative count.
2. **No check of the remaining steps before the erase.** A program can loop, so the steps a plan
   still needs are not known before it runs; a static bound would refuse valid plans or need a
   new IR attribute, and a guess could still be wrong after the erase. The limit stays what it
   was for a first run: a guard against a procedure that never ends.

## Consequences

- A redone transfer has the same budget as a first run, so the steps of earlier runs can no
  longer stop it after the erase. A redo that needs more steps than the limit allows also fails
  that way on a first run, and the procedure or the limit is then wrong for that job.
- The job's total steps are bounded by the limit times the number of its runs. Restarts are
  bounded by the plan's resume limit (ADR-229); plain starts are not. Under the cumulative count,
  a plain start after a run the limit stopped before the erase stopped at once, sending nothing;
  now it runs the program again up to the limit, its requests before the erase included (a
  SecurityAccess among them counts against the ECU's attempt limit). Nothing in the agent
  resumes a job on its own, so this happens only when a caller starts it again; whether such a
  plain start should be refused or counted is left open.
- The replay before the erase and the redo after it share one budget, since they are one run.
- ADR-253 item 3's last sentence, and the step-limit consequences of ADR-255 and ADR-276,
  no longer hold.
