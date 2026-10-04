# ADR-222: `UniqueRespIdTable` RX Matching Gains CAN-ID-Width Disambiguation for Contended Ids

**Date:** 2026-09-09
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/events.rs` (`FrameRouteContext`,
             `RxEntryKind::Hardware` (`uudt_on_companion`),
             `process_frame_for_entry`, `FcPair`, `usdt_addressing_by_id`/
             `uudt_addressing_by_id` construction, `header_footer_len`'s
             consumer, `poll_rx_inner`'s per-message `RxStatus` read and
             `CanChannelMode` snapshot),
             `j2534-0404-service/src/service.rs` (`LogicalLinkState::isotp_rx`),
             `j2534-0404-service/src/service/events_rx_routing.rs`
             (`WidthDomain`, `is_native_mixed_link`, `link_is_qualified`,
             `is_dual_channel_link`, `width_domains`,
             `UniqueRespIdKey`, `UniqueRespIdKey::matched`,
             `route_frame_with_role`, `route_frame_uudt_only`,
             `build_cll_rx_entries`),
             `j2534-0404-service/src/service/rpc_link.rs`
             (`PointToPointFilterEligibility`,
             `point_to_point_filter_eligibility`,
             `install_point_to_point_fc_filters`)

## Context

`j2534-0404-service`'s RX attribution matches an incoming CAN frame to a CLL
purely by numeric CAN ID: `UniqueRespIdKey::matched` compares `can_id`
against a table entry's `CP_CanRespUSDTId`/`CP_CanRespUUDTId`, and
`route_frame_uudt_only` (the Companion-channel and native-mixed raw-CAN-tagged
path) does the same for `CP_CanRespUUDTId` alone. Neither reads SAE
J2534-1's `RxStatus` bit 8 (`CAN_29BIT_ID`, Figure 43) — the wire fact that
tells 11-bit `0x123` and 29-bit `0x00000123` apart.

[ADR-061](ADR-061-rxstatus-rxflag-no-direct-passthrough.md) audited every
`RxStatus` read in the crate and found none feeding CAN-ID/addressing
interpretation, concluding there was "no live ambiguity" because hardware
ID+mask filters already constrained a received frame to exactly one
CAN-ID+width combination before this service's own matching ran. That
finding was carried forward unmodified through
[ADR-097](ADR-097-iso22900-2-rxflag-start-of-message.md) and
[ADR-098](ADR-098-rxflag-all-status-bits.md) (both otherwise about
an unrelated topic, `ResultData.rx_flag` encoding) and is cited again in
ADR-098's own Decision as the reason `RxStatus` bits are treated as opaque
forwarded payload, not decoded.

The premise no longer holds in two situations that did not exist, or were
not analyzed against it, when ADR-061 was written:

1. **`CAN_ID_BOTH` raw-CAN channels.**
   [ADR-065](ADR-065-passthruconnect-flag-derivation.md) makes every raw-CAN
   channel (a raw-`CAN`-protocol CLL's primary channel, an ISO15765-family
   CLL running in `software-isotp` mode, and the ADR-046 UUDT companion
   channel) always connect with `CAN_ID_BOTH` and install one pass-all
   `PASS_FILTER` per width — deliberately a wide-open receive path, with
   `UniqueRespIdTable` matching doing "the real filtering" (ADR-065's own
   words). Two entries configuring the same numeric id at different widths
   on such a channel both pass the hardware filter; only this service's own
   (currently width-blind) matching decides which entry wins.
2. **Per-entry point-to-point FC filters on native ISO15765 hardware.**
   [ADR-048](ADR-048-connect-time-filters-from-unique-resp-id-table.md)'s
   `install_point_to_point_fc_filters` installs one width-specific
   `FLOW_CONTROL_FILTER` *per table entry*, not one per channel. Two native
   ISO15765 entries sharing a numeric id at different widths (e.g. USDT
   `0x123` 11-bit on entry A, USDT `0x123` 29-bit on entry B) each get their
   own filter, so the channel receives both — ADR-065's "no shared wide-open
   receive path" observation is true of the *filters*, but says nothing
   about *attribution* once both filters' frames arrive. ADR-061's "hardware
   already constrains to one exact ID+format" premise assumed a table never
   contains this shape; it does not hold when it does.

Concrete collision: entry A (`CP_CanRespUUDTId = 0x123`, `CP_CanRespUUDTFormat`
bit 1 clear -> 11-bit) and entry B (`CP_CanRespUUDTId = 0x123`,
`CP_CanRespUUDTFormat` bit 1 set -> 29-bit) on the same physical channel. A
29-bit frame numerically `0x00000123` is today attributed to A (the first/
only numeric match) instead of B.

This was escalated to `design-advisor` given the protocol-interpretation
judgment call involved (which entries are actually affected, and how to
avoid regressing single-width configurations that never set the Format
ComParam) — see that agent's brief and decision for the full reasoning this
ADR summarizes.

## Decision

**Every frame carries its own width.** `poll_rx_inner` reads `RxStatus` bit
8 (`CAN_29BIT_ID_STATUS`, already exported as `j2534-0404`'s
`CAN_29BIT_ID_STATUS` constant) once per polled message, alongside the
existing bit 0-4/bit-7 reads, and threads it as a new `frame_is_29bit: bool`
field on `FrameRouteContext`.

**Each `UniqueRespIdKey` field gets a width gate, computed once per poll
pass in `build_cll_rx_entries`, not read per-frame.** A new
`CanIdWidthGate` enum — `Any`, `Bits11`, `Bits29` — is derived per numeric
id as follows:

1. Decode each entry's configured width from its Format ComParam using the
   same rule as `tx_header::can_29bit_id`/`rpc_link::CanIdFormat::extended_can_id`
   (`format_raw.is_some_and(|raw| raw & 0x02 != 0)`; absent Format = 11-bit):
   `CP_CanRespUSDTFormat` for `can_resp_usdt_id`, `CP_CanRespUUDTFormat` for
   `can_resp_uudt_id`.
2. **Contention is computed per *frame population*, not per physical channel
   alone** (design-advisor audit, round 4, Codex review finding, PR #141 —
   superseding this Decision's original single-combined-map wording, which
   two earlier rounds of this same PR already patched piecemeal before this
   rule replaced them outright). `process_frame_for_entry` compares a
   numeric id against exactly two distinct, and on some entries genuinely
   disjoint, frame populations: **`iso`** (ISO15765-tagged frames — the
   USDT/ISO-tagged `Hardware` arm and the `SoftwareIsoTp` arm) and **`can`**
   (non-ISO15765-tagged frames — a raw-CAN `PASS_FILTER`/companion-channel
   match). `build_cll_rx_entries` builds two maps, `iso_widths_seen` and
   `can_widths_seen` (same shape as the single map this replaces), and
   classifies each visited link `l`'s two fields into a `WidthDomain`
   (`Iso`, `Can`, `Both`, or `Neither`) that decides which map(s) each
   field's `(id, width)` observation is inserted into — and, later, which
   map(s) that SAME field's own gate is resolved from (one classification
   per link, computed once and reused for both contribution and every
   resolution site, never recomputed independently — see below):

   | This link `l`, building for `channel_id` | USDT domain | UUDT domain |
   |---|---|---|
   | primary (`l.channel_id == channel_id`), `l.software_isotp` | `Both` | `Both` |
   | primary, native-mixed (`mode_is_native_mixed` and `l`'s own protocol/pin/channel-index qualify — the exact predicate `RxEntryKind::Hardware`'s `native_mixed` field already used) | `Iso` | `Can` |
   | primary, `l.uudt_channel_id.is_some()` (ADR-046 dual-channel primary) | `Both` | `Neither` |
   | primary, none of the above (plain single-channel/ADR-041/qualified/FD/KWP/J1939) | `Both` | `Both` |
   | companion (`l.uudt_channel_id == channel_id`) | `Neither` | `Both` |

   `Both` means the field's `(id, width)` observation is inserted into (and
   its own gate resolved as contended by) EITHER map — mirroring this
   Decision's original cross-field reasoning (a numeric id shared between
   one entry's USDT field and another's UUDT field is exactly as ambiguous
   as a same-field collision), now scoped to only the population(s) that
   genuinely compare the two. `Iso`/`Can` restrict a field to one map only.
   `Neither` means the field never contributes and its own gate is always
   `Any` (inert either way, since the field never legitimately arrives on
   this channel at all — a dual-channel primary's own UUDT field, which
   ADR-046 never installs a filter for on the primary).

   The native-mixed and dual-channel-primary rows exist because
   `uudt_eligible` (below) structurally prevents `matched` from ever
   comparing that field against the OTHER population's frames at all —
   treating them as contended anyway (an earlier version of this fix did,
   for native-mixed specifically) recreates exactly the false collision
   [ADR-162](ADR-162-native-mixed-client-filter-and-uudt-usdt-collision.md)
   Decision 2's `find_native_mixed_uudt_usdt_collision` already exists to
   reject at connect time, in this newer RX-gating mechanism that precedent
   doesn't cover: it wrongly turns an `Any` gate
   into a strict width check for a numeric id that, in routing terms, was
   never actually shared with anything, dropping a valid frame on any
   adapter that omits or misreports `RxStatus` bit 8.

   The link-membership test that decides which CLLs are visited at all
   (`l.channel_id == Some(channel_id) || l.uudt_channel_id ==
   Some(channel_id)`) is unchanged from the round-1 P2 fix — only what each
   visited link's two fields are classified as, and which map(s) they
   populate/resolve from, is domain-scoped.
3. A numeric id is **contended** within a domain iff it appears at both
   widths anywhere in that domain's own map(s). A contended field's gate is
   its own decoded width (`Bits11`/`Bits29`); an uncontended field's gate is
   `Any` — matches a frame of either width, identical to today's behavior.
4. **Contribution ALSO requires the field to be filter-admission-eligible on
   a native ISO15765/native-mixed hardware channel** (design-advisor audit,
   round 6, Codex review finding, PR #141): a table entry's USDT/UUDT field
   only genuinely risks contention if it could ever actually receive a real
   frame of its own configured width — which, on native hardware, requires
   `rpc_link.rs`'s `install_point_to_point_fc_filters` to have installed a
   point-to-point filter for it at all. A new shared predicate,
   `point_to_point_filter_eligibility(entry, dual_channel, native_mixed) ->
   { usdt: bool, uudt: bool }` (`rpc_link.rs`, the single source of truth
   `install_point_to_point_fc_filters` itself now calls too, rather than
   re-deriving the same go/no-go decision inline a second time), mirrors
   that function's own per-field install decision exactly:
   - **USDT**: eligible iff `usdt_resp_id(entry)` is configured (not the
     `0xFFFFFFFF` "not used" sentinel), the entry's `CP_CanRespUSDTFormat`
     has flow control enabled (bit 0 — absent Format defaults to enabled),
     and the entry configures a `CP_CanPhysReqId` (key presence only).
   - **UUDT**: eligible iff `uudt_resp_id(entry)` is configured, the link is
     NOT a genuine dual-channel-mode primary (UUDT is received on the
     ADR-046 companion there instead — no primary-channel filter is ever
     installed for it, mirroring `uudt_domain`'s own `Neither` classification
     above but computed independently, since "does this link have a
     companion" and "is this entry's own field eligible" are different
     questions answered at different granularity — link-wide vs.
     per-entry), and either the link is native-mixed (an unconditional
     `PASS_FILTER` is installed there regardless of `CP_CanPhysReqId`) or
     the entry configures a `CP_CanPhysReqId` (the ADR-041 workaround's own
     `FLOW_CONTROL_FILTER` requires it). UUDT's own Format's flow-control
     bit is never consulted — `install_point_to_point_fc_filters` never
     reads it for the UUDT branch either.

   Applied only when the visited link's own primary channel is native
   ISO15765 hardware (`l.channel_id == channel_id && base_protocol_id(l.
   hw_protocol_id) == ISO15765`) — every non-native channel (software-ISO-TP,
   raw CAN, the ADR-046 companion channel itself) installs pass-all filters
   with no per-entry eligibility question at all, so every field stays
   trivially eligible there, unchanged from round 4. This changes
   CONTRIBUTION only: an ineligible field is simply never inserted into
   either contention map. It does NOT change gate RESOLUTION — an ineligible
   entry's own gate still resolves normally against the (now-correctly-
   gated) maps, exactly like any other entry's. Forcing an ineligible
   field's gate to `Any` unconditionally was considered and rejected: given
   entry A (USDT `X`, 29-bit, eligible), entry B (USDT `X`, 11-bit,
   eligible), and entry C (USDT `X`, 11-bit, INeligible, iterated before B),
   `X` is genuinely contended between A and B — forcing C's gate to `Any`
   would let C's `find_map` position steal A's own honestly-tagged 29-bit
   frames before `matched` ever reaches B or A, which an eligibility-blind
   `Any` cannot distinguish from the genuine "C's own configured width
   matches" case.

   `CanChannelMode` (not just a pre-collapsed `mode_is_native_mixed: bool`)
   is threaded into `build_cll_rx_entries` as of this round, so both
   `native_mixed`- and `DualChannel`-ness can be derived from one shared
   snapshot rather than two independently-read/derived facts that could
   disagree — `poll_rx_inner` reads `effective_can_channel_mode()` once and
   passes the full enum through.

**`uudt_eligible` widens to also exclude a dual-channel primary**
(design-advisor audit, round 4): `RxEntryKind::Hardware` gains a second
field, `uudt_on_companion: bool` (`l.uudt_channel_id.is_some()`, alongside
the existing `native_mixed: bool`), and `process_frame_for_entry`'s
ISO-tagged arm's `uudt_eligible` becomes `!native_mixed &&
!uudt_on_companion` (was `!native_mixed` alone, ADR-217 round 2). This
closes a related, pre-existing (not introduced by this PR, but newly
load-bearing for the domain rule's own soundness) latent bug: a
dual-channel-mode primary channel never has a `FLOW_CONTROL_FILTER`/
`PASS_FILTER` for its own `CP_CanRespUUDTId` installed at all — UUDT is
only ever received via the separate ADR-046 companion channel — so the
UUDT tier being live on the primary was structurally dead weight that
could, depending on `UniqueRespIdTable` entry iteration order, wrongly
match a USDT-role frame against an unrelated entry's own `CP_CanRespUUDTId`
at the same numeric value before `matched`'s `find_map` ever reached the
correct USDT entry.

Matching becomes `id == can_id && gate.accepts(frame_is_29bit)` everywhere a
numeric CAN-ID comparison against `can_resp_usdt_id`/`can_resp_uudt_id`
currently happens:

- `UniqueRespIdKey::matched`'s USDT and UUDT tiers (gains a `frame_is_29bit:
  bool` parameter).
- `route_frame_uudt_only` (used by the Companion-channel path, the
  native-mixed raw-CAN-tagged `Hardware` arm, and the `SoftwareIsoTp` arm's
  UUDT check) — gains the same parameter.
- The `SoftwareIsoTp` arm's inline USDT `find` (`process_frame_for_entry`).
- `IsoTpRxContext::fc_pairs`' `FcPair` lookup (`ctx.fc_pairs.iter().find(...)`
  in `process_frame_for_entry`) — `FcPair` gains its own width gate so a
  reassembly's `rx_addressing`/`phys_req_can_id` pairing can't cross to a
  same-id-different-width entry either.
- `usdt_addressing_by_id`/`uudt_addressing_by_id` (`build_cll_rx_entries`),
  consumed by `header_footer_len`'s extended-addressing lookup — each
  `(id -> Addressing)` entry becomes `(id, gate) -> Addressing`.

Every one of the five sites above resolves its gate via the SAME per-link
`WidthDomain` classification computed once at the top of that link's own
`build_cll_rx_entries` iteration — never a second, independently-derived
classification (the exact "two searches disagree" bug class ADR-217 round 2
already taught this codebase to avoid).

**The software-ISO-TP reassembly state itself is also keyed by width, not
just the entry-selection gates above** (Codex review finding, PR #141):
`LogicalLinkState::isotp_rx` / `IsoTpRxContext::reassembly`
(`HashMap<u32, isotp::Reassembly>`, tracking each in-flight segmented
transfer across poll passes) was still keyed by `can_id` alone. When two
contended USDT entries on the same software-ISO-TP CLL share a numeric id at
different widths, a `Frame::First` at one width would silently replace the
other width's in-progress reassembly state in this shared map, after which a
`Frame::Consecutive` could complete against — or abort — the wrong transfer
and be delivered under the wrong `unique_resp_identifier`, even though
entry-selection itself (the gates above) already picked the right entry per
frame. The map's key is now `(u32, bool)` (`can_id`, a canonical width bool)
throughout the insert (`Frame::First`)/get_mut/remove
(`Frame::Consecutive`'s `Complete`/`WrongSequence`/`TimedOut` steps) call
sites in `process_frame_for_entry`.

**The key's width component is the matched entry's own
`CanIdWidthGate::reassembly_key_width()`, never a frame's raw
`frame_is_29bit` bit directly** (a second Codex review finding on the first
version of this fix, same PR): for a genuinely contended id the two are
guaranteed equal (`accepts` already required it to route here at all), but
for an UNCONTENDED id (`Any` gate) they are not — a device may honestly tag
one frame of a segmented transfer with `RxStatus` bit 8 and omit it on
another (e.g. FirstFrame tagged 29-bit, its own completing
ConsecutiveFrame untagged), and routing's own `Any` gate already tolerates
this per-frame inconsistency for BOTH frames. Keying reassembly by each
frame's raw bit instead of the gate's own canonical value would silently
fragment one transfer's segments across two map slots and drop the
response — a real regression from the pre-ADR-222 CAN-ID-only key, which
had no such sensitivity. `reassembly_key_width` canonicalizes `Any` (and
`Bits11`) to `false` and `Bits29` to `true`, so an uncontended id's
FirstFrame and ConsecutiveFrames always land on the same key regardless of
that noise, while a contended id still gets its own two independent slots.

`J1939Sa`/`Tp20RxId`/`Tp20TxId`/`ecu_resp_source_addr` tiers are unchanged:
J1939 CLLs always connect flat `CAN_29BIT_ID` (never `CAN_ID_BOTH`, per
`rpc_link.rs`'s own connect-flags derivation), so a J1939 SA match is never
width-ambiguous; TP2.0's `tp20_rx_id`/`tp20_tx_id` are structural
per-connection ids with no configured Format at all; `ecu_resp_source_addr`
is a KWP/J1850 concept, not CAN.

Contention-gating (rather than an unconditional width check, or a check
gated only on whether Format was ever explicitly set) is deliberate:

- **Unconditional** would regress every existing single-width configuration
  that relies on the 11-bit default and never sets the Format ComParam —
  including every current `grpc_mock` test fixture, none of which stamps
  `RxStatus` bit 8 on an injected 29-bit frame's `rx_status` today.
- **Explicit-Format-only** still loses the motivating collision when one
  side (say, the pre-existing 11-bit entry) relies on the default and the
  other explicitly sets 29-bit: the defaulted entry would remain a wildcard
  and, being first/only, would still win.
- **Contention-gated** is a *provable no-op* on every channel that never
  configures a same-id-different-width pair: every gate on such a channel
  computes to `Any`, and `matched`/`route_frame_uudt_only` reduce to exactly
  today's numeric-only comparison. It only engages where a real collision
  exists — precisely the case this ADR exists to fix — and does not depend
  on whether a device honestly reports `RxStatus` bit 8 except on a
  genuinely contended channel, where under-reporting degrades to today's
  (already wrong) behavior rather than introducing a new failure mode.

`FcCapture::fc_can_id` (the software-ISO-TP TX driver's own-FlowControl-echo
capture, `events.rs`) and `TesterPresentDiscard::target_can_ids` remain
numeric-only — both are single-CLL, single-configured-id mechanisms with no
table-collision concept, out of scope here.

ADR-098's Decision cites ADR-061's "no CAN-ID/addressing interpretation
logic reads `RxStatus` bits" finding as a premise; its Status line is
annotated to note the narrowing this ADR makes (see Supersession).

## Consequences

- Fixes the concrete USDT/UUDT-id collision described in Context: a
  contended numeric id now routes to the entry whose configured width
  matches the frame's actual `RxStatus` bit 8, on both a same-table and a
  cross-CLL-shared-channel collision.
- A same-entry cross-field collision (e.g. one entry's `CP_CanRespUSDTId`
  equal to another entry's `CP_CanRespUUDTId`, at different widths) now also
  resolves correctly — including the `uudt_routed` role, which previously
  could report `Uudt` when USDT should have won, or vice versa, on such a
  pairing.
- **Zero behavior change on every uncontended channel** — the common case,
  and every existing `grpc_mock` test fixture — since an uncontended id's
  gate is always `Any`.
- Accepted residual: a device that under-reports `RxStatus` bit 8 on a
  genuinely contended channel still misattributes (degrades to this ADR's
  pre-existing behavior); on a software-ISO-TP CLL specifically, if a
  contended id's FirstFrame and its own completing ConsecutiveFrame(s) are
  tagged with DIFFERENT (both dishonest) widths, the reassembly is dropped
  outright (a stray-CF miss) rather than merely misattributed, since the
  two frames compute different reassembly keys. This ADR does not — and
  cannot — validate the hardware's own bit-8 fidelity.
- Accepted residual: `FcCapture`/`TesterPresentDiscard`'s numeric-only
  matching is unchanged; a contended id colliding with either mechanism's
  own tracked id is not addressed here.
- **Restores the no-op guarantee for a table containing a "dead" entry**
  (round 6): a table entry whose USDT/UUDT field would never actually get a
  point-to-point filter installed (missing `CP_CanPhysReqId`, or
  flow-control-disabled Format for USDT) no longer falsely marks a sibling,
  properly-configured entry's same numeric id as contended — the sibling's
  gate stays `Any` (or its own genuine width, if contended by some OTHER
  eligible entry) exactly as if the dead entry weren't in the table at all.
- Accepted residual (round 6): under `CanChannelMode::Auto`'s own
  probe-then-resolve sequencing, the FIRST channel connect on a module can
  install real filters (including a genuine UUDT `FLOW_CONTROL_FILTER`)
  while `effective_can_channel_mode()` still reads `SingleChannel` — the
  probe that might later resolve the module to `DualChannel` hasn't run
  yet. `point_to_point_filter_eligibility`'s `dual_channel` parameter (and
  `is_dual_channel_link`) reads the CURRENT mode at poll time, not the mode
  in effect when a given filter was actually installed — a mismatch here is
  narrow (only the single window between an `Auto` module's first connect
  and its probe resolving) and self-corrects once the probe completes; not
  investigated further or fixed here.
- Accepted residual (round 6, pre-existing, not introduced or worsened by
  this ADR): if a dual-channel CLL's own ADR-046 companion channel fails to
  open, `uudt_channel_id` stays `None` — `uudt_on_companion` (round 4) reads
  `false`, so `matched`'s UUDT tier stays live on the primary, but
  `rpc_link.rs`'s own `dual_channel` local (computed from the LIVE
  `effective_can_channel_mode()`, independent of whether the companion
  actually opened) still causes `install_point_to_point_fc_filters` to skip
  installing a primary-channel UUDT filter regardless — so the live tier
  never actually matches real hardware-admitted traffic either way. Recorded
  in `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog
  (design-advisor's original round-4 finding of this same gap).
- Accepted residual (round 6, Codex review finding, PR #141):
  `point_to_point_filter_eligibility` answers "would this field's
  ComParams cause `install_point_to_point_fc_filters` to ATTEMPT a filter
  install," not "did that specific attempt actually succeed." A field's
  install call can still fail at connect time (`install_point_to_point_fc_
  filter` returns `Err` — e.g. the adapter has exhausted its own filter
  resources — logged via `warn!` and simply not pushed into the returned
  `Vec<MessageFilterId>`); `LogicalLinkState::unique_resp_filter_ids`
  stores only that flat, unkeyed `Vec` for later removal, with no
  per-entry/per-field record of which specific install succeeded. A
  contended id where one eligible sibling's filter failed to install while
  another's succeeded is therefore still treated as genuinely contended
  (both configs were eligible), giving the surviving entry a strict
  `Bits11`/`Bits29` gate even though, in practice, only ONE width's frames
  can ever actually arrive (the failed filter's own width never reaches
  the service at all) — dropping the surviving entry's own frames if its
  adapter ALSO omits/misreports `RxStatus` bit 8. This requires TWO
  independent adapter-side faults to compound (a filter-install failure —
  itself an already-degraded state, distinct from and rarer than ordinary
  operation — and `RxStatus` bit-8 unreliability) before it manifests as
  incorrect behavior; before this ADR, the same double-fault could not
  cause a regression (matching stayed numeric-only, so the surviving
  filter's own numeric id always matched regardless of bit-8 fidelity) —
  this is a genuine, if narrow, ADR-222-introduced regression class, not a
  pre-existing gap. Not fixed here: closing it properly needs
  `install_point_to_point_fc_filters` to return (and something to
  persist, per-entry/per-field, keyed rather than a flat `Vec`) which
  SPECIFIC installs actually succeeded, then thread that live state into
  `build_cll_rx_entries`'s per-poll-pass contention computation — real new
  state-tracking infrastructure, not a proportionate response to a
  triple-compounding rare-fault scenario. Recorded in
  `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog.
- Accepted residual: a change in a USDT id's contention status while one of
  its software-ISO-TP transfers is in flight — via `SetUniqueRespIdTable`
  on this CLL, or a sibling CLL connecting/disconnecting on the same shared
  raw-CAN channel with the other width — drops that one transfer when the
  flip is `Any` <-> `Bits29` (the canonical reassembly key changes;
  `Any` <-> `Bits11` is unaffected, since both canonicalize to the same
  key). The orphaned slot is purged by the next FirstFrame's N_Cr `retain`
  sweep; the map stays bounded at two slots per numeric id, and a stale
  slot can never complete a later, unrelated transfer (both the N_Cr
  timeout and the ISO-TP sequence-number check independently guard
  against it). Not fixed: the native-hardware path already aborts an
  in-flight reassembly on any table change via its own per-entry
  `FLOW_CONTROL_FILTER` reinstall (ADR-048/ADR-122), and mid-transfer
  reconfiguration already redefines a transfer's addressing/uid even
  pre-ADR-222 — this residual is the software-ISO-TP path converging with
  that same pre-existing behavior at the moment contention appears or
  disappears, not a new class of data loss. A lookup fallback to the other
  width's slot on a miss was considered and rejected: it cannot distinguish
  a genuinely pre-flip transfer from a live transfer under the sibling
  width, reintroducing the very cross-width clobber this ADR fixes.
- Accepted residual: `SingleChannel`'s own share of ADR-217 Consequences'
  cross-row role-attribution ambiguity (two DIFFERENT table rows, one
  `CP_CanRespUSDTId = X`, the other `CP_CanRespUUDTId = X`, on a plain
  single-channel primary — `matched`'s `find_map` attributes the delivery
  to whichever row iterates first, non-deterministically) is NOT closed by
  this ADR's domain split, and is a genuine ambiguity, unlike `DualChannel`'s
  (below) — `SingleChannel` has no companion channel, so both rows' fields
  are legitimately `Both`-domain and genuinely compete for the same frame.
  See Consequences below for the `DualChannel` portion this ADR does close.
- New `grpc_mock` tests (via `MockBackdoor::inject_rx_with_status` with
  `CAN_29BIT_ID_STATUS`) cover: a raw-CAN single-table collision (this ADR's
  motivating scenario), a cross-CLL collision on a shared `(CAN, baud)`
  `CAN_ID_BOTH` channel, a native-ISO15765 two-FC-filter collision, a
  cross-field collision (one entry's USDT id equal to another's UUDT id at
  a different width), a dual-channel CLL whose own primary-channel USDT id
  and companion-channel UUDT id coincide numerically at different widths
  (proving the per-role channel scoping above), an uncontended 29-bit
  table with `RxStatus == 0` asserting delivery is unchanged (the no-op
  guarantee above), an uncontended software-ISO-TP USDT id whose
  FirstFrame and completing ConsecutiveFrame carry INCONSISTENT `RxStatus`
  bit 8 tagging, asserting the reassembly still completes (proving
  `reassembly_key_width`'s canonicalization), a native-mixed entry
  configuring the SAME numeric id as both its USDT and UUDT fields at
  different widths, both delivering despite `RxStatus == 0` (proving the
  domain split), a dual-channel primary with a UUDT-only entry listed
  BEFORE a same-numeric-id USDT-only entry, asserting delivery under the
  correct USDT entry regardless of table iteration order (proving
  `uudt_on_companion`), and a genuine native-mixed UUDT/UUDT width
  collision (`Can` domain) plus its symmetric USDT/USDT counterpart (`Iso`
  domain, edge-case-hunter coverage-gap finding) still disambiguating
  (proving the domain split doesn't over-relax a real same-domain
  collision in either domain), an ineligible sibling USDT entry (no
  `CP_CanPhysReqId`) on plain single-channel mode not falsely contending an
  eligible entry's own id (round 6), and the same for an ineligible sibling
  UUDT entry under the ADR-041 single-channel workaround (round 6) — each
  asserting only the eligible entry's filter is installed
  (`filter_count == 1`) and delivery still lands on it under `RxStatus ==
  0`. `can_mode::software_isotp_reconnect_clears_stale_reassembly_state`
  (pre-existing) continues to cover the reassembly map's ADR-086 clear-on-
  reconnect discipline, orthogonal to and unaffected by this ADR's key-shape
  change.

## Supersession

Does not supersede [ADR-061](ADR-061-rxstatus-rxflag-no-direct-passthrough.md)
(already fully superseded by ADR-097, itself superseded by ADR-098, for the
unrelated `rx_flag`-encoding topic). Narrows the "no live ambiguity" premise
ADR-098's Decision inherits from ADR-061: that premise still holds for every
channel whose `UniqueRespIdTable` never configures the same numeric id at two
different widths; it does not hold for a channel that does, per this ADR's
Decision. ADR-098's Status line is annotated accordingly; ADR-098's own
5-low-bits `rx_flag` encoding and its amendments (ADR-100, ADR-143, ADR-146,
ADR-151, ADR-172) are unaffected and not revisited here. ADR-065's connect-flag
derivation and pass-all-filter installation are unaffected and not revisited
either — this ADR addresses attribution once frames already arrive, not which
frames arrive.

Also does not supersede [ADR-217](ADR-217-can-mixed-format-all-frames.md), but
closes the `DualChannel`-primary portion of its Consequences' cross-row
role-attribution residual (`uudt_on_companion`, added to `RxEntryKind::Hardware`
for this ADR's own domain-classification purpose, incidentally supplies the
"distinguish a dual-channel primary" capability that residual's own text said
was missing) — ADR-217's Status line and Consequences bullet are annotated
accordingly. `SingleChannel`'s own share of that residual is untouched (see
this ADR's own Consequences above) and remains ADR-217's problem to solve, not
this one's.
