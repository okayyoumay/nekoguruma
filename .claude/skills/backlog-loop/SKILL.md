---
name: backlog-loop
description: Work through the nekoguruma backlog one item at a time. Pick the next unblocked item, implement it in one pull request, run pr-review-loop, hand the PR to the maintainer, and start the next item only after the maintainer merges it. Run only when the maintainer asks for it.
disable-model-invocation: true
argument-hint: "[max items, default 3] [lowest priority to include: P1 (default) or P2]"
---

# Backlog loop

One iteration is one backlog item and one pull request. Iterations run strictly one after
another: the next one starts only after the maintainer has merged the previous PR. Claude never
merges, and never works on two loop items at once.

Arguments: $ARGUMENTS. The first is the number of items for this run (default 3); the second is
the lowest priority to include: P1 (default: P0, P1 and the `Known Flaky Tests` entries) or P2.
P3 items are not worked by the loop.

A run starts only when the maintainer asks for one, or from the previous iteration's hand-off
(step 6). Never schedule a recurring trigger for it.

## Run state

The run's state is: items left, the priority cap, and three lists (merged PRs with their shadow
risk verdicts, skipped items with the reason, and backlog-only PRs). Keep it in a "Loop run"
section of the current loop PR's description and update it at every step that changes it, so it
survives a compacted conversation or a fresh session.

## 1. Check before each iteration

Stop the run and go to "Final report" if any of these holds:

- no items are left;
- the latest CI run on `main` failed (a red `main` is P0 work, outside the loop);
- an open pull request carries the `backlog-loop` label (one item at a time: wait for it);
- the previous loop PR was closed without being merged (ask the maintainer why before going on).

Then start the branch from the latest `main`.

## 2. Pick

Run `next-task`. Leave out of the candidates every item on the run's skipped list, and every item
below the priority cap: this applies to the pick and to the alternative alike. Take the first of
the pick and the alternative that passes these checks:

- **Blocked.** It has a `Blocked on:` clause. If `next-task` reports the blocker as resolved,
  verify that, remove the clause with the `backlog` skill, and take the item; otherwise skip it.
- **Too big.** It does not fit in one pull request: record the split with the `backlog` skill and
  skip it.
- **Stale.** `next-task` reports it as already done: close it with the `backlog` skill and skip it.

When neither passes, run `next-task` again; skipped items are now left out, so it moves on. Stop
the run when it returns nothing within the cap.

## 3. Claim the item

Commit the backlog edits so far (see "Backlog edits") or, if there are none, a commit that only
starts the work, push, and open the draft PR right away (step 5's first paragraph). The open
labelled PR is what tells any other session that an item is in progress.

## 4. Implement

Work the item as any other task, following CLAUDE.md and `.claude/README.md` (task size, agents,
documentation sync, ADRs). Close the item with the `backlog` skill in the same PR.

If the work shows that the item needs something the loop cannot supply, do not guess. Add a
`Blocked on:` clause to the item with the `backlog` skill, using the forms in `work/README.md`
(`Blocked on: the maintainer's decision on ...` for a design choice that the specs and ADRs
leave open), and put the item on the skipped list. Commit that edit, then revert the other
changes of this iteration with a new commit (no history rewrite), turn the PR into a
backlog-only PR (retitle it; it keeps the label), take it through step 5, and count it on the
backlog-only list. A skipped item does not count against the items left. The run is unattended,
so follow `unattended-clarification` rather than waiting for an answer.

### Backlog edits

Edits to the backlog made while picking (a stale item closed, a split recorded, a blocker added
or removed) are committed on the branch and go into this iteration's PR. If the run stops before
a PR carries them, open a backlog-only loop PR with them and take it through step 5 before the
final report, so a later run does not pick the same items again. Never do this while another
labelled PR is open; in that case there are no such edits, because step 1 runs before any pick.

## 5. Pull request

The PR is a draft with the `backlog-loop` label. Create the label first if the repository does
not have it, and check that the PR carries it; without it, step 1 cannot see the open item.

Run `scripts/classify-pr-risk.sh` and copy its output into the PR description under a
"Risk (shadow)" heading. The verdict is recorded only to compare it later with the maintainer's
own judgement; it changes nothing about who merges. If the script exits with status 2, write
"not classified" and the reason it printed. Then run `pr-review-loop`. Before it hands the PR
to the maintainer, run the classifier again on the final head and replace the section if the
verdict or its reasons changed.

## 6. Wait for the merge, then continue

End the turn. While the PR is open, any wake other than its merge or close (a review, a CI
result, a comment) is handled by `pr-review-loop`, and the turn ends again; it does not start
step 1.

When the PR is merged, decrease the items left (a backlog-only PR does not count), copy the run
state forward, and go to step 1. When it is closed without merging, go to step 1, which stops
the run.

Each item should start with a small context. If the session can start a fresh session for the
next iteration (for example a project coordinator that opens a new thread), hand off there with
the command `/backlog-loop <items left> <cap>` followed by the run state. Otherwise continue in
this session.

## Final report

One message to the maintainer:

- the PRs merged in this run, each with its shadow risk verdict, and any backlog-only PRs
  opened (merged or still open);
- the items skipped and why;
- everything that waits on the maintainer: every backlog item whose clause begins
  `Blocked on: the maintainer` (a decision, a purchase, a search, a setting), plus the items
  blocked on hardware the maintainer could supply. Write each decision as a question that can be
  answered in a word, with a recommendation;
- why the run stopped (no items left, nothing unblocked within the cap, `main` red, a PR open or
  closed without merging).
