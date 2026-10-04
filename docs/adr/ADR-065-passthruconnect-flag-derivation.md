# ADR-065: `PassThruConnect` `Flags` Are Derived From Working ComParams / UniqueRespIdTable

**Date:** 2026-07-07
**Status:** Accepted (the `ISO9141_NO_CHECKSUM` bit's "deliberately never
             set" rule below is superseded by ADR-198 Phase 2, scoped to a
             RawMode=ON K-line CLL: `connect_flags` now sets it iff
             `raw_mode && !checksum_mode`; every other CLL, including every
             RawMode=OFF K-line CLL, keeps this ADR's original bit-9-stays-0
             behavior unchanged)
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`connect_flags`,
             `can_connect_flags`, `entry_format_extended`, `phys_req_extended`,
             `install_pass_all_filter`, `connect_new_physical_channel`,
             `rpc_connect_com_logical_link`, `ensure_uudt_companion_channel`,
             `spawn_new_shared_channel`), `j2534-0404-service/src/service.rs`
             (`SharedChannel::connect_flags`),
             `j2534-0404-service/src/service/rpc_misc.rs` (`CLEAR_MSG_FILTERS`
             IoCtl handler), `j2534-0404-mock/src/lib.rs`,
             `j2534-0404-service/tests/grpc_mock/`, `j2534-0404/src/lib.rs`

## Context

`PassThruConnect`'s `Flags` argument was hardcoded to `0` at both call sites
(`connect_new_physical_channel` for the primary channel,
`ensure_uudt_companion_channel` for the ADR-046 UUDT companion channel). Per
SAE J2534-1 v04.04, four bits of `Flags` are meaningful here:

| Bit | Constant | Meaning |
|---|---|---|
| 8 | `CAN_29BIT_ID` | 0 = receive 11-bit CAN Ids, 1 = receive 29-bit |
| 9 | `ISO9141_NO_CHECKSUM` | ISO9141/14230 checksum handling |
| 11 | `CAN_ID_BOTH` | both 11- and 29-bit ID types in use; bit 8 then selects the *prioritized* type |
| 12 | `ISO9141_K_LINE_ONLY` | ISO9141/14230 init: 1 = K-line only, 0 = K and L line |

Always connecting with `Flags = 0` meant a channel configured entirely for
29-bit CAN Ids (e.g. via `UniqueRespIdTable` entries carrying only 29-bit
`CP_Can*Format`) was told at connect time that it would only ever see 11-bit
traffic — some adapters filter received frames by this bit at the hardware
level regardless of installed message filters. It also meant
`install_pass_all_filter`'s zero-`TxFlags` `PASS_FILTER` only ever matched
11-bit CAN Ids, silently dropping 29-bit frames on any raw-CAN channel
(`CAN` protocol, `software-isotp` mode, or the ADR-046 dual-channel
companion) whose traffic used 29-bit addressing.

The information needed to derive these bits already exists on the CLL at
connect time: the Working ComParam set (`CP_CanMixedFormat`, the link-level
`CP_CanFuncReqFormat`, `CP_K_L_LineInit`) and, for CAN/ISO15765, the
`UniqueRespIdTable` (`CP_Can{PhysReq,RespUSDT,RespUUDT}Format`, gated by
their respective `Id` params, exactly as `install_point_to_point_fc_filters`
already reads them for `FLOW_CONTROL_FILTER` construction).

## Decision

A new free function, `connect_flags(j2534_proto_id, working, urid_table)` in
`rpc_link.rs`, derives the v04.04 connect flags this service can determine:

- **`CAN`**: always `CAN_ID_BOTH`, plus `CAN_29BIT_ID` as the priority bit iff
  the physical-request address is 29-bit (`phys_req_extended`: the
  entry-level `CP_CanPhysReqFormat` of the `UniqueRespIdTable` entry that
  configures `CP_CanPhysReqId`, or the link-level Working `CP_CanPhysReqFormat`
  if no entry does). Raw-CAN channels in this service are wide-open receive
  channels — one pass-all `PASS_FILTER` per ID type, with service-side
  `UniqueRespIdTable` matching in `poll_rx` doing the real filtering — shared
  by `channel_key = (CAN, baud_rate)` alone. The set of CAN-ID widths such a
  channel will ever carry is therefore unknowable at connect time: a raw-CAN
  CLL connecting later at the same baud rate may configure the other width,
  and the ADR-046 UUDT companion channel (below) may reuse an existing
  `(CAN, baud)` channel that a raw-CAN CLL created, or vice versa, in either
  order. An observation-based derivation (the original version of this ADR)
  left a real gap here: whichever width connected first became the only
  width the shared channel — and hence every later joiner, including a UUDT
  companion reusing it — could ever receive, since `PassThruConnect` cannot
  be reissued on a live channel. `CAN_ID_BOTH` sidesteps the gap entirely by
  never claiming a single width in the first place.
- **`ISO15765`** (`can_connect_flags`): unlike `CAN`, an ISO15765 channel's
  steady-state filters are point-to-point `FLOW_CONTROL_FILTER`s built from
  the exact CAN ID (ADR-048), so deriving the connect flags from what is
  actually configured is safe here — there is no shared wide-open receive
  path for a width mismatch to hide in. Collects a CAN-ID-type observation
  (11- vs. 29-bit, from Table B.13 bit 1) for each configured address —
  mirroring the exact gating `install_point_to_point_fc_filters` uses:
  `CP_CanPhysReqFormat` counts only when the entry has `CP_CanPhysReqId`,
  `CP_CanRespUSDTFormat`/`CP_CanRespUUDTFormat` only when their `Id` is
  present; an `Id` present with no `Format` defaults to 11-bit
  (`CanIdFormat::default()`, unchanged from ADR-040). If the `UniqueRespIdTable`
  contributes no observation at all, the link-level Working
  `CP_CanPhysReqFormat`/`CP_CanRespUSDTFormat`/`CP_CanRespUUDTFormat` are used
  instead. `CP_CanFuncReqFormat` is link-level only (functional requests are
  not per-ECU) and is always additionally consulted. `CAN_ID_BOTH` is set
  when `CP_CanMixedFormat` is nonzero *or* the observations disagree; its
  priority bit `CAN_29BIT_ID` then follows the physical-request address
  specifically via the same `phys_req_extended` helper the `CAN` case uses
  (entry-level if any entry configures one, else the link-level format) —
  `CP_CanMixedFormat` is the client's tool to force `CAN_ID_BOTH` up front
  even with a single-type table. Otherwise `CAN_29BIT_ID` alone is set when
  every observation was 29-bit.
- **`ISO9141` / `ISO14230`**: `ISO9141_K_LINE_ONLY` iff `CP_K_L_LineInit` is
  present and nonzero — the D-PDU param mirrors the J2534 flag's own
  encoding (`0` = K and L line, matching the service's default).
  `ISO9141_NO_CHECKSUM` is deliberately never set: this service never
  generates or verifies ISO9141/14230 checksums itself (ADR-050), so bit 9
  stays `0` and checksum handling stays with the interface.
- **Every other protocol:** `0` — no v04.04 connect flag this service
  derives applies to them.

`rpc_connect_com_logical_link` computes this once, inside the same
`logical_links` lock read that snapshots `working`/`unique_resp_id_table` for
the connecting CLL, and threads it through `connect_new_physical_channel` to
`PassThruConnect`. Because channels are shared by `channel_key =
(j2534_proto_id, baud_rate)` and only the creator CLL's Working set is ever
applied to a shared channel (ADR-044), the connect flags follow the same
creator-decides rule: a joining CLL's ComParams/table never change the
already-connected channel's flags, and — a genuine J2534 limitation, not a
choice this service could route around — a `UniqueRespIdTable` set *after*
`Connect` does not retroactively change the flags either, since
`PassThruConnect` cannot be re-issued on a live channel. A client that needs
`CAN_ID_BOTH` from the first frame must set `CP_CanMixedFormat` (or a fully
representative `UniqueRespIdTable`) before calling `ConnectComLogicalLink`.

The ADR-046 UUDT companion channel (`ensure_uudt_companion_channel`) is
shared across CLLs by `(CAN, baud)` alone, and different CLLs sharing it may
configure either CAN-ID type for their UUDT response — heterogeneous and
unknown at the moment the companion is opened. It always connects with
`CAN_ID_BOTH`, unconditionally, rather than trying to infer a type from
whichever CLL happens to trigger the open — the same reasoning as the `CAN`
case above, since the companion channel *is* a raw-CAN channel. Because every
raw-CAN primary channel now also always connects `CAN_ID_BOTH`, reusing an
already-open `(CAN, baud)` channel in `ensure_uudt_companion_channel` is
width-safe regardless of which side created it first: a raw-CAN CLL's primary
channel that a UUDT companion later joins, or a UUDT companion that a
later-connecting raw-CAN CLL joins, are both guaranteed to already be
`CAN_ID_BOTH`.

`install_pass_all_filter` now takes the channel's connect flags and, for
`protocol_id == CAN` (the only protocol it can be called with that is CAN-ID
sensitive), installs one `PASS_FILTER` per ID type actually in use: two
filters (`TxFlags` 0 and `TX_EXTENDED_ID`) when `CAN_ID_BOTH` is set, one
`TX_EXTENDED_ID` filter for plain `CAN_29BIT_ID`, and the prior single
zero-`TxFlags` filter otherwise. A `TxFlags`-0 `PASS_FILTER` only matches
11-bit CAN Ids, so without this a channel connected with `CAN_ID_BOTH` or
`CAN_29BIT_ID` would still only ever receive 11-bit traffic at the pass-all
site — the same problem `Flags` itself fixes at the adapter level. Every
other protocol is unaffected by `connect_flags` (single zero filter,
unchanged).

The connect flags are also persisted on `SharedChannel` (a new
`connect_flags: u32` field, set by `spawn_new_shared_channel` from its new
`connect_flags` parameter, which both call sites — `rpc_connect_com_logical_link`
and `ensure_uudt_companion_channel` — already had in scope) so that the
`CLEAR_MSG_FILTERS` IoCtl's non-ISO15765 rebuild path in `rpc_misc.rs` can call
`install_pass_all_filter` with the same flags the channel was connected with,
instead of the single TxFlags-0 filter it built inline before this revision.
Without this, `CLEAR_MSG_FILTERS` on a `CAN_ID_BOTH` channel (raw CAN CLL
primary, `software-isotp` physical channel, or the ADR-046 UUDT companion)
would silently stop delivering 29-bit frames until reconnect — the same gap
`install_pass_all_filter` closes at connect time, reopened by a filter clear.
`connect_flags` is looked up from `shared_channels` via the CLL's
`channel_key`; if the key or channel entry is unexpectedly missing, the
rebuild falls back to a single TxFlags-0 filter with a `warn!`, matching the
pre-existing behavior in that case. The ISO15765 rebuild path
(`reinstall_iso15765_channel_filters_after_clear`) is untouched — it rebuilds
point-to-point `FLOW_CONTROL_FILTER`s from the CLL's `UniqueRespIdTable`
directly and was never affected by this gap.

`j2534-0404/src/lib.rs` gains re-exports for the two connect-flag constants
this service didn't already alias: `CONNECT_FLAG_CAN_29BIT_ID as
CAN_29BIT_ID` and `CONNECT_FLAG_CAN_ID_BOTH as CAN_ID_BOTH`, alongside the
pre-existing `ISO9141_NO_CHECKSUM` alias and a new
`CONNECT_FLAG_ISO9141_K_LINE_ONLY as ISO9141_K_LINE_ONLY`.

`j2534-0404-mock` records the `Flags` of every successful `PassThruConnect`
in a `connect_flags_log: Vec<u32>` (independent of per-channel state, so it
stays complete even for a channel later disconnected — e.g. the ADR-046
addendum's dual-channel-capability probe), exposed via
`__mock_get_connect_flags_log_len`/`__mock_get_connect_flags_log_entry` FFI
exports and the `MockBackdoor::connect_flags_log()` test helper.

## Consequences

- New tests in `j2534-0404-service/tests/grpc_mock/connect_flags.rs` cover:
  default ISO15765 (no addressing configured) `Flags == 0`; an ISO15765
  `UniqueRespIdTable` entry with a 29-bit physical-request/USDT-response pair
  alongside the link-level 11-bit `CP_CanFuncReqFormat` default, producing
  `CAN_ID_BOTH | CAN_29BIT_ID` (`0x900`); a default raw-`CAN` CLL (no
  addressing configured) producing `CAN_ID_BOTH` alone (`0x800`); a raw-`CAN`
  CLL with a `UniqueRespIdTable` entry configuring a 29-bit
  `CP_CanPhysReqId`/`CP_CanPhysReqFormat` pair producing `CAN_ID_BOTH |
  CAN_29BIT_ID` (`0x900`); `CP_K_L_LineInit = 1` on an ISO14230 CLL producing
  `ISO9141_K_LINE_ONLY` (`0x1000`); and the same CLL without it, `0`.
- `can_mode.rs`'s `dual_channel_mode_opens_companion_can_channel_for_uudt`
  was updated (in the original version of this ADR): the companion channel
  connects with `CAN_ID_BOTH` (`0x800`) and installs two `PASS_FILTER`s
  (TxFlags `0` and `TX_EXTENDED_ID`) instead of one.
- Every raw-CAN channel this service connects — a `CAN`-protocol CLL's
  primary channel, an ISO15765-family CLL's physical channel in
  `software-isotp` mode (`CanChannelMode::hw_protocol_id` maps it to `CAN`),
  or the ADR-046 UUDT companion — now always connects `CAN_ID_BOTH` and gets
  two pass-all `PASS_FILTER`s (TxFlags `0` and `TX_EXTENDED_ID`) instead of
  one. This updated two `can_mode.rs` tests that asserted a single filter on
  a raw-CAN channel in `software-isotp` mode:
  `software_isotp_mode_sends_single_frame_on_raw_can_channel` and
  `software_isotp_mode_reassembles_segmented_response_and_sends_flow_control`
  now expect `filter_count == 2` (both `PASS_FILTER`) instead of `1`; the
  former also asserts `connect_flags_log() == vec![0x800]`. ISO15765-family
  primary channels (`single-channel`/`dual-channel`/`auto` modes) and the
  ADR-046 dual-channel/probe companion connects were already `CAN_ID_BOTH`
  and are unaffected.
- Every existing test that connects a CAN-family or KWP-family CLL using a
  numeric `protocol_id` resource (no bustype/protocol name, so no
  `comparam_defaults` populate the Working set beyond explicit
  `SetComParam` calls) and configures no addressing before connect is
  unaffected for `ISO15765`/`ISO9141`/`ISO14230`: `connect_flags` still
  evaluates to `0` in the absence of any observation, identical to the prior
  hardcoded value. `CAN` is the one exception: it now always evaluates to at
  least `CAN_ID_BOTH` (`0x800`) regardless of configuration, per the gap this
  revision closes.
- New tests in `j2534-0404-service/tests/grpc_mock/clear_msg_filters.rs` cover
  the `CLEAR_MSG_FILTERS` rebuild: a raw-`CAN` CLL's channel and a
  `software-isotp`-mode ISO15765 CLL's underlying raw-CAN channel each
  reinstall exactly two `PASS_FILTER`s (TxFlags `0` and `TX_EXTENDED_ID`)
  after the IoCtl, matching the two installed at connect time.
- **Not addressed:** this decision does not attempt to detect or correct a
  connect-flags mismatch discovered after the fact (e.g. a joining CLL whose
  own table would have implied different flags than the channel's creator
  used) — per the J2534 API there is no way to change `Flags` on a live
  channel short of disconnecting and reconnecting, which would disrupt every
  other CLL sharing it, so this is left as a client-side responsibility
  (configure `CP_CanMixedFormat`/the table before the first CLL connects).
