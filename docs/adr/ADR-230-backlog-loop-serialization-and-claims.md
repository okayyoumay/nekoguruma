# ADR-230: Backlog Loop Runs One Item at a Time, with PR-Based Claims and Run State

**Date:** 2026-10-05
**Status:** Accepted
**Affects:** `.claude/skills/backlog-loop/SKILL.md`, `.claude/skills/next-task/SKILL.md`, `scripts/classify-pr-risk.sh`

## Context

The `backlog-loop` skill lets Claude work through the backlog unattended: pick an item, implement
it in one pull request, run the review loop, and hand the PR to the maintainer. The maintainer
asked for three properties:

- items are never processed in parallel;
- Claude never merges; the maintainer merges every PR;
- the next item starts only after the previous PR is merged.

A run spans many sessions, because each iteration waits for a merge that can take hours and a
long conversation gets compacted. Two sessions can also start a run at about the same time (for
example from two project threads). So the loop needs durable run state, a way to keep a second
run from working while one is active, and a way to begin each iteration from `main` without
history rewrites, which need approval in an unattended session.

## Decision

1. **The open loop PR is the claim.** A loop PR has the `[backlog-loop]` title prefix, set when
   the PR is created, and the `backlog-loop` label. The title prefix counts only for a head branch
   in this repository, since anyone can choose a fork PR's title. A run claims an item by opening
   its draft PR before any implementation, using an empty commit if there is nothing to commit
   yet.
2. **Lower PR number wins a claim race.** Right after opening its PR, a run lists the open loop
   PRs again. If one has a lower number, this run marks its own PR "final", closes it with a
   comment naming the other, and stops. PR numbers are assigned by GitHub in creation order, so
   both racing runs reach the same verdict without talking to each other. A run that finds
   another run's loop PR open in its pre-iteration check stops at once.
3. **Run state lives in the current PR's description.** A "Loop run" section holds:
   - the run id (the UTC start time) and the items left;
   - the priority cap;
   - the merged PRs with their shadow risk verdicts;
   - the skipped items with their reasons;
   - the backlog-only PRs.

   A PR belongs to a run only when its section carries that run's id. A new session resumes with
   `pr=<n>` and acts on that PR's state (draft, handed over, merged, closed, "final").
4. **A "final" PR ends the run** whether it is merged or closed. Nothing restarts after it; the
   next run starts only when the maintainer asks.
5. **Each iteration starts from `main` by a merge, not a reset.** The branch ends up with exactly
   `origin/main`'s tree and with `origin/main` in its history.
   - When `origin/main` is not an ancestor, the branch records a merge of it whose tree is
     `origin/main`'s (`merge -s ours`, then `read-tree --reset`). This happens even when the trees
     already match, because a squash-merged previous PR leaves equal trees on a different
     history, and diffs against `main` would show that PR's changes again.
   - When `origin/main` is already an ancestor, an ordinary commit resets the content.
   - When it is an ancestor and the trees match, nothing is done.

   This cannot conflict and needs no force push. It runs only when the working tree is clean and
   no open PR uses the branch.
6. **Stop conditions are explicit:**
   - no items left;
   - `main` red;
   - another run's loop PR open, or a lost claim race;
   - the current PR closed unmerged, which also adds a `Blocked on:` clause so no later run picks
     the item again;
   - three backlog-only PRs;
   - `next-task` finding nothing eligible.
7. **Risk classification is shadow-only.** `scripts/classify-pr-risk.sh` records a LOW/HIGH
   verdict on each loop PR. It changes nothing about who merges; it exists so the maintainer can
   later judge whether low-risk PRs could merge automatically.

## Alternatives considered

- **A lock branch** (like `adr-reservation/{NNN}`, created atomically through the GitHub API).
  Rejected: a lock outlives a crashed session and needs expiry and cleanup rules, while an open PR
  is visible to the maintainer, closes naturally and already carries the run state.
- **Run state in a committed file or in project memory.** Rejected: a file in the branch is
  dropped by the per-iteration reset and would conflict between runs; memory is not visible on
  the PR the maintainer reviews.
- **A recurring trigger that starts iterations.** Rejected: it could start an iteration while a
  PR is still unmerged, and the maintainer wants runs only on request.

## Consequences

- At most one loop PR is open at a time, except during the short window of a claim race, which
  the losing run ends.
- Backlog edits the losing run made while picking are dropped with its PR. The winning run or a
  later one makes them again with the same checks, and the final report lists them.
- The loop relies on the label and the title prefix; a maintainer who opens a PR with the
  `[backlog-loop]` prefix by hand stops any run that sees it.
