# ADR-300: Statically Resolved Lambdas, Nested Functions and Keyword Arguments in Starlark

**Date:** 2026-10-15
**Status:** Accepted
**Affects:** `diag-frontend` (`docs/starlark-subset.md` 2.2, 2.3, 2.6, 2.7, 4.1, 4.5, 4.7, 5); `docs/system-architecture.md` 8.4; ADR-254 Decision item 4

## Context

ADR-254 item 4 removed `lambda`, nested `def`, functions as values, and default, keyword and
variadic parameters of user functions from the Starlark subset. Two reasons stand behind the
function bans. A closure needs the environment it captured, and the `diag-ir` VM has no
instruction that builds or reads one. A function value needs an indirect call, and the VM's
`Call` jumps to a fixed address only, so the callee must be known at ingestion. Default and
keyword arguments were left out of version 1 only to keep the transpiler small.

The bans also rule out procedure features that depend on neither mechanism: a short expression
bound to a name, a helper written next to the one function that uses it, a helper such as a
retry or polling loop that takes the step to repeat as an argument, and calls that name their
arguments or leave the usual ones out. Authors must write a separate top-level `def` for each
repeated step, duplicate the loop around it, and pass every argument by position.

A lambda or nested `def` whose body names only its own parameters and locals and top-level
names (constants, top-level `def`s, built-ins, `diag`) captures nothing, so the first reason does
not apply. The second can be met at ingestion if the places a function may flow to are limited
so that every call has one known callee. Default and keyword arguments are resolved at each call
site and never reach the VM.

## Decision

1. A **capture-free function** is allowed: a `lambda`, or a `def` written directly in the body
   of a top-level `def` (a nested `def`). Its body may name only its own parameters and locals,
   top-level constants, top-level `def`s, the built-ins of the subset and `diag`. A name from the
   enclosing function (a local, a parameter, a loop variable, or another lambda or nested `def`
   bound there) is a capture and is still rejected with `STAR_CLOSURE_NOT_ALLOWED`, as are a
   lambda or `def` inside a lambda or nested `def`, and a nested `def` inside `if` or `for`.
   Their parameters follow the rules for top-level `def` parameters (item 6).
2. A function (a capture-free function or a top-level `def`) may appear in two places only:
   - **Binding**: a lambda assigned to a plain name, as a top-level constant or as a statement
     directly in a function body (not inside `if` or `for`), or a nested `def`, with no other
     binding of that name in the same scope. A use before the binding is rejected.
   - **Argument**: a lambda, a name bound to a function, a top-level `def` name or a function
     parameter (below), passed as an argument to a top-level `def` or to a bound function.
3. A parameter that receives a function is a **function parameter**. In its function's body it
   may only be called or passed on as an argument (item 2). A parameter is a function parameter
   when its body calls it or passes it to a function parameter of another function; the
   transpiler derives this from the bodies, following such forwarding chains, and checks every
   call site against it: passing a function to any other parameter, or a value to a function
   parameter, is rejected.
4. Every other use of a function stays rejected with `STAR_FUNCTION_VALUE_NOT_ALLOWED`:
   assigning it to another name, returning it, storing it in a list or dict, comparing it,
   passing it to a built-in or a `diag` function, using it as a default value, or passing it to
   a parameter that is neither called nor forwarded to a function parameter. `main` still takes
   no parameters.
5. **Lowering without new instructions.** A bound or passed lambda and a nested `def` are lifted
   to unnamed top-level functions. A call that passes functions is resolved by specialization:
   the transpiler emits one copy of the callee per distinct combination of the functions it
   receives, with each call through a function parameter replaced by a `Call` to the received
   function. Copies are shared between call sites that pass the same functions. Recursion
   detection (`STAR_UNBOUNDED_NOT_ALLOWED`) runs on the call graph after specialization, so a
   cycle formed through a function argument is caught.
6. **Default values and keyword arguments.** A parameter of a user function (a top-level `def`
   or a capture-free function) may have a default value, which must fold to a constant at
   ingestion and must not be a function; parameters with defaults follow those without, as
   Starlark requires. A call to a user function may name arguments after its positional ones.
   The transpiler binds each argument to its parameter at the call site and fills in the
   defaults, so the IR call stays positional; it keeps the source order in which the arguments
   are evaluated. A missing, repeated or unknown argument, or too many positional ones, is
   rejected with `STAR_ARGUMENT_MISMATCH`; for a call through a function parameter this is
   checked in each specialized copy. `*args`, `**kwargs`, keyword-only parameters and keyword
   arguments to built-ins and `diag` functions stay out.
7. None of this needs an IR, VM or agent change.

## Consequences

- Procedures can bind short helpers to names, keep a helper next to its one user, pass the step
  a helper repeats as an argument (for example a polling helper that takes the request to poll),
  and give helpers optional arguments.
- Specialization grows the program by one copy of a callee per distinct set of functions passed
  to it. Recursion is forbidden, so the number of copies is finite, but deep chains of helpers
  that each receive functions multiply. The VM's existing program limits still apply to the
  result.
- Source maps (subset 4.7) record a lifted function's instructions at its own position, and a
  specialized copy's instructions at the callee's position, so traces still point at the source
  the author wrote. Loop limits (subset 4.2) inside a callee apply to every copy. A nested `def`
  is a function body of its own for annotations (subset 4.5): a section starts and ends inside
  it.
- Reordering keyword arguments must not reorder their evaluation, since an argument can be a
  `diag` call; the transpiler evaluates in source order into locals before the call.
- The VM still has no environment and no indirect call. Capturing closures, functions stored in
  data, and functions passed to built-ins (a `key=` for `sorted`, `map`) stay out of the subset;
  allowing them would need IR instructions and an agent update.
- ADR-254 item 4's bans on `lambda`, nested `def`, functions as values and default and keyword
  arguments are narrowed by this ADR; variadic parameters stay banned.
