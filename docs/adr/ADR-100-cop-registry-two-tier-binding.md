# ADR-100: Two-Tier ComPrimitive Response-Binding Registry (ISO 22900-2 §9.2.6.3.4)

**Date:** 2026-07-17
**Status:** Accepted (supersedes ADR-053's `NumReceiveCycles = -1` receive-phase decision and
ADR-088's "pending COP claim always wins" precedence rule; amends ADR-059, ADR-086, ADR-087,
ADR-095, ADR-098, ADR-099; amended by ADR-101 — cross-channel registrant writeback merge;
amended by ADR-151 — SOM/TxDone indication delivery gated by CP_StartMsgIndEnable/
CP_TransmitIndEnable; Decision §4's `-1`-only `CP_CyclicRespTimeout` scope and the Out of
scope section's Stage 3 inline-execution boundary for created-receive-only COPs superseded by
ADR-182)
**Affects:** `j2534-0404-service` service, events, rpc_primitive, rpc_link, names/comparam
support (`CP_CyclicRespTimeout`), docs (GLOSSARY, RPC_API_GUIDE, j2534-0404-architecture)

## Context

### The field bug that started the investigation

A deployment reported: a communication with no response (timeout) is followed by a
tester-present exchange that the ECU answers normally; the *next* client communication then
receives the tester-present's reply instead of its own. Root cause was confirmed in the field
after a full investigation (design-advisor, this repo, 2026-07-17):

- The client's next `CoptSendrecv` ("Comm C") carried an **empty-mask/pattern
  `expected_response` descriptor**, which `ExpectedResponse::matches` treats as
  match-anything (`service.rs:484-494`).
- `poll_rx_inner` evaluates `MatchProbe` attribution (`events.rs:1267-1322`) **before** the
  tester-present discard check (`events.rs:1353-1383`) — ADR-088's "a pending COP's own claim
  always wins" rule. The tester-present reply (`7E 00`), read during Comm C's receive phase,
  was therefore bound to Comm C (`cop_handle: Some(CommC)` on the client-visible event,
  confirmed in the failing capture) before the correctly-configured, still-open
  `DiscardWindow` was ever consulted.
- A contributing gap synchronized the two exchanges: `send_tester_present_once` stamps
  `no_response_required: !expects_response` (`events.rs:2463-2466`), so under
  `CP_TesterPresentReqRsp = 1` the ADR-060 gap gate (`events.rs:2197-2210`) classified the
  tester-present send as "naturally paced by its response wait" — but tester-present has **no**
  response wait (`dispatch_due_tester_present` arms a passive window and returns,
  `events.rs:2829-2871`), so Comm C's request was written back-to-back with the tester-present
  request, landing the reply inside Comm C's probe window.

ADR-088's precedence rule was argued only in one direction (never sacrifice a genuine client
transaction to housekeeping discard); the converse — housekeeping traffic being *captured by* a
client transaction — was never examined, though ADR-088's own Consequences documented its
sibling (the pending-RC `7F 3E 78` capture residual).

### The IS-CYCLIC compliance finding

Investigating fix options surfaced a second, deeper deviation: `poll_channel_events` dispatches
one `TxItem` at a time (`events.rs:3618-3643`), and an IS-CYCLIC (`NumReceiveCycles = -1`)
receive phase holds that dispatch **indefinitely** (`no_deadline`, `events.rs:6348`,
`:6731-6736`; per ADR-053: "ends only via `CancelComPrimitive` or a hard channel error"). No
other ComPrimitive — same CLL or sibling CLL on the shared physical channel — can execute until
the cyclic COP is cancelled. The wedge was known in-repo as a *mechanical* fact (ADR-083 line
183; ADR-087 Decision point 1, "wedges the channel's FIFO poll task"; ADR-095 audit row 3;
`tester_present_send_type.rs:2903` uses it as a test fixture) but was never audited against the
spec text below. ADR-053's Alternative 2 rejection addressed only the send-cycle axis
(solved by `CycleContinuation` parking); the receive-phase axis went unexamined.

### ISO 22900-2 §9.2.6.3.4 — the normative model (paraphrased)

Per ISO 22900-2:2009(E) §9.2.6.3.4, response binding on the receive side
works as a three-stage match: an incoming message passes through the
Pass/Block filters, is then correlated to a known ECU via the
UniqueRespIdTable, and finally is checked against ComPrimitives' Expected
Response Structures. Two candidate pools are scanned in a fixed order — the
currently active Send/Receive ComPrimitives are tried first, and only when
none of those match does the scan fall through to the Receive Only list.
Binding stops at the first match; nothing downstream of that first hit is
ever consulted. A message that matches no ComPrimitive at all is discarded
outright — this is what Decision §5 below calls the unbound-discard rule.

The UniqueRespIdentifier table and the currently active SendRecv
ComPrimitive's own ComParams drive this initial receive *handling* — the
transport-level step described above, distinct from the multi-candidate
response *binding* discussed next; when a ComLogicalLink has no active
SendRecv ComPrimitive at the moment a frame arrives, its CLL-level ComParam
buffer stands in instead. From the moment a frame is actually bound to a
ComPrimitive, everything downstream (receive timing included) is governed
by that specific ComPrimitive's own ComParams.

A **SEND AND RECEIVE** ComPrimitive (`NumReceiveCycles != 0`,
`NumSendCycles != 0`) tries to collect `NumReceiveCycles` responses on each
send; failing to reach that count before the receive timeout (`P2Max`, for
instance) raises an error event. A periodic instance is the exception to
watch for: it never transitions to `PDU_COPST_FINISHED` on a receive
timeout — its cyclic sends simply keep going.

**IS-CYCLIC** (Infinite Responses): the moment a ComPrimitive's transmit
completes and its first positive response arrives, it drops into a
receive-only mode. That's the mechanism that frees the ComLogicalLink to
send other ComPrimitives while the cyclic one keeps collecting responses in
the background.

**7F handling**: negative-response (0x7F) processing only applies to
ComPrimitives that actually have an expected response configured —
receive-only ComPrimitives are excluded from it entirely by definition.

A **SEND ONLY** ComPrimitive (`NumReceiveCycles = 0`) is transmitted with no
receive timer set up at all; if the protocol permits back-to-back sends, any
other pending ComPrimitive becomes immediately eligible to run.

A **RECEIVE ONLY** ComPrimitive (`NumSendCycles = 0`) uses its
`NumReceiveCycles` purely as a count of bus messages to watch for against
its Expected Response Structure, finishing (`PDU_COPST_FINISHED`) once that
count is met. Two additional behaviors: setting `NumReceiveCycles` to
infinite (`-1`) lets `CP_CyclicRespTimeout` drive completion by timeout
instead of by count (absent that, the client must cancel it explicitly via
`PDUCancelComPrimitive`), and a receive-only ComPrimitive never carries
`pCopData` payload bytes.

Correction (A3-6): an earlier draft of this ADR treated the receive-handling clause's
phrasing about the currently active Send/Receive ComPrimitive(s) as deliberately plural, contrasted
against the transport-level NOTE's singular wording, to argue the spec text itself signals
multi-candidacy for binding versus single ownership for handling. That grammatical-number
argument does not hold up: the repo's own ISO 22900-2:2009(E) conversion uses the singular
form in both places (Table 9's "Receive Message Handling" row and its NOTE, §9.2.6.3.4) — there
is no plural instance of this phrase in the available 2009 text. This ADR's Tier 1 grouping
does not actually rest on that distinction: a periodic SendRecv COP that does not transition to
`PDU_COPST_FINISHED` on a receive timeout, and a separately executing one-shot COP, are
simultaneous match candidates on one CLL by construction of the state machine (§9.2.6.3 Table
6), independent of how either clause happens to be worded. **Verified against the 2022 edition
(2026-08-07, `iso22900-2-conformance-audit.md` A3-6):** its content-equivalent passage (Table 9's
receive-handling row and NOTE) also uses singular phrasing throughout, with no plural instance
found — the same reading holds in both editions, and this Tier 1 grouping's actual basis (the
state-machine argument above) is unaffected either way. The spec
therefore defines a general attribution pipeline — ordered two-tier scan, first-match-wins,
unbound-discard — that this service had never implemented before this ADR. The reported
tester-present capture, the IS-CYCLIC wedge, and the deliver-everything-unbound behavior are
three symptoms of that one missing mechanism.

## Decision

### 1. Per-CLL registrant set, snapshotted per poll pass

Every outstanding ComPrimitive becomes a **registrant** in per-CLL state (`LogicalLinkState`),
snapshotted into `CllRxEntry` each poll pass by `build_cll_rx_entries` — the exact precedent
`tester_present_discard` already established (`events.rs:706-726`). A registrant carries: its
`cop_handle`, `connect_generation` (ADR-086 staleness gating moves here), its
`expected_response` descriptors, its tier (below), match bookkeeping (count/remaining), and —
tier-1 only — its RC (0x7F/0x21/0x23/0x78) configuration. The single ephemeral
`MatchProbe` is **replaced** by the executing COP's registrant entry:
`wait_for_expected_response` keeps its current inline, blocking loop shape but consults its own
registrant's per-pass outcome from the shared attribution step instead of owning a private
probe. Execution semantics of finite receives are deliberately unchanged (see Out of scope).

### 2. Tier is a dynamic per-registrant attribute, with different matching contracts

- **Tier 1 — active Send/Receive** (§9.2.6.3.4 receive-handling clause — singular ("ComPrimitive")
  in the repo's 2009(E) conversion; this tier's multi-candidate grouping is a structural reading
  of the state machine, not a grammatical-plural argument from the spec text, see the correction
  above): the executing one-shot COP, periodic SendRecv COPs, and an IS-CYCLIC COP before its
  first positive response. Tier-1 registrants carry the full RC state machine (pending-RC
  detection feeding the existing ADR-057 ceilings and RC21/23 re-request logic).
- **Tier 2 — Receive Only list**: populated by (a) COPs created with `NumSendCycles == 0`
  (ADR-059's category, unchanged in creation semantics), and (b) IS-CYCLIC COPs **migrating**
  from tier 1 at the spec's transition point — once the COP has finished its transmit and
  received its first positive response. Tier-2 registrants are plain-match only: per the 7F
  clause, negative-response handling does not apply to receive-only ComPrimitives — no RC
  detection, no ceilings, no re-requests.

IS-CYCLIC is therefore a hybrid-lifecycle COP, not a creation-time class: its receive phase
waits inline for the **first** match exactly as today, then registers into tier 2 and
**returns**, freeing the poll task — this is the compliance fix for the wedge, anchored at the
spec's own transition trigger. The COP remains in `primitives`, reports `PDU_COPST_EXECUTING`
via `GetStatus`, and ends via `CancelComPrimitive`, hard error, generation-staleness (checked
at snapshot build), or `CP_CyclicRespTimeout` (below). Its matched frames are delivered through
the existing `ReceivedFrame.cop_handle` path, unchanged.
**Resolved:** `executing_cop` stays a single slot — it means "the `TxItem` the poll task is
dispatching right now" (`events.rs:3762`, cleared `:3897-3901`), and a detached cyclic is
definitionally not that; widening it to a set would corrupt its two existing consumers.
Instead, `rpc_get_status`'s `CopHandle` branch (`rpc_primitive.rs:1694`) gains one extra check:
Cancelled > (`executing_cop` match **or** a live detached registrant) > `Waiting`. A required
companion fix: `rpc_misc.rs:582`'s `CLEAR_TX_QUEUE` handler cancels every `primitives` entry on
the CLL except `executing_cop` — since a detached cyclic sits in `primitives` but is not a
queued item, that filter must also exclude COPs with a live detached registrant, or
`CLEAR_TX_QUEUE` would wrongly cancel a spec-compliant receive-only COP.

**Correction (Codex review of PR #103, round 9, Finding 1):** the implementation of "(a) COPs
created with `NumSendCycles == 0`" above was narrower than this bullet's own already-decided text
— `wait_for_expected_response`'s registrant construction keyed `tier`/`rc_cfg`/`request_sid` on
`created_receive_only && num_receive_cycles == -1` (`is_receive_only_cyclic`, `events.rs:7322`),
not on `created_receive_only` alone. A created-receive-only COP with a FINITE `NumReceiveCycles`
or `-2` (IS-MULTIPLE) therefore landed as `RegistrantTier::ActiveSendReceive` — wrong per this
bullet's own "(a) COPs created with `NumSendCycles == 0`" rule, no qualifier on
`NumReceiveCycles`. Consequence: such a COP was scanned with tier-1 (non-vacuous) precedence,
letting it steal a frame from an older tier-2 monitor with equal or better claim to it, and its
`rc_cfg` was wrongly `Some` (RC78/21/23 auto-handling), directly contradicting the 7F clause this
same bullet already cites — ISO 22900-2 §9.2.6.3.4 excludes receive-only ComPrimitives from
negative-response auto-handling entirely — for the whole tier, not just its `-1` subtype.

Fixed by keying `tier`/`rc_cfg`/`request_sid` on `wait.created_receive_only` alone.
`cyclic_timeout_ms`/`cyclic_deadline` (Decision §4) are UNCHANGED — deliberately still scoped to
the `-1` subtype only (`is_receive_only_cyclic`), per Decision §4's own already-resolved
"Resolved (scope, per (e))" sub-decision anchoring `CP_CyclicRespTimeout` to that subtype
specifically; this correction does not revisit that scoping.

**Client-visible behavior change, deliberately called out (not silent):** a created-receive-only
finite-`N`/`-2` COP previously received RC78/21/23 auto-handling as an accidental side effect of
its wrong tier-1 classification — an ECU answering such a COP with a pending-RC (`0x78`) would
have had its wait extended. Post-fix, per the 7F clause, it correctly does not. An ECU that relies
on this (spec-noncompliant) behavior today will see a different, now spec-conformant, outcome.

**Safety-invariant interaction (found investigating this correction, not part of Codex's own
finding):** a finite-`N`/`-2` created-receive-only COP, unlike the `-1` subtype, is NOT detached
from execution (Decision §4/Out-of-scope's Stage 3 boundary: "finite receives... keep their
current inline/blocking execution shape") — it stays the actively inline-waited COP in
`wait_for_expected_response_inner`'s own blocking loop, on the SAME poll task, for the whole of
its wait. This correction therefore makes it possible for a registrant to be BOTH tier-2 (per
this fix) AND the COP a hold's own reap-injection sites (ADR-095's amendment) are running from —
a case the ADR-095 amendment's own safety argument had not accounted for, since it was written
when every inline-waited registrant was, by construction, tier-1. See ADR-095's amendment for the
corrected invariant and its fix (`ctx.executing_cop` exclusion in both reap predicates).

**Correction (Codex review of PR #103, round 9, Finding 2):** "waits inline for the first match
exactly as today, then registers into tier 2 and returns" (above) described the CONTRACT
correctly but not its actual IMPLEMENTATION timing. `migrate_registrant_to_receive_only` was
called only in `wait_for_expected_response_inner`'s OUTER loop, after `poll_rx_and_check_match`
returned a `Matched` result for that pass — i.e. after `poll_rx_inner`'s entire frame batch (up to
`MAX_POLL_MESSAGES` frames from one `PassThruReadMsgs` call) had already been fully processed
against the per-pass registrant snapshot. `bind_registrant` itself never touched a registrant's
`tier` field inline, only `matches_got`/`cyclic_deadline`/`pending_rc`. So if the SAME batch
contained both this COP's first positive response AND one or more later frames, every later frame
in that batch was still scanned against the registrant's stale tier-1 snapshot — capable of
stealing a frame from tester-present (step 3, which a tier-2 registrant would lose to) or from an
older tier-2 monitor with equal or better claim, exactly the shape of capture bug this whole ADR
exists to close, just re-opened for one batch's worth of frames at the moment of migration.

Fixed by moving the transition INLINE, into `bind_registrant`'s own match-acceptance step: a new
`CopRegistrant.migrate_on_first_match: bool` flag (set at registration, true only for the true
IS-CYCLIC shape — `!created_receive_only && num_receive_cycles == -1`, false for every other
registrant including the round-9-Finding-1-corrected created-receive-only shapes above) gates a
tier flip applied directly to the snapshot immediately after accepting a match:
`if r.migrate_on_first_match && r.tier == ActiveSendReceive { r.tier = ReceiveOnly; }`, gated on
CURRENT tier rather than "is this the first match" so it is robust to a snapshot taken after an
earlier pass already migrated the live registrant. Every subsequent frame in the SAME batch now
scans the already-migrated snapshot. `merge_registrant_writeback` (ADR-101 Decision §A) gains a
fourth field: `tier` merges as a one-way latch (`if snap.tier == ReceiveOnly { live.tier =
ReceiveOnly; }`, never the reverse) — see that Decision for the full formulation. The outer
`migrate_registrant_to_receive_only` call is KEPT, not deleted, even though every traced path now
makes it a redundant confirmation of a transition the in-batch flip (plus its writeback merge)
already completed before that call is reached — it remains the explicit, self-describing anchor
of the `ReceivePhaseOutcome::DetachedToTier2` contract with this function's caller.

### 3. Attribution order — one explicit precedence table, replacing probe-then-discard

Per delivered frame, per CLL entry, in order; first match **binds**, and the spec does not continue
matching other ComPrimitives once an initial match has been made:

1. **Indication frames bypass binding entirely.** Frames with SOM / TxDone / loopback /
   RxBreak bits are never binding candidates (ADR-098's exclusion, unchanged) and are
   **explicitly carved out of the unbound-discard rule**: §9.2.6.3.4 governs *messages*
   (reassembled responses); these are adapter-level artifacts of J2534 multiplexing that ISO
   22900-2's model never delivers as messages at all. Decision: they keep their existing
   `ResultData` delivery path per ADR-098, and ADR-099's tester-present SOM-herald and TX-echo
   discard arms are retained verbatim on top of it. (ADR-151: for the SOM/TxDone subset
   specifically, this delivery path is additionally gated per CLL by `CP_StartMsgIndEnable`/
   `CP_TransmitIndEnable` — RxBreak/loopback are unaffected and still deliver unconditionally.) (An alternative non-`ResultData` event
   channel was considered and rejected here: it would be a second client-visible interface
   change stacked on this ADR's already-significant one, for no attribution benefit.)
2. **Tier-1 scan, non-vacuous claims only**, in COP start order: a descriptor match where the
   descriptor has a non-empty mask/pattern, or the COP's own pending-RC detection.
3. **Tester-present's own signature check** (module-internal; the spec's own "periodic
   ComPrimitive" framing, invisible to the client per `CP_TesterPresentReqRsp = 1`): ranked
   identically to a registrant scan in this precedence table, but — per S7's own review, see
   "Resolved (c)" below — implemented as a dedicated per-pass check (`TesterPresentDiscard`),
   not a literal `CopRegistrant`. Binds when the frame carries tester-present's full identity
   signature — open `CP_P2Max` window from an actual send, `target_can_ids` gate, and non-empty
   frozen `pos`/`neg` prefix match (`starts_with` semantics per ADR-088, unchanged — these are
   bytefield prefixes, not mask/pattern). A bound frame is **discarded**, per spec. Under
   `CP_TesterPresentReqRsp = 0` no response check fires from content/SOM-herald matching
   (ADR-099's always-open TX-echo window is unaffected, handled in step 1).
4. **Tier-1 vacuous claims** (empty-mask/pattern descriptors), in COP start order.
5. **Tier-2 Receive Only list**, in COP start order (vacuous descriptors permitted — broad
   monitoring is this tier's spec-sanctioned purpose).
6. **Unbound → discarded.** (Behavior change; see Decision 5.)

Steps 2-vs-3-vs-4 are this ADR's resolution of the ADR-088 precedence tension, superseding "a
pending COP's own claim always wins": a *specifically-expected* client response still always
beats tester-present (ADR-088's original worry remains honored), but tester-present's
identity-signature match now beats a *vacuous* client claim — which is exactly the reported
bug. The intra-tier order is COP start order; the vacuous/non-vacuous split and the
tester-present insertion are refinements within latitude the spec leaves open (it defines no
intra-tier order and does not model module-internal tester-present as a client COP).
**Resolved (a):** intra-tier tie-break is ascending `registration_seq` — a per-CLL monotonic
`u64` assigned under the `logical_links` lock at registrant insertion. This makes "COP start
order" unambiguous: same-instant starts cannot occur under the lock, and client-influenced
handle-value ordering is never consulted.

**Resolved (b):** yes, `detect_pending_rc` gains a request-SID gate. Verified
(`service.rs:187-199`): today it checks only `data[rc_byte_offset] ∈ {0x78, 0x21, 0x23}` — no
`0x7F` first-byte check and no SID check at all, which is worse than assumed: a *positive*
response with a coincidental `0x78`-valued byte at the offset (e.g. `62 F1 90 78 …` under
UDS's offset 2) is already misdetected as pending today, latent and pre-existing. Fix:
`RcConfig` gains `request_sid: Option<u8>` (the first post-header byte of the COP's own
original request, captured where `RcConfig` is resolved for the COP); when
`rc_byte_offset >= 2`, detection additionally requires `data[0] == 0x7F && data[1] ==
request_sid`; when `rc_byte_offset < 2` (non-UDS/KWP framings the offset ComParam exists to
serve), behavior is unchanged. This closes the ADR-088 pending-RC capture residual for the
standard `offset == 2` shape (a `7F 3E 78` is no longer claimable by a non-`0x3E` COP) and
fixes the latent positive-response misdetection as a side effect; the `offset < 2` framings
remain an accepted, narrower residual.

**Addendum (Codex review, PR #103 round 3):** the request-SID gate above was not sufficient on
its own. `detect_pending_rc`'s claim in `bind_registrant`'s `Tier1NonVacuous` arm is additionally
scoped by `unique_resp_ids` (ECU addressing), the same way the positive-match scan a few lines
below it already filters `ExpectedResponse::unique_resp_ids` against the frame's
`unique_resp_identifier`. Without this, on a CLL with a `UniqueRespIdTable` configured (multi-ECU
functional addressing), a registrant scoped to one ECU (e.g. `unique_resp_ids = [2]`) could still
have its wait extended by a `7F <sid> 78/21/23` pending-RC frame from a *different* ECU that
happened to echo the same request SID (plausible under functional/broadcast addressing, where
multiple ECUs reply to the same broadcast SID) — the request-SID gate alone cannot distinguish
that case. The fix gates the `detect_pending_rc` call on `r.expected.iter().any(|e|
e.unique_resp_ids.is_empty() || e.unique_resp_ids.contains(&unique_resp_identifier))`: `.any(...)`
across all of the registrant's descriptors, since a pending RC is not tied to one specific
descriptor the way a positive match is — it just means "this COP should keep waiting because one
of the ECUs it's listening for hasn't finished yet." Two distinct cases both fall through to
"unrestricted" and are both intentional, not just one blanket "empty = any ECU" rule: a registrant
with **zero** `expected` descriptors (a client can submit `CoptSendrecv`/`CoptStopcomm` with an
empty `expected_response_array` — ADR-058 already treats this as a legitimate "naturally times
out" case) has expressed no ECU preference at all, so any ECU's pending RC legitimately extends
it; separately, a registrant with **one or more** descriptors where *any single one* has an empty
`unique_resp_ids` (that descriptor's own "any ECU" per its own doc comment) also passes, even if a
sibling descriptor on the same registrant is scoped to a specific ECU — consistent with the
existing OR-across-descriptors convention the positive-match scan already uses. No-table / single-ECU
CLLs are unaffected: `unique_resp_identifier` is always `0` there, and `unique_resp_ids.is_empty()`
already means "any ECU," so the gate is a no-op on that (common) path.

The `DiscardWindow`'s content arm (`service.rs:1350`, its struct definition --
moved there since this line was originally drafted) is replaced by step 3. Its
send-time-frozen state (`pos`/`neg`/`target_can_ids`/`until` — ADR-088 second amendment's
freezing discipline) carries over unchanged as `TesterPresentDiscard`'s own signature fields
(`CllRxEntry::tester_present_discard`, built once per poll pass by `build_cll_rx_entries`) —
**not** as a `CopRegistrant`'s fields; see "Resolved (c)" below for why. `DiscardWindow` itself
is not deletable by this ADR: it is where that frozen state actually lives
(`TesterPresentState::Armed::discard_until`), and `TesterPresentDiscard` is derived from it
each pass, not a replacement for it.

**Resolved (c) [S7]:** tester-present is *not* converted into a literal `CopRegistrant`. S3
already placed its check at the correct, permanent rank (step 3 above) as a dedicated block in
`bind_frame`, reading `entry.tester_present_discard: Option<TesterPresentDiscard>`; S7's job was
to decide whether to fold that block into the registrant vector or finalize it in place, and
concluded the latter. Reasons: tester-present is module-internal and has no `cop_handle` a
client could ever observe via `GetStatus`/`CancelComPrimitive`, so `CopRegistrant::cop_handle`
would need a synthetic sentinel value with no real referent; its match action is **discard**,
not "deliver `ResultData` to a `cop_handle`", so even a successful field-for-field mapping would
still need a special case in the caller to distinguish "matched, discard" from "matched, deliver"
— no dispatch logic is actually shared; and its matching shape (a single frozen
`pos`/`neg`/`target_can_ids`/`tx_can_id` prefix/SOM-herald/TX-echo signature against one open
window) has no counterpart in `CopRegistrant::expected: Vec<ExpectedResponse>` mask/pattern
matching, no `rc_cfg`, and no `matches_needed`/`matches_got` counting to reuse — `tier`,
`registration_seq`, and `connect_generation` staleness-gating are the only fields that would
carry real meaning for it. Forcing tester-present into `CopRegistrant`'s shape would therefore
add `Option`/unused fields to every real registrant's neighbor without eliminating the dedicated
matching logic `bind_frame` already runs for it — a relabeling, not a simplification. Decision
§3 step 3's original wording ("Tester-present registrant") is corrected above to describe the
shipped shape accurately: ranked identically to a registrant scan, implemented as its own
dedicated check.

**Addendum (Codex review, PR #103 round 5):** steps 2 and 4 above were implemented with
registrant-level granularity — a single precomputed `CopRegistrant::vacuous` flag, `true` only when
EVERY descriptor in `expected` was empty — instead of the per-descriptor granularity this section's
own prose already specifies ("a descriptor match where *the descriptor* has a non-empty
mask/pattern" for step 2; "Tier-1 vacuous claims (empty-mask/pattern *descriptors*)" for step 4).
For a registrant with a MIX of specific and empty descriptors, `vacuous == false`, so step 2's own
gate did not skip it, but the shared match-acceptance scan it fell through to had no per-descriptor
filter at all — its empty descriptor still vacuously matched any payload (`ExpectedResponse::matches`'s
own "matches anything" rule for an empty mask/pattern), letting a tester-present reply or any
unrelated frame bind at step 2 via that empty descriptor before step 3's tester-present check ever
ran. This is the ORIGINAL field bug's exact capture mechanism, reproduced for any registrant that
also happens to carry one non-empty descriptor — a shape the original bug report's registrant did
not have, and the registrant-level `vacuous` flag alone could not distinguish. Fixing this in
isolation would have created a symmetric under-match at step 4: with a per-descriptor filter added
only to step 2, a mixed registrant's empty descriptor would then match nowhere in tier 1 at all —
blocked at step 2 by the new filter, and already blocked at step 4 by its OWN registrant-level gate
(`if scan == Tier1Vacuous && !r.vacuous { continue }`, which skips any mixed registrant entirely).

Fix: `ExpectedResponse` gains a per-descriptor `is_vacuous()` predicate, and the shared
match-acceptance scan filters candidates by it, scoped per `AttributionScan` variant —
`Tier1NonVacuous` considers only non-vacuous descriptors, `Tier1Vacuous` considers only vacuous
ones, `Tier2` is unfiltered (unchanged: "vacuous descriptors permitted" remains this tier's
spec-sanctioned purpose, Decision §3 step 5). Both registrant-level gates are deleted as part of
this fix — the step-2 gate becomes purely redundant once per-descriptor filtering exists (an
all-vacuous registrant's filtered scan already returns no candidate), and the step-4 gate is
actively wrong under per-descriptor semantics and must go rather than be patched, since patching it
(e.g. "skip only when zero descriptors are vacuous") would still scan every descriptor for no
behavioral gain over removing it outright. With both gates gone, `CopRegistrant::vacuous` has no
remaining reader and is deleted along with its production computation site
(`wait_for_expected_response`) and every test-harness initializer that set it.

`is_vacuous()`'s definition is **not** "empty mask AND empty pattern," despite that reading being
what this ADR's own step-2/4 prose ("empty-mask/pattern descriptors") and the pre-fix registrant-level
flag's formula both suggested. `ExpectedResponse::matches`'s actual match-anything condition is
`cmp_len == 0`, where `cmp_len = mask.len().min(pattern.len()).min(data.len())` — this is zero
whenever EITHER array is empty, not only when both are. A descriptor with `mask = [0xFF]` and an
empty `pattern` (or the reverse) is therefore effectively vacuous — matches every payload — despite
being classified non-vacuous under the AND reading, reproducing the identical capture bug a third
way, for a shape a client can construct today (the proto layer forwards both byte arrays verbatim
with no length-parity requirement between them). `is_vacuous()` is defined as
`mask.is_empty() || pattern.is_empty()`, matching `matches`'s real semantics rather than the
narrower AND phrasing this section used before this addendum.

**Flagged, not fixed (accepted, narrower, orthogonal):** a zero-length content payload makes
`cmp_len == 0` against ANY descriptor regardless of that descriptor's own mask/pattern length,
so it vacuously matches even a genuinely non-vacuous descriptor at step 2. SOM (start-of-message)
frames are already carved out of this matching path entirely (ADR-097, `poll_rx_inner` never calls
`matches` for one), so this residual is scoped to whatever other path, if any, can still deliver a
zero-length content payload — not identified as reachable during this addendum's review, recorded
as a backlog item rather than chased further here.

### 4. `CP_CyclicRespTimeout` for tier-2 `-1` registrants

Per RECEIVE ONLY NOTE 1: a created-receive-only registrant with `NumReceiveCycles = -1` and a
non-zero `CP_CyclicRespTimeout` transitions to `PDU_COPST_FINISHED` when no match arrives
within the timeout (deadline restarted per accepted match), removing ADR-053's "ends only via
cancel or hard error" absolutism. **Resolved:** `CP_CyclicRespTimeout` is already registered (ID `0x8010`, `unum32`,
`service_params.rs:53-54`; name-resolvable both directions, `names.rs:41`, `:740`, `:1436`;
declared protocol-supported, `comparam_support.rs:191/306/356/397/439`; default `0` in ~12
protocol presets, e.g. `comparam_defaults.rs:417`) but has **zero runtime consumers** today —
no `ComParamSet` accessor, never read in `events.rs`/`service.rs`. Stage 1 therefore adds only
an accessor (`cyclic_resp_timeout_ms()`) plus the tier-2 deadline logic, not registration
plumbing. Unit caution: `service_params.rs:53`'s doc comment says "in ms," but sibling timing
params (e.g. `CP_P2Max`, per ADR-053) are stored µs with a `get_us_as_ms` conversion
(`service.rs:179`); the accessor is implemented on that same µs-storage convention for
consistency, `0` = disabled, pending confirmation against the spec's 11.2.x resolution row —
either way this is a one-line flip inside the accessor, isolated from every other Stage-1 item.

**Resolved (scope, per (e)):** NOTE 1 is textually anchored inside the RECEIVE ONLY
(`NumSendCycles = 0`) subsection; nothing in the relayed SEND AND RECEIVE / IS-CYCLIC text
extends or forbids it for a migrated COP. Stage 1 applies `CP_CyclicRespTimeout` to
**created-receive-only `-1` registrants only** (the strict textual anchor); extending it to
migrated (tier-1→tier-2) IS-CYCLIC COPs is a small, separately-gated follow-up once the
ComParam's own 11.2.x definition text is confirmed to (or not to) cover that case.

**Superseded in part by ADR-182:** this "`-1` registrants only" scope was itself grounded in
ISO 22900-2:2009(E)'s text at the time; the 2022 edition's RECEIVE ONLY row and
`CP_CyclicRespTimeout`'s own Annex B definition extend this ComParam's governing role to
finite `NumReceiveCycles > 0` created-receive-only registrants too (not `-2`), which
[ADR-182](ADR-182-cp-cyclic-resp-timeout-finite-receive-only-scope.md) implements. The
migrated-IS-CYCLIC follow-up noted above remains open and out of scope for ADR-182 (a
different, still-undecided extension) — but the 2022 SEND AND RECEIVE text states more
plainly than the 2009 text did that a migrated tier-2 registrant's completion semantics are
not automatically the same as a created-receive-only one's, which is a slightly stronger
textual anchor for eventually revisiting this follow-up than was available in 2009; recorded
here, not acted on, per ADR-182's own explicit non-scope.

### 5. Unbound frames are discarded — explicit behavior change, not silent

Today every frame routed to a CLL that binds to nothing is buffered and fanned out as
unsolicited `ResultData` (`events.rs:1385-1430`, `rpc_primitive.rs:1783`). Per §9.2.6.3.4's
unbound-discard rule summarized above, this ADR flips that: unbound frames are dropped (step 6). This is the
change that makes the reported bug's FIFO-shift variant structurally impossible. **Migration
path for clients relying on unsolicited delivery** (bus monitoring): register a
`NumSendCycles = 0` receive-only ComPrimitive (ADR-059) with a broad
`ExpectedResponseStructure` — the spec's own mechanism for exactly this use case, and already
implemented. This flip must be called out in release notes / RPC_API_GUIDE, and the
`GetEventItem`/`SubscribeEvent` documentation updated accordingly.

## Out of scope (deferred, evidence-driven — explicitly open, not dropped)

- **Stage 2 — de-blocking bounded holds within the current model:** parking `CoptDelay` and
  the RC21/23 `request_time_ms` sleeps on the existing `CycleContinuation`-style scheduling,
  shrinking `dispatch_due_tester_present`'s seven hooks (`events.rs:1918`, `:1966`, `:3452`,
  `:3544`, `:4426`, `:6509`, `:6729`); and correcting `send_tester_present_once`'s
  `no_response_required` stamp to `true` (ADR-093's derivation conflated "the ECU replies"
  with "a wait paces the bus") — deferred because it changes wire timing, which must not ride
  an attribution ADR.
- **Stage 3 — detaching finite/IS-MULTIPLE receive *execution*** for cross-CLL responsiveness.
  Key scope boundary: finite receives **join the registry in this ADR** (the spec's binding
  algorithm requires their participation — a periodic COP and an executing one-shot must be
  co-candidates), but they **keep their current inline/blocking execution shape** — the spec
  mandates multi-candidate *binding*, not detached *execution*, for finite COPs (the NOTE's
  singular transport owner; per-CLL COP queue model). Revisit only if cross-CLL finite-wait
  latency proves to matter in real deployments. **Narrowly superseded by
  [ADR-182](ADR-182-cp-cyclic-resp-timeout-finite-receive-only-scope.md) for one specific
  subfamily**: a *created-receive-only* (`NumSendCycles == 0`) finite-`N > 0` COP now detaches
  at creation too, exactly like the `-1` subtype already did — ADR-182 found this boundary
  itself indefensible for that specific shape once `CP_CyclicRespTimeout` (not `CP_P2Max`)
  governs it, per the same "would wedge the poll task on a quiet, default-timeout bus" defect
  class this ADR's own Context already condemned for the `-1` case. Every other finite/`-2`
  receive execution shape (SEND-AND-RECEIVE finite COPs; `-2` IS-MULTIPLE created-receive-only)
  is unaffected and keeps this Stage 3 boundary in force.
- TX-side holds (`isotp_send` FC-wait/STmin pacing) remain blocking by physical necessity;
  their tester-present hooks remain.

## Alternatives Considered

1. **Bespoke tester-present-only fix (phantom COP + detached completion wait) without a
   registry** — the pre-§9.2.6.3.4 plan. Rejected: now dominated — the registry is a spec
   compliance obligation regardless, so the bespoke mechanism would build the narrow thing
   twice, and it fixed the capture bug by execution serialization when attribution-layer
   binding (step 3) fixes it without touching wire scheduling.
2. **Full uniform detachment of all receive execution now** — appealing uniformity ("nothing
   ever blocks the poll task"), rejected for this ADR: 3-5× scope; converts ~420 lines of
   battle-hardened sequential receive logic (`events.rs:6317-6740`) plus RC handling and
   ADR-087's StopComm engine into an event-driven state machine, resetting six ADRs' worth of
   review-proven race fixes (ADR-053/057/058/086/087/095); the spec does not mandate execution
   detachment for finite COPs; and TX-side holds survive regardless, so "one hook" is
   unreachable anyway. Remains available as Stage 3 evolution on top of this ADR's registry —
   nothing here is thrown away by it.
3. **Separate per-CLL scheduler task with real preemption** — rejected; ADR-053's Alternative 3
   reasoning still holds in full (single poll task is the serialization point for all J2534 API
   calls; a second task would park on the device-global `api` mutex for the same durations
   while reopening the concurrency classes that design exists to prevent).

## Consequences

- **Closes the reported field bug at its root, in both variants:** the vacuous-match capture
  (step 3 outranks step 4) and the FIFO leak (step 6 discards unbound frames). IS-CYCLIC
  becomes spec-compliant (receive-only mode frees the CLL/channel for other COPs).
- **Supersessions/amendments** (Status-line edits listed for the INDEX below): ADR-053's `-1`
  decision text and ADR-088's precedence bullet are superseded; ADR-059's receive-only COPs
  gain their tier-2 role; ADR-086's IS-CYCLIC-specific loop guards partially migrate to
  registrant/snapshot staleness; ADR-087's "wedges the poll task" rationale for rejecting
  IS-CYCLIC on StopComm must be restated on its remaining true grounds (a StopComm must
  terminate; the wedge argument no longer holds); ADR-095's audit row 3 changes (the unbounded
  IS-CYCLIC hold no longer exists — bounded first-response wait only); ADR-098/099's
  indication-frame delivery is reaffirmed via this ADR's explicit bind-or-discard carve-out.
- **Client-visible changes:** unbound frames are no longer delivered (migration: receive-only
  COPs); a vacuous-descriptor COP no longer receives a tester-present reply it technically
  "matched"; IS-CYCLIC no longer blocks subsequent COPs.
- **Accepted residuals, carried or narrowed:** the byte-identical client `0x3E` COP ambiguity
  (ADR-088) *narrows* — a non-vacuous descriptor now always wins over tester-present; a
  genuinely-unbound `7F …` frame inside a tester-present window with bare `[0x7F]` `neg` is
  bound to tester-present and discarded (previously delivered unsolicited — now also
  spec-consistent via step 6 regardless); pending-RC capture closes for the standard
  `rc_byte_offset == 2` (UDS/KWP) shape via the `detect_pending_rc` request-SID gate (Decision
  §3, resolved (b)) and remains an accepted residual only for `rc_byte_offset < 2` framings.
  The same gate also fixes a latent, pre-existing defect found during verification: a positive
  response whose byte at `rc_byte_offset` coincidentally equals `0x78`/`0x21`/`0x23` was
  misdetected as a pending negative response with no `0x7F` first-byte check at all.
- **Accepted residual (Codex review of PR #103, design-advisor verdict): registrant snapshot vs.
  frame-drain ordering on a UUDT companion channel.** `build_cll_rx_entries`'s per-CLL registrant
  snapshot (Decision §1) is taken AFTER `poll_rx_inner` has already drained frames via
  `PassThruReadMsgs`, under a separate lock acquisition. On a CLL with a UUDT companion channel
  (ADR-046, two independent poll tasks — the same architecture ADR-101 addresses for writeback
  merging), a frame the companion channel drained before a brand-new `CoptSendrecv`'s registrant
  existed can, in principle, still be bound to that registrant if the registrant's insertion
  (primary channel) races into the gap between the companion's own frame-read and its own
  registrant-snapshot lock acquisition. Investigated for a fix and rejected: the send always
  precedes registrant insertion (`transmit → wait_for_expected_response → insert_cop_registrant`),
  so a genuinely fast ECU response can legitimately already be on the wire, and drained, before the
  registrant exists — the exact same interleaving that lets that genuine response bind instead of
  being permanently discarded at step 6. There is no oracle (no host-correlated frame-arrival
  timestamp from the J2534 API) to distinguish the two cases; gating on registrant-existence-time
  would convert the rare false-bind into a strictly worse, common-case false-discard (timeout) for
  ordinary fast responses. Reordering the snapshot before the read is a symmetric inversion of the
  same problem, not a fix. Bounded blast radius: the window is one `logical_links` lock acquisition
  (µs-scale); the frame must first survive precedence steps 1–3 (a tester-present reply — the
  original field bug's frame class — is already discarded at step 3 before any registrant scan is
  reached); and it must then match the new COP's descriptors, so a non-vacuous descriptor (already
  the general mitigation this ADR's Decision §3 establishes) is the practical safeguard. Not a new
  exposure class: on a single-channel CLL, an adapter-buffered frame that arrived before a COP's
  request bound to the pre-ADR-100 `MatchProbe` identically (the probe is created post-transmit,
  the buffer is drained on the first wait pass), and ISO 22900-2 §9.2.6.3.4's own model binds at
  receive-handling time with no frame-age concept at all — the companion-channel case only widens
  an already spec-inherent exposure by one lock acquisition's worth of timing.
- **Follow-up work:** regression tests — the reported scenario end-to-end (vacuous Comm C +
  tester-present reply → bound to TP, discarded; Comm C receives its own response); IS-CYCLIC
  detachment (second COP executes while cyclic receives; matches still attributed);
  tier-migration at first positive response; RC handling active pre-migration and inert
  post-migration; unbound-discard + receive-only-COP monitoring path; `CP_CyclicRespTimeout`
  finish. Docs — GLOSSARY "IS-CYCLIC" entry (lines 93-94), `rpc-api-guide.md` (line 232 area
  and the `num_receive_cycles` row at line 432), `j2534-0404-architecture.md` (lines 617-621,
  815), same commit as the code change per CLAUDE.md. Stage 2/3 items recorded in
  `j2534-0404-service/docs/implementation-notes.md`'s backlog so they survive per the merge
  sweep rule.
