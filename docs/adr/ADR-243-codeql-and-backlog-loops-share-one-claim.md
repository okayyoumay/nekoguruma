# ADR-243: The CodeQL and Backlog Loops Share One PR-Based Claim

**Date:** 2026-10-07
**Status:** Accepted
**Affects:** `.claude/skills/codeql-alerts/SKILL.md`, `.claude/skills/backlog-loop/SKILL.md`

## Context

ADR-230 serializes the `backlog-loop` skill: the open `[backlog-loop]` PR is the claim, and when
two runs open PRs at about the same time the lower PR number wins. The `codeql-alerts` skill adds
a second unattended loop that fixes CodeQL code scanning alerts, one alert and one PR at a time,
started on request or by a weekly routine. The maintainer wants tasks processed one at a time,
not one at a time per loop. With two loops, a check that only looks at its own kind of PR lets a
CodeQL fix and a backlog item proceed in parallel, and a check made before either PR exists
lets two runs that start together both pass it.

## Decision

1. **One claim set.** The open PRs from branches in this repository whose title starts with
   `[backlog-loop]` or `[codeql]`, or that carry the `backlog-loop` or `codeql` label, form a
   single claim set. Either loop stops before starting work while any of them is open (other
   than its own run's PR).
2. **Claim before work, lower number wins.** Each loop opens its draft PR (with an empty commit
   if there is nothing to commit yet) before implementing anything, then lists the claim set
   again. If any PR in the set has a lower number, the run closes its own PR with a comment
   naming the other and stops, exactly as ADR-230 item 2 does within one loop. GitHub assigns PR
   numbers in creation order, so both runs reach the same verdict.
3. **Only real fixes claim.** A CodeQL run claims an alert only after verifying that it will fix
   it. Alerts it skips (false positives, generated code, high or critical security severity) are
   reported to the maintainer and open no PR.

## Alternatives considered

- **Separate claims per loop.** Rejected: it lets a CodeQL fix and a backlog item run in
  parallel, against the maintainer's one-task-at-a-time rule.
- **A pre-start check only.** Rejected: two runs that start within the same minutes both pass
  it, which ADR-230 already solved with the post-open comparison.

## Consequences

- At most one PR from either loop is open at a time, except during the short window of a claim
  race, which the losing run ends.
- A weekly CodeQL run finds nothing to do while a backlog-loop PR waits for the maintainer, and
  reports that; a long backlog run can delay CodeQL fixes by weeks.
- A maintainer who opens a PR with either title prefix by hand stops both loops.
