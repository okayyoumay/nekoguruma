# ADR-303: Starlark Nesting Bounds Checked Before Parsing

**Date:** 2026-10-15
**Status:** Accepted
**Affects:** `crates/diag-frontend/src/star.rs`, `crates/diag-frontend/src/star/bounds.rs`,
`crates/diag-frontend/docs/starlark-subset.md` (2.8, 5, 6), design 8.4

## Context

Procedures are untrusted input on the server (design 8.4). `starlark_syntax` (0.14) parses with
a recursive-descent and Pratt parser, runs a validation pass over the AST inside
`AstModule::parse`, and its AST is a tree of `Box`es; the subset checker walks that AST
recursively too. Each of these recurses once per level of the AST, and a deep enough file
overflows the thread's stack. Rust cannot recover from a stack overflow: the process aborts, so
one file could take the whole service down.

The parser itself loops over left-associative operator chains and postfix chains (`a + b + c`,
`x["a"]["b"]`), but the AST it builds for them is one level deeper per link, and the validation
pass, the checker and the drop recurse over it. Measured per level of nesting, in a debug build:
nested lists about 14.6 KiB of stack, parentheses 12.5 KiB, nested `if` blocks 8.2 KiB, `elif`
clauses 6.7 KiB (each nests the rest of the statement), unary and operator chains 1.6 to
2.6 KiB; a release build needs roughly a tenth of that.

A source size limit alone does not bound this: keeping the stack at a few MiB would need a limit
of 10 to 20 KiB of source, too small for real procedures. The other options considered were a
parser of our own for the subset (full control, but a few thousand lines and a port of the
checker), a fork of `starlark_syntax` with a depth counter (a fork to maintain, and the drop and
the checker still recurse), growing the stack on demand (`stacker`, which cannot reach the
recursion inside `starlark_syntax`), and converting in a child process (which contains the
abort but cannot tell the author what is wrong).

## Decision

1. **The depth of the AST is bounded before parsing, from the tokens.** `check` lexes the file
   with `starlark_syntax`'s own lexer, so strings, f-strings, comments and line continuations
   are read exactly as the parser reads them, and counts:
   - the bracket depth (`(`, `[`, `{`, an f-string's `{`), at most 64;
   - the block depth (indented blocks), at most 32;
   - the `elif` clauses of the `if` statements enclosing a point, at most 256 together;
   - the expression depth, at most 512: the operators, keywords, postfix accesses (`.`, an
     index, a call) and brackets on the deepest path from a statement down to an operand.

   Every inner node of an expression has a token of its own, so these counts are never less than
   the depth they stand for; where a token nests nothing (a statement keyword, a parenthesis),
   the bound is only stricter. The expression depth is computed per bracket level from the
   elements that commas separate: an element nests at most its links plus its deepest bracketed
   operand, since any link may wrap any operand, and a closed bracket nests one more than its
   deepest element. Commas between a `lambda`'s parameters do not separate elements, since its
   body nests under all of them; a `;` ends an expression but not the `if` statement whose
   inline body holds it. A file past a bound is refused with `STAR_NESTING_TOO_DEEP` at the token that passes
   it, and is never parsed. A file larger than 1 MiB is refused with `STAR_SOURCE_TOO_LARGE`
   before it is lexed, which also bounds the time and memory of the later passes.
2. **The bounds are kept separate**, not folded into one weighted budget, so the specification
   can state each in terms an author recognizes.
3. **A file within the bounds is parsed and checked on a thread with a fixed 16 MiB stack**
   (`on_parser_stack`). A file at every bound at once, in the costliest shapes (nested
   comprehensions inside nested blocks inside an `elif` chain, around a `not` chain that fills
   the rest of the expression depth), needs
   about 5 MiB in a debug build and under 1 MiB in release. The stack is reserved address
   space; only the pages a parse touches are committed. Any later recursive walk over the AST
   (the lowering) runs on the same kind of thread. Dropping the AST needs far less and may happen
   on the caller's thread.
4. **Tests at the bounds are the guard against a change in `starlark_syntax`.** For each shape a
   file at the bound checks and one past it is refused; a file at every bound at once checks in a
   debug build on the fixed stack and its AST drops on a test thread's default stack; 100,000
   levels are refused. Dependabot's grouped minor and patch Cargo updates merge themselves once
   the required checks pass (ADR-248), so an update that makes a level cost more stack fails
   these tests instead of reaching main.
5. **`starlark_syntax` is built with optimisation in the dev profile too.** Its lexer, which
   logos generates, recurses once per character of a long run of digits, spaces or line
   continuations when built without optimisation, so a debug build of a test or a tool would
   overflow on a large but valid file; with `opt-level = 1` it does not, as in release. The
   pre-scan, which lexes the whole file, also runs on the fixed stack.

## Consequences

- A procedure deeper than the bounds must be split up, for example into helper functions or
  intermediate names. The bounds are well above what hand-written procedures use (CPython's own
  limits are 200 levels of brackets and 100 of indentation).
- The bounds hold only for `starlark_syntax`'s current recursion; a version that recurses more
  per level needs the bounds or the stack revisited, which the tests at the bounds report.
- Converting in a child process, so that an unforeseen crash in the conversion cannot take the
  server down either, stays possible as defence in depth once the server exists.
