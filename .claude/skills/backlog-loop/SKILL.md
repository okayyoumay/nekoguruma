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
- an open pull request carries the `backlog-loop` label and it is not this run's current PR
  (one item at a time). If it is this run's current PR (named in its "Loop run" section, for
  example after a fresh session picked up the hand-off), go to step 6 for it instead;
- this run's previous loop PR (from the run state) was closed without being merged. Put its
  item on the skipped list so the run does not pick it again, and report the close;
- this run has opened three backlog-only PRs (step 4), so the remaining items need the
  maintainer more than the loop.

Then fetch `origin main` and bring the branch you were given up to date by merging `main` into
it. Do not reset it or force-push: both need approval, and the run is unattended.

## 2. Pick

Run `next-task` with an argument that names the priority cap and lists the items on the run's
skipped list, asking it to leave those out (`next-task` sees nothing but its argument). Still
check its answer: leave out every skipped item and every item below the cap, for the pick and
the alternative alike. Take the first of
the pick and the alternative that passes these checks:

- **Blocked.** It has a `Blocked on:` clause. If `next-task` reports the blocker as resolved,
  verify that, remove the clause with the `backlog` skill, and take the item; otherwise skip it.
- **Too big.** It does not fit in one pull request: record the split with the `backlog` skill and
  skip it.
- **Stale.** `next-task` reports it as already done: close it with the `backlog` skill and skip it.

When neither passes, run `next-task` again with the longer skipped list. Stop the run when it
returns nothing eligible, or when five calls in a row gave nothing eligible.

## 3. Claim the item

Commit the backlog edits so far (see "Backlog edits") or, if there are none, an empty commit
(`git commit --allow-empty`) naming the item, push, and open the draft PR right away (step 5's
first paragraph). The open labelled PR is what tells any other session that an item is in
progress.

Then list the open `backlog-loop` PRs again. If another one was opened before this one (two runs
started together), close this one with a comment naming the other, and stop the run.

## 4. Implement

Work the item as any other task, following CLAUDE.md and `.claude/README.md` (task size, agents,
documentation sync, ADRs). Close the item with the `backlog` skill in the same PR.

If the work shows that the item needs something the loop cannot supply, do not guess. Add a
`Blocked on:` clause to the item with the `backlog` skill, using the forms in `work/README.md`
(`Blocked on: the maintainer's decision on ...` for a design choice that the specs and ADRs
leave open), and put the item on the skipped list. Commit that edit, then revert this
iteration's changes other than backlog edits with a new commit (no history rewrite), turn the PR into a
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

Run `scripts/classify-pr-risk.sh` (after the fetch in step 1) and copy its output into the PR description under a
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
- everything that waits on the maintainer: every backlog item whose `Blocked on:` clause
  names the maintainer anywhere, not only at its start (a decision, a purchase, a search, a
  setting), plus the items
  blocked on hardware the maintainer could supply. Write each decision as a question that can be
  answered in a word, with a recommendation;
- why the run stopped (no items left, nothing unblocked within the cap, `main` red, another loop
  PR open, a PR closed without merging, or three backlog-only PRs).
