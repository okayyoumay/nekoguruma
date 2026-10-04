use super::*;
use crate::service::ChannelProtocol;

const TEST_CLL: u32 = 1;

/// A `LogicalLinkState` with every field at its `CreateComLogicalLink`
/// default -- mirrors `registrant_lifecycle_tests::minimal_link`.
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

/// A created-receive-only (`NumSendCycles == 0`, `NumReceiveCycles ==
/// -1`) cyclic registrant -- the ONLY shape `is_eager_cyclic_deadline_
/// confirm_scope` accepts -- with `cyclic_deadline` at `deadline` and a
/// nonzero configured `CP_CyclicRespTimeout`.
fn cyclic_registrant(cop_handle: u32, deadline: tokio::time::Instant) -> CopRegistrant {
    CopRegistrant {
        cop_handle,
        registration_seq: 0,
        tier: RegistrantTier::ReceiveOnly,
        expected: Vec::new(),
        rc_cfg: None,
        request_sid: None,
        matches_needed: None,
        matches_got: 0,
        pending_rc: None,
        connect_generation: 7,
        cyclic_deadline: Some(deadline),
        cyclic_timeout_ms: Some(200),
        migrate_on_first_match: false,
        timing_cfg: None,
        timing_accumulator: None,
        pending_timing_change: None,
        concat_enabled: false,
        concat: Vec::new(),
        concat_segments_got: 0,
    }
}

// --- `is_eager_cyclic_deadline_confirm_scope` scoping tests ---

#[test]
fn scope_accepts_a_created_receive_only_registrant_with_nonzero_timeout() {
    let now = tokio::time::Instant::now();
    assert!(is_eager_cyclic_deadline_confirm_scope(&cyclic_registrant(
        100, now
    )));
}

/// Tier-1 (`ActiveSendReceive`) never has `cyclic_timeout_ms` set in
/// practice, but the scope check must reject it on `tier` alone even if
/// it somehow did -- defense-in-depth matching the predicate's own
/// `&&`.
#[test]
fn scope_rejects_tier_one_even_with_a_timeout_set() {
    let now = tokio::time::Instant::now();
    let r = CopRegistrant {
        tier: RegistrantTier::ActiveSendReceive,
        ..cyclic_registrant(100, now)
    };
    assert!(!is_eager_cyclic_deadline_confirm_scope(&r));
}

/// A migrated (S5) IS-CYCLIC COP, or any other tier-2 registrant that
/// never had `CP_CyclicRespTimeout` configured: `cyclic_timeout_ms` is
/// `None`.
#[test]
fn scope_rejects_tier_two_with_no_timeout_configured() {
    let now = tokio::time::Instant::now();
    let r = CopRegistrant {
        cyclic_timeout_ms: None,
        ..cyclic_registrant(100, now)
    };
    assert!(!is_eager_cyclic_deadline_confirm_scope(&r));
}

/// `CP_CyclicRespTimeout = 0` means "disabled" (ADR-100 Decision §4).
#[test]
fn scope_rejects_tier_two_with_a_zero_timeout() {
    let now = tokio::time::Instant::now();
    let r = CopRegistrant {
        cyclic_timeout_ms: Some(0),
        ..cyclic_registrant(100, now)
    };
    assert!(!is_eager_cyclic_deadline_confirm_scope(&r));
}

// --- `confirm_cyclic_deadline_writeback` mechanism tests ---

#[tokio::test]
async fn confirm_advances_a_stale_live_deadline_and_returns_true() {
    let now = tokio::time::Instant::now();
    let stale = now; // already expired by the time confirm runs
    let restarted = now + Duration::from_millis(500);

    let mut link = minimal_link();
    link.registrants.push(cyclic_registrant(100, stale));
    let mut links = HashMap::from([(TEST_CLL, link)]);

    let confirmed =
        confirm_cyclic_deadline_writeback(&mut links, TEST_CLL, 100, 7, Some(restarted));

    assert!(confirmed);
    assert_eq!(
        links.get(&TEST_CLL).unwrap().registrants[0].cyclic_deadline,
        Some(restarted)
    );
}

/// A missing live registrant (already reaped by a racing
/// `reap_expired_cyclic_registrants` call) -- `confirm` must not panic
/// and must report `false` so the caller discards the frame.
#[tokio::test]
async fn confirm_returns_false_when_the_registrant_was_already_reaped() {
    let mut links = HashMap::from([(TEST_CLL, minimal_link())]); // no registrants
    let now = tokio::time::Instant::now();

    let confirmed = confirm_cyclic_deadline_writeback(&mut links, TEST_CLL, 100, 7, Some(now));

    assert!(!confirmed);
}

/// A reconnect since this pass's snapshot changed `connect_generation`
/// -- same ADR-086 staleness guard the end-of-pass writeback loop
/// applies -- must not be merged into.
#[tokio::test]
async fn confirm_returns_false_on_a_connect_generation_mismatch() {
    let now = tokio::time::Instant::now();
    let mut link = minimal_link();
    link.registrants.push(cyclic_registrant(100, now)); // connect_generation: 7
    let mut links = HashMap::from([(TEST_CLL, link)]);

    let confirmed = confirm_cyclic_deadline_writeback(
        &mut links,
        TEST_CLL,
        100,
        8, // stale snapshot's generation
        Some(now + Duration::from_millis(500)),
    );

    assert!(!confirmed);
}

/// Edge-case-hunter review of this Decision §D fix: a `CancelComPrimitive`
/// already marked in `cancelled_cops` but not yet reaped by
/// `reap_cancelled_detached_registrants` must not be extended/confirmed
/// -- the registrant is still technically "found" in `registrants` at
/// this point, so without this check it would otherwise pass.
#[tokio::test]
async fn confirm_returns_false_when_a_cancel_is_already_marked() {
    let now = tokio::time::Instant::now();
    let mut link = minimal_link();
    link.registrants.push(cyclic_registrant(100, now));
    link.cancelled_cops.insert(100);
    let mut links = HashMap::from([(TEST_CLL, link)]);

    let confirmed = confirm_cyclic_deadline_writeback(
        &mut links,
        TEST_CLL,
        100,
        7,
        Some(now + Duration::from_millis(500)),
    );

    assert!(!confirmed);
    // The deadline must be left untouched, not just the return value.
    assert_eq!(
        links.get(&TEST_CLL).unwrap().registrants[0].cyclic_deadline,
        Some(now)
    );
}

/// The CLL itself was torn down mid-pass.
#[tokio::test]
async fn confirm_returns_false_when_the_cll_is_gone() {
    let mut links: HashMap<u32, LogicalLinkState> = HashMap::new();
    let now = tokio::time::Instant::now();

    let confirmed = confirm_cyclic_deadline_writeback(&mut links, TEST_CLL, 100, 7, Some(now));

    assert!(!confirmed);
}

// --- Race-closing regression test (ADR-101 Decision §D) ---

/// The actual race: a companion-channel poll pass bound a matching
/// frame to a tier-2 cyclic registrant and restarted its OWN
/// snapshot's `cyclic_deadline` (`restarted`, well in the future), but
/// the live registrant's `cyclic_deadline` is still the old, already-
/// expired one (`stale`) -- the end-of-pass writeback (`merge_
/// registrant_writeback`) has not landed yet. `is_cyclic_deadline_
/// expired` -- the exact predicate `reap_expired_cyclic_registrants`
/// evaluates -- reports the registrant expired BEFORE confirm runs, and
/// no longer expired AFTER confirm runs: proof that running the confirm
/// step is what closes the window, not merely the passage of time.
#[tokio::test]
async fn confirm_closes_the_reap_race_fail_without_pass_with() {
    let now = tokio::time::Instant::now();
    let stale = now.checked_sub(Duration::from_millis(50)).unwrap_or(now);
    let restarted = now + Duration::from_millis(500);

    let mut link = minimal_link();
    link.registrants.push(cyclic_registrant(100, stale));
    let mut links = HashMap::from([(TEST_CLL, link)]);

    // Fail-without: before the confirm step ever runs, the live
    // registrant's stale deadline is already expired -- a racing
    // `reap_expired_cyclic_registrants` tick right now would delete it.
    assert!(
        is_cyclic_deadline_expired(&links.get(&TEST_CLL).unwrap().registrants[0], now),
        "documents the bug ADR-101 Decision §D fixes: without the eager confirm step, the \
             live deadline is still stale and would be reaped despite the companion channel's \
             own snapshot having already restarted it"
    );

    // Run the fix.
    let confirmed =
        confirm_cyclic_deadline_writeback(&mut links, TEST_CLL, 100, 7, Some(restarted));
    assert!(confirmed);

    // Pass-with: the SAME predicate, evaluated against the SAME
    // registrant at the SAME instant `now`, now reports not-expired --
    // a racing reap tick would find it alive and skip it.
    assert!(
        !is_cyclic_deadline_expired(&links.get(&TEST_CLL).unwrap().registrants[0], now),
        "once confirm has run, the live deadline is extended and the registrant survives a \
             racing reap tick"
    );
}

/// Scoping control: a normal tier-1 (`ActiveSendReceive`) registrant's
/// delivery path must be completely unaffected by this step -- no
/// `logical_links` lock acquisition, no behavior change. Since
/// `poll_rx_inner` itself is not unit-testable here, this pins the
/// gating condition its `if` chain uses: `is_eager_cyclic_deadline_
/// confirm_scope` must return `false` for a tier-1 registrant, meaning
/// the `&&`-chain short-circuits before `confirm_cyclic_deadline_
/// writeback` -- and therefore before `ctx.logical_links.lock().await`
/// -- is ever reached, exactly like every other registrant shape this
/// step must leave alone.
#[test]
fn a_finite_count_tier_one_registrant_never_enters_confirm_scope() {
    let tier1 = CopRegistrant {
        cop_handle: 200,
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
    };
    assert!(
        !is_eager_cyclic_deadline_confirm_scope(&tier1),
        "a finite-count tier-1 registrant must never enter the eager confirm step -- it \
             never has cyclic_timeout_ms set"
    );
}
