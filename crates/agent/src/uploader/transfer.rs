//! One attempt on one entry (ADR-495 item 5): verify the source (once per run, and again after a
//! `failed` completion), begin, status, the parts after the ones the store holds, complete. The
//! attempt ends in one [`Outcome`]; what the loop does with it (the ledger's removal, the
//! warning, the counters) is `Worker::settle`.
//!
//! The same file holds the three things every request of an attempt goes through: the wait the
//! back-off owes ([`Worker::pace`]), the token ([`Worker::ensure_token`], exchanged only when a
//! request is about to need it) and the repeat after a 401 ([`Worker::authed`]).

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::routes::{Class, Completion, Declaration, Failure, Kind, Routes, State, UploadId};
use super::source::{self, PartReader, Verdict};
use super::{Pending, Stopped, Worker, until_stop};
use crate::control::dial::AtPort;
use crate::control::exchange::{self, Fail};
use crate::upload_ledger::Kind as EntryKind;

/// What an attempt came to.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// A stop came: the task ends.
    Stopped,
    /// The server has the object: the entry is removed.
    Delivered,
    /// The agent proved the source broken (the word names how): the entry is removed.
    Abandon(&'static str),
    /// Try again after the back-off.
    Retry(&'static str, Option<u16>),
    /// Leave the entry for this pass.
    PassOver(&'static str, Option<u16>),
    /// Leave the entry until a restart.
    SetAside(&'static str),
}

/// Why a step of an attempt did not give its result.
pub(super) enum Stray {
    Stopped,
    /// A route's failure, after the 401 rule.
    Failure(Failure),
    /// The exchange gave no token; the back-off was told how.
    Exchange,
}

impl From<Stopped> for Stray {
    fn from(_: Stopped) -> Self {
        Stray::Stopped
    }
}

impl Worker {
    /// Waits the back-off when the last attempt owed it, then doubles the delay.
    async fn pace(&mut self) -> Result<(), Stopped> {
        if !self.owe_wait {
            return Ok(());
        }
        let wait = self.backoff.wait(self.retry_after.take());
        until_stop(&self.stop, tokio::time::sleep(wait)).await?;
        self.backoff.after_wait();
        self.owe_wait = false;
        Ok(())
    }

    /// The token, exchanged now when there is none. A failure tells the back-off: the ordinary
    /// delay, a longer `Retry-After`, or the cap.
    async fn ensure_token(&mut self) -> Result<String, Stray> {
        if let Some(token) = &self.token {
            return Ok(token.as_str().to_owned());
        }
        let at = AtPort {
            dial: &*self.dial,
            port: self.port,
        };
        let seams = exchange::Seams {
            dial: &at,
            signer: &*self.signer,
            random: self.random,
            agent_id: &self.agent_id,
            audience: &self.audience,
            host: &self.host,
        };
        let got = until_stop(&self.stop, exchange::token(&seams, &mut self.log)).await?;
        match got {
            Ok(token) => {
                let text = token.as_str().to_owned();
                self.token = Some(token);
                Ok(text)
            }
            Err(how) => {
                // A failed exchange always owes the wait, whatever the attempt stored.
                self.owe_wait = true;
                match how {
                    Fail::Ord => {}
                    Fail::RetryAfter(secs) => self.retry_after = Some(secs),
                    Fail::Cap => self.backoff.cap(),
                }
                Err(Stray::Exchange)
            }
        }
    }

    /// One of the four routes, with the token: a 401 drops the token, exchanges at once and
    /// repeats the request once; a second 401 is a retry.
    async fn authed<T, F>(&mut self, call: impl Fn(Routes, String) -> F) -> Result<T, Stray>
    where
        F: Future<Output = Result<T, Failure>>,
    {
        let mut repeated = false;
        loop {
            self.pace().await?;
            let token = self.ensure_token().await?;
            let answer = until_stop(&self.stop, call(self.routes.clone(), token)).await?;
            let mut failure = match answer {
                Ok(value) => return Ok(value),
                Err(failure) => failure,
            };
            if failure.class == Class::Reauthenticate {
                self.token = None;
                if !repeated {
                    repeated = true;
                    continue;
                }
                failure.class = Class::Retry;
            }
            self.retry_after = failure.retry_after_secs;
            return Err(Stray::Failure(failure));
        }
    }

    /// An answer that says the server has the object. The delay goes back to its start only when
    /// the removal that follows succeeds (`Worker::settle`).
    fn has_it(&mut self) -> Outcome {
        self.progress = true;
        Outcome::Delivered
    }

    fn outcome_of(stray: Stray) -> Outcome {
        match stray {
            Stray::Stopped => Outcome::Stopped,
            Stray::Exchange => Outcome::Retry("exchange", None),
            Stray::Failure(failure) => match failure.class {
                Class::PassOver => Outcome::PassOver(failure.why, failure.status),
                Class::Retry | Class::Reauthenticate => Outcome::Retry(failure.why, failure.status),
            },
        }
    }
}

/// One attempt on `job`.
pub(super) async fn attempt(w: &mut Worker, job: &Pending) -> Outcome {
    // The wait the last attempt owed comes first, whether or not this one makes a request: a
    // source that cannot be read must not spin the loop.
    if w.pace().await.is_err() {
        return Outcome::Stopped;
    }
    w.progress = false;
    match steps(w, job).await {
        Ok(outcome) => outcome,
        Err(stray) => Worker::outcome_of(stray),
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn steps(w: &mut Worker, job: &Pending) -> Result<Outcome, Stray> {
    let entry = &job.entry;
    let seq = entry.seq;
    if !w.verified.contains(&seq) {
        let verdict = source::verify(job.path.clone(), entry.length, entry.sha256, &w.stop).await;
        match verdict {
            Verdict::Verified => {
                w.verified.insert(seq);
            }
            Verdict::Broken(why) => return Ok(Outcome::Abandon(why)),
            Verdict::Transient => return Ok(Outcome::Retry("source-read", None)),
            Verdict::Stopped => return Err(Stray::Stopped),
        }
    }
    let digest = hex(&entry.sha256);
    let declared = Declaration {
        job_id: &entry.key.job_id.0,
        generation: entry.key.generation,
        kind: match entry.kind {
            EntryKind::Result => Kind::Result,
            EntryKind::CommLog => Kind::CommLog,
        },
        size_bytes: entry.length,
        sha256: &digest,
    };
    let declared = &declared;
    let begun = w
        .authed(|r, t| async move { r.begin(&t, declared).await })
        .await?;
    let id = &begun.id;
    match begun.state {
        State::Finished => return Ok(w.has_it()),
        State::Completing => return conclude(w, seq, id).await,
        State::Open | State::Failed => {}
    }
    let status = match w
        .authed(|r, t| async move { r.status(&t, id, entry.length).await })
        .await
    {
        Ok(status) => status,
        // A lost upload and a store outage answer alike: one completion in this attempt tells
        // them apart (ADR-495 item 10).
        Err(Stray::Failure(failure)) if failure.status == Some(503) => {
            return conclude(w, seq, id).await;
        }
        Err(stray) => return Err(stray),
    };
    match status.state {
        State::Finished => return Ok(w.has_it()),
        State::Completing => return conclude(w, seq, id).await,
        State::Failed => {
            w.open_completions.remove(&seq);
            return Ok(failed(w, seq));
        }
        State::Open => {}
    }
    for part in status.parts_held.unwrap_or(0) + 1..=status.part_count {
        // At most 10000 parts (the routes check it).
        let number = u32::try_from(part).unwrap_or(u32::MAX);
        let url = w
            .authed(|r, t| async move { r.part_url(&t, id, number).await })
            .await?;
        let offset = (part - 1) * status.part_bytes;
        let length = status.part_bytes.min(entry.length - offset);
        let unsound = Arc::new(AtomicBool::new(false));
        let reader = PartReader::new(job.path.clone(), offset, length, unsound.clone());
        let put = until_stop(&w.stop, w.routes.put_part(&url, length, Box::new(reader))).await?;
        if unsound.load(Ordering::Relaxed) {
            // The file did not give the bytes the check saw: look at it again next time.
            w.verified.remove(&seq);
        }
        put.map_err(Stray::Failure)?;
        w.progress = true;
        w.backoff.reset();
    }
    conclude(w, seq, id).await
}

/// The completion, whose answer decides (repeated while the server says it is still verifying).
async fn conclude(w: &mut Worker, seq: u64, id: &UploadId) -> Result<Outcome, Stray> {
    loop {
        match w
            .authed(|r, t| async move { r.complete(&t, id).await })
            .await?
        {
            Completion::Finished => {
                w.failed_once.remove(&seq);
                w.open_completions.remove(&seq);
                return Ok(w.has_it());
            }
            Completion::Completing => {
                w.open_completions.remove(&seq);
                // A wait is taken before the repeat.
                w.owe_wait = true;
            }
            Completion::Open => {
                w.failed_once.remove(&seq);
                // The parts did not make the object: the next attempt's status sends them again.
                // A second `open` in a row, whatever was stored in between, is for good (this
                // pass), so a server that stores nothing is not sent the object again and again.
                return Ok(if w.open_completions.insert(seq) {
                    Outcome::Retry("completion-open", Some(409))
                } else {
                    w.open_completions.remove(&seq);
                    Outcome::PassOver("completion-open", Some(409))
                });
            }
            Completion::Failed => {
                w.open_completions.remove(&seq);
                return Ok(failed(w, seq));
            }
            Completion::Unstated => {
                w.open_completions.remove(&seq);
                return Ok(Outcome::PassOver("completion-state", Some(409)));
            }
        }
    }
}

/// The row failed: look at the source again and begin once more; a second failure in a row sets
/// the entry aside until a restart (ADR-495 item 9).
fn failed(w: &mut Worker, seq: u64) -> Outcome {
    w.verified.remove(&seq);
    if w.failed_once.insert(seq) {
        Outcome::Retry("completion-failed", Some(409))
    } else {
        w.failed_once.remove(&seq);
        Outcome::SetAside("completion-failed")
    }
}
