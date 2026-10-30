# ADR-381: Enumerate the Cases Before Coding, and Pause Long Code Review Loops

**Date:** 2026-10-29
**Status:** Accepted (item 1 extended by ADR-387: one row per path that writes the value the rule decides)
**Affects:** `.claude/README.md` ("Gates for the analysis agents"), `.claude/skills/backlog-loop/SKILL.md` (step 4), `.claude/skills/pr-review-loop/SKILL.md` (Steps 3 and 4), `CLAUDE.md` ("Pull requests"), `.github/copilot-instructions.md`; ADR-349 Decision items 1 and 4; ADR-251's Status note

## Context

ADR-349 found that a long specification review loop is driven by fixes that close exactly the
reported case and leave its neighbours open, so each fix opens the next finding. It added a pause
after eight rounds with findings, but only for specification pull requests.

The same pattern shows in code. Over 48 consecutive `backlog-loop` pull requests, 41 of the 64
review findings fell in four pull requests that changed the agent's communication-log
settlement: what happens to an exchange at job end, which exchange owns an answer once an earlier
one settles, how a time is placed on a clock across a suspend. Each round named one to three
further combinations of state and event that the code did not handle. One of those pull requests
took more than a dozen review requests; another stopped only when the maintainer cut its scope by hand. Pull
requests outside such logic mostly passed on the first or second review.

The checks the loop already runs (the sweep, `edge-case-hunter` before commit) look at code that
is already written, one gap at a time. Nothing lists the combinations before the code exists, so
the reviewer ends up doing that enumeration a round at a time, at the cost of a review request,
a CI run and a fix each.

## Decision

1. **A case table before code, when the change calls for one.** Before implementation,
   `design-advisor` writes a case table for a change that meets any of:
   - its outcome depends on combinations of states and events: settlement or attribution of
     results, clocks and time placement, concurrency, state machines, protocol timing;
   - its estimated change, tests aside, exceeds 300 lines;
   - the latest merged pull request that changed the same source files needed five or more
     review rounds with findings.

   The table lists each combination with its expected outcome, or marks it out of scope with the
   reason. The gate is the nature of the change, not a module: no crate is in or out by name.
2. **The table drives the work.** The implementation and its tests cover every in-scope row. The
   table goes into the pull request description under "Cases". The `edge-case-hunter` pass before
   commit gets the table and is asked for combinations missing from it; a missing row it finds
   is added to the table, not only fixed in code.
3. **ADR-349's pause applies to every pull request.** The count, the four-round blame check, the
   share above one half and the three options are unchanged, and the specification-only
   condition is dropped. On a code pull request, Rewrite means rebuilding the affected logic from
   a new case table for that part, as in item 1, instead of rewriting a section of text.

## Consequences

- A gated change costs one `design-advisor` call before coding, about 10 to 15 minutes. Applying
  it to every change would cost more than it saves, since most changes pass review early; the
  gate keeps it to the changes that produced the long loops.
- The reviewer still reviews; the table moves the enumeration it was doing round by round to
  before the first request. Combinations neither the table nor the reviewer thinks of still get
  through, as before.
- The third gate condition looks back at review history, so it catches an area whose difficulty
  the first two conditions missed, after one long pull request.
- A code pull request can now pause for the maintainer after eight rounds with findings, as a
  specification pull request already could. A loop with no maintainer present waits, as
  ADR-349 item 3 says.
