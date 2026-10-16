---
name: milestone-exit
description: Track the current nekoguruma milestone to its exit and close it. Count the open P1 items per exit criterion with the change since the last count (status), sweep the P2/P3 items for work the milestone should still take (sweep), or verify the exit criteria and move the plan and backlog to the next milestone (close). Use when the maintainer asks how far the milestone is, what is left, whether anything else belongs in it, or to finish it.
argument-hint: "status | sweep | close"
---

# Milestone exit

The development plan and the backlog files are in `work/` (file table in `work/README.md`). The
plan's section for the current milestone gives its goal, scope and exit criteria; the
project-wide backlog's Status section names the current milestone. Arguments: $ARGUMENTS
(default `status`).

Work from the latest `main` (`git fetch origin main`) and the open pull requests, and give the
commit you read in every report. Each mode reports; only `close`, and a `sweep` the maintainer
answered, change files.

## Counting rules (every mode)

Count the same way every time, so the numbers of two reports can be compared:

- **Milestone P1**: every `- **P1**` item in the backlog files outside the project-wide
  backlog's "Development process" section, plus every entry under a `Known Flaky Tests` section
  (`work/README.md`). Items with a `Blocked on:` clause count too; show the blocker.
- **Development-process items** are P1 whatever the milestone and are not part of its exit
  (`work/README.md`). Report them on a separate line, never inside the milestone count.
- **P0** items are reported first, whatever they concern.
- Count with a command, not by eye, for example
  `awk 'FNR==1{s=""} /^## /{s=$0} /^- \*\*P1\*\*/{print FILENAME" | "s}' work/*backlog.md | sort | uniq -c`
  plus the flaky entries; then read the items themselves.

## status

1. Count as above and assign each item to the exit criterion it serves, or to "on the path"
   when it serves the goal or scope without one criterion.
2. **Change since the last report.** If an earlier count exists (in the conversation, the
   thread, or the last PR that changed the backlog), list what was closed (with the merged PR),
   what was added, and what was split. A count that grew because items were found or split is
   said in one sentence, so it does not read as lost progress.
3. **In progress**: open PRs that close a milestone item, with their state (draft, CI, review).
4. Report, leading with the count and the commit: per exit criterion the remaining items in a
   short line each, then work in progress, then development-process items. When the milestone
   count is zero, say that the items are done but the milestone is not yet closed, and propose
   `sweep` (or `close` when a sweep since the last raise found nothing).

## sweep

One pass over every P2 and P3 item in both backlog files, plus a search for work no item
records. An item belongs in the milestone when one of these holds; note which one:

1. **A promise in the plan.** The milestone's goal, scope or exit criteria name it (a fault
   kind, a target, a test), and neither code nor a P1 item covers it. Check the code; a promise
   with no item at all becomes a new item.
2. **A residual of this milestone's work.** Items added since the milestone became current,
   above all those added since the previous sweep: `git log -p` on the backlog files from the
   commit that named the milestone in the Status section shows them. Fixes leave residuals, and
   the residual of a safety fix usually carries the same reason as the fix.
3. **Safety or a design violation in code the milestone added**: a state the design forbids
   (cite the section), a missing terminal state, a P0-type harm (`work/README.md`).
4. **A verification gap**: the milestone's code or tests that CI does not run (an excluded
   crate, a release-only path, a platform or target left out), or a path the exit criteria rely
   on with no test.

Everything else stays at its priority; give the reason once per group (later milestone, server
or hardware work, blocked on a later milestone). Report two lists, **recommended** and
**optional**, each item with its file, line, current priority and the reason above, then ask the
maintainer which to raise, offering: recommended only (the recommendation), recommended and
optional, or none. Change nothing before the answer. Raise the chosen items with the `backlog`
skill (new items for promises with no item) in one backlog-only PR (no automated review, ADR-251).

After the raised items are done, the next sweep only needs rule 2 for items added since this
one, plus rules 1, 3 and 4 for code and CI changes merged since (a fix can add a platform or
release-only path that CI does not run). The milestone is ready to close when a sweep
finds nothing.

## close

Run only when the maintainer asks to finish the milestone. The milestone count is zero and the
last sweep found nothing; if a sweep has not run since the last raised items merged, run it
first and stop if it finds something.

1. **Evidence for each exit criterion**, not only closed items: the test or CI job that shows
   it, in a successful CI run on `main` for the commit the report names (job, run, the log line
   that says it passed). If that run is still going, use an earlier green run only after
   checking that the commits between it and the reported one cannot affect the criterion, and
   never past a newer failure of the same job. Note jobs that run only on `main` and not on
   pull requests. A criterion that the plan says needs real hardware (design 13.5, ADR-275)
   is shown by the recorded hardware run instead (what ran, on which equipment, the result). A
   criterion without evidence becomes a P1 item and the close stops.
2. **Plan.** Delete the milestone's section. Carry over to the next milestone's section
   anything that still applies (optional hardware checks, notes on values still to be checked).
   Update the overview table and the order-of-work paragraph (the finish date). Move any lasting
   decision into `docs/system-architecture.md` or an ADR.
3. **Status.** Name the next milestone in the project-wide backlog's Status section with the
   date and the maintainer's decision, summarize its goal, and bring the "where the crates
   stand" list up to date.
4. **Re-check P1 and P2 against the next milestone** (`work/README.md`). Go through its scope
   and exit criteria one by one: an item on the path becomes P1, a P1 off the path becomes P2,
   a piece with no item becomes a new item, an item holding several pieces is split, and an item
   that waits on another gets a `Blocked on:` clause. Leave development-process items as they
   are.
5. Run `scripts/check-backlog.sh` and `scripts/check-work-refs.sh`, open one draft PR with the
   plan and backlog changes, and run `pr-review-loop` (the plan is not a backlog file, so
   it gets the automated review). The PR description names the raised, lowered and added items by their
   wording, and the next milestone's P1 count per file. The maintainer merges.
