# ADR-337: A Run's Start Is an Interruptible Point

**Date:** 2026-10-20
**Status:** Accepted
**Affects:** `agent` job runner (`run_on_from`, `run_vm` in `crates/agent/src/runner.rs`),
`crates/agent/docs/ngr-agent.md`, Starlark subset 4.5
(`crates/diag-frontend/docs/starlark-subset.md`); design 5.6, 8.10.1; ADR-330, ADR-332, ADR-336

## Context

ADR-330 holds a cancel inside every uninterruptible or recovery-required section and takes it at
the next interruptible point. Its Consequences accept that a program with one such section over
all of its code cannot be cancelled before it ends. With the Starlark layout of ADR-304 item 1,
pc 0 was a `Call(main)` outside every section, so even a procedure whose `main` was a section
had one interruptible point before anything was sent: a cancel that came while the link was
opening or the journal was being created ended the job as cancelled.

ADR-336 item 5 emits `main`'s body at pc 0. A `main` marked `uninterruptible` or
`recoveryRequired` now covers pc 0, so the same cancel would be held, and the whole procedure,
writes included, would run to its end. Nothing in the procedure has started at that point: no
request has been sent, and no part of the ECU is in a state the section protects.

## Decision

1. The first instruction of a run from the program's start is an interruptible point, whatever
   sections cover it. This applies to a new run and to a plain start on an existing journal
   (ADR-335), both of which enter the step loop at pc 0 with a fresh VM. The job runner reads
   the real cancel flag at that first loop iteration, after the journal's run start, so a cancel
   set before the first step ends the job with `JobError::Cancelled` and nothing sent.
2. Every other entry keeps ADR-330's rule unchanged: a checkpoint resume, a flash plan's restart
   and its replay enter the loop without the exception, and the ADR-335 classification of a
   checkpoint at pc 0 still treats a covering non-interruptible section as not interruptible.
3. The rule sits in the loop's first iteration rather than in a check before the loop, so a
   cancel that comes between the run start and the first step cannot be missed.

## Consequences

- A Starlark procedure whose `main` is a section can be cancelled before its first step, as it
  could before ADR-336, and a hand-built program with one section over all of its code now can
  too. ADR-330's Consequence that such a program cannot be cancelled before it ends holds from
  the first step on.
- Once the first step has run, the procedure is inside the section and a cancel is held to its
  end (ADR-330).
- The OEM authentication check before an uninterruptible section (design 8.10.1, ADR-330) has,
  for a section `main`, this same entry point to hook into.
