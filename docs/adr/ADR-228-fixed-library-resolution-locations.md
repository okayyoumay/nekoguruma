# ADR-228: Library Resolution from Fixed Locations, Shared by Agent and Worker

**Date:** 2026-10-04
**Status:** Accepted
**Affects:** `docs/system-architecture.md` (4, 4.2, 7.1.1, 7.2, 7.3, 9.2, 9.3, 9.4, 11, 11.1, 16.1), `api/openapi.yaml`, `schemas/extension-manifest.*`, `db/migrations/0002_extensions_without_vci_profile.sql`, `crates/vci-discovery`, `crates/vci-service-config`, `crates/j2534-0404-registry`, `crates/worker-host`, worker services

## Context

The agent starts a worker service for a VCI, and the service has to load that VCI's vendor
library. On Windows the service finds the library through the J2534 registry (HKLM), which only
administrators can change. Linux has no registry: design 7.1.1 defines registration files of
this software's own format, searched in `/etc/nekoguruma/j2534/` and in a per-user directory
under `$XDG_CONFIG_HOME`. The worker services, for their part, read a library path and
per-library settings from their own configuration file (`vci-service-config`). Nothing connected
the two, and the agent had no way to tell a service which file to load.

Two ways of passing the library were considered: the agent generating a configuration file per
worker and pointing the service at it through `VCI_CONFIG_PATH`, or adding an absolute path to the
service's startup argument. Both let a value supplied at run time decide which library a worker
loads. The worker services already avoid that for their configuration file: its location is
embedded at build time, and the runtime `VCI_CONFIG_PATH` override exists only in debug builds
(ADR-073), so that a release build cannot be pointed at a different file.

The agent also checks a library before it is loaded (7.2: not writable by regular users, signer
if signed). If the agent checks one path and the service later resolves the name on its own, the
checked file and the loaded file can differ.

## Decision

1. **Fixed locations.** Every file that decides which library a worker loads (Linux registration
   definitions, the worker service's configuration file) is read from a location fixed at build
   time. Release builds take no such location from environment variables or command-line
   arguments; a runtime override is compiled into debug builds only, for tests, following
   ADR-073.
2. **No per-user registration definitions.** On Linux, registration definitions are read only from
   `/etc/nekoguruma/j2534/`, in both user mode and device mode. The per-user directory of 7.1.1 is
   dropped, along with the precedence rule it would have needed.
3. **The agent passes a name, not a path.** The agent starts a worker service with the VCI's
   library name in the startup argument. The service resolves the path itself, from the registry
   on Windows and from the registration definitions on Linux.
4. **One shared resolver.** Resolution and the 7.2 pre-load checks live in one crate used by both
   the agent (discovery, loadability in `capabilities`) and the worker services. The service runs
   the checks itself immediately before loading, so the file checked is the file loaded.
5. **VCI profiles are installed outside this software.** VCI profiles (9.3), both the data the
   agent reads and the per-library settings in the worker services' configuration file, are local
   files at fixed, administrator-only locations. They are installed and updated by an external
   package management or software distribution service, which is out of this project's scope.
   They are removed from the extension packages the agent syncs from the server (9.4), so they no
   longer carry the ingestion signature; their trust rests on the location (item 1).

## Consequences

- An attacker who can set a worker's environment or influence its arguments still cannot make it
  load a library from an unexpected location; only an administrator-writable file can.
- Users without administrator rights cannot register a J2534 library on Linux themselves. A
  vendor installer or an administrator has to place the definition in `/etc/nekoguruma/j2534/`.
- Release-build tests that need a different configuration location (for example a launch test
  against `sim-vci` on the cross-built targets) need either a debug build or a build with a test
  location embedded.
- The worker services' configuration file is `/etc/nekoguruma/config.toml` on Linux and
  `%ProgramData%\nekoguruma\config.toml` on Windows by default (`vci-service-config`, build-time
  `VCI_CONFIG_PATH`), replacing a placeholder that resolved to the filesystem root on Linux. The
  registration-definition directory is fixed the same way (`j2534_definition_dir()`, build-time
  `NGR_J2534_DEFINITION_DIR`, default `/etc/nekoguruma/j2534`), each with a debug-only runtime
  override.
- A VCI profile fix can no longer be pushed to agents from the server; it reaches devices only
  through the operator's package management. The server's extension-package ingestion no longer
  handles VCI profiles.
- The shared resolver crate does not exist yet. `vci-discovery`, `vci-service-config` and
  `j2534-0404-registry` each hold part of the logic today and are consolidated into it.
