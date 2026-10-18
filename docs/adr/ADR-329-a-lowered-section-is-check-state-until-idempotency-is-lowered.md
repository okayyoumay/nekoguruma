# ADR-329: A Lowered Section Is CheckState Until Idempotency Is Lowered

**Date:** 2026-10-18
**Status:** Accepted
**Affects:** `diag-frontend` Starlark lowering of section functions (`src/star/lower.rs`),
`crates/diag-frontend/docs/starlark-subset.md` 4.5, design 8.4; ADR-245, ADR-271, ADR-301

## Context

ADR-301 makes a section a whole function: the lowering gives every function a section reaches a
copy only that section runs, and covers each copy with a `diag_ir::Section` carrying the section
function's interruptibility and expected duration. A `Section` also carries an `Idempotency`,
which the `@section` annotation does not give. `@idempotency` (subset 4.6), which is to say how
safe a step is to repeat, is a separate annotation that is not lowered yet, and its lowering has
to be designed consistently with section functions.

The lowered section therefore needs an idempotency that adds nothing the author did not say. The
places that read a section's idempotency today are the restart checks:

- `unsafe_continuation` (`crates/agent/src/restart.rs`, ADR-271 item 4) treats a primitive
  outside every flash plan as unsafe to repeat unless every section covering it is `Safe`, and
  one covered by no section as unsafe too;
- in the window of a plan that allows a restart, it and `Program::validate`
  (`crates/diag-ir/src/recovery.rs`, ADR-245) refuse only a primitive covered by an `Unsafe`
  section, and accept one covered by no section.

## Decision

1. **A lowered section's idempotency is `CheckState`.** In both checks above a primitive covered
   only by `CheckState` sections is judged exactly as one covered by no section: not safe to
   repeat outside the plans, and not refused in a plan's window. So covering a function with a
   section changes its interruptibility and expected duration, which the author wrote, and
   nothing about whether its steps may be repeated, which the author did not.
2. **`expected=` defaults to 0.** A `@section` without `expected=` gives the section an expected
   duration of 0 milliseconds, which never exceeds the remaining OEM authentication time.
3. When `@idempotency` is lowered, the idempotency it gives a statement takes the place of
   `CheckState` for the instructions it covers; this ADR's default stays for the rest.

## Consequences

- A procedure written in Starlark can declare interruptibility now, and its program carries it.
  The agent reads only `recoveryRequired` so far: a journaled run (`run_program_journaled`) keeps
  a journal for it and writes its intents as ADR-291 and ADR-293 describe, and the restart checks
  judge it. `uninterruptible`, `interruptible` and the expected duration reach the program but
  no part of the agent acts on them yet.
- One section function becomes several `Section` entries, one per copy it runs (ADR-301 item 3).
  A check made when a section is entered, such as comparing the expected duration with the
  remaining OEM authentication time (design 8.10.1), has to tell the section function's own
  entry from a call into a helper's copy inside the section.
- A reader of the IR sees `CheckState` on every lowered section; it means "not stated", not that
  the author chose a state query. A later change to how the job runner treats `CheckState` must
  keep it equivalent to "not covered" for sections this ADR lowers, or lower them differently.
