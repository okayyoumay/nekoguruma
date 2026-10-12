# ADR-281: Worker Library Resolution Has No Config-File Path, the Agent Passes the Mode, One J2534 Device Is One Worker, and Pre-Release Compatibility May Break

**Date:** 2026-10-12
**Status:** Accepted
**Affects:** `docs/system-architecture.md` (3.4, 7.1, 7.2, 8.2.6, 8.8.1), `docs/j2534-0404-architecture.md`, `crates/vci-service-config` (`find_library_path`, `list_configured_library_paths`, docs), `crates/j2534-0404-registry`, `crates/iso22900-registry`, `crates/j2534-0404-service`, `crates/iso22900-service`, `crates/agent` (`launch.rs`, `link.rs`); ADR-029, ADR-030, ADR-033, ADR-034, ADR-035, ADR-036, ADR-107, ADR-195, ADR-223, ADR-235, ADR-240, ADR-228, ADR-270, ADR-278, ADR-280 (consequence on schema compatibility)

## Context

Design 7.1 and 7.2 fix where a library is found: the Linux J2534 definition directory, the
Windows registry and the D-PDU API root description file, all at locations fixed at build time,
with a runtime override in debug builds only (ADR-228 item 1). The shared resolver
`ngr-library-resolver` implements those chains and the two 7.2 checks (ADR-270, ADR-278). Four
questions were left open before the worker services and the agent could switch over to it.

1. **The `library_path` override.** The worker services' `config.toml` can name a library path
   per library (`config.apis.<api>.libs."<lib>".library_path`). `resolve_library_path` in both
   registry crates reads it before the registry or root file, and the agent's `lookup` mirrors
   that order. The shared resolver does not read it. A path taken from a configuration file
   bypasses the fixed locations of 7.1 and the checks that 7.2 attaches to the files that name
   a library.
2. **Device mode or user mode.** The checks report and the caller decides (ADR-270 item 5,
   ADR-278 item 4): device mode refuses a failing library, user mode reports and loads. The
   worker runs the checks itself immediately before loading (ADR-228 item 4), but its startup
   argument carries only the library name, and nothing tells it which mode applies.
3. **Several devices behind one J2534 library.** The `modules` entries of ADR-107 select a
   device by `pname`, but ADR-107 item (b) keeps one open device per worker process and the
   agent refuses a worker whose `GetModuleIds` returns more than one row. The maintainer wants
   each entry usable as a separate device, connected at the same time.
4. **Compatibility before the first release.** ADR-280 changes the IR schema version, and the
   runtime refuses any version it does not support. Whether older schema versions or stored
   journals must stay readable had no authoritative decision.

## Decision

1. **The `library_path` override is removed as a resolution source.** A worker resolves its
   library only through the chains of 7.1: the Linux definition directory, the Windows registry
   and the D-PDU API root description file. The agent's `lookup` resolves through the same
   shared resolver, with no override of its own. Tests use the resolvers' debug-only overrides
   (the definition directory of `j2534_definition_dir()`, the root file of `pdu_api_root_file()`,
   or `resolve_in_dir` / `resolve_in_root_file`) with test definitions that name `sim-vci` or
   the mock library. The `libs."<lib>"` section of `config.toml` itself stays: it holds
   `modules`, `can_channel_mode`, `vendor_ioctls`, `vendor_struct_types` and logging, keyed by
   the same library name the resolver resolves. `find_library_path` and
   `list_configured_library_paths` and their readers go with the resolver adoption work.
2. **The agent passes the mode as a startup argument.** The agent knows its device's mode,
   which is fixed to exactly one of the two (design 3.3, 6.2), and passes it to the worker
   next to the library name. The worker does not infer it and no configuration file sets it.
   Device mode refuses a library that fails a 7.2 check; user mode reports the findings and
   loads (ADR-270 item 5, ADR-278 item 4).
3. **One J2534 device is one worker process.** For a library with several `modules` entries,
   the agent starts a separate worker per device and passes the library name plus the entry's
   `label` as startup arguments. The `label` is the selector: it must be non-empty and unique
   within one library's `modules` list (today it is only the display text of
   `ModuleData.vendor_module_name`, and duplicates or empty values are allowed). The J2534
   worker validates this at startup and refuses to start when the rule is broken, and a label
   that matches no entry is a startup error. An index or handle is not used as the selector:
   it would silently point at a different device after the configuration is edited, while the
   label is what the operator sees. That worker opens only that module, with the entry's `pname`
   as `PassThruOpen`'s argument (ADR-107 item (a)), and the agent links each worker as its own
   device. The `pname` must also be unique within one library's `modules` list, checked at the
   same time: two entries naming the same target would let two workers, and two per-VCI locks,
   share one physical VCI. The per-VCI lock of design 8.8 is keyed by the library name and the
   entry's `pname`, the target the worker actually opens, not by the label. The agent reads the
   entry once, takes the lock for its `pname`, and passes that `pname` to the worker with the
   label; the worker refuses to start when the entry it reads for the label has a different
   `pname`, so an edit of `config.toml` between the two reads cannot make a worker open a target
   other than the one locked. Two different
   `pname` strings that a vendor library maps to the same device cannot be detected; that stays
   the configuration's responsibility.
   A worker started with a label keeps that entry's handle from ADR-107 Decision (c), which
   numbers modules 1..N in configuration order, so its position in the list is the handle in
   every mode. It lists only that row from `GetModuleIds` with that handle and refuses every
   other handle (Consequences). The agent takes the handle from the single row it receives and
   never computes one, and a log or trace shows the same handle as a worker started without a
   label. ADR-107 item (b) stays: a worker has one open device and no per-module state
   partition; simultaneous use comes from several worker processes. When no label is passed the
   worker behaves as before (every entry is listed, or the synthesized default entry when
   `modules` is absent); the agent passes a label whenever the library has `modules` entries.
4. **Before the first release, compatibility may break.** Changes to the IR schema and to the
   journal's stored state may break compatibility, with no migration and no acceptance of older
   versions, since nothing is stored outside this repository. The runtime keeps refusing a
   schema version it does not support. This decision is revisited when the first release is
   made.

## Alternatives considered

- **Keep `library_path` as a resolution source (item 1).** It is convenient for tests and for
  libraries with no definition. Rejected: it gives a configuration file a say over which file is
  loaded that the fixed locations were meant to remove, and the device-mode checks of 7.2 do not
  cover it; the shared resolver would have to grow a second, unchecked path.
- **Keep it as a debug-only override (item 1).** Rejected as redundant: the resolvers already
  have debug-only overrides for their naming files (ADR-228 item 1), and a test definition that
  names `sim-vci` exercises the real resolution chain instead of skipping it.
- **A setting in `config.toml` for the mode (item 2).** Rejected: the mode is a property of the
  device that the agent already holds, a second copy can disagree with it, and a file that an
  administrator edits separately from the agent's installation is the wrong place to decide
  whether checks are enforced.
- **The worker infers the mode (item 2)** from its account, its parent or its environment.
  Rejected: each inference is a heuristic that can fail open, and the worker then enforces a
  policy it does not own.
- **Select the device by config index or module handle (item 3).** Rejected: an index would
  silently point at a different device after the `modules` list is edited (an entry added,
  removed or reordered), while the label is the text the operator sees and chose.
- **Accept older schema versions or migrate journals now (item 4).** Rejected: nothing is stored
  outside this repository yet, so the code and tests for a range of versions would serve no one.
- **One worker opens several devices at once (item 3).** Rejected for now: it revises ADR-107
  item (b) and needs the worker's state (`device_id`, module state, shared channels, logical
  links, subscriptions, the poll tasks) partitioned per module, which ADR-107 judged a much
  larger change than selecting one device.

## Consequences

- Libraries that no definition, registry entry or root file names cannot be loaded by a worker,
  not even for a test. Tests need a definition or root file; the debug-only overrides provide
  one without touching the machine's locations.
- A `library_path` key left in an existing `config.toml` has no effect once the code is
  changed.
- The startup argument contract of both workers gains the mode, and the J2534 worker gains an
  optional module label. A worker started with a label reports only that entry from
  `GetModuleIds`, with the entry's ADR-107 Decision (c) handle (its configuration position,
  not 1), and refuses `ModuleConnect`, and every module-scoped request, for any other
  module handle, so a client cannot open another configured device than the one its per-VCI
  lock names. The agent's rule that a worker report exactly one module (ADR-235 item 4) stays
  as it is: the job's VCI is chosen by which worker the agent starts (library name and label),
  not by picking a row from `GetModuleIds`.
- Some vendor DLLs may not support being loaded in two processes at once, for example when
  they take a process-wide lock on a driver. Such a library stays limited to one device at a
  time; this is a property of the library and is not detected by the agent.
- Each running J2534 device costs one worker process, which refines design 3.4's one process per library for J2534. The per-VCI lock (design 8.8) stays per device, keyed by the library name and `pname`.
- ADR-280's consequence that a program or journal of the previous schema version is refused is
  now this decision's rule (item 4), not a limit to be lifted before release. After the first
  release, a schema change needs a compatibility path, or a new decision.
- A `modules` list with an empty or repeated label, which loaded before, makes the J2534 worker
  refuse to start once item 3 is implemented.
- Implementation work (the resolver adoption, removing the key and its readers, the startup
  arguments, the agent's per-device workers) is tracked in the backlog, not here.
