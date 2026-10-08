---
applyTo: ".github/workflows/**,.config/**,.cargo/**,scripts/**"
---

# Review guidelines: CI and scripts

Flag as P1:

- **CI cost**: the goal of CI changes here is fewer GitHub Actions minutes, not shorter
  wall-clock time. Flag a workflow change that raises total minutes: new parallel jobs or matrix
  entries, cross-target release builds on pull requests, losing the docs-only skip, or dropping
  build caching. The `msrv` job is an accepted exception, limited to main and to pull requests
  that change a manifest, the lockfile or `.cargo/`; widening its trigger is still a finding.
- **Required checks**: a change that lets a required check be skipped and so pass without
  running, or that renames or removes a job the branch ruleset requires (`repo-checks`,
  `core-linux`, `core-windows`, `worker-check`, `abi-roundtrip`, `msrv`). `msrv` is skipped on
  pull requests that change no `Cargo.toml`, `Cargo.lock` or `.cargo/` file; that skip is intended.
- **Out-of-scope targets**: Windows workers target `*-pc-windows-gnullvm` only (ADR-227). An MSVC
  worker target or MSVC-only build path is P1.
- **Tests weakened**: excluding tests, adding `--skip` filters or loosening a nextest profile to
  get CI green.
- **Dependabot auto-merge**: `dependabot-auto-merge.yml` runs on `pull_request_target` with a
  write token, so any step that checks out or runs code from the pull request is P1, as is
  widening it beyond Dependabot's `cargo-minor-patch` group, or dropping the step that turns
  auto-merge off after a push by anyone other than Dependabot (other Dependabot updates and all
  human pull requests are merged by the maintainer).
