# ADR-457: The Workspace's Minimum Rust Version Is 1.94

**Date:** 2026-11-11
**Status:** Accepted
**Affects:** `Cargo.toml` (`rust-version`), ADR-255 (item 7's MSRV bullet); `.cargo/config.toml`'s resolver preference and the `msrv` job in `.github/workflows/ci.yml` follow the manifest

## Context

ADR-255 set the workspace's minimum Rust version to 1.89, the first release with
`File::try_lock`, which the write-job journal's writer lock needs. M3 gives the server its
PostgreSQL use, and the maintainer chose sqlx 0.9.0 for it (2026-11-11). sqlx 0.9.0 declares
a minimum Rust version of 1.94. The 0.8 series still builds with 1.89, but it has had no
release since 2025-05 and brings more packages into the lock, so the maintainer chose to raise
the minimum instead of staying on 0.8.

## Decision

1. The workspace's `rust-version` is 1.94. This supersedes ADR-255 item 7's MSRV bullet.
2. Nothing else changes with it. `.cargo/config.toml` keeps preferring dependency versions
   that support the workspace's `rust-version`, which now means 1.94, and the `msrv` job keeps
   reading the version from `Cargo.toml`.
3. A later raise follows the same rule: an ADR names the dependency or language feature that
   needs it, and this record is annotated.

## Consequences

- Code may use what Rust 1.94 stabilized. The `msrv` job checks the workspace with 1.94 on
  pull requests that touch a manifest, the lock or `.cargo/`, and on every push to `main` that is not documentation-only.
- A contributor or a build machine with an older toolchain than 1.94 cannot build the
  workspace. The CI jobs use stable, apart from `msrv` and `fuzz`.
- `cargo update`, and Dependabot through it, may now pick versions that need up to 1.94.
