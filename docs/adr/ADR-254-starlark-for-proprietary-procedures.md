# ADR-254: Starlark Replaces JavaScript as the Proprietary Procedure Language

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `docs/system-architecture.md` 8.2, 8.4, 12; `diag-frontend` (`docs/starlark-subset.md`, `examples/sequence.example.star`); `diag-ir` `ir.fbs` `EcuDocument.source_format`

## Context

The proprietary vehicle-knowledge format (design 8.2, 8.4) pairs CSV tables for the declaration
part with a script language for procedures, which `diag-frontend` transpiles into the
procedure-part bytecode on the server. The design chose a JavaScript subset. No transpiler had
been written yet; only the subset specification and an example existed.

The procedure-part VM (8.2.3, ADR-233) dictates what a procedure language may express: every
loop must end, calls nest to a fixed depth, there is no exception mechanism, no object model,
no closures, no access to files, the network or the clock, and integer arithmetic is checked.
The JavaScript subset reached this by removing most of the language (`var`, closures, classes,
`this`, `async`, exceptions, regular expressions, `while` needed a limit annotation) and listing
over a dozen rejection codes. Procedure authors also had to learn which JavaScript they could
not write.

The maintainer asked which language fits the IR best. Candidates compared:

- **Starlark** (the configuration language of Bazel, a Python dialect with a published
  language specification): its base language has no `while`, forbids recursion, has no
  exceptions or classes, is deterministic and hermetic, and has int, float, string, bytes,
  list and dict types without implicit conversion.
- **OTX**: already planned as its own frontend (design 8.3, milestone M7), and an XML exchange
  format rather than a language people write by hand. The standard is not held yet, so how
  closely it matches the procedure part cannot be checked; it does not replace a hand-written
  format either way.
- **Lua**: execution state can only be persisted at a coroutine yield (Pluto, Eris), and the
  language has unbounded loops and recursion.
- **Rhai** and similar embedded Rust languages: no suspend/resume, general-purpose control flow.

## Decision

1. The proprietary format's procedure language is **Starlark**, as defined by the language
   specification in the `bazelbuild/starlark` repository. JavaScript is dropped entirely; there
   is one script frontend, not two.
2. Starlark is only parsed on the server. The transpiler uses the parser and AST of
   `starlark-rust` (`starlark_syntax`), not an evaluator, and emits `diag-ir` bytecode as
   before. The agent still ships no script engine.
3. A procedure file contains top-level constants and `def` statements, and runs from
   `def main()`. Base Starlark's restrictions (no `while`, no recursion) are kept; options some
   implementations offer to lift them are rejected.
4. The subset removes only what the VM cannot represent: `lambda` and nested `def`, functions as
   values, comprehensions and `set` (version 1), `load`, keyword/default/variadic parameters of
   user functions, dict access with computed keys, `fail` (replaced by `diag.fail` so every
   abnormal end carries a code) and reflection built-ins. The full list, the `diag` API and the
   comment annotations for section attributes and idempotency are in
   `crates/diag-frontend/docs/starlark-subset.md`.
5. Numbers keep Starlark semantics. Integers map to the IR's checked `I64`, so a value outside
   its range is a run-time error. `//` and `%` are floor division and floor remainder; the
   transpiler corrects the IR's truncating division for operands of different signs.
6. The provenance string in `EcuDocument.source_format` for this format is `csv-starlark`.

## Consequences

- The subset specification shrinks to the constructs that matter, and the rejected list is
  mostly constructs Starlark authors rarely use. Procedure authors write Python-like code.
- Loop limits stay: `for` over a non-constant `range` or a run-time value still gets an
  embedded iteration limit, since a bound read from a response can be very large.
- The annotations are comments, so the transpiler takes them from the comment tokens the
  parser's lexer reports (never from raw source lines, where `# @` could sit inside a string)
  and attaches each by its line to the following statement.
- Floor division needs the remainder and negation instructions the IR does not have yet
  (ADR-233 consequences); the transpiler's first procedures need them anyway.
- The `diag` API is wider than the current instruction set: `diag.fail`, `diag.ecu_info` and
  `diag.precondition` have no instruction, and `Log` takes only a constant message. The subset
  specification lists how each call lowers; a call whose instruction does not exist yet is
  rejected at ingestion until the variant is appended.
- The VM's values are `I64`, `F64`, `Bool` and `Bytes`, so lists and dicts exist only at
  ingestion (constant lists, `params` literals, literal-key access to responses), `None` is not
  supported, and a string built at run time is UTF-8 bytes. Constructs that need an instruction
  the IR lacks are rejected at ingestion; the subset specification's value table lists them.
- Users who already know JavaScript lose that familiarity; Starlark's Python syntax is the
  more common one in tooling and test automation, which is accepted.
- M2's exit criterion becomes "a CSV + Starlark definition of one ECU". The differential test
  against ODX (8.4, M7) is unchanged in shape.
