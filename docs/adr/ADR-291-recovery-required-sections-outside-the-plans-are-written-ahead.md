# ADR-291: A `RecoveryRequired` Section outside the Flash Recovery Plans Is Written Ahead

**Date:** 2026-10-14
**Status:** Accepted
**Affects:** `agent` (`src/journaling.rs`, `src/restart.rs`, `src/runner.rs` tests,
`docs/ngr-agent.md`), `docs/system-architecture.md` 8.2.5; ADR-245 item 3, ADR-252, ADR-253

## Context

A procedure section can be marked `RecoveryRequired` (design 8.10.1): an interruption inside it
needs on-site intervention, and the section must not be repeated automatically. ADR-245 item 3
makes the restart's classification refuse a point inside such a section, and `Program::validate`
lets a section overlap a flash recovery plan only from the plan's recovery-required point on,
where the intent of ADR-253 already places a crash.

Outside the plans the runner journaled nothing (ADR-252 item 2). Take a program whose
`RecoveryRequired` section lies before a plan's entry, such as a bootloader switch, and a crash
after a request in it was sent but before its response. The journal's last record is then the
run start, the classification finds no point in the section, and the job makes a plain start.
That runs the program from instruction 0 and sends the section's requests again.

ADR-271 item 4 already refuses the continuation after a read-back verification for such a
program. The plain start had no such check.

## Decision

1. **The first diagnostic primitive of a pass through a `RecoveryRequired` section outside every
   plan gets a request intent.** The runner commits `Record::Intent` for it before it is sent,
   once the VM's checks of it passed, as for the recovery-required point (ADR-253 item 1).
   - "Outside every plan" means outside `entry_pc..post_transfer_end_pc` of each plan.
   - A pass starts when execution arrives at such a section's instruction after one outside
     them, and ends when it arrives at an instruction outside them. The pass's later primitives
     add no intent: the first already places the point in the section.
   - A cancel before the intent sends nothing and leaves no intent, as for the plan's intent.
   - No record variant is added, so the journal format does not change.
2. **The classification uses the point as it stands.** The intent is a request written ahead, so
   it takes part in the interruption point (ADR-229 item 1, ADR-253). When it is the newest
   record by step count, the point lies in the section, and `restart::classify` returns
   `OnSiteInterventionRequired(RecoveryRequiredSection)`, before it looks at the transfer.
3. **The intent stays the point until a later record moves it.** Nothing marks the end of a
   section, so a crash after the section's last response, or a run that ended later outside the
   plans (a failed or unanswered step, a cancel), still classifies as inside the section. This is
   deliberate: such a job could only go on with a plain start, and a plain start runs the section
   again from the program's start, which 8.10.1 rules out. A later record, such as the step into
   a plan's entry or the erase's transfer-start marker, moves the point past the section, and
   the restart then replays from the plan's entry only, so it does not repeat the section.

## Consequences

- A crash in a `RecoveryRequired` section before a plan's entry, or after a plan, now ends the
  job in `OnSiteInterventionRequired` instead of a plain start or, after a completed plan, the
  read-back verification. Nothing is sent and no resume is counted.
- The same holds for a job whose run ended after such a section and before the next journaled
  record. The person on site decides whether the section may run again.
- Each pass through such a section costs one more synced journal write.
- A program without a flash recovery plan keeps no journal (ADR-252 item 7), so this does not
  cover it: a resume of such a job is still a plain start, whatever sections it has.
- ADR-271 item 4's static refusal stays. It still covers a crash between a plan's end and a
  section after it, before the section's intent.
