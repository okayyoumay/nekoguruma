use super::*;

fn frame(timestamp: u32) -> CllQueueItem {
    CllQueueItem::Frame(ReceivedFrame {
        timestamp,
        data: Vec::new(),
        header_bytes: Vec::new(),
        footer_bytes: Vec::new(),
        unique_resp_identifier: 0,
        acceptance_id: 0,
        cop_handle: None,
        cop_tag: None,
        rx_status_flags: 0,
        ecu_timing_change: false,
        sw_can_hv_rx: false,
    })
}

/// Extracts a `ResultData` frame's `timestamp` from a `SubscribeEvent`
/// notification, panicking on anything else -- these tests only ever
/// deliver `Frame` items.
fn frame_timestamp(notification: EventNotification) -> u32 {
    match notification.event_data {
        Some(EventNotificationData::Item(EventItem {
            data: Some(EventItemData::ResultData(_)),
            timestamp,
            ..
        })) => timestamp,
        other => panic!("unexpected notification: {other:?}"),
    }
}

/// No `live_sender` at all: unchanged pre-round-4 behavior, kept as a
/// sanity baseline.
#[tokio::test]
async fn no_live_sender_still_enqueues_normally() {
    let queue = Arc::new(Mutex::new(CllEventQueue {
        event_queue_cap: 16,
        ..CllEventQueue::default()
    }));

    deliver_or_enqueue(&queue, 1, frame(7)).await;

    assert_eq!(queue.lock().await.items.len(), 1);
}

/// Codex's round-6 review scenario (edge-case-hunter item (a)): a
/// replacement subscriber attaches to a queue that already has a
/// backlog, and the very next push crosses this CLL's cap right around
/// that attachment. Since `deliver_or_enqueue` reads `live_sender`
/// fresh, under the lock, at the point of use -- not from a value some
/// caller captured earlier -- the newly-attached subscriber correctly
/// receives the `Lost` notification for the item this push evicts,
/// followed by the full surviving backlog draining live, FIFO, in the
/// same call. There is no longer a separate "capture, then maybe go
/// stale before the lock" step for a concurrent replacement to race at
/// all -- this is the structural fix, not a narrower guard.
#[tokio::test]
async fn replacement_subscriber_receives_live_delivery_including_lost_from_push_around_attachment()
{
    let queue = Arc::new(Mutex::new(CllEventQueue {
        event_queue_cap: 2,
        ..CllEventQueue::default()
    }));

    // A backlog builds up with no live subscriber attached yet.
    deliver_or_enqueue(&queue, 1, frame(1)).await;
    deliver_or_enqueue(&queue, 1, frame(2)).await;
    assert_eq!(queue.lock().await.items.len(), 2);

    // A `SubscribeEvent` replacement attaches -- mirroring
    // `rpc_subscribe_event`'s atomic insert-and-stamp critical section,
    // which writes `live_sender` under the queue's own lock.
    let (tx, mut rx) = mpsc::unbounded_channel();
    queue.lock().await.live_sender = Some(tx);

    // The next push crosses the cap: frame(1) is evicted (Lost), and
    // frame(3) is inserted.
    deliver_or_enqueue(&queue, 1, frame(3)).await;

    let lost = rx
        .try_recv()
        .expect("the newly-attached subscriber should receive the Lost notification")
        .expect("Ok notification");
    assert!(
        matches!(
            lost.event_data,
            Some(EventNotificationData::Lost(LostEventItemNotification {}))
        ),
        "expected a Lost notification, got {lost:?}"
    );
    let first = rx
        .try_recv()
        .expect("the surviving backlog should drain live, FIFO")
        .expect("Ok notification");
    let second = rx
        .try_recv()
        .expect("the newly-pushed item should also drain live")
        .expect("Ok notification");
    assert!(rx.try_recv().is_err(), "no further items should be pending");
    assert_eq!(
        frame_timestamp(first),
        2,
        "frame(1) was evicted; frame(2) drains first"
    );
    assert_eq!(frame_timestamp(second), 3);
    assert!(
        queue.lock().await.items.is_empty(),
        "queue should be empty after the live drain"
    );
}

/// Edge-case-hunter item (c): a `tx.send()` failure inside
/// `deliver_or_enqueue` (the receiver has been dropped) clears
/// `queue.live_sender` and preserves the triggering item in the queue --
/// round 3's existing fallback-to-enqueue behavior, plus the round-6
/// self-heal specifically asserted on `live_sender` itself, not just the
/// item's survival.
#[tokio::test]
async fn dead_sender_send_failure_clears_live_sender_and_preserves_item() {
    let queue = Arc::new(Mutex::new(CllEventQueue {
        event_queue_cap: 16,
        ..CllEventQueue::default()
    }));

    let (tx, rx) = mpsc::unbounded_channel();
    queue.lock().await.live_sender = Some(tx);
    // Drop the receiver so `tx.send()` is now guaranteed to fail --
    // deterministic for `mpsc::UnboundedSender`, not transient.
    drop(rx);

    deliver_or_enqueue(&queue, 1, frame(9)).await;

    let locked = queue.lock().await;
    assert!(
        locked.live_sender.is_none(),
        "a dead sender must be self-healed to None, not left pointing at an orphaned channel"
    );
    assert_eq!(
        locked.items.len(),
        1,
        "the item must survive in the queue when the live send fails"
    );
}

/// Builds a minimal `ReceivedFrame` for the deferred-finalization
/// delivery tests below -- only `timestamp` varies, everything else is
/// neutral filler (mirrors this module's own `frame()` helper, which
/// wraps the same shape in `CllQueueItem::Frame`).
fn received_frame(timestamp: u32) -> ReceivedFrame {
    ReceivedFrame {
        timestamp,
        data: Vec::new(),
        header_bytes: Vec::new(),
        footer_bytes: Vec::new(),
        unique_resp_identifier: 0,
        acceptance_id: 0,
        cop_handle: Some(1),
        cop_tag: None,
        rx_status_flags: 0,
        ecu_timing_change: true,
        sw_can_hv_rx: false,
    }
}

/// Codex round-5 Finding 1 (PR #17): direct regression test for
/// deferred-finalization delivery's queue-position guarantee. A
/// qualifying timing frame reserves its TRUE arrival position via
/// `reserve_pending_timing_frame`; a later, unrelated `Status` item for
/// the SAME CLL arrives (and is pushed into the real queue) BEFORE the
/// reservation finalizes. With a live subscriber attached throughout,
/// NEITHER item may be delivered live until `finalize_pending_timing_frame`
/// runs -- `drain_queue_live`'s head-of-line barrier withholds the status
/// item too, since it is queued behind the still-pending reservation.
/// Once finalized, both arrive over the channel IN ORDER: the reserved
/// timing frame first (its true arrival position), then the status item.
///
/// Fail-without/pass-with: temporarily removing the `PendingTimingFrame`
/// barrier check from `drain_queue_live` (so it pops through everything
/// unconditionally, like the pre-round-5 code) confirmed-fails this test
/// -- `drain_queue_live` then pops the still-pending reservation itself
/// and passes it to `cll_queue_item_to_event_item`, which hits its own
/// deliberate `unreachable!()` guard for `PendingTimingFrame` (see that
/// function's own doc comment) and panics; restoring the barrier check
/// makes the test pass again. (In a hypothetical build where that
/// defensive `unreachable!()` were itself weakened to a silent
/// conversion instead, the same missing barrier would let the status
/// item jump the queue and arrive live before the reservation
/// finalizes -- the scenario this test's assertions are actually
/// written to catch.)
#[tokio::test]
async fn a_pending_timing_frame_blocks_a_later_status_item_on_the_same_cll_until_finalized() {
    let queue = Arc::new(Mutex::new(CllEventQueue {
        event_queue_cap: 16,
        ..CllEventQueue::default()
    }));
    let (tx, mut rx) = mpsc::unbounded_channel();
    queue.lock().await.live_sender = Some(tx);

    let reservation_id = reserve_pending_timing_frame(&queue, 1, received_frame(10))
        .await
        .expect("push should succeed under this cap");

    deliver_or_enqueue(
        &queue,
        1,
        CllQueueItem::Status(TrackedStatus {
            event: StatusEvent::Cll(PduComLogicalLinkStatus::PduCllstOnline),
            timestamp: 20,
        }),
    )
    .await;

    assert!(
        rx.try_recv().is_err(),
        "neither item may be delivered live while the reservation is still pending"
    );
    assert_eq!(
        queue.lock().await.items.len(),
        2,
        "both items must still be sitting in the real queue, in their true arrival order"
    );

    finalize_pending_timing_frame(&queue, 1, reservation_id, true).await;

    let first = rx
        .try_recv()
        .expect("the finalized timing frame should now drain live")
        .expect("Ok notification");
    assert_eq!(
        frame_timestamp(first),
        10,
        "the reserved timing frame must arrive first, at its TRUE arrival position"
    );
    let second = rx
        .try_recv()
        .expect("the status item queued behind it should now also drain live")
        .expect("Ok notification");
    assert!(
        matches!(
            second.event_data,
            Some(EventNotificationData::Item(EventItem {
                data: Some(EventItemData::CllStatus(_)),
                ..
            }))
        ),
        "expected a CllStatus notification, got {second:?}"
    );
    assert!(rx.try_recv().is_err(), "no further items should be pending");
    assert!(
        queue.lock().await.items.is_empty(),
        "queue should be empty after the live drain"
    );
}

/// `GetEventItem`-shaped coverage (feasible without full RPC scaffolding,
/// per this test's own brief): with a `PendingTimingFrame` at the front
/// of `queue.items` and no live subscriber, `rpc_get_event_item`'s own
/// `match queue.items.front() { Some(CllQueueItem::PendingTimingFrame {
/// .. }) => None, ... }` gate correctly identifies it as pending -- this
/// is a direct unit test on the same `CllEventQueue` state
/// `rpc_get_event_item` reads, without spinning up the full RPC handler.
#[tokio::test]
async fn a_pending_timing_frame_at_the_front_is_identified_as_not_yet_available() {
    let queue = Arc::new(Mutex::new(CllEventQueue {
        event_queue_cap: 16,
        ..CllEventQueue::default()
    }));

    let reservation_id = reserve_pending_timing_frame(&queue, 1, received_frame(5))
        .await
        .expect("push should succeed under this cap");

    let locked = queue.lock().await;
    assert!(
        matches!(
            locked.items.front(),
            Some(CllQueueItem::PendingTimingFrame { reservation_id: id, .. }) if *id == reservation_id
        ),
        "a still-pending reservation at the front must be identifiable as such, exactly \
             like rpc_get_event_item's own peek-before-pop gate requires"
    );
}
