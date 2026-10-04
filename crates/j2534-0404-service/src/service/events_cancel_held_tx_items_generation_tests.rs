use std::collections::HashSet;

use super::*;
use crate::service::ChannelProtocol;

const TEST_CLL: u32 = 1;
const LIVE_GENERATION: u64 = 3;
const STALE_GENERATION: u64 = 2;

/// A `LogicalLinkState` with every field at its `CreateComLogicalLink`
/// default (mirrors `events_registrant_lifecycle_tests.rs`'s own
/// `minimal_link()`), except `connect_generation`, `tx_suspended_by_ioctl`,
/// `tx_suspended_by_error`, and `stop_comm_pending`, which callers seed
/// explicitly for this module's ADR-161 generation-gating tests.
fn seeded_link(connect_generation: u64) -> LogicalLinkState {
    LogicalLinkState {
        channel_id: None,
        protocol: ChannelProtocol::CAN,
        hw_protocol_id: 0,
        software_isotp: false,
        uudt_channel_id: None,
        uudt_channel_key: None,
        isotp_rx: Arc::new(Mutex::new(HashMap::new())),
        connect_in_flight: std::sync::Weak::new(),
        connected: true,
        comm_started: true,
        raw_mode: false,
        checksum_mode: false,
        connect_generation,
        stop_comm_pending: true,
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
        tx_held: VecDeque::from([
            TxItem::StopComm {
                cop_handle: 100,
                cll_handle: TEST_CLL,
                protocol_id: 0,
                tx: None,
                connect_generation,
            },
            TxItem::StopComm {
                cop_handle: 101,
                cll_handle: TEST_CLL,
                protocol_id: 0,
                tx: None,
                connect_generation,
            },
        ]),
        tx_suspended_by_ioctl: true,
        tx_suspended_by_lock: false,
        tx_suspended_by_error: true,
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

/// ADR-161: `expected_generation: Some(g)` disagreeing with the live
/// `connect_generation` must skip the whole function -- no drain, no
/// suspend-flag reset, no `stop_comm_pending` clear, and no
/// `PduCopstCancelled` notification -- exactly as if `PDU_IOCTL_RESET`'s
/// snapshot had raced a disconnect+reconnect of this `cll_handle`.
///
/// Notification emission is asserted via `rx_buf.live_sender`, mirroring
/// `events_deliver_or_enqueue_live_sender_tests.rs`'s own harness pattern for
/// this file's live-delivery path (`deliver_or_enqueue`/`drain_queue_live`)
/// -- NOT the `subscriptions` map, which `emit_terminal_if_live`'s
/// `send_cop_status` only falls back to when no `LogicalLinkState` exists for
/// the CLL at all.
#[tokio::test]
async fn mismatched_expected_generation_skips_drain_flags_and_notification() {
    let link = seeded_link(LIVE_GENERATION);
    let rx_buf = Arc::clone(&link.rx_buf);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    rx_buf.lock().await.live_sender = Some(tx);

    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    let primitives = Arc::new(Mutex::new(HashMap::from([
        (
            100,
            CopEntry {
                cll_handle: TEST_CLL,
                dispatched: false,
                transmits: true,
                is_send_recv: false,
                cop_tag: None,
            },
        ),
        (
            101,
            CopEntry {
                cll_handle: TEST_CLL,
                dispatched: false,
                transmits: true,
                is_send_recv: false,
                cop_tag: None,
            },
        ),
    ])));
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let terminal_cops = Arc::new(Mutex::new(TerminalCopsLedger::default()));

    cancel_held_tx_items(
        &primitives,
        &logical_links,
        &subscriptions,
        &terminal_cops,
        TEST_CLL,
        Some(STALE_GENERATION),
        true,
    )
    .await;

    let links = logical_links.lock().await;
    let link = links.get(&TEST_CLL).unwrap();
    assert_eq!(
        link.tx_held.len(),
        2,
        "tx_held must stay fully populated on a generation mismatch"
    );
    assert!(
        link.tx_suspended_by_ioctl,
        "tx_suspended_by_ioctl must be untouched on a generation mismatch"
    );
    assert!(
        link.tx_suspended_by_error,
        "tx_suspended_by_error must be untouched on a generation mismatch"
    );
    assert_eq!(
        link.error_clear_seq, 0,
        "error_clear_seq must not bump on a generation mismatch"
    );
    assert!(
        link.stop_comm_pending,
        "stop_comm_pending must not clear on a generation mismatch"
    );
    drop(links);

    assert_eq!(
        primitives.lock().await.len(),
        2,
        "no held item's cop may be removed from primitives on a generation mismatch"
    );
    assert!(
        rx.try_recv().is_err(),
        "no PduCopstCancelled notification may be emitted on a generation mismatch"
    );
}

/// Companion case: a MATCHING `expected_generation` runs the drain/flag-reset/
/// notification exactly as `expected_generation: None` always has, pinning
/// that the new parameter is a pure narrowing (no behavior change on match).
#[tokio::test]
async fn matching_expected_generation_drains_and_notifies_as_before() {
    let link = seeded_link(LIVE_GENERATION);
    let rx_buf = Arc::clone(&link.rx_buf);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    rx_buf.lock().await.live_sender = Some(tx);

    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    let primitives = Arc::new(Mutex::new(HashMap::from([
        (
            100,
            CopEntry {
                cll_handle: TEST_CLL,
                dispatched: false,
                transmits: true,
                is_send_recv: false,
                cop_tag: None,
            },
        ),
        (
            101,
            CopEntry {
                cll_handle: TEST_CLL,
                dispatched: false,
                transmits: true,
                is_send_recv: false,
                cop_tag: None,
            },
        ),
    ])));
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let terminal_cops = Arc::new(Mutex::new(TerminalCopsLedger::default()));

    cancel_held_tx_items(
        &primitives,
        &logical_links,
        &subscriptions,
        &terminal_cops,
        TEST_CLL,
        Some(LIVE_GENERATION),
        true,
    )
    .await;

    let links = logical_links.lock().await;
    let link = links.get(&TEST_CLL).unwrap();
    assert!(
        link.tx_held.is_empty(),
        "tx_held must be fully drained on a matching generation"
    );
    assert!(!link.tx_suspended_by_ioctl);
    assert!(!link.tx_suspended_by_error);
    assert!(!link.stop_comm_pending);
    drop(links);

    assert!(primitives.lock().await.is_empty());
    let notification = rx
        .try_recv()
        .expect(
            "a PduCopstCancelled notification must be emitted per drained item on a matching \
             generation",
        )
        .expect("Ok notification");
    assert!(
        matches!(
            notification.event_data,
            Some(EventNotificationData::Item(EventItem {
                data: Some(EventItemData::CopStatus(status)),
                ..
            })) if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ),
        "expected a CopStatus(PduCopstCancelled) notification, got {notification:?}"
    );
}
