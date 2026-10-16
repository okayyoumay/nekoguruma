# ADR-304: Lowering Starlark Procedures to the IR

**Date:** 2026-10-15
**Status:** Accepted
**Affects:** `diag-frontend` (`src/star/lower.rs`, `docs/starlark-subset.md` 4.7 and 4.8); `docs/system-architecture.md` 8.4

## Context

`diag_frontend::star::check` accepts a procedure written in the Starlark subset (ADR-254), and
the procedure part of the IR is a `diag_ir::Program`: a flat instruction list for a stack VM
with four value types, typed instructions and no instruction that asks a value's type
(ADR-233). Lowering one to the other needs choices the subset specification leaves to the
transpiler: how functions are laid out and called, where values live, how a Starlark operator
that depends on its operands' types (a comparison, a division, a truth test) picks its
instructions, what happens to `None`, and how the source map is laid out.

## Decision

1. **Layout and calls.** The program starts with a `Call` to `main` and a `Ret` that ends it;
   the functions follow. A caller pushes the arguments in order and calls; the callee's first
   instructions store them into its first local slots, last argument first. A function that
   returns a value leaves it on the stack before `Ret`; one that returns none leaves nothing.
   A call whose value is unused is followed by `Pop`.
2. **Values.** A function's parameters take its first local slots, then every other name its
   body assigns, sorted by name; temporaries for multi-instruction operators (`//`, `%`, `>>`,
   float `<=` and `>=`) come after. Top-level constants are folded at ingestion and never stored
   at run time, so programs use no globals. A `bool` constant is pushed as the result of a
   comparison, since there is no push for one. A `str` exists at run time as UTF-8 bytes.
3. **Types are inferred at ingestion.** Every local, parameter and return value gets the set of
   types it can have: a local the union over its assignments, a parameter over the arguments its
   callers pass, a call its function's return types. The analysis passes over the functions
   `main` reaches until no set grows (at most 256 passes, then the procedure is rejected). An
   operator whose instructions depend on its operands' types requires each to have one type;
   otherwise it is `STAR_NOT_LOWERABLE`. Flow-insensitive sets are coarser than Starlark's
   dynamic types, but they need no instruction the IR lacks, and a procedure is written for one
   ECU's data, where a value rarely changes type.
4. **Starlark semantics with the existing instructions.** `//` and `%` on `int`s use the
   truncating `Div` and correct the result when the remainder is non-zero and its sign differs
   from the divisor's; `>>` is lowered as `~((~x) >> n)` for negative `x`; a remainder by -1 is 0
   without a division; float comparisons test their operands for NaN (a float that does not
   equal itself) before the IEEE 754 comparison, giving Starlark's total order, where NaN equals
   NaN and is the greatest float, and `<=` is then `not (a > b)`; a float `/` tests the divisor
   and sends zero to the integer `Div` by zero, so it stops the program as Starlark would;
   `-x` on a float is `x * -1.0`, which keeps the sign of zero; `and` and `or` give the deciding operand's value. Values of different
   types are unequal without a comparison. What needs a missing instruction (conversion between
   `int` and `float`, concatenation, a length, a slice, `<<` at run time, a float floor or
   remainder, built-ins and string methods on values known only at run time) is
   `STAR_NOT_LOWERABLE`; with constant operands it is folded instead.
5. **No `None`.** A function that returns a value must return one on every path, and a call to
   a function that returns none cannot be used as a value (`STAR_UNSUPPORTED_SYNTAX`).
6. **Only what runs is lowered.** Functions that `main` does not reach are left out of the
   program and contribute no types.
7. **Source map.** An entry is written for the first instruction and wherever the source
   position changes; it covers the instructions after it up to the next entry. Columns count
   characters from 1.
8. **What is not lowered yet is rejected.** Loops, the `diag` functions not lowered yet (at
   first all but `diag.fail`), and section and idempotency annotations each have their own work; until then the lowering
   rejects them (`STAR_NOT_LOWERABLE`, or `STAR_API_NOT_AVAILABLE` for a `diag` function without
   an instruction), so nothing is lowered silently wrong.

## Consequences

- A procedure that uses a value as two types, or passes a helper both an `int` and a `float`,
  is rejected and must be written for one type, or wait for a conversion instruction.
- Every new instruction (length, concatenation, conversion, slice, remainder) turns some
  `STAR_NOT_LOWERABLE` cases into lowered ones without changing this model.
- Calls nest at most 64 deep at run time (`MAX_CALL_DEPTH`); recursion is already rejected by
  the checker, so only a chain of more than 64 distinct functions reaches the limit.
- Inference runs the lowering of each reached function once per pass, so a procedure whose
  types widen one step per pass costs that many passes; the 256-pass limit bounds it.
