# ADR-335: Journal and Restart of a Plain Procedure

**Date:** 2026-10-20
**Status:** Accepted (the Consequences' remedy realized by ADR-336: session control and TesterPresent default to `Safe`; item 3's `CheckState` read as `Unsafe` within a run too, and a request marker withdrawn when its request was not sent, by ADR-392)
**Affects:** `agent` (`src/journaling.rs`, `src/journal.rs`, `src/restart.rs`, `src/runner.rs`,
`docs/ngr-agent.md`), `diag-ir` (`Program::validate`, a reachability check), design 5.6, 8.2.5,
8.10.1, `crates/diag-frontend/docs/starlark-subset.md` 4.6; ADR-244, ADR-252 item 7, ADR-253
item 3, ADR-271 item 4, ADR-272 item 2, ADR-293 items 1 to 3, ADR-325 item 7, ADR-330, ADR-332

## Context

Design 8.2.5 gives a plain procedure, meaning the step ranges outside every flash recovery plan,
a checkpoint per diagnostic primitive and a resume driven by idempotency after an agent crash,
a device restart or a forced shutdown, when the VM state is lost. The agent does not do this.
It journals a flash plan and a `RecoveryRequired` section only (ADR-252, ADR-291, ADR-293;
`restart::keeps_journal`), so a restart runs every plain procedure again from its start and
sends its `Unsafe` primitives a second time.

What a decision has to work with:

- A restart opens a fresh link, but within its session timeout the ECU may still hold the
  session and the unlock the procedure had entered, or it may have fallen back to its default
  session and relocked. A flash restart settles this with ADR-265's teardown and default-session
  confirmation, which waits the session timeout and startup time a flash plan declares; a plain
  procedure declares neither, so a resumed run knows neither state.
- The IR has no state query. A `CheckState` primitive cannot be settled by asking the ECU.
- A lowered section gives no re-entry boundary: ADR-332 item 4 cuts a section function into
  one-instruction pieces, so no range says where a procedure may be entered again.
- The case where the VM state survives (a worker crash, a VCI disconnect) is out of scope here.
  It is the `Idempotency` repeat policy, decided separately.

## Decision

In this decision a diagnostic primitive is *safe to repeat* when at least one section covers it
and every section covering it is `Safe`. A primitive outside every section, or covered by a
`Safe` and a non-`Safe` section at once (possible in hand-built IR, which `Program::validate`
accepts), is not: the readers combine covering sections conservatively (ADR-332 item 5), and
items 1 to 3 read them the same way.

1. **Every journaled job keeps a journal.** `run_program_journaled` and
   `resume_program_journaled` create and open one for every program, not only for one with a
   plan or a `RecoveryRequired` section. `ngr-agent run` still keeps none. A missing journal at
   a resume is a plain start only for a program with no plan, no `RecoveryRequired` section and
   no primitive outside the plans that is not safe to repeat. For every other program it is
   `OnSiteInterventionRequired(MissingJournal)`: a lost journal cannot be told from a crash after
   a write. This generalises ADR-293's missing-journal rule.
2. **Two new append-only records**, with no format version change (the ADR-244 pattern).
   - `Checkpoint { at, vm_state }` is committed after every diagnostic primitive outside every
     plan's range (a plan's range runs from its entry to its post-transfer end). It is skipped
     where the step already writes a boundary state at the same step count.
   - `Request { at }` is committed before every primitive outside the plans that is not safe to
     repeat. The runner takes the cancel check there that intents have
     (ADR-291).

   They fold into new facts only: the latest checkpoint (the newest record that carries a VM
   state, including the run start and the boundary steps), the first request, and the last
   request. The flash-plan facts (last and previous VM state, last intent, interruption point)
   are not touched, so ADR-253, ADR-269, ADR-271 and ADR-274 behave as before. This costs one
   fsync per primitive and two for a non-`Safe` one. Records are not batched.
3. **Classification at a restart.** It applies where a job with no transfer-start marker is a
   plain start today. The `Failed`, `Aborted`, `Stopped` and `RecoveryRequired` checks come
   first and are unchanged, and a started transfer keeps the flash path. Then:
   - a `Request` newer than the newest state record gives on-site intervention,
     `UnconfirmedRequest`. `CheckState` counts as non-`Safe` here: with the VM state lost there
     is no query to ask, and an ADR that adds a state query may amend this;
   - otherwise, if no `Request` was ever recorded, the job makes a plain start on the same
     journal, counted by item 6. A repeat is preferred to a skip whenever it is allowed, because
     the repeat re-establishes the session and the unlock through the procedure's own steps;
   - otherwise the job resumes from the latest checkpoint when all of these hold (the
     continuation runs in whatever session the ECU is in, item 8):
     - (S) every diagnostic primitive reachable from the checkpoint's pc outside every plan is
       safe to repeat and covered by no `RecoveryRequired` section, and no plan entry is
       reachable. Reachability is the forward closure over jumps, calls and returns, and is
       conservative;
     - (I) the checkpoint's pc is an interruptible point in ADR-330's sense;
     - (identified) the job names a target VIN (ADR-261) and a VIN source exists;
   - otherwise the job ends in on-site intervention, `NotResumable` with a cause: unsafe
     primitive ahead, recovery-required section ahead, plan ahead, uninterruptible section, no
     identity.

   A primitive outside every section is not safe to repeat for S, as ADR-271's continuation
   rule reads it. I is exactly the runner's interruptible-point marking of ADR-330: a pc outside every
   section is an interruptible point except a `Ret` (ADR-330 item 2), so a checkpoint standing on
   the return after a `No` section's last primitive is not one, and the job ends on site
   (uninterruptible section) as item 5 requires. This is ADR-271 item 4's all-`Safe`
   continuation rule applied from a checkpoint instead of from a plan's end.
4. **The primitive in flight.** A `Safe` primitive has no `Request`, so the resumed or repeated
   run runs it again. An `Unsafe` or `CheckState` primitive in flight gives `UnconfirmedRequest`.
   Every later resume decides it again before it counts one, so it holds until a person acts, as
   ADR-286 and ADR-291 do for their endings.
5. **Interruptibility at the crash point.** With `Yes`, item 3 applies as it stands. With `No`,
   the job never resumes from a checkpoint: it makes a plain start while no request was
   recorded, and otherwise ends on site (uninterruptible section), since a plain procedure
   declares no re-entry point. With `RecoveryRequired`, nothing changes: ADR-291 and ADR-293
   refuse first.
6. **Limit and gates.** A plain procedure has no plan to carry `max_resumes`, so the limit is
   agent policy for the job (default 3) and is not in the IR. It bounds plain starts on an
   existing journal and checkpoint resumes together. Each is recorded with the existing resume
   record under a reserved stage id, which `Program::validate` refuses for a plan. The record is
   committed after the checks that need no ECU and before anything is sent. At the limit the job
   ends on site, `ProcedureResumeLimitReached`.
   - A checkpoint resume reads the VIN first and promotes to the per-vehicle lock on a match
     (ADR-263). Another decoded VIN is the journaled identity abort (ADR-286), which is terminal.
     A VIN that is not established ends on site with `ProcedureIdentityNotEstablished`, and the
     job can be resumed again within the limit (ADR-290).
   - A procedure with several targets goes through the reopening order of 8.2.4 first, and a job
     that changes an ECU runs its 8.9.1 guard again, as with any restart.
   - The resumed run commits a run start with the restored state (the invariant of ADR-274). This
     settles the open point of ADR-272 item 2 for this kind of run. It gets its own step budget
     (ADR-287).
7. **End states.** The endings this decision adds are on-site intervention, never `Failed`:
   `Failed` is the procedure's own verdict (ADR-280). The new on-site reasons are
   `UnconfirmedRequest`, `NotResumable { cause }`, `ProcedureResumeLimitReached` and
   `ProcedureIdentityNotEstablished`. Endings that other decisions give a resumed or repeated
   run keep their states: the procedure's own `Fail` (ADR-280), the identity abort (ADR-286),
   and a lost access target during the 8.2.4 reopening, which ends as failed while the journal
   holds no transfer-start marker (ADR-325 item 12), as it does for any restart without one.

8. **`Safe` covers the session the ECU is left in.** A checkpoint resume does not establish the
   default session first: a plain procedure declares no session timeout or startup time to wait
   (ADR-265 takes them from the flash plan), and sending a session change would itself be a
   request that changes the ECU. A primitive is therefore `Safe` only when a repeat is harmless
   and it behaves correctly in any session and security state the procedure can leave the ECU
   in: the default session after a timeout, or the session and unlock the procedure had
   entered. A continuation that is all `Safe` changes nothing whichever of these holds, so
   running it in the residual state is sound. The 4.6 defaults make every read `Safe`, so an
   author whose read, DTC read, `ecu_info` or `precondition` needs a particular session (or
   unlock) annotates it `# @idempotency unsafe`, which makes the resume end on site instead; an
   unannotated read is taken to work in either state.

Rejected alternatives:
- **Resume at the checkpoint whenever S holds, even if no request was sent.** It skips `Safe`
  context steps (a session change, an unlock check) when a sound repeat exists.
- **Allow unsent non-`Safe` primitives ahead.** It sends requests that change the ECU under a
  lost session or unlock, with no state check.
- **Resume at the start of the enclosing section.** Lowered IR declares no such boundary.
- **Establish the default session before a checkpoint resume.** A plain procedure declares no
  session timeout or startup time to wait, and a session change is a request that changes the
  ECU, which the continuation must not send.
- **Continue an interrupted `No` section to its end.** The design's uninterruptible unit, flash,
  is redone from a declared point and never continued.
- **Batch the `Safe` checkpoints.** The resume origin would be lost.
- **Put the limit in the IR.** It is agent policy and would cost a schema change.

## Consequences

- The journal gains two records and three facts. A plain procedure costs one fsync per primitive
  outside the plans, two for a non-`Safe` one. Two things to measure when it is implemented: the
  encoded VM state against the journal's frame limit, and the fsync cost on eMMC.
- Behaviour changes:
  1. resuming a job of a writing plain program that never ran is `MissingJournal` (it ran the
     program before);
  2. a flash program that crashed before its transfer, after a non-`Safe` primitive ahead of its
     plan, ends on site (it made a plain start);
  3. a plain start counts against the limit.
- With the 4.6 defaults an unannotated `request` is `Unsafe`. A procedure that opens an extended
  session with an unannotated request is therefore not repeatable after that step. The remedy is
  a revised default for session control and TesterPresent, which ADR-332 (Consequences) expects
  when the 4.6 table is revisited together with the restart-declaration syntax.
- The handover summary (design 8.2.5) gains two of the new facts and no VM state: whether a
  `Request` was ever recorded, and whether the last one is newer than the newest state record.
  A receiving device makes a plain start when none was recorded and otherwise ends on site,
  `UnconfirmedRequest` when the last one is unconfirmed and `NotResumable` otherwise, since it
  holds no checkpoint to resume from.
- ADR-244's record set gains `Checkpoint` and `Request`.
- ADR-325 item 7's journal condition (any combination with a flash plan or a recovery-required
  section) becomes every journaled job; a missing journal of a job with several combinations is a
  plain start only when every combination meets item 1's condition.
- ADR-252 item 7 (journals) and ADR-293 items 1 to 3 (which jobs keep one, and the
  missing-journal rule) are amended, as is ADR-253 item 3's no-journal case. ADR-272 item 2's
  open point is settled for this run kind.

### Test matrix

"Request recorded" means a non-`Safe` `Request` was ever committed. One runner test per row.

| Crash point | Outcome |
|---|---|
| Inside a `RecoveryRequired` section, any | on-site `RecoveryRequiredSection` (unchanged) |
| `Unsafe` or `CheckState` primitive in flight | on-site `UnconfirmedRequest` |
| Between primitives, or a `Safe` one in flight, no request recorded (`Yes` or `No` section, any tail) | plain start, counted |
| Request recorded, `Yes`, tail all `Safe`, VIN matches | checkpoint resume, the job completes |
| Request recorded, tail reaches `Unsafe` or `CheckState` (also by a loop back) | on-site `NotResumable` (unsafe primitive ahead) |
| Request recorded, tail enters a `RecoveryRequired` section or a plan entry | on-site `NotResumable` (recovery-required section ahead, plan ahead) |
| Request recorded, inside a `No` section | on-site `NotResumable` (uninterruptible section) |
| Request recorded, `Yes`, tail all `Safe`, no target VIN or no VIN source | on-site `NotResumable` (no identity) |
| Same, VIN not established | on-site `ProcedureIdentityNotEstablished`, re-resumable |
| Same, another VIN decoded | identity abort, terminal |
| Any resumable row at the limit | on-site `ProcedureResumeLimitReached`, nothing sent |
| No journal, all-`Safe` program | plain start |
| No journal, any other program | `MissingJournal` |
| Flash program, crash before its transfer after a non-`Safe` primitive ahead of the plan | on-site `NotResumable` (plan ahead) |
