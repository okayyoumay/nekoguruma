# ADR-285: The Launcher Checks the Worker's Configuration File Before Reading It

**Date:** 2026-10-13
**Status:** Accepted
**Affects:** `vci-service-launcher` (`start_service`, `config_check.rs`, `VciServer::new`,
`ServiceIdentity`), `ngr-library-resolver` (`check_files_with`), `iso22900-service` and
`j2534-0404-service` (`preload.rs`, service construction), design 7.2, ADR-270 Decision item 5

## Context

ADR-270 Decision item 5 puts the worker service's configuration file (`config.toml`) among the
files the design 7.2 writability check covers, since the service acts on its per-library
settings. Both workers ran that check inside the service constructor, together with the checks
on the library, immediately before loading it.

By then the launcher (`vci_service_launcher::start_service`) had already read the same file:
it looks up the logging settings and starts logging before it constructs the service, and the
logging settings name a log file that the worker creates or appends to. In device mode a
configuration file that regular users can write therefore chose a file the worker opened, under
the worker's account, before the check that refuses that file had run.

## Decision

1. **The launcher checks the configuration file first.** Before it reads any setting,
   `start_service` checks the file at its fixed location with the 7.2 writability rules for a
   naming file (`ngr_library_resolver::check_files_with`, which walks files without a library)
   against the system owners. Only a file that is not there counts as absent; any other failure
   to probe it goes to the check, which reports it.
2. **The mode from the startup argument decides, as for the library.** `ServiceIdentity` carries
   the operating mode, so the launcher applies it before the service exists:
   - a file that passed: logging uses its settings and the service starts;
   - a failed file in device mode: logging starts with its defaults, the findings are logged as
     an error, and the service does not start; the startup error names no path (ADR-089) and is
     reported through the JSON-RPC layer like any other startup failure;
   - a failed file in user mode: the findings are logged as a warning, and logging and the
     service use the file;
   - no file: logging uses its defaults and nothing is configured.
3. **The service reuses the verdict.** `VciServer::new` receives it as a `ConfigFile` value
   (`Absent`, `Checked` or `AcceptedWithFindings`). The services take per-library settings from
   the file only when `ConfigFile::settings_apply` says so, and their pre-load checks cover the
   library and the files that named it, no longer the configuration file. A file absent at
   startup stays unused for the worker's lifetime, as before.

## Consequences

- No setting from a configuration file that device mode refuses is acted on, logging included.
- The configuration file is checked once per worker start, at startup, rather than again
  immediately before the library loads. A file that passed is owned and writable only by system
  principals, so only they could change it in between, which is the same premise ADR-270's
  Consequences state for the window between the library check and the load.
- A device-mode worker whose configuration file fails the check now refuses to start even when
  the library itself would pass. Before, the combined check refused the library for the same
  finding, so the outcome for the client is unchanged; only the log message differs.
- `iso22900-service` validates its `vendor_struct_types` keys only when the settings apply, so a
  file absent at startup cannot fail the start through a later read.

## Alternatives considered

- **Keep the check in the services and read the logging settings after it.** Rejected: logging
  is set up before the service is constructed so that construction errors are logged, and the
  service constructors would each need to start logging themselves.
- **A process-wide value the launcher sets and the services read.** Rejected: the services are
  also constructed directly by tests, which would then run without a verdict or need a second
  check path; an argument to `new` makes the dependency explicit.
