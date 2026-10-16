# ADR-305: Lowering Starlark Loops

**Date:** 2026-10-16
**Status:** Accepted
**Affects:** `diag-frontend` (`src/star/lower.rs`, `src/star.rs`, `docs/starlark-subset.md` 4.2)

## Context

Subset 4.2 says every Starlark loop is a `for` over a finite sequence. A loop over a constant
list is unrolled. A loop over a `range` whose bound is known only at run time gets a counter
limited by `@maxIterations` (default 1000, reported as a warning), and running past the limit
ends the procedure with `Fail` code `LOOP_LIMIT_EXCEEDED` (ADR-280). The core lowering (ADR-304)
rejected every loop.

Lowering loops needs choices the subset leaves open:

- what an unrolled iteration binds the variable to;
- which `range` loops carry a counter;
- how a `range` that ends near the end of the integer range steps without overflowing;
- how far unrolling may grow;
- how a warning reaches the caller, since `lower` returned only errors.

## Decision

1. **Unrolling binds the variable to a constant.** Each iteration of a loop over a constant
   list (or a constant dict, whose keys it runs over) binds the loop variable to the item at
   ingestion, so the body folds with it. For example, a constant `did` reaches a call that needs
   a constant argument. When the item can exist at run time, the iteration also stores it in
   the variable's slot, so the variable holds the last item after the loop, as in Starlark.

   An assignment to the variable ends the binding for the rest of the iteration. The binding is
   cleared in place, not removed, so the bindings of the loops around keep their places.
   When the items cannot exist at run time (lists, dicts), no slot holds them, so the variable
   has a value only inside the body. Assigning it in the body, or using it anywhere in the
   function outside the loop, is rejected.
2. **`break` and `continue` jump.** The lowering keeps a stack of the enclosing loops' labels:
   `break` jumps past the loop, and `continue` to the next iteration (in an unrolled loop, the
   next copy of the body). No loop keeps a value on the stack, so the jumps need no clean-up.
3. **A `range` loop runs.** It is not unrolled, even with constant bounds, so `range(1000)`
   costs one body.
   - Its step must be a constant other than 0, because the loop's comparison (`<` or `>`)
     depends on the step's sign.
   - Its start and stop may be known only at run time. Both are evaluated once, before the
     loop, and kept in temporaries.
4. **Which loops are limited.** A `range` loop gets the counter when a bound is known only at
   run time, or when `@maxIterations` precedes it. A loop with constant bounds and no
   annotation is finite by its constants and needs none.
   - The counter allows exactly `limit` iterations. The next one ends the procedure with
     `Fail` `LOOP_LIMIT_EXCEEDED`, with a detail that names the loop's line and its limit.
   - A limited loop without the annotation is reported as the `STAR_LOOP_LIMIT_DEFAULTED`
     warning.
   - `@maxIterations` on an unrolled loop has no effect: the loop is finite by its list.
5. **No overflow at the end of the range.** Before stepping, the loop checks whether the next
   value would leave the 64-bit range. Such a value would be past every stop anyway, so the
   loop ends there instead of failing with the VM's overflow error.
6. **Unrolling is bounded.** One pass over the function bodies may do at most 2^20 units of
   unrolling work. A unit is an iteration, a statement or expression lowered in an unrolled
   body, or an argument of a call in one, which is looked at even when the call is refused,
   so nested loops multiply and a large body counts in full. A name an unrolled copy
   copies, hashes or compares also costs a unit per 64 bytes, so a long name repeated in many
   iterations counts by its length; names shorter than that cost nothing extra.
   - This bounds the size of the program and the work of lowering it, since each unrolled
     iteration lowers the body again, in every inference pass. A count of iterations alone
     would let a large body grow without bound.
   - Running past the bound rejects the procedure (`STAR_NOT_LOWERABLE`), even where errors are
     muted, like the constant budgets of subset 4.8.
   - Once any of these budgets is spent, unrolling and type inference stop where they are,
     since the procedure is rejected anyway. Otherwise the remaining copies would repeat work
     the budget has already refused, such as building the same large literal again.
7. **Warnings come back with the program.** `lower` returns `Lowered { program, warnings }`.
   Warnings are resolved like errors (file, line, column, snippet, hint), matching the
   `warnings` list of subset 5. An unrolled body meets the same problem once per copy, so
   errors and warnings are recorded once per code and place, as they are reported, and an
   expensive hint is not built again for a place already reported. The checker records the line of the `for` each `@maxIterations`
   belongs to, so the lowering finds a loop's limit by the loop's line.

## Consequences

- Loops over constant tables are lowered, and the items fold. This is what lets a loop pass a
  table's entries to calls that need constants.
- A `range` with a step known only at run time is `STAR_NOT_LOWERABLE`. A loop over bytes needs
  `range(len(b))`, which waits on a run-time `len` (the built-ins item).
- Callers of `lower` take `program` from the result and should show `warnings` to the author.
