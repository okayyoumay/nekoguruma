use super::*;

/// A full `MAX_POLL_MESSAGES`-sized batch proves nothing about queue
/// exhaustion -- classified `MaybeMore`, not `Drained`. This is the exact
/// distinction the bug this ADR amendment closes hinges on: a stale
/// "a poll ran" signal could not tell this case apart from a
/// genuinely-exhausted queue.
#[test]
fn a_full_batch_is_maybe_more() {
    assert_eq!(
        classify_poll_batch(MAX_POLL_MESSAGES),
        PollOutcome::MaybeMore
    );
}

/// Any batch strictly smaller than `MAX_POLL_MESSAGES` -- including an
/// empty one, the buffer-empty error arm's own effective batch size --
/// proves the adapter's RX queue was empty at that instant (SAE
/// J2534-1's own `Timeout = 0` `PassThruReadMsgs` semantics).
#[test]
fn a_short_batch_is_drained() {
    assert_eq!(
        classify_poll_batch(MAX_POLL_MESSAGES - 1),
        PollOutcome::Drained
    );
    assert_eq!(classify_poll_batch(0), PollOutcome::Drained);
}

/// `is_ok` mirrors the pre-this-change `bool` return exactly: `false`
/// only for `HardError`, regardless of exhaustiveness -- every
/// mechanically-updated `if !poll_rx_inner(...).await.is_ok() { ... }`
/// call site depends on this being unaffected by the exhaustiveness
/// split this change introduces.
#[test]
fn is_ok_is_false_only_for_hard_error() {
    assert!(PollOutcome::Drained.is_ok());
    assert!(PollOutcome::MaybeMore.is_ok());
    assert!(!PollOutcome::HardError.is_ok());
}

/// `is_drained` -- the exhaustiveness signal `poll_rx_inner`'s own
/// end-of-pass block consumes to decide whether to stamp
/// `ctx.drain_watermarks[ctx.channel_id]` (ADR-101 Decision §E) -- is
/// `true` for exactly one variant.
#[test]
fn is_drained_is_true_only_for_drained() {
    assert!(PollOutcome::Drained.is_drained());
    assert!(!PollOutcome::MaybeMore.is_drained());
    assert!(!PollOutcome::HardError.is_drained());
}
