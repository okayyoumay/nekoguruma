---
applyTo: ".github/workflows/**,.config/**,.cargo/**,scripts/**"
---

# Review guidelines: CI and scripts

Flag as P1:

- **CI cost**: the goal of CI changes here is fewer GitHub Actions minutes, not shorter
  wall-clock time. Flag a workflow change that raises total minutes: new parallel jobs or matrix
  entries, cross-target release builds on pull requests, losing the docs-only skip, or dropping
  build caching.
- **Required checks**: a change that lets a required check be skipped and so pass without
  running, or that renames or removes a job the branch ruleset requires (`repo-checks`,
  `core-linux`, `core-windows`, `worker-check`, `abi-roundtrip`, `msrv`). `msrv` is skipped on
  pull requests that change no `Cargo.toml`, `Cargo.lock` or `.cargo/` file; that skip is intended.
- **Out-of-scope targets**: Windows workers target `*-pc-windows-gnullvm` only (ADR-227). An MSVC
  worker target or MSVC-only build path is P1.
- **Tests weakened**: excluding tests, adding `--skip` filters or loosening a nextest profile to
  get CI green.
