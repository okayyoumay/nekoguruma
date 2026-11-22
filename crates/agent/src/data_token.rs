//! The session token of a data channel client, shared by the uploader and the fetcher (design 6.4,
//! 14.3, ADR-493 item 10, ADR-497 item 6): each exchanges a token of its own, only while it has
//! work, and each repeats a request once after a 401.
//!
//! [`Holder`] keeps the token and the exchange's log lines (in the owner's words: the prefix names
//! the client), [`Holder::unauthorized`] is the 401 rule and [`Holder::unproven`] the rule for a
//! data route's answer the server did not prove. The callers keep what differs: how
//! a failed exchange moves their back-off, and what a stop does.

use std::collections::HashSet;

use crate::answer_proof::UNPROVEN_CAUSES;
use crate::control::exchange::{self, Fail, Token};
use crate::uploader::routes::{Class, Failure};

/// The exchange's log lines: one warning per class until a token is issued, debug after that, and
/// one warning per refusal until a token is issued again. A data route's unproven answers are the
/// class `unproven`, which clears only when a data route's answer is proved (ADR-504): each of
/// them drops the token, so a token issued says nothing of whether the server answering is the
/// real one. The prefix names the client in each line.
pub(crate) struct ExchangeLog {
    prefix: &'static str,
    seen: HashSet<&'static str>,
    refused: bool,
    /// An unproven data route answer was warned about and no answer has been proved since.
    answer_unproven: bool,
}

impl ExchangeLog {
    fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            seen: HashSet::new(),
            refused: false,
            answer_unproven: false,
        }
    }

    /// The line for an answer that is not proved: a warning when `warn`, else debug.
    fn log_unproven(prefix: &str, warn: bool, class: &'static str, status: u16) {
        if warn {
            tracing::warn!(
                class,
                status,
                causes = UNPROVEN_CAUSES,
                "{prefix} answer is not proved by the server"
            );
        } else {
            tracing::debug!(
                class,
                status,
                causes = UNPROVEN_CAUSES,
                "{prefix} answer is not proved by the server"
            );
        }
    }
}

impl exchange::Log for ExchangeLog {
    fn failure(&mut self, class: &'static str, status: Option<u16>) {
        let prefix = self.prefix;
        if self.seen.insert(class) {
            tracing::warn!(class, status = ?status, "{prefix} exchange attempt failed");
        } else {
            tracing::debug!(class, status = ?status, "{prefix} exchange attempt failed");
        }
    }

    fn refusal(&mut self, class: &'static str, status: u16) {
        let prefix = self.prefix;
        if self.refused {
            tracing::debug!(class, status, "{prefix} exchange refused again");
        } else {
            self.refused = true;
            tracing::warn!(class, status, "{prefix} exchange refused");
        }
    }

    fn unproven(&mut self, class: &'static str, status: u16) {
        let warn = self.seen.insert(class);
        Self::log_unproven(self.prefix, warn, class, status);
    }

    fn accepted(&mut self) {
        self.seen.clear();
        self.refused = false;
    }

    fn stale_challenge(&mut self) {
        let prefix = self.prefix;
        tracing::debug!("{prefix} challenge was not live; asking for a new one");
    }
}

/// The token of one client, and its exchange's log.
pub(crate) struct Holder {
    token: Option<Token>,
    log: ExchangeLog,
}

impl Holder {
    /// A holder with no token whose log lines begin with `prefix` (`uploader`, `fetch`).
    pub(crate) fn new(prefix: &'static str) -> Self {
        Self {
            token: None,
            log: ExchangeLog::new(prefix),
        }
    }

    /// The token's text, exchanged now when there is none.
    pub(crate) async fn ensure(&mut self, seams: &exchange::Seams<'_>) -> Result<String, Fail> {
        if let Some(token) = &self.token {
            return Ok(token.as_str().to_owned());
        }
        let token = exchange::token(seams, &mut self.log).await?;
        let text = token.as_str().to_owned();
        self.token = Some(token);
        Ok(text)
    }

    /// Drops the token: nothing is held while there is no work.
    pub(crate) fn clear(&mut self) {
        self.token = None;
    }

    /// The rule for a request answered 401: the token is dropped, and `true` says to exchange
    /// again and repeat the request once. A second 401 (`repeated` already set) is a retry:
    /// `failure`'s class becomes [`Class::Retry`] and the answer is `false`.
    pub(crate) fn unauthorized(&mut self, repeated: &mut bool, failure: &mut Failure) -> bool {
        self.token = None;
        if *repeated {
            failure.class = Class::Retry;
            return false;
        }
        *repeated = true;
        true
    }

    /// The rule for a data route's answer that was to be a result and is not proved by the server
    /// (ADR-504): the token is dropped, so that the next request exchanges a new one, and the
    /// answer is logged as the class `unproven`, one warning until [`Holder::proved`]. No repeat
    /// follows; the caller waits the back-off's cap.
    pub(crate) fn unproven(&mut self, status: Option<u16>) {
        self.token = None;
        let warn = !self.log.answer_unproven;
        self.log.answer_unproven = true;
        // The status of a result is always known; 0 never matches one.
        ExchangeLog::log_unproven(self.log.prefix, warn, "unproven", status.unwrap_or(0));
    }

    /// A data route's answer was proved: the class `unproven` warns again at its next answer.
    pub(crate) fn proved(&mut self) {
        self.log.answer_unproven = false;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use exchange::Log;

    /// A log writer that every event is appended to.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The lines `act` logs at debug level and above, one string each.
    fn logged(act: impl FnOnce()) -> Vec<String> {
        let out = Sink::default();
        let writer = out.clone();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .finish(),
        );
        // Another test's thread may have cached a call site as disabled before this subscriber
        // existed.
        tracing::callsite::rebuild_interest_cache();
        act();
        drop(guard);
        let text = String::from_utf8_lossy(&out.0.lock().unwrap()).into_owned();
        text.lines().map(str::to_owned).collect()
    }

    /// An unproven answer is one warning per class until a token is issued, debug after that.
    #[test]
    fn an_unproven_answer_warns_once_and_again_only_after_a_token() {
        let mut log = ExchangeLog::new("fetch");
        let lines = logged(|| {
            log.unproven("session-unproven", 200);
            log.unproven("session-unproven", 200);
            log.accepted();
            log.unproven("session-unproven", 200);
        });
        let levels: Vec<&str> = lines
            .iter()
            .map(|line| {
                ["WARN", "DEBUG"]
                    .into_iter()
                    .find(|level| line.contains(&format!(" {level} ")))
                    .expect("a warn or debug line")
            })
            .collect();
        assert_eq!(levels, ["WARN", "DEBUG", "WARN"], "{lines:?}");
        assert!(lines.iter().all(|line| line.contains("session-unproven")));
    }

    /// A data route's unproven answers drop the token each time, so a new token is issued between
    /// them; the class warns once until an answer is proved, and the exchange's classes clear with
    /// each token as before.
    #[test]
    fn the_holders_unproven_answers_warn_once_until_an_answer_is_proved() {
        let mut holder = Holder::new("uploader");
        let lines = logged(|| {
            holder.unproven(Some(200));
            // A token is issued: the exchange's class clears, the answers' does not.
            holder.log.accepted();
            holder.unproven(Some(409));
            holder.log.unproven("session-unproven", 200);
            holder.log.accepted();
            holder.unproven(Some(200));
            holder.proved();
            holder.log.accepted();
            holder.unproven(Some(200));
        });
        let warned: Vec<bool> = lines.iter().map(|line| line.contains(" WARN ")).collect();
        assert_eq!(warned, [true, false, true, false, true], "{lines:?}");
        assert!(lines.iter().all(|line| line.contains("uploader answer")));
        assert!(lines[0].contains(UNPROVEN_CAUSES), "{lines:?}");
        assert!(lines[2].contains("session-unproven"), "{lines:?}");
    }
}
