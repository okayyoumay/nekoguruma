//! The reconnection delay of the control channel session (design 5.3, ADR-471).
//!
//! The delay starts at 1 second. Every wait that is taken doubles it, up to 60 seconds. It goes
//! back to 1 second only when a connection lived 60 seconds, counted from `hello` to the last
//! frame received (never to the end, so a silent tail does not count), and a refusal that wants
//! the cap sets it to 60 seconds. There is no jitter yet.

use std::time::Duration;

use tokio::time::Instant;

/// The first delay, and the one a connection that lived long enough returns to.
pub(super) const INITIAL: Duration = Duration::from_secs(1);
/// The longest delay.
pub(super) const CAP: Duration = Duration::from_secs(60);
/// How long a connection must have lived, from `hello` to its last frame, for the delay to
/// reset.
pub(super) const LIVED: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(super) struct Backoff {
    delay: Duration,
}

impl Backoff {
    pub(super) fn new() -> Self {
        Self { delay: INITIAL }
    }

    /// The delay the next wait uses.
    pub(super) fn delay(&self) -> Duration {
        self.delay
    }

    /// After a wait was taken: double, up to the cap.
    pub(super) fn after_wait(&mut self) {
        self.delay = (self.delay * 2).min(CAP);
    }

    /// A connection lived long enough.
    pub(super) fn reset(&mut self) {
        self.delay = INITIAL;
    }

    /// A refusal that backs off to the cap.
    pub(super) fn cap(&mut self) {
        self.delay = CAP;
    }

    /// How long the next wait is: the delay, or a longer `Retry-After` the server named.
    pub(super) fn wait(&self, retry_after_secs: Option<u32>) -> Duration {
        match retry_after_secs {
            Some(secs) => self.delay.max(Duration::from_secs(u64::from(secs))),
            None => self.delay,
        }
    }
}

/// Whether a connection lived: from `hello` to the last frame received.
pub(super) fn lived(hello_at: Instant, last_frame_at: Instant) -> bool {
    last_frame_at.saturating_duration_since(hello_at) >= LIVED
}
