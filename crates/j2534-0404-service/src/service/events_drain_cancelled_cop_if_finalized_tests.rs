use super::*;
use crate::service::ChannelProtocol;

const TEST_CLL: u32 = 1;

/// A `LogicalLinkState` with every field at its `CreateComLogicalLink`
/// default -- mirrors `events_cyclic_deadline_confirm_writeback_tests::
/// minimal_link`/`registrant_lifecycle_tests::minimal_link`.
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

fn cop_entry(cll_handle: u32) -> CopEntry {
    CopEntry {
        cll_handle,
        dispatched: true,
        transmits: false,
        is_send_recv: false,
        cop_tag: None,
    }
}

/// ADR-182 follow-up fix (Codex review round, PR #78), scenario 1
/// (`rpc_cancel_com_primitive`'s own self-check): a `cancelled_cops` mark
/// was inserted for `cop_handle` AFTER the maintenance reap had already
/// fully finished it -- i.e. `primitives` no longer contains it by the time
/// this runs, the exact ordering-C interleaving design-advisor traced. The
/// self-check must detect the miss, drain the now-permanently-stale mark,
/// and report `true` (so the RPC can log it) -- mirroring the shape
/// `rpc_cancel_com_primitive` itself calls this in.
#[tokio::test]
async fn drains_a_stale_mark_left_behind_by_an_already_finalized_cop() {
    let mut link = minimal_link();
    link.cancelled_cops.insert(100); // the RPC's own mark-and-defer insert
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    // The reap already removed this COP from `primitives` before the mark
    // landed -- `primitives` has no entry for `cop_handle: 100` at all.
    let primitives: Arc<Mutex<HashMap<u32, CopEntry>>> = Arc::new(Mutex::new(HashMap::new()));

    let drained =
        drain_cancelled_cop_if_finalized(&primitives, &logical_links, TEST_CLL, 100).await;

    assert!(
        drained,
        "a mark for an already-finalized COP must be reported as drained"
    );
    assert!(
        !logical_links
            .lock()
            .await
            .get(&TEST_CLL)
            .unwrap()
            .cancelled_cops
            .contains(&100),
        "the stale mark must actually be removed from cancelled_cops, not just reported drained"
    );
}

/// ADR-182 follow-up fix, scenario 2 (formerly `reap_expired_cyclic_
/// registrants`'s own late drain, before the PR #78 edge-case-hunter
/// follow-up folded that call site's drain into `emit_terminal_if_live`
/// itself): the SAME
/// function, exercised with the input shape that former call site used to
/// produce -- covers the RPC's mark landing in the narrow window between
/// the reap's `link.registrants` removal and its `primitives` removal/
/// emission completing. Structurally identical input shape to the scenario
/// above (mark present, `primitives` already absent by the time this call
/// runs); kept as a standalone regression test of `drain_cancelled_cop_if_
/// finalized`'s own behavior even though nothing calls it from that exact
/// site anymore.
#[tokio::test]
async fn drains_a_stale_mark_that_landed_during_the_reaps_own_emission_window() {
    let mut link = minimal_link();
    link.cancelled_cops.insert(200);
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    // Mirrors `reap_expired_cyclic_registrants`'s own call site: by the
    // time its late drain runs, `emit_terminal_if_live` has already removed
    // the entry from `primitives` (whether via count-completion or expiry
    // -- both reap reasons share this exact call site).
    let primitives: Arc<Mutex<HashMap<u32, CopEntry>>> = Arc::new(Mutex::new(HashMap::new()));

    let drained =
        drain_cancelled_cop_if_finalized(&primitives, &logical_links, TEST_CLL, 200).await;

    assert!(drained);
    assert!(
        !logical_links
            .lock()
            .await
            .get(&TEST_CLL)
            .unwrap()
            .cancelled_cops
            .contains(&200)
    );
}

/// Negative control: the COP is still live in `primitives` (the ordinary,
/// non-raced case -- the mark-and-defer path's usual outcome, or the reap
/// not having reached this registrant yet). The mark must be left in place
/// -- `GetStatus`/`reap_cancelled_detached_registrants` still need it to
/// report/finalize `PduCopstCancelled` normally -- and the function must
/// report `false`.
#[tokio::test]
async fn leaves_a_live_cops_mark_alone() {
    let mut link = minimal_link();
    link.cancelled_cops.insert(300);
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    let primitives = Arc::new(Mutex::new(HashMap::from([(300, cop_entry(TEST_CLL))])));

    let drained =
        drain_cancelled_cop_if_finalized(&primitives, &logical_links, TEST_CLL, 300).await;

    assert!(
        !drained,
        "a still-live COP's mark must not be touched by this cleanup"
    );
    assert!(
        logical_links
            .lock()
            .await
            .get(&TEST_CLL)
            .unwrap()
            .cancelled_cops
            .contains(&300),
        "the mark must remain in place for reap_cancelled_detached_registrants to finalize \
         normally"
    );
}

/// Defensive: the CLL itself is already gone (torn down between the mark
/// insert and this cleanup running) -- must not panic, and reports `false`
/// since there is no mark left to have drained.
#[tokio::test]
async fn no_panic_when_the_cll_is_already_gone() {
    let logical_links: Arc<Mutex<HashMap<u32, LogicalLinkState>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let primitives: Arc<Mutex<HashMap<u32, CopEntry>>> = Arc::new(Mutex::new(HashMap::new()));

    let drained =
        drain_cancelled_cop_if_finalized(&primitives, &logical_links, TEST_CLL, 400).await;

    assert!(!drained);
}

/// PR #78 edge-case-hunter follow-up: `CoptStopcomm`'s cancel-all block
/// (`rpc_primitive.rs`) and `PDU_IOCTL_CLEAR_TX_QUEUE`'s handler
/// (`rpc_misc.rs`) both added the SAME shape of batch drain loop --
/// `for &cop in &cop_handles { drain_cancelled_cop_if_finalized(...).await; }`
/// -- right after their own `cancelled_cops.extend`. Since everything that
/// loop does beyond "call this already-exhaustively-tested helper once per
/// cop" is the iteration itself, this directly proves that shape over a
/// batch mixing a stranded mark (a cop the maintenance reap already
/// finalized -- absent from `primitives` -- between the batch's own extend
/// and this loop running) with a still-live one: only the stranded entry is
/// drained, the live one is left untouched for
/// `reap_cancelled_detached_registrants`/`GetStatus` to keep reporting
/// normally. A true end-to-end reproduction of the reap race through either
/// call site would need a test-only pause/gate hook inside `rpc_start_com_
/// primitive`/`ioctl_clear_tx_queue` themselves -- the same class of
/// infrastructure `rollback_stop_comm_pending_tests`'s own doc comment
/// (`rpc_primitive.rs`) judges not worth the invasiveness elsewhere in this
/// crate, since both functions' OWN earlier locking of the very locks this
/// race needs (`cancel_held_tx_items`'s `primitives` use ahead of
/// `ioctl_clear_tx_queue`'s own `cop_handles` read, `cops_to_cancel`'s
/// single-task sequential execution in `CoptStopcomm`) leaves no reusable
/// external contention point to force the exact narrow window between "cop
/// still in primitives when the batch was built" and "cop gone from
/// primitives by the time this loop's own per-cop check runs" -- so this
/// test exercises the loop construct directly instead, matching both sites'
/// actual code shape one-for-one.
#[tokio::test]
async fn batch_drain_loop_drains_only_the_stranded_marks_in_a_mixed_batch() {
    let mut link = minimal_link();
    link.cancelled_cops.insert(500); // stranded: reap already removed 500 from primitives
    link.cancelled_cops.insert(501); // still live: 501 stays in primitives below
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    let primitives = Arc::new(Mutex::new(HashMap::from([(501, cop_entry(TEST_CLL))])));

    // The exact shape both new call sites add right after their own
    // `cancelled_cops.extend`.
    let cop_handles = [500u32, 501u32];
    for &cop in &cop_handles {
        drain_cancelled_cop_if_finalized(&primitives, &logical_links, TEST_CLL, cop).await;
    }

    let links = logical_links.lock().await;
    let link = links.get(&TEST_CLL).unwrap();
    assert!(
        !link.cancelled_cops.contains(&500),
        "a stranded mark for an already-finalized cop must be drained by the batch loop"
    );
    assert!(
        link.cancelled_cops.contains(&501),
        "a still-live cop's mark must be left in place by the batch loop"
    );
}

/// P2 backlog fix (PR #78 edge-case-hunter follow-up, "`cancelled_cops`
/// strand, normal-completion side"): `emit_terminal_if_live`
/// itself now drains a pre-existing `cancelled_cops` mark for `cop_handle`
/// when THIS call wins the `primitives` removal -- closing the leak a
/// normal-completion caller (`handle_send_recv`'s `remaining == 0` arm,
/// `handle_delay`) used to leave open, since neither ever drained a mark a
/// `rpc_cancel_com_primitive`/batch-cancel insert left behind after losing
/// its own race against `primitives`.
#[tokio::test]
async fn emit_terminal_if_live_drains_its_own_cops_stale_mark_on_a_winning_removal() {
    let mut link = minimal_link();
    link.cancelled_cops.insert(600); // a mark stranded by a losing cancel race
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    let primitives = Arc::new(Mutex::new(HashMap::from([(600, cop_entry(TEST_CLL))])));
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let terminal_cops = Arc::new(Mutex::new(TerminalCopsLedger::default()));

    emit_terminal_if_live(
        &primitives,
        &logical_links,
        &subscriptions,
        &terminal_cops,
        TEST_CLL,
        600,
        PduComPrimitiveStatus::PduCopstFinished,
    )
    .await;

    assert!(
        !primitives.lock().await.contains_key(&600),
        "the winning call must remove the cop from primitives as before"
    );
    assert_eq!(
        terminal_cops.lock().await.lookup(600),
        Some((TEST_CLL, PduComPrimitiveStatus::PduCopstFinished)),
        "the winning call must still record the terminal status as before"
    );
    assert!(
        !logical_links
            .lock()
            .await
            .get(&TEST_CLL)
            .unwrap()
            .cancelled_cops
            .contains(&600),
        "emit_terminal_if_live must drain its own cop's stale cancelled_cops mark on a winning \
         removal"
    );
}

/// Negative control for the fix above: `emit_terminal_if_live` called for a
/// `cop_handle` NOT present in `primitives` (this call loses the race) must
/// stay a complete no-op -- in particular it must NOT blindly drain the
/// whole `cancelled_cops` set, only ever the one entry belonging to a
/// winning removal. A different, still-live cop's mark on the SAME CLL must
/// be left untouched.
#[tokio::test]
async fn emit_terminal_if_live_leaves_other_cops_marks_alone_on_a_losing_call() {
    let mut link = minimal_link();
    link.cancelled_cops.insert(700); // belongs to a different, still-live cop
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    // cop_handle 701 is NOT in primitives -- this call loses the race.
    let primitives = Arc::new(Mutex::new(HashMap::from([(700, cop_entry(TEST_CLL))])));
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let terminal_cops = Arc::new(Mutex::new(TerminalCopsLedger::default()));

    emit_terminal_if_live(
        &primitives,
        &logical_links,
        &subscriptions,
        &terminal_cops,
        TEST_CLL,
        701,
        PduComPrimitiveStatus::PduCopstFinished,
    )
    .await;

    assert!(
        terminal_cops.lock().await.lookup(701).is_none(),
        "a losing call must not record any terminal status"
    );
    assert!(
        logical_links
            .lock()
            .await
            .get(&TEST_CLL)
            .unwrap()
            .cancelled_cops
            .contains(&700),
        "a losing call must not touch a different cop's cancelled_cops mark on the same CLL"
    );
}

/// Direct unit-level coverage of `emit_nonterminal_if_live` (Codex review,
/// P2, PR #101, round 12, ADR-192) itself -- the nonterminal counterpart to
/// `emit_terminal_if_live` above, mirroring that pair's own present/absent
/// test shape. Positive case: `cop_handle` is still present in `primitives`,
/// so the status must be sent -- and, unlike `emit_terminal_if_live`, the
/// entry must NOT be removed from `primitives` (a nonterminal status must
/// never finalize a COP).
#[tokio::test]
async fn emit_nonterminal_if_live_sends_status_when_entry_present() {
    let link = minimal_link();
    let target = CllQueueTarget::from_link(&link);
    let rx_buf = Arc::clone(&link.rx_buf);
    let primitives = Arc::new(Mutex::new(HashMap::from([(900, cop_entry(TEST_CLL))])));
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let terminal_cops = Arc::new(Mutex::new(TerminalCopsLedger::default()));

    emit_nonterminal_if_live(
        &primitives,
        &subscriptions,
        &terminal_cops,
        Some(&target),
        TEST_CLL,
        900,
        PduComPrimitiveStatus::PduCopstExecuting,
    )
    .await;

    assert!(
        primitives.lock().await.contains_key(&900),
        "a nonterminal status must never remove the cop from primitives"
    );
    let queue = rx_buf.lock().await;
    assert_eq!(
        queue.items.len(),
        1,
        "exactly one status event must be queued"
    );
    assert!(
        matches!(
            queue.items.front(),
            Some(CllQueueItem::Status(TrackedStatus {
                event: StatusEvent::Cop {
                    cop_handle: 900,
                    status: PduComPrimitiveStatus::PduCopstExecuting,
                    ..
                },
                ..
            }))
        ),
        "the queued event must be this cop_handle's own PduCopstExecuting"
    );
}

/// Negative control for the fix above: `cop_handle` is NOT present in
/// `primitives` (a concurrent terminal-finalization path already removed it
/// -- the exact race `finalize_or_orphan_broadcast_periodic_start`'s `Live`
/// arm can now hit, see its own doc comment). The status must NOT be sent,
/// and this must not panic.
#[tokio::test]
async fn emit_nonterminal_if_live_is_a_no_op_when_entry_absent() {
    let link = minimal_link();
    let target = CllQueueTarget::from_link(&link);
    let rx_buf = Arc::clone(&link.rx_buf);
    let primitives: Arc<Mutex<HashMap<u32, CopEntry>>> = Arc::new(Mutex::new(HashMap::new()));
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let terminal_cops = Arc::new(Mutex::new(TerminalCopsLedger::default()));

    emit_nonterminal_if_live(
        &primitives,
        &subscriptions,
        &terminal_cops,
        Some(&target),
        TEST_CLL,
        901,
        PduComPrimitiveStatus::PduCopstExecuting,
    )
    .await;

    assert!(
        rx_buf.lock().await.items.is_empty(),
        "no status event may be queued when the cop_handle is already absent from primitives"
    );
}

/// PR #78 edge-case-hunter follow-up, `cancel_link_cops` half: this CLL going away entirely
/// makes any lingering `cancelled_cops` mark moot, whether it belongs to a
/// cop this call's own batch-remove loop just cancelled or to an unrelated
/// cop that was never in `primitives` for this CLL at all. Mirrors
/// `events_registrant_lifecycle_tests::cancel_link_cops_clears_this_clls_registrants`'s
/// fixture-construction pattern for the sibling `registrants.clear()`
/// assertion.
#[tokio::test]
async fn cancel_link_cops_wholesale_clears_cancelled_cops() {
    let mut link = minimal_link();
    link.cancelled_cops.insert(800); // belongs to a cop this call will cancel below
    link.cancelled_cops.insert(801); // unrelated: never in primitives for this CLL at all
    let logical_links = Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)])));
    let primitives = Arc::new(Mutex::new(HashMap::from([(800, cop_entry(TEST_CLL))])));
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
            .cancelled_cops
            .is_empty(),
        "cancel_link_cops must wholesale-clear cancelled_cops, mooting both a cop it just \
         cancelled and an unrelated stranded mark"
    );
}
