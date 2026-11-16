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
//! A verified instruction is then checked against who may ask this agent for a job (design 6.6,
//! ADR-483): the operator the server signed into it must be the agent's registered owner on the
//! `personal` and `personalRemote` presets, and `shared` checks nothing. A mismatch is not a
//! refusal of the command; the control channel records one terminal `jobState` event for the job
//! and still answers `commandAck`, since the delivery itself is settled. The entry for it holds
//! the job ID and the failure code, never the operator ID, the owner ID or a `commandId`.
//!
//! What a verified instruction goes to is an [`Intake`]. The gate opens no link and keeps no job
//! state; the answer to the server is the caller's.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use shared_crypto::PublicKey;
use shared_crypto::registration::Preset;
use shared_proto::{FailureCode, InstructionRejectReason};

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

/// What the gate does with a verified instruction's operator ID, decided once per session from the
/// record's preset (design 6.6, ADR-483).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperatorPolicy {
    /// The operator must be the registered owner.
    OwnerOnly,
    /// No agent-side check: the server's own permission test stands alone.
    Anyone,
}

impl OperatorPolicy {
    /// The policy of the record's preset. A name that is not a preset (the record's reader
    /// refuses one, so this is not reached) is owner-only: the stricter choice.
    fn of_preset(preset: &str) -> OperatorPolicy {
        match Preset::parse(preset) {
            Some(Preset::Personal | Preset::PersonalRemote) | None => OperatorPolicy::OwnerOnly,
            Some(Preset::Shared) => OperatorPolicy::Anyone,
        }
    }
}

/// What the gate made of a command's `signedJob`.
pub(super) enum Admitted {
    /// The instruction verified and its operator may ask this agent for a job.
    Accepted(Box<VerifiedInstruction>),
    /// The instruction verified but its operator may not: the job is refused, never taken.
    OperatorNotAllowed { job_id: String },
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
    /// Verified instructions whose operator the policy did not allow (not a command refusal).
    operator_not_allowed: u64,
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
            && self.operator_not_allowed == 0
    }
}

/// One connection's gate.
pub(super) struct Gate<'a> {
    key: &'a PublicKey,
    agent_id: &'a str,
    owner_id: &'a str,
    policy: OperatorPolicy,
    refused: Refused,
}

impl<'a> Gate<'a> {
    /// `owner_id` and `preset` are the registration record's.
    pub(super) fn new(
        key: &'a PublicKey,
        agent_id: &'a str,
        owner_id: &'a str,
        preset: &str,
    ) -> Gate<'a> {
        Gate {
            key,
            agent_id,
            owner_id,
            policy: OperatorPolicy::of_preset(preset),
            refused: Refused::default(),
        }
    }

    /// Verifies the command's `signedJob` (the base64 of one envelope; `None` when the property
    /// was missing or not a string). A refusal is counted and logged here and returned as its
    /// reason. The operator check runs only on an instruction that passed every check of
    /// [`instruction::open_unlogged`], the agent ID included.
    pub(super) fn admit(
        &mut self,
        signed_job: Option<&str>,
    ) -> Result<Admitted, InstructionRejectReason> {
        let Some(text) = signed_job else {
            return Err(self.refuse(Refusal::from(InstructionRejectReason::Malformed), 0));
        };
        let Ok(bytes) = STANDARD.decode(text) else {
            let refusal = Refusal::from(InstructionRejectReason::Malformed);
            return Err(self.refuse(refusal, text.len()));
        };
        let verified = instruction::open_unlogged(&bytes, self.key, self.agent_id)
            .map_err(|refusal| self.refuse(refusal, bytes.len()))?;
        Ok(self.accept(verified))
    }

    /// The operator check (ADR-483): the operator ID against the owner ID as exact bytes, with
    /// no case folding or trimming.
    fn accept(&mut self, verified: VerifiedInstruction) -> Admitted {
        let allowed = match self.policy {
            OperatorPolicy::OwnerOnly => verified.instruction().operator_id == self.owner_id,
            OperatorPolicy::Anyone => true,
        };
        if allowed {
            return Admitted::Accepted(Box::new(verified));
        }
        self.refused.operator_not_allowed += 1;
        let job_id = verified.instruction().job_id.0.clone();
        let code = FailureCode::OperatorNotAllowed.wire_name();
        if self.refused.operator_not_allowed == 1 {
            tracing::warn!(job_id = %job_id, code, "a job was refused: its operator is not allowed");
        } else {
            tracing::debug!(job_id = %job_id, code, "a job was refused: its operator is not allowed");
        }
        Admitted::OperatorNotAllowed { job_id }
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
            operator_not_allowed,
        } = self.refused;
        tracing::info!(
            malformed,
            unknown_key,
            bad_signature,
            unsupported_version,
            not_for_this_agent,
            operator_not_allowed,
            "job instructions refused on this connection"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_crypto::{Signer, SigningKey, instruction as envelope};
    use shared_proto::{AgentId, INSTRUCTION_VERSION, JobId, JobInstruction, JobKind, VciId, Vin};

    const AGENT: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
    const OWNER: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d50";
    const JOB: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f";

    fn sealed(key: &SigningKey, agent: &str, operator: &str) -> String {
        let body = JobInstruction {
            version: INSTRUCTION_VERSION,
            job_id: JobId(JOB.to_owned()),
            ownership_generation: 1,
            agent_id: AgentId(agent.to_owned()),
            operator_id: operator.to_owned(),
            kind: JobKind::RunSequence,
            vci: VciId("vci-a".to_owned()),
            vin: Some(Vin("VIN".to_owned())),
            document_hashes: vec![],
            artifact_hash: None,
            plan_hash: None,
            issued_at: 1,
            start_deadline: 2,
        };
        let bytes = envelope::seal(key, &body.encode().unwrap()).unwrap();
        STANDARD.encode(bytes)
    }

    /// Whether a gate with `preset` takes a command from `operator`.
    fn takes(preset: &str, operator: &str) -> bool {
        let key = SigningKey::generate().unwrap();
        let public = key.public_key();
        let mut gate = Gate::new(&public, AGENT, OWNER, preset);
        match gate.admit(Some(&sealed(&key, AGENT, operator))) {
            Ok(Admitted::Accepted(_)) => true,
            Ok(Admitted::OperatorNotAllowed { job_id }) => {
                assert_eq!(job_id, JOB);
                false
            }
            Err(reason) => panic!("verification failed: {reason:?}"),
        }
    }

    // O1, O2, O3, O4
    #[test]
    fn the_policy_follows_the_preset() {
        for preset in ["personal", "personalRemote"] {
            assert!(takes(preset, OWNER), "{preset}: the owner");
            assert!(!takes(preset, "someone-else"), "{preset}: another operator");
        }
        assert!(takes("shared", OWNER));
        assert!(takes("shared", "someone-else"));
    }

    // O5: the match is on exact bytes.
    #[test]
    fn the_match_is_exact_bytes() {
        for odd in [
            OWNER.to_uppercase(),
            format!(" {OWNER}"),
            format!("{OWNER} "),
            format!("{OWNER}\n"),
        ] {
            assert!(!takes("personal", &odd), "{odd:?}");
        }
    }

    // O11: a preset that does not parse is owner-only.
    #[test]
    fn a_preset_that_does_not_parse_is_owner_only() {
        for preset in ["", "Personal", "other", "SHARED"] {
            assert!(takes(preset, OWNER), "{preset:?}: the owner");
            assert!(
                !takes(preset, "someone-else"),
                "{preset:?}: another operator"
            );
        }
    }

    // O6: the agent ID is checked first, so a command for another agent is a refusal whatever
    // its operator.
    #[test]
    fn another_agents_command_is_refused_before_the_operator_is_read() {
        let key = SigningKey::generate().unwrap();
        let public = key.public_key();
        let mut gate = Gate::new(&public, AGENT, OWNER, "personal");
        let text = sealed(&key, "other-agent", "someone-else");
        assert!(matches!(
            gate.admit(Some(&text)),
            Err(InstructionRejectReason::NotForThisAgent)
        ));
        assert!(gate.refused.operator_not_allowed == 0);
    }
}
