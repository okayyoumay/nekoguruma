# ADR-079: GetObjectId IO_CTRL Name Table and PDU_IOCTL Command Set

**Date:** 2026-07-11
**Status:** Accepted, superseding ADR-078 (`OBJT_IO_CTRL` always-reject only); items 12-13's
silent-success error reporting superseded by ADR-114 (retry-tracking behavior unchanged); item
15's error-code choice superseded by ADR-135, then again by ADR-187 (the rejection itself is unchanged)
**Affects:**
- `j2534-0404-service/src/service/service_params.rs`
- `j2534-0404-service/src/service/names.rs`
- `j2534-0404-service/src/service/rpc_misc.rs`
- `j2534-0404-service/src/service.rs`
- `j2534-0404-service/src/service/events.rs`

## Context

ADR-078 made `GetObjectId(OBJT_IO_CTRL, shortname)` unconditionally reject
with `PDU_ERR_INVALID_PARAMETERS`, because this service had no IOCTL name
table at all — `rpc_misc.rs::rpc_io_ctl` only recognized 4 raw J2534 v04.04
IOCTL IDs (`CLEAR_RX_BUFFER`, `CLEAR_TX_BUFFER`, `CLEAR_PERIODIC_MSGS`,
`CLEAR_MSG_FILTERS`), passed straight through from the client as numeric
values, with no shortname resolution path.

This service now implements 17 D-PDU API (ISO 22900) style IOCTL commands as
real adapter-layer state and behavior (TX queue suspend/resume, client
message filters, RX event-queue policy, module-level diagnostics, and four
commands rejected as genuinely unsupported by this adapter). Named resolution
via `GetObjectId(OBJT_IO_CTRL, ...)` is required for a client to discover the
numeric `io_ctrl_command_id` for any of them, since the D-PDU API has no
universal numeric ID space of its own for IOCTL commands — that discovery
step is the entire point of `OBJT_IO_CTRL`.

## Decision

**Add `names.rs::map_ioctl_name`**, matching each of the 17 `PDU_IOCTL_*`
shortnames (case-insensitively, mirroring `map_pintype_name`/
`map_comparam_name`'s style) to an ID drawn from a new private namespace,
`service_params.rs::PDU_IOCTL_BASE = 0x2900_0000`. These IDs are an adapter-
owned contract: they only round-trip through this service's own
`GetObjectId` → `io_ctrl_command_id` → `rpc_io_ctl` path and never reach the
underlying J2534 DLL, so the exact numbering carries no cross-adapter
meaning. They are distinct from the 4 legacy raw J2534 IOCTL IDs (all
`<= 0x14`), which remain supported in `rpc_io_ctl` as recognized aliases for
their own 4 pre-existing commands, untouched by this ADR.

Wired into `resolve_object_id`'s `ObjtIoCtrl` arm: `map_ioctl_name` is tried
first, falling back to the same `PDU_ERR_INVALID_PARAMETERS` rejection
(ADR-078) only when the shortname matches none of the 17. **Only this one
object type's "always reject" behavior is superseded** — the general
"unknown shortname → `PDU_ERR_INVALID_PARAMETERS`" principle, and every other
`resolve_object_id` arm (`OBJT_BUSTYPE`/`OBJT_COMPARAM`/`OBJT_PINTYPE`/
`OBJT_RESOURCE`, and `OBJT_PROTOCOL`'s own separate numeric-fallback
behavior), are unaffected.

The 17 commands, their target (`M` = module-level/device-scoped, `L` =
ComLogicalLink-level/per-CLL), and their semantics:

1. **PDU_IOCTL_RESET (M)** — Soft state reset only: resets `module_state` to
   ready/idle and, per CLL, clears `rx_buf`/`tx_held`/`client_filters`
   (stopping each client filter via the wrapper first) and resets
   `tx_suspended`; flushes hardware RX/TX per physical channel where a
   `channel_id` is present. Does **not** `PassThruClose`/reopen the device —
   that would drop live channels/connections (explicit product decision).
2. **PDU_IOCTL_CLEAR_TX_QUEUE (L)** — Drops this CLL's `tx_held` queue and
   marks every currently-queued COP of this CLL cancelled via
   `cancelled_cops` (so in-flight mpsc items still get `PduCopstCancelled`
   through `should_skip_cancelled_item`, `events.rs`); clears the hardware TX
   buffer only when this CLL's `SharedChannel::ref_count == 1` — the
   channel-wide hardware clear cannot be scoped to one CLL when the physical
   channel is shared. Adapter-level, distinct from the legacy
   `IOCTL_CLEAR_TX_BUFFER` raw ID.
3. **PDU_IOCTL_SUSPEND_TX_QUEUE (L)** — Sets `tx_suspended = true`;
   `events::dispatch_tx_item` checks this per item (including cyclic/periodic
   follow-up cycles) and parks it in `tx_held` instead of executing it.
4. **PDU_IOCTL_RESUME_TX_QUEUE (L)** — Clears `tx_suspended` and drains
   `tx_held` in FIFO order back onto the owning `SharedChannel::tx_queue`.
5. **PDU_IOCTL_CLEAR_RX_QUEUE (L)** — Clears this CLL's `rx_buf`.
6. **PDU_IOCTL_READ_VBATT (M)** — `PassThruIoctl(READ_VBATT)`, `PDU_IT_IO_UNUM32` output.
7. **PDU_IOCTL_SET_PROG_VOLTAGE (M)** — `PassThruSetProgrammingVoltage`;
   mirrors the value into `J2534Service::prog_voltage` on success.
8. **PDU_IOCTL_READ_PROG_VOLTAGE (M)** — `PassThruIoctl(READ_PROG_VOLTAGE)`, `PDU_IT_IO_UNUM32` output.
9. **PDU_IOCTL_GENERIC (M)** — Rejected: `PDU_ERR_ID_NOT_SUPPORTED` (no invented byte-array passthrough convention).
10. **PDU_IOCTL_SET_BUFFER_SIZE (L)** — Caps the per-item byte size of
    `GetComPrimitiveData` result items; stored in
    `LogicalLinkState::result_buffer_limit` and consulted by
    `rpc_get_event_item` (Codex-review fix), which truncates
    `ResultData.data_bytes` to at most this many bytes when popping a
    buffered frame — `header_bytes`/`footer_bytes` (protocol framing,
    ADR-051) are untouched. `SubscribeEvent`'s live push fan-out
    (`events.rs`) is a separate concern from `GetComPrimitiveData` result
    retrieval and is intentionally left unaffected.
11. **PDU_IOCTL_START_MSG_FILTER (L)** — Installs client filters via
    `PassThruStartMsgFilter`, keyed by client-supplied `FilterNumber` in
    `LogicalLinkState::client_filters` (kept separate from
    `unique_resp_filter_ids`, this service's own ADR-005/008/039 filters). On
    a CAN channel connected with `CAN_29BIT_ID`/`CAN_ID_BOTH`, one
    `FilterNumber` now maps to more than one underlying `MessageFilterId` —
    the client's filter is installed once per applicable `TxFlags`/ID-width
    variant, mirroring `install_pass_all_filter`'s existing approach for the
    pass-all baseline — so the mask/pattern the client supplies matches
    regardless of which ID width the channel accepts (Codex-review fix, round
    12: a `TxFlags`-0 filter only matches 11-bit CAN Ids, so it previously
    never matched 29-bit traffic on such a channel).
    Rejected on an ISO15765 link (`PDU_ERR_VALUE_NOT_SUPPORTED`): per
    ADR-038, `FLOW_CONTROL_FILTER` is the only valid J2534 filter type there,
    and the D-PDU `PDU_IO_FILTER_DATA` the client supplies carries no
    flow-control-message field at all, so none of `PDU_FLT_PASS`/`_BLOCK`/
    `_PASS_UUDT`/`_BLOCK_UUDT` can become a spec-conformant
    `FLOW_CONTROL_FILTER` — this is a hard capability gap, not a policy
    choice. Also rejected outright when this CLL's physical channel is
    shared with another CLL (`ref_count > 1`, `PDU_ERR_FUNCTION_NOT_SUPPORTED`):
    a real hardware filter installs on the shared `channel_id`, not something
    scoped to one CLL — a `PDU_FLT_BLOCK` from this CLL would silently drop a
    sibling CLL's traffic. `rpc_connect_com_logical_link` enforces this
    reciprocally: a second CLL cannot join a physical channel that already
    has a client filter installed by its current sole owner.
    **`PDU_FLT_PASS`/`PDU_FLT_PASS_UUDT` are additionally rejected on any
    channel** (`PDU_ERR_FUNCTION_NOT_SUPPORTED`, amendment): every
    non-ISO15765 physical channel already carries a wide-open zero-mask
    `PASS_FILTER` installed at connect time (`install_pass_all_filter`,
    ADR-008) so this service's own response matching in `poll_rx_inner` sees
    every frame, and `poll_rx_inner` does not consult `client_filters` in
    software — so a narrower client `PASS_FILTER`/`PASS_UUDT` would be
    installed on real hardware yet change nothing observable, a silent no-op
    rather than the filtering behavior its name promises. `PDU_FLT_BLOCK`/
    `_BLOCK_UUDT` are unaffected: J2534 v04.04 BLOCK-wins precedence means a
    client `BLOCK_FILTER` genuinely restricts traffic even against the
    pass-all baseline, so blocking remains the one real filtering primitive
    this command offers. Software-enforced PASS filtering (delivering only
    matching frames to a specific CLL's `GetEventItem`/`SubscribeEvent`
    stream without touching hardware or other CLLs) was considered and
    rejected for this PR: it collides with an unresolved product question —
    whether a client's PASS filter should also gate the frames a live
    `ComPrimitive` is waiting on for its own response matching — that this
    ADR does not decide unilaterally; it is left for a future ADR if a real
    need for PASS narrowing emerges.
12. **PDU_IOCTL_STOP_MSG_FILTER (L)** — Looks up `FilterNumber` in
    `client_filters`, calls `PassThruStopMsgFilter`, removes it.
13. **PDU_IOCTL_CLEAR_MSG_FILTER (L)** — Stops every filter in
    `client_filters`, removing only the entries that actually stopped; a
    `FilterNumber` whose `PassThruStopMsgFilter` call fails stays tracked
    (logged, best effort) rather than being forgotten while potentially still
    active on hardware, so the client can retry `STOP_MSG_FILTER`/
    `CLEAR_MSG_FILTER` on it. Does not touch `unique_resp_filter_ids` and
    does not call the legacy channel-wide `IOCTL_CLEAR_MSG_FILTERS` raw ID.
14. **PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES (L)** — Sets
    `LogicalLinkState::event_queue_cap`/`event_queue_mode`, consulted by both
    RX ring-buffer insert sites in `events.rs` via the shared
    `push_rx_frame` helper. `T_PDU_QUEUE_MODE` maps to the new
    `EventQueueMode` enum: `PDU_QUE_CIRCULAR` → `OverwriteOldest` (the
    pre-existing default behavior), `PDU_QUE_LIMITED` → `DiscardNewest`, and
    `PDU_QUE_UNLIMITED` → `OverwriteOldest` as well, since this adapter's RX
    buffer is always bounded and has no unbounded mode to map it to.
15. **PDU_IOCTL_GET_CABLE_ID (M)** — Rejected (no underlying hardware capability): originally
    `PDU_ERR_CABLE_UNKNOWN`; error code superseded by ADR-135 → `PDU_ERR_FCT_FAILED` per Table 53
    (the 2009(E) edition's Table 53 reserves `PDU_ERR_CABLE_UNKNOWN` for "detection ran, cable
    unrecognized," which this adapter never reaches since it has no detection capability at all),
    itself further superseded by ADR-187 → `PDU_ERR_ID_NOT_SUPPORTED` once the 2022-edition
    re-check invalidated ADR-135's Table-53-specific basis.
16. **PDU_IOCTL_SEND_BREAK (L)** — Rejected: `PDU_ERR_ID_NOT_SUPPORTED` (J2534 v04.04 has no outbound UART break API, only inbound `RX_FLAG_BREAK`).
17. **PDU_IOCTL_READ_IGNITION_SENSE_STATE (M)** — Rejected: `PDU_ERR_ID_NOT_SUPPORTED` (no underlying hardware capability).

## Consequences

- **New per-CLL TX-suspend/`tx_held` siphon is a new concurrency surface on
  the shared per-physical-channel poll task.** Per-CLL FIFO ordering is
  preserved across a suspend/resume cycle (each CLL's own items park and
  drain in the order the poll task saw them); cross-CLL ordering on a shared
  physical channel was never guaranteed before this ADR and still is not —
  suspending one CLL does not pause or reorder a sibling CLL's items.
- **`PDU_IOCTL_CLEAR_TX_QUEUE`'s hardware TX-buffer clear is silently skipped
  when the channel is shared** (`ref_count > 1`); the software-side
  cancellation (`tx_held` drop, `cancelled_cops`) still applies unconditionally
  to this CLL's own items regardless of sharing.
- **`PDU_IOCTL_RESET` is a soft state reset, not a device close/reopen** —
  callers relying on a hardware-level `PassThruClose`/`PassThruOpen` cycle
  (e.g. to force-renegotiate something outside this service's own state) will
  not get one from this command.
- **`PDU_IOCTL_GENERIC`, `PDU_IOCTL_GET_CABLE_ID`, `PDU_IOCTL_SEND_BREAK`, and
  `PDU_IOCTL_READ_IGNITION_SENSE_STATE` are rejected as unsupported by this
  adapter** — by explicit product decision (no underlying J2534 v04.04
  hardware capability for any of them), not an oversight or a placeholder
  pending future work.
- **`PDU_IOCTL_START_MSG_FILTER` only truly restricts traffic via
  `PDU_FLT_BLOCK`/`_BLOCK_UUDT`.** `PDU_FLT_PASS`/`_PASS_UUDT` are rejected
  (`PDU_ERR_FUNCTION_NOT_SUPPORTED`) rather than silently accepted as a
  no-op, because this adapter's non-ISO15765 channels always carry a
  wide-open pass-all hardware filter and there is no software-side RX
  filtering by CLL. A caller wanting to narrow inbound traffic to specific
  patterns has no way to do so through this command today; only blocking
  specific patterns is possible. Installing (or later joining) a shared
  physical channel is also barred once any CLL on it has an active client
  filter, in either direction.
- **`GetObjectId` behavior change for `OBJT_IO_CTRL` only.** A caller that
  relied on ADR-078's unconditional rejection for this one object type must
  be updated to expect a successful resolution for the 17 recognized
  shortnames; an unrecognized shortname (including any bare numeric string —
  there is still no numeric fallback) continues to reject with
  `PDU_ERR_INVALID_PARAMETERS`, unchanged from ADR-078.
- **Tests:** `names.rs`'s `map_ioctl_name_resolves_all_seventeen_commands`
  pins the full name-to-ID table (case-insensitively); its
  `resolve_object_id_objt_io_ctrl_resolves_known_names_and_rejects_others`
  replaces the old `..._always_rejects` test. `events.rs`'s
  `push_rx_frame_tests` module pins `OverwriteOldest`/`DiscardNewest`
  eviction and a live mode switch. `tests/grpc_mock/pdu_ioctl.rs` pins the
  four reject-as-unsupported commands' exact `Status` code/message (resolved
  via `GetObjectId`, the real client-facing path, not a hardcoded literal)
  and TX-suspend/resume FIFO ordering end to end against the mock.
