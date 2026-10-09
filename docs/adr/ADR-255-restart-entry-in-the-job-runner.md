# ADR-255: The Restart Entry in the Job Runner

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/runner.rs`, `src/restart.rs`, `src/journaling.rs`, `src/lib.rs`), ADR-253 item 4

## Context

ADR-253 decides from a job's journal how the job goes on: a plain start, the restart order of
an interrupted transfer, or on-site intervention. Nothing in the job runner acted on that
decision. `run_program_journaled` creates the journal and refuses one that exists, so a job
that ran before had no way back in.

ADR-229 item 2 step 1 lists what a restart checks before it sends anything to the ECU: the
start deadline, the resume limit of the stage, the interruptibility attribute and the supply
voltage read through the VCI. If the limit allows another resume, the agent then increments
the stage's resume count and commits it to the journal. An agent without a server (the M1
setup) makes no attempt key and reserves nothing. Design 8.2.5 lists a resume limit that has
been reached among the cases where resumption must not occur.

These questions are left open by ADR-229 and ADR-253:
- how a caller tells a first run from a resume;
- how the job result carries on-site intervention;
- which voltage source step 1 reads and which range it is checked against;
- what a failed voltage check leads to;
- what a restart that passes step 1 does while the later steps of the restart order do not exist.

## Decision

1. **A separate entry point resumes a job.** `run_program_journaled` stays the first run: it
   creates the journal and refuses one that exists. `resume_program_journaled` takes the same
   arguments for a job that ran before.
   - Once the link is open and the policy allows the program, and before anything is sent, it
     opens the journal (`Journal::open`) and classifies it (`restart::classify`).
   - The caller knows whether the job ran before; the file system does not. A first run on a
     leftover journal would otherwise be taken for a restart, and a resume whose journal was
     lost would be taken for a first run. With two entry points, a missing journal for a
     program with a plan stays on-site intervention (ADR-253 item 3).
2. **On-site intervention is a job error.** `JobError::OnSiteInterventionRequired` carries
   `restart::OnSiteReason`. The job ends there, like the runner's other outcomes that are not a
   finished procedure. The reason names what stopped it.

   The classification's reasons are joined by three that the restart's own checks give:
   `ResumeLimitReached`, `SupplyVoltage` and `RestartOrderUnavailable`.
3. **A plain start goes on with the existing journal.** The program runs from its start on the
   opened journal, and its step count starts at `restart::next_steps`, so its records come
   after the old ones. The identity is read and committed again, which the journal allows while
   no transfer has started. A program without a flash recovery plan and without a journal runs
   without one, as a first run would (ADR-252 item 7).
4. **Step 1 of an interrupted transfer** (`restart::check_before_ecu`) runs in this order:
   1. **The resume limit.** A stage whose journal count has reached the plan's `max_resumes`
      ends in `ResumeLimitReached`.
   2. **The supply voltage.** When the program declares a voltage range
      (`Preconditions::voltage_mv`), the voltage is read as
      `RuntimeInput::SupplyVoltageMillivolts`, from the VCI. The program's declared sources
      are not used, because step 1 uses no ECU service.
      - A reading outside the declared range, or no reading at all, ends in `SupplyVoltage`.
        No reading covers a VCI that offers none, an undecodable answer and a failure to ask
        the worker.
      - A program that declares no range is not checked here. Step 3's full precondition
        check (ADR-229 item 2) covers whatever the program declares.
   3. **The resume count.** `Journal::commit_resume` increments it, with no attempt key.

   A failed check counts no resume, so a job stopped by low voltage keeps its attempts for
   when the supply is fixed. A cancel stops step 1 before the voltage read and before the
   commit.

   The start deadline arrives with a server's job instruction (design 5.2). A standalone run
   has none, so step 1 checks none. The interruptibility attribute is already part of
   `classify`.
5. **A restart that passes step 1 ends in on-site intervention.** The rest of the restart order
   (ADR-229 item 2 steps 2 to 4: identity, teardown, the ECU state check, the replay to the
   erase) does not run in the agent. The job therefore ends in `RestartOrderUnavailable` once
   its resume is counted, with nothing sent to the ECU.
   - Counting first keeps the limit honest: each crash during a recovery consumes an attempt,
     as ADR-229 requires.
   - The OS-level locks of design 8.8 are taken before step 1's voltage read. They are not
     taken here.
6. **Nothing is sent before the decision.** The worker host exists from the link's opening,
   but no request reaches the ECU before the classification and step 1 have passed. A journal
   that cannot be opened or read sends nothing.

## Consequences

- An agent without a server recovers on its journal count alone. Repeated crashes during
  recovery on one device stop at the plan's resume limit. No attempt key is made, so the
  server-side reservation of ADR-229 does not apply to such a run.
- A program that declares a voltage range cannot be restarted on a VCI that reports no supply
  voltage: J2534 workers without `READ_VBATT` read it as "cannot be established". This errs on
  the side of on-site intervention.
- A restarted job's step count goes on across runs, so `JobLimits::max_steps` bounds all of a
  job's runs together (ADR-253 item 3).
- `ngr-agent run` keeps no journal and has no resume. Only library callers reach
  `resume_program_journaled`.
