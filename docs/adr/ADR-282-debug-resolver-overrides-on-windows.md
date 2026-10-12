# ADR-282: In Debug Builds the Resolvers' Runtime Overrides Also Replace the Windows Registry

**Date:** 2026-10-12
**Status:** Accepted
**Affects:** `crates/ngr-library-resolver` (`j2534_0404::resolve`, `j2534_0404::resolve_on_registry`, `iso22900::root_file_path`, `docs/library-resolver.md`), `crates/vci-service-config` (`j2534_definition_dir`, `pdu_api_root_file`), `docs/system-architecture.md` (7.1.1, 7.2), the integration tests of `j2534-0404-service`, `iso22900-service` and `agent`; ADR-228, ADR-281

## Context

ADR-281 Decision item 1 removes the `library_path` key of `config.toml` as a resolution source.
From then on a worker loads only what the fixed locations of design 7.1 name, and tests are to
point a worker at the mock library or `sim-vci` through the resolvers' debug-only runtime
overrides (ADR-228 Decision item 1):

- `NGR_J2534_DEFINITION_DIR`, the directory of Linux J2534 registration definitions
  (`vci_service_config::j2534_definition_dir`);
- `NGR_PDU_API_ROOT_FILE`, the D-PDU API root description file on Linux
  (`vci_service_config::pdu_api_root_file`).

Both overrides name Linux locations only. On Windows, `ngr_library_resolver::j2534_0404::resolve`
reads the J2534 registry, and `iso22900::root_file_path` reads the `Root File` value of the
`D-PDU API` key, both in HKLM. Neither has an override, so a Windows test has no way to name a
test library once `library_path` is gone. These tests run on the Windows CI runners:
`iso22900-service` on every pull request, and `j2534-0404-service` on `main`.

Three ways were open:

1. In debug builds, the existing runtime override also takes precedence over the registry on
   Windows.
2. The tests write a registry entry for the test library.
3. A separate debug-only hook in each service resolves through `resolve_in_dir` or
   `resolve_in_root_file`.

## Decision

1. **The debug override replaces the registry on Windows.** In a debug build, when
   `NGR_J2534_DEFINITION_DIR` is set and not empty, J2534 resolution on Windows reads the
   definitions in that directory instead of the registry, exactly as on Linux. This applies to
   `resolve` and to `resolve_on_registry`, whatever view the caller asks for. When
   `NGR_PDU_API_ROOT_FILE` is set and not empty, `root_file_path` returns that file for every
   registry view instead of reading the `D-PDU API` key.
2. **Release builds are unchanged.** The override is compiled out of release builds, as ADR-228
   item 1 and ADR-073 already require. A release build on Windows reads only HKLM; on Linux only
   the fixed locations.
3. **One definition format.** A Windows test writes the same TOML registration definition, or
   the same root description file, that a Linux test writes, naming the test library's absolute
   path. No second format and no registry fixture exist for tests.
4. **The 7.2 checks still run.** A library found through an override is checked like any other.
   The override's definition or root file is a naming file of the result (ADR-270), so tests that
   expect device mode to load a library have to place both files where the check passes, or use
   user mode.

## Alternatives considered

- **Tests write a registry entry (option 2).** Rejected: writing HKLM needs administrator
  rights, which a developer machine may not grant; a test that leaves its entry behind changes
  the machine's real J2534 list, and parallel tests would share one key. The resolvers read only HKLM
  (design 7.2), so an HKCU entry would test a path production never takes.
- **A debug-only hook in each service (option 3).** Rejected: the hook would bypass
  `resolve` itself, so the Windows path the agent and the worker share would go untested, and
  the agent would need its own matching hook to detect the ABI of the same library (ADR-281
  Decision item 1).

## Consequences

- A debug build on Windows resolves differently from a release build whenever one of the two
  variables is set. The variables are named in the test harnesses and in
  `docs/library-resolver.md`; nothing sets them outside tests.
- On a Windows debug build with the override set, `resolve_on_registry` returns the same
  definition for both views, so the agent's per-view choice of a worker build falls back to the
  library's ABI alone, as on Linux.
- The definition parser and its validation (absolute `FunctionLibrary`, size bound, links) now
  run on Windows as well in tests.
- Implementation work (the resolver change and moving the tests off `library_path`) is tracked
  in the backlog, not here.
