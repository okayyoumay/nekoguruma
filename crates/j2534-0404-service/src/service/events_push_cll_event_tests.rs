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

fn timestamps(buf: &VecDeque<CllQueueItem>) -> Vec<u32> {
    buf.iter()
        .map(|item| match item {
            CllQueueItem::Frame(f) => f.timestamp,
            CllQueueItem::Error(e) => e.timestamp,
            CllQueueItem::Status(s) => s.timestamp,
            CllQueueItem::PendingTimingFrame { frame, .. } => frame.timestamp,
        })
        .collect()
}

fn cll_status(timestamp: u32) -> CllQueueItem {
    CllQueueItem::Status(TrackedStatus {
        event: StatusEvent::Cll(PduComLogicalLinkStatus::PduCllstOnline),
        timestamp,
    })
}

fn cop_status(timestamp: u32, cop_handle: u32) -> CllQueueItem {
    CllQueueItem::Status(TrackedStatus {
        event: StatusEvent::Cop {
            cop_handle,
            status: PduComPrimitiveStatus::PduCopstFinished,
            cop_tag: None,
        },
        timestamp,
    })
}

#[test]
fn overwrite_oldest_pops_the_front_to_make_room() {
    let mut buf = VecDeque::new();
    for ts in 1..=3 {
        // Under cap: never reports a drop, always inserted.
        assert_eq!(
            push_cll_event(&mut buf, 3, EventQueueMode::OverwriteOldest, frame(ts)),
            PushOutcome::Inserted
        );
    }
    assert_eq!(timestamps(&buf), vec![1, 2, 3]);

    // At capacity: the oldest (1) is evicted to make room for 4, and the
    // eviction is reported as `Evicted` (ADR-115 corrected rule: drives
    // both the PDU_EVT_DATA_LOST notification AND the frame's own
    // notification, since the frame itself was inserted).
    assert_eq!(
        push_cll_event(&mut buf, 3, EventQueueMode::OverwriteOldest, frame(4)),
        PushOutcome::Evicted
    );
    assert_eq!(timestamps(&buf), vec![2, 3, 4]);
}

#[test]
fn discard_newest_drops_the_incoming_frame_once_at_capacity() {
    let mut buf = VecDeque::new();
    for ts in 1..=3 {
        assert_eq!(
            push_cll_event(&mut buf, 3, EventQueueMode::DiscardNewest, frame(ts)),
            PushOutcome::Inserted
        );
    }
    assert_eq!(timestamps(&buf), vec![1, 2, 3]);

    // At capacity: frame 4 is dropped entirely, not inserted, and the
    // drop is reported as `Discarded` (ADR-115 corrected rule: drives
    // only the PDU_EVT_DATA_LOST notification -- frame 4's own
    // notification must never be sent, since it was never inserted).
    assert_eq!(
        push_cll_event(&mut buf, 3, EventQueueMode::DiscardNewest, frame(4)),
        PushOutcome::Discarded
    );
    assert_eq!(timestamps(&buf), vec![1, 2, 3]);
}

/// Switching a CLL's mode (as `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`
/// would) changes eviction behavior for frames pushed after the switch,
/// without needing to rebuild the buffer. Both modes report a drop
/// (`PushOutcome::lost()`), but only `OverwriteOldest` also inserts the
/// new frame (ADR-115 corrected rule).
#[test]
fn mode_switch_changes_behavior_for_subsequent_pushes() {
    let mut buf = VecDeque::new();
    for ts in 1..=2 {
        assert_eq!(
            push_cll_event(&mut buf, 2, EventQueueMode::OverwriteOldest, frame(ts)),
            PushOutcome::Inserted
        );
    }
    assert_eq!(timestamps(&buf), vec![1, 2]);

    // Switch to DiscardNewest: the next push at capacity is dropped and
    // not inserted.
    assert_eq!(
        push_cll_event(&mut buf, 2, EventQueueMode::DiscardNewest, frame(3)),
        PushOutcome::Discarded
    );
    assert_eq!(timestamps(&buf), vec![1, 2]);

    // Switch back to OverwriteOldest: the next push evicts the front and
    // inserts.
    assert_eq!(
        push_cll_event(&mut buf, 2, EventQueueMode::OverwriteOldest, frame(4)),
        PushOutcome::Evicted
    );
    assert_eq!(timestamps(&buf), vec![2, 4]);
}

/// Frame and Error entries share one true-arrival-order FIFO (ADR-105
/// revision) -- an Error pushed between two Frames stays between them,
/// and participates in the same eviction policy as any other entry.
#[test]
fn frame_and_error_entries_share_one_fifo_in_arrival_order() {
    let mut buf = VecDeque::new();
    push_cll_event(&mut buf, 3, EventQueueMode::OverwriteOldest, frame(1));
    push_cll_event(
        &mut buf,
        3,
        EventQueueMode::OverwriteOldest,
        CllQueueItem::Error(TrackedError {
            event: PduErrorEvent::PduErrEvtLostCommToVci,
            timestamp: 2,
            cop: None,
            cop_tag: None,
        }),
    );
    push_cll_event(&mut buf, 3, EventQueueMode::OverwriteOldest, frame(3));

    let kinds: Vec<&str> = buf
        .iter()
        .map(|item| match item {
            CllQueueItem::Frame(_) => "frame",
            CllQueueItem::Error(_) => "error",
            CllQueueItem::Status(_) => "status",
            CllQueueItem::PendingTimingFrame { .. } => "pending_timing_frame",
        })
        .collect();
    assert_eq!(kinds, vec!["frame", "error", "frame"]);
    assert_eq!(timestamps(&buf), vec![1, 2, 3]);
}

/// ADR-105 P2 follow-up: `Status` (CLL/COP status transitions) shares the
/// exact same true-arrival-order FIFO as `Frame`/`Error`, interleaving
/// correctly among them -- extending
/// `frame_and_error_entries_share_one_fifo_in_arrival_order` to the third
/// item kind. Covers both `StatusEvent` shapes (`Cll` and `Cop`) so the
/// not-yet-production-constructed `Cop` arm is still exercised end to
/// end (see `StatusEvent::Cop`'s own doc comment, `service.rs`).
#[test]
fn status_entries_share_one_fifo_with_frame_and_error_in_arrival_order() {
    let mut buf = VecDeque::new();
    push_cll_event(&mut buf, 4, EventQueueMode::OverwriteOldest, frame(1));
    push_cll_event(&mut buf, 4, EventQueueMode::OverwriteOldest, cll_status(2));
    push_cll_event(
        &mut buf,
        4,
        EventQueueMode::OverwriteOldest,
        CllQueueItem::Error(TrackedError {
            event: PduErrorEvent::PduErrEvtLostCommToVci,
            timestamp: 3,
            cop: None,
            cop_tag: None,
        }),
    );
    push_cll_event(
        &mut buf,
        4,
        EventQueueMode::OverwriteOldest,
        cop_status(4, 7),
    );

    let kinds: Vec<&str> = buf
        .iter()
        .map(|item| match item {
            CllQueueItem::Frame(_) => "frame",
            CllQueueItem::Error(_) => "error",
            CllQueueItem::Status(_) => "status",
            CllQueueItem::PendingTimingFrame { .. } => "pending_timing_frame",
        })
        .collect();
    assert_eq!(kinds, vec!["frame", "status", "error", "status"]);
    assert_eq!(timestamps(&buf), vec![1, 2, 3, 4]);
}

/// `Status` entries participate in `OverwriteOldest` eviction exactly
/// like `Frame`/`Error`: at capacity, the oldest entry (regardless of
/// kind) is evicted to make room, and the eviction is reported.
#[test]
fn status_entry_evicts_the_oldest_entry_under_overwrite_oldest() {
    let mut buf = VecDeque::new();
    push_cll_event(&mut buf, 2, EventQueueMode::OverwriteOldest, frame(1));
    push_cll_event(
        &mut buf,
        2,
        EventQueueMode::OverwriteOldest,
        CllQueueItem::Error(TrackedError {
            event: PduErrorEvent::PduErrEvtLostCommToVci,
            timestamp: 2,
            cop: None,
            cop_tag: None,
        }),
    );
    assert_eq!(timestamps(&buf), vec![1, 2]);

    // At capacity: pushing a Status entry evicts the oldest (the Frame),
    // not just other Status entries.
    assert_eq!(
        push_cll_event(&mut buf, 2, EventQueueMode::OverwriteOldest, cll_status(3)),
        PushOutcome::Evicted
    );
    assert_eq!(timestamps(&buf), vec![2, 3]);
}

/// `Status` entries participate in `DiscardNewest` exactly like
/// `Frame`/`Error`: at capacity, an incoming `Status` entry is dropped
/// entirely rather than displacing an older `Frame`/`Error` entry.
#[test]
fn status_entry_is_discarded_under_discard_newest_at_capacity() {
    let mut buf = VecDeque::new();
    push_cll_event(&mut buf, 2, EventQueueMode::DiscardNewest, frame(1));
    push_cll_event(&mut buf, 2, EventQueueMode::DiscardNewest, cll_status(2));
    assert_eq!(timestamps(&buf), vec![1, 2]);

    assert_eq!(
        push_cll_event(&mut buf, 2, EventQueueMode::DiscardNewest, cop_status(3, 9)),
        PushOutcome::Discarded
    );
    assert_eq!(timestamps(&buf), vec![1, 2]);
}
