# ADR-388: An Item Another Is Blocked On Takes Its Priority

**Date:** 2026-10-30
**Status:** Accepted
**Affects:** `work/README.md` (priorities), `.claude/skills/backlog` (add, update and close an
item), `.claude/skills/backlog-triage` (wrong priority)

## Context

A backlog item's priority is how much it matters once it can be done, and an item that waits on
another states that with a `Blocked on:` clause instead of a lower priority. Nothing set the
priority of the item waited on. So an item could block a milestone item and still sit below it.

A `backlog-loop` run with the P1 cap then found nothing eligible. Every P1 item was blocked, and
one of them, the job runner's repeat policy for diagnostic primitives (what remains of the
current milestone's restart criterion), waited only on a P2 item about the agent host's errors
that nothing blocked. `next-task` skips blocked items and does not go below the cap, so the work
that would move the milestone was not offered, and the milestone count left it out.

Two fixes were weighed: give the blocker the priority of what it blocks, or keep the labels and
let `next-task` and `milestone-exit` follow the clauses to an effective priority. The second
keeps a label that says less than the item's real weight, and puts the same walk over the
clauses in two skills. The maintainer chose the first (2026-10-30).

## Decision

1. **A blocker has at least the priority of what it blocks.** An item that another item's
   `Blocked on:` clause names has at least the priority of the highest item that names it. It is
   raised when the clause is written, and the section is kept sorted.
2. **It is judged on its own again when nothing needs the higher level.** When the items it
   blocks are closed, lowered or no longer name it, the blocker's priority is judged on its own
   merits again.
3. **Development-process items raise only their own kind.** A development-process item is P1
   whatever the milestone and is not part of a milestone's exit. A product item it waits on
   keeps the priority it has on its own, so the milestone count does not take in work that only
   the process needs. A development-process item it waits on is P1 already.

## Consequences

- `next-task`, `milestone-exit` and the loop's cap need no change: the label carries the weight.
- A blocker of a milestone item counts toward the milestone, as its work is on the milestone's
  path.
- A chain of blockers is raised along its length, one clause at a time.
- A product item that a development-process item waits on is still below the loop's P1 cap and
  is not offered by it.
