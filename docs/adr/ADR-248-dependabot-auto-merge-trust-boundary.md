# ADR-248: Dependabot's Cargo Minor/Patch Group Merges Itself

**Date:** 2026-10-08
**Status:** Accepted
**Affects:** `.github/workflows/dependabot-auto-merge.yml`, `.github/dependabot.yml`,
`.github/instructions/ci.instructions.md`

## Context

Dependabot opens a weekly grouped pull request for Cargo minor and patch updates
(`cargo-minor-patch`) and one pull request per update that Cargo treats as breaking (a new major
version, or a 0.x minor bump such as 0.22 to 0.23). Until now the maintainer merged every one of
them by hand. The grouped minor/patch updates are routine: they are semver-compatible, and main's
required checks (`repo-checks`, `core-linux`, `core-windows`, `worker-check`, `abi-roundtrip`,
and `msrv`, which runs whenever a manifest or the lockfile changes) build and test them like any
other change.

Turning on GitHub auto-merge for a pull request needs a token with write access. GitHub runs a
`pull_request` workflow started by Dependabot like a fork run: its token is read-only and the
`permissions` key cannot raise it. A `pull_request_target` workflow runs the file as it is on the
base branch with a write-capable token. Dependabot restricts that token only when the pull
request's base branch was itself created by Dependabot, which `main` is not. That write token is
the trust boundary this decision crosses: a careless step there, such as checking out the pull
request, would run untrusted code with write access.

## Decision

1. Dependabot's `cargo-minor-patch` group pull request has GitHub auto-merge (squash) turned on
   when it opens. It merges once the required checks pass. No human review is required.
2. Every other pull request is merged by the maintainer, including Dependabot's breaking Cargo
   updates, GitHub Actions updates and security updates. Selection is by Dependabot group, not by
   the update type `dependabot/fetch-metadata` reports: that type can call a 0.x bump "minor"
   while Cargo treats it as breaking, and `dependabot.yml` already keeps such bumps out of the
   group.
3. The workflow runs on `pull_request_target` for pull requests whose author is
   `dependabot[bot]`, and turns auto-merge on only when the triggering actor is Dependabot too.
   GitHub keeps auto-merge on after a push by someone with write access, so when anyone else
   pushes to the branch the workflow turns auto-merge off instead, and the pull request waits for
   the maintainer. Runs for one pull request are serialized, and enabling auto-merge is bound to
   the commit Dependabot pushed, so a run for an older push cannot turn it back on for a newer
   head. A failure to turn it off fails the job.
4. The workflow never checks out or runs anything from the pull request. It only reads
   Dependabot's metadata and calls `gh pr merge --auto`. Copilot review treats breaking this rule,
   or widening the workflow beyond the group, as P1 (`.github/instructions/ci.instructions.md`).

## Consequences

- Routine Cargo updates land without the maintainer, and breaking ones still get a person's
  review.
- The repository setting "Allow auto-merge" must stay on. If it is turned off, the workflow fails
  and the group pull request waits for a manual merge, so the failure is safe.
- A compromised minor or patch release that passes CI could reach main without a person seeing
  it. The 7-day cooldown in `dependabot.yml` limits this: a release is proposed only after it has
  been public for a week.
- The workflow cannot be exercised before it is on main, because `pull_request_target` runs the
  base branch's copy.
