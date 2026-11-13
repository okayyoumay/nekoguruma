//! The gate for job commands on one connection (design 5.3; ADR-461 Decision items 9 to 12,
//! ADR-466, ADR-471).
//!
//! A [`Gate`] wraps [`crate::instruction`]'s verification with the trusted key and the agent's own
//! ID from the registration record, and bounds what the refusals write to the log: the first
//! refusal per reason on a connection is a `warn`, later ones are `debug`, and when the connection
//! ends one line gives the count per reason (only if there were any). A refusal is logged without
//! any field of the refused payload (the reason, the envelope's length and, for an unknown key,
//! the fingerprint it offered), and never with a `commandId`.
//!
//! What a verified instruction goes to is an [`Intake`]. The gate opens no link and keeps no job
//! state; the answer to the server is the caller's.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use shared_crypto::PublicKey;
use shared_proto::InstructionRejectReason;

use crate::instruction::{self, VerifiedInstruction};

/// Where a verified job instruction goes. The control channel hands it over before it answers
/// `commandAck`.
pub trait Intake: Send + Sync {
    /// Takes the instruction.
    fn take(&self, instruction: VerifiedInstruction);
}

/// The default intake: the job-start work is not here yet, so it logs the job ID at `debug` and
/// drops the instruction.
pub struct LogIntake;

impl Intake for LogIntake {
    fn take(&self, instruction: VerifiedInstruction) {
        tracing::debug!(
            job_id = %instruction.instruction().job_id.0,
            "a verified job instruction was dropped: no job start yet"
        );
    }
}

const REASONS: [InstructionRejectReason; 5] = [
    InstructionRejectReason::Malformed,
    InstructionRejectReason::UnknownKey,
    InstructionRejectReason::BadSignature,
    InstructionRejectReason::UnsupportedVersion,
    InstructionRejectReason::NotForThisAgent,
];

fn slot(reason: InstructionRejectReason) -> usize {
    REASONS
        .iter()
        .position(|r| *r == reason)
        .unwrap_or_default()
}

/// One connection's gate.
pub(super) struct Gate<'a> {
    key: &'a PublicKey,
    agent_id: &'a str,
    /// Refusals so far, per reason (indexed as [`REASONS`]).
    refused: [u64; REASONS.len()],
}

impl<'a> Gate<'a> {
    pub(super) fn new(key: &'a PublicKey, agent_id: &'a str) -> Gate<'a> {
        Gate {
            key,
            agent_id,
            refused: [0; REASONS.len()],
        }
    }

    /// Verifies the command's `signedJob` (the base64 of one envelope; `None` when the property
    /// was missing or not a string). A refusal is counted and logged here and returned as its
    /// reason.
    pub(super) fn admit(
        &mut self,
        signed_job: Option<&str>,
    ) -> Result<VerifiedInstruction, InstructionRejectReason> {
        let Some(text) = signed_job else {
            self.malformed(0);
            return Err(InstructionRejectReason::Malformed);
        };
        let Ok(bytes) = STANDARD.decode(text) else {
            self.malformed(text.len());
            return Err(InstructionRejectReason::Malformed);
        };
        match instruction::open_unlogged(&bytes, self.key, self.agent_id) {
            Ok(verified) => Ok(verified),
            Err(refusal) => {
                let offered = refusal.offered_hex();
                self.refuse(refusal.reason, bytes.len(), offered.as_deref());
                Err(refusal.reason)
            }
        }
    }

    fn malformed(&mut self, envelope_len: usize) {
        self.refuse(InstructionRejectReason::Malformed, envelope_len, None);
    }

    fn refuse(
        &mut self,
        reason: InstructionRejectReason,
        envelope_len: usize,
        offered_key: Option<&str>,
    ) {
        let count = &mut self.refused[slot(reason)];
        *count += 1;
        let first = *count == 1;
        let word = format!("{reason:?}");
        match (first, offered_key) {
            (true, Some(offered_key)) => tracing::warn!(
                reason = word,
                envelope_len,
                offered_key,
                "job instruction refused"
            ),
            (true, None) => {
                tracing::warn!(reason = word, envelope_len, "job instruction refused");
            }
            (false, Some(offered_key)) => tracing::debug!(
                reason = word,
                envelope_len,
                offered_key,
                "job instruction refused"
            ),
            (false, None) => {
                tracing::debug!(reason = word, envelope_len, "job instruction refused");
            }
        }
    }

    /// The end of the connection: one line with the count per reason, only if any command was
    /// refused.
    pub(super) fn finish(&self) {
        if self.refused.iter().all(|count| *count == 0) {
            return;
        }
        let [
            malformed,
            unknown_key,
            bad_signature,
            unsupported_version,
            not_for_this_agent,
        ] = self.refused;
        tracing::info!(
            malformed,
            unknown_key,
            bad_signature,
            unsupported_version,
            not_for_this_agent,
            "job instructions refused on this connection"
        );
    }
}
