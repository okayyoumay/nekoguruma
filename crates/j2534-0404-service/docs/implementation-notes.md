# j2534-0404-service Implementation Note

## Scope

Acts as a D-PDU API compliant gRPC service that maps ISO 22900-2 operations onto SAE J2534-1 (v04.04) PassThru calls. Manages service lifecycle through stdio JSON-RPC and a local gRPC server.

## Assumptions

- Startup argument contract is `j2534-0404:<library name>?port=<u16>&...` with required scheme and non-empty library name.
- `<library name>` resolves to a path via `j2534_0404_registry::resolve_library_path()`, which checks a `library_path` entry in the shared `config.toml` first, falling back to the Windows registry (`j2534-0404-registry`) when no such entry exists. See `startup-spec.md`.
- Local gRPC server starts before JSON-RPC request loop begins.
- JSON-RPC stdio framing uses one-line JSON messages.
- Physical channels are shared across Logical Links (CLLs) that use the same hardware protocol and baud rate.
- RX polling occurs on a 10 ms background task per physical channel.
- CAN-family CLLs are mapped onto J2534 channels according to the per-library `can_channel_mode` config key (`single-channel` default / `dual-channel` / `software-isotp` / `auto`), loaded once at startup via `vci_service_launcher::config::find_can_channel_mode()`; an invalid value fails startup (ADR-046, ADR-047). `auto` resolves its `dual-channel`-vs-`single-channel` decision at the first ISO15765-family connect and caches it (`J2534Service::effective_can_channel_mode()`); it never resolves to `software-isotp`.
- **Device selection is config-declared, but the service still opens at most one device at a time (ADR-107).** `config.apis.j2534-0404.libs."<lib>".modules` (a TOML array-of-tables, each entry `{ label, pname }`) is loaded once at startup via `crate::config::resolve_modules` (`J2534Service::modules: Arc<Vec<config::ModuleEntry>>`, never empty — `modules = []` fails startup, as does a non-ASCII/embedded-NUL `pname`, as does a config file that exists but cannot be read or parsed at all (`crate::config::resolve_modules` treats `vci_service_launcher::config::find_modules`'s `Err` as a startup error rather than falling back to the default single module — ADR-107 addendum (j), closing a Codex-review P1 finding that a malformed `modules` entry was silently swallowed into "not configured" by the shared crate's normally-lenient TOML loader)); `module_handle` is the entry's 1-based array position, and `modules` absent synthesizes one default entry (`pname = None`) matching pre-ADR-107 behavior byte-for-byte. `J2534Service::require_module_handle` is the single range-check choke point (generalized from a fixed `== DEFAULT_MODULE_HANDLE` to `1..=self.modules.len()`); `ensure_open_device_for` is the strict, module-selecting device-open path. **Every RPC that itself receives and validates a `module_handle` AND is allowed to lazily open a device uses `ensure_open_device_for(module_handle)`** — `ModuleConnect`, `GetVersion`, and `CreateComLogicalLink` — so a request for module 2 either opens module 2 or is rejected with `PDU_ERR_RESOURCE_BUSY`/`FailedPrecondition` if a different module is already open; it can no longer silently be served by whatever device happens to be open (ADR-107 follow-up fix, closing ADR-107's former Accepted Residual #3, identified during edge-case review). The plain, non-selecting `ensure_open_device` remains for the small set of genuinely handle-less internal helpers reached only after `CreateComLogicalLink` already pinned a module down for that `cll_handle` — `rpc_connect_com_logical_link`, `ensure_uudt_companion_channel`, `probe_sae_j1850_flavor` — which take only a `cll_handle`, not a `module_handle`. **`ModuleDisconnect` (`rpc_module.rs`) and all 7 module-scoped IOCTL handlers -- `PDU_IOCTL_RESET`/`READ_VBATT`/`SET_PROG_VOLTAGE`/`READ_PROG_VOLTAGE`, plus `GENERIC`/`GET_CABLE_ID`/`READ_IGNITION_SENSE_STATE` (unsupported by this adapter, but still connection-gated first, Codex review PR #143) (`rpc_misc.rs`) -- are also module-handle-scoped but must not open a device on the caller's behalf** (ISO 22900-2 §9.4.29.2 NOTE 1/Table 12 -- only `GetResourceIds`/`GetObjectId`/`GetConflictingResources`/`GetStatus` are allowed to run before `ModuleConnect`; `PDUIoCtl` is not among them), so neither calls `ensure_open_device_for`. `ModuleDisconnect` calls `lock_device_for(requested)`, which checks `device_id` and rejects with the same `PDU_ERR_RESOURCE_BUSY`/`FailedPrecondition` shape as `ensure_open_device_for`'s mismatch branch (both share one error-formatting function, `open_under_different_handle_status`) without opening or closing anything itself, and is a no-op (not a rejection) when nothing is open at all -- disconnecting an already-unconnected module is harmless and this "nothing open" case is deliberately left as-is (out of scope for A2-8, see below). All 7 IOCTL handlers instead call a fourth, sibling helper, `require_connected_device_for(requested)`: it shares `lock_device_for`'s mismatch-rejection behavior, but additionally rejects with `PDU_ERR_MODULE_NOT_CONNECTED` (`module_not_connected_status`) rather than proceeding when nothing is open at all, AND rejects the same way when `device_id` is open but `module_state.status != PduModstReady` (Codex-review fix, PR #143, P1) -- `events::handle_channel_hard_error` sets `module_state.status` to `PduModstNotAvail` on a lost-comm event without closing `device_id`, so an open-but-stale slot must still reject per ISO 22900-2 §9.4.29.2.1 use case (a) (only `PDU_MODST_READY` allows API function calls). **`PDU_MODST_NOT_AVAIL` is sticky until an explicit `ModuleDisconnect` (ADR-131, amending ADR-107 Decision (d)).** `rpc_module_connect` (`rpc_module.rs`), after `ensure_open_device_for` succeeds, reads `module_state.status` while still holding the returned `device_id` guard (closing the TOCTOU window a concurrent `ModuleDisconnect` would otherwise open) and rejects with `PDU_ERR_FCT_FAILED` (carrying the tracked `PDU_ERR_EVT_LOST_COMM_TO_VCI` in `ErrorDetail`) if `status != PduModstReady` -- it does NOT reset `module_state` to Ready itself, on either branch. This is the second and corrected form of a fix that went through two Codex-review rounds on the same mechanism (PR #143, both P1): the first attempt unconditionally reset `module_state` to `ModuleState::default()` after every successful `ensure_open_device_for` call including the already-open no-op branch, which a second review round correctly rejected as a false success -- that branch makes no native call, so declaring the module `READY` again was a blind assertion. ADR-131 resolved it via a closer spec reading: ISO 22900-2 §9.4.29.2 Behaviour (d) (the step that moves the module status to `PDU_MODST_READY`) only applies after Behaviour (a)-(c) succeed, and the spec's use-case table defines no `NOT_AVAIL -> READY` transition via `ModuleConnect` at all; NOTE 2 and §9.4.30 prescribe the only recovery sequence, `ModuleDisconnect` then `ModuleConnect` again, whose fresh `PassThruOpen` (`ensure_open_device_inner`, unchanged) is the only place `module_state` is ever reset to `PduModstReady`. See ADR-131 for the full account and the two rejected alternatives (forced reopen, native-call revalidation) -- **conformance-audit fix A2-8**: `READ_VBATT`/`SET_PROG_VOLTAGE`/`READ_PROG_VOLTAGE` used to call `ensure_open_device_for` (silently connecting the module on the caller's behalf), `PDU_IOCTL_RESET` used to call `lock_device_for` (silently proceeding as a no-op reset), and `GENERIC`/`GET_CABLE_ID`/`READ_IGNITION_SENSE_STATE` never checked connection state at all before falling through to their unconditional `Status::unimplemented` -- none of which is spec-conformant (ISO 22900-2 §9.4.29.2 NOTE 1 applies to every `PDUIoCtl` call regardless of whether the specific command is supported). **`ioctl_set_prog_voltage` calls `require_connected_device_for` before parsing its `input_data` payload, not after** (Codex-review fix, PR #143) — it originally validated `PDU_IT_IO_PROG_VOLTAGE` first, so a disconnected module with missing/malformed input got `InvalidArgument` instead of `PDU_ERR_MODULE_NOT_CONNECTED`, inconsistent with every other module-scoped IOCTL where the connection check is the first thing that runs. **`lock_device_for`/`require_connected_device_for` return the held `MutexGuard` rather than releasing it after the check** (renamed from the original `reject_if_open_under_different_handle`, which peeked and released) — an edge-case-hunter finding on that first fix showed a peek-and-release guard left a window, between the check and the caller's later teardown/reset work, in which a concurrent request could open/close a different device and defeat the guard entirely (TOCTOU). `rpc_module_disconnect` and the four device-touching IOCTL handlers hold the returned guard for their entire function body: `rpc_module_disconnect` binds it as `slot` and reuses it for the later `slot.take()` teardown instead of re-locking `device_id`, and each of `ioctl_reset`/`ioctl_read_vbatt`/`ioctl_set_prog_voltage`/`ioctl_read_prog_voltage` binds it as `_device_guard` (`ioctl_reset`) or destructures it alongside the `DeviceId` (the other three) and keeps it alive to the end of the function via drop order. The three unsupported IOCTL handlers (`GENERIC`/`GET_CABLE_ID`/`READ_IGNITION_SENSE_STATE`) drop the guard immediately after the check (`let _ = ...`) instead, since they never touch `DeviceId` -- they only need the connection-state gate itself, not a held guard across any subsequent native call. `device_id` is the strict outermost lock in this service (ADR-107 addendum) — never acquired while any other service lock (`logical_links`, `shared_channels`, `primitives`, `subscriptions`, `module_state`, `api`) is already held — so holding it across these functions' other lock acquisitions is safe by construction. **`GetVersion`, `ConnectComLogicalLink`, and every companion-CAN-channel open/join now also reject a `NotAvail` module the same way (ADR-134, closing the two residuals ADR-131 left open)** — `rpc_get_version` and `rpc_connect_com_logical_link` each gained the same scoped `module_state.status == PduModstReady` check (held-guard-continuous with the caller's own `device_id` acquisition, same discipline as `rpc_module_connect`/`rpc_create_com_logical_link` above) immediately after their respective device-open call succeeds; `ensure_uudt_companion_channel` gained its own copy of the same check, since it is the single choke point reached not only from `ConnectComLogicalLink`'s tail and `probe_can_channel_mode` but also from `promote_unique_resp_id_table` (`CoptUpdateparam` execution on an already-connected CLL) — a path with no other gate anywhere in its call chain. `probe_sae_j1850_flavor` (the SAE J1850 autodetect probe, which runs before `ConnectComLogicalLink`'s gate) gained a fourth copy of the same check too (ADR-134 Correction, Codex review round on the same PR): its `active_probe` branch transmits a real OBD Mode 01 PID 00 request on the vehicle bus, so ADR-134's first-cut characterization of this window as harmless/read-only was itself wrong, not merely an accepted residual — see ADR-134 for the full audit and that Correction. **`SharedChannel` also carries a `dead: bool` tombstone (ADR-134 round-6 Correction) set by `events::handle_channel_hard_error`, under the same `shared_channels` guard as its per-CLL teardown, for the entry matching the channel that just hard-errored** — closing a gap the module_state gates above cannot: `rpc_connect_com_logical_link`/`ensure_uudt_companion_channel`'s existing-channel-join branches used to reuse a `SharedChannel` entry's `channel_id` with no liveness check at all, so a join racing ahead of `handle_channel_hard_error`'s later `module_state = NotAvail` write (which happens only after `shared_channels` is released) could publish a CLL as `connected` on a channel whose poll task is exiting -- a permanent, silent hang no future RX poll would ever revisit, unlike a fresh `PassThruConnect` on a truly dead device (which fails the native call outright). Both join branches now reject a `dead` entry before their own reciprocal-filter checks. The entry itself is deliberately not removed at hard-error time (`channel_key` is preserved on the affected CLLs specifically so a later `Disconnect`/`Destroy` can still release it via `release_shared_channel_ref`, which already no-ops gracefully on an absent key) -- removing it early would let a fresh connect reinsert at the same key and have a stale CLL's later teardown decrement/disconnect the wrong (new) channel.
- **Vendor IOCTL (`cmd_id >= 0x10000`) buffer shape/sizes are config-declared, loaded once at startup, and validated before any lock is taken (ADR-219, as amended).** `config.apis.j2534-0404.libs."<lib>".vendor_ioctls."0x<cmd_id>"` (each entry `{ shape, input_bytes, output_bytes, input_required, output_required }`, `vci_service_launcher::config::VendorIoctlConfigEntry`) is loaded and fail-fast validated once at startup via `crate::config::resolve_vendor_ioctls` into `J2534Service::vendor_ioctls: Arc<HashMap<u32, config::VendorIoctlContract>>` — an unrecognized `shape` string, a malformed hex key, `input_required`/`output_required` set alongside `shape = "raw"` (or `input_bytes`/`output_bytes` set alongside `shape = "sbyte_array"`), a `cmd_id` colliding with one of this service's own ~28 reserved `PDU_IOCTL_BASE`-derived ids (`service::is_reserved_service_ioctl_id`, since `rpc_io_ctl`'s dedicated handler for that id would intercept every request before the vendor dispatch path ever ran), or a `shape = "raw"` `input_bytes`/`output_bytes` above `VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES` (16 MiB — a startup-rejection ceiling on the configured value, distinct from the removed fixed allocation cap: a value within it is honored exactly as configured, one above it fails startup instead of letting `AlignedByteBuf::zeroed` attempt an unbounded allocation the first time any client sends any request to that `cmd_id`) fails startup, mirroring `modules`/`can_channel_mode`'s own convention. `rpc_misc.rs::rpc_io_ctl_vendor` looks a `cmd_id` up in this map and calls `validate_vendor_ioctl_buffers` BEFORE any handle resolution or `shared_channels`/`api` lock acquisition — a rejected call never takes those locks. **Every vendor `cmd_id` must have an entry, with no exception for a nominally bufferless one:** a request against a `cmd_id` with no configured entry is unconditionally rejected `FAILED_PRECONDITION`, even one carrying no input and requesting no output (a design-advisor consult, PR #133 seventh round, reversed an earlier exemption for exactly that bufferless case — a vendor command's `pInput`/`pOutput` requirements are a property of the `cmd_id` alone, not of what a particular client request asks for, so an unconfigured command could still crash on a NULL pointer it always dereferences regardless of the client's own request shape; a genuinely bufferless command is allowlisted as `shape = "raw"` with both byte counts left at `0`). A configured `cmd_id` whose shape does not match the client's flags-bit-2 selection (raw vs. `SBYTE_ARRAY`) is rejected `INVALID_ARGUMENT` — the client picks the wire shape, but only the operator-declared contract knows which shape a given command's real native side actually expects, closing an out-of-bounds-write class two earlier same-PR fixes (a fixed 64 KiB raw-mode allocation cap, then padding both raw buffers to that cap) did not: the cap was itself an invented bound with no per-command basis, and wrapped mode was independently reachable for a raw-contract command via the client-selectable shape bit, writing through the small embedded `SBYTE_ARRAY` struct field. Raw mode's backing allocations are now sized EXACTLY to the configured `input_bytes`/`output_bytes` (client input shorter than `input_bytes` is zero-padded up via `pad_to_capacity`; longer input, or `output_capacity` above `output_bytes`, is rejected `INVALID_ARGUMENT`); a nonzero `input_bytes`/`output_bytes` also mandates that direction's presence regardless of the other. Wrapped mode's `input_required`/`output_required` fill the identical "this direction is mandatory" role for `SBYTE_ARRAY` presence, since wrapped mode has no byte count of its own to double as one; its allocation otherwise keeps the renamed `VENDOR_IOCTL_MAX_WRAPPED_CAPACITY` (64 KiB) as a resource sanity cap only, since its `SBYTE_ARRAY.NumOfBytes` already self-describes the real written length. See ADR-219 and ADR-218 (the identical governing precedent this design mirrors: operator config is the sole source of a native-only-known contract, no client/default trust).

## Implementation Policy

- `get_status`'s `error` field and `map_construct_error`/`map_registry_error` (`error.rs`) must stay sanitized: never format a raw `libloading::Error` or `j2534_0404_registry::RegistryError::Io`/`io::Error` (Display or Debug) into client- or launcher-facing text, since it can contain a local filesystem path or raw OS error text. Log the full error server-side via `tracing::error!` instead (ADR-089). Any new call into `j2534_0404_registry` (or another crate returning an error type that can wrap `io::Error`/`libloading::Error`) from `Service::new()` must route through one of these mapping functions rather than a bare `?`.
- Keep module boundaries explicit: `rpc_module`, `rpc_link`, `rpc_primitive`, `rpc_misc` each own a distinct RPC surface.
- Protocol name and ComParam resolution is centralized in `names.rs` and `comparam_support.rs`. `names.rs::map_comparam_name` delegates to one `map_comparam_name_{native,service_timing,transport,physical}` helper per D-PDU API mapping-table section; add new shortname aliases to the matching section rather than growing a single flat match. `map_comparam_name` is protocol-agnostic with no exception (one shortname → one `ComParamId`, always, for every caller — `GetComParam`, `SetComParam`, `SetUniqueRespIdTable`, and the channel-context-free `GetObjectId` all resolve through the same `names.rs::resolve_comparam_name`, ADR-103) — whether the resolved id is actually usable on a given CLL's protocol is checked separately, after resolution, by `comparam_support::check_param_allowed`. `CP_P3Phys`/`CP_P3Func` always resolve to the CAN-context service-level `PARAM_P3_PHYS`/`PARAM_P3_FUNC` (never to native `P3_MIN`, on any protocol); `CP_P3Min` is the distinct, KWP-only name for `P3_MIN`. A previous design (removed) special-cased `CP_P3Phys`/`CP_P3Func` to alias `P3_MIN` on non-CAN protocols on the mistaken premise that KWP's `P3_MIN` was an interchangeable substitute — see ADR-104.
- **`GetResourceIds`/`CreateComLogicalLink` resolve through the static resource table in `resources.rs` (ADR-069), not a bare protocol ID.** `resources.rs::RESOURCE_TABLE` is the single source of truth for the 47 resource IDs (`0x0201..=0x022F` — 37 through `0x0225` plus 10 SWCAN rows `0x0226`-`0x022F`, ADR-164/Phase 4) and 9 bus type IDs (`0x0301..=0x0309`) — add new resource/bus-type rows there, not as hardcoded numeric literals elsewhere. Both namespaces are opaque MDF-style handles, disjoint from `ChannelProtocol` values; a caller must discover them via `GetResourceIds` and pass them back verbatim. `names.rs::resolve_resource_ids_from_data` AND-filters table rows across every supplied selector, falling back to the legacy raw/extended-`ChannelProtocol`/`map_bustype_name` interpretation only when a selector matches no table row at all — this is what keeps a legacy caller (bare protocol ID/name) working unchanged. `names.rs::parse_protocol_id_from_resource` resolves `CreateComLogicalLink`'s resource field through the table first (returning the matched row alongside the `ChannelProtocol` so `rpc_create_com_logical_link` can derive ComParam defaults from the row's own canonical `bus_type_name`/`protocol_name`), falling back to `ChannelProtocol::from_raw`/`map_protocol_name` for anything not in the table. `names.rs::find_table_row_by_name` rejects (`invalid_argument`, listing the available `config_name`s/resource IDs) a name matching multiple table rows that differ in `ChannelProtocol` **or** `hw_protocol_override` (spec correction; e.g. `"SAE_J2610_SCI"`'s four configurations, or `"SAE_J2610_on_SAE_J2610_SCI"`'s four hardware overrides) rather than silently picking the first match in table order; a name matching rows that all share one `ChannelProtocol` *and* `hw_protocol_override` (true alias rows) is unaffected. **`ResourceDef::dlc_pins` is typed** (`(pin_number, pin_type_id)` pairs, pin type IDs matching `map_pintype_name`'s 2000-range values) — `RscData`'s `dlc_pin_data` can narrow an otherwise-ambiguous `protocol_name` match to exactly one row via `find_table_row_by_name`'s pin-aware path (`retain_rows_matching_pin`, shared with `resolve_resource_ids_from_data`'s row-based `GetResourceIds` pin filtering); narrowing to zero rows is `invalid_argument`, narrowing to one resolves regardless of remaining `ChannelProtocol`/`hw_protocol_override` differences. `ResourceName` (a bare string) never gets this narrowing. Combined-bus-type rows fix one connect protocol per row — except the renamed `SAE_J1850` bus (`0x0307`, formerly `SAE_J1850_VPW_and_SAE_J1850_PWM`), whose `J1850VPW` is only the initial candidate for a connect-time VPW/PWM auto-detect probe (ADR-070, see below); the combined K-line bus still fixes one protocol with no probing. `SAE_J2610_SCI` and `SAE_J2610_on_SAE_J2610_SCI` each expand to 4 rows, one per SCI configuration — the latter via `hw_protocol_override` (spec correction) rather than a distinct `ChannelProtocol`, since ComParam defaults/`TX_FLAG_SCI_MODE` are identical regardless of which SCI wiring is used; `SCI_MODE` is therefore no longer used as a connect protocol by any table row (`rpc_create_com_logical_link` prefers `resource_row.hw_protocol_override` over `can_channel_mode.hw_protocol_id(protocol)` when set — SCI is unaffected by `can_channel_mode`/J1850 autodetect either way, so this cannot conflict with either). **Every row's `bus_type_name` must resolve via `bustype_default_params`, and every row's `protocol_name` must either resolve via `protocol_default_params` or be listed in `comparam_defaults::PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS`** (the five native-`ChannelProtocol` rows whose "protocol name" is really an ISO 22900-2 BUSTYPE name already covered by the bus default alone: `ISO_15765_2`, `ISO_9141_2`, `ISO_14230_4`, `SAE_J1850_PWM`, `SAE_J1850_VPW`) — `resources.rs`'s `every_resource_row_has_protocol_and_bustype_defaults_or_is_allowlisted` test enforces this exhaustively over all 37 rows, so a row with neither a preset nor an allowlist entry fails the test instead of silently shipping an incomplete Working set (three separate PR reviews each caught one more missing-entry gap before this test existed — do not add a new row without either giving it a `comparam_defaults.rs` entry, reusing the closest spec-identical preset, or adding it to the allowlist with a reason). `rpc_get_resource_status` (`rpc_link.rs`) resolves its own `resource_id`/`resource_name` input the same table-first way (`names.rs::find_table_rows_by_name` for names) as `CreateComLogicalLink`, falling back to `ChannelProtocol::from_raw`/legacy name mapping — do not compare a `resource_id` input against `link.protocol.value()` directly, or resolve a `resource_name` via the legacy `map_protocol_name` alone, since neither ever matches a table resource ID/table-only name. Unlike `CreateComLogicalLink`, it treats an ambiguous name as "match any of its `ChannelProtocol`s" rather than an error, since it is a filter, not a creation request. Its response `resource_id` echo is table-aware too, not a raw `ChannelProtocol` value: it prefers a direct table-name match's *own* row (`names.rs::find_table_rows_by_name`), else the table-order tie-break for a protocol resolved only via legacy name mapping (`resources.rs::find_resource_id_for_protocol`, e.g. alias rows or a legacy-only name like `"ISO15765"`), preferring whichever matched row has an active link when the name matched several (e.g. `"SAE_J2610_SCI"`). `rpc_get_conflicting_resources` (ADR-106) no longer shares this resolution path — see `docs/protocol-mapping.md`'s `GetConflictingResources (ADR-106)` section for its static-table-only resolution (`resources::find_by_resource_id`/`names::find_table_rows_by_name`, no legacy fallback either way). `GetResourceIds`'s `protocol_id` *selector* is the inverse direction and takes a `ChannelProtocol`/J2534 value, never a resource ID.
- **`rpc_get_resource_status`'s `resource_status` bitfield (Table D.1) is computed by a SEPARATE match from `active_candidate`'s own `ChannelProtocol`+`connected` match (ADR-127, finding A2-3).** Bit 0 ("in use") and bits 2/3 (TX-queue/ComParam lock, from `LogicalLinkState::held_lock_mask`, ADR-123) match every resolved candidate's own derived `hw_protocol_id` (computed the same way `CreateComLogicalLink` derives a new CLL's `hw_protocol_id`) against every link's `hw_protocol_id` — **as of ADR-157, the link side of that comparison is `link.base_hw_protocol_id()`, not `link.hw_protocol_id` directly, so a `_PS` link (whose `hw_protocol_id` is a `_PS` variant) still correctly reports its base protocol's resource as occupied; see the "hw_protocol_id _PS Normalization" section below** — regardless of `link.connected`, a created-but-unconnected CLL already counts as "in use" (§9.4.9.2 c)), and a lock held pre-connect (`LockResource` allows this) still surfaces. This is deliberately not the same comparison as `active_candidate` (`ChannelProtocol` equality + `connected`), which is unchanged and still exists only for the `resource_id` echo tie-break below it (preferring the actually-connected configuration for an ambiguous name) — do not fold the two matches together, or collapse them into one, without re-reading ADR-127: `hw_protocol_id` equality is required so a software-ISO-TP raw-CAN CLL and its ISO15765 sibling (ADR-046, sharing one physical CAN channel under two different `ChannelProtocol` values) correctly see each other's lock/in-use state, which `ChannelProtocol` equality alone would miss. Both matches run under the same single `logical_links` lock acquisition, gated by the same open-module check (ADR-107). Bit 1 (Availability Status) is untouched, always 0 — out of scope of A2-3.
- **`GetObjectId(OBJT_RESOURCE, shortname)` (`names.rs::resolve_object_id`, called from `rpc_misc.rs::rpc_get_object_id`) shares `CreateComLogicalLink`'s exact table resolution, not its own logic.** It calls `find_table_row_by_name` directly (unique match → that row's ID; ambiguous match, e.g. `"SAE_J2610_SCI"` → `invalid_argument`, same rejection `CreateComLogicalLink` uses) and falls back to `find_resource_id_for_protocol`/the raw `ChannelProtocol` value for a legacy-only name, same as the status RPCs above. There is no "match any" reading here (unlike `GetResourceStatus`/`GetConflictingResources`) since this is a single-value name→ID lookup, not a filter. The now-removed `map_object_type_name` wrapper (`map_protocol_name(name).map(|p| p.value())`) was this arm's *only* caller — do not reintroduce a legacy-only resolution path for `OBJT_RESOURCE` without re-adding a similar wrapper *and* re-checking this same class of gap (three earlier PR-review passes each found one more RPC/row still using the legacy-only path before this fourth pass covered `OBJT_RESOURCE`). **A shortname matching none of the above is rejected with `PDU_ERR_INVALID_PARAMETERS` (`Status::invalid_argument`, ADR-078), not the pre-ADR-078 `0` fallback** — this applies to every `resolve_object_id` arm except `OBJT_PROTOCOL` (unchanged `Status::not_found` behavior); `OBJT_BUSTYPE`/`OBJT_COMPARAM`/`OBJT_PINTYPE` lose their `shortname.parse::<u32>().unwrap_or(0)`-style trailing fallback the same way, and `OBJT_IO_CTRL` — which had no name table at all, only that fallback — now always rejects. Do not reintroduce a numeric-string fallback for these five object types without updating ADR-078.
- **`GetObjectId(OBJT_PROTOCOL, ...)`/`GetObjectId(OBJT_BUSTYPE, ...)` share `OBJT_RESOURCE`'s table-first convention (A1-2 conformance-audit fix, extending ADR-069) — but `OBJT_PROTOCOL` does NOT reuse `find_table_row_by_name` (Codex-review fix, PR #115).** `OBJT_PROTOCOL` uses a dedicated helper, `find_protocol_for_name`, which only rejects a name as ambiguous when its matching rows differ in `ChannelProtocol` itself — unlike `find_table_row_by_name` (used by `OBJT_RESOURCE`/`CreateComLogicalLink`), which *also* treats differing `hw_protocol_override`s as ambiguous, since those callers must pick one specific hardware configuration. Reusing `find_table_row_by_name` directly for `OBJT_PROTOCOL` briefly regressed `"SAE_J2610_on_SAE_J2610_SCI"` (previously resolvable via `map_protocol_name`): its four table rows share one `ChannelProtocol` (`0x0160`) and differ only in `hw_protocol_override`, which is irrelevant to a protocol-identity query, so `find_protocol_for_name` resolves it unambiguously while `find_table_row_by_name` would still (correctly, for `OBJT_RESOURCE`'s purposes) reject it. A name spanning genuinely distinct `ChannelProtocol`s (e.g. bare `"SAE_J2610_SCI"`, four different SCI protocols) stays ambiguous under both helpers. `OBJT_BUSTYPE` cannot reuse `find_table_row_by_name` either (it only matches `protocol_name`/`config_name`, not `bus_type_name`), so it uses a new helper, `find_bustype_id_by_name` — a plain case-insensitive first-match over `resources::resource_table()` by `bus_type_name`, with no ambiguity handling: unlike `protocol_name` (which can span several distinct `ChannelProtocol`s), `bus_type_name` is 1:1 with `bus_type_id` for every row by construction, so the first match is always authoritative. All three arms fall back to their pre-existing legacy alias-map resolution unchanged when no table row matches.
- **The `SAE_J1850` bus (`0x0307`) auto-detects VPW vs. PWM at `ConnectComLogicalLink`, gated by `ChannelProtocol::needs_j1850_autodetect` (true only for `SAE_J2190_ON_SAE_J1850`/`ISO_15031_5_ON_SAE_J1850`, values `0x0152`/`0x0153`) — ADR-070.** `rpc_link.rs::autodetect_sae_j1850_flavor` runs before the channel_key/shared-channel logic in `rpc_connect_com_logical_link`, consulting a per-module cache (`J2534Service::j1850_bus_flavor`, an `Arc<Mutex<Option<u32>>>` held across the probe's `.await` points like `resolved_can_channel_mode`, ADR-046) so only the first such CLL on a module pays the probe latency. `probe_sae_j1850_flavor` connects `J1850VPW` first (10.4k baud), **installs a pass-all filter on that temporary channel before reading (P1 fix -- real J2534 adapters silently discard RX until a filter exists, exactly like `connect_new_physical_channel`'s own filter; the mock's RX queue doesn't itself enforce this, so the fix is pinned via the `PassThruStartMsgFilter` call count instead, `j1850_autodetect.rs::probe_installs_a_pass_all_filter_on_each_candidate_channel`)**; for the OBD-active resource (`ISO_15031_5_ON_SAE_J1850`) it transmits a standard OBD-II functional Mode 01 PID 00 request (`J1850_OBD_PROBE_VPW`/`_PWM`, header bytes documented at their declaration) before reading, while the J2190 resource (`SAE_J2190_ON_SAE_J1850`) only listens — there is no universal J2190 probe request; any RX within `ComParamSet::j1850_autodetect_window_ms` (Working `CP_P2Max` when present, else `J1850_AUTODETECT_DEFAULT_WINDOW_MS` = 500 ms) wins that candidate, else it disconnects and retries on `J1850PWM` (41.6k); neither conclusive defaults to VPW with a logged warning rather than failing the connect. **`probe_sae_j1850_flavor` returns a `J1850ProbeOutcome` (`Conclusive`/`Fallback`), not a bare flavor, and `autodetect_sae_j1850_flavor` only writes the module-wide cache on `Conclusive` (verification-pass fix)** — a `Fallback` (e.g. the J2190 resource's passive-only probe seeing nothing on a quiet bus) still resolves that one CLL's `hw_protocol_id` to the VPW default, but leaves the cache empty so a later CLL that *can* actively probe (the OBD-capable resource) still gets to run its own probe instead of permanently inheriting an unconfirmed guess; once any CLL's probe is conclusive, the cache is authoritative for the rest of the module's lifetime. The winning flavor is written into `LogicalLinkState::hw_protocol_id` (the same ADR-046 divergence field, not a new one) regardless of conclusiveness, and, on a PWM win, `comparam_defaults::sae_j1850_pwm_override_params` is merged into the CLL's Working `ComParamSet` in place — **but only the keys that actually differ between the merged VPW and PWM presets (verification-pass fix), not the full PWM preset**, so a client's own `SetComParam` staged before `ConnectComLogicalLink` on a same-named flavor-*independent* key survives instead of being clobbered (DATA_RATE/NETWORK_LINE/IFR_CTRL always differ and so always change; both bus-agnostic protocols' header/priority-byte ComParams -- `CP_FuncReqFormatPriorityType`/`CP_FuncRespFormatPriorityType`/`CP_PhysReqFormatPriorityType`/`CP_PhysRespFormatPriorityType` -- also differ VPW-vs-PWM and so always change too). **P1 fix: `comparam_defaults::iso_15031_5_on_sae_j1850_pwm` previously carried the VPW byte values for those four keys verbatim (copy-paste from its VPW counterpart), which hid them from the diff entirely and left a PWM-detected `ISO_15031_5_on_SAE_J1850` link sending the VPW format byte (`0x68`) instead of the PWM one (`0x61`) -- this also affected the fixed-PWM resource `0x0215` directly, which never goes through the probe at all.** Corrected to the PWM byte values (`0x61`/`0x41`/`0xC4`/`0xC4`), matching `sae_j2190_on_sae_j1850_pwm`'s already-correct values (resource `0x0217`, unaffected) and the `J1850_OBD_PROBE_PWM` probe constant -- see ADR-070's "PWM preset header bytes" paragraph. Do not add a third probe candidate or change the VPW-first order without updating ADR-070. `j2534-0404-mock`'s `__mock_set_j1850_bus_flavor` (backdoor: `MockBackdoor::set_j1850_bus_flavor`) drives this deterministically in tests: it makes the simulated bus "answer" only a connect opened with the matching protocol id, by queuing a canned response frame on that channel at connect time.
- Service-level ComParam ID constants (0x8000-0x80FF) and the `LockResource`/`UnlockResource` mask bits (`LOCK_PHYSICAL_COM_PARAMS`, `LOCK_PHYSICAL_TX_QUEUE`) live in `service/service_params.rs`, not `service.rs` itself — `service.rs` re-exports them via a private glob `use` so `rpc_*`/`events`/`comparam_*` submodules see them unqualified (`super::PARAM_X`).
- Any new RPC path that mutates hardware on a shared physical channel (a `PassThruIoctl SET_CONFIG`, `PassThruStartMsgFilter`/`StopMsgFilter`, etc.) must check for a conflicting `LOCK_PHYSICAL_COM_PARAMS`/`LOCK_PHYSICAL_TX_QUEUE` holder via `find_physical_lock_holder` (`service.rs`) rather than re-deriving the channel_key/protocol-fallback match inline — see ADR-043/045, all of which this helper backs at the RPC boundary. `CoptUpdateparam` also calls it, but no longer from the RPC boundary and no longer to reject the call: ADR-044's synchronous rejection (and `SetComParam`'s own, alongside it) is superseded by ADR-110, which instead calls this helper live inside `handle_update_param` (poll-task execution time) to resolve a lock conflict per-`PDU_PC_BUSTYPE`-param rather than rejecting the whole call.
- **`rpc_misc.rs::rpc_io_ctl` implements the 25 D-PDU `PDU_IOCTL_*` commands (17 from ADR-079, `SW_CAN_HS`/`SW_CAN_NS` from ADR-164 Decision 3/Phase 4, `START`/`QUERY`/`STOP_REPEAT_MESSAGE` from ADR-165/Phase 12, `READ_J1962PIN_VOLTAGE` from Phase 13, and `GET_DEVICE_CONFIG`/`SET_DEVICE_CONFIG` from ADR-176/Phase 14), split by target: `M` (module-level, `ModuleHandle`) vs. `L` (per-CLL, `cll_handle`).** Their IDs are minted from a private `PDU_IOCTL_BASE = 0x2900_0000` namespace in `service_params.rs` (`PDU_IOCTL_RESET` through `PDU_IOCTL_READ_IGNITION_SENSE_STATE`, plus `PDU_IOCTL_SW_CAN_HS`/`PDU_IOCTL_SW_CAN_NS`, `PDU_IOCTL_START/QUERY/STOP_REPEAT_MESSAGE`, `PDU_IOCTL_READ_J1962PIN_VOLTAGE`, and `PDU_IOCTL_GET_DEVICE_CONFIG`/`PDU_IOCTL_SET_DEVICE_CONFIG`), distinct from the raw J2534 ioctl IDs (`≤ 0x14`) that the 4 legacy arms (`CLEAR_RX_BUFFER`/`CLEAR_TX_BUFFER`/`CLEAR_PERIODIC_MSGS`/`CLEAR_MSG_FILTERS`, `rpc_io_ctl_legacy`) still accept unchanged. `names.rs::map_ioctl_name` resolves all 25 shortnames for `GetObjectId(OBJT_IO_CTRL, ...)`, superseding ADR-078's always-reject rule for that one object type only (every other arm's unknown-shortname `PDU_ERR_INVALID_PARAMETERS` behavior is untouched). See "SAE J2534-2 Extended Programming Voltage + J1962 Pin Voltage Read (Phase 13)" further below for `READ_J1962PIN_VOLTAGE`'s own detail and clause 15's `SET_PROG_VOLTAGE` error-mapping extension. SW_CAN_HS/SW_CAN_NS's own implementation detail is covered in the "SAE J2534-2 Single Wire CAN" section below, and START/QUERY/STOP_REPEAT_MESSAGE's in the "SAE J2534-2 Repeat Messaging" section further below, not repeated here. New per-CLL `LogicalLinkState` fields back the L-scoped commands: `tx_suspended`/`tx_held: VecDeque<TxItem>` (`SUSPEND`/`RESUME_TX_QUEUE` — `events::dispatch_tx_item` siphons a suspended CLL's items into `tx_held` instead of executing them, draining FIFO on resume; per-CLL ordering is preserved, cross-CLL ordering on a shared channel was never guaranteed), `event_queue_cap`/`event_queue_mode: EventQueueMode` (`SET_EVENT_QUEUE_PROPERTIES` — replaces the fixed `RX_BUF_CAPACITY` cap check at the `rx_buf` insert site with a per-CLL cap and an `OverwriteOldest`/`DiscardNewest` eviction choice), `client_filters: HashMap<u32, Vec<MessageFilterId>>` (`START`/`STOP`/`CLEAR_MSG_FILTER` — kept disjoint from `unique_resp_filter_ids` so client filter churn never disturbs the service's own ADR-039 flow-control filters; the value is a `Vec` — not a single id — because `ioctl_start_msg_filter` installs one hardware filter per applicable `TxFlags`/ID-width variant on a CAN channel connected with `CAN_29BIT_ID`/`CAN_ID_BOTH`, mirroring `install_pass_all_filter`'s existing dual-install approach, since a `TxFlags`-0 filter alone would never match 29-bit CAN Ids — Codex-review fix, round 12), and `result_buffer_limit` (`SET_BUFFER_SIZE`, consulted by `rpc_get_event_item` in `rpc_primitive.rs`, which truncates `ResultData.data_bytes` to at most this many bytes -- `header_bytes`/`footer_bytes` are untouched; `SubscribeEvent`'s live push fan-out in `events.rs` is a separate concern and is intentionally not truncated). `J2534Service::prog_voltage` mirrors `SET_PROG_VOLTAGE`'s last-applied value per pin. Only `CLEAR_TX_QUEUE` (`LOCK_PHYSICAL_TX_QUEUE`) and the three filter commands (`LOCK_PHYSICAL_COM_PARAMS`) call `find_physical_lock_holder` — `SUSPEND`/`RESUME_TX_QUEUE`, `CLEAR_RX_QUEUE`, `SET_BUFFER_SIZE`, and `SET_EVENT_QUEUE_PROPERTIES` touch only per-CLL state, not shared hardware, so they do not. `RESET` is a soft per-module state reset (clears every live CLL's queues/filters/suspend flag and flushes hardware RX/TX buffers) — it never `PassThruClose`s/reopens the device, and (A2-8) rejects with `PDU_ERR_MODULE_NOT_CONNECTED` via `require_connected_device_for` if the target module was never connected, rather than silently succeeding as a no-op. `READ_VBATT`/`SET_PROG_VOLTAGE`/`READ_PROG_VOLTAGE` reject the same way instead of lazily opening the device (A2-8; see "Device selection" above). `GENERIC`, `GET_CABLE_ID`, `SEND_BREAK`, and `READ_IGNITION_SENSE_STATE` are rejected as unsupported (`Status::unimplemented`) since this adapter has no underlying J2534 v04.04 capability for them — a deliberate product decision, not an oversight. **`Disconnect`/`DestroyComLogicalLink` (`rpc_link.rs`) also stop every surviving `client_filters` entry and drain `tx_held` (emitting `PduCopstCancelled` for each, via the same `cancel_link_cops` path used for `primitives`-queued COPs, resetting `tx_suspended`) before the CLL goes away** — this is required regardless of whether the physical channel is shared (a leaked hardware filter on a still-live sibling CLL) or this is the last CLL (a stale `tx_held` item must never survive a later reconnect-and-resume on the same `cll_handle`). `SET_EVENT_QUEUE_PROPERTIES`'s `event_queue_cap` is clamped to `MAX_EVENT_QUEUE_CAP` (`rpc_misc.rs`, 16× the old fixed `RX_BUF_CAPACITY`) rather than accepting an unbounded client-supplied value. **`events::cancel_held_tx_items` is the single place that drains a CLL's `tx_held` and notifies `PduCopstCancelled`** (Codex-review fix) — it wins a race against any concurrent `primitives`-scan cancellation via `primitives.remove(&cop).is_some()`, so a cop already cancelled through the `primitives` path is never double-notified. `RESET` and `PDU_IOCTL_CLEAR_TX_QUEUE` (`rpc_misc.rs`) both call it too (passing whether to also reset `tx_suspended`) instead of bare `tx_held.clear()`, so held COPs are properly cancelled — not silently stranded — under either command. `PDU_IOCTL_START_MSG_FILTER`'s per-filter install loop rolls back (`stop_message_filter`) every filter already installed in the same request if a later filter in that request fails, rather than leaving earlier successes live on hardware but untracked in `client_filters`. **`PDU_IOCTL_CLEAR_TX_QUEUE` checks `LOCK_PHYSICAL_TX_QUEUE` *before* calling `cancel_held_tx_items`** (Codex-review fix) — cancelling held items is not reversible, so a caller that gets `ResourceExhausted` must see no side effects. **`PDU_IOCTL_START_MSG_FILTER` also rejects outright when this CLL's physical channel is shared (`SharedChannel::ref_count > 1`)** (Codex-review fix) — a real `PASS_FILTER`/`BLOCK_FILTER` installs on the shared `channel_id`, not something `poll_rx_inner` can scope to one CLL, so a `BLOCK_FILTER` from one CLL would silently drop frames a sibling CLL still needs; duplicate/already-installed `FilterNumber` rejection (above) is checked first, then this shared-channel check, before any hardware call. **The legacy raw `CLEAR_MSG_FILTERS` arm (`rpc_io_ctl_legacy`) also clears `client_filters` for every CLL sharing that `channel_id`** (Codex-review fix) — the channel-wide hardware clear it performs invalidates every CLL's client-installed filter_ids on that channel, not just the calling CLL's, so leaving those map entries in place would make a later `STOP_MSG_FILTER` fail on a dead id and wrongly reject `FilterNumber` reuse as already-installed. **`SET_EVENT_QUEUE_PROPERTIES` trims `rx_buf` down to the new `event_queue_cap` immediately when lowering it** (Codex-review fix), instead of only converging as `push_rx_frame` evicts one entry per inbound frame. **`SET_EVENT_QUEUE_PROPERTIES` is now pre-Connect-only** (A2-5 conformance fix, ADR-126) — a connected CLL, or one with a `ConnectComLogicalLink` call already in flight for it (`LogicalLinkState::pdu_connect_begun`, round 2, backed by a cancellation-safe `connect_in_flight: Weak<()>` token claimed atomically at the top of `rpc_connect_com_logical_link`), is rejected with `PDU_ERR_CLL_CONNECTED` before anything is mutated; the immediate-trim behavior above is retained and stays reachable via a disconnect-then-reconfigure-then-reconnect flow, since disconnect clears `connected`/`channel_id` but not `rx_buf`. **`rpc_connect_com_logical_link` also rejects a second CLL joining a physical channel that already has a `client_filters` entry from its current sole owner** (Codex-review fix), the reciprocal of `START_MSG_FILTER`'s own shared-channel rejection — checked in the same TOCTOU-preventing critical section as the `ref_count` bump. **`PDU_FLT_PASS`/`PDU_FLT_PASS_UUDT` are rejected outright by `ioctl_start_msg_filter`, on any channel** (ADR-079 amendment, Codex-review fix) — every non-ISO15765 channel already carries a wide-open pass-all `PASS_FILTER` from connect time (ADR-008) so this service's own response matching sees every frame, and `poll_rx_inner` never consults `client_filters` in software, so a narrower client PASS filter would install on real hardware yet be a pure no-op. `PDU_FLT_BLOCK`/`_BLOCK_UUDT` are unaffected (J2534 BLOCK-wins precedence still restricts traffic genuinely); software-enforced PASS filtering was considered and deferred to a future ADR, since it collides with an unresolved question of whether a client PASS filter should also gate the frames a live `ComPrimitive` is waiting on. **`ioctl_start_msg_filter` now holds `shared_channels` as the outermost lock across its entire ref_count-check-through-`client_filters`-write sequence** (ADR-080, Codex-review fix) — previously it released `shared_channels` right after the ref_count read, leaving a TOCTOU window where a concurrent `ConnectComLogicalLink` could join the channel before this call's `client_filters` write, defeating both this function's and `rpc_connect_com_logical_link`'s reciprocal shared-channel checks. **`shared_channels` is the established outermost lock of the three (`shared_channels` → `logical_links`/`api`)** — see ADR-080; any new code acquiring `api`/`logical_links` and then `shared_channels` would invert this order and risk deadlock. **`PDU_IOCTL_RESET` also preserves `client_filters` entries whose `PassThruStopMsgFilter` call fails** (Codex-review fix), matching `PDU_IOCTL_CLEAR_MSG_FILTER`'s own failure-retention behavior, instead of unconditionally clearing the map regardless of whether the hardware filter was actually removed. **`ensure_uudt_companion_channel` (`rpc_link.rs`) also rejects joining a `(CAN, baud)` channel that already has an active `client_filters` entry** (Codex-review fix) — the same reciprocal guard `rpc_connect_com_logical_link` already applies to its own primary-CLL joins, closing the gap where a raw-CAN CLL's `BLOCK_FILTER` could otherwise drop an ISO15765 CLL's UUDT companion traffic on the shared channel. **`rpc_disconnect_com_logical_link` and `rpc_destroy_com_logical_link` also hold `shared_channels` continuously across their `client_filters` teardown and `ref_count` update** (ADR-080, Codex-review fix) — previously each dropped/removed `client_filters` under a briefly-held `logical_links` guard and only re-acquired `shared_channels` much later for the `ref_count` decrement, leaving a window where a racing `ConnectComLogicalLink`/`ensure_uudt_companion_channel` could see the CLL's filters already gone and join before the hardware filter was actually stopped. **`PDU_IOCTL_CLEAR_TX_QUEUE` excludes the CLL's currently-executing COP (`SharedChannel::executing_cop`) from `cancelled_cops`** (Codex-review fix) — `primitives` tracks a COP from `StartComPrimitive` until full completion, both while merely queued and while actively executing (ADR-021), but only queued items belong to "the TX queue"; a long-running `CoptSendrecv` already dispatched must keep running, not be treated like an explicit `CancelComPrimitive`. **`PDU_IOCTL_RESUME_TX_QUEUE` no longer re-injects `tx_held` items onto the shared `tx_queue`'s tail** (ADR-081, Codex-review fix) — doing so could reorder them behind this CLL's own items still in flight (un-dequeued) in that same shared, multi-CLL queue. It now clears `tx_suspended` and sends a content-free `TxItem::ResumeWake`; the poll loop's `drain_tx_held_backlog` (`events.rs`) flushes the actual backlog at the next dequeue for that CLL (real item or the wake itself), which is provably order-preserving since everything in `tx_held` is always older than anything still queued for the same CLL. **The legacy raw `CLEAR_MSG_FILTERS` arm (`rpc_io_ctl_legacy`) also holds `shared_channels` across its hardware clear and `client_filters` purge** (ADR-080, Codex-review fix) — without it, this arm could interleave inside a concurrent `ioctl_start_msg_filter` call (which holds `shared_channels` across its own install-then-record sequence), running its clear+purge after `START_MSG_FILTER`'s hardware install but before its `client_filters` write, so `START_MSG_FILTER` would go on to record a filter_id that no longer existed on hardware. **`dispatch_tx_item` takes an `is_backlog_drain: bool` and re-siphons to `tx_held`'s *front* (not back) only when called from `drain_tx_held_backlog`** (ADR-081 follow-up, Codex-review fix) — a concurrent `PDU_IOCTL_SUSPEND_TX_QUEUE` landing between `drain_tx_held_backlog` popping an item and `dispatch_tx_item`'s own suspend re-check could otherwise re-siphon that item to the back via the ordinary (fresh-dequeue) path, placing it behind items still in the backlog and inverting order on the next resume. **The `parked` due-item loop also calls `drain_tx_held_backlog` before dispatching a due cyclic continuation** (ADR-081 follow-up, Codex-review fix) — a due parked item is dispatched directly from this loop, bypassing `tx_rx.recv()` entirely, so without this a CLL's older `tx_held` backlog could sit un-drained while a later cyclic follow-up ran ahead of it. **Disconnect/Destroy's failed `stop_message_filter` calls are safe to only log, not retry or re-track** (ADR-082) — `client_filters` non-empty implies sole channel ownership (the install-time shared-channel rejection and both reciprocal join guards make any other state unreachable), so a filter-bearing CLL tearing down is always the last on its channel, and `PassThruDisconnect` always follows to tear down any hardware filter regardless of the explicit stop's outcome.
- Anything that touches the physical channel (connect, `ChannelKey`, message building, filter-family selection, ADR-028 SET_CONFIG gating, physical-lock comparisons) keys off `LogicalLinkState::hw_protocol_id`, never `protocol.j2534_protocol_id()` directly — the two differ in `software-isotp` mode (ADR-046). **This bullet's "keys off `hw_protocol_id`" guidance is the Plane A/Plane C half of a narrower rule as of ADR-156 Phase 2a/ADR-157 (see the "hw_protocol_id _PS Normalization" section below): a `_PS` link's `hw_protocol_id` is itself a `_PS` variant, so a *family/behavior decision* (ComParam support, filter-family selection, header format, resource occupancy — "Plane B") must normalize it first, via `LogicalLinkState::base_hw_protocol_id()`/`resources::base_protocol_id()`, while the hardware-facing uses this bullet already covers (the real `PassThruConnect`/message `ProtocolID` field, `ChannelKey`) correctly keep the raw value unchanged.**
- Any check that distinguishes `DualChannel` from `SingleChannel` behaviour (companion-channel open/close, the ADR-041 UUDT filter skip) must go through `J2534Service::effective_can_channel_mode()`, never compare `self.can_channel_mode` directly — `auto` mode's actual decision is only known after `probe_can_channel_mode` runs (ADR-047).
- Pure ISO 15765-2 frame encoding/decoding and reassembly state live in `service/isotp.rs` (unit-testable, no J2534 access); the TX driver / FlowControl waiting / RX fan-out integration lives in `events.rs` (`isotp_send`, `poll_rx_inner`, `RxEntryKind`). Addressing (`isotp::Addressing`, normal vs. extended) is resolved per UniqueRespIdTable entry, not once per CLL — see `FcPair` in `events.rs` and `SoftIsoTpTx`/`SoftIsoTpFraming` in `service.rs` (ADR-047).
- `ConnectComLogicalLink` never installs an ISO15765 pass-all filter — it first promotes `working_unique_resp_id_table` to `active_unique_resp_id_table` for THIS CLL (every connecting CLL, not just the physical channel's creator — ADR-068), then builds point-to-point `FLOW_CONTROL_FILTER`s directly from the just-promoted Active table (empty table ⇒ no filter of its own). Non-ISO15765 channels are unaffected and still get a pass-all `PASS_FILTER` at connect. **There is no pass-all fallback for ISO15765 anywhere in this service (ADR-122)** — the zero-mask fallback ADR-039 originally introduced and ADR-048 removed only from `ConnectComLogicalLink` is now removed from every remaining lifecycle point too (`CoptUpdateparam` promotion, `CLEAR_MSG_FILTERS`); an unaddressed CLL simply has no `FLOW_CONTROL_FILTER` of its own, ever.
- **UniqueRespIdTable gets a Working/Active split, mirroring ComParamSet (ADR-068).** `SetUniqueRespIdTable` (`rpc_misc.rs`) stages `working_unique_resp_id_table` only — it performs no hardware I/O itself, though it still checks `LOCK_PHYSICAL_COM_PARAMS` on ISO15765 at stage time (ADR-043, amended). Promotion (Working → Active, plus reconciling ISO15765 `FLOW_CONTROL_FILTER`s and the dual-channel-mode UUDT companion channel — no pass-all fallback to resync, ADR-122) happens via `J2534Service::promote_unique_resp_id_table` (`rpc_link.rs`), called only from `CoptUpdateparam` execution (`events::handle_update_param`, on hardware success) — `ConnectComLogicalLink` promotes inline instead (see above). The promotion helper diff-gates all filter I/O against the previous Active table (`unique_resp_id_tables_equal`, order-insensitive by `unique_resp_identifier`): a `CoptUpdateparam` whose table snapshot is unchanged (e.g. only a plain ComParam changed) does no filter churn. `GetUniqueRespIdTable` reads Working; `CoptRestoreParam` copies Active back into Working for the table too, with no filter I/O. `CoptSendrecv`/`CoptStartcomm` always resolve TX addressing, and RX routing (`events::build_cll_rx_entries`) always matches, against the Active table — snapshotted at `StartComPrimitive` call time exactly like `ParamBinding`, but *unconditionally* Active even under `temp_param_update=1` (there is no Working-side table for a temp COP to borrow, unlike `ParamBinding::Temp`'s `effective` ComParamSet — ADR-067 §G amended). A temp COP's Working-from-Active writeback never touches the table.
- `PassThruConnect`'s `Flags` argument is derived, not hardcoded to `0`: `rpc_link::connect_flags` computes it per protocol. `CAN` always gets `CAN_ID_BOTH` plus `CAN_29BIT_ID` as the priority bit from `phys_req_extended` (the physical-request address format) — every raw-CAN channel is a wide-open, pass-all-filtered receive channel shared by `(CAN, baud)` alone, so the CAN-ID widths it will ever see (a later raw-CAN CLL joining with a different width, or an ADR-046 UUDT companion reusing the channel in either creation order) are unknowable at connect time; an observation-based derivation left a real width-mismatch gap here, which `CAN_ID_BOTH` closes unconditionally. `ISO15765` (`can_connect_flags`) still derives `CAN_29BIT_ID`/`CAN_ID_BOTH` from Working `CP_Can*Format`/`CP_CanMixedFormat` and the connecting CLL's just-promoted `active_unique_resp_id_table` (ADR-068), since its point-to-point `FLOW_CONTROL_FILTER`s (ADR-048) are built from the exact CAN ID and are not exposed to that gap. `ISO9141`/`ISO14230` get `ISO9141_K_LINE_ONLY` from `CP_K_L_LineInit`; every other protocol stays `0`. Decided once by the channel-creator CLL — a later `SetUniqueRespIdTable`/`SetComParam` does not retroactively change it (J2534 has no way to re-issue `PassThruConnect` on a live channel). The ADR-046 UUDT companion channel always connects `CAN_ID_BOTH` too (same reasoning as the `CAN` case, since it *is* a raw-CAN channel), so reusing an existing `(CAN, baud)` channel there is width-safe regardless of which side created it first. `install_pass_all_filter` now takes the channel's connect flags and installs one `PASS_FILTER` per CAN-ID type in use, since a `TxFlags`-0 filter only matches 11-bit CAN Ids (ADR-065). The connect flags are persisted on `SharedChannel::connect_flags` so the `CLEAR_MSG_FILTERS` IoCtl's non-ISO15765 rebuild path (`rpc_misc.rs`) can call `install_pass_all_filter` with the same flags instead of a single TxFlags-0 filter — otherwise a `CAN_ID_BOTH` channel goes deaf to 29-bit frames after any client clears its filters (ADR-065).
- `CoptSendrecv`'s `cop_data` (and `CoptStartcomm`'s `CP_TesterPresentMsg`, when non-empty) is **payload-only** — the ID/header prefix (CAN ID, KWP format/target/source, J1850 format/target/source) is built by `tx_header::build_tx_message` from the ComParam set bound at `StartComPrimitive` call time (Active normally, or Working when `temp_param_update` is set, ADR-067) and the CLL's `active_unique_resp_id_table`, snapshotted unconditionally at the same call time regardless of `temp_param_update` (ADR-068; only its first entry is consulted; there is no per-request ECU selector), synchronously, before the primitive is queued (ADR-050). `CP_RequestAddrMode` selects physical (default, per-ECU via the Active table) or functional (broadcast, read directly off the bound set's `CP_CanFuncReq*`/`CP_FuncReqFormatPriorityType`/`CP_FuncReqTargetAddr` — never the table) addressing (ADR-054); `tx_header::CanAddressing::functional` records which one was resolved. The *constructed* message is then validated against the SAE J2534-1 per-protocol `PassThruMessage.Data` length range (`ChannelProtocol::tx_message_size_range`), rejecting out-of-range messages with `Status::invalid_argument`. **This check, and `build_tx_message` itself, resolve from `ChannelProtocol::from_raw(link.hw_protocol_id)` (`rpc_primitive::resolve_send_recv_tx`'s `hw_protocol`), not `link.protocol` directly** (PR-review fix on ADR-070) — `protocol.j2534_protocol_id()` is a fixed *initial candidate* for the two `SAE_J1850` bus-agnostic protocols regardless of which flavor the VPW/PWM auto-detect probe actually lands on, and is one shared value across all four `SAE_J2610_on_SAE_J2610_SCI` `hw_protocol_override` rows, so validating against it instead of the real connected hardware silently used the wrong (usually far more permissive) size range. The one deliberate exception is software-ISO-TP mode (ADR-046): `hw_protocol` there stays `link.protocol` (still ISO15765-family) since the buffer being sized is the *logical* pre-segmentation ISO15765 message, and `hw_protocol_id` is the raw CAN channel underneath, not a flavor this check should follow; `check_param_allowed`'s callers (`rpc_link.rs`) and `handle_update_param`'s (`events.rs`) `CoptUpdateparam` SET_CONFIG push apply the same `hw_protocol_id`-not-`protocol` correction for the same reason (`CP_NetworkLine`, PWM-only, is the concrete param this was caught on). On ISO15765, a *further* check rejects a functionally addressed `cop_data` longer than `isotp::Addressing::max_sf_payload()` — ISO 15765-2 requires functional requests to fit one Single Frame, since there is no way to negotiate FlowControl with an unspecified broadcast target (ADR-055). This is TX-only (RX is not validated) and applies to `CoptSendrecv`/tester-present only. `CoptStartcomm`'s K-line fast-init `init_data` is likewise **payload-only** (ADR-075, superseding ADR-050's raw-bytes scoping): `rpc_start_com_primitive` builds the KWP header through the same `build_tx_message` at call time from `binding.resolved()` and the call-time Active table snapshot — but only when `select_init_sequence` (a pure function of that same snapshot + `cop_data`, so its call-time answer always matches the poll task's execution-time one) selects fast-init — validates the constructed frame against the SAE J2534-1 TX size range like `CoptSendrecv` does, and carries it to the poll task as `TxItem::StartComm.fast_init` (`Some(FastInit::WithRequest(frame))`); the five-baud path (never rejected over the unsent fast-init frame) now splits in two (ADR-076): the pre-ADR-076 absent-`CP_InitializationSettings` legacy heuristic still consumes raw `init_data[0]` and still treats empty `init_data` as "skip the init sequence entirely," but the spec-mandated `CP_InitializationSettings == 1` path instead resolves its address from `CP_5BaudAddressFunc`/`CP_5BaudAddressPhys` (per `CP_RequestAddrMode`, both range-checked to a single byte), requires `init_data` to be empty (a non-empty payload is rejected synchronously), and *runs* the init on that empty payload rather than skipping it — its `NumReceiveCycles` (`1`/`0`/absent) instead gates whether the `[KB1, KB2]` key bytes get delivered. **ADR-077** adds a third, narrower exception to the empty-`init_data`-means-skip rule: an **explicit** `CP_InitializationSettings == 2` (not the absent-param legacy heuristic) with empty `cop_data` on a K-line link now runs a **wakeup-only** fast-init (`TxItem::StartComm.fast_init = Some(FastInit::WakeupOnly)`) — `PassThruIoctl(FAST_INIT)` with a NULL input message, no KWP header built, no request sent, and (per the D-PDU spec, which makes the fast-init service request optional) no response delivered at all, gated on the `FastInit` variant rather than on frame emptiness. `tx_header::resolve_can_addressing` (`params`+`entries.first()`-based) is shared by the size check, the software-ISO-TP addressing setup, and the tester-present addressing lookup. `CoptStartcomm`'s tester-present message/TxFlags/addressing (and its K-line init frame's TxFlags) are likewise resolved once, synchronously, at `StartComPrimitive` call time (ADR-067, reverting ADR-066's live-in-the-poll-task deferral) — the tester-present is always resolved from the call-time **Active** snapshot regardless of `temp_param_update` (it is a persistent side effect that outlives the transient init transaction, which alone may borrow Working for the duration of a `temp_param_update` call before hardware is unconditionally reverted to the live Active set read at revert time — see `rpc_primitive::resolve_tester_present`/`resolve_init_tx_flags`, called eagerly from `rpc_start_com_primitive`, and `events::handle_start_comm`, which now performs no ComParam resolution of its own). In software-ISO-TP mode the AE byte is never embedded in this buffer — `build_tx_message`'s `software_isotp` parameter must be `true` there, since the poll task's own frame builders add the AE per-frame (see `tx_header.rs::can_header_bytes`). The "Manual Checksum" ISO14230 variant is not implemented (ADR-049).
- `PASSTHRU_MSG.TxFlags` is not simply the client's requested value forwarded verbatim (ADR-062). `rpc_primitive::compute_j2534_tx_flags` maps only `TX_FLAG_WAIT_P3_MIN_ONLY`/`TX_FLAG_ISO15765_FRAME_PAD` from the client's named `TxFlagBits`/raw bytes — the raw bytes are the ISO 22900-2 D.2.1 (Table D.4) `TxFlag` byte-array layout (byte 0 first), decoded bit-by-bit into these two J2534 positions, NOT an already-native J2534 `TxFlags` u32 (ADR-116); `apply_resolved_tx_flags` then overwrites `TX_FLAG_CAN_29BIT_ID`/`TX_FLAG_ISO15765_ADDR_TYPE` with `tx_header::can_addressing_tx_flags(can_addressing)` (the same `CP_CanPhysReqFormat`/`CP_CanFuncReqFormat` resolution used to build the message body) and ORs in `ComParamSet::sci_tx_flags()` (`TX_FLAG_SCI_MODE`/`TX_FLAG_SCI_TX_VOLTAGE`, from `CP_SCITransmitMode`/`CP_SCISetProgVoltage`). Apply this in any new TX call site that constructs `TxFlags` from `ComPrimitiveCtrlData.tx_flag` — do not reintroduce a bare `compute_j2534_tx_flags(...)` result without also running it through `apply_resolved_tx_flags`, and remember it must reach *every* message on a CLL. `rpc_start_com_primitive`'s `CoptStartcomm` branch calls `resolve_tester_present`/`resolve_init_tx_flags` (ADR-067) separately, since they can read *different* ComParam sets during a `temp_param_update` call (tester-present always the call-time Active snapshot; the init frame the bound `ParamBinding::resolved()`, Working-or-Active) — both still run their raw `base_tx_flags` through `apply_resolved_tx_flags`, never a hardcoded `TX_NORMAL_TRANSMIT`.
- **ComParams bind to the ComPrimitive at `StartComPrimitive` call time, not live at poll-task execution time (ADR-067, superseding ADR-063/064/066).** `ParamBinding` (`service.rs`) is `Plain(active_snapshot)` or, for `temp_param_update=1`, `Temp { effective }` — cloned inside `rpc_start_com_primitive` at call time, never read live by the poll task. `resolve_send_recv_tx`/`resolve_tester_present`/`resolve_init_tx_flags` are called eagerly from `rpc_start_com_primitive` against `binding.resolved()`; a resolution failure is a synchronous `INVALID_ARGUMENT` again, not an execution-time error event. `events.rs`'s `handle_send_recv`/`handle_start_comm` perform no ComParam resolution — they only apply/revert hardware for a `Temp` binding. The hardware *revert* target is the live Active set read at revert time (`events::revert_hardware_to_live_active`) — a restoration duty, not a COP-bound param, so a `CoptUpdateparam` queued ahead of (or interleaved with) the temp COP is not undone by the revert. `temp_param_update=1` additionally (a) writes Working back from Active immediately after a successful call (`rpc_start_com_primitive`, right before returning — even for `CoptStopcomm`, which writes no hardware config), and (b) is rejected synchronously with `PDU_ERR_TEMPPARAM_NOT_ALLOWED` if Working differs from Active on any `PDU_PC_BUSTYPE`-class ComParam (`comparam_support::bustype_params_differ`, checked before any side effect — this is the only physical-ComParam guard `temp_param_update` is subject to; the `LOCK_PHYSICAL_COM_PARAMS` check this once also ran alongside is removed, since `temp_param_update` can never stage a `PDU_PC_BUSTYPE`-class ComParam in the first place, lock or no lock — ADR-110, superseding ADR-044 here). `CoptUpdateparam` snapshots Working into `TxItem::UpdateParam.params` at its own call time; `handle_update_param` applies that snapshot and, on hardware success, promotes it to Active — a `SetComParam` issued after the `CoptUpdateparam` call is not promoted by it. The one live-read exception, unchanged: `CP_P3Func`/`CP_P3Phys`'s inter-request gap (`wait_for_p3_gap`) always reads the live Active set, since it protects the shared bus across every CLL, not just this COP. ADR-083 adds a second, narrower live-read touchpoint: the mode-0 periodic tester-present *start* (`CP_TesterPresentSendType=0`, `events::handle_start_comm`) is now gated through the same `wait_for_p3_gap` mechanism before `PassThruStartPeriodicMsg` runs — but only that initial start, not individual periodic ticks thereafter, since the DLL gives the service no per-tick visibility once the periodic message is running; this is a documented, accepted J2534-API limitation, not a gap. **ADR-084 adds a third, distinct kind of exception — an actual poll-task re-resolution, not just a live read.** `handle_update_param` (`CoptUpdateparam`) calls `rpc_primitive::resolve_tester_present` a second time, live, in the poll task, against the ComParam set it has just promoted to Active, to decide whether an already `comm_started` CLL's mode-1 (idle-triggered) tester-present should immediately send and re-arm. This is deliberately gated to only fire when the re-resolved output actually differs from what is currently armed (`ResolvedTesterPresent::same_wire_behavior`) — an earlier draft resolved and re-armed unconditionally on every successful promotion, which re-sent a frame and reset the idle clock for a `CoptUpdateparam` promoting a ComParam with no bearing on tester-present at all (caught in review; pinned by `send_type_1_unrelated_updateparam_does_not_resend_or_rearm`/`send_type_1_relevant_updateparam_resends_and_rearms` in `tests/grpc_mock/tester_present_send_type.rs`). This does not reopen the "ComParams bind at call time" invariant for `TxItem::SendRecv`/`StartComm` — it is scoped entirely to `TxItem::UpdateParam`'s own execution, deciding a live re-arm, not resolving a COP's own TX content.
- **ADR-088 (amended per a requester correction after its first merge, then amended three more times per Codex-review findings on the correction's own PR, then amended a SECOND time — see the ADR's own Second Amendment section — closing a live-vs-snapshot mismatch the first Amendment's `discard_until` field still had; read the ADR's Amendment sections in full, not just its original Decision): `CP_TesterPresentReqRsp = 1` discards the ECU's tester-present response, scoped to the actual request tester-present itself sent — not a bare content filter over all RX traffic.** `build_cll_rx_entries` (`events.rs`) computes `CllRxEntry.tester_present_discard: Option<TesterPresentDiscard>` (`{ pos, neg, target_can_ids }`) entirely from that CLL's own `TesterPresentState::Armed::discard_until` (`DiscardWindow`) snapshot — **not** from live Active state (Second Amendment): `Some(window)` with `window.until` still in the future is fully sufficient, on its own, to know the send that opened it expected a response, and `window.pos`/`window.neg` (not a live `l.active.tester_present_exp_pos_resp()`/`_neg()` read) are what the prefix match uses. ~~Mode 0 (`TesterPresentState::Periodic`) stays armed-wide (`PassThruStartPeriodicMsg` gives no per-tick send visibility, ADR-083);~~ **superseded by ADR-093: both modes are now unified into `TesterPresentState::Armed`, dispatched by this service's own software poll loop, so both get the identical treatment described next.** Discard is scoped to `discard_until: Option<DiscardWindow>` (`DiscardWindow { until, pos, neg }`, widened from a bare `Option<tokio::time::Instant>` by the Second Amendment), an `Armed` field set to `Some(DiscardWindow { until: fired_at + CP_P2Max, pos, neg })` only when a send both **succeeded** AND was made while `expects_response` (`CP_TesterPresentReqRsp == 1` at that same send instant) was `true` — `None` otherwise, including a failed send OR a send made under `ReqRsp = 0` — at all three call sites that construct/update `Armed` (the unified `handle_start_comm` arm branch, `handle_update_param`'s re-arm, `dispatch_due_tester_present`'s per-tick send), each also capturing `pos`/`neg` from that same pre-send snapshot (via two new `ResolvedTesterPresent` fields, `exp_pos_resp`/`exp_neg_resp`, resolved from the same Active `CP_TesterPresentExpPosResp`/`ExpNegResp` every other field on that struct comes from, and excluded from `same_wire_behavior` like `expects_response`/`target_can_ids`/`p2_max_ms`) — a frame arriving outside that window, or one matching only the CURRENT live Exp* pattern rather than the window's own frozen one, was never actually elicited by this CLL's tester-present as it stood at send time. `target_can_ids: Option<TesterPresentTargetCanIds>` (`{ usdt, uudt }`, `service.rs`) additionally restricts discard, in both modes, to the CLL's own tester-present target's physical CAN ID(s) when addressing resolves to CAN-family **physical** (mirroring `resolve_tester_present`'s own `can_functional` derivation) — `None` (no restriction) for functional/broadcast addressing, where every routed ECU is a legitimate reply. Matched directly against `frame_can_id` (the raw CAN ID already extracted per received message), never through a live `unique_resp_identifier` (uid) lookup. **Both modes now read `target_can_ids` off a value frozen at whichever arm/re-arm actually resolved it — NEITHER mode recomputes it live from the current table, and getting there took three review rounds before this ever landed committed:** at the time these rounds landed, `TesterPresentState::Periodic { id, target_can_ids }` (no longer a bare `PeriodicMessageId` tuple) froze it at mode-0's one arm site while `TesterPresentState::Idle` read it straight off its own pre-existing `resolved: ResolvedTesterPresent` field — both populated from a new `ResolvedTesterPresent::target_can_ids` field computed once in `resolve_tester_present`. ADR-093 later collapsed `Periodic`/`Idle` into one `Armed { resolved, .. }` variant; both modes now read `resolved.target_can_ids` through the exact same code path, which is what made deleting the mode-0-specific freezing site safe (mode 0 can re-arm now too, per ADR-093, so a live table change reaches it the same way it already reached mode 1). Round 1 (edge-case-hunter) caught that a live recompute for mode 0 would drift the moment a table reorder changed which entry is "first" — fixed by freezing, initially as just the target's `unique_resp_identifier` label. Round 2 (Codex review of that fix) caught that freezing the *label* alone is insufficient: `route_frame` always resolves a frame's uid from whichever CAN-ID-to-uid pairing is live *right now*, so a later `SetUniqueRespIdTable` reassigning the SAME uid to a *different* CAN ID (not merely reordering, which a plain reorder test does not actually exercise — see `reqrsp_1_mode_0_resp_uid_survives_table_reorder`'s own doc comment) would silently reintroduce the exact bug round 1 fixed, via relabeling instead of reordering. Fixed by freezing the raw CAN ID(s) instead of the uid label, sidestepping the live uid lookup entirely (`reqrsp_1_mode_0_target_can_id_survives_uid_reassignment` pins this specific scenario). Round 3 (Codex review of round 2's fix) caught that mode 1 had the SAME divergence via a different path: its `target_can_ids` was (wrongly) assumed safe to recompute live because mode 1's ADR-084 re-arm gate would catch any relevant addressing change — but `same_wire_behavior` compares `data` (built from `CP_CanPhysReqId`, the request/TX addressing), not `target_can_ids`, so a `CoptUpdateparam` that swaps only the response addressing (`CP_CanRespUSDTId`) between two entries never re-arms at all, and the per-tick send itself never re-resolves `resolved` between arms either — a live recompute would silently move the discard target to the new table mid-`CP_P2Max`-window. Fixed by reading `resolved.target_can_ids` directly instead — a net simplification, not new state (`reqrsp_1_mode_1_target_can_id_survives_response_id_only_updateparam` pins this scenario). Round 4 (Codex review of round 3's fix) caught that `handle_start_comm`'s mode-1 arm sized `discard_until` from `binding.resolved().p2_max_timeout_ms()` — under `temp_param_update = 1`, `binding` is the transient, already-reverted Working snapshot, while tester-present itself always resolves from call-time Active regardless (ADR-067 claim 8), so the very first discard window was sized from a value never actually in effect for the send. Fixed by adding `ResolvedTesterPresent::p2_max_ms` (resolved from the same `active` as everything else on that struct) and reading `tester_present.p2_max_ms` at the arm site instead of `binding` (`reqrsp_1_mode_1_p2max_from_active_not_temp_working_binding` pins this); `handle_update_param`'s re-arm site and `dispatch_due_idle_tester_present`'s per-tick sends were already reading `CP_P2Max` from the correct source and needed no change. `poll_rx_inner`'s per-delivery loop, right after computing `(acceptance_id, frame_cop_handle)`, drops a delivery when `frame_cop_handle.is_none()` (a pending COP's own claim always wins, unconditionally first) and the payload `starts_with()` either configured non-empty prefix and `target_can_ids.is_none_or(|ids| frame_can_id.is_some_and(|id| Some(id) == ids.usdt || Some(id) == ids.uudt))`. `CP_TesterPresentExpPosResp`/`CP_TesterPresentExpNegResp` had no consuming logic anywhere before this ADR. Known residuals (not fixed, see ADR-088's Consequences and Amendments): a tester-present-shaped frame containing a pending-response NRC can still be mis-claimed by the pre-existing pending-RC branch; a client's own `0x3E` CoptSendrecv whose `expected_response` pattern under-matches the ECU's actual reply, landing inside either mode's `discard_until` window (both modes are windowed as of ADR-093), can have that reply discarded instead of delivered unsolicited; a one-poll-tick window around `CoptStopcomm` disarming where an in-flight response is delivered rather than discarded; `CP_P2Max`'s default (50 ms) may under-cover a slow ECU's mode-1 reply; the `target_can_ids` restriction always takes the *first* UniqueRespIdTable entry under physical addressing, matching `resolve_tester_present`'s own targeting rule; and (Codex review finding, requester-confirmed accepted trade-off) the six legacy-OBD presets' bare `CP_TesterPresentExpNegResp = [0x7F]` is not service-scoped even inside the window — an unclaimed NRC for a genuinely different diagnostic service, from the same ECU, landing inside the same `CP_P2Max` window as one of tester-present's own idle-mode sends, is still discarded, since `[0x7F]` alone can't distinguish which service's negative response it is. **Extended by ADR-099** (a user question about whether SOM/TX_DONE J2534 indication frames were leaking past this discard, confirmed real): the above described only *content* discard (payload prefix match); indication-type `rx_status_flags` (SOM, TX_DONE/TX_INDICATION, CONFIG_LOOPBACK/TX_MSG_TYPE — near-empty payload, so they could never satisfy a non-empty prefix match) sailed straight through undiscarded. `poll_rx_inner`'s check is now three arms, not one: (a) content, unchanged from above; (b) a `START_OF_MESSAGE` herald of that same response, gated by the same `target_can_ids` restriction and eligible only when a non-empty `pos`/`neg` was actually frozen for the window (proof the send expected a response); (c) a `TX_DONE`/`TX_INDICATION`/`TX_MSG_TYPE` echo of tester-present's own send, gated by a new `TesterPresentDiscard.tx_can_id: Option<u32>` (tester-present's own frozen outgoing TX CAN ID, taken from `framed_data`'s leading 4 bytes) rather than `target_can_ids` (which is response-side only). `RX_BREAK` is excluded from all three unconditionally, even combined with another indication bit. Because case (c) is independent of `CP_TesterPresentReqRsp`, `discard_until`/`DiscardWindow` now opens on **every** successful tester-present send, not only ones where `expects_response` was true — `pos`/`neg` freeze empty (never content-matches) instead of the window itself staying `None` when a send's `expects_response` was false, so `Some(discard_until)` now proves only "the send succeeded," and non-empty `pos`/`neg` is what additionally proves "and expected a response." See ADR-099 for the full mechanism and its accepted residuals, including a `route_frame` routing-layer limitation on case (c)'s practical reachability (tracked separately in the Prioritized Backlog).
- Symmetric with the above on RX: `poll_rx_inner` splits a leading header and, where applicable, a trailing footer off of `ResultData.data_bytes` into `ResultData.extra_info.header_bytes`/`footer_bytes` (`ReceivedFrame::header_bytes`/`footer_bytes`), via `events::header_footer_len`, for every protocol this service supports: CAN (4-byte CAN ID, no footer), ISO15765 (4-byte CAN ID, or 5 bytes with the Address Extension when the matched `UniqueRespIdTable` entry uses extended addressing — `CllRxEntry::usdt_addressing_by_id`/`uudt_addressing_by_id` (split by role, ADR-217 Codex-review fix: a single flat table let one role's `Addressing` bleed into the other role's delivery under a colliding `CAN_MIXED_FORMAT_ALL_FRAMES`/`dual-channel`/software-ISO-TP configuration; `CllRxEntry::rx_addressing_table` selects the right one per delivery from its own `uudt_routed` flag), keyed by `CllRxEntry::header_protocol`, per-`RxEntryKind`-derived (ADR-171 follow-up: `l.protocol.j2534_protocol_id()` for a `SoftwareIsoTp` entry, `resources::base_protocol_id(l.hw_protocol_id)` for `Hardware`/`Companion`, since `j2534_protocol_id()` alone never reflects the J1850 VPW/PWM auto-detect result — no footer), ISO9141/ISO14230 (`events::kwp_header_and_payload_len` parses the response's own format byte for a 1-4 byte header *and* its declared payload length — this service always *sends* the fixed 4-byte variant, ADR-050, but does not assume a response uses the same encoding or carries a checksum at all; the footer is simply whatever bytes remain after the header's own declared payload length, so it is correct whether or not a checksum is present), J1850 (a fixed 3-byte header and, since J1850 has no length field to make the footer self-describing the way KWP's is, a fixed 1-byte footer whenever a trailing byte remains), and SCI (no header/footer concept — always a no-op, matching `tx_header::build_tx_message`'s TX-side stance; `data_bytes` stays the raw frame, `extra_info` stays `None`) (ADR-051). `ExpectedResponseData.mask_data`/`pattern_data`/`CP_RCByteOffset` matching runs against the resulting payload-only slice. The ISO15765 AE-byte widening only applies to a raw (non-reassembled) delivery — `process_frame_for_entry`'s `raw: bool` return flag distinguishes this from an already-reassembled software-ISO-TP payload, which never has an AE byte left to split. Both the `SubscribeEvent` push path (`poll_rx_inner`) and the `GetEventItem` pull path (`rpc_get_event_item`) read the split fields off the same `ReceivedFrame`, so they must be kept in sync if either changes. The synthetic fast-init response frame in `handle_start_comm` gets the same `header_footer_len` header/footer split before delivery (ADR-075); five-baud keybytes are not a KWP frame and stay raw with `extra_info` `None`. `ResultData.rx_flag` (ADR-098, extending ADR-097 which superseded ADR-061) is `events::rx_flag_bytes(rx_status_flags, extras)` at both sites — a 4-byte ISO 22900-2 `RxFlag` buffer with byte 3 bits 0-4 populated unconditionally (`[0, 0, 0, RxStatus & 0x1F]`) from the source J2534 `RxStatus`'s 5 low bits (`TX_MSG_TYPE`, `START_OF_MESSAGE`, `RX_BREAK`, `TX_INDICATION`, `ISO15765_PADDING_ERROR`), `Vec::new()` (no flags asserted) when all 5 are clear AND `extras` is its `Default` (a Normal Message); `ReceivedFrame::rx_status_flags: u8` carries the masked byte from `poll_rx_inner`'s per-message `msg.rx_status()` read to both delivery sites (the synthetic fast-init response frame is hardcoded `0`/`RxFlagExtras::default()`/`Vec::new()`, since it has no real `RxStatus`). `extras` (`events::RxFlagExtras`) bundles the two `RxFlag` byte 1 bits this service computes/forwards itself rather than folding into this 5-low-bits mechanism: `ecu_timing_change` (byte 1 bit 1, ADR-146 — set only on a qualifying `CP_ModifyTiming` Access Timing response) and `sw_can_hv_rx` (byte 1 bit 0, ADR-191 — a direct ISO 22900-2 Table D.5-standardized forward of SAE J2534-2's `SW_CAN_HV_RX` RxStatus bit for a genuine SW-CAN-family reception). Every other `RxFlag` bit ISO 22900-2 defines remains unreported/zero — future scope. A SOM/TxDone/RxBreak frame's post-header-split payload is often empty (ADR-051 split leaves nothing behind for a header-only or wholly-empty frame), which would otherwise vacuously satisfy `ExpectedResponse::matches`/pending-RC detection for a broad/empty descriptor, and a Loopback echo of our own request can separately trip `RcHandlingConfig::detect_pending_rc` when the request's own byte at `CP_RCByteOffset` happens to equal a pending-NRC value (e.g. SID 0x23 looks like NRC 0x23); `poll_rx_inner`'s `MatchProbe` guard therefore requires `rx_status_flags & !(RX_ISO15765_PADDING_ERROR as u8) == 0`, so a Normal Message's `frame_cop_handle` can always be attributed, and so can a message tagged ONLY `ISO15765_PADDING_ERROR` (ADR-098's Correction: unlike the other four bits, that one tags a fully received, non-empty response — a short final CAN frame, not a header-only or empty indication — so excluding it broke matching for legitimate short responses and, systemically, pending-RC extension for any ECU whose un-padded final frame is under 8 bytes). Every other frame kind (SOM, TxDone, RxBreak, Loopback, or any combination of those with padding-error) is still delivered only as an unsolicited indication, never attributed to a pending `CoptSendrecv`/pending-RC wait. The software-ISO-TP TX driver's `FcCapture` additionally excludes any frame with `RX_TX_MSG_TYPE` set, so a CONFIG_LOOPBACK echo of this service's own transmitted FlowControl frame is never mistaken for the ECU's.
- `RcHandlingConfig::from_params`'s RC78 (response-pending) handling now splits across two ComParams (ADR-102, amending ADR-056): `rc78_p2_star_ms` reads `CP_P2Star`, the D-PDU-standard "extended P2" ComParam, and reloads the response deadline to `now + rc78_p2_star_ms` on *every* 0x78 occurrence (ISO 14229-2 §7.3 P2*client semantics); `rc78_total_ceiling_ms: Option<u32>` reads the service-specific `CP_RC78CompletionTimeout` ADR-018 originally introduced — reinstated, no longer inert — as an independent total-duration ceiling, anchored once at the COP's first 0x78, that every reloaded deadline is clamped to when set (`0`/absent disables it). `protocol_default_params` no longer seeds `CP_P2Star` from `CP_RC78CompletionTimeout` (ADR-102 removes that ADR-056 sync); each preset's `CP_RC78CompletionTimeout` value now serves directly as its ceiling, and `CP_P2Star` falls back to `RcHandlingConfig::from_params`'s own 5000 ms default unless a preset sets it explicitly.
- **`CP_SuspendQueueOnError` (ADR-147) is now implemented as a third, content-triggered `LogicalLinkState::tx_suspended()` source**, `tx_suspended_by_error`, alongside the two call-triggered sources ADR-123 built (`tx_suspended_by_ioctl`/`tx_suspended_by_lock`). It has no cross-CLL inputs and is never touched by `recompute_lock_tx_suspensions`'s sweep; it is set by a COP timeout or an unhandled `0x7F` negative response (`RcHandlingConfig::is_unhandled_negative`, a new sibling to `detect_pending_rc` that classifies an NRC the RC engine was never asked to auto-handle -- disabled `CP_RCxxHandling`, or an NRC outside the RC21/RC23/RC78 set entirely), classified at bind time and written exactly once per frame, from that frame's final binding outcome (a bound registrant match's own fresh `rc_cfg` check, a bound tester-present reply's unconditional no-write, or an otherwise-unbound frame's fallthrough -- never as a loose side effect of the registrant scan itself, a correction to this feature's first cut; see `bind_frame`'s doc comment in `events.rs`), then folded per poll-pass across multiple frames with last-frame-wins semantics and applied in the same end-of-pass critical section as `merge_registrant_writeback` (a plain link-field write, not a new registrant-writeback participant). Cleared by `PDU_IOCTL_RESUME_TX_QUEUE`, a `CoptUpdateparam` promotion landing Active `= 0`, a later positive response (unconditional), or a hard channel error -- `PDU_IOCTL_CLEAR_TX_QUEUE` cancels held items but does not itself clear any suspend source, matching its own pre-existing contract. `CP_RepeatReqCountApp` remains an inert stub, not covered by this change. **Follow-up fix (ADR-147 addendum, Codex review on the original PR + design-advisor): the documented recovery `CoptUpdateparam` deadlocked once any transmitting item was already parked in the held backlog** -- the shared FIFO no-overtake clause caught the recovery item too, and the drain loop's front-pop-only gate could never reach it behind a still-blocked transmitting item, making the held item a precondition of its own release. Fixed narrowly: `dispatch_tx_item`'s siphon gate exempts `TxItem::UpdateParam` *specifically* (never the whole non-transmitting class -- that would reintroduce ADR-123 Fix B's comm-state-inversion hazard) from the FIFO clause while `tx_suspended_by_error` is set, regardless of whether `tx_suspended_by_lock` is also set concurrently; `drain_tx_held_backlog` gained a matching fallback that searches the backlog for an `UpdateParam` from anywhere, not just the front, when the front-pop condition doesn't hold; and `recompute_lock_tx_suspensions`'s wake-target set was widened to fire whenever `tx_suspended_by_lock` itself transitions true → false, even if the CLL remains effectively suspended (`tx_suspended_by_error` persists) -- closing a second stranding path where a sibling's lock release would otherwise send no wake. See the ADR's own addendum for the full argument. The naive "lock held while a COP on that same locked CLL times out" precondition is genuinely unreachable (`LockResource`'s busy check rejects lock acquisition for a resource with any live transmitting COP), but a mirrored path IS real: a not-yet-connected SAE J1850 CLL can acquire the lock before its flavor resolves, and `autodetect_sae_j1850_flavor`'s later `recompute_lock_tx_suspensions` call folds in another CLL's already-live transmission without re-running the busy check -- documented as its own accepted residual (lock+error coexistence is reachable, but each channel's single serial poll task sequences both error setters before any later dispatch, so this path still cannot strand the recovery `CoptUpdateparam`; the wake-widening's real justification is two narrower, unforceable dual-channel (UUDT companion) race windows instead). No deterministic regression test exists for the middle-pop fallback or the wake-widening in this harness -- structural-argument-only, per the ADR's Consequences section. **Follow-up fix (Codex review, wrong-SID classification): a bound `0x7F`-led frame that cannot be confirmed as this registrant's own unhandled negative response -- a different request's SID, a truncated frame with no RC-offset byte, or an `rc_byte_offset < 2` protocol -- was being misclassified `Positive` instead of declining to classify**, wrongly clearing `tx_suspended_by_error` and releasing the held backlog on what was actually an unattributable negative response. Fixed by unifying the classification rule already used for the tier-2 (no `rc_cfg`) case: any bound `0x7F`-led frame not confirmed `Suspend` now declines to classify regardless of `rc_cfg` presence; only a genuinely non-`0x7F`-led frame classifies `Positive`. **Follow-up fix (Codex review, recovery-bypass narrowing): the `TxItem::UpdateParam` FIFO-bypass exemption above originally fired for ANY queued `UpdateParam` while error-suspended, not just one that actually clears the suspension** -- an unrelated `CoptUpdateparam` (its own queued `params` snapshot leaves `CP_SuspendQueueOnError` enabled) got the same bypass despite recovering nothing, letting it overtake older held work for no benefit. Both the dispatch-side bypass and the drain-side middle-pop fallback now additionally require the queued item's own `params.suspend_queue_on_error()` to read disabled -- exact by construction, since promotion replaces `Active` wholesale and the same live check that clears the suspension reads that replaced value. **Follow-up fix (Codex review, stale-classification race): a batch's frames are exposed to the client (event/result stream) mid-pass, before that same pass's end-of-pass classification apply runs, so an explicit client action (`PDU_IOCTL_RESUME_TX_QUEUE`, etc.) landing in that gap could be silently undone by the now-stale `Suspend` classification the client's own reaction was based on** -- the apply step's only staleness guard was `connect_generation`, which a resume with no reconnect never trips. Fixed with a new `LogicalLinkState.error_suspend_epoch: u64`, bumped -- via a small shared helper, `LogicalLinkState::clear_error_suspension` -- in the same critical section as each of the four explicit clear sites, captured into the per-pass snapshot alongside `connect_generation`, and checked at apply time -- any mismatch discards the classification entirely (`Suspend` or `Positive` alike). This also closes a second, non-reactive shape of the same race (a UUDT-companion pass's `Suspend` landing after the main task's `CoptUpdateparam` promotion already cleared the flag) that no delivery-ordering fix could have caught, since the triggering exposure there is an earlier COP, not the racing frame. The apply decision itself (generation/epoch match vs. classification) is factored into a pure helper with direct unit-test coverage of the full truth table, since the race itself is unforceable deterministically in this harness (same class as ADR-123's Findings A/E/G). **Follow-up fix (post-merge Codex review round, design-advisor mechanism, fail-open correction): the epoch bump above was originally unconditional, even when `tx_suspended_by_error` was already `false` -- a client resuming an already-resolved episode still bumped the epoch, and a genuinely fresh, unrelated `Suspend` classification landing in the same pass got wrongly discarded, failing OPEN instead of staying suspended on a real unhandled error.** Fixed by narrowing `clear_error_suspension` to bump only when the flag was actually `true` beforehand (there must be an active suspension to invalidate), returning that boolean for the caller's own use -- no current call site needs it beyond the narrowing check itself, since it reflects only the `tx_suspended_by_error` source, not the OR with `tx_suspended_by_ioctl`/`tx_suspended_by_lock` a wake-gating decision actually needs (`ioctl_resume_tx_queue` discards the return value entirely; `handle_update_param` discards it too, computing its own before/after `tx_suspended()` comparison instead). Narrowing the teardown-site bumps this way reopens a boundary leak the old unconditional bump incidentally closed -- a stale offline-window classification could otherwise leak `tx_suspended_by_error = true` into a freshly reconnected session, since `connect_generation` is unchanged until reconnect and the apply-time epoch check alone doesn't cover the pre-reconnect offline window -- so `finalize_connected_link` (`rpc_link.rs`) also resets `tx_suspended_by_error = false` at connect time (every finalized connect, including a same-`cll_handle` reconnect), in the same critical section that stamps the fresh `connect_generation`. **Follow-up fix (Codex review round; design-advisor's FINAL ruling on this mechanism, after the two prior attempts above each closed one race and reopened another): capture-at-fold sequencing replaces the pass-level epoch entirely.** The pass-level epoch's own capture point (a per-pass snapshot taken up front, alongside `connect_generation`) never aligned with the actual moment of risk (a specific frame's exposure to the client) -- so neither an unconditional bump (round 1, fail-open) nor a conditionally-narrowed one (round 2, reopened the original race for the FIRST suspend-worthy frame of an episode, classified while `tx_suspended_by_error` was still `false`) could get both directions right at once. The fix moves the capture side, not the bump side: `LogicalLinkState.error_suspend_epoch` is renamed `error_action_seq` and its bump is unconditional again (matching round 1's shape), but `CllRxEntry` now captures a NEW field, `suspend_seq: Option<u64>`, at the EXACT moment `bind_frame` writes/rewrites a frame's classification to `Suspend` -- briefly re-acquiring `logical_links`, with no nesting against `rx_buf` -- strictly BEFORE that frame is exposed to the client, rather than at pass-snapshot time; a later frame in the same pass that does not rewrite the classification to `Suspend` leaves an already-captured `suspend_seq` untouched. The apply gate is now asymmetric: a `Suspend` classification applies iff `link.error_action_seq == entry.suspend_seq`, in addition to the existing `connect_generation` check AND the existing live-policy re-check (`link.active.suspend_queue_on_error()`) -- three conditions, not two, the live-policy check being the same veto this ADR's original Decision already applied before any of the epoch/seq work existed; `Positive` needs no seq check at all, only `connect_generation`, since a stale `Positive` discard was already a no-op by construction. A new unit test, `wrote_suspend_flag_set_only_on_the_frame_that_folds_suspend` (`events.rs`, `bind_frame_tests`), pins the fold-capture timing itself via `bind_frame`'s new `wrote_suspend: &mut bool` out-param -- set `true` at exactly the write site that writes `Suspend`, never on a `Positive` write or a frame that doesn't rewrite the classification. Accepted residual, durably recorded in ADR-147's Consequences and NARROWED by this correction: the only remaining ambiguity is an explicit action landing AFTER a `Suspend`-worthy frame was exposed but NOT actually in reaction to it -- indistinguishable from a genuine reaction, since exposure-ordering is the finest available signal without reading client intent; the loss is usually not permanent (a receive timeout's own set-before-expose hook re-suspends independently of this mechanism, and permanent loss additionally requires the unhandled negative to match the COP's own permissive expected descriptor and complete the phase as a successful match). **Follow-up fix (Codex review round; design-advisor re-derived the mechanism from first principles and confirmed a FOURTH real bug on it -- this closes the mechanism's design): the prior round's asymmetric apply gate reasoned "a stale `Positive` discard is a no-op by construction," but that reasoning covered only content-vs-content races, never a `Positive` racing the TIMEOUT HOOK specifically, which sets `tx_suspended_by_error = true` through a code path with no seq association at all.** Concrete bug: on a dual-channel CLL, a companion-channel poll pass folds a `Positive` classification (capturing nothing, since `Positive` wasn't sequenced), then pauses before its own end-of-pass writeback; the PRIMARY channel's receive-phase timeout fires independently, setting `tx_suspended_by_error = true`; the companion pass's delayed writeback then applies its `Positive` classification unconditionally, clearing the flag the timeout just set and sending a wake -- letting queued transmissions run despite an unhandled timeout that should still be suspending them. Fixed with one unified counter, symmetric gating for both classifications: `LogicalLinkState.error_action_seq` is renamed `error_state_seq` (the semantics genuinely change from "clear actions only" to "every synchronous state change to `tx_suspended_by_error`"); the timeout hook's own set of the flag now ALSO bumps this counter, in the same critical section, before the timeout event emission (a bump on SET, distinct from the four existing bump sites which are all on clear, but sharing the same counter); `CllRxEntry.suspend_seq` is renamed `class_seq` and generalized to capture on EITHER a `Suspend` OR a `Positive` fold (`bind_frame`'s `wrote_suspend` out-param is renamed `wrote_class` and generalized identically); and `queue_error_class_to_apply` now requires `link.error_state_seq == entry.class_seq` for BOTH classifications, symmetrically, with `Suspend`'s live-policy re-check folded into the same function as the one remaining asymmetric dimension. Governing rule: this counter sequences SYNCHRONOUS state changes -- whose evidence time equals their apply time, so they are never themselves stale -- against DEFERRED content classifications, whose evidence time is their fold time; deferred-vs-deferred (two passes' classifications racing each other) remains last-writer-wins per the existing cross-channel residual, which is why classification APPLIES do not themselves bump the counter -- making them bump would re-key cross-channel outcomes to apply order rather than evidence order, the exact defect the residual already documents, just manifesting as discard instead of overwrite. `queue_error_class_to_apply_tests` (`events.rs`) was rewritten for symmetric gating across the full truth table, including new cells modeling the bug scenario's seq arithmetic directly (`positive_discarded_on_seq_mismatch_after_a_timeout_bump`) and the genuine-recovery direction (`positive_applies_when_captured_after_a_timeout_bump`); `bind_frame_tests`' fold-capture-timing test is renamed and extended to cover both classification variants. No new end-to-end integration test was added -- the companion-vs-timeout interleave is structurally unforceable in this harness, same class as the prior round's residual; see `reconnect_does_not_inherit_prior_sessions_error_suspension`'s doc comment (`tests/grpc_mock/queue_error_suspend.rs`) for the specific note. **Follow-up fix (Codex review round; design-advisor's full re-derivation -- the FIFTH amendment to this mechanism, and the one that finally closes it correctly: `Suspend` and `Positive` need DIFFERENT anchoring semantics, and the prior round's unified counter was itself the root cause of this round's Codex finding, not merely under-fixed by it.** Codex found `bind_frame` (synchronous, no lock held) folding a classification, with the seq capture happening in a SEPARATE, LATER `logical_links.lock().await` in `poll_rx_inner` -- an explicit clear or the timeout hook could bump `error_state_seq` in the gap between the two. Design-advisor's re-derivation: this only matters for HALF of that finding. The `Suspend`-vs-explicit-clear direction is NOT a bug -- `Suspend`'s correct anchor has always been EXPOSURE, and the capture site always runs before this frame's delivery, so a clear absorbed in the fold-to-capture gap necessarily predates the frame's exposure and cannot be a client reaction to it; absorbing the bump and still applying `Suspend` is correct, needing only a code comment, not a fix. The `Positive`-vs-timeout direction IS a real bug, and deeper than non-atomicity: the correct anchor is WIRE-EVIDENCE (BATCH-READ) order -- every frame in one `PassThruReadMsgs` batch predates that read completing, so a timeout determined after the read invalidates every frame in that batch's `Positive`, regardless of exactly when within the batch the fold ran; fold-time capture (even made atomic) cannot express this. Fix: `LogicalLinkState.error_state_seq` splits back into TWO counters -- `error_clear_seq: u64` (bumped only by the four explicit-clear sites via `clear_error_suspension`, unchanged bump logic, `Suspend`'s own anchor) and `error_set_seq: u64` (bumped only by the timeout hook's flag-set, same critical section/placement as before, `Positive`'s own anchor). `Suspend` gating reverts to the second correction's shape essentially unchanged: `bind_frame`'s `wrote_class` out-param reverts to Suspend-only, renamed `wrote_suspend`; `CllRxEntry.class_seq` reverts to Suspend-specific, renamed `suspend_seq`, captured at the SAME existing post-fold site, now reading `error_clear_seq`, with a new code comment at that site stating the EXPOSURE-anchor argument so a future reviewer does not re-file this half. `Positive` gating moves to an entirely NEW mechanism: `CllRxEntry` gains `set_seq_at_read: u64`, captured ONCE per pass in `build_cll_rx_entries` (before any frame in the pass is processed), under the SAME `logical_links` lock already taken there for `connect_generation` -- zero new lock traffic; `Positive` classifications gate on `entry.set_seq_at_read == link.error_set_seq` at apply time instead of any fold-time capture, and no longer participate in the fold-capture mechanism (`bind_frame`) at all. No scenario needs a CROSS check (`Suspend` against `error_set_seq`, or `Positive` against `error_clear_seq`): an explicit clear racing a stale `Positive` is clear-vs-clear (a wake-free no-op regardless); a timeout racing a stale `Suspend` is set-vs-set (idempotent). `queue_error_class_to_apply_tests` (`events.rs`) was rewritten: `Suspend` cells carry over renamed to the split fields, plus a new cross-check (`suspend_ignores_error_set_seq_and_set_seq_at_read`); `Positive` cells are entirely rewritten for batch-anchor semantics -- the critical discriminating test, `positive_discarded_when_batch_was_read_before_a_timeout_bump`, sets `entry.set_seq_at_read` to a value simulating a batch read before an intervening timeout, then bumps `link.error_set_seq` once to simulate that timeout, and confirms the `Positive` classification is discarded even though no fold-time parameter is involved at all; `positive_applies_when_batch_was_read_at_current_error_set_seq` confirms the genuine-recovery direction; `positive_discarded_on_seq_mismatch_after_a_timeout_bump`/`positive_applies_when_captured_after_a_timeout_bump` (the old fold-time-seq mechanism for `Positive`) are removed, since that mechanism no longer exists for `Positive`; `positive_ignores_error_clear_seq_and_suspend_seq` is added as `Positive`'s own cross-check. `bind_frame_tests`' `wrote_class_flag_set_on_the_frame_that_folds_either_classification` is renamed `wrote_suspend_flag_set_only_on_the_frame_that_folds_suspend` and its Frame-2 (`Positive`) assertion is RE-INVERTED back to the second correction's original shape (`wrote_suspend` must NOT be `true` for a `Positive` fold). No new end-to-end integration test was added; the companion-interleave argument now applies independently to both direction-specific anchors -- see ADR-147's Consequences section for the full "two-mechanism reality" update.** **Follow-up fix (design-advisor careful re-derivation -- the SIXTH amendment to this mechanism: the `Positive` batch anchor's own capture point still ran too late.** `CllRxEntry.set_seq_at_read` was populated at `build_cll_rx_entries` construction time, but that function runs strictly AFTER `poll_rx_inner`'s `read_messages` call has already returned and its `api` lock released, with a genuine intervening `.await` (`ctx.last_bus_activity.lock().await`, for any batch with real content) a concurrent bump could land in -- so the fifth amendment's own capture point did not actually achieve the read-completion anchor it was believed to; simply moving the capture to "right after `read_messages` returns" does not fix this either, since that capture itself still needs `logical_links.lock().await` as its own yield point. The fix: `poll_rx_inner` now snapshots `error_set_seq` for every CLL on the channel under ONE `logical_links` lock acquisition, immediately BEFORE acquiring `ctx.api` for the read -- guard dropped before `ctx.api` is acquired, no lock nesting either direction -- achieving the weaker but sufficient inequality `T_capture <= T_read-start <= T_read-completion` instead of an unreachable "at read-completion" equality (unreachable because it would require being inside two different async locks' critical sections at once). This snapshot threads into `build_cll_rx_entries` as a new parameter; `CllRxEntry.set_seq_at_read` becomes `Option<u64>` (`None` for a CLL connected in the narrow window between the snapshot and `build_cll_rx_entries` running -- always discards `Positive` for that CLL this pass, conservatively, self-correcting on its next pass); `queue_error_class_to_apply`'s `entry_set_seq_at_read` parameter changes to match. A bump landing between the pre-read capture and read-completion is now absorbed (over-discard, fail-CLOSED, self-correcting on the next batch) rather than missed (fail-OPEN) -- the safe direction for a mechanism whose entire purpose is not resuming on unconfirmed evidence. An atomic (lock-free) `error_set_seq` was considered and rejected: it would let a reader observe the counter bump and the `tx_suspended_by_error` write as separately-ordered events, which are currently paired inside one `logical_links` critical section -- trading a checkable lock-discipline argument for a harder-to-verify memory-ordering one. Two new unit tests, `present_cll_is_stamped_from_the_snapshot_not_live_state`/`absent_cll_is_stamped_none` (`events.rs`, new `build_cll_rx_entries_tests` module), pin the stamping logic directly; `queue_error_class_to_apply_tests` gained `positive_discarded_when_set_seq_at_read_is_none`. As with every prior round, the actual cross-task interleave this round closes remains structurally unforceable deterministically in this harness -- no new end-to-end integration test was attempted; see ADR-147's Consequences section, now updated to correct the fifth amendment's own "captured before any frame is processed" framing (conflated with "at read-completion") that this round's re-derivation found imprecise.** **Follow-up fix (ADR-147 seventh amendment, Codex review + design-advisor + edge-case-hunter): the `Suspend` SET was published only at end-of-pass, after the triggering frame had already been delivered to the client via `deliver_or_enqueue`, on any CLL (not only dual-channel) -- a dispatch task reacting to the delivery could observe `tx_suspended_by_error == false` and transmit before the deferred apply landed.** Fixed by publishing the flag eagerly, in the same critical section that captures `entry.suspend_seq`, strictly before delivery -- gated on `connect_generation` freshness and a live policy re-check, deliberately with no seq-counter bump (would break same-batch last-frame-wins `Positive` handling) and no wake (false -> true only); the end-of-pass loop is unchanged and remains authoritative for last-frame-wins reconciliation and the `Positive` clear-plus-wake path. Two new tests were added (`queue_error_suspend.rs`) but are steady-state pins, not fail-before/pass-after discriminators for the eager write itself -- this crate's `current_thread` test runtime never yields mid-pass when uncontended, so the whole pass always completes synchronously regardless of the fix (verified by revert-and-rerun); correctness was instead confirmed by direct code inspection and independent design-advisor/edge-case-hunter re-derivation. See ADR-147's new seventh-amendment section for the full argument. **Follow-up fix (ADR-147 eighth amendment, Codex review): `ioctl_resume_tx_queue` (`rpc_misc.rs`) captured `channel_key`/`tx_queue` in one `logical_links` acquisition and cleared the suspend flags plus sent the wake in a later, separate one -- a same-handle disconnect+reconnect racing in between could clear a brand-new session's suspension while waking the OLD session's (now stale) `tx_queue`.** Fixed by capturing `connect_generation` alongside `channel_key` and re-checking it in the second acquisition, skipping both the clear and the wake on a mismatch -- the same freshness-gate pattern already used everywhere else in this mechanism. See ADR-147's eighth-amendment section for the full argument, including why this was NOT extended to `PDU_IOCTL_RESET`'s structurally similar call site (undetermined whether generation-scoping is even the correct semantics for a module-wide reset).
- `CP_P2Star` and every `CP_RC{21,23}CompletionTimeout`/`CP_RC{21,23}RequestTime` value is microsecond-denominated, like every other D-PDU timing ComParam (`CP_P2Min`/`CP_P2Max`, `N_Ar`/`N_As`/etc.) — `RcHandlingConfig::from_params` converts via the same `div_ceil(1000).max(1)` pattern `ComParamSet::p2_max_timeout_ms` uses, treating 0/absent as "use the ms-denominated fallback default." Do not read these raw (ADR-057 fixed exactly this bug — the fallback defaults were ms-sensible, masking that the actual preset literals were being read 1000x too large). **`CP_RC21RequestTime`/`CP_RC23RequestTime` are a partial exception to that "0/absent → fallback default" convention, and the exception itself splits into two different rules by protocol family (ADR-125, B11 in `iso22900-2-conformance-audit.md`): Annex I.1.4.3 defines an explicit `0` as a real configured value, not "unset," so `RcHandlingConfig::from_params` takes a `protocol: ChannelProtocol` parameter and, when the request-time ComParam itself is present in the `ComParamSet` AND `protocol.is_kwp_family()`, their fallback default is `0`, then floored by `Max(CP_P3Min, value)` (`CP_P3Min` = native `P3_MIN`, itself only ever settable via `SetComParam` on a K-line CLL — `comparam_support.rs`'s `is_can_param` never allowlists it). When the request-time ComParam is present AND `protocol.is_j1850_family()` instead, the value is used verbatim with NO `CP_P3Min` floor — `CP_P3Min` has no defined value for J1850 at all (Table B.10/B.19, J2534-1 v04.04 scope it to ISO 9141/ISO 14230 only), implemented by forcing `p3_min_ms = 0` on that branch so the shared `.max(p3_min_ms)` expression becomes a no-op. Otherwise — CAN-family `protocol`, or any protocol whose `CP_RC2xRequestTime` was genuinely never seeded (a legacy/raw CLL created via a numeric ID matching no resources-table row gets a fully empty `ComParamSet`) — neither rule engages, and `rc21_request_time_ms`/`rc23_request_time_ms` fall back to the original plain `25` ms default, unchanged from before ADR-125. Five Codex/adversarial-review rounds on PR #136 progressively found that every simpler gate was unsound (Codex itself hit its usage limit after round 4; round 5 was an `edge-case-hunter` adversarial pass run in its place, then a `design-advisor` consult resolved the genuine spec question it raised): round 1 shipped the floor fully ungated (relying on no CAN preset seeding `CP_P3Min`, which missed that `iso15765_4_common`/`iso_15765_3_on_iso_15765_2` seed `CP_RC23RequestTime = 0` too, with `CP_RC23Handling` runtime-enable-able); round 2 gated on `CP_P3Min`'s raw value being nonzero, which couldn't distinguish "absent" from a K-line client legally `SetComParam(CP_P3Min, 0)`-ing, then on `CP_P3Min`'s *presence*, which broke on resource `ISO_14230_3_on_ISO_15765_2` (0x0204) — a CAN-family (ISO15765 hardware) preset that nonetheless seeds a literal, client-inaccessible `CP_P3Min = 55_000`; round 3 switched to the real `ChannelProtocol`, sidestepping `CP_P3Min`'s presence entirely, but that alone still misread a legacy K-line CLL's *genuine* absence of `CP_RC2xRequestTime` as the explicit-`0` case; round 4 added the request-time-presence condition — safe because (unlike `CP_P3Min`) `CP_RC21RequestTime`/`CP_RC23RequestTime` are allowlisted on CAN, KWP, and both J1850 arms, so their presence carries no per-protocol-inaccessible-value trap; round 5 widened the K-line-only gate to also cover J1850 (Annex I.1.4's own title names both protocol families), but as a distinct no-floor rule rather than extending the K-line `Max` mechanism, since J1850 genuinely has no `CP_P3Min`.** In `wait_for_expected_response` (`events.rs`), RC21/RC23 still each get a completion-timeout ceiling computed once, on that code's *first* occurrence in the COP (`rc21_ceiling`/`rc23_ceiling`) — a repeat of the same code continues re-requesting under the existing ceiling rather than recomputing (pushing) it; a different code gets its own independently-anchored ceiling (ADR-057). RC78 no longer follows this anchor-once pattern (ADR-102): its deadline reloads on every occurrence (`rc78_p2_star_ms`), and `rc78_ceiling` -- when `rc78_total_ceiling_ms` is set -- is instead an optional independent total-duration cap anchored once at the first 0x78, not a ceiling on the deadline computation itself. All of RC78/RC21/RC23's deadline writes also clamp to `match_reset_ceiling` (`CoptStopcomm`'s IS-MULTIPLE ceiling, ADR-087) when one is in scope (ADR-102) -- RC21/23's own request-time sleep and retransmit additionally check it before, during (per chunk), and once more after the sleep.
- Update lifecycle behavior and docs together when changing shutdown/startup flows.
- Sync ComParam/protocol mapping tables in `docs/` after any mapping change.
- ComParam values that need unit/encoding conversion before reaching hardware go through `comparam_id.rs::to_j2534_config_value` (run at both hardware-forwarding call sites, after `to_j2534_config_id` and after any `expand_*` fan-out/split step such as `expand_uart_config`/`expand_tidle` -- so a derived entry is converted exactly once, alongside every other entry), not ad hoc logic at the call site (see ADR-037, ADR-072).

## Related Documents

- [startup-spec.md](startup-spec.md) — Startup argument format and stdio JSON-RPC contract.
- [protocol-mapping.md](protocol-mapping.md) — ISO 22900-2 protocol name to J2534-1 protocol ID mapping, and the `GetResourceIds`/`CreateComLogicalLink` resource table (ADR-069).
- [comparam-mapping.md](comparam-mapping.md) — ISO 22900-2 CP_* ComParam to J2534-1 parameter ID mapping.
- [comparam-protocol-support.md](comparam-protocol-support.md) — Per-protocol ComParam support matrix.

## Test-suite reliability: past flaky-test root causes

Six flaky-test root causes have been found and fixed in the `tests/grpc_mock/` suite. Each turned out to be a defect in the test or its harness. In every case a read of the production code showed no race there. The write-time rules these led to are in `tests/grpc_mock/harness.rs`'s module doc and ADR-149. The measured round-trip ceiling is `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS` in the same file (about 85 ms under load).

Triage lessons:

- A failure is a regression only if it reproduces in isolation, over several consecutive runs, on a commit that predates the change.
- If widening a suspected timing margin makes a test fail more often, or every time, the margin theory is wrong. Stop widening and add temporary instrumentation (`eprintln!` in the test and the production path), then remove it. Cause 5 below was found this way.
- Rotating failures in unrelated tests that disappear when run alone point to shared process or cross-process state (cause 6), not to timing.

Timing-margin causes (ADR-149):

1. **Chained relative sleeps with almost no margin.** `tester_present_send_type.rs::send_type_1_idle_timer_resets_on_prior_bus_traffic` chained `sleep(40ms)` and `sleep(35ms)` against an 80 ms idle interval. That left about 5 ms before the deadline, and normal round-trip overhead used it up. The test now waits with `tokio::time::sleep_until` on one anchored deadline and uses a 300 ms interval. Its structure copies the second test below.
2. **The same defect in an older test body.** `tester_present_send_type.rs::kline_five_baud_init_stamps_last_bus_activity_deferring_mode_1_sibling` failed once while it still had the chained-sleep shape of cause 1. The current body already uses the anchored `sleep_until` pattern and has not failed since. It is the model for new idle-timer tests.
3. **An exact assertion racing an event round trip.** In `tester_present_send_type.rs::send_type_1_no_periodic_start_and_fires_one_shot_after_idle`, the first check (exactly one frame written) ran against a 50 ms idle interval. The client learns about the immediate send only after an event round trip that can take longer than 50 ms, so the idle-triggered second frame could already be there. Relaxing the check to `>= 1` was rejected: it would no longer tell an arm-time send from a delayed idle-timer send. The fix widens `CP_TesterPresentTime` to 300 ms and keeps `== 1`. The elapsed bounds are 225 ms to 1500 ms. The upper bound stays well under the preset's 2 s default, so a missing override is still caught.
4. **Two round trips inside a window sized for one.** `stopcomm_data_tx.rs::stopcomm_disconnect_then_reconnect_same_channel_suppresses_stale_final_transmit` sleeps 150 ms into a 300 ms `CP_P3Phys` gap, then does a disconnect and a reconnect before the gap ends. Its sibling `stopcomm_disconnect_racing_the_p3_gap_wait_suppresses_the_final_transmit` does one round trip in the same window and is stable. The margin must grow with the number of round trips. The gap is now 800 ms and the settle sleep 700 ms. The production check it exercises (the re-check of the connect generation after `wait_for_p3_gap` in `handle_stop_comm`, `events.rs`) was confirmed correct.
5. **A predicate not scoped to the entity under test.** `cop_ctrl_cycles.rs::receive_only_cyclic_reap_gated_by_exhaustive_drain_not_just_a_poll_running` used an "is finished" check that matched any COP's `PduCopstFinished`. The `CoptUpdateparam` that `set_unique_resp_table_and_promote` issues before `subscribe` had already finished. Its events sit in the link's event buffer and are replayed first to any new subscriber, about 40 ms later. The test was therefore checking that stale event, not cop_a. Widening the margins 6x made it fail every time, which exposed this. Every check now filters on cop_a's own handle (`is_finished_for_cop_a`, as in `cyclic_reap_defers_until_the_uudt_companion_channels_watermark_also_catches_up`). The wider margins stay as extra safety.

Shared-state cause:

6. **Startup race on the harness config file (ADR-195).** `TestServer::try_start_with_extra_config` (`harness.rs`) writes a temp config file and sets the process-wide `VCI_CONFIG_PATH`, and `J2534Service::new` reads both back before its first `.await`. Concurrent starts could interleave. `vci-service-config::load_toml_config` treats a torn read as "nothing configured", so `library_path` was empty and the service fell through to Windows registry discovery. The visible symptom was `RegistryUnsupported` ("registry lookup is only supported on Windows") on non-Windows hosts, or a test silently getting another test's config. Two layers fix it. Inside one process, `VCI_CONFIG_STARTUP_LOCK` (a `tokio::sync::Mutex`) is held across the file write, the env set and `J2534Service::new(..).await`. Across processes (two `cargo test` runs at once), the file name now includes `std::process::id()`. The same suffix is used at the other three sites with this pattern: `tests/live_grpc_flow.rs` (`resolve_library_name`), `iso22900-service`'s `tests/grpc_mock.rs` (`TestServer::start`) and `src/service/rpc.rs` (`set_mock_library_path_config`). The lock alone did not cover the cross-process case. Running two test processes together on a tree without the suffix reproduced the failure reliably. A suspected ordering race between `spawn_channel_poll_task` and `TestServer::shutdown()` was tested with instrumentation and ruled out.

## SAE J2534-2 Discovery Mechanism (ADR-153, Phase 1)

`service/discovery.rs` adds an epoch-tagged, lazily-populated cache of
`GET_DEVICE_INFO`/`GET_PROTOCOL_INFO` results (`J2534Service::
discovery_device_info`/`discovery_protocol_info` fields, `HashMap` keyed by
parameter ID / `(protocol_id, parameter)` respectively), mirroring the
`resolved_can_channel_mode`/`j1850_bus_flavor` device-derived cache pattern
(ADR-107 addendum (h)) rather than an eager blanket-query-at-open-time
design — see ADR-153 for why. A lookup is gated on the target module's
`pname` carrying the `"J2534-2:"` prefix (clause 5; `is_j2534_2_opted_in`) —
a module never opted into J2534-2 is never queried. **Internal-only for
this phase:** no `PDU_IOCTL_*`/RPC surface exposes this to clients yet
(ADR-153 Decision 3). **Updated by Phase 2b:** `discovery_device_info`
itself still had no production caller of its own at Phase 2b (see the "SAE
J2534-2 Additional Channels" section's deadlock note above for why
`check_chx_capacity` can't be that caller directly), but its shared
cache-hit/native-call machinery (factored into
`discovery_device_info_cache_hit`/`discovery_device_info_with_open_device`)
did, via `check_chx_capacity` — the first real connect-time capability
decision this crate makes with Discovery data. **Updated by ADR-185 Stage
1:** `discovery_device_info` is now reachable in production code, through
`J2534Service::enforce_discovery_capability`'s `DeviceAccess::OpenIfNeeded`
resolution path (see the "SAE J2534-2 Discovery-Cache Connect-Time/IOCTL-Time
Enforcement" section below) -- though no Stage 1 call site actually
constructs `OpenIfNeeded` yet (every one already holds a `device_guard`),
so it still has no call site that runs it at runtime. **Updated by ADR-185
Stage 2 (including its same-PR lock-order fix):** `enforce_discovery_capability`'s
`ProtocolCapacity` arm now resolves for real, giving
`discovery_protocol_info_with_open_device` (mirroring
`discovery_device_info_with_open_device`) its first production caller --
`ioctl_start_repeat_message` (`rpc_misc.rs`), via `DeviceAccess::AlreadyOpen`.
Stage 2 originally wired this up via `DeviceAccess::OpenIfNeeded` instead
(deferred until after `shared_channels` was already locked), but that
inverted this crate's documented `device_id`-outermost lock order
(`service.rs`'s `require_connected_device_for` doc comment, ADR-107
addendum) and risked an AB-BA deadlock against `ConnectComLogicalLink`'s own
lock order -- caught by design-advisor review before merge and fixed in the
same PR by acquiring `device_id` before `shared_channels`, matching every
other call site. `DeviceAccess::OpenIfNeeded` and `discovery_protocol_info`
remain fully implemented and unit-tested but have no production caller
again. Unit test coverage lives in
`service/discovery.rs`'s own `#[cfg(test)]` module, built the same way as
`rpc_misc.rs`/`rpc_link.rs`/`rpc_module.rs`'s existing mock-backed
`J2534Service` test harnesses; Phase 2b adds the first `tests/grpc_mock/`-level
coverage of this data actually gating a real RPC decision
(`tests/grpc_mock/additional_channels.rs`'s `_CHx` connect tests).

**Adding these fields is a same-PR propagation duty (atomicity per ADR-155) across every
hand-built `J2534Service` struct literal in this crate**, not just
`Service::new`: as of this phase, that is `service.rs` (`Service::new` and
its own `#[cfg(test)]` `minimal_service`), `rpc_misc.rs`
(`service_with_seeded_queue`), `rpc_link.rs`
(`service_with_auto_can_mode_and_no_links`), `rpc_module.rs`
(`minimal_service_with_modules`), `rpc_primitive.rs`'s own test builder, and
`service/discovery.rs`'s own `#[cfg(test)]` builder (`service_with_pname`)
— seven literals total (an edge-case-hunter finding on this same phase's
diff caught this note initially undercounting its own addition by one, an
irony worth remembering: run the grep below rather than trusting any
hand-maintained list, including this one). A future field addition to
`J2534Service` must update all of these (`grep -n "J2534Service {"
j2534-0404-service/src -r` finds every current site); missing one is a
compile error, not a silent bug, but
still worth flagging up front rather than discovering it file-by-file.

## SAE J2534-2 Pin Selection (ADR-156, Phase 2a)

`names.rs` resolves SAE J2534-2 clause 6 Pin Selection at
`CreateComLogicalLink` time, layered on top of the existing
`parse_protocol_id_from_resource`/`resolve_protocol_id`/`resolve_protocol_name`
resolution: `resolve_pin_selection` compares the caller's
`ResourceData.dlc_pin_data` structurally (number *and*, where the caller
specified one, type) against the resolved protocol's default DLC pins (the
matched resource-table row's own `dlc_pins`, or
`resources::default_dlc_pins_for_hw_protocol` when no row matched) via
`dlc_pin_data_matches_defaults` — an exact structural match (same pin
count, every number/type pair equal to the default's, an untyped requested
pin matching any default type for that number) or no `dlc_pin_data` at all
leaves resolution unchanged (`Ok(None)`), matching clause 6.3.3.2's
auto-connect-to-default-pins behavior. (A bare pin-number-*set* comparison
was tried first and found buggy — Codex review, PR #28: it silently
accepted a swapped-polarity request whose pin numbers happened to match the
default set, and silently accepted malformed `dlc_pin_data` with
duplicate/extra entries whose deduplicated numbers happened to match too;
`dlc_pin_data_matches_defaults`'s own doc comment has the details.) A
genuine non-default request is rejected `invalid_argument`
unless the connecting module opted into J2534-2 (`pname` carrying the
`"J2534-2:"` prefix, `discovery::is_j2534_2_opted_in`, ADR-153), then
resolved via `resources::ps_protocol_id` to a `_PS` hardware protocol id
and packed into a `pin_select: u32` bitmask by `compute_pin_select`
(`0x0000PPSS`, validated against clause 6.3.3.2 Table 3's numeric
range/exclusion/`PP != SS` constraints — see `compute_pin_select`'s own doc
comment and ADR-156's Corrections for exactly what it does and does not
validate).

**Raw `_PS` protocol id normalization (ADR-156 Correction, Codex review, PR
#28):** a caller can also name a `_PS` hardware protocol id directly via
`protocol_id` (e.g. `PROTOCOL_CAN_PS`) instead of requesting non-default
pins on the base id — `resolve_protocol_id`'s table fallback preserves that
raw id unchanged, since `_PS` ids have no `resources` table row.
`resolve_pin_selection` normalizes the resolved hardware id via
`resources::base_protocol_id` (ADR-157) before every default-pins/
`ps_protocol_id` lookup, and tracks whether normalization changed the value
(`already_ps`) to reject an empty `dlc_pin_data` outright when the caller
named a `_PS` id directly (it has no pins to compute `SET_CONFIG` from, so
the channel could never be assigned). The bare `ResourceId`/`ResourceName`
variants (no `dlc_pin_data` field at all) also call
`resolve_pin_selection(..., &[], ...)` for the same reason -- a raw `_PS`
id numerically supplied through either is rejected the same way, since
neither route can ever supply pins (found by `edge-case-hunter` as a
sibling gap the first fix left uncovered). **Accepted residual (ADR-157):**
naming `PROTOCOL_J2610_PS` directly (no resource row) with a
non-representative SCI variant's pins connects and pins correctly, but
`base_protocol_id`'s single-representative SCI collapse means
`GetResourceStatus`'s ambiguous-name occupancy reporting for such a link is
internally inconsistent — a status-reporting-only gap, not a connect
failure.

**Default-wiring `_PS` canonicalization (ADR-156 Correction,
design-advisor, PR #28, a later round):** a directly-named `_PS` id whose
supplied pins structurally match the base protocol's own defaults
canonicalizes to `Ok(None)` -- an ordinary base-identity connect, no
`SET_CONFIG(CONFIG_J1962_PINS)` at all, sharing its `ChannelKey` (and
therefore its physical-resource lock scope) with a plain default-pins
connect of the same protocol. An earlier version of this fix always
computed a real `pin_select` for this case instead, reasoning that a `_PS`
channel starts pin-unassigned regardless of what its pins equal (true per
clause 6.3.3.2, but incomplete): that gave one physical wiring two
different `ChannelKey`s, so the two connects opened separate physical
channels and a lock held by one never protected the other -- a real
correctness gap Codex review found. The J2534-2 opt-in gate now runs
*before* the defaults short-circuit for a directly-named `_PS` id
specifically, so opting out of J2534-2 still rejects such a request even
though its pins would otherwise canonicalize.

**`LogicalLinkState.protocol` normalization (ADR-157 Correction,
design-advisor, PR #28):** naming a `_PS` id directly also left `protocol`
itself holding the raw `_PS` value, violating the invariant every
`link.protocol` consumer (`is_can_family`/`is_kwp_family`/`is_j1850_family`/
`is_sci_family`, `unique_id_params`, `uds_session_timing_applies`,
`tx_message_size_range`, `GetResourceStatus`'s `active_candidate` match)
relies on -- found as a fourth independent instance of the same
normalization gap, crossing this repo's design-advisor escalation
threshold. Fixed at the source rather than by enumerating consumers:
`parse_protocol_id_from_resource` normalizes `protocol` to its base
identity (`resources::base_protocol_id`) at its single return point, after
`resolve_pin_selection` (whose `already_ps` detection needs the raw value
first). Identity for every non-`_PS` value. The `GetResourceStatus`
`ResourceId` query route (`rpc_link.rs`) has the same `ChannelProtocol::
from_raw` fallback shape for a raw `_PS` numeric id with no table row and
got the same normalization, so querying by a raw `_PS` id directly still
finds a link connected with that exact id.

**Two more sites (ADR-157, Codex review, PR #28, the round after the above):**
`events.rs::handle_start_comm` passed the raw `protocol_id` (not
`base_protocol_id`, already in scope in the same function) to
`header_footer_len`, so a pin-selected K-line link's fast-init response
skipped the KWP header/footer split entirely (the whole frame landed in
`data_bytes`). And `j2534-0404-mock`'s `IOCTL_FIVE_BAUD_INIT`/
`IOCTL_FAST_INIT` handlers normalized the protocol-family check
(`base_protocol_id`) but never checked `channel.pins_assigned` at all --
unlike every other I/O IOCTL in the mock -- so a `_PS` K-line channel could
run either init sequence over unassigned pins. Both fixed; see ADR-157 for
detail.

**A third site (ADR-157, Codex review, PR #28, the following round):**
`rpc_get_resource_status`'s `status_hw_id` substitutes the module-wide
`can_channel_mode`'s hardware mapping (raw `CAN` for ISO15765-family in
`SoftwareIsoTp` mode) -- but Pin Selection overrides that mode per-link
(`_PS` links are always hardware-ISO-TP), so querying the ISO15765 resource
on a `software-isotp`-configured module missed a connected `ISO15765_PS`
link entirely. Fixed by also matching against `hardware_native_hw_id` (the
same candidate's identity without the mode substitution); see ADR-157.

**A fourth site, same round following** (`rpc_get_resource_status` again):
a raw `PROTOCOL_J2610_PS` query normalized via `base_protocol_id` (which
collapses all four native SCI ids to one representative, `SCI_A_ENGINE`)
falsely missed a connected link using a different variant (e.g.
`SCI_B_TRANS`). Fixed by matching any SCI variant
(`resources::is_sci_hw_protocol_id`) specifically when the caller's
literal query id was the raw consolidated `_PS` id itself. **A fifth site,
in the mock:** `IOCTL_SET_CONFIG(CONFIG_J1962_PINS = 0)` -- clause
6.3.3.2's own sentinel, never produced by the service -- unconditionally
marked pins assigned, letting a direct mock client bypass the
`ERR_PIN_INVALID` gate; fixed by only assigning pins for a nonzero value.
See ADR-157 for detail on both.

**Scope restriction (ADR-156 Correction):** Pin Selection only applies to
the `RscData::ProtocolId` route and the `RscData::ProtocolName` route when
`name` does **not** match any `resources` table row by canonical ISO
22900-2 name (i.e. only the `map_protocol_name` legacy-alias fallback, e.g.
`"can"`/`"iso15765"`). A `ProtocolName` that matches a table row (e.g. the
canonical `"ISO_15765_2"`) keeps `find_table_row_by_name`'s pre-existing
all-or-nothing pin-narrowing contract (ADR-106/ADR-069) completely
unchanged — `dlc_pin_data` there disambiguates a specific, fixed resource
configuration, not a dynamic clause 6 pin choice. `ResourceId` and
`ResourceName` (a bare string, which cannot carry `dlc_pin_data` at all)
never trigger Pin Selection either way. See `parse_protocol_id_from_resource`'s
doc comment for the full routing.

**`ChannelKey`/`LogicalLinkState` widening:** `ChannelKey` (`rpc_link.rs`)
widened from `(hw_protocol_id, baud_rate)` to `(hw_protocol_id, baud_rate,
pin_select)`, with `pin_select = 0` for every non-`_PS` link — this
reproduces the pre-Phase-2a 2-tuple's exact sharing behavior for every
existing protocol; two `_PS` links resolving to the same hardware protocol
and baud rate but different DLC pins now get distinct physical channels
(clause 6.3.2.1). `LogicalLinkState` (`service.rs`) gained a `pin_select:
Option<u32>` field (`None` for every non-`_PS` link) alongside
`hw_protocol_id`.

**Widened again, to a 4-tuple, by ADR-158 (Phase 3 Stage 3a, Codex review
correction):** `ChannelKey` is now `(hw_protocol_id, baud_rate, pin_select,
fd_data_phase_rate)`, `fd_data_phase_rate = 0` for every non-FD link (same
"`0` for the common case" trick `pin_select` already used) — two CAN FD
links at the same arbitration baud/pins but different effective data-phase
rates now get distinct physical channels, the same way differing
`pin_select` values already did. See ADR-158's Correction section for the
full rationale (effective connect-time-fixed rate treated as channel
identity, keyed like `baud_rate`, not join-time-compared like the
mutable `client_filters` check). Every paragraph below this point that
still describes `ChannelKey` as a 3-tuple is describing Phase 2's
original widening accurately as history; the type itself is a 4-tuple as
of ADR-158.

**Connect-sequence change (`rpc_link.rs`):** `connect_new_physical_channel`
takes a new `pin_select: Option<u32>` parameter; when `Some`, it issues
`PassThruIoctl(SET_CONFIG, CONFIG_J1962_PINS = pin_select)` immediately
after `PassThruConnect` and before ComParam application/filter install,
inside the same rollback-protected sequence (a native failure at either
step disconnects the channel and fails the RPC). Only reached on
first-open of the physical channel — a CLL joining an already-open shared
channel never re-issues it, since the joining CLL's `pin_select` is
already guaranteed equal by the widened `ChannelKey` match. The client
never observes an unpinned-but-connected channel: this entire sequence is
internal to one `ConnectComLogicalLink` RPC call.

**`LogicalLinkState` construction sites — same-PR propagation duty (ADR-155),
following the Discovery section's precedent above:** adding `pin_select`
(Phase 2a) and `channel_index` (Phase 2b) each touched every hand-built
`LogicalLinkState` struct literal in this crate — 9 total: the one
production site (`rpc_link.rs`'s `rpc_create_com_logical_link`, which
resolves both from `pin_selection`/`channel_selection`) plus 8 test-helper
literals (`service.rs::minimal_link`, `rpc_link.rs::minimal_not_connected_link`,
`rpc_misc.rs`'s `service_with_seeded_queue`-adjacent builder,
`rpc_primitive.rs`'s own test builder, and `events.rs`'s four separate
`minimal_link` helpers). A future field addition to `LogicalLinkState`
must update all of these (`grep -n "LogicalLinkState {"
j2534-0404-service/src -r` finds every current site, mirroring the
Discovery section's own grep-don't-trust-the-list caveat above) — missing
one is a compile error, not a silent bug, but still worth flagging up
front. `ChannelKey` itself needs no equivalent enumeration: as a bare tuple
type alias (not a struct with named fields), every call site using a
narrower arity already fails to compile against a widened one (the same
property held for the Phase 2a 2-tuple→3-tuple widening and again for
ADR-158's Phase 3 3-tuple→4-tuple widening above), so there is no
silent-drift risk to document separately — `_CHx` (Phase 2b) needed no
further `ChannelKey` widening at all (ADR-156 Decision 3: the hardware id
itself already differs per index).

## SAE J2534-2 Additional Channels (ADR-156, Phase 2b; field-based route removed by ADR-178)

**Revision note (ADR-178, corrected by Codex review PR #67):** Phase 2b
originally shipped two input routes to the same resolution — a
`ResourceData.channel_index` field (`0`/absent = base channel, `1..=128`
selects `_CH1`..`_CH128`) and a directly-named `_CHx` hardware protocol id.
ADR-178's first draft removed the `channel_index` proto field on the
premise that the directly-named route alone resolves identically for every
in-scope family — true for six of the seven, but **not** for SAE J2610
(Chrysler SCI): its four native hardware ids (`SCI_A_ENGINE`/`SCI_A_TRANS`/
`SCI_B_ENGINE`/`SCI_B_TRANS`) all collapse onto one shared `_CHx` numeric
block, so a bare numeric `_CHx` id cannot say which variant a caller means
(`resources::chx_base_protocol_id` always decomposes to the `SCI_A_ENGINE`
representative). Codex review caught this as a real regression (3 of 4 SCI
variants unreachable on an Additional Channel) before merge. **Fix:**
`resolve_channel_selection` regained an index-driven resolution arm — not
the removed proto field, but a compound-name grammar
(`"<name>_CH<n>"`) accepted in the two existing `string`-typed routes
(`ResourceName`, `RscData.protocol_name`) only, tried after plain-name
resolution fails. The rest of this section (capacity precheck, the deadlock
fix, `rpc_link.rs` qualifier-gate widening, `GetResourceStatus`'s `_CHx`
handling) describes internal mechanics keyed on the *resolved* index
(`LogicalLinkState.channel_index: Option<u32>`), unaffected by either the
field's removal or the compound-name grammar's addition — only the
paragraphs below describing the client-facing input shape were updated.

`names.rs` resolves SAE J2534-2 clause 7 Additional Channels at
`CreateComLogicalLink` time, layered on the same
`parse_protocol_id_from_resource` pipeline Pin Selection uses, and running
after it (`resolve_channel_selection` needs `resolve_pin_selection`'s
outcome to enforce mutual exclusion), through **two routes**:

- **Direct `_CHx` naming** (six of the seven in-scope families): a caller
  names a `_CHx` hardware protocol id directly — via `protocol_id`, or a
  bare `ResourceId`/`ResourceName` — which `resources::chx_base_protocol_id`
  decomposes to `(base_hw_protocol_id, index)` — a range-membership test,
  not an enumerable match, since block membership is what needs testing,
  and the exact inverse of `resources::chx_protocol_id(base_hw_protocol_id,
  index)`'s forward arithmetic mapping (`chx_base(base) + index - 1`, not a
  128-entry lookup table, since every in-scope `_CHx` family is a
  contiguous 128-value block). This differs from `_PS`'s bare-id handling
  (rejected, missing pin data) because a `_CHx` id is fully self-describing
  once decomposed — except for SAE J2610, where the decomposition always
  returns the `SCI_A_ENGINE` representative regardless of which variant was
  actually connected (see below).
- **Compound-name grammar** (SAE J2610 SCI specifically, added by the
  Codex-review fix): `resolve_protocol_name_with_chx_suffix` tries
  whole-string resolution first (unchanged), and on failure attempts one
  rightmost `"_CH<digits>"` suffix split (`split_chx_suffix` — case-
  insensitive, non-empty head, no recursion), resolving the head through
  the *same* name-resolution call used for a plain name. This preserves the
  caller's exact SCI variant (from the head's own resolution) independent
  of the numeric `_CHx` id's collapse, matching what the removed
  `channel_index` field route did — the caller's resource selector supplied
  the variant, the (always-ambiguous) numeric arithmetic supplied only the
  index. `resolve_channel_selection` takes this as `requested_index:
  Option<u32>` and computes `chx_id = chx_protocol_id(base_protocol_id(raw),
  requested_index)`, returning `(base_protocol_id(raw), chx_id,
  requested_index)` — `base_protocol_id(raw)` is the caller's exact variant
  when `raw` is already a specific base id, so this is functionally the old
  field route's own tuple, just fed by the compound name instead of a proto
  field. Numeric routes (`ResourceId`/`protocol_id`) never accept this
  grammar.

Both routes are scoped to the same seven clause-6.3.1 Table 1 protocols Pin
Selection covers (J1850VPW, J1850PWM, ISO9141, ISO14230, CAN, ISO15765, SAE
J2610); `resources::is_chx_protocol_id` tests membership in the FULL
clause-24/26 `_CHx` region (all 18 protocol families' blocks, 19 native
blocks since `SW_CAN` splits across two) — needed for the clause-5 opt-in
gate to catch an out-of-scope `_CHx` id before resolution, so it gets a
clean rejection rather than silently falling through as an unrecognized raw
id.

**Mutual exclusion and double-qualification:** either route (a directly-
named `_CHx` id, or a compound-name suffix) combined with non-default
`dlc_pin_data` is rejected `invalid_argument` — clause 7 channels live on
vendor connectors, never on J1962/J1939/J1708 pins, so an Additional-Channel
qualifier and Pin Selection can never both apply.
`resolve_channel_selection` reuses `resolve_pin_selection`'s own outcome
(`Option<(u32, u32, u32)>`) as the Pin Selection signal, plus
`is_ps_protocol_id(raw_hw_protocol_id)` unconditionally (a no-op on the
direct-`_CHx`-naming path, since a `_CHx` id can never also be a `_PS` id
by construction — but real on the compound-name path, where `raw` can be an
arbitrary resolved id, including a `_PS`-shaped compound-name head).
Supplying both a directly-named `_CHx` id and a compound-name suffix (e.g.
a compound name whose head resolves to a raw `_CHx` id itself) is rejected
as double qualification — two routes to the same qualifier must not both
be supplied, even when they'd agree.

**Opt-in gating (clause 5) applies before resolution**, mirroring
`_PS`'s gate exactly: any id matching `is_chx_protocol_id` (the full
18-family region), or any resolved `requested_index` from a compound name,
is rejected `invalid_argument` for a module that hasn't opted into
J2534-2. An out-of-scope-family `_CHx` id (matches `is_chx_protocol_id` but
not `chx_base_protocol_id`), or a compound-name index outside `1..=128` or
resolving to an out-of-scope family, is then rejected cleanly with its own
message rather than silently passing through as an unrecognized raw id —
mirrors `resolve_pin_selection`'s equivalent scope-narrowing rejection.

**`LogicalLinkState`/`LinkView` field changes:** `pin_select_base_hw_protocol_id`
(Phase 2a/ADR-157) is renamed `base_hw_protocol_override` (both structs) —
a qualifier-agnostic base-override field, populated identically by either
qualifier route at `CreateComLogicalLink` time. `LogicalLinkState` gains
`channel_index: Option<u32>` (`Some(1..=128)` for a `_CHx` link, mutually
exclusive with `pin_select`); `LinkView` does NOT mirror it, since none of
`LinkView`'s consumers (`GetComParam`/`SetComParam`/`SetComParamField_Bytes`/
`SetComParamField_Struct`) need to know WHICH qualifier route produced the
override, only its Plane B answer, which `base_hw_protocol_override` alone
already gives them. Both `base_hw_protocol_id()` accessors (`LogicalLinkState`'s
and `LinkView`'s) gate on `base_hw_protocol_override.is_some()` directly —
not on `pin_select.is_some()`, which only covered the `_PS` case before
`_CHx` also populated the same override field. Every existing call site
already satisfied this (both accessors already used `.unwrap_or(...)`
against the Option directly, not an explicit `pin_select.is_some()`
branch), so no accessor-body change was needed — only the doc comments and
the field's own populate-together invariant needed updating to state the
qualifier-agnostic contract explicitly.

**Capacity precheck (ADR-156 Decision 4 correction, implemented in Phase
2b — unlike `_PS`'s deferred pin precheck):** `J2534Service::check_chx_capacity`
(`discovery.rs`) queries `resources::chx_device_info_supported_parameter(base_hw_protocol_id)`'s
Discovery answer (the `DEVICE_INFO_<PROTOCOL>_SUPPORTED` clause-25.3.2.2
parameter for that family — the consolidated `DEVICE_INFO_J2610_SUPPORTED`
for the SCI case). Its packed `0xPPQQRRSS` value's `QQ` byte (bits 16-23 --
not bits 8-15, which is `RR`, the unrelated `_PS` channel count; Codex
review, PR #29, caught both this service's original mis-extraction and a
matching mis-packing in the mock, which concealed each other in testing)
carries the count of available `_CHx` indices, contiguous from 1;
`channel_index` above that count is rejected synchronously with
`invalid_argument`. Called from `rpc_connect_com_logical_link`'s
brand-new-physical-channel path (never for a CLL joining an already-open
shared channel, which already proved capacity was available), immediately
before the native `PassThruConnect` — when nothing is cached (module not
opted in, or Discovery never queried), this is a no-op and the native
connect error remains the fallback (ADR-153 Decision 1).

**Deadlock found and fixed while writing the first end-to-end `Connect`
test for a `_CHx` link (hung, not panicked — the integration-test-hang
class `harness.rs`'s module doc warns about, traced to a real bug rather
than a leaked subscription):** `check_chx_capacity` is always called from
`rpc_connect_com_logical_link` while that function's own `device_guard`
(a `tokio::sync::MutexGuard` on `self.device_id`, from an earlier
`ensure_open_device` call) is still held, across the entire join-or-create
decision, by design (ADR-107 addendum (i)). The first version of
`check_chx_capacity` called `discovery_device_info` directly, which
internally calls `ensure_open_device_for` — re-locking that SAME
`self.device_id` mutex from within the same task. `tokio::sync::Mutex` is
not reentrant, so this deadlocked the calling task against itself on any
`Connect` for a `channel_index`-qualified link that opens a brand-new
physical channel (the create-a-new-shared-channel branch always reaches
`check_chx_capacity` when `channel_index.is_some()`). Fixed by splitting
`discovery_device_info` into three pieces: `discovery_device_info_cache_hit`
(the cache lookup alone, no locks beyond the cache's own), the public
`discovery_device_info` (opens its own device via `ensure_open_device_for`
when the caller has no guard of its own — its original contract, now with
`#[allow(dead_code)]` restored since Phase 2b's real caller can't use it
directly), and `discovery_device_info_with_open_device` (the native-call/
cache-write half, taking an already-open `DeviceId` directly, no locking of
`self.device_id` at all). `check_chx_capacity` now takes `device_id:
DeviceId` as a parameter (passed from `rpc_connect_com_logical_link`'s own
`device_id`, already in scope alongside `device_guard`) and calls
`discovery_device_info_with_open_device` directly, never
`ensure_open_device_for`. Caught by running the new
`tests/grpc_mock/additional_channels.rs` integration tests in isolation
before considering this feature done — the specific test that hung
(`two_chx_links_at_different_indices_get_distinct_channels`) is the first
one in this codebase to actually `Connect` a `channel_index`-qualified
link to a brand-new physical channel; every earlier `_CHx` test only
exercised `CreateComLogicalLink` resolution, which never reaches
`check_chx_capacity` at all.

**`rpc_link.rs` qualifier-gate widening:** every `_PS`-specific
`pin_select.is_some()`/`.is_none()` conditional that exists purely to
decide "does this link carry a connect-time qualifier" (as opposed to using
`pin_select`'s actual bitmask value, e.g. the `SET_CONFIG(CONFIG_J1962_PINS)`
call) widened to "either qualifier present"
(`pin_select.is_some() || channel_index.is_some()`): the software-ISO-TP/
dual-channel-mode-probe skip and its paired dual-channel-mode UUDT
companion-channel-open skip (`rpc_connect_com_logical_link`'s tail and
`promote_unique_resp_id_table`'s equivalent block), and
`install_point_to_point_fc_filters`'s own internal "does a companion exist
for this link" condition (its `pin_select: Option<u32>` parameter became a
`qualified: bool`, computed identically at all three call sites — the
connect-time tail, `promote_unique_resp_id_table`, and
`reinstall_iso15765_channel_filters_after_clear`) — these two conditions
must move together (an edge-case-hunter finding from Phase 2a: letting them
drift apart loses UUDT capture silently, worse than either being wrong
alone). SAE J1850 flavor auto-detect's cache bypass
(`autodetect_sae_j1850_flavor`) widened the same way; its write-back now
threads the detected flavor back through `resources::chx_protocol_id`
(mirroring the existing `ps_protocol_id` write-back) when the link is
`channel_index`-qualified rather than `pin_select`-qualified — see
ADR-156's Corrections for the accepted residual on the probe itself not
being connector-aware.

**`GetResourceStatus`'s `PROTOCOL_J2610_PS` literal-broadening special
case gains a `_CHx` analog:** a literal query for an id inside the
`PROTOCOL_J2610_CHx` block decomposes via `chx_base_protocol_id` to one
representative SCI variant plus a channel index (the query id itself never
distinguishes which of the four native SCI ids the caller means, since
`chx_protocol_id` already collapses all four onto one block per index), so
it must match a connected link on ANY SCI variant AT THAT SAME INDEX —
unlike the `_PS` case, per-index granularity still applies: a literal
`_CHx` query matches only its exact index, never a different index of the
same family. A `_CHx` link occupying its base resource for a base-protocol
status query already works for free, since `base_hw_protocol_id()` reads
`base_hw_protocol_override`, which both qualifier routes populate the same
way. **Accepted residual (unchanged from ADR-157):** per-index occupancy
granularity for a *base*-resource status query (distinguishing which of
several connected `_CHx` indices is in use from a base-protocol query
alone) is not implemented.

**Mock (`j2534-0404-mock`):** extends its own crate-local `PS_PROTOCOL_IDS`/
`BASE_PROTOCOL_IDS` positional-array pattern with a `CHX_BLOCK_BASE_IDS`
range table (`is_chx_protocol_id`/`chx_base_protocol_id`, mirroring the
service's shape without sharing code across the FFI boundary). The
existing `pins_assigned` gate does NOT apply to `_CHx` channels — clause 7
channels are vendor-connector-based, never J1962-pin-tied, so a `_CHx`
channel starts with pins already "assigned" (this falls out of the
existing `pins_assigned: !is_ps_protocol(protocol_id)` initializer for
free, since `is_ps_protocol` never matches a `_CHx` id). `PassThruConnect`
now simulates `_CHx` capacity/conflict: an index beyond
`chx_capacity_override.unwrap_or(DEFAULT_CHX_CAPACITY)` (overridable per
test via `__mock_set_chx_capacity`) or an out-of-scope family fails
`ERR_NOT_SUPPORTED`; a second connect with the exact same `_CHx` id already
open fails `ERR_RESOURCE_IN_USE` (clause 7's own conflict-vs-unsupported
distinction). `IOCTL_GET_DEVICE_INFO`'s `DEVICE_INFO_<PROTOCOL>_SUPPORTED`
answers (the six base families' own, plus the consolidated
`DEVICE_INFO_J2610_SUPPORTED`) now pack the same capacity into the
returned value's `QQ` byte (bits 16-23) instead of a flat `0x0000_0001`.

## `hw_protocol_id` `_PS`/`_CHx` Normalization (ADR-157)

Phase 2a's implementation of the Pin Selection section above stored a `_PS`
hardware protocol variant (e.g. `PROTOCOL_CAN_PS`) in
`LogicalLinkState::hw_protocol_id` without auditing every one of this
crate's ~30 pre-existing call sites that literal-compare `hw_protocol_id`
(or a `ChannelProtocol::from_raw` of it) against a base J2534-1 constant to
decide protocol-family behavior — every one of them silently stopped
recognizing a `_PS` id as belonging to its base family. ADR-157 classifies
every such read into one of three planes and fixes every Plane B site.
**Phase 2b (ADR-156 Decision 3 addendum) reuses this same funnel for
`_CHx`:** `resources::base_protocol_id` gained a range-based tier
(`chx_base_protocol_id`) consulted after its `_PS` match, so every Plane B
site below handles a `_CHx` link correctly for free, with no new sweep.

- **Plane A (hardware-facing)** — the real `PassThruConnect` argument and
  `PASSTHRU_MSG.ProtocolID` on read/write/filter messages — keeps the raw
  `_PS` id unchanged; these must match the literal id the channel was
  actually opened with.
- **Plane B (behavior/family decision)** — every ComParam-support check,
  filter-install-type decision, protocol-family gate (ISO15765-specific
  logic, J1850 header format, K-line-only IOCTLs, `select_init_sequence`,
  timing-parameter derivation, `GetResourceStatus`/lock-holder occupancy) —
  normalizes to the base protocol id first, via
  `resources::base_protocol_id(hw_protocol_id: u32) -> u32` (the exact
  inverse of `ps_protocol_id`, identity for every non-`_PS` value) when only
  a raw `u32` is in scope, or `LogicalLinkState::base_hw_protocol_id(&self)
  -> u32` (`self.base_hw_protocol_override.unwrap_or(self.hw_protocol_id)`)
  when a `LogicalLinkState`/`LinkView` reference is already in scope — the
  latter also preserves the exact SAE J2610 SCI variant, unlike the free
  function's single-representative collapse for `PROTOCOL_J2610_PS`, since
  `base_hw_protocol_override` is captured at `CreateComLogicalLink` time
  from `names::resolve_pin_selection`'s own already-correctly-resolved base
  id (ADR-157's Correction; the field is `None`, so this reads
  `hw_protocol_id` directly, for every non-`_PS` link).
- **Plane C (physical-resource identity)** — `ChannelKey`
  construction/lookup and `find_physical_lock_holder`-style
  channel-sharing/locking checks — keeps the raw `_PS` id unchanged, since
  `ChannelKey`'s `pin_select` component (ADR-156 Decision 2) already handles
  sharing/separation correctly on its own.

**Accepted residual, `_PS` links are hardware-ISO-TP only:** the CAN
dual-channel-mode auto-probe (`rpc_link.rs::rpc_connect_com_logical_link`,
gated additionally on `link_pin_select.is_none()`) and the software-ISO-TP
mode selection (`LogicalLinkState::software_isotp`, gated on
`pin_selection.is_none()` at `CreateComLogicalLink` construction time) are
both skipped for a `_PS` link — an `ISO15765_PS` link always uses hardware
ISO-TP, never the software-driven raw-CAN-channel path ADR-046 added.
Software-ISO-TP × Pin Selection interaction remains unimplemented; a caller
requesting Pin Selection on an ISO15765 link in a context that would
otherwise select software ISO-TP gets hardware ISO-TP instead, silently.

**Accepted residual (Codex review, PR #28): dual-channel-mode UUDT companion
capture is also skipped for a `_PS` link, for the same reason.**
`ConnectComLogicalLink`'s dual-channel-mode companion-open block
(`ensure_uudt_companion_channel`, immediately after the probe above) is now
also gated on `link_pin_select.is_none()` — `ensure_uudt_companion_channel`
always opens a plain (non-`_PS`) raw-CAN companion channel on the link's
default pins, discarding `pin_select` entirely, so a pin-selected
`ISO15765_PS`/`CAN_PS` link would otherwise get UUDT traffic captured on the
wrong physical pins. Full pin-aware companion-channel support (a `_PS`
companion opened on the caller's own pins, its own `SET_CONFIG`, its own
`ChannelKey` entry) is deliberately scoped out, same strategy as the
software-ISO-TP residual just above — a pin-selected link on a
dual-channel-capable adapter simply loses dual-channel-mode UUDT capture
(the connect itself still succeeds cleanly).

**`j2534-0404-mock`** needed its own small, crate-local inverse mapping
(`base_protocol_id`, positionally derived from the existing
`PS_PROTOCOL_IDS`/`BASE_PROTOCOL_IDS` arrays — no shared code across the
crate boundary) for its four confirmed-broken gates:
`IOCTL_GET_PROTOCOL_INFO`'s protocol-id validation (now answers based on the
base protocol's capability data instead of rejecting outright),
`IOCTL_FIVE_BAUD_INIT`/`IOCTL_FAST_INIT`'s K-line-protocol checks, and the
SAE J1850 bus-flavor simulation's response-queue match.

**Coordinator follow-up (same PR): the `ParamBinding::Temp` hardware
apply/revert bracket needed the identical fix on a THIRD code path, not just
`ConnectComLogicalLink`/`CoptUpdateparam`.** Any `StartComPrimitive` binding
a temp Working snapshot — `CoptSendrecv`'s `handle_send_recv` and
`CoptStartcomm`'s `handle_start_comm`, plus both call sites' shared
`revert_hardware_to_live_active` — pushes ComParams to hardware via
`apply_params_to_hardware`/`apply_params_to_hardware_locked`
(`to_j2534_config_id`/`expand_tidle`, both Plane B decisions) using a raw
`protocol_id` threaded from `TxItem::SendRecv`/`TxItem::StartComm` that is
also needed, unnormalized, for message building/RC-handling elsewhere in
the same variants — so it cannot simply be replaced. Following the
crate's existing `TxItem::SendRecv::logical_protocol` precedent (added for
the identical "need both the raw hardware id and a normalized identity"
reason), both `TxItem` variants (and `SendRecvCycle`/`StartCommParams`,
their poll-task-local counterparts) gained a sibling `base_protocol_id: u32`
field, captured via `LogicalLinkState::base_hw_protocol_id()` at the same
`StartComPrimitive` call time as the existing `protocol_id` capture, and
used ONLY at the temp-apply/revert call sites;
`revert_hardware_to_live_active`'s own single `protocol_id` parameter was
renamed to `base_protocol_id` outright (its only use is the hardware push,
so unlike the two `TxItem` variants it needs no sibling raw field). Do not
reuse `base_protocol_id` for any other purpose in these functions — message
building, filter ids, and `RcHandlingConfig`/`TimingChangeConfig` identity
in `handle_send_recv`/`handle_start_comm` all correctly keep using the raw
`protocol_id`/`logical_protocol`. `tests/grpc_mock/pin_selection.rs` pins
all three paths against an `ISO9141_PS`/`CAN_PS` link: `CP_TIdle` applies
during a `CoptSendrecv` temp bracket and survives past a revert that
doesn't touch it; `CP_TIdle` also correctly reverts back to Active when the
revert DOES need to touch it (via a `wait_for_config_value` poll helper,
since the revert runs asynchronously and not synchronously with the write
`wait_for_written_count` observes); and `CP_P1Max` applies-then-reverts
across a `CoptStartcomm` init transaction, using `set_config_count()`'s
exact `+2` delta (not just the final reverted value) as the load-bearing
assertion — `apply_params_to_hardware_locked` silently skips the native
`SET_CONFIG` call entirely when its protocol-filtered config list is empty,
which is exactly what an unnormalized raw `_PS` id produces, so checking
only the final value would false-pass a build where both the apply AND the
revert silently no-op.

**Independent-sweep fixes beyond ADR-157's own enumerated ~30-site list**
(same classification, found while implementing the ADR, not a deviation
from it): `rpc_link.rs::connect_flags`'s CAN-width/K-line match,
`rpc_primitive.rs`'s `is_kline` gate (which gated the ADR's own enumerated
`select_init_sequence` call site — normalizing only the callee argument
without also fixing this gate would have been a no-op fix), two additional
`RcHandlingConfig::from_params` sites (`rpc_primitive.rs`'s
`CoptStartcomm`/`CoptStopcomm` final-message resolution), and
`events.rs::handle_update_param`'s lock-conflict
`apply_params_to_hardware_locked` push.

**Two more Codex-found corrections (PR #28, after the fixes above had
already merged into the branch), both recorded in ADR-157's Correction
section:** (1) `LogicalLinkState::base_hw_protocol_id()`'s first
implementation derived a `_PS` link's base id via
`protocol.j2534_protocol_id()`, which collapses every SAE_J2610 SCI variant
to the shared `SCI_MODE` value instead of the connecting row's actual
`hw_protocol_override` — silently dropping `T1_MAX`-`T5_MAX` SCI timing
ComParams for a pin-selected SCI connect. Fixed by having
`names::resolve_pin_selection` return its own already-correct
`hw_protocol_override`-aware base id, captured into a new
`LogicalLinkState.base_hw_protocol_override` field at
`CreateComLogicalLink` time; `base_hw_protocol_id()` now reads that field
directly. The `GetResourceStatus`/resource-conflict candidate match
(`rpc_link.rs`, the `active_candidate` search comparing a resource row's
`hw_override` against `link.hw_protocol_id`) needed the same fix — this is
a *different* comparison shape (row-override-vs-link, not literal-family)
than ADR-157's original ~30-site sweep targeted, so it was missed until
this round; now uses `link.base_hw_protocol_id()`. (2)
`autodetect_sae_j1850_flavor`/`probe_sae_j1850_flavor` (the SAE_J1850
VPW/PWM probe, ADR-070) probed on the bus's *default* wiring even for a
pin-selected bus-agnostic J1850 link, since the probe never applied
`CONFIG_J1962_PINS` to its candidate channels — a bus reachable only via
the caller's selected (non-default) pins could be missed entirely, and
(having also silently reverted the `_PS` id, the first-round bug already
fixed above) opened the wrong flavor. Fixed by threading `pin_select` into
the probe: when set, candidates connect via their `_PS` id and apply
`CONFIG_J1962_PINS` before reading, and the module-wide `j1850_bus_flavor`
cache (a single global slot, not keyed per pin selection) is bypassed
entirely — never read from or written to — for a pin-selected probe, since
a different pin selection can mean a genuinely different physical bus
segment (the same principle that already required widening `ChannelKey`).

**Fixed (Codex review, PR #28):** a separate `wait.protocol_id`-based
`ChannelProtocol::from_raw` check gating `CP_EnableConcatenation` eligibility
(ADR-148, `events.rs::wait_for_expected_response` at line ~13238, keyed off
`is_kwp_family()`/`is_j1850_family()`) had the same Plane B shape as every
site this fix does correct, but was found during this fix's review, after
the fix's implementation rounds had already concluded, and was recorded as
an accepted residual in ADR-157's Consequences rather than reopening the fix
again at the time. A live PR #28 review round subsequently flagged it, and
it is now fixed: the gate normalizes via `resources::base_protocol_id`
before the `ChannelProtocol::from_raw` call, mirroring the same
normalization already applied to `rc_cfg`'s `RcHandlingConfig::from_params`
call a few lines above it in the same function. See ADR-157's Corrections
section (4th correction) for the full writeup.

## SAE J2534-2 CAN FD Core Connect Mechanics (ADR-158, Phase 3 Stage 3a)

Unlike every Phase 2 `_PS` family (ADR-156 Decision 2), clause 21 CAN FD has
no unqualified base `ProtocolID` at all (Table 89 defines only `_PS`/`_CHx`
variants), and ISO 22900-2 has no separate CAN-FD resource either — a D-PDU
client stages `CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate` on the ordinary
`CAN` resource, and `rpc_link::J2534Service::apply_fd_mode` infers FD mode
from those Working ComParams at `ConnectComLogicalLink` time, substituting
`link.hw_protocol_id`/`base_hw_protocol_override` to/from
`PROTOCOL_FD_CAN_PS`. It runs alongside `autodetect_sae_j1850_flavor`
(ADR-070) at the top of `rpc_connect_com_logical_link`, mirroring that
function's shape exactly (mutate under `logical_links`, then
`recompute_lock_tx_suspensions` in the SAME critical section per ADR-123
Finding G).

**Trigger rule:** `TX_DL > 8 || CP_CANFDBaudrate != 0`, read from Working
ComParams unconditionally (before the protocol-family branch below) since
`is_param_allowed`'s CAN-family gate (`comparam_support.rs`) allow-lists
these two ComParams for `ISO15765` too, not just `CAN` — only the
*handling* of a true trigger differs by family, not whether it's read.
Recomputed fresh on every connect, never sticky across a
disconnect/reconnect — the same "derived only from Working ComParams at
`PassThruConnect` time" pattern `connect_flags` (ADR-065) already uses.

**Family gate:** substitution to `PROTOCOL_FD_CAN_PS` only happens when
`resources::base_protocol_id(link.hw_protocol_id) == CAN`. A non-`CAN`
link (in practice, only `ISO15765` can even reach this with the trigger
true — see above) with the trigger true is **rejected**
(`Status::invalid_argument`), not silently ignored (ADR-158 correction,
Codex review PR #30) — `FD_ISO15765_PS`/clause 22 remains Stage 3b, not yet
implemented, but the absence of that support is now loud instead of
silent. A non-`CAN` link with the trigger false is an ordinary no-op,
unchanged from before.

**Rejections** (only checked while transitioning INTO FD mode on a `CAN`
link): module not opted into J2534-2 (clause 5); `link.software_isotp` (no
software-ISO-TP extension for FD-sized segmentation this phase). The
non-`CAN`-family rejection above is a separate, unconditional check (not
gated on "transitioning INTO FD mode" the way these are, since a
non-`CAN` link can never already be FD). **Update (ADR-213/Round 3):**
`channel_index.is_some()` is no longer a rejection reason — a `_CHx`-connected
link (`channel_index = Some(n)`) staging FD ComParams now promotes to
`FD_CAN_CH<n>` instead (`resources::fd_protocol_id_for_link`), and reverts
symmetrically back to `CAN_CH<n>` (not bare `CAN`) when the trigger stops
firing.

**`same_physical_resource`'s connected-state branch is rate-insensitive
too** (ADR-158 correction, Codex review PR #30 round 2): once both CLLs
being compared for `LOCK_PHYSICAL_COM_PARAMS`/`LOCK_PHYSICAL_TX_QUEUE`
purposes are connected, this predicate now compares only `hw_protocol_id`
and `pin_select` from each side's `ChannelKey`, ignoring `baud_rate` and
the FD data-phase rate — matching the pre-connect fallback branch's
already-rate-insensitive shape, so two `FD_CAN_PS` links on the same pins
but different data-phase rates (distinct physical channels by `ChannelKey`
design) are still correctly recognized as contending for the same J1962
wires for locking purposes.

**`CoptUpdateparam` rejects a promotion whose FD signal contradicts the
already-connected channel** (ADR-158 correction, Codex review PR #30 round
3): `CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate` have no native
`SET_CONFIG` mapping (`to_j2534_config_id`'s catch-all), and
`handle_update_param` (the `CoptUpdateparam` execution path) never calls
`apply_fd_mode`, so nothing would otherwise catch a live promotion whose
staged FD trigger disagrees with the CLL's connect-latched `hw_protocol_id`.
`rpc_start_com_primitive`'s `CoptUpdateparam` branch now compares
`fd_mode_staged(&params)` (the same trigger predicate `apply_fd_mode` uses,
factored into a shared free function so the two can never drift) against
`resources::is_fd_protocol_id(hw_protocol_id)` at call time, using the same
`update_param_working_snapshot` critical section that already reads
Working/UniqueRespIdTable together (now also returns the live raw
`hw_protocol_id`) — rejects with `Status::invalid_argument` in EITHER
direction (Classic-connected + FD staged, or FD-connected + Classic
staged). An FD-connected link promoting a different but still-nonzero
`CP_CANFDBaudrate` (a within-mode value change) still passes — the
connect-latched data-phase rate itself is a separate, still-accepted
residual (same class as `CP_Baudrate`/ADR-011), not closed by this check.

**FD-connected TX size validation, padding, and TX flags** (ADR-158
correction, Codex review PR #30 round 4): `resolve_send_recv_tx`
(`rpc_primitive.rs`) used to validate an FD-connected link's message
against `base_protocol_id`-normalized Classic CAN's fixed `4..=12` range,
rejecting every payload over 8 data bytes regardless of the link's staged
`CP_CANFDTxMaxDataLength` — CAN FD's whole purpose (up to 64-byte
payloads) was unreachable. `rpc_link::fd_can_tx_message_size_range` now
computes `4..=(4 + effective_tx_dl)` from the live staged
`CP_CANFDTxMaxDataLength` (floored at `8`, ISO 22900-2's own fallback for
an unset value), so the cap tracks a live `CoptUpdateparam` promotion
within FD mode rather than a fixed 64. A payload whose length isn't one of
SAE J2534-2 Table 91's DLC-encoded lengths is padded up to the nearest one
with `CP_CanFillerByte` (`comparam_id::fd_can_padded_data_len`, ISO
22900-2's `CP_CANFDTxMaxDataLength` NOTE 5), not rejected. Both
`resolve_send_recv_tx` and `resolve_tester_present` now also OR
`TX_FD_CAN_FORMAT` (newly re-exported from `j2534-0404`) into an
FD-connected link's TxFlags — SAE J2534-2 21.4.4 requires a conformant
module to reject any widened-range message whose FD_CAN_FORMAT flag is
unset, so the range fix alone was incomplete — plus `TX_FD_CAN_BRS` iff
the link staged a nonzero `CP_CANFDBaudrate`. Extended addressing is
ignored for the FD range, matching Classic CAN's existing behavior (Table
91 has no AE-byte variant). Separately, the same review round found
`names.rs` was missing the `CP_CANFDTxMaxDataLength` ParamName shortname
mapping (present for its three CAN-FD siblings but not this one) — a
mechanical omission, fixed alongside.

**Direct FD-id naming is rejected outright, unconditionally** (not
opt-in-gated, unlike `_PS`) — `names::resolve_pin_selection`'s
`resources::is_fd_protocol_id` check, run before `already_ps`/
`dlc_pin_data` handling, prevents `base_protocol_id`'s new FD arm from
silently normalizing a directly-named `PROTOCOL_FD_CAN_PS` down to a plain
`CAN` connect — the same bug shape ADR-156/ADR-157 repeatedly fixed for
`_PS`/`_CHx` direct naming, but here unconditional since clause 21 has no
legitimate direct-naming route at all.

**`to_j2534_config_id`'s contract changed from base-id-keyed to
Plane-A-keyed-with-an-FD-exception** (a narrow supersession of ADR-157's
"translation uses base" rule — see ADR-157's Status line and ADR-158's
Decision item 4) so `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` can be correctly
suppressed (clause 21.3.2.5.1's read-only rule) on an FD_CAN_PS link, a
distinction only visible on the raw id.

**Corrected (see ADR-158's Corrections section for the full writeup):** the
original Stage 3a diff's premise that three `events.rs` call sites could
switch to the raw id because `apply_params_to_hardware`/
`apply_params_to_hardware_locked` "normalized internally for `expand_tidle`"
was false — only `to_j2534_config_id` self-normalized at the time, silently
breaking `ISO9141_PS`/`ISO14230_PS`'s `CP_TIdle`→`W0`/`W5` derivation at
those 3 paths. The fix makes `expand_tidle` self-normalize too
(`comparam_id.rs`), so the ComParam-translation pipeline now has a single,
uniform contract: every caller (`apply_j2534_params` in `rpc_link.rs`,
`apply_params_to_hardware`/`apply_params_to_hardware_locked` in
`events.rs`) always passes the Plane A (raw) hw protocol id, and every
downstream consumer (`to_j2534_config_id`, `expand_tidle`) self-normalizes
internally — no caller ever needs to choose which id to pass, and no
caller-side base-id re-derivation is needed anywhere in this pipeline.
`revert_hardware_to_live_active`'s 13 call sites and the RC21/23
timing-change capture path were swept to this same raw-id contract as part
of the correction (previously passed the base id — see the now-resolved P3
backlog item this replaced). This is contract hygiene, not a fix to an
active bug: no `SET_CONFIG` carrying an FD-invalid param reaches real
hardware today only because `revert_hardware_to_live_active` unconditionally
strips every `BUSTYPE_UNUM32` key before pushing (ADR-110's existing
boundary) and both currently-relevant FD-read-only params happen to be
`BUSTYPE_UNUM32`-class — a coincidental, not structural, protection that
would silently stop covering a future FD-invalid param that isn't
`BUSTYPE_UNUM32`-class (plausible once Stage 3b/clause 22 lands).

**Sequencing fix:** `connect_new_physical_channel` issues
`SET_CONFIG(CONFIG_FD_CAN_DATA_PHASE_RATE)` (computed from
`CP_CANFDBaudrate` if nonzero, else `CP_Baudrate`) between the native
`PassThruConnect` call and the existing `CONFIG_J1962_PINS` step — a purely
conditional insertion (`NewPhysicalChannelParams.fd_data_phase_rate:
Option<u32>`, `None` for every non-FD connect), never an unconditional
reorder. The pins step itself now also fires for an *unqualified* FD
connect (no genuine Pin Selection): `resources::default_pin_select_for_base`
packs the base protocol's own default DLC pins for the physical connect
parameters (`ChannelKey`/`NewPhysicalChannelParams.pin_select`) only —
`link.pin_select` itself is never touched, so a later plain-CAN reconnect on
the same link does not incorrectly believe it still needs
`CONFIG_J1962_PINS`.

**Mock simulation** (`j2534-0404-mock`): `PROTOCOL_FD_CAN_PS` is its own
small, separate concept from `PS_PROTOCOL_IDS` (`is_fd_can_protocol`),
pin-gated like a `_PS` channel plus an additional ordering gate
(`SET_CONFIG(CONFIG_J1962_PINS)` fails `ERR_FAILED` until
`CONFIG_FD_CAN_DATA_PHASE_RATE` has already been set on that channel) and a
read-only-param gate (`SET_CONFIG(BIT_SAMPLE_POINT | SYNC_JUMP_WIDTH)` fails
`ERR_NOT_SUPPORTED`). A new per-channel `set_config_param_log`
(`ChannelState`, mirroring `connect_flags_log`'s len/entry accessor shape)
records every `SET_CONFIG` entry's `Parameter` id in call order — used by
`tests/grpc_mock/fd_can.rs::fd_can_connect_applies_data_phase_rate_before_pins`
to assert the rate-before-pins ordering directly. A new
`__mock_get_channel_protocol_id` accessor reads back the native
`PassThruConnect` `ProtocolID` a channel actually opened with.

**Structurally unreachable in Stage 3a only, since `apply_fd_mode` only
substituted a CAN-family link at the time** — `probe_can_channel_mode`'s and
`ensure_uudt_companion_channel`'s gates both require `base_proto_id ==
ISO15765`/`CanChannelMode::applies_to(protocol)` (ISO15765-keyed), so no
"any qualifier present" widening was needed for FD in Stage 3a. **This
stopped being true the moment Stage 3b (below) taught `apply_fd_mode` to
substitute ISO15765-family links too** — see that section's UUDT-companion
fix for the corrected, now-actually-reachable version of this path. This
paragraph is left here, corrected, rather than deleted, since it documents
exactly the assumption Stage 3b had to revisit (ADR-158's own Consequences
section flagged this as a known future gap before Stage 3b closed it).

See ADR-158 for the full design and Consequences (accepted residuals: `_CHx`
FD Additional Channels deferred, software-ISO-TP FD-sized segmentation
deferred, `revert_hardware_to_live_active`/RC21-23 raw-id threading
deferred).

## SAE J2534-2 ISO15765-on-CAN-FD Substitution and ComParam Mapping (ADR-159, Phase 3 Stage 3b)

Extends Stage 3a's CAN FD connect-time substitution mechanism (above) to the
ISO15765 family — SAE J2534-2 clause 22, `FD_ISO15765_PS`. Full design and
Consequences in [ADR-159](../../../docs/adr/ADR-159-j2534-2-iso15765-on-can-fd.md);
this section is the implementation-level summary.

**Substitution reuses `fd_mode_staged` unchanged.** `apply_fd_mode`'s
non-CAN-family branch, which previously rejected any FD trigger staged on a
link whose base protocol wasn't `CAN` (Stage 3a's PR #30 round-1
correction), is now a three-way family dispatch: `CAN` (unchanged),
`ISO15765` (new — mirrors the `CAN` branch's opt-in rejection and
substitute/revert logic, substituting to
`resources::fd_protocol_id(ISO15765)` = `PROTOCOL_FD_ISO15765_PS`; ADR-213/
Round 3 later made both branches' `channel_index` handling index-aware
promotion rather than a rejection, see that section below), and
every other family (still rejected outright). No new trigger predicate:
ISO 22900-2 has no clause-22 vocabulary of its own, so the same
`TX_DL > 8 || CP_CANFDBaudrate != 0` rule that drives clause-21 substitution
drives clause-22 substitution too. A `debug_assert!(!link.software_isotp)`
in the ISO15765 branch documents (rather than redundantly re-checks) that a
software-ISO-TP CLL's `hw_protocol_id` is always raw `CAN`, so it's already
caught by the `CAN` branch's own software-ISO-TP rejection and can never
reach the ISO15765 branch. `resources.rs`'s `fd_protocol_id`/
`is_fd_protocol_id`/`base_protocol_id` gain the mirror-image ISO15765 arm.

**`to_j2534_config_id` gains its first non-identity, protocol-conditional
translations.** Before this stage, every ComParam this function maps to a
native `SET_CONFIG` id maps to *itself* (identity) or to nothing (`None`) —
a fact the function's own doc comment relied on. On an `FD_ISO15765_PS`
link specifically (checked against the raw `hw_protocol_id`, same ordering
as the existing FD/`BIT_SAMPLE_POINT` check), three ComParams that are
either unsupported or only software-consumed everywhere else gain a
genuinely different native target:

| ComParam | Everywhere else | On `FD_ISO15765_PS` |
|---|---|---|
| `CP_CANFDTxMaxDataLength` | unsupported (pure trigger signal) | `CONFIG_FD_ISO15765_TX_DATA_LENGTH` (value floored at 8) |
| `CP_Cr` | unsupported natively (only feeds the software-ISO-TP `Reassembly` timeout) | `CONFIG_N_CR_MAX` (µs→ms via `us_to_ms`, clamped `1..=0xFFFF`) |
| `CP_CanFillerByte` | unsupported for native forwarding | `CONFIG_ISO15765_PAD_VALUE` (identity) |

`CP_CANFDBaudrate` is unchanged — `FD_CAN_DATA_PHASE_RATE` is still set once,
at connect time, by Stage 3a's existing `fd_data_phase_rate` connect step,
which clause 22 reuses unmodified. **Consequence for the round-3
`CoptUpdateparam` residual:** on an `FD_ISO15765_PS` link, a live
`CoptUpdateparam` promoting `CP_CANFDTxMaxDataLength`/`CP_Cr`/
`CP_CanFillerByte` now genuinely reaches hardware (they go through the
ordinary `apply_params_to_hardware` pipeline like any other native-mapped
ComParam) — this narrows, but does not close, the round-3 residual;
`CP_CANFDBaudrate` itself stays connect-latched.

**Message-size validation: a constant range, no service-side padding.**
`resolve_send_recv_tx`'s FD branch (Stage 3a round 4) now splits by base
family instead of treating every FD link identically: `FD_CAN_PS` keeps
Stage 3a's live-staged-TX_DL range and DLC-rounding padding unchanged (this
service constructs every raw CAN FD frame itself); `FD_ISO15765_PS` uses a
new constant `protocol::fd_iso15765_tx_message_size_range` (`4..=4128`
normal / `5..=4128` extended, SAE J2534-2 Table 98) with **no padding** —
padding individual ISO15765 frames is the native adapter's own ISO15765-2
duty on a hardware link, exactly as it already is for Classic hardware
ISO15765 today. Both FD cases keep the existing `TX_FD_CAN_FORMAT`
(+`TX_FD_CAN_BRS` iff a nonzero `CP_CANFDBaudrate` is staged) flags.
`resolve_tester_present` needed no change — its FD-flag logic was already
family-agnostic.

**Corrected (edge-case-hunter, verification pass on this same stage):** the
round-3 `CoptUpdateparam` FD-crossing-mismatch rejection's error-message
branch predates Stage 3b and picked its wording by `base_protocol_id(...) !=
CAN` — correct back when CAN was the only FD-supporting family, but once
ISO15765 became one too, that condition wrongly matched a genuinely-supported
ISO15765 mismatch and reported the stale "CAN FD is only supported on the
CAN family... clause 22 is a future stage" message for the very feature this
stage ships (the gRPC status code was still correctly `InvalidArgument`
throughout — this was a message-text bug only, not a behavior bug). Now
keyed on `matches!(base_protocol_id(...), CAN | ISO15765)` instead, matching
`apply_fd_mode`'s own family dispatch.

**UUDT-companion and probe machinery gain FD as a third disqualifier** (the
gap flagged above, and by ADR-158's own Consequences section). All three
gates that previously keyed on "base family ISO15765, no `pin_select`/
`channel_index`" — the two `ensure_uudt_companion_channel` call guards and
the `probe_can_channel_mode` guard — now also require `!is_fd_protocol_id`.
**Moved in lockstep** (per `install_point_to_point_fc_filters`'s own
documented hazard: letting its `qualified` argument drift from the
companion gates it mirrors reproduces a prior silent-UUDT-loss bug), all
three of that function's call sites' `qualified` computations gain a
matching `|| is_fd_protocol_id(...)` — so an FD-connected ISO15765 link now
correctly gets the point-to-point UUDT fallback filter instead of either a
wrongly-opened Classic-format companion channel or, worse, no UUDT capture
at all.

**Mock simulation** (`j2534-0404-mock`): `is_fd_can_protocol` widened to
`is_fd_protocol` (both `PROTOCOL_FD_CAN_PS` and `PROTOCOL_FD_ISO15765_PS`),
`base_protocol_id` gains the ISO15765 arm, and the existing rate-before-pins
`ERR_FAILED` gate and `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` `ERR_NOT_SUPPORTED`
gate both now cover either FD id — `PROTOCOL_FD_ISO15765_PS` reuses the
exact same `CONFIG_FD_CAN_DATA_PHASE_RATE` ordering gate Stage 3a built, not
a second mock-side mechanism.

**Verified, not implemented, per ADR-159's explicit scope boundary (at the
time):** `FD_ISO15765_CHx` (Additional Channels) was rejected by the
existing `_CHx`-region check (`PROTOCOL_FD_ISO15765_CH128` was already that
region's upper bound) and by `apply_fd_mode`'s existing `channel_index`
rejection — no new code needed then. **Update (ADR-213/Round 3):** this
deferral is now closed — `apply_fd_mode`'s `channel_index.is_some()`
rejection is replaced with index-aware promotion (a `_CHx`-connected
classic `ISO15765_CHx` link staging FD ComParams now promotes to
`FD_ISO15765_CHx`), and `chx_block_base`/`chx_base_protocol_id` gained a
real mapping for this family's block. Direct naming of `FD_ISO15765_CHx`
stays rejected, unchanged. Clause 8 (Mixed-Format CAN) needs zero groundwork here:
its device-level mixed-format requirements are native-adapter obligations
this service never implements frame construction for on a hardware
ISO15765 link.

**Deferred, recorded as backlog below:** `HS_CAN_TERMINATION` ComParam
forwarding (`CP_TerminationType` exists but is stored-not-forwarded for
every protocol today, and its non-`0`/`3` values have no native encoding);
`FD_ISO15765_PS` discovery-cache wiring (`DEVICE_INFO_FD_ISO15765_SUPPORTED`/
`_SIMULTANEOUS`/`_PS_J1962`, matching Stage 3a's own identical deferral,
ADR-153 precedent — **update, ADR-213/Round 3:** `DEVICE_INFO_FD_ISO15765_SUPPORTED`'s
own connect-path `DeviceFlag` check and `_CHx`-capacity dimension are now
wired (`connect_discovery_check`/`chx_device_info_supported_parameter`);
`_SIMULTANEOUS`/`_PS_J1962` remain deferred, matching every other family's
own residual); the ADR-055 functional-addressing Single-Frame limit
staying at Classic CAN's conservative `7`/`6` bytes on an FD-connected
ISO15765 link rather than widening for the larger frame capacity (rejects a
functional SF payload the frame size could actually carry — never silently
accepts an oversized one; physical addressing is unaffected); and a
pre-existing, unrelated unit bug found during this stage's investigation —
`ComParamSet::isotp_n_cr_timeout_ms`/`isotp_n_bs_timeout_ms` (software-ISO-TP's
own timeout resolution, ADR-046) read `CP_Cr`/`CP_N_Bs`'s µs-resolution
stored values and return them unconverted as if already milliseconds, a
~1000x timeout inflation for that unrelated emulation path — deliberately
NOT fixed in this PR since it would silently change unrelated ADR-046
behavior (fixed as its own standalone follow-up later, now out of the
backlog: both accessors now convert via the same `us.div_ceil(1000).max(1)`
pattern `p2_max_timeout_ms` already used).

New tests: `j2534-0404-service/tests/grpc_mock/fd_iso15765.rs` (substitution
and revert, rate-before-pins ordering, the three ComParam mapping/conversion
cases, TX size range up to 4128 with no padding, TX flags, `CoptUpdateparam`
FD-crossing rejection on ISO15765, direct-naming rejection, and the
UUDT-companion/fallback-filter fix). **ADR-213/Round 3 additions:** the
`_CHx` direct-naming-rejection test's own reason changed (from
out-of-scope-region to the FD-naming guard, now widened to `_CHx`) --
updated in place, not duplicated; new tests pin the `_CHx` core promotion
(`fd_mode_combined_with_a_directly_named_iso15765_chx_id_promotes_to_fd_iso15765_chx`),
its symmetric reversion, the `_CHx`-specific capacity cap (via
`MockBackdoor::set_fd_iso15765_chx_capacity`), and that a `_CHx` FD-ISO15765
connect does not synthesize `CONFIG_J1962_PINS`.

## SAE J2534-2 Mixed-Format CAN (ADR-160, Phase 3 Stage 3c)

Brings SAE J2534-2 clause 8 ("Mixed Format Frames on a CAN Network") into
scope as a fourth explicit `CanChannelMode` variant, `NativeMixed`
(`"native-mixed"` / `"native-mix"` / `"mixed-format"` / `"mixed"`), alongside
the existing `SingleChannel`/`DualChannel`/`SoftwareIsoTp`/`Auto`. Full design
and Consequences in
[ADR-160](../../../docs/adr/ADR-160-j2534-2-mixed-format-can-native-mixed-mode.md);
this section is the implementation-level summary. Unlike clauses 21/22
(Stages 3a/3b), this feature is entirely orthogonal to CAN FD — it is a
Classic-CAN mechanism, and an FD-substituted link (`FD_ISO15765_PS`,
ADR-159) is excluded from it outright.

**Why a `CanChannelMode` variant, not a client-facing IOCTL surface.** No
D-PDU client can ever call `SET_CONFIG(CAN_MIXED_FORMAT)` directly — ISO
22900-2's `PDU_IOCTL_*` enum has no clause-8 vocabulary, and ISO15765
channels already reject every client-requested filter type outright
(`ioctl_start_msg_filter`, `PDU_ERR_VALUE_NOT_SUPPORTED`, ADR-038). The
feature is therefore implemented the same way ADR-046's `dual-channel`
mode and ADR-041's `FLOW_CONTROL_FILTER` workaround already are: entirely
service-internal, selected by the module's `can_channel_mode` config key.
This directly targets the device-capability gap ADR-046/047's own
`CanChannelMode` machinery exists to work around — a device that actually
implements clause 8 no longer needs either the dual-channel companion
channel or the ADR-041 workaround for UUDT reception.

**Connect-time `SET_CONFIG`, creator-only, mirrors Stage 3a's
`fd_data_phase_rate` step exactly.** `connect_new_physical_channel` issues
`SET_CONFIG(CONFIG_CAN_MIXED_FORMAT, CAN_MIXED_FORMAT_ON)` immediately after
`PassThruConnect`, only when the `native_mixed_format` value passed into
`NewPhysicalChannelParams` is true. That value is `base_proto_id ==
ISO15765 && !is_fd_protocol_id(...) && effective_can_channel_mode() ==
NativeMixed && link_pin_select.is_none() && link_channel_index.is_none()`
— the base three-way computation lives in `rpc_connect_com_logical_link`,
strictly before `device_id`/`api` are locked (the same AB-BA-deadlock-safe
timing `dual_channel`'s own computation already established), and the
trailing qualified-link exclusion (`link_pin_select.is_none() &&
link_channel_index.is_none()`) is applied at the `NewPhysicalChannelParams`
construction call site, the same expression `install_point_to_point_fc_
filters`'s `PASS_FILTER` installer and both ADR-162 collision checks use.
So this `SET_CONFIG` only ever fires for an unqualified (non-pin-selected,
non-`_CHx`) ISO15765-family link — a qualified link's physical channel
never has `CAN_MIXED_FORMAT` turned on at all, and never gets this
`SET_CONFIG` call in any order. `ERR_NOT_SUPPORTED` disconnects the
just-opened channel and fails the connect outright — no silent fallback to
the ADR-041 workaround. Because this step only runs at physical-channel
*creation*, a second CLL joining an already-open native-mixed channel never
re-issues it — the same creator-only invariant Stage 3a's
`fd_data_phase_rate` step already relies on, not a new mechanism. An
earlier revision of this paragraph described the qualified-link case as
this `SET_CONFIG` running after `CONFIG_J1962_PINS` rather than immediately
after `PassThruConnect` — that description is now superseded; see the
"Per-UUDT-id `PASS_FILTER`..." paragraph below and ADR-160's "Correction
(2026-08-17)" and "Correction (2026-08-17, part 2)" sections for the full
history of why the `SET_CONFIG` moved from "runs after pins" to "doesn't
run at all" on a qualified link.

**Per-UUDT-id `PASS_FILTER` replaces the ADR-041 workaround, using the
clause 8.1 paired raw-CAN `ProtocolID`.** `install_point_to_point_fc_filters`
gains a `native_mixed` boolean (computed with the same timing/AB-BA-safety
as `dual_channel`, and mutually exclusive with it — a channel's
`CanChannelMode` resolves to exactly one variant). An unqualified link's
UUDT response id gets a `PASS_FILTER` instead of the `FLOW_CONTROL_FILTER`
fallback; a qualified (pin-selected/`_CHx`) link keeps the fallback
unchanged even under native-mixed mode. **ADR-160 Correction (2026-08-17,
part 2):** its own connect-time `SET_CONFIG(CAN_MIXED_FORMAT)` is now also
skipped entirely (not just the UUDT filter mechanism) — a Codex review on
the SET_CONFIG-ordering fix above found that firing it unconditionally on a
qualified link's channel exposed a clause-8 Figure-1 UUDT misparse/loss
hazard once that ordering fix let the connect actually succeed. **Round-1 fix
(edge-case-hunter, post-implementation audit):** the `PASS_FILTER`'s
mask/pattern messages must carry the raw-CAN-family `ProtocolID` paired
with the channel's own ISO15765-family hardware id per clause 8.1's
association rule (e.g. `CAN_PS` for `ISO15765_PS`), the *opposite* of the
literal-connect-id rule every other filter type follows (clause 8.2.2.4) —
the first implementation reused the literal connect id for both filter
types, which a conformant device would reject with `ERR_MSG_PROTOCOL_ID`,
silently defeating the feature entirely. Fixed via a new
`resources::mixed_format_can_protocol_id` helper (covers the base id, the
`_PS` variant, and every `_CHx` index) consulted only at the native-mixed
`PASS_FILTER` call site; every `FLOW_CONTROL_FILTER` call site (USDT, the
non-native-mixed UUDT fallback, and the qualified-link fallback under
native-mixed) is unaffected. The `CLEAR_MSG_FILTERS`/reset reinstall path
(`reinstall_iso15765_channel_filters_after_clear`, ADR-008/114) shares this
same function, so it correctly rebuilds the `PASS_FILTER` too.

**RX per-frame routing branches on native `ProtocolID`, no new
`RxEntryKind` variant.** `RxEntryKind::Hardware` gains a `native_mixed:
bool` field (resolved once per poll cycle from `effective_can_channel_mode
()`). Inside `process_frame_for_entry`, a native-mixed `Hardware` entry
whose frame's `base_protocol_id(frame_protocol_id) != ISO15765` (i.e. a
CAN-tagged, `PASS_FILTER`-matched frame) routes exactly like a
dual-channel companion channel — UUDT-id-match-only, via
`route_frame_uudt_only`; an ISO15765-tagged frame on the same channel falls
through to the ordinary path unchanged. Provably a no-op for the three
pre-existing modes, whose frames always carry their channel's own
connect-time `ProtocolID` (`native_mixed` is `false` for all of them, so
the new guard never fires).

**Mock simulation** (`j2534-0404-mock`): per-channel `can_mixed_format`
state, `SET_CONFIG`/`GET_CONFIG` simulation gated to ISO15765-family
channels, Table 5 side effects on a value change (clear RX/TX queues, drop
PASS/BLOCK filters — periodic-message deletion is inapplicable, see
Deferred below), and an `__mock_set_can_mixed_format_unsupported` backdoor
for the `ERR_NOT_SUPPORTED` rollback test.

**Deferred, recorded as backlog below:** TX-side `ProtocolID` selection (a
client sending a raw CAN-tagged message on a native-mixed channel — this
stage is RX-only); auto-detection of device clause-8 support (no
probe-and-fall-back the way `Auto` mode probes dual-channel capability —
`Auto` never resolves to `NativeMixed`/`NativeMixedAllFrames`);
discovery-cache wiring for `CAN_MIXED_FORMAT_SUPPORTED` advertisement
(matching every prior stage's identical deferral, ADR-153 precedent); and
native-mixed mode's own interaction with an FD-substituted link
(`FD_ISO15765_PS`) — clause 22.2.2.d requires `CAN_MIXED_FORMAT` support
there too, but this stage excludes FD links from either native-mixed
sub-mode entirely, keeping ADR-159's existing `FLOW_CONTROL_FILTER`
fallback for them regardless of the module's `can_channel_mode` setting.
(`CAN_MIXED_FORMAT_ALL_FRAMES` itself — parallel raw-CAN delivery of
USDT/flow-control frames alongside their ISO15765 processing — was closed
by ADR-217, which added `CanChannelMode::NativeMixedAllFrames`.)

**Test-coverage residuals, accepted (edge-case-hunter, post-implementation
audit):** no test connects two CLLs sharing one native-mixed channel to
assert `SET_CONFIG(CONFIG_CAN_MIXED_FORMAT)` fires exactly once (the
mechanism is structurally creator-only — this stage's connect step reuses
Stage 3a's already-proven `fd_data_phase_rate` creator-only pattern
verbatim, not a new invariant needing its own regression test); and
`native_mixed_mode_excludes_fd_substituted_link` asserts only that
`SET_CONFIG` never fires for an FD-substituted link, not that its UUDT id
still gets ADR-159's `FLOW_CONTROL_FILTER` fallback specifically (this is
exercised indirectly by
`native_mixed_mode_qualified_link_still_uses_fc_filter_fallback`'s
identical fallback mechanism, and is structurally guaranteed since an
FD-substituted link's `native_mixed` boolean is unconditionally `false`).

New tests: `j2534-0404-service/tests/grpc_mock/mixed_format_can.rs` (10
tests — connect-time `SET_CONFIG` assertion, `ERR_NOT_SUPPORTED`
connect-failure rollback, `PASS_FILTER` installed with the correct paired
raw-CAN `ProtocolID` for a native-mixed UUDT id, a qualified/pin-selected
link keeping the `FLOW_CONTROL_FILTER` fallback with its own literal
connect-time `ProtocolID`, CAN-tagged and ISO15765-tagged frame routing,
the other three modes never calling `SET_CONFIG`, `CLEAR_MSG_FILTERS`
reinstalling the `PASS_FILTER` correctly, and FD-substituted-link
exclusion).

## SAE J2534-2 Single Wire CAN (ADR-164, Phase 4)

Brings SAE J2534-2 clause 9 (Single Wire CAN / SWCAN / GMLAN) into scope,
reversing the specific `CP_ChangeSpeed*` rejection ADR-017 put in place.
Full design and Consequences in
[ADR-164](../../../docs/adr/ADR-164-j2534-2-single-wire-can-phase4.md); this
section is the implementation-level summary. Clause 9 defines no
unqualified base SWCAN protocol id at all (only `SW_CAN_PS`/
`SW_ISO15765_PS`), the same shape CAN FD has, but unlike CAN FD, ISO
22900-2 Annex G treats SWCAN as its own independently client-selectable
resource, not a ComParam-inferred substitution — so this phase's shape is a
hybrid of the Pin Selection (own resource-table rows) and CAN FD (`_PS`-only
protocol ids) precedents; see ADR-164's Context section for the full
reasoning neither precedent alone covers.

**Resource-table additions (`resources.rs`).** Ten new rows, `0x0226`-
`0x022F`, mirroring every existing row on `BUSTYPE_ISO_11898_2_DWCAN`
(`0x0201`-`0x020A`, including `0x0209`'s alias relationship, reproduced as
`0x022E` aliasing `0x022A`) on a new `BUSTYPE_SAE_J2411_SWCAN` (`0x0309`),
single default pin `PINS_SAE_J2411_SWCAN = [(1, PIN_HI)]` (clause 9.2.1).
Each row reuses its dual-wire sibling's own `ChannelProtocol` unchanged
(same D-PDU application-layer identity, different bus type) but gets its
own `protocol_name` (`_SWCAN` suffix) — a correction from an earlier draft
that reused the sibling's exact name, which would have made
`find_table_row_by_name` treat every one of the ten pre-existing dual-wire
names as ambiguous for a caller not supplying `dlc_pin_data` (three
pre-existing tests caught this). `hw_protocol_override` is the `_PS` id
directly (`PROTOCOL_SW_CAN_PS` for the raw-CAN row, `PROTOCOL_SW_ISO15765_PS`
for the other nine) rather than a native base id, since clause 9 has no
base id to override *from*. New `is_sw_protocol_id()` funnel and
`base_protocol_id()` arms (`PROTOCOL_SW_CAN_PS → CAN`,
`PROTOCOL_SW_ISO15765_PS → ISO15765`) let every existing Plane B call site
treat an SW link as its base family, the same pattern ADR-157 established
for every other `_PS`/`_CHx` id.

**Connect-time model (`names.rs`, `rpc_link.rs`).** `resolve_pin_selection`
gains an early SW arm (positioned like the existing FD guard): an SW id
requires the connecting module to have opted into J2534-2 (rejected
`invalid_argument` otherwise), then always returns a real `pin_select`
triple — never the `Ok(None)` short-circuit an ordinary base connect gets —
using the caller's `dlc_pin_data` via the existing `compute_pin_select`
(single-pin, already proven safe by `J1850VPW`'s own row) or the row's own
default pin (`0x0000_0100`) when none is supplied. This always issues an
explicit `SET_CONFIG(CONFIG_J1962_PINS)` at connect, matching clause
9.2.1's requirement that the physical layer stay unassigned until the pins are set
regardless of whether the caller customized them. `ps_protocol_id()`/
`is_ps_protocol_id()` and `default_pin_select_for_base()` (Precedent B's
two-pin substitution contract) are deliberately NOT reused — SW has no base
id to canonicalize to and no ComParam-inferred substitution, so neither
mechanism's invariants apply. `resolve_channel_selection` also rejects a
directly-named SW `_CHx` id outright (clause-24 region check), and
`apply_fd_mode` (`rpc_link.rs`) gained an explicit reject arm for staged FD
ComParams on an SW link — both mirror the existing `software_isotp`/
`channel_index` rejection shapes already there, rather than silently
mis-substituting.

**ComParam scope (`comparam_support.rs`, `comparam_id.rs`).** All five
`CP_ChangeSpeed*` ComParams (`Ctrl`/`Msg`/`Rate`/`ResCtrl`/`TxDelay`) plus
`CP_SwCan_HighVoltage` are allowlisted family-wide on CAN
(`is_can_param`) — not just on an SW link — mirroring the existing
`CP_P3Func`/`CP_P3Phys` "seeded but previously unreachable" precedent, so a
conformant SWCAN MDF's full `SetComParam`/`GetComParam` round-trip doesn't
break on the 2 unmapped members. `to_j2534_config_id()` gains three real
translations gated on `is_sw_protocol_id(hw_protocol_id)` (checked against
the raw, un-normalized id, since an SW row's own `hw_protocol_override` IS
the `_PS` id — no `base_protocol_id` normalization step to place this
before, unlike the FD checks above it in the same function):
`CP_ChangeSpeedRate → CONFIG_SW_CAN_HS_DATA_RATE`, `CP_ChangeSpeedCtrl →
CONFIG_SW_CAN_SPEEDCHANGE_ENABLE`, `CP_ChangeSpeedResCtrl →
CONFIG_SW_CAN_RES_SWITCH` (ISO 22900-2:2009 Annex A.1.2 Table A.3).
`CP_ChangeSpeedMsg`/`TxDelay` have no documented native mapping at all
(Table A.3 omits them, and clause 9's own automatic-detection model has no
configurable "message pattern"/"delay" `SET_CONFIG` param) and remain
accepted-but-unmapped. `CP_SwCan_HighVoltage` is not a `SET_CONFIG` param
at all — `ComParamSet::sw_can_tx_flags()` (cloned from ADR-062's
`sci_tx_flags()`) ORs `TX_FLAG_SW_CAN_HV_TX` into the same
`apply_resolved_tx_flags` path (`rpc_primitive.rs`), gated on the link's SW
hardware id so a dual-wire CAN link with this family-wide-allowlisted
ComParam set nonzero never has it consulted. See
`j2534-0404-service/docs/comparam-protocol-support.md`'s COM/Timing section
footnotes for the full per-param support-matrix detail.

**IOCTL exposure (`service_params.rs`, `names.rs`, `rpc_misc.rs`).**
`SW_CAN_HS`/`SW_CAN_NS` are `ChannelID`-scoped per clause 9's own text,
which already matches ADR-079's `L` (per-CLL) convention — added as
`PDU_IOCTL_SW_CAN_HS`/`PDU_IOCTL_SW_CAN_NS` (`PDU_IOCTL_BASE + 0x12`/
`+ 0x13`, the next two unused offsets after the existing 17 commands),
`map_ioctl_name` entries, and two new `resolve_object_id`/`rpc_io_ctl`
handlers (`ioctl_sw_can_hs`/`_ns`, sharing an `ioctl_sw_can_mode` body)
resolving `cll_handle → channel_id → PassThruIoctl(IOCTL_SW_CAN_HS/NS)`.
Rejected (`PDU_ERR_ID_NOT_SUPPORTED`) on a non-SW link. Gated on
`SharedChannel::ref_count == 1` (decided at implementation time, not
pre-specified by the ADR): mirrors `PDU_IOCTL_CLEAR_TX_QUEUE`'s existing
shared-channel no-op precedent — a shared SWCAN bus has only one wire, so a
per-CLL mode switch cannot be scoped narrower than the whole channel; a
`ref_count > 1` call is skipped (`Ok(())`, no native call) rather than
silently affecting every sibling CLL. No new RPC — rides the existing
`IoCtl` RPC per ADR-152 Decision 2's mechanism, extended one scope letter.

**Live-link resource-status attribution fix (`resources.rs`,
`rpc_link.rs`).** ADR-164's Context section anticipated, but did not
itself resolve, a live-link misattribution risk: `find_resource_id_for_protocol`'s
table-order tie-break takes no `hw_protocol_override` input, so it cannot
by itself distinguish a connected SW row (e.g. `0x0226`) from its dual-wire
sibling (`0x0201`) when both share one `ChannelProtocol` — safe only at
that function's two existing fallback-only call sites (no live-link state
involved), but `rpc_get_resource_status`'s own `matches_status_hw_id`
closure needed a genuine fix: it now checks a candidate link's *raw*
`hw_protocol_id` in addition to its normalized `base_hw_protocol_id()` (an
SW link's own connect id IS the qualified `_PS` id a table row's
`hw_protocol_override` names directly, unlike the SCI override case
`base_hw_protocol_id()` alone already handled), so `GetResourceStatus`
correctly attributes occupancy to the queried SW resource rather than
falsely reporting its dual-wire sibling's status.

**Backlog entries filed, not fixed this phase:** the RxStatus SW-mode-
transition bits (`SW_CAN_HV_RX`/`SW_CAN_HS_RX`/`SW_CAN_NS_RX`) are deferred
as a genuine RxFlag-byte extension, not a mechanical bit-table addition
(ADR-098's existing forwarding mechanism only carries `RxStatus`'s 5 low
bits). **Update (ADR-191):** `SW_CAN_HV_RX` (bit 16) is now forwarded into
`RxFlag` byte 1 bit 0; `SW_CAN_HS_RX`/`_NS_RX` (bits 17/18) remain deferred
— see "Prioritized Backlog" below for the narrowed item. SW Additional
Channels (`_CHx`) are out of scope this phase, matching every other
family's `_CHx` deferral (ADR-156 precedent).

**Mock support and integration tests are tracked separately** (a parallel
change, not part of this doc update) — `j2534-0404-mock` and
`j2534-0404-service/tests/` are unmodified by this section.

## SAE J2534-2 Repeat Messaging (ADR-165, Phase 12)

Brings SAE J2534-2 clause 14 (Repeat Messaging) into scope: a client hands
the interface a repeat-message setup (interval, stop `Condition`, and up to
3 `PASSTHRU_MSG` slots) and the interface autonomously retransmits it until
a stop condition is met. Full design and Consequences in
[ADR-165](../../../docs/adr/ADR-165-j2534-2-repeat-messaging-phase12.md); this
section is the implementation-level summary. Unlike every other J2534-2
phase brought into scope so far (Pin Selection, CAN FD, Mixed-Format CAN,
SWCAN), clause 14 assigns the autonomous retransmission, interval timing,
and mask/pattern evaluation entirely to the *interface* — this service is
purely a thin `IoCtl` forwarder here, the same position `ioctl_sw_can_hs`
already occupies for ADR-164, not a reimplementer of device-internal
behavior.

**IOCTL exposure (`service_params.rs`, `names.rs`, `rpc_misc.rs`).**
`START`/`QUERY`/`STOP_REPEAT_MESSAGE` are `ChannelID`-scoped per clause 14,
matching the existing `L` (per-CLL) convention — added as
`PDU_IOCTL_START_REPEAT_MESSAGE`/`_QUERY_REPEAT_MESSAGE`/`_STOP_REPEAT_MESSAGE`
(`PDU_IOCTL_BASE + 0x14`/`+ 0x15`/`+ 0x16`, the next three unused offsets
after ADR-164's `+0x12`/`+0x13`), `map_ioctl_name` entries, and three new
`rpc_misc.rs` handlers (`ioctl_start_repeat_message`/`ioctl_query_repeat_
message`/`ioctl_stop_repeat_message`) resolving `cll_handle → channel_id →
PassThruIoctl(*_REPEAT_MESSAGE)`. Gated on the connecting module's SAE
J2534-2 opt-in (`"J2534-2:"` `pname` prefix, ADR-152 Decision 1) — checked
directly against the open device's `pname` (mirroring `apply_fd_mode`'s
identical gate in `rpc_link.rs`), since clause 14 applies channel-wide, not
per-protocol, so there is no protocol-family connect-time gate to piggyback
on the way SWCAN's IOCTLs lean on `is_sw_protocol_id`. Rejected with
`PDU_ERR_ID_NOT_SUPPORTED` (`Code::InvalidArgument`, distinct from
`SW_CAN_HS`/`_NS`'s `Code::Unimplemented` mapping) on a non-opted-in
module. No new RPC — rides the existing `IoCtl` RPC per ADR-152 Decision 2's
mechanism, extended three more scope-letter entries.

**Proto (`vci-service-interface/src/proto/service.proto`; byte layout
reverted to `IOBytearray` by ADR-178).**

**Revision note (ADR-178):** Phase 12 originally added a dedicated
`IORepeatMessageSetup` message (`time_interval`, `condition`,
`repeat_msg_data`, `mask_data`, `pattern_data`, `tx_flag_bits`) and
`DataItem.repeat_message_setup` oneof variant (tag 11) to carry `START`'s
structured input. ADR-178 removed that message entirely as part of freezing
`service.proto`'s interface: the same six fields now ride the pre-existing
`DataItem.bytearray_data` (`IOBytearray`) carrier as a hand-packed
little-endian byte payload (`u32 time_interval`, `u32 condition`, then
three length-prefixed byte spans — `u32 len` + that many bytes — for
`repeat_msg_data`/`mask_data`/`pattern_data`, then `u32 tx_flag_bits_count`
followed by that many `u32`s, each a `TxFlagBit` enum's raw wire value) —
`rpc_misc.rs`'s `unpack_repeat_message_setup`, feeding a local
`RepeatMessageSetup` struct with the exact same field names/types the
removed proto message had, so every line of the ~700-line
`ioctl_start_repeat_message` handler below the extraction point needed zero
changes. `START`'s returned `MsgId` and `QUERY`'s returned status both
still reuse the pre-existing `unum32_value` (tag 1) output, unaffected
either way. Everything below this note (IOCTL exposure, D-PDU-to-native
composition, mock state machine, MsgId ownership) describes mechanics
downstream of this decode step and is unaffected by the byte-layout
change.

`tx_flag_bits` (Codex review PR #42 round 7): pass-through TX flags (e.g.
ISO-TP frame padding) for the transmitted `RepeatMsgData[0]` message,
reusing the existing `TxFlagBit` enum and `rpc_primitive`'s per-bit mapping
(`tx_flag_bit_to_j2534`, factored out of `compute_j2534_tx_flags` so both
sites share it, unchanged by ADR-178) — never applied to the mask/pattern
messages.

**D-PDU-to-native composition (`tx_header.rs`, `rpc_misc.rs`).** The
service composes the native `RepeatMsgData[0]` frame from the CLL's own TX
addressing the same way an ordinary `CoptSendrecv` TX is built
(`tx_header::build_tx_message`), and — since D-PDU clients speak
payload-only bytes (ADR-051) while the device's mask/pattern evaluates raw
wire frames — prepends this CLL's own expected-response header/ID bytes
(`tx_header::response_header_bytes`) to the client-supplied `mask_data`/
`pattern_data`, scoping a repeat slot to the addressed ECU's own responses
(mirroring ADR-006/ADR-051's expected-response convention) rather than
exposing a raw full-frame mask/pattern surface. `response_header_bytes`
returns `(header_bytes, header_mask, tx_flags)`, not a plain all-ones-masked
prefix (Codex review PR #42 round 2 Finding B): a KWP/ISO14230 response in
the default (`0x80`) format carries a wire-level length byte
(`events.rs::kwp_header_and_payload_len`'s own parsing already documents
this) that this service cannot predict the value of, so that position gets
mask byte `0x00` (don't-care, this codebase's existing zero-mask
convention) instead of `0xFF` — every other header byte, and every other
protocol/format, still gets an exact-match `0xFF` mask. The CAN/ISO15765
branch's response-side `tx_flags` sets `ISO15765_ADDR_TYPE` only when the
protocol is actually `ISO15765` (Codex review PR #42 round 3 Finding D) —
this bit is an ISO15765-only extended-addressing indicator per SAE J2534-1
Table B.13 (the same rule `CanIdFormat::tx_flags` in `rpc_link.rs` already
enforces for filter messages, PR #32), so a raw-CAN link's extended UUDT
addressing still gets its address byte appended to the header but must not
set this flag — a conforming adapter may reject a raw-CAN template that
carries it. `response_header_bytes` is called unconditionally for both `Condition`
values (ADR-173, superseding the PR #42 round 7/8/17 carve-outs that gated
response-header resolution, mask/pattern length-equality validation, and
mask/pattern size-range validation on `setup.condition != 0`): those
carve-outs were built on ADR-165's own inverted paraphrase of clause
14.2.2.1, which claimed `Condition == 0` never has the device evaluate the
mask/pattern. The real pairing is the opposite — `Condition == 0`
(`REPEAT_MESSAGE_UNTIL_MATCH`) retransmits through silence and stops only
on a matching frame, `Condition == 1` (`REPEAT_MESSAGE_WHILE_MATCH`) stops
on the first non-matching frame or a silent interval — so both conditions
genuinely evaluate mask/pattern against incoming traffic, and a link with
no resolvable response header (e.g. raw CAN with only `CP_CanPhysReqId`
set, or functional addressing) is now rejected under `Condition == 0`
exactly as it already was under `Condition == 1`; this is an intentional,
client-visible behavior change (ADR-173 Decision 4), not a regression.
On the ISO9141/ISO14230 branch, `CP_PhysRespFormatPriorityType` is resolved
via the same entries-first-then-Active-set fallback chain `ecu_addr` already
uses for `CP_EcuRespSourceAddress` — checking the selected UniqueRespIdTable
entry's own params before falling back to the common Active ComParamSet
(Codex review, ADR-165 PR #42 round 11) — since it is a `PDU_PC_UNIQUE_ID`-
classified (per-ECU) ComParam per the SAE J2534-1 ComParam table, not a
common-set-only value; reading it from the Active set alone missed a
per-ECU override and could compose the mask/pattern (and the length-byte
gate above) against the wrong response format. The J1850 branch needed the
identical fallback chain (Codex review, ADR-165 PR #42 round 12) — the
`PDU_PC_UNIQUE_ID` classification applies per-ComParam, not per-protocol-
family, so J1850's own `CP_PhysRespFormatPriorityType` read was carrying
the exact same pre-round-11 bug; only the resolution source changed, the
protocol-specific `default_format` fallback (0x68/0x61) is unaffected.

**MsgId ownership and teardown (`service.rs`, `rpc_link.rs`).** The device
assigns `MsgId` at `START` time — `LogicalLinkState` gained a
`repeat_message_ids: Vec<u32>` for the owning CLL, populated on a
successful `START` and drained on `STOP`, placed alongside `client_filters`
as "hardware state this CLL owns, cleaned up the same way."
`require_owned_repeat_message` (`rpc_misc.rs`) validates a caller-supplied
`MsgId` against this set before `QUERY`/`STOP` ever reach the native call —
a `MsgId` never issued to this CLL, including one issued to a sibling CLL
sharing the same physical channel (the device has no notion of CLL
identity on a shared channel), is rejected `PDU_ERR_INVALID_MSG_ID`
(`Code::NotFound`) service-side. A `Condition == 1` slot that has already
self-completed device-side (no notification back to this service) is
pruned from `repeat_message_ids` the moment a `QUERY`/`STOP` on it comes
back `ERR_INVALID_MSG_ID` from the native call (Codex review PR #42 round 2
Finding C) — not just on a successful `STOP` — so a long-lived CLL that
starts many condition-1 messages never accumulates unbounded stale entries.
This pruning (and `QUERY`/`STOP`'s own successful-STOP removal) happens
*before* `shared_channels` is released, not after (Codex review PR #42
round 3 Finding G — the earlier ordering reopened the exact
resolve-state/native-call/record-state race the round-1 Finding-1 fix
closed for `START`). A successful `START` also revokes the returned
`MsgId` from every OTHER CLL sharing the same physical channel's tracking,
and from `SharedChannel::leaked_repeat_message_ids` (Codex review PR #42
round 3 Finding E) — the device handing out a `MsgId` is authoritative
proof it currently considers that number free, so any sibling CLL's stale,
unpruned claim on the same number (e.g. from a `Condition == 1` slot that
self-completed without that CLL ever issuing a `QUERY`/`STOP` on it) must
be revoked before it could otherwise let that sibling `STOP`/`QUERY` this
brand-new, unrelated slot. The same recording step also deduplicates a
stale claim on the reused `MsgId` that THIS SAME CLL still carries in its
own `repeat_message_ids` before pushing the new entry (Codex review,
ADR-165 PR #42 round 12) — round 3 Finding E's sibling-revocation loop
above only scans OTHER CLLs sharing the channel, missing the case where
this CLL's own earlier `Condition == 1` slot self-completed and left a
stale entry the device then reassigned back to this same CLL.
`retry_leaked_repeat_message_stops` applies the
same distinction to its own retry loop (Codex review PR #42 round 3
Finding F): a retry that comes back `ERR_INVALID_MSG_ID` means the device
already forgot the slot, so it is dropped from
`leaked_repeat_message_ids` rather than retained for another retry.
`DestroyComLogicalLink`/`DisconnectComLogicalLink` (`rpc_link.rs`) both best-effort `STOP` every
tracked slot in their existing client-filter-cleanup loops before the CLL
goes away. Unlike `client_filters` (ADR-082: non-empty always implies sole
channel ownership, so the imminent `PassThruDisconnect` cleans up any failed
stop regardless), a repeat slot has no such guarantee — the slot budget is
explicitly shared across sibling CLLs on one physical channel (ADR-165
Consequences). A failed `STOP` during teardown is only safe to just-log when
the channel is about to close (`ref_count` hitting 0 right after); otherwise
the `MsgId` is pushed into `SharedChannel::leaked_repeat_message_ids`
(Codex review PR #42 round 2 Finding A) and retried opportunistically the
next time any CLL touches `ioctl_start_repeat_message`/`ioctl_query_repeat_
message`/`ioctl_stop_repeat_message` on that channel
(`retry_leaked_repeat_message_stops`), with a final backstop right before
the channel's `SharedChannel` entry is removed on `ref_count` reaching 0
(at that point the imminent `PassThruDisconnect` tears the slot down too,
so any still-leaked id is simply dropped, not retried further).

**Mock support (`j2534-0404-mock`).** Implements the real clause-14 state
machine, since it stands in for the device this design forwards to: a
per-channel `repeat_slots` table (≥10 capacity,
`MAX_REPEAT_SLOTS_PER_CHANNEL`), a single unified background thread per
slot (`spawn_repeat_worker`) driving the interval timer for both
conditions (no existing shared tick-loop/async runtime in this crate to
piggyback on), and the short-frame-never-matches/beyond-pattern-length-
don't-care mask/pattern evaluation rule. Per ADR-173 (correcting an
inversion in ADR-165's original design): `Condition == 0`
(`REPEAT_MESSAGE_UNTIL_MATCH`) retransmits through silence and terminates
on the first *matching* received frame; `Condition == 1`
(`REPEAT_MESSAGE_WHILE_MATCH`) terminates on the first *non-matching*
received frame, or on an interval elapsing with zero eligible received
frames (silence) — two independent stop triggers, and the silence check
resets every interval (a match in interval N does not immunize interval
N+1). `RepeatSlot::terminated` replaces the old `matched` flag as the
terminal state (set by either trigger); a terminated slot is *retained*,
not removed, until an explicit `STOP_REPEAT_MESSAGE` (clause 14.2.2.3/
Table 53) — `QUERY_REPEAT_MESSAGE` reports `1` for a live slot, `0` for a
terminated-but-unstopped one, and `ERR_INVALID_MSG_ID` only once actually
removed by `STOP`. `RepeatSlot::rx_seen_this_interval` tracks whether any
eligible frame arrived during the current grid window, consulted only by
`Condition == 1`'s silence check; a frame already excluded by the
ineligibility gate below (including the worker's own transmit echo) counts
as neither a match, a non-match, nor "received" for this purpose. The
window timeline itself is grid-anchored, not reset at each transmit
(Codex review PR #60 round 3, superseding that PR's own round-1/round-2
patches, design-advisor consult): `RepeatSlot::interval_deadline` (an
`Option<Instant>`, `None` until the slot's first transmit) always holds
the end of the earliest window still owed a silence evaluation, and the
single `advance_repeat_slot_windows` routine — called by
`note_rx_frame_for_repeat_slots` on every eligible frame, by
`spawn_repeat_worker` at every wake, and by the `IOCTL_QUERY_REPEAT_MESSAGE`
handler before it derives its reported status (round 4 fix, closing a gap
where a QUERY landing before either of the other two next noticed an
elapsed deadline could report a stale, already-silent slot as still live) —
advances it by whole `TimeInterval` steps from wherever it already sits,
never by re-anchoring to `Instant::now()`. This keeps `Condition == 1`'s
silence stop-trigger exact even when a worker is delayed by more than one
full `TimeInterval` (e.g. a severe scheduler stall): every silent window
within the stall is still closed and evaluated, not silently skipped the
way re-anchoring to "now" at each transmit would skip it. A frame arriving
before the slot's first transmit (`interval_deadline` still `None`) credits
no window. Evaluation
now runs for every non-terminated slot
regardless of `condition` (previously gated to `condition == 1` only,
another artifact of the same inversion). The underlying byte/format/
protocol match computation itself is unchanged by ADR-173 — only which
condition treats a match vs. a non-match as its stop trigger flipped. A
match also requires the incoming frame's `TxFlags`, masked
to `TX_FLAG_CAN_29BIT_ID`/`TX_FLAG_ISO15765_ADDR_TYPE`, to agree with the
slot's own `response_tx_flags` (captured from `RepeatMsgData[1]`/`[2]` at
`START` time, both always identical per round 2 Finding 2) — a byte-level-
only comparison cannot distinguish an 11-bit from a 29-bit CAN ID (or
normal from extended ISO15765 addressing) when the numeric ID value
coincides, since `tx_header::can_header_bytes` always encodes it as the
same 4-byte big-endian `Data` prefix regardless of width (Codex review,
ADR-165 PR #42 round 9). A `Condition == 1` match likewise requires the
incoming frame's `ProtocolID` to equal the slot's own
`response_protocol_id` (also captured from `RepeatMsgData[1]`/`[2]` at
`START` time, exact equality, not masked) — a native-mixed CAN channel
(`CanChannelMode::NativeMixed`, ADR-160/162) can carry both raw-CAN and
ISO15765 frames on one physical channel distinguished only by each frame's
own `ProtocolID`, so neither the byte-level nor `TxFlags` check alone can
rule out a coincidentally-matching frame from the wrong protocol family
completing the wrong slot (Codex review, ADR-165 PR #42 round 10).
`START_REPEAT_MESSAGE` also now rejects `Condition` values outside `{0, 1}`
with `ERR_INVALID_IOCTL_VALUE` up front, before any slot is created — SAE
J2534-2 clause 14 only defines those two values, and an out-of-range value
was previously silently accepted into a slot that could never usefully
stop (it falls into the `Condition != 0` poll-once branch, but
`note_rx_frame_for_repeat_slots` never marks a non-`1` slot matched, so it
would just expire after one transmission once `time_interval_ms` elapsed,
unlike a conforming device) (Codex review, ADR-165 PR #42 round 10). The
`__mock_inject_rx_msg` back-door FFI export gained the `tx_flags` argument
above as a sixth parameter in round 9, directly on the existing symbol;
since consumers resolve this symbol dynamically (not type-checked against
this crate's definition), an external caller still using the five-argument
ABI documented in `j2534-0404-mock/docs/testing-guide.md` would read an
indeterminate sixth argument as `tx_flags`. Fixed by reverting
`__mock_inject_rx_msg` to its original five-argument signature (`TxFlags`
implicitly `0`) and exposing the six-argument form as a separate
`__mock_inject_rx_msg_with_flags` symbol; both now delegate to a shared
private `inject_rx_msg_ffi` helper (Codex review, ADR-165 PR #42 round 13).
Own inline unit tests cover this state machine directly (bypassing gRPC),
searchable via "Repeat Messaging" near the end of
`j2534-0404-mock/src/lib.rs`.

**Round 14 correction: the format check compared the wrong `PASSTHRU_MSG`
field.** Round 9's check above compared the incoming frame's `TxFlags` —
but `StoredMessage::write_to_passthru` always zeroes `TxFlags` for
RX-direction messages, so that comparison never actually fired as
intended; the CAN-ID-width/ISO15765-addressing-type bits a genuinely-
received frame reports live in `RxStatus` instead
(`RX_FLAG_CAN_29BIT_ID`/`RX_FLAG_ISO15765_ADDR_TYPE`), a different SAE
J2534-1 field for the opposite direction that merely happens to share the
same bit values (`0x100`/`0x80`) as their `TX_FLAG_*` counterparts. Fixed
by translating the slot's setup-sourced `TxFlags` into `RxStatus` space
once at slot-creation time (`tx_format_flags_to_rx_status_bits`, stored as
`RepeatSlot::response_format_rx_bits`) and switching the incoming-frame
comparison in `note_rx_frame_for_repeat_slots` to `RxStatus`. Also adds an
eligibility mask (`REPEAT_INELIGIBLE_RX_STATUS_MASK`) so a loopback echo of
the CLL's own transmission or another indication frame (TX-done,
start-of-message, break) — none of which are a message received from
another node, per SAE J2534-2 clause 14's own stop-condition scope — can
never satisfy a slot's stop condition, even if its `Data` bytes
coincidentally match; previously nothing prevented this; before this fix, a
loopback echo whose `TxFlags`/`ProtocolID`/`Data` happened to agree with a
slot's own criteria (a real possibility, since a write and its own loopback
echo share the same `TxFlags` and `ProtocolID`) could incorrectly complete
it. Removes the round-9/13 `tx_flags`-threading mechanism entirely as dead
weight: `__mock_inject_rx_msg_with_flags` (the separate six-argument
symbol round 13 introduced), the harness's `inject_rx_with_status_and_
flags`/`inject_rx_via_legacy_five_arg_symbol` methods, and
`mock_inject_rx_message`'s `tx_flags` parameter are all removed;
`__mock_inject_rx_msg`/`mock_inject_rx_message`/`inject_rx_with_status`
revert to their pre-round-9 signatures, now hardcoding `TxFlags` to `0` on
the resulting `StoredMessage` since no code path reads an RX-direction
frame's `TxFlags` anymore (Codex review, ADR-165 PR #42 round 14). The
governing rule for future readers: RX-direction wire-format facts live in
`RxStatus`, TX-direction in `TxFlags` — never compare one against a value
sourced from the other without translating at the boundary first.

**Round 15 (Codex review, ADR-165 PR #42 round 15, design-advisor consult).**
Round 15's finding claimed an FD-connected slot's mask/pattern (`RepeatMsgData[1]`/
`[2]`) couldn't distinguish classic-vs-FD responses and asked to compare FD
format bits in the mock's matcher — verified against SAE J2534-2 and found
partly wrong: clauses 21.2.2(g)/22.2.2(d) are normative rules that
FD-capable-channel filtering/matching ignore CAN message format entirely
(matching is on address+data only), so comparing FD format bits in the
matcher would itself violate the spec. The real, narrower gap: 21.4.4 lets a
device cap a `PASSTHRU_MSG`'s `DataSize` at 12 bytes when `TX_FD_CAN_FORMAT`
is unset, so `rpc_misc.rs`'s `ioctl_start_repeat_message` now ORs
`TX_FD_CAN_FORMAT` into `response_tx_flags` (both condition branches, since
21.4.4's validity concern applies regardless of `condition`) whenever
`fd_link` is true — for template DataSize validity only, never for
comparison. `TX_FD_CAN_BRS` stays deliberately unset on the mask/pattern:
Table 93 defines BRS as a bit-timing property within an FD frame, not a
format discriminator, with no equivalent validity coupling. Separately,
`j2534-0404-mock`'s `tx_format_flags_to_rx_status_bits` now also translates
`TX_FLAG_FD_CAN_FORMAT`/`TX_FLAG_FD_CAN_BRS` into `RX_FLAG_FD_CAN_FORMAT`/
`RX_FLAG_FD_CAN_BRS`, so a genuinely-received/echoed FD frame (e.g. the
`PassThruWriteMsgs` loopback-echo call site, which needed no code change
since it already calls this helper) reports honest RX-side FD bits. This
alone would have broken every FD-link `Condition == 1` slot: since
`rpc_misc.rs`'s round-15 change now sets `TX_FD_CAN_FORMAT` on the mask/
pattern's `TxFlags`, the newly-extended translation would put
`RX_FLAG_FD_CAN_FORMAT` into `RepeatSlot::response_format_rx_bits`, which
`note_rx_frame_for_repeat_slots` compares against the INCOMING frame's
`rx_status` masked to `REPEAT_RESPONSE_FORMAT_RX_STATUS_MASK` — a mask that
(correctly, per 21.2.2(g)/22.2.2(d)) excludes the FD bits, so the two sides
would never again be equal. Fixed by masking `response_format_rx_bits` to
`REPEAT_RESPONSE_FORMAT_RX_STATUS_MASK` at slot-creation time (the
`IOCTL_START_REPEAT_MESSAGE` handler), so the translation helper can grow to
cover more RX-echo-fidelity bits in the future without ever leaking one into
the comparison. A new `RepeatSlot::mask_pattern_tx_flags` field plus
`__mock_get_repeat_slot_mask_pattern_tx_flags`/`MockBackdoor::
repeat_slot_mask_pattern_tx_flags` (test-introspection only, mirroring
`written_tx_flags`'s shape) let a test directly confirm the mask/pattern's
raw, unmasked native `TxFlags`, since no other accessor exposed it. Two
gaps design-advisor flagged as out of scope for this fix at the time —
`install_client_message_filters`'s own mask/pattern composition
(`rpc_link.rs`) not getting the same `TX_FD_CAN_FORMAT` treatment, and
whether `ioctl_start_repeat_message`'s round-4/5 `FD_CAN_PS` repeat-message
padding logic was itself conformant with SAE J2534-2 14.2.1's
periodic-message DataSize limits — are both since fixed by ADR-186 (see its
own note near the end of this section).

**Round 16 (Codex review, ADR-165 PR #42 round 16).** `tx_header.rs`'s
`response_header_bytes` ISO9141/ISO14230 branch was unconditionally
appending the 2 tester/ECU address bytes to the composed response header,
regardless of the response format byte's own addressing bit. The RX-side
parser this composition is supposed to mirror,
`events.rs::kwp_header_and_payload_len`, only expects those 2 bytes on the
wire when bit 7 of the format byte (`has_addr`) is set — for an unaddressed
response format (bit 7 clear, e.g. `0x01`), the real wire header is just the
format byte (plus the pre-existing, unrelated embedded-length-byte
condition just below it). Composing a 3-byte header against a 1-byte real
frame shifted the mask/pattern by 2 bytes, so a valid unaddressed
ISO9141/ISO14230 response could never satisfy a `Condition == 1` repeat
slot's stop condition. Fixed by gating the 2 address-byte pushes on
`format & 0x80 != 0`, the identical bit and condition
`kwp_header_and_payload_len` already uses; the mask's length is unaffected
since it is already derived from `header.len()`.

**Round 17 (Codex review, ADR-165 PR #42 round 17).** The round-6 general TX
message size-range check only validated `full_message` (`RepeatMsgData[0]`,
the actually-transmitted message) — the composed mask/pattern
(`RepeatMsgData[1]`/`[2]`, `mask_data`/`pattern_data` in `rpc_misc.rs`, built
by prepending the response header to the client's own
`setup.mask_data`/`setup.pattern_data`) was never checked against the
protocol's real TX message size range at all; `PassThruMessage::new` only
enforces the struct-wide capacity, not this per-protocol range. An oversized
client-supplied `mask_data`/`pattern_data` (e.g. exceeding classic CAN's
4-byte-header-plus-8-byte-payload `4..=12` total) was therefore silently
accepted. Fixed by validating both composed templates against the same
size-range logic the round-6 check used at the time (`fd_base_family`-dispatched
`fd_can_tx_message_size_range`/`fd_iso15765_tx_message_size_range`/
`ChannelProtocol::tx_message_size_range`) -- **ADR-186 note:** the round-6
check itself was later replaced by a periodic-message cap for
`full_message`'s own check (see this section's ADR-186 note near the end),
but the mask/pattern check this round-17 fix added is deliberately
untouched by that change and still uses this same ordinary-TX-range logic
today, since mask/pattern templates are exempt from the periodic cap. This
check is gated on `setup.condition != 0`
(mirroring this function's own established `condition == 0` mask/pattern
carve-out) — but derived from the mask/pattern's own RESPONSE-side
extended-addressing basis (`response_tx_flags & j2534_0404::
ISO15765_ADDR_TYPE`), not the request-side `extended_addressing` value
`full_message`'s own check uses, since this PR has already established
(rounds 2-3) that request and response addressing can genuinely differ for
ISO15765.

**Round 18 (Codex review, ADR-165 PR #42 round 18).** Round 17's mask/
pattern size-range check used `fd_can_tx_message_size_range` for an
FD_CAN_PS link's `Some(j2534_0404::CAN)` arm — at the time, the same
function `full_message`'s own check (correctly) used too. (**ADR-186
note:** `full_message`'s own check no longer uses this function at all —
it now uses ADR-186's periodic-message cap instead — but the mask/pattern
check this round-18 fix corrects is unaffected and still uses
`fd_can_tx_message_size_range` today, for the reason this entry gives.)
That function derives its
upper bound from `CP_CANFDTxMaxDataLength`, the tester's own declared max
TX length (ISO 15765-2's TX_DL). But `mask_data`/`pattern_data` are never
transmitted — they are a comparison template evaluated against a received
ECU response — and an ECU's RX capability on a CAN FD bus is independent of
the tester's own configured TX_DL, so round 17's check could wrongly reject
a template long enough to match a longer, entirely valid ECU response, even
though round 15 already sets `TX_FD_CAN_FORMAT` on this exact template
specifically to make it valid up to the full CAN FD frame limit. Fixed by
using the CAN FD frame's structural maximum payload
(`CANFD_TX_MAX_DATA_LENGTH_ACCEPTED`'s max, 64 bytes) for this specific
check's `Some(j2534_0404::CAN)` arm instead of the staged
`CP_CANFDTxMaxDataLength`; the `full_message` check above it, and the
`Some(j2534_0404::ISO15765)`/fallback arms of this same check, are
unaffected. Neither of those arms shares the bug: `Some(j2534_0404::
ISO15765)` calls `fd_iso15765_tx_message_size_range` (a fixed structural
range per SAE J2534-2 clause 22/Table 98, not ComParam/TX_DL-dependent),
while the fallback arm calls `ChannelProtocol::tx_message_size_range`, the
ordinary SAE J2534-1 per-protocol table (covers J1850PWM/VPW, ISO9141,
ISO14230, SCI) -- unrelated to Table 98.

**Round 19 (Codex review, ADR-165 PR #42 round 19; corrected in a same-round
follow-up after direct spec verification).** `response_header_bytes`'s
CAN/ISO15765 branch read the response ID via a plain `unum32` lookup for
both the raw-CAN (`CP_CanRespUUDTId`) and ISO15765 (`CP_CanRespUSDTId`)
cases, treating any present value as a real response address. But ISO
22900-2 Table 76 documents `0xFFFFFFFF` as the "not used" sentinel for
BOTH `CP_CanRespUUDTId` and, identically (two entries later in the same
table), `CP_CanRespUSDTId` -- confirmed against both available spec
editions in the sibling `vehicle-comm-specs` repository. That value can be
present in an unedited/default UniqueRespIdTable entry (confirmed by
several raw-CAN presets in `comparam_defaults.rs` that seed the UUDT
ComParam to exactly `0xFFFFFFFF`) even though the field was never actually
configured -- so a `Condition == 1` repeat request against a default
UniqueRespIdTable could silently accept `0xFFFFFFFF` as a real CAN ID and
build a stop-condition mask/pattern template around an invalid identifier.
Fixed by routing both cases through `rpc_link::uudt_resp_id`/
`rpc_link::usdt_resp_id` (the latter added alongside it, both
`pub(super)`), the helpers this crate uses to filter this exact sentinel,
instead of duplicating the check; the same "not set" error this function
already returns for an absent ComParam is now also returned for the
sentinel value on both branches. Every ISO15765 preset in
`comparam_defaults.rs` happens to seed `CP_CanRespUSDTId` with a real
response ID (`0x7E8`), never `0xFFFFFFFF` -- but that is a
default-authoring convention only, not something `SetUniqueRespIdTable`'s
own validation (`rpc_misc.rs`) enforces, so a client can still legitimately
stage the sentinel there, and the USDT path must filter it exactly like
the UUDT path does.

**Round 20 (Codex review, ADR-165 PR #42 round 20; three findings — two
against `j2534-0404-mock/src/lib.rs`, discussed here, and a third against
`tx_header.rs`'s shared KWP/ISO14230 header composition, design-advisor-
escalated and written up as its own ADR since the fix revises ADR-050's
model rather than being Repeat-Messaging-specific — see "Round 20's third
finding" below and ADR-166).** Finding 1: `repeat_mask_pattern_matches`'s
round-14 `.max()`-of-lengths fix (see above) was itself insufficient to
reject a mismatched mask/pattern length pair -- it only routed the
mismatch through the `data.len() < cmp_len` short-frame rejection, which
does nothing when `data` is long enough to cover `cmp_len`: the shorter
array's missing tail bytes default to `0` via `.unwrap_or(0)` in the
compare loop, so a `data` byte that happens to be `0` after masking can
spuriously "match" a position the shorter array never specified (concrete
counter-example: `mask = [0xFF, 0xFF]`, `pattern = [0xAA]`,
`data = [0xAA, 0x00]` incorrectly matched). Fixed by unconditionally
rejecting `mask.len() != pattern.len()` at the top of the function, before
the `.max()`/short-frame logic runs at all -- this closes the gap for any
direct mock caller bypassing the service layer's own equal-length
validation (`ioctl_start_repeat_message`'s `mask_data.len() !=
pattern_data.len()` check, `rpc_misc.rs`, unaffected by this fix). Finding
2: the repeat worker's autonomous transmit (`spawn_repeat_worker`) only
ever recorded its transmission in `written_msgs`, never producing a
loopback echo the way `PassThruWriteMsgs`'s own handler does when
`CONFIG_LOOPBACK` is enabled -- so a client observed loopback echoes for
ordinary writes but never for repeat traffic, purely because of which code
path happened to transmit an otherwise-identical frame. Fixed by mirroring
`PassThruWriteMsgs`'s exact echo-construction block (same `RxStatus`/
`RX_TX_MSG_TYPE` composition via `tx_format_flags_to_rx_status_bits`, same
`note_rx_frame_for_repeat_slots` call, same `rx_queue` push) inside the
worker's existing transmit critical section, gated on
`channel.loopback_enabled()`. Both fixes are covered by regression tests:
`j2534-0404-mock/src/lib.rs`'s own `#[cfg(test)]` module gained
`mismatched_length_mask_pattern_is_rejected_even_when_data_is_long_enough`
(Finding 1's exact counter-example), and
`j2534-0404-service/tests/grpc_mock/repeat_message.rs` gained
`repeat_worker_autonomous_transmit_emits_a_loopback_echo` (Finding 2, a
`Condition == 0` slot on a loopback-enabled CAN CLL, observed via a
receive-only monitor + `wait_for_result_data`).

**Round 20's third finding (`tx_header.rs`'s `kwp_header_bytes`/
`response_header_bytes`, KWP/ISO14230 request-header composition; full
design in ADR-166).** Codex reported that `kwp_header_bytes`'s fixed
4-byte header (format, target, source, explicit length byte — ADR-050's
original model) malforms every request built from this codebase's own
ISO 9141-2 presets (`iso_15031_5_on_iso_9141_2`/`sae_j2190_on_iso_9141_2`),
which configure the CARB/ISO9141-2 exception-addressing format byte shape
(`0x6C`/`0x68`) rather than ISO14230's addressed-with-separate-length
shape (`0x80`) ADR-050 assumed unconditionally. A `design-advisor` consult
(the literal Codex-suggested fix — mirroring `events.rs::kwp_header_and_
payload_len`'s RX-side bit-7-only address check — would itself have
broken these same presets, since CARB's format byte sets bit 6, not bit
7) found the actual rule is the format byte's top two bits jointly, per
ISO 22900-2 Table 76: CARB (`0x40`) is a 3-byte header with no trailing
length byte at all; ISO14230 addressed (`0x80`/`0xC0`) keeps ADR-050's
original 4-byte shape when the format's low 6 bits are configured `0`,
or recomposes the length into those low 6 bits (capped at 63, `Err`
above that) when configured nonzero; unaddressed (`0x00`) is a 2-byte
`[format, length]` header with no target/source. Fixed directly in the
shared `kwp_header_bytes`/`build_tx_message` functions (all four call
sites already `Result`-propagating, unlike `tx_header::can_addressing_tx_flags`'s
own unbounded-blast-radius `ISO15765_ADDR_TYPE`-gating gap, since fixed and
out of this backlog) and mirrored in `response_header_bytes`'s ISO9141/ISO14230
stop-condition-template branch so a repeat slot's transmitted frame and its
own stop-condition mask describe the same wire shape. Two residuals from this
investigation were recorded in the Prioritized Backlog and have since been
resolved: `events.rs::kwp_header_and_payload_len`'s own CARB-blind
RX-direction parsing (fixed by ADR-167), and `response_header_bytes`'s
embedded-length branch not wildcarding the response format byte's
unpredictable low-6-bits value (fixed by wildcarding just the low 6 bits,
mask `0xC0`, keeping the top-2-bit address-mode discriminator an exact
match).

**Round 21 (Codex review, ADR-165 PR #42 round 21; `j2534-0404-mock/src/lib.rs`).**
`spawn_repeat_worker`'s `Condition == 0` (unconditional/free-running) branch
did one uninterruptible `std::thread::sleep(interval_ms)` between
retransmissions -- a worker whose slot `IOCTL_STOP_REPEAT_MESSAGE`/disconnect/
`__mock_reset` removed while it was mid-sleep stayed alive, asleep, for up to
the rest of the interval before noticing and exiting, so repeatedly
starting/stopping long-interval slots could accumulate unboundedly many
sleeping worker threads despite `MAX_REPEAT_SLOTS_PER_CHANNEL`. Fixed by
polling in `POLL_STEP_MS` (5ms) steps up to the deadline, mirroring the
`Condition == 1` branch's own pre-existing, already-reviewed polling
pattern -- checking `REPEAT_WORKER_EPOCH` and slot presence each step, and
exiting immediately once either indicates the slot is gone. A same-round
edge-case-hunter pass caught that the new loop's under-lock re-check omitted
the `REPEAT_WORKER_EPOCH` re-check the `Condition == 1` loop already has --
not exploitable for corruption (this branch only reads, and the transmit
block's own independent epoch re-check still guards every write), but it
could let a stale-epoch worker mistake a fresh same-`msg_id`-keyed
post-reset slot for its own and keep polling on its own stale deadline for
nearly the rest of the original interval, reproducing almost exactly the
symptom this round's fix exists to close. Fixed by adding the missing
under-lock re-check, matching the `Condition == 1` loop exactly. The same
pass also flagged a test-coverage gap (Prioritized Backlog): no test
exercises the actual thread-exit-promptness property either fix bears on,
since the mock has no thread-liveness introspection hook.

**Round 22 (Codex review, ADR-165 PR #42 round 22; `rpc_misc.rs`'s
`require_owned_repeat_message`).** `PDU_IOCTL_QUERY_REPEAT_MESSAGE`/
`_STOP_REPEAT_MESSAGE`'s shared ownership-check helper validated `MsgId`
ownership before validating connection state. `DisconnectComLogicalLink`'s
teardown (`rpc_link.rs`) clears `channel_id` to `None` and drains
`repeat_message_ids` to empty while leaving the CLL's own entry in
`logical_links` (only `DestroyComLogicalLink` removes the entry outright),
so with ownership checked first, a disconnected-but-not-destroyed CLL
always failed that check -- regardless of whether the `MsgId` had genuinely
been owned immediately before disconnect -- making the function's
documented connection requirement structurally unreachable and reporting a
misleading `PDU_ERR_INVALID_MSG_ID` instead of `PDU_ERR_CLL_NOT_CONNECTED`.
Fixed by swapping the check order: connection state first, ownership
second. New test: `query_after_disconnect_reports_not_connected_not_
invalid_msg_id`. A same-round edge-case-hunter pass confirmed the reorder
has no effect on any other reachable scenario (a connected CLL querying an
unowned or sibling-owned `MsgId` still correctly reports
`PDU_ERR_INVALID_MSG_ID`) and caught that the fix's own doc comment
overstated its mechanism by also naming `DestroyComLogicalLink` -- a
destroyed CLL's handle is removed from `logical_links` entirely, so it hits
`require_owned_repeat_message`'s separate `unknown_handle_status` guard
(`PDU_ERR_INVALID_HANDLE`) before either check the reorder touches ever
runs, on both orderings; Destroy was never part of this bug. Corrected the
comment and added a companion test,
`query_after_destroy_reports_unknown_handle_not_not_connected`, pinning
that distinct, already-correct behavior. The same pass also surfaced an
unrelated, pre-existing gap (not caused or changed by this round's
reorder): at the time, `handle_channel_hard_error` (`events.rs`) cleared a
hard-errored CLL's `channel_id`/`connected` state the same way
`Disconnect` does, but did not yet drain its `repeat_message_ids` the way
`Disconnect`/`Destroy` do -- later fixed by PR #47, which added that drain
gated on `was_primary`. The remaining `was_primary`-discrimination
test-coverage gap that fix left behind is tracked in the Prioritized
Backlog.

**ADR-186 (SAE J2534-1 §7.2.7 periodic-message DataSize cap).** Closes the
two gaps round 15's own note above flagged as out of scope: (1)
`RepeatMsgData[0]`'s size-range check (round 6, above) reused the wider
ordinary-TX ranges instead of the flat periodic-message cap SAE J2534-1
§7.2.7/SAE J2534-2 14.2.1 impose on every protocol — replaced with a
periodic-capped range (`4..=12` raw CAN, `4..=11`/`5..=11` ISO15765; SAE
J1939 initially got its own dedicated `5..=13` arm on the theory that clause
16.4.4's DON'T-CARE destination-address byte made 13 the correct
single-frame boundary rather than 12, but a third-round design-advisor
consult reversed this: SAE J2534's `<DataSize>` field never carries a "wire
bytes transmitted" carve-out anywhere in the spec family, so J1939 falls
through to the same generic `5..=12` treatment — the existing per-protocol
range's own upper bound capped at 12 — as every other protocol below),
rejecting an oversized
`repeat_msg_data` with `invalid_argument` before composition; the round-4/5
`FD_CAN_PS` padding-to-64-bytes block is deleted as unreachable under the
new cap. `RepeatMsgData[1]`/`[2]` (mask/pattern templates) are explicitly
exempt — the round-17/18 widening above still applies to them unchanged. (2)
`install_client_message_filters` (`rpc_link.rs`) now ORs `TX_FD_CAN_FORMAT`
into a client filter's TxFlags for any FD-connected link, mirroring round
15's own repeat-message-template treatment; `install_pass_all_filter`'s own
templates are unaffected (already valid regardless — clause 21.4.4's own
unconditional `4..=12` range, which applies whenever `TX_FD_CAN_FORMAT` is
unset, not Table 91, which only governs the `FD_CAN_FORMAT = 1` case a
plain pass-all template never sets). A follow-up design-advisor consult
confirmed the periodic cap fully subsumes (not merely "runs ahead of and
makes largely unreachable") the pre-existing functionally-addressed Single
Frame check (ADR-055/ADR-169) for every ISO15765 flavor, classic and FD
alike — that inlined check has since been deleted outright, not left in
place as dead code. Full design and Consequences in
[ADR-186](../../../docs/adr/ADR-186-repeat-message-periodic-datasize-cap.md).

**Integration tests (`j2534-0404-service/tests/grpc_mock/repeat_message.rs`).**
Exercises the same behavior through the full gRPC `IoCtl` stack: START/QUERY
round-trip on a connected, opted-in, addressed CAN CLL; START rejected on a
non-opted-in module; QUERY/STOP cross-CLL `MsgId` ownership rejection
between two sibling CLLs sharing one physical channel; STOP-then-QUERY slot
teardown; `DestroyComLogicalLink`/`DisconnectComLogicalLink` slot cleanup
(verified via a raw `PassThruIoctl(QUERY_REPEAT_MESSAGE)` call directly
against the mock cdylib, `MockBackdoor::repeat_message_exists`, since the
CLL — and any gRPC handle to query the slot through the service — is gone
by the time that assertion runs); and missing/wrong-shaped `input_data`
rejection for all three commands.

## SAE J2534-2 Extended Programming Voltage + J1962 Pin Voltage Read (Phase 13)

Implements clause 15 (Extended Programming Voltage) and clause 23 (J1962 Pin
Voltage Read) — both classified "no `design-advisor` needed" in
`docs/j2534-2-support-plan.md` §6, and confirmed during implementation to be
mechanical extensions of two already-established patterns rather than a new
design decision, so **no ADR was written for this phase**: the error-mapping
choice below directly reuses the pre-existing `ERR_CHANNEL_IN_USE ->
PduErrResourceBusy` precedent (`error.rs`) and the pre-existing `ERR_PIN_INVALID
-> PduErrMuxRscNotSupported` precedent (A2-21, `ioctl_set_prog_voltage`), and
the new IOCTL reuses Phase 4/12's existing module-scoped `PDU_IOCTL_*`
dispatch shape and the existing generic `Unum32Value` `DataItem` variant —
no new proto message, no new state, no new lock ordering, no new concurrency
model.

**Clause 15 (pin 9 Short-to-Ground) needed no new pin validation.**
`ioctl_set_prog_voltage` (`rpc_misc.rs`) already forwards `pin_on_dlc`/
`prog_voltage_mv` straight to `PassThruSetProgrammingVoltage` with no
service-side pin allowlist, so pin 9 already flowed through unchanged. The
actual gap was error-mapping: the native call's two new clause-15 failure
codes, `ERR_PIN_IN_USE` (setting voltage on pin 9 while it is grounded, or
grounding pin 9 while it has voltage applied — a same-pin state conflict)
and `ERR_VOLTAGE_IN_USE` (grounding pin 9 while pin 15 is grounded, or vice
versa — pin 9's and pin 15's Short-to-Ground states are mutually exclusive),
previously fell through the function's existing `is_pin_invalid` 2-way
branch into the generic `PDU_ERR_VOLTAGE_NOT_SUPPORTED` catch-all — wrong,
since these represent a genuine resource conflict, not an unsupported
voltage value. Extended to a 3-way dispatch: `ERR_PIN_INVALID` (unchanged) ->
`PDU_ERR_MUX_RSC_NOT_SUPPORTED`; `ERR_PIN_IN_USE`/`ERR_VOLTAGE_IN_USE` (new)
-> `PDU_ERR_RESOURCE_BUSY`, matching the existing `ERR_CHANNEL_IN_USE`
precedent (not `PDU_ERR_SHARING_VIOLATION`, reserved for `ERR_DEVICE_IN_USE`'s
distinct cross-process sharing conflict); everything else (including
`ERR_NOT_SUPPORTED`, a device lacking pin 9 support) -> the unchanged generic
catch-all. Also added to the generic `pdu_error_for` table (`error.rs`) for
defense-in-depth, alongside the sibling `ERR_CHANNEL_IN_USE` entry — the 3-way
dispatch above doesn't consult that table (it uses `map_native_error_as` with
an explicit override, matching its own pre-existing pattern), but any other
call site reached via the generic `map_native_error_for_link` now gets the
correct mapping too, should a future phase encounter either code elsewhere.

**Clause 23 adds `PDU_IOCTL_READ_J1962PIN_VOLTAGE` (`PDU_IOCTL_BASE + 0x17`,
the 23rd D-PDU command), module-scoped** (`M`, like `READ_VBATT`/
`SET_PROG_VOLTAGE`/`READ_PROG_VOLTAGE` — a device-level property, not a
per-CLL one). Input and output both reuse the existing generic `Unum32Value`
`DataItem` variant (no proto change): input is the J1962 pin number (1-16),
output is the voltage in millivolts. `j2534-0404`'s new `read_j1962_pin_voltage`
wrapper is built on a new private `ioctl_write_read_u32` helper (mirroring
the existing input-less `ioctl_read_u32`, but passing `InputPtr` too).
`ERR_PIN_INVALID` (unsupported pin — clause 23 always excludes pins 4/5, and
any pin outside 1-16 is out of range) maps to `PDU_ERR_MUX_RSC_NOT_SUPPORTED`,
consistent with `ioctl_set_prog_voltage`'s own A2-21 mapping for the same
native code (both represent ISO 22900-2 Table 49's "invalid pin/resource"
concept); any other native failure falls through to the generic
`map_native_error_for_link` — no IN_USE conflict concept applies to a
read-only IOCTL. The service does no pin-range validation of its own,
mirroring `ioctl_set_prog_voltage`'s existing no-allowlist convention; the
mock (and a real adapter) is the sole source of truth. `j2534-0404-mock`'s
new dispatcher arm rejects pins `0`, `4`, `5`, and `17..` with
`ERR_PIN_INVALID`, reports pin 16 through the exact same code path as its
`IOCTL_READ_VBATT` arm (`MOCK_VBATT_MV`, per clause 23's "pin 16 must match
READ_VBATT" requirement — not a separately-maintained duplicate literal),
and reports a fixed `MOCK_J1962_PIN_VOLTAGE_MV` (5000 mV) for every other
valid pin. A new `__mock_set_j1962_pin_voltage_error` backdoor (mirroring
`__mock_set_prog_voltage_error`'s convention) lets a test force the generic
native-failure fallback branch, which the pin-based checks alone can never
reach.

**PR #48 round 1 corrections (Codex review, 2 findings, P2 each):**
1. `ioctl_read_j1962_pin_voltage` (`rpc_misc.rs`) was originally missing the
   SAE J2534-2 clause 5 opt-in gate entirely — a base J2534-1 module (no
   `"J2534-2:"` `pname` prefix) could reach `PDU_IOCTL_READ_J1962PIN_VOLTAGE`
   and get a successful reading, since the service did no pin allowlist and
   the mock does no opt-in check of its own. Fixed by adding the same
   `discovery::is_j2534_2_opted_in` check `ioctl_start_repeat_message` (ADR-165)
   already uses for clause 14, rejecting with `PDU_ERR_ID_NOT_SUPPORTED`
   before the input is even parsed. `ioctl_set_prog_voltage` (clause 15)
   deliberately does **not** get this same gate: unlike `READ_J1962PIN_VOLTAGE`,
   `SET_PROG_VOLTAGE` is not a new IOCTL — clause 15 only extends an
   *existing* J2534-1 function's behavior (pin 9 support), matching the
   support plan's own step 6 distinction ("a phase with no new IOCTL...");
   the real native adapter's own clause-5 `PassThruOpen`-time gating (a
   non-opted DLL "must assume J2534-1-only behavior") is the correct
   enforcement point for that case, not a service-side duplicate check.
2. `j2534-0404-mock`'s `IOCTL_GET_DEVICE_INFO` dispatcher never advertised
   `DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED`/`_MAX` (both added in Phase 0,
   never wired to any dispatcher arm before this PR) — an opted-in client
   following the clause 25 discovery-first workflow would conclude the
   already-implemented operation is unavailable and never call it. Fixed by
   adding a dedicated match arm reporting `Supported = 1`, `Value = 1` for
   `_SUPPORTED` and `Value = 24_000` (millivolts — clause 23's minimum
   required 0-24 VDC measurable range) for `_MAX`.

Both fixes verified with a new white-box `discovery.rs` unit test
(`device_info_reports_supported_for_j1962_pin_voltage_read`, mirroring
`device_info_reports_supported_for_a_base_protocol`'s pattern — no
gRPC-visible discovery RPC exists per ADR-152 Decision 2, so this internal
`discovery_device_info` call is the correct test seam) and a new
`pdu_ioctl.rs` integration test
(`read_j1962_pin_voltage_rejects_module_not_opted_into_j2534_2`, mirroring
`repeat_message.rs`'s own not-opted-in test). Adding the opt-in gate also
required switching the five existing `PDU_IOCTL_READ_J1962PIN_VOLTAGE`
success-path tests from the plain `TestServer::start()` to a new local
`start_j2534_2_server()` helper (mirroring `repeat_message.rs`'s identical
helper, not shared via `harness.rs` per this codebase's existing
per-file-helper convention) — they would otherwise now fail the new gate.

**Registered at all three `names.rs` ADR-079 sites** (import list, lowercase
shortname match, name/value table) alongside the existing 22 commands, and in
`service_params.rs`. `names.rs`'s "22 commands" references (including the
`map_ioctl_name_resolves_all_twenty_two_commands` test name, renamed to
`...twenty_three_commands`) updated to 23 throughout.

**Tests** (`tests/grpc_mock/pdu_ioctl.rs`): clause 15's two new native-error
mappings (`ERR_PIN_IN_USE`/`ERR_VOLTAGE_IN_USE` -> `PDU_ERR_RESOURCE_BUSY`,
alongside the existing `ERR_PIN_INVALID` test, unchanged); clause 23's
successful read on a supported pin, pin 16 matching a live `READ_VBATT` call
in the same test, pins `0`/`4`/`5`/`17`/`1000` all rejected with
`PDU_ERR_MUX_RSC_NOT_SUPPORTED` (an edge-case-hunter follow-up on this same
PR — the initial diff only covered the two named-unsupported pins 4/5, not
the full out-of-range boundary the service's own no-range-validation design
leaves entirely to the mock), missing `input_data` rejected with
`InvalidArgument`, module-not-connected rejected with
`PDU_ERR_MODULE_NOT_CONNECTED`, and the generic native-failure fallback
branch exercised via the new mock backdoor (another edge-case-hunter
follow-up — that branch was otherwise unreachable by any test).

## SAE J2534-2 Fault-Tolerant CAN (ADR-168, Phase 6)

Brings SAE J2534-2 clause 20 (Fault-Tolerant CAN, ISO 11898-3) into scope.
Full design and Consequences (including two implementation-time corrections
to the original Decision) in
[ADR-168](../../../docs/adr/ADR-168-j2534-2-fault-tolerant-can-phase6.md); this
section is the implementation-level summary. Clause 20's own text is short:
`FT_CAN_PS`/`FT_ISO15765_PS` are equivalent to `CAN_PS`/`ISO15765_PS` except
as specifically mentioned, and ISO 22900-2 models `ISO_11898_3_DWFTCAN` as
its own bus type — the same shape ADR-164 (Phase 4, Single Wire CAN)
established, so this phase mirrors ADR-164's pattern directly rather than
re-deriving it.

**Resource-table additions (`resources.rs`).** Ten new rows, `0x0230`-
`0x0239`, mirroring every existing row on `BUSTYPE_ISO_11898_2_DWCAN`
(including the `0x0234`/`0x0238` alias relationship, reproduced from
`0x022A`/`0x022E`'s SWCAN precedent) on a new `BUSTYPE_ISO_11898_3_DWFTCAN`
(`0x030A`). Unlike SWCAN's single-pin default, FTCAN is a genuine CAN-high/
CAN-low differential pair (like DWCAN), so `PINS_ISO_11898_3_DWFTCAN` is a
two-pin default: pin 1 (HI) / pin 9 (LOW), clause 20.2.1's first-listed
pin-pair (the clause's other documented pair, pins 3/11, is reachable via
ordinary Pin Selection override, not a second resource row). Each row
reuses its dual-wire sibling's `ChannelProtocol` unchanged with an
`_FTCAN`-suffixed `protocol_name` (same disambiguation reasoning as
SWCAN's `_SWCAN` suffix) and `hw_protocol_override` set directly to
`PROTOCOL_FT_CAN_PS`/`PROTOCOL_FT_ISO15765_PS`. New `is_ft_protocol_id()`
funnel and `base_protocol_id()` arms (`PROTOCOL_FT_CAN_PS → CAN`,
`PROTOCOL_FT_ISO15765_PS → ISO15765`) mirror `is_sw_protocol_id()`'s own
shape exactly, so `comparam_id::to_j2534_config_id` treats an FT link as
its base CAN/ISO15765 family for free — the same ADR-157 funnel every
other `_PS`/`_CHx` id already uses.

**Connect-time model (`names.rs`, `rpc_link.rs`).** `resolve_pin_selection`
gains an FT arm alongside its existing SW one: requires the connecting
module to have opted into J2534-2, then always returns a real `pin_select`
pair — never the base-connect `Ok(None)` short-circuit — using the
caller's `dlc_pin_data` or the row's own two-pin default
(`0x0000_0109`) when none is supplied, so a connect always issues the
explicit `SET_CONFIG(CONFIG_J1962_PINS)` clause 20.2.1's explicit-pin-only
text requires. **Implementation-time correction to ADR-168's
original Decision (see ADR-168 Consequences):** giving FT links
`base_protocol_id` arms for ComParam routing also made
`rpc_link.rs::apply_fd_mode`'s `CAN`/`ISO15765` match arms reachable for FT
links, which without a guard would have silently substituted a staged-FD-
ComParams FT link to `FD_CAN_PS`/`FD_ISO15765_PS` — the ADR's original text
claimed this hazard didn't exist. Fixed with explicit FT reject arms in
`apply_fd_mode`, mirroring its pre-existing SW reject arms exactly; see
`staging_fd_comparams_on_an_ft_link_is_rejected_not_substituted` in
`tests/grpc_mock/ft_can.rs` for the regression test. **A second
implementation-time correction** (edge-case-hunter finding, BLOCKING): the
same `base_protocol_id` FT arms exposed `GetResourceStatus`'s
`matches_status_hw_id` closure (`rpc_link.rs`) to the identical false-"in
use" hazard ADR-164's own "Bug 1" fix addressed for SWCAN — that guard only
checked `is_sw_protocol_id(hw_id)`, so a query naming the plain dual-wire
CAN resource falsely reported bit 0 set while only an FTCAN sibling was
connected. Fixed by adding the missing `is_ft_protocol_id(hw_id)` check to
the same guard; see `get_resource_status_does_not_let_a_connected_ft_can_link_occupy_its_dual_wire_sibling`
and its reverse-direction sibling in `tests/grpc_mock/ft_can.rs`. **A third
implementation-time correction** (Codex review round 2, P2): naming the raw
`FT_CAN_PS`/`FT_ISO15765_PS` hardware protocol id directly (bypassing the
resource table entirely, `resource_row: None`) with no `dlc_pin_data` was
silently assigned the resource-table row's own default pin pair
(`0x0000_0109`) — a default that only reflects the *table row's* own
choice, not one the caller made; clause 20.2.1 itself has no default
pin-pair at all. `resolve_pin_selection`'s FT arm now requires
`resource_row.is_some()` before applying the hardcoded default, rejecting
with `invalid_argument` otherwise (`connecting_the_raw_ft_protocol_id_directly_without_pins_is_rejected`
in `tests/grpc_mock/ft_can.rs`). **This same fix was also applied to the
pre-existing SW arm** (identical bug, not introduced by this PR but
surfaced by the same review round and fixed alongside it — see
`connecting_the_raw_sw_protocol_id_directly_without_pins_is_rejected` in
`tests/grpc_mock/sw_can.rs`).

**No ComParam or IOCTL work this phase.** Unlike SWCAN, clause 20 calls
out no new `SET_CONFIG` translations or IOCTLs — an FT link uses its base
CAN/ISO15765 family's existing ComParam translations unchanged, with
`comparam_defaults::bustype_default_params("ISO_11898_3_DWFTCAN")`'s
already-seeded Table B.21 baudrate default (added generically in an
earlier phase, ahead of this phase's resource-table wiring) supplying the
bus-type-scoped baseline.

**Mock support (`j2534-0404-mock`).** New `is_ft_protocol()` mirrors
`is_sw_protocol()` exactly: gates `pins_assigned` (unassigned until
explicit `SET_CONFIG`, same as SW) and feeds the mock's own local
`base_protocol_id` (consulted by `IOCTL_GET_PROTOCOL_INFO`'s validity
check). No new state-machine logic — connect, filter install, and
periodic-message start all reuse the existing CAN-family paths
parameterized by `pins_assigned` alone, the same "pure parameterization"
shape SWCAN's own mock support used.

**Deferred, not fixed this phase (see ADR-168 Consequences):**
`LINK_FAULT` (RxStatus bit 17) forwarding into `RxFlag` — ADR-168's
original Decision assumed a bare `RX_STATUS_FLAGS_MASK` extension would
suffice, but `events.rs`'s existing forwarding path casts to `u8` before
packing into `rx_flag_bytes`'s single flags byte, and bit 17 sits entirely
outside `u8`'s range; a mask-only extension would be dead code. This isn't
a new residual — bit 17 sits inside the same RxStatus 16-18 range the P2
backlog entry (`SW_CAN_HV_RX`/`SW_CAN_HS_RX`/`SW_CAN_NS_RX`, ADR-164)
already defers as "a genuine new RxFlag-byte extension." **Update
(ADR-191):** decided against — `LINK_FAULT` tags a message that was
correctly received despite a transceiver-detected fault (genuine,
already-delivered content per Table 86), so only the fault annotation
itself is missing; recorded there as a closed, accepted residual rather
than an open item. FT Additional Channels (`_CHx`) are out of scope this
phase, matching every other family's `_CHx` deferral (ADR-156 precedent).

**A fourth implementation-time correction** (Codex review round 3, P2):
the third correction's own `resource_row.is_some()` connect-time fix has a
`GetResourceStatus` counterpart bug. `rpc_link.rs`'s pre-existing
(ADR-157-era) query-side fallback for a `ResourceId` matching no
resource-table row normalizes the raw id down to its base protocol,
discarding any qualifier — correct for the general `_PS` case, but wrong
for SW/FT specifically (the one family whose connected link's raw
`hw_protocol_id` *is* the qualified id, per the second correction's own
reasoning). Combined with that second correction's guard, a raw
`FT_CAN_PS`/`FT_ISO15765_PS` `GetResourceStatus` query bypassing the table
lost its FT identity before the guard ran, falsely reporting a connected
FT link as idle while a plain dual-wire sibling link could satisfy the
query instead. Fixed by preserving the raw id as the query candidate's own
`hw_protocol_override` when it is SW/FT-family. **The identical
pre-existing bug in the SW arm** was fixed the same way in the same
commit.

**Tests** (`tests/grpc_mock/ft_can.rs`): connecting an FT resource with no
caller-supplied pins assigns the row's own two-pin default and issues the
mandatory `SET_CONFIG(CONFIG_J1962_PINS)`; connecting with the clause
20.2.1's second documented pin-pair (3/11) via ordinary Pin Selection
overrides the row default correctly; the `apply_fd_mode` FD-on-FT
regression above; the `GetResourceStatus` cross-occupancy regression pair
above (both directions, mirroring `sw_can.rs`'s own pair); the raw-protocol-id
rejection regression above (and its `sw_can.rs` counterpart for the
identical pre-existing SW bug); the raw-protocol-id `GetResourceStatus`
regression pair above (both directions, and its `sw_can.rs` counterpart);
a `resources.rs` unit test confirming `is_ft_protocol_id()` is true only
for its own two ids and that `base_protocol_id()` round-trips correctly.
A final close-out `edge-case-hunter` pass (round 4) found the raw-id
rejection test and the raw-id `GetResourceStatus` test exercised the two
fixes above only in isolation — the rejection test never supplies pins
(so it never reaches a successful raw-bypass connect), and the status
test connects through the resource-*table* row, not the raw-bypass path.
`connecting_the_raw_ft_protocol_id_with_explicit_pins_then_querying_by_raw_id_finds_the_link`
(and its `sw_can.rs` counterpart) closes that gap: a raw-bypass connect
with explicit pins (`resource_row: None`), followed by a `GetResourceStatus`
query on that same raw id, confirmed end to end.
A Codex review round 5 finding then found `names.rs`'s
`candidates_matching_protocol` (the `GetResourceIds` protocol-id/name
filter) never consulted `hw_protocol_override` in its fallback, so
`GetResourceIds(protocol_id = PROTOCOL_FT_CAN_PS)`/`PROTOCOL_FT_ISO15765_PS`
returned an empty list even though those ids already worked end to end for
connect/query — a pre-existing bug every `hw_protocol_override` row
(SW/FD's `_PS` rows, the SCI configuration rows) inherited, not something
this phase introduced. Fixed by mirroring `legacy_bustype_hw_id`'s already-
accepted pattern (`hw_protocol_override.unwrap_or_else(protocol.j2534_protocol_id)`)
in both the `ProtocolId` and `ProtocolName` fallback arms. New tests in
`ft_can.rs`/`sw_can.rs` pin `GetResourceIds(PROTOCOL_FT_CAN_PS)` → `[0x0230]`,
`GetResourceIds(PROTOCOL_FT_ISO15765_PS)` → the nine ISO15765-based FT rows,
and the SW equivalents. `docs/j2534-0404-architecture.md`'s "GetResourceIds
/ CreateComLogicalLink Resource Table" section is updated to describe the
`hw_protocol_override`-aware fallback.
A Codex review round 6/design-advisor consult then found the FT arm's
explicit-`dlc_pin_data` branch delegated straight to `compute_pin_select`
with no further validation, silently accepting pin selections clause
20.2.1 does not document — a single explicit pin (physically incomplete)
or a mismatched pair combining primaries/secondaries from the two
different documented pairs, both of which pack to a well-formed bitmask
per the generic pin-typing rules `compute_pin_select` alone enforces.
Fixed by validating the packed `pin_select` result is exactly `0x0000_0109`
or `0x0000_030B` (clause 20.2.1's two documented pairs), rejecting
`invalid_argument` otherwise — deliberately scoped to the FT arm only, not
the general clause 6 mechanism (see the Prioritized Backlog entry below).
`connecting_an_ft_resource_with_a_single_explicit_pin_is_rejected` and
`connecting_an_ft_resource_with_a_mismatched_pin_pair_is_rejected`
(`tests/grpc_mock/ft_can.rs`) pin the two rejected shapes; the two accepted
pairs were already pinned by
`connecting_an_ft_resource_emits_the_internal_pins_set_config` (default
1/9 pair) and
`connecting_an_ft_resource_with_explicit_pins_uses_the_caller_supplied_pin_pair`
(explicit 3/11 pair) above.
A final close-out `edge-case-hunter` pass then found the round-5
`GetResourceIds` fix and the round-6/7 pin-pair validation fix were each
pinned only in isolation, with no test chaining a discovery-returned
resource_id into a connect that exercises pin-pair validation, and no
existing pin pair supplied in LOW-then-HI array order to confirm
`compute_pin_select`'s pin-TYPE-based (not array-position-based)
classification; `discovered_ft_resource_id_then_connect_validates_pin_pairs`
and `connecting_an_ft_resource_with_pins_in_low_then_hi_order_packs_the_same_value`
(`tests/grpc_mock/ft_can.rs`) close both gaps.

## SAE J2534-2 UART Echo Byte Protocol (ADR-170, Phase 9)

Brings SAE J2534-2 clause 12 (UART Echo Byte Protocol: Honda ABS/VSA per
SAE J2809, or KWP1281 per SAE J2818) into scope. Full design and rejected
alternatives in
[ADR-170](../../../docs/adr/ADR-170-j2534-2-uart-echo-byte-phase9.md); this
section is the implementation-level summary. Architecturally different
from every prior J2534-2 phase this crate has shipped: clause 12 is a
K-line UART protocol with its own message format (a byte-echo scheme
between tester and ECU), not a CAN-family variant, so unlike CAN FD/SWCAN/
FT-CAN (which reuse `ChannelProtocol::CAN`/`ISO15765` via
`hw_protocol_override`) this phase adds a genuinely new, standalone
`ChannelProtocol::UART_ECHO_BYTE_PS` — the same category `ISO9141`/
`J1850PWM` already occupy.

**New `ChannelProtocol` and resource-table row.** `protocol.rs` gains
`ChannelProtocol::UART_ECHO_BYTE_PS` (`0x0000800A`, matching Phase 0's
already-committed `PROTOCOL_UART_ECHO_BYTE_PS` header constant), self-
mapping through `j2534_protocol_id()`. `resources.rs` gains exactly ONE new
resource-table row (`0x023A`, next in the `0x0200` namespace after FT-CAN's
`0x0230`-`0x0239`) — not ten like SWCAN/FT-CAN, since no OBD-family/service
-composite spec exists for this protocol to mirror, mirroring `0x0210
ISO_9141_2`'s own single-row shape instead. New `BUSTYPE_UART_ECHO_BYTE`
(`0x030B`) and `PINS_UART_ECHO_BYTE` (single pin, J1962 pin 7, the VW/Audi
convention clause 12.2.2 names) with `hw_protocol_override: None` — this
row's `protocol` field IS the new variant directly, not an override onto a
different base. `comparam_defaults.rs` gains a `uart_echo_byte_uart()`
bustype default seeding only `DATA_RATE` (9600 bps, clause 12.3.4.1's own
stated default), and a `PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS`
allowlist entry (required by this file's existing
`every_resource_row_has_protocol_and_bustype_defaults_or_is_allowlisted`
regression gate — a bustype-only default, no protocol-level default,
matches the established SWCAN/FT-CAN precedent for a native protocol with
no further per-protocol default to seed).

**No default pin, always explicit `SET_CONFIG` (`names.rs`).**
`resolve_pin_selection` gains a UART Echo Byte arm alongside its existing
SW/FT ones (renamed from `matched_row_is_sw_or_ft` to
`matched_row_needs_pin_selection` now that a third, non-CAN-family
protocol needs the same routing), always issuing the explicit
`SET_CONFIG(CONFIG_J1962_PINS)` clause 6's Pin Selection mechanism
requires, using the caller's `dlc_pin_data` or the row's own pin-7 default
when none is supplied. **Edge-case-hunter finding, fixed same-diff:** the
`ProtocolName`-matched connect route (as opposed to `ProtocolId`/
`ResourceId`) has its own separate row-matched-but-not-yet-pin-selection-
routed helper, which initially omitted the new protocol — a connect via
`RscData { protocol: ProtocolName("UART_ECHO_BYTE") }` would have silently
skipped pin selection entirely, leaving the channel permanently
pin-unassigned. Closed by extending the same helper;
`connecting_via_protocol_name_applies_the_default_pin`
(`tests/grpc_mock/uart_echo_byte.rs`) is the regression test, confirmed to
fail against the pre-fix code by direct revert-and-rerun.

**Narrow ComParam allowlist (`comparam_support.rs`).**
`is_param_allowed` gains an exact-identity branch for this protocol,
checked before the "Unknown protocol — allow" fallback that would
otherwise (silently, incorrectly) permit every ComParam on it: only
`DATA_RATE`/`LOOPBACK` (`is_universal_param`) are allowed, matching clause
12.3.4.1's closed parameter list. This also makes `CP_TesterPresentSendType`
and `CP_InitializationSettings` unreachable for this protocol by
construction — the former closes off mode-0 periodic tester-present
(clause 12.3.3.3 excludes `PassThruStartPeriodicMsg` for this protocol)
without a separate runtime guard; the latter is a deliberate policy choice
(ADR-170 Decision 3), not something clause 12.3.4.1 itself mandates, since
that ComParam has no native `SET_CONFIG` form to begin with.

**Repeat Messaging rejected (`rpc_misc.rs`).** `ioctl_start_repeat_message`
gains an explicit rejection for this protocol (`PDU_ERR_ID_NOT_SUPPORTED`),
mirroring the existing software-ISO-TP rejection; the shared
`require_owned_repeat_message` helper (used by `QUERY`/`STOP` too) gains
the same check, so all three of `START`/`QUERY`/`STOP_REPEAT_MESSAGE` are
covered from one call site — clause 12.3.3.1 explicitly excludes this
feature for this protocol (the interface cannot reliably track the
per-message counter it needs across an echo-byte exchange).

**`CoptStartcomm`'s `cop_data` must be exactly one byte
(`rpc_primitive.rs`/`events.rs`) — design-advisor fix, second edge-case-
hunter round.** The first implementation pass broadened the existing
`is_kline` five-baud-init eligibility gate (previously `ISO9141`/
`ISO14230` only, which excluded this protocol entirely — first
edge-case-hunter finding) to include `UART_ECHO_BYTE_PS`, but that alone
exposed two new failure modes a second edge-case-hunter pass caught live:
empty `cop_data` silently skipped protocol init altogether (returning
success with zero wire traffic — `comm_started = true`, no
`FIVE_BAUD_INIT`/`FAST_INIT` call at all), and `cop_data.len() >= 4`
silently misrouted through the legacy init-sequence heuristic to native
`FAST_INIT`, which clause 12 never defines (only an enhanced
`FIVE_BAUD_INIT` exists for this protocol). A `design-advisor` consult
decided the fix: `rpc_primitive.rs` gains a synchronous guard immediately
after the `is_kline` check — `cop_data.len() != 1` on this protocol is
rejected with `Status::invalid_argument` before init-sequence selection
ever runs, since clause 12's `cop_data` on `CoptStartcomm` carries only the
5-baud address byte, nothing else (message payloads belong in
`CoptSendrecv`). `events.rs::select_init_sequence`'s `legacy_heuristic`
closure also independently selects `FiveBaud` for this protocol
unconditionally, as defense-in-depth keeping that pure function's own
contract correct on its own terms. Regression tests:
`startcomm_with_empty_cop_data_is_rejected`,
`startcomm_with_oversized_cop_data_is_rejected`,
`startcomm_runs_five_baud_init_via_the_legacy_single_byte_heuristic`
(`tests/grpc_mock/uart_echo_byte.rs`), plus two `legacy_heuristic` unit
tests (`events_init_sequence_tests.rs`) — all confirmed non-vacuous by
direct revert-and-rerun at each of the three review rounds.

**TX/RX message size range (`protocol.rs`).** `tx_message_size_range`
gains a `UART_ECHO_BYTE_PS`/`_CHx` entry, `4..=256` bytes (clause 12.4.2
Table 38), single addressing mode. RX size (Table 38's own `3..=256`) is
not separately validated — confirmed during implementation that this
service has no RX-side size-validation mechanism for any protocol
(`PassThruReadMsgs` results are forwarded as-is), so this needed no new
code.

**Mock support (`j2534-0404-mock`).** New `is_uart_echo_byte_protocol`
wired into `ChannelState::new`'s `pins_assigned` gate (unassigned until
explicit `SET_CONFIG`, same as SW/FT), and into both the `FIVE_BAUD_INIT`
and `FAST_INIT` K-line acceptance gates (mirroring the service's own
`is_kline` broadening) so the mock can simulate this protocol's mandatory
5-baud init for tests.

**Deferred, matching every prior phase's precedent:** Additional Channels
(`_CHx`) for this protocol — a bare `PROTOCOL_ECHO_BYTE_CH1`/`CH128` header
macro already exists and `is_chx_protocol_id`'s range already covers it,
so naming it directly gets the existing generic out-of-scope rejection,
not a working resolution (an earlier ADR-170 draft incorrectly claimed
otherwise — corrected same-phase, edge-case-hunter finding). Discovery-cache
connect-time enforcement wiring for `DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED`
Phase 0 already added is now wired (ADR-185 Stage 1); its `_SIMULTANEOUS`
companion bit remains unwired.

New test file: `tests/grpc_mock/uart_echo_byte.rs` (11 tests, spanning
connect via resource id/raw id/`ProtocolName`, the ComParam allowlist,
Repeat Messaging rejection, the `cop_data` length guard, and — Codex
review round 2 — rejecting a second `dlc_pin_data` pin for this single-wire
protocol; the identical pre-existing bug found and fixed the same PR in
`sw_can.rs`'s own arm, which this new arm's pin-resolution logic copied).

**Accepted residual (Codex review + design-advisor consult, PR #57): no
client-facing access path exists for the ten `UEB_T0_MIN`-`UEB_T9_MIN`
timing parameters this phase's own header constants added.** This service
has no generic native `SET_CONFIG`/`GET_CONFIG` passthrough IOCTL at all
(`rpc_misc.rs`'s `PDU_IOCTL_GENERIC` is unconditionally unimplemented, and
ADR-152 Decision 2's own convention is curated per-command IOCTLs, not
generic forwarding) — an earlier ADR-170 draft incorrectly claimed such a
path existed. Every link runs at clause 12.3.4.1's own spec-mandated
defaults (Table 36), fully functional for the common case (nominal SAE
J2809/J2818 timing); only an ECU needing off-nominal 5-baud/inter-byte
timing is affected. See the new Prioritized Backlog item below and
ADR-170's Consequences section for the full writeup.

## SAE J2534-2 Honda DIAG-H Protocol (ADR-174, Phase 10)

Brings SAE J2534-2 clause 13 (Honda DIAG-H Protocol: Honda's proprietary
"92 Hm/2" diagnostic message format over the single-wire, half-duplex,
UART-based "DIAG-H" physical layer) into scope. Full design and rejected
alternatives in
[ADR-174](../../../docs/adr/ADR-174-j2534-2-honda-diagh-phase10.md); this
section is the implementation-level summary. The second standalone,
non-CAN-family J2534-2 protocol this crate has shipped (after UART Echo
Byte, ADR-170/Phase 9) — clause 13 defines no unqualified base id and no
relationship to any protocol this service already models, so it gets its
own `ChannelProtocol::HONDA_DIAGH_PS`, the same category `UART_ECHO_BYTE_PS`
already occupies.

**New `ChannelProtocol` and resource-table row.** `protocol.rs` gains
`ChannelProtocol::HONDA_DIAGH_PS` (`0x0000800B`, matching Phase 0's
already-committed `PROTOCOL_HONDA_DIAGH_PS` header constant), self-mapping
through `j2534_protocol_id()`. `resources.rs` gains exactly ONE new
resource-table row (`0x023B`, next in the `0x0200` namespace after UART Echo
Byte's `0x023A`) — mirroring UART Echo Byte's own single-row shape, since no
OBD-family/service-composite spec exists for this protocol either. New
`BUSTYPE_HONDA_DIAGH` (`0x030C`) and `PINS_HONDA_DIAGH` (single pin, J1962
pin 14 — the pin Honda's own diagnostic application always targets per
clause 13.2.4, including through the adapter cables 3-pin/5-pin-DLC
vehicles use) with `hw_protocol_override: None`. `comparam_defaults.rs`
gains a `honda_diagh_uart()` bustype default seeding `LOOPBACK` (0),
`P1_MAX`/`P3_MIN`/`P4_MIN` (reusing ISO9141's own K-line timing values per
clause 13.3.1's "reuses ISO9141 timing" text), **and `DATA_RATE` (9_600)** —
this last one is seeded internally even though clause 13's baud rate is
fixed at 9600bps and not `SetComParam`/`GetComParam`-reachable for this
protocol at all (see the ComParam allowlist paragraph below). The internal
seed exists because `ComParamSet::baud_rate()` has no protocol-specific
fallback and returns 0 when `DATA_RATE` is absent, and `rpc_link.rs` feeds
that value directly into `PassThruConnect`'s native baud-rate argument —
omitting the seed (the original implementation) would silently connect at
baud_rate=0 instead of the spec-mandated 9600bps, risking
`ERR_INVALID_BAUDRATE` on real hardware that validates the argument (Codex
review finding, PR #63). `PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS` gains the
matching allowlist entry.

**Round 2 (Codex review, PR #63): the same baud-rate gap also reaches a
second connect route.** `rpc_create_com_logical_link` only selects
`honda_diagh_uart()` via `resource_row.bus_type_name` — a `CreateComLogicalLink`
naming the raw `HONDA_DIAGH_PS` id directly (`RscData.protocol =
ProtocolId(...)`, explicit `dlc_pin_data`, no `bus_type_name`) resolves
`resource_row: None`, so the same fallback that already handles an empty/
unrecognized `bustype_name` string left the Working set fully empty —
`DATA_RATE` included — for this specific route. Fixed by checking
`resources::is_honda_diagh_protocol_id(protocol.j2534_protocol_id())`
**before** the `bustype_name` lookup in the `resource_row.is_none()` branch
(a `None if ... =>` match arm, not an `.or_else` fallback after
`bustype_name`) — an **edge-case-hunter follow-up in this same round** found
that an `.or_else`-after ordering only closes the *absent* `bustype_name`
case: `RscData.bus_type` and `RscData.protocol` are independent optional
fields with no cross-validation anywhere in this resolution path, so a
caller could name `HONDA_DIAGH_PS` via `protocol` while ALSO supplying an
unrelated but well-formed `bus_type_name` (e.g. `"ISO_14230_1_UART"`), which
an after-the-fact `.or_else` would never reach at all — the primary lookup
resolves successfully to the *wrong* protocol's defaults (10400bps) instead.
Checking the protocol identity first makes Honda DIAG-H's own fixed 9600bps
authoritative regardless of whatever `bus_type_name` RscData happens to
carry, closing both the absent- and mismatched-name cases in one change. Two
regression tests:
`connecting_the_raw_protocol_id_seeds_the_mandatory_9600_baud_rate` (absent
`bus_type_name`) and
`connecting_the_raw_protocol_id_ignores_a_mismatched_bus_type_name`
(mismatched `bus_type_name`) (`tests/grpc_mock/honda_diagh.rs`), both
asserting the native connect call actually receives 9600 via the mock's
`__mock_get_channel_baud_rate` backdoor. The identical *absent-bustype-name*
gap also existed for SWCAN/FT-CAN/UART Echo Byte's own raw-id-with-explicit-
pins routes (pre-existing, predates this PR), plus SAE J1939's (found
later) — all four were subsequently closed by a single general mechanism,
`resources::bustype_default_name_for_hw_protocol_id`
(`j2534-0404-service/src/service/resources.rs`), generalizing this same
Honda DIAG-H fix to every standalone `_PS`-only protocol family: SWCAN, FT-
CAN, UART Echo Byte, and SAE J1939 (Honda DIAG-H and SAE J1708 already had
their own dedicated fallback arms, which this helper also subsumes). Each
family's own regression tests for this fallback live alongside its other
tests in `tests/grpc_mock/{sw_can,ft_can,uart_echo_byte,j1939}.rs`.

**Round 3 (Codex review, PR #63): the allowed `P1_MAX`/`P3_MIN`/`P4_MIN`
values never actually reached the adapter.** `comparam_id::ComParamId::
to_j2534_config_id` only recognized `ISO9141`/`ISO14230` for this shared
translation, and `resources::base_protocol_id` does not collapse
`HONDA_DIAGH_PS` onto either (it self-maps) — so `apply_j2534_params`
silently dropped every staged `P1_MAX`/`P3_MIN`/`P4_MIN` value, seeded
defaults and client `SetComParam` overrides alike, instead of forwarding it
via `PassThruIoctl(SET_CONFIG)`. The RPC reported the values as accepted and
retained them in link state, but the adapter itself ran at its own unrelated
defaults. Fixed with a `resources::is_honda_diagh_protocol_id`-gated block
in `to_j2534_config_id`, scoped to exactly `P1_MAX`/`P3_MIN`/`P4_MIN` (not
the wider `W1`-`W4`/`TIDLE`/`TINIL`/`TWUP`/`PARITY`/`DATA_BITS`/
`FIVE_BAUD_MOD` group the shared ISO9141/ISO14230 match arm also covers,
since clause 13's own closed allowlist never permits those others for this
protocol at all). New unit tests
(`honda_diagh_reuses_p1_max_p3_min_p4_min_but_not_the_wider_kwp_timing_group`,
`comparam_id.rs`) and a new integration test
(`p1_max_p3_min_p4_min_reach_the_native_adapter_via_set_config`,
`tests/grpc_mock/honda_diagh.rs`) confirming both the seeded connect-time
default and a post-connect `SetComParam` override reach the mock's native
`SET_CONFIG` state with the correct 1us→0.5ms conversion (ADR-072).
**Close-out edge-case-hunter finding:** that integration test only exercised
the resource-id connect route; round 2's comparam-defaulting fix and round
3's translation fix were never jointly regression-tested on the raw-id
route both were written for. Closed by extending
`connecting_the_raw_protocol_id_seeds_the_mandatory_9600_baud_rate` with a
matching `config_value(MOCK_CHANNEL_ID, P1_MAX) == 40` assertion.

**Round 4 (Codex review, PR #63): the canonical `protocol_name` route
rejected a legitimate non-default pin.** `find_table_row_by_name` narrows a
name match's `dlc_pin_data` against the matched row's own fixed
resource-table pin data — built to disambiguate several same-named rows
(e.g. `SAE_J2610_SCI`'s four configurations), not to validate pins for a
protocol whose binding is resolved dynamically. Naming `"HONDA_DIAGH"` with
`dlc_pin_data` selecting pin 1 (valid per clause 13.2.4, but not row 0x023B's
own pin-14 convenience default) narrowed to zero rows and failed with a
generic "matches no configuration" error, before
`matched_row_needs_pin_selection` ever routed the match through
`resolve_pin_selection`'s own correct closed-set check. Fixed by factoring
the SW/FT/UART-Echo-Byte/Honda-DIAG-H family check
`matched_row_needs_pin_selection` already used into a shared
`row_needs_dynamic_pin_selection` predicate, and using it in
`find_table_row_by_name` to skip fixed-row pin narrowing entirely for a
single match belonging to one of these dynamically-pin-selectable families.
New regression test: `connecting_via_canonical_name_with_pin_1_succeeds`
(`tests/grpc_mock/honda_diagh.rs`). As a side effect this also closes the
identical latent gap for FT-CAN's own alternate pin-pair (clause 20.2.1's
`(3,11)`, not row 0x0230's own `(1,9)` default) and UART Echo Byte's own
non-default single pin (clause 12.2.2 documents no closed pin set at all, so
`resolve_pin_selection`'s arm already accepted any single pin — just not
reachable via the canonical-name route) reached via their own canonical
names — neither itself Codex-flagged, but the same mechanism this fix
corrects. A **close-out edge-case-hunter pass** (after Codex's round-4
approval) found this side effect was undertested: only Honda DIAG-H had a
regression test for it. Closed with two new tests in the sibling protocol
files this PR does not otherwise touch:
`connecting_via_canonical_name_with_the_alternate_pin_pair_succeeds`
(`tests/grpc_mock/ft_can.rs`) and
`connecting_via_protocol_name_with_a_non_default_pin_succeeds`
(`tests/grpc_mock/uart_echo_byte.rs`) — both pin down the correct,
already-shipped behavior so a future edit narrowing
`row_needs_dynamic_pin_selection`'s scope back toward "Honda-only" would be
caught, not silently reintroduce the bug for these two protocols.

**Closed-set pin validation, two single pins, not one.** Unlike SWCAN
(clause 9.2.1, one valid pin) or UART Echo Byte (clause 12.2.2, no
documented closed set at all), clause 13.2.4 documents exactly TWO valid
single pins for this protocol — J1962 pin 1 and pin 14. `names.rs`'s new
`resolve_pin_selection` arm (positioned after the UART Echo Byte arm,
structurally identical to the SWCAN arm's `is_empty()`/`len() > 1`/else
shape) accepts only `0x0000_0100` (pin 1) or `0x0000_0E00` (pin 14) in its
closed-set check — the single-pin analog of the two-pair check the SWCAN
pin-allow-list fix (this same session, `names.rs`) and the original FT-CAN
fix (ADR-168's "Seventh correction") already established for their own
protocols. `matched_row_needs_pin_selection`
(`resolve_com_logical_link_resource`) gains the matching
`is_honda_diagh_protocol_id` disjunct, so a connect via the resource-table
row (not just the raw protocol id) also routes through this pin-selection
logic rather than silently skipping it.

**ComParam allowlist narrower than the universal default
(`comparam_support.rs`).** `is_param_allowed` gains a `HONDA_DIAGH_PS`
branch allowing only `LOOPBACK`/`P1_MAX`/`P3_MIN`/`P4_MIN` (clause
13.3.3.1's closed parameter list) — narrower than `is_universal_param`
(`DATA_RATE`/`LOOPBACK`), since `DATA_RATE` is excluded (see above). This is
the first protocol-specific branch in this function checked *before* the
`is_universal_param` short-circuit rather than after it, specifically
because `is_universal_param` would otherwise let `DATA_RATE` through
unconditionally for every protocol including this one.
`CP_InitializationSettings` is excluded too, the same deliberate-policy
reasoning ADR-170 Decision 3 already established for UART Echo Byte (no
init sequence ever runs for this protocol regardless of the param's value —
see below — so it would carry no real information).

**`CoptStartcomm`'s `cop_data` is a genuine optional message, not an init
address (`rpc_primitive.rs`) — design-advisor decision, ADR-174 Decision
5.** Clause 13.3.1 states this protocol has no initialization process at
all — not 5-baud, not fast-init. Unlike UART Echo Byte (added to the
`is_kline` gate, since clause 12 still mandates a mandatory 5-baud address
byte), `HONDA_DIAGH_PS` is deliberately **excluded** from `is_kline` — a
non-empty `cop_data` therefore flows through the ordinary
`resolve_send_recv_tx` path as a genuine optional `CoptStartcomm` message,
the same treatment CAN/J1850/SCI already get, with no new code in
`rpc_start_com_primitive` itself. A comment at the `is_kline` match
documents this as deliberate, citing ADR-174, so a future reader does not
"fix" it by grouping DIAG-H with UART Echo Byte on physical-layer
resemblance alone. Regression test:
`startcomm_with_optional_message_transmits_as_a_genuine_message_not_an_init_address`
(`tests/grpc_mock/honda_diagh.rs`) asserts both that the message transmits
verbatim (no addressing header) and that `five_baud_init_count()`/
`fast_init_count()` both stay zero.

**Repeat Messaging needs no new rejection.** Clause 14.1 states Repeat
Messaging is supported on all protocols unless a protocol's own clause
explicitly excludes it (the mechanism UART Echo Byte's clause 12.3.3.1
exercises). Clause 13 contains no such exclusion for DIAG-H, so the existing
generic `START`/`QUERY`/`STOP_REPEAT_MESSAGE` forwarding already covers this
protocol correctly — verified directly against clause 13's text rather than
assumed from the UART Echo Byte precedent, which goes the other way.

**TX/RX message size range (`protocol.rs`).** `tx_message_size_range` gains
a `HONDA_DIAGH_PS` entry, `3..=255` bytes (clause 13.4.3 Table 48), single
addressing mode — different bounds from UART Echo Byte's own `4..=256`. RX
size (Table 48's own `1..=255`) needs no separate validation, matching UART
Echo Byte's own precedent (this service has no RX-side size-validation
mechanism for any protocol).

**Mock support (`j2534-0404-mock`).** New `is_honda_diagh_protocol` wired
into `ChannelState::new`'s `pins_assigned` gate (unassigned until explicit
`SET_CONFIG`, same as SW/FT/UART Echo Byte) — this is the only mock change
needed: `PassThruConnect` performs no protocol-id validation at all for a
non-`_CHx` id, and since this protocol never reaches `FIVE_BAUD_INIT`/
`FAST_INIT` (see the `is_kline` decision above), those IOCTL handlers' own
K-line acceptance gates need no new entry.

**Deferred, matching every prior phase's precedent:** Additional Channels
(`_CHx`) for this protocol. Discovery-cache connect-time enforcement wiring
for `DEVICE_INFO_HONDA_DIAGH_SUPPORTED` is now wired (ADR-185 Stage 1); its
`_SIMULTANEOUS` companion bit remains unwired.

New test file: `tests/grpc_mock/honda_diagh.rs` (10 tests, spanning connect
via resource id/raw id (both valid pins, an undocumented pin, no pins, two
pins), the ComParam allowlist contrast with UART Echo Byte (`DATA_RATE`
rejected here despite being universal elsewhere), the `CoptStartcomm`
optional-message behavior with its undersized-message rejection, and — edge-
case-hunter finding, verification pass — a `START`/`QUERY`/`STOP_REPEAT_
MESSAGE` smoke test proving the "no protocol-specific rejection needed"
claim below end-to-end rather than resting on spec/code reading alone.

**Verification-pass findings (edge-case-hunter, this phase):**
- **New row 0x023B's pin 14 is a genuine new resource-table conflict, not a
  test bug.** `rows_conflict` (`resources.rs`) flags any two distinct rows
  sharing a raw DLC pin number regardless of bus type, by design — clause
  13.2.3 itself flags exactly this collision (DIAG-H on pin 14 versus
  dual-wire CAN's CAN-L, also pin 14). Adding this row genuinely changed
  `GetConflictingResources`'s output for resource `0x0206` (`ISO_15765_2`)
  and the union of the four `SAE_J2610_SCI` rows to now include `0x023B`.
  Five pre-existing tests in `tests/grpc_mock/resources.rs` hardcoded the
  pre-diff resource count/conflict lists and needed updating to include the
  new row — the same "same-PR propagation duty" this codebase's own
  precedent already handles for UART Echo Byte's own pin-7 conflict
  (`resources.rs`'s doc comment on that ADR-170 update), just missed on the
  first implementation pass for pin 14. Fixed directly; all five tests
  (`unfiltered_get_resource_ids_returns_all_59_ids_in_table_order` and four
  `get_conflicting_resources_*`/`get_resource_status_and_conflicting_
  resources_resolve_table_resource_id` tests) now correctly include
  `0x023B` in their expected output.
- **Repeat Messaging's "no rejection needed" claim was previously verified
  only by spec/code reading, not by an end-to-end test** — closed by the
  new smoke test noted above.

## SAE J2534-2 SAE J1708 Protocol (ADR-175, Phase 11)

Brings SAE J2534-2 clause 17 (SAE J1708 Protocol: the heavy-duty-truck
serial bus, 9600bps, device-computed checksum unless disabled) into scope.
Full design and rejected alternatives in
[ADR-175](../../../docs/adr/ADR-175-j2534-2-j1708-phase11.md); this section is
the implementation-level summary. The third standalone, non-CAN-family
J2534-2 protocol this crate has shipped (after UART Echo Byte, ADR-170/Phase
9, and Honda DIAG-H, ADR-174/Phase 10) — clause 17 defines no unqualified
base id and no relationship to any protocol this service already models, so
it gets its own `ChannelProtocol::J1708_PS`, the same category
`HONDA_DIAGH_PS`/`UART_ECHO_BYTE_PS` already occupy.

**Multi-connector pin space, scoped to J1962 only.** Clause 6.3.3.2 (Table
3) defines three separate, protocol-agnostic pin-numbering spaces
(`J1962_PINS`, `J1939_PINS`, `J1708_PINS` — the dedicated 6-pin SAE J1708
Deutsch connector), and requires only "at least one" per `_PS` protocol.
ADR-156 (Phase 2a) explicitly deferred `J1939_PINS`/`J1708_PINS` to
whichever of Phase 5 (J1939) or this phase reached it first, noting ISO
22900-2's `PinData` has no connector discriminator at all — a code-scout
investigation this phase confirmed the J1962-coupling runs through every
layer of the existing Pin Selection mechanism (`names.rs`, `resources.rs`'s
`dlc_pins` tables, `ChannelKey`'s `pin_select` encoding), not a single
swappable constant. This phase scopes to J1962 only, per clause 6.3.3.2's
own "at least one" text — `J1939_PINS`/`J1708_PINS` support remains open,
needing the connector-discriminator/`ChannelKey`-widening design work a
future phase (or `design-advisor` session) would need to attempt.

**New `ChannelProtocol` and resource-table row.** `protocol.rs` gains
`ChannelProtocol::J1708_PS` (`0x0000800D`, matching Phase 0's
already-committed `PROTOCOL_J1708_PS` header constant, now also re-exported
from `j2534-0404/src/lib.rs`), self-mapping through `j2534_protocol_id()`.
`resources.rs` gains exactly ONE new resource-table row (`0x023C`, next in
the `0x0200` namespace after Honda DIAG-H's `0x023B`) — no OBD-family/
service-composite spec exists for this protocol either. New
`BUSTYPE_SAE_J1708` (`0x030D`) and `PINS_SAE_J1708` (J1962 pins 3/11, typed
`PIN_PLUS`/`PIN_MINUS` — the differential-pair typing already used for
J1850) with `hw_protocol_override: None`. **This pin pair has no textual
basis in clause 17 or clause 6 for the J1962 connector specifically** — it
is an editorial convenience default only, matching a commonly-cited
real-world heavy-truck scan-tool wiring convention, the least
spec-grounded default this codebase has picked so far; flagged in the
Prioritized Backlog for revisit if a primary SAE J1708
electrical-layer reference becomes available.

**ComParam allowlist is a superset of the universal default, unlike Honda
DIAG-H.** `comparam_support.rs` gains `is_j1708_param`
(`DATA_RATE`/`LOOPBACK`/`PARAM_MESSAGE_PRIORITY`, clause 17.3.2.2.1's closed
list) — unlike Honda DIAG-H, J1708's baud rate genuinely is
`SetComParam`-configurable (clause 17.2.2 gives a minimum-support default,
not a fixed value), so this protocol's exact-identity check in
`is_param_allowed` sits *after* the universal short-circuit (mirroring UART
Echo Byte's position), not before it (mirroring Honda DIAG-H's). The
pre-existing but previously-unwired `comparam_defaults.rs::sae_j1708_uart()`
preset (`DATA_RATE = 9_600`, clause 17.2.2) is now reachable: its
`bus_type_name`, `"SAE_J1708_UART"`, already matched an existing
case-insensitive `bustype_default_params` arm from an earlier phase, so no
new match arm was needed — only the resource row itself and a
`PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS` entry.

**New mechanism: `MSG_PRIORITY_VALUE` TxFlags (clause 17.4.5), the first
real native translation of `CP_MessagePriority`.** `PARAM_MESSAGE_PRIORITY`
(D-PDU `CP_MessagePriority`, native id `0x8083`) was already registered and
allowlisted on several CAN/KWP/J1850 bustype defaults, but had no native
TxFlags translation anywhere in this codebase until now.
`ComParamSet::msg_priority_tx_flags` (`service.rs`) reads the link's Active
`PARAM_MESSAGE_PRIORITY`, clamps to `1..=8` (absent/0/out-of-range all map
to `8`, the lowest priority, per clause 17.4.5's own text), and shifts it
into TxFlags bits 16-19 (`TX_FLAG_MSG_PRIORITY_VALUE`, newly re-exported
from `j2534-0404/src/lib.rs`).
`rpc_primitive.rs::apply_resolved_tx_flags` ORs this in, gated behind
`resources::is_j1708_protocol_id(hw_protocol_id)` — the same gating shape
`sw_can_tx_flags` already uses, since `PARAM_MESSAGE_PRIORITY` is
allowlisted on unrelated protocol families too and must not bleed onto
their TxFlags.

**Checksum flag needs no new code.** Clause 17.3.2.1.1's `CHECKSUM_DISABLED`
connect flag (bit 9) is numerically identical to the existing
`CONNECT_FLAG_ISO9141_NO_CHECKSUM` macro (flagged as a Phase 11 decision
point back in Phase 0's own progress notes). `rpc_link.rs::connect_flags`'s
existing ADR-050 policy (this service never generates/verifies ISO9141/
14230 checksums itself, and deliberately never sets that bit) already
applies to any protocol without a dedicated match arm via its `_ => 0`
fallback — direct reuse, no new arm added.

**Pin resolution (`names.rs`) has no closed-set check, unlike every
predecessor.** Unlike SWCAN/FT-CAN/Honda DIAG-H, clause 17 documents no
valid-pin table on the J1962 connector at all, so the new `J1708_PS` arm in
`resolve_pin_selection` is structurally simpler: `is_empty()` returns the
row's own default pins only when `resource_row.is_some()` (same "no
default, always explicit" gate as every predecessor), and the non-empty
branch runs `compute_pin_select` and accepts its result verbatim — 1 or 2
well-formed pins, no closed-set rejection. Confirmed spec-conformant by two
integration tests connecting via explicit alternate pins (a single pin, and
a two-pin pair) that are NOT the row's own default 3/11.

**TX/RX message size range (`protocol.rs`).** `tx_message_size_range` gains
a `J1708_PS` entry, `1..=4095` bytes (clause 17.4.3 Table 67) — by far the
largest range this codebase has implemented, single addressing mode. RX
size (Table 67's own identical `1..=4095`) needs no separate validation,
matching every prior phase's precedent (no RX-side size-validation
mechanism exists for any protocol).

**Mock support (`j2534-0404-mock`).** New `is_j1708_protocol` wired into
`ChannelState::new`'s `pins_assigned` gate (unassigned until explicit
`SET_CONFIG`, same as SW/FT/UART Echo Byte/Honda DIAG-H) — the only mock
change needed, matching every prior standalone-protocol phase's precedent.

**Repeat Messaging needs no new rejection.** Clause 14.1's default rule
(supported on all protocols unless a protocol's own clause explicitly
excludes it) applies — clause 17's text (§17.1-17.5) contains no such
exclusion, verified directly rather than assumed.

**RxStatus.** Clause 17.4.5 states only the `TX_MSG_TYPE` RxStatus bit is
meaningful for this protocol — no code change needed, ADR-098's existing
5-low-bit `RxFlag` forwarding already covers it.

**Deferred, matching every prior phase's precedent:** Additional Channels
(`_CHx`) for this protocol. Discovery-cache connect-time enforcement wiring
for `DEVICE_INFO_J1708_SUPPORTED` is now wired (ADR-185 Stage 1); its
`_SIMULTANEOUS` companion bit and the clause-6 `J1708_PS_J1962`/`_J1939`/
`_J1708` connector-validity queries remain unwired -- no Stage 1 or Stage 2
call site consults them. `J1939_PINS`/`J1708_PINS` connector support is
likewise deferred, per the multi-connector discussion above.

**Verification-pass finding (edge-case-hunter, this phase): the raw-id
route lost `sae_j1708_uart()`'s `DATA_RATE` default, the same bug class
Honda DIAG-H's own PR #63 round 2 fixed.** `rpc_create_com_logical_link`
only selects a bustype-default `ComParamSet` via `resource_row.bus_type_name`
— a `CreateComLogicalLink` naming the raw `J1708_PS` id directly (explicit
`dlc_pin_data`, no `bus_type_name`, the exact route `names.rs`'s own new
J1708 arm supports and requires) resolved `resource_row: None` and
`bustype_name: ""`, so the Working set stayed fully empty and
`ComParamSet::baud_rate()` fed `PassThruConnect` a native baud rate of 0
instead of clause 17.2.2's minimum-support 9600bps default. Fixed the same
way as Honda DIAG-H's own fix: a `None if
resources::is_j1708_protocol_id(protocol.j2534_protocol_id()) =>` match arm
in `rpc_link.rs` selecting `sae_j1708_uart()` directly, positioned right
after the Honda DIAG-H arm. Unlike Honda DIAG-H, J1708's `DATA_RATE` is
genuinely `SetComParam`-configurable, so a caller has a workaround this
specific bug — but the connect-time default should still be correct without
one, matching every other protocol's "sane defaults without a prior
`SetComParam`" model. Confirmed via a new regression test,
`connecting_the_raw_protocol_id_seeds_the_9600_baud_rate_default`
(`tests/grpc_mock/j1708.rs`), asserting the native connect call receives
9600 via the mock's baud-rate backdoor. The identical gap for
SW_CAN_PS/FT_CAN_PS/UART_ECHO_BYTE_PS's own raw-id routes (and, discovered
later, SAE J1939's) was subsequently closed by a single general mechanism,
`resources::bustype_default_name_for_hw_protocol_id`
(`j2534-0404-service/src/service/resources.rs`), generalizing this same
Honda DIAG-H/SAE J1708 fix to the four remaining protocol families (SWCAN,
FT-CAN, UART Echo Byte, SAE J1939). Each family's own regression tests for
this fallback live alongside its other tests in
`tests/grpc_mock/{sw_can,ft_can,uart_echo_byte,j1939}.rs`.

New test file: `tests/grpc_mock/j1708.rs`, covering connect via
resource-id/raw-id (default pins, an alternate single pin, an alternate
pin pair — proving the no-closed-set claim), the `DATA_RATE`-included
ComParam allowlist contrast with Honda DIAG-H, the `MSG_PRIORITY_VALUE`
TxFlags mechanism across in-range/out-of-range/absent priority values, TX
message-size boundaries (1/4095/4096 bytes), and the raw-id `DATA_RATE`
default regression test above.

**Verification-pass finding (Codex review, PR #64 round 6): the round-3
bustype-alias normalization fix (above) was itself lossy for the DW-CAN
family, resolving DWFTCAN aliases to DWCAN's `bus_type_id`.** That fix
preferred the first table row whose `legacy_bustype_hw_id` matched a
`map_bustype_name` hw id, reasoning that every row sharing one hw id also
shares one `bus_type_name`/`bus_type_id` — true for `BUSTYPE_SAE_J1708`
and `SCI_MODE`, but false for `map_bustype_name`'s generic `CAN` hw id,
which `ISO_11898_2_DWCAN` (`0x0301`) and `ISO_11898_3_DWFTCAN` (`0x030A`)
both map to despite being distinct physical buses with distinct table
rows. A DWFTCAN alias not spelled with the row's own exact underscore
separators (e.g. `"iso-11898-3-dwftcan"`, `"iso11898_3_dwftcan"`) silently
resolved to DWCAN's id via `resolve_object_id`'s `ObjtBustype` fallback's
`.find()` landing on the first CAN-hw-id row in table order — two names
for different physical buses collapsing onto one object id. The same
generic-hw-id ambiguity affected `candidates_matching_bustype`'s
(`GetResourceIds`) own fallback too, over-broadly matching every DWCAN
*and* DWFTCAN row instead of just the family the caller named. Fixed with
a new `canonical_bustype_name_for_alias` helper mapping only the aliases
that need disambiguation (DWCAN's and DWFTCAN's own spelling variants)
directly to their row's exact `bus_type_name`, consulted before the lossy
hw-id fallback in both call sites; every other alias (J1708, SCI, and
CAN-family spellings with no dedicated row of their own like
`ISO_11992_1_DWCAN`/`SAE_J1939_11_DWCAN`) is unaffected and keeps
resolving exactly as before. Regression tests:
`resolve_object_id_disambiguates_dwcan_from_dwftcan_aliases` and
`resolve_resource_ids_from_data_disambiguates_dwftcan_alias_from_dwcan_rows`
(`names.rs`).

## SAE J2534-2 Device Configuration (ADR-176, Phase 14; byte layout reverted to `IOBytearray` by ADR-178)

**Revision note (ADR-178):** Phase 14 originally shipped a dedicated
`IODeviceConfigList`/`IODeviceConfigEntry` proto message pair for this
IOCTL's payload. ADR-178 removed those messages entirely as part of
freezing `service.proto`'s interface: the same `{parameter_id, value}`
batched-pair payload now rides the pre-existing `DataItem.bytearray_data`
(`IOBytearray`) carrier as a hand-packed little-endian byte payload (`u32
entry_count`, then `entry_count` × `{u32 parameter_id, u32 value}`) instead
of a typed message. Everything below describing the native/mock layer,
opt-in gating, and persistence is unaffected by this change — only the
`DataItem` variant and how it's decoded changed; the paragraph on the
`DataItem` variant itself was rewritten accordingly.

Brings SAE J2534-2 clause 18 (Extended `PassThruIoctl` for Device
Configuration Parameters) into scope: two new `PassThruIoctl` commands,
`GET_DEVICE_CONFIG`/`SET_DEVICE_CONFIG`, reading/writing ten 4-byte
non-volatile storage slots on the pass-thru device itself. Full design and
rejected alternatives in
[ADR-176](../../../docs/adr/ADR-176-j2534-2-device-configuration-phase14.md)
(proto-field mechanism superseded by
[ADR-178](../../../docs/adr/ADR-178-freeze-j2534-2-dpdu-interface.md));
this section is the implementation-level summary.

**No protocol/resource-table work at all — the first J2534-2 phase this
crate has shipped with none.** Clause 18 has no `ProtocolID`, pin space, or
ComParam; it isn't scoped to a channel or CLL, only to the device itself.
`protocol.rs`/`resources.rs`/`names.rs`'s protocol-resolution machinery and
`comparam_support.rs` are all untouched.

**FFI level: nothing to do — Phase 0 already added everything.**
`IOCTL_GET_DEVICE_CONFIG`/`IOCTL_SET_DEVICE_CONFIG` (`0x00008007`/
`0x00008008`), `SCONFIG`/`SCONFIG_LIST`, and `CONFIG_NON_VOLATILE_STORE_1`
through `_10` (`0x0000C001`-`0x0000C00A`) were already in
`j2534_v0404.h` and every target's committed bindings since Phase 0; this
phase adds no header edit and runs no bindgen build.

**New DeviceID-scoped D-PDU IOCTL pair, module-level (`M`) like
`READ_J1962PIN_VOLTAGE`.** `PDU_IOCTL_GET_DEVICE_CONFIG`/
`PDU_IOCTL_SET_DEVICE_CONFIG` (`PDU_IOCTL_BASE + 0x18`/`+ 0x19`, the
24th/25th D-PDU commands this adapter implements) resolve
`module_handle -> device_id` via the existing `require_connected_device_for`
helper, the same one `ioctl_read_j1962_pin_voltage` already uses to reach a
native `DeviceId`. Gated on the connecting module's SAE J2534-2 opt-in
(clause 5), the identical `discovery::is_j2534_2_opted_in` check.

**`DataItem` carrier: `bytearray_data` (`IOBytearray`), holding a
hand-packed batched array of `{parameter_id, value}` pairs mirroring native
`SCONFIG_LIST` directly (ADR-178 correction — originally a dedicated
`IODeviceConfigList` message).** `rpc_misc.rs`'s `pack_device_config_entries`/
`unpack_device_config_entries` helpers convert between `Vec<(u32, u32)>` and
the wire bytes (`u32 entry_count` LE, then `entry_count` × `{u32
parameter_id, u32 value}` LE); `unpack_device_config_entries` rejects a
payload shorter than the 4-byte count, or whose remaining length doesn't
exactly equal `entry_count * 8`, as malformed (`INVALID_ARGUMENT`) — an
empty list is `entry_count = 0` (4 zero bytes), not a zero-length byte
array. One shape serves both directions and both request/response roles,
the same way native `SCONFIG` itself does: `GET_DEVICE_CONFIG`'s
`input_data` supplies entries with only `parameter_id` meaningful (`value`
ignored), and its `output_data` returns the same packed shape with `value`
populated; `SET_DEVICE_CONFIG`'s `input_data` supplies both fields, no
output.

**Parameter-id range enforcement lives in the native/mock layer, not the
service, mirroring `READ_J1962PIN_VOLTAGE`'s pin-range precedent, not
`is_param_allowed`'s ComParam-allowlist precedent.** `NON_VOLATILE_STORE_1`
through `_10` have no ISO 22900-2 ComParam behind them at all, so there is
no allowlist concern analogous to `SET_CONFIG`/`GET_CONFIG`'s — the
service forwards every `parameter_id`/`value` pair straight to
`j2534-0404`/the mock, exactly like `pin_number` is forwarded unvalidated
to `READ_J1962PIN_VOLTAGE`'s native call. An out-of-range `parameter_id`
comes back as the native `ERR_INVALID_IOCTL_PARAM_ID`, mapped through the
existing generic native-error path — this phase adds `ERR_INVALID_IOCTL_PARAM_ID`
its own arm in `j2534-0404-service/src/error.rs::pdu_error_for`, alongside
`ERR_INVALID_IOCTL_ID`/`ERR_INVALID_PROTOCOL_ID`'s existing
`PduErrIdNotSupported` mapping (`edge-case-hunter` finding, this phase: it
was previously unmapped and would have fallen through to the generic
`PduErrFctFailed` wildcard).

**`j2534-0404` wrapper: `get_device_config`/`set_device_config`, verbatim
mirrors of `get_config`/`set_config` except for `DeviceId` instead of
`ChannelId`.** Same `SCONFIG_LIST` marshaling; no new abstraction needed,
since `get_device_info`/`get_protocol_info` already established that a
`DeviceId.0` can be passed as `PassThruIoctl`'s handle argument exactly
like a `ChannelId.0` can.

**Mock persistence: survives `PassThruClose`/`PassThruOpen`, wiped only by
`__mock_reset`.** The ten-slot store (`MockState::non_volatile_store:
[u32; 10]`, defaulting to all-zero per Table 73's own default column)
lives on `MockState` (process-global), not `ChannelState` (torn down on
connect/disconnect) — nothing clears it on device open/close, mirroring
real non-volatile persistence; only the `__mock_reset` test backdoor wipes
it, the same rule every other `MockState` field already follows. A
`non_volatile_store_slot` helper maps the ten `CONFIG_NON_VOLATILE_STORE_x`
constants to indices `0..10` and is where the actual range check lives.
`SET_DEVICE_CONFIG` validates the whole batch before writing any slot
(matching `SET_CONFIG`'s own existing "reject the whole batch on one bad
param" precedent for FD channels) — a partially-invalid batch never
partially applies.

**Discovery-cache connect-time wiring deferred, matching every prior
phase's precedent.** Clause 18.5 requires the device to advertise support
via clause 25 Discovery; no `DEVICE_INFO_*`-gated enforcement is added at
IOCTL-dispatch time this phase.

New tests: appended to `tests/grpc_mock/pdu_ioctl.rs` (following Phase
13's own precedent of extending this file rather than adding a new one,
since clause 18 — like `READ_J1962PIN_VOLTAGE` — has no protocol/CLL
involvement at all): a set-then-get round trip on a single slot (proving
genuine storage across two separate `IoCtl` calls, not an in-request
echo), a batched multi-slot set-and-get in one call (proving the
array-of-pairs design actually batches), an out-of-range `parameter_id`
rejection, and the SAE J2534-2 opt-in gate rejection.

## SAE J2534-2 Analog Inputs (ADR-177, Phase 15; `analog_sample_rate` reverted to a ComParam by ADR-178)

**Revision note (ADR-178):** Phase 15 originally shipped a top-level
`CreateComLogicalLinkRequest.analog_sample_rate` request field (`uint32`,
sibling to the `resource` oneof). ADR-178 removed that field entirely as
part of freezing `service.proto`'s interface, re-expressing the native
clause 10.3.3.2.2 SAMPLE_RATE acquisition parameter as a project-minted
ComParam, `CP_AnalogSampleRate` (`PARAM_ANALOG_SAMPLE_RATE`, id `0x80C4`,
`service_params.rs`) — staged via `SetComParam` on the CLL's Working set
and validated at `ConnectComLogicalLink` time instead. This also moved the
required-nonzero validation from `CreateComLogicalLink` time to
`ConnectComLogicalLink` time (an observable behavior change: a client
relying on `CreateComLogicalLink`'s own success as "the resource is valid"
no longer gets that signal for a missing rate specifically) and replaced
the join-mismatch check's comparison target: instead of comparing a second
CLL's requested rate against a first CLL's own immutable per-link field
(the removed `LogicalLinkState.analog_sample_rate`), it now compares
against `SharedChannel::applied_analog_sample_rate` — the rate actually
applied via `SET_CONFIG` at the moment the physical channel was created,
recorded once and never re-read — since a staged ComParam is re-stageable
at any time post-connect and comparing two live Working values again would
let an owner's post-connect re-stage (with no reconnect) silently desync
the check from what hardware is actually running. A new guard was also
added at `CoptUpdateparam` time (`rpc_primitive.rs`): re-staging
`CP_AnalogSampleRate` away from the applied rate on an already-connected
analog link is rejected outright, mirroring the pre-existing CAN FD
`CoptUpdateparam` guard's "reject rather than silently ignore" pattern.
Everything below describing the resource-table shape, the mock, the
write/filter/repeat-message rejections, and the accepted residuals is
unaffected by this change — only the rate's carrier, its validation
timing, and the join/`CoptUpdateparam` comparison target changed; the
paragraphs describing those specifically were rewritten accordingly (the
"Codex review finding, PR #66" paragraph below is now describing a defect
in a mechanism this ADR has since replaced, kept for historical context).

Brings SAE J2534-2 clause 10 (Analog Inputs) into scope: 32 independent,
read-only native ProtocolIDs (`PROTOCOL_ANALOG_IN_1`..`PROTOCOL_ANALOG_IN_32`,
already in `j2534_v0404.h`/every target's committed bindings since Phase 0 —
no bindgen work this phase), each connecting independently and delivering
device-queued 4-byte signed little-endian millivolt readings via
`PassThruReadMsgs` only. Full design and rejected alternatives (including
the `design-advisor` consult this phase's resource-model question needed,
`docs/j2534-2-support-plan.md` §6's coarse "No" verdict notwithstanding) in
[ADR-177](../../../docs/adr/ADR-177-j2534-2-analog-inputs-phase15.md); this
section is the implementation-level summary.

**32 static resource-table rows (`0x023D`-`0x025C`), the same "N distinct
native ids -> N rows differing only by `hw_protocol_override`" shape
SWCAN/FT-CAN already established, not a new resource-model mechanism.**
Unlike SWCAN/FT-CAN/UART Echo Byte/Honda DIAG-H/J1708, all 32 rows share
ONE new `ChannelProtocol::ANALOG_IN` identity (the SAE J2610 SCI shape: one
shared identity fanning out to N native ids via each row's own
`hw_protocol_override`) — a genuinely new, first-of-its-kind resource
family with no ISO 22900-2 anchor at all (ISO 22900-2, both editions,
defines no resource/ComParam concept resembling multi-channel analog
acquisition). No `dlc_pins`/pin table on any of the 32 rows — clause 10 has
no pin concept whatsoever — and a new synthetic bus type
(`BUSTYPE_ANALOG_IN`, `0x030E`) with no ISO 22900-2 source, documented as
such rather than implying a spec basis that doesn't exist (mirroring
J1708's own `PINS_SAE_J1708` precedent for flagging a non-spec-grounded
choice explicitly).

**`CP_AnalogSampleRate` ComParam (native clause 10.3.3.2.2 SAMPLE_RATE,
`PARAM_ANALOG_SAMPLE_RATE = 0x80C4`, ADR-178), staged via `SetComParam` and
required nonzero when connecting to one of the 32 Analog Input resources**
(clause 10.3.3.2.2's own zero default means the acquisition subsystem is
disabled — a bare deferral, ADR-170-style, would connect successfully and
then read nothing forever). `comparam_defaults::analog_in` seeds it at `0`
(unset) in the Working default set; `comparam_support::is_param_allowed`'s
ANALOG_IN branch allows exactly this one ComParam for one of the 32
`ANALOG_IN_x` protocol ids and rejects it for every other resource at
`SetComParam` time (`PDU_ERR_COMPARAM_NOT_SUPPORTED`) — the allowlist
itself is the exclusivity mechanism, mirroring `channel_index`'s own
exclusivity treatment above but enforced one RPC earlier than a
request-field check could be. Required-nonzero validation runs in
`rpc_connect_com_logical_link`'s existing Working-snapshot block
(`rpc_link.rs`), reading `PARAM_ANALOG_SAMPLE_RATE` off the CLL's Working
`ComParamSet` right there, using the resolved native hw id
(`link.hw_protocol_id`, already Plane-A-resolved by that point) —
`resources::is_analog_in_protocol_id` is the single canonical check every
call site (this connect-time validation, the write/filter rejections
below, the ComParam-allowlist gate) uses for this protocol's identity,
taking the resolved native id rather than `protocol.value()` directly
(which is the same, shared, non-native `ChannelProtocol::ANALOG_IN`
value for every one of the 32 rows). The connecting module must also have
opted into SAE J2534-2 (clause 5) — gated in `names.rs`'s
`resolve_pin_selection`, the same choke point SWCAN/FT-CAN/UART Echo
Byte/Honda DIAG-H/J1708 gate their own opt-in requirement in, even though
an Analog Input connect supplies no `dlc_pin_data` and would otherwise
reach that function's generic tail with no opt-in check at all; a
non-empty `dlc_pin_data` on an Analog Input resource is rejected outright
(clause 10 has no clause-6 Pin Selection vocabulary to apply).

**Codex review finding, PR #66:** `analog_sample_rate` originally lived on
`ResourceData` as described just above, reachable only via the `RscData`
`resource` oneof variant. Since `GetResourceIds` returns a bare
`resource_id`, and the normal discovery flow feeds that straight into
`CreateComLogicalLink(Resource::ResourceId(...))` with no `RscData`
involved at all, this made every one of the 32 Analog Input resources
unreachable through their own discoverable ids — only the undocumented
`RscData`-wrapped route (which this phase's own tests happened to use)
could ever supply a rate. Fixed by moving `analog_sample_rate` to a new
top-level field on `CreateComLogicalLinkRequest` itself (sibling to the
`resource` oneof, the same way `cll_create_flag` already sits
there), so it applies uniformly regardless of which `resource` variant
(`RscData`/`ResourceId`/`ResourceName`) selects the target. The
requirement/exclusivity behavior described above is unchanged, just
checked against the new top-level field; see ADR-177's "Codex review
correction" note.

**Applied via `SET_CONFIG(CONFIG_SAMPLE_RATE)` immediately after
`PassThruConnect` succeeds, mirroring CAN FD's own connect-time
`CONFIG_FD_CAN_DATA_PHASE_RATE` SET_CONFIG shape (ADR-158) exactly —
including the rollback-and-map-native-error failure path
(`map_native_error_for_link`/`pdu_error_for`, no bespoke error path).**
`rpc_connect_com_logical_link`'s existing Working-snapshot block reads
`PARAM_ANALOG_SAMPLE_RATE` off the CLL's Working `ComParamSet`, validates
it nonzero right there (see above), and threads it into
`NewPhysicalChannelParams.analog_sample_rate: Option<u32>` (`Some(rate)`
only when opening a physical channel with one of the 32 native ids) for
`connect_new_physical_channel` to apply — the same single Working read is
also what gets recorded onto `SharedChannel::applied_analog_sample_rate`
(ADR-178), never a second independent read. **`edge-case-hunter` finding,
fixed (not left as a documented residual); comparison target revised by
ADR-178:** unlike `fd_data_phase_rate`, `analog_sample_rate` is NOT folded
into `ChannelKey` (the physical-channel-sharing tuple is a fixed 4-tuple;
widening it to a 5-tuple would ripple across every other `ChannelKey`
construction site in this crate, 8+ call sites, for a need scoped to
exactly one protocol family). Without a second guard, this would let a
second CLL independently connecting to the exact same `ANALOG_IN_x`
resource with a different sample rate silently join the first CLL's
physical channel with its own rate request never applied — no error, no
signal, mirroring the pre-ADR-158 CAN FD gap before that same
consideration was added for FD. Clause 10 gives no reason two CLLs should
ever share one acquisition subsystem at different rates the way CAN's
physical-bus sharing is legitimate, so instead of widening `ChannelKey`,
`rpc_connect_com_logical_link`'s existing "reject the join" pattern
(already used for the client-filter conflict just above it) gained a
matching check: joining an Analog Input channel whose recorded
`SharedChannel::applied_analog_sample_rate` differs from this CLL's own
staged rate is rejected (`PDU_ERR_FCT_FAILED`), the same shape as the
filter-conflict rejection. **ADR-178 revised the comparison target from a
live per-CLL field scan to this recorded applied value** specifically
because `CP_AnalogSampleRate` became re-stageable post-connect once it
moved to a ComParam — comparing two live Working values again would let an
owner's post-connect `SetComParam` re-stage (with no reconnect) silently
desync the join check from what hardware is actually running; see
`SharedChannel::applied_analog_sample_rate`'s own doc comment
(`service.rs`). Two CLLs whose staged rate matches the recorded applied
value still join normally, including a joining CLL staged at the
originally-applied rate even after the owner has since re-staged (but not
yet applied, since that would need a reconnect) a different one.

**`CoptUpdateparam` guard (ADR-178, `rpc_primitive.rs`):** re-staging
`CP_AnalogSampleRate` away from `SharedChannel::applied_analog_sample_rate`
on an already-connected analog link is rejected outright
(`INVALID_ARGUMENT`) rather than silently promoted, mirroring the
pre-existing CAN FD `CoptUpdateparam` guard
(`fd_mode_staged(&params) != resources::is_fd_protocol_id(hw_protocol_id)`)
— silently ignoring the re-stage would let `GetComParam` report a Working
value hardware was never actually reconfigured to match.
`update_param_working_snapshot`'s return tuple gained a 4th element,
`channel_key: Option<ChannelKey>` (this CLL's own
`LogicalLinkState::channel_key`, read in the same `logical_links`
snapshot), so the guard can look up this CLL's `SharedChannel` in a
separate, later `shared_channels` acquisition — sequential, not nested,
since `update_param_working_snapshot`'s own `logical_links` guard has
already dropped by the time that lookup runs. A `channel_key` miss (a
disconnect racing between the snapshot and this lookup) is not treated as
an error here — `handle_update_param`'s own `connect_generation`/ADR-086
staleness check already catches that case at execution time, so the guard
skips silently rather than rejecting a legitimate request on a benign
race.

**This service's own internal pass-all-filter installation (every
non-ISO15765 connect, ADR-008) is also skipped for an Analog Input
channel** — found only by running the new end-to-end tests: clause 10 has
no filter concept for the mock's own spec-conforming `PassThruStartMsgFilter`
rejection to accept even this service's internal RX-delivery-enablement
filter, so `connect_new_physical_channel`'s existing
`base_proto_id != ISO15765` gate gained a second exclusion,
`!resources::is_analog_in_protocol_id(base_proto_id)`, alongside it.

**ComParam allowlist accepts exactly `CP_AnalogSampleRate` (ADR-178) — not
even the universal `DATA_RATE`/`LOOPBACK` pair — checked before
`is_universal_param`'s short-circuit, mirroring Honda DIAG-H's own
precedent for a narrower-than-universal list.** Clause 10 defines no
native ComParam-shaped concept at all; the closed allowlist still closes
off `CP_TesterPresentSendType` by construction, the same mechanism
ADR-170 Decision 3 established for UART Echo Byte's own closed list — this
is how "periodic messages are disallowed on this protocol" falls out for
free, without a bespoke runtime guard. **Revised by ADR-178:** the
allowlist used to be empty (Phase 15's original shape); it now allows this
one ComParam so the sample rate can be staged at all now that its carrier
moved off the request message. `comparam_defaults::bustype_default_params("analog_in")`
now seeds `PARAM_ANALOG_SAMPLE_RATE` at `0` (unset/unstaged) rather than
returning `Some(ComParamSet::default())` verbatim — this file's own
`every_resource_row_has_protocol_and_bustype_defaults_or_is_allowlisted`
regression gate requires every resource row's `bus_type_name` to resolve,
so an entry existed purely to satisfy that invariant before ADR-178, and
now additionally carries the real seeded default. The 32
`ANALOG_IN_1`..`ANALOG_IN_32` `protocol_name`s are listed in
`comparam_defaults::PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS` for the
identical structural reason.

**Write (`CoptSendrecv`/`CoptStartcomm`/a non-empty `CoptStopcomm
cop_data`), `PDU_IOCTL_START_MSG_FILTER`, and `PDU_IOCTL_START_REPEAT_MESSAGE`
are all rejected `PDU_ERR_ID_NOT_SUPPORTED` on a connected Analog Input
link, enforced adapter-side (service-layer rejection, not a native-error
passthrough).** The write rejection lives in `rpc_start_com_primitive`
(`rpc_primitive.rs`), gated on the same `transmits` classification the
function already computes per `cop_type` (a receive-only `CoptSendrecv`
with `num_send_cycles == 0` is unaffected) — mirroring the exact rejection
pattern `rpc_misc.rs`'s UART Echo Byte + Repeat Messaging rejection uses.
The filter rejection lives in `ioctl_start_msg_filter` (`rpc_misc.rs`),
positioned right after the existing ISO15765 filter-type check and
running unconditionally (not scoped to "already connected"), the same way
that check does. **Codex review finding, PR #66:** the original PR missed
`PDU_IOCTL_START_REPEAT_MESSAGE` (`rpc_misc.rs::ioctl_start_repeat_message`)
— a *separate* IOCTL dispatch path that never goes through
`rpc_start_com_primitive`'s `transmits` gate at all, so a client could
still start a device-autonomous repeat transmit on a link clause 10
defines as strictly read-only. Fixed the same way, mirroring the
UART Echo Byte rejection immediately above it in the same function; the
mock's own `IOCTL_START_REPEAT_MESSAGE` handler also gained a matching
`ERR_NOT_SUPPORTED` check (it was not caught by the existing
`!pins_assigned` guard, since Analog Input is not a `_PS` protocol and so
`pins_assigned` defaults `true` at channel creation).

**Mock (`j2534-0404-mock`): accepts all 32 ids on `PassThruConnect`
(already-existing "no protocol-id validation on a plain connect"
behavior, unchanged); `PassThruWriteMsgs`/`PassThruStartPeriodicMsg`/
`PassThruStartMsgFilter`/`IOCTL_START_REPEAT_MESSAGE` return
`ERR_NOT_SUPPORTED` for any of the 32 ids (defense-in-depth: the service
layer already rejects client requests before they reach the mock, but a
spec-conforming device must reject them too); `SET_CONFIG`/
`GET_CONFIG(CONFIG_SAMPLE_RATE)` need no special-case
code at all (the existing generic per-channel params map already stores
and reads any parameter id, defaulting to `0`/disabled); once a channel's
rate is armed, `PassThruReadMsgs` synthesizes a fixed 4-byte signed-LE
millivolt reading (`MOCK_ANALOG_READING_MV = 2500`) tagged with the
channel's own native protocol id whenever its RX queue is otherwise empty
— deterministic and test-friendly, not a real sample-rate timing
simulation. `DEVICE_INFO_ANALOG_IN_SUPPORTED`/`_SIMULTANEOUS` report a flat
`Supported = 1`/`Value = 0x0000_0001`, the same shape the SCI variants use
(no `_CHx`-capacity concept applies here).**

**Accepted residuals (this phase, matching every prior phase's own
precedent):**
- `ACTIVE_CHANNELS`/`SAMPLES_PER_READING`/`READINGS_PER_MSG`/
  `AVERAGING_METHOD` (clause 10.3.3.2's other four acquisition parameters)
  and the three read-only capability parameters
  (`SAMPLE_RESOLUTION`/`INPUT_RANGE_LOW`/`INPUT_RANGE_HIGH`) stayed at
  native/mock defaults, unexposed to D-PDU clients, through this phase --
  now closed by ADR-216 (`CP_AnalogActiveChannels`/`CP_AnalogSamplesPerReading`/
  `CP_AnalogReadingsPerMsg`/`CP_AnalogAveragingMethod`/`CP_AnalogSampleResolution`/
  `CP_AnalogInputRangeLow`/`CP_AnalogInputRangeHigh`).
- Discovery-cache connect-time enforcement wiring for
  `DEVICE_INFO_ANALOG_IN_SUPPORTED` is now wired (ADR-185 Stage 1);
  `_SIMULTANEOUS` remains unwired -- no Stage 1 or Stage 2 call site
  consults it.
- Additional Channels (`_CHx`) do not apply — clause 10's 32 channels are
  enumerated directly, with no base/additional-channel relationship for
  `_CHx` to model.

Test file: `tests/grpc_mock/analog_inputs.rs` (new resource family,
following this suite's own "one file per protocol phase" convention,
mirroring `j1708.rs`/`honda_diagh.rs`'s structure; rewritten by ADR-178 to
stage `CP_AnalogSampleRate` via `SetComParam` before `ConnectComLogicalLink`
instead of setting a `CreateComLogicalLinkRequest` field) — connect with a
valid nonzero `CP_AnalogSampleRate` staged (asserts the native
`SET_CONFIG(CONFIG_SAMPLE_RATE)` call and confirms no `CONFIG_J1962_PINS`
is ever issued), connect with no rate staged rejected at
`ConnectComLogicalLink` (create still succeeds — the rejection moved from
create- to connect-time), staging `CP_AnalogSampleRate` on a
non-Analog-Input resource rejected at `SetComParam` itself (both via
`RscData::ProtocolId` and via an ordinary resource-id-routed table row), a
receive-only-monitor read retrieving the mock's synthetic reading, write
rejection with `PDU_ERR_ID_NOT_SUPPORTED`, `PDU_IOCTL_START_MSG_FILTER`/
`PDU_IOCTL_START_REPEAT_MESSAGE` rejection with `PDU_ERR_ID_NOT_SUPPORTED`,
resource-name resolution (`"ANALOG_IN_17"`), the join-rate-mismatch
pair (different rate rejected / same rate joins), the bare-`ResourceId`/
top-level-`ResourceName` regression pair, and three ADR-178-specific
regression tests: a joining CLL staged at the originally-APPLIED rate
still succeeds even after the owner has re-staged (but not reconnected) a
different rate on its own Working set
(`join_after_owner_restages_a_different_rate_without_reconnecting_still_uses_the_applied_rate`),
a joining CLL staged at that owner's newly-staged-but-unapplied rate is
still rejected
(`join_with_owners_newly_staged_but_unapplied_rate_is_still_rejected`), and
`CoptUpdateparam` rejects re-staging `CP_AnalogSampleRate` away from the
applied rate on an already-connected link
(`coptupdateparam_rejects_restaging_analog_sample_rate_on_a_connected_link`).

## SAE J2534-2 SAE J1939 Protocol (ADR-179, Phase 5)

Clause 16 covers the SAE J1939 heavy-duty-vehicle protocol: 250 kbps fixed,
`CAN_29BIT_ID` only, and a device-owned address claim/defend negotiation
(SAE J1939-81) — the first J2534-2 phase with a genuinely asynchronous,
multi-round COP negotiation rather than a single-call variant substitution
(contrast the SAE J1850 VPW/PWM auto-detect or CAN FD substitution, both of
which resolve entirely within one `ConnectComLogicalLink` call).

**Standalone `ChannelProtocol`, not a CAN bus-type variant.** Unlike SWCAN/
FT-CAN (which reuse `ChannelProtocol::Can` via `hw_protocol_override`),
J1939 gets a new, self-mapping `ChannelProtocol::J1939_PS` (the UART Echo
Byte/Honda DIAG-H/J1708 shape) — its 5-byte message framing (a CAN-ID
prefix, not a plain CAN ID), source-address unique-response rule, and
claim state machine are not CAN's own, so reusing `Can`'s identity would
have scattered `is_j1939` special cases through CAN-family code paths
instead of confining them to one new identity.

**Two resource rows, reusing pre-existing-but-unwired ComParam
scaffolding.** `comparam_defaults.rs` already had `j1939_can_common()` (a
shared J1939 ComParam-default helper) and two protocol-name presets built
on it, `iso_obd_on_sae_j1939_73()`/`sae_j1939_73_on_sae_j1939_21()`, from
an earlier general ISO 22900-2 ComParam-table authoring pass that predates
this phase entirely — unreachable until now, since no resource-table row
or `ChannelProtocol` identity ever named them. Two new rows (`0x025D`-
`0x025E`), one per preset, share one new standalone bus type,
`SAE_J1939_11_DWCAN` (`0x030F`) — not a CAN-family bus-type variant either,
reusing the also-pre-existing-but-unwired `sae_j1939_11_dwcan()` 250kbps
physical-layer default. No default pins (per clause 16.3.2.1, the physical
layer is not connected until an explicit `SET_CONFIG` is issued) — connect
always issues an explicit `SET_CONFIG(CONFIG_J1962_PINS)`, and
`names::resolve_pin_selection`'s new arm rejects empty `dlc_pin_data`
unconditionally (unlike J1708/Honda DIAG-H/UART Echo Byte, which fall back
to a row default when one exists — J1939's own rows have none).

**Address-claim/defend state machine** (`events_j1939_claim.rs`, new file):
`PDU_COPT_STARTCOMM` drives the claim when `CP_J1939AddressNegotiationRule`
bit 1 requests it (the spec default, confirmed against ISO 22900-2's own
inverted-polarity bit description). For each candidate address in
`CP_J1939PreferredAddress`'s staged list, in order: issues
`IOCTL_PROTECT_J1939_ADDR` (never byte[0] = 254 or 255 — both are rejected
by the native call per clause 16.3.3.2, and 255 is only the conceptual
power-on default), then actively drives RX polling — bounded by
`CP_J1939AddrClaimTimeout` (already seeded, 1,250,000 µs) — for a matching
`RX_FLAG_J1939_ADDRESS_CLAIMED`/`_LOST` indication for that address. These
are **native RxStatus bits** (clause 16.4.6 Table 63, bits 16/17,
overloaded with SWCAN's `SW_CAN_HS_RX`/`_NS_RX` and FT-CAN's `LINK_FAULT`
at the same positions, disambiguated by `resources::is_j1939_protocol_id`
in `poll_rx_inner`'s withhold arm the same way the SWCAN arm already
disambiguates by `is_sw_protocol_id`) consumed from the ordinary RX poll
path, not events this service synthesizes itself — corrected mid-design
from an initial misreading in the design-advisor brief that treated them
as locally-computed D-PDU indication flags (see ADR-179's Context section).
`_LOST` advances the candidate cursor and retries; list exhaustion fails
the StartComm COP (`PduErrEvtInitError` + `PduCopstFinished`, mirroring
this function's existing init-failure pattern). `_CLAIMED` writes the
claimed address back into `CP_TesterSourceAddress` (native `NODE_ADDRESS`,
already an established alias in this codebase) on both Working and Active,
for client `GetComParam` readback.

A per-physical-channel routing map, `SharedChannel::j1939_claims: HashMap<u8,
(cll_handle, connect_generation)>`, resolves each indication (which carries
only the affected address in `Data[0]`, `DataSize`=1) back to its owning
CLL — multiple sibling CLLs sharing one channel may each hold a distinct
claim, since the device must protect at least 10 addresses (clause 16.2.2).
Entries are tagged with the owning CLL's existing `connect_generation`
(ADR-086); a stale indication surviving that CLL's disconnect/reconnect
fails the generation comparison and is dropped, rather than misattributed
— reusing the established guard mechanism, not a new one. A later
spontaneous `_LOST` (the device was out-defended mid-session) re-enters the
retry loop from `run_due_tick_duties`'s periodic per-tick pass
(`run_j1939_reclaim_duties`), from `cursor = 0`; exhaustion there surfaces
a CLL-scoped error event instead of a COP failure (no live COP is driving
it). On a J1939 CLL's disconnect while its shared physical channel
survives, every address it owns is best-effort cancelled (the zero-NAME
cancel form) and dropped from the routing map (`cancel_j1939_claims_for_cll`,
wired into both `DisconnectComLogicalLink`/`DestroyComLogicalLink`,
mirroring `client_filters`/`repeat_message_ids`'s own teardown shape at the
same call sites).

**`CP_J1939TargetAddress == 0xFFFF` fails `StartComPrimitive`
synchronously** (`rpc_primitive.rs`) — a separate, spec-mandated gate from
the claim loop itself, checked unconditionally for every J1939 CLL before
`cop_handle` allocation proceeds further.

**`ERR_ADDRESS_NOT_CLAIMED` is native-authoritative, not duplicated
service-side.** This service's own claim-state cache can be stale in the
claimed-to-lost direction (defense is asynchronous and device-internal), so
a service-side pre-check could pass while the device correctly rejects —
it can never be more than a false gate. The native write failure is mapped
through the existing generic native-error path (already display-mapped in
`j2534-0404/src/error.rs` since Phase 0); `transmit_request`'s own generic
`PduErrEvtTxError` folding of every native write failure (not J1939-specific,
pre-existing behavior for every protocol) means the COP surfaces a generic
TX-error event rather than an `ERR_ADDRESS_NOT_CLAIMED`-specific one — this
is the same collapsed granularity every other protocol's native write
failure already has, not a new gap this phase introduced.

**Timer ComParam translations — non-ordinal, closing the shortname-collision
gap PR #71 left open.** Five ComParams this codebase already had (`CP_Cr`/`PARAM_N_CR`,
`CP_T5Max`/native `T5_MAX`, `CP_T4Max`/native `T4_MAX`, `CP_Bs`/`PARAM_N_BS`,
`CP_Cs`/`PARAM_N_CS`) get their first native translations in
`comparam_id.rs::to_j2534_config_id`, gated on `is_j1939_protocol_id`:
`CP_Cr`→`CONFIG_J1939_T1`, `CP_T5Max`→`CONFIG_J1939_T2`,
`CP_T4Max`→`CONFIG_J1939_T3`, `CP_Bs`→`CONFIG_J1939_T4`,
`CP_Cs`→`CONFIG_J1939_BRDCST_MIN_DELAY` — verified directly against ISO
22900-2:2022's own text, **not** an ordinal T1↔T1/T2↔T2/... correspondence.
`PARAM_T3_MAX`/`_T4_MAX`/`_T5_MAX` (`service_params.rs`) are retired
entirely: every reference now uses the native `ComParamId(j2534_0404::
T3_MAX/T4_MAX/T5_MAX)` directly, since ISO 22900-2 defines `CP_T3Max`/
`CP_T4Max`/`CP_T5Max` as one ComParam each with a per-protocol default
(SAE_J2610_SCI vs. SAE_J1939_21) — the same shape this codebase already
uses correctly elsewhere — not two distinct constants, resolving the
shortname collision with the pre-existing native SCI mapping PR #71 left
unregistered. `CP_T3Max` (native `T3_MAX`) and `CP_Br`/`PARAM_N_BR` have no
native `CONFIG_J1939_*` counterpart at all (Table 60 defines neither) and
stay ComParam-only, an accepted residual.

**Message framing** (`tx_header.rs`, new functions): `j1939_header_bytes`
composes the outbound 5-byte prefix — a 29-bit CAN ID (priority/data
page/PDU format/PDU-specific-or-target-address depending on the PF≥240
peer-to-peer-vs-broadcast split, clause 16.4.3/16.4.4) plus a destination-
address byte (`CP_J1939TargetAddress`, `0xFF` selects BAM/broadcast) —
ahead of client-supplied payload, the same shape every other
protocol-specific composer in this file uses. `PARAM_J1939_SOURCE_ADDRESS`-based
UniqueRespIdTable routing (wiring a received frame's source address into
`route_frame`/`build_cll_rx_entries`) was originally deferred here, then
wired later by [ADR-184](../../../docs/adr/ADR-184-j1939-unique-resp-id-source-address-matching.md);
that ADR's own `UniqueRespIdKey::matched()` precedence method superseded
and removed the original standalone `j1939_source_address` parser this
paragraph used to describe. BAM/Connection-Management
multi-frame segmentation (SAE J1939-21) is handled entirely by the
adapter once an address is claimed (clause 16.1) — this service passes the
up-to-1790-byte buffer straight through, no service-side segmentation.

**Mock support** (`j2534-0404-mock`): simulates `IOCTL_PROTECT_J1939_ADDR`
(claim/cancel forms, 254/255 rejection, synchronous RX-queue delivery of
the CLAIMED indication) and the native `ERR_ADDRESS_NOT_CLAIMED` write
check; a `__mock_set_j1939_claim_lost` backdoor forces every claim
attempt to lose, and `__mock_set_j1939_claim_lost_count(n)` only the next
`n`, for testing the retry-list behavior.

Deferred, matching every prior phase's precedent unless noted: `_CHx`
Additional Channels; `CONFIG_J1939_PINS` (the SAE J1939-13 connector — the
same multi-connector deferral J1708/ADR-156 already flagged, "at least one"
connector being spec-conformant); NAME-based (as opposed to address-based)
target/source resolution; Discovery-cache connect-time enforcement wiring.
See [ADR-179](../../../docs/adr/ADR-179-j2534-2-j1939-phase5.md) for the full
design rationale, including two corrections made mid-design (the RxStatus-
vs-RxFlag misreading above, and the byte[0]=255 clarification) and the
non-ordinal timer-mapping table.

Integration tests (`tests/grpc_mock/j1939.rs`): claim success on StartComm
(verified via the claimed address reaching a subsequent `CoptSendrecv`
frame's wire-visible source-address byte — `GetComParam(CP_TesterSourceAddress)`
was initially rejected by `is_j1939_param`'s allowlist, a real gap this
phase's own test-writing pass caught and fixed by admitting it via
ADR-179 Decision 3's own client-readback fix); retry-over-the-candidate-list on a forced LOST;
full-list exhaustion failure; the `CP_J1939TargetAddress == 0xFFFF`
synchronous rejection; `ERR_ADDRESS_NOT_CLAIMED` on an unclaimed source
address (asserted via the generic `PduErrEvtTxError` this protocol shares
with every other protocol's native write failure, not a J1939-specific
event — none exists, see above); the composed 5-byte wire prefix,
byte-for-byte; disconnect-while-sibling-survives not erroring or
disturbing the sibling CLL (a lighter check than direct claim-cancellation
observation — the mock exposes no accessor for its own claimed-address
set to the test harness); and two regression tests added for Codex review
findings (PR #72 round 1, now fixed and out of the backlog): the RX-side
5-byte prefix split (`header_footer_len`'s missing `PROTOCOL_J1939_PS`
arm), and a joining sibling CLL correctly skipping (not stealing) an
address a live sibling already claimed (`owned_by_a_live_sibling`) — a
third finding's fix (`build_cll_rx_entries`'s `unique_resp_ids` construction
changed from `.map()` to `.filter_map()` to drop a table entry lacking
both USDT/UUDT ids) had no J1939-specific integration-test coverage at the
time, since `comparam_support::unique_id_params` had no `J1939_PS` branch
yet and `SetUniqueRespIdTable` rejected every param for a J1939 CLL
outright; [ADR-184](../../../docs/adr/ADR-184-j1939-unique-resp-id-source-address-matching.md)
later added that branch, making this path independently J1939-testable —
see its own `unique_resp_id_table_*` regression tests. A round 3 regression test proves a claim-enabled `CoptStartcomm`'s
optional message reaches the wire with its header recomposed against the
just-claimed source address (ADR-180 Decision 3). A round 4 regression test
proves a sibling CLL can claim an address only after the CLL that
previously held it moved off via `StopComm`+`CP_J1939PreferredAddress`
change (ADR-180 Decision 4) — the suite is now 12 tests. A round 5 regression test proves a
sibling CLL can claim an address only after a spontaneous loss for it is followed by a fresh
reclaim landing on a different (earlier) candidate (ADR-180 Decision 5) — the suite is now 13
tests. A round 6 regression test proves an out-of-range `CP_J1939TargetAddress` is rejected
synchronously (ADR-180 Decision 6's own two tester-present fixes -- recomposing `tester_present_data`'s
5-byte header against a just-claimed/reclaimed address, mirroring Decision 3's `CoptStartcomm`
fix -- proved unreachable via the live RPC surface and have no dedicated test: `comparam_support::
is_j1939_param` admits no `CP_TesterPresent*` ComParam, so `SetComParam` rejects configuring
tester-present on a J1939 CLL outright, and `CP_TesterPresentHandling` is never seeded either, so
`resolve_tester_present` always short-circuits before composing `tester_present_data` for J1939) —
the suite was 14 tests. Three
round 7 regression tests prove `CoptUpdateparam` also synchronously rejects an out-of-range
`CP_J1939TargetAddress` (unconditionally) and a sentinel one (once the CLL is genuinely
`comm_started`), plus the negative case an `edge-case-hunter` pass caught before it shipped — an
unrelated `CoptUpdateparam` on a not-yet-started CLL must NOT be rejected just because
`CP_J1939TargetAddress` is still at its legitimate `0xFFFF` default (ADR-180 Decision 7; Decision
8's reserved-candidate skip -- `run_j1939_claim_loop` now skips a `CP_J1939PreferredAddress`
candidate of 254/255 before issuing a native claim for it, rather than relying on the mock's own
synchronous rejection -- has no dedicated test: the native rejection has zero observable side
effects, so "skip locally" and "issue-then-reject" are indistinguishable via this harness) —
the suite was 17 tests. Two round 8 regression tests prove `SetComParam` now synchronously rejects
an out-of-range `CP_TesterSourceAddress` on a J1939 CLL and out-of-range `CP_J1939PDUFormat`/
`CP_J1939PDUSpecific` values — the suite was 19 tests. A third round 8 regression test proves
cancelling a claim-enabled `CoptStartcomm`'s optional message mid-receive-phase now frees the
claimed address for a sibling CLL (ADR-180 Decision 9) — the suite was 20 tests. A round 9
regression test proves the new J1939 `response_header_bytes` arm exact-matches only the
responding ECU's source address and wildcards every other header byte (ADR-179 Decision 9) —
the suite was 21 tests. Three further round 9 regression tests prove a spontaneous address loss,
a fresh StartComm's own claim reset, and cancelling `cancel_j1939_claim_after_failed_startcomm`'s
own optional-message transmit sequence all stop the CLL's own live SAE J2534-2 clause 14 repeat
slots (ADR-180 Decision 10; the third site was found missing by an `edge-case-hunter`
verification pass on this round's own diff and fixed before the round closed) — the suite was
24 tests. A round 10 regression test proves a spontaneous address loss now cancels a live cyclic
`CoptSendrecv` transmitting under the lost address (ADR-180 Decision 11) — the suite was 25
tests. Three further round 10 regression tests prove `CoptUpdateparam` rejects promoting
`CP_TesterSourceAddress` off a live claim, allows re-staging the same claimed value, and allows a
non-negotiated CLL to change its address freely (ADR-180 Decision 12) — the suite was 28
tests. A round-10 `edge-case-hunter` verification pass on Decision 11's own diff found that its
static `transmits`/`is_send_recv` snapshot never excludes a `CoptSendrecv` that has since detached
to ADR-100 tier 2 (finished sending, now a pure receive-only registrant) — fixed by also excluding
any live `RegistrantTier::ReceiveOnly` registrant's `cop_handle`, and pinned by a new regression
test proving such a registrant survives an unrelated claim relinquishment (ADR-180 Decision 11's
amended text) — the suite is now 29 tests. A round 11 regression test proves an ordinary
`CoptSendrecv` frame is now written with `TX_EXTENDED_ID` set, closing a gap where
`apply_resolved_tx_flags` always cleared the bit for J1939 since `resolve_can_addressing` (a
CAN-specific resolution) never resolves anything for this protocol (ADR-179 Decision 6's round-11
amendment) — the suite was 30 tests. Two further round 11 regression tests prove `SetComParam` now
synchronously rejects an out-of-range `CP_MessagePriority` (family-gated on J1939, boundary value 7
still accepted) and an out-of-range `CP_J1939DataPage` (boundary value 1 still accepted), both
packed into the same outgoing CAN-ID byte `j1939_header_bytes` composes (ADR-179 Decision 6's
round-11 amendment) — the suite was 32 tests. A round 11 regression test proves
`PDU_IOCTL_START_REPEAT_MESSAGE`'s own J1939 `response_header_bytes` arm now rejects an
out-of-range `CP_J1939TargetAddress` synchronously, the same one-byte-range gate
`StartComPrimitive` applies but reachable earlier since Repeat Messaging setup only requires a
connected CLL, not `comm_started` (ADR-179 Decision 9's round-11 amendment) — the suite was 33
tests. Four further round 11 regression tests, a `design-advisor` consult (ADR-180 Decision 13),
prove `PDU_IOCTL_START_REPEAT_MESSAGE` itself now rejects starting a repeat slot on a
negotiation-enabled J1939 CLL with no claimed address (both before any claim and after claim
exhaustion), still succeeds on a non-negotiated CLL with no claim ever, and still succeeds
normally after a successful claim — plus every claim-success/-exhaustion outcome now sweeps any
repeat slot a client raced into existence during the attempt's own pending window, serialized via
`shared_channels` against `PDU_IOCTL_START_REPEAT_MESSAGE`'s own pre-existing lock hold — the
suite was 37 tests. A final round 11 regression test, an `edge-case-hunter` verification pass on
this same round's own diff, proves a SAE J1939 repeat slot's actually-transmitted
`RepeatMsgData[0]` frame now also carries `TX_EXTENDED_ID` — `ioctl_start_repeat_message`'s own
independent inline TxFlags composition was a second site the `apply_resolved_tx_flags` fix above
missed (ADR-179 Decision 6's round-11 amendment) — the suite is now 38 tests. Two round 12
regression tests, a `design-advisor` consult (ADR-180 Decision 14), prove a transmitting
`CoptSendrecv` is now rejected synchronously at `StartComPrimitive` time on a negotiation-enabled
J1939 CLL with no claimed address, while a receive-only `CoptSendrecv` on the same unclaimed CLL
still succeeds — the suite was 40 tests. A further round 12 regression test (ADR-180 Decision 15)
proves `CoptUpdateparam`'s new execution-time re-check catches a claim that lands (via a
`CoptStartcomm` queued ahead of it) after the enqueue-time guard already passed the
`CoptUpdateparam` through, using `PDU_IOCTL_SUSPEND_TX_QUEUE`/`RESUME_TX_QUEUE` to make the race
deterministic — the suite is now 41 tests. A third round 12 fix (ADR-180 Decision 16) excludes a
negotiation-enabled J1939 CLL with no claimed address from `dispatch_due_tester_present`'s dispatch
pass; its shared `j1939_negotiated_unclaimed` predicate is unit-tested directly
(`events_j1939_claim.rs::tests::negotiated_unclaimed_*`, four cases), but the filter's own
integration has no end-to-end regression test here — tester-present remains unreachable via the
live RPC surface for a J1939 CLL, the same gap Decision 6's own tester-present fixes already
document — so the `tests/grpc_mock/j1939.rs` suite count is unchanged by this third fix, staying at
41 tests. A round-12 `edge-case-hunter` verification pass on this round's own diff then found and
fixed a real gap in Decision 14 Part A's enqueue-time gate: it read this CLL's Active
`CP_J1939AddressNegotiationRule` unconditionally, even for a `temp_param_update = 1`
`CoptSendrecv`, whose own resolution (`rpc_primitive::resolve_send_recv_tx`) binds against Working
(`effective`) instead (ADR-067) — so the gate and the resolution could disagree about whether this
CLL even counts as "negotiated" for one Temp-bound send, either spuriously rejecting a Temp-bound
send that would never have needed a claim, or (the direction actually caught by a repro test)
letting one through the gate that `resolve_send_recv_tx` would then mark for cancellation on its
very first cycle. Fixed by adding `j1939_negotiated_unclaimed_for` (reads an explicit `ComParamSet`
instead of always `link.active`) and having the enqueue-time gate pass it Working when
`temp_param_update`/`temp_eligible` apply, mirroring the SAME Temp-vs-Active choice
`resolve_send_recv_tx`'s own caller already makes for `binding`. New regression test:
`tests/grpc_mock/j1939.rs::coptsendrecv_temp_binding_uses_workings_own_negotiation_rule_not_actives`,
confirmed to catch the regression by temporarily reverting the gate to its Active-only read and
observing the test fail. Also removed one of the five originally-added
`negotiated_unclaimed_*` unit tests (`..._true_during_a_spontaneous_loss_to_reclaim_window`) as a
duplicate of `..._true_before_any_claim` — the predicate is a pure function of current state, and
setting `j1939_claimed_address` to `Some` then immediately back to `None` before asserting left an
identical final state, providing no incremental coverage despite its name implying otherwise (also
an `edge-case-hunter` finding). Full J1939 integration suite is now 42 tests. Round 13's two Codex
findings then closed the EIGHTH instance of this bug class plus one unrelated mechanical gap:
`ConnectComLogicalLink`'s finalization stamps a fresh `connect_generation` on every finalized
connect, including a same-`cll_handle` reconnect (ADR-086), but never reset
`j1939_claimed_address`/`j1939_claim_cursor` alongside it — `cancel_j1939_claims_for_cll`
(disconnect teardown) only clears the SHARED `SharedChannel::j1939_claims` routing entry, taking no
`logical_links` reference at all, so the per-CLL LOCAL fields survived a disconnect untouched. A
reconnected CLL that never re-claimed therefore kept reading as "claimed" to every gate consulting
`j1939_claimed_address` (Decisions 13/14/16), permitting e.g. an autonomous repeat slot framed with
an address the adapter had already stopped defending. Fixed by resetting both fields unconditionally
at the same point `connect_generation` itself is stamped — a no-op for a brand-new CLL's first
connect, the actual fix only for a reconnect. See ADR-180 Decision 17. The second, unrelated finding
was a `names.rs::resolve_pin_selection` gap: its J1939 arm used the broad `is_j1939_protocol_id`
predicate (which deliberately also matches the deferred `PROTOCOL_J1939_CH1..CH128` Additional
Channel range, for other callers) to decide whether to accept a raw hardware protocol id, letting a
`_CHx` id through as if it were the `_PS` shape this arm actually implements. Since every call site
resolves `resolve_pin_selection(...)?` before `resolve_channel_selection(...)`, this let a `_CHx` id
either wrongly succeed here (with pins supplied, only to be rejected downstream by an unrelated
mutual-exclusion guard, for the wrong stated reason) or fail with a misleading "must explicitly
select pins" message (with no pins, clause 7's own correct shape). Fixed by narrowing this arm to
reject any non-`PROTOCOL_J1939_PS` id immediately. No ADR Decision item for this one (a
straightforward rejection with no design alternative, so no ADR is needed; recorded
in ADR-180's Consequences section alongside Decision 17 since it landed in the same round). A
follow-up `edge-case-hunter` pass on this round's own diff then found the raw-numeric-id check alone
missed clause 7's OTHER route into the same gap — SAE J2534-2's compound resource-name grammar
(`"<name>_CH<n>"`) resolves to `protocol = ChannelProtocol::J1939_PS` plus a separate
`requested_index`, bypassing the raw-id-only check entirely. Fixed by threading `requested_index`
into `resolve_pin_selection` and widening the check to also reject when it is `Some`; also fixed a
stale `service.rs` doc comment (see Change Checklist for both). Full J1939 integration suite is now
46 tests. Round 14's own Codex finding then closed the recurring "stale claimed-address" bug class's
ninth instance, this time on the successful-claim recomposition of the optional CoptStartcomm
message's header: `handle_start_comm`'s J1939 `Claimed` arm (round 3's own fix) used to recompose
`tx.send.data`'s 5-byte header against `link.active` unconditionally, discarding a `temp_param_update`
CoptStartcomm's Working-bound target address/PDU format/specific/data page/priority (its ORIGINAL
composition at `StartComPrimitive` call time correctly reads `binding.resolved()`, ADR-067) for every
field but the source address. Fixed by recomposing against `binding.resolved()` instead. A candidate
identical fix to `tester_present_data`'s own recomposition was drafted and then reverted after
directly reading `resolve_tester_present`'s call site: it is deliberately ALWAYS bound to Active,
never Working (ADR-067 claim 8 -- tester-present is a persistent product that outlives the transient
init transaction), so `link.active` was already correct there and stays unchanged. See ADR-180
Decision 3's round-14 correction. Full J1939 integration suite was 47 tests.
Round 15's two Codex findings closed the tenth and eleventh instances of this recurring bug class. Decision
15 was extended with an identical execution-time re-check for `CP_J1939TargetAddress`'s `0xFFFF` sentinel
(the same enqueue-vs-execution TOCTOU shape as Decision 15's own NODE_ADDRESS re-check, applied to a
different field). More significantly, a `design-advisor` consult diagnosed and fixed a genuine spoofing gap
(ADR-180 Decision 18): the shared `j1939_negotiated_unclaimed_for` predicate re-read
`CP_J1939AddressNegotiationRule` from whichever `ComParamSet` snapshot was passed (Working, for a
`temp_param_update` call), but that field governs whether a CLL is negotiation-managed AT ALL -- a
structural fact established once at its own real `CoptStartcomm`, not a legitimately per-call-varying
framing choice. A client could stage the rule to "disabled" in Working only, then issue a
`temp_param_update` `CoptSendrecv`, and bypass both the enqueue-time gate and the transmit-time re-check on
a CLL that was genuinely still unclaimed. Fixed by adding a persistent `LogicalLinkState::
j1939_negotiation_engaged` flag, set at each real `CoptStartcomm`'s own dispatch (immune to any later
Temp-bound staging) and reset at reconnect, ORed into the shared predicate. Full J1939 integration suite is
now 49 tests.

## SAE J2534-2 TP2.0 (ADR-188, Phase 7 Stage 7a)

Clause 19 covers VW/Audi's TP2.0 (SAE J2819) connection-oriented transport
protocol: up to four simultaneous logical connections, each individually
established/torn down via `IOCTL_REQUEST_CONNECTION`/`_TEARDOWN_CONNECTION`,
multiplexed over one native `PassThruConnect(TP2_0_PS)` channel and
addressed purely by a 4-byte CAN-ID prefix on the wire (clause 19.4.4).
Neither ISO 22900-2 edition defines any TP2.0/J2819/VWTP equivalent
(verified directly against both spec files), so — like UART Echo Byte/Honda
DIAG-H/J1708/J1939 before it — this phase adds a wholly standalone
`ChannelProtocol::TP2_0_PS` identity (self-mapping, `hw_protocol_override:
None`), not a CAN-family bus-type variant, even though it physically rides
a CAN transceiver.

**One D-PDU CLL models one TP2.0 connection — the central design
decision.** Multiple CLLs join the same physical `TP2_0_PS` channel through
the existing `ChannelKey`/`SharedChannel` ref-count machinery unchanged (the
same sharing mechanism every other multi-CLL-per-channel protocol already
uses); each CLL tracks its own connection state independently
(`LogicalLinkState::tp20_connection`). `REQUEST_CONNECTION`/
`TEARDOWN_CONNECTION` are invoked **internally** from
`PDU_COPT_STARTCOMM`/`PDU_COPT_STOPCOMM` and CLL teardown — never exposed as
client-visible `PDU_IOCTL_*` commands. This directly answers the ADR-178
"genuinely new RPC-level semantics" question `docs/j2534-2-support-plan.md`
flagged for this phase: the D-PDU object model already contains the verb
this needs (`CoptStartcomm` establishes communication with an ECU;
`CoptStopcomm` ends it), so no new IOCTL id, `bytearray_data` layout, or
proto surface was needed. This model was chosen over the alternative (one
CLL = the whole TP2.0 channel, with `REQUEST_CONNECTION`/
`TEARDOWN_CONNECTION` exposed as new `L`-scoped `IoCtl` commands, mirroring
Repeat Messaging's shape) because the D-PDU COP model has no concept of
addressing one of several multiplexed connections within a single CLL,
while clause 19's own frame-routing rule (an inbound frame attributed to a
connection by matching its 4-byte CAN-ID prefix against that connection's
established RX-ID) is structurally identical to how this service already
routes frames to one of several CLLs sharing a physical CAN channel (the
SAE J1939 source-address routing precedent, ADR-184's `UniqueRespIdKey`).

**Resource row and pins.** One new resource-table row (`0x025F`,
`SAE_J2819_TP2_0`) on a minted standalone bus type, `TP2_0_DWCAN`
(`BUSTYPE_TP2_0 = 0x0310`) — the `ANALOG_IN`/`BUSTYPE_ANALOG_IN` minting
precedent (ADR-177), not `ISO_11898_2_DWCAN`, which would falsely imply an
ISO 22900-2 sanction that doesn't exist. `tx_message_size_range`: `4..=4096`
per Table 80 (4-byte CAN-ID prefix + up to 4092 data bytes). Clause 19.2.2
documents exactly one pin pair (J1962 pins 6/14) — `names::
resolve_pin_selection`'s new closed-single-pair arm accepts only that pair
(packed `0x0000_060E`), rejecting any other pin or pin pair; unlike FT-CAN's
own two documented pairs, TP2.0 has exactly one.

**Connection lifecycle** (`events_tp20_connection.rs`, new file, reusing
`events_j1939_claim.rs`'s own polling-without-holding-`logical_links` shape
and `connect_generation` staleness discipline directly): `handle_start_comm`
gains a TP2.0 arm that (1) rejects a non-empty `cop_data` outright (the
inverse of UART Echo Byte's mandatory-one-byte contract, ADR-170 — clause 19
has no COP-borne initialization payload at all); (2) resolves the five newly
minted Working ComParams and packs the 11-byte `SBYTE_ARRAY` request per
Table 78 (setup CAN ID, destination address, the fixed opcode `0xC0`,
proposed TX-ID, proposed RX-ID, application type); (3) issues native
`IOCTL_REQUEST_CONNECTION` and registers a per-physical-channel routing-map
entry (`SharedChannel::tp20_connections`) keyed on the proposed RX-ID,
tagged with the CLL's `connect_generation`; (4) actively drives `poll_rx` in
a bounded wait (a fixed ~2s floor — Table 77's ten timing/count ComParams
stay at native defaults this stage, so this is a compile-time constant, not
read from a live ComParamSet) for the matching `CONNECTION_ESTABLISHED`/
`_LOST` indication, consumed from the ordinary RX poll path -- if this local
deadline is reached with no indication at all (Codex review fix, PR #97,
Fix E), best-effort issues `IOCTL_TEARDOWN_CONNECTION` for the proposed
RX-ID before reporting `Lost(1)`: the native `IOCTL_REQUEST_CONNECTION`
really was issued earlier in this same attempt, so the device may have
genuinely established the connection with the `CONNECTION_ESTABLISHED`
indication simply delayed or lost past this local deadline (a
nonblocking-adapter race, not a native failure) -- without this call the
native slot would otherwise stay alive until the device's own
maintenance-timeout self-heal (clause 19.3.1) rather than being freed
immediately; a failure here is logged only (`warn!`), matching the same
best-effort shape `handle_stop_comm`'s own teardown call already uses, and
falls under the same accepted residual below. Two further Codex review
findings (PR #97, 4th round) close a related pair of gaps around this same
critical section and wait loop: **Fix F** rechecks THIS CLL's own liveness
(`channel_id`/`connect_generation`, the same shape the pre-insert ownership
check above already uses for a sibling) inside the same critical section
that issues the native call, returning `Stale` before ever issuing
`IOCTL_REQUEST_CONNECTION` if `Disconnect`/`DestroyComLogicalLink` raced in
after `handle_start_comm` recorded the `Requested` phase but before this
block ran (a sibling CLL sharing the physical channel can keep it open
through such a race) -- previously only a sibling's liveness was checked
here, not this CLL's own. **Fix G** extends the SAME best-effort
`IOCTL_TEARDOWN_CONNECTION` call Fix E's local-deadline branch already makes
(factored into a shared `best_effort_teardown_on_abandon` helper) to the
wait loop's own mid-wait staleness check too: previously, a CLL going stale
(disconnect/destroy) strictly AFTER the native call had already succeeded
but WHILE still waiting for the indication silently dropped the routing
entry with no teardown attempt at all, leaking the slot unconditionally
rather than only on a best-effort-teardown failure (Fix E's own
conditional leak). Both fixes' own end-to-end regression coverage is
recorded in the Prioritized Backlog -- Fix G's own mid-wait case is
now covered (`tp20.rs`'s `disconnect_mid_wait_after_native_issue_tears_
down_the_leaked_slot`, using a new mock hold hook,
`__mock_set_tp20_no_indication`, the TP2.0 analogue of `j1939.rs`'s own
`__mock_set_j1939_claim_no_indication` -- see that test's own doc comment);
Fix F's own pre-issue case has no analogous construction available (no
natural preemption point exists between the `Requested`-phase write and
this check in a single-threaded harness, unlike Fix G's own wait loop,
which has one at its per-tick `tokio::time::sleep`) and is reviewed by code
inspection instead. (5) on
`CONNECTION_ESTABLISHED`, records the established TX-ID on
`LogicalLinkState::tp20_connection` (`established_tx_id`, phase
`Established`) and `CoptStartcomm` succeeds; on `CONNECTION_LOST`, maps the reason byte
(`1` timeout → `PduErrEvtInitError`; `0xD6`/`0xD7` not-supported →
`PduErrEvtProtErr`; `0xD8` no-resources-free → `PduErrEvtRscLocked`, the
closest `PduErrorEvent` analog to the Phase 13 `ERR_*_IN_USE` /
`PduErrResourceBusy` precedent, since this async COP-failure path has no
synchronous-RPC-only error to report through). `handle_stop_comm` first
best-effort stops this CLL's own live repeat message slots
(`stop_repeat_slots_for_cll`/`push_leaked_repeat_slots`, the same SAE
J1939 "relinquished address" shape ADR-180 Decision 10 already uses,
Codex review fix, PR #97, Fix D) before tearing down the native
connection -- a repeat slot's payload has the connection's fixed TX-ID
baked in once, at `PDU_IOCTL_START_REPEAT_MESSAGE` time, and never
re-resolved per retransmission cycle, so a still-live slot left running
past teardown would have the adapter keep autonomously retransmitting
that now-stale TX-ID as raw, non-connection traffic. `handle_stop_comm`,
plus every existing `DestroyComLogicalLink`/`DisconnectComLogicalLink`
cleanup site (the same call sites Repeat Messaging's ADR-165 Decision 4
already touches, `rpc_link.rs`), issue a best-effort
`IOCTL_TEARDOWN_CONNECTION` keyed on the connection's own established
RX-ID.

**Fix D's own clear-connection + sweep + native-teardown span now holds
`shared_channels` outermost throughout (Codex review fix, PR #97, Fix
I).** Previously `handle_stop_comm`'s TP2.0 arm cleared
`LogicalLinkState::tp20_connection` under `logical_links` alone, released
it, then separately re-acquired `logical_links`/`api` for the repeat-slot
sweep and native teardown -- with no `shared_channels` hold spanning the
whole sequence, `ioctl_start_repeat_message` (`rpc_misc.rs`, which holds
`shared_channels` across its own entire check/compose/native-call/register
span) could interleave in the gap: read a still-live established TX-ID,
register a brand-new repeat slot baked with it, and land that registration
after `stop_repeat_slots_for_cll`'s own sweep had already run and found
nothing -- leaking a freshly-created slot that autonomously retransmits the
now-stale TX-ID after the connection has torn down natively. `handle_stop_
comm` now acquires `ctx.service.shared_channels` first (before `logical_
links`), and holds it through the repeat-slot sweep (calling `push_leaked_
repeat_slots` directly, since `record_leaked_repeat_slots` would
self-deadlock by re-acquiring the same lock) and through the native
`api.tp20_teardown_connection` call, mirroring `ioctl_start_repeat_
message`'s own span and this codebase's documented `shared_channels` ->
`logical_links`/`api` lock-ordering discipline (see the Device selection
section). `stop_repeat_slots_for_cll` itself still only ever acquires
`logical_links`/`api` (never `shared_channels`), so calling it from a
caller that already holds `shared_channels` remains safe by construction.
`tp20.rs`'s `stopcomm_serializes_against_a_racing_repeat_message_start`
races the two RPCs and checks the correctness outcome (no repeat slot
survives with a stale TX-ID either way) but does not, and could not by
construction, force the exact interleaving the fix closes -- see the
Prioritized Backlog for that tracked residual.

**TX framing reads the established TX-ID from `LogicalLinkState`, never from
a ComParam (Codex review fix, PR #97).** `tx_header::build_tx_message` takes
an explicit `tp20_established_tx_id: Option<u32>` parameter, threaded
through by every call site from its own already-held (or freshly acquired)
`logical_links` snapshot of the CLL's `tp20_connection` — `Some(tx_id)` only
when `phase == Established`, `None` otherwise, mapping to the existing
no-connection error. An earlier version instead read `CP_TP20TxIdProposal`
directly off the Active `ComParamSet`, written back by `handle_start_comm`'s
`CONNECTION_ESTABLISHED` arm (mirroring ADR-179's `CP_TesterSourceAddress`
write-back precedent) on the false premise that `comm_started` could not
become `true` for a TP2.0 CLL without that write-back having already
landed — but `CP_TP20TxIdProposal` is itself a client-settable Working
ComParam (`comparam_support.rs`'s `is_tp20_param` allowlists it), so a
client could stage a bogus value via `SetComParam` before `CoptStartcomm`
ever ran, and `CoptSendrecv` has no `comm_started` gate to stop a send from
reading it as though it were the real, device-assigned TX-ID. The
write-back is removed entirely, not just bypassed, closing the
double-duty-field ambiguity (client input vs. service-internal output) at
its root; `CP_TP20TxIdProposal` now only ever reflects whatever a client
last staged on it.

**Repeat Messaging's stop-condition header composition gained the
identical missing TP2.0 arm, for the RX direction (Codex review fix, PR
#97, Fix H).** `tx_header::response_header_bytes` -- the RX-direction
counterpart of `build_tx_message` above, prepending the expected response
header/mask onto a `PDU_IOCTL_START_REPEAT_MESSAGE` stop-condition
mask/pattern (ADR-165/ADR-173 Decision 4) -- had match arms for every
other protocol family but none for `PROTOCOL_TP2_0_PS`, silently falling
through to the empty-header default: since TP2.0's incoming frame leads
with the 4-byte RX-ID header (clause 19.4.4 Table 81), not payload, a
repeat slot's client-supplied mask/pattern (meant to match payload
content) was compared starting at byte 0 of the raw frame, misaligned by 4
bytes -- a slot could stop on unrelated traffic or never stop on its
actual intended response. `response_header_bytes` now takes an explicit
`tp20_established_rx_id: Option<u32>` parameter, mirroring `build_tx_
message`'s `tp20_established_tx_id` exactly: threaded from `ioctl_start_
repeat_message`'s (`rpc_misc.rs`) own already-held `logical_links`
snapshot of the CLL's `tp20_connection` (`requested_rx_id`, only when
`phase == Established`) -- never a ComParam. `Some(rx_id)` composes a
4-byte big-endian header with an all-`0xFF` mask; `None` rejects with the
same "requires an established connection" mapping `build_tx_message`
already uses, per ADR-173 Decision 4's "reject a condition whose stop
criterion could never evaluate correctly" precedent, rather than silently
falling through to the empty-header default that caused the bug.

**RX-ID-matched routing reuses ADR-184's machinery, not a new mechanism.**
`events.rs`'s `header_footer_len` gains a 4-byte-header/no-footer arm for
`PROTOCOL_TP2_0_PS`. Per-CLL delivery across sibling CLLs sharing one
physical channel reuses `UniqueRespIdKey`/`route_frame`
(`events_rx_routing.rs`) directly: `UniqueRespIdKey` gains a `tp20_rx_id`
field, and `build_cll_rx_entries` synthesizes exactly one entry per TP2.0
CLL, established or not (Codex review fix, PR #97: an earlier version only
pushed an entry once `phase == Established`, so a CLL that was merely
`Requested`/`Lost`/never-`tp20_connection`-at-all contributed nothing,
leaving `unique_resp_ids` empty and falling into `route_frame`'s own
"empty table means no table configured, deliver every frame
unconditionally" wildcard fallback — the correct behavior for KWP/J1850,
but a cross-CLL data leak here, wildcard-delivering an established
sibling's own traffic). An `Established` CLL gets the real entry
(`tp20_rx_id: Some(requested_rx_id)`) directly from
`LogicalLinkState::tp20_connection` — not from a client-configured
`UniqueRespIdTable` entry the way J1939's own SA matching is, since
TP2.0's RX-ID is already a structural property of the connection itself
with no addressing-table concept to mirror; any other CLL gets a
deliberately unmatchable sentinel entry (every field `None`), so
`route_frame`'s `.find(...)` correctly returns `None` (frame dropped)
instead of ever reaching the wildcard fallback.

**Deliberately simpler than the SAE J1939 claim loop in several ways this
stage does not revisit:** no NAME-collision concept -- but RX-ID uniqueness
against a live sibling CLL's own still-PENDING request on the same physical
channel IS locally enforced, the same `owned_by_a_live_sibling` shape the
J1939 claim loop uses (`run_tp20_connection_request`'s pre-insert
availability check, `tp20_rx_id_unavailable_for`; clause 19.3.3.2's native
`ERR_NOT_UNIQUE` only fires against an already-ESTABLISHED channel, not a
still-pending one, so it does not itself cover this case) -- extended by Fix
J (7th round, above) to also reject a still-quarantined `abandoned` `rx_id`
regardless of owner. Unlike `j1939_claims` (which retains a successful
claim's entry for the CLL's whole session, so a later sibling naturally
collides against it), `tp20_connections` removes an attempt's own entry as
soon as it resolves either way EXCEPT when abandoned locally (Fix J), and
this codebase gives each physical channel exactly one poll task that
processes a queued `TxItem::StartComm` (including this wait) to completion
before dequeuing the next -- so the live-sibling half of this check can only
ever observe a genuinely still-in-flight sibling, and is not reachable via
the ordinary client-driven RPC path today (defense-in-depth, the same status
`events_j1939_claim.rs`'s own cleanup-gate check documents for its analogous
race); no ongoing monitoring for a LATER spontaneous connection loss once
established (ADR-188 only covers the initial `CoptStartcomm`-driven
exchange — no analogue of J1939's own spontaneous-reclaim duty); and no
`CancelComPrimitive`/`PDU_COPT_STOPCOMM`-race cancellation of an in-flight
connection-request wait (a documented residual, see the Prioritized
Backlog).

**Mock support** (`j2534-0404-mock`): a per-`TP2_0_PS`-channel four-slot
connection table (`ChannelState::tp20_connections`).
`IOCTL_REQUEST_CONNECTION` validates `NumOfBytes == 11`, allocates a slot
and queues a `CONNECTION_ESTABLISHED` indication (`Data[0..3]` echoing the
requested RX-ID, `Data[4..7]` a mock-assigned TX-ID, `rx_id |
0x1000_0000`), or, when all four slots are taken, a `CONNECTION_LOST`
indication with reason `0xD8`. `IOCTL_TEARDOWN_CONNECTION` validates
`NumOfBytes == 4` and a matching slot (else `ERR_INVALID_IOCTL_VALUE`),
removes it, and -- unless `tp20_no_indication` is armed (Fix J, 7th round,
above) -- queues a `CONNECTION_LOST` indication with reason `0`; armed, the
slot is still genuinely freed but no indication is queued for it.
`PassThruWriteMsgs`
implements clause 19.4.4's frame-routing rule: a write whose leading 4-byte
prefix doesn't match any established connection's own TX-ID is rejected
with `ERR_NO_CONNECTION_ESTABLISHED` when it exceeds a single raw CAN
frame's size (a non-connection write that DOES fit is accepted, for mock/
spec completeness, even though this service's own payload-only TX contract
never constructs one). `DEVICE_INFO_TP2_0_SUPPORTED` is advertised per the
existing ADR-185 mock pattern.

**Explicitly out of scope this stage** (ADR-188 §7): `TP2_0_CHx` Additional
Channels (every prior phase's own deferral precedent — fulfilled by
[ADR-210](../../../docs/adr/ADR-210-tp2-0-additional-channels.md)); passive connections
(`TP2_0_IDENTIFER`/`TP2_0_RXIDPASSIVE`, deferred to Stage 7b — fulfilled by
ADR-190, see the "SAE J2534-2 TP2.0 Passive Connections" section below);
broadcast frames and periodic re-trigger (`TX_FLAG_TP2_0_BROADCAST_MSG`,
deferred to Stage 7c — fulfilled by ADR-192, see the "SAE J2534-2 TP2.0
Broadcast Frames and Periodic Re-Trigger" section below); nine of Table
77's ten timing/count ComParams (native defaults only — the tenth,
`TP2_0_T_BR_INT`, is wired by ADR-192 as `CP_TP20BroadcastInterval`, the
one exception ADR-192's own investigation carved out); a raw single-frame
send to a non-connection address (this service's payload-only TX contract
for a connection-bound CLL carries no client-supplied CAN ID to address
such a send with); and `DEVICE_INFO_TP2_0_SIMULTANEOUS`/`_PS_J1962`
discovery wiring (the same `_SIMULTANEOUS` residual every ADR-185 Stage-1
family already accepts).

**Accepted residual (ADR-188 Consequences):** a leaked native connection
slot, if a best-effort teardown fails while a sibling CLL keeps the
physical channel open, is accepted this stage — unlike a leaked Repeat
Messaging slot (ADR-165 round 2), a stranded TP2.0 connection self-heals
device-side via the connection's own mandatory maintenance-timeout (clause
19.3.1), and the `ref_count == 0` `PassThruDisconnect` backstop remains the
eventual cleanup path. This same accepted-residual umbrella now also
covers `run_tp20_connection_request`'s own local-deadline-abandonment best-
effort teardown call (Codex review fix, PR #97, Fix E, see the Connection
lifecycle section above) and its structurally identical mid-wait-staleness
sibling (Fix G, same section, PR #97 4th round): before Fix E/G, an
abandoned attempt whose native call had already succeeded never attempted a
teardown at all -- neither on local-deadline timeout nor on the requesting
CLL going stale (disconnect/destroy) mid-wait -- so a genuinely-established-
but-delayed-indication connection leaked unconditionally rather than only
on a best-effort-teardown failure. Implementer-level residuals from this
stage's own build (see the Prioritized Backlog for durable tracking): no
`CancelComPrimitive`/`PDU_COPT_STOPCOMM`-race cancellation of an in-flight
`run_tp20_connection_request` wait; no regression coverage of clause
19.3.3.2's own native `ERR_NOT_UNIQUE` rejection for a DEVICE-level
collision between two genuinely-established connections (the mock does not
simulate it — ADR-188's own Mock support section scopes the mock to
`NumOfBytes`/slot-capacity validation only); no end-to-end regression
coverage of the local-deadline-abandonment best-effort teardown call itself
(Fix E specifically, as opposed to Fix G's own mid-wait-staleness sibling,
which now IS covered -- see below). PR #97's 4th round added a TP2.0-
specific mock hold hook, `__mock_set_tp20_no_indication` (`j2534-0404-mock`,
the analogue of `__mock_set_j1939_claim_no_indication` this paragraph used
to say did not exist): armed, `IOCTL_REQUEST_CONNECTION` still genuinely
allocates a `tp20_connections` slot but never posts the resulting
`CONNECTION_ESTABLISHED` indication, making `run_tp20_connection_request`'s
bounded wait genuinely block. This closes Fix G's own end-to-end gap
(`tp20.rs`'s `disconnect_mid_wait_after_native_issue_tears_down_the_leaked_
slot` disconnects the requesting CLL mid-wait, after the native call has
already allocated a slot, and proves the slot is freed via the same
"all four slots" indirect-observation technique
`connection_rejected_when_all_four_slots_are_full` already uses) but NOT
Fix E's own local-deadline-abandonment gap specifically, since that test
disconnects well before the ~2s local deadline is ever reached -- closed by
Fix J's own regression test (7th round, below), which lets the same armed
wait run to its own local deadline without disconnecting (and needed one
further mock-side extension, `tp20_no_indication` also gating `IOCTL_
TEARDOWN_CONNECTION`'s own indication push, to make the resulting
abandoned-entry quarantine actually observable -- see Fix J's own
paragraph). A same-service RX-ID collision against a live sibling CLL's
still-PENDING
request is now rejected locally before reaching the native call at all
(`run_tp20_connection_request`'s ownership check, Prioritized Backlog
entry) -- though, per that same entry, this specific check is not reachable
via the ordinary client-driven RPC path in the current
single-poll-task-per-channel architecture (defense-in-depth only today);
that same entry's own missing mock-hook prerequisite is now available too
(same hook), narrowing what is left to close it to the architectural
decoupling half alone. Fix F (the pre-issue `self_still_live` recheck, same
section above) has no analogous end-to-end construction available at all,
armed toggle or not -- see `tp20.rs`'s own doc comment on
`disconnect_mid_wait_after_native_issue_tears_down_the_leaked_slot` for why
(no natural preemption point exists between the point checked and the
native call in a single-threaded harness) and the Prioritized Backlog for
durable tracking.

PR #97's 6th review round found that `LogicalLinkState::tp20_connection`'s
own doc comment already promised a reset "at the next
`ConnectComLogicalLink`'s finalization" (the same reconnect-only reasoning
ADR-180 Decision 17 uses for `j1939_claimed_address`), but nothing actually
implemented it -- fixed by adding `link.tp20_connection = None;` to
`finalize_connected_link` (`rpc_link.rs`), in the same critical section as
the pre-existing SAE J1939 claim-state resets. A clean `CoptStopcomm`/
`DisconnectComLogicalLink` already cleared this field on its own
(`handle_stop_comm`'s queued clear / `rpc_disconnect_com_logical_link`'s own
`take()`), so the only disconnect shape this fix actually changes is
`events::handle_channel_hard_error`. That same disconnect shape also
unconditionally marks the whole module `PduModstNotAvail`, and ADR-131/
ADR-134 make that status sticky until an explicit `ModuleDisconnect` -- which
unconditionally clears the entire `logical_links` map, destroying the very
entry whose staleness this fix protects, before `ConnectComLogicalLink` can
succeed again for that `cll_handle`. A hard-error-then-reconnect-the-same-
handle round trip is therefore not constructible through this crate's gRPC
surface today; `tp20.rs`'s `hard_error_then_reconnect_on_the_same_channel_
is_reported_as_module_not_avail` pins the currently-correct rejection that
scenario actually hits instead, and the fix's real regression proof is
`rpc_link.rs::tests::finalize_connected_link_resets_a_stale_tp20_connection`,
which calls `finalize_connected_link` directly on a hand-built stale link
(mirroring `finalize_connected_link_resets_prior_sessions_error_
suspension`'s identical reasoning for the neighboring `tx_suspended_by_
error` reset in the same critical section).

**Fix J (Codex review finding, PR #97, 7th round): abandoned-RX-ID
quarantine.** `run_tp20_connection_request`'s two local-abandonment paths
(Fix E's own local-deadline timeout, Fix G's own mid-wait staleness) both
issue the native `IOCTL_REQUEST_CONNECTION` successfully before giving up,
so a delayed `CONNECTION_ESTABLISHED`/`_LOST` indication can still arrive
for `rx_id` after the local side has walked away from it. Before this fix,
the post-loop cleanup block removed the `SharedChannel::tp20_connections`
routing entry unconditionally regardless of which outcome fired -- so an
immediate retry proposing the SAME `rx_id` (from the same `cll_handle`,
still connected, exactly the local-deadline case since no disconnect
happened) registered a brand-new entry `deliver_tp20_connection_indication`
could not distinguish from the abandoned one, risking misattributing the
stale indication to the retry (falsely establishing it with an obsolete
TX-ID, or failing it while its real native connection remains allocated).
Fixed by a new `Tp20ConnEntry::abandoned: bool` field (default `false` at
registration): `run_tp20_connection_request` tracks locally (a `let mut
abandoned = false` flipped at either `best_effort_teardown_on_abandon` call
site, right before its own `break`) whether THIS call's own outcome came
from local abandonment; its post-loop cleanup block now marks the entry
`abandoned` in place instead of removing it when so, leaving it registered
as a quarantine rather than removing it (the removal path is unchanged for
a normal `Established`/device-reported-`Lost`/native-issue-`Failed`
outcome). `run_tp20_connection_request`'s own pre-insert availability check
-- renamed from `tp20_rx_id_owned_by_a_live_sibling` to `tp20_rx_id_
unavailable_for`, since its own name no longer covered its second rejection
reason -- now also rejects a NEW request proposing a still-`abandoned`
`rx_id`, from ANY `cll_handle` including the SAME one retrying its own
abandoned attempt (`Tp20ConnectionRequestOutcome::RxIdInUse`, the same
client-visible outcome a live-sibling collision already produces).
`deliver_tp20_connection_indication` releases the quarantine: once its own
existing re-verify check (the entry is still the one it snapshotted)
passes, a NEW check asks whether that entry is `abandoned`
(`tp20_connection_indication_is_for_an_abandoned_entry`, factored out the
same pure-function-for-unit-testability way `resolve_tp20_connection_
indication` already is) -- if so, the entry is removed WITHOUT writing to
`tp20_connection_results` (nothing is waiting on it, and writing there could
collide with a LATER, still-registered live wait for the same `cll_handle`
if one exists by then); otherwise, existing behavior (write the result) is
unchanged.

**Mock-side extension needed to make the fix's own regression test
deterministic, discovered while writing it:** `j2534-0404-mock`'s
`IOCTL_TEARDOWN_CONNECTION` handler previously queued its own `CONNECTION_
LOST` reason-`0` indication UNCONDITIONALLY, regardless of `tp20_no_
indication`. Since `best_effort_teardown_on_abandon`'s own teardown call is
issued as part of EVERY local-abandonment path, and the physical channel's
single poll task drains a queued indication on the very next loop tick --
structurally before it can ever dequeue a subsequent client-driven RPC
(`poll_channel_events`'s own per-iteration `run_due_tick_duties` tail runs
unconditionally right after a `CoptStartcomm` dispatch returns) -- the
quarantine self-released so promptly via this teardown-generated indication
that no client-driven retry, however promptly issued, could ever observe it
still in effect. `tp20_no_indication` now also gates this push (see its own
field doc in `j2534-0404-mock`): armed, teardown still genuinely frees the
native slot, but the release indication is withheld, letting a test hold
the quarantine open long enough to prove a same-`rx_id` retry is rejected
by it. See `tp20.rs`'s `abandoned_connection_request_quarantines_its_rx_id_
against_an_immediate_retry`'s own doc comment for the full construction and
why the brief's original "naturally deterministic, no extra machinery
needed" assumption did not hold once measured.

**Fix K (Codex review finding, P2, PR #97, 8th round): a connection
becoming `Established` right as its owner disappears was never torn down.**
`events.rs`'s `handle_start_comm` TP2.0 `Established` write-back arm already
had its own `still_on_this_channel` recheck (mirroring every other Guard in
that function), but its `!still_on_this_channel` branch previously just
cancelled the COP and returned -- it never tore down the native connection
`run_tp20_connection_request` had just successfully established. The
disconnect/destroy path that raced in moments earlier only ever saw
`tp20_connection` still in `Requested` phase (this write-back hadn't run
yet), so its own `phase == Established`-gated teardown correctly skipped
this connection at the time it ran; nothing else closed the gap, so a real
native TP2.0 connection slot leaked silently, with no local record of it at
all -- unlike Fix E/G/J's own local-abandonment paths, the outcome here is
`Established`, not a local give-up, so `run_tp20_connection_request`'s own
`abandoned` tracking never engages for it. Fixed by best-effort tearing the
connection down in that branch too, reusing `best_effort_teardown_on_
abandon` (now `pub(super)`, callable from `events.rs`) -- and, since this
write-back site is conceptually a THIRD abandonment path (alongside Fix E's
local-deadline timeout and Fix G's mid-wait staleness), quarantining `rx_id`
the same way: `run_tp20_connection_request`'s own post-loop cleanup has
already unconditionally REMOVED this attempt's own `tp20_connections` entry
by the time this write-back runs (its local `abandoned` flag is never set
for an `Established` outcome), so there is nothing left to mark in place --
the new `quarantine_tp20_connection_for_orphaned_write_back` helper
(`events_tp20_connection.rs`) instead vacant-only RE-INSERTS an `abandoned`
entry, leaving an already-occupied slot untouched (a genuine new
registration racing into the gap wins, rather than being clobbered).
Without this, an immediate retry proposing the same RX-ID (from a
reconnected/new CLL) could register a new, indistinguishable entry and have
the teardown call's own delayed `CONNECTION_LOST` indication misattributed
to it -- the same misattribution risk Fix J closed for the other two paths,
just reachable via this third one instead.

Attempted first, per this fix's own brief, but found infeasible: an
end-to-end `tests/grpc_mock/tp20.rs` regression landing a `Destroy`/
`Disconnect` exactly in the gap between `run_tp20_connection_request`
returning `Established` and this write-back running. Unlike Fix G's own
mid-wait race (which has a genuine yield point -- `tokio::time::sleep`
inside the bounded wait loop -- a disconnect can land during), this specific
gap contains no `.await` that ever returns `Pending`: every lock acquisition
from "outcome resolved" through the post-loop cleanup and back into the
write-back is uncontended, and Rust's async model is cooperative, so no
other task (including a concurrent `DisconnectComLogicalLink`/
`DestroyComLogicalLink` RPC handler) can ever be scheduled inside it without
artificially manufactured lock contention this crate has no test hook for --
the same "no natural preemption point in this single-threaded-runtime
harness" limit this file's own Fix F residual and `events_j1939_claim.rs`'s
own `ctx_with_a_send_recv_cop` drain-loop race already document. Proven
instead by two direct unit tests against the new `quarantine_tp20_
connection_for_orphaned_write_back` helper (`events_tp20_connection.rs`'s
own `tests` module, mirroring `tp20_rx_id_unavailable_for`'s own
unit-tested-pure-decision precedent): a vacant `rx_id` gets a fresh
`abandoned` entry, and an already-registered entry (a genuine new
registration racing into the gap) is left untouched rather than clobbered.

**Fix L (Codex review finding, P2, PR #97, 8th round): quarantine release
could deadlock forever once the abandoned entry's own owner CLL
disappeared.** `deliver_tp20_connection_indication` previously resolved its
indication's owning CLL (`resolve_tp20_connection_indication`, a
live-owner-`connect_generation`-gated lookup) BEFORE ever checking whether
the matched entry was `abandoned` -- returning early (`None`) the moment
that resolution failed, which is nearly certain for an abandoned entry,
since two of its three sources (Fix E/G's own local-abandonment paths, and
now Fix K's write-back path above) are the owner disconnecting, reconnecting,
or being destroyed. That made the quarantine PERMANENT: every later
indication for `rx_id` hit the same early return before the function's own
release logic (`tp20_connection_indication_is_for_an_abandoned_entry`) ever
ran, so the entry never released and the RX-ID stayed blocked for every CLL
-- not just the original owner -- until the entire physical channel closed.
Fixed by reordering: `deliver_tp20_connection_indication` now checks
`tp20_connection_indication_is_for_an_abandoned_entry` FIRST, independent of
`resolve_tp20_connection_indication`'s own live-owner resolution -- an
abandoned entry only needs the existing exact-entry recheck (confirming it
is still the same entry snapshotted, not yet replaced/removed by anything
else) to release, never a live owner. Only when the entry is NOT abandoned
does the function fall through to the pre-fix live-owner-resolve-and-deliver
path. Proven end-to-end by `tp20.rs`'s `abandoned_entrys_quarantine_
releases_even_after_its_owner_cll_is_gone`: abandons a request via its own
local deadline (this file's own `abandoned_connection_request_quarantines_
its_rx_id_against_an_immediate_retry` technique), DESTROYS (not merely
disconnects -- a plain disconnect leaves `connect_generation` unchanged, so
the pre-fix bug would not even reproduce) the abandoned attempt's owning
CLL, synthesizes the delayed indication directly via `inject_rx_with_status`
(the mock's own `IOCTL_TEARDOWN_CONNECTION` handler cannot regenerate one a
second time for an already-freed slot, since `tp20_no_indication` was armed
when the original teardown call ran), and confirms a brand-new CLL can then
establish on the same `rx_id` -- the RX-ID's own quarantine having actually
released rather than staying permanently blocked.

**Fix M (Codex review finding, P2, PR #97, 9th round): a `CP_Loopback`-enabled
write's own device-generated echo was dropped, or misdelivered to a
sibling.** `events_rx_routing.rs`'s synthetic TP2.0 routing entry
(`UniqueRespIdKey`) only ever carried `tp20_rx_id`, matched against an inbound
frame's leading 4-byte address prefix. Per the mock's own `PassThruWriteMsgs`
loopback-echo path (and, per clause 19's own frame format, any real adapter's
too), a loopback echo carries the WRITTEN frame's own prefix unchanged -- the
established TX-ID, not the RX-ID any peer ECU ever addresses us with -- so it
never matched `tp20_rx_id` at all: dropped for the originating CLL, or, if
this TX-ID happened to equal a different sibling connection's own RX-ID,
delivered to that wrong CLL instead. Fixed by adding a second field,
`tp20_tx_id: Option<u32>` (populated from `Tp20Connection::established_tx_id`
alongside `tp20_rx_id`, same `Established`-only gating), and a matching
`MatchKind::Tp20TxId` tier in `UniqueRespIdKey::matched` -- originally
unconditional (superseded by Fix N below, same round of review, one round
later). Proven by a `route_frame`-level unit test
(`route_frame_matches_tp20_tx_id_as_well_as_rx_id`, `events_bind_frame_tests.rs`)
showing both IDs live and independently matchable on the same entry, and a
`build_cll_rx_entries`-level unit test asserting `tp20_tx_id` is populated
exactly when `tp20_rx_id` is (`events_build_cll_rx_entries_tests.rs`,
`tp20_established_cll_gets_its_real_rx_id_and_tx_id_entry` -- renamed from
its pre-fix `..._real_rx_id_entry` name).

**Fix N (Codex review finding, P1, PR #97, 10th round): Fix M's own
unconditional TX-ID match was itself a cross-CLL data leak.** Matching
`tp20_tx_id` regardless of frame kind meant a GENUINE inbound frame
addressed to a *different* sibling connection's own RX-ID -- not a loopback
echo at all -- would also match this entry whenever that sibling's RX-ID
happened to equal this connection's own TX-ID: real received traffic,
delivered to two CLLs at once. (Unreachable via this repo's own mock, whose
`IOCTL_REQUEST_CONNECTION` handler always assigns `rx_id | 0x1000_0000` as
the TX-ID -- structurally disjoint from any valid 16-bit RX-ID proposal --
but not something clause 19 itself guarantees against a real adapter, so the
fix stands on protocol grounds, not just mock fidelity.) Fixed by gating the
`tp20_tx_id` tier on a new `is_tx_side: bool` parameter threaded through
`UniqueRespIdKey::matched`/`route_frame`/`route_frame_matched_uudt`/
`process_frame_for_entry`, populated at each frame's own poll-loop call site
from the SAME `is_tx_side` local `poll_rx_inner` already computes for
ADR-099's own TX-side/indication classification (`rx_status_flags &
(RX_TX_MSG_TYPE | RX_TX_INDICATION) != 0`) -- no new bit-flag concept, just
reusing the one already available at exactly the right call site.
`tp20_rx_id` is unaffected: real content and our own echo can never carry
the same 4-byte prefix simultaneously (one CAN ID per frame), so gating only
the TX-ID tier is sufficient. Proven by a new `route_frame`-level unit test
(`route_frame_never_matches_tp20_tx_id_on_a_non_tx_side_frame`,
`events_bind_frame_tests.rs`) constructing exactly this collision (an
entry's `tp20_tx_id` equal to a value standing in for a sibling's
`tp20_rx_id`) and confirming it is dropped when `is_tx_side` is `false` and
routed only when `true` -- an end-to-end reproduction via `tests/grpc_mock/
tp20.rs`'s own harness is not attempted since, per the mock-fidelity note
above, the mock's own TX-ID assignment scheme cannot construct the collision
in the first place.

**Fix N-1 (Codex review finding, P2, PR #97, 11th round): the mock never
simulated clause 19.3.3.2's `ERR_NOT_UNIQUE` for a duplicate established
RX-ID, closing a previously-documented mock-fidelity gap.**
`j2534-0404-mock`'s `IOCTL_REQUEST_CONNECTION` handler unconditionally
`insert`ed into `ChannelState::tp20_connections` regardless of whether
`rx_id_proposal` already had an entry, silently overwriting an existing
established connection's own TX-ID and reporting a second successful
establishment for a device-level collision a real adapter rejects
synchronously. Fixed by checking `tp20_connections.contains_key(&
rx_id_proposal)` before the slots-full check and returning `ERR_NOT_UNIQUE`
directly (a synchronous native-call failure, distinct from the slots-full
case's own asynchronous `CONNECTION_LOST`/`0xD8` indication). This closes
the P3 backlog entry that tracked this exact gap (below, now removed) --
`tp20.rs`'s `second_cll_reusing_an_established_siblings_rx_id_does_not_
disturb_it` (renamed `..._is_rejected_and_does_not_disturb_it`) now asserts
CLL B's colliding `CoptStartcomm` finishes with `PduErrEvtInitError` (the
client-visible mapping of `Tp20ConnectionRequestOutcome::Failed`, the
existing outcome a native-call failure already produces) instead of
succeeding.

**Fix O (Codex review finding, P2, PR #97, 11th round): an abandoned
request's delayed `Established` indication released the quarantine but
never re-attempted teardown.** `deliver_tp20_connection_indication`'s
abandoned-entry release path (Fix L, 8th round) removed the quarantine
entry unconditionally on release, regardless of what the delayed
indication's own `outcome` said. If the abandonment-time
`best_effort_teardown_on_abandon` call (Fix E/G/K) raced ahead of the
device's own internal state -- the native `IOCTL_REQUEST_CONNECTION` was
issued, but the device had not yet finished establishing the connection
internally when the immediately-following `IOCTL_TEARDOWN_CONNECTION`
arrived -- a real device could reject that teardown as "no such
connection," then still deliver a delayed `CONNECTION_ESTABLISHED` for the
same `rx_id`. Releasing the quarantine without inspecting `outcome` left
that now-genuinely-active native connection with no owning CLL and no
further teardown attempt, consuming one of the four slots until the
device's own maintenance timeout. Fixed by checking `outcome` once release
succeeds: an `Established` outcome re-issues `IOCTL_TEARDOWN_CONNECTION`
via the same `best_effort_teardown_on_abandon` helper (with the entry's own
recorded `cll_handle` for logging only, not for any liveness check --
release already happened independently of owner liveness, per Fix L); a
`Lost` outcome needs no further action. **Attempted first, found
infeasible:** an end-to-end (or even black-box mock-level) proof that this
second teardown call actually fires needs the mock to hold a genuine
`tp20_connections` slot for `rx_id` at the moment the delayed indication is
delivered -- but every reachable abandonment path's own FIRST
`best_effort_teardown_on_abandon` call already synchronously removes the
mock's real slot (the mock has no "device still processing the request"
window to simulate the actual race this fix targets, and this crate has no
`IOCTL_TEARDOWN_CONNECTION` call-count backdoor either -- see
`j1939_cancel_error`'s own doc comment in `j2534-0404-mock/src/lib.rs` for
the nearest existing precedent for the kind of force-this-call-to-fail
backdoor that would be needed). Recorded as a new P3 backlog entry
rather than adding new mock instrumentation disproportionate to this fix's
own scope.

**Fix P (Codex review finding, P1, PR #97, 12th round): Fix N's `is_tx_side`
gate was one-sided -- `tp20_rx_id` still matched a TX-side frame
unconditionally.** Round 10's fix gated the `tp20_tx_id` tier on
`is_tx_side` to stop a genuine inbound frame from matching via `tp20_tx_id`
on a CAN-ID collision, but left the OTHER tier, `tp20_rx_id`, matching
regardless of frame kind -- the exact same class of bug, symmetric: a
TX-side echo of a DIFFERENT connection's own write could match a sibling's
entry via `tp20_rx_id` whenever that other connection's TX-ID happened to
equal the sibling's own RX-ID, leaking the transmit echo there too. Fixed
by gating `tp20_rx_id` on `!is_tx_side` as well, making the two tiers
strictly mutually exclusive: a TX-side frame can only ever match via
`tp20_tx_id`, a non-TX-side frame only ever via `tp20_rx_id`. Proven by a
new `route_frame`-level unit test
(`route_frame_never_matches_tp20_rx_id_on_a_tx_side_frame`,
`events_bind_frame_tests.rs`), the mirror image of round 10's own
`route_frame_never_matches_tp20_tx_id_on_a_non_tx_side_frame` -- confirming
`tp20_rx_id` is dropped when `is_tx_side` is `true` and routed only when
`false`. Same mock-fidelity caveat as Fix N: unreachable via `tests/
grpc_mock/tp20.rs`'s own harness, since the mock's `rx_id | 0x1000_0000`
TX-ID assignment scheme keeps TX-IDs and RX-IDs structurally disjoint.

**Fix Q (Codex review finding, P1, PR #97, 13th round): two concurrent
`CoptStartcomm` RPCs on the SAME `cll_handle` could race each other.**
`comm_started` is only ever set `true` at the very end of
`handle_start_comm`'s overall processing (well after a TP2.0 connection
actually establishes), so two concurrent `CoptStartcomm` RPCs for the same
CLL can both pass `rpc_primitive.rs`'s own pre-flight/TOCTOU-recheck
precondition (which only ever checks `comm_started`, still `false` for
both) and get queued as two separate `TxItem::StartComm` entries. This
codebase's single-poll-task-per-physical-channel serialization then runs
them one after the other rather than concurrently, but nothing at dispatch
time re-checked whether an EARLIER queued attempt for the SAME handle had
already succeeded: the TP2.0 arm's own "fresh attempt" block
unconditionally wrote a fresh `Requested` phase before issuing another
native `IOCTL_REQUEST_CONNECTION`, silently clobbering the first attempt's
own just-established `Tp20Connection` state, then failing natively as a
duplicate (`ERR_NOT_UNIQUE`, Fix N-1) -- leaving `comm_started == true`
(set once, correctly, by the first attempt) but the service's own
bookkeeping stuck at `Requested`: `build_tx_message`'s TP2.0 arm then
rejects every send (it requires `Established` specifically), and
`handle_stop_comm`'s own TP2.0 teardown -- also gated on `Established` --
never tears down the still-genuinely-active native connection the FIRST
attempt established, leaking one of the four slots until the device's own
maintenance timeout. Fixed by rechecking `link.comm_started` under the
SAME lock acquisition that writes the fresh `Requested` phase: if already
`true`, this second attempt is rejected outright (`PduErrEvtInitError` +
`PduCopstFinished`, matching this function's own established async-COP-
failure shape) WITHOUT touching `tp20_connection` at all, leaving the
first attempt's own established connection completely untouched. Proven
end-to-end by a new `tp20.rs` test,
`concurrent_startcomm_on_the_same_cll_rejects_the_loser_without_
disturbing_the_winner` (`tokio::join!` over cloned clients, the same
technique this file's own `stopcomm_serializes_against_a_racing_repeat_
message_start` uses -- not a forced guarantee, but the server genuinely has
the opportunity to accept both RPCs before the winner's own establishment,
which normally resolves within a single `POLL_INTERVAL_MS` tick,
completes): asserts the correctness outcome regardless of which RPC's own
attempt the poll task happens to dequeue first -- exactly one of the two
COPs finishes cleanly, the other with `PduErrEvtInitError`, and the winner
keeps working (its established TX-ID still frames a `CoptSendrecv`
afterward). This same design shape (`comm_started` set only at the very
end of `handle_start_comm`, checked only at RPC-accept time, never
rechecked at dispatch time by any OTHER protocol's own "fresh attempt"
reset) plausibly affects J1939's own analogous block too -- out of scope
for this TP2.0-specific finding, flagged as a new P3 backlog entry
for a future look rather than silently assumed safe.

**Fix R (Codex review finding, P2, PR #97, 14th round): every NORMAL,
successful TP2.0 teardown (`CoptStopcomm`, `DisconnectComLogicalLink`,
`DestroyComLogicalLink`) left the torn-down `rx_id` unquarantined, unlike
the LOCAL-abandonment paths (Fix J/K).** `IOCTL_TEARDOWN_CONNECTION` is
non-blocking: the device's own delayed `CONNECTION_LOST` confirmation can
arrive well after these call sites' own native call returns. Since
`run_tp20_connection_request`'s own success path already removes an
attempt's `tp20_connections` entry once it resolves (this ADR's own
"resolves either way" cleanup, `tp20_rx_id_unavailable_for`'s own doc
comment), a NORMALLY torn-down connection leaves NOTHING in
`tp20_connections` for these teardown call sites to clean up or guard --
so nothing prevented a promptly-issued new `CoptStartcomm` proposing the
SAME `rx_id` from registering its own fresh pending entry before that
stale confirmation drained. `deliver_tp20_connection_indication` would then
misattribute the OLD teardown's own delayed indication onto the NEW
request (resolving it via the normal, non-abandoned `resolve_tp20_
connection_indication` path, since the new entry is not marked
`abandoned`) -- the retry could finish as spuriously `Lost` while its own
genuine establishment indication, arriving afterward, found no entry left
to attribute to and was silently dropped, leaving an actively-established
native connection with no owning CLL. Fixed by reusing Fix K's own
`quarantine_tp20_connection_for_orphaned_write_back` helper (vacant-only
insert) at all three call sites, right after issuing the native teardown
for an `Established` connection: `handle_stop_comm` (`events.rs`),
`DisconnectComLogicalLink`, and `DestroyComLogicalLink` (both
`rpc_link.rs`). The helper needed wider visibility to reach `rpc_link.rs`
-- its own declaration changed from `pub(super)` to `pub(in crate::
service)`, and `events.rs`'s own `use tp20_connection::*;` (bringing the
`events_tp20_connection` submodule's items into scope) changed to
`pub(super) use tp20_connection::*;`, mirroring `cancel_j1939_claims_for_
cll`'s own identical cross-module visibility shape and its own `pub(in
crate::service)` declaration. Proven end-to-end for the `CoptStopcomm` call
site by a new `tp20.rs` test, `stopcomm_quarantines_its_rx_id_against_an_
immediate_retry` -- the same `abandoned_connection_request_quarantines_
its_rx_id_against_an_immediate_retry` (7th round) technique, but arming
`__mock_set_tp20_no_indication` only AFTER establishing normally (not from
the start), right before `CoptStopcomm`, so the teardown call genuinely
frees the mock's slot while withholding its own confirmation -- confirming
an immediate same-`rx_id` retry is now correctly rejected
(`PduErrEvtRscLocked`/`RxIdInUse`) instead of racing. The `rpc_link.rs`
`Disconnect`/`Destroy` call sites reuse the identical, already-tested
`quarantine_tp20_connection_for_orphaned_write_back` primitive and mirror
`handle_stop_comm`'s own call shape exactly; not independently re-proven
end-to-end (a `Disconnect`/`Destroy`-triggered equivalent of this same
race would need its own `no_indication`-armed construction per call site,
disproportionate given the shared helper's own correctness is already
proven both in isolation and via the `CoptStopcomm` end-to-end case) --
recorded as a new P3 backlog entry rather than silently left
unmentioned.

**Fix S (Codex review finding, P2, PR #97, 15th round): Fix O's own
follow-up teardown (11th round) released the quarantine BEFORE issuing
it.** `deliver_tp20_connection_indication`'s abandoned-entry branch, on a
delayed `Established` outcome, removed the quarantine entry first and only
THEN called `best_effort_teardown_on_abandon` to issue the follow-up
native call -- but per Fix R's own finding (14th round, this same file,
above), that follow-up `IOCTL_TEARDOWN_CONNECTION` is itself just as
non-blocking as any other teardown call, so a promptly-issued new
`CoptStartcomm` proposing the SAME `rx_id` could register before the
follow-up's own delayed `CONNECTION_LOST` confirmation drains -- the
identical misattribution risk Fix R closed for the normal-teardown paths,
reopened here by Fix O's own release-then-teardown ordering. Fixed by NOT
removing the entry on an `Established` outcome: it stays `abandoned`,
still blocking any new request against `rx_id` (`tp20_rx_id_unavailable_
for`'s existing rule), until the follow-up teardown's own terminal
indication arrives and re-enters this same function -- expected as `Lost`,
which releases it then, exactly as before. Proven end-to-end by a new
`tp20.rs` test, `abandoned_entrys_established_outcome_stays_quarantined_
until_the_followup_teardown_confirms`: abandons a request via its own
local deadline (the 7th-round technique), injects a synthetic
`CONNECTION_ESTABLISHED` indication directly (`inject_rx_with_status`, the
same technique the 8th-round Fix L test uses, since the mock's own real
slot was already freed by the FIRST teardown call and cannot naturally
reproduce a second confirmation), confirms a new CLL proposing the same
`rx_id` right after is still rejected (`RxIdInUse`), then injects a
synthetic `CONNECTION_LOST` standing in for the follow-up teardown's own
eventual confirmation and confirms a THIRD attempt now establishes
normally.

**Fix T (Codex review finding, P2, PR #97, 16th round): Fix Q's own
`already_started` rejection (13th round) skipped the `Temp` hardware
revert every OTHER async-COP failure path in `handle_start_comm` already
performs.** A racing second `CoptStartcomm` using `temp_param_update = 1`
already pushed its own effective Working ComParam set to hardware -- the
`Temp` apply this function performs at its own start, before any of the
dispatch-time work Fix Q's own check runs. Rejecting that second attempt
without also calling `revert_hardware_to_live_active` (the same call every
other failure arm in this function makes, gated on `matches!(&binding,
ParamBinding::Temp { .. })`) left an allowed temporary setting (e.g.
`CP_Loopback`) active on the FIRST attempt's own now-established
connection, even though Active still holds the old value -- a hardware/
bookkeeping mismatch surviving past the COP that requested it. Fixed by
adding the identical revert call, gated the identical way, to Fix Q's own
rejection branch. Not independently end-to-end tested: constructing this
exact interaction (the round-13 concurrent-StartComm race, PLUS
`temp_param_update = 1` on the losing attempt, PLUS an observable hardware-
state check proving the revert actually ran) compounds two already-
narrow-window regressions into one test and would need its own dedicated
harness verification beyond what this fix's own scope justifies -- the
call itself reuses an already-thoroughly-exercised primitive
(`revert_hardware_to_live_active`, called identically by three other
arms in this same function, each independently covered by this codebase's
existing `temp_param_update` test suite for their own protocols) in a
newly-identified but structurally identical position; recorded as a new
P3 backlog entry rather than silently left untested.

**Fix U (Codex review finding, P2, PR #97, 17th round): the mock's
simulated TX-ID formula produced a CAN ID outside the valid 29-bit range.**
`IOCTL_REQUEST_CONNECTION`'s handler assigned `rx_id_proposal |
0x8000_0000` (bit 31 set) as the mock-simulated established TX-ID, but a
29-bit extended CAN identifier maxes out at `0x1FFF_FFFF` -- bit 31 (and
bits 29-30) can never be set on any real CAN ID, so every TX-ID this mock
ever produced was structurally unrepresentable on the wire, not merely an
unlikely value. Every prior fix that relied on this scheme's disjointness
from valid 16-bit RX-ID proposals (Fix N, Fix P) remains correct on its
own terms -- the disjointness property itself doesn't depend on which
otherwise-unused high bit is chosen -- but the mock's own fidelity to
clause 19's CAN-ID space was itself broken since this subsystem's initial
implementation (round 0), not introduced by any TP2.0 fix in this PR.
Fixed by moving the marker bit from bit 31 to bit 28 (`rx_id_proposal |
0x1000_0000`), which stays within the 29-bit range for every possible
16-bit `rx_id_proposal` (maximum result `0x1000_FFFF < 0x1FFF_FFFF`) while
preserving the same structural-disjointness property every mock-fidelity
caveat above (Fix N, Fix P) already relies on. Propagated mechanically to
every occurrence of the old literal across the mock (`j2534-0404-mock/
src/lib.rs`'s handler, doc comment, and two internal unit tests),
`rpc_link.rs`'s test fixture, `events_bind_frame_tests.rs`,
`events_build_cll_rx_entries_tests.rs`, and `tests/grpc_mock/tp20.rs` (9
occurrences) -- a same-PR propagation of one value change across
pre-enumerated call sites (ADR-155), not new exploratory work. No new test
needed: this is a literal-value substitution behind existing, already-
passing coverage, not a new behavior.

**Fix V (Codex review finding, P1, PR #97, 17th round): Fix R's own
quarantine-after-teardown call (14th round) ran unconditionally, even when
the native teardown call itself failed synchronously.** A synchronous
`Err` from `tp20_teardown_connection` means the device never received (or
never processed) the teardown request at all, so no async
`CONNECTION_LOST` indication will ever arrive later to release a
quarantine entry registered for that failed attempt -- unlike the
delayed-indication race Fix R closes, where the indication genuinely is
still in flight and will eventually resolve the quarantine one way or
another. Quarantining unconditionally on a synchronous failure meant every
future `CoptStartcomm` for that rx_id would be rejected as `RxIdInUse`
until the entire physical channel closed: a permanent lock-out, the same
pathology class Fix L (8th round) already closed for a different gap, now
reintroduced by Fix R's own fix for a different one. Fixed at all three
call sites Fix R touched (`handle_stop_comm` in `events.rs`,
`DestroyComLogicalLink` and `DisconnectComLogicalLink` in `rpc_link.rs`)
by tracking whether the native call actually succeeded
(`let teardown_issued = api.tp20_teardown_connection(...)
.inspect_err(|err| { warn!(...) }).is_ok();`, preserving the existing
warning log) and gating the quarantine-insert call on `teardown_issued`
being `true` -- a failed native call now returns its error to the caller
exactly as it did before Fix R existed, with no quarantine side effect at
all.

**Reverted, round 21 (Codex review finding, P2, PR #97): the `teardown_
issued` gate itself reopened the same misattribution race it was meant to
prevent.** Rounds 19-20 already forced the identical conclusion at
`best_effort_teardown_on_abandon`'s own, structurally separate call sites
(see Fix Y below): a synchronous `IOCTL_TEARDOWN_CONNECTION` failure is not
a reliable local signal that no device-side indication is in flight,
because the device may have independently and spontaneously lost the
connection (ADR-188's own "no ongoing monitoring for a later spontaneous
loss once established" residual) before this deliberate teardown call ever
ran -- in which case the call fails precisely BECAUSE a stale
`CONNECTION_LOST` indication may already be queued, not because none is
coming. Round 21 identified that Fix V's own three sites share this exact
gap: a spontaneous loss can race a `CoptStopcomm`/`Disconnect`/`Destroy`
just as easily as it can race `best_effort_teardown_on_abandon`'s callers.
Reverted at all three sites (`handle_stop_comm` in `events.rs`,
`DestroyComLogicalLink` and `DisconnectComLogicalLink` in `rpc_link.rs`)
back to Fix R's original unconditional quarantine-insert, restoring
consistency with every other call site in this mechanism (all now
uniformly quarantine regardless of the best-effort/deliberate teardown
call's own outcome). Not independently end-to-end tested for the same
reason Fix R's own original entry never was: reproducing a synchronous
native-call failure requires a mock-level fault-injection hook this
harness does not currently have for `IOCTL_TEARDOWN_CONNECTION`
specifically.

**Fix W (Codex review finding, P1, PR #97, 18th round): a TP2.0 send never
carried `TX_EXTENDED_ID` even when its established TX-ID needs a 29-bit CAN
identifier.** `rpc_primitive.rs::apply_resolved_tx_flags` -- the shared
helper every TX-flags call site (`resolve_send_recv_tx`, `resolve_tester_
present`, `resolve_init_tx_flags`) funnels through -- derives this bit from
`tx_header::can_addressing_tx_flags(can_addressing, ..)`, which always
returns `0` for TP2.0 (`resolve_can_addressing` is keyed on `CP_CanPhysReqId`/
`CP_CanFuncReqId`, ComParams TP2.0 never configures -- the identical gap
this function's own SAE J1939 branch already documents for itself). Unlike
J1939 (always 29-bit, forced unconditionally), TP2.0 had no dedicated
branch at all before this fix, so an ordinary `CoptSendrecv` on an
established TP2.0 connection submitted its genuinely 29-bit TX-ID header as
an 11-bit frame -- a conforming adapter may reject or misroute every write.
(Round 17's Fix U, which moved the mock's own TX-ID formula into the valid
29-bit range, made this reachable/visible for the first time; the gap
itself is not new to that fix.) Fixed by adding a `tp20_established_tx_id:
Option<u32>` parameter to `apply_resolved_tx_flags`, deriving the flag from
its own magnitude (`tx_id > 0x7FF`, the 11-bit CAN ID maximum) rather than
forcing it unconditionally the way J1939's always-29-bit ID is -- clause 19
permits either width, decided by the device's own address assignment.
`resolve_send_recv_tx` passes its own already-threaded `tp20_established_tx_id`
parameter through; `resolve_tester_present`/`resolve_init_tx_flags` pass
`None` (both are unreachable for TP2.0 -- the former by `handling_enabled`
always being `false` for TP2.0's ComParam allowlist, the latter being the
K-line fast-init wakeup frame, never reached on a CAN-based link). `rpc_
misc.rs::ioctl_start_repeat_message`'s own TxFlags composition for the
actually-transmitted `RepeatMsgData[0]` message duplicates `apply_resolved_
tx_flags`'s logic independently (documented precedent: this same
composition needed its own separate J1939 fix in PR #72 round 11, for the
identical reason) -- fixed there too, with the identical magnitude-based
derivation, immediately after its own J1939 branch. Proven for the two
directly-testable call sites: `resolve_send_recv_tx_tp20_extended_id_tests`
(`rpc_primitive.rs`) confirms `TX_EXTENDED_ID` is set for a TX-ID above
`0x7FF` and clear for one within range. The `ioctl_start_repeat_message`
composition-site duplicate is not independently tested -- see the new P3
backlog entry.

**Fix X (Codex review finding, P2, PR #97, 18th round): `response_header_
bytes`'s TP2.0 arm returned `tx_flags = 0` unconditionally, unlike its own
CAN/ISO15765 arm above it.** SAE J2534-2 clause 14's repeat-slot stop-
condition template needs `TX_EXTENDED_ID` set whenever the established RX-ID
(the 4-byte header this arm prefixes onto the client's mask/pattern) is
above `0x7FF` -- without it, the device evaluates the template as an 11-bit
comparison and may never recognize a match against the intended extended-ID
response, so a repeat slot's own stop condition could never fire. Fixed by
deriving `tx_flags` from `rx_id > 0x7FF`, the same magnitude-based rule Fix W
applies to the TX-side counterpart. Proven by a new unit test
(`response_header_bytes_tp20_sets_extended_id_when_rx_id_exceeds_11_bit_range`,
`tx_header.rs`); the existing `response_header_bytes_tp20_prepends_the_
supplied_established_rx_id` regression test (RX-ID `0x0456`, within the
11-bit range) continues to assert `tx_flags == 0`, unaffected by this fix.

**Fix Y (Codex review findings investigated, P2, PR #97, rounds 19-20): no
code fix -- both of the two plausible-looking fixes tried for this finding
were reverted after being shown unsafe, one by a pre-existing regression
test, the other by a follow-up Codex finding.** Codex's round-19 finding
reported `deliver_tp20_connection_indication`'s late-`Established`
re-teardown branch (Fix O/S) leaving its quarantine permanently in place
when its own follow-up teardown call fails synchronously -- the same shape
Fix V (round 17) originally closed at the three NORMAL-teardown call sites
(`handle_stop_comm`/`DestroyComLogicalLink`/`DisconnectComLogicalLink`,
later itself reverted at round 21 for the identical reason this
investigation reaches below). Two things happened investigating it:

1. Applying Codex's own suggestion literally -- releasing the quarantine at
   that re-teardown branch on a synchronous failure -- was implemented, then
   reverted after `cargo-runner` caught it regressing `abandoned_entrys_
   established_outcome_stays_quarantined_until_the_followup_teardown_
   confirms` (`tests/grpc_mock/tp20.rs`), confirmed non-flaky across 3
   isolated re-runs. The premise doesn't hold at that call site: by the time
   it runs, `rx_id` already had ONE best-effort teardown call issued for it,
   by whichever of `run_tp20_connection_request`'s own two internal
   abandonment paths originally quarantined the entry -- so THIS follow-up
   call failing is routine evidence the ORIGINAL call already tore the
   native connection down (confirmed by the regression test's own
   construction: it arms `__mock_set_tp20_no_indication`, so the original
   abandonment-time teardown call genuinely succeeds and removes the mock's
   slot with no indication queued, making the follow-up call fail with no
   matching slot), not evidence that no indication is coming. A real
   device's own confirmation for that ORIGINAL teardown can still be
   delayed/in flight -- releasing early would let a same-`rx_id` retry
   register before that confirmation drains, reproducing the exact
   misattribution race Fix J/O/S exist to prevent.
2. Investigating `best_effort_teardown_on_abandon`'s (this whole
   mechanism's shared helper) full call graph surfaced a FOURTH,
   genuinely-affected call site Fix V's own sweep had missed for an
   unrelated reason: `events.rs`'s `handle_start_comm` TP2.0 `Established`
   write-back arm (Fix K, 8th round) -- never one of the three call sites
   Fix R (round 14) touched, so outside Fix V's stated scope, but sharing
   the same "fresh, vacant-only insert as a direct result of an
   already-consumed establishment indication" shape. Gating that
   write-back arm's own quarantine-insert on teardown success (mirroring
   Fix V) was implemented and initially believed safe -- until Codex's
   round-20 re-review flagged a DIFFERENT race than round 19's: even though
   this call IS the first and only teardown attempt for `rx_id` from this
   service's own perspective, the device may have independently and
   spontaneously lost the connection before this call ever ran (this ADR's
   own "no ongoing monitoring for a later spontaneous loss once
   established" residual), in which case the call fails precisely BECAUSE
   an indication may already be queued/in flight, not because none is
   coming -- skipping the quarantine there risks the identical
   misattribution race a new registration could hit. Reverted.

Both investigations converge on the same conclusion: a synchronous
`IOCTL_TEARDOWN_CONNECTION` failure at any of `best_effort_teardown_
on_abandon`'s four call sites is not a reliable local signal that no
indication is in flight, so none of them may use it to skip quarantining.
All four now quarantine unconditionally (restoring pre-round-19 behavior);
`best_effort_teardown_on_abandon`'s return type reverted from the round-19
`#[must_use] bool` back to `()`, since no caller ends up using it. The
narrower residual behind Codex's original concern -- a chain of failures
with genuinely no teardown call ever reaching the device and no indication
ever generated at all -- is not new: it is the same accepted residual this
function's own doc comment already documents ("a leaked native slot
self-heals via the device's own maintenance timeout"), just applied to the
quarantine entry rather than the native slot; no local information
available at any of these four call sites can distinguish that from
"already torn down by an earlier/independent event," so none of them
attempt to. Round 21 (Codex review finding, P2, PR #97) generalized this
same conclusion to Fix V's three NORMAL-teardown call sites too
(`handle_stop_comm`/`DestroyComLogicalLink`/`DisconnectComLogicalLink`),
which shared the identical gap and were reverted the same way: every
quarantine-insert call site in this whole mechanism (all seven, across
`best_effort_teardown_on_abandon`'s four and Fix V's three) now uniformly
quarantines unconditionally, regardless of whether its own best-effort/
deliberate teardown call succeeds.

**Fix Z (Codex review finding, P1, PR #97, 22nd round; design-advisor
consult): round 21's uniform-unconditional-quarantine conclusion exposed
the opposite failure mode -- a spontaneous post-establishment connection
loss, silently dropped, could leave a later `CoptStopcomm`'s quarantine
permanently unreleasable.** Once a connection reaches `Established`,
`run_tp20_connection_request`'s own post-loop cleanup already removes its
`tp20_connections` routing entry, so a LATER, spontaneous device-side loss
(unrelated to any local teardown) has no entry to resolve against and was
dropped silently by `deliver_tp20_connection_indication` -- exactly the "no
ongoing monitoring for a later spontaneous connection loss once
established" residual this ADR already accepted. The owning CLL's own
`LogicalLinkState::tp20_connection.phase` then stayed `Established`
forever: a later `CoptStopcomm` believed the connection still live, issued
a doomed native teardown, and (per round 21's own unconditional quarantine)
inserted a quarantine entry with no confirmation left to ever release it --
a permanent lock-out, since the real `Lost` confirmation had already
arrived and been discarded before that quarantine entry ever existed.
Unlike rounds 19-21, no further gating of a teardown call's own result
could fix this -- the two scenarios (a stale confirmation still genuinely
in flight vs. one already consumed-and-discarded) are locally
indistinguishable at the point a teardown call fails; the missing piece was
the reconciliation itself. Escalated to `design-advisor` given the
concurrency/state-machine design stakes and this session's own recent
history of plausible-looking-but-wrong fixes in this exact mechanism.

Fixed per the recommended design: `deliver_tp20_connection_indication`
gains a last-resort reconciliation arm (`reconcile_established_tp20_loss`,
`events_tp20_connection.rs`) -- an unmatched `Lost` indication now scans
every live CLL on the physical channel for one whose own `tp20_connection`
still claims `Established` for this `rx_id`, and flips it to `Lost`
directly (keeping `requested_rx_id`/`established_tx_id`, mirroring the
`Lost`-phase record `handle_start_comm`'s own `CoptStartcomm` outcome
write-back already produces for the other way a connection ends up
`Lost`). At most one live CLL can genuinely hold this match: clause
19.3.3.2's native `ERR_NOT_UNIQUE` rejects a second request against an
already-established `rx_id` before it can ever reach `Established` on a
second CLL (Fix N-1, round 11) -- scanning ALL matches instead of stopping
at the first is defensive, not a real multi-match case. Every downstream
consumer already gates on `phase == Established`, so the flip propagates
for free (sends, RX routing, new repeat-slot starts); the one exception --
an ALREADY-running repeat slot, whose native `IOCTL_START_REPEAT_MESSAGE`
keeps autonomously retransmitting regardless of this CLL's own local
`phase` bookkeeping -- is stopped explicitly via the same `stop_repeat_
slots_for_cll`/`push_leaked_repeat_slots` best-effort shape `handle_stop_
comm`'s own normal teardown already uses (Fix D). **This incidentally
closes a previously-open P2 backlog item** (this same Prioritized Backlog
section, "a spontaneous connection loss... still leaves a live repeat slot
with no way to self-correct or even detect staleness"): that entry's own
third named remedy -- "ADR-188's own spontaneous-loss-monitoring gap being
closed first... needing its own repeat-slot-stop hook layered on top,
mirroring Fix D" -- is exactly what this fix does, so the entry is deleted
rather than kept open. No new client-visible event fires (no existing
`PduErrEvt*` fits a per-connection loss); the next `CoptSendrecv`/repeat-
slot-start on the affected CLL surfaces the existing "no established
connection" error naturally, a documented, narrower residual than full
proactive monitoring (ADR-188 Consequences). Proven by a new `grpc_mock`
regression test
(`spontaneous_loss_after_establishment_reconciles_before_a_later_stopcomm`,
`tests/grpc_mock/tp20.rs`): establish, inject an unsolicited
`CONNECTION_LOST`, confirm `CoptStopcomm` finishes cleanly, then confirm a
brand-new CLL proposing the SAME `rx_id` on the same still-open physical
channel establishes normally (not rejected as `RxIdInUse`) -- the direct
proof no permanent lock-out remains.

**Corrected round 23 (Codex review finding, P1, PR #97): the round-22
shape's own scan-then-flip ran under a `logical_links`-only critical
section, with no `shared_channels` hold spanning it or the subsequent
repeat-slot cleanup -- leaving a window for a concurrent `CoptStopcomm`/
`Disconnect`/`Destroy` to race in, "win" by taking the SAME CLL's
`tp20_connection` first, and (per round 21's unconditional quarantine)
insert a fresh `abandoned` entry for `rx_id` that only THIS `Lost`
indication could ever have released.** Fixed two ways together: (1)
`reconcile_established_tp20_loss` now acquires `shared_channels` FIRST and
holds it for its entire duration, mirroring `handle_stop_comm`'s/
`DestroyComLogicalLink`'s/`DisconnectComLogicalLink`'s own identical
outermost-lock discipline, and the scan-then-flip is now one unbroken
`logical_links` critical section rather than two separate ones; (2) since
serialization alone doesn't tell either side what the other one already
did while waiting for the lock, a scan that finds NO live match now also
checks whether `tp20_connections` holds an `abandoned` entry for `rx_id`
and releases it if so -- clause 19.3.3.2's own RX-ID uniqueness rule means
at most one entry can exist there regardless of cause, and a genuinely
unrelated new `CoptStartcomm` racing onto the same `rx_id` on some other
CLL could not have inserted its own (non-`abandoned`) entry during this
same hold either, so only an entry directly caused by this exact race can
ever be found and removed here.

**Fix AA (found by `edge-case-hunter`'s pre-merge pass, fixed per
`design-advisor` consult, P1, PR #97, round 24): a stale spontaneous-loss
`CONNECTION_LOST` indication for an already-cleanly-resolved connection
could misattribute onto an unrelated, brand-new `CoptStartcomm` that
legitimately reuses the same `rx_id`.** With Codex having approved round
23 clean, a mandatory pre-merge `edge-case-hunter` pass (triggered by this
diff's own concurrency/state-machine scope and its extensive ADR-188
paraphrasing) found a genuinely new failure class, distinct from every
prior round: CLL A establishes a TP2.0 connection completely normally (no
abandonment, nothing quarantined); `run_tp20_connection_request`'s own
post-loop cleanup removes its `tp20_connections` entry as always. Later,
the device spontaneously loses A's connection for a reason entirely
unrelated to any local action, queuing an unsolicited `CONNECTION_LOST`
that sits in the RX queue until the next poll tick. Before that stale
indication drains, CLL B -- a completely ordinary, legitimate client
action (e.g. reconnecting to the same ECU address) -- issues its own
`CoptStartcomm` proposing the SAME `rx_id`; since `tp20_connections` has
no entry for it (A's was already cleanly removed), B registers normally.
When the poll task finally drains A's stale indication,
`deliver_tp20_connection_indication`'s routing resolves it to B -- the
CURRENT occupant of the `rx_id` slot -- since nothing distinguishes "the
current occupant" from "the specific request attempt an old, delayed
indication actually belongs to." B's own `CoptStartcomm` then fails with
A's stale, unrelated loss reason, and (since B was never locally
abandoned) its own entry is removed on that bogus resolution -- so if B's
REAL native connection subsequently succeeds, its genuine
`CONNECTION_ESTABLISHED` finds no entry left to resolve against and is
silently dropped: a spurious COP failure for B, plus a silently leaked
native connection slot with no accepted-residual language covering it (no
teardown attempt happened on this path at all). Escalated to
`design-advisor` given the concurrency/identity-model stakes; confirmed
reachable via direct code reading of `resolve_tp20_connection_indication`
(only checks the entry's OWN `connect_generation` staleness, nothing
about which specific request attempt an indication itself belongs to) and
via clause 19.3.3.2 (a device only accepts a NEW `IOCTL_REQUEST_
CONNECTION` for an `rx_id` if it does NOT consider that `rx_id` occupied
by an established channel -- so B's own registration succeeding is itself
proof A's slot was already free device-side, with A's own `CONNECTION_
LOST` necessarily queued per Table 81).

Fixed with a registration-time reconcile plus a one-shot swallow flag,
not a wire-correlation token (Table 81's indication payload carries only
the `rx_id` and TX-ID/reason byte -- no per-attempt correlation ID the
device echoes back for OUR OWN bookkeeping to compare against) and not a
timed quarantine (any window anchored at A's own `Established` resolution
can't help, since the stale frame originates at spontaneous-loss time,
arbitrarily long afterward -- `CP_TP20MNTC`/`CP_TP20T_E` govern
device-side keepalive, not host drain latency, so any such bound would be
pure numerology). `Tp20ConnEntry` gains `expect_stale_lost: bool`.
Immediately after B's OWN native `IOCTL_REQUEST_CONNECTION` succeeds (the
causal proof described above), `run_tp20_connection_request` scans
`logical_links` for any live CLL on this channel still believing `phase ==
Established` for this SAME `rx_id` and reconciles it to `Lost` right
there -- reusing the identical scan-flip-and-stop-repeat-slots core Fix
Z's own `reconcile_established_tp20_loss` uses, now factored out into a
shared `reconcile_live_established_cll` helper both call sites share --
and arms B's own new entry with `expect_stale_lost: true` if a sibling was
actually found and flipped. `deliver_tp20_connection_indication` then
checks this flag BEFORE routing a `Lost` outcome to whichever CLL
currently occupies the entry: if set, the flag is cleared (never the
entry itself removed -- B's own request is still very much live) and the
frame is swallowed without writing a result, instead of misattributing A's
old reason onto B. An `Established` outcome never consults the flag --
it can only ever mean B's OWN request genuinely succeeded, since a prior
occupant reaching `Established` again would itself be a live sibling
clause 19.3.3.2 would already reject B's own registration against.
Registering the reconcile at REGISTRATION time (not only at delivery
time, mirroring Fix Z's own no-match fallback) closes a sibling variant a
delivery-time-only fix would leave open: A established, spontaneous loss
queued, B registers, then A's OWN `CoptStopcomm` runs BEFORE the stale
frame ever drains -- a delivery-time-only reconcile would find no live
`Established` believer left (A's own StopComm already cleared it) and
still misroute to B. The registration-time reconcile closes this variant
too, since it fires the instant B registers, strictly before A's own
StopComm could ever run.

Proven by a new `grpc_mock` regression test
(`stale_spontaneous_loss_never_misattributes_onto_an_unrelated_new_
startcomm`, `tests/grpc_mock/tp20.rs`), using a new harness backdoor
(`tp20_teardown_connection_directly`, mirroring the existing `stop_
repeat_message_directly`'s "raw IOCTL, service never told" shape) to
simulate a genuine spontaneous device-side loss -- registered immediately
after, with no sleep, since the fix's own correctness does not depend on
any particular interleaving with the poll task's drain of the stale
frame: establish A, spontaneously lose A's connection via the raw
backdoor, immediately register B on the same `rx_id`, and confirm (1)
B's own `CoptStartcomm` finishes cleanly with no error, (2) B's own real
established TX-ID correctly prefixes a subsequent `CoptSendrecv` (direct
proof B's connection is genuinely, correctly `Established`, not left
corrupted), and (3) A's own `CoptSendrecv` is now rejected (direct proof
A's own phase was correctly reconciled to `Lost`, not left stuck). This
proves the OBSERVABLE end-to-end outcome, not which of the two correct
internal paths reconciled A -- the registration-time swallow flag this
section describes, or the round-22 no-match fallback
(`reconcile_established_tp20_loss`) reconciling A directly if the poll
task drains its stale indication before B's own registration arms the
flag. Round 25 (below) found the fallback is empirically what this test's
own "immediately, no sleep" timing exercises in practice, not the swallow
flag; the flag's own one-shot correctness is proven separately by unit
test, not by this integration test. Residual: this rests on device conformance to clause 19.3.3.2's
uniqueness rule and Table 81's queue-on-loss guarantee -- a nonconformant
device degrades to B hitting its own local 2-second connection-request
timeout (`Lost(1)`) plus the existing abandon-quarantine mechanism,
bounded and self-healing, not a new failure mode.

**Round 25 (Codex review finding on Fix AA itself, design-advisor consult
continuing the same round-24 escalation, PR #97): could the one-shot
`expect_stale_lost` swallow eat B's OWN genuine `Lost` instead of A's
stale one, since Table 81 carries no per-attempt correlation token?**
Confirmed provably safe on a conformant device: J2534-1 clause 7.2.5
requires a channel's indications to be read back in the order their
underlying events occurred, and clauses 19.3.3.2/19.3.3.3 place TP2.0
connection indications in that same queue; B's own registration can only
succeed once the device already recorded A's own loss (the same
clause-19.3.3.2 ordering proof Fix AA's registration-time reconcile
already rests on) --
so A's stale `Lost` always occupies an earlier queue position than any
outcome B's own request can produce, meaning the FIRST `Lost` the flag
ever swallows is provably A's stale one, and at most one stale `Lost` can
be outstanding per registration (no counter needed). Only a device
violating clause 7.2.5's own ordering reopens the ambiguity, and even then
the worst case is bounded: a swallowed genuine `Lost` degrades B to its
own local connection-request timeout (possibly surfacing A's stale reason
byte instead of B's own), or a stale `Lost` arriving after B establishes
flips B's healthy connection to `Lost` via the round-22 reconcile arm
(the existing leaked-native-slot residual class, not addressable by any
flag redesign since that reconcile arm never consults the flag).

The one-shot swallow property itself (a second, later `Lost` for the same
entry is never swallowed twice) is proven deterministically by a new
`tp20_connection_indication_is_a_swallowed_stale_lost` predicate, extracted
from the swallow branch and unit-tested directly (plain `HashMap`, no
timing involved) in `events_tp20_connection.rs`'s `#[cfg(test)]` module --
NOT by the `grpc_mock` integration test below. An initial attempt to prove
the swallow specifically via an end-to-end integration test (registering
CLL B immediately after A's stale frame is queued, racing this branch
against the round-22 no-match fallback) was empirically checked via
temporary instrumentation during this round's own `edge-case-hunter`
pass and found to be won by the fallback SYSTEMATICALLY, not 50/50: a
client-driven gRPC registration's own dispatch latency reliably exceeds
the mock's RX-poll interval, so A's stale frame is reconciled by
`reconcile_established_tp20_loss` before CLL B's own registration ever
arms `expect_stale_lost` in the first place. An integration test therefore
can only prove the OBSERVABLE end-to-end invariant (the new occupant never
surfaces the prior occupant's stale reason), not that the swallow branch
itself executed -- this applies equally to the round-24 test above, whose
own doc comment (`tests/grpc_mock/tp20.rs`) and this section's own
"Proven by" paragraph above were both corrected in this same round to say
so explicitly, having previously implied the swallow flag itself was what
the test exercised.

`stale_lost_swallow_is_one_shot_and_never_eats_the_new_occupants_own_
genuine_loss` (`tests/grpc_mock/tp20.rs`) is that end-to-end regression
test: fills the physical channel to its four-slot capacity around CLL A
(mirroring `connection_rejected_when_all_four_slots_are_full`'s own
setup), spontaneously loses A via the raw teardown backdoor (queuing A's
stale `Lost(0)` first), immediately re-fills the freed slot with a fourth
filler so the channel is genuinely full again, then registers CLL B on
A's vacated `rx_id` -- B's own native `IOCTL_REQUEST_CONNECTION` now
itself hits the resource-exhaustion path and queues B's own `Lost(0xD8)`
second, strictly after A's. Confirms B surfaces its own `PduErrEvtRscLocked`
(from reason `0xD8`), never A's stale reason's `PduErrEvtInitError` shape,
regardless of which internal path reconciled A.

**Fix BB (Codex review finding, round 26, PR #97): ADR-188 §1 already
documents that `install_pass_all_filter` is skipped for a TP2.0 channel
(clause 19's per-connection addressing is the RX model, not a pass-all
baseline, the same exclusion clause 10 Analog Inputs already gets per
ADR-177), but the exclusion was never actually wired into
`rpc_link.rs::connect_new_physical_channel`'s filter gate.** That gate only
excluded ISO15765 and Analog Input protocol ids, so every TP2.0
physical-link connect reached `PassThruStartMsgFilter` regardless -- at
minimum a spec-conformance defect regardless of how a given adapter
happens to respond (ADR-188 §1 documents this service should never issue
that call for TP2.0 at all, independent of whether a specific adapter
would reject or silently accept it; whether clause 19 actually requires an
adapter to reject the call outright is the same open question the Prioritized Backlog entry below tracks, not settled here).
The mock never caught this because its own generic filter handler accepts
a TP2.0 protocol id it was never asked to reject. Fixed by adding
`!resources::is_tp2_0_protocol_id(base_proto_id)`
to the same gate the Analog Input exclusion uses, mirroring that
exclusion's own shape exactly. Proven by a new regression test,
`physical_link_connect_never_installs_a_pass_all_filter`
(`tests/grpc_mock/tp20.rs`): connects a TP2.0 CLL and asserts
`filter_count(MOCK_CHANNEL_ID) == 0` at connect time, before any
`CoptStartcomm` ever runs.

A mandatory `edge-case-hunter` pass on this fix (round 26, PR #97) found a
SECOND, independent site with the identical gap: `rpc_misc.rs`'s
`PDU_IOCTL_CLEAR_MSG_FILTERS` handler re-installs a pass-all filter after
clearing every filter on a channel, using the same `base_proto_id ==
ISO15765` / `else` shape the connect-time gate had -- but its `else` arm
had NEITHER the TP2.0 nor the (separately, already pre-existing) Analog
Input exclusion, so issuing `CLEAR_MSG_FILTERS` against an already-
connected TP2.0 or Analog Input CLL silently reinstalled a pass-all filter
the connect-time gate had correctly skipped, independent of whether
connect time itself ever tried to install one. Fixed by adding the
identical `is_analog_in_protocol_id`/`is_tp2_0_protocol_id` exclusion as a
new branch there (do nothing for either protocol, mirroring connect
time's own skip, rather than reinstalling anything) instead of extending
the existing branches.

A separate, related question `edge-case-hunter`'s same pass surfaced but
did NOT fix -- a client's own explicit `SetMsgFilter` against a TP2.0 CLL
has no protocol gate at all, unlike Analog Input's own blanket mock
rejection, and whether clause 19 forbids client-driven filters entirely or
only this service's own automatic baseline is an open spec question ADR-188
§1 doesn't settle -- is tracked in the Prioritized Backlog rather than guessed at here.

## SAE J2534-2 TP2.0 Passive Connections (ADR-190, Phase 7 Stage 7b)

Extends the TP2.0 section above (ADR-188, Stage 7a): clause 19.3.1 also
requires the interface to support exactly one inbound-accepting ("passive")
connection, individually configurable via `TP2_0_IDENTIFER`/
`TP2_0_RXIDPASSIVE` (Table 77) and counting as one of the shared four-
connection total, not a fifth. Stage 7a deliberately deferred this design
("Stage 7b's passive-connection client-surface design is explicitly
deferred and unresolved by this ADR" — ADR-188 Consequences); ADR-190 is
that design.

**Arm-and-complete, not wait-for-accept — the central decision.** A CLL
arms the passive slot via its own `CoptStartcomm`, which completes
successfully immediately once armed; it does not wait for the first inbound
connection. This is forced by this codebase's own concurrency architecture,
not a stylistic choice: each physical channel has exactly one poll task
processing every queued `TxItem` (including an active connection request's
entire bounded ~2s wait) to completion before dequeuing the next. Stage 7a's
active-connection wait tolerates this because it is bounded and spec-
derived; a passive listen has no spec-derived bound at all (the peer may
connect seconds, hours, or never), so a `CoptStartcomm` that waited for the
first inbound connection would starve every sibling CLL's sends and
`CoptStartcomm`s on the same physical channel indefinitely. Establishment/
loss then arrives via the exact same `CONNECTION_ESTABLISHED`/`_LOST`
RxStatus-bit-16/17 indication path Stage 7a already built (Table 81:
`Data[0..3]` is `RX-ID-P` instead of `RX-ID-A`, otherwise identical), routed
to the armed CLL directly — no wait loop involved, since nothing is
waiting.

**ComParam surface** (`service_params.rs`): two new minted ComParams, the
next-free ids after Stage 7a's own `0x80C9`-`0x80CD` block —
`PARAM_TP20_PASSIVE_IDENTIFIER` (`0x80CE`) / `PARAM_TP20_PASSIVE_RX_ID`
(`0x80CF`). Both `to_j2534_config_id → None` (service-level-only,
`comparam_id.rs`), deliberately NOT routed through the generic ComParam→
`SET_CONFIG` pipeline despite the native `CONFIG_TP2_0_IDENTIFER`/
`_RXIDPASSIVE` (`0x804E`/`0x804F`) constants already existing since Phase 0
— routing them generically would let ordinary ComParam application
(connect-time, or a `CoptUpdateparam`) arm/re-arm the device-side listener
outside this mechanism's own arm/disarm lifecycle and exclusivity gate.
`handle_start_comm`'s passive arm applies both directly via its own native
`SET_CONFIG` calls (`j2534-0404`'s newly re-exported `CONFIG_TP2_0_
IDENTIFER`/`_RXIDPASSIVE` constants), exactly when arming. If
`CONFIG_TP2_0_IDENTIFER` succeeds but the second `CONFIG_TP2_0_RXIDPASSIVE`
call then fails, `arm_tp20_passive_listener` best-effort rolls the first
param back to `0` before returning failure (Codex review finding, PR #99)
— otherwise the adapter is left with a real, nonzero `TP2_0_IDENTIFER` and
no recorded service-side state to ever clear it, since the arm itself
failed. A failed rollback is logged and accepted as a residual, the same
class ADR-188's own leaked-native-slot residual already documents
elsewhere in this mechanism. No `grpc_mock` test exercises this specific
second-call-failure path — the mock has no error-injection hook for a
selective, call-ordinal-specific `SET_CONFIG` failure (the same shape of
gap this file already accepts for `PassThruStartMsgFilter`'s own A2-7
filter-install-rollback coverage); accepted as a residual rather than
adding a new mock backdoor for it alone.
`events_tp20_connection::resolve_tp20_passive_params` resolves and
validates them from a `ComParamSet` snapshot: neither staged means "don't
arm" (`Ok(None)`, falls through to Stage 7a's active-connection resolution
unchanged); exactly one staged, or either staged alongside any of Stage 7a's
five active-connection `PARAM_TP20_*` ComParams, is rejected as ambiguous
intent; both staged must be in range (`identifier` `0x200-0x2EF`,
`rx_id_passive` `0x300-0x7FF` — a staged `0` is rejected too, since not
staging at all already means "don't arm"). `comparam_support.rs`'s
`is_tp20_param` allowlist gains both ids alongside Stage 7a's five.

**Exclusivity** (`service.rs`): a new `SharedChannel::tp20_passive:
Option<Tp20PassiveSlot>` field, the exclusivity token for the interface's
single passive slot — `Some` for the entire armed lifetime of whichever CLL
last armed it. Deliberately an `Option`, not an `Arc<AtomicBool>`
reservation flag like GM UART's `become_master_in_flight` (ADR-189): that
pattern guards a bounded ~2s in-flight native call and rejects a sibling's
channel-*join* for that window; here the exclusive state is held for the
listener's entire ARMED lifetime (unbounded), and clause 19.3.1 requires
active connections and the passive slot to coexist (four total, not a
choice between them) — sibling joins and active `CoptStartcomm`s must keep
succeeding while a CLL holds the slot armed. A persistent owner-token is the
correct shape, not an in-flight gate.

**Routing and re-listen** (`events_tp20_connection.rs`): arming
(`arm_tp20_passive_listener`) runs steps 1-7 as one `shared_channels`-held
critical section, mirroring `run_tp20_connection_request`'s own shape —
reject if the slot is already armed (`PduErrEvtRscLocked`, the same shape
the `0xD8` reason byte already maps to for a network-side rejection);
reject if `rx_id_passive` is claimed by a live sibling's own pending active
request or a quarantined entry (`tp20_rx_id_unavailable_for`, reused
unchanged); reject if a live sibling CLL's own `tp20_connection` already
claims `Established` for `rx_id_passive` (a NEW scan this stage adds — an
active connection's own routing entry is removed once it establishes, so
the prior check alone cannot see it); issue the native `SET_CONFIG` calls
(originally two, now three — see the "Defensive identifier zero" Correction
paragraph below, round 4); insert a **persistent** `Tp20ConnEntry { passive: true, .. }` (new
`passive: bool` field, `false` everywhere else) keyed on `rx_id_passive` —
deliberately never removed once inserted while the slot stays armed,
breaking the existing "entry removed once resolved" invariant for passive
entries only (adopting the same retain-for-session shape
`SharedChannel::j1939_claims` already uses); write
`LogicalLinkState::tp20_connection = Some(Tp20Connection { phase: Listening,
passive: true, .. })` (new `Tp20ConnectionPhase::Listening` variant, new
`Tp20Connection::passive` field). `deliver_tp20_connection_indication` gains
a passive delivery arm, checked after the existing abandoned-entry/stale-
swallow checks (both apply unchanged to a passive entry too): for a
non-abandoned `passive` entry, re-verifies owner CLL liveness and writes
`LogicalLinkState` directly (the same no-live-wait shape
`reconcile_live_established_cll`'s own write-back already uses, since no
wait loop reads a passive entry's own outcome) — `Established(tx_id)` sets
phase `Established`/`established_tx_id`; `Lost(reason)` sets phase back to
**`Listening`**, NOT the terminal `Lost` an active connection reaches — per
clause 19.3.3.1, with both `SET_CONFIG` params still valid and a slot free,
the device auto-accepts the next inbound request too, and the persistent
entry is what lets that next indication route correctly. Deliberately does
NOT run `reconcile_live_established_cll`/set `expect_stale_lost` at arm time
(unlike `run_tp20_connection_request`'s own registration-time reconcile
step): that mechanism rests on a successful native `IOCTL_REQUEST_
CONNECTION` being itself proof (clause 19.3.3.2) that the device no longer
considers the target RX-ID occupied — arming issues no such call, so no
equivalent proof exists.

**Disarm** (`events_tp20_connection.rs`, `events.rs::handle_stop_comm`,
`rpc_link.rs`'s `DestroyComLogicalLink`/`DisconnectComLogicalLink`): three
call sites, all gated on the CLL owning `tp20_passive` (matching
`cll_handle`/`connect_generation`), each running the same four-step
sequence, split into two small reusable pieces since the three call sites'
own lock topology differs too much for one self-locking async helper (each
already holds `shared_channels`/`api` itself by the time it reaches this
point — a shared helper that re-acquired either would self-deadlock,
mirroring `stop_repeat_slots_for_cll`'s own `logical_links`/`api`-only
acquisition reasoning): (1) `best_effort_disarm_tp20_passive_native_config`
(plain `&J2534Api0404` reference, no locking) best-effort clears both native
`SET_CONFIG` params, `TP2_0_IDENTIFER` first — before anything else runs,
so the device stops accepting a further inbound connection before any
service-side state changes; now returns `config_cleared: bool` (`true` if
either call succeeded) rather than `()`, feeding the bounded-release
decision below. (2) if the CLL's own connection was `Established`,
best-effort `tp20_teardown_connection` (called directly, no wrapper, the
same convention Stage 7a's own three active-teardown sites use) — the
`rpc_link.rs` sites' own pre-existing active-connection teardown scan is
narrowed with an explicit `!c.passive` filter guard (Codex review finding,
PR #99) so a passive connection is never torn down twice, once by that
scan and once by this dedicated step; `handle_stop_comm` already carried
this guard. (3)-(4) `quarantine_tp20_passive_slot_on_disarm` (plain `&mut
SharedChannel`, no locking; now takes `was_established: bool,
config_cleared: bool`) unconditionally marks the persistent entry
`abandoned` in place (never removed — the same unconditional-quarantine
convention ADR-188 rounds 19-21 established for every quarantine-insert
site in this whole mechanism: an accept or a spontaneous device-side loss
can race a disarm exactly as it can race an active connection's own
teardown, so no synchronous call result may safely decide whether
quarantining is skippable) and clears `tp20_passive`. A delayed
`Established` indication landing on the now-abandoned entry re-tears-down
and holds the quarantine until the follow-up `Lost` drains — the existing
Fix O/S/L machinery (ADR-188 Consequences) already covers this exactly, no
passive-specific extension needed.

**Bounded release for a never-established listener (Codex review finding,
P1, PR #99; design-advisor consult — full reasoning in ADR-190's
"Correction" paragraph).** The unconditional quarantine above is sound for
a disarm that reached `Established` (step 2's real `TEARDOWN_CONNECTION`,
or clause 19.3.1's own maintenance-timeout loss, eventually releases it the
ordinary way) but was a permanent leak for a disarm still in `Listening` —
step 2 issues no native call in that case, so nothing would ever arrive to
release the quarantine, in the ordinary (not merely raced) case of arming
and later disarming with nothing ever having connected. Fixed by reusing
the existing ADR-101 Decision §E "drain watermark" mechanism (originally
built for cyclic COP registrant reaping): `Tp20ConnEntry` gains
`idle_release_at: Option<tokio::time::Instant>`;
`quarantine_tp20_passive_slot_on_disarm` stamps `Some(now +
TP20_PASSIVE_IDLE_RELEASE_GRACE)` (a new 500ms const) only when
`!was_established && config_cleared` — the config-clear succeeding is what
bounds the indication horizon (clause 19.3.3.1's both-nonzero accept
condition, plus clause 7.2.5's in-order-read guarantee that anything still
queued for this listener is flushed by one drain past the disarm moment).
A new pure predicate, `tp20_passive_idle_release_due`, and a new poll-task
sweep, `release_idle_passive_slots`, remove a `tp20_connections` entry once
`abandoned && passive && idle_release_at` has passed the current drain
watermark; wired into both `run_due_tick_duties` and
`run_detached_registrant_maintenance`, mirroring
`reap_expired_cyclic_registrants`'s own dual wiring exactly. The abandoned-
branch `Established` re-teardown arm in `deliver_tp20_connection_indication`
clears `idle_release_at` back to `None` when it keeps the entry — release
must again wait for the follow-up `Lost`, not the drain barrier. Two naive
fixes were rejected as unsafe: immediate release on `Listening` (reopens
the exact raced-accept misattribution `disarm_races_an_inbound_accept_and_
leaves_the_entry_quarantined` proves is real); and gating release on an
unconditional `TEARDOWN_CONNECTION`'s own synchronous result (cannot
distinguish "never accepted" from "accepted-then-spontaneously-lost with a
queued `Established`+`Lost` pair still in flight" — the identical hazard
ADR-188 rounds 19-21 already proved unsafe). A companion fix closes a
latent gap the new bounded release makes dangerous:
`deliver_tp20_connection_indication` now retries once against a fresh
snapshot when the passive delivery arm's owner-liveness/`connect_
generation` recheck fails, instead of silently dropping the indication — a
dropped `Established` would otherwise mean the marker-clear above never
runs, and the sweep could release an RX-ID whose native connection is
genuinely established.

**Second window in the same delivery arm (Codex review finding, P1, PR
#99, round 2).** The companion fix above closed the window between the
delivery arm's *initial* snapshot and its recheck, but left a second
window open: the recheck itself acquired and released `shared_channels`
before the arm separately acquired `logical_links` to write the outcome.
A disarm (`handle_stop_comm`/`rpc_link.rs`, both of which acquire
`shared_channels` FIRST and hold it across their own `logical_links` read
that decides `was_established`) landing in that second window could
observe the connection as still `Listening`, skip `TEARDOWN_CONNECTION`,
`take()` the CLL's `tp20_connection`, and schedule a bounded release — all
before the delivery arm's own `logical_links` write ran, which then
silently no-ops (`link.tp20_connection` already `None`). Fixed by
acquiring `shared_channels` FIRST in the delivery arm too and holding it
across both the recheck and the `logical_links` write, mirroring every
disarm site's own outermost-lock discipline — the two flows are now
mutually exclusive via the same mutex. A pure lock-discipline fix, not a
timing mitigation: the race is not re-testable at the integration level
(constructing it would require reverting the fix), so it is verified by
the lock-ordering argument plus the full existing passive-arm test suite
continuing to pass unchanged.

**Repeat slots on passive spontaneous loss (Codex review finding, P2, PR
#99, round 2).** The passive delivery arm's `Lost` outcome re-enters
`Listening` but did not stop any repeat-message slot running against the
connection just lost — `PDU_IOCTL_STOP_REPEAT_MESSAGE` is not tied to
`comm_started`, so a slot left running keeps autonomously retransmitting
the now-stale peer TX-ID both while idle and after a new peer establishes
with a different TX-ID. Fixed by calling `stop_repeat_slots_for_cll`/
`push_leaked_repeat_slots` (the same best-effort pair `reconcile_live_
established_cll` already uses for an active TP2.0 connection's own
spontaneous loss) whenever the lost connection was genuinely `Established`
beforehand. New test:
`passive_connection_spontaneous_loss_stops_its_own_repeat_slot`.

**Repeat slots on passive `CoptStopcomm` disarm (Codex review finding, P2,
PR #99, round 3).** The round-2 fix above covers the device-side
spontaneous-loss path only; `handle_stop_comm`'s own passive-disarm arm
had the identical gap on the client-driven `CoptStopcomm` path — its
active-connection arm's repeat-slot stop is gated on `tp20_teardown.
is_some()`, which the active/passive split deliberately excludes a passive
connection from, and the passive arm never called `stop_repeat_slots_for_
cll` on its own either. Fixed by calling it there too (before the native
config-clear/teardown, when `was_established`), mirroring both the active
arm's own ordering and round 2's identical fix. `rpc_link.rs`'s `Destroy`/
`DisconnectComLogicalLink` sites needed no equivalent fix — both already
unconditionally stop every one of a CLL's repeat slots ahead of any
TP2.0-specific branch. New test:
`stopcomm_stops_an_established_passive_connections_own_repeat_slot`.

**Defensive identifier zero before the final enable (Codex review finding,
P2, PR #99, round 4).** `arm_tp20_passive_listener`'s original two-call
native sequence (`TP2_0_IDENTIFER` then `TP2_0_RXIDPASSIVE`) is unsafe
whenever a prior disarm's own best-effort clear only partially succeeded,
leaving a stale nonzero `TP2_0_RXIDPASSIVE` on the device (that helper
attempts both clears unconditionally, so either can fail independently —
the exclusivity check only inspects this service's own tracked state,
never the device's actual native config). Writing the new identifier
first then briefly pairs it with the stale rx_id, satisfying clause
19.3.3.1's both-nonzero accept condition for an rx_id with no registered
routing entry — an untracked native connection. Fixed with a 3-call
sequence: force `TP2_0_IDENTIFER` to `0` first, unconditionally (safe
regardless of the device's actual prior state, since one side of the
both-nonzero condition is now known-zero); write the new
`TP2_0_RXIDPASSIVE` next; write the new `TP2_0_IDENTIFER` last — the
single call that actually enables the listener, by which point
`TP2_0_RXIDPASSIVE` already holds its correct value. New test:
`passive_arm_issues_a_defensive_identifer_zero_before_the_final_enable`,
asserting the exact 3-call sequence via the mock's own
`set_config_param_log` backdoor (the actual native race this fix closes
is not independently constructible — no mock hook injects a selective
per-call `SET_CONFIG` failure, the same accepted-residual class already
documented above for the arm's own second-call-failure rollback path).

**Mock support** (`j2534-0404-mock`): `IOCTL_SET_CONFIG` gains range
validation for `CONFIG_TP2_0_IDENTIFER` (`0` or `0x200-0x2EF`)/
`CONFIG_TP2_0_RXIDPASSIVE` (`0` or `0x300-0x7FF`), rejecting the whole batch
with `ERR_INVALID_IOCTL_VALUE` on an out-of-range value — the same
whole-batch-rejection convention the FD-rate-before-pins/mixed-format gates
already use. A new test-only backdoor,
`__mock_inject_tp20_passive_connection(channel_id, peer_tx_id)`, simulates
the network-side accept clause 19.3.3.1 describes as fully autonomous (no
`IOCTL_REQUEST_CONNECTION` call at all): reads the channel's own currently-
armed `CONFIG_TP2_0_IDENTIFER`/`_RXIDPASSIVE` values; if either is unset, or
the channel's four-slot `tp20_connections` table is already full, silently
no-ops (no `rx_queue` push — clause 19.3.3.1's own network-side rejection
never reaches the application as an indication) and increments a new
per-channel `ChannelState::passive_connection_rejected_count` counter for
test observability (`__mock_get_passive_connection_rejected_count`);
otherwise inserts a `Tp20MockConnection` keyed on `CONFIG_TP2_0_
RXIDPASSIVE` and queues a `CONNECTION_ESTABLISHED` indication, mirroring
`IOCTL_REQUEST_CONNECTION`'s own success-path shape exactly. Respects
`tp20_no_indication` (ADR-188's own toggle) the same way `IOCTL_REQUEST_
CONNECTION` already does — the native slot still allocates, but no
indication is queued — letting a test deterministically construct "the
service disarms while genuinely unaware a native accept already landed"
without depending on the poll task's own timing.
`IOCTL_TEARDOWN_CONNECTION` needed no change: it already matches purely on
`rx_id` against `tp20_connections` regardless of how the entry originated,
so a passive-injected connection's teardown already worked via the existing
code path.

**Accepted residuals (ADR-190 Consequences):** no client-visible establish/
loss event for a passive connection — the client observes establishment
only indirectly (a previously-rejected send now succeeds, or the peer's
first data telegram arrives), the same shape as Stage 7a's already-accepted
"no client-visible loss event" residual for active connections. **Update
(ADR-191):** resolved by decision, not a new RxFlag bit — TP2.0's
indication frames are state-machine input, not vehicle content, and no
ISO 22900-2 `RxFlag` bit exists for them either; the connection's own
establishment outcome is already client-visible via the `CoptStartcomm`
COP's own result, and a mid-session loss is already observable indirectly
via a subsequent send failure. Synthesizing a fake RX message to signal
this instead was considered and rejected — inventing client-facing data
the wire contract never actually defines would be worse than the
documented residual. Whether a real adapter actually honors `TEARDOWN_CONNECTION`
against an established passive connection is unverified against real
hardware (VW/Audi-specific, unavailable to this workspace, the same
standing caveat every TP2.0-adjacent claim in this workspace already
carries); degrades to the pre-existing maintenance-timeout self-heal
residual class if a device rejects it, not a new failure mode. **Update
(ADR-210):** this section's own "revisit if Additional Channels are ever
added" trigger has now fired, and is resolved, not left open — per-channel
exclusivity (both the passive-connection slot this section covers and the
four-total-connection budget Stage 7a's own section covers) stays scoped to
`SharedChannel` (keyed by the live hardware protocol id, `_PS` or a specific
`_CHx` index), so a `_CH1` channel and a `_CH2` channel now each get their
own independent one-passive/four-total budget, not a budget shared across
the whole TP2.0 module. This is not a new design decision ADR-210
introduces — every other `_CHx`-in-scope family's own per-`SharedChannel`
state (SAE J1939's claims, GM UART's/Honda DIAG-H's/SAE J1708's repeat-
message slots) has been scoped this way since ADR-156 Phase 2b first
established that a `_CHx` id is its own independently-manageable D-PDU
channel resource, decoupled from its `_PS` sibling; ADR-210 simply extends
that same, already-settled architectural precedent to TP2.0's connection
state rather than deciding it fresh. Whether a real TP2.0 adapter's own
transport-layer connection budget is actually a single module-wide pool
shared across every `_CHx`/`_PS` id it exposes (as opposed to per-channel
independent pools, what's implemented) is unverified against real
hardware — the same standing VW/Audi-hardware-unavailable caveat every
TP2.0-adjacent claim in this section already carries, not a new one. ADR-188
needs no Status-line change — its own Stage 7b deferral is fulfilled by
ADR-190, not superseded or revised.

**Tests** (`tests/grpc_mock/tp20.rs`): arm-and-complete plus establish/data-
exchange via the mock's own injection backdoor
(`passive_listener_arms_immediately_then_establishes_and_exchanges_data`);
a second CLL's own passive-arm attempt rejected while one is already armed
(`second_cll_passive_arm_is_rejected_while_one_is_already_armed`); the
active-vs-passive RX-ID collision in both directions
(`active_established_connection_blocks_a_passive_arm_on_the_same_rx_id`,
`passive_armed_slot_blocks_an_active_proposal_on_the_same_rx_id`); a loss-
then-re-listen-then-re-establish cycle proving the persistent entry survives
a `Lost` outcome
(`passive_connection_re_listens_after_a_spontaneous_loss_and_re_establishes`);
a disarm racing an in-flight accept (constructed deterministically via
`tp20_no_indication` rather than timing, proving the unconditional-
quarantine path drains correctly —
`disarm_races_an_inbound_accept_and_leaves_the_entry_quarantined`, its doc
comment and final assertion updated to describe the now-*bounded* (not
permanent) quarantine the P1 fix leaves this exact scenario with — the
test's own permanent-suppression technique, `tp20_no_indication=true`,
models an already-accepted non-conformant-device residual, distinct from
the timing race the fix actually handles); a disarm of a listener that
never received any connection releasing after the grace period
(`disarm_of_a_never_established_passive_listener_bounded_releases_after_
the_grace_period`); `destroy_com_logical_link_disarms_the_passive_listener`
covering both the never-established and the establish-then-destroy cases
(the latter added to catch the double-teardown finding above — a fix that
only covered the never-established case would have passed regardless of
that bug); and mock-fidelity coverage for the silent-rejection-before-
arming case (`injecting_a_passive_connection_before_arming_is_silently_
rejected`). `events_tp20_connection.rs`'s own `#[cfg(test)]` module adds
unit coverage for `resolve_tp20_passive_params`'s full decision table
(none staged, exactly one staged, out-of-range, staged-as-zero, active-
ComParam collision, valid round-trip), `quarantine_tp20_passive_slot_on_
disarm`'s pure map-mutation behavior (including its `idle_release_at`
stamping decision across the `was_established`/`config_cleared`
combinations), and `tp20_passive_idle_release_due`'s full predicate table
(deadline reached, not abandoned, not passive, no deadline set, no
watermark, watermark before deadline).

## SAE J2534-2 TP2.0 Broadcast Frames and Periodic Re-Trigger (ADR-192, Phase 7 Stage 7c)

Extends the TP2.0 sections above (ADR-188 Stage 7a, ADR-190 Stage 7b) with
the last unshipped TP2.0 sub-stage: clause 19.3.2.2/19.3.2.3's broadcast
send and its periodic re-trigger. `TX_FLAG_TP2_0_BROADCAST_MSG` (bit 16)
already existed in `j2534-0404-sys`'s bindings from Phase 0 (ADR-152); this
stage is the first to use it.

**Broadcast addressing** (`service_params.rs`/`comparam_id.rs`/
`comparam_support.rs`): a new per-send-scoped ComParam,
`CP_TP20BroadcastAddress` (`0x80D0`, `PARAM_TP20_BROADCAST_ADDRESS`) — `0`
= no broadcast/normal send, `0xF0`-`0xFF` = broadcast to that address,
anything else nonzero rejected before any native call. `to_j2534_config_id`
returns `None` for it (service-level only, matching the Stage 7b passive
params' shape) — there is no native `SET_CONFIG` for a per-message
address, so it is read directly by the `CoptSendrecv` resolution path in
`rpc_primitive.rs`/`tx_header.rs`, never forwarded to hardware.
`comparam_support.rs::is_tp20_param` allowlists it alongside the existing
seven TP2.0 ComParams.

**Usage recipe (`temp_param_update`-scoped, ADR-067):** a client stages the
broadcast address into Working via `SetComParam`, then issues the
broadcast `CoptSendrecv` with `temp_param_update` set — reusing the
existing single-ComParam-snapshot binding/revert mechanism unchanged, since
this ComParam is the first ComParam-derived TxFlags bit in this service
that is per-send rather than link-invariant (every prior instance —
`sci_tx_flags`/`sw_can_tx_flags`/`msg_priority_tx_flags` — is an objective,
link-wide fact). Staging it into Active instead (making every send on the
CLL a broadcast) is not forbidden, just not the documented recipe.

**Composition** (`tx_header::build_tx_message`'s `PROTOCOL_TP2_0_PS` arm):
when `active.tp20_broadcast_address()` resolves `Some(address)`, the arm
composes `[address] ++ payload` and returns immediately, WITHOUT consulting
`tp20_established_tx_id` at all or requiring the connection to be
`Established` — a broadcast addresses a group, not an established peer.
Checked before the established-TX-ID branch that every other TP2.0 send
still uses unchanged. **A single-shot broadcast dispatches through the
ordinary `tx_queue`/`dispatch_tx_item` pipeline unchanged (`num_send_cycles
== 1`), which carries a separate dispatch-time TP2.0-connection-awareness
mechanism (ADR-188/PR #97, `events.rs::handle_send_recv`/`handle_stop_comm`)
that re-reads the CLL's live connection state and overwrites `data[0..4]`
with the fresh established TX-ID whenever the connection hasn't drifted —
this must be skipped entirely for a broadcast item, since a CLL can have
BOTH an established connection AND a staged broadcast for one send
(broadcast is per-send, connection-independent), so `tp20_established_tx_id`
can be `Some` even for a broadcast.** `ResolvedSendRecvTx`/`SendRecvTx`
carry an explicit `tp20_is_broadcast: bool` flag for exactly this purpose,
checked first by both dispatch sites (Codex review, P1, PR #101) — without
it, a single-shot broadcast issued on a CLL with an established connection
would have its address byte and 3 payload bytes silently overwritten with
the connection's TX-ID. `rpc_primitive.rs::apply_resolved_tx_flags` ORs in
`TX_FLAG_TP2_0_BROADCAST_MSG`, gated the same way `sw_can_tx_flags`/
`msg_priority_tx_flags` gate their own bits
(`resources::is_tp2_0_protocol_id(hw_protocol_id)`), since the function has
no other way to know the live link's protocol. A broadcast `CoptSendrecv`
with a nonzero `num_receive_cycles` or a non-empty `expected_response_array`
is rejected (`INVALID_ARGUMENT`) — no response is attributable to a
broadcast send under this service's per-connection response routing; so is
any `num_send_cycles` outside `{1, -1}` — the native adapter offers only
"burst once" (`PassThruWriteMsgs`, `1`) or "burst once then repeat forever"
(`PassThruStartPeriodicMsg`, `-1`), no finite repeat count in between. The
composed message's own size range is `3..=8` bytes, not `1..=8` (an
edge-case-hunter follow-up correction): the burst simulation alternates the
*last two bytes of the whole message*, not just the payload, so a message
shorter than 3 bytes (1 address byte + at least 2 payload bytes) would let
that alternation corrupt `Data[0]`, the address byte itself, mid-burst.

**Periodic re-trigger** (`rpc_primitive.rs::rpc_start_com_primitive`): a
cyclic broadcast `CoptSendrecv` (`num_send_cycles == -1`) bypasses the
ordinary `tx_queue`/poll-task dispatch pipeline entirely and issues a real
native `PassThruStartPeriodicMsg` call directly — that pipeline calls
`PassThruWriteMsgs` once per configured period, which would re-emit the
fixed 5-frame burst every tick instead of a single alternating frame after
the initial burst. This is a narrow, deliberate reintroduction of the
periodic-message call for this one use only; it does not reopen ADR-093's
decision to move tester-present off native periodic dispatch (the two
cases differ in what a software poll loop can reproduce — see ADR-192's
Decision item 2 for the full comparison). `rpc_link.rs`/`service.rs` track
the started `PeriodicMessageId` on a new
`LogicalLinkState::tp20_broadcast_periodic:
Option<Tp20BroadcastPeriodic { cop_handle, message_id, started_epoch, pending_clear_generation }>`
field (`started_epoch` added by the round-5 periodic-clear epoch fix below;
`pending_clear_generation` added by the later sentinel deferral fix, also below) and call
`stop_periodic_message` on `CoptCancel` of that COP and on
`DisconnectComLogicalLink`/`DestroyComLogicalLink` of the owning CLL —
including the shared-channel case, where a sibling CLL keeping the physical
channel open must not suppress the stop call. This deliberately reinstates
the ADR-010 leak class for this one call site (ADR-093's "ADR-010 fully
superseded" claim held only because tester-present never started a
periodic message anymore; it does not extend to this new, separate use).
`rpc_misc.rs`'s `CLEAR_PERIODIC_MSGS` IOCTL handler (channel-wide) now also
finalizes any affected broadcast-periodic COP to `PduCopstFinished` when it
clears `tp20_broadcast_periodic` device-side, rather than leaving the COP
claiming a `PeriodicMessageId` the native call already invalidated. Only one
live broadcast-periodic message is supported per CLL — a second concurrent
start is rejected (`FailedPrecondition`) rather than silently clobbering the
first one's tracking `Option` and orphaning its native periodic message
(edge-case-hunter follow-up). `CoptStopcomm`'s cancel-all-queued-primitives
sweep, `PDU_IOCTL_CLEAR_TX_QUEUE`'s `cop_handles` filter, and (defensively)
`cancel_send_recv_cops_for_cll` all exclude a live broadcast-periodic COP's
handle from `cancelled_cops`, the same way they already exclude
`executing_cop`/detached-tier-2 handles — without this, `GetStatus` would
report the COP `Cancelled` while its native periodic message was still
actually transmitting (also an edge-case-hunter follow-up). Starting the
periodic message itself is a check-and-reserve done in one atomic critical
section — `message_id: Option<PeriodicMessageId>` is written as `None`
before the native call's `.await` and overwritten with `Some(id)` on success
(or rolled back to the enclosing `Option<Tp20BroadcastPeriodic>`'s own
`None` on failure) — closing a TOCTOU race two concurrent starts could
otherwise exploit to clobber each other the same way (Codex review, P1, PR
#101); every site reading `tp20_broadcast_periodic` treats a `None`
`message_id` as "a start is in flight, nothing real to stop yet." The
`None` sentinel is out-of-band by construction, so it can never collide
with a genuine adapter-assigned id: SAE J2534-1 clause 7.2.7.2 places no
floor on `PassThruStartPeriodicMsg`'s `pMsgID` output, so `Some(
PeriodicMessageId(0))` is a legitimate, real, live message and is treated
as such by every comparison site — a prior `PeriodicMessageId(0)` in-band
sentinel convention was replaced with this `Option`-based one for exactly
this reason (Codex review, PR #101). A `CoptCancel` whose
native `PassThruStopPeriodicMsg` call itself fails does NOT silently report
`PduCopstCancelled` — the message may still be transmitting device-side, so
the tracking entry is restored onto the CLL and a gRPC error is returned
instead, leaving `GetStatus` correctly reporting `Executing` until a later
retry actually succeeds (Codex review, P2, PR #101).

**Shared-channel teardown/hard-error leak-tracking (Fix B, design-advisor
consult, Codex review round 3, ADR-192 Decision item 2).** A failed native
`stop_periodic_message` during `DisconnectComLogicalLink`/
`DestroyComLogicalLink` teardown while a sibling CLL keeps the physical
channel open, or during `events::handle_channel_hard_error`'s dead-link
drain (no native stop attempted there — `api` is not held at that point),
is no longer a bare log-and-drop: the `PeriodicMessageId` is pushed onto a
new `SharedChannel::leaked_periodic_message_ids: Vec<(PeriodicMessageId, u64)>`
(`service.rs`; the paired `u64` is the round-5 periodic-clear epoch, below),
mirroring `leaked_repeat_message_ids`'s own precedent
(ADR-165 Decision 6) exactly — dup-checked pushes so a concurrent teardown
racing the same message onto the list twice cannot double-push; the
`None` in-flight-start sentinel has nothing real to leak-track and is
skipped.
Unlike the repeat-message pair, retry and prune are ONE function, not a
split pair — `rpc_misc.rs::retry_leaked_periodic_message_stops` — since the
native periodic-message API has no QUERY primitive; the retry's own
`stop_periodic_message` attempt IS the liveness probe (`Ok` or
`ERR_INVALID_MSG_ID` prunes the id, any other error keeps it tracked). Three
consumers: `rpc_link.rs::rpc_lock_resource`'s `LOCK_PHYSICAL_TX_QUEUE` grant
scan retries/prunes each candidate channel's list right alongside its
existing `leaked_repeat_message_ids` check, rejecting the grant if anything
still remains after pruning; `rpc_misc.rs`'s `CLEAR_PERIODIC_MSGS` success
path `retain`s the channel's list epoch-gated (round 5, below) rather than
clearing it outright; and the `ref_count == 0` backstop (both teardown
sites) debug-logs and drops any remainder when the `SharedChannel` entry
itself is removed, the same shape `leaked_repeat_message_ids`' own backstop
uses. Deliberately NOT added to `rpc_start_com_primitive`'s own
periodic-start path — that function never takes `shared_channels` (a
documented lock-ordering choice), and the other three sites plus the
backstop are sufficient coverage without paying that lock-order cost.

**Periodic-clear epoch fix (Codex review round 5, PR #101, ADR-192/Phase 7
Stage 7c, design-advisor consult).** Closes a race Fix B/Fix 1 above did not
cover: `CLEAR_PERIODIC_MSGS`'s reconciliation scan used to unconditionally
`take()` every CLL's `tp20_broadcast_periodic` sharing the cleared channel,
even a REAL, already-committed entry whose native `start_periodic_message`
call actually raced AFTER `clear_periodic_messages`'s own native call
returned (both calls serialize on the shared `self.api` mutex, but the two
calls' own `logical_links`/`shared_channels` reconciliation steps do not) —
wrongly finalizing a message that was never actually touched by the
native clear, leaking it live device-side with zero tracking anywhere. Fixed
with a single global `J2534Service::periodic_clear_epoch: Arc<AtomicU64>`,
bumped by exactly one (`fetch_add`, `Ordering::Relaxed`) while still holding
`self.api`, immediately after a successful `clear_periodic_messages` call
returns; read the same way, immediately after a successful
`start_periodic_message` call returns, and stashed as the new
`Tp20BroadcastPeriodic::started_epoch` field on the committed entry (also
paired onto `leaked_periodic_message_ids` entries, above). Because both
native calls serialize on `self.api`, comparing a committed entry's
`started_epoch` against a LATER clear's own post-increment value
(`clear_generation`) tells the reconciliation scan whether that entry's
native start provably preceded this clear's native call
(`started_epoch < clear_generation`, so the entry is finalized) or
raced/followed it (`started_epoch >= clear_generation`, so the entry is left
live and tracked) — regardless of how the two operations' own
`logical_links`/`shared_channels` bookkeeping happens to interleave
afterward. Unit-tested directly in
`rpc_primitive.rs::finalize_or_orphan_broadcast_periodic_start_tests`
(`started_epoch` round-trips through a caller-supplied value, and through
`periodic_clear_epoch`'s own live value) and end-to-end in
`tests/grpc_mock/tp20.rs` (a pre-clear-epoch entry is reconciled as before;
a post-clear-epoch entry is left alone, not finalized, and a later
`CancelComPrimitive` on it still works normally). See ADR-192 Decision item
2 for the full rationale, and the Prioritized Backlog for the
narrower residual (an id-targeted terminator's own stop call racing this
same native clear) this fix does not close.

**Sentinel deferral fix (edge-case-hunter finding, design-advisor-approved,
same PR/ADR-192 Decision item 2).** The periodic-clear epoch fix above left
a `None`-sentinel in-flight-start reservation (written by
`reserve_tp20_broadcast_periodic` before its own native
`start_periodic_message` call has returned) always still take-and-finalized
by the scan regardless of epoch — but a reservation's ordering against this
clear is genuinely undecidable at scan time (its native call hasn't
returned yet, so no `started_epoch` exists to compare), and if that
in-flight native call actually completes AFTER the clear's own native call,
finalizing the reservation gives the client a premature `PduCopstFinished`
for a COP whose broadcast message hadn't even reached the device yet. Fixed
by deferring the decision to commit time: a new `Tp20BroadcastPeriodic::
pending_clear_generation: u64` field (meaningful only while
`message_id.is_none()`; `0` means "no scan has passed over this reservation
yet", a safe sentinel since `clear_generation` is a post-increment counter
starting at 1) is stamped by the scan via `p.pending_clear_generation =
p.pending_clear_generation.max(clear_generation)` (the `.max`, not a plain
overwrite, so a SECOND racing clear can't regress an already-recorded
higher generation from a first clear) instead of taking the entry.
`finalize_or_orphan_broadcast_periodic_start` (`rpc_primitive.rs`) resolves
the reservation's true fate once its own native call completes and
`started_epoch` becomes known: `started_epoch < pending_clear_generation`
means at least one recorded clear's native call ran after this start's own
returned, so the message is already dead device-side (taken and finalized
via `events::emit_terminal_if_live` — the same helper the scan's own
take-and-finalize path uses — with no per-id native stop, since the
channel-wide clear already did the device-side equivalent);
`started_epoch >= pending_clear_generation` (including the `0` case)
commits it live exactly as an ordinary reservation would. Accepted
transient: between the scan recording `pending_clear_generation` and the
native call resolving, `GetStatus` briefly still reports `Executing` for a
COP whose message may already be cleared device-side — bounded by the
in-flight native call's own duration, not indefinite. This deferral is
specific to `CLEAR_PERIODIC_MSGS`; the other four terminators (`CoptCancel`,
suspension-termination, teardown, hard channel error) intentionally remain
unconditional sentinel-takers, since their termination isn't contingent on
native-call ordering against a channel-wide clear.

`rollback_tp20_broadcast_periodic_reservation` (`rpc_primitive.rs`) — called
when the native start then FAILS — was the specific trap this fix had to
avoid: its own sentinel-ownership check used to be a full-struct equality
comparison against `{ message_id: None, started_epoch: 0 }`, which (with no
`pending_clear_generation` field in the literal) would silently stop
matching a reservation the scan had since stamped a nonzero
`pending_clear_generation` onto, leaving a stale sentinel stuck forever —
blocking every future start on that CLL and blocking
`LOCK_PHYSICAL_TX_QUEUE` grants. Both this method and
`finalize_or_orphan_broadcast_periodic_start`'s own ownership check now use
a field-wise match (`cop_handle` + `message_id.is_none()`, ignoring
`pending_clear_generation`'s value) instead. Unit-tested in
`rpc_primitive.rs::finalize_or_orphan_broadcast_periodic_start_tests`
(`resolves_live_when_started_epoch_at_or_after_pending_clear_generation`,
`resolves_finished_when_started_epoch_before_pending_clear_generation`,
`double_clear_generation_resolves_finished_for_g1_and_live_for_g2`,
`rollback_clears_sentinel_even_after_pending_clear_generation_stamped` —
this last one is the rollback-trap regression test, confirmed to fail
against the pre-fix full-struct-equality comparison) and in
`rpc_misc.rs::clear_periodic_msgs_epoch_gating_tests`
(`sentinel_scan_defers_instead_of_finalizing_and_stamps_pending_clear_generation`,
`second_racing_clear_does_not_regress_pending_clear_generation`).

**TX-suspension synchronous rejection.** Unlike an ordinary transmitting
ComPrimitive — accepted and queued in `tx_held`, then dispatched once a
suspension clears (ISO 22900-2 §9.4.13.3 use case 1, ADR-123) — a broadcast
periodic start has no `tx_held`-shaped "accept now, dispatch later"
representation to defer into. `rpc_start_com_primitive` therefore reads
this CLL's live `tx_suspended()` state fresh (not from the earlier
`LinkView` snapshot) immediately before issuing the native call, and
rejects synchronously with `Status::failed_precondition` if TX dispatch is
currently suspended — by this CLL's own `PDU_IOCTL_SUSPEND_TX_QUEUE`, a
sibling CLL's held `LOCK_PHYSICAL_TX_QUEUE`, or `CP_SuspendQueueOnError`.
Starting it anyway would put real broadcast traffic on the wire while a
sibling CLL believes it holds exclusive transmit privilege over the shared
physical resource, defeating `LOCK_PHYSICAL_TX_QUEUE`'s purpose. A
single-shot broadcast burst (`num_send_cycles == 1`) is unaffected, since
it dispatches through the ordinary `tx_queue`/`dispatch_tx_item` pipeline
and is already correctly siphoned/held by the existing mechanism. The
reverse direction is also guarded: `rpc_link.rs::rpc_lock_resource` scans
for a live (or reserved) `tp20_broadcast_periodic` on any CLL sharing the
physical resource — including the requester's own, no self-skip, matching
the existing `repeat_message_ids` check's identical precedent (ISO 22900-2
§9.4.13.2(b) has no "other CLL" qualifier) — and rejects a
`LOCK_PHYSICAL_TX_QUEUE` grant attempt while one is active, closing the gap
where a lock holder could otherwise believe it had exclusive transmit
access while a broadcast periodic message kept transmitting regardless
(Codex review, P1, PR #101).

**Suspension terminates a live broadcast periodic too (Fix A, design-advisor
consult, Codex review round 3, ADR-192 Decision item 2).** The synchronous
rejection above only ever covered a NEW start; leaving an ALREADY-RUNNING
broadcast periodic going under the identical suspension sources was the
incoherent middle state that review round flagged. All four
suspension-authority sites now terminate a live broadcast periodic in the
same critical section that sets their own suspension flag —
`rpc_misc.rs::ioctl_suspend_tx_queue`, and `events.rs`'s three
`CP_SuspendQueueOnError` transition sites (the batch-final reconciliation
authority and the two receive-phase timeout hooks; the fourth,
eager-mid-batch `tx_suspended_by_error = true` publish a later batch-final
pass can still override is deliberately excluded — terminating a COP on a
provisional classification would be premature). Each site takes
`link.tp20_broadcast_periodic` under `logical_links`, then calls the shared
`J2534Service::terminate_tp20_broadcast_periodic_for_suspension` helper
(`rpc_misc.rs`) after releasing that lock — native calls need `self.api`,
which must never be held together with `logical_links` (ADR-080). On
success (or the `None`-sentinel in-flight-start skip), the COP is finalized
with `PduCopstFinished`, not `PduCopstCancelled` — mirroring
`CLEAR_PERIODIC_MSGS`'s own reconciliation shape, since this is a side
effect of an administrative action, not a client-initiated
`CancelComPrimitive`. On a real native-stop failure the entry is restored
onto the CLL (mirroring `rpc_cancel_com_primitive`'s own restore-on-failure)
rather than leak-tracked — the CLL itself is not going away here, unlike
Fix B's shared-channel-teardown/hard-error case. `PDU_IOCTL_RESUME_TX_QUEUE`
needs no change: a suspension-terminated broadcast periodic is simply done,
like any other `CoptCancel`-terminated COP, with nothing left to resume.

**RX-side loopback classification** (`events.rs::poll_rx_inner`): a
broadcast frame's TX-side echo (`[address] ++ payload`) does not match
either of Stage 7a's two established-connection routing tiers
(`tp20_rx_id`/`tp20_tx_id`), and must never be delivered to any CLL — a
sibling CLL whose own `tx_id` happens to coincide with the broadcast
frame's address prefix would otherwise receive it, the same leak class
ADR-188's Fixes N/P already closed twice for the connection-bound case.
Classified and dropped directly from the raw frame's own first data byte
(`data[0]` in `0xF0`-`0xFF`, `is_tx_side`, TP2.0 protocol), unconditionally,
before the frame ever reaches per-CLL routing — mirroring the existing
`CONNECTION_ESTABLISHED`/`_LOST` indication early-drop just above it in the
same function. **Update (Codex review round 2, PR #101):** the first
implementation classified this per-CLL instead, inside
`events_rx_routing.rs::UniqueRespIdKey::matched`, by reinterpreting the
frame's leading bytes as a big-endian `can_id` the same way a genuine
connection-bound echo is read — but `route_frame` only ever handed that
classifier a `can_id` when the frame was `>= 4` bytes, and a broadcast's
legal composed size is `3..=8` bytes, so the minimum-size (3-byte) case
always produced no `can_id`, hit `route_frame`'s too-short-frame fallback
(deliver unconditionally, no routing at all), and leaked every burst to
every sibling CLL — the classifier was dead code for the case most likely
to actually occur, and has been removed now that the earlier, length-
independent drop in `poll_rx_inner` makes it unreachable. The safety
argument is unchanged, just checked on the raw byte directly: a genuine
established TX-ID's own top byte (`data[0]` of a connection-bound send)
can never reach `0xF0`-`0xFF`, since it is masked to SAE J2534-1's 29-bit
CAN identifier space (`0x1FFF_FFFF`) at its parse site (`events.rs`'s
`CONNECTION_ESTABLISHED` indication-parsing arm, edge-case-hunter
follow-up) — a malformed indication with non-zeroed upper bits could
otherwise misclassify every one of that connection's own legitimate
TX-side echoes as a broadcast echo.

**Timing ComParam** (`service_params.rs`/`comparam_defaults.rs`): a second
new ComParam, `CP_TP20BroadcastInterval` (`0x80D1`,
`PARAM_TP20_BROADCAST_INTERVAL`), routed to native `CONFIG_TP2_0_T_BR_INT`
(`0x8044`) through the ordinary generic ComParam→`SET_CONFIG` pipeline
(default 20, Table 77's own default) — unlike the broadcast address, this
one IS a real `SET_CONFIG`-forwarded, hardware-resident setting. It is
classified pacing-class, not `PDU_PC_BUSTYPE`, though — the same
`CP_Cs`→`CONFIG_J1939_BRDCST_MIN_DELAY` precedent (`comparam_id.rs`) already
establishes that "per-channel, hardware-resident" and "`PDU_PC_BUSTYPE`" are
not the same thing: BUSTYPE means bus-configuration-class (baud rate, sample
point, termination), not merely "has a native `SET_CONFIG` id." **Update
(Codex review round 8 Finding 2, PR #101):** Codex proposed reclassifying it
as `PDU_PC_BUSTYPE` (adding it to `comparam_support.rs`'s `BUSTYPE_UNUM32`
array) so `LOCK_PHYSICAL_COM_PARAMS` would gate it; rejected, since
`BUSTYPE_UNUM32` membership also drives `bustype_params_differ`, the guard
`temp_param_update` staging must pass (ISO 22900-2 §9.4.16.2.1's own NOTE on
the BUSTYPE class's `temp_param_update` prohibition) — reclassifying this
param would make that guard reject this ADR's own already-shipped
`temp_param_update=1` per-burst override of it (see "Periodic re-trigger"
above, Fix 5). Accepted residual: because it stays outside `BUSTYPE_UNUM32`,
`LOCK_PHYSICAL_COM_PARAMS` never gates it — a sibling CLL sharing the
physical channel can still move the standing hardware value between this
CLL's own `Plain`-bound bursts, the same behavior every other non-BUSTYPE
`SET_CONFIG`-forwarded ComParam already has under ADR-110's "non-conflicting
params are still pushed even when locked" design; a client wanting this
burst's own value isolated from that already has the `temp_param_update=1`
override for it. Three regression-fence unit tests in
`comparam_support.rs` (`tp20_broadcast_interval_is_not_a_bustype_param`,
`bustype_params_differ_false_when_only_tp20_broadcast_interval_differs`,
`com_param_class_tp20_broadcast_interval_is_pdu_pc_specified`) and an
integration fence in `tests/grpc_mock/tp20.rs`
(`broadcast_periodic_temp_bound_start_succeeds_despite_sibling_holding_com_param_lock`)
pin this decision against a future well-intentioned re-attempt. It is the
only one of Table 77's ten TP2.0 timing parameters wired by any TP2.0 stage
to date; the other nine remain exactly as ADR-188 §4 deferred them.

**Mock support** (`j2534-0404-mock`): `Data[0]`-range validation
(`ERR_INVALID_MSG` outside `0xF0`-`0xFF`, defense in depth alongside this
service's own pre-native-call rejection) and a working
`PassThruStartPeriodicMsg`/`PassThruStopPeriodicMsg` pair for the broadcast
case, verified not to have bit-rotted since ADR-093 removed tester-present's
own usage of it.

**Accepted limitations (ADR-192 Consequences):** no per-cycle events for a
broadcast periodic COP — the native periodic-message pair gives the same
zero-per-tick-visibility ADR-083 originally documented for tester-present
mode 0 (a single confirmation at start, nothing per actual transmission);
no live re-arm — a rate or address change requires cancelling and
re-starting, a direct consequence of using the native hardware-autonomous
primitive rather than a software poll loop; `num_send_cycles` values other
than `1` (single burst) or `-1` (infinite) are not supported, since no
native primitive exists for any other finite repeat count; the native
adapter's own `TimeInterval` band (SAE J2534-1's 5-65535 ms) is enforced by
the device, not synthesized here, matching ADR-083's prior reasoning for
not hardcoding a range check.

**Tests** (`tests/grpc_mock/tp20.rs`): broadcast composition and the
device's simulated 5-frame alternating burst, with no `CoptStartcomm` ever
issued
(`broadcast_send_via_temp_comparam_writes_five_alternating_frames`);
out-of-range address and unsupported finite `num_send_cycles` rejections
(`broadcast_send_with_out_of_range_address_is_rejected`,
`broadcast_send_with_unsupported_finite_num_send_cycles_is_rejected`); a
nonzero `num_receive_cycles` rejection
(`broadcast_send_with_nonzero_num_receive_cycles_is_rejected`); the
periodic re-trigger's native `PassThruStartPeriodicMsg` call and immediate
burst
(`broadcast_periodic_starts_native_periodic_message_with_immediate_burst`);
`CoptCancel`/`DisconnectComLogicalLink`/`CLEAR_PERIODIC_MSGS` each stopping
or reconciling the live native periodic message
(`cancel_com_primitive_stops_the_native_broadcast_periodic_message`,
`disconnect_com_logical_link_stops_the_native_broadcast_periodic_message`,
`clear_periodic_msgs_reconciles_a_live_broadcast_periodic_cop`); the
RX-side echo-classification fix, proven end-to-end against a sibling
established connection sharing the physical channel
(`broadcast_echo_is_dropped_not_misrouted_to_a_sibling_established_connection`);
and the TX-suspension synchronous-rejection fix, both via a sibling CLL's
held `LOCK_PHYSICAL_TX_QUEUE`
(`broadcast_periodic_rejects_when_a_sibling_holds_the_physical_tx_queue_lock`)
and via this CLL's own `PDU_IOCTL_SUSPEND_TX_QUEUE`
(`broadcast_periodic_rejects_when_this_cll_own_tx_queue_is_suspended`),
each confirming no native periodic message is ever started. The
edge-case-hunter follow-up fixes add: a second concurrent broadcast-periodic
start rejected rather than clobbering the first
(`second_concurrent_broadcast_periodic_start_is_rejected_not_clobbered`);
`PDU_IOCTL_CLEAR_TX_QUEUE` and `CoptStopcomm`'s own cancel sweep each
leaving a live broadcast-periodic COP un-`Cancelled`
(`clear_tx_queue_does_not_mark_a_live_broadcast_periodic_cop_cancelled`,
`coptstopcomm_cancel_sweep_does_not_mark_a_live_broadcast_periodic_cop_cancelled`);
and the corrected `3..=8` size floor's boundary, in `rpc_primitive.rs`'s own
unit tests
(`broadcast_message_below_the_three_byte_address_safety_floor_is_rejected`,
`broadcast_message_at_exactly_the_three_byte_floor_is_accepted`). Codex
review adds: a single-shot broadcast on a CLL with an established
connection is not corrupted by the dispatch-time TX-ID overwrite
(`broadcast_send_on_a_cll_with_an_established_connection_is_not_corrupted`);
the atomic reservation's TOCTOU-closing behavior (re-verified against the
same sequential-clobber scenario,
`second_concurrent_broadcast_periodic_start_is_rejected_not_clobbered`); a
`CoptCancel` whose native stop fails restores tracking and errors instead of
falsely reporting `Cancelled`
(`cancel_com_primitive_restores_tracking_and_errors_when_the_native_stop_fails`,
using a new mock fault-injection backdoor,
`__mock_set_stop_periodic_message_error`); `LOCK_PHYSICAL_TX_QUEUE`
rejecting both a sibling's and the requester's own live broadcast-periodic
message
(`lock_physical_tx_queue_rejects_while_a_sibling_has_a_live_broadcast_periodic_message`,
`lock_physical_tx_queue_rejects_for_this_clls_own_live_broadcast_periodic_message`);
and the minimum-size (3-byte) broadcast-echo RX-routing-leak fix, folded
into the existing echo-classification test
(`broadcast_echo_is_dropped_not_misrouted_to_a_sibling_established_connection`).

**Codex review round 4 (PR #101) adds five more fixes to the periodic-start
branch, all in `rpc_start_com_primitive`'s `is_broadcast_send &&
num_send_cycles == -1` arm:** Fix 1 (P1) best-effort stops a native periodic
message that started successfully AFTER something else (`CoptCancel`, a
suspension-triggered termination, `CLEAR_PERIODIC_MSGS`, or CLL teardown)
already took/cleared this cop_handle's own `None`-sentinel reservation, closing
the "orphaned with nothing left tracking it" gap the Prioritized
Backlog previously documented as an accepted residual for this exact scenario
(the surviving backlog entry, "ADR-192/Phase 7 Stage 7c Fix 1, accepted
residual", now only covers a failure of THIS best-effort stop itself). Fix 2 (P1) + Fix 3 (P1) merge the suspension check,
the already-active check, and the reservation write into one `logical_links`
critical section (a new `reserve_tp20_broadcast_periodic` helper) and widen
it to also re-verify `connect_generation`/`connected` against the earlier
`LinkView` snapshot, reading `channel_id`/`hw_protocol_id` from the LIVE
`LogicalLinkState` rather than the possibly-stale snapshot -- unit-tested
directly (`reserve_tp20_broadcast_periodic_tests`, `rpc_primitive.rs`) with
matching-generation/mismatched-generation/disconnected-same-generation/
suspended/already-active/vanished-CLL fixtures, mirroring
`rollback_stop_comm_pending_tests`'s own documented reasoning for why a
genuine disconnect/reconnect-during-the-race scenario is infeasible to
construct through this crate's gRPC test harness; the existing
`broadcast_periodic_rejects_when_this_cll_own_tx_queue_is_suspended`
end-to-end test re-confirms the merged critical section leaves the
suspension rejection unaffected. Fix 4 (P2) extends the raw out-of-range
`CP_TP20BroadcastAddress` rejection (previously `CoptSendrecv`-only) to
`CoptStartcomm`'s own optional-message path too, via a shared
`validate_tp20_broadcast_address_range` helper
(`startcomm_optional_message_with_out_of_range_broadcast_address_is_rejected`;
`CoptStopcomm`'s own optional-message path gets the identical guard, sharing
the same helper, though ADR-192's own test-plan slot for it is this same
entry since no additional CoptStopcomm-specific scenario differs from the
CoptStartcomm one). Fix 5 (P2) wraps the native
`PassThruStartPeriodicMsg` call in the same temp-apply-then-revert bracket
`handle_send_recv` runs around an ordinary transmit (ISO 22900-2 §9.4.3,
`events::apply_params_to_hardware`/`events::revert_hardware_to_live_active`,
both widened from private to `pub(super)` for this new call site), so a
`temp_param_update=1` broadcast periodic start with `CP_TP20BroadcastInterval`
staged into Working actually pushes it to hardware for the start, then
reverts immediately after to the live revert target (round 18, ADR-192
Decision item 3 amendment, below: for this specific channel-wide key, the
revert target is the captured pre-bracket hardware value, not this CLL's
own Active snapshot -- see that entry for the full mechanism)
(`broadcast_periodic_temp_param_update_applies_and_reverts_broadcast_interval`
-- asserts the deterministic, non-racy halves of this contract, not the
momentarily-applied intermediate value: unlike `CoptSendrecv`/`CoptStartcomm`,
a broadcast periodic start is fully synchronous within the RPC handler
itself, so there is no asynchronous window a black-box gRPC/backdoor poll
could observe the Working value through before the revert already ran,
the way `pin_selection.rs`'s own temp-apply/revert tests can for the
poll-task-dispatched ordinary-send case; see the test's own doc comment).

An `edge-case-hunter` follow-up on this same PR caught that
`terminate_tp20_broadcast_periodic_for_suspension`'s and `CLEAR_PERIODIC_MSGS`'s
own broadcast-periodic COP finalization each hand-rolled a
remove-then-`send_cop_status` instead of using the crate's existing
`emit_terminal_if_live` helper (`events_event_senders.rs`) -- reopening the
exact A2-23 race that helper exists to close (dropping the `primitives`
guard before the `send_cop_status` await lets a concurrent
`CancelComPrimitive`/`GetStatus` see a miss on both `primitives` and
`terminal_cops`), and never draining a stale `cancelled_cops` mark the way
`emit_terminal_if_live` also does. Both sites now call it directly;
`emit_terminal_if_live` was widened from `pub(super)` to
`pub(in crate::service)` (matching `send_cop_status`'s own visibility) so
`rpc_misc.rs` can reach it.

**Codex review round 8 Finding 1 (PR #101): the temp-apply/native-start/
revert bracket (Fix 5, above) is now one continuously-held `self.api` guard,
not three separately acquired ones.** The first implementation of Fix 5
still acquired and released `self.api` separately for the apply
(`events::apply_params_to_hardware`), the native `start_periodic_message`
call, and the revert (`events::revert_hardware_to_live_active`) — three
independent `self.api.lock().await`s with real gaps between them, letting a
concurrent operation on the same physical channel (a sibling CLL's own
temp-bound broadcast start, or a queued `CoptUpdateparam`) interleave and
corrupt which hardware value the burst actually transmits with. Fixed by
acquiring `self.api` once, before the apply step, and holding it
continuously through the apply, the native call plus the round-5
periodic-clear-epoch read immediately after it (which already had to share
a guard with the native call), and the revert — on every exit path (apply
failure, native-start failure, native-start success) — dropping it only
afterward, before touching `self.logical_links` for a `last_error` read,
calling `rollback_tp20_broadcast_periodic_reservation`, returning an `Err`,
or calling `finalize_or_orphan_broadcast_periodic_start` (which itself
separately re-acquired both `self.logical_links` and `self.api`, and so must
not find this outer guard still held). Round 15's Fix 2 (ADR-193, below)
narrows that last clause: the finalization's `logical_links` resolution and
its orphan stop now run INSIDE this same guard
(`finalize_or_orphan_broadcast_periodic_start_locked`), and only the
bookkeeping half runs after `drop(api)`. This is sound under the ADR-110
amendment's lock-ordering invariant ("Lock-grant/apply serialization",
Finding 2): `api` outer, `logical_links` inner, whenever both are held
together — reading `logical_links` (the revert's own live-Active lookup)
while `api` is already held is the SANCTIONED order, not the forbidden
reverse one (never acquire `api` while already holding `logical_links`);
`events.rs::handle_update_param` is the established precedent for this exact
nesting. Two comments in `events.rs` (near `handle_update_param`'s own
`apply_params_to_hardware_locked` call and its `claim_owns_address`
rejection) previously misstated this invariant backwards ("must be dropped
before ... re-locks `logical_links`, which must never happen while `api` is
held") — corrected in place to say `api` drops there for prompt-release
hygiene, not because of any ordering requirement forcing it.

`events::apply_params_to_hardware_locked` (already existing, for
`handle_update_param`'s own analogous bracket) was made `pub(super)` and
reused directly; a new `events::revert_hardware_to_live_active_locked` was
added as the `_locked` counterpart of the existing
`revert_hardware_to_live_active` wrapper (mirroring the existing
`apply_params_to_hardware`/`apply_params_to_hardware_locked` pairing) —
every other existing call site of `revert_hardware_to_live_active`
(`handle_send_recv`, `handle_start_comm`'s several paths) keeps working
unchanged through the now-thin wrapper. The revert still performs a LIVE
read of `logical_links` at each revert point, never a value pre-fetched
before the bracket, matching `revert_hardware_to_live_active`'s own general
ADR-067 contract for its other (non-`_locked`) callers, where `active` genuinely
can change mid-bracket. For THIS specific caller a pre-fetch would in fact
also be safe (edge-case-hunter, PR #101 round 8 verification): every writer
of `LogicalLinkState::active` (`handle_update_param`, `finalize_connected_link`)
itself needs `self.api` first, so none can run while this bracket already
holds it -- the live read is kept anyway for uniformity with the general
contract and because it costs nothing, not because a pre-fetch would be
unsafe here.

No literal concurrent-interleaving repro was attempted for this fix's own
regression test, for the same reason the ADR-110 amendment accepted for its
analogous `handle_update_param` fix: this crate's single-threaded
`current_thread` test runtime has no production yield hook to force the
interleaving deterministically. Two new tests instead pin the mechanism:
`events::revert_hardware_to_live_active_locked_tests::
pushes_active_with_bustype_keys_stripped`/`no_op_when_cll_absent` (direct
unit coverage of the new `_locked` function — hand-seeded `logical_links`,
an already-held mock `api` reference, asserting a BUSTYPE key is never
pushed and a non-BUSTYPE key is — since round 18, ADR-192 Decision item 3
amendment below, the first test is renamed
`pushes_active_with_bustype_and_channel_wide_keys_stripped` and now asserts
the channel-wide `CP_TP20BroadcastInterval` is ALSO stripped from the
`active` push, since it is no longer that revert's restore target; see the
round-18 entry below for the current behavior), and the existing
`tests/grpc_mock/tp20.rs::broadcast_periodic_temp_param_update_applies_and_reverts_broadcast_interval`
integration test gained a log-adjacency assertion (the apply batch and the
revert batch of `SET_CONFIG` ids must be two back-to-back, identical
sequences in the mock's own `set_config_param_log`, with nothing else
interleaved) — see that test's own doc comment for why this specific
assertion is not a literal before/after repro either (a single synchronous
client request has no concurrent op to actually interleave), but does pin
the bracket's steady-state shape.

**Accepted residual** (`edge-case-hunter`, PR #101 round 8 verification): the
apply-failure branch (`if !applied { ... }`, immediately above the native
`start_periodic_message` call) has no dedicated test, unlike the
structurally identical native-start-failure branch just below it -- the
mock has no hook to force a selective `SET_CONFIG` failure at exactly this
point (same accepted-residual class as the TP2.0 passive-listener arm's own
second-call-failure rollback path, documented above). Logically correct by
inspection; not independently constructible without new mock fault-injection
support.

**Codex review round 9 (PR #101): Repeat Messaging rejects a staged broadcast
address.** `tx_header::build_tx_message`'s TP2.0 arm is shared infrastructure
between the broadcast-COP paths and `rpc_misc.rs::ioctl_start_repeat_message`
-- it composes `[address] ++ payload` broadcast framing whenever the resolved
`CP_TP20BroadcastAddress` is nonzero, regardless of caller intent, while
`ioctl_start_repeat_message`'s own TxFlags composition has no broadcast
awareness at all and never adds `TX_FLAG_TP2_0_BROADCAST_MSG`. Since
`CP_TP20BroadcastAddress` can be staged into Active via a plain `SetComParam`
+ `CoptUpdateparam`, entirely independent of ever issuing a broadcast COP, a
`PDU_IOCTL_START_REPEAT_MESSAGE` while it is staged would compose a
broadcast-address-prefixed frame but transmit it with ordinary
connection-bound flags. Fixed by rejecting `PDU_IOCTL_START_REPEAT_MESSAGE`
synchronously (`PduErrIdNotSupported`, in the same critical section as the
existing J1939 claim-state check, before any state mutation) whenever a
TP2.0 CLL's Active `CP_TP20BroadcastAddress` is staged -- a scope exclusion
(ADR-192 Decision item 1), not a gap this PR intends to close by extending
Repeat Messaging to also support broadcast framing.

**Tests** (`tests/grpc_mock/tp20.rs`):
`start_repeat_message_is_rejected_while_a_broadcast_address_is_staged` --
stages `CP_TP20BroadcastAddress` into Active via `SetComParam` +
`CoptUpdateparam`, then asserts `PDU_IOCTL_START_REPEAT_MESSAGE` fails with
`FailedPrecondition`. Confirmed to actually catch the regression by
temporarily disabling the new check and watching the test fail.

**Codex review round 10 (PR #101): two more races in the broadcast-periodic
terminator machinery, both fixed and described in ADR-192 Decision item 2's
own round-10 update paragraph rather than restated in full here.**

Fix A (P1) closes a stale-snapshot race at two of the three
`cancelled_cops` exclusion sites for a broadcast-periodic COP
(`rpc_misc.rs`'s `PDU_IOCTL_CLEAR_TX_QUEUE` handler and
`rpc_primitive.rs`'s `CoptStopcomm` queued-primitive cancellation block): a
concurrent `StartComPrimitive` publishing a fresh reservation into
`link.tp20_broadcast_periodic` between the early snapshot and the final
`cancelled_cops.extend` used to be invisible to the stale snapshot, wrongly
marking the freshly reserved COP cancelled. Both sites now re-read
`link.tp20_broadcast_periodic` fresh, under the same lock the final extend
already holds -- matching `events_j1939_claim.rs::cancel_send_recv_cops_for_cll`'s
own already-correct shape (third site, unmodified).

Fix B (P2) gates `terminate_tp20_broadcast_periodic_for_suspension`'s
failed-native-stop restoration on the captured `connect_generation` still
matching the live link's -- a concurrent disconnect/reconnect racing the
failure branch could otherwise make the tracking slot read as empty for an
unrelated reason and wrongly graft an old channel's failed transmitter onto
a new session. On a generation mismatch (or the link being gone), the
failed stop is now leak-tracked against the captured `channel_key`'s
`SharedChannel::leaked_periodic_message_ids` instead, mirroring
`DisconnectComLogicalLink`'s own Fix B leak-tracking shape, or logged as an
accepted-residual double-fault if the physical channel has also fully
closed. Two new parameters, `connect_generation: u64` and
`channel_key: Option<ChannelKey>`, are threaded through all four call sites
(`ioctl_suspend_tx_queue` and `events.rs`'s three `CP_SuspendQueueOnError`
transition sites), captured from the same `link`/lock scope every caller
already takes `periodic`/`channel_id` from.

Fix B follow-up (`edge-case-hunter`, PR #101 round 10 verification): the
mismatch branch's leak-track originally trusted `channel_key` alone, but a
`ChannelKey` (protocol/baud/flags/pin) has no uniqueness-over-time
guarantee -- a channel close followed by an unrelated fresh
`ConnectComLogicalLink` at the same params installs a brand-new
`SharedChannel` at the identical key, so the leak-track could land on a
channel that never actually ran the failed message. Gated on the captured
`channel_id` also still matching the live `SharedChannel`'s own
`channel_id` (see ADR-192 Decision item 2's own round-10 update paragraph
for the full accepted-residual reasoning and the deferred `occupancy_epoch`
alternative).

**Tests** (`rpc_misc.rs::terminate_tp20_broadcast_periodic_for_suspension_tests`,
direct unit coverage -- a genuine disconnect/reconnect landing in this
narrow window is infeasible to construct through the gRPC layer with this
crate's established techniques, the same class of limitation
`rollback_stop_comm_pending_tests` already documents):
`generation_mismatch_leak_tracks_instead_of_restoring_when_channel_survives`
-- a generation mismatch with a surviving `shared_channels` entry asserts
nothing is restored onto the link and the failed stop lands in
`leaked_periodic_message_ids`; `generation_mismatch_with_no_surviving_channel_does_not_panic_or_leak`
-- the same mismatch with no matching `shared_channels` entry asserts
nothing is restored or leaked and no panic occurs;
`generation_mismatch_with_channel_id_mismatch_does_not_leak_onto_unrelated_channel`
-- the same mismatch with a `shared_channels` entry present at `channel_key`
but a DIFFERENT `channel_id` (simulating an unrelated channel reusing the
same key) asserts the leak-track is skipped, not misattributed. All three
use `__mock_set_stop_periodic_message_error` (the round-2 `CoptCancel`
fault-injection backdoor, commit 3f3cd98) to force the native
`PassThruStopPeriodicMsg` call to fail deterministically.

**Codex review round 11 (PR #101): two P2 findings about the same
underlying gate, in two separate places -- fixed together by extracting
the decision into one shared helper, described in full in ADR-192
Decision item 2's own round-11 update paragraph rather than restated here.**

Finding 1: round 10's `same_session` gate (above) compared only
`connect_generation`, but a plain disconnect with no reconnect never bumps
it (`rpc_link.rs`'s disconnect path only clears `connected`/`channel_id`),
so the gate still read as a session match after a plain disconnect and
wrongly restored the failed entry onto the now-disconnected link. Finding
2: `rpc_cancel_com_primitive`'s `CoptCancel` handling of a TP2.0
broadcast-periodic COP had the identical unprotected hazard -- it restored
a failed stop's entry onto the CLL unconditionally, with no
session/generation/connected check at all, because it never received
round 10's Fix B protection in the first place.

Both are closed by a new shared method,
`restore_or_leak_track_broadcast_periodic` (`rpc_misc.rs`), called by both
`terminate_tp20_broadcast_periodic_for_suspension` and `CoptCancel`'s
failure branch instead of each keeping its own inline copy of the
decision: `same_session` now also requires `link.connected` (mirroring
`reserve_tp20_broadcast_periodic`'s own identical
generation-plus-connected gate), and `CoptCancel` now captures
`connect_generation`/`channel_key` in the same `logical_links` critical
section it already captures `channel_id`/`periodic` from.

**Tests:** `rpc_misc.rs::terminate_tp20_broadcast_periodic_for_suspension_tests::
matching_generation_but_disconnected_leak_tracks_instead_of_restoring` --
finding 1's scenario (matching `connect_generation`, `connected = false`)
asserts the failed stop is leak-tracked, not restored. A new module,
`rpc_primitive.rs::rpc_cancel_com_primitive_broadcast_periodic_session_gate_tests`,
covers the same shared gate from `CoptCancel`'s own call shape: generation
mismatch with a surviving shared channel (leak-tracked), generation
mismatch with no surviving channel (nothing restored or leaked, no
panic), and matching generation with `connected = false` (leak-tracked).
Both modules call `restore_or_leak_track_broadcast_periodic` directly with
deliberately mismatched parameters rather than through the full RPC/ioctl
call -- the same class of narrow-window infeasibility
`rollback_stop_comm_pending_tests` already documents, since capturing and
re-checking the session happen synchronously within one call with no
natural preemption point for a genuine concurrent disconnect to land in.
`edge-case-hunter` additionally located and ran a pre-existing (round 2)
`grpc_mock` integration test, `tp20.rs::cancel_com_primitive_restores_tracking_and_errors_when_the_native_stop_fails`,
confirming it already exercises `CoptCancel`'s real call-site wiring
end-to-end for the same-session restore case, so the new direct-helper
tests are not the only coverage of that path through the actual RPC.

`edge-case-hunter` verification (PR #101 round 11): one minor, non-blocking
note -- `CoptCancel`'s `last_error` read now happens in its own separate
`logical_links` lock acquisition rather than inside the same critical
section as the restore decision (a cosmetic atomicity reduction from the
refactor). Traced to confirm `last_error` feeds only the diagnostic
`ErrorEventData` on the returned `Status` (`error.rs::error_event_data_for`)
and has no bearing on the restore-vs-leak-track decision itself, which
remains fully atomic inside the shared helper's own single
`shared_channels`-then-`logical_links` critical section. No fix needed.

**Codex review round 12 (P2, PR #101): a later race in the same
`finalize_or_orphan_broadcast_periodic_start` finalization path, between
the `logical_links` commit and this method's own status emission --
described in full in ADR-192 Decision item 2's own round-12 update
paragraph rather than restated here.** The `Live` resolution's
`logical_links` critical section commits the real entry (replacing the
`None`-sentinel in-flight reservation) before releasing that lock; a
concurrent `CLEAR_PERIODIC_MSGS`/suspension-termination/`CoptCancel` can
then see the freshly-committed entry, take it, stop it natively, remove
the COP from `primitives`, and emit its own terminal status -- all before
this method's own deferred `PduCopstExecuting` report runs, which would
otherwise surface `Executing` after a terminal event.

Fixed with a new sibling helper to the existing `emit_terminal_if_live`,
`events::emit_nonterminal_if_live` (`events_event_senders.rs`): it reuses
`emit_terminal_if_live`'s proven "hold the `primitives` lock across the
liveness check and the send" atomicity technique, but never removes the
entry -- a nonterminal status must never finalize a COP, it only gates the
emission on `cop_handle` still being present. `finalize_or_orphan_broadcast_periodic_start`'s
`Live` arm now calls this helper instead of `send_cop_status` directly.
`primitives` containment alone is a sufficient liveness signal for this
COP kind: round 10's Fix A already excludes a broadcast-periodic COP's own
`cop_handle` from ever being marked `cancelled_cops` while still genuinely
live and un-finalized, matching every other terminal-finalization site in
this crate.

**Tests:** `events.rs::drain_cancelled_cop_if_finalized_tests::
emit_nonterminal_if_live_sends_status_when_entry_present`/
`emit_nonterminal_if_live_is_a_no_op_when_entry_absent` -- direct coverage
of the new helper itself, mirroring `emit_terminal_if_live`'s own
present/absent pair in the same module. `rpc_primitive.rs::
finalize_or_orphan_broadcast_periodic_start_tests::
resolution_live_suppresses_executing_when_primitives_entry_already_gone`
-- reproduces the race end-to-end at the method level: `primitives` seeded
without the cop_handle while `logical_links` still carries this exact
cop_handle's own sentinel, asserting the `Live` resolution still commits
the real entry into `logical_links` (independent of `primitives`) while no
`PduCopstExecuting` is queued.

**Codex review round 13 (P2, PR #101): `restore_or_leak_track_broadcast_periodic`'s
reclaimed-slot branch now leak-tracks instead of dropping the failed stop untracked --
described in full in ADR-192 Decision item 2's own round-13 update paragraph rather than
restated here.** When `same_session` holds but `link.tp20_broadcast_periodic` was already
reclaimed by a fresh, unrelated reservation before this failure could be recorded, the helper
previously only logged a `warn!` and dropped the old message, leaving it tracked nowhere --
neither `tp20_broadcast_periodic` (reclaimed) nor `leaked_periodic_message_ids` (only the
session-mismatch branch leak-tracked). Fixed by restructuring the helper so both "can't
restore" cases -- session mismatch and same-session-with-slot-reclaimed -- fall through to the
same leak-tracking logic, gated on the live `SharedChannel::channel_id` matching the captured
one exactly as the session-mismatch case already was. The double-fault `warn!` now carries a
`slot_reclaimed` field so the two causes stay distinguishable in logs.

**Tests:** `rpc_misc.rs::terminate_tp20_broadcast_periodic_for_suspension_tests::
same_session_with_slot_reclaimed_leak_tracks_instead_of_dropping` -- seeds a live,
`connect_generation`-matching, connected link whose `tp20_broadcast_periodic` already holds a
different (fresh) reservation, calls `restore_or_leak_track_broadcast_periodic` directly with
the old (now-failed) entry, and asserts the old message lands in the shared channel's
`leaked_periodic_message_ids` while the fresh reservation occupying the slot is left untouched.
Both call sites (`terminate_tp20_broadcast_periodic_for_suspension` and `CoptCancel`) route
through this same helper, so one direct-call test covers both, matching this module's own
established pattern for the session-mismatch cases above.

**Codex review round 14 (P2, PR #101): `CoptCancel`'s failed-native-stop branch now treats
`ERR_INVALID_MSG_ID` as already-cancelled instead of a genuine failure -- described in full in
ADR-192 Decision item 2's own round-14 update paragraph rather than restated here.** When a
channel-wide clear (e.g. `CLEAR_PERIODIC_MSGS`) natively clears this exact message's slot
device-side after `CoptCancel` already took the tracking entry off the link but before its own
`stop_periodic_message` call runs, a conforming adapter returns `ERR_INVALID_MSG_ID` --
authoritative confirmation the message is already gone, not a transient failure. Before this fix,
`CoptCancel` treated every `stop_periodic_message` error identically (restore-or-leak-track, then
return a gRPC error), which left the COP `Executing` forever and made any cancel retry loop
forever against a message that will never again succeed a real stop. Fixed by checking for
`ERR_INVALID_MSG_ID` specifically and falling through to the ordinary `Cancelled` success path
instead of the restore/leak-track/error path -- completing the same pattern already established
for `retry_leaked_periodic_message_stops`, `retry_leaked_repeat_message_stops`, and the
`PDU_IOCTL_STOP_REPEAT_MESSAGE` handler (all `rpc_misc.rs`).

**Tests:** `rpc_primitive.rs::rpc_cancel_com_primitive_zero_message_id_tests::
cancel_treats_err_invalid_msg_id_from_stop_as_already_cancelled` -- seeds a live, committed
`tp20_broadcast_periodic` entry, forces the mock's `PassThruStopPeriodicMsg` to fail with
`ERR_INVALID_MSG_ID` via `__mock_set_stop_periodic_message_error`, calls `CoptCancel` through the
full gRPC handler, and asserts the RPC succeeds, the COP is removed from `primitives`, nothing is
restored onto `link.tp20_broadcast_periodic`, and nothing is leak-tracked into any
`SharedChannel::leaked_periodic_message_ids` (`shared_channels` stays empty).

This closes the Prioritized Backlog entry a round-5 design-advisor consult left open (previously
"the only genuine residual is a narrow client-visible UX wrinkle: a `CancelComPrimitive`/
suspension-termination racing this exact window returns a bare gRPC error instead of `Cancelled`/
silent success"): the `CancelComPrimitive`/`CoptCancel` half is exactly what this fix closes.
The suspension-termination path that entry also named
(`J2534Service::terminate_tp20_broadcast_periodic_for_suspension`) turns out not to share this
residual at all on inspection -- that helper has no gRPC return path (`-> ()`, fire-and-forget:
it only `warn!`s and restores/leak-tracks on a native failure, never surfaces a client-visible
error), so the original entry's framing of one shared UX wrinkle across both paths was itself
imprecise. Deleted outright rather than reworded, per this file's own Prioritized Backlog
convention, since no separate genuine residual survives it.

**Codex review round 15 (P2, PR #101): `terminate_tp20_broadcast_periodic_for_suspension` gets
the identical `ERR_INVALID_MSG_ID` fix round 14 gave `CoptCancel` -- described in full in
ADR-192 Decision item 2's own round-15 update paragraph rather than restated here.** The same
race applies to this function's own native stop: a `CLEAR_PERIODIC_MSGS` that wins `self.api`'s
lock after this function's caller already took the tracking entry off the link, but before this
function's own `stop_periodic_message` call runs, has already cleared the message device-side,
surfaced here too as `ERR_INVALID_MSG_ID`. Before this fix, this function treated every
`stop_periodic_message` error identically (restore-or-leak-track via the round-11 shared
helper), which for this authoritative-already-gone code wrongly perpetuated tracking for a
message that no longer exists, leaving the COP reported `Executing` and blocking later periodic
starts until an explicit cancellation cleaned it up -- round 14's note above concluded this
function had no shared residual with `CoptCancel` because it has no gRPC return path to
surface a bare error on, but that framing missed this stuck-tracking consequence, which does not
depend on a gRPC error path at all. Fixed the same way as round 14: on `ERR_INVALID_MSG_ID`,
skip the restore/leak-track path entirely and fall through to this function's own ordinary
success finalization (`PduCopstFinished`, via `events::emit_terminal_if_live`) instead -- the
fourth call site of this exact pattern in this crate, alongside round 14's `CoptCancel` fix and
`retry_leaked_periodic_message_stops`/`retry_leaked_repeat_message_stops`.

**Tests:** `rpc_misc.rs::terminate_tp20_broadcast_periodic_for_suspension_tests::
err_invalid_msg_id_finalizes_cop_instead_of_restoring_or_leak_tracking` -- seeds a live,
`connect_generation`-matching, connected link with a committed `tp20_broadcast_periodic` entry,
forces the mock's `PassThruStopPeriodicMsg` to fail with `ERR_INVALID_MSG_ID` via
`__mock_set_stop_periodic_message_error`, calls
`terminate_tp20_broadcast_periodic_for_suspension` directly, and asserts the COP is removed from
`primitives`, a single `PduCopstFinished` terminal status lands in the CLL's `rx_buf` queue,
nothing is restored onto `link.tp20_broadcast_periodic`, and nothing is leak-tracked into the
shared channel's `leaked_periodic_message_ids`.

**Codex review round 15, Fix 2 (P1, PR #101, `design-advisor` consult): `self.api` becomes the
serialization fence for an in-flight broadcast-periodic start — see ADR-193, which partially
supersedes ADR-192 Decision item 2's in-flight-reservation mechanism.** The
`None`-sentinel reservation `reserve_tp20_broadcast_periodic` writes is written under
`logical_links` alone and released again; `rpc_start_com_primitive` only acquires `self.api`
afterward, and the native `PassThruStartPeriodicMsg` call happens later still. Every terminator
could take that sentinel — and report the owning COP terminal to the client — holding nothing
but `logical_links`, with no synchronization at all against the in-flight native call. Since
clause 19.3.2.3's five-frame burst is emitted synchronously inside that native call, frames went
out AFTER a `CancelComPrimitive` had been answered `PduCopstCancelled` or after a
`PDU_IOCTL_SUSPEND_TX_QUEUE` had reported success, and the after-the-fact orphan stop cannot
retract frames already on the wire.

*Mechanism.* (1) The start bracket re-runs `reserve_tp20_broadcast_periodic`'s ownership/session
predicates a second time — link still present, `connect_generation`/`connected` unchanged, this
`cop_handle`'s own `None`-sentinel still in the slot, `!tx_suspended()` (not an exact mirror of
that function's list: revalidation omits its `channel_id.is_some()` arm, benignly, since
`connect_generation` + `connected` cover that transitively) — from inside the
already-held `api` guard and before the temp-param apply, via the new
`revalidate_tp20_broadcast_periodic_reservation` (nested `logical_links` read, the
ADR-110-sanctioned `api`-outer order this same function already uses). A reservation that is no
longer this cop_handle's own covers three shapes — the CLL vanished from `logical_links`
entirely (a completed `DestroyComLogicalLink`), a DIFFERENT cop_handle now owns the slot, or
this cop_handle's own entry was genuinely taken — and in every one of them, some other path
already owns this COP. Not necessarily one that has REPORTED anything yet, since
a suspension terminator takes the entry under `logical_links` alone and can still be queued
behind `self.api` itself — so that path now owns this cop_handle's finalization outright. The
start therefore returns the RPC successful without ever calling the native start and without
touching anything: no `logical_links` cleanup exists to do (the entry is gone by definition), and
`self.primitives` is deliberately left alone for the terminator, mirroring
`finalize_broadcast_periodic_start_bookkeeping`'s `Resolution::NotOwned` arm one step later in
the same bracket. It DOES perform ADR-067 claim D's `temp_param_update` Working-writeback before
returning, via the shared `apply_temp_param_working_writeback` helper this exit and the RPC's own
`Ok` tail both call: claim D's criterion is that the RPC accepted the COP, not that hardware
consumed the staged snapshot (`CoptStopcomm` writes back while touching no hardware at all), and
this early return never reaches the tail. Skipping it — which the first cut of this fix did,
reasoning that the exit precedes the hardware apply — left the client's staged Working values in
place after a call it was told succeeded, so a later `CoptUpdateparam` would have promoted values
that different race timing would have discarded (second `edge-case-hunter` pass, same round).
Calling `rollback_tp20_broadcast_periodic_reservation` on this path — which the first cut of this
fix did — is what an `edge-case-hunter` pass then caught and reproduced: that helper also removes
the `primitives` entry, every terminator's finalization is gated on that entry still being
present (`events::emit_terminal_if_live`, `CoptCancel`'s own `prims.remove(..).is_some()`), so
the COP vanished with no terminal status and no `terminal_cops` record, and a later
`GetStatus`/`CancelComPrimitive` answered `PDU_ERR_INVALID_HANDLE` instead of ADR-128's
already-terminal no-op. Any other broken predicate leaves the entry still nominally this
cop_handle's own, so that path keeps the cleanup duty, rolls back both halves via that same
helper, and is a genuine rejection carrying the `Status` the initial reservation attempt would
have returned. (2) `finalize_or_orphan_broadcast_periodic_start` was split into
`finalize_or_orphan_broadcast_periodic_start_locked` (the `logical_links` resolution plus the
`NotOwned` orphan stop, both now running under the caller's still-held `api` guard, immediately
after the native start returns) and `finalize_broadcast_periodic_start_bookkeeping` (the
`primitives`/`dispatched` update and the status emissions, which need no fence and still run
after `drop(api)`). The `Resolution` enum moved to module scope as
`BroadcastPeriodicStartResolution`; the `periodic_clear_epoch`/`pending_clear_generation`
resolution rule is unchanged. (3) Three call sites changed: `CoptCancel` now acquires `self.api`
first and takes the entry through the new shared
`J2534Service::take_broadcast_periodic_under_api_locked` (`rpc_misc.rs`, next to
`restore_or_leak_track_broadcast_periodic`), issuing its native stop under that same guard and
dropping it before the restore-or-leak-track/status work (which acquires `shared_channels`, and
must never do so while holding `api`); `terminate_tp20_broadcast_periodic_for_suspension`
acquires `self.api` UNCONDITIONALLY, sentinel case included, purely as the fence — its four
callers (`ioctl_suspend_tx_queue` plus three `CP_SuspendQueueOnError` sites in `events.rs`) still
take the entry under `logical_links` alone and are deliberately unchanged, because that unfenced
take is load-bearing rather than a gap: it only removes from `logical_links` and never touches
`primitives`; it is exactly what makes a racing start's revalidation detect "already resolved"
and abort before transmitting (taking it in the suspension flag's own critical section is what
makes "suspended" and "nothing tracked" one atomic fact); and the thing that genuinely needs the
fence is the DECISION taken on the result — what to report, whether a native stop is owed, how
`primitives` is finalized — which happens inside this function, under its own `self.api`
acquisition, with the racing start leaving `primitives` untouched for it; and
`DisconnectComLogicalLink` keeps taking the entry in its big early `logical_links` critical
section, atomically with its own `channel_id`/`channel_key` clear, and fences only the
native-stop/leak-track decision inside the `api` bracket it already acquires for filter/repeat
teardown, using those same pre-clear captures (see the correction note below — this is the shape
the fix was corrected BACK to, after briefly routing Disconnect through the shared helper).
The helper is therefore id-targeted only, with `CoptCancel` its sole caller. Its three outcomes are
exactly the two possible fence orderings plus "already resolved": still-`None`-sentinel (the
terminator won; the start will find its reservation gone and never transmit), now-`Some(id)` (the
start won and committed; stop it for real), or absent/foreign (no-op).

*Two sites need no change.* `DestroyComLogicalLink` removes the CLL from `logical_links` outright
before it acquires `api`, so an in-flight start is on one of two equally safe paths: it has not
yet revalidated, in which case revalidation resolves `AlreadyResolved` on the "CLL vanished
entirely" shape and aborts before ever issuing the native start; or it is already inside its own
`api` bracket, whose in-bracket resolution hits `NotOwned` and stops the just-started message
under that guard — which Destroy's own later `api` acquisition necessarily waits behind. Removing
the whole map entry at once also means Destroy can never publish the partial CLL state the
correction note below describes for Disconnect. `CLEAR_PERIODIC_MSGS` already defers rather than takes (the sentinel deferral fix
above), a different and already-correct mechanism.

*Correction, second `edge-case-hunter` pass (same round).* The first cut of this fix moved
`DisconnectComLogicalLink`'s `link.tp20_broadcast_periodic.take()` out of its early
`logical_links` critical section — the one that also clears `channel_id`, takes `channel_key`,
and sets `connected = false` — and down into the fenced section via the shared helper. That
reintroduced this ADR's own bug class through a different door: between the two points the CLL
was observable, under `logical_links` alone, carrying a live entry alongside an already-`None`
`channel_id`/`channel_key`. `ioctl_suspend_tx_queue`, the three `CP_SuspendQueueOnError` sites,
and `CoptCancel`'s session capture all read the entry with only that lock and capture
`channel_id` alongside it, so any of them would have taken a committed `message_id` with
`channel_id: None`, skipped `PassThruStopPeriodicMsg` for want of a channel, and reported the COP
terminal — leaving a still-transmitting native periodic with zero tracking anywhere, neither on
the link nor in `SharedChannel::leaked_periodic_message_ids`. The take was restored to the early
critical section (atomic with the session clear), leaving only the native-stop/leak-track
DECISION inside the `api` bracket, exactly the shape the suspension-path callers already had.
`take_broadcast_periodic_under_api_locked`'s now-dead `owner: Option<u32>` parameter was
simplified to a required `cop_handle: u32` at the same time. ADR-193's Decision item 3 reasons
(a)/(b)/(c) for why an early unfenced take is safe apply to EVERY terminator, not just the ones
they were originally written for.

*Closed (Codex review round 16, P1, PR #101, ADR-193 amendment):* a round-16 Codex review flagged
that `events.rs::handle_channel_hard_error`'s dead-channel sweep was still the one call site
ADR-193 had left as an accepted, unfenced residual — the exact TOCTOU shape ADR-193 closes
everywhere else, just against the sweep's own multi-CLL `cancel_link_cops` loop instead of a
single terminator. `handle_channel_hard_error`'s dead-channel sweep no longer silently drops a
`None`-sentinel taken off a swept CLL. The take (under `logical_links` alone, same as before) now
records the sentinel case via a per-sweep `in_flight_start_taken` flag instead of relying on a
let-chain to discard it; after `links` releases but while `shared_channels` (`chans`) is still
held (per ADR-139), one batched `self.api.lock().await`-then-drop — sanctioned by ADR-080 as
`api` nesting under an already-held `shared_channels`, with `logical_links` not held at that
point — fences every `None`-sentinel taken during that sweep pass against a racing in-flight
start, before the sweep's `cancel_link_cops` loop can report any of that pass's CLLs terminal.
`was_primary`'s gate over the take (both arms) is safe unchanged: it is vacuously true whenever
`tp20_broadcast_periodic` is `Some` in the first place, since TP2.0 links never gain a UUDT
companion channel. See ADR-193's own "Amendment" paragraph in its Consequences section for the
full mechanism and rationale, and `events_hard_error_broadcast_periodic_tests.rs` for
direct-unit coverage, including a fence-blocking test. This closes the residual the previous
version of this paragraph, and a now-deleted Prioritized Backlog entry, used to describe as
accepted and deliberately not fixed.

*Closed (Codex review round 17, P1, PR #101, ADR-193 amendment):* a round-17 Codex review found a
FIFTH unfenced terminator — `rpc_module.rs::rpc_module_disconnect` (`ModuleDisconnect`'s ISO
22900-2 §9.3.3 force-cleanup) — and it was worse than the four fixed before it: it did nothing
with `tp20_broadcast_periodic` at all. It set `connected = false` and swept every owned COP
terminal via `cancel_link_cops` for every CLL, then `links.clear()` wiped every
`LogicalLinkState` — `tp20_broadcast_periodic` included — with no fence and no leak-tracking
whatsoever. Fixed inside this function's existing `shared_channels`-held window (the same block
already spanning the `cancel_link_cops` loop, per ADR-139 — no second lock window added): the
per-CLL loop that sets `connected = false` now also takes `tp20_broadcast_periodic` and matches
on `message_id`, recording a `None`-sentinel via a per-call `in_flight_start_taken` flag (declared
before the `shared_channels`/`logical_links` acquisitions, mirroring the round-16 flag's own
declaration site) the identical way the round-16 sweep does. Immediately after that loop but
before the `cancel_link_cops` loop, the identical batched `if in_flight_start_taken {
drop(self.api.lock().await); }` fence closes the in-flight-start race. Unlike the four prior
fixes, a committed `Some(message_id)` entry is deliberately NOT leak-tracked onto
`SharedChannel::leaked_periodic_message_ids` here. This does not rest on `api.disconnect`/
`api.close` running unconditionally — they run only on the branch where a device was actually
open (`slot.take()` returning `Some`); the reachability argument instead is that `self.device_id`
is cleared only by this function's own `slot.take()` and by `spawn_shutdown_task` (`service.rs`),
and `connect_new_physical_channel` (`rpc_link.rs`) can only add a `shared_channels` entry while
holding that same `self.device_id` guard, so a `None` outcome implies `shared_channels` was
already empty. On the branch that does run them, `api.disconnect` (per channel) then `api.close`
(for the whole device) run a few lines later, and SAE J2534-1 §7.2.4 (`PassThruDisconnect`) and
§7.2.2 (`PassThruClose`) each independently document that tearing down a channel/device clears its
resident periodic messages device-side (also restated together in §7.2.7) — combined with the
entire `shared_channels` map already being `mem::take`n and dropped before any leak-tracked entry
could ever be retried, leak-tracking here would be a dead store regardless. This mirrors this same
function's own pre-existing (previously undocumented) precedent for `repeat_message_ids`:
`links.clear()` wipes every CLL's `repeat_message_ids` too, with no per-message
`api.stop_repeat_message` attempt, for the identical reason. `rpc_module.rs`'s own `Some(_)` arm
now carries a `debug_assert!` at the `slot.take()` site pinning the `slot == None ⇒
shared_channels empty` invariant this rests on. `rpc_destroy_com_logical_link`'s own correctness
was re-verified (not changed): its `channel_id_for_tp.is_some()` gate now carries a comment
pinning the invariant it already relied on — `channel_id_for_tp: None` implies
`tp20_broadcast_periodic` was already `None`, or was taken atomically alongside `channel_id` by
`DisconnectComLogicalLink`. This
amendment's own audit — every already-fixed path, this function's one `links.clear()` site, every
other `tp20_broadcast_periodic.take()` call site in the crate, and confirmation that
`CoptStopcomm` never touches the broadcast COP at all — found no sixth unfenced terminator. See
ADR-193's own round-17 amendment paragraph in its Consequences section for the full mechanism and
`rpc_module.rs::tests` for direct-unit coverage, including a fence-blocking test using the
round-16 fence test's own technique.

*Refined (Codex review round 18, P1, PR #101, ADR-193 amendment):* round 17's "deliberately not
leak-tracked, the imminent `api.disconnect`/`api.close` clears it device-side regardless" reasoning
above did not account for those two native calls themselves failing — a round-18 Codex review
found that a genuine double failure of both left a committed periodic transmitting device-side with
no tracking anywhere and no way to ever retry stopping it, since neither call's result was checked
before this function proceeded. A `design-advisor` consult confirmed leak-tracking the failure was
a dead end specifically here, not a general objection: by the time such a failure could be observed,
`slot.take()` has already unconditionally cleared `self.device_id` (ADR-107 addendum (h)), and the
opportunistic leaked-stop retry mechanisms (`retry_leaked_periodic_message_stops`/
`retry_leaked_repeat_message_stops`, `rpc_misc.rs`) only ever probe a live `SharedChannel` on an
open device, so nothing could ever reach the old device again to retry against regardless of what
got tracked. The validated fix instead adds an explicit best-effort native stop of its own: while
iterating `links.values_mut()` in the same loop the round-17 fix already added, each link now also
resolves its owning `SharedChannel` via `link.channel_key.and_then(|k| chans.get(&k))` (the
already-held `chans` guard) and skips collecting anything for that link if the resolved channel is
already `dead` (mirroring `handle_channel_hard_error`'s own precedent of never attempting a native
call against an already-dead channel); otherwise a committed `Some(message_id)` periodic is
collected into a `(channel_id, message_id)` pair. Later, inside the `if let Some((_, id)) =
slot.take() { ... }` block, under the SAME `self.api` guard already acquired for the
disconnect/close sequence and strictly BEFORE the pre-existing `for (_, sc) in channels {
api.disconnect(sc.channel_id); }` loop, `api.stop_periodic_message(channel_id, message_id)` is
issued for every collected pair; a failure is `warn!`-logged and otherwise ignored — not
propagated, not leak-tracked, matching how `DisconnectComLogicalLink`/`DestroyComLogicalLink`/
`handle_channel_hard_error` already treat a failed stop with nowhere further to escalate to. The
broader `api.disconnect`/`api.close` sequence still runs immediately afterward, unconditionally, as
a backstop for whatever the explicit stop missed, was skipped for (a `dead` channel), or itself
failed to clear — round 17's SAE J2534-1 §7.2.4/§7.2.2 reasoning is now a backstop argument, not the
sole one. **Symmetric fix for `repeat_message_ids`** (design-advisor's own recommendation, since
round 17's text above cited `repeat_message_ids`' lack of a per-message stop attempt as its own
precedent for periodic's then-no-stop behavior): the identical `live_channel_id` gate now also
collects every entry in `repeat_message_ids` into its own `(channel_id, msg_id)` pairs, resolved at
the same later call site via `api.stop_repeat_message(channel_id, msg_id)`, same best-effort/
log-on-failure/`dead`-skip treatment — closing that now-asymmetric-if-left-alone precedent instead
of leaving it stale. The round-17 `debug_assert!` pinning "`slot == None` ⇒ `shared_channels` was
already empty" is extended with a second `debug_assert!` pinning the direct corollary: if
`shared_channels` was empty, no link could have resolved a live `SharedChannel` above either, so
both collected `Vec`s must be empty too. If the explicit stop AND the later `api.disconnect` AND
`api.close` all fail for the same message, it may still keep transmitting device-side with no
tracking anywhere — an accepted residual, not a new or worse gap specific to periodic/repeat
messages: every other resource class on an abandoned device is equally unreachable the moment
`self.device_id` clears, per ADR-107 addendum (h)'s existing deliberate device-abandonment design;
periodic/repeat messages now merely share that same already-accepted cost instead of being a
silent, undocumented exception to it. This refinement adds no new fenced terminator and does not
reopen the round-17 "no sixth unfenced terminator remains" audit — it changes only what the
already-fenced decision inside this function's existing `self.api` bracket does, not when or under
what lock that bracket is acquired. See ADR-193's own round-18 amendment paragraph in its
Consequences section for the full mechanism, and `rpc_module.rs::tests` for direct-unit coverage of:
the explicit stop firing for a committed periodic on a live channel, the overall RPC still
succeeding when that stop fails, the stop being skipped for an already-`dead` channel, and the
symmetric `repeat_message_ids` failure-tolerance behavior.

**Tests** (`rpc_primitive.rs::tp20_broadcast_periodic_api_fence_tests`, 18 tests): the
revalidation's pass/already-resolved (sentinel taken, foreign cop_handle now in the slot, CLL
vanished)/rejected (`tx_suspended()` became true, plain disconnect) outcomes, including that the
already-resolved outcome leaves this COP's `primitives` entry for the terminator that took the
reservation, and that the rollback the *other* failure paths still use stays a no-op on another
COP's fresh reservation; the shared take helper's three branches (still-owned sentinel — also
draining a stale `cancelled_cops` mark — committed id, foreign/absent no-op); the `CoptCancel`
call site end-to-end through the real RPC for the sentinel branch (no native
`PassThruStopPeriodicMsg`, entry taken, `PduCopstCancelled` queued immediately); and a
direct-state assertion that the real `message_id` is committed BEFORE the `api` guard is
released, so the next terminator to acquire that guard sees a stoppable id rather than a sentinel
it would have had to skip.

Six of those tests exercise the production call sites rather than the helpers, by having the
test task itself hold `self.api` (standing in for an in-flight start bracket) while the path
under test runs as a spawned task — deterministic on the `current_thread` test runtime, since
every other await on these paths is an uncontended `Mutex::lock` that never yields, so a spawned
task that is still unfinished after repeated `yield_now`s can only be parked on the contended
fence. `a_start_that_loses_the_fence_leaves_primitives_for_the_terminator_to_finalize` reproduces
the `edge-case-hunter` finding end to end: the real `rpc_start_com_primitive` parks on the fence,
an `ioctl_suspend_tx_queue`-shaped take removes the entry under `logical_links` alone, the start
wins the fence, revalidates to already-resolved and returns its silent `Ok`, and the terminator
is then still able to finalize the COP and emit `PduCopstFinished` (this test fails against the
pre-fix unconditional rollback).
`a_suspension_termination_waits_for_the_fence_before_reporting_the_cop_terminal` and
`cancel_waits_for_the_fence_before_taking_the_sentinel` are fence-acquisition isolation tests:
each fails if the `self.api` acquisition at its own call site
(`terminate_tp20_broadcast_periodic_for_suspension`'s unconditional one,
`rpc_cancel_com_primitive`'s pre-take one) is removed — verified by temporarily reverting each,
which the earlier take/emit-only assertions could not detect since the shared helper's guard
parameter is unused. `the_real_start_bracket_holds_the_fence_across_the_native_start` drives the
real start RPC against a genuinely opened mock channel and pins that no native
`PassThruStartPeriodicMsg` runs on the wrong side of the fence.
`disconnect_takes_the_entry_atomically_and_fences_only_the_native_stop` is the Disconnect
counterpart added by the second `edge-case-hunter` pass: it drives the real
`rpc_disconnect_com_logical_link` against a genuinely live native periodic message and samples
the CLL under `logical_links` alone throughout the parked window, failing if a tracked entry is
ever observable alongside a cleared `channel_id`/`channel_key` (it fails against the
briefly-shipped late-take shape) while still requiring the native
`PassThruStopPeriodicMsg` to wait for the fence.
`a_start_that_loses_the_fence_still_performs_the_temp_param_writeback` pins the ADR-067 claim D
writeback on the `AlreadyResolved` early `Ok` return (it fails if that call is removed).

*Accepted test-coverage gap (this class of limitation is the same one
`rollback_stop_comm_pending_tests` documents).* The STRICT ordering "the real `message_id` is
committed before `drop(api)`" is asserted only against
`finalize_or_orphan_broadcast_periodic_start_locked` directly, never through the real RPC:
observing it there would require another task to run inside the guard's own window, and this
crate has no test-only hook to pause a native call mid-flight, nor any preemption point in that
window on a `current_thread` runtime (a task queued on the same mutex is woken on release but
not polled until the releasing task yields, which it does not do before committing). Reverting
the real caller to commit after `drop(api)` therefore still passes the suite; the direct test
plus the four production-path tests above are the available substitute.

**Codex review round 18, P2 (PR #101, ADR-192 Decision item 3 amendment): a
sibling-CLL clobber bug in the `temp_param_update=1` per-burst isolation
mechanism the round-8 accepted residual (above, ADR-192 Decision item 3)
pointed to as the fix for exactly this scenario.** Every `temp_param_update`
bracket's revert (`events::revert_hardware_to_live_active_locked`) read THIS
CLL's own `LogicalLinkState::active` snapshot, stripped `PDU_PC_BUSTYPE`-class
keys (ADR-110), and pushed the remainder to hardware. Because
`CP_TP20BroadcastInterval` is channel-wide/hardware-resident but deliberately
NOT `PDU_PC_BUSTYPE` (ADR-192 Decision item 3), the BUSTYPE strip never
excluded it — and `active` is tracked PER-CLL, not per-channel. Two CLLs
sharing one physical channel with different `CP_TP20BroadcastInterval` values
in their own `active` meant CLL A's own temp-bound bracket reverted to CLL
A's own (possibly stale) `active` copy, clobbering the channel's real,
currently-live value — which could be CLL B's own already-promoted value —
with CLL A's stale one.

Fixed by capturing the channel's actual pre-bracket HARDWARE value
(`GET_CONFIG`, native units) for every channel-wide, non-`PDU_PC_BUSTYPE` key
at apply time, while `api` is already held continuously for the whole
apply → ... → revert bracket (the same continuous-guard fences Fix 5/round 8
and the round-17 `handle_send_recv` fix already established), and restoring
that captured value on revert instead of this CLL's own `active`. A new
`comparam_support::CHANNEL_WIDE_UNUM32` const (currently only
`CP_TP20BroadcastInterval`, pinned disjoint from `BUSTYPE_UNUM32` by a
regression-fence test) names the classification; a companion
`strip_captured_channel_wide_keys` mirrors `strip_bustype_keys`'s shape, but
strips a key from the `active` push ONLY when a captured pair for it
actually exists (see the edge-case-hunter correction paragraph below for why
this is conditional, not unconditional);
`events::capture_channel_wide_hardware_locked` performs the capture.
`events::revert_hardware_to_live_active`/`_locked` gained a required
`channel_wide_restore: &[(u32, u32)]` parameter, threaded through from every
bracket's own capture to every one of its own revert call sites:
`handle_send_recv`'s continuous bracket and `rpc_primitive.rs`'s TP2.0
broadcast-periodic-start continuous bracket call
`capture_channel_wide_hardware_locked` directly under their own already-held
`api` guard; `handle_start_comm`'s several split-bracket call sites go
through a new `events::apply_params_to_hardware_capturing` helper instead,
which locks `api` once, captures, then applies, so a caller that cannot hold
`api` continuously all the way to its own later revert still captures at the
correct point in time. The soft-ISO-TP arm of `handle_send_recv` passes
`&[]` (ADR-046 gating means software ISO-TP never coexists with TP2.0 on one
channel).

**Residual not closed by this fix, accepted (reworded -- edge-case-hunter
adversarial review, PR #101, minor Finding 8; see ADR-192's own "Residual
not closed by this fix, accepted" section for the full reasoning, kept in
sync here):** `handle_start_comm`'s split-bracket callers cannot hold `api`
continuously from their own capture/apply all the way to their own
(possibly much later, after further native I/O) revert call sites, unlike
the two continuous-bracket sites above -- narrower than the pre-fix bug
(the captured value is at least the channel's real value as of shortly
before the bracket started, not this CLL's own arbitrarily-stale `active`),
but not airtight against a change to the channel-wide value landing in that
specific gap. Of the two mechanisms an earlier version of this note named
as able to land there: a sibling CLL's own `CoptUpdateparam` on the same
channel cannot actually interleave at all -- `handle_start_comm` and
`handle_update_param` are both dispatched exclusively as `TxItem`s
processed one at a time by that channel's single poll task
(`spawn_channel_poll_task`/`poll_channel_events`, `service.rs`), so a
queued `CoptUpdateparam` is already fully serialized behind
`handle_start_comm`'s entire run. A broadcast-periodic-start via
`rpc_start_com_primitive` genuinely can run concurrently (it bypasses the
poll task, ADR-192 Decision item 2), but its own apply -> native-start ->
revert bracket holds `api` continuously and always captures-then-restores
whatever value is live when it runs, symmetrically with `handle_start_comm`'s
own split bracket -- an interleave between the two is self-correcting, not
value-clobbering. Theoretical, not reachable as a value-corrupting race
given today's single-poll-task-per-channel design.

**Explicitly out of scope (unchanged by this fix):** two CLLs with genuinely
different `active` copies of `CP_TP20BroadcastInterval` and NO
`temp_param_update` bracket involved by either one remains plain
last-writer-wins on the shared physical channel — identical to the round-8
residual above and to every other non-`PDU_PC_BUSTYPE` param under ADR-110.

**Correction (edge-case-hunter adversarial review, PR #101, BLOCKING Finding
1):** the first version of this fix stripped every `CHANNEL_WIDE_UNUM32` key
from the `active` push UNCONDITIONALLY, regardless of whether
`capture_channel_wide_hardware_locked`'s `GET_CONFIG` read for it actually
succeeded. On a capture failure (a real possibility -- J2534-2 Table 77
params are optional, and some adapters accept `SET_CONFIG` but reject the
matching `GET_CONFIG`), that left the key with NEITHER an `active`-derived
restore (stripped from the push) NOR a captured one (capture failed), so the
bracket's own temp-bound value stayed live on hardware forever -- worse than
the pre-round-18 behavior, which at least restored *some* value. Fixed by
making the strip conditional: `comparam_support::strip_channel_wide_keys`
was renamed to `strip_captured_channel_wide_keys` and now only strips a key
when `channel_wide_restore` actually carries a captured pair for it (resolved
per key via `ComParamId::to_j2534_config_id`); a key with no captured pair
now falls back to being restored from `active` alongside every other
non-channel-wide key, exactly like the pre-round-18 behavior, rather than
being left with no restore source at all.

**Tests:** `events::comparam_support::tests::channel_wide_unum32_disjoint_from_bustype_unum32`/
`channel_wide_unum32_contains_only_tp20_broadcast_interval` (regression
fences, mirroring the existing `BUSTYPE_UNUM32` guard tests).
`events::revert_hardware_to_live_active_locked_tests` gained
`pushes_active_with_bustype_stripped_and_channel_wide_key_falls_back_to_active_when_uncaptured`
(renamed from `pushes_active_with_bustype_keys_stripped`, then from the
BLOCKING-Finding-1-era `pushes_active_with_bustype_and_channel_wide_keys_
stripped` -- now asserts a BUSTYPE key is always stripped AND a
channel-wide key with no captured pair falls back to being pushed from
`active`, not dropped), `capture_failure_falls_back_to_restoring_from_
active_instead_of_leaking_temp_value` (new -- the BLOCKING Finding 1
fallback itself: an empty `channel_wide_restore` simulating a capture
failure must restore from `active`, not leave a prior temp-bound hardware
value in place forever), `restores_captured_channel_wide_value_over_stale_
active` (a captured restore pair wins over a differing stale `active`
copy), `restores_channel_wide_value_even_when_cll_absent` (a captured
restore pair is still applied even with no CLL entry at all), and
`sibling_cll_channel_wide_clobber_is_avoided_by_the_captured_restore` — the
actual two-CLL scenario: CLL B's own value is already live on hardware, CLL
A's own bracket captures/applies/reverts, and hardware ends up back on CLL
B's value, never CLL A's stale `active` copy. `events::
handle_send_recv_api_fence_tests` (round 17's own test module) updated to
seed a distinct pre-bracket hardware baseline via `connect_mock_tp20_channel`
so the revert's actual restore target (the captured hardware baseline, not
`active`) is observably distinguished from both `active` and the
temp-applied Working value.
`tests/grpc_mock/tp20.rs` gained
`broadcast_periodic_temp_param_update_reverts_to_sibling_captured_value_not_stale_active`
(edge-case-hunter minor Finding 4) -- the same sibling-clobber scenario
proven end-to-end through the real gRPC/native-mock path, since the
pre-existing `broadcast_periodic_temp_param_update_applies_and_reverts_
broadcast_interval` test's captured value and CLL A's own Active happen to
numerically coincide (both 20) and so cannot itself discriminate the two
mechanisms.

**Codex review round 19, P2 (PR #101):** `ioctl_start_repeat_message`'s
existing rejection guard for a staged `CP_TP20BroadcastAddress` (round 12,
above) consulted the normalized `ComParamSet::tp20_broadcast_address()`
accessor -- whose own doc comment already stated the RPC layer is
responsible for rejecting an out-of-range nonzero value *before* this
accessor is ever consulted (ADR-192 Decision item 1), an obligation the COP
send paths fulfill via `validate_tp20_broadcast_address_range`
(`rpc_primitive.rs`) but Repeat Messaging never routed through at all. A
client could stage an out-of-range nonzero value (e.g. `0x01`) via plain
`SetComParam`/`CoptUpdateparam`, entirely independent of any COP send, and
the accessor would silently fold it to `None`, letting it slip past this
guard.

**Correction (edge-case-hunter adversarial review, PR #101):** the paragraph
above originally claimed this let the request reach `build_tx_message`'s
broadcast-framing composition with ordinary connection-bound TxFlags -- a
malformed-frame outcome. That is not what happens: `build_tx_header`'s TP2.0
arm (`tx_header.rs`) consults the SAME normalized `tp20_broadcast_address()`
accessor to decide whether to compose `[address]++payload` framing, so an
out-of-range value folds to `None` there too and never reaches broadcast
composition at all -- it falls through to the ordinary established-connection
branch instead. The real, narrower pre-fix defect: staging a garbage value
with no TP2.0 connection established produced the wrong error code
(`InvalidArgument` from ordinary connection/message-composition validation,
not the intended broadcast-incompatibility `FailedPrecondition`); staging it
with a connection already established would have silently started Repeat
Messaging with ordinary, well-formed (non-broadcast) framing while ignoring
the garbage `CP_TP20BroadcastAddress`, rather than rejecting outright. The
fix itself needed no code change from this correction -- rejecting any
nonzero staged value remains the right, conservative behavior -- only this
narrative's stated justification was overstated. Fixed by checking the raw staged
`PARAM_TP20_BROADCAST_ADDRESS` value directly instead of the normalized
accessor, rejecting any nonzero value regardless of range: unlike the COP
send paths (where an in-range value is a legitimate broadcast address),
Repeat Messaging has no supported broadcast use for a staged address at
all, so no range check is needed here, only a zero/nonzero check. New test
`tests/grpc_mock/tp20.rs::start_repeat_message_is_rejected_while_an_out_of_
range_broadcast_address_is_staged` stages `0x01` and confirms the same
`FailedPrecondition` rejection the existing in-range-address sibling test
already covers.

## SAE J2534-2 GM UART Protocol (ADR-189, Phase 8)

Brings SAE J2534-2 clause 11 (GM UART Protocol, SAE J2740) into scope. Full
design and rejected alternatives in
[ADR-189](../../../docs/adr/ADR-189-j2534-2-gm-uart-phase8.md); this section is
the implementation-level summary. Clause 11 defines a master/slave UART bus:
the tester must be granted bus mastership (a poll-message/poll-response
handshake, or an immediate grant on vehicles with no bus master) before
diagnostic communication can begin — but, per the clause's own text, that
negotiation *logic* (when to listen, what poll type was seen, when to
relinquish, retry/backoff) belongs to the client reprogramming application,
not this service. The device/driver's job is limited to two new thin,
synchronous `PassThruIoctl` sub-functions, so — unlike SAE J1939 (ADR-179)
or TP2.0 (ADR-188), each of which needed a new `events_*`-style
service-owned async state machine — this phase adds no new indication
routing, no new per-CLL asynchronous state, and no new concurrency hazard
class.

**New `ChannelProtocol` and resource-table row.** `protocol.rs` gains
`ChannelProtocol::GM_UART_PS` (`0x00008009`, matching Phase 0's
already-committed `PROTOCOL_GM_UART_PS` header constant — note this value
sits numerically *before* `UART_ECHO_BYTE_PS`'s `0x0000800A`, since SAE
J2534-2 numbers clause 11 before clause 12; unrelated to this service's own
phase-implementation order, which shipped GM UART last), self-mapping
through `j2534_protocol_id()` the same way every other standalone protocol
does. `resources.rs` gains exactly ONE new resource-table row (`0x0260`,
next in the `0x0200` namespace after TP2.0's `0x025F`) — mirroring UART
Echo Byte's/Honda DIAG-H's own single-row shape, since clause 11 defines no
ISO 22900-2 preset either. New `BUSTYPE_GM_UART` (`0x0311`) and
`PINS_GM_UART` (single pin, J1962 pin 9 — clause 11.2.2's own "primary"
pin, an editorial convenience default the same category as Honda DIAG-H's
own pin-14 default) with `hw_protocol_override: None`.

**Pin selection: the same closed-two-single-pin shape Honda DIAG-H already
established.** Clause 11.2.2 names exactly two valid single pins — 9
(primary) and 1 (secondary) — with no spec-mandated default. `names.rs`'s
new `resolve_pin_selection` arm mirrors Honda DIAG-H's own arm exactly: a
resource-table row access with no explicit `dlc_pin_data` defaults to pin 9
(packed `0x0000_0900`); a raw-protocol-id access bypassing the resource
table with no explicit pin is rejected; a second (secondary) pin is
rejected outright (single-wire, no secondary role); any pin other than
`{1, 9}` is rejected regardless of access path. Unlike Honda DIAG-H's own
arm, this one is deliberately narrower than
`resources::is_gm_uart_protocol_id` (which also recognizes the `_CH1..128`
range, needed for the two new IOCTLs' own protocol gate below) — a
directly-named `_CHx` id must fall through this arm untouched and reach
`resolve_channel_selection`'s own `already_chx` route instead, exactly like
a directly-named `CAN_CHx` id already does, since clause 7 pin selection
has no meaning for an Additional Channel.

**Round 3 corrections (Codex review, PR #98).** (a) `row_needs_dynamic_pin_selection`
— the shared predicate `find_table_row_by_name` and
`parse_protocol_id_from_resource` both consult to decide whether a matched
row routes through `resolve_pin_selection` at all — never registered
`resources::is_gm_uart_protocol_id`, so a name-based connect
(`protocol_name: "GM_UART"`) skipped Pin Selection entirely: no
`SET_CONFIG(CONFIG_J1962_PINS)` at all with implicit pins, and an explicit,
spec-valid pin 1 choice rejected during fixed-row narrowing (the row lists
only its own pin-9 default). Fixed by adding the predicate; regression
tests `connecting_via_canonical_name_without_pins_applies_the_default_pin`/
`connecting_via_canonical_name_with_pin_1_succeeds`. (b) That fix exposed a
second gap unique to GM UART: the compound `_CHx`-suffixed name grammar
(e.g. `"GM_UART_CH1"`) resolves to the SAME row/protocol via
`resolve_protocol_name_with_chx_suffix`, reaching this same arm with
`requested_index` set instead of empty `dlc_pin_data` — the arm computed a
mandatory default pin selection regardless, which `resolve_channel_selection`'s
clause-6/clause-7 mutual-exclusion check then rejected as an invalid
combination, even though `_CHx` channels have no J1962 pin concept. Fixed
by checking `requested_index` first: `Some` with empty `dlc_pin_data`
bypasses pin selection (`Ok(None)`, mirroring how a directly-named `_CHx`
id already bypasses this whole function); `Some` combined with non-empty
`dlc_pin_data` is rejected outright, since `resolve_channel_selection`'s
own mutual-exclusion check can't independently catch that combination here
(`resources::is_ps_protocol_id` was never extended to the standalone-row
families, GM UART included) — an edge-case-hunter verification pass on the
first version of this bypass found it silently discarded the caller's pins
instead of rejecting the double-qualification; regression tests
`connecting_via_compound_chx_name_succeeds` (integration) and
`resolve_pin_selection_rejects_gm_uart_chx_combined_with_explicit_pins`
(unit).

**The first standalone (non-CAN-family) protocol to combine a single
resource-table row with `_CHx` Additional Channels support.**
`GM_UART_CH1..CH128` (already-minted Phase 0 constants) participate in the
existing Phase 2b arithmetic `_CHx` mapping — `resources::chx_block_base`/
`chx_base_protocol_id` each gain one new entry, keyed by `GM_UART_PS`
itself (not a native base id, since clause 11 defines no unqualified base
id at all — mirroring `chx_block_base`'s existing SCI-consolidation
precedent's own "key by the id actually available" shape). This mechanically
extends the existing eight-vs-seven-in-scope-family bookkeeping throughout
`resources.rs`/`names.rs` (doc comments, the `BLOCKS`/`CHX_BLOCK_BASE_IDS`
arrays, `is_chx_protocol_id`'s own "eight in-scope, ten out-of-scope"
region-membership doc) — no new mechanism. `resources::is_gm_uart_protocol_id`
mirrors `is_j1939_protocol_id`'s own range-inclusive shape (not
`is_j1708_protocol_id`'s/`is_tp2_0_protocol_id`'s single-value shape),
since GM UART, unlike those two, has in-scope `_CHx` support.

**Two new `ChannelID`-scoped (L) D-PDU IOCTL commands**, the 26th and 27th,
minted the next `PDU_IOCTL_BASE` offsets (`service_params.rs`) the same way
Phase 12/14 minted theirs:

- `PDU_IOCTL_SET_POLL_RESPONSE` (`PDU_IOCTL_BASE + 0x1A`) forwards to a new
  `j2534-0404::J2534Api0404::set_poll_response(channel_id, bytes)`, a thin
  `PassThruIoctl(SET_POLL_RESPONSE)` wrapper built on the crate's existing
  `ioctl_sbyte_array_input` helper (the same one `protect_j1939_addr`/
  `tp20_request_connection` already use). Input is Table 28's
  `PollResponseMsg[100]` (≤100 bytes) via `bytearray_data`; no output; no
  client-side length pre-validation (a thin passthrough, the native call is
  the authoritative check). Round 5 correction (Codex review, PR #98):
  `j2534-0404-mock`'s own `IOCTL_SET_POLL_RESPONSE` handler used to accept
  and copy a payload of any size, so every `grpc_mock` test using it
  observed success for an oversized payload a real adapter should reject
  -- no test in this codebase could ever catch a service-side regression
  that started forwarding an oversized `bytearray_data` unchecked. Fixed
  by rejecting `NumOfBytes > 100` with `ERR_INVALID_IOCTL_VALUE` in the
  mock before copying, standing in for a conforming adapter's own
  rejection; regression test
  `set_poll_response_rejects_a_payload_over_100_bytes`.
- `PDU_IOCTL_BECOME_MASTER` (`PDU_IOCTL_BASE + 0x1B`) forwards to a new
  `become_master(channel_id, poll_id)` wrapper, same underlying helper.
  Input is Table 32's single `Poll_ID` byte via `unum32_value` (0..=255,
  rejected client-side above that range to avoid a silent truncation to
  `u8`) — not `bytearray_data`, since the proto's `DataItem` oneof has no
  dedicated single-byte variant; no output. This native call's own ~2s
  worst-case blocking wait (clause 11.3.3.2) needs no service-side
  timeout/cancellation logic, but does need its own dispatch treatment
  (Codex review finding, PR #98, round 1): unlike every other native call
  this service issues, all near-instant and so far always awaited directly
  under `self.api`'s ordinary async `Mutex` guard, `become_master`'s own
  call runs inside `tokio::task::spawn_blocking`, acquiring `self.api` via
  `Mutex::blocking_lock` from within that closure instead — a multi-second
  wait held under the async guard would otherwise tie up a tokio runtime
  worker thread for the whole duration. `shared_channels` is also released
  before this call, not held across it, unlike the original round-1
  implementation (it was only needed to resolve `channel_id`). `self.api`'s
  own pre-existing global mutex still serializes this call against every
  other native call for its own ~2s duration, exactly as it already does
  for every native call regardless of duration -- an accepted residual
  (ADR-189 Consequences), not something this fix changes. `ERR_FAILED`
  (the documented "no poll message within 2s" outcome) is left to
  `map_native_error_for_link`'s existing generic mapping — no
  protocol-specific error-code handling.

Both IOCTLs are rejected (`PDU_ERR_ID_NOT_SUPPORTED`) on a non-GM-UART
link, and are gated on `SharedChannel::ref_count == 1` via a shared
`require_gm_uart_link` helper in `rpc_misc.rs` — the same
shared-physical-channel precaution `ioctl_sw_can_mode` already
established, applied here because a poll-response definition or a bus-
mastership bid affects the whole physical GM UART bus, so a per-CLL call
would otherwise silently affect a sibling CLL sharing the same channel. A
channel that fails this gate is REJECTED
(`PDU_ERR_RSC_LOCKED_BY_OTHER_CLL`/`ResourceExhausted`, via the new shared
`error::gm_uart_shared_channel_locked_status` helper), not silently
accepted as a no-op success — round 2 correction (Codex review, PR #98):
the original round-1 shape returned `Ok(())` for a shared channel, which
lied to the caller about having attempted the IOCTL at all.

**`BECOME_MASTER`-in-flight sibling-join race (round 2, Codex P2 finding,
PR #98, design-advisor consult).** The `ref_count == 1` gate above is
checked, `shared_channels` is released, and only then does the ~2s
blocking native call run inside `spawn_blocking` — leaving a window where
a sibling CLL's `ConnectComLogicalLink` could join the SAME physical
channel (bumping `ref_count` under its own, separate `shared_channels`
acquisition in `rpc_link.rs`, which never touches `self.api`) while the
mastership bid was still outstanding, defeating the sole-owner gate's own
purpose. Closed by a new `SharedChannel::become_master_in_flight:
Arc<AtomicBool>` field: set under the SAME `shared_channels` critical
section as the `ref_count == 1` gate itself (armed atomically with it, so
no sibling join — which needs this same lock — can land in between), and
cleared by an RAII guard (`rpc_misc.rs`'s private
`BecomeMasterInFlightGuard`) constructed as the FIRST statement inside the
`spawn_blocking` closure — deliberately not in the surrounding async fn —
so the clear survives every exit path: normal return, a panic/unwind
inside the native call, and a tonic RPC cancellation of the awaiting async
future (which does not stop a blocking closure `spawn_blocking` has
already started running on its own worker thread). Both
existing-channel-join branches now reject a join while the flag is set,
the same shape as the pre-existing `dead`-channel rejection:
`rpc_connect_com_logical_link` (`rpc_link.rs`) gets a new check;
`ensure_uudt_companion_channel` is confirmed unreachable for a GM_UART_PS
channel and was deliberately left unmodified — its own `channel_key` is
always `(j2534_0404::CAN, baud_rate, 0, 0)`, so it can never match a
GM-UART-keyed `SharedChannel` entry (a GM UART `SharedChannel`'s own key
uses `PROTOCOL_GM_UART_PS`/`_CHx`, never `CAN`). `PDU_IOCTL_SET_POLL_RESPONSE`
gets no equivalent reservation flag -- an accepted residual, not a verified
guarantee: clause 11.3.3.1 describes it as a simple define-the-response-
bytes call with no wait documented anywhere in its own text, unlike
`BECOME_MASTER`'s explicit ~2s bound (clause 11.3.3.2), but the spec's
silence on timing is an assumption this phase makes, not a spec-guaranteed
bound -- a real adapter whose own `SET_POLL_RESPONSE` implementation is
slow would reopen a narrower version of the window `BECOME_MASTER`'s own
fix just closed. Regression coverage in
`tests/grpc_mock/gm_uart.rs` uses a new mock backdoor
(`__mock_arm_become_master_hold`/`__mock_become_master_hold_engaged`/
`__mock_release_become_master_hold`, `j2534-0404-mock`'s `BecomeMasterHold`)
that makes `IOCTL_BECOME_MASTER` block until released, letting the test
observe a sibling connect rejected while the call is genuinely in flight,
then observe it clear (a subsequent connect succeeds normally) once
released. See ADR-189 Consequences for the accepted residual this flag
does NOT close: mastership *tenure* after a successful `BECOME_MASTER`
call returns has no service-observable end signal (clause 11 places
relinquish/renegotiation in the client application), so a sibling joining
immediately after is normal, expected behavior.

**Round 4: stale-CLL-attachment race (Codex finding, PR #98).** Both IOCTL
handlers used to resolve the requesting CLL's `channel_key` via
`require_gm_uart_link` under `logical_links` alone, release that lock, and
only afterward separately acquire `shared_channels` for the `ref_count`/
`become_master_in_flight` gate. In the gap between those two acquisitions,
`DisconnectComLogicalLink` (which acquires `shared_channels` first per
ADR-080's outermost-lock invariant, then `logical_links` nested inside it,
clearing the disconnecting CLL's own `channel_key` to `None` under that
held guard) could run to completion for the exact CLL making the IOCTL
call — if a sibling CLL remained attached, the now-stale `channel_key`
snapshot still resolved to a `SharedChannel` entry whose `ref_count` had
just dropped to 1, so the handler wrongly believed it observed sole
ownership on behalf of the (now-disconnected) requesting CLL and fired the
native call on what was actually the sibling's own exclusively-owned
channel. Fixed (design-advisor consult, this being the third
concurrency-adjacent finding in this same mechanism across PR #98's review
rounds) by giving `require_gm_uart_link` a proof parameter — an immutable
reference to the caller's already-held `shared_channels` `MutexGuard` —
that forces `channel_key` resolution to happen inside the SAME critical
section the `ref_count`/`become_master_in_flight` gate uses, eliminating
the snapshot-then-relock shape entirely rather than adding a
revalidate-and-compare step after the fact (a fresh read under the held
lock is always current, since every production write to
`LogicalLinkState::channel_key` — connect finalization, disconnect,
destroy — itself happens under a held `shared_channels` guard). The
pre-existing `ioctl_sw_can_mode` (ADR-164, unrelated to GM UART) shared the
identical snapshot-then-relock shape and the identical latent gap;
design-advisor recommended fixing it in this same PR rather than leaving a
known concurrency bug next to its own newly-documented fix pattern, and it
received the identical fix (hoisting its own `shared_channels` acquisition
ahead of the `logical_links` snapshot) without going through
`require_gm_uart_link`, since its own gate predicate and no-op/reject
semantics differ. New accepted residual: once `become_master_in_flight` is
armed and `shared_channels` is dropped for the `spawn_blocking` call, the
*requesting* CLL's own disconnect is not blocked by the flag and can race
its own in-flight bid — bounded by the pre-existing global `self.api`
mutex (held by the `spawn_blocking` closure), which still serializes
disconnect's own native teardown behind `become_master`'s return, so no
interleaved hardware calls occur; the bid simply completes on a channel
that closes immediately after.

**Discovery-cache wiring.** Stage 1's `DeviceFlag` mechanism (ADR-185) gets
one new `connect_discovery_check` entry, keyed on
`DEVICE_INFO_GM_UART_SUPPORTED` — unlike every other Stage-1 family's own
single-`_PS`-id entry, this one also recognizes the `GM_UART_CHx` range
(via `is_gm_uart_protocol_id`), since GM UART is the first Stage-1 family
with in-scope `_CHx` support. `GM_UART_SIMULTANEOUS`'s companion bit and
`GM_UART_PS_J1962`'s per-pin validity query are deliberately left unwired
this phase (see Prioritized Backlog) — the same residual every other
Stage-1 family already carries. The `_CHx` capacity precheck
(`resources::chx_device_info_supported_parameter`/`check_chx_capacity`) is
deliberately NOT extended to GM UART this phase — ADR-189 decided only the
flat `DeviceFlag` connect-path gate, not the numeric capacity-packed one;
a GM UART `_CHx` connect is therefore never capacity-prechecked, the same
treatment SAE J1939's own out-of-scope `_CHx` range already gets.

**ComParam defaults: clause 11 defines no ComParam concept at all.** Unlike
every other standalone protocol's own Win32 API section (each of which
defines at least a fixed or configurable baud rate, ADR-170/174/188),
clause 11.3 defines no baud rate, loopback, or timing parameter whatsoever.
`comparam_defaults.rs`'s new `gm_uart_uart()` bustype default therefore
seeds `DATA_RATE` at its unset/unstaged default of `0` — the same "no
spec-mandated default exists, a caller must `SetComParam` it before
connecting" shape `analog_in()`'s own `PARAM_ANALOG_SAMPLE_RATE` entry
already documents — rather than inventing a numeric value with no textual
basis. `comparam_support.rs` gets no new `GM_UART_PS`-specific allowlist
branch either: `GM_UART_PS` falls through `is_param_allowed`'s generic
"Unknown protocol — allow" tail, so every ComParam (not just `DATA_RATE`)
stays `SetComParam`/`GetComParam`-reachable on a GM UART link. Whether
clause 11 actually warrants a narrower, dedicated allowlist (the way every
other standalone protocol has one) is an open question this phase does not
settle — noted in the Prioritized Backlog, since ADR-189 explicitly
scoped the "remaining design questions" it resolved to protocol identity,
pin selection, and the two new IOCTLs' own dispatch, not ComParam scope.
`resources::bustype_default_name_for_hw_protocol_id` (the general
raw-id-direct-connect bustype-defaulting fallback every other standalone
`_PS`-only family uses) gains an `is_gm_uart_protocol_id` branch resolving
to `"GM_UART_UART"` — round 3 correction (Codex review, PR #98): the initial
implementation left this fallback without a GM UART entry, so a
`CreateComLogicalLink` naming the raw `GM_UART_PS`/`GM_UART_CHx` id directly
(bypassing the resource table, `resource_row: None`) did not seed
`gm_uart_uart()`'s own Working ComParamSet the way the resource-id connect
route does for free — if `RscData.bus_type_name` named an unrelated bus
(e.g. a CAN bustype), that bus's own baud rate and CAN-specific
`SET_CONFIG` parameters would have been forwarded to the GM UART channel
instead.

## SAE J2534-2 Ethernet_NDIS (ADR-194, Phase 16)

Brings SAE J2534-2 clause 24 (Ethernet_NDIS) into scope. Full design and
rejected alternatives in
[ADR-194](../../../docs/adr/ADR-194-j2534-2-ethernet-ndis-phase16.md); this
section is the implementation-level summary. Clause 24 binds a J2534
channel to an NDIS/RNDIS Ethernet adapter — but the actual Ethernet
payload traffic is routed by the spec itself entirely outside the J2534
API, onto the OS network stack, a diagnostic application's own concern
this pass-thru-DLL-caller service never touches. From this service's
vantage point clause 24 is therefore one of the *simplest* J2534-2
protocols: connect, disconnect, one info IOCTL, everything else
hard-errored — a strictly smaller job than Analog Inputs (ADR-177),
already shipped in-crate with the same "most-message-API-disallowed"
shape. The plan's own up-front "architecturally divergent" `design-advisor`
flag (`docs/j2534-2-support-plan.md` §6/§8) inverted on investigation for
exactly this reason, and the DoIP-overlap check the plan's dependency note
also flagged came back negative (no DoIP implementation, branch, or plan
exists anywhere in this repository) — both resolved directly in ADR-194
rather than needing a separate scoping decision.

**New `ChannelProtocol` and resource-table row.** `protocol.rs` gains
`ChannelProtocol::ETHERNET_NDIS` (`0x00008013`, matching Phase 0's
already-committed `PROTOCOL_ETHERNET_NDIS` header constant), self-mapping
through `j2534_protocol_id()` the same way every other standalone protocol
does. `resources.rs` gains exactly ONE new resource-table row (`0x0261`,
next in the `0x0200` namespace after GM UART's `0x0260`) — the same
single-row shape UART Echo Byte/Honda DIAG-H/GM UART use, since clause 24
defines no ISO 22900-2 preset either. New `BUSTYPE_IEEE_802_3` (`0x0312`)
— the first standalone-protocol bus type in this table with a real ISO
22900-2:2022 Table B.2 anchor (`IEEE_802_3`, DoIP's own physical-layer/
BUSTYPE short name, verified directly against
`iso22900-2-2022/ISO_22900-2_2022(en).md`'s Table B.2 "ISO UDS on DoIP"
row) rather than a project-chosen name like every prior standalone
protocol's bus type. `PINS_ETHERNET_NDIS` lists clause 24.2.4 Table 103's
Option 1 pin set (pins 3/8/11/12/13 — the first-listed option, mirroring
ADR-164's/ADR-168's own "first-listed as practical default" precedent),
reusing this table's existing `PIN_PLUS`/`PIN_MINUS` (Tx pair),
`PIN_HI`/`PIN_LOW` (Rx pair), and `PIN_K` (Activation Line) type constants
since clause 24 is the first row needing two independent differential
pairs plus a lone control pin in one row — none of the three reused type
families is a genuine textual match, the same "explicitly-flagged,
non-spec-grounded typing choice" shape `PINS_SAE_J1708` already
established. This row's `dlc_pins` is purely descriptive: unlike every
`_PS`/`_CHx` row, it is never resolved into a native
`SET_CONFIG(CONFIG_J1962_PINS)` call — clause 24 defines no `_PS`/`_CHx`
pin-selection mechanics at all, so the row is deliberately NOT registered
in `names.rs`'s `row_needs_dynamic_pin_selection`/`resolve_pin_selection`
family list; pin usage is chosen by connect flag instead (next
paragraph).

**Connect-time pin option: a new project-minted ComParam, not
`J1962_PINS`.** `service_params.rs` gains `PARAM_NDIS_PIN_OPTION`
(`0x80D2`, the next-free service-level ComParam id after Stage 7c's
`CP_TP20BroadcastInterval` block), backing `CP_NdisPinOption` (UNUM32; `0`
= auto/default, `1` = Option 1, `2` = Option 2) — staged via `SetComParam`,
resolved at `ConnectComLogicalLink` time by a new `rpc_link.rs`
`ndis_pin_option_connect_flags` arm in the shared `connect_flags` function
into `CONNECT_FLAG_NDIS_PINS_OPTION1`/`OPTION2` (never both — the native
table treats both bits set as equivalent to neither, so auto encodes
neither bit). Unlike `CP_AnalogSampleRate`'s required, connect-failing-
if-unset design (clause 10.3.3.2.2's own zero-disables-the-subsystem
semantics), `0` here is spec-functional, so this ComParam is genuinely
optional — no connect-time validation beyond the ordinary
`comparam_support::is_param_allowed` allowlist, which accepts exactly this
one param for `ETHERNET_NDIS` (checked before `is_universal_param`, the
same "narrower-than-universal closed list" shape Honda DIAG-H's/Analog
Inputs' own allowlists use — clause 24 defines no baud rate, loopback, or
any other native ComParam-shaped concept) and explicitly excludes it from
the generic "Unknown protocol — allow" fallback (the same
`PARAM_ANALOG_SAMPLE_RATE`/`PARAM_TP20_*` exclusion mechanism), so it can
never leak through as settable-but-inert on an unrelated protocol.
`comparam_defaults.rs`'s new `ieee_802_3()` bustype default seeds
`PARAM_NDIS_PIN_OPTION` at its own spec-functional `0`.

**Native `ERR_NO_CONNECTION_ESTABLISHED` gets its own distinct D-PDU
error.** `error.rs`'s `pdu_error_for` gains a
`ERR_NO_CONNECTION_ESTABLISHED -> PduError::PduErrNoCableDetected` arm —
the closest ISO 22900-2 analog for "the physical connection to the
vehicle could not be established" (an activation-line failure), rather
than the generic `PDU_ERR_FCT_FAILED` catch-all every other unclassified
connect failure falls through to, mirroring the pre-existing
`ERR_CHANNEL_IN_USE -> PDU_ERR_RESOURCE_BUSY` precedent (Phase 13) for a
native connect-time failure meaning one specific thing.

**COP gate: broader than Analog Inputs, no receive-only exemption, and
checked earlier.** `rpc_start_com_primitive` (`rpc_primitive.rs`) gains an
unconditional `link.hw_protocol_id == PROTOCOL_ETHERNET_NDIS` rejection
(`PDU_ERR_ID_NOT_SUPPORTED`) for every `cop_type` — clause 24 bars reads
too (clause 24.2.5.2), unlike Analog Inputs, whose whole point is reading,
so this gate has no `transmits`-gated exemption for a receive-only
(`NumSendCycles == 0`) `CoptSendrecv` the way the Analog Inputs gate does.
Placed immediately after `cop_type` validation, BEFORE the pre-existing
`comm_started` pre-flight state checks (`CoptStartcomm` while already
started / `CoptStopcomm` while not started) — found while writing this
phase's own integration test: placing it alongside the narrower Analog
Inputs gate (which runs later, after `transmits` is classified) let
`CoptStopcomm` on a never-started Ethernet_NDIS CLL hit the earlier
`comm_started` check first and report `PDU_ERR_CLL_NOT_STARTED`/
`FailedPrecondition` instead of the intended `PDU_ERR_ID_NOT_SUPPORTED`/
`InvalidArgument` — this protocol has no COP-level API surface at all, so
its rejection must be unconditional on `comm_started` state too, not just
on `cop_type`.

**Poll task: uniform spawn, `rx_supported`-gated RX pass only.** Per
ADR-194's explicit rejection of skipping the poll-task spawn entirely,
the poll task is still spawned uniformly from `rpc_link.rs`'s connect
path, preserving the shared `SharedChannel` cancel/teardown/`GetStatus`
lifecycle every other protocol relies on. A new `ChannelPollCtx::
rx_supported: bool` field (`events.rs`), computed at
`spawn_new_shared_channel` call sites from the connecting protocol
(`false` only for `PROTOCOL_ETHERNET_NDIS`; always `true` for the ADR-046
UUDT-companion path, which is always CAN-family), gates ONLY
`poll_rx_inner`'s shared RX pass: when `false`, the function returns
`PollOutcome::Drained` (stamping `drain_watermarks`, same as an ordinary
empty batch) immediately, BEFORE issuing the native `PassThruReadMsgs`
call at all — not merely discarding its result afterward. Without this,
clause 24.2.5.2's expected, permanent `ERR_NOT_SUPPORTED` from every poll
tick's read would trip this task's pre-existing hard-read-error-closes-
the-channel rule and tear the link down moments after connect. No other
per-tick poll-task duty needs gating: nothing can enqueue a `TxItem` for
this protocol once the COP gate above is in place, and no ComParam
allowlist entry exists for tester-present/timing parameters on it either
(verified against the actual allowlist above, not just asserted).

**Connect-time internal pass-all-filter installation is also skipped for
this protocol.** Found while writing this phase's own integration test,
not anticipated by the ADR text: `connect_new_physical_channel`
(`rpc_link.rs`) unconditionally installs a pass-all `PASS_FILTER` after
`PassThruConnect` for every non-ISO15765, non-Analog-Input, non-TP2.0
channel so the adapter delivers received frames at all — Ethernet_NDIS
has no filter concept either (clause 24.2.5.5:
`PassThruStartMsgFilter` always returns `ERR_NOT_SUPPORTED`), so a
conforming adapter/mock genuinely rejects this internal step, failing the
whole connect with `ERR_NOT_SUPPORTED` before ADR-194's own design was
even reachable. Fixed by adding `base_proto_id !=
PROTOCOL_ETHERNET_NDIS` to the same exclusion gate Analog Inputs'/TP2.0's
own entries already use — moot in practice once `rx_supported` gates the
poll task's own RX pass regardless, but the connect-time install attempt
itself still needs the exclusion to avoid failing synchronously against a
spec-conforming adapter.

**One new `ChannelID`-scoped (L) D-PDU IOCTL command**, the 28th, minted
the next `PDU_IOCTL_BASE` offset (`service_params.rs`) the same way GM
UART's own two commands minted theirs: `PDU_IOCTL_GET_NDIS_ADAPTER_INFO`
(`PDU_IOCTL_BASE + 0x1C`) forwards to a new
`j2534-0404::J2534Api0404::get_ndis_adapter_info(channel_id)`, a thin,
no-input `PassThruIoctl(GET_NDIS_ADAPTER_INFO)` wrapper following the
established `SW_CAN_HS`/`BECOME_MASTER`/`*_REPEAT_MESSAGE`
thin-forwarder shape — rejected `PDU_ERR_ID_NOT_SUPPORTED` on a
non-`ETHERNET_NDIS` link, and `PDU_ERR_CLL_NOT_CONNECTED` on an
unconnected one (unlike the fire-and-forget IOCTLs above, this one
returns real data, so there is no sensible "no channel, no-op" response
to fabricate — clause 24's own IOCTL definition requires a live
`ChannelID` too). `j2534-0404`'s new `NdisAdapterInfo` wrapper
(`message.rs`) mirrors `PassThruMessage`'s owned-wrapper-over-a-raw-
`#[repr(C)]`-struct shape; `rpc_misc.rs`'s new `pack_ndis_adapter_info`
hand-packs its fields into `DataItem.bytearray_data` in the native
struct's own field order/widths exactly (`AdapterUniqueID`/`AdapterName`
as raw byte spans, `Status`/`EthernetPinConfig` as little-endian `u32`s
per ADR-178's convention, `MAC_Address`/`IPV6_Address`/`IPV4_Address`
passed through byte-for-byte since the spec already stores them in
network order) — 226 bytes total; byte layout documented in
`docs/rpc-api-guide.md`. `j2534-0404-mock` serves a fixed, deterministic
canned struct (`mock_ndis_adapter_info`, arbitrary-but-plausible test
fixture values, no spec basis) and gains a dedicated connect-failure
injection backdoor, `__mock_set_ndis_connect_error`, scoped to
`PROTOCOL_ETHERNET_NDIS` connects only (unlike `__mock_set_fast_init_
error`'s blanket "every channel" shape) — mirroring `j1850_bus_flavor`'s
own protocol-scoped connect-time behavior. One field is not fully
canned: `EthernetPinConfig` is derived from the connected channel's own
stored `flags` (Codex review, PR #102 round 9) — `2` only when
`CONNECT_FLAG_NDIS_PINS_OPTION2` was set at `PassThruConnect` time AND
`CONNECT_FLAG_NDIS_PINS_OPTION1` was not, `1` otherwise — rather than
always echoing the fixture's Option 1 value regardless of which pin
option the connection actually resolved. The `1` case covers Option 1,
auto/unset, and both bits set together: `rpc_link.rs`'s own
`ndis_pin_option_connect_flags` never emits both bits for a real gRPC
client, but a direct FFI caller of the mock can, and this ADR documents
the native table treating both bits set as equivalent to neither — the
mock now mirrors that convention exactly instead of reporting `2` for
that case (`edge-case-hunter` finding, PR #102 close-out). New mock
unit tests
`get_ndis_adapter_info_reports_option_2_when_connected_with_option_2_flag`
and `get_ndis_adapter_info_reports_option_1_when_both_option_flags_are_set`,
plus service-level end-to-end test
`get_ndis_adapter_info_reports_option_2_when_connected_with_option_2`
(`tests/grpc_mock/ethernet_ndis.rs`), pin this, complementing the
pre-existing auto/Option-1 case
`get_ndis_adapter_info_decodes_the_canned_struct` already covered.

**Discovery-cache wiring.** Stage 1's `DeviceFlag` mechanism (ADR-185)
gets one new `connect_discovery_check` entry keyed on
`DEVICE_INFO_ETHERNET_NDIS_SUPPORTED` — the ninth Stage-1-wired family
(the original six -- SWCAN, FT-CAN, UART Echo Byte, Honda DIAG-H, J1708,
Analog Inputs -- plus TP2.0 and GM UART, each added in a later phase),
the same flat single-`_PS`-id shape every prior Stage-1 family except GM
UART uses. `j2534-0404-mock` advertises it supported by default, with a
dedicated `__mock_set_ndis_supported` test toggle mirroring
`__mock_set_gm_uart_supported`'s own shape (unlike every other Stage-1
family before GM UART, which have no analogous per-family toggle).

**`resources::bustype_default_name_for_hw_protocol_id` gains an
Ethernet_NDIS arm** (`PROTOCOL_ETHERNET_NDIS -> "IEEE_802_3"`) — the same
raw-id-direct-connect bustype-defaulting fallback every other standalone
`_PS`-only family needs (GM UART's own Phase 8 round-3 correction found
this gap for that family; added correctly for Ethernet_NDIS from the
start this phase, with its own `bustype_default_name_for_hw_protocol_id_
covers_every_in_scope_family` test case) so a `CreateComLogicalLink`
naming the raw `PROTOCOL_ETHERNET_NDIS` id directly (bypassing the
resource table) still seeds `ieee_802_3()`'s own Working ComParamSet
rather than an unrelated bus's defaults.

**Deferred / accepted residuals.** No `_SIMULTANEOUS` companion bit is
wired (matching every Stage-1 family's own residual — clause 24 defines
no such concept in the first place, since only one adapter binding is
ever meaningful). ISO 22900-2:2022's `PDU_IOCTL_SET_ETH_SWITCH_STATE`
(independent activation-line control) has no clause-24 substrate to back
it and stays out of scope — clause 24 ties activation to the connect/
disconnect lifetime, not an independent toggle (see the reworded
"2022-edition delta audit" P3 entry above, which this phase partially
addresses for `GET_ETH_PIN_OPTION` specifically, not `SET_ETH_SWITCH_
STATE`).

**Test coverage.** `tests/grpc_mock/ethernet_ndis.rs` covers: connecting
with each of the three `CP_NdisPinOption` values and confirming the
recorded `PassThruConnect` flags; the injected `ERR_NO_CONNECTION_
ESTABLISHED` connect failure mapping to `PDU_ERR_NO_CABLE_DETECTED`
(and confirming the injection is scoped to `ETHERNET_NDIS` only); every
COP type rejected `PDU_ERR_ID_NOT_SUPPORTED`, including the receive-only
`CoptSendrecv` case Analog Inputs would exempt; the poll task surviving
several `POLL_INTERVAL_MS` ticks past connect (proven indirectly — an
unrelated second CAN connect still succeeds afterward, which would fail
if the module had wrongly gone `PduModstNotAvail` from a misfired hard
read error); `PDU_IOCTL_GET_NDIS_ADAPTER_INFO` decoding the canned struct
correctly from `bytearray_data`, and being rejected `PDU_ERR_ID_NOT_
SUPPORTED`/`PDU_ERR_CLL_NOT_CONNECTED` on a non-`ETHERNET_NDIS`/not-yet-
connected link respectively; the ADR-185 Stage 1 Discovery-gated connect
rejection when the mock's `DEVICE_INFO_ETHERNET_NDIS_SUPPORTED` toggle is
off; the ADR-194 Shared-channel join guard rejecting a second CLL that
stages a different resolved `CP_NdisPinOption` than the physical channel's
creator (`PDU_ERR_FCT_FAILED`), and admitting one that resolves to the
same flags; `SetComParam` rejecting an out-of-range `CP_NdisPinOption`
value; and `PDU_IOCTL_START_REPEAT_MESSAGE` rejected `PDU_ERR_ID_NOT_
SUPPORTED` on a connected link (Codex review, PR #102 — Repeat Messaging
is a separate device-autonomous TRANSMIT path the COP gate alone does not
close). Every test in this file connects through a module opted into SAE
J2534-2 (`start_j2534_2_server`, mirroring `gm_uart.rs`'s/`tp20.rs`'s
identical helper) — the file originally used the default, non-opted-in
`TestServer::start()` throughout, which masked a real gap
`names.rs::resolve_pin_selection` had no dedicated `ETHERNET_NDIS` arm
for (Codex review, PR #102): a non-opted-in module's
`PassThruConnect(PROTOCOL_ETHERNET_NDIS)` fell through to the generic
tail's unconditional `Ok(None)` with no opt-in check at all, since the
Discovery capability check is also a no-op for a non-opted-in module.
Fixed with a dedicated arm mirroring the Analog Inputs/GM UART/every
other standalone-protocol opt-in gate; unit-tested directly in
`names.rs`'s own `resolve_pin_selection` test module (rejecting a
non-opted-in module, accepting an opted-in one with no `dlc_pin_data`).
That arm was reachable only via `parse_protocol_id_from_resource`'s
pin-selection branch until an `edge-case-hunter` finding (PR #102 round
2) caught its table-row-matched branch (`RscData::ProtocolName`/a bare
`ResourceName`) skipping it entirely, since Ethernet_NDIS is excluded
from `row_needs_dynamic_pin_selection`; fixed with an explicit exception
routing it through the pin-selection branch on that path too, unit-tested
directly in `names.rs`'s own `parse_protocol_id_from_resource` test
module (rejecting a non-opted-in module and rejecting non-empty
`dlc_pin_data`, both via `ProtocolName`; accepting an opted-in module with
no `dlc_pin_data`). `j2534-0404-service/src/service/rpc_misc.rs`'s own
`pack_ndis_adapter_info_tests` module unit-tests the hand-packed struct's
serialization/deserialization round-trip directly (a populated struct and
an all-zero one). `resources.rs`'s own `rows_conflict` tests cover
`GetConflictingResources` correctly reporting a conflict with `GM_UART`
(0x0260) over Option 2's alternate Tx pin 9 — invisible to a plain
`dlc_pins` overlap, since the row's typed pins cover only Option 1 (Codex
review, PR #102) — and correctly reporting no conflict against a fully
pin-disjoint row. A further test covers a peer row's own dynamically
selectable pins, not just its static default (Codex review, PR #102 round
7): `HONDA_DIAGH` (0x023B) defaults to pin 14 but also accepts pin 1
(clause 13.2.4), which now correctly conflicts with Ethernet_NDIS's Option
2 alternate Tx(+); its own static `dlc_pins` don't overlap at all. A
round-8 finding raised the mirrored question for `ISO_11898_RAW_FTCAN`
(0x0230, defaults to pins 1/9, clause 20.2.1 also accepts pins 3/11) —
investigation (`edge-case-hunter`, PR #102 close-out) found this pair was
already correctly reported conflicting *before* any round-7/8 change, since
FT-CAN's own default pins 1/9 are identical to Ethernet_NDIS's Option 2
alternate set; the peer-dynamic-pin mechanism above was never needed for
it. `rows_conflict_is_true_for_ethernet_ndis_against_ftcan_via_default_pin_overlap`
pins this real, already-correct behavior via its actual mechanism
(the ordinary `dlc_pins`-vs-alternate-set overlap), not the peer-dynamic-pin
path a since-reverted round-8 change mistakenly believed was needed.
`tests/grpc_mock/resources.rs`'s own
`filters_by_ethernet_ndis_option_2_alternate_pin` test covers the sibling
gap in resource lookup (Codex review, PR #102): `GetResourceIds` filtered
by Option 2's alternate Tx pins (1 = PLUS, 9 = MINUS) correctly resolves
to the Ethernet_NDIS row, and a mismatched type at the same pin number
(1 typed HI) correctly does not. Its own
`filters_reject_mixed_ethernet_ndis_pin_options` test (Codex review, PR
#102 round 6) covers `retain_rows_matching_all_pins`'s atomic whole-set
evaluation: an impossible mix of Option 2's Tx(+) (pin 1) with Option 1's
Tx(-) (pin 11), or two conflicting PLUS pins from different options,
correctly resolve to no match, while the fully-consistent Option 1 pin
set (3/11 -- shared verbatim with `SAE_J1708`'s own identical wiring,
`PINS_SAE_J1708`) and Option 2 pin set (1/9) each still correctly match.
`j2534-0404-mock`'s own lib tests cover: connect
succeeding with no pin-assignment gating and every message-API call
rejected `ERR_NOT_SUPPORTED`; the connect-error injection's `ETHERNET_
NDIS`-only scoping; `IOCTL_GET_NDIS_ADAPTER_INFO` returning the canned
struct and rejecting a non-`ETHERNET_NDIS` channel; `IOCTL_START_REPEAT_
MESSAGE` rejecting an `ETHERNET_NDIS` channel, mirroring its existing
Analog Inputs rejection; and the `DEVICE_INFO_ETHERNET_NDIS_SUPPORTED`
toggle.

## SAE J2534-2 Discovery-Cache Connect-Time/IOCTL-Time Enforcement (ADR-185)

**Closes the long-standing "added-tested-documented-but-uncalled" gap** the
Phase 1/ADR-153 Discovery mechanism left open (previously the consolidated
P2 Prioritized Backlog entry accumulated across every J2534-2 phase since --
now closed by this phase and removed from the backlog). ADR-156
Decision 4/Phase 2b's `check_chx_capacity` was the only production caller of
the cache-hit/native-call machinery before this phase; this phase
generalizes it into a shared primitive with a second caller class.

**`discovery.rs` gains two new types and one new method.** `DiscoveryCheck`
(`DeviceFlag`/`DeviceCapacity`/`ProtocolCapacity`) describes what a call
site needs verified; `DeviceAccess` (`AlreadyOpen(DeviceId)`/`OpenIfNeeded`)
encodes which of `discovery_device_info_with_open_device`/
`discovery_device_info` a caller must funnel through, preventing the
non-reentrant `self.device_id` deadlock class in the type system rather than
by caller discipline alone (the same hazard `check_chx_capacity`'s own doc
comment already flagged). `J2534Service::enforce_discovery_capability`
consumes one `DiscoveryCheck` and rejects (via `state_guard_status`, reusing
the native call's own `PduError`) only on a definitive negative -- not
opted into J2534-2, no cached/queryable answer, or any Discovery-query error
itself all fall through as a no-op, exactly matching `check_chx_capacity`'s
own "Discovery refines, native error is the fallback" discipline (ADR-153
Decision 1).

**The Discovery `GET_DEVICE_INFO` cache key widens to `(module_handle,
parameter, input_value)`,** closing the per-pin landmine `discovery.rs`'s
own module doc comment used to flag: `discovery_device_info`/
`discovery_device_info_with_open_device` now thread a real `input_value`
through to the native query instead of hardcoding `0`. Every caller added
before this widening (`check_chx_capacity`, this phase's own six
`DeviceFlag` rows) passes `input_value: 0`, a mechanical,
behavior-preserving change for them; no production caller yet exercises the
per-pin case this closes (Stage 2's own future work).

**Stage 1 wires the CONNECT path only,** for the six J2534-2 protocol
families that had zero Discovery-based fail-fast before this: SWCAN/clause
9, FT-CAN/clause 20, UART Echo Byte/clause 12, Honda DIAG-H/clause 13,
J1708/clause 17, and Analog Inputs/clause 10. `resources::
connect_discovery_check(j2534_proto_id)` maps each family's raw,
POST-SUBSTITUTION hardware protocol id to its `DEVICE_INFO_*_SUPPORTED`
`DeviceFlag` check -- keyed on `j2534_proto_id`, not `base_protocol_id`,
since `base_protocol_id` (immediately above `connect_discovery_check` in
`resources.rs`) already normalizes `PROTOCOL_SW_CAN_PS`/`PROTOCOL_FT_CAN_PS`
(and their ISO15765 counterparts) down to `CAN`/`ISO15765` for every other
Plane B purpose; a base-keyed lookup here would check the wrong
`DEVICE_INFO_*` flag entirely. `rpc_link.rs`'s brand-new-physical-channel
connect path (the same `else` branch `check_chx_capacity`'s own call lives
in) consults it right after the `_CHx` capacity precheck -- an independent,
additional check, not a replacement: different resource-capability
question (does the device support this protocol family at all, vs. how many
`_CHx` indices it has for a family it already supports). A joining CLL
(reusing an already-successfully-opened physical channel) skips both checks,
same reasoning `check_chx_capacity` already applies.

**`j2534-0404-mock` gains six new advertised `DEVICE_INFO_*_SUPPORTED`
flags** (`SW_CAN`/`SW_ISO15765`/`FT_CAN`/`FT_ISO15765`/`UART_ECHO_BYTE`/
`HONDA_DIAGH`/`J1708`), matching the protocols it already implements --
without this, every opted-in connect test for these six families would now
be rejected pre-emptively by the very enforcement this phase adds, since the
mock previously left these bits at their default `Supported = 0` (nothing
read them before this phase, so that default was inert). The `_SIMULTANEOUS`
companion bits stay at their default -- no Stage 1 or Stage 2 call site
consults them.

**Stage 2 wires the four IOCTL-shape call sites** `resources::
connect_discovery_check` doesn't cover: `ioctl_read_j1962_pin_voltage`,
`ioctl_get_device_config`/`ioctl_set_device_config`,
`ioctl_set_prog_voltage`'s pin-9 Short-to-Ground case, and
`ioctl_start_repeat_message`, all in `rpc_misc.rs`. Each calls
`enforce_discovery_capability` directly at its own top (after argument
parsing and device acquisition, per Decision 2's rejected-blanket-hook
reasoning), rather than through a `resources.rs` mapping table -- the
IOCTL-shape call sites are few enough, and each needs call-site-local data
(a parsed pin number, a live repeat-message slot count) a table lookup
can't supply. Three of the four (`ioctl_read_j1962_pin_voltage`,
`ioctl_get_device_config`/`ioctl_set_device_config`,
`ioctl_set_prog_voltage`) already hold a `device_guard` and use
`DeviceAccess::AlreadyOpen`. `ioctl_start_repeat_message` originally
acquired no `device_guard` of its own and used `DeviceAccess::OpenIfNeeded`
instead -- but that peek-and-drop deferred the guard's re-acquisition (via
`OpenIfNeeded`) until after this function's pre-existing "Bug 1 fix" had
already locked `shared_channels`, inverting this crate's documented
`device_id`-outermost lock order (`service.rs`'s `require_connected_device_for`
doc comment, ADR-107 addendum) and risking an AB-BA deadlock against
`ConnectComLogicalLink`'s own lock order. Caught by design-advisor review
before merge and fixed in the same PR: `ioctl_start_repeat_message` now
acquires and holds its `device_id` guard up front, before `shared_channels`,
and resolves via `DeviceAccess::AlreadyOpen` instead, giving
`J2534Service::discovery_protocol_info_with_open_device` (mirroring
`discovery_device_info_with_open_device`) its first production caller, via
`enforce_discovery_capability`'s `ProtocolCapacity` arm. `DeviceAccess::
OpenIfNeeded`/`discovery_protocol_info` remain fully implemented and
unit-tested but have no production caller again.

**Second lock-order fix (`design-advisor` consult, same PR):** a second
instance of the same lock-order class affected both
`discovery_protocol_info_with_open_device` (this Stage) and
`discovery_device_info_with_open_device` (Stage 1, already on `main`) --
both read `self.module_state` internally on their native-call error branch,
reachable while a caller already holds `self.shared_channels`
(`ioctl_start_repeat_message`'s Bug-1 region above, and
`rpc_connect_com_logical_link`'s check-create-insert region in
`rpc_link.rs`). Fixed by threading `last_error: Option<TrackedError>` in
from the caller instead of reading `module_state` fresh inside either
helper -- every caller (the `OpenIfNeeded` wrappers, `check_chx_capacity`,
`enforce_discovery_capability`, and `rpc_link.rs`'s two connect-path call
sites, which now share one hoisted `last_error` read) supplies its own
already-read snapshot. Also removes a `self.api` `MutexGuard`-held-during-
`module_state`-lock overlap in `discovery_device_info_with_open_device`
that existed alongside it. See ADR-185's own "Correction (second lock-order
fix, `design-advisor` consult)" note.

`ioctl_get_device_config`/`ioctl_set_device_config` check
`DEVICE_INFO_MAX_NON_VOLATILE_STORAGE` (`DeviceCapacity`, `needed: 1`) --
`j2534-0404-mock` gains a new dispatch arm advertising `Value = 10,
Supported = 1` for this parameter (matching the ten `NON_VOLATILE_STORE_1..
_10` slots it already implements); without this mock fix, Stage 2 would
have pre-emptively rejected every opted-in Device Configuration test, since
the parameter previously fell through to the mock's default
`Supported = 0`. `ioctl_start_repeat_message` checks
`PROTOCOL_INFO_MAX_REPEAT_MESSAGING` (`ProtocolCapacity`, `needed` = this
physical channel's live repeat-message slot count summed across every
sibling CLL sharing `channel_id`, plus 1 for the slot about to start) --
already advertised by the mock's `IOCTL_GET_PROTOCOL_INFO` dispatcher, so no
mock change was needed there; a pre-existing test
(`repeat_message.rs::start_repeat_message_reports_exceeded_limit_through_the_full_grpc_stack`)
now hits this early Discovery rejection instead of the native
`ERR_EXCEEDED_LIMIT` path (`Code::FailedPrecondition` in place of the native
path's `Code::Internal` -- the `PDUError` itself, `PDU_ERR_RESOURCE_ERROR`,
is unchanged, per Decision 5).

**Spec-accuracy correction to Decision 3's table** (found verifying against
`vehicle-comm-specs/j2534-2-0404/J2534-2_202012 - Optional Pass-Thru
Features.md`, Table 111/clause 25.3.2.2, during Stage 2 implementation):
`DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED` is NOT a flat `DeviceFlag`
(`input_value: 0`) as the table originally said -- its `Value` field is a
per-pin bitmap too, invalid to set more than one bit, but it uses the same
LOW-half bit convention as `SHORT_TO_GND_J1962` (bit 0 = pin 1) -- NOT
`PGM_VOLTAGE_J1962`'s HIGH-half convention (bit 16 = pin 1), a different
parameter this Discovery check does not use -- so it is not "defined
identically" to both per-pin parameters in that table row, only to
`SHORT_TO_GND_J1962`'s half. `ioctl_read_j1962_pin_voltage`'s Discovery
check constructs the real per-pin bitmap (`1 << (pin_number - 1)` for
`pin_number` in `1..=16`; an out-of-range `pin_number` skips the check
entirely, falling through to the existing native `ERR_PIN_INVALID` path
unchanged). No mock change was needed for this spec-accuracy correction
itself: at the time it was made, the mock's own
`DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED` dispatch arm still ignored
`p.Value` and unconditionally reported `Supported = 1`. A separate
mock-fidelity fix (same PR, below) closed that gap afterward. See ADR-185's
own "Correction (Stage 2 implementation)" note.

**Mock-fidelity fix: `DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED`'s mock
answer becomes per-pin-aware.** Closes the P3 conformance-audit finding
(this file's "SAE J2534-2 (DEC2020) already-implemented-phases conformance
audit" section, Repeat Messaging area): the dispatch arm used to ignore
`p.Value` and unconditionally report `Supported = 1`, contradicting the
mock's own real `IOCTL_READ_J1962PIN_VOLTAGE` handler, which rejects pins
0/4/5/17+ with `ERR_PIN_INVALID` -- so a discovery-first client saw pin 4/5
reported supported, then had its actual read rejected. The arm now decodes
the same low-half single-pin bitmap `SHORT_TO_GND_J1962`'s own arm already
uses and reports `Supported = 0` for exactly the same pins (0/4/5/17+) the
real IOCTL rejects, `Supported = 1` otherwise, matching it exactly.
`device_info_reports_supported_for_j1962_pin_voltage_read` (`discovery.rs`)
was updated to query a real single-pin bitmap (pin 1) instead of `0` (which
now correctly reports unsupported -- `0` selects no pin, same as pin 0 on
the real IOCTL). New end-to-end coverage:
`read_j1962_pin_voltage_pin_4_is_rejected_by_the_discovery_fail_fast_path`
(`tests/grpc_mock/pdu_ioctl.rs`) confirms pin 4 is now rejected by the
Discovery-cache fail-fast layer specifically (`Code::FailedPrecondition`),
not just the pre-existing native `ERR_PIN_INVALID` path (`Code::Internal`)
-- the `PduError` itself (`PDU_ERR_MUX_RSC_NOT_SUPPORTED`) is unchanged
either way, per Decision 5.

**Deliberately still out of scope** (see the Prioritized Backlog for
the two genuine residuals Stage 2 does not close): each Stage-1-wired
family's `_SIMULTANEOUS` companion bit, and SAE J1708's clause-6
connector-validity queries.

## Change Checklist

1. Run integration tests after JSON-RPC dispatch or startup parsing changes.
2. Validate stop and stdin-close paths terminate active streams and release server resources.
3. After protocol or ComParam mapping changes, update the relevant docs and `names.rs` / `comparam_support.rs` together, then run the `tests/grpc_mock/` suite (one module per theme; shared scaffolding in `harness.rs`).
   - **`comparam_tx.rs`** — its `comparam_tx.rs` module exercises `SetComParam` + `ConnectComLogicalLink` + `StartComPrimitive` through the gRPC surface against `j2534-0404-mock` and asserts the exact `PassThruConnect`/`PassThruIoctl(SET_CONFIG)`/`PassThruWriteMsgs` values the adapter received (protocol id, config values, baud rate, `TxFlags`, and payload bytes), so it catches regressions in per-protocol ComParam forwarding or TX data/flag composition. Its `iso9141_uart_config_decodes_to_data_bits_and_parity`/`iso9141_explicit_parity_overrides_uart_config_derived_parity`/`set_com_param_rejects_unrepresentable_uart_config_and_parity` tests (ADR-071) pin `CP_UartConfig`'s decode into `DATA_BITS`+`PARITY`, the explicit-`CP_Parity`-wins precedence rule, and the accepted-6-value `SetComParam` range check — re-run these alongside `comparam_id.rs`'s own unit tests (`uart_config_full_decode_table`, `uart_config_rejects_unrepresentable_values`, `expand_uart_config_*`) after any change to the `CP_UartConfig`/`CP_Parity` mapping. Its `iso14230_timing_params_convert_from_microseconds_to_native_units`/`sci_timing_params_convert_from_microseconds_to_native_units`/`iso14230_tidle_fans_out_to_w5`/`iso9141_tidle_fans_out_to_w0`/`iso14230_explicit_w5_overrides_tidle_derived_value` tests (ADR-072) pin the K-line/KWP/SCI timing family's microsecond-to-native-step conversion and `CP_TIdle`'s `W0`/`W5` fan-out with explicit-entry-wins precedence — re-run these alongside `comparam_id.rs`'s own unit tests (`us_to_half_ms_*`, `us_to_ms_*`, `to_j2534_config_value_*`, `expand_tidle_*`) after any change to the K-line/SCI timing mapping.
   - **`lifecycle.rs`** — `lifecycle.rs::iso15765_standard_grpc_call_sequence_round_trips_through_mock` additionally drives the full CLL/COP lifecycle end to end (mirroring `live_grpc_flow.rs`'s "Typical gRPC Client Flow", but mock-backed and CI-run) — re-run it after changes to CLL state transitions, `CoptStartcomm`/`CoptSendrecv`/`CoptStopcomm` sequencing, or expected-response matching.
   - **`resources.rs`** (ADR-069) — `resources.rs` (ADR-069) pins the `resources.rs`/`RESOURCE_TABLE` resolution: unfiltered/filtered `GetResourceIds` table-order output, AND-filtering across protocol/bus-type/pin selectors including the legacy-fallback paths, `CreateComLogicalLink` resolving both a table resource ID and a legacy raw/unknown numeric ID, a combined-bus-type resource and the ISO_14229_3/standalone-ISO_15765_3 resources getting non-empty `comparam_defaults.rs` protocol-layer ComParam defaults (not just bus-type defaults), `CreateComLogicalLink` rejecting an ambiguous resource name (`"SAE_J2610_SCI"`, and now `"SAE_J2610_on_SAE_J2610_SCI"` too) while a specific configuration name still resolves, typed-pin `RscData` narrowing an ambiguous name to exactly one row (or `invalid_argument` on zero/no-pins-supplied), `GetResourceStatus`/`GetConflictingResources` resolving a round-tripped table resource ID or a table-only resource name, and `GetResourceStatus`'s response `resource_id` echo (an alias-sharing unique name still echoes its own row, and an ambiguous SCI name echoes the actually-active configuration's ID, not the first in table order) — re-run it after any change to `resources.rs`, `comparam_defaults.rs`'s resource-row entries, or the selector-resolution/echo logic in `names.rs`/`rpc_link.rs`. `resources.rs`'s own `#[cfg(test)]` module additionally runs `every_resource_row_has_protocol_and_bustype_defaults_or_is_allowlisted` over all 37 rows on every `cargo test`/`cargo test -p j2534-0404-service --lib` invocation — no separate re-run instruction needed, but see its doc comment and the allowlist constant's before adding a new row; it also pins the `SAE_J2610_on_SAE_J2610_SCI` four-row expansion (`sae_j2610_on_rows_expand_to_four_configs_with_distinct_hw_overrides`) and the typed-pin `PLUS`/`MINUS` invariant across the three J1850 buses.
   - **`names.rs`**'s test module — `names.rs`'s own test module pins `GetObjectId(OBJT_RESOURCE, ...)`'s table-first resolution (`resolve_object_id_objt_resource_resolves_through_table_first`: table-only name, direct protocol_name/config_name match, legacy-only alias, ambiguous SCI name rejection, unrecognized name) alongside its existing per-`ObjectType` tests — re-run both alongside `resources.rs`'s tests after any change to `find_table_row_by_name`/`find_resource_id_for_protocol`. The same table-first convention for `OBJT_PROTOCOL`/`OBJT_BUSTYPE` (A1-2 fix) is pinned by `resolve_object_id_objt_protocol_resolves_through_table_first`, `resolve_object_id_objt_bustype_resolves_through_table_first`, and `resolve_object_id_objt_bustype_round_trips_into_resolve_resource_ids` — re-run these alongside the `OBJT_RESOURCE` test after any change to `find_protocol_for_name`/`find_bustype_id_by_name`.
   - **`j1850_autodetect.rs`** (ADR-070) — `j1850_autodetect.rs` (ADR-070) pins the `SAE_J1850` bus's VPW/PWM auto-detect probe end to end via `MockBackdoor::set_j1850_bus_flavor`: a VPW-responding bus connects (and transmits) as `J1850VPW`; a PWM-responding bus lands `J1850PWM` and swaps the CLL's Working `DATA_RATE` (and other flavor-dependent ComParams) to the PWM preset; a silent bus still connects, defaulting to VPW; the J2190 resource's detection never transmits (passive-only); a second `SAE_J1850` CLL on the same module skips the re-probe and shares the already-open physical channel (asserted via `connect_count()` staying flat); and `GetResourceIds(bus_type_name=...)` reflects the rename (`"SAE_J1850"` → `0x021A`/`0x021C`/`0x021D`, `"SAE_J1850_VPW"` no longer includes `0x021A`, the old combined name hits the unrecognized-name error path) — re-run it after any change to `probe_sae_j1850_flavor`/`autodetect_sae_j1850_flavor`, the `j1850_bus_flavor` mock knob, or the `0x0307` bus rows. It also pins the hw-flavor-driven checks fixed after a PR review: a PWM-detected link rejects a message within VPW's TX size range but outside PWM's (and the VPW-detected counterpart still accepts it), and `CP_NetworkLine` is allowed only once PWM is actually detected, not merely because the resource's service-level identity permanently reports `J1850VPW` — re-run these after any change to `resolve_send_recv_tx`'s `hw_protocol` or `check_param_allowed`'s callers. It also pins two verification-pass fixes: `inconclusive_fallback_is_not_cached_and_a_later_active_probe_still_runs` (a silent-bus J2190 CLL falls back to VPW without populating the cache, so a subsequent OBD-capable CLL on a now-PWM-answering bus still runs its own probe and detects PWM, and a third CLL then dedups against the now-conclusive cache) and `pwm_win_preserves_client_staged_working_param_but_still_swaps_data_rate`/`vpw_win_leaves_client_staged_working_param_untouched` (a `SetComParam`-staged flavor-independent key, e.g. `CP_CyclicRespTimeout`, survives a PWM win while `DATA_RATE` still swaps to the PWM value) — re-run these after any change to `J1850ProbeOutcome`'s cache-write gating or `sae_j1850_pwm_override_params`'s diff logic. A P1 fix additionally added `pwm_bus_responds_lands_pwm_and_swaps_working_params`'s/`vpw_bus_responds_connects_with_vpw_header_byte`'s outgoing-wire-byte assertions (`server.backdoor.written_data(channel_id, 0)[0]` — `CP_FuncReqFormatPriorityType` is not on the J1850 `GetComParam`/`SetComParam` allowlist, so the wire bytes are the strongest observable check) and `fixed_pwm_resources_send_the_pwm_header_byte_directly` (resources `0x0215`/`0x0217` connect+send the PWM format byte `0x61` directly, no probe involved) — re-run these after any change to the J1850 PWM/VPW protocol presets in `comparam_defaults.rs`.
   - **`cop_ctrl_cycles.rs`** — `cop_ctrl_cycles.rs` pins the PDU_COP_CTRL_DATA cycle semantics (`Time`/`NumSendCycles`/`NumReceiveCycles`, response window from `CP_P2Max` — ADR-053; `NumReceiveCycles == 0` as "no response required," not "wait for one" — ADR-058; `NumSendCycles == 0` as "no send at all, receive-only," not "send once" — ADR-059); re-run it after poll-task dispatch or receive-phase changes. A PR #92 review round added `sendrecv_is_multiple_not_bounded_by_stopcomm_match_reset_ceiling` (ADR-087): pins that `CoptSendrecv`'s `NumReceiveCycles = -2` (IS-MULTIPLE) receive phase stays genuinely unbounded — unaffected by the `match_reset_ceiling_ms` mechanism added to bound `CoptStopcomm`'s non-cancellable IS-MULTIPLE receive — by chattering matching frames well past where that ceiling would have fired and asserting the COP is still not finished; re-run it alongside `stopcomm_data_tx.rs`'s chatty-ECU test after any change to `ExpectedResponseWait.match_reset_ceiling_ms` or either call site that constructs it.
   - **`startcomm_comparam.rs`** — `startcomm_comparam.rs` pins `CoptStartcomm`/`CoptStopcomm`'s call-time ComParam binding and `temp_param_update` handling (ADR-067) — a `CoptUpdateparam` whose `PduCopstFinished` is awaited before a following `CoptStartcomm` call is reflected in it, the temp-Working-then-revert-to-live-Active bracket around the init transaction with the periodic tester-present always resolved from the call-time Active snapshot, the writeback of Working from Active after a successful temp call, the BUSTYPE-key stripping on the temp apply/revert that replaced the ADR-044 lock check (ADR-110), and `CoptStopcomm`'s no-op-with-respect-to-hardware acceptance of the flag (while still performing the writeback) — this remains true for ComParam/`SET_CONFIG` writes specifically; it is unrelated to `stopcomm_data_tx.rs` (ADR-085) below.
   - **`startcomm_optional_message_tx.rs`** (ADR-111, A1-4 conformance-audit fix) — pins the CAN/J1850 `CoptStartcomm` optional-message transmit(+receive) path that reuses `CoptStopcomm`'s one-shot machinery (`StopCommTx` renamed `OneShotCommTx`, ADR-085/ADR-087): `NumReceiveCycles = 0` transmits and reaches `PDU_CLLST_COMM_STARTED` fire-and-forget; `NumReceiveCycles = 1` with a matching mock ECU response delivers `ResultData` attributed to the COP's own `cop_handle` before `COMM_STARTED`; `NumReceiveCycles = 1` with NO matching response emits `PduErrEvtRxTimeout` but still reaches `COMM_STARTED` (`can_optional_message_no_response_times_out_but_still_reaches_comm_started` — the key spec-conformance assertion this whole fix exists for, per ISO 22900-2 §9.2.6.3.2 b)'s unconditional state-change sentence); `NumReceiveCycles == -1` (IS-CYCLIC) is rejected synchronously; a `CancelComPrimitive` mid-receive-phase produces `PduCopstCancelled` and the CLL never reaches `COMM_STARTED` (unlike `CoptStopcomm`'s analogous, always non-cancellable phase — this one is fully `cancellable: true`, ADR-111); and `temp_param_update=1` with a genuine transmit failure (engineered via a software-ISO-TP multi-frame send whose `CP_N_Bs` FlowControl wait times out — the mock has no direct "fail the next write" backdoor, so this is the only existing way to force a real `TxFailure::Event`) reverts hardware to the live Active set, never reaches `COMM_STARTED`, and never emits `PduErrEvtInitError` (that event is K-line-init-specific). Re-run it after any change to `handle_start_comm`'s `else if let Some(tx) = tx` branch, `OneShotCommTx`, or the `CoptStartcomm` arm's `tx` resolution in `rpc_start_com_primitive`. Fixing this finding required updating one pre-existing test, `startcomm_comparam.rs`'s `can_explicit_five_baud_setting_is_inert_on_non_k_line_link`: it previously relied on a non-empty `cop_data` with no CAN addressing configured being silently discarded on CAN, which is no longer true post-ADR-111 (it now legitimately fails synchronously, unresolvable addressing) — changed to use empty `cop_data`, preserving the test's original, narrower "5-baud settings are inert on non-K-line" intent. A verification-pass follow-up round added `can_optional_message_is_multiple_collects_all_matches_and_reaches_comm_started` (`NumReceiveCycles = -2`, two ECUs answering the same request, mirroring `cop_ctrl_cycles.rs`'s `CoptSendrecv` IS-MULTIPLE test) and `j1850_optional_message_transmits_and_reaches_comm_started` (fire-and-forget on `J1850VPW`, confirming the `tx` branch is not accidentally CAN-specific despite `protocol_requires_init`'s former CAN/J1850 pairing) — re-run these two alongside the rest of the file for the same reasons as above. That same round also examined, but deliberately did NOT add, a genuine channel-hard-error-mid-phase test for this path: no primitive to force a hard channel error (a failing `PassThruReadMsgs`/`handle_channel_hard_error` trigger) exists anywhere in `j2534-0404-mock` or is used by any other test in this suite (confirmed by search — `stopcomm_data_tx.rs`'s own equivalent gap note, around the ADR-087 S3-guard discussion in this same file, documents the identical absence for `CoptStopcomm`). Accepted residual: the new `else if let Some(tx) = tx` branch's `ReceivePhaseOutcome::Terminal` arm's hardware-revert behavior for the hard-error sub-case (unconditional revert, not gated like `handle_send_recv`'s `!channel_lost`) is verified correct by code inspection only — see the code comment at that arm and ADR-111's Consequences section for the full reasoning — not by a dedicated test, for the same "no injection primitive for any COP type" reason.
   - **`stopcomm_data_tx.rs`** (ADR-085/ADR-087) — `stopcomm_data_tx.rs` (ADR-085) pins `CoptStopcomm`'s separate, non-empty-`cop_data` bus transmit (fire-and-forget by default, `NumReceiveCycles == 0`) — call-time resolution via `resolve_send_recv_tx` against the Active snapshot only (never Working, `temp_param_update` still ignored), `LOCK_PHYSICAL_TX_QUEUE` gating only when `cop_data` is non-empty, synchronous ADR-049/055 validation, and empty-`cop_data` behavior staying byte-for-byte unchanged — re-run it after any change to the `CoptStopcomm` branch of `rpc_start_com_primitive` or `handle_stop_comm`. It also pins the ADR-085 amendment's `stop_comm_pending` guard (`LogicalLinkState.stop_comm_pending`, an atomic test-and-set in the same critical section as the `comm_started` TOCTOU re-check): `stopcomm_second_concurrent_call_rejected_while_first_in_progress` (a second `CoptStopcomm` racing the first, held open via a `CoptDelay` queued on a sibling CLL of the same channel, since accepting a `CoptStopcomm` cancels every other COP on its own CLL, a running `CoptDelay` included, is rejected `FAILED_PRECONDITION` with "already in progress," while the first's transmit and single `PduCllstOnline`/`PduCopstFinished` sequence proceed unaffected), `stopcomm_pending_cleared_after_normal_completion_allows_full_cycle` (a `CoptStartcomm`→`CoptStopcomm`→`CoptStartcomm`→`CoptStopcomm` cycle still works, confirming the flag clears at normal completion), `stopcomm_rejected_synchronously_rolls_back_stop_comm_pending` (a synchronously-rejected `CoptStopcomm`, e.g. oversized `cop_data`, rolls the flag back so an immediately-following valid `CoptStopcomm` still succeeds), and `stopcomm_held_and_cancelled_by_clear_tx_queue_clears_stop_comm_pending` (a `CoptStopcomm` siphoned into `tx_held` by `PDU_IOCTL_SUSPEND_TX_QUEUE`, never reaching `handle_stop_comm`, still has `stop_comm_pending` cleared when `PDU_IOCTL_CLEAR_TX_QUEUE` drains and cancels it via `cancel_held_tx_items` — the fourth reset point, found one round after the other three). A round-4 Codex-review fix on the same ADR-085 amendment covers a fifth reset point, `should_skip_cancelled_item` (`events.rs`): `stopcomm_queued_and_cancelled_by_clear_tx_queue_never_transmits` and `stopcomm_queued_and_cancelled_by_cancel_com_primitive_never_transmits` (a `CoptStopcomm` still genuinely sitting in the poll task's mpsc queue — queued, not yet dispatched, and never parked in `tx_held` — cancelled via `PDU_IOCTL_CLEAR_TX_QUEUE` or an explicit `CancelComPrimitive` respectively: both assert `PduCopstCancelled` for the StopComm's own `cop_handle`, not `PduCopstFinished`, zero bytes ever written to the mock backdoor — the removed pre-ADR-085 carve-out used to let it execute and transmit anyway — and a follow-up `CoptStopcomm` succeeding, proving `stop_comm_pending` was cleared in the same critical section). A round-5 Codex-review fix on the same ADR-085 amendment covers a sixth reset point, `rpc_cancel_com_primitive` (`rpc_primitive.rs`): `stopcomm_held_and_cancelled_by_cancel_com_primitive_clears_stop_comm_pending` (a `CoptStopcomm` already siphoned into `tx_held` by `PDU_IOCTL_SUSPEND_TX_QUEUE`, cancelled via a bare `CancelComPrimitive` on its own `cop_handle` with the queue never resumed, cleared, or disconnected — asserts `PduCopstCancelled` is emitted for that `cop_handle`, not `PduCopstFinished`, and that a second `CoptStopcomm` immediately succeeds without any later resume/clear/disconnect, proving `stop_comm_pending` was cleared synchronously by the cancel RPC itself rather than by a later drain) — re-run these seven alongside the rest of `stopcomm_data_tx.rs` after any change to the `stop_comm_pending` test-and-set/rollback/reset points (`rpc_start_com_primitive`'s `CoptStopcomm` branch, `handle_stop_comm`, `handle_channel_hard_error`, `rpc_disconnect_com_logical_link`, `cancel_held_tx_items`, `should_skip_cancelled_item`, `rpc_cancel_com_primitive`). A round-6 Codex-review fix on the same ADR-085 amendment threads a `cancellable: bool` parameter through `transmit_request`/`transmit_request_inner`/`isotp_send` (positioned between `count_as_bus_activity` and `ctx`, mirroring `wait_for_p3_gap`'s parameter of the same name), `false` only at `handle_stop_comm`'s call site and `true` everywhere else, so `isotp_send`'s own FlowControl-wait cancellation check — previously unconditional, unlike `wait_for_p3_gap`'s already-gated one — can no longer truncate a mid-flight multi-frame software-ISO-TP StopComm transmit: `stopcomm_multiframe_cancel_during_transmit_does_not_abort` (`can_mode.rs`'s software-ISO-TP FlowControl-injection pattern, `BlockSize = 1` to force a second FlowControl wait) pins that a `CancelComPrimitive` on the StopComm's own `cop_handle`, arriving after the FirstFrame and first ConsecutiveFrame are already on the wire but before the second ConsecutiveFrame, does not abort the send: the full payload still reaches the mock backdoor, `PduCllstOnline`/`PduCopstFinished` are still emitted (not `PduCopstCancelled`), and no `PduErrorEvent` fires — re-run it, and the rest of `stopcomm_data_tx.rs`, after any change to `isotp_send`/`transmit_request`/`transmit_request_inner`'s `cancellable` threading or the FlowControl-wait cancellation check. ADR-087 adds a bounded, non-cancellable receive phase to the same non-empty-`cop_data` transmit: `stopcomm_num_receive_cycles_is_cyclic_rejected_synchronously`/`stopcomm_num_receive_cycles_below_minus_two_rejected_synchronously` pin the synchronous `INVALID_ARGUMENT` rejection of `NumReceiveCycles == -1`/`< -2` (with `stop_comm_pending` rolled back so a follow-up `CoptStopcomm` still succeeds), and `stopcomm_expected_response_delivers_result_data_before_teardown` pins the happy path — `NumReceiveCycles = 1` with a matching `expected_response_array` entry holds `PduCllstOnline` until the response arrives, then delivers it via `ResultData` with the descriptor's `acceptance_id` before the normal terminal sequence — re-run these three alongside the rest of `stopcomm_data_tx.rs` after any change to `StopCommTx`, the `CoptStopcomm` branch's `expected_response`/`num_receive_cycles` parsing, or `handle_stop_comm`'s `wait_for_expected_response` call. A PR #92 review round found `NumReceiveCycles = -2` (IS-MULTIPLE) could run this non-cancellable receive phase indefinitely against a chatty ECU (the shared engine resets its per-match deadline on every match, with no absolute cap, and `CancelComPrimitive` is deliberately ignored here) — fixed by `ExpectedResponseWait.match_reset_ceiling_ms` (`max(16 × response_timeout_ms, 2000ms)`, anchored once at receive-phase entry, clamping only the per-match deadline extension; `None` at `CoptSendrecv`'s call site, since that phase is cancellable). `stopcomm_is_multiple_chatty_ecu_bounded_by_match_reset_ceiling` pins this: a mock ECU chattering matching frames faster than the window would naturally close still ends the phase at roughly the ceiling bound, with `PduCllstOnline`/`PduCopstFinished` emitted and no `PduErrEvtRxTimeout` (since responses were collected). That test's `CP_P2Max = 50ms` means `16 × 50 = 800ms < 2000ms`, so `STOPCOMM_IS_MULTIPLE_CEILING_FLOOR_MS` dominates its assertions and `STOPCOMM_IS_MULTIPLE_CEILING_FACTOR` (16x) itself goes unexercised — a regression that broke the factor specifically (wrong multiplier, or `.max`/`.saturating_mul` operands swapped) would pass it untouched (edge-case-hunter finding, same review round). `stopcomm_is_multiple_chatty_ecu_bounded_by_ceiling_factor_not_floor` closes that gap with `CP_P2Max = 200ms` (`16 × 200 = 3200ms > 2000ms`, so the factor — not the floor — determines the ceiling), asserting the phase ends near ~3.2s rather than the ~2s floor. Re-run both, alongside the rest of `stopcomm_data_tx.rs`, after any change to `match_reset_ceiling_ms`, the `STOPCOMM_IS_MULTIPLE_CEILING_FACTOR`/`FLOOR_MS` constants, or the `Matched` arm's deadline clamp in `wait_for_expected_response`. Test coverage note (edge-case-hunter, PR #92 review): ADR-087's new S3-equivalent `still_on_this_channel` guard in `handle_stop_comm` — placed immediately after `transmit_request` succeeds and before the `wait_for_expected_response` call, to catch a disconnect+reconnect completing during the transmit before the (also non-cancellable) receive phase starts — has no dedicated regression test, for the same structural reason as the round-5/round-6 infeasibility notes above (see line 110): this crate's single-threaded `current_thread` `#[tokio::test]` runtime resolves an uncontended `.lock().await` without a genuine `Pending`-yielding point, so there is no natural preemption opportunity between the transmit's `Ok(())` arm and this guard's check for a concurrent disconnect+reconnect to land in. The guard's logic was verified correct by code inspection (mirrors `handle_send_recv`'s own S3 guard and this function's own pre-transmit guard, both structurally identical and already exercised) and its removal was confirmed to not be caught by the existing suite (a repro that hardcoded it to always pass still left all 238 tests green) — closing this gap would need a new test-only pause/gate hook inside `handle_stop_comm`, a production-code change beyond this fix's scope (same conclusion as the round-5/round-6 notes). **Correction/supersession (ADR-086, generation-aware RX attribution round):** this note's framing of the S3 guard as the mechanism that closes the stale-attribution window for the receive phase was inaccurate — it never did; the S3 guard only protects entry into the receive phase (a reconnect that has already completed by the time it runs), not any individual poll pass once the receive phase is under way. The actual, structurally-necessary fix is a `connect_generation` check inside `poll_rx_inner`'s `MatchProbe` attribution arm itself (the shared engine both `CoptSendrecv` and `CoptStopcomm` use), documented in its own ADR-086 round below. Unlike the S3 guard's own narrow window (still untested, for the reasons this note gives above — that specific gap is unaffected by the new fix and remains an accepted residual), the receive-phase's broader misattribution window turned out to be testable, now fixed and out of the backlog: `stopcomm_receive_phase_reconnect_mid_wait_does_not_misattribute_new_frame_to_stale_cop` exploits the poll loop's own real `tokio::time::sleep` as a natural preemption point, the same property round 11's `handle_delay` test already used, to prove `poll_rx_inner`'s `MatchProbe` attribution arm correctly rejects a stale-generation match after a mid-wait reconnect.
   - **`param_binding.rs`** — `param_binding.rs` pins the call-time-binding/BUSTYPE-guard/writeback claims that cut across all three temp-eligible COP types, plus the live-Active revert (a `CoptUpdateparam` queued ahead of a temp COP is not undone by the temp COP's hardware revert). Re-run both after any `handle_send_recv`/`handle_start_comm`/`rpc_start_com_primitive` change.
     - **`startcomm_comparam.rs`** (ADR-074/075/077 init-test cluster) — Its `iso14230_init_settings_five_baud_overrides_multi_byte_init_data`/`iso14230_init_settings_fast_overrides_single_byte_init_data`/`iso14230_init_settings_none_skips_init_sequence`/`set_com_param_init_settings_rejects_out_of_range_values` tests (ADR-074) pin `CP_InitializationSettings`-driven K-line init sequence selection (`1`/`2`/`3` overriding the legacy heuristic, `3` skipping the init call entirely via `MockBackdoor::five_baud_init_count`/`fast_init_count`, and the `1..=3` `SetComParam` range validation) — the param must be staged in Working *before* `ConnectComLogicalLink` for `CoptStartcomm` (no `temp_param_update`) to see it, since it binds from the Active snapshot; re-run these alongside `events::init_sequence_tests` (`select_init_sequence`'s pure-function unit tests) and `comparam_id::tests::init_settings_valid_range_is_1_to_3` after any change to `select_init_sequence`/`run_protocol_init`/`is_valid_init_settings`. The fast-init tests additionally pin ADR-075's header round trip — the call-time-constructed input frame via `MockBackdoor::fast_init_input` (including the Working-snapshot addressing under `temp_param_update`) and the response's payload/`extra_info` header-footer split — re-run them after any change to the `fast_init` construction in `rpc_start_com_primitive` or the response split in `handle_start_comm`. `iso14230_init_settings_fast_wakeup_only_on_empty_cop_data`/`iso14230_legacy_heuristic_empty_cop_data_still_skips_init`/`iso9141_init_settings_fast_wakeup_only_on_empty_cop_data` (ADR-077) pin the wakeup-only fast-init exception — explicit `CP_InitializationSettings == 2` with empty `cop_data` on a K-line link runs `FAST_INIT` with a NULL input (`fast_init_count() == 1` and `fast_init_input() == None` together, since a NULL-input call resets the mock's recorded input), delivers no `ResultData`/`ReceivedFrame` (the ISO14230 test drains the `SubscribeEvent` stream itself, not just the `GetEventItem`/rx_buf side, before asserting `PduCopstFinished`), and still emits `PduCllstCommStarted`/`PduCopstFinished`, on both K-line protocols; the absent-param legacy heuristic's empty-`cop_data`-means-skip behavior stays pinned at `fast_init_count() == 0`. `iso14230_temp_param_update_startcomm_resolves_wakeup_only_fast_init_from_working` mirrors the five-baud `temp_param_update` test above to pin that the wakeup-only gate itself resolves `CP_InitializationSettings` from `binding.resolved()` (Working under `temp_param_update=1`), not Active. `iso14230_wakeup_only_fast_init_failure_emits_error_and_reverts_temp_params` pins the init-failure path (`MockBackdoor::set_fast_init_error`): `PduErrEvtInitError` + `PduCopstFinished` are emitted, a `temp_param_update=1` transaction's apply/revert `SET_CONFIG` bracket still runs on failure, the failed attempt is not counted by `fast_init_count`, and a retried `CoptStartcomm` afterward is still accepted and succeeds (nothing left locked by the failure). Re-run all of these after any change to the `fast_init` construction in `rpc_start_com_primitive`, the `FastInit` dispatch/delivery gating in `handle_start_comm`/`run_protocol_init`, or the mock's `IOCTL_FAST_INIT` NULL-input/error-injection handling.
   - **`fd_can.rs`** (ADR-158, Phase 3 Stage 3a; extended by ADR-213/Round 3) — pins SAE J2534-2 clause 21 CAN FD's connect-time protocol substitution end to end: leaving `CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate` at their defaults connects as plain `CAN` with zero extra `SET_CONFIG` calls (regression test); staging them on an opted-in module connects with `PROTOCOL_FD_CAN_PS`, applies `CONFIG_FD_CAN_DATA_PHASE_RATE` before `CONFIG_J1962_PINS` (`server.backdoor.set_config_param_log`'s ordering), and assigns the base protocol's own default packed pins for an unqualified connect; `TX_DL == 8` alone does not trigger FD mode but a nonzero `CP_CANFDBaudrate` alongside it does; a disconnect + ComParam-clear + reconnect flips back to plain `CAN` on a brand-new physical channel; the opt-in/software-ISO-TP rejections at `ConnectComLogicalLink` time; and a directly-named `PROTOCOL_FD_CAN_PS`/`_CHx` protocol id is rejected at `CreateComLogicalLink` (message now mentions `_CHx` too). **ADR-213/Round 3 update:** the former `_CHx` rejection test is now the core promotion test (`fd_mode_combined_with_a_directly_named_chx_id_promotes_to_fd_can_chx`) — a `_CHx`-connected classic `CAN_CHx` link staging FD ComParams promotes to `FD_CAN_CHx` instead; new tests also pin the symmetric reversion (`reconnect_after_clearing_fd_comparams_on_a_chx_link_flips_back_to_can_chx_not_bare_can`), the `_CHx`-specific capacity cap (`connect_rejects_a_chx_index_within_the_generic_capacity_but_above_fd_cans_own`, via `MockBackdoor::set_fd_can_chx_capacity`), and that a `_CHx` FD connect does not synthesize `CONFIG_J1962_PINS` (`fd_can_chx_connect_does_not_synthesize_j1962_pins`). The mock's own `_CHx`-attach-on-rate gate (`ERR_PIN_INVALID` on I/O until `CONFIG_FD_CAN_DATA_PHASE_RATE` is set) is pinned at the native-mock layer instead, in `j2534-0404-mock/src/lib.rs`'s own `tests` module (`fd_can_chx_io_rejected_until_data_phase_rate_is_set_then_succeeds`) — unreachable through the gRPC surface, since the service always sets that rate as part of the same connect that opens the channel. Re-run it after any change to `apply_fd_mode`, `to_j2534_config_id`'s FD exception, `resources::fd_protocol_id`/`fd_protocol_id_for_link`/`is_fd_protocol_id`/`default_pin_select_for_base`, or the mock's FD_CAN_PS/`_CHx` simulation.
   - **`tp20.rs`** (ADR-188, Phase 7 Stage 7a) — pins SAE J2534-2 clause 19 TP2.0's active-connection lifecycle end to end against `PROTOCOL_TP2_0_PS` (resource `0x025F`): a `CoptStartcomm`/`CoptStopcomm` cycle establishes and cleanly tears down one connection; the mock's 4-slot-per-physical-channel cap rejects a 5th concurrent connection; the five minted `PARAM_TP20_*` ComParams round-trip via `SetComParam`/`GetComParam`; the resource row's closed pin pair (6/14) is enforced at `CreateComLogicalLink` time; two CLLs sharing one physical channel independently connect and tear down; RX frames route only to the CLL whose established connection's RX-ID matches, reusing `events_rx_routing.rs`'s ADR-184 `UniqueRespIdKey` machinery; (Codex review fix, PR #97, ADR-188 Fix A/B) a client-staged `CP_TP20TxIdProposal` never substitutes for a real established connection, and a non-`Established` CLL's own end-to-end health around a `CoptStopcomm` transition is unaffected by the routing-construction fix (the routing fix's own direct proof lives in `events_build_cll_rx_entries_tests.rs` instead -- see the Prioritized Backlog for why an end-to-end proof of the cross-CLL leak itself isn't constructible through this harness); and (Codex review fix, PR #97, ADR-188 Fix D) `stopcomm_stops_this_clls_own_repeat_slots_before_native_teardown` pins that `CoptStopcomm` stops a live `PDU_IOCTL_START_REPEAT_MESSAGE` slot this CLL owns (observed via `MockBackdoor::repeat_message_exists`) before/alongside the native connection teardown, not leaving it retransmitting the now-stale established TX-ID; and (Codex review fix, PR #97, ADR-188 Fix G, 4th round) `disconnect_mid_wait_after_native_issue_tears_down_the_leaked_slot` pins that a requesting CLL going stale (disconnect) strictly AFTER its native `IOCTL_REQUEST_CONNECTION` call succeeded but WHILE still waiting for the indication gets its leaked slot best-effort torn down, using a new mock hold hook (`__mock_set_tp20_no_indication`) to make the wait genuinely block long enough for a real disconnect to land inside it deterministically (Fix F, the sibling pre-issue `self_still_live` recheck, has no analogous end-to-end construction available -- see that test's own doc comment and the Prioritized Backlog); (Codex review fix, PR #97, ADR-188 Fix H) `tx_header.rs`'s own unit tests `response_header_bytes_tp20_prepends_the_supplied_established_rx_id`/`response_header_bytes_tp20_rejects_when_not_established` pin `response_header_bytes`'s new `PROTOCOL_TP2_0_PS` arm (the RX-direction counterpart of Fix A's `build_tx_message` fix), and `stopcomm_stops_this_clls_own_repeat_slots_before_native_teardown` above (unchanged) continues to exercise it indirectly through a live repeat slot; and (Codex review fix, PR #97, ADR-188 Fix I) `stopcomm_serializes_against_a_racing_repeat_message_start` races `PDU_IOCTL_START_REPEAT_MESSAGE` against `CoptStopcomm` and checks the correctness outcome (no repeat slot survives with a stale TX-ID either way) -- see that test's own doc comment for why it cannot force the exact interleaving the fix closes, and the Prioritized Backlog for that tracked residual. Re-run it after any change to `events_tp20_connection.rs`, the TP2.0 arms in `events.rs`/`tx_header.rs`/`events_rx_routing.rs`, `resources.rs`'s TP2.0 row/pins, or the mock's `IOCTL_REQUEST_CONNECTION`/`IOCTL_TEARDOWN_CONNECTION` handlers.
   - **`ethernet_ndis.rs`** (ADR-194, Phase 16) — pins SAE J2534-2 clause 24 Ethernet_NDIS end to end against `PROTOCOL_ETHERNET_NDIS` (resource `0x0261`): `ConnectComLogicalLink` resolves `CP_NdisPinOption` (values 1/2) into `CONNECT_FLAG_NDIS_PINS_OPTION1`/`OPTION2` on the native `PassThruConnect` call, with the default (unset/0) passing no NDIS pin flag; a mock-injected native `ERR_NO_CONNECTION_ESTABLISHED` at connect time maps to `PDU_ERR_NO_CABLE_DETECTED`, scoped only to Ethernet_NDIS connect attempts (`MockBackdoor::set_ndis_connect_error`); every `ComOperationType` is rejected `INVALID_ARGUMENT`/`PDU_ERR_ID_NOT_SUPPORTED` unconditionally, including the receive-only/`CoptStopcomm`-on-a-never-started-link exemptions Analog Inputs and other protocols grant but clause 24 does not, and independent of `comm_started` state (the gate runs before the pre-flight `comm_started` checks); the physical channel's poll task stays alive after connect (no pass-all `PassThruStartMsgFilter` installed, since clause 24.2.5.5 payload traffic never crosses the J2534 read/write API) without tripping `handle_channel_hard_error`; `PDU_IOCTL_GET_NDIS_ADAPTER_INFO` decodes the hand-packed `NDIS_ADAPTER_INFORMATION` struct (`AdapterUniqueID`/`AdapterName`/`Status`/`MAC_Address`/`IPV4_Address`/`IPV6_Address`) from `mock_ndis_adapter_info()`'s fixture via `pack_ndis_adapter_info`, with `EthernetPinConfig` specifically reflecting the connected channel's own resolved pin option (`1` for auto/Option 1, `2` for Option 2 — Codex review, PR #102 round 9, `get_ndis_adapter_info_reports_option_2_when_connected_with_option_2`); and a module opted into SAE J2534-2 (`start_with_modules`) with Discovery reporting `DEVICE_INFO_ETHERNET_NDIS_SUPPORTED` unsupported rejects `ConnectComLogicalLink` per ADR-185 Stage 1. Re-run it after any change to `rpc_link.rs`'s `ndis_pin_option_connect_flags`/the pass-all-filter Ethernet_NDIS exclusion, `rpc_primitive.rs`'s Ethernet_NDIS COP gate (and its placement relative to the `comm_started` pre-flight block), `events.rs`'s `rx_supported` gating in `ChannelPollCtx`/`poll_rx_inner`, `rpc_misc.rs`'s `ioctl_get_ndis_adapter_info`/`pack_ndis_adapter_info`, `error.rs`'s `ERR_NO_CONNECTION_ESTABLISHED` mapping, `resources.rs`'s Ethernet_NDIS row/pins/`connect_discovery_check` arm, or the mock's `IOCTL_GET_NDIS_ADAPTER_INFO`/`is_ethernet_ndis_protocol`/connect-error-injection handlers.
4. After physical channel sharing or poll-task changes, confirm CLL lifecycle tests still pass, and re-run `tests/grpc_mock/response_distribution.rs` (`iso15765_shared_channel_routes_responses_only_to_the_matching_cll` and neighbours) — it pins ADR-007's `poll_rx` routing (per-CLL CAN-ID matching, per-entry `unique_resp_identifier` attribution, no-table wildcard mode) and ADR-014's `unique_resp_ids` expected-response gate across CLLs sharing a channel. `tests/grpc_mock/p3_gap.rs` pins `CP_P3Func`/`CP_P3Phys` minimum inter-request gap enforcement (ADR-060), including that the gap state is scoped to the shared physical channel across CLLs, not to one CLL — re-run it after `SharedChannel`/poll-task threading changes too.
5. After library name resolution changes, update `startup-spec.md` and `vci-service-config/docs/logging-config.md` together.
6. `tests/live_grpc_flow.rs` is a manual, opt-in gRPC-level smoke test against a real J2534 device (`J2534_LIVE_SERVICE_TEST=1` + `J2534_DLL_PATH`/`J2534_LIVE_LIBRARY_NAME`; no-ops otherwise, so it never runs in CI; the `J2534_DLL_PATH` mode is debug-builds-only because it rides on the runtime `VCI_CONFIG_PATH` override, ADR-073). It is not a substitute for the `grpc_mock/` suite in the normal dev loop, but is worth re-running by hand against real hardware after changes to the CAN-family **or K-line** CLL lifecycle (connect/addressing/COP flow) — the `J2534_LIVE_*` env vars select the target family (CAN by default; set `J2534_LIVE_PROTOCOL_NAME`/`J2534_LIVE_BUS_TYPE_NAME`/pin vars for K-line), and each CAN-class `SetComParam` call falls back to its KWP-class equivalent on rejection so the same flow drives either family; a K-line `SetUniqueRespIdTable` rejection is left as an empty table rather than a `CP_EcuRespSourceAddress`-only entry — as of [ADR-203](../../../docs/adr/ADR-203-kwp-j1850-source-address-rx-routing.md), RX routing (`events_rx_routing.rs::route_frame`) DOES have a matching tier for `CP_EcuRespSourceAddress` on KWP/J1850 (in addition to `CP_CanRespUSDTId`/`CP_CanRespUUDTId`/`CP_J1939SourceAddress`), so this is now a simplification rather than a required workaround — see `docs/j2534-0404-architecture.md` §10.
7. `examples/` holds real-hardware gRPC **client** demo binaries -- one per protocol family, connecting to an already-running `j2534-0404-service` process via `VciServiceClient` (unlike `tests/live_grpc_flow.rs`, which embeds the server in-process as a test harness; start the service as described in `startup-spec.md`, then run `cargo run -p j2534-0404-service --example <name> -- <addr>`). Shared connect/lifecycle/event-wait boilerplate lives in `examples/common/mod.rs` (loaded into each single-file example via `#[path = "common/mod.rs"] mod common;`, since Cargo's example layout doesn't let a same-directory example implicitly `mod` a sibling subdirectory); protocol-specific resource selection, `SetComParam`/`SetUniqueRespIdTable` addressing, and the `CoptSendrecv` payload stay in each protocol's own file. Coverage is now complete for all 14 protocol families this service implements: `grpc_can` (raw CAN, `ISO_11898_RAW`), `grpc_iso15765` (ISO 15765-2 / UDS-on-CAN), `grpc_iso9141` (ISO 9141-2 K-line), `grpc_iso14230` (ISO 14230-4 KWP2000 K-line), `grpc_j1850vpw`/`grpc_j1850pwm` (SAE J1850, each targeting its own fixed-flavor resource row rather than the auto-detecting combined `SAE_J1850` bus, ADR-070, for a deterministic single-protocol demo), `grpc_sci` (SAE J2610 SCI, all four native configs in one CLI-selectable file), `grpc_uart_echo_byte` (SAE J2534-2 clause 12), `grpc_honda_diagh` (clause 13), `grpc_j1708` (clause 17), `grpc_j1939` (clause 16, including the `CoptStartcomm`-driven address claim), `grpc_tp2_0` (clause 19, staging its five mandatory `PARAM_TP20_*` ComParams by raw numeric ID since they have no `GetObjectId`/name-based resolution -- see the Prioritized Backlog), `grpc_gm_uart` (clause 11, `CP_Baudrate` mandatory since clause 11.3 seeds no default), and `grpc_ethernet_ndis` (clause 24, connect-only -- clause 24 defines no `ComPrimitive`/COP-level API surface at all, so this example demonstrates `GetResourceIds` -> `CreateComLogicalLink` -> `ConnectComLogicalLink` -> `PDU_IOCTL_GET_NDIS_ADAPTER_INFO` -> teardown instead of a `CoptStartcomm`/`CoptSendrecv`/`CoptStopcomm` flow). `ANALOG_IN` is deliberately NOT covered by an example -- it is a read-only measurement channel (32 independent acquisition resources), not a diagnostic communication protocol, so it does not fit this directory's `CoptSendrecv`-request/response demo shape; this is a scope decision, not an oversight. `SetUniqueRespIdTable` now appears in seven of the fourteen examples, each keying it on the `PDU_PC_UNIQUE_ID`-class ComParam ADR-042/ADR-184/ADR-202/ADR-203 establish for that protocol family, set exclusively through this call rather than a plain `SetComParam`: `grpc_can` uses `CP_CanRespUUDTId` (unformatted); `grpc_iso15765` uses `CP_CanRespUSDTId` (ISO-TP-segmented); `grpc_iso9141`/`grpc_iso14230`/`grpc_j1850vpw`/`grpc_j1850pwm` share `CP_EcuRespSourceAddress` (ADR-203's KWP/J1850 RX-routing tier); `grpc_j1939` uses `CP_J1939SourceAddress` (ADR-184), additionally client-side-filtering the response's PGN over `ResultData.extra_info.header_bytes` since the payload-only `ExpectedResponseData` descriptor can't see it (the header is split off before matching, ADR-051). The remaining protocols have no equivalent RX-classification mechanism to install: `grpc_sci`/`grpc_honda_diagh` select the responding ECU by physical wiring, not addressing; `grpc_gm_uart` has no addressing ComParam at all (master/slave poll bus); `grpc_j1708` genuinely has no per-node RX routing tier in this service (documented as an accepted limitation in that file); `grpc_tp2_0`'s routing is connection-scoped (`tp20_tx_id`/`tp20_rx_id`, synthesized per CLL, not table-driven); `grpc_ethernet_ndis` has no `ComPrimitive`/response-matching flow at all.

## 2022-edition delta audit (2026-08-11)

The findings below come from six parallel design-advisor audits comparing the ISO 22900-2:2022(en)
edition's normative text against this repo's current implementation, each covering one of the same
six areas `iso22900-2-conformance-audit.md`'s original 2009-based audit used (ComParam system,
Resource/Object ID model, IOCTL command set, Error/status model, ComPrimitive execution & response
binding, TX/RX message construction). Each was instructed to skip anything already tracked and
report only genuinely new 2022-specific findings. See that audit doc's own new "2022-edition delta
audit" note (added in this same pass) for the overall context.

- P2 (2022 delta audit, Error/status model area): `PduError`'s enum is missing the seven TLS
  connection-failure codes ISO 22900-2:2022 Annex D.3 adds (`PDU_ERR_TLS_REQUIRED` through
  `PDU_ERR_TLS_CIPHER_NOT_SUPPORTED`, `vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:10372-10378`,
  also listed as normative `PDUConnect` return values at `:1919-1923`). The proto's `PduError` enum
  (`vci-service-interface/src/proto/service.proto:130-133`) and `d_pdu_api_defs.h`
  (`iso22900-sys/src/bindings/d_pdu_api_defs.h:222`) both stop at `PDU_ERR_DOIP_RESPONSE_TIMEOUT`
  (0xBC) — the range 0xBD-0xC3 is absent from both. Unreachable via `j2534-0404-service` itself (no
  J2534 v04.04 native code has TLS/DoIP semantics — genuinely inert there), but reachable via the
  shared `iso22900-service`, whose `DPduApiError::PduError(UNUM32)` pass-through
  (`iso22900-service/src/error.rs:79-87`) forwards a raw driver-returned 0xBD..0xC3 numerically
  intact but undecodable by any typed client (`PduError::try_from` fails) — not a shared-proto-level
  gap this repo's own scope originates, but the enum itself is a shared artifact. Not investigated or
  scoped here — closing it needs a proto enum extension plus a `d_pdu_api_defs.h` edit, and the
  latter requires regenerating the bindings for every target, so it should be its
  own scoped task rather than folded into an unrelated PR.
- P2 (2022 delta audit, IOCTL command set area): ISO 22900-2:2022 adds a new CLL-scoped IOCTL,
  `PDU_IOCTL_CLEAR_TX_QUEUE_PENDING` (§8.5.7, `vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:3602-3621`,
  Table 47), with no 2009 counterpart — it cancels only ComPrimitives still waiting to transmit
  (IDLE/WAITING), leaving receive-only ComPrimitives running, distinct from the broader existing
  `PDU_IOCTL_CLEAR_TX_QUEUE`. Not in `names.rs`'s `map_ioctl_name` (`j2534-0404-service/src/service/names.rs:1553-1586`)
  or `service_params.rs`, so `GetObjectId(OBJT_IO_CTRL, "PDU_IOCTL_CLEAR_TX_QUEUE_PENDING")` rejects
  identically to a typo'd name, and no dispatch arm exists in `rpc_misc.rs`. Unlike the four
  deliberately-unsupported IOCTLs (`GENERIC`/`GET_CABLE_ID`/`SEND_BREAK`/`READ_IGNITION_SENSE_STATE`),
  the "no underlying J2534 v04.04 hardware capability" rationale doesn't apply here — this is
  adapter-layer queue state the service already tracks (ADR-021's IDLE/WAITING distinction,
  `tx_held`/`cancelled_cops`), so it's a genuine 2009-baseline staleness gap against the workspace's
  stated 2022 target, not an accepted hardware-capability limitation. Implementation note for
  whoever picks this up: the 2022 text itself is internally inconsistent about the name — §8.5.7's
  heading/Table 47 say `..._CLEAR_TX_QUEUE_PENDING`, the body's first sentence says
  `PDU_IOCTL_CLEAR_TX_PENDING` (line 3604), and Table 41's IOCTL overview (lines 3457-3483) omits
  the command entirely — the eventual name-table entry should probably resolve both spellings. Not
  investigated or scoped here; ADR-worthy once implemented (the cancellation-scope mapping onto
  `tx_held`/`cancelled_cops` vs. receive-only CoPs, and the dual-spelling question, are both design
  decisions).
- P3 (2022 delta audit, IOCTL command set area, documentation staleness only, not a code defect):
  the 2022 edition adds eight DoIP/Ethernet/TLS/ISOBUS IOCTLs (`PDU_IOCTL_VEHICLE_ID_REQUEST`,
  `SET_ETH_SWITCH_STATE`, `GET_ENTITY_STATUS`, `GET_DIAGNOSTIC_POWER_MODE`, `GET_ETH_PIN_OPTION`,
  `TLS_SET_CERTIFICATE`, `TLS`/`DOIP_GET_CURRENT_SESSION_MODE` — the 2022 text itself disagrees on
  this one's own name between Table 41 and its own §8.5.26 heading — and `ISOBUS_GET_DETECTED_CFS`;
  `vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:3476-3483,4011-4369`), none present in
  `names.rs`/`service_params.rs`. Not implementing them is legitimate — J2534 v04.04 has no
  DoIP/ISOBUS substrate, and 2022 §8.4.4.4 makes MDF listing the discovery contract for supported
  IDs, so an absent shortname mirrors an absent MDF entry (consistent with this audit's own
  already-verified A3-3 stance). **Partially stale as of ADR-194/Phase 16 (SAE J2534-2 clause 24
  Ethernet_NDIS):** the "no substrate" premise no longer fully holds for `GET_ETH_PIN_OPTION`
  specifically — `PDU_IOCTL_GET_NDIS_ADAPTER_INFO`'s native `NDIS_ADAPTER_INFORMATION.
  EthernetPinConfig` field is a real, now-implemented native data source `GET_ETH_PIN_OPTION`'s own
  answer could in principle be derived from. This phase does not itself implement `GET_ETH_PIN_OPTION`
  (a 2022-only IOCTL, semantically pin-number-in/option-out and module-adjacent, distinct from
  clause 24's channel-scoped `GET_NDIS_ADAPTER_INFO` — see ADR-194's own rejected-alternatives
  section for why the two were not merged), so the other seven DoIP/Ethernet/TLS/ISOBUS IOCTLs above
  (and `SET_ETH_SWITCH_STATE`, which also has no clause-24 substrate — clause 24 ties activation to
  the connect/disconnect lifetime, not an independent toggle) remain fully covered by the original
  "no substrate" reasoning. The only actionable residue: `service_params.rs:494-508` and
  ADR-079 frame "17 commands" as the IOCTL command set's complete enumeration — true only of the
  2009 edition; worth a doc-comment update next time that file is touched for an unrelated reason.
- P3 (2022 delta audit, IOCTL command set area, favorable drift, doc-only): the event-queue default
  eviction mode's spec-side default annotation moved between editions — 2009 Table 57
  (`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:3630-3632`) marks
  **Limited** as the CLL default, 2022 Table 58 (`vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:3904`)
  marks **Unlimited**. This service's own default is `OverwriteOldest`
  (`rpc_misc.rs:3382-3384`, ADR-079 item 14, chosen because the buffer itself is bounded) — under
  the 2009 default (Limited) this was a latent, never-flagged mismatch for a client that never calls
  `SET_EVENT_QUEUE_PROPERTIES`; under the 2022 default (Unlimited) the code's existing choice now
  falls squarely under ADR-079's already-accepted bounded-buffer deviation instead of being an
  unadjudicated divergence. No code change needed — recommend a one-line note on
  `rpc_misc.rs:3383`'s "pre-existing default" comment (or ADR-079 itself) recording the 2022
  alignment, next time either is touched.
- P3 (2022 delta audit, ComPrimitive execution area, spec self-contradictory — capped at P3 for that
  reason): ISO 22900-2:2022 Table 6's closing row (`vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:679`)
  states `NumSendCycles=0` combined with `NumReceiveCycles=-2` is disallowed, a combination the 2009
  edition never addressed — but the 2022 edition's own Table 5 (`:654`) still lists `-2` in the
  RECEIVE ONLY row, so the two 2022 tables disagree with each other. Current code accepts and
  executes `(0, -2)` (`rpc_primitive.rs:1354-1365`, `events.rs:11614`, the ADR-100 round-9 tier-2
  registrant fix). Lenient-acceptance only; no harm to a conformant client. Not investigated or
  scoped here — reasonable dispositions are either rejecting synchronously (mirroring the existing
  `num_receive_cycles < -2` rejection arm) or documenting current acceptance as a deliberate
  deviation citing the Table 5/Table 6 self-contradiction, whoever picks this up should pick one.
  Re-flagged (not fixed) by [ADR-182](../../../docs/adr/ADR-182-cp-cyclic-resp-timeout-finite-receive-only-scope.md)'s
  own audit round, which widened `CP_CyclicRespTimeout`/tier-2 detachment to finite `N > 0`
  created-receive-only COPs but deliberately left this `(0, -2)`-validation gap untouched per its
  own explicit non-scope.
- P3 (2022 delta audit, ComPrimitive execution area): ISO 22900-2:2022 Table 26
  (`vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:2470`, absent from 2009's equivalent
  at `iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:2312`) and Table 9's 7F-row NOTE
  (`:758`) require rejecting a `SENDRECV`/`STARTCOMM`/`STOPCOMM` `StartComPrimitive` with
  `PDU_ERR_INVALID_PARAMETERS` when `NumSendCycles > 0` but the CoP's message data is empty. Current
  code only rejects empty `cop_data` in software-ISO-TP mode (`rpc_primitive.rs:1263-1268`); other
  protocols build and transmit a header-only message that passes the size-range check (e.g. a
  4-byte CAN-ID-only frame on raw CAN), and `CoptStartcomm`/`CoptStopcomm` never compare
  `num_send_cycles` against data emptiness at all (`rpc_primitive.rs:1215-1220`). Only
  non-conformant client input reaches this path — narrow blast radius. Not investigated or scoped
  here.
- P3 (2022 delta audit, ComPrimitive execution area, informational, no action needed): ISO
  22900-2:2009 Annex A.1 (Tables A.2/A.3, the RxStatus/TxFlags↔RxFlag/TxFlag mapping statement at
  `iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:4794` and CP↔IOCTL rows at
  `:4806-4824`) has no 2022 counterpart at all — 2022's own Annex A covers only D-Server API/ODX
  mappings, and `RxStatus`/`TxFlags`/`PASSTHRU_MSG` appear nowhere in the 2022 text. This repo's
  existing citations of this annex are already correctly pinned to the 2009 edition specifically
  (`comparam_id.rs:109`, `service_params.rs:98`, `comparam-protocol-support.md:558,630`) — no code
  change needed. Recorded here so a future "verify against 2022" sweep of this audit doc's own
  Annex-A-based reasoning can close as "clause removed in 2022; the 2009 edition remains the sole
  mapping authority" rather than re-investigating.
- **Resolved by [ADR-202](../../../docs/adr/ADR-202-j1850-unique-id-comparam-reclassification.md):**
  `J1850_UNIQUE_ID_UNUM32` (`comparam_support.rs`) now lists all four response-addressing params
  below alongside `CP_MidRespId`, mirroring `KWP_UNIQUE_ID_UNUM32`; the rest of this finding is kept
  for its own analysis/citations. P2 (2022 delta audit, ComParam system area, ADR-worthy — protocol
  interpretation, not a mechanical
  fix): ISO 22900-2:2022 Table B.11 (`vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:5864,5873,5874,5903`)
  classifies `CP_EcuRespSourceAddress`, `CP_FuncRespFormatPriorityType`, `CP_FuncRespTargetAddr`, and
  `CP_PhysRespFormatPriorityType` as `PDU_PC_UNIQUE_ID`-class, mandatory-support, for SAE J1850
  VPW/PWM — and scopes `CP_MidRespId` (line `:5896`) to SAE J1708 only, not J1850. This repo's
  `J1850_UNIQUE_ID_UNUM32` (`comparam_support.rs:591-593`) has exactly the inverse: it lists only
  `CP_MidRespId`, feeding both `GetUniqueRespIdTable`'s J1850 template (`rpc_misc.rs:3545`) and
  `SetUniqueRespIdTable`'s per-entry class validation (`rpc_misc.rs:3695-3765`), which rejects
  every other param with `PDU_ERR_COMPARAM_NOT_SUPPORTED`. Concrete scenario: a 2022-conforming
  client on an `ISO_15031_5_on_SAE_J1850*` CLL follows the spec's own template-then-fill-then-Set
  flow for per-ECU response identification and is rejected on every entry — meanwhile this
  service's own READ side already resolves `CP_EcuRespSourceAddress`/`CP_PhysRespFormatPriorityType`
  entries-first from the URID table for J1850 (`tx_header.rs`'s `ecu_addr`/`response_header_bytes`
  fallback chains, per prior Codex rounds 11-12) — so the write path makes the read path's own
  per-ECU mechanism unreachable. This sharpens (does not duplicate) a prior backlog item that used
  to sit in the Prioritized Backlog (`CP_PhysRespFormatPriorityType` alone, explicitly
  deferred pending "its own read of ISO 22900-2's UNIQUE_ID classification"): this finding answers
  that question and broadens it to four params plus the bogus `CP_MidRespId` entry, and — now that
  ADR-202 has resolved it — that prior entry has been deleted per this repo's Prioritized Backlog
  convention rather than kept as a closed record. `route_frame`'s (`events.rs`) own table-mode KWP/J1850
  RX-drop gap — once tracked as a separate P2/P3 here (ADR-148 third Amendment) — was already closed
  at the time of this audit (before ADR-203): `events_rx_routing.rs::build_cll_rx_entries` filtered
  any `UniqueRespIdTable` entry configuring none of `CP_CanRespUSDTId`/`CP_CanRespUUDTId`/
  `CP_J1939SourceAddress` (which a KWP/J1850 `CP_EcuRespSourceAddress`-keyed entry always was, at the
  time) out of `unique_resp_ids` entirely, so `route_frame` saw an effectively-empty table and fell
  back to its no-table wildcard delivery instead of dropping frames (Codex review finding, PR #72
  round 1, ADR-179/Phase 5) — so this Set-side rejection's own blast radius was not bounded by that
  RX-side drop; a client whose `SetUniqueRespIdTable` call was rejected only lost per-ECU response
  disambiguation (frames still arrived, undifferentiated,
  via the wildcard fallback), not RX delivery entirely. The
  2009 edition could not have settled this itself — its own transport-vs-application-layer ComParam
  tables contradict each other on these params' class (`iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:5698,5706`
  vs `:6006,6014`), which is why it survived the original 2009-based audit. Companion P3, same
  table: `KWP_UNIQUE_ID_UNUM32`/`CAN_UNIQUE_ID_UNUM32` (`comparam_support.rs:558-588`) both include
  `CP_MidRespId` plus (for CAN) the four J1850 response params — surplus/inert entries, not
  incorrect, in both editions. ADR-202's own fix retains the same surplus `CP_MidRespId` entry in
  the now-corrected `J1850_UNIQUE_ID_UNUM32` for the identical reason (an empty Table B.11 cell
  means "not required", not "forbidden") — it belongs to that same future uniform-removal pass
  across all three lists, if one is ever done. The design-advisor consult this finding originally
  called for (given the interaction with the already-built `route_frame`/read-side mechanism) has
  since happened — see ADR-202. That consult (and a first-pass verification before it, corrected by
  a Codex review on ADR-202's own PR) found frame *delivery* to a CLL was, at the time, unaffected
  (`route_frame` ran in no-table wildcard mode for KWP/J1850 regardless of this table's contents),
  but the *URID tagging* of each delivered frame was wrong: every frame was tagged the wildcard
  sentinel `unique_resp_identifier = 0` rather than the application-assigned identifier ISO
  22900-2:2022 §8.4.28.7.2's match-then-return-URID model requires, and a restricted
  `ExpectedResponseData.unique_resp_ids` descriptor could never match on these protocols as a
  result. **Resolved by [ADR-203](../../../docs/adr/ADR-203-kwp-j1850-source-address-rx-routing.md):**
  `route_frame` now has a real `CP_EcuRespSourceAddress`-keyed matching tier for KWP/J1850, so a
  table-mode CLL gets its application-assigned URID on RX delivery (and a restricted
  `unique_resp_ids` descriptor now actually matches) instead of the wildcard sentinel; see that ADR
  for the design and its own accepted residuals.
- P3 (2022 delta audit, ComParam system area): ISO 22900-2:2022 Table B.13 adds bit 6 ("Addressing
  Scheme Ext": NormalFixed/Mixed, `vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:5985`,
  Table B.14 combinations at `:5996-6003`, NOTE 2 at `:6061`) with no 2009 equivalent (whose bit-3
  row conflates N_AE/N_TA — `iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:5793-5795`).
  `CanIdFormat::from_raw` (`rpc_link.rs:284-316`) decodes only bits 3/1/0; its doc comment enumerates
  bits 5-0 with no bit 6. `SetComParam` has no value whitelist for `CP_Can*Format`
  (`rpc_link.rs:5884-6038`), so a client staging e.g. `0x47` (NormalFixed, 29-bit, segmented, FC) or
  `0x4D` (Mixed) is accepted; decoding happens to be frame-correct by coincidence today
  (NormalFixed→Normal composes identical bytes, Mixed→Extended prepends the ExtAddr byte where N_AE
  belongs) but is entirely undocumented. Inert today — worth a `CanIdFormat` doc-comment update next
  time this file is touched, so a future format-value whitelist or equality-comparison change
  doesn't silently break bit-6 configurations. Not investigated or scoped here.
- P3 (2022 delta audit, ComParam system area, cosmetic error-shape gap): ISO 22900-2:2022 adds
  several optional K-line/ISO15765 ComParams this service already declares constants for but never
  wires up (`CP_5BaudAddressInverted`, `CP_5BaudCommBaudrateOverride`, `CP_5BaudInitBaudrate`,
  `CP_DisableTransportChecksumCheck`, `CP_EscapeSequenceHandling`, `CP_ISOKeyByteCount`,
  `CP_MaxDataLength_Ecu`, `CP_EnableInitSeqRepetition` — `vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:5812,5816,5817,5853,5866,5878,5893,5762`,
  consts already at `service_params.rs:407-447`). None appear in `names.rs`'s name map or any
  `comparam_support.rs` allowlist, so a by-name Get/SetComParam fails name resolution
  (`invalid_argument`, unknown name) instead of the known-but-unsupported
  `PDU_ERR_COMPARAM_NOT_SUPPORTED` shape 2022 Tables 23/25 associate with an unsupported optional
  param. Non-support itself is conformant (these are Optional/ECU-usage params) — this is dead-const
  cleanup and an error-shape nit, not a functional gap. Not investigated or scoped here.

## SAE J2534-1 v04.04 conformance audit (2026-08-11)

The findings below are this subsystem's first-ever systematic audit against SAE J2534-1 v04.04
(DEC2004), the native/hardware-facing PassThru spec (as opposed to the ISO 22900-2 D-PDU-facing
audit above). Three parallel design-advisor passes each covered one area: Connect/Protocol/IOCTL
surface, SConfig/ComParam mapping, and message TX/RX/filters. All FFI numeric constants
(`j2534-0404-sys/src/bindings/j2534_v0404.h`) were separately verified correct against the spec in
every area — no silent wrong-constant class of bug was found anywhere in this pass.

- P3 (J2534-1 audit, Connect/Protocol/IOCTL area): the spec's ADD/DELETE/CLEAR_FUNCT_MSG_LOOKUP_TABLE
  IOCTLs (J2534-1 §7.3.10-12, `vehicle-comm-specs/j2534-1-0404/J2534_1_200412...md:1401,1414,1444`)
  — which configure a J1850PWM adapter's physical layer to accept functionally-addressed frames —
  are exposed by the safe wrapper (`j2534-0404/src/lib.rs:587,670,679`) but never called anywhere in
  `j2534-0404-service/src` (confirmed by grep). ISO 22900-2:2009 Table A.3
  (`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:4814`) maps
  `CP_FuncRespTargetAddr` onto exactly these three IOCTLs; this service instead stores that
  ComParam (`service_params.rs:287`) and does purely service-side RX matching, never configuring the
  device's physical-layer acceptance. Concrete scenario (real-hardware-only — `j2534-0404-mock`
  doesn't model physical-layer gating, so no existing test catches this): a D-PDU client on
  `SAE_J1850_PWM` whose ECU replies with a functionally-addressed frame — on a conforming adapter,
  that frame never reaches the RX queue at all, since the physical layer itself was never told to
  accept it; a physically-addressed reply (target = `NODE_ADDRESS`) is unaffected. Not investigated
  or scoped here — if fixed, this is ADR-worthy (the shared-channel "does a joiner also re-issue
  ADD_TO_..." question, mirroring ADR-044/065's precedent).
- P3 (J2534-1 audit, Connect/Protocol/IOCTL area, doc-only — the FFI header itself is correct):
  [ADR-017](../../../docs/adr/ADR-017-j2534-1-protocol-scope.md)'s protocol-ID table (lines 14-25) has
  J1850VPW and J1850PWM's numeric IDs swapped relative to J2534-1 Figure 8
  (`vehicle-comm-specs/j2534-1-0404/J2534_1_200412...md:473-474`: VPW=0x01, PWM=0x02) —
  `j2534-0404-sys/src/bindings/j2534_v0404.h:104-105` has the correct values; only ADR-017's prose
  table is inverted. ADR-017 itself is superseded (by ADR-152), but remains the only in-repo prose
  tabulation of these IDs — a future reader cross-checking the header against it risks "fixing" the
  header into a real interop break. Trivial one-line correction whenever that ADR is next touched.
- P3 (J2534-1 audit, Connect/Protocol/IOCTL area, near-inert): `five_baud_init`
  (`j2534-0404/src/lib.rs:830-854`) always returns a fixed `[u8; 2]` for the 5-baud-init key bytes,
  never reading back the native call's output `NumOfBytes` (J2534-1 Figure 33,
  `vehicle-comm-specs/j2534-1-0404/J2534_1_200412...md:1327` — an OUTPUT field, the actual byte
  count returned, ≤ 2). A DLL reporting only 1 valid key byte yields a fabricated `KB2 = 0x00`
  indistinguishable from a real one. Near-inert today since every FIVE_BAUD_MOD variant
  (`:1263`) yields 2 key bytes in practice. Not investigated or scoped here.
- P3 (J2534-1 audit, SConfig/ComParam mapping area, same class as the `CP_BlockSizeOverride`
  range-check fix and the W/T-group timing-ComParam 16-bit clamp fix, both now fixed and out of
  this backlog, smaller blast radius): the remaining bounded Figure-30 params (`DATA_RATE` 5-500000, `LOOPBACK` 0/1,
  `NETWORK_LINE` 0-2, `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` 0-100, `ISO15765_BS`/`STMIN` 0-0xFF,
  `FIVE_BAUD_MOD` 0-3 — `vehicle-comm-specs/j2534-1-0404/J2534_1_200412...md:1228,1230,1232,1251-1252,1258-1259,1263`)
  are all identity-forwarded with no range validation, inconsistent with this codebase's own
  established pattern of rejecting unrepresentable values at `SetComParam` time (e.g. `PARITY` at
  `rpc_link.rs:5949`). Deliberately excludes `ISO15765_WFT_MAX`, whose `[0,1027]`-vs-8-bit range gap
  is already tracked above (A2-11 deferral) — this item is the same class extended to the other
  bounded rows; ride along whenever this area is next touched.
- P3 (J2534-1 audit, TX/RX/filters area, error-quality only, no silent corruption): client filter
  shape rules (J2534-1 `:737` — mask/pattern >12 bytes is invalid; `:743` — a non-ISO15765 filter's
  mask/pattern must share `DataSize`/`TxFlags`) aren't pre-validated by
  `install_client_message_filters` (`rpc_link.rs:198-254`, which does enforce equal TxFlags but
  forwards client-supplied mask/pattern lengths unchecked) — a violating filter is rejected by the
  native device with a generic error instead of a precise D-PDU `PDU_ERR_INVALID_PARAMETERS`.
  Behaviorally conformant (the DLL enforces it); not investigated or scoped here.
- P3 (J2534-1 audit, TX/RX/filters area, doc note, not a fix): J2534-1 (`:737`) guarantees only ten
  filters per protocol on a minimally-conforming adapter. `SetUniqueRespIdTable` has no entry cap,
  and connect-time filter installation can install up to two filters per table entry (the ADR-041
  UUDT-workaround path, `rpc_link.rs:3623-3725`) — a large table can hit `PDU_ERR_EXCEEDED_LIMIT` at
  connect time on such hardware. Surfaces loudly (not silent); worth a doc note next time this area
  is touched, not a dedicated fix.

## SAE J2534-2 (DEC2020) already-implemented-phases conformance audit (2026-08-11)

The findings below are this subsystem's first conformance audit of its already-implemented SAE
J2534-2 phases (Discovery, Pin Selection, Additional Channels, CAN FD Core, ISO15765-on-CAN-FD,
Mixed-Format CAN, Single Wire CAN, Fault-Tolerant CAN, Repeat Messaging, Extended Programming
Voltage) independent of each phase's own implementation-PR review. Each phase already went through
Codex/edge-case-hunter/design-advisor review during its own PR — this pass instead re-read the raw
J2534-2 clause text end-to-end per feature, the way the ISO 22900-2 audit was originally done, since
narrowly-scoped bug-fix PR review is structurally unlikely to catch a misread baked into a feature's
foundational design. Four parallel design-advisor passes ran, clustered as: Discovery/Pin
Selection/Additional Channels; the CAN FD family (Core/ISO15765-on-FD/Mixed-Format); Single Wire
CAN/Fault-Tolerant CAN; Repeat Messaging/Extended Programming Voltage.

- P2 (J2534-2 audit, Repeat Messaging area) — **mock-side half FIXED by ADR-173**, both
  discovery-wiring gaps **FIXED** (below); service-side residual remains open: clause 14.2.2.2 Table 53
  (`vehicle-comm-specs/j2534-2-0404/J2534-2_202012 - Optional Pass-Thru Features.md:1841-1847`)
  defines QUERY status as 1 while transmission is in progress, 0 once the stop condition fires and
  transmission has ceased; clause 14.2.2.3 (`:1865`) states the `MsgId` is never auto-released on
  self-termination — an explicit STOP is always required to release it. `j2534-0404-mock`'s worker
  used to delete the slot outright on self-termination, so a subsequent QUERY/STOP wrongly returned
  `ERR_INVALID_MSG_ID` instead of `STATUS_NOERROR` + status 0, and for a still-live slot the mock
  reported status 0 unconditionally — the polarity was backwards per Table 53. ADR-173's state-
  machine rewrite (needed anyway for [ADR-173](../../../docs/adr/ADR-173-repeat-messaging-condition-semantics-correction.md)'s
  own `Condition == 0`/`Condition == 1` stop-condition-semantics correction, since both touch the
  same mock slot lifecycle) folded this fix in: a terminated slot is now retained until an explicit
  `STOP_REPEAT_MESSAGE`, and `QUERY_REPEAT_MESSAGE` reports 1 (live) / 0 (terminated-but-valid) per
  Table 53's actual polarity — the mock-side half of this P2 is closed. **Remaining service-side
  residual:** `prune_stale_repeat_message_ids` (`rpc_misc.rs`, used by the `LOCK_PHYSICAL_TX_QUEUE`
  grant check) was updated alongside the mock fix to stop treating a status-0 (terminated-but-valid)
  `MsgId` as still blocking a lock grant, while continuing to retain it in the tracked id set (not
  pruning it, since the claim stays valid until STOP) — this closes the over-blocking half of the
  original finding. Any further service-side implications of the Table-53 status semantics beyond
  this are not otherwise known to be open; re-audit if a future finding surfaces one. Separately,
  the two discovery-wiring gaps flagged alongside this residual are now **FIXED**: (a) Repeat
  Messaging discovery (clause 14.3, `:1869-1871`, and 25.3.2.3, `:3572-3573` —
  `MAX_REPEAT_MESSAGING(_LENGTH)`, 0 meaning unsupported) was never wired — the constants exist
  (`j2534-0404-sys/src/bindings/j2534_v0404.h:2875-2876`) but no `GET_PROTOCOL_INFO` arm reported
  them in the mock, so a discovery-first client concluded Repeat Messaging was unavailable and
  never called the already-implemented IOCTLs (the same bug class already fixed for clause 23's
  discovery, PR #48 round 1). Fixed by adding `PROTOCOL_INFO_MAX_REPEAT_MESSAGING` /
  `_MAX_REPEAT_MESSAGING_LENGTH` arms to the mock's `IOCTL_GET_PROTOCOL_INFO` handler, both
  `Supported = 1`; the former reports `MAX_REPEAT_SLOTS_PER_CHANNEL` (10). The latter originally
  reported a flat 4128, later superseded by [ADR-186](../../../docs/adr/ADR-186-repeat-message-periodic-datasize-cap.md)'s
  SAE J2534-1 v04.04 §7.2.7 flat periodic-message cap: `j2534-0404-mock`'s
  `max_repeat_messaging_length` helper (`j2534-0404-mock/src/lib.rs`) now reports, and
  `IOCTL_START_REPEAT_MESSAGE` enforces, 12 bytes for most protocols (CAN, J1850VPW, ISO9141,
  ISO14230/KWP, the four SCI variants, J1939, UART Echo Byte, Honda DIAG-H, J1708), 11 for
  ISO15765 (clause 22.2.2(h)), and 10 for J1850PWM (its own ordinary TX size range, `3..=10`,
  is narrower than the flat 12-byte cap). (b) Extended Programming Voltage discovery (clause 15.4,
  `:1915-1917`, and 25.3.2.2, `:3486-3487` — per-pin `SHORT_TO_GND_J1962`/`PGM_VOLTAGE_J1962`) had
  the identical gap — the constants exist (`j2534_v0404.h:2804-2805`) but the mock's
  `GET_DEVICE_INFO` never reported them, so discovery denied pin 9/15 support while
  `PassThruSetProgrammingVoltage` itself accepted them. Fixed by adding
  `DEVICE_INFO_SHORT_TO_GND_J1962` / `DEVICE_INFO_PGM_VOLTAGE_J1962` arms to the mock's
  `IOCTL_GET_DEVICE_INFO` handler; per Table 53's sibling per-pin format (clause 25.3.2, `Value` is
  an input bit-mapped `0xHHHHLLLL` unsigned long that must remain un-altered), `SHORT_TO_GND_J1962`
  reads the pin selector from the low 16 bits (`Value & 0xFFFF`) while `PGM_VOLTAGE_J1962` reads it
  from the high 16 bits (`(Value >> 16) & 0xFFFF`) — the two constants use different nibble-pair
  encodings, confirmed against the spec text directly rather than assumed from the
  `READ_J1962PIN_VOLTAGE_SUPPORTED` precedent (which was flat, not per-pin, at the time of this
  audit — later corrected to be per-pin, low-half convention, by the ADR-185 Stage 2 fix noted
  below; deliberately not used as a template here regardless, since it was still flat when this
  audit ran). Both arms answer
  `Supported` for pins 9/15 only and leave `p.Value` un-altered, matching the spec's requirement.
  New tests in `j2534-0404-service/src/service/discovery.rs`:
  `protocol_info_reports_max_repeat_messaging`, `protocol_info_reports_max_repeat_messaging_length`,
  `device_info_reports_supported_for_short_to_gnd_j1962_pin_9`,
  `device_info_reports_supported_for_short_to_gnd_j1962_pin_15`,
  `device_info_reports_not_supported_for_short_to_gnd_j1962_other_pin`,
  `device_info_reports_supported_for_pgm_voltage_j1962_pin_9`,
  `device_info_reports_not_supported_for_pgm_voltage_j1962_other_pin` (the latter five via a new
  `raw_device_info_query` test helper that bypasses `discovery_device_info()`'s cache, since the
  cache always sends `value: 0` and cannot express a per-pin query input). **Note:** the new
  discovery answers restrict `Supported` to pins 9/15 per SAE J2534-2 Table 111, but the mock's
  `PassThruSetProgrammingVoltage` implementation itself performs no per-pin gating at all (any pin
  succeeds) — the existing "SAE J2534-2 Extended Programming Voltage" (Phase 13) section above
  already documents this lack of runtime pin validation as an intentional, pre-existing choice
  (clause 15 needed no new pin validation at the service layer); this discovery fix is a
  one-directional tightening (discovery says NOT_SUPPORTED for pins the operation would actually
  accept), never the reverse, so it introduces no new client-facing correctness gap, but a future
  test asserting "discovery says unsupported, therefore the operation should fail" for a pin other
  than 9/15 would be wrong against the current mock and is not written here.
- P2 (new, ADR-173 fix, functional-addressing interaction discovered during implementation):
  `tx_header::response_header_bytes` already rejected functional (broadcast) addressing for CAN/
  ISO15765 links pre-ADR-173 — Repeat Messaging has no single expected responder to scope a
  stop-condition template to, so a `Condition == 1` `START_REPEAT_MESSAGE` on a functionally-
  addressed link was already rejected before this fix. ADR-173's carve-out reversal (response-
  header resolution now unconditional, Decision 4) extends this same, already-existing rejection to
  `Condition == 0` as well — functionally-addressed links can now never start a repeat slot under
  either condition. This is the same category of "no resolvable response header" behavior change
  ADR-173's Consequences already document as intentional (not a new design question), just worth
  naming explicitly here since it was not previously written down anywhere. Not fixed here (nothing
  to fix — this is the spec-correct outcome of clause 14 having no functional/broadcast stop-
  condition concept, mirroring the existing explicit rejection this codebase already has for
  ISO9141/ISO14230 functional addressing, `tx_header.rs`).
- P3 (J2534-2 audit, Repeat Messaging area) — **FIXED (ADR-185 Stage 2, same PR as the lock-order
  fix)**: `READ_J1962PIN_VOLTAGE_SUPPORTED` discovery
  (`vehicle-comm-specs/j2534-2-0404/J2534-2_202012 - Optional Pass-Thru Features.md:3516` — `Value`
  is an input pin bitmap that must be left unaltered, `Supported` answers for the specific queried
  pin) used to unconditionally overwrite `p.Value = 1` and report `Supported = 1` regardless of
  which pin bit was actually queried (`j2534-0404-mock/src/lib.rs`, from a prior PR #48 fix) —
  including pins 4/5, which the same mock's runtime IOCTL separately rejects with
  `ERR_PIN_INVALID`, so discovery and runtime answers contradicted each other for those pins. Fixed
  by making the dispatch arm decode the same low-half single-pin bitmap `SHORT_TO_GND_J1962`'s own
  arm already uses and report `Supported = 0` for the same pins (0/4/5/17+) the real IOCTL rejects.
  See this file's ADR-185 section ("Mock-fidelity fix" paragraph) for detail and new test coverage.
- P3 (J2534-2 audit, Repeat Messaging area): `TimeInterval`'s spec-mandated 5-65535 ms range
  (`vehicle-comm-specs/j2534-2-0404/J2534-2_202012 - Optional Pass-Thru Features.md:1757`) is
  unvalidated end-to-end — the service forwards `setup.time_interval` verbatim (`rpc_misc.rs:1900`,
  no range check in the handler) and the mock only clamps to `.max(1)` (`lib.rs:631`), so a 0 ms
  interval is silently accepted where a conforming device would reject it. Low priority since a
  real DLL enforces this; the mock stands in for the device here and doesn't. Not fixed here.
- P3 (J2534-2 audit, SWCAN area): `comparam_defaults.rs:121-131`'s `sae_j2411_swcan()` seeds no
  `CP_ChangeSpeedCtrl`/`CP_ChangeSpeedRate`/`CP_ChangeSpeedResCtrl` values, so a D-PDU client reading
  `CP_ChangeSpeedRate` before ever setting it sees `0` while a `PDU_IOCTL_SW_CAN_HS` call would
  actually transition the bus to the spec's own 83,333 bps default (Table 9,
  `vehicle-comm-specs/j2534-2-0404/J2534-2_202012 - Optional Pass-Thru Features.md:638`). Wire
  behavior stays conformant (the device's own default governs), so this is a reporting-only gap.
  Related minor items found in the same pass, not separately tracked: Table 13's note (`:698`) that
  simultaneous high-voltage-transmit + high-speed mode is undefined is unguarded (informational —
  the spec doesn't mandate rejection); pre-connect `PDU_IOCTL_SW_CAN_HS`/`_NS` silently succeeds
  with no native call and no channel (`rpc_misc.rs:1119-1124`), documented only in a code comment,
  unlike the already-explicitly-documented `ref_count > 1` no-op; ADR-164 Decision 3's prose says
  `PDU_ERR_FUNCTION_NOT_SUPPORTED` for non-SW-link rejection where the code and
  implementation notes both actually say `PDU_ERR_ID_NOT_SUPPORTED` (`rpc_misc.rs:1114`) — cosmetic
  ADR-text drift. Not fixed here.
- **P3** (J2534-2 audit, ADR-160 mixed-format CAN, accepted residual, edge-case-hunter
  post-round-2-fix audit): the original clause-6.3.2.7 `SET_CONFIG` ordering bug (`connect_new_
  physical_channel` issuing `SET_CONFIG(CONFIG_CAN_MIXED_FORMAT)` before `SET_CONFIG(CONFIG_J1962_
  PINS)` on a pin-selected `ISO15765_PS` channel, now fixed and out of this backlog) is now
  structurally untestable, not merely untested. `CONFIG_J1962_PINS` only fires when
  `link_pin_select.is_some()`, and `CONFIG_CAN_MIXED_FORMAT` only fires when
  `link_pin_select.is_none() && link_channel_index.is_none()` — mutually exclusive by construction,
  so the two `SET_CONFIG` calls can never both fire on the same connect and nothing can exercise
  their relative order anymore. Not a live risk today (`connect_new_physical_channel` is a private
  method with exactly one caller, `rpc_connect_com_logical_link`, which always derives both booleans
  the same way), but the method's own signature carries no guarantee against a future caller passing
  `pin_select: Some(_)` together with `native_mixed_format: Some(_)` (the field's `Option<u32>`
  shape as of ADR-217; a non-`None` value, `CAN_MIXED_FORMAT_ON` or `_ALL_FRAMES`, plays the role
  the old `true` did) — at that point the currently-correct pins-before-mixed-format block order
  matters again, with zero test coverage to catch a regression. Not fixed here; revisit if
  `connect_new_physical_channel` ever grows a second caller or its parameter contract changes.
- P3 (J2534-2 audit, CAN FD family area): `CP_CANFDBaudrate`'s valid-value set differs by protocol
  in the spec — Table 90 (`vehicle-comm-specs/j2534-2-0404/J2534-2_202012 - Optional Pass-Thru
  Features.md:2848`) allows 125,000 for `FD_CAN_PS`, Table 97 (`:3045`) starts at 250,000 for
  `FD_ISO15765_PS`, and both require the data-phase rate not fall below the arbitration rate — none
  of this is validated (`rpc_link.rs:6013-6021` validates only `CP_CANFDTxMaxDataLength`;
  `CP_CANFDBaudrate` accepts any u32 and forwards verbatim). [ADR-159](../../../docs/adr/ADR-159-j2534-2-iso15765-on-can-fd.md#L47)
  calls this parameter "already implemented by ADR-158, reused unchanged," missing that Table 97
  narrows the valid set relative to Table 90. Concrete scenario: an ISO15765 link at
  `CP_Baudrate = 125000` (a legitimate classic arbitration rate) staging only
  `CP_CANFDTxMaxDataLength = 64` triggers FD substitution and forwards `CP_CANFDBaudrate = 125000`
  to `FD_CAN_DATA_PHASE_RATE` — a conformant adapter rejects it, and the connect fails with an
  opaque native error instead of a service-level explanation. Loud/device-enforced, hence P3 not
  P2. Not fixed here.
- P3 (J2534-2 audit, CAN FD family area, undocumented API-impedance limit, not a code defect): CAN
  2.0-format transmission is unreachable on any FD-substituted link — `TX_FD_CAN_FORMAT` is set
  unconditionally on every message an FD link sends (`rpc_primitive.rs:366-367,763`), because ISO
  22900-2 has no per-message format flag to select CAN 2.0 vs. FD framing, even though J2534-2
  Tables 93/100 (`vehicle-comm-specs/j2534-2-0404/J2534-2_202012 - Optional Pass-Thru
  Features.md:2921,3124`) and clauses 21.2.2.d/22.2.2.a (`:2743,2945`) expect devices to handle
  mixed-format traffic on one channel — the normal FD deployment shape. Not a service bug (an API
  ceiling), but neither ADR-158's round-4 correction nor ADR-159 Decision 3 records this as an
  accepted residual. Should be documented as one next time either ADR is touched, not fixed in
  code.
- P3 (J2534-2 audit, CAN FD family area, doc-only, latent): `resources.rs:1080-1090`'s
  `mixed_format_can_protocol_id` fallback doc comment claims the `SW_ISO15765_PS`/other-qualified
  case is "unreachable given that gate," but the actual connect-time gate passes `SW_ISO15765_PS`
  through (`base_protocol_id` maps it to `ISO15765`, `resources.rs:887`) — the real protection is
  the separate `qualified` disqualifier at the filter-install call sites, not the gate the comment
  names. Inert today; would go live immediately if the CAN-FD-family P2 above is ever fixed in the
  "extend PASS_FILTER to qualified links" direction. Fix the doc comment now; add SW/FT arms only
  if that direction is chosen.
- P3 (J2534-2 audit, CAN FD family area, cosmetic): `rpc_link.rs:6011-6012`'s comment for
  `CP_CANFDTxMaxDataLength` cites "SAE J2534-2 Table 90/97" for its valid DLC encodings; Table 90
  (`vehicle-comm-specs/j2534-2-0404/J2534-2_202012 - Optional Pass-Thru Features.md:2844-2849`) has
  no such row — the actual sources are Table 91's note (`:2873`) and Table 97's
  `FD_ISO15765_TX_DATA_LENGTH` row (`:3046`). Wrong citation, not wrong behavior.
- P3 (J2534-2 audit, Discovery/Pin Selection/Additional Channels area, doc-only): three prose
  inaccuracies found while re-reading the raw spec against the ADRs, none affecting code
  correctness — (1) [ADR-153](../../../docs/adr/ADR-153-j2534-2-discovery-mechanism-phase1.md#L159-L160)
  paraphrases clause 5's non-prefixed-`pName` rule backwards (it describes the NULL-`pName` case's
  rule, not the non-NULL-non-prefixed case's — the implementation is conservative and safe under
  either reading, so no code change); (2)
  [ADR-156](../../../docs/adr/ADR-156-j2534-2-pin-selection-additional-channels-phase2.md#L107-L109)'s
  prose lists six protocols while calling them "the seven Table 1 protocols" (Table 1,
  `vehicle-comm-specs/j2534-2-0404/J2534-2_202012 - Optional Pass-Thru Features.md:244-254`, has
  seven entries including `J2610_PS`, which the code and `implementation-notes.md` both already
  handle correctly — just dropped from this one ADR's enumeration); (3) the mock's
  `GET_PROTOCOL_INFO` (`j2534-0404-mock/src/lib.rs:2056-2077`) normalizes a queried `_PS` protocol
  id to its base family before answering, collapsing Table 113/114's distinct `_PS` protocol groups
  (`:3535-3590`) onto the base-group answer — no longer inert as of ADR-185 Stage 2:
  `ioctl_start_repeat_message`'s `ProtocolCapacity` precheck (`rpc_misc.rs`) is a real production
  caller of `discovery_protocol_info_with_open_device`/`GET_PROTOCOL_INFO`, and passes the link's
  raw `hw_protocol_id` unmodified, which can be a `_PS`-substituted id on an FD-connected link — so
  this normalization gap is reachable today, not merely a future-phase concern. Still not fixed
  here; the mock's collapsed answer happens to match every currently-implemented FD family's real
  `PROTOCOL_INFO_MAX_REPEAT_MESSAGING` value, so no test currently observes the gap.
- **P3** (ADR-216, accepted residual, explicitly not fixed by that ADR): `LOOPBACK` remains allowed
  on a UART Echo Byte link via the existing `is_universal_param` check (ADR-170 Decision 3) even
  though clause 12.3.4.1's own Table 36 closing text names only `DATA_RATE`/`J1962_PINS` among the
  standard J2534-1 parameters as supported for this protocol — a pre-existing allowlist looseness
  ADR-216's own UEB timing-parameter addition sits beside but does not itself introduce or widen.
  Correcting it is a narrower, independent allowlist question (removing `LOOPBACK` from
  `UART_ECHO_BYTE_PS`'s effective allowlist without disturbing the universal `DATA_RATE`/`LOOPBACK`
  pair every other protocol relies on) outside ADR-216's own scope.
