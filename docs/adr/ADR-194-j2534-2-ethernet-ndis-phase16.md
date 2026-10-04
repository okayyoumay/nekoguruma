# ADR-194: SAE J2534-2 Ethernet_NDIS (Phase 16)

**Date:** 2026-08-26
**Status:** Accepted
**Affects:** `j2534-0404-service` (`protocol.rs`, `resources.rs`, `rpc_link.rs`, `rpc_primitive.rs`,
`rpc_misc.rs`, `names.rs`, `service_params.rs`, `events.rs`, `comparam_id.rs`/`comparam_defaults.rs`,
`discovery.rs`), `j2534-0404-mock`

## Context

SAE J2534-2 clause 24 adds `ETHERNET_NDIS`, binding a J2534 channel to an NDIS/RNDIS Ethernet
adapter (native `PROTOCOL_ETHERNET_NDIS`, `0x00008013`) instead of the serial/CAN physical layer
every prior phase targets. `PassThruWriteMsgs`/`PassThruStartPeriodicMsg`/`PassThruStartMsgFilter`
are all rejected unconditionally on this protocol, `PassThruReadMsgs` is rejected too (clause
24.2.5.2-5.5), and the one new IOCTL, `GET_NDIS_ADAPTER_INFO` (`0x0000800F`), reports adapter
identity/status. Phase 0 (ADR-152) already added every native constant this phase needs to
`j2534-0404-sys/src/bindings/j2534_v0404.h` and regenerated bindings for all 5 targets — no header
edit or bindgen work this phase: `PROTOCOL_ETHERNET_NDIS`, `CONNECT_FLAG_NDIS_PINS_OPTION1`/`OPTION2`
(`0x00010000`/`0x00020000`), `IOCTL_GET_NDIS_ADAPTER_INFO`, the `NDIS_ADAPTER_INFORMATION` struct
(128-byte `AdapterUniqueID` + 64-byte `AdapterName` + 4-byte `Status` + 6-byte `MAC_Address` +
16-byte `IPV6_Address` + 4-byte `IPV4_Address` + 4-byte `EthernetPinConfig` = 226 bytes),
`DEVICE_INFO_ETHERNET_NDIS_SUPPORTED`, and `ERR_NO_CONNECTION_ESTABLISHED` (`0x00010001`) are all
already present and verified against the header directly.

`docs/j2534-2-support-plan.md` §6/§8 flagged this phase as "architecturally divergent from this
subsystem's synchronous DLL/10ms-poll model... may warrant its own crate," and recommended a
`design-advisor` consult before committing to an approach, plus confirming no overlap with any
separately-planned DoIP (ISO 13400) work before investing here. ADR-178 independently anticipated
the same consult, flagging Ethernet_NDIS as a candidate needing "genuinely new client-visible
RPC-level semantics... a new stream, a new handle concept" beyond what ComParam staging or
`DataItem.bytearray_data` can express.

Both concerns are resolved by this ADR. **The DoIP-overlap check is negative**: no DoIP
implementation, branch, or plan exists anywhere in this repository. The only DoIP-adjacent material
is a set of already-logged, already-accepted-as-out-of-scope backlog entries in
`j2534-0404-service/docs/implementation-notes.md` describing DoIP/TLS/ISOBUS IOCTLs and error codes
that exist in ISO 22900-2:2022 but have no J2534 v04.04 native substrate — a different layer
entirely (D-PDU/`iso22900-service`-side gaps, not J2534-side work), and unrelated to clause 24's
own scope (raw NDIS adapter binding, not DoIP message-level support — the plan's own §1 already
states no J2534-2 feature area defines DoIP itself). **The architectural-divergence concern
inverts where the actual divergence lives.** Clause 24 keeps the J2534 API surface entirely
conventional: an ordinary `PassThruConnect(ETHERNET_NDIS, flags, baud)` / `PassThruDisconnect`
channel lifecycle plus one channel-scoped read-only IOCTL. Everything unconventional — the actual
Ethernet payload traffic — is routed by the spec itself *outside* the J2534 API onto the OS network
stack (the diagnostic application does its own vehicle communication via sockets once the NDIS
adapter is bound and connected). Since this service is exactly and only a pass-thru DLL caller,
never the diagnostic application itself, that divergence is the application's concern, not this
service's — from this service's vantage point `ETHERNET_NDIS` is one of the *simplest* J2534-2
protocols: connect, disconnect, one info IOCTL, everything else hard-errored. It is a strictly
smaller job than Analog Inputs (ADR-177), already shipped in-crate with the same
"most-message-API-disallowed" shape.

## Decision

**Ship as an ordinary in-crate phase — no separate crate, no new RPC method, no proto change.**

- **Identity.** A seventh standalone, self-mapping `ChannelProtocol` variant (the UART Echo
  Byte/Honda DIAG-H/J1708/J1939/TP2.0/GM UART shape — Phase 9/10/11/5/7/8's precedent — not a
  CAN-family bus-type variant), one new `resources.rs` row (`hw_protocol_override` pointing at
  `PROTOCOL_ETHERNET_NDIS`), bus-type name `IEEE_802_3` (ISO 22900-2:2022 Table B.2 already names
  this as DoIP's own physical-layer/BUSTYPE short name — the first J2534-2 standalone protocol
  whose resource-row bus-type name has a real ISO anchor, unlike Phase 9/10/11/15's synthetic
  names), pin data per clause 24 Table 103's J1962 pin set. No `_PS`/`_CHx` mechanics: pin selection
  is by connect flag (Option 1/2), not `J1962_PINS`-style resolution, and clause 24 defines no
  additional-channel concept. The row's typed `dlc_pins` carries only Option 1's Tx pins (3/11);
  `GetConflictingResources`'s static, pre-connect `rows_conflict` computation (ISO 22900-2 §9.4.26)
  and `GetResourceIds`'s pin-qualified resource lookup (`names::retain_rows_matching_all_pins`) each
  separately account for Option 2's alternate Tx pins (1/9) too, since a real connection could
  still stage either option (Codex review, PR #102) — kept out of `dlc_pins` itself because that
  field also feeds `names::resolve_pin_selection`'s exact-length defaults-match check, where
  widening it would turn a currently-accepted no-op `dlc_pin_data` request into a spurious
  rejection. A type-only pin query (no specific number) needs no alternate-pin handling of its
  own: Option 1's and Option 2's Tx pins share the same logical `PIN_PLUS`/`PIN_MINUS` types, so
  the row's existing `dlc_pins` already matches those queries correctly. `GetResourceIds`'s
  multi-pin queries are evaluated as ONE atomic wiring choice, not per-entry independently against
  the union of both options (Codex review, PR #102 round 6): the Ethernet_NDIS row is set aside
  before the ordinary per-entry retain loop runs, then separately re-admitted only if the WHOLE
  requested pin set is simultaneously satisfiable under Option 1 or simultaneously satisfiable
  under Option 2 — never a pin-by-pin mix, which would otherwise let an impossible combination
  (e.g. Option 2's Tx(+) pin 1 together with Option 1's Tx(-) pin 11) still match.
  `ethernet_ndis_alternate_pin_overlap` also consults a peer row's own dynamically selectable
  pins, not just its static `dlc_pins` default, via `resources::peer_closed_set_extra_pins`,
  checked against Ethernet_NDIS's Option 2 alternate pins (1/9) (Codex review, PR #102 round 7):
  Honda DIAG-H's row (0x023B) defaults to pin 14, but clause 13.2.4 also lets a real connection
  select pin 1 (overlapping Option 2); GM UART's row (0x0260) defaults to pin 9 but clause 11.2.2
  also allows pin 1 (likewise Option 2) — two closed, bounded alternate-pin sets a plain
  `dlc_pins` comparison on either side would miss. A round-8 finding asked the mirrored question
  for FT-CAN's row (0x0230, clause 20.2.1's alternate pin-pair 3/11, reversed onto Ethernet_NDIS's
  Option 1 BASE pins) — investigation (`edge-case-hunter`, PR #102 close-out) found this is not a
  live gap: FT-CAN's own `dlc_pins` DEFAULT to pins 1/9, identical to Ethernet_NDIS's Option 2
  alternate set, so the ordinary unconditional `dlc_pins`-overlap check already reports this pair
  as conflicting independent of `peer_closed_set_extra_pins`; a round-8 change adding FT-CAN's
  (3, 11) extras there changed no observable `rows_conflict` result (confirmed by enumerating all
  97 table rows with and without it) and was reverted. Deliberately scoped to only these two
  protocols' alternates: UART Echo Byte/J1708/J1939 accept arbitrary pins with no closed set to
  check at all, left as an accepted residual (`implementation-notes.md` backlog), since a fully
  general fix would need `rows_conflict` to understand every dynamic-pin protocol's alternate
  space against every other, not just Ethernet_NDIS's own two options — and even for the
  closed-set protocols, this covers only their overlap with Ethernet_NDIS specifically, not (e.g.)
  Honda DIAG-H's alternate pin 1 against GM UART's own alternate pin 1, which remains part of that
  same general, unaddressed limitation. Like every other
  standalone J2534-2 protocol, creating/connecting an Ethernet_NDIS CLL requires the module's own
  clause 5 opt-in (`"J2534-2:"` `pname` prefix), enforced in `names::resolve_pin_selection`'s
  dedicated arm (Codex review, PR #102). That enforcement is reachable only through
  `parse_protocol_id_from_resource`'s pin-selection branch, not its table-row-matched branch (used
  by `RscData::ProtocolName`/a bare `ResourceName`) — Ethernet_NDIS is excluded from
  `row_needs_dynamic_pin_selection` (clause 24 has no pin concept to route dynamically), so an
  explicit exception routes it through the pin-selection branch anyway on that path too
  (`edge-case-hunter` finding, PR #102 round 2), since `resolve_channel_selection`'s own opt-in
  check is deliberately scoped to `_CHx` only. The identical bypass predated this phase for Analog
  Inputs (ADR-177/178, same excluded-from-`row_needs_dynamic_pin_selection` shape) — out of this
  phase's own scope, so recorded as its own backlog item rather than fixed here, then closed
  separately via the identical `matched_row_is_analog_in` exception mirroring this one, once
  Ethernet_NDIS's own case had already proven the fix shape.
- **Connect-time pin option.** New service-level ComParam `CP_NdisPinOption` (UNUM32; `0` = auto,
  `1` = Option 1, `2` = Option 2; default `0`), staged via `SetComParam`, resolved at
  `ConnectComLogicalLink` into `CONNECT_FLAG_NDIS_PINS_OPTION1`/`OPTION2` (never both — the native
  table treats both bits set as equivalent to neither, so auto is encoded as neither bit).
  Default `0` is spec-functional (auto-detect), so — unlike `CP_AnalogSampleRate`'s required,
  connect-failing-if-unset design (ADR-177/ADR-178) — this ComParam is genuinely optional. A native
  `ERR_NO_CONNECTION_ESTABLISHED` (activation-line failure) maps to its own D-PDU error rather than
  being folded into an existing generic connect-failure code.
- **Shared-channel join guard.** `ChannelKey` (`hw_proto`, `baud`, `pin_select`, `fd_rate`) cannot
  distinguish two CLLs staging different `CP_NdisPinOption` values on Ethernet_NDIS, since `baud`
  and `pin_select` are always `0` for this protocol — the same hazard class ADR-178 already closed
  once for `CP_AnalogSampleRate` via a join-mismatch rejection. A joining CLL whose resolved
  `CP_NdisPinOption` connect-flag bits (`CONNECT_FLAG_NDIS_PINS_OPTION1`/`OPTION2`, via
  `ndis_pin_option_connect_flags`) differ from the physical channel's already-recorded
  `SharedChannel::connect_flags` bits is rejected at `ConnectComLogicalLink` with
  `PDU_ERR_FCT_FAILED`, mirroring ADR-178's Analog Inputs precedent. Unlike that precedent, no new
  `SharedChannel` field is added: `connect_flags` is already stamped verbatim at physical-channel-
  creation time from the creating CLL's resolved connect flags and never mutated afterward, so it
  already satisfies ADR-178's "applied-at-creation, not live-restageable" invariant and is reused
  directly rather than duplicated into a second field that could desync from it. The comparison is
  in resolved-flag space, not raw staged-ComParam space, since `ndis_pin_option_connect_flags` maps
  any unrecognized staged value to auto (neither bit) — comparing raw values would spuriously
  reject two CLLs that in fact resolve to the same applied hardware configuration. No
  `CoptStartcomm`/`CoptUpdateparam` companion guard is needed: every COP type is already rejected on
  an Ethernet_NDIS link by the COP gate below, so no COP-time hook through which a mismatch could
  otherwise surface exists.
- **COP gate.** `rpc_start_com_primitive` rejects every COP type on an `ETHERNET_NDIS` link with
  `PDU_ERR_ID_NOT_SUPPORTED`, before handle allocation — a broader sibling of the Analog Inputs gate
  (`rpc_primitive.rs`), which exempts a receive-only (`num_send_cycles == 0`) `CoptSendrecv` since
  Analog Inputs' whole point is reading. Clause 24 bars reads too, so no exemption applies here.
  `PDU_IOCTL_START_REPEAT_MESSAGE` is a separate device-autonomous TRANSMIT path reached through the
  IOCTL dispatcher, not through this gate — `ioctl_start_repeat_message` (`rpc_misc.rs`) carries its
  own Ethernet_NDIS rejection, mirroring the existing Analog Inputs rejection there, and the mock
  mirrors it too (Codex review, PR #102).
- **Poll task.** Spawned uniformly from the existing `rpc_link.rs` connect path — no protocol-based
  skip of the physical-channel poll task itself, preserving the uniform `SharedChannel`
  cancel/teardown/`GetStatus` lifecycle every other protocol relies on (ADR-139 and others). A new
  protocol-derived `rx_supported: bool` on `ChannelPollCtx` gates only the shared RX pass
  (`events.rs`): without this gate, the poll task's ordinary `PassThruReadMsgs` call returns
  `ERR_NOT_SUPPORTED` per clause 24.2.5.2, which the poll task's existing hard-read-error-closes-
  the-channel rule would otherwise misinterpret as an external channel loss and tear the link down
  moments after connect. No other per-tick duty needs gating: nothing can enqueue a `TxItem` (the
  COP gate above prevents it), and no ComParam allowlist entry exists for tester-present/timing
  parameters on this protocol, so those duties are naturally inert.
- **IOCTL.** `PDU_IOCTL_GET_NDIS_ADAPTER_INFO`, channel-scoped, a direct adapter call following the
  established `rpc_misc.rs` thin-forwarder pattern (`SW_CAN_HS`/`BECOME_MASTER`/`*_REPEAT_MESSAGE`).
  Output is hand-packed into `DataItem.bytearray_data`, mirroring the native struct's field order
  and widths exactly (`AdapterUniqueID`/`AdapterName` as raw byte spans, `Status`/`EthernetPinConfig`
  as little-endian `u32`s per ADR-178's convention, `MAC_Address`/`IPV6_Address`/`IPV4_Address`
  passed through byte-for-byte since the spec already stores them in network order) — no new proto
  message, per ADR-178's freeze. The byte layout is documented in `docs/rpc-api-guide.md`.
- **Discovery.** Connect gated on `DEVICE_INFO_ETHERNET_NDIS_SUPPORTED`, following ADR-185 Stage 1's
  existing `resources::connect_discovery_check` pattern; the mock's `GET_DEVICE_INFO` handler
  advertises it for the protocol it now implements.
- **Mock.** Accepts `PassThruConnect(PROTOCOL_ETHERNET_NDIS)` (with a failure-injection knob for the
  `ERR_NO_CONNECTION_ESTABLISHED` activation-failure path), returns `ERR_NOT_SUPPORTED` from
  read/write/periodic/filter on that channel, and serves a mostly-canned `NDIS_ADAPTER_INFORMATION`
  — every field fixed except `EthernetPinConfig`, which is derived from the channel's own
  connect-time flags (`2` only when `CONNECT_FLAG_NDIS_PINS_OPTION2` is set and
  `CONNECT_FLAG_NDIS_PINS_OPTION1` is not, `1` otherwise — Codex review, PR #102 round 9; the mock
  previously always returned `1` regardless of which `CP_NdisPinOption` the connection actually
  resolved). The `1` case covers Option 1, auto/unset, AND both bits set together, mirroring
  `rpc_link.rs`'s own `ndis_pin_option_connect_flags`/this ADR's "both bits set is equivalent to
  neither" convention (above) exactly — unreachable through the real gRPC service, whose connect-flag
  derivation always emits at most one bit, but a direct FFI caller of the mock can set both
  (`edge-case-hunter` finding, PR #102 close-out).

**Alternatives rejected:**

- **Separate crate.** The crate boundary would enclose a protocol making roughly four native calls
  through the exact same DLL handle, module lifecycle (ADR-107), Discovery cache, and resource
  table the existing crate already owns — everything genuinely shared would be duplicated, and
  nothing would actually be isolated, since the divergent part (the out-of-band socket traffic) is
  outside the scope of *any* crate in this gateway, not something a new crate boundary would
  contain either.
- **New RPC method or proto field (ADR-178's flagged possibility).** No data flow exists here that
  needs one: the only new data is a fixed-size, read-only struct, squarely inside
  `bytearray_data`'s already-established job (Device Configuration, Repeat Messaging). Adding proto
  surface would also violate ADR-178's freeze directly, for no offsetting benefit.
- **Mapping the IOCTL onto ISO 22900-2:2022's `PDU_IOCTL_GET_ETH_PIN_OPTION`/
  `PDU_IOCTL_SET_ETH_SWITCH_STATE`** instead of a dedicated 1:1 native forward. Semantics mismatch:
  the 2022 D-PDU commands are pin-number-in/option-out and module-adjacent, while clause 24's
  `GET_NDIS_ADAPTER_INFO` is channel-scoped and returns the whole struct at once; clause 24 also
  gives no independent activation on/off control for `SET_ETH_SWITCH_STATE` to bind to (activation
  is tied to the connect/disconnect lifetime, not a separate toggle). Kept the service's established
  1:1 native-IOCTL convention instead (`READ_J1962PIN_VOLTAGE`, `*_REPEAT_MESSAGE` precedent).
- **Skipping the poll-task spawn entirely for this protocol.** Tempting, since no COP can ever exist
  on this link — but it would fork the uniform `SharedChannel` lifecycle multiple other ADRs lean on
  for marginal savings. The narrower `rx_supported` gate on just the RX pass is the smaller
  deviation from the established architecture.

## Consequences

- Payload traffic is out-of-band by spec design: a gRPC client remote from the service host can
  only use this feature if it has independent network reach to that host's NDIS adapter.
  `GET_NDIS_ADAPTER_INFO`'s MAC/IP output exists precisely to enable that, which is why it is
  surfaced faithfully rather than treated as a low-priority read.
- Adapter info is only queryable post-connect — clause 24's own IOCTL definition requires a live
  `ChannelID`; no pre-connect enumeration path exists.
- ISO 22900-2:2022's `PDU_IOCTL_SET_ETH_SWITCH_STATE` (independent activation-line control) has no
  clause-24 substrate to back it and stays out of scope, same disposition as every other
  2022-delta-audit IOCTL this codebase has already logged as intentionally unimplemented.
- Retires `docs/j2534-2-support-plan.md` §8's "Ethernet_NDIS scope fit" open risk — the plan's own
  hedge that this phase "may be better scoped as a separate initiative than as a
  `j2534-0404-service` phase" is resolved: it is not.
- The 2022-delta-audit P3 backlog entry in `j2534-0404-service/docs/implementation-notes.md`
  describing `PDU_IOCTL_GET_ETH_PIN_OPTION` as unimplementable ("J2534 v04.04 has no DoIP/ISOBUS
  substrate") becomes partially stale once this phase ships: `NDIS_ADAPTER_INFORMATION`'s
  `EthernetPinConfig` field is a real, now-implemented native source `GET_ETH_PIN_OPTION`'s answer
  could in principle be derived from, even though this phase does not itself implement that
  2022-only IOCTL. That entry is reworded in the same PR rather than left silently overtaken by
  this ADR's own Decision.
- Post-connect re-staging of `CP_NdisPinOption` on an already-joined CLL takes effect only at the
  next physical-channel creation, per normal connect-time-resolved ComParam semantics — it does not
  retroactively change a channel already open under a different resolved option. Auto (`0`) is not
  treated as wildcard-compatible with a forced Option 1/2 on a join: clause 24 Table 105 defines
  auto as its own active detection procedure, not a "don't care" that could silently match whatever
  a forced-option creator already established.
