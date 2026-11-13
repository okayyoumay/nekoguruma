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

use crate::instruction::{self, Refusal, VerifiedInstruction};

/// Where a verified job instruction goes. The control channel hands it over before it answers
/// `commandAck`.
///
/// An intake can receive the same verified instruction twice: the hand-over succeeded, then the
/// ack was lost (the write failed or the connection ended) and the server redelivers the command
/// on the next connection. An implementation must therefore be idempotent on the job ID.
pub trait Intake: Send + Sync {
    /// Takes the instruction.
    fn take(&self, instruction: VerifiedInstruction);
}

/// The default intake, connected to no job runner: it logs the job ID at `debug` and drops the
/// instruction.
pub struct LogIntake;

impl Intake for LogIntake {
    fn take(&self, instruction: VerifiedInstruction) {
        tracing::debug!(
            job_id = %instruction.instruction().job_id.0,
            "a verified job instruction was dropped: no job runner is connected"
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
    // Exhaustive, so a new reason fails the build here and in [`REASONS`].
    match reason {
        InstructionRejectReason::Malformed => 0,
        InstructionRejectReason::UnknownKey => 1,
        InstructionRejectReason::BadSignature => 2,
        InstructionRejectReason::UnsupportedVersion => 3,
        InstructionRejectReason::NotForThisAgent => 4,
    }
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
            return Err(self.refuse(Refusal::from(InstructionRejectReason::Malformed), 0));
        };
        let Ok(bytes) = STANDARD.decode(text) else {
            let refusal = Refusal::from(InstructionRejectReason::Malformed);
            return Err(self.refuse(refusal, text.len()));
        };
        instruction::open_unlogged(&bytes, self.key, self.agent_id)
            .map_err(|refusal| self.refuse(refusal, bytes.len()))
    }

    /// Counts the refusal, writes its entry (a warning the first time per reason) and returns
    /// the reason.
    fn refuse(&mut self, refusal: Refusal, envelope_len: usize) -> InstructionRejectReason {
        let count = &mut self.refused[slot(refusal.reason)];
        *count += 1;
        instruction::log_refusal(&refusal, envelope_len, *count == 1);
        refusal.reason
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
