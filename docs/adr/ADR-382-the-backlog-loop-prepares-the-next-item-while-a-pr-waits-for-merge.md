# ADR-382: The Backlog Loop Prepares the Next Item While a PR Waits for Merge

**Date:** 2026-10-29
**Status:** Accepted
**Affects:** `.claude/skills/backlog-loop/SKILL.md` (introduction, run state, steps 1-3 and 6, final report), `.claude/skills/next-task/SKILL.md` (`avoid=`), `CLAUDE.md` ("Backlog"); ADR-230

## Context

ADR-230 runs the backlog loop strictly one item at a time: the next item starts only after the
maintainer merges the previous pull request. Over 48 consecutive loop pull requests, the time
between handing a pull request over and its merge added up to about 31 hours, a third of the
time those pull requests were open, and the loop did nothing during it. Implementing an item, up
to its first review request, took about 27 hours over the same sample.

Running two items fully in parallel was considered and not chosen: almost every loop pull request
edits the backlog files, most edit the same crate as the one before, and two pull requests in
review at once double the maintainer's review load and the review and CI runs spent on conflicts.
Using the merge wait for the next item's implementation gets most of the gain while keeping one
pull request in review.

## Decision

1. **One item in review, one in preparation.** While the current loop pull request waits for the
   maintainer's merge, the loop may prepare the next item: pick it and implement it, through the
   local checks and the `edge-case-hunter` pass, in a separate local worktree started from
   `origin/main`. It pushes nothing, opens no pull request and requests no review for that item
   until the current pull request is merged. At most one item is prepared at a time.
2. **The open pull request is still the claim.** No new claim mechanism: ADR-230 items 1 and 2
   still decide which run owns the loop, since another run stops on seeing the open loop pull
   request. The prepared item is named in the current pull request's run state, so a resuming
   session knows it existed.
3. **Picking avoids the open pull request's files.** The pick passes `next-task` the files the
   current pull request changes (the backlog files aside) as `avoid=`, and leaves out items that
   need that pull request's change. With no such item, the loop prepares nothing and waits as
   before.
4. **The current pull request comes first.** A review, a CI failure, a conflict or a maintainer
   comment on it is handled before any preparation work, which pauses until then.
5. **After the merge, the prepared work moves onto `main`.** The loop runs step 1's checks,
   checks that the prepared item is still open and eligible on the new `main`, rebases the local
   commits onto `origin/main` (they were never pushed, so this rewrites no shared history), and
   resolves conflicts: a backlog file conflict mechanically, by taking `main`'s side and redoing
   the item's own backlog edits; a code conflict as `pr-review-loop` resolves one with its base (Step 3, check 5), followed by
   the local checks and the `edge-case-hunter` pass again. Then the session's branch is set to the
   result, pushed, and claimed as ADR-230 item 1 says, with the claim race check unchanged.
6. **Prepared work is local and may be dropped.** It is discarded when the current pull request is
   closed unmerged (the run stops, as ADR-230 item 6 says), when the item is no longer eligible
   after the merge, or when the session that holds the worktree ends; the final report lists it.
   A run with prepared work continues in its own session instead of handing off to a fresh one.

## Consequences

- The merge wait is used for implementation, so the next pull request can request its first
  review minutes after the previous merge instead of after a full implementation.
- The maintainer still sees one loop pull request at a time, though the next one now arrives soon
  after a merge.
- Prepared work that turns out ineligible, or that a session loss drops, is wasted; the cost is
  bounded by one item.
- Rebasing onto a `main` that moved can conflict in code the open pull request also changed; the
  `avoid=` pick makes that rare, and the checks run again after the rebase.
- Staying in one session for prepared work trades ADR-230's small per-item context for the saved
  wait; the run state in the pull request still allows a fresh session to resume, without the
  prepared work.
