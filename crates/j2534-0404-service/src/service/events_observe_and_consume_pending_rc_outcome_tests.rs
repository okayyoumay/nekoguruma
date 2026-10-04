use super::*;

/// A `CopRegistrant` with every field at a neutral default, ready for
/// per-test customization via struct-update syntax -- mirrors
/// `merge_registrant_writeback_tests::registrant`'s own role in this
/// file.
fn registrant(matches_got: u32, pending_rc: Option<u8>) -> CopRegistrant {
    CopRegistrant {
        cop_handle: 100,
        registration_seq: 0,
        tier: RegistrantTier::ActiveSendReceive,
        expected: Vec::new(),
        rc_cfg: None,
        request_sid: None,
        matches_needed: Some(2),
        matches_got,
        pending_rc,
        connect_generation: 1,
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

/// Required test 1 (the core data-loss scenario, ADR-101 Decision §C): a
/// pre-set `Some` `pending_rc` -- standing in for a companion-channel
/// merge (Decision §A) that landed before this call -- with no
/// `matches_got` delta is reported as `PendingRc`, and the field is left
/// consumed (`None`) afterward.
#[test]
fn preset_pending_rc_with_no_match_delta_is_reported_and_consumed() {
    let mut reg = registrant(0, Some(0x78));

    let result = observe_and_consume_pending_rc_outcome(&mut reg, 0, 0);

    assert!(matches!(result, PollMatchResult::PendingRc(0x78)));
    assert_eq!(
        reg.pending_rc, None,
        "the observed occurrence must be consumed by this call"
    );
}

/// Required test 2 (Consequences residual (a)): a `matches_got` delta
/// coexisting with a `Some` `pending_rc` reports `Matched` (which
/// outranks `PendingRc`), and `pending_rc` is STILL cleared -- the
/// unconditional-clear residual is deliberate (see `Decision §C`): the
/// RC was never reported, but leaving it set would surface a stale RC on
/// a later call after the match already superseded it.
#[test]
fn coexisting_match_and_pending_rc_reports_matched_and_still_clears_pending_rc() {
    let mut reg = registrant(1, Some(0x78));

    let result = observe_and_consume_pending_rc_outcome(&mut reg, 0, 0);

    assert!(
        matches!(result, PollMatchResult::Matched(1, 0)),
        "a genuine match must outrank a coexisting pending RC"
    );
    assert_eq!(
        reg.pending_rc, None,
        "the coexisting pending RC must be cleared even though it was never reported -- \
             the unconditional-clear residual documented in ADR-101 Decision §C / Consequences (a)"
    );
}

/// Required test 3: no delta and no `pending_rc` is a pure no-op --
/// reports `NoMatch` and leaves the (already-`None`) field untouched.
#[test]
fn no_delta_and_no_pending_rc_is_a_no_op() {
    let mut reg = registrant(0, None);

    let result = observe_and_consume_pending_rc_outcome(&mut reg, 0, 0);

    assert!(matches!(result, PollMatchResult::NoMatch));
    assert_eq!(reg.pending_rc, None);
}

/// Required test 4 (chattering-ECU / repeated companion detection): two
/// successive calls, with `pending_rc` manually re-set to a NEW `Some`
/// between them (simulating a second companion-channel detection landing
/// after the first was consumed), each independently report their own
/// occurrence -- proving consume-then-detect-again works, not just a
/// one-shot.
#[test]
fn two_successive_occurrences_are_each_independently_observed_and_consumed() {
    let mut reg = registrant(0, Some(0x78));

    let first = observe_and_consume_pending_rc_outcome(&mut reg, 0, 0);
    assert!(matches!(first, PollMatchResult::PendingRc(0x78)));
    assert_eq!(reg.pending_rc, None);

    // A second, distinct occurrence lands (e.g. another companion-channel
    // merge) between the two calls.
    reg.pending_rc = Some(0x21);

    let second = observe_and_consume_pending_rc_outcome(&mut reg, 0, 0);
    assert!(
        matches!(second, PollMatchResult::PendingRc(0x21)),
        "a second, distinct occurrence must be independently reported, not swallowed \
             by having already been consumed once"
    );
    assert_eq!(reg.pending_rc, None);
}

/// Required test (ADR-148 Amendment 5, the P1 fix itself): a
/// `concat_segments_got` delta above baseline coexisting with a `Some`
/// `pending_rc` reports `Absorbed` (which outranks `PendingRc`, same
/// ranking as before), but -- unlike the `Matched` case above --
/// `pending_rc` must be PRESERVED, not cleared: an absorbed segment is
/// not a completion, so the registrant is still mid-accumulation and the
/// coexisting RC's own P2*/RC21/RC23 timing action must not be silently
/// discarded unreported.
#[test]
fn absorbed_outcome_preserves_pending_rc_not_cleared() {
    let mut reg = CopRegistrant {
        concat_segments_got: 1,
        ..registrant(0, Some(0x78))
    };

    let result = observe_and_consume_pending_rc_outcome(&mut reg, 0, 0);

    assert!(
        matches!(result, PollMatchResult::Absorbed(1)),
        "an absorbed segment outranks a coexisting pending RC in the same batch"
    );
    assert_eq!(
        reg.pending_rc,
        Some(0x78),
        "ADR-148 Amendment 5: an absorbed segment is not a completion -- the coexisting \
             pending RC must be preserved, not silently discarded like the pre-fix behavior"
    );
}

/// Companion to the test above: on the VERY NEXT poll pass (the
/// `Absorbed` arm's caller always sets `poll_immediately = true`, so this
/// is immediate, not a full poll-interval later), with the wait loop's
/// own cumulative concat baseline now caught up to the live value it just
/// observed, the preserved `pending_rc` correctly surfaces as
/// `PollMatchResult::PendingRc` and IS cleared then -- exactly the
/// existing `PendingRc`-reported-clears-it behavior, just one pass later
/// than it otherwise would have (ADR-148 Amendment 5's accepted
/// residual).
#[test]
fn absorbed_then_next_pass_reports_preserved_pending_rc_and_clears_it() {
    let mut reg = CopRegistrant {
        concat_segments_got: 1,
        ..registrant(0, Some(0x78))
    };

    let first = observe_and_consume_pending_rc_outcome(&mut reg, 0, 0);
    assert!(matches!(first, PollMatchResult::Absorbed(1)));
    assert_eq!(reg.pending_rc, Some(0x78));

    // Next poll pass: the caller's own cumulative concat baseline has
    // caught up to the live value it just observed (simulating
    // `wait_for_expected_response_inner`'s own local counter advancing
    // by the reported `Absorbed` delta), so no further Absorbed delta is
    // reported this time -- the preserved `pending_rc` surfaces instead.
    let concat_baseline = reg.concat_segments_got;
    let second = observe_and_consume_pending_rc_outcome(&mut reg, 0, concat_baseline);

    assert!(
        matches!(second, PollMatchResult::PendingRc(0x78)),
        "the preserved pending RC must surface on the very next poll pass"
    );
    assert_eq!(
        reg.pending_rc, None,
        "reporting PendingRc must consume it, same as the normal PendingRc case"
    );
}

/// Required test (pins the unchanged ADR-101 §C behavior explicitly by
/// name, distinct from `coexisting_match_and_pending_rc_reports_matched_and_still_clears_pending_rc`
/// above, which already covers this exact scenario): a `matches_got`
/// delta above baseline coexisting with a `Some` `pending_rc` still
/// reports `Matched` and still clears `pending_rc` -- ADR-148 Amendment 5
/// only changes the `Absorbed` case, not this one.
#[test]
fn matched_outcome_still_clears_pending_rc_unchanged_by_amendment_5() {
    let mut reg = registrant(1, Some(0x78));

    let result = observe_and_consume_pending_rc_outcome(&mut reg, 0, 0);

    assert!(matches!(result, PollMatchResult::Matched(1, 0)));
    assert_eq!(
        reg.pending_rc, None,
        "Matched must still unconditionally clear pending_rc, per ADR-101 Decision §C, \
             untouched by ADR-148 Amendment 5's Absorbed-only preservation"
    );
}

/// Edge-case-hunter finding on this Amendment's own review round,
/// empirically confirmed (not just traced): a `pending_rc` preserved by
/// the `Absorbed` fix above is NOT safely "bounded" by
/// `CONCAT_MAX_BUF_BYTES`/`CONCAT_MAX_BUF_SEGMENTS` the way an earlier
/// draft of ADR-148 Amendment 5's Consequences section implied --
/// hitting either cap force-finalizes the buffer via
/// `finalize_one_concat_buffer` from INSIDE `bind_registrant` itself,
/// which increments `matches_got` synchronously in the same pass, so
/// the eventual `observe_and_consume_pending_rc_outcome` call reports
/// `Matched` (not `Absorbed`) -- and `Matched` still unconditionally
/// clears `pending_rc`. A cap-triggered finalize therefore silently
/// discards a coexisting RC exactly like the pre-Amendment-5 behavior
/// did, just one step removed. See the ADR's second accepted-residual
/// bullet (same-ECU cap-triggered `Matched`).
#[test]
fn cap_triggered_matched_still_clears_a_coexisting_pending_rc() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![ExpectedResponse {
            mask: vec![0xFF],
            pattern: vec![0x62],
            unique_resp_ids: Vec::new(),
            acceptance_id: 77,
        }],
        matches_needed: Some(1),
        concat_enabled: true,
        concat: vec![ConcatBuf {
            key: (0, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 0,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0xAA; CONCAT_MAX_BUF_BYTES], // already sitting at the cap
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: 1,
        }],
        // The same ECU also has an outstanding pending RC on this
        // registrant, coexisting with the open (about-to-cap-finalize)
        // buffer -- the scenario the ADR's second residual describes.
        // This module's local `registrant` helper takes
        // (matches_got, pending_rc); start matches_got at 0 since the
        // cap-triggered finalize inside bind_registrant is what should
        // advance it to 1, not the constructor.
        ..registrant(0, Some(0x78))
    }];
    let mut finalized = Vec::new();

    let result = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xFF], // one more byte pushes data.len() over the cap
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        result,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(
        registrants[0].matches_got, 1,
        "the cap-triggered force-finalize counts as one completed match"
    );
    assert_eq!(
        registrants[0].pending_rc,
        Some(0x78),
        "bind_registrant's cap-triggered finalize does not itself touch pending_rc"
    );

    // The wait loop's next observe-and-consume call sees matches_got
    // advance past its baseline -- reports Matched, per
    // check_match_against_baseline's ranking -- and still clears
    // pending_rc unconditionally, discarding the RC this ECU's own
    // response was still outstanding for.
    //
    // ADR-148 Amendment 8 (Codex round-9 finding): the cap-triggered
    // finalize also absorbed one segment into the concat buffer (via
    // `bind_registrant`'s absorb path) BEFORE the cap forced the finalize
    // that advanced `matches_got` -- so `registrants[0].concat_segments_got`
    // is `1` at this point. `Matched`'s second field, `1`, is exactly
    // that concat-segment delta, now correctly reported instead of
    // silently dropped: a caller correctly syncs its own
    // `concat_segments_got` baseline to `1` here and does NOT see a
    // phantom `Absorbed` report (and a needless extra `CP_P2Max` restart)
    // on the next poll pass.
    let outcome = observe_and_consume_pending_rc_outcome(&mut registrants[0], 0, 0);
    assert!(matches!(outcome, PollMatchResult::Matched(1, 1)));
    assert_eq!(
        registrants[0].pending_rc, None,
        "confirms the ADR's second accepted residual: a cap-triggered Matched still \
             discards a coexisting pending RC, even though the ECU did not actually finish \
             answering -- it was truncated by this codebase's own internal cap"
    );
}
