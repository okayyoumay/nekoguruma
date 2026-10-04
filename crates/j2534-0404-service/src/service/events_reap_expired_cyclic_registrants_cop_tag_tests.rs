//! ADR-204 follow-up (Codex review, PR #116; edge-case-hunter verification,
//! 2026-08-31): `tests/grpc_mock/cop_tag.rs`'s own `cop_tag_is_attributed_
//! correctly_when_multiple_cyclic_cops_are_reaped_concurrently` was
//! empirically proven not to discriminate the fix it claims to cover --
//! reverting `reap_expired_cyclic_registrants`'s one-line wiring change
//! (`send_error_event_with_tag(..., reaped.cop_tag.clone())` back to the old
//! fresh-lookup `send_error_event(...)`) left it passing, since that test
//! never actually removes a COP's `primitives` entry between collection and
//! emission (see that test's own corrected doc comment). This file provides
//! the test that DOES discriminate, driving `reap_expired_cyclic_
//! registrants` itself directly (not through `tests/grpc_mock`, which has no
//! hook to remove a `primitives` entry independently of a full COP
//! lifecycle -- `harness.rs`'s backdoors only reach the MOCK native
//! library's process-global state, never this service's own internal
//! `logical_links`/`primitives` maps).
//!
//! **Why this can be sequenced deterministically, unlike the genuinely
//! unforceable window `receive_only_finite_n_cancel_races_cyclic_timeout_
//! expiry_stays_cancelled`'s own doc comment (`tests/grpc_mock/
//! cop_ctrl_cycles.rs`) documents for its own, different same-tick race:**
//! that other race needs an external `CancelComPrimitive` to land inside a
//! gap between two SEPARATE ticks (`reap_cancelled_detached_registrants`
//! (S5) then `reap_expired_cyclic_registrants` (S6) in the same tick), with
//! no reusable contention point to force it -- its own doc comment and
//! `events_drain_cancelled_cop_if_finalized_tests.rs`'s `batch_drain_loop_
//! drains_only_the_stranded_marks_in_a_mixed_batch` doc comment both
//! independently reach that same "not worth the invasiveness" conclusion
//! for their own analogous races. This one is different in kind: it is a
//! gap WITHIN a single `reap_expired_cyclic_registrants` call, between
//! releasing `primitives` at the end of its own collection critical section
//! and re-acquiring it moments later (inside `send_error_event`'s `resolve_
//! cop_tag`, if reverted, or `emit_terminal_if_live`, if fixed) --
//! `tokio::sync::Mutex`'s documented FIFO-fair semaphore semantics make that
//! gap forceable with no real concurrency or timing luck: holding
//! `primitives` before the sweep is ever polled forces its collection to
//! queue as the FIRST waiter; releasing it, then immediately re-queuing for
//! the SAME lock from this test's own task, guarantees (FIFO) the sweep's
//! already-queued collection acquisition is served first, and this test's
//! new request is served immediately after -- strictly before the sweep's
//! own LATER re-acquisition, which has not even been requested yet at that
//! point. `hard_channel_error_waits_for_the_fence_before_reporting_the_
//! swept_cop_terminal` (`events_hard_error_broadcast_periodic_tests.rs`)
//! already establishes this crate's `current_thread` test runtime as
//! deterministic for exactly this class of lock-ordering assertion.
//!
//! Verified fail-without/pass-with directly (see this file's own test doc
//! comment below for the exact revert used and the observed outcome).

use super::*;
use crate::service::ChannelProtocol;

const RC_TEST_CHANNEL_ID: ChannelId = ChannelId(4242);
const RC_TEST_CLL: u32 = 1;
const RC_TEST_COP_HANDLE: u32 = 9001;

/// A finite-`N` (`matches_needed: Some(2)`) created-receive-only registrant
/// with an already-expired `cyclic_deadline` -- the same shape `reap_
/// expired_cyclic_decision_tests::finite_n_registrant` builds, duplicated
/// here rather than shared since that helper is private to its own test
/// module.
fn expired_finite_n_registrant(deadline: tokio::time::Instant) -> CopRegistrant {
    CopRegistrant {
        cop_handle: RC_TEST_COP_HANDLE,
        registration_seq: 0,
        tier: RegistrantTier::ReceiveOnly,
        expected: Vec::new(),
        rc_cfg: None,
        request_sid: None,
        matches_needed: Some(2),
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

/// Mirrors `events_drain_cancelled_cop_if_finalized_tests::minimal_link`'s
/// own field-for-field default shape, connected on `RC_TEST_CHANNEL_ID` (so
/// `reap_expired_cyclic_registrants`'s own `link.channel_id != Some(ctx.
/// channel_id)` filter selects it) and carrying one already-expired finite-N
/// registrant.
fn link_with_the_expired_registrant(deadline: tokio::time::Instant) -> LogicalLinkState {
    LogicalLinkState {
        channel_id: Some(RC_TEST_CHANNEL_ID),
        protocol: ChannelProtocol::CAN,
        hw_protocol_id: 0,
        software_isotp: false,
        uudt_channel_id: None,
        uudt_channel_key: None,
        isotp_rx: Arc::new(Mutex::new(HashMap::new())),
        connect_in_flight: std::sync::Weak::new(),
        connected: true,
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
        registrants: vec![expired_finite_n_registrant(deadline)],
        next_registrant_seq: 1,
        j1939_claimed_address: None,
        j1939_claim_cursor: 0,
        j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
        tp20_connection: None,
        tp20_broadcast_periodic: None,
    }
}

/// See this file's own module doc comment for the full mechanism and why it
/// is deterministic, not timing-dependent.
///
/// **Fail-without/pass-with, verified directly against this fix (2026-08-31,
/// same session)**: temporarily reverted `reap_expired_cyclic_registrants`'s
/// `send_error_event_with_tag(&ctx.subscriptions, &ctx.logical_links,
/// reaped.cll_handle, PduErrorEvent::PduErrEvtRxTimeout, Some(reaped.
/// cop_handle), reaped.cop_tag.clone())` back to `send_error_event(&ctx.
/// subscriptions, &ctx.logical_links, &ctx.primitives, reaped.cll_handle,
/// PduErrorEvent::PduErrEvtRxTimeout, Some(reaped.cop_handle))`. Against the
/// reverted code this test FAILS (`last_error.cop_tag` observed as `None`,
/// since the fresh lookup inside `send_error_event`'s `resolve_cop_tag` runs
/// AFTER this test's own concurrent removal). Restoring the fix, it PASSES.
#[tokio::test]
async fn cop_tag_capture_survives_a_primitives_removal_racing_the_later_emission() {
    let deadline = tokio::time::Instant::now();
    let tag = b"race-token".to_vec();

    let logical_links = Arc::new(Mutex::new(HashMap::from([(
        RC_TEST_CLL,
        link_with_the_expired_registrant(deadline),
    )])));
    let primitives = Arc::new(Mutex::new(HashMap::from([(
        RC_TEST_COP_HANDLE,
        CopEntry {
            cll_handle: RC_TEST_CLL,
            dispatched: true,
            transmits: false,
            is_send_recv: true,
            cop_tag: Some(tag.clone()),
        },
    )])));
    let drain_watermarks = Arc::new(Mutex::new(HashMap::from([(RC_TEST_CHANNEL_ID, deadline)])));
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));

    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    std::mem::forget(shutdown_tx);

    let service = J2534Service {
        api: Arc::new(Mutex::new(api)),
        startup_config: Arc::new(crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap()),
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
        drain_watermarks: Arc::clone(&drain_watermarks),
        shared_channels: Arc::new(Mutex::new(HashMap::new())),
        primitives: Arc::clone(&primitives),
        terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
        next_cll_handle: Arc::new(Mutex::new(0)),
        next_cop_handle: Arc::new(Mutex::new(0)),
        next_connect_generation: Arc::new(Mutex::new(0)),
        next_occupancy_epoch: Arc::new(Mutex::new(0)),
        subscriptions: Arc::clone(&subscriptions),
        shutdown: shutdown_rx,
        module_state: Arc::new(Mutex::new(ModuleState::default())),
        module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
        system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
        j1850_bus_flavor: Arc::new(Mutex::new(None)),
        prog_voltage: Arc::new(Mutex::new(HashMap::new())),
        discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
        discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
    };

    // Fence: hold `primitives` before the sweep is ever polled, forcing its
    // collection critical section (which nests a `primitives` lock inside
    // `logical_links`) to queue as the FIRST waiter.
    let guard1 = primitives.lock().await;

    let sweep = {
        let primitives = Arc::clone(&primitives);
        let logical_links = Arc::clone(&logical_links);
        let drain_watermarks = Arc::clone(&drain_watermarks);
        let subscriptions = Arc::clone(&subscriptions);
        let service = service.clone();
        tokio::spawn(async move {
            let ctx = ChannelPollCtx {
                channel_id: RC_TEST_CHANNEL_ID,
                primitives,
                api: Arc::clone(&service.api),
                logical_links,
                drain_watermarks,
                subscriptions,
                module_state: Arc::clone(&service.module_state),
                module_event_buf: Arc::clone(&service.module_event_buf),
                system_event_buf: Arc::clone(&service.system_event_buf),
                executing_cop: Arc::new(Mutex::new(None)),
                last_func_tx: Arc::new(Mutex::new(None)),
                last_phys_tx: Arc::new(Mutex::new(None)),
                last_bus_activity: Arc::new(Mutex::new(tokio::time::Instant::now())),
                rx_supported: true,
                service,
            };
            reap_expired_cyclic_registrants(&ctx).await;
        })
    };

    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !sweep.is_finished(),
        "the sweep must be parked waiting for `primitives` before its collection critical \
         section can even begin"
    );

    // Release, then immediately re-queue for the SAME lock: FIFO fairness
    // guarantees the sweep's own (already-queued) collection acquisition is
    // served first, and this new request is placed second -- so it is
    // handed the very next permit, strictly before the sweep's own LATER
    // re-acquisition (whichever call site the wiring under test routes
    // through), which has not even been requested yet at this point.
    drop(guard1);
    let mut guard2 = primitives.lock().await;

    // The sweep has now run its entire collection critical section (which
    // never yields) to completion, capturing `RC_TEST_COP_HANDLE`'s tag
    // while it was still present, and is parked again waiting for the SAME
    // lock this test now holds -- simulate a concurrent finalization path
    // (`CancelComPrimitive`/`cancel_link_cops`/a hard-error path) removing
    // the entry in the exact gap this fix closes.
    guard2.remove(&RC_TEST_COP_HANDLE);
    drop(guard2);

    sweep.await.expect("the sweep task must not panic");

    let links = logical_links.lock().await;
    let last_error = links
        .get(&RC_TEST_CLL)
        .unwrap()
        .last_error
        .as_ref()
        .expect("PDU_ERR_EVT_RX_TIMEOUT should have been recorded");
    assert_eq!(
        last_error.cop_tag,
        Some(tag),
        "the tag captured during collection must survive a concurrent primitives removal \
         landing before the later emission call"
    );
}
