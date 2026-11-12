# ADR-375: The Communication Log Adds No Estimate to Reach a Judgement

**Date:** 2026-10-29
**Status:** Accepted (item 5's default timing margin settled by ADR-377, the default drift bound and clock margin in a settlement by ADR-378 and in a clock's wrap count by ADR-383, the presumed VCI timer by ADR-376, the late attribution by ADR-384)
**Affects:** design 8.11 (the section's head); `agent` communication log (`crates/agent/src/comm_log.rs`
and `crates/agent/src/comm_log/`); the review guidelines for Rust code
(`.github/instructions/rust.instructions.md`); ADR-346 item 2

## Context

ADR-346 item 2 says that every judgement in the communication log rests on what the agent can
observe, and that a question the evidence leaves open is `unknown`, `undetermined` or "not
judged", never a guess. That rule names what the log must not do with a gap in its evidence,
but not what it must not do to close one.

The boot clock work (ADR-374) showed the difference. To judge whether a lost VCI kept its power
across runs, it measured the gap on the boot clock, and each time review found a case the
comparison got wrong, it added something to make the comparison decide:
- a bound on the sleep's error;
- a widened tolerance;
- a rule for a clock that may have wrapped;
- a check for a suspend in the gap.

None of these came from the evidence. Each was an estimate standing in for a quantity the log
could not observe, and each left a further case that came out false in one direction or the
other. The maintainer ended that work by recording the gap and judging nothing from it, and set
(2026-10-29) the rule this ADR writes down.

## Decision

1. **A judgement only where the evidence decides it.** The log states an outcome (a
   classification, a lateness, an owner, a kept power) only when every case its evidence
   leaves possible gives that outcome. If a realistic case consistent with the evidence gives
   another, the outcome is `unknown` or `undetermined`, however rare that case seems.

2. **No estimate in place of evidence.** To reach a judgement, the log adds nothing it has not
   measured or been given: no bound on an unmeasured error, no tolerance chosen so that a
   comparison decides, no value assumed for one it did not read, no rule of thumb about what
   usually happens. A value the job is given for the link or the VCI (a profile's margins, a
   document's timing) is evidence of what was configured, and the entry names where it came
   from (design 8.11 `source`).

3. **What it cannot judge, it records faithfully.** An entry that leaves a question open:
   - says so in its outcome (`unknown`, `undetermined`, "not judged");
   - names in its reason what is missing or what the evidence leaves possible;
   - records the inputs and the comparison it could make, so a reader with more knowledge (of
     the vehicle, the VCI, or the machine) can judge it.

   Recording a figure is not judging from it: the log may record a measurement it does not yet
   trust (ADR-374's boot clock gap) beside an `unknown` outcome.

4. **A check that falls short is not patched case by case.** When review shows that a
   judgement's check lets a false outcome through, the fix makes the evidence decide. That means
   either reading what was missing, or narrowing the judgement to the cases the evidence
   decides. It does not add a further estimate that closes only the case found.

5. **Existing judgements.** The log's existing rules were written under ADR-346 item 2. They
   are checked against this ADR in a later change, which backs each outcome with its evidence
   or turns it into an open one; until then, they stand as written. That covers the agent's
   default margins (ADR-357), which stand in for a VCI profile that no job reads yet.

## Consequences

- Design 8.11 states the rule at the head of its section, so a reader takes every outcome in
  the log as one the evidence decided.
- More exchanges and settlements may end as `unknown` or `undetermined`. That is the intent:
  an open outcome with its inputs tells the reader more than a decided one that may be false.
- Review of the communication log's code checks each new or changed outcome against items 1
  and 2: what evidence decides it, and what case would give another outcome
  (`.github/instructions/rust.instructions.md`).
- The check of the existing rules (item 5) may revise rules that earlier ADRs set; each
  revision is recorded in its own ADR.
