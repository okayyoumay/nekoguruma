# ADR-279: The Backlog Loop Handles Open Dependabot Pull Requests First

**Date:** 2026-10-11
**Status:** Accepted
**Affects:** `.claude/skills/backlog-loop/SKILL.md`

## Context

Dependabot opens pull requests every week (`.github/dependabot.yml`). Only the Cargo
`cargo-minor-patch` group merges itself (ADR-248); every other update waits for the maintainer.
Nothing looked at a Dependabot pull request whose checks failed, so it stayed open until someone
asked a session to deal with it. The maintainer asked for the `backlog-loop` skill to cover open
Dependabot pull requests as well.

Three facts shape how the loop can do that:

- Claude sessions cannot push to Dependabot's branches. A fix for a failing update has to go in
  a separate pull request that carries the same update.
- The loop runs one task and one pull request at a time and never merges (ADR-230).
- Some checks run only on `main`: the Windows release builds and the DLL import check in
  `ci.yml`. A Windows dependency update can pass every pull request check and still turn `main`
  red, which has happened once.

## Decision

1. **Dependabot first.** Before picking a backlog item, each iteration looks at the open
   Dependabot pull requests, security updates first, then oldest first.
2. **Only a failing one becomes an iteration.** A Dependabot pull request with a failed required
   check that is green on `main` is the iteration's task. The loop applies the same update on its
   own branch, fixes the failure at its root cause, and opens it as an ordinary loop pull request
   (the `[backlog-loop]` prefix and label, so ADR-230's claim covers it) that names the pull
   request it replaces. It counts against the run's items like a backlog item. The maintainer
   merges it, also when the update came from the `cargo-minor-patch` group: ADR-248's auto-merge
   covers only Dependabot's own pull requests. The loop does not close the Dependabot pull
   request; Dependabot closes it once `main` has the update.
3. **The rest are not iterations.** A pending one is looked at again next iteration; a green one
   in the `cargo-minor-patch` group is left to merge itself; a conflicted one gets a single
   `@dependabot rebase` comment; a green one that needs the maintainer is listed in the final
   report as a merge-or-close question, without stopping the run. A failing one whose fix needs
   something the loop cannot supply is listed there too, with the reason.
4. **Windows updates name the `main`-only checks.** When an update changes a dependency built for
   a Windows target, the loop checks the new version's DLL imports against `ci.yml`'s allowlist
   before handing over a replacement, and the final report says that the green pull request
   checks do not cover those builds.
5. **Run state.** The "Loop run" section gains a Dependabot list (replaced, with the replacement,
   or reported, with the reason), so a resumed run does not handle the same pull request twice.
   A replacement the run closes itself because it cannot finish it does not stop the run and
   adds no `Blocked on:` clause, since there is no backlog item.

## Alternatives considered

- **Waiting for the maintainer to merge each green Dependabot pull request before going on.**
  Rejected: the loop would stall on every weekly update, and those merges need no work from the
  loop.
- **A separate Dependabot loop skill**, like `codeql-alerts`. Rejected for now: replacements are
  rare and short, and one loop keeps the one-task-at-a-time rule without another claim set.
- **Merging a green, non-auto-merge update from the loop.** Rejected: ADR-230 keeps every merge
  with the maintainer, and ADR-248 reserves breaking, Actions and security updates for a
  person's review.

## Consequences

- Failing Dependabot pull requests get fixed without a separate request, one at a time, before
  backlog work.
- A replacement has no backlog item, so its shadow risk verdict is always HIGH. It adds no
  LOW samples to the classifier comparison.
- Green updates that wait for the maintainer appear in every final report until they are merged
  or closed.
