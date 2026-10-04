# ADR-025: Thread-Local Test Override for config_root()

**Date:** 2026-07-01
**Status:** Accepted
**Affects:** `vci-service-launcher/src/config.rs`, `vci-service-launcher/Cargo.toml`

## Context

`config_file_path()` resolves via `config_root()`, which has exactly one of
three mutually-exclusive real implementations compiled per (target,
feature): the executable's directory (`config-root-exe-dir`), a Windows
known folder resolved at runtime via `SHGetKnownFolderPath` (default
`FOLDERID_ProgramData`, which needs admin rights to write to), or a
hardcoded `/` on non-Windows (see ADR-024). None of these are usable in an
isolated `cargo test` run.

As a result, `find_logging_config()` — a 5-level priority fallthrough
(`arch+lib > arch > api+lib > api > root`) — had zero unit test coverage.
A way was needed to point `config_root()` at a temp directory from
`#[cfg(test)]` code, without changing `find_logging_config()`'s public
signature (used by `lib.rs`) and without requiring any `--features` flag
just to run `cargo test`.

Two options were considered:

1. An always-on environment-variable override, matching the existing
   precedent in `iso22900-registry`'s `root_description_file_path()`
   (`ISO22900_ROOT_DESCRIPTION_FILE[_<ARCH>]`, used with `tempfile` in
   `iso22900-registry/tests/arch_field.rs`).
2. A `#[cfg(test)]`-only thread-local override with an RAII guard.

## Decision

Went with (2). `config_root()`'s three prior implementations were renamed to
`config_root_impl()` (gates and bodies unchanged), and a thin dispatcher
named `config_root()` was added that checks a `#[cfg(test)]`-only
thread-local override (`test_support::OVERRIDE`, a
`RefCell<Option<PathBuf>>`) before delegating to `config_root_impl()`. A
`RootOverrideGuard` (RAII, restores the previous value on `Drop`) lets tests
scope the override to a `tempfile::TempDir` for the duration of a `#[test]`.

This was chosen over the env-var precedent because `config_root()`'s
three-way `#[cfg(...)]` split makes the thread-local approach strictly
better here: it needs no `unsafe` (this workspace is on a rustc version
where `env::set_var`/`remove_var` are `unsafe fn`), and it gives free
per-test isolation under cargo's one-thread-per-test default — no `Mutex`
or `#[serial]`-style serialization needed even with several small test
functions in the same file, unlike the single large test function the
env-var approach uses in `arch_field.rs`.

It was also chosen over full dependency injection (passing a `root: &Path`
parameter through `find_logging_config` / `load_toml_config` /
`config_file_path`) as disproportionate for a config module, and because it
would touch the public API and its one caller in `lib.rs` unnecessarily.

## Consequences

- No production behavior change: `#[cfg(test)]` code does not exist in
  release builds, so `config_root()`'s dispatcher compiles down to a direct
  call to `config_root_impl()` outside test builds.
- The Windows `SHGetKnownFolderPath` code path remains compiled in test
  builds (still the same `cfg(windows)` target) but is never invoked at
  runtime during tests, since the override is always set before
  `find_logging_config()` is exercised in each test.
- `cargo test -p vci-service-launcher` requires no `--features` flags, and
  the override takes priority regardless of which `config-root-*` feature
  (if any) is active alongside it.
- New dev-dependency: `tempfile = "3"`, matching the `iso22900-registry`
  precedent (declared per-crate, not promoted to
  `[workspace.dependencies]`).
- This establishes a second, different pattern (thread-local vs. env-var)
  for making a hard-coded platform path lookup testable in this workspace.
  Future test-override needs for a *single* (non-cfg-split) path-resolution
  function may still reasonably follow the `iso22900-registry` env-var
  precedent instead; this ADR does not deprecate that pattern.

  **Update (ADR-030):** the `iso22900-registry` env-var precedent this ADR
  compares against (`ISO22900_ROOT_DESCRIPTION_FILE[_<ARCH>]`) has since
  been removed; `iso22900-registry`'s `root_description_file_path()` no
  longer has any override, env-var or otherwise. The comparison above is
  kept for historical context on why `vci-service-launcher` chose
  differently at the time.
