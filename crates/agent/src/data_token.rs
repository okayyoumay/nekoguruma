//! The session token of a data channel client, shared by the uploader and the fetcher (design 6.4,
//! 14.3, ADR-493 item 10, ADR-497 item 6): each exchanges a token of its own, only while it has
//! work, and each repeats a request once after a 401.
//!
//! [`Holder`] keeps the token and the exchange's log lines (in the owner's words: the prefix names
//! the client), and [`Holder::unauthorized`] is the 401 rule. The callers keep what differs: how
//! a failed exchange moves their back-off, and what a stop does.

use std::collections::HashSet;

use crate::control::exchange::{self, Fail, Token};
use crate::uploader::routes::{Class, Failure};

/// The exchange's log lines: one warning per class until a token is issued, debug after that, and
/// one warning per refusal until a token is issued again. The prefix names the client in each
/// line.
pub(crate) struct ExchangeLog {
    prefix: &'static str,
    seen: HashSet<&'static str>,
    refused: bool,
}

impl ExchangeLog {
    fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            seen: HashSet::new(),
            refused: false,
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
}
