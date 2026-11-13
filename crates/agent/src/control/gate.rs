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

/// Refusals so far on one connection, one counter per reason. A field per reason, bumped through
/// one exhaustive `match`, so a new reason fails the build here and cannot index out of range.
#[derive(Default)]
struct Refused {
    malformed: u64,
    unknown_key: u64,
    bad_signature: u64,
    unsupported_version: u64,
    not_for_this_agent: u64,
}

impl Refused {
    /// Counts one refusal and returns how many of this reason there now are.
    fn bump(&mut self, reason: InstructionRejectReason) -> u64 {
        let count = match reason {
            InstructionRejectReason::Malformed => &mut self.malformed,
            InstructionRejectReason::UnknownKey => &mut self.unknown_key,
            InstructionRejectReason::BadSignature => &mut self.bad_signature,
            InstructionRejectReason::UnsupportedVersion => &mut self.unsupported_version,
            InstructionRejectReason::NotForThisAgent => &mut self.not_for_this_agent,
        };
        *count += 1;
        *count
    }

    fn none(&self) -> bool {
        self.malformed == 0
            && self.unknown_key == 0
            && self.bad_signature == 0
            && self.unsupported_version == 0
            && self.not_for_this_agent == 0
    }
}

/// One connection's gate.
pub(super) struct Gate<'a> {
    key: &'a PublicKey,
    agent_id: &'a str,
    refused: Refused,
}

impl<'a> Gate<'a> {
    pub(super) fn new(key: &'a PublicKey, agent_id: &'a str) -> Gate<'a> {
        Gate {
            key,
            agent_id,
            refused: Refused::default(),
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
        let count = self.refused.bump(refusal.reason);
        instruction::log_refusal(&refusal, envelope_len, count == 1);
        refusal.reason
    }

    /// The end of the connection: one line with the count per reason, only if any command was
    /// refused.
    pub(super) fn finish(&self) {
        if self.refused.none() {
            return;
        }
        let Refused {
            malformed,
            unknown_key,
            bad_signature,
            unsupported_version,
            not_for_this_agent,
        } = self.refused;
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
