---
name: backlog
description: Add, update or close an item in the nekoguruma backlog (the backlog files in work/). Use whenever work is deferred (a follow-up, a residual, a test gap, an "out of scope for this PR" note), when an item is finished, or when the user asks to record, re-prioritize or remove an open item.
argument-hint: "add|close|update <description>"
---

# Backlog: add, update, close

The backlog lives in `work/` (temporary material). The format, the priority
scale and the list of files are in `work/README.md`; read its "Backlog
format" section first. Permanent files never name a backlog file or item.

## Add an item

1. **Pick the file and section.** Worker crates (`iso22900*`, `j2534-0404*`,
   `vci-service-*`): the crate's `### Prioritized Backlog` in the worker-crate
   backlog file. Everything else: the matching `##` area in the project-wide
   backlog file. Create a section only when no existing one fits.
   A test that fails intermittently goes under the crate's `### Known Flaky
   Tests` instead (test, symptom, how to confirm in isolation, known cause);
   those entries count as P1.
2. **Check for duplicates.** Search both backlog files for the identifiers
   involved (function, file, ComParam, design section, ADR). If an item
   already covers it, update that item instead of adding a second one.
3. **Write the item** as one top-level bullet:
   `- **P<n>**: what is missing. Where (paths, design section, ADR). Done when: ...`
   - Self-contained: cite sources directly, never "see above".
   - Give the evidence that made you record it (a failing case, a review
     finding, a code location), so a later session can verify it quickly.
   - No spec text verbatim; cite the clause and paraphrase.
4. **Choose the priority** from the table in `work/README.md`, against the
   current milestone named in the project-wide backlog's Status section.
   Priority is how much it matters once it can be done: if it waits on a
   decision, a vendor, real data or another item, add a `Blocked on:` clause
   instead of lowering the priority. P0 is only for real harm (ECU or
   vehicle damage, lost write or journal state, a broken trust boundary) or
   work that stops everything else. A development-process item (backlog
   and priority rules, documentation and ADR conventions, `CLAUDE.md`,
   `.claude/` agents, skills and rules, review guidelines; defined in
   `work/README.md`) is P1 whatever the milestone, unless it meets P0, and
   goes in the project-wide backlog's "Development process" section. When unsure between two levels, pick the
   higher one and say so in your report.
5. **Place it** in priority order within the section.
6. Run `scripts/check-backlog.sh`.

## Update an item

Re-prioritize, sharpen the "Done when", or add new evidence in place. Move
the item if its priority changes so the section stays sorted. Do not append
a dated history of changes; the item states the current understanding.

## Close an item

When the work is done (in this PR or found already done):

1. **Read the whole item.** If part of it is still open (a residual, a
   deferred sub-case, a missing test), write that part as a new
   self-contained item before deleting this one.
2. **Move lasting decisions** into the permanent document they belong to
   (`docs/system-architecture.md`, a crate's docs, an ADR). Do this in the
   same PR as the code change.
3. **Delete the item.** No strikethrough, no "done" mark, no "resolved"
   list.
4. **Fix references** to it. Search `work/` for the item's identifiers
   (other items may depend on it; search line-wrap aware, e.g.
   `rg -U` or flatten whitespace first). Permanent files must not
   reference backlog items, but check them too in case one slipped in.
5. **Remove empties.** Delete a section with no items left, and a file with
   no sections left; then update the file table in `work/README.md`.
6. Run `scripts/check-backlog.sh` and `scripts/check-work-refs.sh`.

## In a pull request

Before marking a PR ready, every follow-up deferred during the work must be
either done in the PR or added here. Mention added or closed items in the PR
description by their wording (backlog items have no IDs).
