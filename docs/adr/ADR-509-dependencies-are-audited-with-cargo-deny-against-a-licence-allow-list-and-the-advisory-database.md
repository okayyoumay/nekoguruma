# ADR-509: Dependencies Are Audited With cargo-deny Against a Licence Allow-List and the Advisory Database

**Date:** 2026-11-26
**Status:** Accepted
**Affects:** `deny.toml`, `.github/workflows/ci.yml` (`deps-audit`, the `changes` job's `audit` output), the `license` key of the workspace crates' manifests, `CLAUDE.md` ("Building and testing"), `.github/instructions/ci.instructions.md`

## Context

Nothing checked the dependency tree for known vulnerabilities, unsound or yanked crates, or
licences. The workspace is published under MIT (`LICENSE`, `[workspace.package]` in
`Cargo.toml`), so a dependency under a copyleft licence would be a legal problem that nobody
would see until someone read the lockfile. An advisory against a crate the lockfile pins is
published on a day when no one touches the lockfile, so a check that runs only when the
lockfile changes would miss it. The Pragmatic Rust Guidelines (M-STATIC-VERIFICATION) ask for
such checks to run automatically.

## Decision

1. **Tool.** `cargo-deny`, configured by `deny.toml` at the repository root. It checks the
   advisories, licences, sources and bans of the main workspace's tree from `Cargo.lock` and the
   manifests; it compiles nothing.
2. **Job.** `deps-audit` in `ci.yml` runs `cargo deny --locked check`. It is in `ci.yml` and not
   in `repo-checks.yml` because it must not run on every pull request, and only `ci.yml` has the
   `changes` job that decides by the files a pull request changes. `changes` gains an `audit`
   output: true when a `Cargo.toml`, `Cargo.lock`, a file of `.cargo/` or `deny.toml` changes.
   The `msrv` job's `deps` output stays as it was. Like the other jobs it runs whenever `changes`
   did not succeed, and on every push to main that starts the workflow (documentation-only
   pushes are skipped by the workflow's filter), which also finds an advisory published after
   the lockfile last changed. The advisory database is fetched from GitHub at run time.
3. **Pinning.** `taiki-e/install-action` installs `cargo-deny@<exact version>`, as the workflow
   installs `cargo-nextest`, `cargo-fuzz` and `cargo-zigbuild`; the action checks the download
   against hashes it records. The version is at least two weeks old when chosen, and changing it
   is a change of the workflow.
4. **Licences.** `deny.toml` allows exactly the licences the tree uses today, all permissive:
   MIT, Apache-2.0 (also with the LLVM exception), BSD-2-Clause, BSD-3-Clause, ISC, Unicode-3.0,
   Zlib, BSL-1.0, CC0-1.0, MIT-0 and Unlicense. A copyleft licence is not on the list. A crate
   offered under `A OR B` passes when one side is allowed, so a copyleft alternative beside a
   permissive one is not a finding. A crate under a licence not on the list fails the job and
   the maintainer decides whether to allow it. The confidence threshold is the default.
5. **The workspace's own crates** carry `license.workspace = true`, as most already did; the
   eleven that had no `license` key now have it. They are not marked private
   (`[licenses.private]`), since the workspace is published and the check should cover them too.
6. **Advisories.** A vulnerability, an unsound crate (`unsound = "all"`) and a yanked version
   fail the job. An unmaintained crate fails it only when a workspace crate depends on it
   directly (`unmaintained = "workspace"`); a transitive one is a warning. A new advisory fails
   the job; the maintainer chooses between updating, replacing the dependency and an `ignore`
   entry in `deny.toml`. An `ignore` entry carries a reason and has a backlog item that removes
   it. The tree has one such case: `heapless` 0.7's unsoundness advisories are ignored, because
   it comes in only through a dependency of `starlark_syntax` that enables `postcard`'s
   `heapless-cas` feature itself, no workspace setting removes it, and no workspace crate uses a
   `heapless` type.
7. **Sources.** crates.io only. An unknown registry or a git dependency fails the job.
8. **Bans.** Two versions of one crate and wildcard requirements are allowed; this check does
   not govern them.
9. **Targets.** `deny.toml` sets no `graph.targets`, so a dependency that only one platform's
   build pulls in is checked too, and `graph.all-features = true`, so a dependency that only an
   optional feature enables is checked as well.
10. **Run it locally** from the repository root with `cargo deny --locked check`, with the
    version the workflow pins.

## Rejected

- **`cargo-audit` alone.** It covers advisories only, not licences or sources, and would need a
  second tool for them.
- **Running on every pull request.** Most pull requests change no dependency, and the main
  push already catches a new advisory; the job runs when a manifest, the lockfile, `.cargo/`
  or `deny.toml` changes, which costs fewer Actions minutes.
- **Failing on duplicate versions.** A tree this size always has some; the check would fail
  without a defect and is not what this decision is for.

## Consequences

- A pull request that adds a dependency under an unlisted licence, a git dependency or a crate
  with an advisory fails `deps-audit` before it merges. A new advisory against a dependency
  that is already merged fails the job on the next push to main, whatever that push changes.
- The job downloads the crates' manifests to read the tree (`cargo metadata`) but builds
  nothing. It is not a required check of the branch ruleset; adding it is the maintainer's
  decision.
- `fuzz/` is its own workspace with its own lockfile, and `deny.toml` does not cover it. Its
  tree repeats the main tree's advisories, and its own package declares no licence, so covering
  it takes a second run with `--manifest-path fuzz/Cargo.toml` and a licence for that package.
