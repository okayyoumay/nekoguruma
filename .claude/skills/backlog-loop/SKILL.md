---
name: backlog-loop
description: Work through the nekoguruma backlog one item at a time. Pick the next unblocked item, implement it in one pull request, run pr-review-loop, hand the PR to Yoko, and start the next item only after she merges it. Use when Yoko asks to work through the backlog, or to continue a running loop after a loop PR was merged.
argument-hint: "[max items, default 3] [lowest priority to include: P1 (default) or P2]"
---

# Backlog loop

One iteration is one backlog item and one pull request. Iterations run strictly one after
another: the next one starts only after Yoko has merged the previous PR. Claude never merges,
and never works on two loop items at once.

Arguments: $ARGUMENTS. The first is the number of items for this run (default 3); the second is
the lowest priority to include: P1 (default: P0, P1 and the `Known Flaky Tests` entries) or P2.
P3 items are not worked by the loop; `next-task` never ranks them while higher items are open.
When continuing a run, the hand-off message (step 5) carries the remaining count, the priority
cap and the lists gathered so far.

## 1. Check before each iteration

Stop the run and go to "Final report" if any of these holds:

- the run's item count is used up;
- the latest CI run on `main` failed (a red `main` is P0 work, outside the loop);
- an open pull request carries the `backlog-loop` label (one item at a time: wait for it);
- the previous loop PR was closed without being merged (ask Yoko why before going on).

## 2. Pick

Run `next-task`. It offers P2 items only once no P0 or P1 item is left in scope; with the
default P1 cap, a P2 pick means nothing eligible is left, so stop. Take its pick, unless the pick:

- has a `Blocked on:` clause, or turns out to need a decision, a document or hardware that is not
  available (see step 3);
- will not fit in one pull request (record a split with the `backlog` skill instead, and pick
  again);
- was reported stale (close it with the `backlog` skill and pick again; the edit rides along as
  described under "Backlog edits" below).

Then take its alternative under the same rules. If neither works, stop: nothing unblocked is left
within the cap.

## 3. Implement

Work the item as any other task, following CLAUDE.md and `.claude/README.md` (task size, agents,
documentation sync, ADRs). Close the item with the `backlog` skill in the same PR.

If the work shows that the item needs something the loop cannot supply, do not guess. Write a
`Blocked on:` clause into the item with the `backlog` skill, using the forms in `work/README.md`
(`Blocked on: Yoko's decision on ...` for a design choice that the specs and ADRs leave open),
add it to the run's skipped list, discard the branch's other changes, and go back to step 2. A
skipped item does not count against the run's item count; its `Blocked on:` edit is kept as
described under "Backlog edits" below. The run is unattended, so follow
`unattended-clarification` rather than waiting for an answer.

### Backlog edits

Backlog edits made while picking (a stale item closed, a split recorded, a `Blocked on:` clause
added) stay on the branch and go into the next loop PR. If the run stops before another PR is
opened, open a backlog-only loop PR with them (step 4) and hand it to Yoko before the final
report, so a later run does not pick the same items again.

## 4. Pull request

Open the PR as a draft with the `backlog-loop` label. Run `scripts/classify-pr-risk.sh` and copy
its output into the PR description under a "Risk (shadow)" heading. The verdict is recorded only
to compare it later with Yoko's own judgement; it changes nothing about who merges. Then run
`pr-review-loop`. Before it hands the PR to Yoko, run the classifier again on the final head and
replace the section if review fixes changed the verdict or its reasons.

## 5. Wait for the merge, then continue

End the turn; the merge event wakes the session. Before the next iteration, check that `main` is
green (step 1) and refresh the branch from `main`.

Each item should start with a small context. If the session can start a fresh session for the
next iteration (for example a project coordinator that opens a new thread, or a scheduled
trigger), hand off there with a message such as "backlog-loop continue: 2 items left, cap P1",
followed by the merged, skipped and Yoko-waiting lists so far. Otherwise continue in this
session.

## Final report

One message to Yoko:

- the PRs merged in this run, each with its shadow risk verdict;
- the items skipped and why;
- everything that waits on Yoko: every backlog item with `Blocked on: Yoko's decision`, plus the
  items blocked on a purchase or hardware she could supply. Write each decision as a question she
  can answer in a word, with a recommendation;
- why the run stopped (item count used up, nothing unblocked left, `main` red, a PR closed).
