use super::*;

fn matched_count(result: PollMatchResult) -> Option<u32> {
    match result {
        PollMatchResult::Matched(count, _) => Some(count),
        _ => None,
    }
}

/// Basic correctness: a live count above baseline reports the delta.
#[test]
fn reports_the_delta_above_baseline() {
    let result = check_match_against_baseline(3, None, 1, 0, 0);
    assert_eq!(matched_count(result), Some(2));
}

/// ADR-148 (Codex round-9 finding): when a completing match and a
/// concat-segment absorb land in the same pass, `Matched` must report
/// BOTH deltas, not just the matches one -- otherwise the caller's own
/// concat-segment baseline falls behind the live registrant and a
/// following call phantom-reports the already-accounted-for delta as a
/// fresh `Absorbed`, needlessly restarting the CP_P2Max deadline again.
#[test]
fn matched_also_reports_a_coexisting_concat_segment_delta() {
    let result = check_match_against_baseline(1, None, 0, 3, 1);
    assert!(matches!(result, PollMatchResult::Matched(1, 2)));
}

/// A pending RC is reported only when the live count is at or below
/// baseline -- a genuine match always outranks a pending RC in the same
/// pass (mirrors `PollMatchResult`'s own doc comment).
#[test]
fn pending_rc_reported_only_when_no_match_above_baseline() {
    assert!(matches!(
        check_match_against_baseline(0, Some(0x78), 0, 0, 0),
        PollMatchResult::PendingRc(0x78)
    ));
    assert!(matches!(
        check_match_against_baseline(1, Some(0x78), 0, 0, 0),
        PollMatchResult::Matched(1, 0)
    ));
}

/// Required test 4 (ADR-101 Decision §B mechanism): a companion-channel
/// -style increment landing between two calls is correctly observed by
/// a cumulative baseline that only advances when THIS wait loop's own
/// calls report a match -- exactly what `wait_for_expected_response_inner`
/// threads in as `matches_got` (the loop's own local counter).
#[test]
fn cumulative_baseline_observes_an_increment_from_another_pass_between_calls() {
    let mut local_matches_got = 0u32;

    // Call 1: nothing yet.
    let r1 = check_match_against_baseline(0, None, local_matches_got, 0, 0);
    assert!(matches!(r1, PollMatchResult::NoMatch));

    // Between call 1 and call 2, a companion-channel pass's own
    // writeback lands (ADR-101 Decision §A), merging its own +1
    // contribution directly onto the live registrant -- entirely
    // outside this loop's own immediately-preceding call.
    let live_matches_got_after_companion = 1;

    // Call 2: this loop's own primary-channel pass finds nothing
    // further this tick, but the companion's contribution is still
    // visible because the baseline never silently absorbed it.
    let r2 = check_match_against_baseline(
        live_matches_got_after_companion,
        None,
        local_matches_got,
        0,
        0,
    );
    match r2 {
        PollMatchResult::Matched(count, _) => local_matches_got += count,
        other => {
            panic!("expected the companion-channel increment to be reported, got {other:?}")
        }
    }

    assert_eq!(
        local_matches_got, 1,
        "the wait loop's own cumulative counter must observe a contribution merged in by \
             another pass between two of its own calls"
    );
}

/// Fail-without-the-fix control for test 4: a fresh-per-call baseline
/// (the pre-ADR-101 behavior -- re-read directly from the live
/// registrant at the START of each call) already includes the
/// companion-channel contribution by the time call 2 starts, so the
/// SAME live reading that test 4 above correctly reports as `Matched(1)`
/// is instead silently absorbed and reported as `NoMatch`.
#[test]
fn a_fresh_per_call_baseline_would_have_silently_absorbed_the_same_increment() {
    let live_matches_got_after_companion = 1;
    // "Fresh re-read at call start" == the live value already includes
    // the companion's own contribution before this call's own pass ran.
    let stale_baseline = live_matches_got_after_companion;

    let r2 =
        check_match_against_baseline(live_matches_got_after_companion, None, stale_baseline, 0, 0);

    assert!(
        matches!(r2, PollMatchResult::NoMatch),
        "documents the bug ADR-101 Decision §B fixes: a same-value re-read baseline \
             silently swallows a concurrently-merged increment"
    );
}
