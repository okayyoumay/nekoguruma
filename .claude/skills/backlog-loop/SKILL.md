---
name: backlog-loop
description: Work through the nekoguruma backlog one item at a time. Pick the next unblocked item, implement it in one pull request, run pr-review-loop, hand the PR to the maintainer, and start the next item only after the maintainer merges it. Run only when the maintainer asks for it.
disable-model-invocation: true
argument-hint: "[max items, default 3] [lowest priority to include: P1 (default) or P2] | pr=<number of the PR to resume>"
---

# Backlog loop

One iteration is one backlog item and one pull request. Iterations run strictly one after
another: the next one starts only after the maintainer has merged the previous PR. Claude never
merges, and never works on two loop items at once.

Arguments: $ARGUMENTS. To start a run: the number of items for this run (default 3), then the
lowest priority to include: P1 (default: P0, P1 and the `Known Flaky Tests` entries) or P2. P3
items are not worked by the loop. To resume a run: `pr=<n>`, the run's current PR; everything
else comes from that PR's run state.

A run starts only when the maintainer asks for one, or from the previous iteration's hand-off
(step 6). Never schedule a recurring trigger for it.

## Run state

The run's state is: a run id (the UTC time the run started), the current PR number, items left,
the priority cap, and three lists (merged PRs with their shadow risk verdicts, skipped items with
the reason, and backlog-only PRs, the one that ends the run marked "final"). Keep it in a "Loop
run" section of the current loop PR's description and update it at every step that changes it,
so it survives a compacted conversation or a fresh session; a merged PR's description can still
be edited. That section is the only source of the state. A PR belongs to this run only when its
"Loop run" section carries this run's id.

A run resumes in a new session from the hand-off, or when the maintainer asks it to go on and
names the PR (`pr=<n>`). It reads the state from that PR, subscribes to the PR's activity, and
goes on by the PR's state. To work on an open PR it must be able to push to that PR's head
branch; if that is not the branch this session was given, tell the maintainer and stop instead.

- a draft still being worked returns to step 4, or to step 5 if the item is implemented;
- an open PR already handed to the maintainer goes to step 6;
- a merged PR whose merge is already in the merged list goes to step 1; one not yet recorded
  goes to step 6's merge handling;
- a closed, unmerged PR not marked "final" goes to step 1;
- a closed or merged PR marked "final" ends the run: write the final report if it was not
  written yet.

## 1. Check before each iteration

List the open loop PRs: those that carry the `backlog-loop` label, or whose title starts with
`[backlog-loop]` and whose head branch is in this repository (not a fork). Loop PRs always come
from branches here, and anyone can choose a fork PR's title. If one belongs to this run, resume
it as described under "Run state" instead of going on. If one belongs to another run, stop the
run and go to "Final report" (one item at a time).

Otherwise reset the branch's content to `main` without rewriting history (the run is
unattended, and a reset or force-push needs approval). This discards everything on the branch,
so first check that nothing else needs it: stop and tell the maintainer if `git status
--porcelain` prints anything, or if any open PR, labelled or not, has this branch as its head.
Then `git fetch origin main`. If `git diff --quiet HEAD origin/main` succeeds, the branch already
matches and there is nothing to do; otherwise record a merge of `origin/main` whose tree is
exactly `origin/main`'s:

```sh
git merge -s ours --no-commit origin/main
git read-tree -u --reset origin/main
git commit -m "Start from origin/main"
```

This cannot conflict, and it drops anything the branch still carries from a PR that was closed
unmerged. Every later step, including a stop, starts from this clean branch.

Then stop the run and go to "Final report" if any of these holds:

- no items are left;
- the latest CI run on `main` failed (a red `main` is P0 work, outside the loop);
- this run's current PR was closed without being merged. Add `Blocked on: the maintainer's
  reason for closing PR #<n>` to its item as a backlog edit (see "Backlog edits"), so no later
  run picks it again, and report the close;
- this run has opened three backlog-only PRs (step 4), so the remaining items need the
  maintainer more than the loop.

## 2. Pick

Run `next-task` with `cap=<cap>` and `skip=` followed by the items on the run's skipped list
(`next-task` sees nothing but its arguments). Still check its answer: leave out every skipped
item and every item below the cap, for the pick and the alternative alike. Take the first of the
pick and the alternative that passes these checks:

- **Blocked.** It has a `Blocked on:` clause. If `next-task` reports the blocker as resolved,
  verify that, remove the clause with the `backlog` skill, and take the item; otherwise skip it.
- **Too big.** It does not fit in one pull request: record the split with the `backlog` skill and
  skip it.
- **Stale.** `next-task` reports it as already done: close it with the `backlog` skill and skip it.
  Close every other item `next-task` lists as stale the same way.

When neither passes, run `next-task` again with the longer skipped list. Stop the run when it
answers "Nothing eligible", when a call added nothing to the skipped list (the next call would
give the same answer), or after five calls.

## 3. Claim the item

Commit the backlog edits so far (see "Backlog edits") or, if there are none, an empty commit
(`git commit --allow-empty`) naming the item, push, and open the draft PR right away (step 5's
first paragraph). The open loop PR is what tells any other session that an item is in
progress.

Then list the open loop PRs again (title prefix or label, as in step 1). The title prefix is set
when the PR is created, so this does not depend on when either PR got its label. If one of them
has a lower number than this one (two runs started together), mark this one "final" in its run
state, close it with a comment naming the other, and stop the run. The same check follows every loop PR this skill opens, including a
backlog-only PR.

## 4. Implement

Work the item as any other task, following CLAUDE.md and `.claude/README.md` (task size, agents,
documentation sync, ADRs). Close the item with the `backlog` skill in the same PR.

If the work shows that the item needs something the loop cannot supply, do not guess. Add a
`Blocked on:` clause to the item with the `backlog` skill, using the forms in `work/README.md`
(`Blocked on: the maintainer's decision on ...` for a design choice that the specs and ADRs
leave open), and put the item on the skipped list. Commit that edit, then revert this
iteration's implementation changes with a new commit. All backlog edits stay: those made while
picking, the new clause and any follow-up items; if the item itself was already closed, restore
it first so the clause has an item to attach to (no history rewrite), turn the PR into a
backlog-only PR (retitle it, keeping the `[backlog-loop]` prefix and the label), take it through step 5, and count it on the
backlog-only list. A skipped item does not count against the items left. The run is unattended,
so follow `unattended-clarification` rather than waiting for an answer.

### Backlog edits

Edits to the backlog made while picking (a stale item closed, a split recorded, a blocker added
or removed) are committed on the branch and go into this iteration's PR. If the run stops before
a PR carries them, open a backlog-only loop PR with them and take it through step 5 before the
final report, so a later run does not pick the same items again. Never do this while another
loop PR is open. That PR ends the run: mark it "final" in the run state; when it merges,
nothing restarts, and the next run starts only when the maintainer asks.

## 5. Pull request

The PR is a draft whose title starts with `[backlog-loop]`, set when it is created, and it
carries the `backlog-loop` label. Create the label first if the repository does not have it, and
check that the PR carries it.

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

A PR marked "final" ends the run whether it is merged or closed: write the final report if it
was not written yet, and do nothing else.

For any other PR: when it is merged, record it in the run state of its description first (merged
list, items left decreased; a backlog-only PR does not count), then copy the state forward and go
to step 1. When it is closed without merging, go to step 1, which stops the run.

Each item should start with a small context. If the session can start a fresh session for the
next iteration (for example a project coordinator that opens a new thread), hand off there with
the command `/backlog-loop pr=<n>`, naming the PR whose merge was just recorded. Otherwise continue in
this session.

## Final report

One message to the maintainer:

- the PRs merged in this run, each with its shadow risk verdict, and any backlog-only PRs
  opened (merged or still open);
- the items skipped and why;
- everything that waits on the maintainer: every backlog item whose `Blocked on:` clause
  names the maintainer anywhere, not only at its start (a decision, a purchase, a search, a
  setting), plus the items blocked on hardware the maintainer could supply. Write each one the
  maintainer can act on now as a question that can be answered in a word, with a
  recommendation; for the rest (another precondition is still open), give only their count;
- why the run stopped (no items left, nothing eligible within the cap or five `next-task` calls
  without a pick, `main` red, another loop PR open or a lost claim race, a PR closed without merging, or three backlog-only PRs).
