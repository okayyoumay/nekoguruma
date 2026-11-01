# ADR-407: A Blocked Item Stays P1 Only While the Milestone Can Unblock It

**Date:** 2026-11-01
**Status:** Accepted
**Affects:** `work/README.md` (priorities), `.claude/skills/milestone-exit` (`status`, `sweep`,
`close`), `.claude/skills/backlog` (update an item), `.claude/skills/backlog-triage` (blocked
items), `CLAUDE.md` and `.claude/README.md` (skill summaries)

## Context

P1 means needed for the current milestone. An item that waits on something keeps its priority
and gets a `Blocked on:` clause instead of being lowered, since priority is how much the item
matters once it can be done. `milestone-exit` counts every P1 item, blocked or not, and closes a
milestone only when that count is zero.

Nothing decided what happens when the only milestone items left are blocked on something the
milestone does not provide: a key that only a later milestone's server issues, or evidence that
the operating system may never give. Such an item serves none of the exit criteria, the
milestone cannot unblock it, and the count cannot reach zero. The sweep only raised P2/P3 items
into the milestone, never judged whether a P1 item still belonged in it, and `backlog-triage`
would give an item lowered because it was blocked its P1 back.

Two fixes were weighed: let `close` proceed with such items open, or move them out of the
milestone. The first leaves P1 labels on work the milestone does not need, so `status` and the
loop's P1 count say less than they seem to. The maintainer chose the second (2026-11-01), applied
by the sweep on the maintainer's answer, not automatically.

## Decision

1. **Membership, not ability.** A blocked P1 item that serves none of the milestone's exit
   criteria stays P1 only while the milestone can unblock it. This is a judgement of which
   milestone the item belongs to; the rule that a blocked item is not lowered because it cannot
   be done now still holds within the milestone.
2. **Judged by the blocker.** Another P1 item, or a chain of them, keeps the item P1 (its
   blocker is P1 already, ADR-388). Something the maintainer can supply (a decision, a purchase,
   hardware, data) is put to the maintainer, who supplies it during the milestone or moves the
   item out. Anything else (a later milestone's work, or something outside the project) moves
   the item to P2, and its clause names what unblocks it.
3. **Exit criteria are never moved out this way.** A blocked item that serves an exit criterion
   keeps the milestone open until the maintainer unblocks it or changes the exit criteria.
4. **The sweep proposes, the maintainer decides.** `milestone-exit`'s `sweep` judges the blocked
   P1 items alongside the P2/P3 items it may raise, and changes nothing before the maintainer's
   answer. `status` proposes the sweep when every milestone item left is blocked. `close` keeps
   its zero count precondition.
5. **Side effects of moving an item out.** The blockers raised only for it are judged on their
   own again (ADR-388 item 2). A promise of the item in the milestone's scope moves, in the same
   pull request, to the plan section of the milestone that unblocks it, or to the plan's open
   decisions when no milestone does; that pull request then gets the automated review.
6. **Coming back.** When a `Blocked on:` clause is removed, the item's priority is judged again
   against the current milestone, so a moved-out item returns to P1 once it is on that
   milestone's path. The P1/P2 re-check at each milestone close catches the items a later
   milestone unblocks.

## Consequences

- A milestone can close with blocked work open, but only as P2 with a clause that says what
  unblocks it; the milestone count stays the one condition for close.
- Development-process items and `Known Flaky Tests` entries are P1 whatever the milestone and
  are not judged by this rule.
- An item blocked on the maintainer is never moved out without the maintainer's answer, so a
  purchase or a decision the maintainer would make now does not silently leave the milestone.
