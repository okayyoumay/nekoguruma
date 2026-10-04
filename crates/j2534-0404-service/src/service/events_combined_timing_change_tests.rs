use super::*;

/// A minimal `LogicalLinkState` with every field at a neutral default,
/// for tests that only care about `active`/`working` (mirrors
/// `registrant_lifecycle_tests::minimal_link`'s role for that module).
fn minimal_link() -> LogicalLinkState {
    LogicalLinkState {
        channel_id: None,
        protocol: ChannelProtocol::ISO14230,
        hw_protocol_id: j2534_0404::ISO14230,
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

/// Builds a [`WorkingTimingSnapshot`] from `link`'s CURRENT `working`
/// state for `combined`'s own keys, mirroring exactly how the real
/// fold-time capture in `poll_rx_inner` builds one. Call this BEFORE
/// mutating `link` to get a "nothing changed since fold" snapshot for
/// tests that don't exercise the round-7 concurrent-write guard itself.
fn snapshot_from_link(
    link: &LogicalLinkState,
    combined: &CombinedTimingChange,
) -> WorkingTimingSnapshot {
    WorkingTimingSnapshot {
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
    }
}

/// Codex review, PR #17, finding 1: derived ComParams must be stored
/// into `link.active.unum32`/`link.working.unum32` when the hardware
/// push succeeded, not only folded into the caller's transient
/// hardware-push delta -- otherwise `GetComParam` keeps reporting the
/// pre-exchange values, and `CP_P2Star` (no J2534 hardware mapping)
/// never changes by any path at all.
///
/// Fail-without-the-fix control: temporarily removing the `if
/// hardware_push_ok` gate's body (storing derived unconditionally, the
/// shape this test's sibling below guards against) or removing the
/// `derived` loop entirely (the original Codex-flagged gap) both make
/// one of these two tests fail; restoring the real body makes both pass.
#[test]
fn store_combined_timing_change_stores_derived_only_when_hardware_push_ok() {
    let mut link = minimal_link();
    let mut combined = CombinedTimingChange::default();
    combined
        .derived
        .insert(ComParamId(j2534_0404::P2_MIN), 5_000);
    combined.derived.insert(PARAM_P2_STAR, 12_500_000);
    combined.ecu_entries.insert(1, [10, 20, 30, 40, 50]);
    let working_snapshot = snapshot_from_link(&link, &combined);

    store_combined_timing_change(&mut link, &combined, true, &working_snapshot);

    assert_eq!(
        link.active.unum32.get(&ComParamId(j2534_0404::P2_MIN)),
        Some(&5_000),
        "derived CP_P2Min must be stored into the Active set on push success"
    );
    assert_eq!(
        link.working.unum32.get(&ComParamId(j2534_0404::P2_MIN)),
        Some(&5_000)
    );
    assert_eq!(
        link.active.unum32.get(&PARAM_P2_STAR),
        Some(&12_500_000),
        "CP_P2Star has no J2534 hardware mapping, but its stored value must still change"
    );
    assert_eq!(link.working.unum32.get(&PARAM_P2_STAR), Some(&12_500_000));

    let sf = link
        .active
        .structfield
        .get(&PARAM_ACCESS_TIMING_ECU)
        .unwrap();
    let vci_service_interface::param_structfield::Data::AccessTiming(list) =
        sf.data.as_ref().unwrap()
    else {
        panic!("expected AccessTiming");
    };
    assert_eq!(list.entries.len(), 1);
}

/// edge-case-hunter finding (PR #17, round 2): a failed hardware push
/// must NOT promote the derived values into Active/Working -- "Active"
/// means "actually on hardware" (`handle_update_param`'s own `all_ok`
/// precedent) -- but `ecu_entries` (a wire observation, never itself
/// pushed to hardware) must still be stored regardless, since TPI=1's
/// reapply path depends on it and it was never contingent on THIS
/// pass's SET_CONFIG outcome in the first place.
#[test]
fn store_combined_timing_change_skips_derived_but_keeps_ecu_entry_on_push_failure() {
    let mut link = minimal_link();
    let mut combined = CombinedTimingChange::default();
    combined
        .derived
        .insert(ComParamId(j2534_0404::P2_MIN), 5_000);
    combined.ecu_entries.insert(1, [10, 20, 30, 40, 50]);
    let working_snapshot = snapshot_from_link(&link, &combined);

    store_combined_timing_change(&mut link, &combined, false, &working_snapshot);

    assert!(
        !link
            .active
            .unum32
            .contains_key(&ComParamId(j2534_0404::P2_MIN)),
        "derived ComParams must not be promoted when the hardware push failed"
    );
    let sf = link
        .active
        .structfield
        .get(&PARAM_ACCESS_TIMING_ECU)
        .unwrap();
    let vci_service_interface::param_structfield::Data::AccessTiming(list) =
        sf.data.as_ref().unwrap()
    else {
        panic!("expected AccessTiming");
    };
    assert_eq!(
        list.entries.len(),
        1,
        "the ECU's wire observation must still be recorded even when the push failed"
    );
}

/// Codex round-7 finding, PR #17: a concurrent `SetComParam` that landed
/// on `link.working` for one of `combined.derived`'s own keys during the
/// hardware-push await window (simulated here by making the snapshot
/// reflect an OLDER value than `link.working`'s CURRENT one) must survive
/// -- Active still gets this pass's exchange-derived value (it always
/// reflects "actually on hardware"), but Working must not be clobbered.
///
/// Fail-without-the-fix control: removing the `working_snapshot`
/// per-key compare (storing Working unconditionally like Active) makes
/// this test fail (`link.working` would read 5_000 instead of the
/// concurrent write's 9_999); restoring the guard makes it pass. Both
/// outcomes confirmed by temporarily reverting the guard and re-running
/// this test in isolation during implementation.
#[test]
fn store_combined_timing_change_preserves_a_concurrent_working_write_for_a_mutated_key() {
    let mut link = minimal_link();
    let mut combined = CombinedTimingChange::default();
    combined
        .derived
        .insert(ComParamId(j2534_0404::P2_MIN), 5_000);

    // Fold-time Working value was 1_000 ...
    link.working
        .unum32
        .insert(ComParamId(j2534_0404::P2_MIN), 1_000);
    let working_snapshot = snapshot_from_link(&link, &combined);
    // ... but a concurrent client SetComParam landed during the
    // hardware-push await window, staging a DIFFERENT value.
    link.working
        .unum32
        .insert(ComParamId(j2534_0404::P2_MIN), 9_999);

    store_combined_timing_change(&mut link, &combined, true, &working_snapshot);

    assert_eq!(
        link.active.unum32.get(&ComParamId(j2534_0404::P2_MIN)),
        Some(&5_000),
        "Active always wins: it reflects this pass's exchange-derived value"
    );
    assert_eq!(
        link.working.unum32.get(&ComParamId(j2534_0404::P2_MIN)),
        Some(&9_999),
        "the concurrent client's Working write must survive, not be overwritten"
    );
}

/// Codex round-7 finding, PR #17 (edge-case-hunter audit, same round):
/// the doc comment on `store_combined_timing_change`'s per-key guard
/// says a key missing from `working_snapshot.derived` is "treated
/// conservatively as 'assume changed' (skip)" -- this test exercises
/// that branch directly, rather than relying on `snapshot_from_link`
/// (whose keys are always a subset of `combined.derived`'s, so the
/// branch is otherwise unreachable from any other test in this module).
/// Unreachable from the one production call site too (it builds the
/// snapshot from `combined.derived`'s own keys), but kept as an
/// explicit proof of the defensive branch's documented behavior.
///
/// Fail-without-the-fix control: changing `is_some_and` to
/// `is_none_or` (treating a missing key as "assume unchanged," the
/// opposite of the documented policy) makes this test fail (`Working`
/// would read 5_000 instead of keeping its own concurrent value);
/// restoring `is_some_and` makes it pass.
#[test]
fn store_combined_timing_change_skips_working_when_key_missing_from_snapshot() {
    let mut link = minimal_link();
    let mut combined = CombinedTimingChange::default();
    combined
        .derived
        .insert(ComParamId(j2534_0404::P2_MIN), 5_000);

    // Working already holds a value the snapshot never recorded for
    // this key at all (not merely a different one).
    link.working
        .unum32
        .insert(ComParamId(j2534_0404::P2_MIN), 9_999);
    let working_snapshot = WorkingTimingSnapshot::default();

    store_combined_timing_change(&mut link, &combined, true, &working_snapshot);

    assert_eq!(
        link.active.unum32.get(&ComParamId(j2534_0404::P2_MIN)),
        Some(&5_000),
        "Active always wins regardless of snapshot completeness"
    );
    assert_eq!(
        link.working.unum32.get(&ComParamId(j2534_0404::P2_MIN)),
        Some(&9_999),
        "a key missing from the snapshot must be treated as 'assume changed' and skipped"
    );
}

/// Sibling of the test above: when NOTHING changed Working since the
/// fold-time snapshot, the pre-fix behavior is preserved exactly -- both
/// Active and Working get the exchange's derived value.
#[test]
fn store_combined_timing_change_stores_into_working_when_unchanged_since_snapshot() {
    let mut link = minimal_link();
    let mut combined = CombinedTimingChange::default();
    combined
        .derived
        .insert(ComParamId(j2534_0404::P2_MIN), 5_000);

    link.working
        .unum32
        .insert(ComParamId(j2534_0404::P2_MIN), 1_000);
    let working_snapshot = snapshot_from_link(&link, &combined);
    // No concurrent write happens here.

    store_combined_timing_change(&mut link, &combined, true, &working_snapshot);

    assert_eq!(
        link.active.unum32.get(&ComParamId(j2534_0404::P2_MIN)),
        Some(&5_000)
    );
    assert_eq!(
        link.working.unum32.get(&ComParamId(j2534_0404::P2_MIN)),
        Some(&5_000),
        "no concurrent write happened, so Working must be updated exactly like Active"
    );
}

/// Codex round-7 finding, PR #17: the same first-wins guard, applied to
/// the WHOLE `CP_AccessTiming_Ecu` structfield rather than per-key -- a
/// concurrent client write that replaces (any part of) the structfield
/// during the hardware-push await window must survive; this pass's
/// `ecu_entries` are simply not merged into Working (Active still gets
/// them, unconditionally).
///
/// Fail-without-the-fix control: removing the structfield compare
/// (storing Working's `ecu_entries` unconditionally, the pre-fix shape)
/// makes this test fail -- Working's own `TimingSet=1` entry would be
/// overwritten with the exchange's bytes (`p2_min == 10`) instead of
/// keeping the concurrent write's own untouched bytes (`p2_min == 1`);
/// restoring the guard makes it pass. Both outcomes confirmed by
/// temporarily reverting the guard and re-running this test in isolation
/// during implementation.
#[test]
fn store_combined_timing_change_preserves_a_concurrent_working_structfield_write() {
    let mut link = minimal_link();
    let mut combined = CombinedTimingChange::default();
    combined.ecu_entries.insert(1, [10, 20, 30, 40, 50]);

    // Fold-time Working state: one TimingSet=1 entry.
    store_access_timing_ecu_entry(&mut link.working, 1, [1, 2, 3, 4, 5]);
    let working_snapshot = snapshot_from_link(&link, &combined);
    // A concurrent client SetComParam(CP_AccessTiming_Ecu) landed during
    // the hardware-push await window, adding a second TimingSet=2 entry
    // -- any change at all to the structfield must be detected, not just
    // a wholesale replacement.
    store_access_timing_ecu_entry(&mut link.working, 2, [9, 9, 9, 9, 9]);

    store_combined_timing_change(&mut link, &combined, true, &working_snapshot);

    let active_sf = link
        .active
        .structfield
        .get(&PARAM_ACCESS_TIMING_ECU)
        .unwrap();
    let vci_service_interface::param_structfield::Data::AccessTiming(active_list) =
        active_sf.data.as_ref().unwrap()
    else {
        panic!("expected AccessTiming");
    };
    assert_eq!(
        active_list.entries.len(),
        1,
        "Active always gets this pass's ecu_entries, unconditionally"
    );
    assert_eq!(active_list.entries[0].p2_min, 10);

    let working_sf = link
        .working
        .structfield
        .get(&PARAM_ACCESS_TIMING_ECU)
        .unwrap();
    let vci_service_interface::param_structfield::Data::AccessTiming(working_list) =
        working_sf.data.as_ref().unwrap()
    else {
        panic!("expected AccessTiming");
    };
    assert_eq!(
        working_list.entries.len(),
        2,
        "Working must keep the concurrent write's shape untouched -- the exchange's \
             ecu_entries must not be merged in"
    );
    assert_eq!(
        working_list
            .entries
            .iter()
            .find(|e| e.timing_set == 1)
            .unwrap()
            .p2_min,
        1,
        "the concurrent write's own TimingSet=1 bytes must survive, not the exchange's"
    );
}

/// Sibling of the test above: when NOTHING changed Working's structfield
/// since the fold-time snapshot, the pre-fix behavior is preserved
/// exactly -- both Active and Working get the exchange's ecu_entries.
#[test]
fn store_combined_timing_change_stores_structfield_into_working_when_unchanged() {
    let mut link = minimal_link();
    let mut combined = CombinedTimingChange::default();
    combined.ecu_entries.insert(1, [10, 20, 30, 40, 50]);

    store_access_timing_ecu_entry(&mut link.working, 1, [1, 2, 3, 4, 5]);
    let working_snapshot = snapshot_from_link(&link, &combined);
    // No concurrent write happens here.

    store_combined_timing_change(&mut link, &combined, true, &working_snapshot);

    let working_sf = link
        .working
        .structfield
        .get(&PARAM_ACCESS_TIMING_ECU)
        .unwrap();
    let vci_service_interface::param_structfield::Data::AccessTiming(working_list) =
        working_sf.data.as_ref().unwrap()
    else {
        panic!("expected AccessTiming");
    };
    assert_eq!(working_list.entries.len(), 1);
    assert_eq!(
        working_list.entries[0].p2_min, 10,
        "no concurrent write happened, so Working must be overwritten with the exchange's \
             bytes exactly like Active"
    );
}

/// ADR-150: `CP_SessionTiming_Ecu` gets the IDENTICAL whole-structfield
/// first-wins guard as `CP_AccessTiming_Ecu` above, applied
/// independently -- mirrors
/// `store_combined_timing_change_preserves_a_concurrent_working_structfield_write`
/// exactly, just for `session_entries`/`PARAM_SESSION_TIMING_ECU`.
///
/// Fail-without-the-fix control: removing the `session_timing_sf`
/// compare (storing Working's `session_entries` unconditionally) makes
/// this test fail -- Working's own `session == 1` entry would be
/// overwritten with the exchange's values (`p2_max == 10`) instead of
/// keeping the concurrent write's own untouched value (`p2_max == 1`);
/// restoring the guard makes it pass.
#[test]
fn store_combined_timing_change_preserves_a_concurrent_working_session_structfield_write() {
    let mut link = minimal_link();
    let mut combined = CombinedTimingChange::default();
    combined.session_entries.insert(1, (10, 20));

    // Fold-time Working state: one session=1 entry.
    store_session_timing_ecu_entry(&mut link.working, 1, 1, 2);
    let working_snapshot = snapshot_from_link(&link, &combined);
    // A concurrent client SetComParam(CP_SessionTiming_Ecu) landed during
    // the hardware-push await window, adding a second session=2 entry --
    // any change at all to the structfield must be detected.
    store_session_timing_ecu_entry(&mut link.working, 2, 9, 9);

    store_combined_timing_change(&mut link, &combined, true, &working_snapshot);

    let active_sf = link
        .active
        .structfield
        .get(&PARAM_SESSION_TIMING_ECU)
        .unwrap();
    let vci_service_interface::param_structfield::Data::SessionTiming(active_list) =
        active_sf.data.as_ref().unwrap()
    else {
        panic!("expected SessionTiming");
    };
    assert_eq!(
        active_list.entries.len(),
        1,
        "Active always gets this pass's session_entries, unconditionally"
    );
    assert_eq!(active_list.entries[0].p2_max, 10);

    let working_sf = link
        .working
        .structfield
        .get(&PARAM_SESSION_TIMING_ECU)
        .unwrap();
    let vci_service_interface::param_structfield::Data::SessionTiming(working_list) =
        working_sf.data.as_ref().unwrap()
    else {
        panic!("expected SessionTiming");
    };
    assert_eq!(
        working_list.entries.len(),
        2,
        "Working must keep the concurrent write's shape untouched -- the exchange's \
             session_entries must not be merged in"
    );
    assert_eq!(
        working_list
            .entries
            .iter()
            .find(|e| e.session == 1)
            .unwrap()
            .p2_max,
        1,
        "the concurrent write's own session=1 value must survive, not the exchange's"
    );
}

/// Sibling of the test above: when NOTHING changed Working's
/// `CP_SessionTiming_Ecu` structfield since the fold-time snapshot, both
/// Active and Working get the exchange's `session_entries` -- mirrors
/// `store_combined_timing_change_stores_structfield_into_working_when_unchanged`.
#[test]
fn store_combined_timing_change_stores_session_structfield_into_working_when_unchanged() {
    let mut link = minimal_link();
    let mut combined = CombinedTimingChange::default();
    combined.session_entries.insert(1, (10, 20));

    store_session_timing_ecu_entry(&mut link.working, 1, 1, 2);
    let working_snapshot = snapshot_from_link(&link, &combined);
    // No concurrent write happens here.

    store_combined_timing_change(&mut link, &combined, true, &working_snapshot);

    let working_sf = link
        .working
        .structfield
        .get(&PARAM_SESSION_TIMING_ECU)
        .unwrap();
    let vci_service_interface::param_structfield::Data::SessionTiming(working_list) =
        working_sf.data.as_ref().unwrap()
    else {
        panic!("expected SessionTiming");
    };
    assert_eq!(working_list.entries.len(), 1);
    assert_eq!(
        working_list.entries[0].p2_max, 10,
        "no concurrent write happened, so Working must be overwritten with the exchange's \
             values exactly like Active"
    );
}

/// Codex review, PR #17, round 4: two DIFFERENT registrants qualifying on
/// the same CLL in one pass are, by construction, always two independent
/// SID 0x83 exchanges -- the later exchange's result must simply
/// supersede the earlier one, not worst-case-combine with it. Change A
/// (seq 0) has a larger P2Max (60) than change B's (seq 3, P2Max 20): the
/// OLD worst-case fold would have wrongly maxed these to 60, but
/// last-arrival-wins must produce B's own value (20). Fed in REVERSED
/// insertion order (`[(3, &b), (0, &a)]`) to prove the function sorts by
/// `frame_seq` itself rather than relying on iterator order.
#[test]
fn select_latest_timing_changes_lets_the_later_change_win() {
    let a = PendingTimingChange {
        derived: vec![
            (ComParamId(j2534_0404::P2_MIN), 10 * 500),
            (ComParamId(j2534_0404::P2_MAX), 60 * 500),
        ],
        ecu_entry: Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [10, 60, 30, 40, 50],
        }),
    };
    let b = PendingTimingChange {
        derived: vec![
            (ComParamId(j2534_0404::P2_MIN), 5 * 500),
            (ComParamId(j2534_0404::P2_MAX), 20 * 500),
        ],
        ecu_entry: Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [5, 20, 20, 45, 55],
        }),
    };

    let combined = select_latest_timing_changes([(3, &b), (0, &a)].into_iter());

    assert_eq!(
        combined.derived.get(&ComParamId(j2534_0404::P2_MIN)),
        Some(&(5 * 500)),
        "the later exchange's (B, seq 3) own P2Min must win"
    );
    assert_eq!(
        combined.derived.get(&ComParamId(j2534_0404::P2_MAX)),
        Some(&(20 * 500)),
        "the later exchange's (B, seq 3) own P2Max must win -- the OLD worst-case fold \
             would have wrongly maxed this to 60 (A's value)"
    );
    assert_eq!(
        combined.ecu_entries.get(&1),
        Some(&[5, 20, 20, 45, 55]),
        "same-TimingSet ecu_entries must be plainly overwritten by the later exchange's own \
             bytes, not worst-case-combined with the earlier one"
    );
}

/// A single registrant's observation folds to itself (no other exchange
/// to be superseded by) -- the degenerate/common single-CLL-single-
/// registrant case.
#[test]
fn select_latest_timing_changes_single_change_is_a_no_op_fold() {
    let a = PendingTimingChange {
        derived: vec![(ComParamId(j2534_0404::P2_MIN), 10 * 500)],
        ecu_entry: Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [10, 20, 30, 40, 50],
        }),
    };
    let combined = select_latest_timing_changes([(0, &a)].into_iter());
    assert_eq!(
        combined.derived.get(&ComParamId(j2534_0404::P2_MIN)),
        Some(&(10 * 500))
    );
    assert_eq!(combined.ecu_entries.get(&1), Some(&[10, 20, 30, 40, 50]));
}

/// Baseline: all three conditions satisfied, flag must read `true`.
#[test]
fn timing_frame_flag_ok_true_when_all_conditions_hold() {
    let derived_stored: HashSet<u32> = HashSet::from([42u32]);
    let derived_winner: HashMap<u32, u32> = HashMap::from([(42, 1)]);
    let superseded_timing: HashSet<(u32, u32, usize)> = HashSet::new();

    assert!(timing_frame_flag_ok(
        42,
        1,
        0,
        &derived_stored,
        &derived_winner,
        &superseded_timing,
    ));
}

/// Codex round-2 Finding A (PR #17), mirrors old
/// `buffered_frame_flag_cleared_when_derived_not_stored`: a frame whose
/// CLL never made it into `derived_stored` (push failed, or the
/// post-push `connect_generation` recheck found the CLL torn
/// down/reconnected) must read `false` -- the flag must assert "derived
/// timing WAS stored," never merely "a qualifying frame was observed
/// this pass." Fail-without/pass-with: dropping the `derived_stored`
/// half of the `&&` makes this wrongly return `true`.
#[test]
fn timing_frame_flag_ok_false_when_cll_not_in_derived_stored() {
    let derived_stored: HashSet<u32> = HashSet::new(); // handle 42 did not make it in.
    let derived_winner: HashMap<u32, u32> = HashMap::from([(42, 1)]);
    let superseded_timing: HashSet<(u32, u32, usize)> = HashSet::new();

    assert!(!timing_frame_flag_ok(
        42,
        1,
        0,
        &derived_stored,
        &derived_winner,
        &superseded_timing,
    ));
}

/// edge-case-hunter finding on the round-4 fix (PR #17), mirrors old
/// `buffered_frame_flag_cleared_for_the_losing_registrant_on_a_shared_cll`:
/// two DIFFERENT registrants (`cop_handle` 1 and 2) qualifying on the
/// SAME CLL in one pass are two independent SID 0x83 exchanges (round-4's
/// own premise) -- `select_latest_timing_changes` keeps only the later
/// one's (cop_handle 2's) derived contribution, so even though the CLL's
/// overall push succeeded (`derived_stored` contains the CLL), the LOSING
/// registrant's (cop_handle 1's) own frame must read `false` --
/// `derived_stored` alone cannot distinguish the winner's frames from the
/// loser's on the same CLL. Fail-without/pass-with: dropping the
/// `derived_winner` half of the `&&` (leaving only
/// `derived_stored.contains(...)`) makes cop_handle 1 wrongly return
/// `true`.
#[test]
fn timing_frame_flag_ok_false_when_a_different_cop_handle_won() {
    let derived_stored: HashSet<u32> = HashSet::from([7u32]);
    let derived_winner: HashMap<u32, u32> = HashMap::from([(7, 2)]); // cop_handle 2 won the fold.
    let superseded_timing: HashSet<(u32, u32, usize)> = HashSet::new();

    assert!(
        !timing_frame_flag_ok(
            7,
            1,
            0,
            &derived_stored,
            &derived_winner,
            &superseded_timing
        ),
        "the losing registrant's (cop_handle 1) frame must read false -- its derived \
             contribution was fully overwritten before the push ever ran"
    );
    assert!(
        timing_frame_flag_ok(
            7,
            2,
            1,
            &derived_stored,
            &derived_winner,
            &superseded_timing
        ),
        "the winning registrant's (cop_handle 2) frame keeps its flag true"
    );
}

/// Codex round-5 Finding 2 (PR #17): a SINGLE physically addressed
/// registrant (`cop_handle` 1) with two qualifying responses in one pass
/// (`frame_seq` 0 and 1) -- physical addressing never accumulates
/// (round 3), so the later response's values fully overwrote the
/// earlier one's before either was ever stored. `derived_winner` alone
/// (round 4, per-registrant granularity) cannot distinguish the two --
/// both share the same `cop_handle` -- so `superseded_timing` marks the
/// earlier `frame_seq` explicitly. The earlier frame_seq must read
/// `false`; the later one, `true`.
///
/// Fail-without/pass-with: temporarily dropping the `superseded_timing`
/// check from `timing_frame_flag_ok` (removing the third `&&` term)
/// makes frame_seq 0 wrongly return `true` -- confirmed: with the check
/// removed, `timing_frame_flag_ok(7, 1, 0, ...)` returns `true`, which
/// fails this test's first assertion; restoring the check makes it
/// return `false` again and the test passes.
#[test]
fn timing_frame_flag_ok_false_when_superseded_by_a_later_same_registrant_response() {
    let derived_stored: HashSet<u32> = HashSet::from([7u32]);
    let derived_winner: HashMap<u32, u32> = HashMap::from([(7, 1)]); // cop_handle 1 is the (only) winner.
    let superseded_timing: HashSet<(u32, u32, usize)> = HashSet::from([(7, 1, 0)]); // frame_seq 0 was superseded.

    assert!(
        !timing_frame_flag_ok(
            7,
            1,
            0,
            &derived_stored,
            &derived_winner,
            &superseded_timing
        ),
        "the earlier, superseded frame_seq (0) must read false even though its own \
             registrant is the CLL's overall fold winner"
    );
    assert!(
        timing_frame_flag_ok(
            7,
            1,
            1,
            &derived_stored,
            &derived_winner,
            &superseded_timing
        ),
        "the later, surviving frame_seq (1) keeps its flag true"
    );
}
