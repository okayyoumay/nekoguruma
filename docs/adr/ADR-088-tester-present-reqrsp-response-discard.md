# ADR-088: CP_TesterPresentReqRsp Discards the ECU's Tester-Present Response

**Date:** 2026-07-15
**Status:** Accepted (amended 2026-07-16 — see Amendment below; mode-0 discard scoping further
superseded by ADR-093 same-day — see note at the end of the Amendment; second amendment 2026-07-16 —
see Second Amendment below, fixing a live-vs-snapshot mismatch the first Amendment's own
`discard_until` field still had; extended by ADR-099 2026-07-17 to also discard SOM/TX_DONE/loopback
indication frames, and to open `discard_until` on every successful send regardless of
`CP_TesterPresentReqRsp` — this ADR's content-prefix discard mechanism itself is otherwise unchanged;
precedence rule ("a pending COP's own claim always wins over discard") superseded by ADR-100
2026-07-17, which replaces probe-then-discard with an explicit attribution precedence table)
**Affects:** `j2534-0404-service` service, events, rpc_primitive, comparam_defaults

## Context

`CP_TesterPresentReqRsp` (`0x8007`) already existed as a stored-only ComParam — settable/gettable,
with per-protocol defaults and protocol-support declarations, but no runtime behavior — the same
tier as its siblings `CP_TesterPresentHandling`/`CP_TesterPresentAddrMode`/`CP_TesterPresentTime_Ecu`.
ISO 22900-2 defines it as: `0` = the ECU sends no reply to a tester-present message; `1` = a
reply is expected, and **the MVCI protocol module itself swallows that reply** instead of passing it on
(see `CP_TesterPresentExpPosResp`/`CP_TesterPresentExpNegResp` for recognizing it).

Before this ADR, `poll_rx_inner` (`events.rs`) unconditionally pushed every RX frame routed to a
CLL into that CLL's `rx_buf` and fanned it out to the client as an unsolicited `ResultData` — there
was no general "drop this frame" path. Any ECU reply elicited by a tester-present send (mode 0
periodic `PassThruStartPeriodicMsg`, or mode 1 idle-triggered via `send_idle_tester_present_once`)
that matched a CLL's UniqueRespIdTable routing was therefore delivered to the client exactly like
any other frame — the opposite of what `CP_TesterPresentReqRsp = 1` requires.
`CP_TesterPresentExpPosResp`/`CP_TesterPresentExpNegResp` (bytefield ComParams, e.g. UDS defaults
`[0x7E]` / `[0x7F, 0x3E]`) were themselves stored but never read back for matching — no comparam in
this family had ever driven response-side behavior.

Scope for this change (confirmed with the requester before implementation): build the actual
discard behavior, not just wire the value into unrelated bookkeeping.

## Decision

Per poll tick, `build_cll_rx_entries` computes a new `CllRxEntry.tester_present_discard:
Option<(Vec<u8>, Vec<u8>)>` — `Some((exp_pos_resp, exp_neg_resp))` only when **both** hold for that
CLL's live Active set: tester-present is actually armed (`TesterPresentState::Idle{..}` or
`Periodic(_)`), and `CP_TesterPresentReqRsp == 1`. `None` otherwise, which reproduces prior behavior
exactly (nothing is ever discarded) — deliberately guards against a CLL that left
`CP_TesterPresentReqRsp` at a preset default of `1` without ever configuring a tester-present
message (`CP_TesterPresentMsg` empty), which must not start eating unrelated frames just because
the ComParam happens to be `1`.

In `poll_rx_inner`'s per-delivery loop, immediately after the existing `MatchProbe` computes
`(acceptance_id, frame_cop_handle)` for a delivered frame and before it is buffered/fanned out: if
`frame_cop_handle.is_none()` (this frame was **not** already claimed as advancing some pending COP's
own `expected_response`/pending-RC wait) and the frame's payload (post header/footer split, the same
slice COP matching already uses) `starts_with()` either configured, non-empty `ExpPosResp`/`ExpNegResp`
prefix, the frame is dropped — skipped from both `push_rx_frame` and the subscription fan-out — for
that one delivery only.

Design choices, reviewed and confirmed (`design-advisor`) before landing:

- **A pending COP's own claim always wins over this discard.** Mirrors the existing precedent in
  the same function where a pending-RC frame (0x78/0x21/0x23) already takes priority over a broader
  pattern match — a genuine client-awaited transaction is never sacrificed to this housekeeping
  discard.
- ~~**No `unique_resp_id` restriction.**~~ **Corrected by the Amendment below** — this held only
  under the original (flawed) content-only mechanism. Once discard is scoped to the actual response
  to a request tester-present itself sent, a *physically* addressed tester-present has one, single,
  identifiable target, and the reasoning that follows no longer applies to that case. Kept here,
  struck through, as the historical record of what this ADR originally argued. `CP_TesterPresentExpPosResp`/`ExpNegResp`
  are plain per-CLL byte-vector ComParams with no addressing dimension in the spec, and tester-present
  can legitimately be sent functionally to multiple ECUs (the mode-0 periodic send's actual target
  isn't even visible to this service — ADR-083) — this part still holds for **functional** addressing,
  where there genuinely is nothing to restrict *to*.
- **Prefix (`starts_with`) matching, not `ExpectedResponse`'s mask/pattern.** The stored bytes are a
  plain `Vec<u8>` (bytefield ComParam), not a mask+pattern pair, so mask semantics have nothing to
  apply to. Full-message equality would also break the shipped UDS defaults themselves: `[0x7E]`
  against an actual `7E 00` subfunction-echo reply would never match. Prefix matching is the only
  reading that both fits the stored type and matches the shipped defaults.
- **NOTE 19 ("any change to a tester-present ComParam resends immediately") is read as scoped to
  ComParams that affect what's transmitted or when** — the set ADR-084's `same_wire_behavior` already
  covers generically (`data`/`interval_ms`/`tx_flags`/`isotp_framing`/`send_type`/`can_functional`).
  `CP_TesterPresentReqRsp`/`ExpPosResp`/`ExpNegResp` change RX classification, not the outgoing frame
  or send cadence, so they are deliberately **not** added to that gate — doing so would resend a
  spurious frame on a pure RX-policy change, the exact failure mode ADR-084's diff gate exists to
  prevent. NOTE 19's "changes take effect" intent is satisfied on the RX side regardless:
  `build_cll_rx_entries` reads `l.active` fresh every poll tick, so a live `CoptUpdateparam`
  promoting these three ComParams changes discard behavior within one tick, with no re-arm needed.

## Consequences

- A tester-present ECU response, once discarded, never reaches the client — by design, per spec.
- **Accepted residual — byte-pattern ambiguity, pre-existing, not introduced by this change:** a
  frame that is byte-identical to a configured `ExpNegResp` plus a pending-response NRC (e.g.
  `0x7F 0x3E 0x78`) arriving while some COP is probing the same CLL is claimed by the existing
  pending-RC branch (`frame_cop_handle = Some`) and will spuriously extend that COP's wait, rather
  than being recognized as the tester-present's own reply. This ambiguity already existed for any two
  services sharing NRC byte shapes; not addressed here.
- **Accepted residual:** a client's own explicit tester-present-service (`0x3E`) `CoptSendrecv`
  whose `expected_response` pattern fails to match the ECU's actual reply (e.g. the pattern covers
  only the positive-response shape but the ECU sends `0x7F 0x3E xx`) will now have that reply
  discarded by this mechanism instead of delivered unsolicited, if `CP_TesterPresentReqRsp == 1` and
  the reply happens to match `ExpNegResp`'s prefix. The frame is indistinguishable, at the byte
  level, from a genuine tester-present reply; discard is the spec-consistent tiebreak given the
  ambiguity is unresolvable without a mask.
- **Accepted residual — one-tick disarm window:** `build_cll_rx_entries` snapshots
  `tester_present_discard` once per poll tick; a tester-present response still in flight when
  `CoptStopcomm` disarms tester-present between snapshot and delivery is delivered rather than
  discarded. Bounded by one poll interval; not fixed.
- ~~**Accepted residual — legacy-OBD preset collision** ... narrowed `CP_TesterPresentExpNegResp` to
  `[0x7F, 0x01]` ...~~ **Superseded by the Amendment below.** This entire residual, and the preset
  default-value change it led to, were an artifact of the original (flawed) content-only mechanism:
  once discard is scoped to a bounded window after tester-present's own actual send (the Amendment's
  mode-1 fix — and these four presets are all mode 1, `CP_TesterPresentSendType = 1`), an unclaimed
  reply to a client's own, independently-timed Mode 01 PID 00 request is no longer a discard
  candidate merely because it shares `ExpPosResp`'s bytes; it would have to additionally land inside
  the few-hundred-ms window right after one of tester-present's own 2-second-interval sends. The
  `CP_TesterPresentExpNegResp` preset values were reverted to their original unscoped `[0x7F]` — see
  the Amendment — since narrowing them was solving a problem the corrected scoping no longer has.
- ~~**Deferred, tracked in `j2534-0404-service/docs/implementation-notes.md`'s backlog, not fixed
  here:** `send_idle_tester_present_once`'s `TxGapState.no_response_required` is still hardcoded to
  `true` regardless of `CP_TesterPresentReqRsp` — a `CP_P3Func`/`CP_P3Phys` gap-timing classification
  question (ADR-060), independent of the discard mechanism this ADR adds.~~ **FIXED by ADR-093
  (2026-07-16):** `ResolvedTesterPresent::expects_response` now derives `no_response_required`/
  `wait_for_p3_gap`'s `num_receive_cycles` from `CP_TesterPresentReqRsp` in the shared send path both
  modes use.

## Amendment — 2026-07-16: Response Discard Scoped to the Actual Request

**This amendment landed the day after the original decision above merged, after the requester (a
domain expert) reviewed it post-merge and corrected it.** Their point, verbatim: the ComParams this feature
discards against ("handled responses are just ones to requests that is sent by this tester present's
handling") describe the response to a request tester-present *itself* transmitted — not a bare
content filter applied to every unclaimed frame on the CLL for as long as tester-present stays armed.
The original Decision above discarded any unclaimed frame matching `ExpPosResp`/`ExpNegResp`'s prefix
regardless of whether that specific frame was ever actually elicited by a tester-present send — a
genuine over-broad bug, not merely an edge case, and the reason the preset-defaults change in the
original Consequences (narrowing `CP_TesterPresentExpNegResp` to `[0x7F, 0x01]`) is reverted below:
it was solving a problem that does not exist once the mechanism is correctly scoped.

### Corrected decision

**Mode 1 (`CP_TesterPresentSendType = 1`, idle-triggered) is window-scoped to the actual send.** This
service calls `transmit_request` itself for every mode-1 send (`send_idle_tester_present_once`), so it
knows exactly when each one happened. `TesterPresentState::Idle` (`service.rs`) gains a
`discard_until: Option<tokio::time::Instant>` field — `Some(fired_at + CP_P2Max)` after a
**successful** send, `None` after a failed one — set at all three call sites that construct or update
`Idle` (`handle_start_comm`'s arm, `handle_update_param`'s re-arm, `dispatch_due_idle_tester_present`'s
per-tick send), each reading `CP_P2Max` from the live/bound ComParamSet in effect for that send
(mirroring `handle_send_recv`'s own response-window source, ADR-053). `build_cll_rx_entries` now
treats mode-1 tester-present as a discard candidate only while `tokio::time::Instant::now() <
discard_until` — a frame arriving outside that window was never actually elicited by this CLL's
tester-present and must not be touched, no matter how closely it matches the configured prefix.

**~~Mode 0 (`CP_TesterPresentSendType = 0`, hardware-autonomous periodic) keeps the original armed-wide
prefix match, unchanged.~~ Superseded by ADR-093 (2026-07-16):** mode 0 no longer uses
`PassThruStartPeriodicMsg` — it is dispatched by this service's own software poll loop exactly like
mode 1, so it now has a real send instant to window against and gets the identical
`discard_until`-scoped treatment described above. The paragraph below is kept as the historical
record of why the armed-wide match was originally accepted for mode 0.

~~`PassThruStartPeriodicMsg` gives this service no per-tick visibility into
individual periodic sends (ADR-083's already-accepted, unfixable J2534 API limitation) — there is no
send instant to window against. A synthetic window on the configured interval's cadence was
considered and rejected: it would fabricate a send instant/phase this service does not actually know,
which is neither correct nor conservative (ADR-083 rejected the analogous idea for a different
mode-0 gap elsewhere, for the same reason). The armed-wide prefix match remains the closest
achievable reading of `CP_TesterPresentReqRsp = 1` for mode 0, and is the reason the physical-target
restriction below applies there too — it is mode 0's only available tightening.~~

**Physical addressing now restricts discard to the actual target's physical CAN ID(s), in both modes —
sourced differently per mode, and matched directly against the frame's own raw CAN ID, never through a
live `unique_resp_identifier` (uid) lookup.** A new `TesterPresentTargetCanIds { usdt: Option<u32>,
uudt: Option<u32> }` (`service.rs`) holds the target UniqueRespIdTable entry's own
`CP_CanRespUSDTId`/`CP_CanRespUUDTId`, `Some(first entry's ids)` when addressing resolves to CAN-family
**physical** (`can_functional == Some(false)`, non-empty table; the same derivation
`rpc_primitive::resolve_tester_present`'s own `can_functional` uses), else `None`. `poll_rx_inner`'s
discard check requires `discard.target_can_ids.is_none_or(|ids| frame_can_id.is_some_and(|id| Some(id)
== ids.usdt || Some(id) == ids.uudt))` alongside the existing prefix match — `frame_can_id`, the raw
4-byte CAN ID already extracted per received message earlier in `poll_rx_inner`, not the frame's
live-resolved `unique_resp_identifier`. `None` for **functional** (broadcast) addressing — every ECU
that answers a functional tester-present is a legitimate reply, so the original Decision's "nothing to
restrict to" reasoning still holds there — and for non-CAN-family/no-table CLLs.

**Both modes now read `target_can_ids` from a value frozen at whichever arm/re-arm actually resolved
it — neither mode recomputes it live from the current table.** `TesterPresentState::Periodic` changed
from a bare `PeriodicMessageId` tuple to `Periodic { id: PeriodicMessageId, target_can_ids:
Option<TesterPresentTargetCanIds> }`; `TesterPresentState::Idle` reads it straight off its own existing
`resolved: ResolvedTesterPresent` field (no new state needed there — `resolved` was already threaded
through). Both are populated from a new `ResolvedTesterPresent::target_can_ids` field (computed once in
`resolve_tester_present`, excluded from `same_wire_behavior` for the same reason
`CP_TesterPresentReqRsp`/`ExpPosResp`/`ExpNegResp` already are — RX classification, not wire content).
`SetUniqueRespIdTable`/`CoptUpdateparam`'s `promote_unique_resp_id_table` runs unconditionally,
regardless of tester-present mode or whether a re-arm actually happens — three review rounds, each
before this amendment was ever committed, progressively found every way a live-vs-frozen mismatch could
reopen the exact over/under-broad-discard bug class this amendment exists to close:

- **Round 1 (design-advisor/edge-case-hunter, mode 0):** an edge-case-hunter pass caught that a *live*
  recompute for mode 0 would drift from `PassThruStartPeriodicMsg`'s actual, permanently-frozen target
  the moment a later `CoptUpdateparam` reordered or replaced the table's first entry — un-discarding the
  true target's replies and wrongly discarding whichever entry became first. Fixed by freezing at arm
  time, initially as `resp_uid: Option<u32>` (the target entry's `unique_resp_identifier` label).
- **Round 2 (Codex review of the PR carrying round 1's fix, mode 0):** freezing the *uid* alone is
  still insufficient. `route_frame` always resolves an incoming frame's uid from whichever CAN-ID-to-uid
  pairing is live *right now* — a later `SetUniqueRespIdTable` can reassign the SAME uid to a
  *different* CAN ID (or swap which uid a given CAN ID resolves to), which a mere reorder never
  exercises (reordering leaves every uid's own pairing untouched, so live resolution for any given CAN
  ID is unchanged regardless of position). A frozen uid would then compare against a *live-resolved*
  uid that no longer means what it meant at arm time — silently reintroducing both failure directions
  from round 1, now via relabeling instead of reordering. Fixed by freezing the raw CAN ID(s) instead
  of the uid label (`target_can_ids`, matched directly against `frame_can_id`), which sidesteps the
  live uid lookup entirely — no later table update, of any kind, can change what a frozen CAN ID means.
- **Round 3 (Codex review of the PR carrying round 2's fix, mode 1):** mode 1 (`Idle`) had the SAME
  divergence, via a different path — its `target_can_ids` was still being recomputed live under the
  (wrong) assumption that mode 1's ADR-084 re-arm gate would always catch a relevant addressing change.
  It does not: `same_wire_behavior` compares `data` (built from `CP_CanPhysReqId`, the *request*/TX
  addressing), not `target_can_ids` — a `CoptUpdateparam` that swaps only the *response* addressing
  (`CP_CanRespUSDTId`) between two entries, leaving each entry's own `CP_CanPhysReqId` (and so the
  transmitted bytes) unchanged, does not re-arm at all. `dispatch_due_idle_tester_present`'s own
  per-tick send never re-resolves `resolved`/`framed_data` between arms either, so every send inside an
  already-open `CP_P2Max` window genuinely went out (and expects a reply) against whatever `resolved`
  said at the last arm/re-arm — a live recompute would silently move the discard target to the new
  table mid-window. Fixed by reading `resolved.target_can_ids` directly (already frozen there, since
  `resolved` is only replaced at an actual arm/re-arm) instead of recomputing from the live table — a
  net simplification, not new state.
- **Round 4 (Codex review of round 3's fix, mode 1's arm site specifically):** `handle_start_comm`'s
  mode-1 arm sized `discard_until` from `binding.resolved().p2_max_timeout_ms()`. Under
  `temp_param_update = 1`, `binding` is `ParamBinding::Temp { effective }` — the transient Working
  snapshot bound for the init transaction, whose hardware config is already reverted by the time
  tester-present arms — while tester-present itself is always resolved from the call-time **Active**
  snapshot regardless of `temp_param_update` (ADR-067 claim 8: it is a persistent product of the COP
  that outlives the transient transaction). Reading `binding.resolved()` there sized the very first
  discard window from a value that was never actually in effect for tester-present's own send. Fixed by
  adding `ResolvedTesterPresent::p2_max_ms` — resolved from the same `active` every other field on that
  struct comes from — and reading `tester_present.p2_max_ms` at the arm site instead of `binding`.
  `handle_update_param`'s own re-arm site was already correct (it reads `params`, the just-promoted
  Active set directly, with no Temp/Working split to confuse); `dispatch_due_idle_tester_present`'s
  per-tick sends were already correct too (each reads live Active fresh immediately before its own
  send, in the same pre-send snapshot round 2's `CP_P2Max` read-timing fix established — not from this
  struct at all, since a per-tick send's own window duration should reflect whatever `CP_P2Max` is
  configured *right now*, unlike `target_can_ids`, which is about a past send's fixed physical
  identity).

The pending-COP-claim-always-wins rule is completely unaffected — it is still the first, unconditional
check, ahead of both the window and the `target_can_ids` gate.

### Consequences of the amendment

- The legacy-OBD preset collision (original Consequences, now struck through) is not eliminated but
  is now bounded to a few-hundred-millisecond window after each of tester-present's own 2-second-
  interval sends, rather than "for as long as tester-present stays armed" — a materially narrower,
  much less likely accepted residual. The `CP_TesterPresentExpNegResp` narrowing to `[0x7F, 0x01]`
  on those six presets is reverted to the original unscoped `[0x7F]`, per the requester's explicit
  instruction.
- **Accepted residual — bare `[0x7F]` is not service-scoped, even inside the window (Codex review
  finding, confirmed by the requester as an accepted trade-off, not a bug to fix).** These same six
  legacy-OBD presets default to `CP_TesterPresentSendType = 1` (idle-triggered), so every successful
  tester-present send opens a `CP_P2Max` discard window; `target_can_ids` restricts that window to the
  same ECU tester-present targeted, but neither the window nor the CAN-ID gate restricts *which
  service's* negative response it is. An unclaimed NRC (`0x7F <any SID> <any code>`) from a genuinely
  different diagnostic request — not tester-present's own Mode 01 PID 00 — arriving from that same ECU
  within that same window is still discarded, since `[0x7F]` alone cannot distinguish it from tester-
  present's own `0x7F 0x01 <code>`. The narrower `[0x7F, 0x01]` (reverted above) would have closed this
  specific gap by re-adding the service byte to the match; kept unscoped per the requester's instruction
  regardless, on the basis that the window+CAN-ID scoping already narrows the exposure considerably (an
  unrelated request to the *same* ECU landing inside the same few-hundred-millisecond window as tester-
  present's own 2-second-interval send) and a client that needs this window closed entirely can still
  narrow `CP_TesterPresentExpNegResp` itself via `SetComParam`.
- **New accepted residual — window sizing.** `CP_P2Max`'s default (50 ms) may under-cover a slow ECU:
  a genuine tester-present reply arriving just after `discard_until` is delivered to the client
  instead of discarded. Fail-safe direction (deliver, not silently drop) and client-tunable via
  `CP_P2Max`.
- **New accepted residual — physical-target restriction is first-entry-only.** `target_can_ids` takes
  the *first* UniqueRespIdTable entry's CAN ID(s) unconditionally when addressing resolves to physical,
  matching `resolve_tester_present`'s own addressing rule (ADR-050); a CLL with multiple physical
  entries where a later entry is somehow the actual tester-present target (not a configuration this
  service's own resolution logic would ever produce) is out of scope. **Mode 0's `target_can_ids` is
  pinned to whichever entry was first at arm time specifically — a later table reorder, or a later
  reassignment of that entry's own uid label, does not follow the "first entry" rule to a *different*
  entry or CAN ID, by design** (see the two-round freezing rationale above, both fixed, not residual);
  this residual is only about which entry is targeted at arm time, not about staleness after arming.
- **CP_P2Max read-timing consistency (fixed during the same edge-case-hunter round that caught the
  mode-0 freezing bug above, before either landed committed):** all three `discard_until` write sites
  (`handle_start_comm`, `handle_update_param`, `dispatch_due_idle_tester_present`) now capture
  `CP_P2Max` from a pre-send snapshot alongside `connect_generation`, not a live re-read of Active at
  post-send write-back time — matching `handle_send_recv`'s own established "never live-re-read Active
  for a response-window duration" discipline (ADR-053/ADR-067). An earlier draft of
  `dispatch_due_idle_tester_present` read it live at write-back, which would let a `CoptUpdateparam`
  racing the send's own hardware `.await` change the window's *duration* to a value not actually in
  effect at the send instant.
- No change to the one-tick disarm window, byte-pattern-ambiguity, or `no_response_required` residuals
  from the original Consequences — all still apply unchanged.

## Second Amendment — 2026-07-16: Discard Eligibility and Exp* Patterns Also Frozen at Send Time

**Codex review, PR #97, seventh round.** The first Amendment above correctly froze `target_can_ids`
and `p2_max_ms` at send time (see rounds 1–4), but left TWO more fields on the live-vs-snapshot side of
the exact same bug class it had just fixed: `discard_until` itself was set unconditionally on every
successful tester-present send, regardless of whether `expects_response`
(`CP_TesterPresentReqRsp == 1` at that send instant) was `true`; `build_cll_rx_entries` then gated
discard eligibility by re-reading `l.active.tester_present_req_rsp() == 1` **live**, every poll tick —
completely decoupled from what was true at the actual send instant that opened the window. The same
drift applied to `CP_TesterPresentExpPosResp`/`CP_TesterPresentExpNegResp`, read live via
`l.active.tester_present_exp_pos_resp()`/`_neg()` at the same call site. Since `expects_response` (and,
as of this amendment, `exp_pos_resp`/`exp_neg_resp`) are deliberately excluded from
`ResolvedTesterPresent::same_wire_behavior` (RX classification, not wire content — the same reasoning
`target_can_ids`/`p2_max_ms` already established), a `CoptUpdateparam` that flips only one of these
three ComParams never re-arms tester-present, so the live re-read could drift arbitrarily from send-time
truth for the whole `CP_P2Max` window: a 0→1 `CP_TesterPresentReqRsp` flip wrongly discarded an
unrelated, genuinely-arriving frame from a different exchange; a 1→0 flip let an actual tester-present
reply (elicited under `ReqRsp = 1`) leak through to the client undiscarded; and a live
`ExpPosResp`/`ExpNegResp` change silently swapped which byte pattern an already-open window matched
against, instead of continuing to match what was actually configured at the send that opened it.

### Corrected decision

**`TesterPresentState::Armed::discard_until` is widened from `Option<tokio::time::Instant>` to
`Option<DiscardWindow>`**, where `DiscardWindow { until: tokio::time::Instant, pos: Vec<u8>, neg: Vec<u8> }`
(`service.rs`) carries the send instant's own `CP_TesterPresentExpPosResp`/`CP_TesterPresentExpNegResp`
snapshot alongside the existing `CP_P2Max` deadline. All three write sites
(`dispatch_due_tester_present`, `handle_start_comm`, `handle_update_param`) now construct
`Some(DiscardWindow { .. })` only when the send both succeeded AND the same send-time
`expects_response` snapshot was `true` — otherwise `None`, exactly reusing the pattern this ADR's first
Amendment already established for `target_can_ids`/`p2_max_ms`: freeze at send time, never recompute
live. `dispatch_due_tester_present` extends its existing pre-send `still_due` critical-section snapshot
(round 2's `CP_P2Max` read-timing fix) to also capture `exp_pos_resp`/`exp_neg_resp` at that same
pre-send instant. `handle_start_comm` and `handle_update_param` source `pos`/`neg` from two new
`ResolvedTesterPresent` fields, `exp_pos_resp`/`exp_neg_resp` (`rpc_primitive.rs`), resolved from the
same call-time Active `ComParamSet` every other field on that struct comes from (mirroring the
`p2_max_ms`/`target_can_ids` precedent exactly) and excluded from `same_wire_behavior` for the same
reason those two, and `expects_response` itself, already are.

**`build_cll_rx_entries` no longer reads Active live for this decision at all.** The outer
`if l.active.tester_present_req_rsp() == 1` condition is deleted; `discard_until` being `Some(..)` (with
`until` still in the future) is now fully sufficient proof that the send which opened the window
expected a response, since the write-site gating above already guarantees that. The window's own frozen
`pos`/`neg` are used for the prefix match, not a live `l.active.tester_present_exp_pos_resp()`/`_neg()`
read.

### Consequences of the second amendment

- The core finding is closed in both directions: a send made under `CP_TesterPresentReqRsp = 0` never
  opens a discard window regardless of a later live flip to `1`; a send made under `ReqRsp = 1` keeps
  its window (and its own `ExpPosResp`/`ExpNegResp` patterns) fully intact regardless of a later live
  flip away from either.
- **Accepted residual, pre-existing, not introduced by this fix:** the single most-recent-window
  semantics mean a subsequent send under a flipped `ReqRsp = 0` (if `CP_TesterPresentTime < CP_P2Max`)
  can overwrite a still-open legitimate window to `None` early — a failed send already did this before
  this amendment (`discard_until` was already single-valued and send-outcome-gated), so this is not a new
  regression.
- No change to any other residual from the original Decision or the first Amendment — all still apply
  unchanged.
