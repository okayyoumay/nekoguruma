use super::*;

/// Generation matches, seq matches, AND live policy is on: a `Suspend`
/// classification applies. `entry_set_seq_at_read`/`live_error_set_seq`
/// are irrelevant to `Suspend` -- left mismatched (`Some(0)` vs `99`) to
/// prove it.
#[test]
fn suspend_applies_when_generation_seq_and_policy_all_match() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Suspend),
            1,
            1,
            Some(1),
            1,
            Some(0),
            99,
            true
        ),
        Some(QueueErrorClass::Suspend)
    );
}

/// `connect_generation` mismatch (a reconnect completed since this
/// pass's snapshot was taken) discards a `Suspend` classification even
/// though the seq and live policy still match.
#[test]
fn suspend_discarded_on_generation_mismatch() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Suspend),
            1,
            2,
            Some(1),
            1,
            Some(0),
            0,
            true
        ),
        None,
        "a stale generation must discard the classification regardless of seq/policy"
    );
}

/// Seq mismatch (an explicit resume/reset/promotion/hard-error bumped
/// `error_clear_seq` since this `Suspend` classification's fold captured
/// `entry_suspend_seq`) discards a `Suspend` classification even though
/// the generation still matches -- the client-reaction race the third
/// amendment closed; unchanged by the fifth/sixth amendments (this
/// direction's anchor/capture point never moved).
#[test]
fn suspend_discarded_on_seq_mismatch() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Suspend),
            1,
            1,
            Some(1),
            2,
            Some(0),
            0,
            true
        ),
        None,
        "a stale seq must discard the classification regardless of generation/policy"
    );
}

/// `entry_suspend_seq == None` for a `Suspend` classification should
/// never happen by construction (every `Suspend` fold captures one at
/// `poll_rx_inner`'s call site), but is treated as stale defensively --
/// never applied.
#[test]
fn suspend_discarded_when_fold_seq_was_never_captured() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Suspend),
            1,
            1,
            None,
            1,
            Some(0),
            0,
            true
        ),
        None,
        "a Suspend classification with no captured fold-seq must never apply"
    );
}

/// Generation and seq both match, but live policy is off: a `Suspend`
/// classification is discarded -- the one remaining asymmetric
/// dimension, folded into this function by the fourth amendment
/// restructure (unaffected by the fifth/sixth).
#[test]
fn suspend_discarded_when_live_policy_is_off() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Suspend),
            1,
            1,
            Some(1),
            1,
            Some(0),
            0,
            false
        ),
        None,
        "Suspend must not apply when live CP_SuspendQueueOnError is 0, even with a fresh seq"
    );
}

/// Cross-check: `Suspend` must be genuinely indifferent to
/// `entry_set_seq_at_read`/`live_error_set_seq` -- applies even when
/// they mismatch, since that counter belongs entirely to `Positive`'s
/// own anchor (design-advisor's trace: no scenario needs `Suspend`
/// checked against `error_set_seq`).
#[test]
fn suspend_ignores_error_set_seq_and_set_seq_at_read() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Suspend),
            1,
            1,
            Some(1),
            1,
            Some(1),
            99,
            true
        ),
        Some(QueueErrorClass::Suspend)
    );
}

/// `Positive` needs no live-policy check at all -- it applies purely on
/// generation and batch-read-seq matching, even with the live-policy
/// argument `false`. This is the discriminating case proving the policy
/// check is `Suspend`-only.
#[test]
fn positive_applies_regardless_of_live_policy_value() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Positive),
            1,
            1,
            None,
            0,
            Some(5),
            5,
            false
        ),
        Some(QueueErrorClass::Positive)
    );
}

/// `connect_generation` mismatch discards a `Positive` classification
/// too -- symmetric with `Suspend` on this check, unchanged from the
/// third amendment.
#[test]
fn positive_discarded_on_generation_mismatch() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Positive),
            1,
            2,
            None,
            0,
            Some(5),
            5,
            true
        ),
        None
    );
}

/// THE critical discriminating test (design-advisor's own): a `Positive`
/// whose containing batch was READ (`entry_set_seq_at_read`) BEFORE an
/// intervening timeout's bump of `error_set_seq` must be discarded, even
/// though nothing about FOLD timing is modeled here at all -- there is
/// no fold-time parameter for `Positive` anymore. `entry_set_seq_at_read
/// == Some(1)` simulates a batch whose pre-read capture ran while
/// `error_set_seq` was still `1`; `live_error_set_seq == 2` simulates a
/// timeout bumping it once, AFTER that capture (whether or not also
/// after the read itself completed -- the sixth amendment's inequality
/// argument covers both). Only batch-anchored comparison gets this right
/// -- a naive fold-time-only capture (unified or not) could not, since
/// it has no way to distinguish "captured from evidence that arrived
/// before the bump" from "folded after the bump merely because of
/// unrelated processing order within the same pass".
#[test]
fn positive_discarded_when_batch_was_read_before_a_timeout_bump() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Positive),
            1,
            1,
            None,
            0,
            Some(1),
            2,
            true
        ),
        None,
        "a Positive from a batch read before a timeout's bump must be discarded, not \
             applied -- applying it would wrongly clear the flag the timeout just set"
    );
}

/// Genuine-recovery direction: a `Positive` from a batch READ AT OR
/// AFTER the current `error_set_seq` (a later `PassThruReadMsgs` call,
/// reflecting wire content sent after any timeout) correctly applies.
#[test]
fn positive_applies_when_batch_was_read_at_current_error_set_seq() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Positive),
            1,
            1,
            None,
            0,
            Some(2),
            2,
            true
        ),
        Some(QueueErrorClass::Positive),
        "a Positive from a batch read at the current error_set_seq must still apply -- \
             it is not stale"
    );
}

/// ADR-147 sixth amendment: a `Positive` classification whose CLL was
/// absent from the pre-read snapshot (`entry_set_seq_at_read == None`)
/// must always be discarded, even when `live_error_set_seq` happens to
/// be the "obvious" default `0` a naive `unwrap_or(0)` might have
/// produced -- there is no valid batch-anchor to compare, so this is
/// conservative-by-construction, not a coincidental match.
#[test]
fn positive_discarded_when_set_seq_at_read_is_none() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Positive),
            1,
            1,
            None,
            0,
            None,
            0,
            true
        ),
        None,
        "a Positive with no valid pre-read batch-anchor must never apply"
    );
}

/// Cross-check: `Positive` must be genuinely indifferent to
/// `entry_suspend_seq`/`live_error_clear_seq` -- applies even when they
/// mismatch, since that counter belongs entirely to `Suspend`'s own
/// anchor (design-advisor's trace: no scenario needs `Positive` checked
/// against `error_clear_seq`).
#[test]
fn positive_ignores_error_clear_seq_and_suspend_seq() {
    assert_eq!(
        queue_error_class_to_apply(
            Some(QueueErrorClass::Positive),
            1,
            1,
            Some(1),
            99,
            Some(5),
            5,
            true
        ),
        Some(QueueErrorClass::Positive)
    );
}

/// No classification at all this pass: always discarded, independent of
/// generation/seq/policy.
#[test]
fn none_classification_never_applies() {
    assert_eq!(
        queue_error_class_to_apply(None, 1, 1, Some(1), 1, Some(0), 0, true),
        None
    );
}
