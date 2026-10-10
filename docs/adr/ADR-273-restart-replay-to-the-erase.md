# ADR-273: The Restart Replays the Program from the Entry State up to the Erase

**Date:** 2026-10-10
**Status:** Accepted
**Affects:** `agent` (`src/runner.rs`, `src/restart.rs`, `docs/ngr-agent.md`), ADR-229 item 2 step 4, ADR-245 item 4, ADR-272 item 2

## Context

ADR-229 item 2 step 4 has the restart replay the procedure's steps from the recovery entry
boundary to the erase once the full precondition check passed, and ADR-245 item 4 declares which
steps those are: the plan's `entry_pc` is where the replay starts, so steps before it (a routine
that must run once, for example) are not run again. The restart already reaches that point:
steps 1 to 3 and the precondition check of step 4a (`restart::check_reentry`) pass, and
`RestartPoint::entry_state` holds the checked VM state at `entry_pc`. Nothing ran the program
from it yet. ADR-272 item 2 made every run from instruction 0 commit a run start and left open
whether a run that starts from a restored state elsewhere does.

## Decision

1. **After step 4a passed on a redone transfer, the restart runs the program from
   `RestartPoint::entry_state`** (`Vm::resume`) with the journaling of a first run
   (`JobJournal`: `arrive` before each instruction, `completed` after it), and stops when
   execution arrives at the plan's `boundaries.erase_pc`, before `arrive` runs there. The
   transfer-start marker belongs to the arrival at the erase, so none is committed, and nothing
   at or after `erase_pc` is sent. The runner's step loop is one helper shared by this replay
   and the run from instruction 0 (`run_vm`); the run start stays in the latter only.
2. **The replay commits no run start.** Its starting state is already the newest in the journal:
   it is the entry state the restart took. A run start would add a second copy of it, and
   ADR-272 item 2 keeps run starts to runs from instruction 0. A crash during the replay is an
   interrupted transfer again, restarts from the same entry state, and counts one more resume;
   if the replay itself steps back into the entry, the state that step journals belongs to the
   replay's pass, as on a first run.
3. **The replay ends as a first run would**, with the same errors, when it finishes the program,
   waits on something unanswerable, reaches the step limit, fails in the VM or the host, or is
   cancelled. A replay that finishes the program before reaching the erase cannot happen for a
   validated plan; it ends the job in `JournalError::Invariant` rather than being taken for a
   stop.
4. **A replay that reached the erase ends in `OnSiteReason::RestartOrderUnavailable`**, as the
   restart did before, since the second check of the mutable conditions (step 4b-3) and the
   erase (step 4c) do not exist yet. Only ReadDataByIdentifier requests, the teardown's
   ECUReset and the replayed steps reach the ECU. The resume count, the gates, the teardown,
   the confirmation, the identity check and the state check are unchanged.

## Alternatives rejected

- **Replaying from instruction 0.** It would run again the steps before the boundary that
  ADR-245 item 4 excludes.
- **Committing a run start at the entry.** It adds nothing: the starting state is already the
  newest, and ADR-272 item 2 keeps run starts to instruction 0.
- **Stopping after the arrival at the erase.** `arrive` would commit a new transfer-start
  marker before the second check of step 4b-3 and the erase of step 4c, which own both.

## Consequences

- A crash during the replay is again an interrupted transfer of the same pass; the next restart
  takes the same entry state. The replay counts no resume of its own: the restart counted one
  in step 1.
- The old transfer's facts stay in the journal until the new transfer-start marker of step 4c
  clears them.
- Step 4b-3 adds the second check of the mutable conditions at the stop point, and step 4c the
  erase and what follows.
