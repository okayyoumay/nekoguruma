use std::collections::HashSet;

use super::*;
use crate::service::ChannelProtocol;

const TEST_CLL: u32 = 1;

/// A `LogicalLinkState` with every field at its `CreateComLogicalLink`
/// default (mirrors `rpc_link.rs`'s own constructor), for tests that only
/// care about `registrants`/`next_registrant_seq`.
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

/// A `CopRegistrant` for `cop_handle`, matching the defaults
/// `wait_for_expected_response` itself builds (tier 1, `Some(rc_cfg)`).
/// `registration_seq: 0` is a placeholder -- `insert_cop_registrant`
/// always overwrites it.
fn sample_registrant(cop_handle: u32) -> CopRegistrant {
    CopRegistrant {
        cop_handle,
        registration_seq: 0,
        tier: RegistrantTier::ActiveSendReceive,
        expected: Vec::new(),
        rc_cfg: Some(RcHandlingConfig::default()),
        request_sid: None,
        matches_needed: Some(1),
        matches_got: 0,
        pending_rc: None,
        connect_generation: 7,
        cyclic_deadline: None,
        cyclic_timeout_ms: None,
        migrate_on_first_match: false,
        timing_cfg: None,
        timing_accumulator: None,
        pending_timing_change: None,
        concat_enabled: false,
        concat: Vec::new(),
        concat_segments_got: 0,
    }
}

#[tokio::test]
async fn insert_assigns_ascending_registration_seq_and_updates_next_seq() {
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, minimal_link())])));

    insert_cop_registrant(&logical_links, TEST_CLL, sample_registrant(100)).await;
    insert_cop_registrant(&logical_links, TEST_CLL, sample_registrant(101)).await;

    let links = logical_links.lock().await;
    let link = links.get(&TEST_CLL).unwrap();
    assert_eq!(link.registrants.len(), 2);
    assert_eq!(link.registrants[0].cop_handle, 100);
    assert_eq!(link.registrants[0].registration_seq, 0);
    assert_eq!(link.registrants[1].cop_handle, 101);
    assert_eq!(link.registrants[1].registration_seq, 1);
    assert_eq!(link.next_registrant_seq, 2);
}

#[tokio::test]
async fn remove_deletes_only_the_matching_cop_handle() {
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, minimal_link())])));
    insert_cop_registrant(&logical_links, TEST_CLL, sample_registrant(100)).await;
    insert_cop_registrant(&logical_links, TEST_CLL, sample_registrant(101)).await;

    remove_cop_registrant(&logical_links, TEST_CLL, 100).await;

    let links = logical_links.lock().await;
    let link = links.get(&TEST_CLL).unwrap();
    assert_eq!(link.registrants.len(), 1);
    assert_eq!(link.registrants[0].cop_handle, 101);
}

/// ADR-148. Removing a
/// registrant mid-accumulation (an open `concat` buffer with segments
/// already absorbed) simply drops the buffer along with the rest of the
/// registrant -- no partial delivery is synthesized for it. This falls
/// out of `remove_cop_registrant`'s existing `retain` wholesale removal
/// with no extra logic; this test exists to pin that behavior for the
/// concat feature specifically.
#[tokio::test]
async fn remove_mid_accumulation_drops_the_open_concat_buffer_without_delivery() {
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, minimal_link())])));
    let mut registrant = sample_registrant(100);
    registrant.concat_enabled = true;
    registrant.concat = vec![ConcatBuf {
        key: (0, None, 0x62),
        timestamp: 0,
        header_bytes: Vec::new(),
        unique_resp_identifier: 0,
        acceptance_id: 0,
        rx_status_flags: 0,
        data: vec![0x62, 0xAA],
        footer_bytes: Vec::new(),
        opened_vacuous: false,
        segments: 1,
    }];
    registrant.concat_segments_got = 1;
    insert_cop_registrant(&logical_links, TEST_CLL, registrant).await;

    remove_cop_registrant(&logical_links, TEST_CLL, 100).await;

    let links = logical_links.lock().await;
    let link = links.get(&TEST_CLL).unwrap();
    assert!(
        link.registrants.is_empty(),
        "the registrant, including its open concat buffer, must be gone entirely -- no \
             partial delivery is synthesized for a cancelled mid-accumulation buffer"
    );
}

/// A missing `cll_handle` (torn down concurrently) is a silent no-op for
/// both insert and remove -- mirrors every other `logical_links.get_mut`
/// guard in this file.
#[tokio::test]
async fn insert_and_remove_are_noops_for_a_missing_cll_handle() {
    let logical_links: Arc<Mutex<HashMap<u32, LogicalLinkState>>> =
        Arc::new(Mutex::new(HashMap::new()));

    insert_cop_registrant(&logical_links, TEST_CLL, sample_registrant(100)).await;
    assert!(logical_links.lock().await.is_empty());

    // Does not panic even though TEST_CLL was never inserted.
    remove_cop_registrant(&logical_links, TEST_CLL, 100).await;
}

/// ADR-100: `cancel_link_cops` (CLL disconnect/destroy teardown) clears
/// every registrant for that CLL wholesale, not just the ones still in
/// `primitives` -- defense-in-depth against a still-in-flight receive
/// phase's own registrant lingering until its next per-pass staleness
/// check.
#[tokio::test]
async fn cancel_link_cops_clears_this_clls_registrants() {
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, minimal_link())])));
    insert_cop_registrant(&logical_links, TEST_CLL, sample_registrant(100)).await;
    assert_eq!(
        logical_links
            .lock()
            .await
            .get(&TEST_CLL)
            .unwrap()
            .registrants
            .len(),
        1
    );

    let primitives = Arc::new(Mutex::new(HashMap::new()));
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let terminal_cops = Arc::new(Mutex::new(TerminalCopsLedger::default()));
    cancel_link_cops(
        &primitives,
        &logical_links,
        &subscriptions,
        &terminal_cops,
        TEST_CLL,
    )
    .await;

    assert!(
        logical_links
            .lock()
            .await
            .get(&TEST_CLL)
            .unwrap()
            .registrants
            .is_empty(),
        "cancel_link_cops must clear registrants so a torn-down CLL does not leak them"
    );
}

/// ADR-146 (edge-case-hunter finding): `migrate_registrant_to_receive_only`
/// must clear `timing_cfg` the same way it already clears `rc_cfg` --
/// mirroring the fact that tier-2 (Receive Only) never runs the KWP
/// Access Timing live-exchange mechanism, same as it never runs RC
/// detection. Without this, an IS-CYCLIC registrant that migrates to
/// tier-2 after its first match would keep deriving and pushing
/// ComParam changes from every subsequent qualifying `0xC3` response for
/// the rest of its life.
#[tokio::test]
async fn migrate_to_receive_only_clears_timing_cfg_like_rc_cfg() {
    let mut registrant = sample_registrant(100);
    // Simulate the state `bind_registrant`'s own in-batch flip has
    // already produced by the time this function runs (its
    // `debug_assert_eq!` requires `tier == ReceiveOnly` on entry).
    registrant.tier = RegistrantTier::ReceiveOnly;
    registrant.timing_cfg = Some(TimingChangeConfig::KwpAccess(AccessTimingConfig {
        tpi: Some(2),
        tpi3_request_bytes: None,
        default_timing: None,
        override_timing: None,
        functional: false,
    }));
    let mut link = minimal_link();
    link.registrants.push(registrant);
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));

    migrate_registrant_to_receive_only(&logical_links, TEST_CLL, 100).await;

    let links = logical_links.lock().await;
    let live = &links.get(&TEST_CLL).unwrap().registrants[0];
    assert!(live.rc_cfg.is_none(), "rc_cfg must still be cleared");
    assert!(
        live.timing_cfg.is_none(),
        "timing_cfg must be cleared on migration to ReceiveOnly, same as rc_cfg"
    );
}

/// edge-case-hunter finding (round following the ADR-148 Amendment),
/// still pinned after the round-7 fix folded delivery into the same
/// `logical_links` acquisition as the guard+drain:
/// `finalize_and_deliver_concat_buffers_if_live` must reject a stale
/// caller BEFORE anything is drained or delivered. This test simulates
/// that staleness -- a `LogicalLinkState` whose live `connect_generation`
/// (5) no longer matches the caller's captured value (1), exactly as it
/// would right after a reconnect -- directly against the registrant with
/// 2 concurrently open concat buffers, and pins the contract: a stale
/// caller gets back `0` delivered, with the registrant's `matches_got`
/// and both open buffers completely untouched -- proving the real call
/// site's own `if delivered_count > 0 { matches_got += ...; if
/// matches_needed.is_some_and(...) { return CycleComplete } }` gate can
/// never fire for a guard failure.
#[tokio::test]
async fn finalize_and_deliver_concat_buffers_if_live_stale_generation_advances_nothing() {
    let mut registrant = sample_registrant(100);
    registrant.concat_enabled = true;
    registrant.matches_needed = Some(2);
    registrant.concat = vec![
        ConcatBuf {
            key: (1, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 1,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0x62, 0xAA, 0xBB],
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: 2,
        },
        ConcatBuf {
            key: (2, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 2,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0x62, 0xCC, 0xDD],
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: 2,
        },
    ];
    registrant.concat_segments_got = 4;
    let live_channel_id = ChannelId(9);
    let live_generation = 5; // the live link has already moved past the caller's generation
    let mut link = minimal_link();
    link.channel_id = Some(live_channel_id);
    link.connect_generation = live_generation;
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    insert_cop_registrant(&logical_links, TEST_CLL, registrant).await;
    let primitives = Arc::new(Mutex::new(HashMap::new()));

    // Caller captured generation 1 (e.g. at `wait_for_expected_response`
    // call time) -- stale relative to the live link's generation 5.
    let (delivered_count, _live_concat_segments_got) = finalize_and_deliver_concat_buffers_if_live(
        &logical_links,
        &primitives,
        TEST_CLL,
        live_channel_id,
        1,
        100,
    )
    .await;

    assert_eq!(
        delivered_count, 0,
        "a failed staleness guard must drain and deliver nothing at all"
    );
    let links = logical_links.lock().await;
    let live_registrant = &links.get(&TEST_CLL).unwrap().registrants[0];
    assert_eq!(
        live_registrant.matches_got, 0,
        "matches_got must NOT advance when the guard fails -- this is the exact bug the fix \
             closes: the old two-acquisition version committed matches_got before checking \
             staleness"
    );
    assert_eq!(
        live_registrant.concat.len(),
        2,
        "both buffers must remain open, untouched, ready for the NEXT pass's own per-pass \
             staleness check (which returns Terminal) or cancel_link_cops's teardown to reap \
             them -- never re-attempted from here"
    );
}

/// Companion positive-path test for the round-7 fix: proves that when
/// the ADR-086 staleness guard HOLDS, `finalize_and_deliver_concat_buffers_if_live`
/// drains, commits `matches_got`, AND delivers every finalized buffer
/// into the link's `rx_buf` queue, all within the one `logical_links`
/// acquisition -- not just that a stale guard is inert (the sibling
/// `..._stale_generation_advances_nothing` test above). Also pins the
/// round-9/ADR-148 Amendment 8 second-fix contract: the registrant's
/// `concat_segments_got` here (4, from two 2-segment buffers) is
/// deliberately NOT equal to `delivered_count` (2, one per buffer) --
/// proving the returned `live_concat_segments_got` is the registrant's
/// own absolute segment total, not a value derivable from
/// `delivered_count` alone.
#[tokio::test]
async fn finalize_and_deliver_concat_buffers_if_live_delivers_and_advances_when_guard_holds() {
    let mut registrant = sample_registrant(100);
    registrant.concat_enabled = true;
    registrant.matches_needed = Some(2);
    registrant.concat = vec![
        ConcatBuf {
            key: (1, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 1,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0x62, 0xAA, 0xBB],
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: 2,
        },
        ConcatBuf {
            key: (2, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 2,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0x62, 0xCC, 0xDD],
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: 2,
        },
    ];
    registrant.concat_segments_got = 4;
    let live_channel_id = ChannelId(9);
    let live_generation = 5;
    let mut link = minimal_link();
    link.channel_id = Some(live_channel_id);
    link.connect_generation = live_generation;
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    insert_cop_registrant(&logical_links, TEST_CLL, registrant).await;
    // ADR-205 follow-up (design-advisor review): `finalize_and_deliver_
    // concat_buffers_if_live` now bails (no delivery) when `cop_handle`
    // is absent from `primitives` -- an absent entry means a concurrent
    // cancel/teardown already claimed this COP. This test's whole point is
    // the OPPOSITE case (a genuinely live, un-cancelled COP), so it needs a
    // matching `primitives` entry for cop_handle 100, same as any real
    // in-flight COP would have.
    let primitives = Arc::new(Mutex::new(HashMap::from([(
        100,
        CopEntry {
            cll_handle: TEST_CLL,
            dispatched: true,
            transmits: false,
            is_send_recv: true,
            cop_tag: None,
        },
    )])));

    // Caller's captured channel_id/generation matches the live link --
    // the guard holds.
    let (delivered_count, live_concat_segments_got) = finalize_and_deliver_concat_buffers_if_live(
        &logical_links,
        &primitives,
        TEST_CLL,
        live_channel_id,
        live_generation,
        100,
    )
    .await;

    assert_eq!(
        delivered_count, 2,
        "both open buffers must be finalized and delivered in one pass"
    );
    assert_eq!(
        live_concat_segments_got, 4,
        "the returned live segment count must be the registrant's own absolute \
             concat_segments_got (4, from two 2-segment buffers), not derived from \
             delivered_count (2) -- ADR-148 Amendment 8's second fix"
    );

    let links = logical_links.lock().await;
    let link = links.get(&TEST_CLL).unwrap();
    let live_registrant = &link.registrants[0];
    assert_eq!(
        live_registrant.matches_got, 2,
        "matches_got must advance once per delivered buffer"
    );
    assert!(
        live_registrant.concat.is_empty(),
        "both buffers must be drained from the registrant"
    );

    let rx_buf = link.rx_buf.clone();
    drop(links);
    let queue = rx_buf.lock().await;
    assert_eq!(
        queue.items.len(),
        2,
        "both finalized buffers must be delivered into the link's rx_buf queue"
    );
    assert!(
        queue
            .items
            .iter()
            .all(|item| matches!(item, CllQueueItem::Frame(_))),
        "each delivered item must be a CllQueueItem::Frame"
    );
}

/// ADR-205 follow-up (edge-case-hunter finding, round 5): pins the fix's own
/// new behavior -- the `channel_id`/`connect_generation` guard HOLDS (unlike
/// `..._stale_generation_advances_nothing` above) but `cop_handle` is absent
/// from `primitives` (a concurrent cancel/teardown already claimed this COP).
/// The function must bail with `(0, 0)` WITHOUT ever draining the
/// registrant's open concat buffer or bumping `matches_got` -- round 5's
/// first attempt at this fix checked `primitives` liveness only AFTER
/// `finalize_concat_buffers` had already drained `r` and bumped
/// `matches_got`, silently discarding the buffered ECU response while still
/// reporting it as delivered. Mirrors
/// `finalize_and_deliver_concat_buffers_if_live_stale_generation_advances_nothing`'s
/// structure, but for the `primitives`-absence guard instead of the
/// channel_id/generation one.
#[tokio::test]
async fn finalize_and_deliver_concat_buffers_if_live_bails_when_primitives_entry_is_absent() {
    let mut registrant = sample_registrant(100);
    registrant.concat_enabled = true;
    registrant.matches_needed = Some(1);
    registrant.concat = vec![ConcatBuf {
        key: (1, None, 0x62),
        timestamp: 0,
        header_bytes: Vec::new(),
        unique_resp_identifier: 1,
        acceptance_id: 77,
        rx_status_flags: 0,
        data: vec![0x62, 0xAA, 0xBB],
        footer_bytes: Vec::new(),
        opened_vacuous: false,
        segments: 1,
    }];
    registrant.concat_segments_got = 1;
    let live_channel_id = ChannelId(9);
    let live_generation = 5;
    let mut link = minimal_link();
    link.channel_id = Some(live_channel_id);
    link.connect_generation = live_generation;
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    insert_cop_registrant(&logical_links, TEST_CLL, registrant).await;
    // No entry for cop_handle 100 -- simulates a concurrent cancel/teardown
    // that already removed it from `primitives`.
    let primitives = Arc::new(Mutex::new(HashMap::new()));

    let (delivered_count, live_concat_segments_got) = finalize_and_deliver_concat_buffers_if_live(
        &logical_links,
        &primitives,
        TEST_CLL,
        live_channel_id,
        live_generation,
        100,
    )
    .await;

    assert_eq!(
        delivered_count, 0,
        "an absent primitives entry must bail with zero delivered, even though the \
             channel_id/connect_generation guard holds"
    );
    assert_eq!(
        live_concat_segments_got, 0,
        "the bail path's second return value carries no meaning and must not be \
             read from a partially-drained registrant"
    );

    let links = logical_links.lock().await;
    let link = links.get(&TEST_CLL).unwrap();
    let live_registrant = &link.registrants[0];
    assert_eq!(
        live_registrant.matches_got, 0,
        "matches_got must NOT advance -- the buffer must never be drained on this path"
    );
    assert_eq!(
        live_registrant.concat.len(),
        1,
        "the open concat buffer must be left exactly as-is, not drained and discarded"
    );

    let rx_buf = link.rx_buf.clone();
    drop(links);
    let queue = rx_buf.lock().await;
    assert!(
        queue.items.is_empty(),
        "nothing may be delivered into rx_buf when primitives is absent"
    );
}

/// edge-case-hunter finding (round-7 review of the delivery-folding fix):
/// the guard-holds case has two distinct `0`-returning shapes --
/// `finalize_and_deliver_concat_buffers_if_live`'s own doc comment
/// distinguishes "guard failed" (previous test) from "guard held, but
/// the registrant had no open buffers at all" (e.g. a second
/// deadline-expiry pass after an earlier pass already drained
/// everything). Only the guard-failure shape had a test; this pins the
/// second one -- a live, matching `channel_id`/`connect_generation`
/// against a registrant with an empty `concat` `Vec` must still return
/// `0`, leaving `matches_got` and the `rx_buf` queue untouched, exactly
/// like a guard failure from the caller's point of view (both read as
/// "nothing to advance on this pass").
#[tokio::test]
async fn finalize_and_deliver_concat_buffers_if_live_guard_holds_no_open_buffers() {
    let mut registrant = sample_registrant(100);
    registrant.concat_enabled = true;
    registrant.matches_needed = Some(2);
    registrant.concat = Vec::new();
    registrant.concat_segments_got = 0;
    let live_channel_id = ChannelId(9);
    let live_generation = 5;
    let mut link = minimal_link();
    link.channel_id = Some(live_channel_id);
    link.connect_generation = live_generation;
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    insert_cop_registrant(&logical_links, TEST_CLL, registrant).await;
    // ADR-205 follow-up (design-advisor review): a genuinely live, matching
    // `primitives` entry, same as the sibling `_delivers_and_advances_when_
    // guard_holds` test above -- this test's "guard holds" premise means
    // this COP is NOT concurrently cancelled, so it must have one.
    let primitives = Arc::new(Mutex::new(HashMap::from([(
        100,
        CopEntry {
            cll_handle: TEST_CLL,
            dispatched: true,
            transmits: false,
            is_send_recv: true,
            cop_tag: None,
        },
    )])));

    let (delivered_count, _live_concat_segments_got) = finalize_and_deliver_concat_buffers_if_live(
        &logical_links,
        &primitives,
        TEST_CLL,
        live_channel_id,
        live_generation,
        100,
    )
    .await;

    assert_eq!(
        delivered_count, 0,
        "a held guard with no open buffers must still report 0 delivered"
    );

    let links = logical_links.lock().await;
    let link = links.get(&TEST_CLL).unwrap();
    assert_eq!(
        link.registrants[0].matches_got, 0,
        "matches_got must not advance when nothing was open to deliver"
    );
    let rx_buf = link.rx_buf.clone();
    drop(links);
    let queue = rx_buf.lock().await;
    assert!(
        queue.items.is_empty(),
        "no item should be delivered into rx_buf when nothing was open"
    );
}

/// ADR-148 Amendment 10 (Codex round-14 finding on PR #18): when the
/// ADR-086 liveness guard holds, `discard_concat_buffers_for_retransmit_if_live`
/// must clear every open concat buffer on the registrant -- but leave
/// `matches_got`/`concat_segments_got` completely untouched, since those
/// fields' monotone-baseline diff contract (`check_match_against_baseline`)
/// depends on `concat_segments_got` never decreasing, and the wait loop's
/// own local `concat_segments_got` variable already reflects everything
/// absorbed so far regardless of whether the buffer contents survive.
#[tokio::test]
async fn discard_concat_buffers_for_retransmit_if_live_clears_open_buffers_when_guard_holds() {
    let mut registrant = sample_registrant(100);
    registrant.concat_enabled = true;
    registrant.matches_needed = Some(2);
    registrant.matches_got = 1;
    registrant.concat = vec![
        ConcatBuf {
            key: (1, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 1,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0x62, 0xAA, 0xBB],
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: 2,
        },
        ConcatBuf {
            key: (2, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 2,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0x62, 0xCC, 0xDD],
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: 2,
        },
    ];
    registrant.concat_segments_got = 4;
    let live_channel_id = ChannelId(9);
    let live_generation = 5;
    let mut link = minimal_link();
    link.channel_id = Some(live_channel_id);
    link.connect_generation = live_generation;
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    insert_cop_registrant(&logical_links, TEST_CLL, registrant).await;

    let guard_held = discard_concat_buffers_for_retransmit_if_live(
        &logical_links,
        TEST_CLL,
        live_channel_id,
        live_generation,
        100,
    )
    .await;

    assert!(
        guard_held,
        "a live channel_id/connect_generation must hold the guard"
    );

    let links = logical_links.lock().await;
    let live_registrant = &links.get(&TEST_CLL).unwrap().registrants[0];
    assert!(
        live_registrant.concat.is_empty(),
        "both open buffers must be discarded when the guard holds"
    );
    assert_eq!(
        live_registrant.matches_got, 1,
        "matches_got must be completely unaffected by a discard -- this is not a delivery"
    );
    assert_eq!(
        live_registrant.concat_segments_got, 4,
        "concat_segments_got must be completely unaffected by a discard -- the wait loop's \
             own local baseline already reflects everything absorbed so far"
    );
}

/// Mirrors `finalize_and_deliver_concat_buffers_if_live_stale_generation_advances_nothing`'s
/// own staleness contract: a caller whose captured `connect_generation`
/// no longer matches the live link must get `false` back, with every
/// open buffer left completely untouched -- the discard must never fire
/// on a stale caller (that caller's retransmit doesn't happen either, so
/// there is no attempt boundary to protect against).
#[tokio::test]
async fn discard_concat_buffers_for_retransmit_if_live_stale_generation_leaves_buffers_untouched() {
    let mut registrant = sample_registrant(100);
    registrant.concat_enabled = true;
    registrant.matches_needed = Some(2);
    registrant.concat = vec![ConcatBuf {
        key: (1, None, 0x62),
        timestamp: 0,
        header_bytes: Vec::new(),
        unique_resp_identifier: 1,
        acceptance_id: 77,
        rx_status_flags: 0,
        data: vec![0x62, 0xAA, 0xBB],
        footer_bytes: Vec::new(),
        opened_vacuous: false,
        segments: 2,
    }];
    registrant.concat_segments_got = 2;
    let live_channel_id = ChannelId(9);
    let live_generation = 5; // the live link has already moved past the caller's generation
    let mut link = minimal_link();
    link.channel_id = Some(live_channel_id);
    link.connect_generation = live_generation;
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    insert_cop_registrant(&logical_links, TEST_CLL, registrant).await;

    // Caller captured generation 1 -- stale relative to the live link's
    // generation 5.
    let guard_held = discard_concat_buffers_for_retransmit_if_live(
        &logical_links,
        TEST_CLL,
        live_channel_id,
        1,
        100,
    )
    .await;

    assert!(!guard_held, "a stale caller must not hold the guard");

    let links = logical_links.lock().await;
    let live_registrant = &links.get(&TEST_CLL).unwrap().registrants[0];
    assert_eq!(
        live_registrant.concat.len(),
        1,
        "the open buffer must survive untouched when the guard fails"
    );
    assert_eq!(
        live_registrant.concat[0].data,
        vec![0x62, 0xAA, 0xBB],
        "buffer contents must be exactly as before -- no partial discard on guard failure"
    );
}

/// Boundary case, matching this PR's own established precedent (e.g.
/// `finalize_and_deliver_concat_buffers_if_live_guard_holds_no_open_buffers`):
/// the guard holds but the registrant's `concat` is already empty --
/// this is a trivial no-op and must not panic or otherwise misbehave.
#[tokio::test]
async fn discard_concat_buffers_for_retransmit_if_live_guard_holds_no_open_buffers() {
    let mut registrant = sample_registrant(100);
    registrant.concat_enabled = true;
    registrant.matches_needed = Some(2);
    registrant.concat = Vec::new();
    registrant.concat_segments_got = 0;
    let live_channel_id = ChannelId(9);
    let live_generation = 5;
    let mut link = minimal_link();
    link.channel_id = Some(live_channel_id);
    link.connect_generation = live_generation;
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    insert_cop_registrant(&logical_links, TEST_CLL, registrant).await;

    let guard_held = discard_concat_buffers_for_retransmit_if_live(
        &logical_links,
        TEST_CLL,
        live_channel_id,
        live_generation,
        100,
    )
    .await;

    assert!(
        guard_held,
        "the guard must still hold even with nothing open to discard"
    );

    let links = logical_links.lock().await;
    let live_registrant = &links.get(&TEST_CLL).unwrap().registrants[0];
    assert!(
        live_registrant.concat.is_empty(),
        "concat must remain empty -- a no-op, not a panic"
    );
}

/// A sample `ConcatDelivery`, distinguishable via `unique_resp_identifier`
/// and `acceptance_id` so delivery order can be asserted precisely.
fn sample_concat_delivery(unique_resp_identifier: u32, acceptance_id: u32) -> ConcatDelivery {
    ConcatDelivery {
        cop_handle: 100,
        acceptance_id,
        timestamp: 0,
        header_bytes: Vec::new(),
        footer_bytes: Vec::new(),
        unique_resp_identifier,
        rx_status_flags: 0,
        data: vec![0x62, 0xAA, 0xBB],
    }
}

/// ADR-148 Amendment 9: a batch drained from a per-poll-pass snapshot by
/// `bind_registrant`'s inline force-finalize triggers must be dropped in
/// its entirety -- not partially delivered -- when the live link's
/// `channel_id`/`connect_generation` no longer match what the caller
/// captured at snapshot time (e.g. a `DisconnectComLogicalLink` landed
/// mid-poll-pass).
#[tokio::test]
async fn deliver_concat_batch_if_live_stale_generation_drops_whole_batch() {
    let live_channel_id = ChannelId(9);
    let live_generation = 5; // the live link has already moved past the caller's generation
    let mut link = minimal_link();
    link.channel_id = Some(live_channel_id);
    link.connect_generation = live_generation;
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    let cop_tags: HashMap<u32, Vec<u8>> = HashMap::new();

    let batch = vec![sample_concat_delivery(1, 77), sample_concat_delivery(2, 78)];

    // Caller captured generation 1 -- stale relative to the live link's
    // generation 5.
    deliver_concat_batch_if_live(
        &logical_links,
        &cop_tags,
        TEST_CLL,
        live_channel_id,
        1,
        batch,
    )
    .await;

    let links = logical_links.lock().await;
    let link = links.get(&TEST_CLL).unwrap();
    let rx_buf = link.rx_buf.clone();
    drop(links);
    let queue = rx_buf.lock().await;
    assert!(
        queue.items.is_empty(),
        "a failed staleness guard must drop the whole batch, delivering nothing"
    );
}

/// ADR-148 Amendment 9: when the caller's captured `channel_id`/
/// `connect_generation` still match the live link, every entry in the
/// batch is delivered, in the same order the batch was given (first-
/// opened-first, matching `bind_registrant`'s own accumulation order).
#[tokio::test]
async fn deliver_concat_batch_if_live_delivers_all_entries_in_order_when_guard_holds() {
    let live_channel_id = ChannelId(9);
    let live_generation = 5;
    let mut link = minimal_link();
    link.channel_id = Some(live_channel_id);
    link.connect_generation = live_generation;
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    // ADR-204 Codex review (PR #116, round 3): both entries share
    // `sample_concat_delivery`'s fixed `cop_handle: 100`, so a single
    // `cop_tags` entry covers the whole batch -- proves
    // `deliver_concat_batch_if_live` reads the caller's own per-pass
    // snapshot (this map), not a fresh `primitives` lookup, for the tag it
    // stamps onto each delivered frame.
    let cop_tags: HashMap<u32, Vec<u8>> = HashMap::from([(100, b"concat-tag".to_vec())]);

    let batch = vec![sample_concat_delivery(1, 77), sample_concat_delivery(2, 78)];

    deliver_concat_batch_if_live(
        &logical_links,
        &cop_tags,
        TEST_CLL,
        live_channel_id,
        live_generation,
        batch,
    )
    .await;

    let links = logical_links.lock().await;
    let link = links.get(&TEST_CLL).unwrap();
    let rx_buf = link.rx_buf.clone();
    drop(links);
    let queue = rx_buf.lock().await;
    assert_eq!(
        queue.items.len(),
        2,
        "both batch entries must be delivered when the guard holds"
    );
    let (first_uri, first_acc, first_tag) = match &queue.items[0] {
        CllQueueItem::Frame(f) => (f.unique_resp_identifier, f.acceptance_id, f.cop_tag.clone()),
        other => panic!("expected Frame item, got {other:?}"),
    };
    let (second_uri, second_acc, second_tag) = match &queue.items[1] {
        CllQueueItem::Frame(f) => (f.unique_resp_identifier, f.acceptance_id, f.cop_tag.clone()),
        other => panic!("expected Frame item, got {other:?}"),
    };
    assert_eq!(
        (first_uri, first_acc),
        (1, 77),
        "first batch entry must be delivered first"
    );
    assert_eq!(
        (second_uri, second_acc),
        (2, 78),
        "second batch entry must be delivered second, preserving batch order"
    );
    assert_eq!(
        first_tag,
        Some(b"concat-tag".to_vec()),
        "cop_tag must be stamped from the caller's cop_tags snapshot"
    );
    assert_eq!(
        second_tag,
        Some(b"concat-tag".to_vec()),
        "cop_tag must be stamped from the caller's cop_tags snapshot for every entry sharing \
         that cop_handle"
    );
}
