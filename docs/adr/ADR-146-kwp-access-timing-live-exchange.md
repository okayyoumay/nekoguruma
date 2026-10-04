# ADR-146: KWP Access Timing Parameter (SID 0x83/0xC3) Live Exchange for CP_ModifyTiming

**Date:** 2026-07-29
**Status:** Accepted
**Affects:** `j2534-0404-service` service, service/events, service/comparam_support, service/comparam_defaults, service/rpc_link, service/service_params, service/names

## Context

`CP_ModifyTiming`, `CP_AccessTiming_Ecu`, and `CP_AccessTimingOverride` were
defined as inert `SetComParam`/`GetComParam`-storable values with no
behavior: nothing in this service ever inspected bus traffic for the ISO
14230-2 Access Timing Parameter service (request SID 0x83, positive response
SID 0xC3, sub-function/TPI byte 0-3), and nothing derived from an observed
exchange ever reached the channel's actual P2/P3/P4 timing. Two related
bugs were also present: `CP_AccessTiming_Ecu`/`CP_AccessTimingOverride`
were allow-listed only for the CAN/ISO15765 protocol family in
`comparam_support.rs`, though ISO 22900-2's own per-protocol applicability
table scopes them to ISO 14230 only (this service's KWP-family grouping
also includes ISO 9141, which has no equivalent service); and both
`CP_AccessTiming_Ecu`/`CP_AccessTimingOverride` and the pre-existing
`CP_SessionTiming_Ecu` were stored as plain `Unum32` values although ISO
22900-2 types them as `PDU_PT_STRUCTFIELD` (the same
`PDU_CPST_ACCESS_TIMING` structure `CP_ExtendedTiming` already uses
correctly in this codebase, ISO 22900-2 §B.3.3.2.3).

ISO 22900-2's `CP_ModifyTiming` description (ISO 22900-2:2022, the edition
this subsystem's ADRs already cite) states that when enabled, a positive ECU
response to the ISO 14230-2 service 0x83/0xC3 (KWP) or, on CAN/UDS, ISO
15765-3/14229-3 service 0x10/0x50, auto-adjusts `CP_P2Max`/`CP_P2Min`/
`CP_P2Star`/`CP_P3Min`/`CP_P4Min`, using the worst-case value across
responding ECUs for a functionally-addressed exchange. SAE J2534-1 v04.04
has no IOCTL corresponding to this exchange (Figure 27's full IOCTL list),
so any implementation must observe the exchange as it rides the normal
CoptSendrecv message-send/receive path — this service never originates a
0x83 request itself, matching this feature's scope to the CAN/UDS
0x10/0x50 side being explicitly out of scope for this change (the user's
request only covers the KWP 0x83/0xC3 exchange).

ISO 14230-2 primary text is not available anywhere in this workspace (only
ISO 22900-2, J2534-1, and J2534-2 are, per this repo's spec inventory) —
only the ISO 22900-2 `PDU_PARAM_STRUCT_ACCESS_TIMING` struct documentation
(§B.3.3.2.3) is available as an in-repo, verifiable source for the wire
struct's per-field resolutions.

## Decision

**Detection and pairing.** A per-COP `TimingChangeConfig` is captured at
the RPC layer from the payload-only request bytes (`cop_data`, the same
payload-only source `RcHandlingConfig::request_sid` already reads, ADR-050),
threaded through `ExpectedResponseWait`/`CopRegistrant` the same way
`rc_cfg`/`request_sid` already are. It is populated only when the channel's
protocol is ISO 14230 (not the wider KWP-family grouping that also includes
ISO 9141 — ISO 22900-2's own default tables for both new ComParams list
ISO14230_2/ISO14230_4 only), the COP's `ComParamSet` snapshot has
`CP_ModifyTiming` enabled, the registrant is tier-1 (mirroring `rc_cfg`'s
existing `created_receive_only` gate — ISO 22900-2's own text disables
negative-response handling, and by the same reasoning this feature, for
receive-only COPs), and the request's first payload byte is `0x83`. For a
TPI=3 (set given values) request, the 5 timing bytes accompanying the
request are captured alongside the TPI value, since ISO 14230-2's positive
response to TPI=3 carries no payload of its own — the values being adopted
only exist in the original request.

Response pairing happens inside `bind_registrant`'s existing positive-match
branch (after a registrant's `expected` descriptors already accept the
frame, so unique-response-ID scoping, connect-generation gating, and tier
gating come for free), checking the paired registrant's captured TPI/SID
context against the header-stripped response payload's first byte
(`0xC3` — the fixed SID+0x40 relationship for a KWP positive response,
requiring no byte-search) and second byte (echoed TPI). This coexists with
existing pending-RC detection unchanged: a `7F 83 78` interim response is
still claimed by the existing 0x21/0x23/0x78 pending-RC path, and a
subsequent genuine `0xC3` still binds normally afterward.

**Per-TPI behavior.**

- TPI=0 (read limits of possible values) is out of scope: `CP_ModifyTiming`
  only names TPI 1/2/3, and TPI=0's payload reports *limits*, not values
  that should ever become the live active timing.
- TPI=1 (set to default values): the response carries no timing bytes.
  If `CP_AccessTiming_Ecu` already has a `TimingSet=1` entry (ISO 22900-2's
  struct documents `TimingSet=1` as exactly "the ECU's own default set,
  also what TPI 1/2 report"), that entry's values are (re-)applied; if none
  exists, this is a documented no-op — this service cannot originate ISO
  14230-2's own default timing constants without a verified primary-text
  source, and does not invent them.
- TPI=2 (read active values): the response's 5 timing bytes are always
  recorded into `CP_AccessTiming_Ecu`'s `TimingSet=1` entry as the ECU's
  actual observed values. Separately, if `CP_AccessTimingOverride` is
  non-empty, the derived engineering ComParams (`CP_P2Min`/`CP_P2Max`/
  `CP_P2Star`/`CP_P3Min`/`CP_P4Min`) are computed from the Override's
  `TimingSet=2` entry instead of the observed bytes — ISO 22900-2's
  override clause names TPI=2 specifically and redirects which data the
  *derived* ComParams are computed from, without implying the observed
  record itself should be overwritten (the struct's own `TimingSet=1` vs.
  `TimingSet=2` distinction documents both coexisting).
- TPI=3 (set given values): the response carries no payload; the request's
  captured 5 bytes are applied as the new active timing and recorded as a
  `TimingSet=3` entry. `CP_AccessTimingOverride`'s clause only names TPI=2,
  so it does not apply here even when non-empty.
- Wire-byte-to-ComParam mapping, reinterpreted to the D-PDU API's 1 µs
  resolution: `P2Min`/`P3Min`/`P4Min` at 0.5 ms resolution map to
  `CP_P2Min`/`CP_P3Min`/`CP_P4Min`; `P3Max` maps to `CP_P2Star` at 250 ms
  resolution (ISO 22900-2's struct documentation ties this specific field
  to the tester-side `CP_P2Star`, not a `CP_P3Max` ComParam — this makes
  the struct's 5 fields a field-for-field match against `CP_ModifyTiming`'s
  own modified-ComParam list, with no field left over). `P2Max`'s
  resolution is documented in ISO 22900-2 as table-driven per an external
  ISO 14230-2 table this workspace cannot verify; the ×500 µs (0.5 ms)
  interpretation is applied provisionally and isolated in a single
  conversion function pending verification against that table (see
  Accepted Residuals).

**Functional-addressing worst case.** For a functionally-addressed COP with
multiple responding ECUs, each field combines toward whichever direction
constrains the tester more: `P2Min` takes the minimum across responses (the
tester must be ready to accept the earliest legal responder), while
`P2Max`/`P3Min`/`P2Star`(from `P3Max`)/`P4Min` take the maximum (the tester
must accommodate the slowest/most-demanding responder). This applies only
to TPI=2 (TPI=3's adopted values are request-side and identical across
responders; TPI=1 is a lookup, not a per-response combination). The
combination is accumulated per-COP and re-applied incrementally after each
qualifying response rather than deferred to COP completion — the
combination is monotone per field, so each intermediate application is
already a correct worst-case-so-far, and this avoids inventing a
completion hook that cyclic (`-1`) registrants never reach.

**Gated on `CP_RequestAddrMode` (amended -- Codex round-3 finding, PR #17).**
The worst-case combination above is meaningful only when more than one ECU
can legitimately answer the same request, i.e. a functionally addressed
(`CP_RequestAddrMode == 2`) COP. The first cut folded every TPI=2 response
into the running accumulator unconditionally, including for a physically
addressed COP, where exactly one ECU ever responds — a second TPI=2
response there (e.g. re-reading active timing after an earlier TPI=3
changed it) is that SAME ECU's new current values, not a second responder
to combine against the first. Folding them anyway produced a synthetic
min/max mixture of the same ECU's own before/after values instead of its
latest-reported ones. `TimingChangeConfig` now captures `functional`
(`tx_header::use_functional_addressing(AddrModeSource::Request, ...)`,
the same ADR-054/ADR-138 addressing resolution `CoptSendrecv`'s own TX path
already uses) at COP-creation time, alongside the other call-time-bound
(ADR-067) fields; `observe_timing_response`'s TPI=2 arm only folds into
`timing_accumulator` when `functional` is true, and otherwise uses each
response's own bytes as-is.

**Hardware application and locking.** `bind_registrant` only mutates
registrant-local state (no lock is taken there). Amended after a Codex
review + edge-case-hunter round (PR #17) found the first cut's storage
eager and unconditional: `link.active`/`link.working` were written before
the hardware push even ran, with no rollback on failure, and per-CLL
storage used plain overwrite rather than combining across that CLL's own
registrants. The mechanism now follows this codebase's existing
`CoptUpdateparam` push-then-promote precedent instead
(`handle_update_param`'s `all_ok`-gated Working -> Active promotion,
ADR-086): under the SAME `logical_links` acquisition the attribution pass
already holds, every CLL folds its own qualifying registrants' observations
into one combined record (derived ComParams via
`combine_derived_timing_value`'s worst-case table; same-`TimingSet`
`CP_AccessTiming_Ecu` entries via `combine_worst_case_timing`'s identical
table) without touching `link.active`/`link.working` yet, and also folds
into the cross-CLL hardware-push delta. Also under this SAME fold-time
acquisition, each CLL captures a `WorkingTimingSnapshot`: the SPECIFIC
`link.working` values (per-key for the derived quintuple, and the WHOLE
`CP_AccessTiming_Ecu` structfield) this fold is about to conditionally
overwrite once the push resolves -- see the concurrent-write paragraph
below for why. The lock releases, the single combined push runs
(`apply_params_to_hardware`, introducing no new lock-ordering edge against
ADR-110), and its `bool` result is kept, not discarded. `logical_links` is
then re-acquired: each CLL is re-checked against `handle_update_param`'s
own post-await recheck predicate (`channel_id == Some(ctx.channel_id) &&
connect_generation == <captured at fold time>`, since the push is a real
hardware await a disconnect+reconnect can complete during), and only then
stored. `link.active` is written unconditionally for `CP_AccessTiming_Ecu`
entries (a wire observation, never itself hardware-pushed, and TPI=1's
reapply path depends on it regardless of this pass's own push outcome) and
gated on the push's success for the derived quintuple (including
hardware-unmapped `CP_P2Star`), so "Active" keeps meaning "actually on
hardware" for every ComParam that claim covers -- this part is unchanged
from the original push-then-promote design. `link.working` is written the
same way, but ONLY when it is still unchanged, per key/per whole
structfield, since the fold-time `WorkingTimingSnapshot` was captured
(Codex round-7 finding, PR #17): if `link.working`'s current value for a
derived key, or its whole `CP_AccessTiming_Ecu` structfield, no longer
matches the snapshot, a concurrent `SetComParam` (or the ADR-067
Temp-writeback in `rpc_primitive.rs`) must have written it during the
hardware-push await window, and that write wins instead -- first-wins,
matching this codebase's own established idiom for this exact race shape.
`link.active` needs no equivalent guard: every OTHER writer of Active
(`handle_update_param`, `handle_restore_param`, the 5-baud-init readback)
runs on the SAME single per-channel poll task as this fold, so they are
strictly sequential with it, and a fresh reconnect always stamps a new
`connect_generation` the post-push recheck above already excludes. Two RPC
handlers with no coordination against this mechanism write Working
concurrently with the poll task -- `rpc_set_com_param` and the ADR-067
Temp-writeback in `rpc_primitive.rs` -- and together they are the reason a
guard is needed here at all: `handle_update_param`'s own established
push-then-promote precedent never needed one, because it only ever writes
`link.active` (in that flow, `link.working` is the RPC's own call-time
snapshot, never itself mutated by the promotion step).
`store_combined_timing_change` deliberately ALSO writes Working -- so
`GetComParam` (which reads ONLY `link.working`, per `rpc_get_com_param`)
and a later `CoptUpdateparam`'s own Working snapshot both see the derived
timing -- and that extra write is what uniquely exposes this mechanism,
among this codebase's push-then-promote flows, to a concurrent-Working-write
race. Application is otherwise still immediate relative to the exchange
(not deferred to the next *separate* `CoptUpdateparam`): ISO 22900-2
describes the modification as taking effect upon receipt of a positive ECU
response, and deferring it that way would leave the very next exchange
racing against stale hardware timing after a TPI=3 request lengthens it. An
in-flight COP's own remaining receive cycles continue against whatever
`ComParamSet` it already captured per ADR-067; only later COPs observe the
newly modified timing.

Because `ChannelKey` is `(j2534_protocol_id, baud_rate)` with no protocol-
family restriction, two ISO14230 CLLs at the same baud rate can share one
physical channel, and more than one tier-1 registrant can independently
observe the same physical `0xC3` frame in a single `poll_rx_inner` pass.
Rather than issuing one independent `apply_params_to_hardware` call per
registrant — where a later push would silently clobber an earlier one with
no defined winner — every qualifying observation in a pass is merged into
one combined delta first, using the same per-field worst-case direction
table as the functional-addressing combination above (min for `CP_P2Min`,
max for the rest), then applied as a single push. This is a direct
generalization of an already-decided rule (multiple ECU responses to one
registrant) to a second aggregation axis (multiple registrants sharing one
physical channel), not a new design decision.

**Per-CLL storage is arrival-order / last-exchange-wins across registrants,
not a worst-case fold (amended -- Codex round-4 finding, PR #17).** A third
axis found in the same review round as the paragraph above: more than one
tier-1 registrant on the SAME CLL (not just across sibling CLLs) can
independently qualify in one pass — e.g. two concurrently outstanding SID
0x83 COPs, which ADR-100's concurrent-COP model does not rule out. An
earlier cut of this fix treated this the same as the cross-CLL case above
and worst-case-folded these registrants' observations together, but that
verdict was wrong: two different registrants qualifying on the same CLL in
one pass are, by construction, always two INDEPENDENT SID 0x83 exchanges —
never "another ECU responding to the same broadcast," which is a
completely different situation already fully resolved WITHIN one
registrant's own `timing_accumulator`, gated on
`TimingChangeConfig.functional`, before either exchange's result ever
reaches this per-CLL fold. Per-CLL storage is therefore now the
ARRIVAL-ORDER / LAST-EXCHANGE-WINS fold of THAT CLL's own registrants'
observations this pass (`select_latest_timing_changes`), keyed by a
pass-local frame-sequence number captured (on `CopRegistrant.
pending_timing_change`) when each registrant's qualifying response is bound
— the later exchange's result is simply the ECU's more current,
superseding state, not a second data point to worst-case-combine against
an earlier, independent exchange's result. A sibling CLL sharing the
channel still gets its own independent fold.

The CROSS-CLL fold (`timing_delta`, feeding the single shared hardware push
when multiple ISO14230 CLLs share one physical channel) is UNCHANGED and
stays worst-case: unlike the cross-registrant case above, multiple CLLs can
have concurrently LIVE sessions with different legitimate timing
requirements sharing one physical hardware constraint at once — not one
exchange superseding another — so worst-case (min `P2Min`, max everything
else) is what keeps every live session's requirements satisfiable by the
one shared hardware value.

The consuming loop also gates on each CLL's live `connect_generation`
(ADR-086) both when folding (matching the writeback loop immediately above
it in the same critical section) and again when storing after the push
(matching `handle_update_param`'s own post-await recheck), so a
disconnect/reconnect race landing either before or after the hardware push
cannot apply a stale, pre-reconnect observation to the freshly-reconnected
session.

**Deferred-finalization delivery (in-queue reservation, amended -- Codex
round-5 findings, PR #17).** The first cut delivered each frame's
`ReceivedFrame` (including its `ecu_timing_change` flag) to the subscriber
immediately, inside the same per-frame loop that later feeds the
fold/push/store steps above -- so a qualifying `0xC3` frame's delivery could
reach a client before this pass's own hardware push and Active/Working store
had even run, and unconditionally even when that push then failed (round 2
Finding A). Rounds 2-4 all fixed this the same way: a side-buffer
(`buffered_frames`, a plain `Vec` held OUTSIDE the CLL's real event queue)
accumulated every frame from the pass's first qualifying `ecu_timing_change
= true` frame onward, flushed once the fold/push/store outcome was known.
Round 4's own `derived_winner` addition was itself needed because that
side-buffer had no per-registrant granularity to distinguish which of two
independent SID 0x83 exchanges on the same CLL actually won the fold. Round
5 found two further problems with this whole approach, both traced to the
same root cause: the side-buffer duplicated the real queue's own
ordering/eviction/capacity duties in a second, parallel structure that kept
drifting from the real queue's contract as each new edge case was found.

1. **Finding 1 (FIFO violation).** Holding a qualifying frame outside the
   real queue meant a concurrent, unrelated event (cancellation, status,
   error) reaching `deliver_or_enqueue` for the same CLL during the
   buffering window got inserted -- and delivered live, and counted against
   capacity/eviction -- BEFORE the earlier-arriving buffered frame, which
   isn't in the real queue at all yet. This violates the queue's own
   documented true-arrival-order FIFO contract.
2. **Finding 2 (per-frame, not per-registrant, supersession).** Round 4's
   `derived_winner` only tracks the winning REGISTRANT per CLL, not the
   winning FRAME within that registrant. A single physically addressed
   registrant with multiple qualifying responses in one pass (each fully
   overwriting `r.pending_timing_change`, since physical addressing doesn't
   accumulate -- round 3) produces multiple `ecu_timing_change = true`
   frames sharing the same `cop_handle`, so ALL of them wrongly kept the
   flag even though only the LAST one's values actually survived and got
   stored.

The structural fix moves deferral INTO the queue itself, so there is exactly
one ordering authority instead of two that can drift apart. A qualifying
frame reserves its TRUE arrival position in the queue immediately, as a new
`CllQueueItem::PendingTimingFrame { reservation_id, frame }` variant
(`reserve_pending_timing_frame`) -- cap/mode enforcement and `Lost`
notification happen at THIS point, same as any other item
(`push_and_notify_lost`, factored out of the old `deliver_or_enqueue` and
shared by both paths). The queue's live-drain loop
(`drain_queue_live`, the other half `deliver_or_enqueue` was split into) gains
a head-of-line barrier: it stops at the first `PendingTimingFrame` it
encounters, withholding live/poll delivery of that item AND everything
queued behind it. Once this pass's fold/push/store step (above) resolves,
`finalize_pending_timing_frame` corrects the flag and downgrades the item to
a plain `CllQueueItem::Frame`, then re-attempts the drain -- unblocking this
item and, if it was at the front, whatever now-eligible items follow it.
`rpc_get_event_item` (`GetEventItem`) gets the same treatment: it peeks the
queue's front before popping, and refuses to pop (reporting "nothing
available yet" instead) if it is a still-pending reservation, so a poller
can never destroy/skip a reservation `finalize_pending_timing_frame` still
needs to find later by `reservation_id`.

Only finalization -- the flag's final value, and delivery -- is deferred;
queue position, eviction exposure, and capacity accounting are decided at
arrival time, exactly like every other event. **Safety invariant:** this is
sound only because `poll_rx_inner` never aborts mid-pass -- see
`poll_channel_events`'s cancel/shutdown handling, which only breaks BETWEEN
iterations, never inside one -- so a reservation is always finalized in the
SAME `poll_rx_inner` call that created it and can never be stranded pending
forever. A future change to the poll task's abort behavior would need to
preserve this invariant, or reservations could leak.

The store step still returns, per CLL, whether it actually promoted that
CLL's derived ComParams (i.e. the post-push `connect_generation` recheck
passed AND the push itself succeeded); this still forms the `derived_stored`
set of CLL handles, and the fold step still records `derived_winner` (a
per-CLL map to the `cop_handle` whose observation
`select_latest_timing_changes` actually kept this pass), exactly as round 4
left them. Round 5's Finding 2 fix adds a third, per-frame-sequence
correction: `bind_registrant` now also returns `superseded_timing_seq` --
`Some(prev_seq)` when a PHYSICALLY addressed registrant's qualifying
response overwrites (never merges, per round 3) an earlier THIS-PASS
observation from the SAME registrant -- which `poll_rx_inner` folds into a
`superseded_timing: HashSet<(cll_handle, cop_handle, frame_seq)>` alongside
its per-frame reservations. A functionally addressed registrant's responses
are never superseded this way: every one of them genuinely contributed to
the running worst-case accumulator (`timing_accumulator`), so every one
keeps its own flag as long as the other two conditions hold. The pure,
directly-unit-testable predicate `timing_frame_flag_ok(cll_handle,
cop_handle, frame_seq, &derived_stored, &derived_winner,
&superseded_timing)` combines all three checks (replacing round 2/4's
`correct_buffered_ecu_timing_change_flags`, which corrected a whole buffered
`Vec` in one pass rather than being callable per-reservation): `true` iff
`derived_stored` contains the CLL, `derived_winner` names this frame's own
`cop_handle` for that CLL, AND `superseded_timing` does NOT contain this
frame's own `(cll_handle, cop_handle, frame_seq)`. `poll_rx_inner` calls it
once per reservation, in the pass's recorded order (frame_seq order across
the whole batch, interleaved across CLLs -- never reordered/sorted/
parallelized), so an earlier reservation for a given CLL always finalizes
before a later one for the same CLL, letting `drain_queue_live`'s barrier
fully unblock that CLL's backlog by the time the last one for it finalizes.

**Three accepted residuals (not fixed in this round):**

1. (design-advisor) A `Lost` notification for a DIFFERENT evicted/discarded
   item can still be observed live before an earlier, still-pending
   reservation on the SAME CLL finalizes -- `Lost` carries no ordering
   token of its own (`make_lost_notification`'s own doc comment already
   notes it has no queue representation); this is a pre-existing
   out-of-band property this round does not alter.
2. (design-advisor) A losing registrant's `ecu_entry` (a distinct
   `TimingSet`, no key collision with the winner) can still be stored
   (`store_combined_timing_change` stores `ecu_entries` unconditionally)
   even though that registrant's OWN frame's flag now correctly reads
   `false` -- consistent with the flag's derived-focused semantics (it
   asserts "derived timing WAS stored for THIS exchange," not "nothing
   about this exchange was recorded anywhere").
3. (edge-case-hunter) The "sound only because `poll_rx_inner` never aborts
   mid-pass" safety invariant above covers cancellation/shutdown/task-abort
   (verified: no `.abort()` call exists anywhere in the crate, and
   `poll_rx_inner` is never raced inside a `tokio::select!` arm) but not a
   **panic**: a panic anywhere between a frame's `reserve_pending_timing_frame`
   call and that pass's finalize loop would unwind through the spawned poll
   task (`spawn_channel_poll_task` discards its `JoinHandle`, so nothing
   catches or restarts it), permanently stranding that `PendingTimingFrame`
   in the CLL's real queue -- `drain_queue_live`'s barrier and
   `rpc_get_event_item`'s peek gate would then block every subsequent item
   for that CLL, live and polled alike, for the rest of its life. No
   currently-reachable panic exists in that span (the one `.expect()` this
   change adds, on `frame_cop_handle`, is unreachable by construction:
   `ecu_timing_change` is only ever `true` when `bind_frame` supplied a real
   `cop_handle`), so this is a latent robustness gap in the safety argument,
   not a live bug -- but it is a severity escalation versus rounds 2-4's
   side-buffer, where the same panic could only lose that pass's in-flight
   `Vec` (transient), not permanently poison the queue. Left undefended
   (no `catch_unwind`/Drop-guard cleanup) rather than adding panic-safety
   machinery disproportionate to a currently-unreachable trigger; a future
   change introducing a fallible operation between reservation and
   finalization should re-evaluate this residual first.

Migrating a tier-1 registrant to tier-2 (`migrate_registrant_to_receive_only`,
ADR-100 S5) also clears `timing_cfg`, mirroring how it already clears
`rc_cfg` — tier-2 (Receive Only) never runs this mechanism, same as it
never runs RC detection, for the whole registrant lifetime, not just at
COP-creation time.

**In-batch migration window (fixed -- Codex round-6 finding, PR #17).**
`migrate_registrant_to_receive_only`'s `timing_cfg = None` clear above runs
only once the WHOLE poll pass has already returned, but an IS-CYCLIC
(`NumReceiveCycles == -1`) registrant's `tier` flips to `ReceiveOnly`
INLINE, within `bind_registrant`, the instant its first match is accepted
(ADR-100 round-9 Finding-2). Without also clearing `timing_cfg` at that
same inline flip, a second qualifying `0xC3` response for the SAME
registrant later in the SAME `PassThruReadMsgs` batch would still bind
(via the `Tier2` scan, since `tier` had already flipped) and re-enter the
ADR-146 pairing block, making hardware/ComParam updates depend on adapter
batching -- directly contradicting this mechanism's own "tier-2 never
runs it" invariant, and the very latent gap design-advisor flagged during
the round-4 consult but that was left undocumented rather than closed at
the time. `bind_registrant` now also clears `timing_cfg` inline, but only
AFTER that same call's own ADR-146 pairing block has run -- so the
migration-triggering frame itself (a legitimate tier-1 response that also
happens to be this registrant's first accepted match) still gets full
handling; only a frame later in the same batch is affected. `rc_cfg` needs
no equivalent inline clear: its own pending-RC detection is gated on
`scan == AttributionScan::Tier1NonVacuous`, which a `Tier2`-scanned frame
never reaches in the first place, so it was never exposed to this window.

**Snapshot-only, not a replacement (edge-case-hunter finding, round 6):**
`bind_registrant`'s inline clear above mutates only the PASS-LOCAL snapshot
`CopRegistrant` clone the frame loop iterates -- `timing_cfg` is not among
the fields `merge_registrant_writeback` copies back to the live registrant
(unlike `tier`, whose reassignment in `migrate_registrant_to_receive_only`
genuinely is a redundant confirmation, since `tier` IS merged back).
`migrate_registrant_to_receive_only`'s own `timing_cfg = None` therefore
remains the only clear of the LIVE registrant's `timing_cfg` and stays
load-bearing; it must not be removed on the assumption that the new inline
clear already covers its effect.

**Interaction with `CP_EnableConcatenation` (ADR-148) (fixed -- merge
finding, PR #17).** Both mechanisms are scoped to ISO14230, so a client CAN
enable `CP_ModifyTiming` and `CP_EnableConcatenation` on the same CLL at
once, giving one registrant both `timing_cfg: Some(_)` and
`concat_enabled: true`. `bind_registrant`'s ADR-146 pairing block originally
lived only in the plain (non-concat) match arm; a qualifying `0xC3` response
on such a dual-enabled registrant took the ADR-148 concat branch instead
(opening a segment-merge buffer, since every real response satisfies
`ExpectedResponse::matches`) and the pairing block never ran at all --
silently dropping the exchange, with no error or log. The pairing logic is
now extracted into `observe_registrant_timing_change` and called from the
plain match arm, the concat "open new buffer" arm, AND both concat "absorb
into existing buffer" arms (edge-case-hunter finding on the merge fix
itself, PR #17: an already-open buffer is finalized only by the
receive-phase deadline or a cap overflow, never by "another matching frame
arrived", so a SECOND, independent qualifying `0xC3` response sharing an
already-open buffer's exact key -- an ECU retransmission, a repeat/cyclic
exchange, or a second distinct `CP_ModifyTiming` negotiation -- is absorbed
as if it were a continuation; calling this against the CURRENT frame's own
`payload`, not the buffer's accumulated `data`, keeps `r.pending_timing_change`
tracking the LATEST qualifying exchange, matching this mechanism's own
established arrival-order/last-exchange-wins semantics elsewhere
(`select_latest_timing_changes`), instead of silently freezing on the
buffer-opening response's now-stale values). A genuine multi-frame
continuation of the SAME response (as opposed to a second, independent
response colliding on the same key) is not expected to occur in practice --
a qualifying `0xC3` response is always ≤7 bytes (TPI=2's 5 timing bytes plus
2 header bytes), far under any KWP/J1850 frame's data capacity -- but is not
unsafe if it did: `observe_timing_response`'s TPI=2 arm bounds-checks
`payload.get(2..7)` and returns `None` rather than panicking on a short
payload, so a genuine continuation segment (whose second byte is arbitrary
response data, not an echoed TPI) either fails the `payload.get(1) ==
cfg.tpi` gate outright or, in the narrow case where it doesn't, produces at
worst a spurious no-op re-observation, not corrupted or unsafe state. This
fix does NOT touch the separate, ADR-148-owned residual that the same
key-collision still corrupts `ConcatBuf::data` itself (the second response's
raw bytes are still appended as if they were a continuation) -- that is
pre-existing behavior, already accepted for the narrower NRC 0x21/0x23
retransmit case in ADR-148's Amendment 10, and out of scope here; see this
PR's `implementation-notes.md` backlog entry for the general (not
retransmit-specific) case this merge's review surfaced. Accepted residual:
the eventual finalized `ConcatDelivery`'s own `ECU_TIMING_CHANGE` RxFlag (see
below) still reads `false` unconditionally, even for a buffer some of whose
segments WERE paired -- only the ComParam side effect
(`r.pending_timing_change`, and therefore the eventual Active/Working store)
is preserved; retrofitting the flag onto a delayed concat delivery would
need `ConcatBuf` to carry its own `(ecu_timing_change, superseded_timing_seq)`
through to finalization, not done here.

**`ECU_TIMING_CHANGE` RxFlag.** ISO 22900-2's RxFlag table places this bit
at byte 1, bit 1 — distinct from the byte-3 range ADR-098 reserved for raw,
bit-for-bit-forwarded native J2534 `RxStatus` flags, so there is no
collision. `rx_flag_bytes` gains a second parameter for this synthesized
bit — the first RxFlag bit this service computes itself rather than
forwards verbatim from the adapter. It is set on the specific bound `0xC3`
response frame(s) that qualified this pass (every qualifying response in
the functional-addressing case) *and* whose derived ComParams this same
pass's fold/push/store step actually promoted into Active (the
deferred-finalization delivery correction described above,
`timing_frame_flag_ok`) — asserting "derived timing WAS stored into
Active," not just "a qualifying response was received." As of the
`WorkingTimingSnapshot` guard (Codex round-7 finding, PR #17; see "Hardware
application and locking" above), the flag is tied to Active only: Working
is now first-wins-guarded against a concurrent client write, so a frame
can carry the flag `true` (Active updated, hardware push succeeded) in the
same pass where that pass's own Working write for the same key was skipped
because a concurrent `SetComParam`/Temp-writeback had already changed it.
The flag therefore does not guarantee a subsequent `GetComParam` (which
reads only `link.working`) reflects the exchange — only that Active does. It is
never set on TPI=0, a TPI=1 that found no `TimingSet=1` entry to reapply,
loopback/TX-done frames, any other indication, or a qualifying response
whose pass ultimately failed the hardware push or lost the post-push
`connect_generation` recheck.

**Structfield plumbing.** `CP_AccessTiming_Ecu`, `CP_AccessTimingOverride`,
and `CP_SessionTiming_Ecu` all reuse this codebase's existing STRUCTFIELD
plumbing (`comparam_defaults::access_timing_empty()`/`session_timing_empty()`,
`comparam_support::STRUCTFIELD_PARAMS`, and the corresponding
`rpc_link.rs` Get/SetComParam match arms) rather than a new shape — the
proto's `ParamAccessTiming`/`ParamSessionTiming` messages already match
ISO 22900-2's structs field-for-field. `CP_AccessTiming_Ecu` and
`CP_AccessTimingOverride` move from `is_can_param` to `is_kwp_param`'s
ISO-14230-specific branch (not the full KWP-family grouping — see
Detection above), correcting the backwards allow-listing. `CP_ExtendedTiming`
has the identical backwards CAN/KWP allow-listing mistake in the same
functions and is corrected in the same commit as a mechanical, no-design-
alternative fix.

## Consequences

- This is a new state-machine mechanism layered onto the existing
  ADR-100 registrant/attribution model and the existing ADR-098 RxFlag
  scheme; it introduces the first RxFlag bit this service synthesizes
  itself rather than forwards from the adapter unmodified.
- Accepted residual: TPI=1 with no prior `TimingSet=1` observation is a
  documented no-op (no invented ISO 14230-2 default constants).
- Accepted residual: `P2Max`'s ×500 µs conversion is provisional pending
  verification against ISO 14230-2's own resolution table, which this
  workspace does not have primary access to; `P3Max`→`CP_P2Star`'s 250 ms
  conversion is unambiguous in the in-repo ISO 22900-2 source and is not
  affected by this residual.
- Accepted residual: an in-flight COP's remaining receive cycles keep
  their ADR-067 Working snapshot even after a same-channel timing
  modification; only subsequently issued COPs see the new timing.
- CAN/UDS SID 0x10/0x50 (`CP_ModifyTiming`'s other named mechanism) remains
  entirely out of scope for this change. **Closed by ADR-150**, which
  extends `TimingChangeConfig` to a `KwpAccess`/`UdsSession` enum and
  reuses this ADR's pairing/fold/store/`WorkingTimingSnapshot` machinery
  for the UDS DiagnosticSessionControl (SID 0x10/0x50) exchange on
  `ISO15765` channels.
- Accepted residual (design-advisor, PR #17 review round; superseded by the
  Finding A deferred-finalization delivery fix above (originally
  "buffered-suffix delivery," round 2; the in-queue reservation mechanism,
  round 5) — the ORIGINAL version of this bullet justified "no error event
  on push failure" by the flag having "already" been delivered before the
  push ran, which the fix below made no longer true, so the justification,
  not just the wording, needed replacing): a failed hardware push (or a
  lost post-push `connect_generation` recheck) is still warn-logged only,
  with no explicit error event — no COP is terminal at this point to
  attach one to. The deferred-finalization delivery fix means the affected
  frame's own `ECU_TIMING_CHANGE` flag now correctly reads `false` in this
  case (the
  client is never told "timing changed" for a change that never landed),
  but the client still learns of the failure only passively — by the
  absence of the flag it might have expected, or by a later `GetComParam`
  read still showing the pre-exchange values — not through a positive
  notification of the failure itself. A client-visible recovery path is a
  later `CoptUpdateparam` or re-running the exchange; the service always
  reports adapter-true values, never a value it merely wished were on
  hardware.
- Accepted residual (design-advisor, PR #17 review round, Finding A
  follow-up): if a CLL is torn down between this pass's fold (which reads
  `link.hw_protocol_id` and folds that CLL's contribution into the
  cross-CLL `timing_delta`) and the post-push re-acquisition (which finds
  the CLL simply gone from `logical_links` and skips it), that CLL's own
  contribution to the cross-CLL hardware push is neither rolled back nor
  re-attributed to a surviving sibling CLL sharing the same physical
  channel — the push already happened with that contribution folded in,
  and there is no surviving per-CLL state left to store into or correct.
  This is judged acceptable rather than fixed in this change: the teardown
  itself is the reason no further store is needed for that CLL, and a
  sibling CLL on the same channel still gets its own correctly-folded
  and correctly-stored outcome independent of the torn-down CLL's
  presence or absence in the fold.
- Accepted residual: the worst-case functional-addressing combination
  (`timing_accumulator`) assumes a CLL's own poll pass is the only writer
  ever folding that CLL's registrants' observations — true today because
  ISO 14230 has no UUDT companion channel (ADR-046) and therefore no second
  concurrent poll task for the same CLL, unlike the CAN/ISO15765 UUDT case
  `merge_registrant_writeback`'s own doc comment describes. If a future
  change (e.g. `docs/j2534-2-support-plan.md`'s KWP scope work) ever gives
  an ISO 14230 CLL a second concurrent poll task, this accumulator's
  single-writer assumption would need revisiting alongside it.
- Backlog: this mechanism's push-then-promote storage logic
  (`select_latest_timing_changes`/`store_combined_timing_change`) is
  covered by pure-function unit tests (fold correctness, and the
  `hardware_push_ok` gate, each with a fail-without/pass-with regression
  proof) in `events.rs`, but not yet by a full-pipeline integration test
  driving a real ISO14230 `CoptSendrecv` through a mocked SID 0x83/0xC3
  exchange with a forced `SET_CONFIG` failure (`j2534-0404-mock` also has
  no such error-injection hook yet). Attempted during this review round and
  abandoned after repeated KWP RX-framing/COP-matching mismatches produced
  a hanging test rather than a working one — the risk of shipping a flaky
  integration test outweighed the coverage gain given the unit-level proof
  already in place. A future attempt should start from an existing non-init
  KWP `CoptSendrecv`-with-response round-trip test as a template (none
  currently exists in `tests/grpc_mock/`; the closest precedents are
  fast-init-specific) rather than reconstructing the KWP header/addressing
  and `ExpectedResponseData` vacuous-match conventions from scratch.
- Accepted residual (design-advisor, Codex round-4 finding, PR #17): the
  cross-CLL `timing_delta` fold only combines THIS PASS's observations. If
  one CLL sharing a physical channel has a standing timing requirement
  established by an earlier pass, and only a sibling CLL on that same
  channel observes a qualifying exchange this pass, the resulting hardware
  push is folded only from this pass's contributors and could under-serve
  the first CLL's still-live session. This is pre-existing behavior, not
  introduced or fixed by this round's arrival-order change to the
  cross-registrant fold.
- Accepted residual (design-advisor, Codex round-4 finding, PR #17): a
  CLL's own stored `derived` values (now arrival-order / last-exchange-wins
  across that CLL's own registrants, per this round's fix) can diverge from
  what was actually pushed to hardware (the cross-CLL worst-case-folded
  `timing_delta` value) whenever that CLL shares a physical channel with
  another CLL that also qualifies in the same pass. "Active means actually
  on hardware" (this ADR's own earlier amendment, "Hardware application and
  locking") is therefore only exactly true for a CLL on a channel it does
  not share with another qualifying CLL in that pass; a shared-channel CLL's
  `GetComParam` view reflects its own last exchange, not necessarily the
  more conservative value the shared push actually applied.
- Accepted residual (design-advisor, Codex round-7 finding, PR #17): the
  `WorkingTimingSnapshot` first-wins guard has an ABA case -- a concurrent
  client write that happens to write the SAME value the snapshot captured
  is treated as "unchanged" and gets overwritten by the exchange's value
  anyway. This is sound because that write has no observable side effects
  beyond the map write itself for these specific ComParam IDs (verified:
  `rpc_set_com_param`'s ComParam-set arms have none), so serializing that
  concurrent RPC as happening BEFORE the fold is a legal reordering with an
  identical observable outcome. This same guard also automatically covers
  the ADR-067 claim-D Temp-writeback (`l.working = l.active.clone()`, in
  `rpc_primitive.rs`, which replaces the WHOLE Working set including
  structfields with no generation guard of its own) -- the whole-structfield
  and per-key compares correctly detect this as "changed" and skip, letting
  the writeback's overwrite win for Working while Active still gets the
  derived values. After this fix, a skipped Working key can leave Working
  diverging from Active until the client's own next `CoptUpdateparam` --
  this is not a bug, it is the normal D-PDU API staging semantics of any
  pending `SetComParam` before its own `CoptUpdateparam`, not something
  specific to this mechanism.
