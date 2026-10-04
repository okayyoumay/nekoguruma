# ADR-164: SAE J2534-2 Single Wire CAN (SWCAN/GMLAN) — Phase 4

**Date:** 2026-08-07
**Status:** Accepted (this ADR's "RX-side bits deferred" residual's exclusion half resolved by
             ADR-172; the RxFlag-forwarding half now resolved for `SW_CAN_HV_RX` by ADR-191,
             `SW_CAN_HS_RX`/`_NS_RX` narrowed to a separate deferred item by that same ADR;
             Decision's SW `_CHx` Additional Channels deferral fulfilled by ADR-212)
**Affects:** `j2534-0404-service/src/service/resources.rs`, `names.rs`, `rpc_link.rs`, `comparam_support.rs`, `comparam_id.rs`, `rpc_primitive.rs`, `rpc_misc.rs`, `j2534-0404-mock`, `docs/j2534-2-support-plan.md`

## Context

`docs/j2534-2-support-plan.md`'s Phase 4 targets SAE J2534-2 clause 9 (Single
Wire CAN / SWCAN / GMLAN), the family ADR-017 pre-emptively rejected via its
`CP_ChangeSpeed*` ComParam rejection (ADR-017 now superseded by ADR-152,
Phase 0). Phase 0 already added every SWCAN-related constant to
`j2534-0404-sys`'s header in one pass: protocol IDs `PROTOCOL_SW_CAN_PS`/
`PROTOCOL_SW_ISO15765_PS` (plus `_CH1`..`_CH128`), IOCTLs `IOCTL_SW_CAN_HS`/
`IOCTL_SW_CAN_NS`, `SET_CONFIG`/`GET_CONFIG` parameters
`CONFIG_SW_CAN_HS_DATA_RATE`/`CONFIG_SW_CAN_SPEEDCHANGE_ENABLE`/
`CONFIG_SW_CAN_RES_SWITCH`, and the ComParam `PARAM_SW_CAN_HIGH_VOLTAGE`
(seeded in bus-type defaults, not yet allowlisted for any protocol).

The plan's up-front, coarse assessment marked Phase 4 "design-advisor: No,"
but detailed reconnaissance surfaced a genuine structural question that
assessment didn't anticipate: SWCAN's connect-time model does not cleanly
match either of this codebase's two existing `_PS`-family precedents.

**Precedent A (J1850VPW/CAN/etc., ADR-156/157):** the protocol has its own
unqualified base `ProtocolID` and its own resource-table row with default
`dlc_pins`. A client can connect with no explicit pin selection (implicit
default pins, hw id stays the base id) or request custom pins, which
promotes the hw id to the `_PS` variant and triggers an internal
`SET_CONFIG(CONFIG_J1962_PINS)`. `ps_protocol_id()`/`base_protocol_id()` are
the funnel every other call site ("Plane B") uses to treat a promoted `_PS`
link as its base family.

**Precedent B (CAN FD / ISO15765-on-CAN-FD, ADR-158/159):** clause 21/22
define *only* `_PS`/`_CHx` variants — no unqualified base id exists at all.
There is no dedicated resource-table row for FD; FD mode is inferred at
`ConnectComLogicalLink` time from staged Working ComParams on an ordinary
CAN/ISO15765 connect (`rpc_link::J2534Service::apply_fd_mode`), which
substitutes the hw id and issues an explicit
`SET_CONFIG(CONFIG_J1962_PINS)` via `resources::default_pin_select_for_base()`
(needed because a substituted-in-place connect has no "implicit,
already-pinned" default the way a base-id'd connect does).

SWCAN matches neither precedent cleanly:

- Like Precedent B: clause 9 defines *only* `SW_CAN_PS`/`SW_ISO15765_PS`
  (plus `_CHx`) — no unqualified "SW_CAN" base id exists in the header at
  all. Clause 9.2.1: the physical layer stays disconnected until
  `SET_CONFIG(J1962_PINS)` is made — never an implicit already-pinned
  default connect.
- Unlike Precedent B, like Precedent A: SAE J2534-2's own spec and ISO
  22900-2 Annex G's worked example both treat SWCAN as an independently
  client-selectable D-PDU resource, not a substitution inferred from
  ComParams on an existing resource. Annex G explicitly lists "Single-Wire
  CAN on Pin 1 (Protocols: ISO 11898, ISO 15765)" as its own distinct listed
  resource, separate from the example's Dual-Wire CAN resources. ISO
  22900-2's own ComParam value tables (`CP_Baudrate`, `CP_BitSamplePoint`,
  `CP_ListenOnly`, `CP_SamplesPerBit`, `CP_SyncJumpWidth`,
  `CP_TerminationType`/`_Ecu`) all carry an explicit `SAE_J2411_SWCAN`
  bus-type column with its own default values distinct from
  `ISO_11898_2_DWCAN`'s — the same shape this codebase already uses to
  distinguish dual-wire CAN from fault-tolerant CAN (`ISO_11898_3_DWFTCAN`)
  under the *same* application-layer protocols. SWCAN is a third
  `bus_type_name` variant of several already-implemented CAN-family
  resource rows, reusing the existing `ChannelProtocol::Can`/`Iso15765`
  identity, not a substitution with no resource identity of its own.
- Pin data is single-pin only (clause 9.2.1: pin 1 of the J1962 connector,
  no secondary pin) — already a solved problem via Precedent A
  (`J1850VPW`'s row is `[(2, PIN_PLUS)]`, already in production), just not
  via Precedent B's `default_pin_select_for_base` (which requires exactly
  one primary + one secondary pin and is not used for this design; see
  Decision 1).

Two further scoped questions this ADR resolves alongside the connect-time
model: how much of the `CP_ChangeSpeed*` family ADR-017 rejected should
Phase 4 actually reverse, and how the two new `ChannelID`-scoped IOCTLs
(`SW_CAN_HS`/`SW_CAN_NS`) get exposed to a D-PDU client.

## Decision

**1. Connect-time resource/protocol model.** New resource-table rows whose
`hw_protocol_override` is the SW `_PS` id directly (`PROTOCOL_SW_CAN_PS`/
`PROTOCOL_SW_ISO15765_PS`), reusing existing `ChannelProtocol::Can`/
`Iso15765` identity and a `bus_type_name` of `SAE_J2411_SWCAN`, single
default pin `[(1, PIN_HI)]` (pin 1 per clause 9.2.1; `PIN_HI`'s existing
primary-role categorization is reused, not a new pin-type constant). A new
`is_sw_protocol_id()` funnel (parallel to `is_fd_protocol_id()`) and two new
`base_protocol_id()` arms (`PROTOCOL_SW_CAN_PS → CAN`,
`PROTOCOL_SW_ISO15765_PS → ISO15765`) let every existing Plane B call site
treat an SW link as its base family for free, the same reasoning ADR-157
established for every other `_PS`/`_CHx` id. Connect always emits an
explicit `SET_CONFIG(CONFIG_J1962_PINS)` — a new early arm in
`resolve_pin_selection`/`resolve_channel_selection` (positioned like the
existing FD guard) recognizes an SW row and returns the triple
`(base_id, sw_id, pin_select)`, with `pin_select` from caller `dlc_pin_data`
via the existing `compute_pin_select` (single-pin already proven safe by
`J1850VPW`) or the row's own default pin when the caller supplies none.

`ps_protocol_id()`/`is_ps_protocol_id()` (clause 6 Table 1's specific
`already_ps` semantics: requires `dlc_pin_data`, canonicalizes to base on
default pins) and `default_pin_select_for_base()` (Precedent B's two-pin
substitution contract) are **not** touched — SW has no base id to
canonicalize to and no ComParam-inferred substitution to trigger from, so
neither existing mechanism's invariants apply, and forcing SW through either
would either poison `already_ps` callers with the wrong semantics or require
loosening `default_pin_select_for_base`'s two-pin contract for a case that
was never its job.

**Explicit rejection, not silent fallthrough, for the paths this design
does not cover:**
- Staging FD ComParams (`CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate`) on an
  SW link must not silently rewrite it to `FD_CAN_PS` via `apply_fd_mode`
  (which matches on `base_protocol_id(link.hw_protocol_id) == CAN`, which
  now includes SW links) — add an explicit reject arm mirroring the
  existing `software_isotp`/`channel_index` rejections there.
- Connecting an SW resource row on a module not opted into J2534-2 (no
  `"J2534-2:"` `pname` prefix, ADR-152 Decision 1) must reject
  unconditionally, mirroring the existing FD-family opt-in gate.
- SW `_CHx` (Additional Channels on SWCAN) is out of scope this phase,
  matching ADR-156 Consequences' "accepted residual" pattern for every
  other not-yet-implemented `_CHx` family — verify a directly-named SW
  `_CHx` id is rejected outright (not silently accepted as an unrecognized
  id) via the existing clause-24 `_CHx` region check.

**2. `CP_ChangeSpeed*` ComParam scope: accept all 5, translate 3.** ISO
22900-2:2009 Annex A.1.2 (Table A.3, "Mapping of D-PDU API ComParams to SAE
J2534") maps exactly 3 of the 5 `CP_ChangeSpeed*` ComParams to native SAE
J2534 GetConfig/SetConfig names: `CP_ChangeSpeedRate → SWCAN_HS_DATA_RATE`,
`CP_ChangeSpeedCtrl → SWCAN_SPEEDCHANGE_ENABLE`, `CP_ChangeSpeedResCtrl →
SWCAN_RES_SWITCH` — corresponding exactly (modulo an editorial `SW_` vs
`SWCAN_` prefix difference between the 2009-era informative Annex table and
the final DEC2020 J2534-2 spec's naming) to the three native CONFIG ids
Phase 0 already added: `CONFIG_SW_CAN_HS_DATA_RATE`/
`CONFIG_SW_CAN_SPEEDCHANGE_ENABLE`/`CONFIG_SW_CAN_RES_SWITCH`. `to_j2534_config_id()`
gains real translations for these three, gated on `is_sw_protocol_id(hw_protocol_id)`.

`CP_ChangeSpeedMsg`/`CP_ChangeSpeedTxDelay` have no documented native
mapping anywhere (Table A.3 omits them, and clause 9's automatic-detection
model — the adapter itself watches for the fixed GMW3110 `0xA5 0x02`/`0xA5
0x03` byte sequence when `SW_CAN_SPEEDCHANGE_ENABLE` is on — defines no
configurable "message pattern" or "delay" `SET_CONFIG` param at all). All 5
`CP_ChangeSpeed*` ComParams are nonetheless accepted family-wide in the CAN
allowlist (not just the 3 with translations); `Msg`/`TxDelay` are
accepted-but-unmapped (`to_j2534_config_id()` returns `None` for them, the
same accepted-but-unmapped pattern this codebase already uses elsewhere).
This mirrors the CAN family's existing `CP_P3Func`/`CP_P3Phys` precedent
(`comparam_support.rs`): a ComParam seeded in bus-type defaults but absent
from the allowlist was itself a bug (a client-supplied SWCAN MDF ships all
5 `CP_ChangeSpeed*` values together; rejecting 2 of them breaks a
conformant client's `SetComParam`/`GetComParam` round-trip for zero
behavioral benefit, since the runtime never needs to *act* on `Msg`/
`TxDelay` — only store and echo them). `PARAM_SW_CAN_HIGH_VOLTAGE`
(`CP_SwCan_HighVoltage`) is allowlisted family-wide too, and wired as a
genuine per-message `TxFlags` bit this phase
(`TX_FLAG_SW_CAN_HV_TX`) — the mechanism already exists in this codebase
for exactly this "ComParam → per-message TxFlags bit" shape: ADR-062's
`ComParamSet::sci_tx_flags()` is cloned as `sw_can_tx_flags()`, ORed into
the same `apply_resolved_tx_flags` path (gated on the link's SW hw id, so
it cannot bleed onto a dual-wire CAN link that happens to have the
family-wide-allowlisted ComParam set to a nonzero value it never
consulted).

**3. `SW_CAN_HS`/`SW_CAN_NS` exposure: two new `L`-scoped ADR-079 commands,
no new plumbing.** Both IOCTLs are `ChannelID`-scoped per the spec text
(the `ChannelID` parameter is the channel identifier the DLL hands out at `PassThruConnect`), which
already matches ADR-079's existing `L` (ComLogicalLink-level) convention —
six existing commands already resolve this way via
`require_cll_handle_for_ioctl`. `SW_CAN_HS`/`SW_CAN_NS` become
`PDU_IOCTL_BASE + 0x12`/`+ 0x13` (the next two unused offsets after the
existing 17 commands' `+ 0x01`..`+ 0x11`), with `map_ioctl_name` entries and
handling in `resolve_object_id`'s `ObjtIoCtrl` arm, resolving `cll_handle →
channel_id → PassThruIoctl(IOCTL_SW_CAN_HS/NS)`. Rejected
(`PDU_ERR_FUNCTION_NOT_SUPPORTED`) on a non-SW link. No new RPC, no new
`DeviceID`-scoped plumbing — `ADR-152` Decision 2's mechanism (ride the
existing `IoCtl` RPC) extended one scope letter, the same pattern already
established for the 17 module- and CLL-scoped commands.

## Consequences

- **Resolved at implementation time:** `SW_CAN_HS`/`SW_CAN_NS` on a shared
  physical channel (`SharedChannel::ref_count > 1`) are skipped as a no-op
  (matching `PDU_IOCTL_CLEAR_TX_QUEUE`'s existing precedent) rather than
  silently affecting every sibling CLL with a mode change it never
  requested — a shared SWCAN bus has only one wire, so a per-CLL switch
  cannot be scoped narrower than the whole channel. `CP_SwCan_HighVoltage`
  DOES propagate to tester-present/cyclic periodic frames while nonzero
  (mirrors the existing SCI-voltage precedent's own per-message-flag
  behavior) — both decisions are pinned with tests, not left implicit.
  `resources.rs`'s reverse resource-id lookup (`find_resource_id_for_protocol`)
  and `rpc_get_resource_status` needed a genuine fix (not just verification)
  to correctly attribute a connected SW link against its dual-wire sibling
  when both share `ChannelProtocol` — a live-link misattribution gap this
  ADR's Context section anticipated as a risk but did not itself resolve.
- **Naming correction from this ADR's original text:** each new resource row
  gets its own distinct `protocol_name` (dual-wire sibling's name +
  `_SWCAN`), not the sibling's exact name as an earlier draft of this
  Decision implied. Two rows sharing one `protocol_name` with differing
  `hw_protocol_override` is this table's existing ambiguous-name shape
  (used correctly for the ADR-069 SCI wiring-variant rows, which need a
  disambiguating pin selection) — applying that same shape to SWCAN would
  have turned every pre-existing bare-name lookup of these ten dual-wire
  `protocol_name`s (e.g. `"ISO_15765_2"`) into a hard ambiguity error for
  any caller not supplying pins, breaking three pre-existing tests. `dlc_pins`
  and `bus_type_id` still disambiguate physically exactly as this ADR
  intended; only the string key changed.
- **Accepted residuals, this phase:** SW `_CHx` (Additional Channels on
  SWCAN) out of scope, matching every other family's `_CHx` deferral per
  ADR-156. FD-on-SW substitution explicitly rejected rather than silently
  wrong.
- **RX-side bits deferred, not implemented this phase; exclusion half later resolved by
  ADR-172:** `SW_CAN_HV_RX`/`SW_CAN_HS_RX`/`SW_CAN_NS_RX` (RxStatus bits 16-18) are
  explicitly out of scope, discovered to need more than "verify the bit
  position" once implementation reached this point: ADR-098's existing
  RxStatus→RxFlag forwarding mechanism carries only `RxStatus`'s 5 LOW bits
  (`RxStatus & 0x1F`) into `RxFlag` byte 3 — bits 16-18 fall in a
  completely different, currently-unused `RxFlag` byte range, so covering
  them is a genuine new RxFlag-byte extension, not a mechanical addition to
  an existing bit table. Deferred to its own follow-up (its own design
  question: which `RxFlag` byte carries bits 16-23, and whether ADR-098's
  scope should be reopened or a new byte added alongside it) rather than
  rushed into this already-large phase. `SW_CAN_HS`/`SW_CAN_NS`' own
  success/failure is still observable via the RPC's own return status —
  only the passive, transition-confirmation RX indication is missing.
  A conformance audit later found this gap was not merely a missing positive
  indication but an active mishandling (bits 17/18 frames were parsed as
  ordinary content, `rx_status_flags == 0` under the pre-ADR-172 gate);
  ADR-172 withholds these frames from content processing entirely, closing
  the mishandling. The RxFlag-forwarding half described above remains
  deferred and unaffected.
- This pattern is specific to SWCAN's own combination of "no base id" +
  "independent resource identity"; it does not generalize automatically to
  a future J2534-2 family without re-checking which of Precedent A,
  Precedent B, or this ADR's hybrid actually fits that family's own spec
  text (`docs/j2534-2-support-plan.md` §5 step 4's existing per-phase
  design-question gate already requires this check).
