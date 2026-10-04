//! SAE J2534-2 clause 16 SAE J1939 address-claim/defend state machine
//! (ADR-179 Decision 3). This module owns:
//!
//! - Resolving a CLL's staged claim inputs (`CP_J1939PreferredAddress`
//!   candidate list, `CP_J1939Name`, `CP_J1939AddrClaimTimeout`) from a
//!   `ComParamSet` snapshot ([`resolve_j1939_claim_params`]), and whether a
//!   claim is even requested at all (`CP_J1939AddressNegotiationRule` bit 1,
//!   [`j1939_claim_requested`]).
//! - The retry-over-the-candidate-list state machine itself
//!   ([`run_j1939_claim_loop`]), driven by `events.rs::handle_start_comm`
//!   for the initial `PDU_COPT_STARTCOMM`-triggered claim, and by
//!   `run_due_tick_duties` for a spontaneous post-claim `_LOST` retry.
//! - Routing a `RX_FLAG_J1939_ADDRESS_CLAIMED`/`_LOST` indication (consumed
//!   by `poll_rx_inner`'s RxStatus-withhold arm) back to its owning CLL via
//!   `SharedChannel::j1939_claims`, dropping a stale one whose recorded
//!   `connect_generation` no longer matches ([`resolve_j1939_claim_indication`],
//!   [`deliver_j1939_claim_indication`]).
//! - Best-effort claim cancellation on a J1939 CLL's disconnect while its
//!   shared physical channel survives ([`cancel_j1939_claims_for_cll`]).
use super::super::{
    J1939ClaimEntry, J1939ClaimOutcome, J1939ReclaimPending, PARAM_J1939_ADDR_CLAIM_TIMEOUT,
    PARAM_J1939_NAME, PARAM_J1939_PREFERRED_ADDRESS, SharedChannel,
};
use super::*;

/// SAE J2534-2 clause 16 SAE J1939 address-claim inputs resolved from a
/// `ComParamSet` snapshot (ADR-179 Decision 3): the ordered candidate
/// addresses (`CP_J1939PreferredAddress`, a Bytefield ComParam), this
/// node's SAE J1939 NAME (`CP_J1939Name`, an 8-byte Bytefield), and the
/// per-attempt bounded wait (`CP_J1939AddrClaimTimeout`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct J1939ClaimParams {
    pub(super) candidates: Vec<u8>,
    pub(super) name: [u8; 8],
    pub(super) timeout: Duration,
}

/// Resolves [`J1939ClaimParams`] from `params` (ADR-179 Decision 3). Pure --
/// unit-testable independent of any live CLL/hardware state.
///
/// `CP_J1939PreferredAddress` (`PARAM_J1939_PREFERRED_ADDRESS`) is read
/// verbatim as the candidate list, in order -- ISO 22900-2's own BYTEFIELD
/// `pDataArray` already IS that ordered list, so no further decoding is
/// needed beyond the existing `ComParamSet::bytes` storage. Absent (never
/// staged) resolves to an empty list, which [`j1939_claim_cursor_step`]
/// reports as immediately `Exhausted` -- matching every other "ComParam not
/// configured" failure shape in this codebase (fail closed, not a hidden
/// default candidate).
///
/// `CP_J1939Name` (`PARAM_J1939_NAME`) is read the same way and
/// zero-padded/truncated to exactly 8 bytes for
/// [`super::super::j2534_0404`]... `j2534-0404::protect_j1939_addr`'s fixed
/// `[u8; 8]` NAME parameter -- an absent value pads with trailing zero
/// bytes (deliberately: the resulting all-zero NAME is itself detected and
/// rejected by [`run_j1939_claim_loop`]'s own dedicated check, clause
/// 16.3.3.2's wire-level CANCEL form, not a hidden default candidate). A
/// STAGED value of any OTHER wrong length is a distinct failure mode
/// (Codex review, PR #72): silently transforming it here would let the
/// adapter claim/arbitrate with a NAME the client's own `GetComParam`
/// readback disagrees with. `rpc_primitive.rs` now rejects that case
/// synchronously at both `CoptStartcomm` and `CoptUpdateparam` call time
/// (mirroring `CP_J1939TargetAddress`'s own two-call-site range check), so
/// this function's own pad/truncate on a wrong-length STAGED value is
/// unreachable via any validated RPC call path -- it survives here only as
/// this pure function's own defensive fallback for direct unit-test
/// construction (see this module's `tests::negotiated_...`-adjacent cases),
/// not as a live silent-transform path.
///
/// `CP_J1939AddrClaimTimeout` (`PARAM_J1939_ADDR_CLAIM_TIMEOUT`) defaults to
/// 1_250_000 microseconds (already seeded by `comparam_defaults.rs`'s
/// `j1939_can_common`) when absent.
pub(super) fn resolve_j1939_claim_params(params: &ComParamSet) -> J1939ClaimParams {
    let candidates = params
        .bytes
        .get(&PARAM_J1939_PREFERRED_ADDRESS)
        .cloned()
        .unwrap_or_default();
    let name = normalize_j1939_name(params.bytes.get(&PARAM_J1939_NAME).map(Vec::as_slice));
    let timeout_us = params
        .unum32
        .get(&PARAM_J1939_ADDR_CLAIM_TIMEOUT)
        .copied()
        .unwrap_or(1_250_000);
    J1939ClaimParams {
        candidates,
        name,
        timeout: Duration::from_micros(timeout_us as u64),
    }
}

/// Zero-pads/truncates a staged or Active `CP_J1939Name` Bytefield to
/// exactly 8 bytes -- the single normalization rule [`resolve_j1939_claim_params`]
/// has always used, extracted (ADR-180 Decision 23, round-25 correction)
/// so the new `CoptUpdateparam` enqueue-time (`rpc_primitive.rs`) and
/// execution-time (`events.rs`) NAME-drift guards compare against the
/// IDENTICAL normalized form `run_j1939_claim_loop` itself will use once it
/// actually issues a claim under this value -- a byte-identical restage
/// (the common "re-stage the whole ComParam set" case) must never spuriously
/// trip either guard just because one side normalized differently than the
/// other. `bytes: None` (the ComParam absent from the snapshot passed in,
/// distinct from a present-but-empty Bytefield) normalizes to the all-zero
/// NAME -- callers deciding whether a NAME change was even REQUESTED must
/// check `Option::is_some` themselves before calling this, not rely on this
/// function to distinguish "absent" from "explicitly staged empty".
pub(in crate::service) fn normalize_j1939_name(bytes: Option<&[u8]>) -> [u8; 8] {
    let mut name = [0u8; 8];
    if let Some(bytes) = bytes {
        let n = bytes.len().min(8);
        name[..n].copy_from_slice(&bytes[..n]);
    }
    name
}

/// `true` when `CP_J1939AddressNegotiationRule`'s bit 1 requests that
/// `PDU_COPT_STARTCOMM` drive an address claim (ADR-179 Decision 3), per
/// ISO 22900-2:2022's own `CP_J1939AddressNegotiationRule` entry: bit 1 is
/// `0` = claim the own address when a STARTCOMM ComPrimitive is received
/// (claim requested), `1` = do not -- the INVERSE polarity of a naive
/// "bit set = requested" reading, confirmed by ISO's own stated default
/// (`0`, i.e. claim-by-default) matching `comparam_defaults.rs`'s already-
/// seeded `PARAM_J1939_ADDR_NEG_RULE = 0` for `j1939_can_common`. `neg_rule`
/// is `None` only when the ComParam was never staged at all (should not
/// happen for a connected J1939 CLL, since the preset always seeds it) --
/// treated as `0` (claim requested), the same default.
pub(in crate::service) fn j1939_claim_requested(neg_rule: Option<u32>) -> bool {
    (neg_rule.unwrap_or(0) >> 1) & 1 == 0
}

/// ADR-180 Decisions 14/15/16 (design-advisor consult, PR #72 round 12)
/// shared predicate: `true` exactly when `link` is a negotiation-ENABLED
/// SAE J1939 CLL (`j1939_claim_requested`, `CP_J1939AddressNegotiationRule`
/// bit 1 clear) that currently has no claimed source address --
/// `link.j1939_claimed_address.is_none()`, true both before this CLL's
/// first successful claim and during a spontaneous-loss-to-reclaim window
/// (`deliver_j1939_claim_indication`'s `spontaneous_loss` arm). `false` for
/// a non-negotiated CLL (bit 1 set), which never runs the claim loop at all
/// and manages `CP_TesterSourceAddress`/`NODE_ADDRESS` client-side instead
/// (the same distinction ADR-180 Decision 12's own `CoptUpdateparam` guard
/// already draws).
///
/// This is the single choke-point predicate for "stale data framed/sent
/// under an address this CLL no longer -- or does not yet -- own", the
/// recurring bug class ADR-180's Decisions 4/6/9/12 and now 14/15/16 have
/// all independently hit; see ADR-180 Decision 13's own gate (this exact
/// three-way `AND`, refactored to call this shared helper) for the
/// Repeat-Messaging-specific precedent this generalizes.
pub(in crate::service) fn j1939_negotiated_unclaimed(link: &LogicalLinkState) -> bool {
    j1939_negotiated_unclaimed_for(link, &link.active)
}

/// [`j1939_negotiated_unclaimed`], but reading `CP_J1939AddressNegotiationRule`
/// from an explicit `params` snapshot instead of always `link.active`.
///
/// `edge-case-hunter` finding (verification pass on PR #72 round 12's own
/// diff): `rpc_primitive.rs::resolve_send_recv_tx`'s own `j1939_tx_source`
/// computation already reads the negotiation-rule ComParam from whichever
/// snapshot ADR-067's `ParamBinding` resolved for THIS call -- Working
/// (`effective`) for a `temp_param_update = 1` CoptSendrecv, Active
/// otherwise -- not unconditionally Active. `rpc_primitive.rs`'s own
/// enqueue-time gate (Decision 14 Part A) must read the SAME snapshot that
/// call's own resolution will use, or the two can disagree about whether
/// this CLL is even "negotiated" for THIS one Temp-bound send: a CLL whose
/// Active `CP_J1939AddressNegotiationRule` differs from what one
/// `temp_param_update` call stages in Working can either (a) pass the gate
/// (Active says non-negotiated) while `resolve_send_recv_tx` still computes
/// a `Some` `j1939_tx_source` from Working, spuriously cancelling the send
/// on its very first cycle once `handle_send_recv`'s per-cycle check runs,
/// or (b) fail the gate (Active says negotiated-and-unclaimed) for a send
/// whose own Working-bound resolution would never have needed a claim at
/// all. Callers pass whichever `ComParamSet` `ParamBinding::resolved()`
/// would hand to `resolve_send_recv_tx` for this exact call.
///
/// ADR-180 Decision 18 (design-advisor consult, PR #72 round 15), corrected
/// by a later design-advisor consult (mirror-image gap, PR #72 review): `params`
/// re-reads `CP_J1939AddressNegotiationRule` from whichever snapshot THIS
/// call staged -- Working, for a `temp_param_update = 1` call, per ADR-067
/// -- which a client can set independently of this CLL's real, structural
/// posture. A client could stage a Working-only `CP_J1939AddressNegotiation
/// Rule` disable (never promoting it to Active via an ordinary
/// `SetComParam`), then issue a `temp_param_update = 1` `CoptSendrecv`: read
/// from `params` alone, this predicate would conclude "not
/// negotiation-managed" and let an unclaimed send through under the
/// default/unclaimed source address, even though this CLL genuinely
/// requested negotiation at its own `CoptStartcomm` and has never claimed an
/// address. `link.j1939_negotiation_posture` cannot be spoofed per-call -- it
/// is set once, from this CLL's own `CoptStartcomm` execution (`events.rs
/// ::handle_start_comm`), not re-derived from a possibly-staged snapshot.
///
/// A two-state (`bool`) posture closes only that one direction, though: a
/// `temp_param_update = 1` `CoptStartcomm` that explicitly opts OUT of
/// negotiation while Active still holds the stale negotiation-ENABLED
/// default left the old flag `false` -- indistinguishable from "no real
/// StartComm has decided yet" -- so this predicate fell back to Active's
/// stale enabled value and wrongly reported "still negotiated and
/// unclaimed" for every later ordinary call on that CLL, permanently
/// blocking it. `J1939NegotiationPosture`'s third state (`OptedOut`) lets a
/// real opt-out StartComm record its decision just as authoritatively as a
/// real opt-in one: once a real `CoptStartcomm` has decided this CLL's
/// posture either way, this predicate trusts that decision directly and
/// stops consulting the (possibly stale or spoofed) `params` snapshot;
/// `Undecided` is the only state that still falls back to `params`.
pub(in crate::service) fn j1939_negotiated_unclaimed_for(
    link: &LogicalLinkState,
    params: &ComParamSet,
) -> bool {
    resources::is_j1939_protocol_id(link.hw_protocol_id)
        && match link.j1939_negotiation_posture {
            J1939NegotiationPosture::Engaged => true,
            J1939NegotiationPosture::OptedOut => false,
            J1939NegotiationPosture::Undecided => {
                j1939_claim_requested(params.unum32.get(&PARAM_J1939_ADDR_NEG_RULE).copied())
            }
        }
        && link.j1939_claimed_address.is_none()
}

/// One step of ADR-179 Decision 3's retry-over-the-candidate-list state
/// machine, given the CLL's `CP_J1939PreferredAddress` list and its current
/// `LogicalLinkState::j1939_claim_cursor`. Pure/no I/O -- unit-testable
/// independent of the actual `protect_j1939_addr` hardware call and
/// RxStatus wait [`run_j1939_claim_loop`] drives around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum J1939ClaimCursorStep {
    /// Attempt this candidate address next.
    Attempt(u8),
    /// The candidate list has no entry at this cursor position -- the claim
    /// fails (StartComm COP failure for the initial attempt; a CLL error
    /// event for a spontaneous post-claim retry, ADR-179 Decision 3).
    Exhausted,
}

pub(super) fn j1939_claim_cursor_step(candidates: &[u8], cursor: usize) -> J1939ClaimCursorStep {
    match candidates.get(cursor) {
        Some(&addr) => J1939ClaimCursorStep::Attempt(addr),
        None => J1939ClaimCursorStep::Exhausted,
    }
}

/// ADR-179 Decision 3 routing step: resolves a claim/defend indication's
/// affected `address` (from `Data[0]`, clause 16.4.6 Table 63) to its
/// owning `cll_handle`, via `claims` (a snapshot of this physical channel's
/// `SharedChannel::j1939_claims`). Drops (`None`) when either no entry
/// exists for `address`, or the entry's recorded `connect_generation` no
/// longer matches that CLL's CURRENT one (`live_generation`) -- a stale
/// indication surviving that CLL's disconnect/reconnect (ADR-086), the same
/// "fails the generation comparison, dropped" outcome every other
/// `connect_generation`-guarded site in this codebase uses.
///
/// Pure -- unit-testable with a plain `HashMap`/closure, independent of the
/// live `logical_links`/`SharedChannel` maps this is extracted from at the
/// real call site ([`deliver_j1939_claim_indication`]).
pub(super) fn resolve_j1939_claim_indication(
    address: u8,
    claims: &HashMap<u8, J1939ClaimEntry>,
    live_generation: impl Fn(u32) -> Option<u64>,
) -> Option<u32> {
    let entry = claims.get(&address)?;
    (live_generation(entry.cll_handle) == Some(entry.connect_generation))
        .then_some(entry.cll_handle)
}

/// Real call site for [`resolve_j1939_claim_indication`]: reads this frame's
/// physical channel's `SharedChannel::j1939_claims`/`j1939_claim_results`,
/// resolves `address` to its owning (live, non-stale) CLL, and records the
/// outcome for [`run_j1939_claim_loop`]'s wait step to observe on its next
/// poll iteration.
///
/// A `Lost` outcome that matches this CLL's CURRENTLY claimed address (not
/// merely a pending attempt) is ADR-179 Decision 3's "later, spontaneous
/// `_LOST`" case (the device was out-defended mid-session, with no
/// `StartComm`-driven wait in flight for it right now): `j1939_claimed_address`
/// is cleared, the cursor reset to `0`, the now-relinquished address's own
/// entry removed from `SharedChannel::j1939_claims` (ADR-180 Decision 4/
/// Codex review PR #72 round 5 -- nothing else ever does for this case: a
/// fresh reclaim starting from cursor `0` may land on an EARLIER candidate
/// than the one just lost, in which case the lost address's stale entry
/// would otherwise never be cleaned up, permanently blocking a sibling from
/// it; no native cancel is needed, clause 16.4.6, the device already
/// relinquished it itself -- same reasoning as `run_j1939_claim_loop`'s own
/// in-flight-wait `Lost` handling, just applied to the spontaneous case),
/// and `cll_handle` is armed in `SharedChannel::j1939_reclaim_pending` for
/// the next `run_due_tick_duties` pass to re-enter the retry loop. A
/// `run_j1939_claim_loop` wait that IS in flight for this exact address
/// still observes the outcome via `j1939_claim_results` first (that map is
/// written unconditionally, before this spontaneous-loss check) and takes
/// priority in practice, since it will consume/clear that entry -- and
/// remove its own routing entry via its own post-wait cleanup, not this
/// path's -- on its very next poll iteration.
pub(super) async fn deliver_j1939_claim_indication(
    ctx: &ChannelPollCtx,
    address: u8,
    claimed: bool,
) {
    let claims = {
        let chans = ctx.service.shared_channels.lock().await;
        let Some(sc) = chans.values().find(|sc| sc.channel_id == ctx.channel_id) else {
            return;
        };
        sc.j1939_claims.clone()
    };

    let mut links = ctx.logical_links.lock().await;
    let Some(cll_handle) = resolve_j1939_claim_indication(address, &claims, |h| {
        links.get(&h).map(|l| l.connect_generation)
    }) else {
        return;
    };
    // `resolve_j1939_claim_indication` only succeeds when `claims` has an
    // entry for `address` (its own `claims.get(&address)?`), so this is
    // exactly the `connect_generation` that entry (and the live
    // `LogicalLinkState` the closure above just matched it against) recorded
    // -- the value `stop_repeat_slots_for_cll` below needs to guard against
    // this same handle having since raced through a disconnect+reconnect.
    let connect_generation = claims[&address].connect_generation;
    let Some(link) = links.get_mut(&cll_handle) else {
        return;
    };
    let spontaneous_loss = !claimed && link.j1939_claimed_address == Some(address);
    if spontaneous_loss {
        link.j1939_claimed_address = None;
        link.j1939_claim_cursor = 0;
    }
    drop(links);

    let mut chans = ctx.service.shared_channels.lock().await;
    let mut stop_repeat_slots = false;
    if let Some(sc) = chans
        .values_mut()
        .find(|sc| sc.channel_id == ctx.channel_id)
    {
        // edge-case-hunter finding: re-verify this exact claim attempt is
        // STILL registered before writing an outcome back. The `claims`
        // snapshot above (and even the `connect_generation` recheck it
        // fed into `resolve_j1939_claim_indication`) can both be stale
        // relative to a `DisconnectComLogicalLink`/`DestroyComLogicalLink`
        // that raced in between and already ran `cancel_j1939_claims_for_
        // cll` for this exact address -- a plain Disconnect does NOT bump
        // `connect_generation` (only a reconnect does, ADR-086), so the
        // generation check alone cannot see it. Without this recheck,
        // writing back here would resurrect a `j1939_claim_results`/
        // `j1939_reclaim_pending` entry for a CLL that just tore down, and
        // (once/if it later reconnects to the same physical channel)
        // `run_j1939_reclaim_duties`'s own staleness guard would then
        // wrongly fire an unsolicited claim negotiation the client never
        // requested. Comparing against the ORIGINAL snapshot (not just
        // "is there an entry") also catches the address being re-registered
        // for a DIFFERENT attempt (same or different CLL) in the interim.
        if sc.j1939_claims.get(&address) != claims.get(&address) {
            return;
        }
        sc.j1939_claim_results.insert(
            cll_handle,
            if claimed {
                J1939ClaimOutcome::Claimed(address)
            } else {
                J1939ClaimOutcome::Lost(address)
            },
        );
        if spontaneous_loss {
            // Codex review finding (PR #72 round 5): remove the lost
            // address's own routing entry here too -- unlike an in-flight
            // wait's own `Lost` outcome (whose post-wait cleanup in
            // `run_j1939_claim_loop` already removes it), nothing else ever
            // does for a SPONTANEOUS loss. A fresh reclaim re-entering from
            // `j1939_claim_cursor = 0` (armed just below) may land on an
            // EARLIER candidate than the one just relinquished; if so, this
            // address's stale entry would otherwise survive forever,
            // permanently blocking a sibling CLL from it. Safe to remove
            // unconditionally here: the recheck just above already
            // confirmed `sc.j1939_claims.get(&address)` is still exactly
            // the snapshot this whole function resolved `cll_handle` from.
            // No native cancel needed (clause 16.4.6): this is an explicit
            // `_LOST`, so the device has already relinquished the address
            // itself -- same reasoning `run_j1939_claim_loop`'s own
            // explicit-`Lost` break already applies, just reached via a
            // different path here.
            sc.j1939_claims.remove(&address);
            // ADR-180 Decision 23 round-26 correction: capture the NAME
            // this claim was actually issued under (`claims[&address].name`
            // -- the pre-removal snapshot, since `sc.j1939_claims`'s own
            // entry is gone as of the line above) into the pending-reclaim
            // record. This is the last point the authoritative entry is
            // available; `run_j1939_reclaim_duties` has no other sound
            // source for "the NAME this claim defends" once it fires.
            sc.j1939_reclaim_pending.insert(
                cll_handle,
                J1939ReclaimPending {
                    connect_generation,
                    name: claims[&address].name,
                },
            );
            stop_repeat_slots = true;
        }
    }
    drop(chans);

    // ADR-180 Decision 10 (design-advisor consult, round 9): a spontaneous
    // `_LOST` is a relinquishment -- stop this CLL's own live repeat slots
    // now, not once/if a reclaim eventually succeeds. See
    // `stop_repeat_slots_for_cll`'s own doc comment for the full rationale.
    //
    // ADR-180 Decision 11 (design-advisor consult, round 10): same
    // relinquishment, a different mechanism -- cancel any live multi-cycle/
    // cyclic CoptSendrecv still transmitting under the lost address. See
    // `cancel_send_recv_cops_for_cll`'s own doc comment.
    if stop_repeat_slots {
        let failed = stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
        record_leaked_repeat_slots(ctx, failed).await;
        cancel_send_recv_cops_for_cll(cll_handle, connect_generation, ctx).await;
    }
}

/// Outcome of [`run_j1939_claim_loop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum J1939ClaimLoopOutcome {
    /// `address` is now claimed and defended by the device.
    Claimed(u8),
    /// `params.candidates` was exhausted (including an empty list) without
    /// a successful claim -- also returned (round-21 correction to Decision
    /// 2) when a timed-out candidate's best-effort native cancel itself
    /// failed, since the loop then fails the whole attempt closed instead of
    /// advancing to another candidate while that one's outcome is still in
    /// doubt.
    Exhausted,
    /// `cll_handle` went stale (disconnected/reconnected, i.e. its
    /// `connect_generation` no longer matches) partway through -- the
    /// caller must not act on any StartComm-success side effect.
    Stale,
    /// The physical channel suffered a hard error mid-wait
    /// (`handle_channel_hard_error` already ran the full loss-of-comms
    /// sequence for it) -- same "do not act on success" obligation as
    /// `Stale`.
    HardError,
    /// ADR-180 Decision 21 (design-advisor consult, Codex review PR #72):
    /// the driving COP (`run_j1939_claim_loop`'s own `cancel_cop` argument)
    /// was cancelled via `CancelComPrimitive` while this loop was waiting.
    /// Only ever reachable when `cancel_cop` is `Some` -- the spontaneous
    /// reclaim call site (`run_j1939_reclaim_duties`, which always passes
    /// `None`) can never observe it. Like `Stale`/`HardError`, the caller
    /// must not act on any StartComm-success side effect; unlike
    /// `Exhausted`, this always takes priority even when a `Claimed`
    /// outcome raced in at the same time (the loop still natively cancels
    /// the just-claimed address in that case).
    Cancelled,
    /// ADR-180 Decision 24 (round-27 correction to Decision 22, `design-
    /// advisor` consult, Codex review PR #72): a `PDU_COPT_STOPCOMM`
    /// against `cll_handle` set `LogicalLinkState::stop_comm_pending = true`
    /// (`rpc_primitive.rs`) while this loop was running. Checked
    /// unconditionally, both at the outer-loop top and inside the inner
    /// wait loop's own per-tick check, regardless of whether `cancel_cop` is
    /// `Some` or `None` -- `stop_comm_pending` is only ever set while
    /// `comm_started == true`, and `CoptStartcomm` is rejected while
    /// `comm_started` is already `true`, so `handle_start_comm`'s own
    /// initial-claim call site (`cancel_cop = Some(cop_handle)`) can never
    /// actually observe this variant; only `run_j1939_reclaim_duties`'s
    /// spontaneous-reclaim call site (`cancel_cop = None`) can. Since
    /// `stop_comm_pending` is only provisional until the StopComm actually
    /// runs, this function's own caller does not treat this as final: it
    /// re-arms this CLL's `j1939_reclaim_pending` entry (round-27 own-round
    /// correction, edge-case-hunter finding) so a later due-tick retries the
    /// reclaim instead of abandoning it -- see that arm's own doc comment
    /// for the full rationale.
    ///
    /// Sibling to [`Cancelled`](Self::Cancelled), not a widening of it:
    /// `Cancelled` is COP-scoped (an explicit client `CancelComPrimitive`
    /// against the driving COP, reported back as `PduCopstCancelled` for
    /// that exact COP) and only ever reachable when `cancel_cop` is `Some`;
    /// `StopCommPending` is session-teardown-scoped (the client is ending
    /// the whole comm session via `CoptStopcomm`, queued behind this
    /// reclaim on the same physical channel's poll task) and reachable only
    /// when `cancel_cop` is `None`. Reusing `Cancelled` here would be wrong
    /// two ways: its `handle_start_comm` call site emits
    /// `PduCopstCancelled` for a COP that does not exist for a spontaneous
    /// reclaim, and reusing [`Stale`](Self::Stale) would be wrong because
    /// `Stale`'s own inner-wait-loop return path skips cancelling the
    /// in-flight native candidate -- correct there only because teardown
    /// already owns cleanup for a genuinely stale/torn-down CLL, which is
    /// not the case here (the CLL is still live, just ending its comm
    /// session in an orderly way).
    StopCommPending,
}

/// Drives SAE J2534-2 clause 16's address-claim/defend retry loop (ADR-179
/// Decision 3) for `cll_handle`, starting from its CURRENT
/// `LogicalLinkState::j1939_claim_cursor` -- callers reset this to `0`
/// first for a fresh attempt (both `handle_start_comm`'s initial call and a
/// spontaneous post-claim-loss reclaim do, via `deliver_j1939_claim_
/// indication`/the caller driving `run_due_tick_duties`).
///
/// `cancel_cop` (ADR-180 Decision 21, design-advisor consult, Codex review
/// PR #72): the COP handle whose explicit `CancelComPrimitive` should abort
/// this wait, or `None` when no COP drives this call at all.
/// `handle_start_comm`'s initial-claim call site passes `Some(cop_handle)`
/// (the driving `PDU_COPT_STARTCOMM`); `run_j1939_reclaim_duties`'s
/// spontaneous post-claim-loss reclaim passes `None`, since no COP drives
/// it. When `Some`, this loop's own `LogicalLinkState::cancelled_cops`
/// check (folded into the existing per-iteration `still_on_this_channel`
/// staleness checks, both the outer-loop-top one and the inner wait loop's
/// own per-tick one, mirroring `handle_delay`'s own `(was_cancelled,
/// is_stale)` tuple idiom) can end the wait early with
/// [`J1939ClaimLoopOutcome::Cancelled`] instead of running the bounded wait
/// out to `params.timeout` (up to `candidate_count * timeout` total) after
/// the client has already asked to cancel. See that outcome variant's own
/// doc comment for the full priority-over-`Claimed` rationale.
///
/// For each candidate address from the cursor onward: issues
/// `api.protect_j1939_addr`, registers the attempt in this physical
/// channel's `SharedChannel::j1939_claims` routing map (so `deliver_j1939_
/// claim_indication` can find it), then actively drives `poll_rx` --
/// mirroring `wait_for_p3_gap`'s own "sleep `POLL_INTERVAL_MS`, poll,
/// recheck" shape -- until either a matching outcome lands in
/// `SharedChannel::j1939_claim_results` for this `cll_handle`, or
/// `params.timeout` elapses.
///
/// A `Lost` outcome (explicit, a failed `protect_j1939_addr` issue, or an
/// elapsed timeout with no indication at all -- see the note below)
/// advances `j1939_claim_cursor` (persisted immediately, so a caller that
/// itself goes stale mid-retry still leaves the cursor at a sensible resume
/// point) and retries the next candidate, rechecking `still_on_this_channel`
/// (this CLL's `channel_id`/`connect_generation`) before each such
/// hardware-`.await` boundary -- the same idiom `handle_start_comm`'s other
/// multi-step paths already use throughout this file. The one exception
/// (round-21 correction to Decision 2, `design-advisor` consult): an
/// elapsed timeout whose own best-effort native cancel itself fails does
/// NOT advance -- the candidate's outcome is still in doubt, and
/// `SharedChannel::j1939_claim_results` has no way to route a late
/// indication for it once this loop moves on to waiting on a different
/// address, so the whole attempt fails closed (`Exhausted`) instead,
/// leaving reconciliation to whichever cancel owner (a fresh attempt's own
/// sweep, or teardown) runs next for this CLL.
///
/// **Uncertainty flagged for review**: ADR-179 Decision 3 specifies the
/// bounded wait for "the matching indication for that address" but does not
/// say what a wait that times out with NO indication at all (neither
/// `Claimed` nor `Lost`) should do. This function treats that case the same
/// as an explicit `Lost` (advance and retry the next candidate) rather than
/// failing the whole loop outright -- the more forgiving reading, and
/// consistent with every other failure outcome using the same
/// retry-next-candidate path -- but it is a genuine interpretation, not a
/// directly-specified behavior; a different, fail-outright reading is
/// equally defensible and was not ruled out by the ADR.
///
/// Likewise, a synchronous `protect_j1939_addr` issue failure (e.g. the
/// device rejects a malformed candidate) is treated as this candidate's
/// attempt failing -- advance and retry -- rather than failing the whole
/// loop; also not directly specified by ADR-179, flagged for the same
/// reason.
pub(super) async fn run_j1939_claim_loop(
    cll_handle: u32,
    connect_generation: u64,
    cancel_cop: Option<u32>,
    params: &J1939ClaimParams,
    ctx: &ChannelPollCtx,
) -> J1939ClaimLoopOutcome {
    // edge-case-hunter finding: an all-zero `CP_J1939Name` -- what an
    // absent/never-staged `CP_J1939Name` resolves to by
    // `resolve_j1939_claim_params`'s own zero-padding rule, and this
    // service's OWN seeded default (`comparam_defaults.rs::
    // j1939_can_common`) -- is not a valid claim NAME at all: clause
    // 16.3.3.2 defines all-NAME-bytes-zero as the wire-level CANCEL form
    // of `PROTECT_J1939_ADDR`, not a claim (confirmed against the mock's
    // own `is_cancel` check). Issuing it here would silently send a
    // no-op cancel for every candidate -- no indication ever arrives, each
    // attempt times out (the already-documented "no indication" ->
    // treated-as-Lost interpretation above), and the whole list exhausts
    // with no diagnostic pointing at the real cause. Fail closed
    // immediately instead, mirroring this same function's existing
    // "empty candidate list -> immediately Exhausted" precedent
    // (`resolve_j1939_claim_params`'s own doc comment) rather than
    // silently treating an unconfigured NAME as a series of doomed
    // attempts.
    if params.name == [0u8; 8] {
        warn!(
            cll_handle,
            "SAE J1939 address claim requested with an all-zero CP_J1939Name (unconfigured) -- \
             that byte pattern is PROTECT_J1939_ADDR's wire-level cancel form, not a valid \
             claim, per clause 16.3.3.2; failing the claim immediately instead of silently \
             issuing no-op cancels for every candidate"
        );
        return J1939ClaimLoopOutcome::Exhausted;
    }
    loop {
        // ADR-180 Decision 21: fold the `cancel_cop` check into this SAME
        // `logical_links` acquisition, mirroring `handle_delay`'s own
        // `(was_cancelled, is_stale)` tuple idiom (`events.rs`) -- one lock
        // acquisition, not two. Reached here at the top of each outer-loop
        // iteration, no candidate is registered/in-flight yet (the previous
        // one, if any, already fully resolved), so a cancellation observed
        // here can return `Cancelled` directly with no cleanup to do.
        let (was_cancelled, is_stale, stop_comm_pending) = {
            let mut links = ctx.logical_links.lock().await;
            let cancelled = cancel_cop
                .and_then(|cop| {
                    links
                        .get_mut(&cll_handle)
                        .map(|l| l.cancelled_cops.remove(&cop))
                })
                .unwrap_or(false);
            let stale = !links.get(&cll_handle).is_some_and(|l| {
                l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
            });
            // ADR-180 Decision 24: read in the SAME `logical_links`
            // acquisition as `cancelled`/`stale` above, unconditionally
            // (see [`J1939ClaimLoopOutcome::StopCommPending`]'s own doc
            // comment for why this is safe to check regardless of which
            // call site reached this loop).
            let stop_comm_pending = links.get(&cll_handle).is_some_and(|l| l.stop_comm_pending);
            (cancelled, stale, stop_comm_pending)
        };
        if was_cancelled {
            return J1939ClaimLoopOutcome::Cancelled;
        }
        if is_stale {
            return J1939ClaimLoopOutcome::Stale;
        }
        // ADR-180 Decision 24: nothing registered/in-flight yet at the
        // outer-loop top, so this can `return` directly -- mirrors the
        // `was_cancelled`/`is_stale` early returns just above.
        if stop_comm_pending {
            return J1939ClaimLoopOutcome::StopCommPending;
        }

        let cursor = {
            let links = ctx.logical_links.lock().await;
            links.get(&cll_handle).map_or(0, |l| l.j1939_claim_cursor)
        };
        let address = match j1939_claim_cursor_step(&params.candidates, cursor) {
            J1939ClaimCursorStep::Attempt(addr) => addr,
            J1939ClaimCursorStep::Exhausted => return J1939ClaimLoopOutcome::Exhausted,
        };

        // Codex review finding (PR #72 round 7): 254 and 255 are reserved
        // SAE J1939 addresses -- clause 16.3.3.2 defines 255 as the
        // conceptual power-on default only, never a valid explicit claim
        // target, and this module's own design intent (ADR-179 Decision 3's
        // own "never byte[0] = 255... both 254 and 255 return
        // ERR_INVALID_IOCTL_VALUE if ever issued as an explicit claim
        // target" note) already documented that these must never reach the
        // native IOCTL -- but the implementation never actually enforced
        // it, silently relying on the native call's own rejection and
        // treating that rejection as an ordinary lost/retry outcome
        // (`issue_result`'s `Err` arm below), which hides a malformed
        // `CP_J1939PreferredAddress` configuration behind adapter-dependent
        // behavior instead of surfacing it. Skip straight to the next
        // candidate -- mirroring the `owned_by_a_live_sibling` skip below's
        // own shape -- without ever entering the critical section or
        // issuing a native call for a candidate this loop already knows
        // locally is invalid.
        if address == 254 || address == 255 {
            warn!(
                cll_handle,
                address,
                "CP_J1939PreferredAddress candidate is a reserved SAE J1939 address (254/255 \
                 are never valid explicit claim targets, clause 16.3.3.2); skipping without \
                 issuing a native claim"
            );
            if !advance_j1939_claim_cursor(cll_handle, ctx.channel_id, connect_generation, ctx)
                .await
            {
                return J1939ClaimLoopOutcome::Stale;
            }
            continue;
        }

        // Codex review findings (PR #72 rounds 1-3; ADR-180 amending
        // ADR-179 Decision 3): the sibling-ownership check, the native
        // claim issue, and this attempt's own registration in
        // `SharedChannel::j1939_claims` are now one ADR-080-conformant
        // `shared_channels`-held critical section (ADR-080: `shared_channels`
        // outermost, `logical_links`/`api` nested underneath it) instead of
        // several separate lock acquisitions with real hardware I/O and
        // `.await` points in between them. This closes, rather than merely
        // narrows, two related TOCTOU races prior rounds' fixes only
        // mitigated:
        // - A sibling registering this exact address in the gap between a
        //   pre-issue-only sibling-ownership check and this loop's own
        //   insert (round 1's original fix only checked before issuing, not
        //   atomically with the write).
        // - `DisconnectComLogicalLink`'s `cancel_j1939_claims_for_cll`
        //   (`rpc_link.rs`) landing between the native issue and the
        //   registration write, finding no entry to cancel, while this loop
        //   then inserts one AFTER teardown already ran -- orphaned forever,
        //   since that cancellation pass runs exactly once, at disconnect
        //   time. Round 2's fix only narrowed this window (a staleness
        //   recheck immediately before the write, but still under a
        //   SEPARATE lock acquisition); round 3 found it insufficient.
        // `rpc_disconnect_com_logical_link`/`rpc_destroy_com_logical_link`
        // (`rpc_link.rs`) already acquire `shared_channels` first and hold it
        // continuously across their own `logical_links` mutation and
        // `cancel_j1939_claims_for_cll` call (ADR-080), so holding it here
        // too fully serializes the two sequences: this attempt's
        // registration either happens-before teardown's cancellation pass
        // (which then sees and cancels it), or happens-after teardown
        // already marked this CLL stale (in which case the nested staleness
        // check below stops this attempt from ever being issued or
        // registered at all). `protect_j1939_addr` is a thin, non-blocking
        // native primitive (ADR-179 Context), so holding `shared_channels`
        // across it matches ADR-080's own accepted-cost precedent (e.g.
        // `ioctl_start_msg_filter`'s hardware filter install under the same
        // guard). The section ends (see the `drop(chans)` calls below)
        // before the wait loop begins -- `poll_rx`/
        // `deliver_j1939_claim_indication` acquire `shared_channels`
        // themselves.
        let mut chans = ctx.service.shared_channels.lock().await;
        let Some(sc) = chans
            .values_mut()
            .find(|sc| sc.channel_id == ctx.channel_id)
        else {
            return J1939ClaimLoopOutcome::Stale;
        };

        let still_on_this_channel = {
            let links = ctx.logical_links.lock().await;
            links.get(&cll_handle).is_some_and(|l| {
                l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
            })
        };
        if !still_on_this_channel {
            drop(chans);
            return J1939ClaimLoopOutcome::Stale;
        }

        // ADR-180 Decision 22's round-23 correction (Codex review, PR #72;
        // `design-advisor` consult): before considering the current
        // candidate at all, reconcile EVERY address in this channel's
        // `SharedChannel::leaked_j1939_claims` (a PRIOR `cancel_j1939_claims_
        // for_cll` cancel-then-remove whose native cancel failed after its
        // routing entry was already gone -- see that set's own field doc,
        // `service.rs`). The original mechanism only checked whether the
        // CURRENT candidate's own address was leaked -- a fresh attempt
        // whose `CP_J1939PreferredAddress` list simply does not include the
        // leaked address (a changed candidate list, or a leak left behind by
        // a DIFFERENT CLL's teardown on this same physical channel) bypassed
        // it entirely, issuing a fresh native claim for an unrelated address
        // while the adapter might still defend the leaked one -- the same
        // two-claimants violation this whole ADR exists to close, reached by
        // skipping the guard rather than failing it. Retries every leaked
        // entry via [`reconcile_leaked_j1939_claims`] (round-24: extracted
        // so the negotiation opt-out branch, `events.rs`, can share this
        // exact retry logic); if ANY entry survives, this whole claim
        // attempt fails closed (`Exhausted`) without ever reaching the
        // native claim issue below -- the claim loop remains the leaked
        // set's own retry owner (no new periodic duty), so failing without
        // retrying here would permanently wedge the channel after one
        // transient cancel failure. Same lock order as the native claim
        // issue a few lines below (`api` nested under this already-held
        // `shared_channels` guard, the same ADR-080 accepted-cost class the
        // teardown sweep's own per-address native cancel loop already uses
        // under the same hold).
        if !sc.leaked_j1939_claims.is_empty() {
            let api = ctx.api.lock().await;
            let reconciled = reconcile_leaked_j1939_claims(sc, &api, ctx.channel_id, cll_handle);
            drop(api);
            if !reconciled {
                drop(chans);
                // edge-case-hunter finding (round-23 verification pass, PR
                // #72): this is a brand-new early-return point in the
                // middle of the loop's critical section, reached only after
                // an `.await` on `ctx.api.lock()` -- unlike every other exit
                // in this function, it had no recheck of `cancelled_cops`
                // immediately before returning, so a `CancelComPrimitive`
                // landing during the retry-cancel `.await` above would be
                // silently swallowed: the caller's `Exhausted` arm never
                // re-consults `cancelled_cops` either, so the client would
                // see a contradictory `PduCopstFinished` instead of the
                // `PduCopstCancelled` ADR-180 Decision 21 exists to
                // guarantee. No candidate is registered/in-flight yet at
                // this point (mirroring the outer-loop-top's own "nothing
                // to clean up" case), so a cancellation observed here can
                // return `Cancelled` directly, the same as that check does.
                if let Some(cop) = cancel_cop {
                    let mut links = ctx.logical_links.lock().await;
                    if links
                        .get_mut(&cll_handle)
                        .is_some_and(|l| l.cancelled_cops.remove(&cop))
                    {
                        return J1939ClaimLoopOutcome::Cancelled;
                    }
                }
                warn!(
                    cll_handle,
                    "failing this SAE J1939 claim attempt closed until every leaked claim on \
                     this physical channel reconciles, rather than risking a second address \
                     claimed alongside one the adapter may still be defending"
                );
                return J1939ClaimLoopOutcome::Exhausted;
            }
        }

        // ADR-180 Decision 23 (design-advisor consult, Codex review PR #72):
        // this attempt's own NAME (`params.name`) must not already be live
        // (or pending) under a DIFFERENT, still-live sibling CLL on this
        // same physical channel -- regardless of which address that sibling
        // holds. `owned_by_a_live_sibling` below only ever checks the
        // CURRENT candidate's own address, so two sibling CLLs sharing a
        // NAME but configured with disjoint `CP_J1939PreferredAddress`
        // candidate lists could each successfully claim a DIFFERENT
        // address, leaving the adapter defending one SAE J1939 identity at
        // two source addresses simultaneously -- clause 16's actual
        // invariant is one NAME defends at most one address, a distinct
        // dimension from "one address has at most one owner" the existing
        // checks alone never covered. Checked BEFORE `owned_by_a_live_
        // sibling` (candidate-independent -- skipping to the next candidate
        // would just re-hit the identical collision on every remaining one,
        // burning cursor advances to reach the same outcome) and AFTER the
        // leaked-set gate above (a purely local check, no native I/O, cheap
        // relative to that gate's own retry-cancel FFI calls). A collision
        // fails the WHOLE attempt closed (`Exhausted`), the leaked-gate's
        // own shape -- not skip-candidate, for the same "candidate-
        // independent" reason above. Deliberately never cancels the
        // sibling's own claim ("relinquish"): no cancel owner in this
        // ADR's invariant ever cancels a claim a DIFFERENT, still-live CLL
        // is not itself relinquishing -- doing so here would reintroduce
        // the round-1 routing-steal bug class and silently un-defend an
        // address the sibling may be actively transmitting under. The
        // client-side remedy (the sibling relinquishes via StopComm or
        // disconnect) is named in the rejection diagnostic instead.
        //
        // Same-handle entries are excluded (a stale generation already
        // fails the liveness check below regardless), so this can never
        // self-block the reclaim path or the round-20/21 in-doubt-retained-
        // entry case.
        //
        // `j1939_claims` has no uniqueness constraint on `name` (unlike
        // address, its own key) -- more than one entry can carry the same
        // NAME across different addresses. `edge-case-hunter` finding
        // (round-25 verification pass): checking only the FIRST colliding
        // entry `HashMap` iteration order happens to yield would wrongly
        // pass this gate if that first entry were stale while a second,
        // live, same-NAME entry also existed. Collects every colliding
        // candidate and checks EACH ONE's liveness, so any live collision
        // -- not just the first found -- blocks this attempt.
        let name_collisions: Vec<J1939ClaimEntry> = sc
            .j1939_claims
            .values()
            .filter(|entry| entry.name == params.name && entry.cll_handle != cll_handle)
            .copied()
            .collect();
        let name_owned_by_a_live_sibling = if name_collisions.is_empty() {
            false
        } else {
            let links = ctx.logical_links.lock().await;
            name_collisions.iter().any(|entry| {
                links
                    .get(&entry.cll_handle)
                    .is_some_and(|l| l.connect_generation == entry.connect_generation)
            })
        };
        if name_owned_by_a_live_sibling {
            drop(chans);
            // Same edge-case-hunter-established shape as the leaked-gate's
            // own `Exhausted` return just above: this is a post-`.await`
            // early return, so recheck `cancelled_cops` immediately before
            // deciding the outcome -- no candidate is registered/in-flight
            // yet at this point, so a cancellation observed here can return
            // `Cancelled` directly.
            if let Some(cop) = cancel_cop {
                let mut links = ctx.logical_links.lock().await;
                if links
                    .get_mut(&cll_handle)
                    .is_some_and(|l| l.cancelled_cops.remove(&cop))
                {
                    return J1939ClaimLoopOutcome::Cancelled;
                }
            }
            warn!(
                cll_handle,
                "failing this SAE J1939 claim attempt closed -- CP_J1939Name is already \
                 claimed or pending under a different, live sibling ComLogicalLink on this \
                 physical channel; relinquish the sibling's claim (CoptStopcomm or disconnect) \
                 before configuring the same CP_J1939Name here"
            );
            return J1939ClaimLoopOutcome::Exhausted;
        }

        // If this exact address is already registered to a DIFFERENT,
        // still-live CLL on this same physical channel (sibling CLLs with
        // overlapping `CP_J1939PreferredAddress` lists), do not attempt to
        // claim it -- unconditionally overwriting the entry below (as this
        // loop used to) would silently steal indication routing away from
        // the CLL that already legitimately owns or is attempting this
        // address: a later `_LOST` for it would then be delivered only to
        // the new (wrong) claimant, while the true owner keeps transmitting
        // under an address it can no longer detect losing, and can never
        // reclaim it either. Skip straight to the next candidate instead --
        // never even issue a native claim for this one, since we already
        // know locally it is spoken for.
        let owned_by_a_live_sibling = match sc.j1939_claims.get(&address).copied() {
            Some(entry) if entry.cll_handle != cll_handle => {
                let links = ctx.logical_links.lock().await;
                links
                    .get(&entry.cll_handle)
                    .is_some_and(|l| l.connect_generation == entry.connect_generation)
            }
            _ => false,
        };
        if owned_by_a_live_sibling {
            drop(chans);
            if !advance_j1939_claim_cursor(cll_handle, ctx.channel_id, connect_generation, ctx)
                .await
            {
                return J1939ClaimLoopOutcome::Stale;
            }
            continue;
        }

        let issue_result = {
            let api = ctx.api.lock().await;
            api.protect_j1939_addr(ctx.channel_id, address, params.name)
        };
        if let Err(err) = issue_result {
            warn!(
                cll_handle,
                address,
                %err,
                "PROTECT_J1939_ADDR issue failed; treating this CP_J1939PreferredAddress \
                 candidate as failed and retrying the next one"
            );
            drop(chans);
            if !advance_j1939_claim_cursor(cll_handle, ctx.channel_id, connect_generation, ctx)
                .await
            {
                return J1939ClaimLoopOutcome::Stale;
            }
            continue;
        }

        sc.j1939_claims.insert(
            address,
            J1939ClaimEntry {
                cll_handle,
                connect_generation,
                name: params.name,
            },
        );
        sc.j1939_claim_results.remove(&cll_handle);
        drop(chans);

        let deadline = tokio::time::Instant::now() + params.timeout;
        let poll_interval = Duration::from_millis(POLL_INTERVAL_MS);
        // The inner loop only ever exits via an early `return` (Claimed/
        // Stale/HardError) or a `break` on a Lost/timeout/Cancelled outcome
        // -- so reaching the code after it always means "this candidate did
        // not end in a live claim, decide what to do next." `WaitEnd`
        // distinguishes WHICH of the three `break`s happened (Codex review,
        // PR #72 round 3; extended for ADR-180 Decision 21): an explicit
        // `Lost` means the device itself already relinquished the address
        // (clause 16.4.6), so no cancel is needed here -- but a bounded-wait
        // timeout with NO indication at all, or an explicit
        // `CancelComPrimitive` on the driving COP, both mean the adapter may
        // still be processing or defending the candidate non-blocking, so
        // this loop must cancel it itself before moving on, or nothing in
        // this service's own bookkeeping ever points at it again once the
        // local routing entry below is removed.
        enum WaitEnd {
            Lost,
            TimedOut,
            Cancelled,
            /// ADR-180 Decision 24: sibling to `Cancelled`, but session-
            /// teardown-scoped (a pending `CoptStopcomm`) rather than
            /// COP-scoped -- see [`J1939ClaimLoopOutcome::StopCommPending`]'s
            /// own doc comment for the full distinction.
            StopComm,
        }
        let wait_end;
        loop {
            // ADR-180 Decision 21: same `(was_cancelled, is_stale)` tuple
            // idiom as the outer-loop-top check above -- but this candidate
            // DOES have a live `j1939_claims` registration and an issued
            // native claim, so a cancellation observed here cannot `return`
            // immediately; it must fall through to the SAME cleanup block a
            // timeout already uses, so the native claim is cancelled first.
            // ADR-180 Decision 24 extends this with `stop_comm_pending`,
            // read in the same acquisition, for the identical reason.
            let (was_cancelled, is_stale, stop_comm_pending) = {
                let mut links = ctx.logical_links.lock().await;
                let cancelled = cancel_cop
                    .and_then(|cop| {
                        links
                            .get_mut(&cll_handle)
                            .map(|l| l.cancelled_cops.remove(&cop))
                    })
                    .unwrap_or(false);
                let stale = !links.get(&cll_handle).is_some_and(|l| {
                    l.channel_id == Some(ctx.channel_id)
                        && l.connect_generation == connect_generation
                });
                let stop_comm_pending = links.get(&cll_handle).is_some_and(|l| l.stop_comm_pending);
                (cancelled, stale, stop_comm_pending)
            };
            if was_cancelled {
                wait_end = WaitEnd::Cancelled;
                break;
            }
            if is_stale {
                return J1939ClaimLoopOutcome::Stale;
            }
            // ADR-180 Decision 24: this candidate DOES have a live
            // `j1939_claims` registration and an issued native claim here,
            // so (mirroring `was_cancelled` just above, not the outer-loop-
            // top's own direct `return`) this falls through to the SAME
            // cleanup block, cancelling the in-flight candidate before
            // returning `StopCommPending`.
            if stop_comm_pending {
                wait_end = WaitEnd::StopComm;
                break;
            }

            // ADR-180 Decision 21 (Raced-Claimed rule): the cancellation
            // check above runs BEFORE this outcome-drain read on every
            // iteration, so a cancel that races an already-delivered
            // `Claimed` outcome is resolved in favor of `Cancelled` -- a COP
            // that reports `PduCopstCancelled` must never also produce
            // `COMM_STARTED`. The cleanup block below still natively
            // cancels the just-claimed address in that case.
            let outcome = {
                let mut chans = ctx.service.shared_channels.lock().await;
                chans
                    .values_mut()
                    .find(|sc| sc.channel_id == ctx.channel_id)
                    .and_then(|sc| sc.j1939_claim_results.remove(&cll_handle))
            };
            match outcome {
                Some(J1939ClaimOutcome::Claimed(addr)) if addr == address => {
                    return J1939ClaimLoopOutcome::Claimed(address);
                }
                Some(J1939ClaimOutcome::Lost(addr)) if addr == address => {
                    wait_end = WaitEnd::Lost;
                    break;
                }
                // A mismatched outcome for a different address should not
                // happen given the per-address routing map, but is dropped
                // defensively (this enum's own doc comment) rather than
                // acted on.
                _ => {}
            }

            if tokio::time::Instant::now() >= deadline {
                wait_end = WaitEnd::TimedOut;
                break;
            }
            // Codex review finding (PR #72 round 4, P2): mirror the
            // periodic per-tick maintenance every other long-running wait
            // loop in this file performs (e.g. `isotp_send`'s N_Bs wait,
            // `events.rs`) -- without it, a claim attempt's bounded wait
            // (up to `CP_J1939AddrClaimTimeout` PER candidate, so up to
            // `candidate_count * timeout` total) holds this physical
            // channel's single poll task for the whole wait performing
            // only RX polling, starving sibling CLL tester-present
            // dispatch and detached-registrant reap the entire time --
            // `run_due_tick_duties` (this channel's own periodic-duties
            // entry point) never runs while this inner loop is sleeping,
            // since it never returns control back up to the outer poll
            // loop. No `Box::pin` needed here (unlike the `isotp_send`
            // site): `dispatch_due_tester_present`'s own call graph never
            // reaches back into this claim loop, so there is no
            // async-fn recursion cycle to break. `exclude_cll`/
            // `exclude_isotp_target` are both irrelevant here -- this call
            // site is reached from both `handle_start_comm`'s initial-claim
            // wait (where `comm_started` genuinely is still `false`) and
            // `run_j1939_reclaim_duties`'s spontaneous-reclaim wait (where
            // the CLL stays `comm_started == true` throughout, so a
            // `comm_started`-only argument would NOT exclude it there).
            // ADR-180 Decision 16 (design-advisor consult, PR #72 round 12,
            // Finding 3) closes that gap correctly for both call sites at
            // once: `dispatch_due_tester_present`'s own filter now also
            // excludes any negotiation-enabled J1939 CLL with no CURRENTLY
            // claimed address (`j1939_negotiated_unclaimed`) -- true for
            // this CLL for this entire wait loop's duration regardless of
            // which call site reached it, since neither an initial claim
            // nor a spontaneous reclaim has `j1939_claimed_address`
            // `Some` yet. Same plain-periodic shape as
            // `run_due_tick_duties`'s own call, not the in-flight-transfer
            // shape `isotp_send` needs.
            run_detached_registrant_maintenance(ctx).await;
            dispatch_due_tester_present(false, None, None, false, ctx).await;
            tokio::time::sleep(poll_interval).await;
            if !poll_rx(ctx).await.is_ok() {
                return J1939ClaimLoopOutcome::HardError;
            }
        }

        let mut cancel_failed = false;
        {
            let mut chans = ctx.service.shared_channels.lock().await;
            if let Some(sc) = chans
                .values_mut()
                .find(|sc| sc.channel_id == ctx.channel_id)
                // edge-case-hunter finding (PR #72 round 4, minor): reverify
                // this exact attempt still owns `address` before
                // cancelling/removing it, mirroring
                // `deliver_j1939_claim_indication`'s own recheck against a
                // snapshot elsewhere in this file. Not currently reachable
                // -- every mutator of `j1939_claims` for a DIFFERENT
                // `cll_handle` only ever runs from this same physical
                // channel's single poll task, which cannot interleave with
                // this loop's own execution -- but this keeps that safety
                // property an explicit, checked condition in the code
                // rather than an unstated concurrency invariant a future
                // refactor could silently break.
                && sc.j1939_claims.get(&address).is_some_and(|entry| {
                    entry.cll_handle == cll_handle && entry.connect_generation == connect_generation
                })
            {
                // Round-20 correction to Decision 2 (Codex review, PR #72),
                // extended by ADR-180 Decision 21 for `Cancelled` and
                // Decision 24 for `StopComm`: the local entry is now
                // removed ONLY when there is no reason to believe the
                // adapter is still defending this address -- an explicit
                // `Lost` (device already relinquished it, clause 16.4.6,
                // `WaitEnd::Lost`) or a `TimedOut`/`Cancelled`/`StopComm`
                // cancel that actually succeeded. A FAILED cancel leaves the
                // entry in place, still attributed to this exact
                // `(cll_handle, connect_generation)`: the adapter may still
                // be processing or defending the candidate non-blocking
                // (this arm's own original rationale for issuing the cancel
                // at all), so forgetting it here would let a sibling's
                // claim loop treat the address as free and issue its own
                // native claim for it -- two claimants, one address, exactly
                // the bug class this whole ADR exists to close. The loop
                // terminates the whole attempt instead of advancing past an
                // in-doubt address (see the round-21 correction below) --
                // reconciliation ownership passes to whichever of this ADR's
                // existing cancel owners runs next for this CLL (a fresh
                // claim attempt's own `cancel_j1939_claims_for_cll` sweep,
                // Decision 4; or teardown, Decision 1), not a NEW, unowned
                // leak.
                if matches!(
                    wait_end,
                    WaitEnd::TimedOut | WaitEnd::Cancelled | WaitEnd::StopComm
                ) {
                    // Cancel BEFORE any removal: with the registration fix
                    // above, a sibling can only register this exact address
                    // once the entry is gone, so cancelling while it still
                    // blocks a sibling excludes a "our late cancel kills a
                    // sibling's fresh claim" interleave (unlike
                    // `cancel_j1939_claims_for_cll`'s own remove-then-cancel
                    // order, which is safe there only because ITS caller
                    // already holds `shared_channels` throughout its own
                    // remove-and-cancel sequence -- same guarantee,
                    // different ordering).
                    let api = ctx.api.lock().await;
                    if let Err(err) = api.cancel_j1939_addr_protect(ctx.channel_id, address) {
                        warn!(
                            cll_handle,
                            address,
                            %err,
                            client_cancelled = matches!(wait_end, WaitEnd::Cancelled),
                            stop_comm_pending = matches!(wait_end, WaitEnd::StopComm),
                            "PROTECT_J1939_ADDR cancel failed for a candidate whose bounded \
                             wait ended without a CLAIMED/LOST indication (timeout, an explicit \
                             CancelComPrimitive on the driving COP, or a pending CoptStopcomm on \
                             this ComLogicalLink) -- retaining the local registration rather than \
                             forgetting a possibly-still-defended address or advancing to another \
                             candidate while this one's outcome is still in doubt (best-effort \
                             cancel only)"
                        );
                        cancel_failed = true;
                    }
                }
                if !cancel_failed {
                    sc.j1939_claims.remove(&address);
                }
                // ADR-180 Decision 21 (extended by Decision 24 for
                // `StopComm`): a cancelled COP, or a pending CoptStopcomm,
                // drops a possibly-raced, unconsumed late outcome for this
                // `cll_handle` here too -- regardless of whether the native
                // cancel above succeeded -- so a delayed `Claimed`/`Lost`
                // indication that lands after this loop has already returned
                // is never mistaken for a later, unrelated wait's own
                // outcome.
                if matches!(wait_end, WaitEnd::Cancelled | WaitEnd::StopComm) {
                    sc.j1939_claim_results.remove(&cll_handle);
                }
            }
        }
        // Round-21 correction to Decision 2 (Codex review, PR #72;
        // `design-advisor` consult): a failed timeout-cancel used to still
        // advance the cursor and wait on the next candidate. But
        // `SharedChannel::j1939_claim_results` is keyed by `cll_handle`
        // alone, not by address -- if the adapter's delayed response for
        // THIS retained candidate arrives while the loop is now waiting on
        // the NEXT one, the inner wait loop's own address match fails and
        // silently drops it (the catch-all arm below `Some(J1939ClaimOutcome
        // ::Lost(addr)) if addr == address`). A late `Claimed` for the
        // retained address then sits unconsumed while the next candidate
        // might ALSO succeed -- this NAME would then be defended at two
        // source addresses simultaneously (a real SAE J1939 clause-16
        // protocol violation), undetected until whichever cancel owner next
        // runs for this CLL. Failing the whole attempt closed here instead
        // makes that double-claim state unreachable, rather than merely
        // narrowing the window: this loop never advances while a candidate's
        // outcome is unresolved. The retained `j1939_claims` entry above is
        // already the in-doubt record; a fresh claim attempt's own
        // `cancel_j1939_claims_for_cll` sweep (Decision 4) or teardown
        // (Decision 1) reconciles it -- both already re-cancel, deregister,
        // and clear any stale `j1939_claim_results`/`j1939_reclaim_pending`
        // for this CLL, so no new field or routing change is needed.
        //
        // ADR-180 Decision 21: `Cancelled` ALWAYS returns `Cancelled` here,
        // regardless of `cancel_failed` -- unlike `TimedOut`, whose failed
        // cancel only fails the attempt closed (`Exhausted`) while a
        // SUCCESSFUL one advances to the next candidate. A COP the client
        // explicitly cancelled must always terminate as `Cancelled`, never
        // silently advance to a different candidate nor report `Exhausted`
        // (which the caller would otherwise treat as an ordinary ADR-179
        // claim failure, not an honoured cancellation) -- on a failed native
        // cancel here, the retained entry above is the in-doubt record,
        // reconciled by whichever cancel owner runs next for this CLL, same
        // as `TimedOut`'s own failed-cancel case.
        if matches!(wait_end, WaitEnd::Cancelled) {
            return J1939ClaimLoopOutcome::Cancelled;
        }
        // ADR-180 Decision 24: `StopComm` ALWAYS returns `StopCommPending`
        // here too, regardless of `cancel_failed` -- the identical
        // unconditional-return precedent `Cancelled` just established above,
        // for the identical reason (the client is ending the comm session
        // either way; a failed cancel here just means the retained entry
        // above is the in-doubt record, reconciled by whichever cancel
        // owner runs next for this CLL).
        if matches!(wait_end, WaitEnd::StopComm) {
            return J1939ClaimLoopOutcome::StopCommPending;
        }
        if cancel_failed {
            return J1939ClaimLoopOutcome::Exhausted;
        }
        if !advance_j1939_claim_cursor(cll_handle, ctx.channel_id, connect_generation, ctx).await {
            return J1939ClaimLoopOutcome::Stale;
        }
    }
}

/// Advances `cll_handle`'s persisted `j1939_claim_cursor` by one, but only
/// if it is still live (`still_on_this_channel`) -- returns `false` (and
/// does NOT advance) when stale, so [`run_j1939_claim_loop`]'s caller can
/// bail out the same way every other guard in this file does.
async fn advance_j1939_claim_cursor(
    cll_handle: u32,
    channel_id: ChannelId,
    connect_generation: u64,
    ctx: &ChannelPollCtx,
) -> bool {
    let mut links = ctx.logical_links.lock().await;
    let Some(link) = links
        .get_mut(&cll_handle)
        .filter(|l| l.channel_id == Some(channel_id) && l.connect_generation == connect_generation)
    else {
        return false;
    };
    link.j1939_claim_cursor += 1;
    true
}

/// ADR-179 Decision 3's "later, spontaneous `_LOST`" case: drains this
/// physical channel's `SharedChannel::j1939_reclaim_pending` set (armed by
/// [`deliver_j1939_claim_indication`]) and re-enters the claim retry loop
/// -- from `j1939_claim_cursor = 0`, already reset by that same caller --
/// for each CLL in it. Resolves claim inputs from the CLL's live `active`
/// ComParamSet (there is no call-time RPC snapshot to reuse for a
/// spontaneous, non-RPC-driven retry, unlike `handle_start_comm`'s initial
/// claim). Exhaustion surfaces a CLL-scoped error event (`cop_handle =
/// None` -- this is not driven by any live COP, mirroring `send_error_event`'s
/// own module-/CLL-scoped-error shape); success silently writes back
/// `CP_TesterSourceAddress`/`j1939_claimed_address`, mirroring
/// `handle_start_comm`'s own claim-success writeback. Called from
/// `run_due_tick_duties`, alongside this channel's other periodic per-tick
/// duties.
pub(super) async fn run_j1939_reclaim_duties(ctx: &ChannelPollCtx) {
    let pending: Vec<(u32, J1939ReclaimPending)> = {
        let mut chans = ctx.service.shared_channels.lock().await;
        let Some(sc) = chans
            .values_mut()
            .find(|sc| sc.channel_id == ctx.channel_id)
        else {
            return;
        };
        sc.j1939_reclaim_pending.drain().collect()
    };
    for (cll_handle, pending_entry) in pending {
        let resolved = {
            let links = ctx.logical_links.lock().await;
            links
                .get(&cll_handle)
                .filter(|l| {
                    l.channel_id == Some(ctx.channel_id)
                        && l.connect_generation == pending_entry.connect_generation
                })
                .map(|l| (l.connect_generation, resolve_j1939_claim_params(&l.active)))
        };
        let Some((connect_generation, mut params)) = resolved else {
            continue;
        };
        // ADR-180 Decision 23 round-26 correction: override the
        // freshly-resolved NAME with the one actually captured at loss
        // time (`pending_entry.name`) -- `l.active` may have since
        // diverged from (or never matched) the NAME this claim was really
        // issued under, e.g. a `temp_param_update = 1` `CoptStartcomm`
        // that claimed under a Working/Temp-bound NAME never promoted to
        // Active. Re-reading Active here would silently reclaim under the
        // wrong identity.
        params.name = pending_entry.name;
        match run_j1939_claim_loop(cll_handle, connect_generation, None, &params, ctx).await {
            J1939ClaimLoopOutcome::Claimed(addr) => {
                // ADR-180 Decision 13 (design-advisor consult, PR #72 round
                // 11): `shared_channels` held across this writeback and the
                // repeat-slot sweep below, mirroring `handle_start_comm`'s
                // own identical `Claimed` arm fix -- see that arm's doc
                // comment for the full TOCTOU rationale (serializes against
                // `ioctl_start_repeat_message`'s own `shared_channels` hold,
                // which already spans its entire check-native-call-register
                // sequence).
                let mut chans = ctx.service.shared_channels.lock().await;
                let claimed_here = {
                    let mut links = ctx.logical_links.lock().await;
                    if let Some(link) = links.get_mut(&cll_handle).filter(|l| {
                        l.channel_id == Some(ctx.channel_id)
                            && l.connect_generation == connect_generation
                    }) {
                        link.working
                            .unum32
                            .insert(ComParamId(j2534_0404::NODE_ADDRESS), addr as u32);
                        link.active
                            .unum32
                            .insert(ComParamId(j2534_0404::NODE_ADDRESS), addr as u32);
                        link.j1939_claimed_address = Some(addr);
                        // Codex review finding (PR #72 round 6): mirrors
                        // `handle_start_comm`'s own J1939 `Claimed` arm fix for
                        // `tester_present_data` -- a live `TesterPresentState::
                        // Armed`'s cached `framed_data` was composed against
                        // whatever source address was claimed BEFORE this
                        // spontaneous reclaim (the just-lost address, or the
                        // pre-claim default on this CLL's very first claim), and
                        // the NODE_ADDRESS writeback above never touches it.
                        // Recompose its 5-byte J1939 header against the
                        // freshly-claimed address the same way, in place,
                        // keeping the payload bytes after it unchanged --
                        // otherwise a CLL with tester-present enabled keeps
                        // transmitting keep-alives under the lost address after
                        // a successful reclaim. `fresh_header` is computed
                        // before the mutable borrow below so it does not
                        // overlap with `link.active`'s own immutable borrow.
                        let fresh_header = tx_header::j1939_header_bytes(addr, &link.active);
                        if let TesterPresentState::Armed { framed_data, .. } =
                            &mut link.tester_present_state
                            && framed_data.len() >= fresh_header.len()
                        {
                            let mut fresh = fresh_header.clone();
                            fresh.extend_from_slice(&framed_data[fresh_header.len()..]);
                            *framed_data = fresh;
                        }
                        true
                    } else {
                        false
                    }
                };
                if claimed_here {
                    // ADR-180 Decision 13: sweep any repeat slot a client
                    // raced into existence during this reclaim attempt's own
                    // pending window -- see this arm's own `shared_channels`
                    // doc comment above for the full serialization rationale.
                    let failed =
                        stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
                    push_leaked_repeat_slots(&mut chans, ctx.channel_id, failed);
                }
                drop(chans);
            }
            J1939ClaimLoopOutcome::Exhausted => {
                // `still_on_this_channel` guard (edge-case-hunter finding,
                // PR #72 round 4): mirrors `handle_start_comm`'s own J1939
                // `Exhausted` arm guard. `send_error_event` looks up
                // `cll_handle` in `logical_links` unconditionally -- if a
                // disconnect+reconnect (ADR-086: a handle can be reused for
                // a new `connect_generation`) landed while this reclaim
                // attempt was exhausting its candidate list, that lookup
                // would still find AN entry for this handle, just one that
                // is now a completely different logical connection, and
                // would wrongly write a spurious `last_error`/event into it.
                //
                // ADR-180 Decision 13 (design-advisor consult, PR #72 round
                // 11): `shared_channels` held across this snapshot and the
                // repeat-slot sweep just below, mirroring `handle_start_comm`'s
                // own identical `Exhausted` arm fix -- an exhausted reclaim
                // attempt leaves this CLL with no claimed address at all, so
                // any repeat slot a client raced into existence during this
                // attempt's own pending window needs the same cleanup.
                let mut chans = ctx.service.shared_channels.lock().await;
                let still_on_this_channel = {
                    let links = ctx.logical_links.lock().await;
                    links.get(&cll_handle).is_some_and(|l| {
                        l.channel_id == Some(ctx.channel_id)
                            && l.connect_generation == connect_generation
                    })
                };
                if still_on_this_channel {
                    let failed =
                        stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
                    push_leaked_repeat_slots(&mut chans, ctx.channel_id, failed);
                }
                drop(chans);
                if still_on_this_channel {
                    send_error_event(
                        &ctx.subscriptions,
                        &ctx.logical_links,
                        cll_handle,
                        PduErrorEvent::PduErrEvtInitError,
                        None,
                    )
                    .await;
                }
            }
            J1939ClaimLoopOutcome::StopCommPending => {
                // ADR-180 Decision 24 (round-27, Codex review, PR #72;
                // `design-advisor` consult; round-27 correction from the
                // same round's own edge-case-hunter pass): a pending
                // `CoptStopcomm` aborted this reclaim -- not a claim failure
                // (the client is intentionally ending the comm session,
                // which is exactly what this reclaim exists to make way
                // for), so unlike the `Exhausted` arm above, no
                // `send_error_event` is emitted. Mirrors the `Exhausted`
                // arm's own `still_on_this_channel`-guarded repeat-slot
                // sweep for consistency: this CLL's own pending window is
                // over either way, so any repeat slot a client raced into
                // existence during it needs the same cleanup `Exhausted`
                // already gives it.
                //
                // Unlike the original round-27 text, this is a PAUSE, not
                // an abandonment: `stop_comm_pending` is only provisional
                // until `handle_stop_comm` actually runs the StopComm --
                // `CancelComPrimitive` on a held StopComm,
                // `PDU_IOCTL_CLEAR_TX_QUEUE`/`RESET`, or a malformed-
                // `cop_data` rollback can all revert it to `false` without
                // the StopComm ever running, and none of those paths
                // re-arms this CLL's lost claim on their own. So re-insert
                // this CLL's `j1939_reclaim_pending` entry (drained at the
                // top of this function's own loop, at the very start of
                // this iteration) so a later `run_j1939_reclaim_duties`
                // call -- fired every `POLL_INTERVAL_MS` via
                // `run_due_tick_duties` -- retries it. `params.name` still
                // holds the loss-time NAME captured via the round-26
                // override above (`params.name = pending_entry.name`), so
                // the retry keeps defending the correct identity.
                //
                // Deliberately unconditional on `comm_started`:
                // `deliver_j1939_claim_indication`'s own spontaneous-loss
                // arming of this exact entry has no such gate either, and
                // ADR-180 Decision 4 already establishes that `CoptStopcomm`
                // does not relinquish a live claim -- gating the retry here
                // would only create a new, narrower abandonment window
                // (a loss landing just before a stop wouldn't retry, one
                // landing just after would) instead of closing this one.
                // `.entry(..).or_insert(..)` rather than a plain overwrite:
                // defensive against a differently-sourced arming of the
                // same CLL landing here first, even though no such source
                // is currently constructible (see this Decision's own ADR
                // text for why).
                let mut chans = ctx.service.shared_channels.lock().await;
                let still_on_this_channel = {
                    let links = ctx.logical_links.lock().await;
                    links.get(&cll_handle).is_some_and(|l| {
                        l.channel_id == Some(ctx.channel_id)
                            && l.connect_generation == connect_generation
                    })
                };
                if still_on_this_channel {
                    let failed =
                        stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
                    push_leaked_repeat_slots(&mut chans, ctx.channel_id, failed);
                    // `still_on_this_channel` above is reused here, after
                    // the `.await` on `stop_repeat_slots_for_cll` --
                    // `edge-case-hunter` finding (round-27 verification
                    // pass): no re-check runs immediately before this
                    // insert, unlike `deliver_j1939_claim_indication`'s own
                    // explicit recheck-before-write idiom elsewhere in this
                    // file. A stale insert here (this CLL having since
                    // disconnected/reconnected during the `.await`, bumping
                    // `connect_generation`) is not a live bug: it is
                    // silently absorbed downstream -- this function's own
                    // next drain matches on `connect_generation`
                    // (`resolved.filter(...)`, above in this same
                    // function), and `CoptUpdateparam`'s `live_defended_name`
                    // lookup (`rpc_primitive.rs`) filters on the identical
                    // generation -- so a stale-generation entry is simply
                    // never consumed by anything. Left unguarded rather than
                    // adding a second `logical_links` acquisition purely for
                    // symmetry with that idiom.
                    if let Some(sc) = chans
                        .values_mut()
                        .find(|sc| sc.channel_id == ctx.channel_id)
                    {
                        sc.j1939_reclaim_pending
                            .entry(cll_handle)
                            .or_insert(J1939ReclaimPending {
                                connect_generation,
                                name: params.name,
                            });
                    }
                }
                drop(chans);
            }
            J1939ClaimLoopOutcome::Stale | J1939ClaimLoopOutcome::HardError => {}
            J1939ClaimLoopOutcome::Cancelled => {
                unreachable!("cancel_cop is None for spontaneous reclaim")
            }
        }
    }
}

/// ADR-179 Decision 3 cancellation: on a J1939 CLL's disconnect while its
/// shared physical channel survives (this function is a no-op, doing
/// nothing useful but also nothing harmful, when it does not -- the entry
/// is about to be torn down by `PassThruDisconnect` regardless), issues
/// `api.cancel_j1939_addr_protect` for every address `cll_handle` owns in
/// this channel's `SharedChannel::j1939_claims`, and removes those entries
/// from the map -- for EVERY one of this function's 5 call sites (the 2
/// teardown sites in `rpc_link.rs`, Decision 4's "fresh attempt" reset,
/// Decision 9's `cancel_j1939_claim_after_failed_startcomm`, and the
/// Decision 18 opt-out branch), not just the 2 teardown ones.
///
/// ADR-180 Decision 22 (design-advisor consult, Codex review PR #72):
/// per-address cancel-THEN-remove, not remove-all-then-cancel-all --
/// `cll_handle`'s ownership of an address is relinquished from this
/// service's own routing map (`sc.j1939_claims`) UNCONDITIONALLY, regardless
/// of whether the native cancel for it succeeds (the owner is giving the
/// address up either way, and routing to a departing owner is dead weight),
/// but a FAILED native cancel additionally pushes that address into
/// `SharedChannel::leaked_j1939_claims` -- unlike a wasted-but-harmless
/// address slot, this is a real correctness hazard at every one of this
/// function's 5 call sites, including both teardown ones: both are gated on
/// the physical channel staying open (i.e. they run precisely when sibling
/// CLLs -- potential future claimants -- survive), and the Decision-4 "fresh
/// attempt" site can even race against THIS SAME CLL's own imminent
/// re-claim of the address it just gave up. Without tracking the failure,
/// the routing entry being gone would let a NEW claimant -- a sibling, or
/// this same CLL's own fresh attempt -- issue its own native claim for an
/// address the adapter may still be defending under the OLD claim: two
/// claimants, one address. See `leaked_j1939_claims`'s own field doc
/// (`service.rs`) for the full mirror-of-`leaked_repeat_message_ids`
/// mechanism and its opportunistic retry site
/// (`run_j1939_claim_loop`'s own leaked-set guard, `events_j1939_claim.rs`).
///
/// No signature change and no return-value change from this restructuring
/// -- every call site keeps compiling and behaving unchanged; the leaked
/// address is reconciled via the shared channel state, not by caller-side
/// handling.
///
/// `j1939_claim_results`/`j1939_reclaim_pending` removal for this
/// `cll_handle` stays unconditional and still happens exactly once for the
/// whole call, not per-address (unchanged from before this restructuring).
///
/// `api` is taken as an already-locked `&J2534Api0404` (mirroring how the
/// sibling `client_filters`/`repeat_message_ids` teardown loops at both
/// call sites already hold `self.api.lock().await` across their own
/// per-address native calls) -- this function must NOT lock it itself, or
/// it would deadlock against a caller that (like both real call sites)
/// already holds that same lock across this call.
///
/// `connect_generation` (edge-case-hunter finding, PR #72 round 16
/// verification pass): filters `SharedChannel::j1939_claims`' own stored
/// `(owner_cll_handle, connect_generation)` pair on BOTH fields, not just
/// `cll_handle` -- every call site now passes the CALLER's own live/
/// captured generation for this exact `cll_handle` (its own `link.
/// connect_generation` at teardown time, or the COP's call-time-captured
/// generation for the "fresh attempt"/"failed StartComm" call sites),
/// closing a real, if narrow, TOCTOU: without this, a disconnect+reconnect
/// completing on this same `cll_handle` (bumping its generation, ADR-086)
/// and registering a fresh claim, all inside the `.await` gap between a
/// STALE dispatch's earlier checks and this call, would wrongly let that
/// stale dispatch cancel the NEW generation's live claim instead of (or in
/// addition to) whatever the stale generation itself owned.
pub(in crate::service) async fn cancel_j1939_claims_for_cll(
    cll_handle: u32,
    connect_generation: u64,
    channel_id: ChannelId,
    api: &J2534Api0404,
    sc: &mut SharedChannel,
) {
    // round-26 `edge-case-hunter` finding: these two removes used to sit
    // AFTER the `owned.is_empty()` early return below, contradicting this
    // function's own doc comment ("removal ... stays unconditional"). A CLL
    // that suffered a spontaneous claim loss has its address already moved
    // OUT of `j1939_claims` and into `j1939_reclaim_pending` (`deliver_
    // j1939_claim_indication`'s `spontaneous_loss` arm) -- `owned` is
    // computed from `j1939_claims` alone, so it comes back empty for such a
    // CLL, and the early return below would leave its now-orphaned
    // `j1939_reclaim_pending` entry (and any stale `j1939_claim_results`
    // entry) behind on disconnect/destroy. Not a live correctness bug today
    // (`run_j1939_reclaim_duties`'s own `connect_generation` filter already
    // discards a leaked entry on its next drain, since teardown clears this
    // CLL's `channel_id`/bumps nothing an orphaned entry could be mistaken
    // for), but it is a real, avoidable leak this function's own doc comment
    // already claimed didn't happen -- moved above the early return so that
    // claim is actually true, matching the round-16/21/22 precedent of
    // closing exactly this class of "docs said unconditional, code wasn't"
    // gap the moment it's found rather than leaving it to accumulate.
    sc.j1939_claim_results.remove(&cll_handle);
    sc.j1939_reclaim_pending.remove(&cll_handle);
    let owned: Vec<u8> = sc
        .j1939_claims
        .iter()
        .filter(|&(_, entry)| {
            entry.cll_handle == cll_handle && entry.connect_generation == connect_generation
        })
        .map(|(&addr, _)| addr)
        .collect();
    if owned.is_empty() {
        return;
    }
    for addr in owned {
        // ADR-180 Decision 22: cancel BEFORE removing this address's own
        // routing entry -- the entry is removed unconditionally either way,
        // but attempting the native cancel first (rather than after, as this
        // function used to) means a failed cancel is observed and tracked
        // before the routing map stops reflecting `cll_handle`'s ownership.
        let cancel_result = api.cancel_j1939_addr_protect(channel_id, addr);
        sc.j1939_claims.remove(&addr);
        if let Err(err) = cancel_result {
            warn!(
                cll_handle,
                address = addr,
                %err,
                "PROTECT_J1939_ADDR cancel failed on CLL disconnect (best-effort only); \
                 tracking this address in SharedChannel::leaked_j1939_claims so a fresh claim \
                 attempt does not treat it as free while the adapter may still be defending it"
            );
            if !sc.leaked_j1939_claims.contains(&addr) {
                sc.leaked_j1939_claims.push(addr);
            }
        }
    }
}

/// ADR-180 Decision 22's round-23 correction, extracted (round-24
/// correction to Decision 18, `design-advisor` consult, Codex review PR
/// #72) so [`run_j1939_claim_loop`]'s own retry block and the negotiation
/// opt-out branch (`events.rs`) share one retry mechanism instead of two
/// copies that could drift. Retries a native cancel for every address in
/// `sc.leaked_j1939_claims`, removing each one that now succeeds. Returns
/// `true` iff the list is empty once the retry completes -- every leaked
/// claim on this physical channel has reconciled; `false` means at least
/// one address survives and the caller must fail its own operation closed
/// rather than proceed as if the channel were clean.
///
/// Callers must already hold `shared_channels` and pass in an already-
/// acquired, nested `api` guard (ADR-080: `shared_channels` outermost,
/// `api` nested underneath). `cancel_j1939_addr_protect` is synchronous --
/// this function performs no `.await`, so it introduces no new suspension
/// point beyond whatever the caller already crossed to acquire `api`.
///
/// `pub(in crate::service)`, not merely `pub(super)` (relative to this
/// module's parent, `events`) -- mirroring `cancel_j1939_claims_for_cll`'s
/// own wider visibility just above -- since ADR-180 Decision 24 (round-27
/// correction to Decision 22, Codex review, PR #72) adds a third call site
/// in `rpc_misc.rs::ioctl_start_repeat_message`, outside the `events`
/// module entirely.
pub(in crate::service) fn reconcile_leaked_j1939_claims(
    sc: &mut SharedChannel,
    api: &J2534Api0404,
    channel_id: ChannelId,
    cll_handle: u32,
) -> bool {
    sc.leaked_j1939_claims.retain(
        |&addr| match api.cancel_j1939_addr_protect(channel_id, addr) {
            Ok(()) => false,
            Err(err) => {
                warn!(
                    cll_handle,
                    address = addr,
                    %err,
                    "PROTECT_J1939_ADDR retry-cancel failed for an address in \
                     SharedChannel::leaked_j1939_claims -- the adapter may still be \
                     defending it under a prior, now-unrouted claim"
                );
                true
            }
        },
    );
    sc.leaked_j1939_claims.is_empty()
}

/// ADR-180 Decision 9 (Codex review, PR #72 round 8): cancel owner for a
/// claim-enabled `PDU_COPT_STARTCOMM` whose handler terminates on a path
/// that never reaches the COMM_STARTED write-back after `run_j1939_claim_
/// loop` already returned `Claimed` -- cancellation of the optional
/// CoptStartcomm message's P3 wait or receive phase, or a transmit
/// failure/cancellation. None of those early returns previously knew about
/// the claim `handle_start_comm`'s own J1939 branch had just succeeded, so
/// the adapter kept defending an address, and the routing entry kept
/// blocking sibling CLLs, for a StartComm the client never saw complete.
///
/// Callers gate this on their own local "this COP's claim loop just
/// returned `Claimed`" flag (never on live `j1939_claimed_address` state --
/// a stale StopComm/StartComm-cycle claim, or a claim this COP itself never
/// issued, has its own dedicated owner: Decisions 1/4/5 below). Mirrors
/// `handle_start_comm`'s own "fresh attempt" reset block (the round-4
/// fix): `shared_channels` first, nested `api` for the native cancel,
/// released before the `logical_links` acquisition that clears the local
/// marker -- never held simultaneously, same ADR-080-conformant order.
/// `cancel_j1939_claims_for_cll` is already a no-op when this CLL owns
/// nothing on the channel, so calling it here even when `handle_start_comm`
/// took a non-J1939 path is harmless -- but every real call site gates on
/// the caller's own claim-succeeded flag regardless, to keep the intent
/// explicit rather than relying on that no-op behavior.
pub(super) async fn cancel_j1939_claim_after_failed_startcomm(
    cll_handle: u32,
    connect_generation: u64,
    ctx: &ChannelPollCtx,
) {
    let mut chans = ctx.service.shared_channels.lock().await;
    if let Some(sc) = chans
        .values_mut()
        .find(|sc| sc.channel_id == ctx.channel_id)
    {
        let api = ctx.api.lock().await;
        cancel_j1939_claims_for_cll(cll_handle, connect_generation, ctx.channel_id, &api, sc).await;
    }
    drop(chans);

    let mut links = ctx.logical_links.lock().await;
    if let Some(link) = links.get_mut(&cll_handle).filter(|l| {
        l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
    }) {
        link.j1939_claimed_address = None;
    }
    drop(links);

    // ADR-180 Decision 10 (edge-case-hunter finding, round 9 verification
    // pass on this same round's own diff): unlike Decision 2's timeout case
    // (where the claim never completed, so no repeat slot can bear its
    // address), THIS cancellation's whole premise is a claim that DID
    // complete -- `j1939_claimed_this_cop` (this function's caller's own
    // gate) is only ever `true` once `run_j1939_claim_loop` returned
    // `Claimed`, and `PDU_IOCTL_START_REPEAT_MESSAGE` requires only a
    // connected channel, not `comm_started` -- so a client can legitimately
    // start a repeat slot under the freshly-claimed address while the
    // optional CoptStartcomm message's own transmit sequence is still in
    // flight. If that sequence then fails/cancels, this function relinquishes
    // the claim; the CLL's own live repeat slots must be stopped too, or
    // they keep transmitting under an address just handed back. Unconditional
    // and idempotent, same shape as this function's own `cancel_j1939_claims_
    // for_cll` call above.
    let failed = stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
    record_leaked_repeat_slots(ctx, failed).await;

    // ADR-180 Decision 11 (design-advisor consult, round 10): the sibling
    // call for CoptSendrecv COPs, same rationale -- included here for
    // completeness/consistency with this ADR's other two relinquishment
    // sites, though `comm_started` is never true yet at this call site
    // (J1939 CoptSendrecv requires it, `tx_header::build_tx_message`'s own
    // doc comment), so this particular call is always a no-op today.
    cancel_send_recv_cops_for_cll(cll_handle, connect_generation, ctx).await;
}

/// ADR-180 Decision 10 (design-advisor consult, Codex review PR #72 round
/// 9): a live SAE J2534-2 clause 14 Repeat Messaging slot's device-side
/// frozen TX bytes (ADR-165 Decision 3) carry the SAE J1939 source address
/// that was claimed when `START` ran. Once that address is relinquished --
/// a spontaneous `_LOST` (this file's own `deliver_j1939_claim_indication`),
/// or a fresh `StartComm`'s own claim reset (`events.rs::handle_start_comm`)
/// -- the device would otherwise keep transmitting every one of the CLL's
/// repeat slots under an address it (or a sibling node that has since
/// claimed it) no longer owns, indefinitely: SAE J1939-81 requires a node
/// to cease transmitting under an address it no longer holds, and the
/// violation begins at the loss itself, not once/if a reclaim eventually
/// succeeds (which can take up to `candidate_count * CP_J1939AddrClaimTimeout`,
/// or never, on `Exhausted`). Best-effort-stops every MsgId in
/// `cll_handle`'s own `LogicalLinkState::repeat_message_ids`, mirroring
/// `rpc_misc.rs::ioctl_stop_repeat_message`'s own pruning convention: a
/// MsgId is dropped from tracking on `Ok` (genuinely stopped) or
/// `ERR_INVALID_MSG_ID` (already self-completed or otherwise gone
/// device-side, ADR-165 Decision 6) -- any other error is logged and the
/// entry retained, the same "wasteful, not unsafe" best-effort treatment
/// `cancel_j1939_claims_for_cll` above already gives a failed native call.
///
/// No new client-visible signal: ADR-165's own established contract
/// (`repeat_message_ids` membership is a claim, not a fact; slots can
/// already vanish device-side with no notification) already covers this --
/// a client's next `QUERY`/`STOP` on the stopped MsgId observes exactly the
/// `ERR_INVALID_MSG_ID` outcome that contract already documents.
///
/// Deliberately NOT folded into `cancel_j1939_claims_for_cll` above: a
/// different native primitive and a different piece of bookkeeping
/// (`repeat_message_ids`, not `SharedChannel::j1939_claims`), and that
/// helper's own Decision 2/9 call sites must NOT stop slots -- no slot can
/// bear a never-completed claim's address (an accepted residual: a `START`
/// landing inside `handle_start_comm`'s own await windows, between a
/// failed claim and this cancellation, is not covered here, mirroring this
/// file's other documented race-window residuals).
///
/// Does not touch `SharedChannel` at all (`repeat_message_ids` lives on
/// `LogicalLinkState`) -- it acquires `logical_links` then `api` itself,
/// sequentially, never simultaneously. Unlike `cancel_j1939_claims_for_cll`
/// this function itself has no ADR-080 lock-ordering obligation of its
/// own, but as of ADR-180 Decision 13 (round-11 fix for a repeat-slot-
/// created-during-a-pending-claim race) some callers now hold
/// `shared_channels` across their own call to this function (both
/// `Claimed`/`Exhausted` arms in this file and in `events.rs::
/// handle_start_comm`, serializing against `ioctl_start_repeat_message`'s
/// own `shared_channels` hold) -- safe precisely because this function
/// never itself acquires `shared_channels`, so a caller holding it first
/// (outermost, per ADR-080) never conflicts.
///
/// ADR-180 Decision 20 (Codex review PR #72 round 20): a MsgId whose
/// native `STOP_REPEAT_MESSAGE` fails with anything other than
/// `ERR_INVALID_MSG_ID` is returned to the caller in the `Vec<u32>` below,
/// rather than silently retained-and-forgotten -- routing it into
/// `SharedChannel::leaked_repeat_message_ids` (via
/// [`push_leaked_repeat_slots`]/[`record_leaked_repeat_slots`] below) is
/// the caller's obligation, never done here. This preserves the exact
/// "never itself acquires `shared_channels`" invariant the previous
/// paragraph documents, which ADR-180 Decision 13's own held-caller safety
/// argument already depends on.
///
/// `connect_generation` (mirrors [`cancel_j1939_claims_for_cll`]'s own
/// round-16 fix, applied here for the same reason -- Codex review, PR #72):
/// `cll_handle`s are reused across a disconnect+reconnect (ADR-086 bumps
/// `connect_generation` on reconnect, not the handle itself), and several
/// callers release `ctx.service.shared_channels`'s guard before `.await`ing
/// this helper. Without this check, a disconnect+reconnect on the SAME
/// `cll_handle` racing into that gap -- with the new session starting its
/// own repeat slot before this helper's lookup runs -- would let the lookup
/// find the NEW `LogicalLinkState` (same handle, different generation) and
/// wrongly stop/relinquish the NEW session's live repeat slots, attributing
/// them to the OLD, already-gone session's cleanup. Both the initial lookup
/// AND the later re-acquisition that prunes `repeat_message_ids` re-check
/// `connect_generation` -- the same TOCTOU window can occur between the two,
/// not just before the first.
#[must_use]
pub(super) async fn stop_repeat_slots_for_cll(
    cll_handle: u32,
    connect_generation: u64,
    ctx: &ChannelPollCtx,
) -> Vec<u32> {
    let msg_ids: Vec<u32> = {
        let links = ctx.logical_links.lock().await;
        links
            .get(&cll_handle)
            .filter(|l| l.connect_generation == connect_generation)
            .map(|l| l.repeat_message_ids.clone())
            .unwrap_or_default()
    };
    if msg_ids.is_empty() {
        return Vec::new();
    }
    let api = ctx.api.lock().await;
    let mut stopped = Vec::new();
    let mut failed = Vec::new();
    for msg_id in msg_ids {
        let result = api.stop_repeat_message(ctx.channel_id, msg_id);
        let is_invalid_msg_id = matches!(
            &result,
            Err(j2534_0404::Error::ApiStatus { code, .. })
                if code.as_u32() == j2534_0404::ERR_INVALID_MSG_ID
        );
        match &result {
            Ok(()) => stopped.push(msg_id),
            Err(_) if is_invalid_msg_id => stopped.push(msg_id),
            Err(err) => {
                warn!(
                    cll_handle,
                    msg_id,
                    %err,
                    "STOP_REPEAT_MESSAGE failed while tearing down a relinquished J1939 \
                     address's live repeat slots (best-effort only)"
                );
                failed.push(msg_id);
            }
        }
    }
    drop(api);
    if !stopped.is_empty() || !failed.is_empty() {
        let mut links = ctx.logical_links.lock().await;
        if let Some(link) = links
            .get_mut(&cll_handle)
            .filter(|l| l.connect_generation == connect_generation)
        {
            link.repeat_message_ids
                .retain(|id| !stopped.contains(id) && !failed.contains(id));
        }
    }
    failed
}

/// ADR-180 Decision 20: routes [`stop_repeat_slots_for_cll`]'s returned
/// `failed` MsgIds into `SharedChannel::leaked_repeat_message_ids`, for a
/// caller that already holds `chans` (`ctx.service.shared_channels`'s own
/// guard) -- NEVER call this without already holding that guard; use
/// [`record_leaked_repeat_slots`] instead when it is not already held
/// (this repo's mutexes are not reentrant, so acquiring `shared_channels`
/// a second time here would deadlock).
///
/// No-op when `failed` is empty. Looks up the `SharedChannel` the same way
/// every other same-shaped lookup in this file/`events.rs` does (grep
/// `values_mut().find(|sc| sc.channel_id`) -- `None` means the channel was
/// concurrently torn down, in which case the native disconnect already
/// killed the device-side slot along with the channel, so nothing further
/// is done. When found, each id is pushed only if not already present:
/// a concurrent CLL teardown may have already pushed the same id via
/// `rpc_link.rs`'s own leak-on-teardown path, and a duplicate push would
/// double-track it.
pub(super) fn push_leaked_repeat_slots(
    chans: &mut HashMap<ChannelKey, SharedChannel>,
    channel_id: ChannelId,
    failed: Vec<u32>,
) {
    if failed.is_empty() {
        return;
    }
    let Some(sc) = chans.values_mut().find(|sc| sc.channel_id == channel_id) else {
        debug!(
            ?channel_id,
            ?failed,
            "channel torn down concurrently with a repeat-slot stop failure; \
             the native disconnect already killed the device-side slot"
        );
        return;
    };
    for id in failed {
        if !sc.leaked_repeat_message_ids.contains(&id) {
            sc.leaked_repeat_message_ids.push(id);
        }
    }
}

/// ADR-180 Decision 20: the async-acquiring counterpart of
/// [`push_leaked_repeat_slots`], for a caller that does NOT already hold
/// `ctx.service.shared_channels`. NEVER call this while already holding
/// that guard -- this repo's mutexes are not reentrant, so a nested
/// acquisition deadlocks; use [`push_leaked_repeat_slots`] directly with
/// the already-held guard instead.
///
/// Early-returns before acquiring the lock when `failed` is empty, so the
/// common (nothing leaked) path never pays for the acquisition.
pub(super) async fn record_leaked_repeat_slots(ctx: &ChannelPollCtx, failed: Vec<u32>) {
    if failed.is_empty() {
        return;
    }
    let mut chans = ctx.service.shared_channels.lock().await;
    push_leaked_repeat_slots(&mut chans, ctx.channel_id, failed);
}

/// ADR-180 Decision 11 (design-advisor consult, Codex review PR #72 round
/// 10): mirrors [`stop_repeat_slots_for_cll`]'s own rationale, for a
/// different mechanism -- `events.rs::handle_send_recv`'s own doc comment
/// establishes that every cycle of a `PDU_COPT_SENDRECV` COP (including a
/// multi-cycle/cyclic follow-up, re-enqueued via `CycleContinuation`)
/// retransmits the SAME `SendRecvTx::data` resolved once at
/// `StartComPrimitive` call time (ADR-067) -- for J1939, already framed
/// with the 5-byte `j1939_header_bytes` prefix, including the source
/// address that was live at that call-time moment. Nothing ever recomposes
/// it, so a claim relinquishment mid-COP (spontaneous loss, or a fresh
/// StartComm's own claim reset) leaves a still-running multi-cycle/cyclic
/// SendRecv transmitting under a lost or reassigned address indefinitely.
///
/// Unlike Decisions 3/6 (which recompose the claim machinery's OWN cached
/// frames in place), recomposing a client-owned `CoptSendrecv`'s `data`
/// here would not actually fix the problem: on a spontaneous loss, Active
/// `NODE_ADDRESS` itself still holds the LOST address until a reclaim
/// completes (Decision 5 clears only the local `j1939_claimed_address`/
/// cursor markers, and Decision 9's own accepted residual documents the
/// write-back is never reverted) -- so "recompose from live Active each
/// cycle" would keep reproducing the same stale byte for the whole
/// loss-to-reclaim window. It would also silently override a Temp-bound
/// COP's `effective` snapshot for every other header ComParam
/// (priority/DP/PF/PS), a genuine ADR-067 contract break for client-bound
/// values. So this mirrors Decision 10's own resolution instead: cancel,
/// don't recompose.
///
/// Cancels every live, *transmitting* `PDU_COPT_SENDRECV` COP this CLL owns
/// by inserting its `cop_handle` into `LogicalLinkState::cancelled_cops`
/// (`CopEntry::is_send_recv && CopEntry::transmits`, the same mark-and-defer
/// mechanism `rpc_primitive::rpc_cancel_com_primitive` uses for an explicit
/// client cancel) -- already handled correctly at every point a `TxItem::
/// SendRecv` can be: still queued (`should_skip_cancelled_item`), a parked
/// continuation, or the in-flight cycle's own drain check. `transmits`
/// excludes a receive-only (`NumSendCycles == 0`, ADR-059) monitor, which
/// never puts a frame on the bus and has nothing to invalidate;
/// `is_send_recv` excludes `CoptStartcomm` (would cancel the very reclaim
/// attempt driving this call) and a terminal-send `CoptStopcomm` (would
/// recreate ADR-085's `stop_comm_pending` deadlock) -- both also set
/// `transmits`.
///
/// `CopEntry::transmits`/`is_send_recv` are computed once, statically, at
/// `StartComPrimitive` call time and never updated afterward -- so an
/// IS-CYCLIC `CoptSendrecv` that has since migrated to ADR-100 tier-2
/// (`ReceivePhaseOutcome::DetachedToTier2`: its send phase already
/// finished, `NumReceiveCycles == -1`, now a pure receive-only registrant)
/// still reads `transmits == true`/`is_send_recv == true` forever, even
/// though it is no longer putting anything on the bus and so has nothing an
/// address change could make stale. Excluded here the same way
/// `ioctl_clear_tx_queue`'s own ADR-100 S5 companion fix excludes it from
/// "the TX queue": any `cop_handle` with a live tier-2
/// (`RegistrantTier::ReceiveOnly`) registrant on this CLL is filtered out
/// before extending `cancelled_cops`, so a healthy detached listener is
/// never terminated by an unrelated claim relinquishment (round-10
/// edge-case-hunter finding, ADR-180 Decision 11 amendment).
///
/// Sequential lock order (`primitives` then `logical_links`), mirroring
/// `rpc_cancel_com_primitive`'s own shape.
///
/// `connect_generation` (Codex review, PR #72): the final `logical_links`
/// lookup is scoped to it, the same "reused-handle-across-reconnect" race
/// class the round-16/round-21 corrections already fixed for
/// `cancel_j1939_claims_for_cll`/`stop_repeat_slots_for_cll` -- without it,
/// a disconnect+reconnect on the SAME `cll_handle` completing inside this
/// function's own `.await` gap (its caller already released `shared_
/// channels` before awaiting it, at every call site) would wrongly extend
/// the NEW generation's `cancelled_cops` in response to the OLD
/// generation's relinquishment. `cop_handles` themselves need no such
/// scoping -- `CopEntry` carries no `connect_generation` field, but a COP
/// entry's own lifecycle is already tied to the CLL generation that created
/// it (a reconnect starts a fresh COP handle space), so a stale entry
/// simply can't exist here; only the write into the (handle-keyed, reused)
/// `LogicalLinkState` needed the guard.
pub(super) async fn cancel_send_recv_cops_for_cll(
    cll_handle: u32,
    connect_generation: u64,
    ctx: &ChannelPollCtx,
) {
    let cop_handles: Vec<u32> = {
        let prims = ctx.primitives.lock().await;
        prims
            .iter()
            .filter(|(_, entry)| {
                entry.cll_handle == cll_handle && entry.is_send_recv && entry.transmits
            })
            .map(|(&cop_handle, _)| cop_handle)
            .collect()
    };
    if cop_handles.is_empty() {
        return;
    }
    let mut links = ctx.logical_links.lock().await;
    if let Some(link) = links
        .get_mut(&cll_handle)
        .filter(|l| l.connect_generation == connect_generation)
    {
        let detached_tier2: std::collections::HashSet<u32> = link
            .registrants
            .iter()
            .filter(|r| r.tier == RegistrantTier::ReceiveOnly)
            .map(|r| r.cop_handle)
            .collect();
        // ADR-192/Phase 7 Stage 7c edge-case-hunter fix (defense in depth --
        // J1939 and TP2.0 are mutually exclusive protocols per-CLL, so this
        // is practically unreachable here, but matches the same exclusion
        // added at the other two `cancelled_cops` sweep sites).
        let broadcast_periodic_cop = link.tp20_broadcast_periodic.map(|p| p.cop_handle);
        let cops_to_cancel: Vec<u32> = cop_handles
            .into_iter()
            .filter(|h| !detached_tier2.contains(h) && Some(*h) != broadcast_periodic_cop)
            .collect();
        link.cancelled_cops.extend(cops_to_cancel.iter().copied());
        drop(links);
        // ADR-182 follow-up fix (Codex review round, PR #78; one of the
        // sibling sites of the same fix, added in a later PR #78
        // edge-case-hunter follow-up): same read-then-mark race as
        // `rpc_cancel_com_primitive` / `CoptStopcomm`'s cancel-all block /
        // `PDU_IOCTL_CLEAR_TX_QUEUE`'s handler -- `reap_expired_cyclic_
        // registrants`'s own former late drain was folded into
        // `emit_terminal_if_live` itself by the PR #78 edge-case-hunter
        // follow-up and is no longer a separate sibling site (see `drain_cancelled_cop_if_
        // finalized`'s own doc comment for the current call-site list) --
        // this function's own doc comment above already establishes that
        // `cop_handles` are the same IS-CYCLIC/tier-2-eligible COPs the
        // maintenance reap operates on, so if the reap fully finishes one of
        // them (removing it from `primitives` and its registrant) in the gap
        // between the read above and the `cancelled_cops.extend` just above,
        // that mark would otherwise be stranded permanently. Drain each one
        // through the same shared helper; see `drain_cancelled_cop_if_
        // finalized`'s own doc comment for the full sibling-site list.
        for cop in cops_to_cancel {
            drain_cancelled_cop_if_finalized(&ctx.primitives, &ctx.logical_links, cll_handle, cop)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `LogicalLinkState` with every field at a neutral default, for
    /// testing pure predicates that only care about a handful of fields --
    /// mirrors `events_combined_timing_change_tests::minimal_link`'s role
    /// for that module (and `events_registrant_lifecycle_tests`'s own).
    fn minimal_link() -> LogicalLinkState {
        LogicalLinkState {
            channel_id: None,
            protocol: ChannelProtocol::J1939_PS,
            hw_protocol_id: j2534_0404::PROTOCOL_J1939_PS,
            software_isotp: false,
            uudt_channel_id: None,
            uudt_channel_key: None,
            isotp_rx: Arc::new(Mutex::new(HashMap::new())),
            connect_in_flight: std::sync::Weak::new(),
            connected: false,
            comm_started: false,
            raw_mode: false,
            checksum_mode: false,
            connect_generation: 0,
            stop_comm_pending: false,
            channel_key: None,
            pin_select: None,
            channel_index: None,
            base_hw_protocol_override: None,
            rx_buf: Arc::new(Mutex::new(CllEventQueue {
                event_queue_cap: 16,
                ..CllEventQueue::default()
            })),
            working: ComParamSet::default(),
            active: ComParamSet::default(),
            tester_present_state: TesterPresentState::None,
            tester_present_base_tx_flags: 0,
            open_tp_discards: Vec::new(),
            working_unique_resp_id_table: Vec::new(),
            active_unique_resp_id_table: Vec::new(),
            unique_resp_filter_ids: Vec::new(),
            cancelled_cops: std::collections::HashSet::new(),
            held_lock_mask: 0,
            last_error: None,
            tx_held: VecDeque::new(),
            tx_suspended_by_ioctl: false,
            tx_suspended_by_lock: false,
            tx_suspended_by_error: false,
            error_clear_seq: 0,
            error_set_seq: 0,
            client_filters: HashMap::new(),
            repeat_message_ids: Vec::new(),
            pending_client_filters: HashMap::new(),
            registrants: Vec::new(),
            next_registrant_seq: 0,
            j1939_claimed_address: None,
            j1939_claim_cursor: 0,
            j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
            tp20_connection: None,
            tp20_broadcast_periodic: None,
        }
    }

    /// ADR-180 Decisions 14/15/16 (design-advisor consult, PR #72 round
    /// 12): the shared `j1939_negotiated_unclaimed` predicate is `true`
    /// only for a negotiation-enabled J1939 CLL with no CURRENTLY claimed
    /// address -- both before a first successful claim and during a
    /// spontaneous-loss-to-reclaim window.
    #[test]
    fn negotiated_unclaimed_true_before_any_claim() {
        let link = minimal_link();
        assert!(j1939_negotiated_unclaimed(&link));
    }

    #[test]
    fn negotiated_unclaimed_false_once_an_address_is_claimed() {
        let mut link = minimal_link();
        link.j1939_claimed_address = Some(0x80);
        assert!(!j1939_negotiated_unclaimed(&link));
    }

    #[test]
    fn negotiated_unclaimed_false_when_negotiation_is_not_requested() {
        // CP_J1939AddressNegotiationRule bit 1 set: negotiation NOT
        // requested -- this CLL never runs the claim loop at all, and
        // `NODE_ADDRESS` is client-owned (ADR-180 Decision 12's own
        // precedent) -- must never be gated by this predicate regardless of
        // `j1939_claimed_address`.
        let mut link = minimal_link();
        link.active.unum32.insert(PARAM_J1939_ADDR_NEG_RULE, 0b10);
        assert!(!j1939_negotiated_unclaimed(&link));
    }

    #[test]
    fn negotiated_unclaimed_false_for_a_non_j1939_protocol() {
        let mut link = minimal_link();
        link.protocol = ChannelProtocol::ISO14230;
        link.hw_protocol_id = j2534_0404::ISO14230;
        assert!(!j1939_negotiated_unclaimed(&link));
    }

    /// ADR-180 Decision 18 (design-advisor consult, PR #72 round 15): the
    /// widened predicate's `Engaged` arm -- a CLL whose live
    /// `j1939_negotiation_posture` is `Engaged` (this CLL genuinely
    /// requested negotiation at its own `CoptStartcomm`) but whose SNAPSHOT
    /// says non-negotiated (a Working-only-staged
    /// `CP_J1939AddressNegotiationRule` opt-out this round's finding
    /// exploited) must still be treated as negotiation-managed and
    /// unclaimed -- this is the exact spoof case the widened predicate
    /// closes. Round-15/16 regression tripwire -- must keep passing
    /// unchanged.
    #[test]
    fn negotiated_unclaimed_true_when_engaged_even_if_snapshot_says_non_negotiated() {
        let mut link = minimal_link();
        link.j1939_negotiation_posture = J1939NegotiationPosture::Engaged;
        // Snapshot (Active, since `j1939_negotiated_unclaimed` always reads
        // `link.active`) says negotiation NOT requested -- exactly what a
        // spoofed Working-only opt-out would look like via
        // `j1939_negotiated_unclaimed_for`'s explicit-`params` variant.
        link.active.unum32.insert(PARAM_J1939_ADDR_NEG_RULE, 0b10);
        assert!(j1939_negotiated_unclaimed(&link));
    }

    /// The `Engaged` arm does not bypass the "currently unclaimed" conjunct
    /// -- an engaged CLL that HAS successfully claimed an address is still
    /// not gated by this predicate, same as the pre-existing
    /// `negotiated_unclaimed_false_once_an_address_is_claimed` case.
    #[test]
    fn negotiated_unclaimed_false_when_engaged_but_already_claimed() {
        let mut link = minimal_link();
        link.j1939_negotiation_posture = J1939NegotiationPosture::Engaged;
        link.j1939_claimed_address = Some(0x80);
        assert!(!j1939_negotiated_unclaimed(&link));
    }

    /// Correction to ADR-180 Decision 18 (design-advisor consult, PR #72
    /// review round): the mirror-image gap the two-state `bool` posture
    /// missed. A CLL whose live `j1939_negotiation_posture` is `OptedOut`
    /// (a real `CoptStartcomm` explicitly requested NO negotiation) but
    /// whose SNAPSHOT still carries a stale "negotiation requested" value
    /// (Active never got updated off its negotiation-ENABLED default) must
    /// NOT be treated as negotiation-managed and unclaimed -- the real,
    /// structural opt-out wins over the stale snapshot. Pre-fix (two-state
    /// `bool`), this case was indistinguishable from `Undecided` and fell
    /// back to the stale snapshot, wrongly blocking every later ordinary
    /// call on this CLL forever. Regression tripwire for that bug.
    #[test]
    fn negotiated_unclaimed_false_when_opted_out_even_if_snapshot_says_negotiated() {
        let mut link = minimal_link();
        link.j1939_negotiation_posture = J1939NegotiationPosture::OptedOut;
        // Snapshot (Active) still says negotiation requested (bit 1 clear /
        // `Some(0)`) -- the stale default a `temp_param_update=1` opt-out
        // StartComm would leave behind without promoting it to Active.
        link.active.unum32.insert(PARAM_J1939_ADDR_NEG_RULE, 0);
        assert!(link.j1939_claimed_address.is_none());
        assert!(!j1939_negotiated_unclaimed(&link));
    }

    #[test]
    fn claim_requested_bit1_clear_means_requested_and_matches_iso_default() {
        // ISO 22900-2:2022's own default (SAE_J1939 = 0) already means
        // "claim requested" -- confirms the polarity is not inverted.
        assert!(j1939_claim_requested(Some(0)));
        assert!(j1939_claim_requested(None));
    }

    #[test]
    fn claim_requested_bit1_set_means_not_requested() {
        assert!(!j1939_claim_requested(Some(0b010)));
        // Combined with other bits set -- only bit 1 governs this check.
        assert!(!j1939_claim_requested(Some(0b111)));
        assert!(j1939_claim_requested(Some(0b101)));
    }

    #[test]
    fn resolve_claim_params_defaults_when_absent() {
        let params = resolve_j1939_claim_params(&ComParamSet::default());
        assert!(params.candidates.is_empty());
        assert_eq!(params.name, [0u8; 8]);
        assert_eq!(params.timeout, Duration::from_micros(1_250_000));
    }

    #[test]
    fn resolve_claim_params_reads_candidates_name_and_timeout() {
        let mut p = ComParamSet::default();
        p.bytes
            .insert(PARAM_J1939_PREFERRED_ADDRESS, vec![0x80, 0x81, 0x82]);
        p.bytes
            .insert(PARAM_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        p.unum32.insert(PARAM_J1939_ADDR_CLAIM_TIMEOUT, 500_000);
        let params = resolve_j1939_claim_params(&p);
        assert_eq!(params.candidates, vec![0x80, 0x81, 0x82]);
        assert_eq!(params.name, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(params.timeout, Duration::from_micros(500_000));
    }

    #[test]
    fn resolve_claim_params_pads_short_name_and_truncates_long_one() {
        let mut short = ComParamSet::default();
        short.bytes.insert(PARAM_J1939_NAME, vec![0xAA, 0xBB]);
        let resolved_short = resolve_j1939_claim_params(&short);
        assert_eq!(
            resolved_short.name,
            [0xAA, 0xBB, 0, 0, 0, 0, 0, 0],
            "short NAME zero-pads on the right"
        );

        let mut long = ComParamSet::default();
        long.bytes
            .insert(PARAM_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let resolved_long = resolve_j1939_claim_params(&long);
        assert_eq!(
            resolved_long.name,
            [1, 2, 3, 4, 5, 6, 7, 8],
            "over-length NAME is truncated to 8 bytes"
        );
    }

    // ── retry-list cursor advance ─────────────────────────────────────────

    #[test]
    fn cursor_step_attempts_each_candidate_in_order() {
        let candidates = [0x10u8, 0x20, 0x30];
        assert_eq!(
            j1939_claim_cursor_step(&candidates, 0),
            J1939ClaimCursorStep::Attempt(0x10)
        );
        assert_eq!(
            j1939_claim_cursor_step(&candidates, 1),
            J1939ClaimCursorStep::Attempt(0x20)
        );
        assert_eq!(
            j1939_claim_cursor_step(&candidates, 2),
            J1939ClaimCursorStep::Attempt(0x30)
        );
    }

    #[test]
    fn cursor_step_exhausted_past_the_last_candidate() {
        let candidates = [0x10u8, 0x20];
        assert_eq!(
            j1939_claim_cursor_step(&candidates, 2),
            J1939ClaimCursorStep::Exhausted
        );
        assert_eq!(
            j1939_claim_cursor_step(&candidates, 100),
            J1939ClaimCursorStep::Exhausted
        );
    }

    #[test]
    fn cursor_step_exhausted_immediately_for_an_empty_list() {
        assert_eq!(
            j1939_claim_cursor_step(&[], 0),
            J1939ClaimCursorStep::Exhausted
        );
    }

    // ── generation-staleness drop ────────────────────────────────────────

    fn test_entry(cll_handle: u32, connect_generation: u64) -> J1939ClaimEntry {
        J1939ClaimEntry {
            cll_handle,
            connect_generation,
            name: [1, 2, 3, 4, 5, 6, 7, 8],
        }
    }

    #[test]
    fn claim_indication_routes_to_live_cll() {
        let mut claims = HashMap::new();
        claims.insert(0x80u8, test_entry(42, 7));
        let resolved =
            resolve_j1939_claim_indication(0x80, &claims, |h| if h == 42 { Some(7) } else { None });
        assert_eq!(resolved, Some(42));
    }

    #[test]
    fn claim_indication_drops_when_generation_is_stale() {
        let mut claims = HashMap::new();
        // Recorded generation (7) no longer matches the CLL's CURRENT live
        // generation (8) -- a disconnect/reconnect landed since the claim
        // attempt was registered.
        claims.insert(0x80u8, test_entry(42, 7));
        let resolved =
            resolve_j1939_claim_indication(0x80, &claims, |h| if h == 42 { Some(8) } else { None });
        assert_eq!(resolved, None);
    }

    #[test]
    fn claim_indication_drops_when_cll_no_longer_exists() {
        let mut claims = HashMap::new();
        claims.insert(0x80u8, test_entry(42, 7));
        let resolved = resolve_j1939_claim_indication(0x80, &claims, |_| None);
        assert_eq!(resolved, None);
    }

    #[test]
    fn claim_indication_drops_when_address_is_unrouted() {
        let claims: HashMap<u8, J1939ClaimEntry> = HashMap::new();
        let resolved = resolve_j1939_claim_indication(0x80, &claims, |_| Some(1));
        assert_eq!(resolved, None);
    }

    // ── cancel_send_recv_cops_for_cll's drain-loop (fifth
    // `drain_cancelled_cop_if_finalized` sibling site, PR #78 edge-case-
    // hunter follow-up, ADR-182) ────────────────────────────────────────

    const CSRC_TEST_CLL: u32 = 1;
    const CSRC_SEND_RECV_COP: u32 = 501;
    const CSRC_CONNECT_GENERATION: u64 = 3;

    /// Builds a full `ChannelPollCtx` (a real mock-library-backed
    /// `J2534Api0404`, a cheap `J2534Service` clone, and fresh per-channel
    /// gap-timing state) around one `primitives`/`logical_links` pair --
    /// mirrors `rpc_primitive.rs::copt_stopcomm_cancel_all_cancelled_cops_
    /// tests::service_with_a_started_link`'s construction shape. No device
    /// open or channel connect is needed: `cancel_send_recv_cops_for_cll`
    /// only ever reads `ctx.primitives`/`ctx.logical_links`, never
    /// `ctx.api`/`ctx.service`, so both are populated with inert values.
    async fn ctx_with_a_send_recv_cop() -> ChannelPollCtx {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");

        let mut link = minimal_link();
        link.connect_generation = CSRC_CONNECT_GENERATION;
        let mut logical_links = HashMap::new();
        logical_links.insert(CSRC_TEST_CLL, link);
        let logical_links = Arc::new(Mutex::new(logical_links));

        let mut primitives = HashMap::new();
        primitives.insert(
            CSRC_SEND_RECV_COP,
            CopEntry {
                cll_handle: CSRC_TEST_CLL,
                dispatched: false,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );
        let primitives = Arc::new(Mutex::new(primitives));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);

        let service = J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::default(),
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(vec![crate::config::ModuleEntry {
                label: "j2534-0404".to_string(),
                pname: None,
            }]),
            device_id: Arc::new(Mutex::new(None)),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            logical_links: Arc::clone(&logical_links),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(HashMap::new())),
            primitives: Arc::clone(&primitives),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(0)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            shutdown: shutdown_rx,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        };

        ChannelPollCtx {
            channel_id: ChannelId(0),
            primitives,
            api: Arc::clone(&service.api),
            logical_links,
            drain_watermarks: Arc::clone(&service.drain_watermarks),
            subscriptions: Arc::clone(&service.subscriptions),
            module_state: Arc::clone(&service.module_state),
            module_event_buf: Arc::clone(&service.module_event_buf),
            system_event_buf: Arc::clone(&service.system_event_buf),
            executing_cop: Arc::new(Mutex::new(None)),
            last_func_tx: Arc::new(Mutex::new(None)),
            last_phys_tx: Arc::new(Mutex::new(None)),
            last_bus_activity: Arc::new(Mutex::new(tokio::time::Instant::now())),
            rx_supported: true,
            service,
        }
    }

    /// Steady-state no-op: a still-live send-recv cop stays in `primitives`
    /// throughout the call, so the new drain loop's per-cop
    /// `drain_cancelled_cop_if_finalized` check sees it still present and
    /// leaves its freshly-inserted `cancelled_cops` mark alone -- mirrors
    /// `rpc_primitive.rs::copt_stopcomm_cancel_all_cancelled_cops_tests::
    /// leaves_a_still_live_queued_cops_mark_in_place` and `rpc_misc.rs::
    /// ioctl_clear_tx_queue_cancelled_cops_tests`'s own equivalent case for
    /// the other two batch-cancel sites.
    ///
    /// The genuine race this drain loop guards against -- the maintenance
    /// reap (`reap_expired_cyclic_registrants`) fully finishing this cop,
    /// including its `primitives` removal, in the gap between this
    /// function's own `primitives` read and its `cancelled_cops.extend` --
    /// is understood to be structurally infeasible to reproduce
    /// deterministically here too, same as the other four sites: this
    /// function only ever runs against a full `ChannelPollCtx` from inside
    /// the poll-task pipeline, with no test-only pause hook to land the reap
    /// inside that narrow window. See `events_drain_cancelled_cop_if_
    /// finalized_tests.rs`'s `batch_drain_loop_drains_only_the_stranded_
    /// marks_in_a_mixed_batch` for the loop-shape-level coverage of the
    /// genuine race, and `drain_cancelled_cop_if_finalized`'s own doc
    /// comment (`events_event_senders.rs`) for the full sibling-site list.
    #[tokio::test]
    async fn leaves_a_still_live_send_recv_cops_mark_in_place() {
        let ctx = ctx_with_a_send_recv_cop().await;

        cancel_send_recv_cops_for_cll(CSRC_TEST_CLL, CSRC_CONNECT_GENERATION, &ctx).await;

        assert!(
            ctx.primitives
                .lock()
                .await
                .contains_key(&CSRC_SEND_RECV_COP),
            "cancel_send_recv_cops_for_cll must not remove a queued cop from primitives"
        );
        let links = ctx.logical_links.lock().await;
        assert!(
            links
                .get(&CSRC_TEST_CLL)
                .unwrap()
                .cancelled_cops
                .contains(&CSRC_SEND_RECV_COP),
            "a still-live send-recv cop must be marked cancelled and its mark left in place"
        );
    }
}
