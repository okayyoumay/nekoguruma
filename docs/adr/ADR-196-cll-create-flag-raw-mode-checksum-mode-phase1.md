# ADR-196: `CllCreateFlag` RawMode/ChecksumMode Support, Phase 1 (CAN Family)

**Date:** 2026-08-28
**Status:** Accepted (Decision item 2 partially amends ADR-062's "objective fact,
             service-derived" rule for `TX_FLAG_CAN_29BIT_ID`/`TX_FLAG_ISO15765_ADDR_TYPE`,
             scoped to RawMode=ON CLLs only; ADR-050/ADR-051 gain a RawMode-scoping
             annotation, decisions otherwise unchanged. Decision item 1's protocol
             allowlist is extended to hardware K-line by ADR-198 Phase 2, which also
             supersedes Decision item 3b's narrow claim that `AccessTimingConfig::
             with_request` needs no `tx_prefix` parameter/is unreachable under RawMode
             -- that call site is reachable once K-line RawMode ships, exactly as this
             Decision item's own "revisit only if Phase 2 ever extends RawMode to
             K-line" note anticipated. ADR-200 Phase 3 further extends Decision item 1's
             allowlist to SAE J1850 (VPW/PWM, via the same generic RawMode passthrough,
             no mechanism change) and SAE J1939 (via a NEW protocol-specific TX/RX shim,
             NOT the generic passthrough Decision item 2's own "`build_tx_message`'s raw
             branch is protocol-agnostic" claim describes -- that claim is now false for
             SAE J1939 specifically, see ADR-200), and closes TP2.0 out as a permanent
             exclusion rather than a residual; every other Decision item is unaffected)
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`rpc_create_com_logical_link`),
             `j2534-0404-service/src/service.rs` (`LogicalLinkState`, `RcHandlingConfig::
             detect_pending_rc`/`is_unhandled_negative`, `SessionTimingConfig::with_request`),
             `j2534-0404-service/src/service/tx_header.rs` (`build_tx_message`),
             `j2534-0404-service/src/service/rpc_primitive.rs` (`compute_j2534_tx_flags`,
             `resolve_send_recv_tx`, `request_sid` capture), `j2534-0404-service/src/service/events.rs`
             (`poll_rx_inner`'s header/footer split call site, `classify_queue_error`,
             `observe_session_timing_response`), `docs/rpc-api-guide.md`,
             `j2534-0404-service/docs/comparam-protocol-support.md`

## Context

`CreateComLogicalLinkRequest.cll_create_flag_bits`/`cll_create_flag_raw`
(`vci-service-interface/src/proto/service.proto:704-705`, the `CllCreateFlag`
enum: `CLL_CREATE_FLAG_CHECKSUM_MODE`, `CLL_CREATE_FLAG_RAW_MODE`) have been
accepted structurally since the proto's introduction but never read anywhere
in `j2534-0404-service` — confirmed by direct grep, zero references in
`rpc_create_com_logical_link` or elsewhere in the crate
(`j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog,
"found while answering a user question about RAW mode support" and its
sibling ChecksumMode entry). A client that requests RawMode/ChecksumMode
gets ordinary (non-raw) behavior with no error or indication the flag was
ignored.

ISO 22900-2:2022 Annex D.2.3 (Table D.6, byte 0 bits 7/6) defines these two
CLL-creation-time flags:

- **RawMode** (default OFF): when OFF, the D-PDU API builds header bytes and
  checksums onto outgoing messages before transmission and strips them from
  incoming messages before delivery to the client (this is exactly what this
  service already does unconditionally, for every protocol — ADR-050 on the
  TX side, ADR-051 on the RX side). When ON, the client is responsible for
  including header bytes/checksums directly in the COP payload it transmits,
  and the service leaves them attached in what it delivers.
- **ChecksumMode** (default OFF, and ignored entirely whenever RawMode is
  OFF, or for a protocol with no checksum concept): when ON, for
  checksum-using UART-family protocols specifically, the D-PDU API still
  appends/validates/strips the checksum on the client's behalf even though
  RawMode is otherwise leaving headers/footers untouched.

`vci-service-interface/src/proto/service.proto` is frozen (ADR-178): this
design must be delivered entirely through the existing
`cll_create_flag_bits`/`cll_create_flag_raw`/`TxFlagBit` proto surface, with
no new fields or messages.

A `design-advisor` consult (this ADR's own analysis) also surfaced a
pre-existing wrinkle relevant to RawMode's TX side: `TX_FLAG_CAN_29BIT_ID`
and `TX_FLAG_ISO15765_ADDR_TYPE` are documented by ISO 22900-2:2022 Annex
D.2.1 (Table D.4) as meaningful "RAW_MODE Only" TxFlag bits, but `rpc_primitive.rs`'s
`compute_j2534_tx_flags` (ADR-062) already deliberately **discards** any
client-supplied value on those exact two bits and re-derives them from
ComParams instead, on the reasoning that outside RawMode they describe
objective facts about a message this service itself constructs, not client
input. That reasoning holds only when the service is the one building the
header — it does not hold once RawMode hands header construction to the
client.

Two related, differently-shaped proto-acceptance gaps were found in the same
discovery pass (`implementation-notes.md`'s "found via the same RAW-mode
sweep"/"found via the same sweep" entries): `cll_tag` (event-delivery
correlation, no coupling to Table D.6, and closing it correctly would need
`EventItem` to grow a new echo field — a direct collision with ADR-178's
freeze that deserves its own separate consult) is explicitly **out of
scope** for this ADR.

## Decision

**1. RawMode is a per-CLL boolean, captured once at `CreateComLogicalLink`
time**, read from `cll_create_flag_bits`/`cll_create_flag_raw` byte 0 bit 7
(`rpc_create_com_logical_link`, `rpc_link.rs`) and stored on
`LogicalLinkState`. Any nonzero reserved bit in byte 0 bits 5-0 or bytes 1-3
(Table D.6: all Unused) is rejected `invalid_argument` at create time,
rather than silently accepted — this phase does not repeat the
silently-ignored-flag defect it closes for bits 6/7.

**Phase 1 supports RawMode=ON only for base CAN and hardware ISO15765.**
Every other protocol (every K-line/UART-family protocol, J1850, SCI aside
below, SAE J1939, TP2.0), and any CLL running in software-ISO-TP mode
(`ChannelKey`'s `software_isotp` flag, already resolved at create time,
`rpc_link.rs`), rejects RawMode=ON at `CreateComLogicalLink` with a clear
`PDU_ERR_ID_NOT_SUPPORTED`-shaped rejection rather than a silent no-op.
Analog Inputs/SCI is a documented exception: it may accept RawMode=ON as a
behavioral no-op, since its TX path is already a raw passthrough (ADR-050)
and its `header_footer_len` result is already `(0, 0)` — RawMode ON/OFF are
observably identical for it already.

**2. TX: when RawMode is ON, `tx_header::build_tx_message`'s header
construction is skipped entirely** — `cop_data`/`CP_TesterPresentMsg` is
treated as the literal `PassThruMessage.Data` the client already assembled
(CAN ID in bytes 0-3, per Table D.4), validated against the existing SAE
J2534-1 TX size range (ADR-049) directly against that raw buffer rather than
a constructed one. `compute_j2534_tx_flags` (ADR-062) is amended, scoped
strictly to RawMode=ON CLLs: `TX_FLAG_CAN_29BIT_ID`/`TX_FLAG_ISO15765_ADDR_TYPE`
become client-authoritative (mapped through to native `TX_EXTENDED_ID`/
`ISO15765_ADDR_TYPE` as the client sets them) instead of service-derived and
discarded, since the service can no longer derive facts about a header it
did not build. RawMode=OFF keeps ADR-062's existing rule unchanged and
still conformant — outside RawMode those bits are meaningless as client
input per Table D.4, so there is nothing for the existing discard-and-derive
behavior to conflict with.

**3. RX: when RawMode is ON, the header/footer split
(`events::header_footer_len`, called from `poll_rx_inner`, ADR-051) is
skipped** — the full received frame is delivered as `ResultData.data_bytes`
with `extra_info` `None` (mirroring `ENABLE_EXTRA_INFO`'s own existing
"no split requested" shape). No change is needed to RX routing
(`route_frame` already matches CAN IDs against the wire frame before any
split happens) or to expected-response matching: masks/patterns already run
against the post-split payload (ADR-051), so running them against the
whole, unsplit frame in RawMode is the same mechanism applied to a
differently-shaped input — exactly Table 80's own RawMode expected-response
rule (§10.1.4.19.5).

**3b. Internal UDS negative-response anchors are re-based by the per-frame
raw prefix width, not disabled** (`edge-case-hunter` finding, this PR's own
close-out pass): a post-implementation audit found four internal consumers
that assume a response's logical start is `data_bytes[0]` — `service.rs`'s
`RcHandlingConfig::detect_pending_rc`/`is_unhandled_negative` (the
`0x7F`-at-position-0 / SID-echo-at-position-1 gate feeding RC78/21/23
auto-handling), `events.rs`'s `classify_queue_error` (`CP_SuspendQueueOnError`,
ADR-147) and `observe_session_timing_response` (`CP_ModifyTiming`
auto-derivation, ADR-150) — plus `request_sid`'s own capture
(`rpc_primitive.rs`, three call sites) and `SessionTimingConfig`'s TX-side
session capture (`service.rs`). For a RawMode=OFF CLL these positions are
correct by construction (the service always stripped the header first,
ADR-051); for a RawMode=ON CLL they are not, since `data_bytes` now carries
the frame's own CAN-ID prefix (4 bytes, or 5 with an Address Extension) --
silently misclassifying a genuine negative response as an ordinary positive
one, since the CAN-ID byte at position 0 is essentially never `0x7F`.

Fixed by re-basing each internal anchor to the exact per-frame prefix width
`events::header_footer_len` already computes for the (now-skipped) RX split
above -- reused, not re-derived, so this inherits ADR-051/167/171/179's
already-audited correctness rather than opening a second, possibly
diverging notion of "how wide is this frame's header." The RawMode RX path
still calls `header_footer_len` and keeps its result as a
classification-only `raw_prefix` value (the actual split stays `(0, 0)`,
per Decision item 3 above -- only the four detectors' own byte anchors
move). `CP_RCByteOffset` (`RcHandlingConfig::rc_byte_offset`) is NOT
re-interpreted -- it stays exactly what Decision item 3's mask/pattern
matching already established: relative to `data_bytes` as delivered, i.e.
raw-relative on a RawMode CLL, matching Table 80's own RawMode expected-
response rule. Only the service's OWN internal `0x7F`/SID-echo anchors move
to `raw_prefix`/`raw_prefix + 1`; the existing gate condition becomes
`rc_byte_offset.checked_sub(raw_prefix)` -- `None` (an offset that falls
inside the raw prefix itself, not a real RC-byte position) declines to
classify, matching this mechanism's own existing "decline rather than risk
a false positive" philosophy (ADR-147); `Some(logical_offset)` runs the
unchanged existing body, with the ADR-100 `rc_byte_offset < 2` carve-out
shifted intact (now `logical_offset < 2`, i.e. `raw_prefix..raw_prefix + 2`
ungated) -- a RawMode client using a non-UDS RC-first framing is the exact
raw-relative analog of today's carve-out user, not a case to regress. TX
side: `request_sid`'s capture and `SessionTimingConfig::with_request`'s
session-response snapshot move to `tx_prefix` (0 for RawMode=OFF, 4 or 5
for RawMode=ON, derived from the RESOLVED TX flags at each call site --
`TX_FLAG_ISO15765_ADDR_TYPE` is client-authoritative under RawMode per
Decision item 2 -- via one shared helper so the three call sites cannot
drift apart). KWP's own `AccessTimingConfig::with_request` needs no change:
K-line RawMode is already rejected at `CreateComLogicalLink` (Decision item
1's protocol allowlist), so this call site is unreachable for a RawMode
CLL in Phase 1; revisit only if Phase 2 ever extends RawMode to K-line.

**Correction (round-3 `edge-case-hunter` audit, same close-out pass):** the
shared `tx_prefix` helper (`rpc_primitive::compute_tx_prefix`) initially
widened to 5 by comparing `hw_protocol_id` against `j2534_0404::ISO15765`
with raw equality, and had no case at all for Analog Inputs/SCI. Both were
wrong: (1) a RawMode=ON hardware-ISO15765 CLL's `hw_protocol_id` can be
mutated post-create to the `_PS`-qualified `PROTOCOL_FD_ISO15765_PS` by
`J2534Service::apply_fd_mode` (staged `CP_CANFDTxMaxDataLength`/
`CP_CANFDBaudrate`, which applies regardless of `raw_mode`), so the
comparison must go through `resources::base_protocol_id` — the same
ADR-157 Plane B normalization `resolve_send_recv_tx` and the RX-side
`header_footer_len` anchor already apply; (2) Decision item 1's Analog
Inputs/SCI no-op exception means `tx_prefix` must resolve to `0` for those
ids (matching `header_footer_len`'s own `(0, 0)` catch-all for them), not
fall through to the plain-CAN 4-byte default, which would desync
`request_sid`'s anchor by 4 bytes on a link with no header to skip at all.
Fixed in the same helper before this PR's own review; both cases now have
dedicated unit coverage in `compute_tx_prefix_tests`.

Rejected alternatives: disabling RC-handling/`CP_SuspendQueueOnError`/
`CP_ModifyTiming` auto-derivation entirely for RawMode CLLs (leaves them
dead in RawMode ISO15765's flagship reprogramming use case, where NRC 0x78
storms during reflash are exactly what these mechanisms exist to absorb --
no spec basis scopes them out of RawMode CLLs, so this would be a silent,
self-inflicted conformance hole); rejecting RawMode together with an
already-enabled RC-handling ComParam at create/`SetComParam` time
(invents a restriction the spec doesn't have, and RC ComParams are
legitimately toggled mid-link, ADR-067, so it isn't cleanly enforceable
anyway); minting a new "response start offset" ComParam (the proto is
frozen, ADR-178, and the value is already fully derivable from
`header_footer_len`, so a client-set knob could only contradict the
derivable truth).

**4. ChecksumMode needs no new mechanism in Phase 1 and remains fully
conformant as a no-op.** Table D.6 states it is ignored whenever RawMode is
OFF or for a checksumless protocol; Phase 1's only RawMode-eligible
protocols (CAN, hardware ISO15765) have no checksum concept, so
ChecksumMode is inert for every CLL this phase can create with RawMode=ON.
Its real semantics — selecting whether a K-line checksum stays
device-managed (ON, closest to today's existing ADR-065 connect-flag
behavior) or is left to the client (OFF, needing an `ISO9141_NO_CHECKSUM`-
shaped connect flag plus a shared-channel join-compatibility check) — are
recorded as a Phase 2 residual below, not implemented here.

**5. `cll_tag` is explicitly out of scope for this ADR** — a different
mechanism (event-delivery correlation) with no coupling to RawMode/
ChecksumMode. (`cll_tag` was later removed outright by ADR-204, which is now
the durable record for it.)

### Alternatives rejected

- **Full-protocol RawMode in one PR.** K-line/J1850 RawMode needs its own
  connect-flag derivation work (ADR-065), a shared-channel join-compatibility
  check, and per-protocol verification against Table 80's RX rules —
  disproportionate blast radius for one change; phased the same way this
  crate has phased every other SAE J2534-2 feature area.
- **Continuing to silently no-op the flag.** That is the defect this ADR
  closes, not an option.
- **Emulating raw K-line by re-synthesizing checksums on this service's own
  hardware-checksum-managing path.** Would misrepresent the wire bytes
  RawMode promises the client; deferred to the real connect-flag-driven
  Phase 2 route instead.
- **Repurposing `cll_create_flag_raw`'s byte layout for anything beyond
  Table D.6.** ADR-178 explicitly reserved that field for its native
  ISO 22900-2 meaning; inventing a side-channel encoding inside it would
  violate the freeze's own intent.

## Consequences

- **ADR-050**/**ADR-051** gain a Status annotation: TX header construction /
  RX header-footer split apply to RawMode=OFF CLLs only; RawMode=ON CLLs
  follow this ADR's Decision items 2/3 instead. Neither ADR's own Decision
  text changes otherwise.
- **ADR-062** gains a Status annotation: its "service-derived,
  client-input-discarded" rule for `TX_FLAG_CAN_29BIT_ID`/
  `TX_FLAG_ISO15765_ADDR_TYPE` is scoped to RawMode=OFF CLLs; RawMode=ON
  CLLs follow this ADR's Decision item 2 instead (client-authoritative).
- **Documented Phase 2 residual** (a new backlog bullet, replacing the two
  closed RawMode/ChecksumMode entries): RawMode for K-line/J1850/J1939/
  TP2.0 protocols, and ChecksumMode's real (non-no-op) connect-flag-driven
  semantics for those protocols, once picked up.
- **Documented residual, RAW-only TxFlag bits already reachable in Phase 1
  vs. deferred:** `TX_FLAG_ISO15765_FRAME_PAD` and `TX_FLAG_WAIT_P3_MIN_ONLY`
  are already mapped to native flags elsewhere in `rpc_primitive.rs`
  (pre-existing, unrelated to this ADR); `WAIT_P3_MIN_ONLY` is ISO14230-only
  and is unreachable in Phase 1 anyway, since K-line RawMode is rejected at
  create time — no new gap opened by this phase.
- **`edge-case-hunter`'s post-implementation audit (this PR's own close-out
  pass) ran the `data_bytes[0]`-assumes-SID sweep this ADR originally
  flagged as future work.** ISO15765 concatenation keying is unreachable
  under RawMode (`CP_EnableConcatenation` is KWP/J1850-family only, mutually
  exclusive with Phase 1's CAN/ISO15765-only RawMode scope) and `route_frame`/
  `ExpectedResponse::matches`/the tester-present discard signature are
  genuinely unaffected (purely client-configured byte patterns, no internal
  fixed offset) — confirmed by direct code reading, not just this ADR's own
  reasoning. The sweep DID find four further internal consumers this ADR's
  first draft had not anticipated (`RcHandlingConfig::detect_pending_rc`/
  `is_unhandled_negative`, `classify_queue_error`, `observe_session_timing_
  response`, plus `request_sid`/`SessionTimingConfig` capture) — see Decision
  item 3b above for the fix.
- **Correction (round-6 Codex review, same PR close-out): the residual just
  above was itself under-scoped and has since been fixed, not left as-is.**
  The ISO15765 TX size-range check's `extended_addressing` value did come
  from ComParam-derived addressing under RawMode — but the actual
  consequence was broader than "misclassifies only at the 4100-byte upper
  boundary": since `tx_addressing` silently defaults to `Normal` when no
  `CP_Can*Format` is configured (the RawMode-typical case), a genuinely
  extended-addressed RawMode message missing its required AE byte (only 4
  raw bytes) would incorrectly pass the *Normal* range's `4..=4099` floor
  instead of being rejected against the *Extended* range's `5..=4100`
  floor — a lower-bound gap, not just an upper-bound one. Fixed by deriving
  `extended_addressing` from `tx_prefix == 5` (`compute_tx_prefix`, the same
  shared helper Decision item 3b already established) when `raw_mode` is
  true, computed once and reused by both this check and the functional
  Single Frame check below it.
- `docs/rpc-api-guide.md`'s existing `CllCreateFlag` bullet list and
  `j2534-0404-service/docs/comparam-protocol-support.md` are updated to
  describe the now-implemented Phase 1 behavior and the Phase 2 residual.
- `cll_tag` was removed outright by ADR-204 (gRPC event-correlation tags:
  `cop_tag` added, `cll_tag` deleted), which is now the durable record for
  it — no backlog entry tracks it any longer.
