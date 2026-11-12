# ADR-464: Open Work Is Tied to a Milestone by Its Exit Criteria

**Date:** 2026-11-12
**Status:** Accepted
**Affects:** `.claude/skills/milestone-exit` (new `coverage` mode), `work/README.md`
(priorities), the development plan in `work/`, `CLAUDE.md` and `.claude/README.md` (skill
summaries), the weekly plan re-evaluation run

## Context

Most P2 items in the backlog name no milestone. A survey on 2026-11-12 found 191 of 241 P2
items without one. Some of that work was promised by a milestone's goal or scope, but no exit
criterion checked it: M3's communication-log upload, M6's journal encryption and VCI profiles,
and M7's OTX and ExVe. Other work appeared in no milestone at all, such as the server's
ingestion of definition packages. Nothing in the process looked for either case.
`milestone-exit`'s sweep looks only at the current milestone. The P1/P2 re-check runs only
when a milestone changes, and it reads only the next milestone's scope. A gap in a later
milestone's exit criteria therefore stayed hidden until that milestone began, and work that no
milestone covered was never found at all.

Per-item milestone tags were considered on 2026-10-13 and rejected. The maintainer decided on
2026-11-12 to tie work to milestones through their exit criteria instead: written as goals
that cover a group of items, not as one criterion per item. The maintainer also asked for this
check to run in the weekly re-evaluation of the development plan.

## Decision

1. **Exit criteria tie the work.** Open work belongs to a milestone when one of that
   milestone's exit criteria in the development plan covers it. Criteria are written as goals
   a reviewer can check, and each one covers a group of items. Backlog items still carry no
   milestone tag. A P1 item of the current milestone names the criterion it serves, in its
   `Milestone:` clause. Items of later milestones stay P2 until their milestone becomes
   current. The re-check of `work/README.md` then raises them through the criteria.
2. **`coverage` mode.** `milestone-exit coverage` compares the plan with the backlog for the
   current milestone and every later one. It reports:
   - goals or scope a milestone states that no exit criterion checks;
   - P2 work that no milestone's goal, scope or exit criteria covers. For each group it
     proposes a new or extended criterion for the milestone whose goal fits. Otherwise it
     says the group stays milestone-independent, and why;
   - for the current milestone, any item that a criterion covers but that is still below P1.
     These are raise candidates, as rule 1 of the sweep finds them.

   P3 items (nice to have) are not judged. Development-process items are P1 whatever the
   milestone and are outside every milestone's exit, so the mode leaves them out.
3. **The maintainer decides.** `coverage` reports and changes nothing. The proposals go to the
   maintainer with a recommendation each, as recommended and optional. The chosen ones go into
   one pull request: the plan change and, for the current milestone, the raised items. That
   pull request gets the automated review, because the plan is not a backlog file (ADR-251).
4. **Coverage of the changes.** Every coverage report names the commit it read. A later run
   covers only:
   - the items added or changed since that commit;
   - the changes to the plan since that commit;
   - the items those plan changes bear on.

   With no earlier report, it covers every P2 item.
5. **Weekly run.** The weekly re-evaluation of the development plan runs `coverage` (of the
   changes) after `status`. Its proposals reach the maintainer as decisions. It never edits the
   plan before the maintainer answers.

## Consequences

- Planned work no exit criterion checks is found within a week of being written, not when its
  milestone begins.
- The first coverage run on the plan as of 2026-11-12 was done by hand. It added nine new and
  six extended exit criteria across M3 to M8, and it raised the five M3 items that the new
  criteria need. Weekly runs then cover only the changes.
- Many items stay milestone-independent by choice. Examples are the quality items in the
  worker crates and the extensions to the authoring language. Each coverage run names these
  groups once instead of proposing them again. The maintainer can change that answer at any
  time.
- Exit criteria grow with the work they cover. A criterion that becomes too broad to check is
  split when it is written, as items are.
