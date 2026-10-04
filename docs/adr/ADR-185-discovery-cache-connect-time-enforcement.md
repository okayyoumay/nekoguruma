# ADR-185: Discovery-Cache Connect-Time/IOCTL-Time Capability Enforcement — Shared Mechanism, Two-Stage Rollout

**Date:** 2026-08-22
**Status:** Accepted
**Affects:** `j2534-0404-service` service/discovery, service/resources, service/rpc_link, service/rpc_misc, `j2534-0404-mock`

## Context

ADR-152 Decision 1 ("Advertising policy") already settled that resource
*advertising* stays static (gated only by the module's `pname` `"J2534-2:"`
opt-in, clause 5), while actual device *capability* is meant to be "enforced
at `ConnectComLogicalLink` time — refined by a cached `GET_DEVICE_INFO`/
`GET_PROTOCOL_INFO` result once Phase 1 exists and the device is open." That
ADR's own Consequences section documents the resulting accepted residual
precisely: a caller can see an advertised resource whose connect
subsequently fails on a less-capable device; this "surfaces as a mapped
`PDU_ERR_*` at `ConnectComLogicalLink`, refined by cached Discovery data
where available" — Discovery-based fail-fast was always meant to be an
*enhancement* layered on top of the native-call fallback, never a
replacement for it.

In practice, "refined by cached Discovery data" has been wired up exactly
once: `J2534Service::check_chx_capacity` (`discovery.rs`, ADR-156 Decision
4/Phase 2b), which checks a connecting SAE J2534-2 clause 7 Additional
Channel (`_CHx`) index against the device's advertised channel count via
`resources::chx_device_info_supported_parameter` — a small table covering
only the six original J2534-1 base protocols. Every J2534-2 phase shipped
since Phase 2b — SWCAN/FT-CAN (clause 9/20), UART Echo Byte/Honda DIAG-H/
J1708 (clauses 12/13/17), Repeat Messaging (clause 14), Extended Programming
Voltage/J1962 Pin Voltage Read (clause 15/23), Device Configuration
(clause 18), Analog Inputs (clause 10, Phase 15/ADR-177) — connects or
dispatches its own resource/IOCTL with no equivalent check, reaching the
native call directly. Not a correctness gap (every native failure path is
already exercised and mapped to the right `PduError`) — a missed fail-fast
opportunity that recurred across nine phases without ever being picked up,
tracked as a P2 backlog item until this ADR (`j2534-0404-service/docs/
implementation-notes.md`'s Prioritized Backlog, now closed).

`discovery.rs`'s own module doc comment already flags a landmine bearing on
any general mechanism: the Discovery cache key (`(module_handle,
parameter)`) and `discovery_device_info_with_open_device`'s native query
both hardcode the query's `value` input to `0`, correct for every flag/
capacity `DEVICE_INFO_*` parameter checked so far but not for clause
15/25.3.2.2's two per-pin parameters (`DEVICE_INFO_SHORT_TO_GND_J1962`/
`DEVICE_INFO_PGM_VOLTAGE_J1962`), which take a caller-supplied pin-selector
bitmap as genuine input. Extended Programming Voltage (Phase 13) — one of
the nine phases this ADR wires up — is exactly the case that landmine
warns about.

This ADR was written following a `design-advisor` consult (2026-08-22);
its analysis and per-phase parameter mapping (§ below) are incorporated
directly.

## Decision

**1. One shared enforcement primitive, `discovery.rs`.** A new `DiscoveryCheck`
enum describes what a call site needs verified:

- `DeviceFlag { parameter: u32, input_value: u32 }` — `GET_DEVICE_INFO` must
  report `supported == true`. `input_value` is `0` for the six flag-shaped
  phases below, a pin-selector bitmap for the clause 15 per-pin parameters.
- `DeviceCapacity { parameter: u32, extract: fn(u32) -> u32, needed: u32 }`
  — `GET_DEVICE_INFO` must report `supported == true` AND
  `extract(result.value) >= needed` (mirrors `check_chx_capacity`'s own
  `(value >> 16) & 0xFF` unpacking shape for a numeric capacity, not a
  boolean).
- `ProtocolCapacity { protocol_id: u32, parameter: u32, needed: u32 }` —
  `GET_PROTOCOL_INFO` must report the equivalent for a protocol-scoped
  numeric limit.

A single `enforce_discovery_capability(&self, module_handle, device:
DeviceAccess, check: DiscoveryCheck, operation: &str, reject_as: PduError,
last_error: Option<...>) -> Result<(), Status>` consumes one of these.
`DeviceAccess` is `AlreadyOpen(DeviceId)` (caller already holds
`device_guard`; funnels through `discovery_device_info_with_open_device`,
`check_chx_capacity`'s own discipline, avoiding the non-reentrant
`self.device_id` deadlock the module doc comment already documents) or
`OpenIfNeeded` (funnels through `discovery_device_info`/
`discovery_protocol_info`, which open the device themselves). Encoding this
in the signature prevents the deadlock class by construction rather than by
caller discipline alone.

**Fail-fast fires only on a definitive negative** (`supported == false`, or
a capacity check that resolves and falls short). Not-opted-in (`Ok(None)`
from the underlying Discovery call), no table mapping for this call site,
and any Discovery-query error all fall through as a no-op — the native call
downstream remains the authority, exactly as ADR-152 Decision 1 and ADR-153
Decision 1 already require. `check_chx_capacity` itself is unchanged by
this ADR; it is the existing precedent this mechanism generalizes, not a
site migrated onto it.

**2. Two per-shape mapping tables, not one.** The connect path and the
IOCTL dispatch path have genuinely different keys — a connect is
protocol-id-driven, an IOCTL is command-id-driven — so forcing one shared
key type would be artificial:

- `resources::connect_discovery_check(j2534_proto_id: u32) ->
  Option<DiscoveryCheck>` — consulted from `rpc_link.rs`'s
  brand-new-physical-channel connect path, immediately adjacent to the
  existing `check_chx_capacity` call. **Keyed on the raw, post-substitution
  `j2534_proto_id`, not `base_proto_id`**: `resources.rs` already
  normalizes `PROTOCOL_SW_CAN_PS`/`PROTOCOL_FT_CAN_PS` both down to `CAN`
  for other purposes, and a base-keyed lookup here would check
  `DEVICE_INFO_CAN_SUPPORTED` and never distinguish a CAN-capable-but-
  no-SWCAN device from a genuinely SWCAN-capable one. Joining an existing
  shared channel skips the check, same reasoning `check_chx_capacity`
  already applies (capability already proven by the channel's own
  creation).
- Four IOCTL handlers each call `enforce_discovery_capability` at their own
  top, after their own argument parsing and device acquisition —
  `ioctl_read_j1962_pin_voltage`, `ioctl_get_device_config`/
  `ioctl_set_device_config`, `ioctl_set_prog_voltage` (pin-9
  short-to-ground case only — `prog_voltage_mv == SHORT_TO_GROUND`
  specifically, not every pin-9 call: a real millivolt value and the
  `VOLTAGE_OFF` sentinel are both pin-9 `PassThruSetProgrammingVoltage`
  calls too, per SAE J2534-1 v04.04 §7.2.11.3's "Voltage Values" table, but
  get no Discovery precheck at all, deliberately — see the
  `j2534-0404-service/docs/implementation-notes.md` Prioritized Backlog
  entry this scoping decision left open, `DEVICE_INFO_PGM_VOLTAGE_J1962`
  is never wired), `ioctl_start_repeat_message`. A blanket hook
  inside `rpc_misc.rs`'s IOCTL dispatcher itself was considered and
  rejected: the dispatcher has neither a device guard nor the parsed input
  (a per-pin check specifically needs the pin number out of `input_data`)
  at that point. "One general enforcement point" means the shared
  primitive and its tables, not one textual call site.

**3. Per-phase parameter mapping** (verified against
`j2534-0404-sys/src/bindings/j2534_v0404.h`):

| Phase (clause) | Shape | Parameter(s) | Site |
|---|---|---|---|
| SWCAN (9) | `DeviceFlag` | `DEVICE_INFO_SW_CAN_SUPPORTED` / `DEVICE_INFO_SW_ISO15765_SUPPORTED` | connect |
| FT-CAN (20) | `DeviceFlag` | `DEVICE_INFO_FT_CAN_SUPPORTED` / `DEVICE_INFO_FT_ISO15765_SUPPORTED` | connect |
| UART Echo Byte (12) | `DeviceFlag` | `DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED` | connect |
| Honda DIAG-H (13) | `DeviceFlag` | `DEVICE_INFO_HONDA_DIAGH_SUPPORTED` | connect |
| J1708 (17) | `DeviceFlag` | `DEVICE_INFO_J1708_SUPPORTED` | connect |
| Analog Inputs (10) | `DeviceFlag` | `DEVICE_INFO_ANALOG_IN_SUPPORTED` | connect |
| Repeat Messaging (14) | `ProtocolCapacity` | `PROTOCOL_INFO_MAX_REPEAT_MESSAGING` (`needed` = this channel's live slot count + 1) | IOCTL |
| Device Configuration (18) | `DeviceCapacity` | `DEVICE_INFO_MAX_NON_VOLATILE_STORAGE` (`needed = 1`) | IOCTL |
| Extended Programming Voltage / J1962 Pin Voltage Read (15/23) | `DeviceFlag` (per-pin, all three parameters — see "Correction" note below) | `DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED`, `DEVICE_INFO_PGM_VOLTAGE_J1962`, `DEVICE_INFO_SHORT_TO_GND_J1962` (each `input_value` = the pin bitmap) | IOCTL |

All nine phases have a corresponding Discovery parameter — none is
Discovery-blind, which is what justifies building the general mechanism
rather than treating each phase as its own dead end. The same table shape
extends later, as a one-line addition each, to protocol/feature areas not
yet wired (CAN FD, J1939, TP2.0, GM UART, and `_CHx` counts for families
beyond the original six) — deliberately out of scope here, not silently
forgotten (see Consequences).

**4. The per-pin landmine is closed now, not deferred.** The Discovery
cache key widens to `(module_handle, parameter, input_value)`, and
`input_value` threads through `discovery_device_info`/
`discovery_device_info_with_open_device` in place of the hardcoded `0`. All
existing callers (`check_chx_capacity`, this ADR's own six `DeviceFlag`
connect-path rows) pass `0`, so this is a mechanical, behavior-preserving
widening for them. `discovery.rs`'s existing `raw_device_info_query`
test-only helper — added specifically to bypass the old hardcoded-`0`
limitation and exercise real per-pin behavior — becomes redundant once
production code can express the same query, and the module doc comment's
landmine warning is removed in the same change. Deferring this would ship
the general mechanism with a documented hole at exactly the parameter class
Phase 13 (in scope this same ADR) needs.

**5. Error behavior: reuse the native-equivalent `PduError`.** A
Discovery-driven rejection uses whatever `PduError` the native call would
eventually have produced for the same unsupported capability (e.g.
`PduErrIdNotSupported`, the code already used for statically-unsupported
IOCTLs), emitted via `state_guard_status` with Discovery-provenance message
text distinguishing it from the native path for diagnostic purposes only.
This is what ADR-152's Consequences residual already promises: the
client-visible error *class* must not depend on whether the rejection
happened early (Discovery) or late (native) — only latency and message
text differ. `check_chx_capacity`'s own `Status::invalid_argument` is not
the model for a capability-absence rejection (it is a caller-argument
index-out-of-range error, a different failure shape) and is unaffected by
this rule; a `DeviceCapacity`/`ProtocolCapacity` shortfall (e.g. Repeat
Messaging's slot count) may reuse it where the native equivalent is itself
an argument-range rejection.

This class-equivalence guarantee is made at the `PduError` level (the ISO
22900-2 semantic error code carried in the status details), not the outer
gRPC `Code`: every Discovery-driven rejection goes through
`enforce_discovery_capability`'s uniform `state_guard_status`/
`Code::FailedPrecondition` path regardless of what the equivalent native
failure's own mapping would have produced (e.g. `map_native_error_as`'s
blanket `Code::Internal` for every `PduError` but `PduErrInvalidHandle`,
`error.rs`) — this was already true for every Stage-1 `DeviceFlag`
connect-time check, not something Stage 2 introduces, but a Stage 2 test
(`repeat_message.rs`'s exceeded-limit integration test, and
`pdu_ioctl.rs`'s `read_j1962_pin_voltage_pin_4_is_rejected_by_the_discovery_
fail_fast_path`) makes the gap concretely observable for the first time.

**6. Two-PR rollout, not one PR per phase.** A phase's mapping is a table
row, not a phase's own body of work, so nine separate PRs would be
disproportionate; one PR mixing two structurally different call shapes
plus the cache-key widening would be a large, hard-to-review diff. This ADR
covers both stages; Stage 1 (this PR) delivers the mechanism, the cache-key
widening, and the connect-shape wiring (all six `DeviceFlag` connect rows,
one call site, one mock-backed test sweep per no-op/fail-fast pair). Stage
2 (a follow-up PR) delivers the four IOCTL-shape call sites, mirroring the
sub-stage precedent already established for Phase 2 (2a/2b) and Phase 3
(3a/3b/3c) under ADR-155.

## Consequences

- Nine phases gain a fail-fast layer with zero new client-visible surface —
  no new RPC, no new proto field, consistent with ADR-152 Decision 2.
- The per-pin landmine and its module-doc-comment warning are retired in
  Stage 1; `raw_device_info_query`'s bypass becomes unnecessary (left in
  place as a test-only convenience unless Stage 1's implementation finds
  reason to remove it).
- **Deliberately out of scope, enumerated so this is a decision and not an
  omission:** CAN FD, SAE J1939, TP2.0, GM UART, and `_CHx` counts for any
  protocol family beyond the original six `check_chx_capacity` already
  covers. Each is a one-row addition to the relevant table when that phase
  is next touched, not a structural gap in this mechanism.
- **Accepted residual — under-reporting risk.** A device that advertises a
  capability more conservatively than what it actually accepts turns a
  connect/IOCTL that would have succeeded natively into an earlier
  rejection. Bounded by construction: the mechanism only rejects on a
  definitive negative, never on an ambiguous or missing answer, and the
  native call remains reachable (and authoritative) for every resource this
  ADR does not wire up.
- Stage 2 (IOCTL-shape wiring: `ioctl_read_j1962_pin_voltage`,
  `ioctl_get_device_config`/`ioctl_set_device_config`,
  `ioctl_set_prog_voltage`'s pin-9 case, `ioctl_start_repeat_message`) is
  implemented, in the same PR as this Correction note. It also implements
  `DiscoveryCheck::ProtocolCapacity` for real via `DeviceAccess::AlreadyOpen`
  (see "Correction (Stage 2 lock-order fix)" below for why this ended up
  `AlreadyOpen` rather than the originally-planned `OpenIfNeeded`) and gives
  `discovery_protocol_info_with_open_device` its first production caller
  (`ioctl_start_repeat_message`). `DeviceAccess::OpenIfNeeded` and
  `discovery_protocol_info` remain fully implemented and unit-tested but have
  no production caller. Two residuals found while scoping Decision 3's
  table remain unwired by either stage — each Stage-1-wired family's
  `_SIMULTANEOUS` companion bit, and SAE J1708's clause-6
  `DEVICE_INFO_J1708_PS_J1962`/`_J1939`/`_J1708` connector-validity queries —
  tracked in `j2534-0404-service/docs/implementation-notes.md`'s Prioritized
  Backlog.

## Correction (Stage 2 implementation)

Decision 3's table row for Extended Programming Voltage/J1962 Pin Voltage
Read labeled `DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED` as `DeviceFlag`
"(flat)" — implying `input_value: 0`. Verifying against the primary spec
text during Stage 2 implementation
(`vehicle-comm-specs/j2534-2-0404/J2534-2_202012 - Optional Pass-Thru
Features.md`, Table 111, clause 25.3.2.2) found this wrong: its `Value`
field is a bit-mapped pin selector too, invalid to set more than one bit —
but it uses the same LOW-half bit convention as `SHORT_TO_GND_J1962` (bit 0
= pin 1), not `PGM_VOLTAGE_J1962`'s HIGH-half convention (bit 16 = pin 1) --
a different parameter this Discovery check does not use, so it is not
"defined identically" to both of the other two per-pin parameters, only to
`SHORT_TO_GND_J1962`'s half of the pair. The corrected row: `DeviceFlag`
(per-pin), `input_value` = the pin bitmap, same low-half shape as
`SHORT_TO_GND_J1962`. `ioctl_read_j1962_pin_voltage`'s Stage 2 Discovery
check constructs the real per-pin bitmap (`1 << (pin_number - 1)` for
`pin_number` in `1..=16`; an out-of-range `pin_number` skips the check,
unchanged native `ERR_PIN_INVALID` handling). Mock behavior did NOT change
as part of this correction itself: `j2534-0404-mock`'s
`DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED` dispatch arm still ignored
`p.Value` and unconditionally reported `Supported = 1` at the time this
correction was made, so it initially affected real-hardware/future-mock-
fidelity accuracy only, not test behavior -- until a separate mock-fidelity
fix (same PR as this note's own correction) made the mock's arm
per-pin-aware too, closing the gap this left between the mock's Discovery
answer and its own real `IOCTL_READ_J1962PIN_VOLTAGE` handler's pin
rejection set (0/4/5/17+). See `j2534-0404-service/docs/
implementation-notes.md`'s ADR-185 section for that fix's detail.

## Correction (Stage 2 lock-order fix)

`ioctl_start_repeat_message`'s original Stage 2 implementation acquired
`self.device_id` lazily (a peek-and-drop, only to derive `module_handle` and
the J2534-2 opt-in flag) and resolved its new `ProtocolCapacity` Discovery
precheck via `DeviceAccess::OpenIfNeeded` — deferred until after
`self.shared_channels` was already locked and held (this function's
pre-existing "Bug 1 fix," which holds `shared_channels` across the native
`start_repeat_message` call). `OpenIfNeeded` internally re-locks
`self.device_id` on a cache miss, so this ordering was `shared_channels`
(outer) → `device_id` (inner) — the reverse of this codebase's documented
outermost-lock invariant (`j2534-0404-service/src/service.rs`'s
`require_connected_device_for` doc comment, ADR-107 addendum: `device_id` is
always acquired first). `ConnectComLogicalLink`'s own connect path acquires
`device_id` outer and `shared_channels` inner, so the two orderings could
deadlock each other (AB-BA) under concurrent access — found by a
`design-advisor` review of the Stage 2 diff before merge, not by a test
failure.

The fix (same PR as the rest of Stage 2, no redesign): `ioctl_start_repeat_message`
now acquires and holds a named `device_id` guard up front, before
`shared_channels`, matching the connect path's own order, and resolves its
`ProtocolCapacity` Discovery precheck via `DeviceAccess::AlreadyOpen` against
that already-open device instead of `OpenIfNeeded`. This gave
`enforce_discovery_capability`'s `ProtocolCapacity` + `AlreadyOpen`
combination a real resolution path for the first time
(`discovery_protocol_info_with_open_device`, mirroring
`discovery_device_info_with_open_device`'s existing shape) — previously a
documented no-op, since no Stage 2 caller needed it. `DeviceAccess::OpenIfNeeded`
and `discovery_protocol_info` are unaffected in behavior and remain fully
implemented and unit-tested (`enforce_discovery_capability`'s `OpenIfNeeded`
match arm is untouched); they simply have no production caller again, the
same state they were in before this fix, since `ioctl_start_repeat_message`
was their only one. The `device_id` guard is dropped immediately after the
Discovery check, before the native call and the `chans`/`logical_links`
bookkeeping that follows, so it does not needlessly serialize
`ModuleDisconnect` against the rest of the call.

## Correction (second lock-order fix, `design-advisor` consult)

A second instance of the same lock-order class the previous correction
fixed was found affecting BOTH `discovery_protocol_info_with_open_device`
(this Stage) and `discovery_device_info_with_open_device` (shipped in
Stage 1, already on `main` before this fix). Unlike the previous
correction (a caller acquiring `shared_channels` before `device_id`), this
one was inside the two `_with_open_device` helpers themselves: both
functions' native-call error branches read `self.module_state.lock().await
.last_error` internally, on the assumption that doing so was always safe.
It is not: both are reachable via `DeviceAccess::AlreadyOpen` while the
caller already holds `self.shared_channels` --
`discovery_protocol_info_with_open_device` via
`ioctl_start_repeat_message`'s own "Bug 1" region (`rpc_misc.rs`, which
holds `chans` across the Discovery precheck, per the correction above), and
`discovery_device_info_with_open_device` via
`rpc_connect_com_logical_link`'s check-create-insert region (`rpc_link.rs`,
which holds `chans` from its own `shared_channels.lock()` call through a
later `drop(chans)`). Locking `module_state` internally in either helper
therefore risked the same inverted `device_id -> module_state ->
shared_channels` order (ADR-107 addendum/ADR-134) the previous correction
fixed for the caller-side case -- this time inside the callee. A
secondary defect of the same shape existed alongside it in
`discovery_device_info_with_open_device`: its native call's `self.api`
`MutexGuard` (from `self.api.lock().await.get_device_info(...)` as the
`match` scrutinee) lived to the end of the match arm by temporary lifetime
extension, so the error arm's `module_state` lock also overlapped an
already-held `self.api` guard.

**The fix (same PR): both helpers take `last_error: Option<TrackedError>`
in from their caller instead of reading `module_state` fresh.**
`discovery_device_info_with_open_device` and
`discovery_protocol_info_with_open_device` gained a trailing `last_error`
parameter, used directly in their `map_native_error_for_link` call instead
of a fresh lock. Every caller now supplies its own already-read snapshot:
the `OpenIfNeeded`-shaped wrappers (`discovery_device_info`/
`discovery_protocol_info`) read `module_state` themselves right after
`ensure_open_device_for` (legal there -- they never hold
`shared_channels`) and pass it through; `check_chx_capacity` and
`enforce_discovery_capability` (via its `resolve_discovery_device_info`
dispatch helper, and directly for the `ProtocolCapacity`+`AlreadyOpen` arm)
gained the same trailing parameter and forward whatever `last_error` their
own caller already supplied. `rpc_link.rs`'s `check_chx_capacity`/
`enforce_discovery_capability` call sites (both in
`rpc_connect_com_logical_link`'s connect-new-physical-channel branch) now
share one `last_error` read, hoisted above both calls instead of
duplicated for the second one, so both consult the identical snapshot.
This closes both the caller-side and callee-side instances of the same
lock-order hazard in one change, and removes the `self.api`-guard overlap
as a side effect (the error arm no longer locks anything else, so no
temporary-lifetime-extension overlap is possible).

**Accepted residual:** threading `last_error` in from the caller instead of
reading `module_state` fresh inside the two `_with_open_device` helpers
trades away some diagnostic freshness in exchange for closing the deadlock.
In `check_chx_capacity`'s call path (`rpc_link.rs`'s connect-new-physical-
channel branch), the shared `last_error` snapshot is now read before the
native `GET_DEVICE_INFO` call (and before waiting on `self.api.lock()`)
rather than after it fails, inverting this codebase's usual ADR-105
convention of reading `last_error` after the native call fails, not before
waiting for the api lock (`rpc_misc.rs`, around line 3237-3242, states that
convention canonically elsewhere). A concurrent hard-channel error arriving
while this connect is waiting on the api lock can now be missed from this
particular error's diagnostic detail — the `PduError`/gRPC `Code` returned
are unaffected, this is diagnostic-message-only. A parallel, smaller
staleness exists in `ioctl_start_repeat_message`'s own Discovery rejection
(`rpc_misc.rs`): its `last_error` snapshot is read notably earlier than the
check itself, when a fresher read was available at the check's own
`logical_links` re-lock a few lines later. This tradeoff was reviewed and
accepted by a `design-advisor` consult during this fix (the caller-supplied
snapshot still satisfies ADR-105's requirement as paraphrased for a
`?`-propagated path: a failing RPC reflects the tracked error, and nothing
requires re-reading it at the exact failure instant) and is not being
revisited — reopening it risks reintroducing the lock-order deadlock this
fix closes.
