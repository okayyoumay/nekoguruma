# ADR-191: SAE J2534-2 RxStatus Bits 16-18 — RxFlag Forwarding Decision

**Date:** 2026-08-24
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/events.rs` (`poll_rx_inner`, `rx_flag_bytes`,
             `CllQueueItem::Frame`/frame-struct fields), `j2534-0404-mock/src/lib.rs` (RX
             injection), `j2534-0404-service/docs/implementation-notes.md` (Prioritized Backlog),
             ADR-172 (extended, not superseded), ADR-179 (reconfirmed unchanged), ADR-188/ADR-190
             (their own "no client-visible establish/loss event" residual partially resolved by
             decision)

## Context

SAE J2534-2's `RxStatus` bits 16-18 are overloaded — their meaning depends on the receiving
frame's own native protocol id — and have accumulated as a single, repeatedly-deferred P2 backlog
item (`implementation-notes.md`'s Prioritized Backlog) since ADR-164 (Phase 4, SW-CAN) first
discovered the gap. Four already-shipped features each carry their own piece of it:

- **SW-CAN (clause 9, Table 12, ADR-164/ADR-172):** bit 16 = `SW_CAN_HV_RX` (a genuine,
  content-eligible high-voltage message, ADR-172 Decision 3), bit 17 = `SW_CAN_HS_RX`, bit 18 =
  `SW_CAN_NS_RX` (speed-transition confirmations, explicitly undefined data content, withheld
  entirely by ADR-172).
- **Fault-Tolerant CAN (clause 20, Table 86, ADR-168):** bit 17 = `LINK_FAULT` (same bit position
  as SW-CAN's `HS_RX`, disambiguated by protocol id) — a correctly-received, content-eligible
  message per Table 86, explicitly excluded from ADR-172's own withhold (Decision 3).
- **SAE J1939 (clause 16.4.6, Table 63, ADR-179):** bit 16 = `ADDRESS_CLAIMED`, bit 17 =
  `ADDRESS_LOST` — already routed into the address-claim/defend state machine and withheld from
  content; ADR-179 Decision 7 already explicitly decided **against** forwarding these into any
  `RxFlag` bit ("ISO 22900-2 defines no address-claim `RxFlag` bit... the client-visible surface
  is the StartComm COP's own result plus `CP_TesterSourceAddress` plus... a CLL error event, not a
  content-frame flag").
- **TP2.0 (clause 19.4.4, Table 81, ADR-188/ADR-190):** bit 16 = `CONNECTION_ESTABLISHED`, bit 17
  = `CONNECTION_LOST` — already routed into the connection state machine (both active, ADR-188,
  and passive, ADR-190) and withheld from content. Both ADRs' own Consequences sections list "no
  client-visible establish/loss event" as an accepted residual, explicitly pointing at this same
  backlog item for eventual resolution.

Two of these bits (SW-CAN's `HS_RX`/`NS_RX`) are handled today the way ADR-172 designed: withheld
entirely, no `RxFlag` bit, because their data content is explicitly undefined and this codebase
had no representable tag for them at the time. The other two SW-CAN/FT-CAN bits — `HV_RX` and
`LINK_FAULT` — are, by contrast, genuine content-eligible messages that ADR-172 deliberately left
un-withheld; **neither is currently tagged with any `RxFlag` bit either**, meaning a client
receiving one today gets the message but no annotation that it was a high-voltage reception or a
fault-tolerant link fault.

ISO 22900-2:2022 Annex D.2.2 (Table D.5, the `RxFlag` definition) already standardizes RxFlag byte
1 with specific bit assignments this codebase had not previously cross-referenced against J2534's
own `RxStatus` bit-16-18 table:

- byte 1 bit 0 = `SW_CAN_HV_RX` — described in near-identical terms to J2534-2 Table 12 bit 16.
- byte 1 bit 1 = `ECU_TIMING_CHANGE` — already implemented and shipped (ADR-146); a
  service-synthesized signal, not sourced from any `RxStatus` bit.
- byte 1 bit 2 = `SPD_CHG_EVENT` — described as a generic signal that the serial bus has switched to a different
  speed, direction-agnostic, with the frame's own accompanying data optionally carrying
  which speed. Structurally parallel to J2534-2 Table 12's `HS_RX`/`NS_RX` pair, but this is an
  inference from wording parallelism, not an explicit ISO↔J2534 cross-reference either spec draws
  itself — **not independently confirmed**, and forwarding through it would need a new
  flag-only-frame delivery mechanism (no CAN ID, no `UniqueRespIdTable`/COP match, and a
  recipient-set decision, since a speed transition is physical-bus-wide and an automatic
  transition has no commanding CLL) rather than a bit on the existing content-delivery path.
- byte 2, and the rest of byte 1, define nothing else relevant here; neither `LINK_FAULT` nor
  TP2.0's `CONNECTION_ESTABLISHED`/`_LOST` nor J1939's `ADDRESS_CLAIMED`/`_LOST` has any ISO
  22900-2-defined `RxFlag` counterpart anywhere in Table D.5 — each is a J2534-2-only addition
  with no D-PDU standard surface to forward through.

The `rx_flag` proto field (`vci-service-interface/src/proto/service.proto`, `bytes rx_flag = 1`)
is an unbounded byte array, not a fixed-width structure — populating more of it needs no proto
change and does not touch ADR-178's frozen-interface policy.

This ADR was reached via a `design-advisor` consult (2026-08-24), given the genuine
protocol-interpretation and design-decision content across four already-shipped features and one
partially-standardized cross-reference.

## Decision

**Split the backlog item into two independently-scoped pieces.** This ADR resolves the first
piece in full and reconfirms/records decisions closing three of the four protocols' own residuals;
the second piece (SW-CAN `HS_RX`/`NS_RX` → `SPD_CHG_EVENT`) is deliberately deferred to its own
future ADR, since it needs new delivery machinery, not a bit assignment, and bundling it here
would gate a settled 1:1 mapping behind an unsettled routing design.

### 1. Forward SW-CAN `HV_RX` (bit 16) into ISO RxFlag byte 1 bit 0

A direct, standardized mapping — ISO 22900-2:2022 Table D.5 and SAE J2534-2 Table 12 bit 16
describe the same signal in near-identical terms, and Table D.5's own footnote makes handling
optional and mandates `0` when a device cannot measure it, so forwarding whatever the device
reports (including a device that always reports `0`) is conformant in every case. Gated on
`resources::is_sw_protocol_id(frame_protocol_id)`, the same predicate ADR-172's own bit-17/18
withhold already uses — bit 16 is undefined on FT-CAN (Table 86 defines only bit 17), and J1939's/
TP2.0's own bit-16 frames are already withheld upstream of this check (`poll_rx_inner`'s existing
`is_j1939_protocol_id`/`is_tp2_0_protocol_id` arms run first and `continue` before this point is
ever reached for those protocols).

This does **not** change ADR-172's own content-eligibility decision for `HV_RX` — the frame stays
exactly as content-eligible as it is today (ADR-172 Decision 3 already reasoned through this: a
high-voltage message is genuine content, unlike `HS_RX`/`NS_RX`'s undefined data). This ADR only
adds the missing annotation on top of already-correct delivery.

**Mechanism:** `rx_flag_bytes` (`events.rs`) currently takes `(rx_status_flags: u8,
ecu_timing_change: bool)`. Two independent, RxFlag-byte-1-scoped booleans is the practical limit
for positional bool parameters before call-site ambiguity sets in — replace the signature with
`rx_flag_bytes(rx_status_flags: u8, extras: RxFlagExtras)`, where `RxFlagExtras { ecu_timing_change:
bool, sw_can_hv_rx: bool }`. This also gives a future `SPD_CHG_EVENT` bit (Piece 2, if it ships) a
natural landing spot without another signature change. `sw_can_hv_rx` is computed in
`poll_rx_inner`'s existing per-frame loop, immediately after the ADR-172 SW-CAN withhold check
(which already reads the frame's raw `RxStatus` value and its native protocol id for exactly this
disambiguation), and carried on the frame struct alongside the existing `ecu_timing_change` field
through to wherever `rx_flag_bytes` is finally called.

### 2. No new RxFlag bit for FT-CAN `LINK_FAULT` — record as an accepted residual, not a gap

Table 86 documents `LINK_FAULT` as tagging a message that was correctly received *despite* a
transceiver-detected fault — genuine, already-content-eligible data (matching ADR-172's own
reasoning for why `LINK_FAULT` was excluded from its withhold). The current fall-through delivery
is therefore correct, not a leak; only the fault annotation itself is missing. Minting a new,
non-ISO-standard `RxFlag` bit for it is rejected, for two reasons: it would invent D-PDU-visible
surface ISO 22900-2 does not define (the same objection ADR-179 Decision 7 already raised and
rejected for J1939's own address-claim bits), and it risks colliding with whatever bit position a
future ISO revision might eventually standardize for it. The client loses only the fault
annotation on an otherwise fully-delivered message — recorded here as a closed, accepted residual,
not carried forward as an open item.

### 3. No new RxFlag bit for TP2.0 `CONNECTION_ESTABLISHED`/`_LOST` — resolves ADR-188/ADR-190's residual by decision

Unlike SW-CAN/FT-CAN's bits, TP2.0's indication frames are not vehicle content at all — their data
(`Data[0..3]` = RX-ID, `Data[4..7]` = peer TX-ID on establish) is state-machine input for
`events_tp20_connection.rs`'s own connection routing, consumed and withheld before any content
processing runs. Tagging them into the content-delivery stream via a synthesized `RxFlag` bit
would be a category error (there is no "message" left to tag once the indication has been
withheld), and no ISO 22900-2 bit exists for this either. The connection's own establishment
outcome is already client-visible as the `CoptStartcomm` COP's own result (ADR-188's central
design: `REQUEST_CONNECTION` is invoked internally from `PDU_COPT_STARTCOMM`, so a client already
learns success/failure synchronously from the COP itself); a mid-session loss is already
observable indirectly, since the CLL's connection phase flips and a subsequent send fails. If
proactive loss notification is ever wanted beyond that indirect signal, the correct vehicle is a
CLL-scoped error/status event — the same shape ADR-179 already uses for J1939's own address-loss
visibility — not a `RxFlag` content-frame bit. This decision applies identically to both active
(ADR-188) and passive (ADR-190) connections; the CLL identity already disambiguates which
connection an operation belongs to, so no additional bit-per-kind distinction is needed even if a
future CLL-event mechanism is built.

**This decision resolves, by explicit choice rather than further deferral, the "no client-visible
establish/loss event" accepted residual ADR-188's Consequences (§7) and ADR-190's Consequences
both point at this backlog item to eventually settle.** Neither ADR's own Decision or Consequences
text needs revision beyond a Status-line annotation (§ below) — this ADR settles the question the
existing residual note already anticipated, it does not revise what either ADR decided about
connection lifecycle itself.

### 4. J1939 `ADDRESS_CLAIMED`/`_LOST` — reconfirmed out of scope, unchanged

ADR-179 Decision 7 already explicitly decided against forwarding these bits, for the same "no ISO
`RxFlag` bit exists, client visibility comes from elsewhere" reasoning items 2 and 3 above both
reach independently. Nothing since ADR-179 has undermined that reasoning — ADR-188/ADR-190 (TP2.0)
followed the identical pattern rather than revising it. Recorded here as explicitly reconfirmed,
not re-decided.

### 5. Deferred: SW-CAN `HS_RX`/`NS_RX` → `SPD_CHG_EVENT` (Piece 2, its own future ADR)

The semantic equivalence between J2534-2's `HS_RX`/`NS_RX` pair and ISO 22900-2's `SPD_CHG_EVENT`
looks genuine (structurally parallel descriptions of a bus switching to a different speed, direction-agnostic
at the bit level, consistent with clause 9.3.2.3's transition sequence) but is an inference from
wording parallelism, not a citation either spec draws itself, and needs independent verification
against both specs' surrounding prose before being committed to code. Even once confirmed, this is
not a bit-assignment change like item 1 above: `HS_RX`/`NS_RX` frames are currently withheld
entirely (ADR-172), so forwarding them requires a genuinely new delivery path — a flag-only frame
with no CAN ID and no COP/`UniqueRespIdTable` match, plus a recipient-set decision (a speed
transition is physical-bus-wide; an automatic transition has no single commanding CLL, unlike a
`SW_CAN_HS`/`_NS` IoCtl a specific CLL issued) — closer in shape to ADR-146's own
`ECU_TIMING_CHANGE` synthesis than to a simple `RxFlagExtras` field. ADR-172 Decision 2 already
anticipated this eventual replacement explicitly ("once the ADR-164-deferred RxFlag-byte-extension
work lands, this withhold can be replaced with a properly tagged delivery"), so deferring it here
supersedes nothing ADR-172 decided. Tracked as its own backlog item, replacing the old
undifferentiated "bits 16-18" entry (see Consequences).

## Consequences

- **`RxFlagExtras` is the first `RxFlag`-byte-1 field sourced from a raw `RxStatus` bit** —
  `ECU_TIMING_CHANGE` remains the only synthesized (non-`RxStatus`-sourced) byte-1 bit. Any future
  byte-1 addition (including a future `SPD_CHG_EVENT` piece) should extend `RxFlagExtras` the same
  way, not grow `rx_flag_bytes`'s parameter list further.
- **Narrows, rather than closes, the old "RxStatus bits 16-18" backlog item.** The old
  undifferentiated entry is replaced with a single, narrower P2/P3 item scoped to `SPD_CHG_EVENT`
  delivery alone (`implementation-notes.md`'s Prioritized Backlog) — the FT-CAN/TP2.0/J1939 pieces
  are closed outright per items 2-4 above, not carried forward.
- **ADR-172's own Status line** gains a partial-resolution annotation: its Decision item 3 (bit 16
  stays content-eligible, untagged) is superseded in the "untagged" half only — bit 16 is now
  tagged via `RxFlagExtras::sw_can_hv_rx`, while items 1/2/4 (the bit 17/18 withhold itself) remain
  fully in force, unchanged.
- **ADR-188's Consequences §7 "explicitly out of scope" bullet and ADR-190's Consequences residual
  note** are both now resolved by this ADR's item 3 rather than still-open — their own Status lines
  gain a pointer to this ADR rather than a text rewrite, consistent with this repo's own partial-
  supersession annotation convention (see ADR-142's Status line for the precedent this mirrors).
- **Test coverage:** the new `sw_can_hv_rx` forwarding needs a `j2534-0404-mock` injection knob
  (mirroring ADR-172's own Affects-line precedent for RX injection) and a `grpc_mock` regression
  test proving an SW-CAN frame with only bit 16 set is delivered with `RxFlag` byte 1 bit 0 set,
  distinct from ADR-172's own existing bit-17/18-withhold tests (which must continue to pass
  unchanged — this ADR does not touch that withhold).
- **Accepted residual:** whether `SPD_CHG_EVENT` genuinely corresponds to `HS_RX`/`NS_RX` is
  unverified beyond wording parallelism (see Decision item 5) — a future implementer picking up
  the narrowed backlog item should re-check both specs' surrounding prose before committing to the
  mapping, not treat this ADR's inference as settled.
- No new proto surface, no new IOCTL, no new `DataItem` variant — `rx_flag`'s existing unbounded
  `bytes` field absorbs the new bit with no schema change, consistent with ADR-178's freeze.
