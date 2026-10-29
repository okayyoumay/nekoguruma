---
name: backlog-loop
description: Work through the nekoguruma backlog one item at a time. Handle open Dependabot pull requests first, then pick the next unblocked item that, checked against the code before it is claimed, fits one pull request and leaves no design choice open, implement it in one pull request, run pr-review-loop, hand the PR to the maintainer, and start the next item only after the maintainer merges it. The final report ends with a retrospective on the loop itself. Run only when the maintainer asks for it.
disable-model-invocation: true
argument-hint: "[max items, default 3] [lowest priority to include: P1 (default) or P2] | pr=<number of the PR to resume>"
---

# Backlog loop

One iteration is one backlog item, or one failing Dependabot pull request (step 2), and one
pull request. Iterations run strictly one after
another: the next one starts only after the maintainer has merged the previous PR. Claude never
merges, and never works on two loop items at once. ADR-230 records why claims, run state and the
branch reset work the way they do.

Arguments: $ARGUMENTS. To start a run: the number of items for this run (default 3), then the
lowest priority to include: P1 (default: P0, P1 and the `Known Flaky Tests` entries) or P2. P3
items are not worked by the loop. To resume a run: `pr=<n>`, the run's current PR; everything
else comes from that PR's run state.

A run starts only when the maintainer asks for one, or from the previous iteration's hand-off
(step 6). Never schedule a recurring trigger for it.

## Run state

The run's state is: a run id (the UTC time the run started), the current PR number, items left,
the priority cap, and four lists (merged PRs with their shadow risk verdicts, skipped items with
the reason, backlog-only PRs, the one that ends the run marked "final", and Dependabot PRs: each
one this run replaced, with its replacement, or reported to the maintainer, with the reason),
and the milestone count at the start: the number of milestone P1 items, counted on `origin/main`
when the run starts by the counting rules of the `milestone-exit` skill. Keep it in a "Loop
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

Otherwise start the branch afresh from `origin/main`, locally and on the remote (ADR-249). This
discards everything on the local branch, so first check that nothing else needs it: stop and
tell the maintainer if `git status --porcelain` prints anything, or if any open PR, labelled or
not, has this branch as its head. Then:

```sh
git fetch origin
git checkout -B <branch> origin/main
git push -u origin <branch>
```

The repository deletes a PR's head branch when the PR merges, so after a merged iteration the
push creates the branch anew and its history is exactly `origin/main`'s: the next PR lists only
its own commits, and no diff against `main` shows the previous iteration's changes. Never
force-push. A rejected push means the remote branch survived with commits that are not on
`main` (the previous PR was closed without merging, or something else pushed to it): stop the run, go to "Final report", and ask
the maintainer to delete the remote branch. List there any backlog edit the stop could not
push, such as the `Blocked on:` clause for a PR closed unmerged. Every later step starts from
this fresh branch.

Then stop the run and go to "Final report" if any of these holds:

- no items are left;
- the latest CI run on `main` failed (a red `main` is P0 work, outside the loop);
- an open `codeql-alerts` PR exists (the `codeql` label, or the title prefix `[codeql]` with a
  head branch in this repository; ADR-243): tasks run one at a time, across both loops;
- this run's current PR was closed without being merged. Unless it is a Dependabot replacement
  (step 2), add `Blocked on: the maintainer's
  reason for closing PR #<n>` to its item as a backlog edit (see "Backlog edits"), so no later
  run picks it again, and report the close;
- this run has opened three backlog-only PRs (step 4), so the remaining items need the
  maintainer more than the loop.

## 2. Pick

### Dependabot pull requests first

Before picking a backlog item, list the open PRs whose author is `dependabot[bot]`, security
updates first, then oldest first (ADR-279). Sessions cannot push to Dependabot's branches, and
Claude never merges, so the loop handles each one by its state. A failed required check (the
last case below) decides first, even while other checks are pending or auto-merge is on; a merge
conflict decides next; otherwise the first case that matches decides:

- **Checks pending:** leave it; look again at the next iteration.
- **In the `cargo-minor-patch` group, with auto-merge turned on** (the PR's `auto_merge` field
  is set) and no failed check: leave it. It merges itself once the required checks pass (ADR-248). Green without
  auto-merge (the auto-merge workflow failed, or the repository setting is off) is handled like
  the next case.
- **Green, any other update** (a Cargo update Cargo treats as breaking, a GitHub Actions update,
  a security update): it waits for the maintainer. Do not merge it, and do not stop for it. Put
  it on the run's Dependabot list for the final report. If it changes a dependency built for a
  Windows target (`cargo tree -i <crate> --target <target>` finds it for either target the
  `worker-windows` job builds, `x86_64-pc-windows-gnullvm` and `i686-pc-windows-gnullvm`), say
  in the report that the Windows release build and its DLL import check in `ci.yml` run only on
  `main`, so the PR's green checks do not cover them.
- **Merge conflict:** comment `@dependabot rebase` once, unless the PR already has such a
  comment. Dependabot rebases its own branch; this is not an iteration. If the PR is still
  conflicted after Dependabot has answered (for example, it says it cannot rebase), do not
  comment again: put it on the run's Dependabot list for the final report.
- **A required check failed**, and the same check is green on `main`: this PR is the
  iteration's task, ahead of every backlog item. Take the first such PR not already on the run's
  Dependabot list. Claim it as step 3 does, with the title `[backlog-loop] ` followed by
  Dependabot's title and a "Replaces #<n>" line in the description. In step 4, apply the same
  update on the loop branch (the same versions in `Cargo.toml`, `Cargo.lock` or the workflow
  files), reproduce the failure with the repository's own checks, and fix it at its root cause.
  If the update changes a dependency built for either of those Windows targets, also check that
  the new version links no DLL that Windows itself lacks (a toolchain or third-party DLL), since
  `ci.yml`'s DLL import check runs only on `main` (`abi-roundtrip-windows`, ADR-283).
  The replacement PR has no claimed item, so its shadow risk verdict is HIGH. The maintainer
  merges it like any other loop PR, also when the update came from the `cargo-minor-patch`
  group. Do not close the Dependabot PR: Dependabot closes it once `main` has the update. If it
  is still open after the replacement merges, list it in the final report.
  When the fix needs something the loop cannot supply (a design choice, an API migration too big
  for one PR), do not open a replacement: put the PR on the Dependabot list with the reason and
  go on with the backlog.

A replacement counts against the items left like a backlog item. With no Dependabot PR to
replace, pick a backlog item as below.

### Backlog item

Run `next-task` with `cap=<cap>` and `skip=` followed by the items on the run's skipped list
(`next-task` sees nothing but its arguments). Still check its answer: leave out every skipped
item and every item below the cap, for the pick and the alternative alike. Take the first of the
pick and the alternative that passes these checks:

- **Blocked.** It has a `Blocked on:` clause. If `next-task` reports the blocker as resolved,
  verify that, remove the clause with the `backlog` skill, and take the item; otherwise skip it.
- **Too big or undecided.** Judge this from the code, not from the item's text alone, before
  claiming it: open the files the item names, estimate the files and lines the change touches
  against `scripts/classify-pr-risk.sh`'s limits (20 files, 800 lines), and look for a design
  choice the design document and the ADRs leave open (where the work belongs, an interface it
  changes, an outcome or rule it would have to invent). If it does not fit one pull request,
  record the split with the `backlog` skill and skip it. If a choice is open, add `Blocked on:
  the maintainer's decision on ...` with the `backlog` skill (the forms in `work/README.md`) and
  skip it. An estimate over the limits is no reason to split when the excess is tests or
  generated code the one change needs; say so in the PR.
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

Then list the open loop PRs again (title prefix or label, as in step 1), together with the open
`codeql-alerts` PRs (the `codeql` label, or the title prefix `[codeql]` with a head branch in
this repository; ADR-243). The title prefix is set when the PR is created, so this does not depend on
when either PR got its label. If one of them has a lower number than this one (two runs started together), mark this one "final" in its run
state, close it with a comment naming the other, and stop the run. Backlog edits made while
picking are dropped with it on purpose: the winning run, or a later one, finds the same stale,
too-big, undecided or unblocked items with the same checks and makes those edits again. The final report
lists the dropped edits. The same check follows every loop PR this skill opens, including a
backlog-only PR.

## 4. Implement

Work the item as any other task, following CLAUDE.md and `.claude/README.md` (task size, agents,
documentation sync, ADRs). For an item whose blocker a decision of the maintainer just resolved,
write CLAUDE.md's design summary ("Guidelines") and get it confirmed before implementing. Close the item with the `backlog` skill in the same PR. A Dependabot
replacement (step 2) has no item to close. If its work shows the loop cannot finish it, put the
Dependabot PR on the run's Dependabot list with the reason, mark the replacement "final" in its
run state, close it unmerged with a comment giving the reason, and write the final report. Its
remote branch survives the close, so the next run's push in step 1 would be rejected (ADR-249):
the report asks the maintainer to delete that branch. No `Blocked on:` clause is added, since
there is no backlog item.

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

Run the classifier (after the fetch in step 1) with the claimed item and copy its output into
the PR description under a "Risk (shadow)" heading. The claim is the item's first physical line,
the one starting with `- `, as it reads on the PR's base. Read it from the file rather than
typing it, since backticks and quotes in item text would be mangled or run by the shell:

```sh
claim="$(git show origin/main:<backlog file> | sed -n '<line number>p')"
scripts/classify-pr-risk.sh --claimed-item "$claim"
```

For a Dependabot replacement, run the classifier without the option. Without the option, when
the claim names no item or several, or when the PR deletes any other
item, the verdict is HIGH. The verdict is recorded only to compare it later with the maintainer's
own judgement; it changes nothing about who merges. If the script exits with status 2, write
"not classified" and the reason it printed. Then run `pr-review-loop`. Before it hands the PR
to the maintainer, run the classifier again on the final head and replace the section if the
verdict or its reasons changed.

## 6. Wait for the merge, then continue

Start the 30-minute merge-wait timer (`pr-review-loop`, "Waking up" in Step 0; ADR-380) and end
the turn. A timer wake reads the PR's state: merged or closed goes on as below, and still open
starts the next timer. While the PR is open, any other wake (a review, a CI result, a comment) is
handled by `pr-review-loop`, and the turn ends again; it does not start step 1.

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
- the Dependabot PRs: each one replaced, with its replacement, and each one still open that waits
  on the maintainer, as a question that can be answered in a word (merge or close) with a
  recommendation and the reason, including the `main`-only Windows checks where they apply;
- the items skipped and why;
- everything that waits on the maintainer: every backlog item whose `Blocked on:` clause
  names the maintainer anywhere, not only at its start (a decision, a purchase, a search, a
  setting), plus the items blocked on hardware the maintainer could supply. Write each one the
  maintainer can act on now as a question that can be answered in a word, with a
  recommendation; for the rest (another precondition is still open), give only their count;
- one line on the current milestone: its P1 count now, counted the same way on the latest
  `origin/main`, and the change from the count at the start. Give no per-item list; the
  `milestone-exit` skill's `status` mode gives the full report. When the count is zero, say so
  and recommend that the maintainer run its `sweep` mode. Never run `sweep` or `close` from the
  loop: both need the maintainer;
- why the run stopped (no items left, nothing eligible within the cap or five `next-task` calls
  without a pick, `main` red, another loop PR open or a lost claim race, a PR closed without merging, a Dependabot replacement the loop could not finish (with the
  branch the maintainer should delete), or three backlog-only PRs);
- the retrospective (below).

### Retrospective

The last part of the final report looks for ways to improve the loop itself (this skill,
`pr-review-loop`, the agents and rules they use, CI). It reads only what the run already left
behind, and looks only for these signs:

- **Long review.** A PR of this run that took five or more review rounds (its description's review
  summary, `pr-review-loop` Step 5). Read its findings and name what made it long: findings of one
  kind arriving one at a time, an item too big for one PR, a rule the implementation missed.
- **Repeated finding.** Findings of the same kind (the same rule broken, or the same kind of gap)
  in two or more PRs of this run.
- **Failure only on `main`.** A CI run on `main` that failed on the merge of one of this run's
  PRs, in a job or test that pull requests do not run, or a PR outside the loop merged during
  the run to fix such a failure. Name the check that would have caught it before the merge.
- **Stop.** The run stopped for a reason other than its items running out or nothing being
  eligible.

When none of these shows, the retrospective is one line saying so. Otherwise give at most three
proposals. Each one names the PRs and findings or check runs it rests on, the file it would
change and how, and is a question the maintainer can answer in a word. Leave out a proposal that
has no such evidence: a general lesson with no PR behind it is not worth the maintainer's time.
Do not propose what an open backlog item already covers; name that item instead.

The loop never makes these changes itself, and never edits a skill, an agent or a rule during
the run. A proposal the maintainer accepts becomes a P1 development-process item in the
backlog (`backlog` skill) and goes through the loop like any other item.
