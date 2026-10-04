//! SAE J2534-2 clause 19 TP2.0 connection-request state machine (ADR-188,
//! Phase 7 Stage 7a; extended by ADR-190, Phase 7 Stage 7b). This module
//! owns:
//!
//! - Resolving a CLL's staged connection-request inputs (the five minted
//!   `PARAM_TP20_*` ComParams) from a `ComParamSet` snapshot
//!   ([`resolve_tp20_connection_params`]).
//! - Issuing `IOCTL_REQUEST_CONNECTION` and driving the bounded, non-
//!   blocking wait for the matching `CONNECTION_ESTABLISHED`/`_LOST`
//!   indication ([`run_tp20_connection_request`]), driven by
//!   `events.rs::handle_start_comm`.
//! - Routing a `CONNECTION_ESTABLISHED`/`_LOST` indication (consumed by
//!   `poll_rx_inner`'s RxStatus-withhold arm) back to its owning CLL via
//!   `SharedChannel::tp20_connections`, dropping a stale one whose recorded
//!   `connect_generation` no longer matches
//!   ([`resolve_tp20_connection_indication`],
//!   [`deliver_tp20_connection_indication`]) -- the structural sibling of
//!   `events_j1939_claim.rs`'s own claim-indication routing, reused here
//!   directly rather than re-derived.
//! - Best-effort `IOCTL_TEARDOWN_CONNECTION` on `PDU_COPT_STOPCOMM` and CLL
//!   disconnect/destroy: call sites in `events.rs`/`rpc_link.rs` invoke
//!   `j2534_0404::J2534Api0404::tp20_teardown_connection` directly (no
//!   wrapper here needed), mirroring how `events_j1939_claim.rs`'s own
//!   teardown call sites invoke `cancel_j1939_addr_protect` directly.
//! - ADR-190/Phase 7 Stage 7b's own passive-listener arm/disarm lifecycle
//!   (the interface's single inbound-accepting connection slot):
//!   [`resolve_tp20_passive_params`], [`arm_tp20_passive_listener`],
//!   [`best_effort_disarm_tp20_passive_native_config`]/
//!   [`quarantine_tp20_passive_slot_on_disarm`] (the two disarm building
//!   blocks -- not one self-locking helper, since the three disarm call
//!   sites' own lock topology differs too much for that; see either
//!   function's own doc comment),
//!   [`deliver_tp20_connection_indication`]'s own passive delivery arm, and
//!   (ADR-190's "Correction" paragraph under `### 4. Disarm / teardown`,
//!   Codex review finding, P1, PR #99; design-advisor consult) the ADR-101
//!   Decision §E-style bounded quarantine release
//!   ([`release_idle_passive_slots`]/[`tp20_passive_idle_release_due`]) for
//!   a disarm that never reached `Established` and therefore never issues a
//!   native call whose own indication would otherwise release the
//!   quarantine.
//!
//! Deliberately simpler than `events_j1939_claim.rs` in several ways not
//! revisited by ADR-188 this stage: no NAME-collision concept -- but RX-ID
//! uniqueness against a live sibling CLL's own still-PENDING request on the
//! same physical channel IS locally enforced here
//! ([`run_tp20_connection_request`]'s pre-insert ownership check), the same
//! `owned_by_a_live_sibling` shape `events_j1939_claim.rs`'s claim loop uses
//! -- clause 19.3.3.2's native `ERR_NOT_UNIQUE` only fires against an
//! already-ESTABLISHED channel, not a still-pending one, so it does not
//! itself cover the case this local check closes. Unlike `j1939_claims`
//! (which retains a successful claim's entry for the CLL's entire session,
//! so a later sibling naturally collides against it), `tp20_connections`
//! removes an attempt's own entry as soon as it resolves either way
//! (established or lost) -- so this check can only ever observe a
//! genuinely still-in-flight sibling attempt, never an already-established
//! one. Because this codebase gives each physical channel exactly one poll
//! task (`events::spawn_channel_poll_task`) that processes every queued
//! `TxItem::StartComm` -- including this entire function's own bounded
//! wait -- to completion before dequeuing the next one, two sibling CLLs on
//! one physical channel can never have overlapping "both still pending"
//! windows via the ordinary client-driven RPC path today; this check is
//! therefore defense-in-depth against that invariant (not currently
//! reachable via this crate's own request path, the same status
//! `events_j1939_claim.rs`'s own cleanup-gate check documents for its
//! analogous race) rather than a currently-observed live bug, and becomes
//! load-bearing if a future refactor ever decouples per-CLL `StartComm`
//! processing from that single serialized task. See
//! `tests/grpc_mock/tp20.rs`'s own regression test doc comment and
//! the Prioritized Backlog
//! for the resulting test-coverage limits. No PROACTIVE monitoring for a
//! LATER spontaneous connection loss once established (ADR-188 Decision
//! item 2 only covers the initial `CoptStartcomm`-driven exchange -- ADR-188
//! names no analogue of J1939's own reclaim duty; this module never polls
//! for one) -- narrowed, round 22 (Codex review finding, P1, PR #97;
//! design-advisor consult), from "no monitoring at all" to "passive
//! reconciliation only": [`deliver_tp20_connection_indication`]'s own
//! no-match fall-through now recognizes an unmatched `Lost` indication as a
//! possible spontaneous post-establishment loss and reconciles the owning
//! CLL's own phase to `Lost` when it finds one
//! ([`reconcile_established_tp20_loss`]) -- closing the permanent-lock-out
//! this residual otherwise produced once round 21 made every quarantine
//! insert unconditional (see that function's own doc comment for the full
//! account). And no `CancelComPrimitive`/`PDU_COPT_STOPCOMM`-race
//! cancellation of an in-flight wait (a documented residual, see
//! the Prioritized Backlog).
//!
//! **Abandoned-RX-ID quarantine (Codex review finding, PR #97, 7th round):**
//! [`run_tp20_connection_request`]'s own two LOCAL-abandonment outcomes
//! (mid-wait staleness, local-deadline timeout) both issue the native
//! `IOCTL_REQUEST_CONNECTION` successfully before giving up -- the device may
//! still deliver a `CONNECTION_ESTABLISHED`/`_LOST` indication for `rx_id`
//! after this side has walked away. Removing the routing entry immediately
//! (the pre-fix behavior) let an immediate retry proposing the same `rx_id`
//! register a brand-new entry indistinguishable from the abandoned one, so
//! [`deliver_tp20_connection_indication`] could misattribute the stale
//! indication to the retry -- falsely establishing it with an obsolete TX-ID,
//! or failing it while its real native connection remains allocated. Fixed
//! by marking the entry `abandoned` (`Tp20ConnEntry::abandoned`, see its own
//! doc comment) and keeping it registered instead of removing it:
//! [`tp20_rx_id_unavailable_for`] (renamed from `tp20_rx_id_owned_by_a_live_
//! sibling` to cover this second rejection reason) also rejects a new
//! request -- from ANY `cll_handle`, including the same one retrying its own
//! abandoned attempt -- proposing an `rx_id` still marked `abandoned`, and
//! [`deliver_tp20_connection_indication`] releases the quarantine (removes
//! the entry, without writing a `tp20_connection_results` entry nobody is
//! waiting on) the moment the delayed indication finally arrives and still
//! re-verifies against this exact entry -- and (Codex review finding, PR
//! #97, round 11), if that delayed indication says the device actually
//! established the connection, re-issues `IOCTL_TEARDOWN_CONNECTION` right
//! then: the abandonment-time best-effort teardown most likely raced ahead
//! of the device's own slot allocation and was rejected/no-op'd as a
//! result, so this is the first point at which the device has confirmed
//! the slot is genuinely occupied.
use super::super::{
    PARAM_TP20_APPLICATION_TYPE, PARAM_TP20_CHANNEL_SETUP_CAN_ID, PARAM_TP20_DESTINATION_ADDRESS,
    PARAM_TP20_PASSIVE_IDENTIFIER, PARAM_TP20_PASSIVE_RX_ID, PARAM_TP20_RX_ID_PROPOSAL,
    PARAM_TP20_TX_ID_PROPOSAL, SharedChannel, Tp20ConnEntry, Tp20Connection, Tp20ConnectionOutcome,
    Tp20ConnectionPhase, Tp20PassiveSlot,
};
use super::*;

/// Bounded wait floor for [`run_tp20_connection_request`] (ADR-188 Decision
/// item 2 step 4): Table 77's `CP_TP20T_E` (100 ms default) x
/// (`CP_TP20MNTC` (10 default) + 1) = 1100 ms, rounded up to a round 2 s
/// figure. These ten timing/count ComParams are deferred at native defaults
/// this stage (ADR-188 section 4) -- never client-configurable -- so this is
/// a fixed constant rather than read from a live `ComParamSet`.
const TP20_CONNECTION_TIMEOUT_MS: u64 = 2_000;

/// ADR-190's "Correction" paragraph under `### 4. Disarm / teardown` (Codex
/// review finding, P1, PR #99; design-advisor consult): grace period a
/// disarmed-while-`Listening` passive entry's own `Tp20ConnEntry::idle_
/// release_at` deadline is set this far past the disarm moment, margining
/// for a real device's own latency between an inbound accept landing and
/// its `CONNECTION_ESTABLISHED` indication becoming queue-readable. This
/// mock queues indications synchronously, so `grpc_mock` test correctness
/// rests entirely on the drain-watermark condition [`release_idle_passive_
/// slots`]/[`tp20_passive_idle_release_due`] enforce, never on this
/// constant's own value -- see ADR-190's accepted-residuals paragraph for
/// the narrow, conformance-dependent misattribution window a real device
/// exceeding this margin would reopen.
const TP20_PASSIVE_IDLE_RELEASE_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// The five minted `PARAM_TP20_*` ComParams (ADR-188 section 4), resolved
/// from a `ComParamSet` snapshot and packed into the clause 19 Table 78
/// `IOCTL_REQUEST_CONNECTION` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Tp20ConnectionParams {
    pub(super) setup_can_id: u32,
    pub(super) destination_address: u8,
    pub(super) tx_id_proposal: u16,
    pub(super) rx_id_proposal: u16,
    pub(super) application_type: u8,
}

/// Resolves [`Tp20ConnectionParams`] from `params`. Returns `Err` if ANY of
/// the five is absent -- ADR-188 Decision item 4: no spec-mandated default
/// exists for any of them, so a client must stage all five before
/// `CoptStartcomm`; validated here (at `CoptStartcomm` time, not
/// `CreateComLogicalLink` time -- the CAN-FD/ISO15765-on-CAN-FD deferral
/// shape, ADR-158/159) -- or if a PRESENT value does not fit the native
/// field width Table 78 packs it into (Codex review fix, PR #97): all five
/// `PARAM_TP20_*` ComParams are plain `Unum32` values with no width
/// restriction at the ComParam-storage layer, so
/// `destination_address`/`application_type` (native `u8`) and
/// `tx_id_proposal`/`rx_id_proposal` (native `u16`) must each be checked
/// with `try_from` rather than narrowed with `as`, which would otherwise
/// silently truncate an out-of-range value (e.g. a staged RX-ID of
/// `0x10321` silently becoming `0x0321`) instead of rejecting it -- letting
/// this service proceed to request/route the WRONG connection number with
/// no error. Each of the two error variants gets its own distinct,
/// accurate message; `setup_can_id` stays a plain `u32` (Table 78's own
/// 4-byte field, no narrowing).
pub(super) fn resolve_tp20_connection_params(
    params: &ComParamSet,
) -> Result<Tp20ConnectionParams, String> {
    let setup_can_id = *params
        .unum32
        .get(&PARAM_TP20_CHANNEL_SETUP_CAN_ID)
        .ok_or("CP_TP20ChannelSetupCanId must be staged before CoptStartcomm")?;
    let destination_address = *params
        .unum32
        .get(&PARAM_TP20_DESTINATION_ADDRESS)
        .ok_or("CP_TP20DestinationAddress must be staged before CoptStartcomm")?;
    let tx_id_proposal = *params
        .unum32
        .get(&PARAM_TP20_TX_ID_PROPOSAL)
        .ok_or("CP_TP20TxIdProposal must be staged before CoptStartcomm")?;
    let rx_id_proposal = *params
        .unum32
        .get(&PARAM_TP20_RX_ID_PROPOSAL)
        .ok_or("CP_TP20RxIdProposal must be staged before CoptStartcomm")?;
    let application_type = *params
        .unum32
        .get(&PARAM_TP20_APPLICATION_TYPE)
        .ok_or("CP_TP20ApplicationType must be staged before CoptStartcomm")?;
    let destination_address = u8::try_from(destination_address).map_err(|_| {
        format!(
            "CP_TP20DestinationAddress value {destination_address} exceeds the 1-byte range \
             Table 78 packs it into"
        )
    })?;
    let tx_id_proposal = u16::try_from(tx_id_proposal).map_err(|_| {
        format!(
            "CP_TP20TxIdProposal value {tx_id_proposal} exceeds the 2-byte range Table 78 packs \
             it into"
        )
    })?;
    let rx_id_proposal = u16::try_from(rx_id_proposal).map_err(|_| {
        format!(
            "CP_TP20RxIdProposal value {rx_id_proposal} exceeds the 2-byte range Table 78 packs \
             it into"
        )
    })?;
    let application_type = u8::try_from(application_type).map_err(|_| {
        format!(
            "CP_TP20ApplicationType value {application_type} exceeds the 1-byte range Table 78 \
             packs it into"
        )
    })?;
    Ok(Tp20ConnectionParams {
        setup_can_id,
        destination_address,
        tx_id_proposal,
        rx_id_proposal,
        application_type,
    })
}

/// Resolves ADR-190/Phase 7 Stage 7b's two minted passive-connection
/// ComParams (`PARAM_TP20_PASSIVE_IDENTIFIER`/`PARAM_TP20_PASSIVE_RX_ID`)
/// from a `ComParamSet` snapshot, mirroring
/// [`resolve_tp20_connection_params`]'s own shape/error-return convention
/// (a plain `String`, since this runs from the same async COP-processing
/// context that function does, not a synchronous RPC precondition).
///
/// - `Ok(None)`: NEITHER is staged -- ordinary "don't arm the passive
///   listener," the fall-through case `handle_start_comm`'s TP2.0 arm uses
///   to proceed with Stage 7a's active-connection resolution unchanged.
/// - `Err`: exactly one is staged (ambiguous/incomplete pair), or either is
///   staged alongside any of Stage 7a's five active-connection
///   `PARAM_TP20_*` ComParams (ambiguous intent, ADR-190 section 1), or a
///   staged value is out of range -- `identifier` must be `0x200-0x2EF`,
///   `rx_id_passive` must be `0x300-0x7FF` (a staged `0` is meaningless: not
///   staging at all already means "don't arm," ADR-190 section 1).
/// - `Ok(Some((identifier, rx_id_passive)))`: both staged and in range --
///   `handle_start_comm` should arm the passive listener with these values.
pub(super) fn resolve_tp20_passive_params(
    params: &ComParamSet,
) -> Result<Option<(u16, u16)>, String> {
    let identifier = params.unum32.get(&PARAM_TP20_PASSIVE_IDENTIFIER).copied();
    let rx_id_passive = params.unum32.get(&PARAM_TP20_PASSIVE_RX_ID).copied();

    let (identifier, rx_id_passive) = match (identifier, rx_id_passive) {
        (None, None) => return Ok(None),
        (Some(_), None) | (None, Some(_)) => {
            return Err(
                "CP_TP20PassiveIdentifier and CP_TP20PassiveRxId must both be staged together \
                 to arm the TP2.0 passive listener"
                    .to_string(),
            );
        }
        (Some(identifier), Some(rx_id_passive)) => (identifier, rx_id_passive),
    };

    // ADR-190 section 1: staging either passive ComParam alongside any of
    // Stage 7a's five active-connection ones is rejected as ambiguous
    // intent.
    let any_active_staged = [
        PARAM_TP20_CHANNEL_SETUP_CAN_ID,
        PARAM_TP20_DESTINATION_ADDRESS,
        PARAM_TP20_TX_ID_PROPOSAL,
        PARAM_TP20_RX_ID_PROPOSAL,
        PARAM_TP20_APPLICATION_TYPE,
    ]
    .iter()
    .any(|id| params.unum32.contains_key(id));
    if any_active_staged {
        return Err(
            "CP_TP20PassiveIdentifier/CP_TP20PassiveRxId cannot be staged alongside any Stage \
             7a active-connection CP_TP20* ComParam -- ambiguous intent"
                .to_string(),
        );
    }

    let identifier = u16::try_from(identifier).map_err(|_| {
        format!(
            "CP_TP20PassiveIdentifier value {identifier} exceeds the 2-byte range Table 77 \
             packs it into"
        )
    })?;
    let rx_id_passive = u16::try_from(rx_id_passive).map_err(|_| {
        format!(
            "CP_TP20PassiveRxId value {rx_id_passive} exceeds the 2-byte range Table 77 packs \
             it into"
        )
    })?;
    if !(0x200..=0x2EF).contains(&identifier) {
        return Err(format!(
            "CP_TP20PassiveIdentifier value {identifier:#06x} must be 0x200-0x2EF to arm the \
             passive listener"
        ));
    }
    if !(0x300..=0x7FF).contains(&rx_id_passive) {
        return Err(format!(
            "CP_TP20PassiveRxId value {rx_id_passive:#06x} must be 0x300-0x7FF to arm the \
             passive listener"
        ));
    }

    Ok(Some((identifier, rx_id_passive)))
}

/// Outcome of [`run_tp20_connection_request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tp20ConnectionRequestOutcome {
    /// The device assigned this TX-ID.
    Established(u32),
    /// The connection failed/was lost; the reason byte (clause 19.4.4 Table
    /// 81: `1` = timeout, `0xD6` = not supported, `0xD7` = temporarily not
    /// supported, `0xD8` = no resources free, `0` = teardown -- never
    /// reached from this function, which only ever issues a fresh request).
    Lost(u8),
    /// The synchronous native `IOCTL_REQUEST_CONNECTION` issue itself
    /// failed (e.g. `ERR_NOT_UNIQUE` for an RX-ID already in use by another
    /// established channel, clause 19.3.3.2) -- distinct from `Lost`, which
    /// is only ever a device-reported indication reason.
    Failed,
    /// Service-side rejection: the proposed RX-ID is currently unavailable,
    /// for either of two reasons (`run_tp20_connection_request`'s pre-insert
    /// [`tp20_rx_id_unavailable_for`] check -- see this module's own doc
    /// comment for why the live-sibling case can only ever observe a
    /// still-pending sibling, never an already-established one): (1) already
    /// claimed by a live sibling CLL's own still-PENDING
    /// `IOCTL_REQUEST_CONNECTION` on the same physical channel, or (2) still
    /// quarantined as `abandoned` (Codex review finding, PR #97, 7th round --
    /// this module's own doc comment on the quarantine mechanism), regardless
    /// of which `cll_handle` -- including the SAME one retrying -- proposes
    /// it. Distinct from `Failed` (a native-call failure) and from clause
    /// 19.3.3.2's own native `ERR_NOT_UNIQUE` (which only fires against an
    /// already-ESTABLISHED channel): this is the LOCAL enforcement neither
    /// native uniqueness nor a device-confirmed outcome itself covers. No
    /// native call is issued for this outcome.
    RxIdInUse,
    /// `cll_handle` went stale (disconnected/reconnected, i.e. its
    /// `connect_generation` no longer matches) partway through -- the
    /// caller must not act on any StartComm-success side effect.
    Stale,
    /// The physical channel suffered a hard error mid-wait
    /// (`handle_channel_hard_error` already ran the full loss-of-comms
    /// sequence for it) -- same "do not act on success" obligation as
    /// `Stale`.
    HardError,
}

/// Best-effort `IOCTL_TEARDOWN_CONNECTION` for a connection-request attempt
/// being abandoned locally (`reason` is a short human-readable clause
/// describing why, used only in the failure log line) without a
/// device-confirmed final outcome -- the native `IOCTL_REQUEST_CONNECTION`
/// call was already successfully issued for `rx_id` earlier in the caller,
/// so the device may have genuinely established the connection regardless
/// of why this side is walking away from it (Codex review finding, PR #97).
/// Tearing down here frees the slot immediately instead of leaking it until
/// the device's own maintenance-timeout self-heal (clause 19.3.1); if the
/// device never actually established it, clause 19.3.3.3 makes this call a
/// no-op the device rejects, which is harmless and logged only.
///
/// `pub(super)` (Codex review fix, PR #97, 8th round / Fix K): besides
/// [`run_tp20_connection_request`]'s own two internal call sites (mid-wait
/// staleness, local-deadline timeout), `events.rs`'s `handle_start_comm`
/// TP2.0 `Established` write-back arm also calls this directly for a THIRD
/// abandonment shape -- the connection became `Established` right as its
/// owning CLL disappeared, in the gap between `run_tp20_connection_request`
/// returning and the write-back running.
///
/// `reason`-only logging, deliberately returning nothing for a caller to act
/// on (Codex review findings investigated, P2, PR #97, rounds 19-20): a
/// synchronous `IOCTL_TEARDOWN_CONNECTION` failure
/// at any of this function's four call sites is NOT a reliable local signal
/// that no device-side indication is still in flight for `rx_id`, so none of
/// them may use it to decide whether quarantining is safe to skip. Two
/// distinct races were found and confirmed during rounds 19-20, at two
/// different call sites, before this conclusion held for all four:
/// - `deliver_tp20_connection_indication`'s late-`Established` re-teardown
///   branch (round 19): a synchronous failure there is routine evidence that
///   an EARLIER best-effort call (from one of this function's own two
///   `run_tp20_connection_request` call sites) already tore the connection
///   down -- that earlier call's own device confirmation can still be
///   delayed/in flight. Releasing the quarantine on this branch's own
///   failure was tried and reverted after `cargo-runner` caught it
///   regressing `abandoned_entrys_established_outcome_stays_quarantined_
///   until_the_followup_teardown_confirms` (`tests/grpc_mock/tp20.rs`).
/// - `events.rs`'s `handle_start_comm` write-back arm (round 20): even
///   though this call IS the first and only teardown attempt for `rx_id`
///   from THIS service's own perspective, the device may have independently
///   and spontaneously lost the connection before this call ever ran (no
///   ongoing monitoring exists for a later spontaneous loss once
///   established -- this module's own doc comment) -- in which case the
///   call fails (no matching native slot) precisely BECAUSE an indication
///   may already be queued, not because none is coming. Gating that
///   write-back arm's quarantine-insert on this call's success (mirroring
///   Fix V, round 17) was tried and reverted after this second finding.
///
/// Every call site therefore quarantines unconditionally regardless of this
/// function's own outcome, accepting the same leaked-native-slot residual
/// this function's own log message already documents.
pub(super) async fn best_effort_teardown_on_abandon(
    ctx: &ChannelPollCtx,
    cll_handle: u32,
    rx_id: u32,
    reason: &str,
) {
    let api = ctx.api.lock().await;
    if let Err(err) = api.tp20_teardown_connection(ctx.channel_id, rx_id) {
        warn!(
            cll_handle,
            rx_id,
            %err,
            reason,
            "best-effort TP2.0 IOCTL_TEARDOWN_CONNECTION on local abandonment failed -- a \
             leaked native slot self-heals via the device's own maintenance timeout if the \
             connection was never actually established (accepted residual)"
        );
    }
}

/// Originally `events.rs`'s `handle_start_comm` TP2.0 `Established`
/// write-back arm's own quarantine-insert decision (Codex review fix, PR
/// #97, 8th round / Fix K): whether to insert a fresh `abandoned` entry for
/// `rx_id`, quarantining this now-orphaned but genuinely-established
/// connection the same way `run_tp20_connection_request`'s own two internal
/// abandonment paths do (Fix J, 7th round) -- that write-back site is
/// conceptually a THIRD abandonment path (this module's own doc comment on
/// the mechanism).
///
/// Reused since round 14 (Codex review fix, PR #97) by every NORMAL,
/// successful teardown call site for an `Established` TP2.0 connection --
/// `events.rs`'s `handle_stop_comm` and `rpc_link.rs`'s
/// `DisconnectComLogicalLink`/`DestroyComLogicalLink` cleanup -- for the
/// same underlying reason: `IOCTL_TEARDOWN_CONNECTION` is non-blocking, so
/// the device's own delayed `CONNECTION_LOST` confirmation can still arrive
/// after these call sites return, and nothing else prevents a promptly-
/// issued new `CoptStartcomm` from registering a fresh pending entry for
/// the SAME `rx_id` before that confirmation drains -- `pub(in crate::
/// service)` (rather than `pub(super)`) exists specifically so `rpc_link.rs`
/// can reach it as `events::quarantine_tp20_connection_for_orphaned_
/// write_back`, mirroring `cancel_j1939_claims_for_cll`'s own identical
/// cross-module visibility shape.
///
/// Vacant-only (`HashMap::entry`/`or_insert`) at every call site: for the
/// original write-back use, `run_tp20_connection_request`'s own post-loop
/// cleanup has ALREADY unconditionally removed this attempt's own
/// `tp20_connections` entry (its local `abandoned` flag is never set for an
/// `Established` outcome, so that cleanup's own `else` branch -- remove,
/// not mark -- always runs for it) -- so there is nothing of THIS attempt's
/// own left to mark in place, only a fresh entry to insert. For the round-14
/// normal-teardown reuse, the entry was likewise already removed when the
/// connection first established (the same "resolves either way" cleanup).
/// Either way, if something else has already claimed `rx_id` in the interim
/// (a genuine new registration racing into the gap), this call leaves it
/// alone rather than clobbering a live attempt.
///
/// Pure -- unit-testable with a plain `HashMap`, the same extraction
/// [`tp20_rx_id_unavailable_for`]/[`resolve_tp20_connection_indication`]/
/// [`tp20_connection_indication_is_for_an_abandoned_entry`] below use for
/// their own decisions.
pub(in crate::service) fn quarantine_tp20_connection_for_orphaned_write_back(
    rx_id: u32,
    cll_handle: u32,
    connect_generation: u64,
    connections: &mut HashMap<u32, Tp20ConnEntry>,
) {
    connections.entry(rx_id).or_insert(Tp20ConnEntry {
        cll_handle,
        connect_generation,
        abandoned: true,
        expect_stale_lost: false,
        passive: false,
        idle_release_at: None,
    });
}

/// Drives SAE J2534-2 clause 19's connection-request lifecycle (ADR-188
/// Decision item 2, steps 2-5) for `cll_handle`: packs and issues
/// `IOCTL_REQUEST_CONNECTION`, registers this attempt in the physical
/// channel's `SharedChannel::tp20_connections` routing map (so
/// [`deliver_tp20_connection_indication`] can find it), then actively
/// drives `poll_rx` -- mirroring `run_j1939_claim_loop`'s own "sleep
/// `POLL_INTERVAL_MS`, poll, recheck" shape -- until either a matching
/// outcome lands in `SharedChannel::tp20_connection_results` for this
/// `cll_handle`, or [`TP20_CONNECTION_TIMEOUT_MS`] elapses (treated as
/// `Lost(1)`, the timeout reason byte, clause 19.4.4 Table 81).
///
/// Unlike `run_j1939_claim_loop`, this function does not check
/// `cancelled_cops`/`stop_comm_pending` mid-wait (documented residual, see
/// this module's own doc comment) -- a `CancelComPrimitive` or
/// `PDU_COPT_STOPCOMM` racing an in-flight request still resolves once the
/// bounded wait above completes.
pub(super) async fn run_tp20_connection_request(
    cll_handle: u32,
    connect_generation: u64,
    params: &Tp20ConnectionParams,
    ctx: &ChannelPollCtx,
) -> Tp20ConnectionRequestOutcome {
    let rx_id = u32::from(params.rx_id_proposal);
    // Set `true` at either `best_effort_teardown_on_abandon` call site below,
    // right before its own `break` -- tells the post-loop cleanup block to
    // quarantine this attempt's entry (mark `abandoned`) instead of removing
    // it outright (Codex review finding, PR #97, 7th round; this module's
    // own doc comment on the quarantine mechanism).
    let mut abandoned = false;

    // ADR-080-conformant order (mirroring `run_j1939_claim_loop`): issue the
    // native call and register the routing entry under one `shared_channels`
    // critical section, `api` nested underneath.
    {
        let mut chans = ctx.service.shared_channels.lock().await;
        // Pre-insert availability check (mirrors `run_j1939_claim_loop`'s own
        // `owned_by_a_live_sibling` check, `events_j1939_claim.rs` lines
        // 888-905): unlike J1939's NAME-driven candidate list, TP2.0's
        // RX-ID is caller-proposed via ComParams with no "next candidate"
        // to advance to, so an unavailable-RX-ID collision here must REJECT
        // this request outright rather than retry -- and must do so BEFORE
        // issuing the native call, so a doomed request never wastes a
        // native round-trip (the same "never even issue a native claim"
        // reasoning the J1939 precedent's own comment gives). Clause
        // 19.3.3.2's native `ERR_NOT_UNIQUE` only fires against an
        // already-ESTABLISHED channel, not a still-pending request, so it
        // does not itself cover either reason this check closes on
        // (live-sibling collision, or the same/a different `cll_handle`
        // retrying a still-quarantined `abandoned` entry -- Codex review
        // finding, PR #97, 7th round, this module's own doc comment). The
        // decision itself is factored into a pure helper
        // ([`tp20_rx_id_unavailable_for`]) the same way
        // [`resolve_tp20_connection_indication`] below extracts its own
        // routing decision -- see that function's own doc comment for why
        // (direct unit-testability independent of the live
        // `logical_links`/`SharedChannel` maps this call site reads from).
        //
        // Read-only (`.values()`, not `.values_mut()`): this borrow is
        // scoped tightly to this one check rather than held across the
        // native call below, so the registration-time reconcile step
        // further down (Codex review finding via `edge-case-hunter`,
        // design-advisor consult, P1, PR #97, round 24) can acquire its own
        // `&mut chans` without a borrow conflict.
        let (self_still_live, rx_id_unavailable) = {
            let Some(sc) = chans.values().find(|sc| sc.channel_id == ctx.channel_id) else {
                return Tp20ConnectionRequestOutcome::Stale;
            };
            let links = ctx.logical_links.lock().await;
            let self_still_live = links.get(&cll_handle).is_some_and(|l| {
                l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
            });
            let rx_id_unavailable =
                tp20_rx_id_unavailable_for(rx_id, cll_handle, &sc.tp20_connections, |h| {
                    links.get(&h).map(|l| l.connect_generation)
                });
            (self_still_live, rx_id_unavailable)
        };
        // Codex review finding (PR #97): this CLL's own liveness must be
        // rechecked here too, not just a sibling's -- `handle_start_comm`
        // records `Requested` state and queues this attempt, but
        // `Disconnect`/`DestroyComLogicalLink` can race in before this
        // dispatch actually runs (the physical channel can stay open the
        // whole time if a sibling CLL still shares it). Without this
        // check, a doomed request for an already-dead CLL would still
        // issue the native call -- the mirror of the ownership check
        // above's own "never even issue a native claim" reasoning,
        // applied to self-liveness instead of sibling-ownership.
        if !self_still_live {
            return Tp20ConnectionRequestOutcome::Stale;
        }
        if rx_id_unavailable {
            return Tp20ConnectionRequestOutcome::RxIdInUse;
        }

        let issue_result = {
            let api = ctx.api.lock().await;
            api.tp20_request_connection(
                ctx.channel_id,
                params.setup_can_id,
                params.destination_address,
                params.tx_id_proposal,
                params.rx_id_proposal,
                params.application_type,
            )
        };
        if let Err(err) = issue_result {
            warn!(
                cll_handle,
                rx_id,
                %err,
                "TP2.0 IOCTL_REQUEST_CONNECTION issue failed"
            );
            return Tp20ConnectionRequestOutcome::Failed;
        }

        // Codex review finding via `edge-case-hunter`, design-advisor
        // consult (P1, PR #97, round 24): a SUCCESSFUL native
        // `IOCTL_REQUEST_CONNECTION` for `rx_id` is itself proof, per
        // clause 19.3.3.2's uniqueness rule (the device only rejects this
        // call with `ERR_NOT_UNIQUE` against an rx_id an ESTABLISHED
        // channel already occupies), that any sibling CLL still LOCALLY
        // believing `phase == Established` for this SAME `rx_id` is
        // already stale device-side -- the device just told us, by letting
        // this request through, that it doesn't consider `rx_id` occupied
        // anymore. That sibling's own eventual `CONNECTION_LOST`
        // indication (queued device-side per Table 81 whenever an
        // established connection is lost) is therefore a genuine, still-
        // in-flight frame that will arrive at SOME later poll tick --
        // addressed to the SAME `rx_id` THIS brand-new, unrelated entry is
        // about to claim. Without reconciling that sibling's own belief
        // right now, `deliver_tp20_connection_indication` would later
        // route that stale `Lost` frame straight to THIS entry's own
        // `cll_handle` (nothing else distinguishes "the CURRENT occupant
        // of this rx_id slot" from "the specific attempt an old, delayed
        // indication actually belongs to" -- the round-22/23 reconciliation
        // only covers the NO-MATCH case, when nothing has re-registered
        // this rx_id yet), misattributing the sibling's own old loss reason
        // onto this request and permanently leaking the sibling's own
        // native connection if it was actually still healthy at that
        // moment. Reconciling here, before this entry's own insert, closes
        // that identity gap at its root: the sibling's phase flips to
        // `Lost` (propagating for free to every phase-gated consumer, and
        // best-effort stopping its own repeat slots -- see
        // `reconcile_live_established_cll`'s own doc comment), and
        // `expect_stale_lost` marks THIS entry so the one stale `Lost`
        // frame that's now provably still in flight for `rx_id` gets
        // swallowed instead of misattributed the moment it arrives (see
        // `deliver_tp20_connection_indication`'s own swallow check).
        let expect_stale_lost = !reconcile_live_established_cll(ctx, &mut chans, rx_id)
            .await
            .is_empty();

        let Some(sc) = chans
            .values_mut()
            .find(|sc| sc.channel_id == ctx.channel_id)
        else {
            return Tp20ConnectionRequestOutcome::Stale;
        };
        sc.tp20_connections.insert(
            rx_id,
            Tp20ConnEntry {
                cll_handle,
                connect_generation,
                abandoned: false,
                expect_stale_lost,
                passive: false,
                idle_release_at: None,
            },
        );
        sc.tp20_connection_results.remove(&cll_handle);
    }

    let deadline = tokio::time::Instant::now() + Duration::from_millis(TP20_CONNECTION_TIMEOUT_MS);
    let poll_interval = Duration::from_millis(POLL_INTERVAL_MS);
    let outcome = loop {
        let still_on_this_channel = {
            let links = ctx.logical_links.lock().await;
            links.get(&cll_handle).is_some_and(|l| {
                l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
            })
        };
        if !still_on_this_channel {
            // Codex review finding (PR #97): the native request WAS
            // successfully issued in the critical section above before
            // this CLL went stale (`Disconnect`/`DestroyComLogicalLink`
            // racing in mid-wait, on a physical channel a sibling CLL
            // keeps open) -- best-effort tear it down for the same reason
            // the local-deadline branch below does, rather than silently
            // abandoning a request the device may still complete. See
            // `best_effort_teardown_on_abandon`'s own doc comment for why
            // its result is never used to decide whether to quarantine.
            best_effort_teardown_on_abandon(ctx, cll_handle, rx_id, "this CLL went stale mid-wait")
                .await;
            abandoned = true;
            break Tp20ConnectionRequestOutcome::Stale;
        }

        let result = {
            let mut chans = ctx.service.shared_channels.lock().await;
            chans
                .values_mut()
                .find(|sc| sc.channel_id == ctx.channel_id)
                .and_then(|sc| sc.tp20_connection_results.remove(&cll_handle))
        };
        match result {
            Some(Tp20ConnectionOutcome::Established(tx_id)) => {
                break Tp20ConnectionRequestOutcome::Established(tx_id);
            }
            Some(Tp20ConnectionOutcome::Lost(reason)) => {
                break Tp20ConnectionRequestOutcome::Lost(reason);
            }
            None => {}
        }

        if tokio::time::Instant::now() >= deadline {
            // No indication arrived at all within the bounded wait --
            // treated the same as an explicit `Lost(1)` (timeout reason
            // byte, Table 81), the same forgiving-reading precedent
            // `run_j1939_claim_loop`'s own doc comment flags for its
            // identical "no indication at all" case.
            //
            // Codex review finding (PR #97): the native request WAS
            // successfully issued earlier in this function -- the device
            // may have genuinely established it, with the
            // `CONNECTION_ESTABLISHED` indication simply delayed past this
            // local deadline (a nonblocking-adapter race, not a native
            // failure). Since this attempt is being abandoned locally
            // regardless, best-effort tear it down now -- see
            // [`best_effort_teardown_on_abandon`]'s own doc comment.
            best_effort_teardown_on_abandon(
                ctx,
                cll_handle,
                rx_id,
                "the local wait deadline elapsed",
            )
            .await;
            abandoned = true;
            break Tp20ConnectionRequestOutcome::Lost(1);
        }

        run_detached_registrant_maintenance(ctx).await;
        tokio::time::sleep(poll_interval).await;
        if !poll_rx(ctx).await.is_ok() {
            break Tp20ConnectionRequestOutcome::HardError;
        }
    };

    // This stage does not monitor a connection for a LATER spontaneous loss
    // once established (this module's own doc comment) -- the routing entry
    // is removed once this initial outcome resolves (established, or a
    // device-reported/native-issue-failure loss), regardless of which
    // outcome it is. The one exception: an outcome reached via LOCAL
    // abandonment (`abandoned`, set above at either `best_effort_teardown_
    // on_abandon` call site) is quarantined instead -- see this module's own
    // doc comment on the mechanism (Codex review finding, PR #97, 7th
    // round).
    {
        let mut chans = ctx.service.shared_channels.lock().await;
        if let Some(sc) = chans
            .values_mut()
            .find(|sc| sc.channel_id == ctx.channel_id)
        {
            // Ownership recheck (mirrors `run_j1939_claim_loop`'s own
            // cleanup-gate check, `events_j1939_claim.rs` lines ~1085-1104):
            // only touch the map entry if it still belongs to THIS attempt.
            // The pre-insert availability check above should make a removal
            // race structurally unreachable (a live sibling can never have
            // overwritten this entry out from under us), but keep the check
            // explicit here too rather than relying on that as an unstated
            // invariant, matching the J1939 precedent's own stated reasoning
            // for keeping its analogous check even where it says the race
            // isn't currently reachable there either.
            let still_owns_entry = sc.tp20_connections.get(&rx_id).is_some_and(|entry| {
                entry.cll_handle == cll_handle && entry.connect_generation == connect_generation
            });
            if still_owns_entry {
                if abandoned {
                    // Quarantine: mark in place rather than remove, so a
                    // same-`rx_id` retry cannot register an indistinguishable
                    // new entry while the device may still deliver a delayed
                    // indication for THIS attempt. Released by
                    // `deliver_tp20_connection_indication` once that
                    // indication finally arrives (or never, if this
                    // `SharedChannel` entry itself is torn down first).
                    if let Some(entry) = sc.tp20_connections.get_mut(&rx_id) {
                        entry.abandoned = true;
                    }
                } else {
                    sc.tp20_connections.remove(&rx_id);
                }
            }
            // Drain any `tp20_connection_results` entry left behind: if the
            // wait loop above broke out via the `Stale` staleness check on
            // the same tick `deliver_tp20_connection_indication` posted an
            // outcome, the rest of that iteration (including the
            // `tp20_connection_results.remove` read) never ran, leaking a
            // one-entry `HashMap` slot. No ownership gate needed here --
            // unlike `tp20_connections` (keyed by the caller-proposed
            // `rx_id`, which two CLLs can collide on), `cll_handle` is
            // never reused (`service.rs`'s monotonic `next_cll_handle`), so
            // this key can never collide across CLLs.
            sc.tp20_connection_results.remove(&cll_handle);
        }
    }

    outcome
}

/// Outcome of [`arm_tp20_passive_listener`] (ADR-190/Phase 7 Stage 7b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tp20PassiveArmOutcome {
    /// Armed successfully -- `CoptStartcomm` completes immediately
    /// ("arm-and-complete," ADR-190 Decision), no wait involved.
    Armed,
    /// The interface's single passive slot is already armed by a live CLL
    /// (ADR-190 section 2 / section 3 step 1) -- maps to the same
    /// `PduErrEvtRscLocked` shape the `0xD8` reason byte already maps to for
    /// a network-side rejection (keeping "the passive slot is a resource,
    /// and it is busy" one consistent error semantics regardless of which
    /// side detected the conflict).
    SlotBusy,
    /// `rx_id_passive` is unavailable: quarantined or claimed by a live
    /// sibling's own still-pending active request
    /// ([`tp20_rx_id_unavailable_for`], section 3 step 2), or occupied by a
    /// live sibling's own already-`Established` active connection (section
    /// 3 step 3 -- an active connection's routing-map entry is removed once
    /// it establishes, so step 2's map check alone cannot see it). Same
    /// `PduErrEvtRscLocked` shape as `SlotBusy`.
    RxIdInUse,
    /// The native `SET_CONFIG` call(s) failed; no state was mutated.
    NativeFailed,
    /// `cll_handle` went stale (disconnected/reconnected) before arming
    /// could complete -- the caller must not act on any success side
    /// effect.
    Stale,
}

/// Implements ADR-190 section 3 steps 1-7: arms `cll_handle`'s passive
/// listener on `ctx.channel_id`'s physical channel. Mirrors
/// [`run_tp20_connection_request`]'s own lock-acquisition shape -- ONE
/// `shared_channels` hold spans every step (Decision: "arm-and-complete" is
/// fully synchronous, so unlike the active-connection request there is no
/// wait loop to release the lock across), with `logical_links`/`api` nested
/// underneath per this crate's own lock order (ADR-080).
///
/// Unlike [`run_tp20_connection_request`]'s own registration-time reconcile
/// step, this function deliberately does NOT run
/// [`reconcile_live_established_cll`]/set `expect_stale_lost` (ADR-190
/// section 3): that mechanism exists because a successful native
/// `IOCTL_REQUEST_CONNECTION` is itself proof (clause 19.3.3.2) that the
/// device no longer considers the target RX-ID occupied -- arming issues no
/// such call, so no equivalent proof exists here.
pub(super) async fn arm_tp20_passive_listener(
    cll_handle: u32,
    connect_generation: u64,
    identifier: u16,
    rx_id_passive: u16,
    ctx: &ChannelPollCtx,
) -> Tp20PassiveArmOutcome {
    let rx_id = u32::from(rx_id_passive);
    let mut chans = ctx.service.shared_channels.lock().await;

    // Steps 1-3: read-only availability checks, mirroring `run_tp20_
    // connection_request`'s own pre-insert shape.
    let (self_still_live, slot_busy, rx_id_unavailable, established_collision) = {
        let Some(sc) = chans.values().find(|sc| sc.channel_id == ctx.channel_id) else {
            return Tp20PassiveArmOutcome::Stale;
        };
        let links = ctx.logical_links.lock().await;
        let self_still_live = links.get(&cll_handle).is_some_and(|l| {
            l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
        });
        // Step 1.
        let slot_busy = sc.tp20_passive.is_some();
        // Step 2.
        let rx_id_unavailable =
            tp20_rx_id_unavailable_for(rx_id, cll_handle, &sc.tp20_connections, |h| {
                links.get(&h).map(|l| l.connect_generation)
            });
        // Step 3: an already-`Established` active connection's own routing
        // entry is removed once it establishes, so step 2's map check alone
        // cannot see it -- scan live CLLs on this channel directly instead
        // (mirroring `reconcile_live_established_cll`'s own scan shape, but
        // read-only here, no mutation).
        let established_collision = links.values().any(|l| {
            l.channel_id == Some(ctx.channel_id)
                && l.tp20_connection.is_some_and(|c| {
                    c.phase == Tp20ConnectionPhase::Established && c.requested_rx_id == rx_id
                })
        });
        (
            self_still_live,
            slot_busy,
            rx_id_unavailable,
            established_collision,
        )
    };
    if !self_still_live {
        return Tp20PassiveArmOutcome::Stale;
    }
    if slot_busy {
        return Tp20PassiveArmOutcome::SlotBusy;
    }
    if rx_id_unavailable || established_collision {
        return Tp20PassiveArmOutcome::RxIdInUse;
    }

    // Step 4: issue the native SET_CONFIG calls, still under `chans`
    // (mirroring `run_tp20_connection_request`'s own native-call-while-
    // holding-shared_channels shape).
    //
    // Codex review finding (P2, PR #99, round 4): the previous two-call
    // sequence (`TP2_0_IDENTIFER` first, `TP2_0_RXIDPASSIVE` second) is
    // unsafe whenever a PRIOR disarm's own best-effort clear
    // (`best_effort_disarm_tp20_passive_native_config`) only partially
    // succeeded, leaving a stale nonzero `TP2_0_RXIDPASSIVE` on the device
    // (that helper always attempts both clears, so either one alone can
    // fail independently -- `slot_busy` above only checks THIS service's
    // own tracked state, never the device's actual native config, so it
    // cannot detect this). Writing the NEW nonzero identifier first then
    // briefly pairs it with that STALE nonzero rx_id -- clause 19.3.3.1's
    // both-nonzero accept condition is satisfied for that stale rx_id
    // during the real-world gap between the two blocking IOCTL calls, and
    // an inbound accept landing there has no registered `Tp20ConnEntry`
    // (this arm's own entry, keyed on the NEW rx_id, is not inserted until
    // step 5, after both calls return) -- an untracked native connection.
    // The mirror-image residual (a prior disarm's `TP2_0_IDENTIFER` clear
    // failing, `TP2_0_RXIDPASSIVE` succeeding) is symmetric.
    //
    // Fixed by an unconditional 0(a)-4(b)-4(c) sequence instead of a plain
    // 2-call one: (a) force `TP2_0_IDENTIFER` to 0 FIRST, regardless of
    // whatever value the device may already hold -- this alone already
    // guarantees the device cannot auto-accept, since one side of the
    // both-nonzero condition is now known-zero, neutralizing a stale
    // residual on EITHER param; (b) write the new `TP2_0_RXIDPASSIVE` next,
    // still safe since identifier is 0; (c) write the new `TP2_0_IDENTIFER`
    // LAST -- the single call that actually enables the listener, and by
    // this point `TP2_0_RXIDPASSIVE` already holds its correct new value
    // from (b), so no window ever pairs a stale value with a live nonzero
    // counterpart. A failure at (a) aborts immediately (nothing attempted
    // yet, matching the original single-call failure shape); a failure at
    // (b) or (c) leaves the device at `TP2_0_IDENTIFER = 0` (disabled,
    // never a mismatched pair) with a best-effort rollback of whatever (b)
    // already wrote, mirroring this mechanism's own "always attempt
    // cleanup, never leave known-bad state uncleaned" convention (e.g.
    // `best_effort_disarm_tp20_passive_native_config`'s own unconditional
    // clear).
    let api = ctx.api.lock().await;
    if let Err(err) = api.set_config_u32(ctx.channel_id, j2534_0404::CONFIG_TP2_0_IDENTIFER, 0) {
        warn!(
            cll_handle,
            %err,
            "TP2.0 passive-arm defensive SET_CONFIG(TP2_0_IDENTIFER, 0) failed -- aborting \
             the arm rather than risk pairing a stale native param with a live nonzero one"
        );
        drop(api);
        return Tp20PassiveArmOutcome::NativeFailed;
    }
    if let Err(err) = api.set_config_u32(
        ctx.channel_id,
        j2534_0404::CONFIG_TP2_0_RXIDPASSIVE,
        u32::from(rx_id_passive),
    ) {
        warn!(
            cll_handle,
            rx_id_passive,
            %err,
            "TP2.0 passive-arm SET_CONFIG(TP2_0_RXIDPASSIVE) failed"
        );
        drop(api);
        return Tp20PassiveArmOutcome::NativeFailed;
    }
    if let Err(err) = api.set_config_u32(
        ctx.channel_id,
        j2534_0404::CONFIG_TP2_0_IDENTIFER,
        u32::from(identifier),
    ) {
        warn!(
            cll_handle,
            identifier,
            %err,
            "TP2.0 passive-arm SET_CONFIG(TP2_0_IDENTIFER) failed -- rolling back the \
             already-applied TP2_0_RXIDPASSIVE"
        );
        if let Err(rollback_err) =
            api.set_config_u32(ctx.channel_id, j2534_0404::CONFIG_TP2_0_RXIDPASSIVE, 0)
        {
            warn!(
                cll_handle,
                %rollback_err,
                "TP2.0 passive-arm rollback SET_CONFIG(TP2_0_RXIDPASSIVE, 0) also failed -- \
                 leaked native config is an accepted residual (ADR-190 Consequences)"
            );
        }
        drop(api);
        return Tp20PassiveArmOutcome::NativeFailed;
    }
    drop(api);

    // Steps 5-7: persistent routing entry, exclusivity token, per-CLL
    // state -- all still under the SAME `chans` hold.
    let Some(sc) = chans
        .values_mut()
        .find(|sc| sc.channel_id == ctx.channel_id)
    else {
        return Tp20PassiveArmOutcome::Stale;
    };
    sc.tp20_connections.insert(
        rx_id,
        Tp20ConnEntry {
            cll_handle,
            connect_generation,
            abandoned: false,
            expect_stale_lost: false,
            passive: true,
            idle_release_at: None,
        },
    );
    sc.tp20_passive = Some(Tp20PassiveSlot {
        cll_handle,
        connect_generation,
        identifier,
        rx_id_passive,
    });

    let mut links = ctx.logical_links.lock().await;
    if let Some(link) = links.get_mut(&cll_handle).filter(|l| {
        l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
    }) {
        link.tp20_connection = Some(Tp20Connection {
            requested_rx_id: rx_id,
            established_tx_id: None,
            phase: Tp20ConnectionPhase::Listening,
            passive: true,
        });
    }

    Tp20PassiveArmOutcome::Armed
}

/// ADR-190/Phase 7 Stage 7b section 4, step 1: best-effort clears both
/// native `SET_CONFIG` params, `TP2_0_IDENTIFER` first -- stops the device
/// from accepting any further inbound connection before anything else runs.
/// Takes a plain `&J2534Api0404` reference rather than acquiring its own
/// lock (unlike [`arm_tp20_passive_listener`]/[`best_effort_teardown_on_
/// abandon`], which own their own `ctx.api` acquisition): this mechanism's
/// three disarm call sites (`events.rs::handle_stop_comm`,
/// `rpc_link.rs`'s `DestroyComLogicalLink`/`DisconnectComLogicalLink`) each
/// already hold a locked `J2534Api0404` guard of their own by the time they
/// reach this step (`ctx.api`, or `self.api` respectively) -- re-acquiring
/// either lock here would self-deadlock against the guard already held by
/// the caller (this repo's mutexes are not reentrant), the same reasoning
/// `stop_repeat_slots_for_cll`'s own doc comment gives for its own
/// `logical_links`/`api`-only (never `shared_channels`) acquisition.
///
/// Returns `config_cleared: bool` -- `true` if EITHER `SET_CONFIG` call
/// above succeeded (ADR-190's "Correction" paragraph under `### 4. Disarm /
/// teardown`, Codex review finding, P1, PR #99; design-advisor consult):
/// clause 19.3.3.1's own accept condition requires BOTH `TP2_0_IDENTIFER`/
/// `TP2_0_RXIDPASSIVE` to stay nonzero, so either call alone succeeding is
/// already enough to make the device unable to auto-accept a NEW inbound
/// connection on this listener ever again. Both calls are still issued
/// unconditionally regardless of this return value -- only the caller's own
/// use of it is new. The caller (`quarantine_tp20_passive_slot_on_disarm`)
/// uses this to decide whether a disarm that never reached `Established`
/// (so never issued a real `TEARDOWN_CONNECTION`, and therefore has no
/// indication ever coming to release its own quarantine the ordinary way)
/// is safe to bound with a release deadline instead of quarantining
/// `rx_id_passive` forever -- see that function's own doc comment for the
/// full mechanism.
pub(in crate::service) fn best_effort_disarm_tp20_passive_native_config(
    api: &j2534_0404::J2534Api0404,
    channel_id: ChannelId,
    cll_handle: u32,
) -> bool {
    let identifer_cleared =
        if let Err(err) = api.set_config_u32(channel_id, j2534_0404::CONFIG_TP2_0_IDENTIFER, 0) {
            warn!(
                cll_handle,
                %err,
                "best-effort TP2.0 passive-disarm SET_CONFIG(TP2_0_IDENTIFER, 0) failed"
            );
            false
        } else {
            true
        };
    let rxidpassive_cleared =
        if let Err(err) = api.set_config_u32(channel_id, j2534_0404::CONFIG_TP2_0_RXIDPASSIVE, 0) {
            warn!(
                cll_handle,
                %err,
                "best-effort TP2.0 passive-disarm SET_CONFIG(TP2_0_RXIDPASSIVE, 0) failed"
            );
            false
        } else {
            true
        };
    identifer_cleared || rxidpassive_cleared
}

/// ADR-190/Phase 7 Stage 7b section 4, steps 3-4: unconditionally marks the
/// persistent `Tp20ConnEntry` for `rx_id_passive` `abandoned` in place
/// (never removed -- the same unconditional-quarantine convention ADR-188
/// rounds 19-21 established for every other quarantine-insert site in this
/// mechanism: an accept or a spontaneous device-side loss can race a
/// disarm exactly as it can race an active connection's own teardown, so no
/// synchronous native-call result may safely decide whether quarantining is
/// skippable), then clears [`SharedChannel::tp20_passive`]. Takes `&mut
/// SharedChannel` directly (the same reason [`best_effort_disarm_tp20_
/// passive_native_config`] takes a plain `api` reference above): every call
/// site already holds `shared_channels` and has its own `&mut SharedChannel`
/// in hand by this point.
///
/// A delayed `Established` indication landing on the now-abandoned entry
/// re-tears-down and holds the quarantine until the follow-up `Lost`
/// drains -- the existing Fix O/S/L machinery
/// ([`deliver_tp20_connection_indication`]'s own abandoned-entry branch,
/// ADR-188 Consequences) already covers this exactly, with no
/// passive-specific extension needed.
///
/// **`idle_release_at` stamping (ADR-190's "Correction" paragraph under
/// `### 4. Disarm / teardown`, Codex review finding, P1, PR #99;
/// design-advisor consult):** unconditional quarantine is correct once this
/// slot ever reached `Established` -- step 2 (the caller) already issued a
/// real `TEARDOWN_CONNECTION`, whose eventual confirmation (or clause
/// 19.3.1's own maintenance-timeout loss) releases the quarantine the
/// ordinary way. It is unsound for a disarm still `Listening` (never
/// accepted a connection): no native call was ever issued in that case, so
/// nothing will ever deliver a follow-up indication to release it --
/// permanently leaking `rx_id_passive`. `was_established` and
/// `config_cleared` (from [`best_effort_disarm_tp20_passive_native_
/// config`]'s own return value, the caller's step-1 result) together gate a
/// bounded release instead: when `!was_established && config_cleared`, this
/// entry's own `idle_release_at` is stamped `now + [`TP20_PASSIVE_IDLE_
/// RELEASE_GRACE`]` -- once the native config-clear itself has succeeded,
/// clause 19.3.3.1's own both-nonzero accept condition means the device can
/// never generate a NEW indication for this listener again, so whatever
/// will ever arrive for `rx_id_passive` is already sitting in the native RX
/// queue and one exhaustive drain past this deadline (the same ADR-101
/// Decision §E soundness condition [`release_idle_passive_slots`] reuses)
/// is guaranteed to flush it. Releasing immediately on `phase == Listening`
/// alone, or gating on the teardown call's own synchronous result, were
/// both explicitly considered and rejected as unsafe (ADR-188 rounds
/// 19-21's identical conclusion for the synchronous-result case) -- see the
/// ADR's own Correction paragraph for the full argument. Every other
/// combination (`was_established` regardless of `config_cleared`, or
/// `!was_established && !config_cleared`) stamps `None` -- the pre-existing
/// indefinite-quarantine behavior, correct in both cases (a real teardown
/// call is what releases the first; the device may genuinely still be able
/// to auto-accept on this RX-ID in the second, since the config-clear
/// itself failed).
pub(in crate::service) fn quarantine_tp20_passive_slot_on_disarm(
    rx_id_passive: u32,
    sc: &mut SharedChannel,
    was_established: bool,
    config_cleared: bool,
) {
    if let Some(entry) = sc.tp20_connections.get_mut(&rx_id_passive) {
        entry.abandoned = true;
        entry.idle_release_at = if !was_established && config_cleared {
            Some(tokio::time::Instant::now() + TP20_PASSIVE_IDLE_RELEASE_GRACE)
        } else {
            None
        };
    }
    sc.tp20_passive = None;
}

/// ADR-190's "Correction" paragraph under `### 4. Disarm / teardown` (Codex
/// review finding, P1, PR #99; design-advisor consult): `true` only for an
/// `abandoned && passive` entry whose [`Tp20ConnEntry::idle_release_at`]
/// deadline the per-channel drain watermark has already proven safe to
/// release against -- mirrors `events.rs::is_cyclic_reap_sound`'s own "a
/// stale watermark only ever defers, never wrongly permits" soundness
/// argument (ADR-101 Decision §E), reused here rather than re-derived.
/// Extracted as a pure, sync, directly-unit-testable function, the same
/// reason `events.rs`'s `is_cyclic_deadline_expired`/`is_cyclic_reap_sound`
/// are ([`release_idle_passive_slots`] itself is not unit-testable in
/// isolation -- it needs a live `ChannelPollCtx`).
fn tp20_passive_idle_release_due(
    entry: &Tp20ConnEntry,
    watermark: Option<tokio::time::Instant>,
) -> bool {
    entry.abandoned
        && entry.passive
        && entry
            .idle_release_at
            .is_some_and(|dl| watermark.is_some_and(|wm| wm >= dl))
}

/// ADR-190's "Correction" paragraph under `### 4. Disarm / teardown` (Codex
/// review finding, P1, PR #99; design-advisor consult): the ADR-101
/// Decision §E-style bounded-release sweep for a passive listener disarmed
/// while still `Listening` -- see [`Tp20ConnEntry::idle_release_at`]'s own
/// doc comment for the full mechanism this closes. Called unconditionally
/// from `run_due_tick_duties` (`events.rs`), immediately alongside
/// `reap_expired_cyclic_registrants` -- same watermark-read-before-lock
/// ordering, ADR-080-safe for the identical reason: a stale watermark only
/// ever defers release, never wrongly permits it.
///
/// Unlike `reap_expired_cyclic_registrants` (which scans every CLL across
/// `logical_links`), this sweep only ever needs `ctx.channel_id`'s own
/// `SharedChannel`: `tp20_connections` is keyed per physical channel, not
/// per CLL, so there is no UUDT-companion-channel case to consult a second
/// watermark for the way that cyclic reap must.
pub(super) async fn release_idle_passive_slots(ctx: &ChannelPollCtx) {
    // ADR-101 Decision §E: read the watermark map BEFORE acquiring
    // `shared_channels` -- a stale read only ever defers release, never
    // wrongly permits it (the same ordering rationale `reap_expired_cyclic_
    // registrants` above already documents).
    let watermark = ctx
        .drain_watermarks
        .lock()
        .await
        .get(&ctx.channel_id)
        .copied();
    let mut chans = ctx.service.shared_channels.lock().await;
    if let Some(sc) = chans
        .values_mut()
        .find(|sc| sc.channel_id == ctx.channel_id)
    {
        sc.tp20_connections
            .retain(|_, entry| !tp20_passive_idle_release_due(entry, watermark));
    }
}

/// [`run_tp20_connection_request`]'s pre-insert availability-check decision
/// (this module's own doc comment; the `events_j1939_claim.rs`
/// `owned_by_a_live_sibling` shape it mirrors, extended here -- Codex review
/// finding, PR #97, 7th round -- to also cover the quarantine mechanism).
/// Renamed from `tp20_rx_id_owned_by_a_live_sibling`, whose name no longer
/// covered its own second rejection reason. Returns `true` (`rx_id`
/// unavailable to `cll_handle`, the CANDIDATE about to register -- not the
/// existing entry's own owner) for either of two reasons:
///
/// 1. `connections` already has an entry for `rx_id` still marked
///    `abandoned` -- quarantined by a still-outstanding local abandonment
///    (`run_tp20_connection_request`'s post-loop cleanup), regardless of
///    which `cll_handle` owns it (including `cll_handle` itself retrying its
///    own abandoned attempt) and regardless of whether that owner is still
///    live by check 2 below (the local-deadline-abandonment case this fix
///    targets is exactly a still-live owner retrying).
/// 2. `connections` already has an entry for `rx_id` under a DIFFERENT,
///    still-live `cll_handle` (its `connect_generation`, read via
///    `live_generation`, still current) -- a live sibling's own
///    still-PENDING request, never a resolved/established one (an
///    established attempt's entry has already been removed by the time any
///    later request could observe it, this module's own doc comment).
///
/// Pure -- unit-testable with a plain `HashMap`/closure, the same
/// extraction [`resolve_tp20_connection_indication`] below uses for its own
/// routing decision, independent of the live `logical_links`/
/// `SharedChannel` maps the real call site reads from.
pub(super) fn tp20_rx_id_unavailable_for(
    rx_id: u32,
    cll_handle: u32,
    connections: &HashMap<u32, Tp20ConnEntry>,
    live_generation: impl Fn(u32) -> Option<u64>,
) -> bool {
    match connections.get(&rx_id).copied() {
        Some(entry) if entry.abandoned => true,
        Some(entry) if entry.cll_handle != cll_handle => {
            live_generation(entry.cll_handle) == Some(entry.connect_generation)
        }
        _ => false,
    }
}

/// ADR-188 routing step, the structural sibling of
/// `events_j1939_claim.rs`'s own `resolve_j1939_claim_indication`: resolves
/// a `CONNECTION_ESTABLISHED`/`_LOST` indication's affected `rx_id` (from
/// `Data[0..3]`, clause 19.4.4 Table 81) to its owning `cll_handle`, via
/// `connections` (a snapshot of this physical channel's
/// `SharedChannel::tp20_connections`). Drops (`None`) when either no entry
/// exists for `rx_id`, or the entry's recorded `connect_generation` no
/// longer matches that CLL's CURRENT one -- a stale indication surviving
/// that CLL's disconnect/reconnect, dropped the same way every other
/// `connect_generation`-guarded site in this codebase is.
///
/// The real call site ([`deliver_tp20_connection_indication`]) only reaches
/// this function for an entry NOT already marked `abandoned` (Codex review
/// fix, PR #97, 8th round / Fix L): a quarantine release must never depend
/// on this function's own live-owner resolution, since the recorded owner
/// going stale (disconnect/reconnect) is itself one of the ways an entry
/// becomes `abandoned` in the first place -- see
/// [`tp20_connection_indication_is_for_an_abandoned_entry`]'s own doc
/// comment and [`deliver_tp20_connection_indication`]'s own doc comment for
/// the fixed control flow.
///
/// Pure -- unit-testable with a plain `HashMap`/closure, independent of the
/// live `logical_links`/`SharedChannel` maps this is extracted from at the
/// real call site.
pub(super) fn resolve_tp20_connection_indication(
    rx_id: u32,
    connections: &HashMap<u32, Tp20ConnEntry>,
    live_generation: impl Fn(u32) -> Option<u64>,
) -> Option<u32> {
    let entry = connections.get(&rx_id)?;
    (live_generation(entry.cll_handle) == Some(entry.connect_generation))
        .then_some(entry.cll_handle)
}

/// [`deliver_tp20_connection_indication`]'s own quarantine-release decision
/// (Codex review finding, PR #97, 7th round; this module's own doc comment
/// on the mechanism): `true` when `connections` still has an entry for
/// `rx_id` marked `abandoned` -- an indication belonging to a locally-
/// abandoned, quarantined attempt that nobody is waiting on anymore. The
/// real call site does NOT write this indication's `outcome` to
/// `tp20_connection_results` in that case (nothing reads it) and instead
/// removes the `tp20_connections` entry, releasing the quarantine for a
/// later retry.
///
/// Checked FIRST at the real call site (Codex review fix, PR #97, 8th
/// round / Fix L), before [`resolve_tp20_connection_indication`]'s own
/// live-owner resolution ever runs -- a quarantine release must not require
/// the entry's recorded owner CLL to still be live (a permanent-quarantine
/// bug the old ordering had: see [`deliver_tp20_connection_indication`]'s
/// own doc comment for the full failure mode this closes).
///
/// Pure -- unit-testable with a plain `HashMap`, the same extraction
/// [`tp20_rx_id_unavailable_for`] and [`resolve_tp20_connection_indication`]
/// above use for their own decisions.
pub(super) fn tp20_connection_indication_is_for_an_abandoned_entry(
    rx_id: u32,
    connections: &HashMap<u32, Tp20ConnEntry>,
) -> bool {
    connections.get(&rx_id).is_some_and(|entry| entry.abandoned)
}

/// [`deliver_tp20_connection_indication`]'s own one-shot swallow decision
/// (Fix AA, round 24; extracted round 25, PR #97): `true` when `rx_id`'s
/// current routing entry is armed with `expect_stale_lost` AND `outcome` is
/// a `Lost` indication -- the exact condition under which a prior TP2.0
/// occupant's own stale confirmation is swallowed instead of being
/// misattributed onto the entry's own, current owner. An `Established`
/// outcome is never a candidate, regardless of the flag (see the real call
/// site's own doc comment for why that can only mean the CURRENT entry's
/// own request succeeded).
///
/// Pure -- unit-testable with a plain `HashMap`, the same extraction
/// [`tp20_connection_indication_is_for_an_abandoned_entry`] above uses for
/// its own decision. Its own unit tests assert this predicate's output for
/// static, hand-constructed entry states (flag armed vs. already cleared),
/// which is the deterministic half of the one-shot claim; the other half --
/// that the real call site's own clearing statement
/// (`entry.expect_stale_lost = false`, right after this predicate returns
/// `true`) is what produces that already-cleared state on any actual second
/// delivery -- is established by reading that call site directly: it
/// re-acquires `shared_channels` and re-runs the SAME exact-entry equality
/// recheck (`sc.tp20_connections.get(&rx_id) == connections.get(&rx_id)`)
/// the abandoned-entry branch above it already uses for the identical
/// reason, before ever touching the flag -- so even a write that raced in
/// during the re-lock is guarded against, regardless of what else might be
/// concurrently delivering for this channel. Not established by a dynamic
/// swallow-then-redeliver simulation: an end-to-end `grpc_mock` attempt at
/// that turned out to be systematically won by the round-22 no-match
/// fallback instead (see the real call site's own doc comment), which is
/// why this predicate-level static check is what this codebase relies on
/// instead.
pub(super) fn tp20_connection_indication_is_a_swallowed_stale_lost(
    rx_id: u32,
    outcome: &Tp20ConnectionOutcome,
    connections: &HashMap<u32, Tp20ConnEntry>,
) -> bool {
    matches!(outcome, Tp20ConnectionOutcome::Lost(_))
        && connections.get(&rx_id).is_some_and(|e| e.expect_stale_lost)
}

/// Real call site for [`resolve_tp20_connection_indication`]: reads this
/// frame's physical channel's `SharedChannel::tp20_connections` and either
/// releases a quarantined `abandoned` entry, or resolves `rx_id` to its
/// owning (live, non-stale) CLL and records the outcome for
/// [`run_tp20_connection_request`]'s wait step to observe on its next poll
/// iteration.
///
/// The two cases are handled by two independent paths, checked in this
/// order (Codex review fix, PR #97, 8th round / Fix L -- see this module's
/// own doc comment on the quarantine mechanism):
///
/// 1. **Abandoned entry -> release.** If the snapshotted entry for `rx_id`
///    is already marked `abandoned`
///    ([`tp20_connection_indication_is_for_an_abandoned_entry`]), this
///    indication is this attempt's own delayed terminal outcome arriving --
///    act on it once the exact-entry recheck below confirms nothing else
///    has replaced it, WITHOUT ever calling
///    [`resolve_tp20_connection_indication`]. Acting on an abandoned entry
///    must never depend on the entry's recorded owner CLL still being
///    live: before this fix, this case was reached only via
///    [`resolve_tp20_connection_indication`]'s own live-owner resolution,
///    which returns `None` (and this function returned early, before ever
///    reaching the abandoned-entry check below it) the moment that owner's
///    `connect_generation` no longer matches -- which is nearly certain for
///    an abandoned entry, since two of its three sources (this module's own
///    "third abandonment path" doc comment) are the owner disconnecting or
///    reconnecting. That made the quarantine permanent: every later
///    indication for `rx_id` kept hitting the same early return, the entry
///    never released, and the RX-ID stayed blocked for every CLL (not just
///    the original owner) until the whole physical channel closed. A
///    `Lost` `outcome` (Codex review fix, PR #97, round 11) releases the
///    quarantine now (removes the entry) -- the device already confirms no
///    slot is occupied. An `Established` `outcome` does NOT release it:
///    instead it re-issues `IOCTL_TEARDOWN_CONNECTION` (via
///    [`best_effort_teardown_on_abandon`]) while leaving the entry in place,
///    still `abandoned` -- the abandonment-time teardown attempt most
///    likely raced ahead of the device's own slot allocation and was
///    rejected/no-op'd, so this delayed indication is the first point this
///    side can know for certain the native slot is genuinely occupied and
///    needs a fresh teardown call, and (Codex review fix, PR #97, round 15)
///    that follow-up teardown is ITSELF just as non-blocking, so releasing
///    the quarantine before issuing it would reopen the exact same
///    misattribution window Fix R (round 14) closed for the normal-
///    teardown paths -- the entry only actually releases once the
///    follow-up's own terminal indication arrives and re-enters this same
///    function (expected as `Lost`).
/// 2. **Not abandoned -> resolve owner and deliver.** Otherwise, fall
///    through to the pre-fix behavior: resolve `rx_id` to its owning
///    (live, non-stale) CLL via [`resolve_tp20_connection_indication`] and
///    record the outcome for it. A no-match (no pending request for this
///    `rx_id`, or a stale one) is dropped silently -- there is no per-CLL
///    owner to route to.
///
/// Public entry point: a thin wrapper around
/// [`deliver_tp20_connection_indication_once`] that implements ADR-190's
/// "Correction" paragraph under `### 4. Disarm / teardown`'s companion fix
/// (Codex review finding, P1, PR #99; design-advisor consult) -- see that
/// function's own doc comment for why the passive-delivery arm's exact-entry
/// recheck needs a retry at all.
pub(super) async fn deliver_tp20_connection_indication(
    ctx: &ChannelPollCtx,
    rx_id: u32,
    outcome: Tp20ConnectionOutcome,
) {
    if matches!(
        deliver_tp20_connection_indication_once(ctx, rx_id, outcome).await,
        DeliverTp20IndicationOutcome::RetryPassiveRecheckFailed
    ) {
        // A `Destroy`/`Disconnect` disarm holds `shared_channels` across its
        // whole sequence (this module's own outermost-lock discipline), so
        // if it raced in between the initial snapshot above and the passive
        // arm's own recheck, it has already fully completed -- including
        // marking the entry `abandoned` -- by the time this retry re-reads
        // state. The fresh pass therefore correctly falls into the
        // abandoned-entry branch above (re-teardown, marker-clear, hold)
        // instead of silently dropping the indication, which would
        // otherwise leave the passive-release sweep free to release an
        // RX-ID whose native connection may still be genuinely established.
        // At most one retry, per the ADR's own "one retry converges"
        // reasoning: a second race within the retry's own recheck window is
        // not chased further.
        deliver_tp20_connection_indication_once(ctx, rx_id, outcome).await;
    }
}

/// Outcome of one [`deliver_tp20_connection_indication_once`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliverTp20IndicationOutcome {
    /// This pass fully handled the indication (including "nothing to do" --
    /// no `SharedChannel` for this channel, no tracked entry for `rx_id`,
    /// etc.). The caller does not retry.
    Handled,
    /// The passive-delivery arm's own exact-entry recheck failed: a disarm
    /// raced in between the initial snapshot at the top of this function and
    /// that recheck. [`deliver_tp20_connection_indication`] retries once
    /// against a fresh snapshot instead of silently dropping the indication
    /// (ADR-190's "Correction" paragraph companion fix, Codex review
    /// finding, P1, PR #99).
    RetryPassiveRecheckFailed,
}

async fn deliver_tp20_connection_indication_once(
    ctx: &ChannelPollCtx,
    rx_id: u32,
    outcome: Tp20ConnectionOutcome,
) -> DeliverTp20IndicationOutcome {
    let connections = {
        let chans = ctx.service.shared_channels.lock().await;
        let Some(sc) = chans.values().find(|sc| sc.channel_id == ctx.channel_id) else {
            return DeliverTp20IndicationOutcome::Handled;
        };
        sc.tp20_connections.clone()
    };

    if tp20_connection_indication_is_for_an_abandoned_entry(rx_id, &connections) {
        let entry_cll_handle = connections.get(&rx_id).map(|e| e.cll_handle);
        // Codex review fix (PR #97, round 11, corrected round 15): a `Lost`
        // outcome needs no further action (the device already confirms no
        // slot is occupied) and releases the quarantine now. An
        // `Established` outcome means the abandonment-time `best_effort_
        // teardown_on_abandon` call (this attempt's own caller) most likely
        // raced ahead of the device's own slot allocation and was
        // rejected/no-op'd as a result -- the device has now confirmed the
        // slot IS genuinely occupied, so this issues a follow-up teardown.
        // That follow-up call is ITSELF just as non-blocking as any other
        // `IOCTL_TEARDOWN_CONNECTION` (Fix R's own finding, round 14): if
        // the quarantine were released here, right before issuing it, a
        // promptly-issued new `CoptStartcomm` proposing the SAME `rx_id`
        // could register before the follow-up's own delayed
        // `CONNECTION_LOST` confirmation drains, reproducing the identical
        // misattribution risk Fix R closed for the normal-teardown paths
        // (round 15 finding). So an `Established` outcome does NOT remove
        // the entry here -- it stays `abandoned`, still blocking any new
        // request against `rx_id`, until the follow-up teardown's own
        // terminal indication arrives and re-enters this same function
        // (almost certainly as `Lost`, releasing it then via the branch
        // below).
        let is_established = matches!(outcome, Tp20ConnectionOutcome::Established(_));
        let acted = {
            let mut chans = ctx.service.shared_channels.lock().await;
            match chans
                .values_mut()
                .find(|sc| sc.channel_id == ctx.channel_id)
            {
                // Exact-entry recheck (the same one case 2 below performs):
                // confirms this is still the SAME abandoned attempt
                // snapshotted above, not yet replaced/removed by anything
                // else, before acting on it -- independent of whether its
                // recorded owner CLL is still live.
                Some(sc) if sc.tp20_connections.get(&rx_id) == connections.get(&rx_id) => {
                    if is_established {
                        // ADR-190's "Correction" paragraph under `### 4.
                        // Disarm / teardown` (Codex review finding, P1, PR
                        // #99): this re-teardown arm keeps the entry (never
                        // removes it), so release must again wait for the
                        // follow-up `Lost` below, not the drain-watermark
                        // barrier -- clear any bounded release deadline
                        // stamped at the original disarm.
                        if let Some(entry) = sc.tp20_connections.get_mut(&rx_id) {
                            entry.idle_release_at = None;
                        }
                    } else {
                        sc.tp20_connections.remove(&rx_id);
                    }
                    true
                }
                _ => false,
            }
        };
        if acted && is_established {
            // Codex review finding investigated (P2, PR #97, round 19): the
            // suggested fix -- release the quarantine when this follow-up
            // teardown call itself fails synchronously, on the reasoning
            // that the establishment indication was already consumed so no
            // further indication is coming -- is UNSAFE at this specific
            // call site and was reverted after `cargo-runner` caught it
            // regressing `abandoned_entrys_established_outcome_stays_
            // quarantined_until_the_followup_teardown_confirms`
            // (`tests/grpc_mock/tp20.rs`). The reasoning doesn't hold here:
            // by the time this branch runs, `rx_id` already had ONE
            // best-effort teardown call issued for it, by whichever of
            // `run_tp20_connection_request`'s two internal abandonment paths
            // originally quarantined this entry -- so THIS follow-up call
            // failing is not evidence that no indication is coming; it is
            // routine evidence that the ORIGINAL call already tore the
            // native connection down (a real device's own confirmation for
            // THAT teardown can still be delayed/in flight, exactly the
            // race this whole re-teardown branch exists to handle). Result
            // discarded, same as `run_tp20_connection_request`'s own two
            // call sites and for the identical reason -- see
            // `best_effort_teardown_on_abandon`'s own doc comment.
            best_effort_teardown_on_abandon(
                ctx,
                entry_cll_handle.unwrap_or(0),
                rx_id,
                "abandoned request established late, after the original best-effort teardown call",
            )
            .await;
        }
        return DeliverTp20IndicationOutcome::Handled;
    }

    // Codex review finding via `edge-case-hunter`, design-advisor consult
    // (P1, PR #97, round 24): before resolving this indication to whichever
    // CLL CURRENTLY occupies `rx_id`'s routing entry, check whether that
    // entry itself is marked `expect_stale_lost` -- set by `run_tp20_
    // connection_request`'s own registration-time reconcile step when
    // registering THIS entry provably reconciled a DIFFERENT, prior
    // occupant's own stale `Established` belief for the SAME `rx_id` (see
    // that call site's own doc comment for the clause-19.3.3.2 proof this
    // rests on). A `Lost` outcome here is then that PRIOR occupant's own
    // still-in-flight confirmation, not this entry's own -- swallow it
    // (clear the flag, drop the frame without writing a result) instead of
    // misattributing the prior occupant's own old loss reason onto this
    // entry's own, unrelated `cll_handle`. Cleared, never the whole entry
    // removed: this entry's own request is very much still live and must
    // keep tracking normally. An `Established` outcome never consults this
    // flag -- it can only mean THIS entry's own request genuinely
    // succeeded, never the prior occupant's (a prior occupant reaching
    // `Established` again would itself be a live sibling clause 19.3.3.2's
    // native `ERR_NOT_UNIQUE` would already have rejected this
    // registration against).
    //
    // Codex review finding on this exact branch (P1, round 25, PR #97):
    // since Table 81 carries no per-attempt correlation token, could this
    // swallow a genuinely NEW `Lost` this entry's OWN request produced,
    // mistaking it for the prior occupant's stale one? Confirmed safe on a
    // conformant device by design-advisor consult (round 25): J2534-1
    // clause 7.2.5 requires a channel's indications to be read back in the
    // order their underlying events occurred, clauses 19.3.3.2/19.3.3.3
    // place TP2.0 connection indications in that same queue, and the prior
    // occupant's own stale `Lost` can only have been queued (Table 81)
    // strictly BEFORE this registration's own native call could produce
    // any outcome of its
    // own (the same clause-19.3.3.2 ordering proof this whole mechanism
    // already rests on) -- so the first `Lost` this flag ever swallows is
    // provably the stale one, and at most one can be outstanding per
    // registration. See ADR-188's Fix AA bullet (Consequences) for the full
    // argument and its accepted residual under a nonconformant device. The
    // one-shot swallow property itself (a second, later `Lost` is never
    // swallowed a second time) is proven deterministically by
    // [`tp20_connection_indication_is_a_swallowed_stale_lost`]'s own unit
    // tests below, rather than by any `grpc_mock` integration test -- an
    // end-to-end attempt to race this specific branch against the round-22
    // no-match fallback (`reconcile_established_tp20_loss`) turned out to
    // be systematically won by the fallback (the mock's own RX-poll task
    // reliably drains a queued stale frame before a client-driven gRPC
    // registration can dispatch), so an integration test can only ever
    // prove the OBSERVABLE end-to-end invariant (the new occupant never
    // surfaces the prior occupant's stale reason), not that this specific
    // branch executed -- see `stale_lost_swallow_is_one_shot_and_never_
    // eats_the_new_occupants_own_genuine_loss` (`tests/grpc_mock/tp20.rs`)
    // for that end-to-end proof and its own doc comment for why it cannot
    // pin the internal path.
    if tp20_connection_indication_is_a_swallowed_stale_lost(rx_id, &outcome, &connections) {
        let mut chans = ctx.service.shared_channels.lock().await;
        if let Some(sc) = chans
            .values_mut()
            .find(|sc| sc.channel_id == ctx.channel_id)
        {
            // Exact-entry recheck (the same pattern case 1's own `acted`
            // check above performs): confirms this is still the SAME entry
            // snapshotted at the top of this function before clearing its
            // flag.
            if sc.tp20_connections.get(&rx_id) == connections.get(&rx_id)
                && let Some(entry) = sc.tp20_connections.get_mut(&rx_id)
            {
                entry.expect_stale_lost = false;
            }
        }
        return DeliverTp20IndicationOutcome::Handled;
    }

    // ADR-190/Phase 7 Stage 7b section 3: a `passive` entry's own establish/
    // loss indication is delivered by directly mutating the owning CLL's
    // `LogicalLinkState::tp20_connection` in place (the same no-live-wait
    // direct-write shape `reconcile_live_established_cll` already uses,
    // since no wait loop reads a passive entry's own outcome the way
    // `run_tp20_connection_request` does for an active one) -- NEVER
    // written into `tp20_connection_results`. Checked here, after the
    // abandoned-entry and stale-swallow checks above, which apply unchanged
    // to a passive entry too: a quarantined passive entry (post-disarm)
    // drains through that same existing machinery, not this arm.
    if let Some(entry) = connections.get(&rx_id).copied()
        && entry.passive
    {
        let owner_cll_handle = entry.cll_handle;
        let owner_connect_generation = entry.connect_generation;

        // Exact-entry recheck (the same pattern every other branch above
        // performs): confirms this is still the SAME entry snapshotted at
        // the top of this function, not yet replaced/disarmed by anything
        // else, before acting on it.
        //
        // ADR-190's "Correction" paragraph companion fix (Codex review
        // finding, P1, PR #99; design-advisor consult): a mismatch here used
        // to be silently dropped -- a latent gap the new bounded passive
        // release (`Tp20ConnEntry::idle_release_at`) makes dangerous rather
        // than merely theoretical. A `Destroy`/`Disconnect` disarm (which
        // holds `shared_channels` across its whole sequence) can land
        // between the initial snapshot at the top of this function and this
        // recheck; a dropped `Established` here would mean the abandoned-
        // branch's own marker-clear (see the `is_established` arm above)
        // never runs, and the release sweep could then reap an RX-ID whose
        // native connection is genuinely established. Fixed by signalling
        // the caller to retry once against a fresh snapshot instead of
        // dropping -- the fresh pass correctly routes into the
        // abandoned-entry branch above (re-teardown, marker-clear, hold).
        //
        // Lock discipline (Codex review finding, P1, PR #99, round 2): the
        // recheck above used to acquire and release `shared_channels`
        // BEFORE acquiring `logical_links` for the write below, leaving a
        // window between the two acquisitions. `handle_stop_comm`'s and
        // `rpc_link.rs`'s own disarm sequences acquire `shared_channels`
        // FIRST and hold it across their own `logical_links` read (which
        // decides `was_established`) -- so a disarm landing in that window
        // could observe this connection as still `Listening` (this
        // function's own write hadn't happened yet), skip
        // `TEARDOWN_CONNECTION`, `take()` the CLL's `tp20_connection`, and
        // schedule a bounded release, all before this function's own
        // `logical_links` write ran. That write would then silently no-op
        // (`link.tp20_connection` already `None`), leaving `tp20_
        // connections` scheduled for release while the native connection is
        // genuinely established with nothing tracking it. Fixed by
        // acquiring `shared_channels` FIRST here too and holding it across
        // both the recheck and the `logical_links` write below, mirroring
        // every disarm site's own outermost-lock discipline -- the two
        // flows are now mutually exclusive via the same mutex, so a disarm
        // can only ever run either fully before this recheck (in which case
        // `still_current` already catches it and this function retries
        // against a fresh snapshot) or fully after this whole passive arm
        // releases `chans` below, never interleaved in between.
        let mut chans = ctx.service.shared_channels.lock().await;
        let still_current = chans
            .values()
            .find(|sc| sc.channel_id == ctx.channel_id)
            .is_some_and(|sc| sc.tp20_connections.get(&rx_id) == connections.get(&rx_id));
        if !still_current {
            drop(chans);
            return DeliverTp20IndicationOutcome::RetryPassiveRecheckFailed;
        }

        // Re-verify owner CLL liveness + `connect_generation` match (the
        // same recheck pattern the normal-resolve branch below already
        // does via `resolve_tp20_connection_indication`).
        let mut repeat_slot_stop_target = None;
        {
            let mut links = ctx.logical_links.lock().await;
            if let Some(link) = links.get_mut(&owner_cll_handle).filter(|l| {
                l.channel_id == Some(ctx.channel_id)
                    && l.connect_generation == owner_connect_generation
            }) {
                match outcome {
                    Tp20ConnectionOutcome::Established(tx_id) => {
                        if let Some(conn) = link.tp20_connection.as_mut() {
                            conn.phase = Tp20ConnectionPhase::Established;
                            conn.established_tx_id = Some(tx_id);
                        }
                    }
                    Tp20ConnectionOutcome::Lost(_reason) => {
                        // Re-enters `Listening`, NOT a terminal `Lost`
                        // (`Tp20ConnectionPhase::Listening`'s own doc
                        // comment): per clause 19.3.3.1, with both
                        // `SET_CONFIG` params still valid and a slot free,
                        // the device keeps auto-accepting the NEXT inbound
                        // connection too, and the persistent
                        // `Tp20ConnEntry` (kept registered, never removed
                        // for a passive entry) is what lets that next
                        // indication route correctly back here.
                        if let Some(conn) = link.tp20_connection.as_mut() {
                            // Codex review finding (P2, PR #99, round 2): a
                            // repeat-message slot started while this
                            // passive connection was `Established` still
                            // carries the now-stale peer TX-ID baked into
                            // its payload -- unlike an ordinary
                            // ComPrimitive, `PDU_IOCTL_STOP_REPEAT_MESSAGE`
                            // is not tied to `comm_started`, so it would
                            // otherwise keep autonomously retransmitting
                            // that stale TX-ID both while idle and after a
                            // new peer establishes with a different one.
                            // Mirrors `reconcile_live_established_cll`'s
                            // own stop-on-spontaneous-loss handling for an
                            // active TP2.0 connection.
                            if conn.phase == Tp20ConnectionPhase::Established {
                                repeat_slot_stop_target =
                                    Some((owner_cll_handle, owner_connect_generation));
                            }
                            conn.phase = Tp20ConnectionPhase::Listening;
                            conn.established_tx_id = None;
                        }
                    }
                }
            }
        }
        if let Some((cll_handle, connect_generation)) = repeat_slot_stop_target {
            let failed = stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
            push_leaked_repeat_slots(&mut chans, ctx.channel_id, failed);
        }
        drop(chans);
        return DeliverTp20IndicationOutcome::Handled;
    }

    let cll_handle = {
        let links = ctx.logical_links.lock().await;
        resolve_tp20_connection_indication(rx_id, &connections, |h| {
            links.get(&h).map(|l| l.connect_generation)
        })
    };
    let Some(cll_handle) = cll_handle else {
        // Codex review finding (P1, PR #97, round 22; design-advisor
        // consult): a `Lost` outcome with no pending/tracked request to
        // resolve against is not necessarily spurious. Once a connection
        // reaches `Established`, `run_tp20_connection_request`'s own
        // post-loop cleanup already removes its `tp20_connections` routing
        // entry (the "resolves either way" cleanup) -- so a LATER,
        // spontaneous device-side loss has nothing left to resolve against
        // here and would otherwise be dropped silently, exactly the gap
        // ADR-188 already documents as an accepted residual ("no ongoing
        // monitoring for a later spontaneous connection loss once
        // established"). Left unreconciled, the owning CLL's own
        // `LogicalLinkState::tp20_connection.phase` stays `Established`
        // forever: a later `CoptStopcomm`/`Disconnect`/`Destroy` believes
        // the connection is still live, issues a doomed native teardown,
        // gets a synchronous failure, and (since round 21) quarantines
        // `rx_id` unconditionally -- but the `Lost` confirmation for this
        // exact connection already arrived right here, so nothing will
        // ever release that quarantine: a permanent lock-out. An
        // `Established` outcome with no match is still dropped silently,
        // unchanged -- see [`reconcile_established_tp20_loss`]'s own doc
        // comment for why only `Lost` needs reconciliation.
        if matches!(outcome, Tp20ConnectionOutcome::Lost(_)) {
            reconcile_established_tp20_loss(ctx, rx_id).await;
        }
        return DeliverTp20IndicationOutcome::Handled;
    };

    let mut chans = ctx.service.shared_channels.lock().await;
    if let Some(sc) = chans
        .values_mut()
        .find(|sc| sc.channel_id == ctx.channel_id)
    {
        // Re-verify this exact attempt is STILL registered before writing an
        // outcome back -- the same edge-case-hunter-established recheck
        // `deliver_j1939_claim_indication` performs, guarding against a
        // disconnect/destroy teardown racing in between the snapshot above
        // and this write. Also re-verifies this entry hasn't become
        // `abandoned` since the snapshot above (in which case case 1 above
        // will handle it on this indication's own retry/next delivery, not
        // here).
        if sc.tp20_connections.get(&rx_id) != connections.get(&rx_id) {
            return DeliverTp20IndicationOutcome::Handled;
        }
        sc.tp20_connection_results.insert(cll_handle, outcome);
    }
    DeliverTp20IndicationOutcome::Handled
}

/// Codex review finding (P1, PR #97, round 22; design-advisor consult;
/// race corrected round 23): closes the "no ongoing monitoring for a later
/// spontaneous connection loss once established" gap ADR-188 already
/// accepts as a residual, just enough to stop it from producing an
/// unreleasable quarantine. Called only from
/// [`deliver_tp20_connection_indication`]'s own no-match fall-through, only
/// for a `Lost` outcome (an unmatched `Established` outcome has nothing
/// useful to reconcile and stays dropped, unchanged).
///
/// Scans every live CLL on this physical channel for one whose own
/// `LogicalLinkState::tp20_connection` is `Some(Tp20Connection { phase:
/// Established, requested_rx_id, .. })` matching `rx_id`, and flips its
/// `phase` to `Lost` in place -- keeping `requested_rx_id`/
/// `established_tx_id` untouched, mirroring the `Lost`-phase record
/// `handle_start_comm`'s own `CoptStartcomm` outcome write-back already
/// produces for the OTHER way a connection can end up `Lost`. At most one
/// live CLL can genuinely hold this: SAE J2534-2 clause 19.3.3.2's native
/// `ERR_NOT_UNIQUE` rejects a second request against an already-established
/// `rx_id` before it can ever reach `Established` on a second CLL (Fix N-1,
/// round 11) -- scanning ALL matches instead of stopping at the first is
/// defensive, not a real multi-match case: any additional "match" found
/// here would itself already be a stale belief equally worth correcting.
///
/// Every downstream consumer already gates on `phase == Established`, so
/// this flip propagates for free: `tx_header::build_tx_message`/
/// `response_header_bytes`'s TP2.0 arms re-resolve the "requires an
/// established connection" error on this CLL's next send or repeat-slot
/// setup; `build_cll_rx_entries` (`events_rx_routing.rs`) falls back to the
/// unmatchable sentinel entry; `rpc_misc.rs::ioctl_start_repeat_message`'s
/// own precondition
/// blocks a NEW repeat slot. The one exception -- an ALREADY-running repeat
/// slot, whose native `IOCTL_START_REPEAT_MESSAGE` keeps autonomously
/// retransmitting on the device regardless of this CLL's own local `phase`
/// bookkeeping -- is NOT covered by that filtering alone, so this function
/// stops it explicitly, the same best-effort way `handle_stop_comm`'s own
/// normal teardown already does for a deliberate StopComm.
///
/// Deliberately fires no new client-visible event: no existing
/// `PduErrEvt*` fits a per-connection loss (`PduErrEvtLostCommToVci` is
/// VCI-level, not per-CLL) -- the next `CoptSendrecv`/repeat-slot-start on
/// this CLL surfaces the existing "no established connection" error
/// naturally. A narrower, documented residual than full proactive
/// monitoring (ADR-188 Consequences): this reconciles passively, only when
/// the device's own unsolicited indication happens to arrive, never polls
/// for one.
///
/// **Lock discipline (Codex review finding, P1, PR #97, round 23):** the
/// original round-22 shape scanned/flipped under a `logical_links`-only
/// critical section, released before calling `stop_repeat_slots_for_cll`,
/// with no `shared_channels` hold spanning any of it. That left a window
/// for a concurrent `CoptStopcomm`/`Disconnect`/`Destroy` to race in
/// between this function's own scan and flip, "win" by taking the SAME
/// CLL's `tp20_connection` first (still `Established` at that point), and
/// (per round 21's unconditional quarantine) insert a fresh `abandoned`
/// entry for `rx_id` into `SharedChannel::tp20_connections` -- one only
/// THIS `Lost` indication could ever have released, since it is the
/// device's one-and-only unsolicited confirmation of the very loss that
/// teardown's own (necessarily failing) native call just rediscovered.
/// Fixed two ways together: (1) `shared_channels` is now acquired FIRST and
/// held for this function's ENTIRE duration, mirroring `handle_stop_comm`'s/
/// `DestroyComLogicalLink`'s/`DisconnectComLogicalLink`'s own identical
/// outermost-lock discipline for their own teardown+quarantine sequences --
/// the two flows are now mutually exclusive via the same mutex, and the
/// scan-then-flip itself is now one unbroken `logical_links` critical
/// section (no separate re-check step). (2) Serialization alone does not
/// tell either side what the OTHER one already did while waiting for the
/// lock, so when this scan finds NO live match, it additionally checks
/// whether `tp20_connections` now holds an `abandoned` entry for `rx_id` --
/// if the racing teardown "won" the ordering, its own quarantine-insert
/// already landed by the time this scan ran, and releasing it here is the
/// only way it can ever self-heal. No `cll_handle`/`connect_generation`
/// match check is needed before releasing it: clause 19.3.3.2's own RX-ID
/// uniqueness rule means at most one entry can exist for `rx_id` at a time
/// regardless of cause, and a NEW, unrelated `CoptStartcomm` racing onto
/// the SAME `rx_id` on some OTHER CLL could not have inserted its own
/// (non-`abandoned`, pending) entry during this hold either -- `run_tp20_
/// connection_request`'s own pre-insert check needs this SAME
/// `shared_channels` lock, so it would simply block until this function
/// releases it; only an `abandoned` entry directly caused by this exact
/// race can appear here, and only that kind is ever removed.
async fn reconcile_established_tp20_loss(ctx: &ChannelPollCtx, rx_id: u32) {
    let mut chans = ctx.service.shared_channels.lock().await;
    let matches = reconcile_live_established_cll(ctx, &mut chans, rx_id).await;

    if matches.is_empty()
        && let Some(sc) = chans
            .values_mut()
            .find(|sc| sc.channel_id == ctx.channel_id)
        && sc.tp20_connections.get(&rx_id).is_some_and(|e| e.abandoned)
    {
        sc.tp20_connections.remove(&rx_id);
    }
}

/// Shared core of "does any live CLL on this physical channel still believe
/// `rx_id` is `Established`, and if so, reconcile it to `Lost`" -- factored
/// out (Codex review finding via `edge-case-hunter`, design-advisor
/// consult, P1, PR #97, round 24) so both [`reconcile_established_tp20_loss`]
/// (round 22/23, called from `deliver_tp20_connection_indication`'s own
/// no-match fall-through for an unmatched `Lost` indication) and
/// `run_tp20_connection_request`'s own registration-time reconcile (round
/// 24 -- see that call site's own doc comment for why a SUCCESSFUL native
/// `IOCTL_REQUEST_CONNECTION` for `rx_id` is itself proof, per clause
/// 19.3.3.2, that any sibling's `Established` belief on the SAME `rx_id` is
/// already stale) can reuse the identical scan-flip-and-stop-repeat-slots
/// logic instead of diverging.
///
/// Requires the caller to ALREADY hold `ctx.service.shared_channels`
/// (`chans`) -- never acquires it itself, mirroring `push_leaked_repeat_
/// slots`'s own documented discipline, since both call sites need this
/// step to happen atomically with their own surrounding `tp20_connections`
/// bookkeeping under the SAME lock hold (round 23's own lock-ordering
/// lesson: releasing `shared_channels` between finding a match and acting
/// on the surrounding map state reopens exactly the race that round fixed).
///
/// Returns the `(cll_handle, connect_generation)` pairs actually flipped,
/// for a caller that needs to know whether anything happened.
async fn reconcile_live_established_cll(
    ctx: &ChannelPollCtx,
    chans: &mut HashMap<ChannelKey, SharedChannel>,
    rx_id: u32,
) -> Vec<(u32, u64)> {
    let matches: Vec<(u32, u64)> = {
        let mut links = ctx.logical_links.lock().await;
        let mut found = Vec::new();
        for (&cll_handle, link) in links.iter_mut() {
            if link.channel_id != Some(ctx.channel_id) {
                continue;
            }
            // ADR-190/Phase 7 Stage 7b defense-in-depth (edge-case-hunter
            // finding): `!c.passive` is excluded here explicitly, not left
            // implicit. Both current call sites happen to prevent this
            // function from ever observing a live passive connection's own
            // `Established` state today -- `run_tp20_connection_request`'s
            // registration-time reconcile is gated behind `tp20_rx_id_
            // unavailable_for`, which already treats a live, non-abandoned
            // passive entry as unavailable (rejecting the registration
            // before this function could ever run against it), and
            // `reconcile_established_tp20_loss`'s own no-match fallback is
            // only reachable when NO `tp20_connections` entry exists for
            // `rx_id`, which cannot happen while a passive slot stays armed
            // (its own entry is deliberately never removed on resolve,
            // ADR-190 section 3). But this function is shared by both
            // callers with no guarantee either one's own gating stays
            // correct across a future change, and flipping a live passive
            // connection straight to a TERMINAL `Lost` here (instead of
            // ADR-190's own required `Listening` re-entry, `deliver_tp20_
            // connection_indication`'s own passive arm) would silently
            // strand it -- exactly the class of stranding bug ADR-188's own
            // 25-round history warns about. Guarded here directly rather
            // than relying solely on upstream callers staying correct.
            let is_match = link.tp20_connection.is_some_and(|c| {
                !c.passive
                    && c.phase == Tp20ConnectionPhase::Established
                    && c.requested_rx_id == rx_id
            });
            if !is_match {
                continue;
            }
            if let Some(conn) = link.tp20_connection.as_mut() {
                conn.phase = Tp20ConnectionPhase::Lost;
            }
            found.push((cll_handle, link.connect_generation));
        }
        found
    };

    for &(cll_handle, connect_generation) in &matches {
        warn!(
            cll_handle,
            rx_id,
            "TP2.0 connection spontaneously lost by the device after establishment -- \
             reconciled this CLL's own tp20_connection phase to Lost from an unsolicited \
             CONNECTION_LOST indication (ADR-188 Consequences: no proactive monitoring, \
             passive reconciliation only)"
        );
        let failed = stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
        push_leaked_repeat_slots(chans, ctx.channel_id, failed);
    }

    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fix 1 regression (concurrency-bug fix, `run_tp20_connection_
    /// request`'s pre-insert availability check): a candidate CLL proposing
    /// an `rx_id` already registered to a DIFFERENT, still-live `cll_handle`
    /// (matching `connect_generation`) must be rejected.
    ///
    /// This exercises [`tp20_rx_id_unavailable_for`] directly rather than
    /// going through a full gRPC round trip in `tests/grpc_mock/tp20.rs`:
    /// this codebase's single poll-task-per-physical-channel architecture
    /// (`events::spawn_channel_poll_task`) serializes every queued
    /// `TxItem::StartComm` on a channel to completion before the next one is
    /// dequeued, so two sibling CLLs can never have overlapping still-pending
    /// `run_tp20_connection_request` calls via the ordinary client-driven RPC
    /// path this crate's `grpc_mock` test harness drives -- this module's own
    /// doc comment and the Prioritized Backlog
    /// record why an end-to-end regression test cannot exercise this decision
    /// today. This unit test is the actual proof of the decision's own
    /// correctness, independent of that reachability question.
    #[test]
    fn rejects_a_rx_id_owned_by_a_different_live_cll() {
        let mut connections = HashMap::new();
        connections.insert(
            0x0321,
            Tp20ConnEntry {
                cll_handle: 1,
                connect_generation: 5,
                abandoned: false,
                expect_stale_lost: false,
                passive: false,
                idle_release_at: None,
            },
        );
        // The candidate is CLL 2, proposing the SAME rx_id CLL 1 already
        // has a pending request registered for; CLL 1 is still live (its
        // `connect_generation` still matches).
        assert!(tp20_rx_id_unavailable_for(0x0321, 2, &connections, |h| {
            (h == 1).then_some(5)
        }));
    }

    /// The SAME candidate CLL re-proposing its OWN already-registered
    /// `rx_id` (e.g. a retry within its own attempt) must never be treated
    /// as a sibling collision against itself -- as long as that entry is not
    /// itself `abandoned` (see `rejects_a_same_cll_retry_against_its_own_
    /// abandoned_entry` below for that case).
    #[test]
    fn does_not_reject_the_same_cll_reproposing_its_own_rx_id() {
        let mut connections = HashMap::new();
        connections.insert(
            0x0321,
            Tp20ConnEntry {
                cll_handle: 1,
                connect_generation: 5,
                abandoned: false,
                expect_stale_lost: false,
                passive: false,
                idle_release_at: None,
            },
        );
        assert!(!tp20_rx_id_unavailable_for(0x0321, 1, &connections, |h| {
            (h == 1).then_some(5)
        }));
    }

    /// A registered entry whose owning CLL is no longer live (disconnected/
    /// reconnected, i.e. `live_generation` no longer returns its recorded
    /// `connect_generation` -- or the CLL is gone entirely, `None`) is
    /// stale and must not block a new candidate from claiming the same
    /// `rx_id` -- as long as that entry is not itself `abandoned`.
    #[test]
    fn does_not_reject_when_the_registered_owner_is_stale() {
        let mut connections = HashMap::new();
        connections.insert(
            0x0321,
            Tp20ConnEntry {
                cll_handle: 1,
                connect_generation: 5,
                abandoned: false,
                expect_stale_lost: false,
                passive: false,
                idle_release_at: None,
            },
        );
        // `connect_generation` moved on (CLL 1 disconnected/reconnected).
        assert!(!tp20_rx_id_unavailable_for(0x0321, 2, &connections, |h| {
            (h == 1).then_some(6)
        }));
        // CLL 1 is gone entirely.
        assert!(!tp20_rx_id_unavailable_for(0x0321, 2, &connections, |_| {
            None
        }));
    }

    /// No registered entry for `rx_id` at all -- the ordinary, unclaimed
    /// case.
    #[test]
    fn does_not_reject_an_unregistered_rx_id() {
        let connections: HashMap<u32, Tp20ConnEntry> = HashMap::new();
        assert!(!tp20_rx_id_unavailable_for(0x0321, 2, &connections, |_| {
            None
        }));
    }

    /// Quarantine regression (Codex review finding, PR #97, 7th round): a
    /// DIFFERENT sibling CLL proposing an `rx_id` whose entry is
    /// `abandoned` must be rejected, even though that entry's owner
    /// (`live_generation` still returning its recorded `connect_generation`)
    /// would otherwise look just like any other live, non-abandoned entry
    /// under check 2's own generation-match test.
    #[test]
    fn rejects_a_different_cll_proposing_a_still_quarantined_rx_id() {
        let mut connections = HashMap::new();
        connections.insert(
            0x0321,
            Tp20ConnEntry {
                cll_handle: 1,
                connect_generation: 5,
                abandoned: true,
                expect_stale_lost: false,
                passive: false,
                idle_release_at: None,
            },
        );
        assert!(tp20_rx_id_unavailable_for(0x0321, 2, &connections, |h| {
            (h == 1).then_some(5)
        }));
    }

    /// Quarantine regression (Codex review finding, PR #97, 7th round): the
    /// SAME `cll_handle` retrying its own abandoned attempt must also be
    /// rejected -- this is the actual bug scenario (an immediate same-`rx_
    /// id` retry after local-deadline abandonment must not be treated as
    /// "reproposing its own rx_id" the way `does_not_reject_the_same_cll_
    /// reproposing_its_own_rx_id` above proves for a NON-abandoned entry).
    #[test]
    fn rejects_a_same_cll_retry_against_its_own_abandoned_entry() {
        let mut connections = HashMap::new();
        connections.insert(
            0x0321,
            Tp20ConnEntry {
                cll_handle: 1,
                connect_generation: 5,
                abandoned: true,
                expect_stale_lost: false,
                passive: false,
                idle_release_at: None,
            },
        );
        assert!(tp20_rx_id_unavailable_for(0x0321, 1, &connections, |h| {
            (h == 1).then_some(5)
        }));
    }

    /// Quarantine release regression (Codex review finding, PR #97, 7th
    /// round): [`deliver_tp20_connection_indication`]'s own pure decision
    /// seam, [`tp20_connection_indication_is_for_an_abandoned_entry`],
    /// correctly identifies a quarantined entry so the real call site clears
    /// it (without writing a `tp20_connection_results` entry nobody is
    /// waiting on) rather than routing the indication normally.
    #[test]
    fn indication_for_an_abandoned_entry_is_identified_for_quarantine_release() {
        let mut connections = HashMap::new();
        connections.insert(
            0x0321,
            Tp20ConnEntry {
                cll_handle: 1,
                connect_generation: 5,
                abandoned: true,
                expect_stale_lost: false,
                passive: false,
                idle_release_at: None,
            },
        );
        assert!(tp20_connection_indication_is_for_an_abandoned_entry(
            0x0321,
            &connections
        ));
    }

    /// The ordinary (non-abandoned) case must NOT be misidentified as a
    /// quarantine release -- the normal routing path (write to
    /// `tp20_connection_results`) must still run for it.
    #[test]
    fn indication_for_a_live_entry_is_not_identified_for_quarantine_release() {
        let mut connections = HashMap::new();
        connections.insert(
            0x0321,
            Tp20ConnEntry {
                cll_handle: 1,
                connect_generation: 5,
                abandoned: false,
                expect_stale_lost: false,
                passive: false,
                idle_release_at: None,
            },
        );
        assert!(!tp20_connection_indication_is_for_an_abandoned_entry(
            0x0321,
            &connections
        ));
        // No entry at all for this rx_id.
        let empty: HashMap<u32, Tp20ConnEntry> = HashMap::new();
        assert!(!tp20_connection_indication_is_for_an_abandoned_entry(
            0x0321, &empty
        ));
    }

    /// The core Fix AA / round-25 claim: a `Lost` indication IS swallowed
    /// exactly when the current entry is armed with `expect_stale_lost`.
    #[test]
    fn lost_outcome_is_swallowed_when_flag_is_armed() {
        let mut connections = HashMap::new();
        connections.insert(
            0x0321,
            Tp20ConnEntry {
                cll_handle: 2,
                connect_generation: 1,
                abandoned: false,
                expect_stale_lost: true,
                passive: false,
                idle_release_at: None,
            },
        );
        assert!(tp20_connection_indication_is_a_swallowed_stale_lost(
            0x0321,
            &Tp20ConnectionOutcome::Lost(0),
            &connections
        ));
    }

    /// The one-shot property Codex's round-25 finding asked about: once the
    /// real call site clears the flag (simulated here by an entry that
    /// already has it cleared, exactly the post-swallow state), a SECOND
    /// `Lost` for the SAME entry -- even carrying a completely different
    /// reason byte, this entry's own genuine one -- is never swallowed
    /// again. This is the property an end-to-end `grpc_mock` test cannot
    /// deterministically pin (see this predicate's own doc comment).
    #[test]
    fn a_second_lost_is_not_swallowed_once_the_flag_is_cleared() {
        let mut connections = HashMap::new();
        connections.insert(
            0x0321,
            Tp20ConnEntry {
                cll_handle: 2,
                connect_generation: 1,
                abandoned: false,
                expect_stale_lost: false,
                passive: false,
                idle_release_at: None,
            },
        );
        assert!(!tp20_connection_indication_is_a_swallowed_stale_lost(
            0x0321,
            &Tp20ConnectionOutcome::Lost(0xD8),
            &connections
        ));
    }

    /// An `Established` outcome is never a swallow candidate, even with the
    /// flag armed -- it can only mean the CURRENT entry's own request
    /// genuinely succeeded (see the real call site's own doc comment for
    /// the clause-19.3.3.2 proof).
    #[test]
    fn established_outcome_is_never_swallowed_even_if_flag_is_armed() {
        let mut connections = HashMap::new();
        connections.insert(
            0x0321,
            Tp20ConnEntry {
                cll_handle: 2,
                connect_generation: 1,
                abandoned: false,
                expect_stale_lost: true,
                passive: false,
                idle_release_at: None,
            },
        );
        assert!(!tp20_connection_indication_is_a_swallowed_stale_lost(
            0x0321,
            &Tp20ConnectionOutcome::Established(0x1000_0321),
            &connections
        ));
    }

    /// No entry at all for `rx_id` is never a swallow candidate.
    #[test]
    fn lost_outcome_with_no_entry_is_not_swallowed() {
        let empty: HashMap<u32, Tp20ConnEntry> = HashMap::new();
        assert!(!tp20_connection_indication_is_a_swallowed_stale_lost(
            0x0321,
            &Tp20ConnectionOutcome::Lost(0),
            &empty
        ));
    }

    /// Codex review regression (PR #97, Fix C): all five `PARAM_TP20_*`
    /// ComParams staged at valid, in-range values resolve successfully, with
    /// every field carried through unchanged.
    fn valid_params() -> ComParamSet {
        let mut params = ComParamSet::default();
        params
            .unum32
            .insert(PARAM_TP20_CHANNEL_SETUP_CAN_ID, 0x0000_0700);
        params.unum32.insert(PARAM_TP20_DESTINATION_ADDRESS, 0x10);
        params.unum32.insert(PARAM_TP20_TX_ID_PROPOSAL, 0x0300);
        params.unum32.insert(PARAM_TP20_RX_ID_PROPOSAL, 0x0321);
        params.unum32.insert(PARAM_TP20_APPLICATION_TYPE, 1);
        params
    }

    #[test]
    fn resolve_tp20_connection_params_round_trips_all_valid_values() {
        let resolved = resolve_tp20_connection_params(&valid_params())
            .expect("all five valid ComParams should resolve");
        assert_eq!(resolved.setup_can_id, 0x0000_0700);
        assert_eq!(resolved.destination_address, 0x10);
        assert_eq!(resolved.tx_id_proposal, 0x0300);
        assert_eq!(resolved.rx_id_proposal, 0x0321);
        assert_eq!(resolved.application_type, 1);
    }

    #[test]
    fn resolve_tp20_connection_params_accepts_destination_address_at_u8_max() {
        let mut params = valid_params();
        params
            .unum32
            .insert(PARAM_TP20_DESTINATION_ADDRESS, u32::from(u8::MAX));
        let resolved = resolve_tp20_connection_params(&params)
            .expect("u8::MAX should be in range for the native 1-byte field");
        assert_eq!(resolved.destination_address, u8::MAX);
    }

    #[test]
    fn resolve_tp20_connection_params_rejects_destination_address_one_above_u8_max() {
        let mut params = valid_params();
        params
            .unum32
            .insert(PARAM_TP20_DESTINATION_ADDRESS, u32::from(u8::MAX) + 1);
        let err = resolve_tp20_connection_params(&params)
            .expect_err("one above u8::MAX must be rejected, not silently truncated");
        assert!(err.contains("CP_TP20DestinationAddress"), "{err}");
    }

    #[test]
    fn resolve_tp20_connection_params_accepts_application_type_at_u8_max() {
        let mut params = valid_params();
        params
            .unum32
            .insert(PARAM_TP20_APPLICATION_TYPE, u32::from(u8::MAX));
        let resolved = resolve_tp20_connection_params(&params)
            .expect("u8::MAX should be in range for the native 1-byte field");
        assert_eq!(resolved.application_type, u8::MAX);
    }

    #[test]
    fn resolve_tp20_connection_params_rejects_application_type_one_above_u8_max() {
        let mut params = valid_params();
        params
            .unum32
            .insert(PARAM_TP20_APPLICATION_TYPE, u32::from(u8::MAX) + 1);
        let err = resolve_tp20_connection_params(&params)
            .expect_err("one above u8::MAX must be rejected, not silently truncated");
        assert!(err.contains("CP_TP20ApplicationType"), "{err}");
    }

    #[test]
    fn resolve_tp20_connection_params_accepts_tx_id_proposal_at_u16_max() {
        let mut params = valid_params();
        params
            .unum32
            .insert(PARAM_TP20_TX_ID_PROPOSAL, u32::from(u16::MAX));
        let resolved = resolve_tp20_connection_params(&params)
            .expect("u16::MAX should be in range for the native 2-byte field");
        assert_eq!(resolved.tx_id_proposal, u16::MAX);
    }

    #[test]
    fn resolve_tp20_connection_params_rejects_tx_id_proposal_one_above_u16_max() {
        let mut params = valid_params();
        params
            .unum32
            .insert(PARAM_TP20_TX_ID_PROPOSAL, u32::from(u16::MAX) + 1);
        let err = resolve_tp20_connection_params(&params)
            .expect_err("one above u16::MAX must be rejected, not silently truncated");
        assert!(err.contains("CP_TP20TxIdProposal"), "{err}");
    }

    #[test]
    fn resolve_tp20_connection_params_accepts_rx_id_proposal_at_u16_max() {
        let mut params = valid_params();
        params
            .unum32
            .insert(PARAM_TP20_RX_ID_PROPOSAL, u32::from(u16::MAX));
        let resolved = resolve_tp20_connection_params(&params)
            .expect("u16::MAX should be in range for the native 2-byte field");
        assert_eq!(resolved.rx_id_proposal, u16::MAX);
    }

    #[test]
    fn resolve_tp20_connection_params_rejects_rx_id_proposal_one_above_u16_max() {
        let mut params = valid_params();
        params
            .unum32
            .insert(PARAM_TP20_RX_ID_PROPOSAL, u32::from(u16::MAX) + 1);
        let err = resolve_tp20_connection_params(&params)
            .expect_err("one above u16::MAX must be rejected, not silently truncated");
        assert!(err.contains("CP_TP20RxIdProposal"), "{err}");
    }

    // ── quarantine_tp20_connection_for_orphaned_write_back (Codex review
    // fix, PR #97, 8th round / Fix K) ─────────────────────────────────────
    //
    // A connection that becomes `Established` right as its owning CLL
    // disappears (`Disconnect`/`DestroyComLogicalLink` landing in the gap
    // between `run_tp20_connection_request` returning and `events.rs`'s own
    // write-back running) is a THIRD abandonment path this module's own doc
    // comment describes -- see `best_effort_teardown_on_abandon`'s own doc
    // comment for the native-teardown half of the fix, and this function's
    // own doc comment for why the map-mutation half is a vacant-only insert
    // rather than a mark-in-place.
    //
    // **Why these are unit tests against the pure decision, not an
    // end-to-end `tests/grpc_mock/tp20.rs` regression (attempted first, per
    // this fix's own brief):** the entire span from `run_tp20_connection_
    // request` resolving `Established` (inside its own bounded wait loop,
    // consuming the indication `poll_rx` just delivered into `tp20_
    // connection_results`) through its own post-loop cleanup and back into
    // `events.rs`'s write-back is one uninterrupted synchronous chain in the
    // SAME task: every `.lock().await` along that specific span acquires an
    // uncontended `tokio::sync::Mutex`, which resolves `Ready` on first poll
    // without ever suspending -- Rust's async model is cooperative, so a
    // task can only be preempted at an `.await` that actually returns
    // `Pending`, and this file's own `#[tokio::test]`s all run on tokio's
    // default single-threaded (current-thread) runtime (confirmed: no test
    // in this suite uses `#[tokio::test(flavor = "multi_thread")]`), so no
    // other task -- including a concurrent `DisconnectComLogicalLink`/
    // `DestroyComLogicalLink` RPC handler -- can ever be scheduled to run
    // inside this exact gap without artificially manufactured lock
    // contention on one of these specific locks, which no existing test
    // hook in this crate provides. This is the same class of "no natural
    // preemption point in this harness" limit `tp20.rs`'s own `disconnect_
    // mid_wait_after_native_issue_tears_down_the_leaked_slot` doc comment
    // documents for the sibling pre-issue `self_still_live` recheck (Fix F),
    // and `events_j1939_claim.rs`'s own `ctx_with_a_send_recv_cop` test
    // helper doc comment documents for its own analogous drain-loop race --
    // both cited here as precedent for this same reasoning, not re-derived
    // from scratch. These unit tests are therefore the actual proof of the
    // quarantine-insert decision's own correctness, independent of that
    // reachability question, mirroring `tp20_rx_id_unavailable_for`'s own
    // unit tests above for the identical reason.

    const QOWB_RX_ID: u32 = 0x0321;
    const QOWB_CLL_HANDLE: u32 = 7;
    const QOWB_CONNECT_GENERATION: u64 = 2;

    /// The ordinary case this fix targets: `run_tp20_connection_request`'s
    /// own post-loop cleanup has already unconditionally removed this
    /// attempt's entry (no `tp20_connections` entry for `rx_id` at all), so
    /// this call must insert a fresh `abandoned` quarantine entry recording
    /// the orphaned attempt's own `cll_handle`/`connect_generation`.
    #[test]
    fn inserts_a_fresh_abandoned_entry_when_vacant() {
        let mut connections: HashMap<u32, Tp20ConnEntry> = HashMap::new();
        quarantine_tp20_connection_for_orphaned_write_back(
            QOWB_RX_ID,
            QOWB_CLL_HANDLE,
            QOWB_CONNECT_GENERATION,
            &mut connections,
        );
        assert_eq!(
            connections.get(&QOWB_RX_ID).copied(),
            Some(Tp20ConnEntry {
                cll_handle: QOWB_CLL_HANDLE,
                connect_generation: QOWB_CONNECT_GENERATION,
                abandoned: true,
                expect_stale_lost: false,
                passive: false,
                idle_release_at: None,
            }),
            "a vacant rx_id must be quarantined with a fresh abandoned entry"
        );
    }

    /// A genuine new registration racing into the tiny gap between `run_
    /// tp20_connection_request`'s own removal and this write-back running
    /// must never be clobbered -- this call must leave an already-occupied
    /// entry untouched, regardless of whether it belongs to a different CLL
    /// or is itself already `abandoned`.
    #[test]
    fn does_not_clobber_an_entry_already_registered_by_something_else() {
        let mut connections = HashMap::new();
        connections.insert(
            QOWB_RX_ID,
            Tp20ConnEntry {
                cll_handle: 99,
                connect_generation: 40,
                abandoned: false,
                expect_stale_lost: false,
                passive: false,
                idle_release_at: None,
            },
        );
        quarantine_tp20_connection_for_orphaned_write_back(
            QOWB_RX_ID,
            QOWB_CLL_HANDLE,
            QOWB_CONNECT_GENERATION,
            &mut connections,
        );
        assert_eq!(
            connections.get(&QOWB_RX_ID).copied(),
            Some(Tp20ConnEntry {
                cll_handle: 99,
                connect_generation: 40,
                abandoned: false,
                expect_stale_lost: false,
                passive: false,
                idle_release_at: None,
            }),
            "an already-registered entry (a genuine new registration racing into the gap) must \
             not be overwritten by the orphaned write-back's own quarantine attempt"
        );
    }

    // ── deliver_tp20_connection_indication's abandoned-entry-release
    // ordering (Codex review fix, PR #97, 8th round / Fix L) ─────────────
    //
    // The real call site's control flow (see its own doc comment) now
    // checks `tp20_connection_indication_is_for_an_abandoned_entry` FIRST,
    // independent of `resolve_tp20_connection_indication`'s own live-owner
    // resolution -- these two functions' own existing unit tests above
    // (`indication_for_an_abandoned_entry_is_identified_for_quarantine_
    // release`, `rejects_a_same_cll_retry_against_its_own_abandoned_entry`,
    // etc.) already pin each pure decision in isolation; the actual fixed
    // ordering itself is proven end-to-end in `tests/grpc_mock/tp20.rs`
    // instead (an owner-disappears-then-a-delayed-indication-still-releases-
    // the-quarantine scenario is deterministically constructible through
    // this crate's ordinary `__mock_set_tp20_no_indication` toggle, unlike
    // Fix K's own race above), so no additional unit test is added here
    // beyond confirming the existing two decision-seam tests above remain
    // accurate for the new ordering -- both do, since neither function's own
    // behavior changed, only the ORDER `deliver_tp20_connection_indication`
    // calls them in.

    // ── resolve_tp20_passive_params (ADR-190/Phase 7 Stage 7b) ────────────

    fn valid_passive_params() -> ComParamSet {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_TP20_PASSIVE_IDENTIFIER, 0x0250);
        params.unum32.insert(PARAM_TP20_PASSIVE_RX_ID, 0x0350);
        params
    }

    #[test]
    fn resolve_tp20_passive_params_returns_none_when_neither_is_staged() {
        let params = ComParamSet::default();
        assert_eq!(resolve_tp20_passive_params(&params), Ok(None));
    }

    #[test]
    fn resolve_tp20_passive_params_round_trips_valid_values() {
        let resolved = resolve_tp20_passive_params(&valid_passive_params())
            .expect("both valid ComParams should resolve");
        assert_eq!(resolved, Some((0x0250, 0x0350)));
    }

    #[test]
    fn resolve_tp20_passive_params_rejects_only_identifier_staged() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_TP20_PASSIVE_IDENTIFIER, 0x0250);
        let err = resolve_tp20_passive_params(&params)
            .expect_err("exactly one of the two passive ComParams staged must be rejected");
        assert!(err.contains("CP_TP20PassiveIdentifier"), "{err}");
    }

    #[test]
    fn resolve_tp20_passive_params_rejects_only_rx_id_staged() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_TP20_PASSIVE_RX_ID, 0x0350);
        let err = resolve_tp20_passive_params(&params)
            .expect_err("exactly one of the two passive ComParams staged must be rejected");
        assert!(err.contains("CP_TP20PassiveRxId"), "{err}");
    }

    #[test]
    fn resolve_tp20_passive_params_rejects_identifier_out_of_range() {
        let mut params = valid_passive_params();
        params.unum32.insert(PARAM_TP20_PASSIVE_IDENTIFIER, 0x0100);
        let err = resolve_tp20_passive_params(&params)
            .expect_err("an out-of-range CP_TP20PassiveIdentifier must be rejected");
        assert!(err.contains("CP_TP20PassiveIdentifier"), "{err}");
    }

    #[test]
    fn resolve_tp20_passive_params_rejects_identifier_staged_as_zero() {
        // ADR-190 section 1: a staged 0 is meaningless -- not staging at all
        // already means "don't arm."
        let mut params = valid_passive_params();
        params.unum32.insert(PARAM_TP20_PASSIVE_IDENTIFIER, 0);
        let err = resolve_tp20_passive_params(&params)
            .expect_err("a staged 0 identifier must be rejected, not treated as unset");
        assert!(err.contains("CP_TP20PassiveIdentifier"), "{err}");
    }

    #[test]
    fn resolve_tp20_passive_params_rejects_rx_id_out_of_range() {
        let mut params = valid_passive_params();
        params.unum32.insert(PARAM_TP20_PASSIVE_RX_ID, 0x0200);
        let err = resolve_tp20_passive_params(&params)
            .expect_err("an out-of-range CP_TP20PassiveRxId must be rejected");
        assert!(err.contains("CP_TP20PassiveRxId"), "{err}");
    }

    #[test]
    fn resolve_tp20_passive_params_rejects_rx_id_staged_as_zero() {
        let mut params = valid_passive_params();
        params.unum32.insert(PARAM_TP20_PASSIVE_RX_ID, 0);
        let err = resolve_tp20_passive_params(&params)
            .expect_err("a staged 0 rx_id_passive must be rejected, not treated as unset");
        assert!(err.contains("CP_TP20PassiveRxId"), "{err}");
    }

    #[test]
    fn resolve_tp20_passive_params_rejects_an_active_comparam_staged_alongside() {
        for &active_id in &[
            PARAM_TP20_CHANNEL_SETUP_CAN_ID,
            PARAM_TP20_DESTINATION_ADDRESS,
            PARAM_TP20_TX_ID_PROPOSAL,
            PARAM_TP20_RX_ID_PROPOSAL,
            PARAM_TP20_APPLICATION_TYPE,
        ] {
            let mut params = valid_passive_params();
            params.unum32.insert(active_id, 1);
            let err = resolve_tp20_passive_params(&params).expect_err(&format!(
                "staging {active_id:?} alongside both passive ComParams must be rejected as \
                 ambiguous intent"
            ));
            assert!(err.contains("ambiguous intent"), "{err}");
        }
    }

    // ── quarantine_tp20_passive_slot_on_disarm (ADR-190/Phase 7 Stage 7b) ─

    /// Builds a minimal `SharedChannel` for direct construction in a unit
    /// test -- mirrors `rpc_link.rs`'s own `dead_shared_channel_for_test`
    /// construction shape (not reused directly: that helper is private to
    /// `rpc_link.rs`'s own test module).
    fn test_shared_channel_for_disarm() -> SharedChannel {
        SharedChannel {
            channel_id: ChannelId(9999),
            ref_count: 1,
            tx_queue: tokio::sync::mpsc::unbounded_channel().0,
            executing_cop: Arc::new(Mutex::new(None)),
            _poll_cancel: tokio::sync::oneshot::channel().0,
            connect_flags: 0,
            dead: false,
            occupancy_epoch: 0,
            leaked_repeat_message_ids: Vec::new(),
            leaked_periodic_message_ids: Vec::new(),
            applied_analog_sample_rate: None,
            applied_analog_samples_per_reading: None,
            applied_analog_readings_per_msg: None,
            j1939_claims: HashMap::new(),
            j1939_claim_results: HashMap::new(),
            j1939_reclaim_pending: HashMap::new(),
            leaked_j1939_claims: Vec::new(),
            tp20_connections: HashMap::new(),
            tp20_connection_results: HashMap::new(),
            become_master_in_flight: Arc::new(portable_atomic::AtomicBool::new(false)),
            tp20_passive: None,
        }
    }

    #[test]
    fn quarantine_tp20_passive_slot_on_disarm_marks_the_entry_abandoned_and_clears_the_slot() {
        let mut sc = test_shared_channel_for_disarm();
        sc.tp20_connections.insert(
            0x0350,
            Tp20ConnEntry {
                cll_handle: 1,
                connect_generation: 1,
                abandoned: false,
                expect_stale_lost: false,
                passive: true,
                idle_release_at: None,
            },
        );
        sc.tp20_passive = Some(Tp20PassiveSlot {
            cll_handle: 1,
            connect_generation: 1,
            identifier: 0x0250,
            rx_id_passive: 0x0350,
        });

        quarantine_tp20_passive_slot_on_disarm(0x0350, &mut sc, true, true);

        assert!(
            sc.tp20_connections
                .get(&0x0350)
                .is_some_and(|e| e.abandoned),
            "the persistent entry must be marked abandoned in place, never removed"
        );
        assert_eq!(
            sc.tp20_passive, None,
            "the exclusivity token must be cleared"
        );
    }

    #[test]
    fn quarantine_tp20_passive_slot_on_disarm_is_a_no_op_when_no_entry_exists() {
        // Defense-in-depth: must not panic if called with no matching entry
        // (e.g. it was already released by a delayed indication).
        let mut sc = test_shared_channel_for_disarm();
        sc.tp20_passive = Some(Tp20PassiveSlot {
            cll_handle: 1,
            connect_generation: 1,
            identifier: 0x0250,
            rx_id_passive: 0x0350,
        });

        quarantine_tp20_passive_slot_on_disarm(0x0350, &mut sc, false, true);

        assert!(!sc.tp20_connections.contains_key(&0x0350));
        assert_eq!(sc.tp20_passive, None);
    }

    /// ADR-190's "Correction" paragraph under `### 4. Disarm / teardown`
    /// (Codex review finding, P1, PR #99; design-advisor consult):
    /// `quarantine_tp20_passive_slot_on_disarm`'s own `idle_release_at`
    /// stamping decision -- `Some` (a bounded release deadline) only for
    /// the one combination that means "no native call was ever issued, and
    /// the native config-clear itself succeeded", `None` (the pre-existing
    /// indefinite quarantine) for every other combination.
    #[test]
    fn quarantine_tp20_passive_slot_on_disarm_stamps_idle_release_at_only_when_never_established_and_config_cleared()
     {
        let cases: [(bool, bool, &str); 4] = [
            (
                false,
                true,
                "never established, config-clear succeeded -- must stamp a bounded release deadline",
            ),
            (
                true,
                true,
                "already established -- a real TEARDOWN_CONNECTION was issued, so release must \
                 wait for its own indication, not a bounded deadline",
            ),
            (
                true,
                false,
                "already established, even though the config-clear itself failed -- still gated \
                 on the real teardown's own indication",
            ),
            (
                false,
                false,
                "never established AND the config-clear itself failed -- the device may still be \
                 able to auto-accept on this RX-ID, so release must stay indefinite",
            ),
        ];
        for (was_established, config_cleared, why) in cases {
            let mut sc = test_shared_channel_for_disarm();
            sc.tp20_connections.insert(
                0x0350,
                Tp20ConnEntry {
                    cll_handle: 1,
                    connect_generation: 1,
                    abandoned: false,
                    expect_stale_lost: false,
                    passive: true,
                    idle_release_at: None,
                },
            );

            quarantine_tp20_passive_slot_on_disarm(
                0x0350,
                &mut sc,
                was_established,
                config_cleared,
            );

            let idle_release_at = sc
                .tp20_connections
                .get(&0x0350)
                .and_then(|e| e.idle_release_at);
            if !was_established && config_cleared {
                assert!(
                    idle_release_at.is_some(),
                    "was_established={was_established}, config_cleared={config_cleared}: {why}"
                );
            } else {
                assert!(
                    idle_release_at.is_none(),
                    "was_established={was_established}, config_cleared={config_cleared}: {why}"
                );
            }
        }
    }

    // ── tp20_passive_idle_release_due (ADR-190's "Correction" paragraph
    // under `### 4. Disarm / teardown`, Codex review finding, P1, PR #99;
    // design-advisor consult) ──────────────────────────────────────────

    fn idle_release_test_entry(
        abandoned: bool,
        passive: bool,
        idle_release_at: Option<tokio::time::Instant>,
    ) -> Tp20ConnEntry {
        Tp20ConnEntry {
            cll_handle: 1,
            connect_generation: 1,
            abandoned,
            expect_stale_lost: false,
            passive,
            idle_release_at,
        }
    }

    /// The one `true` case: abandoned, passive, a deadline is stamped, and
    /// the watermark has caught up to (or passed) it.
    #[test]
    fn tp20_passive_idle_release_due_is_true_once_the_watermark_reaches_the_deadline() {
        let now = tokio::time::Instant::now();
        let dl = now + std::time::Duration::from_millis(1);
        let entry = idle_release_test_entry(true, true, Some(dl));
        assert!(tp20_passive_idle_release_due(&entry, Some(dl)));
        assert!(tp20_passive_idle_release_due(
            &entry,
            Some(dl + std::time::Duration::from_millis(1))
        ));
    }

    #[test]
    fn tp20_passive_idle_release_due_is_false_when_not_abandoned() {
        let now = tokio::time::Instant::now();
        let entry = idle_release_test_entry(false, true, Some(now));
        assert!(!tp20_passive_idle_release_due(&entry, Some(now)));
    }

    #[test]
    fn tp20_passive_idle_release_due_is_false_when_not_passive() {
        let now = tokio::time::Instant::now();
        let entry = idle_release_test_entry(true, false, Some(now));
        assert!(!tp20_passive_idle_release_due(&entry, Some(now)));
    }

    #[test]
    fn tp20_passive_idle_release_due_is_false_when_idle_release_at_is_none() {
        let now = tokio::time::Instant::now();
        let entry = idle_release_test_entry(true, true, None);
        assert!(!tp20_passive_idle_release_due(&entry, Some(now)));
    }

    #[test]
    fn tp20_passive_idle_release_due_is_false_when_watermark_is_none() {
        let now = tokio::time::Instant::now();
        let entry = idle_release_test_entry(true, true, Some(now));
        assert!(!tp20_passive_idle_release_due(&entry, None));
    }

    #[test]
    fn tp20_passive_idle_release_due_is_false_when_watermark_is_before_the_deadline() {
        let now = tokio::time::Instant::now();
        let dl = now + std::time::Duration::from_millis(50);
        let entry = idle_release_test_entry(true, true, Some(dl));
        assert!(!tp20_passive_idle_release_due(
            &entry,
            Some(now + std::time::Duration::from_millis(10))
        ));
    }
}
