---
name: pr-review-loop
description: Drive the automated review cycle on a nekoguruma pull request Claude opened. Request a Codex review, verify what each finding actually claims, route it (fix, design-advisor escalation, accepted limitation, or decline), push one verified fix per round and re-request review until the review is clean, then hand the PR to Yoko. Use after opening a PR, and whenever a Codex review or review comment arrives on one.
argument-hint: "<PR number>"
---

# PR review loop

CLAUDE.md's "Pull requests" section says when this loop runs; this skill is how. The reviewer is
the Codex GitHub App (`chatgpt-codex-connector[bot]`). It runs outside GitHub Actions, so review
rounds cost no Actions minutes, but every fix push starts a CI run that does, which is why a
round ends in at most one push.

The finding-by-finding decision guide is in
[reference/finding-routing.md](reference/finding-routing.md). Read it before acting on a
finding.

## Step 0: request a review

Pull requests here open as drafts, and Codex reviews a draft only on request. After opening the
PR, and after every fix push, post a top-level PR comment whose whole body is `@codex review`
(`add_issue_comment`, `issue_number` = PR number). Nothing else goes in that comment.

Codex reacts 👀 on the trigger comment when it starts, then posts a review with inline comments,
or reacts 👍 when it has nothing to report.

**Codex not available.** If the trigger comment still has no 👀 and no review 15 minutes after
it was posted (and after CI on that commit has finished, whichever is later; a PR that skips
`ci.yml` finishes CI in seconds, so the time limit is what counts there), Codex is not enabled
for this repository (it is switched on per
repository in the Codex settings on chatgpt.com, which only Yoko can do). Say so once in the
thread, do not post the trigger again on this PR, and run the fallback review instead:
`edge-case-hunter` on the whole PR diff (`git diff origin/main...HEAD`), whatever the diff's
size. Route its findings with the same guide, then go to Step 5.

## Step 1: find out what the review found

A review's summary comment is not evidence of a clean round. Findings arrive as separate inline
review comments, sometimes later than the summary. Each time a review event arrives, read
everything:

```
pull_request_read(method="get_review_comments", owner, repo, pullNumber, perPage=100)
pull_request_read(method="get_reviews", owner, repo, pullNumber)
```

- **All pages.** `get_review_comments` returns at most 100 threads per call. While its
  `pageInfo.hasNextPage` is true, call it again with `after` set to `pageInfo.endCursor`; the
  current round's findings can be on a later page.
- **Which commit a thread reviews.** Threads carry no commit SHA. Reviews do (`commit_id`), and a
  Codex review's inline comments are created at the same moment the review is submitted. Match
  each thread's first comment `created_at` to a review's `submitted_at` to learn the commit it
  reviewed. The comment ID for replies is the number at the end of the comment's `html_url`
  (`#discussion_r<ID>`).

- **New or re-surfaced?** Compare each thread's reviewed commit (above) with the PR head. Codex re-raises
  an unchanged finding when nearby code moves, sometimes anchored on a different line or file
  and worded differently. Match findings by their claim, not by location or wording. A finding
  already fixed, accepted or declined gets a reply pointing at the earlier answer, not a new
  investigation.
- **Clean round** means: Codex reacted 👍 or its review says it found no major issues, *and*
  there is no new inline thread on the current head commit. A "Reviewed commit: …" summary with
  no approving words is not clean; check the threads.

## Step 2: route each new finding

Read the code the finding points at in its current state (line numbers drift between commits)
and make sure you can state the failure scenario yourself. Then pick one path from
[reference/finding-routing.md](reference/finding-routing.md):

- **2a. Direct fix**: clear root cause, an established pattern, no trade-off.
- **2b. `design-advisor`**: several viable mechanisms, a real trade-off, shared state, locking,
  a state machine, or a second finding of the same shape.
- **2c. Accepted limitation**: real, but closing it costs more than its bounded, low-severity
  impact, confirmed by `design-advisor`.
- **2d. Decline**: the claimed mechanism does not happen in this code.

When a new finding lands on a mechanism `design-advisor` already analysed in this session,
continue that agent with `SendMessage` rather than spawning a fresh one.

## Step 3: verify before the push

Handle **every** finding of the round before pushing; one push per round, never one per
finding. A round where every finding was declined (2d) or answered by pointing at an earlier
reply changes no file: skip the checks below and the push, never make an empty commit, and go straight to
the replies and the re-request in Step 4.

1. Run what CLAUDE.md's "Building and testing" lists, delegated to `cargo-runner` when the output
   is long. Run the whole changed crate (`cargo test -p <crate>` runs unit and integration
   tests; `--lib` alone skips `tests/`).
2. Run `edge-case-hunter` on the round's fix when the fix meets its gate in `.claude/README.md`.
   A review-round fix is not exempt because Codex will look again: an unaudited fix tends to
   become the next round's finding.
3. Run `doc-sync-checker` when the fix changes behaviour a document describes, an ADR, or a spec
   citation. A fix to a mechanism an ADR documents updates that ADR in the same PR.
4. **Spec text.** When the fix adds or rewords a citation of ISO 22900-2, SAE J2534 or
   ISO 14229-1, check that no prose was copied from the standard: compare each touched block
   that cites a clause against `vehicle-comm-specs` for runs of six or more consecutive words
   (`doc-sync-checker` does this). A search for quotation marks alone misses unquoted copies.
   Include untracked files (`git status --short --untracked-files=all`).
5. **Base branch.** `git fetch origin main` and merge it into the branch if it moved. Run the
   checks on the merged tree; a PR that conflicts with its base gets no CI run at all.

## Step 4: commit, push, answer, re-request

- The commit message explains the bug and why the fix is right, not "address review feedback".
- After the push (if the round has one), reply on **each** finding's own thread in a couple of sentences: what changed,
  or why not (with the ADR or backlog item for 2c, the trace for 2d). Take the comment ID from a
  `get_review_comments` result fetched in this turn, never from memory.
- React on the finding's comment: 👍 when fixed (2a, 2b), 👎 when accepted as a limitation or
  declined (2c, 2d).
- Every reply ends with the attribution footer GitHub posts from Claude carry.
- Only after every thread of the round has its reply, post `@codex review` as its own comment
  (Step 0), then go back to Step 1.

## CI facts

- Read CI with `pull_request_read(method="get_check_runs")`; the Actions jobs report through the
  Checks API, and `get_status` shows nothing for them. When listing workflow runs, check that
  the run's `head_sha` is the commit you pushed.
- No run at all after a push usually means a merge conflict (`mergeable_state: "dirty"`): GitHub
  cannot build the PR's merge ref. Merge `main` (Step 3.5) before suspecting infrastructure.
- Every job failing within seconds with no runner assigned is an Actions capacity or budget
  problem, not a code failure. Re-run once to confirm, then stop and tell Yoko; more retries
  only spend minutes.
- PRs that change only documentation, `work/` or `.claude/` skip `ci.yml`, so their required
  checks stay pending and Yoko merges them with the ruleset bypass. That is expected, not a CI
  failure.

## Step 5: close out

When a round is clean (or the fallback review in Step 0 is done):

1. **Nothing deferred only in the conversation.** Go through the loop's history: follow-ups any
   agent or you called "deferred", "out of scope" or "worth a separate look", `design-advisor`
   flags of latent gaps, and every 2c limitation. Each one is fixed in this PR, added with the
   `backlog` skill, or (for a 2c limitation not worth closing later) recorded in its ADR's
   Consequences as 2c requires; that ADR bullet is enough, and it needs no backlog item. Check also that `edge-case-hunter` has run against the diff as it
   stands now, if the diff met its gate at any point; a run against an earlier revision does not
   count after a later qualifying fix. If this step edits a file, go back through Steps 3 and 4.
2. **Resolve the threads** that were fixed, accepted or declined (`resolve_review_thread`, with
   the thread `id` from `get_review_comments`). Codex never resolves its own threads.
3. **Hand over.** Mark the PR ready for review and tell Yoko in the project thread that it is
   ready, listing anything accepted as a limitation and anything added to the backlog. Yoko
   reviews and merges; do not merge. Stay subscribed to the PR until it is merged or closed.
4. If one ADR was amended three or more times in the loop, mention that rewriting its Decision
   section in one piece may now read better than the appended amendments.

## When the loop itself has a gap

If a finding fits no routing case, or an agent or rule in this loop produced a bad outcome,
re-read the current guidance (this skill, `.claude/README.md`, CLAUDE.md) to confirm the gap is
real, then propose the change to Yoko with the evidence, as `.claude/README.md` ("Changing this
configuration") asks. Do it when you notice the gap, while the PR is still open, not at
close-out.
