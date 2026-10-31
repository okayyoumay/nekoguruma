# ADR-397: A Parity Gap Is Fixed by Sharing the Existing Code

**Date:** 2026-10-31
**Status:** Accepted
**Affects:** `.claude/README.md` ("Gates for the analysis agents", the `edge-case-hunter` parity
check), `.claude/agents/edge-case-hunter.md` (parity check)

## Context

The `edge-case-hunter` pass has a parity check: when a change adds a path that builds what an
existing path already builds, the agent lists the existing path's steps and reports each one the
new path leaves out. The check says what is missing; nothing says how the gap is closed.

In the presence-request pull request (ADR-396), the identification runner rebuilt ADR-394's
recovery of a worker that exited or hung, which `WorkerHost::try_reopen` already did. The
parity check reported one missing step (a worker that exits between two links was started
again without being logged). The fix brief asked to reuse the existing code, or at least to log
the exit, and the smaller fix was taken: a second probe in the identification runner. Automated
review then found, one round at a time, two more steps that probe lacked (a worker that exits
during the link's set-up, and one that hangs there). Only the fourth round's fix moved the
settle step into one function both runners call, which also closed the same gap in the program
runner's reopen. The two rounds in between each copied one more step into the second path.

## Decision

1. A step that the parity check, or a later review, finds missing from a path that rebuilds an
   existing one is fixed by having the new path call the existing path's code, extracted into a
   function both paths call. Copying the step, or reproducing part of it, is not the fix.
2. A copy stays only when the existing code cannot be called from the new path (it needs state
   the new path does not have, or its caller's contract differs); the pull request says why.
3. The fix brief asks for the extraction without offering a smaller fallback, and the parity
   check proposes the function to extract and its two callers.

## Consequences

- The steps the parity check did not list, and that review would otherwise find one per round,
  come with the shared function, as they did once the settle step was shared.
- An extraction can be larger than the one-step fix, and it changes the existing path's code, so
  that path's tests must stay green unchanged.
- When the existing code truly cannot be called, the copy and its reason are visible in the pull
  request, where a reviewer can challenge them.
