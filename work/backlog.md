> **TEMPORARY WORKING MATERIAL.** This file is consumable: remove items once they are done, and delete the file when it is empty. Permanent files must not reference it. See `work/README.md`.

# Backlog

Project-wide open items, per area. Item format and priorities: `work/README.md`. Worker-crate items (`iso22900*`, `j2534-0404*`, `vci-service-*`) are in `worker-crates-backlog.md`.

## Status

**Current milestone (decided by Yoko, 2026-10-04):** the local end-to-end path works and is verified on the worker targets: agent -> worker service -> `sim-vci` -> `sim-ecu`, with the ABI interpretation table (design 7.1.2) checked on the six worker targets and the interrupted-write resume scenario (design 5.3 / 5.6) passing against the simulators.

Workspace skeleton (step 1): every crate goes as far as type and boundary definitions; the actual logic is mostly left as `todo!()`.

- `shared-proto`: type definitions for job specs, states, failure codes, capabilities
- `diag-ir`: instruction set, section attributes (interruptibility, idempotency), VM state, `DiagHost` trait
- `sim-vci`: J2534 exported functions and `PASSTHRU_MSG`, switching of `unsigned long` width
- `sim-ecu`: types for sessions, flash stages, NRCs, fault injection
- `j2534-defs`: J2534 constants, protocol name resolution, COMPARAM -> SET_CONFIG mapping (tested)
- `vci-discovery`: J2534 registry discovery (Windows), D-PDU API root file parsing (tested)
- Worker crates (`iso22900*`, `j2534-0404*`, `vci-service-*`): imported working services (`docs/worker-crates.md`)
- `shared-crypto`, `diag-frontend`, `vendor-manifest`, `server`, `agent`: placeholders only

## Simulators and core logic

- **P1**: Implement the UDS services in `sim-ecu` per ISO 14229-1 (2026 edition, in `vehicle-comm-specs`): 0x10 / 0x11 / 0x27 / 0x3E (clauses 9.2, 9.3, 9.4, 9.7), 0x22 / 0x2E (10.2, 10.7), 0x14 / 0x19 (11.2, 11.3), 0x31 (13.2), 0x34 / 0x36 / 0x37 (14.2, 14.4, 14.5), following the server response rules in clause 7.7 and the NRCs in Annex A.1. Done when each service has a unit test for its positive response and at least one NRC, checked against the cited clause.
- **P1**: Delegate `sim-vci` calls to `sim-ecu`, and implement fault injection. Blocked on: UDS services in `sim-ecu`.
- **P1**: Verify the ABI interpretation table (design 7.1.2) on the six worker targets in CI. Today the `abi-roundtrip` job in `.github/workflows/ci.yml` builds only `i686-unknown-linux-gnu` and `scripts/abi-roundtrip.sh` only checks that `libsim_vci.so` exports 8 J2534 symbols; nothing calls the library, `qemu-user` is installed but unused, and `sim-vci`'s `PassThruReadVersion` writes no strings (`crates/sim-vci/src/lib.rs`), so there is nothing to compare. The launch test runs through `j2534-0404-service` (design 7.3: `GetVersion` -> `PassThruReadVersion`). Done when: a CI job runs `j2534-0404-service` built for each of the six targets against `sim-vci` built for the same target (ARM under qemu-user, Windows on the Windows runner) and checks the returned version strings and `unsigned long` width.
- **P1**: Instruction dispatch in `diag-ir`, with postcard serialization/restoration tests for VM state (design 8.2.5).
- **P1**: Interruption/resumption scenario tests: power loss during transfer, reconnection, state check, resume (design 5.3 / 5.6).
- **P1**: `sim-vci` exports `#[no_mangle] extern "C"` functions, but real J2534 DLLs use stdcall on Windows x86. Apply the existing (unused) `passthru_abi!` macro in `crates/sim-vci/src/lib.rs` so the x86 build exports stdcall. Done when the `unused macro` warning is gone and the i686 Windows build exports stdcall symbols.
- **P2**: `sim-vci`'s `PassThruIoctl` ignores `READ_VBATT` and returns no voltage (`crates/sim-vci/src/lib.rs`). The write-job pre-validation checks power (design 5.6, 8.9.1), so its tests need a configurable voltage. Done when: `READ_VBATT` returns a configurable value and a test reads it through the export.
- **P2**: Handle `libloading` symbol resolution when a library exports decorated stdcall names (e.g. `_PassThruReadVersion@16`) in `crates/j2534-0404-sys` (the `sym!` lookups in `src/abi.rs`). Unverified whether any target vendor DLL does this.
- **P3**: `sim-vci`'s `PassThruOpen` (`crates/sim-vci/src/lib.rs`) accepts any number of opens and returns no device ID. Add an option that limits concurrent opens, to exercise the "can multiple devices be opened" vendor question (design 9.3, 17). Done when: a test opens two devices with the limit off, and gets an error for the second with it on.

## Data model (`db/`)

- **P2**: Partition granularity: monthly is assumed; finalize once scale is decided. Blocked on: design 17 P1 and P5.
- **P2**: `vehicle_locks` expiry: derive the default for `expires_at` from the expected duration of each job kind.
- **P2**: `retain_until` logic for the statutory retention period of maintenance records. Blocked on: design 17 P7.
- **P2**: `ir_documents` lookup has only a GIN index on part numbers. ECU-VARIANT-PATTERN matching happens on the agent side; verify with real data whether this is enough server-side filtering.
- **P2**: Seed data: role definitions, the default confirmation level, and preset defaults.

## IR schema (`crates/diag-ir/schema/`)

- **P2**: Variable-length fields: only `length_from_field` references are supported. Decide on ODX's other length specifications (terminators, all remaining bytes). Blocked on: real ODX data.
- **P2**: TABLE (ODX): confirm whether anything goes beyond what `CompuTextTable` can express. Blocked on: real ODX data.
- **P2**: DID hierarchy: nested DIDs cannot be expressed with the flat `Field`; add parent-child relationships to `Field` if real data needs them. Blocked on: real ODX data.

## `crates/j2534-defs`, `crates/vci-discovery`

- **P1**: Linux J2534 registration definition (design 7.1.1) is not implemented in `vci-discovery` yet. The worker services have no equivalent; they read a library path from their own `config.toml`.
- **P2**: `REG_EXPAND_SZ`: `FunctionLibrary` is returned unexpanded; expansion per library bitness (design 7.1) is still to do.
- **P2**: Wiring: `agent` does not call `vci-discovery` yet.
- **P3**: COMPARAM mapping covers J2534-1 base protocols only. J2534-2 cases (CAN FD, J1939, TP2.0, SW/FT CAN, pin-switched variants beyond the plain `_PS` IDs) exist in `j2534-0404-service` and can be carried over when needed.
- **P3**: `sim-vci` and `j2534-0404-mock` are both J2534 cdylib mocks; decide whether `sim-vci` builds on the full mock.

## Worker services (`docs/worker-crates.md`, `crates/worker-host`)

- **P1**: gRPC client in the agent: `worker-host` launches the service and holds the auth key, but token minting (`vci1.<payload>.<HMAC>`, `crates/vci-service-launcher/src/token.rs`) and the tonic client are not written yet.
- **P1**: Library resolution on Linux: the services resolve a library name through their own `config.toml` (`vci-service-config`). Decide whether the agent writes that file from the design 7.1.1 definition, or the service is changed to take an absolute path.
- **P1**: `long_size = 8` end to end is only unit-tested with fake 64-bit functions. `sim-vci` uses `c_ulong` on Linux but exports only 8 of the 14 functions the service needs; add the missing stubs and run the service against it in CI.
- **P3**: Vendor IOCTLs with data are rejected in 64-bit mode (layout unknown); add a VCI profile entry (design 9.3) for them. Blocked on: a vendor that needs one.
- **P3**: J2534 v05.00 workers are not covered. A standalone service manager is not planned (the agent takes that role).

## Agent (`crates/agent`)

- **P1**: The agent is a stub (`crates/agent/src/main.rs` is an empty `main`). Write a minimal job runner: launch the j2534-0404 worker through `worker-host`, implement `diag_ir::DiagHost` on top of the worker gRPC client, and run an IR program (design 3.3, 8.2). Done when: a test in CI's core job starts the agent, which launches `j2534-0404-service` against `sim-vci` on Linux x86_64, opens a channel and reads a DID from `sim-ecu`. Blocked on: the agent's gRPC client and token minting; `sim-vci` delegating to `sim-ecu`; `diag-ir` instruction dispatch.
- **P1**: Write-job journal: nothing records write steps yet, so an interrupted write cannot resume (design 5.5 "journaling of each step", 5.6 `Interrupted -> Writing` transition). Add a minimal on-disk journal in the agent that records each step and the last confirmed block, and resume from it after restart. Done when: a test kills the agent mid-write and the restarted agent either resumes from the journal or ends in `OnSiteInterventionRequired`, never in an undefined state.
- **P2**: Journal protection (design 5.5): encrypt the journal and local cache with an OS-credential-store key, delete the body after job and sync completion, and erase on agent-key revocation. Not needed for the local E2E milestone. Done when: each of the three behaviours has a test.

## CI and repository (`.github/workflows/ci.yml`)

- **P2**: The Linux worker builds do not pin a glibc version, although design 12.1 says `cargo-zigbuild` pins it and the `worker-linux` job comment in `.github/workflows/ci.yml` says it is pinned. Pick the minimum glibc and append it to the zigbuild targets (e.g. `x86_64-unknown-linux-gnu.2.17`). Done when: the targets carry a glibc suffix and design 12.1 names the version. Blocked on: Yoko choosing the minimum glibc (target distributions, design 17 P3).
- **P2**: CI does not run `cargo fmt --check` or `cargo clippy`, and the `core` job runs `cargo test` without `--locked`, although `CLAUDE.md`'s build section lists all three as the checks CI runs. Done when: `ci.yml` runs fmt, clippy (`-D warnings` once the two existing dead-code warnings in `sim-vci` and `sim-ecu` are gone) and `--locked` tests.
- **P2**: Startup tests for the operating modes (Windows service, systemd user service; design 6.2, 6.5, 13.4 "CI matrix: OS x worker ABI x operating mode") are only a `TODO` comment at the end of `.github/workflows/ci.yml`. Done when: CI starts the agent in each mode on Linux and Windows and checks it reports its mode.
- **P2**: Clean up `work/worker-crates-backlog.md` with `backlog-triage`: `j2534-0404-service` keeps four dated "Resolved" history blocks under Known Flaky Tests, and its ~140 items were prioritized on the earlier scale (most are P3). Done when: the Resolved blocks are gone (lasting facts moved to the crate docs) and the items are re-checked against the current scale.

## Standards and project

- **P2**: Obtain ISO 14229-2 (UDS session layer: P2 / P2* timing, response pending), needed for the worker L1 timing in M2 and for checking `sim-ecu`'s timing values. Done when: the converted text is in `vehicle-comm-specs`. Blocked on: Yoko purchasing it.
- **P2**: Obtain ISO 15765-2 (ISO-TP), needed for the worker L1 transport (design 8.1). Done when: the converted text is in `vehicle-comm-specs`. Blocked on: Yoko purchasing it.
- **P2**: Obtain ISO 22901-1 (ODX) and sample ODX data; the `crates/diag-ir/schema/` items (variable-length fields, TABLE, DID hierarchy) wait on real ODX data. Done when: the standard is in `vehicle-comm-specs` and at least one sample ODX/PDX file is available to the project. Blocked on: Yoko purchasing it.
- **P2**: Trademark search for "Nekoguruma" and "NGR" (J-PlatPat, classes 9 and 42) before any public release. The session network cannot reach J-PlatPat. Done when: the result is recorded and the name is kept or changed. Blocked on: Yoko running the search.
- **P2**: Public release of the repository. The repository was recreated on 2026-10-04 from a single clean commit (the earlier history, which carried verbatim standard quotes, stays private in `nekoguruma-old`), under the MIT license. Before switching visibility to public, enable Actions approval for fork pull requests, a read-only default `GITHUB_TOKEN`, secret scanning with push protection, Dependabot alerts and a branch ruleset for `main`. Done when: those settings are on and the repository is public. Blocked on: the trademark search above and Yoko switching the visibility.
