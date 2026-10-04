use super::*;

/// `(before, deadline, after)`: three instants `deadline_ms` apart,
/// anchored to `tokio::time::Instant::now()` -- `before < deadline <
/// after`.
fn instants() -> (
    tokio::time::Instant,
    tokio::time::Instant,
    tokio::time::Instant,
) {
    let base = tokio::time::Instant::now();
    let deadline = base + Duration::from_millis(10);
    let after = base + Duration::from_millis(20);
    (base, deadline, after)
}

/// Single-channel CLL (`has_companion: false`, the pre-ADR-101-§E
/// behavior): the companion watermark argument is irrelevant, and only
/// the primary watermark needs to have caught up to the deadline.
#[test]
fn single_channel_needs_only_the_primary_watermark() {
    let (before, dl, after) = instants();
    assert!(
        !is_cyclic_reap_sound(dl, Some(before), None, false),
        "primary watermark stale (predates the deadline) -> not sound"
    );
    assert!(
        !is_cyclic_reap_sound(dl, None, None, false),
        "no primary watermark recorded at all -> not sound"
    );
    assert!(
        is_cyclic_reap_sound(dl, Some(after), None, false),
        "primary watermark caught up -> sound, regardless of the (irrelevant) companion arg"
    );
    assert!(
        is_cyclic_reap_sound(dl, Some(after), Some(before), false),
        "a stale companion watermark must not matter when has_companion is false"
    );
}

/// Dual-channel CLL (`has_companion: true`): BOTH the primary and the
/// companion watermark must be `>= dl` -- primary-only is exactly the
/// round-6 bug ADR-101 Decision §E fixes (a companion response could
/// still be un-polled while the primary alone looked drained).
#[test]
fn dual_channel_requires_both_watermarks_to_catch_up() {
    let (before, dl, after) = instants();

    // Primary caught up, companion stale or never recorded.
    assert!(!is_cyclic_reap_sound(dl, Some(after), Some(before), true));
    assert!(!is_cyclic_reap_sound(dl, Some(after), None, true));

    // Companion caught up, primary stale or never recorded.
    assert!(!is_cyclic_reap_sound(dl, Some(before), Some(after), true));
    assert!(!is_cyclic_reap_sound(dl, None, Some(after), true));

    // Neither caught up.
    assert!(!is_cyclic_reap_sound(dl, Some(before), Some(before), true));

    // Both caught up: sound.
    assert!(is_cyclic_reap_sound(dl, Some(after), Some(after), true));
}

/// A watermark exactly equal to the deadline counts as caught up (`>=`,
/// not a strict `>`) -- matches `is_cyclic_deadline_expired`'s own `now
/// >= dl` convention.
#[test]
fn watermark_equal_to_deadline_is_sound() {
    let (_, dl, _) = instants();
    assert!(is_cyclic_reap_sound(dl, Some(dl), None, false));
    assert!(is_cyclic_reap_sound(dl, Some(dl), Some(dl), true));
}
