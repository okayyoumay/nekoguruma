# ADR-148: `CP_EnableConcatenation` Runtime Support — Registrant-Level Segment Merge Scoped to ISO 22900-2:2022's Protocol List

**Date:** 2026-07-29
**Status:** Accepted (amended twelve times, 2026-07-29 through 2026-08-21 — see Amendment through Amendment 12 below)
**Affects:** `j2534-0404-service` service, service/events, service/comparam_support, service/rpc_primitive, docs/comparam-protocol-support.md

## Context

The D-PDU ComParam `CP_EnableConcatenation` (`PARAM_ENABLE_CONCATENATION`, id `0x807B`) already had an id, a `GetObjectId`/name mapping, and a seeded default of `0` in several KWP and SAE J1850 presets (`comparam_defaults.rs`), but no working `GetComParam`/`SetComParam` path and no runtime effect at all — the same class of dead-ComParam gap already recorded more than once in `j2534-0404-service/docs/implementation-notes.md`'s backlog (e.g. `PARAM_TESTER_PRESENT_IMMED`, `PARAM_EXTENDED_TIMING` on KWP). Specifically:

- `comparam_support.rs::is_can_param` wrongly allowlisted it for the CAN/ISO15765 family.
- `is_kwp_param`/`is_j1850pwm_param`/`is_j1850vpw_param` did not allowlist it at all, so it was unreachable via `GetComParam`/`SetComParam` on every protocol that actually seeds a default for it.
- Nothing in this crate ever read the seeded value — it had zero runtime effect anywhere.

Per ISO 22900-2 Table B.20 (paraphrased, not quoted): this ComParam instructs the D-PDU layer to automatically detect that an ECU's answer to a single request is arriving as more than one complete message — using only the SID (the first payload byte) to recognize a continuation — and to construct one combined response for delivery to the client, waiting for a receive timeout before deciding all segments have arrived. This is distinct from ISO 15765-2 transport-layer reassembly, which happens beneath this layer and already produces one complete message per frame sequence before this logic ever runs — `CP_EnableConcatenation` is about combining multiple already-complete messages that share one SID.

The two ISO 22900-2 editions available in the sibling `vehicle-comm-specs` repository disagree on exactly which protocols carry this ComParam (Table B.11, transport-layer ComParam summary):

- **2009(E)**: `ISO_14230_4`, `ISO_9141_2`, `ISO_15765_2`, `SAE_J1850_PWM`, `SAE_2610_SCI`.
- **2022**: `ISO_14230_4`, `ISO_9141_2`, `ISO_14230_2`, `SAE_J1850_VPW`, `SAE_J1850_PWM`.

They agree only on `ISO_14230_4`, `ISO_9141_2`, and `SAE_J1850_PWM`.

No existing mechanism in `j2534-0404-service` accumulates or merges more than one physical received message into a single delivered response. The closest analog — the tier-2 Receive-Only `NumReceiveCycles` machinery (ADR-100, ADR-059, ADR-053) with its `CP_CyclicRespTimeout`-driven window restart — only counts matches; it never merges payload content, and it is scoped to a different registrant lifecycle (created-receive-only / migrated-IS-CYCLIC) than a normal send/receive COP.

## Decision

**1. Protocol scope: the ISO 22900-2:2022 edition's list.** `docs/j2534-0404-architecture.md` already targets the 2022 edition wherever the two disagree (per `CLAUDE.md`'s Spec References note), so this follows established precedent rather than picking a side ad hoc. Concretely: this codebase's whole KWP family (native `ISO9141`/`ISO14230` plus their overlay variants — the family grouping `is_kwp_family` already uses covers both `ISO_9141_2` and `ISO_14230_2`/`ISO_14230_4`) and both SAE J1850 variants (`J1850VPW`, `J1850PWM`). The CAN/ISO15765 family and the SCI family are explicitly excluded — the pre-existing CAN-family allowlist entry was spec-incorrect and is removed, not extended.

**2. Allowlist fix** (`comparam_support.rs`): `PARAM_ENABLE_CONCATENATION` removed from `is_can_param`; added to `is_kwp_param`, `is_j1850pwm_param`, `is_j1850vpw_param`. `is_sci_param` is unchanged.

**3. Runtime mechanism — a registrant-level accumulator integrated into the existing ADR-100 two-tier binding model**, not a parallel mechanism:

- `CopRegistrant` gains `concat_enabled: bool` (resolved once at registrant-creation time in `wait_for_expected_response`, from the ComParam value **and** the CLL's protocol **and** the registrant's own shape), `concat: Option<ConcatBuf>`, and `concat_segments_got: u32` (monotone, merged by delta exactly like `matches_got` already is).
- **v1 scope is deliberately narrower than "every registrant on an eligible protocol":** only a registrant that is tier-1 (`ActiveSendReceive`) at creation gets `concat_enabled = true`. A true IS-CYCLIC registrant (`NumReceiveCycles == -1`) and a created-receive-only registrant (ADR-059) are excluded even on an otherwise-eligible protocol+ComParam combination — neither has a bounded per-response window to finalize a buffer against the way a normal send/receive COP does. IS-MULTIPLE (`NumReceiveCycles == -2`, distinct ECUs distinguished by `unique_resp_identifier`) **is** in scope; because each expected ECU already gets its own `CopRegistrant` under the pre-existing multi-registrant model, each such registrant's single buffer is naturally scoped to one ECU with no additional keying needed.
- `ConcatBuf` keys an open buffer on `(unique_resp_identifier, SID)`. A continuation candidate is checked against the OPEN buffer's key **before**, not after, the registrant's full `ExpectedResponse` descriptor match — a continuation segment's bytes past the SID are genuinely new response data, so requiring them to also satisfy a mask/pattern written to match a first-segment header would silently break concatenation for any descriptor checking more than byte 0. The full descriptor match still runs, as before, whenever there is no open buffer or the incoming frame's key doesn't match one — deciding whether the frame starts a new response.
- A differing-key frame, or an empty-payload match, arriving while a buffer is open force-finalizes the open buffer first (delivered as one completed logical match) before the new frame is processed on its own terms. An empty payload is never treated as a continuation.
- **Timeout: reuses the existing `CP_P2Max`-derived deadline** already computed in `wait_for_expected_response_inner` for the per-accepted-response restart (ADR-053), now also restarted on every absorbed segment. No new ComParam or internal timing constant was introduced. `CP_CyclicRespTimeout` was considered and rejected: ADR-100 scopes it to created-receive-only (`-1`) registrants, which this feature's v1 explicitly excludes.
- On deadline expiry with a non-empty open buffer, the buffer is finalized and delivered instead of the timeout being reported as `PduErrEvtRxTimeout`. The `channel_id`/`connect_generation` staleness guard (ADR-086 precedent) and the buffer drain/finalize (which commits the live registrant's own `matches_got`) share ONE `logical_links` lock acquisition — a disconnect/reconnect racing the deadline is caught by the guard BEFORE anything is drained, so a failed guard leaves `matches_got` and every open buffer untouched this pass rather than committing a completion for data that was never delivered (edge-case-hunter finding, corrected post-Amendment).
- **Merge semantics:** the merged payload is the first segment's full payload, then each later segment's `payload[1..]` (the SID appears once in the result); delivered metadata (`timestamp`, `header_bytes`, `unique_resp_identifier`, `acceptance_id`, `rx_status_flags`) comes from the FIRST segment; `footer_bytes` comes from the LAST segment. `matches_got` counts one per finalized logical response, not one per physical frame.
- `merge_registrant_writeback` overwrites `concat` wholesale from a pass's snapshot rather than merging it field-by-field — justified by (and `debug_assert!`-documented at the call site) the invariant that concat-eligible protocols never have the CAN/ISO15765 dual-poll-task UUDT-companion setup (ADR-046) that would otherwise let two poll tasks race on the same registrant's buffer; a concat-eligible registrant only ever has one poll task touching it.

## Consequences

- **Completion latency:** a concatenating COP now finishes one full `CP_P2Max` window after its last segment arrives — it cannot know a segment was the last one until the window elapses with no successor. This is an inherent cost of the spec's own requirement to wait out a receive timeout, not implementation overhead.
- **Accepted residual — v1 scope exclusions:** IS-CYCLIC and created-receive-only registrants never get concatenation behavior, even on an eligible protocol with the ComParam set to 1, because neither has a bounded per-response window to finalize against. A future revision could extend this if a concrete need arises; not designed speculatively here.
- **Accepted residual — spec-edition conflict:** `SAE_2610_SCI` and plain `ISO_15765_2` lose `CP_EnableConcatenation` support relative to the 2009(E) edition's list. If a client depends on the 2009(E) behavior for either, this Decision would need revisiting — flagged per this workspace's edition-drift policy, not resolved speculatively.
- `j2534-0404-service/docs/comparam-protocol-support.md`'s `CP_EnableConcatenation` row is updated to `S` for KWP/J1850, `—` for CAN/ISO15765, citing this ADR and ISO 22900-2:2022 Table B.11.
- `docs/j2534-0404-architecture.md`'s `CopRegistrant` field documentation needs a matching update for `concat_enabled`/`concat`/`concat_segments_got` and the new `PollMatchResult::Absorbed` variant — tracked as a same-PR doc-sync follow-up.

## Amendment (2026-07-29): IS-MULTIPLE interleaved-segment P1 fix — single buffer replaced by a keyed `Vec`

A Codex review of the PR implementing this ADR found a P1 bug: the original
mechanism created exactly one `CopRegistrant` per COP regardless of
`NumReceiveCycles`, including `-2` (IS-MULTIPLE — multiple distinct ECUs
answering one broadcast/functional request). Decision §3's `ConcatBuf`
held only ONE open buffer at a time per registrant. When two ECUs'
segmented responses genuinely interleaved on the wire (e.g. ECU A's first
segment, ECU B's first segment, ECU A's second segment, ECU B's second
segment), the "a differing key force-finalizes the sole open buffer" rule
wrongly treated ECU B's first segment as a reason to force-finalize ECU
A's still-incomplete buffer, splitting what should have been two correct
per-ECU merged responses into four incorrect partial deliveries. The
existing IS-MULTIPLE regression test did not catch this because it sent
fully-separated batches (all of ECU A's segments, then all of ECU B's)
rather than genuinely interleaved ones. This amendment records the
design-advisor-approved corrected mechanism; the underlying decision (a
registrant-level accumulator integrated into ADR-100's two-tier binding
model) is unchanged, only the mechanism for scoping and finalizing buffers
is corrected.

**(a) Correcting Decision §3's disproven scoping claim.** The original text
asserted IS-MULTIPLE "gets its scoping for free from one-registrant-per-ECU"
— i.e. that each expected ECU already has its own `CopRegistrant`, so a
single open buffer per registrant was naturally scoped to one ECU with no
additional keying needed. This is false: IS-MULTIPLE uses exactly ONE
`CopRegistrant` per COP, shared across every ECU that may answer the
broadcast/functional request; ECUs are distinguished only by
`unique_resp_identifier` on the frames they send, not by separate
registrants. The actual, corrected scoping mechanism is per-`(unique_resp_
identifier, SID)`-keyed buffers within that one registrant (see (b) below)
— the registrant-per-ECU model this ADR originally assumed does not exist
for IS-MULTIPLE.

**(b) Data structure: `Option<ConcatBuf>` → `Vec<ConcatBuf>`.**
`CopRegistrant::concat` (`j2534-0404-service/src/service.rs`) is now a
`Vec<ConcatBuf>` — one entry per distinct `(unique_resp_identifier, SID)`
key currently mid-accumulation, not a `HashMap` (expected cardinality is
single-digit distinct ECUs, so a linear scan is simpler and preserves
first-segment arrival order for deterministic finalize/delivery order). A
continuation frame is absorbed into whichever open buffer's own key
matches; a frame matching no open buffer's key opens a FRESH buffer,
gated on the quota invariant `matches_got + concat.len() < matches_needed`
(for a finite `matches_needed`) so the total of already-counted matches
plus still-open buffers never exceeds the COP's configured limit.
`finalize_concat_buffer` (singular, `Option` return) becomes
`finalize_concat_buffers` (plural, drains the whole `Vec` and returns every
finalized delivery, incrementing `matches_got` once per buffer).

**(c) Differing-key force-finalize removed; deadline-only finalization.**
Decision §3's "a differing-key frame force-finalizes the open buffer"
rule is removed entirely. A differing key now only means "possibly a
different ECU, try to open its own buffer" — never a trigger to finalize
something else. The ONE remaining finalize trigger besides the
receive-phase deadline is the pre-existing empty-payload-match case
(edge-case-hunter Finding 3, unchanged in kind): an empty payload that
satisfies `ExpectedResponse::matches`'s `cmp_len == 0` short-circuit still
force-finalizes every currently-open buffer before the empty match is
itself considered (with its own post-finalize quota re-check, so it is not
double-counted once the finalize(s) alone already meet the quota).
Deadline expiry (`wait_for_expected_response_inner`) keeps ONE shared
deadline for the whole registrant, restarted by any absorbed/matched frame
on ANY buffer (not a separate deadline per buffer) — while any ECU is
still replying the link-level `CP_P2Max` window is not idle, so an
already-complete-looking buffer is deliberately held open until the whole
burst quiets down. On expiry, ALL open buffers finalize together in one
pass (`finalize_concat_buffers`), delivered as separate `ResultData`s in
buffer (first-opened-first) order, with `matches_got` incremented by the
total count before the existing quota-met/`CycleComplete` vs.
restart-and-continue decision is made.

**Semantic change for finite `NumReceiveCycles`:** previously, a
differing-key frame arriving while a buffer was open immediately
force-finalized it (an early, single-frame-triggered completion). Under
the corrected mechanism, a differing-key frame is never itself a finalize
trigger — the original buffer is held open, exactly like every other case,
until the shared receive-phase deadline expires (or, unchanged, the
empty-payload-match case above). This can shift completion later in time
for a finite-`N` COP that previously completed early via that force-finalize
path, but corrects the interleaved-IS-MULTIPLE data-corruption bug and
removes an inconsistency where only PART of the buffer set had deadline-only
finalization.

## Amendment 2 (2026-07-29): continuation fast path bypassed ADR-100's non-vacuous-first precedence

### Context

A Codex round-3 review of the first Amendment's fix found a second P1 bug,
design-corrected by design-advisor. `events::bind_registrant` runs once per
`events::AttributionScan` variant per frame (`Tier1NonVacuous` first, then
`Tier1Vacuous`, then `Tier2` — first successful bind wins; ADR-100's
documented non-vacuous-first precedence). The continuation fast path added
by Amendment 1(b)/(c) — checking whether an incoming frame's `(unique_
resp_identifier, SID)` matches an already-open `ConcatBuf`'s key, absorbing
it without going through the full `ExpectedResponse` descriptor match — had
no gating on `scan` at all; it only checked `concat_enabled`, quota, and the
buffer key. The sibling "open a new buffer" arm, by contrast, correctly
gated on `scan` matching the admitting descriptor's vacuousness.

Consequence: if a registrant R's buffer was originally opened via a
VACUOUS descriptor (meaning R should only win frames during the
`Tier1Vacuous` pass, after every non-vacuous registrant has had first
refusal during `Tier1NonVacuous`), a later continuation frame with the
same key was still absorbed by R's fast path during the earlier
`Tier1NonVacuous` pass, because the fast path never consulted `scan`. A
genuinely different, non-vacuous registrant B that should have claimed the
frame first per precedence never got the chance: the scan hits R first
(registration order), R's fast path claims the frame immediately, and
`bind_frame` returns before B is ever considered.

### Decision

- `ConcatBuf` (`j2534-0404-service/src/service.rs`) gains `opened_vacuous:
  bool`, set from the admitting descriptor's `ExpectedResponse::is_vacuous()`
  at the moment a buffer is opened (`bind_registrant`'s "open a new buffer"
  arm) and never mutated afterward — a continuation's bytes may satisfy no
  descriptor's mask/pattern at all (the whole reason the fast path exists),
  so there is no descriptor to re-derive vacuousness from on a
  continuation.
- The continuation fast path is now gated on the current `scan` matching
  the buffer's recorded classification: `Tier1NonVacuous` requires
  `!buf.opened_vacuous`, `Tier1Vacuous` requires `buf.opened_vacuous`,
  `Tier2` is written as an unreachable-but-exhaustive `false` (a
  concat-eligible registrant is always tier-1/`ActiveSendReceive` for its
  whole life — `concat_enabled` excludes every tier-2/created-receive-only/
  IS-CYCLIC registrant per this ADR's v1 scope — so `bind_registrant`'s own
  tier gate already keeps it out of the `Tier2` scan). When the gate fails,
  the frame simply falls through; the same registrant is scanned again on
  the next `scan` variant via the normal `bind_frame` call sequence, where
  the gate then passes.
- The "open new buffer" arm's `debug_assert!` that a same-key buffer can
  never already exist at that point is no longer true once the fast path
  is scan-gated: a registrant can have a vacuous-opened buffer (fast path
  declines during `Tier1NonVacuous`) AND a separate non-vacuous descriptor
  that also matches the same continuation's full payload, reaching the
  "open new buffer" arm for a key that already has an open buffer. The
  assert is replaced with a real branch: if an existing same-key buffer is
  found, absorb into it (extend `data`, overwrite `footer_bytes`, increment
  `concat_segments_got`, return `absorbed: true`) instead of opening a
  duplicate — checked BEFORE the quota gate, since absorption never
  consumes quota. Only when no existing buffer matches the key does the arm
  open a genuinely new one, now recording `opened_vacuous` from the
  matched descriptor.

### Consequences

- A vacuous-opened buffer's continuations now correctly yield to a
  non-vacuous claimant, and to tester-present's discard signature, during
  the earlier `Tier1NonVacuous`/step-3 scan passes — matching how the
  buffer's own first (opening) segment was originally attributed under
  ADR-100's precedence table.
- The `matches_got + concat.len() < matches_needed` quota invariant
  (Amendment 1(b)) is unaffected: the newly added absorb-into-existing
  branch never consumes quota, exactly like the pre-existing fast path it
  sits alongside.
- Two regression tests were added in `j2534-0404-service/src/service/
  events.rs` (`bind_frame_tests`): one reproducing the Codex scenario (a
  vacuous-opened buffer must yield to a differently-registered, non-vacuous
  registrant during `Tier1NonVacuous`), and one for the
  both-descriptor-vacuities corner (a registrant with both a vacuous
  opener descriptor and a separately-matching non-vacuous descriptor must
  absorb into its single existing buffer, not open a duplicate).

## Amendment 3 (2026-07-29): no-table URID collision, unbounded buffer growth, unbounded buffer count — three P1 fixes from a single Codex review round

### Context

A Codex review round on the PR implementing the second Amendment found two
P1 bugs; design-advisor's proactive sweep of the same PR found a third.

1. **No-table URID collision corrupts multi-ECU responses.** `route_frame`
   (`events.rs`) returns `unique_resp_identifier = 0` for every frame when
   the CLL has no configured `UniqueRespIdTable` — confirmed to be the ONLY
   functioning RX mode for KWP/J1850 (this ADR's concat-eligible protocols):
   a table-configured KWP/J1850 CLL's `unique_resp_ids` list only ever
   carries CAN USDT/UUDT IDs (`PARAM_CAN_RESP_USDT_ID`/`PARAM_CAN_RESP_
   UUDT_ID`), which KWP/J1850 never populate (they use `PARAM_ECU_RESP_
   SOURCE_ADDR`, TX-only) — so a table-configured KWP/J1850 CLL actually
   drops every frame ≥4 bytes in `route_frame` today, a pre-existing,
   concat-independent RX-routing bug not fixed by this Amendment (the
   drop-vs-wildcard half closed by the PR #72 round-1 fix referenced below;
   the deeper "no real per-ECU matching tier at all" gap later closed by
   ADR-203). Since no-table mode is universal for these protocols,
   `ConcatBuf`'s key of `(unique_resp_identifier, SID)` collapsed to `(0,
   SID)` for every frame, colliding across distinct ECUs answering the same
   functional/broadcast request with the same SID — the second ECU's
   message was wrongly appended as a continuation of the first ECU's buffer.
2. **Unbounded buffer growth (memory/time).** Neither absorb site (the
   continuation fast path, nor the "open new buffer" arm's absorb-into-
   existing branch) capped `ConcatBuf.data`'s length or the number of
   segments absorbed, and `PollMatchResult::Absorbed` restarts the receive-
   phase deadline on every absorbed segment — a noisy/misbehaving sender
   could grow a buffer and postpone finalization indefinitely.
3. **Unbounded open-buffer *count* (design-advisor's proactive sweep).** For
   IS-MULTIPLE (`matches_needed: None`), the "open a new buffer" arm's quota
   gate always passes when `matches_needed` is `None`, so nothing capped how
   many DISTINCT open buffers one registrant could accumulate — combined
   with fix 1's larger key space (URID × source_id × SID), a noisy adapter
   feeding a vacuous-descriptor IS-MULTIPLE COP could open unbounded
   buffers.

### Decision

**Fix 1 — three-component key.** `ConcatBuf::key` (`service.rs`) widens from
`(u32, u8)` (URID, SID) to `(u32, Option<u8>, u8)` (URID, `source_id`, SID).
`source_id` is derived once per frame, inside `bind_frame` (`events.rs`),
from `CllRxEntry::header_protocol` and that frame's own already-split
`header_bytes` (ADR-051) — never from `unique_resp_identifier`, which is
unreliable exactly where this fix matters:

- ISO9141/ISO14230 (KWP consolidated header `[fmt, tgt, src, ..]`):
  `Some(header_bytes[2])` when `header_bytes.len() >= 3`.
- J1850 PWM/VPW (`[pri/type, tgt, src]`): `Some(header_bytes[2])` when
  `header_bytes.len() == 3` exactly.
- Every other protocol, or a too-short/headerless frame: `None`.

The key is deliberately NOT the full header (the KWP format byte varies
per segment within one ECU's own response, which would wrongly split a
single ECU's own continuation) — only the source-address byte. A `None`
`source_id` (headerless/too-short frame) groups with every other `None`-
keyed frame sharing the same `(unique_resp_identifier, SID)`, same as
before this fix — an accepted residual: no ECU-identifying data is
physically available at this layer for such a frame. The new component is
threaded from `bind_frame` into `bind_registrant` via a new `ConcatFrameMeta::
source_id: Option<u8>` field, computed once at the top of `bind_frame` and
overriding whatever the caller passed in (every call site outside
`bind_frame` itself always passes `None`, since only `bind_frame` has the
`header_protocol`/`header_bytes` pairing needed to derive a real value).

**Fix 2 — per-buffer byte and segment caps, force-finalized inline.** Two
new internal, non-configurable guard constants in `events.rs`, following the
existing `ISOTP_TX_MAX_CONSECUTIVE_RX_WAIT_FRAMES`-style precedent
(ADR-124):

- `CONCAT_MAX_BUF_BYTES: usize = j2534_0404::MAX_MESSAGE_DATA` (4128) — the
  same cap this crate already enforces on one whole `PassThruMessage`'s
  data; a concatenated response can never legitimately need to exceed what
  a single physical message is itself bounded to.
- `CONCAT_MAX_BUF_SEGMENTS: u32 = 256` — closes the degenerate case where a
  SID-only 1-byte payload adds `0` bytes to `data` per segment (never
  tripping the byte cap) but would otherwise restart the deadline forever.

`ConcatBuf` gains a `segments: u32` field (physical segments absorbed into
that ONE buffer, starting at `1` for the opening segment). Both absorb
sites check, immediately after absorbing the triggering segment, whether
`data.len() > CONCAT_MAX_BUF_BYTES` or `segments > CONCAT_MAX_BUF_SEGMENTS`;
if either is now true, that ONE buffer (never any other open buffer on the
same registrant) is force-finalized inline — removed from `concat`,
`matches_got` incremented by 1, and the resulting `ConcatDelivery` pushed
into `finalized_concat` — via a new `finalize_one_concat_buffer` helper,
following the same "finalize inline, push to `finalized_concat`" pattern
the pre-existing empty-payload-match arm (edge-case-hunter Finding 3) already
established for finalizing every open buffer at once. The triggering segment
itself is still absorbed (not dropped) before the cap forces finalization —
no data is lost, the buffer is simply not held open any longer than the cap
allows.

**Fix 3 — open-buffer-count cap.** A third constant, `CONCAT_MAX_OPEN_
BUFFERS: u32 = 32` — generous headroom over Amendment 1's documented
real-world cardinality (single-digit distinct ECUs for IS-MULTIPLE), not a
real-world limit; it exists purely to bound a noisy adapter, not to
constrain legitimate multi-ECU traffic. The "open a new buffer" arm's
existing quota gate (`matches_got + concat.len() < matches_needed`, vacuous
for `matches_needed: None`) is now ANDed with `concat.len() < CONCAT_MAX_
OPEN_BUFFERS`. This check applies ONLY to opening a genuinely new buffer —
absorbing into an already-open buffer never increases the open count, so it
is unaffected. When the cap is hit, the triggering frame is simply not
absorbed by this registrant — the same fall-through behavior as today's
quota-exhausted case, no special handling.

### Consequences

- **Accepted residual — headerless/short-header frames still group under
  one key.** A `None` `source_id` cannot distinguish concurrently-answering
  ECUs; this is unchanged behavior for that narrow case (no regression, just
  not improved by this fix), since no ECU-identifying data is physically
  available at this layer for such a frame.
- **Accepted residual — the pre-existing table-configured KWP/J1850
  RX-routing bug is NOT fixed here.** It is concat-independent (affects RX
  routing generally, not just this feature). **Update (Codex review finding,
  PR #72 round 1, ADR-179/Phase 5):** resolved — `events_rx_routing.rs`'s
  `build_cll_rx_entries` now filters any `UniqueRespIdTable` entry
  configuring none of `CP_CanRespUSDTId`/`CP_CanRespUUDTId`/
  `CP_J1939SourceAddress` (exactly what a KWP/J1850 `CP_EcuRespSourceAddress`
  -keyed entry does) out of `unique_resp_ids` entirely, so `route_frame` sees
  an effectively-empty table and falls back to its no-table wildcard delivery
  instead of dropping frames, closing this residual.
- **The three guard constants (`CONCAT_MAX_BUF_BYTES`, `CONCAT_MAX_BUF_
  SEGMENTS`, `CONCAT_MAX_OPEN_BUFFERS`) are internal robustness bounds, not
  exposed as a ComParam** — no client-visible configuration surface changes;
  a client cannot query or adjust them. They exist solely to bound
  pathological/misbehaving-sender cases, mirroring `ISOTP_TX_MAX_
  CONSECUTIVE_RX_WAIT_FRAMES`'s own precedent and rationale.
- **Client-visible change, Fix 2 (byte/segment caps):** a COP whose
  concatenated response would have exceeded 4128 bytes, or accumulated more
  than 256 segments, now completes early instead of growing that buffer
  unboundedly or timing out — force-finalized on the first absorb that pushes
  it past either cap, not trimmed to an exact ceiling: the delivered
  `ResultData` includes the triggering segment in full, bounded by
  `CONCAT_MAX_BUF_BYTES + MAX_MESSAGE_DATA − 1` bytes / `CONCAT_MAX_BUF_
  SEGMENTS + 1` segments (see Amendment 7). This is expected to be
  unreachable in practice for any conformant ECU/bus combination — the caps
  exist for pathological input, not normal operation.
- **Client-visible change, Fix 3 (open-buffer-count cap) — NOT an early
  completion.** Unlike Fix 2, hitting `CONCAT_MAX_OPEN_BUFFERS` never
  force-finalizes anything: for an IS-MULTIPLE COP answered by more than 32
  distinct-keyed responders, the "open a new buffer" arm's cap check simply
  declines to absorb the 33rd+ distinct responder's triggering frame — the
  same fall-through behavior as today's quota-exhausted case (the frame is
  left unclaimed by this registrant, same as any other unmatched frame).
  That 33rd+ responder's data is silently not delivered; the other,
  already-open buffers are unaffected and continue accumulating/finalizing
  normally. This is expected to be unreachable in practice for any
  conformant ECU/bus combination — the cap exists for pathological input
  (a noisy adapter), not normal multi-ECU traffic.
- Regression tests were added in `j2534-0404-service/src/service/events.rs`
  (`bind_frame_tests`): two for Fix 1 (two distinct source addresses under
  the same no-table URID opening two separate buffers; a headerless-frame
  case confirming graceful `None` grouping), two for Fix 2 (a buffer
  force-finalizing when an absorb pushes it over the byte cap; the
  SID-only-segment degenerate case force-finalizing at the segment-count
  cap), and one for Fix 3 (an IS-MULTIPLE registrant at `CONCAT_MAX_OPEN_
  BUFFERS` declining a new-key frame while still absorbing an existing
  buffer's own continuation normally).
- A subsequent edge-case-hunter review round (still 2026-07-29) found the
  above coverage exercised only the continuation fast path's own byte-cap
  check, never the SECOND absorb site (the "open new buffer" arm's
  absorb-into-existing branch), and never the exactly-at-cap boundary or the
  Fix 2/Fix 3 interaction (a Fix-2 force-finalize freeing a slot for Fix 3's
  count cap). Four more tests were added to close these gaps: the
  exactly-at-cap case for both the byte and segment caps (confirming neither
  force-finalizes early), the byte cap exercised at the SECOND absorb site
  specifically, and a Fix 2/Fix 3 interaction test confirming a force-finalize
  actually frees an open-buffer slot for a subsequent new key. A full
  RPC/poll-task-level integration test (`j2534-0404-service/tests/grpc_mock/
  concat.rs`) was also added for Fix 1's `source_id` keying, since every
  pre-existing integration test in that file used a fixed source address.

## Amendment 4 (2026-07-29, Codex round-4 P2 finding)

**Context.** For an IS-MULTIPLE COP (`NumReceiveCycles == -2`, `matches_needed:
None`) with concatenation enabled, `wait_for_expected_response_inner`'s
deadline-expiry finalize path (`matches_got += delivered_count`) completed
the phase only via `matches_needed.is_some_and(|needed| matches_got >=
needed)` — which can never be `true` for `None`. Since a concat-enabled
registrant's `matches_got` only ever advances through this same finalize
path (every match is funneled through a buffer), reaching this branch at
all already means the receive-phase deadline expired with nothing new
absorbed during the whole prior window — exactly IS-MULTIPLE's own
pre-existing completion signal (ADR-053: the collection window closes once
a full `CP_P2Max` period passes with no new match). The `is_some_and` check
treated that as "not yet done" and unconditionally restarted a SECOND full
window before finishing, needlessly doubling the COP's completion latency
after its results had already been delivered.

**Decision.** Changed the check to `matches_needed.is_none_or(|needed|
matches_got >= needed)`. For a finite `matches_needed` (`Some(n)`), behavior
is unchanged (the same closure still runs); for IS-MULTIPLE (`None`), the
phase now completes immediately on this deadline-triggered finalize instead
of always restarting.

**Consequences.** An IS-MULTIPLE + concat COP now finishes after one
`CP_P2Max` window past its last accepted segment, matching non-concat
IS-MULTIPLE's existing completion latency, instead of two. Regression test:
`j2534-0404-service/tests/grpc_mock/concat.rs::concat_is_multiple_completes_after_one_window_not_two`
(bounds total elapsed time comfortably under two windows).

This ADR has now been amended four times in one review loop; per this
repo's `codex-pr-review-loop` skill guidance, a follow-up consolidation
(rewriting the Decision section to absorb the amendments, rather than
appending further) is worth considering the next time this mechanism
changes.

## Amendment 5 (2026-07-29, Codex review + design-advisor P1 fix): `Absorbed` outranking `PendingRc` combined with an unconditional reset silently discarded the pending RC

**Context.** `check_match_against_baseline` (`events.rs`) ranks a poll
pass's own outcome `Matched` > `Absorbed` > `PendingRc` > `NoMatch`
(unchanged by this Amendment — design-advisor traced reordering this
ranking and rejected it, since it just mirrors the same bug in the
opposite direction). `observe_and_consume_pending_rc_outcome` calls that
ranking function and then reset `registrant.pending_rc` to `None`
unconditionally, regardless of which outcome was actually reported —
correct for `Matched` (ADR-101 Decision §C: a completed match means the
ECU already fully answered, superseding a pending RC), but wrong for
`Absorbed`: an absorbed segment is not a completion, the registrant is
still mid-accumulation. When one `PassThruReadMsgs` batch contained both
(a) a frame that absorbed into this registrant's own open concat buffer
and (b) a pending NRC (0x78/0x21/0x23) for the same registrant — from a
different ECU, or, as design-advisor additionally confirmed, even the
same ECU emitting an NRC between its own segments — `Absorbed` outranked
`PendingRc` and was reported, but the unconditional reset discarded the
pending RC in the very same call, unreported, so its own P2*
(0x78-triggered reload) or RC21/RC23 re-request timing action never
fired. The other ECU's (or the same ECU's delayed) eventual response
could then time out.

**Decision.** Made the `registrant.pending_rc = None` reset in
`observe_and_consume_pending_rc_outcome` CONDITIONAL on the reported
outcome: clear it when the outcome is `Matched` (unchanged — ADR-101
Decision §C's documented supersede) or `PendingRc` (it was just reported
here — the existing "every writer of `pending_rc` only ever sets it when
currently `None`" invariant already covers this as a reported
occurrence); PRESERVE it (do not reset) when the outcome is `Absorbed`.
`check_match_against_baseline`'s own ranking and the wait loop's
`PollMatchResult::PendingRc`/`Absorbed` arms are both unchanged — the
preserved `pending_rc` correctly surfaces as `PollMatchResult::PendingRc`
on the very next poll pass, which is immediate (the `Absorbed` arm
already sets `poll_immediately = true`), and the existing `PendingRc` arm
already applies the P2* reload / RC21/RC23 re-request unchanged. This is
deliberately a one-condition fix to the reset, not a restructuring of the
ranking or the wait loop.

**Consequences.** The RC's own timing action may now lag by one
immediate poll pass in the worst case (the pass where it coexisted with
an `Absorbed` report), rather than being silently discarded. A
continuous-absorb run (repeated `Absorbed` reports with no intervening
quiet pass) defers the RC's surfacing until the first pass with no new
absorb — this is NOT bounded to a safe outcome by
`CONCAT_MAX_BUF_SEGMENTS`/`CONCAT_MAX_BUF_BYTES`: hitting either cap
force-finalizes the buffer via `finalize_one_concat_buffer`, which
increments `matches_got` synchronously in the same pass, so
`check_match_against_baseline` reports `Matched` instead — and `Matched`
still clears `pending_rc` unconditionally (unchanged by this Amendment,
per the Decision above). A cap-triggered finalize therefore silently
discards a still-preserved RC exactly like the pre-fix behavior did, one
pass later rather than resolved; see the new accepted-residual bullet
below (edge-case-hunter finding) for the concat-specific instance of this
gap. Regression tests:
`j2534-0404-service/src/service/events.rs`'s
`observe_and_consume_pending_rc_outcome_tests::absorbed_outcome_preserves_pending_rc_not_cleared`,
`::absorbed_then_next_pass_reports_preserved_pending_rc_and_clears_it`,
`::matched_outcome_still_clears_pending_rc_unchanged_by_amendment_5`, and
`bind_frame_tests::concat_absorb_and_pending_rc_coexist_in_one_batch`
(pins that the absorb-vs-RC-detect mechanics at the `bind_registrant`
layer itself were already correct before this fix — the bug was entirely
in the later observe-and-consume step).

- **Separate accepted residual, recorded but explicitly NOT fixed by this
  Amendment** (per this repo's `CLAUDE.md` requirement that every
  deferred item be durably recorded before merge): design-advisor found a
  related PRE-EXISTING gap, not introduced by this PR and not
  concat-specific. ADR-101 Decision §C's residual (a) treats an
  unconditional clear on `Matched` as narrow in practice because "a
  deadline restart from the match makes the discarded RC largely moot" —
  reasoning that assumes the completing response means the ECU already
  fully answered. That assumption holds for a completing single-response
  COP, but when `matches_needed` is `> 1` or `None` (an ongoing
  IS-MULTIPLE phase, still collecting further responses), one ECU's
  match completing does NOT mean a DIFFERENT ECU — whose own pending RC
  is also outstanding on the same registrant — has answered. That other
  ECU's pending RC is still wrongly cleared by the `Matched` case today,
  exactly as residual (a) describes, but the "largely moot" framing does
  not hold for this multi-ECU IS-MULTIPLE case; ordinary non-concat CAN
  functional requests can hit this too, so it predates concat entirely.
  This Amendment does not change the `Matched` case's unconditional
  clear (see Decision above); the gap is recorded here as an
  explicitly-scoped accepted residual, cross-referenced against ADR-101's
  residual (a), for a future fix.

- **Second accepted residual (edge-case-hunter finding on this Amendment's
  own review round, concat-specific, not fixed here):** the cap-triggered
  finalize case the Consequences paragraph above flags is a SAME-ECU
  instance of the identical gap, distinct from the multi-ECU residual just
  above and arguably a tighter violation of ADR-101 Decision §C's own
  premise: when `finalize_one_concat_buffer` force-finalizes a buffer at
  `CONCAT_MAX_BUF_SEGMENTS`/`CONCAT_MAX_BUF_BYTES`, that registrant's own
  ECU did NOT actually finish answering — the response was truncated by
  this codebase's own internal robustness cap, not by the ECU's own
  completion — yet the resulting `Matched` report still unconditionally
  clears any `pending_rc` coexisting on that same registrant, discarding
  that same ECU's own still-outstanding RC as if it were moot. This did
  not exist before concat: no non-concat path force-finalizes a response
  mid-stream. Not fixed here — would need `finalize_one_concat_buffer`'s
  cap-triggered `Matched` to be distinguished from a genuine
  quota/deadline-driven `Matched` before the `Matched`-clears rule could
  be safely narrowed for this case specifically.

## Amendment 6 (2026-07-29, Codex round-7 P1 finding): delivery folded into the guard+drain critical section, closing an await-gap disconnect race

**Context.** `finalize_concat_buffers_if_live` (the helper Amendment 1(c)/
the fix following Amendment 1 introduced to make the ADR-086 `channel_id`/
`connect_generation` staleness guard and the buffer drain/finalize atomic
under one `logical_links` lock acquisition) still returned `(Vec<
ConcatDelivery>, Option<Arc<Mutex<CllEventQueue>>>)` and RELEASED the
`logical_links` lock before its caller (`wait_for_expected_response_inner`'s
deadline-expiry branch) delivered each drained buffer via `deliver_or_
enqueue`. A `DisconnectComLogicalLink` landing in the await gap between the
helper returning and the caller finishing its delivery loop clears
`channel_id` and cancels the COP (`cancel_link_cops`), but the batch had
already been drained and `matches_got` already committed inside the
helper's own lock acquisition — the delivery loop then still ran,
delivering an already-cancelled batch into the link's `rx_buf` queue.
Unlike `DestroyComLogicalLink`, `DisconnectComLogicalLink` does not
destroy or replace `rx_buf` (`rpc_link.rs`: `rx_buf` is constructed once,
at `CreateComLogicalLink` time, and persists across a `Disconnect`/
reconnect of the same CLL handle) — so the stale delivery reaches whatever
subscriber is live on that queue right now, including one that reconnected
after the disconnect that should have cancelled this delivery.

A design-advisor agent, reading the current code directly (not just this
ADR's prior text), confirmed the race is real and client-visible: no
duplicate terminal status results (the receive-phase task's own terminal
emission is already protected by the pre-existing `primitives` first-wins
discriminator), but the Frame delivery itself is unguarded — a client can
receive a `ResultData` tagged with a `cop_handle` it just saw `Cancelled`,
or a reconnected session's `GetEventItem` can hand out a response that
belongs to the disconnected session that preceded it.

**Decision.** Fold the delivery loop into the SAME `logical_links` lock
acquisition that already does the guard check and the drain, rather than
returning a queue handle across the await gap. The helper is renamed
`finalize_and_deliver_concat_buffers_if_live` and now returns `u32` (the
delivered count) instead of the old tuple; callers gate on `> 0` in place
of the old `!finalized_concats.is_empty()`/`Some(rx_buf)` pair — guard
failure and "guard held but nothing was open" both still read as `0`,
so `matches_got`/completion at the call site remain conditional on the
guard having held, exactly as before this Amendment.

This is safe under the workspace's own documented lock order
(`logical_links -> subscriptions -> per-CLL queue`, `service.rs`): holding
`logical_links` across `deliver_or_enqueue`'s `.await` calls does not
reverse it, since `deliver_or_enqueue` only ever acquires the innermost
per-CLL queue lock and never touches `logical_links`. This ordering is
already exercised elsewhere in this file — `cancel_link_cops` calls
`send_cop_status` (itself an enqueue) while holding `primitives`, and
`rpc_misc.rs`'s `SET_EVENT_QUEUE_PROPERTIES` handler already nests an
`rx_buf` lock directly inside a held `logical_links` guard — this Amendment
repeats the same nesting in a bounded loop (at most `CONCAT_MAX_OPEN_
BUFFERS` = 32 iterations), not a new pattern.

The design-advisor brief also considered, and rejected, per-iteration
revalidation (re-checking the guard before each delivery without holding
the lock through the whole batch): that only shrinks the race window per
item rather than closing it, and correctness there would require locking
per item anyway — which reduces to this Decision's single-critical-section
shape but leaves an unanswerable partial-delivery question (the drain
already commits every buffer's `matches_got` in one pass; failing partway
through a per-item-locked delivery loop would leave `matches_got` ahead of
what was actually delivered). Folding delivery into the one acquisition
dissolves that question by construction: disconnect's own teardown also
takes `logical_links`, so it serializes strictly before (guard fails,
nothing drained or delivered) or strictly after (the whole batch delivered
before `Cancelled` is emitted) this helper's critical section — never
interleaved with it.

**Consequences.**

- `logical_links` is now held for the duration of up to 32 sequential
  `rx_buf`-queue critical sections per deadline-expiry pass, instead of
  being released before delivery. Each nested critical section is O(1)
  in-memory work (`push_cll_event` plus, at most, one non-blocking
  `UnboundedSender::send`), matching the existing `SET_EVENT_QUEUE_
  PROPERTIES` precedent cited above; an edge-case-hunter pass on this
  Amendment found no evidence this matters for real workloads, only
  flagged it as a bounded, informational cost.
- Regression tests, `j2534-0404-service/src/service/events.rs`
  (`registrant_lifecycle_tests`): the pre-existing stale-generation test
  was renamed and updated for the new `u32` contract
  (`finalize_and_deliver_concat_buffers_if_live_stale_generation_advances_nothing`);
  a new positive-path test
  (`..._delivers_and_advances_when_guard_holds`) pins that a held guard
  drains, commits `matches_got`, AND delivers every buffer into `rx_buf`
  within the one acquisition; a third test
  (`..._guard_holds_no_open_buffers`, added per the edge-case-hunter review
  of this Amendment) pins the guard-holds-but-nothing-open shape
  separately from the guard-failure shape, since the helper's doc comment
  distinguishes them even though both currently return `0`.
- **Accepted residual (edge-case-hunter finding on this Amendment's own
  review round, pre-existing, not concat-specific, not fixed here):** the
  same "resolve `rx_buf` under `logical_links`, release, deliver later"
  shape this Amendment closes for the concat path still exists in two
  NON-concat sites in this file: `poll_rx_inner`'s per-frame delivery fan-out
  (snapshots `entry.rx_buf` before a delivery loop with `.await` points in
  between) and the fast-init synthetic-frame delivery (which re-resolves
  under a generation check immediately before delivering, but still has the
  same post-check-pre-deliver residual window). Both predate this PR
  (ADR-100/ADR-115 era). Confirmed narrower than the concat case: `poll_
  rx_inner`'s own writeback (`merge_registrant_writeback`) discards its
  ENTIRE per-pass delta, including `matches_got`, whenever the live
  `connect_generation` no longer matches the pass's own snapshot — so a
  race there cannot corrupt a live COP's completion count the way the
  pre-Amendment-6 concat bug could; only a stray frame delivered into a
  stale `rx_buf` survives, the same category of residual `Amendment 3`'s
  "headerless frame" note and this file's existing accepted residuals
  already tolerate elsewhere. Not fixed by this Amendment — tracked in
  `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog,
  citing this Amendment, for a future sweep of the same shape across all
  three sites.

This ADR has now been amended six times in one review loop; the follow-up
consolidation flagged after Amendment 4 (rewriting the Decision section to
absorb the amendments, rather than appending further) remains worth doing
in a follow-up change once this PR's review loop closes — deliberately not
done mid-review here, to avoid churning the diff Codex is re-reviewing.

## Amendment 7 (2026-07-29, Codex round-8 P2 finding): cap-after-append delivery size reaffirmed and documented, not changed

**Context.** Both concat absorb sites in `bind_registrant` (the continuation
fast path and the "open new buffer" arm's absorb-into-existing branch)
extend the buffer's `data` and increment `segments` BEFORE checking
`CONCAT_MAX_BUF_BYTES`/`CONCAT_MAX_BUF_SEGMENTS` — Amendment 3 Fix 2's
original design, made deliberately: "the triggering segment itself is
still absorbed (not dropped) before the cap forces finalization -- no data
is lost." A round-8 Codex review flagged this as a P2: a buffer already
sitting at the byte cap that then absorbs one more full-size segment
delivers up to `CONCAT_MAX_BUF_BYTES + MAX_MESSAGE_DATA - 1` bytes —
nearly double the cap constant's own stated rationale ("a concatenated
response can never legitimately need to exceed what a single physical
message is itself bounded to"), an overclaim the doc comment made but the
code never actually enforced as an exact ceiling.

A design-advisor review, reading the current code and this ADR's full
Amendment 3 text directly, confirmed the discrepancy is real but is a
**documentation-accuracy gap, not a behavior defect**: nothing downstream
of `ConcatDelivery`/`ResultData.data_bytes` assumes a `MAX_MESSAGE_DATA`
ceiling (it reaches the client as an unbounded protobuf `bytes` field;
`MAX_MESSAGE_DATA` is otherwise used only on this crate's outbound TX
validation path, `protocol.rs`, unrelated to concat delivery), and the
existing Fix 2 regression tests already pin cap-plus-one-segment delivery
as the intended, tested behavior (`..._exceeding_max_buf_bytes_...`/
`..._exceeding_max_buf_segments_...` in `events.rs`, asserting
`finalized[0].data.len() == CONCAT_MAX_BUF_BYTES + 1` with a comment
stating the triggering segment is absorbed, not rejected).

**Decision.** Reaffirm Amendment 3 Fix 2's absorb-then-check-then-finalize
shape at both sites — no code or test change. The two rejected
alternatives, weighed against this ADR's own established constraints:

- **Preflight-finalize-then-open-a-fresh-buffer for the triggering
  segment:** breaks in the two most common shapes reachable today. For the
  overwhelmingly common `matches_needed: Some(1)` COP, finalizing the
  existing buffer first sets `matches_got = 1`, so the "open new buffer"
  arm's own quota gate (`matches_got + concat.len() < matches_needed`)
  then rejects the triggering segment anyway — data lost AND the phase
  completes without it, strictly worse than today. At the continuation
  fast-path site specifically, a continuation's payload typically satisfies
  no registered `ExpectedResponse` descriptor at all (the fast path's own
  reason for existing), so "let it open a fresh buffer via the normal
  descriptor-match arm" frequently cannot fire either — the segment would
  still be dropped, just through a more roundabout path.
- **Reject the triggering segment outright:** enforces an exact byte
  ceiling nothing downstream actually consumes, at the direct cost of
  discarding real ECU response bytes — reversing Amendment 3's own
  reviewed "no data is lost" decision for no corresponding correctness
  gain.
- **Split the triggering segment's payload across the cap boundary:**
  rejected as unrepresentable — `ConcatDelivery`/`ResultData` carry no
  continuation marker a client could use to recognize a delivery as the
  tail of a split segment rather than an independent response.

Corrected the `CONCAT_MAX_BUF_BYTES`/`CONCAT_MAX_BUF_SEGMENTS` doc comments
(`j2534-0404-service/src/service/events.rs`) and this ADR's own Amendment 3
Consequences bullet to state the actual delivered bound (`CONCAT_MAX_BUF_
BYTES + MAX_MESSAGE_DATA - 1` bytes / `CONCAT_MAX_BUF_SEGMENTS + 1`
segments) instead of implying an exactly-enforced ceiling neither the
code nor the tests ever provided.

**Consequences.** No functional or test change — this Amendment makes an
already-tested, already-deliberate behavior a durably documented decision
rather than an implicit one, per this repo's requirement that a reviewer
finding be either fixed or explicitly recorded as an accepted limitation
before merge. The caps continue to serve their original Amendment 3
purpose (bounding a buffer's growth across polls and preventing an
indefinite deadline-restart against a noisy sender), not a hard ceiling on
a single delivered response's size — an open buffer, between polls, never
itself exceeds either cap, since no single physical segment's own payload
can exceed `MAX_MESSAGE_DATA`. The Amendment 4/6 note that this ADR's
amendments are due for a Decision-section consolidation still stands and
is unaffected by this Amendment.

This ADR has now been amended seven times in one review loop.

## Amendment 8 (2026-07-29, Codex round-9 P2 finding, extended by edge-case-hunter to a sibling path): `Matched` losing a coexisting `concat_segments_got` delta

**Context.** `check_match_against_baseline` ranks `Matched > Absorbed > PendingRc > NoMatch`. Before this Amendment, a `Matched` result carried only the matches-count delta (`Matched(u32)`), even when the SAME pass also advanced the registrant's `concat_segments_got` — most directly when a byte/segment-cap force-finalize (Amendment 3 Fix 2) runs inside `bind_registrant`, which increments `matches_got` synchronously in the same batch as the absorb that increments `concat_segments_got`. Since `Matched` outranks `Absorbed`, that coexisting segment delta was silently dropped from the report. The wait loop's `Matched` arm (`wait_for_expected_response_inner`) only synced its own local `matches_got`, never its local `concat_segments_got` baseline (unlike the `Absorbed` arm, which does). Consequence: the loop's local baseline fell behind the live registrant by exactly the dropped amount, and the VERY NEXT poll call phantom-reported the already-accounted-for segment as a fresh `Absorbed`, restarting `CP_P2Max` for no real progress — delaying completion and, for a still-open IS-MULTIPLE or finite-N>1 receive phase, potentially admitting a response beyond the intended window.

An edge-case-hunter review of the direct fix (below) found the SAME bug class survives, unfixed, in a structurally separate sibling path: the round-7 deadline-expiry finalize mechanism (`finalize_and_deliver_concat_buffers_if_live`, called from `wait_for_expected_response_inner`'s deadline-expiry branch) drains open concat buffers and advances `matches_got` via its own `delivered_count`, entirely bypassing `check_match_against_baseline`/`PollMatchResult` — so the direct fix's new second field never reaches this path.

**Correction (post-approval, same day):** the original text of this paragraph further justified Fix 2 (below) with a claimed live race: a SIBLING COP's own poll pass — "a second, independently-ticking `wait_for_expected_response_inner` task on the same CLL" — absorbing a segment into this registrant's buffer between this loop's own polls. A design-advisor trace found that scenario unreachable in the current architecture: all `bind_frame` calls for a channel run on that channel's single `poll_channel_events` task (the file's only spawn; every dispatch handler and wait loop is awaited inline in it, so two wait loops on one channel can never tick concurrently — the same serialization the ADR-095 amendment's reap fix already documents), the only genuinely concurrent second poller (a UUDT companion task, ADR-046/ADR-101) is CAN/ISO15765-only while `concat_enabled` requires KWP/J1850, and a concat registrant exists only while its own wait loop is running (inserted at wait entry, removed synchronously at wait exit; the sole persist-past-wait path, `DetachedToTier2`, requires the `-1` shape concat excludes). Every advance of a live `concat_segments_got` therefore passes through this loop's own `check_match_against_baseline` report (`Matched(_, delta)` or `Absorbed(delta)`, both of which sync the local baseline), so the deadline-expiry resync (Fix 2) is a no-op in every reachable configuration today.

**Decision.**

**Fix 1 — `PollMatchResult::Matched` widened to a 2-tuple.** `Matched(u32)` (matches delta only) becomes `Matched(u32, u32)` (matches delta, concat-segments delta), both diffed against their respective caller-supplied baselines in `check_match_against_baseline`. The wait loop's `Matched` arm now does `concat_segments_got += concat_delta` alongside its existing `matches_got += count`, before any of that arm's completion/deadline-reset logic (pure bookkeeping, no behavior change to the rest of the arm). `observe_and_consume_pending_rc_outcome`'s match arm updated for the new arity only (still unconditionally clears `pending_rc` on `Matched`, unchanged from Amendment 5). The `PendingRc` branch needs no equivalent change: reaching it already requires `live_concat_segments_got <= concat_segments_baseline` to hold (per the ranking), so there is by construction no coexisting delta to lose there.

**Fix 2 — `finalize_and_deliver_concat_buffers_if_live` returns the live absolute segment count alongside the delivered-buffer count.** Return type widens from `u32` to `(u32, u32)`: `(delivered_count, live_concat_segments_got)`, the second value read fresh from the registrant inside the SAME `logical_links` lock acquisition the drain itself runs under (safe: the drain touches `concat`/`matches_got`, never `concat_segments_got`). The caller's deadline-expiry branch now does `concat_segments_got = live_concat_segments_got` — an ABSOLUTE assignment, not a delta add, since `delivered_count` (a count of finalized BUFFERS) has no fixed relationship to the total number of SEGMENTS absorbed across them (a buffer force-finalized at the segment cap alone can hold well over one segment) and cannot be used to reconstruct a correct delta the way Fix 1's direct per-pass delta can. **Kept as defense-in-depth, not as a race fix** (per the Correction above): the single-poller channel topology, the companion channel's CAN-only gate, and this ADR's own v1 tier-1-only concat scope are what actually guarantee `live == local` today, none of them local to this code. Fix 2 replaces that distant, multi-premise guarantee with an absolute resync enforced locally, under the same lock as the drain — cheap (two `u32`s, one already-held lock) and correct regardless of whether those premises hold, so it costs nothing to keep even though no reachable configuration currently needs it.

**Consequences.**

- Both fixes are pure bookkeeping-sync corrections; neither changes when a match completes, when a deadline restarts, or any delivered `ResultData` content.
- Regression tests: `check_match_against_baseline_tests::matched_also_reports_a_coexisting_concat_segment_delta` (new, pins Fix 1 at the pure-function level); `observe_and_consume_pending_rc_outcome_tests::cap_triggered_matched_still_clears_a_coexisting_pending_rc` (existing test, updated to assert `Matched(1, 1)` instead of `Matched(1)` — the scenario it already exercised end-to-end via `bind_registrant` turned out to be the exact repro case for Fix 1); `registrant_lifecycle_tests::finalize_and_deliver_concat_buffers_if_live_delivers_and_advances_when_guard_holds` (existing test, extended to assert the returned `live_concat_segments_got` (4, from two 2-segment buffers) is NOT derivable from `delivered_count` (2) alone — pinning Fix 2's absolute-value contract).
- **Accepted residual, unchanged by this Amendment:** the same-ECU cap-triggered-`Matched`-still-discards-`pending_rc` residual (Amendment 5's second accepted-residual bullet) is untouched — this Amendment only fixes the `concat_segments_got` baseline, not `pending_rc`'s own unconditional clear on `Matched`.
- **Forward-looking note:** Fix 2's defense-in-depth framing (above) is not merely hypothetical caution — this ADR's own earlier Consequences section (under the Decision) already flags extending concat to tier-2/created-receive-only registrants as a possible future revision. A tier-2 concat registrant would persist outside any wait loop (unlike today's tier-1-only scope), so its buffer could genuinely be touched by the timer/reap machinery while no wait loop is running — exactly the shape that would make Fix 2's resync load-bearing rather than a no-op. No code change needed here; noted so a future reviewer doesn't have to re-derive why this resync was kept.
- The Amendment 4/6/7 note that this ADR's amendments are due for a Decision-section consolidation still stands, now nine sections deep (Decision + 8 Amendments); still deliberately deferred to a follow-up change rather than done mid-review.

This ADR has now been amended eight times in one review loop.

## Amendment 9 (2026-07-30, Codex round-13 P1 finding): inline force-finalize delivery guarded against the same disconnect race as Amendment 6, at a separate site

**Context.** Amendment 6 closed a disconnect race in the deadline-expiry finalize path (`finalize_and_deliver_concat_buffers_if_live`): a `DisconnectComLogicalLink` landing in an await gap between draining a concat buffer and delivering it could push an already-cancelled COP's response into the retained `rx_buf` queue. A round-13 Codex review found the SAME bug class, unfixed, in a structurally separate site: `bind_registrant`'s two INLINE (non-deadline-expiry) force-finalize triggers — the empty-payload-match arm's force-finalize-all (edge-case-hunter Finding 3, extended by this ADR's original Decision), and the byte/segment-cap hit's force-finalize-one (Amendment 3 Fix 2) — both operate on `entry.registrants`, a per-poll-pass SNAPSHOT captured by `build_cll_rx_entries` under one `logical_links` lock acquisition BEFORE `poll_rx_inner`'s frame loop runs (`CllRxEntry`'s own doc comment documents the full snapshot/mutate/writeback lifecycle: the snapshot is mutated in place across every frame in a poll pass, then reconciled back to live state via `merge_registrant_writeback` only after the whole loop completes). The resulting `finalized_concat: Vec<ConcatDelivery>` was delivered immediately after `bind_frame` returned, via `entry.rx_buf` (the snapshot's own `Arc` clone) — with no re-validation against the LIVE `logical_links` state's `channel_id`/`connect_generation` before delivering. A disconnect landing anywhere across this poll pass's frame loop (which can process several frames, each with its own `.await`s, before this delivery for a given frame runs) could therefore deliver a stale response into the retained `rx_buf`, the same consequence class Amendment 6 already closed for the deadline-expiry path.

This is the mechanism's third occurrence of this exact bug shape (stale-snapshot-derived delivery bypassing a live guard): Amendment 6 (fixed), and Amendment 6's own edge-case-hunter review additionally flagging (but deferring as pre-existing, non-concat, out-of-scope-for-this-PR residuals) two more instances — `poll_rx_inner`'s general per-frame delivery fan-out, and the fast-init synthetic-frame delivery. This round's finding is a fourth instance, but — unlike those two residuals — is concat-specific code this PR itself introduced, so in scope to fix here rather than defer.

**Decision.** A design-advisor trace of the full snapshot/mutate/writeback lifecycle, plus a grep-verified enumeration of every `deliver_or_enqueue` call site in this file, confirmed: this is the ONLY remaining concat-specific site with this gap (the two non-concat residuals above are unaffected and remain correctly deferred). Added `deliver_concat_batch_if_live` (`j2534-0404-service/src/service/events.rs`, alongside `finalize_and_deliver_concat_buffers_if_live`): acquires `logical_links` once, applies the same ADR-086 `channel_id`/`connect_generation` staleness guard Amendment 6 established, and on success delivers every entry in the batch while STILL HOLDING that lock (the same nesting Amendment 6 already sanctioned — `deliver_or_enqueue` only ever acquires the innermost per-CLL queue lock, never `logical_links`, so this doesn't reverse the documented `logical_links -> subscriptions -> per-CLL queue` lock order). On guard failure, the WHOLE batch is silently dropped, never partially delivered. `poll_rx_inner`'s call site now calls this helper instead of delivering directly via the snapshot's own unguarded `entry.rx_buf`.

Drop-on-stale-guard is self-consistent with the snapshot/writeback lifecycle, verified against both reachable teardown shapes: a **reconnect** bumps `connect_generation`, failing this guard directly, and the writeback's own per-registrant lookup then also misses (a fresh registrant list after reconnect has no entry matching this pass's snapshot); a **disconnect without reconnect** instead clears `channel_id` (leaving `connect_generation` itself unchanged) and `cancel_link_cops` clears `registrants` outright, so this guard's `channel_id` comparison fails directly, and the writeback separately no-ops via its own registrant-not-found branch — different mechanisms on each side, same outcome (delivery dropped and the paired `matches_got` contribution discarded, together, never diverging) either way, since `matches_got` was only ever advanced on the SNAPSHOT copy by these same force-finalize calls.

**Consequences.**

- Closes the last concat-specific instance of this bug shape; the residual is exactly the two already-accepted non-concat sites (Amendment 6), nothing new.
- Regression tests: `registrant_lifecycle_tests::deliver_concat_batch_if_live_stale_generation_drops_whole_batch` and `..._delivers_all_entries_in_order_when_guard_holds` (pure-function level, mirroring Amendment 6's own test trio); `tests/grpc_mock/concat.rs::concat_empty_payload_match_finalizes_via_inline_guarded_delivery` (integration level, per an edge-case-hunter finding that the pure unit tests alone don't confirm the real call site's `ctx.channel_id`/`entry.connect_generation` wiring — this test exercises the empty-payload-match trigger through the actual `poll_rx_inner` path, confirming the new call site is genuinely reachable and wired correctly, not re-proving the disconnect-race-closing property itself, which the unit tests already cover in isolation).
- **Accepted residual, documented not fixed (edge-case-hunter finding on this Amendment's own review round):** `deliver_concat_batch_if_live`'s guard compares the caller's `channel_id` against the live link's `channel_id` — for a UUDT companion poll pass, that comparison would always fail, since a companion entry's own polling identity is deliberately the companion channel, not the primary one. Currently harmless only because `concat_enabled` is KWP/J1850-only while a UUDT companion is CAN-only (ADR-046) — the two invariants never coexist today. Would need revisiting if concat's scope is ever widened to CAN/ISO15765, the same possible-future-extension case Amendment 8's Fix 2 residual note flags for the deadline-expiry path's own resync. No code change needed now; documented (both here and in the function's own doc comment) so a future reviewer doesn't have to re-derive it.
- The Amendment 4/6/7/8 note that this ADR's amendments are due for a Decision-section consolidation still stands, now ten sections deep (Decision + 9 Amendments); still deliberately deferred to a follow-up change rather than done mid-review.

This ADR has now been amended nine times in one review loop.

## Amendment 10 (2026-07-30, Codex round-14 P1 finding): open concat buffers discarded, not carried across, an RC21/RC23 retransmit

**Context.** Round 14 found that the NRC 0x21 (BusyRepeatRequest) / 0x23 (ConditionsNotCorrect) handling in `wait_for_expected_response_inner` retransmits the original request (`transmit_request`) without ever touching `registrant.concat`. `bind_registrant`'s continuation fast path absorbs any frame whose `(unique_resp_identifier, source_id, SID)` key matches an already-open buffer, bypassing the full descriptor entirely — so a response arriving after the retransmit, from the same ECU/SID, was silently appended to a buffer opened BEFORE the retry, merging bytes from two distinct physical request/response exchanges into one corrupted logical response. Concretely: for an IS-MULTIPLE functional request, ECU A can have a partial same-SID response buffered when ECU B returns NRC 0x21/0x23; the retransmit re-sends the request to every ECU, but ECU A's stale buffer was left open to catch its next reply as a false continuation.

**Decision.** At the retransmit boundary, discard (never finalize-and-deliver) every open concat buffer on the registrant — added `discard_concat_buffers_for_retransmit_if_live` (`j2534-0404-service/src/service/events.rs`), folded into the pre-existing `retransmit_still_on_this_channel` ADR-086 liveness check as one `logical_links` acquisition (the same atomicity precedent Amendments 6 and 9 established): `!skip_retransmit && discard_concat_buffers_for_retransmit_if_live(...).await`. Touches only `r.concat` (`r.concat.clear()`) — `matches_got`/`concat_segments_got` are deliberately untouched, since the monotone-baseline diff contract `check_match_against_baseline` and Amendment 8's resync both depend on `concat_segments_got` never decreasing, and discarding buffer contents doesn't change what the wait loop's own local baseline already reflects.

**Discard, not finalize-and-deliver — the deciding factor is quota, not just data fidelity.** `finalize_concat_buffers` increments `matches_got` once per drained buffer. For the common `matches_needed: Some(1)` case, finalizing at the retry boundary would satisfy the WHOLE COP with truncated pre-retry data, and the real, complete post-retry answer would then never be delivered — permanently, not just delayed. This is worse than Amendment 3's cap-hit precedent ("the triggering segment itself is still absorbed... no data is lost"), where the buffer being force-finalized is the ECU's own genuine, uninterrupted answer; here the buffer is definitionally an interrupted partial, with a strictly better complete replacement specifically expected imminently. This is a deliberate asymmetry with the deadline-expiry finalize path (`finalize_and_deliver_concat_buffers_if_live`): at deadline expiry the phase is ending and no better data will ever come, so partial data beats none; at the RC21/23 retry boundary, better data is coming, so partial data is worse than none.

**Scope: every open buffer on the registrant, both codes identically.** IS-MULTIPLE's single shared `CopRegistrant` (this ADR's own Amendment 1 correction) holds buffers for multiple ECUs; the retransmit re-sends the ONE functional/broadcast request every one of those buffers was answering, so every buffer crosses the same attempt boundary regardless of which ECU's NRC triggered the retry. 0x21 and 0x23 share identical retransmit semantics in this codebase (only their completion-timeout/request-time config values and ceiling-tracking slots differ) — no basis for treating them differently here. NRC 0x78 (ResponsePending) is exempt by construction: it reloads the deadline but never retransmits, so no new request attempt exists and no cross-attempt merge hazard arises.

**`skip_retransmit` short-circuit is deliberately preserved.** `!skip_retransmit && ...` means the discard (and its lock acquisition) never runs when the retransmit itself is being skipped (a `match_reset_ceiling` — CoptStopcomm's IS-MULTIPLE ceiling, ADR-087 — already elapsed). In that case no new request goes out, so the open buffers remain valid continuations of the SAME still-outstanding attempt; they are correctly left for the normal deadline-expiry finalize path to handle once the phase's own ceiling-clamped deadline arrives.

**Consequences.**

- Regression tests: `registrant_lifecycle_tests::discard_concat_buffers_for_retransmit_if_live_clears_open_buffers_when_guard_holds` (also pins `matches_got`/`concat_segments_got` staying exactly unchanged), `..._stale_generation_leaves_buffers_untouched`, `..._guard_holds_no_open_buffers` (the established guard-holds/stale/no-op trio, mirroring Amendments 6 and 9); `bind_frame_tests::discard_then_same_key_frame_opens_a_new_buffer_instead_of_continuing_the_old_one` (the actual protocol-correctness guarantee, pinned at the `bind_registrant` level: post-discard, a same-key frame falls through to the "open new buffer" arm with no trace of the discarded pre-retry bytes, rather than fast-path-absorbing into nothing).
- **Accepted residual, documented not fixed:** a segment from the PRE-retry attempt that is already queued in the underlying adapter/mock (received on the wire but not yet polled by this service) at the moment the retransmit goes out is polled only AFTER the retransmit — since raw KWP/J1850 carries no attempt/sequence identifier at this protocol layer, such a segment is physically indistinguishable from a genuine post-retry response and can still satisfy the full descriptor match (not just the now-empty fast path), opening a fresh buffer that a genuinely-new-attempt segment sharing the same key could then merge into. This is narrower than the bug just fixed: it requires a specific timing window (old data arriving after the retransmit specifically) rather than being deterministic on any post-retry same-key response, and it's the same category of no-header-disambiguation residual this ADR already accepts elsewhere (Amendment 3's headerless/short-header grouping). Not fixed here — draining/polling RX before retransmitting to flush queued old-attempt data was considered and rejected as disproportionate complexity (would re-enter match/RC handling mid-arm, risking new pending-RC reentrancy) for a residual this narrow.
- The Amendment 4/6/7/8/9 note that this ADR's amendments are due for a Decision-section consolidation still stands, now eleven sections deep (Decision + 10 Amendments); still deliberately deferred to a follow-up change rather than done mid-review — this is now the strongest signal yet that the consolidation should happen promptly once this review loop closes.

This ADR has now been amended ten times in one review loop.

## Amendment 11 (2026-07-30, merge with `main`): `bind_registrant` unified to carry both `absorbed` and a fresh `CP_SuspendQueueOnError` classification

**Context.** While this PR's review loop was still open, `main` independently merged ADR-147 (`CP_SuspendQueueOnError` — a third, content-triggered TX-queue-suspend source). Both ADRs extend `bind_registrant`'s return value's third tuple element in incompatible ways: this ADR's `absorbed: bool` (was this frame consumed into a concat buffer instead of completing a match) versus ADR-147's `classification: Option<QueueErrorClass>` (this bound frame's fresh suspend/positive/no-effect verdict, judged against the registrant's own `rc_cfg`). Merging this branch onto `main` required resolving that conflict, which raised a genuine cross-feature design question, not a mechanical one: does a frame absorbed into an open concat buffer (a continuation segment, or one opening a fresh buffer) still get an ADR-147 classification, or does it default to `None` (no queue effect) the same way a pending-RC match already does?

**Decision.** `bind_registrant`'s return widens to a 4-tuple, `Option<(u32, u32, bool, Option<QueueErrorClass>)>` — `(acceptance_id, cop_handle, absorbed, classification)`. Classification is computed for absorbed frames too, via a new shared helper, `classify_queue_error(rc_cfg, payload)`, called at every accepted-frame return site (the continuation fast path, the open-new-buffer/absorb-into-existing arm, and the genuine-match arm) — not left at `None`. Rejected the `None`-for-absorbed alternative after re-deriving it against ADR-147's own governing rule in both directions: since every non-empty-payload frame a concat-enabled registrant accepts returns `absorbed: true` until the receive-phase deadline (the genuine "match" only happens at deadline-time `finalize_concat_buffers`, outside any poll pass and therefore outside `bind_registrant`'s fold entirely), leaving absorbed frames unclassified would mean NO positive response on a concat-enabled COP ever clears `tx_suspended_by_error` (a total loss of ADR-147's auto-resume semantics for KWP/J1850 traffic, not merely a delay), and a confirmed-unhandled `7F <sid> <nrc>` that opens a fresh concat buffer would silently fail to trigger auto-suspend at all (ADR-147's own paradigm trigger case). `classify_queue_error` folds in ADR-147's existing decline-to-classify heuristic unchanged (a 0x7F-led payload that isn't a CONFIRMED unhandled negative response classifies `None`, not a false `Positive`) — a concat continuation absorbed into a buffer is essentially never 0x7F-led in practice (it is definitionally the tail of an already-open positive response), so this resolves to `Some(Positive)` for the overwhelming majority of absorbed frames, exactly the resume evidence ADR-147 exists to observe. The pending-RC early return (unrelated to either concat or this classification question) is unaffected: it stays `(0, cop_handle, false, None)`, unchanged in shape from before the merge.

`bind_frame`'s signature additively merges both ADRs' parameters (`concat_meta`/`finalized_concat` from this ADR, `wrote_suspend: &mut bool` from ADR-147) — no design question there. `poll_rx_inner`'s call site sequences ADR-147's eager `tx_suspended_by_error` publish-before-exposure block STRICTLY BEFORE this ADR's `deliver_concat_batch_if_live` call, not after: `deliver_concat_batch_if_live` is itself a form of client-visible exposure (a `ResultData` reaching a subscriber), so ADR-147's own publish-before-ANY-exposure invariant (guarding against a dispatch task observing stale suspend state the instant something this same `bind_frame` call produced becomes visible) applies to it exactly as it does to this frame's own delivery — a constructible scenario (a same-key absorb into a buffer that both confirms an unhandled negative response AND trips the segment-count cap in the same call) would violate that invariant if concat delivery ran first.

**Consequences.**

- A partially-received multi-segment response now counts as `CP_SuspendQueueOnError` resume evidence starting at its FIRST absorbed segment, not only once the full response is later finalized and delivered — accepted as correct, not a residual: this is the same evidential grade ADR-147 already assigns to any single-frame `Positive` classification (per-frame wire evidence, not per-completed-match).
- No classification is computed at buffer-finalize time (the receive-phase deadline path, or a `finalized_concat` entry) — a finalize event adds no wire evidence beyond what its constituent segments already carried at absorb time, and wiring a new write site into `wait_for_expected_response_inner`'s deadline path would reopen ADR-147's own multi-amendment sequencing/anchoring mechanism for no benefit.
- Regression tests: every existing `bind_registrant`/`bind_frame` unit test in `registrant_lifecycle_tests`/`bind_frame_tests` (and the sibling scan-precedence/pending-RC test modules) was widened for the new 4-tuple shape, asserting the correct `classification` for its own scenario's payload (not defaulted to `None`) — this ADR's own tests assert `Some(QueueErrorClass::Positive)` throughout, since none of this ADR's fixtures use a 0x7F-led payload.
- See `docs/adr/ADR-147-cp-suspend-queue-on-error-tx-suspend-source.md` for the reciprocal cross-reference; that ADR's own Decision text is unaffected by this merge (its `classify_queue_error`-equivalent logic moved verbatim into the new shared helper, no behavior change for any non-concat registrant).
- The Amendment 4/6/7/8/9/10 note that this ADR's amendments are due for a Decision-section consolidation still stands, now twelve sections deep (Decision + 11 Amendments) — deliberately still deferred, but this merge amendment is itself evidence the append-only format is past its useful life; strongly recommended as the very next follow-up once this PR closes.

This ADR has now been amended eleven times in one review loop.

## Amendment 12 (2026-08-21, `design-advisor` consult on an `edge-case-hunter`-flagged residual): general same-key collision between independent exchanges is spec-conformant behavior, not a bug

**Context.** `bind_registrant`'s two absorb arms (`events.rs`, the continuation fast path and the open-new-buffer/absorb-into-existing arm) merge any frame whose `(unique_resp_identifier, source_id, SID)` matches an already-open buffer, with no check that the frame is actually a continuation of that buffer's own in-progress response rather than a wholly separate, independent exchange that happens to share the same key (an ECU retransmission, a repeat/cyclic exchange, or two distinct requests answered with the same SID). An `edge-case-hunter` review confirmed this via a repro: two independent 7-byte `[0xC3, 0x02, ...]` responses collapsed into one garbled 13-byte `ConcatBuf::data`. Recorded as a P2 backlog item (`j2534-0404-service/docs/implementation-notes.md`) pending a `design-advisor` consult on whether a fix is possible, distinct from Amendment 10's narrower, already-fixed case (the service's own NRC 0x21/0x23 retransmit boundary).

**Decision.** No mechanism added. ISO 22900-2:2022 Table B.20's `CP_EnableConcatenation` description (paraphrased, not quoted) defines the ComParam's entire decision procedure as: use only the SID (first data byte) to recognize a segmented response, and wait for the receive timeout to decide all segments have arrived. SID-plus-window is not an implementation shortcut standing in for a more precise rule — it *is* the rule. A service-invented discriminator (inter-segment timing narrower than the receive window, content/duplicate-frame detection, or tighter segment/byte caps) would each deviate from that rule rather than refine it, and each mishandles a constructible genuine continuation: a real continuation may legitimately arrive anywhere up to the full `CP_P2Max`-class window (the reason each absorb already restarts that deadline), KWP/J1850 carry no sequence or total-length field to dedupe against (Amendment 10's own residual note), and a byte-identical continuation is legal, so duplicate-suppression would silently corrupt it. Enabling `CP_EnableConcatenation` (default `0`/disabled on every eligible protocol) is the client declaring "same-SID frames from one ECU in this window are one logical response"; an ECU that emits two independent same-SID responses in one window is exactly the configuration the ComParam instructs this layer to merge.

Unlike Amendment 10's retransmit boundary — an externally-knowable event this service itself controls, which is what made discarding the open buffer at that exact moment a principled fix — the general case has no equivalent service-side event to trigger a discard or a force-finalize-and-deliver (`finalize_one_concat_buffer`) against. A heuristic-triggered finalize would in fact be worse than the option Amendment 10's own reasoning already rejected for its narrower case: a false positive both truncates a genuine in-progress response and consumes `matches_got` quota with the truncated data, permanently starving the COP of the real answer for the common `matches_needed: Some(1)` case — with none of the "a better replacement is expected imminently" mitigation that made Amendment 10's discard-not-finalize choice sound.

Reachability is scoped to `CP_EnableConcatenation`-eligible protocols only (KWP, SAE J1850 — CAN/ISO15765 and SCI are out of scope per this ADR's own Decision 1). A multi-send-cycle COP (`num_send_cycles > 1`) does not additionally expose this: each cycle re-enqueues its own `TxItem::SendRecv` and inserts/removes its own registrant, so no `ConcatBuf` can survive across a cycle boundary. The service's own RC21/23 retransmit boundary remains covered by Amendment 10, including that Amendment's own narrower accepted residual (a pre-retry frame already queued in the adapter when the retransmit fires).

Also corrects `events.rs`'s continuation-fast-path absorb-arm comment, which mislabeled this general collision as "ADR-148's own pre-existing, already-documented residual (see ADR-148's Amendment 10)" — Amendment 10's own residual is the narrower queued-pre-retry-frame window, not this general same-key case; the comment now cites this Amendment instead.

**Consequences.**

- Documented sharp edge, not a defect: a client that needs to disambiguate independent same-SID exchanges from genuine continuations must do so by not enabling `CP_EnableConcatenation` on that link (or by disabling it once such ECU behavior is observed) — this is a configuration choice available today, not a missing capability.
- `j2534-0404-service/docs/comparam-protocol-support.md`'s `CP_EnableConcatenation` row gains a one-line caveat pointing here.
- The P2 backlog entry this consult resolved is removed from `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog — the design decision it was waiting on is now made and recorded here.
- Revisit if `CP_EnableConcatenation` scope ever widens beyond KWP/J1850 to a protocol whose wire format carries a genuine sequence/attempt identifier (the "no discriminator exists" premise this Decision rests on would no longer hold for that protocol), or if a future conformance audit pins this service to the 2009(E) edition's slightly different `CP_EnableConcatenation` protocol list (this ADR's existing Consequences already flag that edition-drift risk generally; the description text itself is materially the same across both editions for this specific ComParam, so this Decision is not expected to change, but re-verify before assuming so).
- The Amendment 4/6/7/8/9/10/11 note that this ADR is overdue for a Decision-section consolidation still stands — not performed as part of this documentation-only amendment.

This ADR has now been amended twelve times in one review loop.
