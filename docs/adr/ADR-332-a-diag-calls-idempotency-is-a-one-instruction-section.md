# ADR-332: A `diag` Call's Idempotency Is a One-Instruction Section

**Date:** 2026-10-18
**Status:** Accepted
**Affects:** `diag-frontend` Starlark lowering (`src/star/lower.rs`: the annotations and the
section ranges built per covered copy), `crates/diag-frontend/docs/starlark-subset.md` 4.5 and
4.6, design 8.2.5; ADR-233, ADR-301, ADR-329, ADR-330

## Context

Subset 4.6 gives every `diag` call an idempotency. The value is either an `@idempotency`
annotation or the call's default. The job runner reads idempotency from `diag_ir::Section`, a
range of instructions that carries interruptibility, idempotency and an expected duration
together. Sections may overlap.

ADR-329 lowers each copy of the functions a section function reaches to one `Section` with
`CheckState`, meaning "not stated". Its item 3 says that a lowered `@idempotency` takes that
value's place for the instructions it covers. No decision said how.

Today's readers combine overlapping sections conservatively:
- the restart's continuation check (`unsafe_continuation` in `crates/agent/src/restart.rs`)
  treats a primitive as not safe when no section covers it, or when any covering section is
  not `Safe` or is `RecoveryRequired`;
- `Program::validate` refuses an `Unsafe` section that overlaps a restartable plan's replay
  window;
- the cancel hold (`cancel_held_at` in `crates/agent/src/runner.rs`, ADR-330) holds a cancel
  wherever any covering section is not interruptible.

So a `Safe` per-call range laid over a function's `CheckState` range would still read as not
safe. The mapping must either change how the readers combine sections, or keep lowered
sections from overlapping.

## Decision

1. **One `Section` per diagnostic primitive.** For every diagnostic primitive it emits, the
   lowering emits one `Section` over that instruction alone (`pc..pc + 1`), not over the
   instructions that set up its operands. The section's idempotency comes from the innermost
   `@idempotency` whose statement contains the call. Without one, it is the 4.6 default for the
   Starlark call; `request`'s default follows its service identifier, which is a constant.
   Design 8.2.5 gives the attribute to each primitive, and a one-instruction range is exactly
   that. A range that also covered the operand set-up could reach into a plan's replay window
   ahead of an erase and trigger a false refusal; no reader reads idempotency at an
   instruction that is not a primitive.
2. **What an annotation reaches.** An `@idempotency` applies to every primitive its statement
   lowers to, including those of the nested statements of an `if`, `for` or `while`. An
   annotation on a nested statement takes precedence there.

   It does not reach:
   - the functions the statement calls, which keep their own annotations and defaults;
   - a nested `def` or lambda, which is a function of its own (2.6).

   A statement that lowers to no primitive cannot be annotated (`STAR_BAD_ANNOTATION`), so an
   annotation never does nothing silently. A specialized copy (ADR-301) carries its
   original's annotations.
3. **Interruptibility and expected duration are inherited.** A per-call section outside every
   section function is `Yes` with an expected duration of 0, which matches code outside every
   section. Inside a section function's copy, it carries that function's interruptibility and
   expected duration. A per-call section must never weaken them: a `Yes` piece inside an
   uninterruptible function would open a cancel point right before the request
   (ADR-330). In a `RecoveryRequired` section, a piece with another interruptibility would
   reset the journal's pass and record a second intent for one pass.
4. **Split, never overlap.** Each covered copy's range is cut around its primitives:
   - each primitive gets its one-instruction section (item 1);
   - each piece between primitives, which covers no primitive, keeps `CheckState` as ADR-329
     item 1 says;
   - empty pieces are dropped.

   In a lowered program, therefore:
   - sections never overlap;
   - each diagnostic primitive is covered by exactly one section;
   - each covered copy is covered without a gap by pieces that share one interruptibility and
     one expected duration.
5. **Readers unchanged.** The runner, the restart and `Program::validate` keep combining
   covering sections as they do today. That rule stays the safe reading of hand-built or
   overlapping IR, and ADR-301 item 5 promised no IR or agent change for section functions.
   The invariant of item 4 is a property of the lowering, tested in `diag-frontend`;
   `Program::validate` does not enforce it.
6. **Entering a section.** With items 3 and 4, execution moves from outside every
   non-interruptible section to inside one only at the section function's own call. Every
   piece of one section shares the function's expected duration. So the check before an
   uninterruptible section (design 8.10.1) can read it from the section that covers the first
   instruction of the called function.

Rejected alternatives:
- **Narrowest covering range wins.** It changes three readers and the future repeat policy,
  makes one struct's two attributes combine by different rules, and is ambiguous for ranges of
  equal size or partial overlap.
- **Keep the function's range and set its idempotency to `Safe`.** `Safe` would then mean both
  "safe" and "not stated", and a section without per-call ranges would make everything inside
  it safe.
- **Move idempotency out of `Section` into its own per-primitive table.** It is closest to
  design 8.2.5's wording, but it needs a schema bump that touches the golden files, the fuzz
  seeds and every reader, for a problem disjoint ranges already solve. It stays open as a
  later option.
- **Per-call ranges only for annotated calls.** It contradicts 4.6's defaults: an unannotated
  read would stay uncovered and therefore not safe.

## Consequences

- Unannotated reads become covered and `Safe`, so the restart's continuation check stops
  treating them as unsafe, as 4.6 intends. Only programs with flash recovery plans can observe
  this, and the frontend emits none yet.
- The golden IR files gain sections when the lowering lands (`NGR_UPDATE_GOLDEN=1`).
  `Program.sections` grows to about one entry per primitive plus the pieces between, and the
  cancel hold stays linear in it. Section indices in on-site reasons and validation errors
  name pieces, not section functions.
- `request`'s default makes a session change or a TesterPresent `Unsafe`. Once a restart
  declaration can be written in Starlark, an unannotated session change in a restartable
  plan's replay window fails validation. The 4.6 table is revisited with the
  restart-declaration syntax, not here.
- The lowering of `@idempotency` follows this decision, and its tests check the four
  properties of item 4. The entry check before an uninterruptible section (design 8.10.1)
  builds on item 6.
- ADR-329 item 3 is realized by this ADR. Its item 1 stays for the pieces between primitives.
