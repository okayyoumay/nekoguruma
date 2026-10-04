# ADR-180: SAE J1939 Claim Registration Atomicity, Timeout Cancellation, and Post-Claim Optional StartComm Message

**Date:** 2026-08-14
**Status:** Accepted (amends ADR-179 Decision 3: claim-registration atomicity, timeout-cancel mechanism, re-StartComm cancel ownership, and spontaneous-loss cancel ownership; amends ADR-179 Decision 3's optional-CoptStartcomm-message scoping, extended to tester-present framing; Decision 10 amends ADR-165 Decision 3's repeat-slot lifecycle to stop on a J1939 CLL's address relinquishment; Decision 11 extends the same relinquishment-anchored cancellation to live CoptSendrecv COPs; Decision 12 amends ADR-179 Decision 3's CoptUpdateparam handling to keep CP_TesterSourceAddress aligned with a live claim; Decision 13 extends Decision 10's repeat-slot lifecycle further, closing a slot's own creation window during a pending claim, not just its continued transmission after one changes; Decision 14 extends Decision 11's live-`CoptSendrecv` cancellation with a `StartComPrimitive`-time gate and closes its own enqueue-to-dispatch TOCTOU with a transmit-time re-check; Decision 15 extends Decision 12's `CoptUpdateparam` guard with a second, authoritative execution-time re-check closing its own enqueue-time TOCTOU; Decision 16 extends `dispatch_due_tester_present`'s dispatch filter to exclude a negotiation-enabled J1939 CLL with no currently claimed address, mirroring Decision 13's own repeat-slot gate for tester-present; Decision 17 resets `j1939_claimed_address`/`j1939_claim_cursor` at `ConnectComLogicalLink` finalization so a same-handle reconnect never inherits a prior generation's stale claim state; a round-14 correction to Decision 3 recomposes the optional CoptStartcomm message's header against `binding.resolved()` instead of live Active, so a `temp_param_update` CoptStartcomm's Working-bound target/PGN/priority survive the claim-completion recompose; Decision 15 is extended (round 15) with an identical execution-time re-check for CP_J1939TargetAddress's 0xFFFF sentinel; Decision 18 adds a persistent j1939_negotiation_engaged flag so a temp_param_update CoptSendrecv cannot spoof a negotiation opt-out via a Working-only staged CP_J1939AddressNegotiationRule; a round-16 correction closes Decision 18's own previously-accepted residual by having the non-negotiated re-StartComm branch also cancel and deregister any address the CLL previously claimed; Decision 19 re-verifies a CoptStopcomm's pre-composed J1939 source address against the CLL's live claim immediately before transmit, suppressing (not sending) it on a drift; a round-17 correction relocates Decision 15's two execution-time re-checks to run before the hardware push (not after), closing a partial-apply bug where a rejection used to leave the adapter already holding every other staged ComParam from the same CoptUpdateparam; Decision 20 routes a `stop_repeat_slots_for_cll` STOP failure other than ERR_INVALID_MSG_ID into `SharedChannel::leaked_repeat_message_ids` for opportunistic retry (ADR-165 Decision 6), instead of silently dropping it; a round-20 correction to Decision 2 retains a candidate's local claim-registration entry when its timeout-triggered native cancel itself fails, instead of forgetting it unconditionally and risking a sibling double-claim; a round-20 correction to Decision 18 replaces its persistent bool with a tri-state `J1939NegotiationPosture` (Undecided/Engaged/OptedOut) so a real, successfully-opted-out CoptStartcomm is no longer mistaken for still-negotiated by later ordinary sends reading a stale Active ComParam; a round-21 correction to Decision 2 fails a claim attempt closed instead of advancing the cursor when a timed-out candidate's native cancel itself fails, closing a double-claim window the round-20 correction's own retain-on-failure left open; a round-21 correction widens `stop_repeat_slots_for_cll` to check `connect_generation`, the same bug class the round-16 correction already fixed for `cancel_j1939_claims_for_cll`; a round-22 correction widens `cancel_send_recv_cops_for_cll` to check `connect_generation`, the same bug class already fixed for its two sibling helpers; a round-22 correction rejects a non-empty, wrong-length CP_J1939Name at both CoptStartcomm and CoptUpdateparam call time instead of silently zero-padding/truncating it; Decision 21 has the claim loop's own bounded wait observe an in-flight CoptStartcomm COP's own cancellation via `cancelled_cops`, ending the wait promptly as `Cancelled` instead of running to completion or holding the full timeout; Decision 22 routes a relinquishment's failed native cancel into `SharedChannel::leaked_j1939_claims` for opportunistic retry by the next claim attempt considering that address, instead of forgetting it and risking a double-claim; a round-23 correction to Decision 22 replaces its per-candidate leaked-set guard with a channel-wide batch-reconcile-then-gate, since a claim attempt whose own candidate list was disjoint from the leaked address bypassed the original guard entirely; a round-24 correction to Decision 18 gives the negotiation opt-out branch the same channel-wide reconcile-then-gate (via a helper shared with round-23's own mechanism), since that branch discarded a failed relinquishment-cancel's outcome and promoted OptedOut regardless, letting the client transmit under a new source while the adapter might still defend the old address; Decision 23 (round-25 correction) widens `SharedChannel::j1939_claims` to carry each entry's own SAE J1939 NAME, adding a NAME-collision gate to the claim loop (sibling CLLs sharing a NAME could each claim a different address) and a NAME-drift guard to CoptUpdateparam at both enqueue and execution time (mirroring the existing CP_TesterSourceAddress guard), since neither the address-keyed map nor any prior guard ever tracked NAME identity; a round-26 correction to Decision 23 fixes both `CoptUpdateparam` NAME guards and the spontaneous-reclaim path, all of which wrongly read Active's own CP_J1939Name instead of the NAME a Temp-bound claim actually defends, by widening `SharedChannel::j1939_reclaim_pending` to carry the defended NAME (and connect_generation) captured atomically at the moment of spontaneous loss; a round-27 correction to Decision 22 gives `ioctl_start_repeat_message` the same channel-wide reconcile-then-gate as a third call site, since an opted-out/client-managed-source sibling CLL could start an autonomous repeat slot while a leaked claim on the same physical channel remained unresolved, a case the pre-existing per-CLL `j1939_negotiated_unclaimed` gate could never reach; Decision 24 has a spontaneous J1939 reclaim observe a pending CoptStopcomm via a new `stop_comm_pending` check, aborting closed before issuing any native claim instead of blocking the physical channel's poll task for the reclaim's full candidate-list duration before the queued StopComm can execute; a round-27 own-round correction to Decision 24 has that same abort re-arm the CLL's own pending reclaim entry instead of abandoning it, since `stop_comm_pending` is only provisional and can revert to `false` without the StopComm ever running)
**Affects:** `j2534-0404-service/src/service/events_j1939_claim.rs`, `j2534-0404-service/src/service/events.rs`, `j2534-0404-service/src/service/rpc_primitive.rs`, `j2534-0404-service/src/service/rpc_link.rs`, `j2534-0404-service/src/service/rpc_misc.rs`, `j2534-0404-service/src/service/protocol.rs`, `j2534-0404-service/src/service/tx_header.rs`, `j2534-0404-service/src/service.rs`, `j2534-0404-service/src/service/names.rs`, `j2534-0404-service/src/service/events_build_cll_rx_entries_tests.rs`, `j2534-0404-service/src/service/events_cancel_held_tx_items_generation_tests.rs`, `j2534-0404-service/src/service/events_combined_timing_change_tests.rs`, `j2534-0404-service/src/service/events_cyclic_deadline_confirm_writeback_tests.rs`, `j2534-0404-service/src/service/events_registrant_lifecycle_tests.rs`

## Context

PR #72 (SAE J2534-2 Phase 5, ADR-179) implemented SAE J1939's address-claim/defend state machine. Three successive Codex review rounds on that PR found real concurrency and correctness gaps in the same mechanism, `events_j1939_claim.rs::run_j1939_claim_loop` and its caller `events.rs::handle_start_comm`, each fixed independently before the next round found the next gap:

- **Round 1**: an unconditional overwrite of `SharedChannel::j1939_claims`' entry let a later sibling CLL's claim attempt on the same physical channel silently steal address-claim-indication routing away from a CLL that already legitimately owned or was attempting that address.
- **Round 2**: a TOCTOU race between the native `protect_j1939_addr` claim issuing and this attempt's own registration in `SharedChannel::j1939_claims` — a `DisconnectComLogicalLink` landing in that window ran `cancel_j1939_claims_for_cll` (a one-shot cleanup pass) before the attempt was registered, so it found nothing to cancel, and the loop then registered the entry AFTER teardown had already run. That orphaned entry was never cleaned up by anything, and — since it still carried a live `connect_generation` (a plain Disconnect does not bump it, ADR-086) — the round-1 sibling-ownership guard would still read it as "live" and permanently block a sibling from that address. Round 2's fix narrowed this window (a staleness recheck immediately before the registration write) but left it as two separate lock acquisitions, not one atomic section.
- **Round 3**: three further findings on the same mechanism. (A) A claim-enabled `CoptStartcomm`'s optional message (`cop_data`, resolved into `tx` by `rpc_start_com_primitive` at call time) was silently discarded rather than sent, since J1939's outbound framing needs the CLL's own claimed source address, not yet known at RPC-call time. (B) When a claim attempt's bounded wait timed out with no `CLAIMED`/`LOST` indication at all, the cleanup path removed only the local routing entry, never issuing a native cancel — if the adapter had actually accepted the claim non-blocking, the device could keep defending that address with nothing in this service's own bookkeeping pointing at it anymore. (C) Round 2's fix, while narrower than round 1's original gap, still left the recheck and the registration write as two separate `.await` lock acquisitions — Codex correctly identified this as insufficient, not merely narrow.

- **Round 4** (after Decisions 1–3 below were committed): Codex found one further gap the three-owner cancel invariant Decisions 1–2 establish (see below) did not cover — a `StopComm` → `CP_J1939PreferredAddress` change → `StartComm` cycle left the CLL's own previously claimed address registered and defended forever, since none of the three existing cancel owners (teardown, an explicit `Lost`, a timed-out attempt) ever run for that cycle. A subsequent `edge-case-hunter` verification pass against the round-3 diff separately found three narrower gaps in the same mechanism. See Decision 4 and the Consequences section below.

- **Round 5** (after Decision 4 was committed): Codex found the sibling gap to Decision 4, in `deliver_j1939_claim_indication`'s "later, spontaneous `_LOST`" case (ADR-179 Decision 3's own term for an out-of-band loss with no `run_j1939_claim_loop` wait in flight) — that path armed a fresh reclaim from `j1939_claim_cursor = 0` but never removed the just-lost address's own `SharedChannel::j1939_claims` entry. If the fresh reclaim lands on an EARLIER candidate than the one just lost, the lost address's stale entry has no owner left to ever clean it up. See Decision 5.

- **Round 6** (after Decision 5 was committed): Codex found three further findings. (A) `tester_present_data` (resolved at `StartComPrimitive` call time, ADR-067, the same way `tx.send.data` was before Decision 3) is never recomposed against the claimed address on a successful initial claim, the identical gap Decision 3 already closed for the optional `CoptStartcomm` message but never applied to tester-present. (B) The sibling gap to (A): `run_j1939_reclaim_duties`'s own `Claimed` arm never refreshes a live `TesterPresentState::Armed`'s cached `framed_data` either, so a spontaneous reclaim onto a different address would leave it transmitting under the lost address. Both (A) and (B) are fixed (see Decision 6) — but proved unreachable via the live RPC surface while writing regression tests for them (see Consequences). (C) `CP_J1939TargetAddress` (a one-byte wire field) had no upper-range validation beyond the pre-existing `0xFFFF` "not configured" sentinel check, silently truncating an out-of-range value rather than rejecting it — a straightforward, mechanical fix (`rpc_primitive.rs`) with no design alternatives, so it is not a Decision item here; see the Consequences section and `implementation-notes.md`.

- **Round 7** (after Decision 6 was committed): Codex found two further findings. (1) Round 6's mechanical `CP_J1939TargetAddress` upper-range/sentinel validation was added only to `CoptStartcomm`'s handler, not to `CoptUpdateparam`'s — so a CLL already started on a valid J1939 target address could have that ComParam pushed back to an out-of-range value or the `0xFFFF` sentinel via `CoptUpdateparam` alone, with no synchronous rejection, mirroring the same gap the pre-existing FD-mode guard was already written to close for a different ComParam. (2) `run_j1939_claim_loop` never rejected `CP_J1939PreferredAddress` candidates of 254 or 255 before issuing a native claim for them — ADR-179's own Decision 3 text (and clause 16.3.3.2) already documents that neither is ever a valid explicit claim target, but nothing in the loop actually enforced it locally; the native side does reject both (confirmed against the mock's `IOCTL_PROTECT_J1939_ADDR` handler), so this is a defense-in-depth/wasted-native-round-trip gap rather than a reachable claim-success bug. See Decision 7 and the Consequences section.

- **Round 8** (after Decision 7 was committed): Codex found three further findings. (1) `CP_TesterSourceAddress` (`NODE_ADDRESS`) became client-writable on a J1939 CLL once ADR-179 Decision 3's own client-readback fix admitted it into `is_j1939_param`'s allowlist, but `rpc_set_com_param`'s pre-existing out-of-range guard for the same underlying ComParam was scoped to J1850/KWP links only, never extended to J1939 — a mechanical validation gap, the same shape as round 6/7's `CP_J1939TargetAddress` fix, with no design alternatives. (2) `CP_J1939PDUFormat`/`CP_J1939PDUSpecific` (both one-byte wire fields `tx_header::j1939_header_bytes` narrows with `as u8`) had no upper-range validation at all — the identical mechanical gap. Both (1) and (2) are fixed in `rpc_link.rs`'s `rpc_set_com_param`; neither is a Decision item here (CLAUDE.md's "ADR not needed" criteria — straightforward validation fixes with no design alternative), see the Consequences section and `implementation-notes.md`. (3) A SIXTH gap in the "every issued claim has exactly one cancel owner" invariant (Decisions 1, 2, 4, 5 below): a claim-enabled `CoptStartcomm` carrying an optional message can have its post-claim transmit sequence abort (P3 wait cancelled, a staleness recheck, or `transmit_request` itself failing) without ever cancelling the just-succeeded claim or clearing `j1939_claimed_address`, since `comm_started` is only set `true` in that sequence's own success tail — none of its early-return branches know about J1939 claim cancellation at all. This is a real design question (a fourth instance of the same recurring "who cancels the claim" bug class, following Decisions 2/4/5), routed to a `design-advisor` consult; see Decision 9.

- **Round 9** (after Decision 9 was committed): Codex found two NEW findings on a
  never-before-considered interaction between this ADR's own claim/reclaim
  mechanism and SAE J2534-2 clause 14 Repeat Messaging (ADR-165, developed
  independently, many rounds earlier, over Phase 12's own history). (1)
  `tx_header::response_header_bytes` had no J1939 arm at all — see ADR-179
  Decision 9, which resolves this finding directly since it is a gap in that
  ADR's own message-framing decision (Decision 6), not this ADR's
  claim-atomicity mechanism. (2) A live repeat slot's device-side frozen TX
  bytes (ADR-165 Decision 3) still carry the SAE J1939 source address that
  was claimed when `START` ran — neither a spontaneous `_LOST` (Decision 5's
  site) nor a fresh `StartComm`'s own claim reset (Decision 4's site) ever
  stops the CLL's own live repeat slots, so the device keeps retransmitting
  under an address it (or a sibling that has since claimed it) no longer
  holds, for up to the full reclaim window or forever on `Exhausted`. Both
  findings were routed to the same `design-advisor` consult (they concern
  related state under J1939 addressing); see Decision 10 for finding (2)'s
  resolution.

- **Round 10** (after Decision 10 was committed): Codex found two MORE
  findings, both further instances of Decision 10's own recurring "framed/
  cached J1939 data goes stale when the claimed address changes" bug class
  (Decisions 3, 6, and 10, all in this same file, already fixed three prior
  instances). (1) `events.rs::handle_send_recv`'s own doc comment
  establishes that every cycle of a multi-cycle/cyclic `CoptSendrecv`
  (`CycleContinuation`) retransmits the SAME `SendRecvTx::data` resolved
  once at `StartComPrimitive` call time (ADR-067) — for J1939, already
  framed with the source address live at that moment — and nothing ever
  recomposes or invalidates it, so a still-running send keeps transmitting
  under a lost or reassigned address indefinitely after a relinquishment.
  (2) `CoptUpdateparam` had no check on `CP_TesterSourceAddress` (native
  `NODE_ADDRESS`) at all — a client could independently promote it to a
  DIFFERENT value than the live claim owns, moving Active `NODE_ADDRESS`
  (which every subsequent frame composer reads) out from under the claim
  machinery, which stays completely unaware of the change. Both routed to
  the same `design-advisor` consult (both concern the claimed address
  going out of sync with cached/framed data, the same theme as Decision
  10); see Decisions 11 and 12.

Three rounds finding new gaps in the same ~180-line function is itself a signal: this needed an actual design decision, not another incremental patch. A `design-advisor` consult was used to produce one (see the three findings' resolution in Decision below). The consult also identified that this codebase already has a documented, established answer to the root cause of Finding C: [ADR-080](ADR-080-shared-channels-outermost-lock-hierarchy.md) declares `shared_channels` the outermost of the three-mutex hierarchy (`shared_channels`, `logical_links`, `api`), sanctions holding it across nested `logical_links` reads/writes and even native FFI calls, and both J1939 disconnect/destroy teardown paths (`rpc_link.rs::rpc_disconnect_com_logical_link`/`rpc_destroy_com_logical_link`) already follow it — acquiring `shared_channels` first and holding it continuously across their own `logical_links` mutation and their `cancel_j1939_claims_for_cll` call. `run_j1939_claim_loop` was simply the one module in this area not yet using that established order.

## Decision

**1. Claim issue + registration is now one ADR-080-conformant `shared_channels`-held critical section.** The round-1 sibling-ownership check, the native `protect_j1939_addr` issue, and the registration write into `SharedChannel::j1939_claims` are folded into a single section: acquire `shared_channels` first, nested-acquire `logical_links` for the staleness and sibling-ownership checks (each a separate, sequential acquisition — never held simultaneously with `shared_channels`' own nested `api` acquisition), nested-acquire `api` for the native issue, then write the registration — all before releasing `shared_channels`. Because the disconnect/destroy teardown paths already hold `shared_channels` continuously across their own `logical_links` mutation and `cancel_j1939_claims_for_cll` call (ADR-080), this fully serializes the two sequences: a claim attempt's registration either happens-before teardown's cancellation pass (which then finds and cancels it) or happens-after teardown already marked the CLL stale (in which case the nested staleness check inside this section stops the attempt from ever being issued or registered at all). `protect_j1939_addr` is a thin, non-blocking native primitive, so holding `shared_channels` across it matches ADR-080's own accepted-cost precedent (`ioctl_start_msg_filter`'s hardware filter install under the same guard). The critical section ends before the wait loop begins — `poll_rx`/`deliver_j1939_claim_indication` acquire `shared_channels` themselves, and a wait loop holding it for the full `CP_J1939AddrClaimTimeout` duration would serialize every other physical-channel operation for that long.

**2. A claim attempt's bounded wait timing out with no indication now issues a best-effort native cancel before removing the local routing entry.** The inner wait loop distinguishes an explicit `Lost` outcome (the device itself reported relinquishing the address, clause 16.4.6 — no cancel needed) from a timeout with no indication at all (the device may still be processing or defending the candidate non-blocking). Only the latter issues `api.cancel_j1939_addr_protect` for the address, under the same `shared_channels`-held critical section as the local entry removal, and — critically — **before** that removal: with Decision 1 in place, a sibling CLL can only register this exact address once the local entry is gone, so cancelling while it is still present excludes an interleave where our late cancel would land after a sibling has already re-claimed the same address. This is the opposite order from `cancel_j1939_claims_for_cll`'s own remove-then-cancel sequence, which is safe there only because its caller already holds `shared_channels` throughout the whole remove-and-cancel sequence — the same mutual-exclusion guarantee, achieved by a different ordering appropriate to each site's own shape.

**Round-20 correction (Codex review, PR #72):** Decision 2's own local-entry removal ran unconditionally after the cancel attempt — regardless of whether `api.cancel_j1939_addr_protect` actually succeeded. On a failed cancel, the adapter may genuinely still be defending the address (the same non-blocking-processing possibility this Decision's own text already cites as the reason a cancel is attempted at all), but the local entry was forgotten anyway — freeing the address for a sibling CLL's claim loop to issue its own native claim for it, producing two claimants defending one address, exactly the bug class this whole ADR exists to close. Fixed by removing the local entry only when there is no reason to believe the adapter is still defending it: an explicit `Lost` (`timed_out_without_indication == false`, no cancel needed to begin with), or a `timed_out_without_indication` cancel that actually returned `Ok`. A failed cancel now retains the entry, still attributed to this exact `(cll_handle, connect_generation)` — not an orphan, since `owned_by_a_live_sibling` (Decision 1) only ever blocks a DIFFERENT `cll_handle`; it is cleaned up by whichever of this ADR's existing cancel owners next runs for this CLL (a fresh claim attempt's own `cancel_j1939_claims_for_cll` sweep, Decision 4; or teardown, Decision 1). *(The claim loop's own behavior after a failed cancel — originally "advances past this candidate unconditionally" — is corrected below, round-21.)* **No dedicated regression test** — the mock's own `IOCTL_PROTECT_J1939_ADDR` (non-cancel form) always synchronously enqueues either a CLAIMED or LOST indication at claim-issue time, so `timed_out_without_indication` itself has never been reachable via this harness (the same infeasible-to-construct-deterministically class Decision 2's own original mechanism already documents, for the identical reason — no test-only hook exists to simulate the adapter silently never responding at all); verified by code inspection instead, and by the full J1939 suite (55 tests) passing unchanged.

**Round-21 correction (Codex review, PR #72; `design-advisor` consult):** the round-20 correction above still advanced the claim loop's cursor unconditionally after a failed cancel, moving on to wait on the next candidate. But `SharedChannel::j1939_claim_results` (the map `deliver_j1939_claim_indication` writes a late CLAIMED/LOST indication into, and the wait loop reads outcomes from) is keyed by `cll_handle` alone, not by address — so a delayed adapter response for the RETAINED candidate, arriving while the loop now waits on a DIFFERENT candidate, fails the wait loop's own address match and is silently dropped. A late `Claimed` for the retained address then sits unconsumed while the next candidate might also succeed: this NAME would then be defended by the adapter at two source addresses simultaneously — a genuine SAE J1939 clause-16 one-NAME-one-address violation, undetected until whichever cancel owner next runs for this CLL. Fixed by treating a failed timeout-cancel as terminal for the whole attempt: the loop returns `Exhausted` immediately instead of advancing, so it never has a second potentially-live candidate in flight while one's outcome is still in doubt — closing the double-claim window outright rather than merely narrowing it. No new field or routing change: the retained `j1939_claims` entry above is already the in-doubt record, and its existing reconciliation owners (Decision 4's sweep, or teardown) already re-cancel, deregister, and clear any stale `j1939_claim_results`/`j1939_reclaim_pending` for this CLL the next time either runs. **Rejected alternatives:** an address-keyed (or per-attempt) outcome map with late-outcome reconciliation — rejected as over-engineering that must itself answer "does a late higher-preference `Claimed` cancel the in-flight next candidate, whose own cancel can then also fail, recursing the same problem" and abandons ADR-179 Decision 3's one-attempt-in-flight-per-CLL model several other sites rely on; a bounded cancel-retry before advancing — rejected, since retrying inside the critical section holds `shared_channels` across repeated native calls (an ADR-080 violation class), while releasing between retries reopens the same interleavings, and the natural retry already exists via Decision 4's sweep; documenting the double-claim window as an accepted residual instead of fixing it — rejected, since (unlike this ADR's other infeasible-to-close race windows) the concrete failure case is fully constructible and is a live protocol violation, not a bookkeeping blemish. **No dedicated regression test** (extends the round-20 correction's own identical-class limitation above — deterministic coverage would need a mock hook making `timed_out_without_indication` itself reachable, which does not currently exist); a backlog note recording the two mock hooks (a cancel-failure trigger, and a claim-response-suppression trigger) that would retire this limitation is added to `j2534-0404-service/docs/implementation-notes.md`. Verified by code inspection and the full J1939 suite passing unchanged.

Decisions 1 and 2 together establish the invariant this ADR records explicitly: **every issued `PROTECT_J1939_ADDR` claim has exactly one cancel owner** — a successful claim is owned by teardown's cancellation pass (or the physical channel's own disconnect); an explicit `Lost` needs no cancel (the device relinquished it itself); a timeout with no indication is owned by the claim loop itself (Decision 2, now retaining ownership on a failed cancel and failing the whole attempt closed rather than advancing past it, per the round-20/round-21 corrections above); and an address already owned by a live sibling, or a stale/disconnected attempt, is never issued a native claim at all (Decision 1 / the pre-existing round-1 guard).

**3. The optional CoptStartcomm message is now sent after a successful claim, with its header recomposed against the claimed address.** `handle_start_comm`'s J1939 branch no longer silently discards `tx` (the optional message resolved by `rpc_start_com_primitive` at `StartComPrimitive` call time, per ADR-067's call-time snapshot). J1939 message headers are composed from the CLL's live Active `ComParamSet` at the point of send (`tx_header::j1939_header_bytes` reads `NODE_ADDRESS` from it), not baked in once and reused — so on a `Claimed` outcome, in the same critical section that writes the claimed address back into `NODE_ADDRESS`, `tx.send.data`'s 5-byte header is recomposed against that just-written Active set (the payload bytes after it are left untouched), and the whole `if/else if` chain that used to make the J1939 claim branch and the optional-message transmit sequence mutually exclusive is restructured so the shared transmit sequence (ADR-111) now runs after a successful J1939 claim too. On `Exhausted`/`Stale`/`HardError`, the message is still never sent — those arms already `return` before reaching the transmit sequence, matching the K-line init-failure precedent (a failed StartComm never gets to send its optional message either).

**Round-14 correction (Codex review, PR #72):** the recomposition above described reading "the CLL's live Active `ComParamSet`," which was accurate for a Plain-bound `CoptStartcomm` but wrong for a `temp_param_update` one. `tx.send.data`'s ORIGINAL composition (`rpc_primitive.rs`, at `StartComPrimitive` call time) resolves every header field via `binding.resolved()` (ADR-067: Working when `temp_param_update` is set, else the call-time-bound Active snapshot) — but this recomposition read `link.active` unconditionally, silently discarding the Temp-bound target address/PDU format/specific/data page/priority for every field except the source address it was actually meant to update, potentially addressing the wrong ECU/PGN/priority even though the claimed source address itself was correct. Fixed by recomposing against `binding.resolved()` instead of `link.active` — `binding` is the same `ParamBinding` this whole `CoptStartcomm`'s transaction was already bound to at enqueue time (owned, still in scope, never moved), and `addr` (the only new information the claim loop actually contributes) is still passed as the explicit source-address argument, not read from `binding` itself. An earlier draft of this same fix also widened `tester_present_data`'s own recomposition (Decision 6 below) to `binding.resolved()`, on the assumption it followed the identical binding — caught before landing by reading `resolve_tester_present`'s own call site directly: `rpc_primitive.rs` binds it to `&bound_active` unconditionally, per ADR-067 claim 8 ("periodic tester-present is always resolved from the call-time Active snapshot, never Working... a persistent product of this COP that outlives the transient init transaction"). `tester_present_data`'s recomposition (Decision 6) is therefore UNCHANGED by this correction — `link.active` remains correct there.

**4. A fresh claim attempt is now the cancel owner of any address this CLL itself previously claimed (round 4).** `handle_start_comm`'s "fresh attempt" reset block (entered on every `CoptStartcomm` that requests a claim, including a `StopComm` → `CP_J1939PreferredAddress` change → `StartComm` cycle) previously reset only the local `j1939_claim_cursor`/`j1939_claimed_address` markers — leaving any address this CLL had already claimed still registered in `SharedChannel::j1939_claims` and still defended by the adapter indefinitely, since none of the three cancel owners the invariant above enumerates (teardown's cancellation pass, an explicit `Lost`, or a timed-out attempt owned by Decision 2) ever run for this cycle. The reset block now first calls `cancel_j1939_claims_for_cll` — the same helper the disconnect/destroy teardown paths use — under an ADR-080-conformant `shared_channels`-held section (nested `api` for the native cancel, released before the `logical_links` acquisition that resets the local markers), cancelling and deregistering every address this CLL currently owns on the channel before re-entering the claim loop from `cursor = 0`. `cancel_j1939_claims_for_cll` queries `SharedChannel::j1939_claims` directly (the source of truth), so this is correct even were the local `j1939_claimed_address` marker itself somehow already stale, and is a no-op (its own early-return on an empty owned list) on a CLL's very first `StartComm`, when it owns nothing yet. This extends the invariant above with a fourth cancel owner: **a fresh claim attempt is the cancel owner of every address this CLL itself previously claimed** — the one case the original three-owner enumeration did not cover.

**5. A spontaneous `_LOST` is now the cancel owner of the address it reports (round 5).** `deliver_j1939_claim_indication`'s `spontaneous_loss` branch — reached when a claim/defend indication for a CLL's CURRENTLY claimed address arrives with no `run_j1939_claim_loop` wait in flight for it (ADR-179 Decision 3's "later, spontaneous `_LOST`" case) — clears the local `j1939_claimed_address`/`j1939_claim_cursor` markers and arms `SharedChannel::j1939_reclaim_pending`, but previously left the lost address's own entry in `SharedChannel::j1939_claims` untouched. This differs from the in-flight-wait `Lost` case (`run_j1939_claim_loop`'s own post-wait cleanup already removes that entry unconditionally) precisely because no such cleanup runs for a spontaneous loss — the fresh reclaim `j1939_reclaim_pending` arms re-enters the loop from `cursor = 0`, and if that lands on a DIFFERENT (earlier) candidate than the one just lost, the lost address's entry is never revisited. Now removed in the same `shared_channels`-held section that arms the reclaim, immediately verified live via the same snapshot-comparison recheck (`sc.j1939_claims.get(&address) != claims.get(&address)`) this function already performs before any write. No native cancel needed — this is itself an explicit `_LOST` (clause 16.4.6), so the device has already relinquished the address, the same reasoning Decision 2's in-flight-wait handling already applies. This extends the invariant with a fifth cancel owner: **a spontaneous `_LOST` indication is the cancel owner of the address it reports.**

**6. Tester-present framing is now recomposed against the claimed address, mirroring Decision 3 (round 6).** Decision 3 recomposed the optional `CoptStartcomm` message's header on a successful claim, but did not extend the same treatment to `tester_present_data` — resolved at the same `StartComPrimitive` call time (ADR-067), via the same `tx_header::build_tx_message` path (for J1939, `frame_tester_present_data`'s catch-all arm just clones `ResolvedTesterPresent::data` unchanged, since J1939 never uses software ISO-TP), and therefore carrying the identical pre-claim-default staleness. Two sites now recompose it the same way Decision 3 already established: `handle_start_comm`'s J1939 `Claimed` arm recomposes the local `tester_present_data` variable before it is cached into `TesterPresentState::Armed::framed_data`; `run_j1939_reclaim_duties`'s own `Claimed` arm recomposes a LIVE `Armed` state's already-cached `framed_data` in place (computing the fresh header before taking the mutable borrow, to avoid overlapping it with the immutable borrow of `link.active` the header composition itself needs). Both reuse the same length-gated 5-byte-prefix-recompose-keep-payload slicing Decision 3 uses, and both are a no-op when the cached data is empty or the CLL is not currently `Armed`.

**7. `CoptUpdateparam` now re-validates `CP_J1939TargetAddress` synchronously at call time, mirroring `CoptStartcomm` (round 7).** Round 6's mechanical fix gave `CoptStartcomm` a synchronous, call-time check rejecting an out-of-range (>0xFF) or sentinel (0xFFFF, unconditionally — `CoptStartcomm` never runs on an already-started CLL by construction) `CP_J1939TargetAddress` before it could reach `handle_start_comm`. `CoptUpdateparam` had no equivalent: since it only ever adjusts an already-connected CLL's Working ComParams, a client could push `CP_J1939TargetAddress` back to an invalid value after a successful claim with no rejection at all — the value simply sat unused (nothing in the running claim/defend state machine re-reads it once claimed) until the next `StopComm`/`StartComm` cycle re-triggered `handle_start_comm`'s own consumption of it, at which point the earlier-established gap (this ADR's whole subject) would have applied to a value that was never validated in the first place. `rpc_primitive.rs`'s `CoptUpdateparam` handler now runs the same one-byte-range check `CoptStartcomm`'s does (an out-of-range value other than the `0xFFFF` sentinel itself is never valid, `comm_started` or not), guarded behind `resources::is_j1939_protocol_id(hw_protocol_id)` and returning `Status::invalid_argument` synchronously rather than deferring to `handle_update_param`, mirroring the pre-existing FD-mode guard's own "checked at call time, not inside the handler" rationale (`hw_protocol_id` cannot change without a disconnect/reconnect, which bumps `connect_generation` (ADR-086) and is independently caught by `handle_update_param`'s own U1 staleness check). **The `0xFFFF` sentinel check, unlike `CoptStartcomm`'s, is gated on `link.comm_started`** — an initial version of this fix rejected the sentinel unconditionally on protocol ID alone, which incorrectly blocked an ordinary `CoptUpdateparam` staging an unrelated ComParam on a connected-but-not-yet-started CLL (where `0xFFFF` is still `CP_J1939TargetAddress`'s own legitimate, documented default), caught by an `edge-case-hunter` verification pass on this round's own diff before it shipped. `update_param_working_snapshot`'s return tuple now also carries `comm_started` (read under the same single `logical_links` acquisition as the rest of the snapshot) so this check has it available without a second lock acquisition.

**8. A `CP_J1939PreferredAddress` claim candidate of 254 or 255 is now skipped without issuing a native claim (round 7).** ADR-179 Decision 3 already documented that neither 254 (the null address) nor 255 (the global/broadcast address) is ever a valid explicit claim target per clause 16.3.3.2, but `run_j1939_claim_loop` never enforced this itself — a candidate list containing either value would still reach the ADR-080-conformant critical section (Decision 1) and issue a native `protect_j1939_addr` call for it, relying entirely on the adapter to reject it (which it does, per the mock's own `IOCTL_PROTECT_J1939_ADDR` handler, returning `ERR_INVALID_IOCTL_VALUE` with no observable side effect). The loop now checks for 254/255 immediately after `j1939_claim_cursor_step` resolves the next candidate address, before entering the critical section — logging and advancing the cursor to the next candidate (or returning `Stale` if the CLL went stale in the meantime, via the same `advance_j1939_claim_cursor` path the rest of the loop already uses) rather than issuing a claim the native side would reject anyway. This is a defense-in-depth/no-wasted-round-trip fix, not a reachable claim-success bug — the native rejection already prevented these addresses from ever being successfully claimed.

**9. A failed/cancelled StartComm after a successful claim is now the cancel owner of that claim (round 8, `design-advisor` consult).** A claim issued by a `PDU_COPT_STARTCOMM` COP whose handler terminates on any path that never reaches the COMM_STARTED write-back — cancellation of the optional CoptStartcomm message's P3 wait or receive phase, a transmit failure (`TxFailure::Event`), or a transmit cancellation (`TxFailure::Cancelled`) — is now cancelled by that same handler at the terminating early return, via `cancel_j1939_claims_for_cll` plus a generation-gated clear of `j1939_claimed_address`, through a new `cancel_j1939_claim_after_failed_startcomm` helper (`events_j1939_claim.rs`) that mirrors Decision 4's own critical-section shape (`shared_channels` first, nested `api` for the native cancel, released before the `logical_links` acquisition that clears the local marker). A `CoptStartcomm` therefore either completes to COMM_STARTED with the claimed address live and defended, or terminates holding no claim at all — the same post-state a claim-loop failure (`Exhausted`) already has. `handle_start_comm` gates every call site on a new local `j1939_claimed_this_cop` flag (`true` only once `run_j1939_claim_loop` itself returns `Claimed` for THIS COP), not on live `j1939_claimed_address` state: a StopComm→re-StartComm cycle (Decision 4) or a fresh StartComm that does not request a claim at all can both reach this same shared optional-message transmit sequence while `j1939_claimed_address` is still set from an EARLIER, unrelated COP — cancelling that address here would incorrectly steal Decision 1/4's own ownership of it. Two termination causes reachable from the same transmit sequence retain their PRE-EXISTING owners and are deliberately NOT cancelled by this Decision: loss of `still_on_this_channel` (Decision 1's teardown pass already runs in the same disconnect/destroy handler that caused the staleness) and a hard channel error (`handle_channel_hard_error` marks the `SharedChannel` dead and unjoinable, and intentionally retains `channel_key` so the CLL's eventual teardown still runs Decision 1's cancellation pass). At the one site where a live cancellation and an already-owned stale/hard-error cause are indistinguishable from the code's own vantage point (`ReceivePhaseOutcome::Terminal`, which folds several causes into one variant), the handler calls the new helper unconditionally — in the already-owned sub-cases this is an idempotent no-op (`cancel_j1939_claims_for_cll`'s own early-return on an empty owned list) or a best-effort native cancel against a channel that is already gone, the same "wasteful, not unsafe" tradeoff this exact arm already accepts for its own unconditional Temp-binding-revert call. **Accepted residual:** the `NODE_ADDRESS` Working/Active write-back performed when the claim first succeeded is NOT reverted by this cancellation (matching Decision 4's own precedent) — a `CoptSendrecv` issued on the still-not-`comm_started` CLL after this cancellation frames with a now-undefended source address until the next claim-requesting `CoptStartcomm` overwrites it.

**10. A relinquished claimed address now stops the CLL's own live SAE J2534-2
clause 14 repeat slots (round 9, `design-advisor` consult).** A live repeat
slot's device-side frozen `RepeatMsgData[0]` (ADR-165 Decision 3) carries
the SAE J1939 source address that was claimed when `START` ran; no setup is
cached service-side, so nothing previously refreshed or stopped it when
that claim was later lost or superseded. Per SAE J1939-81's own requirement
that a node cease transmitting under an address it no longer holds, the
violation begins at the moment of relinquishment, not once/if a reclaim
eventually succeeds (which can take up to
`candidate_count * CP_J1939AddrClaimTimeout`, or never, on `Exhausted`) —
so the fix is anchored at relinquishment, not at reclaim completion. A new
`stop_repeat_slots_for_cll` helper (`events_j1939_claim.rs`) best-effort-
stops every MsgId in the CLL's `LogicalLinkState::repeat_message_ids` via
`api.stop_repeat_message`, pruning on `Ok` or `ERR_INVALID_MSG_ID` exactly
per `rpc_misc.rs::ioctl_stop_repeat_message`'s own convention, and
warn-and-retain on any other error — the same best-effort treatment
`cancel_j1939_claims_for_cll` already gives a failed native call. Called
from THREE relinquishment sites — not two, corrected by an `edge-case-hunter`
verification pass on this Decision's own diff, in the same round 9: initially
wired into only `deliver_j1939_claim_indication`'s spontaneous-loss branch
(Decision 5's site — the PRIMARY case, since a spontaneous loss has no
client-driven trigger of its own to hang a different fix on) and
`handle_start_comm`'s fresh-attempt reset (Decision 4's site — a `StopComm`
→ preferred-address change → `StartComm` cycle voluntarily relinquishes the
old address the same way), the initial version reasoned by analogy that
`cancel_j1939_claims_for_cll`'s own Decision 2/9 call sites (reached via the
new `cancel_j1939_claim_after_failed_startcomm` helper) must NOT stop slots
either, on the theory that "no slot can bear a never-completed claim's
address." That reasoning is correct for Decision 2 (a claim attempt that
timed out — nothing was ever claimed, so no slot could have been started
under it) but WRONG for Decision 9: Decision 9's entire premise is a claim
that DID complete (`j1939_claimed_this_cop`, its caller's own gate, is only
ever `true` once `run_j1939_claim_loop` returned `Claimed`), cancelled later
because the optional CoptStartcomm message's own transmit sequence failed or
was cancelled — and `PDU_IOCTL_START_REPEAT_MESSAGE` requires only a
connected channel, not `comm_started`, so a client can legitimately start a
repeat slot under that freshly-claimed address while the optional message is
still in flight. `cancel_j1939_claim_after_failed_startcomm` (Decision 9's
own helper, all four of its call sites in `handle_start_comm`) now also
calls `stop_repeat_slots_for_cll`, right after clearing
`j1939_claimed_address`, closing this third site. All three call sites are
unconditional and idempotent (`stop_repeat_slots_for_cll` is itself a no-op
when the CLL owns no repeat slots), the same "harmless when nothing is
owned" shape Decision 4's own `cancel_j1939_claims_for_cll` call already
has. Deliberately a DIFFERENT helper from `cancel_j1939_claims_for_cll`,
not folded into it: a different native primitive and a different piece of
bookkeeping (`repeat_message_ids`, not `SharedChannel::j1939_claims`) — a
Decision-2-only exclusion, not a blanket one; Decision 2's own call site
(the claim-timeout case) remains correctly excluded, since no slot can bear
a never-completed claim's address (an accepted residual below covers the
one narrow remaining exception). No new client-visible
signal was needed: ADR-165's own established contract already covers this
outcome — `repeat_message_ids` membership is a claim, not a fact, and slots
already vanish device-side with no notification, so a client's next
`QUERY`/`STOP` on a stopped MsgId observes exactly the `ERR_INVALID_MSG_ID`
outcome that contract already documents. **Rejected alternative:**
caching each slot's setup and re-issuing it under the newly-claimed address
— rejected as contradicting ADR-165's own thin-forwarder decision (this
service does not own repeat-slot state), and a native re-START mints a new
device-assigned MsgId regardless, so the client's original handle would
dangle either way without an old→new mapping layer this ADR does not
introduce. **Accepted residual:** a `START` landing inside one of the three
call sites' own internal await windows — between the address actually being
relinquished (the native cancel, or the device's own `_LOST`) and this
Decision's `stop_repeat_slots_for_cll` call later in the same sequence — is
not covered; such a slot survives until its own next relinquishment event.
The same class of narrow, infeasible-to-construct-deterministically race
window this ADR's Decisions 1/2 already accept.

**11. A relinquished claimed address now cancels the CLL's own live,
transmitting `CoptSendrecv` COPs (round 10, `design-advisor` consult).**
`events.rs::handle_send_recv`'s own doc comment establishes that every cycle
of a multi-cycle/cyclic `CoptSendrecv` (a re-enqueued `CycleContinuation`)
retransmits the SAME `SendRecvTx::data` resolved once at `StartComPrimitive`
call time (ADR-067) — for J1939, already framed with the source address live
at that moment — and nothing ever recomposes or invalidates it later. A new
`events_j1939_claim.rs::cancel_send_recv_cops_for_cll` helper marks every
live `PDU_COPT_SENDRECV` COP this CLL owns that actually transmits
(`CopEntry::is_send_recv && CopEntry::transmits` — a new `is_send_recv` field
on `CopEntry`, set alongside `transmits` at `StartComPrimitive` call time) as
cancelled, via the same `LogicalLinkState::cancelled_cops` mark-and-defer
mechanism `rpc_primitive::rpc_cancel_com_primitive` already uses for an
explicit client cancel — already handled correctly at every point a
`TxItem::SendRecv` can be (still queued, a parked continuation, or the
in-flight cycle's own drain check), so no new emission logic is needed.
Called unconditionally (idempotent no-op when the CLL owns no live SendRecv
COPs) from the same three relinquishment sites Decision 10 already
enumerates: `deliver_j1939_claim_indication`'s spontaneous-loss branch (the
PRIMARY case), `handle_start_comm`'s fresh-attempt reset, and
`cancel_j1939_claim_after_failed_startcomm` (always a no-op at this third
site today, since J1939 `CoptSendrecv` requires `comm_started`, which is
never true yet when this helper runs — kept for consistency with the other
two sites and as a defensive no-regression measure).

`transmits` excludes a receive-only (`NumSendCycles == 0`, ADR-059) monitor,
which never puts a frame on the bus and has nothing to invalidate;
`is_send_recv` excludes `CoptStartcomm` (cancelling it would cancel the very
reclaim attempt driving this call) and a terminal-send `CoptStopcomm` (would
recreate ADR-085's `stop_comm_pending` deadlock) — both of which also set
`transmits`.

**Round-10 `edge-case-hunter` correction to this filter's own static
snapshot.** `CopEntry::transmits`/`is_send_recv` are computed once, at
`StartComPrimitive` call time, and never updated afterward. An IS-CYCLIC-
shaped `CoptSendrecv` (a finite send phase, `NumReceiveCycles == -1`) whose
send phase has since finished and detached to ADR-100 tier 2
(`ReceivePhaseOutcome::DetachedToTier2`, a pure receive-only registrant,
`RegistrantTier::ReceiveOnly`) still reads `transmits == true`/`is_send_recv
== true` forever under that stale snapshot, even though it no longer puts
anything on the bus and so has nothing an address change could make stale.
`cancel_send_recv_cops_for_cll` now also reads `LogicalLinkState::
registrants` (under the same `logical_links` lock it already holds) and
excludes any `cop_handle` with a live tier-2 registrant on this CLL before
extending `cancelled_cops` — the same exclusion `rpc_misc.rs::
ioctl_clear_tx_queue`'s own ADR-100 S5 companion fix already applies to its
own TX-queue clearing. New regression test:
`tests/grpc_mock/j1939.rs::spontaneous_loss_does_not_cancel_a_detached_tier2_registrant_under_the_lost_address`.

**Rejected alternative: recompose `SendRecvTx::data` in place, mirroring
Decisions 3/6.** Two independent disqualifiers, beyond the general tension
with ADR-067's "every cycle uses the SAME call-time-bound snapshot"
principle: (1) recompose cannot fix the primary violation window — on a
spontaneous loss, Active `NODE_ADDRESS` itself *retains the lost address*
until a reclaim completes (Decision 5 clears only the local
`j1939_claimed_address`/cursor markers, and Decision 9's own accepted
residual documents the write-back is never reverted), so "recompose from
live Active each cycle" would keep reproducing the identical stale byte for
the entire loss-to-reclaim window (up to `candidate_count *
CP_J1939AddrClaimTimeout`, or forever on `Exhausted`) — Decision 10's own
anchoring rule (the SAE J1939-81 violation begins at relinquishment, not at
reclaim) is the governing rule here, and recompose fails it; (2) a generic
every-cycle re-read of live Active would also silently override a Temp
binding's `effective` snapshot for every OTHER header ComParam
(priority/DP/PF/PS), a genuine ADR-067 contract break for client-bound
values that Decisions 3/6's own narrower, claim-machinery-owned-frame
recomposition never risked. **Accepted residual:** a `CoptSendrecv` whose
framing is resolved (at RPC call time, `tx_header::build_tx_message`) INSIDE
a claim/reclaim window — queued after the relinquishment site already ran,
or while a claim-requesting `StartComm` is mid-loop — still bakes in a
stale/pre-claim address and is not covered by this cancellation; the native
`ERR_ADDRESS_NOT_CLAIMED` backstop (ADR-179 Decision 4) catches >8-byte
payloads, but shorter payloads transmit silently wrong. The same class of
narrow, infeasible-to-construct-deterministically window this ADR's
Decisions 1/2/10 already accept.

**12. `CoptUpdateparam` now rejects promoting `CP_TesterSourceAddress` off
the address a live J1939 claim owns (round 10, `design-advisor` consult).**
`CP_TesterSourceAddress` (native `NODE_ADDRESS`) is negotiation-owned once a
claim or claim attempt is registered for this CLL — ADR-179 Decision 3's
claim loop writes the successfully claimed address into it on both Working
and Active, and every subsequent `tx_header::j1939_header_bytes` read
(`CoptSendrecv` framing, tester-present re-arm) reads Active directly.
Nothing previously stopped `CoptUpdateparam` from promoting a DIFFERENT
staged value — moving Active out from under the claim machinery, whose own
bookkeeping (`LogicalLinkState::j1939_claimed_address`,
`SharedChannel::j1939_claims`) and the adapter's own native defense of the
ORIGINAL address all stay unaware of the change.

`rpc_primitive.rs`'s existing J1939-scoped `CoptUpdateparam` block (Decision
7's own site) now also rejects when the staged Working `NODE_ADDRESS`
differs from the live Active value AND `SharedChannel::j1939_claims`
registers an entry for this exact `(cll_handle, connect_generation)`.
`update_param_working_snapshot`'s return tuple gains the Active
`NODE_ADDRESS` value and `connect_generation` (the same single-acquisition
extension Decision 7 made for `comm_started`); the `j1939_claims` membership
check is a separate, sequential `shared_channels` acquisition mirroring the
existing `CP_AnalogSampleRate` guard in the same function.

Three deliberate gating choices, each closing a naive-fix trap: (a) gated on
live `j1939_claims` *registration*, not `CP_J1939AddressNegotiationRule`'s
bit 1 — a client could flip negotiation off post-claim while the claim
itself is still live and defended, which rule-gating alone would miss; a
non-negotiated CLL never registers an entry, so `NODE_ADDRESS` stays fully
client-owned for it (round 8's own "made client-writable" change), matching
that precedent; (b) gated on live registration, not `comm_started` alone —
an `UpdateParam` landing while a claim ATTEMPT is still in flight (before
`comm_started` is set) would otherwise slip through and get clobbered once
the attempt's own write-back runs; `j1939_claims` membership covers both the
live-claim and in-flight-attempt cases with one condition; (c) compared
against Active, not `j1939_claimed_address` — an unrelated restage that
leaves `CP_TesterSourceAddress` unchanged (e.g. re-staging the full ComParam
set after a successful claim) must pass, the same differs-from-applied
shape the `CP_AnalogSampleRate` guard already uses.

**Rejected alternative: treat the promotion as a request to switch the
claim to the new address** (cancel the old claim, issue a new one) —
rejected: ADR-179 Decision 3 routes claiming exclusively through
`PDU_COPT_STARTCOMM`'s bounded async wait on the channel poll task, and this
ADR's Decisions 1/2/4/5/9 already enumerate a closed set of cancel owners;
an `UpdateParam`-triggered cancel-and-reclaim would mint a sixth owner class
and a second concurrent entry point into `run_j1939_claim_loop`, re-opening
exactly the registration/cancel-ownership races this ADR spent nine rounds
closing. The supported path for changing address already exists and is
tested (Decision 4): `CoptStopcomm` → restage `CP_J1939PreferredAddress` →
`CoptStartcomm`. The rejection message points clients there. **Accepted
residual:** an `UpdateParam` validated (this guard's own snapshot taken)
BEFORE a fresh attempt's own Decision-1 registration, but promoted after
that attempt's write-back runs, is not covered — the same call-time-vs-
execution-time window Decision 7's own `comm_started` gate already tacitly
accepts.

**13. `PDU_IOCTL_START_REPEAT_MESSAGE` now rejects starting a repeat slot on
a negotiation-enabled J1939 CLL with no claimed address, AND every claim
outcome (success or exhaustion) sweeps any slot a client raced into
existence during the attempt's own pending window (round 11, `design-advisor`
consult).** SAE J2534-2 clause 14's device-autonomous retransmission model
freezes a repeat slot's source-address byte at `START` time
(`tx_header::build_tx_message`'s J1939 arm reads Active `NODE_ADDRESS`, only
meaningful once a claim has resolved) and never revisits it — but
`ioctl_start_repeat_message` never checked J1939 claim state at all, so a
`START` during ANY unclaimed window (before this CLL's first successful
claim, during a pending reclaim after a spontaneous loss, after a claim
attempt's candidate list is exhausted) created a permanent slot
transmitting under a source address this CLL does not own (the `0xF1`
pre-claim default, or a just-relinquished address). This is the same
recurring "framed/cached J1939 data goes stale when the claimed address
changes" bug class Decisions 10/11 already closed for repeat slots'
lifecycle and ordinary `CoptSendrecv`'s live cycles respectively — this
Decision closes the remaining gap: a slot's own *creation*, not just its
continued transmission after an address it already held changes.

Neither of Codex's own two proposed remediations alone closes the gap —
both are needed together:

- **A START-time gate.** `ioctl_start_repeat_message` (inside its existing
  `shared_channels`-then-`logical_links` critical section, ADR-080-
  conformant) now rejects with `PDU_ERR_CLL_NOT_STARTED`/`FailedPrecondition`
  when the CLL is J1939, `j1939_claim_requested` (`CP_J1939AddressNegotiationRule`
  bit 1 clear) says negotiation is enabled, and `j1939_claimed_address` is
  `None`. Scoped to negotiation-ENABLED CLLs only: a non-negotiated CLL (bit
  1 set) never runs the claim loop at all and `j1939_claimed_address` stays
  `None` for its entire life by design, with `CP_TesterSourceAddress`
  client-managed instead (the same distinction Decision 12's own guard
  already draws) — gating on `j1939_claimed_address` alone would
  permanently and wrongly block Repeat Messaging on every such CLL. This is
  a genuine client-visible behavior change: a `START` on a negotiation-
  enabled J1939 CLL that never successfully claimed an address previously
  succeeded silently (composing under the `0xF1` default); it is now
  rejected, matching SAE J1939's own claim-before-transmit precondition
  (clause 16 — the same precondition the native `ERR_ADDRESS_NOT_CLAIMED`
  return already surfaces for other J1939 sends, ADR-179 Decision 4's own
  context).
- **A terminal-outcome sweep, at all four `Claimed`/`Exhausted` arms**
  (`events.rs::handle_start_comm`'s own two arms, `events_j1939_claim.rs::
  run_j1939_reclaim_duties`'s own two arms) — the gate alone cannot close a
  check-then-act race: a spontaneous loss can land after a `START` already
  passed the gate (claim was live) but before that `START`'s own native call
  and `repeat_message_ids` registration complete. Each arm now acquires
  `shared_channels` BEFORE its existing `logical_links` writeback and holds
  it across a `stop_repeat_slots_for_cll` call made immediately after (only
  when the writeback/staleness-check itself confirms the CLL is still live
  on this channel/generation) — serializing against
  `ioctl_start_repeat_message`'s own pre-existing `shared_channels` hold,
  which already spans its ENTIRE check-native-call-register sequence
  (Codex-review Finding, ADR-165 PR #42 round 5, Bug 1). With both sides
  serialized on the same lock, a racing `START` either fully completes
  (registers its new MsgId in `repeat_message_ids`) before a sweep runs —
  caught and stopped by that sweep — or waits until after the writeback
  releases the lock, at which point the gate (re-checked fresh) sees the
  correct post-outcome claim state (a freshly-claimed address, or none —
  rejected) and never creates a slot under a stale one. The `Exhausted` arms
  sweep too, not just `Claimed`: an exhausted attempt leaves the CLL with NO
  claimed address at all, so a slot raced in during that attempt's own
  window is exactly as stale.

**Rejected alternatives:**
- **The START-time gate alone** — rejected: closes the "before any claim"
  and "after exhaustion" windows, since nothing ever writes back an address
  to satisfy it, but not the narrower during-claim race the finding
  actually reported (a `START` that legitimately passed the gate while a
  claim was live, racing a spontaneous loss).
- **The terminal-outcome sweep alone** — rejected: a `START` after
  `Exhausted`, or before this CLL's very first claim ever completes, has no
  later claim-success writeback to ever trigger a sweep, so a slot created
  in either window would be permanent, not merely delayed.
- **Recomposing the caught slot's source-address byte in place, instead of
  stopping it** — rejected, the same "stop, don't recompose" choice
  Decision 10 already made for this exact mechanism: `LogicalLinkState::
  repeat_message_ids` is a bare list of native `MsgId`s, not the original
  `REPEAT_MSG_SETUP` needed to reissue a slot under a new address — there is
  no data to recompose from, only to discard and let the client re-`START`
  if it still wants the slot. ADR-165's own established contract already
  covers the client-visible consequence (`repeat_message_ids` membership is
  a claim, not a fact; a slot can already vanish device-side with no
  notification).
- **Sweeping on `Stale`/`HardError` too** — rejected: `Stale` means a
  disconnect/reconnect (ADR-086) already ran during this exact claim
  attempt, so the CLL's CURRENT `repeat_message_ids` may belong to an
  entirely different `connect_generation` than the one this attempt was
  for — sweeping here risks stopping a legitimate, unrelated slot the NEW
  connection owns. Both cases are already covered by their own existing
  teardown paths (disconnect/destroy's own best-effort repeat-slot cleanup;
  `handle_channel_hard_error`'s own full-channel teardown), which correctly
  scope to the CLL's actual current state rather than the stale attempt's.

**Accepted residuals:** (a) a `START` that slips past the gate (claim was
live at check time) and whose claim then ends `Stale`/`HardError` (rather
than `Claimed`/`Exhausted`) is not swept by this Decision — those outcomes
route through disconnect/hard-error teardown instead (see the rejected
alternative just above), which is correct for THEIR own state but does not
retroactively sweep a slot registered under the now-superseded attempt; the
same narrow, infeasible-to-construct-deterministically race-window class
this ADR's Decisions 1/2/10 already accept. (b) A slot that slips past the
gate transmits under the about-to-be-lost address for the remainder of that
claim attempt's own duration before the terminal sweep runs — the same
transient exposure window Decision 10 already documents for the
loss-to-stop interval on a pre-existing slot, now also covered for a
freshly-created one.

**Round 12 (after Decision 13 was committed): the recurring bug class
Decisions 4/6/9/12/13 already close for other mechanisms hit its SEVENTH
instance, in three more places at once (Codex review, `design-advisor`
consult).** Whenever a J1939 CLL's claimed source address changes (or has
not yet resolved), data framed or transmitted against the OLD/unclaimed
address must be caught somewhere — this round's three findings are Decisions
14, 15, and 16 below. Decision 13 stays the one documented exception to a
single shared "gate + re-check" shape across this whole bug class: SAE
J2534-2 clause 14 Repeat Messaging is device-autonomous once started (no
software choke point exists to re-check a live slot's own frozen bytes
against a later address change, only its *creation*), so Decision 13's own
mechanism (a `START`-time gate plus a terminal-outcome sweep) is
structurally different from Decisions 14/15/16 below, which each have a
genuine software choke point to re-check at.

**14. A transmitting `CoptSendrecv` is now rejected at `StartComPrimitive`
time on a negotiation-enabled J1939 CLL with no claimed address, AND every
cycle's own dispatch re-verifies the resolved source address against the
CLL's CURRENT claim before writing (Finding 1).** `rpc_start_com_primitive`
never checked J1939 claim state for `CoptSendrecv` at all — a transmitting
send (`NumSendCycles != 0`; a receive-only monitor, ADR-059, puts nothing on
the bus and has nothing to gate) on a negotiation-enabled CLL with no
claimed address yet composed and transmitted under the pre-claim `0xF1`
default. This is the same "before any claim / during a pending reclaim /
after exhaustion" window Decision 13 already closes for Repeat Messaging's
own `START`, now closed for `CoptSendrecv` too. Two parts, both required —
the gate alone has a TOCTOU the transmit-time check closes:

- **A `StartComPrimitive`-time gate.** `rpc_start_com_primitive` now rejects,
  synchronously, before `cop_handle` allocation (mirroring every other
  synchronous call-time rejection in this function, e.g. the SAE J2534-2
  clause 10 Analog Input write-rejection immediately above it), when the new
  shared `j1939_negotiated_unclaimed` predicate (below) is `true` for this
  CLL and this `CoptSendrecv` actually transmits. `j1939_negotiated_unclaimed`
  is factored out of Decision 13's own inline three-way `AND`
  (`resources::is_j1939_protocol_id` && `j1939_claim_requested` &&
  `j1939_claimed_address.is_none()`) into `events_j1939_claim.rs`, and
  Decision 13's own gate is refactored (behavior-neutral) to call it too —
  this Decision and Decision 16 below both need the identical predicate.
  **Round-12 `edge-case-hunter` correction (same round, found after this
  Decision had already landed):** the gate originally read this CLL's
  Active `CP_J1939AddressNegotiationRule` unconditionally, even for a
  `temp_param_update = 1` `CoptSendrecv`, whose own resolution below binds
  against Working (`effective`) instead (ADR-067) — so the gate and
  `resolve_send_recv_tx` could disagree about whether this CLL even counts
  as "negotiated" for one Temp-bound send, either wrongly rejecting a
  Temp-bound send whose Working snapshot would never have needed a claim,
  or wrongly passing one whose Working snapshot IS negotiated-and-unclaimed
  while Active is not — only for the transmit-time re-check below to then
  cancel it on its very first cycle. Fixed with
  `j1939_negotiated_unclaimed_for(link, params)` (reads an explicit
  `ComParamSet` instead of always `link.active`); the gate now passes it
  Working when `temp_param_update`/`temp_eligible` apply for this call,
  mirroring the exact Temp-vs-Active choice `binding`'s own construction
  below already makes.
- **A transmit-time re-check, at EVERY cycle.** The gate alone has a
  TOCTOU: a claim can be spontaneously lost, or a multi-cycle/cyclic send
  can still be queued from before this CLL's first claim resolved, between
  `StartComPrimitive` accepting the COP and any given cycle's actual
  dispatch. `rpc_primitive.rs::resolve_send_recv_tx` now surfaces the exact
  J1939 source-address byte it resolved the frame's header with
  (`ResolvedSendRecvTx::j1939_tx_source`) — `Some` only for a
  negotiation-ENABLED J1939 CLL (`events::j1939_claim_requested`), `None`
  for every other protocol AND for a non-negotiated J1939 CLL alike:
  `j1939_claimed_address` stays permanently `None` for a non-negotiated CLL
  by design (client-managed `NODE_ADDRESS`, Decision 12's own distinction),
  so comparing against it would spuriously "detect drift" and cancel EVERY
  send on such a CLL, not just a genuinely stale one — caught by this
  Decision's own regression tests (`unclaimed_source_address_over_8_bytes_
  fails_the_send`, a pre-existing non-negotiated-CLL test, started failing
  against an early draft that scoped `j1939_tx_source` on
  `is_j1939_protocol_id` alone). Threaded through
  `SendRecvTx::j1939_tx_source` into the queued `TxItem::SendRecv`, copied
  unchanged into every follow-up cycle's own re-enqueued `SendRecvTx`
  (`handle_send_recv`'s existing cyclic-continuation shape, ADR-053).
  `events.rs::handle_send_recv` re-checks it against the CLL's CURRENT
  `j1939_claimed_address` immediately before each cycle's own
  `transmit_request` call (alongside the pre-existing `still_on_this_channel`
  staleness check at that exact point) — on a mismatch, the cycle is routed
  through the SAME local `cancelled` flag/terminal-`PduCopstCancelled` path
  the pre-existing P3-gap-cancellation and `TxFailure::Cancelled` arms
  already use, not a new terminal-status shape. This runs on every cycle,
  not just the first, since `handle_send_recv` (and thus this check) is
  freshly re-entered via `dispatch_tx_item` for every cyclic follow-up —
  there is only ever one `transmit_request` call site per cycle in this
  function to guard.

**Rejected alternatives:**
- **Recomposing the frame in place, instead of cancelling** — rejected, the
  same "cancel, don't recompose" choice Decision 11 already made for this
  exact mechanism: Active `NODE_ADDRESS` still holds the LOST address for
  the whole loss-to-reclaim window (Decision 9's own accepted residual
  documents the write-back is never reverted on a failed/cancelled
  StartComm either), so "recompose from live Active each cycle" would keep
  reproducing the same stale byte for the whole window. It would also
  silently override a Temp-bound COP's `effective` snapshot for every other
  header field (priority/DP/PS/target address), an ADR-067 contract break
  for client-bound values — the same reasoning `cancel_send_recv_cops_for_cll`'s
  own doc comment (Decision 11) already documents.

**Accepted residual:** no dedicated regression test exists for the
transmit-time re-check's own outcome-arm race (a claim spontaneously lost,
or still pending, in the narrow window between `wait_for_p3_gap` returning
`Ready` and `transmit_request` actually writing) — deterministically racing
a live claim wait against a queued cyclic send's own per-cycle dispatch is
the same class of ~10ms-internal-poll-window race `tests/grpc_mock/j1939.rs`'s
own module doc already documents as unobservable/uncontrollable from a pure
RPC round trip (see that file's own "Item 2" note), the same accepted-residual
shape Decision 13's own outcome-arm race already uses. The gate's own effect
(Part A) IS independently testable and deterministic — see Consequences.

**15. `CoptUpdateparam`'s promotion of `CP_TesterSourceAddress` off a live
claim now re-checks Decision 12's own guard a second time, authoritatively,
at execution time (Finding 2).** Decision 12 rejects, synchronously, at
`StartComPrimitive` call time, a `CoptUpdateparam` whose staged
`CP_TesterSourceAddress` differs from the CLL's Active value while a claim
(or claim attempt) is registered in `SharedChannel::j1939_claims` — but that
snapshot is taken once, at enqueue time. A `CoptStartcomm` queued AHEAD of
an already-queued `CoptUpdateparam` (e.g. both enqueued back-to-back before
the poll task processes either) can register this exact `(cll_handle,
connect_generation)`'s claim, and promote Active `NODE_ADDRESS` to it, AFTER
Decision 12's own snapshot already passed the `CoptUpdateparam` through, but
BEFORE that `CoptUpdateparam` itself actually executes — the same
enqueue-to-dispatch TOCTOU shape Finding 1 above closes for `CoptSendrecv`.

Decision 12's own `StartComPrimitive`-time guard is kept unchanged, as a
fast-fail UX convenience (it still rejects the common case immediately,
without a round trip through the poll task). A second, AUTHORITATIVE check
is added to `events.rs::handle_update_param` — the SAME predicate/comparison
Decision 12's own guard uses (staged `CP_TesterSourceAddress` differs from
the LIVE Active value, AND `SharedChannel::j1939_claims` contains an entry
for this exact `(cll_handle, connect_generation)`) — run immediately before
the Working → Active promotion write, sequentially against `logical_links`
(via the live Active snapshot this function already captures earlier in the
SAME poll-task pass) THEN `shared_channels` (never held simultaneously,
mirroring Decision 12's own "SEPARATE `shared_channels` acquisition, AFTER
`logical_links`'s own guard has already dropped" lock-ordering note). On a
drift, the WHOLE COP is terminated — `send_error_event(PduErrEvtProtErr)`
plus `emit_terminal_if_live(.., PduCopstCancelled)`, mirroring this same
function's own `PduErrEvtProtErr`-then-terminal shape used for its other
already-queued-COP conflict (the `promote_unique_resp_id_table` rejection
just below this check) — `PduComPrimitiveStatus` has no "Failed" variant in
this codebase's vocabulary, only Idle/Executing/Finished/Cancelled/Waiting.

**Rejected alternatives:**
- **A partial-apply/strip-just-`CP_TesterSourceAddress` promotion**
  (ADR-110-style) — rejected: Working already holds this call's full staged
  set, and nothing in `CoptUpdateparam`'s own contract ever reverts just one
  key of it back toward Active — a partial apply would leave a permanent
  Working/Active divergence on `CP_TesterSourceAddress` specifically, while
  every other staged param in the same `CoptUpdateparam` promotes normally,
  a worse and more confusing outcome than terminating the whole COP.

**Round-15 extension (Codex review, PR #72):** the identical enqueue-vs-
execution-time TOCTOU shape exists for a DIFFERENT field this Decision's
own mechanism did not yet cover: `rpc_primitive.rs`'s synchronous
`CoptUpdateparam`-time rejection of `CP_J1939TargetAddress` staying at the
`0xFFFF` "not configured" sentinel is gated on `comm_started &&
j1939_target_address == Some(0xFFFF)`, where `comm_started` is captured at
enqueue time. A `CoptStartcomm` queued ahead of an already-queued
`CoptUpdateparam` staging `0xFFFF` (both enqueued while `comm_started` is
still `false`, so the enqueue-time gate does not reject) can complete
BEFORE that `CoptUpdateparam` actually executes, promoting the sentinel
onto an already-started link unvalidated — later sends silently truncate
it to `0xFF` (an unintended broadcast) via `tx_header::j1939_header_bytes`'s
cast, while `GetComParam` keeps reporting the untruncated `0xFFFF`. Fixed
by extending this Decision's own mechanism to the new field: `handle_update_
param`'s `live_ctx` capture (the same one-critical-section snapshot this
Decision's own `NODE_ADDRESS` re-check already reads `active` from) was
widened to also capture the LIVE `comm_started` bool, and a second,
independent check — adjacent to, not merged with, this Decision's own
`NODE_ADDRESS` re-check (the two fields' checks are unrelated to each
other, just co-located by sharing the same TOCTOU shape and the same
critical section) — rejects the same way (`PduErrEvtProtErr` +
`PduCopstCancelled`) when `is_j1939_protocol_id(..) && live_comm_started &&
params`'s staged `CP_J1939TargetAddress == Some(0xFFFF)`. New regression
test: `tests/grpc_mock/j1939.rs::coptupdateparam_execution_time_check_
catches_j1939_target_address_ffff_after_startcomm`, using a NON-negotiated
`CoptStartcomm` deliberately (a claim-enabled one would also trip this
Decision's own `NODE_ADDRESS` re-check via the claim's own write-back,
confounding the regression-catch for this field specifically). Confirmed
to catch the regression by temporarily short-circuiting the new check and
observing the test fail, then restoring it.

**Round-17 correction (Codex review, PR #72):** both of this Decision's
execution-time re-checks (the original `NODE_ADDRESS` one and the
round-15 `CP_J1939TargetAddress` extension) used to run only inside the
`if all_ok { .. }` arm, AFTER `apply_params_to_hardware_locked` had
already pushed every OTHER staged ComParam in the same `CoptUpdateparam`'s
`hw_set` to the adapter. A rejection at that point terminated the COP
(per this Decision's own "terminate the whole COP" rejection shape) but
left the adapter already holding new values for everything EXCEPT the one
conflicting J1939 field, while Active (and `GetComParam`) kept reporting
the pre-call values for all of it — a partial apply this Decision's own
rejection shape was never supposed to allow (the exact alternative its
own "Rejected alternatives" note above already ruled out, just reached
through the hardware push instead of a deliberate partial promotion).

Fixed by relocating both checks to the TOP of the `Some(Some((j2534_
protocol_id, active, locked_by_other, live_comm_started))) => { .. }` arm
in `handle_update_param`, before `apply_bustype_lock`/`apply_params_to_
hardware_locked` run at all — a rejection now returns before any hardware
write for this COP is ever attempted. `claim_owns_address`'s claim-
ownership conjunct is now fed by `holds_live_j1939_claim`, a SEQUENTIAL
`shared_channels` pre-read taken BEFORE `ctx.api.lock().await` (ADR-080:
`shared_channels` must never nest under `api`/`logical_links`), rather
than a fresh `shared_channels` acquisition at the old, later check site.
This pre-read is still just as authoritative as a read taken at the old
promotion-adjacent site: the poll task serializes this whole function
against every mutator that could flip `SharedChannel::j1939_claims`
dangerously — `run_j1939_claim_loop` (`events_j1939_claim.rs`) is the
SOLE insertion site, driven only by `handle_start_comm`/`run_j1939_
reclaim_duties`, neither of which can run concurrently with this
function on the same poll task. The only OFF-poll-task mutators are RPC
teardown removals (`cancel_j1939_claims_for_cll` from disconnect/
destroy), which bump `connect_generation` and are already caught by this
function's own U1/U2 generation rechecks elsewhere in the same function
— an invariant worth re-verifying with `rg 'j1939_claims\.insert|comm_
started = true' j2534-0404-service/src` if a future change ever adds an
RPC-side (non-poll-task) `j1939_claims` insertion path. `api` is now held
continuously from before this arm begins (the pre-existing ADR-110
Finding-2 invariant), so each rejection branch now calls `drop(api)`
before `send_error_event`/`emit_terminal_if_live` — unlike the old check
site, which ran after `drop(api)` had already released it.

**Behavior change, not just an internal reordering:** the `PDU_ERR_EVT_
RSC_LOCKED` event (`rsc_locked`, computed by `apply_bustype_lock`) is no
longer emitted for a `CoptUpdateparam` this Decision's own checks reject
— the function now returns before `apply_bustype_lock` ever runs, so a
single call that BOTH conflicts with the physical-ComParam lock (ADR-110)
AND drifts off a live J1939 claim (this Decision) now surfaces only the
`PduErrEvtProtErr`/`PduCopstCancelled` this Decision's own rejection
already produces, not a `PduErrEvtRscLocked` as well. Accepted: the whole
COP is terminated either way, and ADR-110's own lock-conflict handling
(the BUSTYPE-key exclusion/substitution `apply_bustype_lock` performs) is
moot for a COP that never reaches the hardware push at all.

**Second behavior change, same relocation (`edge-case-hunter` finding,
round 17): a COP that BOTH trips this Decision's own checks AND would
independently have failed its hardware push (an unrelated `SET_CONFIG`
call in `hw_set` erroring, the `!all_ok` residual (1) above) now always
reports `PduCopstCancelled`, never `PduCopstFinished`.** Before this
correction, these two checks only ran inside `if all_ok { .. }` — a COP
whose hardware push would have failed for an unrelated reason never
reached them at all (since `all_ok` was already `false`), so it took the
generic `!all_ok` path instead, which reports `PduErrEvtProtErr` but
still falls through to the unconditional terminal `PduCopstFinished`
(this codebase's established "the COP still finishes normally" pattern
for a non-fatal adapter error). Now the checks run unconditionally,
before `all_ok` is even computed, so a COP this Decision's own checks
reject is ALWAYS `PduCopstCancelled` regardless of what the hardware push
would otherwise have done — matching this Decision's own pre-existing
"terminate the whole COP" rejection shape (its own "Rejected
alternatives" note above), which already made `PduCopstCancelled`
unconditional whenever these checks tripped; the only change is that more
calls now reach that unconditional shape than before (previously an
unrelated `!all_ok` outcome could "shadow" this Decision's own rejection
by running first). Accepted, for the identical reason Decision 15's
original design already accepted the same unconditional-`Cancelled`
shape: a COP that never reaches the hardware push at all has nothing left
for `PduCopstFinished` to report on.

**Explicitly NOT closed by this correction — two separate, pre-existing
residuals, unrelated to this Decision's own J1939-specific checks:** (1)
the `!all_ok` branch's own adapter-fault case (`apply_params_to_hardware_
locked` returns `false` when at least one SET_CONFIG call in `hw_set`
fails) can itself still leave the adapter holding a partial subset of
`hw_set`'s keys applied and others not, with no revert — an inherent
property of pushing a multi-key `hw_set` via a sequence of individual
SET_CONFIG calls, orthogonal to and not addressed by relocating this
Decision's own two checks; (2) U2's own pre-existing "stale bail" note
above (this same function, ADR-086) already documents that a hardware
SET_CONFIG U1 issued before a stale-generation bail is an accepted
channel-scoped residual — `CoptUpdateparam` has no revert obligation the
way a `Temp` binding does, and this correction does not change that.

New regression test: `tests/grpc_mock/j1939.rs::coptupdateparam_
execution_time_rejection_pushes_no_hardware_write_for_the_cancelled_cop`
reuses the original test's own `PDU_IOCTL_SUSPEND_TX_QUEUE`/`RESUME_TX_
QUEUE` determinism trick, additionally staging an unrelated, hardware-
backed `DATA_RATE` change in the SAME `CoptUpdateparam` as the conflicting
`NODE_ADDRESS` promotion, and asserts `server.backdoor.set_config_count()`
never advances past its pre-dispatch baseline once the COP is cancelled —
proving no hardware write for the cancelled COP occurs at all, not merely
that the COP itself gets rejected. Full J1939 integration suite is now 52
tests.

**16. `dispatch_due_tester_present` now excludes a negotiation-enabled J1939
CLL with no currently claimed address from its dispatch pass (Finding
3).** The per-link filter closure that selects which CLLs' armed
tester-present state is even considered for dispatch now also excludes any
CLL for which `j1939_negotiated_unclaimed` (Decision 14's shared predicate)
is `true` — true both before this CLL's first successful claim and during a
spontaneous-loss-to-reclaim window. This single read-side filter, at this
one choke point, is self-resuming: once `j1939_claimed_address` becomes
`Some` again, the filter passes and Decision 6's existing `framed_data`
recompose has already fixed this CLL's armed frame by the time it does.

A stale doc comment in `run_j1939_claim_loop`'s own inner wait loop (which
also calls `dispatch_due_tester_present`, for the identical periodic-duties
reason Decision 4's Consequences entry documents) claimed this CLL was
"already excluded since `comm_started` is still false at this point" — true
only for `handle_start_comm`'s initial-claim call site, false for
`run_j1939_reclaim_duties`'s spontaneous-reclaim call site, where the CLL
stays `comm_started == true` throughout. This Decision's new filter covers
BOTH call sites correctly (neither has `j1939_claimed_address == Some(_)`
for the whole wait), so the comment is corrected to explain that, rather
than relying on the old, partially-wrong `comm_started` assumption.

**Rejected alternatives:**
- **`exclude_cll`-based exclusion at each of the (already three)
  `dispatch_due_tester_present` call sites** — rejected: `exclude_cll` only
  covers the in-loop window of the call that passes it, not the gap before
  the NEXT periodic tick, nor a sibling CLL also mid-reclaim that no
  `exclude_cll` argument at any one call site could ever name.
- **A suspend/re-arm state machine on `TesterPresentState`** — rejected:
  would need mutating state at every claim-loss/claim-gain transition site
  (roughly the same ~7 sites this exact bug class has now recurred at across
  Decisions 4/6/9/12/13/14/15), the precise propagation-multiplication
  pattern this recurrence is evidence against, versus the single read-side
  choke point this Decision uses instead.

**Accepted residual:** no dedicated end-to-end regression test exists for
this Decision's own dispatch-filter integration point. `comparam_support::
is_j1939_param` (J1939's closed ComParam allowlist) does not admit
`CP_TesterPresentHandling`/`CP_TesterPresentMessage`/`CP_TesterPresentTime`/
`CP_TesterPresentSendType` at all (the same gap Decision 6's own
Consequences entry already documents), so `resolve_tester_present` always
short-circuits disabled for a J1939 CLL reachable from a real client —
`TesterPresentState::Armed` can never actually exist for one today, so
`dispatch_due_tester_present`'s filter (old or new) never runs its `Armed`
arm for a J1939 CLL either. The shared `j1939_negotiated_unclaimed`
predicate itself IS unit-tested directly (`events_j1939_claim.rs::tests::
negotiated_unclaimed_*`, four cases: before any claim, once claimed,
negotiation not requested, non-J1939 protocol) — verified to catch a
reverted predicate (temporarily forced to always return `false`) failing
the before-any-claim case, then restored. (A fifth case asserting
`j1939_negotiated_unclaimed` on a link whose `j1939_claimed_address` was
set to `Some` and then immediately back to `None` before the assertion was
removed during round-12's own `edge-case-hunter` verification pass — the
predicate is a pure function of current state only, so that test's final
state was bit-for-bit identical to the before-any-claim case and provided
no incremental coverage despite its own "spontaneous-loss-to-reclaim
window" name implying otherwise.) The filter's own integration is verified by code inspection
instead, the same "found unreachable while writing the test, documented
instead of adding a misleading one" resolution Decision 6's own tester-present
fixes already use.

**17. `ConnectComLogicalLink`'s finalization now resets `j1939_claimed_address`/
`j1939_claim_cursor` to their unclaimed defaults on every connect, including a
same-handle reconnect (Codex review, round 13).** This bug class's EIGHTH
instance, and a new mechanism within it: `cancel_j1939_claims_for_cll`
(disconnect teardown) only clears the SHARED `SharedChannel::j1939_claims`
routing entry — it takes no `logical_links` reference at all — so the per-CLL
LOCAL `j1939_claimed_address` field used to survive a disconnect untouched.
`ConnectComLogicalLink`'s own finalization already reuses the same
`LogicalLinkState` entry for a same-`cll_handle` reconnect (ADR-086's
`connect_generation` bump exists specifically for this reuse), but stamping a
fresh generation there never reset this specific field alongside it. Without
this fix, a same-handle reconnect left `j1939_claimed_address` pointing at an
address the adapter had already stopped defending (disconnect's own cancel
already relinquished it natively), while every "am I still unclaimed" gate
that reads it (`j1939_negotiated_unclaimed`, Decisions 13/14/16) wrongly
reported "claimed" until a fresh `CoptStartcomm` overwrote it — e.g.
`ioctl_start_repeat_message` would permit an autonomous repeat slot framed
with the stale, now-undefended Active `NODE_ADDRESS`. Fixed by resetting both
fields unconditionally at the same point `connect_generation` itself is
stamped (`rpc_link.rs::rpc_connect_com_logical_link`'s finalization) — a
no-op for a brand-new CLL's very first connect (both fields already default
there), the actual fix only for a reconnect.

**Rejected alternatives:**
- **Clearing `j1939_claimed_address` inside `cancel_j1939_claims_for_cll`
  itself, at disconnect time, instead of at reconnect** — rejected: that
  function is also called from `run_j1939_claim_loop`'s own live-claim-attempt
  paths and `cancel_j1939_claim_after_failed_startcomm` (Decision 9), neither
  of which should clear an UNRELATED, already-successfully-claimed address
  this exact call is not itself acting on (it operates on `owned`, the
  addresses this CLL currently has entries for in `SharedChannel::j1939_claims`
  — not necessarily `j1939_claimed_address` itself, e.g. mid-retry before any
  claim has succeeded at all). Threading a `logical_links` reference through
  every one of this shared helper's call sites, each needing its own care
  about lock ordering against `shared_channels` (already held throughout by
  every caller, ADR-080), is a larger and riskier change than resetting two
  fields once, at the one place a fresh `connect_generation` is already
  unconditionally stamped for exactly this "must not inherit prior-generation
  state" reason (the same reasoning already applied there to
  `tx_suspended_by_error`/`isotp_rx`).
- **Resetting only `j1939_claimed_address`, leaving `j1939_claim_cursor`
  alone** — rejected: `j1939_claim_cursor` is Decision 3's own claim-loop
  progress marker into `CP_J1939PreferredAddress`'s candidate list; a stale
  non-zero value would make a reconnect's first `CoptStartcomm` silently
  resume mid-list rather than starting from the first candidate again, a
  second, narrower staleness bug of the same root cause left half-fixed.

**Accepted residual:** no dedicated regression test exists proving
`j1939_claim_cursor`'s own reset in isolation (only the combined,
externally-observable effect via the Decision 13 repeat-message gate is
tested) — the cursor has no independent externally-observable effect of its
own short of a full second claim sequence exercising a non-first candidate
post-reconnect, which the existing `claim_retries_the_next_candidate_after_
the_first_is_lost` test already covers for the ordinary (non-reconnect) case;
constructing a reconnect-specific variant was judged disproportionate given
both fields reset via the same unconditional two-line write, verified by
code inspection.

**18. A persistent `LogicalLinkState::j1939_negotiation_engaged` flag now
gates the claim-required predicate, closing a Temp-binding spoofing gap
(`design-advisor` consult, PR #72 round 15).** This bug class's TENTH
instance, and the first of a new kind: every prior instance was about
correctly picking WHICH real, structurally-legitimate ComParamSet snapshot
(Working vs. Active) to read for a field that genuinely CAN differ per-call
under ADR-067. This one is about a field that should NOT be re-interpreted
per-call at all for this specific purpose. The shared predicate
`j1939_negotiated_unclaimed_for(link, params)` (Decisions 13/14/16's
choke point) re-read `CP_J1939AddressNegotiationRule` from whichever
`params` snapshot was passed — Working, for a `temp_param_update = 1` call
(the same Decision 14-Part-A/B agreement round 12 correctly established for
OTHER, genuinely per-call-varying header fields). But `CP_J1939Address
NegotiationRule` bit 1 governs whether this CLL EVER attempted a claim in
the first place — a structural fact established once, at the CLL's own real
`CoptStartcomm`, not a transient per-COP framing choice. A client could
stage `CP_J1939AddressNegotiationRule = 2` (disable) in Working ONLY (an
ordinary `SetComParam`, never promoted to Active) and issue a
`temp_param_update = 1` `CoptSendrecv` — the predicate would read Working,
conclude "not negotiation-managed," and let an unclaimed send through
framed with the default source address, bypassing both Decision 14 Part A's
enqueue-time gate and Part B's transmit-time re-check, even though this
CLL's real, StartComm-established state says it IS negotiation-managed and
still unclaimed. ADR-180 Decision 12 already rejected gating a related
guard on the live `CP_J1939AddressNegotiationRule` ComParam for the
identical underlying reason ("a client could flip negotiation off post-claim
while the claim itself is still live") — this finding is that same failure
mode reaching the round-12 predicate through the Temp binding instead of a
live SetComParam.

Fixed by adding `LogicalLinkState::j1939_negotiation_engaged: bool` — a
PERSISTENT record of the most recent real `CoptStartcomm`'s own negotiation
decision, immune to any later `temp_param_update`'s Working staging.
`handle_start_comm`'s J1939 claim branch (entered only when negotiation IS
genuinely requested, per that branch's own real, non-Temp-bindable-for-this-
purpose dispatch condition) sets it `true` before `run_j1939_claim_loop`
runs, so the flag is live for the whole in-flight-claim wait window; a new,
sibling `else if` branch for a genuinely non-negotiated real `CoptStartcomm`
sets it `false`. `ConnectComLogicalLink`'s finalization resets it to `false`
alongside Decision 17's own two-field reset (same "no-op for a first
connect, actual fix for reconnect" reasoning). The shared predicate now ORs
the per-call snapshot check with this persistent flag:
`(j1939_claim_requested(params...) || link.j1939_negotiation_engaged) &&
link.j1939_claimed_address.is_none()` — no call-site changes needed at any
of Decisions 13/14/16's four consumers, since all already pass `link`
itself. `resolve_send_recv_tx`'s `j1939_tx_source` computation (Decision 14
Part B) is widened to compute for every J1939 CLL unconditionally (dropping
its own `j1939_claim_requested(params...)` sub-gate, now redundant since
applicability moves to the next step), and `handle_send_recv`'s per-cycle
transmit-time cancellation check is widened to additionally require the
LIVE `link.j1939_negotiation_engaged` flag alongside its existing
source-address-mismatch comparison — closing not just the reported spoof
(unclaimed + engaged + Working-spoofed-non-negotiated) but a sibling case
the `design-advisor` consult identified: a CLL that IS claimed at address X,
whose Temp-bound send stages non-negotiated AND a different `NODE_ADDRESS`
value Y — the live-flag-gated comparison still cancels it (`Some(X) !=
Some(Y)`), since a genuinely negotiation-engaged CLL's live claimed address
must never be silently overridden by a per-call Working value either.

**Rejected alternatives (design-advisor consult):**
- **Gate on `SharedChannel::j1939_claims` registration instead of a new
  field** — rejected: that map has no entry during several windows this
  gate must still cover (after `Exhausted`, during a spontaneous-loss
  reclaim window post-Decision-5's removal, after Decision 9's own
  cancel-on-failed-StartComm) — the wrong structural fact for this purpose.
- **Revert round 12 entirely (read Active unconditionally again)** —
  rejected: reopens round 12's own gate/resolution disagreement and breaks
  the legitimate Temp-bound opt-out case (`coptsendrecv_temp_binding_uses_
  workings_own_negotiation_rule_not_actives`, a CLL that never issues a real
  `CoptStartcomm` at all, so `j1939_negotiation_engaged` correctly stays
  `false` and the Temp-bound opt-out must still succeed).
- **Derive "engaged" from `comm_started` plus Active's own negotiation
  rule** — rejected: `comm_started` is `false` in several of the same
  windows the first rejected alternative names, and Active's own rule value
  is itself client-promotable mid-window via an ordinary `SetComParam`.

**Round-16 correction (Codex review, PR #72):** the paragraph above
originally recorded an accepted residual here — a non-negotiated
RE-`CoptStartcomm` (the `else if` branch) set `j1939_negotiation_engaged =
false` without itself calling `cancel_j1939_claims_for_cll`, leaving the
adapter still natively defending the previously-claimed address while the
client believed it had opted out. That residual is now closed: the branch
mirrors the negotiation-requesting branch's own "fresh attempt" cancel
(Decision 4) exactly — same `shared_channels`-first, nested-`api`-for-the-
native-cancel, ADR-080-conformant order, released before the
`logical_links` acquisition that clears `j1939_claim_cursor`/
`j1939_claimed_address`/`j1939_negotiation_engaged` — followed by the same
`stop_repeat_slots_for_cll`/`cancel_send_recv_cops_for_cll` pair Decisions
10/11 already apply everywhere else this bug class relinquishes an
address. New regression test:
`tests/grpc_mock/j1939.rs::opting_out_of_negotiation_after_a_claim_frees_the_address_for_a_sibling`
— proves a sibling CLL with the same single candidate can claim the
address only once the opting-out CLL's stale claim has actually been
cancelled and deregistered (the same sibling-claim-proof pattern Decision
4's own regression test uses).

**Round-20 correction (Codex review, PR #72; `design-advisor` consult):**
the persistent flag this Decision introduced was a `bool`, which can only
distinguish "engaged" from "not yet touched / opted out" — it cannot tell
"never touched because this CLL never requested negotiation at all" apart
from "false because this CLL's real StartComm explicitly, successfully
opted OUT," even though those two `false` cases need opposite predicate
answers once the CLL's Active `CP_J1939AddressNegotiationRule` no longer
agrees with which one actually happened. A `temp_param_update = 1`
`CoptStartcomm` that stages an OPT-OUT in Working only (Active still
holding the negotiation-enabled default, never promoted) correctly takes
the non-negotiated branch — no claim attempted, the flag set `false` — but
every LATER ordinary (non-Temp) operation on that CLL reads `link.active`
via the default `j1939_negotiated_unclaimed` wrapper, which still reports
negotiation requested, so the OR'd bool contributes nothing and the
predicate wrongly concludes "negotiated and unclaimed" forever — the
mirror image of the spoof this Decision originally closed.

`LogicalLinkState::j1939_negotiation_engaged: bool` is replaced with a
tri-state `J1939NegotiationPosture { Undecided (default), Engaged,
OptedOut }`. The predicate now matches on posture instead of OR-ing a
bool: `Engaged` is unconditionally negotiated (preserving this Decision's
original spoof-closing behavior), `OptedOut` is unconditionally NOT
negotiated (closing this round's gap), and only `Undecided` — no real
`CoptStartcomm` has yet decided this CLL's posture — falls back to reading
`CP_J1939AddressNegotiationRule` from the passed-in `params` snapshot, the
pre-Decision-18 behavior for a CLL that has never issued a real
`CoptStartcomm` at all. Both `handle_start_comm` J1939 branches now set the
posture directly instead of a bool: the negotiation-requesting branch to
`Engaged`, the non-negotiated branch (this Decision's own round-16
correction, above) to `OptedOut`. `ConnectComLogicalLink`'s finalization
resets posture to `Undecided`, matching the original reset's own
"no-op for a first connect, actual fix for reconnect" reasoning. The two
raw-flag drift-cancellation reads this Decision added
(`resolve_send_recv_tx`'s Part B check and `handle_stop_comm`'s Decision
19 check) now test `matches!(posture, Engaged)` — `OptedOut` and
`Undecided` both keep today's `false` at those two sites, no behavior
change there.

Posture is re-decided only by the CLL's next real `CoptStartcomm`, never by
a later `CoptUpdateparam`/`SetComParam` promoting
`CP_J1939AddressNegotiationRule` back onto Active — per clause 16's own
model (paraphrased at `events_j1939_claim.rs`'s `j1939_claim_requested` doc
comment), a claim is only ever performed upon a STARTCOMM ComPrimitive, so
a per-call send can never itself trigger one; gating a live send on a
promoted-but-unacted-on Active value would just re-litigate Decision 12's
already-rejected "gate on the live ComParam" alternative from the opposite
direction. One deliberate, narrower behavior change beyond the fix itself:
a Temp-bound send now staging negotiation back ON while the CLL's real
posture is `OptedOut` is allowed to proceed unclaimed (previously it
would incorrectly have been blocked forever by the bug this correction
fixes) — correct for the same clause-16 reason, and the intended symmetric
completion of this Decision's own "per-call staging must not re-interpret
structural posture" principle.

**Rejected alternatives (design-advisor consult):**
- **Promote Working to Active immediately on a Temp-bound StartComm, drop
  the persistent field entirely** — rejected: violates ADR-067's
  per-call-only Temp-binding contract (Active changes only via an ordinary
  `CoptUpdateparam`/`SetComParam` or a service-originated write-back like
  the claimed `NODE_ADDRESS` itself), and Active remains client-promotable
  mid-window regardless — the same reason this Decision's own original
  text already rejected deriving posture from Active.
- **A second, independent `j1939_opted_out: bool` alongside the existing
  flag** — rejected: admits an invalid both-`true` state the tri-state enum
  makes unrepresentable, and every reset/set site would need to
  independently maintain the two-bool invariant by hand.
- **Write the StartComm's resolved negotiation decision back into Active,
  mirroring the `NODE_ADDRESS` write-back precedent** — rejected: does not
  close the round-15 spoof this Decision's own mechanism prevents (a
  Temp-bound send still reads Working, not Active, per Decision 14 Part
  A/B), and Active remains later-overwritable regardless, so a persistent
  field is still required — the write-back would add nothing but
  client-visible surprise.

New unit test: `events_j1939_claim.rs::tests::negotiated_unclaimed_false_when_opted_out_even_if_snapshot_says_negotiated`
(an `OptedOut` posture with a `params` snapshot that still reads
negotiation-requested must return `false`), alongside the pre-existing
`Engaged`/`Undecided` cases kept unchanged as this round's own regression
tripwires. New integration test:
`tests/grpc_mock/j1939.rs::temp_bound_negotiation_opt_out_does_not_wrongly_block_later_ordinary_sends`
— a Temp-bound opt-out `CoptStartcomm` finishes with no claim attempted,
then confirms both an ordinary `CoptSendrecv` and
`PDU_IOCTL_START_REPEAT_MESSAGE` on that same CLL now succeed (both were
wrongly rejected pre-fix). Confirmed to catch the regression by
temporarily reverting the opt-out branch's posture assignment back to
`Undecided` and observing the new test fail with the exact
`FailedPrecondition` rejection this round's finding describes, then
restoring it. Full J1939 integration suite is now 56 tests.

**Round-24 correction (Codex review, PR #72; `design-advisor` consult):**
the non-negotiated branch's own round-16 correction (above) called
`cancel_j1939_claims_for_cll` but discarded its outcome entirely, promoting
this CLL to `OptedOut` regardless of whether the native cancel succeeded.
`cancel_j1939_claims_for_cll`'s settled shape (Decision 22) always removes
the routing entry, but on a failed cancel additionally pushes the address
into `SharedChannel::leaked_j1939_claims` instead of returning any
success/failure signal to its caller. Because `OptedOut` is the posture
`j1939_negotiated_unclaimed_for` unconditionally trusts as "not
negotiation-managed," a failed cancel here let this StartComm complete and
the client immediately start transmitting under a NEW client-managed
`CP_TesterSourceAddress` while the adapter might still defend the OLD
address under the SAME NAME. Unlike the negotiation-requesting branch, this
branch never calls `run_j1939_claim_loop`, so it never revisited round-23's
own reconcile-then-gate for `leaked_j1939_claims` — an opted-out CLL may
never claim an address again, so nothing else would ever reconcile a leak
this branch itself just caused.

Fixed by giving this branch the same channel-wide reconcile-then-gate
round-23 gave the claim loop, via a shared helper,
`reconcile_leaked_j1939_claims` (`events_j1939_claim.rs`), extracted from
round-23's own block so both call sites retry the exact same way instead of
risking drift between two copies (widened to a third call site by round-27's
own correction below, `ioctl_start_repeat_message`, for the identical
disjoint-caller reasoning -- see the "Round-27 correction to Decision 22"
paragraph, not Decision 24, which documents an unrelated mechanism). Under
the branch's existing
`shared_channels`-then-nested-`api` hold (the same lock order
`cancel_j1939_claims_for_cll`'s own call already used), immediately after
that call: retry every address in `SharedChannel::leaked_j1939_claims`
(channel-wide, not just this CLL's own just-relinquished address — the
identical reasoning round-23 already recorded: `leaked_j1939_claims`
carries no NAME/owner attribution, so a same-NAME reconnect under a
DIFFERENT `cll_handle` would still evade a narrower, this-address-only
check, since the hazard is transmission under a new source address while
the NAME's old claim is still adapter-defended, not a future reclaim by
this specific handle). If any address survives the retry, this StartComm
fails closed — mirroring the negotiation-requesting branch's own
`J1939ClaimLoopOutcome::Exhausted` arm emission shape exactly
(`still_on_this_channel` snapshot first, Temp-binding revert, then
`PduErrEvtInitError`+`PduCopstFinished`, or `PduCopstCancelled` when stale)
— and, critically, `return`s before the shared optional-message transmit
sequence that runs after this whole `if`/`else if` chain, which would
otherwise send under this CLL's new client-managed source while the
adapter may still defend the relinquished one (Decision 3's own precedent:
a failed StartComm never sends its optional message). `j1939_claimed_
address` is still cleared unconditionally either way (the routing entry is
already gone regardless of the retry's outcome, and a stale `Some(..)`
here would let a later send frame under the very address this branch just
tried to relinquish); `j1939_negotiation_posture` is the actual gate — left
untouched (not promoted to `OptedOut`) when a leak survives, so
`j1939_negotiated_unclaimed_for` keeps every send/repeat/tester-present
gate closed for this CLL exactly as it would for a CLL that never opted
out at all. The Decision-10/11 relinquishment sweeps
(`stop_repeat_slots_for_cll`/`cancel_send_recv_cops_for_cll`) still run
unconditionally on both outcomes — the relinquishment already happened at
the cancel call, not at StartComm success, so any repeat slot or live
`CoptSendrecv` this CLL held under the old address needs the same cleanup
either way.

Retried inline here too (not a bare check), for the same permanent-wedge
reason round-23 retries rather than merely checking: an opt-out-bound CLL
never re-enters `run_j1939_claim_loop`, so a channel left with only
opt-out CLLs would otherwise have no reconciliation owner at all for a
pre-existing leak — the client's own natural recovery (retry the
StartComm) must itself be a reconciliation opportunity, exactly as it
already is for the negotiated path.

**Rejected alternatives (design-advisor consult):**
- **Check only this CLL's own just-relinquished address, not the whole
  channel** — rejected for the identical reason round-23 rejected it for
  the claim loop: a same-NAME reconnect under a different `cll_handle`
  would evade a narrower check, since `leaked_j1939_claims` has no NAME
  field to match against.
- **"Retain a gated posture" alone, without failing the StartComm itself**
  — rejected as insufficient on its own: a *completing* StartComm still
  sends the optional message under the new source and reports success to
  the client, both false while a leak persists. The untouched posture is
  kept as defense-in-depth (belt), not as the mechanism (suspenders) — the
  actual fix is failing the StartComm closed.
- **Force posture to `Undecided` instead of leaving it untouched** —
  rejected: `Undecided` falls back to reading `params`, which would
  *weaken* an `Engaged` prior posture's gate to a spoofable snapshot read;
  leaving posture untouched is strictly safer.

**New regression test:**
`tests/grpc_mock/j1939.rs::opt_out_startcomm_fails_closed_while_a_relinquished_claim_remains_leaked`.
Three acts, reusing Decision 22's own `__mock_set_j1939_cancel_error`
backdoor: (1) CLL A claims 0x80 via a negotiated StartComm; (2) backdoor
armed, StopComm, restage `CP_J1939AddressNegotiationRule` to opt out,
StartComm — proves `PduErrEvtInitError`, nothing written to the mock, and a
subsequent ordinary transmitting `CoptSendrecv` still synchronously
rejected (`FailedPrecondition`) since posture never promoted to
`OptedOut`; (3) backdoor disarmed, the opt-out StartComm repeated —
succeeds once the reconcile retry clears 0x80, and a subsequent
`CoptUpdateparam` promoting `CP_TesterSourceAddress` plus an ordinary send
confirm the client-managed source genuinely takes effect on the wire.
Confirmed to catch the regression by temporarily short-circuiting the new
gate's condition (`false && !reconcile_leaked_j1939_claims(...)`) and
observing the test fail exactly as expected, then restoring it. Full
J1939 integration suite is now 62 tests.

**19. A `CoptStopcomm` final message's pre-composed J1939 source address is
re-verified against the CLL's live claim immediately before transmit,
suppressing (not sending) it on a drift (`design-advisor` consult, PR #72
round 16, Codex P1). This bug class's TWELFTH instance, and a new
mechanism shape.** `cancel_send_recv_cops_for_cll` (Decision 11) filters
on `is_send_recv && transmits`, deliberately excluding `CoptStartcomm`
(would cancel its own reclaim attempt) and `CoptStopcomm` (would recreate
ADR-085's `stop_comm_pending` deadlock) from its cancellation sweep — see
that function's own doc comment. A `CoptStopcomm`'s optional final
message is resolved and framed with the CLL's claimed source address at
`StartComPrimitive` call time (ADR-085/ADR-087, always Active-bound, never
Working). If this CLL's claim is then spontaneously lost (clause 16.4.6)
before the COP actually dispatches — a window `cancel_send_recv_cops_for_cll`
structurally cannot close for this COP kind — the pre-composed frame would
transmit anyway, still claiming (byte 3 of the J1939 header) an address
this CLL no longer owns, impersonating whichever CLL — or nothing — claims
it next.

Fixed by extending `handle_stop_comm`'s existing pre-transmit
`still_on_this_channel` critical section (the `P3GapOutcome::Ready` arm)
with the identical transmit-time re-check Decision 14 Part B/Decision 18
already apply to `CoptSendrecv`: under the same `logical_links` lock hold,
`drifted = link.j1939_negotiation_engaged &&
tx.send.j1939_tx_source.is_some_and(|src| link.j1939_claimed_address !=
Some(src))`. `j1939_negotiation_engaged` gates it the same way it gates
`handle_send_recv`'s check — `false` permanently for a CLL that never
requested negotiation (never suppressed here); `true` for one that
structurally did, in which case a mismatch (INCLUDING no current claim at
all) means this final message would defend or impersonate an address this
CLL no longer holds. On drift: emit `PduErrEvtProtErr`, skip the transmit
(and any receive phase) entirely, and fall through unconditionally to the
unchanged terminal `PduCllstOnline`/`PduCopstFinished` block — the same
best-effort "still complete teardown" contract the sibling
`TxFailure::Event` arm just below already uses for a transmit that reached
the wire and failed. No `StartComPrimitive`-time gate is added
(deliberately, unlike Decision 14 Part A's `CoptSendrecv` gate): rejecting
the StopComm at enqueue time would block teardown itself, contradicting
ADR-085's "teardown must never be blocked" principle; the dispatch-time
check alone already covers both the queued-before-loss and
already-lost-at-call-time cases.

**Rejected alternatives (design-advisor consult):**
- **Cancel the whole COP** (add `CoptStopcomm` to
  `cancel_send_recv_cops_for_cll`'s filter) — rejected: recreates
  ADR-085's `stop_comm_pending`/teardown deadlock that function's own doc
  comment already names as the reason `CoptStopcomm` is excluded.
- **Recompose the frame against a fresh address** — rejected: no address
  exists during the loss-to-reclaim window (the identical rejection
  Decisions 11/14 already record); Active `NODE_ADDRESS` still holds the
  lost address for that whole window (Decision 9's own accepted residual).
- **`StartComPrimitive`-time gate** — rejected: blocks teardown itself: the
  dispatch-time check alone covers every window a gate would, without that
  cost.
- **Silent suppression (no error event)** — rejected: the client would see
  `PduCopstFinished` with no signal that its final message never went out;
  every other best-effort transmit-failure path in this function (e.g.
  `TxFailure::Event`) reports.

**Accepted residual:** the in-window race between this drift check and
`transmit_request`'s own native write (a loss landing in that narrow gap)
is the same unobservable, sub-poll-interval class Decision 14's own
residual documents — not closed here for the same reason. The RC21/RC23
receive-phase re-request path (`original_data`, a potential second
transmit of the same stale frame if a response times out) was verified
structurally unreachable for J1939 rather than assumed so:
`rc21_handling`/`rc23_handling` come only from
`PARAM_RC21/23_HANDLING` (`service.rs`, default `0`), which
`comparam_support::is_j1939_param`'s allowlist excludes and which
`comparam_defaults.rs::j1939_can_common` never seeds — so a J1939
`CoptStopcomm` can never reach that path regardless of this fix. New
regression test:
`tests/grpc_mock/j1939.rs::stopcomm_final_message_after_a_spontaneous_loss_is_suppressed_not_sent`
— claims an address, forces the spontaneous-loss-armed reclaim to exhaust
deterministically (`set_j1939_claim_lost`, single candidate), then issues
a `CoptStopcomm` with a non-empty `cop_data` and asserts no additional
byte reaches the mock, a `PduErrEvtProtErr` is reported, and the COP still
completes to `PduCllstOnline`/`PduCopstFinished`.

**20. A `stop_repeat_slots_for_cll` STOP failure other than `ERR_INVALID_MSG_ID`
is now routed into `SharedChannel::leaked_repeat_message_ids` for opportunistic
retry, instead of being silently dropped (Codex review, PR #72 round 20).**
`stop_repeat_slots_for_cll` (Decision 10) best-effort-stops every live SAE
J2534-2 clause 14 repeat-message slot a J1939 CLL owns whenever it relinquishes
its claimed address. Before this fix, a `STOP_REPEAT_MESSAGE` failure other
than `ERR_INVALID_MSG_ID` was only logged (`warn!`) and the `MsgId` left in
`link.repeat_message_ids` forever, with no retry — the device kept
transmitting under the relinquished address indefinitely, the same "cease
transmitting under a lost address" violation Decision 10's own Context
paragraph describes, just reintroduced via the one failure branch Decision 10
did not actually close.

This codebase already has an established, tested mechanism for exactly this
shape of problem: ADR-165 Decision 6's `SharedChannel::leaked_repeat_message_ids`
tracks a `MsgId` whose STOP failed while its owning CLL was torn down but its
shared physical channel survived, and `J2534Service::retry_leaked_repeat_message_stops`
opportunistically retries it the next time any CLL on that channel touches a
repeat-message IOCTL, self-pruning on success or `ERR_INVALID_MSG_ID`. Decision
20 reuses that same mechanism for a live CLL's own relinquishment-triggered
failure, rather than inventing a parallel one.

Implemented by:
- Widening `stop_repeat_slots_for_cll`'s signature to
  `#[must_use] async fn(..) -> Vec<u32>`, returning every `MsgId` whose STOP
  failed with something other than `ERR_INVALID_MSG_ID` (previously just
  logged and left in `repeat_message_ids`). A failed id is now also removed
  from `repeat_message_ids` — ownership transfers to the leaked list, not
  double-tracked in both places at once.
- Two new helpers in `events_j1939_claim.rs`, mirroring the split this file's
  callers already have between holding `shared_channels` (ADR-180 Decision 13)
  and not: `push_leaked_repeat_slots(chans: &mut HashMap<ChannelKey,
  SharedChannel>, channel_id, failed)` — synchronous, for a caller that
  already holds the guard, using the exact same `values_mut().find(|sc|
  sc.channel_id == channel_id)` lookup this file and `events.rs` already use
  elsewhere, no-op if the channel was concurrently torn down (the native
  disconnect already killed the device-side slot with it), and skipping a
  push if the id is already present (a concurrent CLL teardown may have
  already pushed the same id via `rpc_link.rs`'s own leak-on-teardown path);
  and `record_leaked_repeat_slots(ctx, failed)` — the async, lock-acquiring
  counterpart for a caller that does not already hold `shared_channels`,
  early-returning before acquiring anything when `failed` is empty.
- All 8 call sites of `stop_repeat_slots_for_cll` (4 in `events_j1939_claim.rs`,
  4 in `events.rs`) updated to capture the returned `Vec<u32>` and route it
  through whichever helper matches that call site's own lock state — the 4
  sites already holding `shared_channels` across the call (both `Claimed`/
  `Exhausted` arms in `run_j1939_claim_loop` and `handle_start_comm`, per
  Decision 13) now bind that guard `mut` and call `push_leaked_repeat_slots`
  directly; the other 4 (`deliver_j1939_claim_indication`'s spontaneous-loss
  branch, `cancel_j1939_claim_after_failed_startcomm`, and `handle_start_comm`'s
  two non-`Claimed`/`Exhausted` relinquishment call sites) call
  `record_leaked_repeat_slots`.

`stop_repeat_slots_for_cll` itself still never acquires `shared_channels` --
the exact invariant its own doc comment already documented, which Decision
13's held-caller safety argument depends on (a caller holding `shared_channels`
first, outermost per ADR-080, never conflicts with a callee that never itself
acquires it). Decision 20 preserves that invariant by having the CALLER route
the returned `Vec<u32>`, not by having `stop_repeat_slots_for_cll` reach for
`shared_channels` itself.

**Rejected alternatives (design-advisor consult):**
- **Thread an `Option<&mut MutexGuard>` into `stop_repeat_slots_for_cll`
  itself, so it can push the leaked ids inline regardless of caller lock
  state** — rejected: this destroys the "never touches `SharedChannel`, never
  itself acquires `shared_channels`" invariant Decision 13's own safety
  argument depends on, and a caller that passes `None` while actually holding
  the guard elsewhere in the same call stack would silently deadlock the next
  time something tries to acquire it — a much worse failure mode than a
  logged, retried leak.
- **Have the helper acquire `shared_channels` itself, unconditionally, after
  the caller releases any locks it holds** — rejected: deadlocks at the 4
  call sites that hold `shared_channels` across the call by design (Decision
  13's own TOCTOU-closing serialization), which cannot release it before this
  cleanup step without reopening the exact race Decision 13 exists to close.
- **A brand-new per-link retry field/pump dedicated to this failure, instead
  of reusing `leaked_repeat_message_ids`** — rejected: duplicates ADR-165
  Decision 6's existing, already-tested channel-scoped retry mechanism for no
  benefit, and a per-LINK field would lose the ids entirely the moment the
  CLL itself disconnects — exactly the scenario `leaked_repeat_message_ids`
  (channel-scoped, surviving its originating CLL) exists to survive.

**New regression test:**
`tests/grpc_mock/j1939.rs::spontaneous_loss_leaked_repeat_stop_is_drained_by_a_later_sibling_ioctl`.
A new mock backdoor, `__mock_set_stop_repeat_message_error`/
`MockBackdoor::set_stop_repeat_message_error` (mirroring `stop_filter_error`'s
existing channel-scoped shape and its "checked after the bad-`channel_id`
check, before the real operation" ordering -- `j1962_pin_voltage_error` is
device-scoped, with no `channel_id` check to order against, so it isn't the
right precedent here despite the superficially similar `Option<c_long>`
override idiom; checked inline inside the generic `IOCTL_STOP_REPEAT_MESSAGE`
dispatch arm rather than a dedicated FFI export, since `STOP_REPEAT_MESSAGE`
itself has no dedicated export to hang the check on), forces
`PassThruIoctl(STOP_REPEAT_MESSAGE)` to fail deterministically for a `MsgId`
that is genuinely still alive -- something the mock previously could only
fail with `ERR_INVALID_MSG_ID` for a `MsgId`
that never existed. The test claims 0x80, starts a repeat slot under it
(condition 0, `REPEAT_MESSAGE_UNTIL_MATCH`, so the claim-loss injection's own
non-matching payload cannot confound the result via condition 1's separate
self-termination rule), arms the STOP override, synthesizes the same
`RX_FLAG_J1939_ADDRESS_LOST` spontaneous-loss injection Decision 10's own
tests use, and confirms the slot stays live (`Some(1)`) after the sweep has
had time to run and fail -- proving the failure is genuine, not a test
artifact. It then clears the override and issues `PDU_IOCTL_START_REPEAT_
MESSAGE` from a second, non-negotiated sibling CLL sharing the same physical
channel (`j1939_negotiated_unclaimed` never blocks a non-negotiated CLL,
letting it exercise `retry_leaked_repeat_message_stops`'s own "top of every
START/QUERY/STOP_REPEAT_MESSAGE handler" entry point without needing its own
claim), and polls for the originally leaked `MsgId` to finally report gone
(`None`). Confirmed to catch the regression by temporarily deleting the
`record_leaked_repeat_slots(ctx, failed).await;` call at this Decision's
spontaneous-loss call site (`deliver_j1939_claim_indication`) and observing
the final poll assertion fail (the leaked id was never recorded, so the
sibling's retry had nothing to drain), then restoring it. Full J1939
integration suite is now 55 tests.

**Accepted residual (`edge-case-hunter` finding, round 20):** only the
`record_leaked_repeat_slots`/not-held call site inside
`deliver_j1939_claim_indication`'s spontaneous-loss branch is exercised
end-to-end by the regression test above; the other 7 rewired call sites
(the sync, held-guard `push_leaked_repeat_slots` path used by
`run_j1939_reclaim_duties`'s `Claimed`/`Exhausted` arms and
`handle_start_comm`'s `Claimed`/`Exhausted` arms; the remaining
`record_leaked_repeat_slots` sites in `handle_start_comm`'s fresh-attempt
reset and non-negotiated-opt-out branches and in
`cancel_j1939_claim_after_failed_startcomm`) have no dedicated test forcing
a STOP failure through them specifically. All 8 sites' lock-state
classification (held vs. not-held, correct helper chosen for each) was
independently re-derived and confirmed correct by direct code reading in
two separate verification passes (the `design-advisor` consult that
designed this mechanism, and a later `edge-case-hunter` pass reviewing the
finished diff) — but manual verification is not a substitute for a test,
and a future refactor of any of the other 7 sites could silently regress
with nothing to catch it. Not fixed here: the 8 sites are structurally
identical (two thin, already-tested helper functions; the only variable is
which of two already-verified call shapes wraps them), so the marginal
value of 7 more integration tests exercising the same two-function
mechanism was judged not to justify the cost against this PR's already
long review history — revisit if a future round finds an actual regression
in one of the untested sites.

**21. `run_j1939_claim_loop`'s bounded wait now observes an in-flight
`CoptStartcomm` COP's own cancellation (Codex review, PR #72; `design-advisor`
consult).** The claim loop can block for up to `candidate_count *
CP_J1939AddrClaimTimeout` per StartComm-driven claim attempt, but it was
never given the driving COP's `cop_handle` and never consulted
`LogicalLinkState::cancelled_cops` -- the mark-and-defer mechanism
`rpc_cancel_com_primitive` populates for an explicit client cancel, and every
other long-running wait loop in this file already checks (mirroring
`CoptDelay`'s own wait loop, `events.rs`). A `CancelComPrimitive` against an
in-flight claim-driving `CoptStartcomm` -- especially one with empty
`cop_data`, which has no later transmit-phase cancellation check to catch it
-- could therefore still let the loop run to completion, reporting
`COMM_STARTED`/`PduCopstFinished` for a COP the client had already cancelled,
or wait through the entire candidate list regardless.

`run_j1939_claim_loop` now takes an additional `cancel_cop: Option<u32>`
parameter -- `Some(cop_handle)` from `handle_start_comm`'s claim-driving call
site, `None` from `run_j1939_reclaim_duties`'s spontaneous-reclaim call site
(no COP drives a spontaneous reclaim, so cancellation can never apply there).
Both of the loop's existing per-iteration `logical_links` staleness checks
(the outer-loop top, and the inner wait loop's own per-tick check) now also
consult and consume `cancelled_cops` for this `cop_handle`, folded into the
same lock acquisition via the crate-standard `(cancelled, stale)` tuple idiom
-- no new lock acquisitions, no new per-tick cost. A cancellation observed at
the outer-loop top (nothing registered/in-flight for the current candidate
yet) returns `J1939ClaimLoopOutcome::Cancelled` (new variant) directly. A
cancellation observed inside the inner wait loop -- where the current
candidate DOES have a live `j1939_claims` registration and an issued native
claim -- is routed through the SAME cancel-then-conditionally-remove cleanup
block Decisions 2's round-20/21 corrections already established for the
timeout case (replacing the prior `timed_out_without_indication: bool` local
with a three-way `WaitEnd { Lost, TimedOut, Cancelled }`), additionally
draining any raced, unconsumed `j1939_claim_results` entry for this
`cll_handle`. Unlike the timeout case, `Cancelled` is returned regardless of
whether the native cancel itself succeeded -- the COP terminates as
client-cancelled either way; on a failed native cancel, the retained entry
is the same in-doubt record round-20/21 already reconciles via the next
cancel-owning event for this CLL (Decision 4's sweep, or teardown; and now
Decision 22's leaked-set guard, if the sweep's own cancel also fails).

**Raced-`Claimed` rule:** because the cancellation check runs before the
outcome-drain read in each loop iteration, a cancellation racing an
already-delivered `Claimed` outcome resolves in favor of `Cancelled` -- the
loop still runs the same cleanup block, natively cancelling the
just-claimed address. This is the correct client contract: a COP reporting
`PduCopstCancelled` must never also produce `COMM_STARTED`.

`handle_start_comm`'s own `match` on the loop's outcome gains a `Cancelled`
arm mirroring the existing `P3GapOutcome::Cancelled` arm elsewhere in this
same function: revert any Temp-binding hardware change, `emit_terminal_if_
live(.., PduCopstCancelled)`, return -- deliberately NOT calling Decision 9's
`cancel_j1939_claim_after_failed_startcomm` (`j1939_claimed_this_cop` is
still `false`; the loop already cancelled its own in-flight candidate
internally) and deliberately leaving `j1939_negotiation_posture` untouched
(matching the existing `Exhausted` arm's own behavior -- posture is
re-decided only by the CLL's next real StartComm, Decision 18).
`run_j1939_reclaim_duties`'s own match gains a `Cancelled =>
unreachable!(..)` arm, since it always passes `None`.

This extends the invariant this ADR records (Decisions 1/2's own text,
above): a client-cancelled in-flight claim attempt is now owned by the claim
loop itself, via the same cleanup block the timeout case already uses --
never left to run to an unwanted success or to hold the wait for its full
duration after the client has already moved on.

**Rejected alternatives (design-advisor consult):**
- **Checking only in the inner wait loop, not the outer-loop top** --
  rejected: correct in outcome, but a cancellation racing the outer-loop
  check would still issue one doomed native claim for the next candidate
  before the inner loop's own check catches it on its first tick; the outer
  check is free (same lock acquisition already happening there).
- **Having `handle_start_comm` check `cancelled_cops` only after the loop
  returns, instead of inside it** -- rejected: this is exactly the finding
  itself -- the whole `candidate_count * timeout` wait would still run to
  completion first.
- **A dedicated cancellation-token field on `LogicalLinkState` instead of
  reusing `cancelled_cops`** -- rejected: duplicates the crate-wide mechanism
  every other long-running wait loop already uses, for no benefit.

**Accepted residual:** a cancellation landing after `run_j1939_claim_loop`
already returned `Claimed` but before `handle_start_comm`'s own shared
transmit-tail (the optional CoptStartcomm message's send, for an
empty-`cop_data` COP with nothing left to check cancellation against) is the
same pre-existing, crate-wide terminal-race window every COP already accepts
as best-effort -- neither widened nor narrowed by this Decision.

**New regression test:**
`tests/grpc_mock/j1939.rs::cancel_com_primitive_during_the_claim_wait_ends_it_promptly_as_cancelled`.
New mock backdoor `__mock_set_j1939_claim_no_indication`/
`MockBackdoor::set_j1939_claim_no_indication` (mirroring
`__mock_set_j1939_claim_lost`'s existing global-toggle shape): the claim
form of `IOCTL_PROTECT_J1939_ADDR` still returns success synchronously but
skips both its normal claimed-address bookkeeping and its normal RX
indication push, so the loop's bounded wait genuinely waits rather than
resolving instantly -- the same "stage a long real wait window, then act
inside it" technique this suite already uses elsewhere (e.g.
`wait_for_p3_gap`-seeding tests). With a large staged
`CP_J1939AddrClaimTimeout`, the backdoor enabled, and an empty-`cop_data`
`CoptStartcomm`, the test waits for the COP to reach `PduCopstExecuting`,
issues `CancelComPrimitive`, and confirms `PduCopstCancelled` arrives
promptly (not after the full timeout) with no `COMM_STARTED`/`Finished` ever
reported. Confirmed to catch the regression by temporarily forcing
`cancel_cop` to `None` inside the loop and observing the test fail exactly
as expected (hangs to the staged timeout instead of ending promptly), then
restoring it. Full J1939 integration suite is now 60 tests.

**Accepted residual (`edge-case-hunter` finding):** the regression test above
uses a single-candidate `CP_J1939PreferredAddress` list and only sends
`CancelComPrimitive` once the loop is already blocked inside the INNER wait
(the sole candidate's native claim already issued) -- with one candidate,
the OUTER-loop-top cancellation check (the one that fires when nothing is
currently in-flight, e.g. between candidates) can never be the checkpoint
that actually catches this test's cancellation, and the "force `cancel_cop`
to `None`" regression-catch proof disables both checks at once, so it
cannot demonstrate the outer check's own individual contribution in
isolation. Closing this needs a multi-candidate list with the cancellation
timed to land between one candidate's resolution and the next one's native
issue -- a narrow window with no existing mock hook to pause execution
inside it deterministically, the same "no natural preemption point" class
several of this ADR's other race-window residuals already accept. The outer
check's own correctness was verified by code inspection (both checks share
the identical tuple-check idiom and route to the same well-tested
`Cancelled` handling) rather than by an isolated test; not fixed here --
revisit if a future round finds an actual regression specific to the
outer-loop-top checkpoint, or if a dedicated pause-here mock hook is added
for another reason and this test can be extended to use it.

**22. A relinquishment's failed native cancel now leaks the address into
`SharedChannel::leaked_j1939_claims` for opportunistic retry, instead of
being silently forgotten (Codex review, PR #72; `design-advisor`
consult).** `cancel_j1939_claims_for_cll` -- the shared helper Decisions 1
(teardown), 4 (fresh attempt), 9 (failed StartComm), and 18 (negotiation
opt-out) all use to relinquish every address a CLL owns -- removed every
owned address from `SharedChannel::j1939_claims` unconditionally, BEFORE
attempting any native cancel, and only logged (never tracked) a cancel
failure. Its own doc comment framed this as "a wasted address slot, not a
correctness hazard... nothing routes to it," but that framing does not hold
at any of its 5 call sites: both teardown sites are gated on the physical
channel staying open (i.e. they run precisely when sibling CLLs -- potential
future claimants -- survive), and the Decision-4 fresh-attempt site
immediately re-enters the claim loop from cursor 0 for the SAME `cll_handle`.
A failed cancel therefore freed the address for a NEW claimant (a sibling,
or this CLL's own fresh attempt) while the adapter might still be defending
it under the OLD claim -- two claimants, one address, the exact bug class
this whole ADR exists to close.

Mirrors this codebase's own established mechanism for exactly this problem
shape, ADR-165 Decision 6 / this ADR's own Decision 20:
`SharedChannel::leaked_j1939_claims: Vec<u8>` (new field, dup-checked
pushes) records an address whose native cancel failed after its routing
entry was removed. `cancel_j1939_claims_for_cll` is restructured to
per-address cancel-THEN-remove (still removing the routing entry
unconditionally either way -- the owner is relinquishing regardless, and
routing to a departing owner is dead weight); a failed cancel additionally
pushes the address into the leaked list. No signature or return-value
change -- all 5 call sites keep compiling and behaving unchanged; the leaked
address is reconciled via the shared channel's own state, not by
caller-side handling. `run_j1939_claim_loop` itself is the leaked set's
opportunistic retry point: inside its existing `shared_channels`-held
critical section, between the sibling-ownership check and the native claim
issue for a candidate address, a leaked entry for that exact address gets
one retry-cancel attempt right there (`api` nested under the already-held
`shared_channels`, the same lock order the native issue itself already
uses a few lines later) -- success removes it from the leaked list and
falls through to claim normally; failure skips this candidate (same
drop/warn/advance/continue shape `owned_by_a_live_sibling`'s own skip
already uses) and retries again whenever any CLL's claim loop next
considers this address. This guarantees no native claim is ever issued for
an address whose prior cancel is still unresolved, regardless of which CLL
attempts it next -- the actual invariant this ADR's Decisions 1/2 already
record. Remaining leaked entries are pruned (debug-logged, not retried
further) when the physical channel itself closes (`ref_count == 0`),
mirroring `leaked_repeat_message_ids`'s own same-shaped prune-at-close.

**Rejected alternatives (design-advisor consult):**
- **Retain a failed-cancel entry (the round-20 shape) instead of removing
  and leak-tracking it** -- rejected: retention is inert at teardown (a dead
  CLL's retained entry blocks no one -- `owned_by_a_live_sibling` only ever
  blocks a DIFFERENT, live, generation-matching CLL) and unactionable at 3
  of the 5 call sites; it would also need `owned_by_a_live_sibling`'s
  liveness semantics reversed, permanently wedging an address with no
  cleanup owner at all.
- **Fail the Decision-4 fresh attempt closed on any sweep-cancel failure,
  mirroring round-21's own fail-closed shape** -- rejected as unnecessary
  once the leaked-set retry guard exists (a fresh claim loop can no longer
  double-claim a leaked address regardless -- it must retry-cancel first),
  and it would deny service for a transient native failure; round-21's own
  fail-closed was justified by a live in-flight wait keyed to this exact
  `cll_handle`'s own results map, a condition that does not hold here.
- **A bounded inline cancel-retry loop inside the sweep itself** -- rejected
  for the same ADR-080 reason round-21 already recorded for its own
  sibling-address case (retrying inside a `shared_channels`-held critical
  section against repeated native calls).
- **A new periodic retry duty in `run_due_tick_duties`, mirroring
  `retry_leaked_repeat_message_stops`'s own periodic-adjacent shape** --
  rejected as more surface than needed: the claim loop's own candidate-issue
  point is the only place a leaked J1939 address ever actually matters
  (unlike a repeat-message MsgId, which can matter independent of any claim
  attempt).

**New regression test:**
`tests/grpc_mock/j1939.rs::leaked_claim_cancel_blocks_a_fresh_attempt_until_the_native_cancel_finally_succeeds`.
New mock backdoor `__mock_set_j1939_cancel_error`/`MockBackdoor::
set_j1939_cancel_error`: the cancel form of `IOCTL_PROTECT_J1939_ADDR`
returns a failure without removing the address from the mock's own internal
claimed-address bookkeeping (so the mock's own state genuinely still
reflects "still claimed" after a failed cancel, for test realism). A
three-act test: (1) a CLL claims an address normally; (2) the backdoor
enabled, `CoptStopcomm` then `CoptStartcomm` again triggers Decision 4's
fresh-attempt sweep, whose cancel fails and leaks the address -- the leaked
guard's own retry-cancel (still under the same backdoor) also fails, so the
candidate is skipped and the claim attempt exhausts (`PduErrEvtInitError`);
(3) the backdoor disabled, one more `CoptStartcomm` -- the leaked guard's
retry-cancel now succeeds, the address leaves `leaked_j1939_claims`, and the
claim succeeds normally. Fully synchronous against the mock, no timing
windows involved. Confirmed to catch the regression by temporarily
disabling the leaked-set guard and observing the test fail exactly as
expected (the fresh attempt wrongly re-claims the still-defended address
instead of failing closed -- the two-claimants bug this Decision exists to
close), then restoring it. Full J1939 integration suite is now 60 tests
(same total as Decision 21's own new test -- one round's two fixes, one
shared test-count bump).

**Round-23 correction (Codex review, PR #72; `design-advisor` consult):**
the retry-cancel guard above was keyed to the SPECIFIC candidate address
under consideration -- `if sc.leaked_j1939_claims.contains(&address) { .. }`
sitting between the sibling-ownership check and the native claim issue --
so it only ever intervened when a claim attempt's OWN candidate happened to
equal a leaked address. A CLL whose candidate list is disjoint from every
currently-leaked address (e.g. its own `CP_J1939_PREFERRED_ADDRESS`
reconfigured to a different value between the leak and the next attempt,
Codex's own finding scenario) sailed straight past the guard and issued a
brand-new native claim while an OLDER claim on a DIFFERENT address the
adapter may still be defending sat untouched -- reintroducing the exact
two-claimants failure mode this Decision exists to close, just for a
candidate the guard was never watching.

Fixed by replacing the per-candidate guard with a channel-wide
batch-reconcile-then-gate, run once per claim-loop iteration immediately
after the existing `still_on_this_channel` staleness check and BEFORE the
sibling-ownership check (so it applies uniformly, ahead of any
candidate-specific logic): every address currently in
`SharedChannel::leaked_j1939_claims` gets one retry-cancel attempt right
there; any that still fail leave the list non-empty, and a non-empty list
after the retry fails the WHOLE claim attempt closed (`Exhausted`) --
regardless of whether the CLL's own candidate list ever intersects the
leaked set. `SharedChannel::leaked_j1939_claims` deliberately keeps its
existing plain `Vec<u8>` shape -- no owner/CLL/generation attribution was
added, even though that would make a narrower, candidate-scoped gate
possible -- because attribution would not actually close the gap: a
reconnect under a new `cll_handle` but the SAME NAME (clause 16's
one-NAME-one-address identity) would still evade a per-owner gate, since
NAME is not part of the leak record at all, only the address is. Blocking
channel-wide is the only shape that closes both the disjoint-candidate-list
route this round's finding demonstrated AND the same-NAME-different-handle
route neither the original nor a per-owner-attributed guard would catch.

This trades an accepted, narrower cost: an innocent sibling CLL on the same
physical channel, whose own candidates have nothing to do with the leaked
address, is now also blocked from claiming until the leak reconciles --
whereas the old (broken) per-candidate guard would have let it through.
This is judged acceptable because it only ever applies while the adapter is
already in a degraded state (a native cancel has already failed at least
once and not yet recovered) -- not a cost paid in the ordinary case -- and
failing closed for every claimant on that physical channel until the
adapter's own defended-address bookkeeping is confirmed clear is exactly
this ADR's own standing bias (see Decisions 1/2/22's own reasoning) over
letting a second claimant proceed against unconfirmed adapter state. The
gate retries (rather than permanently latching closed) specifically so a
transient native failure does not wedge the physical channel forever --
the same opportunistic-retry shape Decision 22's own original mechanism
already established, just moved to run unconditionally instead of only
when the current candidate happens to match.

No caller-facing change: `cancel_j1939_claims_for_cll`'s own per-address
cancel-then-leak behavior (Decision 22's own text above) is unchanged, only
the claim loop's own consumption of `leaked_j1939_claims` moved and
widened. No interaction with Decision 21's `Cancelled` handling -- that
Decision's own cancellation-observability wiring is orthogonal to which
candidates this gate blocks.

**New regression test:**
`tests/grpc_mock/j1939.rs::leaked_claim_on_a_disjoint_candidate_list_still_blocks_the_whole_attempt`
proves the exact scenario the old per-candidate guard missed: a CLL claims
0x80, a forced native-cancel failure (`__mock_set_j1939_cancel_error`)
leaks 0x80 on a fresh-attempt relinquish, `CP_J1939_PREFERRED_ADDRESS` is
then reconfigured to 0x81 (disjoint from the leaked 0x80) before the next
`CoptStartcomm` -- proving the new channel-wide gate still fails the whole
attempt closed (`PduErrEvtInitError`, nothing written to the mock) even
though 0x81 was never itself leaked; a third `CoptStartcomm`, with the
cancel-error backdoor cleared, then succeeds once the leaked-set retry
finally reconciles. Confirmed to catch the regression by temporarily
disabling the new gate (`if false && !sc.leaked_j1939_claims.is_empty()`)
and observing the test fail exactly as expected (the disjoint candidate
wrongly claims 0x81 while 0x80 remains leaked), then restoring it. Full
J1939 integration suite is now 61 tests.

**`edge-case-hunter` verification pass on this correction found three
issues, one fixed here, two accepted residuals:**

1. **Fixed:** the new batch-reconcile block is a genuinely new early-return
   point (`return Exhausted` after an `.await` on `ctx.api.lock()`) that had
   no recheck of `cancelled_cops` immediately before returning, unlike
   every other exit in this loop -- a `CancelComPrimitive` landing during
   the retry-cancel `.await` would have been silently swallowed, since the
   caller's own `Exhausted` arm (`events.rs`) never re-consults
   `cancelled_cops` either. Fixed by rechecking `cancelled_cops` for
   `cancel_cop` immediately before the `Exhausted` return, returning
   `Cancelled` instead when a cancellation landed during the retry -- no
   candidate is registered/in-flight yet at this point, so this mirrors the
   outer-loop-top check's own "nothing to clean up" `Cancelled` return
   rather than needing Decision 21's fuller cleanup-block shape. Not
   independently testable via this crate's single-threaded `current_thread`
   test runtime -- the same infeasible-to-construct-deterministically class
   this ADR's own Decisions 1/2 already document (no natural preemption
   point to land a cancellation precisely inside this `.await`); verified
   by code inspection and the full J1939 suite passing unchanged.
2. **Accepted residual:** the batch retry holds `shared_channels` (and,
   nested under it, `api`) across potentially several sequential native
   cancel calls, not the single thin call the ADR-080 "thin, non-blocking
   native primitive" precedent this Decision's own text cites was
   originally measured against -- `leaked_j1939_claims` is channel-wide and
   can in principle accumulate entries from multiple CLLs/reconnects over
   the physical channel's life, unlike `cancel_j1939_claims_for_cll`'s own
   per-call-site `owned` list, which this loop's own invariant (never more
   than one live claim per `cll_handle`) keeps at ≤1 in practice. Accepted
   because the condition under which this list is ever non-empty at all is
   itself already a degraded-adapter state (a native cancel has already
   failed at least once), the same "cost only paid while already degraded,
   not in the ordinary case" framing this Decision's own text already uses
   to justify the channel-wide block radius; the list is bounded to the
   live SAE J1939 address space (≤253 entries) and pruned wholesale at
   `ref_count == 0`, so it cannot grow without bound. Revisit if a future
   round finds this lock-hold duration causing an observed cross-channel
   stall, rather than a theoretical one.
3. **Accepted residual:** the new regression test only exercises the two
   outcome extremes (the retry-cancel wholesale-fails or wholesale-succeeds)
   because `__mock_set_j1939_cancel_error` (Decision 22's own backdoor) is a
   single global bool, not per-address -- it cannot construct a leaked set
   with more than one entry where some cancel and others don't in the same
   retry pass, so the `retain` closure's actual partial-batch behavior is
   unverified by any test. The same "found infeasible via the available
   test surface, documented instead of forcing a misleading test"
   resolution this ADR already uses elsewhere (Decision 6, Decision 8);
   revisit if a per-address cancel-failure backdoor is ever added for
   another reason.

**23. `SharedChannel::j1939_claims` now carries each entry's own SAE J1939
NAME, closing a NAME-collision gap neither the address-keyed map nor any
existing guard ever covered (`design-advisor` consult, Codex review PR
#72; round-25 correction).** `j1939_claims: HashMap<u8, (u32, u64)>`
(address -> owning `cll_handle`/`connect_generation`) tracks ADDRESS
ownership only -- but SAE J1939 clause 16's actual invariant is "one NAME
defends at most one address," a distinct dimension from "one address has
at most one owner." Two findings exposed the gap this left open:

- **Finding 1 (P1):** `owned_by_a_live_sibling` (the claim loop's existing
  sibling-ownership check) only ever compares the CURRENT candidate's own
  address against the map -- two sibling CLLs sharing a `CP_J1939Name` but
  configured with DISJOINT `CP_J1939PreferredAddress` candidate lists could
  each successfully claim a DIFFERENT address, since neither candidate
  collides with the other's entry. The adapter then defends one SAE J1939
  identity at two source addresses simultaneously -- the exact two-
  claimants violation this whole ADR exists to close, reached via a
  dimension (NAME collision) the existing address-keyed check never
  considered.
- **Finding 2 (P2):** `CoptUpdateparam` promoting a DIFFERENT, validly-
  shaped `CP_J1939Name` onto Active while a claim is live was accepted
  outright -- the adapter keeps defending the address under the OLD NAME
  while Active (and a later spontaneous reclaim, which reads Active's NAME
  directly) uses the NEW one, diverging this service's own bookkeeping
  from what the adapter actually defends.

**Schema decision:** `SharedChannel::j1939_claims`'s value type is widened
to a named struct, `J1939ClaimEntry { cll_handle: u32, connect_generation:
u64, name: [u8; 8] }` (`Copy + Eq`, so existing `.copied()`/equality call
sites keep working), rather than adding a second, NAME-keyed map. A
separate map was considered and rejected: it would need a paired
insert/remove kept manually in sync with every one of this map's own
mutation sites (the claim-issue insert, the round-20/21 retained-on-
cancel-failure path, the spontaneous-loss removal, and
`cancel_j1939_claims_for_cll`'s own teardown sweep) -- one missed pairing
would permanently wedge a NAME with no cleanup owner, a new bug class in a
mechanism that already took 24 rounds to stabilize. Widening the existing
map's value keeps one source of truth; every one of its own read/write
sites (`resolve_j1939_claim_indication`, `deliver_j1939_claim_indication`,
`owned_by_a_live_sibling`, the claim-issue insert, the post-wait recheck,
`cancel_j1939_claims_for_cll`'s teardown filter) was updated to destructure
the new struct's fields, with no behavior change to any of them.

**Finding 1's fix:** a new NAME-collision gate in `run_j1939_claim_loop`'s
existing critical section, checked AFTER the round-23/24 leaked-set gate
(a purely local check, no native I/O, cheaper than that gate's own retry-
cancel FFI calls) and BEFORE `owned_by_a_live_sibling` (candidate-
independent -- skipping to the next candidate would just re-hit the
identical collision on every remaining one). Collision means: any entry in
`j1939_claims` with the SAME `name` as this attempt's own `params.name`,
owned by a DIFFERENT, still-live `cll_handle` -- regardless of which
address that entry holds. On collision, the WHOLE attempt fails closed
(`Exhausted`, the leaked-gate's own shape, including its established
`cancelled_cops` recheck immediately before the return, since this check
itself crosses an `.await` to verify the colliding owner's liveness) --
not skip-candidate, since the collision is a property of the attempt's
NAME, not the candidate address. Same-handle entries are excluded (a stale
generation already fails the liveness check regardless), so this can never
self-block the reclaim path or the round-20/21 in-doubt-retained-entry
case.

Deliberately never cancels the colliding sibling's own claim
("relinquish"): no cancel owner in this ADR's invariant ever cancels a
claim a DIFFERENT, still-live CLL is not itself relinquishing -- every
existing owner (Decisions 1/2/4/5/9, the round-24 opt-out branch) only
ever cancels claims its OWN CLL held. Doing so here would reintroduce the
round-1 routing-steal bug class and silently un-defend an address the
sibling may be actively transmitting under. The client-side remedy (the
colliding sibling relinquishes via `CoptStopcomm` or disconnect) is named
in the rejection diagnostic instead.

**Finding 2's fix:** the NAME counterpart of Decision 12's own
`CP_TesterSourceAddress` guard, at both `CoptUpdateparam` call sites --
enqueue-time (`rpc_primitive.rs`) and execution-time (`events.rs`,
Decision 15's own TOCTOU-closing shape). Reject-outright, not cancel-and-
reclaim: mirrors Decision 12's own already-rejected "cancel-and-reclaim"
alternative verbatim -- a NAME-change-triggered cancel-and-reclaim would
mint a new cancel-owner class and a second concurrent entry point into
`run_j1939_claim_loop`. The supported NAME-change path already exists and
needs no new mechanism: `CoptStopcomm` -> restage `CP_J1939Name` ->
`CoptStartcomm` (Decision 4's own fresh-attempt sweep cancels the old-NAME
claim; the fresh loop reads NAME at its own call time). Gated on
`holds_live_j1939_claim` (live `j1939_claims` membership for this exact
`(handle, connect_generation)`), hoisted into ONE shared boolean the
pre-existing `CP_TesterSourceAddress` guard now also consumes -- one
`shared_channels` read, two consumers, instead of two separate
acquisitions computing the identical membership test. Compared against
normalized Active (via the extracted `normalize_j1939_name` helper --
`resolve_j1939_claim_params`'s own pre-existing zero-pad-to-8 rule, now
shared rather than duplicated), not `j1939_claimed_address`, so a byte-
identical restage (re-staging the full ComParam set after a successful
claim) must pass -- the same differs-from-applied shape Decision 12's own
guard already uses. `None` (the ComParam never staged into Working at all)
passes unconditionally; only an actually-staged value -- including an
explicit empty restage, normalized to all-zero -- is compared.

The execution-time re-check closes the identical TOCTOU class Decision 15
already closes for `CP_TesterSourceAddress`/`CP_J1939TargetAddress`: a
NAME-staging `CoptUpdateparam` enqueued before an in-flight claim attempt
completes (so the enqueue-time snapshot sees no live claim yet) landing
AFTER that claim completes. `active` is already the full live `ComParamSet`
captured in the same execution-time critical section the pre-existing
NODE_ADDRESS re-check reuses -- no additional snapshot widening needed for
this one check, unlike the enqueue-time guard, which needed
`update_param_working_snapshot`'s return tuple widened with Active's own
normalized NAME bytes (the same single-acquisition-widening precedent
Decision 7/12's own tuple elements already established).

**Interaction between the two fixes (traced, not assumed):** genuinely
orthogonal, with one directed dependency. Finding 2's own hazard, with
Finding 1's gate in place: a single CLL promotes a new NAME via
`CoptUpdateparam` -- the map holds exactly one entry (the OLD, stored
NAME); no second, same-NAME entry is ever created, so Finding 1's gate
sees nothing at any point -- the divergence is purely Active-vs-adapter,
invisible to any map-based check. Finding 1's own hazard, with Finding 2's
guard in place: two CLLs share a NAME from the start; no `CoptUpdateparam`
is ever involved. Neither fix is redundant given the other. The real
coupling runs the other way: Finding 1's gate compares STORED NAMEs, which
is only sound if a stored NAME can never drift from what the adapter
actually defends -- precisely the invariant Finding 2's guard establishes
(the claim-issue insert records the NAME actually issued; Finding 2's
guard pins Active's NAME for the entry's whole lifetime; a fresh
`CoptStartcomm` re-issue passes through Decision 4's sweep, which removes
the old entry first).

**Rejected alternatives (design-advisor consult):**
- **A separate NAME-keyed map instead of widening `j1939_claims`** --
  rejected: two sources of truth, four paired-removal sites, and cannot
  represent this codebase's own already-legitimate transient states (e.g.
  a round-20 in-doubt retained entry coexisting with a round-22 leak)
  without a silent overwrite.
- **Skip-candidate instead of fail-the-whole-attempt on a NAME collision**
  -- rejected: the collision is candidate-independent, so skipping would
  just re-hit it on every remaining candidate, wasting cursor advances to
  reach the identical `Exhausted` outcome with a misleading cursor trail.
- **Cancelling the colliding sibling's own claim ("relinquish")** --
  rejected: no cancel-owner precedent in this ADR ever cancels a claim a
  different, live CLL is not itself relinquishing; would reintroduce the
  round-1 routing-steal bug class.
- **Cancel-and-reclaim on a NAME change, instead of reject-outright** --
  Decision 12's own already-rejected alternative, verbatim: mints a new
  cancel-owner class and a second concurrent entry point into the claim
  loop.
- **Enqueue-time-only NAME guard, no execution-time re-check** -- rejected
  as leaving a fully constructible Decision-15-class TOCTOU open (proven
  constructible via the `PDU_IOCTL_SUSPEND_TX_QUEUE`/`RESUME_TX_QUEUE`
  determinism trick this ADR's own Decision 15 test already established).

**New regression tests** (all in `tests/grpc_mock/j1939.rs`):
`name_collision_blocks_a_sibling_claiming_a_disjoint_address` (Finding 1 --
two sibling CLLs sharing a NAME with disjoint candidate lists; the second
fails closed while the first survives undisturbed; disconnecting the first
lets the second's retry succeed -- a plain `CoptStopcomm` alone does NOT
relinquish a live claim, ADR-180 Decision 4 only cancels on a fresh
re-`CoptStartcomm` attempt, so the test disconnects instead, Decision 1's
own unconditional teardown cancel).
`coptupdateparam_rejects_j1939_name_off_the_live_claim` /
`coptupdateparam_allows_restaging_the_same_j1939_name_as_the_live_claim`
(Finding 2's enqueue-time guard, positive/negative pair, mirroring Decision
12's own two-test shape).
`coptupdateparam_execution_time_check_catches_a_name_change_that_lands_after_enqueue`
(Finding 2's execution-time re-check, mirroring Decision 15's own
suspend/resume-queue determinism trick) -- this test's own `CoptUpdateparam`
must ALSO stage `CP_TesterSourceAddress` to the address its pending claim
will actually win, or the pre-existing NODE_ADDRESS execution-time check
fires first once the claim's write-back promotes Active in the interim,
masking whether the NAME-specific check under test is itself load-bearing
(confirmed by constructing the test without this staging first and finding
it passed even with the NAME check disabled, tracing the false pass to the
NODE_ADDRESS check firing instead, then fixing the test -- not the guard
logic, which a targeted `false &&` disable/restore on this test's own
final form confirmed is correct). All four confirmed to catch their
respective regressions by temporarily disabling the corresponding gate
condition and observing the correct test fail, then restoring it. Full
J1939 integration suite is now 66 tests.

**Round-26 correction (Codex review, PR #72; `design-advisor` consult):**
Decision 23's own two `CoptUpdateparam` NAME guards, and the pre-existing
spontaneous-reclaim path (`run_j1939_reclaim_duties`), all read Active's own
`CP_J1939Name` as "the NAME this claim defends" -- but a `temp_param_update
= 1` `CoptStartcomm` can claim under a Working-bound (Temp) NAME that never
promotes to Active (the same Temp-bound-field-not-promoted precedent the
round-24 tester-present/header-recompose fixes already established
elsewhere in this file), so all three sites used the wrong reference: a
spontaneous reclaim after a Temp-bound claim silently reclaimed under a
DIFFERENT NAME than the one actually being defended (an unrequested
mid-session identity change, also feeding the wrong candidate NAME into
Decision 23's own collision gate on reclaim), and both `CoptUpdateparam`
guards could wrongly reject a legitimate restage of the actually-claimed
Temp NAME (since it differed from stale Active) while wrongly allowing a
restage that happened to match stale Active (a different NAME than the one
genuinely defended). Root cause: the authoritative NAME (`SharedChannel::
j1939_claims`'s own `J1939ClaimEntry.name`, already correct since round 25)
is deleted from that map at the moment of spontaneous loss, before
`SharedChannel::j1939_reclaim_pending` is armed for the reclaim -- so
neither candidate source of the defended NAME survives that instant, ruling
out simply reading `j1939_claims` more carefully at any of the three sites.
Fixed by widening `j1939_reclaim_pending` from `HashSet<u32>` to
`HashMap<u32, J1939ReclaimPending>` (`{connect_generation, name}`),
captured atomically at the loss (same critical section as the `j1939_claims`
removal, under the pre-existing snapshot recheck) so the defended NAME's
lifetime exactly spans the loss-to-reclaim window with one source of truth
at every instant: the `j1939_claims` entry while a claim is live, the
`j1939_reclaim_pending` entry during the window. `run_j1939_reclaim_duties`
now overrides its rebuilt claim params' NAME with the captured value instead
of Active's, and (bonus correction, same bug class as the round-16/21/22
`connect_generation` fixes) now also filters its drain-time link lookup on
`connect_generation`, closing a hole where a same-handle disconnect+
reconnect during the loss window could otherwise fire an unsolicited
reclaim for the new session. Both `CoptUpdateparam` guards are widened from
a `bool` "holds a live claim" gate to an `Option<[u8; 8]>` "the NAME
currently defended" value -- the matching `j1939_claims` entry's name, or
(new) a matching-generation `j1939_reclaim_pending` entry's -- so a claim
attempt or claim in flight during the loss window is protected the same
way an already-completed claim already was, closing a second gap the same
consult identified: without the pending-fallback, a NAME/`NODE_ADDRESS`
restage landing during that window bypassed the guard and was silently
clobbered once the reclaim's own write-back ran. `update_param_working_
snapshot`'s round-25 8th-tuple-element widening (`active_j1939_name`) is
reverted -- no remaining consumer. Separately, the same verification pass
found `cancel_j1939_claims_for_cll`'s cleanup of `j1939_claim_results`/
`j1939_reclaim_pending` sat after an early return keyed on `j1939_claims`
membership alone, contradicting this function's own doc comment ("removal
... stays unconditional") -- a CLL that had already lost its claim
spontaneously (address already moved out of `j1939_claims` into
`j1939_reclaim_pending`) and then disconnected before the next reclaim
tick left its pending-reclaim entry orphaned. Not a live correctness bug
(the next `run_j1939_reclaim_duties` drain already discards it via the
just-added `connect_generation` filter, and every consumer of the map
compares the entry's OWN generation field regardless), but a real,
avoidable leak the doc comment already claimed didn't happen -- the two
removes are moved above the early return so that claim is actually true.
**New regression tests** (`tests/grpc_mock/j1939.rs`):
`spontaneous_reclaim_after_a_temp_bound_claim_uses_the_temp_bound_name_not_active`
proves the reclaim path carries the Temp-bound NAME forward instead of
Active's;
`coptupdateparam_j1939_name_guard_compares_against_the_temp_bound_claimed_name_not_active`
proves the enqueue-time guard allows restaging the actually-claimed
Temp-bound NAME and rejects restaging Active's own (never-claimed) NAME --
the exact false-reject/false-allow pair this correction closes. Both
confirmed to catch their respective regressions by temporarily
short-circuiting the corresponding fix and observing the correct test fail,
then restoring it. Full J1939 integration suite is now 68 tests.
**Round-26 `edge-case-hunter` residual: neither new test exercises the two
guards' own `j1939_reclaim_pending`-fallback branch specifically** -- i.e. a
`CoptUpdateparam` landing during the (real, ~`POLL_INTERVAL_MS`-wide, not
the razor-thin drain-to-insert sub-gap noted below) window between a
spontaneous loss and the next reclaim-duties drain, where only the pending
entry (not a `j1939_claims` entry) is available for the guard to consult.
Unlike this ADR's usual "provably infeasible on this crate's single-threaded
runtime" residuals, this window is plausibly constructible (this crate has
no existing `tokio::time::pause`-based test, and introducing one is a
testing-infrastructure decision broader than this one correction), so it is
recorded here as a deliberately deferred coverage gap rather than a proven-
infeasible one; the fallback branch itself was verified correct by code
inspection (structurally identical to the already-tested `j1939_claims`
branch, differing only in which map is consulted) and by hand-tracing both
new tests' assertions against the pre-fix comparison logic. A second,
narrower residual (noted by the same consult that approved this fix): the
`.await` gap between `j1939_reclaim_pending.drain()` and the claim loop's
own registration insert, where neither gate is armed at all -- but the
reclaim itself still uses the captured (correct) NAME regardless, so a
restage landing there only diverges Active, never the actual defended
identity, the same accepted state the Temp-bound case already establishes;
this one IS the ADR's usual infeasible-on-this-runtime class, per the
round-16/21/22 precedent, verified by code inspection and the full suite
passing unchanged.

**Round-27 correction to Decision 22 (Codex review, PR #72; `design-advisor`
consult): `ioctl_start_repeat_message`'s own J1939 gate only ever covered
THIS CLL's own claim posture, never `SharedChannel::leaked_j1939_claims`.**
`rpc_misc.rs::ioctl_start_repeat_message` gates a SAE J2534-2 clause 14
repeat-slot START on `events::j1939_negotiated_unclaimed(link)` -- but that
predicate is scoped entirely to the CALLING CLL's own negotiation posture
and claimed-address state. A CLL that structurally opted out of negotiation
(`CP_J1939AddressNegotiationRule` bit 1 set) or manages its own source
address (ADR-180 Decision 12's own precedent) never runs the claim loop at
all and `j1939_claimed_address` stays `None` by design, so it sails straight
past that gate regardless of channel state. `SharedChannel::
leaked_j1939_claims` (Decision 22) can independently hold an address a
DIFFERENT, already-torn-down sibling CLL failed to natively cancel -- the
leak belongs to the physical channel, not to any one CLL's own negotiation
posture -- so an opted-out CLL could start an autonomous device-side repeat
slot while the adapter might still be defending that leaked address on the
SAME physical channel, the exact two-claimants-on-one-resource hazard this
whole ADR exists to close, just reached through a call site round-23/24's
own reconcile-then-gate never covered.

Fixed by adding a THIRD call site of the shared `reconcile_leaked_j1939_
claims` helper (`events_j1939_claim.rs`, round-23's own extraction), inside
`ioctl_start_repeat_message` -- placed after the existing
`LOCK_PHYSICAL_TX_QUEUE` holder check and before the pre-existing
`retry_leaked_repeat_message_stops` opportunistic retry, where `shared_
channels` is already held (this function's own documented lock order) and
`logical_links` is not. Unconditional regardless of this CLL's own J1939
negotiation posture -- the leaked claim belongs to the physical channel, not
to this CLL, so no additional `is_j1939_protocol_id`/`j1939_negotiated_
unclaimed` scoping is needed (a non-J1939 channel's `leaked_j1939_claims` is
always empty by construction, so the emptiness check alone already scopes
it correctly). A non-empty list that fails to fully reconcile fails the
START closed (`FailedPrecondition`/`PduErrCllNotStarted`), mirroring the
claim loop's own `Exhausted` and the opt-out branch's own failed-StartComm
shape. If the physical channel is no longer found in `shared_channels`
(concurrent teardown), the check is skipped silently, mirroring `events.rs`'s
own identical-shaped opt-out-branch call site (teardown already owns cleanup
for a vanished channel).

`reconcile_leaked_j1939_claims`'s own visibility is widened from `pub(super)`
(visible only within the `events` module) to `pub(in crate::service)`
(mirroring `cancel_j1939_claims_for_cll`'s own wider visibility) -- its first
two call sites both lived inside `events`/`events_j1939_claim.rs`, so
`pub(super)` sufficed until this round's third call site, outside that
module entirely.

**Rejected alternatives (design-advisor consult):**
- **Scope the new check to negotiation-enabled CLLs only, mirroring the
  pre-existing `j1939_negotiated_unclaimed` gate's own scoping** -- rejected:
  this is precisely the finding itself -- an opted-out CLL is exactly the
  case the pre-existing gate cannot reach, and the leaked address's hazard
  is unrelated to which CLL is asking.
- **A per-candidate/per-address guard instead of the channel-wide
  reconcile-then-gate** -- rejected for the identical reason round-23
  rejected it for the claim loop and round-24 rejected it for the opt-out
  branch: `leaked_j1939_claims` carries no NAME/owner attribution, so a
  narrower guard would miss a same-NAME reconnect under a different
  `cll_handle`.

**New regression test:**
`tests/grpc_mock/j1939.rs::leaked_claim_blocks_repeat_message_start_on_an_opted_out_sibling_cll`.
Mirrors `leaked_claim_cancel_blocks_a_fresh_attempt_until_the_native_cancel_
finally_succeeds`'s own `__mock_set_j1939_cancel_error` leak-seeding shape:
CLL A claims 0x80, the backdoor armed, a StopComm+fresh-StartComm cycle on
CLL A leaks 0x80 into `leaked_j1939_claims` (both the fresh-attempt sweep's
own cancel and the claim loop's own leaked-set retry fail while the backdoor
stays armed). An opted-out sibling CLL B on the SAME physical channel (which
never calls `CoptStartcomm` at all, so the pre-existing gate can never fire
for it) then attempts `PDU_IOCTL_START_REPEAT_MESSAGE` -- proves it is
rejected (`FailedPrecondition`, message naming the leaked claim) while
0x80's leak persists. The backdoor cleared and the START retried proves it
now succeeds once the reconcile finally clears -- a successful START is only
reachable when `reconcile_leaked_j1939_claims` returned `true` (its own
contract, "the list is empty once the retry completes"), positively proving
`leaked_j1939_claims` is empty afterward. Confirmed to catch the regression
by temporarily short-circuiting the new gate's condition (`if false && ...`)
and observing the test fail exactly as expected (CLL B's first attempt
wrongly succeeds instead of being rejected), then restoring it. Full J1939
integration suite is now 69 tests.

**24. A spontaneous J1939 reclaim now observes a pending `CoptStopcomm` and
aborts before issuing any native claim, instead of only ever observing an
in-flight COP's own `CancelComPrimitive` (Codex review, PR #72; `design-
advisor` consult).** `run_j1939_reclaim_duties` always drives `run_j1939_
claim_loop` with `cancel_cop: None` -- no COP drives a spontaneous reclaim,
so Decision 21's own `cancelled_cops` cancellation check (which only fires
when `cancel_cop` is `Some`) can never apply there. But a `PDU_COPT_STOPCOMM`
against the same CLL sets `LogicalLinkState::stop_comm_pending = true`
synchronously (`rpc_primitive.rs`) regardless of whether a reclaim is
in-flight, and since this reclaim loop runs ON the same physical channel's
single poll task (via `run_due_tick_duties`), the queued StopComm work
cannot actually execute until the reclaim either succeeds or exhausts its
entire candidate list -- a potentially multi-minute delay against an
unresponsive adapter, for a client that has already asked to end the whole
comm session.

Fixed by a new `J1939ClaimLoopOutcome::StopCommPending` variant (deliberately
NOT a reuse of `Cancelled` or `Stale`, and deliberately siblings this
Decision to Decision 21 rather than amending it -- see the rationale
paragraph below) and a new `LogicalLinkState::stop_comm_pending` read, folded
into the SAME `(was_cancelled, is_stale)` tuple check `run_j1939_claim_loop`
already performs at both the outer-loop top and the inner wait loop's own
per-tick check (Decision 21's own idiom), now a three-way `(was_cancelled,
is_stale, stop_comm_pending)` tuple. Checked UNCONDITIONALLY at both sites,
with no scoping parameter distinguishing `handle_start_comm`'s own
`Some(cop_handle)` call site from `run_j1939_reclaim_duties`'s own `None`
one: `stop_comm_pending` is only ever set while `comm_started == true`, and
`PDU_COPT_STARTCOMM` is rejected while `comm_started` is already `true`, so
during an initial-claim loop driven by `Some(cop_handle)` the flag is
provably `false` at every check this loop performs -- an unconditional check
avoids adding a scoping parameter and stays correct even if that invariant
ever changes. Checked in priority order cancelled -> stale -> stop-pending
(the first true wins), matching the existing checks' own ordering
convention.

At the outer-loop top (nothing registered/in-flight yet, mirroring the
existing `was_cancelled`/`is_stale` early returns), a `stop_comm_pending`
observation `return`s `StopCommPending` directly. Inside the inner wait
loop, a new `WaitEnd::StopComm` variant is folded into the SAME cleanup
block the pre-existing `TimedOut`/`Cancelled` cases already share:
best-effort native cancel of the in-flight candidate, retaining the local
`j1939_claims` entry on a failed cancel (mirroring `Cancelled`'s own
established handling -- the retained-on-failure entry becomes the in-doubt
record, reconciled later by the next StartComm's Decision-4 sweep or
teardown's Decision-1 sweep) and dropping any raced, unconsumed `j1939_claim_
results` entry the same way `Cancelled` already does, then returning
`StopCommPending` UNCONDITIONALLY regardless of whether the native cancel
itself succeeded or failed -- matching `Cancelled`'s own precedent at its
equivalent final-return point.

`run_j1939_reclaim_duties`'s own `match` on the loop's outcome gains a new
`StopCommPending` arm. No error event is emitted -- this is not a claim
failure; the client is intentionally ending the comm session via
`CoptStopcomm`, which is exactly what this reclaim exists to make way for.
Mirrors the pre-existing `Exhausted` arm's own
`still_on_this_channel`-guarded repeat-slot sweep (any repeat slot a client
raced into existence during this attempt's own pending window needs the
same cleanup an exhausted reclaim already gives it) but omits `Exhausted`'s
own `send_error_event` call.

**Round-27 own-round correction (`edge-case-hunter` finding on the
just-written diff, repro-confirmed, `design-advisor` consult): the original
text above treated this abort as final, reasoning the CLL was
"losing/never-regaining its claimed address either way" since its
`j1939_reclaim_pending` entry was already drained at the top of this
function's own loop before any outcome is known. That reasoning is wrong --
`stop_comm_pending` is only PROVISIONAL until `handle_stop_comm` actually
runs the queued StopComm. `CancelComPrimitive` against a held StopComm,
`PDU_IOCTL_CLEAR_TX_QUEUE`/`PDU_IOCTL_RESET_CHANNEL` clearing the queue, or
a malformed-`cop_data` rollback can all revert `stop_comm_pending` back to
`false` without the StopComm ever executing, and none of those paths
re-arms this CLL's lost claim on their own -- so the original text let a
client legally cancel/clear the queued StopComm and permanently lose the
CLL's J1939 address, with no retry and no client-visible error.**

Fixed by having this arm re-insert `cll_handle`'s
`SharedChannel::j1939_reclaim_pending` entry (via
`.entry(cll_handle).or_insert(..)`, not a plain overwrite -- defensive
against a differently-sourced arming of the same CLL landing here first;
see the new accepted residual below for why no such source is currently
constructible) whenever `still_on_this_channel`, carrying the same
`connect_generation` and the same loss-time-captured NAME
(`params.name`, still holding `pending_entry.name` from the round-26
override earlier in this same loop iteration) the aborted attempt itself
used. This turns the abort into a pause: a later `run_j1939_reclaim_duties`
call -- fired every `POLL_INTERVAL_MS` via `run_due_tick_duties`, the same
channel poll task's own periodic per-tick duties -- picks the re-armed
entry back up and retries the reclaim.

1. **Why per-tick re-arm cannot recreate the starvation this Decision
   exists to prevent** (traced against the actual code, not just asserted):
   - A normally-queued (not suspended) StopComm: `poll_channel_events`'s own
     `tokio::select! { biased; .. item = tx_rx.recv() => .. , _ =
     tokio::time::sleep_until(next_tick) => {} }` (`events.rs`) lists the
     `tx_rx.recv()` arm ahead of the `sleep_until(next_tick)` arm. A
     `biased` `select!` polls its arms in listed order and resolves with the
     first one found ready, so once the client's `TxItem::StopComm` is
     sitting in `tx_rx`, the very next iteration of this loop dispatches it
     (running `handle_stop_comm`, which clears `stop_comm_pending`) as soon
     as that iteration's `select!` is polled -- it does not wait for
     `next_tick`'s own deadline to elapse first, and `run_due_tick_duties`
     (where this reclaim's own retry lives) only runs once per outer-loop
     iteration, after the `select!` resolves. So on THIS channel's own poll
     task, at most one abort/re-arm cycle happens before the queued
     StopComm is dispatched and the retry can proceed cleanly -- this
     bound is about ordering WITHIN the single poll task's own loop
     iterations, not a claim that the RPC handler's own hand-off (`stop_
     comm_pending = true` in `rpc_primitive.rs`, then two more `.await`s
     before the item actually lands in `tx_rx`) is atomic with respect to
     it: this service runs on `#[tokio::main]`'s default multi-threaded
     executor (`main.rs`), so the RPC handler's thread and this channel's
     poll-task thread genuinely run concurrently, and a real (if narrow)
     window exists for more than one due-tick to observe `stop_comm_
     pending == true` before the queued item is actually visible in
     `tx_rx`. This does not change the safety argument -- each such cycle
     is still bounded by the identical "few mutex acquisitions, no native
     call" cost the suspended-queue case below establishes -- but the
     precise "at most one cycle" framing describes the poll task's own
     internal ordering, not a cross-thread atomicity guarantee this
     codebase does not make.
   - A StopComm pinned behind `PDU_IOCTL_SUSPEND_TX_QUEUE` (client
     deliberately holding the queue open, siphoned into `tx_held` instead of
     `tx_rx` while suspended): each retry re-enters `run_j1939_claim_loop`
     and aborts at its outer-loop-top check (`events_j1939_claim.rs`, the
     `logical_links.lock().await` read of `cancelled_cops`/staleness/
     `stop_comm_pending` folded into one acquisition, before the candidate
     cursor is even read) -- no native call, no bounded wait. Each
     abort/re-arm cycle therefore costs only a couple of mutex acquisitions
     and an idempotent, already-empty repeat-slot sweep, not the
     `candidate_count * CP_J1939AddrClaimTimeout` physical-channel
     monopolization this Decision exists to prevent -- it just repeats,
     harmlessly, every `POLL_INTERVAL_MS` until the client either resumes
     the queue (letting the StopComm dispatch) or cancels it (reverting
     `stop_comm_pending`, letting the very next tick's retry actually claim).

2. **Why the retry is NOT gated on `comm_started`:** `deliver_j1939_claim_
   indication`'s own spontaneous-loss arming of this exact
   `j1939_reclaim_pending` entry (earlier in this same file) has no such
   gate either, and Decision 4's own established invariant is that
   `CoptStopcomm` does not relinquish a live claim. Adding a `comm_started`
   gate here would not close a gap -- it would open a NEW, narrower one: a
   loss landing just before a stop wouldn't retry, while one landing just
   after would, the same class of abandonment this correction exists to
   close, just on a smaller trigger window. This is a deliberate design
   choice with a real, reachable consequence worth stating explicitly
   (`edge-case-hunter` finding, round-27 verification pass): the re-armed
   entry can outlive a StopComm that completes NORMALLY, not only one that
   is later cancelled -- `handle_stop_comm`'s ordinary completion path
   clears `comm_started`/`stop_comm_pending` but never touches `SharedChannel::
   j1939_reclaim_pending`, so a due-tick firing after that completion still
   picks the entry up and issues a genuine native `PROTECT_J1939_ADDR`
   claim, with its own `NODE_ADDRESS`/`j1939_claimed_address` write-back,
   for a CLL whose comm session the client already, successfully stopped.
   This is the SAME behavior point 2's own reasoning already establishes
   (a stopped CLL's lost address is reclaimed today regardless of stop
   state, matching `deliver_j1939_claim_indication`'s own no-gate
   precedent) -- not a new gap this correction introduces -- but it is
   worth naming directly rather than leaving it as an unstated consequence
   of "not gated on `comm_started`": autonomous bus traffic and claim-state
   writeback after an explicit, completed `CoptStopcomm` is a real,
   client-visible effect, even though it is the intended one.

3. **New accepted residual:** the narrow window between this function's own
   top-of-loop `j1939_reclaim_pending.drain()` and this arm's own
   `or_insert`, where a DIFFERENT arming of the same CLL's entry (if one
   existed) could theoretically race with this one. Not currently
   constructible: the sole arming site,
   `deliver_j1939_claim_indication`'s spontaneous-loss branch, requires
   `j1939_claimed_address == Some(address)` for this CLL, which stays
   `None` throughout this whole reclaim/abort window (the reclaim never
   succeeded), and every mutator this analysis depends on
   (`run_j1939_reclaim_duties`, `deliver_j1939_claim_indication`, `handle_
   stop_comm`) runs on this same physical channel's single poll task, so
   there is no concurrent execution to interleave. Same
   infeasible-to-construct-deterministically class as the round-16/21/22
   `connect_generation` corrections and this ADR's own `Exhausted` arm
   residual (Consequences, below) -- documented rather than left as an
   unstated invariant.

`handle_start_comm`'s own exhaustive `match` on `J1939ClaimLoopOutcome`
(its initial-claim call site, always `Some(cop_handle)`) gains a
`StopCommPending => unreachable!(..)` arm, mirroring `run_j1939_reclaim_
duties`'s own existing `Cancelled => unreachable!(..)` arm for the identical
"provably impossible at this specific call site" reason (see the
`comm_started`/`stop_comm_pending` invariant above) -- this codebase's own
established convention for a genuinely-unreachable-today match arm on this
exact enum, rather than a defensive fallback.

**Rationale for a new variant, not reusing `Cancelled` or `Stale`:**
`Cancelled` is COP-scoped -- an explicit client `CancelComPrimitive` against
the driving COP, reported back as `PduCopstCancelled` for that exact COP --
and its `handle_start_comm` call site emits that status; a spontaneous
reclaim has no driving COP to report it against, and reusing `Cancelled`
would need `run_j1939_reclaim_duties`'s own `match` to fabricate a COP
identity that does not exist. `Stale`'s own inner-wait-loop return path
skips cancelling the in-flight native candidate entirely -- correct there
only because the CLL itself is genuinely stale/torn-down and teardown
already owns cleanup for it, which does not hold here: the CLL is still
fully live, just intentionally ending its comm session in an orderly way,
so the in-flight candidate genuinely needs the same best-effort native
cancel `Cancelled`/`TimedOut` already perform, not `Stale`'s skip-cancel
shape. `StopCommPending` therefore siblings `Cancelled` (Decision 21) as a
companion mechanism -- COP-scoped vs. this Decision's own session-teardown-
scoped trigger -- rather than amending or superseding it.

**No coordination needed with `handle_stop_comm`** (round-27
`edge-case-hunter` correction to the design-advisor consult's own overstated
claim): `handle_stop_comm` (`events.rs`) never touches `SharedChannel::
j1939_claims` -- but it DOES read `LogicalLinkState::j1939_claimed_address`,
via Decision 19's own pre-existing transmit-time drift check. This is
benign here, not a live conflict: post-abort, `j1939_claimed_address` stays
`None` (this reclaim never succeeded), so Decision 19's check reads that
`None` the same way it already does for any never-yet-claimed or
still-unclaimed CLL -- no special-casing needed. Post-abort state
(`j1939_claimed_address` stays `None`, cursor un-advanced) is safe as-is:
the next `CoptStartcomm` naturally resets the
cursor to `0` per Decision 4's own existing "fresh attempt" logic.

**Rejected alternatives (design-advisor consult):**
- **Reuse `Cancelled`, threading a synthetic/`None` `cop_handle` through the
  existing arm** -- rejected: `handle_start_comm`'s own `Cancelled` arm
  unconditionally reports `PduCopstCancelled` for a specific COP, which does
  not exist for a spontaneous reclaim; would need its own new branching
  inside an arm that is otherwise COP-identity-agnostic today, for no
  benefit over a dedicated variant.
- **Reuse `Stale`** -- rejected: `Stale`'s inner-wait-loop return skips the
  in-flight candidate's own native cancel, which is only safe when the CLL
  itself is torn down; a live CLL merely ending its comm session still needs
  the candidate cancelled, the same obligation `Cancelled`/`TimedOut` already
  discharge.
- **Scope the `stop_comm_pending` check to the `cancel_cop: None` call site
  only, via a boolean parameter** -- rejected as unnecessary complexity: the
  `comm_started`/`stop_comm_pending` invariant already makes the flag
  provably `false` at the `Some(cop_handle)` call site, so an unconditional
  check is both simpler and stays correct even if a future call site someday
  passes both `Some(cop_handle)` and observes a genuine `stop_comm_pending`.

**New regression test:**
`tests/grpc_mock/j1939.rs::spontaneous_reclaim_aborts_on_a_pending_stopcomm_without_issuing_a_claim`.
CLL A claims 0x80 normally; its TX queue is suspended
(`PDU_IOCTL_SUSPEND_TX_QUEUE`, this file's own established determinism
trick) before an empty-`cop_data` `CoptStopcomm` is issued -- the RPC's own
critical section sets `stop_comm_pending = true` synchronously, but the
queued `TxItem::StopComm` is unconditionally siphoned into `tx_held` while
suspended (never dispatched), so `handle_stop_comm` never runs and `stop_
comm_pending` stays pinned `true` indefinitely until resumed. A synthesized
`RX_FLAG_J1939_ADDRESS_LOST` for 0x80 then arms `SharedChannel::j1939_
reclaim_pending` for CLL A (`poll_rx` is channel-wide and unaffected by
CLL A's own TX suspension). Since this harness has no direct call-count on
`IOCTL_PROTECT_J1939_ADDR`'s claim form, "no native claim was issued" is
proven the same indirect sibling-block way several other Decision 22/23
regression tests in this file already do: sibling CLL C, whose own single
candidate is the exact address CLL A had claimed, attempts its own claim
afterward -- blocked and exhausting if CLL A's aborted reclaim had wrongly
registered 0x80 (the bug this fix closes), succeeding cleanly if not.
Confirmed to catch the regression by temporarily disabling BOTH of the new
`stop_comm_pending` checks together (the outer-loop-top `return` and the
inner wait loop's own `WaitEnd::StopComm` break) and observing the test fail
exactly as expected (CLL C's probe blocks and exhausts instead), then
restoring both. **Accepted residual, the same class Decision 21's own text
already documents for its sibling `Cancelled` checkpoint:** disabling ONLY
the outer-loop-top check does not fail this test -- the inner wait loop's
own per-tick check (still active) independently cancels and removes the one
candidate CLL A's single-candidate list ever registers, before CLL C's own
probe runs, so this test does not independently pin the outer checkpoint's
own individual contribution, the identical "no natural preemption point in
this harness to land the precondition strictly between one candidate's
resolution and the next one's native issue" residual Decision 21's own text
already accepts. Full J1939 integration suite is now 70 tests.

**Round-27 own-round correction's own new regression test:**
`tests/grpc_mock/j1939.rs::spontaneous_reclaim_resumes_after_a_cancelled_stopcomm_reverts_stop_comm_pending`.
Same setup as the sibling test just above (CLL A claims 0x80, suspends its
TX queue, issues an empty-`cop_data` `CoptStopcomm` to pin `stop_comm_
pending = true`, then a synthesized `RX_FLAG_J1939_ADDRESS_LOST` arms a
reclaim a due-tick aborts), but this test then additionally cancels the
held `CoptStopcomm` directly via `CancelComPrimitive` on its own
`cop_handle` -- reverting `stop_comm_pending` to `false` without the
StopComm ever running -- resumes the TX queue, and lets the next due-tick
retry. Proven via the same indirect sibling-probe idiom, with the INVERSE
expectation from the sibling test above: CLL C's own single-candidate claim
for 0x80 must now FAIL (blocked/exhausted), since CLL A's retried reclaim
re-claims 0x80 first if (and only if) the re-arm fix is present. Confirmed
to catch the regression by temporarily disabling the re-arm insert and
observing this test fail exactly as expected (CLL C's probe wrongly
succeeds, since CLL A's reclaim was never retried and 0x80 was never
re-claimed), then restoring it. Full J1939 integration suite is now 71
tests.

## Consequences

- Codex round 4 also found the inner wait loop never ran this physical channel's periodic per-tick maintenance (detached-registrant reap, sibling tester-present dispatch) on each poll — unlike every other long-running wait loop in this file (e.g. `events.rs::isotp_send`'s N_Bs wait) — so a claim attempt's bounded wait (up to `CP_J1939AddrClaimTimeout` PER candidate) could starve sibling CLL duties for up to `candidate_count * timeout`, since this channel's single poll task never returns to `run_due_tick_duties` while the inner wait loop is sleeping. Fixed by calling the same `run_detached_registrant_maintenance`/`dispatch_due_tester_present` pair `isotp_send`'s own wait loop calls, at each poll tick inside the wait loop (no `Box::pin` needed here, unlike that site — `dispatch_due_tester_present`'s own call graph never reaches back into this claim loop, so there is no recursion cycle to break).
- The same `edge-case-hunter` pass additionally found and fixed three narrower gaps in this mechanism, all mechanical guards mirroring this file's own existing patterns rather than new design decisions, so they are recorded here rather than as their own Decision items: (1) `handle_start_comm`'s J1939 `Exhausted` arm was missing the same `still_on_this_channel` staleness recheck its own sibling `Stale`/`HardError` arms and the analogous K-line `Err` arm already have, so a disconnect racing during claim exhaustion could emit a duplicate terminal status (`PduErrEvtInitError`+`PduCopstFinished`) for a COP that had already been cancelled — now guarded the same way, emitting `PduCopstCancelled` instead when stale. (2) `run_j1939_reclaim_duties`'s own `Exhausted` arm had the same gap (lower severity, since `cop_handle = None` there rules out a terminal-status contradiction, but a stale `cll_handle` reused for a new `connect_generation`, ADR-086, could still receive a spurious `last_error`/event meant for the old connection) — now guarded the same way. (3) The timeout-without-indication cancel block in the inner wait loop now reverifies `SharedChannel::j1939_claims` still maps `address` to this exact `(cll_handle, connect_generation)` immediately before cancelling/removing it, mirroring `deliver_j1939_claim_indication`'s own recheck elsewhere in this file — not currently reachable (every mutator of a DIFFERENT CLL's entry in this map only ever runs from this same physical channel's single poll task, which cannot interleave with this loop's own execution), but this keeps that safety property an explicit, checked condition rather than an unstated concurrency invariant a future refactor could silently break.
- No dedicated regression test exists for Decision 4's own cancel call succeeding under a genuine race (e.g. a disconnect landing mid-cancel) — the same infeasible-to-construct-deterministically class noted below for Decisions 1/2, and for the same reason. Decision 4's *effect*, however, IS independently testable and deterministic (unlike Decisions 1/2's pure race-window fixes): a new regression test, `tests/grpc_mock/j1939.rs::restarting_after_stopcomm_frees_the_previously_claimed_address_for_a_sibling`, proves a sibling CLL can claim the address a CLL released via `StopComm`+`CP_J1939PreferredAddress` change, which is only possible once the stale entry has actually been cancelled and deregistered.
- Decision 5's effect IS independently testable and deterministic, unlike Decisions 1/2's own pure race-window fixes — the mock's `__mock_set_j1939_claim_lost` toggle is global and cannot synthesize a per-address spontaneous loss, but the RxStatus bits `deliver_j1939_claim_indication` actually consumes (`RX_FLAG_J1939_ADDRESS_LOST`, `events.rs::RX_J1939_ADDRESS_LOST`, `0x0002_0000`) can be injected directly via `MockBackdoor::inject_rx_with_status` with the target address as `Data[0]`, entirely independent of any in-flight `protect_j1939_addr` wait. New regression test: `tests/grpc_mock/j1939.rs::spontaneous_loss_frees_the_lost_address_for_a_sibling_after_an_earlier_candidate_reclaims` — a CLL claims a later candidate (its earlier one owned by a sibling at claim time), that sibling then disconnects (freeing the earlier candidate), a synthesized spontaneous loss for the later (currently-claimed) address arms a fresh reclaim that lands on the now-free earlier candidate instead, and a THIRD CLL then proves the originally-lost, later address is claimable again — possible only if Decision 5's removal actually ran. Confirmed to catch the regression by temporarily reverting the fix and observing the test fail (`0x80`/`0x81`-style source-address mismatch), then restoring it.
- Round 2's now-superseded fix (a staleness recheck under a separate `logical_links` acquisition, immediately before a separately-acquired `shared_channels` registration write) is removed; Decision 1 replaces it, not narrows it further.
- `handle_channel_hard_error` clears a channel's `channel_id` without holding `shared_channels` — an accepted residual scoped narrowly: Decision 1's atomicity guarantee is specifically against the disconnect/destroy race it was designed for (both of which do hold `shared_channels` throughout, per ADR-080), not against a hard-error teardown, which destroys the whole physical channel and every CLL's claim state on it wholesale — there is no partial/orphaned state a hard error could leave behind for this mechanism to worry about.
- No dedicated regression test exists for Decisions 1 or 2's own race windows — the same infeasible-to-construct-deterministically class as this codebase's other documented disconnect-race windows (e.g. `deliver_j1939_claim_indication`'s own edge-case-hunter fix): this crate's single-threaded `current_thread` `#[tokio::test]` runtime resolves an uncontended `.lock().await` without a genuine `Pending`-yielding point, so there is no natural preemption opportunity to land a `Disconnect` precisely inside either critical section. Both were verified correct by code inspection and by confirming the full J1939 integration suite still passes unchanged. Decision 3 (the optional-message fix) IS independently testable and deterministic — a new regression test, `tests/grpc_mock/j1939.rs::optional_startcomm_message_sends_with_the_claimed_source_address`, proves the recomposed header reaches the wire with the claimed address rather than the pre-claim default.
- Decision 3 above closes the optional-CoptStartcomm-message scope decision this ADR's own predecessor, ADR-179 Decision 3, originally deferred.
- **Decision 6's two code paths are currently unreachable via the live RPC surface.** `comparam_support::is_j1939_param` (J1939's closed ComParam allowlist) does not include `CP_TesterPresentHandling`/`CP_TesterPresentMessage`/`CP_TesterPresentTime`/`CP_TesterPresentSendType` at all, so `SetComParam` rejects every attempt to configure them on a J1939 CLL with `PDU_ERR_COMPARAM_NOT_SUPPORTED` (confirmed directly while writing this Decision's own regression test: an early draft that called `SetComParam(CP_TesterPresentTime, ...)` on a J1939 CLL failed exactly that way), and `comparam_defaults.rs::j1939_can_common` never seeds `PARAM_TESTER_PRESENT_HANDLING` either, so `ComParamSet::tester_present_handling()` always falls back to its own `0` (disabled) default. `resolve_tester_present` therefore always short-circuits at its very first check for a J1939 CLL, so `tester_present_data`/a live `Armed` state can never actually exist for one today. Both fixes are still correct and are kept as a defensive no-regression measure (should this allowlist ever be widened to admit tester-present for J1939), but no regression test exists for either — the same "found unreachable while writing the test, documented instead of adding a misleading one" resolution this codebase used for the `PARAM_J1939_SOURCE_ADDRESS`-based routing gap, which was open for the same reason at the time but has since been closed by ADR-184's `CllRxEntry::unique_resp_ids`/`unique_id_params` work.
- Decision 6 aside, round 6 also fixed an unrelated, purely mechanical gap on the same PR: `CP_J1939TargetAddress` (a one-byte wire field) had no upper-range validation beyond the pre-existing `0xFFFF` sentinel check, silently truncating an out-of-range value (e.g. `0x100`) at cast time rather than rejecting it — fixed in `rpc_primitive.rs`, mirroring the sentinel check's own synchronous-rejection shape. No Decision item for this one (a straightforward validation fix with no design alternative, CLAUDE.md's "ADR not needed" criteria). New regression test: `tests/grpc_mock/j1939.rs::target_address_over_one_byte_rejects_startcomprimitive_synchronously`.
- Decision 7's effect IS independently testable and deterministic: three regression tests in `tests/grpc_mock/j1939.rs`. `coptupdateparam_rejects_target_address_over_one_byte` proves the unconditional one-byte-range rejection on a connected-but-not-yet-started CLL (the range check does not depend on `comm_started`). `coptupdateparam_rejects_target_address_sentinel` starts the CLL first (via a no-negotiation `CoptStartcomm` so `comm_started` becomes `true` without requiring an actual address claim), then proves the sentinel rejection fires only once genuinely started. `coptupdateparam_before_startcomm_with_unrelated_param_is_not_rejected_by_the_sentinel_guard` is the negative counterpart the `edge-case-hunter` finding above (Decision 7's own text) demanded: an unrelated `CoptUpdateparam` on a not-yet-started CLL, with `CP_J1939TargetAddress` still at its legitimate `0xFFFF` default, must succeed — confirmed to catch the regression the initial (unconditional) version of the fix introduced, since that version failed this exact test.
- **No dedicated regression test exists for Decision 8's reserved-candidate (254/255) skip.** Confirmed by reading the mock's `IOCTL_PROTECT_J1939_ADDR` handler directly (`j2534-0404-mock/src/lib.rs`) that native-side rejection of 254/255 is synchronous and has zero observable side effects (no partial state, no event, no wire traffic) — so "the loop skips the candidate locally" and "the loop issues the claim and the adapter rejects it" are indistinguishable from anything this test harness can observe. The same "found unreachable/indistinguishable via the available test surface, documented instead of forcing a misleading test" resolution already used for Decision 6's tester-present gap and the pre-existing `PARAM_J1939_SOURCE_ADDRESS` routing gap.
- Round 8 also fixed two unrelated, purely mechanical gaps on the same PR (no Decision item for either — straightforward validation fixes with no design alternative, CLAUDE.md's "ADR not needed" criteria): `CP_TesterSourceAddress`'s (`NODE_ADDRESS`) newly-client-writable J1939 case, and `CP_J1939PDUFormat`/`CP_J1939PDUSpecific`, all had no upper-range `SetComParam`-time validation, silently truncating an out-of-range value at cast time instead of rejecting it — fixed in `rpc_link.rs::rpc_set_com_param`, mirroring `CP_J1939TargetAddress`'s own round-6/7 synchronous-rejection shape. `protocol.rs` gained a new `ChannelProtocol::is_j1939_family` predicate (mirroring `is_j1850_family`/`is_kwp_family`) so the `CP_TesterSourceAddress` check can be scoped the same way the pre-existing J1850/KWP addressing-byte guard already is. New regression tests: `tests/grpc_mock/j1939.rs::set_com_param_rejects_out_of_range_tester_source_address_on_j1939` and `::set_com_param_rejects_out_of_range_pdu_format_and_specific_on_j1939`.
- Decision 9's effect IS independently testable and deterministic: `tests/grpc_mock/j1939.rs::cancelling_the_optional_startcomm_message_after_a_successful_claim_frees_it_for_a_sibling` mirrors `startcomm_optional_message_tx.rs::can_optional_message_cancel_during_receive_phase_cancels_before_comm_started`'s own cancellation shape (`CoptStartcomm` with an optional message and an `expected_response_array` the mock never answers, cancelled mid-receive-phase via `CancelComPrimitive`) but proves the J1939-specific consequence the way this file's other cancel-owner tests do: CLL A claims 0x80, its StartComm is cancelled, and sibling CLL B (candidate `[0x80]`) can only successfully claim 0x80 if A's claim was actually cancelled and deregistered. Confirmed to catch the regression by temporarily disabling the fix's one unconditionally-reached call site (`ReceivePhaseOutcome::Terminal`) and observing CLL B fall back to the unclaimed default source address (`0xF1`) instead. Unlike the CAN analog, this test cannot stage a long `CP_P2Max` to guarantee it is "genuinely still waiting" before cancelling (`CP_P2Max` is not in J1939's closed `is_j1939_param` allowlist, so it stays at its fixed 200 ms seeded default) — it cancels immediately after observing the transmit instead, which is reliable in practice since local test round-trip latency is well under that ceiling, but is a documented, accepted looser guarantee than the CAN test's own.
- **Decision 9's other three call sites (`P3GapOutcome::Cancelled`, `Err(TxFailure::Event(...))`, `Err(TxFailure::Cancelled)`) have no dedicated regression test — an `edge-case-hunter` verification pass on this fix found this gap and it was investigated, not silently left.** `P3GapOutcome::Cancelled` is structurally unreachable for J1939 via any live RPC path: `wait_for_p3_gap` returns `Ready` immediately whenever `gap_ms == 0` (`ComParamSet::p3_phys_gap_ms`/`p3_func_gap_ms`'s own `unwrap_or(0)` fallback), and `CP_P3Phys`/`CP_P3Func` are NOT in J1939's closed `is_j1939_param` allowlist (`comparam_defaults.rs::j1939_can_common` never seeds either), so `gap_ms` is always `0` for a J1939 CLL and this arm can never actually wait long enough to cancel into. `Err(TxFailure::Event(...))`/`Err(TxFailure::Cancelled)` both require a genuine mid-transmit failure or cancellation from `transmit_request`; the only existing, established way this test suite produces a genuine `TxFailure::Event` at all is a software-ISO-TP multi-frame send whose FlowControl never arrives (`startcomm_optional_message_tx.rs::can_optional_message_temp_param_update_transmit_failure_reverts_hardware`'s own doc comment: "the mock adapter has no direct 'fail the next write' backdoor"), and J1939 never uses software ISO-TP framing (this file's own module doc). All three sites call the exact same helper with the exact same three arguments as the one proven site (`ReceivePhaseOutcome::Terminal`) — textually identical, not independently-varied logic — verified correct by code inspection and the `design-advisor` consult's own TOCTOU analysis (Decision 9's text above) rather than by a test that cannot be deterministically constructed. The same "found unreachable/infeasible via the available test surface, documented instead of forcing a misleading or flaky test" resolution already used elsewhere in this ADR (Decision 6's tester-present gap, Decision 8's reserved-candidate skip, Decisions 1/2's own race windows).
- Decision 10's effect IS independently testable and deterministic: three regression tests in `tests/grpc_mock/j1939.rs`. `spontaneous_loss_stops_the_clls_own_live_repeat_slots` proves the primary site (Decision 5's spontaneous-loss branch) — a synthesized `RX_FLAG_J1939_ADDRESS_LOST` for a CLL's currently claimed address, with a live repeat slot started under it, stops that slot (the mock's own `PDU_IOCTL_QUERY_REPEAT_MESSAGE` backdoor reports the MsgId as no longer tracked). `restarting_after_stopcomm_stops_the_previously_claimed_addresss_own_repeat_slots` proves the sibling site (Decision 4's fresh-attempt reset) with the same assertion shape, mirroring `restarting_after_stopcomm_frees_the_previously_claimed_address_for_a_sibling`'s own StopComm/reconfigure/StartComm sequence. `cancelling_the_optional_startcomm_message_after_a_successful_claim_stops_its_own_repeat_slots` proves the THIRD site — found missing by an `edge-case-hunter` verification pass on this Decision's own diff, in this same round 9, and fixed before this Decision's text above was finalized (`cancel_j1939_claim_after_failed_startcomm`, Decision 9's own cancellation helper) — mirroring `cancelling_the_optional_startcomm_message_after_a_successful_claim_frees_it_for_a_sibling`'s claim-then-cancel shape, but starting a repeat slot under the successfully-claimed-but-not-yet-`comm_started` address before cancelling the optional message, then proving the slot is stopped too. All three were confirmed to catch the regression by temporarily disabling `stop_repeat_slots_for_cll`'s call at each site independently and observing the corresponding test fail (the slot stayed live/queryable instead of being stopped).
- No dedicated regression test exists for Decision 10's accepted-residual race window (a `START` landing inside one of the three call sites' own internal await window, between the address's actual relinquishment and this Decision's cancellation) — the same infeasible-to-construct-deterministically class as this ADR's own Decisions 1/2 windows, for the identical reason (this crate's single-threaded `current_thread` test runtime resolves an uncontended `.lock().await` without a genuine preemption point).
- Decision 11's effect IS independently testable and deterministic for its primary site: `tests/grpc_mock/j1939.rs::spontaneous_loss_cancels_a_live_cyclic_send_recv_under_the_lost_address` claims 0x80, starts an infinite cyclic `CoptSendrecv` (`NumSendCycles = -1`, `Time = 20ms`), waits for at least two cycles to transmit, synthesizes `RX_FLAG_J1939_ADDRESS_LOST` for 0x80, and proves the COP reaches `PduCopstCancelled` AND that the written-message count stops growing afterward (a stability check, not an exact count, since one already-dispatched cycle may legitimately land before the loss is processed on the single poll task). Confirmed to catch the regression by temporarily disabling `cancel_send_recv_cops_for_cll`'s call at the primary site and observing the test fail. The sibling sites (`handle_start_comm`'s fresh-attempt reset, `cancel_j1939_claim_after_failed_startcomm`) are not independently tested here — the fresh-attempt-reset site's own live-SendRecv scenario would need a claim, a running cyclic send, then a StopComm/reconfigure/StartComm cycle layered on top of the cyclic-send setup already exercised above, and `cancel_j1939_claim_after_failed_startcomm`'s call is a documented no-op today (J1939 `CoptSendrecv` requires `comm_started`, never true at that call site) — verified correct by code inspection (both call the identical helper) rather than by additional tests, the same "textually identical, not independently-varied logic" resolution Decision 9's own three untested call sites already use.
- Decision 11's round-10 `edge-case-hunter` correction (the live-tier-2-registrant exclusion, added to this Decision's own text above) is also independently testable and deterministic: `tests/grpc_mock/j1939.rs::spontaneous_loss_does_not_cancel_a_detached_tier2_registrant_under_the_lost_address` claims 0x80, starts a `CoptSendrecv` shaped to migrate on its first match (`NumSendCycles = 1`, `NumReceiveCycles = -1`), migrates it to ADR-100 tier 2 via a real matched response, confirms it is alive-but-detached, then synthesizes the same `RX_FLAG_J1939_ADDRESS_LOST` for 0x80 and proves the COP does NOT reach `PduCopstCancelled`. Confirmed to catch the regression by temporarily reverting the tier-2 exclusion (back to the plain `cancelled_cops.extend(cop_handles)`) and observing the test fail (the detached registrant was wrongly cancelled), then restoring it; the full J1939 integration suite (`tests/grpc_mock/j1939.rs`) is now 29 tests.
- Decision 12's effect IS independently testable and deterministic: three regression tests in `tests/grpc_mock/j1939.rs`. `coptupdateparam_rejects_tester_source_address_off_the_live_claim` proves the positive case (claim 0x80, stage `CP_TesterSourceAddress = 0x90`, `CoptUpdateparam` rejected). `coptupdateparam_allows_restaging_the_same_tester_source_address_as_the_live_claim` proves the differs-check negative (re-staging 0x80, the already-claimed value, succeeds). `coptupdateparam_allows_tester_source_address_change_on_a_non_negotiated_cll` proves the negotiation-off negative (a CLL with `CP_J1939AddressNegotiationRule` bit 1 set, no claim ever registered, may freely change its address). All three require the CoptStartcomm's own `PduCopstFinished` to be observed (via `SubscribeEvent`) before the `CoptUpdateparam` call, not merely awaited synchronously — an earlier draft of the first two tests raced the claim's own async completion (the RPC call returns once the COP is enqueued, not once the claim negotiation finishes) and failed non-deterministically depending on incidental round-trip timing; confirmed to catch the regression by temporarily disabling the guard and observing the positive test fail.
- Decision 13's START-time gate IS independently testable and deterministic: four regression tests in `tests/grpc_mock/j1939.rs`. `repeat_message_start_rejects_before_any_claim_on_a_negotiation_enabled_cll` proves the "before any claim" window (a connected, negotiation-enabled CLL that never calls `CoptStartcomm` at all). `repeat_message_start_rejects_after_claim_exhaustion` proves the "after exhaustion" window, using `server.backdoor.set_j1939_claim_lost(true)` (the same deterministic exhaustion trigger `claim_exhaustion_fails_startcomm_with_init_error` uses) to force every candidate to come back `_LOST`. `repeat_message_start_succeeds_on_non_negotiated_cll_with_no_claim` proves the negation — a non-negotiated CLL (`CP_J1939AddressNegotiationRule` bit 1 set) is never blocked despite `j1939_claimed_address` staying `None` for its entire life by design. `repeat_message_start_succeeds_after_a_successful_claim` is the regression-safety positive case, confirming the gate does not regress the ordinary already-claimed path every other repeat-message test in this file relies on. Both rejection tests confirmed to catch the regression by temporarily short-circuiting the gate's condition (`if false && ...`) and observing them fail, then restoring it. One pre-existing test, `repeat_slot_stop_condition_wildcards_pgn_and_destination_exact_matches_source_address`, needed a fix alongside this Decision: it called `start_com_primitive(CoptStartcomm)` and immediately started a repeat slot without first waiting for `PduCopstFinished` — harmless before this Decision (no claim-state precondition existed to race against), but exposed by the new gate, since the RPC call returns once the COP is enqueued, not once the claim negotiation actually finishes on the poll task (the same timing lesson Decision 12's own tests already document). Fixed by adding the same `SubscribeEvent`/`wait_for_event(PduCopstFinished)` wait every other claim-then-act test in this file already uses.
- **No dedicated regression test exists for the terminal-outcome sweep's own specific TOCTOU window** (a `START` that genuinely races a live claim attempt — passing the gate while the claim is in flight, then losing the race for `shared_channels` against that attempt's own `Claimed`/`Exhausted` arm) — the same infeasible-to-construct-deterministically class this ADR's own Decisions 1/2/10 windows already document, for a related but distinct reason: unlike those windows (an uncontended `.lock().await` with no genuine `Pending`-yielding point), this loop's own `tokio::time::sleep(poll_interval)` inside its wait IS a genuine preemption point in principle, but reliably landing a second client's RPC call inside that exact window, rather than before or after it, is a timing-sensitive construction this test harness has no existing tool for (no mock backdoor to pause/resume the claim loop's own wait deterministically). Verified correct by code inspection instead: the sweep reuses `stop_repeat_slots_for_cll`, already independently proven correct by Decision 10's own three regression tests, and the `shared_channels` serialization against `ioctl_start_repeat_message`'s own pre-existing hold (Bug 1, ADR-165 PR #42 round 5) was traced by hand for both interleavings (sweep-first, START-first) in this Decision's own text above.
- Decision 14 Part A (the `StartComPrimitive`-time gate) IS independently testable and deterministic: `tests/grpc_mock/j1939.rs::sendrecv_rejects_before_any_claim_on_a_negotiation_enabled_cll` proves a transmitting `CoptSendrecv` on a connected, negotiation-enabled CLL that never calls `CoptStartcomm` is rejected synchronously (`FailedPrecondition`) with nothing reaching the mock. `sendrecv_receive_only_is_not_gated_by_the_no_claim_check` is the negative counterpart — a receive-only `CoptSendrecv` (`NumSendCycles == 0`) on the same unclaimed CLL must succeed, since it puts nothing on the bus. Confirmed to catch the regression by temporarily short-circuiting the gate's condition (`if false && ...`, mirroring Decision 13's own verification shape) and observing the positive test fail, then restoring it. Writing this Decision's own regression tests also caught a genuine scoping bug in `ResolvedSendRecvTx::j1939_tx_source` (Part B) before it shipped: an early draft scoped it on `resources::is_j1939_protocol_id` alone, which broke the PRE-EXISTING `unclaimed_source_address_over_8_bytes_fails_the_send` test (a non-negotiated CLL, where `j1939_claimed_address` stays `None` by design) by spuriously cancelling every send on it — caught by running this file's full suite after implementing Part B, not by a test written specifically for Part B itself; fixed by also gating `j1939_tx_source` on `events::j1939_claim_requested`. Part B's own transmit-time re-check has no dedicated regression test for its outcome-arm race — see this Decision's own Accepted residual note above; the full J1939 integration suite (`tests/grpc_mock/j1939.rs`) was 41 tests.
- **Round-12 `edge-case-hunter` correction to Decision 14 Part A's own enqueue-time gate** (recorded in this Decision's own text above): the gate originally read this CLL's Active `CP_J1939AddressNegotiationRule` unconditionally, even for a `temp_param_update = 1` `CoptSendrecv`, whose own resolution (`resolve_send_recv_tx`) binds against the Working (`effective`) snapshot instead per ADR-067 — so the gate and the resolution could disagree about whether this CLL counted as "negotiated" for that one Temp-bound send. Fixed by adding `j1939_negotiated_unclaimed_for(link, params)`, a parameterized variant of the shared predicate that reads whichever `ComParamSet` the gate's own caller passes, and calling it with the same Working-vs-Active choice `resolve_send_recv_tx` already makes for `temp_param_update`. This correction's effect IS independently testable and deterministic: `tests/grpc_mock/j1939.rs::coptsendrecv_temp_binding_uses_workings_own_negotiation_rule_not_actives` stages a CLL whose Active set stays negotiation-enabled and unclaimed (never started) while an ordinary post-connect `SetComParam` stages Working (only) to non-negotiated, then proves a `temp_param_update = 1` `CoptSendrecv` succeeds — it must bind Working's own non-negotiated rule, not this CLL's unclaimed Active rule. Confirmed to catch the regression by temporarily reverting the fix (back to the unconditional Active-only read) and observing the test fail, then restoring it. The round-12 `edge-case-hunter` pass also found and removed one duplicate/misleadingly-named unit test on the shared predicate itself (`negotiated_unclaimed_true_during_a_spontaneous_loss_to_reclaim_window` — its `Some(0x80)` then immediately `None` sequence left state bit-for-bit identical to the sibling `negotiated_unclaimed_true_before_any_claim` test, providing zero incremental coverage despite its name implying otherwise); the shared predicate now has four unit tests, not five. Full J1939 integration suite (`tests/grpc_mock/j1939.rs`) is now 42 tests.
- Decision 15's effect IS independently testable and deterministic: `tests/grpc_mock/j1939.rs::coptupdateparam_execution_time_check_catches_a_claim_that_lands_after_enqueue` uses `PDU_IOCTL_SUSPEND_TX_QUEUE`/`RESUME_TX_QUEUE` to make the enqueue-to-dispatch race fully deterministic (rather than a bare back-to-back RPC race, which this file's own module doc already documents as unobservable/uncontrollable): a `CoptStartcomm` and a `CoptUpdateparam` staging a different `CP_TesterSourceAddress` are both enqueued while the queue is held suspended (so Decision 12's own enqueue-time guard sees no live claim yet and lets the `CoptUpdateparam` through), then the queue is resumed — draining strictly FIFO, so `CoptStartcomm` claims and finishes before `CoptUpdateparam` reaches Decision 15's own execution-time check, which now catches the drift Decision 12's snapshot could not see and cancels the whole COP (`PduErrEvtProtErr` then `PduCopstCancelled`), and a subsequent ordinary send confirms Active `NODE_ADDRESS` was never corrupted by the rejected promotion. Confirmed to catch the regression by temporarily short-circuiting the new check's own condition (`false && staged_node_address != active_node_address && { .. }`) and observing the test fail (a ~2.3s timeout waiting for a `PduCopstCancelled` that never arrives), then restoring it.
- Decision 16's shared `j1939_negotiated_unclaimed` predicate is independently unit-tested: `events_j1939_claim.rs::tests::negotiated_unclaimed_true_before_any_claim`, `::negotiated_unclaimed_false_once_an_address_is_claimed`, `::negotiated_unclaimed_false_when_negotiation_is_not_requested`, `::negotiated_unclaimed_false_for_a_non_j1939_protocol` (a fifth test, `::negotiated_unclaimed_true_during_a_spontaneous_loss_to_reclaim_window`, was removed by the round-12 `edge-case-hunter` correction recorded above — it provided zero incremental coverage over `::negotiated_unclaimed_true_before_any_claim`). Confirmed to catch the regression by temporarily forcing the predicate to always return `false` and observing the one remaining "true" case fail, then restoring it. The `dispatch_due_tester_present` filter integration itself has no dedicated end-to-end regression test — see this Decision's own Accepted residual note above; a temporary `(true || !j1939_negotiated_unclaimed(link))` short-circuit of the filter's new clause was also confirmed NOT to break any test in this file's full suite (41 tests at the time of this check, all pass either way), independently corroborating that this integration point is currently unreachable via the live RPC surface, not merely asserted to be.
- Decision 17's effect IS independently testable and deterministic: `tests/grpc_mock/j1939.rs::reconnecting_the_same_cll_handle_clears_the_stale_claimed_address` claims 0x80, disconnects, reconnects on the same `cll_handle` without ever re-claiming, and proves Decision 13's own `PDU_IOCTL_START_REPEAT_MESSAGE` gate now rejects the attempt the same as any other never-yet-claimed negotiation-enabled CLL. Confirmed to catch the regression by temporarily reverting the two-field reset in `rpc_link.rs::rpc_connect_com_logical_link` and observing the test fail (the stale claim let the repeat message start), then restoring it. `j1939_claim_cursor`'s own reset has no independent regression test — see this Decision's own Accepted residual note above.
- Round 13 also fixed one unrelated, mechanical connect-time validation gap on the same PR (no Decision item for this one — CLAUDE.md's "ADR not needed" criteria: a straightforward rejection with no design alternative, not itself part of the claim-registration-atomicity mechanism this ADR documents): `names.rs::resolve_pin_selection`'s J1939 arm used the broad `is_j1939_protocol_id` predicate (which deliberately also matches the deferred `PROTOCOL_J1939_CH1..CH128` Additional Channel range, for other callers like `tx_header.rs`'s family-scoped ComParam guards) to decide whether THIS arm accepts a raw hardware protocol id, letting a `_CHx` id through as if it were the `_PS` shape it actually implements — every call site resolves `resolve_pin_selection(...)?` before `resolve_channel_selection(...)`, so this let a `_CHx` id either wrongly succeed here (with pins supplied, only to be rejected downstream by `resolve_channel_selection`'s unrelated clause-6/clause-7 mutual-exclusion guard, for the wrong stated reason) or get rejected with a misleading "must explicitly select pins" message (with no pins, clause 7's own correct shape) instead of the unambiguous "Additional Channels not supported" one. Fixed by narrowing this arm to reject any non-`PROTOCOL_J1939_PS` id immediately, before the opt-in/pins checks below it. New regression tests: `tests/grpc_mock/j1939.rs::create_rejects_a_deferred_additional_channel_id` (pins supplied, matching the finding's own scenario) and `::create_rejects_a_deferred_additional_channel_id_even_with_no_pins` (the clause-7-correct no-pins shape). Both confirmed to catch the regression by temporarily removing the new check and observing each fail with the wrong (unrelated) rejection message instead, then restoring it.
- **Round-13 `edge-case-hunter` correction to the mechanical `names.rs` fix above:** the raw-numeric-id check alone missed SAE J2534-2 clause 7's OTHER route into the same gap — a compound resource name (`"<name>_CH<n>"`, e.g. `"ISO_OBD_on_SAE_J1939_73_CH5"`) resolves via `resolve_protocol_name_with_chx_suffix` to `protocol = ChannelProtocol::J1939_PS` (not a raw `_CHx` id) plus a separate `requested_index`, so `raw_hw_protocol_id` inside `resolve_pin_selection` normalized straight to `PROTOCOL_J1939_PS` and the raw-id-only check never fired — with no pins (clause 7's own correct shape), this route still fell through to the misleading "must explicitly select pins" message the round-13 fix's own text above claims to have eliminated, since `resolve_channel_selection`'s own correct rejection (via `chx_protocol_id` returning `None` for J1939) never got a chance to run. Fixed by threading `requested_index: Option<u32>` into `resolve_pin_selection`'s signature and widening the check to `raw_hw_protocol_id != PROTOCOL_J1939_PS || requested_index.is_some()`. New regression test: `tests/grpc_mock/j1939.rs::create_rejects_a_deferred_additional_channel_compound_name`, confirmed to catch the regression by temporarily reverting the `requested_index.is_some()` addition and observing the test fail with the wrong message, then restoring it. The same pass also fixed a stale `service.rs::LogicalLinkState::j1939_claimed_address` doc comment that had always incorrectly claimed the field was "cleared to `None` on disconnect" — corrected to describe Decision 17's actual mechanism (reset at the NEXT connect's finalization, not at disconnect). Full J1939 integration suite is now 46 tests.
- Decision 3's round-14 correction (see that Decision's own text above) is independently testable and deterministic: `tests/grpc_mock/j1939.rs::optional_startcomm_message_after_claim_uses_the_bound_snapshots_other_header_fields` stages Active `CP_J1939TargetAddress = 0x10` (never changed) and Working (only) `CP_J1939TargetAddress = 0x20` via an ordinary post-connect `SetComParam`, then issues a `temp_param_update = 1` `CoptStartcomm` with an optional message and proves the written frame's target-address bytes equal 0x20 (Working-bound), not 0x10 (stale Active), while the source-address byte is still the freshly claimed address. Confirmed to catch the regression by temporarily reverting the recomposition to `&link.active` and observing the assertion fail (0x10 instead of 0x20), then restoring it. `tester_present_data`'s own recomposition (Decision 6) is unaffected by this correction and has no new test, since its own Active-only binding was already correct and unchanged. Full J1939 integration suite is now 47 tests.
- Decision 15's round-15 extension (see that Decision's own text above) is independently testable and deterministic: `tests/grpc_mock/j1939.rs::coptupdateparam_execution_time_check_catches_j1939_target_address_ffff_after_startcomm` uses `PDU_IOCTL_SUSPEND_TX_QUEUE`/`RESUME_TX_QUEUE` the same way Decision 15's own original test does, with a deliberately NON-negotiated `CoptStartcomm` (a claim-enabled one would also trip Decision 15's own pre-existing `NODE_ADDRESS` re-check via the claim's own write-back, confounding the regression-catch for this field specifically): a `CoptStartcomm` and a `CoptUpdateparam` staging `CP_J1939TargetAddress = 0xFFFF` are both enqueued while the queue is suspended (so the enqueue-time gate sees `comm_started == false` and lets the sentinel through), then the queue is resumed -- draining FIFO, so `CoptStartcomm` completes (`comm_started` becomes live-true) before the queued `CoptUpdateparam` reaches this extension's own execution-time check, which now catches the drift and cancels the whole COP. Confirmed to catch the regression by temporarily short-circuiting the new check and observing the test fail (a timeout waiting for a `PduCopstCancelled` that never arrives), then restoring it. Full J1939 integration suite is now 48 tests.
- Decision 18's effect IS independently testable and deterministic: `tests/grpc_mock/j1939.rs::coptsendrecv_temp_binding_cannot_spoof_a_negotiation_opt_out_on_an_engaged_unclaimed_cll` proves this round's exact finding is now rejected -- a negotiation-enabled CLL that has genuinely engaged the claim machinery via a real `CoptStartcomm` (never yet claimed, or unclaimed again after a spontaneous loss) stages `CP_J1939AddressNegotiationRule = 0b10` in Working ONLY, then issues a `temp_param_update = 1` transmitting `CoptSendrecv` -- rejected synchronously by Decision 14 Part A's enqueue-time gate, which now consults the persistent `j1939_negotiation_engaged` flag rather than trusting the spoofable Working snapshot alone. Two new unit tests on the shared predicate itself, `events_j1939_claim.rs::tests::negotiated_unclaimed_true_when_engaged_even_if_snapshot_says_non_negotiated` and `::negotiated_unclaimed_false_when_engaged_but_already_claimed`, cover the new OR-branch directly. The pre-existing legitimate-opt-out and non-negotiated-CLL tests this fix must not break -- `coptsendrecv_temp_binding_uses_workings_own_negotiation_rule_not_actives` (round 12, a CLL that never issues a real `CoptStartcomm` at all, so `j1939_negotiation_engaged` correctly stays `false`) and `unclaimed_source_address_over_8_bytes_fails_the_send` (a genuinely non-negotiated CLL, `j1939_negotiation_engaged` permanently `false` for its whole lifetime) -- were confirmed still passing, closing the exact round-12 "is_j1939_protocol_id-alone" regression class this fix's own `j1939_tx_source` widening could otherwise have reintroduced. Full J1939 integration suite was 49 tests.
- Decision 18's round-16 residual correction (see that Decision's own text above) is independently testable and deterministic: `tests/grpc_mock/j1939.rs::opting_out_of_negotiation_after_a_claim_frees_the_address_for_a_sibling` proves the non-negotiated re-StartComm branch now actually relinquishes a previously-claimed address, not just its own local `j1939_negotiation_engaged` marker -- a sibling CLL with the identical single candidate can only claim it if the stale entry was truly cancelled and deregistered from `SharedChannel::j1939_claims`, the same sibling-claim-proof pattern Decision 4's own regression test established. Confirmed to catch the regression by temporarily reverting the branch to its pre-round-16 shape (clearing only `j1939_negotiation_engaged`) and observing the sibling's claim attempt exhaust with `PduErrEvtInitError` instead of succeeding, then restoring it. Full J1939 integration suite was 50 tests.
- Decision 19's effect (see that Decision's own text above) is independently testable and deterministic: `tests/grpc_mock/j1939.rs::stopcomm_final_message_after_a_spontaneous_loss_is_suppressed_not_sent` claims an address, forces the spontaneous-loss-armed reclaim to exhaust deterministically via `set_j1939_claim_lost` (single candidate, so exhaustion -- not a real re-claim -- is guaranteed), then issues a `CoptStopcomm` carrying a non-empty `cop_data` and proves: no additional byte reaches the mock (`MockBackdoor::written_count` unchanged), a `PduErrEvtProtErr` reports the drift against this exact COP's handle, and the COP still completes to `PduCllstOnline`/`PduCopstFinished` -- ADR-085's teardown is never blocked by the suppression. Confirmed to catch the regression by temporarily short-circuiting the new `j1939_claim_drifted` check (forcing it permanently `false`) and observing the assertion on `written_count` fail (a stale frame reached the mock), then restoring it. Full J1939 integration suite is now 51 tests.
- Decision 15's round-17 correction (see that Decision's own text above) is independently testable and deterministic: `tests/grpc_mock/j1939.rs::coptupdateparam_execution_time_rejection_pushes_no_hardware_write_for_the_cancelled_cop` reuses the original Decision 15 test's own `PDU_IOCTL_SUSPEND_TX_QUEUE`/`RESUME_TX_QUEUE` determinism trick, additionally staging an unrelated, hardware-backed `DATA_RATE` change in the SAME `CoptUpdateparam` as the conflicting `NODE_ADDRESS` promotion, and asserts `server.backdoor.set_config_count()` never advances past its pre-dispatch baseline once the COP is cancelled. Confirmed two ways: (1) temporarily short-circuiting both checks' own conditions (`if false && claim_owns_address` / `if false && stages_unconfigured_j1939_target`) and observing the test fail via a timeout waiting for the `PduCopstCancelled` that never arrives (nothing rejects the COP at all) — proving the test is load-bearing on the checks running at all; (2) an `edge-case-hunter` verification pass on this round's own diff asked for stronger evidence that the RELOCATION itself, not merely the checks existing, is what the test proves — so the two checks (with `drop(api)` removed from each, since `api` is already released by the pre-round-17 hardware-push site) were temporarily moved back to their exact pre-round-17 position (inside `if all_ok { .. }`, after `apply_bustype_lock`/`apply_params_to_hardware_locked`), and the test failed with `assertion left == right failed: ... left: 3, right: 2` — `set_config_count` had already advanced by one SET_CONFIG call (the unrelated DATA_RATE push) before the rejection fired, the exact partial-apply this correction closes — then the relocation was restored and the test re-confirmed passing. Full J1939 integration suite is now 52 tests.
- **Round-16 `edge-case-hunter` correction: `cancel_j1939_claims_for_cll`'s own filter ignored `connect_generation`.** `SharedChannel::j1939_claims: HashMap<u8, (u32, u64)>` stores `(owner_cll_handle, connect_generation)` per claimed address, but `cancel_j1939_claims_for_cll`'s filter matched on `cll_handle` alone, at all three of its non-teardown call sites (Decision 4's "fresh attempt" reset, Decision 9's `cancel_j1939_claim_after_failed_startcomm`, and this round's own Decision 18 residual-closing branch above) plus the two teardown call sites (`DisconnectComLogicalLink`/`DestroyComLogicalLink`, `rpc_link.rs`). For the three non-teardown sites, a disconnect+reconnect completing on the SAME `cll_handle` (bumping `connect_generation`, ADR-086) and registering a fresh claim, landing inside the `.await` gap between one of these callers' own earlier staleness checks and this call, would wrongly let a stale dispatch cancel the NEW generation's live claim instead of whatever the stale generation itself owned. Fixed by widening `cancel_j1939_claims_for_cll`'s signature to take `connect_generation: u64` and filtering on `owner == cll_handle && owner_generation == connect_generation`; every call site now passes its own live/captured generation for this exact `cll_handle` (the three non-teardown sites already had `connect_generation` in scope as their own parameter; the two teardown sites now capture `link.connect_generation` into a local before the link's other fields are taken/cleared, since at teardown time this CLL's generation is simply itself -- an unconditional match, so this closes the gap for the non-teardown sites without changing teardown's own always-cancel-everything-this-handle-owns behavior). No dedicated regression test exists for this specific race window -- the same infeasible-to-construct-deterministically class this ADR's own Decisions 1/2 already document as accepted (this crate's single-threaded `current_thread` `#[tokio::test]` runtime has no natural preemption point to land a reconnect precisely inside the narrow `.await` gap); verified instead by code inspection and by confirming the full J1939 integration suite (including `reconnecting_the_same_cll_handle_clears_the_stale_claimed_address` and `disconnect_while_sibling_survives_does_not_error_or_disturb_the_sibling`, the suite's own existing reconnect/disconnect-adjacent coverage) still passes unchanged.
- Decision 20's effect (see that Decision's own text above) is independently testable and deterministic: `tests/grpc_mock/j1939.rs::spontaneous_loss_leaked_repeat_stop_is_drained_by_a_later_sibling_ioctl` proves both halves of the fix -- a forced native `STOP_REPEAT_MESSAGE` failure during a spontaneous loss's own repeat-slot sweep leaves the slot genuinely live (not silently dropped), and clearing the override then issuing any repeat-message IOCTL on the same physical channel (from a second, non-negotiated sibling CLL, so the trigger does not depend on the original CLL's own claim state) drains the leaked id via ADR-165 Decision 6's pre-existing opportunistic retry. Confirmed to catch the regression by temporarily removing the `record_leaked_repeat_slots` call at this Decision's spontaneous-loss call site and observing the final poll assertion fail, then restoring it. Full J1939 integration suite is now 55 tests.
- Decision 2's round-20 correction (see that Decision's own text above) has no dedicated regression test -- `timed_out_without_indication` is unreachable via this harness (the mock's `IOCTL_PROTECT_J1939_ADDR` always synchronously enqueues a CLAIMED/LOST indication), the same infeasible-to-construct-deterministically class Decision 2's own original mechanism already documents; verified by code inspection and the full J1939 suite (55 tests, unchanged) passing.
- Decision 18's round-20 correction (see that Decision's own text above) is independently testable and deterministic: new unit test `events_j1939_claim.rs::tests::negotiated_unclaimed_false_when_opted_out_even_if_snapshot_says_negotiated` proves the tri-state posture overrides a stale negotiated `params` snapshot, alongside the pre-existing `Engaged`/`Undecided` unit tests kept passing unchanged as this round's own regression tripwires; new integration test `tests/grpc_mock/j1939.rs::temp_bound_negotiation_opt_out_does_not_wrongly_block_later_ordinary_sends` proves a Temp-bound opt-out `CoptStartcomm` no longer blocks a later ordinary `CoptSendrecv` or `PDU_IOCTL_START_REPEAT_MESSAGE` on that CLL. Confirmed to catch the regression by temporarily reverting the opt-out branch's posture assignment to `Undecided` and observing the new integration test fail with the exact `FailedPrecondition` rejection this round's finding describes, then restoring it. Full J1939 integration suite is now 56 tests.
- Decision 2's round-21 correction (see that Decision's own text above) has no dedicated regression test, extending the round-20 correction's own identical-class limitation -- `timed_out_without_indication` is still unreachable via this harness; verified by code inspection and the full J1939 suite passing unchanged.
- **Round-21 correction: `stop_repeat_slots_for_cll`'s own lookup ignored `connect_generation`.** Same bug class as the round-16 `edge-case-hunter` correction above, on a different shared helper: `stop_repeat_slots_for_cll` (Decision 10) looked up `LogicalLinkState::repeat_message_ids` by `cll_handle` alone at both its initial read and its later `repeat_message_ids`-pruning re-acquisition. Several of its 8 call sites (across Decisions 5/9/10/11/18's various relinquishment/cancellation arms in `events_j1939_claim.rs` and `events.rs::handle_start_comm`) release `shared_channels` before `.await`ing this helper; a disconnect+reconnect on the SAME `cll_handle` racing into that gap, with the new session starting its own repeat slot before this helper's lookup runs, would wrongly stop the NEW generation's live slots while attributing the cleanup to the OLD, already-torn-down session. Fixed identically to the round-16 correction: widened to take `connect_generation: u64` and filter on it at both acquisitions; all 8 call sites already had `connect_generation` in scope (as their own function parameter, or newly captured from the claim-routing snapshot at `deliver_j1939_claim_indication`'s call site). No dedicated regression test -- the same infeasible-to-construct-deterministically class the round-16 correction's identical race-window fix already documents (no natural preemption point in this crate's single-threaded test runtime to land a reconnect inside the narrow `.await` gap); verified by code inspection and by the full J1939 suite (56 tests) plus the full crate suite (871 tests) passing unchanged.
- **Round-22 correction: `cancel_send_recv_cops_for_cll`'s own lookup ignored `connect_generation`.** The third and last instance of the same bug class the round-16/round-21 corrections already fixed for `cancel_j1939_claims_for_cll`/`stop_repeat_slots_for_cll` -- `cancel_send_recv_cops_for_cll` (Decision 11) looked up `LogicalLinkState` by `cll_handle` alone before extending `cancelled_cops`. Fixed identically: widened to take `connect_generation: u64`, filtering the `logical_links` lookup on it; all 4 call sites already had the generation in scope. `cop_handles` themselves (sourced from `ctx.primitives`, a separate map with no `connect_generation` field on `CopEntry`) need no equivalent scoping -- a COP entry's own lifecycle is already tied to the generation that created it, so no stale entry can exist there; only the write into the reused, handle-keyed `LogicalLinkState` needed the guard. No dedicated regression test, the same infeasible-to-construct-deterministically class as its two sibling corrections; verified by code inspection and the full J1939 suite (58 tests at the time of this fix) passing unchanged.
- **Round-22 correction: `CP_J1939Name` accepted any length, silently zero-padding or truncating to fit `protect_j1939_addr`'s fixed 8-byte NAME parameter.** A client's own `GetComParam` readback still returned the original, untransformed bytes, so the adapter could claim/arbitrate with a 64-bit NAME the client's own record of its configuration disagreed with -- the identical "adapter acts on a value the client's readback disagrees with" failure mode `CP_J1939TargetAddress`'s own truncation check (round 6) already closes for a Unum32 field. Fixed by rejecting a NON-EMPTY, wrong-length staged value synchronously at both `CoptStartcomm` and `CoptUpdateparam` call time (mirroring `CP_J1939TargetAddress`'s own two-call-site range check, `rpc_primitive.rs`) -- an EXPLICITLY exempted length of 0 bytes is `comparam_defaults.rs`'s own documented "not configured" default (`j1939_can_common` seeds `PARAM_J1939_NAME` to an empty Bytefield, not absent), which `resolve_j1939_claim_params` zero-pads to an all-zero NAME that `run_j1939_claim_loop`'s own dedicated check already fails closed (clause 16.3.3.2: all-zero is `PROTECT_J1939_ADDR`'s wire-level CANCEL form, not a valid claim) -- only a STAGED, non-empty, wrong-length value is the new failure mode this closes. New tests: `tests/grpc_mock/j1939.rs::j1939_name_wrong_length_rejects_startcomprimitive_synchronously` and `::coptupdateparam_rejects_j1939_name_wrong_length`, mirroring the equivalent `CP_J1939TargetAddress` tests' own two-call-site shape. Full J1939 integration suite is now 58 tests.
- Decision 21's effect (see that Decision's own text above) is independently testable and deterministic: new integration test `tests/grpc_mock/j1939.rs::cancel_com_primitive_during_the_claim_wait_ends_it_promptly_as_cancelled` proves a `CancelComPrimitive` against an in-flight claim-driving `CoptStartcomm` ends the COP promptly as `Cancelled` rather than running to completion or holding the full staged timeout, using a new mock backdoor (`__mock_set_j1939_claim_no_indication`) that makes the native claim issue "succeed" without ever delivering an indication, so the loop's bounded wait genuinely blocks long enough for a real `CancelComPrimitive` to land inside it. Confirmed to catch the regression by temporarily forcing `cancel_cop` to `None` inside the loop and observing the test fail exactly as expected (hangs toward the staged timeout instead of ending promptly), then restoring it. Full J1939 integration suite is now 60 tests.
- Decision 22's effect (see that Decision's own text above) is independently testable and deterministic: new integration test `tests/grpc_mock/j1939.rs::leaked_claim_cancel_blocks_a_fresh_attempt_until_the_native_cancel_finally_succeeds` proves a failed relinquishment cancel leaks the address, blocks a fresh claim attempt from re-claiming it while the leak persists (fails closed, `PduErrEvtInitError`), and lets a later claim attempt succeed once the leaked guard's own retry-cancel finally succeeds -- using a new mock backdoor (`__mock_set_j1939_cancel_error`) that fails the native cancel form without disturbing the mock's own claimed-address bookkeeping, for test realism. Fully synchronous against the mock, no timing windows. Confirmed to catch the regression by temporarily disabling the leaked-set retry guard and observing the test fail exactly as expected (the fresh attempt wrongly re-claims the still-defended address -- the two-claimants bug this Decision exists to close), then restoring it. Full J1939 integration suite is now 60 tests (shared bump with Decision 21's own new test).
- Decision 22's round-23 correction (see that Decision's own text above) is independently testable and deterministic: new integration test `tests/grpc_mock/j1939.rs::leaked_claim_on_a_disjoint_candidate_list_still_blocks_the_whole_attempt` proves the channel-wide gate blocks a fresh claim attempt whose own candidate list never intersects the leaked address, closing the gap the original per-candidate guard left open. Confirmed to catch the regression by temporarily disabling the new gate and observing the test fail exactly as expected (the disjoint candidate wrongly claimed while the leaked address remained unresolved), then restoring it. Full J1939 integration suite is now 61 tests.
- Decision 18's round-24 correction (see that Decision's own text above) is independently testable and deterministic: new integration test `tests/grpc_mock/j1939.rs::opt_out_startcomm_fails_closed_while_a_relinquished_claim_remains_leaked` proves the opt-out branch now fails its own StartComm closed (and keeps every send/repeat gate shut) while a relinquished claim remains leaked, instead of promoting `OptedOut` and letting the client transmit under a new source regardless, then proves the opt-out genuinely takes effect -- including the client-managed source address actually reaching the wire -- once the shared reconcile retry clears the leak. Confirmed to catch the regression by temporarily short-circuiting the new gate's condition and observing the test fail exactly as expected, then restoring it. Full J1939 integration suite is now 62 tests. **No dedicated regression test exists for this correction's own `still_on_this_channel == false` (stale) branch** (`PduCopstCancelled` instead of `PduErrEvtInitError`+`PduCopstFinished`) -- the same infeasible-to-construct-deterministically class this ADR's own `Exhausted` arm (Consequences, above) and round-16/21/22 `connect_generation` corrections already document: this branch has no polling/suspend-queue mechanism (unlike Decision 15's own `PDU_IOCTL_SUSPEND_TX_QUEUE` trick) to force a disconnect to interleave precisely between the branch's own state writes and this snapshot. Verified by code inspection: the snapshot is taken fresh, immediately before the terminal-status decision, at least as tightly as the `Exhausted` arm's own equivalent check.
- Decision 23 (see that Decision's own text above) is independently testable and deterministic: four new integration tests in `tests/grpc_mock/j1939.rs` cover both findings' fixes -- `name_collision_blocks_a_sibling_claiming_a_disjoint_address` (Finding 1's channel-wide NAME-collision gate), `coptupdateparam_rejects_j1939_name_off_the_live_claim`/`coptupdateparam_allows_restaging_the_same_j1939_name_as_the_live_claim` (Finding 2's enqueue-time guard, positive/negative pair), and `coptupdateparam_execution_time_check_catches_a_name_change_that_lands_after_enqueue` (Finding 2's execution-time re-check). All four confirmed to catch their respective regressions by temporarily disabling the corresponding gate condition and observing the correct test fail, then restoring it. Full J1939 integration suite is now 66 tests. **Round-25 `edge-case-hunter` residual: no dedicated test exercises Finding 1's own NAME-collision gate against a genuinely stale (superseded-generation) colliding entry specifically** -- i.e. a case where a same-NAME entry exists for a different `cll_handle` but that handle's `connect_generation` no longer matches, so the gate's own liveness recheck (not just the collision detection) is what has to correctly let the attempt through. This is a pre-existing gap, not a regression this round's diff introduced: the pre-existing `owned_by_a_live_sibling` check (the identical address-keyed liveness-recheck pattern this NAME-keyed gate was modeled on) has never had a dedicated stale-generation test of its own either, anywhere in this file's history. Constructing one deterministically would require landing a reconnect precisely inside the same narrow `.await` gap the round-16/21/22 `connect_generation` corrections already document as infeasible on this crate's single-threaded test runtime; verified instead by code inspection (the recheck is structurally identical to `owned_by_a_live_sibling`'s own, confirmed correct by the same reasoning) and by the full J1939 suite passing unchanged.
