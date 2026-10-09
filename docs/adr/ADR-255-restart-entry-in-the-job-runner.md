# ADR-255: The Restart Entry in the Job Runner

**Date:** 2026-10-09
**Status:** Accepted (item 5's "locks not taken" and the duplicate-resume consequence superseded by ADR-256)
**Affects:** `agent` (`src/runner.rs`, `src/restart.rs`, `src/journaling.rs`, `src/journal.rs`, `src/lib.rs`), `Cargo.toml` (`rust-version`), ADR-253 item 4, ADR-244 (the no-lock consequence)

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

ADR-244 left the journal without a lock and made one writer per job and generation the job
scheduler's duty, and no scheduler exists. While only `Journal::create` opened a journal for
writing, a second writer could not arise, since a second create of the same key fails. A resume
opens an existing journal, so two resumes of one job could both write it. The file store appends
at the length it remembers, so two writers overwrite each other's frames. Two resume records of
the same length would then collapse into one, undercounting the resume limit, and records of
different lengths would leave a corrupt tail.

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
   no transfer has started. A program without a flash recovery plan keeps no journal (ADR-252
   item 7): it runs without one when it has none, or when the one it has records no transfer.
   A journal that does not read back ends in on-site intervention whatever the program, as
   `classify` decides.
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
   when the supply is fixed. A cancel stops step 1 before the voltage read, right after it
   (whatever the read gave, so a cancel is never reported as a voltage failure) and before the
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
7. **The journal has one writer, held by an OS lock.** `Journal::create` and `Journal::open` take
   an exclusive OS lock (`File::try_lock`) on a sidecar `{job_id}.g{generation}.journal.lock`
   before they touch the journal. `open` checks that the journal exists first, so a missing one
   leaves no sidecar. The lock is held until the `Journal` is dropped or the process ends.
   - **Why a sidecar.** Locks on Windows are mandatory, so locking the journal itself would
     shut out `Journal::read`, the reader that takes no lock.
   - **Never deleted.** A process that locked a recreated sidecar would not exclude one still
     holding the old file.
   - **Contention.** A second writer gets `JournalError::InUse` without reading or truncating
     anything. `resume_program_journaled` ends with `JobError::Journal(InUse)` before it
     classifies. Contention means another run is handling the job, so it is not an on-site
     verdict.
   - **Try, not wait.** The lock is tried, not waited for: a duplicate of the same job must not
     queue up and run the restart a second time. ADR-229's waiting applies to guards that
     another job holds.
   - **MSRV.** The workspace's minimum Rust version becomes 1.89, which has `File::try_lock`.

## Consequences

- An agent without a server recovers on its journal count alone. Repeated crashes during
  recovery on one device stop at the plan's resume limit. No attempt key is made, so the
  server-side reservation of ADR-229 does not apply to such a run.
- A program that declares a voltage range cannot be restarted on a VCI that reports no supply
  voltage: J2534 workers without `READ_VBATT` read it as "cannot be established". This errs on
  the side of on-site intervention.
- A resumed run's step count continues from the journal's last record, so
  `JobLimits::max_steps` also counts the earlier runs' steps up to that record (ADR-253 item 3).
- A plain start writes no VM state when it starts at a plan's entry (ADR-252 item 6). For a plan
  whose entry is instruction 0, the newest state in the journal can then be one an earlier run
  took on a jump back to the entry, with that run's locals and stack. `classify` would hand that
  state to a later restart as its entry state. A restart that passes step 1 sends nothing
  (item 5), so nothing uses it yet; the replay to the erase must not start from it.
- `ngr-agent run` keeps no journal and has no resume. Only library callers reach
  `resume_program_journaled`.
- The OS releases the journal's lock when a run crashes, so a restart never waits on a dead run.
  Sidecar `.lock` files stay in the journal directory.
- A duplicate resume refused by the journal lock has already opened and closed the link to its
  VCI, because the journal is opened after the link. Keeping two jobs off one VCI is the per-VCI
  lock's work (design 8.8).
- A journal directory on a file system without OS file locks (some network file systems) fails
  every create and open. The journal belongs on the device (design 5.5).
