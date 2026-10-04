use std::{
    collections::{HashMap, HashSet, VecDeque},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use portable_atomic::{AtomicBool, AtomicU64, Ordering};

use j2534_0404::{ChannelId, DeviceId, J2534Api0404, MessageFilterId};
use tokio::sync::{Mutex, mpsc, oneshot, watch::Receiver};
use tokio_stream::Stream;
use tonic::{Code, Request, Response, Status};
use vci_service_interface::{self, EventNotification, PduError, vci_service_server::VciService};
use vci_service_launcher::{BoxError, vci_server::VciServer};

use crate::config::StartupConfig;
use crate::error::{
    map_construct_error, map_native_error_for_link, map_registry_error, state_guard_status,
    unknown_handle_status,
};

pub(super) type SubscriptionSender =
    mpsc::UnboundedSender<Result<vci_service_interface::EventNotification, Status>>;

/// Key for the subscription map: `(module_handle, cll_handle)`.
///
/// For CLL subscriptions the second element is the CLL handle.
/// For module-level subscriptions the second element is `PDU_HANDLE_UNDEF`.
/// This avoids the collision that would occur when using a bare `u32`
/// because J2534 assigns `DEFAULT_MODULE_HANDLE = 1` and CLL handles
/// also start from 1.
pub(super) type SubscriptionKey = (u32, u32);

/// A single frame received from the adapter, buffered by the poll task.
///
/// Stored in the per-CLL `rx_buf` ring buffer so that `GetEventItem` callers
/// can drain received frames without racing against the poll task for
/// `PassThruReadMsgs` access.
#[derive(Debug)]
pub(super) struct ReceivedFrame {
    pub(super) timestamp: u32,
    /// Payload only — leading header bytes (e.g. a CAN ID) and trailing
    /// footer bytes (e.g. a KWP2000/J1850 checksum) are split into
    /// `header_bytes`/`footer_bytes` instead, for the protocols ADR-051
    /// covers (CAN, ISO15765, ISO9141, ISO14230, J1850). Every other
    /// protocol: the frame exactly as read from the J2534 library
    /// (`header_bytes`/`footer_bytes` are empty).
    pub(super) data: Vec<u8>,
    /// This frame's `data` had these bytes stripped from its leading edge
    /// (e.g. a CAN ID, or a KWP2000/J1850 format/address/length header),
    /// surfaced to the client via `ResultData.extra_info` instead of
    /// `data_bytes` (ADR-051). Empty when the protocol has no header
    /// concept here.
    pub(super) header_bytes: Vec<u8>,
    /// This frame's `data` had these bytes stripped from its trailing edge
    /// (e.g. a KWP2000 checksum or J1850 CRC byte), surfaced via
    /// `ResultData.extra_info` instead of `data_bytes` (ADR-051). Empty
    /// when the protocol has no footer concept, or none was present on
    /// this particular frame (e.g. checksum-managed J2534 connections,
    /// where the vendor DLL already stripped it before this service ever
    /// saw the frame).
    pub(super) footer_bytes: Vec<u8>,
    /// Populated from UniqueRespIdTable routing; 0 when table is empty or routing was bypassed.
    pub(super) unique_resp_identifier: u32,
    /// Populated from the matching ExpectedResponse descriptor; 0 for frames outside
    /// a request/response cycle.
    pub(super) acceptance_id: u32,
    /// The `CoptSendrecv` / `CoptStartcomm` COP whose request this frame is a
    /// response to; `None` for frames received outside a request/response cycle
    /// (e.g. the background `poll_rx` timer with no matching in-flight COP).
    pub(super) cop_handle: Option<u32>,
    /// The client-supplied `StartComPrimitiveRequest.cop_tag` (ADR-204) for
    /// `cop_handle`'s COP, resolved from `CopEntry::cop_tag` (`primitives`)
    /// at the time this frame was bound to that COP -- `None` whenever
    /// `cop_handle` is `None` (no COP to look a tag up for), and also `None`
    /// when `cop_handle` is `Some` but no tag was supplied at
    /// `StartComPrimitive`. Echoed verbatim on `EventItem.cop_tag`
    /// (`events_event_senders::cll_queue_item_to_event_item`'s `Frame` arm)
    /// -- ADR-204's Decision item 1 requires the echo whenever `cop_handle`
    /// is present, and a `ResultData` frame is the primary case a client
    /// relies on it for (the response to a `CoptSendrecv` COP).
    pub(super) cop_tag: Option<Vec<u8>>,
    /// The 5 low bits of the source J2534 `PASSTHRU_MSG.RxStatus`
    /// (`TX_MSG_TYPE`, `START_OF_MESSAGE`, `RX_BREAK`, `TX_INDICATION`,
    /// `ISO15765_PADDING_ERROR`), bit-copied unconditionally (ADR-098).
    /// `0` means a Normal Message. Drives `ResultData.rx_flag` encoding
    /// (`events::rx_flag_bytes`) and gates `ExpectedResponse`/pending-RC
    /// matching eligibility: eligible unless one of the 4 indication-type
    /// bits (`TX_MSG_TYPE`, `START_OF_MESSAGE`, `RX_BREAK`, `TX_INDICATION`)
    /// is set -- `ISO15765_PADDING_ERROR` alone does not exclude a frame
    /// (ADR-098 Correction). `0` for the synthetic fast-init response frame,
    /// which has no real `RxStatus`.
    pub(super) rx_status_flags: u8,
    /// `true` only when this frame was a qualifying `0xC3` Access Timing
    /// Parameter response (`CP_ModifyTiming`, ADR-146) that actually
    /// produced a ComParam modification -- drives the synthesized
    /// `ECU_TIMING_CHANGE` `RxFlag` bit (`events::rx_flag_bytes`), distinct
    /// from `rx_status_flags`, which is bit-copied verbatim from the
    /// adapter's own `RxStatus` (ADR-098) rather than computed by this
    /// service. `false` for every frame this mechanism does not apply to,
    /// including the synthetic fast-init response frame.
    pub(super) ecu_timing_change: bool,
    /// `true` only when this frame is a genuine SW-CAN-family reception
    /// (`resources::is_sw_protocol_id`) whose raw `RxStatus` carries bit 16
    /// (SAE J2534-2 clause 9.4.1.1 Table 12 `SW_CAN_HV_RX`) -- drives the
    /// forwarded ISO 22900-2 Table D.5 `RxFlag` byte 1 bit 0
    /// (`events::rx_flag_bytes`/`RxFlagExtras`, ADR-191). `false` for every
    /// frame this does not apply to, including the synthetic fast-init
    /// response frame and every finalized `CP_EnableConcatenation` delivery.
    pub(super) sw_can_hv_rx: bool,
}

/// An entry in a CLL's per-handle event queue (`LogicalLinkState::rx_buf`):
/// a received frame, an async error event, or a CLL/COP status transition,
/// sharing one true-arrival-order FIFO under
/// `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`'s capacity/eviction policy --
/// matching ISO 22900-2's single typed per-handle event queue (`PDU_IT_RESULT`
/// / `PDU_IT_ERROR` / `PDU_IT_STATUS`), not independent streams (ADR-105
/// revision, extended to `Status` by the P2 follow-up closing the same gap
/// for `send_cll_status`/`send_cop_status` one item-kind over). `GetEventItem`
/// maps `Frame` to `ResultData`, `Error` to `ErrorData`, and `Status` to
/// `CllStatus`/`CopStatus`, exactly like the shapes `SubscribeEvent` already
/// sends down one channel.
#[derive(Debug)]
pub(super) enum CllQueueItem {
    Frame(ReceivedFrame),
    Error(TrackedError),
    Status(TrackedStatus),
    /// A qualifying ADR-146 timing-change frame reserved at its TRUE arrival
    /// position in the queue (Codex round-5 finding, PR #17) but not yet
    /// finalized: `poll_rx_inner`'s hardware push for this pass hasn't
    /// completed, so `frame.ecu_timing_change` may still need correcting.
    /// Reserving the position (rather than buffering outside the queue)
    /// preserves the queue's true-arrival-order FIFO contract for
    /// eviction/capacity accounting and for any OTHER item (status/error/a
    /// later frame) that arrives while this one is still pending -- only
    /// FINALIZATION (the flag's final value, and live/poll delivery) is
    /// deferred, never the queue position itself. Safe only because
    /// `poll_rx_inner` never aborts mid-pass (see `poll_channel_events`'s
    /// cancel/shutdown, which only break BETWEEN iterations) -- a
    /// reservation is always finalized in the SAME `poll_rx_inner` call that
    /// created it, so it can never be stranded pending forever.
    PendingTimingFrame {
        reservation_id: u64,
        frame: ReceivedFrame,
    },
}

/// A CLL or COP status transition (`send_cll_status`/`send_cop_status`),
/// paired with the module-clock time it was recorded -- the `Status`
/// counterpart to [`TrackedError`], queued alongside `Frame`/`Error` in the
/// same per-CLL `CllQueueItem` FIFO so `GetEventItem` pollers see it too, not
/// just a live `SubscribeEvent` subscriber.
#[derive(Debug, Clone)]
pub(super) struct TrackedStatus {
    pub(super) event: StatusEvent,
    pub(super) timestamp: u32,
}

/// Which status transition a [`TrackedStatus`] carries -- CLL-scoped
/// (`send_cll_status`, mirrors `EventItemData::CllStatus`, never carries a
/// `cop_handle`) or COP-scoped (`send_cop_status`, mirrors
/// `EventItemData::CopStatus`, always paired with the ComPrimitive it
/// pertains to).
///
/// `Cop` is constructed by every `send_cop_status` call site: each one
/// resolves a `CllQueueTarget` from `logical_links` *before* `primitives` is
/// acquired (`events::resolve_queue_target`/`CllQueueTarget::from_link`),
/// then passes it into `send_cop_status`, which enqueues this variant into
/// `CllQueueItem::Status` -- see `send_cop_status`'s own doc comment
/// (`events.rs`) for the full lock-order rationale.
#[derive(Debug, Clone)]
pub(super) enum StatusEvent {
    Cll(vci_service_interface::PduComLogicalLinkStatus),
    Cop {
        cop_handle: u32,
        status: vci_service_interface::PduComPrimitiveStatus,
        /// This COP's `CopEntry::cop_tag`, captured by the caller at the same
        /// time `cop_handle` was resolved against `primitives` (ADR-204) --
        /// carried alongside `status`/`cop_handle` rather than looked up
        /// again here, since by the time a terminal status is queued the
        /// `primitives` entry may already have been removed.
        cop_tag: Option<Vec<u8>>,
    },
}

/// A CLL's per-handle event queue (`LogicalLinkState::rx_buf`): the FIFO
/// backlog (`items`) bundled with a direct reference to whichever
/// `SubscribeEvent` subscription currently counts as this CLL's live
/// subscriber (`live_sender`) -- the ADR-115 round 6 redesign, replacing
/// round 4/5's generation-counter/captured-ref-comparison scheme with a
/// single co-resident source of truth.
///
/// **Why this exists**: round 4/5 had every producer (`poll_rx_inner`,
/// `send_error_event`, `handle_start_comm`) clone a `SubscriberRef` from
/// `J2534Service::subscriptions` *before* acquiring this queue's lock, then
/// compare that clone's captured generation against a separately-tracked
/// generation stamped into this struct. That comparison could only ever
/// detect a stale clone after the fact -- if `rpc_subscribe_event` replaced
/// the subscription in the gap between the two lock acquisitions, the
/// producer correctly refused to send to the now-dead captured `tx`, but had
/// nothing further to do: no delivery was ever attempted to the CURRENT,
/// correctly-registered subscriber, and a `Lost` notification computed for
/// THIS push (which is never buffered, by design -- see
/// `make_lost_notification`'s own doc comment) was silently dropped instead
/// of reaching anyone (Codex re-review finding on PR #122, round 6).
///
/// Bundling `live_sender` directly alongside `items` under this struct's one
/// `Mutex`, rather than tracking subscriber identity in a value cloned ahead
/// of time, eliminates the whole bug class rather than papering over one
/// instance of it: there is no longer a separate "capture, then maybe go
/// stale before the lock" step for a replacement to race against.
/// `deliver_or_enqueue` reads `live_sender.clone()` fresh, every call, under
/// this exact lock -- always current by construction, since nothing else can
/// write it without holding the same lock. Written by `rpc_subscribe_event`'s
/// atomic insert-and-stamp critical section (nested under `subscriptions`,
/// same lock order as before) and `rpc_create_com_logical_link`'s atomic
/// seed+insert; cleared to `None` by `J2534Service::terminate_subscription`/
/// `terminate_all_subscriptions` (when the removed entry's sender
/// `same_channel`s this field) and by `deliver_or_enqueue`'s own
/// send-failure self-heal. See
/// `docs/adr/ADR-115-pdu-evt-data-lost-emission.md`'s "Correction (round 6,
/// ...)" section for the full history and rationale, including why the
/// round-4/5 generation counter was removed rather than kept alongside this
/// (it became fully redundant the moment `live_sender` is read fresh under
/// the same lock at point of use -- a parallel copy would only reintroduce
/// the divergence class this redesign exists to eliminate).
///
/// **Lock-order rule** (see `J2534Service::logical_links`'s own doc comment
/// for the full table): `logical_links` -> `subscriptions` -> this queue's
/// lock. `deliver_or_enqueue` holds this lock for its whole duration and
/// must NEVER acquire `subscriptions` while holding it -- that is the one
/// reversal that would deadlock against `rpc_subscribe_event`'s own
/// `subscriptions` -> queue nesting. `deliver_or_enqueue` never needs to:
/// `live_sender` is everything it reads.
///
/// **`event_queue_cap`/`event_queue_mode`/`result_buffer_limit` live here,
/// not on `LogicalLinkState` (Codex review on PR #3, ADR-140 follow-up)**:
/// this mirrors the exact fix `live_sender` itself already got (ADR-115
/// round 6) for the identical bug shape -- a value ahead-of-time-captured
/// from `LogicalLinkState` by a producer (`CllQueueTarget`, `CllRxEntry`,
/// or a bare snapshot tuple) could go stale before that producer actually
/// reached this queue's lock, most concretely during the disconnect-
/// transition window where `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`'s
/// `pdu_connect_begun()` guard (ADR-126) reopens before `cancel_link_cops`
/// finishes, letting a client change queue policy while a stale-snapshotting
/// producer is still mid-flight. Keeping these three fields on this struct
/// instead means every producer (`deliver_or_enqueue`, `rpc_get_event_item`)
/// reads them fresh, under this exact lock, at the point of use -- there is
/// no longer a separate "capture, then maybe go stale before the lock" step
/// for a concurrent `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`/
/// `PDU_IOCTL_SET_BUFFER_SIZE` write to race against. A parallel copy on
/// `LogicalLinkState` would only reintroduce the exact divergence class this
/// redesign exists to eliminate (see `live_sender`'s own doc comment above,
/// same rationale).
#[derive(Debug)]
pub(super) struct CllEventQueue {
    pub(super) items: VecDeque<CllQueueItem>,
    pub(super) live_sender: Option<SubscriptionSender>,
    /// RX ring buffer capacity for this CLL
    /// (`PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`); defaults to
    /// `RX_BUF_CAPACITY`, matching pre-existing behavior.
    pub(super) event_queue_cap: usize,
    /// RX ring buffer overflow policy for this CLL
    /// (`PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`); defaults to
    /// `OverwriteOldest`, matching pre-existing behavior.
    pub(super) event_queue_mode: EventQueueMode,
    /// Per-item byte size cap for `GetComPrimitiveData` result items
    /// (`PDU_IOCTL_SET_BUFFER_SIZE`); distinct from `event_queue_cap`.
    pub(super) result_buffer_limit: Option<u32>,
    /// Monotonically increasing source of `CllQueueItem::PendingTimingFrame`
    /// reservation IDs (ADR-146 deferred-finalization delivery, Codex
    /// round-5 finding, PR #17) -- unique per queue, so
    /// `finalize_pending_timing_frame` can find the exact reservation it
    /// created even if other items have since been pushed/popped around it.
    pub(super) next_reservation_id: u64,
}

impl Default for CllEventQueue {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            live_sender: None,
            event_queue_cap: RX_BUF_CAPACITY,
            event_queue_mode: EventQueueMode::default(),
            result_buffer_limit: None,
            next_reservation_id: 0,
        }
    }
}

/// Maximum number of frames kept in the per-CLL receive ring buffer.
/// Oldest frames are dropped when the buffer is full.
pub(super) const RX_BUF_CAPACITY: usize = 64;

/// Default response window (ms) for the `SAE_J1850` VPW/PWM auto-detect
/// probe (ADR-070) when the connecting CLL's Working `CP_P2Max` is absent or
/// zero. See [`ComParamSet::j1850_autodetect_window_ms`].
pub(super) const J1850_AUTODETECT_DEFAULT_WINDOW_MS: u32 = 500;

mod service_params;
use service_params::*;

/// True if `id` collides with one of this service's own ~28
/// `PDU_IOCTL_BASE`-derived (`0x2900_0001..=0x2900_001C`) private IOCTL
/// ids -- each already matched by its own dedicated arm in
/// [`J2534Service::rpc_io_ctl`], ahead of the `cmd_id >= 0x10000`
/// vendor-dispatch fallback (`config::parse_vendor_ioctl_entry` calls this
/// to reject a `vendor_ioctls` entry for one of these ids at startup:
/// ADR-219's Accepted residual on this exact collision only covered a
/// future maintainer *relocating* one of these constants into the vendor
/// range -- not an operator's `vendor_ioctls` config independently landing
/// on one of the already-reserved values, which needed no relocation at
/// all to happen (Codex review, PR #133 tenth round). Such a contract would "load"
/// successfully at startup, yet `rpc_io_ctl` would never actually route a
/// request for it to `rpc_io_ctl_vendor` -- it is always intercepted by
/// the earlier, more specific match arm instead, e.g. silently resetting
/// module state instead of reaching the vendor DLL.
pub(crate) fn is_reserved_service_ioctl_id(id: u32) -> bool {
    matches!(
        id,
        PDU_IOCTL_RESET
            | PDU_IOCTL_CLEAR_TX_QUEUE
            | PDU_IOCTL_SUSPEND_TX_QUEUE
            | PDU_IOCTL_RESUME_TX_QUEUE
            | PDU_IOCTL_CLEAR_RX_QUEUE
            | PDU_IOCTL_READ_VBATT
            | PDU_IOCTL_SET_PROG_VOLTAGE
            | PDU_IOCTL_READ_PROG_VOLTAGE
            | PDU_IOCTL_GENERIC
            | PDU_IOCTL_SET_BUFFER_SIZE
            | PDU_IOCTL_START_MSG_FILTER
            | PDU_IOCTL_STOP_MSG_FILTER
            | PDU_IOCTL_CLEAR_MSG_FILTER
            | PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES
            | PDU_IOCTL_GET_CABLE_ID
            | PDU_IOCTL_SEND_BREAK
            | PDU_IOCTL_READ_IGNITION_SENSE_STATE
            | PDU_IOCTL_SW_CAN_HS
            | PDU_IOCTL_SW_CAN_NS
            | PDU_IOCTL_START_REPEAT_MESSAGE
            | PDU_IOCTL_QUERY_REPEAT_MESSAGE
            | PDU_IOCTL_STOP_REPEAT_MESSAGE
            | PDU_IOCTL_READ_J1962PIN_VOLTAGE
            | PDU_IOCTL_GET_DEVICE_CONFIG
            | PDU_IOCTL_SET_DEVICE_CONFIG
            | PDU_IOCTL_SET_POLL_RESPONSE
            | PDU_IOCTL_BECOME_MASTER
            | PDU_IOCTL_GET_NDIS_ADAPTER_INFO
    )
}

#[cfg(test)]
mod is_reserved_service_ioctl_id_tests {
    use super::*;

    #[test]
    fn rejects_every_reserved_pdu_ioctl_constant() {
        for id in [
            PDU_IOCTL_RESET,
            PDU_IOCTL_CLEAR_TX_QUEUE,
            PDU_IOCTL_SUSPEND_TX_QUEUE,
            PDU_IOCTL_RESUME_TX_QUEUE,
            PDU_IOCTL_CLEAR_RX_QUEUE,
            PDU_IOCTL_READ_VBATT,
            PDU_IOCTL_SET_PROG_VOLTAGE,
            PDU_IOCTL_READ_PROG_VOLTAGE,
            PDU_IOCTL_GENERIC,
            PDU_IOCTL_SET_BUFFER_SIZE,
            PDU_IOCTL_START_MSG_FILTER,
            PDU_IOCTL_STOP_MSG_FILTER,
            PDU_IOCTL_CLEAR_MSG_FILTER,
            PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES,
            PDU_IOCTL_GET_CABLE_ID,
            PDU_IOCTL_SEND_BREAK,
            PDU_IOCTL_READ_IGNITION_SENSE_STATE,
            PDU_IOCTL_SW_CAN_HS,
            PDU_IOCTL_SW_CAN_NS,
            PDU_IOCTL_START_REPEAT_MESSAGE,
            PDU_IOCTL_QUERY_REPEAT_MESSAGE,
            PDU_IOCTL_STOP_REPEAT_MESSAGE,
            PDU_IOCTL_READ_J1962PIN_VOLTAGE,
            PDU_IOCTL_GET_DEVICE_CONFIG,
            PDU_IOCTL_SET_DEVICE_CONFIG,
            PDU_IOCTL_SET_POLL_RESPONSE,
            PDU_IOCTL_BECOME_MASTER,
            PDU_IOCTL_GET_NDIS_ADAPTER_INFO,
        ] {
            assert!(
                is_reserved_service_ioctl_id(id),
                "{id:#010x} is a reserved PDU_IOCTL_BASE id and must be rejected"
            );
        }
    }

    #[test]
    fn accepts_a_genuine_vendor_range_id() {
        assert!(!is_reserved_service_ioctl_id(0x0001_0000));
        assert!(!is_reserved_service_ioctl_id(0x0001_0001));
    }

    #[test]
    fn accepts_values_adjacent_to_the_reserved_range() {
        assert!(!is_reserved_service_ioctl_id(PDU_IOCTL_BASE));
        assert!(!is_reserved_service_ioctl_id(
            PDU_IOCTL_GET_NDIS_ADAPTER_INFO + 1
        ));
    }
}

/// A snapshot of ComParam values — either the Working set (written by
/// `SetComParam` / read by `GetComParam`) or the Active set (what is
/// actually applied to the J2534 adapter).
///
/// # Working / Active lifecycle
///
/// | Event | Effect |
/// |-------|--------|
/// | `SetComParam` | write → Working |
/// | `GetComParam` | read ← Working |
/// | `ConnectComLogicalLink` (Offline → Online) | Working → Active + apply to hardware |
/// | `CoptUpdateparam` | Working → Active + apply to hardware |
/// | `CoptRestoreParam` | Active → Working (no hardware call) |
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct ComParamSet {
    /// Unum32 params keyed by ComParam ID.
    pub(super) unum32: HashMap<ComParamId, u32>,
    /// Bytefield params keyed by ComParam ID.
    pub(super) bytes: HashMap<ComParamId, Vec<u8>>,
    /// Structfield params keyed by ComParam ID.
    /// Used for CP_ExtendedTiming (PDU_CPST_ACCESS_TIMING) and
    /// CP_SessionTimingOverride (PDU_CPST_SESSION_TIMING).
    pub(super) structfield: HashMap<ComParamId, vci_service_interface::ParamStructfield>,
}

/// Snapshot of response-code handling ComParams read from the Active set at the
/// start of a `CoptSendrecv`.  Drives automatic handling of response-pending NRCs
/// (0x78 / 0x21 / 0x23) inside `wait_for_expected_response`.
#[derive(Debug, Clone, Default)]
pub(super) struct RcHandlingConfig {
    /// Byte offset within J2534 frame data where the response code resides.
    /// Set by `CP_RCByteOffset`; defaults to 0 when not configured.
    pub(super) rc_byte_offset: usize,

    /// If true, NRC 0x78 (ResponsePending) triggers an extended wait instead of
    /// finishing the COP immediately. Each 0x78 reloads the response deadline
    /// to `now + rc78_p2_star_ms` (ISO 14229-2 §7.3's P2*client reload-per
    /// -occurrence semantics, ADR-102), clamped to `rc78_total_ceiling_ms` when
    /// set (an independent anti-stall ceiling, anchored once at the first
    /// 0x78). `rc78_p2_star_ms` is sourced from `CP_P2Star` (D-PDU's standard
    /// "extended P2" timing parameter for exactly this purpose, ADR-056);
    /// `rc78_total_ceiling_ms` is sourced from `CP_RC78CompletionTimeout`
    /// (reinstated by ADR-102 — inert since ADR-056, now a distinct ceiling
    /// role rather than the deadline source).
    pub(super) rc78_handling: bool,
    pub(super) rc78_p2_star_ms: u32,
    pub(super) rc78_total_ceiling_ms: Option<u32>,

    /// If true, NRC 0x21 (BusyRepeatRequest) triggers a re-request after a short
    /// delay.  The COP deadline is then reset to `rc21_completion_timeout_ms`.
    ///
    /// `rc21_request_time_ms`/`rc23_request_time_ms` already fold in
    /// ADR-125's Annex I.1.4 handling, which is TWO DIFFERENT rules
    /// depending on protocol family, both requiring `CP_RC2xRequestTime`
    /// itself to be present in the ComParam set (so a legacy CLL with a
    /// completely empty preset still falls back to the plain 25 ms default
    /// for "nothing configured at all" rather than misreading true absence
    /// as an explicit `0`):
    ///
    /// - K-line (`ChannelProtocol::is_kwp_family`): the full
    ///   `Max(CP_P3Min, CP_RC2xRequestTime)` floor from step 2 applies — an
    ///   explicit `CP_RC2xRequestTime = 0` (the ISO 14230-3 "re-request
    ///   after P3Min" case) resolves to `CP_P3Min` itself.
    /// - J1850 (`ChannelProtocol::is_j1850_family`): `CP_P3Min` has no
    ///   defined value at all (Table B.10/B.19, J2534-1 v04.04 scope it to
    ///   ISO 9141/ISO 14230 only) — an explicit `CP_RC2xRequestTime` is
    ///   used verbatim, including `0`, with no floor term.
    /// - CAN is in neither branch: this codebase's CAN presets seed
    ///   `CP_RC23RequestTime = 0` as their OWN default with
    ///   `CP_RC23Handling` runtime-enable-able, so presence there doesn't
    ///   imply the client explicitly chose `0` the way it does on J1850 —
    ///   CAN keeps the plain 25 ms fallback unconditionally.
    ///
    /// Neither rule is re-derived at the `events.rs` call site; the
    /// resolved value is already final by the time it lands
    /// here.
    pub(super) rc21_handling: bool,
    pub(super) rc21_completion_timeout_ms: u32,
    pub(super) rc21_request_time_ms: u32,

    /// Same as RC21 but for NRC 0x23 (RequestSequenceError / ConditionsNotCorrect).
    pub(super) rc23_handling: bool,
    pub(super) rc23_completion_timeout_ms: u32,
    pub(super) rc23_request_time_ms: u32,

    /// First post-header byte of the COP's own original request (i.e.
    /// `cop_data[0]` -- `cop_data` is payload-only per ADR-050, so no
    /// header-stripping is needed here). Feeds `detect_pending_rc`'s
    /// request-SID gate (ADR-100 Decision §3, resolved (b)). `from_params`
    /// alone never sets this -- it has no access to the COP's request
    /// payload, only its ComParams -- so callers attach it separately via
    /// `with_request_sid`. `None` when the request was empty or the config
    /// was never attached to a specific COP (e.g. the unit tests below).
    pub(super) request_sid: Option<u8>,
}

impl RcHandlingConfig {
    pub(super) fn from_params(params: &ComParamSet, protocol: ChannelProtocol) -> Self {
        let get = |id, default: u32| params.unum32.get(&id).copied().unwrap_or(default);
        // RC-family completion timeouts and request-times are D-PDU timing
        // ComParams stored in microseconds, like CP_P2Min/CP_P2Max
        // (`p2_max_timeout_ms`) -- 0/unset falls back to `default_ms`
        // (already in ms) rather than being treated as a real zero-length
        // wait (ADR-057; this conversion was previously missing entirely,
        // so a 25_000_000 us preset value was read as 25_000_000 ms).
        let get_us_as_ms = |id, default_ms: u32| {
            params
                .unum32
                .get(&id)
                .copied()
                .filter(|&v| v > 0)
                .map(|us| us.div_ceil(1000).max(1))
                .unwrap_or(default_ms)
        };
        // Annex I.1.4.3 steps 1-3 (ISO 14230-3/SAE J1850 VPW K-line RC21/RC23
        // handling): an explicit `CP_RC2xRequestTime = 0` is not "unset" --
        // it is a real configured value -- so it must not be coerced to a
        // made-up 25 ms fallback constant (ADR-125; the prior
        // `get_us_as_ms(id, 25)` treated 0 as unset and silently substituted
        // a 25 ms literal with no spec basis instead).
        //
        // Two DIFFERENT rules apply depending on protocol family
        // (design-advisor consult, Codex review PR #136 round 5, after an
        // edge-case-hunter adversarial pass flagged the K-line-only gate as
        // excluding J1850 despite Annex I.1.4's own title covering "SAE
        // J1850 VPW and ISO 14230 protocols"):
        //
        // - K-line (`is_kwp_family`): the full `Max(CP_P3Min, CP_RC2xRequestTime)`
        //   floor from step 2 (the larger of the two values wins) applies --
        //   `CP_P3Min` is itself only a K-line ComParam (Table B.10/B.19,
        //   J2534-1 v04.04 scopes `P3_MIN` to the ISO 9141 and ISO 14230
        //   protocol IDs), so a configured non-zero request time below `CP_P3Min`
        //   is floored up to it, and an absent/explicit-`0` request time
        //   resolves to `CP_P3Min` itself.
        // - J1850 (`is_j1850_family`): `CP_P3Min` has NO defined value here
        //   at all -- Table B.10's SAE_J1850_VPW column is blank, Table
        //   B.19 lists `CP_P3Min` only for the ISO9141/ISO14230 family, and
        //   step 1/3's "waits CP_P3Min"/"re-request ... when CP_P3Min has
        //   expired" language appears only in their ISO-14230-3 bullets,
        //   never the SAE-J1850-VPW ones. So there is no floor to apply --
        //   an explicit `CP_RC2xRequestTime` (including `0`) is used
        //   VERBATIM, never floored against (or coerced by) anything. This
        //   is numerically `Max(0, request_time) = request_time` if you
        //   like, but conceptually there is no P3Min term on this branch at
        //   all -- `p3_min_ms` below is force-zeroed for non-K-line
        //   protocols specifically so a J1850 CLL can never pick up an
        //   inert, client-invisible `CP_P3Min` value the same way round 3
        //   found on a CAN-family resource (`ISO_14230_3_on_ISO_15765_2`,
        //   0x0204, seeds a literal `CP_P3Min = 55_000` despite being
        //   CAN-family; `comparam_support.rs`'s `is_can_param` rejects
        //   `SetComParam`/`GetComParam` for it there, so the value is inert
        //   and client-invisible -- the same reasoning rules out ever
        //   reading it for J1850, which likewise never allowlists it).
        //
        // CAN is deliberately NOT included in either branch: unlike J1850,
        // this codebase's CAN presets (`iso15765_4_common`,
        // `iso_15765_3_on_iso_15765_2`) seed `CP_RC23RequestTime = 0` as
        // their OWN Table-B.19-driven default with `CP_RC23Handling`
        // runtime-enable-able (round 1's regression) -- so on CAN,
        // "present" does not imply "the client explicitly chose 0". J1850's
        // presets (`j1850_common` and its callers, `comparam_defaults.rs`)
        // now likewise seed `CP_RC21RequestTime`/`CP_RC23RequestTime = 0` as
        // their OWN genuine Table-B.19 default, so presence-of-0 doesn't
        // signal client intent there either -- but that parallel doesn't
        // change the CAN-exclusion reasoning above: CAN is excluded from
        // the K-line/J1850 branches on protocol family alone
        // (`is_kwp_family`/`is_j1850_family`), independent of whatever value
        // either family's presets happen to seed. A CAN CLL therefore still
        // needs BOTH the protocol AND request-time-presence gates to fail,
        // keeping the plain 25 ms fallback exactly as it did pre-ADR-125.
        //
        // Every branch here ALSO requires `CP_RC2xRequestTime` itself to be
        // present (Codex review, PR #136, round 4): a legacy/raw CLL
        // created via a bare numeric `resource_id`/`protocol_id` matching
        // no resources-table row (`rpc_create_com_logical_link`'s
        // `resource_row = None` fallback) gets an entirely empty
        // `ComParamSet` -- no preset ever ran, so `CP_RC2xRequestTime` was
        // never seeded at all, unlike every real preset in this codebase
        // (which always seeds it explicitly, 0 or otherwise, per Table
        // B.19). Without this gate, protocol family alone would still read
        // the truly-absent request time as an explicit `0` and (on
        // K-line) apply `Max(0, 0) = 0` -- an unintended immediate retry
        // where the pre-ADR-125 25 ms fallback (correct for a genuinely
        // *unconfigured* CLL, as opposed to one explicitly configured to
        // `0`) must still apply. `PARAM_RC21_REQUEST_TIME`/
        // `PARAM_RC23_REQUEST_TIME` are allowed on CAN, KWP, and both J1850
        // arms (`is_can_param`/`is_kwp_param`/`is_j1850pwm_param`/
        // `is_j1850vpw_param` in `comparam_support.rs` all include them --
        // only `is_sci_param` omits them, and SCI is excluded by every
        // branch here regardless), so presence has none of round 3's
        // per-protocol-inaccessible-value trap -- unlike `CP_P3Min`, no
        // protocol this gate can ever be true for makes this key's
        // presence a false signal.
        //
        // Round 2 (same PR) caught a `p3_min_ms > 0` value-based gate
        // conflating "absent" with "present and explicitly `0`" (a K-line
        // client legally `SetComParam(CP_P3Min, 0)`ing); round 1 caught an
        // entirely ungated version silently dropping the CAN-side 25 ms
        // fallback to an immediate 0 ms retry for `iso15765_4_common`/
        // `iso_15765_3_on_iso_15765_2` clients that runtime-enable
        // `CP_RC23Handling` with the preset's `CP_RC23RequestTime = 0`.
        let is_kwp = protocol.is_kwp_family();
        let is_j1850 = protocol.is_j1850_family();
        // `CP_P3Min` only has a defined value on K-line -- force it to `0`
        // (a no-op floor) everywhere else, rather than reading whatever
        // (possibly inert) value happens to be in the raw map.
        let p3_min_ms = if is_kwp {
            params
                .unum32
                .get(&ComParamId(j2534_0404::P3_MIN))
                .copied()
                .unwrap_or(0)
                .div_ceil(1000)
        } else {
            0
        };
        let rc21_request_time_configured =
            (is_kwp || is_j1850) && params.unum32.contains_key(&PARAM_RC21_REQUEST_TIME);
        let rc23_request_time_configured =
            (is_kwp || is_j1850) && params.unum32.contains_key(&PARAM_RC23_REQUEST_TIME);
        Self {
            rc_byte_offset: get(PARAM_RC_BYTE_OFFSET, 0) as usize,
            rc78_handling: get(PARAM_RC78_HANDLING, 0) != 0,
            // ADR-056: CP_P2Star, not CP_RC78CompletionTimeout, is the
            // authoritative source (see this struct's field doc comment).
            rc78_p2_star_ms: get_us_as_ms(PARAM_P2_STAR, 5000),
            // ADR-102: CP_RC78CompletionTimeout is reinstated as an
            // independent total-duration ceiling, distinct from CP_P2Star's
            // per-occurrence reload. 0/unset means no ceiling.
            rc78_total_ceiling_ms: params
                .unum32
                .get(&PARAM_RC78_COMPLETION_TIMEOUT)
                .copied()
                .filter(|&v| v > 0)
                .map(|us| us.div_ceil(1000).max(1)),
            rc21_handling: get(PARAM_RC21_HANDLING, 0) != 0,
            rc21_completion_timeout_ms: get_us_as_ms(PARAM_RC21_COMPLETION_TIMEOUT, 5000),
            rc21_request_time_ms: if rc21_request_time_configured {
                get_us_as_ms(PARAM_RC21_REQUEST_TIME, 0).max(p3_min_ms)
            } else {
                get_us_as_ms(PARAM_RC21_REQUEST_TIME, 25)
            },
            rc23_handling: get(PARAM_RC23_HANDLING, 0) != 0,
            rc23_completion_timeout_ms: get_us_as_ms(PARAM_RC23_COMPLETION_TIMEOUT, 5000),
            rc23_request_time_ms: if rc23_request_time_configured {
                get_us_as_ms(PARAM_RC23_REQUEST_TIME, 0).max(p3_min_ms)
            } else {
                get_us_as_ms(PARAM_RC23_REQUEST_TIME, 25)
            },
            request_sid: None,
        }
    }

    /// Attaches the COP's own request SID to an already-built config
    /// (ADR-100 Decision §3, resolved (b)) -- see `request_sid`'s field doc
    /// comment for why this is a separate step from `from_params`.
    pub(super) fn with_request_sid(mut self, request_sid: Option<u8>) -> Self {
        self.request_sid = request_sid;
        self
    }

    pub(super) fn any_enabled(&self) -> bool {
        self.rc78_handling || self.rc21_handling || self.rc23_handling
    }

    /// Returns `Some(rc_code)` when `data` is a pending negative response
    /// (`0x7F <SID> <rc_code>`, `rc_code` at `rc_byte_offset`) whose
    /// auto-handling is enabled; returns `None` otherwise.
    ///
    /// ADR-100 Decision §3, resolved (b): for the standard UDS/KWP
    /// negative-response shape (`rc_byte_offset >= 2`), a `0x78`/`0x21`/
    /// `0x23`-valued byte at the offset alone is not enough evidence -- a
    /// genuine *positive* response can coincidentally carry that value there
    /// (e.g. `62 F1 90 78 ...` under UDS's offset 2, a latent, pre-existing
    /// misdetection this gate also fixes as a side effect), and a genuine
    /// `7F <SID> <rc>` for a *different* SID (e.g. another COP's request)
    /// must not be claimed by this COP either. The frame must additionally
    /// start with `0x7F`, and -- when `request_sid` is known -- its second
    /// byte must echo it. `rc_byte_offset < 2` framings (the non-UDS/KWP
    /// protocols the ComParam exists to serve) are deliberately left
    /// unchanged: this service does not know their negative-response shape
    /// well enough to guess an equivalent gate, and this remains an
    /// accepted residual (ADR-100 Consequences).
    ///
    /// `raw_prefix` (ADR-196 Decision item 3b): the per-frame raw CAN-ID
    /// prefix width (`events::header_footer_len`'s own already-audited
    /// computation, reused rather than re-derived) that precedes the actual
    /// UDS/KWP payload in `data` for a RawMode=ON CLL -- `0` unconditionally
    /// for RawMode=OFF. `rc_byte_offset` itself stays interpreted relative to
    /// `data` exactly as delivered (raw-relative on a RawMode CLL, matching
    /// how expected-response mask/pattern matching already treats it) -- only
    /// this method's OWN internal `0x7F`/SID-echo anchors move, from `0`/`1`
    /// to `raw_prefix`/`raw_prefix + 1`, and the `rc_byte_offset >= 2` gate
    /// becomes `logical_offset >= 2`, where `logical_offset` is
    /// `rc_byte_offset` re-based by `raw_prefix`. An offset that falls
    /// *inside* the raw prefix itself (`rc_byte_offset < raw_prefix`, i.e.
    /// `checked_sub` underflows) is not a real RC-byte position at all and
    /// declines to detect, matching this mechanism's own "decline rather than
    /// risk a false positive" philosophy.
    pub(super) fn detect_pending_rc(&self, data: &[u8], raw_prefix: usize) -> Option<u8> {
        if !self.any_enabled() {
            return None;
        }
        let rc = *data.get(self.rc_byte_offset)?;
        let logical_offset = self.rc_byte_offset.checked_sub(raw_prefix)?;
        if logical_offset >= 2
            && (data.get(raw_prefix) != Some(&0x7F)
                || self
                    .request_sid
                    .is_some_and(|sid| data.get(raw_prefix + 1) != Some(&sid)))
        {
            return None;
        }
        match rc {
            0x78 if self.rc78_handling => Some(0x78),
            0x21 if self.rc21_handling => Some(0x21),
            0x23 if self.rc23_handling => Some(0x23),
            _ => None,
        }
    }

    /// `CP_SuspendQueueOnError` classification (ADR-147): `true` when `data`
    /// is a negative response (`0x7F <SID> <rc_code>`) at `rc_byte_offset`
    /// whose NRC is NOT claimable by `detect_pending_rc` -- either because
    /// the code isn't one of the RC21/RC23/RC78-mapped codes at all, or
    /// because it is but the matching `CP_RCxxHandling` is currently
    /// disabled on this frozen snapshot. Deliberately does its own
    /// shape/NRC check rather than delegating to `detect_pending_rc`: that
    /// method early-returns `None` via `any_enabled()` whenever the whole RC
    /// engine is disabled, which would make every negative response here
    /// look indistinguishable from "not a negative response at all" instead
    /// of "unhandled negative response" -- exactly the case this method
    /// exists to catch.
    ///
    /// Only classifies for the standard UDS/KWP negative-response shape
    /// (`rc_byte_offset >= 2`), matching `detect_pending_rc`'s own
    /// precondition and using the same SID-echo gate. `rc_byte_offset < 2`
    /// framings decline to classify (return `false`) -- an accepted
    /// residual; `CP_SuspendQueueOnError`'s timeout hook still covers those
    /// protocols. Judged purely against this frozen `RcHandlingConfig`
    /// snapshot (ADR-067 call-time binding), never against live ComParam
    /// values.
    ///
    /// `raw_prefix` (ADR-196 Decision item 3b): same meaning, and the same
    /// `rc_byte_offset.checked_sub(raw_prefix)` re-basing, as
    /// `detect_pending_rc`'s own `raw_prefix` parameter above -- see its doc
    /// comment for the full reasoning. `rc_byte_offset < raw_prefix`
    /// (`checked_sub` underflow) declines to classify (`false`), same as the
    /// existing `rc_byte_offset < 2` decline.
    pub(super) fn is_unhandled_negative(&self, data: &[u8], raw_prefix: usize) -> bool {
        let Some(logical_offset) = self.rc_byte_offset.checked_sub(raw_prefix) else {
            return false;
        };
        if logical_offset < 2 {
            return false;
        }
        let Some(&rc) = data.get(self.rc_byte_offset) else {
            return false;
        };
        if data.get(raw_prefix) != Some(&0x7F)
            || self
                .request_sid
                .is_some_and(|sid| data.get(raw_prefix + 1) != Some(&sid))
        {
            return false;
        }
        !matches!(
            (
                rc,
                self.rc78_handling,
                self.rc21_handling,
                self.rc23_handling
            ),
            (0x78, true, _, _) | (0x21, _, true, _) | (0x23, _, _, true)
        )
    }
}

/// Per-COP configuration for `CP_ModifyTiming`'s live-exchange mechanism: a
/// KWP variant observing the ISO 14230-2 SID 0x83/0xC3 Access Timing
/// Parameter service (ADR-146), and a UDS variant observing ISO
/// 15765-3/14229-3 SID 0x10/0x50 DiagnosticSessionControl on ISO15765
/// (ADR-150). A sibling of [`RcHandlingConfig`] rather than a field on it --
/// an unrelated concern -- following the same construction pattern:
/// `from_params` builds the call-time-bound (ADR-067) config from the COP's
/// own `ComParamSet` snapshot and dispatches on the channel's protocol, and
/// `with_request` attaches the request's own payload bytes separately (the
/// COP's ComParams alone don't carry its request).
///
/// `None` (not just an inert default variant) is returned by `from_params`
/// whenever neither mechanism can possibly apply, so a `CopRegistrant`
/// carries `Option<TimingChangeConfig>` exactly like `rc_cfg`: `Some` only
/// for a tier-1 registrant on an ISO14230 or ISO15765 channel with
/// `CP_ModifyTiming` enabled at COP-creation time. Everything downstream
/// (pairing in `events::observe_registrant_timing_change`, storage,
/// derivation) dispatches on this enum's variant; the seven Codex-round
/// fixes ADR-146 already accumulated for the shared pairing/storage
/// machinery are reused unchanged by both variants.
#[derive(Debug, Clone)]
pub(super) enum TimingChangeConfig {
    KwpAccess(AccessTimingConfig),
    UdsSession(SessionTimingConfig),
}

impl TimingChangeConfig {
    /// `None` unless `protocol.kwp_access_timing_applies()` (KWP Access
    /// Timing, ADR-146) or `protocol.uds_session_timing_applies()` (UDS
    /// Session Timing, ADR-150) -- see those predicates' own doc comments
    /// in `protocol.rs` for the exact, ISO-22900-2-Table-B.10-derived match
    /// sets -- AND `CP_ModifyTiming` is enabled in `params`.
    ///
    /// `protocol` must be the CLL's LOGICAL/service-level protocol
    /// (`LogicalLinkState::protocol`), not its hardware channel protocol
    /// (`LogicalLinkState::hw_protocol_id`) -- Codex review, PR #22 round 1:
    /// in `software-isotp` mode an ISO15765-family CLL's `hw_protocol_id`
    /// is `CAN` (ADR-046, a raw CAN channel carries the service's own
    /// ISO-TP segmentation), so gating on it would leave `timing_cfg`
    /// permanently `None` in that supported mode.
    ///
    /// Round 3 correction: an intermediate version of this fix (rounds 1-2)
    /// matched via `protocol.j2534_protocol_id()` -- exact HARDWARE channel
    /// family, deliberately widened from exact `ChannelProtocol` identity
    /// to also catch extended service-level resource rows like
    /// `ISO_14229_3_ON_ISO_15765_2`. That went too far the other way: it
    /// collapsed every extended protocol sharing an ISO15765/ISO14230
    /// hardware channel into one bucket regardless of which diagnostic
    /// SERVICES layer actually runs on it -- `ISO_14230_3_ON_ISO_15765_2`
    /// (KWP2000 services over CAN transport), `SAE_J2190_ON_ISO_15765_2`,
    /// and `ISO_15031_5_ON_ISO_15765_4` (OBD services) all share the
    /// ISO15765 hardware channel with genuine UDS variants but do not run
    /// UDS DiagnosticSessionControl, so a coincidentally SID-0x10/0x50-shaped
    /// exchange on one of them would have been misinterpreted. The two
    /// `..._applies()` predicates below replace the hardware-family check
    /// with an exhaustively-enumerated, spec-derived match set instead.
    pub(super) fn from_params(params: &ComParamSet, protocol: ChannelProtocol) -> Option<Self> {
        let enabled = params
            .unum32
            .get(&PARAM_MODIFY_TIMING)
            .copied()
            .unwrap_or(0)
            != 0;
        if !enabled {
            return None;
        }
        if protocol.kwp_access_timing_applies() {
            Some(Self::KwpAccess(AccessTimingConfig {
                tpi: None,
                tpi3_request_bytes: None,
                default_timing: access_timing_structfield_entry(params, PARAM_ACCESS_TIMING_ECU, 1),
                override_timing: access_timing_structfield_entry(
                    params,
                    PARAM_ACCESS_TIMING_OVERRIDE,
                    2,
                ),
                functional: tx_header::use_functional_addressing(
                    tx_header::AddrModeSource::Request,
                    params,
                ),
            }))
        } else if protocol.uds_session_timing_applies() {
            Some(Self::UdsSession(SessionTimingConfig {
                session: None,
                override_entries: session_timing_structfield_entries(
                    params,
                    PARAM_SESSION_TIMING_OVERRIDE,
                ),
                can_transmission_time_us: params
                    .unum32
                    .get(&PARAM_CAN_TRANSMISSION_TIME)
                    .copied()
                    .unwrap_or(0),
                functional: tx_header::use_functional_addressing(
                    tx_header::AddrModeSource::Request,
                    params,
                ),
            }))
        } else {
            None
        }
    }

    /// Attaches the COP's own request payload (`cop_data`, payload-only per
    /// ADR-050) -- dispatches to each variant's own `with_request`, which
    /// populates that variant's request-derived fields only for its own
    /// genuine request SID, leaving them `None`/unset for any other request
    /// so this COP's eventual response can never be mistaken for a
    /// qualifying one.
    ///
    /// `tx_prefix` (ADR-196 Decision item 3b) is forwarded to BOTH variants'
    /// own `with_request` -- `UdsSession`'s always did (see its doc
    /// comment); `KwpAccess`'s now also does (ADR-198 Phase 2), since
    /// hardware K-line RawMode is no longer rejected at
    /// `CreateComLogicalLink` (ADR-198 extended Decision item 1's protocol
    /// allowlist to ISO9141/ISO14230) -- this call site is now reachable
    /// for a RawMode K-line CLL, and `tx_prefix` re-bases the SID 0x83
    /// anchor the same way it already re-bases `UdsSession`'s SID 0x10
    /// anchor. `0` for RawMode=OFF leaves this byte-for-byte the pre-ADR-196
    /// behavior for both variants, unchanged.
    pub(super) fn with_request(self, cop_data: &[u8], tx_prefix: usize) -> Self {
        match self {
            Self::KwpAccess(cfg) => Self::KwpAccess(cfg.with_request(cop_data, tx_prefix)),
            Self::UdsSession(cfg) => Self::UdsSession(cfg.with_request(cop_data, tx_prefix)),
        }
    }

    /// `true` iff this COP's request used functional (broadcast) addressing
    /// -- both variants capture this identically at construction time (see
    /// each variant's own `functional` field doc comment); `events::
    /// observe_registrant_timing_change` reads this without needing to match
    /// on the variant itself.
    pub(super) fn functional(&self) -> bool {
        match self {
            Self::KwpAccess(cfg) => cfg.functional,
            Self::UdsSession(cfg) => cfg.functional,
        }
    }
}

/// ADR-146: KWP (ISO 14230-2 SID 0x83/0xC3) Access Timing Parameter
/// live-exchange config -- the original `TimingChangeConfig`'s own body,
/// renamed and wrapped in `TimingChangeConfig::KwpAccess` when ADR-150 added
/// a UDS sibling variant.
#[derive(Debug, Clone, Default)]
pub(super) struct AccessTimingConfig {
    /// TPI byte (second payload byte) of this COP's own SID 0x83 request,
    /// attached by `with_request`. `None` until attached, or when the
    /// request captured by `with_request` wasn't a genuine SID 0x83 Access
    /// Timing Parameter request at all.
    pub(super) tpi: Option<u8>,
    /// TPI=3 (set given values) only: the 5 timing bytes accompanying the
    /// request, captured from the payload-only request bytes (ADR-050) --
    /// ISO 14230-2's positive response to TPI=3 carries no payload of its
    /// own, so these are the only source for the adopted values (ADR-146
    /// Decision, "Per-TPI behavior").
    pub(super) tpi3_request_bytes: Option<[u8; 5]>,
    /// `CP_AccessTiming_Ecu`'s existing `TimingSet=1` entry, if any, read
    /// from the SAME bound `ComParamSet` snapshot as everything else here
    /// (ADR-067 call-time binding, not a live re-read) -- feeds TPI=1's
    /// "reapply the ECU's own default set" behavior. `None` means TPI=1 is a
    /// documented no-op (ADR-146 Consequences: "this service cannot
    /// originate ISO 14230-2's own default timing constants ... and does
    /// not invent them").
    pub(super) default_timing: Option<[u8; 5]>,
    /// `CP_AccessTimingOverride`'s `TimingSet=2` entry, if non-empty, read
    /// from the same bound snapshot -- redirects which bytes TPI=2's
    /// *derived* engineering ComParams are computed from, without changing
    /// what gets recorded into `CP_AccessTiming_Ecu` itself (ADR-146
    /// Decision, TPI=2 bullet).
    pub(super) override_timing: Option<[u8; 5]>,
    /// `true` iff this COP's request used functional (broadcast) addressing
    /// (`CP_RequestAddrMode == 2`), read from the same bound snapshot via
    /// `tx_header::use_functional_addressing`. ADR-146's worst-case
    /// combination across multiple responses only applies to a functionally
    /// addressed exchange, where more than one ECU can legitimately answer
    /// the same request -- a physically addressed COP has exactly one
    /// responder, so each TPI=2 response's own bytes are that ECU's current
    /// values, not one input to a running worst-case fold across a set of
    /// distinct ECUs (Codex review, PR #17, round 3).
    pub(super) functional: bool,
}

impl AccessTimingConfig {
    /// Populates `tpi`/`tpi3_request_bytes` only when `cop_data` is a
    /// genuine SID 0x83 Access Timing Parameter request; a no-op (leaving
    /// both `None`) for any other request.
    ///
    /// `tx_prefix` (ADR-198 Phase 2, extending ADR-196 Decision item 3b's
    /// shared helper to KWP): the raw CAN-ID/header prefix width preceding
    /// SID 0x83's own request bytes in `cop_data` -- `0` for RawMode=OFF
    /// unconditionally (leaves this byte-for-byte the pre-ADR-198 behavior),
    /// or the client's own KWP header length (`rpc_primitive::
    /// compute_tx_prefix`'s K-line arm, reusing `events::
    /// kwp_header_and_payload_len`) for a RawMode=ON K-line CLL. Mirrors
    /// `SessionTimingConfig::with_request`'s identical `tx_prefix` parameter
    /// exactly.
    pub(super) fn with_request(mut self, cop_data: &[u8], tx_prefix: usize) -> Self {
        if cop_data.get(tx_prefix) != Some(&0x83) {
            return self;
        }
        self.tpi = cop_data.get(tx_prefix + 1).copied();
        if self.tpi == Some(3)
            && let Some(bytes) = cop_data.get(tx_prefix + 2..tx_prefix + 7)
        {
            let mut tpi3 = [0u8; 5];
            tpi3.copy_from_slice(bytes);
            self.tpi3_request_bytes = Some(tpi3);
        }
        self
    }
}

/// ADR-150: UDS (ISO 15765-3/14229-3 SID 0x10/0x50) DiagnosticSessionControl
/// live-exchange config on ISO15765 -- `CP_ModifyTiming`'s CAN/UDS sibling
/// mechanism to ADR-146's KWP one, deriving `CP_P2Max`/`CP_P2Star` and
/// recording `CP_SessionTiming_Ecu`, optionally redirected by
/// `CP_SessionTimingOverride`.
#[derive(Debug, Clone, Default)]
pub(super) struct SessionTimingConfig {
    /// The captured request's own session type (`cop_data[1] & 0x7F`,
    /// masking off the suppressPosRspMsgIndicationBit), attached by
    /// `with_request`. `None` until attached, when the request captured by
    /// `with_request` wasn't a genuine SID 0x10 DiagnosticSessionControl
    /// request at all, or when the masked value is `0` (ISO 22900-2
    /// §B.3.3.2.2: the session field's valid range is [1;127] -- a masked
    /// `0` is rejected rather than adopted).
    pub(super) session: Option<u8>,
    /// `CP_SessionTimingOverride`'s entries, if any, read from the SAME
    /// bound `ComParamSet` snapshot as everything else here (ADR-067) --
    /// `(session, p2_max_ms, p2_star_10ms)` triples, redirecting which
    /// values the *derived* engineering ComParams are computed from for a
    /// matching echoed session, without changing what gets recorded into
    /// `CP_SessionTiming_Ecu` itself (mirrors `CP_AccessTimingOverride`'s
    /// own redirect-derivation-only semantics, ADR-146).
    pub(super) override_entries: Vec<(u16, u16, u16)>,
    /// `CP_CanTransmissionTime` (µs), added onto both derived
    /// `CP_P2Max`/`CP_P2Star` ComParams (`events::session_timing_to_comparams`)
    /// -- `0` when absent from the bound set.
    pub(super) can_transmission_time_us: u32,
    /// `true` iff this COP's request used functional (broadcast) addressing
    /// -- same capture as `AccessTimingConfig::functional`, same worst-case
    /// combination semantics (ADR-146 Decision, "Functional-addressing
    /// worst case") applied to this variant's own P2Max/P2Star pair instead
    /// of the KWP quintuple -- both are client-side timeout ceilings with no
    /// P2Min-analog direction, so both fold toward the maximum.
    pub(super) functional: bool,
}

impl SessionTimingConfig {
    /// Populates `session` only when `cop_data` is a genuine SID 0x10
    /// DiagnosticSessionControl request; a no-op (leaving `session: None`)
    /// for any other request.
    ///
    /// `tx_prefix` (ADR-196 Decision item 3b): the raw CAN-ID prefix width
    /// (`0` for RawMode=OFF unconditionally, `4`/`5` for RawMode=ON,
    /// depending on whether the RESOLVED TX flags this send actually used
    /// include `TX_FLAG_ISO15765_ADDR_TYPE` -- client-authoritative under
    /// RawMode per Decision item 2) preceding SID 0x10's own request bytes
    /// in `cop_data` -- computed by the ONE shared helper
    /// (`rpc_primitive::compute_tx_prefix`) every capture site derived from
    /// this same resolved-TX-flags value uses, so they cannot drift apart.
    /// `0` for RawMode=OFF leaves this byte-for-byte the pre-ADR-196
    /// behavior.
    pub(super) fn with_request(mut self, cop_data: &[u8], tx_prefix: usize) -> Self {
        if cop_data.get(tx_prefix) != Some(&0x10) {
            return self;
        }
        self.session = cop_data
            .get(tx_prefix + 1)
            .map(|&b| b & 0x7F)
            .filter(|&s| s != 0);
        self
    }
}

/// Reads `id`'s `TimingSet == timing_set` entry out of `params`' Structfield
/// map, if present -- shared by `TimingChangeConfig::from_params`'s
/// `default_timing`/`override_timing` reads (ADR-146).
fn access_timing_structfield_entry(
    params: &ComParamSet,
    id: ComParamId,
    timing_set: u32,
) -> Option<[u8; 5]> {
    let sf = params.structfield.get(&id)?;
    let vci_service_interface::param_structfield::Data::AccessTiming(list) = sf.data.as_ref()?
    else {
        return None;
    };
    let entry = list.entries.iter().find(|e| e.timing_set == timing_set)?;
    Some([
        entry.p2_min as u8,
        entry.p2_max as u8,
        entry.p3_min as u8,
        entry.p3_max as u8,
        entry.p4_min as u8,
    ])
}

/// Reads every entry out of `id`'s SessionTiming-shaped Structfield, if
/// present -- feeds `SessionTimingConfig::from_params`'s `override_entries`
/// (ADR-150). Unlike `access_timing_structfield_entry` (a single
/// `TimingSet`-keyed lookup), this reads the WHOLE list: the matching
/// override entry, if any, isn't known until the response's own echoed
/// session type is observed (`events::observe_session_timing_response`).
fn session_timing_structfield_entries(
    params: &ComParamSet,
    id: ComParamId,
) -> Vec<(u16, u16, u16)> {
    let Some(sf) = params.structfield.get(&id) else {
        return Vec::new();
    };
    let Some(vci_service_interface::param_structfield::Data::SessionTiming(list)) =
        sf.data.as_ref()
    else {
        return Vec::new();
    };
    list.entries
        .iter()
        .map(|e| (e.session as u16, e.p2_max as u16, e.p2_star as u16))
        .collect()
}

/// One qualifying ADR-146/ADR-150 response's effect, computed by
/// `events::observe_timing_response`/`events::observe_session_timing_response`
/// and stashed on the paired `CopRegistrant` for the poll task to apply to
/// hardware once the attribution pass's borrow of the registrant list has
/// ended (ADR-146 Decision, "Hardware application and locking").
#[derive(Debug, Clone)]
pub(super) struct PendingTimingChange {
    /// Derived engineering ComParam `(id, value-in-microseconds)` pairs to
    /// push to hardware and store into the CLL's ComParam sets.
    pub(super) derived: Vec<(ComParamId, u32)>,
    /// The new ECU-side timing entry to store -- `None` for a KWP TPI=1
    /// reapply, which only re-uses an already-stored entry and writes
    /// nothing new (ADR-146).
    pub(super) ecu_entry: Option<EcuTimingRecord>,
}

/// One ADR-146/ADR-150 qualifying exchange's own "what to record into the
/// ECU-side Structfield ComParam" side, replacing the pre-ADR-150
/// `(timing_set, raw_wire_bytes)` tuple `PendingTimingChange::ecu_entry`
/// carried when only the KWP mechanism existed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum EcuTimingRecord {
    /// KWP: `CP_AccessTiming_Ecu`'s new/reapplied entry, keyed by
    /// `TimingSet` (ADR-146).
    AccessTiming { timing_set: u32, bytes: [u8; 5] },
    /// UDS: `CP_SessionTiming_Ecu`'s new entry, keyed by `session` (ADR-150)
    /// -- always the COMBINED (post-functional-fold) observed values, never
    /// a single response's own raw values (mirrors ADR-146's own "combined,
    /// not single-response" fix for `CP_AccessTiming_Ecu`).
    SessionTiming {
        session: u16,
        p2_max_ms: u16,
        p2_star_10ms: u16,
    },
}

/// Running worst-case functional-addressing accumulator (ADR-146 Decision,
/// "Functional-addressing worst case"), generalized (ADR-150) beyond
/// ADR-146's original KWP-only `[u8; 5]` shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum TimingAccumulator {
    /// KWP: `[P2Min, P2Max, P3Min, P3Max, P4Min]` raw wire bytes (ADR-146).
    Kwp([u8; 5]),
    /// UDS: `(P2Max_ms, P2Star_10ms)` raw wire values (ADR-150) -- both
    /// client-side timeout ceilings, so both fold toward the maximum (no
    /// P2Min-analog direction exists for this mechanism).
    Session { p2_ms: u16, p2_star_10ms: u16 },
}

/// One entry in a logical link's UniqueRespIdTable.
///
/// Maps a `unique_resp_identifier` (referenced in `ExpectedResponseData.unique_resp_ids`)
/// to the addressing ComParams for that ECU (e.g. CP_CanRespUSDTId, CP_CanPhysReqId).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct EcuUniqueRespEntry {
    pub(super) unique_resp_identifier: u32,
    pub(super) params: ComParamSet,
}

/// Compares two UniqueRespIdTable snapshots for equality, order-insensitively,
/// keyed by `unique_resp_identifier` -- the promotion diff gate ADR-068 uses to
/// skip `FLOW_CONTROL_FILTER` I/O when a `CoptUpdateparam`'s table snapshot is
/// unchanged from the current Active table (e.g. a com-param-only update).
///
/// Falls back to `false` ("different") if either list has a duplicate
/// `unique_resp_identifier` that would collapse entries when building the
/// lookup map -- a merely defensive case, since `SetUniqueRespIdTable` does
/// not itself deduplicate, but not one this comparison should ever silently
/// paper over.
pub(super) fn unique_resp_id_tables_equal(
    a: &[EcuUniqueRespEntry],
    b: &[EcuUniqueRespEntry],
) -> bool {
    fn to_map(entries: &[EcuUniqueRespEntry]) -> HashMap<u32, &ComParamSet> {
        entries
            .iter()
            .map(|e| (e.unique_resp_identifier, &e.params))
            .collect()
    }

    if a.len() != b.len() {
        return false;
    }
    let (map_a, map_b) = (to_map(a), to_map(b));
    if map_a.len() != a.len() || map_b.len() != b.len() {
        return false;
    }
    map_a
        .iter()
        .all(|(id, params)| map_b.get(id) == Some(params))
}

impl ComParamSet {
    pub(super) fn baud_rate(&self) -> u32 {
        self.unum32
            .get(&ComParamId(j2534_0404::DATA_RATE))
            .copied()
            .unwrap_or(0)
    }

    pub(super) fn tester_present_data(&self) -> &[u8] {
        self.bytes
            .get(&PARAM_TESTER_PRESENT_MSG)
            .map_or(&[], Vec::as_slice)
    }

    /// `CP_TesterPresentHandling`: the master enable switch for
    /// periodic/idle-triggered tester-present, independent of
    /// `CP_TesterPresentTime`'s own cadence-disable sentinel. `0` = disabled,
    /// `1` = enabled. Defaults to `0`.
    pub(super) fn tester_present_handling(&self) -> u32 {
        self.unum32
            .get(&PARAM_TESTER_PRESENT_HANDLING)
            .copied()
            .unwrap_or(0)
    }

    pub(super) fn tester_present_interval_us(&self) -> u32 {
        self.unum32
            .get(&PARAM_TESTER_PRESENT_INTERVAL_US)
            .copied()
            .unwrap_or(0)
    }

    /// `CP_TesterPresentSendType`: `0` = periodic, `1` = idle-triggered --
    /// both software-driven (ADR-083/this diff: mode 0 was previously
    /// hardware-autonomous via `PassThruStartPeriodicMsg`). Defaults to `0`,
    /// matching most presets.
    pub(super) fn tester_present_send_type(&self) -> u32 {
        self.unum32
            .get(&PARAM_TESTER_PRESENT_SEND_TYPE)
            .copied()
            .unwrap_or(0)
    }

    /// `CP_TesterPresentReqRsp`: `0` = no ECU response is returned for a
    /// tester-present message, `1` = a response is expected and must be
    /// discarded by this module rather than delivered to the client (see
    /// `tester_present_exp_pos_resp`/`tester_present_exp_neg_resp`).
    /// Defaults to `0`.
    pub(super) fn tester_present_req_rsp(&self) -> u32 {
        self.unum32
            .get(&PARAM_TESTER_PRESENT_REQ_RSP)
            .copied()
            .unwrap_or(0)
    }

    /// `CP_SuspendQueueOnError` (ADR-147): `true` when this CLL's TX queue
    /// should suspend on an unhandled negative response/timeout. Read live
    /// from `LogicalLinkState::active` at the moment a suspend-worthy event
    /// is classified/applied (link-scoped policy), never frozen into a COP's
    /// `rc_cfg` snapshot -- unlike this same `ComParamSet`'s RC-handling
    /// fields, which ARE frozen at call time (ADR-067). Defaults to `0`
    /// (disabled).
    pub(super) fn suspend_queue_on_error(&self) -> bool {
        self.unum32
            .get(&PARAM_SUSPEND_QUEUE_ON_ERROR)
            .copied()
            .unwrap_or(0)
            != 0
    }

    /// `CP_StartMsgIndEnable`: `true` when this CLL should surface a "start of
    /// message" indication (first frame of a multi-frame ISO 15765 message, or
    /// first byte of a UART message) as its own RX result item (ADR-151).
    /// Defaults to `0` (disabled) — spec default for every protocol.
    pub(super) fn start_msg_ind_enable(&self) -> bool {
        self.unum32
            .get(&PARAM_START_MSG_IND_ENABLE)
            .copied()
            .unwrap_or(0)
            != 0
    }

    /// `CP_TransmitIndEnable`: `true` when this CLL should surface a
    /// transmit-completion indication as its own RX result item (ADR-151).
    /// Defaults to `0` (disabled) — spec default for every protocol.
    pub(super) fn transmit_ind_enable(&self) -> bool {
        self.unum32
            .get(&PARAM_TRANSMIT_IND_ENABLE)
            .copied()
            .unwrap_or(0)
            != 0
    }

    /// `CP_TesterPresentExpPosResp`: the expected positive-response byte
    /// prefix for a tester-present message (e.g. UDS `0x7E`), used to
    /// recognize -- and discard -- the ECU's reply when
    /// `tester_present_req_rsp() == 1`. Empty means "not configured";
    /// never matches.
    pub(super) fn tester_present_exp_pos_resp(&self) -> &[u8] {
        self.bytes
            .get(&PARAM_TESTER_PRESENT_EXP_POS_RESP)
            .map_or(&[], Vec::as_slice)
    }

    /// `CP_TesterPresentExpNegResp`: the expected negative-response byte
    /// prefix for a tester-present message (e.g. UDS `0x7F 0x3E`), same role
    /// as `tester_present_exp_pos_resp` for the negative-response case.
    pub(super) fn tester_present_exp_neg_resp(&self) -> &[u8] {
        self.bytes
            .get(&PARAM_TESTER_PRESENT_EXP_NEG_RESP)
            .map_or(&[], Vec::as_slice)
    }

    // ── Software ISO-TP parameter snapshots (ADR-046) ─────────────────────────
    //
    // In software-ISO-TP mode the ISO15765 CONFIG IDs are never forwarded to
    // the (raw CAN) hardware channel, so the service reads them from the
    // Active set itself.

    /// Frame padding settings: enabled by `TX_ISO15765_FRAME_PAD` on the COP
    /// or `CP_CANFillerByteHandling = 1`; filler byte from `CP_CANFillerByte`.
    pub(super) fn isotp_framing(&self, tx_flags: u32) -> IsoTpFraming {
        let pad = tx_flags & j2534_0404::TX_ISO15765_FRAME_PAD != 0
            || self
                .unum32
                .get(&PARAM_CAN_FILLER_BYTE_HANDLING)
                .copied()
                .unwrap_or(0)
                == 1;
        let filler = self
            .unum32
            .get(&PARAM_CAN_FILLER_BYTE)
            .copied()
            .unwrap_or(0) as u8;
        IsoTpFraming { pad, filler }
    }

    /// ISO 15765-2 N_Bs (FlowControl wait timeout) in ms, derived from the
    /// Active `CP_N_Bs` (stored in µs per the D-PDU API timing-param
    /// convention, same as `CP_P2Max` -- see `p2_max_timeout_ms`); default
    /// 1000 ms when unset or zero. Unit-conversion bug found during ADR-159's
    /// investigation, fixed here: this used to return the raw µs value as if
    /// already milliseconds, a ~1000x timeout inflation for the
    /// software-ISO-TP emulation path (ADR-046) this feeds.
    pub(super) fn isotp_n_bs_timeout_ms(&self) -> u32 {
        self.unum32
            .get(&PARAM_N_BS)
            .copied()
            .filter(|&v| v > 0)
            .map(|us| us.div_ceil(1000).max(1))
            .unwrap_or(1000)
    }

    /// ISO 15765-2 N_Cr (ConsecutiveFrame wait timeout) in ms, derived from
    /// the Active `CP_Cr` (stored in µs per the D-PDU API timing-param
    /// convention, same as `CP_P2Max` -- see `p2_max_timeout_ms`); default
    /// 1000 ms when unset or zero. Same unit-conversion bug and fix as
    /// `isotp_n_bs_timeout_ms` above.
    pub(super) fn isotp_n_cr_timeout_ms(&self) -> u32 {
        self.unum32
            .get(&PARAM_N_CR)
            .copied()
            .filter(|&v| v > 0)
            .map(|us| us.div_ceil(1000).max(1))
            .unwrap_or(1000)
    }

    /// `CoptSendrecv` response-window timeout in ms, derived from the
    /// Active `CP_P2Max` (stored in µs per the D-PDU API timing-param
    /// convention); default 50 ms when unset or zero (ADR-053).  This is
    /// the window `wait_for_expected_response` allows for each expected
    /// response — `PDU_COP_CTRL_DATA.Time` is the cyclic-send cycle time,
    /// not a response timeout.
    pub(super) fn p2_max_timeout_ms(&self) -> u32 {
        self.unum32
            .get(&ComParamId(j2534_0404::P2_MAX))
            .copied()
            .filter(|&v| v > 0)
            .map(|us| us.div_ceil(1000).max(1))
            .unwrap_or(50)
    }

    /// Response-window timeout in ms for the `SAE_J1850` VPW/PWM auto-detect
    /// probe (ADR-070): the Working `CP_P2Max` when present and nonzero
    /// (converted from µs, same conversion as `p2_max_timeout_ms`), else
    /// [`J1850_AUTODETECT_DEFAULT_WINDOW_MS`]. A dedicated default (rather
    /// than reusing `p2_max_timeout_ms`'s 50 ms) because the probe is a
    /// one-shot bus-flavor decision, not a per-expected-response window, and
    /// needs enough headroom for a real ECU to answer once per candidate
    /// flavor.
    pub(super) fn j1850_autodetect_window_ms(&self) -> u32 {
        self.unum32
            .get(&ComParamId(j2534_0404::P2_MAX))
            .copied()
            .filter(|&v| v > 0)
            .map(|us| us.div_ceil(1000).max(1))
            .unwrap_or(J1850_AUTODETECT_DEFAULT_WINDOW_MS)
    }

    /// `(BlockSize, STmin)` advertised in FlowControl frames this service
    /// sends while receiving a segmented message (`CP_BlockSize` /
    /// `CP_StMin`, native IDs 0x1E / 0x1F); defaults `(0, 0)`.
    pub(super) fn isotp_rx_fc(&self) -> (u8, u8) {
        let bs = self
            .unum32
            .get(&ComParamId(j2534_0404::ISO15765_BS))
            .copied()
            .unwrap_or(0);
        let st_min = self
            .unum32
            .get(&ComParamId(j2534_0404::ISO15765_STMIN))
            .copied()
            .unwrap_or(0);
        (bs.min(0xFF) as u8, st_min.min(0xFF) as u8)
    }

    /// `CP_P3Func` (CAN context, µs like every other D-PDU timing ComParam)
    /// as milliseconds: the minimum gap enforced before the next
    /// functionally-addressed send when either side of the pair required no
    /// response (ADR-060). `0`/absent is a real "no gap required" value,
    /// unlike `p2_max_timeout_ms`'s response-window fallback.
    pub(super) fn p3_func_gap_ms(&self) -> u32 {
        self.unum32
            .get(&PARAM_P3_FUNC)
            .copied()
            .unwrap_or(0)
            .div_ceil(1000)
    }

    /// `CP_P3Phys` (CAN context, µs) as milliseconds: the minimum gap
    /// enforced before the next physically-addressed send when the
    /// previous one required no response (ADR-060).
    pub(super) fn p3_phys_gap_ms(&self) -> u32 {
        self.unum32
            .get(&PARAM_P3_PHYS)
            .copied()
            .unwrap_or(0)
            .div_ceil(1000)
    }

    /// `CP_CyclicRespTimeout` as milliseconds (ADR-100 Decision §4, scope
    /// widened from `-1`-only to also cover finite `N > 0` by ADR-182): the
    /// cyclic-receive-timeout window for a tier-2 (Receive Only) COP created
    /// with `NumReceiveCycles == -1` or a finite `N > 0` (ADR-059's
    /// category; `-2` is unaffected) -- when no
    /// matching response arrives within this many ms of the last one, the
    /// COP transitions to `PDU_COPST_FINISHED` (ISO 22900-2 §9.2.6.3.4
    /// RECEIVE ONLY NOTE 1) -- for the `-1` subtype, a plain, non-error
    /// finish; for the finite-`N` subtype, an expiry with the target count
    /// still unmet additionally raises `PduErrEvtRxTimeout` first (ADR-182;
    /// reaching the count itself, regardless of this timeout, always wins as
    /// a non-error finish). Stored as µs like every other D-PDU timing
    /// ComParam (`p2_max_timeout_ms`'s `get_us_as_ms` conversion; see
    /// `PARAM_CYCLIC_RESP_TIMEOUT`'s own doc comment for the unit-wording
    /// caveat this corrects). `0` (absent or explicit) is a real "disabled,
    /// no cyclic timeout" value, unlike `p2_max_timeout_ms`'s response-window
    /// fallback -- no default substitution here, mirroring
    /// `p3_func_gap_ms`/`p3_phys_gap_ms`'s "0 is a real value" treatment.
    pub(super) fn cyclic_resp_timeout_ms(&self) -> u32 {
        self.unum32
            .get(&PARAM_CYCLIC_RESP_TIMEOUT)
            .copied()
            .unwrap_or(0)
            .div_ceil(1000)
    }

    /// `CP_EnableConcatenation` (D-PDU `PARAM_ENABLE_CONCATENATION` =
    /// 0x807B): `true` when this snapshot has it set to `1`. ISO 22900-2:2022
    /// Table B.11 restricts applicability to the KWP family (ISO 9141-2,
    /// ISO 14230-2/-4) and SAE J1850 VPW/PWM -- this accessor reports only
    /// the raw ComParam value; callers must additionally check the CLL's
    /// protocol (see `events::wait_for_expected_response`'s `concat_enabled`
    /// resolution, which combines this with the protocol check and the
    /// registrant-shape scope restriction) before acting on it.
    pub(super) fn enable_concatenation(&self) -> bool {
        self.unum32.get(&PARAM_ENABLE_CONCATENATION).copied() == Some(1)
    }

    /// `TX_FLAG_SCI_MODE`/`TX_FLAG_SCI_TX_VOLTAGE` implied by `CP_SCITransmitMode`
    /// (nonzero = half-duplex, matching the TxFlags bit's own 0/1 meaning) and
    /// `CP_SCISetProgVoltage` (any value other than the `0xFFFF_FFFF`
    /// "no override" default means apply the 20V programming voltage after
    /// transmit) (ADR-062). `0` for every non-SCI protocol, since both
    /// ComParams stay at their seeded defaults there.
    pub(super) fn sci_tx_flags(&self) -> u32 {
        let mut flags = 0;
        if self
            .unum32
            .get(&PARAM_SCI_TRANSMIT_MODE)
            .copied()
            .unwrap_or(0)
            != 0
        {
            flags |= j2534_0404::SCI_MODE;
        }
        if self
            .unum32
            .get(&PARAM_SCI_SET_PROG_VOLTAGE)
            .copied()
            .unwrap_or(0xFFFF_FFFF)
            != 0xFFFF_FFFF
        {
            flags |= j2534_0404::SCI_TX_VOLTAGE;
        }
        flags
    }

    /// SAE J2534-2 clause 9 Single Wire CAN (ADR-164 Decision 2/Phase 4):
    /// `TX_FLAG_SW_CAN_HV_TX` implied by a nonzero `CP_SwCan_HighVoltage`
    /// (`PARAM_SW_CAN_HIGH_VOLTAGE`) -- `sci_tx_flags`'s identical "ComParam
    /// -> per-message TxFlags bit" shape (ADR-062), cloned for this second
    /// case.
    ///
    /// **Unlike `sci_tx_flags`, this alone is NOT protocol-safe.**
    /// `PARAM_SW_CAN_HIGH_VOLTAGE` is allowlisted CAN-family-wide (Decision
    /// 2 accepts it for every CAN/ISO15765 link, not just an SW one), so a
    /// dual-wire CAN link's Working/Active set can legitimately carry a
    /// nonzero value here even though it must never affect the wire. Callers
    /// MUST gate this behind `resources::is_sw_protocol_id(hw_protocol_id)`
    /// themselves (`apply_resolved_tx_flags`) -- this method only reads the
    /// ComParam value, it does not know which link it is being read for.
    pub(super) fn sw_can_tx_flags(&self) -> u32 {
        if self
            .unum32
            .get(&PARAM_SW_CAN_HIGH_VOLTAGE)
            .copied()
            .unwrap_or(0)
            != 0
        {
            j2534_0404::TX_FLAG_SW_CAN_HV_TX
        } else {
            0
        }
    }

    /// SAE J2534-2 clause 17.4.5 (ADR-175/Phase 11): `MSG_PRIORITY_VALUE`
    /// (TxFlags bits 16-19) from `CP_MessagePriority` (`PARAM_MESSAGE_PRIORITY`).
    /// Clamps to 1..=8; any value outside that range (including the ComParam's
    /// absent/0 default) maps to 8, the lowest priority, per clause 17.4.5's
    /// own text. Callers MUST gate this behind
    /// `resources::is_j1708_protocol_id(hw_protocol_id)` themselves
    /// (`apply_resolved_tx_flags`) -- same pattern as `sw_can_tx_flags`, since
    /// `PARAM_MESSAGE_PRIORITY` is allowlisted on several unrelated protocol
    /// families too and must not bleed onto their TxFlags.
    pub(super) fn msg_priority_tx_flags(&self) -> u32 {
        let raw = self
            .unum32
            .get(&PARAM_MESSAGE_PRIORITY)
            .copied()
            .unwrap_or(0);
        let priority = if (1..=8).contains(&raw) { raw } else { 8 };
        (priority << 16) & j2534_0404::TX_FLAG_MSG_PRIORITY_VALUE
    }

    /// SAE J2534-2 clause 19.3.2.2 (ADR-192/Phase 7 Stage 7c): the broadcast
    /// address staged via `CP_TP20BroadcastAddress`, or `None` if unset/`0`
    /// (normal send). Callers MUST gate this behind
    /// `resources::is_tp2_0_family_protocol_id(hw_protocol_id)` themselves
    /// (ADR-210: widened from the narrow `is_tp2_0_protocol_id` so a
    /// `_CHx`-connected TP2.0 link's own broadcast staging is recognized
    /// too), the same shape `sw_can_tx_flags` requires of its own callers.
    pub(super) fn tp20_broadcast_address(&self) -> Option<u8> {
        match self
            .unum32
            .get(&PARAM_TP20_BROADCAST_ADDRESS)
            .copied()
            .unwrap_or(0)
        {
            0 => None,
            v @ 0xF0..=0xFF => Some(v as u8),
            _ => None, // out-of-range value: treated as "no broadcast" here;
                       // the RPC layer (rpc_primitive.rs) is responsible for
                       // rejecting an out-of-range nonzero value outright with
                       // PduErrInvalidParameters BEFORE this is ever consulted,
                       // per ADR-192 Decision item 1 -- this fallback exists
                       // only so this accessor itself can never panic/UB on a
                       // stale/racing value.
        }
    }
}

/// One expected-response descriptor passed via `ComPrimitiveCtrlData.expected_response_array`.
///
/// The poll task compares each received frame against these descriptors after
/// issuing `PassThruWriteMsgs` for a `CoptSendrecv` item.  Each send cycle's
/// receive phase runs until `NumReceiveCycles` matching frames arrive or the
/// `CP_P2Max` response window elapses (ADR-053).
#[derive(Debug, Clone)]
pub(super) struct ExpectedResponse {
    /// Bit mask applied to each data byte before comparison with `pattern`.
    pub(super) mask: Vec<u8>,
    /// Pattern that `(data[i] & mask[i])` must equal for all bytes in `mask`.
    pub(super) pattern: Vec<u8>,
    /// `unique_resp_identifier` values that restrict which UniqueRespIdTable
    /// entries are eligible to provide the matching frame.  Empty = any ECU.
    pub(super) unique_resp_ids: Vec<u32>,
    /// `acceptance_id` from the proto (forwarded verbatim to `ResultData`).
    pub(super) acceptance_id: u32,
}

impl ExpectedResponse {
    /// Returns `true` when `data` matches this descriptor's mask/pattern.
    ///
    /// Vacuously `true` for an empty `data` whenever `mask`/`pattern` is
    /// short enough that `cmp_len == 0` (in particular a fully empty
    /// `mask`/`pattern`, matching anything). This is why `events::poll_rx_inner`
    /// never calls `matches` for a START_OF_MESSAGE frame at all (ADR-097) --
    /// such a frame's post-header-split payload is always empty, and would
    /// otherwise vacuously satisfy a broad/empty descriptor and falsely
    /// complete the wait.
    pub(super) fn matches(&self, data: &[u8]) -> bool {
        if self.mask.is_empty() && self.pattern.is_empty() {
            return true;
        }
        let cmp_len = self.mask.len().min(self.pattern.len()).min(data.len());
        self.mask[..cmp_len]
            .iter()
            .zip(&self.pattern[..cmp_len])
            .zip(&data[..cmp_len])
            .all(|((&m, &p), &d)| (d & m) == p)
    }

    /// Per-descriptor vacuousness: `true` when this descriptor matches any
    /// payload, mirroring `ExpectedResponse::matches`'s own `cmp_len == 0`
    /// condition -- which is zero whenever EITHER `mask` or `pattern` is
    /// empty, not only when both are (ADR-100 Decision §3's round-5
    /// addendum). Do NOT write this as `mask.is_empty() && pattern.is_empty()`
    /// -- that narrower AND reading misses a `mask=[0xFF], pattern=[]]`-shaped
    /// descriptor (or the reverse), which is also effectively vacuous under
    /// `matches`'s real semantics.
    pub(super) fn is_vacuous(&self) -> bool {
        self.mask.is_empty() || self.pattern.is_empty()
    }
}

/// A `CopRegistrant`'s tier (ADR-100 Decision §2). Every registrant defaults
/// to `ActiveSendReceive` at insertion (S1 of ADR-100), EXCEPT a COP created
/// receive-only (`NumSendCycles == 0`, ADR-059), which is `ReceiveOnly` from
/// creation for ANY `NumReceiveCycles` (round-9 Finding-1 correction). Of
/// those, the `-1` and finite-`N > 0` subtypes additionally DETACH from
/// execution at creation (S6, widened from `-1`-only by ADR-182) -- they
/// never have an active-send-and-receive phase to begin with, so there is
/// nothing to migrate out of (`events::wait_for_expected_response`'s doc
/// comment); the `-2` (IS-MULTIPLE) subtype is tier-2 from creation too but
/// stays inline/blocking, unaffected by ADR-182. As of S5, an IS-CYCLIC
/// (`NumReceiveCycles == -1`) registrant created WITH a send phase migrates
/// itself from `ActiveSendReceive` to `ReceiveOnly` in place, inside
/// `wait_for_expected_response_inner`, the moment its first positive
/// response arrives -- see `events::migrate_registrant_to_receive_only` and
/// `events::ReceivePhaseOutcome::DetachedToTier2`.
//
// S3 of ADR-100 (`events::bind_registrant`) reads `tier` to scan tier-1 vs
// tier-2 candidates; S5 added migration-based `ReceiveOnly` construction, S6
// added creation-time `ReceiveOnly` construction for the receive-only `-1`
// case (see above).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RegistrantTier {
    /// Spec's "current active Send/Receive ComPrimitives": the executing
    /// one-shot COP, periodic SendRecv COPs, and an IS-CYCLIC COP before its
    /// first positive response. Carries the full RC state machine.
    ActiveSendReceive,
    /// Spec's "Receive Only list": COPs created with `NumSendCycles == 0`
    /// (ADR-059) -- constructed directly in this tier at insertion for ANY
    /// `NumReceiveCycles` value (finite `N`, `-1`, or `-2`; ADR-100 round-9
    /// Finding-1 correction to Decision §2/§4 -- originally, and now for the
    /// `-1`/finite-`N > 0` subtypes, S6's `CP_CyclicRespTimeout`/
    /// `cyclic_deadline` scope, widened from `-1`-only by ADR-182; `-2` stays
    /// out of that scope), and IS-CYCLIC COPs created WITH a send phase,
    /// after migrating post-first-match (S5). Plain-match only -- per the 7F
    /// clause, negative response handling does not apply to receive only
    /// ComPrimitives, for the WHOLE tier.
    ReceiveOnly,
}

/// One outstanding ComPrimitive's entry in a CLL's response-binding registry
/// (ADR-100 Decision §1). Inserted into `LogicalLinkState::registrants` when
/// the COP's receive phase begins and removed when it ends
/// (`wait_for_expected_response`, `cancel_link_cops`).
///
/// S3 of ADR-100 (`events::bind_frame`/`bind_registrant`) is the attribution
/// pipeline that replaced the single-`MatchProbe` scan and now reads every
/// field here except `request_sid` (redundant with -- and, at every current
/// construction site, always equal to -- `rc_cfg`'s own `request_sid`, which
/// is what `detect_pending_rc` actually consults; see that field's own doc
/// comment) -- it keeps `#[allow(dead_code)]` until a later step gives it an
/// independent reader. `cyclic_deadline`/`cyclic_timeout_ms` are read/written
/// as of S6 (ADR-100 Decision §4, scope widened by ADR-182):
/// `bind_registrant`'s match-acceptance step restarts `cyclic_deadline` from
/// `cyclic_timeout_ms` on every accepted match, and
/// `events::wait_for_expected_response_inner`'s `no_deadline` loop, plus the
/// per-tick `events::reap_expired_cyclic_registrants` sweep for an
/// already-detached registrant, check `cyclic_deadline` for expiry. That
/// same per-tick sweep additionally checks `matches_needed`/`matches_got`
/// count-completion for a finite-`N` detached registrant (ADR-182), ahead of
/// (and independent from) the `cyclic_deadline` expiry check.
#[derive(Debug, Clone)]
pub(super) struct CopRegistrant {
    pub(super) cop_handle: u32,
    /// Per-CLL monotonic tie-break (ADR-100 Decision §3, resolved (a)):
    /// assigned from `LogicalLinkState::next_registrant_seq` under the
    /// `logical_links` lock at insertion time. Same-instant registrations
    /// cannot occur under the lock, so ascending `registration_seq` gives an
    /// unambiguous "COP start order".
    pub(super) registration_seq: u64,
    pub(super) tier: RegistrantTier,
    pub(super) expected: Vec<ExpectedResponse>,
    /// `Some` only for a tier-1 registrant -- tier-2 (Receive Only) never
    /// runs RC detection (ISO 22900-2's 7F clause: receive only
    /// ComPrimitives get no negative response handling).
    pub(super) rc_cfg: Option<RcHandlingConfig>,
    /// First post-header byte of this COP's own original request (ADR-100
    /// Decision §3, resolved (b)). `None` when unknown or not applicable
    /// (e.g. a receive-only COP with no request of its own).
    ///
    /// Not read by `bind_registrant`/`bind_frame` (S3): the request-SID gate
    /// is applied via `rc_cfg.as_ref().and_then(|cfg| cfg.detect_pending_rc(..))`,
    /// and `rc_cfg`'s own `request_sid` (set by the same `with_request_sid`
    /// call, at the same call site, from the same source byte) is what that
    /// method actually consults -- this top-level copy is therefore always
    /// equal to `rc_cfg.as_ref().and_then(|c| c.request_sid)` at every
    /// current construction site and has no independent reader yet. Kept
    /// (not removed) since it is `CopRegistrant`'s own natural home for this
    /// value if a later step (e.g. a tier-2 registrant with `rc_cfg: None`)
    /// ever needs it without a full `RcHandlingConfig`.
    #[allow(dead_code)]
    pub(super) request_sid: Option<u8>,
    pub(super) matches_needed: Option<u32>,
    pub(super) matches_got: u32,
    pub(super) pending_rc: Option<u8>,
    /// `LogicalLinkState::connect_generation` captured at registration
    /// (ADR-086 staleness gating moves here per ADR-100 Decision §1).
    pub(super) connect_generation: u64,
    /// `CP_CyclicRespTimeout` deadline for a created-receive-only
    /// (`NumSendCycles == 0`, ADR-059) `NumReceiveCycles == -1` or finite
    /// `N > 0` registrant (ADR-100 Decision §4, S6, scope widened from
    /// `-1`-only by ADR-182): `Some(now + cyclic_timeout_ms)` when the
    /// ComParam is configured (nonzero) at insertion, restarted to
    /// `Some(now + cyclic_timeout_ms)` again on every accepted match
    /// (`bind_registrant`), `None` when the ComParam is `0` (disabled -- the
    /// COP then behaves exactly as before this step, ending only via
    /// cancel/hard-error/staleness) or the registrant is not this specific
    /// case (every tier-1 registrant, every other tier-2 registrant
    /// including a migrated (S5) IS-CYCLIC COP and a created-receive-only
    /// `-2`, which never get a cyclic deadline in this step per ADR-100
    /// Decision §4's resolved scope). For the finite-`N` subtype, expiry with
    /// `matches_needed` still unmet is a reap-time error path
    /// (`events::reap_expired_cyclic_registrants`, ADR-182); for `-1`
    /// (`matches_needed: None`), expiry stays a plain, non-error finish,
    /// unchanged.
    pub(super) cyclic_deadline: Option<tokio::time::Instant>,
    /// The configured `CP_CyclicRespTimeout` in ms (`ComParamSet::
    /// cyclic_resp_timeout_ms`) for this specific registrant, `Some` only
    /// when `cyclic_deadline` is eligible to be set (see that field's own
    /// doc comment) -- kept alongside `cyclic_deadline` so `bind_registrant`
    /// can recompute a fresh `now + cyclic_timeout_ms` deadline on each
    /// accepted match without needing access to the COP's `ComParamSet`
    /// (which is not otherwise threaded into the attribution layer). `Some(0)`
    /// (ComParam present but explicitly zero) and `None` (not this case) are
    /// both "no deadline, never restart" -- `bind_registrant` only acts when
    /// this is `Some(ms)` with `ms > 0`.
    pub(super) cyclic_timeout_ms: Option<u32>,
    /// `true` only for the true IS-CYCLIC shape (a send phase present,
    /// `NumReceiveCycles == -1`) -- set at registration
    /// (`wait_for_expected_response`), consulted by `bind_registrant`'s
    /// match-acceptance step (ADR-100 round-9 Finding-2 correction) to flip
    /// `tier` from `ActiveSendReceive` to `ReceiveOnly` in place, immediately
    /// after accepting this registrant's first match, so every later frame in
    /// the SAME `PassThruReadMsgs` batch is scanned against the already-
    /// migrated tier instead of a stale tier-1 snapshot. `false` for every
    /// other registrant, including the round-9-Finding-1-corrected
    /// created-receive-only shapes (finite `N`, `-1`, `-2`), which are
    /// already tier-2 from creation and must never migrate again.
    pub(super) migrate_on_first_match: bool,
    /// `CP_ModifyTiming`'s KWP live-exchange config (ADR-146), attached the
    /// same way `rc_cfg` is: `Some` only for a tier-1 registrant, mirroring
    /// `rc_cfg`'s own `created_receive_only` gate (ISO 22900-2 disables
    /// negative-response handling for receive-only COPs, and by the same
    /// reasoning this mechanism too). `None` on every non-ISO14230 channel,
    /// or when `CP_ModifyTiming` was disabled at COP-creation time.
    pub(super) timing_cfg: Option<TimingChangeConfig>,
    /// Running worst-case timing accumulator for a functionally-addressed
    /// COP (ADR-146 Decision, "Functional-addressing worst case";
    /// generalized beyond KWP by ADR-150): KWP combines
    /// `[P2Min, P2Max, P3Min, P3Max, P4Min]` raw wire bytes element-wise
    /// (min for P2Min, max for the rest) on every qualifying TPI=2 response;
    /// UDS combines `(P2Max_ms, P2Star_10ms)` toward the maximum on every
    /// qualifying SID 0x50 response. `None` until the first such response.
    pub(super) timing_accumulator: Option<TimingAccumulator>,
    /// Set by `bind_registrant` when the frame just accepted was a
    /// qualifying `0xC3` Access Timing Parameter response that produced a
    /// ComParam modification (ADR-146) -- consumed by the poll task once
    /// this pass's attribution borrow of the registrant list has ended, then
    /// discarded (unlike `pending_rc`, this is never left for a blocked
    /// COP's own wait loop: it is a background side effect of the exchange,
    /// not part of any COP's own result, and a fresh snapshot never inherits
    /// a stale value since it is never written back to the live registrant).
    ///
    /// The `usize` is the pass-local frame sequence number (`poll_rx_inner`'s
    /// `for (frame_seq, msg) in messages.iter().enumerate()`) that produced
    /// this change, used by `select_latest_timing_changes` (ADR-146 amended,
    /// PR #17 round-4 Codex finding) to resolve two different registrants
    /// that both qualify on the same CLL in one pass by arrival order
    /// (last-exchange-wins) rather than worst-case combination -- two
    /// different registrants are, by construction, always two independent
    /// SID 0x83 exchanges, never "multiple ECUs responding to the same
    /// broadcast" (that case is already resolved WITHIN one registrant via
    /// `timing_accumulator`, gated on `TimingChangeConfig.functional`).
    /// This value is pass-local and is never merged back to the live
    /// registrant by `merge_registrant_writeback` -- only
    /// `timing_accumulator` is -- so a pass-local sequence number is a valid
    /// total order across this pass's registrants on one CLL; no cross-pass
    /// comparison of this sequence number ever happens.
    pub(super) pending_timing_change: Option<(usize, PendingTimingChange)>,
    /// `CP_EnableConcatenation` (D-PDU `PARAM_ENABLE_CONCATENATION` =
    /// 0x807B) resolved once at registrant-creation time
    /// (`events::wait_for_expected_response`) from the bound ComParamSet's
    /// value AND the CLL's protocol (ISO 22900-2:2022 Table B.11: KWP
    /// family -- ISO 9141-2, ISO 14230-2/-4 -- and SAE J1850 VPW/PWM only)
    /// AND this registrant's own shape (v1 scope: tier-1 at creation, never
    /// IS-CYCLIC `NumReceiveCycles == -1`, never created-receive-only per
    /// ADR-059 -- IS-MULTIPLE `NumReceiveCycles == -2` IS in scope). `false`
    /// for every registrant outside that scope, including every tier-2
    /// registrant -- `events::bind_registrant`'s concat branches are gated
    /// on this field so every other protocol/registrant shape is completely
    /// unaffected.
    pub(super) concat_enabled: bool,
    /// Open `CP_EnableConcatenation` segment-merge accumulators -- one entry
    /// per distinct `(unique_resp_identifier, SID)` key currently
    /// mid-accumulation for this registrant (`events::bind_registrant`/
    /// `events::finalize_concat_buffers`). Not a `HashMap`: expected
    /// cardinality is single-digit distinct ECUs (IS-MULTIPLE,
    /// `NumReceiveCycles == -2`), so a linear scan
    /// (`iter_mut().find(|b| b.key == frame_key)`) is simpler and preserves
    /// first-segment arrival order, which keeps finalize/delivery order
    /// deterministic. Empty initially and whenever no buffer is open. Always
    /// empty when `concat_enabled` is `false`. (ADR-148 Amendment: was
    /// `Option<ConcatBuf>` -- a single shared buffer wrongly merged
    /// interleaved segments from different ECUs under IS-MULTIPLE; see the
    /// amendment section.)
    pub(super) concat: Vec<ConcatBuf>,
    /// Monotone count of segments absorbed into `concat` over this
    /// registrant's whole lifetime (never reset, never decremented) --
    /// mirrors `matches_got`'s own per-pass delta-merge pattern
    /// (`events::merge_registrant_writeback`) so a poll pass's own
    /// contribution can be diffed against a baseline the same way. Always
    /// `0` when `concat_enabled` is `false`.
    pub(super) concat_segments_got: u32,
}

/// Accumulator for one in-progress `CP_EnableConcatenation` segment merge
/// (D-PDU `PARAM_ENABLE_CONCATENATION` = 0x807B), one entry in a
/// `CopRegistrant::concat` `Vec` (`concat_enabled: true`). Opened by the
/// first segment of a response with a `key` not already open, extended by
/// each subsequent segment sharing the SAME `key`, and finalized -- as one
/// completed logical match, delivered as a single `ResultData` -- by
/// either of two triggers (ADR-148 Amendment; a differing-key frame is no
/// longer, by itself, one of them): (a) the shared `CP_P2Max`-derived
/// receive-phase deadline expiring with the buffer still open, which
/// finalizes every currently-open buffer on this registrant together, in
/// one pass (`events::wait_for_expected_response_inner`, `events::
/// finalize_concat_buffers`); or (b) an empty-payload frame arriving that
/// matches the registrant's descriptor, which force-finalizes every
/// currently-open buffer first, before the empty match is itself
/// considered (`events::bind_registrant`, `events::finalize_concat_buffers`).
/// See `docs/adr/` for the full design.
#[derive(Debug, Clone)]
pub(super) struct ConcatBuf {
    /// `(unique_resp_identifier, source_id, SID)` of the segment that opened
    /// this buffer. A later segment extends it only when all three match
    /// this exact buffer's own key -- `events::bind_registrant` looks the
    /// incoming frame's key up across every open buffer in `concat`, not
    /// just one.
    ///
    /// ADR-148 third Amendment (Fix 1): the middle `source_id` component
    /// (`events::ConcatFrameMeta::source_id`, `Option<u8>`) was added
    /// because `unique_resp_identifier` alone is `0` for EVERY frame on a
    /// no-`UniqueRespIdTable` CLL, or a table-configured CLL with no
    /// `CP_EcuRespSourceAddress`-keyed entries (`events::route_frame`'s own
    /// doc comment) -- wildcard delivery either way, and (before
    /// [ADR-203](../../docs/adr/ADR-203-kwp-j1850-source-address-rx-routing.md))
    /// the only RX mode that actually worked for KWP/J1850 at all, the
    /// concat-eligible protocols. Keying on `(unique_resp_identifier, SID)`
    /// alone let two distinct ECUs answering the same broadcast/functional
    /// request with the same SID collide into one corrupted buffer.
    /// `source_id` is the frame's own split-header source-address byte, not
    /// the URID; a headerless/too-short frame's `source_id` is `None`,
    /// which groups it with every other `None`-keyed frame sharing the same
    /// `(unique_resp_identifier, SID)` -- an accepted residual (no
    /// ECU-identifying data is physically available at this layer for such
    /// a frame). ADR-203 gives a table-configured, SA-keyed KWP/J1850 CLL a
    /// real, distinct `unique_resp_identifier` per ECU too, but `source_id`
    /// remains the correct key component regardless, since concat can be
    /// enabled on a wildcard-mode CLL just as well.
    pub(super) key: (u32, Option<u8>, u8),
    /// First segment's timestamp -- the finalized delivery's own timestamp.
    pub(super) timestamp: u32,
    /// First segment's header bytes (ADR-051 split) -- the finalized
    /// delivery's own header bytes.
    pub(super) header_bytes: Vec<u8>,
    /// First segment's `unique_resp_identifier` -- the finalized delivery's
    /// own value (every segment absorbed into one buffer shares the same
    /// `unique_resp_identifier`, since that is part of `key`).
    pub(super) unique_resp_identifier: u32,
    /// First segment's matched descriptor's `acceptance_id` -- the
    /// finalized delivery's own value.
    pub(super) acceptance_id: u32,
    /// First segment's RxStatus flags -- the finalized delivery's own
    /// value.
    pub(super) rx_status_flags: u8,
    /// Merged payload: the first segment's full payload, then each
    /// subsequent segment's `payload[1..]` appended (the SID/first byte
    /// appears once in the merged result).
    pub(super) data: Vec<u8>,
    /// Last segment's footer bytes (ADR-051 split) -- overwritten on every
    /// append, so this is always the MOST RECENT segment's footer, not the
    /// first's.
    pub(super) footer_bytes: Vec<u8>,
    /// `true` iff the descriptor that admitted this buffer's opening segment
    /// was vacuous (`ExpectedResponse::is_vacuous()`) -- fixed at the moment
    /// the buffer is opened (`events::bind_registrant`'s "open a new buffer"
    /// arm) and never mutated afterward: a continuation frame absorbed by
    /// the fast path may not satisfy ANY descriptor's mask/pattern at all
    /// (that's the whole reason the fast path bypasses
    /// `ExpectedResponse::matches` for it), so there is no descriptor to
    /// re-derive vacuousness from on a continuation. ADR-148 second
    /// Amendment: gates the continuation fast path against
    /// `events::AttributionScan` so a vacuous-opened buffer's continuations
    /// still yield to a non-vacuous claimant during the earlier
    /// `Tier1NonVacuous` scan pass, preserving ADR-100's non-vacuous-first
    /// precedence.
    pub(super) opened_vacuous: bool,
    /// Count of physical segments absorbed into this ONE buffer so far,
    /// including the opening segment (starts at `1`) -- distinct from
    /// `CopRegistrant::concat_segments_got`, which is a per-REGISTRANT
    /// lifetime total across every buffer it has ever held. ADR-148 third
    /// Amendment (Fix 2): checked against `events::CONCAT_MAX_BUF_SEGMENTS`
    /// on every absorb, alongside `data.len()` against `events::
    /// CONCAT_MAX_BUF_BYTES`, so a noisy/misbehaving sender cannot grow one
    /// buffer or postpone its receive-phase deadline restart indefinitely --
    /// closes the degenerate case where a SID-only 1-byte payload adds `0`
    /// bytes to `data` per segment but would otherwise restart the deadline
    /// forever.
    pub(super) segments: u32,
}

/// ISO-TP frame finishing options used when this service builds CAN frames
/// itself (`can_channel_mode = "software-isotp"`, ADR-046).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct IsoTpFraming {
    /// Pad every CAN frame to 8 data bytes (`TX_ISO15765_FRAME_PAD` on the
    /// COP, or `CP_CANFillerByteHandling = 1` in the Active set).
    pub(super) pad: bool,
    /// Padding byte value (`CP_CANFillerByte`, default 0x00).
    pub(super) filler: u8,
}

/// Per-send parameters for the software ISO-TP TX driver (ADR-046).
///
/// Present on a `TxItem::SendRecv` only when the CLL runs in
/// `software-isotp` mode; `None` means the payload is written to the J2534
/// channel unchanged (hardware ISO15765 or genuinely raw traffic).
#[derive(Debug, Clone, Copy)]
pub(in crate::service) struct SoftIsoTpTx {
    pub(super) framing: IsoTpFraming,
    /// Addressing used to build our own outgoing SF/FF/CF frames, decoded
    /// from the UniqueRespIdTable entry's `CP_CanPhysReqFormat` /
    /// `CP_CanPhysReqExtAddr` (extended addressing, ADR-046 addendum).
    pub(super) tx_addressing: isotp::Addressing,
    /// CAN ID the ECU's FlowControl frames are expected from
    /// (`CP_CanRespUSDTId` paired with the request's `CP_CanPhysReqId` in the
    /// UniqueRespIdTable).  `None` = accept a FlowControl from any CAN ID.
    pub(super) fc_can_id: Option<u32>,
    /// Addressing used to parse the incoming FlowControl frame, decoded from
    /// the same UniqueRespIdTable entry's `CP_CanRespUSDTFormat` /
    /// `CP_CanRespUSDTExtAddr` (extended addressing, ADR-046 addendum).
    pub(super) fc_rx_addressing: isotp::Addressing,
    /// ISO 15765-2 N_Bs: how long to wait for a FlowControl frame after a
    /// FirstFrame / block of ConsecutiveFrames (ms).
    pub(super) n_bs_timeout_ms: u32,
}

/// Software-ISO-TP framing/addressing for the periodic tester-present
/// message (ADR-046 addendum). Unlike a `CoptSendrecv` payload, the
/// tester-present message is always sent as a single SingleFrame — never
/// segmented, since every tester-present send (both `CP_TesterPresentSendType`
/// values) retransmits the exact same fixed payload, with no sequence-number
/// state that would carry meaningfully across repeats the way a genuine
/// multi-frame transfer's would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::service) struct SoftIsoTpFraming {
    pub(super) framing: IsoTpFraming,
    pub(super) addressing: isotp::Addressing,
}

/// The ComParam snapshot(s) bound to a `CoptSendrecv`/`CoptStartcomm`/
/// `CoptStopcomm` at `StartComPrimitive` call time (ADR-067, per ISO
/// 22900-2 §9.4.3: a ComPrimitive's ComParam state binds once, at the
/// moment the call is made -- not "live" at whatever moment the poll task
/// later happens to execute it). Every ComParam-dependent element of the
/// COP -- addressing, message construction, TX size validation, TxFlags,
/// ISO-TP framing, and the response-phase `CP_P2Max`/RC21/23/78 config -- is
/// resolved from this bound snapshot for the COP's *entire* life, including
/// every cycle of a cyclic `CoptSendrecv`. A `SetComParam` issued after
/// `StartComPrimitive` returns can never retroactively affect an
/// already-started COP; a new `StartComPrimitive` call is required to pick
/// it up.
///
/// This supersedes ADR-063/064/066's "resolve live, in the poll task, right
/// before this COP's first cycle actually runs" design: that design's
/// premise -- that resolving live is required for FIFO consistency with a
/// queued `CoptUpdateparam`/`CoptRestoreParam` -- does not hold. ISO 22900-2
/// binds a ComPrimitive's params at submission, so a `CoptUpdateparam`/
/// `CoptRestoreParam` racing a `StartComPrimitive` call is exactly the case
/// where the *earlier* call's bound snapshot must win, not whichever the
/// poll task happens to observe when it later gets around to the item. See
/// ADR-067.
///
/// The one exception, unchanged since ADR-060: `CP_P3Func`/`CP_P3Phys`'s
/// inter-request gap protects the shared physical bus across every CLL on
/// it, not just this one COP's transaction, and continues to read the live
/// Active set unconditionally (`wait_for_p3_gap`).
#[derive(Debug, Clone)]
pub(in crate::service) enum ParamBinding {
    /// `temp_param_update` unset: the call-time Active snapshot. Hardware
    /// already reflects Active (from the last `CoptUpdateparam` / connect),
    /// so no hardware push/revert is needed around this COP's transaction.
    Plain(ComParamSet),
    /// `temp_param_update` set (ISO 22900-2 §9.4.3): `effective` is the
    /// Working snapshot at call time -- what addressing/message
    /// construction/response-phase config are resolved from, and what is
    /// pushed to hardware before this COP's transaction runs (every cycle,
    /// for a cyclic `CoptSendrecv`; the init transaction only, for
    /// `CoptStartcomm`). Afterward, hardware is reverted -- finally-style,
    /// on every path including a failed hardware apply and a failed
    /// TX/init -- to the *live* Active set read at revert time in the poll
    /// task, NOT an Active snapshot bound at call time. The revert target
    /// is a restoration duty ("leave the link in whatever is Active now"),
    /// not a COP-bound param, so ADR-067's call-time binding does not apply
    /// to it: a `CoptUpdateparam` already queued ahead of this COP, or
    /// interleaved between cycles of a cyclic temp send, has legitimately
    /// moved hardware and `LogicalLinkState.active` on, and the revert must
    /// not undo it.
    Temp { effective: ComParamSet },
}

impl ParamBinding {
    /// The ComParam set this COP's resolution (addressing, message
    /// construction, response-phase config) reads from: `effective` for a
    /// temp COP, the bound Active snapshot otherwise.
    pub(super) fn resolved(&self) -> &ComParamSet {
        match self {
            Self::Plain(p) => p,
            Self::Temp { effective, .. } => effective,
        }
    }
}

/// Everything a `CoptSendrecv` needs to actually transmit, resolved once,
/// eagerly, by `rpc_start_com_primitive` at `StartComPrimitive` call time
/// against the bound `ParamBinding` (ADR-067; reverting ADR-064's deferral
/// of this resolution to the poll task) -- reused unchanged by every cycle
/// of a cyclic send, including follow-up cycles (`CycleContinuation`).
#[derive(Debug, Clone)]
pub(in crate::service) struct SendRecvTx {
    pub(super) data: Vec<u8>,
    pub(super) tx_flags: u32,
    /// `Some` when the poll task must segment `data` into raw CAN
    /// frames (software ISO-TP, ADR-046) instead of writing it as one
    /// message.
    pub(super) isotp_tx: Option<SoftIsoTpTx>,
    /// CAN-family addressing of this send
    /// (`tx_header::CanAddressing::functional`), `None` for non-CAN-family
    /// protocols or when addressing could not be resolved. Drives
    /// `CP_P3Func`/`CP_P3Phys` minimum inter-request gap enforcement
    /// (ADR-060, unaffected by ADR-067).
    pub(super) can_functional: Option<bool>,
    /// The original `cop_data`'s own SID byte this send was resolved from
    /// (`data` above is header-prefixed per ADR-050, so this is captured
    /// separately, before the header is prepended) -- feeds
    /// `RcHandlingConfig::request_sid` (ADR-100 Decision §3, resolved (b))
    /// wherever this COP's `rc_cfg` is built. `None` for an empty request (or
    /// a request shorter than `tx_prefix`, ADR-196 Decision item 3b).
    /// `cop_data[0]` for a RawMode=OFF CLL; `cop_data[tx_prefix]` for a
    /// RawMode=ON one, both captured at the exact same call site `tx_prefix`
    /// itself is computed at (`rpc_primitive::compute_tx_prefix`) -- see that
    /// function's own doc comment.
    pub(super) request_sid: Option<u8>,
    /// The original, payload-only `cop_data` this send was resolved from
    /// (ADR-050) -- captured verbatim, alongside `request_sid`, so
    /// `TimingChangeConfig::with_request` (ADR-146) can be built wherever
    /// this COP's `timing_cfg` is, from the exact bytes the client sent
    /// (needed in full for a TPI=3 request's 5 timing bytes, unlike
    /// `request_sid`'s single leading byte). `None` for an empty request.
    /// Captured verbatim (RawMode's own raw CAN-ID prefix included, unlike
    /// `request_sid` above, which is already re-based) -- `tx_prefix` below
    /// is threaded alongside it for `TimingChangeConfig::with_request` to
    /// re-base its own anchors against.
    pub(super) access_timing_request: Option<Vec<u8>>,
    /// ADR-196 Decision item 3b: the raw CAN-ID prefix width
    /// (`rpc_primitive::compute_tx_prefix`'s output, the SAME shared helper
    /// `request_sid`'s own capture used) preceding the actual UDS/KWP
    /// payload in `access_timing_request` above, for a RawMode=ON CLL -- `0`
    /// unconditionally for RawMode=OFF. Forwarded to
    /// `TimingChangeConfig::with_request` wherever this COP's `timing_cfg`
    /// is built (`events::handle_send_recv`, and the two
    /// `CoptStartcomm`/`CoptStopcomm` optional-message call sites in
    /// `rpc_primitive.rs` that build `timing_cfg` directly).
    pub(super) tx_prefix: usize,
    /// ADR-180 Decision 14 (design-advisor consult, PR #72 round 12,
    /// Finding 1 Part B): the SAE J1939 source address byte this send's
    /// `data` header was actually composed with (`rpc_primitive.rs::
    /// resolve_send_recv_tx`'s `ResolvedSendRecvTx::j1939_tx_source`,
    /// copied through unchanged) -- `None` for every non-J1939 protocol.
    /// `events.rs::handle_send_recv` re-checks this against the CLL's
    /// CURRENT `j1939_claimed_address` at EACH transmit cycle (not just the
    /// first) and cancels the COP the same way an already-cancelled item is
    /// skipped, if they no longer match -- closing the TOCTOU between this
    /// struct's call-time resolution and a later cycle's actual dispatch (a
    /// claim can be spontaneously lost, or a claim can still be pending, in
    /// between).
    pub(super) j1939_tx_source: Option<u8>,
    /// ADR-188 fix (edge-case-hunter, PR #97): the bind-time SAE J2534-2
    /// clause 19 TP2.0 TX-ID this send's `data` header was actually
    /// composed with (`rpc_primitive.rs::resolve_send_recv_tx`'s
    /// `ResolvedSendRecvTx::tp20_established_tx_id`, copied through
    /// unchanged) -- `None` for every non-TP2.0 protocol. Mirrors
    /// `j1939_tx_source` above exactly: `events.rs::handle_send_recv`/
    /// `handle_stop_comm` re-check this against the CLL's CURRENT
    /// `LogicalLinkState::tp20_connection` at EACH transmit dispatch (not
    /// just this COP's bind time) and cancel the COP if the connection is
    /// no longer `Established` with this same TX-ID -- closing the TOCTOU
    /// between this struct's call-time resolution and a later dispatch, for
    /// the identical reason `j1939_tx_source` exists: `CoptSendrecv` has no
    /// `comm_started` precondition gate, so a `CoptStopcomm`'s queued
    /// connection teardown can complete after this COP was bound but before
    /// it dispatches.
    pub(super) tp20_established_tx_id: Option<u32>,
    /// Codex review fix (P1, PR #101, ADR-192/Phase 7 Stage 7c): whether
    /// this send was resolved as a TP2.0 broadcast
    /// (`rpc_primitive.rs::resolve_send_recv_tx`'s `ResolvedSendRecvTx::
    /// tp20_is_broadcast`, copied through unchanged). `tp20_established_
    /// tx_id` above is captured from the CLL's live connection state
    /// independent of whether THIS send is a broadcast (ADR-192 Decision
    /// item 1: broadcast is per-send, connection-independent, so a CLL can
    /// have both an established connection and a staged broadcast at once),
    /// so `events.rs::handle_send_recv`/`handle_stop_comm` must consult
    /// this flag FIRST and skip their own TP2.0-connection-awareness
    /// dispatch-time overwrite entirely for a broadcast item -- otherwise
    /// that overwrite unconditionally clobbers `data[0..4]` (the broadcast
    /// address byte plus 3 payload bytes) with the connection's live TX-ID
    /// whenever the connection hasn't drifted.
    pub(super) tp20_is_broadcast: bool,
}

/// Everything a one-shot transmit-then-optionally-receive COP phase needs,
/// bundled with the `SendRecvTx` the transmit itself already uses so
/// "receive config without a transmit" is structurally unrepresentable.
/// Shared by two call sites:
///
/// - `CoptStopcomm`'s optional final message (ADR-085/ADR-087, unchanged
///   behavior): all fields bind at `StartComPrimitive` call time from the
///   bound Active ComParam snapshot -- never Working, regardless of
///   `temp_param_update` (`CoptStopcomm`'s `temp_param_update` remains a
///   documented no-op, ADR-044/ADR-067).
/// - `CoptStartcomm`'s optional CAN/J1850 request message (ADR-111, new):
///   resolved from `binding.resolved()` (Working when `temp_param_update`
///   was set, Active otherwise) -- unlike `CoptStopcomm`, `CoptStartcomm`'s
///   `Temp` binding is genuinely pushed to hardware for the duration of this
///   transaction.
///
/// `num_receive_cycles == -1` (IS-CYCLIC) is rejected synchronously by both
/// call sites' `rpc_start_com_primitive` branches before this is ever
/// constructed, since an "until cancelled" receive would contradict a COP
/// that must terminate (to `PDU_CLLST_ONLINE` for StopComm, or to
/// `PDU_CLLST_COMM_STARTED` for StartComm).
#[derive(Debug, Clone)]
pub(in crate::service) struct OneShotCommTx {
    pub(super) send: SendRecvTx,
    pub(super) expected_response: Vec<ExpectedResponse>,
    pub(super) num_receive_cycles: i32,
    pub(super) response_timeout_ms: u32,
    pub(super) rc_cfg: RcHandlingConfig,
    /// `CP_ModifyTiming` live-exchange config (ADR-146), built the same way
    /// and at the same call site as `rc_cfg` above -- `None` exactly when
    /// `TimingChangeConfig::from_params` returns `None` (non-ISO14230 or
    /// `CP_ModifyTiming` disabled). Boxed: `OneShotCommTx` is embedded
    /// inline in `TxItem::StartComm`/`StopComm`, and this field would
    /// otherwise widen the enum's largest variant enough to trip
    /// `clippy::large_enum_variant` against the far smaller `SendRecv`
    /// variant.
    pub(super) timing_cfg: Option<Box<TimingChangeConfig>>,
    /// `ComParamSet::enable_concatenation()` resolved from the same
    /// `binding.resolved()` snapshot as `rc_cfg`/`response_timeout_ms`
    /// (ADR-067) -- threaded into `ExpectedResponseWait::enable_concatenation`
    /// at this COP's `wait_for_expected_response` call site so a
    /// `CP_EnableConcatenation`-aware registrant can be created for this
    /// optional `CoptStartcomm`/`CoptStopcomm` message too, not just
    /// `CoptSendrecv`.
    pub(super) enable_concatenation: bool,
}

/// Resolved 5-baud initialisation inputs for a K-line `CoptStartcomm`
/// (ADR-076), carried in `TxItem::StartComm::five_baud`. Resolved once,
/// eagerly, by `rpc_start_com_primitive` at `StartComPrimitive` call time --
/// same call-time-binding discipline as `ResolvedTesterPresent` and
/// `fast_init` (ADR-067/ADR-075) -- so the poll task never re-derives
/// which init sequence applies or what address/delivery behavior it implies.
///
/// Two distinct construction paths populate this, both call-time:
/// - The spec-mandated 5-baud contract (`CP_InitializationSettings == 1` on a
///   K-line link): `address` comes from `CP_5BaudAddressFunc`/
///   `CP_5BaudAddressPhys` (per `CP_RequestAddrMode`), and `deliver_keybytes`
///   follows `PDU_COP_CTRL_DATA.NumReceiveCycles` (`1` = deliver, `0` =
///   suppress). `cop_data` must be empty on this path (validated
///   synchronously) and is not consulted.
/// - The pre-ADR-076 legacy heuristic (`CP_InitializationSettings` absent,
///   K-line, `select_init_sequence` picks `FiveBaud`): `address` is the raw
///   client byte `cop_data[0]`. For ISO9141/ISO14230, `deliver_keybytes` is
///   always `true` -- unchanged from before ADR-076. For
///   `PROTOCOL_UART_ECHO_BYTE_PS`, whose only reachable init path is this
///   legacy heuristic (`CP_InitializationSettings` is outside its ComParam
///   allowlist, ADR-170 Decision 3), `deliver_keybytes` instead follows the
///   spec path's own `NumReceiveCycles` gating described above (ADR-183) --
///   `address` still comes from `cop_data[0]`, unchanged.
#[derive(Debug, Clone, Copy)]
pub(in crate::service) struct FiveBaudInit {
    /// Target ECU address sent at 5 baud (`PassThruIoctl(FIVE_BAUD_INIT)`'s
    /// single input byte).
    pub(in crate::service) address: u8,
    /// Whether the ECU key bytes returned by `five_baud_init` should be
    /// pushed into the receive buffer and delivered via a `ResultData`
    /// event. `false` on the spec path with `NumReceiveCycles == 0`, and on
    /// the legacy path for `PROTOCOL_UART_ECHO_BYTE_PS` with
    /// `NumReceiveCycles == 0`/absent (ADR-183); always `true` on the legacy
    /// path for ISO9141/ISO14230 (the init still runs in the suppressed
    /// cases, but nothing is delivered).
    pub(in crate::service) deliver_keybytes: bool,
}

/// Resolved fast-init dispatch for a K-line `CoptStartcomm` (ADR-075,
/// extended by ADR-077 for the wakeup-only case), carried in
/// `TxItem::StartComm::fast_init`. Resolved once, eagerly, by
/// `rpc_start_com_primitive` at `StartComPrimitive` call time -- same
/// call-time-binding discipline as `FiveBaudInit`/`ResolvedTesterPresent` --
/// so the poll task never re-derives which fast-init variant applies.
///
/// `None` (not this enum) at the `TxItem::StartComm` field means no
/// fast-init runs at all; see `TxItem::StartComm::fast_init`.
#[derive(Debug, Clone)]
pub(in crate::service) enum FastInit {
    /// The D-PDU API spec's fast-init service request is OPTIONAL: an empty
    /// `cop_data` with `CP_InitializationSettings` explicitly `2` on a
    /// K-line link sends only the wakeup pattern, via
    /// `PassThruIoctl(FAST_INIT)` with a NULL input message -- no KWP header
    /// is built, and (per spec) no response is delivered since no request
    /// was sent. Distinct from the legacy behavior (param unset), which
    /// keeps treating empty `cop_data` as "skip init entirely".
    WakeupOnly,
    /// The wakeup pattern followed by a start-communication request: the KWP
    /// wakeup frame, pre-built at `StartComPrimitive` call time from
    /// `binding.resolved()` and the UniqueRespIdTable snapshot (ADR-075).
    WithRequest(Vec<u8>),
}

/// Outbound operation enqueued by `StartComPrimitive` for async dispatch by
/// the per-channel poll task.
///
/// All J2534 API calls are serialised through the poll task so that they run
/// in strict FIFO order on a single logical thread.
#[derive(Debug)]
pub(in crate::service) enum TxItem {
    /// Normal message send (`CoptSendrecv`).
    SendRecv {
        cop_handle: u32,
        cll_handle: u32,
        protocol_id: u32,
        /// The CLL's LOGICAL/service-level protocol (`LogicalLinkState::protocol`),
        /// captured at the same `StartComPrimitive` call time as `protocol_id`
        /// but never affected by `software-isotp` mode's hardware-channel
        /// substitution (ADR-046) the way `protocol_id` is -- feeds
        /// `TimingChangeConfig::from_params`/`RcHandlingConfig::from_params`
        /// in `handle_send_recv`, which must gate on the LOGICAL protocol
        /// (edge-case-hunter finding, PR #22: `protocol_id` alone left
        /// `timing_cfg` permanently `None` for a `software-isotp`-mode
        /// ISO15765 CLL's ordinary `CoptSendrecv` -- the exact primitive a
        /// client uses to drive ADR-150's UDS exchange).
        logical_protocol: ChannelProtocol,
        /// ADR-157 Plane B normalization: `LogicalLinkState::base_hw_protocol_id()`
        /// captured at the same call time as `protocol_id`.
        ///
        /// **ADR-158 correction:** originally added because the
        /// `ParamBinding::Temp` hardware apply/revert
        /// (`apply_params_to_hardware`/`revert_hardware_to_live_active` in
        /// `handle_send_recv`) needed a base id for its ComParam-support
        /// decision -- that premise turned out to be wrong (see ADR-158's
        /// Corrections section): both now self-normalize from the raw
        /// `protocol_id` instead, uniformly with the rest of the
        /// ComParam-translation pipeline, so `handle_send_recv` no longer
        /// reads this field for any decision. Kept (not removed) since
        /// carrying it through the cyclic-continuation re-enqueue is
        /// simpler and lower-risk than a field-removal sweep across this
        /// variant, `SendRecvCycle`, and `rpc_start_com_primitive`'s
        /// construction site -- not itself evaluated as part of this
        /// correction (out of scope; flagged for a future cleanup pass).
        base_protocol_id: u32,
        /// Fully resolved, eagerly, by `rpc_start_com_primitive` at call
        /// time (ADR-067) -- reused as-is for every cycle of a cyclic send.
        tx: SendRecvTx,
        /// The ComParam snapshot this COP bound at `StartComPrimitive`
        /// call time (ADR-067): `Plain(active)` for an ordinary
        /// `CoptSendrecv`, `Temp { effective }` when `temp_param_update`
        /// was set. Drives the temporary hardware push (and the revert to
        /// the live Active set) around each cycle's transmit and the
        /// response-phase `CP_P2Max`/RC21/23/78 config, for every cycle
        /// including follow-ups of a cyclic send.
        binding: ParamBinding,
        /// Expected-response descriptors. Whether the receive phase runs at
        /// all is governed by `num_receive_cycles`, not by this being empty
        /// (ADR-058).
        expected_response: Vec<ExpectedResponse>,
        /// `PDU_COP_CTRL_DATA.Time` (ms): the cycle time between cyclic
        /// sends.  `0` = re-enqueue the follow-up cycle at the back of the
        /// TX queue after each completion cycle (lower priority than other
        /// queued ComPrimitives) instead of scheduling it by time (ADR-053).
        /// Not the response timeout — that comes from `CP_P2Max`.
        cycle_time_ms: u32,
        /// Send cycles still to perform (`PDU_COP_CTRL_DATA.NumSendCycles`,
        /// decremented per completed cycle); `0` = no send at all, a single
        /// non-repeating receive-only pass (ADR-059); `-1` = infinite cyclic
        /// send, ended only by `CancelComPrimitive` / disconnect.
        send_cycles_remaining: i32,
        /// `PDU_COP_CTRL_DATA.NumReceiveCycles` for each send cycle's
        /// receive phase: `0` = no response required, the phase completes
        /// immediately (ADR-058); `n > 0` = exactly `n` matching responses;
        /// `-1` (IS-CYCLIC) = receive indefinitely until cancelled; `-2`
        /// (IS-MULTIPLE) = collect every matching response until the
        /// `CP_P2Max` window closes (ADR-053).
        num_receive_cycles: i32,
        /// `LogicalLinkState.connect_generation` captured at
        /// `StartComPrimitive` call time, from the same already-consistency-
        /// checked shared ADR-067 snapshot capture `StartComm`/`StopComm`
        /// already reuse. Compared against the live value by
        /// `handle_send_recv`'s `still_on_this_channel`-style guards to
        /// detect a disconnect-then-reconnect of the same `cll_handle` onto
        /// the same shared channel since this COP was accepted (ADR-086). A
        /// cyclic follow-up cycle copies this field through unchanged
        /// (ADR-086: one COP is tied to one connection generation for its
        /// entire cyclic lifetime, never recaptured for a continuation).
        connect_generation: u64,
    },
    /// Start-communication sequence (`CoptStartcomm`).
    ///
    /// Every ComParam-dependent element of this COP (tester-present message
    /// content/interval/tx_flags/addressing, ISO-TP framing, fast-init frame
    /// addressing/tx_flags/header) is resolved once, eagerly, by
    /// `rpc_start_com_primitive` at `StartComPrimitive` call time (ADR-067,
    /// reverting ADR-066's live-at-execution deferral). The UniqueRespIdTable
    /// snapshot the resolution consulted is not re-read at execution time --
    /// there is nothing left to resolve there. The fast-init KWP header
    /// construction (`fast_init`'s `WithRequest` frame) is call-time
    /// resolution in the same sense, per ADR-075/ADR-077.
    ///
    /// The poll task (`handle_start_comm`):
    /// 1. When `binding` is `Temp` (`temp_param_update` set): pushes
    ///    `effective` to hardware via `apply_params_to_hardware`, for the
    ///    duration of the init transaction only.
    /// 2. Performs the J2534 init sequence when `five_baud` is `Some` or
    ///    `fast_init` is `Some` (ADR-076/ADR-077: both are call-time
    ///    decisions -- `handle_start_comm` never re-derives which sequence
    ///    applies), using the already-resolved `init_tx_flags` and, for
    ///    fast-init, `fast_init`. Alternatively, on a non-K-line (CAN/J1850)
    ///    link, runs the optional one-shot transmit(+receive) phase from
    ///    `tx` (ADR-111, ISO 22900-2 §9.2.6.3.2 b)/Table 7 step 5) -- the
    ///    two are mutually exclusive by construction (`tx` is `Some` only
    ///    when `five_baud`/`fast_init` are both `None`).
    /// 3. When `binding` is `Temp`: reverts hardware to the live Active set
    ///    (read at revert time) before proceeding -- on both init success
    ///    and init failure (finally-style, ADR-067).
    /// 4. Starts the periodic tester-present message, from the
    ///    already-resolved `tester_present` -- always built from the
    ///    call-time Active snapshot (never Working, regardless of `binding`,
    ///    claim 8) -- the tester-present message is a persistent product of
    ///    this COP, not part of the transient init transaction.
    /// 5. Updates `LogicalLinkState.comm_started` and emits
    ///    `PduCllstCommStarted` -- unconditionally after the one-shot `tx`
    ///    phase, even if its optional receive timed out (ADR-111): only a TX
    ///    failure (never reaching the bus) or a cancellation skips this.
    StartComm {
        cop_handle: u32,
        cll_handle: u32,
        protocol_id: u32,
        /// ADR-157 Plane B normalization: `LogicalLinkState::base_hw_protocol_id()`
        /// captured at the same call time as `protocol_id`.
        ///
        /// **ADR-158 correction:** originally added (mirroring
        /// `TxItem::SendRecv::base_protocol_id`) because the
        /// `ParamBinding::Temp` hardware apply/revert
        /// (`apply_params_to_hardware`/`revert_hardware_to_live_active` in
        /// `handle_start_comm`) needed a base id for its ComParam-support
        /// decision -- that premise turned out to be wrong (see ADR-158's
        /// Corrections section): both now self-normalize from the raw
        /// `protocol_id` instead. This field's remaining live consumer in
        /// `handle_start_comm` is `header_footer_len` (a genuinely
        /// base-id-keyed, unrelated protocol-family decision for fast-init
        /// response splitting -- see the comment at its call site). Every
        /// other use of `protocol_id` in this variant/`handle_start_comm`
        /// keeps using the raw `protocol_id`.
        base_protocol_id: u32,
        /// The periodic tester-present message, resolved once, eagerly, from
        /// the call-time Active snapshot regardless of `temp_param_update`
        /// (ADR-067 claim 8: the periodic message outlives the transient
        /// init transaction, so it must never be built from Working even
        /// when `binding` is `Temp`).
        tester_present: rpc_primitive::ResolvedTesterPresent,
        /// K-line fast-init wakeup frame TxFlags, resolved eagerly from
        /// `binding.resolved()` (Working when `temp_param_update` was set) --
        /// scoped entirely to the init step, unlike `tester_present` above.
        init_tx_flags: u32,
        /// `Some` when `cop_data` was non-empty at `StartComPrimitive` call
        /// time AND the link's protocol does NOT require an init sequence
        /// (CAN/J1850) -- mutually exclusive with `five_baud`/`fast_init`,
        /// which are `Some` only for K-line links. Resolved once, eagerly,
        /// from `binding.resolved()` (Working when `temp_param_update` was
        /// set, Active otherwise -- unlike `CoptStopcomm`'s always-Active
        /// resolution, since `CoptStartcomm`'s `Temp` binding is genuinely
        /// pushed to hardware for the duration of this transaction).
        /// Carries the one-shot transmit plus optional receive-phase config
        /// for the optional `CoptStartcomm` request message (ISO 22900-2
        /// §9.2.6.3.2 b), Table 7 step 5; ADR-111). `None` when `cop_data`
        /// was empty, or the link requires a K-line init sequence. Boxed
        /// for the same `clippy::large_enum_variant` reason as
        /// `OneShotCommTx::timing_cfg` above.
        tx: Option<Box<OneShotCommTx>>,
        /// Resolved 5-baud initialisation inputs (ADR-076), or `None` when
        /// this COP will not run a 5-baud init: a fast-init COP (`fast_init`
        /// is `Some`), an `InitSequence::None` COP, or a non-K-line link.
        /// See [`FiveBaudInit`] for the two call-time resolution paths (spec
        /// vs. legacy heuristic) that populate this.
        five_baud: Option<FiveBaudInit>,
        /// Resolved fast-init dispatch (ADR-075/ADR-077), built eagerly at
        /// `StartComPrimitive` call time from `binding.resolved()` and the
        /// UniqueRespIdTable snapshot -- the same call-time resolution as
        /// `init_tx_flags`. `None` when `five_baud` is `Some`, `cop_data` is
        /// empty and `CP_InitializationSettings` is not explicitly `2` (skip
        /// init, the legacy-compatible default), or the link's protocol is
        /// not K-line (ISO9141/ISO14230). See [`FastInit`] for the
        /// `WakeupOnly` / `WithRequest` distinction; `handle_start_comm` uses
        /// this in place of `five_baud` whenever it is `Some` (the two are
        /// call-time-exclusive, never both populated for the same COP).
        fast_init: Option<FastInit>,
        /// The ComParam snapshot this COP bound at `StartComPrimitive`
        /// call time (ADR-067): `Plain(active)` for an ordinary
        /// `CoptStartcomm`, `Temp { effective }` when `temp_param_update`
        /// was set -- brackets the init transaction with a hardware push of
        /// `effective` then a revert to the live Active set.
        /// Boxed (clippy `large_enum_variant`): this is the variant that
        /// pushed `TxItem` over the lint's size threshold once
        /// `OneShotCommTx` grew a `CP_EnableConcatenation` field (ADR-148) --
        /// no behavior change, `binding` is still owned by this variant.
        binding: Box<ParamBinding>,
        /// `LogicalLinkState.connect_generation` captured at
        /// `StartComPrimitive` call time, after the comm_started-already-
        /// started precondition check (so it reflects the connection this
        /// COP is actually being accepted against). Compared against the
        /// live value by `handle_start_comm`'s `still_on_this_channel`-style
        /// guards to detect a disconnect-then-reconnect of the same
        /// `cll_handle` onto the same shared channel since this COP was
        /// accepted (ADR-086).
        connect_generation: u64,
    },
    /// Stop-communication sequence (`CoptStopcomm`).
    ///
    /// The poll task stops the periodic tester-present message (if active),
    /// transmits `tx.send` (when `tx` is `Some`) as a final message
    /// (ADR-085), then -- when `tx.num_receive_cycles != 0` -- runs a
    /// bounded, non-cancellable receive phase against `tx.expected_response`
    /// (ADR-087), before clearing `LogicalLinkState.comm_started` and
    /// emitting `PduCllstOnline`/`PduCopstFinished`. Not universally
    /// "fire-and-forget" any more: only `tx: None` (empty `cop_data`) or
    /// `tx.num_receive_cycles == 0` skip the receive phase.
    StopComm {
        cop_handle: u32,
        cll_handle: u32,
        protocol_id: u32,
        /// `Some` when `cop_data` was non-empty at `StartComPrimitive` call
        /// time: resolved once, eagerly, exactly like `CoptSendrecv`'s `tx`
        /// (ADR-085), always against the call-time Active snapshot (never
        /// Working -- `temp_param_update` remains a no-op for `CoptStopcomm`,
        /// ADR-044/ADR-067 unchanged). Since ADR-087, also carries the
        /// optional receive-phase config (`expected_response`,
        /// `num_receive_cycles`, `response_timeout_ms`, `rc_cfg`) resolved
        /// from the same snapshot. `None` preserves the pre-ADR-085
        /// behavior: no transmit, no receive, TX-queue-lock-exempt. Boxed
        /// for the same `clippy::large_enum_variant` reason as
        /// `OneShotCommTx::timing_cfg` above.
        tx: Option<Box<OneShotCommTx>>,
        /// `LogicalLinkState.connect_generation`, captured atomically with
        /// the `stop_comm_pending = true` test-and-set in the same critical
        /// section (`rpc_start_com_primitive`'s `CoptStopcomm` branch).
        /// Compared against the live value by `handle_stop_comm`'s
        /// `still_on_this_channel`-style guards to detect a disconnect-then-
        /// reconnect of the same `cll_handle` onto the same shared channel
        /// since this COP was accepted (ADR-086).
        connect_generation: u64,
    },
    /// Timed delay (`CoptDelay`).
    ///
    /// The poll task sleeps for `delay_ms` milliseconds before dequeuing the
    /// next item, serialising the wait with all other channel operations.
    Delay {
        cop_handle: u32,
        cll_handle: u32,
        /// Delay duration in milliseconds (from `ComPrimitiveCtrlData.time`).
        delay_ms: u32,
        /// `LogicalLinkState.connect_generation` captured at
        /// `StartComPrimitive` call time, from the same already-consistency-
        /// checked shared ADR-067 snapshot capture `StartComm`/`StopComm`/
        /// `SendRecv`/`UpdateParam` already reuse. Compared against the live
        /// value by `handle_delay`'s `still_on_this_channel`-style guard to
        /// detect a disconnect-then-reconnect of the same `cll_handle` onto
        /// the same shared channel since this COP was accepted (ADR-086
        /// round 11).
        connect_generation: u64,
    },
    /// Apply a call-time Working snapshot to the adapter and, on success,
    /// promote it to Active (`CoptUpdateparam`).
    ///
    /// `params` is `LogicalLinkState.working.clone()` taken by
    /// `rpc_start_com_primitive` at `StartComPrimitive` call time (ADR-067
    /// claim F) -- a `SetComParam` issued after this call must not be
    /// promoted by it. The poll task forwards all J2534-standard params in
    /// `params` to the hardware via `PassThruIoctl SET_CONFIG` and, only on
    /// success, sets `LogicalLinkState.active = params` (in-memory Active
    /// still updates at execution time, unchanged from before ADR-067; only
    /// the Working *read* moved to call time).
    ///
    /// `unique_resp_id_table` is `LogicalLinkState.working_unique_resp_id_table.clone()`,
    /// taken at the same call time for the same reason (ADR-068, mirroring
    /// ADR-067 claim F for the table). Only on hardware success does the poll
    /// task promote it to `LogicalLinkState.active_unique_resp_id_table` via
    /// `J2534Service::promote_unique_resp_id_table`, which also reconciles
    /// ISO15765 `FLOW_CONTROL_FILTER`s -- diff-gated, so a com-param-only
    /// `CoptUpdateparam` (identical table) does no filter I/O.
    UpdateParam {
        cop_handle: u32,
        cll_handle: u32,
        params: ComParamSet,
        unique_resp_id_table: Vec<EcuUniqueRespEntry>,
        /// `LogicalLinkState.connect_generation` captured at
        /// `StartComPrimitive` call time, from the same already-consistency-
        /// checked shared ADR-067 snapshot capture `StartComm`/`StopComm`
        /// already reuse. Compared against the live value by
        /// `handle_update_param`'s `still_on_this_channel`-style guards to
        /// detect a disconnect-then-reconnect of the same `cll_handle` onto
        /// the same shared channel since this COP was accepted (ADR-086).
        connect_generation: u64,
    },
    /// Restore the Working param set from the Active snapshot
    /// (`CoptRestoreParam`).
    ///
    /// The poll task copies `LogicalLinkState.active` → `working`, and
    /// `LogicalLinkState.active_unique_resp_id_table` →
    /// `working_unique_resp_id_table` (ADR-068). No hardware calls are made
    /// for either.
    RestoreParam {
        cop_handle: u32,
        cll_handle: u32,
        /// `LogicalLinkState.connect_generation` captured at
        /// `StartComPrimitive` call time, from the same already-consistency-
        /// checked shared ADR-067 snapshot capture `StartComm`/`StopComm`/
        /// `SendRecv`/`UpdateParam` already reuse. Compared against the live
        /// value by `handle_restore_param`'s `still_on_this_channel`-style
        /// guard to detect a disconnect-then-reconnect of the same
        /// `cll_handle` onto the same shared channel since this COP was
        /// accepted (ADR-086 round 11).
        connect_generation: u64,
    },
    /// Content-free wake sent by `PDU_IOCTL_RESUME_TX_QUEUE` (Codex-review
    /// fix) after clearing `tx_suspended`, purely to guarantee the poll
    /// loop dequeues *something* for this CLL soon so its `tx_held` backlog
    /// (drained by the loop the moment any item for this CLL is dequeued --
    /// see `drain_tx_held_backlog` in events.rs) doesn't strand forever when
    /// the client sends nothing further. Never dispatched/executed itself --
    /// intercepted and discarded by the poll loop before reaching
    /// `dispatch_tx_item`.
    ResumeWake { cll_handle: u32 },
}

impl TxItem {
    /// Returns `(cop_handle, cll_handle)` for this item.
    pub(super) fn handles(&self) -> (u32, u32) {
        match self {
            Self::SendRecv {
                cop_handle,
                cll_handle,
                ..
            } => (*cop_handle, *cll_handle),
            Self::StartComm {
                cop_handle,
                cll_handle,
                ..
            } => (*cop_handle, *cll_handle),
            Self::StopComm {
                cop_handle,
                cll_handle,
                protocol_id: _,
                tx: _,
                connect_generation: _,
            } => (*cop_handle, *cll_handle),
            Self::Delay {
                cop_handle,
                cll_handle,
                ..
            } => (*cop_handle, *cll_handle),
            Self::UpdateParam {
                cop_handle,
                cll_handle,
                ..
            } => (*cop_handle, *cll_handle),
            Self::RestoreParam {
                cop_handle,
                cll_handle,
                ..
            } => (*cop_handle, *cll_handle),
            Self::ResumeWake { cll_handle } => (0, *cll_handle),
        }
    }

    /// Returns the `connect_generation` this item was captured against at
    /// `StartComPrimitive` call time, for the variants that carry one
    /// (`StartComm`/`StopComm`/`SendRecv`/`UpdateParam`/`Delay`/
    /// `RestoreParam`, ADR-086; `Delay`/`RestoreParam` added round 11). `None`
    /// for every other variant. Used by `should_skip_cancelled_item` (round 5)
    /// to detect an item that went stale -- its captured generation no longer
    /// matches the live one -- while still sitting in the queue.
    ///
    /// `ResumeWake` is structurally exempt (round 11): it is intercepted by
    /// the poll loop before `dispatch_tx_item` is ever called, is
    /// content-free, and has zero side effects of its own -- there is nothing
    /// for a `connect_generation` field to protect.
    pub(super) fn connect_generation(&self) -> Option<u64> {
        match self {
            Self::StartComm {
                connect_generation, ..
            } => Some(*connect_generation),
            Self::StopComm {
                connect_generation, ..
            } => Some(*connect_generation),
            Self::SendRecv {
                connect_generation, ..
            } => Some(*connect_generation),
            Self::UpdateParam {
                connect_generation, ..
            } => Some(*connect_generation),
            Self::Delay {
                connect_generation, ..
            } => Some(*connect_generation),
            Self::RestoreParam {
                connect_generation, ..
            } => Some(*connect_generation),
            Self::ResumeWake { .. } => None,
        }
    }

    /// Whether this item actually transmits on the physical bus, for
    /// `LOCK_PHYSICAL_TX_QUEUE`-driven suspension (ADR-123): `SendRecv` is
    /// transmitting iff `send_cycles_remaining != 0` -- `0` means a single
    /// non-repeating receive-only monitoring pass with no bus write at all
    /// (ADR-059, ADR-123 Fix H), while `-1` (infinite cyclic send) and any
    /// positive count both transmit; `StartComm` always transmits, kept
    /// deliberately conservative rather than given the same conditional
    /// treatment -- a no-init StartComm with a configured tester-present
    /// would otherwise execute mid-lock and start new periodic bus traffic
    /// via `StartPeriodicMsg`, a case ADR-123's existing TesterPresent
    /// accepted-residual does not cover (that residual is scoped to an
    /// already-running tester-present bypassing suspension, not one
    /// starting fresh during a sibling's held lock); `StopComm` iff its
    /// `tx` is `Some` (ADR-085's empty-data exemption); `Delay`/`UpdateParam`/
    /// `RestoreParam`/`ResumeWake` never do. `ResumeWake` never actually
    /// reaches the siphon this classification feeds (intercepted earlier in
    /// `dispatch_tx_item`'s caller) -- included here for completeness/defense
    /// in depth, not because it's reachable.
    pub(super) fn transmits(&self) -> bool {
        match self {
            Self::SendRecv {
                send_cycles_remaining,
                ..
            } => *send_cycles_remaining != 0,
            Self::StartComm { .. } => true,
            Self::StopComm { tx, .. } => tx.is_some(),
            Self::Delay { .. }
            | Self::UpdateParam { .. }
            | Self::RestoreParam { .. }
            | Self::ResumeWake { .. } => false,
        }
    }
}

mod can_mode;
mod comparam_defaults;
mod comparam_id;
mod comparam_support;
mod discovery;
mod events;
mod isotp;
mod names;
mod protocol;
mod resources;
mod rpc_link;
mod rpc_misc;
mod rpc_module;
mod rpc_primitive;
mod tx_header;

pub(super) use can_mode::CanChannelMode;
pub(super) use comparam_id::{
    CANFD_TX_MAX_DATA_LENGTH_ACCEPTED, ComParamId, UART_CONFIG_ACCEPTED, expand_tidle,
    expand_uart_config, fd_can_padded_data_len, is_valid_block_size_override,
    is_valid_canfd_tx_max_data_length, is_valid_init_settings, is_valid_parity,
    is_valid_uart_config, to_j2534_config_value, uart_config_to_parity, us_to_ms,
};
pub(super) use protocol::ChannelProtocol;

type EventStream =
    Pin<Box<dyn Stream<Item = Result<vci_service_interface::EventNotification, Status>> + Send>>;

pub(crate) const DEFAULT_MODULE_HANDLE: u32 = 1;
const PDU_HANDLE_UNDEF: u32 = 0xFFFF_FFFF;
/// PDU_ID_UNDEF (0xFFFFFFFE) — sentinel for an unassigned unique-response identifier.
///
/// Distinct from PDU_HANDLE_UNDEF (0xFFFFFFFF) which is reserved for handles.
/// ISO 22900-2 §9.3.3.6: GetUniqueRespIdTable returns a single template entry
/// with UniqueRespIdentifier = PDU_ID_UNDEF when no table has been set yet.
const PDU_ID_UNDEF: u32 = 0xFFFF_FFFE;

/// Subscription map key for system-handle `SubscribeEvent` callers.
///
/// `0` is never assigned as a module or CLL handle (both allocators skip 0),
/// so `(0, 0)` is guaranteed not to collide with any CLL or module-level key.
pub(super) const SYSTEM_SUBSCRIPTION_KEY: SubscriptionKey = (0, 0);

/// Key for the shared J2534 channel table: `(hw_protocol_id, baud_rate,
/// pin_select)`.
///
/// Uses the J2534 hardware protocol ID (not the service-level `ChannelProtocol` value)
/// so that CLLs using different service-level protocols that share the same physical
/// J2534 channel type (e.g. ISO_15765_3_ON_ISO_15765_2 and ISO_14229_3_ON_ISO_15765_2,
/// both of which use ISO15765 = 0x06) map to the same `ChannelKey` and share a
/// single physical J2534 channel.
///
/// The third element (ADR-156 Decision 2, partially superseding ADR-023's
/// original 2-tuple shape) is `LogicalLinkState::pin_select.unwrap_or(0)` --
/// `0` for every non-`_PS` link, which reproduces the pre-Phase-2a 2-tuple's
/// exact sharing behavior for every existing protocol (two links with
/// differing pin selections on the same `_PS` hardware id must NOT share a
/// physical channel; every other case is unaffected).
///
/// The fourth element (ADR-158 Correction, Codex review on PR #30) is the
/// link's effective CAN FD data-phase rate -- `CP_CANFDBaudrate` if nonzero,
/// else `DATA_RATE` (exactly the value `rpc_link.rs`'s `fd_data_phase_rate`
/// local already computes), `0` for every non-FD link, mirroring the third
/// element's own "`0` for every non-`_PS` link" convention. The data-phase
/// rate is fixed at connect time exactly like `baud_rate` (the second
/// element) already is, so two `FD_CAN_PS` links agreeing on protocol/baud/
/// pins but disagreeing on data-phase rate must NOT share a physical
/// channel -- otherwise only the creator's rate is ever applied to hardware
/// and a joiner silently runs at a rate it never actually configured.
pub(super) type ChannelKey = (u32, u32, u32, u32);

/// Cache key for `J2534Service::discovery_device_info`: `(module_handle,
/// parameter, input_value)` (ADR-153 correction, Codex review on PR #25;
/// widened with `input_value` by ADR-185 Stage 1). `module_handle` is
/// load-bearing, not just documentation: without it, a cache hit for one
/// module could serve another module's cached answer whenever the epoch
/// happens to still match (e.g. querying module A's capabilities while
/// module B is the one actually open) -- entirely bypassing
/// `ensure_open_device_for`'s resource-busy check, which only runs on a
/// miss. `input_value` is load-bearing for the same reason: without it, a
/// query for one per-pin parameter value (clause 15/25.3.2.2's
/// `DEVICE_INFO_SHORT_TO_GND_J1962`/`DEVICE_INFO_PGM_VOLTAGE_J1962`) could be
/// served a different pin's cached answer.
pub(super) type DiscoveryDeviceKey = (u32, u32, u32);

/// Cache key for `J2534Service::discovery_protocol_info`: `(module_handle,
/// protocol_id, parameter)` (ADR-153 correction, Codex review on PR #25) --
/// see `DiscoveryDeviceKey`'s doc comment for why `module_handle` must be
/// part of the key, not just the epoch.
pub(super) type DiscoveryProtocolKey = (u32, u32, u32);

/// One epoch-tagged `GET_DEVICE_INFO`/`GET_PROTOCOL_INFO` cache entry (ADR-153) --
/// see `J2534Service::discovery_device_info`'s doc comment for the epoch-invalidation rule.
pub(super) type DiscoveryCacheEntry = (u64, j2534_0404::DiscoveryResult);

/// One `SharedChannel::j1939_claims` entry (ADR-180 Decision 23, round-25
/// correction, `design-advisor` consult, Codex review PR #72): the owning
/// CLL, its `connect_generation` at claim-registration time, and -- new in
/// this correction -- the SAE J1939 NAME (`CP_J1939Name`) this claim was
/// actually issued under. A named struct, not a bare tuple, so every call
/// site's field access is self-documenting and the type cannot silently
/// grow a positional-field mixup as more attributes accrue. `Copy + Eq` so
/// existing `.copied()`/equality-comparison call sites keep working
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct J1939ClaimEntry {
    pub(super) cll_handle: u32,
    pub(super) connect_generation: u64,
    pub(super) name: [u8; 8],
}

/// ADR-180 Decision 23 round-26 correction (design-advisor consult, Codex
/// review PR #72 round 26): the record armed by `deliver_j1939_claim_
/// indication`'s spontaneous-loss arm for a pending SAE J1939 reclaim.
/// Carries forward the owning CLL's `connect_generation` (same staleness
/// guard `J1939ClaimEntry` already uses) and the NAME the just-lost claim
/// was actually issued under, captured at the moment of loss -- the
/// authoritative `j1939_claims` entry (the only other place this NAME
/// lives) is removed in the same critical section this record is armed
/// in, so without carrying it forward here a later reclaim would have no
/// sound source for "the NAME this claim defends" and would fall back to
/// re-reading (possibly stale, possibly Temp-diverged) Active instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct J1939ReclaimPending {
    pub(super) connect_generation: u64,
    pub(super) name: [u8; 8],
}

/// A physical J2534 channel shared by one or more `ComLogicalLink`s that have
/// the same `(protocol_id, baud_rate)`.  Released when `ref_count` reaches zero.
struct SharedChannel {
    channel_id: ChannelId,
    /// Number of `ComLogicalLink`s currently connected to this channel.
    ref_count: u32,
    /// Sends outbound operations to the poll task for ordered async execution.
    tx_queue: mpsc::UnboundedSender<TxItem>,
    /// COP handle currently being executed by the poll task, or `None` when the
    /// task is idle.  Shared with `GetStatus(cop)` to distinguish
    /// `PduCopstExecuting` (actively running) from `PduCopstWaiting` (queued).
    executing_cop: Arc<Mutex<Option<u32>>>,
    /// Signals the poll task to stop.  Held only for its Drop side effect:
    /// dropping this sender (when `ref_count` hits zero) cancels the poll
    /// task's `cancel` receiver, which is otherwise never read.
    _poll_cancel: oneshot::Sender<()>,
    /// The `PassThruConnect` flags this channel was connected with (ADR-065).
    /// Persisted so `CLEAR_MSG_FILTERS` can rebuild the same per-ID-type
    /// `PASS_FILTER`s via `install_pass_all_filter` instead of guessing.
    connect_flags: u32,
    /// ADR-134 Correction (Codex review round 6, PR #149): set by
    /// `events::handle_channel_hard_error`, under the same `shared_channels`
    /// guard as its per-CLL teardown, for the entry matching the channel
    /// that just hard-errored. This channel's poll task is exiting (or has
    /// already exited) and will never service RX/TX again, but the entry
    /// itself is deliberately NOT removed from `shared_channels` here (only
    /// an explicit `Disconnect`/`Destroy` on a CLL that still references
    /// this `channel_key` releases it via `release_shared_channel_ref` --
    /// removing it here instead would let a fresh connect reinsert at the
    /// same key, after which that stale CLL's later teardown would decrement
    /// -- and potentially disconnect -- the NEW channel out from under a
    /// live CLL). Both existing-channel-join branches
    /// (`rpc_connect_com_logical_link`, `ensure_uudt_companion_channel`)
    /// reject a `dead` entry instead of reusing its `channel_id`, since
    /// doing so would publish a CLL as `connected` on a channel with no
    /// live poll task to ever service it -- a permanent, silent hang, not
    /// something the module going `NotAvail` (this ADR's own gates) can
    /// self-heal, unlike a brand-new channel which fails its own first RX
    /// poll if the device is genuinely dead.
    dead: bool,
    /// Occupancy epoch (ADR-161): a `u64` drawn from
    /// `J2534Service::next_occupancy_epoch`, stamped at creation and
    /// re-stamped on every `ref_count` increment (both the primary-connect
    /// join and the UUDT-companion join), always under this entry's
    /// `shared_channels` critical section. Never bumped on decrement --
    /// releasing an occupant cannot introduce a session `PDU_IOCTL_RESET`
    /// never observed. `ioctl_reset`'s Phase 1 compares its snapshotted
    /// value of this field against the live value to detect a sibling CLL
    /// joining this channel between the snapshot and Phase 1's teardown,
    /// a hazard the per-target `connect_generation` recheck alone cannot
    /// see (a UUDT-companion join never changes `connect_generation`).
    occupancy_epoch: u64,
    /// SAE J2534-2 clause 14 Repeat Messaging (ADR-165 Decision 6, Codex
    /// review PR #42 round 2 Finding A): `MsgId`s whose best-effort STOP
    /// failed during a CLL's `DestroyComLogicalLink`/`DisconnectComLogicalLink`
    /// teardown while this channel stayed open for a sibling CLL -- unlike
    /// `client_filters` (ADR-082: always the sole channel owner when
    /// non-empty), a repeat slot has no such guarantee, so a failed STOP on
    /// a channel that is NOT about to close cannot rely on the imminent
    /// `PassThruDisconnect` to clean it up. Retried opportunistically the
    /// next time any CLL on this channel touches `ioctl_start_repeat_message`/
    /// `ioctl_query_repeat_message`/`ioctl_stop_repeat_message` (see
    /// `J2534Service::retry_leaked_repeat_message_stops`), and as a final
    /// backstop right before this entry is removed from `shared_channels`
    /// when `ref_count` hits 0 -- at that point `PassThruDisconnect` is
    /// about to tear the whole channel (and every repeat slot on it) down
    /// anyway, so any still-leaked `MsgId`s are simply dropped (with a debug
    /// log) rather than retried. Every push site has a matching retry/prune
    /// site, so this never grows unboundedly across the channel's lifetime --
    /// **except** the third push site `events::handle_channel_hard_error`
    /// adds (edge-case-hunter finding, same PR): that push always targets an
    /// already-`dead` entry, and if the hard-errored CLL later reconnects to
    /// a *different* `ChannelKey` without an intervening Disconnect/Destroy
    /// (nothing prevents this once `connected == false`), this entry's own
    /// `ref_count` never reaches 0 again -- no code path revisits this exact
    /// key for that CLL -- so it, and whatever it still carries here, is
    /// never removed. See the Prioritized Backlog for the accepted-residual writeup (bounded to a
    /// same-process `HashMap`/`Vec` leak, never device- or client-visible)
    /// and the broader, pre-existing `finalize_connected_link` ref-count gap
    /// this new push site's residual sits on top of.
    ///
    /// As of ADR-180 Decision 20 (Codex review PR #72 round 20), this field
    /// also gains entries from a still-live SAE J1939 CLL's own
    /// relinquishment-triggered `stop_repeat_slots_for_cll` STOP failure
    /// (`events_j1939_claim.rs`'s `push_leaked_repeat_slots`/
    /// `record_leaked_repeat_slots`) -- not only CLL teardown/hard-error as
    /// above. Same retry/prune treatment via
    /// `retry_leaked_repeat_message_stops`; a duplicate push is avoided by
    /// checking membership first, since a concurrent CLL teardown can race
    /// the same `MsgId` onto this list via both paths.
    leaked_repeat_message_ids: Vec<u32>,
    /// SAE J2534-2 clause 19.3.2.3 TP2.0 broadcast periodic re-trigger
    /// (ADR-192/Phase 7 Stage 7c Fix B, design-advisor consult, Codex review
    /// round 3): mirrors `leaked_repeat_message_ids` just above exactly, one
    /// physical resource over -- `PeriodicMessageId`s whose best-effort STOP
    /// failed during a CLL's `DisconnectComLogicalLink`/`DestroyComLogicalLink`
    /// teardown (`rpc_link.rs`) while this channel stayed open for a sibling
    /// CLL, or during `events::handle_channel_hard_error`'s dead-link drain
    /// (no native stop attempted there -- the channel is already dead).
    /// Channel identity is implicit in this owning `SharedChannel` entry,
    /// exactly like `leaked_repeat_message_ids` -- no protocol/channel-id
    /// pair needed on the entry itself. Dup-checked pushes (`.contains()`
    /// before pushing, mirroring `events_j1939_claim.rs`'s own push-site
    /// guard) so a concurrent teardown racing the same message onto this list
    /// twice cannot double-push.
    ///
    /// Unlike `leaked_repeat_message_ids`, retry and prune are ONE function,
    /// not a split pair: the native periodic-message API has no QUERY
    /// primitive, so the retry's own `stop_periodic_message` attempt IS the
    /// liveness probe (`rpc_link.rs::retry_leaked_periodic_message_stops`).
    /// Retried opportunistically by `rpc_lock_resource`'s
    /// `LOCK_PHYSICAL_TX_QUEUE` grant scan (the same `chans`-scanned loop
    /// that already checks `leaked_repeat_message_ids`), and, as a final
    /// backstop, debug-logged and dropped when this `SharedChannel` entry is
    /// removed at `ref_count == 0`, the same shape
    /// `leaked_repeat_message_ids`' own backstop uses (this entry's own
    /// `handle_channel_hard_error` push site inherits the same accepted
    /// residual `leaked_repeat_message_ids`' doc comment above records for a
    /// hard-errored CLL reconnecting to a different `ChannelKey` without an
    /// intervening Disconnect/Destroy -- see that comment for the full
    /// shape, unchanged here).
    ///
    /// Each entry pairs the id with the `periodic_clear_epoch` value its
    /// native start captured (`Tp20BroadcastPeriodic::started_epoch`'s own
    /// doc comment explains the epoch itself). `CLEAR_PERIODIC_MSGS`'s
    /// success path no longer clears this list outright: it `retain`s only
    /// entries whose epoch is `>= clear_generation` (this clear's own
    /// post-increment value) -- an entry that started before this clear's
    /// native call is moot, since the channel-wide native clear already
    /// covered it, while an entry that started at or after it is a genuine
    /// post-clear leak this clear never touched and must stay tracked.
    leaked_periodic_message_ids: Vec<(j2534_0404::PeriodicMessageId, u64)>,
    /// SAE J2534-2 clause 10 Analog Inputs (ADR-178): the CONFIG_SAMPLE_RATE
    /// value actually applied to this physical channel via SET_CONFIG at
    /// connect time, `Some(rate)` iff this channel was opened for one of the
    /// 32 native `PROTOCOL_ANALOG_IN_x` ids, `None` for every other channel.
    /// Recorded once, at channel creation, from the SAME Working-snapshot
    /// value `connect_new_physical_channel` applied to hardware -- never a
    /// fresh independent read -- so it cannot race a concurrent `SetComParam`
    /// re-staging the same CLL's Working set. The join-mismatch check
    /// (`rpc_connect_com_logical_link`) and the `CoptUpdateparam` guard
    /// (`rpc_primitive.rs`) both compare against THIS recorded value, never
    /// against any CLL's live per-CLL/Working state, because a staged
    /// ComParam is re-stageable post-connect while this recorded value is
    /// not -- comparing against live state would let an owner's post-connect
    /// re-stage silently desync the check from what hardware is actually
    /// running (ADR-178).
    applied_analog_sample_rate: Option<u32>,
    /// SAE J2534-2 clause 10 Analog Inputs (ADR-216 Decision item 10 fix):
    /// the CONFIG_SAMPLES_PER_READING value actually applied to this
    /// physical channel via SET_CONFIG at connect time, mirroring
    /// `applied_analog_sample_rate` exactly (same `Some(value)`/`None`
    /// gating, same connect-time snapshot recording, same non-live-state
    /// comparison rationale) -- see that field's own doc comment above for
    /// the full reasoning, which applies identically here. Compared against
    /// by the `CoptUpdateparam` guard (`rpc_primitive.rs`) to reject a
    /// genuine re-stage attempt while letting a same-value re-stage, or an
    /// unrelated update (e.g. `CP_AnalogAveragingMethod` alone), through.
    applied_analog_samples_per_reading: Option<u32>,
    /// SAE J2534-2 clause 10 Analog Inputs (ADR-216 Decision item 10 fix):
    /// the CONFIG_READINGS_PER_MSG value actually applied to this physical
    /// channel via SET_CONFIG at connect time -- the `CP_AnalogReadingsPerMsg`
    /// sibling of `applied_analog_samples_per_reading` immediately above;
    /// see that field's doc comment (and `applied_analog_sample_rate`'s,
    /// which both mirror) for the full reasoning.
    applied_analog_readings_per_msg: Option<u32>,
    /// SAE J2534-2 clause 16 SAE J1939 address-claim routing (ADR-179
    /// Decision 3): `address -> J1939ClaimEntry { owning cll_handle, that
    /// CLL's connect_generation at claim time, and (ADR-180 Decision 23,
    /// round-25 correction) the NAME the claim was actually issued under }`,
    /// for every address a sibling CLL on this shared physical channel
    /// currently holds claimed OR has a claim attempt pending for. A
    /// `J1939_ADDRESS_CLAIMED`/`_LOST` indication (`events.rs`'s RxStatus
    /// routing, `is_j1939_protocol_id`-gated) carries only the affected
    /// address in `Data[0]` (SAE J2534-2 Table 63) -- since multiple sibling
    /// CLLs sharing one physical channel may each hold a distinct claim (the
    /// device must protect at least ten addresses per clause 16.2.2), this
    /// map is what resolves an indication back to the CLL whose
    /// `handle_start_comm` claim-loop (or spontaneous-loss retry) is waiting
    /// on it. Entries are tagged with the owning CLL's `connect_generation`
    /// (ADR-086) at the moment the claim attempt was registered -- no new
    /// generation-style mechanism is needed: a stale indication surviving
    /// that CLL's disconnect/reconnect simply fails the ordinary
    /// `connect_generation` comparison and is dropped, the same as every
    /// other `connect_generation`-guarded site in this codebase. Entries are
    /// inserted when a claim attempt is issued (`api.protect_j1939_addr`)
    /// and removed either when that CLL's own disconnect cancels its claims
    /// (ADR-179 Decision 3's cancellation step) or when a fresh claim
    /// attempt for the same address supersedes a stale entry.
    ///
    /// The `name` field (Decision 23) is the SOURCE OF TRUTH `run_j1939_
    /// claim_loop`'s own NAME-collision gate compares against -- it is only
    /// sound because a live entry's NAME can never drift from what the
    /// adapter actually defends: `CoptUpdateparam`'s own enqueue-time AND
    /// execution-time guards (`rpc_primitive.rs`/`events.rs`) reject any
    /// attempt to promote a DIFFERENT `CP_J1939Name` onto Active while this
    /// map shows a live claim for `(handle, connect_generation)`, and a
    /// fresh claim attempt only ever registers the NAME it actually issued
    /// the native call with (`resolve_j1939_claim_params`'s own snapshot, at
    /// `run_j1939_claim_loop`'s call-time). A separate NAME-keyed map was
    /// considered and rejected: it would need a paired insert/remove kept
    /// manually in sync with every one of this map's own mutation sites (the
    /// claim-issue insert, the Decision-2/20/21 retained-on-cancel-failure
    /// path, the spontaneous-loss removal, and `cancel_j1939_claims_for_
    /// cll`'s own teardown sweep) -- one missed pairing would permanently
    /// wedge a NAME with no cleanup owner, a new bug class in a mechanism
    /// that already took 24 rounds to stabilize. Widening this map's own
    /// value type instead keeps one source of truth.
    j1939_claims: HashMap<u8, J1939ClaimEntry>,
    /// Latest ADR-179 Decision 3 claim/defend outcome delivered for a CLL
    /// currently (or most recently) waiting on one -- `events.rs`'s
    /// `poll_rx_inner` RxStatus-withhold arm writes here (via
    /// `deliver_j1939_claim_indication`) once it has resolved an
    /// indication's affected address to a live, non-stale owning CLL
    /// (`j1939_claims` above); `run_j1939_claim_loop`'s wait step reads and
    /// clears its own `cll_handle`'s entry after each `poll_rx_inner`
    /// iteration it drives. A later write for the same `cll_handle`
    /// overwrites an unconsumed earlier one -- ADR-179 Decision 3's model
    /// has exactly one claim attempt in flight per CLL at a time, so there
    /// is never a legitimate reason to queue more than the latest outcome.
    j1939_claim_results: HashMap<u32, J1939ClaimOutcome>,
    /// `cll_handle -> J1939ReclaimPending` for a claim that was
    /// spontaneously lost (device out-defended) while no `StartComm`-driven
    /// wait was in flight for it -- set by `deliver_j1939_claim_indication`,
    /// cleared and acted on by the next `run_due_tick_duties` pass, which
    /// re-enters the claim retry loop from `j1939_claim_cursor = 0`
    /// (ADR-179 Decision 3's "later, spontaneous `_LOST`" case). Deliberately
    /// NOT reused for the ordinary in-`StartComm`-wait `_LOST` case -- that
    /// is entirely handled synchronously by `run_j1939_claim_loop`'s own
    /// retry-next-candidate step via `j1939_claim_results` above, without
    /// ever touching this map. As of ADR-180 Decision 23's round-26
    /// correction, the value also carries the owning CLL's
    /// `connect_generation` at the moment of loss and the actually-defended
    /// NAME (`J1939ClaimEntry::name` as it stood immediately before removal)
    /// so the eventual reclaim re-issues the SAME identity instead of
    /// re-reading (possibly stale or Temp-diverged) Active.
    j1939_reclaim_pending: HashMap<u32, J1939ReclaimPending>,
    /// SAE J2534-2 clause 16 SAE J1939 address-claim cancellation backstop
    /// (ADR-180 Decision 22, design-advisor consult, Codex review PR #72):
    /// addresses whose native `cancel_j1939_addr_protect` failed AFTER their
    /// `j1939_claims` routing entry was already removed -- mirrors
    /// `leaked_repeat_message_ids`' own precedent above (ADR-165 Decision 6
    /// / ADR-180 Decision 20) for the same problem shape, one level up:
    /// `cancel_j1939_claims_for_cll` (all 5 call sites -- the 2 teardown
    /// sites in `rpc_link.rs`, Decision 4's "fresh attempt" reset, Decision
    /// 9's `cancel_j1939_claim_after_failed_startcomm`, and the Decision 18
    /// opt-out branch) always relinquishes ownership of an address it is
    /// cancelling, regardless of whether the native cancel itself succeeds,
    /// since the CLL giving it up either way. A failed native cancel with
    /// the routing entry gone means the adapter may still be defending an
    /// address that now looks free to every other claimant on this shared
    /// physical channel -- including a sibling CLL, or (Decision 4's own
    /// call site) this SAME CLL's own imminent fresh claim attempt -- two
    /// claimants, one address, until reconciled. Dup-checked pushes (mirrors
    /// `push_leaked_repeat_slots`'s own `contains` guard): a concurrent
    /// cancel of the same address from a different call site must not
    /// double-track it. Reconciled opportunistically, not by a periodic
    /// duty, via the shared `reconcile_leaked_j1939_claims` helper
    /// (`events_j1939_claim.rs`), which retries a cancel for EVERY entry in
    /// this list, channel-wide -- not scoped to any one candidate address.
    /// Three call sites share it: `run_j1939_claim_loop`'s own per-iteration
    /// critical section (`events_j1939_claim.rs`, Decision 22's round-23
    /// correction), the negotiation opt-out branch in `handle_start_comm`
    /// (`events.rs`, Decision 18's round-24 correction) -- an opt-out-bound
    /// CLL never re-enters `run_j1939_claim_loop`, so without its own retry
    /// point a channel left with only opt-out CLLs would have no
    /// reconciliation owner at all for a pre-existing leak -- and
    /// `ioctl_start_repeat_message` (`rpc_misc.rs`, Decision 24, round-27
    /// correction to Decision 22) -- a CLL that opted out of negotiation
    /// entirely, or manages its own source address, sails past both of the
    /// other two owners' own gates without ever touching this list, so
    /// starting a SAE J2534-2 clause 14 repeat slot needed its own
    /// reconcile-then-gate too, since the leak belongs to the physical
    /// channel a torn-down sibling shares, not to any one CLL's own
    /// negotiation posture. The first two round corrections each replaced an
    /// earlier per-candidate-address guard that only retried an entry
    /// matching the CLL's own current candidate, which let an operation
    /// whose candidate/relinquished address was disjoint from every leaked
    /// address bypass the guard entirely; the third (Decision 24) adds a
    /// call site where no reconcile-then-gate existed at all before, for
    /// the identical disjoint-candidate reasoning. This list
    /// deliberately carries no owner/CLL/generation attribution, even under
    /// the channel-wide gate: a reconnect under a new `cll_handle` but the
    /// same NAME (clause 16's one-NAME-one-address identity) would still
    /// evade a per-owner gate, since NAME is not part of the leak record,
    /// only the address is -- see ADR-180 Decision 22's round-23 correction
    /// for the full variant analysis. Any entry still unresolved after the
    /// retry fails the whole operation closed (a fresh `Exhausted` claim
    /// attempt, or a failed-closed opt-out `StartComPrimitive`), regardless
    /// of which address the caller itself was pursuing. Pruned (with a
    /// debug log, same shape as `leaked_repeat_message_ids`' own backstop)
    /// when this `SharedChannel` entry itself is removed at `ref_count ==
    /// 0`: the channel is closing for good at that point, so any
    /// still-leaked address is moot.
    leaked_j1939_claims: Vec<u8>,
    /// SAE J2534-2 clause 19 TP2.0 connection-request routing (ADR-188/Phase
    /// 7 Stage 7a), the structural sibling of `j1939_claims` just above:
    /// `requested_rx_id -> Tp20ConnEntry { owning cll_handle, that CLL's
    /// connect_generation at request time }`, for every RX-ID a CLL on this
    /// shared physical channel currently has an `IOCTL_REQUEST_CONNECTION`
    /// pending for. A `CONNECTION_ESTABLISHED`/`_LOST` indication (Table 81)
    /// carries only the affected RX-ID in `Data[0..3]` -- this map resolves
    /// it back to the CLL whose `handle_start_comm` is waiting on it, the
    /// same generation-staleness discipline `j1939_claims` uses (a stale
    /// indication surviving that CLL's disconnect/reconnect fails the
    /// `connect_generation` comparison and is dropped). Unlike `j1939_claims`,
    /// this stage does not monitor a connection for a LATER spontaneous loss
    /// once established (ADR-188 Decision item 2 only covers the initial
    /// `CoptStartcomm`-driven exchange) -- an entry is removed once its
    /// initial outcome (established or lost) resolves, not retained for the
    /// connection's ongoing lifetime. The one exception: an entry abandoned
    /// locally without a device-confirmed outcome (`Tp20ConnEntry::abandoned`,
    /// Codex review finding, PR #97, 7th round) stays registered, quarantining
    /// its `rx_id` against an immediate retry, until the delayed indication
    /// it is still owed finally arrives (or this `SharedChannel` entry itself
    /// is torn down) -- see `Tp20ConnEntry::abandoned`'s own doc comment.
    tp20_connections: HashMap<u32, Tp20ConnEntry>,
    /// Latest ADR-188 connection-request outcome delivered for a CLL
    /// currently (or most recently) waiting on one -- `events.rs`'s
    /// `poll_rx_inner` RxStatus-withhold arm writes here (via
    /// `deliver_tp20_connection_indication`) once it has resolved an
    /// indication's affected RX-ID to a live, non-stale owning CLL
    /// (`tp20_connections` above); `run_tp20_connection_request`'s wait step
    /// reads and clears its own `cll_handle`'s entry. The structural sibling
    /// of `j1939_claim_results` above.
    tp20_connection_results: HashMap<u32, Tp20ConnectionOutcome>,
    /// SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8, Codex review
    /// P2, PR #98): `true` while a `PDU_IOCTL_BECOME_MASTER` call is
    /// actually in flight for this physical channel -- set (under
    /// `shared_channels`, alongside the pre-existing `ref_count == 1`
    /// sole-owner check `ioctl_become_master` already gates on) BEFORE
    /// `shared_channels` is released and the ~2s blocking native call is
    /// handed off to `spawn_blocking`, and cleared by a `Drop`-guard
    /// constructed as the FIRST statement inside that blocking closure (so
    /// it clears on normal return, on panic/unwind, and even if the
    /// awaiting async future is later cancelled by the caller). Closes the
    /// window the plain `ref_count == 1` gate alone leaves open: released
    /// before the call runs, that gate cannot see a sibling CLL's
    /// `ConnectComLogicalLink` joining this SAME channel while the bid is
    /// still outstanding (the join path bumps `ref_count` under its own,
    /// separate `shared_channels` acquisition and never touches `self.api`,
    /// so it isn't naturally serialized against the in-flight call). Both
    /// existing-channel-join branches reject a join while this flag is set,
    /// the same shape as the `dead` rejection just above. Not consulted by
    /// `PDU_IOCTL_SET_POLL_RESPONSE` -- that call is fast/non-blocking, so
    /// its own `ref_count == 1` check alone (now also rejecting instead of
    /// silently no-opping on a shared channel, the same fix this flag's own
    /// `ref_count != 1` case got) is sufficient; see
    /// `j2534-0404-service/docs/implementation-notes.md`'s GM UART section
    /// for the full writeup.
    become_master_in_flight: Arc<AtomicBool>,
    /// SAE J2534-2 clause 19.3.1 TP2.0 passive connections (ADR-190/Phase 7
    /// Stage 7b): the exclusivity token for this interface's single passive
    /// slot -- `Some` for the entire armed lifetime of whichever CLL last
    /// succeeded `arm_tp20_passive_listener`, `None` otherwise. Arming
    /// requires this to be `None`; a second CLL's own passive-arm attempt on
    /// this channel while one is already armed is rejected with the same
    /// `PduErrEvtRscLocked` shape the `0xD8` reason byte already maps to for
    /// a network-side rejection.
    ///
    /// **Deliberately `Option`, not an `Arc<AtomicBool>` reservation flag
    /// like [`Self::become_master_in_flight`] (ADR-189)** -- that pattern
    /// does not transfer here, and copying it would be the wrong shape, not
    /// merely an unnecessary one. `become_master_in_flight` guards a bounded
    /// ~2s in-flight native call and rejects a SIBLING's channel-join for
    /// that window; here, the exclusive state is held for the passive
    /// listener's ENTIRE armed lifetime (unbounded -- the peer may connect
    /// seconds, hours, or never), and clause 19.3.1 requires active
    /// connections and the passive slot to coexist (four total slots, not a
    /// choice between them): sibling joins and active `CoptStartcomm`s must
    /// keep succeeding on this same physical channel while a CLL holds this
    /// slot armed. A persistent owner-token, not an in-flight `AtomicBool`
    /// gating joins, is the correct shape (ADR-190 section 2).
    tp20_passive: Option<Tp20PassiveSlot>,
}

/// [`SharedChannel::tp20_passive`]'s own value type (ADR-190/Phase 7 Stage
/// 7b): the owning CLL, its `connect_generation` at arm time, and the two
/// native `SET_CONFIG` values currently applied (`CONFIG_TP2_0_IDENTIFER`/
/// `_RXIDPASSIVE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Tp20PassiveSlot {
    pub(super) cll_handle: u32,
    pub(super) connect_generation: u64,
    pub(super) identifier: u16,
    pub(super) rx_id_passive: u16,
}

/// One `SharedChannel::tp20_connections` entry (ADR-188/Phase 7 Stage 7a):
/// the owning CLL and its `connect_generation` at request-registration time.
/// The structural sibling of `J1939ClaimEntry` (minus the NAME field --
/// TP2.0 has no NAME-collision concept). Unlike NAME, RX-ID uniqueness
/// against a live sibling CLL IS locally enforced (`run_tp20_connection_
/// request`'s pre-insert ownership check, the `events_j1939_claim.rs`
/// `owned_by_a_live_sibling` shape) -- clause 19.3.3.2's native
/// `ERR_NOT_UNIQUE` only fires against an already-ESTABLISHED channel, not
/// a still-pending one, so it does not itself cover the case the local
/// check closes.
///
/// `abandoned` (Codex review finding, PR #97, 7th round -- ADR-188's own
/// quarantine mechanism): `true` once `run_tp20_connection_request` has
/// walked away from this attempt locally (local-deadline timeout, or this
/// CLL going stale mid-wait) WITHOUT a device-confirmed terminal outcome --
/// the native `IOCTL_REQUEST_CONNECTION` call already succeeded, so a
/// delayed `CONNECTION_ESTABLISHED`/`_LOST` indication can still arrive for
/// `rx_id` after this attempt gave up on it. Rather than removing the entry
/// immediately (the pre-fix behavior, which let an immediate same-`rx_id`
/// retry register a brand-new, indistinguishable entry and risk having the
/// stale indication misattributed to it), the entry is marked `abandoned`
/// and kept registered: `tp20_rx_id_unavailable_for` then also rejects a
/// new request proposing this `rx_id` while it stays `abandoned`, and
/// `deliver_tp20_connection_indication` releases the quarantine (removes
/// the entry, without writing a result) the moment the stale indication
/// finally arrives and is re-verified to still match this entry -- see
/// both functions' own doc comments.
///
/// `expect_stale_lost` (Codex review finding via `edge-case-hunter`,
/// design-advisor consult, P1, PR #97, round 24): `true` when this
/// registration itself reconciled a DIFFERENT, sibling CLL's own
/// `LogicalLinkState::tp20_connection` from `Established` to `Lost` at
/// registration time (`run_tp20_connection_request`'s own post-native-call
/// reconcile step) -- meaning a genuine, still-unresolved device-side
/// `CONNECTION_LOST` indication for that PRIOR occupant's own attempt is
/// still queued somewhere and will eventually drain, even though it now
/// arrives against THIS (unrelated, newly-registered) entry's own `rx_id`.
/// Without this flag, `deliver_tp20_connection_indication` would have no
/// way to tell that stale indication apart from a genuine `Lost` outcome
/// for THIS entry's own still-pending request, and would misattribute the
/// prior occupant's own old loss reason onto this brand-new, unrelated
/// attempt. Set `true` only when the reconcile step actually found and
/// flipped a live match; cleared back to `false` (never removing the entry
/// itself) the first time a `Lost` outcome for this `rx_id` is swallowed by
/// it -- so at most ONE stale `Lost` is ever absorbed per registration, and
/// a genuine `Lost` for THIS entry's own request is still delivered
/// normally afterward. See `run_tp20_connection_request`'s own reconcile
/// step and `deliver_tp20_connection_indication`'s own swallow check for
/// the full mechanism.
///
/// `passive` (ADR-190/Phase 7 Stage 7b): `true` marks a **persistent**
/// entry, one per armed passive slot, registered by
/// `events_tp20_connection::arm_tp20_passive_listener` and keyed on
/// `TP2_0_RXIDPASSIVE` -- unlike every Stage 7a active-connection entry
/// (`passive: false` everywhere else), this entry deliberately survives its
/// own resolution instead of being removed once an outcome lands, adopting
/// the same retain-for-session shape `SharedChannel::j1939_claims` already
/// uses: a `Lost` outcome for a passive entry re-enters
/// `Tp20ConnectionPhase::Listening` (never a terminal `Lost`) so the device's
/// next auto-accepted inbound connection has a routing entry to resolve
/// against. Only removed by an explicit disarm (`CoptStopcomm`/
/// `DisconnectComLogicalLink`/`DestroyComLogicalLink`'s passive branch,
/// ADR-190 section 4), which marks it `abandoned` in place -- never a plain
/// removal -- the same unconditional-quarantine convention every other
/// quarantine-insert site in this mechanism uses (ADR-188 rounds 19-21).
///
/// `idle_release_at` (ADR-190's "Correction" paragraph under `### 4. Disarm
/// / teardown`, Codex review finding, P1, PR #99; design-advisor consult):
/// an `abandoned && passive` entry's own ADR-101 Decision §E-style bounded-
/// release deadline, for exactly the one disarm shape that issues no native
/// call at all -- a passive listener disarmed while still `Listening`
/// (never accepted a connection), so no `TEARDOWN_CONNECTION` is ever sent
/// and therefore no indication will ever arrive to release the quarantine
/// the ordinary way. Stamped by `events_tp20_connection::quarantine_tp20_
/// passive_slot_on_disarm` only when `!was_established && config_cleared`
/// (the native `SET_CONFIG` clears themselves succeeded, so clause
/// 19.3.3.1's own accept condition can never again fire for this listener);
/// consulted by `events_tp20_connection::release_idle_passive_slots`
/// (`tp20_passive_idle_release_due`), the poll-task sweep that removes the
/// entry once the per-channel drain watermark (`ChannelPollCtx::
/// drain_watermarks`) proves every channel that could still deliver a
/// pre-disarm accept's own indication has been exhaustively drained past
/// this deadline. `None` means the pre-existing indefinite-quarantine
/// behavior applies -- either this entry is not eligible for bounded
/// release (already `Established` at disarm time, so a real teardown call
/// -- and its own eventual indication -- is what releases it instead), or
/// the config-clear itself failed (the device may genuinely still be able
/// to auto-accept on this RX-ID, so releasing early would be unsound).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Tp20ConnEntry {
    pub(super) cll_handle: u32,
    pub(super) connect_generation: u64,
    pub(super) abandoned: bool,
    pub(super) expect_stale_lost: bool,
    pub(super) passive: bool,
    pub(super) idle_release_at: Option<tokio::time::Instant>,
}

/// SAE J2534-2 clause 19 TP2.0 connection-request outcome (ADR-188/Phase 7
/// Stage 7a), as delivered by a `CONNECTION_ESTABLISHED`/`_LOST` indication
/// (Table 81) and routed to the owning CLL via `SharedChannel::tp20_connections`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tp20ConnectionOutcome {
    /// The device assigned this TX-ID (`Data[4..7]`).
    Established(u32),
    /// The connection failed/was lost; the reason byte (`Data[4]`, clause
    /// 19.4.4 Table 81).
    Lost(u8),
}

/// SAE J2534-2 clause 16 SAE J1939 address-claim/defend outcome (ADR-179
/// Decision 3), as delivered by a `RX_FLAG_J1939_ADDRESS_CLAIMED`/`_LOST`
/// indication (clause 16.4.6 Table 63) and routed to the owning CLL via
/// `SharedChannel::j1939_claims`. Carries the affected address so a
/// consumer can confirm it matches the specific address it is currently
/// waiting on (defense-in-depth against a stale/mismatched read; in
/// practice `SharedChannel::j1939_claims`' own per-address keying already
/// ensures this, but a consumer checks anyway rather than trusting that
/// invariant blindly across the async gap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum J1939ClaimOutcome {
    Claimed(u8),
    Lost(u8),
}

/// A tester-present target's physical CAN-ID matching criteria (ADR-088
/// amendment): the `CP_CanRespUSDTId`/`CP_CanRespUUDTId` of the
/// UniqueRespIdTable entry tester-present is physically addressed to,
/// matched directly against an incoming frame's own raw CAN ID -- never
/// through a live `unique_resp_identifier` table lookup, which a later
/// `SetUniqueRespIdTable` can retarget out from under a frozen uid (see
/// `ResolvedTesterPresent::target_can_ids`'s doc comment for the concrete
/// failure this avoids).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TesterPresentTargetCanIds {
    pub(super) usdt: Option<u32>,
    pub(super) uudt: Option<u32>,
}

/// One tester-present send's response/TX-echo discard window (ADR-088 second
/// amendment, widened by ADR-099), frozen entirely from that send's own
/// pre-send snapshot -- never re-derived from live Active afterward. `until`
/// is `fired_at + CP_P2Max` from that send instant; `pos`/`neg` are that same
/// instant's `CP_TesterPresentExpPosResp`/`CP_TesterPresentExpNegResp`. Wrapped
/// in a [`ResidualTesterPresentDiscard`] and pushed to
/// `LogicalLinkState::open_tp_discards` (ADR-137 fourth Codex-review fix /
/// round-4 restructure) by all three write sites
/// (`dispatch_due_tester_present`, `handle_start_comm`, `handle_update_param`)
/// whenever that send merely succeeded (ADR-099: TX-side indication frames
/// are artifacts of the send itself, not of `CP_TesterPresentReqRsp`'s "does
/// the ECU reply" semantics) -- a failed send never pushes at all. `pos`/`neg`
/// are frozen from that same pre-send snapshot's `exp_pos_resp`/
/// `exp_neg_resp` only when `expects_response` (`CP_TesterPresentReqRsp == 1`
/// at send time) was also `true`; otherwise they are frozen empty.
/// `build_cll_rx_entries` (`events.rs`) therefore treats a still-open window
/// (`now < until`) as proof only that the send *succeeded* -- non-empty
/// `pos`/`neg` additionally proves that send expected a response and so is
/// eligible for content/SOM-herald discard; `tx_can_id`-gated TX-side discard
/// applies regardless, without re-reading `l.active.tester_present_req_rsp()`
/// live: `expects_response` is excluded from `same_wire_behavior` (see
/// `ResolvedTesterPresent`'s doc comments), so a `CoptUpdateparam` that flips
/// only `CP_TesterPresentReqRsp` (or only `CP_TesterPresentExpPosResp`/
/// `CP_TesterPresentExpNegResp`) never re-arms -- a live re-read of any of
/// these three fields could drift arbitrarily from what was actually true at
/// the send instant that opened this window, for the whole `CP_P2Max`
/// duration. This is the same reused pattern as
/// `ResolvedTesterPresent::target_can_ids`/`p2_max_ms`/`expects_response`
/// (ADR-088 first amendment): freeze at send time, never recompute live.
#[derive(Debug, Clone)]
pub(super) struct DiscardWindow {
    pub(super) until: tokio::time::Instant,
    pub(super) pos: Vec<u8>,
    pub(super) neg: Vec<u8>,
}

/// One entry in `LogicalLinkState::open_tp_discards` (ADR-137 fourth
/// Codex-review fix / round-4 restructure): a still-open response/TX-echo
/// discard window from one specific successful tester-present send, self-
/// contained enough to be matched against an incoming frame independently of
/// `tester_present_state` -- open windows now live in their own per-CLL list
/// rather than being carried, variant-by-variant, through every
/// `tester_present_state` transition (the rounds 1-3 design this replaces:
/// `Armed::discard_until`, `Cleared::residual`, `Disarmed::residual`). This
/// storage move fixes the rounds 1-3 approach's structural mismatch (a
/// discard window's lifetime is per-*send*; `tester_present_state`'s
/// lifetime is per-*configuration*) as well as two latent bugs of the exact
/// same class found in the pre-existing per-tick sender
/// (`dispatch_due_tester_present`): (i) a `CoptUpdateparam` that changes only
/// a field excluded from `same_wire_behavior` (e.g.
/// `CP_TesterPresentExpPosResp`/`CP_P2Max`) never re-arms, so the next send's
/// window used to silently replace a still-open prior window with different
/// `pos`/`until`; (ii) a *failed* send used to unconditionally clear the
/// slot, dropping a still-open prior window outright.
///
/// Freezes exactly the four things `build_cll_rx_entries`'s discard-matcher
/// lookup (`events.rs`) reads to build a `TesterPresentDiscard` for a match
/// -- `pos`/`neg` (via `window`), `target_can_ids`, and `tx_can_id` -- without
/// carrying the full `resolved`/`framed_data` an `Armed` state needs for a
/// FUTURE send (this is purely a record of a past one). Pushed on every
/// successful send (never on failure) at all three write sites
/// (`dispatch_due_tester_present`, `handle_start_comm`,
/// `handle_update_param`'s re-arm), pruned of expired entries at each push,
/// and deduplicated by signature rather than capped by count (ADR-137 fifth
/// Codex-review fix: a same-signature push extends an existing entry's
/// deadline in place instead of appending a duplicate -- see
/// `push_open_tp_discard`'s own doc comment for why a numeric cap is unsafe
/// here). `build_cll_rx_entries` discards a frame matching ANY still-open
/// entry in the list -- each entry is an independently elicited send, not a
/// precedence chain.
#[derive(Debug, Clone)]
pub(super) struct ResidualTesterPresentDiscard {
    pub(super) window: DiscardWindow,
    pub(super) target_can_ids: Option<TesterPresentTargetCanIds>,
    pub(super) tx_can_id: Option<u32>,
}

/// `LogicalLinkState::tester_present_state`: what (if anything) `CoptStartcomm`
/// armed for tester-present dispatch, keyed off `CP_TesterPresentSendType`
/// (ADR-083). Both `CP_TesterPresentSendType` values (0 "periodic", 1 "idle-
/// triggered") are software-driven through the same `Armed` arm; `send_type`
/// on `resolved` is the mode discriminator, not the enum shape.
#[derive(Debug)]
enum TesterPresentState {
    /// No tester-present configured (empty message or zero interval), or not
    /// yet started. Distinct from `Cleared`: this CLL's tester-present has
    /// never been armed at all, so `handle_update_param`'s re-arm gate
    /// treats a non-empty resolution as the legitimate first-enable
    /// (ADR-084) -- `Cleared` is for a CLL that WAS armed and had it
    /// explicitly disarmed via `PDU_IOCTL_CLEAR_PERIODIC_MSGS`.
    None,
    /// Software-driven send, armed for either `CP_TesterPresentSendType`
    /// (`resolved.send_type` distinguishes mode 0 "periodic" from mode 1
    /// "idle-triggered" -- see `dispatch_due_tester_present`'s due-check
    /// formula). No hardware resource to release at teardown -- just
    /// reverts to `None`. `resolved` and `interval` are the values
    /// `CoptStartcomm` resolved at call time (ADR-067 claim 8: never
    /// re-resolved live); `framed_data` is the already-SF-framed wire
    /// payload computed once at arm time (identical to what
    /// `frame_tester_present_data` would recompute from
    /// `resolved.data`/`resolved.isotp_framing` on every poll tick, so it is
    /// cached here instead). The poll loop's tick handler dispatches a
    /// one-shot send through `transmit_request` once `interval` has elapsed
    /// since the mode-appropriate reference instant (ADR-083). `armed_at` is
    /// this CLL's own arm instant; it is NOT a mutation of the shared
    /// `last_bus_activity` clock (arming is not bus activity) -- see
    /// `dispatch_due_tester_present`. `last_fired` is `Some` from the moment
    /// this state is constructed: arming (via `CoptStartcomm` or a live
    /// `CoptUpdateparam` re-arm) now performs the first send synchronously
    /// instead of waiting a full interval, so there is no window where this
    /// CLL is armed but has never sent (ADR-084); it is then advanced to the
    /// post-send instant on every subsequent send; this CLL's own sends
    /// are excluded from the shared `last_bus_activity` clock
    /// (`transmit_request`'s `count_as_bus_activity = false` here), so a
    /// same-interval sibling CLL on the same channel cannot be starved by
    /// this CLL's keep-alives (a Codex-review-caught regression in an
    /// earlier draft, see ADR-083 Consequences).
    Armed {
        resolved: rpc_primitive::ResolvedTesterPresent,
        interval: Duration,
        armed_at: tokio::time::Instant,
        last_fired: Option<tokio::time::Instant>,
        framed_data: Vec<u8>,
    },
    /// Explicitly disarmed via `PDU_IOCTL_CLEAR_PERIODIC_MSGS` (a third
    /// Codex-review finding on this mechanism): distinct from `None`
    /// ("never configured") so that `handle_update_param`'s re-arm gate
    /// (`events.rs`) can tell "this CLL's tester-present was explicitly
    /// cleared, and an unrelated `CoptUpdateparam` (e.g. promoting
    /// `CP_Loopback`) must not resurrect it" apart from "tester-present was
    /// never configured at all, and this `CoptUpdateparam` is the
    /// legitimate first-enable (ADR-084)". `resolved` is carried forward
    /// from the `Armed` state being cleared so the re-arm gate's
    /// `same_wire_behavior` comparison still has a baseline: an unrelated
    /// promotion compares equal to it and does not re-arm, while a
    /// `CoptUpdateparam` that actually reconfigures tester-present's wire
    /// content compares different and DOES re-arm -- "reconfiguring
    /// tester-present after a clear is itself a re-enable" is deliberate
    /// (ADR-093). No hardware resource, `interval`, `armed_at`, or
    /// `last_fired` to carry: this state is inert until either a fresh
    /// `CoptStartcomm` (always overwrites unconditionally) or a
    /// wire-content-changing `CoptUpdateparam` re-arms it. As of ADR-137's
    /// fourth Codex-review fix (round-4 restructure), this variant no longer
    /// carries a `residual` field: any still-open discard window from the
    /// `Armed` state being cleared already lives independently in
    /// `LogicalLinkState::open_tp_discards`, which this write does not touch
    /// -- see that field's own doc comment.
    Cleared {
        resolved: rpc_primitive::ResolvedTesterPresent,
        cleared_at: tokio::time::Instant,
    },
    /// The disarm-via-`CP_TesterPresentHandling = 0` counterpart to
    /// `Cleared` (ADR-137 second Codex-review fix): written when a live
    /// `CoptUpdateparam` resolves `CP_TesterPresentHandling` to `0` on a
    /// CLL that was `Armed`. Deliberately does NOT carry the full
    /// `resolved: ResolvedTesterPresent` the way `Cleared` does for ITS own
    /// purpose (the re-arm gate's `same_wire_behavior` baseline) --
    /// `handling_enabled` is deliberately excluded from `same_wire_behavior`
    /// (ADR-137), so if a handling-disable transition left a `resolved`
    /// baseline behind, a later `CP_TesterPresentHandling` 0->1 promotion
    /// would compare equal against it (every other field unchanged) and
    /// never re-arm, breaking the spec's "once enabled, sent immediately"
    /// contract (ADR-084) this whole mechanism exists to implement. Carrying
    /// no `resolved` field structurally forces `handle_update_param`'s
    /// re-arm snapshot's `old_resolved` to be `None` on a 0->1 promotion out
    /// of `Disarmed` (the same `None` `TesterPresentState::None` already
    /// produces), which is what makes 0->1 re-enable work correctly after a
    /// disarm. As of ADR-137's fourth Codex-review fix (round-4
    /// restructure), this variant no longer carries a `residual` field
    /// either: any still-open discard window from the state being disarmed
    /// already lives independently in `LogicalLinkState::open_tp_discards`,
    /// which this write does not touch -- see that field's own doc comment.
    Disarmed { disarmed_at: tokio::time::Instant },
}

/// Timestamp and no-response-required flag of a CAN transmission on a
/// shared channel, tracked separately for the last functionally- and
/// physically-addressed sends to enforce `CP_P3Func`/`CP_P3Phys` (ADR-060).
#[derive(Debug, Clone, Copy)]
pub(super) struct TxGapState {
    pub(super) at: tokio::time::Instant,
    /// `true` when this transmission's `NumReceiveCycles == 0` (no response
    /// was required) — the condition that triggers gap enforcement before
    /// the *next* transmission in the same addressing bucket.
    pub(super) no_response_required: bool,
}

/// A `PduErrorEvent` paired with the module-clock time it was recorded
/// (`events::module_timestamp_us()`, monotonic and process-start-relative,
/// not wall-clock — ADR-120), so a later failing RPC's
/// `ErrorDetail.error_event_data` can report when the tracked error
/// actually occurred instead of a placeholder (ADR-105 follow-up).
#[derive(Debug, Clone)]
pub(super) struct TrackedError {
    pub(super) event: vci_service_interface::PduErrorEvent,
    pub(super) timestamp: u32,
    /// The ComPrimitive this error pertained to, if any (ISO 22900-2
    /// §9.4.7 c) / §9.6.2: an error associated with a specific
    /// ComPrimitive must carry that ComPrimitive's handle -- mirrors
    /// native `PDU_EVENT_ITEM.hCop`). `None` means module- or
    /// CLL-scoped (native `PDU_HANDLE_UNDEF`), e.g. the module-level
    /// hard-error broadcast on lost comm to the VCI. Per §9.4.7.1, this
    /// is a snapshot at record time and may reference a COP that has
    /// since finished -- staleness here is spec-permitted, not a bug.
    pub(super) cop: Option<CopRef>,
    /// `cop`'s `CopEntry::cop_tag` (ADR-204), supplied by `send_error_event`'s
    /// caller as part of the same [`ErrorCop`] pair `cop` above is derived
    /// from (ADR-205 Decision item 1) -- never resolved fresh inside
    /// `send_error_event` itself. `None` whenever `cop` is `None`, and also
    /// `None` when `cop` is `Some` but no tag was supplied at
    /// `StartComPrimitive`. `TrackedError` lost `Copy` when this field was
    /// added (`Vec<u8>` isn't `Copy`) -- callers that used to copy a
    /// `TrackedError` out of a lock now clone it instead.
    pub(super) cop_tag: Option<Vec<u8>>,
}

/// The `(cll_handle, cop_handle)` pair a [`TrackedError`] pertains to.
#[derive(Debug, Clone, Copy)]
pub(super) struct CopRef {
    pub(super) cll_handle: u32,
    pub(super) cop_handle: u32,
}

/// A `cop_handle` paired with its already-captured `CopEntry::cop_tag`
/// (ADR-205 Decision item 1) -- the value every `send_error_event` call site
/// supplies as a single unit, sourced from whatever earlier read of a live
/// `CopEntry` already decided the event should fire. `send_error_event`
/// itself performs no `primitives` lookup of its own (its former fresh-lookup
/// form was deleted -- see its own doc comment in `events_event_senders.rs`
/// for the full rationale). `CopEntry::cop_tag` is write-once (set only at
/// `StartComPrimitive` insertion, `rpc_primitive.rs`, never mutated again
/// for that COP's lifetime), so any earlier read of it remains valid
/// forever -- there is no "the tag changed since I read it" case to guard
/// against, only "was this read before or after the entry might have been
/// removed." Also used by `frame_tester_present_data`/
/// `send_tester_present_once` (`events.rs`), which only ever forward it
/// unchanged into their own `send_error_event` call.
pub(super) type ErrorCop = (u32, Option<Vec<u8>>);

/// Persistent module-level status and error state, shared across channel
/// lifecycle events.  Updated by `handle_channel_hard_error` when the VCI
/// adapter becomes unavailable; reset to Ready when a new physical channel
/// opens successfully.
pub(super) struct ModuleState {
    pub(super) status: vci_service_interface::PduModuleStatus,
    pub(super) last_error: TrackedError,
}

impl Default for ModuleState {
    fn default() -> Self {
        Self {
            status: vci_service_interface::PduModuleStatus::PduModstReady,
            last_error: TrackedError {
                event: vci_service_interface::PduErrorEvent::PduErrEvtNoerror,
                timestamp: 0,
                cop: None,
                cop_tag: None,
            },
        }
    }
}

/// Clone-able snapshot of a logical link's state for use outside the lock.
#[derive(Debug, Clone)]
struct LinkView {
    /// Key into `J2534Service::shared_channels`; `None` when not connected.
    ///
    /// No `channel_id` field here (removed as dead code once
    /// `rpc_io_ctl_legacy`'s TOCTOU fix stopped reading it -- this was its
    /// only remaining reader): every other consumer of this snapshot already
    /// resolves the LIVE channel id through `channel_key` against a held
    /// `shared_channels` guard instead, since a snapshot's own `channel_id`
    /// can go stale the instant `logical_links` is released.
    channel_key: Option<ChannelKey>,
    protocol: ChannelProtocol,
    /// J2534 protocol ID actually used for `PassThruConnect` / messages —
    /// may differ from `protocol.j2534_protocol_id()` in software-ISO-TP
    /// mode (ADR-046).
    hw_protocol_id: u32,
    /// `true` when this CLL performs ISO-TP in software over a raw CAN
    /// channel (`can_channel_mode = "software-isotp"`, ADR-046).
    software_isotp: bool,
    /// Mirrors `LogicalLinkState::base_hw_protocol_override` (ADR-157/
    /// Bug 1 fix, generalized to `_CHx` by ADR-156 Decision 3 addendum/
    /// Phase 2b) -- needed by [`LinkView::base_hw_protocol_id`], since
    /// several call sites (`GetComParam`/`SetComParam`/
    /// `SetComParamField_Bytes`/`SetComParamField_Struct`) only have a
    /// `LinkView` snapshot in scope, not a live `LogicalLinkState`
    /// reference. Neither `LogicalLinkState::pin_select` nor
    /// `LogicalLinkState::channel_index` is mirrored here -- nothing in
    /// this snapshot's own consumers needs to know WHICH qualifier route
    /// (or neither) produced this override, only its Plane B answer, once
    /// `base_hw_protocol_id()` reads this field directly instead of
    /// branching on a qualifier-specific sibling field.
    base_hw_protocol_override: Option<u32>,
    connected: bool,
    /// `true` after `CoptStartcomm` completes successfully.
    comm_started: bool,
    /// Mirrors `LogicalLinkState::raw_mode` (ADR-196 Decision item 1) --
    /// needed because `rpc_primitive.rs`'s TX-flags/header resolution
    /// (`resolve_send_recv_tx`/`resolve_tester_present`/
    /// `resolve_init_tx_flags`, and their shared `compute_j2534_tx_flags`/
    /// `apply_resolved_tx_flags` helpers) reads link state through this
    /// snapshot at every call site, not the raw `LogicalLinkState` map.
    raw_mode: bool,
    /// Mirrors `LogicalLinkState::checksum_mode` (ADR-198 Phase 2) -- see
    /// that field's own doc comment. Needed for the same reason `raw_mode`
    /// above is mirrored: `rpc_primitive.rs`'s TX-flags/header resolution
    /// reads link state through this snapshot, not the raw
    /// `LogicalLinkState` map.
    checksum_mode: bool,
    /// Working ComParam set — what `SetComParam`/`GetComParam` see.
    working: ComParamSet,
    /// Mirrors `LogicalLinkState::connect_generation` (ADR-086, round 5) --
    /// used by `rpc_start_com_primitive` to detect a reconnect landing
    /// between this early snapshot and its later, separate
    /// `logical_links` reads.
    connect_generation: u64,
    /// Mirrors `LogicalLinkState::last_error` at snapshot time -- used by
    /// state-guard rejections that reject synchronously off this same
    /// snapshot, with no intervening `.await` (ADR-105 follow-up). A guard
    /// that re-locks `logical_links` before rejecting must read the fresh
    /// value from that lock instead, not this field.
    last_error: Option<TrackedError>,
}

impl LinkView {
    /// ADR-157 Plane B normalization primitive, mirroring
    /// `LogicalLinkState::base_hw_protocol_id` exactly -- see that method's
    /// doc comment. Needed because several call sites (`GetComParam`/
    /// `SetComParam`/`SetComParamField_Bytes`/`SetComParamField_Struct`)
    /// only have a `LinkView` snapshot in scope, not a live
    /// `LogicalLinkState` reference.
    fn base_hw_protocol_id(&self) -> u32 {
        self.base_hw_protocol_override
            .unwrap_or(self.hw_protocol_id)
    }
}

/// Value type of `J2534Service::primitives` (`cop_handle -> CopEntry`).
///
/// `dispatched` distinguishes `PduCopstIdle` (`false` — never dispatched) from
/// `PduCopstWaiting` (`true` — a cyclic COP resting between cycles) in
/// `GetStatus`. It lives exactly as long as this `primitives` entry does, so
/// it can never leak independently of it: any `primitives.remove(...)` call
/// site across the crate discards `dispatched` along with `cll_handle`, with
/// no separate cleanup needed anywhere (ADR-117).
#[derive(Debug, Clone)]
pub(super) struct CopEntry {
    pub(super) cll_handle: u32,
    pub(super) dispatched: bool,
    /// The client-supplied `StartComPrimitiveRequest.cop_tag` (ADR-204),
    /// verbatim, or `None` if no tag was supplied. This is the authoritative
    /// source every `EventItem`-emitting path for this `cop_handle` resolves
    /// its own echoed `cop_tag` from -- `events::send_cop_status` (COP-status
    /// transitions) reads it directly; `ReceivedFrame::cop_tag`/
    /// `TrackedError::cop_tag` (result/error events) are each a snapshot
    /// taken from here at the time the frame/error was bound to this COP,
    /// since by the time either is read back this entry may already be gone
    /// (see those fields' own doc comments) -- while the entry is still
    /// present here -- lives exactly as long as this `primitives` entry
    /// does, so it needs no separate cleanup (same rationale as
    /// `dispatched`, above). `None` for a COP this service synthesizes
    /// internally rather than one a client requested via
    /// `StartComPrimitive` (there is no client tag to attach).
    pub(super) cop_tag: Option<Vec<u8>>,
    /// Whether the COP that created this entry actually transmits on the
    /// physical bus, per `TxItem::transmits()`'s classification -- set once
    /// at `StartComPrimitive` call time from `cop_type`/`cop_data`, read by
    /// `LockResource`'s active-transmission check (ADR-123) to distinguish a
    /// transmitting COP from a merely-executing one (e.g. `CoptDelay`).
    /// Never consulted by `GetStatus`/`PduCopstExecuting` -- that meaning is
    /// unchanged.
    pub(super) transmits: bool,
    /// Whether this entry is a `PDU_COPT_SENDRECV` COP -- set once at
    /// `StartComPrimitive` call time alongside `transmits`, from the same
    /// `cop_type` match. ADR-180 Decision 11: lets a J1939 claim-
    /// relinquishment site find and cancel this CLL's live *transmitting*
    /// SendRecv COPs (`cll_handle` match `&& is_send_recv && transmits` --
    /// the `transmits` conjunct excludes a receive-only, `NumSendCycles ==
    /// 0` monitor, ADR-059, which never puts a frame on the bus and must
    /// survive an address change) without also catching `CoptStartcomm`
    /// (would cancel the very reclaim attempt driving this) or a
    /// terminal-send `CoptStopcomm` (would recreate ADR-085's
    /// `stop_comm_pending` deadlock) -- both of which also set `transmits`.
    pub(super) is_send_recv: bool,
}

/// Full logical link state.
#[derive(Debug)]
struct LogicalLinkState {
    /// J2534 channel handle; `None` before `ConnectComLogicalLink` is called.
    channel_id: Option<ChannelId>,
    /// Service-level protocol; set at `CreateComLogicalLink` from the resource field.
    protocol: ChannelProtocol,
    /// J2534 protocol ID used for the physical channel (`PassThruConnect`,
    /// `ChannelKey`, message building, ADR-028 SET_CONFIG gating).  Equals
    /// `protocol.j2534_protocol_id()` except in software-ISO-TP mode, where
    /// ISO15765-family CLLs run on a raw CAN channel (ADR-046).
    hw_protocol_id: u32,
    /// `true` when this CLL performs ISO-TP in software (ADR-046).
    software_isotp: bool,
    /// Companion raw-CAN channel used for UUDT reception in dual-channel
    /// mode (ADR-046); `None` in every other mode or while no
    /// `CP_CanRespUUDTId` is configured.
    uudt_channel_id: Option<ChannelId>,
    /// Key of the companion channel in `J2534Service::shared_channels`.
    uudt_channel_key: Option<ChannelKey>,
    /// Per-`(CAN ID, canonical width)` reassembly state for software ISO-TP
    /// RX, shared with the poll task (like `rx_buf`). Always empty unless
    /// `software_isotp`. ADR-222: a numeric id contended between two
    /// `CP_CanRespUSDTId` entries of different widths on the same CLL must
    /// reassemble each width's segmented transfer independently, or a
    /// FirstFrame at one width clobbers the other's in-progress state
    /// (Codex review finding, PR #141). The width component is the MATCHED
    /// ENTRY's own `CanIdWidthGate::reassembly_key_width()` -- a canonical
    /// value derived once per frame from table configuration -- never a
    /// frame's raw `RxStatus`-bit-8 reading directly: keying by the raw bit
    /// regressed an uncontended id whenever a device tags that bit
    /// inconsistently across one segmented transfer's own frames (a second,
    /// distinct Codex review finding, same PR).
    isotp_rx: Arc<Mutex<HashMap<(u32, bool), isotp::Reassembly>>>,
    /// A live `Weak` here means a `ConnectComLogicalLink` RPC for this CLL is
    /// currently between accepting the call and `finalize_connected_link`
    /// publishing `connected = true` -- ISO 22900-2 §9.5.16 gates
    /// `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES` on the call preceding
    /// PDUConnect (call time, not completion time), so this closes the
    /// window `connected` alone leaves open (ADR-126 round 2). A `Weak`, not
    /// a `bool`: the owning `Arc<()>` lives only as a local in
    /// `rpc_connect_com_logical_link`'s stack frame and Rust drops it on
    /// every exit path (success, `?`, or the RPC future being cancelled by
    /// tonic) with no manual clear -- a `bool` would strand `true` forever
    /// if the future is ever dropped mid-await, permanently bricking both
    /// this guard and all future Connect retries for the CLL.
    connect_in_flight: std::sync::Weak<()>,
    connected: bool,
    /// `true` after `CoptStartcomm` completes; reset to `false` on `CoptStopcomm` / disconnect.
    comm_started: bool,
    /// ISO 22900-2:2022 Annex D.2.3 (Table D.6, byte 0 bit 7) RawMode
    /// (ADR-196 Decision item 1): resolved once from
    /// `CreateComLogicalLinkRequest.cll_create_flag_bits`/
    /// `cll_create_flag_raw` at `CreateComLogicalLink` time
    /// (`rpc_create_com_logical_link`) and fixed for this CLL's whole
    /// lifetime -- Table D.6 defines no later mechanism to change it.
    /// `false` (OFF, the default and the only value every pre-ADR-196 CLL
    /// ever had) for every CLL whose protocol this service does not
    /// allowlist for RawMode=ON: `rpc_create_com_logical_link` rejects
    /// RawMode=ON for every protocol outside base CAN/hardware ISO15765/
    /// hardware K-line (ADR-198 Phase 2 extended Phase 1's CAN/ISO15765-only
    /// allowlist to ISO9141/ISO14230) (plus a documented Analog Inputs/SCI
    /// no-op exception) at create time, instead of ever storing `true` for
    /// one. When `true`, `tx_header::build_tx_message` skips header
    /// construction entirely (Decision item 2),
    /// `rpc_primitive::compute_j2534_tx_flags`/`apply_resolved_tx_flags`
    /// treat `TX_FLAG_CAN_29BIT_ID`/`TX_FLAG_ISO15765_ADDR_TYPE` as
    /// client-authoritative instead of service-derived (Decision item 2),
    /// and `poll_rx_inner` skips the RX header/footer split (Decision item
    /// 3).
    raw_mode: bool,
    /// ISO 22900-2:2022 Annex D.2.3 (Table D.6, byte 0 bit 6) ChecksumMode
    /// (ADR-198 Phase 2 Decision item 1): resolved once from
    /// `CreateComLogicalLinkRequest.cll_create_flag_bits`/
    /// `cll_create_flag_raw` at `CreateComLogicalLink` time, alongside
    /// `raw_mode` above, and fixed for this CLL's whole lifetime. Meaningful
    /// only for a RawMode=ON K-line (ISO9141/ISO14230) CLL -- Table D.6
    /// defines it as ignored whenever RawMode is OFF or for a checksumless
    /// protocol, so every other CLL's value here is inert. When `true`
    /// (alongside `raw_mode`), `rpc_link::connect_flags` leaves the native
    /// `CONNECT_FLAG_ISO9141_NO_CHECKSUM` bit clear (the D-PDU API/vendor
    /// interface still manages the checksum); when `false`, that bit is set
    /// (the client manages the checksum itself, and `tx_header`/
    /// `rpc_primitive` pass the client's own checksum byte through
    /// unchanged).
    checksum_mode: bool,
    /// Monotonic generation stamped by `finalize_connected_link` every time
    /// `ConnectComLogicalLink` finalizes a connection for this `cll_handle`
    /// (including a reconnect onto the same physical channel). `0` before
    /// the first connect. Compared against a COP's call-time-captured value
    /// by `handle_start_comm`/`handle_stop_comm`'s `still_on_this_channel`-
    /// style guards to detect a disconnect-then-reconnect since the COP was
    /// accepted, even when `channel_id` happens to match again on a shared
    /// channel (ADR-086).
    connect_generation: u64,
    /// `true` from a `CoptStopcomm` RPC call's acceptance until its
    /// `handle_stop_comm` poll-task execution fully completes (or the link
    /// is disconnected/hard-errored) -- guards against a second concurrent
    /// `CoptStopcomm` being accepted while comm_started stays `true`
    /// throughout the first one's stop-comm sequence (ADR-085 amendment):
    /// without this, a retried CoptStopcomm could cancel the first's
    /// in-flight final transmit or produce a duplicate one. See ADR-085.
    stop_comm_pending: bool,
    /// Key into `J2534Service::shared_channels`; `None` when not connected.
    channel_key: Option<ChannelKey>,
    /// SAE J2534-2 clause 6 Pin Selection value (ADR-156 Decision 2):
    /// `Some(0x0000PPSS)` when this CLL resolved to a `_PS` hardware
    /// protocol variant at `CreateComLogicalLink` because the caller's
    /// `dlc_pin_data` differed from the matched resource row's default DLC
    /// pins; `None` for every other CLL (the overwhelming majority), which
    /// reproduces pre-Phase-2a behavior exactly. When `Some`, `hw_protocol_id`
    /// above is also the `_PS` variant id (not the base protocol id), and
    /// `rpc_connect_com_logical_link`'s `connect_new_physical_channel` issues
    /// one `PassThruIoctl(SET_CONFIG, CONFIG_J1962_PINS = pin_select)` right
    /// after `PassThruConnect`, only on first-open of the physical channel
    /// (a joining CLL sharing the same `ChannelKey`, which already includes
    /// `pin_select`, never re-issues it).
    pin_select: Option<u32>,
    /// SAE J2534-2 clause 7 (Additional Channels) selected index (ADR-156
    /// Decision 3/Phase 2b): `Some(1..=128)` when this CLL resolved to a
    /// `_CHx` hardware protocol variant at `CreateComLogicalLink` -- because
    /// the caller named a `_CHx` hardware protocol id directly (decomposed
    /// via `resources::chx_base_protocol_id`, ADR-156 Decision 3 addendum;
    /// the `ResourceData.channel_index` field route was removed by
    /// ADR-178); `None` for every other CLL. Mutually exclusive with `pin_select`
    /// (`names::resolve_channel_selection` rejects a request supplying
    /// both). When `Some`, `hw_protocol_id` above is also the `_CHx`
    /// variant id (not the base protocol id), and `base_hw_protocol_override`
    /// below is set together with this field, at the same time, from the
    /// same resolution tuple.
    channel_index: Option<u32>,
    /// The correctly-resolved base hardware protocol id for a `_PS` or
    /// `_CHx` link (ADR-157/Bug 1 fix, generalized by ADR-156 Decision 3
    /// addendum/Phase 2b to cover `_CHx` too) -- `names::resolve_pin_selection`/
    /// `names::resolve_channel_selection`'s own internally computed
    /// `hw_protocol_id` (resource row's `hw_protocol_override` when
    /// present, else `protocol.j2534_protocol_id()`), captured here so
    /// `base_hw_protocol_id()` doesn't need to lossily re-derive it via
    /// `protocol.j2534_protocol_id()` alone -- which collapses every SAE
    /// J2610 SCI `_PS`/`_CHx` variant onto the single shared `SCI_MODE` id
    /// (ADR-023), dropping the exact `SCI_A_ENGINE`/`_A_TRANS`/`_B_ENGINE`/
    /// `_B_TRANS` distinction a non-qualified connect to the same row would
    /// have used. `Some` exactly when `pin_select` OR `channel_index` is
    /// `Some` (all three set together at `CreateComLogicalLink` from the
    /// same resolution tuple, whichever qualifier route produced it);
    /// `None` for every unqualified link. `base_hw_protocol_id()` below
    /// gates on THIS field's own presence, not on `pin_select`/`channel_index`
    /// individually -- ADR-156 Decision 3 addendum: branching on a single
    /// qualifier-specific sibling field is a "two fields must move together"
    /// trap once a second qualifier route exists.
    base_hw_protocol_override: Option<u32>,
    /// Per-CLL event queue -- received frames AND async error events
    /// (`CllQueueItem`), drained by `GetEventItem`, bundled with this CLL's
    /// live-subscription generation stamp (`CllEventQueue`, ADR-115 round
    /// 4). Allocated at `CreateComLogicalLink` and shared with the poll
    /// task.
    rx_buf: Arc<Mutex<CllEventQueue>>,
    /// Working ComParam set — written by `SetComParam`, read by `GetComParam`.
    /// Applied to hardware at `ConnectComLogicalLink` and `CoptUpdateparam`.
    working: ComParamSet,
    /// Active ComParam set — snapshot of the last Working set that was applied
    /// to the hardware.  `CoptRestoreParam` copies this back to `working`.
    active: ComParamSet,
    /// Tester-present dispatch state armed by the last successful
    /// `CoptStartcomm`, keyed off `CP_TesterPresentSendType` (ADR-083). See
    /// [`TesterPresentState`].
    tester_present_state: TesterPresentState,
    /// `base_tx_flags` the last successful `CoptStartcomm` resolved
    /// tester-present with (`ResolvedTesterPresent::base_tx_flags`) -- reused
    /// by a later live `CoptUpdateparam` re-resolution of tester-present,
    /// since `TxItem::UpdateParam` carries no client TxFlags of its own for a
    /// persistent tester-present.
    tester_present_base_tx_flags: u32,
    /// Still-open tester-present response/TX-echo discard windows (ADR-137
    /// fourth Codex-review fix / round-4 restructure), each one frozen from
    /// one specific successful tester-present send -- see
    /// [`ResidualTesterPresentDiscard`]'s own doc comment. Lives independently
    /// of `tester_present_state` on purpose: a discard window's lifetime is
    /// per-*send* (it opens at that send and closes at its own `until`),
    /// while `tester_present_state`'s lifetime is per-*configuration* (it
    /// changes on every arm/re-arm/disarm/clear). Storing a window inside a
    /// `TesterPresentState` variant (the rounds 1-3 design) forced per-send
    /// data to be hand-carried through every configuration transition one
    /// Codex-review round at a time, and still had two latent bugs of that
    /// exact class (see `dispatch_due_tester_present`'s own push site).
    /// Pushed to (never overwritten) by every successful tester-present send
    /// (`dispatch_due_tester_present`, `handle_start_comm`,
    /// `handle_update_param`'s re-arm); a *failed* send never pushes, so a
    /// still-open prior window is never dropped by a later failure. Pruned
    /// (`retain(|r| now < r.window.until)`) at every push site before the
    /// push; a same-signature push extends an existing entry's deadline
    /// in place instead of appending a duplicate (`push_open_tp_discard`,
    /// `events.rs`, ADR-137 fifth Codex-review fix) -- there is no numeric
    /// cap, since one was found to evict still-unexpired entries under a
    /// long `CP_P2Max` combined with frequent same-`same_wire_behavior`-
    /// excluded-field promotions. `build_cll_rx_entries` (`events.rs`)
    /// discards a frame that matches ANY entry in this list
    /// (not first-match-wins the way the surrounding tier scan is): each
    /// entry is an independently elicited send, not a precedence chain.
    /// Cleared on a full teardown (`DisconnectComLogicalLink`,
    /// `DestroyComLogicalLink` drops it along with the whole
    /// `LogicalLinkState`) -- but deliberately NOT cleared by `CoptStopcomm`
    /// (`handle_stop_comm`, `events.rs`): stopcomm's "no further tester
    /// presents will be sent" governs future sends, not replies already
    /// elicited by sends that already went out.
    open_tp_discards: Vec<ResidualTesterPresentDiscard>,
    /// Working UniqueRespIdTable — written by `SetUniqueRespIdTable`, read by
    /// `GetUniqueRespIdTable`. Promoted to `active_unique_resp_id_table` at
    /// this CLL's own `ConnectComLogicalLink` and at `CoptUpdateparam`
    /// execution (ADR-068, mirroring the `working`/`active` `ComParamSet`
    /// split, ADR-067) — `SetUniqueRespIdTable` itself never touches hardware.
    working_unique_resp_id_table: Vec<EcuUniqueRespEntry>,
    /// Active UniqueRespIdTable — snapshot of the table actually reflected by
    /// this CLL's installed ISO15765 `FLOW_CONTROL_FILTER`s (and by RX
    /// routing / TX addressing at any given moment). A `CoptSendrecv`/
    /// `CoptStartcomm` always snapshots THIS table at `StartComPrimitive`
    /// call time, regardless of `temp_param_update` — unlike ComParamSet's
    /// Working/Active, there is no Working-side COP binding for the table
    /// (ADR-068, ADR-067 claim G). `CoptRestoreParam` copies this back to
    /// `working_unique_resp_id_table`; a temp COP's completion never touches
    /// either table.
    active_unique_resp_id_table: Vec<EcuUniqueRespEntry>,
    /// Filter IDs of the point-to-point `FLOW_CONTROL_FILTER`s currently installed
    /// on `channel_id` for this CLL's `active_unique_resp_id_table` entries (ISO15765
    /// only). Empty when the table has no entry with a full `CP_CanRespUSDTId` +
    /// `CP_CanPhysReqId` address pair — this CLL then has no `FLOW_CONTROL_FILTER`
    /// of its own on the shared channel at all (the zero-mask pass-all fallback
    /// that used to cover this case was spec-non-conformant and has been removed
    /// entirely, ADR-122).
    unique_resp_filter_ids: Vec<MessageFilterId>,
    /// cop_handles that have been cancelled but not yet dequeued by the poll task.
    /// The poll task checks this set before executing each TxItem and emits
    /// PduCopstCancelled then skips the item when the handle is present.
    cancelled_cops: HashSet<u32>,
    /// LockMask bits currently held by this CLL (see LOCK_PHYSICAL_COM_PARAMS /
    /// LOCK_PHYSICAL_TX_QUEUE).  Cleared automatically when the CLL is destroyed.
    held_lock_mask: u32,
    /// Most recent PDU error event recorded for this link by the poll task,
    /// with the time it was recorded. Surfaced as `ErrorDetail.error_event_data`
    /// on a failing RPC that has this CLL's handle in scope (ADR-105's rich
    /// error model, which replaced polling via `GetLastError`).
    last_error: Option<TrackedError>,
    /// `TxItem`s siphoned off by `dispatch_tx_item` while `tx_suspended()` is
    /// `true` (`PDU_IOCTL_SUSPEND_TX_QUEUE` and/or `LOCK_PHYSICAL_TX_QUEUE`),
    /// in FIFO order. Drained back onto the owning `SharedChannel::tx_queue`
    /// by `PDU_IOCTL_RESUME_TX_QUEUE` / `recompute_lock_tx_suspensions`.
    tx_held: VecDeque<TxItem>,
    /// `true` while a client has suspended this CLL's TX dispatch via
    /// `PDU_IOCTL_SUSPEND_TX_QUEUE` (unset only by `PDU_IOCTL_RESUME_TX_QUEUE`
    /// or a teardown/reset path) -- independent of `tx_suspended_by_lock` so
    /// neither source can clobber the other's suspension (ADR-123).
    tx_suspended_by_ioctl: bool,
    /// `true` while another ComLogicalLink sharing this physical resource
    /// holds `LOCK_PHYSICAL_TX_QUEUE` (ISO 22900-2 §9.4.13.3 use case 1).
    /// Owned exclusively by `recompute_lock_tx_suspensions` -- never set/cleared
    /// anywhere else (ADR-123).
    tx_suspended_by_lock: bool,
    /// `true` while this CLL's TX dispatch is suspended by
    /// `CP_SuspendQueueOnError` reacting to unhandled negative-response/
    /// timeout content on this CLL (ADR-147). Cleared by the four explicit
    /// escapes: `cancel_held_tx_items`'s ioctl-class reset branch, an
    /// explicit `PDU_IOCTL_RESUME_TX_QUEUE`, a `CoptUpdateparam` promotion
    /// that lands Active `CP_SuspendQueueOnError == 0`, and a hard channel
    /// error going offline for this CLL — plus a later positive response
    /// classification (`QueueErrorClass::Positive`), which clears it once it
    /// survives this same sequence's apply-time gate (see below; the fourth
    /// amendment made this gate symmetric, so "always clears it
    /// unconditionally" is no longer accurate -- a `Positive` fold can now be
    /// discarded as stale exactly like a `Suspend` fold can). Set (in
    /// addition to being cleared) by the receive-phase TIMEOUT HOOK
    /// (`wait_for_expected_response_inner`) when live `CP_SuspendQueueOnError
    /// == 1`. Scoped and cleared entirely within this CLL alone (no
    /// cross-CLL inputs), so unlike `tx_suspended_by_lock` it is never
    /// touched by `recompute_lock_tx_suspensions`'s sweep (ADR-147).
    tx_suspended_by_error: bool,
    /// Monotonic sequence, bumped ONLY by the four explicit-clear sites
    /// (`ioctl_resume_tx_queue` in rpc_misc.rs, `cancel_held_tx_items`'s
    /// `reset_suspended`-gated branch, `handle_update_param`'s
    /// promotion-clear block, and `handle_channel_hard_error`'s per-CLL
    /// offline handling -- all via `LogicalLinkState::clear_error_suspension`,
    /// unconditionally, in the same critical section as the
    /// `tx_suspended_by_error` clear itself), compared against the sequence
    /// captured at the moment a frame's classification last folded `Suspend`
    /// into a pass's per-CLL entry (`CllRxEntry::suspend_seq`) -- NOT at
    /// pass-snapshot time (ADR-147 fifth amendment, split direction-specific
    /// anchors).
    ///
    /// Governing rule (ADR-147 fifth amendment, superseding the fourth
    /// amendment's single unified counter): explicit clears invalidate
    /// `Suspend` content anchored at EXPOSURE, not fold time -- a clear
    /// cannot invalidate what the client never saw, and the fold-time
    /// capture site (`poll_rx_inner`, immediately after `bind_frame` returns)
    /// always runs strictly before this frame is delivered/exposed to the
    /// client, so a clear landing in the gap between fold and capture is a
    /// genuinely NEW incident from the clear's own perspective, not a race to
    /// fix -- absorbing the bumped value and still applying `Suspend` is the
    /// correct outcome. `Positive` content is anchored differently (at
    /// BATCH-READ evidence order, via `error_set_seq` below) and is never
    /// compared against this counter at all. The unified `error_state_seq`
    /// (fourth amendment) was superseded because one capture point cannot
    /// correctly serve both anchors simultaneously -- see `error_set_seq`'s
    /// own doc comment and ADR-147 for the full argument.
    ///
    /// The bump is unconditional regardless of whether `tx_suspended_by_error`
    /// was actually `true` beforehand -- see `clear_error_suspension`'s own
    /// doc comment for why (unchanged reasoning from the third/fourth
    /// amendments, which this field's split does not revisit).
    /// `finalize_connected_link` (rpc_link.rs) additionally resets
    /// `tx_suspended_by_error = false` at connect time, in the same
    /// publication critical section, WITHOUT bumping this sequence -- the
    /// existing `connect_generation` check at apply time already discards any
    /// old-session pass post-reconnect on its own. `poll_rx_inner`'s
    /// end-of-pass `CP_SuspendQueueOnError` classification apply (events.rs)
    /// gates `Suspend` on this value matching the fold-time-captured
    /// `CllRxEntry::suspend_seq` -- see `queue_error_class_to_apply`'s own
    /// doc comment.
    error_clear_seq: u64,
    /// Monotonic sequence, bumped ONLY by the receive-phase timeout hook
    /// (`wait_for_expected_response_inner`, events.rs) when it SETS
    /// `tx_suspended_by_error = true`, in the same critical section as the
    /// set itself, before the resulting error event is emitted (ADR-147
    /// fifth amendment, split direction-specific anchors).
    ///
    /// Governing rule: autonomous sets (the timeout hook) invalidate
    /// `Positive` content anchored at BATCH-READ evidence order -- a
    /// positive response can only genuinely resume a suspension it postdates
    /// on the wire, and every frame in one `PassThruReadMsgs` batch
    /// physically arrived before that read call returned, so the correct
    /// anchor for `Positive` is this counter's value as of that batch's own
    /// READ-COMPLETION. `CllRxEntry::set_seq_at_read` (an `Option<u64>`) is
    /// NOT captured at `build_cll_rx_entries` construction time (ADR-147
    /// sixth amendment, pre-read capture -- superseding the fifth
    /// amendment's construction-time capture, which still ran strictly
    /// AFTER `events.rs::poll_rx_inner`'s `read_messages` call returned,
    /// with a genuine intervening `.await` a concurrent bump could land in).
    /// Exact "at read-completion" equality is structurally unachievable
    /// across the two different async locks involved (`ChannelPollCtx::api`
    /// for the read, this struct's own `logical_links` map for the
    /// counter) -- no single instant can be inside both critical sections at
    /// once. What IS achievable is capturing BEFORE the read even starts,
    /// satisfying the inequality `T_capture <= T_read-start <=
    /// T_read-completion`: a bump strictly after read-completion is always
    /// caught (it necessarily happened after the pre-read capture too),
    /// while a bump landing between the pre-read capture and
    /// read-completion is absorbed (over-discard, fail-CLOSED,
    /// self-correcting on the next batch) rather than missed (fail-OPEN).
    /// `Suspend` is never compared against this counter -- see
    /// `error_clear_seq`'s own doc comment for why the two directions cannot
    /// share one capture point. `poll_rx_inner`'s end-of-pass apply gates
    /// `Positive` on `entry.set_seq_at_read == Some(link.error_set_seq)` --
    /// see `queue_error_class_to_apply`'s own doc comment.
    error_set_seq: u64,
    /// Client-installed message filters (`PDU_IOCTL_START_MSG_FILTER`/
    /// `_STOP_MSG_FILTER`/`_CLEAR_MSG_FILTER`), keyed by the client-supplied
    /// `FilterNumber`. Kept separate from `unique_resp_filter_ids` so a
    /// client's STOP/CLEAR never touches this service's own ADR-005/008/039
    /// filters. A single `FilterNumber` can map to more than one
    /// `MessageFilterId`: on a CAN channel connected with `CAN_29BIT_ID`/
    /// `CAN_ID_BOTH`, `ioctl_start_msg_filter` installs one hardware filter
    /// per applicable `TxFlags`/ID-width variant (mirroring
    /// `install_pass_all_filter`), so the client's mask/pattern is evaluated
    /// against every ID width the channel accepts (Codex-review fix).
    client_filters: HashMap<u32, Vec<MessageFilterId>>,
    /// `MsgId`s of SAE J2534-2 clause 14 Repeat Messaging slots this CLL
    /// started (`PDU_IOCTL_START_REPEAT_MESSAGE`) and has not yet stopped
    /// (ADR-165/Phase 12) -- mirrors `client_filters`' own "hardware state
    /// this CLL owns, cleaned up the same way" pattern: the device assigns
    /// each `MsgId` at `START` time (no service-side allocation), and
    /// `PDU_IOCTL_QUERY_REPEAT_MESSAGE`/`_STOP_REPEAT_MESSAGE` validate a
    /// caller-supplied `MsgId` against this set before forwarding to the
    /// device, so a client cannot query/stop a sibling CLL's slot on a
    /// shared physical channel. Drained (best-effort `STOP` per entry) in
    /// `DestroyComLogicalLink`'s/`DisconnectComLogicalLink`'s existing
    /// `client_filters` teardown loops.
    repeat_message_ids: Vec<u32>,
    /// Filters accepted by `PDU_IOCTL_START_MSG_FILTER` while this CLL is not
    /// yet connected (`channel_id.is_none()`), keyed by the client-supplied
    /// `FilterNumber`. ISO 22900-2 §9.5.13/§9.4.11.2 d) let a client
    /// pre-configure filters before `PDUConnect`; they have no hardware
    /// representation yet, so they are kept separate from `client_filters`
    /// rather than folded into it as an `Option`-valued entry -- every
    /// existing `client_filters.is_empty()` scan and teardown loop (ADR-082's
    /// sole-channel-ownership invariant, `DestroyComLogicalLink`/
    /// `DisconnectComLogicalLink`'s hardware-filter teardown) means "actually
    /// installed on hardware", and folding pending entries in would silently
    /// change all of those. Invariant: non-empty only while `channel_id` is
    /// `None`; `ConnectComLogicalLink` drains this into `client_filters` (new
    /// channel) or fails the connect outright (joining an existing, already-
    /// shared channel -- ADR-082 forbids a filtered CLL from sharing) rather
    /// than ever installing a subset. Not touched by `PDU_IOCTL_RESET`,
    /// matching `working`/`working_unique_resp_id_table`'s existing "reset
    /// only touches hardware-reflecting state" policy.
    pending_client_filters: HashMap<u32, vci_service_interface::IoFilter>,
    /// Per-CLL response-binding registry (ADR-100 Decision §1). Populated by
    /// `wait_for_expected_response`'s insert/remove around each receive
    /// phase, cleared wholesale by `cancel_link_cops`. Consulted every poll
    /// pass by `bind_frame`/`bind_registrant` (ADR-100 Decision §3, S3) for
    /// attribution, and by `rpc_get_status`'s `CopHandle` branch (ADR-100
    /// Decision §2) to detect a live detached tier-2 registrant.
    registrants: Vec<CopRegistrant>,
    /// Monotonic per-CLL counter stamped into `CopRegistrant::registration_seq`
    /// at insertion, under this same `logical_links` lock (ADR-100 Decision
    /// §3, resolved (a)).
    next_registrant_seq: u64,
    /// SAE J2534-2 clause 16 SAE J1939 address-claim/defend state (ADR-179
    /// Decision 3): the address this CLL currently holds claimed, or `None`
    /// before a first successful claim (or after this CLL's claim list is
    /// exhausted / this CLL is not a J1939 CLL at all). Written back by
    /// `handle_start_comm`'s claim loop on a `J1939_ADDRESS_CLAIMED`
    /// indication, alongside the same-instant `CP_TesterSourceAddress`
    /// (native `NODE_ADDRESS`) Active-set writeback client code reads this
    /// value through. NOT cleared on disconnect (`rpc_disconnect_com_logical_
    /// link`/`rpc_destroy_com_logical_link` only clear the SHARED
    /// `SharedChannel::j1939_claims` routing map) -- reset to `None` at the
    /// NEXT `ConnectComLogicalLink`'s finalization instead (including a
    /// same-handle reconnect), alongside `j1939_claim_cursor = 0` (ADR-180
    /// Decision 17). Also cleared on a spontaneous `J1939_ADDRESS_LOST`
    /// mid-session (the device was out-defended) while the retry loop
    /// re-runs from `j1939_claim_cursor = 0`.
    j1939_claimed_address: Option<u8>,
    /// Index into this CLL's staged `CP_J1939PreferredAddress` candidate
    /// list (ADR-179 Decision 3): the entry the claim loop is currently
    /// attempting, or -- once list-exhausted -- one past the last entry.
    /// `0` on a fresh `StartComm`. Advanced by one on each
    /// `J1939_ADDRESS_LOST` outcome (the claim loop retries the NEXT list
    /// entry); reset to `0` at the start of every fresh claim attempt
    /// (a new `StartComm` after `StopComm`, or a spontaneous post-claim
    /// `J1939_ADDRESS_LOST` restarting the retry loop from the top of the
    /// list, per ADR-179 Decision 3's "later, spontaneous `_LOST`" case).
    j1939_claim_cursor: usize,
    /// This CLL's REAL, structural negotiation posture, established once at
    /// its own `CoptStartcomm` (ADR-180 Decision 18, corrected by a later
    /// fix -- see `J1939NegotiationPosture`'s own doc comment). Unlike
    /// `j1939_negotiated_unclaimed_for`'s `params` argument, which may be a
    /// `temp_param_update=1` call's Working snapshot, this field cannot be
    /// spoofed per-call: it is set once, from the CLL's own `CoptStartcomm`
    /// execution, and consulted directly by `j1939_negotiated_unclaimed_for`
    /// so neither direction of a Temp-bound `CoptStartcomm` (opting IN or
    /// OUT of negotiation) can desynchronize later ordinary calls' view of
    /// this CLL's negotiation posture from what its own real `CoptStartcomm`
    /// actually decided. `Undecided` before any `CoptStartcomm`; reset to
    /// `Undecided` at the next `ConnectComLogicalLink`'s finalization
    /// alongside `j1939_claimed_address`/`j1939_claim_cursor` (same
    /// reconnect reasoning as ADR-180 Decision 17). NOT cleared on a
    /// spontaneous `J1939_ADDRESS_LOST` mid-session -- the CLL is still
    /// structurally negotiation-managed, only its claim was lost.
    j1939_negotiation_posture: J1939NegotiationPosture,
    /// SAE J2534-2 clause 19 TP2.0 connection state (ADR-188/Phase 7 Stage
    /// 7a): `None` for a non-TP2.0 CLL, or before this CLL's first
    /// `CoptStartcomm`; `Some` once `handle_start_comm` has issued
    /// `IOCTL_REQUEST_CONNECTION` for it. Mirrors `repeat_message_ids`'s own
    /// placement (ADR-165 Decision 4): per-CLL hardware-reflecting state,
    /// under this same `logical_links` lock. NOT cleared on disconnect --
    /// reset to `None` at the next `ConnectComLogicalLink`'s finalization,
    /// the same `j1939_claimed_address` reconnect reasoning (ADR-180
    /// Decision 17).
    tp20_connection: Option<Tp20Connection>,
    /// SAE J2534-2 clause 19.3.2.3 TP2.0 broadcast periodic re-trigger
    /// (ADR-192/Phase 7 Stage 7c): `Some` while a cyclic broadcast
    /// `CoptSendrecv` (`num_send_cycles == -1`, `CP_TP20BroadcastAddress`
    /// resolved) owns a live native `PassThruStartPeriodicMsg` message on
    /// this CLL's physical channel -- deliberately reintroducing
    /// `j2534-0404`'s `start_periodic_message`/`stop_periodic_message`/
    /// `PeriodicMessageId` API for this one feature (ADR-093 left it
    /// untouched for exactly this kind of future native-surface use; this is
    /// NOT tester-present's own removed periodic-message state, see
    /// [`Tp20BroadcastPeriodic`]'s own doc comment). Started directly by
    /// `rpc_start_com_primitive`'s `CoptSendrecv` handling (bypassing the
    /// ordinary `tx_queue`/poll-task dispatch pipeline entirely -- a
    /// software cyclic loop calling `PassThruWriteMsgs` once per configured
    /// period would re-emit the fixed five-frame burst on every tick instead
    /// of a single alternating frame, observably wrong on the wire; ADR-192
    /// Decision item 2). Stopped (`stop_periodic_message`, set back to
    /// `None`) by every COP-level mechanism this service already has,
    /// per ADR-192 Decision item 2's governing rule (a broadcast periodic is
    /// client-visibly a cyclic ComPrimitive, so every such mechanism must
    /// terminate or track it exactly as it would any other executing COP):
    /// `CoptCancel` of the owning COP; `DisconnectComLogicalLink`/
    /// `DestroyComLogicalLink` of this CLL (`rpc_link.rs`, including the
    /// shared-channel case -- the exact ADR-010 leak class, deliberately
    /// reinstated for this feature); `rpc_misc.rs`'s `CLEAR_PERIODIC_MSGS`
    /// reconciliation when the native call it issues invalidates this
    /// message device-side as a side effect; `events::handle_channel_hard_
    /// error` when the physical channel goes offline; and, as of Fix A
    /// (design-advisor consult, Codex review round 3), TX-dispatch
    /// suspension taking effect (`PDU_IOCTL_SUSPEND_TX_QUEUE` or
    /// `CP_SuspendQueueOnError` at its three authoritative transition sites)
    /// via `J2534Service::terminate_tp20_broadcast_periodic_for_suspension`
    /// (`rpc_misc.rs`) -- a live broadcast periodic is stopped and finalized
    /// when suspension takes effect, not merely blocked from a future re-arm,
    /// so `PDU_IOCTL_RESUME_TX_QUEUE` has nothing left to resume for it.
    ///
    /// A failed native stop during shared-channel teardown or a hard channel
    /// error (the two cases where this CLL's own tracking is about to
    /// disappear, unlike suspension/`CoptCancel`, which retain the entry on
    /// this still-live CLL instead) is preserved in the physical channel's
    /// own `SharedChannel::leaked_periodic_message_ids` (Fix B, design-
    /// advisor consult, Codex review round 3) -- mirroring
    /// `leaked_repeat_message_ids`'s own precedent exactly, so it keeps
    /// blocking a `LOCK_PHYSICAL_TX_QUEUE` grant on that physical resource
    /// until either a later retry succeeds or the channel itself closes.
    tp20_broadcast_periodic: Option<Tp20BroadcastPeriodic>,
}

/// [`LogicalLinkState::tp20_broadcast_periodic`] (ADR-192/Phase 7 Stage 7c):
/// the COP that owns this CLL's live native broadcast periodic message, and
/// the `PeriodicMessageId` `j2534_0404::start_periodic_message` returned for
/// it -- everything `stop_periodic_message` needs to tear it back down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Tp20BroadcastPeriodic {
    pub(super) cop_handle: u32,
    /// `None` while a `StartComPrimitive` reservation is in flight (written
    /// by `reserve_tp20_broadcast_periodic` before the native
    /// `PassThruStartPeriodicMsg` call has returned) -- an out-of-band
    /// sentinel, not an in-band magic value, so it cannot collide with any
    /// real adapter-assigned id. `Some(id)` once
    /// `finalize_or_orphan_broadcast_periodic_start_locked` commits the real
    /// id the native call returned -- which, as of ADR-193 (Codex review
    /// round 15, PR #101), happens while `self.api` is still held, and every
    /// terminator makes its native-stop/COP-finalization DECISION about this
    /// field only under that same guard -- either by taking the entry under
    /// it (`take_broadcast_periodic_under_api_locked`, `rpc_misc.rs`, for
    /// id-targeted `CoptCancel`) or by taking it earlier, atomically with its
    /// own `channel_id`/`channel_key` capture, and then acquiring the guard
    /// before deciding (`DisconnectComLogicalLink`,
    /// `terminate_tp20_broadcast_periodic_for_suspension`'s callers -- ADR-193
    /// Decision item 3). That mutex is the serialization fence for the whole
    /// in-flight-start window: no site can resolve this sentinel and report
    /// the owning COP terminal while the native `PassThruStartPeriodicMsg`
    /// call is still in flight and about to emit clause 19.3.2.3's five-frame
    /// burst.
    /// SAE J2534-1 clause 7.2.7.2's `pMsgID` output
    /// parameter is merely "assigned by the DLL", with no stated floor or
    /// reserved value, so a conformant adapter may legitimately return
    /// `PeriodicMessageId(0)` for a genuine live message -- every comparison
    /// site below therefore tests `.is_none()`/`.is_some()`, never
    /// `== PeriodicMessageId(0)` (Codex review, PR #101).
    pub(super) message_id: Option<j2534_0404::PeriodicMessageId>,
    /// `J2534Service::periodic_clear_epoch`'s value read (while still
    /// holding `self.api`) immediately after the native
    /// `start_periodic_message` call that produced `message_id` returned
    /// successfully (`rpc_primitive.rs::rpc_start_com_primitive`'s
    /// broadcast-periodic branch, via
    /// `finalize_or_orphan_broadcast_periodic_start_locked`). Meaningful only
    /// relative to `periodic_clear_epoch`'s current value -- not a
    /// timestamp or a value comparable across restarts -- and used by
    /// `CLEAR_PERIODIC_MSGS`'s reconciliation scan (`rpc_misc.rs`) to tell
    /// whether this entry's native start provably preceded that clear's own
    /// native `clear_periodic_messages` call (`started_epoch <
    /// clear_generation`, so the entry must be finalized) or raced/followed
    /// it (`started_epoch >= clear_generation`, so the entry is still live
    /// and must be left tracked). The reservation written by
    /// `reserve_tp20_broadcast_periodic` before the native start has even
    /// been attempted uses `started_epoch: 0` -- indistinguishable in value
    /// from a genuinely-early real commit, but harmless, since a reservation
    /// is always identified by `message_id.is_none()` first (see every
    /// comparison site), never by `started_epoch` alone.
    pub(super) started_epoch: u64,
    /// Meaningful ONLY while `message_id.is_none()` (the reservation case,
    /// written by `reserve_tp20_broadcast_periodic` before the native
    /// `start_periodic_message` call has returned). `0` means "no
    /// `CLEAR_PERIODIC_MSGS` scan has passed over this reservation yet" --
    /// a safe sentinel value since `J2534Service::periodic_clear_epoch`'s
    /// `clear_generation` is a post-increment counter starting at 1, so a
    /// real recorded generation is always `>= 1`. Always `0` on every
    /// committed entry (`message_id.is_some()`).
    ///
    /// At scan time a reservation's ordering against a given clear is
    /// undecidable (its native call hasn't returned yet, so no
    /// `started_epoch` exists to compare), so `CLEAR_PERIODIC_MSGS`'s
    /// reconciliation scan (`rpc_misc.rs`) defers the decision by recording
    /// `max(pending_clear_generation, clear_generation)` here instead of
    /// finalizing -- the `.max` matters because a second racing clear must
    /// not regress an already-recorded higher generation from a first
    /// clear. `finalize_or_orphan_broadcast_periodic_start_locked`
    /// (`rpc_primitive.rs`) resolves the reservation's true fate once its own
    /// native call completes and `started_epoch` becomes known:
    /// `started_epoch < pending_clear_generation` means at least one
    /// recorded clear's native call ran after this start's own native call
    /// returned, so the message is already dead device-side (take + emit
    /// `PduCopstFinished`, no native stop needed); `started_epoch >=
    /// pending_clear_generation` (including the `pending_clear_generation
    /// == 0` case, i.e. no clear ever scanned it) means an ordinary live
    /// commit.
    pub(super) pending_clear_generation: u64,
}

/// The "captured session" identity a caller of
/// `J2534Service::restore_or_leak_track_broadcast_periodic` (`rpc_misc.rs`)
/// took from the same `logical_links` critical section a
/// [`Tp20BroadcastPeriodic`] entry itself came from, before releasing that
/// lock to make the native `stop_periodic_message` call that then failed
/// (Codex review, round 11, PR #101). Bundled into one struct purely to
/// keep that helper's own argument count under clippy's
/// `too_many_arguments` threshold -- not a broader abstraction; see the
/// helper's own doc comment for the full restore-vs-leak-track reasoning
/// that uses these fields.
#[derive(Debug, Clone, Copy)]
pub(super) struct CapturedBroadcastPeriodicSession {
    pub(super) connect_generation: u64,
    pub(super) channel_key: Option<ChannelKey>,
    pub(super) channel_id: Option<ChannelId>,
}

/// SAE J2534-2 clause 19 TP2.0 per-CLL connection state (ADR-188/Phase 7
/// Stage 7a; extended by ADR-190/Phase 7 Stage 7b): the RX-ID this CLL
/// requested (the key its physical channel's `SharedChannel::tp20_connections`
/// routing map registers the pending attempt, or -- for a passive arm -- the
/// persistent slot entry, under), the TX-ID the device assigned once
/// established, and the connection's own lifecycle phase.
///
/// `passive` (ADR-190 section 3): `true` marks a CLL's connection state as
/// originating from/governed by the passive-arm lifecycle
/// (`events_tp20_connection::arm_tp20_passive_listener`/
/// `disarm_tp20_passive_listener`) rather than an active
/// `REQUEST_CONNECTION` (`run_tp20_connection_request`) -- `false`
/// everywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Tp20Connection {
    pub(super) requested_rx_id: u32,
    pub(super) established_tx_id: Option<u32>,
    pub(super) phase: Tp20ConnectionPhase,
    pub(super) passive: bool,
}

/// [`Tp20Connection::phase`] (ADR-188/Phase 7 Stage 7a; extended by
/// ADR-190/Phase 7 Stage 7b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tp20ConnectionPhase {
    /// `IOCTL_REQUEST_CONNECTION` issued, awaiting the matching
    /// `CONNECTION_ESTABLISHED`/`_LOST` indication. Never reached by a
    /// passive connection (ADR-190 section 3) -- a passive arm goes straight
    /// to `Listening`, since no application-initiated request is ever
    /// issued.
    Requested,
    /// The device confirmed the connection; `established_tx_id` is `Some`.
    Established,
    /// The connection failed to establish, or was torn down. Never reached
    /// by a passive connection's own loss (ADR-190 section 3) -- see
    /// `Listening`'s own doc comment for why.
    Lost,
    /// ADR-190/Phase 7 Stage 7b: the passive-arm's own resting phase -- set
    /// on arm (before any inbound connection has ever been accepted), and
    /// re-entered from `Established` on a `Lost` outcome instead of the
    /// terminal `Lost` an active connection reaches. Per clause 19.3.3.1,
    /// with both native `SET_CONFIG` params still valid and a slot free, the
    /// device keeps auto-accepting the next inbound connection request and
    /// will emit a fresh `Established` indication for it -- the persistent
    /// `Tp20ConnEntry` (`passive: true`) is what lets that next indication
    /// route correctly back to this same CLL. Never reached by an active
    /// connection.
    Listening,
}

/// This CLL's real, structural J1939 address-negotiation posture, decided
/// once at its own (non-Temp) `CoptStartcomm` and consulted by
/// `j1939_negotiated_unclaimed_for` (`events_j1939_claim.rs`) alongside that
/// predicate's per-call `params` snapshot argument.
///
/// `Undecided`: no real `CoptStartcomm` has yet decided this CLL's
/// negotiation posture (before any `CoptStartcomm`, or -- since this field is
/// only ever written by a real `CoptStartcomm`'s own dispatch -- for the
/// entire life of a CLL that has only ever seen `temp_param_update=1` calls).
/// The predicate falls back to the per-call `params` snapshot in this case.
///
/// `Engaged`: the most recent real `CoptStartcomm` requested J1939 address
/// negotiation. This is ADR-180 Decision 18's original fix: it closes the
/// gap where a Temp-bound `CoptSendrecv` could stage a Working-only
/// `CP_J1939AddressNegotiationRule = 2` to spoof "not negotiated" and bypass
/// the no-claim send gate on a CLL that structurally IS negotiation-managed
/// and still unclaimed.
///
/// `OptedOut`: the most recent real `CoptStartcomm` explicitly requested NO
/// J1939 address negotiation. This is a correction to Decision 18's original
/// two-state (`bool`) design: with only `Engaged`/not-`Engaged`, a
/// `temp_param_update=1` `CoptStartcomm` that opted out while Active still
/// held the stale negotiation-ENABLED default left this field `false` (i.e.
/// indistinguishable from `Undecided`), so the predicate fell back to
/// Active's stale enabled value and wrongly treated the CLL as still
/// negotiated-and-unclaimed for every later ordinary call -- permanently
/// blocking `CoptSendrecv`, `PDU_IOCTL_START_REPEAT_MESSAGE`, and the
/// tester-present filter on that CLL. The third state lets a real opt-out
/// StartComm record its decision as authoritatively as an opt-in one, so the
/// predicate stops consulting the stale `params` snapshot once a real
/// `CoptStartcomm` has actually decided the question either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::service) enum J1939NegotiationPosture {
    #[default]
    Undecided,
    Engaged,
    OptedOut,
}

/// `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`'s `QueueMode` (`T_PDU_QUEUE_MODE`),
/// narrowed to the two ring-buffer eviction policies this adapter's bounded
/// `VecDeque<CllQueueItem>` per-CLL event queue can actually implement.
///
/// `PDU_QUE_CIRCULAR` maps to `OverwriteOldest` (the pre-existing default
/// behavior) and `PDU_QUE_LIMITED` maps to `DiscardNewest`; `PDU_QUE_UNLIMITED`
/// also maps to `OverwriteOldest` since this adapter's RX buffer is always
/// bounded -- a true unbounded queue is not representable by the fixed-size
/// ring buffer this service uses (see
/// `J2534Service::ioctl_set_event_queue_properties` in `rpc_misc.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum EventQueueMode {
    /// Pop the oldest buffered frame to make room for a new one (the
    /// pre-existing, hardcoded `RX_BUF_CAPACITY` behavior).
    #[default]
    OverwriteOldest,
    /// Drop the incoming frame when the buffer is at capacity.
    DiscardNewest,
}

impl LogicalLinkState {
    /// Effective TX-dispatch suspension: `true` if either the client
    /// (`PDU_IOCTL_SUSPEND_TX_QUEUE`) or a sibling CLL's held
    /// `LOCK_PHYSICAL_TX_QUEUE` (ADR-123) currently suspends this CLL.
    /// Checked by `dispatch_tx_item`/`drain_tx_held_backlog` for every item
    /// belonging to this CLL, including cyclic/periodic follow-up cycles.
    fn tx_suspended(&self) -> bool {
        self.tx_suspended_by_ioctl || self.tx_suspended_by_lock || self.tx_suspended_by_error
    }

    /// Clears `tx_suspended_by_error` and bumps `error_clear_seq`
    /// UNCONDITIONALLY, regardless of whether the flag was actually `true`
    /// beforehand (ADR-147 third amendment, capture-at-fold sequencing;
    /// unaffected by the fifth amendment's split of the counter this bumps
    /// into `error_clear_seq`/`error_set_seq` -- this method only ever
    /// bumps the clear-specific half).
    ///
    /// Two prior amendments each closed one race by tuning this bump alone
    /// and reopened another: bumping unconditionally (the first amendment)
    /// let a resume of an already-resolved episode discard a genuinely
    /// fresh `Suspend` classification landing in the same poll pass (a
    /// fail-open bug); gating the bump on `was_suspended` (the second
    /// amendment) fixed that, but let the FIRST suspend-worthy frame of a
    /// new episode -- classified while this flag was still `false` -- pass
    /// through the disabled gate and let an immediate client resume be
    /// silently undone by that pass's own end-of-pass writeback. The actual
    /// fix is entirely on the CAPTURE side: `CllRxEntry::suspend_seq` is now
    /// captured at the exact moment a frame's classification folds `Suspend`
    /// into the entry, not at pass-snapshot time -- see that field's doc
    /// comment. With capture correctly fold-time-scoped, the bump can safely
    /// go back to unconditional: a bump can only ever invalidate a
    /// classification whose fold predates it (captured a now-stale seq), and
    /// can never touch a classification whose fold happens after (which
    /// captures the bumped seq itself and so still matches at apply time).
    /// This is also exactly the EXPOSURE anchor the fifth amendment
    /// confirmed correct for `Suspend`: capture always precedes this frame's
    /// delivery, so a clear absorbed in the fold-to-capture gap necessarily
    /// predates the frame's exposure to the client.
    ///
    /// Returns whether `tx_suspended_by_error` itself was `true` before this
    /// call, for the caller's own use -- no current call site consumes this
    /// beyond deciding whether a wake may be needed. It is NOT equivalent to
    /// "effective `tx_suspended()` changed": that also depends on
    /// `tx_suspended_by_ioctl`/`tx_suspended_by_lock`, which this per-source
    /// flag alone cannot reflect, so callers that need a wake-gating
    /// decision (e.g. `events::handle_update_param`) compute their own
    /// before/after `tx_suspended()` comparison instead of reusing this
    /// return value.
    fn clear_error_suspension(&mut self) -> bool {
        let was_suspended = self.tx_suspended_by_error;
        self.tx_suspended_by_error = false;
        self.error_clear_seq += 1;
        was_suspended
    }

    /// `true` once `PDUConnect` has been called for this CLL and it either has
    /// finished (`connected`) or is still in flight (`connect_in_flight`).
    /// ISO 22900-2 §9.5.16-style guards (currently only
    /// `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`) should check this, not `connected`
    /// alone, so the guard also covers the window between the RPC being accepted
    /// and `finalize_connected_link` publishing `connected = true`.
    fn pdu_connect_begun(&self) -> bool {
        self.connected || self.connect_in_flight.upgrade().is_some()
    }

    /// ADR-157 Plane B normalization primitive: the base J2534-1 protocol id
    /// this link's family/behavior decisions should use, regardless of
    /// whether `hw_protocol_id` itself holds a `_PS`/`_CHx` variant. Gates
    /// on `base_hw_protocol_override` being `Some` directly (ADR-156
    /// Decision 3 addendum/Phase 2b) -- NOT on `pin_select.is_some()` alone,
    /// which only covered the `_PS` route before `_CHx` also populated this
    /// field: `resolve_pin_selection`/`resolve_channel_selection` both
    /// populate `base_hw_protocol_override` together with their own
    /// qualifier field, at `CreateComLogicalLink` time, so this accessor
    /// works for either qualifier route (or neither) without needing to
    /// know which one produced it. `names::resolve_pin_selection`/
    /// `names::resolve_channel_selection`'s own correctly-resolved base id
    /// (ADR-157/Bug 1 fix) preserves the exact SCI variant for a `_PS`/`_CHx`
    /// SAE_J2610 link, unlike the free-function `resources::base_protocol_id`'s
    /// single-representative collapse (or the previous, lossy derivation via
    /// `protocol.j2534_protocol_id()` alone, which collapsed every SCI
    /// variant onto the shared `SCI_MODE` id). Identity (`hw_protocol_id`
    /// itself) for every unqualified link, which is every link that existed
    /// before Phase 2a and every link Phase 2a/2b themselves create for a
    /// default-pins, non-`_CHx` request.
    fn base_hw_protocol_id(&self) -> u32 {
        self.base_hw_protocol_override
            .unwrap_or(self.hw_protocol_id)
    }

    fn view(&self) -> LinkView {
        LinkView {
            channel_key: self.channel_key,
            protocol: self.protocol,
            hw_protocol_id: self.hw_protocol_id,
            software_isotp: self.software_isotp,
            base_hw_protocol_override: self.base_hw_protocol_override,
            connected: self.connected,
            comm_started: self.comm_started,
            raw_mode: self.raw_mode,
            checksum_mode: self.checksum_mode,
            working: self.working.clone(),
            connect_generation: self.connect_generation,
            last_error: self.last_error.clone(),
        }
    }
}

/// Pins `LogicalLinkState::clear_error_suspension`'s current (third
/// amendment, capture-at-fold sequencing) behavior directly: `error_clear_seq`
/// bumps UNCONDITIONALLY, regardless of whether `tx_suspended_by_error` was
/// `true` beforehand -- this is a deliberate INVERSION of the second
/// amendment's conditional-bump behavior (which this module's tests used to
/// pin), now correct again because the staleness fix moved to the capture
/// side (`CllRxEntry::suspend_seq`, fold-time-captured) instead of the bump
/// side. Mirrors `events.rs`'s `queue_error_class_to_apply_tests` module's
/// location/pattern convention -- co-located immediately after the tested
/// method, one cell of the (flag was true / flag was false) truth table per
/// test, both cells now expecting the SAME "always bumps" outcome. Renamed
/// from `error_state_seq` (ADR-147 fifth amendment, split direction-specific
/// anchors) -- this method only ever bumps the clear-specific half.
#[cfg(test)]
mod clear_error_suspension_tests {
    use super::*;

    /// A `LogicalLinkState` with every field at its `CreateComLogicalLink`
    /// default, mirroring `events.rs`'s own `minimal_link()` test helper.
    fn minimal_link() -> LogicalLinkState {
        LogicalLinkState {
            channel_id: None,
            protocol: ChannelProtocol::CAN,
            hw_protocol_id: 0,
            software_isotp: false,
            uudt_channel_id: None,
            uudt_channel_key: None,
            isotp_rx: Arc::new(Mutex::new(HashMap::new())),
            connect_in_flight: std::sync::Weak::new(),
            connected: false,
            comm_started: false,
            raw_mode: false,
            checksum_mode: false,
            connect_generation: 7,
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
            cancelled_cops: HashSet::new(),
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

    /// Flag was `true` beforehand: the clear takes effect, `error_clear_seq`
    /// advances, and the helper reports the flag actually changed.
    #[test]
    fn bumps_seq_and_returns_true_when_flag_was_suspended() {
        let mut link = minimal_link();
        link.tx_suspended_by_error = true;
        link.error_clear_seq = 5;

        let was_suspended = link.clear_error_suspension();

        assert!(was_suspended, "should report the flag was true beforehand");
        assert!(!link.tx_suspended_by_error);
        assert_eq!(
            link.error_clear_seq, 6,
            "the bump is unconditional -- an active suspension existed here, but that is not why it bumped"
        );
    }

    /// Flag was already `false` beforehand: `error_clear_seq` STILL
    /// advances -- this is the inversion from the second amendment's
    /// conditional-bump behavior. Fold-time capture (`CllRxEntry::suspend_seq`)
    /// is what makes an unconditional bump safe again: this call can only
    /// ever invalidate a classification whose fold already happened (it
    /// captured a now-stale seq), never one whose fold happens later (which
    /// captures the bumped seq itself and so still matches at apply time).
    #[test]
    fn bumps_seq_and_returns_false_when_flag_was_not_suspended() {
        let mut link = minimal_link();
        link.tx_suspended_by_error = false;
        link.error_clear_seq = 5;

        let was_suspended = link.clear_error_suspension();

        assert!(
            !was_suspended,
            "should report the flag was already false beforehand"
        );
        assert!(!link.tx_suspended_by_error);
        assert_eq!(
            link.error_clear_seq, 6,
            "the bump is unconditional again (third amendment) -- capture moved to fold time instead"
        );
    }

    /// ADR-157: a non-`_PS` link (`pin_select: None`, every link that existed
    /// before Phase 2a) gets `base_hw_protocol_id() == hw_protocol_id` --
    /// the identity/regression-safety case.
    #[test]
    fn base_hw_protocol_id_is_identity_when_pin_select_is_none() {
        let mut link = minimal_link();
        link.protocol = ChannelProtocol::CAN;
        link.hw_protocol_id = j2534_0404::CAN;
        link.pin_select = None;
        link.base_hw_protocol_override = None;

        assert_eq!(link.base_hw_protocol_id(), j2534_0404::CAN);
    }

    /// ADR-157: a `_PS` link (`pin_select: Some(_)`) normalizes to
    /// `base_hw_protocol_override` -- the base id -- even though
    /// `hw_protocol_id` itself holds the `_PS` variant.
    #[test]
    fn base_hw_protocol_id_normalizes_to_base_when_pin_select_is_some() {
        let mut link = minimal_link();
        link.protocol = ChannelProtocol::CAN;
        link.hw_protocol_id = j2534_0404_sys::bindings::PROTOCOL_CAN_PS;
        link.pin_select = Some(0x0000_0E06);
        link.base_hw_protocol_override = Some(j2534_0404::CAN);

        assert_eq!(link.base_hw_protocol_id(), j2534_0404::CAN);
    }

    /// ADR-157/Bug 1 regression: a `_PS` SAE J2610 SCI link normalizes to
    /// the exact SCI variant (`SCI_A_ENGINE`) via
    /// `base_hw_protocol_override`, NOT the shared `ChannelProtocol`'s
    /// lossy `j2534_protocol_id()` (`SCI_MODE`, ADR-023) that a derivation
    /// off `protocol` alone would produce.
    #[test]
    fn base_hw_protocol_id_preserves_sci_variant_for_a_pin_selected_j2610_link() {
        let mut link = minimal_link();
        link.protocol = ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI;
        link.hw_protocol_id = j2534_0404_sys::bindings::PROTOCOL_J2610_PS;
        link.pin_select = Some(0x0000_0E07);
        link.base_hw_protocol_override = Some(j2534_0404::SCI_A_ENGINE);

        assert_eq!(link.base_hw_protocol_id(), j2534_0404::SCI_A_ENGINE);
        assert_ne!(
            link.base_hw_protocol_id(),
            j2534_0404::SCI_MODE,
            "must not collapse to the lossy shared ChannelProtocol::j2534_protocol_id() value"
        );
    }
}

/// Finds another CLL that holds any bit in `lock_bits` on the same physical
/// resource as `(handle, protocol, channel_key)`, returning its handle and the
/// subset of its `held_lock_mask` that conflicts.
///
/// "Same physical resource" is `channel_key` equality once both CLLs are
/// connected, falling back to hardware-protocol-ID + `pin_select` equality
/// when either side lacks a `channel_key` — this covers pre-connect
/// `LockResource` reservations (`rpc_lock_resource`) and not-yet-created
/// channels (`rpc_connect_com_logical_link`, ADR-045) with the same
/// comparison `SetUniqueRespIdTable` (ADR-043) uses; factored out further as
/// `same_physical_resource`, shared with `recompute_lock_tx_suspensions`
/// (ADR-123). `StartComPrimitive` no longer calls this at all (ADR-123
/// replaces its former synchronous `LOCK_PHYSICAL_TX_QUEUE` hard-reject,
/// ADR-044, with TX-suspend-and-resume via
/// `LogicalLinkState::tx_suspended_by_lock`). `SetComParam` no longer
/// calls this at all either (ADR-110 removes its former synchronous
/// `LOCK_PHYSICAL_COM_PARAMS` rejection, since `SetComParam` only
/// ever writes the in-memory Working set); `CoptUpdateparam`'s own
/// `LOCK_PHYSICAL_COM_PARAMS` conflict check also moved off this call-time
/// path, onto a live, execution-time check inside `handle_update_param`
/// (`events.rs`, ADR-110).
///
/// The comparison uses `LogicalLinkState::hw_protocol_id` (not the service
/// protocol's `j2534_protocol_id()`) so that in software-ISO-TP mode an
/// ISO15765-family CLL and a raw-CAN CLL — which share one physical CAN
/// channel — are treated as the same resource (ADR-046).
///
/// `pin_select` (ADR-157 Bug D fix) is the caller's own (`handle`'s, possibly
/// not-yet-connected) `LogicalLinkState::pin_select` value, threaded through
/// to `same_physical_resource`'s fallback comparison so that two `_PS` links
/// on the same consolidated `_PS` protocol id but genuinely different pins
/// (ADR-156 Decision 2 explicitly allows simultaneous `_PS` opens differing
/// only in pins) are never falsely treated as the same physical resource
/// before either side has a `channel_key` yet.
fn find_physical_lock_holder(
    links: &HashMap<u32, LogicalLinkState>,
    handle: u32,
    hw_protocol_id: u32,
    pin_select: Option<u32>,
    channel_key: Option<ChannelKey>,
    lock_bits: u32,
) -> Option<(u32, u32)> {
    links.iter().find_map(|(&h, l)| {
        if h == handle {
            return None;
        }
        let held = l.held_lock_mask & lock_bits;
        if held == 0 {
            return None;
        }
        same_physical_resource(
            (hw_protocol_id, pin_select, channel_key),
            (l.hw_protocol_id, l.pin_select, l.channel_key),
        )
        .then_some((h, held))
    })
}

/// "Same physical resource" predicate shared by `find_physical_lock_holder`
/// and `recompute_lock_tx_suspensions`, factored out so the two can never
/// drift apart: `channel_key` equality once both sides are connected,
/// falling back to `hw_protocol_id` + `pin_select` equality when either side
/// lacks a `channel_key` yet (see `find_physical_lock_holder`'s doc comment
/// for why). `pin_select` is `None` for every non-`_PS` link (including every
/// software-ISO-TP link, ADR-157's accepted residual: `_PS` links are
/// hardware-ISO-TP only) so `None == None` on that second element reproduces
/// today's exact `hw_protocol_id`-only fallback behavior for every case this
/// predicate previously handled — the widening only changes behavior when
/// BOTH sides are genuinely `_PS` links with differing pin selections.
///
/// Neither `baud_rate` nor the CAN FD effective data-phase rate
/// participates in this comparison, in either the `channel_key` branch or
/// the fallback branch -- and that omission is deliberate, not an
/// oversight (verified: Codex review, PR #30, declined as a false positive
/// on exactly this reasoning). Both are `BUSTYPE_UNUM32`-class ComParams
/// (`comparam_support.rs`'s `BUSTYPE_UNUM32` list, `CP_Baudrate`/
/// `CP_CANFDBaudrate`) -- precisely the configuration class
/// `LOCK_PHYSICAL_COM_PARAMS` exists to protect (ISO 22900-2 §9.4.13.3).
/// Widening this predicate to also compare either rate would let a CLL
/// bypass another CLL's physical-ComParam lock simply by staging a
/// different value for the very parameter class the lock protects --
/// defeating the lock's purpose rather than sharpening it. `pin_select`
/// (ADR-157 Bug D) is different in kind, not merely degree: different pins
/// are different physical wires, a genuinely distinct physical bus, while
/// the same pins at a different rate are the same wires being asked to run
/// a conflicting configuration -- exactly the contention this lock exists
/// to prevent. That is the organizing principle behind `ChannelKey` and
/// this predicate answering different questions on purpose: `ChannelKey`
/// (rate-sensitive, since a native channel has exactly one bit timing)
/// answers "can these two CLLs ride one native channel," while this
/// predicate (rate-insensitive, since contention is physical, not
/// configuration) answers "are these two CLLs contending for the same
/// wires." `baud_rate` has never been part of this fallback comparison
/// since long before ADR-157 threaded `pin_select` through it -- not an
/// oversight either, but the deliberate other half of the same design.
///
/// **ADR-158 correction (Codex review, PR #30, round 2):** the
/// `channel_key` branch used to compare the FULL `ChannelKey` tuple
/// (`a_ck == b_ck`), which -- unlike the fallback branch just described --
/// WAS rate-sensitive, contradicting this doc comment's own stated design
/// (a self-contradiction the prior paragraph's claim "in either... branch"
/// didn't actually hold for in code, only in intent). Concretely: two
/// `FD_CAN_PS` CLLs on the same explicit pins and the same arbitration
/// `baud_rate` but staging different `CP_CANFDBaudrate` values connect to
/// two DISTINCT physical channels (by design -- `ChannelKey`'s own 4th
/// element exists precisely so they don't collapse onto one shared
/// channel), so their `channel_key`s differ and the old full-tuple compare
/// answered "not the same physical resource" even though both channels
/// genuinely contend for the same J1962 pins. A `LOCK_PHYSICAL_COM_PARAMS`/
/// `LOCK_PHYSICAL_TX_QUEUE` held by one would then never block the other.
/// Now fixed to extract only the two physical-identity components
/// (`hw_protocol_id`, `pin_select` -- elements 0 and 2) from each side's
/// `channel_key`, ignoring `baud_rate` and the FD rate (elements 1 and 3)
/// exactly like the fallback branch already does -- both branches now
/// answer the same rate-insensitive "same wires" question the doc comment
/// above always claimed for both.
///
/// This predicate does have a real gap, but in the OPPOSITE direction from
/// the declined finding above: it is currently too PERMISSIVE (a
/// false-negative gap: two links contending for the same physical wires
/// can slip past a lock that should have caught them), never too strict.
/// Tracked as an ADR-158 accepted residual, not fixed here:
/// (1) an FD-vs-Classic `hw_protocol_id` mismatch on the same pins (`
/// FD_CAN_PS != CAN` in the raw comparison above) lets an FD connect slip
/// past a Classic holder's lock on the same wires; (2) a `None`-vs-
/// synthesized-default `pin_select` mismatch has the same effect for two
/// links that are physically on the same (default) pins but where only one
/// side's `pin_select` was ever populated. If this predicate is ever
/// revisited, the correct fix is normalizing `hw_protocol_id` through
/// `resources::base_protocol_id` and treating default pins as equal --
/// making the lock MORE conservative, the opposite of what the declined
/// Codex finding proposed. A future "complete the pattern" attempt should
/// aim at this real hole, not reopen the declined rate-comparison one.
fn same_physical_resource(
    a: (u32, Option<u32>, Option<ChannelKey>),
    b: (u32, Option<u32>, Option<ChannelKey>),
) -> bool {
    match (a.2, b.2) {
        (Some(a_ck), Some(b_ck)) => a_ck.0 == b_ck.0 && a_ck.2 == b_ck.2,
        _ => a.0 == b.0 && a.1 == b.1,
    }
}

/// Recovers a `ChannelKey`'s `pin_select` component in
/// `LogicalLinkState::pin_select`'s own `Option<u32>` shape -- the exact
/// inverse of `ChannelKey` construction's `pin_select.unwrap_or(0)` (see that
/// type's doc comment): `0` (every non-`_PS` channel) maps back to `None`,
/// any other value maps to `Some`. Sound because `compute_pin_select`
/// (`names.rs`) never produces the `0x00000000` sentinel (ADR-156
/// Corrections), so a real `_PS` channel's stored value is always nonzero --
/// this round-trips without collision. Needed at call sites (e.g.
/// `rpc_lock_resource`'s `LOCK_PHYSICAL_TX_QUEUE` active-transmission scan)
/// that compare a not-yet-connected `LogicalLinkState` against an
/// already-open `SharedChannel`'s raw `ChannelKey`, where no
/// `LogicalLinkState::pin_select` is directly in scope for the connected
/// side.
fn channel_key_pin_select(ck: ChannelKey) -> Option<u32> {
    if ck.2 == 0 { None } else { Some(ck.2) }
}

/// Recomputes `tx_suspended_by_lock` for every CLL from scratch: `true` iff
/// some OTHER CLL on the same physical resource currently holds
/// `LOCK_PHYSICAL_TX_QUEUE` (ISO 22900-2 §9.4.13.3 use case 1 -- a
/// ComLogicalLink created later starts out with its ComPrimitive queue
/// suspended (SUSPEND_TX_QUEUE), and releasing the lock sends RESUME_TX_QUEUE
/// to every ComLogicalLink on the shared physical resource).
/// Recompute-from-scratch (not incremental set/clear at
/// grant/release time) because the pre-connect `hw_protocol_id`-fallback
/// resource-scope comparison can change a CLL's matched resource out from
/// under an incremental update (e.g. the lock holder connects to a real
/// `channel_key` after granting) -- recomputing on every relevant transition
/// self-heals that; incremental updates would leak a stale suspension in
/// that case (ADR-123).
///
/// Returns the handles of every CLL whose `tx_suspended_by_lock` transitioned
/// true -> false, so the caller can send each of them a `TxItem::ResumeWake`
/// to flush `tx_held` (ADR-123).
///
/// ADR-147 (wake-target widening): this is keyed on the `tx_suspended_by_lock`
/// transition alone, NOT on the CLL's *effective* suspension
/// (`tx_suspended()`) transitioning -- even a CLL that remains effectively
/// suspended afterwards (e.g. `tx_suspended_by_error` is also set) is
/// included. This closes a stranding path: a sibling CLL holds the physical
/// lock while this CLL has both a transmitting item and a recovery
/// `TxItem::UpdateParam` parked in `tx_held` under the lock's FIFO clause; an
/// in-flight COP then times out and sets `tx_suspended_by_error` while still
/// locked; when the sibling unlocks, `tx_suspended_by_lock` clears but
/// `tx_suspended()` stays true (error-suspended), so under the old
/// effective-transition contract no wake would fire and the parked
/// `UpdateParam` would have no drain trigger. A wake sent here despite
/// remaining effectively suspended is harmless: `drain_tx_held_backlog` runs
/// its own authoritative gate check on every wake regardless of why it fired,
/// so a spurious wake just costs one no-op drain attempt.
/// One CLL's snapshotted physical-resource identity (`hw_protocol_id`,
/// `pin_select`, `channel_key` -- the same 3 discriminants
/// `same_physical_resource` compares) plus whether it currently holds
/// `LOCK_PHYSICAL_TX_QUEUE`, keyed by handle. Factored into a named type
/// (clippy::type_complexity) for `recompute_lock_tx_suspensions`'s snapshot.
type LockTxSnapshotEntry = (u32, u32, Option<u32>, Option<ChannelKey>, bool);

fn recompute_lock_tx_suspensions(links: &mut HashMap<u32, LogicalLinkState>) -> Vec<u32> {
    // First pass (immutable borrow): snapshot each CLL's resource key and
    // whether it currently holds LOCK_PHYSICAL_TX_QUEUE. `pin_select`
    // (ADR-157 Bug D fix) is carried alongside `hw_protocol_id`/`channel_key`
    // so `same_physical_resource`'s fallback comparison below stays in sync
    // with `find_physical_lock_holder`'s -- see that function's doc comment.
    let snapshot: Vec<LockTxSnapshotEntry> = links
        .iter()
        .map(|(&h, l)| {
            (
                h,
                l.hw_protocol_id,
                l.pin_select,
                l.channel_key,
                l.held_lock_mask & LOCK_PHYSICAL_TX_QUEUE != 0,
            )
        })
        .collect();

    // Second pass (mutable borrow): for each CLL, determine whether any
    // OTHER entry in the snapshot shares its physical resource and holds the
    // lock, then apply and record true -> false transitions.
    let mut wake_targets = Vec::new();
    for &(h, hw_protocol_id, pin_select, channel_key, _) in &snapshot {
        let new_by_lock = snapshot.iter().any(
            |&(other_h, other_hw, other_pin_select, other_ck, other_holds)| {
                other_h != h
                    && other_holds
                    && same_physical_resource(
                        (hw_protocol_id, pin_select, channel_key),
                        (other_hw, other_pin_select, other_ck),
                    )
            },
        );
        if let Some(link) = links.get_mut(&h) {
            let old_by_lock = link.tx_suspended_by_lock;
            link.tx_suspended_by_lock = new_by_lock;
            if old_by_lock && !new_by_lock {
                wake_targets.push(h);
            }
        }
    }
    wake_targets
}

/// Backing store for `J2534Service::terminal_cops` (ADR-128 Codex-review
/// round 1 correction). Pairs the terminal-status entries with a
/// `destroyed_clls` marker set so that recording a terminal status and
/// purging/marking a CLL destroyed can be made atomic with each other under
/// ONE lock acquisition each — closing a real race (reachable on this
/// service's multi-threaded `#[tokio::main]` runtime, not merely a
/// single-threaded-test-harness-infeasible one) where a terminal emitter's
/// `primitives.remove()` succeeds, a concurrent `DestroyComLogicalLink`
/// (or `ModuleDisconnect`'s bulk force-cleanup) then fully completes
/// -- including its purge, which finds nothing yet to remove -- and only
/// THEN does the emitter's own `terminal_cops.insert` land, resurrecting a
/// permanently-unpurgeable entry for an already-destroyed CLL. See
/// `TerminalCopsLedger::record`/`purge`.
#[derive(Default)]
pub(super) struct TerminalCopsLedger {
    entries: HashMap<u32, (u32, vci_service_interface::PduComPrimitiveStatus)>,
    /// `cll_handle`s that have been destroyed (`DestroyComLogicalLink` or
    /// `ModuleDisconnect`'s force-cleanup) and must never again accept a
    /// `record()` -- cleared for a given handle only when that exact numeric
    /// handle is reused by a fresh `CreateComLogicalLink` (see
    /// `TerminalCopsLedger::unmark_destroyed`), mirroring the same
    /// astronomically-rare (`u32`-wraparound-scale) reuse residual this
    /// struct's sibling `entries` map already accepts (ADR-128).
    destroyed_clls: std::collections::HashSet<u32>,
}

impl TerminalCopsLedger {
    /// Records `cop_handle`'s terminal status, UNLESS `cll_handle` is
    /// already marked destroyed -- the atomic counterpart to `purge`/
    /// `purge_many` below, closing the resurrection race this struct's own
    /// doc comment describes. Called only by `events::send_cop_status`.
    pub(super) fn record(
        &mut self,
        cop_handle: u32,
        cll_handle: u32,
        status: vci_service_interface::PduComPrimitiveStatus,
    ) {
        if !self.destroyed_clls.contains(&cll_handle) {
            self.entries.insert(cop_handle, (cll_handle, status));
        }
    }

    pub(super) fn lookup(
        &self,
        cop_handle: u32,
    ) -> Option<(u32, vci_service_interface::PduComPrimitiveStatus)> {
        self.entries.get(&cop_handle).copied()
    }

    /// Removes every entry belonging to `cll_handle` and marks it destroyed,
    /// atomically (one lock acquisition covers both). Used by
    /// `rpc_destroy_com_logical_link` for a single CLL.
    pub(super) fn purge(&mut self, cll_handle: u32) {
        self.entries.retain(|_, (cll, _)| *cll != cll_handle);
        self.destroyed_clls.insert(cll_handle);
    }

    /// Batch form of `purge`, for `rpc_module_disconnect`'s force-cleanup of
    /// every CLL on the module at once.
    pub(super) fn purge_many(&mut self, cll_handles: &[u32]) {
        self.entries
            .retain(|_, (cll, _)| !cll_handles.contains(cll));
        self.destroyed_clls.extend(cll_handles.iter().copied());
    }

    /// Clears a stale `destroyed_clls` marker for a `cll_handle` number
    /// about to be reused by a fresh `CreateComLogicalLink` -- without this,
    /// a reused handle would have every future terminal status for its
    /// BRAND NEW CLL silently dropped by `record`'s guard forever, mistaking
    /// "this numeric handle was destroyed in a past life" for "the CLL
    /// currently holding this handle is destroyed". Correctness-required,
    /// not defensive, on the (extremely rare, `u32`-wraparound-scale) path
    /// where `next_logical_link_handle` reissues a number.
    pub(super) fn unmark_destroyed(&mut self, cll_handle: u32) {
        self.destroyed_clls.remove(&cll_handle);
    }
}

#[derive(Clone)]
pub struct J2534Service {
    api: Arc<Mutex<J2534Api0404>>,
    startup_config: Arc<StartupConfig>,
    /// How CAN-family CLLs are mapped onto J2534 physical channels.
    /// Loaded once at startup from `config.toml` (`can_channel_mode`,
    /// ADR-046); default `SingleChannel`. When this is `Auto`, the actual
    /// `DualChannel`-vs-`SingleChannel` decision is resolved at runtime and
    /// cached in `resolved_can_channel_mode`; use `effective_can_channel_mode()`
    /// rather than comparing this field directly.
    can_channel_mode: CanChannelMode,
    /// `Auto` mode's resolved decision (`DualChannel` or `SingleChannel`),
    /// populated by the first capability probe (`probe_can_channel_mode` in
    /// `rpc_link.rs`). `None` before the first ISO15765-family CLL connects a
    /// new physical channel, or whenever `can_channel_mode` is not `Auto`
    /// (left permanently `None` in that case; unused). See ADR-046 addendum.
    ///
    /// Device-derived: tagged with the `device_epoch` (see that field) the
    /// value was probed under, epoch-tagged self-validating cache (ADR-107
    /// addendum (h), superseding the earlier "reset on `ModuleDisconnect`"
    /// approach). A reader treats the entry as a cache hit only when its
    /// stored epoch equals the current `device_epoch`; a stale-epoch entry is
    /// harmless and just triggers a re-probe on the next read rather than
    /// needing active invalidation. `ModuleDisconnect` no longer touches this
    /// field at all -- the epoch bump on close (and on the next open) is
    /// what makes a stale entry unreadable.
    resolved_can_channel_mode: Arc<Mutex<Option<(u64, CanChannelMode)>>>,
    /// Pre-declared device-selection entries (ADR-107), 1-based position ==
    /// `module_handle`. Loaded once at startup from `config.toml`
    /// (`config.apis.j2534-0404.libs."<lib>".modules`); a single synthetic
    /// entry (`label = "j2534-0404"`, `pname = None`) when the `modules` key
    /// is absent, matching pre-ADR-107 behavior. Never empty (`config::
    /// resolve_modules` rejects `modules = []` at startup) — index `0` is
    /// always a valid `DEFAULT_MODULE_HANDLE` target.
    modules: Arc<Vec<crate::config::ModuleEntry>>,
    /// Per-`cmd_id` vendor IOCTL native contracts (ADR-219, as amended).
    /// Loaded once at startup from `config.toml`
    /// (`config.apis.j2534-0404.libs."<lib>".vendor_ioctls`) and fail-fast
    /// validated (`crate::config::resolve_vendor_ioctls`), mirroring
    /// `modules`/`can_channel_mode`'s own "typo surfaces at startup"
    /// convention -- never re-read from disk at dispatch time.
    /// `rpc_misc.rs::rpc_io_ctl_vendor` looks a `cmd_id` up here before
    /// taking any lock: a `cmd_id` absent from this map, carrying a
    /// non-NULL buffer, is rejected `FAILED_PRECONDITION`.
    vendor_ioctls: Arc<HashMap<u32, crate::config::VendorIoctlContract>>,
    /// The single currently-open device, if any, tagged with the
    /// `module_handle` it was opened for. This service opens at most one
    /// J2534 device at a time (ADR-107 Decision (b), not simultaneous
    /// multi-device) — connecting a different configured module requires an
    /// explicit `ModuleDisconnect` first (see `ensure_open_device_for`).
    /// `device_id` is the outermost lock among the nested set (`logical_links`,
    /// `shared_channels`, `primitives`, `subscriptions`, `module_state`,
    /// `api`) — it must never be acquired while holding any of those. Two
    /// device-derived probe caches, `j1850_bus_flavor` and
    /// `resolved_can_channel_mode`, sit ABOVE `device_id` instead (held
    /// across a transient `device_id` acquisition elsewhere) — `device_id`
    /// holders must never acquire either of them (ADR-107 addendum). The
    /// non-selecting `ensure_open_device` acquires `logical_links` (a nested
    /// lock) while holding this lock to re-validate its `cll_handle` still
    /// exists before returning or opening anything, closing a race where a
    /// probe blocked on this lock resumes after a concurrent
    /// `ModuleDisconnect` already released it and cleared `logical_links`
    /// (ADR-107 addendum). `device_epoch` is bumped while this lock is held,
    /// on every successful open and on every close (ADR-107 addendum (h)).
    device_id: Arc<Mutex<Option<(u32, DeviceId)>>>,
    /// Monotonically increasing tag bumped on every successful device OPEN
    /// (`ensure_open_device_inner`) and every device CLOSE, attempted or
    /// successful (`rpc_module_disconnect`) -- mutated only while holding
    /// `device_id`'s lock; read lock-free. Backs the epoch-tagged
    /// self-validation scheme for the two device-derived probe caches
    /// (`j1850_bus_flavor`, `resolved_can_channel_mode`): a cached value is
    /// only used if its stored epoch equals the current value here,
    /// otherwise it is treated as a miss and re-probed (ADR-107 addendum
    /// (h)). Replaces the earlier "clear both caches inside
    /// `ModuleDisconnect`" approach, which could not be ordered safely
    /// against the established lock hierarchy (see that addendum for why).
    device_epoch: Arc<AtomicU64>,
    /// SAE J2534-2 clause 19.3.2.3 TP2.0 broadcast periodic in-flight-start
    /// race fix (ADR-192/Phase 7 Stage 7c, design-advisor consult, Codex
    /// review PR #101 round 5): a global counter bumped by exactly one
    /// while still holding `self.api`, immediately after a successful
    /// `CLEAR_PERIODIC_MSGS` native `clear_periodic_messages` call returns
    /// (`rpc_misc.rs`). Never mutated anywhere else. Read (also while still
    /// holding `self.api`, `Ordering::Relaxed` -- the shared `api` mutex
    /// itself is what provides the happens-before ordering, not the atomic
    /// operation) immediately after a successful native
    /// `start_periodic_message` call returns
    /// (`rpc_primitive.rs::rpc_start_com_primitive`'s broadcast-periodic
    /// branch) and stashed as `Tp20BroadcastPeriodic::started_epoch` on the
    /// committed entry. Because both the native clear and the native start
    /// serialize on the same `self.api` lock, comparing a committed entry's
    /// `started_epoch` against a LATER clear's own post-increment value
    /// (`clear_generation`) tells `CLEAR_PERIODIC_MSGS`'s reconciliation
    /// scan whether that entry's native start definitely preceded this
    /// clear's native call (`started_epoch < clear_generation`, so the
    /// device-side message is already gone and the entry must be
    /// finalized) or raced/followed it (`started_epoch >= clear_generation`,
    /// so the message is still live and must be left tracked) -- regardless
    /// of how the two operations' separate `logical_links`/
    /// `shared_channels` reconciliation steps happen to interleave
    /// afterward. Not epoch-tagged against `device_epoch`: unlike that
    /// field's device-derived probe caches, this counter's entries are
    /// reconciled synchronously within the same `CLEAR_PERIODIC_MSGS` call
    /// that reads it, never cached across a device close/reopen.
    periodic_clear_epoch: Arc<AtomicU64>,
    /// **Lock order invariant (ADR-115), independent of the
    /// `device_id`-outermost hierarchy documented on that field**:
    /// `logical_links` -> `subscriptions` -> a per-CLL queue lock
    /// (`LogicalLinkState::rx_buf`, i.e. a `CllEventQueue`'s own `Mutex`).
    /// Any site may skip levels (e.g. lock `subscriptions` alone, or
    /// `subscriptions` then a queue with no `logical_links` involved at
    /// all); never reverse an order two of these locks are held in
    /// together. Load-bearing for `rpc_subscribe_event`/
    /// `rpc_create_com_logical_link`'s atomic stamp-under-`subscriptions`
    /// critical sections (`rpc_primitive.rs`, `rpc_link.rs`) and for
    /// `terminate_subscription`/`terminate_all_subscriptions` (below):
    /// reversing this order at any of those sites (locking `logical_links`
    /// or `subscriptions` while already holding a queue lock) would deadlock
    /// against a concurrent producer.
    ///
    /// **`deliver_or_enqueue` (`events.rs`) is the one direction that must
    /// never be reversed**: it holds a `CllEventQueue`'s own lock for its
    /// whole push-then-drain duration and must NEVER acquire `subscriptions`
    /// while holding it (ADR-115 round 6) -- `CllEventQueue::live_sender` is
    /// everything it needs, read fresh under the lock it already holds, so
    /// it has no reason to touch `subscriptions` at all. `build_cll_rx_entries`
    /// (`events.rs`) no longer locks `subscriptions` either, for the same
    /// reason (round 6): it used to clone a per-CLL subscriber ref ahead of
    /// time for `deliver_or_enqueue` to consult later, which is exactly the
    /// capture-then-possibly-stale pattern round 6 eliminated.
    ///
    /// **One additional one-way edge, `primitives` -> `subscriptions`
    /// (ADR-118, post-commit disconnect-race fixes; generalized crate-wide
    /// by ADR-128 round 2):** originally two call sites in `events.rs` held
    /// `primitives` across a `subscriptions` acquisition (inside
    /// `send_cop_status`), to make a `contains_key`-check-then-emit atomic
    /// with every CANCELLED-emitting path: `dispatch_tx_item`'s tail
    /// (WAITING between cycles) and `handle_send_recv`'s cycle-start gate
    /// (EXECUTING at the top of every cycle). ADR-128's round-2 correction
    /// extended this to every terminal-status emission in the crate (~40+
    /// sites, most funneled through the shared `emit_terminal_if_live`
    /// helper): a terminal emission's `primitives` removal and its
    /// `send_cop_status` call — which itself acquires `terminal_cops` then
    /// `subscriptions` — now always share ONE continuous `primitives`
    /// critical section, so a concurrent `CancelComPrimitive`/`GetStatus`
    /// can never observe a cop_handle as missing from both `primitives` and
    /// `terminal_cops` at once. `primitives` sits outside this hierarchy
    /// entirely, so every one of these is an instance of the same acyclic
    /// edge — safe as long as no site ever acquires `primitives` while
    /// already holding `subscriptions`, `terminal_cops`, or a queue lock
    /// (still true crate-wide; see `terminal_cops`'s own doc comment below
    /// for the `primitives -> terminal_cops -> subscriptions` chain).
    ///
    /// **Third call site, `logical_links` -> `primitives` -> `subscriptions`
    /// (ADR-118, round 6 explicit-cancel-vs-WAITING fix):**
    /// `dispatch_tx_item`'s WAITING gate now also acquires `logical_links`
    /// first (to drain `cancelled_cops` atomically with the `primitives`
    /// check, so an explicit cancel wins over a stale WAITING), extending
    /// that site's edge from `primitives -> subscriptions` to
    /// `logical_links -> primitives -> subscriptions`. Consistent with both
    /// documented hierarchies above: no other site in this crate acquires
    /// `logical_links` while already holding `primitives`, and no site holds
    /// `subscriptions` while acquiring either — acyclic, no deadlock risk.
    ///
    /// **`primitives` -> a per-CLL queue lock (`LogicalLinkState::rx_buf`,
    /// i.e. a `CllEventQueue`'s own `Mutex`), added when `send_cop_status`
    /// was wired into the same `GetEventItem` queue `send_cll_status`
    /// already used:** every `send_cop_status` call site resolves its
    /// `CllQueueTarget` from `logical_links`
    /// (`events::resolve_queue_target`/`CllQueueTarget::from_link`) *before*
    /// `primitives` is ever acquired, then holds the resolved
    /// `Arc<Mutex<CllEventQueue>>` (not `logical_links` itself) across the
    /// `primitives`-guarded `send_cop_status` call -- so the queue lock ends
    /// up nested *inside* `primitives` at every call site that already holds
    /// `primitives` across the emission (`emit_terminal_if_live` and its
    /// hand-written equivalents), giving the edge `primitives -> queue`. At
    /// `dispatch_tx_item`'s WAITING/CANCELLED tail specifically, which also
    /// holds `logical_links` across the same call (the third call site
    /// above), this extends to `logical_links -> primitives -> queue`. The
    /// queue lock remains a leaf in every case: `deliver_or_enqueue` never
    /// acquires any other lock while holding it (see `CllEventQueue`'s own
    /// doc comment), so this is a pure extension of the existing acyclic
    /// graph, not a new cycle.
    ///
    /// **`logical_links` -> queue, direct (skipping `primitives`),
    /// `ioctl_set_event_queue_properties` (`rpc_misc.rs`; second Codex review
    /// round on PR #3, ADR-140 follow-up):** this IOCTL's `pdu_connect_begun()`
    /// gate check and its `CllEventQueue` policy write + cap-trim loop share
    /// ONE `logical_links` critical section, with the queue lock acquired
    /// nested inside it (`link.rx_buf.lock().await`) rather than the gate
    /// being checked under `logical_links` and then re-locked separately
    /// under the queue -- that two-lock split had reopened a window for a
    /// concurrent `ConnectComLogicalLink` to claim `connect_in_flight`
    /// between the two acquisitions, letting the IOCTL's write land after
    /// PDUConnect had effectively begun (ADR-126). This is a new edge, but
    /// not a new risk: it is consistent with both hierarchies documented
    /// above, since the queue lock remains a leaf that never acquires
    /// anything else while held, and no site acquires `logical_links` while
    /// already holding a queue lock.
    logical_links: Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    /// Per-physical-channel drain watermark (ADR-101 Decision §E): the
    /// `read_started_at` instant of the most recent EXHAUSTIVE `poll_rx_inner`
    /// pass on that channel, stamped only after that pass's own
    /// writeback/merge has landed. Constructed once here, the SAME instance
    /// `Arc::clone`d into every `ChannelPollCtx` (mirroring `logical_links`,
    /// NOT the `last_bus_activity` per-physical-channel pattern) so that a
    /// CLL's primary poll task can read its UUDT companion channel's own
    /// watermark when deciding whether a cyclic registrant is soundly
    /// reapable. See `reap_expired_cyclic_registrants`/`is_cyclic_reap_sound`.
    drain_watermarks: Arc<Mutex<HashMap<ChannelId, tokio::time::Instant>>>,
    /// Tracks physical J2534 channels shared by CLLs with the same
    /// `(protocol_id, baud_rate)`.  Ref-counted; channel is closed when the
    /// last CLL disconnects.
    shared_channels: Arc<Mutex<HashMap<ChannelKey, SharedChannel>>>,
    /// Maps active cop_handle → CopEntry.  Entries are added in start_com_primitive
    /// and removed either by cancel_com_primitive (cancellation path) or by the poll
    /// task when the COP completes normally.
    primitives: Arc<Mutex<HashMap<u32, CopEntry>>>,
    /// Terminal-status ledger for A2-23 (ISO 22900-2:2009(E) §9.2.6.7 /
    /// §9.4.18.2 d), ADR-128): `cop_handle -> (cll_handle, terminal_status)`
    /// for every COP that has reached `PduCopstFinished`/`PduCopstCancelled`,
    /// populated centrally by `events::send_cop_status` whenever it emits a
    /// terminal status (so every emission site is covered without per-site
    /// logic) and consulted by `rpc_cancel_com_primitive`/`rpc_get_status` as
    /// a fallback once the entry has left `primitives`. This models COP
    /// destruction as happening at CLL-destroy time (not at terminal-emission
    /// time, which is `primitives`' own removal point) — the closest
    /// spec-compatible approximation available, since neither delivery path
    /// (`SubscribeEvent` push, `GetStatus` pull) gives this service a real
    /// "the client read the terminal status" signal to destroy on exactly as
    /// §9.2.6.7 describes. Entries are purged per-CLL in
    /// `rpc_destroy_com_logical_link` (after `cancel_link_cops` runs) and in
    /// bulk by `rpc_module_disconnect`'s force-cleanup, both via
    /// `TerminalCopsLedger::purge`/`purge_many` — see that type's doc
    /// comment for why recording and purging must be atomic with each other
    /// (ADR-128 Codex-review round 1).
    /// **Strict leaf mutex: never acquire another lock while holding this
    /// one.** The reverse is fine and, since ADR-128's round-2 correction,
    /// is the EXPECTED case for almost every write: nearly every terminal
    /// emission holds `primitives` continuously across its removal AND the
    /// `send_cop_status` call that acquires `terminal_cops` (the
    /// `primitives -> terminal_cops -> subscriptions` chain, load-bearing
    /// for closing the reader-vs-writer race `emit_terminal_if_live`'s own
    /// doc comment describes), not an occasional exception. Callers may
    /// also already hold `shared_channels` (`rpc_destroy_com_logical_link`
    /// holds it across both `cancel_link_cops` and this map's own purge),
    /// or both `logical_links` and `subscriptions` together
    /// (`rpc_create_com_logical_link`'s handle-reuse clear) when acquiring
    /// `terminal_cops` — no cycle results, since nothing is ever acquired
    /// *while* holding this lock: every access is a single method call on
    /// the guard, released immediately after. See ADR-128.
    terminal_cops: Arc<Mutex<TerminalCopsLedger>>,
    next_cll_handle: Arc<Mutex<u32>>,
    next_cop_handle: Arc<Mutex<u32>>,
    /// Monotonic counter stamped into `LogicalLinkState::connect_generation`
    /// every time `ConnectComLogicalLink` finalizes a connection for a
    /// `cll_handle` (including a reconnect of the same handle onto the same
    /// physical channel). Lets a call-time-captured generation (in
    /// `TxItem::StartComm`/`TxItem::StopComm`) be compared against the live
    /// value to detect a disconnect-then-reconnect since a COP was accepted,
    /// even when `channel_id` happens to match again on a shared channel
    /// (ADR-086).
    next_connect_generation: Arc<Mutex<u64>>,
    /// Monotonic counter stamped into `SharedChannel::occupancy_epoch` at
    /// channel creation and re-stamped on every `ref_count` increment
    /// (primary-connect join and UUDT-companion join alike), always under
    /// `shared_channels`. Lets `ioctl_reset`'s Phase 1 detect a sibling CLL
    /// joining an already-open shared channel between its snapshot and its
    /// per-channel teardown -- a hazard a `connect_generation` scan alone
    /// cannot see, since the UUDT-companion join never changes that field
    /// (ADR-161).
    next_occupancy_epoch: Arc<Mutex<u64>>,
    /// See `logical_links`'s own doc comment for the `logical_links` ->
    /// `subscriptions` -> per-CLL queue lock order this field participates
    /// in (ADR-115).
    pub(super) subscriptions: Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    pub(super) shutdown: Receiver<bool>,
    /// Persistent module-level status and last-error.  Updated on hard channel
    /// errors and reset when a new physical channel opens.  Read by
    /// `GetStatus(module)`, `GetEventItem(module)`, and surfaced as
    /// `ErrorDetail.error_event_data` on a failing module-scoped RPC
    /// (ADR-105's rich error model, which replaced polling via
    /// `GetLastError`). Also reset at fresh device-open time
    /// (`ensure_open_device_inner`, nested under `device_id` at its
    /// established level, ADR-107 addendum) -- NOT at `ModuleDisconnect` --
    /// so `last_error` stays queryable in the window between disconnect and
    /// the next reconnect, while still starting clean for whichever module
    /// opens next.
    pub(super) module_state: Arc<Mutex<ModuleState>>,
    /// Ring buffer of module-level events, drained by `GetEventItem(module_handle)`.
    /// Populated by `send_module_status` alongside the streaming subscription path.
    pub(super) module_event_buf: Arc<Mutex<VecDeque<vci_service_interface::EventItem>>>,
    /// Ring buffer of system-level events, drained by `GetEventItem(system_handle)`.
    /// Populated by `send_system_info` alongside the streaming subscription path.
    pub(super) system_event_buf: Arc<Mutex<VecDeque<vci_service_interface::EventItem>>>,
    /// Cached result of the `SAE_J1850` VPW/PWM auto-detect probe (ADR-070):
    /// the winning native J2534 protocol ID (`j2534_0404::J1850VPW` or
    /// `J1850PWM`), or `None` before the first `SAE_J1850` bus-agnostic CLL
    /// (`ChannelProtocol::needs_j1850_autodetect`) connects. Populated once
    /// per module and consulted by every later such CLL so only the first
    /// one pays the probe latency.
    ///
    /// Device-derived: tagged with the `device_epoch` the value was probed
    /// under, epoch-tagged self-validating cache (ADR-107 addendum (h),
    /// superseding the earlier "reset on `ModuleDisconnect`" approach). A
    /// reader treats the entry as a cache hit only when its stored epoch
    /// equals the current `device_epoch`; a stale-epoch entry is harmless
    /// and just triggers a re-probe on the next read rather than needing
    /// active invalidation. `ModuleDisconnect` no longer touches this field
    /// at all -- the epoch bump on close (and on the next open) is what
    /// makes a stale entry unreadable.
    pub(super) j1850_bus_flavor: Arc<Mutex<Option<(u64, u32)>>>,
    /// Mirrors the last successful `PDU_IOCTL_SET_PROG_VOLTAGE` value per pin
    /// (`PinOnDLC` -> millivolts). Write-only from the adapter's perspective
    /// (the J2534 API has no "get programming voltage setting" call); kept so
    /// a future read-back path has somewhere to look.
    pub(super) prog_voltage: Arc<Mutex<HashMap<u32, u32>>>,
    /// Lazily-populated, epoch-tagged cache of `GET_DEVICE_INFO` results
    /// (ADR-153), keyed by `DiscoveryDeviceKey` (`module_handle`,
    /// `DEVICE_INFO_*` parameter ID) -- `module_handle` is part of the key,
    /// not just the epoch, per that type's doc comment (Codex review on
    /// PR #25: an epoch-only key let one module's cached answer be served
    /// to a query for a different module). Mirrors `resolved_can_channel_mode`/
    /// `j1850_bus_flavor`'s device-derived cache pattern (ADR-107 addendum
    /// (h)): a `(device_epoch, DiscoveryResult)` entry is used only when its
    /// stored epoch matches the current `device_epoch`; a stale-epoch entry
    /// is silently treated as a miss and re-queried rather than needing
    /// active invalidation. Unlike those two fields, never held while
    /// acquiring `api` or any other lock -- a lookup releases this lock
    /// before acquiring `api` to issue a fresh native query, then
    /// re-acquires it to insert (`service/discovery.rs`).
    pub(super) discovery_device_info: Arc<Mutex<HashMap<DiscoveryDeviceKey, DiscoveryCacheEntry>>>,
    /// Same as `discovery_device_info`, but for `GET_PROTOCOL_INFO`, keyed
    /// by `DiscoveryProtocolKey` (`module_handle`, `protocol_id`, parameter)
    /// (ADR-153).
    pub(super) discovery_protocol_info:
        Arc<Mutex<HashMap<DiscoveryProtocolKey, DiscoveryCacheEntry>>>,
}

impl VciServer for J2534Service {
    type StartupConfig = StartupConfig;

    fn get_startup_config(
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self::StartupConfig, BoxError> {
        Ok(crate::config::parse_startup_config(args)?)
    }

    fn service_identity(
        config: &Self::StartupConfig,
    ) -> vci_service_launcher::vci_server::ServiceIdentity {
        vci_service_launcher::vci_server::ServiceIdentity {
            api_name: "j2534-0404",
            library_name: config.library_name.clone(),
            arch: vci_service_launcher::vci_server::current_arch(),
        }
    }

    async fn new(
        startup_config: Self::StartupConfig,
        shutdown: Receiver<bool>,
    ) -> Result<Self, BoxError> {
        // Captures the shared module clock's epoch as close to service
        // start as this constructor allows -- ISO 22900-2 §9.1.6.1's
        // boot-relative time base (ADR-120, Codex review amendment on
        // PR #129). Deliberately first in this function body, ahead of any
        // fallible setup below, so a startup failure never leaves the clock
        // uninitialized-and-then-lazily-started at some arbitrary later
        // first-use moment.
        events::init_module_clock();
        // A `library_path` configured for this library name takes priority over
        // platform auto-discovery (the Windows registry), letting deployers add
        // libraries manually and letting non-Windows builds resolve one at all.
        let library_path = j2534_0404_registry::resolve_library_path(
            vci_service_launcher::vci_server::current_arch(),
            &startup_config.library_name,
        )
        .map_err(map_registry_error)?;
        // Fail fast on an invalid `can_channel_mode` config value so a typo
        // surfaces at startup rather than as silent single-channel behaviour.
        let can_channel_mode = CanChannelMode::from_config(
            vci_service_launcher::vci_server::current_arch(),
            &startup_config.library_name,
        )
        .map_err(|msg| -> BoxError { msg.into() })?;
        // Fail fast on an invalid `modules` config value (empty array,
        // non-ASCII/embedded-NUL pname) too (ADR-107).
        let modules = Arc::new(crate::config::resolve_modules(
            vci_service_launcher::vci_server::current_arch(),
            &startup_config.library_name,
        )?);
        // Fail fast on an invalid vendor_ioctls config value (bad key
        // format, unrecognized shape) too (ADR-219, as amended).
        let vendor_ioctls = Arc::new(crate::config::resolve_vendor_ioctls(
            vci_service_launcher::vci_server::current_arch(),
            &startup_config.library_name,
        )?);
        let api = Arc::new(Mutex::new(
            J2534Api0404::from_path(&library_path).map_err(map_construct_error)?,
        ));
        let logical_links: Arc<Mutex<HashMap<u32, LogicalLinkState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let drain_watermarks: Arc<Mutex<HashMap<ChannelId, tokio::time::Instant>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let device_id: Arc<Mutex<Option<(u32, DeviceId)>>> = Arc::new(Mutex::new(None));
        let subscriptions: Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let shared_channels: Arc<Mutex<HashMap<ChannelKey, SharedChannel>>> =
            Arc::new(Mutex::new(HashMap::new()));

        spawn_shutdown_task(
            shutdown.clone(),
            Arc::clone(&subscriptions),
            Arc::clone(&logical_links),
            Arc::clone(&shared_channels),
            Arc::clone(&device_id),
            Arc::clone(&api),
        );

        Ok(Self {
            api,
            startup_config: Arc::new(startup_config),
            can_channel_mode,
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules,
            vendor_ioctls,
            device_id,
            device_epoch: Arc::new(AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(AtomicU64::new(0)),
            logical_links,
            drain_watermarks,
            shared_channels,
            primitives: Arc::new(Mutex::new(HashMap::new())),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(0)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions,
            shutdown,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn get_requested_port(&self) -> Option<u16> {
        self.startup_config.requested_port
    }
}

fn spawn_shutdown_task(
    mut shutdown: Receiver<bool>,
    subscriptions: Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    logical_links: Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    shared_channels: Arc<Mutex<HashMap<ChannelKey, SharedChannel>>>,
    device_id: Arc<Mutex<Option<(u32, DeviceId)>>>,
    api: Arc<Mutex<J2534Api0404>>,
) {
    tokio::spawn(async move {
        let _ = shutdown.changed().await;

        // Dropping all subscription senders closes every active event stream.
        let _ = std::mem::take(&mut *subscriptions.lock().await);

        // Drop logical links (releases no senders now — they live in shared_channels).
        let _ = std::mem::take(&mut *logical_links.lock().await);

        // Dropping shared_channels drops all _poll_cancel senders (stops poll tasks)
        // and all tx_queue senders.  Then disconnect each channel and close the device.
        let channels: HashMap<_, _> = std::mem::take(&mut *shared_channels.lock().await);
        let device = device_id.lock().await.take();
        if let Some((_, id)) = device {
            let api = api.lock().await;
            for (_, sc) in channels {
                let _ = api.disconnect(sc.channel_id);
            }
            let _ = api.close(id);
        }
    });
}

impl J2534Service {
    /// Rejects any `module_handle` outside the configured range
    /// `1..=module_count` -- the 1-based position of a configured `modules`
    /// entry, or the single synthetic entry when `modules` is absent
    /// (ADR-107). `module_count` is normally `self.modules.len()`; taken as
    /// an explicit parameter (rather than reading `self` directly) so this
    /// stays unit-testable without constructing a full `J2534Service`.
    /// `PDU_HANDLE_UNDEF` is not special-cased here -- individual call sites
    /// that accept it as a "the current module" shorthand (e.g.
    /// `rpc_get_resource_ids`) check for it themselves before/instead of
    /// calling this.
    fn require_module_handle(
        module_handle: Option<vci_service_interface::ModuleHandle>,
        module_count: usize,
    ) -> Result<(), Status> {
        let handle = module_handle
            .ok_or_else(|| Status::invalid_argument("module_handle is required"))?
            .module_handle;
        if handle == 0 || handle as usize > module_count {
            return Err(state_guard_status(
                Code::InvalidArgument,
                format!(
                    "unsupported module_handle {handle}; expected a value in 1..={module_count}"
                ),
                PduError::PduErrInvalidHandle,
                None,
            ));
        }
        Ok(())
    }

    /// Resolves `can_channel_mode` to a concrete `DualChannel` /
    /// `SingleChannel` / `SoftwareIsoTp` decision, reading the `Auto`
    /// capability-probe cache when needed (ADR-046 addendum).
    ///
    /// Defaults `Auto` to `SingleChannel` when no probe has completed yet, OR
    /// when the cached resolution's epoch no longer matches the current
    /// `device_epoch` (ADR-107 addendum (h)) -- a stale-epoch entry belongs
    /// to a device that has since closed (and possibly reopened as a
    /// different module) and must not be used. This only matters for a
    /// caller that checks the effective mode before any ISO15765-family CLL
    /// has connected a new physical channel (e.g. a not-yet-connected CLL's
    /// `SetUniqueRespIdTable`), and being conservative there is safe:
    /// `probe_can_channel_mode` always runs before any code path that would
    /// actually need `DualChannel` behaviour to matter (see
    /// `rpc_connect_com_logical_link`).
    ///
    /// Must never be called while holding `api` or `device_id` -- doing so
    /// risks an AB-BA deadlock against `probe_can_channel_mode`, which holds
    /// this cache's lock while awaiting `api` (Fix for the deadlock found on
    /// PR #110's holistic audit; see `install_point_to_point_fc_filters` for
    /// the fixed call site).
    pub(super) async fn effective_can_channel_mode(&self) -> CanChannelMode {
        match self.can_channel_mode {
            CanChannelMode::Auto => self
                .resolved_can_channel_mode
                .lock()
                .await
                .and_then(|(epoch, mode)| {
                    (epoch == self.device_epoch.load(Ordering::SeqCst)).then_some(mode)
                })
                .unwrap_or(CanChannelMode::SingleChannel),
            other => other,
        }
    }

    /// Ensures a device is open, without selecting which configured module —
    /// for the small set of CLL-scoped internal helpers
    /// (`rpc_connect_com_logical_link`, `ensure_uudt_companion_channel`,
    /// `probe_sae_j1850_flavor`) that operate on "whatever device is already
    /// open" and have no `module_handle` of their own to select with: they
    /// take only a `cll_handle`, whose owning module was already pinned down
    /// at `CreateComLogicalLink` time via `ensure_open_device_for`. If
    /// nothing is open yet, opens the default module
    /// (`DEFAULT_MODULE_HANDLE`, always a valid index — `self.modules` is
    /// never empty). Never rejects on a module-handle mismatch.
    ///
    /// Re-validates that `cll_handle` still exists in `logical_links` while
    /// holding `device_id`, atomically, before returning or opening anything
    /// (`ensure_open_device_inner`'s `cll_liveness` check). This closes a
    /// race where a probe blocked waiting for `device_id` resumes only after
    /// a concurrent `ModuleDisconnect` has already cleared `logical_links`
    /// and released `device_id`: without this check the stale probe could
    /// reopen a device (or reuse one just opened for a different module) for
    /// a CLL that no longer exists (ADR-107 addendum, Codex review on PR
    /// #110).
    ///
    /// Every RPC that itself receives and validates a `module_handle`
    /// (`GetVersion`, `CreateComLogicalLink`, and `ModuleConnect`) must use
    /// [`Self::ensure_open_device_for`] instead — the strict,
    /// module-selecting variant — so that a request for a specific module
    /// can't silently be served by whatever device happens to already be
    /// open (ADR-107 follow-up fix; see "Accepted Residual #3"
    /// in ADR-107).
    ///
    /// Returns the held [`tokio::sync::MutexGuard`] alongside the
    /// `DeviceId`, not just the id: a caller that makes its own follow-up
    /// native `self.api` call using the returned `DeviceId` MUST hold the
    /// returned guard across that call — mirroring
    /// [`Self::lock_device_for`]'s "hold for the entire guarded operation"
    /// rule below. Releasing the guard before the follow-up call reopens the
    /// exact TOCTOU window this function exists to close: a concurrent
    /// `ModuleDisconnect`+reconnect could close/reopen the device in that
    /// gap, so the caller would either fail on a stale `DeviceId` or (if the
    /// native library reuses `DeviceId` values) silently act on the wrong
    /// module's device (ADR-107 addendum (i), Codex review on PR #110).
    async fn ensure_open_device(
        &self,
        cll_handle: u32,
    ) -> Result<
        (
            tokio::sync::MutexGuard<'_, Option<(u32, DeviceId)>>,
            DeviceId,
        ),
        Status,
    > {
        self.ensure_open_device_inner(DEFAULT_MODULE_HANDLE, false, Some(cll_handle))
            .await
    }

    /// Ensures a device is open for `module_handle` specifically (ADR-107).
    ///
    /// - No device open yet: opens `module_handle`'s configured entry
    ///   (`PassThruOpen` with that entry's `pname.as_deref()`, `NULL` for the
    ///   synthetic default entry).
    /// - A device is already open under `module_handle`: no-ops (idempotent
    ///   reconnect), matching pre-ADR-107 `ModuleConnect` behavior. This
    ///   function itself never inspects `module_state` -- `rpc_module_connect`
    ///   (`rpc_module.rs`) additionally rejects with `PDU_ERR_FCT_FAILED` on
    ///   this no-op path when `module_state.status != PduModstReady` (ADR-131:
    ///   `PDU_MODST_NOT_AVAIL` is sticky until `ModuleDisconnect`, since this
    ///   no-op performs no native call that could actually revalidate the
    ///   device).
    /// - A device is already open under a DIFFERENT handle: rejects with
    ///   `PDU_ERR_RESOURCE_BUSY` and does NOT call `PassThruOpen` — this
    ///   service opens at most one J2534 device at a time (single-open-device
    ///   model, ADR-107 Decision (b)); switching requires an explicit
    ///   `ModuleDisconnect` first.
    ///
    /// `module_handle` must already be validated in range (every call site
    /// runs `require_module_handle` first).
    ///
    /// Returns the held [`tokio::sync::MutexGuard`] alongside the
    /// `DeviceId`, not just the id: a caller that makes its own follow-up
    /// native `self.api` call using the returned `DeviceId` MUST hold the
    /// returned guard across that call — see [`Self::ensure_open_device`]'s
    /// doc comment for why (ADR-107 addendum (i), Codex review on PR #110).
    async fn ensure_open_device_for(
        &self,
        module_handle: u32,
    ) -> Result<
        (
            tokio::sync::MutexGuard<'_, Option<(u32, DeviceId)>>,
            DeviceId,
        ),
        Status,
    > {
        self.ensure_open_device_inner(module_handle, true, None)
            .await
    }

    /// `cll_liveness`, when `Some(cll_handle)`, re-validates that
    /// `cll_handle` still exists in `logical_links` immediately after
    /// acquiring `device_id` and before doing anything else — covering both
    /// the "already open, return it" branch below and the "nothing open,
    /// open a new one" branch. `rpc_module_disconnect` clears
    /// `logical_links` while still holding `device_id` (dropped only after,
    /// see `rpc_module.rs`), so this check, run atomically with the lookup
    /// it guards, deterministically rejects a CLL-scoped probe that was
    /// blocked on `device_id` and resumes only after the disconnect it
    /// raced has already completed (ADR-107 addendum). Only
    /// [`Self::ensure_open_device`] (the non-selecting variant) passes
    /// `Some`; [`Self::ensure_open_device_for`] is already strict about
    /// module identity via `reject_on_mismatch` and passes `None`.
    ///
    /// Returns the held [`tokio::sync::MutexGuard`] alongside the
    /// `DeviceId` rather than dropping it: a caller that only peeked the id
    /// and released the guard left a window between this check and its own
    /// follow-up native `self.api` call in which a concurrent
    /// `ModuleDisconnect`+reconnect could close/reopen the device, defeating
    /// the guard (TOCTOU race, Codex review on PR #110). The caller must
    /// hold the returned guard for the entire guarded operation — same rule
    /// as [`Self::lock_device_for`] below.
    async fn ensure_open_device_inner(
        &self,
        requested: u32,
        reject_on_mismatch: bool,
        cll_liveness: Option<u32>,
    ) -> Result<
        (
            tokio::sync::MutexGuard<'_, Option<(u32, DeviceId)>>,
            DeviceId,
        ),
        Status,
    > {
        let mut slot = self.device_id.lock().await;
        if let Some(h) = cll_liveness
            && !self.logical_links.lock().await.contains_key(&h)
        {
            return Err(unknown_handle_status(format!("unknown cll_handle {h}")));
        }
        if let Some((open_handle, id)) = *slot {
            if !reject_on_mismatch || open_handle == requested {
                return Ok((slot, id));
            }
            return Err(Self::open_under_different_handle_status(
                open_handle,
                requested,
            ));
        }

        // `requested` is a validated 1-based position into `self.modules`
        // (require_module_handle already ran at every call site).
        let entry = &self.modules[(requested - 1) as usize];
        let api = self.api.lock().await;
        let last_error = self.module_state.lock().await.last_error.clone();
        let id = api
            .open(entry.pname.as_deref())
            .map_err(|err| map_native_error_for_link("PassThruOpen", &err, Some(last_error)))?;
        *slot = Some((requested, id));
        // Every successful device OPEN bumps the epoch (ADR-107 addendum
        // (h)) -- mutated while `device_id` is held, per `device_epoch`'s
        // doc comment.
        self.device_epoch.fetch_add(1, Ordering::SeqCst);
        *self.module_state.lock().await = ModuleState::default();
        // ADR-132 (Codex review, PR #145): this is the single choke point
        // for every none-to-open transition -- `ModuleConnect`, but also the
        // lazy-open paths (`GetVersion`, `CreateComLogicalLink`) that never
        // themselves call `ModuleConnect`. Since ADR-132, `GetModuleIds`
        // reports `PDU_MODST_AVAIL` for an unopened row and the real
        // `module_state.status` (here, freshly `PduModstReady`) once open --
        // a subscriber that relies on `PDU_INFO_MODULE_LIST_CHG` to know
        // when to re-poll `GetModuleIds` needs this event regardless of
        // which RPC triggered the open, not only an explicit `ModuleConnect`.
        // Safe to await while still holding `slot` (the `device_id` guard):
        // `send_system_info` only locks `system_event_buf`/`subscriptions`,
        // both inner to `device_id` in this service's lock order (ADR-107
        // addendum) -- `rpc_module_connect` already awaited this same call
        // with its own `device_id` guard held, before this change moved the
        // call here.
        events::send_system_info(
            &self.subscriptions,
            &self.system_event_buf,
            vci_service_interface::PduInfo::ModuleListChg,
        )
        .await;
        Ok((slot, id))
    }

    /// Builds the `PDU_ERR_RESOURCE_BUSY`/`FailedPrecondition` rejection
    /// shared by every module-scoped call site that must not silently act on
    /// (or reopen/close) a device other than the one it was asked about
    /// (ADR-107). `open_handle` is whatever `module_handle` is currently open
    /// in `self.device_id`; `requested` is the handle the caller asked for.
    fn open_under_different_handle_status(open_handle: u32, requested: u32) -> Status {
        state_guard_status(
            Code::FailedPrecondition,
            format!(
                "PDU_ERR_RESOURCE_BUSY: module_handle {open_handle} is currently open and \
                 this request targets module_handle {requested}; only one J2534 device may \
                 be open at a time -- disconnect module_handle {open_handle} first (ADR-107)"
            ),
            PduError::PduErrResourceBusy,
            None,
        )
    }

    /// Rejects with [`Self::open_under_different_handle_status`] if a module
    /// OTHER than `requested` currently has its device open; otherwise
    /// returns the held lock guard (`self.device_id` is either empty or
    /// already matches `requested`) WITHOUT opening anything itself. Distinct
    /// from [`Self::ensure_open_device_for`]: this is for the handful of
    /// module-scoped call sites that must not act on a device other than the
    /// one they were asked about, but also must not open a device just to
    /// immediately act on it -- `ModuleDisconnect` (`rpc_module.rs`), which
    /// tears down whatever is open rather than opening anything, and
    /// `PDU_IOCTL_RESET` (`rpc_misc.rs`), which only resets already-live
    /// in-memory state (ADR-107 follow-up fix, Codex review on PR #110).
    ///
    /// Returns the held [`tokio::sync::MutexGuard`] rather than dropping it:
    /// a caller that only peeked-and-released left a window between this
    /// check and its own later teardown/reset work in which a concurrent
    /// request could open/close a different device, defeating the guard
    /// (TOCTOU race, edge-case-hunter finding on the prior fix). The caller
    /// must hold the returned guard for the ENTIRE guarded operation.
    /// `device_id` is the outermost lock -- it must never be acquired while
    /// holding any other service lock; callers hold the returned guard for
    /// the entire guarded operation (ADR-107 addendum).
    async fn lock_device_for(
        &self,
        requested: u32,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<(u32, DeviceId)>>, Status> {
        let slot = self.device_id.lock().await;
        if let Some((open_handle, _)) = *slot
            && open_handle != requested
        {
            return Err(Self::open_under_different_handle_status(
                open_handle,
                requested,
            ));
        }
        Ok(slot)
    }

    /// Rejects with `PDU_ERR_MODULE_NOT_CONNECTED` (ISO 22900-2 §9.4.29.2
    /// NOTE 1, Table 12) if no device is currently open for `requested`, or
    /// with [`Self::open_under_different_handle_status`] if a DIFFERENT
    /// module's device is open (ADR-107); otherwise returns the held guard
    /// alongside the already-open `DeviceId` WITHOUT opening one itself.
    ///
    /// For all 7 module-scoped `PDU_IOCTL_*` commands (`rpc_misc.rs`):
    /// `RESET`/`READ_VBATT`/`SET_PROG_VOLTAGE`/`READ_PROG_VOLTAGE` (which
    /// actually touch the device) and `GENERIC`/`GET_CABLE_ID`/
    /// `READ_IGNITION_SENSE_STATE` (unsupported by this adapter regardless,
    /// but which must still reject on connection state before falling
    /// through to their own `Status::unimplemented`). NOTE 1 lists the only
    /// D-PDU API functions allowed to run before a `ModuleConnect`
    /// (`GetResourceIds`/`GetObjectId`/`GetConflictingResources`/
    /// `GetStatus`) -- `PDUIoCtl` is not among them, so every module-scoped
    /// IOCTL must reject rather than silently connecting the module on the
    /// caller's behalf, or skipping the check because the command itself is
    /// unsupported (Codex review, PR #143). Distinct from
    /// [`Self::ensure_open_device_for`], the lazy-open helper `ModuleConnect`
    /// and `GetVersion` themselves use (A2-8).
    ///
    /// "Connected" here means "a device is open under `requested` AND
    /// `module_state.status == PduModstReady`" -- device-open however it got
    /// that way (an explicit `ModuleConnect`, or `GetVersion`/
    /// `CreateComLogicalLink`'s own lazy-open, ADR-107 Decision (d)) is
    /// necessary but not sufficient: `events::handle_channel_hard_error` sets
    /// `module_state.status` to `PduModstNotAvail` on a lost-comm event
    /// without closing `device_id` (Codex review, PR #143), so a stale-open
    /// slot on a module whose comms have since died must still reject. This
    /// does not track whether `ModuleConnect` itself was ever called: NOTE 1
    /// keys `PDU_ERR_MODULE_NOT_CONNECTED` to the module's status
    /// (`PDU_MODST_READY`), not to call history, and in this service the
    /// READY-equivalent state is "device open, with `module_state.status`
    /// still `PduModstReady`". Deliberate, not an oversight (ADR-107 Accepted
    /// Residual #5). `ModuleConnect` (`rpc_module_connect`) reads this same
    /// status to decide whether to succeed at all (ADR-131: it rejects with
    /// `PDU_ERR_FCT_FAILED`, not a false success, when the already-open path
    /// finds `NotAvail`) but never itself writes `PduModstReady` -- only a
    /// fresh `PassThruOpen` (`ensure_open_device_inner`) does that, on a
    /// genuine open, never on the already-open no-op path.
    ///
    /// `device_id` is the outermost lock -- acquired first here, matching
    /// every other call site's ordering (ADR-107 addendum).
    async fn require_connected_device_for(
        &self,
        requested: u32,
    ) -> Result<
        (
            tokio::sync::MutexGuard<'_, Option<(u32, DeviceId)>>,
            DeviceId,
        ),
        Status,
    > {
        let slot = self.device_id.lock().await;
        match *slot {
            Some((open_handle, id)) if open_handle == requested => {
                let module_state = self.module_state.lock().await;
                if module_state.status != vci_service_interface::PduModuleStatus::PduModstReady {
                    // ISO 22900-2 §9.4.29.2.1 use case (a): API calls to a
                    // module are only permitted while it is in the
                    // PDU_MODST_READY state. `device_id` being open is
                    // necessary but not sufficient -- `handle_channel_hard_error`
                    // (events.rs) sets `module_state.status` to
                    // `PduModstNotAvail` on a lost-comm event without closing
                    // `device_id` (Codex review, PR #143), so this must be
                    // checked in addition to, not instead of, the open-device
                    // check above.
                    let last_error = module_state.last_error.clone();
                    drop(module_state);
                    return Err(Self::module_not_avail_status(requested, last_error));
                }
                drop(module_state);
                Ok((slot, id))
            }
            Some((open_handle, _)) => Err(Self::open_under_different_handle_status(
                open_handle,
                requested,
            )),
            None => {
                let last_error = self.module_state.lock().await.last_error.clone();
                Err(Self::module_not_connected_status(requested, last_error))
            }
        }
    }

    /// Builds the `PDU_ERR_MODULE_NOT_CONNECTED` rejection for a
    /// module-scoped call site addressed to a handle with no device open at
    /// all (A2-8; ISO 22900-2 §9.4.29.2 NOTE 1, Table 12) -- the client
    /// genuinely never called `ModuleConnect` (or it never succeeded), so
    /// telling them to call it is the correct recovery instruction. Distinct
    /// from [`Self::module_not_avail_status`] (Codex review, PR #143): a
    /// device that IS open but whose module a hard error marked
    /// `PduModstNotAvail` needs a different instruction, since `ModuleConnect`
    /// alone will not recover it (ADR-131).
    fn module_not_connected_status(requested: u32, last_error: TrackedError) -> Status {
        state_guard_status(
            Code::FailedPrecondition,
            format!(
                "PDU_ERR_MODULE_NOT_CONNECTED: module_handle {requested} has not been \
                 connected -- call ModuleConnect first"
            ),
            PduError::PduErrModuleNotConnected,
            Some(last_error),
        )
    }

    /// Builds the `PDU_ERR_MODULE_NOT_CONNECTED` rejection for a
    /// module-scoped call site addressed to a handle whose device IS open,
    /// but whose module a hard channel error has marked `PduModstNotAvail`
    /// (ADR-131). Distinct message from [`Self::module_not_connected_status`]:
    /// telling the client to "call ModuleConnect first" here would be wrong
    /// and would leave them stuck, since `ModuleConnect` itself now rejects
    /// this exact state (`PDU_ERR_FCT_FAILED`) rather than recovering it --
    /// only `ModuleDisconnect` then `ModuleConnect` does (Codex review, PR
    /// #143).
    fn module_not_avail_status(requested: u32, last_error: TrackedError) -> Status {
        state_guard_status(
            Code::FailedPrecondition,
            format!(
                "PDU_ERR_MODULE_NOT_CONNECTED: module_handle {requested} lost communication \
                 with the VCI -- call ModuleDisconnect, then ModuleConnect to reconnect"
            ),
            PduError::PduErrModuleNotConnected,
            Some(last_error),
        )
    }

    fn empty_response() -> Response<vci_service_interface::Response> {
        Response::new(vci_service_interface::Response {})
    }

    fn parse_u32_token(value: &str) -> u32 {
        value
            .split(|c: char| !c.is_ascii_digit())
            .find(|token| !token.is_empty())
            .and_then(|token| token.parse::<u32>().ok())
            .unwrap_or(0)
    }

    async fn next_logical_link_handle(&self) -> u32 {
        loop {
            let candidate = {
                let mut next = self.next_cll_handle.lock().await;
                let h = next.wrapping_add(1);
                *next = h;
                h
            };
            if candidate == 0 {
                continue; // 0 is reserved; skip
            }
            if !self.logical_links.lock().await.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    async fn next_primitive_handle(&self) -> u32 {
        loop {
            let candidate = {
                let mut next = self.next_cop_handle.lock().await;
                let h = next.wrapping_add(1);
                *next = h;
                h
            };
            if candidate == 0 {
                continue; // 0 is reserved; skip
            }
            if !self.primitives.lock().await.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    /// Allocates the next `connect_generation` value. Plain monotonic
    /// counter, not a map key, so (unlike `next_logical_link_handle`/
    /// `next_primitive_handle`) it needs no uniqueness loop and no "0 is
    /// reserved" skip: a freshly-created, never-yet-connected
    /// `LogicalLinkState` naturally defaults `connect_generation: 0`, and the
    /// first real connect's value is `1`, so `0` inherently means "never
    /// connected" -- never observed as a live comparison value since
    /// `StartComPrimitive` already requires a connected CLL (ADR-086).
    async fn next_connect_generation(&self) -> u64 {
        let mut next = self.next_connect_generation.lock().await;
        *next = next.wrapping_add(1);
        *next
    }

    /// See `next_occupancy_epoch`'s field doc comment (ADR-161).
    async fn next_occupancy_epoch(&self) -> u64 {
        let mut next = self.next_occupancy_epoch.lock().await;
        *next = next.wrapping_add(1);
        *next
    }

    async fn get_link_state(&self, handle: u32) -> Result<LinkView, Status> {
        let links = self.logical_links.lock().await;
        links
            .get(&handle)
            .map(|s| s.view())
            .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))
    }

    /// Terminates and removes the subscription identified by `key` (ADR-115
    /// round 6, closing a gap design-advisor found in round 4/5's own fix):
    /// besides removing `key` from `subscriptions` and sending `Cancelled`,
    /// also clears the backing CLL queue's `live_sender` when it still points
    /// at the sender being removed -- otherwise that queue would keep
    /// treating an orphaned-but-open channel as live (a `tx.send()` to it
    /// still returns `Ok` until tonic drops the receiver), so a future push
    /// would believe it delivered live when nobody is reading anymore.
    ///
    /// Resolves the queue Arc via `logical_links` first, then drops that
    /// guard, before taking `subscriptions` -- the established
    /// `logical_links` -> `subscriptions` -> queue lock order (see
    /// `logical_links`'s own doc comment). `key` is only ever a CLL-keyed
    /// subscription (`(DEFAULT_MODULE_HANDLE, cll_handle)`) when
    /// `key.0 == DEFAULT_MODULE_HANDLE && key.1 != PDU_HANDLE_UNDEF`; a
    /// module- or system-handle key has no backing queue to clear.
    ///
    /// `queue` must be the CLL's own `Arc<Mutex<CllEventQueue>>`, supplied
    /// by the caller -- **not** re-derived from `self.logical_links` here.
    /// Every real call site (`DestroyComLogicalLink`) removes its own
    /// `logical_links` entry *before* calling this, so a self-lookup would
    /// always find nothing and silently no-op the `live_sender` clear this
    /// exists for (a gap present in this function's first cut: `edge-case-
    /// hunter` caught it dead at both of its only production call sites
    /// before this ever reached Codex). The caller already holds the
    /// removed `LogicalLinkState` (and thus its queue `Arc`) in scope at the
    /// point it calls this -- pass it through directly. `None` when the CLL
    /// truly has no queue to clear (a module/system-handle key, or a
    /// genuinely already-gone CLL with no in-scope `Arc` left anywhere --
    /// harmless in that case, since such a queue is unreachable by any
    /// future producer regardless).
    pub(super) async fn terminate_subscription(
        &self,
        key: SubscriptionKey,
        queue: Option<&Arc<Mutex<CllEventQueue>>>,
    ) {
        let mut subs = self.subscriptions.lock().await;
        if let Some(tx) = subs.remove(&key) {
            if let Some(queue) = queue {
                let mut q = queue.lock().await;
                if q.live_sender
                    .as_ref()
                    .is_some_and(|live| live.same_channel(&tx))
                {
                    q.live_sender = None;
                }
            }
            drop(subs);
            let _ = tx.send(Err(Status::cancelled("Subscription terminated")));
        }
    }

    /// Terminates and removes all active subscriptions (ADR-115 round 6 --
    /// see `terminate_subscription`'s own doc comment for the same
    /// `live_sender` gap this closes, applied to every CLL-keyed
    /// subscription at once).
    ///
    /// `queues` must be captured by the caller from `logical_links` *before*
    /// clearing it -- the sole production call site (`ModuleDisconnect`)
    /// clears `logical_links` before calling this, so a self-lookup here
    /// would always find nothing (the same dead-at-the-real-call-site gap
    /// `terminate_subscription` had; see its own doc comment).
    pub(super) async fn terminate_all_subscriptions(
        &self,
        queues: &HashMap<u32, Arc<Mutex<CllEventQueue>>>,
    ) {
        let drained: Vec<(SubscriptionKey, SubscriptionSender)> = {
            let mut subs = self.subscriptions.lock().await;
            let drained: Vec<_> = std::mem::take(&mut *subs).into_iter().collect();
            for (key, tx) in &drained {
                if key.0 == DEFAULT_MODULE_HANDLE
                    && key.1 != PDU_HANDLE_UNDEF
                    && let Some(queue) = queues.get(&key.1)
                {
                    let mut q = queue.lock().await;
                    if q.live_sender
                        .as_ref()
                        .is_some_and(|live| live.same_channel(tx))
                    {
                        q.live_sender = None;
                    }
                }
            }
            drained
        };
        for (_, tx) in drained {
            let _ = tx.send(Err(Status::cancelled("Subscription terminated")));
        }
    }
}

#[tonic::async_trait]
impl VciService for J2534Service {
    async fn get_module_ids(
        &self,
        request: Request<vci_service_interface::GetModuleIdsRequest>,
    ) -> Result<Response<vci_service_interface::ModuleIdsResponse>, Status> {
        self.rpc_get_module_ids(request).await
    }

    async fn module_connect(
        &self,
        request: Request<vci_service_interface::ModuleConnectRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        self.rpc_module_connect(request).await
    }

    async fn module_disconnect(
        &self,
        request: Request<vci_service_interface::ModuleDisconnectRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        self.rpc_module_disconnect(request).await
    }

    async fn get_version(
        &self,
        request: Request<vci_service_interface::GetVersionRequest>,
    ) -> Result<Response<vci_service_interface::VersionResponse>, Status> {
        self.rpc_get_version(request).await
    }

    async fn get_timestamp(
        &self,
        request: Request<vci_service_interface::GetTimestampRequest>,
    ) -> Result<Response<vci_service_interface::TimestampResponse>, Status> {
        self.rpc_get_timestamp(request).await
    }

    async fn get_resource_status(
        &self,
        request: Request<vci_service_interface::GetResourceStatusRequest>,
    ) -> Result<Response<vci_service_interface::ResourceStatusResponse>, Status> {
        self.rpc_get_resource_status(request).await
    }

    async fn get_resource_ids(
        &self,
        request: Request<vci_service_interface::GetResourceIdsRequest>,
    ) -> Result<Response<vci_service_interface::ResourceIdsResponse>, Status> {
        self.rpc_get_resource_ids(request).await
    }

    async fn get_conflicting_resources(
        &self,
        request: Request<vci_service_interface::GetConflictingResourcesRequest>,
    ) -> Result<Response<vci_service_interface::ConflictingResourcesResponse>, Status> {
        self.rpc_get_conflicting_resources(request).await
    }

    async fn create_com_logical_link(
        &self,
        request: Request<vci_service_interface::CreateComLogicalLinkRequest>,
    ) -> Result<Response<vci_service_interface::ComLogicalLinkResponse>, Status> {
        self.rpc_create_com_logical_link(request).await
    }

    async fn destroy_com_logical_link(
        &self,
        request: Request<vci_service_interface::DestroyComLogicalLinkRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        self.rpc_destroy_com_logical_link(request).await
    }

    async fn connect_com_logical_link(
        &self,
        request: Request<vci_service_interface::ConnectComLogicalLinkRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        self.rpc_connect_com_logical_link(request).await
    }

    async fn disconnect_com_logical_link(
        &self,
        request: Request<vci_service_interface::DisconnectComLogicalLinkRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        self.rpc_disconnect_com_logical_link(request).await
    }

    async fn lock_resource(
        &self,
        request: Request<vci_service_interface::LockResourceRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        self.rpc_lock_resource(request).await
    }

    async fn unlock_resource(
        &self,
        request: Request<vci_service_interface::UnlockResourceRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        self.rpc_unlock_resource(request).await
    }

    async fn get_com_param(
        &self,
        request: Request<vci_service_interface::GetComParamRequest>,
    ) -> Result<Response<vci_service_interface::ComParamResponse>, Status> {
        self.rpc_get_com_param(request).await
    }

    async fn set_com_param(
        &self,
        request: Request<vci_service_interface::SetComParamRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        self.rpc_set_com_param(request).await
    }

    async fn start_com_primitive(
        &self,
        request: Request<vci_service_interface::StartComPrimitiveRequest>,
    ) -> Result<Response<vci_service_interface::ComPrimitiveResponse>, Status> {
        self.rpc_start_com_primitive(request).await
    }

    async fn cancel_com_primitive(
        &self,
        request: Request<vci_service_interface::CancelComPrimitiveRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        self.rpc_cancel_com_primitive(request).await
    }

    async fn get_status(
        &self,
        request: Request<vci_service_interface::GetStatusRequest>,
    ) -> Result<Response<vci_service_interface::StatusResponse>, Status> {
        self.rpc_get_status(request).await
    }

    async fn get_event_item(
        &self,
        request: Request<vci_service_interface::GetEventItemRequest>,
    ) -> Result<Response<vci_service_interface::EventItemResponse>, Status> {
        self.rpc_get_event_item(request).await
    }

    type SubscribeEventStream = EventStream;

    async fn subscribe_event(
        &self,
        request: Request<vci_service_interface::SubscribeEventRequest>,
    ) -> Result<Response<Self::SubscribeEventStream>, Status> {
        self.rpc_subscribe_event(request).await
    }

    async fn io_ctl(
        &self,
        request: Request<vci_service_interface::IoCtlRequest>,
    ) -> Result<Response<vci_service_interface::IoCtlResponse>, Status> {
        self.rpc_io_ctl(request).await
    }

    async fn get_object_id(
        &self,
        request: Request<vci_service_interface::GetObjectIdRequest>,
    ) -> Result<Response<vci_service_interface::ObjectIdResponse>, Status> {
        self.rpc_get_object_id(request).await
    }

    async fn get_unique_resp_id_table(
        &self,
        request: Request<vci_service_interface::GetUniqueRespIdTableRequest>,
    ) -> Result<Response<vci_service_interface::UniqueRespIdTableResponse>, Status> {
        self.rpc_get_unique_resp_id_table(request).await
    }

    async fn set_unique_resp_id_table(
        &self,
        request: Request<vci_service_interface::SetUniqueRespIdTableRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        self.rpc_set_unique_resp_id_table(request).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-128 Codex-review round 1: `record` after `purge` for the same
    /// `cll_handle` must be a no-op -- the atomicity guarantee that closes
    /// the resurrection race (a straggling terminal emitter's `record` call
    /// landing after a concurrent `DestroyComLogicalLink`'s `purge` already
    /// ran). Unit-tests the `TerminalCopsLedger` logic directly rather than
    /// trying to force the actual async interleaving through the gRPC mock
    /// harness -- this crate's established, repeatedly-documented precedent
    /// (ADR-086 rounds 11/13 among others) is that this class of race has no
    /// natural preemption point on the `grpc_mock` suite's single-threaded
    /// (`current_thread`) test runtime, even though the race is reachable in
    /// production (`main.rs` runs the default multi-threaded `#[tokio::main]`
    /// runtime).
    #[test]
    fn terminal_cops_ledger_purge_blocks_a_later_stale_record_for_the_same_cll() {
        let mut ledger = TerminalCopsLedger::default();
        ledger.purge(5); // CLL 5 destroyed, purge wins the race
        ledger.record(
            100,
            5,
            vci_service_interface::PduComPrimitiveStatus::PduCopstFinished,
        ); // straggling emitter arrives after
        assert!(
            ledger.lookup(100).is_none(),
            "a record for an already-destroyed cll_handle must never resurrect an entry"
        );
    }

    /// The ordinary (non-racing) case: `record` before `purge` still works
    /// exactly as before -- `purge` only blocks FUTURE records, it does not
    /// retroactively reject one that already landed.
    #[test]
    fn terminal_cops_ledger_record_before_purge_is_visible_until_purged() {
        let mut ledger = TerminalCopsLedger::default();
        ledger.record(
            100,
            5,
            vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled,
        );
        assert_eq!(
            ledger.lookup(100),
            Some((
                5,
                vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled
            ))
        );
        ledger.purge(5);
        assert!(ledger.lookup(100).is_none());
    }

    /// `purge_many` (the `ModuleDisconnect` bulk-teardown path) has the same
    /// atomic guarantee as single-CLL `purge`, for every handle in the batch.
    #[test]
    fn terminal_cops_ledger_purge_many_blocks_stale_records_for_every_cll_in_the_batch() {
        let mut ledger = TerminalCopsLedger::default();
        ledger.purge_many(&[5, 6, 7]);
        for (cop, cll) in [(100, 5), (101, 6), (102, 7)] {
            ledger.record(
                cop,
                cll,
                vci_service_interface::PduComPrimitiveStatus::PduCopstFinished,
            );
        }
        for cop in [100, 101, 102] {
            assert!(ledger.lookup(cop).is_none());
        }
    }

    /// `unmark_destroyed` (called by `CreateComLogicalLink` when
    /// `next_logical_link_handle`'s wrapping allocator reissues a number) is
    /// what makes a reused `cll_handle` usable again -- without it, every
    /// future terminal status for the brand-new CLL would be silently
    /// dropped by `record`'s guard forever.
    #[test]
    fn terminal_cops_ledger_unmark_destroyed_allows_a_reused_handle_to_record_again() {
        let mut ledger = TerminalCopsLedger::default();
        ledger.purge(5);
        ledger.unmark_destroyed(5); // cll_handle 5 reissued to a fresh CLL
        ledger.record(
            100,
            5,
            vci_service_interface::PduComPrimitiveStatus::PduCopstFinished,
        );
        assert_eq!(
            ledger.lookup(100),
            Some((
                5,
                vci_service_interface::PduComPrimitiveStatus::PduCopstFinished
            ))
        );
    }

    /// Codex review finding on PR #107 (ADR-105): an unrecognized (but
    /// present) module_handle is an emulated D-PDU-level handle rejection,
    /// in ADR-105's rich-error scope -- unlike a missing module_handle
    /// field, which stays a plain request-validation failure.
    #[test]
    fn require_module_handle_rejects_unknown_handle_with_rich_error_detail() {
        let status = J2534Service::require_module_handle(
            Some(vci_service_interface::ModuleHandle {
                module_handle: DEFAULT_MODULE_HANDLE + 1,
            }),
            1,
        )
        .expect_err("a module_handle outside 1..=module_count should be rejected");
        assert_eq!(status.code(), Code::InvalidArgument);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrInvalidHandle as i32);
    }

    #[test]
    fn require_module_handle_missing_field_stays_plain_status() {
        let status = J2534Service::require_module_handle(None, 1)
            .expect_err("a missing module_handle should be rejected");
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(
            vci_service_interface::error_detail_from_status(&status).is_none(),
            "pure request-shape validation should not carry an ErrorDetail"
        );
    }

    /// ADR-107: a `module_handle` within `1..=module_count` (the configured
    /// `modules` array's length) is accepted, generalizing beyond the old
    /// fixed `DEFAULT_MODULE_HANDLE`-only check.
    #[test]
    fn require_module_handle_accepts_any_handle_in_the_configured_range() {
        for handle in 1..=3u32 {
            J2534Service::require_module_handle(
                Some(vci_service_interface::ModuleHandle {
                    module_handle: handle,
                }),
                3,
            )
            .unwrap_or_else(|err| panic!("handle {handle} in 1..=3 should be accepted: {err}"));
        }
        let status = J2534Service::require_module_handle(
            Some(vci_service_interface::ModuleHandle { module_handle: 4 }),
            3,
        )
        .expect_err("handle 4 is outside 1..=3 and should be rejected");
        assert_eq!(status.code(), Code::InvalidArgument);
        let status_zero = J2534Service::require_module_handle(
            Some(vci_service_interface::ModuleHandle { module_handle: 0 }),
            3,
        )
        .expect_err("handle 0 should be rejected (handles are 1-based)");
        assert_eq!(status_zero.code(), Code::InvalidArgument);
    }

    /// ADR-056: CP_P2Star, the D-PDU standard "extended P2" timing param,
    /// is the authoritative source for the RC78 per-occurrence reload window
    /// -- ADR-102: CP_RC78CompletionTimeout is a separate, independent total
    /// ceiling, reinstated rather than merged into the reload value, even
    /// when both are set to different values. ADR-057: both are stored in
    /// microseconds, like every other D-PDU timing ComParam
    /// (CP_P2Min/CP_P2Max, N_Ar, ...), and must be converted to ms, not read
    /// raw.
    #[test]
    fn rc_handling_config_reads_rc78_p2_star_and_ceiling_separately() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_RC78_HANDLING, 1);
        params.unum32.insert(PARAM_P2_STAR, 5_000_000);
        params
            .unum32
            .insert(PARAM_RC78_COMPLETION_TIMEOUT, 25_000_000);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO15765));
        assert!(cfg.rc78_handling);
        assert_eq!(cfg.rc78_p2_star_ms, 5_000);
        assert_eq!(cfg.rc78_total_ceiling_ms, Some(25_000));
    }

    #[test]
    fn rc_handling_config_defaults_rc78_timeout_to_5000ms_when_p2_star_unset() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_RC78_HANDLING, 1);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO15765));
        assert_eq!(cfg.rc78_p2_star_ms, 5_000);
        assert_eq!(cfg.rc78_total_ceiling_ms, None);
    }

    /// ADR-102 (edge-case review follow-up): an explicit `0` -- distinct from
    /// the ComParam being absent entirely, already covered above -- must be
    /// treated identically: `CP_P2Star = 0` still falls back to the 5000ms
    /// default (never a real zero-length reload window), and
    /// `CP_RC78CompletionTimeout = 0` still disables the ceiling (`None`,
    /// never a 0ms instant-timeout ceiling). Both read paths share the same
    /// `filter(|&v| v > 0)` guard this pins.
    #[test]
    fn rc_handling_config_treats_explicit_zero_same_as_absent() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_RC78_HANDLING, 1);
        params.unum32.insert(PARAM_P2_STAR, 0);
        params.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 0);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO15765));
        assert_eq!(cfg.rc78_p2_star_ms, 5_000);
        assert_eq!(cfg.rc78_total_ceiling_ms, None);
    }

    /// ADR-057: CP_RC21/23 CompletionTimeout and RequestTime are also
    /// microsecond-denominated D-PDU timing ComParams needing the same
    /// conversion CP_P2Star gets, matching this codebase's existing preset
    /// literals (e.g. `PARAM_RC21_REQUEST_TIME = 200_000` us = 200 ms, not
    /// 200 seconds).
    #[test]
    fn rc_handling_config_converts_rc21_rc23_timings_from_microseconds() {
        let mut params = ComParamSet::default();
        params
            .unum32
            .insert(PARAM_RC21_COMPLETION_TIMEOUT, 1_300_000);
        params.unum32.insert(PARAM_RC21_REQUEST_TIME, 200_000);
        params
            .unum32
            .insert(PARAM_RC23_COMPLETION_TIMEOUT, 5_050_000);
        params.unum32.insert(PARAM_RC23_REQUEST_TIME, 25_000);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO15765));
        assert_eq!(cfg.rc21_completion_timeout_ms, 1_300);
        assert_eq!(cfg.rc21_request_time_ms, 200);
        assert_eq!(cfg.rc23_completion_timeout_ms, 5_050);
        assert_eq!(cfg.rc23_request_time_ms, 25);
    }

    /// Unchanged from before ADR-125: on a CAN-family CLL (the floor's gate,
    /// `ChannelProtocol::is_kwp_family`, is false), RC21/RC23 request time
    /// still falls back to the plain 25 ms literal exactly as it did
    /// pre-ADR-125, regardless of whether `CP_P3Min`/`CP_RC2xRequestTime`
    /// happen to be present in the ComParam set at all.
    #[test]
    fn rc_handling_config_defaults_rc21_rc23_timings_when_unset() {
        let cfg = RcHandlingConfig::from_params(
            &ComParamSet::default(),
            ChannelProtocol::from_raw(j2534_0404::ISO15765),
        );
        assert_eq!(cfg.rc21_completion_timeout_ms, 5_000);
        assert_eq!(cfg.rc21_request_time_ms, 25);
        assert_eq!(cfg.rc23_completion_timeout_ms, 5_000);
        assert_eq!(cfg.rc23_request_time_ms, 25);
    }

    /// ADR-125 (B11 in `iso22900-2-conformance-audit.md`): Annex I.1.4.3
    /// steps 1-3 -- an explicit `CP_RC2xRequestTime = 0` on a K-line CLL is
    /// the documented ISO 14230-3 "re-request after CP_P3Min" case, not
    /// "unset". Before this fix it was silently coerced to a hardcoded
    /// 25 ms with no spec basis; it must now resolve to the CLL's own
    /// `CP_P3Min`.
    #[test]
    fn rc_handling_config_uses_p3_min_when_rc2x_request_time_is_explicit_zero() {
        let mut params = ComParamSet::default();
        params.unum32.insert(ComParamId(j2534_0404::P3_MIN), 55_000);
        params.unum32.insert(PARAM_RC21_REQUEST_TIME, 0);
        params.unum32.insert(PARAM_RC23_REQUEST_TIME, 0);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO14230));
        assert_eq!(cfg.rc21_request_time_ms, 55);
        assert_eq!(cfg.rc23_request_time_ms, 55);
    }

    /// Step 2's "the greater value ... is used" is a `Max`, not a
    /// substitution: a configured, non-zero request time that is still
    /// shorter than `CP_P3Min` must be floored up to `CP_P3Min`, and one
    /// already longer than `CP_P3Min` must be left unchanged.
    #[test]
    fn rc_handling_config_rc2x_request_time_floored_to_p3_min_when_smaller() {
        let mut params = ComParamSet::default();
        params.unum32.insert(ComParamId(j2534_0404::P3_MIN), 55_000);
        params.unum32.insert(PARAM_RC21_REQUEST_TIME, 10_000);
        params.unum32.insert(PARAM_RC23_REQUEST_TIME, 200_000);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO14230));
        assert_eq!(cfg.rc21_request_time_ms, 55);
        assert_eq!(cfg.rc23_request_time_ms, 200);
    }

    /// Codex review, PR #136, round 2: a K-line client may legally
    /// `SetComParam(CP_P3Min, 0)` -- a real configured `0`, not an absence
    /// -- and `Max(0, 0) = 0` must still apply on a K-line CLL. (Round 2's
    /// original fix gated on `CP_P3Min`'s raw-map presence rather than the
    /// protocol; round 3 replaced that with the protocol-based gate this
    /// test now exercises directly via `ChannelProtocol::ISO14230`.)
    #[test]
    fn rc_handling_config_explicit_zero_p3_min_is_distinguished_from_absent() {
        let mut params = ComParamSet::default();
        params.unum32.insert(ComParamId(j2534_0404::P3_MIN), 0);
        params.unum32.insert(PARAM_RC21_REQUEST_TIME, 0);
        params.unum32.insert(PARAM_RC23_REQUEST_TIME, 0);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO14230));
        assert_eq!(cfg.rc21_request_time_ms, 0);
        assert_eq!(cfg.rc23_request_time_ms, 0);
    }

    /// Codex review, PR #136 (round 1): a CAN/ISO15765 client can
    /// runtime-enable `CP_RC21Handling`/`CP_RC23Handling` via `SetComParam`
    /// while its preset's `CP_RC21RequestTime`/`CP_RC23RequestTime` stays
    /// at the Table-B.19-seeded `0` (`iso15765_4_common`,
    /// `iso_15765_3_on_iso_15765_2`). The ADR-125 floor must not engage on
    /// a CAN-family CLL, so this must still fall back to the plain 25 ms
    /// literal, not an unintended immediate (0 ms) retry loop.
    #[test]
    fn rc_handling_config_can_preset_zero_request_time_keeps_25ms_fallback_without_p3_min() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_RC21_REQUEST_TIME, 0);
        params.unum32.insert(PARAM_RC23_REQUEST_TIME, 0);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO15765));
        assert_eq!(cfg.rc21_request_time_ms, 25);
        assert_eq!(cfg.rc23_request_time_ms, 25);
    }

    /// Codex review, PR #136 (round 3): resource `ISO_14230_3_on_ISO_15765_2`
    /// (0x0204, `comparam_defaults.rs`) is a CAN-family (ISO15765 hardware
    /// protocol) preset that nonetheless seeds a literal, client-invisible
    /// `CP_P3Min = 55_000` (rejected by `comparam_support.rs`'s
    /// `is_can_param` if a client tried to `SetComParam`/`GetComParam` it).
    /// A presence-based gate on the raw ComParamSet would wrongly apply the
    /// K-line floor here; the protocol-based gate must not, since Annex
    /// I.1.4 never applies to a CAN-family CLL regardless of what's in its
    /// ComParamSet.
    #[test]
    fn rc_handling_config_can_family_ignores_inert_p3_min_from_preset() {
        let mut params = ComParamSet::default();
        params.unum32.insert(ComParamId(j2534_0404::P3_MIN), 55_000);
        params.unum32.insert(PARAM_RC21_REQUEST_TIME, 0);
        params.unum32.insert(PARAM_RC23_REQUEST_TIME, 10_000);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO15765));
        assert_eq!(cfg.rc21_request_time_ms, 25);
        assert_eq!(cfg.rc23_request_time_ms, 10);
    }

    /// Codex review, PR #136 (round 4): a legacy/raw K-line CLL created via
    /// a bare numeric `resource_id`/`protocol_id` matching no
    /// resources-table row (`rpc_create_com_logical_link`'s
    /// `resource_row = None` fallback) gets a completely empty
    /// `ComParamSet` -- no preset ever seeded `CP_RC2xRequestTime` or
    /// `CP_P3Min` at all, unlike every real preset in this codebase. The
    /// protocol-based gate alone is not sufficient: `CP_RC2xRequestTime`
    /// itself must also be present, or a truly-unconfigured K-line CLL
    /// would wrongly read as the Annex I.1.4.3 explicit-`0` case
    /// (`Max(0, 0) = 0`) instead of falling back to the pre-ADR-125 25 ms
    /// default for "nothing configured at all."
    #[test]
    fn rc_handling_config_kwp_family_with_no_preset_keeps_25ms_fallback() {
        let cfg = RcHandlingConfig::from_params(
            &ComParamSet::default(),
            ChannelProtocol::from_raw(j2534_0404::ISO14230),
        );
        assert_eq!(cfg.rc21_request_time_ms, 25);
        assert_eq!(cfg.rc23_request_time_ms, 25);
    }

    /// Codex review, PR #136 (round 5, design-advisor consult after an
    /// edge-case-hunter adversarial pass): Annex I.1.4's own title covers
    /// "SAE J1850 VPW and ISO 14230 protocols", and `PARAM_RC21_REQUEST_TIME`/
    /// `PARAM_RC23_REQUEST_TIME` are legally `SetComParam`-able on J1850 --
    /// a client that explicitly configures `CP_RC23RequestTime = 0` on a
    /// J1850 CLL must get that value verbatim (`0`), not the CAN/K-line-
    /// unconfigured 25 ms fallback. Unlike K-line, there is no `CP_P3Min`
    /// floor on this branch at all (see the field/`from_params` doc
    /// comments) -- the value is used as-is, whatever it is.
    #[test]
    fn rc_handling_config_j1850_explicit_zero_request_time_used_verbatim() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_RC21_REQUEST_TIME, 0);
        params.unum32.insert(PARAM_RC23_REQUEST_TIME, 0);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::J1850VPW));
        assert_eq!(cfg.rc21_request_time_ms, 0);
        assert_eq!(cfg.rc23_request_time_ms, 0);
    }

    /// Round 5, continued: a J1850 CLL whose `CP_RC2xRequestTime` was never
    /// configured at all (e.g. a legacy/raw CLL, same class as the round-4
    /// K-line case above) must keep the 25 ms "unconfigured" fallback, not
    /// misread absence as an explicit `0`.
    #[test]
    fn rc_handling_config_j1850_family_with_no_preset_keeps_25ms_fallback() {
        let cfg = RcHandlingConfig::from_params(
            &ComParamSet::default(),
            ChannelProtocol::from_raw(j2534_0404::J1850PWM),
        );
        assert_eq!(cfg.rc21_request_time_ms, 25);
        assert_eq!(cfg.rc23_request_time_ms, 25);
    }

    /// Round 5, continued: `CP_P3Min` has no defined value for J1850 (Table
    /// B.10/B.19, J2534-1 v04.04 scope it to ISO 9141/ISO 14230 only), so
    /// even if a stray `CP_P3Min` entry were present in a J1850 CLL's raw
    /// `ComParamSet` (never reachable via `SetComParam` in practice --
    /// `comparam_support.rs`'s J1850 arms never allowlist it, mirroring
    /// round 3's CAN finding), it must never floor a J1850 request time --
    /// unlike K-line, where an equally "inert" `CP_P3Min` DOES apply.
    #[test]
    fn rc_handling_config_j1850_ignores_p3_min_even_if_present() {
        let mut params = ComParamSet::default();
        params.unum32.insert(ComParamId(j2534_0404::P3_MIN), 55_000);
        params.unum32.insert(PARAM_RC21_REQUEST_TIME, 0);
        params.unum32.insert(PARAM_RC23_REQUEST_TIME, 10_000);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::J1850VPW));
        assert_eq!(cfg.rc21_request_time_ms, 0);
        assert_eq!(cfg.rc23_request_time_ms, 10);
    }

    /// ADR-100 Decision §3, resolved (b): builds an `RcHandlingConfig` with
    /// RC78/21/23 handling enabled and `rc_byte_offset = 2` (the standard
    /// UDS/KWP `0x7F <SID> <NRC>` shape), optionally attaching a request SID.
    fn rc_cfg_at_offset_2(request_sid: Option<u8>) -> RcHandlingConfig {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_RC78_HANDLING, 1);
        params.unum32.insert(PARAM_RC21_HANDLING, 1);
        params.unum32.insert(PARAM_RC23_HANDLING, 1);
        params.unum32.insert(PARAM_RC_BYTE_OFFSET, 2);
        RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO15765))
            .with_request_sid(request_sid)
    }

    /// A `7F <SID> 78` frame that echoes the COP's own request SID is
    /// detected as a pending RC.
    #[test]
    fn detect_pending_rc_matches_negative_response_echoing_request_sid() {
        let cfg = rc_cfg_at_offset_2(Some(0x22));
        assert_eq!(cfg.detect_pending_rc(&[0x7F, 0x22, 0x78], 0), Some(0x78));
    }

    /// A `7F <SID> 78` frame for a *different* SID (e.g. an unrelated COP's
    /// own pending response) must not be claimed by this COP -- the closed
    /// half of the ADR-088 pending-RC capture residual.
    #[test]
    fn detect_pending_rc_rejects_negative_response_for_other_sid() {
        let cfg = rc_cfg_at_offset_2(Some(0x22));
        assert_eq!(cfg.detect_pending_rc(&[0x7F, 0x3E, 0x78], 0), None);
    }

    /// A genuine *positive* response whose byte at `rc_byte_offset`
    /// coincidentally equals `0x78` (e.g. UDS `62 F1 90 78 ...`) must not be
    /// misdetected as a pending RC -- the latent, pre-existing defect this
    /// gate fixes as a side effect (no `0x7F` first-byte check existed at
    /// all before this gate).
    #[test]
    fn detect_pending_rc_rejects_positive_response_with_coincidental_rc_byte() {
        let cfg = rc_cfg_at_offset_2(Some(0x22));
        assert_eq!(cfg.detect_pending_rc(&[0x62, 0xF1, 0x90, 0x78], 0), None);
    }

    /// Same coincidental-byte positive response, but with `request_sid`
    /// unknown (`None`): the `0x7F` first-byte requirement alone (not the
    /// SID echo) is what rejects it, since a positive response never starts
    /// with `0x7F`.
    #[test]
    fn detect_pending_rc_rejects_positive_response_even_without_known_request_sid() {
        let cfg = rc_cfg_at_offset_2(None);
        assert_eq!(cfg.detect_pending_rc(&[0x62, 0xF1, 0x90, 0x78], 0), None);
    }

    /// With `request_sid` unknown (`None`), a genuine `7F <any-SID> 78`
    /// negative response is still detected -- the SID echo is only enforced
    /// when the expected SID is actually known.
    #[test]
    fn detect_pending_rc_matches_negative_response_when_request_sid_unknown() {
        let cfg = rc_cfg_at_offset_2(None);
        assert_eq!(cfg.detect_pending_rc(&[0x7F, 0x3E, 0x78], 0), Some(0x78));
    }

    /// `rc_byte_offset < 2` framings (the non-UDS/KWP protocols the ComParam
    /// exists to serve) are an accepted residual (ADR-100 Consequences):
    /// behavior is completely unchanged by the request-SID gate, even when
    /// the byte at the offset does not follow a `0x7F` first byte and the
    /// request SID is known and mismatched.
    #[test]
    fn detect_pending_rc_unaffected_when_rc_byte_offset_below_2() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_RC78_HANDLING, 1);
        params.unum32.insert(PARAM_RC_BYTE_OFFSET, 0);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO15765))
                .with_request_sid(Some(0x22));
        assert_eq!(
            cfg.detect_pending_rc(&[0x78, 0x04, 0x00], 0),
            Some(0x78),
            "rc_byte_offset < 2 must not gate on a 0x7F first byte or SID echo"
        );
    }

    /// ADR-196 Decision item 3b: same shape as `rc_cfg_at_offset_2`, but with
    /// `rc_byte_offset` parameterized so a RawMode raw-prefix test can build
    /// a config whose `rc_byte_offset` sits at the standard UDS/KWP shape's
    /// logical position (2) RE-BASED by a nonzero `raw_prefix`.
    fn rc_cfg_at_offset(rc_byte_offset: u32, request_sid: Option<u8>) -> RcHandlingConfig {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_RC78_HANDLING, 1);
        params.unum32.insert(PARAM_RC21_HANDLING, 1);
        params.unum32.insert(PARAM_RC23_HANDLING, 1);
        params.unum32.insert(PARAM_RC_BYTE_OFFSET, rc_byte_offset);
        RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO15765))
            .with_request_sid(request_sid)
    }

    /// ADR-196 Decision item 3b: a `7F <SID> 78` shape re-based by
    /// `raw_prefix = 4` (`rc_byte_offset = 6`, i.e. `raw_prefix + 2`) is
    /// still detected as a pending RC, with the `0x7F`/SID-echo anchors
    /// shifted to `raw_prefix`/`raw_prefix + 1` -- the shifted-gate mirror of
    /// `detect_pending_rc_matches_negative_response_echoing_request_sid`.
    #[test]
    fn detect_pending_rc_matches_negative_response_at_nonzero_raw_prefix() {
        let cfg = rc_cfg_at_offset(6, Some(0x22));
        assert_eq!(
            cfg.detect_pending_rc(&[0x11, 0x22, 0x33, 0x44, 0x7F, 0x22, 0x78], 4),
            Some(0x78)
        );
    }

    /// ADR-196 Decision item 3b: `rc_byte_offset < raw_prefix` (the RC byte
    /// would fall INSIDE the raw CAN-ID prefix itself, not a real RC-byte
    /// position) declines to detect at all, via `checked_sub` underflow --
    /// matching this mechanism's own "decline rather than risk a false
    /// positive" philosophy.
    #[test]
    fn detect_pending_rc_declines_when_offset_falls_inside_raw_prefix() {
        let cfg = rc_cfg_at_offset(2, Some(0x22));
        assert_eq!(
            cfg.detect_pending_rc(&[0x11, 0x22, 0x78, 0x44, 0x7F, 0x22, 0x78], 4),
            None,
            "rc_byte_offset (2) < raw_prefix (4) must decline, even though data[2] == 0x78"
        );
    }

    /// ADR-196 Decision item 3b: the ADR-100 `rc_byte_offset < 2` carve-out
    /// shifts intact to `raw_prefix..raw_prefix + 2` (`logical_offset < 2`)
    /// -- an offset in that band stays ungated on the 0x7F-first-byte/SID-echo
    /// shape check, mirroring `detect_pending_rc_unaffected_when_rc_byte_offset_below_2`
    /// shifted by `raw_prefix`.
    #[test]
    fn detect_pending_rc_raw_prefix_carveout_band_stays_ungated() {
        let cfg = rc_cfg_at_offset(4, Some(0x22));
        assert_eq!(
            cfg.detect_pending_rc(&[0x11, 0x22, 0x33, 0x44, 0x78, 0x04, 0x00], 4),
            Some(0x78),
            "rc_byte_offset (4) == raw_prefix (4), logical_offset 0 < 2, must not gate on a \
             0x7F first byte or SID echo"
        );
    }

    /// ADR-146: `TimingChangeConfig::from_params` is `None` unless the
    /// channel is ISO14230 specifically (not the wider KWP-family grouping)
    /// AND `CP_ModifyTiming` is enabled -- produces `KwpAccess` in that case.
    #[test]
    fn timing_change_config_from_params_requires_iso14230_and_modify_timing_enabled() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_MODIFY_TIMING, 1);

        assert!(
            matches!(
                TimingChangeConfig::from_params(&params, ChannelProtocol::ISO14230),
                Some(TimingChangeConfig::KwpAccess(_))
            ),
            "ISO14230 with CP_ModifyTiming enabled must produce Some(KwpAccess)"
        );
        assert!(
            TimingChangeConfig::from_params(&params, ChannelProtocol::ISO9141).is_none(),
            "ISO9141 (KWP-family but not ISO14230) must never enable this mechanism"
        );
        assert!(
            TimingChangeConfig::from_params(&params, ChannelProtocol::CAN).is_none(),
            "raw CAN (not ISO15765) must never enable this mechanism"
        );

        let disabled = ComParamSet::default(); // CP_ModifyTiming absent/0
        assert!(
            TimingChangeConfig::from_params(&disabled, ChannelProtocol::ISO14230).is_none(),
            "CP_ModifyTiming disabled (0/absent) must produce None even on ISO14230"
        );
    }

    /// ADR-150: `TimingChangeConfig::from_params` produces `UdsSession` for
    /// ISO15765 with `CP_ModifyTiming` enabled, `None` when it's disabled --
    /// the gating matrix's ISO15765 half, mirroring the ISO14230 test above.
    #[test]
    fn timing_change_config_from_params_iso15765_gating_matrix() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_MODIFY_TIMING, 1);

        assert!(
            matches!(
                TimingChangeConfig::from_params(&params, ChannelProtocol::ISO15765),
                Some(TimingChangeConfig::UdsSession(_))
            ),
            "ISO15765 with CP_ModifyTiming enabled must produce Some(UdsSession)"
        );

        let disabled = ComParamSet::default(); // CP_ModifyTiming absent/0
        assert!(
            TimingChangeConfig::from_params(&disabled, ChannelProtocol::ISO15765).is_none(),
            "CP_ModifyTiming disabled (0/absent) must produce None even on ISO15765"
        );
    }

    /// Codex review, PR #22 round 1 (positive pin, kept accurate after the
    /// round-3 correction -- `ISO_14229_3_ON_ISO_15765_2` is one of the
    /// explicitly enumerated members of `uds_session_timing_applies`'s
    /// match set, genuine UDS services over ISO15765, not merely a member
    /// of the wider ISO15765 hardware family): a CLL created via this
    /// resource-table extended protocol (a distinct `ChannelProtocol` value
    /// from bare `ISO15765`) must still get `timing_cfg`.
    #[test]
    fn timing_change_config_from_params_recognizes_extended_iso15765_family_protocols() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_MODIFY_TIMING, 1);

        assert!(
            matches!(
                TimingChangeConfig::from_params(
                    &params,
                    ChannelProtocol::ISO_14229_3_ON_ISO_15765_2
                ),
                Some(TimingChangeConfig::UdsSession(_))
            ),
            "ISO_14229_3_ON_ISO_15765_2 (genuine UDS services) must produce Some(UdsSession) \
             exactly like bare ISO15765 does"
        );
    }

    /// Sibling of the test above, for the KWP side (positive pin, kept
    /// accurate after the round-3 correction -- `ISO_14230_3_ON_ISO_14230_2`
    /// is one of the explicitly enumerated members of
    /// `kwp_access_timing_applies`'s match set, genuine KWP2000 services
    /// over its own ISO 14230-2 transport).
    #[test]
    fn timing_change_config_from_params_recognizes_extended_iso14230_family_protocols() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_MODIFY_TIMING, 1);

        assert!(
            matches!(
                TimingChangeConfig::from_params(
                    &params,
                    ChannelProtocol::ISO_14230_3_ON_ISO_14230_2
                ),
                Some(TimingChangeConfig::KwpAccess(_))
            ),
            "ISO_14230_3_ON_ISO_14230_2 (genuine KWP2000 services) must produce \
             Some(KwpAccess) exactly like bare ISO14230 does"
        );
    }

    /// Codex review, PR #22 round 3: sharing a hardware channel is NOT
    /// sufficient for either mechanism to apply -- `ISO_14230_3_ON_ISO_15765_2`
    /// (KWP2000 services over CAN transport), `SAE_J2190_ON_ISO_15765_2`,
    /// and `ISO_15031_5_ON_ISO_15765_4` (OBD services) all share the
    /// ISO15765 HARDWARE channel with genuine UDS variants, but their
    /// diagnostic SERVICES layer is not ISO 14229-3 -- a coincidentally
    /// SID-0x10/0x50-shaped exchange on one of them is not a UDS
    /// DiagnosticSessionControl response and must not derive
    /// `CP_P2Max`/`CP_P2Star`. `SAE_J2190_ON_ISO_14230_2` is the KWP-side
    /// sibling of this same bug shape (round 3's own finding only named
    /// the ISO15765 cases, but the identical over-broad-family mistake was
    /// present on both arms before this fix).
    ///
    /// Fail-without-the-fix control: reverting `from_params` to match on
    /// `protocol.j2534_protocol_id()` (this test's pre-fix shape) was
    /// confirmed to make every one of these assertions fail.
    #[test]
    fn timing_change_config_from_params_rejects_non_applicable_extended_protocols_sharing_a_hardware_channel()
     {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_MODIFY_TIMING, 1);

        for protocol in [
            ChannelProtocol::ISO_14230_3_ON_ISO_15765_2,
            ChannelProtocol::SAE_J2190_ON_ISO_15765_2,
            ChannelProtocol::ISO_15031_5_ON_ISO_15765_4,
        ] {
            assert!(
                TimingChangeConfig::from_params(&params, protocol).is_none(),
                "{protocol:?} shares the ISO15765 hardware channel with genuine UDS variants \
                 but does not run UDS DiagnosticSessionControl services -- must produce None, \
                 not Some(UdsSession)"
            );
        }
        assert!(
            TimingChangeConfig::from_params(&params, ChannelProtocol::SAE_J2190_ON_ISO_14230_2)
                .is_none(),
            "SAE_J2190_ON_ISO_14230_2 shares the ISO14230 hardware channel with genuine KWP \
             variants but does not run KWP2000's own ISO 14230-3 services -- must produce \
             None, not Some(KwpAccess)"
        );
    }

    /// Codex review, PR #22 round 3: `ISO_15031_5_ON_ISO_14230_4` (OBD/ISO
    /// 15031-5 services) is a deliberate INCLUSION, not a naive-symmetry
    /// exclusion -- unlike its ISO15765-side counterpart
    /// (`ISO_15031_5_ON_ISO_15765_4`, rejected above), both ISO 22900-2
    /// editions' Table B.10 mark the ISO 14230-4 applicability column for
    /// `CP_ModifyTiming`, and its own description scopes the 0x83/0xC3
    /// mechanism to the ISO 14230-2 data link, which ISO 14230-4 (an
    /// application-layer profile riding the same data link) shares. A
    /// future "tidy up the KWP match set" pass that assumes
    /// services-layer-must-equal-ISO-14230-3 symmetry would wrongly remove
    /// this -- this test exists specifically to catch that.
    #[test]
    fn timing_change_config_from_params_includes_obd_over_kline_as_a_deliberate_asymmetry() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_MODIFY_TIMING, 1);

        assert!(
            matches!(
                TimingChangeConfig::from_params(
                    &params,
                    ChannelProtocol::ISO_15031_5_ON_ISO_14230_4
                ),
                Some(TimingChangeConfig::KwpAccess(_))
            ),
            "ISO_15031_5_ON_ISO_14230_4 must produce Some(KwpAccess) despite not being a \
             KWP2000-services protocol -- ISO 22900-2 Table B.10 marks CP_ModifyTiming \
             applicable on its ISO 14230-4 data link regardless"
        );
    }

    /// `AccessTimingConfig::with_request` (via `TimingChangeConfig::with_request`)
    /// populates `tpi`/`tpi3_request_bytes` only for a genuine SID 0x83
    /// request, extracting TPI=3's 5 accompanying bytes; any other request
    /// (or a too-short TPI=3 request) leaves both `None`.
    #[test]
    fn timing_change_config_with_request_extracts_tpi_and_tpi3_bytes() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_MODIFY_TIMING, 1);
        let base = TimingChangeConfig::from_params(&params, ChannelProtocol::ISO14230).unwrap();
        let kwp = |cfg: TimingChangeConfig| match cfg {
            TimingChangeConfig::KwpAccess(cfg) => cfg,
            TimingChangeConfig::UdsSession(_) => panic!("expected KwpAccess"),
        };

        let tpi2 = kwp(base.clone().with_request(&[0x83, 0x02], 0));
        assert_eq!(tpi2.tpi, Some(2));
        assert_eq!(tpi2.tpi3_request_bytes, None);

        let tpi3 = kwp(base
            .clone()
            .with_request(&[0x83, 0x03, 10, 20, 30, 40, 50], 0));
        assert_eq!(tpi3.tpi, Some(3));
        assert_eq!(tpi3.tpi3_request_bytes, Some([10, 20, 30, 40, 50]));

        let not_access_timing = kwp(base.clone().with_request(&[0x22, 0xF1, 0x90], 0));
        assert_eq!(not_access_timing.tpi, None);

        let empty_request = kwp(base.with_request(&[], 0));
        assert_eq!(empty_request.tpi, None);
    }

    /// ADR-198 Phase 2: a RawMode=ON K-line CLL's `cop_data` carries the
    /// client's own KWP header (`tx_header::kwp_header_bytes`'s wire shape)
    /// ahead of the genuine SID 0x83 request bytes -- `with_request`'s
    /// `tx_prefix` parameter must anchor the TPI/TPI3 capture there instead
    /// of at position 0, mirroring `session_timing_config_with_request_
    /// captures_session_at_nonzero_tx_prefix` for the KWP variant. Before
    /// ADR-198, `KwpAccess`'s `with_request` was unreachable for a RawMode
    /// CLL at all (K-line RawMode was rejected at `CreateComLogicalLink`,
    /// ADR-196 Decision item 3b), so this is new coverage, not a
    /// regression pin.
    #[test]
    fn timing_change_config_with_request_extracts_tpi_at_nonzero_tx_prefix() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_MODIFY_TIMING, 1);
        let base = TimingChangeConfig::from_params(&params, ChannelProtocol::ISO14230).unwrap();
        let kwp = |cfg: TimingChangeConfig| match cfg {
            TimingChangeConfig::KwpAccess(cfg) => cfg,
            TimingChangeConfig::UdsSession(_) => panic!("expected KwpAccess"),
        };

        // tx_prefix = 3: a CARB/ISO9141-2-shaped 3-byte header (format,
        // target, source) precedes the genuine SID 0x83 TPI=3 request.
        let tpi3 = kwp(base.with_request(&[0x40, 0x10, 0xF1, 0x83, 0x03, 10, 20, 30, 40, 50], 3));
        assert_eq!(
            tpi3.tpi,
            Some(3),
            "the TPI byte must be read starting at tx_prefix (3), not position 0"
        );
        assert_eq!(tpi3.tpi3_request_bytes, Some([10, 20, 30, 40, 50]));
    }

    /// ADR-150: `SessionTimingConfig::with_request` captures the request's
    /// session type masked with `0x7F` (stripping the suppress-positive-
    /// response bit) for a genuine SID 0x10 request; rejects (leaves `None`)
    /// a masked value of `0` (ISO 22900-2 session field valid range
    /// [1;127]); and is a no-op for any other request SID.
    #[test]
    fn session_timing_config_with_request_masks_suppress_bit_and_rejects_zero() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_MODIFY_TIMING, 1);
        let base = TimingChangeConfig::from_params(&params, ChannelProtocol::ISO15765).unwrap();
        let uds = |cfg: TimingChangeConfig| match cfg {
            TimingChangeConfig::UdsSession(cfg) => cfg,
            TimingChangeConfig::KwpAccess(_) => panic!("expected UdsSession"),
        };

        let suppress_bit_set = uds(base.clone().with_request(&[0x10, 0x83], 0));
        assert_eq!(
            suppress_bit_set.session,
            Some(0x03),
            "the suppress-positive-response bit (0x80) must be masked off"
        );

        let session_zero = uds(base.clone().with_request(&[0x10, 0x00], 0));
        assert_eq!(
            session_zero.session, None,
            "a masked session value of 0 must be rejected, not adopted"
        );

        let not_diag_session_control = uds(base.with_request(&[0x22, 0xF1, 0x90], 0));
        assert_eq!(
            not_diag_session_control.session, None,
            "a non-0x10 request must be a no-op"
        );
    }

    /// ADR-196 Decision item 3b: a RawMode=ON CLL's `cop_data` carries the
    /// raw CAN-ID prefix (4, or 5 with an Address Extension byte) ahead of
    /// the genuine SID 0x10 request bytes -- `with_request`'s `tx_prefix`
    /// parameter must anchor the SID/session capture there instead of at
    /// position 0, for both the 4-byte and 5-byte prefix widths.
    #[test]
    fn session_timing_config_with_request_captures_session_at_nonzero_tx_prefix() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_MODIFY_TIMING, 1);
        let base = TimingChangeConfig::from_params(&params, ChannelProtocol::ISO15765).unwrap();
        let uds = |cfg: TimingChangeConfig| match cfg {
            TimingChangeConfig::UdsSession(cfg) => cfg,
            TimingChangeConfig::KwpAccess(_) => panic!("expected UdsSession"),
        };

        // tx_prefix = 4 (plain 4-byte CAN-ID prefix, no Address Extension).
        let at_prefix_4 = uds(base
            .clone()
            .with_request(&[0x11, 0x22, 0x33, 0x44, 0x10, 0x83], 4));
        assert_eq!(
            at_prefix_4.session,
            Some(0x03),
            "the SID/session bytes must be read starting at tx_prefix (4), not position 0"
        );

        // tx_prefix = 5 (4-byte CAN-ID prefix plus an Address Extension
        // byte).
        let at_prefix_5 = uds(base.with_request(&[0x11, 0x22, 0x33, 0x44, 0x99, 0x10, 0x83], 5));
        assert_eq!(
            at_prefix_5.session,
            Some(0x03),
            "the SID/session bytes must be read starting at tx_prefix (5), not position 0"
        );
    }

    /// ADR-147: a `7F <SID> 78` frame is unhandled when `CP_RC78Handling`
    /// (and the other two RC codes) are disabled -- the RC engine was never
    /// asked to auto-handle it.
    #[test]
    fn is_unhandled_negative_true_when_matching_rc_code_disabled() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_RC_BYTE_OFFSET, 2);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO15765))
                .with_request_sid(Some(0x22));
        assert!(cfg.is_unhandled_negative(&[0x7F, 0x22, 0x78], 0));
    }

    /// Companion: when `CP_RC78Handling` is enabled, the same frame is NOT
    /// unhandled -- `detect_pending_rc` claims it instead.
    #[test]
    fn is_unhandled_negative_false_when_matching_rc_code_enabled() {
        let cfg = rc_cfg_at_offset_2(Some(0x22));
        assert!(!cfg.is_unhandled_negative(&[0x7F, 0x22, 0x78], 0));
        assert_eq!(cfg.detect_pending_rc(&[0x7F, 0x22, 0x78], 0), Some(0x78));
    }

    /// An NRC that isn't one of the RC21/RC23/RC78-mapped codes at all is
    /// unhandled regardless of the RC engine's configuration.
    #[test]
    fn is_unhandled_negative_true_for_unmapped_nrc() {
        let cfg = rc_cfg_at_offset_2(Some(0x22));
        assert!(cfg.is_unhandled_negative(&[0x7F, 0x22, 0x31], 0));
    }

    /// A genuine positive response is never classified as unhandled, even
    /// when its byte at `rc_byte_offset` coincidentally matches an RC code.
    #[test]
    fn is_unhandled_negative_false_for_positive_response() {
        let cfg = rc_cfg_at_offset_2(Some(0x22));
        assert!(!cfg.is_unhandled_negative(&[0x62, 0xF1, 0x90, 0x78], 0));
    }

    /// A `7F <SID> ...` for a different SID than this COP's own request must
    /// not be classified against this COP's config (same SID-echo gate as
    /// `detect_pending_rc`).
    #[test]
    fn is_unhandled_negative_false_for_mismatched_sid() {
        let cfg = rc_cfg_at_offset_2(Some(0x22));
        assert!(!cfg.is_unhandled_negative(&[0x7F, 0x3E, 0x31], 0));
    }

    /// `rc_byte_offset < 2` framings decline to classify at all (accepted
    /// residual -- the timeout hook still covers those protocols).
    #[test]
    fn is_unhandled_negative_declines_when_rc_byte_offset_below_2() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_RC_BYTE_OFFSET, 0);
        let cfg =
            RcHandlingConfig::from_params(&params, ChannelProtocol::from_raw(j2534_0404::ISO15765))
                .with_request_sid(Some(0x22));
        assert!(!cfg.is_unhandled_negative(&[0x7F, 0x04, 0x00], 0));
    }

    /// ADR-196 Decision item 3b: same shifted-gate shape as
    /// `detect_pending_rc_matches_negative_response_at_nonzero_raw_prefix`
    /// -- a `7F <SID> <unmapped NRC>` re-based by `raw_prefix = 4` is still
    /// detected as an unhandled negative response.
    #[test]
    fn is_unhandled_negative_true_at_nonzero_raw_prefix() {
        let cfg = rc_cfg_at_offset(6, Some(0x22));
        assert!(cfg.is_unhandled_negative(&[0x11, 0x22, 0x33, 0x44, 0x7F, 0x22, 0x31], 4));
    }

    /// ADR-196 Decision item 3b: `rc_byte_offset < raw_prefix` (`checked_sub`
    /// underflow) declines to classify, mirroring `detect_pending_rc`'s own
    /// identical decline.
    #[test]
    fn is_unhandled_negative_declines_when_offset_falls_inside_raw_prefix() {
        let cfg = rc_cfg_at_offset(2, Some(0x22));
        assert!(!cfg.is_unhandled_negative(&[0x11, 0x22, 0x78, 0x44, 0x7F, 0x22, 0x78], 4));
    }

    /// ADR-196 Decision item 3b: the existing `rc_byte_offset < 2` decline
    /// shifts intact to `logical_offset < 2` (`raw_prefix..raw_prefix + 2`).
    #[test]
    fn is_unhandled_negative_declines_within_raw_prefix_carveout_band() {
        let cfg = rc_cfg_at_offset(4, Some(0x22));
        assert!(!cfg.is_unhandled_negative(&[0x11, 0x22, 0x33, 0x44, 0x78, 0x04, 0x00], 4));
    }

    /// ADR-100 Decision §3's round-5 addendum: `is_vacuous()` is `mask.is_empty()
    /// || pattern.is_empty()`, not the narrower `&&` reading -- both-empty is
    /// vacuous, both-non-empty-matching-lengths is not, and either side alone
    /// being empty (`mask` non-empty/`pattern` empty, or the reverse) is ALSO
    /// vacuous, mirroring `ExpectedResponse::matches`'s real `cmp_len == 0`
    /// semantics rather than the AND phrasing this section's prose used before
    /// the addendum.
    #[test]
    fn is_vacuous_matches_cmp_len_zero_semantics_not_the_narrower_and_reading() {
        let both_empty = ExpectedResponse {
            mask: Vec::new(),
            pattern: Vec::new(),
            unique_resp_ids: Vec::new(),
            acceptance_id: 0,
        };
        assert!(both_empty.is_vacuous(), "both empty must be vacuous");

        let both_non_empty = ExpectedResponse {
            mask: vec![0xFF],
            pattern: vec![0x7E],
            unique_resp_ids: Vec::new(),
            acceptance_id: 0,
        };
        assert!(
            !both_non_empty.is_vacuous(),
            "matching-length non-empty mask/pattern must not be vacuous"
        );

        let mask_only = ExpectedResponse {
            mask: vec![0xFF],
            pattern: Vec::new(),
            unique_resp_ids: Vec::new(),
            acceptance_id: 0,
        };
        assert!(
            mask_only.is_vacuous(),
            "non-empty mask with empty pattern must be vacuous (matches cmp_len == 0)"
        );

        let pattern_only = ExpectedResponse {
            mask: Vec::new(),
            pattern: vec![0x7E],
            unique_resp_ids: Vec::new(),
            acceptance_id: 0,
        };
        assert!(
            pattern_only.is_vacuous(),
            "empty mask with non-empty pattern must be vacuous (matches cmp_len == 0)"
        );
    }

    /// Builds a minimal `J2534Service` backed by the mock J2534 cdylib, with
    /// no logical links registered -- for exercising `require_connected_device_for`
    /// directly, without driving an actual RPC. Mirrors
    /// `rpc_link.rs::tests::service_with_auto_can_mode_and_no_links`'s
    /// construction shape.
    async fn minimal_service() -> J2534Service {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::SingleChannel,
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(vec![crate::config::ModuleEntry {
                label: "j2534-0404".to_string(),
                pname: None,
            }]),
            device_id: Arc::new(Mutex::new(None)),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_epoch: Arc::new(AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(AtomicU64::new(0)),
            logical_links: Arc::new(Mutex::new(HashMap::new())),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(HashMap::new())),
            primitives: Arc::new(Mutex::new(HashMap::new())),
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
        }
    }

    /// Codex review finding on PR #143 (P1): `require_connected_device_for`
    /// used to treat an open `device_id` alone as proof of "connected," but
    /// `events::handle_channel_hard_error` sets `module_state.status` to
    /// `PduModstNotAvail` on a lost-comm event WITHOUT closing `device_id` --
    /// so a module whose comms have died, but whose stale `device_id` slot
    /// is still `Some`, must still reject module-scoped IOCTLs with
    /// `PDU_ERR_MODULE_NOT_CONNECTED` (ISO 22900-2 §9.4.29.2.1 use case (a):
    /// only `PDU_MODST_READY` allows API function calls).
    #[tokio::test]
    async fn require_connected_device_for_rejects_a_module_marked_not_avail_by_a_hard_error() {
        let service = minimal_service().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));
        service.module_state.lock().await.status =
            vci_service_interface::PduModuleStatus::PduModstNotAvail;

        let status = service
            .require_connected_device_for(DEFAULT_MODULE_HANDLE)
            .await
            .expect_err(
                "a device open under a module whose status a hard error marked NotAvail must \
                 still reject",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrModuleNotConnected as i32);
    }

    /// Sanity check pairing the test above: an open `device_id` whose module
    /// status is still `PduModstReady` (the ordinary case) is accepted, so
    /// the new status check above is not itself over-broad.
    #[tokio::test]
    async fn require_connected_device_for_accepts_an_open_device_with_ready_status() {
        let service = minimal_service().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));

        let _ = service
            .require_connected_device_for(DEFAULT_MODULE_HANDLE)
            .await
            .expect("an open device under a PduModstReady module should be accepted");
    }

    /// Unit-conversion bug fix (found during ADR-159's investigation, fixed
    /// as a standalone follow-up): `CP_N_Bs`/`CP_Cr` are stored in µs, like
    /// every other D-PDU API timing param (`CP_P2Max`), but `isotp_n_bs_
    /// timeout_ms`/`isotp_n_cr_timeout_ms` used to return the raw stored
    /// value as if already milliseconds -- a ~1000x timeout inflation for
    /// the software-ISO-TP emulation path (ADR-046). Pins the correct
    /// µs-to-ms conversion (`div_ceil`, rounding up, floor of 1ms for any
    /// nonzero value below 1000µs) for both.
    #[test]
    fn isotp_n_bs_and_n_cr_timeout_ms_convert_from_microseconds() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_N_BS, 30_000);
        params.unum32.insert(PARAM_N_CR, 1_500_000);
        assert_eq!(
            params.isotp_n_bs_timeout_ms(),
            30,
            "30,000µs must convert to 30ms, not be returned as 30,000ms"
        );
        assert_eq!(
            params.isotp_n_cr_timeout_ms(),
            1500,
            "1,500,000µs must convert to 1500ms, not be returned as 1,500,000ms"
        );
    }

    /// A sub-1ms µs value rounds UP to 1ms (never 0 -- a zero timeout would
    /// be indistinguishable from "unset" elsewhere in this codebase's
    /// timeout conventions) via `div_ceil`, not down via plain integer
    /// division.
    #[test]
    fn isotp_n_bs_timeout_ms_rounds_a_sub_millisecond_value_up_to_one() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_N_BS, 1);
        assert_eq!(params.isotp_n_bs_timeout_ms(), 1);
    }

    /// Unset or explicitly zero both fall back to the documented 1000ms
    /// default -- unaffected by the µs-conversion fix, since the fallback
    /// itself is already a millisecond value, not a stored ComParam.
    #[test]
    fn isotp_n_bs_and_n_cr_timeout_ms_default_to_1000_when_unset_or_zero() {
        let unset = ComParamSet::default();
        assert_eq!(unset.isotp_n_bs_timeout_ms(), 1000);
        assert_eq!(unset.isotp_n_cr_timeout_ms(), 1000);

        let mut zeroed = ComParamSet::default();
        zeroed.unum32.insert(PARAM_N_BS, 0);
        zeroed.unum32.insert(PARAM_N_CR, 0);
        assert_eq!(zeroed.isotp_n_bs_timeout_ms(), 1000);
        assert_eq!(zeroed.isotp_n_cr_timeout_ms(), 1000);
    }

    /// ADR-192/Phase 7 Stage 7c: unset/`0` resolves to `None` (normal send).
    #[test]
    fn tp20_broadcast_address_absent_or_zero_resolves_none() {
        let unset = ComParamSet::default();
        assert_eq!(unset.tp20_broadcast_address(), None);

        let mut zeroed = ComParamSet::default();
        zeroed.unum32.insert(PARAM_TP20_BROADCAST_ADDRESS, 0);
        assert_eq!(zeroed.tp20_broadcast_address(), None);
    }

    /// ADR-192/Phase 7 Stage 7c: every value in the valid `0xF0-0xFF`
    /// broadcast-address range resolves `Some`, carrying the address through
    /// verbatim.
    #[test]
    fn tp20_broadcast_address_valid_range_resolves_some() {
        for addr in 0xF0u32..=0xFF {
            let mut params = ComParamSet::default();
            params.unum32.insert(PARAM_TP20_BROADCAST_ADDRESS, addr);
            assert_eq!(
                params.tp20_broadcast_address(),
                Some(addr as u8),
                "address {addr:#x} should resolve Some"
            );
        }
    }

    /// ADR-192/Phase 7 Stage 7c: a nonzero value outside `0xF0-0xFF` resolves
    /// `None` from this accessor -- the RPC layer is responsible for
    /// rejecting it outright (`rpc_primitive.rs`'s synchronous
    /// `PduErrInvalidParameters` check) BEFORE this accessor is ever
    /// consulted; this accessor's own fallback exists only so it can never
    /// panic/UB on a stale/racing out-of-range value.
    #[test]
    fn tp20_broadcast_address_out_of_range_nonzero_resolves_none() {
        for addr in [1u32, 0x0F, 0xEF, 0x100, 0xFFFF_FFFF] {
            let mut params = ComParamSet::default();
            params.unum32.insert(PARAM_TP20_BROADCAST_ADDRESS, addr);
            assert_eq!(
                params.tp20_broadcast_address(),
                None,
                "out-of-range address {addr:#x} should resolve None from this accessor"
            );
        }
    }
}
