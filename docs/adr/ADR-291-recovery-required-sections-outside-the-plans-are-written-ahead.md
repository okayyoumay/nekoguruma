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
2. **The classification refuses while no transfer-start marker follows the intent.**
   `restart::classify` returns `OnSiteInterventionRequired(RecoveryRequiredSection)`, before it
   looks at the transfer, when the journal's last intent is such a section intent and either
   the journal holds no transfer, or its transfer-start marker is older than the intent. This
   holds whether or not the intent is the interruption point:
   - with no transfer, the job could only go on with a plain start, which runs the program from
     instruction 0 and so runs the section again;
   - with an older transfer, the restart redoes that transfer from its plan's entry and goes on
     past the plan, back through the section, or the continuation after a read-back
     verification does (which ADR-271 item 4 already refuses).

   So a crash after the section's last response is refused too, as is a run that ended later
   (a failed or unanswered step, a cancel, an identity read at a plan's entry that failed), up
   to the next transfer-start marker. 8.10.1 rules out repeating the section, and the journal
   cannot tell a section that completed from one that did not.
3. **A transfer-start marker after the intent lifts the refusal.** Execution left the section
   and reached that plan's erase, so the section's pass ended, and the restart replays from the
   plan's entry, not from the program's start. If the program's own flow goes from the plan
   into the section again, for example a section after the plan that a loop back to the entry
   passed before, the restart's pass through it is the next pass the program makes after the
   plan, which the uninterrupted run makes too. It gets its own intent. What 8.10.1 rules out is
   repeating a pass whose end is not known, and a later transfer-start marker shows the earlier
   pass ended.

## Consequences

- A crash in a `RecoveryRequired` section before a plan's entry, or after a plan, now ends the
  job in `OnSiteInterventionRequired` instead of a plain start or, after a completed plan, the
  read-back verification. Nothing is sent and no resume is counted.
- The same holds for a job whose run ended after such a section and before the next
  transfer-start marker, also when steps of a plan were journaled in between. The person on site
  decides whether the section may run again.
- Each pass through such a section costs one more synced journal write. A loop that leaves the
  section and comes back writes one per iteration.
- The section is a range of instructions. A request the section makes through a `Call` to code
  outside its range is outside the section, as it already was for the classification
  (ADR-245 item 3), so it gets no intent.
- A program without a flash recovery plan keeps no journal (ADR-252 item 7), so this does not
  cover it: a resume of such a job is still a plain start, whatever sections it has.
- ADR-271 item 4's static refusal stays. It still covers a crash between a plan's end and a
  section after it, before the section's intent.
- A job refused this way has a journal it cannot go on with: every later resume decides the
  same, before it counts a resume.
