---
name: milestone-exit
description: Track the current nekoguruma milestone to its exit and close it. Count the open P1 items per exit criterion with the change since the last count (status), sweep the P2/P3 items for work the milestone should still take and its blocked P1 items for work it cannot unblock (sweep), check that the exit criteria of the current and later milestones cover the planned work and the open P2 items (coverage), or close it (close), which sweeps what changed since the last sweep, then verifies the exit criteria and moves the plan and backlog to the next milestone. Use when the maintainer asks how far the milestone is, what is left, whether anything else belongs in it or in a later milestone, or to finish it, and in the weekly re-evaluation of the development plan.
argument-hint: "status | sweep | coverage | close"
---

# Milestone exit

The development plan and the backlog files are in `work/` (file table in `work/README.md`). The
plan's section for the current milestone gives its goal, scope and exit criteria; the
project-wide backlog's Status section names the current milestone. Arguments: $ARGUMENTS
(default `status`).

Work from the latest `main` (`git fetch origin main`) and the open pull requests, and give the
commit you read in every report. Each mode reports; only `close`, and a `sweep` or `coverage`
the maintainer answered, change files.

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
   `close`, which starts with a sweep (ADR-413). When every milestone item left has a
   `Blocked on:` clause, say so and propose `close` too: its sweep judges whether each one
   still belongs in the milestone, and it stops there with the questions.

## sweep

`close` starts with this sweep, so the maintainer does not have to ask for it separately
(ADR-413); asked for on its own, it shows the candidates earlier, while milestone items are
still open. Every sweep report names the commit it read and whether it found anything; the next
sweep, and the one `close` starts with, takes that commit as its base. Look for it where
`status` looks for an earlier count; the backlog PR a sweep opens and the PR `close` opens name
it in their descriptions.

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
or hardware work, blocked on a later milestone).

Then the other direction: every milestone P1 item with a `Blocked on:` clause (not the
development-process items or the `Known Flaky Tests` entries, which are P1 whatever the
milestone). A blocked item stays P1 only while this milestone can unblock it (`work/README.md`,
ADR-407). Judge each by what its clause waits for:

- **An exit criterion's item.** It is never moved out. Report it as a question for the
  maintainer: unblock it during this milestone, or change the exit criteria in the plan.
- **Another P1 item**, or a chain of them, that this milestone works on: it stays P1 (its
  blocker is P1 already, ADR-388).
- **Something the maintainer can supply** (a decision, a purchase, hardware, data): a question
  for the maintainer, supply it during this milestone or move the item out, with a
  recommendation.
- **Anything else**: a later milestone's work, or something outside the project (an
  operating-system facility, a vendor). Recommend moving it to P2; its clause names what
  unblocks it.

Report the raise candidates as two lists, **recommended** and **optional**, and the move-out
candidates and questions as a third, each item with its file, line, current priority and the
reason above. Then ask the maintainer which to raise, offering: recommended only (the
recommendation), recommended and optional, or none; and which to move out, offering the
recommended ones or none. Change nothing before the answer. Raise and lower the chosen items with
the `backlog` skill (new items for promises with no item); lowering an item re-judges the blockers
that were raised only for it (ADR-388). Put the changes in one backlog-only PR (no automated
review, ADR-251), unless a moved-out item is promised in the milestone's scope: then the same PR
moves that promise to the section of the milestone that unblocks it, or to the plan's list of
open decisions when no milestone does, and the plan change gets the automated review. The PR
description names the sweep's commit.

After the raised items are done, the next sweep only needs rule 2 for items added since this
one's commit, plus rules 1, 3 and 4 for code and CI changes merged since (a fix can add a
platform or release-only path that CI does not run). Such a **sweep of the changes** still
judges every blocked milestone P1 item, since a clause can change without the item moving.
When no earlier sweep report and its commit can be found, the sweep covers every item. The
milestone is ready to close when a sweep on the commit `close` reports finds nothing.

## coverage

Open work belongs to a milestone through that milestone's exit criteria. These are written as
goals that cover a group of items, never as a tag on each item (ADR-464). This mode checks
whether the exit criteria of the current milestone and of every later one cover the work. It
also runs in the weekly re-evaluation of the development plan, after `status`.

**What it reads.** The first run, and any run with no earlier coverage report to start from,
reads every P2 item in both backlog files and the whole plan. A later run starts from the
commit the last coverage report names. Look for that report where `status` looks for an
earlier count, and in the coverage pull request. It then reads:

- the items added or changed since that commit (`git log -p` on the backlog files);
- the plan changes since that commit;
- the items those plan changes bear on.

P3 items (nice to have) are not judged. Development-process items are P1 whatever the
milestone and are outside every exit, so leave them out. Say that once when they come up.

**What it finds.**

1. **A goal without a criterion.** A milestone's goal or scope states something that none of
   its exit criteria checks. Propose a criterion that checks it, or an extension of an existing
   one.
2. **Work no milestone covers.** A P2 item, or a group of them, that no milestone's goal, scope
   or exit criteria covers. Group items by the goal they would serve, not one by one. Then
   either propose a criterion for the milestone whose goal fits, saying why that milestone, or
   say the group stays milestone-independent and why. An example of the latter is quality work
   that no goal needs. Name a group the maintainer already left milestone-independent in an
   earlier coverage run once, and do not propose it again.
3. **Covered but not raised.** In the current milestone, an item that an exit criterion covers
   but that is still below P1. This is rule 1 of the sweep. Report it as a raise candidate.

Write each proposed criterion as a goal a reviewer can check, in the plan's style. Mark it
"added at the maintainer's request" with the date only once the maintainer chooses it. Cite
the items it covers by file and line, at the commit the report names.

**Reporting and applying.** Report the proposals as two lists, **recommended** and
**optional**, and the milestone-independent groups as a third. Each entry names its milestone,
the criterion's wording or the extension, and the items it covers. Ask the maintainer which to
adopt: the recommended ones, the recommended and optional ones, or a choice of their own.
Change nothing before the answer.

Put the chosen criteria in the development plan. In the same pull request, raise to P1 the
items that the current milestone's new or extended criteria cover, with the `backlog` skill.
Give each raised item a `Milestone:` clause naming its criterion. Items for later milestones
stay P2: the P1/P2 re-check at that milestone's change raises them (`work/README.md`). Run
`scripts/check-backlog.sh` and `scripts/check-work-refs.sh`, open one draft PR, and run
`pr-review-loop`. The plan is not a backlog file, so the PR gets the automated review. The PR
description names the commit the coverage read and the groups left milestone-independent.

## close

Run only when the maintainer asks to finish the milestone. A sweep is part of `close`, and the
maintainer never has to ask for one first: an earlier sweep's result can be out of date by the
time the maintainer asks, since items and code merged after it are not in it (ADR-413).

1. **Count and sweep** on the latest `origin/main`, and name that commit; every later step
   reports on the same commit.
   - Milestone items left and not all blocked: report as `status` does and stop. The milestone
     is not finished.
   - Otherwise run the sweep: a sweep of the changes since the last sweep report's commit, or
     every item when none is found. It finds something when it has a raise candidate, a
     move-out candidate or a question, which includes every milestone item left, since all of
     them are blocked. Report as `sweep` does, ask the same questions, and stop: the close
     goes no further in this run. The maintainer's answer is applied as `sweep` applies it,
     and once those changes and any raised items are merged, the maintainer asks for `close`
     again, which sweeps the changes since this sweep's commit.
   - The sweep finds nothing: say so with its commit and go on. A blocked item that serves an
     exit criterion keeps the milestone open until the maintainer unblocks it or changes the
     exit criteria; the sweep reports it as a question, so the close never gets past it.
2. **Evidence for each exit criterion**, not only closed items: the test or CI job that shows
   it, in a successful CI run on `main` for the commit the report names (job, run, the log line
   that says it passed). If that run is still going, use an earlier green run only after
   checking that the commits between it and the reported one cannot affect the criterion, and
   never past a newer failure of the same job. Note jobs that run only on `main` and not on
   pull requests. A criterion that the plan says needs real hardware (design 13.5, ADR-275)
   is shown by the recorded hardware run instead (what ran, on which equipment, the result). A
   criterion without evidence becomes a P1 item and the close stops.
3. **Plan.** Delete the milestone's section. Carry over to the next milestone's section
   anything that still applies (optional hardware checks, notes on values still to be checked).
   Update the overview table and the order-of-work paragraph (the finish date). Move any lasting
   decision into `docs/system-architecture.md` or an ADR.
4. **Status.** Name the next milestone in the project-wide backlog's Status section with the
   date and the maintainer's decision, summarize its goal, and bring the "where the crates
   stand" list up to date.
5. **Re-check P1 and P2 against the next milestone** (`work/README.md`). Go through its scope
   and exit criteria one by one: an item on the path becomes P1, a P1 off the path becomes P2,
   a piece with no item becomes a new item, an item holding several pieces is split, and an item
   that waits on another gets a `Blocked on:` clause. Leave development-process items as they
   are.
6. Run `scripts/check-backlog.sh` and `scripts/check-work-refs.sh`, open one draft PR with the
   plan and backlog changes, and run `pr-review-loop` (the plan is not a backlog file, so
   it gets the automated review). The PR description names the commit and the result of step
   1's sweep, the raised, lowered and added items by their wording, and the next milestone's P1
   count per file. The maintainer merges.
