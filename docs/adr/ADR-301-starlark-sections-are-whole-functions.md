# ADR-301: Starlark Sections Are Whole Functions

**Date:** 2026-10-15
**Status:** Accepted
**Affects:** `diag-frontend` (`docs/starlark-subset.md` 4.5 and section 6, `src/star.rs`, `examples/sequence.example.star`); `docs/system-architecture.md` 8.4; ADR-300 Consequences

## Context

The Starlark subset marked an interruptibility section (design 8.10.1) with a `@section` comment
and a matching `@endsection` comment in the same function at the same indentation. Two problems
came out of reviewing the checker.

- The pair could open in one block and close in another: the `if` branch and its `else`, or two
  separate loop bodies. The IR's `Section` is one range of instructions (`start_pc..end_pc`), so
  such a pair does not describe a range that can be lowered, and a wrongly bounded
  `uninterruptible` range leaves part of a flash write interruptible.
- A call inside a section runs code outside the section's range. The VM's `Call` jumps to the
  callee's own instructions, and the job runner judges sections by program counter alone
  (`in_section_outside_plans` in `crates/agent/src/restart.rs`, the replay checks in
  `crates/diag-ir/src/recovery.rs`). A helper called from an `uninterruptible` section would run
  as interruptible.

Tightening the pairing rule to "the same statement list" fixes the first problem but not the
second, and keeps a pairing that authors can still get wrong. Starlark has no `with` statement
and no decorators, so a block-structured form has to come from the existing comment
annotations.

## Decision

1. **A section is a whole function.** A `@section kind [expected=millis]` comment directly before
   a top-level `def` makes that function a section function. The comment is outside every
   function body, and only comments and blank lines may stand between it and the `def`. A `def`
   takes one `@section`. `@endsection` is removed; a section ends where its function returns.
2. **Only top-level `def`s.** A nested `def` or a lambda (ADR-300) cannot be a section function.
   Neither may capture anything, so the same step written as a top-level `def` is equivalent.
3. **A section covers everything that runs during its call**: the section function's body, the
   functions it calls directly or through others, and the functions it receives as arguments
   and calls. The lowering emits a copy of each such function that only the section reaches and
   adds a `Section` with the same attributes over each copy's range; a function also called
   outside the section keeps a separate copy for those calls. This uses the specialization of
   ADR-300 item 5, and these copies count toward its budget of 1,000 copies per file.
4. **Sections do not nest.** A call to a section function from anything that runs inside a
   section, directly, through other functions or through function arguments, is rejected at
   that call with `STAR_BAD_ANNOTATION`. A call that closes a cycle is reported as recursion only.
5. No IR, VM or agent change is needed: sections stay ranges of instructions, and every range a
   section needs is a function body or a copy of one.

## Consequences

- A section can no longer be bounded wrongly, and an `uninterruptible` or `recoveryRequired`
  section can call helpers without leaving its range. The job runner's and the restart
  validation's program-counter checks stay correct as they are.
- Authors write each section as a function: values go in as arguments and come back as the
  return value, in a dict or list when there are several, since the subset has no tuples; a loop
  around a section that must stop early tests what the section returns.
- `expected=` is fixed per section function rather than per place in the code.
- A helper called both inside and outside sections appears twice in the program, and once per
  section that reaches it. The specialization budget bounds this.
- The checker does not accept function arguments yet (ADR-300 is not implemented there), so its
  nesting check follows direct calls only; the implementation of ADR-300 must follow calls
  through function parameters along the specialized call paths.
- A flash recovery plan's boundaries (ADR-245) are also ranges of instructions, so a plan
  written in Starlark meets the same question for calls made inside it; the syntax for the
  restart declaration must answer it consistently with item 3.
- ADR-300's Consequences said a nested `def` is a function body of its own for annotations, so a
  section starts and ends inside it. Sections no longer start or end inside a body, and a nested
  `def` cannot be a section function (item 2).
