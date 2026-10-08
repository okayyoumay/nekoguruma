# ADR-249: Backlog Loop Iterations Start from a Fresh Branch at `origin/main`

**Date:** 2026-10-08
**Status:** Accepted
**Affects:** `.claude/skills/backlog-loop/SKILL.md` (step 1), `.claude/skills/codeql-alerts/SKILL.md`

## Context

ADR-230 Decision item 5 starts each loop iteration from `main` by a merge, not a reset: the
branch keeps its history and records a merge of `origin/main` whose tree is `origin/main`'s.
That avoided both conflicts and force pushes. It also means the branch keeps every commit
of earlier iterations. Loop PRs are squash-merged, so those commits never reach `main`, and the
next loop PR lists all of them next to its own. The diff is right, but the commit list is not,
and the maintainer asked for each iteration to start by resetting the branch and syncing it
with the remote.

The repository deletes a PR's head branch when the PR merges. After a merged iteration the
remote branch no longer exists, so recreating it from `origin/main` needs no force push.

## Decision

1. **Each iteration starts from a fresh branch.** After the existing checks (clean working
   tree, no open PR with this branch as its head), step 1 runs `git fetch origin`,
   `git checkout -B <branch> origin/main` and an ordinary `git push -u origin <branch>`. The
   local branch is rebuilt from `origin/main`. The push recreates the remote branch that the
   merge deleted.
2. **No force push.** A rejected push means the remote branch survived with commits that are
   not on `main`: the previous PR was closed without merging, or something else pushed to the
   branch. The run stops, the final report asks the maintainer to delete the remote branch, and
   it lists any backlog edit the stop could not push (such as the `Blocked on:` clause for a PR
   closed unmerged).
3. The `codeql-alerts` skill starts its branch the same way, since it already follows
   `backlog-loop` step 1.

This supersedes ADR-230 Decision item 5. The rest of ADR-230 stands.

## Alternatives considered

- **Keep the merge-based start (ADR-230 item 5).** Rejected: it keeps earlier iterations'
  commits in every later loop PR.
- **Hard reset and `--force-with-lease` every time.** Rejected: once the merge has deleted the
  remote branch, a force push is not needed. Using one anyway would also overwrite a branch that
  survived for a reason, such as a PR closed without merging, which the run should report
  instead.

## Consequences

- A loop PR lists only its own commits.
- The loop now depends on the repository setting that deletes a head branch on merge. Without
  it, every iteration would stop at the push and ask the maintainer to delete the branch.
- A run whose previous PR was closed unmerged still stops, as before. Its `Blocked on:` clause
  now reaches the maintainer through the final report rather than through a backlog-only PR.
