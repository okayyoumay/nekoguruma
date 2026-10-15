# ADR-300: Capture-Free Lambdas in Starlark Procedures

**Date:** 2026-10-15
**Status:** Accepted
**Affects:** `diag-frontend` (`docs/starlark-subset.md` 2.2, 2.3, 2.6, 4.1, 4.7, 5); `docs/system-architecture.md` 8.4; ADR-254 Decision item 4

## Context

ADR-254 item 4 removed `lambda`, nested `def` and functions as values from the Starlark subset.
Two reasons stand behind it. A closure needs the environment it captured, and the `diag-ir` VM
has no instruction that builds or reads one. A function value needs an indirect call, and the
VM's `Call` jumps to a fixed address only, so the callee must be known at ingestion.

The ban also rules out procedure helpers that do not depend on either mechanism: a short
expression bound to a name, or a helper such as a retry or polling loop that takes the step to
repeat as an argument. Authors must write a separate top-level `def` for each repeated step and
duplicate the loop around it.

A lambda whose body names only its own parameters and top-level names (constants, top-level
`def`s, built-ins, `diag`) captures nothing, so the first reason does not apply. The second can
be met at ingestion if the places a function may flow to are limited so that every call has one
known callee.

## Decision

1. A **capture-free lambda** is allowed. Its body may name only its own parameters, top-level
   constants, top-level `def`s, the built-ins of the subset and `diag`. A name from the
   enclosing function (a local, a parameter or a loop variable) is a capture and is still
   rejected with `STAR_CLOSURE_NOT_ALLOWED`, as are nested `def` and a lambda inside a lambda.
   Lambda parameters follow the rules for `def` parameters (positional only).
2. A function (a capture-free lambda or a top-level `def`) may appear in two places only:
   - **Binding**: a lambda assigned to a plain name, either as a top-level constant or as a
     statement directly in a function body (not inside `if` or `for`), with no other assignment
     to that name in the same scope. A use before the binding is rejected.
   - **Argument**: a lambda, a name bound to one, a top-level `def` name or a function
     parameter (below), passed as a positional argument to a top-level `def`.
3. A parameter of a top-level `def` that receives a function is a **function parameter**. In the
   callee's body it may only be called or passed on as an argument (item 2). A parameter is a
   function parameter when its body calls it or passes it to a function parameter of another
   top-level `def`; the transpiler derives this from the bodies, following such forwarding
   chains, and checks every call site against it: passing a function to any other parameter,
   or a value to a function parameter, is rejected.
4. Every other use of a function stays rejected with `STAR_FUNCTION_VALUE_NOT_ALLOWED`:
   assigning it to another name, returning it, storing it in a list or dict, comparing it,
   passing it to a built-in or a `diag` function, or passing it to a parameter that is neither
   called nor forwarded to a function parameter. `main` still takes no parameters.
5. **Lowering without new instructions.** A bound or passed lambda is lifted to an unnamed
   top-level function. A call to a top-level `def` that passes functions is resolved by
   specialization: the transpiler emits one copy of the callee per distinct combination of the
   functions it receives, with each call through a function parameter replaced by a `Call` to
   the received function. Copies are shared between call sites that pass the same functions.
   Recursion detection (`STAR_UNBOUNDED_NOT_ALLOWED`) runs on the call graph after
   specialization, so a cycle formed through a function argument is caught. No IR or agent
   change is needed.

## Consequences

- Procedures can bind short helpers to names and pass the step a helper repeats as an
  argument, for example a polling helper that takes the request to poll.
- Specialization grows the program by one copy of a callee per distinct set of functions passed
  to it. Recursion is forbidden, so the number of copies is finite, but deep chains of helpers
  that each receive functions multiply. The VM's existing program limits still apply to the
  result.
- Source maps (subset 4.7) record a lifted lambda's instructions at the lambda's own position,
  and a specialized copy's instructions at the callee's position, so traces still point at the
  source the author wrote. Loop limits (subset 4.2) inside a callee apply to every copy.
- The VM still has no environment and no indirect call. Capturing closures, functions stored in
  data, and functions passed to built-ins (a `key=` for `sorted`, `map`) stay out of the subset;
  allowing them would need IR instructions and an agent update.
- ADR-254 item 4's ban on `lambda` and functions as values is narrowed by this ADR; nested
  `def` stays banned.
