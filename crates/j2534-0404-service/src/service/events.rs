use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use j2534_0404::{ChannelId, J2534Api0404};
use tokio::sync::{Mutex, mpsc, oneshot, watch::Receiver};
use tracing::{debug, warn};
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveHandle, EventItem, EventNotification,
    LostEventItemNotification, ModuleHandle, PduComLogicalLinkStatus, PduComPrimitiveStatus,
    PduErrorEvent, PduInfo, PduModuleStatus, ResultData, SystemHandle,
    event_item::Data as EventItemData,
    event_notification::{EventData as EventNotificationData, Handle as EventNotificationHandle},
};

// `CanChannelMode` (ADR-217; widened to a named parameter again by ADR-222
// round 6, `poll_rx_inner`'s `can_channel_mode` local): not named directly
// by this file's own live code even so -- `effective_can_channel_mode()`'s
// return type is inferred at its one call site, and `is_native_mixed_
// family()`/the `DualChannel` comparison this ADR needs both now live
// inside `build_cll_rx_entries` (`events_rx_routing.rs`) instead. Several
// `#[cfg(test)]` submodules declared further down
// (`events_j1939_claim.rs`'s own `mod tests`,
// `events_reap_expired_cyclic_registrants_cop_tag_tests.rs`, and others)
// reach it transitively via `use super::*;` for their `LogicalLinkState`
// test fixtures' `can_channel_mode` field -- unused outside `cfg(test)`,
// hence the explicit allow rather than dropping the import.
#[allow(unused_imports)]
use super::CanChannelMode;
use super::rpc_link::{
    PointToPointFilterEligibility, point_to_point_filter_eligibility, usdt_resp_id, uudt_resp_id,
};
use super::{
    AccessTimingConfig, ChannelKey, ChannelProtocol, CllEventQueue, CllQueueItem, ComParamId,
    ComParamSet, ConcatBuf, CopEntry, CopRef, CopRegistrant, DEFAULT_MODULE_HANDLE, DiscardWindow,
    EcuTimingRecord, EcuUniqueRespEntry, ErrorCop, EventQueueMode, ExpectedResponse, FastInit,
    FiveBaudInit, IsoTpFraming, J1939NegotiationPosture, J2534Service, LOCK_PHYSICAL_COM_PARAMS,
    LogicalLinkState, ModuleState, OneShotCommTx, PARAM_ACCESS_TIMING_ECU,
    PARAM_CAN_PHYS_REQ_EXT_ADDR, PARAM_CAN_PHYS_REQ_FORMAT, PARAM_CAN_PHYS_REQ_ID,
    PARAM_CAN_RESP_USDT_EXT_ADDR, PARAM_CAN_RESP_USDT_FORMAT, PARAM_CAN_RESP_USDT_ID,
    PARAM_CAN_RESP_UUDT_EXT_ADDR, PARAM_CAN_RESP_UUDT_FORMAT, PARAM_CAN_RESP_UUDT_ID,
    PARAM_ECU_RESP_SOURCE_ADDR, PARAM_INIT_SETTINGS, PARAM_J1939_ADDR_NEG_RULE, PARAM_J1939_NAME,
    PARAM_J1939_SOURCE_ADDRESS, PARAM_J1939_TARGET_ADDRESS, PARAM_P2_STAR,
    PARAM_SESSION_TIMING_ECU, PDU_HANDLE_UNDEF, ParamBinding, PendingTimingChange,
    RcHandlingConfig, ReceivedFrame, RegistrantTier, ResidualTesterPresentDiscard,
    SYSTEM_SUBSCRIPTION_KEY, SendRecvTx, SessionTimingConfig, SoftIsoTpTx, StatusEvent,
    SubscriptionKey, SubscriptionSender, TerminalCopsLedger, TesterPresentState,
    TesterPresentTargetCanIds, TimingAccumulator, TimingChangeConfig, Tp20BroadcastPeriodic,
    Tp20Connection, Tp20ConnectionOutcome, Tp20ConnectionPhase, TrackedError, TrackedStatus,
    TxGapState, TxItem, comparam_defaults, comparam_support, expand_tidle, expand_uart_config,
    find_physical_lock_holder, isotp, recompute_lock_tx_suspensions, resources, rpc_primitive,
    to_j2534_config_value, tx_header,
};
use isotp::Addressing;

const POLL_INTERVAL_MS: u64 = 10;

/// A TP2.0 broadcast-periodic entry captured for termination via
/// `J2534Service::terminate_tp20_broadcast_periodic_for_suspension`, paired
/// with the `cll_handle` it belongs to: `(cll_handle, periodic, channel_id,
/// connect_generation, channel_key)`. Codex review fix (P2, PR #101, round
/// 10): factored out to satisfy `clippy::type_complexity` once
/// `connect_generation`/`channel_key` were added alongside `periodic`/
/// `channel_id` (that function's own doc comment has the full rationale).
type SuspendedBroadcastPeriodic = (
    u32,
    Tp20BroadcastPeriodic,
    Option<ChannelId>,
    u64,
    Option<ChannelKey>,
);

/// Same as [`SuspendedBroadcastPeriodic`] but without the leading
/// `cll_handle` -- for a call site where the CLL is already known from
/// surrounding scope: `(periodic, channel_id, connect_generation,
/// channel_key)`.
type SuspendedBroadcastPeriodicEntry = (
    Tp20BroadcastPeriodic,
    Option<ChannelId>,
    u64,
    Option<ChannelKey>,
);
const MAX_POLL_MESSAGES: usize = 8;
const MODULE_EVENT_BUF_CAPACITY: usize = 16;
const SYSTEM_EVENT_BUF_CAPACITY: usize = 16;

/// Internal defensive bound on consecutive `FS_WAIT` frames tolerated per
/// FlowControl-wait cycle in the software ISO-TP TX driver -- NOT
/// `CP_CanMaxNumWaitFrames` enforcement (see ADR-124, which supersedes
/// ADR-121's original claim that this comparam governs this direction; it
/// actually bounds WAIT frames this service itself transmits as a
/// *receiver*, not WAIT frames it tolerates from a peer as a *sender*,
/// which is what this constant guards). Set to ISO 22900-2's own documented
/// upper bound for that comparam's `[0, 1027]` range
/// (`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:5992`)
/// so this guard cannot reject any WAIT count a real peer/protocol preset
/// could ever legitimately produce -- it exists purely to bound a
/// pathological, effectively-infinite flood, not to enforce a
/// client-configurable policy.
const ISOTP_TX_MAX_CONSECUTIVE_RX_WAIT_FRAMES: u32 = 1027;

/// ADR-148 third Amendment (Fix 2): internal, non-configurable cap on one
/// `CP_EnableConcatenation` `ConcatBuf`'s merged payload size -- neither of
/// the absorb sites (the continuation fast path, and the "open new buffer"
/// arm's absorb-into-existing branch, both in `bind_registrant`) let a
/// buffer's `data.len()` grow past this without force-finalizing it
/// immediately (`finalize_one_concat_buffer`), rather than holding it open
/// and restarting the receive-phase deadline forever against a noisy/
/// misbehaving sender. Set to `j2534_0404::MAX_MESSAGE_DATA`, the same cap
/// this crate already enforces on one whole `PassThruMessage`'s data --
/// generous headroom for any conformant KWP/J1850 segmented response. This
/// bounds the OPEN buffer's growth across polls, not the delivered
/// `ResultData`'s exact size: both absorb sites check this cap AFTER
/// absorbing the triggering segment, deliberately, so a real ECU's response
/// bytes are never dropped to enforce it (ADR-148 Amendment 7 -- Codex
/// round-8 finding, confirmed a documentation-accuracy gap, not a behavior
/// bug: reaffirmed rather than changed). A force-finalized delivery
/// therefore carries at most `CONCAT_MAX_BUF_BYTES + MAX_MESSAGE_DATA - 1`
/// bytes; an open buffer (between polls, before any absorb that would trip
/// this cap) never exceeds it, since one physical segment's own payload can
/// never itself exceed `MAX_MESSAGE_DATA`.
const CONCAT_MAX_BUF_BYTES: usize = j2534_0404::MAX_MESSAGE_DATA;

/// ADR-148 third Amendment (Fix 2): internal, non-configurable cap on the
/// number of physical segments (`ConcatBuf::segments`) absorbed into one
/// buffer, checked alongside `CONCAT_MAX_BUF_BYTES` on every absorb -- closes
/// the degenerate case where a SID-only 1-byte payload adds `0` bytes to
/// `data` per segment (never tripping the byte cap) but would otherwise
/// restart the receive-phase deadline forever. Checked post-absorb like the
/// byte cap above, for the same reason: a force-finalized delivery reports
/// at most `CONCAT_MAX_BUF_SEGMENTS + 1` segments, not an exact ceiling
/// (ADR-148 Amendment 7).
const CONCAT_MAX_BUF_SEGMENTS: u32 = 256;

/// ADR-148 third Amendment (Fix 3): internal, non-configurable cap on the
/// number of DISTINCT open `ConcatBuf`s (`CopRegistrant::concat.len()`) one
/// registrant may hold at once. Only the "open a new buffer" arm's genuinely-
/// new-buffer branch is gated on this (absorbing into an already-open buffer
/// never increases the open count); when the cap is hit, the triggering
/// frame simply falls through unabsorbed, same as today's quota-exhausted
/// case. `32` is generous headroom over ADR-148 Amendment 1's documented
/// real-world cardinality (single-digit distinct ECUs for IS-MULTIPLE), not
/// a real-world limit -- it exists purely to bound a noisy adapter feeding a
/// vacuous-descriptor IS-MULTIPLE COP unbounded distinct-key frames, not to
/// constrain legitimate multi-ECU traffic.
const CONCAT_MAX_OPEN_BUFFERS: u32 = 32;

/// Bounds `CoptStopcomm`'s non-cancellable IS-MULTIPLE (`NumReceiveCycles ==
/// -2`) receive phase (ADR-087) against a chatty ECU or a broad/empty
/// `expected_response` pattern that would otherwise reset the per-match
/// deadline forever. `CoptSendrecv`'s IS-MULTIPLE receive phase stays
/// unbounded, since it is cancellable via `CancelComPrimitive`.
const STOPCOMM_IS_MULTIPLE_CEILING_FACTOR: u32 = 16;
const STOPCOMM_IS_MULTIPLE_CEILING_FLOOR_MS: u32 = 2_000;

/// `RX_TX_MSG_TYPE` (J2534 v04.04 `PassThru` RxStatus bit `0x00000001`) --
/// marks a received frame as an echo of a message this device itself
/// transmitted (`CONFIG_LOOPBACK`), not genuine external bus traffic. Not
/// present in `j2534-0404-sys`'s generated bindings: bindgen's
/// `allowlist_var` regex matches `RX_FLAG_.*` but not this bare
/// `RX_TX_MSG_TYPE` name, and widening that regex would mean regenerating
/// committed bindings for all 5 pre-committed targets via
/// `--features bindgen` -- out of scope for this fix, which only needs the
/// one bit value the J2534 v04.04 header already defines.
const RX_TX_MSG_TYPE: u32 = 0x0000_0001;

/// `START_OF_MESSAGE` (J2534 v04.04 `PassThru` RxStatus bit `0x00000002`) --
/// marks a received frame as the first byte/frame of a message rather than a
/// (re)assembled response. ADR-098: one of the 5 low `RxStatus` bits this
/// service forwards into ISO 22900-2's `ResultData.rx_flag` (byte 3 bit
/// 1), since J2534 v04.04's `RxStatus` and ISO 22900-2's `RxFlag` are
/// bit-compatible under big-endian serialization for this bit. Also
/// contributes (alongside `RX_BREAK`, `RX_TX_INDICATION`, and
/// `RX_TX_MSG_TYPE` -- but not `RX_ISO15765_PADDING_ERROR`, see its own doc
/// comment) to excluding indication-type frames from
/// `ExpectedResponse`/pending-RC matching in `poll_rx_inner`, since their
/// always-empty post-split payload would otherwise vacuously match a
/// broad/empty descriptor.
const RX_START_OF_MESSAGE: u32 = 0x0000_0002;

/// `RX_BREAK` (J2534 v04.04 `PassThru` RxStatus bit `0x00000004`) -- a break
/// was received (SCI/J1850 VPW only). ADR-098.
const RX_BREAK: u32 = 0x0000_0004;

/// `TX_INDICATION` (J2534 v04.04 `PassThru` RxStatus bit `0x00000008`) -- an
/// ISO 15765 TxDone indication for a message this device transmitted.
/// ADR-098.
const RX_TX_INDICATION: u32 = 0x0000_0008;

/// `ISO15765_PADDING_ERROR` (J2534 v04.04 `PassThru` RxStatus bit
/// `0x00000010`) -- a CAN frame with fewer than 8 data bytes was received
/// under ISO15765. ADR-098 (corrected): unlike the other 4
/// `RX_STATUS_FLAGS_MASK` bits, this one tags a genuine, fully reassembled
/// ISO15765 response (not a header-only indication) and is therefore
/// excluded from `poll_rx_inner`'s `ExpectedResponse`/pending-RC exclusion
/// gate -- a frame with only this bit set (alone or combined with a clean
/// payload) remains eligible for matching.
const RX_ISO15765_PADDING_ERROR: u32 = 0x0000_0010;

/// ADR-098 (corrected): the 5 low `RxStatus` bits this service forwards
/// unconditionally (bit-copy, no combination validation) into
/// `ResultData.rx_flag` byte 3. Of these, only the 4 indication-type bits
/// (`RX_TX_MSG_TYPE`, `RX_START_OF_MESSAGE`, `RX_BREAK`, `RX_TX_INDICATION`)
/// gate `ExpectedResponse`/pending-RC eligibility; `RX_ISO15765_PADDING_ERROR`
/// tags a genuine non-empty response and does not exclude a frame from
/// matching. See the ADR-098 Correction note.
const RX_STATUS_FLAGS_MASK: u32 =
    RX_TX_MSG_TYPE | RX_START_OF_MESSAGE | RX_BREAK | RX_TX_INDICATION | RX_ISO15765_PADDING_ERROR;

/// `RX_FLAG_SW_CAN_HS_RX` (SAE J2534-2 clause 9.4.1.1 Table 12 `RxStatus`
/// bit `0x00020000`, bit 17) -- a Single Wire CAN (SWCAN/GMLAN) high-speed
/// mode speed-transition confirmation, per clause 9.3.2.3 sent for both
/// commanded and automatic speed changes. Its data content is explicitly
/// undefined (ADR-172). Deliberately outside `RX_STATUS_FLAGS_MASK`: that
/// mask's `u8` truncation cannot represent bits above 7, and this bit's
/// SW-CAN-only meaning must be checked against the frame's own native
/// protocol id (`resources::is_sw_protocol_id`) before it can be excluded
/// -- bit 17 means `LINK_FAULT` on Fault-Tolerant CAN (clause 20, Table 86)
/// and must NOT be excluded there.
const RX_SW_CAN_HS_RX: u32 = 0x0002_0000;

/// `RX_FLAG_SW_CAN_NS_RX` (SAE J2534-2 clause 9.4.1.1 Table 12 `RxStatus`
/// bit `0x00040000`, bit 18) -- a Single Wire CAN (SWCAN/GMLAN) normal-speed
/// mode speed-transition confirmation. See [`RX_SW_CAN_HS_RX`]'s doc comment
/// (ADR-172) -- same rationale, same withhold treatment.
const RX_SW_CAN_NS_RX: u32 = 0x0004_0000;

/// `RX_FLAG_SW_CAN_HV_RX` (SAE J2534-2 clause 9.4.1.1 Table 12 `RxStatus`
/// bit `0x00010000`, bit 16) -- a Single Wire CAN (SWCAN/GMLAN) high-voltage
/// mode reception. Unlike [`RX_SW_CAN_HS_RX`]/[`RX_SW_CAN_NS_RX`], this bit
/// tags genuine, content-eligible data (ADR-172 Decision 3) and is never
/// withheld. It shares bit position 16 with SAE J1939's `ADDRESS_CLAIMED`
/// ([`RX_J1939_ADDRESS_CLAIMED`]) and TP2.0's `CONNECTION_ESTABLISHED`
/// ([`RX_TP20_CONNECTION_ESTABLISHED`]), both already defined in this file at
/// the same `0x0001_0000` value -- disambiguated here by
/// `resources::is_sw_protocol_id`, the same way those two are disambiguated
/// by their own protocol-id predicates. ADR-191: this is the first `RxStatus`
/// bit this file forwards into ISO 22900-2's `RxFlag` byte 1 (byte 1 bit 0),
/// unlike [`RX_SW_CAN_HS_RX`]/[`RX_SW_CAN_NS_RX`], which stay withheld
/// entirely per ADR-172.
const RX_SW_CAN_HV_RX: u32 = 0x0001_0000;

/// `RX_FLAG_J1939_ADDRESS_CLAIMED` (SAE J2534-2 clause 16.4.6 Table 63
/// RxStatus bit `0x00010000`, bit 16) -- ADR-179/Phase 5: a SAE J1939
/// address-claim negotiation (SAE J1939-81) succeeded for the address in
/// the indication frame's `Data[0]`. Same bit-position-overloading
/// situation [`RX_SW_CAN_HS_RX`]/[`RX_SW_CAN_NS_RX`] document above (bits
/// 16-18 mean different things depending on the frame's own native
/// protocol id) -- disambiguated here by `resources::is_j1939_protocol_id`
/// instead of `is_sw_protocol_id`. Unlike SWCAN's silent `continue` (ADR-
/// 172's plain withhold), this bit's frame is routed into ADR-179 Decision
/// 3's address-claim state machine before being withheld -- see
/// `poll_rx_inner`'s own withhold arm for this bit.
const RX_J1939_ADDRESS_CLAIMED: u32 = 0x0001_0000;

/// `RX_FLAG_J1939_ADDRESS_LOST` (SAE J2534-2 clause 16.4.6 Table 63
/// RxStatus bit `0x00020000`, bit 17) -- ADR-179/Phase 5: an address claim
/// or defense failed for the address in the indication frame's `Data[0]`
/// (either a fresh claim attempt lost arbitration, or a previously-claimed
/// address was successfully challenged/defended away). See
/// [`RX_J1939_ADDRESS_CLAIMED`]'s doc comment -- same routing/withhold
/// treatment.
const RX_J1939_ADDRESS_LOST: u32 = 0x0002_0000;

/// `RX_FLAG_CONNECTION_ESTABLISHED` (SAE J2534-2 clause 19.4.4 Table 81
/// RxStatus bit `0x00010000`, bit 16) -- ADR-188/Phase 7 Stage 7a: a TP2.0
/// connection request succeeded. Same bit-position-overloading situation
/// [`RX_J1939_ADDRESS_CLAIMED`] documents -- disambiguated here by
/// `resources::is_tp2_0_family_protocol_id` instead of `is_j1939_protocol_id`.
/// Routed into ADR-188's connection-request state machine before being
/// withheld -- see `poll_rx_inner`'s own withhold arm for this bit.
const RX_TP20_CONNECTION_ESTABLISHED: u32 = 0x0001_0000;

/// `RX_FLAG_CONNECTION_LOST` (SAE J2534-2 clause 19.4.4 Table 81 RxStatus
/// bit `0x00020000`, bit 17) -- ADR-188/Phase 7 Stage 7a: a TP2.0
/// connection request failed, or an established connection was lost/torn
/// down. See [`RX_TP20_CONNECTION_ESTABLISHED`]'s doc comment -- same
/// routing/withhold treatment.
const RX_TP20_CONNECTION_LOST: u32 = 0x0002_0000;

/// ADR-151: whether `entry`'s own `CP_StartMsgIndEnable`/
/// `CP_TransmitIndEnable` opt-in withholds a pure SOM/TxDone indication
/// frame from delivery. Called only from `poll_rx_inner`'s
/// `FrameBinding::Unbound` fallthrough arm, where every reachable frame is
/// already known to be non-content (SAE J2534-1 §8.7.2's RxStatus bit
/// table) — this is a whole-frame decision, not a per-bit mask edit.
///
/// Precedence is `RX_BREAK`-first, then TX_INDICATION-dominant, then
/// loopback-independent:
///
/// - `RX_BREAK` always wins over every other gate, even combined with any
///   other bit: it is never a governed indication type in its own right
///   (neither ComParam names it), mirroring the same-file precedent at
///   `bind_frame`'s tester-present signature check (`RX_BREAK`
///   unconditionally defeats that discard too -- "never a tester-present
///   artifact ... even combined with another bit"). A `SOM | RX_BREAK`
///   off-spec combination (edge-case-hunter finding on this diff) would
///   otherwise have its RX_BREAK signal silently withheld from the client
///   whenever `start_msg_ind_enable` is false -- the spec default.
/// - Else a TxDone frame (SAE J2534-1 §8.7.2 always pairs `RX_TX_INDICATION`
///   with `RX_TX_MSG_TYPE`) is governed solely by `transmit_ind_enable`.
/// - Else a bare `RX_TX_MSG_TYPE` bit (`RX_TX_INDICATION` absent) is a
///   CONFIG_LOOPBACK echo, which ADR-098 explicitly decided is never
///   filtered from delivery -- including the documented
///   `RX_TX_MSG_TYPE | RX_START_OF_MESSAGE` combination (a loopback echo of
///   our own SOM-tagged transmit, ADR-098's Context). `RX_TX_MSG_TYPE` is
///   therefore an independent delivery justification that must be checked
///   BEFORE `RX_START_OF_MESSAGE` below, not folded into it (Codex review
///   finding on this diff: an earlier version gated this whole combination
///   on `start_msg_ind_enable`, silently dropping a valid loopback echo
///   whenever that ComParam was at its spec default of disabled).
/// - Else a `RX_START_OF_MESSAGE` bit (a pure SOM herald, no loopback tag)
///   falls to `start_msg_ind_enable`.
/// - Else -> never suppressed by either ComParam.
fn indication_suppressed(entry: &CllRxEntry, rx_status_flags: u8) -> bool {
    if rx_status_flags & RX_BREAK as u8 != 0 {
        false
    } else if rx_status_flags & RX_TX_INDICATION as u8 != 0 {
        !entry.transmit_ind_enable
    } else if rx_status_flags & RX_TX_MSG_TYPE as u8 != 0 {
        false
    } else if rx_status_flags & RX_START_OF_MESSAGE as u8 != 0 {
        !entry.start_msg_ind_enable
    } else {
        false
    }
}

#[path = "events_timestamp.rs"]
mod timestamp;
pub(super) use timestamp::*;

#[path = "events_event_senders.rs"]
mod event_senders;
pub(super) use event_senders::*;

/// Handles a hard channel error: marks all CLLs on `channel_id` as offline,
/// cancels their COPs, and emits the ISO 22900-2 loss-of-comms event sequence:
///
///   PDU_ERR_EVT_LOST_COMM_TO_VCI → PDU_COPST_CANCELLED × N
///   → PDU_CLLST_OFFLINE → PDU_MODST_NOT_AVAIL
///
/// Clearing `channel_id` on each link prevents a subsequent `poll_rx` call from
/// re-emitting events for the same channel.
///
/// Callers must not hold `api` across this call (ADR-080/ADR-123: this
/// function acquires `shared_channels`, which must be the OUTERMOST lock
/// whenever held alongside `api`/`logical_links` -- see `poll_rx_inner`'s own
/// comment at its call site).
async fn handle_channel_hard_error(channel_id: ChannelId, ctx: &ChannelPollCtx) {
    let primitives = &ctx.primitives;
    let logical_links = &ctx.logical_links;
    let subscriptions = &ctx.subscriptions;
    let terminal_cops = &ctx.service.terminal_cops;
    let module_state = &ctx.module_state;
    let module_event_buf = &ctx.module_event_buf;
    let system_event_buf = &ctx.system_event_buf;
    let shared_channels = &ctx.service.shared_channels;

    // A CLL is taken offline when either its primary channel or its
    // dual-channel-mode UUDT companion channel (ADR-046) suffered the hard
    // error — the CLL's communication contract is broken either way.
    // `channel_key` / `uudt_channel_key` are intentionally left in place so a
    // subsequent Disconnect/Destroy still releases the shared-channel refs.
    //
    // ADR-123 Finding I (Codex review round 5): a dead CLL's held
    // `held_lock_mask` is cleared here too, and `recompute_lock_tx_suspensions`
    // is run inside the SAME `logical_links` critical section that clears it,
    // under `shared_channels` (§3's governing invariant) -- hard-error offline
    // is treated as the §9.4.13.3 use-case-3 automatic-unlock analog: a dead
    // CLL can never call `UnlockResource` itself (its channel is gone), and its
    // held `LOCK_PHYSICAL_TX_QUEUE` would otherwise permanently starve a
    // sibling with no recovery path.
    //
    // ADR-139: `chans` is held here and NOT released until after the
    // per-CLL `cancel_link_cops` loop below (unlike the narrower scope this
    // block used before) -- mirroring `rpc_disconnect_com_logical_link`,
    // which holds `shared_channels` across its own `cancel_link_cops` call
    // for exactly this reason (ADR-080). `finalize_connected_link` is the
    // only path that ever publishes a CLL as `connected = true` on a NEW
    // connect_generation, and it itself requires `shared_channels`; the
    // UUDT-companion-channel join (`ensure_uudt_companion_channel`) never
    // touches `connected`/`connect_generation` at all and never inserts a
    // `CopEntry` (ADR-161), so holding `shared_channels` here makes it
    // impossible for a same-`cll_handle` reconnect to insert a live
    // `CopEntry` --
    // `rpc_start_com_primitive` never needs `shared_channels` itself, but it
    // can only run against a connection that already finished connecting --
    // between this function's `chans` acquisition and the last
    // `cancel_link_cops` call below. Without this, a reconnect landing in
    // that window (onto a brand-new physical channel -- ADR-134's `dead`
    // flag only protects a join of the SAME, now-dead `SharedChannel` entry,
    // not a fresh `spawn_new_shared_channel`) could have its live COP
    // wrongly cancelled by this function's own cancel sweep, which matches
    // `primitives` on `cll_handle` alone with no connect-generation check.
    // ADR-193 amendment (Codex review round 16, P1, PR #101): tracks
    // whether this sweep takes a `None`-sentinel in-flight-start reservation
    // off any CLL below, so the batched `self.api` fence acquisition after
    // that block (see its own doc comment) knows whether it has anything to
    // fence at all.
    let mut in_flight_start_taken = false;
    let mut chans = shared_channels.lock().await;
    // ADR-134 Correction (Codex review round 6, PR #149): mark the
    // `SharedChannel` entry this channel_id belongs to (if any -- a
    // companion/primary channel not currently shared by any live CLL
    // has none) as `dead`, under this same `shared_channels` guard, so a
    // concurrent `rpc_connect_com_logical_link`/`ensure_uudt_companion_channel`
    // join racing this hard-error handler cannot publish a CLL as
    // connected on a channel whose poll task is exiting -- see
    // `SharedChannel::dead`'s doc comment for why the entry itself is
    // deliberately not removed here.
    if let Some(sc) = chans.values_mut().find(|sc| sc.channel_id == channel_id) {
        sc.dead = true;
    }
    let (cll_handles, wake_targets) = {
        let mut links = logical_links.lock().await;
        let cll_handles: Vec<u32> = links
            .iter_mut()
            .filter(|(_, l)| {
                l.channel_id == Some(channel_id) || l.uudt_channel_id == Some(channel_id)
            })
            .map(|(h, l)| {
                // Captured before the branch below clears `channel_id`
                // (Codex review, PR #47): `was_primary`'s repeat-message
                // drain further down needs to know whether THIS channel
                // was the link's primary before this function wipes that
                // fact away.
                let was_primary = l.channel_id == Some(channel_id);
                l.connected = false;
                l.comm_started = false;
                l.stop_comm_pending = false;
                if l.channel_id == Some(channel_id) {
                    l.channel_id = None;
                }
                if l.uudt_channel_id == Some(channel_id) {
                    l.uudt_channel_id = None;
                }
                l.held_lock_mask = 0;
                // ADR-147: a dead link's error-suspension must never survive
                // into a reconnect -- cleared here alongside this function's
                // other per-CLL offline resets, via
                // `LogicalLinkState::clear_error_suspension`. No wake
                // needed: the CLL just went offline, so nothing is waiting
                // on its TX queue.
                //
                // `error_clear_seq` bumps unconditionally (third amendment,
                // capture-at-fold sequencing; unaffected by the fifth
                // amendment's split of the counter this bumps into
                // `error_clear_seq`/`error_set_seq` -- this call only ever
                // bumps the clear-specific half): a concurrent poll pass may
                // have already classified this CLL's frame batch before this
                // hard error landed, and that pass's end-of-pass writeback --
                // which still runs after this offline handling completes --
                // must not resurrect a stale `Suspend` classification onto a
                // link that just went offline. The unconditional bump here
                // invalidates any `Suspend` classification whose fold
                // predates this call (its captured `CllRxEntry::suspend_seq`
                // no longer matches the bumped `error_clear_seq`) while never
                // touching a classification whose fold happens later (it
                // captures the bumped seq itself). A Positive classification
                // still in flight from a pre-offline pass is harmless here
                // not because of any seq anchor -- this clear bumps
                // `error_clear_seq`, which gates only Suspend -- but because
                // applying `Positive` to the already-cleared flag is a
                // transition-free no-op, the `connect_generation` gate
                // discards old-session passes after reconnect, and
                // `finalize_connected_link`'s unconditional reset covers the
                // offline window (third amendment). A pre-offline Suspend is
                // the case this bump actually closes.
                l.clear_error_suspension();
                // Backlog fix (edge-case-hunter, Codex review round 22, ADR-165
                // PR #42): unlike `DisconnectComLogicalLink`/`DestroyComLogicalLink`,
                // this link's `repeat_message_ids` was left untouched here, so a
                // hard-errored CLL's stale `MsgId`s would survive into a later
                // reconnect on a fresh channel (no intervening Disconnect/Destroy)
                // and sit alongside any genuinely new slots it starts. Drained the
                // same way Disconnect's teardown drains it -- but with no
                // `api.stop_repeat_message` attempt of its own (moot: the device
                // connection is already gone, this loop doesn't even hold `api`)
                // -- pushed straight into `SharedChannel::leaked_repeat_message_ids`
                // instead, so `retry_leaked_repeat_message_stops`'s existing
                // opportunistic retry (already invoked at the top of every
                // sibling CLL's START/QUERY/STOP_REPEAT_MESSAGE call on this same
                // shared channel) picks them up the same way it already retries a
                // failed live STOP from Disconnect/Destroy -- including its own
                // ERR_INVALID_MSG_ID self-pruning for a slot the device already
                // forgot. `chans` is held across this entire function (this
                // function's own lock-order comment), so this lookup is
                // authoritative.
                //
                // Gated on `was_primary` (edge-case-hunter follow-up, captured
                // above before this closure clears `channel_id`), NOT on this
                // whole closure simply having matched the filter above:
                // `ioctl_start_repeat_message` only ever records a slot
                // against `link.channel_id` (the PRIMARY channel), never
                // `link.uudt_channel_id` (the dual-channel-mode companion,
                // ADR-046) -- a hard error on the companion channel ALONE
                // (this CLL's own primary `channel_id` staying alive and
                // untouched by the branch above) leaves any repeat message
                // this CLL owns genuinely still running, unaffected by the
                // companion's own failure; draining it here regardless of
                // which channel actually died would falsely orphan a still-
                // live, still-owned slot.
                if was_primary
                    && !l.repeat_message_ids.is_empty()
                    && let Some(sc) = l.channel_key.and_then(|key| chans.get_mut(&key))
                {
                    sc.leaked_repeat_message_ids
                        .append(&mut l.repeat_message_ids);
                }
                // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c Fix
                // B, design-advisor consult, Codex review round 3): the
                // structural sibling of the `repeat_message_ids` drain just
                // above, one physical resource over -- a live TP2.0
                // broadcast periodic this CLL owned is genuinely still
                // transmitting device-side, with no `api` held here to
                // attempt a native stop (the channel is dead; this function
                // never holds `self.api` at this point, matching why the
                // repeat-message drain above doesn't attempt a native call
                // here either). Pushed straight into the dead
                // `SharedChannel`'s own `leaked_periodic_message_ids` instead
                // -- mirrors `leaked_repeat_message_ids`'s own precedent
                // exactly. Gated on `was_primary` for the identical reason:
                // a broadcast periodic is only ever started against a link's
                // PRIMARY channel_id, never a UUDT companion -- `was_primary`
                // is vacuously true here whenever `tp20_broadcast_periodic`
                // is `Some` in the first place, since TP2.0 links never gain
                // a UUDT companion channel (UUDT companions are gated to
                // ISO15765-protocol links only, via `CanChannelMode::
                // applies_to` and `is_tp2_0_family_protocol_id` in
                // rpc_primitive.rs); if that invariant ever changes, this
                // gate needs revisiting. The owning COP still receives
                // `PduCopstCancelled` through this function's ordinary
                // per-link `cancel_link_cops` sweep below, unchanged -- that
                // sweep already cancels every COP on a dead link
                // generically.
                //
                // ADR-193 amendment (Codex review round 16, P1, PR #101):
                // the `None`-sentinel in-flight-start reservation is no
                // longer silently discarded here. `.take()` removes it from
                // `l.tp20_broadcast_periodic` regardless of which arm below
                // is taken -- this function still never holds `self.api` at
                // this point, so it cannot decide here whether a racing
                // start's native call is still in flight -- so the `None`
                // arm is recorded via `in_flight_start_taken` instead of
                // being dropped with no trace. A single batched `self.api`
                // fence acquisition below (after `links` drops but while
                // `chans` is still held) closes what was previously
                // documented as ADR-193's one accepted, unfenced residual;
                // see that acquisition's own doc comment for the full
                // rationale.
                if was_primary && let Some(periodic) = l.tp20_broadcast_periodic.take() {
                    match periodic.message_id {
                        Some(message_id) => {
                            if let Some(sc) = l.channel_key.and_then(|key| chans.get_mut(&key))
                                && !sc
                                    .leaked_periodic_message_ids
                                    .iter()
                                    .any(|&(id, _)| id == message_id)
                            {
                                sc.leaked_periodic_message_ids
                                    .push((message_id, periodic.started_epoch));
                            }
                        }
                        None => in_flight_start_taken = true,
                    }
                }
                *h
            })
            .collect();
        let resumed = recompute_lock_tx_suspensions(&mut links);
        let wake_targets: Vec<(u32, mpsc::UnboundedSender<TxItem>)> = resumed
            .into_iter()
            .filter_map(|h| {
                links
                    .get(&h)
                    .and_then(|l| l.channel_key)
                    .and_then(|ck| chans.get(&ck).map(|sc| (h, sc.tx_queue.clone())))
            })
            .collect();
        (cll_handles, wake_targets)
    };
    // ADR-193 amendment (Codex review round 16, P1, PR #101): closes the
    // one call site ADR-193 originally left as an accepted, unfenced
    // residual. A `None`-sentinel reservation taken above means a start's
    // own `revalidate_tp20_broadcast_periodic_reservation`
    // (`rpc_primitive.rs`) has not yet resolved -- briefly acquiring, then
    // immediately releasing, `self.api` here (no native call needed --
    // mirrors `terminate_tp20_broadcast_periodic_for_suspension`'s own
    // `None`-sentinel branch in `rpc_misc.rs`, which also issues no native
    // call) guarantees the opposite ordering, mirroring
    // `terminate_tp20_broadcast_periodic_for_suspension`'s own doc comment
    // (`rpc_misc.rs`) verbatim in shape: either the racing start has not yet
    // revalidated (and will now find its reservation gone -- taken above --
    // and abort without transmitting), or it already committed inside its
    // own `api` bracket and that bracket's own resolution has already
    // orphan-stopped the message it started (via
    // `finalize_or_orphan_broadcast_periodic_start_locked`). In the first
    // case this acquisition finds `self.api` uncontended and returns
    // immediately -- this function does NOT block waiting for that start to
    // finish; the start aborts independently, on its own schedule, whenever
    // it next reaches its own revalidation. This function only actually
    // blocks in the second case, until that start's own bracket releases
    // `self.api`. Either way, this acquisition happens strictly before this
    // function proceeds to report any CLL in this sweep's COPs terminal via
    // `cancel_link_cops` below. One batched acquire-then-release correctly
    // fences EVERY `None`-sentinel taken in this sweep, not just one CLL,
    // because every take happened-before this point, strictly before any
    // `cancel_link_cops` call in the loop that follows. Two
    // `handle_channel_hard_error` calls (for different dead channels) cannot
    // interleave with each other here either, since each holds `chans`
    // across its entire body (ADR-139).
    //
    // Lock order: `api` nests under the already-held `shared_channels`
    // (`chans`, still held here -- not dropped until after the per-CLL
    // loop below), sanctioned by ADR-080 (`shared_channels` is outermost;
    // `api`/`logical_links` may be acquired (and must be released)
    // underneath it). `logical_links` (`links`) is NOT held at this point
    // -- it was released when the block above ended -- so ADR-110's
    // `api`-outer/`logical_links`-inner ordering isn't in play for this
    // acquisition at all.
    if in_flight_start_taken {
        drop(ctx.api.lock().await);
    }
    for (h, tx_queue) in wake_targets {
        let _ = tx_queue.send(TxItem::ResumeWake { cll_handle: h });
    }
    for cll_h in cll_handles {
        send_error_event(
            subscriptions,
            logical_links,
            cll_h,
            PduErrorEvent::PduErrEvtLostCommToVci,
            None,
        )
        .await;
        cancel_link_cops(
            primitives,
            logical_links,
            subscriptions,
            terminal_cops,
            cll_h,
        )
        .await;
        send_cll_status(
            subscriptions,
            logical_links,
            cll_h,
            PduComLogicalLinkStatus::PduCllstOffline,
        )
        .await;
    }
    // `chans` is released here, before `module_state` is ever acquired
    // below -- this crate's established nested-lock order is `device_id ->
    // module_state -> shared_channels` (ADR-107 addendum, ADR-134), so
    // `shared_channels` must never still be held while acquiring
    // `module_state`.
    drop(chans);
    // Persist module-level state so GetStatus / GetEventItem, and a later
    // failing module-scoped RPC's ErrorDetail.error_event_data (ADR-105),
    // reflect the failure without requiring a SubscribeEvent listener.
    {
        let mut state = module_state.lock().await;
        state.status = PduModuleStatus::PduModstNotAvail;
        state.last_error = TrackedError {
            event: PduErrorEvent::PduErrEvtLostCommToVci,
            timestamp: module_timestamp_us(),
            cop: None,
            cop_tag: None,
        };
    }
    send_module_status(
        subscriptions,
        module_event_buf,
        PduModuleStatus::PduModstNotAvail,
    )
    .await;
    send_system_info(subscriptions, system_event_buf, PduInfo::ModuleListChg).await;
}

/// One UniqueRespIdTable entry's software-ISO-TP addressing info (ADR-046
/// addendum): the ECU's USDT response CAN ID/addressing (for parsing frames
/// coming from it) paired with the tester's physical request CAN
/// ID/addressing (for building the FlowControl frame sent back to it).
struct FcPair {
    /// `CP_CanRespUSDTId`: the ECU's response CAN ID.
    usdt_can_id: u32,
    /// ADR-222: `usdt_can_id`'s width gate, computed by `build_cll_rx_entries`
    /// from the same per-channel USDT-id contention map `UniqueRespIdKey::
    /// usdt_width_gate` uses (not a second, independently-computed
    /// contention set) -- `Any` unless `usdt_can_id` is configured at two
    /// different widths somewhere on this physical channel.
    usdt_width_gate: CanIdWidthGate,
    /// Addressing used to parse SF/FF/CF frames from `usdt_can_id`, decoded
    /// from `CP_CanRespUSDTFormat` / `CP_CanRespUSDTExtAddr`.
    rx_addressing: Addressing,
    /// `CP_CanPhysReqId`: where this service sends the FlowControl frame
    /// answering a FirstFrame from `usdt_can_id`.
    phys_req_can_id: u32,
    /// Addressing used to build that FlowControl frame, decoded from
    /// `CP_CanPhysReqFormat` / `CP_CanPhysReqExtAddr`.
    fc_tx_addressing: Addressing,
}

/// Snapshot of a software-ISO-TP CLL's RX parameters (ADR-046), taken by
/// `build_cll_rx_entries` alongside the routing table.
struct IsoTpRxContext {
    /// Shared per-`(CAN ID, is_29bit)` reassembly map
    /// (`LogicalLinkState::isotp_rx`) -- see that field's own doc comment
    /// for why width is part of the key (ADR-222).
    reassembly: Arc<Mutex<HashMap<(u32, bool), isotp::Reassembly>>>,
    /// One entry per UniqueRespIdTable row that names both a
    /// `CP_CanRespUSDTId` and a `CP_CanPhysReqId`.
    fc_pairs: Vec<FcPair>,
    /// BlockSize advertised in FlowControl frames this service sends.
    block_size: u8,
    /// STmin advertised in FlowControl frames this service sends.
    st_min: u8,
    framing: IsoTpFraming,
    /// N_Cr: how long an in-progress reassembly waits for the next
    /// ConsecutiveFrame before being abandoned.
    n_cr: Duration,
}

/// How frames on this channel reach this CLL (ADR-046).
enum RxEntryKind {
    /// The J2534 channel performs the protocol (hardware ISO15765 or a
    /// genuinely raw protocol): deliver frames as-is.
    ///
    /// `native_mixed` (ADR-160/Phase 3c; widened to the family by ADR-217):
    /// `true` iff this CLL's channel is an ISO15765-family, non-FD-substituted,
    /// unqualified (no pin selection, no `_CHx` Additional Channels index --
    /// ADR-217 Codex-review fix, PR #132 round 2) primary channel connected
    /// under either native-mixed sub-mode (`CanChannelMode::NativeMixed` or
    /// `CanChannelMode::NativeMixedAllFrames` -- SAE J2534-2 clause 8 enabled
    /// at connect time, either `ON` or `ALL_FRAMES`,
    /// `connect_new_physical_channel`). `false` for every other combination --
    /// a raw-CAN channel, an FD-substituted link, a qualified link, or any
    /// channel connected under one of the four non-native-mixed modes --
    /// which keeps this a provable no-op for them: the per-frame `ProtocolID`
    /// branch this field gates (`process_frame_for_entry`'s first
    /// `RxEntryKind::Hardware` arm) is only ever consulted when it is `true`,
    /// and its second arm's own `uudt_eligible = !native_mixed && !uudt_on_companion`
    /// gate is a no-op whenever this field is `false`.
    ///
    /// `uudt_on_companion` (ADR-222 round 4, design-advisor audit, PR #141):
    /// `true` iff this CLL's channel is a dual-channel-mode (ADR-046)
    /// primary -- i.e. it has its own separate UUDT companion channel
    /// (`LogicalLinkState::uudt_channel_id.is_some()`). A dual-channel
    /// primary channel never carries a `PASS_FILTER`/`FLOW_CONTROL_FILTER`
    /// for its own `CP_CanRespUUDTId` at all -- UUDT is only ever received
    /// on the separate companion channel (a distinct `RxEntryKind::Companion`
    /// entry) -- so this field disables the ISO-tagged arm's UUDT tier
    /// entirely on the primary (`process_frame_for_entry`'s own
    /// `uudt_eligible` gate), closing a latent (pre-existing, not introduced
    /// by ADR-222) bug: with the tier still live, an ISO-tagged frame could,
    /// depending on `UniqueRespIdTable` entry iteration order, wrongly match
    /// a USDT-role frame against an unrelated entry's own `CP_CanRespUUDTId`
    /// at the same numeric value before ever reaching the correct USDT
    /// entry in `matched`'s `find_map`. `false` for every other combination,
    /// a provable no-op for them (mirrors `native_mixed`'s own shape).
    Hardware {
        native_mixed: bool,
        uudt_on_companion: bool,
    },
    /// Dual-channel-mode companion raw-CAN channel: deliver only frames whose
    /// CAN ID matches a `CP_CanRespUUDTId` table entry.  Everything else on
    /// the bus (including the raw segments of USDT conversations happening on
    /// the ISO15765 channel) is dropped for this CLL.
    Companion,
    /// Software-ISO-TP raw-CAN channel: SF/FF/CF frames from USDT response
    /// IDs are reassembled in the service; UUDT-ID matches and unparseable
    /// frames are delivered raw; FlowControl frames are consumed.
    SoftwareIsoTp(IsoTpRxContext),
}

/// This pass's starting values for one registrant, captured in
/// `build_cll_rx_entries` at the same instant (same lock) as the
/// `CopRegistrant` clone itself is taken. Used only by the writeback merge
/// (ADR-101 Decision §A) to compute this pass's own delta contribution --
/// never read anywhere else, and deliberately not folded into
/// `CopRegistrant` (see `CllRxEntry::registrant_baselines`'s doc comment).
#[derive(Debug, Clone, Copy)]
struct RegistrantBaseline {
    cop_handle: u32,
    matches_got: u32,
    pending_rc: Option<u8>,
    /// `CopRegistrant::concat_segments_got` at pass start -- merged the same
    /// delta way as `matches_got` (ADR-148). Always `0` for a non-concat registrant.
    concat_segments_got: u32,
    /// `CopRegistrant::timing_accumulator` at pass start (ADR-150) -- used by
    /// `merge_registrant_writeback`'s baseline guard to distinguish "this
    /// pass's own snapshot actually changed the accumulator" from "this
    /// pass's snapshot is just carrying forward an unchanged prior value,"
    /// now that `timing_cfg` covers ISO15765 (with its UUDT companion
    /// channel, ADR-046) and not just UUDT-companion-free ISO14230.
    timing_accumulator: Option<TimingAccumulator>,
}

/// Per-CLL routing snapshot used by `poll_rx` and `poll_rx_and_check_match`.
///
/// **No `event_queue_cap`/`event_queue_mode`/`result_buffer_limit` fields
/// here (Codex review on PR #3, ADR-140 follow-up):** this snapshot is built
/// once per poll pass and can live across many `.await` points while frames
/// are routed and delivered, making it the STALEST of this crate's queue-
/// policy producers if it captured those fields ahead of time -- see
/// `CllEventQueue`'s own doc comment for the bug shape this avoids.
/// `deliver_or_enqueue` reads them fresh from `rx_buf` itself, under that
/// queue's own lock, at the point of use.
struct CllRxEntry {
    handle: u32,
    rx_buf: Arc<Mutex<CllEventQueue>>,
    /// One entry per `active_unique_resp_id_table` row that configures at
    /// least one CAN-ID-routable field (`CP_CanRespUSDTId`/
    /// `CP_CanRespUUDTId`/`CP_J1939SourceAddress`, ADR-184) --
    /// `UniqueRespIdKey`'s own doc comment covers the per-field semantics
    /// and `matched`'s precedence. Empty = no-table mode; every frame is
    /// delivered to this CLL.
    unique_resp_ids: Vec<UniqueRespIdKey>,
    kind: RxEntryKind,
    /// Selects the RX header-split policy `poll_rx_inner` applies (ADR-051):
    /// `CAN`/`ISO15765` split a CAN-ID(-plus-AE) header, `ISO14230` parses a
    /// variable 1-4 byte KWP2000 header, every other protocol is unaffected
    /// and keeps delivering the raw frame in `data_bytes` unchanged.
    ///
    /// Derived per `RxEntryKind` (ADR-171 follow-up, `build_cll_rx_entries`),
    /// not a single blanket `ChannelProtocol::j2534_protocol_id()` read: a
    /// `SoftwareIsoTp` entry still uses `l.protocol.j2534_protocol_id()` (the
    /// logical ISO15765 identity must govern, not the raw CAN
    /// `hw_protocol_id` underneath), while `Hardware`/`Companion` entries use
    /// `resources::base_protocol_id(l.hw_protocol_id)` instead -- required
    /// because for the `SAE_J1850` bus-agnostic resources,
    /// `l.protocol.j2534_protocol_id()` is only the VPW *initial probe
    /// candidate* (ADR-070) and never reflects a PWM auto-detect result,
    /// which would otherwise route a PWM-detected link's RX frames through
    /// J1850's VPW split arm and silently drop `ExtraDataIndex`-derived IFR
    /// footer bytes.
    header_protocol: u32,
    /// Mirrors `LogicalLinkState::raw_mode` (ADR-196 Decision item 3): when
    /// `true`, `poll_rx_inner`'s header/footer split call site skips
    /// `header_footer_len` entirely -- the whole received frame is
    /// delivered as `ResultData.data_bytes` and `extra_info` stays `None`,
    /// mirroring the shape a `header_protocol` this file does not split at
    /// all (SCI, `_ => (0, 0)`) already gets. `header_protocol` above still
    /// carries the CLL's real protocol identity even when this is `true` --
    /// `bind_frame`'s ADR-148 concat source-id derivation
    /// (`entry.header_protocol`) and every other `header_protocol` reader
    /// besides the split call site are unaffected by RawMode: that
    /// derivation already returns `None` for every protocol this file's
    /// RawMode allowlist (`rpc_link::rpc_create_com_logical_link`) admits
    /// except J1850 (whose own `header_bytes.len() == 3` guard is
    /// unaffected either, since `header_bytes` is forced empty under
    /// RawMode -- see the split call site just below), the same "not one of
    /// the KWP/J1850 families this derivation covers, or a RawMode CLL never
    /// has a nonempty header_bytes to match against" outcome it already
    /// produced before this crate had any RawMode support (ADR-196 Decision
    /// item 3's own reasoning: routing/matching already operate correctly on
    /// whichever buffer they are handed). ADR-200 (Phase 3)'s own SAE J1939
    /// RawMode RX support is the one exception with a REAL, protocol-specific
    /// change at the split call site itself (the native destination-address
    /// byte is dropped before `payload`/`raw_prefix` are computed) -- see
    /// `raw_j1939_rx_drop_destination_address`'s own doc comment.
    raw_mode: bool,
    /// ISO15765 only (empty otherwise): CAN ID -> `Addressing` for each
    /// configured `CP_CanRespUSDTId`, used to widen a raw (non-reassembled)
    /// USDT-routed delivery's header by the Address Extension byte when
    /// that CAN ID uses extended addressing (ADR-051). Split from
    /// `uudt_addressing_by_id` below (ADR-217 Codex-review fix, PR #132):
    /// under `CAN_MIXED_FORMAT_ALL_FRAMES` a single CAN ID can carry a
    /// `CP_CanRespUSDTId` entry AND a `CP_CanRespUUDTId` entry with
    /// DIFFERENT `Addressing` values (ADR-162's collision check, which
    /// would otherwise reject this, is deliberately not applied under
    /// `ALL_FRAMES` -- ADR-217 Decision item 3) -- a single flat
    /// `Vec<(u32, Addressing)>` covering both roles let either entry's
    /// `Addressing` answer for a delivery routed via the OTHER role,
    /// corrupting that delivery's payload by 1 byte. `poll_rx_inner`'s
    /// header-split call site selects this table for a delivery whose
    /// `uudt_routed` is `false`, `uudt_addressing_by_id` for `true` -- see
    /// `CllRxEntry::rx_addressing_table`.
    ///
    /// ADR-222: each entry also carries the id's own `CanIdWidthGate`
    /// (computed from the same per-channel USDT-id contention map
    /// `UniqueRespIdKey::usdt_width_gate` uses), so `header_footer_len`'s own
    /// lookup can pick the entry whose configured width matches the frame's
    /// actual width instead of the first/any numeric match -- otherwise a
    /// contended id with two different `Addressing` values (one per width)
    /// could have the wrong one answer for a given delivery.
    usdt_addressing_by_id: Vec<(u32, CanIdWidthGate, Addressing)>,
    /// The `CP_CanRespUUDTId` counterpart to `usdt_addressing_by_id` above
    /// -- see that field's own doc comment for why the two are kept
    /// separate rather than merged into one table.
    uudt_addressing_by_id: Vec<(u32, CanIdWidthGate, Addressing)>,
    /// `LogicalLinkState::connect_generation` (ADR-086), snapshotted in the
    /// same `logical_links` lock acquisition as every other field here.
    /// `poll_rx_inner`'s `bind_frame`/`bind_registrant` attribution compares
    /// this against each registrant's own call-time-captured generation
    /// before attributing a matched frame's `ResultData`/`cop_handle` to it
    /// -- `target_cll` equality alone is not enough, since a same-channel
    /// disconnect+reconnect of this CLL can complete between
    /// `wait_for_expected_response`'s own per-pass staleness check (loop
    /// bottom) and the next pass's attribution decision, with `.await`s
    /// (this function's own locks, `ctx.api.lock()`) in between (ADR-086,
    /// this round).
    connect_generation: u64,
    /// The `LogicalLinkState::error_clear_seq` value captured at the EXACT
    /// moment this pass's `queue_error_class` was last (re)written to
    /// `Some(QueueErrorClass::Suspend)` by `bind_frame` -- NOT snapshotted
    /// up front alongside `connect_generation` (ADR-147 fifth amendment,
    /// split direction-specific anchors; this field is the reinstated
    /// third-amendment `suspend_seq` field, Suspend-only again -- the
    /// fourth amendment's generalization to also cover `Positive` is
    /// reverted here, since `Positive` now anchors to `set_seq_at_read`
    /// below instead). `None` until the first `Suspend` fold of this pass;
    /// a later frame that does not REWRITE the classification to `Suspend`
    /// (an `Unbound` frame with nothing sighted, a `TesterPresent` match, or
    /// a `Positive` write -- which no longer touches this field at all)
    /// leaves an already-captured value untouched -- refreshing it on such a
    /// frame would wrongly extend a stale classification's validity past a
    /// legitimate intervening synchronous state change.
    ///
    /// Suspend's invalidation anchor is EXPOSURE, not fold time: an explicit
    /// clear can only invalidate content the client could have seen, and the
    /// capture site (`poll_rx_inner`, immediately after `bind_frame`
    /// returns) always runs strictly before this frame's delivery, so a
    /// clear landing in the fold-to-capture gap necessarily predates this
    /// frame's exposure and is a genuinely NEW incident, not a race to fix
    /// -- see the capture site's own comment and `LogicalLinkState::
    /// error_clear_seq`'s doc comment for the full argument. See the
    /// apply-gate check site (`poll_rx_inner`'s writeback block,
    /// `queue_error_class_to_apply`) for how this is compared against the
    /// live `error_clear_seq` at apply time.
    suspend_seq: Option<u64>,
    /// The `LogicalLinkState::error_set_seq` value captured BEFORE this
    /// pass's own `PassThruReadMsgs` call even starts (ADR-147 sixth
    /// amendment, pre-read capture for the batch anchor -- superseding the
    /// fifth amendment's construction-time capture, which still ran strictly
    /// AFTER `read_messages` returned, with a genuine intervening `.await`
    /// in between that a concurrent task's bump could land in). Captured by
    /// `poll_rx_inner` itself, under its own `logical_links` lock
    /// acquisition, fully released before `ctx.api` is acquired for the read
    /// (ADR-080; no nesting), then threaded into `build_cll_rx_entries` as
    /// `set_seq_snapshot` and stamped onto this field from that map -- see
    /// that function's own doc comment.
    ///
    /// This is `Positive`'s anchor: a positive response can only genuinely
    /// resume a suspension it postdates on the wire, and every frame in one
    /// `PassThruReadMsgs` batch physically arrived before that read call
    /// returned, so the true anchor is "the counter's value as of this
    /// batch's own read-completion". Exact "at read-completion" equality is
    /// structurally
    /// unachievable across two different async locks (`ctx.api` for the
    /// read, `ctx.logical_links` for the counter) -- no single instant can
    /// be inside both critical sections at once. What IS achievable is the
    /// inequality `T_capture <= T_read-start <= T_read-completion`, which is
    /// sufficient: any bump strictly after read-completion is guaranteed to
    /// be caught (mismatch, causing discard) BECAUSE it necessarily happened
    /// after this pre-read capture too. A bump landing between the pre-read
    /// capture and read-completion is absorbed (over-discard, fail-CLOSED,
    /// self-correcting on this CLL's next batch's positive response) rather
    /// than missed (fail-OPEN) -- see the capture site's own comment in
    /// `poll_rx_inner` for the full argument, including why moving the
    /// capture to "right after `read_messages` returns" does not fix this
    /// (that capture point has the identical gap, since it too requires
    /// `logical_links.lock().await` as its own yield point after
    /// read-return).
    ///
    /// `None` when this CLL was absent from the pre-read snapshot --
    /// connected in the narrow window between that snapshot and
    /// `build_cll_rx_entries` running this pass -- and never re-captured
    /// mid-pass; `queue_error_class_to_apply` discards any `Positive`
    /// classification whose anchor is `None` (no valid batch-anchor to
    /// compare), self-correcting on this CLL's next pass. See
    /// `queue_error_class_to_apply`'s own doc comment for the full apply-time
    /// comparison.
    set_seq_at_read: Option<u64>,
    /// This CLL's response-binding registrants (ADR-100 Decision §1),
    /// cloned from `LogicalLinkState::registrants` in the same lock
    /// acquisition as `connect_generation`/`tester_present_discard` above --
    /// closes the identical ADR-086 staleness window those two fields close,
    /// rather than reading `registrants` live from `logical_links` mid-pass
    /// (ADR-100 Decision §3). `poll_rx_inner`'s frame loop mutates this
    /// snapshot's `matches_got`/`pending_rc` bookkeeping in place as frames
    /// bind (`bind_frame`), then writes the accumulated result back to the
    /// live registrants once per poll pass, after the whole frame loop.
    registrants: Vec<CopRegistrant>,
    /// This pass's per-registrant baseline (ADR-101 Decision §A),
    /// index-aligned with `registrants` above -- captured under the SAME
    /// `logical_links` lock acquisition, from the SAME live values, as the
    /// `registrants` clone itself, so it reflects exactly what this pass's
    /// snapshot started from. Kept as a separate field rather than added to
    /// `CopRegistrant` itself: the baseline is only ever meaningful for the
    /// writeback merge below and would otherwise pollute the live registrant
    /// struct and its existing test constructors. Used by the writeback
    /// merge (`poll_rx_inner`, after the frame loop) to compute each pass's
    /// own delta contribution instead of overwriting live state with this
    /// pass's absolute snapshot values -- see `merge_registrant_writeback`'s
    /// own doc comment for why an absolute overwrite is wrong whenever a CLL
    /// has a UUDT companion channel (ADR-046) and so is served by two
    /// independent poll tasks racing this exact snapshot/mutate/writeback
    /// sequence.
    registrant_baselines: Vec<RegistrantBaseline>,
    /// This CLL's still-open tester-present response/TX-echo discard windows
    /// (`LogicalLinkState::open_tp_discards`, ADR-137 fourth Codex-review fix
    /// / round-4 restructure), filtered to `now < window.until` at snapshot
    /// time (ADR-088 amendment, widened by the ADR-088 second amendment,
    /// widened again by ADR-099: a window's mere presence proves only that
    /// the send which opened it succeeded -- not that it expected a
    /// response; non-empty `pos`/`neg` on an entry, see
    /// [`TesterPresentDiscard`]'s own doc, additionally proves that send
    /// expected a response (`CP_TesterPresentReqRsp == 1` at send time) and
    /// so is eligible for content/SOM discard, while `tx_can_id`-gated
    /// TX-side discard applies regardless). Not additionally gated on a live
    /// `l.active.tester_present_req_rsp() == 1` re-read, since that live read
    /// could drift from what was actually true at each entry's own send
    /// instant. Empty when no window is currently open on this CLL --
    /// including "tester-present not armed at all" -- matching the
    /// pre-ADR-088 "no response returned" behavior exactly (nothing is ever
    /// discarded). `bind_frame` discards a frame matching ANY entry, not just
    /// the first: each entry is an independently elicited send, not a
    /// precedence chain.
    tester_present_discard: Vec<TesterPresentDiscard>,
    /// `CP_SuspendQueueOnError` (ADR-147) bind-time classification, folded
    /// across this whole poll pass by `bind_frame`/`bind_registrant` with
    /// last-frame-wins semantics -- `None` means no frame this pass produced
    /// a classification (e.g. a pending-RC-handled negative response, or a
    /// frame that never reached the tier-1 scan at all), leaving the live
    /// `tx_suspended_by_error` flag untouched. Applied to the live CLL in the
    /// same end-of-pass critical section as `merge_registrant_writeback`.
    queue_error_class: Option<QueueErrorClass>,
    /// `CP_StartMsgIndEnable` (ADR-151), snapshotted from this CLL's own
    /// `l.active` at the same `build_cll_rx_entries` pass as every other
    /// Active-derived field here — gates whether a pure SOM indication frame
    /// reaches this entry's client at all.
    start_msg_ind_enable: bool,
    /// `CP_TransmitIndEnable` (ADR-151), same snapshot granularity as
    /// `start_msg_ind_enable` above — gates a pure TxDone indication frame.
    transmit_ind_enable: bool,
    /// ADR-204 Codex review (PR #116, round 3): every currently-live
    /// registrant's `CopEntry::cop_tag` on this CLL (`primitives`), captured
    /// HERE -- in the SAME `logical_links` + nested `primitives` critical
    /// section `build_cll_rx_entries` already uses to clone `registrants`
    /// from live state, above -- rather than resolved fresh, much later,
    /// once this pass's frame loop has actually decided a specific frame's
    /// own `cop_handle` (`bind_frame`/`bind_registrant`, which run purely
    /// against this already-frozen `registrants` snapshot and have no
    /// `primitives` access of their own). That later resolution point
    /// (`resolve_frame_cop_tag`'s call site in `poll_rx_inner`, and
    /// `deliver_concat_batch_if_live`'s per-delivery lookup) used to run
    /// after several intervening `.await` points had already elapsed this
    /// same pass (the `CP_SuspendQueueOnError` eager-publish re-lock,
    /// `deliver_concat_batch_if_live` itself, the eager `cyclic_deadline`
    /// writeback re-lock) -- each a window a concurrent
    /// `CancelComPrimitive`/link-teardown path could race to remove this
    /// `cop_handle`'s `primitives` entry in, even though the registrant
    /// match that eventually produces this `cop_handle` was decided from
    /// THIS exact snapshot. Keyed by `cop_handle` (unlike
    /// `registrant_baselines`, this is NOT index-aligned with `registrants`
    /// -- a plain map is simpler here since lookups are always by
    /// `cop_handle`, never by position). A `cop_handle` absent from this map
    /// at lookup time means "no tag to echo" -- either the COP genuinely has
    /// none, or its `primitives` entry was already gone even at snapshot
    /// time -- `resolve_frame_cop_tag` treats both the same way: as no tag to
    /// echo. This is not the same flattening bug ADR-205's fix removes from
    /// every live-state lookup elsewhere in this file: this map's absence is
    /// decided once, HERE, in the same critical section as the liveness
    /// check that produced `registrants` above, not re-derived later from a
    /// separately-locked, possibly-stale re-check. See `CopEntry::cop_tag`'s own doc
    /// comment for why an immutable-once-set tag makes an earlier capture at
    /// least as correct as a later one, and strictly more resilient against
    /// this exact removal race.
    cop_tags: HashMap<u32, Vec<u8>>,
}

impl CllRxEntry {
    /// Selects `usdt_addressing_by_id` or `uudt_addressing_by_id` for a
    /// delivery's own `uudt_routed` flag (`process_frame_for_entry`'s return
    /// tuple, ADR-184) -- the single place this selection rule lives (ADR-217
    /// Codex-review fix, PR #132). `header_footer_len`'s own `.any()` table
    /// lookup is unaffected by WHICH table it is handed; only the caller
    /// (`poll_rx_inner`'s header-split call site) needs to pick the right one
    /// per delivery, since a single physical CAN ID can carry both a USDT and
    /// a UUDT entry with different `Addressing` under `ALL_FRAMES`.
    fn rx_addressing_table(&self, uudt_routed: bool) -> &[(u32, CanIdWidthGate, Addressing)] {
        if uudt_routed {
            &self.uudt_addressing_by_id
        } else {
            &self.usdt_addressing_by_id
        }
    }
}

/// What `poll_rx_inner` needs to recognize -- and discard -- a tester-present
/// response, SOM herald, and TX-side echo for one CLL, built in
/// `build_cll_rx_entries` entirely from that CLL's own
/// `TesterPresentState::Armed::discard_until` (`DiscardWindow`)/`framed_data`
/// snapshot -- NOT from live Active state (ADR-088 second amendment reverted
/// the original ADR-088 amendment's "computed fresh from live Active state
/// every poll tick" design for `pos`/`neg` and the `CP_TesterPresentReqRsp ==
/// 1` gate: both were found to drift from send-time truth whenever a
/// `CoptUpdateparam` changed only a field excluded from `same_wire_behavior`,
/// since such a change never re-arms and so is never reflected in what was
/// actually sent). ADR-099 widened this further: the window itself now opens
/// on every successful send regardless of `CP_TesterPresentReqRsp`, so that
/// TX-side discard (`tx_can_id`) is available even when content/SOM discard
/// is not.
///
/// ADR-100 S7 confirmed this stays a dedicated struct rather than becoming a
/// field set on [`CopRegistrant`](crate::service::CopRegistrant): the two
/// have no cop_handle, dispatch action, or match-bookkeeping shape in common
/// (`CopRegistrant`'s `expected`/`rc_cfg`/`matches_needed`/`matches_got` all
/// have no tester-present counterpart, and a match here always **discards**
/// rather than delivering to a `cop_handle`) -- see ADR-100 Decision §3,
/// "Resolved (c)" for the full reasoning. `bind_frame` ranks a match against
/// this struct at step 3, permanently interleaved between the tier-1
/// non-vacuous and vacuous registrant scans; that ranking is final, not a
/// placeholder for a future registrant conversion.
struct TesterPresentDiscard {
    /// `CP_TesterPresentExpPosResp` as it was at the send that opened this
    /// window (`DiscardWindow::pos`), not the live value. Empty means "not
    /// configured"; never matches (mirrors `neg`'s own convention).
    pos: Vec<u8>,
    /// `CP_TesterPresentExpNegResp` as it was at that same send
    /// (`DiscardWindow::neg`), not the live value.
    neg: Vec<u8>,
    /// The physical CAN ID(s) tester-present is addressed to, when
    /// resolvable: `Some(first UniqueRespIdTable entry's CP_CanRespUSDTId/
    /// CP_CanRespUUDTId)` when addressing resolves to CAN-family *physical*
    /// (`can_functional == Some(false)`) with a non-empty table -- matched
    /// directly against the frame's own raw CAN ID (`frame_can_id`), never
    /// via a live `unique_resp_identifier` table lookup, since a later
    /// `SetUniqueRespIdTable` can reassign the same uid to a different CAN
    /// ID (see `ResolvedTesterPresent::target_can_ids`'s doc comment). Sourced
    /// from whichever arm/re-arm is currently active (`resolved.target_can_ids`,
    /// frozen at that arm's `CoptStartcomm`/`CoptUpdateparam`), not
    /// recomputed live -- both modes now re-arm identically on any
    /// wire-affecting `CoptUpdateparam` (ADR-084), so live and armed never
    /// diverge. `None` for functional (broadcast) addressing -- every ECU
    /// that answers a functional tester-present is a legitimate reply, so
    /// nothing is restricted -- and for non-CAN-family/no-table CLLs.
    target_can_ids: Option<TesterPresentTargetCanIds>,
    /// Tester-present's own frozen outgoing TX CAN ID -- the leading 4 bytes
    /// of the armed send's `framed_data` -- used to recognize a
    /// TX_DONE/TX_INDICATION or CONFIG_LOOPBACK/TX_MSG_TYPE echo of
    /// tester-present's own send (ADR-099), as opposed to `target_can_ids`
    /// above, which is the RESPONSE-side CAN ID(s) used to recognize the
    /// ECU's actual reply (and its SOM herald). Deliberately the OPPOSITE of
    /// `target_can_ids`'s `None`-means-unrestricted convention (Codex review
    /// finding on the PR that introduced this field): `target_can_ids ==
    /// None` legitimately means "every routed ECU is a valid reply" under
    /// functional/broadcast addressing, a real "no restriction" semantic;
    /// `tx_can_id` has no analogous broadcast case -- our own echo always
    /// bears exactly one TX CAN ID for a CAN-family protocol, so its `None`
    /// only ever means "not CAN-family, no TX CAN ID exists to compare
    /// against at all." Treating that as "unrestricted" would silently
    /// discard EVERY `TX_MSG_TYPE`/`TX_INDICATION` frame on a non-CAN-family
    /// armed CLL for the life of each discard window -- including a genuine
    /// TxDone/loopback indication for a completely unrelated, concurrent
    /// client send on that same CLL, which ADR-098 says must reach the
    /// client as ordinary traffic (indication frames are never claimed by
    /// `bind_frame`'s registrant scan, so nothing else would protect it).
    /// `None` here therefore means arm (c) never fires at all -- see the `is_some_and`
    /// (not `is_none_or`) check at its use site.
    tx_can_id: Option<u32>,
}

/// The `TesterPresentDiscard`/`ResidualTesterPresentDiscard` `tx_can_id`
/// derivation formula: the leading 4 bytes of `framed_data` (tester-present's
/// own already-SF-framed wire payload) when CAN addressing was functional,
/// `None` otherwise (see `TesterPresentDiscard::tx_can_id`'s own doc comment
/// for why `None` is not treated as "unrestricted" here). Factored out
/// (ADR-137 second Codex-review fix) so `build_cll_rx_entries`'s
/// discard-matcher lookup and every `ResidualTesterPresentDiscard`-freezing
/// write site (`dispatch_due_tester_present`, `handle_start_comm`,
/// `handle_update_param`'s re-arm) all compute it identically rather than
/// duplicating the formula.
pub(super) fn tester_present_tx_can_id(
    resolved: &rpc_primitive::ResolvedTesterPresent,
    framed_data: &[u8],
) -> Option<u32> {
    (resolved.can_functional.is_some() && framed_data.len() >= 4)
        .then(|| u32::from_be_bytes(framed_data[0..4].try_into().unwrap()))
}

/// Defensive cap on `LogicalLinkState::open_tp_discards`'s length (ADR-137
/// fourth Codex-review fix / round-4 restructure). The structural bound is
/// "open windows <= overlapping `CP_P2Max` spans between P3-gated sends,"
/// typically 1-2 -- this should never actually bind in practice, but bounds
/// the list unconditionally rather than trusting that invariant to hold
/// under every future change to this mechanism.
/// Prunes expired entries out of `open_tp_discards` (`retain(|r| now <
/// r.window.until)`), then, if `entry` is `Some`, either extends an existing
/// entry's deadline (when one already open has the identical matching
/// signature -- `pos`/`neg`/`target_can_ids`/`tx_can_id`, everything but
/// `until`) or appends `entry` as a genuinely new signature. Codex review
/// round 5 finding: an earlier version of this function bounded the list
/// with a small numeric cap (`OPEN_TP_DISCARDS_CAP = 8`), FIFO-dropping the
/// oldest entry once at capacity -- but a long `CP_P2Max` combined with
/// frequent same-`same_wire_behavior`-excluded-field promotions (or just a
/// short `CP_TesterPresentTime` interval) can legitimately keep more than 8
/// entries open at once, and dropping an UNEXPIRED entry to make room
/// silently reopens the exact leak this whole mechanism exists to close (a
/// delayed reply to an earlier, still-in-window send would then be
/// delivered as ordinary traffic). Deduplicating by signature is the correct
/// bound instead: repeated identical-signature sends (by far the common
/// case -- most `CoptUpdateparam`s change nothing tester-present-relevant,
/// and the per-tick sender's own signature is stable between re-arms) never
/// grow the list at all, and the list's actual size is bounded by the
/// number of DISTINCT signatures a client causes within one `CP_P2Max` span
/// -- unusual, but never something this function may respond to by
/// discarding a still-open, distinctly-signed window. No cap; correctness
/// over a defensive bound with no safe value. Shared by all three
/// `open_tp_discards` push sites (`dispatch_due_tester_present`,
/// `handle_start_comm`, `handle_update_param`'s re-arm) so
/// prune-then-dedup-then-push is applied identically everywhere; every call
/// site invokes this under the exact same `logical_links` lock acquisition
/// as the `tester_present_state` write it accompanies, so a push can never
/// straddle two different states.
fn push_open_tp_discard(
    open_tp_discards: &mut Vec<ResidualTesterPresentDiscard>,
    now: tokio::time::Instant,
    entry: Option<ResidualTesterPresentDiscard>,
) {
    open_tp_discards.retain(|r| now < r.window.until);
    if let Some(entry) = entry {
        if let Some(existing) = open_tp_discards.iter_mut().find(|r| {
            r.window.pos == entry.window.pos
                && r.window.neg == entry.window.neg
                && r.target_can_ids == entry.target_can_ids
                && r.tx_can_id == entry.tx_can_id
        }) {
            existing.window.until = existing.window.until.max(entry.window.until);
        } else {
            open_tp_discards.push(entry);
        }
    }
}
#[path = "events_rx_routing.rs"]
mod rx_routing;
use rx_routing::*;

#[path = "events_j1939_claim.rs"]
mod j1939_claim;
// `pub(super)` (not plain `use`, unlike `rx_routing` above): `rpc_link.rs`'s
// disconnect/destroy teardown needs `cancel_j1939_claims_for_cll` reachable
// as `events::cancel_j1939_claims_for_cll`, the same external-visibility
// shape `cancel_link_cops`/`send_cll_status` (defined directly in this
// file) already have -- mirroring those rather than `rx_routing`'s
// (internal-only) shape.
pub(super) use j1939_claim::*;

#[path = "events_tp20_connection.rs"]
mod tp20_connection;
// `pub(super)` (Codex review fix, PR #97, round 14 -- previously a plain,
// internal-only `use`, the same shape `rx_routing` above still uses):
// `rpc_link.rs`'s `DisconnectComLogicalLink`/`DestroyComLogicalLink`
// teardown call sites still invoke
// `j2534_0404::J2534Api0404::tp20_teardown_connection` directly (ADR-188/
// Phase 7 Stage 7a), but now ALSO need
// `quarantine_tp20_connection_for_orphaned_write_back` reachable as
// `events::quarantine_tp20_connection_for_orphaned_write_back` -- the same
// external-visibility shape `cancel_j1939_claims_for_cll`
// (`j1939_claim`, above) already has, for the identical reason.
pub(super) use tp20_connection::*;

/// Per-message frame-identity fields `process_frame_for_entry` needs,
/// bundled into one small `Copy` struct purely to keep that function under
/// clippy's `too_many_arguments` limit (ADR-203, edge-case-hunter finding
/// this PR) -- mirrors `FrameContext`'s own precedent below for
/// `bind_frame`; no behavior or ownership change from passing the same
/// four values individually. `is_content_frame` (Codex review finding, this
/// PR) is `poll_rx_inner`'s own `is_content_frame` (see that binding's doc
/// comment) -- threaded through only as far as `route_frame`'s new KWP/J1850
/// SA branch, which must not run for a non-content (indication) frame; see
/// `route_frame`'s own doc comment for why.
#[derive(Clone, Copy)]
struct FrameRouteContext {
    frame_can_id: Option<u32>,
    frame_source_addr: Option<u8>,
    frame_protocol_id: u32,
    is_content_frame: bool,
    /// ADR-222: this frame's own `RxStatus` bit 8 (`CAN_29BIT_ID_STATUS`),
    /// read once per message by `poll_rx_inner` alongside `frame_can_id` --
    /// threaded to every numeric-CAN-ID comparison site so a contended id
    /// (the same numeric id configured at two different widths on this
    /// physical channel) resolves to the entry whose configured width
    /// actually matches this frame, instead of the first/any numeric match.
    frame_is_29bit: bool,
}

/// Runs one received frame through `entry`'s routing/transport processing and
/// returns the messages to deliver to that CLL as `(data, unique_resp_id,
/// raw, uudt_routed)` 4-tuples — `data` in the same `[4-byte CAN
/// ID][payload]` layout the hardware ISO15765 path produces, so downstream
/// matching and event encoding are mode-agnostic. `raw` is `true` when
/// `data` is a frame taken directly off the wire (an Address Extension byte,
/// if extended addressing is configured, is still embedded at `data[4]`) and
/// `false` when it is an already-reassembled software ISO-TP payload (any AE
/// byte was already consumed during reassembly) — `poll_rx_inner`'s ADR-051
/// header split consults this to avoid mistaking a reassembled payload's
/// first byte for an AE byte. `uudt_routed` (ADR-150) is `true` iff this
/// delivery was routed via a `CP_CanRespUUDTId` match rather than
/// `CP_CanRespUSDTId` or a no-table wildcard — see `route_frame_with_role`'s
/// own doc comment (ADR-217 Codex-review fix, PR #132 round 2: this arm's
/// production call site moved from `route_frame`/`route_frame_matched_uudt`'s
/// pairing to `route_frame_with_role`'s single combined search).
///
/// For `SoftwareIsoTp` entries this is where reassembly happens: FirstFrames
/// answer with a FlowControl frame (written via `api` when the
/// UniqueRespIdTable pairs the sender with a `CP_CanPhysReqId`), and only
/// complete messages are returned.  FlowControl frames are consumed here —
/// the TX driver captures the ones it needs before fan-out (`FcCapture`).
///
/// `frame_protocol_id` (ADR-160/Phase 3c): the frame's own native
/// `PASSTHRU_MSG.ProtocolID`, read once per message by `poll_rx_inner`
/// alongside `frame_can_id`. Only consulted by the `Hardware` arm, and only
/// when that entry's own `native_mixed` is `true` -- every other arm/mode
/// ignores it entirely, which is what makes this a provable no-op for the
/// three pre-existing modes (see `RxEntryKind::Hardware`'s own doc comment).
///
/// `frame_source_addr` (ADR-203): this frame's own KWP/J1850 source-address
/// byte (`kline_j1850_source_addr`), also read once per message by
/// `poll_rx_inner` alongside `frame_can_id`/`frame_protocol_id`. Threaded
/// only to the `route_frame_with_role` call site below (`RxEntryKind::
/// Hardware`'s ISO-tagged/non-native-mixed arm) -- `route_frame_uudt_only`
/// never needs it, since KWP/J1850 CLLs never use the UUDT companion-channel
/// path.
///
/// `frame` bundles `frame_can_id`/`frame_source_addr`/`frame_protocol_id`/
/// `is_content_frame` into one small `Copy` struct (ADR-203, edge-case-hunter
/// and Codex review findings this PR) purely to keep this function under
/// clippy's `too_many_arguments` limit -- mirrors `FrameContext`'s own
/// precedent above for `bind_frame`; no behavior change from passing the
/// same four values individually.
async fn process_frame_for_entry(
    api: &Arc<Mutex<J2534Api0404>>,
    channel_id: ChannelId,
    entry: &CllRxEntry,
    frame: FrameRouteContext,
    data: &[u8],
    is_tx_side: bool,
) -> Vec<(Vec<u8>, u32, bool, bool)> {
    match &entry.kind {
        RxEntryKind::Hardware { native_mixed, .. }
            if *native_mixed
                && resources::base_protocol_id(frame.frame_protocol_id) != j2534_0404::ISO15765 =>
        {
            // SAE J2534-2 clause 8 (ADR-160): this frame carries a raw/
            // unformatted CAN native ProtocolID (a PASS_FILTER match, not a
            // FLOW_CONTROL_FILTER-built ISO15765 message) -- route exactly
            // like a dual-channel-mode companion channel's own UUDT-only
            // semantics (`route_frame_uudt_only`'s contract): delivered only
            // on a `CP_CanRespUUDTId` match, dropped otherwise, always
            // UUDT-routed (ADR-150).
            match route_frame_uudt_only(entry, frame.frame_can_id, frame.frame_is_29bit) {
                Some(uid) => vec![(data.to_vec(), uid, true, true)],
                None => Vec::new(),
            }
        }
        RxEntryKind::Hardware {
            native_mixed,
            uudt_on_companion,
        } => {
            // ADR-217 Codex-review fix, PR #132 round 2, widened by ADR-222
            // round 4 (design-advisor audit, PR #141): `uudt_eligible =
            // !native_mixed && !uudt_on_companion`. `!native_mixed`'s own
            // reasoning is unchanged -- a native-mixed entry's own UUDT ids
            // are only ever legitimately delivered via the raw-CAN-tagged
            // first arm above (a `PASS_FILTER` match); this ISO-tagged arm
            // reaches here when the frame is NOT raw (either `native_mixed`
            // is false and ANY tag lands here, or it's true and the frame IS
            // ISO15765-tagged, per the first arm's own guard), so a
            // `native_mixed` entry's UUDT tier must never match here -- an
            // ISO-tagged frame whose CAN ID equals this entry's own UUDT id
            // is always some OTHER entry's USDT interpretation of a
            // collision `ALL_FRAMES` now accepts (ADR-217 Decision item 8),
            // never a delivery for THIS entry. `!uudt_on_companion` closes a
            // separate, pre-existing latent bug (ADR-222 round 4): on a
            // dual-channel-mode primary, no UUDT `FLOW_CONTROL_FILTER`/
            // `PASS_FILTER` exists at all (UUDT is only ever received via
            // the separate ADR-046 companion channel, a distinct
            // `RxEntryKind::Companion` entry), so this tier being live on
            // the primary is structurally dead weight that can, depending on
            // `UniqueRespIdTable` entry iteration order, wrongly match a
            // USDT-role frame against an unrelated entry's own
            // `CP_CanRespUUDTId` at the same numeric value before ever
            // reaching the correct USDT entry in `matched`'s `find_map`.
            // `route_frame_with_role` resolves the uid and `uudt_routed`
            // from ONE search (not two independently-computed ones,
            // `route_frame`'s old `+ route_frame_matched_uudt` pairing) so
            // they cannot disagree -- see its own doc comment.
            match route_frame_with_role(
                entry,
                frame.frame_can_id,
                frame.frame_source_addr,
                frame.is_content_frame,
                is_tx_side,
                !*native_mixed && !*uudt_on_companion,
                frame.frame_is_29bit,
            ) {
                Some((uid, uudt_routed)) => vec![(data.to_vec(), uid, true, uudt_routed)],
                None => Vec::new(),
            }
        }
        // Only ever reached via a `CP_CanRespUUDTId` match (`route_frame_uudt_only`'s
        // own contract) -- always UUDT-routed (ADR-150).
        RxEntryKind::Companion => {
            match route_frame_uudt_only(entry, frame.frame_can_id, frame.frame_is_29bit) {
                Some(uid) => vec![(data.to_vec(), uid, true, true)],
                None => Vec::new(),
            }
        }
        RxEntryKind::SoftwareIsoTp(ctx) => {
            let Some(can_id) = frame.frame_can_id else {
                return Vec::new(); // shorter than a CAN ID — not a CAN frame
            };
            // Routing: UUDT matches bypass ISO-TP and deliver raw; USDT
            // matches (or the empty-table wildcard) go through reassembly;
            // anything else is dropped, same as the hardware path.
            let (uid, usdt_width_gate) = if entry.unique_resp_ids.is_empty() {
                // No-table wildcard: no configured entry, hence no width
                // awareness at all -- canonicalize the same as an
                // uncontended `Any` gate (`reassembly_key_width` below),
                // matching this arm's pre-ADR-222 CAN-ID-only behavior.
                (0, CanIdWidthGate::Any)
            } else if let Some(uid) =
                route_frame_uudt_only(entry, frame.frame_can_id, frame.frame_is_29bit)
            {
                // ADR-150: explicit UUDT-id match -- UUDT-routed.
                return vec![(data.to_vec(), uid, true, true)];
            } else if let Some(key) = entry.unique_resp_ids.iter().find(|key| {
                // ADR-222: gated by this entry's own USDT width gate --
                // contended-id collision resolution, same rule the hardware
                // path's `UniqueRespIdKey::matched` applies.
                key.can_resp_usdt_id == Some(can_id)
                    && key.usdt_width_gate.accepts(frame.frame_is_29bit)
            }) {
                (key.unique_resp_identifier, key.usdt_width_gate)
            } else {
                return Vec::new();
            };
            // Every remaining return in this arm stems from the USDT match
            // (or the empty-table wildcard) above, never from a UUDT match
            // (that already returned above) -- ADR-150: not UUDT-routed.

            // ADR-222 (Codex review finding, PR #141): the reassembly map's
            // key uses the MATCHED ENTRY's own canonical width
            // (`reassembly_key_width`), never the raw per-frame
            // `frame.frame_is_29bit` bit directly -- see that method's own
            // doc comment for why an uncontended id must not fragment its
            // reassembly state across a device's per-frame RxStatus bit-8
            // noise.
            let reassembly_key = (can_id, usdt_width_gate.reassembly_key_width());

            // Addressing for frames from `can_id`: the paired table entry's
            // CP_CanRespUSDTFormat/ExtAddr when known, else Normal — matching
            // the pre-extended-addressing behaviour for entries that only
            // set a CAN ID (ADR-046 addendum).
            // ADR-222: gated the same way as the USDT `find` just above --
            // a contended USDT id must not pair a reassembly with the WRONG
            // width's own `CP_CanPhysReqId`/addressing.
            let pair = ctx.fc_pairs.iter().find(|p| {
                p.usdt_can_id == can_id && p.usdt_width_gate.accepts(frame.frame_is_29bit)
            });
            let rx_addressing = pair.map_or(Addressing::Normal, |p| p.rx_addressing);

            let payload = &data[4..];
            let now = Instant::now();
            match isotp::parse_frame(payload, rx_addressing) {
                Some(isotp::Frame::Single { payload }) => {
                    let mut msg = can_id.to_be_bytes().to_vec();
                    msg.extend_from_slice(payload);
                    vec![(msg, uid, false, false)]
                }
                Some(isotp::Frame::First { total_len, payload }) => {
                    // Answer with a FlowControl (ContinueToSend) when the
                    // table names the ECU's physical request ID; without a
                    // pair the ECU may still proceed if it was configured
                    // with BS=0 out of band.
                    if let Some(pair) = pair {
                        let fc_dest = pair.phys_req_can_id;
                        let mut fc = fc_dest.to_be_bytes().to_vec();
                        let mut fc_payload = isotp::flow_control_frame(
                            isotp::FS_CONTINUE_TO_SEND,
                            ctx.block_size,
                            ctx.st_min,
                            pair.fc_tx_addressing,
                        );
                        if ctx.framing.pad {
                            isotp::pad_frame(&mut fc_payload, ctx.framing.filler);
                        }
                        fc.extend_from_slice(&fc_payload);
                        let tx_flags = if fc_dest > 0x7FF {
                            j2534_0404::TX_EXTENDED_ID
                        } else {
                            0
                        };
                        let write_result = {
                            let api = api.lock().await;
                            j2534_0404::PassThruMessage::new(
                                j2534_0404::CAN,
                                0,
                                tx_flags,
                                0,
                                0,
                                &fc,
                            )
                            .and_then(|mut msg| {
                                api.write_messages(channel_id, std::slice::from_mut(&mut msg), 0)
                                    .map(|_| ())
                            })
                        };
                        if let Err(err) = write_result {
                            warn!(can_id, fc_dest, %err, "software ISO-TP: failed to send FlowControl frame");
                        }
                    } else {
                        debug!(
                            can_id,
                            "software ISO-TP: FirstFrame without a CP_CanPhysReqId pair; no FlowControl sent"
                        );
                    }
                    let mut reassembly = ctx.reassembly.lock().await;
                    // Purge reassemblies whose N_Cr expired without a final
                    // CF, so abandoned transfers don't accumulate.
                    reassembly.retain(|_, r| !r.is_expired(now));
                    // ADR-222: keyed by (can_id, canonical width), not can_id
                    // alone -- see `IsoTpRxContext::reassembly`'s and
                    // `CanIdWidthGate::reassembly_key_width`'s own doc
                    // comments for why this uses `reassembly_key` (the
                    // matched entry's own gate), never the raw
                    // `frame.frame_is_29bit` bit.
                    reassembly.insert(
                        reassembly_key,
                        isotp::Reassembly::start(total_len, payload, now, ctx.n_cr),
                    );
                    Vec::new()
                }
                Some(isotp::Frame::Consecutive {
                    sequence_number,
                    payload,
                }) => {
                    let mut reassembly = ctx.reassembly.lock().await;
                    // ADR-222: same `reassembly_key` as the FirstFrame insert
                    // above.
                    let key = reassembly_key;
                    let Some(state) = reassembly.get_mut(&key) else {
                        return Vec::new(); // stray CF with no reassembly in progress
                    };
                    match state.on_consecutive(sequence_number, payload, now) {
                        isotp::ReassemblyStep::Complete(msg_payload) => {
                            reassembly.remove(&key);
                            let mut msg = can_id.to_be_bytes().to_vec();
                            msg.extend_from_slice(&msg_payload);
                            vec![(msg, uid, false, false)]
                        }
                        isotp::ReassemblyStep::Continue => Vec::new(),
                        isotp::ReassemblyStep::WrongSequence => {
                            warn!(
                                can_id,
                                sequence_number,
                                "software ISO-TP: wrong ConsecutiveFrame sequence; reassembly aborted"
                            );
                            reassembly.remove(&key);
                            Vec::new()
                        }
                        isotp::ReassemblyStep::TimedOut => {
                            warn!(can_id, "software ISO-TP: N_Cr expired; reassembly aborted");
                            reassembly.remove(&key);
                            Vec::new()
                        }
                    }
                }
                // Stale FlowControl outside a TX window: consume silently.
                Some(isotp::Frame::FlowControl { .. }) => Vec::new(),
                // Not ISO-TP — deliver raw (raw-CAN traffic on the shared channel).
                None => vec![(data.to_vec(), uid, true, false)],
            }
        }
    }
}

/// ADR-051: how many of `data`'s leading/trailing bytes are protocol
/// header/footer, to be split off into `ResultData.extra_info.header_bytes`
/// / `footer_bytes` instead of `data_bytes`. Returns `(0, 0)` for SCI and
/// any other protocol this does not apply to (`data_bytes` stays the raw
/// frame, `extra_info` stays `None`).
///
/// | Protocol | Header | Footer |
/// |---|---|---|
/// | CAN | 4 bytes of CAN ID | — |
/// | ISO15765 | 4 bytes of CAN ID | — |
/// | ISO15765 (extended addressing) | 4 bytes of CAN ID, 1 Address Extension byte | — |
/// | ISO9141 / ISO14230 | 1-4 bytes, parsed from the KWP2000 format byte (or a fixed 3 bytes for CARB/ISO9141-2 exception addressing, ADR-167) | whatever trails the header's own declared payload length (e.g. a 1-byte checksum) for the two self-describing address modes; always empty for CARB (ADR-167) |
/// | J1850PWM | 3 bytes (format/priority + target + source) | the native `ExtraDataIndex`-reported trailing IFR byte count (0 or more), defensively clamped (ADR-171) |
/// | J1850VPW | 3 bytes (format/priority + target + source) | always empty -- VPW has no IFR mechanism, `ExtraDataIndex` is ignored unconditionally (ADR-171) |
///
/// `raw` (see `process_frame_for_entry`) gates the ISO15765 extended-
/// addressing widening: only a frame taken directly off the wire can still
/// have an AE byte at `data[4]` to report — an already-reassembled software
/// ISO-TP payload never does, so it always gets the plain 4-byte header.
///
/// ISO15765 extended-addressing detection (ADR-197): a frame is treated as
/// extended-addressed if EITHER its CAN ID has an `addressing_by_id` table
/// entry marked `Addressing::Extended`, OR the frame's own native RxStatus
/// bit 7 (`ISO15765_ADDR_TYPE`, SAE J2534-1 §8.7.1 Figure 43, passed in as
/// `rx_ext_addr`) is set — never the table lookup alone. The table lookup is
/// the only signal for UUDT/raw-CAN-read deliveries (no ISO15765 layer ever
/// sets the RxStatus bit for those); the RxStatus bit is the only signal for
/// a "no-table wildcard" CLL that has no `SetUniqueRespIdTable` entry for
/// the CAN ID at all.
///
/// `addressing_by_id` (ADR-217 Codex-review fix, PR #132): the CALLER selects
/// which of `CllRxEntry`'s two role-specific tables
/// (`usdt_addressing_by_id`/`uudt_addressing_by_id`, via
/// `CllRxEntry::rx_addressing_table`) to pass, based on the delivery's own
/// `uudt_routed` -- this function itself is role-agnostic, it just looks up
/// whatever table it is handed. This matters because under
/// `CAN_MIXED_FORMAT_ALL_FRAMES` (and, latently, under `DualChannel`/
/// `SoftwareIsoTp`, ADR-217's Consequences) a single CAN ID can carry a
/// `CP_CanRespUSDTId` entry and a `CP_CanRespUUDTId` entry with DIFFERENT
/// `Addressing` values -- a table that answered for both roles at once would
/// let one role's `Addressing` corrupt a delivery routed via the other role.
///
/// Returns `(data.len(), 0)` degenerately when a frame is too short to
/// contain the header its own leading bytes claim (e.g. an ISO14230 frame
/// naming a separate length byte beyond the frame's actual length) — the
/// whole frame is reported as header rather than risking an out-of-bounds
/// slice or misreading unrelated bytes as payload.
fn header_footer_len(
    protocol_id: u32,
    addressing_by_id: &[(u32, CanIdWidthGate, Addressing)],
    data: &[u8],
    raw: bool,
    extra_data_index: Option<usize>,
    rx_ext_addr: bool,
    frame_is_29bit: bool,
) -> (usize, usize) {
    match protocol_id {
        j2534_0404::CAN => (data.len().min(4), 0),
        j2534_0404::ISO15765 => {
            if data.len() < 4 {
                return (data.len(), 0);
            }
            let extended = raw && {
                let table_says_extended = {
                    let can_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                    // ADR-222: `gate.accepts(frame_is_29bit)` picks the entry
                    // whose configured width matches this frame's actual
                    // width -- otherwise a contended id with two entries at
                    // different widths (and different `Addressing`) could
                    // have the WRONG width's own `Addressing` answer for
                    // this frame. A no-op on every uncontended id (gate
                    // `Any` unconditionally accepts).
                    addressing_by_id.iter().any(|&(id, gate, addressing)| {
                        id == can_id
                            && gate.accepts(frame_is_29bit)
                            && matches!(addressing, Addressing::Extended(_))
                    })
                };
                rx_ext_addr || table_says_extended
            };
            (if extended && data.len() >= 5 { 5 } else { 4 }, 0)
        }
        j2534_0404::ISO9141 | j2534_0404::ISO14230 => match kwp_header_and_payload_len(data) {
            Some((header, payload_len)) if header + payload_len <= data.len() => {
                (header, data.len() - header - payload_len)
            }
            _ => (data.len(), 0),
        },
        j2534_0404::J1850PWM => {
            // ADR-171: the footer is whatever `ExtraDataIndex` reports as
            // trailing IFR data, not a fabricated 1-byte CRC -- J1850's wire
            // checksum is already verified/stripped by the interface before
            // delivery. `ExtraDataIndex` crosses the FFI boundary from an
            // untrusted vendor DLL (or, in tests, the mock), so any value
            // outside `header..=data.len()` is treated as absent rather than
            // trusted: the whole frame is delivered as payload instead of
            // producing a nonsensical split.
            let header = data.len().min(3);
            let footer = match extra_data_index {
                Some(edi) if header <= edi && edi <= data.len() => data.len() - edi,
                _ => 0,
            };
            (header, footer)
        }
        j2534_0404::J1850VPW => {
            // ADR-171/Codex review round 1: VPW has no IFR mechanism at all
            // (unlike PWM), so `ExtraDataIndex` is never consulted here even
            // when in-range -- a non-conforming or stale adapter reporting an
            // in-range-but-wrong value must not be trusted to mean "real IFR
            // bytes" for a protocol that has none. Footer is unconditionally
            // empty; the whole remainder after the header is payload.
            (data.len().min(3), 0)
        }
        // ADR-179/Phase 5 (Codex review, PR #72 round 1): SAE J1939's own
        // 5-byte prefix (`tx_header::j1939_header_bytes`'s TX composition --
        // 4-byte CAN identifier plus a 1-byte destination address) was never
        // mirrored on the RX side here, so this arm fell through to the `_`
        // catch-all and stripped nothing: every ordinary J1939 response
        // exposed its own CAN-ID/destination prefix as if it were payload,
        // breaking expected-response/RC-byte-offset matching (which run
        // against `ResultData.data_bytes`, ADR-051) for every receive. No
        // footer -- clause 16 defines no trailing IFR-style data.
        j2534_0404::PROTOCOL_J1939_PS => (data.len().min(5), 0),
        // ADR-188/Phase 7 Stage 7a: TP2.0's own established 4-byte TX-ID
        // prefix (`tx_header.rs`'s TX composition), the same plain
        // fixed-length-header/no-footer shape as `j2534_0404::CAN`'s own
        // arm above -- clause 19 defines no protocol-specific footer.
        j2534_0404::PROTOCOL_TP2_0_PS => (data.len().min(4), 0),
        _ => (0, 0),
    }
}

/// ADR-200 (Phase 3): drops the native destination-address (DA) byte (wire
/// position 4) from a RawMode SAE J1939 CLL's received frame before it
/// reaches `payload`/`raw_prefix` computation and expected-response/RC
/// matching. SAE J2534-2 §16.4.3/Table 62 gives every native J1939 frame a
/// 5-byte header (4-byte 29-bit CAN ID + 1-byte DA), but the D-PDU RawMode
/// contract (ISO 22900-2:2022 line 774/§10.1.4.19.5/Table 80) is 4 bytes
/// (CAN ID only) -- this is an interior byte removal (5 bytes -> 4 bytes),
/// NOT a prefix/suffix split like `header_footer_len` produces elsewhere,
/// so it needs its own small transform rather than reusing that mechanism.
///
/// A received frame shorter than 5 bytes is malformed/untrusted FFI input
/// (`data` crosses the boundary from a vendor DLL or, in tests, the mock);
/// rather than panicking on an out-of-bounds removal, this returns `data`
/// unchanged in that case -- mirroring `header_footer_len`'s own
/// degenerate-frame fallback philosophy ("treat as unsplittable rather than
/// risk an out-of-bounds slice or misreading unrelated bytes").
fn raw_j1939_rx_drop_destination_address(data: &[u8]) -> Vec<u8> {
    if data.len() < 5 {
        return data.to_vec();
    }
    let mut result = Vec::with_capacity(data.len() - 1);
    result.extend_from_slice(&data[..4]);
    result.extend_from_slice(&data[5..]);
    result
}

/// `edge-case-hunter` finding, ADR-200 close-out pass: `raw_j1939_tx_message`
/// (`tx_header.rs`) has direct unit coverage of its own short-buffer/
/// boundary cases, but this RX-side sibling had none at all -- only an
/// integration test well above the 5-byte boundary. Direct coverage here,
/// including the documented `< 5`-byte fallback and the exact 5-byte
/// boundary (CAN ID + DA, zero-length payload).
#[cfg(test)]
mod raw_j1939_rx_drop_destination_address_tests {
    use super::raw_j1939_rx_drop_destination_address;

    #[test]
    fn drops_the_interior_da_byte_leaving_can_id_and_payload_contiguous() {
        let dropped = raw_j1939_rx_drop_destination_address(&[0x18, 0x00, 0x21, 0x91, 0x21, 0xAA]);
        assert_eq!(
            dropped,
            vec![0x18, 0x00, 0x21, 0x91, 0xAA],
            "the DA byte (index 4) must be removed, leaving the 4-byte CAN ID directly \
             followed by the payload"
        );
    }

    #[test]
    fn exact_five_byte_boundary_with_zero_length_payload_drops_da_to_a_bare_can_id() {
        let dropped = raw_j1939_rx_drop_destination_address(&[0x18, 0x00, 0x21, 0x91, 0x21]);
        assert_eq!(
            dropped,
            vec![0x18, 0x00, 0x21, 0x91],
            "a frame that is exactly CAN ID + DA with no payload must drop to a bare \
             4-byte CAN ID, not panic or under/over-trim"
        );
    }

    #[test]
    fn shorter_than_five_bytes_is_returned_unchanged_rather_than_panicking() {
        for data in [
            &[][..],
            &[0x18][..],
            &[0x18, 0x00][..],
            &[0x18, 0x00, 0x21][..],
            &[0x18, 0x00, 0x21, 0x91][..],
        ] {
            assert_eq!(
                raw_j1939_rx_drop_destination_address(data),
                data,
                "a frame shorter than 5 bytes (malformed/untrusted FFI input) must pass \
                 through unchanged, not panic on an out-of-bounds removal"
            );
        }
    }
}

/// ISO14230-1/CARB format-byte decoding: returns `(header_len, payload_len)`.
/// The format byte's top two bits (`format & 0xC0`) select the wire shape
/// (ADR-166/ADR-167, ISO 22900-2 Table 76), mirroring the TX-direction
/// composer's own gate (`tx_header::kwp_header_bytes`):
///
/// - `0x40` (CARB/ISO9141-2 exception addressing): a fixed 3-byte header
///   (format, target, source) with no length field anywhere in the frame --
///   the entire remainder is payload, and no footer is ever reported for
///   this address mode (ADR-167). Unlike the two cases below, this is NOT
///   self-describing: a genuinely-present trailing checksum byte would be
///   misreported as payload. `rpc_link::connect_flags`/ADR-065 now DOES
///   expose the K-line manual-checksum connect flag (`ISO9141_NO_CHECKSUM`,
///   ADR-198 Phase 2) -- but this residual stays dormant even so: under
///   RawMode this function is still called (to compute a classification-only
///   `raw_prefix`, ADR-196 Decision item 3b), but the actual RX split is
///   unconditionally skipped (`(0, 0)`, Decision item 3), so a genuinely
///   present trailing checksum byte is delivered whole in `data_bytes`
///   either way, never actually split off as a wrongly-classified "payload"
///   the client didn't ask for; and RawMode=OFF (the only mode where this
///   function's `header_len`/`payload_len` split result is actually acted
///   on) can never combine with a CARB frame carrying a manually-managed
///   checksum, since `ISO9141_NO_CHECKSUM` is only ever set for a RawMode=ON
///   CLL (ADR-198's own join-compatibility check additionally bars a
///   RawMode=OFF CLL from ever sharing a physical channel with one that set
///   it). See ADR-167's Consequences for the original accepted-residual
///   reasoning, and ADR-198's own Decision for this dormancy analysis.
/// - `0x80`/`0xC0` (ISO14230 addressed): a 2-byte target+source address
///   follows the format byte; the low 6 bits (the embedded `LEN` field)
///   give the payload length directly when nonzero, or -- when zero --
///   indicate a separate length byte immediately follows (whose value is
///   then the payload length).
/// - `0x00` (unaddressed): no target/source bytes; the low 6 bits/separate
///   length byte behave the same as the addressed case above, minus the
///   2-byte address.
///
/// For the two self-describing cases, `header_len` is `1 (format byte) + (2
/// if addressed) + (1 if a separate length byte is present)`, giving the
/// standard's 1-4 byte range. This service's TX-side composition varies its
/// header shape by the format byte's own configuration
/// (`tx_header::kwp_header_bytes`, ADR-166), and an ECU's response is
/// likewise free to use whatever encoding it wants, so this is parsed
/// per-frame rather than assumed fixed.
///
/// The returned `payload_len` is what lets `header_footer_len` compute a
/// footer (e.g. a trailing checksum byte) as "whatever's left after the
/// header and the declared payload" for the two self-describing cases --
/// self-describing, so it is correct whether or not a checksum is actually
/// present on a given connection (checksum-managed J2534 connections have
/// it stripped before this service ever sees the frame; unmanaged ones do
/// not). The CARB case has no such declaration to lean on -- see above.
///
/// Returns `None` for an empty frame (no format byte to read), for a CARB
/// frame shorter than its fixed 3-byte header, or when a separate length
/// byte is indicated but the frame is too short to contain it.
///
/// `pub(super)` (ADR-198 Phase 2): also reused by
/// `rpc_primitive::compute_tx_prefix`'s K-line arm, against a RawMode CLL's
/// own `cop_data` -- the identical parsing logic applies on the TX side
/// (the client's own header bytes are the same wire shape this function
/// already parses on RX), so it is reused rather than re-derived.
pub(super) fn kwp_header_and_payload_len(data: &[u8]) -> Option<(usize, usize)> {
    let format = *data.first()?;
    if format & 0xC0 == 0x40 {
        // CARB/ISO9141-2 exception addressing (ADR-167): fixed 3-byte
        // header, no length field -- the remainder is entirely payload.
        let payload_len = data.len().checked_sub(3)?;
        return Some((3, payload_len));
    }
    let has_addr = format & 0x80 != 0;
    let addr_len = usize::from(has_addr) * 2;
    let embedded_len = (format & 0x3F) as usize;
    if embedded_len != 0 {
        Some((1 + addr_len, embedded_len))
    } else {
        let payload_len = *data.get(1 + addr_len)? as usize;
        Some((1 + addr_len + 1, payload_len))
    }
}

/// ADR-203: the responding ECU's source-address byte read directly off a
/// RAW, pre-split KWP (ISO9141/ISO14230) or SAE J1850 (VPW/PWM) frame, for
/// `route_frame`'s new `CP_EcuRespSourceAddress`-keyed RX matching tier.
///
/// For KWP, the source-address byte sits at index 2 of `[format, target,
/// source, ..]` for every ADDRESSED header shape -- CARB fixed-3 (`format &
/// 0xC0 == 0x40`) and both `has_addr` (`format & 0x80 != 0`) shapes,
/// embedded-length or separate-length-byte alike -- regardless of whether a
/// trailing length byte is present, since `target`/`source` always precede
/// it. This is deliberately checked directly here rather than by calling
/// `kwp_header_and_payload_len` (edge-case-hunter finding, this PR): that
/// function's separate-length-byte arm requires the 4th length byte to be
/// present to return a header length at all (`data.get(1 + addr_len)?`), so
/// a frame truncated to exactly 3 bytes (`[format, target, source]`, the
/// length byte missing) made it report `None` -- silently dropping the
/// frame instead of extracting the source byte that is, in fact, physically
/// present -- inconsistent with the structurally identical embedded-length
/// shape, which extracts it correctly under the same 3-byte truncation. The
/// two "unaddressed" shapes (`format & 0x80 == 0`, not CARB) have no
/// target/source bytes at all, so they correctly report `None` (source-less
/// frame) via the `has_addr` gate below, not by any header-length
/// computation.
///
/// For J1850 (VPW/PWM), the header is unconditionally 3 bytes
/// (`[format/priority, target, source]`) regardless of frame length --
/// mirrored exactly from `header_footer_len`'s own J1850PWM arm
/// (`data.len().min(3)`) -- so `data.len() >= 3` alone is the correct,
/// sufficient guard: a legal J1850 frame is never headerless the way an
/// unaddressed KWP frame can be.
///
/// This per-protocol addressing logic MUST stay in sync with
/// `kwp_header_and_payload_len`'s and `header_footer_len`'s own per-protocol
/// tables -- all three derive from the same wire shapes, but this function
/// reads RAW, pre-`route_frame` bytes for RX delivery routing, while the
/// other two run strictly afterward (the ADR-051 split) to carve
/// `ResultData.extra_info`.
///
/// This is a DIFFERENT, deliberately separate mechanism from the
/// pre-existing ADR-148 third-Amendment Fix 1 `source_id` derivation in
/// `bind_frame` (`concat_meta.header_bytes[2]`, ~line 3182 above): that one
/// operates on ALREADY-SPLIT header bytes purely for `ConcatBuf` keying;
/// this one operates on RAW pre-split bytes purely for `route_frame`'s RX
/// delivery routing. Do not merge or refactor them together.
pub(super) fn kline_j1850_source_addr(base_protocol_id: u32, data: &[u8]) -> Option<u8> {
    match base_protocol_id {
        j2534_0404::ISO9141 | j2534_0404::ISO14230 => {
            let format = *data.first()?;
            let is_carb = format & 0xC0 == 0x40;
            let has_addr = is_carb || format & 0x80 != 0;
            (has_addr && data.len() >= 3).then(|| data[2])
        }
        j2534_0404::J1850PWM | j2534_0404::J1850VPW => (data.len() >= 3).then(|| data[2]),
        _ => None,
    }
}

/// Outcome of [`bind_frame`]'s ADR-100 Decision §3 attribution scan for one
/// delivered frame at one CLL.
enum FrameBinding {
    /// Bound to a registrant (tier-1 or tier-2): `ResultData`/`ReceivedFrame`
    /// carry `acceptance_id` and `Some(cop_handle)`. `ecu_timing_change`
    /// (ADR-146) is `true` only when this frame was a qualifying `0xC3`
    /// Access Timing Parameter response that produced a ComParam
    /// modification -- feeds the `ECU_TIMING_CHANGE` RxFlag bit.
    Registrant {
        acceptance_id: u32,
        cop_handle: u32,
        ecu_timing_change: bool,
        /// `Some(prev_seq)` when this frame's own qualifying ADR-146 timing
        /// pairing superseded an earlier THIS-PASS observation from the SAME
        /// physically addressed registrant (Codex round-5 Finding 2, PR
        /// #17) -- see `bind_registrant`'s own doc comment. `None` for a
        /// non-timing bind or a `bind_registrant` call site that never even
        /// reaches the ADR-146 pairing block.
        superseded_timing_seq: Option<usize>,
        /// `true` when this frame was absorbed into an open
        /// `CP_EnableConcatenation` segment-merge buffer (`bind_registrant`)
        /// instead of completing a match on its own -- the caller must NOT
        /// deliver `ResultData` for this frame; a separately finalized
        /// buffer (if any, threaded back via `bind_frame`'s own
        /// `finalized_concat` out-parameter) carries the actual delivery.
        /// Always `false` for a non-concat registrant.
        absorbed: bool,
    },
    /// Bound to tester-present's own reply signature (step 3): the caller
    /// discards the frame outright, per spec ("a bound frame is discarded").
    TesterPresent,
    /// Not bound to anything: either step 6 (a genuinely unbound content
    /// frame) or an indication frame (SOM/TxDone/loopback/RxBreak) that no
    /// tester-present arm claimed -- `bind_frame` returns this same variant
    /// for both, since its `is_content_frame` gate only skips the
    /// tier-1/tier-2 scans, not this fallthrough. The caller
    /// (`poll_rx_inner`) distinguishes them via its own `is_content_frame`:
    /// a content frame is discarded outright (ADR-100 Decision §5, S8 --
    /// not buffered, not delivered as unsolicited `ResultData`); an
    /// indication frame keeps its existing `ResultData` delivery path
    /// (Decision §3 step 1's carve-out, ADR-098, unaffected by S8).
    Unbound,
}

/// `CP_SuspendQueueOnError` (ADR-147, corrected) bind-time classification of
/// one delivered frame's effect on this CLL's error-suspend state. Written
/// exactly once per frame by `bind_frame`, from that frame's FINAL binding
/// outcome -- never as a loose side effect of the registrant scan itself --
/// then folded across a whole `poll_rx_inner` pass into
/// `CllRxEntry::queue_error_class` with last-frame-wins semantics (matching
/// "held until a positive response" temporal behavior -- a later positive in
/// the same pass overrides an earlier suspend-worthy frame). See
/// `bind_frame`'s own doc comment for the three mutually-exclusive write
/// sites this single-write-per-frame rule spans (a bound registrant match, a
/// bound tester-present reply, or an unbound content frame). Applied to the
/// live `LogicalLinkState::tx_suspended_by_error` flag in the same
/// end-of-pass critical section that runs `merge_registrant_writeback`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueErrorClass {
    /// An unhandled negative response was observed this pass: either a bound
    /// tier-1 match whose payload is 0x7F-led and not claimable by
    /// `RcHandlingConfig::detect_pending_rc`, or an otherwise-unbound content
    /// frame that also qualifies as unhandled per
    /// `RcHandlingConfig::is_unhandled_negative`. A tier-2 (Receive Only)
    /// match never produces this variant at all -- see `Positive`'s own doc
    /// comment. Sets `tx_suspended_by_error` only when live
    /// `CP_SuspendQueueOnError == 1` at apply time.
    Suspend,
    /// A positive/expected response was observed this pass. Tier-1 produces
    /// this whenever its own `rc_cfg` does NOT classify the bound payload as
    /// unhandled. Tier-2 (Receive Only, no `rc_cfg` at all -- RC handling is
    /// tier-1 only) produces this for any bound match EXCEPT a payload
    /// starting with `0x7F`, which instead writes no classification at all
    /// (an accepted heuristic, ADR-147: a missed auto-resume is the safe
    /// failure direction for that residual case). Always clears
    /// `tx_suspended_by_error` unconditionally, regardless of the live
    /// ComParam value.
    Positive,
}

/// ADR-147's `CP_SuspendQueueOnError` classification rule for one bound
/// frame, judged against `rc_cfg` (the binding registrant's own frozen RC
/// config) and `payload`. `Suspend` requires a CONFIRMED unhandled negative
/// response; a 0x7F-led payload that isn't confirmed (no `rc_cfg`, or
/// `is_unhandled_negative` returns `false` for a reason other than
/// "genuinely not 0x7F-led" -- e.g. an unattributable NRC or a truncated
/// `7F <sid>`) deliberately declines to classify at all (`None`, no queue
/// effect) rather than risk a false `Positive` that would wrongly clear a
/// genuinely-still-suspended queue. Every other payload is `Positive`.
///
/// `raw_prefix` (ADR-196 Decision item 3b): forwarded to `is_unhandled_negative`
/// unchanged (see its own doc comment); this function's OWN `0x7F`-at-position-0
/// tier-2 (no `rc_cfg`) check moves to `payload.get(raw_prefix)`. When
/// `payload` is shorter than `raw_prefix` itself -- a truncated/malformed raw
/// frame with no logical response bytes at all -- this declines to classify
/// (`None`) rather than fall through to `Positive`, mirroring
/// `RcHandlingConfig::detect_pending_rc`'s own `checked_sub`-based decline
/// (a byte-index-bounds check here rather than an offset subtraction, but
/// the same "decline rather than risk a false positive" philosophy). `0` for
/// RawMode=OFF leaves this byte-for-byte the pre-ADR-196 behavior in every
/// case, including this bounds check (`payload.len() < 0` is never true).
fn classify_queue_error(
    rc_cfg: Option<&RcHandlingConfig>,
    payload: &[u8],
    raw_prefix: usize,
) -> Option<QueueErrorClass> {
    match rc_cfg {
        Some(cfg) if cfg.is_unhandled_negative(payload, raw_prefix) => {
            Some(QueueErrorClass::Suspend)
        }
        _ if payload.len() < raw_prefix => None,
        _ if payload.get(raw_prefix) == Some(&0x7F) => None,
        _ => Some(QueueErrorClass::Positive),
    }
}

/// Which of ADR-100 Decision §3's registrant scans to run -- kept as
/// separate calls (rather than one fused pass) so [`bind_frame`] can
/// interleave step 3 (tester-present) between step 2 and step 4 exactly
/// where the ADR places it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AttributionScan {
    /// Step 2: tier-1, non-vacuous claims. A registrant's own pending-RC
    /// detection is evaluated for EVERY tier-1 registrant here regardless of
    /// any descriptor's `is_vacuous()` -- RC detection is a wholly separate
    /// mechanism from expected-response mask/pattern vacuousness (ADR-100
    /// Decision §3, round-5 addendum: vacuousness is per-descriptor, not
    /// per-registrant).
    Tier1NonVacuous,
    /// Step 4: tier-1, vacuous claims (deferred here specifically so
    /// tester-present's step-3 signature match outranks them).
    Tier1Vacuous,
    /// Step 5: tier-2 (Receive Only). Populated by both of ADR-100 Decision
    /// §2's tier-2 sources: IS-CYCLIC registrants that migrated
    /// post-first-match (`migrate_registrant_to_receive_only`, S5), and
    /// created-receive-only (`NumSendCycles == 0`) registrants inserted
    /// directly as tier-2 at creation for ANY `NumReceiveCycles` (finite `N`,
    /// `-1`, or `-2` -- ADR-100 round-9 Finding-1 correction;
    /// `wait_for_expected_response`'s `wait.created_receive_only` gate).
    /// Only the `-1` and finite-`N > 0` subtypes
    /// (`is_comparam_timed_receive_only`, ADR-182 -- `-1`-only before that)
    /// also detach from execution at creation (S6) -- a `-2` (IS-MULTIPLE)
    /// created-receive-only registrant is tier-2 here from the start but
    /// still runs its own inline/blocking receive-phase loop for the whole
    /// of its wait (ADR-100 Decision §4/Out-of-scope's Stage 3 boundary,
    /// unaffected by ADR-182).
    Tier2,
}

/// Per-frame metadata `bind_registrant`/`bind_frame` need, mostly for
/// `CP_EnableConcatenation` buffering (D-PDU `PARAM_ENABLE_CONCATENATION` =
/// 0x807B) -- bundled into one small `Copy` struct so a feature only a
/// concat-enabled registrant ever consults doesn't grow every caller's
/// (including every existing test's) parameter list by three unrelated
/// values. `Default` gives every non-concat call site (which never reads
/// those fields, since `CopRegistrant::concat_enabled` gates every use) a
/// one-token placeholder. `uudt_routed` (ADR-150) is the one field here NOT
/// `CP_EnableConcatenation`-specific -- bundled in for the identical
/// call-site-economy reason, see its own doc comment.
#[derive(Clone, Copy, Default)]
struct ConcatFrameMeta<'a> {
    timestamp: u32,
    header_bytes: &'a [u8],
    footer_bytes: &'a [u8],
    rx_status_flags: u8,
    /// Per-frame source-address byte extracted from this frame's OWN split
    /// header (ADR-051), used as an extra `ConcatBuf` key component
    /// (`ConcatBuf::key`'s `Option<u8>` slot) so two distinct ECUs replying
    /// to the same functional/broadcast request with the same
    /// `unique_resp_identifier` (universal for a no-`UniqueRespIdTable` KWP/
    /// J1850 CLL -- see `route_frame`'s own doc comment) and the same SID
    /// never collide into one corrupted buffer. Derived once, in
    /// [`bind_frame`], from `CllRxEntry::header_protocol` and this frame's
    /// own already-split `header_bytes`:
    /// - ISO9141/ISO14230 (KWP consolidated header `[fmt, tgt, src, ..]`):
    ///   `Some(header_bytes[2])` when `header_bytes.len() >= 3`.
    /// - J1850 PWM/VPW (`[pri/type, tgt, src]`): `Some(header_bytes[2])`
    ///   when `header_bytes.len() == 3` exactly.
    /// - Every other protocol, or a too-short/headerless frame: `None` --
    ///   accepted residual, see `ConcatBuf::key`'s own doc comment. A `None`
    ///   frame groups with every other `None` frame sharing the same
    ///   `(unique_resp_identifier, SID)`, exactly like before this field
    ///   existed, since no ECU-identifying data is physically available at
    ///   this layer for such a frame (ADR-148 third Amendment).
    source_id: Option<u8>,
    /// ADR-150: `true` iff this frame was routed to this CLL via a
    /// `CP_CanRespUUDTId` match -- either the hardware entry's own
    /// UUDT-vs-USDT routing outcome (`route_frame`), or the UUDT companion
    /// channel (`RxEntryKind::Companion`/`route_frame_uudt_only`, ADR-046) --
    /// rather than a `CP_CanRespUSDTId` match or a no-table wildcard. `false`
    /// for every non-CAN-family protocol (no CAN-ID-keyed routing table
    /// exists to be UUDT-routed via). Consulted only by
    /// `observe_registrant_timing_change` (ADR-150: a DiagnosticSessionControl
    /// response is definitionally unicast/USDT traffic, so a UUDT-routed
    /// frame must never pair against `timing_cfg`).
    uudt_routed: bool,
    /// ADR-196 Decision item 3b: the per-frame raw CAN-ID prefix width
    /// (`events::header_footer_len`'s own already-audited computation,
    /// reused rather than re-derived) preceding the actual UDS/KWP payload in
    /// `payload` (the same argument `bind_registrant` classifies) for a
    /// RawMode=ON CLL -- `0` unconditionally for RawMode=OFF. A second field
    /// here NOT `CP_EnableConcatenation`-specific, for the identical
    /// call-site-economy reason `uudt_routed` above already is: every one of
    /// `bind_registrant`'s internal UDS negative-response detectors
    /// (`RcHandlingConfig::detect_pending_rc`/`is_unhandled_negative`,
    /// `classify_queue_error`, `observe_registrant_timing_change` ->
    /// `observe_session_timing_response`) needs it, not just a concat-enabled
    /// registrant, and `Default`'s `0` is the correct RawMode=OFF value for
    /// every one of them.
    raw_prefix: usize,
}

#[path = "events_concat.rs"]
mod concat;
use concat::*;

/// Scans `registrants` (already snapshotted in `registration_seq` order --
/// see `CllRxEntry::registrants`'s doc comment) for the first one `scan`
/// selects that binds `payload`, mutating its `matches_got`/`pending_rc` in
/// place and returning `Some((acceptance_id, cop_handle, ecu_timing_change,
/// superseded_timing_seq, absorbed, classification))`:
///
/// - `ecu_timing_change` (ADR-146) is `true` only when this frame was a
///   qualifying `0xC3` Access Timing Parameter response that produced a
///   ComParam modification -- feeds the `ECU_TIMING_CHANGE` RxFlag bit.
///   `superseded_timing_seq` is `Some(prev_seq)` when this frame's own
///   qualifying ADR-146 timing pairing overwrote (never merged, since
///   physical addressing doesn't accumulate, round 3) an earlier THIS-PASS
///   observation from the SAME physically addressed registrant, naming that
///   earlier observation's own `frame_seq` (Codex round-5 Finding 2, PR #17)
///   -- see `timing_frame_flag_ok`'s own doc comment for how this feeds the
///   per-frame flag-correctness check. `frame_seq` (the parameter) is this
///   pass's pass-local frame sequence number (`poll_rx_inner`'s `for
///   (frame_seq, msg) in messages.iter().enumerate()`) -- stamped onto a
///   qualifying pairing's `pending_timing_change` so
///   `select_latest_timing_changes` can later resolve two different
///   registrants' independent SID 0x83 exchanges on the same CLL by arrival
///   order (last-exchange-wins), not by worst-case combination.
/// - `absorbed` is `true` only when the frame was absorbed into an open
///   `CP_EnableConcatenation` buffer instead of completing a match on its
///   own (see `CopRegistrant::concat_enabled`'s own doc comment for the
///   full scope).
/// - `classification` is this bound frame's own fresh `CP_SuspendQueueOnError`
///   (ADR-147) verdict, judged against THIS registrant's frozen `rc_cfg`
///   (`classify_queue_error`). It is computed for absorbed frames too:
///   ADR-147's fold is per-frame wire evidence (last frame in the pass
///   wins), so an absorbed positive segment is resume evidence and a
///   0x7F-led confirmed-unhandled frame that opens a buffer is suspend
///   evidence -- withholding either would silently disable
///   `CP_SuspendQueueOnError` on every concat-enabled COP, whose non-empty
///   accepted frames are ALL absorbed. `None` means "no queue effect": a
///   pending-RC match, or a 0x7F-led match not positively confirmed
///   unhandled (ADR-147's decline-to-classify heuristic).
///
/// Every candidate is additionally gated on
/// `entry_connect_generation == registrant.connect_generation` (ADR-086,
/// moved from the old `MatchProbe` to per-registrant, same semantics: a
/// frame from a stale, pre-reconnect session never binds to a registrant
/// from the old generation).
///
/// `concat_meta`/`finalized_concat` are `CP_EnableConcatenation`-only.
/// `registrant.concat` is now a `Vec<ConcatBuf>` (ADR-148 Amendment): a
/// continuation is absorbed into whichever open buffer's own key matches the
/// frame's `(unique_resp_identifier, SID)` AND whose recorded
/// `opened_vacuous` classification matches the current `scan` (ADR-148
/// Amendment 2 -- a buffer opened by a vacuous descriptor only ever absorbs
/// during `Tier1Vacuous`, never `Tier1NonVacuous`, preserving ADR-100's
/// non-vacuous-first precedence; a key match whose scan doesn't match simply
/// declines here and the same registrant is re-scanned on the next `scan`
/// variant), found by a linear scan across every entry in `concat`; a frame
/// whose key matches no open buffer -- or matches one whose scan doesn't
/// line up yet -- opens a FRESH one (gated on quota -- see the absorb/open
/// logic's own comments) instead of force-finalizing some other, unrelated
/// buffer -- a differing key is no longer, by itself, a finalize trigger.
/// `finalized_concat`
/// (pushed to, never overwritten) is populated only by the empty-payload-
/// match arm (edge-case-hunter Finding 3), which must still finalize every
/// open buffer before treating the empty match as its own completion --
/// every other finalization happens exclusively at the receive-phase
/// deadline (`wait_for_expected_response_inner`, `finalize_concat_buffers`).
/// Callers must deliver every entry appended to `*finalized_concat` this
/// call before acting on this call's own `Option` return value.
///
/// `sighted_unhandled` (ADR-147, corrected): a pure SIGHTING record, set
/// `true` when the `Tier1NonVacuous` scan observes SOME registrant's own
/// frozen `rc_cfg` classify `payload` as an unhandled negative response --
/// regardless of whether that registrant (or any other) actually binds the
/// frame. Never written to directly by this function's caller; only ever
/// OR'd into by repeat calls across one frame's whole scan sequence. Used by
/// `bind_frame` exclusively for its `FrameBinding::Unbound` fallthrough (see
/// that call site): a frame nobody bound at all still needs this evidence to
/// classify `Suspend`.
/// ADR-146: observes `payload` for a qualifying `0xC3` Access Timing
/// Parameter response against `r`'s own `timing_cfg`, recording the result
/// into `r.pending_timing_change`/`r.timing_accumulator` as a side effect
/// independent of whether `r` goes on to deliver this frame as its own
/// `ResultData` or absorb it into an ADR-148 concat buffer -- the wire
/// observation itself does not depend on delivery mechanics. Checks the
/// header-stripped response payload's first byte (the fixed SID+0x40 KWP
/// positive-response relationship, `0xC3`) and second byte (echoed TPI)
/// against `r.timing_cfg`. Returns `(ecu_timing_change, superseded_timing_seq)`
/// for the caller's own return tuple; both are always `(false, None)` when
/// `r.timing_cfg` is `None` or the payload does not qualify.
///
/// ADR-146/148 interaction (merge finding, PR #17, extended by an
/// edge-case-hunter finding on the merge fix itself): call this from EVERY
/// `bind_registrant` arm that accepts `payload` for a registrant with
/// `timing_cfg` set -- the plain match arm, the ADR-148 concat "open new
/// buffer" arm, AND both concat "absorb into existing buffer" arms. The
/// last of those is needed not for a genuine multi-frame continuation of
/// the SAME response (never expected in practice -- every `0xC3` response
/// TPI variant is well under one KWP/J1850 frame's data capacity, so a
/// genuine Access Timing Parameter response never actually needs
/// continuation), but because an already-open buffer is finalized only by
/// the receive-phase deadline or a cap overflow, never by "another matching
/// frame arrived" -- so a SECOND, independent qualifying response sharing
/// an open buffer's exact key is absorbed here too, and must still update
/// `r.pending_timing_change` to its own (latest) values rather than leave
/// it frozen on the first response's. This call is safe even in the
/// (unexpected) genuine-continuation case: a continuation segment's second
/// byte is arbitrary response data, not an echoed TPI, so it either fails
/// the `payload.get(1) == cfg.tpi` gate below outright, or in the narrow
/// case it doesn't, `observe_timing_response`'s bounds-checked
/// `payload.get(2..7)` still can't panic on a short payload -- worst case is
/// a harmless spurious re-observation, never corrupted state.
fn observe_registrant_timing_change(
    r: &mut CopRegistrant,
    payload: &[u8],
    frame_seq: usize,
    uudt_routed: bool,
    // ADR-196 Decision item 3b, extended by ADR-198 Phase 2: forwarded to
    // BOTH variants below -- `observe_session_timing_response` (the UDS/
    // ISO15765 variant) always needed it; the KWP `observe_timing_response`
    // arm (and this function's own `0xC3`/TPI-echo match guard just below)
    // now also needs it, since K-line RawMode is no longer rejected at
    // `CreateComLogicalLink` (ADR-198 extended Decision item 1's allowlist
    // to ISO9141/ISO14230) -- both arms are reachable for a RawMode K-line
    // CLL now. `0` for RawMode=OFF leaves this byte-for-byte the pre-ADR-196
    // behavior for both variants, unchanged.
    raw_prefix: usize,
) -> (bool, Option<usize>) {
    // ADR-150: a DiagnosticSessionControl response is definitionally
    // unicast/USDT traffic -- a frame routed here via a `CP_CanRespUUDTId`
    // match (either the hardware entry's own UUDT-id routing, or the UUDT
    // companion channel, ADR-046) must never pair against `timing_cfg`
    // regardless of its payload bytes. This is checked before dispatching on
    // the config variant so it applies uniformly (KWP never produces
    // `uudt_routed: true` in the first place -- no CAN-ID-keyed routing
    // table exists for a non-CAN-family protocol -- so this is a genuine
    // no-op for the KWP variant, not a KWP behavior change).
    if uudt_routed {
        return (false, None);
    }
    let mut ecu_timing_change = false;
    let mut superseded_timing_seq = None;
    let change = match r.timing_cfg.as_ref() {
        Some(TimingChangeConfig::KwpAccess(cfg))
            if payload.get(raw_prefix) == Some(&0xC3)
                && cfg.tpi.is_some()
                && payload.get(raw_prefix + 1) == cfg.tpi.as_ref() =>
        {
            observe_timing_response(cfg, payload, &mut r.timing_accumulator, raw_prefix)
        }
        Some(TimingChangeConfig::UdsSession(cfg)) => {
            observe_session_timing_response(cfg, payload, &mut r.timing_accumulator, raw_prefix)
        }
        _ => None,
    };
    if let Some(change) = change {
        // Codex round-5 finding, PR #17 (ADR-146; reused unchanged by
        // ADR-150's UDS variant): a PHYSICALLY addressed registrant's
        // earlier-this-pass observation is fully superseded (never merged)
        // by this one, per round 3's fix (no accumulation for physical
        // addressing) -- unlike a functionally addressed registrant, where
        // every qualifying response genuinely contributed to the running
        // worst-case accumulator and must keep its own flag.
        let functional = r
            .timing_cfg
            .as_ref()
            .is_some_and(TimingChangeConfig::functional);
        if !functional && let Some((prev_seq, _)) = r.pending_timing_change {
            superseded_timing_seq = Some(prev_seq);
        }
        r.pending_timing_change = Some((frame_seq, change));
        ecu_timing_change = true;
    }
    (ecu_timing_change, superseded_timing_seq)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn bind_registrant(
    registrants: &mut [CopRegistrant],
    entry_connect_generation: u64,
    payload: &[u8],
    unique_resp_identifier: u32,
    scan: AttributionScan,
    frame_seq: usize,
    concat_meta: ConcatFrameMeta<'_>,
    finalized_concat: &mut Vec<ConcatDelivery>,
    sighted_unhandled: &mut bool,
) -> Option<(u32, u32, bool, Option<usize>, bool, Option<QueueErrorClass>)> {
    for r in registrants.iter_mut() {
        if r.connect_generation != entry_connect_generation {
            continue;
        }
        match scan {
            AttributionScan::Tier1NonVacuous | AttributionScan::Tier1Vacuous => {
                if r.tier != RegistrantTier::ActiveSendReceive {
                    continue;
                }
            }
            AttributionScan::Tier2 => {
                if r.tier != RegistrantTier::ReceiveOnly {
                    continue;
                }
            }
        }
        if scan == AttributionScan::Tier1NonVacuous {
            // Pending-RC detection is not gated on `vacuous` -- see this
            // scan variant's own doc comment. It IS gated on
            // `unique_resp_ids`, same convention as the positive-match scan
            // below: a pending RC from an ECU none of this registrant's
            // descriptors accept must not extend this COP's wait (ADR-100
            // Decision §3, resolved (b) addendum).
            let rc_unique_resp_ok = r.expected.is_empty()
                || r.expected.iter().any(|e| {
                    e.unique_resp_ids.is_empty()
                        || e.unique_resp_ids.contains(&unique_resp_identifier)
                });
            if rc_unique_resp_ok
                && let Some(code) = r
                    .rc_cfg
                    .as_ref()
                    .and_then(|cfg| cfg.detect_pending_rc(payload, concat_meta.raw_prefix))
            {
                if r.pending_rc.is_none() {
                    r.pending_rc = Some(code);
                }
                // A pending-RC interim response (e.g. `7F 83 78`) is never a
                // qualifying `0xC3` positive response -- ADR-146's own
                // "coexists with existing pending-RC detection unchanged"
                // note.
                return Some((0, r.cop_handle, false, None, false, None));
            }
            // ADR-147 correction: a pure SIGHTING, never a `queue_error_class`
            // write. Fires for ANY tier-1-non-vacuous registrant whose own
            // frozen `rc_cfg` classifies `payload` as unhandled, regardless
            // of whether THIS registrant (or any other) goes on to actually
            // bind the frame below -- `bind_frame`'s `Unbound` fallthrough is
            // the only consumer, for the case where nothing ever binds it.
            if rc_unique_resp_ok
                && r.rc_cfg
                    .as_ref()
                    .is_some_and(|cfg| cfg.is_unhandled_negative(payload, concat_meta.raw_prefix))
            {
                *sighted_unhandled = true;
            }
        }
        if r.matches_needed.is_none_or(|needed| r.matches_got < needed) {
            // ADR-148 Amendment. Continuation
            // fast path (edge-case-hunter Finding 2 fix): a genuine
            // continuation segment's bytes past the SID are new response
            // data, not a repeat of the first segment's content, so
            // re-checking it against the registrant's full configured
            // descriptor (whose mask/pattern can constrain bytes past byte
            // 0) would wrongly reject a real continuation whose later bytes
            // differ, defeating the "SID identifies the segmented response"
            // rule (paraphrase of ISO 22900-2 Table B.20's
            // `CP_EnableConcatenation` description). So: whenever a buffer
            // is already open for `r` whose OWN key matches this frame's
            // `(unique_resp_identifier, SID)`, the continuation-or-not
            // decision is made SOLELY against that buffer's key, bypassing
            // `ExpectedResponse::matches` entirely for this decision. A
            // linear scan across every entry in `r.concat` (not just one)
            // handles IS-MULTIPLE's several concurrently-open, per-ECU
            // buffers. Only when no open buffer's key matches does the
            // frame have to satisfy the full descriptor -- exactly as a
            // genuine first segment must.
            //
            // ADR-148 second Amendment (Codex round-3 finding): the fast
            // path must still respect ADR-100's non-vacuous-first
            // precedence, so it only claims the frame when the CURRENT
            // `scan` pass matches the classification the buffer was opened
            // under (`ConcatBuf::opened_vacuous`, fixed at open time). When
            // the gate fails here, the frame simply falls through -- the
            // SAME registrant is scanned again on the next `scan` variant
            // via the normal `bind_frame` call sequence, where the gate
            // will then pass.
            // ADR-148 third Amendment (Fix 1): the buffer key now includes
            // `concat_meta.source_id` (the frame's own split-header source-
            // address byte, `None` for a headerless/too-short frame) as a
            // middle component, so two distinct ECUs sharing the same
            // `unique_resp_identifier` (universal `0` on a no-table KWP/
            // J1850 CLL, `route_frame`'s own doc comment) and the same SID
            // no longer collide into one buffer.
            if r.concat_enabled
                && !payload.is_empty()
                && let Some(pos) = r.concat.iter().position(|b| {
                    b.key == (unique_resp_identifier, concat_meta.source_id, payload[0])
                })
                && match scan {
                    AttributionScan::Tier1NonVacuous => !r.concat[pos].opened_vacuous,
                    AttributionScan::Tier1Vacuous => r.concat[pos].opened_vacuous,
                    // Structurally unreachable: `concat_enabled` is always
                    // `false` for tier-2/created-receive-only/IS-CYCLIC
                    // registrants (ADR-148's v1 scope), and a concat-eligible
                    // registrant stays `ActiveSendReceive` for its whole
                    // life, so this function's own tier gate above already
                    // excludes it from ever reaching this scan variant.
                    // Written out (not `unreachable!()`) for match
                    // exhaustiveness/defensiveness.
                    AttributionScan::Tier2 => false,
                }
            {
                let buf = &mut r.concat[pos];
                buf.data.extend_from_slice(&payload[1..]);
                buf.footer_bytes = concat_meta.footer_bytes.to_vec();
                buf.segments += 1;
                let acceptance_id = buf.acceptance_id;
                r.concat_segments_got += 1;
                // ADR-148 third Amendment (Fix 2): this absorb may have
                // pushed the buffer over either internal cap -- force-
                // finalize THIS ONE buffer immediately (never any other open
                // buffer on this registrant) rather than holding it open and
                // restarting the receive-phase deadline forever.
                if r.concat[pos].data.len() > CONCAT_MAX_BUF_BYTES
                    || r.concat[pos].segments > CONCAT_MAX_BUF_SEGMENTS
                {
                    finalized_concat.push(finalize_one_concat_buffer(r, pos));
                }
                // ADR-147/148 merge: an absorbed frame still gets a fresh
                // CP_SuspendQueueOnError classification -- see
                // `classify_queue_error`'s own doc comment and this
                // function's own doc comment for why.
                let classification =
                    classify_queue_error(r.rc_cfg.as_ref(), payload, concat_meta.raw_prefix);
                // ADR-146/148 interaction (edge-case-hunter finding, PR #17,
                // round following the merge): an already-open buffer is
                // finalized ONLY by the receive-phase deadline or a cap
                // overflow (`ConcatBuf`'s own doc comment), never by "another
                // matching frame arrived" -- so a SECOND, independent
                // qualifying `0xC3` response (an ECU retransmission, a
                // repeat/cyclic exchange, or a second distinct
                // `CP_ModifyTiming` negotiation) sharing this buffer's exact
                // key is absorbed here as if it were a continuation, even
                // though `payload` on THIS call is still a complete,
                // self-contained response in its own right. Calling this
                // against `payload` (not the buffer's own merged `data`)
                // re-pairs against the CURRENT frame's real TPI/timing bytes,
                // so `r.pending_timing_change` tracks the LATEST qualifying
                // exchange -- matching this mechanism's own established
                // arrival-order/last-exchange-wins semantics elsewhere
                // (`select_latest_timing_changes`) -- instead of silently
                // freezing on the buffer-opening response's now-stale
                // values. Does not touch the corrupted-merged-payload-bytes
                // side of this same collision (`ConcatBuf::data` still gets
                // the second response's bytes appended as if they were a
                // continuation) -- that is ADR-148's own accepted, spec-
                // conformant behavior for the general same-key-collision case
                // (see ADR-148's Amendment 12; Amendment 10 covers only the
                // narrower service-controlled retransmit-boundary case),
                // unrelated to and out of scope for the ComParam side effect
                // this call closes.
                let (ecu_timing_change, superseded_timing_seq) = observe_registrant_timing_change(
                    r,
                    payload,
                    frame_seq,
                    concat_meta.uudt_routed,
                    concat_meta.raw_prefix,
                );
                return Some((
                    acceptance_id,
                    r.cop_handle,
                    ecu_timing_change,
                    superseded_timing_seq,
                    true,
                    classification,
                ));
            }

            if let Some(d) = r.expected.iter().find(|e| {
                (e.unique_resp_ids.is_empty()
                    || e.unique_resp_ids.contains(&unique_resp_identifier))
                    && e.matches(payload)
                    && match scan {
                        AttributionScan::Tier1NonVacuous => !e.is_vacuous(),
                        AttributionScan::Tier1Vacuous => e.is_vacuous(),
                        AttributionScan::Tier2 => true,
                    }
            }) {
                let acceptance_id = d.acceptance_id;

                // ADR-148 Amendment. Gated on
                // `concat_enabled`, which is `false` for every registrant
                // outside the four allowed protocols/the v1 registrant-shape
                // scope (`CopRegistrant::concat_enabled`'s own doc comment) --
                // every other registrant takes the unchanged normal-match arm
                // below, byte-for-byte.
                if r.concat_enabled && !payload.is_empty() {
                    // ADR-148 third Amendment (Fix 1): three-component key,
                    // see the fast path's own comment above for the
                    // `source_id` rationale.
                    let frame_key = (unique_resp_identifier, concat_meta.source_id, payload[0]);

                    // ADR-148 second Amendment: with the fast path above now
                    // scan-gated on `ConcatBuf::opened_vacuous`, a same-key
                    // buffer CAN reach this arm -- e.g. `r`'s buffer was
                    // opened by a vacuous descriptor (so the fast path
                    // declined during `Tier1NonVacuous`), and `r` also has a
                    // separate non-vacuous descriptor whose mask/pattern
                    // this continuation's payload happens to satisfy. In
                    // that case this is still a continuation of the SAME
                    // logical response, not a genuinely new one, so absorb
                    // into the existing buffer instead of opening a
                    // duplicate -- checked BEFORE the quota gate below,
                    // since absorption never consumes additional quota
                    // (only opening a genuinely new buffer does).
                    if let Some(pos) = r.concat.iter().position(|b| b.key == frame_key) {
                        let buf = &mut r.concat[pos];
                        buf.data.extend_from_slice(&payload[1..]);
                        buf.footer_bytes = concat_meta.footer_bytes.to_vec();
                        buf.segments += 1;
                        let acceptance_id = buf.acceptance_id;
                        r.concat_segments_got += 1;
                        // ADR-148 third Amendment (Fix 2): same per-buffer
                        // cap check as the fast path's absorb above.
                        if r.concat[pos].data.len() > CONCAT_MAX_BUF_BYTES
                            || r.concat[pos].segments > CONCAT_MAX_BUF_SEGMENTS
                        {
                            finalized_concat.push(finalize_one_concat_buffer(r, pos));
                        }
                        // ADR-147/148 merge: an absorbed frame still gets a
                        // fresh CP_SuspendQueueOnError classification -- see
                        // `classify_queue_error`'s own doc comment and this
                        // function's own doc comment for why.
                        let classification = classify_queue_error(
                            r.rc_cfg.as_ref(),
                            payload,
                            concat_meta.raw_prefix,
                        );
                        // ADR-146/148 interaction (edge-case-hunter finding,
                        // PR #17): same reasoning as the continuation fast
                        // path's own absorb arm above -- see its comment for
                        // why this call, against `payload` and not the
                        // buffer's merged `data`, is needed here too.
                        let (ecu_timing_change, superseded_timing_seq) =
                            observe_registrant_timing_change(
                                r,
                                payload,
                                frame_seq,
                                concat_meta.uudt_routed,
                                concat_meta.raw_prefix,
                            );
                        return Some((
                            acceptance_id,
                            r.cop_handle,
                            ecu_timing_change,
                            superseded_timing_seq,
                            true,
                            classification,
                        ));
                    }

                    // A same-key frame is always claimed by the
                    // continuation fast path above, or by the absorb branch
                    // just above, before reaching here, so this is always a
                    // genuine NEW response starting -- possibly a different
                    // ECU replying to the same broadcast/functional request
                    // (IS-MULTIPLE). Opening a fresh buffer for it is gated
                    // on quota (the invariant `matches_got + concat.len()`
                    // must stay under `matches_needed` (finite `N`, most
                    // commonly 1) is maintained here rather than by force-
                    // finalizing some OTHER already-open buffer first -- the
                    // first Amendment removes that force-finalize entirely;
                    // only the receive-phase deadline expiring ever
                    // finalizes a buffer now, via `finalize_concat_buffers`)
                    // AND, ADR-148 third Amendment (Fix 3), on the total
                    // number of DISTINCT open buffers staying under
                    // `CONCAT_MAX_OPEN_BUFFERS` -- a noisy adapter feeding a
                    // vacuous-descriptor IS-MULTIPLE COP (unbounded
                    // `matches_needed`) must not be able to accumulate
                    // unbounded open buffers just because the quota gate
                    // alone always passes for `None`.
                    if r.matches_needed
                        .is_none_or(|needed| r.matches_got + (r.concat.len() as u32) < needed)
                        && (r.concat.len() as u32) < CONCAT_MAX_OPEN_BUFFERS
                    {
                        r.concat.push(ConcatBuf {
                            key: frame_key,
                            timestamp: concat_meta.timestamp,
                            header_bytes: concat_meta.header_bytes.to_vec(),
                            unique_resp_identifier,
                            acceptance_id,
                            rx_status_flags: concat_meta.rx_status_flags,
                            data: payload.to_vec(),
                            footer_bytes: concat_meta.footer_bytes.to_vec(),
                            opened_vacuous: d.is_vacuous(),
                            segments: 1,
                        });
                        r.concat_segments_got += 1;
                        // ADR-147/148 merge: see the absorb-into-existing
                        // arm above for why an absorbed frame still gets a
                        // classification.
                        let classification = classify_queue_error(
                            r.rc_cfg.as_ref(),
                            payload,
                            concat_meta.raw_prefix,
                        );
                        // ADR-146/148 interaction (merge finding, PR #17):
                        // this is the buffer's OPENING segment -- a genuine
                        // new response, exactly the shape
                        // `observe_registrant_timing_change`'s own doc
                        // comment scopes it to. Without this call, a
                        // registrant with BOTH `concat_enabled` and
                        // `timing_cfg` set would never pair a qualifying
                        // `0xC3` response that happens to open a concat
                        // buffer -- `r.pending_timing_change` would simply
                        // never be populated, silently dropping the
                        // exchange. The finalized `ConcatDelivery`'s own
                        // `ecu_timing_change` RxFlag still reads `false`
                        // regardless (`ConcatDelivery::into_received_frame`,
                        // accepted residual, ADR-146) -- only the ComParam
                        // side effect captured here matters for hardware
                        // application.
                        let (ecu_timing_change, superseded_timing_seq) =
                            observe_registrant_timing_change(
                                r,
                                payload,
                                frame_seq,
                                concat_meta.uudt_routed,
                                concat_meta.raw_prefix,
                            );
                        return Some((
                            acceptance_id,
                            r.cop_handle,
                            ecu_timing_change,
                            superseded_timing_seq,
                            true,
                            classification,
                        ));
                    }
                    // Quota exhausted, or the open-buffer-count cap reached:
                    // this frame is not absorbed -- fall through to whatever
                    // the caller does with an unmatched frame (the next
                    // registrant, or eventually normal tier-scan/unbound
                    // discard), same as today's non-concat quota-exhausted
                    // case.
                } else {
                    // edge-case-hunter Finding 3 fix, extended by the ADR-148
                    // Amendment: an empty-payload frame that matches via
                    // `ExpectedResponse::matches`'s `cmp_len == 0`
                    // short-circuit must force-finalize EVERY still-open
                    // `r.concat` buffer FIRST -- otherwise the phase can
                    // complete right here (this match satisfying
                    // `matches_needed`) with one or more open buffers'
                    // accumulated data silently discarded instead of
                    // delivered.
                    if r.concat_enabled {
                        finalized_concat.extend(finalize_concat_buffers(r));
                    }
                    // Re-check quota AFTER the possible finalize(s) just
                    // above -- finalizing N open buffers can, by itself,
                    // already meet or exceed this registrant's quota (each
                    // one just incremented `matches_got` inside
                    // `finalize_concat_buffers`).
                    if r.matches_needed
                        .is_some_and(|needed| r.matches_got >= needed)
                    {
                        // Quota exhausted by the finalize(s) above: this
                        // frame is not accepted by this registrant -- fall
                        // through (to the next registrant, or eventually
                        // normal tier-scan/unbound discard);
                        // `finalized_concat` has already recorded the
                        // just-completed buffer(s).
                        continue;
                    }
                    r.matches_got += 1;
                    // ADR-147 correction: this registrant bound the frame --
                    // classification is computed FRESH, right here, from
                    // THIS registrant's own `rc_cfg`, for every scan variant
                    // (not just `Tier1NonVacuous`) -- see
                    // `classify_queue_error`'s own doc comment.
                    let classification =
                        classify_queue_error(r.rc_cfg.as_ref(), payload, concat_meta.raw_prefix);
                    // ADR-100 round-9 Finding-2 correction: flip the true
                    // IS-CYCLIC shape's tier to `ReceiveOnly` INLINE, immediately
                    // after accepting its first match, so every later frame in
                    // the SAME `PassThruReadMsgs` batch is scanned against the
                    // already-migrated tier instead of a stale tier-1 snapshot.
                    // Gated on CURRENT tier (not a "first match" counter) so
                    // this is robust to a snapshot taken after an earlier pass
                    // already migrated the live registrant -- flipping an
                    // already-`ReceiveOnly` registrant to `ReceiveOnly` again is
                    // a no-op.
                    let just_migrated_to_receive_only =
                        r.migrate_on_first_match && r.tier == RegistrantTier::ActiveSendReceive;
                    if just_migrated_to_receive_only {
                        r.tier = RegistrantTier::ReceiveOnly;
                    }
                    // ADR-100 Decision §4 (S6): "deadline restarted per accepted
                    // match" -- mirrors how `CP_P2Max`'s own deadline restarts
                    // per match in `wait_for_expected_response_inner`'s
                    // finite-count receive-phase loop. Only a
                    // created-receive-only (`NumSendCycles == 0`)
                    // `NumReceiveCycles == -1` or finite-`N > 0` registrant
                    // (ADR-182 widened this from the `-1`-only shape) ever
                    // has `cyclic_timeout_ms: Some(ms > 0)`; every other
                    // registrant (including a migrated, S5, IS-CYCLIC COP,
                    // and a created-receive-only `-2`) leaves this a no-op.
                    // This same restart applies unconditionally regardless of
                    // whether a finite-`N` registrant's `matches_needed` has
                    // also just been satisfied by this same match --
                    // `reap_expired_cyclic_registrants` resolves that
                    // precedence (count-completion wins) at reap time, not
                    // here.
                    if let Some(ms) = r.cyclic_timeout_ms.filter(|&ms| ms > 0) {
                        r.cyclic_deadline =
                            Some(tokio::time::Instant::now() + Duration::from_millis(ms as u64));
                    }
                    // ADR-146: pairing against the paired registrant's own
                    // captured TPI/SID context happens here, inside this same
                    // positive-match branch -- unique-response-ID scoping,
                    // connect-generation gating, and tier gating already came
                    // for free from the scan above.
                    let (ecu_timing_change, superseded_timing_seq) =
                        observe_registrant_timing_change(
                            r,
                            payload,
                            frame_seq,
                            concat_meta.uudt_routed,
                            concat_meta.raw_prefix,
                        );
                    // Codex review, PR #17, round 6: closes the in-batch
                    // window `migrate_registrant_to_receive_only`'s own
                    // end-of-pass `timing_cfg = None` clear (mirroring
                    // `rc_cfg`) leaves open -- that call runs only after the
                    // WHOLE poll pass has already returned, so a later frame
                    // in this SAME `PassThruReadMsgs` batch would otherwise
                    // still find `timing_cfg` populated, bind through the
                    // `Tier2` scan above (tier was already flipped inline),
                    // and re-enter the ADR-146 pairing block, making
                    // hardware/ComParam updates depend on adapter batching
                    // despite this mechanism's own "tier-2 never runs it"
                    // invariant. Cleared HERE, after (not before) this
                    // frame's own ADR-146 handling above, so the
                    // migration-triggering frame itself -- a legitimate
                    // tier-1 response that also happens to be this
                    // registrant's first accepted match -- still gets full
                    // handling; only a SUBSEQUENT frame in the same batch is
                    // affected. `rc_cfg` needs no equivalent clear here: its
                    // own pending-RC detection is gated on `scan ==
                    // AttributionScan::Tier1NonVacuous` (above), which a
                    // `Tier2`-scanned frame never reaches in the first place.
                    //
                    // PASS-LOCAL ONLY (edge-case-hunter finding, round 6):
                    // this mutates the snapshot `CopRegistrant` clone this
                    // whole frame loop iterates, not the live registrant --
                    // `timing_cfg` is not among the fields
                    // `merge_registrant_writeback` copies back (unlike
                    // `tier`). `migrate_registrant_to_receive_only`'s own
                    // `r.timing_cfg = None` remains the only clear of the
                    // LIVE registrant's `timing_cfg` and stays load-bearing;
                    // it is not made redundant by this inline clear.
                    if just_migrated_to_receive_only {
                        r.timing_cfg = None;
                    }
                    return Some((
                        acceptance_id,
                        r.cop_handle,
                        ecu_timing_change,
                        superseded_timing_seq,
                        false,
                        classification,
                    ));
                }
            }
        }
    }
    None
}

/// Computes one qualifying `0xC3` Access Timing Parameter response's
/// [`PendingTimingChange`], per ADR-146's per-TPI rules, combining a TPI=2
/// observation into `accumulator` along the way ONLY when `cfg.functional`
/// (the running functional-addressing worst case, ADR-146 Decision,
/// "Functional-addressing worst case" -- gated on `CP_RequestAddrMode`,
/// Codex review, PR #17, round 3): a physically addressed exchange has
/// exactly one responder, so its own bytes are used as-is, un-combined.
/// `None` for TPI=0 (out of scope: `CP_ModifyTiming` only names TPI 1/2/3) or
/// a TPI=1 with no `default_timing` to reapply -- both documented no-ops
/// (ADR-146 Consequences).
///
/// `raw_prefix` (ADR-198 Phase 2, extending ADR-196 Decision item 3b's
/// shared re-basing discipline to KWP's own response-side anchor -- a gap
/// the original ADR-196 Decision item 3b plan did not enumerate, since
/// K-line RawMode was unreachable in Phase 1): the per-frame raw header
/// prefix width preceding `payload`'s actual TPI=2 timing bytes for a
/// RawMode=ON K-line CLL -- `0` unconditionally for RawMode=OFF, leaving
/// this byte-for-byte the pre-ADR-198 behavior. Mirrors
/// `observe_session_timing_response`'s identical `raw_prefix` parameter.
fn observe_timing_response(
    cfg: &AccessTimingConfig,
    payload: &[u8],
    accumulator: &mut Option<TimingAccumulator>,
    raw_prefix: usize,
) -> Option<PendingTimingChange> {
    match cfg.tpi {
        // TPI=1 (set to default values): the response carries no timing
        // bytes of its own -- reapply the already-stored TimingSet=1 entry,
        // if any. Nothing new is recorded into CP_AccessTiming_Ecu (it
        // already holds this same entry).
        Some(1) => {
            let bytes = cfg.default_timing?;
            Some(PendingTimingChange {
                derived: access_timing_bytes_to_comparams(bytes).to_vec(),
                ecu_entry: None,
            })
        }
        // TPI=2 (read active values): the response's 5 timing bytes are
        // always recorded as CP_AccessTiming_Ecu's TimingSet=1 entry. The
        // derived engineering ComParams come from CP_AccessTimingOverride's
        // TimingSet=2 entry instead, when non-empty; otherwise from the
        // running functional-addressing worst-case-combined observation --
        // for a PHYSICALLY addressed request (`cfg.functional == false`)
        // there is exactly one possible responder, so each response's own
        // bytes ARE that ECU's current values and must not be folded with a
        // stale accumulator left over from an earlier response (Codex
        // review, PR #17, round 3: unconditional folding here treated a
        // physical exchange's own re-reads as if they were distinct ECUs in
        // a functional exchange, producing a synthetic min/max mixture
        // instead of latest-observed).
        Some(2) => {
            let observed: [u8; 5] = payload
                .get(raw_prefix + 2..raw_prefix + 7)?
                .try_into()
                .ok()?;
            let combined = if cfg.functional {
                let prev = match *accumulator {
                    Some(TimingAccumulator::Kwp(bytes)) => Some(bytes),
                    _ => None,
                };
                let folded = match prev {
                    Some(acc) => combine_worst_case_timing(acc, observed),
                    None => observed,
                };
                *accumulator = Some(TimingAccumulator::Kwp(folded));
                folded
            } else {
                observed
            };
            let derive_from = cfg.override_timing.unwrap_or(combined);
            Some(PendingTimingChange {
                derived: access_timing_bytes_to_comparams(derive_from).to_vec(),
                // Codex review, PR #17: must be the running worst-case
                // `combined` value, not this single response's own
                // `observed` bytes -- otherwise CP_AccessTiming_Ecu's
                // TimingSet=1 entry reflects whichever ECU happened to
                // respond last in a functional-addressing exchange, and a
                // later TPI=1 reapply would undo the safe combined timing
                // already pushed to hardware with that one ECU's
                // potentially less-conservative values.
                ecu_entry: Some(EcuTimingRecord::AccessTiming {
                    timing_set: 1,
                    bytes: combined,
                }),
            })
        }
        // TPI=3 (set given values): the response carries no payload of its
        // own -- the ORIGINAL REQUEST's captured 5 bytes are applied and
        // recorded as a TimingSet=3 entry. CP_AccessTimingOverride's clause
        // only names TPI=2, so it never applies here.
        Some(3) => {
            let Some(bytes) = cfg.tpi3_request_bytes else {
                // The client's own TPI=3 request was shorter than the 5
                // accompanying timing bytes ISO 14230-2 requires -- a
                // malformed request this service safely no-ops on (no
                // panic, `AccessTimingConfig::with_request` never indexes
                // past what's present), but worth surfacing for
                // diagnosis since it silently means CP_ModifyTiming had
                // nothing to derive from a positive `0xC3 03` response.
                debug!(
                    "qualifying TPI=3 0xC3 response with no captured request timing bytes (malformed/truncated request); no ComParam change derived"
                );
                return None;
            };
            Some(PendingTimingChange {
                derived: access_timing_bytes_to_comparams(bytes).to_vec(),
                ecu_entry: Some(EcuTimingRecord::AccessTiming {
                    timing_set: 3,
                    bytes,
                }),
            })
        }
        // TPI=0 or any other/unknown value: out of scope.
        _ => None,
    }
}

/// ADR-150: computes one qualifying SID 0x50 DiagnosticSessionControl
/// response's [`PendingTimingChange`] against `cfg`'s captured session type,
/// combining a functionally-addressed observation into `accumulator` the
/// same way `observe_timing_response`'s TPI=2 arm does for KWP -- both
/// `P2Max`/`P2Star` are client-side timeout ceilings with no P2Min-analog
/// direction, so both fold toward the maximum. Echo check: byte 1 of the
/// response must equal the captured (already `0x7F`-masked) request session
/// type -- `0x50` never itself carries the suppress-positive-response bit,
/// so no masking is needed on the response side. Bounds-checks
/// `payload.get(2..6)` for the 4 timing bytes (`P2Server_max`,
/// `P2*Server_max`); a short response is a documented no-op, never a panic.
///
/// `raw_prefix` (ADR-196 Decision item 3b): the per-frame raw CAN-ID prefix
/// width (`events::header_footer_len`'s own already-audited computation,
/// reused rather than re-derived) preceding `payload`'s actual SID 0x50
/// bytes for a RawMode=ON CLL -- `0` unconditionally for RawMode=OFF, which
/// leaves every one of this function's byte positions byte-for-byte the
/// pre-ADR-196 behavior.
fn observe_session_timing_response(
    cfg: &SessionTimingConfig,
    payload: &[u8],
    accumulator: &mut Option<TimingAccumulator>,
    raw_prefix: usize,
) -> Option<PendingTimingChange> {
    let session = cfg.session?;
    if payload.get(raw_prefix) != Some(&0x50) || payload.get(raw_prefix + 1) != Some(&session) {
        return None;
    }
    let bytes: [u8; 4] = payload
        .get(raw_prefix + 2..raw_prefix + 6)?
        .try_into()
        .ok()?;
    let p2_ms = u16::from_be_bytes([bytes[0], bytes[1]]);
    let p2_star_10ms = u16::from_be_bytes([bytes[2], bytes[3]]);
    let combined = if cfg.functional {
        let prev = match *accumulator {
            Some(TimingAccumulator::Session {
                p2_ms,
                p2_star_10ms,
            }) => Some((p2_ms, p2_star_10ms)),
            _ => None,
        };
        let folded = match prev {
            Some((prev_p2, prev_star)) => (p2_ms.max(prev_p2), p2_star_10ms.max(prev_star)),
            None => (p2_ms, p2_star_10ms),
        };
        *accumulator = Some(TimingAccumulator::Session {
            p2_ms: folded.0,
            p2_star_10ms: folded.1,
        });
        folded
    } else {
        (p2_ms, p2_star_10ms)
    };
    // CP_SessionTimingOverride redirect (ADR-146's CP_AccessTimingOverride
    // precedent): a matching override entry for the echoed session redirects
    // which values the *derived* ComParams are computed from, without
    // changing what gets recorded into CP_SessionTiming_Ecu below (that
    // always reflects the observed/combined value).
    let derive_from = cfg
        .override_entries
        .iter()
        .find(|&&(s, ..)| s == session as u16)
        .map(|&(_, p2_max_ms, p2_star_10ms)| (p2_max_ms, p2_star_10ms))
        .unwrap_or(combined);
    Some(PendingTimingChange {
        derived: session_timing_to_comparams(
            derive_from.0,
            derive_from.1,
            cfg.can_transmission_time_us,
        )
        .to_vec(),
        ecu_entry: Some(EcuTimingRecord::SessionTiming {
            session: session as u16,
            p2_max_ms: combined.0,
            p2_star_10ms: combined.1,
        }),
    })
}

/// ADR-150: maps a UDS DiagnosticSessionControl response's `P2Server_max`
/// (ms resolution)/`P2*Server_max` (10 ms resolution) pair to the derived
/// D-PDU ComParams at 1 µs resolution, adding `CP_CanTransmissionTime` to
/// both -- the SAME full-addition formula for both `CP_P2Max` and
/// `CP_P2Star` (no 0.5x factor on the P2Star side; confirmed against
/// ADR-146's own `CP_P2Star`-derivation precedent rather than an unverified
/// alternative). The 1 ms / 10 ms resolutions themselves are independently
/// verifiable in-repo -- ISO 22900-2:2022 §B.3.3.2.2's
/// `PDU_PARAM_STRUCT_SESS_TIMING` structfield documents `P2Max_high/low` at
/// 1 ms and `P2Star_high/low` at 10 ms -- unlike the wire request/response
/// byte *layout* itself, which ISO 22900-2 does not specify (that came from
/// the project owner directly; see ADR-150's provenance note).
///
/// `ctt_us` is an unvalidated client-supplied `SetComParam` value (no range
/// check exists anywhere on `CP_CanTransmissionTime`, unlike KWP's
/// wire-byte-bounded inputs) -- `saturating_add` rather than plain `+`
/// (edge-case-hunter finding: a near-`u32::MAX` `CP_CanTransmissionTime`
/// plus a genuine response panicked this channel's poll task, which nothing
/// restarts per ADR-146's own accepted residual on `spawn_channel_poll_task`).
/// Saturating to `u32::MAX` is a deliberately nonsensical timeout for a
/// nonsensical input, not a silent wraparound.
fn session_timing_to_comparams(
    p2_max_ms: u16,
    p2_star_10ms: u16,
    ctt_us: u32,
) -> [(ComParamId, u32); 2] {
    [
        (
            ComParamId(j2534_0404::P2_MAX),
            (p2_max_ms as u32 * 1_000).saturating_add(ctt_us),
        ),
        (
            PARAM_P2_STAR,
            (p2_star_10ms as u32 * 10_000).saturating_add(ctt_us),
        ),
    ]
}

/// Element-wise ADR-146 functional-addressing worst-case combination of two
/// `[P2Min, P2Max, P3Min, P3Max, P4Min]` raw wire-byte observations: `P2Min`
/// takes the minimum (the tester must be ready to accept the earliest legal
/// responder); `P2Max`/`P3Min`/`P3Max` (-> `CP_P2Star`)/`P4Min` take the
/// maximum (the tester must accommodate the slowest/most-demanding
/// responder).
fn combine_worst_case_timing(a: [u8; 5], b: [u8; 5]) -> [u8; 5] {
    [
        a[0].min(b[0]),
        a[1].max(b[1]),
        a[2].max(b[2]),
        a[3].max(b[3]),
        a[4].max(b[4]),
    ]
}

/// Maps the 5 raw wire bytes ISO 14230-2's Access Timing Parameter service
/// carries (`[P2Min, P2Max, P3Min, P3Max, P4Min]`) to the derived D-PDU
/// ComParams at 1 µs resolution (ADR-146 Decision, "wire-byte-to-ComParam
/// mapping") -- kept as one small pure function so the flagged P2Max
/// resolution residual stays isolated and easy to find.
fn access_timing_bytes_to_comparams(bytes: [u8; 5]) -> [(ComParamId, u32); 5] {
    let [p2_min, p2_max, p3_min, p3_max, p4_min] = bytes;
    [
        (ComParamId(j2534_0404::P2_MIN), p2_min as u32 * 500),
        // TODO(ADR-146 residual): P2Max's resolution is documented in ISO
        // 22900-2 as table-driven per an external ISO 14230-2 table this
        // workspace cannot verify (no primary-text access, per this ADR's
        // Context); the same 500 us (0.5 ms) resolution as P2Min/P3Min/P4Min
        // is applied provisionally, pending verification against that table.
        (ComParamId(j2534_0404::P2_MAX), p2_max as u32 * 500),
        (ComParamId(j2534_0404::P3_MIN), p3_min as u32 * 500),
        // P3Max maps to CP_P2Star, NOT CP_P3Max -- ISO 22900-2's struct
        // documentation ties this specific field to the tester-side
        // CP_P2Star at 250 ms resolution (unambiguous, unaffected by the
        // P2Max residual above).
        (PARAM_P2_STAR, p3_max as u32 * 250_000),
        (ComParamId(j2534_0404::P4_MIN), p4_min as u32 * 500),
    ]
}

/// Combines two derived-ComParam observations of the same `id` using the
/// SAME per-field worst-case direction table as [`combine_worst_case_timing`]
/// (`CP_P2Min` -> minimum, everything else -> maximum), used ONLY at the
/// cross-CLL `timing_delta` fold (`poll_rx_inner`): two ISO14230 CLLs at the
/// same `(j2534_protocol_id, baud_rate)` map to the same `ChannelKey` and can
/// share one physical channel, so more than one CLL can independently
/// qualify against the same physical `0xC3` frame in a single pass, each with
/// its own concurrently-live session and its own legitimate timing
/// requirement. Applying this worst-case direction table here, rather than
/// last-write-wins across independent `apply_params_to_hardware` calls, keeps
/// the single shared hardware push well-defined and able to satisfy every
/// live session sharing the channel (ADR-146 amended, Codex review round 4,
/// PR #17: this is the genuinely different cross-CLL case, distinct from the
/// cross-REGISTRANT case `select_latest_timing_changes` now resolves by
/// arrival order instead).
fn combine_derived_timing_value(id: ComParamId, a: u32, b: u32) -> u32 {
    if id == ComParamId(j2534_0404::P2_MIN) {
        a.min(b)
    } else {
        a.max(b)
    }
}

/// Inserts (or replaces) `CP_AccessTiming_Ecu`'s `TimingSet == timing_set`
/// entry in `params` with `bytes`, preserving any other `TimingSet` entries
/// already present -- e.g. a TPI=2 (`TimingSet=1`) observation and a later
/// TPI=3 (`TimingSet=3`) adoption on the same CLL both stay recorded
/// (ADR-146 Decision, "Structfield plumbing").
fn store_access_timing_ecu_entry(params: &mut ComParamSet, timing_set: u32, bytes: [u8; 5]) {
    let sf = params
        .structfield
        .entry(PARAM_ACCESS_TIMING_ECU)
        .or_insert_with(comparam_defaults::access_timing_empty);
    let list = match sf.data.get_or_insert_with(|| {
        vci_service_interface::param_structfield::Data::AccessTiming(
            vci_service_interface::ParamAccessTimingList::default(),
        )
    }) {
        vci_service_interface::param_structfield::Data::AccessTiming(list) => list,
        // CP_AccessTiming_Ecu is always AccessTiming-shaped by construction
        // (comparam_defaults::access_timing_empty/access_timing_zero, and
        // this function itself, are the only writers `link.active`/
        // `link.working` ever see for this key); `rpc_set_com_param`
        // additionally rejects any client SetComParam whose oneof variant
        // doesn't match this key's Structfield type.
        _ => unreachable!("PARAM_ACCESS_TIMING_ECU is always AccessTiming-shaped"),
    };
    let [p2_min, p2_max, p3_min, p3_max, p4_min] = bytes;
    let entry = vci_service_interface::ParamAccessTiming {
        p2_min: p2_min as u32,
        p2_max: p2_max as u32,
        p3_min: p3_min as u32,
        p3_max: p3_max as u32,
        p4_min: p4_min as u32,
        timing_set,
    };
    if let Some(existing) = list.entries.iter_mut().find(|e| e.timing_set == timing_set) {
        *existing = entry;
    } else {
        list.entries.push(entry);
    }
}

/// Inserts (or replaces) `CP_SessionTiming_Ecu`'s `session == session` entry
/// in `params` with `p2_max_ms`/`p2_star_10ms` (ADR-150) -- mirrors
/// `store_access_timing_ecu_entry`'s exact shape (replace-by-key, not
/// accumulate), keyed by `session` instead of `TimingSet`. Stores the RAW
/// wire-resolution values (ms for `p2_max`, 10 ms units for `p2_star`), not
/// the derived 1 µs-resolution engineering ComParam values -- mirrors
/// `CP_AccessTiming_Ecu`'s own "record the wire observation, derive
/// separately" split (ADR-146 Decision, "Structfield plumbing").
fn store_session_timing_ecu_entry(
    params: &mut ComParamSet,
    session: u16,
    p2_max_ms: u16,
    p2_star_10ms: u16,
) {
    let sf = params
        .structfield
        .entry(PARAM_SESSION_TIMING_ECU)
        .or_insert_with(comparam_defaults::session_timing_empty);
    let list = match sf.data.get_or_insert_with(|| {
        vci_service_interface::param_structfield::Data::SessionTiming(
            vci_service_interface::ParamSessionTimingList::default(),
        )
    }) {
        vci_service_interface::param_structfield::Data::SessionTiming(list) => list,
        // CP_SessionTiming_Ecu is always SessionTiming-shaped by construction
        // (comparam_defaults::session_timing_empty and this function itself
        // are the only writers `link.active`/`link.working` ever see for
        // this key); `rpc_set_com_param` additionally rejects any client
        // SetComParam whose oneof variant doesn't match this key's
        // Structfield type (`validate_structfield_shape`).
        _ => unreachable!("PARAM_SESSION_TIMING_ECU is always SessionTiming-shaped"),
    };
    let entry = vci_service_interface::ParamSessionTiming {
        session: session as u32,
        p2_max: p2_max_ms as u32,
        p2_star: p2_star_10ms as u32,
    };
    if let Some(existing) = list
        .entries
        .iter_mut()
        .find(|e| e.session as u16 == session)
    {
        *existing = entry;
    } else {
        list.entries.push(entry);
    }
}

/// One CLL's qualifying Access Timing observations for one poll pass,
/// folded from every one of its registrants that produced a
/// [`PendingTimingChange`] (ADR-146 amendment, edge-case-hunter finding on
/// PR #17, amended again -- Codex review round 4): per-CLL stored values are
/// now the ARRIVAL-ORDER / LAST-EXCHANGE-WINS fold of THAT CLL's own
/// registrants' observations this pass (`select_latest_timing_changes`), not
/// a worst-case combination -- two different registrants qualifying on the
/// same CLL in one pass are, by construction, always two independent SID
/// 0x83 exchanges (never "multiple ECUs responding to the same broadcast",
/// which is already resolved WITHIN one registrant via its own
/// `timing_accumulator`, gated on `TimingChangeConfig.functional`), so the
/// later exchange's result simply supersedes the earlier one. A sibling CLL
/// sharing the same physical channel still gets its OWN independent fold --
/// only the hardware push (`timing_delta`) combines across CLLs, and that
/// cross-CLL fold is unaffected by this change (it stays worst-case, since
/// it genuinely represents multiple concurrently-live sessions sharing one
/// hardware constraint, not one exchange superseding another).
#[derive(Debug, Default, Clone)]
struct CombinedTimingChange {
    /// Derived engineering ComParam values, keyed by id.
    derived: HashMap<ComParamId, u32>,
    /// `CP_AccessTiming_Ecu` entries to store, keyed by `TimingSet` (a single
    /// pass can legitimately produce more than one distinct `TimingSet`,
    /// e.g. a TPI=2 `TimingSet=1` observation and a separate TPI=3
    /// `TimingSet=3` adoption from two different registrants).
    ecu_entries: HashMap<u32, [u8; 5]>,
    /// ADR-150: `CP_SessionTiming_Ecu` entries to store, keyed by `session`
    /// -- `(p2_max_ms, p2_star_10ms)`, same last-exchange-wins fold as
    /// `ecu_entries` above, just keyed differently (UDS has no `TimingSet`
    /// analog).
    session_entries: HashMap<u16, (u16, u16)>,
}

/// A fold-time snapshot of the SPECIFIC `link.working` values
/// `store_combined_timing_change` is about to (conditionally) overwrite --
/// captured under the SAME `logical_links` lock the fold itself runs under,
/// BEFORE the hardware push `.await` releases that lock (Codex round-7
/// finding, PR #17). `store_combined_timing_change` compares this snapshot
/// against the CURRENT `link.working` at store time (after re-acquiring the
/// lock) and skips a key/the whole structfield if it changed in between --
/// first-wins for a concurrent `SetComParam`/`CoptUpdateparam`-Working-copy
/// RPC that completed during the hardware-push await window, matching this
/// codebase's own established first-wins idiom for this exact race shape.
/// `link.active` needs no equivalent snapshot: every OTHER writer of Active
/// runs on the same single per-channel poll task as this fold, so they are
/// strictly sequential with it (see this struct's call site for the full
/// argument).
#[derive(Debug, Default, Clone)]
struct WorkingTimingSnapshot {
    /// `link.working.unum32.get(&id).copied()` for each key `combined.derived`
    /// is about to write, captured at fold time.
    derived: HashMap<ComParamId, Option<u32>>,
    /// `link.working.structfield.get(&PARAM_ACCESS_TIMING_ECU).cloned()`,
    /// captured at fold time -- compared as a WHOLE structfield, not
    /// per-entry: a client that replaced the entire struct during the
    /// window expects a `GetComParam` round-trip of exactly what it wrote,
    /// and merging our entry into their new struct would itself be a
    /// second, subtler lost update.
    access_timing_sf: Option<vci_service_interface::ParamStructfield>,
    /// ADR-150: `link.working.structfield.get(&PARAM_SESSION_TIMING_ECU).cloned()`,
    /// captured at fold time -- the identical whole-structfield first-wins
    /// guard as `access_timing_sf` above, applied independently (two
    /// separate guarded writes in `store_combined_timing_change`, not one
    /// combined check, since a pass can legitimately have either kind of
    /// exchange, both, or neither).
    session_timing_sf: Option<vci_service_interface::ParamStructfield>,
}

/// Folds every `changes` item into one [`CombinedTimingChange`] using
/// ARRIVAL-ORDER / LAST-EXCHANGE-WINS semantics: `changes` are sorted by
/// their pass-local frame sequence number ascending, then each one's
/// `derived`/`ecu_entry` values plainly OVERWRITE (not worst-case-combine)
/// whatever a strictly earlier change already wrote for the same key. This
/// is correct because every distinct registrant's `PendingTimingChange` is,
/// by construction, an independent SID 0x83 exchange (never "another ECU
/// responding to the SAME broadcast" -- that case is fully resolved WITHIN
/// one registrant's own `timing_accumulator`, gated on
/// `TimingChangeConfig.functional`, before it ever reaches here), so the
/// LATER exchange's result is simply the ECU's more current, superseding
/// state -- not a second data point to worst-case-combine against the
/// first (Codex review, PR #17, round 4; ADR-146 amended, overturning the
/// "third axis" bullet's original worst-case-fold choice for this exact
/// axis).
fn select_latest_timing_changes<'a>(
    changes: impl Iterator<Item = (usize, &'a PendingTimingChange)>,
) -> CombinedTimingChange {
    let mut ordered: Vec<(usize, &PendingTimingChange)> = changes.collect();
    ordered.sort_by_key(|(seq, _)| *seq);
    let mut combined = CombinedTimingChange::default();
    for (_, change) in ordered {
        for &(id, value) in &change.derived {
            combined.derived.insert(id, value);
        }
        match change.ecu_entry {
            Some(EcuTimingRecord::AccessTiming { timing_set, bytes }) => {
                combined.ecu_entries.insert(timing_set, bytes);
            }
            Some(EcuTimingRecord::SessionTiming {
                session,
                p2_max_ms,
                p2_star_10ms,
            }) => {
                combined
                    .session_entries
                    .insert(session, (p2_max_ms, p2_star_10ms));
            }
            None => {}
        }
    }
    combined
}

/// Stores one CLL's already-folded [`CombinedTimingChange`] into `link`
/// (ADR-146 amendment): `ecu_entries` are stored unconditionally -- they
/// record what the ECU reported/adopted on the wire, are never themselves
/// sent to hardware (`apply_params_to_hardware_locked` only reads
/// `unum32`), and TPI=1's reapply path depends on a `TimingSet=1` entry
/// existing regardless of whether this pass's OWN hardware push succeeded.
/// `derived` (the engineering ComParams, including hardware-unmapped
/// `CP_P2Star`) is stored all-or-nothing, gated on `hardware_push_ok` --
/// mirroring `handle_update_param`'s own `all_ok`-gated Working -> Active
/// promotion (ADR-086): "Active" means "actually on hardware," and storing
/// only some of one wire observation's 5 fields on a partial failure would
/// leave a frankenstate no single ISO 14230-2 exchange ever reported.
/// Returns whether `combined.derived` was actually promoted into
/// `link.active` (i.e. `hardware_push_ok`) -- callers use this to build the
/// `derived_stored` set the deferred-finalization delivery step corrects
/// `ReceivedFrame::ecu_timing_change` against. This flag is tied to ACTIVE
/// only (Codex round-7 finding, PR #17): Active is written unconditionally
/// (gated on `hardware_push_ok`, matching `handle_update_param`'s own
/// `all_ok`-gated Working -> Active promotion, ADR-086) since it genuinely
/// reflects "actually on hardware," but WORKING is now guarded against a
/// concurrent client write (see `WorkingTimingSnapshot`'s own doc comment)
/// -- a client's own in-flight `SetComParam` staging a DIFFERENT value must
/// not be silently clobbered by this pass's older, exchange-derived one,
/// even when that exchange's derived values DID reach Active/hardware.
fn store_combined_timing_change(
    link: &mut LogicalLinkState,
    combined: &CombinedTimingChange,
    hardware_push_ok: bool,
    working_snapshot: &WorkingTimingSnapshot,
) -> bool {
    for (&timing_set, &bytes) in &combined.ecu_entries {
        store_access_timing_ecu_entry(&mut link.active, timing_set, bytes);
    }
    // Codex round-7 finding, PR #17: only overwrite Working's
    // CP_AccessTiming_Ecu structfield if it is UNCHANGED since fold time --
    // a concurrent SetComParam that replaced it during the hardware-push
    // await window wins (first-wins), and this exchange's ecu_entries are
    // simply not merged into Working this pass (Active still gets them,
    // unconditionally, above).
    if link.working.structfield.get(&PARAM_ACCESS_TIMING_ECU)
        == working_snapshot.access_timing_sf.as_ref()
    {
        for (&timing_set, &bytes) in &combined.ecu_entries {
            store_access_timing_ecu_entry(&mut link.working, timing_set, bytes);
        }
    }
    // ADR-150: CP_SessionTiming_Ecu gets the IDENTICAL treatment as
    // CP_AccessTiming_Ecu above -- unconditional Active write, guarded
    // (independently -- a separate whole-structfield snapshot/compare, not a
    // combined check) Working write.
    for (&session, &(p2_max_ms, p2_star_10ms)) in &combined.session_entries {
        store_session_timing_ecu_entry(&mut link.active, session, p2_max_ms, p2_star_10ms);
    }
    if link.working.structfield.get(&PARAM_SESSION_TIMING_ECU)
        == working_snapshot.session_timing_sf.as_ref()
    {
        for (&session, &(p2_max_ms, p2_star_10ms)) in &combined.session_entries {
            store_session_timing_ecu_entry(&mut link.working, session, p2_max_ms, p2_star_10ms);
        }
    }
    if hardware_push_ok {
        for (&id, &value) in &combined.derived {
            link.active.unum32.insert(id, value);
            // Codex round-7 finding, PR #17: same first-wins guard, per key
            // -- a concurrent SetComParam(this specific ComParam) wins;
            // missing from the snapshot is treated conservatively as
            // "assume changed" (skip).
            if working_snapshot
                .derived
                .get(&id)
                .is_some_and(|snap| link.working.unum32.get(&id).copied() == *snap)
            {
                link.working.unum32.insert(id, value);
            }
        }
    }
    hardware_push_ok
}

/// ADR-146 (amended -- Codex round-2 Finding A, PR #17; amended again --
/// edge-case-hunter finding on the round-4 fix, PR #17; amended again --
/// Codex round-5 findings, PR #17): the pure, directly-unit-testable
/// predicate for whether a qualifying ADR-146 timing frame's
/// `ecu_timing_change` flag should read `true` after this pass's
/// fold/push/store outcome is known. Extracted (replacing the round-2/4
/// `correct_buffered_ecu_timing_change_flags`, which corrected a whole
/// buffered `Vec<ReceivedFrame>` in one pass) so it can be called per-frame
/// against `poll_rx_inner`'s new `CllQueueItem::PendingTimingFrame`
/// reservations (deferred-finalization delivery, `finalize_pending_timing_frame`)
/// without needing a live queue/lock to test it -- preserving this
/// mechanism's existing fail-without/pass-with unit-test style.
///
/// `true` iff ALL of:
/// - `derived_stored` contains this frame's CLL (the CLL's overall push
///   succeeded and the post-push `connect_generation` recheck passed);
/// - `derived_winner` maps this CLL to THIS frame's own `cop_handle` (this
///   frame's registrant, not a different one, is the one whose exchange
///   `select_latest_timing_changes` actually kept for this CLL this pass);
/// - `superseded_timing` does NOT contain `(cll_handle, cop_handle,
///   frame_seq)` -- true for a PHYSICALLY addressed registrant with more
///   than one qualifying response in one pass (round 3: no accumulation for
///   physical addressing), where an earlier response's own values were
///   completely overwritten by a LATER response from the SAME registrant
///   before ever being stored. A functionally-addressed registrant's
///   responses are never superseded this way -- every one of them
///   genuinely contributed to the running worst-case accumulator
///   (`timing_accumulator`), so every one keeps its flag as long as the
///   other two conditions hold.
fn timing_frame_flag_ok(
    cll_handle: u32,
    cop_handle: u32,
    frame_seq: usize,
    derived_stored: &HashSet<u32>,
    derived_winner: &HashMap<u32, u32>,
    superseded_timing: &HashSet<(u32, u32, usize)>,
) -> bool {
    derived_stored.contains(&cll_handle)
        && derived_winner.get(&cll_handle).copied() == Some(cop_handle)
        && !superseded_timing.contains(&(cll_handle, cop_handle, frame_seq))
}

#[cfg(test)]
#[path = "events_combined_timing_change_tests.rs"]
mod combined_timing_change_tests;

/// Frame-identity parameters `bind_frame` needs but `bind_registrant` and
/// `ConcatFrameMeta` don't (the latter is `CP_EnableConcatenation`-only) --
/// bundled into one small `Copy` struct purely to keep `bind_frame` under
/// clippy's `too_many_arguments` limit; no behavior or ownership change from
/// passing the same five values individually.
#[derive(Clone, Copy)]
struct FrameContext<'a> {
    is_content_frame: bool,
    is_tx_side: bool,
    frame_can_id: Option<u32>,
    payload: &'a [u8],
    unique_resp_identifier: u32,
}

/// ADR-100 Decision §3's full attribution precedence table for one delivered
/// frame at one CLL entry, replacing the old single-`MatchProbe` scan (which
/// only ever considered the one COP currently blocked in
/// `wait_for_expected_response`) with a scan over every registrant sharing
/// this CLL, tester-present interleaved at its documented rank:
///
///   1. Indication frames (SOM/TxDone/loopback/RxBreak) never reach steps
///      2/4/5 -- that is `is_content_frame` gating the two
///      `bind_registrant` calls below. They are NOT excluded from step 3:
///      ADR-099's SOM-herald/TX-echo discard arms specifically match
///      indication-bit frames, so tester-present's own signature check
///      below always runs, content frame or not.
///   2. Tier-1, non-vacuous claims (`AttributionScan::Tier1NonVacuous`).
///   3. Tester-present's own reply signature (`entry.tester_present_discard`)
///      -- unchanged matching rules (content/SOM-herald/TX-echo), only moved
///      to this rank. A match against ANY entry in the list discards the
///      frame outright (ADR-137 fourth Codex-review fix / round-4
///      restructure: each entry is an independently elicited send, not a
///      precedence chain -- this is "any entry matches", not
///      "first-match-wins" the way the surrounding tier scan is).
///   4. Tier-1, vacuous claims (`AttributionScan::Tier1Vacuous`).
///   5. Tier-2 / Receive Only (`AttributionScan::Tier2`).
///   6. Unbound -- a content frame reaching this fallthrough is discarded by
///      the caller (ADR-100 Decision §5, S8); an indication frame reaching
///      it (step 1's carve-out) keeps its existing `ResultData` delivery
///      path instead -- see [`FrameBinding::Unbound`]'s own doc comment for
///      how the caller tells the two apart.
///
/// `frame_seq` is threaded through to `bind_registrant` for ADR-146's
/// arrival-order timing-pairing resolution (see `bind_registrant`'s own doc
/// comment). `concat_meta`/`finalized_concat` are `CP_EnableConcatenation`-only
/// (ADR-148). `wrote_suspend`/`entry.queue_error_class` are `CP_SuspendQueueOnError`-only
/// (ADR-147) -- see `bind_registrant`'s own doc comment for the write-site
/// contract this function applies immediately after each `bind_registrant`
/// call returns a classification.
#[allow(clippy::too_many_arguments)]
fn bind_frame(
    entry: &mut CllRxEntry,
    frame_ctx: FrameContext<'_>,
    frame_seq: usize,
    concat_meta: ConcatFrameMeta<'_>,
    finalized_concat: &mut Vec<ConcatDelivery>,
    wrote_suspend: &mut bool,
) -> FrameBinding {
    let mut sighted_unhandled = false;

    // ADR-148 third Amendment (Fix 1): derive this frame's own source-
    // address byte from `entry.header_protocol` and the already-split
    // `concat_meta.header_bytes`, overriding whatever the caller passed in
    // `concat_meta.source_id` (always `None` at every call site -- this is
    // the one place that ever computes a real value). See
    // `ConcatFrameMeta::source_id`'s own doc comment for the exact
    // per-protocol byte offsets and the headerless/short-frame residual.
    // This is a DIFFERENT, deliberately separate mechanism from ADR-203's
    // `kline_j1850_source_addr` (above, ~line 1610): that one operates on
    // RAW pre-split bytes purely for `route_frame`'s RX delivery routing;
    // this one operates on ALREADY-SPLIT `concat_meta.header_bytes` purely
    // for `ConcatBuf` keying. Do not merge or refactor them together.
    let source_id = match entry.header_protocol {
        j2534_0404::ISO9141 | j2534_0404::ISO14230 => {
            (concat_meta.header_bytes.len() >= 3).then(|| concat_meta.header_bytes[2])
        }
        j2534_0404::J1850PWM | j2534_0404::J1850VPW => {
            (concat_meta.header_bytes.len() == 3).then(|| concat_meta.header_bytes[2])
        }
        _ => None,
    };
    let concat_meta = ConcatFrameMeta {
        source_id,
        ..concat_meta
    };

    if frame_ctx.is_content_frame
        && let Some((
            acceptance_id,
            cop_handle,
            ecu_timing_change,
            superseded_timing_seq,
            absorbed,
            classification,
        )) = bind_registrant(
            &mut entry.registrants,
            entry.connect_generation,
            frame_ctx.payload,
            frame_ctx.unique_resp_identifier,
            AttributionScan::Tier1NonVacuous,
            frame_seq,
            concat_meta,
            finalized_concat,
            &mut sighted_unhandled,
        )
    {
        if let Some(class) = classification {
            entry.queue_error_class = Some(class);
            // ADR-147 fifth amendment: Suspend-only capture (a partial
            // revert of the fourth amendment's generalization) -- see this
            // function's own doc comment.
            if class == QueueErrorClass::Suspend {
                *wrote_suspend = true;
            }
        }
        return FrameBinding::Registrant {
            acceptance_id,
            cop_handle,
            ecu_timing_change,
            superseded_timing_seq,
            absorbed,
        };
    }

    // Step 3: tester-present's own reply/SOM-herald/TX-echo signature
    // (ADR-088/093/099, unchanged matching rules -- see the doc comment this
    // block carried at its pre-ADR-100 call site, `TesterPresentDiscard`'s
    // own field docs, and `CllRxEntry::tester_present_discard`'s doc
    // comment). RX_BREAK is never a tester-present artifact and is excluded
    // unconditionally, even combined with another bit. `entry.tester_present_discard`
    // is now a list (ADR-137 fourth Codex-review fix / round-4 restructure,
    // one entry per still-open discard window across possibly multiple
    // sends) -- a match against ANY entry discards the frame; this is not a
    // first-match-wins scan, since every entry is an independently elicited
    // send.
    if concat_meta.rx_status_flags & RX_BREAK as u8 == 0
        && entry.tester_present_discard.iter().any(|discard| {
            let resp_can_id_ok = discard.target_can_ids.is_none_or(|ids| {
                frame_ctx
                    .frame_can_id
                    .is_some_and(|id| Some(id) == ids.usdt || Some(id) == ids.uudt)
            });
            let content_match = frame_ctx.is_content_frame
                && ((!discard.pos.is_empty() && frame_ctx.payload.starts_with(&discard.pos))
                    || (!discard.neg.is_empty() && frame_ctx.payload.starts_with(&discard.neg)))
                && resp_can_id_ok;
            let som_herald_match = !frame_ctx.is_tx_side
                && concat_meta.rx_status_flags & RX_START_OF_MESSAGE as u8 != 0
                && (!discard.pos.is_empty() || !discard.neg.is_empty())
                && resp_can_id_ok;
            // is_some_and, NOT is_none_or -- see `TesterPresentDiscard::tx_can_id`'s
            // own doc comment for why `None` here must not be read as
            // "unrestricted" the way `target_can_ids`'s `None` is.
            let tx_side_match = frame_ctx.is_tx_side
                && discard
                    .tx_can_id
                    .is_some_and(|id| frame_ctx.frame_can_id == Some(id));
            content_match || som_herald_match || tx_side_match
        })
    {
        return FrameBinding::TesterPresent;
    }

    if frame_ctx.is_content_frame {
        if let Some((
            acceptance_id,
            cop_handle,
            ecu_timing_change,
            superseded_timing_seq,
            absorbed,
            classification,
        )) = bind_registrant(
            &mut entry.registrants,
            entry.connect_generation,
            frame_ctx.payload,
            frame_ctx.unique_resp_identifier,
            AttributionScan::Tier1Vacuous,
            frame_seq,
            concat_meta,
            finalized_concat,
            &mut sighted_unhandled,
        ) {
            if let Some(class) = classification {
                entry.queue_error_class = Some(class);
                // ADR-147 fifth amendment: see the Tier1NonVacuous write
                // site above.
                if class == QueueErrorClass::Suspend {
                    *wrote_suspend = true;
                }
            }
            return FrameBinding::Registrant {
                acceptance_id,
                cop_handle,
                ecu_timing_change,
                superseded_timing_seq,
                absorbed,
            };
        }
        if let Some((
            acceptance_id,
            cop_handle,
            ecu_timing_change,
            superseded_timing_seq,
            absorbed,
            classification,
        )) = bind_registrant(
            &mut entry.registrants,
            entry.connect_generation,
            frame_ctx.payload,
            frame_ctx.unique_resp_identifier,
            AttributionScan::Tier2,
            frame_seq,
            concat_meta,
            finalized_concat,
            &mut sighted_unhandled,
        ) {
            if let Some(class) = classification {
                entry.queue_error_class = Some(class);
                // ADR-147 fifth amendment: see the Tier1NonVacuous write
                // site above.
                if class == QueueErrorClass::Suspend {
                    *wrote_suspend = true;
                }
            }
            return FrameBinding::Registrant {
                acceptance_id,
                cop_handle,
                ecu_timing_change,
                superseded_timing_seq,
                absorbed,
            };
        }
    }

    // ADR-147 correction: the ONLY place an unbound frame's classification
    // is written -- and only when SOME registrant's own `rc_cfg` sighted it
    // as unhandled somewhere during the scans above, regardless of which
    // scan pass (or whether any registrant ever bound it at all). This is
    // always a `Suspend` classification, so it always sets `wrote_suspend`.
    if sighted_unhandled {
        entry.queue_error_class = Some(QueueErrorClass::Suspend);
        *wrote_suspend = true;
    }

    FrameBinding::Unbound
}

/// Resolves a bound frame's `cop_tag` from `entry`'s own per-pass
/// `cop_tags` snapshot (`CllRxEntry::cop_tags`'s own doc comment covers the
/// capture side and the exact race this closes) -- `None` when `cop_handle`
/// is `None` (an unbound frame never carries a tag), or when it is present
/// but absent from the snapshot (no tag to echo, whether the COP genuinely
/// has none or its `primitives` entry was already gone even at snapshot
/// time). Purely synchronous and infallible: this never touches `primitives`
/// itself, so there is no lock to await and nothing left to race by the time
/// this is called.
fn resolve_frame_cop_tag(entry: &CllRxEntry, cop_handle: Option<u32>) -> Option<Vec<u8>> {
    cop_handle.and_then(|h| entry.cop_tags.get(&h).cloned())
}

/// FlowControl capture state used by the software ISO-TP TX driver (ADR-046):
/// FlowControl frames on the channel whose CAN ID matches `fc_can_id` (`None`
/// = any) are recorded here and withheld from fan-out.
struct FcCapture {
    fc_can_id: Option<u32>,
    /// Addressing used to parse the captured frame (`SoftIsoTpTx::fc_rx_addressing`,
    /// extended addressing, ADR-046 addendum).
    addressing: Addressing,
    /// Every matching FlowControl frame captured so far in this FC-wait
    /// cycle, in arrival order. A single `PassThruReadMsgs` batch can
    /// return more than one (`MAX_POLL_MESSAGES = 8`); the internal FS_WAIT
    /// flood guard (ADR-124) requires counting every WAIT in a burst, not
    /// just the first one per poll (Codex review finding, PR #130).
    got: VecDeque<(u8, u8, u8)>,
}

/// Handles shared by every stage of one physical channel's poll-task
/// pipeline -- from `spawn_channel_poll_task` down through RX polling,
/// dispatch, and each `TxItem` handler. Bundling them here lets each stage's
/// signature carry only the parameters specific to that call, instead of
/// every handle the pipeline as a whole needs.
///
/// Each field is the SAME handle this pipeline already threaded through
/// individually; grouping them is a signature simplification only -- no
/// `Mutex` is merged, and nothing about what is locked, when, or in what
/// order changes.
pub(super) struct ChannelPollCtx {
    /// The physical channel this poll task owns; fixed for the task's
    /// lifetime.
    pub(super) channel_id: ChannelId,
    pub(super) primitives: Arc<Mutex<HashMap<u32, CopEntry>>>,
    pub(super) api: Arc<Mutex<J2534Api0404>>,
    pub(super) logical_links: Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    /// Per-physical-channel drain watermark (ADR-101 Decision §E), the SAME
    /// shared instance across every physical channel's poll task on this
    /// module (mirroring `logical_links`, not the `last_bus_activity`
    /// per-physical-channel pattern) -- so a CLL's primary poll task can read
    /// its UUDT companion channel's own watermark. Written by `poll_rx_inner`
    /// at the end of an EXHAUSTIVE pass (after that pass's own writeback has
    /// landed); read by `reap_expired_cyclic_registrants`/
    /// `is_cyclic_reap_sound` and by `wait_for_expected_response_inner`'s
    /// tier-1 deadline-break check.
    pub(super) drain_watermarks: Arc<Mutex<HashMap<ChannelId, tokio::time::Instant>>>,
    pub(super) subscriptions: Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    pub(super) module_state: Arc<Mutex<ModuleState>>,
    pub(super) module_event_buf: Arc<Mutex<VecDeque<EventItem>>>,
    pub(super) system_event_buf: Arc<Mutex<VecDeque<EventItem>>>,
    /// COP handle currently being executed by the poll task, or `None` when
    /// idle; shared with `SharedChannel::executing_cop` for `GetStatus`.
    pub(super) executing_cop: Arc<Mutex<Option<u32>>>,
    /// `CP_P3Func`/`CP_P3Phys` minimum inter-request gap state (ADR-060).
    pub(super) last_func_tx: Arc<Mutex<Option<TxGapState>>>,
    pub(super) last_phys_tx: Arc<Mutex<Option<TxGapState>>>,
    /// Timestamp of the last TX or RX bus activity observed on this shared
    /// physical channel -- distinct from `last_func_tx`/`last_phys_tx`
    /// (TX-only, per-addressing-bucket): this is the single per-channel
    /// clock `CP_TesterPresentSendType = 1`'s idle-triggered dispatch
    /// compares each CLL's configured interval against (ADR-083). Stamped in
    /// `poll_rx_inner` (any received frame) and `transmit_request` when its
    /// `count_as_bus_activity` parameter is `true` -- every transmit except
    /// an idle-mode CLL's own one-shot send, which is excluded so a mode-1
    /// CLL's own keep-alive cannot resynchronize (and starve) a same-interval
    /// sibling CLL on the same channel.
    pub(super) last_bus_activity: Arc<Mutex<tokio::time::Instant>>,
    /// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): `false` only
    /// for a physical channel connected on `PROTOCOL_ETHERNET_NDIS` (clause
    /// 24.2.5.2: `PassThruReadMsgs` always returns `ERR_NOT_SUPPORTED`,
    /// which would otherwise misfire this poll task's hard-read-error-
    /// closes-the-channel rule moments after connect). `true` for every
    /// other protocol. Computed once, at poll-task spawn time
    /// (`spawn_new_shared_channel`), from the channel's protocol -- fixed
    /// for the task's lifetime, same as `channel_id`. Gates only
    /// `poll_rx_inner`'s shared RX pass; no other per-tick duty needs
    /// gating (ADR-194 Decision: nothing can enqueue a `TxItem` for this
    /// protocol once the COP gate in `rpc_primitive.rs` is in place, and no
    /// ComParam allowlist entry exists for tester-present/timing parameters
    /// on this protocol either).
    pub(super) rx_supported: bool,
    /// Cloned `J2534Service` handle (cheap: every field is an `Arc`), used
    /// only by `TxItem::UpdateParam`'s `handle_update_param` to call
    /// `J2534Service::promote_unique_resp_id_table`, which needs
    /// `shared_channels` / `can_channel_mode` / `resolved_can_channel_mode`
    /// -- state this poll task otherwise has no access to (ADR-068).
    pub(super) service: J2534Service,
}

/// Outcome of one [`push_cll_event`] call (ADR-115, round-3 correction).
/// `push_cll_event` runs unconditionally on every `deliver_or_enqueue` call
/// -- cap/mode enforcement (and hence `Lost`) is evaluated regardless of
/// whether a live `SubscribeEvent` subscriber exists, exactly like the
/// original pre-A2-6 code. This outcome tells the caller whether a
/// `PDU_EVT_DATA_LOST` notification is owed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PushOutcome {
    /// Inserted without needing to make room. No `Lost` notification.
    Inserted,
    /// `OverwriteOldest`: evicted the oldest buffered entry, then inserted
    /// `item`.
    Evicted,
    /// `DiscardNewest`: buffer was already at `cap`; `item` was discarded,
    /// not inserted.
    Discarded,
}

impl PushOutcome {
    /// `true` for `Evicted` or `Discarded` -- either way the caller owes
    /// exactly one `PDU_EVT_DATA_LOST` notification.
    pub(super) fn lost(self) -> bool {
        matches!(self, PushOutcome::Evicted | PushOutcome::Discarded)
    }
}

/// Pushes `item` onto a CLL's per-handle event queue, honoring its
/// configured event-queue capacity/overflow policy
/// (`PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`,
/// `CllEventQueue::event_queue_cap`/`event_queue_mode`): `OverwriteOldest`
/// pops the oldest buffered entry to make room (the pre-existing, hardcoded
/// `RX_BUF_CAPACITY` behavior every CLL defaults to); `DiscardNewest` drops
/// `item` instead of inserting it once the buffer is already at `cap`.
///
/// ADR-115 round-3 correction: called unconditionally, first thing, by
/// [`deliver_or_enqueue`] for every item from all three entry sources
/// (`poll_rx_inner`'s ordinary frame fan-out, `handle_start_comm`'s
/// synthetic fast-init response frame, `send_error_event`'s async error
/// events) -- regardless of whether a live `SubscribeEvent` subscriber
/// exists. `deliver_or_enqueue` then opportunistically drains whatever ends
/// up in `rx_buf` (including possibly `item` itself) out to a live
/// subscriber, if any, in the same critical section.
///
/// Returns a [`PushOutcome`] so the caller can tell whether a
/// `PDU_EVT_DATA_LOST` notification is owed.
fn push_cll_event(
    buf: &mut VecDeque<CllQueueItem>,
    cap: usize,
    mode: EventQueueMode,
    item: CllQueueItem,
) -> PushOutcome {
    if buf.len() >= cap {
        match mode {
            EventQueueMode::OverwriteOldest => {
                buf.pop_front();
                buf.push_back(item);
                return PushOutcome::Evicted;
            }
            EventQueueMode::DiscardNewest => return PushOutcome::Discarded,
        }
    }
    buf.push_back(item);
    PushOutcome::Inserted
}

/// Delivers `item` to a CLL's event queue (ADR-115, round-3 correction):
/// `item` is *always* pushed through `push_cll_event` first -- cap/mode
/// enforcement (and hence `PDU_EVT_DATA_LOST`) runs unconditionally,
/// regardless of whether a live `SubscribeEvent` subscriber exists, exactly
/// like the original pre-A2-6 code. Only after that does a live subscriber
/// (if any) get an opportunistic chance to drain `rx_buf`'s entire current
/// contents out live, FIFO, in the same critical section (no `.await` gap
/// between the push and the drain).
///
/// This supersedes round 2's "live send first, `push_cll_event` only as a
/// fallback" design: since `SubscriptionSender` is an
/// `mpsc::UnboundedSender`, that live-first send only ever fails once the
/// receiver is permanently gone, so for any live, healthy subscriber
/// `push_cll_event` (and `Lost`) was never reached at all -- confirmed
/// empirically (10 items pushed through round 2's logic with a live
/// receiver: all 10 delivered live, zero `Lost`, even at `cap == 1`). See
/// `docs/adr/ADR-115-pdu-evt-data-lost-emission.md`'s "Correction (round 3,
/// ...)" section for the full rationale.
///
/// **ADR-115 round 6 (design-advisor redesign, superseding round 4/5's
/// generation-gate)**: this function takes no subscriber parameter at all.
/// Call sites (`poll_rx_inner`'s frame fan-out, `send_error_event`,
/// `handle_start_comm`'s fast-init synthetic frame) used to clone a
/// `SubscriberRef` from the `subscriptions` map *before* taking the `rx_buf`
/// lock, and that clone could go stale in the gap between the two lock
/// acquisitions if a concurrent `rpc_subscribe_event` call replaced the
/// subscription in between -- a real bug round 4's generation counter
/// patched over without addressing the root cause (a separate, ahead-of-time
/// capture of "who is the current subscriber" instead of asking the queue
/// itself, at the point of use, under the lock this function already holds).
/// `CllEventQueue::live_sender` eliminates the capture step entirely: the
/// queue IS the single source of truth for "who is the current live
/// subscriber", read fresh, right here, every time. There is no longer
/// anything that can go stale between a capture and a lock acquisition,
/// because there is no separate capture.
///
/// 1. `push_and_notify_lost` runs `push_cll_event(item)` first,
///    unconditionally, honoring this CLL's queue cap/mode. If it reports a
///    drop (`Evicted`/`Discarded`), that item's `Lost` semantics are decided
///    right here -- before any live-delivery attempt, so `Lost` and the
///    surviving payload can never both refer to the same discarded item
///    (`DiscardNewest`'s discarded item never enters `buf`;
///    `OverwriteOldest`'s `Lost` corresponds to the genuinely-evicted older
///    item, not `item` itself). **Self-heal (round 6)**: if the `Lost` send
///    fails -- the receiver has been dropped, deterministic for
///    `mpsc::UnboundedSender`, not transient -- `queue.live_sender` is
///    cleared to `None` right here, under the lock this function already
///    holds.
/// 2. `drain_queue_live` then attempts to drain `buf`, FIFO (`pop_front`),
///    converting each item (`cll_queue_item_to_event_item`/
///    `notification_from_event_item`) and sending it to
///    `queue.live_sender`, if any -- **stopping at the first
///    `CllQueueItem::PendingTimingFrame`** it encounters (ADR-146 deferred-
///    finalization delivery, Codex round-5 finding, PR #17: an unfinalized
///    reservation and everything behind it must wait for
///    `finalize_pending_timing_frame`). No live subscriber
///    (`queue.live_sender.clone()` is `None`) is a no-op: `buf` just
///    accumulates as backlog for a later `GetEventItem` poll or a future
///    subscriber. The first failed send during the drain loop pushes that
///    item back onto the FRONT of `buf`, clears `queue.live_sender` the same
///    way, and stops draining, leaving the remainder for a later drain
///    attempt by whichever subscriber (if any) attaches next.
///
/// **Lock-order rule this function must never violate**: this function holds
/// a per-CLL queue lock (`CllEventQueue`'s own `Mutex`, i.e.
/// `LogicalLinkState::rx_buf`) for its whole duration and must NEVER acquire
/// `J2534Service::subscriptions` while holding it -- that would reverse
/// `rpc_subscribe_event`'s own established `subscriptions` -> queue nesting
/// (see `J2534Service::logical_links`'s doc comment, `service.rs`, for the
/// full lock-order table) and deadlock against it. This function does not
/// need `subscriptions` at all: `queue.live_sender` is everything it reads.
/// `event_queue_cap`/`event_queue_mode`/`result_buffer_limit` are read fresh
/// from the queue itself, under this same lock, at the point of use (Codex
/// review on PR #3, ADR-140 follow-up -- see `CllEventQueue`'s own doc
/// comment); `CllQueueTarget` below carries only `rx_buf` for that reason.
/// `send_cop_status` still needs to resolve `rx_buf` from `logical_links`
/// ahead of time (see [`resolve_queue_target`]) so the lookup can happen
/// strictly *before* `primitives` is acquired at every `send_cop_status`
/// call site -- the restructuring `send_cop_status`'s own doc comment
/// describes -- but that is purely about the `logical_links -> primitives`
/// lock order for finding `rx_buf`, not about any queue-policy field.
pub(super) struct CllQueueTarget {
    pub(super) rx_buf: Arc<Mutex<CllEventQueue>>,
}

impl CllQueueTarget {
    pub(super) fn from_link(link: &LogicalLinkState) -> Self {
        Self {
            rx_buf: Arc::clone(&link.rx_buf),
        }
    }
}

/// Resolves the queue target for `cll_handle`. MUST be called BEFORE
/// `primitives` is acquired by the caller (lock order is `logical_links ->
/// primitives`, never reversed) -- mirrors `send_cll_status`'s own
/// `logical_links` lookup pattern.
///
/// **No staleness window left to document (Codex review on PR #3, ADR-140
/// follow-up, superseding the prior "staleness window" note this doc comment
/// used to carry):** the only thing this resolves is `rx_buf` itself, an
/// `Arc` clone that is always the SAME queue for as long as this CLL exists
/// -- there is no queue-policy field captured here to go stale between this
/// call and the caller's later `primitives` (and, for a terminal status,
/// `terminal_cops`) acquisition. `event_queue_cap`/`event_queue_mode`/
/// `result_buffer_limit` are read fresh, under the queue's own lock, by
/// `deliver_or_enqueue` at the point of use instead.
pub(super) async fn resolve_queue_target(
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    cll_handle: u32,
) -> Option<CllQueueTarget> {
    let links = logical_links.lock().await;
    links.get(&cll_handle).map(CllQueueTarget::from_link)
}

/// Pushes `item` via `push_cll_event` and, if it reports a drop and a live
/// subscriber exists, sends the `Lost` notification (with ADR-115 round-6
/// self-heal on a dead sender). Shared by `deliver_or_enqueue` and
/// `reserve_pending_timing_frame` -- both need identical cap/mode
/// enforcement and `Lost` semantics; only what happens AFTER differs (an
/// ordinary item drains immediately, a reservation does not deliver until
/// finalized).
fn push_and_notify_lost(
    queue: &mut CllEventQueue,
    cll_handle: u32,
    item: CllQueueItem,
) -> PushOutcome {
    // Read fresh, under this exact lock, at the point of use -- not from a
    // value some caller captured ahead of time (Codex review on PR #3,
    // ADR-140 follow-up; see `CllEventQueue`'s own doc comment). Copied out
    // before `push_cll_event`'s `&mut queue.items` borrow below since both
    // are `Copy`.
    let event_queue_cap = queue.event_queue_cap;
    let event_queue_mode = queue.event_queue_mode;

    let outcome = push_cll_event(&mut queue.items, event_queue_cap, event_queue_mode, item);

    if outcome.lost()
        && let Some(tx) = queue.live_sender.clone()
        && tx.send(Ok(make_lost_notification(cll_handle))).is_err()
    {
        // Dead sender (ADR-115 round 6 self-heal): the receiver side is
        // gone, so nothing is reading this channel anymore -- clear
        // `live_sender` right here, under the lock this function already
        // holds, rather than leaving it pointing at an orphaned channel that
        // would otherwise keep "succeeding" (`Ok`) on every future send
        // attempt with nobody ever receiving. The `Lost` notification itself
        // has no queue representation to fall back to (see
        // `make_lost_notification`'s own doc comment) so it is simply
        // dropped; the item that triggered it was already accounted for by
        // `push_cll_event`'s own eviction/discard above.
        queue.live_sender = None;
    }

    outcome
}

/// Drains `queue.items` live, FIFO, to `queue.live_sender` if any (ADR-115
/// round 6), STOPPING at the first `CllQueueItem::PendingTimingFrame` it
/// encounters (Codex round-5 finding, PR #17) -- an unfinalized reservation
/// and everything queued behind it must not be delivered live until
/// `finalize_pending_timing_frame` corrects and downgrades it to a plain
/// `Frame`. This is the barrier that lets a reservation's queue POSITION be
/// decided at arrival time (Finding 1's requirement) while its flag value
/// and delivery stay deferred (round 2's original requirement) -- WITHOUT
/// requiring the two to be decided at the same time, which is what made the
/// old side-buffer (`buffered_frames`) necessary in the first place.
fn drain_queue_live(queue: &mut CllEventQueue, cll_handle: u32) {
    let Some(tx) = queue.live_sender.clone() else {
        return;
    };
    loop {
        if matches!(
            queue.items.front(),
            Some(CllQueueItem::PendingTimingFrame { .. })
        ) {
            break;
        }
        let Some(front) = queue.items.pop_front() else {
            break;
        };
        let result_buffer_limit = queue.result_buffer_limit;
        let notification = notification_from_event_item(
            cll_handle,
            cll_queue_item_to_event_item(cll_handle, result_buffer_limit, &front),
        );
        if tx.send(Ok(notification)).is_err() {
            queue.items.push_front(front);
            queue.live_sender = None;
            break;
        }
    }
}

/// Delivers `item` to a CLL's event queue: pushed via `push_and_notify_lost`
/// (cap/mode enforcement, `Lost` semantics), then drained live via
/// `drain_queue_live` in the same critical section (no `.await` gap between
/// the push and the drain). See those two functions' own doc comments for
/// the full mechanism; see `push_cll_event`/`PushOutcome` for the cap/mode
/// policy itself.
///
/// **Lock-order rule this function must never violate**: this function holds
/// a per-CLL queue lock (`CllEventQueue`'s own `Mutex`, i.e.
/// `LogicalLinkState::rx_buf`) for its whole duration and must NEVER acquire
/// `J2534Service::subscriptions` while holding it -- that would reverse
/// `rpc_subscribe_event`'s own established `subscriptions` -> queue nesting
/// (see `J2534Service::logical_links`'s doc comment, `service.rs`, for the
/// full lock-order table) and deadlock against it. This function does not
/// need `subscriptions` at all: `queue.live_sender` is everything it reads.
async fn deliver_or_enqueue(
    rx_buf: &Arc<Mutex<CllEventQueue>>,
    cll_handle: u32,
    item: CllQueueItem,
) {
    let mut queue = rx_buf.lock().await;
    push_and_notify_lost(&mut queue, cll_handle, item);
    drain_queue_live(&mut queue, cll_handle);
}

/// Reserves `frame`'s TRUE arrival position in `rx_buf`'s queue (Codex
/// round-5 finding, PR #17) as a `CllQueueItem::PendingTimingFrame`, without
/// finalizing its `ecu_timing_change` flag or delivering it yet -- the
/// caller (`poll_rx_inner`) doesn't know the correct flag value until this
/// pass's hardware push and store step complete, later in the same call.
/// Cap/mode enforcement and `Lost` notification happen NOW, at arrival time,
/// identically to `deliver_or_enqueue` -- only finalization (flag + live/poll
/// delivery) is deferred. Returns the `reservation_id` to pass to
/// `finalize_pending_timing_frame` later, or `None` if `push_cll_event`
/// discarded it outright (`DiscardNewest` at capacity) -- nothing to
/// finalize in that case, the frame never entered the queue at all.
async fn reserve_pending_timing_frame(
    rx_buf: &Arc<Mutex<CllEventQueue>>,
    cll_handle: u32,
    frame: ReceivedFrame,
) -> Option<u64> {
    let mut queue = rx_buf.lock().await;
    let reservation_id = queue.next_reservation_id;
    queue.next_reservation_id += 1;
    let outcome = push_and_notify_lost(
        &mut queue,
        cll_handle,
        CllQueueItem::PendingTimingFrame {
            reservation_id,
            frame,
        },
    );
    // Drains anything ahead of/unrelated to this reservation; stops here (or
    // at an earlier still-pending reservation) via `drain_queue_live`'s own
    // barrier.
    drain_queue_live(&mut queue, cll_handle);
    (outcome != PushOutcome::Discarded).then_some(reservation_id)
}

/// Finalizes the `CllQueueItem::PendingTimingFrame` matching `reservation_id`
/// in `rx_buf`'s queue (Codex round-5 finding, PR #17): corrects its
/// `ecu_timing_change` flag to `flag`, downgrades it to a plain
/// `CllQueueItem::Frame`, then attempts to drain the queue live (unblocking
/// this item and, if it was at the front, whatever now-eligible items
/// follow it -- `drain_queue_live`'s own barrier). A no-op if the
/// reservation is no longer present (e.g. evicted under capacity pressure
/// before finalization -- its `Lost` notification, if any, was already sent
/// by `push_and_notify_lost` at reservation time; there is nothing left to
/// correct or deliver).
async fn finalize_pending_timing_frame(
    rx_buf: &Arc<Mutex<CllEventQueue>>,
    cll_handle: u32,
    reservation_id: u64,
    flag: bool,
) {
    let mut queue = rx_buf.lock().await;
    if let Some(pos) = queue.items.iter().position(|item| {
        matches!(item, CllQueueItem::PendingTimingFrame { reservation_id: id, .. } if *id == reservation_id)
    }) && let Some(CllQueueItem::PendingTimingFrame { mut frame, .. }) = queue.items.remove(pos)
    {
        frame.ecu_timing_change = flag;
        queue.items.insert(pos, CllQueueItem::Frame(frame));
    }
    drain_queue_live(&mut queue, cll_handle);
}

/// Bundles the `RxFlag` byte-1 bits `rx_flag_bytes` sets outside the
/// ADR-098 5-low-bits (`rx_status_flags`, byte 3) mechanism: ADR-146's
/// `ecu_timing_change` (byte 1 bit 1, service-synthesized) and ADR-191's
/// `sw_can_hv_rx` (byte 1 bit 0, forwarded from SAE J2534-2 `RxStatus` bit
/// 16). Grouped into one struct, rather than `rx_flag_bytes` growing another
/// positional bool parameter, because two independent bools was already the
/// practical limit for positional bool parameters before call-site ambiguity
/// sets in (ADR-191) -- a future `RxFlag` byte-1 addition should extend this
/// struct the same way.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct RxFlagExtras {
    pub(super) ecu_timing_change: bool,
    pub(super) sw_can_hv_rx: bool,
}

/// ADR-098: encodes a `ResultData.rx_flag` buffer. ISO 22900-2's `RxFlag` is
/// a 4-byte layout bit-compatible with SAE J2534-2's `RxStatus`
/// (big-endian: byte 3 bits 0-4 = `TX_MSG_TYPE`, `START_OF_MESSAGE`,
/// `RX_BREAK`, `TX_INDICATION`, `ISO15765_PADDING_ERROR`); this service
/// forwards those 5 bits unconditionally (bit-copy, no combination
/// validation), leaving the other `RxFlag` bits ISO 22900-2 defines
/// unreported (reserved/zero — future scope, not this ADR), EXCEPT
/// `ECU_TIMING_CHANGE` (byte 1 bit 1, ADR-146) -- the first `RxFlag` bit this
/// service synthesizes itself rather than forwards verbatim from the
/// adapter. `extras.ecu_timing_change` is `true` only on the specific bound
/// `0xC3` response frame(s) that actually caused a `CP_ModifyTiming` ComParam
/// modification (`events::observe_timing_response`); distinct from
/// `rx_status_flags`'s byte 3 range, so the two never collide. `extras.
/// sw_can_hv_rx` (byte 1 bit 0, ADR-191) forwards SAE J2534-2 `RxStatus` bit
/// 16 (`SW_CAN_HV_RX`) verbatim for a genuine SW-CAN high-voltage reception --
/// a direct, standardized 1:1 mapping (ISO 22900-2:2022 Table D.5), unlike
/// `ecu_timing_change`'s synthesis. `Vec::new()` (no flags asserted) when
/// neither `rx_status_flags` nor either `extras` bit is set, matching
/// ADR-061's original "empty means nothing asserted" convention.
pub(super) fn rx_flag_bytes(rx_status_flags: u8, extras: RxFlagExtras) -> Vec<u8> {
    let byte1 = (if extras.ecu_timing_change { 0x02 } else { 0x00 })
        | (if extras.sw_can_hv_rx { 0x01 } else { 0x00 });
    if rx_status_flags != 0 || extras.ecu_timing_change || extras.sw_can_hv_rx {
        vec![0x00, byte1, 0x00, rx_status_flags]
    } else {
        Vec::new()
    }
}

/// Outcome of one [`poll_rx_inner`] pass (ADR-095 amendment, 2026-07-18 final
/// revision): distinguishes a hard channel error from the two possible
/// successful outcomes, which additionally report whether that one bounded
/// `PassThruReadMsgs` call proved the adapter's RX queue was empty at return.
///
/// `Drained` and `MaybeMore` both mean "no hard error, caller may continue
/// normally" -- most call sites only care about that (`is_ok`) and are
/// otherwise unaffected by which of the two it was. A handful of call sites
/// (the detached-registrant maintenance injection points, see
/// `run_detached_registrant_maintenance`) additionally need `is_drained` to
/// gate `reap_expired_cyclic_registrants` correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollOutcome {
    /// The batch had fewer than `MAX_POLL_MESSAGES` entries, or the
    /// `PassThruReadMsgs` call hit the buffer-empty error arm -- either way,
    /// per SAE J2534-1's own `Timeout = 0` semantics, the adapter's RX queue
    /// was provably empty at the instant this call returned.
    Drained,
    /// The batch had exactly `MAX_POLL_MESSAGES` entries -- this call proves
    /// nothing about exhaustiveness; more frames could still be queued.
    MaybeMore,
    /// A hard channel error was detected; `handle_channel_hard_error` has
    /// already run and the caller must stop. Mirrors the old `bool = false`
    /// return exactly.
    HardError,
}

impl PollOutcome {
    /// Mirrors the pre-this-change `bool` return (`true` = ok, `false` =
    /// hard error): `false` only for `HardError`, `true` for either
    /// successful outcome regardless of exhaustiveness. Every pre-existing
    /// `if !poll_rx_inner(...).await { ... }`-style caller becomes
    /// `if !poll_rx_inner(...).await.is_ok() { ... }` with identical
    /// behavior.
    fn is_ok(self) -> bool {
        !matches!(self, PollOutcome::HardError)
    }

    /// `true` only for `Drained` -- the exhaustive-drain signal
    /// `poll_rx_inner`'s own end-of-pass block consumes to decide whether to
    /// stamp `ctx.drain_watermarks[ctx.channel_id]` (ADR-101 Decision §E).
    fn is_drained(self) -> bool {
        matches!(self, PollOutcome::Drained)
    }
}

/// Classifies one `PassThruReadMsgs` batch's own exhaustiveness, pure and
/// directly unit-testable (`poll_rx_inner` itself is not -- it always
/// resolves through `ctx.api.lock().await`/`read_messages`, which requires a
/// live `J2534Api0404` bound to a loaded shared library, only available via
/// the `grpc_mock` integration harness's mock `.so`). `batch_len ==
/// MAX_POLL_MESSAGES` is the only `MaybeMore` case -- SAE J2534-1's own
/// `Timeout = 0` `PassThruReadMsgs` semantics guarantee a short batch means
/// nothing more was queued at that instant.
fn classify_poll_batch(batch_len: usize) -> PollOutcome {
    if batch_len < MAX_POLL_MESSAGES {
        PollOutcome::Drained
    } else {
        PollOutcome::MaybeMore
    }
}

/// One `poll_rx_inner` pass's still-unfinalized ADR-146
/// `CllQueueItem::PendingTimingFrame` reservation (Codex round-5 findings,
/// PR #17): everything `finalize_pending_timing_frame` needs, once
/// `derived_stored`/`derived_winner`/`superseded_timing` are known after
/// this pass's fold/push/store step. A plain tuple here trips clippy's
/// `type_complexity` lint (mirrors [`StillDueSnapshot`]'s own doc comment
/// note on the same lint).
struct PendingTimingReservation {
    rx_buf: Arc<Mutex<CllEventQueue>>,
    cll_handle: u32,
    reservation_id: u64,
    cop_handle: u32,
    frame_seq: usize,
}

/// Single shared RX pass: calls `PassThruReadMsgs` once, runs every frame
/// through per-CLL routing/transport processing (`process_frame_for_entry`),
/// attributes each delivery via `bind_frame` (ADR-100 Decision §3, against
/// each CLL's own registrants snapshotted alongside the rest of
/// `CllRxEntry`), delivers the results to `rx_buf`s and event subscribers,
/// and optionally feeds an [`FcCapture`].
///
/// Returns [`PollOutcome::HardError`] when a hard channel error was detected
/// (`handle_channel_hard_error` has already run; the caller must stop).
/// Otherwise returns [`PollOutcome::Drained`] or [`PollOutcome::MaybeMore`]
/// per `classify_poll_batch`'s own doc comment -- see [`PollOutcome`] for the
/// full contract.
///
/// ADR-101 Decision §E: whenever this pass's own outcome is `Drained`
/// (proven-exhaustive), `ctx.drain_watermarks[ctx.channel_id]` is stamped
/// with `read_started_at` -- the instant captured immediately before the
/// `PassThruReadMsgs` call, not the instant this function returns.
/// Timestamping from read-START is conservative for a frame that arrives
/// mid-read. The stamp is written only at the very end of this function, once
/// this pass's own bindings/merges have already landed in live registrant
/// state (or, for the two early-return `Drained` arms below, immediately --
/// there is no frame batch and therefore no writeback to wait on) so a reader
/// can never observe "channel X drained as of time T" before the state that
/// stamp certifies has actually been merged.
async fn poll_rx_inner(
    ctx: &ChannelPollCtx,
    mut fc_capture: Option<&mut FcCapture>,
) -> PollOutcome {
    let read_started_at = tokio::time::Instant::now();
    // ADR-123 Finding I (Codex review round 5): the hard-error arm below must
    // run with `api`'s guard already dropped -- `handle_channel_hard_error`
    // now acquires `shared_channels` (to clear a dead CLL's held TX-queue
    // lock and wake resumed siblings), and ADR-080 requires `shared_channels`
    // to be the OUTERMOST lock whenever held alongside `api`/`logical_links`.
    // Holding `api` across that call would be the reverse of that order --
    // a deadlock risk against `rpc_lock_resource`'s own
    // `shared_channels` -> `api` -> `logical_links` acquisition. `messages`
    // is therefore resolved to an `Option` inside the `api`-held block, with
    // the hard-error handling itself moved out below the block.
    //
    // ADR-147 sixth amendment (pre-read capture for the batch anchor):
    // snapshot `LogicalLinkState::error_set_seq` for every CLL on this
    // channel (same membership filter `build_cll_rx_entries` itself uses)
    // HERE, under one `logical_links` lock acquisition, BEFORE `ctx.api` is
    // even acquired for the read below -- and drop the guard immediately
    // after, so the two locks are never nested (ADR-080; `logical_links` is
    // fully released before `api` is acquired, and vice versa, at every site
    // in this function).
    //
    // A timeout bumping `error_set_seq` between this pre-read capture and
    // the read's actual completion causes a genuinely POST-error positive
    // response in that same batch to be discarded (its captured seq is
    // pre-bump) -- this is over-discard, fail-CLOSED, and self-correcting on
    // the CLL's next batch's positive response. This is the accepted
    // tradeoff versus the alternative: exact "at read-completion" equality
    // is structurally unachievable across two different async locks
    // (`ctx.api` for the read, `ctx.logical_links` for the counter) -- no
    // single instant can be inside both critical sections at once. The
    // inequality `T_capture <= T_read-start <= T_read-completion` is what's
    // actually achievable and sufficient: it never causes fail-OPEN (a stale
    // Positive incorrectly clearing a genuine timeout-triggered suspension),
    // only an occasional, self-correcting fail-CLOSED (a genuine recovery
    // positive discarded, requiring one more positive response to actually
    // resume) -- the safe direction for a mechanism whose entire purpose is
    // not resuming on unconfirmed evidence. Moving the capture to
    // immediately after `read_messages` returns does NOT fix this: that
    // capture itself still requires `logical_links.lock().await`, its own
    // yield point after read-return, so any post-read capture point still
    // has a gap a concurrent bump can land in. See ADR-147 for the full
    // argument, including why a lock-free atomic `error_set_seq` was
    // rejected as an alternative (it would let a caller observe the counter
    // bump and the `tx_suspended_by_error` write as separately-ordered
    // events, which are currently paired inside one `logical_links` critical
    // section -- trading a checkable lock-discipline argument for a harder
    // to verify memory-ordering one).
    //
    // A CLL absent from this snapshot (created/connected strictly AFTER this
    // capture but before `build_cll_rx_entries` runs, a narrow window) gets
    // no entry in the map; `build_cll_rx_entries` stamps `set_seq_at_read =
    // None` for it, and `queue_error_class_to_apply` treats `None` as
    // "always discard `Positive`" -- conservative, and self-correcting on
    // that CLL's next pass once it has a valid snapshot.
    let set_seq_snapshot: HashMap<u32, u64> = {
        let links = ctx.logical_links.lock().await;
        links
            .iter()
            .filter(|(_, l)| {
                l.channel_id == Some(ctx.channel_id) || l.uudt_channel_id == Some(ctx.channel_id)
            })
            .map(|(&h, l)| (h, l.error_set_seq))
            .collect()
    };
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): skip issuing
    // the native `PassThruReadMsgs` call entirely for this protocol -- not
    // just discarding the resulting error afterward -- so this task's
    // hard-read-error-closes-the-channel rule below never misinterprets
    // clause 24.2.5.2's expected, permanent `ERR_NOT_SUPPORTED` as an
    // external channel loss. Treated the same as an ordinary "nothing
    // queued" empty batch: `Drained`, watermark stamped, for every ADR-101
    // Decision §E caller this outcome feeds.
    if !ctx.rx_supported {
        ctx.drain_watermarks
            .lock()
            .await
            .insert(ctx.channel_id, read_started_at);
        return PollOutcome::Drained;
    }

    let messages = {
        let api = ctx.api.lock().await;
        match api.read_messages(ctx.channel_id, MAX_POLL_MESSAGES, 0) {
            Ok(msgs) => Some(msgs),
            Err(j2534_0404::Error::ApiStatus { code, .. }) if code.is_buffer_empty() => {
                ctx.drain_watermarks
                    .lock()
                    .await
                    .insert(ctx.channel_id, read_started_at);
                return PollOutcome::Drained;
            }
            Err(_) => None,
        }
    };
    let Some(messages) = messages else {
        handle_channel_hard_error(ctx.channel_id, ctx).await;
        return PollOutcome::HardError;
    };

    if messages.is_empty() {
        ctx.drain_watermarks
            .lock()
            .await
            .insert(ctx.channel_id, read_started_at);
        return PollOutcome::Drained;
    }

    // Any received frame counts as bus activity for CP_TesterPresentSendType
    // = 1's idle timer (ADR-083) -- unfiltered by CLL/addressing, unlike the
    // per-CLL routing below. Codex-review fix (PR #82, ninth round): a
    // CONFIG_LOOPBACK TX echo of this device's own transmit (RX_TX_MSG_TYPE
    // set) is not genuine external bus activity -- stamping on it would
    // bypass transmit_request's count_as_bus_activity=false gate for an
    // idle-mode CLL's own send via this separate RX path, reintroducing the
    // same-interval-sibling starvation that gate exists to prevent. Only
    // stamp when at least one frame in this batch is a real external frame.
    let has_external_activity = messages
        .iter()
        .any(|msg| msg.rx_status() & RX_TX_MSG_TYPE == 0);
    if has_external_activity {
        *ctx.last_bus_activity.lock().await = tokio::time::Instant::now();
    }

    // ADR-160/Phase 3c (widened to the family by ADR-217): resolved once per
    // poll cycle (`ctx.service`, this task's own cloned `J2534Service`
    // handle -- `build_cll_rx_entries` itself is a plain associated function
    // with no `self` access) rather than once per frame; `build_cll_rx_
    // entries` narrows it further, per `Hardware`-kind entry, to entries
    // whose own channel is actually ISO15765-family and not FD-substituted.
    // Uses `is_native_mixed_family()` rather than a literal `==
    // NativeMixed`: this only gates whether the per-frame `ProtocolID`
    // branch (`process_frame_for_entry`'s `native_mixed` arm) exists on a
    // channel at all, which is identical under `NativeMixed` and
    // `NativeMixedAllFrames` (ADR-217 Decision item 3) -- unlike the
    // ADR-162 collision checks in `rpc_link.rs`, this is not one of the
    // `ON`-only sites.
    // ADR-222 round 6: the full `CanChannelMode`, not just a pre-collapsed
    // native-mixed bool -- `build_cll_rx_entries` now also needs to tell a
    // `DualChannel` link apart (for the point-to-point filter eligibility
    // gate on its contention-map contribution), and deriving both facts
    // from one shared read here keeps a single source of truth for "what
    // mode is this poll pass running under," rather than two independently
    // read/derived values that could disagree.
    let can_channel_mode = ctx.service.effective_can_channel_mode().await;

    let mut cll_entries = build_cll_rx_entries(
        ctx.channel_id,
        &ctx.logical_links,
        &ctx.primitives,
        &set_seq_snapshot,
        can_channel_mode,
    )
    .await;

    // ADR-146 (amended -- Codex round-5 findings, PR #17: deferred-
    // finalization delivery, superseding rounds 2-4's `buffered_frames`
    // side-buffer): delivering a `ReceivedFrame` with `ecu_timing_change =
    // true` before this pass's own fold/push/store step (below, after both
    // per-frame loops) has run would let a subscriber observe the flag
    // before the derived ComParam values it asserts have actually landed --
    // and, on a failed hardware push, the flag would assert a store that
    // never happens at all. Rather than holding qualifying frames OUTSIDE
    // the real queue until the pass resolves (rounds 2-4's `buffered_frames`
    // Vec, which violated the queue's own true-arrival-order FIFO contract
    // against concurrent non-timing events -- Codex round-5 Finding 1),
    // every qualifying frame now reserves its TRUE arrival position IN the
    // queue itself, immediately, as `CllQueueItem::PendingTimingFrame`
    // (`reserve_pending_timing_frame`); only finalization (the flag's final
    // value, and live/poll delivery) is deferred, to
    // `finalize_pending_timing_frame` once `derived_stored`/`derived_winner`
    // are known (below). Every OTHER frame decides its own delivery
    // independently, based on its own `ecu_timing_change` -- there is no
    // longer any "has this pass seen a qualifying frame yet" state, since
    // the real queue's own head-of-line barrier (`drain_queue_live`) now
    // preserves relative order for everything queued behind a still-pending
    // reservation.
    let mut pending_reservations: Vec<PendingTimingReservation> = Vec::new();
    let mut superseded_timing: HashSet<(u32, u32, usize)> = HashSet::new();

    for (frame_seq, msg) in messages.iter().enumerate() {
        let timestamp = msg.timestamp();
        let data = msg.data().map(|d| d.to_vec()).unwrap_or_default();
        // ADR-160/Phase 3c: this frame's own native ProtocolID, read once
        // per message alongside `frame_can_id` below -- only consulted by
        // `process_frame_for_entry`'s `Hardware` arm, and only for an entry
        // whose own `native_mixed` is `true` (see that function's own doc
        // comment for why this is a no-op everywhere else).
        let frame_protocol_id = msg.protocol_id();

        // ADR-172: a SW-CAN-family frame (checked against its own native
        // protocol id, not this CLL's -- ADR-160's native_mixed model) whose
        // raw, unmasked RxStatus carries a speed-transition confirmation bit
        // (SAE J2534-2 clause 9.4.1.1 Table 12, bit 17 `SW_CAN_HS_RX` or bit
        // 18 `SW_CAN_NS_RX`) is withheld entirely, before this frame is ever
        // parsed as content. These bits don't fit `RX_STATUS_FLAGS_MASK`'s
        // `u8`-truncated scope (bits above 7) and bit 17 means something
        // else entirely on Fault-Tolerant CAN (`LINK_FAULT`), so this check
        // must run against the raw `u32` value, scoped by
        // `resources::is_sw_family_protocol_id`, ahead of and independently
        // of `is_content_frame`'s low-bit gate below -- mirroring this same
        // loop's FlowControl-capture check further down, which also
        // consults the raw `RxStatus` value ahead of the 5-bit-scoped gate.
        // ADR-212/Round 2: re-keyed from `is_sw_protocol_id` to the
        // family-wide `is_sw_family_protocol_id` so this withholding still
        // fires for a `_CHx`-connected SW-CAN link's frames, whose native
        // protocol id is the connected `_CH<N>` id, not the bare `_PS` id.
        if resources::is_sw_family_protocol_id(frame_protocol_id)
            && msg.rx_status() & (RX_SW_CAN_HS_RX | RX_SW_CAN_NS_RX) != 0
        {
            continue;
        }

        // ADR-191: unlike the HS_RX/NS_RX withhold just above, bit 16
        // (`RX_SW_CAN_HV_RX`) stays content-eligible exactly as ADR-172
        // Decision 3 already established -- this does NOT `continue`. It
        // only records whether this frame's eventual `RxFlag` delivery
        // (`rx_flag_bytes`/`RxFlagExtras`) should also carry the ISO
        // 22900-2 Table D.5 byte 1 bit 0 tag. Must stay in scope through the
        // rest of this loop iteration, the same way `rx_status_flags` (below)
        // already does, since it needs to reach the `ReceivedFrame`
        // constructed in the per-CLL delivery loop further down.
        // ADR-212 Decision item 4's own class of fix, mirroring the
        // HS_RX/NS_RX withhold gate just above -- without this, a
        // `_CHx`-connected SW-CAN link's `frame_protocol_id` is the
        // connected `_CH<N>` id, not the bare `_PS` id, so the narrow
        // `is_sw_protocol_id` would silently never set this tag.
        let sw_can_hv_rx = resources::is_sw_family_protocol_id(frame_protocol_id)
            && msg.rx_status() & RX_SW_CAN_HV_RX != 0;

        // ADR-179/Phase 5: a SAE J1939 address-claim/defend indication
        // (clause 16.4.6 Table 63) -- checked the same raw-`u32`,
        // protocol-gated way as the SWCAN arm just above (same bit
        // positions 16/17, disambiguated by `resources::is_j1939_protocol_id`
        // instead of `is_sw_protocol_id` -- ADR-179's Context section).
        // Unlike SWCAN's silent withhold, this routes the outcome into
        // Decision 3's claim/defend state machine (`deliver_j1939_claim_
        // indication`) BEFORE withholding: `Data[0]` (`DataSize` = 1)
        // carries the affected address, which this physical channel's
        // `SharedChannel::j1939_claims` map resolves to the owning CLL
        // (dropping a stale indication whose recorded `connect_generation`
        // no longer matches -- ADR-086). Never forwarded into a D-PDU
        // `RxFlag` bit (ADR-179 Decision 7 -- no such bit exists) and never
        // reaches ordinary content/COP matching below -- always withheld
        // via `continue` regardless of routing outcome.
        if resources::is_j1939_protocol_id(frame_protocol_id) {
            let claim_bits = msg.rx_status() & (RX_J1939_ADDRESS_CLAIMED | RX_J1939_ADDRESS_LOST);
            if claim_bits != 0 {
                if let Some(&address) = data.first() {
                    let claimed = claim_bits & RX_J1939_ADDRESS_CLAIMED != 0;
                    deliver_j1939_claim_indication(ctx, address, claimed).await;
                }
                continue;
            }
        }

        // ADR-188/Phase 7 Stage 7a: a TP2.0 CONNECTION_ESTABLISHED/_LOST
        // indication (clause 19.4.4 Table 81) -- checked the same raw-`u32`,
        // protocol-gated way as the J1939 arm just above (same bit
        // positions 16/17, disambiguated by
        // `resources::is_tp2_0_family_protocol_id`).
        // `Data[0..3]` (MSB first) carries the affected RX-ID, which this
        // physical channel's `SharedChannel::tp20_connections` map resolves
        // to the owning CLL (dropping a stale indication whose recorded
        // `connect_generation` no longer matches). Never forwarded into a
        // D-PDU `RxFlag` bit (ADR-188 §7, folded into the existing RxFlag-
        // widening P2 backlog item) and never reaches ordinary content/COP
        // matching below -- always withheld via `continue` regardless of
        // routing outcome.
        // ADR-210 Decision item 12: re-keyed from the narrow
        // `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id` --
        // `frame_protocol_id` is the RAW protocol id an incoming native frame
        // reports, so a `_CHx`-connected TP2.0 link's own connection
        // indications would otherwise never be recognized as TP2.0 frames at
        // all.
        if resources::is_tp2_0_family_protocol_id(frame_protocol_id) {
            let conn_bits =
                msg.rx_status() & (RX_TP20_CONNECTION_ESTABLISHED | RX_TP20_CONNECTION_LOST);
            if conn_bits != 0 {
                if data.len() >= 4 {
                    let rx_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                    let established = conn_bits & RX_TP20_CONNECTION_ESTABLISHED != 0;
                    let outcome = if established && data.len() >= 8 {
                        // Masked to SAE J2534-1's 29-bit CAN identifier space
                        // (`0x1FFFFFFF`) at this parse site (ADR-192/Phase 7
                        // Stage 7c edge-case-hunter fix): `events_rx_routing.rs`'s
                        // `MatchKind::matched` broadcast-echo classification
                        // relies on the invariant that no real established
                        // TX-ID's top byte can reach `0xF0`-`0xFF` (that range
                        // is reserved for `[address] ++ payload` broadcast
                        // echoes), citing this same 29-bit ceiling in its own
                        // comment -- enforced here at the source, by
                        // construction, rather than merely assumed at the
                        // consuming site. A malformed/non-zeroed-upper-bits
                        // indication would otherwise cause every one of this
                        // connection's own legitimate TX-side echoes to be
                        // misclassified as a broadcast echo and silently
                        // dropped for the life of the connection.
                        Tp20ConnectionOutcome::Established(
                            u32::from_be_bytes([data[4], data[5], data[6], data[7]]) & 0x1FFF_FFFF,
                        )
                    } else {
                        Tp20ConnectionOutcome::Lost(data.get(4).copied().unwrap_or(1))
                    };
                    deliver_tp20_connection_indication(ctx, rx_id, outcome).await;
                }
                continue;
            }
        }

        // ADR-171: this frame's own native ExtraDataIndex, read once per
        // message alongside `frame_protocol_id` above -- only consulted by
        // `header_footer_len`'s J1850 arm to derive the real IFR footer
        // length instead of assuming a fixed 1-byte CRC.
        let frame_extra_data_index = msg.extra_data_index() as usize;
        // ADR-098 (corrected): the 5 low RxStatus bits this service forwards
        // into ResultData.rx_flag (byte 3 bits 0-4), bit-copied
        // unconditionally. A value is eligible to be attributed to a pending
        // COP wait unless one of the 4 indication-type bits is set -- see
        // `bind_frame`'s `is_content_frame` gating below.
        let rx_status_flags = (msg.rx_status() & RX_STATUS_FLAGS_MASK) as u8;

        // ADR-197: this frame's own native RxStatus bit 7
        // (`ISO15765_ADDR_TYPE`, SAE J2534-1 §8.7.1 Figure 43), read once per
        // message alongside `rx_status_flags` above -- it sits outside
        // `RX_STATUS_FLAGS_MASK`'s 5 low bits, so it is read from
        // `msg.rx_status()` directly. Every `deliveries` entry fanned out
        // from this one native message shares this same message's RxStatus,
        // so one bool per message (not per delivery) is correct. Consulted
        // only by `header_footer_len`'s ISO15765 arm, OR'd with the
        // pre-existing `usdt_addressing_by_id`/`uudt_addressing_by_id` table
        // lookup (ADR-217 Codex-review fix, PR #132).
        let rx_ext_addr = msg.rx_status() & j2534_0404::ISO15765_ADDR_TYPE_STATUS != 0;

        // ADR-222: this frame's own native RxStatus bit 8
        // (`CAN_29BIT_ID`, SAE J2534-1 §8.7.1 Figure 43), read once per
        // message the same way as `rx_status_flags`/`rx_ext_addr` above.
        // Threaded through `FrameRouteContext` to every numeric-CAN-ID
        // comparison site (`UniqueRespIdKey::matched`, `route_frame_uudt_only`,
        // the software-ISO-TP arm's inline USDT/FC-pair lookups,
        // `header_footer_len`'s addressing-table lookup) so a numeric id
        // configured at two different widths on this physical channel
        // resolves to the entry whose configured width actually matches this
        // frame, instead of the first/any numeric match -- a provable no-op
        // on every channel that never configures such a contended id (every
        // gate on such a channel is `CanIdWidthGate::Any`, which accepts
        // either width unconditionally).
        let frame_is_29bit = msg.rx_status() & j2534_0404::CAN_29BIT_ID_STATUS != 0;

        // A frame is "content" (a real message body eligible for
        // expected-response/pending-RC matching and, below, tester-present
        // content-discard) unless one of the 4 indication-type RxStatus bits
        // (TX_MSG_TYPE, START_OF_MESSAGE, RX_BREAK, TX_INDICATION) is set.
        // RX_ISO15765_PADDING_ERROR is excluded from this exclusion: it tags a
        // genuine, fully reassembled response whose final CAN frame merely
        // had fewer than 8 data bytes, not an indication (ADR-098 Correction).
        let is_content_frame = rx_status_flags & !(RX_ISO15765_PADDING_ERROR as u8) == 0;

        // Extract the 4-byte CAN ID from the leading bytes of the frame data for
        // UniqueRespIdTable matching (CAN and ISO15765 frames embed CAN ID at [0..4]).
        let frame_can_id: Option<u32> = if data.len() >= 4 {
            Some(u32::from_be_bytes([data[0], data[1], data[2], data[3]]))
        } else {
            None
        };

        // ADR-203: this frame's own KWP/J1850 source-address byte, read once
        // per message (not per CLL entry) directly off the raw wire bytes --
        // keyed off the frame's own native protocol (`frame_protocol_id`),
        // so this is always `None` on CAN/J1939/TP2.0/ISO15765 channels by
        // construction. Consulted only by `route_frame`'s new
        // `CP_EcuRespSourceAddress` tier.
        let frame_source_addr =
            kline_j1850_source_addr(resources::base_protocol_id(frame_protocol_id), &data);

        // Software ISO-TP TX driver: capture the FlowControl frame(s) it is
        // waiting for. Every matching frame in this batch is captured (not just
        // the first — Codex review finding, PR #130: a burst of FS_WAIT frames
        // delivered in one PassThruReadMsgs batch must all count against the
        // internal FS_WAIT flood guard, ADR-124); each is withheld from fan-out —
        // it is transport signalling for the in-flight request, not payload data.
        if let Some(cap) = fc_capture.as_deref_mut()
            && frame_can_id.is_some()
            && (cap.fc_can_id.is_none() || cap.fc_can_id == frame_can_id)
            // ADR-098: a CONFIG_LOOPBACK echo of our OWN transmitted flow-
            // control frame must never be mistaken for an incoming FC frame.
            && msg.rx_status() & RX_TX_MSG_TYPE == 0
            && let Some(isotp::Frame::FlowControl {
                flow_status,
                block_size,
                st_min,
            }) = isotp::parse_frame(&data[4..], cap.addressing)
        {
            cap.got.push_back((flow_status, block_size, st_min));
            continue;
        }

        // ADR-099: identifies a frame that is either the echo of THIS
        // device's own transmit (CONFIG_LOOPBACK/TX_MSG_TYPE) or a
        // TX_DONE/TX_INDICATION completion notice -- used both by
        // `bind_frame`'s tester-present arm (c) below and, via
        // `rx_status_flags`, by the indication-frame exclusion those same
        // bits feed into `is_content_frame` above.
        let is_tx_side = rx_status_flags & (RX_TX_MSG_TYPE | RX_TX_INDICATION) as u8 != 0;

        // SAE J2534-2 clause 19.3.2.2 (ADR-192/Phase 7 Stage 7c, Codex review
        // round 2 fix): a TP2.0 broadcast TX-side echo (`[address] ++
        // payload`, `tx_header::build_tx_message`'s own broadcast
        // composition) must be classified and dropped directly from the raw
        // frame's first data byte, before it ever reaches per-CLL routing --
        // NOT by reinterpreting the leading bytes as a 4-byte `can_id` the
        // way `events_rx_routing.rs::UniqueRespIdKey::matched`'s own
        // broadcast-echo arm used to. That per-entry classifier only ran
        // when `route_frame` had a `can_id` to hand it at all, which
        // required `frame_can_id` (above) to be `Some`, which in turn
        // required `data.len() >= 4` -- but the broadcast message's own
        // legal composed-size range is `3..=8` (`rpc_primitive.rs`'s
        // `size_range` for `tp20_is_broadcast`). A 3-byte broadcast echo
        // (the minimum-size case) therefore always produced
        // `frame_can_id == None`, which `route_frame`'s own
        // `None => Some(0)` fallback delivers unconditionally to every CLL
        // in `entry.unique_resp_ids` -- bypassing the per-entry classifier
        // entirely and leaking the echo to every sibling CLL sharing this
        // physical channel. Checking `data[0]` directly instead covers the
        // full `3..=8`-byte range uniformly, with no length dependency at
        // all. Mirrors the CONNECTION_ESTABLISHED/_LOST indication arm
        // above: an unconditional `continue` before the frame ever reaches
        // `cll_entries`/`process_frame_for_entry`/`route_frame`.
        //
        // Safety: a genuine established-connection TX-side echo can never
        // be misclassified here. `data[0]` of a connection-bound send is
        // `tp20_established_tx_id.to_be_bytes()[0]` -- the TX-ID's own top
        // byte -- and the TX-ID is masked to SAE J2534-1's 29-bit CAN
        // identifier space (`0x1FFF_FFFF`) at its parse site (this file's
        // own TP2.0 CONNECTION_ESTABLISHED indication-parsing arm above),
        // so its top byte can never reach `0xF0`-`0xFF` (29-bit ceiling
        // caps the top byte at `0x1F`).
        //
        // Once this early, universal drop exists, no broadcast echo of any
        // length can ever reach `route_frame`/`UniqueRespIdKey::matched`
        // again, so that per-entry classifier's own broadcast-echo arm is
        // now unreachable and has been removed there.
        // ADR-210 Decision item 12: re-keyed from the narrow
        // `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id` --
        // `frame_protocol_id` is the RAW protocol id reported alongside the
        // frame, so a `_CHx`-connected TP2.0 link's own broadcast echo would
        // otherwise skip this early drop.
        if resources::is_tp2_0_family_protocol_id(frame_protocol_id)
            && is_tx_side
            && data.first().is_some_and(|&b| (0xF0..=0xFF).contains(&b))
        {
            continue;
        }

        for entry in &mut cll_entries {
            let deliveries = process_frame_for_entry(
                &ctx.api,
                ctx.channel_id,
                entry,
                FrameRouteContext {
                    frame_can_id,
                    frame_source_addr,
                    frame_protocol_id,
                    is_content_frame,
                    frame_is_29bit,
                },
                &data,
                is_tx_side,
            )
            .await;

            for (delivery_data, unique_resp_identifier, raw, uudt_routed) in deliveries {
                // ADR-051: for CAN, ISO15765, ISO9141, ISO14230, and J1850,
                // ResultData.data_bytes is payload-only — the leading/trailing
                // bytes `header_footer_len` identifies are split off into
                // `header_bytes`/`footer_bytes` and surfaced via `extra_info`
                // instead. Expected-response mask/pattern matching and
                // RC-byte-offset detection therefore also run against the
                // payload, not the header/footer. SCI (and any other
                // unaffected protocol) is a no-op here: `header`/`footer` stay
                // empty and `payload` is the frame exactly as
                // `process_frame_for_entry` produced it.
                //
                // ADR-196 Decision item 3: a RawMode=ON CLL (`entry.raw_mode`)
                // hard-codes the actual header/footer SPLIT to `(0, 0)`
                // instead -- mirroring the exact "unaffected protocol" shape
                // this comment already describes for SCI, since
                // `events_event_senders.rs`'s `extra_info` construction
                // already treats empty `header_bytes`/`footer_bytes` as
                // `None`, no separate downstream change needed. The whole
                // frame (CAN ID prefix included) lands in `payload` below,
                // unchanged from what `process_frame_for_entry` produced.
                //
                // ADR-196 Decision item 3b: `header_footer_len` is still
                // ALWAYS called, even for a RawMode CLL, purely to compute
                // `raw_prefix` -- the per-frame raw CAN-ID prefix width
                // several internal UDS negative-response detectors
                // (`RcHandlingConfig::detect_pending_rc`/`is_unhandled_negative`,
                // `classify_queue_error`, `observe_session_timing_response`)
                // must re-base their own `0x7F`/SID-echo anchors on, reusing
                // this already-audited computation rather than re-deriving a
                // second, possibly diverging notion of "how wide is this
                // frame's header". `raw_prefix` is `0` for RawMode=OFF
                // UNCONDITIONALLY -- deliberately NOT just `header_len`,
                // which can be nonzero for RawMode=OFF too (that split is the
                // entire point of the `header_len`/`footer_len` pair below);
                // only a RawMode=ON CLL ever gets a nonzero `raw_prefix`.
                //
                // ADR-200 (Phase 3): a RawMode=ON SAE J1939 CLL's
                // `delivery_data` first has its native destination-address
                // (DA) byte dropped (`raw_j1939_rx_drop_destination_address`)
                // -- the D-PDU raw contract (Table 80) is a 4-byte CAN ID,
                // not the native 5-byte CAN-ID+DA shape SAE J2534-2
                // §16.4.3/Table 62 requires this service to receive over the
                // FFI boundary (mirroring `tx_header::raw_j1939_tx_message`'s
                // TX-side insertion of the same byte). This runs BEFORE
                // `header_footer_len`/`raw_prefix` below, so every downstream
                // consumer (`payload`, `ResultData.data_bytes`,
                // expected-response/RC matching) only ever sees the
                // DA-dropped, client-visible 4-byte-CAN-ID shape.
                // `header_footer_len`'s own `PROTOCOL_J1939_PS` arm is NOT
                // reused for `raw_prefix` here -- it assumes the native
                // 5-byte shape this transform has already removed, so
                // `raw_prefix` is derived directly from the DA-dropped
                // buffer's own length instead (mirroring the CAN family's
                // plain 4-byte prefix, the shape this buffer now has).
                let delivery_data =
                    if entry.raw_mode && entry.header_protocol == j2534_0404::PROTOCOL_J1939_PS {
                        raw_j1939_rx_drop_destination_address(&delivery_data)
                    } else {
                        delivery_data
                    };
                // ADR-217 Codex-review fix, PR #132: selects the USDT or UUDT
                // addressing table by this delivery's own `uudt_routed`,
                // rather than a single table blind to which role matched --
                // see `CllRxEntry::rx_addressing_table`'s own doc comment.
                let (computed_header_len, computed_footer_len) = header_footer_len(
                    entry.header_protocol,
                    entry.rx_addressing_table(uudt_routed),
                    &delivery_data,
                    raw,
                    Some(frame_extra_data_index),
                    rx_ext_addr,
                    frame_is_29bit,
                );
                let raw_prefix = if entry.raw_mode {
                    if entry.header_protocol == j2534_0404::PROTOCOL_J1939_PS {
                        delivery_data.len().min(4)
                    } else {
                        computed_header_len
                    }
                } else {
                    0
                };
                let (header_len, footer_len) = if entry.raw_mode {
                    (0, 0)
                } else {
                    (computed_header_len, computed_footer_len)
                };
                let mut remainder = delivery_data;
                let mut payload = remainder.split_off(header_len.min(remainder.len()));
                let header = remainder;
                let footer_start = payload.len().saturating_sub(footer_len);
                let footer = payload.split_off(footer_start);

                // ADR-100 Decision §3: the full attribution precedence table
                // (indication bypass / tier-1 non-vacuous / tester-present /
                // tier-1 vacuous / tier-2 / unbound), replacing the old
                // single-`MatchProbe` scan plus its separate tester-present
                // discard check -- see `bind_frame`'s own doc comment for the
                // step-by-step mapping. A pending-RC frame is never treated
                // as a final match even if it happens to satisfy a
                // registrant's expected_response pattern -- the pattern may
                // be broad enough to match negative responses, and with RC
                // handling enabled the registrant explicitly opts out of
                // treating a pending NRC as completion (ADR-022).
                // ADR-148 Amendment. Populated only
                // by the empty-payload-match arm's own force-finalize-all
                // and the byte/segment cap hit's force-finalize-one
                // (`bind_registrant`) -- every other finalization happens at
                // the receive-phase deadline instead. Delivered below,
                // BEFORE this frame's own outcome is acted on, so a finalized
                // multi-segment response reaches `rx_buf` ahead of whatever
                // (if anything) this frame itself becomes. In buffer
                // (first-opened-first) order, guarded against the
                // `entry`/live-state staleness this snapshot's `rx_buf` alone
                // doesn't rule out (ADR-148 Amendment 9,
                // `deliver_concat_batch_if_live`): a concurrent
                // `DisconnectComLogicalLink` between this pass's snapshot and
                // this delivery can fail the guard, in which case the whole
                // batch is silently dropped -- nothing to reorder, since
                // delivery just doesn't happen. ADR-147/148 merge: this
                // delivery is deliberately sequenced AFTER the
                // `wrote_suspend` eager-publish block below, not before --
                // `deliver_concat_batch_if_live` is itself a form of
                // client-visible exposure, and the Codex-review
                // publish-before-exposure invariant that block's own comment
                // documents (r3680112304) applies to ANY exposure this same
                // `bind_frame` call can trigger, not just this frame's own
                // delivery -- see the eager-publish block's comment for the
                // full reasoning.
                let mut finalized_concat: Vec<ConcatDelivery> = Vec::new();
                let mut wrote_suspend = false;
                let binding = bind_frame(
                    entry,
                    FrameContext {
                        is_content_frame,
                        is_tx_side,
                        frame_can_id,
                        payload: &payload,
                        unique_resp_identifier,
                    },
                    frame_seq,
                    ConcatFrameMeta {
                        timestamp,
                        header_bytes: &header,
                        footer_bytes: &footer,
                        rx_status_flags,
                        // Computed inside `bind_frame` itself from
                        // `entry.header_protocol` and `header_bytes` above --
                        // this call-site value is always overridden there.
                        source_id: None,
                        uudt_routed,
                        raw_prefix,
                    },
                    &mut finalized_concat,
                    &mut wrote_suspend,
                );

                // ADR-147 fifth amendment (split direction-specific
                // anchors): this frame's classification just folded
                // `Suspend` into `entry.queue_error_class` (`bind_frame` set
                // `wrote_suspend` at the exact write site, never on a frame
                // that didn't rewrite the classification to `Suspend` --
                // `Positive` never sets it at all) -- capture the live
                // `error_clear_seq` into `entry.suspend_seq` NOW, briefly
                // re-acquiring `logical_links` (not held anywhere in this
                // call chain otherwise), and BEFORE this frame -- or any
                // OTHER exposure this same `bind_frame` call can trigger,
                // including a finalized concat batch delivered a few lines
                // below (ADR-147/148 merge) -- is delivered/exposed to the
                // client below via `rx_buf`/`deliver_or_enqueue`. Both of the
                // `continue` arms in the match right below skip delivery
                // entirely, so this capture must run before that match, not
                // after it. No lock nesting between `logical_links` and
                // `rx_buf`: the guard is dropped at the end of this block,
                // before `rx_buf` is ever touched for this frame (ADR-080
                // lock-ordering discipline).
                //
                // Suspend's invalidation anchor is EXPOSURE, not fold time --
                // an explicit clear can only invalidate content the client
                // could have seen, and this capture always runs before this
                // frame's delivery, so absorbing a clear that bumped
                // `error_clear_seq` in the gap between fold (inside
                // `bind_frame`, just above) and this capture is the CORRECT
                // outcome (the resume necessarily predates this frame's
                // exposure) -- this is not a race to fix. See
                // `LogicalLinkState::error_clear_seq`'s own doc comment for
                // the full argument, including why `Positive` is anchored
                // completely differently (`CllRxEntry::set_seq_at_read`,
                // captured once per pass at batch-read time, not here).
                //
                // Codex PR review finding r3680112304 (PR #20): this same
                // critical section also eagerly publishes `link.
                // tx_suspended_by_error = true`, BEFORE `deliver_or_enqueue`
                // below exposes this frame -- publish-before-exposure. A
                // dispatch task reacting to this frame (a different poll
                // task on a dual-channel CLL's companion channel, or the
                // gRPC-handler task processing the client's own reactive
                // `SendMsg`/`StartComPrimitive` triggered by seeing the
                // frame) can call `dispatch_tx_item` the instant the frame is
                // delivered; without this eager write it could observe
                // `tx_suspended_by_error == false` and dispatch a
                // transmitting item before the end-of-pass reconciliation
                // loop (below) ever runs, defeating `CP_SuspendQueueOnError`
                // entirely. This mirrors the receive-phase timeout hook's own
                // existing set-before-expose invariant -- see
                // `wait_for_expected_response_inner`, where `tx_suspended_by_
                // error` is set under the `logical_links` lock strictly
                // before that function's own `send_error_event` call exposes
                // the timeout to the client.
                //
                // The gate here is `connect_generation` freshness plus live
                // `CP_SuspendQueueOnError` policy ONLY -- no sequence check
                // against `error_clear_seq`, unlike `queue_error_class_to_
                // apply`'s `Suspend` arm. That omission is deliberate, not an
                // oversight: `entry.suspend_seq` is captured from this exact
                // same live `error_clear_seq` two lines above, in this same
                // critical section, so comparing it back to itself here would
                // be vacuous -- this eager gate is precisely `queue_error_
                // class_to_apply`'s `Suspend` arm minus that vacuous seq
                // comparison. Deliberately NOT done here: no `error_clear_
                // seq`/`error_set_seq` bump (a bump would break the
                // end-of-pass loop's same-batch last-frame-wins `Positive`
                // clear semantics -- a later frame in this same batch that
                // classifies `Positive` must still be able to apply via the
                // unchanged `entry.set_seq_at_read`/`error_set_seq` check),
                // and no `TxItem::ResumeWake` emission (this transition is
                // only ever false -> true; the wake-worthy true -> false
                // direction is exclusively the end-of-pass loop's `Positive`
                // arm).
                if wrote_suspend {
                    let mut links = ctx.logical_links.lock().await;
                    if let Some(link) = links.get_mut(&entry.handle) {
                        entry.suspend_seq = Some(link.error_clear_seq);
                        if link.connect_generation == entry.connect_generation
                            && link.active.suspend_queue_on_error()
                        {
                            link.tx_suspended_by_error = true;
                        }
                    }
                }

                if !finalized_concat.is_empty() {
                    // ADR-204 Codex review (PR #116, round 3): `entry.cop_tags`
                    // (this pass's snapshot), not `ctx.primitives` -- see
                    // `CllRxEntry::cop_tags`'s own doc comment and
                    // `deliver_concat_batch_if_live`'s own doc comment for
                    // why a fresh `primitives` lookup here was racy.
                    deliver_concat_batch_if_live(
                        &ctx.logical_links,
                        &entry.cop_tags,
                        entry.handle,
                        ctx.channel_id,
                        entry.connect_generation,
                        finalized_concat,
                    )
                    .await;
                }

                let (
                    acceptance_id,
                    frame_cop_handle,
                    ecu_timing_change,
                    superseded_timing_seq,
                    absorbed,
                ) = match binding {
                    FrameBinding::Registrant {
                        acceptance_id,
                        cop_handle,
                        ecu_timing_change,
                        superseded_timing_seq,
                        absorbed,
                    } => (
                        acceptance_id,
                        Some(cop_handle),
                        ecu_timing_change,
                        superseded_timing_seq,
                        absorbed,
                    ),
                    // CP_TesterPresentReqRsp = 1 (spec: the MVCI protocol
                    // module drops the tester-present response): bound
                    // to tester-present's own reply/SOM-herald/TX-echo
                    // signature -- discarded here, before it would otherwise
                    // be buffered/fanned out.
                    FrameBinding::TesterPresent => continue,
                    // ADR-100 Decision §5 (S8): per spec, a response that
                    // matches no ComPrimitive's ExpectedResponseStructure
                    // is thrown away -- dropped before
                    // `push_cll_event`/live-delivery fan-out, not buffered and
                    // not delivered as unsolicited `ResultData` (behavior
                    // change from S3's deliberately-preserved old delivery
                    // path; migration: register a `NumSendCycles == 0`
                    // receive-only ComPrimitive with a broad
                    // `ExpectedResponseStructure`, ADR-059/ADR-100 §5).
                    //
                    // `bind_frame` returns `Unbound` for BOTH a genuinely
                    // unbound content frame and an indication frame
                    // (SOM/TxDone/loopback/RxBreak) that no tester-present
                    // arm claimed -- `is_content_frame` gates its own
                    // tier-1/tier-2 scans but not its final fallthrough (see
                    // `indication_frame_never_binds_to_a_registrant`, this
                    // module's tests). Decision §3 step 1 carves indication
                    // frames out of the unbound-discard rule entirely (they
                    // keep ADR-098's existing `ResultData` delivery path), so
                    // the discard here is gated on `is_content_frame`: only a
                    // content frame that reached step 6 is dropped.
                    FrameBinding::Unbound if is_content_frame => continue,
                    // ADR-151: a pure SOM/TxDone indication frame that no
                    // tester-present arm claimed is still subject to this
                    // CLL's own `CP_StartMsgIndEnable`/`CP_TransmitIndEnable`
                    // opt-in before it reaches the client -- see
                    // `indication_suppressed`'s own doc comment for the
                    // per-bit precedence.
                    FrameBinding::Unbound if indication_suppressed(entry, rx_status_flags) => {
                        continue;
                    }
                    FrameBinding::Unbound => (0, None, false, None, false),
                };

                // ADR-101 Decision §D: eager confirm-and-write-through for
                // `cyclic_deadline`, closing the race against
                // `reap_expired_cyclic_registrants` deleting a registrant on
                // the strength of a still-stale LIVE deadline while this
                // pass's own end-of-batch writeback (below) hasn't landed
                // yet. Scoped to exactly the registrant shape that can ever
                // have `cyclic_timeout_ms` set at all -- a created-receive-
                // only (`NumReceiveCycles == -1` or a finite `N > 0`,
                // ADR-182) registrant with a nonzero `CP_CyclicRespTimeout`
                // -- every other registrant shape takes no extra lock here
                // and is completely unaffected.
                // `matches_got` deliberately stays out of this: only
                // `cyclic_deadline` has a concurrent deletion consumer
                // racing the end-of-pass writeback.
                if let Some(cop_handle) = frame_cop_handle
                    && let Some(snap) = entry
                        .registrants
                        .iter()
                        .find(|r| r.cop_handle == cop_handle)
                    && is_eager_cyclic_deadline_confirm_scope(snap)
                {
                    // Copy the fields this step needs out of `snap` before
                    // taking the `logical_links` lock -- `snap` borrows
                    // `entry.registrants` immutably and the delivery block
                    // right below still needs `entry.rx_buf`/`entry.handle`.
                    let (cop_handle, connect_generation, restarted_deadline): (
                        u32,
                        u64,
                        Option<tokio::time::Instant>,
                    ) = (
                        snap.cop_handle,
                        snap.connect_generation,
                        snap.cyclic_deadline,
                    );
                    let mut links = ctx.logical_links.lock().await;
                    let confirmed = confirm_cyclic_deadline_writeback(
                        &mut links,
                        entry.handle,
                        cop_handle,
                        connect_generation,
                        restarted_deadline,
                    );
                    drop(links);
                    if !confirmed {
                        // Discard this frame rather than deliver `ResultData`
                        // under a `cop_handle` that no longer exists live.
                        continue;
                    }
                }

                // Codex round-5 finding, PR #17: record which (cll_handle,
                // cop_handle, frame_seq) triple was superseded THIS PASS by a
                // later response from the SAME physically addressed
                // registrant -- feeds `timing_frame_flag_ok`'s per-frame
                // supersession check once `pending_reservations` is finalized
                // below.
                if let Some(prev_seq) = superseded_timing_seq
                    && let Some(cop_handle) = frame_cop_handle
                {
                    superseded_timing.insert((entry.handle, cop_handle, prev_seq));
                }

                // ADR-115 round-3 correction: this frame is always pushed
                // through `push_cll_event` (cap/mode enforcement) first; the
                // queue's own `live_sender` (ADR-115 round 6), if any, then
                // opportunistically drains `rx_buf`'s full current contents
                // out live, FIFO, in the same critical section. `deliver_or_enqueue`
                // reads `live_sender` itself, fresh, under the queue lock it
                // takes -- no subscriber is captured here ahead of time. See
                // `deliver_or_enqueue`'s own doc comment for the full
                // contract.
                //
                // `absorbed` (ADR-148): this frame was absorbed into a
                // still-open concat buffer instead of completing a match on
                // its own -- no delivery for it yet; it is folded into
                // whichever future frame finally finalizes that buffer.
                // Always `false` for a non-concat registrant. Checked FIRST,
                // before the ADR-146 deferred-finalization branch below: an
                // absorbed frame must never be delivered nor reserved on its
                // own, regardless of `ecu_timing_change` -- a registrant with
                // BOTH `concat_enabled` and `timing_cfg` set CAN produce
                // `ecu_timing_change == true` here (its buffer's opening
                // segment, `bind_registrant`'s `observe_registrant_timing_change`
                // call), but the ComParam side effect that flag reflects was
                // already captured into `r.pending_timing_change` regardless
                // of delivery, and this specific frame's own diagnostic flag
                // is superseded by the eventual finalized delivery's own
                // (always-`false`, ADR-146/148 interaction, see
                // `ConcatDelivery::into_received_frame`'s own doc comment).
                //
                // ADR-146 deferred-finalization delivery (Codex round-5
                // findings, PR #17): a qualifying frame
                // (`ecu_timing_change == true`) reserves its true arrival
                // position in the queue via `reserve_pending_timing_frame`
                // instead of delivering immediately -- its flag isn't final
                // until this pass's fold/push/store step (below) resolves.
                // Every other frame is delivered right away, exactly as
                // before; the queue's own head-of-line barrier
                // (`drain_queue_live`) is what now preserves relative order
                // against a still-pending reservation, not any state tracked
                // here.
                if !absorbed {
                    // ADR-204 Codex review (PR #116, round 3): resolved from
                    // this pass's own `entry.cop_tags` snapshot -- captured
                    // by `build_cll_rx_entries` at the same instant as
                    // `entry.registrants` itself, well before this frame's
                    // own binding was even decided -- rather than a fresh,
                    // separately-locked `primitives` lookup here. A fresh
                    // lookup at this point (well after `wrote_suspend`'s
                    // eager-publish re-lock and `deliver_concat_batch_if_live`
                    // above, both `.await` points) could otherwise have
                    // raced a concurrent `CancelComPrimitive`/link-teardown
                    // path that already removed this `cop_handle`'s
                    // `primitives` entry, even though the registrant match
                    // that produced it was decided from the exact same
                    // snapshot `cop_tags` was captured alongside. Also
                    // unaffected by `frame` instead being reserved via
                    // `reserve_pending_timing_frame` below and read back much
                    // later -- `cop_tags` is a plain owned map, not a
                    // `primitives` lock, so there is nothing left to race
                    // regardless of how much later `frame` is actually
                    // delivered. See `CllRxEntry::cop_tags`'s own doc comment
                    // for the full argument.
                    let cop_tag = resolve_frame_cop_tag(entry, frame_cop_handle);
                    let frame = ReceivedFrame {
                        timestamp,
                        data: payload,
                        header_bytes: header,
                        footer_bytes: footer,
                        unique_resp_identifier,
                        acceptance_id,
                        cop_handle: frame_cop_handle,
                        cop_tag,
                        rx_status_flags,
                        ecu_timing_change,
                        sw_can_hv_rx,
                    };
                    if ecu_timing_change {
                        if let Some(reservation_id) =
                            reserve_pending_timing_frame(&entry.rx_buf, entry.handle, frame).await
                        {
                            pending_reservations.push(PendingTimingReservation {
                                rx_buf: Arc::clone(&entry.rx_buf),
                                cll_handle: entry.handle,
                                reservation_id,
                                cop_handle: frame_cop_handle.expect(
                                    "ecu_timing_change is only ever true when FrameBinding::Registrant supplied a cop_handle",
                                ),
                                frame_seq,
                            });
                        }
                    } else {
                        deliver_or_enqueue(&entry.rx_buf, entry.handle, CllQueueItem::Frame(frame))
                            .await;
                    }
                }
            }
        }
    }

    // Write this pass's accumulated `matches_got`/`pending_rc`/`cyclic_deadline`
    // bookkeeping back to the live registrants (ADR-100 Decision §3, §4;
    // ADR-101 Decision §A): `cll_entries`' `registrants` are an owned clone
    // (`CllRxEntry::registrants`'s doc comment), mutated locally by
    // `bind_frame` above across however many frames this batch contained, so
    // the live state -- consulted by both the NEXT poll pass's snapshot and
    // `poll_rx_and_check_match`'s outcome read-back -- must be merged in
    // once, here, per pass. `cyclic_deadline` (S6): `bind_registrant`'s
    // match-acceptance step restarts it on the snapshot copy alongside
    // `matches_got`, so it needs the exact same merge-back, or a restart
    // from THIS pass would never reach the live registrant
    // `wait_for_expected_response_inner`'s per-pass expiry check (and
    // `reap_expired_cyclic_registrants`, for an already-detached
    // registrant) actually reads.
    //
    // `merge_registrant_writeback` (ADR-101) applies a delta/monotone merge
    // rather than the absolute overwrite this block used before: a CLL with
    // a UUDT companion channel (ADR-046) is served by TWO independent poll
    // tasks, each running its own full snapshot -> mutate -> writeback
    // sequence against the SAME live registrant with no synchronization
    // between the two sequences, so an absolute overwrite from a later
    // pass's writeback (cloned from live state *before* an earlier pass's
    // own writeback landed) would silently discard the earlier pass's
    // contribution.
    // ADR-146 (amended -- Codex review + edge-case-hunter, PR #17): merges
    // every qualifying Access Timing exchange observed this pass into ONE
    // combined delta, applied to hardware AFTER the `logical_links` lock
    // below is released (ADR-110: `logical_links` must never be held across
    // an `api.lock().await` acquisition). All entries this pass produces
    // share `ctx.channel_id`, and more than one of them can independently
    // produce a qualifying observation of the SAME physical frame when two
    // CLLs share this channel (same `(j2534_protocol_id, baud_rate)`
    // `ChannelKey`) -- `timing_delta` combines same-key values with
    // `combine_derived_timing_value`'s worst-case direction table instead of
    // letting a later push silently clobber an earlier one via independent
    // `apply_params_to_hardware` calls.
    //
    // Per-CLL storage (`link.active`/`link.working`) is deferred to a THIRD
    // phase below, after the push's success/failure is known, following
    // `handle_update_param`'s own push-then-promote precedent (ADR-086/U2) --
    // storing eagerly and reverting on failure has no precedent anywhere else
    // in this file and would need the identical re-acquire/recheck machinery
    // anyway, with a revert-vs-concurrent-write race the deferred write does
    // not have. `timing_pending` carries each CLL's own folded observation
    // (`(handle, connect_generation-at-fold-time, CombinedTimingChange,
    // WorkingTimingSnapshot)`) across the push -- the snapshot (Codex
    // round-7 finding, PR #17) captures the SPECIFIC `link.working` values
    // this fold is about to (conditionally) overwrite, taken under this SAME
    // lock before the push's `.await` releases it, so `store_combined_
    // timing_change` can detect and preserve a concurrent `SetComParam`
    // write that lands on `link.working` during the await window (see
    // `WorkingTimingSnapshot`'s own doc comment).
    let mut timing_delta: HashMap<ComParamId, u32> = HashMap::new();
    let mut timing_protocol_id: Option<u32> = None;
    let mut timing_pending: Vec<(u32, u64, CombinedTimingChange, WorkingTimingSnapshot)> =
        Vec::new();
    // ADR-146 (amended -- edge-case-hunter finding on the round-4 fix, PR
    // #17): the `cop_handle` whose OWN exchange is the one this CLL's
    // arrival-order fold actually kept (the max-`frame_seq` contributor --
    // every TPI arm writes the identical 5-key derived quintuple, so
    // whichever registrant's observation `select_latest_timing_changes`
    // applied LAST owns every derived key, not just some of them). Needed
    // because `derived_stored` below is CLL-scoped, not registrant-scoped:
    // when two different registrants (= two independent SID 0x83 exchanges)
    // qualify on the same CLL this pass, the LOSING registrant's own derived
    // contribution was already fully overwritten before the hardware push
    // even ran, so its frames must have `ecu_timing_change` cleared even
    // though the CLL's overall push succeeded -- `derived_stored` alone
    // cannot tell the winner's frames from the loser's.
    let mut derived_winner: HashMap<u32, u32> = HashMap::new();
    // ADR-147: `(cll_handle, channel_key)` pairs whose `tx_suspended()` this
    // pass's `CP_SuspendQueueOnError` classification (applied below, under
    // the SAME `logical_links` critical section as the writeback loop above)
    // flipped true -> false, collected for a `TxItem::ResumeWake` send AFTER
    // `links` is released -- mirroring `ioctl_resume_tx_queue`'s own
    // lock-then-release-then-send shape (rpc_misc.rs) rather than
    // `handle_channel_hard_error`'s nested-`shared_channels` shape, so
    // `logical_links` and `shared_channels` are never held at once here.
    // This loop is the batch-final reconciliation authority over an eager
    // mid-batch `tx_suspended_by_error = true` publish that may already have
    // happened per-frame above (Codex PR review finding r3680112304), and
    // remains authoritative for last-frame-wins `Positive` overriding an
    // earlier `Suspend` within the same batch.
    let mut queue_error_wake_targets: Vec<(u32, ChannelKey)> = Vec::new();
    // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c Fix A): a
    // `Suspend` classification applied below must also terminate an
    // already-running TP2.0 broadcast periodic on the same CLL, in this SAME
    // critical section -- collected here and acted on (native stop, COP
    // finalization) after `links` is released, mirroring
    // `queue_error_wake_targets`'s own collect-then-act shape, via
    // `J2534Service::terminate_tp20_broadcast_periodic_for_suspension`.
    // Codex review fix (P2, PR #101, round 10): `connect_generation`/
    // `channel_key` are carried alongside `periodic`/`channel_id`, captured
    // from the SAME `link` reference in the SAME critical section below --
    // see `terminate_tp20_broadcast_periodic_for_suspension`'s own doc
    // comment for why the callee needs them.
    let mut suspended_broadcast_periodics: Vec<SuspendedBroadcastPeriodic> = Vec::new();
    {
        let mut links = ctx.logical_links.lock().await;
        for entry in &cll_entries {
            let Some(link) = links.get_mut(&entry.handle) else {
                continue; // CLL torn down mid-pass; nothing to merge back.
            };
            for snap in &entry.registrants {
                let Some(live) = link
                    .registrants
                    .iter_mut()
                    .find(|r| r.cop_handle == snap.cop_handle)
                else {
                    continue; // Registrant removed mid-pass; nothing to merge.
                };
                // A NEW registrant reusing the same `cop_handle` (or a
                // reconnect since this pass's snapshot was taken) must never
                // inherit a stale snapshot's counts -- same ADR-086
                // generation guard `bind_registrant` itself applies.
                if live.connect_generation != snap.connect_generation {
                    continue;
                }
                let Some(base) = entry
                    .registrant_baselines
                    .iter()
                    .find(|b| b.cop_handle == snap.cop_handle)
                else {
                    continue; // Should not happen: baselines are index-aligned with registrants.
                };
                merge_registrant_writeback(live, snap, base);
            }
            // ADR-146 (Decision, "Hardware application and locking", amended
            // -- Codex review round 4, PR #17): folds every registrant THIS
            // CLL saw this pass, not just the ones the writeback loop above
            // found still live (a registrant that only just detached to
            // tier-2, S5, still needs its final pass's observation folded
            // in) -- same ADR-086 generation guard as the writeback loop
            // above, applied per-registrant since a snapshot can outlive a
            // mid-pass reconnect of the same CLL. Folded by arrival order
            // (last-exchange-wins), not worst-case combination: two
            // different registrants qualifying on the same CLL this pass are
            // always two independent SID 0x83 exchanges, so the later one's
            // result simply supersedes the earlier one.
            let qualifying: Vec<(usize, u32, &PendingTimingChange)> = entry
                .registrants
                .iter()
                .filter_map(|snap| {
                    if link.connect_generation != snap.connect_generation {
                        return None;
                    }
                    snap.pending_timing_change
                        .as_ref()
                        .map(|(seq, c)| (*seq, snap.cop_handle, c))
                })
                .collect();
            if let Some(&(_, winner_cop_handle, _)) =
                qualifying.iter().max_by_key(|(seq, _, _)| *seq)
            {
                derived_winner.insert(entry.handle, winner_cop_handle);
            }
            let combined =
                select_latest_timing_changes(qualifying.iter().map(|&(seq, _, c)| (seq, c)));
            if !combined.derived.is_empty()
                || !combined.ecu_entries.is_empty()
                || !combined.session_entries.is_empty()
            {
                // ADR-158 correction (supersedes this comment's prior
                // ADR-157 Plane B framing): capture the RAW hw protocol id
                // -- this value later drives `to_j2534_config_id`/
                // `expand_tidle` (via `apply_params_to_hardware`), both of
                // which self-normalize internally from a single raw id, so
                // passing the raw id here (rather than pre-normalizing) is
                // required for `to_j2534_config_id` to detect and suppress
                // an FD-invalid param on an FD_CAN_PS link. Functionally a
                // no-op versus the base id once `expand_tidle` also
                // self-normalizes, but kept uniform with every other
                // `apply_params_to_hardware` call site's raw-id contract.
                timing_protocol_id.get_or_insert(link.hw_protocol_id);
                for (&id, &value) in &combined.derived {
                    timing_delta
                        .entry(id)
                        .and_modify(|existing| {
                            *existing = combine_derived_timing_value(id, *existing, value)
                        })
                        .or_insert(value);
                }
                // Codex round-7 finding, PR #17: snapshot the SPECIFIC
                // `link.working` values this fold is about to (conditionally)
                // overwrite, under this SAME lock, before the hardware
                // push's `.await` below releases it -- see
                // `WorkingTimingSnapshot`'s own doc comment.
                let working_snapshot = WorkingTimingSnapshot {
                    derived: combined
                        .derived
                        .keys()
                        .map(|&id| (id, link.working.unum32.get(&id).copied()))
                        .collect(),
                    access_timing_sf: link
                        .working
                        .structfield
                        .get(&PARAM_ACCESS_TIMING_ECU)
                        .cloned(),
                    session_timing_sf: link
                        .working
                        .structfield
                        .get(&PARAM_SESSION_TIMING_ECU)
                        .cloned(),
                };
                timing_pending.push((
                    entry.handle,
                    link.connect_generation,
                    combined,
                    working_snapshot,
                ));
            }

            // ADR-147: apply this pass's `CP_SuspendQueueOnError`
            // classification, if any, to the live CLL -- gated on the SAME
            // `connect_generation` freshness check the timeout hook uses (a
            // reconnected link must never have a stale pass's classification
            // applied to it), AND (ADR-147 sixth amendment, pre-read capture
            // for the batch anchor) on a DIRECTION-SPECIFIC seq check:
            // `Suspend` on `entry.suspend_seq` (captured at fold time)
            // matching the LIVE `error_clear_seq`; `Positive` on
            // `entry.set_seq_at_read` (an `Option<u64>`, captured by
            // `poll_rx_inner` BEFORE this pass's own read call even started,
            // `None` if this CLL was absent from that snapshot) matching
            // `Some(the LIVE error_set_seq)` -- see
            // `queue_error_class_to_apply`'s own doc comment for the race
            // shapes each closes, including the sixth amendment's
            // pre-read-anchored closure of the companion-`Positive`-vs-
            // primary-timeout race. The remaining Suspend-only
            // differentiator -- live Active `CP_SuspendQueueOnError == 1` at
            // apply time, a live, link-scoped policy read distinct from the
            // frozen `rc_cfg` snapshot `is_unhandled_negative` judged the
            // frame against -- stays folded into `queue_error_class_to_apply`
            // itself (fourth amendment restructure, unaffected by the fifth):
            // a discarded-by-policy `Suspend` is equivalent to no
            // classification at all for this call site's purposes (no state
            // changes, so no wake is possible either).
            if let Some(class) = queue_error_class_to_apply(
                entry.queue_error_class,
                entry.connect_generation,
                link.connect_generation,
                entry.suspend_seq,
                link.error_clear_seq,
                entry.set_seq_at_read,
                link.error_set_seq,
                link.active.suspend_queue_on_error(),
            ) {
                let old_effective = link.tx_suspended();
                match class {
                    QueueErrorClass::Positive => link.tx_suspended_by_error = false,
                    QueueErrorClass::Suspend => {
                        link.tx_suspended_by_error = true;
                        if let Some(periodic) = link.tp20_broadcast_periodic.take() {
                            suspended_broadcast_periodics.push((
                                entry.handle,
                                periodic,
                                link.channel_id,
                                link.connect_generation,
                                link.channel_key,
                            ));
                        }
                    }
                }
                if old_effective
                    && !link.tx_suspended()
                    && let Some(channel_key) = link.channel_key
                {
                    queue_error_wake_targets.push((entry.handle, channel_key));
                }
            }
        }
    }
    for (cll_handle, periodic, channel_id, connect_generation, channel_key) in
        suspended_broadcast_periodics
    {
        ctx.service
            .terminate_tp20_broadcast_periodic_for_suspension(
                cll_handle,
                periodic,
                channel_id,
                connect_generation,
                channel_key,
            )
            .await;
    }
    let timing_all_ok = if let Some(j2534_protocol_id) = timing_protocol_id {
        let delta = ComParamSet {
            unum32: timing_delta,
            ..ComParamSet::default()
        };
        apply_params_to_hardware(&ctx.api, ctx.channel_id, j2534_protocol_id, &delta).await
    } else {
        true
    };
    // ADR-146 (amended -- Codex round-2 Finding A, PR #17): the set of CLL
    // handles for which this pass's fold actually got its derived ComParam
    // values promoted into `link.active`/`link.working` -- i.e. the U2
    // recheck below found the link still live under the same
    // `connect_generation` AND `timing_all_ok`. The deferred-finalization
    // delivery step after this block (`finalize_pending_timing_frame`) uses
    // this to correct each reserved frame's `ecu_timing_change` flag so it
    // asserts "derived timing WAS stored" for THIS CLL, not merely "a
    // qualifying frame was observed this pass".
    let mut derived_stored: HashSet<u32> = HashSet::new();
    if !timing_pending.is_empty() {
        // ADR-086/U2 re-acquisition (mirroring `handle_update_param`'s own
        // post-hardware-`.await` recheck): the push above is a real hardware
        // I/O await a disconnect+reconnect of any of these CLLs can complete
        // during, even though each was live moments ago when folded.
        let mut links = ctx.logical_links.lock().await;
        for (handle, generation, combined, working_snapshot) in timing_pending {
            let Some(link) = links.get_mut(&handle) else {
                continue; // CLL torn down during the hardware push.
            };
            if link.channel_id != Some(ctx.channel_id) || link.connect_generation != generation {
                continue;
            }
            if store_combined_timing_change(link, &combined, timing_all_ok, &working_snapshot) {
                derived_stored.insert(handle);
            }
        }
    }
    for (cll_handle, channel_key) in queue_error_wake_targets {
        let tx_queue = {
            let chans = ctx.service.shared_channels.lock().await;
            chans.get(&channel_key).map(|sc| sc.tx_queue.clone())
        };
        if let Some(tx_queue) = tx_queue {
            let _ = tx_queue.send(TxItem::ResumeWake { cll_handle });
        }
    }

    // ADR-146 (amended -- Codex round-5 findings, PR #17): finalize every
    // reservation made this pass now that `derived_stored`/`derived_winner`/
    // `superseded_timing` are known, correcting and delivering each one via
    // `finalize_pending_timing_frame`. See `pending_reservations`'s own
    // declaration (above the per-frame loop) for the full rationale.
    //
    // Processed in recorded order (frame_seq order across the whole batch,
    // interleaved across CLLs) -- NOT reordered/sorted/parallelized: this
    // ensures an earlier reservation for a given CLL is always finalized
    // before a later one for the SAME CLL, so `drain_queue_live`'s barrier
    // fully unblocks that CLL's backlog by the time the last one for it
    // finalizes.
    for reservation in pending_reservations {
        let flag = timing_frame_flag_ok(
            reservation.cll_handle,
            reservation.cop_handle,
            reservation.frame_seq,
            &derived_stored,
            &derived_winner,
            &superseded_timing,
        );
        finalize_pending_timing_frame(
            &reservation.rx_buf,
            reservation.cll_handle,
            reservation.reservation_id,
            flag,
        )
        .await;
    }

    let outcome = classify_poll_batch(messages.len());
    // ADR-101 Decision §E: stamp the watermark AFTER the writeback block
    // above has landed -- see this function's own doc comment for why the
    // ordering matters.
    if outcome.is_drained() {
        ctx.drain_watermarks
            .lock()
            .await
            .insert(ctx.channel_id, read_started_at);
    }
    outcome
}

/// ADR-101 Decision §D: scopes the eager `cyclic_deadline` confirm-and-
/// write-through (`confirm_cyclic_deadline_writeback`, called from
/// `poll_rx_inner`) to exactly the registrant shape that can ever have
/// `cyclic_timeout_ms` set at all -- a created-receive-only
/// (`NumSendCycles == 0`, `NumReceiveCycles == -1` or a finite `N > 0`,
/// ADR-182) registrant with a nonzero `CP_CyclicRespTimeout`. Every other
/// registrant shape
/// (`RegistrantTier::ActiveSendReceive`, or a tier-2 registrant with
/// `cyclic_timeout_ms` unset/zero, e.g. a migrated (S5) IS-CYCLIC COP or the
/// ComParam disabled) must never take the extra `logical_links` lock this
/// step introduces. Extracted as a pure, sync, directly-unit-testable
/// predicate -- same rationale as `check_match_against_baseline`'s own
/// extraction (ADR-101 Decision §B) -- since `poll_rx_inner` itself is not
/// unit-testable (it always resolves through `ctx.api.lock().await`, which
/// requires a live `J2534Api0404` bound to a loaded shared library, only
/// available via the `grpc_mock` integration harness's mock `.so`).
fn is_eager_cyclic_deadline_confirm_scope(snap: &CopRegistrant) -> bool {
    snap.tier == RegistrantTier::ReceiveOnly
        && snap.cyclic_timeout_ms.filter(|&ms| ms > 0).is_some()
}

/// ADR-101 Decision §D: the eager confirm-and-write-through step itself,
/// called from `poll_rx_inner` between `bind_frame` returning a
/// `FrameBinding::Registrant` outcome for a registrant that
/// `is_eager_cyclic_deadline_confirm_scope` accepts, and that frame's
/// delivery. Closes the race against `reap_expired_cyclic_registrants`
/// deleting a registrant on the strength of a still-stale LIVE
/// `cyclic_deadline` while this pass's own end-of-batch writeback
/// (`merge_registrant_writeback`) hasn't landed yet -- see that ADR section
/// for the full rationale.
///
/// `restarted_deadline` is `snap.cyclic_deadline` (this pass's own snapshot,
/// already restarted by `bind_registrant` if this frame was the accepted
/// match) -- callers must copy it, along with `cop_handle`/
/// `connect_generation`, out of `snap` BEFORE calling this function, since
/// `snap` borrows the same `CllRxEntry::registrants` the caller's delivery
/// block still needs afterward.
///
/// Returns `true` when a live registrant was found (by `cop_handle` AND
/// `connect_generation`, the same pair `merge_registrant_writeback`'s own
/// call site keys on) and its `cyclic_deadline` was monotonically advanced;
/// `false` when the CLL itself is gone, no live registrant matches both keys
/// (already reaped, or a reconnect changed `connect_generation` since this
/// pass's snapshot was taken), or the registrant has a `CancelComPrimitive`
/// already marked in `cancelled_cops` but not yet reaped (edge-case-hunter
/// review of this Decision §D fix: this function itself introduces the only
/// `.await` point between `bind_frame` accepting a match and that frame's
/// delivery -- previously a purely synchronous span for every registrant --
/// so without this check, a cancel racing into exactly that new window would
/// still see the registrant "found" here and get its deadline extended plus
/// its frame delivered, moments before `reap_cancelled_detached_registrants`
/// removes it. The caller must then discard the frame instead of delivering
/// `ResultData` under a `cop_handle` that no longer exists live, or is about
/// to stop existing (same "unbound -> discarded" precedent as ADR-100
/// Decision §5/§6).
fn confirm_cyclic_deadline_writeback(
    links: &mut HashMap<u32, LogicalLinkState>,
    cll_handle: u32,
    cop_handle: u32,
    connect_generation: u64,
    restarted_deadline: Option<tokio::time::Instant>,
) -> bool {
    let Some(link) = links.get_mut(&cll_handle) else {
        return false; // CLL torn down mid-pass.
    };
    if link.cancelled_cops.contains(&cop_handle) {
        return false; // Cancel already marked; about to be reaped.
    }
    let Some(live) = link
        .registrants
        .iter_mut()
        .find(|r| r.cop_handle == cop_handle && r.connect_generation == connect_generation)
    else {
        return false; // Already reaped, or reconnected under a new generation.
    };
    live.cyclic_deadline = live.cyclic_deadline.max(restarted_deadline);
    true
}

/// ADR-147 fifth amendment (split direction-specific anchors, superseding
/// the fourth amendment's single unified counter): the end-of-pass
/// `CP_SuspendQueueOnError` classification apply DECISION, extracted as a
/// small pure function so it can be unit-tested directly (`poll_rx_inner`
/// itself is not unit-testable -- see `check_match_against_baseline_tests`'s
/// own doc comment for why). Returns `Some(class)` when this pass's
/// classification is still eligible to apply to the live CLL, `None` when it
/// must be discarded as stale (or when there was no classification to begin
/// with).
///
/// - `connect_generation` (ADR-086, pre-existing) applies to BOTH classes: a
///   reconnect completed since this pass's snapshot was taken, so the
///   snapshot no longer describes the live CLL at all.
/// - `Suspend` additionally requires `entry_suspend_seq == Some(live_error_clear_seq)`:
///   `Suspend`'s anchor is EXPOSURE, not fold time -- an explicit clear
///   cannot invalidate content the client never saw, and the fold-time
///   capture site (`poll_rx_inner`, immediately after `bind_frame` returns)
///   always runs strictly before this frame's delivery, so a clear landing
///   in the fold-to-capture gap is a genuinely NEW incident from the
///   clear's own perspective, not a race to fix -- it is CORRECT for such a
///   clear to invalidate this classification (`entry_suspend_seq` captured
///   the pre-clear value, which no longer matches). Any of the four
///   explicit-clear sites routed through `LogicalLinkState::
///   clear_error_suspension` (an ioctl resume via `ioctl_resume_tx_queue`, a
///   reset/clear-queue via `cancel_held_tx_items`'s `reset_suspended`
///   branch, a recovery `CoptUpdateparam` promotion via
///   `handle_update_param`, or a hard-error offline via
///   `handle_channel_hard_error`) may have bumped `error_clear_seq` AFTER
///   this classification's fold captured `entry_suspend_seq`; discarding in
///   that case is exactly the desired outcome, not a defensive fallback.
///   `entry_suspend_seq == None` (no `Suspend` fold ever captured a seq for
///   this pass, which should not coexist with `queue_error_class ==
///   Some(Suspend)` by construction, but is treated as stale defensively)
///   also discards.
/// - `Positive` additionally requires `entry_set_seq_at_read ==
///   Some(live_error_set_seq)`: `Positive`'s anchor is BATCH-READ evidence
///   order, not fold time -- a positive response can only genuinely resume a
///   suspension it postdates on the wire, and every frame in one
///   `PassThruReadMsgs` batch physically arrived before that read call
///   returned, so a `Positive` classified from ANY frame in this pass's
///   batch must be compared against `error_set_seq` as of THIS BATCH'S OWN
///   read time. `entry_set_seq_at_read` is `Option<u64>` (ADR-147 sixth
///   amendment, pre-read capture): captured by `poll_rx_inner` BEFORE this
///   pass's `PassThruReadMsgs` call even starts, under its own
///   `logical_links` lock acquisition, and threaded into
///   `build_cll_rx_entries` -- achieving the inequality `T_capture <=
///   T_read-start <= T_read-completion`, since exact "at read-completion"
///   equality is structurally unachievable across the two different async
///   locks involved (`ctx.api` for the read, `ctx.logical_links` for the
///   counter). `entry_set_seq_at_read == None` means this CLL was absent
///   from that pre-read snapshot (connected in the narrow window between
///   the snapshot and `build_cll_rx_entries` running) -- always discarded,
///   conservatively, since there is no valid batch-anchor to compare;
///   self-correcting on this CLL's next pass. This correctly discards a
///   `Positive` from a batch whose pre-read capture happened to run before a
///   receive-phase timeout hook's own bump of `error_set_seq` landed -- the
///   wire evidence for that `Positive` cannot be proven to postdate the
///   timeout, so it must never be allowed to clear a suspension it might not
///   actually postdate. A `Positive` from a LATER batch (read after the
///   timeout) correctly captures the post-timeout `error_set_seq` at ITS OWN
///   pre-read capture and so still applies, closing the genuine-recovery
///   direction. `Suspend` is never compared against `error_set_seq`, and
///   `Positive` is never compared against `error_clear_seq` -- design-advisor's
///   trace confirmed no cross-check is needed: an explicit clear racing a
///   stale `Positive` is clear-vs-clear (the stale `Positive`, even applied,
///   is a wake-free no-op -- the flag is already `false` from the clear); a
///   timeout racing a stale `Suspend` is set-vs-set (idempotent -- the flag
///   is already `true` from the timeout).
///
/// Governing rule (ADR-147 sixth amendment, refining the fifth): explicit
/// clears invalidate `Suspend` content anchored at EXPOSURE (fold-time
/// capture, since capture always precedes delivery); autonomous sets (the
/// timeout hook) invalidate `Positive` content anchored at BATCH-READ
/// evidence order, now captured BEFORE the read starts rather than at
/// `build_cll_rx_entries` construction time (which still ran strictly after
/// the read returned, with a genuine intervening `.await` a concurrent bump
/// could land in). The unified `error_state_seq` (fourth amendment) was
/// superseded because one capture point cannot correctly serve both anchors
/// simultaneously -- that was the fourth amendment's own design error, not
/// an implementation bug in it: a fold-time capture (even made perfectly
/// atomic with the fold) still gets the `Positive` direction wrong whenever
/// a same-batch timeout's bump lands, in program order, between an earlier
/// frame's arrival on the wire and that frame's own fold running. The fifth
/// amendment's construction-time capture fixed that but introduced a new,
/// narrower version of the same shape (a gap after read-completion, before
/// capture); the sixth amendment closes it by capturing before the read
/// even starts, trading equality for the achievable, sufficient inequality
/// above. Deferred-vs-deferred (two passes' classifications racing each
/// other) remains last-writer-wins per the existing cross-channel residual
/// for BOTH directions, which is why classification APPLIES do not
/// themselves bump either counter -- making them bump would re-key
/// cross-channel outcomes to apply order rather than evidence order, the
/// exact defect the residual already documents, just manifesting as discard
/// instead of overwrite.
///
/// `live_suspend_queue_on_error` (fourth amendment restructure, unaffected
/// by the fifth): the ONE remaining asymmetry between the two directions --
/// `Suspend` additionally requires live Active `CP_SuspendQueueOnError == 1`
/// at apply time, a live, link-scoped policy read distinct from the frozen
/// `rc_cfg` snapshot `is_unhandled_negative` judged the frame against -- is
/// folded into this function too, rather than left as a second check at the
/// call site, so the full apply decision is a single pure, unit-testable
/// truth table. A `Suspend` classification that clears every other gate but
/// finds live policy `false` here is discarded exactly like a stale one: no
/// state change would result either way (the flag is never set), so treating
/// it as "nothing to apply" is behaviorally identical to the pre-restructure
/// caller-side no-op and skips the (harmless but pointless) wake-eligibility
/// check at the call site too.
#[allow(clippy::too_many_arguments)]
fn queue_error_class_to_apply(
    queue_error_class: Option<QueueErrorClass>,
    entry_connect_generation: u64,
    live_connect_generation: u64,
    entry_suspend_seq: Option<u64>,
    live_error_clear_seq: u64,
    entry_set_seq_at_read: Option<u64>,
    live_error_set_seq: u64,
    live_suspend_queue_on_error: bool,
) -> Option<QueueErrorClass> {
    let class = queue_error_class?;
    if live_connect_generation != entry_connect_generation {
        return None;
    }
    match class {
        QueueErrorClass::Suspend => {
            if entry_suspend_seq != Some(live_error_clear_seq) {
                return None;
            }
            if !live_suspend_queue_on_error {
                return None;
            }
        }
        QueueErrorClass::Positive => {
            // ADR-147 sixth amendment: `None` (this CLL absent from the
            // pre-read snapshot) has no valid batch-anchor to compare --
            // always discarded, conservatively, same direction as a
            // mismatched `Some(seq)`.
            if entry_set_seq_at_read != Some(live_error_set_seq) {
                return None;
            }
        }
    }
    Some(class)
}

/// One registrant's writeback delta for one poll pass (ADR-101 Decision §A),
/// applied onto `live` in place: `snap`/`base` come from the SAME `CllRxEntry`
/// (`snap` is this pass's mutated clone, `base` is that clone's own starting
/// values, captured together in `build_cll_rx_entries`). Delta/monotone,
/// never an absolute overwrite -- see this function's call site (inside
/// `poll_rx_inner`'s writeback block) for why an absolute overwrite is wrong
/// whenever a CLL has a UUDT companion channel (ADR-046) and so is served by
/// two independent poll tasks racing this exact snapshot/mutate/writeback
/// sequence. Callers must have already checked `live.connect_generation ==
/// snap.connect_generation` (ADR-086) -- this function does not re-check it.
/// Merges four fields: `matches_got` (summed per-pass delta), `pending_rc`
/// (first-transition-wins), `cyclic_deadline` (monotone max), and `tier`
/// (ADR-100 round-9 Finding-2 correction / ADR-101 Decision §A's new `tier`
/// bullet: a one-way `ActiveSendReceive -> ReceiveOnly` latch).
fn merge_registrant_writeback(
    live: &mut CopRegistrant,
    snap: &CopRegistrant,
    base: &RegistrantBaseline,
) {
    // `matches_got` only ever increments within a single pass
    // (`bind_registrant`'s `r.matches_got += 1`), so `snap` can never fall
    // below its own pass's baseline.
    debug_assert!(
        snap.matches_got >= base.matches_got,
        "a pass's own snapshot must never decrease matches_got below its own baseline"
    );
    // Sum each pass's own contribution instead of overwriting with whichever
    // pass's absolute count landed last -- this is what actually closes the
    // cross-channel bug: two concurrent passes, each accepting their own
    // match against a baseline of 0, must both be reflected in `live`.
    live.matches_got = live
        .matches_got
        .saturating_add(snap.matches_got - base.matches_got);
    // Merge only a transition made BY THIS PASS (baseline `None` ->
    // snapshot `Some`); never overwrite a live `Some` a different pass, or a
    // consumer reset racing this merge, already established. A snapshot
    // taken before a consumer reset, itself unchanged at `Some(x)`, would
    // otherwise resurrect a stale `x` over a live `None` the reset
    // legitimately produced.
    if base.pending_rc.is_none()
        && let Some(code) = snap.pending_rc
        && live.pending_rc.is_none()
    {
        live.pending_rc = Some(code);
    }
    // No baseline needed: a deadline only ever moves forward (restarted on
    // an accepted match) or the registrant stops existing, so taking
    // whichever deadline is further in the future is always correct.
    // `Option<Instant>`'s derived `Ord` (`None < Some`) gives the right
    // behavior for a registrant with no cyclic deadline configured at all.
    live.cyclic_deadline = live.cyclic_deadline.max(snap.cyclic_deadline);
    // One-way latch: a migration only ever moves forward (tier-1 -> tier-2)
    // or the registrant stops existing, never the reverse, so no baseline is
    // needed -- "adopt the more-migrated of the two" is unconditionally
    // correct regardless of which pass's snapshot is newer. Never flips a
    // live `ReceiveOnly` registrant back to `ActiveSendReceive` from a stale
    // tier-1 snapshot.
    if snap.tier == RegistrantTier::ReceiveOnly {
        live.tier = RegistrantTier::ReceiveOnly;
    }
    // ADR-146/ADR-150: the running Access Timing (KWP) / Session Timing
    // (UDS) worst-case accumulator, restarted/combined by `bind_registrant`
    // on `snap` this pass. Before ADR-150, this field was only ever `Some`
    // on an ISO14230 channel (`timing_cfg`'s own from_params gate), and
    // ISO14230 has no UUDT-companion-channel scenario (ADR-046 is
    // CAN-family-only) -- so, UNLIKE `matches_got`/`pending_rc`/
    // `cyclic_deadline`, this CLL was never served by two independent poll
    // tasks racing this exact snapshot/mutate/writeback sequence, and a
    // plain overwrite from `snap` was safe without a delta/baseline scheme.
    // That premise no longer holds once `timing_cfg` also covers ISO15765
    // (which DOES have a UUDT companion channel, ADR-046): a companion-task
    // pass that observed no qualifying response this pass still clones
    // `live.timing_accumulator` into its own `snap` unchanged, and an
    // unconditional overwrite here would let that pass's stale write land
    // AFTER a concurrently-running primary-task pass's own fresher write for
    // the SAME registrant, clobbering it. Guarded the same way `pending_rc`
    // above is: only overwrite when THIS pass's own snapshot actually
    // differs from what it started with (`base`), i.e. this specific pass
    // is the one that changed it.
    if snap.timing_accumulator != base.timing_accumulator {
        live.timing_accumulator = snap.timing_accumulator;
    }
    // ADR-148. `concat_segments_got`
    // merges by the exact same delta/monotone pattern as `matches_got`
    // above -- always `0` for a non-concat registrant, so both the assert
    // and the merge are no-ops there.
    debug_assert!(
        snap.concat_segments_got >= base.concat_segments_got,
        "a pass's own snapshot must never decrease concat_segments_got below its own baseline"
    );
    live.concat_segments_got = live
        .concat_segments_got
        .saturating_add(snap.concat_segments_got - base.concat_segments_got);
    // `concat` (the open buffers' own contents, as opposed to the segment
    // COUNT above) is overwritten wholesale from `snap` (ADR-148 Amendment:
    // now a `Vec<ConcatBuf>`, still one plain overwrite of the whole `Vec`)
    // rather than delta-merged -- there is no meaningful "sum" of two buffer
    // states, only "this pass's own final state". A plain overwrite is safe
    // specifically
    // because concat-eligible protocols (KWP/J1850 family --
    // `CopRegistrant::concat_enabled`'s own doc comment) never have the
    // CAN/ISO15765 UUDT-companion dual-poll-task setup that forces
    // `matches_got`'s delta treatment elsewhere in this function (ADR-046):
    // a concat-eligible registrant is therefore ever touched by exactly ONE
    // poll task, so `snap.concat` can never be racing a concurrently-running
    // sibling pass's own write the way `matches_got` must guard against --
    // "overwrite with this pass's snapshot" and "overwrite only when it
    // changed vs baseline" produce the identical live value here, since
    // there is no second contributor whose own change could otherwise be
    // clobbered.
    live.concat = snap.concat.clone();
}

/// Polls the adapter for received frames once and fans them out to all CLLs on
/// `channel_id`.  Propagates [`poll_rx_inner`]'s [`PollOutcome`] unchanged --
/// see that function's doc comment for the full contract, including the
/// exhaustive-drain signal `handle_delay` and `run_due_tick_duties` both
/// consume.
async fn poll_rx(ctx: &ChannelPollCtx) -> PollOutcome {
    poll_rx_inner(ctx, None).await
}

/// Cancels all active primitives for `cll_handle`: removes them from `primitives`
/// and emits `PduCopstCancelled` for each one. Also delegates to
/// `cancel_held_tx_items` to drain this CLL's held TX queue (`tx_held`,
/// populated by `PDU_IOCTL_SUSPEND_TX_QUEUE`) and reset `tx_suspended` --
/// otherwise those items would sit in `tx_held` across a disconnect/reconnect
/// (same `cll_handle`) and a later `PDU_IOCTL_RESUME_TX_QUEUE` would replay
/// them onto the new connection even though the client already received
/// `PduCopstCancelled` for them. A held item's cop is always already caught
/// by the `primitives` scan above, so `cancel_held_tx_items` will not
/// double-notify for it.
///
/// Called from `disconnect_com_logical_link` and `destroy_com_logical_link` to
/// notify subscribers of queued COPs that will never execute.
pub(super) async fn cancel_link_cops(
    primitives: &Arc<Mutex<HashMap<u32, CopEntry>>>,
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    subscriptions: &Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    terminal_cops: &Arc<Mutex<TerminalCopsLedger>>,
    cll_handle: u32,
) {
    // ADR-128 correction (Codex review round 2): removal and emission used
    // to be two separate passes -- a batch removal under one `primitives`
    // critical section, released, THEN a loop emitting `send_cop_status`
    // for each cop with no `primitives` lock held at all. That left every
    // cop in the batch (not just a momentary single-cop gap) visible to a
    // concurrent `CancelComPrimitive`/`GetStatus` as "missing from BOTH
    // `primitives` and `terminal_cops`" for as long as the emission loop
    // takes. Now one continuous `primitives` critical section spans the
    // whole remove-then-emit sequence, per cop -- matching
    // `emit_terminal_if_live`'s single-cop invariant, just repeated for a
    // batch. Dropped BEFORE acquiring `logical_links` below: this crate's
    // documented order is `logical_links -> primitives`, never the reverse.
    let queue_target = resolve_queue_target(logical_links, cll_handle).await;
    {
        let mut prims = primitives.lock().await;
        let to_cancel: Vec<u32> = prims
            .iter()
            .filter(|&(_, entry)| entry.cll_handle == cll_handle)
            .map(|(&cop, _)| cop)
            .collect();
        for cop in to_cancel {
            let cop_tag = prims.remove(&cop).and_then(|entry| entry.cop_tag);
            send_cop_status(
                subscriptions,
                terminal_cops,
                queue_target.as_ref(),
                cll_handle,
                cop,
                PduComPrimitiveStatus::PduCopstCancelled,
                cop_tag,
            )
            .await;
        }
    }

    // ADR-100: clear this CLL's response-binding registry too, so a
    // registrant a still-in-flight receive phase inserted does not linger
    // visible for the (bounded) time it takes that phase's own per-pass
    // staleness check to notice this CLL going away and remove it itself via
    // `wait_for_expected_response`. S1 of ADR-100: write-only -- nothing
    // reads `registrants` yet.
    //
    // Also wholesale-clear `cancelled_cops` (PR #78 edge-case-hunter follow-up,
    // closing the leak's `cancel_link_cops` half): this CLL is going away
    // entirely, so any lingering mark -- whether for a cop this call's own batch-remove
    // loop above just cancelled, or for an entirely unrelated cop that
    // reached a terminal outcome by some other path and was never drained --
    // is moot, exactly like `registrants.clear()` above.
    if let Some(link) = logical_links.lock().await.get_mut(&cll_handle) {
        link.registrants.clear();
        link.cancelled_cops.clear();
    }

    // Drain (and, since the link is going away, unsuspend) this CLL's held TX
    // queue. `cancel_held_tx_items` is defensive against the primitives scan
    // above having already cancelled a held item's cop -- see its doc comment.
    cancel_held_tx_items(
        primitives,
        logical_links,
        subscriptions,
        terminal_cops,
        cll_handle,
        None,
        true,
    )
    .await;
}

/// Drains `cll_handle`'s held TX queue (`tx_held`, populated by
/// `PDU_IOCTL_SUSPEND_TX_QUEUE`, `LOCK_PHYSICAL_TX_QUEUE`, and/or
/// `CP_SuspendQueueOnError`) and emits `PduCopstCancelled` for each drained
/// item's cop, removing it from `primitives`. When `reset_suspended` is
/// `true`, also resets `tx_suspended_by_ioctl` to `false` and routes
/// `tx_suspended_by_error` through `LogicalLinkState::clear_error_suspension`
/// in the same critical section (ADR-147 third amendment, capture-at-fold
/// sequencing -- this explicit escape must invalidate any in-flight
/// `CP_SuspendQueueOnError` `Suspend` classification a concurrent poll pass
/// computed before this call but has not yet applied; `error_clear_seq` now
/// bumps unconditionally -- see that method's doc comment for why that is
/// safe again now that capture moved to fold time; a `Positive`
/// classification is unaffected by this call entirely, since its own anchor
/// is batch-read time against `error_set_seq`, ADR-147 fifth amendment) --
/// ADR-147 groups the error-suspend clear with this ioctl-class
/// reset (not the lock-class one) since, like `tx_suspended_by_ioctl`, it
/// protects no sibling CLL's privilege and so is safe to fully escape here;
/// `tx_suspended_by_lock` is never touched here (ADR-123), it is owned
/// exclusively by `recompute_lock_tx_suspensions`. This scoping matters because this
/// function's callers fall into two groups: `cancel_link_cops` (disconnect/
/// destroy, and -- since ADR-123 Finding I -- hard channel error via
/// `handle_channel_hard_error`), where a separate `recompute_lock_tx_suspensions`
/// sweep already handles `tx_suspended_by_lock` correctly once `held_lock_mask`
/// is cleared; and `PDU_IOCTL_RESET`/`PDU_IOCTL_CLEAR_TX_QUEUE` (rpc_misc.rs), where
/// `held_lock_mask` is untouched and a lock-driven suspension must survive --
/// a client-issued RESET/CLEAR_TX_QUEUE must never let a client bypass
/// another CLL's held physical-resource lock.
///
/// Per the codebase's own invariant (`StartComPrimitive` inserts into
/// `primitives` before the poll task ever parks an item into `tx_held`),
/// every held item's cop is already present in `primitives` at the time this
/// runs. A caller that already cancelled overlapping cops via its own
/// `primitives` scan (e.g. `cancel_link_cops`'s primitives-scan-and-cancel
/// pass) must not be double-notified here: `primitives.remove` is used as
/// the atomic first-wins discriminator, mirroring the implicit-cancel branch
/// in `should_skip_cancelled_item` -- only notify when this call is the one
/// that actually removes the entry.
///
/// `expected_generation` (ADR-161): when `Some(g)`, this call is a no-op
/// (no drain, no suspend-flag reset, no `stop_comm_pending` clear, no
/// `PduCopstCancelled` emission) unless the live `LogicalLinkState`'s
/// `connect_generation` still equals `g`, checked inside this function's one
/// `logical_links` critical section so the check and the mutation stay
/// atomic. This exists for `PDU_IOCTL_RESET` (`rpc_misc.rs`), whose snapshot
/// of `cll_handle`s is taken well before this call runs and can otherwise
/// reach forward across a same-`cll_handle` disconnect+reconnect to
/// destructively cancel a brand-new session's own already-queued work.
/// `ioctl_clear_tx_queue` and `cancel_link_cops` (this function's other two
/// call sites) pass `None`: neither has a cross-`.await` staleness window of
/// its own (single acquisition for the former; the link is already being
/// torn down for the latter).
pub(super) async fn cancel_held_tx_items(
    primitives: &Arc<Mutex<HashMap<u32, CopEntry>>>,
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    subscriptions: &Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    terminal_cops: &Arc<Mutex<TerminalCopsLedger>>,
    cll_handle: u32,
    expected_generation: Option<u64>,
    reset_suspended: bool,
) {
    let held_cops: Vec<u32> = {
        let mut links = logical_links.lock().await;
        match links.get_mut(&cll_handle) {
            Some(link) => {
                if let Some(expected) = expected_generation
                    && link.connect_generation != expected
                {
                    return;
                }
                if reset_suspended {
                    link.tx_suspended_by_ioctl = false;
                    link.clear_error_suspension();
                }
                let drained: Vec<TxItem> = link.tx_held.drain(..).collect();
                // A held CoptStopcomm cancelled here never reaches
                // handle_stop_comm, which is the only other place that
                // clears stop_comm_pending on a non-error path -- without
                // this, a still-comm_started link would reject every later
                // CoptStopcomm with "already in progress" until disconnect/
                // hard-error (Codex-review fix, ADR-085 amendment).
                if drained
                    .iter()
                    .any(|item| matches!(item, TxItem::StopComm { .. }))
                {
                    link.stop_comm_pending = false;
                }
                drained.into_iter().map(|item| item.handles().0).collect()
            }
            None => Vec::new(),
        }
    };

    for cop in held_cops {
        emit_terminal_if_live(
            primitives,
            logical_links,
            subscriptions,
            terminal_cops,
            cll_handle,
            cop,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
    }
}

/// Spawns a background task that:
///
/// - **Outbound** (`tx_rx` branch): dequeues `TxItem`s sent by `StartComPrimitive`
///   and dispatches them to the J2534 adapter:
///   - `SendRecv` → `PassThruWriteMsgs`
///   - `StartComm` → init sequence + periodic tester-present start + status update
///   - `StopComm` → periodic tester-present stop + status update
/// - **Inbound** (timer branch, 10 ms interval): calls `PassThruReadMsgs` — the
///   *only* place in the service that calls `read_messages` for this channel —
///   pushes received frames into `rx_buf` (for `GetEventItem` consumers) *and*
///   forwards them to the `SubscribeEvent` stream (fan-out).
///
/// Centralising all J2534 API calls here serialises them through the queue and
/// eliminates races that would occur if callers called into the adapter directly.
///
/// The task stops when any of these occurs:
/// - `cancel` sender is dropped (link disconnected / destroyed)
/// - `shutdown` watch fires
/// - `tx_rx` channel is closed (all `TxItem` senders dropped)
/// - `PassThruReadMsgs` returns a hard error (channel closed externally)
pub(super) fn spawn_channel_poll_task(
    tx_rx: mpsc::UnboundedReceiver<TxItem>,
    tx_requeue: mpsc::UnboundedSender<TxItem>,
    cancel: oneshot::Receiver<()>,
    shutdown: Receiver<bool>,
    ctx: ChannelPollCtx,
) {
    tokio::spawn(poll_channel_events(
        tx_rx, tx_requeue, cancel, shutdown, ctx,
    ));
}

/// Checks whether `(item_cop, item_cll_pre)` was cancelled — explicitly via
/// `CancelComPrimitive`, or implicitly because the CLL disconnected or was
/// destroyed while the item was queued — and, if so, emits `PduCopstCancelled`
/// and cleans up bookkeeping. Returns `true` when the poll task should skip
/// dispatching this item (`continue` the outer loop).
///
/// Two reasons a COP might be cancelled at this point:
/// (a) Explicit: `cancel_com_primitive` intentionally left it in `primitives`
///     (so `GetStatus` keeps returning Cancelled until the event fires) and
///     set `cancelled_cops`. `primitives.remove` here is the first-wins
///     discriminator against a concurrent `cancel_link_cops` teardown, same
///     as case (b) below.
/// (b) Implicit: the CLL disconnected/destroyed while this item was queued.
///     `cancel_link_cops` already sent `PduCopstCancelled` for in-`primitives`
///     COPs and removed them; use `primitives.remove` as the first-wins
///     discriminator.
///
/// A queued `StopComm` that is explicitly cancelled (via `CLEAR_TX_QUEUE` or
/// `CancelComPrimitive`) is now skipped like any other COP: `PduCopstCancelled`
/// is emitted, `handle_stop_comm` never runs, and `comm_started` stays `true`
/// (a truthful, retryable state -- the TP session genuinely never stopped).
/// `stop_comm_pending` is cleared in the same critical section so a follow-up
/// `CoptStopcomm` is not permanently rejected with "already in progress".
///
/// A prior version of this function carved `StopComm` out of the skip path so
/// it would always execute even when explicitly cancelled, on the theory that
/// it "must execute to clear `comm_started` state". That carve-out predated
/// ADR-085: once `CoptStopcomm` began transmitting a non-empty `cop_data`
/// payload as a final fire-and-forget message, the carve-out meant a queued
/// `StopComm` cancelled via `PDU_IOCTL_CLEAR_TX_QUEUE` or `CancelComPrimitive`
/// would silently transmit that payload anyway, contradicting
/// `CLEAR_TX_QUEUE`'s documented "clear pending TX" contract. This is a round-4
/// fix on the ADR-085 amendment: the carve-out is removed so a COP is either
/// cancelled or executes, never a partial "cancelled but still transmits"
/// hybrid.
async fn should_skip_cancelled_item(
    item_cop: u32,
    item_cll_pre: u32,
    is_stop_comm: bool,
    item_generation: Option<u64>,
    ctx: &ChannelPollCtx,
) -> bool {
    let primitives = &ctx.primitives;
    let subscriptions = &ctx.subscriptions;
    let terminal_cops = &ctx.service.terminal_cops;
    let logical_links = &ctx.logical_links;
    let (explicit_cancel, cll_is_active, generation_stale, queue_target) = {
        let mut links = logical_links.lock().await;
        match links.get_mut(&item_cll_pre) {
            Some(l) if l.connected => {
                let was_cancelled = l.cancelled_cops.remove(&item_cop);
                if is_stop_comm && was_cancelled {
                    // A queued CoptStopcomm that was explicitly cancelled (CLEAR_TX_QUEUE
                    // or CancelComPrimitive) is now cancelled like any other COP --
                    // PduCopstCancelled, handle_stop_comm never runs, comm_started stays
                    // true (a truthful, retryable state; TP genuinely still running).
                    // stop_comm_pending must be cleared in this same critical section,
                    // or a follow-up CoptStopcomm would be permanently rejected with
                    // "already in progress" (the same class of bug round 3 fixed for
                    // the tx_held path -- see ADR-085).
                    l.stop_comm_pending = false;
                }
                // ADR-086 (round 5): a queued item's captured connect_generation
                // no longer matching the live one means a disconnect+reconnect
                // happened while this item sat in the queue -- `cancel_link_cops`
                // already cancelled it directly (bypassing `cancelled_cops`), and
                // `l.connected`/`was_cancelled` alone can't see that, since the
                // reconnect made `connected` true again and this item was never
                // in `cancelled_cops` to begin with. Deliberately does NOT clear
                // `stop_comm_pending` here: `DisconnectComLogicalLink` already
                // resets it as part of its own teardown (rpc_link.rs), and a new
                // session on this same cll_handle may have its own legitimate
                // CoptStopcomm in progress with it set -- touching it here would
                // corrupt that new session's state.
                let generation_stale = item_generation.is_some_and(|g| g != l.connect_generation);
                (
                    was_cancelled,
                    true,
                    generation_stale,
                    Some(CllQueueTarget::from_link(l)),
                )
            }
            _ => (false, false, false, None),
        }
    };
    if !explicit_cancel && !generation_stale && cll_is_active {
        return false;
    }

    if explicit_cancel {
        // First-wins through `primitives`, the same discriminator every other
        // CANCELLED emitter uses: cancel_com_primitive intentionally left the
        // entry in place, so this remove normally succeeds and this call
        // emits the one CANCELLED. A concurrent teardown (`cancel_link_cops`)
        // that already removed the entry has already emitted it -- skip,
        // never duplicate. (executing_cop is not set here — the item was
        // skipped, not executed.)
        let mut prims = primitives.lock().await;
        if let Some(entry) = prims.remove(&item_cop) {
            send_cop_status(
                subscriptions,
                terminal_cops,
                queue_target.as_ref(),
                item_cll_pre,
                item_cop,
                PduComPrimitiveStatus::PduCopstCancelled,
                entry.cop_tag,
            )
            .await;
        }
    } else {
        // Implicit cancel (CLL went offline): cancel_link_cops may have
        // already removed the entry; only notify when we are first to remove.
        emit_terminal_if_live(
            primitives,
            logical_links,
            subscriptions,
            terminal_cops,
            item_cll_pre,
            item_cop,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
    }
    {
        let mut links = logical_links.lock().await;
        if let Some(link) = links.get_mut(&item_cll_pre) {
            link.cancelled_cops.remove(&item_cop);
        }
    }
    true
}

/// Why a request transmission did not complete.
enum TxFailure {
    /// A frame failed to build or write; report `PduErrorEvent` and finish
    /// the COP as today.
    Event(PduErrorEvent),
    /// A hard channel error occurred mid-transmission;
    /// `handle_channel_hard_error` has already emitted the full
    /// loss-of-comms event sequence (including `PduCopstCancelled` for this
    /// COP) — the caller must not emit any further status.
    ChannelLost,
    /// `CancelComPrimitive` arrived while the software ISO-TP driver was
    /// waiting for a FlowControl frame; the caller emits `PduCopstCancelled`.
    /// Only ever produced when `isotp_send`'s (and, transitively,
    /// `transmit_request`'s/`transmit_request_inner`'s) `cancellable`
    /// parameter is `true` -- `false` (`handle_stop_comm`'s call site)
    /// skips the `cancelled_cops` check entirely, so this variant can never
    /// occur there.
    Cancelled,
}

/// Builds one raw CAN frame (`[4-byte CAN ID][pci_payload]`, padded per
/// `framing`) and writes it to `ctx.channel_id`.
///
/// This is the sole chokepoint through which every software-ISO-TP frame
/// (SingleFrame, FirstFrame, and each ConsecutiveFrame) reaches the wire, so
/// it is also where `last_bus_activity` (ADR-083) is stamped for that path:
/// on a successful write, gated by `count_as_bus_activity`, immediately
/// after the write succeeds -- not deferred to `isotp_send`'s or
/// `transmit_request`'s overall result. A multi-frame send that writes a
/// FirstFrame (and possibly some ConsecutiveFrames) and only later fails --
/// e.g. no FlowControl within `CP_STmin`/N_Bs -- still put real activity on
/// the bus; stamping per-frame here means that activity is not lost just
/// because the send's overall `Result` ends up `Err`.
async fn write_can_frame(
    ctx: &ChannelPollCtx,
    tx_flags: u32,
    can_id_bytes: &[u8],
    mut pci_payload: Vec<u8>,
    framing: IsoTpFraming,
    count_as_bus_activity: bool,
) -> Result<(), TxFailure> {
    if framing.pad {
        isotp::pad_frame(&mut pci_payload, framing.filler);
    }
    let mut frame = can_id_bytes.to_vec();
    frame.extend_from_slice(&pci_payload);
    let result = {
        let api = ctx.api.lock().await;
        match j2534_0404::PassThruMessage::new(j2534_0404::CAN, 0, tx_flags, 0, 0, &frame) {
            Ok(mut msg) => api
                .write_messages(ctx.channel_id, std::slice::from_mut(&mut msg), 0)
                .map(|_| ())
                .map_err(|_| TxFailure::Event(PduErrorEvent::PduErrEvtTxError)),
            Err(_) => Err(TxFailure::Event(PduErrorEvent::PduErrEvtFrameStruct)),
        }
    };
    if result.is_ok() && count_as_bus_activity {
        *ctx.last_bus_activity.lock().await = tokio::time::Instant::now();
    }
    result
}

/// Software ISO-TP TX driver (ADR-046): segments `data`
/// (`[4-byte CAN ID][payload]`) into SingleFrame or
/// FirstFrame/ConsecutiveFrame sequences on a raw CAN channel, honouring the
/// ECU's FlowControl frames (BlockSize / STmin / Wait / Overflow).
///
/// While waiting for FlowControl, RX polling continues through
/// `poll_rx_inner` so concurrent traffic for other CLLs is still fanned out;
/// the FlowControl frame itself is captured via [`FcCapture`] and withheld
/// from delivery.
///
/// `count_as_bus_activity` is threaded straight through to every
/// `write_can_frame` call so a real frame reaching the wire stamps
/// `last_bus_activity` as soon as that write succeeds, regardless of whether
/// a later step in this same send (FlowControl wait, a subsequent
/// ConsecutiveFrame write) goes on to fail -- see `write_can_frame` and
/// `transmit_request`'s doc comments (ADR-083).
#[allow(clippy::too_many_arguments)]
async fn isotp_send(
    cll_handle: u32,
    cop_handle: u32,
    tx_flags: u32,
    data: &[u8],
    opts: &SoftIsoTpTx,
    count_as_bus_activity: bool,
    cancellable: bool,
    ctx: &ChannelPollCtx,
) -> Result<(), TxFailure> {
    // rpc_start_com_primitive validated the shape ([CAN ID][1..=4095 bytes]).
    let can_id_bytes = &data[..4];
    let payload = &data[4..];

    // PR #97 sixth Codex review round: this transfer's own outgoing wire
    // target, computed once and passed to both `dispatch_due_tester_present`
    // call sites below as `exclude_isotp_target` -- see
    // `InFlightIsoTpTarget`'s doc comment for why this (not
    // `resolved.target_can_ids`, the response-side address) is the correct
    // same-CAN-ID collision check for a sibling CLL's tester-present.
    let in_flight_target = Some(InFlightIsoTpTarget {
        can_id: can_id_bytes
            .try_into()
            .expect("rpc_start_com_primitive validated a 4-byte CAN ID prefix"),
        can_29bit: tx_flags & j2534_0404::TX_EXTENDED_ID != 0,
    });

    if payload.len() <= opts.tx_addressing.max_sf_payload() {
        return write_can_frame(
            ctx,
            tx_flags,
            can_id_bytes,
            isotp::single_frame(payload, opts.tx_addressing),
            opts.framing,
            count_as_bus_activity,
        )
        .await;
    }

    write_can_frame(
        ctx,
        tx_flags,
        can_id_bytes,
        isotp::first_frame(payload.len(), payload, opts.tx_addressing),
        opts.framing,
        count_as_bus_activity,
    )
    .await?;

    let chunks: Vec<(u8, &[u8])> = isotp::consecutive_chunks(payload, opts.tx_addressing).collect();
    let poll_interval = Duration::from_millis(POLL_INTERVAL_MS);
    let mut idx = 0;

    while idx < chunks.len() {
        // ── Wait for a FlowControl frame (N_Bs) ─────────────────────────────
        // Consecutive FS_WAIT frames within this FC-wait cycle, counted
        // against `ISOTP_TX_MAX_CONSECUTIVE_RX_WAIT_FRAMES`, an internal
        // defensive bound (ADR-124) -- NOT `CP_CanMaxNumWaitFrames`; see
        // ADR-124, which supersedes ADR-121's original (incorrect-direction)
        // claim that this comparam governs WAIT frames tolerated from a
        // peer while sending. Reset every time a new block starts waiting
        // for FlowControl (i.e. every `while idx < chunks.len()` iteration)
        // -- entering a new block after FS_CONTINUE_TO_SEND IS the reset.
        let mut wait_frames: u32 = 0;
        // `capture` (and its queue) is created ONCE per FC-wait cycle, not
        // per poll or per WAIT re-arm -- it must persist across the `continue`s
        // below so a `PassThruReadMsgs` batch containing multiple matching FC
        // frames (MAX_POLL_MESSAGES = 8; Codex review finding, PR #130) isn't
        // truncated to just the frame consumed by this iteration. Every frame
        // captured is drained in arrival order via `pop_front` before the next
        // poll runs.
        let mut capture = FcCapture {
            fc_can_id: opts.fc_can_id,
            addressing: opts.fc_rx_addressing,
            got: VecDeque::new(),
        };
        let (block_size, st_min) = loop {
            let deadline =
                tokio::time::Instant::now() + Duration::from_millis(opts.n_bs_timeout_ms as u64);
            let fc = loop {
                if let Some(fc) = capture.got.pop_front() {
                    break Some(fc);
                }
                let poll_outcome = poll_rx_inner(ctx, Some(&mut capture)).await;
                if !poll_outcome.is_ok() {
                    return Err(TxFailure::ChannelLost);
                }
                if let Some(fc) = capture.got.pop_front() {
                    break Some(fc);
                }
                // Honour in-flight cancellation, mirroring wait_for_expected_response.
                if cancellable {
                    let was_cancelled = {
                        let mut links = ctx.logical_links.lock().await;
                        links
                            .get_mut(&cll_handle)
                            .map(|l| l.cancelled_cops.remove(&cop_handle))
                            .unwrap_or(false)
                    };
                    if was_cancelled {
                        return Err(TxFailure::Cancelled);
                    }
                }
                // CP_TesterPresentSendType (either value, ADR-095 audit
                // round): this FlowControl (N_Bs) wait can hold this poll
                // task for up to `CP_TesterPresentTime`'s own interval or
                // longer -- fire any due tester-present CLL on this channel
                // on every tick here too, or a sibling CLL (mode 0, now
                // software-dispatched per ADR-093/090) could starve for the
                // whole N_Bs wait. `Box::pin` breaks the async-fn recursion
                // cycle `isotp_send` -> `dispatch_due_tester_present` ->
                // `send_tester_present_once` -> `transmit_request` ->
                // `transmit_request_inner` -> `isotp_send` that Rust cannot
                // size without boxing one edge. Runtime recursion depth is
                // bounded at 1: `send_tester_present_once` always passes
                // `isotp_tx: None` to `transmit_request`/
                // `transmit_request_inner`, so a tester-present send
                // triggered from inside this function can never itself
                // re-enter `isotp_send` (this invariant is load-bearing).
                // `defer_gap_wait = true` (ADR-083): `dispatch_due_tester_present`'s
                // own `wait_for_p3_gap` call runs an unprobed `poll_rx` with
                // no `FcCapture` active -- an FC frame arriving during that
                // unprobed poll would be silently consumed as unattributed
                // background RX and lost, causing a spurious N_Bs timeout on
                // this very transfer. `Some(cll_handle)` excludes this CLL's
                // own mode-0 tester-present from firing while this CLL's own
                // multi-frame ISO-TP send is in flight: a same-CAN-ID
                // single-frame tester-present spliced into the middle of
                // this FirstFrame/ConsecutiveFrame sequence would look like
                // an unexpected SF to the receiving ECU (ISO 15765-2),
                // aborting its in-progress reception. `in_flight_target`
                // (PR #97 sixth Codex review round) closes the same gap for
                // a DIFFERENT sibling CLL that shares this physical channel
                // and happens to be configured with this exact same target
                // CAN ID -- `exclude_cll` alone only excludes by CLL handle,
                // so such a sibling's own tester-present would otherwise
                // still splice onto this wire target undetected.
                // Deadline check moved to BEFORE the dispatch call (ADR-095
                // amendment, PR #97 8th Codex review round): the dispatch
                // above is real hardware write I/O, so checking the deadline
                // only after it let a dispatch run on an already-expired
                // N_Bs wait, and let an FC that arrived on the wire *during*
                // that write sit uncaptured while this loop broke out on
                // timeout without ever polling again. No dispatch now ever
                // runs on an expired wait, and thanks to the capture-check
                // at the top of this loop, every dispatch is followed by one
                // more probed `poll_rx_inner` pass before this check can end
                // the wait -- a deliberate one-poll grace period.
                if tokio::time::Instant::now() >= deadline {
                    break None;
                }
                // ADR-095 amendment (2026-07-18): reap any detached
                // (tier-2) registrant whose CancelComPrimitive/cyclic
                // timeout is otherwise starved for this entire N_Bs wait,
                // same as the tester-present dispatch just below -- reap
                // runs first, matching `run_due_tick_duties`'s own order. No
                // boxing needed here (see
                // `run_detached_registrant_maintenance`'s own doc comment):
                // this helper never touches RX and is not on this function's
                // call graph. Called unconditionally (ADR-101 Decision §E):
                // `reap_expired_cyclic_registrants` now resolves its own
                // soundness from the per-channel drain watermark map, not
                // from any exhaustiveness signal this call site would
                // otherwise need to compute.
                run_detached_registrant_maintenance(ctx).await;
                Box::pin(dispatch_due_tester_present(
                    true,
                    Some(cll_handle),
                    in_flight_target,
                    false,
                    ctx,
                ))
                .await;
                // Deadline-saturating: the dispatch above may itself have
                // overrun the deadline; `saturating_duration_since` (unlike
                // `deadline - now`) never panics/underflows in that case,
                // and yields a zero-length sleep so the next pass's poll
                // runs immediately.
                let now = tokio::time::Instant::now();
                tokio::time::sleep(deadline.saturating_duration_since(now).min(poll_interval))
                    .await;
            };
            match fc {
                // N_Bs expired without a FlowControl frame.
                None => return Err(TxFailure::Event(PduErrorEvent::PduErrEvtRxTimeout)),
                Some((isotp::FS_CONTINUE_TO_SEND, block_size, st_min)) => {
                    // Any frame(s) still queued after this batch's terminal
                    // CTS are a spurious trailing FC N_PDU (ISO 15765-2:
                    // unexpected FC frames are ignored) -- discarded, not
                    // counted or acted on, by dropping them here rather than
                    // carrying them into the next FC-wait cycle.
                    capture.got.clear();
                    break (block_size, st_min);
                }
                // FS_WAIT: bounded by an internal defensive constant (ADR-124), NOT
                // `CP_CanMaxNumWaitFrames` (see ADR-124 -- that comparam governs WAIT
                // frames this service transmits as a receiver, not WAIT frames it
                // tolerates from a peer as a sender, which is this check). Re-arm N_Bs
                // and keep waiting for the next FlowControl only while still within the
                // defensive bound. `wait_frames` resets every time a new block of
                // ConsecutiveFrames starts waiting for FlowControl (i.e. every
                // `while idx < chunks.len()` iteration).
                Some((isotp::FS_WAIT, ..)) => {
                    wait_frames += 1;
                    if wait_frames > ISOTP_TX_MAX_CONSECUTIVE_RX_WAIT_FRAMES {
                        return Err(TxFailure::Event(PduErrorEvent::PduErrEvtProtErr));
                    }
                    continue;
                }
                // FS_OVERFLOW (or reserved): the ECU cannot take the message.
                Some((..)) => return Err(TxFailure::Event(PduErrorEvent::PduErrEvtProtErr)),
            }
        };

        // ── Send one block of ConsecutiveFrames ────────────────────────────
        let st_delay = isotp::st_min_delay(st_min);
        let block_len = if block_size == 0 {
            chunks.len() - idx
        } else {
            block_size as usize
        };
        for _ in 0..block_len {
            let Some(&(sn, chunk)) = chunks.get(idx) else {
                break;
            };
            // CP_TesterPresentSendType (either value, ADR-095 audit round):
            // an STmin-paced ConsecutiveFrame block can span many seconds
            // (e.g. ~15 CFs at STmin=127ms is ~1.9s) -- fire any due
            // tester-present CLL on this channel before each CF here too,
            // same reasoning (boxing, recursion-depth-1 invariant,
            // defer_gap_wait, exclude_cll, in_flight_target) as the FC-wait
            // loop above.
            //
            // ADR-095 amendment (2026-07-18): reap any detached (tier-2)
            // registrant that would otherwise starve for this entire STmin
            // pacing block, same as the tester-present dispatch just below --
            // reap runs first, matching `run_due_tick_duties`'s own order. No
            // boxing needed (see `run_detached_registrant_maintenance`'s own
            // doc comment). Called unconditionally (ADR-101 Decision §E): no
            // RX poll runs anywhere in this loop, so this channel's own drain
            // watermark simply stays as stale as it already was -- the reap
            // itself defers a genuinely expired `CP_CyclicRespTimeout`
            // registrant until RX is next exhaustively drained elsewhere, not
            // lost; the cancellation reap above still runs every iteration
            // unconditionally.
            run_detached_registrant_maintenance(ctx).await;
            Box::pin(dispatch_due_tester_present(
                true,
                Some(cll_handle),
                in_flight_target,
                false,
                ctx,
            ))
            .await;
            if !st_delay.is_zero() {
                tokio::time::sleep(st_delay).await;
            }
            write_can_frame(
                ctx,
                tx_flags,
                can_id_bytes,
                isotp::consecutive_frame(sn, chunk, opts.tx_addressing),
                opts.framing,
                count_as_bus_activity,
            )
            .await?;
            idx += 1;
        }
    }

    Ok(())
}

/// Writes one request: a single `PassThruWriteMsgs` for hardware-transport
/// CLLs, or the software ISO-TP driver when `isotp_tx` is set (ADR-046).
/// Shared by the initial `CoptSendrecv` write, the RC21/RC23 re-request path,
/// and the idle-mode (`CP_TesterPresentSendType = 1`) one-shot send, so all
/// three are segmented/gap-coordinated identically.
///
/// `count_as_bus_activity` gates whether a successful send stamps the shared
/// per-physical-channel `last_bus_activity` clock used by
/// `CP_TesterPresentSendType = 1`'s idle-detection (ADR-083). `false` only at
/// the idle-mode call site (`dispatch_due_tester_present`): an earlier
/// draft stamped unconditionally, which let a mode-1 CLL's own keep-alive
/// permanently resynchronize -- and starve -- a same-interval sibling CLL on
/// the same channel (Codex-review-caught regression, ADR-083 Consequences).
/// Every other caller (`handle_send_recv`'s `CoptSendrecv` write, the
/// RC21/RC23 re-request) passes `true`: those are genuine external bus
/// events, not a subsystem's own scheduled keep-alive.
///
/// For the software-ISO-TP path (`isotp_tx.is_some()`), `count_as_bus_activity`
/// is threaded down into `isotp_send`/`write_can_frame`, which stamp
/// `last_bus_activity` per frame, as soon as each write succeeds -- not just
/// on this function's overall success below. A multi-frame ISO-TP send can
/// write a FirstFrame (and possibly some ConsecutiveFrames) and only later
/// fail, e.g. `PduErrEvtRxTimeout` when no FlowControl arrives; that TX
/// still reached the bus and must still count as activity even though the
/// overall `Result` here is `Err` (a Codex-review-caught gap -- the post-hoc
/// `result.is_ok()` stamp below never saw it). The post-hoc stamp below is
/// still applied for the ISO-TP path too on full success; it is a harmless
/// duplicate to the same instant already stamped by the last successful
/// frame write. For the non-ISO-TP hardware-transport path (`isotp_tx.is_none()`),
/// this post-hoc stamp remains the *only* stamp: `write_messages` there is a
/// single atomic call with no partial-success-then-later-failure scenario.
#[allow(clippy::too_many_arguments)]
async fn transmit_request(
    cll_handle: u32,
    cop_handle: u32,
    protocol_id: u32,
    tx_flags: u32,
    data: &[u8],
    isotp_tx: Option<&SoftIsoTpTx>,
    count_as_bus_activity: bool,
    cancellable: bool,
    ctx: &ChannelPollCtx,
) -> Result<(), TxFailure> {
    let result = transmit_request_inner(
        cll_handle,
        cop_handle,
        protocol_id,
        tx_flags,
        data,
        isotp_tx,
        count_as_bus_activity,
        cancellable,
        ctx,
    )
    .await;
    if result.is_ok() && count_as_bus_activity {
        *ctx.last_bus_activity.lock().await = tokio::time::Instant::now();
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn transmit_request_inner(
    cll_handle: u32,
    cop_handle: u32,
    protocol_id: u32,
    tx_flags: u32,
    data: &[u8],
    isotp_tx: Option<&SoftIsoTpTx>,
    count_as_bus_activity: bool,
    cancellable: bool,
    ctx: &ChannelPollCtx,
) -> Result<(), TxFailure> {
    match isotp_tx {
        Some(opts) => {
            isotp_send(
                cll_handle,
                cop_handle,
                tx_flags,
                data,
                opts,
                count_as_bus_activity,
                cancellable,
                ctx,
            )
            .await
        }
        None => {
            let api = ctx.api.lock().await;
            transmit_request_locked(&api, ctx.channel_id, protocol_id, tx_flags, data)
        }
    }
}

/// Same as [`transmit_request_inner`]'s `isotp_tx.is_none()` (hardware-
/// transport, `PassThruWriteMsgs`) arm, but takes an already-locked
/// `&J2534Api0404` instead of acquiring `ctx.api` itself -- mirrors the
/// `apply_params_to_hardware`/`apply_params_to_hardware_locked` naming
/// pairing (Codex review round 17, P1, PR #101, ADR-192/Phase 7 Stage 7c
/// amendment): lets `handle_send_recv`'s `ParamBinding::Temp` +
/// `isotp_tx.is_none()` bracket hold `ctx.api` continuously across
/// apply -> native write -> revert, closing the same split-bracket
/// clobber race `rpc_primitive.rs::rpc_start_com_primitive`'s round-8 fix
/// already closed for the TP2.0 broadcast-periodic-start path.
///
/// Unlike [`transmit_request`]/[`transmit_request_inner`], this performs no
/// `isotp_tx` dispatch and no `last_bus_activity`/`TxGapState` bookkeeping
/// of its own -- callers that bypass `transmit_request` to hold `ctx.api`
/// across this call (as `handle_send_recv`'s single-guard bracket does) are
/// responsible for that bookkeeping themselves, same as `transmit_request`'s
/// own doc comment already documents for the hardware-transport path (a
/// single atomic call with no partial-success-then-later-failure scenario,
/// so the post-hoc stamp is the only stamp needed).
fn transmit_request_locked(
    api: &J2534Api0404,
    channel_id: ChannelId,
    protocol_id: u32,
    tx_flags: u32,
    data: &[u8],
) -> Result<(), TxFailure> {
    match j2534_0404::PassThruMessage::new(protocol_id, 0, tx_flags, 0, 0, data) {
        Ok(mut msg) => api
            .write_messages(channel_id, std::slice::from_mut(&mut msg), 0)
            .map(|_| ())
            .map_err(|_| TxFailure::Event(PduErrorEvent::PduErrEvtTxError)),
        Err(_) => Err(TxFailure::Event(PduErrorEvent::PduErrEvtFrameStruct)),
    }
}

/// A follow-up send cycle of a cyclic `CoptSendrecv` (ADR-053), produced by
/// [`handle_send_recv`] when send cycles remain after the current one: the
/// re-enqueued `TxItem::SendRecv` plus how it should be scheduled.
struct CycleContinuation {
    item: TxItem,
    /// When the next cycle becomes due (cycle start + `Time`); already due
    /// for `lower_priority` items.
    due: tokio::time::Instant,
    /// `Time == 0`: the follow-up cycle goes to the back of the TX queue —
    /// lower priority than every other queued ComPrimitive — instead of
    /// being scheduled by time.
    lower_priority: bool,
}

/// Outcome of [`wait_for_p3_gap`].
enum P3GapOutcome {
    /// No wait was needed, or the wait elapsed without interruption; the
    /// caller should proceed to transmit.
    Ready,
    Cancelled,
    HardError,
    /// `defer_if_blocked = true` and the gap would have required an actual
    /// wait (`now < deadline`) -- returned immediately, before entering the
    /// sleep/`poll_rx` loop, so no RX drain happens (ADR-083). The caller
    /// should treat this the same as `Cancelled`/`HardError` (skip this
    /// send for the current pass) and let the next evaluation re-check the
    /// gap.
    Deferred,
}

/// Waits, if necessary, for `CP_P3Func`/`CP_P3Phys`'s minimum inter-request
/// gap to elapse before a CAN-family send (ADR-060).
///
/// A no-op (`Ready` immediately) when `can_functional` is `None`
/// (non-CAN-family protocol, or addressing could not be resolved) or when
/// the relevant ComParam's configured gap is `0`. Otherwise, per
/// `last_func_tx`/`last_phys_tx` (the shared channel's last functionally-
/// or physically-addressed send, selected by `can_functional`), the gap is
/// enforced when *either* that previous send had `NumReceiveCycles == 0`
/// (recorded on it) *or* this upcoming send does (`num_receive_cycles`
/// here) -- the condition that requires no response on either side is what
/// leaves nothing else to naturally pace the bus.
///
/// Polls RX and, when `cancellable` is `true`, honours cancellation in
/// `POLL_INTERVAL_MS`-sized chunks while waiting, mirroring `handle_delay`
/// (ADR-003), so the wait never starves other CLLs sharing this physical
/// channel.
///
/// `cancellable = false` (ADR-083/ADR-085) skips the `cancelled_cops` check
/// entirely -- used by call sites reached only after the point of no return
/// where a `CancelComPrimitive` must no longer be able to abort the COP. The
/// timing delay and the `HardError` (lost comms) check still apply either
/// way -- only the cancellation short-circuit is skipped. `CoptSendrecv`'s
/// call site (`handle_send_recv`'s pre-write gate) remains `cancellable =
/// true`, unchanged.
///
/// `defer_if_blocked = true` (ADR-083) makes a gap that would actually
/// require waiting return `P3GapOutcome::Deferred` immediately, without
/// entering the sleep/`poll_rx` loop below -- used only by
/// `dispatch_due_tester_present`'s call site when it is itself being
/// driven from inside `wait_for_expected_response`'s poll loop, where this
/// function's unprobed `poll_rx` could otherwise drain the very ECU response
/// the active `CoptSendrecv` is waiting to match. An already-clear gap
/// (deadline already in the past) is unaffected and still returns `Ready`
/// with zero `poll_rx` calls either way. The three `cancellable = false` call
/// sites -- `handle_start_comm`'s unified arm-time first-send gate,
/// `handle_update_param`'s tester-present re-arm gate, and
/// `handle_stop_comm`'s terminal-send gate (ADR-085) -- always pass
/// `defer_if_blocked = false` -- `Deferred` is unreachable at any of them.
async fn wait_for_p3_gap(
    cop_handle: u32,
    cll_handle: u32,
    can_functional: Option<bool>,
    num_receive_cycles: i32,
    cancellable: bool,
    defer_if_blocked: bool,
    ctx: &ChannelPollCtx,
) -> P3GapOutcome {
    let Some(functional) = can_functional else {
        return P3GapOutcome::Ready;
    };

    let gap_ms = {
        let links = ctx.logical_links.lock().await;
        links.get(&cll_handle).map_or(0, |l| {
            if functional {
                l.active.p3_func_gap_ms()
            } else {
                l.active.p3_phys_gap_ms()
            }
        })
    };
    if gap_ms == 0 {
        return P3GapOutcome::Ready;
    }
    let last_tx = if functional {
        &ctx.last_func_tx
    } else {
        &ctx.last_phys_tx
    };
    // CP_P3Phys triggers only on the *previous* physical send requiring no
    // response. CP_P3Func also triggers when the *upcoming* functional send
    // itself requires no response -- there is no symmetric "upcoming" clause
    // in CP_P3Phys's own definition (ADR-060).
    let this_requires_no_response = functional && num_receive_cycles == 0;

    let deadline = {
        let last = last_tx.lock().await;
        match *last {
            Some(prev) if prev.no_response_required || this_requires_no_response => {
                Some(prev.at + Duration::from_millis(gap_ms as u64))
            }
            _ => None,
        }
    };
    let Some(deadline) = deadline else {
        return P3GapOutcome::Ready;
    };

    if defer_if_blocked && tokio::time::Instant::now() < deadline {
        return P3GapOutcome::Deferred;
    }

    let poll_interval = Duration::from_millis(POLL_INTERVAL_MS);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return P3GapOutcome::Ready;
        }
        let remaining = deadline - now;
        tokio::time::sleep(remaining.min(poll_interval)).await;
        if !poll_rx(ctx).await.is_ok() {
            return P3GapOutcome::HardError;
        }
        // Deliberately no `dispatch_due_tester_present(..).await` here
        // (ADR-083/ADR-095 audit round). Three independent reasons, not just
        // "this wait is short":
        // 1. This wait has a fixed, one-shot deadline anchored at the last
        //    TX instant (`deadline` above) -- it never self-renews on each
        //    loop pass the way e.g. `wait_for_expected_response`'s response
        //    window can. The worst-case tester-present delay from omitting
        //    the dispatch here is therefore bounded to exactly one
        //    configured `CP_P3Func`/`CP_P3Phys` gap, not an unbounded hold.
        // 2. For a tester-present send in the SAME addressing bucket as the
        //    send this gap-wait is guarding, dispatching from here would
        //    gain nothing anyway: that tester-present send would immediately
        //    re-enter this very same gap wait (same `last_func_tx`/
        //    `last_phys_tx` bucket, same deadline), so it could not fire any
        //    sooner than waiting for this wait to finish and calling
        //    `dispatch_due_tester_present` at the caller's own next tick.
        // 3. Calling `dispatch_due_tester_present` from here would also
        //    close a `dispatch_due_tester_present -> wait_for_p3_gap ->
        //    dispatch_due_tester_present` re-entrancy cycle -- unlike Fix A's
        //    `isotp_send` call sites, boxing alone would not be enough here,
        //    since the *same* CLL's own gap-wait would need a re-entrancy
        //    guard (not just a broken-cycle recursion depth of 1) to avoid
        //    recursing on itself per point 2 above. Not worth adding for no
        //    proportionate benefit given (1) and (2).
        if !cancellable {
            // Past the point of no return (ADR-083) -- keep waiting out the
            // timing delay without consulting `cancelled_cops`. Any
            // cancellation recorded here is cleaned up by `dispatch_tx_item`
            // when the COP completes normally, as with any other stale
            // `cancelled_cops` entry.
            continue;
        }
        let was_cancelled = {
            let mut links = ctx.logical_links.lock().await;
            links
                .get_mut(&cll_handle)
                .map(|l| l.cancelled_cops.remove(&cop_handle))
                .unwrap_or(false)
        };
        if was_cancelled {
            return P3GapOutcome::Cancelled;
        }
    }
}

/// Fires each due tester-present CLL (either `CP_TesterPresentSendType`
/// value) on this shared channel (ADR-083; both native
/// `PassThruStartPeriodicMsg` mode-0 dispatch and its native periodic-start
/// gate are gone as of this diff -- mode 0 is now fired from here too),
/// called once per poll-loop tick after the ordinary RX poll.
///
/// A CLL is due when its `tester_present_state` is `Armed { interval,
/// armed_at, last_fired, resolved, .. }`, `comm_started` is `true`, and the
/// mode-appropriate due-check formula is satisfied: for mode 1
/// (`resolved.send_type == 1`, idle-triggered), elapsed time since
/// `last_bus_activity.max(last_fired.unwrap_or(armed_at))` is at least
/// `interval` -- the later of "the shared channel's last real bus event" and
/// "when this CLL itself last fired (or armed, before its first fire)", so
/// one CLL arming, or firing its own keep-alive, never perturbs a sibling
/// CLL's already-counting idle window (ADR-083). For mode 0
/// (`send_type == 0`, periodic), elapsed time since
/// `last_fired.unwrap_or(armed_at)` alone is at least `interval` --
/// `last_bus_activity` is never consulted, so mode 0 fires strictly every
/// `interval` since its own last fire/arm, unperturbed by any other bus
/// traffic (this diff: mode 0 was previously hardware-autonomous and had no
/// software due-check at all). Candidates are snapshotted under
/// `logical_links` and the lock is dropped before any `.await` on
/// `api`/`transmit_request` (mirrors every other TxItem handler's lock
/// discipline).
///
/// Each due send reuses `framed_data`, the wire payload `handle_start_comm`
/// already SF-framed once at arm time (software ISO-TP SingleFrame wrapping
/// when applicable) -- no per-tick re-framing -- and dispatches it through
/// `transmit_request` -- NOT enqueued as a `TxItem`: it has no `cop_handle`,
/// emits no COP status event, and must not interact with per-CLL FIFO
/// ordering, `tx_held`, or `tx_suspended` (ADR-081). Going through
/// `transmit_request` means the send participates in `wait_for_p3_gap` (a
/// synthetic `cop_handle = 0` -- reserved, never allocated to a real COP,
/// see `next_primitive_handle`) and updates `last_func_tx`/`last_phys_tx`,
/// but is passed `count_as_bus_activity = false`: this CLL's own send must
/// never re-stamp the shared `last_bus_activity` clock (that re-stamp used
/// to permanently resynchronize, and starve, a same-interval sibling CLL --
/// a Codex-review-caught regression, see ADR-083 Consequences). Instead, a
/// successful send stamps this CLL's own `last_fired`, below.
///
/// `defer_gap_wait` is threaded straight through to this send's
/// `wait_for_p3_gap` call as `defer_if_blocked` (ADR-083): `true` only at
/// the `wait_for_expected_response` call site, where this function's own
/// unprobed `poll_rx` inside a `wait_for_p3_gap` wait could otherwise drain
/// the very ECU response an active `CoptSendrecv` is waiting to match;
/// `false` at the outer-select and `handle_delay` call sites, which have no
/// COP-specific match state to protect and keep waiting out the gap as
/// before. A deferred item is simply left `Armed` for the next call to
/// re-evaluate.
///
/// `exclude_cll` (ADR-095 audit round) excludes one specific CLL from the
/// due-snapshot filter entirely -- used only by `isotp_send`'s two in-flight
/// call sites (`Some(cll_handle)`, the CLL whose own multi-frame ISO-TP send
/// is currently on the wire); every other call site passes `None`. A
/// same-CAN-ID single-frame tester-present spliced into the middle of that
/// CLL's own FirstFrame/ConsecutiveFrame sequence would look like an
/// unexpected SF to the receiving ECU mid-transfer (ISO 15765-2), aborting
/// the ECU's in-progress reception and corrupting the very transfer this
/// dispatch call is running inside of. Mode 1 already avoids this via its
/// own `last_bus_activity`-driven self-exclusion from firing during its own
/// recent activity; mode 0 has no such protection, so it must be excluded
/// explicitly here instead.
///
/// Because the send happens after the snapshot lock is dropped, and
/// `wait_for_p3_gap` itself awaits, the CLL is re-validated -- still exists,
/// `tester_present_state` still `Armed { .. }` with the same `armed_at` (a
/// re-arm with a different `armed_at`/interval is a legitimate reason to
/// skip this particular fire and let the next tick re-evaluate, not an
/// error) -- immediately before the `transmit_request` call. A concurrent
/// `DisconnectComLogicalLink`/`DestroyComLogicalLink`/`CoptStopcomm` landing
/// between the snapshot and the send is therefore never raced past: the send
/// is silently skipped (no error event -- the CLL just isn't in a state that
/// wants this send anymore).
///
/// The write itself failing emits `PduErrEvtTesterPresentError` but leaves
/// `tester_present_state` as `Armed` unchanged -- retried on the next
/// elapsed window. A cancelled P3-gap wait (never actually reachable here --
/// the synthetic `cop_handle = 0` is never inserted into `cancelled_cops`)
/// or a hard channel error (`handle_channel_hard_error` already emitted the
/// loss-of-comms event sequence) are simply skipped without an extra event,
/// matching `wait_for_p3_gap`'s other call sites.
struct DueTesterPresent {
    cll_handle: u32,
    protocol_id: u32,
    tx_flags: u32,
    can_functional: Option<bool>,
    data: Vec<u8>,
    armed_at: tokio::time::Instant,
    /// `CP_TesterPresentReqRsp == 1` at the due-snapshot critical section
    /// (ADR-088 P3-classification fix), used as this due send's
    /// `wait_for_p3_gap` `num_receive_cycles` argument -- captured here
    /// (rather than re-read after the P3 wait) since `wait_for_p3_gap`'s own
    /// call must happen before the `still_due` recheck can run. Re-read
    /// fresh a second time in the `still_due` pre-send critical section for
    /// the send's own `TxGapState` stamp, matching the existing
    /// `connect_generation`/`p2_max_ms` two-stage capture pattern in this
    /// function.
    expects_response: bool,
}

/// Synthetic cop_handle = 0 (reserved, never allocated to a real COP -- see
/// `J2534Service::next_primitive_handle`): every tester-present due-send has
/// no COP of its own, but `wait_for_p3_gap` needs a handle to check
/// `cancelled_cops` against.
const NO_COP: u32 = 0;

/// Cheap, `Copy`-able identity token for a CLL's `tester_present_state`, used
/// for pre-/post-`.await` re-validation ("did this CLL's tester-present state
/// change out from under me while I was awaiting something") the same way
/// `dispatch_due_tester_present` already does via bare `armed_at`
/// equality and `handle_start_comm` already does via `channel_id` equality
/// (ADR-084). Keyed on `armed_at` alone -- unique across arm generations for
/// both `CP_TesterPresentSendType` values, so no separate variant is needed
/// per mode.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TesterPresentToken {
    None,
    Armed(tokio::time::Instant),
    /// Keyed on `cleared_at` rather than collapsing to a single value, same
    /// reasoning as `Armed`'s `armed_at` keying: a post-`.await` staleness
    /// recheck (`handle_update_param`'s re-arm gate) must be able to tell
    /// "still the same clear I snapshotted" from "got re-armed and then
    /// re-cleared again during my await" -- a token that couldn't
    /// distinguish two different `Cleared` instances would let a stale
    /// recheck pass incorrectly.
    Cleared(tokio::time::Instant),
    /// Keyed on `disarmed_at`, same reasoning as `Cleared`'s own
    /// `cleared_at` keying (ADR-137 second Codex-review fix): distinguishes
    /// two different `Disarmed` instances the same way `Cleared`/`Armed`
    /// already do, for a post-`.await` staleness re-check.
    Disarmed(tokio::time::Instant),
}

fn tester_present_state_token(state: &TesterPresentState) -> TesterPresentToken {
    match state {
        TesterPresentState::None => TesterPresentToken::None,
        TesterPresentState::Armed { armed_at, .. } => TesterPresentToken::Armed(*armed_at),
        TesterPresentState::Cleared { cleared_at, .. } => TesterPresentToken::Cleared(*cleared_at),
        TesterPresentState::Disarmed { disarmed_at, .. } => {
            TesterPresentToken::Disarmed(*disarmed_at)
        }
    }
}

/// Sends one tester-present frame (either `CP_TesterPresentSendType` value)
/// via `transmit_request` (`count_as_bus_activity = false` -- a tester-present
/// CLL's own send must never resynchronize a sibling CLL's idle window,
/// ADR-083). On success, stamps the `CP_P3Func`/`CP_P3Phys` `TxGapState`
/// bucket -- `no_response_required = !expects_response` (`CP_TesterPresentReqRsp`
/// classification fix folded into this diff; previously hardcoded to `true`
/// regardless of whether this tester-present expects a response) -- and
/// returns the post-send instant. On failure, emits
/// `PduErrEvtTesterPresentError` and returns the failure instant as `Err` so
/// the caller can still arm a full-interval backoff (ADR-083: a
/// persistently-failing send must not retry every poll tick).
///
/// `generation` (ADR-086 round 11, R2): `Some(connect_generation)` at every
/// call site -- `transmit_request`'s own I/O can span a disconnect+reconnect
/// completing mid-call, so the `Err` arm rechecks `still_on_this_channel`
/// before emitting its event, skipping it when stale. This includes
/// `dispatch_due_tester_present`'s own call: an earlier version of this
/// fix passed `None` there on the theory that its `armed_at`-based staleness
/// check (run immediately before the call) already made a post-call recheck
/// redundant -- that reasoning only covers entry into the call, not
/// `transmit_request`'s own `.await` racing a reconnect mid-call, so it was
/// corrected (edge-case-hunter finding, caught before this round was
/// committed) to thread the live `connect_generation`, captured in that same
/// pre-call critical section, through as `Some(..)` too.
#[allow(clippy::too_many_arguments)]
async fn send_tester_present_once(
    cll_handle: u32,
    protocol_id: u32,
    tx_flags: u32,
    can_functional: Option<bool>,
    data: &[u8],
    expects_response: bool,
    generation: Option<u64>,
    ctx: &ChannelPollCtx,
    // ISO 22900-2 §9.4.7 c) / §9.6.2: the COP whose own synchronous
    // execution triggered this send (`handle_start_comm`'s initial arm,
    // `handle_update_param`'s re-arm), paired with its already-captured
    // `cop_tag` (`ErrorCop`, ADR-205 Decision item 1 -- forwarded unchanged
    // into this function's own `send_error_event` call below, never
    // re-resolved here), if any. `None` from `dispatch_due_tester_present`'s
    // periodic/idle-triggered dispatch -- that background tick is not driven
    // by any currently-executing COP (the COP that armed tester-present has
    // long since finished), so a send failure there is correctly
    // module/CLL-scoped, not COP-scoped.
    cop: Option<ErrorCop>,
) -> Result<tokio::time::Instant, tokio::time::Instant> {
    match transmit_request(
        cll_handle,
        NO_COP,
        protocol_id,
        tx_flags,
        data,
        None,
        false,
        true,
        ctx,
    )
    .await
    {
        Ok(()) => {
            if let Some(functional) = can_functional {
                let bucket = if functional {
                    &ctx.last_func_tx
                } else {
                    &ctx.last_phys_tx
                };
                *bucket.lock().await = Some(TxGapState {
                    at: tokio::time::Instant::now(),
                    no_response_required: !expects_response,
                });
            }
            Ok(tokio::time::Instant::now())
        }
        Err(_) => {
            let still_on_this_channel = match generation {
                Some(connect_generation) => {
                    let links = ctx.logical_links.lock().await;
                    links.get(&cll_handle).is_some_and(|l| {
                        l.channel_id == Some(ctx.channel_id)
                            && l.connect_generation == connect_generation
                    })
                }
                None => true,
            };
            if still_on_this_channel {
                send_error_event(
                    &ctx.subscriptions,
                    &ctx.logical_links,
                    cll_handle,
                    PduErrorEvent::PduErrEvtTesterPresentError,
                    cop,
                )
                .await;
            }
            Err(tokio::time::Instant::now())
        }
    }
}

/// Mode-aware due-check reference instant (ADR-083, extended to mode 0 by
/// this diff): mode 1 (`send_type == 1`, idle-triggered) is deferred by any
/// bus activity newer than its own anchor; mode 0 (`send_type == 0`,
/// periodic) fires strictly since its own last fire/arm and never consults
/// `last_bus_activity`. `anchor = last_fired.unwrap_or(armed_at)` either way.
fn tester_present_due_reference(
    send_type: u32,
    armed_at: tokio::time::Instant,
    last_fired: Option<tokio::time::Instant>,
    last_activity: tokio::time::Instant,
) -> tokio::time::Instant {
    let anchor = last_fired.unwrap_or(armed_at);
    if send_type == 1 {
        last_activity.max(anchor)
    } else {
        anchor
    }
}

/// A frozen ISO-TP transfer's own outgoing wire target (PR #97 sixth Codex
/// review round): the CAN ID bytes a caller's own in-flight multi-frame
/// `isotp_send` is currently transmitting on, and whether that CAN ID is
/// 29-bit. Passed to `dispatch_due_tester_present` as
/// `exclude_isotp_target` to additionally suppress any *sibling* CLL's
/// tester-present (not just the in-flight CLL's own, which `exclude_cll`
/// already handles) whose own transmitted wire bytes would collide with this
/// exact CAN ID -- splicing a SingleFrame into the middle of this
/// FirstFrame/ConsecutiveFrame sequence, the same failure mode ADR-095
/// documents for the same-CLL case, but here for a different CLL that
/// happens to share both this physical channel and this exact target ECU
/// address.
#[derive(Debug, Clone, Copy)]
struct InFlightIsoTpTarget {
    /// The leading 4 bytes of the transmitted frame -- the CAN ID the
    /// in-flight transfer is writing to, in the same big-endian byte layout
    /// as `TesterPresentState::Armed::framed_data[..4]`.
    can_id: [u8; 4],
    /// Whether `can_id` is a 29-bit identifier (`TX_EXTENDED_ID`), so an
    /// 11-bit `0x7E0` does not alias a 29-bit `0x000007E0`.
    can_29bit: bool,
}

/// `true` when `framed_data`'s own wire CAN ID (its leading 4 bytes) and
/// 29-bit-ness collide with `target`'s. Deliberately does not compare
/// `framed_data[4]` (the ISO 15765-2 extended-addressing extension byte,
/// when present): a same-CAN-ID sibling with a different extension byte is
/// merely over-suppressed for one dispatch opportunity rather than
/// permanently starved, an accepted residual (PR #97 sixth Codex review
/// round). Defensively `false` (no exclusion) when `framed_data` is shorter
/// than 4 bytes -- not reachable for any CAN-family CLL, but this must not
/// panic if it ever were.
fn isotp_target_collision(
    framed_data: &[u8],
    tx_flags: u32,
    target: Option<InFlightIsoTpTarget>,
) -> bool {
    let Some(target) = target else {
        return false;
    };
    framed_data.len() >= 4
        && framed_data[..4] == target.can_id[..]
        && (tx_flags & j2534_0404::TX_EXTENDED_ID != 0) == target.can_29bit
}

/// Parameters, in order: `defer_gap_wait`, `exclude_cll`,
/// `exclude_isotp_target`, `force`, `ctx`.
///
/// `exclude_isotp_target` (PR #97 sixth Codex review round): see
/// [`InFlightIsoTpTarget`]. Orthogonal to `exclude_cll` -- both exclusions
/// are applied independently, neither replaces the other.
///
/// `force` (pre-init top-up fix, PR #97 fifth Codex review round): when
/// `true`, bypasses ONLY the due-check interval predicate below
/// (`now.duration_since(tester_present_due_reference(..)) >= *interval`) for
/// CLLs whose `resolved.send_type == 0` (mode 0 / periodic). Every other
/// guard -- `channel_id == Some(ctx.channel_id)`, `comm_started`,
/// `TesterPresentState::Armed{..}`, `exclude_cll != Some(cll_handle)` -- stays
/// exactly as-is. Mode 1 (`send_type == 1`, idle-triggered) is deliberately
/// NEVER force-fired, even when `force == true`: it is already correctly
/// deferred by `run_protocol_init`'s own `last_bus_activity` stamp (ADR-083),
/// so bypassing its due-check here would double up on that, not fix a gap.
/// A forced send still goes through the same `still_due` recheck /
/// `send_tester_present_once` / `last_fired`-stamp path as every other due
/// send below, so the sibling's own subsequent scheduling self-corrects from
/// the forced-send instant. `handle_start_comm` is the only call site that
/// passes `force: true` today, immediately before its own `run_protocol_init`
/// call, to bound a shared K-line channel's other mode-0 CLLs' worst-case
/// keepalive gap down from `interval + init_duration` to
/// `max(interval, init_duration)`. Every other call site passes `force:
/// false` (unchanged, normal due-check behaviour). The pre-init top-up call
/// specifically also passes `exclude_isotp_target: None` -- no transfer is
/// in flight at that call site, so there is nothing to protect.
/// `dispatch_due_tester_present`'s pre-send critical-section snapshot of
/// everything needed to gate/size a `DiscardWindow`, threaded across the
/// send's own `.await` instead of being read live afterward (ADR-088 second
/// amendment) -- a plain tuple here trips clippy's `type_complexity` lint.
struct StillDueSnapshot {
    connect_generation: u64,
    p2_max_ms: u32,
    expects_response: bool,
    exp_pos_resp: Vec<u8>,
    exp_neg_resp: Vec<u8>,
}

async fn dispatch_due_tester_present(
    defer_gap_wait: bool,
    exclude_cll: Option<u32>,
    exclude_isotp_target: Option<InFlightIsoTpTarget>,
    force: bool,
    ctx: &ChannelPollCtx,
) {
    let now = tokio::time::Instant::now();
    let last_activity = *ctx.last_bus_activity.lock().await;

    let due: Vec<DueTesterPresent> = {
        let links = ctx.logical_links.lock().await;
        links
            .iter()
            .filter(|&(&cll_handle, link)| {
                link.channel_id == Some(ctx.channel_id)
                    && link.comm_started
                    && exclude_cll != Some(cll_handle)
                    // ADR-180 Decision 16 (design-advisor consult, PR #72
                    // round 12, Finding 3): a negotiation-enabled SAE J1939
                    // CLL with no CURRENTLY claimed source address (before
                    // its first successful claim, or during a spontaneous-
                    // loss-to-reclaim window) must not have a tester-present
                    // frame dispatched under a source address it does not
                    // own -- see `j1939_negotiated_unclaimed`'s own doc
                    // comment. This single read-side filter, at this one
                    // choke point, is self-resuming: once
                    // `j1939_claimed_address` becomes `Some` again, this
                    // filter passes and Decision 6's existing framed_data
                    // recompose has already fixed this CLL's armed frame.
                    && !j1939_negotiated_unclaimed(link)
            })
            .filter_map(|(&cll_handle, link)| match &link.tester_present_state {
                TesterPresentState::Armed {
                    resolved,
                    interval,
                    armed_at,
                    last_fired,
                    framed_data,
                    ..
                } if !isotp_target_collision(
                    framed_data,
                    resolved.tx_flags,
                    exclude_isotp_target,
                ) && ((force && resolved.send_type == 0)
                    || now.duration_since(tester_present_due_reference(
                        resolved.send_type,
                        *armed_at,
                        *last_fired,
                        last_activity,
                    )) >= *interval) =>
                {
                    Some(DueTesterPresent {
                        cll_handle,
                        protocol_id: link.hw_protocol_id,
                        tx_flags: resolved.tx_flags,
                        can_functional: resolved.can_functional,
                        data: framed_data.clone(),
                        armed_at: *armed_at,
                        expects_response: link.active.tester_present_req_rsp() == 1,
                    })
                }
                _ => None,
            })
            .collect()
    };

    for DueTesterPresent {
        cll_handle,
        protocol_id,
        tx_flags,
        can_functional,
        data,
        armed_at: snapshotted_armed_at,
        expects_response: snapshotted_expects_response,
    } in due
    {
        match wait_for_p3_gap(
            NO_COP,
            cll_handle,
            can_functional,
            if snapshotted_expects_response { 1 } else { 0 },
            true,
            defer_gap_wait,
            ctx,
        )
        .await
        {
            P3GapOutcome::Ready => {}
            P3GapOutcome::Cancelled | P3GapOutcome::HardError => continue,
            // A deferred gap wait (ADR-083, defer_gap_wait = true only from
            // wait_for_expected_response) leaves this CLL Armed; the next
            // dispatch_due_tester_present pass -- imminent, since the
            // gap deadline is already close -- re-evaluates it without ever
            // draining RX via this call's unprobed poll_rx.
            P3GapOutcome::Deferred => continue,
        }

        // Re-validate post-snapshot, post-`.await` (Codex-review fix,
        // ADR-083): a Disconnect/Destroy/CoptStopcomm landing between the
        // snapshot above and here must not result in a send for a torn-down
        // CLL, nor a spurious error event for one. Re-reads `last_bus_activity`
        // fresh here rather than trusting the pre-loop snapshot (a second
        // Codex-review fix): genuine external bus activity (RX, another CLL's
        // CoptSendrecv) that lands in this window must still be able to defer
        // a mode-1 send (irrelevant to mode 0, which never reads it). This
        // CLL's own `last_fired` is read fresh too, from live state rather
        // than the pre-wait snapshot, for the same reason.
        //
        // Also requires `channel_id == Some(ctx.channel_id)` and the same
        // `armed_at` as the snapshot (a third Codex-review fix):
        // `DisconnectComLogicalLink` clears `channel_id` but leaves the CLL's
        // entry in `logical_links` (unlike `DestroyComLogicalLink`, which
        // removes it), so a bare existence/state-variant check is not enough
        // -- a disconnect-then-reconnect-and-rearm on a *different* physical
        // channel (or even the same one, on a fresh `armed_at`) between the
        // snapshot and here would otherwise pass this check and send the
        // stale pre-wait `protocol_id`/`tx_flags`/`data` on the wrong channel
        // or for the wrong arm generation. Comparing `armed_at` pins this
        // send to the exact arm that was snapshotted; any re-arm (same
        // channel or not) is treated the same as "not due yet" and left for
        // that arm's own next-tick evaluation. Note this identity check
        // compares the snapshotted `armed_at` against the live `armed_at` --
        // not `last_fired` -- since it validates "same arm generation", which
        // is orthogonal to the due-check anchor below.
        // ADR-086 round 11 (edge-case-hunter finding on the round-11 diff,
        // pre-commit): this block used to yield a plain `bool` and this call
        // site passed `None` for `send_tester_present_once`'s
        // `generation` parameter, on the theory that the `armed_at`-identity
        // check right above the call already made a post-call recheck
        // redundant. That reasoning only covers entry into the call --
        // `transmit_request`'s own `ctx.api.lock().await` inside
        // `send_tester_present_once` is a real hardware `.await` a
        // disconnect+reconnect can complete during, same as every other
        // R1-R3 site in this round; a `None` generation short-circuits the
        // `Err` arm's own recheck to always-true, so a stale send's failure
        // would unconditionally fire `PduErrEvtTesterPresentError` on the
        // reconnected session. Fixed by threading the live
        // `connect_generation`, captured in this same critical section,
        // through as `Some(..)` instead -- the exact mechanism the other two
        // `send_tester_present_once` call sites already use.
        // `p2_max_ms`/`expects_response`/`exp_pos_resp`/`exp_neg_resp` are
        // captured here, alongside `connect_generation`, in this same
        // pre-send critical section (ADR-088 amendment, extended to
        // `exp_pos_resp`/`exp_neg_resp` by the ADR-088 second amendment) --
        // matching `handle_start_comm`'s and `handle_update_param`'s own call
        // sites, both of which read `CP_P2Max`/`CP_TesterPresentReqRsp`/
        // `CP_TesterPresentExpPosResp`/`ExpNegResp` from a pre-send bound
        // snapshot rather than a live re-read after the send's `.await`.
        // Reading them post-send instead (an earlier version of this site
        // did for `p2_max_ms`) would let a `CoptUpdateparam` racing the
        // send's own `ctx.api.lock().await` change the discard window's
        // *duration*/gap classification/patterns to a value not actually in
        // effect at the send instant -- the same
        // never-live-re-read-Active-for-a-response-window discipline
        // `handle_send_recv`'s own snapshot binding already establishes
        // (ADR-053/067). `expects_response` is deliberately re-read fresh
        // here rather than reusing the due-snapshot's own value above: the
        // due-snapshot's copy is only used for the `wait_for_p3_gap` call
        // that happens BEFORE this recheck, and a `CoptUpdateparam`
        // promoting `CP_TesterPresentReqRsp` could land in that gap.
        //
        // `force` (pre-init top-up fix): this recheck re-applies the same
        // due-check interval predicate the outer snapshot above used, so it
        // must bypass it the same way (mode 0 only) -- otherwise a
        // forced-due item from the snapshot would fail this recheck (the
        // real interval has not actually elapsed) and be silently dropped
        // via `continue` below, defeating the whole top-up.
        let still_due: Option<StillDueSnapshot> = {
            let fresh_last_activity = *ctx.last_bus_activity.lock().await;
            let links = ctx.logical_links.lock().await;
            links.get(&cll_handle).and_then(|l| {
                if l.channel_id != Some(ctx.channel_id) {
                    return None;
                }
                match &l.tester_present_state {
                    TesterPresentState::Armed {
                        resolved,
                        interval,
                        armed_at,
                        last_fired,
                        ..
                    } => {
                        let due = *armed_at == snapshotted_armed_at
                            && ((force && resolved.send_type == 0)
                                || tokio::time::Instant::now().duration_since(
                                    tester_present_due_reference(
                                        resolved.send_type,
                                        *armed_at,
                                        *last_fired,
                                        fresh_last_activity,
                                    ),
                                ) >= *interval);
                        due.then_some(StillDueSnapshot {
                            connect_generation: l.connect_generation,
                            p2_max_ms: l.active.p2_max_timeout_ms(),
                            expects_response: l.active.tester_present_req_rsp() == 1,
                            exp_pos_resp: l.active.tester_present_exp_pos_resp().to_vec(),
                            exp_neg_resp: l.active.tester_present_exp_neg_resp().to_vec(),
                        })
                    }
                    TesterPresentState::None
                    | TesterPresentState::Cleared { .. }
                    | TesterPresentState::Disarmed { .. } => None,
                }
            })
        };
        let Some(StillDueSnapshot {
            connect_generation,
            p2_max_ms,
            expects_response,
            exp_pos_resp,
            exp_neg_resp,
        }) = still_due
        else {
            continue;
        };

        // `send_tester_present_once` performs the send, the
        // `TxGapState` stamp on success, and the `PduErrEvtTesterPresentError`
        // event on failure. Both `Ok`/`Err` arms return a post-send instant;
        // `last_fired` is stamped from it below under the same arm-identity
        // guard as the `still_due` re-check, regardless of outcome
        // (Codex-review fix, PR #82, ninth round, ADR-083): a failed send
        // must back off for a full interval too, the same as a successful
        // one -- otherwise a persistently-failing CLL stays "due" and is
        // retried on every poll tick (~10ms) instead of waiting out
        // CP_TesterPresentTime again, flooding PassThruWriteMsgs calls and
        // PduErrEvtTesterPresentError events.
        let (fired_at, send_ok) = match send_tester_present_once(
            cll_handle,
            protocol_id,
            tx_flags,
            can_functional,
            &data,
            expects_response,
            Some(connect_generation),
            ctx,
            // Background periodic/idle-triggered dispatch: no COP is
            // currently executing (the COP that armed tester-present has
            // long since finished), so a send failure here is
            // module/CLL-scoped, not COP-scoped (ISO 22900-2 §9.6.2).
            None,
        )
        .await
        {
            Ok(t) => (t, true),
            Err(t) => (t, false),
        };

        let mut links = ctx.logical_links.lock().await;
        if let Some(l) = links.get_mut(&cll_handle)
            && l.channel_id == Some(ctx.channel_id)
        {
            // ADR-137 fourth Codex-review fix (round-4 restructure): the new
            // window is computed from the SAME arm-identity-guarded `Armed`
            // match that stamps `last_fired`, but pushed into
            // `l.open_tp_discards` -- a per-CLL list independent of
            // `tester_present_state` -- rather than overwriting a single
            // per-variant slot. This closes two latent bugs the old
            // single-slot `*discard_until = send_ok.then(...)` overwrite had:
            // (i) a `CoptUpdateparam` that changes only a field excluded from
            // `same_wire_behavior` (e.g. `CP_TesterPresentExpPosResp`/
            // `CP_P2Max`) never re-arms, so this tick's new window used to
            // silently replace a still-open prior window with different
            // `pos`/`until`; (ii) a failed send (`send_ok == false`) used to
            // unconditionally clear the slot, dropping a still-open prior
            // window outright. Neither the `Armed` match failing (state
            // transitioned away or a different arm generation) nor a failed
            // send now touches a prior window at all.
            let new_discard = if let TesterPresentState::Armed {
                armed_at,
                last_fired,
                resolved,
                framed_data,
                ..
            } = &mut l.tester_present_state
                && *armed_at == snapshotted_armed_at
            {
                *last_fired = Some(fired_at);
                // ADR-099: the window now opens whenever this send merely
                // succeeded (`send_ok`), regardless of `expects_response` --
                // TX-side indication frames (TX_DONE/loopback) are artifacts
                // of the send itself, not of `CP_TesterPresentReqRsp`'s "does
                // the ECU reply" semantics, so hiding tester-present's own
                // TX-confirmation noise from the client must not depend on
                // whether a response is expected. `pos`/`neg` stay frozen
                // from this same pre-send snapshot's `exp_pos_resp`/
                // `exp_neg_resp` ONLY when `expects_response` (the pre-send
                // `CP_TesterPresentReqRsp == 1` snapshot above) was `true`;
                // otherwise they freeze empty, which already can never
                // content-match (`TesterPresentDiscard::pos`/`neg`'s own
                // "empty means not configured, never matches" convention) --
                // so content/SOM-herald discard stays correctly gated on
                // `expects_response` while the window itself now
                // unconditionally opens for TX-side discard (`tx_can_id`).
                send_ok.then(|| ResidualTesterPresentDiscard {
                    window: DiscardWindow {
                        until: fired_at + Duration::from_millis(p2_max_ms as u64),
                        pos: if expects_response {
                            exp_pos_resp
                        } else {
                            Vec::new()
                        },
                        neg: if expects_response {
                            exp_neg_resp
                        } else {
                            Vec::new()
                        },
                    },
                    target_can_ids: resolved.target_can_ids,
                    tx_can_id: tester_present_tx_can_id(resolved, framed_data),
                })
            } else {
                None
            };
            push_open_tp_discard(
                &mut l.open_tp_discards,
                tokio::time::Instant::now(),
                new_discard,
            );
        }
    }
}

/// Call-specific data for one `TxItem::SendRecv` cycle (`handle_send_recv`),
/// as opposed to `ChannelPollCtx`'s handles shared by the whole poll task.
struct SendRecvCycle {
    cop_handle: u32,
    cll_handle: u32,
    protocol_id: u32,
    /// See `TxItem::SendRecv::logical_protocol`'s doc comment.
    logical_protocol: ChannelProtocol,
    /// See `TxItem::SendRecv::base_protocol_id`'s doc comment.
    base_protocol_id: u32,
    tx: SendRecvTx,
    binding: ParamBinding,
    expected_response: Vec<ExpectedResponse>,
    cycle_time_ms: u32,
    send_cycles_remaining: i32,
    num_receive_cycles: i32,
    /// `LogicalLinkState.connect_generation` captured at `StartComPrimitive`
    /// call time (ADR-086), threaded through unchanged from
    /// `TxItem::SendRecv::connect_generation`. Never recaptured for a
    /// cyclic follow-up cycle -- see the continuation re-enqueue below.
    connect_generation: u64,
}

/// Processes one send cycle of a `TxItem::SendRecv` in the poll task
/// (`CoptSendrecv`): writes the already-resolved `tx` via
/// `PassThruWriteMsgs` (or the software ISO-TP driver, ADR-046), optionally
/// staging `binding`'s `effective` ComParam set to hardware first
/// (`ParamBinding::Temp`, ISO 22900-2 §9.4.3), then runs the receive phase
/// (`wait_for_expected_response`, governed by `num_receive_cycles` and the
/// `CP_P2Max` window from `binding.resolved()`).
///
/// `tx` and `binding` were both resolved/bound once, eagerly, by
/// `rpc_start_com_primitive` at `StartComPrimitive` call time (ADR-067) --
/// this function performs no further ComParam resolution of its own, and
/// every cycle of a cyclic send (including follow-ups) reuses the exact same
/// `tx`/`binding` unchanged.
///
/// `send_cycles_remaining == 0` (ADR-059) skips the write entirely -- a
/// receive-only pass with no corresponding request -- but a `Temp` binding's
/// hardware apply/revert and the COP's status transitions still run
/// unconditionally, same as any other cycle.
///
/// Returns `Some(CycleContinuation)` when further send cycles remain
/// (`send_cycles_remaining` > 1 or `-1` for infinite cyclic send, ADR-053) —
/// the caller re-schedules the returned item and must NOT clean the COP up.
/// Returns `None` when the COP ended here (last cycle finished, TX failure,
/// cancellation, or hard channel error); all status events were emitted.
async fn handle_send_recv(cycle: SendRecvCycle, ctx: &ChannelPollCtx) -> Option<CycleContinuation> {
    let SendRecvCycle {
        cop_handle,
        cll_handle,
        protocol_id,
        logical_protocol,
        base_protocol_id,
        tx,
        binding,
        expected_response,
        cycle_time_ms,
        send_cycles_remaining,
        num_receive_cycles,
        connect_generation,
    } = cycle;

    // The cycle time is measured from the start of this cycle's send.
    let cycle_start = tokio::time::Instant::now();
    // ADR-118 (A2-24): EXECUTING at the top of every cycle -- first and
    // continuation alike -- atomically gated on the COP still being live in
    // `primitives`, guard held across the emission (same pattern and deadlock
    // argument as dispatch_tx_item's WAITING emission): every CANCELLED
    // emitter removes the entry under `primitives` before emitting, so
    // EXECUTING can never be observed after CANCELLED. An absent entry means
    // a concurrent disconnect already cancelled this COP and emitted its
    // terminal status -- bail before touching hardware; nothing to clean up.
    // `cop_tag` (ADR-205 Decision item 1) is captured here, once, from this
    // SAME live `CopEntry` read -- kept alive for the rest of this function
    // (cloned into every `send_error_event`/`frame_tester_present_data`/
    // `send_tester_present_once`/`ExpectedResponseWait` call below) rather
    // than re-resolved fresh at each one. `cop_handle` never changes across
    // this function's own execution, and `CopEntry::cop_tag` is write-once,
    // so this single early read remains valid for every later use here.
    let cop_tag = {
        let queue_target = resolve_queue_target(&ctx.logical_links, cll_handle).await;
        let prims = ctx.primitives.lock().await;
        let entry = prims.get(&cop_handle)?;
        let cop_tag = entry.cop_tag.clone();
        send_cop_status(
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            queue_target.as_ref(),
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstExecuting,
            cop_tag.clone(),
        )
        .await;
        cop_tag
    };

    let SendRecvTx {
        mut data,
        tx_flags,
        isotp_tx,
        can_functional,
        request_sid,
        access_timing_request,
        j1939_tx_source,
        tp20_established_tx_id,
        tp20_is_broadcast,
        tx_prefix,
    } = tx;

    // S1 (pre-apply, ADR-086): nothing has touched the hardware yet, so a
    // clean bail is safe here -- mirrors handle_start_comm's Guard A / the
    // other `still_on_this_channel`-style guards in this file.
    let still_on_this_channel = {
        let links = ctx.logical_links.lock().await;
        links.get(&cll_handle).is_some_and(|l| {
            l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
        })
    };
    if !still_on_this_channel {
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
        return None;
    }

    // NumSendCycles == 0 means no send ever -- a receive-only capture with
    // no corresponding request (ADR-059). A `Temp` binding's hardware
    // ComParam apply/revert below is still a real state change (analogous
    // to what CoptStartcomm does to comm state) and runs regardless of
    // whether anything is actually transmitted this cycle -- unaffected by
    // this fix's reordering, see the apply/transmit/revert bracket below.
    let should_transmit = send_cycles_remaining != 0;

    let mut channel_lost = false;
    let mut cancelled = false;
    let mut stale = false;

    // Codex review round 17 fix (P1, PR #101, ADR-192/Phase 7 Stage 7c
    // amendment): CP_P3Func/P3Phys's minimum inter-request gap (ADR-060)
    // and the ADR-086/ADR-180/ADR-188 pre-transmit staleness/addressing-
    // drift recheck (formerly "S2", run AFTER the Temp binding's hardware
    // apply below) now both run BEFORE it instead. P3 is a MINIMUM
    // inter-request gap, so transmitting later than its minimum stays
    // spec-conformant, and resolving whether this cycle will even attempt
    // to transmit before ever touching hardware means a cycle this gate
    // is about to cancel/stale-out never applies a Working snapshot only
    // to revert it having used it for nothing -- closing the window where
    // a sibling CLL's own temp-bound bracket could interleave between this
    // cycle's own apply and its native transmit (see the apply/transmit/
    // revert bracket's own doc comment below for the full race).
    //
    // `should_transmit == false` (a receive-only cycle, ADR-059) skips this
    // whole gate entirely: there is nothing to gap-wait or drift-check for
    // a cycle with no transmit, and the apply/(skip write)/revert bracket
    // below still runs for it unconditionally, same as always.
    //
    // `proceed_to_transmit` becomes `true` only once this gate positively
    // clears a transmit cycle for its native write below; the TP2.0 TX-ID
    // header patch (ADR-188) is applied to `data` here too, immediately
    // once cleared -- unchanged in position relative to the transmit
    // itself, since it only touches the local `data` buffer, never
    // hardware, so its ordering relative to the apply below is immaterial.
    let mut proceed_to_transmit = false;
    if should_transmit {
        // CP_P3Func/P3Phys minimum inter-request gap (ADR-060, CAN-family
        // only): wait, if necessary, before actually writing.
        match wait_for_p3_gap(
            cop_handle,
            cll_handle,
            can_functional,
            num_receive_cycles,
            true,
            false,
            ctx,
        )
        .await
        {
            P3GapOutcome::Cancelled => cancelled = true,
            P3GapOutcome::HardError => channel_lost = true,
            P3GapOutcome::Deferred => unreachable!("defer_if_blocked = false"),
            P3GapOutcome::Ready => {
                // ADR-180 Decision 14 Part B (design-advisor consult, PR #72
                // round 12): re-verify, at THIS cycle's actual transmit
                // point (not just once at `StartComPrimitive` call time),
                // that `j1939_tx_source` -- the source address byte
                // `rpc_primitive.rs::resolve_send_recv_tx` composed this
                // send's frame with -- still matches the CLL's CURRENT
                // `j1939_claimed_address`. Closes the enqueue-time gate's
                // TOCTOU: a claim can be spontaneously lost (or, for a
                // multi-cycle/cyclic send queued before this CLL's first
                // claim resolved, still be pending) between call time and
                // any given cycle's dispatch here -- and this same check
                // runs again on every follow-up cycle, since each one
                // re-enters `handle_send_recv` fresh via `dispatch_tx_item`.
                // `None` (non-J1939) always passes.
                //
                // ADR-180 Decision 18 (design-advisor consult, PR #72 round
                // 15): `j1939_tx_source` is now `Some` for EVERY J1939 CLL
                // (`rpc_primitive.rs::resolve_send_recv_tx`'s own comment),
                // not just a negotiation-requested one, so this comparison
                // alone would now spuriously cancel every send on a
                // genuinely non-negotiated CLL too. Gated additionally on
                // the live `l.j1939_negotiation_posture` field -- not
                // `Engaged` (i.e. `Undecided` or `OptedOut`) for a CLL that
                // never (or explicitly did not) request negotiation at its
                // own `CoptStartcomm` (never cancelled here), `Engaged` for
                // one that structurally did (cancelled whenever the resolved
                // source doesn't match the CURRENT claimed address,
                // INCLUDING when there is no current claimed address at all
                // -- an engaged-but-unclaimed CLL's `l.j1939_claimed_address`
                // is `None`, which never equals `Some(src)`, so the
                // comparison correctly cancels this round's exact spoof
                // finding: a Temp-bound send resolved against a
                // Working-only-staged non-negotiation opt-out on a CLL that
                // is structurally still negotiation-managed and unclaimed).
                let (
                    still_on_this_channel,
                    j1939_claim_drifted,
                    tp20_connection_drifted,
                    tp20_live_tx_id,
                ) = {
                    let links = ctx.logical_links.lock().await;
                    match links.get(&cll_handle) {
                        Some(l)
                            if l.channel_id == Some(ctx.channel_id)
                                && l.connect_generation == connect_generation =>
                        {
                            let drifted = matches!(
                                l.j1939_negotiation_posture,
                                J1939NegotiationPosture::Engaged
                            ) && j1939_tx_source
                                .is_some_and(|src| l.j1939_claimed_address != Some(src));
                            // ADR-188 fix (edge-case-hunter, PR #97): the
                            // TP2.0 sibling of the J1939 claim-drift check
                            // just above -- `tp20_established_tx_id` was
                            // resolved once, at `StartComPrimitive` bind time
                            // (`rpc_primitive.rs`), from this CLL's
                            // `tp20_connection` phase at THAT moment. Unlike
                            // `CoptStartcomm`/`CoptStopcomm`, `CoptSendrecv`
                            // has no `comm_started` precondition gate, so a
                            // `CoptStopcomm`'s queued teardown
                            // (`handle_stop_comm`'s `tp20_connection = None`
                            // writeback) can complete after this COP was
                            // bound and queued but before this cycle actually
                            // dispatches, escaping `CoptStopcomm`'s own
                            // `cops_to_cancel` sweep entirely (that sweep only
                            // cancels COPs already present in `primitives` at
                            // the moment `CoptStopcomm` itself was called).
                            // Re-read the CLL's CURRENT `tp20_connection`
                            // here, under this same lock, rather than trust
                            // the bind-time snapshot: drifted when this CLL
                            // is TP2.0-protocol AND the live connection is
                            // not `Established`, or its live
                            // `established_tx_id` no longer matches the
                            // bind-time value (also covers a torn-down-then-
                            // freshly-re-established connection with a
                            // different TX-ID). `tp20_live_tx_id` is the
                            // live, current TX-ID (`None` for a non-TP2.0
                            // CLL) -- used below, when NOT drifted, in place
                            // of the bind-time value for the actual transmit,
                            // so this closes the window even when the live
                            // and bind-time values happen to coincide only by
                            // construction of this very check.
                            // Codex review fix (P1, PR #101, ADR-192/Phase 7
                            // Stage 7c): a broadcast item (`tp20_is_broadcast`)
                            // skips this whole TP2.0-connection-awareness
                            // path entirely -- no drift computation, no
                            // drift-triggered cancellation, no header
                            // overwrite below. `tp20_established_tx_id` is
                            // captured from the CLL's live connection state
                            // independent of whether this send is a broadcast
                            // (ADR-192 Decision item 1: a CLL can have BOTH an
                            // established connection AND a staged broadcast
                            // for one send), so without this guard a
                            // single-shot broadcast on a CLL with an
                            // established connection would spuriously see
                            // `tp20_drifted == false` and fall into the
                            // overwrite below, clobbering `data[0..4]` (the
                            // broadcast address byte plus 3 payload bytes)
                            // with the connection's live TX-ID.
                            // ADR-210 Decision item 12: both checks re-keyed
                            // from the narrow `is_tp2_0_protocol_id` to
                            // `is_tp2_0_family_protocol_id` -- `l.hw_protocol_id`
                            // is a live link's raw id, so a `_CHx`-connected
                            // TP2.0 link's own TX-ID-drift detection would
                            // otherwise silently never run.
                            let tp20_live_tx_id =
                                (resources::is_tp2_0_family_protocol_id(l.hw_protocol_id)
                                    && !tp20_is_broadcast)
                                    .then(|| {
                                        l.tp20_connection
                                            .filter(|c| c.phase == Tp20ConnectionPhase::Established)
                                            .and_then(|c| c.established_tx_id)
                                    })
                                    .flatten();
                            let tp20_drifted =
                                resources::is_tp2_0_family_protocol_id(l.hw_protocol_id)
                                    && !tp20_is_broadcast
                                    && tp20_live_tx_id != tp20_established_tx_id;
                            (true, drifted, tp20_drifted, tp20_live_tx_id)
                        }
                        _ => (false, false, false, None),
                    }
                };
                if !still_on_this_channel {
                    // S2 (pre-apply, ADR-086 amendment): unlike before this
                    // fix, nothing has touched hardware yet at this point --
                    // the Temp binding's apply now runs strictly after this
                    // whole gate (below), so a stale COP can bail out here
                    // with no hardware-cleanup obligation owed at all,
                    // mirroring S1's own reasoning above. `stale` still
                    // routes through the ordinary funnel below rather than
                    // returning directly, so every terminal exit in this
                    // function keeps going through the same
                    // `channel_lost`/`stale`/`cancelled` handling.
                    stale = true;
                } else if j1939_claim_drifted || tp20_connection_drifted {
                    // Routed through `cancelled` (not `stale`): the
                    // CLL/channel identity `stale` guards is unaffected --
                    // this is this COP's own addressing going stale
                    // relative to the CLL's claim/connection state -- but
                    // both terminate identically via `PduCopstCancelled`
                    // below, the same mechanism `should_skip_cancelled_item`
                    // already uses for every other cancellation class in
                    // this file. Per Finding 1's own "cancel, not recompose
                    // or defer-and-hold" mandate: Active `NODE_ADDRESS`
                    // still holds the lost address for the whole
                    // loss-to-reclaim window (ADR-180 Decision 9's own
                    // accepted residual), so recomposing the frame here
                    // would break the ADR-067 Temp-binding contract for this
                    // send's other header fields. The TP2.0 case (ADR-188
                    // fix) mirrors this exactly: a torn-down/never-
                    // established connection has no header this COP could
                    // legitimately compose against, so it is cancelled here
                    // too, never recomposed against a NEW connection this
                    // COP was never bound against.
                    cancelled = true;
                } else {
                    // ADR-188 fix (edge-case-hunter, PR #97): for a TP2.0
                    // send that reaches here (not drifted, per the check
                    // above), overwrite `data`'s 4-byte TX-ID header prefix
                    // (`tx_header::build_tx_message`'s `PROTOCOL_TP2_0_PS`
                    // arm) with the FRESH, just-re-read
                    // `tp20_live_tx_id` rather than continuing to transmit
                    // the bind-time-bound bytes -- the payload after the
                    // first 4 bytes is unaffected. Since `tp20_drifted` is
                    // `false` here, `tp20_live_tx_id` already equals
                    // `tp20_established_tx_id` numerically for a genuinely
                    // still-live connection; this patch is what keeps that
                    // true structurally (using live state, not merely
                    // gating on a staleness comparison) rather than by
                    // coincidence, so it stays correct even if the drift
                    // condition above is ever loosened independently.
                    if let Some(fresh_tx_id) = tp20_live_tx_id {
                        let header = fresh_tx_id.to_be_bytes();
                        if data.len() >= header.len() {
                            data[..header.len()].copy_from_slice(&header);
                        }
                    }
                    proceed_to_transmit = true;
                }
            }
        }
    }

    // Apply the bound Working snapshot to hardware temporarily (ADR-067,
    // ISO 22900-2 §9.4.3), transmit (only when `proceed_to_transmit` and
    // the apply itself succeeded), then revert. `applied` tracks whether
    // the apply actually ran: the gate above can now reject a cycle
    // (`channel_lost`/`cancelled`/`stale`) BEFORE this bracket ever runs,
    // in which case nothing was ever pushed to hardware and there is
    // nothing to revert -- ADR-067's revert obligation only attaches once
    // an apply actually happened, unlike before this fix, where the apply
    // ran unconditionally, ahead of this whole gate.
    //
    // `isotp_tx.is_none()` (every hardware-transport protocol, including
    // TP2.0 connection-mode and broadcast framing): `ctx.api` is acquired
    // ONCE and held CONTINUOUSLY across apply -> native write -> revert
    // (Codex review round 17, P1, PR #101, ADR-192/Phase 7 Stage 7c
    // amendment) -- mirroring `rpc_primitive.rs::rpc_start_com_primitive`'s
    // own round-8 fix for the TP2.0 broadcast-periodic-start bracket
    // exactly. Without this, a sibling CLL's own temp-bound send/periodic
    // start, or a queued `CoptUpdateparam`, on the same physical channel
    // could land in the gap between this cycle's own apply and its native
    // transmit and clobber which hardware value the transmit actually uses
    // (e.g. `CP_TP20BroadcastInterval`/`CONFIG_TP2_0_T_BR_INT`, wire-visible
    // as of an earlier round of this PR).
    //
    // `isotp_tx.is_some()` (software ISO-TP, ADR-046): the OLD split
    // apply -> `isotp_send` -> revert shape is kept UNCHANGED, each step
    // independently locking `ctx.api` -- `isotp_send`'s own per-frame
    // writes and FlowControl wait each lock `ctx.api` internally, so
    // holding one continuous guard across those would self-deadlock (the
    // same task re-locking the same mutex it is already holding) or need a
    // much larger refactor of the soft-ISO-TP driver. This is an accepted,
    // deliberate, latent residual: there is no constructible instance of
    // this race today for this arm, because software ISO-TP and TP2.0
    // never coexist on the same channel (ADR-046's own CAN dual-channel-
    // mode gating) -- the hazard this fix closes only applies to a
    // hardware-mapped Temp param under a hardware-transport
    // (`isotp_tx.is_none()`) send.
    let mut applied = false;
    let mut apply_ok = true;
    let mut transmit_result: Option<Result<(), TxFailure>> = None;
    if !(channel_lost || cancelled || stale) {
        match &binding {
            ParamBinding::Plain(_) => {
                if proceed_to_transmit {
                    transmit_result = Some(
                        transmit_request(
                            cll_handle,
                            cop_handle,
                            protocol_id,
                            tx_flags,
                            &data,
                            isotp_tx.as_ref(),
                            true,
                            true,
                            ctx,
                        )
                        .await,
                    );
                }
            }
            ParamBinding::Temp { effective } => {
                applied = true;
                // ADR-110 (ISO 22900-2 §9.4.16.2.1 c) NOTE 2 / d)): a temp
                // bracket must never push a PDU_PC_BUSTYPE-class key to
                // hardware at all, lock or no lock -- this CLL's own
                // `effective`/Active can be stale relative to another CLL's
                // real, already-pushed hardware state regardless of
                // whether any lock is currently held, so own-Working-vs-
                // Active agreement can never prove a write is safe.
                // ADR-067 §E's unchanged `PDU_ERR_TEMPPARAM_NOT_ALLOWED`
                // guard already guarantees `effective`'s BUSTYPE portion
                // equals this CLL's own Active, so this strip is a no-op
                // from this CLL's own point of view -- but a real safety
                // fix against the stale-Active clobber.
                let effective = comparam_support::strip_bustype_keys(effective);
                if isotp_tx.is_none() {
                    // ADR-158 (corrected): raw (Plane A, `protocol_id`)
                    // rather than `base_protocol_id` -- see
                    // `apply_params_to_hardware_locked`'s own doc comment.
                    let api = ctx.api.lock().await;
                    // Round 18 (Codex review, P2, PR #101, ADR-192 Decision
                    // item 3 amendment): capture the channel's real
                    // pre-bracket hardware value for CHANNEL_WIDE_UNUM32
                    // keys BEFORE the temp apply below, still under this
                    // same continuously-held `api` guard (round 17's own
                    // fence) -- see `capture_channel_wide_hardware_locked`'s
                    // doc comment for why this closes the sibling-CLL
                    // clobber bug.
                    let channel_wide_restore =
                        capture_channel_wide_hardware_locked(&api, ctx.channel_id, protocol_id)
                            .await;
                    apply_ok = apply_params_to_hardware_locked(
                        &api,
                        ctx.channel_id,
                        protocol_id,
                        &effective,
                    )
                    .await;
                    if apply_ok && proceed_to_transmit {
                        transmit_result = Some(transmit_request_locked(
                            &api,
                            ctx.channel_id,
                            protocol_id,
                            tx_flags,
                            &data,
                        ));
                    }
                    // `transmit_request_locked` never returns
                    // `TxFailure::ChannelLost`/`Cancelled` (only
                    // `isotp_send` -- the `isotp_tx.is_some()` arm below --
                    // ever produces those), so the revert here is
                    // unconditional, unlike the ADR-067 "skip revert on a
                    // hard channel error" case the split-bracket arm below
                    // still has to handle.
                    revert_hardware_to_live_active_locked(
                        &api,
                        &ctx.logical_links,
                        cll_handle,
                        ctx.channel_id,
                        protocol_id,
                        &channel_wide_restore,
                    )
                    .await;
                    drop(api);
                    // `transmit_request`'s own post-hoc `last_bus_activity`
                    // stamp (ADR-083), replicated here since this arm calls
                    // `transmit_request_locked` directly instead of going
                    // through `transmit_request` -- deliberately OUTSIDE
                    // the `api` guard just dropped, same as
                    // `transmit_request`'s own stamp always was.
                    if matches!(transmit_result, Some(Ok(()))) {
                        *ctx.last_bus_activity.lock().await = tokio::time::Instant::now();
                    }
                } else {
                    apply_ok =
                        apply_params_to_hardware(&ctx.api, ctx.channel_id, protocol_id, &effective)
                            .await;
                    if apply_ok && proceed_to_transmit {
                        transmit_result = Some(
                            transmit_request(
                                cll_handle,
                                cop_handle,
                                protocol_id,
                                tx_flags,
                                &data,
                                isotp_tx.as_ref(),
                                true,
                                true,
                                ctx,
                            )
                            .await,
                        );
                    }
                    // ADR-067: skip the revert on a hard channel error --
                    // `handle_channel_hard_error` has already emitted the
                    // full loss-of-comms sequence and hardware may no
                    // longer be reachable at all. Mirrors the pre-existing
                    // `!channel_lost` gate this bracket used to share with
                    // every other binding kind.
                    if !matches!(transmit_result, Some(Err(TxFailure::ChannelLost))) {
                        // Soft ISO-TP (ADR-046) never coexists with TP2.0 on
                        // one channel, so `CP_TP20BroadcastInterval` -- the
                        // only `CHANNEL_WIDE_UNUM32` member today -- is never
                        // relevant here; pass an empty restore set.
                        revert_hardware_to_live_active(
                            &ctx.api,
                            &ctx.logical_links,
                            cll_handle,
                            ctx.channel_id,
                            protocol_id,
                            &[],
                        )
                        .await;
                    }
                }
            }
        }
    }

    let write_ok = if !should_transmit {
        // ADR-059: a receive-only cycle transmits nothing, so its outcome
        // never depends on this cycle's own Temp-binding apply/revert
        // bracket above having succeeded -- matches this function's
        // pre-existing contract exactly (unaffected by this fix).
        true
    } else if channel_lost || cancelled || stale {
        // The pre-apply gate above already rejected this cycle -- nothing
        // was ever applied or transmitted, so no revert was owed either.
        false
    } else if applied && !apply_ok {
        // Recheck generation after a temp-param apply failure (ADR-086,
        // round 10 fix): the apply above is a real hardware `.await` a
        // disconnect+reconnect can complete during, exactly like
        // `transmit_request` itself. Without this check, a stale COP whose
        // temp-param apply failed during that race would fall straight
        // into the plain `!write_ok` path below and get a contradictory
        // `PduCopstFinished` after `cancel_link_cops` already sent
        // `PduCopstCancelled` -- the same class of bug the round-9 fix
        // already closed for the transmit's own `TxFailure::Event` arm,
        // just on the apply-failure side instead of the transmit-failure
        // side. Routes through `stale`, not a direct bail: the revert has
        // already run above regardless (ADR-067 obligation, since the
        // apply DID run here).
        let still_on_this_channel = {
            let links = ctx.logical_links.lock().await;
            links.get(&cll_handle).is_some_and(|l| {
                l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
            })
        };
        if !still_on_this_channel {
            stale = true;
        }
        false
    } else {
        match transmit_result {
            // Nothing to transmit this cycle: the gate above never set
            // `proceed_to_transmit` (structurally unreachable here for a
            // Plain binding since a rejected gate already returned `false`
            // above, but reachable for a Temp binding's own receive-only
            // apply/revert, already handled by the `!should_transmit`
            // branch above -- this arm is dead in practice but kept for
            // exhaustiveness/defense in depth).
            None => true,
            Some(Ok(())) => {
                // Record this send for the *next* CP_P3Func/P3Phys check
                // in the same addressing bucket (ADR-060).
                if let Some(functional) = can_functional {
                    let bucket = if functional {
                        &ctx.last_func_tx
                    } else {
                        &ctx.last_phys_tx
                    };
                    *bucket.lock().await = Some(TxGapState {
                        at: tokio::time::Instant::now(),
                        no_response_required: num_receive_cycles == 0,
                    });
                }
                true
            }
            Some(Err(TxFailure::Event(error_event))) => {
                // Recheck generation (ADR-086, round 9 fix): the transmit
                // above performs real I/O and can span a disconnect+
                // reconnect completing during the call, even though the
                // pre-transmit gate above already passed. This must route
                // through the same `stale` flag as that gate, NOT bail out
                // directly here or merely skip the error event: the revert
                // has already run above regardless (ADR-067 obligation),
                // and falling through to the plain `!write_ok` path further
                // down would send a contradictory `PduCopstFinished` for a
                // COP the client may have already seen `PduCopstCancelled`
                // for (an earlier version of this fix skipped only the
                // error event and left `stale` unset, which missed exactly
                // that).
                let still_on_this_channel = {
                    let links = ctx.logical_links.lock().await;
                    links.get(&cll_handle).is_some_and(|l| {
                        l.channel_id == Some(ctx.channel_id)
                            && l.connect_generation == connect_generation
                    })
                };
                if still_on_this_channel {
                    send_error_event(
                        &ctx.subscriptions,
                        &ctx.logical_links,
                        cll_handle,
                        error_event,
                        Some((cop_handle, cop_tag.clone())),
                    )
                    .await;
                } else {
                    stale = true;
                }
                false
            }
            Some(Err(TxFailure::ChannelLost)) => {
                channel_lost = true;
                false
            }
            Some(Err(TxFailure::Cancelled)) => {
                cancelled = true;
                false
            }
        }
    };

    if channel_lost {
        // handle_channel_hard_error already emitted the loss-of-comms event
        // sequence (including PduCopstCancelled for this COP).
        return None;
    }
    if stale {
        // S2 bail-out (ADR-086): staleness cancels this COP's bookkeeping/
        // status; whether the ADR-067 hardware revert already ran depends on
        // WHICH of `stale`'s setters got here, unlike before this fix's
        // reordering. The pre-apply gate (`channel_lost || cancelled ||
        // stale`, above the apply/write/revert bracket -- see its own
        // comment) can set `stale` BEFORE the bracket ever runs, in which
        // case nothing was applied and no revert was owed (mirrors the
        // `else if channel_lost || cancelled || stale` comment above this
        // one). The post-apply staleness recheck and the post-transmit
        // TxFailure::Event recheck (both further above) only ever set
        // `stale` AFTER the bracket already ran to completion, so in those
        // two cases the revert has indeed already run. Either way this bail
        // is safe: it cancels bookkeeping/status only, never a hardware-
        // cleanup obligation still owed.
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
        return None;
    }
    if cancelled {
        // First-wins through `primitives` (same gate as S2/S3 above and S4
        // below): a concurrent teardown (`cancel_link_cops` /
        // `cancel_held_tx_items`) may have already removed this COP and
        // emitted its own CANCELLED while the ISO-TP FC-wait was still in
        // flight -- skip in that case rather than emitting a duplicate.
        let queue_target = resolve_queue_target(&ctx.logical_links, cll_handle).await;
        let mut prims = ctx.primitives.lock().await;
        if let Some(entry) = prims.remove(&cop_handle) {
            send_cop_status(
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
                queue_target.as_ref(),
                cll_handle,
                cop_handle,
                PduComPrimitiveStatus::PduCopstCancelled,
                entry.cop_tag,
            )
            .await;
        }
        return None;
    }
    if !write_ok {
        // TX failed (temp-param staging or the write itself); the error was
        // already reported via PDU_IT_ERROR events.  End the COP rather than
        // running further cycles into the same failure. ADR-128 correction
        // (Codex review round 2): this emission used to be unconditional --
        // guarded now, matching every other terminal emitter in this file.
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstFinished,
        )
        .await;
        return None;
    }

    // S3 (pre-receive, ADR-086): reachable only when `channel_lost`,
    // `stale`, and `cancelled` are all still false here -- every path that
    // could have set any of them (the pre-apply gate, the post-apply
    // staleness recheck, the post-transmit TxFailure recheck) already
    // returned above before reaching this point. So unlike before this
    // fix's reordering, this is no longer unconditionally "post-revert": it
    // is post-revert only when a Temp binding's apply/write/revert bracket
    // actually ran (it always does for `!should_transmit`, and ran to
    // completion here for a `should_transmit` cycle that reached this far
    // without tripping any of the three flags above); for a Plain binding
    // there was no revert to begin with. Either way a clean bail is safe
    // here -- standard guard idiom, same as S1.
    let still_on_this_channel = {
        let links = ctx.logical_links.lock().await;
        links.get(&cll_handle).is_some_and(|l| {
            l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
        })
    };
    if !still_on_this_channel {
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
        return None;
    }

    // Run this cycle's receive phase: poll RX until `num_receive_cycles`
    // matching frames arrive or the CP_P2Max response window expires
    // (ADR-053).  Each matching frame is delivered to rx_buf and subscribers
    // like any other received frame.  `num_receive_cycles == 0` means the
    // send requires no response at all; `wait_for_expected_response` returns
    // immediately in that case regardless of `expected_response` (ADR-058) --
    // an empty `expected_response` with a nonzero cycle count is not a
    // special case, it naturally times out since nothing can ever match.
    //
    // The response-phase config (CP_P2Max timeout, RC21/23/78 handling) also
    // comes from the bound `ParamBinding` (ADR-067) -- `binding.resolved()`
    // is the SAME snapshot `data`/`tx_flags` above were built from, for
    // every cycle including follow-ups, never a live re-read of Active.
    {
        let params = binding.resolved();
        // ADR-157 Plane B: normalized -- found during this fix's independent
        // sweep (not in ADR-157's own enumerated site list, but the same
        // `RcHandlingConfig::from_params` call `rpc_primitive.rs`'s
        // CoptStartcomm/CoptStopcomm sites were explicitly fixed for).
        // `protocol_id` itself (used below for the actual TX/RX, Plane A)
        // stays raw.
        let rc_cfg = RcHandlingConfig::from_params(
            params,
            ChannelProtocol::from_raw(resources::base_protocol_id(protocol_id)),
        )
        .with_request_sid(request_sid);
        // ADR-146/150: built the same call-time-bound way as `rc_cfg` above,
        // from the SAME snapshot -- `None` on every non-ISO14230/ISO15765
        // channel or when `CP_ModifyTiming` is disabled. Gated on
        // `logical_protocol`, NOT `protocol_id` (edge-case-hunter finding,
        // PR #22) -- `protocol_id` is the hardware channel protocol
        // (`CAN` in `software-isotp` mode for an ISO15765 CLL, per
        // `TxItem::SendRecv::logical_protocol`'s doc comment), while this
        // mechanism must gate on the CLL's logical protocol.
        let timing_cfg = TimingChangeConfig::from_params(params, logical_protocol).map(|cfg| {
            cfg.with_request(access_timing_request.as_deref().unwrap_or(&[]), tx_prefix)
        });
        let response_timeout_ms = params.p2_max_timeout_ms();
        match wait_for_expected_response(
            ExpectedResponseWait {
                cll_handle,
                cop_handle,
                cop_tag: cop_tag.clone(),
                expected: &expected_response,
                num_receive_cycles,
                timeout_ms: response_timeout_ms,
                protocol_id,
                tx_flags,
                original_data: &data,
                isotp_tx: isotp_tx.as_ref(),
                rc_cfg: &rc_cfg,
                timing_cfg: timing_cfg.as_ref(),
                connect_generation,
                cancellable: true,
                match_reset_ceiling_ms: None,
                // ADR-100 Decision §4 (S6), widened by ADR-182:
                // `send_cycles_remaining == 0` is exactly ADR-059's
                // created-receive-only category (this cycle's own doc
                // comment above) -- combined with `num_receive_cycles ==
                // -1` OR a finite `N > 0` inside `wait_for_expected_response`
                // (`is_comparam_timed_receive_only`), selects creation-time
                // tier-2 construction and the `CP_CyclicRespTimeout`
                // deadline below. `-2` (IS-MULTIPLE) is unaffected by
                // ADR-182 -- it is tier-2 from creation too, but never gets
                // a `CP_CyclicRespTimeout` deadline.
                created_receive_only: send_cycles_remaining == 0,
                cyclic_resp_timeout_ms: params.cyclic_resp_timeout_ms(),
                enable_concatenation: params.enable_concatenation(),
            },
            ctx,
        )
        .await
        {
            ReceivePhaseOutcome::Terminal => return None,
            ReceivePhaseOutcome::CycleComplete => {}
            ReceivePhaseOutcome::ReRequestTxFailed => {
                // Byte-for-byte CoptSendrecv's pre-ADR-087 behavior: the
                // error event was already emitted inside the wait; this COP
                // still ends with PduCopstFinished (a failed RC21/RC23
                // re-request is best-effort, same as any other TX failure
                // this receive phase treats as non-fatal). ADR-128
                // correction (Codex review round 2): guarded now, matching
                // every other terminal emitter in this file.
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstFinished,
                )
                .await;
                return None;
            }
            ReceivePhaseOutcome::DetachedToTier2 => {
                // ADR-100 S5: this IS-CYCLIC COP just migrated to tier 2 at
                // its first positive response -- it is permanently done
                // sending (not "between send cycles"), so no
                // CycleContinuation is scheduled and no PduCopstFinished is
                // emitted here. Its lifecycle now continues independently;
                // see ReceivePhaseOutcome::DetachedToTier2's own doc comment
                // for how it ends.
                return None;
            }
        }
    }

    // S4 (terminal/continuation, ADR-086): the receive phase itself can span
    // an arbitrarily long wait (unbounded for IS-CYCLIC) -- re-check here,
    // before the terminal status/continuation decision below, rather than
    // trusting S3's entry-time check alone. If stale: first-wins
    // PduCopstCancelled instead of the normal PduCopstFinished below, and no
    // continuation is scheduled for a stale item (ADR-086: a reconnect
    // mid-cycle cancels this COP, it does not resurrect it under the new
    // generation).
    //
    // A2-24 fix (ADR-118): also drains `cancelled_cops` here, in the SAME
    // `logical_links` lock acquisition, mirroring the `(was_cancelled,
    // is_stale)` idiom used at wait_for_expected_response's own loop bottom
    // (events.rs, `let (was_cancelled, is_stale, uudt_channel_id) = { .. }`
    // a few hundred lines below this function) and at CoptDelay's per-tick
    // check (`handle_delay`, same file). `ReceivePhaseOutcome::CycleComplete`
    // returns from that loop's `Matched` arm the instant this cycle's match
    // quota is hit, structurally bypassing the loop-bottom `cancelled_cops`
    // check for that SAME pass -- so an explicit `CancelComPrimitive`
    // landing concurrently with the cycle-completing response is otherwise
    // never observed until dispatch_tx_item's tail, which cannot distinguish
    // it from a still-live COP and emits a spurious `PduCopstWaiting`.
    let (was_cancelled, still_on_this_channel) = {
        let mut links = ctx.logical_links.lock().await;
        let cancelled = links
            .get_mut(&cll_handle)
            .map(|l| l.cancelled_cops.remove(&cop_handle))
            .unwrap_or(false);
        let on_channel = links.get(&cll_handle).is_some_and(|l| {
            l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
        });
        (cancelled, on_channel)
    };
    if was_cancelled {
        // First-wins through `primitives`, the same discriminator every other
        // CANCELLED emitter uses: CancelComPrimitive left the entry in place,
        // so this remove normally succeeds and this cycle emits the one
        // CANCELLED. A concurrent teardown (`cancel_link_cops` /
        // `cancel_held_tx_items`) that already removed the entry has already
        // emitted it -- skip, never duplicate. ("`cancelled_cops` has one
        // reader" only excludes a double-drain, not an independent teardown
        // emitter -- hence this gate.)
        let queue_target = resolve_queue_target(&ctx.logical_links, cll_handle).await;
        let mut prims = ctx.primitives.lock().await;
        if let Some(entry) = prims.remove(&cop_handle) {
            send_cop_status(
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
                queue_target.as_ref(),
                cll_handle,
                cop_handle,
                PduComPrimitiveStatus::PduCopstCancelled,
                entry.cop_tag,
            )
            .await;
        }
        return None;
    }
    if !still_on_this_channel {
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
        return None;
    }

    // Send-cycle accounting (ADR-053): NumSendCycles -1 = infinite cyclic
    // send, ended only by CancelComPrimitive / disconnect.  0 (no send at
    // all, ADR-059) stays 0 rather than decrementing to -1 (which would
    // otherwise be misread as infinite) -- there is nothing to repeat, so
    // this is always a single non-cyclic pass.
    let remaining = match send_cycles_remaining {
        -1 => -1,
        0 => 0,
        n => n - 1,
    };
    if remaining == 0 {
        // ADR-128 correction (Codex review round 2): this emission used to
        // be unconditional -- guarded now, matching every other terminal
        // emitter in this file.
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstFinished,
        )
        .await;
        return None;
    }

    // More cycles to go: hand the follow-up cycle back to the poll loop.
    // Time > 0 schedules it `Time` ms after this cycle STARTED (cycle time,
    // not a gap); Time == 0 re-enqueues it at the back of the TX queue.
    let due = if cycle_time_ms > 0 {
        cycle_start + Duration::from_millis(cycle_time_ms as u64)
    } else {
        tokio::time::Instant::now()
    };
    Some(CycleContinuation {
        item: TxItem::SendRecv {
            cop_handle,
            cll_handle,
            protocol_id,
            logical_protocol,
            // Copied through unchanged, like `logical_protocol` above --
            // resolved once at call time (ADR-157/ADR-067).
            base_protocol_id,
            // The exact same resolved tx/binding this cycle used (ADR-067):
            // both were bound once, at StartComPrimitive call time, and are
            // reused unchanged by every cycle of a cyclic send.
            tx: SendRecvTx {
                data,
                tx_flags,
                isotp_tx,
                can_functional,
                request_sid,
                access_timing_request,
                j1939_tx_source,
                // Copied through unchanged (ADR-188 fix), mirroring
                // `j1939_tx_source` above: this remains the ORIGINAL
                // `StartComPrimitive`-bind-time TX-ID for every follow-up
                // cycle's own fresh drift check against it, never the
                // previous cycle's live value -- `data`'s 4-byte header
                // prefix, by contrast, DOES carry forward whatever this
                // cycle's own live-TX-ID patch (above) last wrote into it,
                // but that is immaterial: the next cycle re-patches it again
                // from its own fresh read before transmitting.
                tp20_established_tx_id,
                // Copied through unchanged, like `tp20_established_tx_id`
                // above (Codex review fix, P1, PR #101): a broadcast never
                // reaches this follow-up-cycle path in practice (ADR-192
                // Consequences restricts a broadcast `CoptSendrecv` to
                // `num_send_cycles` of `1` or `-1`, and `-1` bypasses the
                // ordinary tx_queue/`dispatch_tx_item` pipeline entirely via
                // the native periodic-message start, so only a single-shot
                // `1` ever reaches the `tx_queue` path this continuation
                // belongs to, which never continues), but the field must
                // still be threaded through since it is mandatory on
                // `SendRecvTx`.
                tp20_is_broadcast,
                // Copied through unchanged, like `request_sid`/
                // `access_timing_request` above (ADR-196 Decision item 3b):
                // this is the same call-time-resolved value every cycle of a
                // cyclic send reuses, never re-derived per cycle.
                tx_prefix,
            },
            binding,
            expected_response,
            cycle_time_ms,
            send_cycles_remaining: remaining,
            num_receive_cycles,
            // Copied through unchanged, like every other call-time-bound
            // field above (ADR-086): one COP is tied to one connection
            // generation for its entire cyclic lifetime. A reconnect mid-
            // cycle must cancel this COP (via the guards above/below), not
            // resurrect it under the new generation -- so this is never
            // re-read from the live link here.
            connect_generation,
        },
        due,
        lower_priority: cycle_time_ms == 0,
    })
}

/// Processes `TxItem::Delay` in the poll task (`CoptDelay`): sleeps for
/// `delay_ms`, in `interval`-sized chunks so the RX path is not blocked for
/// the full duration, polling RX and checking for cancellation after each
/// chunk.
async fn handle_delay(
    cop_handle: u32,
    cll_handle: u32,
    delay_ms: u32,
    connect_generation: u64,
    interval: Duration,
    ctx: &ChannelPollCtx,
) {
    // ADR-118 (A2-24) / ADR-205: EXECUTING is gated atomically on the COP
    // still being live in `primitives`, guard held continuously from the
    // liveness check through the emission -- every CANCELLED emitter removes
    // the entry under `primitives` before emitting, so EXECUTING can never be
    // observed after CANCELLED. An absent entry means a concurrent
    // cancel/teardown already claimed this COP and emitted its own terminal
    // status -- bail before touching hardware; nothing to clean up. `cop_tag`
    // is captured here, once, from this same live `CopEntry` read; `Delay`
    // has no further use for it once this emission is sent, unlike the other
    // handlers sharing this template.
    {
        let queue_target = resolve_queue_target(&ctx.logical_links, cll_handle).await;
        let prims = ctx.primitives.lock().await;
        let Some(entry) = prims.get(&cop_handle) else {
            return;
        };
        let cop_tag = entry.cop_tag.clone();
        send_cop_status(
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            queue_target.as_ref(),
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstExecuting,
            cop_tag,
        )
        .await;
    }

    // Check cancelled_cops after each sleep so that CancelComPrimitive is
    // honoured even when the Delay is already executing.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(delay_ms as u64);
    let mut delay_cancelled = false;
    let mut delay_hard_error = false;
    let mut delay_stale = false;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        tokio::time::sleep(remaining.min(interval)).await;
        let poll_outcome = poll_rx(ctx).await;
        if !poll_outcome.is_ok() {
            // handle_channel_hard_error already emitted
            // PDU_COPST_CANCELLED for this COP via cancel_link_cops.
            delay_hard_error = true;
            break;
        }
        // Honour in-flight cancellation: mirrors the pattern in
        // wait_for_expected_response (CoptSendrecv). Folds a per-tick
        // generation-stale check (ADR-086 round 11) into this SAME
        // `logical_links` lock acquisition, mirroring
        // `wait_for_expected_response`'s own `(was_cancelled, is_stale)`
        // tuple pattern exactly -- this SUPPLEMENTS, not replaces, the
        // `cancelled_cops` check: explicit `CancelComPrimitive` still needs
        // that path, only a disconnect+reconnect needs this one, and both
        // must be checked every tick.
        let (was_cancelled, is_stale) = {
            let mut links = ctx.logical_links.lock().await;
            let cancelled = links
                .get_mut(&cll_handle)
                .map(|l| l.cancelled_cops.remove(&cop_handle))
                .unwrap_or(false);
            let stale = !links.get(&cll_handle).is_some_and(|l| {
                l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
            });
            (cancelled, stale)
        };
        if was_cancelled {
            delay_cancelled = true;
            break;
        }
        if is_stale {
            // End a stale delay early rather than holding the shared
            // channel's poll task for the remainder of a potentially long,
            // client-controlled duration. The terminal block below performs
            // the actual first-wins bail-out.
            delay_stale = true;
            break;
        }
        // ADR-095 amendment (2026-07-18, round 2): the fifth injection site,
        // missed by the original amendment's four-site enumeration --
        // `CoptDelay` polls RX every tick (above) and dispatches
        // tester-present (below) but previously had no detached-registrant
        // maintenance call at all, starving both reap duties for its entire
        // configured duration. Reap runs first, matching
        // `run_due_tick_duties`'s own order. Called unconditionally (ADR-101
        // Decision §E): `reap_expired_cyclic_registrants` consults the
        // per-channel drain watermark map directly rather than this call
        // site's own `poll_outcome`.
        run_detached_registrant_maintenance(ctx).await;
        // CP_TesterPresentSendType (either value, ADR-083): CoptDelay can
        // hold this poll task for an arbitrarily long, client-controlled
        // duration without ever returning to poll_channel_events's outer
        // select loop -- fire any due tester-present CLL on this channel on
        // every tick here too, or a sibling CLL sharing this physical
        // channel could starve for the entire delay.
        dispatch_due_tester_present(false, None, None, false, ctx).await;
    }
    if delay_hard_error {
        // cancel_link_cops already emitted PDU_COPST_CANCELLED.
    } else if delay_cancelled {
        // First-wins through `primitives`, the same discriminator every other
        // CANCELLED emitter uses: CancelComPrimitive left the entry in place,
        // so this remove normally succeeds and this call emits the one
        // CANCELLED. A concurrent teardown (`cancel_link_cops` /
        // `cancel_held_tx_items`) that already removed the entry has already
        // emitted it -- skip, never duplicate.
        let queue_target = resolve_queue_target(&ctx.logical_links, cll_handle).await;
        let mut prims = ctx.primitives.lock().await;
        if let Some(entry) = prims.remove(&cop_handle) {
            send_cop_status(
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
                queue_target.as_ref(),
                cll_handle,
                cop_handle,
                PduComPrimitiveStatus::PduCopstCancelled,
                entry.cop_tag,
            )
            .await;
        }
    } else {
        if delay_stale {
            debug!(
                cop_handle,
                cll_handle,
                "CoptDelay ended early on a per-tick generation-stale detection; terminal recheck below decides the emitted status"
            );
        }
        // Terminal `still_on_this_channel` recheck (ADR-086 round 11),
        // mandatory even when `delay_ms == 0` (breaks out of the loop above
        // with zero ticks ever run, so the per-tick check never gets a
        // chance to run) and for a reconnect landing during the very last
        // tick's `dispatch_due_tester_present` call. Covers both
        // "went stale mid-loop" (`delay_stale` already true, since
        // `connect_generation` only ever moves forward -- this recheck is
        // then redundant but harmless) and "was already stale by the time we
        // got here" (delay_ms == 0 / last-tick race) with a single check,
        // not two separate ones.
        let still_on_this_channel = {
            let links = ctx.logical_links.lock().await;
            links.get(&cll_handle).is_some_and(|l| {
                l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
            })
        };
        // ADR-128 correction (Codex review round 2): the `still_on_this_channel`
        // arm used to call `send_cop_status` directly, unguarded by a
        // `primitives.remove(&cop_handle).is_some()` first-wins check --
        // letting a straggling Finished emission, racing a concurrent
        // `DisconnectComLogicalLink`/`DestroyComLogicalLink` that already
        // removed this entry and recorded the correct `Cancelled`, overwrite
        // that record. Both arms now share ONE `emit_terminal_if_live` call
        // (mirroring `handle_update_param`'s computed-status shape) so there
        // is exactly one `primitives` removal, deciding both which status
        // wins the first-wins race AND which status is actually correct.
        let status = if still_on_this_channel {
            PduComPrimitiveStatus::PduCopstFinished
        } else {
            PduComPrimitiveStatus::PduCopstCancelled
        };
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            status,
        )
        .await;
    }
}

/// Runs the ADR-094 per-tick RX-poll/tester-present duties once `next_tick`'s
/// deadline has passed, and rearms `next_tick` for the next `interval`
/// (ADR-095 audit round: extracted from `poll_channel_events`'s own
/// post-`select!` site into a shared helper so the same tick logic can also
/// run per-item inside the parked-cyclic-follow-up drain loop and
/// `drain_tx_held_backlog`, both of which can otherwise hold this poll task
/// for many ticks' worth of time -- draining a long `tx_held`/parked
/// backlog -- without ever returning to `poll_channel_events`'s own
/// post-`select!` check).
///
/// A no-op success (`true`, `next_tick` left unchanged) when the deadline
/// has not yet passed -- safe to call unconditionally after every drained
/// item rather than only once per outer-loop iteration.
///
/// Returns `false` on `poll_rx`'s hard-error signal, mirroring the existing
/// `break` behaviour at the original inline call site: callers must bail out
/// of the *entire* poll loop (not just their own inner drain loop) when this
/// returns `false`, since `handle_channel_hard_error` has already torn down
/// every COP on this channel.
async fn run_due_tick_duties(
    next_tick: &mut tokio::time::Instant,
    interval: Duration,
    ctx: &ChannelPollCtx,
) -> bool {
    if tokio::time::Instant::now() < *next_tick {
        return true;
    }
    let poll_outcome = poll_rx(ctx).await;
    if !poll_outcome.is_ok() {
        return false;
    }
    // ADR-100 S5: reap any CancelComPrimitive that arrived for a detached
    // (tier-2) IS-CYCLIC registrant -- see that function's own doc comment
    // for why nothing else polls for it.
    reap_cancelled_detached_registrants(ctx).await;
    // ADR-100 Decision §4 (S6): reap any created-receive-only registrant
    // whose `CP_CyclicRespTimeout` deadline expired while detached (i.e.
    // after its first match freed the poll task) -- see that function's own
    // doc comment for why nothing else polls for it in that state. Called
    // unconditionally (ADR-101 Decision §E): `reap_expired_cyclic_registrants`
    // now resolves its own soundness by consulting the per-channel drain
    // watermark map directly, rather than requiring this call site to gate on
    // this tick's own `PollOutcome`. This call is NOT routed through
    // `run_detached_registrant_maintenance` (see that helper's own doc
    // comment for why).
    reap_expired_cyclic_registrants(ctx).await;
    // ADR-190's "Correction" paragraph under `### 4. Disarm / teardown`
    // (Codex review finding, P1, PR #99; design-advisor consult): release
    // any `abandoned && passive` `Tp20ConnEntry` whose bounded `idle_
    // release_at` deadline the per-channel drain watermark has already
    // proven safe to release against -- the same ADR-101 Decision §E
    // "consult the watermark, do not gate on this tick's own outcome"
    // shape `reap_expired_cyclic_registrants` immediately above already
    // uses, reused here rather than re-derived.
    release_idle_passive_slots(ctx).await;
    // CP_TesterPresentSendType (either value, ADR-083/ADR-094): fire any CLL
    // on this channel whose due window has elapsed. After the ordinary RX
    // poll, same as every other periodic per-tick duty at this site.
    dispatch_due_tester_present(false, None, None, false, ctx).await;
    // ADR-179 Decision 3 ("later, spontaneous `_LOST`"): re-enter the
    // address-claim retry loop for any CLL on this channel a spontaneous
    // post-claim loss armed since the last tick.
    run_j1939_reclaim_duties(ctx).await;
    *next_tick = tokio::time::Instant::now() + interval;
    true
}

/// ADR-095 Amendment (2026-07-18): runs both ADR-100 detached-registrant
/// reap duties -- `reap_cancelled_detached_registrants` (S5), then
/// `reap_expired_cyclic_registrants` (S6), same order `run_due_tick_duties`
/// already uses above -- from any of the other long poll-task holds this
/// ADR's Full Enumeration covers, not just the outer tick.
/// `run_due_tick_duties` already calls both reap functions directly above and
/// is deliberately NOT rewritten to call this helper instead; this exists
/// purely so the five non-tick-duty holds (`isotp_send`'s FC-wait and STmin
/// loops, `wait_for_expected_response`'s RC21/RC23 chunked retry sleep, its
/// own receive-phase loop bottom, and `handle_delay`) can pick up the same
/// reap coverage `dispatch_due_tester_present` already has at those sites,
/// without duplicating either reap call inside `run_due_tick_duties` itself.
///
/// Unlike `dispatch_due_tester_present`, no boxing or recursion-cycle guard
/// is needed here: neither reap function touches RX or calls
/// `poll_rx`/`poll_rx_inner` -- both only ever lock `logical_links`,
/// `primitives`, and `subscriptions` to remove a registrant and emit a status
/// notification -- and neither is anywhere on `dispatch_due_tester_present`'s
/// or `wait_for_expected_response`'s own call graph, so there is no
/// recursion cycle for `Box::pin` to break. A reap injected inside one of
/// these holds also can never reap the registrant servicing that same hold:
/// both reap predicates require `RegistrantTier::ReceiveOnly`, and a COP
/// inline-waiting inside one of these holds is tier-1 for the whole of that
/// wait (ADR-100 Decision §2/§4).
///
/// Both reap duties are now called unconditionally, regardless of which site
/// injects this call or whether that site's own loop polls RX at all (ADR-101
/// Decision §E). Earlier revisions of this mechanism (rounds 5/6, since
/// superseded) threaded an `rx_drained`/`rx_freshly_drained` boolean through
/// every call site, approximating `reap_expired_cyclic_registrants`'s
/// soundness condition as "did THIS call's own poll pass exhaustively drain
/// its channel" -- correct for a single-channel CLL, but blind to a UUDT
/// companion channel's own, entirely independent queue state. That
/// approximation has been replaced by a per-channel drain watermark map
/// `reap_expired_cyclic_registrants` consults directly; see
/// [ADR-101 Decision §E](../../../docs/adr/ADR-101-cross-channel-registrant-writeback.md#e-per-channel-drain-watermarks-the-cyclic-reap-must-be-sound-against-both-channels-of-a-dual-channel-cll)
/// for the full invariant and rationale. This function no longer needs any
/// exhaustiveness signal from its caller at all.
///
/// ADR-190's "Correction" paragraph under `### 4. Disarm / teardown` (Codex
/// review finding, P1, PR #99) additionally routes `release_idle_passive_
/// slots` through here, same watermark-consulting shape and same reasoning:
/// a passive listener's bounded quarantine release must not stall just
/// because the poll task is currently parked inside one of this function's
/// own five call sites instead of back at the outer tick.
async fn run_detached_registrant_maintenance(ctx: &ChannelPollCtx) {
    reap_cancelled_detached_registrants(ctx).await;
    reap_expired_cyclic_registrants(ctx).await;
    // ADR-190's "Correction" paragraph under `### 4. Disarm / teardown`
    // (Codex review finding, P1, PR #99; design-advisor consult): same
    // "long non-tick-duty hold still needs this periodic sweep" reasoning
    // this function's own doc comment above already gives for the two reap
    // duties -- a passive slot's `idle_release_at` deadline must not wait
    // for the outer tick to elapse just because the poll task is currently
    // parked inside one of the five long holds this function covers.
    release_idle_passive_slots(ctx).await;
}

/// ADR-100 Decision §2 tier migration (S5): finds and reaps every registrant
/// on `ctx.channel_id`'s CLLs that is BOTH tier-2 (`RegistrantTier::
/// ReceiveOnly`) and marked in its CLL's `cancelled_cops` -- i.e. a detached
/// (migrated) IS-CYCLIC COP that `rpc_cancel_com_primitive` marked cancelled
/// after it already returned from `wait_for_expected_response_inner`
/// (`ReceivePhaseOutcome::DetachedToTier2`). Unlike every OTHER registrant --
/// whose own inline `wait_for_expected_response_inner` loop drains
/// `cancelled_cops` on every pass -- nothing is left actively waiting on a
/// detached registrant to notice a `CancelComPrimitive` once it has
/// exited, so this per-tick sweep is that registrant's only cancellation
/// path (hard error and disconnect/reconnect are already covered
/// unconditionally by `cancel_link_cops`/`handle_channel_hard_error`, which
/// clear `registrants` wholesale regardless of tier).
///
/// Called from `run_due_tick_duties` and, per the ADR-095 amendment covering
/// this function, also from `run_detached_registrant_maintenance` at every
/// other long poll-task hold that ADR's Full Enumeration identifies
/// (`isotp_send`'s FC-wait/STmin loops, `wait_for_expected_response`'s
/// RC21/RC23 retry sleep and receive-phase loop bottom). A cancelled detached
/// registrant is therefore reaped as promptly as the nearest one of those
/// injected hooks the poll task happens to reach -- one `POLL_INTERVAL_MS`
/// tick when no other hold intervenes, but bounded by whatever long hold (if
/// any) the poll task is inside at cancellation time, same as
/// `dispatch_due_tester_present`'s own latency -- not an unconditional
/// per-tick guarantee. Scoped to `ctx.channel_id`: each physical channel's
/// poll task only reaps
/// its own CLLs' registrants (`l.channel_id == Some(ctx.channel_id)`),
/// mirroring `build_cll_rx_entries`'s own channel filter -- a CLL not on this
/// channel is some other poll task's responsibility.
async fn reap_cancelled_detached_registrants(ctx: &ChannelPollCtx) {
    // ADR-095 amendment's round-9 correction / ADR-100's round-9 Finding-1
    // "Safety-invariant interaction": a registrant can now be BOTH tier-2 AND
    // the COP this exact poll task is currently, inline, executing (a
    // finite-N/-2 created-receive-only COP, per Finding-1's tier correction,
    // still runs inline/blocking in `wait_for_expected_response_inner` for
    // the whole of its wait). Never reap that COP's own registrant out from
    // under its own still-running wait loop -- read once here, alongside the
    // existing tier filter.
    let currently_executing = *ctx.executing_cop.lock().await;
    let to_cancel: Vec<(u32, u32)> = {
        let mut links = ctx.logical_links.lock().await;
        let mut found = Vec::new();
        for (&cll_handle, link) in links.iter_mut() {
            if link.channel_id != Some(ctx.channel_id) {
                continue;
            }
            let cancelled_here: Vec<u32> = link
                .registrants
                .iter()
                .filter(|r| {
                    r.tier == RegistrantTier::ReceiveOnly
                        && Some(r.cop_handle) != currently_executing
                        && link.cancelled_cops.contains(&r.cop_handle)
                })
                .map(|r| r.cop_handle)
                .collect();
            for cop_handle in cancelled_here {
                link.registrants.retain(|r| r.cop_handle != cop_handle);
                link.cancelled_cops.remove(&cop_handle);
                found.push((cll_handle, cop_handle));
            }
        }
        found
    };

    for (cll_handle, cop_handle) in to_cancel {
        // First-wins idiom (same as every other cancellation site in this
        // file): only notify when this call is the one that actually removes
        // the entry from `primitives` -- guards against a concurrent
        // `cancel_link_cops`/hard-error sweep (or, in principle, another
        // poll task's own reap pass) having already claimed it.
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
    }
}

/// ADR-100 Decision §4 (S6), widened by ADR-182: finds and reaps every
/// registrant on `ctx.channel_id`'s CLLs that is tier-2 (`RegistrantTier::
/// ReceiveOnly`) and either (a) has reached its finite target match count
/// (`is_receive_only_count_complete`, ADR-182, new), or (b) has an expired
/// `cyclic_deadline` (`is_cyclic_deadline_expired`) -- i.e. a
/// created-receive-only (`NumSendCycles == 0`) `NumReceiveCycles == -1` or
/// finite-`N > 0` COP whose `CP_CyclicRespTimeout` fired with no match since
/// the last one (or ever, if none arrived at all). Every such registrant is
/// inserted directly as tier-2 at creation and detaches immediately
/// (`wait_for_expected_response`'s `is_comparam_timed_receive_only` branch,
/// widened from `-1`-only by ADR-182) -- its owning poll task returns
/// without ever entering `wait_for_expected_response_inner`'s receive loop,
/// so this per-tick sweep is the SOLE mechanism that ever finishes it,
/// whether by count or by timeout -- there is no separate pre-first-match
/// case checked anywhere else. `-2` (IS-MULTIPLE) created-receive-only
/// registrants are unaffected by either condition: `matches_needed` is
/// always `None` for them (unbounded, same as `-1`), and they never get a
/// `cyclic_deadline` (ADR-100 Decision §4's scope, ADR-182 did not widen it
/// to `-2`) -- they keep running inline/blocking, `CP_P2Max`-governed.
///
/// Count-completion (a) is checked BEFORE expiry (b), per registrant: a
/// registrant that satisfies both in the same tick (finite-N's target count
/// reached exactly as its `CP_CyclicRespTimeout` also happens to expire)
/// always finishes via count-completion, never reported as a timeout -- the
/// count is monotone and reaching it is unconditionally a legitimate,
/// successful finish. Count-completion carries no watermark/soundness gate
/// the way expiry does: `matches_got` only ever increments and is already
/// fully merged across a CLL's primary/UUDT companion channels by
/// `merge_registrant_writeback` every poll pass (ADR-101 Decision §A) -- no
/// "a matching frame could still be queued elsewhere" concern applies to a
/// count that has already been reached. A registrant reaped via
/// count-completion emits ONLY `PduCopstFinished` -- no error event -- per
/// ISO 22900-2 §9.2.6.3.4's RECEIVE ONLY row (2022 Table 6): reaching the
/// configured count is this family's normal, successful completion,
/// regardless of `CP_CyclicRespTimeout`.
///
/// A registrant reaped via expiry (b) that has a finite target count
/// (`matches_needed.is_some()` -- the ADR-182-widened finite-`N` subtype)
/// ADDITIONALLY emits `PduErrEvtRxTimeout` and applies ADR-147's
/// `CP_SuspendQueueOnError` hook, BEFORE transitioning to `PduCopstFinished`
/// -- ISO 22900-2:2022 Table 6's RECEIVE ONLY row: with the count not yet
/// reached, an expired `CP_CyclicRespTimeout` is this family's error path,
/// mirroring how `CP_P2Max`'s own expiry is a receive timeout for every
/// other finite-count receive phase in this file (see the analogous hook
/// near the end of `wait_for_expected_response_inner`). A registrant with NO
/// target count (`matches_needed.is_none()` -- the `-1` subtype, UNCHANGED
/// by ADR-182) stays FINISHED-only on expiry, exactly as it always has (ISO
/// 22900-2 §9.2.6.3.4 RECEIVE ONLY NOTE 1's normal, successful completion,
/// not an error) -- `-1` has no target count to fall short of, so there is
/// nothing for a timeout to be an error ABOUT.
///
/// Mirrors `reap_cancelled_detached_registrants` in most respects (same
/// "nothing else polls for it once detached" rationale, same call sites --
/// `run_due_tick_duties` plus, per the ADR-095 amendment, every other long
/// poll-task hold via `run_detached_registrant_maintenance` -- and so the
/// same bounded-by-nearest-injected-hook notification latency, not an
/// unconditional per-tick guarantee, same first-wins notification idiom) --
/// kept as a separate function rather than folded into that one since the
/// REAP conditions (count-completion vs. `cyclic_deadline` expiry, as
/// opposed to `cancelled_cops` membership) are independent and a registrant
/// could in principle need only one of them to ever fire for it. This
/// function also now consults `cancelled_cops` itself, same as
/// `confirm_cyclic_deadline_writeback` does -- but only as a SKIP guard, not
/// a reap trigger: `run_due_tick_duties` calls `reap_cancelled_detached_
/// registrants` (S5) immediately before this function (S6) every tick, but a
/// `CancelComPrimitive` landing on a finite-`N` registrant after S5's own
/// scan and before S6's would otherwise be invisible to S6, which would then
/// wrongly reap it here -- for the finite-`N` family this is no longer
/// harmless once ADR-182 attached `PduErrEvtRxTimeout`/
/// `CP_SuspendQueueOnError` side effects to the expiry arm (edge-case-hunter
/// review of ADR-182; the pre-existing gap was harmless for `-1`, which was
/// always `PduCopstFinished`-only on expiry either way). A cancelled
/// registrant is left in place for `reap_cancelled_detached_registrants`'s
/// own next pass to finalize as `PduCopstCancelled`. NOT mirrored: unlike
/// the cancellation reap, the EXPIRY half of THIS function's correctness
/// additionally depends on RX having been EXHAUSTIVELY drained -- not merely
/// polled -- on EVERY channel that can deliver this registrant a match,
/// since a matching frame could still be sitting queued (behind a full
/// `MAX_POLL_MESSAGES` batch, or entirely unread) on the registrant's own
/// CLL's primary channel OR its UUDT companion channel (ADR-046). Per
/// [ADR-101 Decision §E](../../../docs/adr/ADR-101-cross-channel-registrant-writeback.md#e-per-channel-drain-watermarks-the-cyclic-reap-must-be-sound-against-both-channels-of-a-dual-channel-cll),
/// this is resolved by consulting `ctx.drain_watermarks` directly (see
/// `is_cyclic_reap_sound` below) rather than by any boolean a caller passes
/// in -- this function is therefore now safe to call unconditionally from
/// every injection site. `is_cyclic_reap_sound` itself is reused verbatim,
/// unchanged by ADR-182 -- its soundness argument (every channel that could
/// still deliver a match has been exhaustively drained past the deadline)
/// generalizes to the widened finite-`N` family with no logic change.
///
/// `reap_expired_cyclic_registrants`'s own expiry predicate, extracted as a
/// pure, sync, directly-unit-testable function (`reap_expired_cyclic_
/// registrants` itself is not unit-testable in isolation -- constructing a
/// `ChannelPollCtx` needs a live `J2534Api0404` bound to a loaded shared
/// library) so ADR-101 Decision §D's regression test can exercise the EXACT
/// reap-eligibility check `confirm_cyclic_deadline_writeback` races, not a
/// hand-copied approximation of it.
fn is_cyclic_deadline_expired(r: &CopRegistrant, now: tokio::time::Instant) -> bool {
    r.tier == RegistrantTier::ReceiveOnly && r.cyclic_deadline.is_some_and(|dl| now >= dl)
}

/// ADR-182: `true` when a tier-2 (`RegistrantTier::ReceiveOnly`) registrant
/// with a finite target match count (`matches_needed.is_some()` -- the
/// widened created-receive-only finite-`N > 0` subtype) has reached it.
/// `-1`/`-2` registrants (`matches_needed: None`) never satisfy this --
/// their own completion is governed solely by `is_cyclic_deadline_expired`
/// (`-1`) or the inline `CP_P2Max`-driven loop (`-2`, unaffected by
/// ADR-182), exactly as before. Extracted as a pure, sync function for the
/// same directly-unit-testable reason as `is_cyclic_deadline_expired` above.
fn is_receive_only_count_complete(r: &CopRegistrant) -> bool {
    r.tier == RegistrantTier::ReceiveOnly
        && r.matches_needed
            .is_some_and(|needed| r.matches_got >= needed)
}

/// ADR-101 Decision §E: a cyclic registrant is reapable only once every
/// channel that can deliver it a match has completed an exhaustive drain
/// whose read began at or after its own `cyclic_deadline` -- not merely
/// once the CALLING poll task's own channel has. See that Decision for the
/// full rationale (a UUDT companion channel is an independent poll task
/// with its own adapter queue this reap has no other visibility into).
fn is_cyclic_reap_sound(
    dl: tokio::time::Instant,
    primary_watermark: Option<tokio::time::Instant>,
    uudt_watermark: Option<tokio::time::Instant>,
    has_companion: bool,
) -> bool {
    let primary_ok = primary_watermark.is_some_and(|wm| wm >= dl);
    let companion_ok = !has_companion || uudt_watermark.is_some_and(|wm| wm >= dl);
    primary_ok && companion_ok
}

/// Edge-case-hunter review of ADR-182 (this fix): pure, sync,
/// directly-unit-testable extraction of `reap_expired_cyclic_registrants`'s
/// own per-registrant reap decision, INCLUDING the `cancelled_cops`
/// skip-guard this fix added -- same extraction rationale as
/// `is_cyclic_deadline_expired`/`is_receive_only_count_complete` above
/// (`reap_expired_cyclic_registrants` itself is not unit-testable in
/// isolation; see that function's own doc comment for why). `cancelled` is
/// `link.cancelled_cops.contains(&r.cop_handle)`, read by the caller under
/// its own lock -- passed in rather than looked up here so this stays pure.
///
/// Returns `None` when `r` should be left alone this pass (including when
/// `cancelled` is `true`: a cancelled-but-not-yet-reaped registrant is left
/// for `reap_cancelled_detached_registrants`'s own next pass to finalize as
/// `PduCopstCancelled`, never double-processed here), or `Some(emit_timeout_
/// error)` when it should be reaped this pass -- `emit_timeout_error` gates
/// the `PduErrEvtRxTimeout`/`CP_SuspendQueueOnError` step exactly as
/// `reap_expired_cyclic_registrants`'s own loop already does with this same
/// shape.
fn reap_expired_cyclic_decision(
    r: &CopRegistrant,
    now: tokio::time::Instant,
    cancelled: bool,
    primary_watermark: Option<tokio::time::Instant>,
    uudt_watermark: Option<tokio::time::Instant>,
    has_companion: bool,
) -> Option<bool> {
    if cancelled {
        None
    } else if is_receive_only_count_complete(r) {
        Some(false)
    } else if is_cyclic_deadline_expired(r, now)
        && is_cyclic_reap_sound(
            r.cyclic_deadline.unwrap(),
            primary_watermark,
            uudt_watermark,
            has_companion,
        )
    {
        Some(r.matches_needed.is_some())
    } else {
        None
    }
}

/// One tier-2 registrant this sweep is about to finish, plus whether ADR-182's
/// finite-`N` expiry error path applies to it (see `reap_expired_cyclic_
/// registrants`'s own doc comment for the full count-vs-expiry precedence).
struct ReapedReceiveOnly {
    cll_handle: u32,
    cop_handle: u32,
    /// `true` only for a finite-`N` (`matches_needed.is_some()`) registrant
    /// reaped via `cyclic_deadline` expiry, never via count-completion --
    /// gates the `PduErrEvtRxTimeout`/`CP_SuspendQueueOnError` step below.
    emit_timeout_error: bool,
    /// ADR-204 follow-up (Codex review, PR #116): this COP's `CopEntry::
    /// cop_tag`, captured HERE -- in the same `logical_links`+`primitives`
    /// critical section below that decided this registrant is being reaped
    /// -- rather than resolved fresh, later, by `send_error_event`'s own
    /// internal `primitives` lookup at the point `emit_timeout_error` is
    /// acted on. That later point runs after `logical_links` (and, now,
    /// `primitives`) has been released, so a concurrent `CancelComPrimitive`/
    /// `cancel_link_cops`/hard-error path -- or even `reap_cancelled_
    /// detached_registrants` earlier in the SAME tick, per this function's
    /// own first-wins comment below -- could otherwise have already removed
    /// this `cop_handle`'s `primitives` entry, silently downgrading a
    /// legitimate `cop_tag` to `None` on this one error event even though
    /// every other event for the same COP carried it. Paired with
    /// `cop_handle` into an `ErrorCop` and passed to `send_error_event`
    /// (ADR-205 Decision item 1 merged this function's own fresh-lookup
    /// form and its caller-supplied-tag sibling into one signature -- see
    /// that function's own doc comment).
    cop_tag: Option<Vec<u8>>,
}

async fn reap_expired_cyclic_registrants(ctx: &ChannelPollCtx) {
    let now = tokio::time::Instant::now();
    // ADR-101 Decision §E: read the watermark map BEFORE acquiring
    // `logical_links` -- a stale read only ever defers reaping, never
    // wrongly permits it, so there is no new lock-ordering hazard against
    // ADR-080's existing hierarchy.
    let watermarks = ctx.drain_watermarks.lock().await.clone();
    // ADR-095 amendment's round-9 correction: same uniform "never reap the
    // currently-executing COP" invariant as `reap_cancelled_detached_
    // registrants` -- this reap cannot currently collide (neither
    // `matches_needed`/`matches_got` count-completion nor `cyclic_deadline`
    // is ever set for an inline-waited `-2` created-receive-only COP, ADR-100
    // Decision §4's scope, widened to finite `N > 0` but not `-2` by
    // ADR-182), but the check is one extra comparison and keeps the
    // invariant from silently rotting if that scope is ever widened later.
    let currently_executing = *ctx.executing_cop.lock().await;
    // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c Fix A): a
    // `tx_suspended_by_error` SET below must also terminate an
    // already-running TP2.0 broadcast periodic on the same CLL, in the SAME
    // critical section -- collected here, acted on after `links` is
    // released, see `terminate_tp20_broadcast_periodic_for_suspension`'s own
    // doc comment.
    // Codex review fix (P2, PR #101, round 10): `connect_generation`/
    // `channel_key` are carried alongside `periodic`/`channel_id`, captured
    // from the SAME `link` reference in the SAME critical section below --
    // see `terminate_tp20_broadcast_periodic_for_suspension`'s own doc
    // comment for why the callee needs them.
    let mut suspended_broadcast_periodics: Vec<SuspendedBroadcastPeriodic> = Vec::new();
    let to_finish: Vec<ReapedReceiveOnly> = {
        let mut links = ctx.logical_links.lock().await;
        // ADR-204 follow-up (Codex review, PR #116): nested inside
        // `logical_links`, matching this crate's documented `logical_links ->
        // primitives` order (no inversion) -- held for the rest of this
        // block so each `ReapedReceiveOnly`'s `cop_tag` (below) is read while
        // `primitives` is still guaranteed to reflect this COP's live state,
        // not resolved later after both locks are released.
        let prims = ctx.primitives.lock().await;
        let mut found = Vec::new();
        for (&cll_handle, link) in links.iter_mut() {
            if link.channel_id != Some(ctx.channel_id) {
                continue;
            }
            let primary_watermark = watermarks.get(&ctx.channel_id).copied();
            let uudt_watermark = link
                .uudt_channel_id
                .and_then(|id| watermarks.get(&id).copied());
            let has_companion = link.uudt_channel_id.is_some();
            // ADR-182: count-completion is checked FIRST, per registrant --
            // a registrant satisfying both this tick reaps via
            // count-completion only (`emit_timeout_error: false`), never
            // reported as a timeout. No watermark gate on this arm (see the
            // function-level doc comment for why).
            let reaped_here: Vec<(u32, bool)> = link
                .registrants
                .iter()
                .filter(|r| Some(r.cop_handle) != currently_executing)
                .filter_map(|r| {
                    // edge-case-hunter review of ADR-182: `cancelled` closes
                    // the same-tick race where `CancelComPrimitive` lands
                    // AFTER `reap_cancelled_detached_registrants`'s (S5) scan
                    // but BEFORE this function's (S6) scan in the SAME
                    // `run_due_tick_duties` tick -- see
                    // `reap_expired_cyclic_decision`'s own doc comment for
                    // the full rationale (same `cancelled_cops` set, same
                    // idiom as `confirm_cyclic_deadline_writeback`'s own
                    // check above).
                    reap_expired_cyclic_decision(
                        r,
                        now,
                        link.cancelled_cops.contains(&r.cop_handle),
                        primary_watermark,
                        uudt_watermark,
                        has_companion,
                    )
                    .map(|emit_timeout_error| (r.cop_handle, emit_timeout_error))
                })
                .collect();
            for (cop_handle, emit_timeout_error) in reaped_here {
                // First-wins, checked BEFORE any side effect below (design-
                // advisor review, ADR-205 follow-up): a concurrent
                // `cancel_link_cops` can remove this `cop_handle` from
                // `primitives` in its own critical section, release it, and
                // still be waiting to re-acquire `logical_links` (which this
                // function is holding right now) before it clears
                // `registrants` -- see `cancel_link_cops`'s own removal/
                // re-acquisition gap. If the entry is already gone, whoever
                // removed it owns this COP's terminal emission; skip it
                // entirely this tick (no suspend-queue bump, no
                // `registrants`/`cancelled_cops` mutation, nothing pushed to
                // `found`) rather than acting on a COP that is already gone.
                // Its `registrants` entry will be cleared by that same
                // concurrent caller once this function releases `links`.
                let Some(entry) = prims.get(&cop_handle) else {
                    continue;
                };
                let cop_tag = entry.cop_tag.clone();
                if emit_timeout_error {
                    // ADR-147: same `CP_SuspendQueueOnError`
                    // classify-and-bump shape as the analogous
                    // `PduErrEvtRxTimeout` hook near the end of
                    // `wait_for_expected_response_inner` -- this SET is a
                    // SYNCHRONOUS state change (its evidence, this
                    // registrant's own expiry, is realized at the exact
                    // instant it is applied here), so it bumps
                    // `error_set_seq` too, in this SAME critical section,
                    // strictly BEFORE `send_error_event` below (see that
                    // other call site's own comment for the full race shape
                    // this ordering closes -- unaffected by ADR-182, just
                    // reused here for the newly-widened finite-`N` family).
                    if link.active.suspend_queue_on_error() {
                        link.tx_suspended_by_error = true;
                        link.error_set_seq += 1;
                        if let Some(periodic) = link.tp20_broadcast_periodic.take() {
                            suspended_broadcast_periodics.push((
                                cll_handle,
                                periodic,
                                link.channel_id,
                                link.connect_generation,
                                link.channel_key,
                            ));
                        }
                    }
                }
                link.registrants.retain(|r| r.cop_handle != cop_handle);
                // Drain any stale `cancelled_cops` entry for this COP, same
                // as `reap_cancelled_detached_registrants` and the
                // normal-completion path elsewhere in this file -- this was
                // the one cleanup site in the file that left the entry
                // behind (edge-case-hunter review of ADR-100). No
                // correctness impact (`cancelled_cops` is never consulted
                // independently of `primitives`/`registrants` state), but
                // without this the `HashSet` leaks a `u32` per expiry.
                link.cancelled_cops.remove(&cop_handle);
                found.push(ReapedReceiveOnly {
                    cll_handle,
                    cop_handle,
                    emit_timeout_error,
                    cop_tag,
                });
            }
        }
        found
    };

    for (cll_handle, periodic, channel_id, connect_generation, channel_key) in
        suspended_broadcast_periodics
    {
        ctx.service
            .terminate_tp20_broadcast_periodic_for_suspension(
                cll_handle,
                periodic,
                channel_id,
                connect_generation,
                channel_key,
            )
            .await;
    }

    for reaped in to_finish {
        if reaped.emit_timeout_error {
            // ADR-182: emitted BEFORE the terminal `PduCopstFinished` below,
            // mirroring `wait_for_expected_response_inner`'s own
            // error-then-finish ordering for every other finite-count
            // receive-phase timeout in this file.
            //
            // ADR-204 follow-up (Codex review, PR #116; merged into
            // `send_error_event`'s sole signature by ADR-205 Decision item
            // 1): uses the already-captured `reaped.cop_tag` (see
            // `ReapedReceiveOnly::cop_tag`'s own doc comment) rather than a
            // fresh, internal `primitives` lookup -- by this point
            // `logical_links`/`primitives` have both been released and a
            // concurrent first-wins removal of this same `cop_handle` could
            // otherwise have already dropped its tag.
            send_error_event(
                &ctx.subscriptions,
                &ctx.logical_links,
                reaped.cll_handle,
                PduErrorEvent::PduErrEvtRxTimeout,
                Some((reaped.cop_handle, reaped.cop_tag.clone())),
            )
            .await;
        }
        // First-wins idiom (same as `reap_cancelled_detached_registrants`):
        // only notify when this call is the one that actually removes the
        // entry from `primitives` -- guards against a concurrent
        // `CancelComPrimitive`/`cancel_link_cops`/hard-error path having
        // already claimed it (e.g. the same registrant reaped by
        // `reap_cancelled_detached_registrants` earlier in the same tick).
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            reaped.cll_handle,
            reaped.cop_handle,
            PduComPrimitiveStatus::PduCopstFinished,
        )
        .await;
        // The late-drain race this used to need a separate
        // `drain_cancelled_cop_if_finalized` call for is now handled inside
        // `emit_terminal_if_live` itself (PR #78 edge-case-hunter follow-up;
        // see its doc comment).
    }
}

async fn poll_channel_events(
    mut tx_rx: mpsc::UnboundedReceiver<TxItem>,
    tx_requeue: mpsc::UnboundedSender<TxItem>,
    mut cancel: oneshot::Receiver<()>,
    mut shutdown: Receiver<bool>,
    ctx: ChannelPollCtx,
) {
    let interval = Duration::from_millis(POLL_INTERVAL_MS);
    // Time-scheduled follow-up cycles of cyclic CoptSendrecv items
    // (ADR-053), parked here until their cycle time elapses so that other
    // queued ComPrimitives keep flowing in between.
    let mut parked: Vec<(tokio::time::Instant, TxItem)> = Vec::new();
    // Fixed deadline for the next RX-poll/tester-present tick. Anchored
    // (not recreated per-iteration) so a continuously-non-empty tx_rx
    // cannot starve it: the deadline check after the select! below runs on
    // every loop iteration regardless of which arm fired.
    let mut next_tick = tokio::time::Instant::now() + interval;
    'outer: loop {
        // Dispatch parked follow-up cycles that have come due; they wake
        // with at most one POLL_INTERVAL_MS of jitter via the RX-poll tick
        // below.
        while let Some(pos) = parked
            .iter()
            .position(|(due, _)| *due <= tokio::time::Instant::now())
        {
            let (_, due_item) = parked.remove(pos);
            // Drain this CLL's tx_held backlog first (Codex-review fix,
            // ADR-081 follow-up): a due parked cyclic follow-up is dispatched
            // straight from this loop, bypassing the tx_rx.recv() branch
            // entirely -- without this, a CLL resumed (tx_suspended cleared,
            // ResumeWake sent) but whose wake hasn't been dequeued yet could
            // have an older backlog item still sitting in tx_held while this
            // later cyclic continuation executes first, breaking per-CLL
            // FIFO across a suspend/resume cycle exactly as
            // drain_tx_held_backlog already prevents for the tx_rx.recv()
            // path.
            let (_, due_item_cll) = due_item.handles();
            if !drain_tx_held_backlog(
                due_item_cll,
                interval,
                &ctx,
                &mut parked,
                &tx_requeue,
                &mut next_tick,
            )
            .await
            {
                break 'outer;
            }
            let continuation = dispatch_tx_item(due_item, interval, &ctx, false).await;
            schedule_continuation(
                continuation,
                &mut parked,
                &tx_requeue,
                &ctx.primitives,
                &ctx.logical_links,
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
            )
            .await;
            // CP_TesterPresentSendType (either value, ADR-095 audit round):
            // this parked-cyclic drain loop can iterate for a long time
            // (many due follow-up cycles back-to-back) without ever
            // returning to the post-`select!` tick check below -- run the
            // same shared per-tick duties after each item here too, or a
            // sibling CLL could starve for the whole drain.
            if !run_due_tick_duties(&mut next_tick, interval, &ctx).await {
                break 'outer;
            }
        }

        tokio::select! {
            biased;
            // Stop on disconnect signal or shutdown.
            _ = &mut cancel => break,
            _ = shutdown.changed() => break,
            // Outbound: dispatch a queued operation.
            item = tx_rx.recv() => {
                let Some(item) = item else { break; };
                let (_, item_cll) = item.handles();
                if !drain_tx_held_backlog(
                    item_cll,
                    interval,
                    &ctx,
                    &mut parked,
                    &tx_requeue,
                    &mut next_tick,
                )
                .await
                {
                    break 'outer;
                }
                if !matches!(item, TxItem::ResumeWake { .. }) {
                    let continuation = dispatch_tx_item(item, interval, &ctx, false).await;
                    schedule_continuation(
                        continuation,
                        &mut parked,
                        &tx_requeue,
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                    )
                    .await;
                }
            }
            // Inbound: wake for the next scheduled RX-poll/tester-present
            // tick. The actual work runs unconditionally below once the
            // deadline has passed, regardless of which arm fired this
            // iteration (fixed-deadline anchor; see `next_tick` above).
            _ = tokio::time::sleep_until(next_tick) => {}
        }

        // Unconditional per-tick deadline check: runs every loop iteration
        // (whether tx_rx.recv() fired, the parked-cyclic-follow-up drain
        // loop above ran, or the sleep_until wakeup fired) so a
        // continuously-non-empty tx_rx queue cannot starve RX polling or
        // tester-present dispatch by keeping the biased select's earlier
        // arms perpetually ready.
        if !run_due_tick_duties(&mut next_tick, interval, &ctx).await {
            break;
        }
    }

    // Parked cyclic follow-up cycles that never came due (channel teardown):
    // end them as cancelled, mirroring the queued-item drain below.
    for (_, item) in parked {
        let (cop, cll) = item.handles();
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll,
            cop,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
    }

    // Drain any items that were queued but never dequeued (e.g. channel hard error
    // or disconnect).  Only notify if we are the first to remove the cop from
    // primitives — the disconnect/destroy handler may have beaten us to it.
    while let Ok(item) = tx_rx.try_recv() {
        let (cop, cll) = item.handles();
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll,
            cop,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
    }
}

/// Dispatches one dequeued `TxItem` through its handler, wrapped in the
/// executing-COP bookkeeping shared by fresh queue items and parked cyclic
/// follow-up cycles (ADR-053).  Returns the follow-up cycle to schedule when
/// a cyclic `CoptSendrecv` has more send cycles to run; `None` when the COP
/// ended here (its `primitives` entry has been cleaned up).
async fn dispatch_tx_item(
    item: TxItem,
    interval: Duration,
    ctx: &ChannelPollCtx,
    is_backlog_drain: bool,
) -> Option<CycleContinuation> {
    // Check for cancellation before starting execution.
    let (item_cop, item_cll_pre) = item.handles();
    let is_stop_comm = matches!(item, TxItem::StopComm { .. });
    let item_generation = item.connect_generation();
    if should_skip_cancelled_item(item_cop, item_cll_pre, is_stop_comm, item_generation, ctx).await
    {
        return None;
    }

    // TX-suspend siphon (`PDU_IOCTL_SUSPEND_TX_QUEUE`): while the owning CLL
    // is suspended, hold this item -- including a cyclic/periodic follow-up
    // cycle re-enqueued by `schedule_continuation` -- in `tx_held` instead of
    // executing it, and return early (neither executed nor cancelled).
    // `PDU_IOCTL_RESUME_TX_QUEUE` triggers `drain_tx_held_backlog` to flush
    // `tx_held` at the next same-CLL dequeue, in FIFO order. Checked after
    // the cancellation check above (an explicitly cancelled COP stays
    // cancelled regardless of suspension) and before the item is marked
    // executing (a held item never runs, so it must never flip
    // `executing_cop`).
    //
    // `is_backlog_drain` (Codex-review fix): when `drain_tx_held_backlog`
    // pops the front of `tx_held` and calls this function, a concurrent
    // `PDU_IOCTL_SUSPEND_TX_QUEUE` can land between that pop and this
    // re-check, observing `tx_suspended` true again and re-siphoning the
    // very item that was just popped from the front. Re-inserting it via
    // `push_back` would then place it *behind* whatever is still in
    // `tx_held` (items strictly newer than it), inverting order on the next
    // resume. `push_front` instead restores it to exactly where it was.
    // Items reaching this siphon any other way (a fresh dequeue from the
    // shared `tx_queue`, or a parked cyclic follow-up) are always newer than
    // anything already sitting in `tx_held`, so `push_back` remains correct
    // for those.
    //
    // ADR-123 (Fix B): `tx_suspended_by_lock` alone no longer siphons a
    // non-transmitting item (`item.transmits() == false`, e.g. an empty-data
    // `CoptStopcomm`/`CoptDelay`) -- a `LOCK_PHYSICAL_TX_QUEUE` grant only
    // needs to hold up actual bus traffic (§9.4.13.3 use case 1). The
    // `!link.tx_held.is_empty()` clause is FIFO-preservation, not a
    // transmission check: once a transmitting item is already parked ahead
    // of this one, a non-transmitting item must not overtake it (e.g. a held
    // `CoptStartcomm` + a passing empty `CoptStopcomm` executing out of order
    // would leave comm started when the client asked for stopped).
    // `tx_suspended_by_ioctl` (the client's own explicit
    // `PDU_IOCTL_SUSPEND_TX_QUEUE`) is unconditional, as before -- it has no
    // transmits-only carve-out.
    //
    // ADR-147: `tx_suspended_by_error` (`CP_SuspendQueueOnError`) shares
    // `tx_suspended_by_lock`'s transmits-only gating for the
    // `item.transmits()` check above -- but that alone is not sufficient
    // for recovery: once any transmitting item is already parked in
    // `tx_held`, the FIFO anti-overtake clause still catches a
    // non-transmitting item, including the `CoptUpdateparam` that is the
    // ONLY item kind able to clear the error suspension (via
    // `handle_update_param`'s promotion path). Without a further carve-out
    // that recovery item would be stuck behind its own precondition:
    // permanently held (can't dispatch) and never at the front of
    // `tx_held` for `drain_tx_held_backlog` to pop (can't drain). See
    // `recovery_bypass` immediately below: it exempts `UpdateParam`
    // specifically (not the whole non-transmitting class -- see its own
    // comment for why) from the FIFO clause while error-suspended.
    //
    // ADR-123 (Fix F): the FIFO clause above is gated `!is_backlog_drain`
    // because "don't overtake items ahead of you" means different things at
    // the two call sites this function has. At FRESH-SIPHON time (a new item
    // just dequeued from the shared `tx_queue`, `is_backlog_drain == false`),
    // everything already in `tx_held` is logically AHEAD of the new item, so
    // the clause is the correct FIFO protection described above. At
    // DRAIN-RE-ENTRY time (`drain_tx_held_backlog` popped this exact item off
    // the FRONT of `tx_held` and is re-dispatching it, `is_backlog_drain ==
    // true`), everything still in `tx_held` is strictly BEHIND it -- that
    // function's own doc comment/FIFO invariant guarantees this -- so the
    // clause is vacuously true there and applying it anyway is not merely
    // redundant, it is actively wrong: it would re-siphon (`push_front`) the
    // very item just popped, whenever anything else is still queued behind
    // it, and `drain_tx_held_backlog`'s pop condition would then pop that
    // same front item again next iteration, forever -- a deterministic
    // livelock, not a race, discovered by a naive fix that checked only
    // `drain_tx_held_backlog`'s pop condition without also gating this
    // siphon's FIFO clause. See `drain_tx_held_backlog`'s doc comment for the
    // matching half of this fix.
    //
    // ADR-123 (Fix A): the executing-COP mark below is now inside this same
    // `logical_links` critical section, immediately after the siphon check
    // that would otherwise return early. This closes a race against
    // `rpc_lock_resource`'s active-transmission check (also now taken under
    // `logical_links`, see rpc_link.rs): either the poll task takes
    // `logical_links` first and marks `executing_cop` before dropping the
    // guard, so the grant's busy check (same `logical_links` guard) observes
    // `Some(cop)` and rejects; or the grant takes `logical_links` first and
    // `recompute_lock_tx_suspensions` sets `tx_suspended_by_lock` (under this
    // same `links` guard) before this siphon check can run, so this item
    // gets siphoned instead of dispatched. No interleaving lets a
    // transmission slip through ungated.
    {
        let mut links = ctx.logical_links.lock().await;
        if let Some(link) = links.get_mut(&item_cll_pre) {
            // ADR-147 (recovery-item exemption): an `UpdateParam` item is the
            // one item kind that can itself clear `tx_suspended_by_error` (via
            // `handle_update_param`'s promotion path), so while error-suspended
            // it must bypass the FIFO anti-overtake clause below -- otherwise
            // the recovery item becomes a precondition of its own release (see
            // this function's doc comment above). Narrowly scoped to
            // `UpdateParam` specifically, not the whole non-transmitting class:
            // `RestoreParam` cannot clear the suspension (it only copies
            // Active -> Working in memory) and a blanket exemption would let a
            // non-transmitting item like an empty-data `CoptStopcomm` overtake
            // an already-held transmitting `CoptStartcomm`, reintroducing
            // ADR-123 Fix B's comm-state-inversion hazard. This bypass fires
            // whenever `tx_suspended_by_error` is set, regardless of whether
            // `tx_suspended_by_lock` is also set concurrently: the lock's own
            // guarantee is resource-level (no transmitting traffic on the
            // shared bus resource), not an ordering guarantee over this CLL's
            // own queue, so a non-transmitting item executing under a
            // concurrently-held lock is already legal per ADR-123 Fix B/D. It
            // never bypasses `tx_suspended_by_ioctl`, which stays unconditional.
            //
            // ADR-147 amendment (Codex-review fix, gate narrowing): further
            // scoped to an `UpdateParam` whose OWN call-time-captured `params`
            // snapshot (`ComParamSet::suspend_queue_on_error() == false`) is
            // actually the item that recovers the suspension -- not every
            // `UpdateParam`, regardless of what it changes. An unrelated
            // `CoptUpdateparam` (its snapshot leaves `CP_SuspendQueueOnError`
            // still enabled) does nothing to clear the suspension and must not
            // overtake an older held transmitting item for zero recovery
            // benefit. No live re-check against the link's current state is
            // needed here: if a concurrent `UpdateParam` already promoted
            // `CP_SuspendQueueOnError` to 0, that same critical section already
            // cleared `link.tx_suspended_by_error`, so this predicate's other
            // conjunct is already false and this item takes normal FIFO
            // treatment -- no double-bypass risk. This narrowing is exact only
            // because `apply_bustype_lock` (ADR-110, `LOCK_PHYSICAL_COM_PARAMS`)
            // never substitutes `CP_SuspendQueueOnError`: it only substitutes
            // `PDU_PC_BUSTYPE`-class keys with the CLL's pre-call Active values,
            // and `CP_SuspendQueueOnError` is ERRHDL-class, not BUSTYPE-class --
            // a future widening of that substitution's key class must re-derive
            // this gate's correctness.
            let recovery_bypass = link.tx_suspended_by_error
                && matches!(&item, TxItem::UpdateParam { params, .. } if !params.suspend_queue_on_error());
            if link.tx_suspended_by_ioctl
                || ((link.tx_suspended_by_lock || link.tx_suspended_by_error)
                    && (item.transmits()
                        || (!is_backlog_drain && !link.tx_held.is_empty() && !recovery_bypass)))
            {
                if is_backlog_drain {
                    link.tx_held.push_front(item);
                } else {
                    link.tx_held.push_back(item);
                }
                return None;
            }
        }

        // Mark COP as executing so GetStatus(cop) returns PduCopstExecuting
        // rather than PduCopstWaiting for the duration of item dispatch.
        *ctx.executing_cop.lock().await = Some(item_cop);
    }

    // Record that this cop_handle has been dispatched at least once, so
    // GetStatus(cop) can distinguish a never-started COP (`PduCopstIdle`)
    // from a cyclic COP resting between send cycles (`PduCopstWaiting`).
    // Stored inside the `primitives` entry itself (`CopEntry::dispatched`),
    // so it can never leak independently of that entry (ADR-117).
    // Unconditional and idempotent: re-set on every cycle of a cyclic COP.
    let item_transmits = item.transmits();
    if let Some(entry) = ctx.primitives.lock().await.get_mut(&item_cop) {
        entry.dispatched = true;
        debug_assert_eq!(
            entry.transmits, item_transmits,
            "CopEntry::transmits (set at StartComPrimitive) must match TxItem::transmits() \
             (set at dispatch) for cop_handle {item_cop} -- classification drifted"
        );
    }

    let mut continuation = None;
    match item {
        TxItem::SendRecv {
            cop_handle,
            cll_handle: item_cll,
            protocol_id,
            logical_protocol,
            base_protocol_id,
            tx,
            binding,
            expected_response,
            cycle_time_ms,
            send_cycles_remaining,
            num_receive_cycles,
            connect_generation,
        } => {
            continuation = handle_send_recv(
                SendRecvCycle {
                    cop_handle,
                    cll_handle: item_cll,
                    protocol_id,
                    logical_protocol,
                    base_protocol_id,
                    tx,
                    binding,
                    expected_response,
                    cycle_time_ms,
                    send_cycles_remaining,
                    num_receive_cycles,
                    connect_generation,
                },
                ctx,
            )
            .await;
        }
        TxItem::StartComm {
            cop_handle,
            cll_handle: item_cll,
            protocol_id,
            base_protocol_id,
            tester_present,
            init_tx_flags,
            tx,
            five_baud,
            fast_init,
            binding,
            connect_generation,
        } => {
            handle_start_comm(
                cop_handle,
                item_cll,
                StartCommParams {
                    protocol_id,
                    base_protocol_id,
                    tester_present,
                    init_tx_flags,
                    tx,
                    five_baud,
                    fast_init,
                    binding: *binding,
                    connect_generation,
                },
                ctx,
            )
            .await;
        }
        TxItem::StopComm {
            cop_handle,
            cll_handle: item_cll,
            protocol_id,
            tx,
            connect_generation,
        } => {
            handle_stop_comm(
                cop_handle,
                item_cll,
                protocol_id,
                tx,
                connect_generation,
                ctx,
            )
            .await;
        }
        TxItem::UpdateParam {
            cop_handle,
            cll_handle: item_cll,
            params,
            unique_resp_id_table,
            connect_generation,
        } => {
            handle_update_param(
                cop_handle,
                item_cll,
                params,
                unique_resp_id_table,
                connect_generation,
                ctx,
            )
            .await;
        }
        TxItem::RestoreParam {
            cop_handle,
            cll_handle: item_cll,
            connect_generation,
        } => {
            handle_restore_param(cop_handle, item_cll, connect_generation, ctx).await;
        }
        TxItem::Delay {
            cop_handle,
            cll_handle: item_cll,
            delay_ms,
            connect_generation,
        } => {
            handle_delay(
                cop_handle,
                item_cll,
                delay_ms,
                connect_generation,
                interval,
                ctx,
            )
            .await;
        }
        TxItem::ResumeWake { .. } => {
            // Never reached in practice: the poll loop intercepts and
            // discards `ResumeWake` immediately after triggering
            // `drain_tx_held_backlog`, before it would ever be passed here
            // (and `tx_held`/`parked` never contain a `ResumeWake`, so
            // neither of `dispatch_tx_item`'s other two call sites can hit
            // this arm either).
            debug_assert!(
                false,
                "TxItem::ResumeWake must never reach dispatch_tx_item"
            );
        }
    }

    // Clear executing_cop before removing from primitives.  This closes the
    // window in which GetStatus(cop) could return PduCopstExecuting after
    // the COP has logically finished.  (A continuing cyclic COP reverts to
    // PduCopstWaiting between cycles.)
    *ctx.executing_cop.lock().await = None;

    // ADR-118 (A2-24): a `TxItem::SendRecv` with more cycles to run emits the
    // EXECUTING->WAITING transition here, once per cycle boundary. Gated on
    // `continuation.is_some()` -- true exactly when `handle_send_recv`
    // produced a `CycleContinuation` (more cycles remain), never on the
    // FINISHED path, the stale-channel CANCELLED path, or an ADR-100 tier-2
    // detachment.
    //
    // Explicit-cancel race (round 6): `handle_send_recv`'s own S4 recheck can
    // find nothing (no cancel yet) and still return a continuation, but
    // `CancelComPrimitive` can then complete -- inserting into
    // `cancelled_cops` and leaving the `primitives` entry in place by design
    // -- before this gate runs. A bare `primitives.contains_key` check would
    // then emit a stale WAITING for a COP that was just cancelled, with the
    // correction (CANCELLED) not landing until this parked continuation is
    // next dequeued, up to `cycle_time_ms` later. So `cancelled_cops` is
    // drained here FIRST, atomically with the `primitives` lookup: if it was
    // set, the continuation is dropped (never parked) and CANCELLED is
    // emitted instead of WAITING; otherwise the ordinary `primitives`
    // membership check governs WAITING as before.
    //
    // `logical_links` is deliberately acquired and held across this entire
    // decision (through the `primitives` lock and the send) because
    // `rpc_cancel_com_primitive`'s `cancelled_cops` insert happens under
    // `logical_links` too: holding it here makes the outcome exact --
    // a cancel that completed before we get here is guaranteed visible
    // (CANCELLED, no WAITING), and a cancel still blocked on our held lock is
    // guaranteed to complete strictly after our WAITING send (the ordinary,
    // by-design "cancelled while parked in WAITING" case: `GetStatus`
    // already reports Cancelled immediately via `cancelled_cops` in that
    // case; only the async event defers to the next dequeue, which is
    // intentional, documented behavior -- not this gap).
    //
    // The `primitives` lock guard is still held across the `send_cop_status`
    // call in both the CANCELLED and WAITING arms, for the same reason as
    // before: it stays atomic with respect to every other CANCELLED-emitting
    // path (`cancel_link_cops`, `cancel_held_tx_items`,
    // `should_skip_cancelled_item`) -- each of those removes this entry from
    // `primitives` before emitting CANCELLED, so whichever side wins the
    // `primitives` lock determines the order; WAITING can never be observed
    // after CANCELLED for the same COP.
    //
    // Lock order here is `logical_links -> primitives -> subscriptions`.
    // Deadlock-free: consistent with both of this crate's documented
    // hierarchies (ADR-080's `shared_channels -> {logical_links, api}`,
    // ADR-115's `logical_links -> subscriptions -> queue`); no other site in
    // this crate acquires `logical_links` while already holding `primitives`,
    // and no site holds `subscriptions` while acquiring either (see
    // ADR-118).
    if continuation.is_some() {
        let mut links = ctx.logical_links.lock().await;
        let (was_cancelled, queue_target) = match links.get_mut(&item_cll_pre) {
            Some(l) => (
                l.cancelled_cops.remove(&item_cop),
                Some(CllQueueTarget::from_link(l)),
            ),
            None => (false, None),
        };
        let mut prims = ctx.primitives.lock().await;
        if was_cancelled {
            continuation = None; // never park the cancelled cycle
            if let Some(entry) = prims.remove(&item_cop) {
                send_cop_status(
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    queue_target.as_ref(),
                    item_cll_pre,
                    item_cop,
                    PduComPrimitiveStatus::PduCopstCancelled,
                    entry.cop_tag,
                )
                .await;
            }
        } else if let Some(entry) = prims.get(&item_cop) {
            let cop_tag = entry.cop_tag.clone();
            send_cop_status(
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
                queue_target.as_ref(),
                item_cll_pre,
                item_cop,
                PduComPrimitiveStatus::PduCopstWaiting,
                cop_tag,
            )
            .await;
        }
        // `links` and `prims` guards drop here, before anything below this
        // block runs.
    }

    if continuation.is_none() {
        // ADR-100 S5: a `TxItem::SendRecv` whose receive phase just migrated
        // to tier 2 (`ReceivePhaseOutcome::DetachedToTier2`) also returns no
        // continuation from `handle_send_recv` -- but it is NOT finished, it
        // is alive and independently receiving via its own tier-2
        // registrant. This uniform post-dispatch cleanup must not sweep it
        // from `primitives` the way it does every other no-continuation
        // return (a genuine completion that already emitted its own terminal
        // status, or a `Terminal` outcome that already removed itself
        // earlier inside the wait -- leaving nothing here to remove either
        // way). Detected by checking for a live tier-2
        // (`RegistrantTier::ReceiveOnly`) registrant for this exact
        // `item_cop` on this exact CLL -- cheap and unconditional for every
        // `TxItem` kind, since only `SendRecv` can ever populate one.
        let has_live_detached_registrant = {
            let links = ctx.logical_links.lock().await;
            links.get(&item_cll_pre).is_some_and(|l| {
                l.registrants
                    .iter()
                    .any(|r| r.cop_handle == item_cop && r.tier == RegistrantTier::ReceiveOnly)
            })
        };
        if !has_live_detached_registrant {
            // Normal completion: remove from primitives so that subsequent
            // cancel_com_primitive calls for this handle correctly return not_found.
            // Also clean up any stale cancelled_cops entry (cancel was called while
            // the COP was already executing — the COP completed naturally).
            ctx.primitives.lock().await.remove(&item_cop);
            {
                let mut links = ctx.logical_links.lock().await;
                if let Some(link) = links.get_mut(&item_cll_pre) {
                    link.cancelled_cops.remove(&item_cop);
                }
            }
        }
    }
    continuation
}

/// Re-schedules a cyclic `CoptSendrecv` follow-up cycle (ADR-053):
/// `Time == 0` cycles go to the back of the CLL's TX queue (lower priority
/// than every other queued ComPrimitive, per PDU_COP_CTRL_DATA.Time);
/// `Time > 0` cycles are parked until their cycle time elapses.  When the
/// logical link is already gone, the COP is ended as cancelled.
async fn schedule_continuation(
    continuation: Option<CycleContinuation>,
    parked: &mut Vec<(tokio::time::Instant, TxItem)>,
    tx_requeue: &mpsc::UnboundedSender<TxItem>,
    primitives: &Arc<Mutex<HashMap<u32, CopEntry>>>,
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    subscriptions: &Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    terminal_cops: &Arc<Mutex<TerminalCopsLedger>>,
) {
    let Some(cont) = continuation else { return };
    if !cont.lower_priority {
        parked.push((cont.due, cont.item));
        return;
    }
    let (cop, cll) = cont.item.handles();
    if tx_requeue.send(cont.item).is_err() {
        // The link (or its poll task) went away mid-flight; end the COP the
        // same way the teardown drain does.
        emit_terminal_if_live(
            primitives,
            logical_links,
            subscriptions,
            terminal_cops,
            cll,
            cop,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
    }
}

/// Drains and executes `cll_handle`'s `tx_held` backlog (Codex-review
/// fix), if any, before the caller proceeds to handle a freshly
/// dequeued item belonging to the same CLL. Every entry in `tx_held` is
/// strictly older than anything still in the shared `tx_queue` for this
/// CLL -- items are only ever diverted into `tx_held` at dequeue time,
/// in FIFO order, by `dispatch_tx_item`'s suspend siphon -- so flushing
/// the backlog here, right before the next same-CLL item, restores full
/// per-CLL FIFO order across a suspend/resume cycle without ever
/// re-injecting real items into the shared queue (which would risk
/// reordering against items of the same CLL still in flight, the bug
/// this closes). Stops early if the CLL gets re-suspended mid-drain by
/// a concurrent PDU_IOCTL_SUSPEND_TX_QUEUE (remaining items wait for
/// the next resume) or if the CLL no longer exists.
///
/// ADR-123 (Fix F): the pop gate below no longer refuses to pop ANY item
/// while `tx_suspended_by_lock` alone is set -- it now pops a
/// non-transmitting FRONT item under lock-only suspension, matching Fix B's
/// siphon-time carve-out for non-transmitting items. This is safe only
/// because every item this loop pops is guaranteed, by the FIFO invariant
/// documented above, to have nothing but strictly-older (i.e. behind, not
/// ahead) items remaining in `tx_held` -- so `dispatch_tx_item` must NOT
/// re-apply its fresh-siphon FIFO no-overtake clause when re-dispatching a
/// popped item here (`is_backlog_drain == true` suppresses it; see that
/// function's doc comment). Without that suppression, a lock-only-suspended,
/// non-empty backlog behind a popped non-transmitting front item would
/// re-siphon the very item just popped, and this loop would pop the same
/// front item again next iteration -- a deterministic livelock, not a race.
/// `tx_suspended_by_ioctl` keeps its unconditional, transmits-agnostic gate:
/// the client's own explicit suspend holds the whole queue, so nothing pops
/// at all while it is set, regardless of the front item's transmit status.
///
/// ADR-147: `tx_suspended_by_error` (`CP_SuspendQueueOnError`) is folded into
/// the same lock-class, transmits-only gate as `tx_suspended_by_lock` above,
/// not the ioctl-class unconditional one, for the FRONT-pop arm -- a
/// non-transmitting front item drains normally while error-suspended just
/// like under lock-only suspension. That alone is not sufficient for
/// recovery, though: the front of `tx_held` is commonly a still-blocked
/// *transmitting* item (the one whose failure triggered the suspension in
/// the first place), with the recovery `CoptUpdateparam` -- the only item
/// kind able to clear `tx_suspended_by_error`, via `handle_update_param`'s
/// promotion path -- sitting somewhere behind it, siphoned there by
/// `dispatch_tx_item`'s own FIFO clause. The front-pop arm alone can never
/// reach it. The fallback arm below handles exactly this: while
/// `tx_suspended_by_error` is set and the front-pop condition doesn't hold,
/// search `tx_held` for the first `UpdateParam` from anywhere in the deque
/// (not just the front) and pop it out of order, preserving the relative
/// order of everything else. This is safe from the same livelock this
/// function's Fix F comment describes: `dispatch_tx_item`'s FIFO clause is
/// suppressed at drain-re-entry (`is_backlog_drain == true`), and
/// `UpdateParam` never transmits, so it can never get re-siphoned back into
/// `tx_held` once dispatched from the middle here -- once its promotion
/// clears `tx_suspended_by_error`, this same loop's next iteration resumes
/// popping the front normally.
///
/// ADR-147 amendment (Codex-review fix, gate narrowing): the search predicate
/// is further narrowed to an `UpdateParam` whose own call-time `params`
/// snapshot actually reads `CP_SuspendQueueOnError` disabled
/// (`!params.suspend_queue_on_error()`) -- the identical narrowing applied to
/// `dispatch_tx_item`'s `recovery_bypass` siphon gate, and for the same
/// reason: a non-recovery `UpdateParam` parked in the backlog must not be
/// middle-popped out of FIFO order, since it does nothing to clear the
/// suspension. See that gate's doc comment for the full correctness argument
/// (no live re-check needed, `apply_bustype_lock`/ERRHDL-class dependency).
///
/// `next_tick` (ADR-095 audit round): a large `tx_held` backlog (e.g. a
/// resumed `PDU_IOCTL_SUSPEND_TX_QUEUE`) can hold this poll task across many
/// `POLL_INTERVAL_MS` ticks' worth of dispatches without ever returning to
/// `poll_channel_events`'s own post-`select!` tick check -- `run_due_tick_duties`
/// is called after each drained item so RX polling and tester-present
/// dispatch keep running throughout. Returns `false` on `run_due_tick_duties`'s
/// hard-error signal (propagated straight from `poll_rx`), mirroring its own
/// `false` contract: both existing call sites must then bail out of the
/// entire poll loop, not just continue this drain.
async fn drain_tx_held_backlog(
    cll_handle: u32,
    interval: Duration,
    ctx: &ChannelPollCtx,
    parked: &mut Vec<(tokio::time::Instant, TxItem)>,
    tx_requeue: &mpsc::UnboundedSender<TxItem>,
    next_tick: &mut tokio::time::Instant,
) -> bool {
    loop {
        let next = {
            let mut links = ctx.logical_links.lock().await;
            match links.get_mut(&cll_handle) {
                Some(link) if link.tx_suspended_by_ioctl => None,
                Some(link)
                    if !(link.tx_suspended_by_lock || link.tx_suspended_by_error)
                        || link.tx_held.front().is_some_and(|item| !item.transmits()) =>
                {
                    link.tx_held.pop_front()
                }
                Some(link) if link.tx_suspended_by_error => link
                    .tx_held
                    .iter()
                    .position(|i| {
                        matches!(i, TxItem::UpdateParam { params, .. } if !params.suspend_queue_on_error())
                    })
                    .and_then(|idx| link.tx_held.remove(idx)),
                _ => None,
            }
        };
        let Some(held_item) = next else { break };
        let continuation = dispatch_tx_item(held_item, interval, ctx, true).await;
        schedule_continuation(
            continuation,
            parked,
            tx_requeue,
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
        )
        .await;
        if !run_due_tick_duties(next_tick, interval, ctx).await {
            return false;
        }
    }
    true
}

/// Call-specific data for one `TxItem::StartComm` (`handle_start_comm`), as
/// opposed to `ChannelPollCtx`'s handles shared by the whole poll task.
struct StartCommParams {
    protocol_id: u32,
    /// See `TxItem::StartComm::base_protocol_id`'s doc comment.
    base_protocol_id: u32,
    tester_present: rpc_primitive::ResolvedTesterPresent,
    init_tx_flags: u32,
    /// One-shot transmit(+optional receive) for the optional CAN/J1850
    /// `CoptStartcomm` request message (ADR-111, ISO 22900-2 §9.2.6.3.2 b));
    /// see `TxItem::StartComm::tx`. Mutually exclusive with
    /// `five_baud`/`fast_init`.
    tx: Option<Box<OneShotCommTx>>,
    /// Resolved 5-baud initialisation inputs (ADR-076); see `FiveBaudInit`
    /// and `TxItem::StartComm::five_baud`.
    five_baud: Option<FiveBaudInit>,
    /// Resolved fast-init dispatch, built at `StartComPrimitive` call time
    /// (ADR-075/ADR-077); used by `run_protocol_init` in place of
    /// `five_baud` when `Some` (call-time-exclusive with `five_baud`). See
    /// `FastInit` and `TxItem::StartComm::fast_init`.
    fast_init: Option<FastInit>,
    binding: ParamBinding,
    /// `LogicalLinkState.connect_generation` captured at `StartComPrimitive`
    /// call time; see `TxItem::StartComm::connect_generation` (ADR-086).
    connect_generation: u64,
}

/// Frames `tester_present.data` for one-shot transmission, applying software
/// ISO-TP SingleFrame wrapping (ADR-046) when `tester_present.isotp_framing`
/// is `Some`. Called once at arm time -- both `handle_start_comm`'s initial
/// arm and `handle_update_param`'s re-arm -- and cached in
/// `TesterPresentState::Armed::framed_data` rather than re-framed on every
/// poll tick (ADR-083), since `tester_present.data`/`isotp_framing` never
/// change between arms. A payload that does not fit in a single frame is a
/// soft failure:
/// emits `PduErrEvtTesterPresentError` and returns an empty `Vec` (treated as
/// "no tester-present configured", disabling both modes for this COP) rather
/// than failing the COP.
///
/// `generation` (ADR-086 rounds 11/12, R2): `Some(connect_generation)` from
/// both call sites -- the soft-failure event below is skipped when stale.
/// This function's OWN `send_error_event(...).await` (in the soft-failure
/// arm) is itself a real `.await` a disconnect+reconnect can race,
/// independent of how fresh the call's own entry was; entry-freshness (e.g.
/// `handle_start_comm`'s Guard A2 running with no intervening `.await`
/// before this call) does not protect an `.await` inside the call itself.
/// Round 11 originally passed `None` from `handle_start_comm`'s call site on
/// exactly that mistaken assumption; fixed in round 12 to pass
/// `Some(connect_generation)` there too, matching `handle_update_param`'s
/// tester-present re-arm call site (where `promote_unique_resp_id_table`'s
/// I/O sits immediately before this call).
async fn frame_tester_present_data(
    tester_present: &rpc_primitive::ResolvedTesterPresent,
    cll_handle: u32,
    generation: Option<u64>,
    ctx: &ChannelPollCtx,
    // ISO 22900-2 §9.4.7 c) / §9.6.2: the COP whose own synchronous
    // execution called this (`handle_start_comm`'s initial arm,
    // `handle_update_param`'s re-arm), paired with its already-captured
    // `cop_tag` (`ErrorCop`, ADR-205 Decision item 1 -- forwarded unchanged
    // into this function's own `send_error_event` call below) -- both call
    // sites run inside that COP's own execution, so unlike
    // `send_tester_present_once` (also called from the COP-less periodic
    // dispatch path) this is never `None` in practice, but stays
    // `Option<ErrorCop>` to match `send_error_event`'s signature directly.
    cop: Option<ErrorCop>,
) -> Vec<u8> {
    match (tester_present.isotp_framing, &tester_present.data) {
        (Some(soft), tp) if !tp.is_empty() => {
            let max_payload = soft.addressing.max_sf_payload();
            if tp.len() < 5 || tp.len() - 4 > max_payload {
                warn!(
                    cll_handle,
                    len = tp.len(),
                    max_payload,
                    "software ISO-TP: tester-present message must be [4-byte CAN ID][1..=max_payload byte payload]; disabling tester-present"
                );
                let still_on_this_channel = match generation {
                    Some(connect_generation) => {
                        let links = ctx.logical_links.lock().await;
                        links.get(&cll_handle).is_some_and(|l| {
                            l.channel_id == Some(ctx.channel_id)
                                && l.connect_generation == connect_generation
                        })
                    }
                    None => true,
                };
                if still_on_this_channel {
                    send_error_event(
                        &ctx.subscriptions,
                        &ctx.logical_links,
                        cll_handle,
                        PduErrorEvent::PduErrEvtTesterPresentError,
                        cop,
                    )
                    .await;
                }
                Vec::new()
            } else {
                let mut framed = tp[..4].to_vec();
                let mut sf = isotp::single_frame(&tp[4..], soft.addressing);
                if soft.framing.pad {
                    isotp::pad_frame(&mut sf, soft.framing.filler);
                }
                framed.extend_from_slice(&sf);
                framed
            }
        }
        (_, tp) => tp.clone(),
    }
}

/// Processes `TxItem::StartComm` in the poll task (ADR-067, ADR-076):
///
/// 1. `tester_present` and `init_tx_flags` were already resolved, eagerly, by
///    `rpc_start_com_primitive` at `StartComPrimitive` call time -- no
///    ComParam resolution of any kind happens here.
/// 2. When `binding` is `Temp`: pushes `effective` to hardware via
///    `apply_params_to_hardware` for the duration of the init transaction. A
///    failed apply fails the whole COP (reverting to the live Active set
///    first).
/// 3. Performs the J2534 protocol initialisation sequence when `five_baud`
///    is `Some` or `fast_init` is `Some` (ADR-076/ADR-077 -- both are
///    call-time decisions, never re-derived here), using `init_tx_flags`.
///    The response (keyword bytes / response frame) is pushed into the
///    receive buffer, gated on `five_baud.deliver_keybytes` for the 5-baud
///    case, always delivered for `FastInit::WithRequest`, and NEVER
///    delivered for `FastInit::WakeupOnly` (ADR-077: no request was sent, so
///    per spec there is no response to deliver). On failure, reverts to the
///    live Active set (if `binding` is `Temp`) before emitting
///    `PduErrEvtInitError` + `PduCopstFinished` and returning early.
/// 4. When `binding` is `Temp`: reverts hardware to the live Active set,
///    unconditionally, before the tester-present is ever started -- it must
///    never run under Working.
/// 5. Dispatches tester-present (`tester_present`, always resolved from the
///    call-time Active snapshot) when both the message and its interval are
///    non-zero: both `CP_TesterPresentSendType` values (ADR-083) are
///    software-driven (this diff removes `PassThruStartPeriodicMsg` entirely
///    -- mode 0 is no longer hardware-autonomous), arming a P3-gated
///    software due-clock and sending the first frame immediately through
///    `transmit_request` instead of waiting a full `CP_TesterPresentTime`
///    for the first send (ADR-084). Subsequent sends are dispatched by
///    `dispatch_due_tester_present` per its mode-aware due-check formula.
/// 6. Updates `LogicalLinkState.comm_started` and `tester_present_state`.
/// 7. Emits `PduCllstCommStarted` and `PduCopstFinished`.
async fn handle_start_comm(
    cop_handle: u32,
    cll_handle: u32,
    params: StartCommParams,
    ctx: &ChannelPollCtx,
) {
    let StartCommParams {
        protocol_id,
        base_protocol_id,
        tester_present,
        init_tx_flags,
        mut tx,
        five_baud,
        fast_init,
        binding,
        connect_generation,
    } = params;

    // ADR-118 (A2-24) / ADR-205: EXECUTING is gated atomically on the COP
    // still being live in `primitives`, guard held continuously from the
    // liveness check through the emission -- every CANCELLED emitter removes
    // the entry under `primitives` before emitting, so EXECUTING can never be
    // observed after CANCELLED. An absent entry means a concurrent
    // cancel/teardown already claimed this COP and emitted its own terminal
    // status -- bail before touching hardware; nothing to clean up.
    // `cop_tag` (ADR-205 Decision item 1) is captured here, once, from this
    // SAME live `CopEntry` read -- kept alive for the rest of this function
    // (cloned into every `send_error_event`/`frame_tester_present_data`/
    // `send_tester_present_once`/`ExpectedResponseWait` call below) rather
    // than re-resolved fresh at each one -- `cop_handle` never changes across
    // this function's own execution, and `CopEntry::cop_tag` is write-once,
    // so this single early read remains valid for every later use here, no
    // matter how many `.await` points (hardware I/O, protocol init, J1939
    // claim negotiation) separate this capture from any given use.
    let cop_tag = {
        let queue_target = resolve_queue_target(&ctx.logical_links, cll_handle).await;
        let prims = ctx.primitives.lock().await;
        let Some(entry) = prims.get(&cop_handle) else {
            return;
        };
        let cop_tag = entry.cop_tag.clone();
        send_cop_status(
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            queue_target.as_ref(),
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstExecuting,
            cop_tag.clone(),
        )
        .await;
        cop_tag
    };

    // Guard A (pre-init `still_on_this_channel` check, Codex review /
    // design-advisor, PR #90 P1 fix, ADR-086): unlike the three pre-existing
    // guards below (all placed after `run_protocol_init`), everything from
    // here onward has real, irreversible side effects -- the `Temp`-binding
    // hardware apply immediately below, then `run_protocol_init`'s actual
    // K-line wakeup traffic on the wire. `cancel_link_cops` (called by
    // `DisconnectComLogicalLink`) is not gated on this COP still executing:
    // it removes `cop_handle` from `primitives` and emits `PduCopstCancelled`
    // immediately, even while this poll task is still running. Checking here,
    // before any of that happens, means a clean bail needs no hardware
    // revert -- nothing has touched the hardware yet.
    let still_on_this_channel = {
        let links = ctx.logical_links.lock().await;
        links.get(&cll_handle).is_some_and(|l| {
            l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
        })
    };
    if !still_on_this_channel {
        // Same first-wins idiom as every other guard in this function: see
        // the tester-present arm-time guard below for the full rationale.
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
        return;
    }

    // Round 18 (Codex review, P2, PR #101, ADR-192 Decision item 3
    // amendment): the captured pre-bracket hardware value for every
    // `CHANNEL_WIDE_UNUM32` key, threaded through to every
    // `revert_hardware_to_live_active` call in this function -- see
    // `apply_params_to_hardware_capturing`'s own doc comment for why this
    // (rather than a continuously-held `api` guard) is how this
    // split-bracket function closes the sibling-CLL clobber bug: unlike
    // `handle_send_recv`'s/`rpc_start_com_primitive`'s continuous-bracket
    // sites, `api` is NOT held continuously from the capture/apply below to
    // every one of this function's own later revert call sites (some of
    // which run after further native I/O, e.g. `run_protocol_init`). This
    // does not itself close a window where a concurrent sibling CLL's own
    // `CoptUpdateparam`/broadcast-periodic-start changes the channel-wide
    // value between this capture and one of those later reverts -- see
    // ADR-192's "Residual not closed by this fix, accepted" section for that
    // residual, which is unresolved by this fix and accepted as out of
    // scope (same class of window Guard A/A2/A3's `still_on_this_channel`
    // checks already accept for the unrelated disconnect+reconnect race);
    // also documented in this crate's `docs/implementation-notes.md` under
    // the ADR-192 Phase 7 Stage 7c section's round-18 entry.
    let mut channel_wide_restore: Vec<(u32, u32)> = Vec::new();

    // ── temp_param_update (ADR-067): borrow `effective` for this init transaction only ──
    if let ParamBinding::Temp { effective } = &binding {
        // ADR-110: strip PDU_PC_BUSTYPE-class keys before ever reaching
        // hardware -- see the identical strip and rationale in
        // `handle_send_recv`.
        let effective = comparam_support::strip_bustype_keys(effective);
        // ADR-158 (corrected): raw (Plane A, `protocol_id`) rather than
        // `base_protocol_id` -- see the identical rationale at
        // `handle_send_recv`'s own Temp-binding apply above.
        let (applied, restore) =
            apply_params_to_hardware_capturing(&ctx.api, ctx.channel_id, protocol_id, &effective)
                .await;
        channel_wide_restore = restore;
        if !applied {
            // ADR-086 round 11: `apply_params_to_hardware`'s own `.await` is a
            // real hardware call that can span a disconnect+reconnect landing
            // after Guard A already ran. Snapshot staleness here, before the
            // revert, matching this function's other `Err`-arm pattern of
            // checking before acting.
            let still_on_this_channel = {
                let links = ctx.logical_links.lock().await;
                links.get(&cll_handle).is_some_and(|l| {
                    l.channel_id == Some(ctx.channel_id)
                        && l.connect_generation == connect_generation
                })
            };
            // The temp SET_CONFIG apply itself failed: fail the COP outright
            // rather than silently proceeding with a possibly half-applied
            // Working set -- revert to the live Active set first (ADR-067).
            // Unconditional regardless of staleness: the temp config really
            // is on hardware either way.
            revert_hardware_to_live_active(
                &ctx.api,
                &ctx.logical_links,
                cll_handle,
                ctx.channel_id,
                protocol_id,
                &channel_wide_restore,
            )
            .await;
            if !still_on_this_channel {
                // Same first-wins idiom as every other guard in this
                // function -- the client already saw `PduCopstCancelled` from
                // `cancel_link_cops`, so no `PduErrEvtProtErr`/
                // `PduCopstFinished` pair for the reconnected session.
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstCancelled,
                )
                .await;
                return;
            }
            send_error_event(
                &ctx.subscriptions,
                &ctx.logical_links,
                cll_handle,
                PduErrorEvent::PduErrEvtProtErr,
                Some((cop_handle, cop_tag.clone())),
            )
            .await;
            // ADR-128 correction (Codex review round 2): this emission used
            // to be unconditional -- guarded now, matching every other
            // terminal emitter in this file.
            emit_terminal_if_live(
                &ctx.primitives,
                &ctx.logical_links,
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
                cll_handle,
                cop_handle,
                PduComPrimitiveStatus::PduCopstFinished,
            )
            .await;
            return;
        }
    }

    // Guard A2 (post-Temp-apply `still_on_this_channel` check, ADR-086 round
    // 11): only reachable when `binding` was `Temp` and the apply above
    // succeeded -- `apply_params_to_hardware`'s `.await` can span a
    // disconnect+reconnect completing even though Guard A already ran before
    // it. The very next side effects are `frame_tester_present_data`'s error
    // event and, decisively, `run_protocol_init`'s irreversible K-line wire
    // traffic below -- the existing post-init Guard B structurally cannot
    // prevent that traffic, since it only runs after it has already gone
    // out. A `Plain` binding has no intervening `.await` between Guard A and
    // the next guard beyond the already-documented micro-windows, so it
    // needs nothing here.
    //
    // Note (ADR-086 round 12 fix): Guard A2 (and Guard A, for `Plain`)
    // guarantee a fresh check immediately before `frame_tester_present_data`
    // is CALLED -- they do not protect an `.await` INSIDE that call itself.
    // `frame_tester_present_data`'s own software-ISO-TP size-check failure
    // arm does its own `send_error_event(...).await`, which can independently
    // race a disconnect+reconnect regardless of how fresh the call's entry
    // was. That is exactly what its `generation: Option<u64>` parameter
    // exists to gate -- `Some(connect_generation)` is passed below, not
    // `None` (an earlier version of this fix passed `None` here on the
    // mistaken assumption that entry-freshness alone was sufficient).
    if let ParamBinding::Temp { .. } = &binding {
        let still_on_this_channel = {
            let links = ctx.logical_links.lock().await;
            links.get(&cll_handle).is_some_and(|l| {
                l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
            })
        };
        if !still_on_this_channel {
            // The temp config is physically on hardware regardless of
            // staleness -- revert to the live Active set first (ADR-067),
            // same obligation as Guard B's existing stale-arm pattern.
            revert_hardware_to_live_active(
                &ctx.api,
                &ctx.logical_links,
                cll_handle,
                ctx.channel_id,
                protocol_id,
                &channel_wide_restore,
            )
            .await;
            emit_terminal_if_live(
                &ctx.primitives,
                &ctx.logical_links,
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
                cll_handle,
                cop_handle,
                PduComPrimitiveStatus::PduCopstCancelled,
            )
            .await;
            return;
        }
    }

    // Software ISO-TP mode (ADR-046): the tester-present payload
    // (`[4-byte CAN ID][payload]`) must be wrapped in an ISO-TP SingleFrame
    // by the service — the raw CAN channel will not do it.  Payloads longer
    // than one SingleFrame cannot be sent periodically/idle-triggered and are
    // rejected -- this is a soft failure (tester-present is disabled, but the
    // COP still proceeds and comm still starts), unlike the resolution
    // failure above.
    let mut tester_present_data = frame_tester_present_data(
        &tester_present,
        cll_handle,
        Some(connect_generation),
        ctx,
        Some((cop_handle, cop_tag.clone())),
    )
    .await;

    // Guard A3 (post-framing `still_on_this_channel` check, ADR-086 round
    // 13): round 12 threaded `connect_generation` into `frame_tester_present_data`
    // so its OWN internal `send_error_event(...).await` (soft-failure arm)
    // could gate itself -- but that only stops the stray EVENT, not this
    // COP's continued execution. The helper always returns a plain `Vec<u8>`
    // regardless of whether a disconnect+reconnect landed during its call;
    // without a recheck here, a stale COP would still walk straight into
    // `run_protocol_init`'s irreversible K-line wire traffic below. Same
    // revert-then-bail shape as Guard A2 (only when `binding` was `Temp` --
    // the temp config may already be on hardware).
    let still_on_this_channel = {
        let links = ctx.logical_links.lock().await;
        links.get(&cll_handle).is_some_and(|l| {
            l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
        })
    };
    if !still_on_this_channel {
        if matches!(&binding, ParamBinding::Temp { .. }) {
            revert_hardware_to_live_active(
                &ctx.api,
                &ctx.logical_links,
                cll_handle,
                ctx.channel_id,
                protocol_id,
                &channel_wide_restore,
            )
            .await;
        }
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
        return;
    }

    let tx_flags = tester_present.tx_flags;
    let tester_present_interval_ms = tester_present.interval_ms;

    // ── Step 1: protocol initialisation ──────────────────────────────────────
    // ADR-111: non-empty cop_data for a protocol that does not require an
    // init sequence (CAN/J1850) is no longer silently discarded -- it is
    // carried in `tx` and transmitted (optionally followed by a receive
    // phase) in the `else if let Some(tx) = tx` arm below, rather than
    // warned-and-dropped.
    // ADR-076/ADR-077: whether to run an init step at all, and which one,
    // was already decided at `StartComPrimitive` call time -- `five_baud`
    // and `fast_init` are call-time-exclusive `Option`s, so `five_baud`'s
    // presence alone selects the branch `run_protocol_init` takes below.
    // `InitSequence::None` (explicit skip) and the "cop_data empty and
    // `CP_InitializationSettings` not explicitly `2`" cases leave both unset;
    // a non-K-line link with non-empty cop_data instead populates `tx`
    // (ADR-111), handled by the `else if let Some(tx) = tx` arm below.
    //
    // ADR-180 Decision 9 (Codex review, PR #72 round 8): set `true` only by
    // the J1939 `Claimed` arm below, after `run_j1939_claim_loop` actually
    // succeeds -- read by the shared optional-message transmit sequence a
    // few dozen lines below this whole `if`/`else if` chain to gate
    // `cancel_j1939_claim_after_failed_startcomm` on exactly the early
    // returns that would otherwise leave this COP's own successful claim
    // defended forever with no client-visible COMM_STARTED. Deliberately a
    // local flag, not a live `j1939_claimed_address.is_some()` check: a
    // StopComm -> re-StartComm cycle (Decision 4) or a fresh StartComm that
    // does not request a claim at all can both reach this same shared
    // transmit sequence with `j1939_claimed_address` still set from an
    // EARLIER, unrelated COP -- cancelling that address here would steal
    // Decision 1/4's own ownership of it.
    let mut j1939_claimed_this_cop = false;
    if five_baud.is_some() || fast_init.is_some() {
        // `deliver_keybytes` also gates the whole response-delivery block
        // below (no `ReceivedFrame` push, no `ResultData` notification) for
        // `FastInit::WakeupOnly` -- no request was sent, so per spec there is
        // no response to deliver (ADR-077). Derived from the `FastInit`
        // variant, never from whether the adapter's response bytes happen to
        // be empty.
        let deliver_keybytes = match &five_baud {
            Some(f) => f.deliver_keybytes,
            None => !matches!(fast_init, Some(FastInit::WakeupOnly)),
        };

        // Pre-init tester-present top-up (PR #97 fifth Codex review round,
        // design-advisor-approved): `run_protocol_init` below is one opaque
        // blocking `PassThruIoctl` call -- anywhere from ~0.5s (fast init) to
        // ~2-5s (5-baud, ISO 9141-2 W1-W4 timing) -- held under this shared
        // physical channel's `ctx.api` mutex the whole time. That blocks
        // `dispatch_due_tester_present`'s own periodic dispatch for every
        // OTHER CLL sharing this channel, so an already-armed sibling's
        // mode-0 (`CP_TesterPresentSendType == 0`) keepalive would otherwise
        // silently pause for the full init duration on top of however much
        // of its own interval had already elapsed (worst case:
        // `CP_TesterPresentTime + init_duration`). Force-firing every
        // currently-armed mode-0 sibling once here, unconditionally, bounds
        // that gap down to `max(CP_TesterPresentTime, init_duration)` --
        // the forced send's own `last_fired` stamp makes the sibling's
        // subsequent scheduling self-correct from this instant. Mode 1
        // (idle-triggered) does NOT need this: the init's own wire traffic
        // already stamps `last_bus_activity` below, which is exactly what
        // defers a mode-1 sibling by design (ADR-083) -- `force` never
        // bypasses the due-check for mode-1 CLLs, so they are left alone
        // here and continue to follow their normal idle-triggered logic.
        // `exclude_cll = Some(cll_handle)` (defensive/for consistency with
        // every other call site in this file): this CLL has no
        // tester-present armed yet at this point regardless, since arming
        // happens later in this same function, well after this call
        // returns. `defer_gap_wait = false`: this is a one-shot call outside
        // any match-sensitive receive-phase loop, so there is no
        // `wait_for_expected_response`-style reason to defer an unprobed
        // `poll_rx` inside `wait_for_p3_gap`. `exclude_isotp_target = None`
        // (PR #97 sixth Codex review round): no ISO-TP transfer is in flight
        // at this call site -- this is a one-shot top-up immediately before
        // `run_protocol_init`, not a mid-transfer wait -- so there is no wire
        // target to protect here.
        dispatch_due_tester_present(false, Some(cll_handle), None, true, ctx).await;

        match run_protocol_init(
            five_baud,
            fast_init,
            ctx.channel_id,
            protocol_id,
            init_tx_flags,
            &ctx.api,
        )
        .await
        {
            Ok(response_bytes) => {
                // A successful 5-baud/fast-init wakeup sequence is genuine
                // external bus activity (ADR-083): `run_protocol_init` talks
                // to the adapter directly via `ctx.api`, not through
                // `transmit_request`/`write_can_frame`, so none of those
                // paths' `last_bus_activity` stamps ever see this traffic.
                // Stamped here, unconditionally, so a sibling mode-1
                // (idle-triggered) CLL sharing this physical channel still
                // has its idle window correctly deferred by this init --
                // regardless of which `CP_TesterPresentSendType` this CLL
                // itself ends up using below.
                *ctx.last_bus_activity.lock().await = tokio::time::Instant::now();

                // Guard B (post-init `still_on_this_channel` check, Codex
                // review / design-advisor, PR #90 P1 fix, ADR-086): the K-line
                // wakeup traffic above already happened and cannot be
                // unsent -- hence the unconditional stamp above, which must
                // stay unconditional regardless of what this check finds (it
                // records channel-scoped physical truth that every sibling
                // mode-1 CLL on this shared channel needs, not link-scoped
                // bookkeeping). But `run_protocol_init` may have held
                // `ctx.api`'s mutex for the whole handshake, and
                // `DisconnectComLogicalLink` itself blocks on that same mutex
                // (to stop this CLL's message filters) before it ever reaches
                // `cancel_link_cops` -- so a disconnect+reconnect of this same
                // `cll_handle` onto this still-shared physical channel can
                // land in the window right after `run_protocol_init` returns.
                // Everything below this point -- the DATA_RATE write-back and
                // the synthetic response delivery -- must not land on a
                // session the client has already been told is `Cancelled`.
                let still_on_this_channel = {
                    let links = ctx.logical_links.lock().await;
                    links.get(&cll_handle).is_some_and(|l| {
                        l.channel_id == Some(ctx.channel_id)
                            && l.connect_generation == connect_generation
                    })
                };
                if !still_on_this_channel {
                    if matches!(&binding, ParamBinding::Temp { .. }) {
                        // The temp config is physically on the hardware at
                        // this point (ADR-067) -- staleness cancels the COP's
                        // bookkeeping/status, not its hardware-cleanup
                        // obligation.
                        revert_hardware_to_live_active(
                            &ctx.api,
                            &ctx.logical_links,
                            cll_handle,
                            ctx.channel_id,
                            protocol_id,
                            &channel_wide_restore,
                        )
                        .await;
                    }
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstCancelled,
                    )
                    .await;
                    return;
                }

                // CP_Baudrate write-back (ADR-076): after a successful 5-baud
                // init (spec or legacy path, never fast-init), read back the
                // baud rate the adapter negotiated during the sequence and
                // store it in BOTH Working and Active -- writing only Active
                // would leave Working != Active on a BUSTYPE-class param and
                // spuriously trip ADR-067's PDU_ERR_TEMPPARAM_NOT_ALLOWED
                // guard on a later temp_param_update call. Buffer-only: never
                // pushed to hardware. No ordering constraint against the
                // temp-init revert below -- ADR-011's DATA_RATE filter in
                // `apply_params_to_hardware` already makes that revert
                // baud-neutral. A DATA_RATE the client had staged in Working
                // (pending CoptUpdateparam) is deliberately overwritten: the
                // negotiated rate is ground truth for the now-live link, and
                // a staged value could never reach hardware anyway (ADR-011).
                if five_baud.is_some() {
                    let baud_result = {
                        let api = ctx.api.lock().await;
                        api.get_config_u32(ctx.channel_id, j2534_0404::DATA_RATE)
                    };
                    match baud_result {
                        Ok(negotiated) => {
                            let mut links = ctx.logical_links.lock().await;
                            // Defense-in-depth, not a new bail-out point
                            // (Codex review / design-advisor, PR #90 P1 fix,
                            // ADR-086): re-checks the same predicate Guard B
                            // just checked, atomically with this write, to
                            // close the TOCTOU between Guard B's check and
                            // this write site. Guard B already decided
                            // whether to proceed; if the filter yields `None`
                            // here, just skip this write silently -- no
                            // additional status emission.
                            if let Some(link) = links.get_mut(&cll_handle).filter(|l| {
                                l.channel_id == Some(ctx.channel_id)
                                    && l.connect_generation == connect_generation
                            }) {
                                link.working
                                    .unum32
                                    .insert(ComParamId(j2534_0404::DATA_RATE), negotiated);
                                link.active
                                    .unum32
                                    .insert(ComParamId(j2534_0404::DATA_RATE), negotiated);
                            }
                        }
                        Err(err) => {
                            warn!(
                                %err,
                                "failed to read back CP_Baudrate after a successful 5-baud init; \
                                 leaving the ComParam sets unchanged (best-effort readback only)"
                            );
                        }
                    }
                }

                if deliver_keybytes {
                    // Fast-init responses get the same header/footer split
                    // `poll_rx_inner` performs for ordinary received frames
                    // (ADR-051, ADR-075) -- KWP is the only protocol
                    // `run_protocol_init` ever runs against, and
                    // `usdt_addressing_by_id`/`uudt_addressing_by_id` only
                    // matter for ISO15765, so an empty slice is passed here
                    // (either table would do; `usdt_addressing_by_id` is used
                    // for consistency with an ordinary USDT-routed delivery).
                    // FiveBaud keybytes stay raw
                    // (no header concept for `five_baud_init`'s response).
                    // ADR-157 Plane B (Codex review, PR #28): `header_footer_len`
                    // is a protocol-family decision, so it must see
                    // `base_protocol_id`, not the raw (possibly `_PS`)
                    // `protocol_id` -- `header_footer_len`'s own match only
                    // recognizes base K-line ids, so a pin-selected
                    // `ISO9141_PS`/`ISO14230_PS` link's fast-init response hit
                    // its catch-all `(0, 0)` arm and was delivered as one
                    // unsplit blob instead of separated header/payload/footer.
                    let (header, payload, footer) = if five_baud.is_none() {
                        // ADR-171: `None` here is correct, not a placeholder --
                        // this path only ever runs for K-line links
                        // (ISO9141/ISO14230; see the comment above), which
                        // never populate or consult `ExtraDataIndex`. J1850,
                        // the only branch that reads this parameter, is
                        // structurally unreachable at this call site.
                        // ADR-197: `false` here is correct, not a placeholder --
                        // no real ISO15765 native message is ever reachable at
                        // this K-line fast-init call site (mirrors the `None`
                        // rationale for `extra_data_index` just above).
                        // ADR-222: `frame_is_29bit: false` for the identical
                        // reason -- this K-line-only call site never reaches
                        // ISO15765, the only protocol_id this parameter
                        // affects.
                        let (header_len, footer_len) = header_footer_len(
                            base_protocol_id,
                            &[],
                            &response_bytes,
                            true,
                            None,
                            false,
                            false,
                        );
                        let mut remainder = response_bytes;
                        let mut payload = remainder.split_off(header_len.min(remainder.len()));
                        let header = remainder;
                        let footer_start = payload.len().saturating_sub(footer_len);
                        let footer = payload.split_off(footer_start);
                        (header, payload, footer)
                    } else {
                        (Vec::new(), response_bytes, Vec::new())
                    };
                    let rx_buf = {
                        let links = ctx.logical_links.lock().await;
                        // Same defense-in-depth re-check as the DATA_RATE
                        // write-back above -- skip silently on a `None`
                        // filter result, not a new bail-out point.
                        links
                            .get(&cll_handle)
                            .filter(|l| {
                                l.channel_id == Some(ctx.channel_id)
                                    && l.connect_generation == connect_generation
                            })
                            .map(|l| Arc::clone(&l.rx_buf))
                    };
                    if let Some(rx_buf) = rx_buf {
                        let ts = module_timestamp_us();
                        // ADR-205: reuse this function's own hoisted `cop_tag`
                        // (captured once, near this function's start,
                        // specifically so it survives every intervening
                        // `.await` -- see that capture's own doc comment) --
                        // never re-resolve it fresh here. A previous version
                        // of this line did exactly that
                        // (`resolve_cop_tag(&ctx.primitives, Some(cop_handle)).await`),
                        // reproducing the same "decided-live-then-resolved-
                        // fresh-after-a-gap" bug this ADR exists to eliminate,
                        // since `run_protocol_init`'s real K-line hardware I/O
                        // sits between the hoisted capture and this point.
                        // `deliver_or_enqueue` reads the queue's own
                        // `live_sender` itself, fresh, under the `rx_buf` lock
                        // it takes (ADR-115 round 6) -- no subscriber is
                        // captured here ahead of time. Also forwards to
                        // SubscribeEvent subscribers so streaming clients see
                        // the init response without polling GetEventItem.
                        deliver_or_enqueue(
                            &rx_buf,
                            cll_handle,
                            CllQueueItem::Frame(ReceivedFrame {
                                timestamp: ts,
                                data: payload,
                                header_bytes: header,
                                footer_bytes: footer,
                                unique_resp_identifier: 0,
                                acceptance_id: 0,
                                cop_handle: Some(cop_handle),
                                cop_tag: cop_tag.clone(),
                                // Synthetic fast-init response: no real
                                // RxStatus to derive this from, so always a
                                // Normal Message (ADR-098).
                                rx_status_flags: 0,
                                // Never a KWP Access Timing Parameter
                                // exchange (ADR-146): fast-init keybyte
                                // delivery has no SID/TPI shape at all.
                                ecu_timing_change: false,
                                // Synthetic fast-init response: no real
                                // RxStatus to derive this from, so never
                                // SW-CAN-native (ADR-191).
                                sw_can_hv_rx: false,
                            }),
                        )
                        .await;
                    }
                }
            }
            Err(err) => {
                warn!(%err, "protocol init failed");
                // Guard (Err-arm `still_on_this_channel` check, Codex review
                // / design-advisor, PR #90 P1 fix, ADR-086): same race as
                // Guard B above, but on the failure path -- a
                // disconnect+reconnect can land while `run_protocol_init` was
                // in flight (or in the window right after it returns with an
                // error). Snapshotted before the revert-if-Temp below so the
                // revert itself (unconditional, per ADR-067 -- see below)
                // does not race a second, concurrent write to
                // `logical_links`.
                let still_on_this_channel = {
                    let links = ctx.logical_links.lock().await;
                    links.get(&cll_handle).is_some_and(|l| {
                        l.channel_id == Some(ctx.channel_id)
                            && l.connect_generation == connect_generation
                    })
                };
                if matches!(&binding, ParamBinding::Temp { .. }) {
                    // Revert hardware to the live Active set BEFORE failing
                    // the COP (ADR-067): a temp init transaction must not
                    // leave Working's config pushed to hardware after the
                    // COP ends. Unconditional regardless of staleness -- the
                    // temp config is physically on the hardware either way,
                    // and staleness cancels the COP's bookkeeping/status, not
                    // its hardware-cleanup obligation.
                    revert_hardware_to_live_active(
                        &ctx.api,
                        &ctx.logical_links,
                        cll_handle,
                        ctx.channel_id,
                        protocol_id,
                        &channel_wide_restore,
                    )
                    .await;
                }
                if !still_on_this_channel {
                    // Stale: the client has already seen PduCopstCancelled
                    // for this COP (from cancel_link_cops). Emitting
                    // PduErrEvtInitError + PduCopstFinished here would be a
                    // duplicate terminal status for an already-cancelled COP.
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstCancelled,
                    )
                    .await;
                    return;
                }
                send_error_event(
                    &ctx.subscriptions,
                    &ctx.logical_links,
                    cll_handle,
                    PduErrorEvent::PduErrEvtInitError,
                    Some((cop_handle, cop_tag.clone())),
                )
                .await;
                // ADR-128 correction (Codex review round 2): this emission
                // used to be unconditional -- guarded now, matching every
                // other terminal emitter in this file.
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstFinished,
                )
                .await;
                return;
            }
        }
    } else if resources::is_j1939_protocol_id(protocol_id)
        && j1939_claim_requested(
            binding
                .resolved()
                .unum32
                .get(&PARAM_J1939_ADDR_NEG_RULE)
                .copied(),
        )
    {
        // ADR-179 Decision 3 (amended, ADR-180): `PDU_COPT_STARTCOMM`
        // drives the SAE J1939 address-claim retry loop when
        // `CP_J1939AddressNegotiationRule` bit 1 requests it (the spec
        // default). Any optional CoptStartcomm message (`tx`, resolved by
        // `rpc_start_com_primitive` from a non-empty `cop_data`) was
        // ORIGINALLY composed at `StartComPrimitive` call time (ADR-067),
        // before this claim loop ever ran -- using whatever `NODE_ADDRESS`
        // was active then (the pre-claim default), since J1939's outbound
        // framing (`tx_header::j1939_header_bytes`) needs the CLL's OWN
        // claimed source address, not yet known at that point. A prior
        // revision of this branch discarded `tx` entirely rather than send
        // it with the wrong source address (Codex review, PR #72 round 3):
        // the `Claimed` arm below now instead RECOMPOSES `tx`'s header with
        // the just-claimed address before falling through to the shared
        // `if let Some(tx) = tx` transmit sequence a few dozen lines below
        // this whole `if`/`else if` chain -- see that arm's own comment.
        {
            // Codex review finding (PR #72 round 4, P1): a StopComm ->
            // `CP_J1939PreferredAddress` change -> StartComm cycle used to
            // reset only this CLL's own local markers below, leaving any
            // address it had already claimed still registered in
            // `SharedChannel::j1939_claims` and still defended by the
            // adapter -- cancellation previously only ever ran from
            // disconnect/destroy teardown or a failed attempt, neither of
            // which this cycle triggers. ADR-180's "every issued
            // `PROTECT_J1939_ADDR` claim has exactly one cancel owner"
            // invariant did not yet enumerate this case; a fresh claim
            // attempt starting here is now that address's cancel owner.
            // Cancel and deregister via the same `cancel_j1939_claims_for_
            // cll` helper the disconnect/destroy teardown paths use --
            // there is at most one address to cancel (`j1939_claimed_
            // address` is a single `Option<u8>`), but this queries
            // `SharedChannel::j1939_claims` directly (the source of
            // truth), so it is correct even if the local marker were
            // somehow already stale. ADR-080-conformant order:
            // `shared_channels` first, nested `api` for the native cancel,
            // released before the `logical_links` acquisition below (never
            // held simultaneously, mirroring this loop's own critical
            // sections in `events_j1939_claim.rs`).
            let mut chans = ctx.service.shared_channels.lock().await;
            if let Some(sc) = chans
                .values_mut()
                .find(|sc| sc.channel_id == ctx.channel_id)
            {
                let api = ctx.api.lock().await;
                cancel_j1939_claims_for_cll(
                    cll_handle,
                    connect_generation,
                    ctx.channel_id,
                    &api,
                    sc,
                )
                .await;
            }
            drop(chans);

            // Fresh attempt: reset the cursor to 0 (ADR-179 Decision 3 --
            // "A later StartComm after StopComm... simply re-runs the claim
            // loop from cursor = 0"). Also clear `j1939_claimed_address` --
            // edge-case-hunter finding: leaving a prior claim's address in
            // place let a late `_LOST` indication for that STALE address
            // (arriving after this fresh StartComm already started a new
            // claim attempt) still match `deliver_j1939_claim_indication`'s
            // `spontaneous_loss` check, spuriously arming `SharedChannel::
            // j1939_reclaim_pending` for this CLL and firing a redundant,
            // client-unrequested second claim negotiation once the fresh
            // loop below legitimately succeeds. Clearing it here removes
            // the stale match target entirely -- `run_j1939_claim_loop`
            // below sets it again itself on a fresh `Claimed` outcome.
            let mut links = ctx.logical_links.lock().await;
            if let Some(link) = links.get_mut(&cll_handle).filter(|l| {
                l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
            }) {
                link.j1939_claim_cursor = 0;
                link.j1939_claimed_address = None;
                // ADR-180 Decision 18: this whole `else if` branch is only
                // entered when this CoptStartcomm itself requested
                // negotiation (the branch's own `j1939_claim_requested(...)`
                // gate above), so `Engaged` is established structurally here
                // -- no need to re-derive it. Set before `run_j1939_claim_
                // loop` is awaited below so the posture is already live
                // during the in-flight-claim wait window, closing the spoof
                // window a Temp-bound `CoptSendrecv` could otherwise exploit
                // by staging a Working-only `CP_J1939AddressNegotiationRule`
                // opt-out while this CLL is still unclaimed.
                link.j1939_negotiation_posture = J1939NegotiationPosture::Engaged;
            }
        }

        // ADR-180 Decision 10 (design-advisor consult, PR #72 round 9): this
        // fresh attempt just relinquished (via `cancel_j1939_claims_for_cll`
        // above) any address this CLL previously claimed -- stop its own
        // live repeat slots now too, mirroring `deliver_j1939_claim_
        // indication`'s identical spontaneous-loss handling. See
        // `stop_repeat_slots_for_cll`'s own doc comment for the full
        // rationale; unconditional and idempotent (a no-op when this CLL
        // owns no repeat slots), same shape as the cancel call above.
        let failed = stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
        record_leaked_repeat_slots(ctx, failed).await;

        // ADR-180 Decision 11 (design-advisor consult, PR #72 round 10):
        // same relinquishment, a different mechanism -- also cancel any
        // live multi-cycle/cyclic CoptSendrecv still transmitting under the
        // address this fresh attempt just relinquished. See
        // `cancel_send_recv_cops_for_cll`'s own doc comment.
        cancel_send_recv_cops_for_cll(cll_handle, connect_generation, ctx).await;

        let claim_params = resolve_j1939_claim_params(binding.resolved());
        let claim_outcome = run_j1939_claim_loop(
            cll_handle,
            connect_generation,
            Some(cop_handle),
            &claim_params,
            ctx,
        )
        .await;
        match claim_outcome {
            J1939ClaimLoopOutcome::Claimed(addr) => {
                // CP_TesterSourceAddress write-back (ADR-179 Decision 3),
                // mirroring the CP_Baudrate write-back above: buffer-only,
                // both Working and Active (avoiding a spurious
                // PDU_ERR_TEMPPARAM_NOT_ALLOWED on a later temp_param_update,
                // the same rationale that write-back's own comment gives).
                // Defense-in-depth `still_on_this_channel` filter, same
                // TOCTOU-closing shape as the DATA_RATE write-back.
                //
                // ADR-180 Decision 13 (design-advisor consult, PR #72 round
                // 11): `shared_channels` is held across this whole writeback
                // AND the repeat-slot sweep below (`claimed_here`'s `if`
                // block), serializing against `ioctl_start_repeat_message`'s
                // own `shared_channels` hold, which already spans its entire
                // check-native-call-register sequence (its Bug-1 fix, see
                // that function's own doc comment). This closes the round-11
                // finding: `PDU_IOCTL_START_REPEAT_MESSAGE` requires only a
                // connected CLL, so a slot could be created using a stale
                // pre-claim/relinquished source address in the window
                // between this claim attempt starting and this writeback
                // running, and neither this arm's tester-present/`tx`
                // recompose above nor `stop_repeat_slots_for_cll`'s own
                // pre-claim sweep (this fresh attempt's own reset, a few
                // dozen lines above this whole `if`/`else if` chain) ever
                // revisited it. With both sides serialized on
                // `shared_channels`, a racing START either fully completes
                // (registers its new MsgId in `repeat_message_ids`) before
                // this sweep runs -- caught and stopped below -- or waits
                // until after this critical section releases the lock, at
                // which point `j1939_claimed_address` is already the fresh
                // address and the slot is created correctly under it.
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
                        // ADR-180 (Codex review, PR #72 round 3): `tx.send.data`
                        // (if present) was composed at `StartComPrimitive` call
                        // time against the pre-claim default source address --
                        // recompose its 5-byte `j1939_header_bytes` prefix now,
                        // keeping the payload bytes after it unchanged. The
                        // shared transmit sequence a few dozen lines below this
                        // whole `if`/`else if` chain sends `tx` (for every
                        // protocol) whenever it is `Some`, so this is the only
                        // place J1939 needs to touch it.
                        //
                        // Codex review finding (PR #72 round 14): the
                        // recomposition used to read every OTHER header field
                        // (target address, PDU format/specific, data page,
                        // priority) from `link.active` unconditionally --
                        // correct for a Plain-bound `CoptStartcomm`, but wrong
                        // for a `temp_param_update` one, whose original
                        // composition (at call time, via `build_tx_message` ->
                        // `j1939_header_bytes`) instead resolved those same
                        // fields from `binding.resolved()`'s Working (`effective`)
                        // snapshot (ADR-067). Reading Active here silently
                        // discarded the Temp-bound values for every field but
                        // the source address, potentially addressing the wrong
                        // ECU/PGN/priority even though the claimed source
                        // address itself was correct. `binding.resolved()` is
                        // the SAME `ComParamSet` reference the original
                        // composition used -- `addr` (the only piece of
                        // information this claim loop actually adds) is still
                        // passed as the explicit source-address argument, not
                        // read from `binding`, so this only replaces WHICH
                        // params the other four fields come from, not the
                        // source byte itself.
                        if let Some(tx) = tx.as_mut() {
                            let mut fresh = tx_header::j1939_header_bytes(addr, binding.resolved());
                            if tx.send.data.len() >= fresh.len() {
                                fresh.extend_from_slice(&tx.send.data[fresh.len()..]);
                                tx.send.data = fresh;
                            }
                        }
                        // Codex review finding (PR #72 round 6): `tester_present_data`
                        // (built above -- before this claim loop ever ran -- from
                        // the pre-claim default source address) has the identical
                        // staleness problem as `tx.send.data` just above. For
                        // J1939, `frame_tester_present_data`'s catch-all arm
                        // (J1939 never uses software ISO-TP, so its dedicated
                        // framing arm never applies) just clones `ResolvedTesterPresent::
                        // data` unchanged, itself composed at `StartComPrimitive`
                        // call time via the same `tx_header::build_tx_message` ->
                        // `j1939_header_bytes` path `tx.send.data` uses -- but
                        // UNLIKE `tx.send.data` (see the round-14 fix just above,
                        // and its own comment), `resolve_tester_present`'s own
                        // call site is deliberately always bound to Active, never
                        // Working -- ADR-067 claim C: periodic tester-present is a
                        // persistent product of this COP that outlives the
                        // transient init transaction, so `temp_param_update`
                        // stays a no-op for it. `link.active` here is therefore
                        // still the correct source for the OTHER header fields
                        // (verified directly against `rpc_primitive.rs`'s
                        // `resolve_tester_present(..., &bound_active, ...)` call
                        // site during round-14 triage, after an earlier draft of
                        // this fix mistakenly widened this arm to
                        // `binding.resolved()` too, which would have wrongly
                        // pulled Working's fields into a persistent product that
                        // must never see them). Recompose the same way this arm
                        // always has, before it is cached into
                        // `TesterPresentState::Armed::framed_data` further down
                        // (ADR-083: framed once at arm time, not re-framed per
                        // tick) -- otherwise a short tester-present frame
                        // transmits under the wrong (pre-claim default) source
                        // address, and one over 8 bytes can fail outright with
                        // `ERR_ADDRESS_NOT_CLAIMED`.
                        if !tester_present_data.is_empty() {
                            let mut fresh = tx_header::j1939_header_bytes(addr, &link.active);
                            if tester_present_data.len() >= fresh.len() {
                                fresh.extend_from_slice(&tester_present_data[fresh.len()..]);
                                tester_present_data = fresh;
                            }
                        }
                        true
                    } else {
                        false
                    }
                };
                if claimed_here {
                    // ADR-180 Decision 13: sweep any repeat slot a client
                    // raced into existence during this claim attempt's own
                    // pending window -- see this arm's own `shared_channels`
                    // doc comment above for the full serialization rationale.
                    let failed =
                        stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
                    push_leaked_repeat_slots(&mut chans, ctx.channel_id, failed);
                }
                drop(chans);
                // ADR-180 Decision 9: this COP's own claim loop just
                // succeeded -- from here on, an early return from the
                // shared optional-message transmit sequence below that
                // never reaches COMM_STARTED must cancel it (see that
                // flag's own declaration comment for why this is a local
                // per-COP fact, not a live-state check).
                j1939_claimed_this_cop = true;
                // Fall through to the shared tail below (COMM_STARTED/
                // Finished) -- and, if `tx` carries an optional message, the
                // shared transmit sequence reachable after this whole
                // `if`/`else if` chain closes.
            }
            J1939ClaimLoopOutcome::Exhausted => {
                // Mirrors this function's existing K-line init-failure
                // pattern (PduErrEvtInitError + PduCopstFinished) -- the
                // brief's own "use whatever this function's existing
                // 'StartComm failed' pattern is" instruction; address-claim
                // exhaustion is this protocol's analogue of a failed
                // protocol initialisation.
                //
                // `still_on_this_channel` guard (edge-case-hunter finding,
                // PR #72 round 4): mirrors the K-line `Err` arm's own guard
                // above -- `run_j1939_claim_loop` can return `Exhausted`
                // after a `DisconnectComLogicalLink`/`DestroyComLogicalLink`
                // already ran `cancel_link_cops` for this COP (which
                // already emitted `PduCopstCancelled`); every sibling arm
                // in this match (`Stale`/`HardError`) already accounts for
                // this race, this one previously did not. Snapshotted
                // before the revert-if-Temp below, same rationale as the
                // K-line arm.
                //
                // ADR-180 Decision 13 (design-advisor consult, PR #72 round
                // 11): `shared_channels` held across this snapshot and the
                // repeat-slot sweep just below, the same serialization
                // shape (and rationale) as the `Claimed` arm above -- an
                // exhausted claim attempt leaves this CLL with no claimed
                // address at all, so any repeat slot a client raced into
                // existence during this attempt's own pending window is
                // just as stale (framed against whatever address, if any,
                // was live before this attempt started) and needs the same
                // cleanup.
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
                if matches!(&binding, ParamBinding::Temp { .. }) {
                    revert_hardware_to_live_active(
                        &ctx.api,
                        &ctx.logical_links,
                        cll_handle,
                        ctx.channel_id,
                        protocol_id,
                        &channel_wide_restore,
                    )
                    .await;
                }
                if !still_on_this_channel {
                    // Stale: the client has already seen PduCopstCancelled
                    // for this COP (from cancel_link_cops). Emitting
                    // PduErrEvtInitError + PduCopstFinished here would be a
                    // duplicate terminal status for an already-cancelled
                    // COP.
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstCancelled,
                    )
                    .await;
                    return;
                }
                send_error_event(
                    &ctx.subscriptions,
                    &ctx.logical_links,
                    cll_handle,
                    PduErrorEvent::PduErrEvtInitError,
                    Some((cop_handle, cop_tag.clone())),
                )
                .await;
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstFinished,
                )
                .await;
                return;
            }
            J1939ClaimLoopOutcome::Stale => {
                if matches!(&binding, ParamBinding::Temp { .. }) {
                    revert_hardware_to_live_active(
                        &ctx.api,
                        &ctx.logical_links,
                        cll_handle,
                        ctx.channel_id,
                        protocol_id,
                        &channel_wide_restore,
                    )
                    .await;
                }
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstCancelled,
                )
                .await;
                return;
            }
            J1939ClaimLoopOutcome::Cancelled => {
                // ADR-180 Decision 21 (design-advisor consult, Codex review
                // PR #72): this COP's own explicit `CancelComPrimitive`
                // aborted the claim loop's wait -- mirrors `P3GapOutcome::
                // Cancelled`'s identical call site below, but deliberately
                // does NOT call `cancel_j1939_claim_after_failed_startcomm`:
                // `j1939_claimed_this_cop` is still `false` at this point (it
                // is only ever set `true` by the `Claimed` arm above, which
                // this match arm is mutually exclusive with), and
                // `run_j1939_claim_loop` already cancelled its own in-flight
                // candidate internally before returning `Cancelled` (see that
                // outcome's own doc comment). `j1939_negotiation_posture`
                // (set to `Engaged` earlier in this branch) is deliberately
                // left untouched here too, mirroring how the `Exhausted` arm
                // above already leaves it untouched -- posture is only
                // re-decided by this CLL's next real StartComm (ADR-180
                // Decision 18).
                if matches!(&binding, ParamBinding::Temp { .. }) {
                    revert_hardware_to_live_active(
                        &ctx.api,
                        &ctx.logical_links,
                        cll_handle,
                        ctx.channel_id,
                        protocol_id,
                        &channel_wide_restore,
                    )
                    .await;
                }
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstCancelled,
                )
                .await;
                return;
            }
            J1939ClaimLoopOutcome::HardError => {
                // handle_channel_hard_error already emitted the full
                // loss-of-comms sequence (including PduCopstCancelled for
                // this COP) and took the CLL offline -- mirrors
                // `P3GapOutcome::HardError`'s identical call site above.
                return;
            }
            J1939ClaimLoopOutcome::StopCommPending => unreachable!(
                "ADR-180 Decision 24: LogicalLinkState::stop_comm_pending is only ever set \
                 while comm_started == true (rpc_primitive.rs), and PDU_COPT_STARTCOMM is \
                 rejected while comm_started is already true -- so this branch's own \
                 Some(cop_handle) call into run_j1939_claim_loop can never observe it; only \
                 run_j1939_reclaim_duties's cancel_cop = None call site can (see that \
                 function's own match arm)"
            ),
        }
    } else if resources::is_j1939_protocol_id(protocol_id) {
        // ADR-180 Decision 18: this CoptStartcomm is on a J1939 CLL but did
        // NOT request address negotiation (the branch above's own
        // `j1939_claim_requested(...)` conjunct is what routes a
        // negotiation-requesting StartComm there instead). A StopComm ->
        // `CP_J1939AddressNegotiationRule` disable -> StartComm cycle would
        // otherwise leave `j1939_negotiation_posture` stuck at whatever a
        // PRIOR, negotiation-requesting StartComm on this same CLL last set
        // it to (`Engaged`) -- neither this field nor `j1939_claimed_address`/
        // `j1939_claim_cursor` are cleared by StopComm itself (see this
        // field's own doc comment: reset only at the next `ConnectComLogical
        // Link` finalization, mirroring Decision 17's reconnect-only
        // reasoning), so without this branch a CLL that genuinely opts back
        // OUT of negotiation via a real StartComm would keep failing every
        // subsequent Temp-bound opt-out send it should legitimately be
        // allowed to make.
        //
        // Codex review finding (PR #72 round 16), closing Decision 18's own
        // documented accepted residual: clearing `j1939_negotiation_posture`
        // alone left the PRIOR claim -- native defense, `SharedChannel::
        // j1939_claims` registration, and `j1939_claimed_address` -- fully
        // intact, since only the negotiation-requesting branch above ever
        // called `cancel_j1939_claims_for_cll`. A CLL opting out this way
        // kept consuming a claim slot the adapter went on defending forever,
        // and could never promote a different `CP_TesterSourceAddress` of
        // its own afterward, since Decision 12's own live-claim-membership
        // guard would keep rejecting it. This branch now mirrors the
        // negotiation-requesting branch's own "fresh attempt" cancel
        // (Decision 4) exactly -- same `shared_channels`-first,
        // nested-`api`-for-the-native-cancel, ADR-080-conformant order,
        // released before the `logical_links` acquisition below -- since
        // opting OUT of negotiation is just as much a relinquishment of any
        // address this CLL previously held as a fresh negotiation attempt
        // is.
        //
        // Later correction (mirror-image gap, design-advisor consult, PR #72
        // review): recording the posture as `OptedOut` here (rather than
        // resetting it to `Engaged`'s two-state-`bool` opposite, `false` /
        // "unset") is what actually fixes this branch's remaining bug --
        // before this correction, a `temp_param_update=1` StartComm reaching
        // this branch left the posture reading as `Undecided` to
        // `j1939_negotiated_unclaimed_for`, which then fell back to
        // `link.active`'s STALE (never-updated-off-default)
        // negotiation-ENABLED value and wrongly treated this CLL as still
        // negotiated-and-unclaimed for every later ordinary (non-Temp)
        // `CoptSendrecv`, `PDU_IOCTL_START_REPEAT_MESSAGE`, and the
        // tester-present filter -- permanently blocking the CLL despite this
        // branch's real, structural opt-out. `OptedOut` records that this
        // exact real StartComm decided "no negotiation" just as
        // authoritatively as the `Engaged` branch above records "yes", so
        // the predicate stops consulting the stale snapshot once this branch
        // has run. This branch's cancel/stop-sweep logic above is otherwise
        // unchanged by this correction -- a real opt-out is still exactly as
        // much a relinquishment of any previously-held address as before.
        // ADR-180 Decision 18's round-24 correction (Codex review, PR #72;
        // `design-advisor` consult): `cancel_j1939_claims_for_cll` discards
        // its own success/failure signal (its settled shape always removes
        // the routing entry, tracking a failed cancel in `SharedChannel::
        // leaked_j1939_claims` instead of returning it) -- this branch used
        // to proceed straight to `OptedOut` regardless. Unlike the
        // negotiation-requesting branch above, this opt-out branch never
        // calls `run_j1939_claim_loop`, so it never revisits round-23's own
        // reconcile-then-gate for `leaked_j1939_claims` -- an opted-out CLL
        // may never claim an address again, so nothing else would ever
        // reconcile a leak this branch itself just caused. Without this
        // check, a failed native cancel here would still let this StartComm
        // complete and the client start transmitting under a NEW
        // client-managed `CP_TesterSourceAddress` while the adapter may
        // still defend the OLD address under the same NAME -- exactly the
        // violation round-23 already closed for the negotiated path,
        // reopened here for the opt-out one. Reconciles the WHOLE channel's
        // leaked set, not just this CLL's own just-relinquished address, for
        // the identical reason round-23 went channel-wide:
        // `leaked_j1939_claims` carries no NAME/owner attribution, so a
        // same-NAME reconnect under a different `cll_handle` would evade a
        // narrower, this-address-only check. Retried inline (not a bare
        // check) via the shared [`reconcile_leaked_j1939_claims`] helper
        // (extracted from round-23's own block, `events_j1939_claim.rs`)
        // because an opt-out-bound CLL never re-enters `run_j1939_claim_
        // loop` -- a channel left with only opt-out CLLs would otherwise
        // have no reconciliation owner at all for a pre-existing leak.
        let leaked_remains = {
            let mut chans = ctx.service.shared_channels.lock().await;
            if let Some(sc) = chans
                .values_mut()
                .find(|sc| sc.channel_id == ctx.channel_id)
            {
                let api = ctx.api.lock().await;
                cancel_j1939_claims_for_cll(
                    cll_handle,
                    connect_generation,
                    ctx.channel_id,
                    &api,
                    sc,
                )
                .await;
                !reconcile_leaked_j1939_claims(sc, &api, ctx.channel_id, cll_handle)
            } else {
                // The physical channel itself is gone -- teardown/hard-error
                // already owns cleanup for this CLL, same as every other
                // "channel vanished mid-op" site in this function.
                false
            }
        };

        // `still_on_this_channel`/`connect_generation` filter matches every
        // other write site in this function. `j1939_claimed_address` is
        // cleared unconditionally either way: the routing entry is already
        // gone regardless of `leaked_remains` (the settled `cancel_j1939_
        // claims_for_cll` shape), and a stale `Some(..)` here would let a
        // later send frame under the very address this branch just tried to
        // relinquish -- worse than clearing it. `j1939_negotiation_posture`
        // is the actual gate: left untouched (not `OptedOut`) when a leak
        // survives, so `j1939_negotiated_unclaimed_for` keeps every
        // send/repeat/tester-present gate closed for this CLL exactly as it
        // would for a CLL that never opted out at all.
        let mut links = ctx.logical_links.lock().await;
        if let Some(link) = links.get_mut(&cll_handle).filter(|l| {
            l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
        }) {
            link.j1939_claim_cursor = 0;
            link.j1939_claimed_address = None;
            if !leaked_remains {
                link.j1939_negotiation_posture = J1939NegotiationPosture::OptedOut;
            }
        }
        drop(links);

        // Same relinquishment, the same two sibling mechanisms Decision
        // 10/11 already apply everywhere else this bug class relinquishes
        // an address -- see those Decisions' own doc comments a few dozen
        // lines above this whole `if`/`else if` chain. Runs regardless of
        // `leaked_remains`: the relinquishment already happened (Decision 10
        // anchors the violation at relinquishment, not at StartComm
        // success), so any repeat slot/live CoptSendrecv this CLL held under
        // the old address needs the same cleanup either way.
        let failed = stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
        record_leaked_repeat_slots(ctx, failed).await;
        cancel_send_recv_cops_for_cll(cll_handle, connect_generation, ctx).await;

        if leaked_remains {
            // round-24 correction: fail this StartComm closed instead of
            // completing it, mirroring the negotiation-requesting branch's
            // own `J1939ClaimLoopOutcome::Exhausted` arm emission shape
            // above (`still_on_this_channel` snapshot first, per that arm's
            // own documented ordering, then the Temp revert, then the
            // terminal status) -- and, critically, `return`s before the
            // shared optional-message transmit sequence below (`tx`), which
            // would otherwise send under this CLL's new client-managed
            // source address while the adapter may still defend the
            // relinquished one (Decision 3's own precedent: a failed
            // StartComm never sends its optional message).
            let still_on_this_channel = {
                let links = ctx.logical_links.lock().await;
                links.get(&cll_handle).is_some_and(|l| {
                    l.channel_id == Some(ctx.channel_id)
                        && l.connect_generation == connect_generation
                })
            };
            if matches!(&binding, ParamBinding::Temp { .. }) {
                revert_hardware_to_live_active(
                    &ctx.api,
                    &ctx.logical_links,
                    cll_handle,
                    ctx.channel_id,
                    protocol_id,
                    &channel_wide_restore,
                )
                .await;
            }
            if !still_on_this_channel {
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstCancelled,
                )
                .await;
                return;
            }
            send_error_event(
                &ctx.subscriptions,
                &ctx.logical_links,
                cll_handle,
                PduErrorEvent::PduErrEvtInitError,
                Some((cop_handle, cop_tag.clone())),
            )
            .await;
            emit_terminal_if_live(
                &ctx.primitives,
                &ctx.logical_links,
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
                cll_handle,
                cop_handle,
                PduComPrimitiveStatus::PduCopstFinished,
            )
            .await;
            return;
        }
    } else if resources::is_tp2_0_family_protocol_id(protocol_id) {
        // ADR-188/Phase 7 Stage 7a, Decision item 2: clause 19 defines no
        // COP-borne initialization payload at all -- the inverse of UART
        // Echo Byte's own mandatory-one-byte contract (ADR-170 Decision).
        // ADR-210 Decision item 12: this entry arm was re-keyed from the
        // narrow `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id` --
        // without this, a `_CHx`-connected TP2.0 link's `CoptStartcomm`
        // would never initiate a connection request at all, silently
        // falling through to whatever generic handling follows this
        // else-if chain.
        // `tx` is `Some` only for a non-empty `cop_data` (ADR-111); reject
        // outright rather than silently discarding it, mirroring this
        // function's existing "StartComm failed" pattern (PduErrEvtInitError
        // + PduCopstFinished) every other init-failure arm in this chain
        // uses.
        // Shared cop_data rejection (Stage 7a's active arm and Stage 7b's
        // passive arm alike -- clause 19 defines no COP-borne payload for
        // either).
        if tx.is_some() {
            warn!(
                cll_handle,
                "TP2.0 CoptStartcomm carries no COP-borne initialization payload"
            );
            if matches!(&binding, ParamBinding::Temp { .. }) {
                revert_hardware_to_live_active(
                    &ctx.api,
                    &ctx.logical_links,
                    cll_handle,
                    ctx.channel_id,
                    protocol_id,
                    &channel_wide_restore,
                )
                .await;
            }
            send_error_event(
                &ctx.subscriptions,
                &ctx.logical_links,
                cll_handle,
                PduErrorEvent::PduErrEvtInitError,
                Some((cop_handle, cop_tag.clone())),
            )
            .await;
            emit_terminal_if_live(
                &ctx.primitives,
                &ctx.logical_links,
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
                cll_handle,
                cop_handle,
                PduComPrimitiveStatus::PduCopstFinished,
            )
            .await;
            return;
        }

        // ADR-190/Phase 7 Stage 7b section 1: passive-arm selection, checked
        // before Stage 7a's own active-connection param resolution --
        // "either passive ComParam staged routes to the new passive arm."
        let passive_params = match resolve_tp20_passive_params(binding.resolved()) {
            Ok(passive_params) => passive_params,
            Err(err) => {
                warn!(cll_handle, %err, "TP2.0 CoptStartcomm passive-connection params rejected");
                if matches!(&binding, ParamBinding::Temp { .. }) {
                    revert_hardware_to_live_active(
                        &ctx.api,
                        &ctx.logical_links,
                        cll_handle,
                        ctx.channel_id,
                        protocol_id,
                        &channel_wide_restore,
                    )
                    .await;
                }
                send_error_event(
                    &ctx.subscriptions,
                    &ctx.logical_links,
                    cll_handle,
                    PduErrorEvent::PduErrEvtInitError,
                    Some((cop_handle, cop_tag.clone())),
                )
                .await;
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstFinished,
                )
                .await;
                return;
            }
        };

        if let Some((identifier, rx_id_passive)) = passive_params {
            // Stage 7a's own Fix Q dispatch-time `comm_started` recheck
            // applies to this arm identically (ADR-190 section 1): a second
            // concurrent `CoptStartcomm` racing the first must still be
            // rejected, not overwrite it.
            let (already_started, still_on_this_channel) = {
                let links = ctx.logical_links.lock().await;
                match links.get(&cll_handle).filter(|l| {
                    l.channel_id == Some(ctx.channel_id)
                        && l.connect_generation == connect_generation
                }) {
                    Some(link) => (link.comm_started, true),
                    None => (false, false),
                }
            };
            if !still_on_this_channel {
                if matches!(&binding, ParamBinding::Temp { .. }) {
                    revert_hardware_to_live_active(
                        &ctx.api,
                        &ctx.logical_links,
                        cll_handle,
                        ctx.channel_id,
                        protocol_id,
                        &channel_wide_restore,
                    )
                    .await;
                }
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstCancelled,
                )
                .await;
                return;
            }
            if already_started {
                if matches!(&binding, ParamBinding::Temp { .. }) {
                    revert_hardware_to_live_active(
                        &ctx.api,
                        &ctx.logical_links,
                        cll_handle,
                        ctx.channel_id,
                        protocol_id,
                        &channel_wide_restore,
                    )
                    .await;
                }
                send_error_event(
                    &ctx.subscriptions,
                    &ctx.logical_links,
                    cll_handle,
                    PduErrorEvent::PduErrEvtInitError,
                    Some((cop_handle, cop_tag.clone())),
                )
                .await;
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstFinished,
                )
                .await;
                return;
            }

            let arm_outcome = arm_tp20_passive_listener(
                cll_handle,
                connect_generation,
                identifier,
                rx_id_passive,
                ctx,
            )
            .await;
            match arm_outcome {
                Tp20PassiveArmOutcome::Armed => {
                    // Fall through to the shared tail below (COMM_STARTED/
                    // Finished) -- `tx` is always `None` on this branch, so
                    // the shared `if let Some(tx) = tx` transmit sequence a
                    // few dozen lines below this whole `if`/`else if` chain
                    // is a no-op for it, exactly like the active arm's own
                    // `Established` outcome.
                }
                Tp20PassiveArmOutcome::SlotBusy | Tp20PassiveArmOutcome::RxIdInUse => {
                    if matches!(&binding, ParamBinding::Temp { .. }) {
                        revert_hardware_to_live_active(
                            &ctx.api,
                            &ctx.logical_links,
                            cll_handle,
                            ctx.channel_id,
                            protocol_id,
                            &channel_wide_restore,
                        )
                        .await;
                    }
                    send_error_event(
                        &ctx.subscriptions,
                        &ctx.logical_links,
                        cll_handle,
                        PduErrorEvent::PduErrEvtRscLocked,
                        Some((cop_handle, cop_tag.clone())),
                    )
                    .await;
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstFinished,
                    )
                    .await;
                    return;
                }
                Tp20PassiveArmOutcome::NativeFailed => {
                    if matches!(&binding, ParamBinding::Temp { .. }) {
                        revert_hardware_to_live_active(
                            &ctx.api,
                            &ctx.logical_links,
                            cll_handle,
                            ctx.channel_id,
                            protocol_id,
                            &channel_wide_restore,
                        )
                        .await;
                    }
                    send_error_event(
                        &ctx.subscriptions,
                        &ctx.logical_links,
                        cll_handle,
                        PduErrorEvent::PduErrEvtInitError,
                        Some((cop_handle, cop_tag.clone())),
                    )
                    .await;
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstFinished,
                    )
                    .await;
                    return;
                }
                Tp20PassiveArmOutcome::Stale => {
                    if matches!(&binding, ParamBinding::Temp { .. }) {
                        revert_hardware_to_live_active(
                            &ctx.api,
                            &ctx.logical_links,
                            cll_handle,
                            ctx.channel_id,
                            protocol_id,
                            &channel_wide_restore,
                        )
                        .await;
                    }
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstCancelled,
                    )
                    .await;
                    return;
                }
            }
        } else {
            // Stage 7a's own active-connection arm, unchanged below (ADR-188
            // Decision item 2).
            let params = match resolve_tp20_connection_params(binding.resolved()) {
                Ok(params) => params,
                Err(err) => {
                    // Codex review fix (PR #97): `resolve_tp20_connection_params`
                    // now distinguishes "a required ComParam is absent" from "a
                    // present value doesn't fit its native field width" -- this
                    // async COP-failure path has no client-visible error-string
                    // surface (unlike a synchronous RPC rejection), so the
                    // distinct message is logged here rather than discarded; the
                    // client-visible outcome (PduErrEvtInitError +
                    // PduCopstFinished) is unchanged for either sub-case.
                    warn!(cll_handle, %err, "TP2.0 CoptStartcomm connection params rejected");
                    if matches!(&binding, ParamBinding::Temp { .. }) {
                        revert_hardware_to_live_active(
                            &ctx.api,
                            &ctx.logical_links,
                            cll_handle,
                            ctx.channel_id,
                            protocol_id,
                            &channel_wide_restore,
                        )
                        .await;
                    }
                    send_error_event(
                        &ctx.subscriptions,
                        &ctx.logical_links,
                        cll_handle,
                        PduErrorEvent::PduErrEvtInitError,
                        Some((cop_handle, cop_tag.clone())),
                    )
                    .await;
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstFinished,
                    )
                    .await;
                    return;
                }
            };

            // Fresh attempt: record the Requested phase before issuing the
            // native call, mirroring the J1939 branch's own "fresh attempt"
            // reset above -- a StopComm -> re-StartComm cycle re-runs this from
            // scratch.
            //
            // Codex review fix (PR #97, round 13): recheck `comm_started` under
            // this SAME lock acquisition first -- two concurrent `CoptStartcomm`
            // RPCs on the SAME `cll_handle` can both pass `rpc_primitive.rs`'s
            // own pre-flight/TOCTOU-recheck precondition, since `comm_started`
            // is only ever set `true` at the very END of `handle_start_comm`'s
            // overall processing (Step 3, well after this point), so it stays
            // `false` for this entire connection-request's own wait. Both then
            // get queued as separate `TxItem::StartComm` entries, and this
            // codebase's single-poll-task-per-physical-channel serialization
            // runs them one after another rather than concurrently -- so by the
            // time a SECOND attempt reaches this point, a FIRST one may have
            // already run to completion and set `comm_started = true`.
            // Proceeding unconditionally would overwrite the first attempt's
            // own just-established `Tp20Connection` state with a fresh
            // `Requested`, then issue a SECOND native
            // `IOCTL_REQUEST_CONNECTION` for the identical `rx_id_proposal` --
            // rejected as a duplicate (`ERR_NOT_UNIQUE`, Fix N-1) -- leaving
            // `comm_started == true` but the service's own bookkeeping stuck at
            // `Requested`: `build_tx_message`'s TP2.0 arm then rejects every
            // send (it requires `Established` specifically), and
            // `handle_stop_comm`'s own TP2.0 teardown -- gated on `Established`
            // too -- never tears down the still-genuinely-active native
            // connection the FIRST attempt established, leaking one of the four
            // slots until the device's own maintenance timeout.
            let already_started = {
                let mut links = ctx.logical_links.lock().await;
                match links.get_mut(&cll_handle).filter(|l| {
                    l.channel_id == Some(ctx.channel_id)
                        && l.connect_generation == connect_generation
                }) {
                    Some(link) if link.comm_started => true,
                    Some(link) => {
                        link.tp20_connection = Some(Tp20Connection {
                            requested_rx_id: u32::from(params.rx_id_proposal),
                            established_tx_id: None,
                            phase: Tp20ConnectionPhase::Requested,
                            passive: false,
                        });
                        false
                    }
                    None => false,
                }
            };
            if already_started {
                // Codex review fix (PR #97, round 16): this is an async-COP
                // failure path like every other one in this function, and needs
                // the SAME `Temp` hardware-revert call they all make -- a
                // racing second `CoptStartcomm` with `temp_param_update = 1`
                // already pushed its own effective Working set to hardware (the
                // Temp apply at this function's own start, before any of this
                // dispatch-time work runs), so without this call an allowed
                // temporary setting (e.g. `CP_Loopback`) would stay active on
                // the FIRST attempt's own now-established connection even
                // though Active still holds the old value.
                if matches!(&binding, ParamBinding::Temp { .. }) {
                    revert_hardware_to_live_active(
                        &ctx.api,
                        &ctx.logical_links,
                        cll_handle,
                        ctx.channel_id,
                        protocol_id,
                        &channel_wide_restore,
                    )
                    .await;
                }
                send_error_event(
                    &ctx.subscriptions,
                    &ctx.logical_links,
                    cll_handle,
                    PduErrorEvent::PduErrEvtInitError,
                    Some((cop_handle, cop_tag.clone())),
                )
                .await;
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstFinished,
                )
                .await;
                return;
            }

            let request_outcome =
                run_tp20_connection_request(cll_handle, connect_generation, &params, ctx).await;
            match request_outcome {
                Tp20ConnectionRequestOutcome::Established(tx_id) => {
                    let still_on_this_channel = {
                        let mut links = ctx.logical_links.lock().await;
                        links
                            .get_mut(&cll_handle)
                            .filter(|l| {
                                l.channel_id == Some(ctx.channel_id)
                                    && l.connect_generation == connect_generation
                            })
                            .map(|link| {
                                // Codex review fix (PR #97): no ComParam
                                // write-back here (an earlier version overwrote
                                // `CP_TP20TxIdProposal` in place, mirroring ADR-179
                                // Decision 3's `CP_TesterSourceAddress` precedent).
                                // Unlike that J1939 precedent, `CP_TP20TxIdProposal`
                                // is itself the very ComParam a client can
                                // `SetComParam` to any value before this
                                // `CoptStartcomm` runs -- reusing it as BOTH a
                                // client-writable input (the proposal) and this
                                // write-back's own service-internal output (the
                                // established TX-ID) is exactly the double-duty
                                // ambiguity that made `tx_header::build_tx_message`'s
                                // old "read it back from the ComParam" design
                                // exploitable (a client-staged bogus value read as
                                // though it were the real assigned TX-ID). This
                                // struct's own `established_tx_id`/`phase` fields,
                                // set on `link.tp20_connection` just above, are now
                                // `build_tx_message`'s only source for the
                                // established TX-ID (threaded through explicitly by
                                // every call site -- see that function's own doc
                                // comment) -- ADR-188 itself never documented the
                                // write-back as an intentional `GetComParam`
                                // client-readback feature, so removing it closes
                                // the ambiguity at its root rather than leaving an
                                // unused-but-still-writable second source of truth.
                                link.tp20_connection = Some(Tp20Connection {
                                    requested_rx_id: u32::from(params.rx_id_proposal),
                                    established_tx_id: Some(tx_id),
                                    phase: Tp20ConnectionPhase::Established,
                                    passive: false,
                                });
                            })
                            .is_some()
                    };
                    if !still_on_this_channel {
                        // Codex review fix (PR #97, 8th round / Fix K): the
                        // connection genuinely became `Established` on the
                        // device side, but this CLL's owner disappeared
                        // (`Disconnect`/`DestroyComLogicalLink`) in the gap
                        // between `run_tp20_connection_request` returning and
                        // this write-back running -- the disconnect/destroy path
                        // itself only saw `tp20_connection` still in `Requested`
                        // phase at the time it ran (this write-back hadn't
                        // happened yet), so its own `phase == Established`-gated
                        // teardown correctly skipped this connection back then.
                        // Nothing else closes this gap, so best-effort tear down
                        // the now-orphaned native connection here, using the
                        // same helper the other two local-abandonment paths in
                        // `run_tp20_connection_request` use.
                        let rx_id = u32::from(params.rx_id_proposal);
                        best_effort_teardown_on_abandon(
                            ctx,
                            cll_handle,
                            rx_id,
                            "this CLL's owner disappeared between the connection establishing and \
                             the write-back running",
                        )
                        .await;
                        // Quarantine the RX-ID the same way `run_tp20_connection_
                        // request`'s own two abandonment paths do (Fix J, 7th
                        // round): this write-back site is conceptually a THIRD
                        // abandonment path -- without this, an immediate retry
                        // proposing the same RX-ID (on a reconnected/new CLL)
                        // could register a new, indistinguishable entry and have
                        // the teardown call's own delayed `CONNECTION_LOST`
                        // indication (or any other stray one) misattributed to
                        // it, exactly the bug Fix J closed for the other two
                        // paths. See `quarantine_tp20_connection_for_orphaned_
                        // write_back`'s own doc comment for why this is a
                        // vacant-only insert rather than a mark-in-place.
                        //
                        // Deliberately UNCONDITIONAL, not gated on whether the
                        // teardown call above succeeded (Codex review findings,
                        // PR #97, rounds 19 and 20 -- see `best_effort_teardown_
                        // on_abandon`'s own doc comment for the full account): a
                        // round-19 attempt to gate this on teardown success
                        // (mirroring Fix V, round 17) was reverted after a
                        // round-20 finding identified a genuine race the gate
                        // reopened -- the device may have independently and
                        // spontaneously lost this connection before this
                        // teardown call ever ran, in which case the call fails
                        // (no matching native slot) precisely BECAUSE an
                        // indication may already be in flight, not because none
                        // is coming. Quarantining unconditionally accepts the
                        // same leaked-native-slot residual `best_effort_
                        // teardown_on_abandon`'s own doc comment already
                        // documents, rather than risk that misattribution race.
                        {
                            let mut chans = ctx.service.shared_channels.lock().await;
                            if let Some(sc) = chans
                                .values_mut()
                                .find(|sc| sc.channel_id == ctx.channel_id)
                            {
                                quarantine_tp20_connection_for_orphaned_write_back(
                                    rx_id,
                                    cll_handle,
                                    connect_generation,
                                    &mut sc.tp20_connections,
                                );
                            }
                        }
                        emit_terminal_if_live(
                            &ctx.primitives,
                            &ctx.logical_links,
                            &ctx.subscriptions,
                            &ctx.service.terminal_cops,
                            cll_handle,
                            cop_handle,
                            PduComPrimitiveStatus::PduCopstCancelled,
                        )
                        .await;
                        return;
                    }
                    // Fall through to the shared tail below (COMM_STARTED/
                    // Finished) -- `tx` is always `None` on this branch (rejected
                    // above), so the shared `if let Some(tx) = tx` transmit
                    // sequence a few dozen lines below this whole `if`/`else if`
                    // chain is a no-op for it.
                }
                Tp20ConnectionRequestOutcome::Lost(reason) => {
                    // ADR-188 Decision item 2 step 5: map the reason byte to the
                    // closest existing `PduErrorEvent` this async COP-failure
                    // path can report (unlike a synchronous RPC rejection, this
                    // runs from the poll task -- `PduError::PduErrResourceBusy`,
                    // the Phase 13 ERR_*_IN_USE precedent the ADR itself cites,
                    // is only reachable from a synchronous call and has no
                    // `PduErrorEvent` counterpart; `PduErrEvtRscLocked` is the
                    // closest semantic match for "temporarily no resources are
                    // free"): `1` (timeout) and every other/unknown reason to
                    // the existing COP-timeout error shape (PduErrEvtInitError,
                    // mirroring this function's other init-failure arms);
                    // `0xD6`/`0xD7` to a not-supported rejection
                    // (PduErrEvtProtErr); `0xD8` to PduErrEvtRscLocked.
                    {
                        let mut links = ctx.logical_links.lock().await;
                        if let Some(link) = links.get_mut(&cll_handle).filter(|l| {
                            l.channel_id == Some(ctx.channel_id)
                                && l.connect_generation == connect_generation
                        }) {
                            link.tp20_connection = Some(Tp20Connection {
                                requested_rx_id: u32::from(params.rx_id_proposal),
                                established_tx_id: None,
                                phase: Tp20ConnectionPhase::Lost,
                                passive: false,
                            });
                        }
                    }
                    if matches!(&binding, ParamBinding::Temp { .. }) {
                        revert_hardware_to_live_active(
                            &ctx.api,
                            &ctx.logical_links,
                            cll_handle,
                            ctx.channel_id,
                            protocol_id,
                            &channel_wide_restore,
                        )
                        .await;
                    }
                    let still_on_this_channel = {
                        let links = ctx.logical_links.lock().await;
                        links.get(&cll_handle).is_some_and(|l| {
                            l.channel_id == Some(ctx.channel_id)
                                && l.connect_generation == connect_generation
                        })
                    };
                    if !still_on_this_channel {
                        emit_terminal_if_live(
                            &ctx.primitives,
                            &ctx.logical_links,
                            &ctx.subscriptions,
                            &ctx.service.terminal_cops,
                            cll_handle,
                            cop_handle,
                            PduComPrimitiveStatus::PduCopstCancelled,
                        )
                        .await;
                        return;
                    }
                    let error_event = match reason {
                        0xD6 | 0xD7 => PduErrorEvent::PduErrEvtProtErr,
                        0xD8 => PduErrorEvent::PduErrEvtRscLocked,
                        _ => PduErrorEvent::PduErrEvtInitError,
                    };
                    send_error_event(
                        &ctx.subscriptions,
                        &ctx.logical_links,
                        cll_handle,
                        error_event,
                        Some((cop_handle, cop_tag.clone())),
                    )
                    .await;
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstFinished,
                    )
                    .await;
                    return;
                }
                Tp20ConnectionRequestOutcome::Failed => {
                    if matches!(&binding, ParamBinding::Temp { .. }) {
                        revert_hardware_to_live_active(
                            &ctx.api,
                            &ctx.logical_links,
                            cll_handle,
                            ctx.channel_id,
                            protocol_id,
                            &channel_wide_restore,
                        )
                        .await;
                    }
                    send_error_event(
                        &ctx.subscriptions,
                        &ctx.logical_links,
                        cll_handle,
                        PduErrorEvent::PduErrEvtInitError,
                        Some((cop_handle, cop_tag.clone())),
                    )
                    .await;
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstFinished,
                    )
                    .await;
                    return;
                }
                Tp20ConnectionRequestOutcome::RxIdInUse => {
                    // Locally-detected equivalent of the native "no resources
                    // free" condition (the `0xD8` reason-byte arm above maps to
                    // `PduErrEvtRscLocked`, this async COP-failure path's own
                    // counterpart to the synchronous-RPC-only
                    // `PduError::PduErrResourceBusy` -- see that arm's own doc
                    // comment) -- this outcome is detected locally (the
                    // proposed RX-ID is already owned by a live sibling CLL's
                    // own still-pending request), never device-reported, but
                    // surfaces to the client identically since it is the same
                    // real-world condition either way.
                    if matches!(&binding, ParamBinding::Temp { .. }) {
                        revert_hardware_to_live_active(
                            &ctx.api,
                            &ctx.logical_links,
                            cll_handle,
                            ctx.channel_id,
                            protocol_id,
                            &channel_wide_restore,
                        )
                        .await;
                    }
                    send_error_event(
                        &ctx.subscriptions,
                        &ctx.logical_links,
                        cll_handle,
                        PduErrorEvent::PduErrEvtRscLocked,
                        Some((cop_handle, cop_tag.clone())),
                    )
                    .await;
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstFinished,
                    )
                    .await;
                    return;
                }
                Tp20ConnectionRequestOutcome::Stale => {
                    if matches!(&binding, ParamBinding::Temp { .. }) {
                        revert_hardware_to_live_active(
                            &ctx.api,
                            &ctx.logical_links,
                            cll_handle,
                            ctx.channel_id,
                            protocol_id,
                            &channel_wide_restore,
                        )
                        .await;
                    }
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstCancelled,
                    )
                    .await;
                    return;
                }
                Tp20ConnectionRequestOutcome::HardError => {
                    // handle_channel_hard_error already emitted the full
                    // loss-of-comms sequence (including PduCopstCancelled for
                    // this COP) and took the CLL offline.
                    return;
                }
            }
        }
    }

    // ADR-180 (Codex review, PR #72 round 3): a standalone `if`, not an
    // `else if` on the J1939 claim branch above -- `tx` is `Some` either
    // because this is a non-J1939 (or claim-not-requested) protocol's
    // ordinary optional CoptStartcomm message, or because a J1939 claim
    // branch above just `Claimed` an address and recomposed `tx.send.data`'s
    // header with it (that arm's own comment). `tx` is never `Some` when
    // `five_baud`/`fast_init` is (`OneShotCommTx`'s own doc comment), so this
    // split changes nothing for that branch; a J1939 `Exhausted`/`Stale`/
    // `HardError` outcome above already `return`ed before reaching here, so
    // the optional message is correctly never sent on a failed claim.
    if let Some(tx) = tx {
        // ISO 22900-2 §9.2.6.3.2 b) / Table 7 step 5 (ADR-111): the optional
        // CAN/J1850 CoptStartcomm request message. Unlike CoptStopcomm's
        // analogous final message (ADR-085/ADR-087, always `cancellable:
        // false`), nothing has committed/changed CLL state by this point --
        // this is fully cancellable (`cancellable: true` throughout).
        match wait_for_p3_gap(
            cop_handle,
            cll_handle,
            tx.send.can_functional,
            tx.num_receive_cycles,
            true,
            false,
            ctx,
        )
        .await
        {
            P3GapOutcome::Cancelled => {
                // ADR-180 Decision 9: this COP's own J1939 claim already
                // succeeded (if it requested one) -- the optional message
                // never sent, but the client will never see COMM_STARTED
                // either, so this is the terminating early return that must
                // cancel it (see `j1939_claimed_this_cop`'s own declaration
                // comment).
                if j1939_claimed_this_cop {
                    cancel_j1939_claim_after_failed_startcomm(cll_handle, connect_generation, ctx)
                        .await;
                }
                if matches!(&binding, ParamBinding::Temp { .. }) {
                    revert_hardware_to_live_active(
                        &ctx.api,
                        &ctx.logical_links,
                        cll_handle,
                        ctx.channel_id,
                        protocol_id,
                        &channel_wide_restore,
                    )
                    .await;
                }
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstCancelled,
                )
                .await;
                return;
            }
            P3GapOutcome::HardError => {
                // handle_channel_hard_error already emitted the full
                // loss-of-comms sequence (including PduCopstCancelled for
                // this COP) and took the CLL offline -- the physical channel
                // is gone, so no Temp-binding revert is attempted here
                // (mirrors handle_send_recv's `!channel_lost` gate).
                return;
            }
            P3GapOutcome::Deferred => unreachable!("defer_if_blocked = false"),
            P3GapOutcome::Ready => {
                let still_on_this_channel = {
                    let links = ctx.logical_links.lock().await;
                    links.get(&cll_handle).is_some_and(|l| {
                        l.channel_id == Some(ctx.channel_id)
                            && l.connect_generation == connect_generation
                    })
                };
                if !still_on_this_channel {
                    if matches!(&binding, ParamBinding::Temp { .. }) {
                        revert_hardware_to_live_active(
                            &ctx.api,
                            &ctx.logical_links,
                            cll_handle,
                            ctx.channel_id,
                            protocol_id,
                            &channel_wide_restore,
                        )
                        .await;
                    }
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstCancelled,
                    )
                    .await;
                    return;
                }

                match transmit_request(
                    cll_handle,
                    cop_handle,
                    protocol_id,
                    tx.send.tx_flags,
                    &tx.send.data,
                    tx.send.isotp_tx.as_ref(),
                    true,
                    true,
                    ctx,
                )
                .await
                {
                    Ok(()) => {
                        if let Some(functional) = tx.send.can_functional {
                            let bucket = if functional {
                                &ctx.last_func_tx
                            } else {
                                &ctx.last_phys_tx
                            };
                            *bucket.lock().await = Some(TxGapState {
                                at: tokio::time::Instant::now(),
                                no_response_required: tx.num_receive_cycles == 0,
                            });
                        }

                        // S3-equivalent staleness guard (mirroring
                        // handle_stop_comm's identical post-transmit
                        // recheck): transmit_request performs real I/O and
                        // can span a disconnect+reconnect completing during
                        // the call.
                        let still_on_this_channel = {
                            let links = ctx.logical_links.lock().await;
                            links.get(&cll_handle).is_some_and(|l| {
                                l.channel_id == Some(ctx.channel_id)
                                    && l.connect_generation == connect_generation
                            })
                        };
                        if !still_on_this_channel {
                            if matches!(&binding, ParamBinding::Temp { .. }) {
                                revert_hardware_to_live_active(
                                    &ctx.api,
                                    &ctx.logical_links,
                                    cll_handle,
                                    ctx.channel_id,
                                    protocol_id,
                                    &channel_wide_restore,
                                )
                                .await;
                            }
                            emit_terminal_if_live(
                                &ctx.primitives,
                                &ctx.logical_links,
                                &ctx.subscriptions,
                                &ctx.service.terminal_cops,
                                cll_handle,
                                cop_handle,
                                PduComPrimitiveStatus::PduCopstCancelled,
                            )
                            .await;
                            return;
                        }

                        // ADR-111: when the client asked for a response to
                        // this optional message (NumReceiveCycles != 0), run
                        // the same bounded receive phase CoptSendrecv/
                        // CoptStopcomm use -- cancellable this time (see the
                        // doc comment above). `0` (the default) skips this
                        // entirely, matching a receive-less CoptStartcomm
                        // message byte-for-byte.
                        if tx.num_receive_cycles != 0 {
                            match wait_for_expected_response(
                                ExpectedResponseWait {
                                    cll_handle,
                                    cop_handle,
                                    cop_tag: cop_tag.clone(),
                                    expected: &tx.expected_response,
                                    num_receive_cycles: tx.num_receive_cycles,
                                    timeout_ms: tx.response_timeout_ms,
                                    protocol_id,
                                    tx_flags: tx.send.tx_flags,
                                    original_data: &tx.send.data,
                                    isotp_tx: tx.send.isotp_tx.as_ref(),
                                    rc_cfg: &tx.rc_cfg,
                                    timing_cfg: tx.timing_cfg.as_deref(),
                                    connect_generation,
                                    cancellable: true,
                                    match_reset_ceiling_ms: None,
                                    created_receive_only: false,
                                    cyclic_resp_timeout_ms: 0,
                                    enable_concatenation: tx.enable_concatenation,
                                },
                                ctx,
                            )
                            .await
                            {
                                // `wait_for_expected_response` already emitted
                                // every required status event for every cause
                                // `Terminal` covers (see its own doc comment):
                                // mid-receive cancellation, disconnect/stale,
                                // AND hard channel error (`PollMatchResult::
                                // HardError` / an RC21-23 re-request's own
                                // `TxFailure::ChannelLost`) -- this call
                                // site's only remaining obligation, which that
                                // function knows nothing about, is the
                                // Temp-binding hardware revert (ADR-067).
                                //
                                // Deliberately UNCONDITIONAL, not gated by a
                                // `handle_send_recv`-style `!channel_lost`
                                // (ADR-111 amendment): `ReceivePhaseOutcome`
                                // does not expose which of the above caused
                                // this `Terminal` -- unlike `transmit_request`,
                                // whose `TxFailure::ChannelLost` variant IS
                                // checked explicitly at this same call site's
                                // earlier `Err` arm below (`handle_send_recv`
                                // derives its own `channel_lost` bool from
                                // that exact signal, which is local to it;
                                // `wait_for_expected_response` never surfaces
                                // an equivalent one to ITS caller). Every
                                // hard-error-caused `Terminal` here is
                                // produced only after `handle_channel_hard_
                                // error` has already run for THIS SAME
                                // `ctx.channel_id` (confirmed by reading
                                // `wait_for_expected_response_inner`: its sole
                                // `PollMatchResult::HardError` arm and both
                                // `TxFailure::ChannelLost` arms are reachable
                                // only via `poll_rx_inner`'s hard-error path,
                                // which runs `handle_channel_hard_error`
                                // before ever returning that outcome) -- the
                                // CLL is already marked offline
                                // (`channel_id`/`uudt_channel_id` cleared) and
                                // every COP on it, including this one, has
                                // already gotten its terminal status. The
                                // extra `SET_CONFIG` this revert issues in
                                // that sub-case is a real (harmless) call
                                // against an already-erroring physical
                                // channel -- `apply_params_to_hardware`
                                // swallows its own failure -- not merely
                                // "already a no-op" as an earlier draft of
                                // this comment claimed; it is wasteful, not
                                // unsafe. A post-hoc `still_on_this_channel`
                                // check cannot distinguish this sub-case from
                                // the non-hard-error stale/reconnect sub-case
                                // also folded into `Terminal` (both leave this
                                // CLL's `channel_id`/`connect_generation`
                                // mismatched against `ctx`), and that other
                                // sub-case DOES still need the revert (matches
                                // every other `!still_on_this_channel` guard
                                // in this same branch, e.g. the `P3GapOutcome
                                // ::Ready` arm a few dozen lines above, which
                                // reverts unconditionally on staleness too) --
                                // so narrowing this guard without a real
                                // hard-error-vs-stale signal from
                                // `wait_for_expected_response` would silently
                                // break that sibling case instead. Accepted as
                                // a documented, deliberate simplification
                                // (see ADR-111's Consequences section) rather
                                // than widening `ReceivePhaseOutcome`'s
                                // contract for every caller on the strength of
                                // an untestable change -- this crate's mock
                                // harness has no hard-error injection
                                // primitive to prove a narrower guard correct
                                // (see `j2534-0404-service/docs/
                                // implementation-notes.md`'s A1-4/ADR-111
                                // backlog entry).
                                ReceivePhaseOutcome::Terminal => {
                                    // ADR-180 Decision 9: `Terminal` folds a
                                    // live mid-receive cancellation (this
                                    // COP's own claim, if any, otherwise
                                    // leaks -- the leak this Decision
                                    // closes) together with a stale/hard-
                                    // error cause already owned elsewhere
                                    // (Decision 1, or `handle_channel_hard_
                                    // error`'s own teardown) -- the two are
                                    // indistinguishable from here (see this
                                    // arm's own comment above). Called
                                    // unconditionally: in the already-owned
                                    // sub-cases this is an idempotent no-op
                                    // or a best-effort native cancel against
                                    // a channel that's already gone, the
                                    // same "wasteful, not unsafe" tradeoff
                                    // this arm already accepts for its own
                                    // unconditional Temp-binding revert just
                                    // below.
                                    if j1939_claimed_this_cop {
                                        cancel_j1939_claim_after_failed_startcomm(
                                            cll_handle,
                                            connect_generation,
                                            ctx,
                                        )
                                        .await;
                                    }
                                    if matches!(&binding, ParamBinding::Temp { .. }) {
                                        revert_hardware_to_live_active(
                                            &ctx.api,
                                            &ctx.logical_links,
                                            cll_handle,
                                            ctx.channel_id,
                                            protocol_id,
                                            &channel_wide_restore,
                                        )
                                        .await;
                                    }
                                    return;
                                }
                                // A timed-out receive is non-fatal (ADR-111,
                                // ISO 22900-2 §9.2.6.3.2 b)'s unconditional
                                // state-change sentence, and case d)'s
                                // SendRecv equivalence): fall through to the
                                // shared tail below, which still reaches
                                // PDU_CLLST_COMM_STARTED/PduCopstFinished.
                                ReceivePhaseOutcome::CycleComplete => {}
                                // A failed RC21/RC23 re-request: the error
                                // event was already emitted inside the wait;
                                // same best-effort "still complete" contract
                                // as CoptStopcomm's own arm -- fall through.
                                ReceivePhaseOutcome::ReRequestTxFailed => {}
                                // Statically unreachable: this arm's own
                                // resolution (`rpc_start_com_primitive`'s
                                // `CoptStartcomm` branch) rejects
                                // `num_receive_cycles == -1` (IS-CYCLIC)
                                // synchronously before a `tx` carrying that
                                // value can ever be constructed.
                                ReceivePhaseOutcome::DetachedToTier2 => unreachable!(
                                    "num_receive_cycles == -1 is rejected synchronously for \
                                     the optional CoptStartcomm message"
                                ),
                            }
                        }
                        // Fall through to the shared tail below (Temp-binding
                        // revert, tester-present dispatch, COMM_STARTED/
                        // Finished) -- unconditionally, per ADR-111: neither a
                        // receive-less transmit (num_receive_cycles == 0) nor
                        // a timed-out/re-request-failed receive phase skips
                        // PDU_CLLST_COMM_STARTED.
                    }
                    Err(TxFailure::Event(error_event)) => {
                        // ADR-111: the optional message never reached the
                        // bus, so this COP must NOT transition to
                        // COMM_STARTED -- unlike every other failed-transmit
                        // path in this function, no PduErrEvtInitError is
                        // emitted here (that event is specific to the K-line
                        // init sequence, which this path never runs).
                        let still_on_this_channel = {
                            let links = ctx.logical_links.lock().await;
                            links.get(&cll_handle).is_some_and(|l| {
                                l.channel_id == Some(ctx.channel_id)
                                    && l.connect_generation == connect_generation
                            })
                        };
                        if matches!(&binding, ParamBinding::Temp { .. }) {
                            revert_hardware_to_live_active(
                                &ctx.api,
                                &ctx.logical_links,
                                cll_handle,
                                ctx.channel_id,
                                protocol_id,
                                &channel_wide_restore,
                            )
                            .await;
                        }
                        if !still_on_this_channel {
                            emit_terminal_if_live(
                                &ctx.primitives,
                                &ctx.logical_links,
                                &ctx.subscriptions,
                                &ctx.service.terminal_cops,
                                cll_handle,
                                cop_handle,
                                PduComPrimitiveStatus::PduCopstCancelled,
                            )
                            .await;
                            return;
                        }
                        // ADR-180 Decision 9: this COP's own claim (if any)
                        // already succeeded, but the optional message never
                        // reached the bus -- COMM_STARTED is never reached
                        // on this path (ADR-111), so this is the
                        // terminating early return that must cancel it.
                        if j1939_claimed_this_cop {
                            cancel_j1939_claim_after_failed_startcomm(
                                cll_handle,
                                connect_generation,
                                ctx,
                            )
                            .await;
                        }
                        send_error_event(
                            &ctx.subscriptions,
                            &ctx.logical_links,
                            cll_handle,
                            error_event,
                            Some((cop_handle, cop_tag.clone())),
                        )
                        .await;
                        // ADR-128 correction (Codex review round 2): this
                        // emission used to be unconditional -- guarded now,
                        // matching every other terminal emitter in this file.
                        emit_terminal_if_live(
                            &ctx.primitives,
                            &ctx.logical_links,
                            &ctx.subscriptions,
                            &ctx.service.terminal_cops,
                            cll_handle,
                            cop_handle,
                            PduComPrimitiveStatus::PduCopstFinished,
                        )
                        .await;
                        return;
                    }
                    Err(TxFailure::ChannelLost) => {
                        // handle_channel_hard_error already emitted the full
                        // loss-of-comms sequence for every COP on this
                        // channel (including PduCopstCancelled for this COP)
                        // and already took the CLL offline -- must not emit
                        // anything further; the channel is gone so no
                        // Temp-binding revert is attempted.
                        return;
                    }
                    Err(TxFailure::Cancelled) => {
                        // ADR-180 Decision 9: this arm runs no staleness
                        // check at all (the channel is fully live -- the
                        // cancellation is client-driven, not a race), so if
                        // this COP's own claim already succeeded, this is
                        // unconditionally the terminating early return that
                        // must cancel it.
                        if j1939_claimed_this_cop {
                            cancel_j1939_claim_after_failed_startcomm(
                                cll_handle,
                                connect_generation,
                                ctx,
                            )
                            .await;
                        }
                        if matches!(&binding, ParamBinding::Temp { .. }) {
                            revert_hardware_to_live_active(
                                &ctx.api,
                                &ctx.logical_links,
                                cll_handle,
                                ctx.channel_id,
                                protocol_id,
                                &channel_wide_restore,
                            )
                            .await;
                        }
                        emit_terminal_if_live(
                            &ctx.primitives,
                            &ctx.logical_links,
                            &ctx.subscriptions,
                            &ctx.service.terminal_cops,
                            cll_handle,
                            cop_handle,
                            PduComPrimitiveStatus::PduCopstCancelled,
                        )
                        .await;
                        return;
                    }
                }
            }
        }
    }

    // Revert hardware now that the transient init transaction (if any) is
    // over -- BEFORE the periodic tester-present is ever started below,
    // which must always run under Active regardless of `binding` (ADR-067
    // claim 8). Reads the live Active set at revert time -- see
    // `revert_hardware_to_live_active`.
    if matches!(&binding, ParamBinding::Temp { .. }) {
        revert_hardware_to_live_active(
            &ctx.api,
            &ctx.logical_links,
            cll_handle,
            ctx.channel_id,
            protocol_id,
            &channel_wide_restore,
        )
        .await;
    }

    // ── Step 2: tester-present dispatch (ADR-083) ─────────────────────────────
    // Uses the same resolved TxFlags as the init frame above (ADR-062) --
    // previously hardcoded to TX_NORMAL_TRANSMIT (0) regardless of what the
    // client requested or what this CLL's addressing/SCI ComParams resolve to.
    let (new_tester_present_state, new_open_tp_discard) = if tester_present_data.is_empty()
        || tester_present_interval_ms == 0
        || !tester_present.handling_enabled
    {
        (TesterPresentState::None, None)
    } else {
        // Both `CP_TesterPresentSendType` values (0 "periodic", 1
        // "idle-triggered") are software-driven (ADR-083; both native
        // `PassThruStartPeriodicMsg` dispatch and its "mode 0 never re-arms"
        // consequence are gone as of this diff), sending the first frame
        // immediately at arm time (ADR-084) instead of waiting a full
        // `CP_TesterPresentTime` for the first send. Arming does NOT stamp
        // the shared channel's `last_bus_activity` clock: that clock is
        // shared across every CLL on the physical channel, so mutating it
        // here would silently push out a sibling CLL's already-counting
        // idle window for a send that never happened (arming itself has no
        // `PassThruConnect`, no frame on the wire -- it is not bus activity
        // by itself). This CLL gets its own `armed_at` instant;
        // `dispatch_due_tester_present` fires once the mode-appropriate
        // due-check formula (mode 1: idle-since-bus-activity; mode 0:
        // strictly since its own last fire/arm) is satisfied. `framed_data`
        // caches `tester_present_data` (already SF-framed above) so the poll
        // loop's tick handler does not need to re-run
        // `frame_tester_present_data` on every tick.
        //
        // P3-gated the same way a `CoptSendrecv` write is (ADR-083/ADR-084).
        // `num_receive_cycles` is derived from `CP_TesterPresentReqRsp`
        // (`tester_present.expects_response`) rather than hardcoded `0`
        // (P3-classification fix folded into this diff): a tester-present
        // send that itself expects a response must not be misclassified as
        // `no_response_required` for `CP_P3Func`/`CP_P3Phys` purposes.
        //
        // Non-cancellable (ADR-083/ADR-084): by this point `run_protocol_init`
        // has already put a real ECU handshake on the wire, past the point of
        // no return -- the pre-existing (pre-ADR-083) invariant that nothing
        // after a successful init is cancellable applies here too.
        match wait_for_p3_gap(
            cop_handle,
            cll_handle,
            tester_present.can_functional,
            if tester_present.expects_response {
                1
            } else {
                0
            },
            false,
            false,
            ctx,
        )
        .await
        {
            P3GapOutcome::Cancelled => {
                // Structurally unreachable: this call passes
                // cancellable = false, so wait_for_p3_gap never returns
                // Cancelled here (mirrors dispatch_tx_item's
                // TxItem::ResumeWake arm for the same "match stays
                // exhaustive, the type doesn't know it" situation). Falling
                // through as if Ready matches the invariant that the COP
                // must proceed to PduCopstFinished regardless.
                debug_assert!(
                    false,
                    "wait_for_p3_gap must not return Cancelled when cancellable = false"
                );
            }
            P3GapOutcome::HardError => {
                // handle_channel_hard_error already emitted the loss-of-comms
                // event sequence (including PduCopstCancelled for this COP).
                return;
            }
            P3GapOutcome::Deferred => unreachable!("defer_if_blocked = false"),
            P3GapOutcome::Ready => {}
        }

        // Re-validate the CLL is still connected on *this* physical channel
        // after the (non-cancellable) P3 gap wait (Codex review): a
        // concurrent DisconnectComLogicalLink is not gated by `cancelled_cops`
        // and can still race this wait. Unlike DestroyComLogicalLink (which
        // removes the CLL's entry from `logical_links` entirely),
        // DisconnectComLogicalLink deliberately *leaves the entry in place*
        // and only clears `channel_id`/`channel_key`/`comm_started`/
        // `tester_present_state` -- so a bare `contains_key` check (the first
        // attempt at this fix) still passes for a disconnected CLL. Checking
        // `channel_id == Some(ctx.channel_id)` catches both that case and a
        // CLL that reconnected to a *different* shared channel in the
        // meantime.
        let still_on_this_channel = {
            let links = ctx.logical_links.lock().await;
            links.get(&cll_handle).is_some_and(|l| {
                l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
            })
        };
        if !still_on_this_channel {
            // Emit a terminal COP event before bailing (Codex review): a bare
            // `return` here left the COP stuck at `PduCopstExecuting` forever
            // from a subscriber's point of view. `dispatch_tx_item`'s
            // post-match cleanup unconditionally removes this COP from
            // `primitives` once `handle_start_comm` returns (its
            // `continuation.is_none()` branch), but that cleanup itself never
            // emits an event -- every code path through `handle_start_comm`
            // is individually responsible for sending its own terminal
            // status first, the same obligation the `HardError` arm above
            // discharges via `handle_channel_hard_error` calling
            // `cancel_link_cops`. `DisconnectComLogicalLink` also calls
            // `cancel_link_cops` for this CLL, but only *after* it clears
            // `channel_id` under a separate, earlier lock acquisition -- so
            // there is a real window where this check observes the
            // disconnect (channel_id already cleared) before
            // `cancel_link_cops` has scanned `primitives` for this cop_handle
            // yet. `primitives.remove(&cop_handle).is_some()` is the same
            // first-wins discriminator `cancel_link_cops` and the other
            // early-exit paths in this file use: whichever side removes the
            // entry first is the one that emits, so a `cancel_link_cops` that
            // already won this race (removed it first) does not get a
            // duplicate `PduCopstCancelled` from here.
            emit_terminal_if_live(
                &ctx.primitives,
                &ctx.logical_links,
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
                cll_handle,
                cop_handle,
                PduComPrimitiveStatus::PduCopstCancelled,
            )
            .await;
            return;
        }

        // CP_P2Max from `tester_present.p2_max_ms` (ADR-088 amendment,
        // Codex review finding), NOT `binding.resolved()`: under
        // `temp_param_update = 1`, `binding` is `ParamBinding::Temp {
        // effective }` -- the transient Working snapshot for the init
        // transaction, already reverted on the hardware by this point --
        // while tester-present itself is always resolved from the call-time
        // Active snapshot regardless of `temp_param_update` (ADR-067 claim
        // 8, since it is a persistent product of this COP that outlives the
        // transient transaction). Reading `binding.resolved()` here would
        // size the very first discard window from a value that was never
        // actually in effect for this send -- `tester_present.p2_max_ms`
        // was resolved from the SAME Active snapshot as everything else
        // about this tester-present arm.
        let p2_max_ms = tester_present.p2_max_ms;
        let (fired_at, discard_window) = match send_tester_present_once(
            cll_handle,
            protocol_id,
            tx_flags,
            tester_present.can_functional,
            &tester_present_data,
            tester_present.expects_response,
            Some(connect_generation),
            ctx,
            Some((cop_handle, cop_tag.clone())),
        )
        .await
        {
            // ADR-099: the window now opens whenever this send merely
            // succeeded (`Ok`), regardless of `tester_present.expects_response`
            // -- TX-side indication frames are artifacts of the send itself,
            // not of `CP_TesterPresentReqRsp`'s "does the ECU reply"
            // semantics. `pos`/`neg` (resolved from the same call-time
            // Active snapshot rather than `binding.resolved()` -- see this
            // function's own `p2_max_ms` comment above for why) stay frozen
            // from `tester_present.exp_pos_resp`/`exp_neg_resp` ONLY when
            // `tester_present.expects_response` was `true`; otherwise they
            // freeze empty, which already can never content-match, so
            // content/SOM-herald discard stays correctly gated on
            // `expects_response` while the window itself now unconditionally
            // opens for TX-side discard (`tx_can_id`).
            Ok(t) => (
                t,
                Some(DiscardWindow {
                    until: t + Duration::from_millis(p2_max_ms as u64),
                    pos: if tester_present.expects_response {
                        tester_present.exp_pos_resp.clone()
                    } else {
                        Vec::new()
                    },
                    neg: if tester_present.expects_response {
                        tester_present.exp_neg_resp.clone()
                    } else {
                        Vec::new()
                    },
                }),
            ),
            Err(t) => (t, None),
        };

        // ADR-137 fourth Codex-review fix (round-4 restructure): the window
        // is no longer stored inside the `Armed` variant itself -- it is
        // built into a self-contained `ResidualTesterPresentDiscard` here
        // (before `tester_present_data` moves into `framed_data` below) and
        // pushed into `LogicalLinkState.open_tp_discards` at the write-back
        // site instead, under the same lock as the `tester_present_state`
        // write.
        let new_open_tp_discard = discard_window.map(|window| ResidualTesterPresentDiscard {
            target_can_ids: tester_present.target_can_ids,
            tx_can_id: tester_present_tx_can_id(&tester_present, &tester_present_data),
            window,
        });

        (
            TesterPresentState::Armed {
                resolved: tester_present.clone(),
                interval: Duration::from_millis(tester_present_interval_ms as u64),
                armed_at: fired_at,
                last_fired: Some(fired_at),
                framed_data: tester_present_data,
            },
            new_open_tp_discard,
        )
    };

    // ── Step 3: update link state ────────────────────────────────────────────
    // Re-validate the CLL is still connected on *this* physical channel
    // before writing back (Codex review), mirroring the `still_on_this_channel`
    // check used for the arm-time pre-send gate above. That earlier check
    // only guards the P3-gap/send window -- there are real `.await` points
    // *before* it too (`run_protocol_init`'s K-line wakeup sequence, the
    // `temp_param_update` hardware-revert step), and a concurrent
    // `DisconnectComLogicalLink` can race any of them. `Disconnect...`
    // deliberately leaves the CLL's map entry in place and only clears
    // `channel_id`/`comm_started`/`tester_present_state`, so an unconditional
    // write-back here would silently revive `comm_started = true` and re-arm
    // a stale `tester_present_state` on a link that is no longer connected --
    // and a later `ConnectComLogicalLink` on the same handle does not itself
    // clear `tester_present_state`/`comm_started` (they are presumed
    // already-clean from the prior disconnect), so the stale state would
    // start firing tester-present frames on the new connection with no fresh
    // `CoptStartcomm` ever having authorized it.
    // The channel-identity re-check and the write-back must happen under the
    // *same* lock acquisition (Codex review): an earlier draft checked
    // `still_on_this_channel` and dropped the lock before re-acquiring it for
    // the write below, leaving a residual gap -- narrow, since nothing but
    // the lock acquisition itself sits between them, but tokio's cooperative
    // scheduler can still force a yield at any `.await`, including an
    // uncontended `Mutex::lock`, letting a `DisconnectComLogicalLink` land in
    // between. Folding both into one critical section closes that gap
    // entirely. Nothing is orphaned when this CLL is no longer on this
    // channel -- `new_tester_present_state` is simply dropped, since it is
    // pure software state with no hardware resource to release.
    let still_on_this_channel = {
        let mut links = ctx.logical_links.lock().await;
        let on_channel = links.get(&cll_handle).is_some_and(|l| {
            l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
        });
        if on_channel && let Some(link) = links.get_mut(&cll_handle) {
            link.comm_started = true;
            link.tester_present_state = new_tester_present_state;
            link.tester_present_base_tx_flags = tester_present.base_tx_flags;
            // Pushed under the SAME lock acquisition as the state write
            // above (ADR-137 fourth Codex-review fix / round-4 restructure):
            // this closes the identical re-validation gap the rest of this
            // function already guards, so a stale write can never push into
            // the wrong CLL's list.
            push_open_tp_discard(
                &mut link.open_tp_discards,
                tokio::time::Instant::now(),
                new_open_tp_discard,
            );
        }
        on_channel
    };
    if !still_on_this_channel {
        // Same first-wins idiom as the arm-time pre-send gate's own
        // `still_on_this_channel` bail-out above: whichever side removes
        // this COP from `primitives` first is the one that emits the
        // terminal status, so a `DisconnectComLogicalLink`-driven
        // `cancel_link_cops` that already won this race does not get a
        // duplicate `PduCopstCancelled` from here.
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
        return;
    }

    // ── Step 4: emit status events ────────────────────────────────────────────
    send_cll_status(
        &ctx.subscriptions,
        &ctx.logical_links,
        cll_handle,
        PduComLogicalLinkStatus::PduCllstCommStarted,
    )
    .await;
    // ADR-128 correction: this final COP-status emission used to be
    // unconditional, with `send_cll_status`'s own `.await` sitting between
    // the `still_on_this_channel` check above and this send -- a narrow but
    // real window (matched-precedent gap, already documented as accepted
    // for the `.await` itself) in which a concurrent `DestroyComLogicalLink`
    // could complete (via `cancel_link_cops`, which already removes this
    // entry from `primitives` and emits/records the correct terminal
    // `Cancelled`) before this line ran. An unconditional emission here
    // would then overwrite that correct `terminal_cops` record with a
    // stale, resurrecting `Finished` for an already-fully-destroyed CLL
    // (ADR-128 review finding). Guarded the same "first-wins through
    // `primitives`" way as every CANCELLED emitter in this file: on the
    // normal path this removal always wins (nothing else races an
    // executing COP), so behavior is unchanged; on the destroy-race path,
    // `cancel_link_cops` already won, so this loses and correctly no-ops.
    emit_terminal_if_live(
        &ctx.primitives,
        &ctx.logical_links,
        &ctx.subscriptions,
        &ctx.service.terminal_cops,
        cll_handle,
        cop_handle,
        PduComPrimitiveStatus::PduCopstFinished,
    )
    .await;
}

/// Processes `TxItem::StopComm` in the poll task:
///
/// 1. Reads and clears `tester_present_state` from the link state (ADR-083):
///    no hardware resource to release (both modes are software-driven), so
///    it is simply reset to `None`.
/// 2. When `tx` is `Some` (non-empty `cop_data` was given at
///    `StartComPrimitive` call time, ADR-085): transmits `tx.send`, gap-gated
///    by the same `CP_P3Func`/`CP_P3Phys` enforcement `CoptSendrecv` uses,
///    but non-cancellable (the stop-comm state transition in step 3 below
///    already committed by the time this step runs, so there is no
///    "abort before commit" window left to honour a `CancelComPrimitive` on
///    this COP through). When `tx.num_receive_cycles != 0`, a bounded,
///    equally non-cancellable receive phase follows the transmit, matching
///    `tx.expected_response` exactly like `CoptSendrecv`'s own receive
///    phase (ADR-087) -- `0` (the default) stays receive-less. This step
///    runs strictly after step 1 (disarm) and strictly before step 3, per
///    ADR-085.
/// 3. Clears `LogicalLinkState.comm_started`.
/// 4. Emits `PduCllstOnline` and `PduCopstFinished`.
async fn handle_stop_comm(
    cop_handle: u32,
    cll_handle: u32,
    protocol_id: u32,
    tx: Option<Box<OneShotCommTx>>,
    connect_generation: u64,
    ctx: &ChannelPollCtx,
) {
    // ADR-118 (A2-24) / ADR-205: EXECUTING is gated atomically on the COP
    // still being live in `primitives`, guard held continuously from the
    // liveness check through the emission -- every CANCELLED emitter removes
    // the entry under `primitives` before emitting, so EXECUTING can never be
    // observed after CANCELLED. An absent entry means a concurrent
    // cancel/teardown already claimed this COP and emitted its own terminal
    // status -- bail before touching hardware; nothing to clean up. `cop_tag`
    // (ADR-205 Decision item 1) is captured here, once, from this same live
    // `CopEntry` read, and kept alive for the rest of this function (cloned
    // into every `send_error_event`/`ExpectedResponseWait` call below) rather
    // than re-resolved fresh at each one -- see `handle_send_recv`'s identical
    // hoist for the full rationale.
    let cop_tag = {
        let queue_target = resolve_queue_target(&ctx.logical_links, cll_handle).await;
        let prims = ctx.primitives.lock().await;
        let Some(entry) = prims.get(&cop_handle) else {
            return;
        };
        let cop_tag = entry.cop_tag.clone();
        send_cop_status(
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            queue_target.as_ref(),
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstExecuting,
            cop_tag.clone(),
        )
        .await;
        cop_tag
    };

    // Guard 0 (pre-teardown `still_on_this_channel` check folded into the
    // tester_present_state mutation's own lock, Codex review, PR #90 round 8,
    // ADR-086): round 7's version checked `still_on_this_channel` and then
    // took a *separate* `logical_links` lock acquisition to perform the
    // `std::mem::replace` below -- on the production multi-threaded Tokio
    // runtime, a disconnect+reconnect+fresh CoptStartcomm could land in the
    // gap between those two acquisitions, passing the check but still
    // clobbering whatever tester_present_state a BRAND-NEW session on this
    // same cll_handle had just armed. Folding the check and the mutation
    // into one critical section closes that window, mirroring the terminal
    // write-back's existing single-lock pattern below.
    //
    // `comm_started` is NOT cleared here (Codex-review fix, P2): `GetStatus
    // (CLL)` (`rpc_get_status`) and `CoptStartcomm`'s "comm already started"
    // precondition (`rpc_start_com_primitive`) both read `comm_started`
    // live, so clearing it this early let a client observe
    // `PduCllstOnline`-equivalent state -- and start a NEW `CoptStartcomm`
    // -- before the ADR-085 final transmit below had run. It is cleared
    // instead right before the `PduCllstOnline`/`PduCopstFinished` emission
    // at the end of this function, once the optional transmit has actually
    // been attempted.
    let found_on_channel = {
        let mut links = ctx.logical_links.lock().await;
        links.get_mut(&cll_handle).is_some_and(|link| {
            if link.channel_id == Some(ctx.channel_id)
                && link.connect_generation == connect_generation
            {
                link.tester_present_state = TesterPresentState::None;
                // Deliberately does NOT clear `link.open_tp_discards`
                // (ADR-137 fourth Codex-review fix / round-4 restructure,
                // design-advisor's explicit decision): PDU_COPT_STOPCOMM
                // ("no further tester presents will be sent") governs
                // future *sends*, not replies already elicited by sends
                // that already went out -- a delayed reply to a
                // pre-stopcomm send is still tester-present garbage that
                // should still be discarded, and the window self-expires
                // within its own `CP_P2Max` regardless. Contrast with
                // `DisconnectComLogicalLink` (`rpc_link.rs`), a full
                // teardown, which does clear it.
                true
            } else {
                false
            }
        })
    };
    if !found_on_channel {
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
        return;
    }

    if let Some(mut tx) = tx {
        // `cancellable: false` -- see the doc comment above. `tx.num_receive_cycles`
        // (ADR-087; `0` when this transmit is receive-less, matching the
        // pre-ADR-087 "receiveCycle=0" gap semantics, ADR-060) instead of a
        // hardcoded `0`. `defer_if_blocked: false`: this is a one-shot
        // terminal send, not an idle-mode keepalive dispatch, so `Deferred`
        // never applies here.
        match wait_for_p3_gap(
            cop_handle,
            cll_handle,
            tx.send.can_functional,
            tx.num_receive_cycles,
            false,
            false,
            ctx,
        )
        .await
        {
            P3GapOutcome::Ready => {
                // Ported from handle_start_comm's identical guard
                // (events.rs, tester-present arm branch) -- ADR-085 round-7
                // amendment: a concurrent Disconnect/Destroy can race this
                // (non-cancellable) wait.
                //
                // ADR-180 Decision 19 (design-advisor consult, PR #72 round
                // 16, Codex P1): also re-verify, under this same lock, that
                // `tx.send.j1939_tx_source` -- the source address byte this
                // final message's header was actually composed with -- still
                // matches the CLL's CURRENT `j1939_claimed_address`. Mirrors
                // `handle_send_recv`'s identical Decision 14/18 transmit-time
                // check (events.rs, `P3GapOutcome::Ready` arm above): a
                // `PDU_COPT_STOPCOMM` final message is deliberately excluded
                // from `cancel_send_recv_cops_for_cll`'s own cancellation
                // sweep (ADR-085's `stop_comm_pending` deadlock; see that
                // function's own doc comment), so an address relinquished
                // between this COP's enqueue and this dispatch would
                // otherwise transmit under a source this CLL no longer owns,
                // impersonating whichever CLL (or nothing) claimed it since.
                // `j1939_negotiation_posture` gates this the same way it
                // gates `handle_send_recv`'s check: not `Engaged` (i.e.
                // `Undecided` or `OptedOut`) permanently for a CLL that never
                // requested negotiation (never cancelled here); `Engaged` for
                // one that structurally did, in which case a mismatch --
                // INCLUDING no current claim at all -- means this final
                // message would defend or impersonate an address this CLL no
                // longer holds.
                let (
                    still_on_this_channel,
                    j1939_claim_drifted,
                    tp20_connection_drifted,
                    tp20_live_tx_id,
                ) = {
                    let links = ctx.logical_links.lock().await;
                    match links.get(&cll_handle) {
                        Some(l)
                            if l.channel_id == Some(ctx.channel_id)
                                && l.connect_generation == connect_generation =>
                        {
                            let drifted = matches!(
                                l.j1939_negotiation_posture,
                                J1939NegotiationPosture::Engaged
                            ) && tx
                                .send
                                .j1939_tx_source
                                .is_some_and(|src| l.j1939_claimed_address != Some(src));
                            // ADR-188 fix (edge-case-hunter, PR #97): the
                            // TP2.0 sibling of the J1939 claim-drift check
                            // just above, and of `handle_send_recv`'s
                            // identical own check -- this `CoptStopcomm`
                            // final message's `tp20_established_tx_id` was
                            // also resolved once, at `StartComPrimitive`
                            // (really `CoptStopcomm` call) bind time, and can
                            // go stale before this dispatch if the CLL's
                            // `tp20_connection` is torn down by a spontaneous
                            // `CONNECTION_LOST` indication while this
                            // (non-cancellable) final message is still
                            // waiting on `wait_for_p3_gap` above. Re-read the
                            // CLL's CURRENT `tp20_connection` here, under
                            // this same lock, rather than trust the
                            // bind-time snapshot -- see `handle_send_recv`'s
                            // identical check for the full condition
                            // rationale.
                            // Codex review fix (P1, PR #101, ADR-192/Phase 7
                            // Stage 7c): mirrors `handle_send_recv`'s
                            // identical `tp20_is_broadcast` guard -- see that
                            // call site's own comment for the full rationale.
                            // ADR-210 Decision item 12: both checks re-keyed
                            // from the narrow `is_tp2_0_protocol_id` to
                            // `is_tp2_0_family_protocol_id`, mirroring
                            // `handle_send_recv`'s identical fix above.
                            let tp20_live_tx_id =
                                (resources::is_tp2_0_family_protocol_id(l.hw_protocol_id)
                                    && !tx.send.tp20_is_broadcast)
                                    .then(|| {
                                        l.tp20_connection
                                            .filter(|c| c.phase == Tp20ConnectionPhase::Established)
                                            .and_then(|c| c.established_tx_id)
                                    })
                                    .flatten();
                            let tp20_drifted =
                                resources::is_tp2_0_family_protocol_id(l.hw_protocol_id)
                                    && !tx.send.tp20_is_broadcast
                                    && tp20_live_tx_id != tx.send.tp20_established_tx_id;
                            (true, drifted, tp20_drifted, tp20_live_tx_id)
                        }
                        _ => (false, false, false, None),
                    }
                };
                if !still_on_this_channel {
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstCancelled,
                    )
                    .await;
                    return;
                }
                if j1939_claim_drifted || tp20_connection_drifted {
                    // Suppress-and-complete, not cancel-the-COP (ADR-085's
                    // own "teardown must never be blocked" principle,
                    // Decision 9's Rejected-alternatives precedent): report
                    // the drift, skip the transmit (and any receive phase)
                    // entirely, and fall through unconditionally to the
                    // terminal `PduCllstOnline`/`PduCopstFinished` block
                    // below -- the same best-effort "still complete
                    // teardown" contract `TxFailure::Event`'s arm just below
                    // already uses for a transmit that reached the wire and
                    // failed. The TP2.0 case (ADR-188 fix) mirrors this
                    // exactly: a torn-down/never-(re)established connection
                    // has no header this final message could legitimately
                    // compose against.
                    send_error_event(
                        &ctx.subscriptions,
                        &ctx.logical_links,
                        cll_handle,
                        PduErrorEvent::PduErrEvtProtErr,
                        Some((cop_handle, cop_tag.clone())),
                    )
                    .await;
                } else {
                    // ADR-188 fix (edge-case-hunter, PR #97): mirrors
                    // `handle_send_recv`'s identical fresh-TX-ID patch --
                    // see that call site's own comment for the full
                    // rationale. Not drifted here, so `tp20_live_tx_id`
                    // already equals `tx.send.tp20_established_tx_id`
                    // numerically for a genuinely still-live connection;
                    // this keeps that true structurally rather than by
                    // coincidence.
                    if let Some(fresh_tx_id) = tp20_live_tx_id {
                        let header = fresh_tx_id.to_be_bytes();
                        if tx.send.data.len() >= header.len() {
                            tx.send.data[..header.len()].copy_from_slice(&header);
                        }
                    }
                    match transmit_request(
                        cll_handle,
                        cop_handle,
                        protocol_id,
                        tx.send.tx_flags,
                        &tx.send.data,
                        tx.send.isotp_tx.as_ref(),
                        true,
                        false,
                        ctx,
                    )
                    .await
                    {
                        Ok(()) => {
                            if let Some(functional) = tx.send.can_functional {
                                let bucket = if functional {
                                    &ctx.last_func_tx
                                } else {
                                    &ctx.last_phys_tx
                                };
                                *bucket.lock().await = Some(TxGapState {
                                    at: tokio::time::Instant::now(),
                                    no_response_required: tx.num_receive_cycles == 0,
                                });
                            }

                            // S3-equivalent staleness guard (ADR-087, mirroring
                            // handle_send_recv's S3 / this function's own
                            // pre-transmit guard above): the transmit itself
                            // performs real I/O and can span a
                            // disconnect+reconnect completing during the call --
                            // re-check before starting the (also non-cancellable)
                            // receive phase below. This is a cheap early bail for
                            // a reconnect that has already completed by this
                            // point; it does NOT by itself close the stale RX
                            // -attribution window for the receive phase that
                            // follows -- `.await`s remain between this check and
                            // any single poll pass's attribution decision inside
                            // `wait_for_expected_response`'s loop, so a reconnect
                            // completing after this check but before/during a
                            // later pass would still slip through a `target_cll`
                            // -only guard. That window is closed at the
                            // attribution layer itself (`bind_registrant`
                            // comparing each registrant's own
                            // `connect_generation`, ADR-086 originally, moved
                            // per-registrant by ADR-100), not here; this guard
                            // remains in place as a narrower, defense-in-depth
                            // early exit.
                            let still_on_this_channel = {
                                let links = ctx.logical_links.lock().await;
                                links.get(&cll_handle).is_some_and(|l| {
                                    l.channel_id == Some(ctx.channel_id)
                                        && l.connect_generation == connect_generation
                                })
                            };
                            if !still_on_this_channel {
                                emit_terminal_if_live(
                                    &ctx.primitives,
                                    &ctx.logical_links,
                                    &ctx.subscriptions,
                                    &ctx.service.terminal_cops,
                                    cll_handle,
                                    cop_handle,
                                    PduComPrimitiveStatus::PduCopstCancelled,
                                )
                                .await;
                                return;
                            }

                            // ADR-087: when the client asked for a response to
                            // this final message (NumReceiveCycles != 0), run the
                            // same bounded receive phase CoptSendrecv uses,
                            // non-cancellable (see the doc comment above) --
                            // matched responses get ResultData/resultitem
                            // attribution exactly like CoptSendrecv. `0` (the
                            // default, and still the most common case) skips
                            // this entirely, preserving the pre-ADR-087
                            // fire-and-forget behavior byte-for-byte.
                            if tx.num_receive_cycles != 0 {
                                match wait_for_expected_response(
                                    ExpectedResponseWait {
                                        cll_handle,
                                        cop_handle,
                                        cop_tag: cop_tag.clone(),
                                        expected: &tx.expected_response,
                                        num_receive_cycles: tx.num_receive_cycles,
                                        timeout_ms: tx.response_timeout_ms,
                                        protocol_id,
                                        tx_flags: tx.send.tx_flags,
                                        original_data: &tx.send.data,
                                        isotp_tx: tx.send.isotp_tx.as_ref(),
                                        rc_cfg: &tx.rc_cfg,
                                        timing_cfg: tx.timing_cfg.as_deref(),
                                        connect_generation,
                                        cancellable: false,
                                        match_reset_ceiling_ms: (tx.num_receive_cycles == -2).then(
                                            || {
                                                tx.response_timeout_ms
                                                    .saturating_mul(
                                                        STOPCOMM_IS_MULTIPLE_CEILING_FACTOR,
                                                    )
                                                    .max(STOPCOMM_IS_MULTIPLE_CEILING_FLOOR_MS)
                                            },
                                        ),
                                        // This call site is always driven by an
                                        // actual final transmit (`tx.send`) --
                                        // never ADR-059's created-receive-only
                                        // category -- so ADR-100 Decision §4 (S6)
                                        // does not apply here; `cyclic_resp_timeout_ms`
                                        // is ignored downstream regardless since
                                        // `created_receive_only` is `false`.
                                        created_receive_only: false,
                                        cyclic_resp_timeout_ms: 0,
                                        enable_concatenation: tx.enable_concatenation,
                                    },
                                    ctx,
                                )
                                .await
                                {
                                    // Hard error or staleness -- every required
                                    // status event has already been emitted.
                                    ReceivePhaseOutcome::Terminal => return,
                                    // Required matches arrived, the IS-MULTIPLE
                                    // window closed, or a timeout was reported
                                    // (best-effort) -- fall through to the
                                    // terminal PduCllstOnline/PduCopstFinished
                                    // block below either way.
                                    ReceivePhaseOutcome::CycleComplete => {}
                                    // A failed RC21/RC23 re-request: the error
                                    // event was already emitted inside the wait;
                                    // same best-effort "still complete teardown"
                                    // contract as this step's other TX-failure
                                    // arms below -- fall through.
                                    ReceivePhaseOutcome::ReRequestTxFailed => {}
                                    // Statically unreachable: `rpc_start_com_primitive`'s
                                    // `CoptStopcomm` branch synchronously rejects
                                    // `num_receive_cycles == -1` (IS-CYCLIC) before
                                    // a `TxItem::StopComm` can ever exist, so
                                    // `no_deadline` is never `true` at this call
                                    // site and `DetachedToTier2` can never be
                                    // returned here (ADR-087; ADR-100 Decision §2).
                                    ReceivePhaseOutcome::DetachedToTier2 => unreachable!(
                                        "num_receive_cycles == -1 is rejected synchronously for \
                                     CoptStopcomm"
                                    ),
                                }
                            }
                        }
                        Err(TxFailure::Event(error_event)) => {
                            // Recheck generation before reporting (Codex review,
                            // PR #90 round 8, ADR-086): `transmit_request` itself
                            // does I/O, so a disconnect+reconnect can complete
                            // during the call and land between the pre-transmit
                            // `still_on_this_channel` check above and this
                            // error-reporting step. Without this recheck, a
                            // stale COP's transmit failure would still update
                            // `last_error` and notify subscribers on the
                            // reconnected CLL. The terminal block below already
                            // independently re-checks generation before any
                            // status emission, so skipping this event when stale
                            // does not leave the COP without a terminal status.
                            let still_on_this_channel = {
                                let links = ctx.logical_links.lock().await;
                                links.get(&cll_handle).is_some_and(|l| {
                                    l.channel_id == Some(ctx.channel_id)
                                        && l.connect_generation == connect_generation
                                })
                            };
                            if still_on_this_channel {
                                send_error_event(
                                    &ctx.subscriptions,
                                    &ctx.logical_links,
                                    cll_handle,
                                    error_event,
                                    Some((cop_handle, cop_tag.clone())),
                                )
                                .await;
                            }
                        }
                        Err(TxFailure::ChannelLost) => {
                            // handle_channel_hard_error already emitted the full
                            // loss-of-comms sequence for every COP on this
                            // channel, including PduCopstCancelled for THIS
                            // StopComm COP, and already took the CLL to
                            // PduCllstOffline -- a stronger terminal state than
                            // Online. Must not emit anything further, or we
                            // contradict that sequence.
                            return;
                        }
                        Err(TxFailure::Cancelled) => {
                            unreachable!(
                                "cancellable = false: isotp_send never checks cancelled_cops"
                            )
                        }
                    }
                }
            }
            P3GapOutcome::HardError => {
                // poll_rx (the only source of HardError here) already ran
                // handle_channel_hard_error, which emitted the full
                // loss-of-comms sequence -- including PduCopstCancelled for
                // THIS StopComm COP -- and already took the CLL to
                // PduCllstOffline. Same reasoning as TxFailure::ChannelLost
                // above: must not emit anything further.
                return;
            }
            P3GapOutcome::Cancelled => {
                unreachable!("cancellable = false: wait_for_p3_gap never checks cancelled_cops")
            }
            P3GapOutcome::Deferred => unreachable!("defer_if_blocked = false"),
        }
    }

    // Commit the stop-comm state transition now that the optional final
    // transmit has been attempted (sent, or best-effort-failed via
    // TxFailure::Event -- TxFailure::Cancelled cannot occur here, see the
    // unreachable! above -- the ChannelLost/HardError branches above
    // already returned early without reaching here, and independently
    // cleared comm_started themselves via handle_channel_hard_error).
    //
    // The channel-identity re-check and the write-back happen under the
    // *same* `logical_links` lock acquisition (Codex review precedent, see
    // handle_start_comm's identical write-back above): checking then
    // dropping the lock before re-acquiring it for the write would leave a
    // residual gap for a `DisconnectComLogicalLink` to land in between.
    // `channel_id == Some(ctx.channel_id)` is used rather than `connected`:
    // a fast reconnect on the same `cll_handle` can set `connected = true`
    // again on a *different* channel, and `connected` alone would wrongly
    // let this stale StopComm clear state on the new connection.
    // `channel_id` alone is still not enough: on a *shared* channel
    // (`ref_count > 1`), a disconnect of this CLL does not tear down the
    // physical channel (another CLL keeps it alive), so a same-protocol/baud
    // reconnect of the SAME `cll_handle` rejoins with the identical
    // `channel_id`. `connect_generation == connect_generation` (the call-time
    // capture vs. the live value) additionally catches that case: a reconnect
    // always bumps the live generation, so a stale COP's captured value no
    // longer matches even when `channel_id` happens to match again (ADR-086).
    //
    // Ported from handle_start_comm's identical terminal-block guard
    // (ADR-085 round-7 amendment). A no-I/O micro-window remains between
    // releasing the lock here and the `send_cll_status`/`send_cop_status`
    // calls below -- tokio's cooperative scheduler can still yield there.
    // handle_start_comm's own `CommStarted` emission has the identical
    // residual and does not close it; this is a deliberately-deferred,
    // matched-precedent gap, not something this fix claims to solve.
    // ADR-188/Phase 7 Stage 7a Decision item 2: `PDU_COPT_STOPCOMM` on a
    // TP2.0 CLL best-effort tears down its own connection, keyed on the
    // connection's own `requested_rx_id` (clause 19.3.3.3: teardown is
    // keyed on the original request's RX-ID) -- taken (cleared) here, under
    // the same lock acquisition as `comm_started`'s own clear, so a racing
    // fresh `CoptStartcomm` on this same CLL never observes stale
    // connection state left behind by this StopComm.
    // Codex review fix (PR #97, ADR-188, Fix I): `shared_channels` is
    // acquired FIRST here, outermost across the entire clear-connection +
    // sweep-repeat-slots + native-teardown sequence below -- mirroring
    // `ioctl_start_repeat_message`'s (`rpc_misc.rs`) own `shared_channels`
    // hold across its check/compose/native-call/register span, so the two
    // now serialize against each other. Without this, a repeat-slot START
    // could interleave between this StopComm's `tp20_connection.take()` and
    // its own repeat-slot sweep/native teardown below: it could read a
    // still-live `tp20_established_tx_id` (captured under its own
    // `logical_links` snapshot), compose and register a brand-new repeat
    // slot baked with that now-stale TX-ID, and land that registration
    // AFTER this StopComm's sweep already ran and found nothing -- leaving a
    // freshly-created repeat slot autonomously transmitting a stale TX-ID
    // after the connection has torn down natively. `logical_links` nested
    // underneath, matching this codebase's documented `shared_channels` ->
    // `logical_links`/`api` lock-ordering discipline (`implementation-notes.md`'s
    // "Device selection" section) and `ioctl_start_repeat_message`'s own
    // span. Acquired unconditionally (every protocol, not just TP2.0):
    // whether this CLL even has a TP2.0 connection to tear down is only
    // knowable once `logical_links` is inspected below, and `shared_channels`
    // must already be held before that inspection for the serialization to
    // close the race.
    let mut chans = ctx.service.shared_channels.lock().await;
    let (still_on_this_channel, tp20_connection) = {
        let mut links = ctx.logical_links.lock().await;
        match links.get_mut(&cll_handle) {
            Some(link)
                if link.channel_id == Some(ctx.channel_id)
                    && link.connect_generation == connect_generation =>
            {
                link.comm_started = false;
                link.stop_comm_pending = false;
                let connection = link.tp20_connection.take();
                (true, connection)
            }
            _ => (false, None),
        }
    };
    // ADR-190/Phase 7 Stage 7b section 4: a passive-arm's own connection
    // state (`passive: true`) is classified separately from Stage 7a's
    // active-connection teardown below, regardless of its own `phase`
    // (`Listening` or `Established` -- `CoptStopcomm` always fully disarms
    // the listener, not only while a connection happens to be established
    // under it). `Tp20Connection` is `Copy`, so `.filter()` below on each
    // classification consumes its own copy, leaving `tp20_connection`
    // itself usable for both.
    let tp20_teardown =
        tp20_connection.filter(|c| !c.passive && c.phase == Tp20ConnectionPhase::Established);
    let tp20_passive_disarm = tp20_connection.filter(|c| c.passive);
    if !still_on_this_channel {
        drop(chans);
        // Same first-wins idiom as the pre-transmit-check bail-out above
        // (and handle_start_comm's own terminal-block bail-out): whichever
        // side removes this COP from `primitives` first is the one that
        // emits the terminal status, so a `DisconnectComLogicalLink`-driven
        // `cancel_link_cops` that already won this race does not get a
        // duplicate `PduCopstCancelled` from here.
        emit_terminal_if_live(
            &ctx.primitives,
            &ctx.logical_links,
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstCancelled,
        )
        .await;
        return;
    }

    if tp20_teardown.is_some() {
        // Codex review finding (PR #97): a repeat message slot started
        // while this connection was live still carries the connection's
        // fixed TX-ID baked into its payload -- unlike an ordinary
        // ComPrimitive, PDU_IOCTL_STOP_REPEAT_MESSAGE is not tied to
        // `comm_started`, so it would otherwise keep autonomously
        // retransmitting that now-stale TX-ID as raw traffic after this
        // connection tears down. Stop this CLL's own repeat slots first,
        // mirroring the existing `stop_repeat_slots_for_cll`/
        // `record_leaked_repeat_slots` best-effort shape (ADR-180 Decision
        // 10's own precedent), before issuing the native connection
        // teardown below. `stop_repeat_slots_for_cll` itself only acquires
        // `logical_links` and `api` (never `shared_channels`, see its own
        // doc comment), so calling it here while `chans` is already held is
        // safe and does not invert the lock order.
        let failed = stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
        // `push_leaked_repeat_slots` (Fix I), not `record_leaked_repeat_slots`:
        // `chans` is already held by this caller above -- `record_leaked_
        // repeat_slots` acquires `shared_channels` itself internally, which
        // would self-deadlock against the guard already held here (this
        // repo's mutexes are not reentrant).
        push_leaked_repeat_slots(&mut chans, ctx.channel_id, failed);
    }

    if let Some(conn) = tp20_teardown {
        // `api` nested under the still-held `chans` (Fix I): the native
        // teardown must also complete before `chans` releases below, or a
        // repeat-slot START arriving in the gap between releasing `chans`
        // and this native call could still interleave with the connection
        // tearing down.
        let api = ctx.api.lock().await;
        if let Err(err) = api.tp20_teardown_connection(ctx.channel_id, conn.requested_rx_id) {
            warn!(
                cll_handle,
                requested_rx_id = conn.requested_rx_id,
                %err,
                "best-effort TP2.0 IOCTL_TEARDOWN_CONNECTION failed (PDU_COPT_STOPCOMM) -- \
                 leaked native connection slot is an accepted residual (ADR-188 Consequences)"
            );
        }
        drop(api);
        // Codex review fix (PR #97, round 14, corrected round 17, reverted
        // round 21): `IOCTL_TEARDOWN_CONNECTION` is non-blocking -- the
        // device's own delayed `CONNECTION_LOST` confirmation for
        // `conn.requested_rx_id` can still arrive well after this call
        // returns. Without a quarantine entry here, a promptly-issued new
        // `CoptStartcomm` proposing the SAME rx_id (nothing prevents it --
        // `tp20_connections` already has no entry for this rx_id, since
        // `run_tp20_connection_request`'s own success path already removed
        // it when this connection first established) could register its
        // own pending entry before that stale indication drains, letting
        // `deliver_tp20_connection_indication` misattribute this
        // teardown's own delayed confirmation to the NEW request -- the
        // same class of bug Fix J/K's quarantine mechanism already closes
        // for the LOCAL-abandonment paths, reached here via a normal,
        // successful StopComm instead. Quarantining (vacant-only insert,
        // the same helper Fix K's write-back arm uses) blocks any new
        // request against this rx_id until the delayed indication actually
        // arrives and releases it, exactly like an abandoned entry already
        // does.
        //
        // Deliberately UNCONDITIONAL, not gated on whether the teardown
        // call above succeeded (Codex review finding, PR #97, round 21):
        // round 17 (Fix V) gated this on teardown success, reasoning that a
        // synchronous failure means the device never received this
        // teardown at all, so no delayed indication would ever arrive.
        // Round 21 identified the same race rounds 19-20 already forced a
        // revert for at `best_effort_teardown_on_abandon`'s own call
        // sites: the device may have independently and spontaneously lost
        // this connection (ADR-188's own "no ongoing monitoring for a
        // later spontaneous loss once established" residual) before this
        // deliberate teardown ever ran, in which case the call fails
        // precisely BECAUSE a stale `CONNECTION_LOST` indication may
        // already be queued, not because none is coming -- skipping the
        // quarantine then risks the exact same misattribution race this
        // fix exists to prevent. Reverted to Fix R's original unconditional
        // insert, accepting the same leaked-native-slot residual this
        // call's own log message already documents (`ADR-188` Consequences)
        // as the uniform behavior across every quarantine call site in this
        // mechanism.
        if let Some(sc) = chans
            .values_mut()
            .find(|sc| sc.channel_id == ctx.channel_id)
        {
            quarantine_tp20_connection_for_orphaned_write_back(
                conn.requested_rx_id,
                cll_handle,
                connect_generation,
                &mut sc.tp20_connections,
            );
        }
    }

    // ADR-190/Phase 7 Stage 7b section 4: passive disarm -- reverses
    // `arm_tp20_passive_listener`. `chans` is still held from above (the
    // same outermost-first discipline Fix I's comment already documents),
    // so steps 1-4 run as one unbroken sequence here too.
    if let Some(conn) = tp20_passive_disarm {
        let rx_id_passive = conn.requested_rx_id;
        let was_established = conn.phase == Tp20ConnectionPhase::Established;
        // Codex review finding (P2, PR #99, round 3): the active-connection
        // arm above only stops this CLL's own repeat slots when `tp20_
        // teardown.is_some()`, which the passive-vs-active split (`!c.
        // passive` on `tp20_teardown`'s own filter) deliberately excludes a
        // passive connection from -- but nothing else in THIS arm ever
        // called `stop_repeat_slots_for_cll` for it either, so an
        // established passive connection's own repeat slot survived a
        // `CoptStopcomm` disarm and kept autonomously retransmitting the
        // now-stale peer TX-ID, both across the disarm and into whatever
        // establishes on this rx_id next. Mirrors the active arm's own
        // stop-before-teardown ordering and `deliver_tp20_connection_
        // indication_once`'s own identical fix for the passive `Lost`
        // outcome (ADR-190's Correction paragraph, round 2) -- stop first,
        // so a still-running slot cannot autonomously transmit while the
        // native config-clear/teardown below is still in flight.
        if was_established {
            let failed = stop_repeat_slots_for_cll(cll_handle, connect_generation, ctx).await;
            push_leaked_repeat_slots(&mut chans, ctx.channel_id, failed);
        }
        let config_cleared;
        {
            // Step 1: best-effort clear native config, `TP2_0_IDENTIFER`
            // first -- before anything else runs (stops the device from
            // accepting any further inbound connection before service-side
            // state changes; reversing the order would open a window where
            // the device accepts a fresh, now-orphan connection between
            // service-side teardown and the native config actually
            // clearing).
            let api = ctx.api.lock().await;
            config_cleared =
                best_effort_disarm_tp20_passive_native_config(&api, ctx.channel_id, cll_handle);
            // Step 2: best-effort TEARDOWN_CONNECTION, only if this
            // passive slot is currently Established (spec-available per
            // Table 79/81, unlike active REQUEST_CONNECTION).
            if was_established
                && let Err(err) = api.tp20_teardown_connection(ctx.channel_id, rx_id_passive)
            {
                warn!(
                    cll_handle,
                    rx_id_passive,
                    %err,
                    "best-effort TP2.0 IOCTL_TEARDOWN_CONNECTION on passive disarm \
                     (PDU_COPT_STOPCOMM) failed -- leaked native connection slot is an accepted \
                     residual (ADR-190 Consequences), or the whole call is spec-ambiguous on a \
                     real adapter and self-heals via the maintenance timeout"
                );
            }
        }
        // Steps 3-4: unconditionally mark the persistent routing entry
        // abandoned (never removed -- ADR-188 rounds 19-21's quarantine
        // convention) and clear the exclusivity token. `was_established`/
        // `config_cleared` additionally gate a bounded release deadline for
        // the never-`Established` case (ADR-190's Correction paragraph,
        // Codex review finding, P1, PR #99) -- see `quarantine_tp20_
        // passive_slot_on_disarm`'s own doc comment.
        if let Some(sc) = chans
            .values_mut()
            .find(|sc| sc.channel_id == ctx.channel_id)
        {
            quarantine_tp20_passive_slot_on_disarm(
                rx_id_passive,
                sc,
                was_established,
                config_cleared,
            );
        }
    }
    drop(chans);

    send_cll_status(
        &ctx.subscriptions,
        &ctx.logical_links,
        cll_handle,
        PduComLogicalLinkStatus::PduCllstOnline,
    )
    .await;
    // ADR-128 correction: see handle_start_comm's identical fix for the
    // full rationale -- this final COP-status emission used to be
    // unconditional, with `send_cll_status`'s own `.await` sitting between
    // the `still_on_this_channel` check above and this send, letting a
    // concurrent `DestroyComLogicalLink` resurrect a stale `Finished` in
    // `terminal_cops` over `cancel_link_cops`'s already-correct `Cancelled`
    // record. Guarded the same "first-wins through `primitives`" way.
    emit_terminal_if_live(
        &ctx.primitives,
        &ctx.logical_links,
        &ctx.subscriptions,
        &ctx.service.terminal_cops,
        cll_handle,
        cop_handle,
        PduComPrimitiveStatus::PduCopstFinished,
    )
    .await;
}

/// Captures the channel's actual pre-bracket HARDWARE value (native units,
/// via `GET_CONFIG`) for every `comparam_support::CHANNEL_WIDE_UNUM32` key
/// that resolves to a native config id on `hw_protocol_id` -- the fix for a
/// sibling-CLL clobber bug (round 18, Codex review, P2, PR #101, ADR-192
/// Decision item 3 amendment): `CP_TP20BroadcastInterval` is channel-wide/
/// hardware-resident but deliberately not `PDU_PC_BUSTYPE`-classified, so
/// `strip_bustype_keys` alone never excludes it from a per-CLL Active push
/// during a `temp_param_update` bracket's revert -- and `Active` is tracked
/// PER-CLL, so reverting to this CLL's own copy could clobber the channel's
/// real, currently-live value if a sibling CLL sharing the same physical
/// channel has since promoted a different value of its own.
///
/// Must be called while `api` is already held for the whole apply -> ... ->
/// revert bracket, strictly BEFORE the bracket's own temp-bound apply
/// (ADR-067's revert-target rule: capture happens under the continuously-
/// held `api` guard at apply time, so nothing can interleave and change the
/// channel's value between capture and the temp apply). Callers that cannot
/// hold `api` continuously across their own whole bracket use
/// [`apply_params_to_hardware_capturing`] instead, which locks `api` once,
/// captures, then applies, all under the same guard.
///
/// Returns `(native_config_id, native_value)` pairs -- NOT `ComParamId`/
/// microsecond-unit values -- since these are restored later via
/// `set_config_u32` directly (raw native units), never through the
/// microsecond-converting `apply_params_to_hardware_locked`/
/// `to_j2534_config_value` path. Routing through that path instead would be
/// needless redundancy today (`to_j2534_config_value` currently has no
/// conversion arm for `CONFIG_TP2_0_T_BR_INT`, so it is an identity
/// passthrough for this key) but going straight to `set_config_u32` is a
/// deliberate design safeguard/future-proofing choice regardless: it stays
/// correct even if a conversion arm for this key is added later, and it is
/// the more architecturally correct approach for a raw native-unit
/// restore either way.
///
/// A key that fails to resolve to a native config id on `hw_protocol_id` is
/// silently omitted (not every protocol maps it). A key whose `GET_CONFIG`
/// read fails is also omitted, logged at `warn!` (best-effort, matching this
/// file's convention for other non-fatal native-call failures, e.g. the
/// CP_Baudrate readback above) -- the revert then falls back to restoring
/// this key from this CLL's own per-CLL Active value instead (edge-case-
/// hunter adversarial review, PR #101, BLOCKING Finding 1): an unconditional
/// strip of the key from the Active push, regardless of whether a captured
/// pair actually exists, used to leave a capture-failed key with NEITHER an
/// Active-derived restore NOR a captured one, so the bracket's own
/// temp-bound value stayed live on hardware forever. See
/// `comparam_support::strip_captured_channel_wide_keys`'s own doc comment
/// for the full fallback design.
pub(super) async fn capture_channel_wide_hardware_locked(
    api: &J2534Api0404,
    channel_id: ChannelId,
    hw_protocol_id: u32,
) -> Vec<(u32, u32)> {
    let mut restore = Vec::new();
    for &id in comparam_support::CHANNEL_WIDE_UNUM32 {
        let Some(cfg) = id.to_j2534_config_id(hw_protocol_id) else {
            continue;
        };
        match api.get_config_u32(channel_id, cfg) {
            Ok(value) => restore.push((cfg, value)),
            Err(err) => {
                warn!(
                    channel_id = channel_id.0,
                    cfg,
                    %err,
                    "failed to read back the pre-bracket hardware value for a channel-wide \
                     ComParam before a temp-bound apply; this key will not be restored on \
                     revert (best-effort capture only)"
                );
            }
        }
    }
    restore
}

/// Reverts hardware to the CLL's *live* Active set, read here, at revert
/// time (ADR-067). The revert target is a restoration duty -- "leave the
/// link in whatever is Active now" -- not a COP-bound param, so ADR-067's
/// call-time snapshot binding does not apply to it: a `CoptUpdateparam`
/// already queued ahead of the temp COP (called before it, executed after
/// its call), or interleaved between cycles of a cyclic temp send, has
/// legitimately moved hardware and `LogicalLinkState.active` on, and
/// reverting to an Active snapshot bound at `StartComPrimitive` call time
/// would undo that update and leave hardware diverged from the in-memory
/// Active set the next plain COP resolves from.
///
/// Every one of this function's call sites is inside a `ParamBinding::Temp`
/// arm (confirmed at each: `handle_send_recv`, `handle_start_comm`'s init
/// apply-failure/init-failure/post-init paths). `rpc_primitive.rs`'s
/// `rpc_start_com_primitive` used to call this wrapper too (Fix 5, Codex
/// review, P2, PR #101, ADR-192/Phase 7 Stage 7c), but Codex review round 8
/// Finding 1 moved that call site onto [`revert_hardware_to_live_active_locked`]
/// directly, so its apply/start/revert bracket can hold `api` continuously
/// across all three steps instead of re-acquiring it here. ADR-110 (ISO
/// 22900-2 §9.4.16.2.1 c) NOTE 2 / d)): the live Active read here strips
/// every `PDU_PC_BUSTYPE`-class key before pushing to hardware -- this
/// CLL's own Active can be stale relative to another CLL's real,
/// already-pushed hardware state regardless of whether any lock is held, so
/// a revert must never re-write a BUSTYPE value either. Centralized here
/// rather than at each call site since every call site is Temp-specific.
///
/// Thin wrapper over [`revert_hardware_to_live_active_locked`] that locks
/// `api` itself -- mirrors the `apply_params_to_hardware`/
/// `apply_params_to_hardware_locked` pairing above exactly.
pub(super) async fn revert_hardware_to_live_active(
    api: &Arc<Mutex<J2534Api0404>>,
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    cll_handle: u32,
    channel_id: ChannelId,
    hw_protocol_id: u32,
    // Round 18 (Codex review, P2, PR #101, ADR-192 Decision item 3
    // amendment): `(native_config_id, native_value)` pairs captured via
    // `capture_channel_wide_hardware_locked` BEFORE the bracket's own temp
    // apply, restored via `set_config_u32` AFTER the stripped-Active push
    // below -- see `revert_hardware_to_live_active_locked`'s own doc
    // comment for the full design. Every caller must supply this
    // explicitly; a caller with no channel-wide key ever in play (the
    // soft-ISO-TP arm of `handle_send_recv` -- ADR-046 gating means
    // `CP_TP20BroadcastInterval` never coexists with software ISO-TP on one
    // channel) passes `&[]`.
    channel_wide_restore: &[(u32, u32)],
) {
    let api = api.lock().await;
    revert_hardware_to_live_active_locked(
        &api,
        logical_links,
        cll_handle,
        channel_id,
        hw_protocol_id,
        channel_wide_restore,
    )
    .await;
}

/// Same as [`revert_hardware_to_live_active`], but takes an already-locked
/// `&J2534Api0404` instead of the `Arc<Mutex<...>>` wrapper (Codex review
/// round 8 Finding 1, PR #101, ADR-192/Phase 7 Stage 7c): lets
/// `rpc_primitive.rs`'s `rpc_start_com_primitive` hold `api` continuously
/// across the whole TP2.0 broadcast-periodic `ParamBinding::Temp` apply ->
/// native `start_periodic_message` -> revert bracket, serializing it
/// against a sibling CLL's own temp-bound broadcast start or a queued
/// `CoptUpdateparam` on the same physical channel -- without this, each
/// step re-acquired and released `api` separately, letting a concurrent
/// operation interleave and corrupt which hardware value the burst actually
/// transmits with.
///
/// Reading `logical_links` here while the caller already holds `api` is the
/// sanctioned lock order (ADR-110 amendment: `api` outer, `logical_links`
/// inner -- see `handle_update_param`'s own `api`-held `logical_links` read
/// for the established precedent) -- no special care needed beyond what
/// [`revert_hardware_to_live_active`] already does.
///
/// `hw_protocol_id`: same Plane A (raw) contract as
/// [`revert_hardware_to_live_active`] -- see its doc comment.
pub(super) async fn revert_hardware_to_live_active_locked(
    api: &J2534Api0404,
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    cll_handle: u32,
    channel_id: ChannelId,
    // ADR-158 correction (this comment's prior ADR-157 Plane B framing was
    // wrong): this function's ONLY use of a protocol id is the
    // `apply_params_to_hardware_locked` push below, which now uniformly
    // expects the Plane A (raw, un-normalized) hardware protocol id -- both
    // `to_j2534_config_id` and `expand_tidle` self-normalize internally,
    // so this parameter must always be the raw id, never a
    // base-normalized one. This matters concretely for Active: Active can
    // legitimately hold an FD-invalid param (`BIT_SAMPLE_POINT`/
    // `SYNC_JUMP_WIDTH`) reaching it via `SetComParam`+`Connect` or
    // `SetComParam`+`CoptUpdateparam` (ADR-067/068's Working/Active model
    // does not filter Active for FD-validity), so `to_j2534_config_id`
    // needs the raw id here to be able to detect and suppress it on an
    // FD_CAN_PS link (clause 21.3.2.5.1) when this function pushes Active
    // to hardware. Every caller passes its own raw `protocol_id`/
    // `hw_protocol_id`, never a base-normalized one.
    hw_protocol_id: u32,
    // Round 18 (Codex review, P2, PR #101, ADR-192 Decision item 3
    // amendment): captured `(native_config_id, native_value)` pairs for
    // every `comparam_support::CHANNEL_WIDE_UNUM32` key that
    // `capture_channel_wide_hardware_locked` successfully read from the
    // channel's real pre-bracket hardware state -- NOT this CLL's own
    // `active` -- while `api` was already held for the whole bracket.
    // `CP_TP20BroadcastInterval` is channel-wide/hardware-resident but
    // deliberately not `PDU_PC_BUSTYPE` (ADR-192 Decision item 3), so
    // `strip_bustype_keys` alone never strips it from a per-CLL Active push
    // -- but `Active` is tracked PER-CLL, so reverting to this CLL's own
    // stale copy could clobber a sibling CLL's own, currently-live value
    // for the same physical channel. Restoring the CAPTURED value here
    // instead is correct per ADR-067's revert rule because capture happened
    // under the continuously-held `api` guard at apply time, so nothing
    // could interleave and change the channel's value between capture and
    // the bracket's own temp apply. Restored AFTER the stripped-Active push
    // below, via `set_config_u32` directly (raw native units) -- never
    // through `apply_params_to_hardware_locked` (a deliberate design
    // safeguard/future-proofing choice, not a workaround for a present-day
    // double-conversion bug -- see `capture_channel_wide_hardware_locked`'s
    // own doc comment). A key with no captured pair (omitted by
    // `capture_channel_wide_hardware_locked` on a `GET_CONFIG` failure) is
    // NOT stripped from the Active push below (edge-case-hunter adversarial
    // review, PR #101, BLOCKING Finding 1) -- it instead falls back to its
    // pre-round-18 behavior, restored from this CLL's own Active alongside
    // every other non-channel-wide key, rather than being left with no
    // restore source at all. See
    // `comparam_support::strip_captured_channel_wide_keys`'s own doc
    // comment for the full fallback design.
    channel_wide_restore: &[(u32, u32)],
) {
    let active = logical_links
        .lock()
        .await
        .get(&cll_handle)
        .map(|l| l.active.clone());
    if let Some(active) = active {
        let active = comparam_support::strip_bustype_keys(&active);
        let captured_cfgs: HashSet<u32> =
            channel_wide_restore.iter().map(|&(cfg, _)| cfg).collect();
        let active = comparam_support::strip_captured_channel_wide_keys(
            &active,
            hw_protocol_id,
            &captured_cfgs,
        );
        let _ = apply_params_to_hardware_locked(api, channel_id, hw_protocol_id, &active).await;
    }
    for &(cfg, value) in channel_wide_restore {
        if let Err(err) = api.set_config_u32(channel_id, cfg, value) {
            warn!(
                channel_id = channel_id.0,
                cfg,
                value,
                %err,
                "failed to restore the captured pre-bracket hardware value for a channel-wide \
                 ComParam after a temp-bound revert (best-effort restore only)"
            );
        }
    }
}

/// Applies all J2534-standard unum32 params from `params` to the hardware
/// channel in a single `PassThruIoctl SET_CONFIG` call, skipping
/// service-specific IDs, IDs unsupported on `hw_protocol_id` (ADR-028), and
/// `DATA_RATE`.
///
/// `DATA_RATE` is set once during `PassThruConnect` and cannot be changed on an
/// established connection via SET_CONFIG (most adapters reject it).  Skipping it
/// here prevents spurious `PduErrEvtProtErr` events when the caller only intends
/// to update timing parameters.  See ADR-011.
///
/// A `DATA_BITS` entry originating from `CP_UartConfig` is split into
/// `DATA_BITS` + `PARITY` by `expand_uart_config` (ADR-071), and a `TIDLE`
/// entry additionally derives a `W0` (ISO9141) or `W5` (ISO14230) entry via
/// `expand_tidle` (ADR-072), before being sent. Both expansions run on
/// still-unconverted (ISO 22900-2 microsecond) values -- `to_j2534_config_value`
/// runs last, once per final entry, so a derived entry is converted exactly
/// once like every other.
///
/// `hw_protocol_id` (ADR-158 correction): always the Plane A (raw,
/// un-normalized) hardware protocol id. Every downstream consumer
/// (`to_j2534_config_id`, `expand_tidle`) self-normalizes internally --
/// callers never need to choose which id to pass, and passing the raw id
/// unconditionally is what lets `to_j2534_config_id` detect an FD_CAN_PS
/// link and suppress `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` there (clause
/// 21.3.2.5.1).
///
/// Returns `true` when the batch was accepted by the adapter, `false` if the
/// `SET_CONFIG` call failed (logged).
///
/// Thin wrapper over [`apply_params_to_hardware_locked`] that locks `api`
/// itself -- used by every call site except `handle_update_param`'s
/// lock-conflict-resolution section (ADR-110 amendment, Finding 2), which
/// must keep `api` held across both the lock-conflict read and this push
/// and so calls the inner function directly with its own already-held guard.
///
/// `pub(super)` (Fix 5, Codex review, P2, PR #101, ADR-192/Phase 7 Stage
/// 7c): also called directly from `rpc_primitive.rs`'s
/// `rpc_start_com_primitive`, to temp-apply a `ParamBinding::Temp` COP's
/// Working snapshot around the native TP2.0 broadcast periodic-message
/// start -- the same "borrow Working for one TX then revert" bracket
/// `handle_send_recv` runs around an ordinary transmit, just for a native
/// periodic-message start instead of a `PassThruWriteMsgs` call.
pub(super) async fn apply_params_to_hardware(
    api: &Arc<Mutex<J2534Api0404>>,
    channel_id: ChannelId,
    hw_protocol_id: u32,
    params: &ComParamSet,
) -> bool {
    let api = api.lock().await;
    apply_params_to_hardware_locked(&api, channel_id, hw_protocol_id, params).await
}

/// Same as [`apply_params_to_hardware`], but takes an already-locked
/// `&J2534Api0404` instead of the `Arc<Mutex<...>>` wrapper (ADR-110
/// amendment, Finding 2): lets `handle_update_param` compute the
/// lock-conflict exclusion and perform this hardware push inside one
/// continuously-held `api` critical section, serialized against
/// `rpc_lock_resource`'s own now-api-held grant.
///
/// `pub(super)` (Codex review round 8 Finding 1, PR #101, ADR-192/Phase 7
/// Stage 7c): also called directly from `rpc_primitive.rs`'s
/// `rpc_start_com_primitive`, as part of the single continuously-held
/// `api` guard that now spans the TP2.0 broadcast-periodic `ParamBinding::
/// Temp` apply, the native `start_periodic_message` call, and the revert
/// via [`revert_hardware_to_live_active_locked`] -- see that bracket's own
/// comments for why the whole sequence must stay serialized against a
/// sibling CLL's own temp-bound broadcast start or a queued
/// `CoptUpdateparam` on the same physical channel.
///
/// `hw_protocol_id`: same Plane A (raw) contract as
/// [`apply_params_to_hardware`] -- see its doc comment.
pub(super) async fn apply_params_to_hardware_locked(
    api: &J2534Api0404,
    channel_id: ChannelId,
    hw_protocol_id: u32,
    params: &ComParamSet,
) -> bool {
    // Skip params with no J2534 SET_CONFIG equivalent for this protocol
    // (service-specific ComParam IDs, or native IDs unsupported on
    // hw_protocol_id -- see ADR-028); DATA_RATE (baud rate is fixed at
    // PassThruConnect time); and CONFIG_SAMPLES_PER_READING/
    // CONFIG_READINGS_PER_MSG (ADR-216 Decision item 10 fix): SAE J2534-2
    // clause 10.3.3.2.3/.2.4 require these two to be configured only while
    // CP_AnalogSampleRate is still zero, and CP_AnalogSampleRate can never
    // go back to zero on an already-connected analog link (the guard in
    // rpc_primitive.rs above this one), so a post-connect SET_CONFIG
    // touching either id is never legitimate regardless of the value --
    // exactly like DATA_RATE, both are connect-time-latched and forwarded
    // exactly once, via `apply_j2534_params` at PassThruConnect time, never
    // again via this post-connect path. Excluding them here (rather than
    // relying solely on a value-comparison guard in rpc_primitive.rs) is
    // required because this filter performs a blanket forward of every
    // `unum32` param present in `params`, not a delta of what changed --
    // and `comparam_defaults.rs`'s `analog_in()` unconditionally seeds both
    // ids into every analog CLL's Working set at creation, so without this
    // exclusion they would be re-forwarded on every single analog
    // CoptUpdateparam, including ones that only touch
    // CP_AnalogActiveChannels/CP_AnalogAveragingMethod, defeating those two
    // ComParams' own unrestricted CoptUpdateparam path (ADR-216 Decision
    // item 10).  See ADR-011.  Both `to_j2534_config_id` and `expand_tidle`
    // below self-normalize `hw_protocol_id` internally (ADR-158
    // correction), so this function itself needs no base-id computation of
    // its own.
    let configs: Vec<(u32, u32)> = params
        .unum32
        .iter()
        .filter_map(|(&param_id, &value)| {
            let config_id = param_id.to_j2534_config_id(hw_protocol_id)?;
            Some((config_id, value))
        })
        .filter(|&(config_id, _)| {
            config_id != j2534_0404::DATA_RATE
                && config_id != j2534_0404::CONFIG_SAMPLES_PER_READING
                && config_id != j2534_0404::CONFIG_READINGS_PER_MSG
        })
        .collect();
    let configs = expand_uart_config(configs);
    let configs = expand_tidle(configs, hw_protocol_id);
    let configs: Vec<(u32, u32)> = configs
        .into_iter()
        .map(|(config_id, value)| (config_id, to_j2534_config_value(config_id, value)))
        .collect();
    if configs.is_empty() {
        return true;
    }
    if let Err(err) = api.set_config(channel_id, &configs) {
        warn!(?configs, %err, "set_config failed");
        return false;
    }
    true
}

/// Same as [`apply_params_to_hardware`], but additionally captures each
/// `comparam_support::CHANNEL_WIDE_UNUM32` key's pre-apply hardware value
/// (via [`capture_channel_wide_hardware_locked`]) BEFORE performing the
/// apply, both under one continuously-held `api` guard (round 18, Codex
/// review, P2, PR #101, ADR-192 Decision item 3 amendment).
///
/// For split-bracket callers that cannot hold `api` continuously all the way
/// from apply to their own later revert call (unlike `handle_send_recv`'s/
/// `rpc_start_com_primitive`'s continuous-bracket sites, which call
/// [`capture_channel_wide_hardware_locked`] directly under their own
/// already-held guard) -- e.g. `handle_start_comm`. Channel-wide value
/// capture only needs to happen-before the temp apply, not be held
/// continuously past it, unless a concurrent operation could change the
/// channel-wide value in the gap between this call and the caller's own
/// later revert; see the call site for whether that residual is closed or
/// accepted.
///
/// Returns `(applied_ok, channel_wide_restore)` -- thread `channel_wide_restore`
/// through to every later `revert_hardware_to_live_active`/
/// `revert_hardware_to_live_active_locked` call in the same bracket.
pub(super) async fn apply_params_to_hardware_capturing(
    api: &Arc<Mutex<J2534Api0404>>,
    channel_id: ChannelId,
    hw_protocol_id: u32,
    params: &ComParamSet,
) -> (bool, Vec<(u32, u32)>) {
    let api = api.lock().await;
    let channel_wide_restore =
        capture_channel_wide_hardware_locked(&api, channel_id, hw_protocol_id).await;
    let applied = apply_params_to_hardware_locked(&api, channel_id, hw_protocol_id, params).await;
    (applied, channel_wide_restore)
}

/// Processes `TxItem::UpdateParam` in the poll task (`CoptUpdateparam`):
///
/// 1. Applies `params` -- the Working snapshot `rpc_start_com_primitive`
///    captured at `StartComPrimitive` call time (ADR-067 claim F) -- to the
///    hardware via `PassThruIoctl SET_CONFIG` (service-specific params are
///    skipped). A `SetComParam` issued after this call must not be promoted
///    by it.
/// 2. Only on success, sets `LogicalLinkState.active = params` so the Active
///    snapshot stays in sync (in-memory Active still updates at execution
///    time on hw success -- unchanged from before ADR-067), and promotes
///    `unique_resp_id_table` -- the Working UniqueRespIdTable snapshot bound
///    at the same call time -- to `LogicalLinkState.active_unique_resp_id_table`
///    via `J2534Service::promote_unique_resp_id_table`, reconciling ISO15765
///    `FLOW_CONTROL_FILTER`s (diff-gated: a com-param-only `CoptUpdateparam`
///    does no filter I/O) (ADR-068). ADR-162 Decision 2: under native-mixed
///    CAN mode, this promotion can itself reject (`Err`) a table that would
///    introduce a `CP_CanRespUUDTId`/`CP_CanRespUSDTId` match-key collision
///    on the shared physical channel -- nothing is written in that case (old
///    table and filters stay installed), and this function emits
///    `PDU_ERR_EVT_PROT_ERR` for it rather than failing the COP (see step 4).
/// 3. When this CLL's `comm_started` is `true` (regardless of whether the
///    UniqueRespIdTable promotion in step 2 succeeded or was rejected --
///    the ComParam promotion driving tester-present resolution already
///    applied either way), re-resolves tester-present from the
///    newly-promoted Active set (ADR-084, extended to
///    `CP_TesterPresentSendType = 0` by this diff). A resolution with
///    different on-wire behavior than what's currently armed immediately
///    sends one P3-gated frame and re-arms `tester_present_state` from it --
///    the same "just became enabled -> send now" contract `CoptStartcomm`'s
///    own arm gives. A resolution that yields a disabled tester-present, or
///    unchanged on-wire behavior, is left untouched (a stale keep-alive is
///    left running rather than silently disarmed).
/// 4. Emits `PduCopstExecuting` then `PduCopstFinished` -- the table
///    rejection in step 2 does not change the COP's terminal status
///    (`PduComPrimitiveStatus` has no failed variant); a client observes it
///    only via the `PDU_ERR_EVT_PROT_ERR` error event.
async fn handle_update_param(
    cop_handle: u32,
    cll_handle: u32,
    params: ComParamSet,
    unique_resp_id_table: Vec<EcuUniqueRespEntry>,
    connect_generation: u64,
    ctx: &ChannelPollCtx,
) {
    // ADR-118 (A2-24) / ADR-205: EXECUTING is gated atomically on the COP
    // still being live in `primitives`, guard held continuously from the
    // liveness check through the emission -- every CANCELLED emitter removes
    // the entry under `primitives` before emitting, so EXECUTING can never be
    // observed after CANCELLED. An absent entry means a concurrent
    // cancel/teardown already claimed this COP and emitted its own terminal
    // status -- bail before touching hardware; nothing to clean up. `cop_tag`
    // (ADR-205 Decision item 1) is captured here, once, from this same live
    // `CopEntry` read, and kept alive for the rest of this function (cloned
    // into every `send_error_event`/`frame_tester_present_data`/
    // `send_tester_present_once`/`ExpectedResponseWait` call below) rather
    // than re-resolved fresh at each one -- see `handle_send_recv`'s
    // identical hoist for the full rationale.
    let cop_tag = {
        let queue_target = resolve_queue_target(&ctx.logical_links, cll_handle).await;
        let prims = ctx.primitives.lock().await;
        let Some(entry) = prims.get(&cop_handle) else {
            return;
        };
        let cop_tag = entry.cop_tag.clone();
        send_cop_status(
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            queue_target.as_ref(),
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstExecuting,
            cop_tag.clone(),
        )
        .await;
        cop_tag
    };

    // ADR-180 Decision 15 round-17 correction (Codex review, PR #72): a
    // sequential `shared_channels` pre-read, taken BEFORE `api` is locked
    // below -- ADR-080-conformant (never nested under `api`/`logical_links`)
    // -- of whether this CLL currently holds a live J1939 claim under
    // `SharedChannel::j1939_claims` for `(cll_handle, connect_generation)`.
    // This feeds the `claim_owns_address` re-check further down, moved here
    // (before the hardware push) to close a partial-apply bug: that check
    // used to run only AFTER `apply_params_to_hardware_locked` already
    // pushed every OTHER staged ComParam in this same `hw_set` to the
    // adapter, so a rejection left the adapter holding new values for
    // everything but the two conflicting J1939 fields while Active (and
    // `GetComParam`) kept reporting the old ones -- directly contradicting
    // Decision 15's own "terminate the whole COP" rejection of a partial
    // apply. This pre-read is still just as authoritative as a read taken
    // at the promotion instant (Decision 15's whole point): the poll task
    // serializes this whole function against every mutator that could flip
    // it dangerously -- `run_j1939_claim_loop` (`events_j1939_claim.rs`) is
    // the SOLE insertion site for `j1939_claims`, driven only by
    // `handle_start_comm`/`run_j1939_reclaim_duties`, neither of which can
    // run while this function is executing on the same poll task. The only
    // OFF-poll-task mutators are RPC teardown removals
    // (`cancel_j1939_claims_for_cll` from disconnect/destroy), which bump
    // `connect_generation` and are already caught by this function's
    // existing U1/U2 generation rechecks below. If a future change ever adds
    // an RPC-side (non-poll-task) `j1939_claims` insertion path, this
    // invariant -- and this pre-read's authority -- would need revisiting;
    // `rg 'j1939_claims\.insert|comm_started = true' j2534-0404-service/src`
    // is the cheap way to re-verify it still holds.
    // ADR-180 Decision 23 round-26 correction: widened from a plain
    // membership bool to the actually-defended NAME itself, resolved from
    // EITHER a live `j1939_claims` entry OR a pending reclaim
    // (`j1939_reclaim_pending`) for this exact `(cll_handle,
    // connect_generation)` -- mirrors `rpc_primitive.rs`'s identical
    // enqueue-time `live_defended_name` widening; see that guard's own
    // comment for why a pending reclaim counts too (a restage landing
    // during the loss-to-reclaim window must not bypass this check).
    let live_defended_name: Option<[u8; 8]> = {
        let chans = ctx.service.shared_channels.lock().await;
        chans
            .values()
            .find(|sc| sc.channel_id == ctx.channel_id)
            .and_then(|sc| {
                sc.j1939_claims
                    .values()
                    .find(|entry| {
                        entry.cll_handle == cll_handle
                            && entry.connect_generation == connect_generation
                    })
                    .map(|entry| entry.name)
                    .or_else(|| {
                        sc.j1939_reclaim_pending
                            .get(&cll_handle)
                            .and_then(|pending| {
                                (pending.connect_generation == connect_generation)
                                    .then_some(pending.name)
                            })
                    })
            })
    };

    // ADR-110 amendment ("Lock-grant/apply serialization", Finding 2 --
    // Codex review on PR #116): `api` is acquired FIRST, before
    // `logical_links`, and held through the lock-conflict read below and the
    // hardware push in the `Some(Some(...))` arm -- serializing this section
    // against `rpc_lock_resource`'s own now-api-held grant. Without this, a
    // grant could read `locked_by_other=false` here, this task could then
    // suspend on the (contended) `api` mutex, a concurrent `LockResource`
    // could complete and return success to its caller during that
    // suspension, and this task could then resume and push a stale,
    // unfiltered `hw_set` to hardware -- clobbering exactly what the
    // just-granted lock was supposed to protect, with no
    // `PDU_ERR_EVT_RSC_LOCKED` event. Holding `api` across both the read and
    // the push makes only two orderings possible: either the grant completes
    // (and is visible to its caller) before this section acquires `api` --
    // in which case the fresh `find_physical_lock_holder` read below sees it
    // and excludes correctly -- or the grant cannot complete until after
    // this section releases `api`, i.e. after the push has already
    // happened, so the grant only ever protects from that point forward,
    // never retroactively. New lock-ordering invariant introduced by this
    // amendment: `api` before `logical_links` whenever both are held
    // together -- `logical_links` must never be held across an
    // `api.lock().await` acquisition (crate-wide sweep recorded in ADR-110's
    // amendment). The `None`/`Some(None)` arms below don't need `api` at
    // all and drop it immediately.
    let api = ctx.api.lock().await;

    // `hw_protocol_id`, not `protocol.j2534_protocol_id()`: the actual
    // connected hardware protocol may diverge from the CLL's service-level
    // identity (software-ISO-TP raw CAN, ADR-046; SAE_J1850 VPW/PWM
    // auto-detect, ADR-070; SAE_J2610_on_SAE_J2610_SCI's per-row hardware
    // override, pin-typing amendment) -- SET_CONFIG gating must follow the
    // hardware actually in use, exactly like `apply_j2534_params` already
    // does at `ConnectComLogicalLink` time.
    //
    // U1 (pre-apply, ADR-086), folded into this same critical section: the
    // outer `Option` distinguishes "link missing" (unchanged behavior --
    // falls through to the normal `PduCopstFinished` below, nothing to
    // update) from "link present"; the inner `Option` then distinguishes
    // stale (`None`, a new first-wins bail below, before the SET_CONFIG
    // IOCTL is ever issued) from live (`Some`, proceed as before).
    //
    // ADR-110: the same critical section also captures the live Active
    // snapshot and whether another CLL currently holds
    // `LOCK_PHYSICAL_COM_PARAMS` on this physical resource -- ISO 22900-2
    // §9.4.16 d) resolves the lock conflict "after a PDU_COPT_UPDATEPARAM"
    // (i.e. at execution time, not at `StartComPrimitive` call time), so this
    // must be re-read here rather than reused from a call-time check. Now
    // read with `api` already held above (Finding 2 amendment).
    let live_ctx: Option<Option<(u32, ComParamSet, bool, bool)>> = {
        let links = ctx.logical_links.lock().await;
        links.get(&cll_handle).map(|l| {
            if l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation {
                let locked_by_other = find_physical_lock_holder(
                    &links,
                    cll_handle,
                    l.hw_protocol_id,
                    l.pin_select,
                    l.channel_key,
                    LOCK_PHYSICAL_COM_PARAMS,
                )
                .is_some();
                // ADR-158 (corrected, supersedes this comment's prior
                // ADR-157 Plane B framing): the raw hw id, same as
                // `find_physical_lock_holder` just above -- both
                // `to_j2534_config_id` and `expand_tidle`, the two
                // protocol-family-sensitive consumers
                // `apply_params_to_hardware_locked` drives, self-normalize
                // internally from a single raw id, so passing the raw id
                // here costs nothing for a non-FD link and lets
                // `to_j2534_config_id` correctly suppress
                // `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` (clause 21.3.2.5.1)
                // for a `CoptUpdateparam` push on an FD_CAN_PS link.
                // `l.comm_started` -- the LIVE value at this same poll-task
                // pass, not any enqueue-time snapshot a caller may have
                // captured (e.g. `rpc_primitive.rs`'s own
                // `CoptUpdateparam` enqueue-time guard) -- feeds the
                // CP_J1939TargetAddress re-check below, mirroring why
                // `l.active` itself is captured live here rather than reused
                // from a call-time read.
                Some((
                    l.hw_protocol_id,
                    l.active.clone(),
                    locked_by_other,
                    l.comm_started,
                ))
            } else {
                None
            }
        })
    };

    match live_ctx {
        None => {
            // Link missing: nothing to update -- same as before this
            // amendment, falls through to the unconditional PduCopstFinished
            // at the end of this function. `api` is not needed for this arm.
            drop(api);
        }
        Some(None) => {
            // U1 bail-out (ADR-086): stale before any hardware write --
            // first-wins idiom, same as every other guard in this file.
            // `api` is not needed for this bail.
            drop(api);
            emit_terminal_if_live(
                &ctx.primitives,
                &ctx.logical_links,
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
                cll_handle,
                cop_handle,
                PduComPrimitiveStatus::PduCopstCancelled,
            )
            .await;
            return;
        }
        Some(Some((j2534_protocol_id, active, locked_by_other, live_comm_started))) => {
            // ADR-180 Decision 15 round-17 correction (Codex review, PR #72):
            // both J1939 execution-time re-checks below now run BEFORE the
            // hardware push (`apply_bustype_lock`/`apply_params_to_hardware_locked`
            // further down), not after -- closing a partial-apply bug where a
            // rejection here used to leave the adapter already holding every
            // OTHER staged ComParam from this same `hw_set`, contradicting
            // Decision 15's own "terminate the whole COP" rejection shape.
            // `claim_owns_address`'s claim-ownership conjunct now reuses
            // `live_defended_name`, the sequential `shared_channels`
            // pre-read taken above (before `api` was locked) -- see that
            // pre-read's own comment for why it is still just as
            // authoritative here as a fresh read would be. `api` is already
            // held at this point (Finding 2 amendment), so each rejection
            // below must `drop(api)` first -- unlike this check's old
            // position, which used to run after `drop(api)`.
            let staged_node_address = params
                .unum32
                .get(&ComParamId(j2534_0404::NODE_ADDRESS))
                .copied()
                .unwrap_or(0xF1);
            let active_node_address = active
                .unum32
                .get(&ComParamId(j2534_0404::NODE_ADDRESS))
                .copied()
                .unwrap_or(0xF1);
            let claim_owns_address =
                staged_node_address != active_node_address && live_defended_name.is_some();
            if claim_owns_address {
                // Terminate the whole COP -- NOT a partial-apply/
                // strip-just-NODE_ADDRESS shape (see the pre-read's own
                // comment above for why). `api` is dropped here for
                // prompt-release hygiene before the event emission below,
                // not because any lock-ordering rule forces it -- the
                // ADR-110 amendment sanctions holding `api` outer while
                // `send_error_event` re-locks `ctx.logical_links` inner
                // (see `handle_update_param`'s own `api`-held critical
                // section above for the established precedent); this
                // function simply has no further use for `api` past this
                // point.
                drop(api);
                send_error_event(
                    &ctx.subscriptions,
                    &ctx.logical_links,
                    cll_handle,
                    PduErrorEvent::PduErrEvtProtErr,
                    Some((cop_handle, cop_tag.clone())),
                )
                .await;
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstCancelled,
                )
                .await;
                return;
            }

            // ADR-180 Decision 23 (design-advisor consult, Codex review PR
            // #72; round-26 correction): a live claim's CP_J1939Name must
            // not silently drift onto Active via this CoptUpdateparam --
            // see the enqueue-time guard's own comment (`rpc_primitive.rs`)
            // for the full rationale; this is that guard's Decision-15-
            // shaped execution-time counterpart, closing the identical
            // TOCTOU class (a NAME-staging CoptUpdateparam enqueued before
            // an in-flight claim attempt completes, landing after).
            // Compared against `live_defended_name` -- the actually-issued
            // NAME from a live claim or pending reclaim -- rather than
            // Active's own `CP_J1939Name`: those can diverge for a
            // `temp_param_update = 1` `CoptStartcomm` that claimed under a
            // Working/Temp-bound NAME never promoted to Active.
            // `params.bytes.get(&PARAM_J1939_NAME)` being `None` (the
            // ComParam never staged into Working at all) passes
            // unconditionally, mirroring the enqueue-time guard's own
            // `None`-passes semantics -- only an actually-staged value
            // (including an explicit empty restage, normalized to all-zero)
            // is compared.
            let staged_j1939_name = params.bytes.get(&PARAM_J1939_NAME);
            let name_owns_address = live_defended_name.is_some_and(|defended| {
                staged_j1939_name
                    .is_some_and(|staged| normalize_j1939_name(Some(staged)) != defended)
            });
            if name_owns_address {
                // Same "terminate the whole COP" shape as the NODE_ADDRESS
                // re-check above, for the same reason.
                drop(api);
                send_error_event(
                    &ctx.subscriptions,
                    &ctx.logical_links,
                    cll_handle,
                    PduErrorEvent::PduErrEvtProtErr,
                    Some((cop_handle, cop_tag.clone())),
                )
                .await;
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstCancelled,
                )
                .await;
                return;
            }

            // ADR-180 Decision 15's own established pattern, extended to
            // CP_J1939TargetAddress -- same shape as the NODE_ADDRESS
            // re-check above, moved here for the same round-17 reason.
            let stages_unconfigured_j1939_target =
                resources::is_j1939_protocol_id(j2534_protocol_id)
                    && live_comm_started
                    && params.unum32.get(&PARAM_J1939_TARGET_ADDRESS).copied() == Some(0xFFFF);
            if stages_unconfigured_j1939_target {
                // Same "terminate the whole COP" shape as the NODE_ADDRESS
                // re-check above, for the same reason.
                drop(api);
                send_error_event(
                    &ctx.subscriptions,
                    &ctx.logical_links,
                    cll_handle,
                    PduErrorEvent::PduErrEvtProtErr,
                    Some((cop_handle, cop_tag.clone())),
                )
                .await;
                emit_terminal_if_live(
                    &ctx.primitives,
                    &ctx.logical_links,
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstCancelled,
                )
                .await;
                return;
            }

            // ADR-110 (ISO 22900-2 §9.4.16 d)): a non-owning CLL's
            // CoptUpdateparam is never synchronously rejected by the
            // physical-ComParam lock. Instead (corrected design, after a
            // confirmed regression in the original single-set design): every
            // PDU_PC_BUSTYPE-class key is excluded from the hardware push
            // (`hw_set`) unconditionally whenever locked -- own
            // Working-vs-Active agreement can never prove a write is safe
            // against another CLL's real, already-pushed hardware state --
            // while the promoted Active set (`promote_set`) substitutes each
            // excluded key with this CLL's own pre-call Active value, and
            // exactly one PDU_ERR_EVT_RSC_LOCKED error event is generated
            // only when this CLL actually attempted a BUSTYPE change of its
            // own. The COP still finishes normally either way.
            let comparam_support::BustypeLockResolution {
                mut hw_set,
                promote_set,
                rsc_locked,
            } = comparam_support::apply_bustype_lock(&params, &active, locked_by_other);
            // ADR-216 Decision item 10 amendment (Codex review, PR #130,
            // Finding 2): change-only forward for CP_AnalogActiveChannels/
            // CP_AnalogAveragingMethod -- strip either key from `hw_set`
            // when THIS CLL's own staged value is unchanged from its own
            // `active`, closing a cross-sibling stale-value clobber a
            // blanket forward would otherwise cause (see
            // `strip_unchanged_analog_channel_wide_keys`'s own doc comment).
            // Must run after `apply_bustype_lock` (it operates on the
            // already-lock-resolved `hw_set`, not the raw `params`) and
            // before the hardware push just below.
            comparam_support::strip_unchanged_analog_channel_wide_keys(&mut hw_set, &active);
            // Still under `api` (Finding 2 amendment): the hardware push
            // happens in the SAME critical section as the lock-conflict read
            // above, via the already-locked variant, so no concurrent
            // `LockResource` grant can land between the read and this push.
            let all_ok =
                apply_params_to_hardware_locked(&api, ctx.channel_id, j2534_protocol_id, &hw_set)
                    .await;
            // `api` is released here, ahead of the error event and the
            // `all_ok` branch below, for prompt-release hygiene -- neither
            // needs it once the push above has completed. This is not
            // required by any lock-ordering rule: the ADR-110 amendment
            // sanctions holding `api` outer while `send_error_event`
            // re-locks `ctx.logical_links` inner (see `handle_update_param`'s
            // own `api`-held critical section above); dropping promptly here
            // is simply good hygiene once the hardware push is done.
            drop(api);
            // ADR-110 (ISO 22900-2 §9.4.16 d)): the lock error event is
            // emitted only after the hardware-apply attempt above for every
            // other (non-conflicting) ComParam has actually happened --
            // regardless of whether that attempt succeeded or failed
            // (`all_ok`) -- matching the order in which the spec describes
            // these steps (one PDU_ERR_EVT_RSC_LOCKED error event, then the
            // remaining ComParams applied, then PDU_COPST_FINISHED) and
            // avoiding a subscriber observing the
            // lock-conflict error while the non-conflicting params are still
            // unapplied. "Exactly one event" is unchanged -- still gated
            // solely by the same `rsc_locked` bool computed above, just
            // relocated to run after the apply attempt.
            // Recheck generation before emitting the lock error (ADR-086,
            // mirroring the `!all_ok` branch's own `still_on_this_channel`
            // recheck below, "ADR-086, round 10 fix"; Codex review finding,
            // PR #116, P1). `apply_params_to_hardware_locked` above is a
            // real hardware `.await` a disconnect+reconnect of this exact
            // `cll_handle` can complete during, even though U1 already
            // checked before it started. Without this check, a stale COP's
            // `PDU_ERR_EVT_RSC_LOCKED` would land in the *newly reconnected*
            // session's event queue/`last_error` -- a session that has
            // nothing to do with this stale COP's lock conflict. Gating
            // only, not a bail: unlike the `!all_ok` branch, this must not
            // return out of the whole function -- the `all_ok`/`!all_ok`
            // branches below still run their OWN independent staleness
            // handling (U2's re-arm recheck, or this branch's own
            // `still_on_this_channel` bail) regardless of whether this event
            // fired.
            let still_on_this_channel_for_lock = {
                let links = ctx.logical_links.lock().await;
                links.get(&cll_handle).is_some_and(|l| {
                    l.channel_id == Some(ctx.channel_id)
                        && l.connect_generation == connect_generation
                })
            };
            if rsc_locked && still_on_this_channel_for_lock {
                send_error_event(
                    &ctx.subscriptions,
                    &ctx.logical_links,
                    cll_handle,
                    PduErrorEvent::PduErrEvtRscLocked,
                    Some((cop_handle, cop_tag.clone())),
                )
                .await;
            }
            if all_ok {
                // Working (call-time snapshot) → Active only when every
                // parameter was accepted by the adapter. The re-arm snapshot
                // below (channel/comm_started/protocol/tester-present identity)
                // is taken in this SAME critical section, immediately alongside
                // the promotion write -- no `.await` sits between them, so a
                // concurrent Disconnect/CoptStopcomm/re-arm cannot land in
                // between the promotion and the snapshot it gates on.
                //
                // U2 (pre-promotion, ADR-086), folded into this same critical
                // section: `apply_params_to_hardware` above is a real hardware
                // I/O `.await` -- a disconnect+reconnect can complete during it
                // even though U1 already checked before it started. The outer
                // `Option` mirrors U1 (link missing vs. present); the inner
                // `Option` is `None` when stale (skips the `active` write
                // itself, computed as part of the same predicate) and `Some`
                // when live. NOTE (ADR-086 residual, mirroring the documented
                // Temp-binding-revert asymmetry elsewhere in this file): the
                // hardware SET_CONFIG U1 already issued before this point is an
                // accepted channel-scoped residual on a stale bail here --
                // `CoptUpdateparam` has no revert obligation the way a `Temp`
                // binding does.
                //
                // ADR-147: collects a `(cll_handle, channel_key)` wake target
                // when this promotion's own `CP_SuspendQueueOnError` clear
                // (below, inside the same critical section as the `active`
                // write) flips `tx_suspended()` true -> false -- sent AFTER
                // `links` is released, mirroring `ioctl_resume_tx_queue`'s
                // lock-then-release-then-send shape.
                let mut queue_error_wake_target: Option<(u32, ChannelKey)> = None;
                let rearm_snapshot: Option<Option<_>> = {
                    let mut links = ctx.logical_links.lock().await;
                    links.get_mut(&cll_handle).map(|link| {
                        let channel_ok = link.channel_id == Some(ctx.channel_id)
                            && link.connect_generation == connect_generation;
                        if !channel_ok {
                            None
                        } else {
                            link.active = promote_set.clone();
                            // ADR-147: a `CoptUpdateparam` promotion landing
                            // Active `CP_SuspendQueueOnError == 0` is one of
                            // the four explicit escapes from an
                            // error-triggered suspension -- clear it here, in
                            // the same critical section as the promotion
                            // write itself, via
                            // `LogicalLinkState::clear_error_suspension`,
                            // which bumps `error_clear_seq`
                            // UNCONDITIONALLY (third amendment,
                            // capture-at-fold sequencing) -- this closes the
                            // companion-vs-promotion race: a concurrent
                            // UUDT-companion poll task's pass may have
                            // already classified `Suspend` from its own
                            // frame batch before this promotion landed, and
                            // that pass's end-of-pass writeback (which only
                            // runs later) must not re-apply a classification
                            // whose fold predates this explicit clear -- the
                            // bump invalidates it by mismatching the fold-
                            // time-captured `CllRxEntry::suspend_seq`
                            // against the now-bumped live value (a
                            // `Positive` classification is unaffected: its
                            // own anchor is batch-read time against
                            // `error_set_seq`, ADR-147 fifth amendment, never
                            // this counter). The live-policy re-check below
                            // (`old_effective`
                            // vs. the post-clear `tx_suspended()`) remains
                            // an independent safety net for the wake
                            // decision, unchanged.
                            if !link.active.suspend_queue_on_error() {
                                let old_effective = link.tx_suspended();
                                link.clear_error_suspension();
                                if old_effective
                                    && !link.tx_suspended()
                                    && let Some(channel_key) = link.channel_key
                                {
                                    queue_error_wake_target = Some((cll_handle, channel_key));
                                }
                            }
                            let old_resolved = match &link.tester_present_state {
                                TesterPresentState::Armed { resolved, .. } => {
                                    Some(resolved.clone())
                                }
                                // Carries the resolved value forward from
                                // whichever `Armed` state PDU_IOCTL_
                                // CLEAR_PERIODIC_MSGS cleared (a third
                                // Codex-review finding): feeding this into
                                // `old_resolved` below is what makes
                                // `same_wire_behavior` correctly report
                                // "unchanged" for an unrelated promotion on a
                                // cleared CLL (blocking resurrection) while
                                // still correctly reporting "changed" for a
                                // promotion that actually reconfigures
                                // tester-present's wire content (correctly
                                // re-arming -- ADR-093's own intentional
                                // "reconfiguring after a clear is itself a
                                // re-enable" semantics). See
                                // `TesterPresentState::Cleared`'s doc comment.
                                TesterPresentState::Cleared { resolved, .. } => {
                                    Some(resolved.clone())
                                }
                                // ADR-137 second Codex-review fix: `Disarmed`
                                // deliberately carries no `resolved` at all
                                // (unlike `Cleared`) -- see
                                // `TesterPresentState::Disarmed`'s own doc
                                // comment for why a `resolved` baseline here
                                // would break the 0->1 re-enable contract.
                                // `None` here is what forces this promotion's
                                // `old_resolved` comparison to always report
                                // "changed" out of a disarmed state, the same
                                // as the never-armed `None` case just below.
                                TesterPresentState::Disarmed { .. } => None,
                                TesterPresentState::None => None,
                            };
                            Some((
                                channel_ok,
                                link.comm_started,
                                link.protocol,
                                link.software_isotp,
                                link.tester_present_base_tx_flags,
                                tester_present_state_token(&link.tester_present_state),
                                old_resolved,
                                // ADR-196 Decision items 1/2: threaded through
                                // to the `resolve_tester_present` re-arm call
                                // below, same as every other TX-flags/header
                                // resolution site.
                                link.raw_mode,
                            ))
                        }
                    })
                };
                // ADR-147: `links` is released above -- send the wake now, if
                // this promotion's own suspend-clear flipped `tx_suspended()`
                // true -> false.
                if let Some((wake_cll_handle, channel_key)) = queue_error_wake_target {
                    let tx_queue = {
                        let chans = ctx.service.shared_channels.lock().await;
                        chans.get(&channel_key).map(|sc| sc.tx_queue.clone())
                    };
                    if let Some(tx_queue) = tx_queue {
                        let _ = tx_queue.send(TxItem::ResumeWake {
                            cll_handle: wake_cll_handle,
                        });
                    }
                }
                // `None` (outer): link missing, unchanged existing behavior --
                // promote_unique_resp_id_table is still called unconditionally
                // below regardless, and the re-arm block is skipped. `Some(None)`
                // (inner): U2 bail-out (ADR-086) -- stale, skip the active write
                // (already skipped above, inside the same critical section),
                // skip promote_unique_resp_id_table entirely (avoids stale
                // FLOW_CONTROL_FILTER hardware I/O and an active-table write),
                // skip the re-arm block, first-wins bail. `Some(Some(tuple))`:
                // live, proceed as before.
                let rearm_snapshot = match rearm_snapshot {
                    None => None,
                    Some(None) => {
                        emit_terminal_if_live(
                            &ctx.primitives,
                            &ctx.logical_links,
                            &ctx.subscriptions,
                            &ctx.service.terminal_cops,
                            cll_handle,
                            cop_handle,
                            PduComPrimitiveStatus::PduCopstCancelled,
                        )
                        .await;
                        return;
                    }
                    Some(Some(tuple)) => Some(tuple),
                };
                let promote_result = ctx
                    .service
                    .promote_unique_resp_id_table(cll_handle, unique_resp_id_table.clone())
                    .await;

                // Recheck generation after promotion's own I/O (ADR-086
                // round 12 fix): on an ISO15765 link, `promote_unique_resp_id_table`
                // awaits FLOW_CONTROL_FILTER teardown/install -- a real hardware
                // `.await` a disconnect+reconnect can complete during, even
                // though U2 already checked immediately before it. Without this
                // recheck, a stale COP would resume straight into the
                // tester-present re-arm block below (using the pre-promotion
                // `rearm_snapshot`) and fall through to the unconditional
                // terminal `PduCopstFinished`, after `cancel_link_cops` may
                // already have cancelled it. Three-way, matching U1's own
                // "link missing" precedent: link missing (`None`) falls
                // through unchanged -- `PduCopstFinished` below, re-arm
                // already skipped via `rearm_snapshot`'s own outer `None`;
                // stale (`Some(false)`) bails first-wins with
                // `PduCopstCancelled` before the re-arm block or terminal
                // status; live (`Some(true)`) proceeds as before.
                let post_promotion_check: Option<bool> = {
                    let links = ctx.logical_links.lock().await;
                    links.get(&cll_handle).map(|l| {
                        l.channel_id == Some(ctx.channel_id)
                            && l.connect_generation == connect_generation
                    })
                };
                if post_promotion_check == Some(false) {
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstCancelled,
                    )
                    .await;
                    return;
                }

                // ADR-162 Decision 2: `promote_unique_resp_id_table` rejects a
                // table that would introduce a native-mixed-mode
                // CP_CanRespUUDTId/CP_CanRespUSDTId match-key collision.
                // `PduComPrimitiveStatus` has no failed variant (only
                // Idle/Executing/Finished/Cancelled/Waiting), so this does
                // NOT fail the COP -- it emits `PDU_ERR_EVT_PROT_ERR`
                // (mirroring the `PDU_ERR_EVT_RSC_LOCKED` precedent above)
                // and falls through unchanged to the rest of this
                // CoptUpdateparam (tester-present re-arm below, then the
                // unconditional terminal PduCopstFinished): the rest of this
                // call's ComParam promotion already applied and is not
                // undone by the table rejection. Gated on the same
                // staleness recheck as the bail just above
                // (`post_promotion_check == Some(true)`) so a stale COP
                // whose CLL reconnected during promotion does not emit into
                // the new session.
                if promote_result.is_err() && post_promotion_check == Some(true) {
                    send_error_event(
                        &ctx.subscriptions,
                        &ctx.logical_links,
                        cll_handle,
                        PduErrorEvent::PduErrEvtProtErr,
                        Some((cop_handle, cop_tag.clone())),
                    )
                    .await;
                }

                // Tester-present live re-arm (ADR-084, extended to
                // `CP_TesterPresentSendType = 0` by this diff -- see §6 of
                // the design brief: mode 0 can now be re-armed because it is
                // no longer hardware-driven): a CoptUpdateparam that promotes
                // tester-present-affecting ComParams to Active while this
                // CLL's comm is already started re-resolves tester-present
                // from the newly-active set and, if it has different on-wire
                // behavior than what's currently armed, immediately sends one
                // frame and re-arms -- the same "just became enabled -> send
                // now" contract CoptStartcomm's own arm gives (ADR-084).
                // Deliberately left untouched: an already-armed CLL whose new
                // resolution disables tester-present (the stale keep-alive
                // keeps running rather than being silently disarmed -- "some
                // keep-alive" beats "none"); and -- the common case -- a
                // `CoptUpdateparam` that promotes an entirely unrelated
                // ComParam (e.g. `CP_Loopback`) on a comm-started CLL, which
                // must NOT re-send a frame or reset the idle clock just
                // because *some* param was promoted (caught in review: an
                // earlier draft resolved and re-armed unconditionally on
                // every successful promotion, regardless of whether
                // tester-present's own resolved output actually changed).
                // The short-circuit below also skips calling
                // `resolve_tester_present` at all when tester-present was
                // never configured and still isn't (`token == None` and the
                // newly-promoted set's payload is still empty) -- otherwise a
                // stray out-of-range `CP_TesterPresentSendType` left in Working
                // by an unchecked `SetComParam` (ADR-084; `SetComParam` does not
                // range-check it) would spuriously fail resolution and emit
                // `PduErrEvtTesterPresentError` on every future promotion for a
                // CLL that never had a tester-present payload to send in the
                // first place.
                if let Some((
                    channel_ok,
                    comm_started,
                    protocol,
                    software_isotp,
                    base_tx_flags,
                    token,
                    old_resolved,
                    raw_mode,
                )) = rearm_snapshot
                    && channel_ok
                    && comm_started
                    && !(token == TesterPresentToken::None
                        && promote_set.tester_present_data().is_empty())
                {
                    match rpc_primitive::resolve_tester_present(
                        protocol,
                        j2534_protocol_id,
                        &promote_set,
                        &unique_resp_id_table,
                        software_isotp,
                        base_tx_flags,
                        raw_mode,
                    ) {
                        Ok(resolved) if !resolved.handling_enabled => {
                            // ADR-137: a live CP_TesterPresentHandling 1->0
                            // transition disarms rather than falling through
                            // to ADR-084's "keep the stale keep-alive
                            // running" fallthrough below -- handling is the
                            // spec's only in-band mechanism to stop
                            // tester-present without a full
                            // CoptStopcomm/CoptStartcomm cycle. No send, no
                            // P3-gate wait (disarming never transmits), no
                            // error event -- a successful reconfiguration, a
                            // no-op if the CLL was already
                            // `TesterPresentState::None`. Same re-validation
                            // discipline as the re-arm write-back below
                            // (`channel_id`/`connect_generation`/`comm_started`/
                            // token identity, all re-checked under the lock
                            // immediately before writing) to guard against a
                            // concurrent Disconnect/Destroy/CoptStopcomm/re-arm
                            // racing this async match.
                            let mut links = ctx.logical_links.lock().await;
                            if let Some(l) = links.get_mut(&cll_handle)
                                && l.channel_id == Some(ctx.channel_id)
                                && l.connect_generation == connect_generation
                                && l.comm_started
                                && tester_present_state_token(&l.tester_present_state) == token
                            {
                                // ADR-137 fourth Codex-review fix (round-4
                                // restructure): no longer carries anything
                                // forward here -- any still-open discard
                                // window already lives independently in
                                // `l.open_tp_discards`, which this write does
                                // not touch (see that field's own doc
                                // comment). The rounds 2/3 residual-carry
                                // `match` this replaced is gone entirely.
                                l.tester_present_state = TesterPresentState::Disarmed {
                                    disarmed_at: tokio::time::Instant::now(),
                                };
                            }
                        }
                        Ok(resolved)
                            if resolved.interval_ms > 0
                                && !old_resolved
                                    .as_ref()
                                    .is_some_and(|old| old.same_wire_behavior(&resolved)) =>
                        {
                            let framed_data = frame_tester_present_data(
                                &resolved,
                                cll_handle,
                                Some(connect_generation),
                                ctx,
                                Some((cop_handle, cop_tag.clone())),
                            )
                            .await;
                            if !framed_data.is_empty() {
                                match wait_for_p3_gap(
                                    cop_handle,
                                    cll_handle,
                                    resolved.can_functional,
                                    if resolved.expects_response { 1 } else { 0 },
                                    false,
                                    false,
                                    ctx,
                                )
                                .await
                                {
                                    P3GapOutcome::Ready => {
                                        // Re-validate post-`.await` (same
                                        // discipline as `handle_start_comm`'s and
                                        // `dispatch_due_tester_present`'s own
                                        // pre-send re-checks): a concurrent
                                        // Disconnect/Destroy/CoptStopcomm/re-arm
                                        // must not let a stale send land.
                                        let still_matches = {
                                            let links = ctx.logical_links.lock().await;
                                            links.get(&cll_handle).is_some_and(|l| {
                                                l.channel_id == Some(ctx.channel_id)
                                                    && l.connect_generation == connect_generation
                                                    && l.comm_started
                                                    && tester_present_state_token(
                                                        &l.tester_present_state,
                                                    ) == token
                                            })
                                        };
                                        if still_matches {
                                            let tx_flags = resolved.tx_flags;
                                            let can_functional = resolved.can_functional;
                                            let interval =
                                                Duration::from_millis(resolved.interval_ms as u64);
                                            // CP_P2Max from the newly-promoted
                                            // Active set (ADR-088 amendment) --
                                            // this re-arm's send happens against
                                            // `promote_set`, so its discard
                                            // window is sized from the same set.
                                            let p2_max_ms = promote_set.p2_max_timeout_ms();
                                            let expects_response = resolved.expects_response;
                                            let (fired_at, discard_window) =
                                                match send_tester_present_once(
                                                    cll_handle,
                                                    j2534_protocol_id,
                                                    tx_flags,
                                                    can_functional,
                                                    &framed_data,
                                                    expects_response,
                                                    Some(connect_generation),
                                                    ctx,
                                                    Some((cop_handle, cop_tag.clone())),
                                                )
                                                .await
                                                {
                                                    // ADR-099: window opens
                                                    // whenever this send merely
                                                    // succeeded (`Ok`),
                                                    // regardless of
                                                    // `expects_response` --
                                                    // TX-side indication
                                                    // frames are artifacts of
                                                    // the send itself, not of
                                                    // `CP_TesterPresentReqRsp`'s
                                                    // "does the ECU reply"
                                                    // semantics. `pos`/`neg`
                                                    // (from `resolved.exp_pos_resp`/
                                                    // `exp_neg_resp`, resolved
                                                    // from that same
                                                    // `promote_set` set
                                                    // `p2_max_ms` above uses,
                                                    // not a live re-read) stay
                                                    // frozen ONLY when
                                                    // `expects_response` (the
                                                    // same `promote_set`-
                                                    // resolved snapshot used
                                                    // for the send itself) was
                                                    // `true`;
                                                    // otherwise they freeze
                                                    // empty, which already can
                                                    // never content-match, so
                                                    // content/SOM-herald
                                                    // discard stays correctly
                                                    // gated on
                                                    // `expects_response` while
                                                    // the window itself now
                                                    // unconditionally opens
                                                    // for TX-side discard
                                                    // (`tx_can_id`).
                                                    Ok(t) => (
                                                        t,
                                                        Some(DiscardWindow {
                                                            until: t + Duration::from_millis(
                                                                p2_max_ms as u64,
                                                            ),
                                                            pos: if expects_response {
                                                                resolved.exp_pos_resp.clone()
                                                            } else {
                                                                Vec::new()
                                                            },
                                                            neg: if expects_response {
                                                                resolved.exp_neg_resp.clone()
                                                            } else {
                                                                Vec::new()
                                                            },
                                                        }),
                                                    ),
                                                    Err(t) => (t, None),
                                                };
                                            // ADR-137 fourth Codex-review fix
                                            // (round-4 restructure): built
                                            // into a self-contained
                                            // `ResidualTesterPresentDiscard`
                                            // here (before `resolved`/
                                            // `framed_data` move into the
                                            // `Armed` write below) and pushed
                                            // into `l.open_tp_discards`
                                            // instead of being threaded
                                            // through the `Armed` construction
                                            // itself.
                                            let new_open_tp_discard =
                                                discard_window.map(|window| {
                                                    ResidualTesterPresentDiscard {
                                                        target_can_ids: resolved.target_can_ids,
                                                        tx_can_id: tester_present_tx_can_id(
                                                            &resolved,
                                                            &framed_data,
                                                        ),
                                                        window,
                                                    }
                                                });
                                            let mut links = ctx.logical_links.lock().await;
                                            if let Some(l) = links.get_mut(&cll_handle)
                                                && l.channel_id == Some(ctx.channel_id)
                                                && l.connect_generation == connect_generation
                                                && l.comm_started
                                                && tester_present_state_token(
                                                    &l.tester_present_state,
                                                ) == token
                                            {
                                                l.tester_present_state =
                                                    TesterPresentState::Armed {
                                                        resolved,
                                                        interval,
                                                        armed_at: fired_at,
                                                        last_fired: Some(fired_at),
                                                        framed_data,
                                                    };
                                                // Pushed under the SAME lock
                                                // acquisition as the state
                                                // write above, using the same
                                                // re-validation guards (Codex
                                                // review, this round): a
                                                // stale write can never push
                                                // into the wrong CLL's list.
                                                push_open_tp_discard(
                                                    &mut l.open_tp_discards,
                                                    tokio::time::Instant::now(),
                                                    new_open_tp_discard,
                                                );
                                            }
                                        }
                                    }
                                    P3GapOutcome::HardError => {
                                        // handle_channel_hard_error already
                                        // handled loss-of-comms for this channel.
                                    }
                                    P3GapOutcome::Cancelled => {
                                        debug_assert!(false, "cancellable = false");
                                    }
                                    P3GapOutcome::Deferred => {
                                        unreachable!("defer_if_blocked = false")
                                    }
                                }
                            }
                        }
                        Ok(_) => {
                            // Tester-present now disabled, or unchanged
                            // on-wire behavior versus what's already armed:
                            // leave whatever tester_present_state currently
                            // holds untouched (see doc comment above).
                        }
                        Err(_) => {
                            send_error_event(
                                &ctx.subscriptions,
                                &ctx.logical_links,
                                cll_handle,
                                PduErrorEvent::PduErrEvtTesterPresentError,
                                Some((cop_handle, cop_tag.clone())),
                            )
                            .await;
                        }
                    }
                }
            } else {
                // Recheck generation before reporting a failed apply
                // (ADR-086, round 10 fix): U1 already checked before
                // `apply_params_to_hardware` started, but the call itself is
                // a real hardware `.await` a disconnect+reconnect can
                // complete during -- exactly the same class of gap U2
                // already closes for the success path. Without this check,
                // a stale COP whose apply failed would still emit
                // `PduErrEvtProtErr` on the reconnected session and fall
                // through to the unconditional `PduCopstFinished` below,
                // even though `cancel_link_cops` may already have cancelled
                // it. Direct bail (not routed through a flag): unlike
                // `handle_send_recv`'s `Temp` binding, `CoptUpdateparam` has
                // no unconditional cleanup obligation after this point.
                let still_on_this_channel = {
                    let links = ctx.logical_links.lock().await;
                    links.get(&cll_handle).is_some_and(|l| {
                        l.channel_id == Some(ctx.channel_id)
                            && l.connect_generation == connect_generation
                    })
                };
                if !still_on_this_channel {
                    emit_terminal_if_live(
                        &ctx.primitives,
                        &ctx.logical_links,
                        &ctx.subscriptions,
                        &ctx.service.terminal_cops,
                        cll_handle,
                        cop_handle,
                        PduComPrimitiveStatus::PduCopstCancelled,
                    )
                    .await;
                    return;
                }
                // At least one SET_CONFIG call failed; emit an error event so callers
                // relying on SubscribeEvent can observe the failure without waiting
                // for a subsequent RPC's ErrorDetail (ADR-105).
                send_error_event(
                    &ctx.subscriptions,
                    &ctx.logical_links,
                    cll_handle,
                    PduErrorEvent::PduErrEvtProtErr,
                    Some((cop_handle, cop_tag.clone())),
                )
                .await;
            }
        }
    }

    // Terminal `still_on_this_channel` recheck (ADR-086 round 13 fix):
    // rather than chasing each individual `.await` inside the re-arm block
    // one at a time (`frame_tester_present_data`'s own internal event
    // `.await`, `wait_for_p3_gap`'s `.await`, `send_tester_present_once`'s
    // `.await`, and `send_error_event`'s `.await` in the `!all_ok` branch
    // just above), this single recheck immediately before the terminal
    // status closes all of them at once -- mirroring `handle_send_recv`'s S4
    // and `handle_stop_comm`'s terminal-block guard. Three-way, matching U1's
    // own "link missing" precedent (same idiom as the post-promotion check
    // above): link missing (`None`) still emits `PduCopstFinished`
    // unchanged; stale (`Some(false)`) bails first-wins with
    // `PduCopstCancelled`; live (`Some(true)`) emits `PduCopstFinished` as
    // before. Not redundant with the post-promotion check above: that one
    // protects the re-arm block's own side effects from running under a
    // stale snapshot; this one protects the terminal status itself against
    // staleness introduced anywhere in the block that ran after it.
    //
    // ADR-128 correction: the `None` (link missing) and `Some(true)` (live)
    // arms below used to fall through to an UNCONDITIONAL `send_cop_status`
    // Finished emission -- unlike every other emission site in this
    // function (and every other handler in this file), this one never
    // guarded itself with `primitives.remove(&cop_handle).is_some()`
    // first-wins check. Two independent, compounding bugs this closes:
    // (1) on the ordinary success path, this COP's `primitives` entry was
    // never actually removed here, so `GetStatus` could never resolve it to
    // Finished via the primitives-miss fallback -- it just lingered
    // forever; (2) in the `None` (link-missing) race this comment's own
    // "matching U1's own link missing precedent" reasoning intentionally
    // leaves unchanged (a concurrent `DestroyComLogicalLink` completed,
    // including `cancel_link_cops`, mid-flight through this handler's own
    // multi-`.await` sequence), an unconditional emission let this straggler
    // insert a POST-purge `terminal_cops` entry for an already-fully-destroyed
    // CLL, permanently resurrecting a "resolvable" cop_handle that should be
    // `PDU_ERR_INVALID_HANDLE` (ADR-128 review finding). Guarding this final
    // emission the same "first-wins through primitives" way every sibling
    // guard in this file already does fixes both: on the normal path, this
    // removal is the ONLY one that ever fires (nothing else races an
    // executing, not-yet-cancelled COP), so behavior is unchanged; on the
    // `None`/destroy-race path, `cancel_link_cops` has already removed this
    // entry (and already emitted the correct terminal Cancelled, and
    // recorded it in `terminal_cops`) before this straggler can run, so the
    // guard makes it lose the race and skip -- never overwriting the correct
    // Cancelled record with a stale, resurrecting Finished.
    let final_still_on_this_channel: Option<bool> = {
        let links = ctx.logical_links.lock().await;
        links.get(&cll_handle).map(|l| {
            l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
        })
    };
    let final_status = if final_still_on_this_channel == Some(false) {
        PduComPrimitiveStatus::PduCopstCancelled
    } else {
        PduComPrimitiveStatus::PduCopstFinished
    };
    emit_terminal_if_live(
        &ctx.primitives,
        &ctx.logical_links,
        &ctx.subscriptions,
        &ctx.service.terminal_cops,
        cll_handle,
        cop_handle,
        final_status,
    )
    .await;
}

/// Processes `TxItem::RestoreParam` in the poll task (`CoptRestoreParam`):
///
/// Copies the Active param snapshot back into the Working set, and the
/// Active UniqueRespIdTable back into the Working UniqueRespIdTable
/// (ADR-068). No hardware calls are made — the hardware state is unchanged.
///
/// Emits `PduCopstExecuting` then `PduCopstFinished`.
///
/// `connect_generation` (ADR-086 round 11): compared against the live
/// `LogicalLinkState.connect_generation` in the SAME critical section as the
/// `working`/`working_unique_resp_id_table` write below (round-8 Guard-0
/// style, single lock acquisition), to detect a disconnect-then-reconnect of
/// this `cll_handle` since the COP was accepted. Both "link missing" and
/// "stale" are now routed through the same first-wins bail-out, unlike the
/// pre-fix code, which no-op'd silently on link-missing.
async fn handle_restore_param(
    cop_handle: u32,
    cll_handle: u32,
    connect_generation: u64,
    ctx: &ChannelPollCtx,
) {
    // ADR-118 (A2-24) / ADR-205: EXECUTING is gated atomically on the COP
    // still being live in `primitives`, guard held continuously from the
    // liveness check through the emission -- every CANCELLED emitter removes
    // the entry under `primitives` before emitting, so EXECUTING can never be
    // observed after CANCELLED. An absent entry means a concurrent
    // cancel/teardown already claimed this COP and emitted its own terminal
    // status -- bail out; nothing to clean up. `cop_tag` is captured here,
    // once, from this same live `CopEntry` read; `RestoreParam` has no
    // further use for it once this emission is sent, unlike the other
    // handlers sharing this template.
    {
        let queue_target = resolve_queue_target(&ctx.logical_links, cll_handle).await;
        let prims = ctx.primitives.lock().await;
        let Some(entry) = prims.get(&cop_handle) else {
            return;
        };
        let cop_tag = entry.cop_tag.clone();
        send_cop_status(
            &ctx.subscriptions,
            &ctx.service.terminal_cops,
            queue_target.as_ref(),
            cll_handle,
            cop_handle,
            PduComPrimitiveStatus::PduCopstExecuting,
            cop_tag,
        )
        .await;
    }

    // Active → Working, guarded by a single `logical_links` critical section.
    let still_on_this_channel = {
        let mut links = ctx.logical_links.lock().await;
        match links.get_mut(&cll_handle) {
            Some(link) => {
                let still = link.channel_id == Some(ctx.channel_id)
                    && link.connect_generation == connect_generation;
                if still {
                    link.working = link.active.clone();
                    link.working_unique_resp_id_table = link.active_unique_resp_id_table.clone();
                }
                still
            }
            None => false,
        }
    };

    // First-wins bail-out idiom (ADR-086): `!still_on_this_channel` covers
    // both "link missing" and "stale" the same way. ADR-128 correction
    // (Codex review round 2): the live-path Finished emission below used to
    // be a separate, unguarded `send_cop_status` call -- both arms now
    // share ONE `emit_terminal_if_live` call (computed-status shape,
    // mirroring `handle_update_param`/`handle_delay`) so there is exactly
    // one `primitives` removal deciding both the first-wins race and which
    // status is correct.
    let status = if still_on_this_channel {
        PduComPrimitiveStatus::PduCopstFinished
    } else {
        PduComPrimitiveStatus::PduCopstCancelled
    };
    emit_terminal_if_live(
        &ctx.primitives,
        &ctx.logical_links,
        &ctx.subscriptions,
        &ctx.service.terminal_cops,
        cll_handle,
        cop_handle,
        status,
    )
    .await;
}

/// Which J2534 init call `CoptStartcomm` should issue for a K-line protocol
/// (ISO9141/ISO14230), selected by [`select_init_sequence`] from the bound
/// `CP_InitializationSettings` ComParam (D-PDU `ComParamId(0x8090)`; project
/// definition: `1` = 5-baud init, `2` = fast-init, `3` = no init sequence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InitSequence {
    /// `CP_InitializationSettings == 1`.
    FiveBaud,
    /// `CP_InitializationSettings == 2`.
    Fast,
    /// `CP_InitializationSettings == 3`: the COP must skip the init call
    /// entirely and proceed as though it had succeeded.
    None,
}

/// Selects the K-line [`InitSequence`] for `CoptStartcomm` from the bound
/// `CP_InitializationSettings` ComParam (`params`, the COP's `binding.resolved()`
/// snapshot -- this already respects ADR-067 temp_param_update semantics).
///
/// Called only from `rpc_start_com_primitive`, at `StartComPrimitive` call
/// time (ADR-076 moves the last remaining execution-time call, in
/// `handle_start_comm`, to call time alongside it -- the poll task now only
/// ever dispatches on the already-resolved `TxItem::StartComm::five_baud` /
/// `fast_init`, never re-selects a sequence itself).
///
/// - `Some(1)` → [`InitSequence::FiveBaud`]; `Some(2)` → [`InitSequence::Fast`];
///   `Some(3)` → [`InitSequence::None`].
/// - Absent from the set → the legacy heuristic (ISO9141 or
///   UART_ECHO_BYTE_PS unconditionally -- clause 12.3.2/12.3.4.2 defines
///   only 5-baud init for the latter -- or a single-byte `init_data` on any
///   other protocol, selects 5-baud init; everything else selects
///   fast-init). Behavior-preserving for ISO9141/ISO14230: every K-line
///   bustype default seeds this ComParam (see `comparam_defaults.rs`), so
///   this branch only affects a link whose param set predates
///   `CP_InitializationSettings`. UART_ECHO_BYTE_PS is different: this
///   ComParam is permanently excluded from its allowlist (ADR-170 Decision
///   3), so every UART_ECHO_BYTE_PS link takes this branch always, not just
///   ones with a stale param set.
/// - Any other value → defensively warns (with the offending value) and
///   falls back to the same legacy heuristic; should be unreachable once
///   `SetComParam` range validation (`rpc_link.rs`) is in place, since
///   bustype defaults only ever seed `1`/`2`.
///
/// A `Fast` result does NOT by itself mean fast-init actually runs on empty
/// `init_data`: the legacy heuristic (param absent) also returns `Fast` for
/// empty `init_data` on ISO14230, but empty `cop_data` on that path must
/// keep meaning "skip init entirely" for backward compatibility. The
/// wakeup-only fast-init decision (ADR-077) is therefore made at the call
/// site directly from the bound param's EXPLICIT value (`== Some(2)`), not
/// by calling this function with empty `init_data`.
pub(super) fn select_init_sequence(
    params: &ComParamSet,
    protocol_id: u32,
    init_data: &[u8],
    cll_handle: u32,
) -> InitSequence {
    let legacy_heuristic = || {
        // ADR-170 Decision 8 (design-advisor fix): clause 12.3.2/12.3.4.2
        // defines only 5-baud init for UART_ECHO_BYTE_PS -- no fast-init
        // exists for this protocol, so it always selects `FiveBaud` here,
        // unconditionally, the same way ISO9141 does. This is
        // defense-in-depth: `rpc_primitive.rs`'s synchronous
        // `cop_data.len() != 1` guard already rejects the empty/oversized
        // `init_data` cases before this function is ever called for this
        // protocol, but keeping this pure function's own result correct on
        // its own terms avoids a latent trap for any other future caller.
        if protocol_id == j2534_0404::ISO9141
            || protocol_id == j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
            || init_data.len() == 1
        {
            InitSequence::FiveBaud
        } else {
            InitSequence::Fast
        }
    };
    match params.unum32.get(&PARAM_INIT_SETTINGS).copied() {
        Some(1) => InitSequence::FiveBaud,
        Some(2) => InitSequence::Fast,
        Some(3) => InitSequence::None,
        None => legacy_heuristic(),
        Some(other) => {
            warn!(
                cll_handle,
                value = other,
                "CoptStartcomm: CP_InitializationSettings has an out-of-range value \
                 (expected 1, 2, or 3); falling back to the legacy init-sequence heuristic"
            );
            legacy_heuristic()
        }
    }
}

/// Performs the J2534 protocol initialisation for K-line protocols, from
/// inputs already resolved at `StartComPrimitive` call time (ADR-076,
/// superseding this function's own former `InitSequence` dispatch; extended
/// by ADR-077 for wakeup-only fast-init) -- `handle_start_comm`'s caller
/// guarantees at least one of `five_baud` / `fast_init` is `Some` before
/// calling this (`five_baud.is_some() || fast_init.is_some()`), and the two
/// are call-time-exclusive, so `five_baud`'s presence alone selects the
/// branch.
///
/// - `Some(five_baud)`: calls `five_baud_init` with `five_baud.address`
///   (either the spec-resolved ComParam address or the legacy `cop_data[0]`
///   byte -- see [`FiveBaudInit`]).
/// - `None`, `fast_init` is `Some(FastInit::WakeupOnly)`: calls `fast_init`
///   with a NULL input message -- no `PassThruMessage` is constructed, and
///   `tx_flags` is irrelevant to this branch (ADR-077: the D-PDU API spec's
///   fast-init service request is OPTIONAL).
/// - `None`, `fast_init` is `Some(FastInit::WithRequest(frame))`: `frame` is
///   the wakeup pattern frame, sent via `fast_init` as-is -- no
///   protocol-specific special-casing (an ISO9141 link forced into fast-init
///   this way surfaces the adapter's own error through the normal
///   init-failure path). The frame arrives pre-built, its KWP header
///   constructed from ComParams/UniqueRespIdTable at `StartComPrimitive`
///   call time, not here (ADR-075).
///
/// `InitSequence::None` (explicit skip) is never passed in at all -- the
/// caller skips this call entirely instead (see `handle_start_comm`).
///
/// Returns the raw response bytes to push into the receive buffer on success,
/// or the underlying J2534 error when the init sequence fails. For
/// `FastInit::WakeupOnly` the returned bytes are always empty (ADR-077): no
/// request was sent, so the adapter's output buffer is not a response and is
/// ignored here without parsing -- not even its `DataSize` is trusted, since
/// an adapter may leave the caller-zeroed message untouched or write garbage
/// alongside `STATUS_NOERROR`. The caller's `deliver_keybytes` gate
/// independently never delivers for this variant.
async fn run_protocol_init(
    five_baud: Option<FiveBaudInit>,
    fast_init: Option<FastInit>,
    channel_id: ChannelId,
    protocol_id: u32,
    tx_flags: u32,
    api: &Arc<Mutex<J2534Api0404>>,
) -> Result<Vec<u8>, j2534_0404::Error> {
    match five_baud {
        Some(five_baud) => {
            let api = api.lock().await;
            let keywords = api.five_baud_init(channel_id, five_baud.address)?;
            Ok(keywords.to_vec())
        }
        None => match fast_init {
            Some(FastInit::WakeupOnly) => {
                // ADR-077: no service request follows the wakeup pattern --
                // pass a NULL input message and ignore the output buffer
                // entirely (see the doc comment above).
                let api = api.lock().await;
                api.fast_init(channel_id, None)?;
                Ok(Vec::new())
            }
            Some(FastInit::WithRequest(frame)) => {
                let wakeup_msg =
                    j2534_0404::PassThruMessage::new(protocol_id, 0, tx_flags, 0, 0, &frame)?;
                let api = api.lock().await;
                let resp = api.fast_init(channel_id, Some(&wakeup_msg))?;
                Ok(resp.data().map(|d| d.to_vec()).unwrap_or_default())
            }
            None => {
                // Unreachable: `handle_start_comm` only calls this function
                // when `five_baud.is_some() || fast_init.is_some()`.
                Ok(Vec::new())
            }
        },
    }
}

/// Outcome of one send cycle's receive phase (ADR-053).
enum ReceivePhaseOutcome {
    /// The receive phase ended normally — the required matches arrived, the
    /// IS-MULTIPLE window closed, or the `CP_P2Max` window timed out (with
    /// `PduErrEvtRxTimeout` emitted when the phase came up short).  The
    /// caller decides whether another send cycle follows or the COP is
    /// finished; no `PduCopstFinished` has been emitted here.
    CycleComplete,
    /// The COP was terminally handled inside the receive phase (cancelled,
    /// or a hard channel error); every required status event has been
    /// emitted — the caller must not emit anything further.
    Terminal,
    /// An RC21/RC23 re-request's own transmit failed
    /// (`TxFailure::Event`, non-stale). The error event has already been
    /// emitted; the receive phase is over, but — unlike `Terminal` — the
    /// caller owns the terminal COP status: `handle_send_recv` emits
    /// `PduCopstFinished` (byte-for-byte its pre-ADR-087 behavior),
    /// `handle_stop_comm` falls through to its own terminal
    /// `PduCllstOnline`/`PduCopstFinished` block (ADR-087) instead.
    ReRequestTxFailed,
    /// ADR-100 Decision §2 tier migration (S5): an IS-CYCLIC
    /// (`NumReceiveCycles == -1`) receive phase just accepted its FIRST
    /// positive response and, per spec (the ComPrimitive then continues in
    /// receive only mode), flipped its own `CopRegistrant` from
    /// `RegistrantTier::ActiveSendReceive` to `RegistrantTier::ReceiveOnly`
    /// and returned -- freeing this poll task for other ComPrimitives on the
    /// channel. The COP is NOT finished: it stays in `primitives` and in the
    /// registry, and keeps matching further responses via `bind_frame`'s
    /// tier-2 scan from here on. The caller must not emit `PduCopstFinished`,
    /// must not schedule a `CycleContinuation` (a detached IS-CYCLIC COP is
    /// permanently done sending -- it is not "between send cycles"), and must
    /// not otherwise clean it up: its lifecycle now continues independently,
    /// ending only via `CancelComPrimitive` (reaped by
    /// `reap_cancelled_detached_registrants`, since nothing is actively
    /// waiting on this COP to notice cancellation anymore), a hard channel
    /// error, or an ADR-086 disconnect/reconnect generation change (both
    /// already sweep `primitives`/`registrants` wholesale via
    /// `cancel_link_cops`).
    DetachedToTier2,
}

/// Call-specific data for one send cycle's receive-phase wait
/// (`wait_for_expected_response`), as opposed to `ChannelPollCtx`'s handles
/// shared by the whole poll task.
struct ExpectedResponseWait<'a> {
    cll_handle: u32,
    cop_handle: u32,
    /// `cop_handle`'s already-captured `CopEntry::cop_tag`, sourced by every
    /// call site from the same `cop_tag` local its own enclosing handler
    /// (`handle_send_recv`/`handle_start_comm`/`handle_stop_comm`) already
    /// captured at its own top (ADR-205 Decision item 1) -- never re-resolved
    /// here. Threaded straight into every `send_error_event` call inside
    /// `wait_for_expected_response_inner`.
    cop_tag: Option<Vec<u8>>,
    expected: &'a [ExpectedResponse],
    num_receive_cycles: i32,
    timeout_ms: u32,
    protocol_id: u32,
    tx_flags: u32,
    original_data: &'a [u8],
    isotp_tx: Option<&'a SoftIsoTpTx>,
    rc_cfg: &'a RcHandlingConfig,
    /// `CP_ModifyTiming` live-exchange config (ADR-146), built the same way
    /// and at the same call sites as `rc_cfg` above. `None` whenever
    /// `TimingChangeConfig::from_params` itself returned `None`.
    timing_cfg: Option<&'a TimingChangeConfig>,
    /// `LogicalLinkState.connect_generation` captured at `StartComPrimitive`
    /// call time (ADR-086), threaded through unchanged from
    /// `SendRecvCycle::connect_generation`. Compared against the live value
    /// by the per-pass `still_on_this_channel` check below to detect a
    /// disconnect-then-reconnect completing mid-receive-phase -- the site
    /// that actually ends a stale IS-CYCLIC receive, which otherwise has no
    /// deadline and is today ended only by `cancelled_cops` (bypassed by
    /// `cancel_link_cops`).
    connect_generation: u64,
    /// `false` (ADR-087) skips consulting/removing from `cancelled_cops` on
    /// every pass -- any entry a `CancelComPrimitive` call inserted is left
    /// alone for `dispatch_tx_item`'s existing post-completion cleanup to
    /// reap later, mirroring round 6's `cancellable: false` on
    /// `transmit_request`/`isotp_send`. The staleness half of the same
    /// per-pass check still runs unconditionally either way. Also threaded
    /// into the RC21/RC23 re-request's own `transmit_request` call.
    /// `handle_send_recv`'s call site passes `true`, preserving
    /// `CoptSendrecv`'s cancellation contract bit-for-bit;
    /// `handle_stop_comm`'s call site passes `false`, extending round 6's
    /// non-cancellable-transmit rule to the whole post-teardown receive
    /// phase.
    cancellable: bool,
    /// `None` -- no ceiling; `CoptSendrecv`'s current unbounded-with-
    /// cancellation IS-MULTIPLE semantics, unchanged. `Some(ms)` -- an
    /// absolute deadline, anchored once at receive-phase entry, that clamps
    /// every subsequent per-match deadline extension. Used only by
    /// `CoptStopcomm`'s non-cancellable receive phase (ADR-087), where
    /// `CancelComPrimitive` is deliberately ignored and so provides no other
    /// escape from a chatty ECU or broad/empty `expected_response` pattern
    /// under `NumReceiveCycles == -2`.
    match_reset_ceiling_ms: Option<u32>,
    /// `true` only for a COP created receive-only (`NumSendCycles == 0`,
    /// ADR-059) -- `handle_send_recv`'s call site passes `send_cycles_remaining
    /// == 0`; `handle_stop_comm`'s call site (always driven by an actual final
    /// transmit) passes `false` unconditionally. Combined with
    /// `num_receive_cycles == -1` inside `wait_for_expected_response`, this
    /// selects ADR-100 Decision §4 (S6)'s creation-time `RegistrantTier::
    /// ReceiveOnly` construction -- a receive-only COP by definition never has
    /// an active-send-and-receive phase, so it is never inserted as tier-1 to
    /// begin with (unlike a migrated IS-CYCLIC COP, S5).
    created_receive_only: bool,
    /// `ComParamSet::cyclic_resp_timeout_ms()` resolved at the same call site
    /// and from the same `ParamBinding` snapshot as `timeout_ms`/`rc_cfg`
    /// (ADR-100 Decision §4, S6; scope widened by ADR-182). Only consulted
    /// when `created_receive_only && (num_receive_cycles == -1 ||
    /// num_receive_cycles > 0)` (`is_comparam_timed_receive_only`); `0` means
    /// "disabled, no cyclic deadline." Ignored (any value, including a
    /// nonzero one) for every other case -- in particular a migrated (S5)
    /// IS-CYCLIC COP, and a created-receive-only `-2` (IS-MULTIPLE)
    /// registrant, never get a cyclic deadline in this step, per the ADR's
    /// resolved scope.
    cyclic_resp_timeout_ms: u32,
    /// `ComParamSet::enable_concatenation()` resolved at the same call site
    /// and from the same bound snapshot as `timeout_ms`/`rc_cfg` (ADR
    /// reference: search `CP_EnableConcatenation`) -- the raw
    /// `CP_EnableConcatenation` ComParam value, NOT yet combined with the
    /// protocol check or the registrant-shape scope restriction;
    /// `wait_for_expected_response` combines all three into the
    /// `CopRegistrant::concat_enabled` it actually stores.
    enable_concatenation: bool,
}

/// Wraps [`wait_for_expected_response_inner`] (see its doc comment for the
/// receive-phase contract, unchanged by this wrapper) with ADR-100 registry
/// bookkeeping: inserts this COP's [`CopRegistrant`] into
/// `logical_links[cll_handle].registrants` before the receive phase begins
/// and removes it on every exit path EXCEPT `ReceivePhaseOutcome::
/// DetachedToTier2` (S5) -- that outcome's entire point is for the
/// registrant to keep living in the registry, now as a tier-2 entry, after
/// this wrapper returns. The insert/remove is done here, around the whole
/// call, rather than annotated onto each of the inner function's many
/// internal early returns (`ReceivePhaseOutcome`'s `CycleComplete` /
/// `Terminal` / `ReRequestTxFailed` arms) -- this guarantees no return path
/// can leak a registrant without having to keep every one of those sites in
/// sync with this bookkeeping.
///
/// `NumReceiveCycles == 0` mirrors the inner function's own immediate
/// `CycleComplete` short-circuit (ADR-058: no receive phase runs at all) --
/// no registrant is inserted for that case, since no receive phase begins.
///
/// Every registrant defaults to `RegistrantTier::ActiveSendReceive` and a
/// `Some(rc_cfg)` (this call site is always tier-1 at insertion) --
/// `wait_for_expected_response_inner`'s own IS-CYCLIC first-match arm is what
/// migrates a registrant to `RegistrantTier::ReceiveOnly` in place, post
/// insertion (ADR-100 Decision §2, S5); this wrapper never constructs a
/// tier-2 registrant directly. `request_sid` is the first byte of
/// `wait.original_data` -- `None` for an empty request (e.g. a receive-only
/// cycle with nothing transmitted this cycle).
///
/// As of S3, this registrant is a live attribution candidate the moment it
/// is inserted: `poll_rx_inner`'s `bind_frame`/`bind_registrant` scan every
/// registrant on `cll_handle`, including this one, on every poll pass for as
/// long as it stays in `logical_links[cll_handle].registrants` --
/// `poll_rx_and_check_match` (called from `wait_for_expected_response_inner`'s
/// own loop) reads this SAME registrant's `matches_got`/`pending_rc` back
/// after each pass instead of owning a private `MatchProbe` (deleted).
async fn wait_for_expected_response(
    wait: ExpectedResponseWait<'_>,
    ctx: &ChannelPollCtx,
) -> ReceivePhaseOutcome {
    let cll_handle = wait.cll_handle;
    let cop_handle = wait.cop_handle;
    // Mirrors wait_for_expected_response_inner's own `matches_needed`
    // computation and `Some(0)` short-circuit -- see that function's doc
    // comment for the full `NumReceiveCycles` semantics.
    let matches_needed: Option<u32> = match wait.num_receive_cycles {
        -2 | -1 => None,
        n => Some(n as u32),
    };
    let has_receive_phase = matches_needed != Some(0);

    // ADR-100 Decision §4 (S6), widened by ADR-182: a COP created
    // receive-only (`NumSendCycles == 0`, ADR-059) with `NumReceiveCycles ==
    // -1` OR a finite `N > 0` is inserted directly as tier-2 AND detaches
    // immediately (returns below without ever entering
    // `wait_for_expected_response_inner`) -- it never has an
    // active-send-and-receive phase to migrate out of, unlike an IS-CYCLIC
    // COP created WITH a send phase (S5, unaffected by this flag:
    // `created_receive_only` is `false` at that call site). `-2`
    // (IS-MULTIPLE) is deliberately excluded (ISO 22900-2:2022 Table 6/9
    // name no `CP_CyclicRespTimeout` role for it, and Table 6's own closing
    // row's `(0, -2)` combination is a separate, tracked-not-fixed finding --
    // see the backlog) -- it stays tier-2-from-creation
    // but still runs inline/blocking, `CP_P2Max`-governed, exactly as before
    // ADR-182. This flag governs BOTH `cyclic_timeout_ms`/`cyclic_deadline`
    // population below AND the immediate `DetachedToTier2` return -- ADR-182
    // widened it from the `-1`-only shape (`is_receive_only_cyclic`) that
    // shipped with ADR-100 Decision §4/round-9's own Finding-1 correction.
    let is_comparam_timed_receive_only =
        wait.created_receive_only && (wait.num_receive_cycles == -1 || wait.num_receive_cycles > 0);

    // ADR-148. Combines the raw
    // `CP_EnableConcatenation` ComParam value with the protocol restriction
    // (ISO 22900-2:2022 Table B.11: KWP family -- ISO 9141-2, ISO
    // 14230-2/-4 -- and SAE J1850 VPW/PWM only) and the v1 registrant-shape
    // scope: never a created-receive-only COP (ADR-059, any
    // `NumReceiveCycles`) and never the true IS-CYCLIC shape
    // (`NumReceiveCycles == -1` with a send phase present) -- IS-MULTIPLE
    // (`NumReceiveCycles == -2`) IS in scope. Resolved once, here, at
    // registrant-creation time, and stored on the `CopRegistrant` itself so
    // every later consult (`bind_registrant`, the deadline-expiry finalize
    // in `wait_for_expected_response_inner`) reads a single already-scoped
    // flag instead of re-deriving it.
    let concat_enabled = wait.enable_concatenation
        && !wait.created_receive_only
        && wait.num_receive_cycles != -1
        && {
            // ADR-157 Plane B: normalized -- found by Codex review, PR #28
            // (previously an accepted residual). `wait.protocol_id` can be a
            // raw `_PS` id for a pin-selected KWP-family/J1850-family link,
            // which `ChannelProtocol::from_raw` alone never recognizes as
            // belonging to its base family; mirrors the same
            // `resources::base_protocol_id` normalization already applied to
            // `rc_cfg`'s `RcHandlingConfig::from_params` call above.
            let protocol = ChannelProtocol::from_raw(resources::base_protocol_id(wait.protocol_id));
            protocol.is_kwp_family() || protocol.is_j1850_family()
        };

    if has_receive_phase {
        // `0` (ComParam absent/disabled) means no cyclic deadline at all --
        // this COP then behaves exactly as before this step, ending only via
        // cancel/hard-error/staleness (NOTE 1's "Otherwise, the application
        // cancels...").
        let cyclic_timeout_ms =
            is_comparam_timed_receive_only.then_some(wait.cyclic_resp_timeout_ms);
        let cyclic_deadline = cyclic_timeout_ms
            .filter(|&ms| ms > 0)
            .map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms as u64));
        insert_cop_registrant(
            &ctx.logical_links,
            cll_handle,
            CopRegistrant {
                cop_handle,
                // Overwritten under the lock by `insert_cop_registrant`
                // itself (ADR-100 Decision §3, resolved (a)) -- this value is
                // never observed.
                registration_seq: 0,
                // ADR-100 round-9 Finding-1 correction: tier is keyed on
                // `wait.created_receive_only` alone -- ANY created-receive-
                // only COP (finite `N`, `-1`, or `-2`) is tier-2 from
                // creation, not just the `-1`/finite-`N` subtypes
                // (`is_comparam_timed_receive_only`), which only continue to
                // gate the narrower `cyclic_timeout_ms`/`cyclic_deadline`
                // fields below (ADR-100 Decision §4's scope, widened from
                // `-1`-only to also cover finite `N > 0` by ADR-182; `-2` is
                // never in this scope).
                tier: if wait.created_receive_only {
                    RegistrantTier::ReceiveOnly
                } else {
                    RegistrantTier::ActiveSendReceive
                },
                expected: wait.expected.to_vec(),
                // Tier-2 never runs RC detection (ISO 22900-2's 7F clause:
                // receive only ComPrimitives get no negative response
                // handling) -- for the WHOLE tier, not just the
                // `-1` subtype (ADR-100 round-9 Finding-1 correction).
                rc_cfg: if wait.created_receive_only {
                    None
                } else {
                    Some(wait.rc_cfg.clone())
                },
                // `wait.original_data` is header-prefixed (ADR-050), so its
                // first byte is not the SID -- reuse `rc_cfg.request_sid`
                // instead, which S2 (ADR-100 Decision §3, resolved (b))
                // already computes correctly from the payload-only
                // `cop_data` at the RPC layer. `None` for any
                // created-receive-only COP, mirroring `rc_cfg` immediately
                // above (ADR-100 round-9 Finding-1 correction).
                request_sid: if wait.created_receive_only {
                    None
                } else {
                    wait.rc_cfg.request_sid
                },
                matches_needed,
                matches_got: 0,
                pending_rc: None,
                connect_generation: wait.connect_generation,
                cyclic_deadline,
                cyclic_timeout_ms,
                // ADR-100 round-9 Finding-2 correction: only the true
                // IS-CYCLIC shape (send phase present, `NumReceiveCycles ==
                // -1`) migrates tier on its first match -- the
                // created-receive-only shapes above (including the `-1`/
                // finite-`N` subtypes, which ADR-182 detaches directly at
                // creation, below) are already at their final tier from
                // creation and must never migrate again.
                migrate_on_first_match: !wait.created_receive_only && wait.num_receive_cycles == -1,
                // ADR-146: tier-2 (receive-only) never runs this mechanism
                // either, for the same reason it never runs RC detection
                // (`rc_cfg` above) -- ISO 22900-2's 7F clause disables
                // negative-response handling for receive-only ComPrimitives,
                // and by the same reasoning this mechanism too.
                timing_cfg: if wait.created_receive_only {
                    None
                } else {
                    wait.timing_cfg.cloned()
                },
                timing_accumulator: None,
                pending_timing_change: None,
                concat_enabled,
                concat: Vec::new(),
                concat_segments_got: 0,
            },
        )
        .await;

        if is_comparam_timed_receive_only {
            // ADR-100 gap fix (see implementation-notes.md), widened by
            // ADR-182 from the `-1`-only shape to also cover finite `N > 0`:
            // this registrant was just inserted directly as tier-2, above --
            // unlike a migrated IS-CYCLIC COP (S5), a created-receive-only
            // `-1`/finite-`N` registrant never has an active-send-and-receive
            // phase to migrate out of, so there is no "wait for the first
            // match" step to run at all. Return immediately, mirroring S5's
            // own post-first-match `DetachedToTier2` return shape exactly --
            // freeing this poll task right away instead of blocking on this
            // registrant's own first match (or, for finite `N`, its full
            // count). See `ReceivePhaseOutcome::DetachedToTier2`'s doc
            // comment for what happens to the registrant next (bind_frame's
            // tier-2 scan takes over permanently; count-completion and
            // `CP_CyclicRespTimeout` expiry are both now finalized only by
            // `reap_expired_cyclic_registrants`'s per-tick sweep); the
            // wrapper's own post-call `if has_receive_phase &&
            // !matches!(outcome, DetachedToTier2)` removal guard below is
            // never reached on this path, which is correct -- the registrant
            // must stay live.
            return ReceivePhaseOutcome::DetachedToTier2;
        }
    }

    let outcome = wait_for_expected_response_inner(wait, ctx).await;

    // ADR-100 S5: `DetachedToTier2` is the one outcome that must NOT remove
    // the registrant here -- that is the entire point of the migration
    // (`migrate_registrant_to_receive_only` already flipped it to tier 2 in
    // place, inside the inner function): it stays in
    // `logical_links[cll_handle].registrants`, now a live tier-2 attribution
    // candidate for as long as it remains there, ending only via
    // `CancelComPrimitive`/hard-error/generation-staleness (see
    // `ReceivePhaseOutcome::DetachedToTier2`'s own doc comment). Every other
    // outcome keeps this wrapper's original insert/remove-on-every-exit-path
    // contract unchanged.
    if has_receive_phase && !matches!(outcome, ReceivePhaseOutcome::DetachedToTier2) {
        remove_cop_registrant(&ctx.logical_links, cll_handle, cop_handle).await;
    }

    outcome
}

/// Pushes `registrant` onto `logical_links[cll_handle].registrants`, under
/// the lock, first assigning its `registration_seq` from
/// `LogicalLinkState::next_registrant_seq` (ADR-100 Decision §3, resolved
/// (a); the field's incoming value is ignored). A no-op if `cll_handle` is
/// absent from `logical_links` -- mirrors every other `logical_links.get_mut`
/// guard in this file (the CLL was torn down concurrently; nothing to
/// register into).
async fn insert_cop_registrant(
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    cll_handle: u32,
    mut registrant: CopRegistrant,
) {
    let mut links = logical_links.lock().await;
    if let Some(link) = links.get_mut(&cll_handle) {
        registrant.registration_seq = link.next_registrant_seq;
        link.next_registrant_seq += 1;
        link.registrants.push(registrant);
    }
}

/// Removes every registrant matching `cop_handle` from
/// `logical_links[cll_handle].registrants`. A no-op if `cll_handle` is absent
/// (already torn down) or `cop_handle` has no registrant (already removed,
/// e.g. by `cancel_link_cops`'s wholesale clear).
async fn remove_cop_registrant(
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    cll_handle: u32,
    cop_handle: u32,
) {
    let mut links = logical_links.lock().await;
    if let Some(link) = links.get_mut(&cll_handle) {
        link.registrants.retain(|r| r.cop_handle != cop_handle);
    }
}

/// ADR-100 Decision §2 tier migration (S5): flips `cop_handle`'s own
/// registrant on `cll_handle` from `RegistrantTier::ActiveSendReceive` to
/// `RegistrantTier::ReceiveOnly` in place -- called only from
/// `wait_for_expected_response_inner`'s IS-CYCLIC first-match arm, under the
/// same `logical_links` lock acquisition used for every other per-pass
/// registrant mutation in this file. Also clears `rc_cfg` to `None`, honoring
/// `CopRegistrant::rc_cfg`'s own documented invariant ("`Some` only for a
/// tier-1 registrant") -- `bind_registrant`'s tier gate already ensures a
/// tier-2 registrant's `rc_cfg` is never consulted (RC detection only runs
/// for `AttributionScan::Tier1NonVacuous`), so this is defense-in-depth /
/// documentation-accuracy, not a behavior-affecting change. A no-op if
/// `cll_handle` or the registrant is missing (already torn down concurrently
/// -- e.g. `cancel_link_cops` beat this call to the lock; mirrors every other
/// `logical_links.get_mut` guard in this file).
async fn migrate_registrant_to_receive_only(
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    cll_handle: u32,
    cop_handle: u32,
) {
    let mut links = logical_links.lock().await;
    if let Some(link) = links.get_mut(&cll_handle)
        && let Some(r) = link
            .registrants
            .iter_mut()
            .find(|r| r.cop_handle == cop_handle)
    {
        // ADR-100 round-9 Finding-2 correction: `bind_registrant`'s own
        // in-batch flip (gated by `migrate_on_first_match`), plus its
        // `merge_registrant_writeback` merge, have already migrated this
        // live registrant's `tier` to `ReceiveOnly` by the time this call is
        // reached -- the poll pass that accepted the match has already run
        // its full snapshot/bind/writeback cycle before `poll_rx_and_check_
        // match` returns `Matched` to this function's caller. This call is
        // therefore now a redundant confirmation in every traced path, kept
        // as the explicit, self-describing anchor of the
        // `ReceivePhaseOutcome::DetachedToTier2` contract with this
        // function's own caller.
        debug_assert_eq!(
            r.tier,
            RegistrantTier::ReceiveOnly,
            "the in-batch flip (bind_registrant) plus its writeback merge must already have \
             migrated this registrant to ReceiveOnly before this confirmation call runs"
        );
        r.tier = RegistrantTier::ReceiveOnly;
        r.rc_cfg = None;
        // ADR-146, mirroring `rc_cfg` above: tier-2 (Receive Only) never
        // runs the KWP Access Timing live-exchange mechanism either (same
        // ISO 22900-2 7F-clause reasoning `rc_cfg`'s own doc comment cites),
        // but that gate is enforced at COP-*creation* time only
        // (`wait_for_expected_response`'s `created_receive_only` check) --
        // without clearing it here too, a migrated IS-CYCLIC registrant
        // would keep deriving and pushing ComParam changes from every
        // subsequent qualifying `0xC3` response for the rest of its life
        // (edge-case-hunter finding, verified by a reproducing unit test).
        //
        // UNLIKE the `tier` assignment two lines above, this line is NOT a
        // redundant confirmation (edge-case-hunter finding, round 6):
        // `bind_registrant`'s own inline clear (added round 6, closing the
        // in-batch window this same mechanism had) only ever touches the
        // PASS-LOCAL snapshot `CopRegistrant` clone -- `timing_cfg` is not
        // among the fields `merge_registrant_writeback` copies back to the
        // live registrant (unlike `tier`, which IS merged back, making ITS
        // reassignment here the genuine no-op). This line remains the ONLY
        // place that ever clears the LIVE registrant's `timing_cfg`; do not
        // remove it on the assumption that `bind_registrant`'s inline clear
        // already covers this call's own effect -- it does not.
        r.timing_cfg = None;
    }
}

/// Runs one send cycle's receive phase: polls RX until the number of
/// matching frames required by `num_receive_cycles` arrives, or the
/// `timeout_ms` (`CP_P2Max`) response window closes (ADR-053):
///
/// - `0` — one matching response (legacy single-shot).
/// - `n > 0` — exactly `n` matching responses; the window restarts after
///   each accepted response.
/// - `-1` (IS-CYCLIC) — receive indefinitely; no window, the phase ends
///   only via `CancelComPrimitive` or a hard channel error.
/// - `-2` (IS-MULTIPLE) — collect every matching response from one or more
///   ECUs until the window (restarted after each response) closes; the
///   timeout is the *normal* end of the phase, so `PduErrEvtRxTimeout` is
///   emitted only when no response at all arrived.
///
/// When `rc_cfg` has one or more RC handling modes enabled, response-pending NRCs
/// intercepted from the ECU extend or reset the wait deadline instead of ending
/// the phase immediately.  All received frames (including pending-response frames)
/// are still delivered to `rx_buf` and active subscriptions.
///
/// `protocol_id`, `tx_flags`, `original_data`, and `isotp_tx` are required
/// only for RC21/RC23 re-request handling; they are the same values that were
/// used for the initial transmission, so software-ISO-TP re-requests are
/// segmented identically (ADR-046).
///
/// Called only from [`wait_for_expected_response`], which wraps this with
/// ADR-100 registry insert/remove -- callers outside this file should never
/// need to call this directly.
async fn wait_for_expected_response_inner(
    wait: ExpectedResponseWait<'_>,
    ctx: &ChannelPollCtx,
) -> ReceivePhaseOutcome {
    let ExpectedResponseWait {
        cll_handle,
        cop_handle,
        cop_tag,
        // `expected` is not read here as of ADR-100 S3: attribution now
        // happens entirely inside `bind_frame`/`bind_registrant` against the
        // registrant `wait_for_expected_response`'s wrapper already inserted
        // (cloning `wait.expected` at that point) -- this inner loop only
        // ever needed `expected` to build the old, now-deleted `MatchProbe`.
        expected: _,
        num_receive_cycles,
        timeout_ms,
        protocol_id,
        tx_flags,
        original_data,
        isotp_tx,
        rc_cfg,
        // Not read here, for the same reason as `created_receive_only`/
        // `cyclic_resp_timeout_ms` below (ADR-146): `wait_for_expected_
        // response`'s wrapper already consulted this, before this inner
        // function ever runs, to build the registrant's own `timing_cfg` at
        // insertion -- this loop never needs it directly.
        timing_cfg: _,
        connect_generation,
        cancellable,
        match_reset_ceiling_ms,
        // Not read here (ADR-100 Decision §4, S6): `wait_for_expected_response`'s
        // wrapper already consulted these, before this inner function ever
        // runs, to decide the registrant's tier and `cyclic_deadline` at
        // insertion -- this loop only ever needs the LIVE registrant's own
        // `cyclic_deadline` (read fresh each pass below), not these
        // call-time inputs.
        created_receive_only: _,
        cyclic_resp_timeout_ms: _,
        // Same rationale as `created_receive_only`/`cyclic_resp_timeout_ms`
        // just above: `wait_for_expected_response`'s wrapper already
        // combined this with the protocol/registrant-shape scope into the
        // registrant's own `concat_enabled` at insertion -- this loop only
        // ever needs the LIVE registrant's own `concat` buffer state (read
        // fresh each pass, e.g. the deadline-expiry finalize check below),
        // never this call-time input.
        enable_concatenation: _,
    } = wait;

    let matches_needed: Option<u32> = match num_receive_cycles {
        -2 | -1 => None,     // IS-MULTIPLE / IS-CYCLIC: unbounded match count
        n => Some(n as u32), // 0 => no response required at all
    };
    // NumReceiveCycles == 0 means the preceding send requires no response --
    // the cycle completes right after the write, with no receive phase at
    // all (not even a single-match wait). This is the condition CP_P3Func /
    // CP_P3Phys key off of ("receiveCycle=0").
    if matches_needed == Some(0) {
        return ReceivePhaseOutcome::CycleComplete;
    }
    let no_deadline = num_receive_cycles == -1; // IS-CYCLIC: no response window
    // `match_reset_ceiling_ms` is only ever `Some` for `num_receive_cycles ==
    // -2` (IS-MULTIPLE, handle_stop_comm's call site) -- IS-CYCLIC (-1) is
    // rejected synchronously before a `TxItem::StopComm`/`StopCommTx` can
    // even exist (rpc_primitive.rs), so this combination should be
    // unreachable. Asserted here because `no_deadline`'s loop-bottom branch
    // returns before the deadline/ceiling check runs at all, which would
    // silently defeat the ceiling if this invariant were ever violated.
    debug_assert!(
        !(no_deadline && match_reset_ceiling_ms.is_some()),
        "match_reset_ceiling_ms must not be set for IS-CYCLIC (no_deadline)"
    );
    let mut matches_got: u32 = 0;
    // ADR-148. This loop's own
    // cumulative baseline for `CopRegistrant::concat_segments_got`, mirroring
    // `matches_got`'s own baseline-threading rationale (ADR-101 Decision §B)
    // immediately above -- always stays `0` for a non-concat registrant, so
    // `PollMatchResult::Absorbed` is simply never reported for one.
    let mut concat_segments_got: u32 = 0;

    let mut deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
    let poll_interval = Duration::from_millis(POLL_INTERVAL_MS);

    // Anchored once, here, at receive-phase entry -- never recomputed or
    // reset anywhere else in this function (same anchor-not-reset pattern as
    // the RC21/23/78 completion ceilings below, ADR-057).
    let match_reset_ceiling: Option<tokio::time::Instant> = match_reset_ceiling_ms
        .map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms as u64));

    // CP_RC{21,23,78}CompletionTimeout is the maximum total duration of the
    // repeated wait/re-request cycle *for that response code*, anchored at
    // the code's first occurrence in this COP -- not a per-occurrence window
    // that a later repeat of the same code pushes further out (ADR-057).
    // Each code tracks its own anchor independently, since an ECU can
    // legitimately move between codes (e.g. 0x21 then 0x78) before a final
    // response, each bounded by its own configured timeout.
    let mut rc78_ceiling: Option<tokio::time::Instant> = None;
    let mut rc21_ceiling: Option<tokio::time::Instant> = None;
    let mut rc23_ceiling: Option<tokio::time::Instant> = None;

    loop {
        // Poll RX; abort on hard channel error. ADR-100 S3: `expected`/
        // `rc_cfg`/`connect_generation`/the remaining-match quota no longer
        // need to be threaded through here -- `bind_frame` reads them
        // straight off this COP's own `CopRegistrant`, already inserted by
        // `wait_for_expected_response`'s wrapper before this loop began.
        // `matches_got` (this loop's own cumulative counter, below) is
        // threaded in as the baseline `poll_rx_and_check_match` diffs
        // against (ADR-101 Decision §B) -- not re-read fresh from the live
        // registrant each call, so a companion-channel increment landing
        // between two of this loop's own calls is never absorbed into the
        // next call's baseline instead of being reported here.
        let ok = poll_rx_and_check_match(
            cll_handle,
            cop_handle,
            matches_got,
            concat_segments_got,
            ctx,
        )
        .await;

        let mut poll_immediately = false;
        // ADR-101 Decision §E's tier-1 extension: set when this iteration
        // takes the companion-watermark grace pass below (deadline reached,
        // but the companion hasn't caught up yet) -- forces the loop-bottom
        // sleep to use normal `poll_interval` pacing instead of the
        // deadline-saturating (and, once `now >= deadline`, permanently
        // zero) sleep a few lines down. Without this, every subsequent pass
        // while awaiting the companion computes a zero-length sleep and the
        // loop busy-spins re-acquiring `ctx.api` -- the same mutex the
        // companion's own `PassThruReadMsgs` calls contend for -- instead of
        // taking the single bounded grace poll this mechanism is meant to be
        // (edge-case-hunter review of this Decision).
        let mut awaiting_companion_drain = false;
        match ok {
            PollMatchResult::Matched(count, concat_delta) => {
                matches_got += count;
                // ADR-148 Amendment 8 (Codex round-9 finding): sync this
                // loop's own `concat_segments_got` baseline to a concat
                // delta that coexisted with this same completing match --
                // e.g. a byte/segment cap force-finalizing a concat buffer
                // inside `bind_registrant` increments `matches_got` in the
                // same pass it also incremented `concat_segments_got`.
                // Without this, the baseline would fall behind the live
                // registrant and the very next poll would phantom-report
                // the already-accounted-for delta as a fresh `Absorbed`,
                // needlessly restarting the `CP_P2Max` deadline again.
                concat_segments_got += concat_delta;
                if no_deadline {
                    // ADR-100 Decision §2 tier migration (S5): this is
                    // necessarily this wait's FIRST match -- a no_deadline
                    // (IS-CYCLIC) wait always returns right here the first
                    // time this arm is reached, so a second pass through this
                    // arm, for this call, can never happen. Per spec, once
                    // the transmit is done and the first positive response
                    // has arrived, the ComPrimitive continues in a receive
                    // only mode. Flip this COP's own
                    // registrant to tier 2 and hand control back to the
                    // caller -- see `ReceivePhaseOutcome::DetachedToTier2`'s
                    // own doc comment for the full contract.
                    migrate_registrant_to_receive_only(&ctx.logical_links, cll_handle, cop_handle)
                        .await;
                    return ReceivePhaseOutcome::DetachedToTier2;
                }
                if matches_needed.is_some_and(|needed| matches_got >= needed) {
                    return ReceivePhaseOutcome::CycleComplete;
                }
                // Each accepted response restarts the response window: the
                // remaining expected responses get a fresh CP_P2Max each.
                deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
                if let Some(ceiling) = match_reset_ceiling {
                    deadline = deadline.min(ceiling);
                }
                poll_immediately = true;
            }
            PollMatchResult::Absorbed(count) => {
                // ADR-148. A segment
                // extended (or opened) this registrant's open concat buffer
                // this pass, with no full logical match completing yet --
                // `matches_got` does not advance, but the ECU is still
                // actively answering, so the response window restarts
                // exactly like an accepted match does above (same deadline
                // formula), instead of being left to run down toward a
                // spurious timeout mid-transfer.
                concat_segments_got += count;
                deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
                if let Some(ceiling) = match_reset_ceiling {
                    deadline = deadline.min(ceiling);
                }
                poll_immediately = true;
            }
            PollMatchResult::HardError => {
                // handle_channel_hard_error already emitted PDU_ERR_EVT_LOST_COMM_TO_VCI,
                // PDU_COPST_CANCELLED (for all COPs on this CLL via cancel_link_cops,
                // including this one), and PDU_CLLST_OFFLINE.  Do not emit any
                // additional status for this COP.
                return ReceivePhaseOutcome::Terminal;
            }
            PollMatchResult::PendingRc(rc_code) => {
                // A pending-response NRC was detected.  The frame has already been
                // delivered to rx_buf/subscribers.  Adjust the deadline according to
                // the RC handling config and, for re-request codes, re-send.
                match rc_code {
                    0x78 => {
                        // NRC 0x78 — ResponsePending: reload the deadline on EVERY
                        // occurrence to now + CP_P2Star (ISO 14229-2 §7.3's P2*client
                        // reload-per-occurrence semantics, ADR-102 — supersedes ADR-057's
                        // anchor-once treatment of this deadline, which was correct for
                        // CP_RC78CompletionTimeout's ceiling role but not for CP_P2Star's
                        // reload role). CP_RC78CompletionTimeout is a separate anti-stall
                        // total ceiling, anchored once at the *first* 0x78 in this COP;
                        // `None` (0/absent) means no ceiling applies (ISO 14229-2 defines
                        // none — CancelComPrimitive remains the escape for a chattering ECU).
                        deadline = tokio::time::Instant::now()
                            + Duration::from_millis(rc_cfg.rc78_p2_star_ms as u64);
                        if let Some(ceiling_ms) = rc_cfg.rc78_total_ceiling_ms {
                            let ceiling = *rc78_ceiling.get_or_insert_with(|| {
                                tokio::time::Instant::now()
                                    + Duration::from_millis(ceiling_ms as u64)
                            });
                            deadline = deadline.min(ceiling);
                        }
                        // ADR-102: also clamp to CoptStopcomm's IS-MULTIPLE
                        // match_reset_ceiling (ADR-087), which this branch
                        // previously ignored -- a chatty ECU sending 0x78
                        // could otherwise hold that non-cancellable receive
                        // phase open indefinitely, since CancelComPrimitive
                        // is deliberately ignored there.
                        if let Some(reset_ceiling) = match_reset_ceiling {
                            deadline = deadline.min(reset_ceiling);
                        }
                        poll_immediately = true;
                    }
                    0x21 | 0x23 => {
                        // NRC 0x21 (BusyRepeatRequest) / 0x23 (ConditionsNotCorrect):
                        // wait request_time_ms, then re-send the original request.
                        // Re-request keeps happening on every occurrence, but
                        // (ADR-057) only the code's *first* occurrence
                        // establishes its ceiling (now + completion_ms); later
                        // occurrences of the same code re-request under that
                        // same fixed ceiling rather than pushing it out again.
                        let (completion_ms, request_time_ms, ceiling_slot) = if rc_code == 0x21 {
                            (
                                rc_cfg.rc21_completion_timeout_ms,
                                rc_cfg.rc21_request_time_ms,
                                &mut rc21_ceiling,
                            )
                        } else {
                            (
                                rc_cfg.rc23_completion_timeout_ms,
                                rc_cfg.rc23_request_time_ms,
                                &mut rc23_ceiling,
                            )
                        };

                        // ADR-102 (Codex review, PR #101 on the original
                        // reverted fix): `match_reset_ceiling` must bound the
                        // request_time_ms sleep and the retransmit below too,
                        // not just the `deadline` this arm computes at its
                        // very end -- a client-configured `CP_RC21/23RequestTime`
                        // longer than CoptStopcomm's IS-MULTIPLE ceiling
                        // (ADR-087) would otherwise still hold this
                        // non-cancellable phase open past it. Checked once
                        // before the sleep (skip both the sleep and the
                        // retransmit outright if the ceiling has already
                        // passed by the time this RC21/23 occurrence is
                        // handled) and again on every chunk boundary inside
                        // it (bail out of the remaining sleep, and skip the
                        // retransmit, the moment it elapses mid-wait).
                        let ceiling_already_elapsed = || {
                            match_reset_ceiling
                                .is_some_and(|ceiling| tokio::time::Instant::now() >= ceiling)
                        };
                        let mut skip_retransmit = ceiling_already_elapsed();

                        if request_time_ms > 0 && !skip_retransmit {
                            // Sleep in `POLL_INTERVAL_MS`-sized chunks
                            // (mirroring `wait_for_p3_gap`'s own chunked
                            // wait) rather than one solid sleep, so a
                            // `cancellable: false` (ADR-087 StopComm)
                            // receive phase keeps draining an ignored
                            // `CancelComPrimitive`'s `cancelled_cops` entry
                            // throughout this wait too -- not only once
                            // this whole sleep (plus the retransmit below)
                            // finally lets the loop reach its per-pass
                            // check. Left un-chunked, `GetStatus(COP)`
                            // would still lie for up to the full,
                            // client-configured `request_time_ms` (Codex
                            // review, PR #92, second round of this same
                            // finding). `cancellable: true` (`CoptSendrecv`)
                            // is deliberately unaffected: this only drains
                            // the marker on each chunk boundary, it never
                            // breaks the sleep early or otherwise changes
                            // `CoptSendrecv`'s existing timing -- the total
                            // slept duration is identical either way (unlike
                            // the new ceiling check below, which DOES break
                            // early -- but only ever applies when
                            // `match_reset_ceiling` is `Some`, i.e. only for
                            // `cancellable: false`'s CoptStopcomm IS-MULTIPLE
                            // caller, so `CoptSendrecv`'s timing is still
                            // unaffected in practice).
                            let mut remaining = Duration::from_millis(request_time_ms as u64);
                            let chunk = Duration::from_millis(POLL_INTERVAL_MS);
                            while remaining > Duration::ZERO {
                                if ceiling_already_elapsed() {
                                    skip_retransmit = true;
                                    break;
                                }
                                let step = chunk.min(remaining);
                                tokio::time::sleep(step).await;
                                remaining -= step;
                                if !cancellable {
                                    let mut links = ctx.logical_links.lock().await;
                                    if let Some(l) = links.get_mut(&cll_handle) {
                                        l.cancelled_cops.remove(&cop_handle);
                                    }
                                }
                                // CP_TesterPresentSendType (either value,
                                // ADR-095 audit round): this chunked
                                // RC21/RC23 request_time_ms wait can hold
                                // this poll task just as long as the
                                // top-level response window above -- fire any
                                // due tester-present CLL on this channel on
                                // every chunk here too, or a sibling CLL
                                // could starve for the whole re-request wait.
                                // No boxing needed: this function is not on
                                // `dispatch_due_tester_present`'s own call
                                // graph (it never calls
                                // `wait_for_expected_response`), so there is
                                // no async-fn recursion cycle to break here,
                                // unlike Fix A's `isotp_send` call sites.
                                // `defer_gap_wait = true`, matching the
                                // enclosing receive phase's own top-level
                                // call above: a tester-present send's
                                // unprobed `poll_rx` inside `wait_for_p3_gap`
                                // must not drain the very ECU response this
                                // receive phase is waiting to match.
                                //
                                // ADR-095 amendment (2026-07-18): reap any
                                // detached (tier-2) registrant that would
                                // otherwise starve for this entire chunked
                                // retry wait, same as the tester-present
                                // dispatch just below -- reap runs first,
                                // matching `run_due_tick_duties`'s own order.
                                // No boxing needed (see
                                // `run_detached_registrant_maintenance`'s own
                                // doc comment). Called unconditionally
                                // (ADR-101 Decision §E): no RX poll runs
                                // anywhere in this chunked sleep loop, so this
                                // channel's own drain watermark simply stays
                                // as stale as it already was -- the reap
                                // itself defers a genuinely expired
                                // registrant until RX is next exhaustively
                                // drained elsewhere, not lost; the
                                // cancellation reap above still runs every
                                // chunk unconditionally.
                                run_detached_registrant_maintenance(ctx).await;
                                dispatch_due_tester_present(true, None, None, false, ctx).await;
                            }
                        }

                        // ADR-102 (Codex review, PR #101, second round): the
                        // in-loop `ceiling_already_elapsed` check above only
                        // catches the ceiling elapsing *before* a chunk's
                        // sleep starts -- if the *last* chunk's sleep itself
                        // is what crosses the ceiling (`remaining` reaches
                        // `Duration::ZERO` in the same iteration, so the loop
                        // exits via its `while` condition, not the `break`),
                        // `skip_retransmit` would otherwise still be `false`
                        // here even though the ceiling has, by now, already
                        // elapsed. Recheck once more, unconditionally, right
                        // after the sleep loop -- covers both that case and
                        // the `request_time_ms == 0` (no sleep at all) case,
                        // where this is simply a no-op re-confirmation of the
                        // pre-sleep check already done above.
                        if ceiling_already_elapsed() {
                            skip_retransmit = true;
                        }

                        // Folded recheck (ADR-086, defense-in-depth): skip
                        // the re-request itself when stale -- this protects
                        // a side effect mid-loop, not an independent
                        // decision point (mirrors the round-8 classification
                        // of the transmit-error recheck elsewhere in this
                        // file). For the `Ok(())`/`Cancelled`/`ChannelLost`
                        // outcomes below, the per-pass check later in this
                        // same iteration is still the authoritative bail-out.
                        // The `TxFailure::Event` arm is the one exception --
                        // it `return`s immediately, so the per-pass check
                        // below is never reached for it; that arm carries its
                        // own recheck instead (round-9 fix, see below).
                        // ADR-102: also folded into staleness -- a
                        // ceiling-elapsed skip is not "stale" in the
                        // reconnect sense, but the effect (no retransmit)
                        // is identical, so it reuses the same guard.
                        //
                        // ADR-148 Amendment 10: this guard now also discards
                        // (never delivers) any concat buffer left open on
                        // this registrant, in the SAME `logical_links`
                        // acquisition -- a retransmit re-sends the original
                        // request, so a buffer still open here is an
                        // interrupted partial answer to an attempt that's
                        // about to be superseded; left in place, a later
                        // response from the same ECU/SID would be absorbed
                        // by `bind_registrant`'s continuation fast path as
                        // though it continued the PRE-retry buffer, silently
                        // merging bytes from two different request
                        // transmissions. Deliberately skipped (buffers left
                        // open) when `skip_retransmit` is true: no new
                        // request attempt is happening in that case, so the
                        // buffers remain valid and reach the normal
                        // deadline-finalize path instead. See
                        // `discard_concat_buffers_for_retransmit_if_live`'s
                        // own doc comment for the full rationale.
                        let retransmit_still_on_this_channel = !skip_retransmit
                            && discard_concat_buffers_for_retransmit_if_live(
                                &ctx.logical_links,
                                cll_handle,
                                ctx.channel_id,
                                connect_generation,
                                cop_handle,
                            )
                            .await;
                        // Re-send the original request (segmented again in
                        // software-ISO-TP mode).
                        if retransmit_still_on_this_channel {
                            match transmit_request(
                                cll_handle,
                                cop_handle,
                                protocol_id,
                                tx_flags,
                                original_data,
                                isotp_tx,
                                true,
                                cancellable,
                                ctx,
                            )
                            .await
                            {
                                Ok(()) => {}
                                Err(TxFailure::Event(ev)) => {
                                    // Recheck generation (ADR-086, round 9
                                    // fix): transmit_request performs real
                                    // I/O and can span a disconnect+reconnect
                                    // completing during the call, even though
                                    // the pre-transmit check above
                                    // (retransmit_still_on_this_channel)
                                    // already passed. This arm returns
                                    // immediately -- the per-pass check later
                                    // in this same iteration is never reached
                                    // -- so it must decide the terminal
                                    // status itself: a stale COP gets the
                                    // same first-wins PduCopstCancelled bail
                                    // every other guard in this file uses,
                                    // not a contradictory PduCopstFinished
                                    // after the client already saw Cancelled
                                    // from cancel_link_cops.
                                    let still_on_this_channel = {
                                        let links = ctx.logical_links.lock().await;
                                        links.get(&cll_handle).is_some_and(|l| {
                                            l.channel_id == Some(ctx.channel_id)
                                                && l.connect_generation == connect_generation
                                        })
                                    };
                                    if !still_on_this_channel {
                                        emit_terminal_if_live(
                                            &ctx.primitives,
                                            &ctx.logical_links,
                                            &ctx.subscriptions,
                                            &ctx.service.terminal_cops,
                                            cll_handle,
                                            cop_handle,
                                            PduComPrimitiveStatus::PduCopstCancelled,
                                        )
                                        .await;
                                        return ReceivePhaseOutcome::Terminal;
                                    }
                                    send_error_event(
                                        &ctx.subscriptions,
                                        &ctx.logical_links,
                                        cll_handle,
                                        ev,
                                        Some((cop_handle, cop_tag.clone())),
                                    )
                                    .await;
                                    // ADR-087: the caller owns the terminal
                                    // status from here -- handle_send_recv
                                    // emits PduCopstFinished itself
                                    // (byte-for-byte its pre-ADR-087
                                    // behavior), handle_stop_comm falls
                                    // through to its own terminal
                                    // PduCllstOnline/PduCopstFinished block.
                                    return ReceivePhaseOutcome::ReRequestTxFailed;
                                }
                                Err(TxFailure::ChannelLost) => {
                                    // Loss-of-comms events already emitted.
                                    return ReceivePhaseOutcome::Terminal;
                                }
                                Err(TxFailure::Cancelled) => {
                                    // First-wins through `primitives`, the
                                    // same discriminator every other
                                    // CANCELLED emitter uses: CancelComPrimitive
                                    // left the entry in place, so this remove
                                    // normally succeeds and this retransmit
                                    // emits the one CANCELLED. A concurrent
                                    // teardown (`cancel_link_cops` /
                                    // `cancel_held_tx_items`) that already
                                    // removed the entry has already emitted
                                    // it -- skip, never duplicate.
                                    let queue_target =
                                        resolve_queue_target(&ctx.logical_links, cll_handle).await;
                                    let mut prims = ctx.primitives.lock().await;
                                    if let Some(entry) = prims.remove(&cop_handle) {
                                        send_cop_status(
                                            &ctx.subscriptions,
                                            &ctx.service.terminal_cops,
                                            queue_target.as_ref(),
                                            cll_handle,
                                            cop_handle,
                                            PduComPrimitiveStatus::PduCopstCancelled,
                                            entry.cop_tag,
                                        )
                                        .await;
                                    }
                                    return ReceivePhaseOutcome::Terminal;
                                }
                            }
                        }

                        deadline = *ceiling_slot.get_or_insert_with(|| {
                            tokio::time::Instant::now()
                                + Duration::from_millis(completion_ms as u64)
                        });
                        // ADR-102: also clamp to CoptStopcomm's IS-MULTIPLE
                        // match_reset_ceiling (ADR-087) here, mirroring the
                        // 0x78 branch above -- the anchor-once RC21/23
                        // ceiling itself is unchanged, but it must not
                        // outlive the non-cancellable phase's own bound.
                        if let Some(reset_ceiling) = match_reset_ceiling {
                            deadline = deadline.min(reset_ceiling);
                        }
                        poll_immediately = true;
                    }
                    _ => {
                        // detect_pending_rc only returns 0x78/0x21/0x23; this arm
                        // is unreachable in practice but handled defensively.
                    }
                }
            }
            PollMatchResult::NoMatch => {}
        }

        // Cancellation is honoured on every pass — a CancelComPrimitive that
        // arrived during the last sleep takes precedence over the timeout
        // result, and IS-CYCLIC phases never reach the deadline check below.
        // `cancellable: false` (ADR-087) ignores a `CancelComPrimitive` for
        // *behavioral* purposes (the wait is not aborted), but still drains
        // any `cancelled_cops` entry the RPC inserted, on every pass, rather
        // than leaving it for `dispatch_tx_item`'s later post-completion
        // cleanup to reap: `rpc_get_status`'s `CopHandle` branch checks
        // `cancelled_cops` before `executing_cop` (Cancelled > Executing >
        // Waiting), so a lingering entry would make `GetStatus(COP)` report
        // `PduCopstCancelled` for a COP that is guaranteed to keep running
        // and later emit `PduCopstFinished` -- for as long as the receive
        // phase's own `CP_P2Max`/IS-MULTIPLE-ceiling/RC-completion window
        // takes, not merely "transiently" (Codex review, PR #92; the
        // draining here does not change round 6's identical, still-valid
        // "transient" acceptance for the much shorter, unchanged transmit
        // step's own `cancellable: false` window in
        // `transmit_request`/`isotp_send`, which this fix leaves alone).
        // `HashSet::remove` is a no-op on an absent key, so draining an
        // empty/already-drained set on a later pass is always safe.
        //
        // Per-pass generation-stale check (ADR-086), folded into this SAME
        // `logical_links` lock acquisition rather than a new one: this is
        // the most important site in the whole ADR-086 amendment -- an
        // IS-CYCLIC receive (`no_deadline`, below) has no deadline and is
        // otherwise ended only by `cancelled_cops`, which `cancel_link_cops`
        // bypasses entirely. Without this check, a stale IS-CYCLIC receive
        // would loop forever past a disconnect+reconnect of this CLL. This
        // half always runs, regardless of `cancellable`.
        let (was_cancelled, is_stale, uudt_channel_id) = {
            let mut links = ctx.logical_links.lock().await;
            let drained = links
                .get_mut(&cll_handle)
                .map(|l| l.cancelled_cops.remove(&cop_handle))
                .unwrap_or(false);
            let cancelled = cancellable && drained;
            let stale = !links.get(&cll_handle).is_some_and(|l| {
                l.channel_id == Some(ctx.channel_id) && l.connect_generation == connect_generation
            });
            // ADR-101 Decision §E's tier-1 extension: captured here, under
            // the same `logical_links` lock acquisition this loop already
            // uses for the cancellation/staleness check, so the
            // deadline-break check below can consult the companion's own
            // drain watermark without a second lock acquisition.
            let uudt_channel_id = links.get(&cll_handle).and_then(|l| l.uudt_channel_id);
            (cancelled, stale, uudt_channel_id)
        };
        if was_cancelled {
            // First-wins through `primitives`, the same discriminator every
            // other CANCELLED emitter uses: CancelComPrimitive left the
            // entry in place, so this remove normally succeeds and this pass
            // emits the one CANCELLED. A concurrent teardown
            // (`cancel_link_cops` / `cancel_held_tx_items`) that already
            // removed the entry has already emitted it -- skip, never
            // duplicate.
            let queue_target = resolve_queue_target(&ctx.logical_links, cll_handle).await;
            let mut prims = ctx.primitives.lock().await;
            if let Some(entry) = prims.remove(&cop_handle) {
                send_cop_status(
                    &ctx.subscriptions,
                    &ctx.service.terminal_cops,
                    queue_target.as_ref(),
                    cll_handle,
                    cop_handle,
                    PduComPrimitiveStatus::PduCopstCancelled,
                    entry.cop_tag,
                )
                .await;
            }
            return ReceivePhaseOutcome::Terminal;
        }
        if is_stale {
            // First-wins bail-out idiom (ADR-086): `cancel_link_cops` may
            // already have removed this COP from `primitives` and emitted
            // `PduCopstCancelled` directly at disconnect time, bypassing
            // `cancelled_cops` entirely -- only emit here if this guard is
            // first to remove it.
            emit_terminal_if_live(
                &ctx.primitives,
                &ctx.logical_links,
                &ctx.subscriptions,
                &ctx.service.terminal_cops,
                cll_handle,
                cop_handle,
                PduComPrimitiveStatus::PduCopstCancelled,
            )
            .await;
            return ReceivePhaseOutcome::Terminal;
        }
        // CP_TesterPresentSendType (either value, ADR-083): the response
        // phase can hold this poll task for an arbitrarily long time --
        // unbounded for IS-CYCLIC (no_deadline) and up to the response
        // timeout otherwise -- without ever returning to
        // poll_channel_events's outer select loop. Fire any due
        // tester-present CLL on this channel on every non-terminal iteration
        // here too, or a sibling CLL sharing this physical channel could
        // starve for the whole wait. defer_gap_wait = true here (ADR-083):
        // unlike the outer-select and handle_delay call sites, this poll
        // loop is match-sensitive for the active COP's response -- a
        // tester-present send's own wait_for_p3_gap wait must not drain RX
        // via its unprobed poll_rx and consume that response as unattributed
        // background traffic.
        // Deadline check moved to BEFORE the dispatch call (ADR-095
        // amendment, PR #97 8th Codex review round): `dispatch_due_tester_present`'s
        // send is real hardware write I/O that consumes wall-clock time, so
        // checking the deadline only after the dispatch let a dispatch run
        // on an already-expired wait, and let an expected-response frame
        // that arrived on the wire *during* that write sit unread while the
        // loop broke out on timeout without ever polling again. Skipped
        // entirely for IS-CYCLIC (`no_deadline`), which has no window to
        // expire.
        if !no_deadline && tokio::time::Instant::now() >= deadline {
            // ADR-101 Decision §E's tier-1 extension: a CLL with a UUDT
            // companion channel (ADR-046) can have an in-time response still
            // sitting un-polled in the companion's own, entirely independent
            // adapter queue at the exact instant this deadline is reached --
            // the companion poll task simply hasn't reached its next tick
            // yet, or is itself mid-hold. Breaking unconditionally here would
            // end the wait and later discard that response as unbound
            // (ADR-100 Decision §5) once the companion does eventually poll,
            // a spurious `PduErrEvtRxTimeout` despite a genuine in-time
            // reply. Before breaking, require the companion's own drain
            // watermark to have caught up to this deadline too; if it
            // hasn't, take one more pass instead of breaking -- the same
            // bounded "one grace poll" idiom the deadline-check-before-
            // dispatch reordering above already established for a dispatch
            // that overruns its own deadline. A single-channel CLL
            // (`uudt_channel_id` is `None`) breaks unconditionally, exactly
            // as before this change.
            let companion_caught_up = match uudt_channel_id {
                None => true,
                Some(uudt_id) => ctx
                    .drain_watermarks
                    .lock()
                    .await
                    .get(&uudt_id)
                    .is_some_and(|&wm| wm >= deadline),
            };
            if companion_caught_up {
                // ADR-148 Amendment. Before
                // falling through to the ordinary "genuine timeout" check
                // below, give every still-open concat buffer a chance to
                // finalize as its own completed logical match instead of
                // being silently dropped -- per spec intent, a deadline
                // expiring mid-transfer with data already collected is a
                // completed (if short) response, not a bare timeout. One
                // SHARED deadline covers the whole registrant (restarted by
                // any absorbed/matched frame on ANY buffer, not per-buffer):
                // while any ECU is still replying the bus isn't idle, so an
                // already-complete-looking buffer is deliberately held open
                // until the whole burst quiets down, matching `CP_P2Max`'s
                // link-level-response-window intent rather than a
                // per-response one. When multiple buffers are open (
                // IS-MULTIPLE, several ECUs replying to one broadcast/
                // functional request), ALL of them finalize together, in ONE
                // pass, not one per poll. Concat-eligible registrants
                // (KWP/J1850 family, `CopRegistrant::concat_enabled`'s own
                // doc comment) never have a UUDT companion channel (ADR-046
                // is CAN/ISO15765-only), so `uudt_channel_id` is always
                // `None` and `companion_caught_up` is always `true`
                // unconditionally for them -- this branch is therefore
                // always reached for a concat-eligible registrant whose
                // deadline expires.
                // edge-case-hunter Finding 4 fix: `concat_segments_got` is
                // this loop's own cumulative "segments absorbed so far"
                // counter (mirrors `caller_cumulative_concat_segments_got`'s
                // pattern elsewhere in this function) -- `0` proves no
                // segment has ever been absorbed for this registrant during
                // this whole receive phase, which proves no buffer could
                // possibly be open right now, without needing to thread
                // `CopRegistrant::concat_enabled` itself in here (deliberately
                // discarded above as `enable_concatenation: _`). Skips the
                // `logical_links` lock+lookup below entirely for the common
                // (non-concat, or concat-enabled-but-never-segmented) case.
                // edge-case-hunter Finding 2 fix (round following the
                // multi-buffer amendment), further folded per round-7 Codex
                // finding: guard-check, drain, AND delivery into `rx_buf` now
                // all happen together, atomically, inside ONE
                // `logical_links` lock acquisition in
                // `finalize_and_deliver_concat_buffers_if_live` (its own doc
                // comment has the full before/after). The prior shape
                // (guard+drain under the lock, then a delivery loop here
                // AFTER the lock was released, returning
                // `(Vec<ConcatDelivery>, Option<rx_buf>)`) left an await gap
                // in which a `DisconnectComLogicalLink`/reconnect could clear
                // `channel_id`/cancel the COP, yet the already-drained batch
                // would still land in the retained (not destroyed by
                // disconnect) `rx_buf` queue -- a stale post-cancellation
                // delivery. The helper now returns just the delivered count
                // (`u32`); `delivered_count > 0` is the single guard-held
                // signal, replacing the old `!finalized_concats.is_empty()`/
                // `Some(rx_buf)` pair -- guard failure and "no buffers were
                // open" both still read as `0`, so `matches_got`/completion
                // below remain conditional on the guard having held exactly
                // as before. Any buffer left open by a failed guard is
                // cleaned up by the next pass's own per-pass staleness check
                // above (which returns `Terminal`) or, on an actual
                // disconnect, by `cancel_link_cops`'s `registrants.clear()`
                // -- never re-attempted from here.
                let (delivered_count, live_concat_segments_got) = if concat_segments_got > 0 {
                    finalize_and_deliver_concat_buffers_if_live(
                        &ctx.logical_links,
                        &ctx.primitives,
                        cll_handle,
                        ctx.channel_id,
                        connect_generation,
                        cop_handle,
                    )
                    .await
                } else {
                    (0, 0)
                };
                if delivered_count > 0 {
                    // `matches_got` (the live registrant's own field) was
                    // already incremented once per buffer inside
                    // `finalize_concat_buffers` -- this local mirrors that
                    // same total, exactly like the pre-amendment single-
                    // delivery path did with its own `+= 1`.
                    matches_got += delivered_count;
                    // ADR-148 Amendment 8's second fix (edge-case-hunter
                    // finding on the round-9 `Matched`-delta fix, same bug
                    // class in this sibling path): resync this loop's own
                    // local `concat_segments_got` baseline to the LIVE
                    // absolute value, not a delta. Currently defense-in-
                    // depth, not a live-race fix (design-advisor correction,
                    // post-approval; see the helper's own doc comment and
                    // ADR-148 Amendment 8's Correction paragraph for the
                    // full trace) -- the sibling-COP scenario this was
                    // originally written to close turned out unreachable
                    // given this file's single-poller-per-channel dispatch,
                    // so every live `concat_segments_got` advance already
                    // reaches this loop's own baseline via
                    // `check_match_against_baseline` before this point is
                    // ever hit. Kept because it is cheap and would become
                    // load-bearing if that topology, or this ADR's v1
                    // tier-1-only concat scope, ever changes. An absolute
                    // assignment (not `+=`) is correct either way:
                    // `live_concat_segments_got` was read fresh, in the same
                    // lock acquisition as the drain, so it is already the
                    // exact live total.
                    concat_segments_got = live_concat_segments_got;
                    // `is_none_or`, not `is_some_and` (edge-case-hunter-class
                    // fix, Codex review P2): reaching this branch AT ALL
                    // means the receive-phase deadline just expired with one
                    // or more buffers still open -- i.e. no new segment
                    // arrived during the whole prior window, which is
                    // exactly IS-MULTIPLE's (`matches_needed == None`,
                    // `NumReceiveCycles == -2`) own pre-existing completion
                    // signal: the collection window closes once a full
                    // `CP_P2Max` period passes with nothing new (ADR-053).
                    // `is_some_and` treated `None` as "never satisfied,"
                    // which for a concat-enabled IS-MULTIPLE registrant --
                    // whose `matches_got` only ever advances via this same
                    // finalize path, since every match is funneled through
                    // a buffer -- meant this branch always restarted a
                    // SECOND full `CP_P2Max` window after already
                    // establishing silence once, needlessly doubling the
                    // COP's completion latency. A finite `matches_needed`
                    // (`Some(n)`) is unaffected: `is_none_or` still
                    // evaluates the same `matches_got >= needed` closure for
                    // `Some`, only the `None` case's result changes (`false`
                    // -> `true`).
                    if matches_needed.is_none_or(|needed| matches_got >= needed) {
                        return ReceivePhaseOutcome::CycleComplete;
                    }
                    deadline =
                        tokio::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
                    if let Some(ceiling) = match_reset_ceiling {
                        deadline = deadline.min(ceiling);
                    }
                    poll_immediately = true;
                } else {
                    // ADR-101 Decision §E correction (Codex review of PR #103,
                    // round 8): `companion_caught_up` proves the companion's own
                    // writeback merge has landed in LIVE registrant state (the
                    // watermark is stamped only after that merge), but THIS
                    // iteration's own `matches_got` local was sampled earlier, at
                    // the top-of-loop `poll_rx_and_check_match` call above -- a
                    // point that can predate the companion's merge, since that
                    // call, the cancellation/staleness check, and this watermark
                    // read are three separate `logical_links` lock acquisitions.
                    // Re-check live state before trusting the local counter's
                    // staleness. `check_match_against_baseline` is reused here
                    // because it is a pure read against the registrant's current
                    // `matches_got`/`pending_rc` -- it does NOT reset
                    // `pending_rc` (only `observe_and_consume_pending_rc_outcome`
                    // does that); consuming it here would violate Decision §C's
                    // exactly-one-consumer invariant, since this site does not
                    // act on the result itself -- it only decides whether to
                    // still break.
                    let live_has_unobserved_delta = {
                        let links = ctx.logical_links.lock().await;
                        links
                            .get(&cll_handle)
                            .and_then(|l| l.registrants.iter().find(|r| r.cop_handle == cop_handle))
                            .is_some_and(|r| {
                                !matches!(
                                    check_match_against_baseline(
                                        r.matches_got,
                                        r.pending_rc,
                                        matches_got,
                                        r.concat_segments_got,
                                        concat_segments_got,
                                    ),
                                    PollMatchResult::NoMatch
                                )
                            })
                    };
                    if !live_has_unobserved_delta {
                        break; // genuine timeout -- confirmed against fresh live state
                    }
                    // The companion's already-merged delta is real; let the next
                    // top-of-loop `poll_rx_and_check_match` pass observe and
                    // process it through the existing match/RC-handling arms,
                    // unchanged -- no new handling logic is written at this site.
                    poll_immediately = true;
                }
            } else {
                awaiting_companion_drain = true;
            }
        }

        // ADR-095 amendment (2026-07-18): reap any detached (tier-2)
        // registrant that would otherwise starve for this receive phase's
        // entire wait (unbounded for IS-CYCLIC), same as the tester-present
        // dispatch just below -- reap runs first, matching
        // `run_due_tick_duties`'s own order. No boxing needed (see
        // `run_detached_registrant_maintenance`'s own doc comment). Called
        // unconditionally (ADR-101 Decision §E): `reap_expired_cyclic_
        // registrants` consults the per-channel drain watermark map
        // directly, so this call site no longer needs to compute any
        // exhaustiveness signal of its own.
        run_detached_registrant_maintenance(ctx).await;
        dispatch_due_tester_present(true, None, None, false, ctx).await;

        if no_deadline {
            // IS-CYCLIC: no response window; keep polling until cancelled.
            if !poll_immediately {
                tokio::time::sleep(poll_interval).await;
            }
            continue;
        }

        if poll_immediately {
            // Skip the sleep and re-enter the poll immediately (deadline was
            // already extended/reset above).
            continue;
        }
        // Deadline-saturating: `dispatch_due_tester_present` above may have
        // overrun the deadline itself; `saturating_duration_since` (unlike
        // `deadline - now`) never panics/underflows in that case, and yields
        // a zero sleep, giving this loop one grace poll before the deadline
        // check (now moved above) ends the window on the NEXT pass.
        //
        // `awaiting_companion_drain` overrides this to normal `poll_interval`
        // pacing: once `now >= deadline`, the deadline-saturating formula is
        // permanently zero, so without this override every pass spent
        // waiting on the companion's watermark (ADR-101 Decision §E) would
        // busy-spin instead of taking one bounded poll interval per pass.
        let now = tokio::time::Instant::now();
        let sleep_for = if awaiting_companion_drain {
            poll_interval
        } else {
            deadline.saturating_duration_since(now).min(poll_interval)
        };
        tokio::time::sleep(sleep_for).await;
    }

    // The response window closed.  For IS-MULTIPLE this is the normal end of
    // the phase when at least one response arrived; anything short of the
    // required match count is a receive timeout.
    if matches_needed.map_or(matches_got == 0, |needed| matches_got < needed) {
        // R3 (ADR-086 round 11): gate the event on a fresh
        // `still_on_this_channel` check -- the terminal COP status is owned
        // by the caller's S4 guard regardless (this function still returns
        // `CycleComplete` either way), but a stray `PduErrEvtRxTimeout` must
        // not fire for a session the client already saw `PduCopstCancelled`
        // for.
        // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c Fix A): a
        // `tx_suspended_by_error` SET below must also terminate an
        // already-running TP2.0 broadcast periodic on the same CLL, in the
        // SAME critical section -- extracted here as `suspended_periodic`,
        // acted on after `links` is released, see
        // `terminate_tp20_broadcast_periodic_for_suspension`'s own doc
        // comment.
        //
        // Codex review fix (P2, PR #101, round 10): `connect_generation`/
        // `channel_key` are carried alongside `periodic`/`channel_id`,
        // captured from the SAME `l` reference in this SAME critical
        // section -- see that function's own doc comment for why the
        // callee needs them.
        let (still_on_this_channel, suspended_periodic): (
            bool,
            Option<SuspendedBroadcastPeriodicEntry>,
        ) = {
            let mut links = ctx.logical_links.lock().await;
            match links.get_mut(&cll_handle) {
                Some(l)
                    if l.channel_id == Some(ctx.channel_id)
                        && l.connect_generation == connect_generation =>
                {
                    // ADR-147: `CP_SuspendQueueOnError` is a live, link-scoped
                    // policy read (unlike `RcHandlingConfig`'s frozen COP-time
                    // snapshot) -- the freshness check above is reused as-is,
                    // matching the timeout-hook precedent this ADR follows.
                    //
                    // ADR-147 fifth amendment (split direction-specific
                    // anchors -- this SET is the sole `error_set_seq` bump
                    // site, replacing the fourth amendment's bump of the
                    // unified `error_state_seq`; unaffected by the sixth
                    // amendment's later change to WHEN `set_seq_at_read` is
                    // captured relative to the read, only to where): this
                    // SET of
                    // `tx_suspended_by_error` is a SYNCHRONOUS state change
                    // (its evidence -- this receive phase's own timeout -- is
                    // realized at the exact instant it is applied here), so
                    // it bumps `error_set_seq` too, in this SAME critical
                    // section and strictly BEFORE `send_error_event` below --
                    // preserving the existing set-before-expose property.
                    // This is `Positive`'s BATCH-READ anchor moving forward:
                    // any pass whose `CllRxEntry::set_seq_at_read` was
                    // captured before this bump (i.e. every batch read up to
                    // and including this instant) will mismatch this new
                    // live `error_set_seq` at apply time and so can never
                    // apply a `Positive` classification that would silently
                    // clear the flag this timeout just raised -- regardless
                    // of exactly when within that batch's processing the
                    // `Positive` was folded. Only a batch read AFTER this
                    // bump (a later `PassThruReadMsgs` call, which
                    // necessarily reflects wire content the client sent
                    // after this timeout) can capture the post-bump value and
                    // so genuinely resume the suspension -- see
                    // `queue_error_class_to_apply`'s own doc comment for the
                    // full race shape this closes.
                    let mut periodic = None;
                    if l.active.suspend_queue_on_error() {
                        l.tx_suspended_by_error = true;
                        l.error_set_seq += 1;
                        periodic = l
                            .tp20_broadcast_periodic
                            .take()
                            .map(|p| (p, l.channel_id, l.connect_generation, l.channel_key));
                    }
                    (true, periodic)
                }
                _ => (false, None),
            }
        };
        if let Some((periodic, channel_id, connect_generation, channel_key)) = suspended_periodic {
            ctx.service
                .terminate_tp20_broadcast_periodic_for_suspension(
                    cll_handle,
                    periodic,
                    channel_id,
                    connect_generation,
                    channel_key,
                )
                .await;
        }
        if still_on_this_channel {
            send_error_event(
                &ctx.subscriptions,
                &ctx.logical_links,
                cll_handle,
                PduErrorEvent::PduErrEvtRxTimeout,
                Some((cop_handle, cop_tag.clone())),
            )
            .await;
        }
    }
    ReceivePhaseOutcome::CycleComplete
}

#[derive(Debug)]
enum PollMatchResult {
    /// This many non-pending frames are newly attributed to `cop_handle`'s
    /// own registrant beyond the caller's own cumulative count (ADR-101
    /// Decision §B; diffed against `caller_cumulative_matches_got` -- see
    /// this function's doc comment); always >= 1. The second field is the
    /// `concat_segments_got` delta (ADR-148 Amendment 8) observed in this
    /// SAME pass, diffed against `caller_cumulative_concat_segments_got`
    /// exactly like the first field is against `caller_cumulative_matches_got`;
    /// `0` when nothing was absorbed this pass, or always for a non-concat
    /// registrant. This exists because a completing match and a concat
    /// segment absorb can coexist in one pass -- most directly, a byte/segment
    /// cap force-finalizing a concat buffer inside `bind_registrant`
    /// increments `matches_got` synchronously in the same batch that also
    /// incremented `concat_segments_got` -- and `Matched` outranks `Absorbed`
    /// in `check_match_against_baseline`'s priority order below, so without
    /// this second field the coexisting concat delta would be silently
    /// dropped; the caller's own `concat_segments_got` baseline would then
    /// fall behind the live registrant and phantom-report the already-
    /// accounted-for delta as a fresh `Absorbed` on the very next poll,
    /// needlessly restarting the `CP_P2Max` deadline again.
    Matched(u32, u32),
    /// ADR-148. This many segments
    /// were newly absorbed into `cop_handle`'s own registrant's open concat
    /// buffer beyond the caller's own cumulative `concat_segments_got` count
    /// this pass, with no full logical match completing in the same batch
    /// (a completed merge is reported as `Matched` instead, since it also
    /// advances `matches_got`); always >= 1. Never reported for a
    /// non-concat registrant (`concat_segments_got` never advances for one).
    Absorbed(u32),
    /// A frame bound to `cop_handle`'s own registrant via its pending-RC
    /// detection (`RcHandlingConfig::detect_pending_rc`) in this pass, and no
    /// final matching frame was found in the same poll batch.
    PendingRc(u8),
    /// No relevant frame received.
    NoMatch,
    HardError,
}

/// Calls `PassThruReadMsgs` once for `target_cll`'s channel (via
/// [`poll_rx_inner`]), fans frames out to all CLLs sharing that channel
/// (applying UniqueRespIdTable routing and, for software-ISO-TP CLLs,
/// reassembly — ADR-046), attributing each one via `bind_frame`'s full
/// ADR-100 Decision §3 precedence scan -- not just against `cop_handle`'s own
/// registrant, unlike the pre-ADR-100 `MatchProbe` this replaces -- and
/// reports the outcome for `cop_handle`'s OWN registrant specifically, read
/// back from `logical_links[target_cll].registrants` after the pass:
///
/// `cop_handle`'s registrant's `pending_rc` is observed, and reset to `None`
/// when the reported outcome is `Matched` or `PendingRc` (ADR-101 Decision
/// §C, revised by ADR-148 Amendment 5 -- PRESERVED, not reset, when the
/// outcome is `Absorbed`, since an absorbed segment is not a completion),
/// atomically with the observation, in a single lock acquisition, AFTER the
/// pass: any `Some` value seen at that point is an unreported occurrence
/// since the last time this call consumed the field -- regardless of
/// whether it was set by this call's own pass or merged in by a
/// concurrently-running UUDT companion channel's own writeback (ADR-101
/// Decision §A) at any point since. There is deliberately no separate
/// pre-poll reset: an earlier design reset the field before this call's own
/// poll pass, which could silently and permanently discard a companion-
/// detected occurrence that landed between two of this function's calls,
/// before it was ever reported.
///
/// `caller_cumulative_matches_got` (ADR-101 Decision §B) is the WAIT LOOP's
/// own locally-tracked cumulative match count
/// (`wait_for_expected_response_inner`'s own `matches_got`), threaded in as
/// the baseline this call diffs the live registrant's `matches_got` against
/// -- NOT re-read fresh from the live registrant at the start of this call,
/// as it was before ADR-101. A private per-call baseline has the identical
/// shape of bug the writeback merge itself had one layer down: even with a
/// correct delta merge summing cross-channel contributions into
/// `live.matches_got`, a companion-channel increment landing between two of
/// this wait loop's own calls would be invisible to a freshly-re-read
/// baseline -- it would silently become part of the NEXT call's baseline
/// instead of being reported back to the wait loop's own counter, which
/// would then keep waiting for a match count the live registrant already
/// reached. Threading in the caller's own cumulative count instead means
/// every registrant increment -- from this call's own pass or any
/// concurrently-merged companion-channel pass -- is visible to the very
/// next call that observes it.
///
/// Priority when multiple messages arrive in one batch:
///   Matched (non-pending) > Absorbed > PendingRc > NoMatch
///
/// Returns `NoMatch` (never a panic) if `target_cll` or `cop_handle`'s own
/// registrant is missing at either read -- e.g. the CLL was torn down
/// concurrently; `wait_for_expected_response_inner`'s own per-pass staleness
/// check is the authoritative bail-out for that case, one pass later.
///
/// Returns only this call's own [`PollMatchResult`] -- an earlier revision
/// (ADR-095 amendment) also returned this call's own `poll_rx_inner`
/// [`PollOutcome`] as a second tuple element, consumed by
/// `wait_for_expected_response_inner`'s receive-phase loop bottom as its own
/// `run_detached_registrant_maintenance` gate input. ADR-101 Decision §E
/// replaced that per-call-site exhaustiveness gating with a per-channel drain
/// watermark map `reap_expired_cyclic_registrants` consults directly, so
/// nothing reads the `PollOutcome` half of that tuple anymore -- simplified
/// back to a bare `PollMatchResult`. This function's own internal
/// `poll_rx_inner` call still needs its `PollOutcome` to detect a hard error,
/// just no longer returns it to the caller.
///
/// `caller_cumulative_concat_segments_got` (ADR-148) is the wait loop's own locally-tracked
/// cumulative `concat_segments_got` count, threaded in exactly like
/// `caller_cumulative_matches_got` above and for the identical reason --
/// always `0`/unchanged for a non-concat registrant.
async fn poll_rx_and_check_match(
    target_cll: u32,
    cop_handle: u32,
    caller_cumulative_matches_got: u32,
    caller_cumulative_concat_segments_got: u32,
    ctx: &ChannelPollCtx,
) -> PollMatchResult {
    let poll_outcome = poll_rx_inner(ctx, None).await;

    if !poll_outcome.is_ok() {
        return PollMatchResult::HardError;
    }

    let mut links = ctx.logical_links.lock().await;
    let Some(registrant) = links.get_mut(&target_cll).and_then(|l| {
        l.registrants
            .iter_mut()
            .find(|r| r.cop_handle == cop_handle)
    }) else {
        return PollMatchResult::NoMatch;
    };

    observe_and_consume_pending_rc_outcome(
        registrant,
        caller_cumulative_matches_got,
        caller_cumulative_concat_segments_got,
    )
}

/// Pure tail of `poll_rx_and_check_match`, split out for direct unit testing
/// (ADR-101 Decision §B): diffs a freshly-read live registrant's
/// `matches_got`/`pending_rc` against `baseline` to report this call's own
/// outcome. `poll_rx_and_check_match` is itself not independently unit
/// testable -- its `poll_rx_inner` call always resolves through
/// `ctx.api.lock().await`/`read_messages`, which requires a live
/// `J2534Api0404` bound to a loaded shared library, only available via the
/// `grpc_mock` integration harness's mock `.so` -- so this extraction is
/// what actually lets the ADR-101 Decision §B baseline-threading fix be
/// pinned by a fast, deterministic test.
fn check_match_against_baseline(
    live_matches_got: u32,
    live_pending_rc: Option<u8>,
    baseline: u32,
    live_concat_segments_got: u32,
    concat_segments_baseline: u32,
) -> PollMatchResult {
    if live_matches_got > baseline {
        PollMatchResult::Matched(
            live_matches_got - baseline,
            live_concat_segments_got - concat_segments_baseline,
        )
    } else if live_concat_segments_got > concat_segments_baseline {
        // ADR-148. Ranked below
        // `Matched` (a completed merge already reports as `Matched`, via
        // `matches_got`, and that always outranks a same-pass `Absorbed`)
        // and above `PendingRc` -- an absorbed segment is still positive
        // forward progress on this registrant's own response, unlike a
        // pending-response NRC.
        PollMatchResult::Absorbed(live_concat_segments_got - concat_segments_baseline)
    } else if let Some(rc) = live_pending_rc {
        PollMatchResult::PendingRc(rc)
    } else {
        PollMatchResult::NoMatch
    }
}

/// Atomic observe-and-consume of `registrant.pending_rc` (ADR-101 Decision
/// §C), extracted from `poll_rx_and_check_match` so it can be exercised
/// directly against a `&mut CopRegistrant` without going through
/// `poll_rx_inner`'s live-hardware dependency -- the same testability
/// motivation that produced `check_match_against_baseline`.
///
/// Calls `check_match_against_baseline` against the registrant's CURRENT
/// live state, then resets `pending_rc` to `None` in the same borrow when,
/// and only when, the reported outcome is `Matched` or `PendingRc` (ADR-148
/// Amendment 5, revising this function's earlier unconditional reset).
///
/// `Matched` (which outranks both `Absorbed` and `PendingRc`) still clears
/// unconditionally, exactly as before -- see ADR-101 Decision §C and the
/// Consequences section's residual (a): a completing match means the ECU
/// already fully answered, so leaving a stale RC to be reported on a later
/// call, after a match already superseded it, would trigger a spurious
/// 0x21/0x23 retransmit for a request already answered. `PendingRc` also
/// clears, since it was just reported here -- the existing
/// "every writer of `pending_rc` only ever sets it when currently `None`"
/// invariant (`bind_registrant`'s intra-pass detection and ADR-101 Decision
/// §A's cross-channel merge) already covers a reported occurrence.
///
/// `Absorbed`, however, is NOT a completion -- the registrant is still
/// mid-accumulation, so `pending_rc` is deliberately PRESERVED across it
/// rather than discarded unreported. `check_match_against_baseline` ranks
/// `Absorbed` above `PendingRc`, so one poll batch containing both an
/// absorbable continuation for this registrant and a genuine pending-RC
/// occurrence (from the same or a different ECU) previously reported
/// `Absorbed` while this call's own unconditional reset silently discarded
/// the coexisting RC, unreported, and its P2*/RC21/RC23 timing action never
/// fired -- ADR-148 Amendment 5. Preserving it here means it correctly
/// surfaces as `PollMatchResult::PendingRc` on the very next poll pass
/// instead (the `Absorbed` arm's caller always sets `poll_immediately =
/// true`, so that next pass is immediate, not a full poll-interval later).
fn observe_and_consume_pending_rc_outcome(
    registrant: &mut CopRegistrant,
    caller_cumulative_matches_got: u32,
    caller_cumulative_concat_segments_got: u32,
) -> PollMatchResult {
    let result = check_match_against_baseline(
        registrant.matches_got,
        registrant.pending_rc,
        caller_cumulative_matches_got,
        registrant.concat_segments_got,
        caller_cumulative_concat_segments_got,
    );
    if matches!(
        result,
        PollMatchResult::Matched(_, _) | PollMatchResult::PendingRc(_)
    ) {
        registrant.pending_rc = None;
    }
    result
}

/// `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES` (ADR-079): pins `push_cll_event`'s
/// two eviction policies directly, without spinning up a poll task -- all
/// three entry sources (`poll_rx_inner`, `handle_start_comm`'s synthetic
/// fast-init response, `send_error_event`'s async error events) reach this
/// same helper unconditionally via `deliver_or_enqueue`'s first step
/// (ADR-115 round-3 correction), so this is exhaustive for the eviction
/// behavior itself -- see `deliver_or_enqueue`'s own doc comment and its
/// `grpc_mock` integration tests (`pdu_ioctl.rs`) for the
/// push-then-live-drain behavior layered on top.
#[cfg(test)]
#[path = "events_push_cll_event_tests.rs"]
mod push_cll_event_tests;

/// ADR-115 round 6 (design-advisor redesign, superseding round 4/5's
/// generation-gate class of fix entirely): `deliver_or_enqueue` reads
/// `queue.live_sender` fresh, under the queue lock it already holds, instead
/// of trusting a subscriber reference some caller captured ahead of time.
/// There is no separate capture step left to go stale, so the round-4 race
/// (a `rpc_subscribe_event` replacement landing between a capture and a lock
/// acquisition) is structurally eliminated rather than merely detected.
///
/// Exercised directly against `deliver_or_enqueue` and `CllEventQueue`,
/// mirroring `rpc_primitive::rollback_stop_comm_pending_tests`'s own "drive
/// the exact mechanism directly" pattern for narrow-window races infeasible
/// to force through this crate's single-threaded `current_thread` test
/// runtime.
#[cfg(test)]
#[path = "events_deliver_or_enqueue_live_sender_tests.rs"]
mod deliver_or_enqueue_live_sender_tests;

#[cfg(test)]
#[path = "events_init_sequence_tests.rs"]
mod init_sequence_tests;

/// ADR-100 S1: the response-binding registry itself is write-only (nothing
/// consults `LogicalLinkState::registrants` yet), so these tests exercise
/// `insert_cop_registrant`/`remove_cop_registrant`/`cancel_link_cops`'s
/// registrants-clear directly against a bare `logical_links` map -- no poll
/// task, `ChannelPollCtx`, or mock J2534 library needed, since none of the
/// three touch anything else.
#[cfg(test)]
#[path = "events_registrant_lifecycle_tests.rs"]
mod registrant_lifecycle_tests;

/// ADR-161: `cancel_held_tx_items`'s new `expected_generation` parameter,
/// exercised directly against a bare `logical_links`/`primitives`/
/// `subscriptions` map -- no poll task or mock J2534 library needed, since
/// the generation gate lives entirely inside this function's own single
/// `logical_links` critical section.
#[cfg(test)]
#[path = "events_cancel_held_tx_items_generation_tests.rs"]
mod cancel_held_tx_items_generation_tests;

/// ADR-100 S3: exhaustive coverage of `bind_frame`'s Decision §3 attribution
/// precedence table, exercised directly against a synthetic `CllRxEntry` --
/// no poll task, `ChannelPollCtx`, or mock J2534 library needed, since
/// `bind_frame`/`bind_registrant` are plain synchronous functions over
/// owned/borrowed data. The field-bug end-to-end regression (a vacuous
/// `CoptSendrecv` no longer captures a tester-present reply) is additionally
/// pinned at the `grpc_mock` integration level in
/// `tests/grpc_mock/tester_present_reqrsp.rs`; these tests cover every
/// precedence-order transition in isolation.
#[cfg(test)]
#[path = "events_bind_frame_tests.rs"]
mod bind_frame_tests;
/// ADR-101 Decision §A: `merge_registrant_writeback`'s three formulas, tested
/// directly -- a plain synchronous function over owned/borrowed data, no
/// poll task or `ChannelPollCtx` needed, exactly like `bind_frame_tests`
/// above. Each test exercises exactly one of the three fields the merge
/// touches so a regression in one formula cannot hide behind an unrelated
/// field happening to still look right.
#[cfg(test)]
#[path = "events_merge_registrant_writeback_tests.rs"]
mod merge_registrant_writeback_tests;

/// ADR-147 sixth amendment (pre-read capture for the batch anchor,
/// refining the fifth amendment's split direction-specific anchors):
/// `queue_error_class_to_apply`'s truth table, tested directly -- a plain
/// synchronous function over one `Option<QueueErrorClass>` plus
/// scalar/`Option<u64>`/`bool` arguments, no poll task or `ChannelPollCtx`
/// needed, exactly like `merge_registrant_writeback_tests` above (whose own
/// doc comment explains why the reactive race itself is unforceable
/// deterministically in this mock harness and so is not attempted as an
/// integration test here).
///
/// `Suspend` tests carry over the third-amendment shape essentially
/// unchanged (renamed to the split fields: `entry_suspend_seq` vs.
/// `live_error_clear_seq`) -- the anchor and capture point for this
/// direction did not change in the fifth or sixth amendments. `Positive`
/// tests use BATCH-anchor semantics (`entry_set_seq_at_read: Option<u64>`
/// vs. `live_error_set_seq`) -- the fourth amendment's fold-time capture for
/// `Positive` no longer exists at all, and the sixth amendment additionally
/// makes the anchor `Option`-typed to cover the "CLL absent from the
/// pre-read snapshot" case. A handful of cross-check tests confirm each
/// classification variant is genuinely indifferent to the OTHER direction's
/// counter, per design-advisor's own trace: no scenario needs `Suspend`
/// checked against `error_set_seq`, or `Positive` against `error_clear_seq`.
#[cfg(test)]
#[path = "events_queue_error_class_to_apply_tests.rs"]
mod queue_error_class_to_apply_tests;

/// ADR-147 sixth amendment (pre-read capture for the batch anchor):
/// `build_cll_rx_entries`'s `CllRxEntry::set_seq_at_read` stamping logic,
/// tested directly against a supplied pre-read snapshot map -- the actual
/// cross-task race the pre-read capture closes (a concurrent poll task
/// bumping `error_set_seq` in the gap between this function's caller
/// snapshotting it and this function itself running) is structurally
/// unforceable deterministically in this harness (no controllable
/// interleaving point exists between two independent tokio tasks each
/// racing their own `logical_links` lock acquisition), so these tests pin
/// the deterministic part instead: given a snapshot map, does this function
/// stamp `Some(seq)` for a CLL handle present in it, and `None` for one
/// absent from it (the "connected mid-window" case, ADR-147 sixth
/// amendment item 3)?
#[cfg(test)]
#[path = "events_build_cll_rx_entries_tests.rs"]
mod build_cll_rx_entries_tests;

/// ADR-101 Decision §D: the eager `cyclic_deadline` confirm-and-write-through
/// step (`is_eager_cyclic_deadline_confirm_scope`/
/// `confirm_cyclic_deadline_writeback`), tested directly against pure,
/// extracted logic -- `poll_rx_inner` itself is not unit-testable here (see
/// `check_match_against_baseline_tests`'s own doc comment for why), and a
/// true end-to-end repro of the race this closes (a companion channel's
/// confirm step racing the primary channel's own `reap_expired_cyclic_
/// registrants` tick, both independently-ticking poll tasks with real,
/// unpaused timers) hits the same structural infeasibility already recorded
/// for `check_match_against_baseline_tests`'s own companion-channel race.
/// The tests below instead pin the mechanism directly using
/// `is_cyclic_deadline_expired` -- the EXACT predicate
/// `reap_expired_cyclic_registrants` itself evaluates -- so "the registrant
/// survives once confirm has run, vs. is reaped if it never ran" is proven
/// against the real reap-eligibility check, not a hand-copied stand-in.
#[cfg(test)]
#[path = "events_cyclic_deadline_confirm_writeback_tests.rs"]
mod cyclic_deadline_confirm_writeback_tests;

/// ADR-101 Decision §B: `check_match_against_baseline`'s baseline-threading
/// fix, tested directly against the pure tail `poll_rx_and_check_match`
/// itself calls. `poll_rx_and_check_match`/`wait_for_expected_response_inner`
/// are not independently unit-testable here -- `poll_rx_inner` always
/// resolves through `ctx.api.lock().await`/`read_messages`, which requires a
/// live `J2534Api0404` bound to a loaded shared library, only available via
/// the `grpc_mock` integration harness's mock `.so`.
///
/// A true concurrent-hardware-channel repro (two independently-ticking
/// physical channel poll tasks, one landing its own writeback strictly
/// between two of the OTHER channel's own `poll_rx_and_check_match` calls)
/// was not attempted at the `grpc_mock` integration level: this crate's
/// integration tests run on a `current_thread` `#[tokio::test]` runtime with
/// real, unpaused timers, so the two tasks' 10ms poll ticks have no
/// controllable pause/gate hook to force one channel's writeback to land
/// deterministically inside the other channel's own narrow inter-call
/// window -- the same structural infeasibility class already recorded for
/// comparable narrow-window races in this package's own
/// `docs/implementation-notes.md` (the round-5/round-6 dispatch-window
/// notes). The tests below instead pin the mechanism directly: given the
/// exact live-registrant readings a companion-channel writeback would
/// produce, a cumulative baseline observes the contribution and a
/// fresh-per-call baseline (the pre-ADR-101 behavior) silently absorbs it.
#[cfg(test)]
#[path = "events_check_match_against_baseline_tests.rs"]
mod check_match_against_baseline_tests;

#[cfg(test)]
#[path = "events_observe_and_consume_pending_rc_outcome_tests.rs"]
mod observe_and_consume_pending_rc_outcome_tests;

/// ADR-095 amendment (2026-07-18, final revision): required test 3 --
/// `classify_poll_batch`'s exhaustiveness classification, pinned directly
/// (`poll_rx_inner` itself is not independently unit-testable -- it always
/// resolves through `ctx.api.lock().await`/`read_messages`, which requires a
/// live `J2534Api0404` bound to a loaded shared library, only available via
/// the `grpc_mock` integration harness's mock `.so`) -- plus `PollOutcome`'s
/// own `is_ok`/`is_drained` helpers, which every `poll_rx_inner`/`poll_rx`
/// call site relies on.
#[cfg(test)]
#[path = "events_poll_outcome_tests.rs"]
mod poll_outcome_tests;

/// ADR-101 Decision §E: `is_cyclic_reap_sound`, the pure predicate
/// `reap_expired_cyclic_registrants` consults (ANDed with
/// `is_cyclic_deadline_expired`) to decide whether a cyclic registrant's
/// deadline expiry is soundly observable yet -- i.e. every channel that can
/// deliver it a match has completed an exhaustive drain whose read began at
/// or after that deadline. Pinned directly since `reap_expired_cyclic_
/// registrants` itself is not unit-testable in isolation (constructing a
/// `ChannelPollCtx` needs a live `J2534Api0404` bound to a loaded shared
/// library); a true end-to-end repro of the cross-channel race this closes
/// (two independently-ticking poll tasks with real, unpaused timers) hits the
/// same structural infeasibility already recorded for
/// `check_match_against_baseline_tests`'s own companion-channel race --
/// covered instead by the `grpc_mock` integration tests in
/// `tests/grpc_mock/cop_ctrl_cycles.rs`.
#[cfg(test)]
#[path = "events_is_cyclic_reap_sound_tests.rs"]
mod is_cyclic_reap_sound_tests;

/// Edge-case-hunter regression (ADR-182 follow-up fix): `reap_expired_
/// cyclic_decision`, the pure per-registrant reap decision including the
/// `cancelled_cops` skip-guard this fix added. Pinned directly for the same
/// reason as `is_cyclic_reap_sound_tests` above: `reap_expired_cyclic_
/// registrants` itself is not unit-testable in isolation, and a true
/// end-to-end repro of the same-tick S5-vs-S6 race this fix closes (a
/// `CancelComPrimitive` RPC handler task racing this same poll task's own
/// tick, both independently scheduled by the tokio runtime with no
/// controllable pause/gate hook between them) hit the same structural
/// infeasibility already recorded for `cyclic_deadline_confirm_writeback_
/// tests`'s and `check_match_against_baseline_tests`'s own comparable
/// narrow-window races -- confirmed empirically for this specific race
/// during this fix's own verification: neither a single well-timed
/// `CancelComPrimitive` nor a burst of many concurrent ones reliably landed
/// inside the gap via `tests/grpc_mock/cop_ctrl_cycles.rs`'s real,
/// unpaused-timer harness, across a wide sweep of `CP_CyclicRespTimeout`
/// values. `tests/grpc_mock/cop_ctrl_cycles.rs`'s own
/// `receive_only_finite_n_cancel_races_cyclic_timeout_expiry_stays_cancelled`
/// still covers the general end-to-end mechanism (a cancel landing anywhere
/// up to and including that gap resolves to Cancelled, no error, no
/// suspend) as defense in depth, but the tests below are what actually pin
/// the fix's own decision logic deterministically.
#[cfg(test)]
#[path = "events_reap_expired_cyclic_decision_tests.rs"]
mod reap_expired_cyclic_decision_tests;

/// ADR-182 follow-up fix (Codex review round, PR #78): `drain_cancelled_
/// cop_if_finalized`, the shared cleanup helper `rpc_cancel_com_primitive`'s
/// own self-check and the other batch-cancel sites route through (see its
/// own doc comment for the current call-site list -- `reap_expired_cyclic_
/// registrants`'s former late drain was folded into `emit_terminal_if_live`
/// itself by the PR #78 edge-case-hunter follow-up and is no longer a
/// separate call site here).
/// Directly unit-testable in isolation -- unlike the reap/RPC race itself,
/// this helper takes only `Arc<Mutex<HashMap<...>>>` state, no
/// `ChannelPollCtx`/live `J2534Api0404` needed -- so it is pinned here
/// rather than only covered indirectly through the (structurally infeasible
/// to land deterministically, see `reap_expired_cyclic_decision_tests`
/// above) end-to-end race itself. This module also now covers
/// `emit_terminal_if_live`'s own internal drain and `cancel_link_cops`'s
/// wholesale clear (the same PR #78 fix).
#[cfg(test)]
#[path = "events_drain_cancelled_cop_if_finalized_tests.rs"]
mod drain_cancelled_cop_if_finalized_tests;

/// ADR-204 follow-up (Codex review, PR #116; edge-case-hunter verification):
/// `reap_expired_cyclic_registrants`'s `ReapedReceiveOnly::cop_tag` capture,
/// driven directly against the real function -- unlike the S5-vs-S6 race
/// `reap_expired_cyclic_decision_tests` above documents as unforceable, this
/// gap sits WITHIN one `reap_expired_cyclic_registrants` call and is
/// forceable deterministically via `tokio::sync::Mutex`'s FIFO fairness; see
/// this test file's own module doc comment for the full mechanism and its
/// fail-without/pass-with verification.
#[cfg(test)]
#[path = "events_reap_expired_cyclic_registrants_cop_tag_tests.rs"]
mod reap_expired_cyclic_registrants_cop_tag_tests;

/// ADR-205 Decision item 1: the `CoptStartcomm` failure path Codex's fourth
/// review round named as the concrete new instance of the same bug class
/// `reap_expired_cyclic_registrants_cop_tag_tests` closed above -- driven
/// directly against `handle_start_comm` itself; see this test file's own
/// module doc comment for the full mechanism and its fail-without/pass-with
/// verification.
#[cfg(test)]
#[path = "events_handle_start_comm_cop_tag_tests.rs"]
mod handle_start_comm_cop_tag_tests;

/// ADR-120 amendment: `PDU_IOCTL_RESET` must rebase the synthetic module
/// clock to (approximately) zero, not just capture it once at process start
/// (ISO 22900-2 §9.1.6.1 / 2022 §8.1.6.1: the module time base is defined to
/// zero both at boot and on every `PDU_IOCTL_RESET`).
#[cfg(test)]
#[path = "events_module_clock_reset_tests.rs"]
mod module_clock_reset_tests;

/// ADR-192/Phase 7 Stage 7c Fix B (design-advisor consult, Codex review
/// round 3): `handle_channel_hard_error`'s new leak-tracking push for a live
/// TP2.0 broadcast periodic -- see this test file's own module doc comment
/// for why it drives `handle_channel_hard_error` directly rather than
/// through `tests/grpc_mock`.
#[cfg(test)]
#[path = "events_hard_error_broadcast_periodic_tests.rs"]
mod hard_error_broadcast_periodic_tests;

/// Codex review round 8 Finding 1 (PR #101, ADR-192/Phase 7 Stage 7c):
/// direct unit coverage of `revert_hardware_to_live_active_locked` -- see
/// this test file's own module doc comment for why it is a direct test
/// rather than a `tests/grpc_mock` one.
#[cfg(test)]
#[path = "events_revert_hardware_to_live_active_locked_tests.rs"]
mod revert_hardware_to_live_active_locked_tests;

/// Codex review round 17 fix (P1, PR #101, ADR-192/Phase 7 Stage 7c
/// amendment): direct unit coverage of `handle_send_recv`'s continuous-
/// `ctx.api`-guard bracket for a `ParamBinding::Temp` + `isotp_tx.is_none()`
/// cycle -- see this test file's own module doc comment for the fence
/// technique and why it is a direct test rather than a `tests/grpc_mock`
/// one.
#[cfg(test)]
#[path = "events_handle_send_recv_api_fence_tests.rs"]
mod handle_send_recv_api_fence_tests;
