use super::*;

/// A created-receive-only (`NumSendCycles == 0`) tier-2 registrant with a
/// finite target count and a `cyclic_deadline` -- the ADR-182-widened
/// finite-`N` shape `reap_expired_cyclic_decision`'s expiry arm applies its
/// `emit_timeout_error: true` branch to. `matches_needed`/`matches_got`
/// default to "target 2, none matched yet" so callers only need to override
/// what a given test actually varies.
fn finite_n_registrant(cop_handle: u32, deadline: tokio::time::Instant) -> CopRegistrant {
    CopRegistrant {
        cop_handle,
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

/// `(now, expired_deadline)`: `expired_deadline` is already in the past
/// relative to `now`, with `now` itself usable as an already-sound
/// watermark (`>= expired_deadline`).
fn now_and_expired_deadline() -> (tokio::time::Instant, tokio::time::Instant) {
    let base = tokio::time::Instant::now();
    let deadline = base;
    let now = base + Duration::from_millis(10);
    (now, deadline)
}

/// Edge-case-hunter regression (ADR-182 follow-up fix): the core assertion
/// this fix exists for -- a registrant that is BOTH cancelled AND past a
/// sound, expired `cyclic_deadline` must be left alone (`None`), never
/// reaped via the expiry arm. Before the fix, `cancelled` was not consulted
/// at all here, so this exact input reaped it as `Some(true)` (finite-N
/// expiry error path) instead.
#[test]
fn cancelled_registrant_past_a_sound_expired_deadline_is_left_alone() {
    let (now, deadline) = now_and_expired_deadline();
    let r = finite_n_registrant(100, deadline);
    assert_eq!(
        reap_expired_cyclic_decision(&r, now, true, Some(now), None, false),
        None,
        "a cancelled registrant must never be reaped here, even with an otherwise-sound expiry"
    );
}

/// Same setup, `cancelled: false`: the expiry arm must still fire normally
/// -- the new skip-guard must not swallow the ordinary, uncancelled case.
#[test]
fn uncancelled_registrant_past_a_sound_expired_deadline_is_reaped_as_timeout() {
    let (now, deadline) = now_and_expired_deadline();
    let r = finite_n_registrant(100, deadline);
    assert_eq!(
        reap_expired_cyclic_decision(&r, now, false, Some(now), None, false),
        Some(true),
        "an uncancelled finite-N registrant past a sound expired deadline must be reaped with \
         emit_timeout_error: true"
    );
}

/// The `-1` (`matches_needed: None`) subtype's own expiry arm stays
/// FINISHED-only (`emit_timeout_error: false`), unchanged by this fix --
/// only the cancellation skip-guard is new behavior for it too (see the
/// next test).
#[test]
fn uncancelled_is_cyclic_registrant_past_a_sound_expired_deadline_is_reaped_without_error() {
    let (now, deadline) = now_and_expired_deadline();
    let r = CopRegistrant {
        matches_needed: None,
        ..finite_n_registrant(100, deadline)
    };
    assert_eq!(
        reap_expired_cyclic_decision(&r, now, false, Some(now), None, false),
        Some(false),
        "the -1 subtype's own expiry arm must stay FINISHED-only, unaffected by this fix"
    );
}

/// The `-1` subtype's own gap this fix closes too (see ADR-182 follow-up
/// fix doc comment: "this fix closes the same-tick race window for both
/// subtypes uniformly"): a cancelled `-1` registrant past a sound expired
/// deadline must also be left alone, not reaped as a (pre-fix, harmless for
/// `-1`) FINISHED.
#[test]
fn cancelled_is_cyclic_registrant_past_a_sound_expired_deadline_is_left_alone() {
    let (now, deadline) = now_and_expired_deadline();
    let r = CopRegistrant {
        matches_needed: None,
        ..finite_n_registrant(100, deadline)
    };
    assert_eq!(
        reap_expired_cyclic_decision(&r, now, true, Some(now), None, false),
        None,
        "a cancelled -1 registrant must never be reaped here either, closing the same window \
         uniformly for both subtypes"
    );
}

/// Count-completion is checked before -- and independently of -- the
/// cancellation skip-guard's own expiry branch, but the skip-guard itself
/// gates BOTH arms (per this fix's own doc comment: "before applying
/// either... skip it if cancelled"): a cancelled registrant whose target
/// count has already been reached must also be left alone, not reaped via
/// count-completion.
#[test]
fn cancelled_registrant_with_count_already_reached_is_left_alone() {
    let (now, deadline) = now_and_expired_deadline();
    let r = CopRegistrant {
        matches_got: 2, // == matches_needed
        ..finite_n_registrant(100, deadline)
    };
    assert_eq!(
        reap_expired_cyclic_decision(&r, now, true, Some(now), None, false),
        None,
        "a cancelled registrant must never be reaped here, even with its target count already \
         reached"
    );
}

/// Same setup, `cancelled: false`: count-completion must still fire
/// normally and win over the (also-satisfied) expiry condition, per this
/// function's own precedence (count-completion checked first, no
/// `emit_timeout_error`).
#[test]
fn uncancelled_registrant_with_count_already_reached_is_reaped_via_count_completion() {
    let (now, deadline) = now_and_expired_deadline();
    let r = CopRegistrant {
        matches_got: 2, // == matches_needed
        ..finite_n_registrant(100, deadline)
    };
    assert_eq!(
        reap_expired_cyclic_decision(&r, now, false, Some(now), None, false),
        Some(false),
        "count-completion must win over the also-satisfied expiry condition, with no timeout \
         error"
    );
}

/// A cancelled registrant whose deadline has NOT yet expired (or is not yet
/// soundly observable) must also be left alone -- the same `None` outcome
/// an uncancelled-but-not-yet-eligible registrant already gets, just via a
/// different arm of the decision.
#[test]
fn cancelled_registrant_not_yet_expired_is_left_alone() {
    let base = tokio::time::Instant::now();
    let deadline = base + Duration::from_millis(200);
    let r = finite_n_registrant(100, deadline);
    assert_eq!(
        reap_expired_cyclic_decision(&r, base, true, Some(base), None, false),
        None
    );
}

/// An uncancelled registrant whose deadline has expired but is not yet
/// SOUNDLY observable (`is_cyclic_reap_sound` false -- e.g. an un-caught-up
/// UUDT companion watermark, ADR-101 Decision §E) must still be left alone
/// this pass, cancelled or not -- the cancellation skip-guard does not
/// bypass the existing soundness gate.
#[test]
fn uncancelled_registrant_expired_but_not_yet_sound_is_left_alone() {
    let (now, deadline) = now_and_expired_deadline();
    let r = finite_n_registrant(100, deadline);
    assert_eq!(
        reap_expired_cyclic_decision(
            &r,
            now,
            false,
            Some(now),
            Some(deadline - Duration::from_millis(1)),
            true
        ),
        None,
        "a stale UUDT companion watermark must still defer reaping, unaffected by this fix"
    );
}
