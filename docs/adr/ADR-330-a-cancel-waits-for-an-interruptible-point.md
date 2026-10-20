# ADR-330: A Cancel Waits for an Interruptible Point

**Date:** 2026-10-18
**Status:** Accepted (the Consequences' program with one section over all of its code can be cancelled before its first step since ADR-337)
**Affects:** `agent` job runner (`run_vm`, `cancel_held_at`, `holds_cancel` in
`crates/agent/src/runner.rs`), `crates/agent/docs/ngr-agent.md`; design 5.6, 8.10.1; ADR-235
item 7, ADR-252 item 3, ADR-301, ADR-329

## Context

Design 5.6 lets a cancelled write job reach Cancelled only once it arrives at an interruptible
point: in an uninterruptible section the cancel is held, and the section runs to its end. The
job runner took a cancel before every instruction (ADR-235 item 7, repeated by ADR-252 item 3)
and left section policy to a later runner. Starlark section functions now lower to sections
(ADR-301), one per copy of the functions a section reaches (ADR-329), so a procedure can mark
its own uninterruptible ranges, and the runner has to honour them.

Three points are left open by the design text:

- design 8.10.1 has three attributes, and 5.6 names only the uninterruptible one when it holds
  a cancel;
- a procedure whose last section holds a cancel reaches its end through returns only, which lie
  outside every section;
- the restart's own checks between its replay and its continuation read the cancel flag
  directly.

## Decision

1. **A cancel is taken only at an interruptible point.** The runner takes a cancel before an
   instruction outside every section whose attribute is uninterruptible or recovery-required.
   A recovery-required section is no interruptible point either: stopping in it needs on-site
   intervention (8.10.1), and 5.6 lets a job become Cancelled only at an interruptible point.
   Inside such a section the cancel is held, both by the step loop and by the journal's
   boundary reads, which see a flag that is never set.
2. **A procedure that ends while a cancel is held finishes.** A `Ret` is not an interruptible
   point. The returns after a procedure's last section therefore do not turn a procedure that
   ran every instruction into a cancelled one, and a cancel held there is taken before the next
   instruction that is neither in such a section nor a `Ret`. A procedure that ends by `Fail`,
   the step limit or an error while a cancel is held ends that way, as it would without the
   cancel.
3. **Which instructions hold a cancel is computed once per run.** The runner marks each
   instruction from the program's sections before the first step, so a step does not scan the
   sections.
4. **The restart's own checks are unchanged.** A restart's checks before its replay run before
   any resumed program and keep reading the flag (ADR-264). The checks between the replay and
   the continuation (`check_before_erase`, the continuation's first check) read it too, so a
   cancel held through a replayed range that ends inside a section is taken there. No frontend
   emits flash plans yet; holding it there is a follow-up.

## Consequences

- A cancel can be held for as long as a section runs, which a procedure decides: the step limit
  bounds the steps, but a `Wait` takes time without steps, so a held cancel can keep the job
  thread, its guards and its link for a long time. The caller must not hand the worker to
  another job meanwhile (ADR-235 consequences). A signed program with one non-interruptible
  section over all of its code cannot be cancelled before it ends.
- ADR-235 item 7's check before every instruction and ADR-252 item 3's "no further instruction
  runs" hold outside non-interruptible sections only.
- The OEM authentication check before an uninterruptible section (8.10.1) is not part of this
  decision; the agent holds no OEM authentication yet.
