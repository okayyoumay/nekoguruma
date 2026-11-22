# ADR-503: The Review Loop Audits the Whole Diff for Documentation Before the First Review

**Date:** 2026-11-22
**Status:** Accepted
**Affects:** `.claude/skills/pr-review-loop` (Step 0), `.claude/skills/backlog-loop` (step 4), `.claude/README.md` (the pipelines)

## Context

`.claude/README.md`'s medium and large pipelines have `doc-sync-checker` audit a change before it
is committed, but `pr-review-loop` named the agent only in Step 3, for the fix of each review
round. So a pull request reached its first review with no documentation audit, and a missing ADR
or a stale document became the reviewer's first finding and cost a round, although the agent
reports exactly that kind of gap when it is run on the same diff. The maintainer approved the
change on 2026-11-22.

## Decision

1. **Before the first review request of a pull request, `doc-sync-checker` audits the whole
   diff** against `origin/main`, given the PR description and the branch's commit messages, which
   are not in the diff, and what it reports is fixed before the request.
2. **Step 3's check 3 still runs it on each round's fix**; the first audit does not replace it.
3. A pull request that changes only backlog files gets no review (ADR-251) and so no first
   audit; `scripts/check-backlog.sh` checks its format.

## Consequences

- One more agent run per pull request, before any reviewer's round; a missing ADR or a stale
  document is then fixed before the reviewer reads the diff rather than after its first round.
- The agent's judgement on whether an ADR is needed is not binding: a reviewer may still ask for
  one, and the loop routes that finding as any other.
