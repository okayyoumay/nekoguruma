# ADR-413: The Milestone Close Starts with a Sweep

**Date:** 2026-11-03
**Status:** Accepted
**Affects:** `.claude/skills/milestone-exit` (`status`, `sweep`, `close`),
`.claude/skills/backlog-loop` (final report), `work/README.md` (priorities), `CLAUDE.md` and
`.claude/README.md` (skill summaries)

## Context

`milestone-exit` closes a milestone in two requests from the maintainer: `sweep` first, then
`close`. `close` ran a sweep again only when raised or lowered items had merged since the last
one. When a sweep found nothing and the maintainer asked for `close` days later, new P2/P3
items (residuals of fixes) and code or CI changes merged in between were in no sweep, and the
close went ahead on the old result. The P1 count and the exit evidence were current; the sweep's
part was not.

The maintainer asked whether the backlog loop should run the sweep itself when the milestone
count reaches zero. Weighed against that:

- the loop's session is long by then, and a full sweep reads every P2/P3 item (hundreds), so a
  missed candidate is more likely there than in a fresh session, and an unrequested "nothing
  found" would become the condition for the close;
- its result goes stale in the same way while it waits for the maintainer's answer;
- the loop's final report is one message, and three candidate lists would bury its questions;
- each run that stops with the count at zero would repeat the sweep.

Moving the sweep into `close` avoids all four: it runs in the session the maintainer starts, on
the same commit the close reports on, only when the maintainer asks to finish. The maintainer
chose this (2026-11-03).

## Decision

1. **`close` starts with a sweep** on the latest `origin/main`, the commit every later step of
   the close reports on. The maintainer never asks for a sweep first.
2. **A sweep of the changes.** The sweep covers rule 2 for the items added since the last sweep
   report's commit and rules 1, 3 and 4 for the code and CI changes merged since, and judges
   every blocked milestone P1 item. With no earlier sweep report, it covers every item. Every
   sweep report, the backlog PR a sweep opens and the PR `close` opens name the sweep's commit,
   so the next sweep finds its base.
3. **Stopping.** With milestone items left that are not all blocked, `close` reports as `status`
   does and stops. When the sweep finds a raise candidate, a move-out candidate or a question,
   `close` asks the sweep's questions and stops; the answer is applied as `sweep` applies it,
   and the maintainer asks for `close` again once those changes are merged. Only a sweep that
   finds nothing lets the close go on to the exit evidence.
4. **Who recommends it.** `status` and the backlog loop's final report recommend `close` when
   the milestone count is zero or every milestone item left is blocked. The loop still never
   runs `sweep` or `close`. `status` stays the default mode, and `close` runs only when asked
   for by name.
5. **`sweep` on its own** stays, for seeing the candidates while milestone items are still open.

## Consequences

- The maintainer's steps to finish a milestone are asking for `close`, answering its questions,
  merging the pull requests and recording any hardware run; asking for the sweep and judging
  which sweep result is current are gone.
- A close that stops on a finding does not count as a close; the next `close` sweeps only what
  changed after that sweep's commit, so the repeat is short.
- ADR-407 item 4 is amended: `status` proposes `close` (which starts with the sweep) instead of
  `sweep` when every milestone item left is blocked, and `close` accepts that state to run the
  sweep and stop with its questions; the zero count still holds for the close to go past the
  sweep, since the answers move or unblock every item left.
