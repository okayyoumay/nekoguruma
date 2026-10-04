# ADR-195: Cross-Process Test-Harness Startup Race (Per-Process-Unique Config Paths)

**Date:** 2026-08-27
**Status:** Accepted
**Affects:** `j2534-0404-service/tests/grpc_mock/harness.rs`, `j2534-0404-service/tests/live_grpc_flow.rs`, `iso22900-service/tests/grpc_mock.rs`, `iso22900-service/src/service/rpc.rs`

## Context

On 2026-08-07, `j2534-0404-service` fixed a flaky-test root cause: `TestServer::try_start_with_extra_config` (`tests/grpc_mock/harness.rs`) writes a config file to a fixed temp path and sets the `VCI_CONFIG_PATH` env var (both process-global mutable state), then starts a service that reads them back synchronously before its first `.await`. That fix added `VCI_CONFIG_STARTUP_LOCK: tokio::sync::Mutex<()>`, held across the write+env-set+service-startup sequence, to serialize concurrent test *threads* within one process. See `j2534-0404-service/docs/implementation-notes.md`'s "Resolved (2026-08-07)" entry for the full original mechanism.

That fix's scope turned out to be incomplete. A design-advisor investigation, triggered by rotating flakiness observed across `repeat_message.rs`, `additional_channels.rs`, `j1939.rs`, `tp20.rs`, and `response_distribution.rs` during unrelated verification work, was initially misdiagnosed as two or three separate, distinct flakiness clusters. It was also briefly suspected to be a poll-task-shutdown-ordering race in `spawn_channel_poll_task`/`TestServer::shutdown()` — that specific hypothesis was investigated and empirically refuted via instrumented reproduction before the real cause was found.

The actual cause: `VCI_CONFIG_STARTUP_LOCK` is a per-process Rust static, so it does nothing to protect against two *separate* `cargo test` processes (e.g. two different test binaries, or the same binary invoked twice concurrently) racing on the identical fixed config file path — there is no cross-process synchronization at all. This was empirically reproduced by running two `cargo test` processes concurrently, each touching the same fixed path, which reliably reproduced the exact 2026-08-07 failure signature (`RegistryUnsupported`/`"registry lookup is only supported on Windows"` panics), plus other rotating spurious failures where a test silently received the WRONG config (e.g. missing `modules`/`can_channel_mode` entries belonging to a different, concurrently-running process's test).

Reading confirmed the same fixed-path-plus-env-var pattern, with no cross-process protection of any kind (and, in two of the four cases, no intra-process protection either), in four call sites across the workspace:

1. `j2534-0404-service/tests/grpc_mock/harness.rs`'s `TestServer::try_start_with_extra_config` — already has the 2026-08-07 intra-process `VCI_CONFIG_STARTUP_LOCK`.
2. `iso22900-service/tests/grpc_mock.rs`'s `TestServer::start` — no intra-process lock at all (out of scope for this ADR; a separate, not-yet-investigated concern).
3. `iso22900-service/src/service/rpc.rs`'s `set_mock_library_path_config` — a `#[cfg(test)]` helper whose own doc comment already documents `#[serial]` as its (intra-process-only) mitigation.
4. `j2534-0404-service/tests/live_grpc_flow.rs`'s `resolve_library_name` (`J2534_DLL_PATH` branch) — a manual, opt-in smoke test that never runs in CI (lower real-world risk, but the same fixed-path pattern).

An already-correct precedent for the right fix pattern already exists in this repo: `j2534-0404-service/tests/stdio_startup.rs` and `iso22900-service/tests/stdio_startup.rs` both use a `unique_temp_path` helper that builds a per-call-unique temp path from a nanosecond timestamp.

## Decision

Make the fixed temp config-file path per-process-unique at all four call sites, by suffixing the filename with `std::process::id()`:

```rust
let config_path = std::env::temp_dir()
    .join(format!("<base-name>-{}.toml", std::process::id()));
```

This is deliberately simpler than the nanosecond-timestamp `unique_temp_path` precedent: each of these four call sites only needs per-*process* uniqueness, not per-*call* uniqueness — any existing intra-process serialization (`VCI_CONFIG_STARTUP_LOCK` in harness.rs, `#[serial]` in `rpc.rs`) already covers repeated use of the same path across multiple calls within one process, and a fresh path per call would add unnecessary complexity for no benefit here.

Existing intra-process mechanisms are left unchanged: `VCI_CONFIG_STARTUP_LOCK` stays in `harness.rs`, and the `#[serial]`-based convention (and its absence in `iso22900-service/tests/grpc_mock.rs`, which is out of scope) stays as-is elsewhere. This is a minimal, surgical change to path construction only — no `std::fs::remove_file` cleanup was added (see Consequences).

## Consequences

- Per-PID temp config files may accumulate across many CI runs, since no cleanup was added. This is accepted: temp directories are ephemeral and are reaped by the OS or the CI runner between jobs, and adding cleanup logic (with its own correctness concerns, e.g. not deleting a file another concurrent test still needs) was judged not worth the complexity for a file this small and short-lived.
- The `j2534-0404-service/docs/implementation-notes.md` "Known Flaky Tests" entry for `tests/grpc_mock/j1939.rs`'s full-module flakiness (round 15, not yet root-caused) is left as-is: it is plausible that this same cross-process mechanism explains some or all of that flakiness, but it was not confirmed to, so it is not claimed resolved by this change.
- The design-advisor investigation that refuted the poll-task-shutdown-ordering hypothesis also surfaced a separate, distinct, speculative hazard: the `j2534-0404-mock` cdylib's `spawn_repeat_worker` has no protection against the library being unloaded while its detached worker thread is still running. This is unrelated to the cross-process config race this ADR fixes, not fixed here, and tracked as a P3 accepted residual in `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog instead.
- This is a test-infrastructure-only change: no production code was touched at any of the four call sites.
