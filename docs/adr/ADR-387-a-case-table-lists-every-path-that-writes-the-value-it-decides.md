# ADR-387: A Case Table Lists Every Path That Writes the Value It Decides

**Date:** 2026-10-30
**Status:** Accepted
**Affects:** `.claude/README.md` ("Gates for the analysis agents", the `design-advisor` case
table); ADR-381 item 1

## Context

ADR-381 asks `design-advisor` for a table of every combination of states and events before a
gated change is implemented. Its rows are cases of input: which times an item has, where an
error falls, what the exchange already holds.

In the communication-log pull requests since, the findings that kept a review going were of
another kind. The rule a change set was right, but a second path that writes the same value did
not follow it:

- The rule that a late answer counts since a late link error only when its time is shown after
  the error (ADR-384 item 2) was first applied where a late response settles. Review then found,
  one round at a time, that a late start reused an earlier pending's answer with the start's
  time, that a pending with no placed time took a later item's time, that the primitive's end
  had its own copy of the choice, that an answer freed through its owners was counted again, and
  that the recount a late error starts counted by position. The table had a row per input case
  and none per writing path; the row naming the settling paths was added after review found
  them. The recount was cut into a pull request of its own (ADR-385), which started from one
  test that every path calls and drew far fewer findings.
- The `vci_loss` record was rewritten by one settling entry and left stale by the others, until
  review named them (ADR-386 item 6).

Every input row of such a table can be right while one writer of the value still breaks it.

## Decision

When the rule a change sets decides a value that more than one path writes or changes (a count,
a flag, a classification, a stored time, a record), the case table also lists each of those
paths as a row: where it writes the value and what it writes under the rule, or why the path is
out of scope. The paths are found in the code before the table is written, by searching for
every write of the value. The other rows, and the rest of ADR-381, are unchanged.

## Consequences

- A rule enforced in one place and bypassed in another shows in the table before the first
  review, where it can be met by one shared function, as ADR-385 did, instead of a fix per path.
- The table grows by one row per writer. For a value with one writer it adds nothing.
- A writer the search misses (one reached through a helper with another name) still gets
  through, as before.
