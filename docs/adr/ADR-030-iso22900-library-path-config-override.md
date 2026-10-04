# ADR-030: Remove `ISO22900_ROOT_DESCRIPTION_FILE` Env Var; Extend `library_path` Config Override to `iso22900-service`

**Date:** 2026-07-01
**Status:** Accepted
**Affects:** `iso22900-registry/src/lib.rs`, `iso22900-service/src/service/rpc.rs`, `iso22900-service/tests/grpc_mock.rs`, `iso22900-service/tests/stdio_startup.rs`, `iso22900-registry/tests/arch_field.rs` (removed)

## Context

`iso22900-registry::root_description_file_path()` resolved the D-PDU API
Root Description File (RDF) path with an env-var override
(`ISO22900_ROOT_DESCRIPTION_FILE[_<ARCH>]`) checked before the platform
fallback (Windows registry `HKLM\SOFTWARE\D-PDU API\Root File`, or the
hardcoded `/etc/pdu_api_root.xml` on non-Windows). ADR-025 already noted
this env var as an established precedent, distinct from the thread-local
test-override pattern it introduced for `vci-service-launcher`.

Separately, ADR-029 added a `library_path` config override
(`config.apis.<api>.libs.<lib>.library_path`) to `vci-service-launcher`,
consulted so far only by `j2534-0404-service`. That ADR explicitly flagged
extending the same mechanism to `iso22900-service` as unimplemented future
work.

This ADR does both at once: removes the `ISO22900_ROOT_DESCRIPTION_FILE`
env var entirely, and wires `iso22900-service` to the existing
`library_path` config mechanism as its replacement for test/manual
library resolution, mirroring `j2534-0404-service`.

## Decision

### Env var removed

`iso22900-registry::root_description_file_path()` no longer checks any
environment variable. It resolves directly to the Windows registry value
(`HKLM\SOFTWARE\D-PDU API\Root File`) on Windows, or the hardcoded
`/etc/pdu_api_root.xml` on non-Windows — the same fallback behavior as
before, minus the override. The now-unused `_from_registry` function
suffix and the macro-generated `get_view_mode_name()` helper (only used to
build the arch-specific env var name) were removed as dead code.

### `library_path` config wired into `iso22900-service`

`Iso22900Service::new()` now calls
`vci_service_launcher::config::find_library_path("iso22900", arch,
library_name)` before falling back to `iso22900_registry::find_pdu_libraries()`,
exactly mirroring `J2534Service::new()`'s use of `find_library_path()`
before `j2534_0404_registry::find_j2534_device()` (ADR-029). When a
`library_path` is configured for the requested name, `DPduApi::new()` loads
it directly — the RDF is never read for that library name.

### Test fallout

Every existing use of the env var was in test setup, pointing tests at a
generated mock RDF without touching the registry or `/etc/pdu_api_root.xml`:

- `iso22900-service/src/service/rpc.rs` (3 unit tests) and
  `iso22900-service/tests/grpc_mock.rs` now write a temp `config.toml` with
  `[config.apis.iso22900.libs."TestLib"].library_path` pointing at
  `iso22900_mock::mock_library_path()`, and set `VCI_CONFIG_PATH` to it —
  the same pattern `j2534-0404-service/tests/grpc_mock.rs` already used for
  ADR-029.
- `iso22900-service/tests/stdio_startup.rs`'s
  `write_test_root_description()` helper (which generated an RDF XML file)
  was replaced with `write_test_library_config()`, which writes a
  `config.toml` `library_path` entry instead; each subprocess test now sets
  `VCI_CONFIG_PATH` rather than the removed env var.
  `get_status_returns_startup_error_details_when_grpc_start_fails` (which
  intentionally requests a library name that resolves to nothing) now
  configures a `library_path` pointing at a file that does not exist on
  disk, so the library-load failure is deterministic regardless of
  registry or `/etc/pdu_api_root.xml` state on the test machine. Requesting
  a name with no config entry at all would instead fall through to RDF
  lookup, whose failure depends on environment-specific filesystem state.
  (Note: as of ADR-089, the test asserts the startup-error message does
  *not* contain the requested name/path — the error text was sanitized —
  rather than asserting it does, as originally written here.)
- `iso22900-registry/tests/arch_field.rs`, which exercised RDF
  arch-tagging (`LibraryArch::W32`/`W64`) by pointing
  `ISO22900_ROOT_DESCRIPTION_FILE_X64`/`_X86` at generated fixture RDFs,
  was deleted. `library_path` bypasses RDF parsing entirely (it goes
  straight to a path, with no short-name/arch metadata), so it cannot
  substitute as a test seam for this logic, and `iso22900-registry` has no
  dependency on `vci-service-launcher` to reuse its config-file
  thread-local test-override pattern (ADR-025). There is now no test
  coverage for RDF-derived `LibraryArch` tagging on Windows.

## Consequences

- `iso22900-service` gains the same benefits ADR-029 gave
  `j2534-0404-service`: a library can be added or overridden by editing
  `config.toml`, without touching the Windows registry or generating an
  RDF file, and it is now possible to point `iso22900-service` at a
  library on non-Windows without an RDF at `/etc/pdu_api_root.xml`.
- A misconfigured `library_path` silently shadows a correct RDF entry for
  the same name — same trade-off already accepted for `j2534-0404-service`
  in ADR-029.
- Loss of test coverage for `iso22900-registry`'s RDF arch-tagging logic
  (`enumerate_pdu_libraries_from_rdf`'s `LibraryArch` assignment per
  `RegistryViewMode`) on Windows, since `arch_field.rs` was deleted with no
  replacement seam. Re-adding coverage would require either a
  Windows-registry-writable CI environment or a dedicated test-only
  override added directly to `iso22900-registry` (not currently justified
  by a second caller).
- `iso22900-mock::mock_root_definition_file_path()` (RDF generator) is kept
  for any future direct `iso22900-registry`-level RDF testing, but is no
  longer used by any `iso22900-service` test.
