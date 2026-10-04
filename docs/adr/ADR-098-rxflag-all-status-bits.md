# ADR-098: ISO 22900-2 RxFlag Forwards All 5 Low RxStatus Bits

**Date:** 2026-07-17
**Status:** Accepted (amended by ADR-100, ADR-151; content-frame gate extended with a
             protocol-family-conditional SW-CAN withhold by ADR-172; the
             ADR-061-inherited "no CAN-ID/addressing logic reads RxStatus
             bits" premise this ADR's Decision cites narrowed by ADR-222 to
             channels with no contended same-id-different-width
             `UniqueRespIdTable` entries)
**Affects:** `j2534-0404-service/src/service.rs` (`ReceivedFrame`, `ExpectedResponse::matches`),
             `j2534-0404-service/src/service/events.rs` (`poll_rx_inner`, `FcCapture`,
             `rx_flag_bytes`), `j2534-0404-service/src/service/rpc_primitive.rs`
             (`rpc_get_event_item`)

## Context

ADR-097 forwarded exactly one J2534 `RxStatus` bit — `START_OF_MESSAGE`
(bit 1) — into ISO 22900-2's `ResultData.rx_flag` byte 3 bit 1, and excluded
SOM frames from `ExpectedResponse`/pending-RC matching. ADR-097's own
byte-compatibility table, reproduced below, already established that
`RxFlag` byte 3 bits 0-7 map directly onto `RxStatus` bits 0-7 — the SOM-only
scope of that ADR was a deliberate narrowing, not a finding that the other
bits are unmappable.

The user-supplied "Valid RxStatus Bit Combinations" table drives this ADR's
scope — the 5 low bits it names:

| Status | bit4 ISO15765_PADDING_ERROR | bit3 TX_DONE (=TX_INDICATION) | bit2 RX_BREAK | bit1 START_OF_MESSAGE | bit0 TX_MSG_TYPE |
|---|---|---|---|---|---|
| Normal Message | 0 | 0 | 0 | 0 | 0 |
| RxStart | 0 | 0 | 0 | 1 | 0 |
| RxBreak | 0 | 0 | 1 | 0 | 0 |
| RxPadError | 1 | 0 | 0 | 0 | 0 |
| TxDone | 0 | 1 | 0 | 0 | 1 |
| Loopback Message | 0 | 0 | 0 | 0 | 1 |

This table is not exhaustive in practice: ADR-097 already documented a
CONFIG_LOOPBACK+SOM combination outside its 6 rows (bits 0 and 1 both set —
a loopback echo of our own SOM-tagged transmit). Real hardware is not
guaranteed to only ever emit these 6 combinations, so this ADR does not
validate or reject "invalid" combinations (see Decision).

Two of the other rows share ADR-097's SOM-specific hazard, for the same
underlying reason (`ExpectedResponse::matches`'s `cmp_len == 0` vacuous-match
case is not specific to an empty *descriptor* — it triggers for any
mask/pattern whenever the post-header-split *payload* is empty too, which is
the common shape of a TxDone or RxBreak indication just as much as a SOM
one):

- **TxDone**: an ISO15765 TxDone indication's `Data` is header-only (or
  empty), matching SOM's shape. Before this ADR, a TxDone frame carried no
  SOM bit and so was not excluded — an in-flight `CoptSendrecv`/pending-RC
  wait with a broad/empty `expected_response` descriptor would treat the
  TxDone indication itself as the awaited response, completing the COP
  before the ECU's real response arrived.
- **Loopback**: a CONFIG_LOOPBACK echo of our own transmitted request hits
  two hazards. First, the same empty/short-payload vacuous-match hazard as
  above. Second, and more subtly, a loopback echo can register a **phantom
  pending-RC**: `RcHandlingConfig::detect_pending_rc` reads
  `payload[CP_RCByteOffset]` and treats a matching byte value as a pending
  NRC (0x78/0x21/0x23) — but a NRC byte value is just a byte value, and a
  request whose own first byte happens to equal one of those (e.g. SID 0x23,
  ReadMemoryByAddress, is byte-identical to NRC 0x23,
  RequestSequenceError/ConditionsNotCorrect) will, when looped back with
  `CP_RC23Handling` enabled, be misread by `detect_pending_rc` as a genuine
  pending NRC from the ECU. This check runs *before* the mask/pattern match
  check in `poll_rx_inner`'s `MatchProbe` arm, so it is not merely a
  vacuous-match variant of the SOM hazard — it is a distinct false-positive
  path that a non-empty, well-formed mask/pattern does not protect against
  either.

Additionally, the software-ISO-TP TX driver's `FcCapture` (the mechanism
that watches for the ECU's FlowControl reply to our own segmented request)
has no `RX_TX_MSG_TYPE` awareness at all: if CONFIG_LOOPBACK is active and
our own transmitted FlowControl frame (sent by this service in reply to a
segmented *incoming* message, see `process_frame_for_entry`'s
`Frame::First` arm) is echoed back, `FcCapture` would parse it as ISO-TP
FlowControl content indistinguishable from a genuine ECU FC frame and
wrongly release the withheld ConsecutiveFrames of an unrelated in-flight
outbound segmented request.

## Decision

`ResultData.rx_flag` byte 3 becomes `RxStatus & 0x1F` (the 5 low bits:
`TX_MSG_TYPE`, `START_OF_MESSAGE`, `RX_BREAK`, `TX_INDICATION`,
`ISO15765_PADDING_ERROR`), bit-copied **unconditionally** — no combination
validation or rejection of RxStatus values outside the 6-row table above.
This is consistent with this codebase's existing trust-the-hardware idiom:
ADR-061's audit found no CAN-ID/addressing interpretation logic anywhere in
this service that inspects `RxStatus` bits itself (they are opaque payload
to be forwarded, not decoded), and `has_external_activity`
(`poll_rx_inner`'s `RX_TX_MSG_TYPE` read for the tester-present idle timer)
already reads a raw `RxStatus` bit unconditionally without validating it
against any expected combination. `rx_flag_bytes(rx_status_flags: u8)`
emits `[0x00, 0x00, 0x00, rx_status_flags]` when nonzero, `Vec::new()`
(ADR-061's "empty means nothing asserted" convention) when all 5 bits are
zero (a Normal Message).

`ReceivedFrame` gains a `rx_status_flags: u8` field (replacing ADR-097's
`start_of_message: bool`), computed once per polled message as
`(msg.rx_status() & RX_STATUS_FLAGS_MASK) as u8` and carried to both
`ResultData` construction sites, same as ADR-097's field was. The synthetic
fast-init response frame in `handle_start_comm` has no real `RxStatus` and
stays `rx_status_flags: 0` (`rx_flag: Vec::new()`, unchanged from ADR-097).

`ExpectedResponse`/pending-RC eligibility in `poll_rx_inner`'s `MatchProbe`
arm requires `rx_status_flags & !RX_ISO15765_PADDING_ERROR == 0` — a frame is
eligible unless one of the 4 indication-type bits (`TX_MSG_TYPE`,
`START_OF_MESSAGE`, `RX_BREAK`, `TX_INDICATION`) is set; `RX_ISO15765_PADDING_ERROR`
alone (or combined with a clean payload) does not exclude a frame. **See the
2026-07-17 Correction below: the guard originally shipped in this ADR as the
stricter `rx_status_flags == 0` (all 5 bits clear), which was wrong — that
version is superseded by the carve-out described here and in Consequences.**
This condition supersedes ADR-097's `!start_of_message` guard and excludes
TxDone, RxBreak, and Loopback frames from both the mask/pattern-matching
sub-branch and the `detect_pending_rc` sub-branch — closing the
phantom-pending-RC hazard described above, since `detect_pending_rc` is never
called for a TxDone/RxBreak/SOM/Loopback frame. As with ADR-097, this is a
narrower, more targeted fix than changing `ExpectedResponse::matches` itself:
these frame kinds are indications, not responses, so excluding them by kind
is correct regardless of what any particular descriptor's mask/pattern (or
`CP_RCByteOffset` value) would have matched. RxPadError is not excluded: see
Correction.

`FcCapture`'s capture condition in `poll_rx_inner` gains
`&& msg.rx_status() & RX_TX_MSG_TYPE == 0` — a CONFIG_LOOPBACK echo of our
own transmitted FlowControl frame is never captured as the awaited
incoming one, regardless of whether the echoed frame's CAN ID and byte
content otherwise match. This is checked directly against the live
`msg.rx_status()`, not `rx_status_flags`, since `FcCapture`'s withhold-from-
fan-out decision happens before the frame reaches `ReceivedFrame`
construction.

Loopback is now tagged via `RxFlag` byte 3 bit 0 (`TX_MSG_TYPE`) whenever a
Loopback frame is delivered as an ordinary unsolicited indication (every
case except when the `FcCapture` guard above withholds it entirely from
fan-out). This narrows ADR-061's original "not filtered or tagged" gap for
`TX_MSG_TYPE` to "tagged, not filtered": this ADR does not add any logic
that drops or specially routes a loopback echo before delivery (other than
the pre-existing `FcCapture` withholding and the pre-existing
`has_external_activity` idle-timer exclusion, both unrelated to
delivery/dropping of the frame itself) — filtering loopback frames out of
delivery entirely would be a behavior change beyond what was requested here;
the client, now that it can see the tag, is free to decide what to do with
a loopback indication.

`tx_msg_done_timestamp`/`start_msg_timestamp` (`ResultData`'s two dedicated
timing fields ISO 22900-2 defines alongside `RxFlag`) remain unpopulated —
out of scope for this ADR. Frame timing already reaches clients via
`EventItem.timestamp`; populating these two more specific fields (e.g. from
a TxDone indication's own timestamp) is recorded as backlog, not
implemented here (see Consequences and
`j2534-0404-service/docs/implementation-notes.md`). (resolved by ADR-143)

`RxFlag` bits 5-8 (`CAN_29BIT_ID`, `ISO15765_ADDR_TYPE`, and the five
`RxStatus`-less concepts `REMOTE_FRAME`/`SPD_CHG_EVENT`/`ECU_TIMING_CHANGE`/
`CAN_SEGMENTATION`/`SW_CAN_HV_RX`) remain out of scope, unchanged from
ADR-097's own residual.

## Consequences

- Fixes a real false-completion bug: before this ADR, any pending
  native-ISO15765 `CoptSendrecv`/pending-RC wait with a broad or empty
  `expected_response` descriptor would vacuously complete on a TxDone
  indication's empty post-split payload, exactly as ADR-097 already fixed
  for SOM frames specifically.
- Fixes a real phantom-pending-RC bug: a CONFIG_LOOPBACK echo of our own
  request could previously be misread by `RcHandlingConfig::detect_pending_rc`
  as a genuine pending NRC when the request's own byte at `CP_RCByteOffset`
  happened to equal 0x78/0x21/0x23 (e.g. SID 0x23, ReadMemoryByAddress) and
  the corresponding `CP_RC*Handling` was enabled — extending the COP's
  deadline and attributing `cop_handle` to a frame that was never a genuine
  ECU response.
- Fixes a related TX-side bug in the software-ISO-TP driver: `FcCapture`
  could previously be fooled by a CONFIG_LOOPBACK echo of this service's
  own outbound FlowControl frame (sent in reply to an incoming segmented
  message) into releasing an unrelated outbound segmented request's
  withheld ConsecutiveFrames early.
- Accepted residual: `tx_msg_done_timestamp`/`start_msg_timestamp` still
  unpopulated — tracked as a backlog item in
  `j2534-0404-service/docs/implementation-notes.md`, not implemented here.
  (resolved by ADR-143)
- Accepted residual, carried over from ADR-097: `RxFlag` byte 2 bit 0
  (`CAN_29BIT_ID`) and byte 3 bit 7 (`ISO15765_ADDR_TYPE`) still read `0`
  always — not part of the 5-bit scope this ADR covers either.
- Software-ISO-TP reassembly (`RxEntryKind::SoftwareIsoTp`,
  `process_frame_for_entry`) still does not synthesize its own SOM/TxDone/
  RxBreak/RxPadError indications for a reassembled or raw-passthrough
  frame it emits — same latent gap ADR-097 already noted, unwidened by this
  ADR.
- No test asserted on more than the SOM (`0x02`) and empty (`Vec::new()`)
  `rx_flag` values before this ADR; tests now additionally cover TxDone
  (`0x09`), Loopback-only (`0x01`), RxBreak (`0x04`), RxPadError (`0x10`,
  including a degenerate too-short-for-header frame), the `ExpectedResponse`/
  pending-RC exclusion for TxDone and the loopback phantom-pending-RC case,
  and the `FcCapture` loopback-echo exclusion.
- **Superseded by the 2026-07-17 Correction below**: the blanket
  `rx_status_flags == 0` guard as originally accepted also excluded
  `RxPadError` frames from `ExpectedResponse`/pending-RC eligibility. That
  part of the decision was wrong and is corrected in place (see Correction);
  it is listed here only for the historical record.

## Correction (2026-07-17)

The guard as originally accepted (`rx_status_flags == 0`, all 5 bits clear)
incorrectly treated `RX_ISO15765_PADDING_ERROR` (bit 4, `0x10`) the same as
the other 4 bits. It differs in kind: `ISO15765_PADDING_ERROR` tags a
genuine, fully reassembled ISO15765 *response* whose final CAN frame simply
had fewer than 8 data bytes — not a header-only/empty-payload *indication*
like SOM, TxDone, RxBreak, or Loopback. Excluding it from matching had two
consequences:

1. A legitimate, byte-matching ECU response tagged with the padding-error bit
   never completed its pending `CoptSendrecv` wait — it timed out at
   `CP_P2Max` instead of being delivered as the awaited response.
2. Systemically worse: a `7F SS 78` (response-pending NRC) frame is exactly 3
   bytes, so any ECU that does not pad its CAN frames always sends it on a
   CAN frame with fewer than 8 data bytes, setting bit 4 unconditionally.
   `detect_pending_rc` was therefore never reached for such an ECU at all —
   `CP_RC78Handling` pending-RC extension was systematically broken for it,
   not merely a single-response edge case.

The guard is corrected to `rx_status_flags & !RX_ISO15765_PADDING_ERROR == 0`:
a frame is excluded only if one of the 4 genuinely indication-type bits
(`TX_MSG_TYPE`, `START_OF_MESSAGE`, `RX_BREAK`, `TX_INDICATION`) is set,
regardless of whether `ISO15765_PADDING_ERROR` also happens to be set. A
combination that also sets one of those 4 bits (e.g. `PAD|LOOPBACK = 0x11`)
remains excluded, unchanged from the original decision.

Accepted residual, carried forward: a malformed padding-error frame with an
empty post-split payload (e.g. the degenerate too-short-for-header case) could
still vacuously match an empty/broad `expected_response` descriptor. This is
the same risk an equally-shaped Normal Message (`rx_status_flags == 0`, also
never excluded) already carries — not a new exposure introduced by this
correction.

`rx_flag_bytes` and the `FcCapture` capture guard are unaffected by this
correction: `rx_flag_bytes` already bit-copies all 5 bits unconditionally
regardless of matching eligibility, and `FcCapture`'s guard operates on the
raw-CAN software-ISO-TP path where `ISO15765_PADDING_ERROR` (an
ISO15765-protocol bit) is never set.

## Supersession

Supersedes [ADR-097](ADR-097-iso22900-2-rxflag-start-of-message.md) in
full: ADR-097's `START_OF_MESSAGE`-only scope is subsumed by this ADR's
5-bit encoding, and its `!start_of_message` `MatchProbe` guard is replaced
by this ADR's `rx_status_flags == 0` guard. ADR-097's own byte-compatibility
analysis (the `RxFlag`/`RxStatus` bit-position table) and its documentation
of the routing (`header_footer_len`) behavior for SOM's header-only/empty
`Data` shapes are not revisited here — both still hold and are reproduced
in this ADR's Context only where directly relevant. ADR-097's own
supersession of ADR-061 also still holds unmodified.
