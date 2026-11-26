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
//! A verified instruction that the operator check allows is then judged against its start
//! deadline (ADR-486): the channel does that with [`crate::deadline::IssueMark`], and an expired
//! job is counted and logged here ([`Gate::expired`]) the way the operator refusal is: the first
//! per connection is a `warn` with the job ID, the repeats are `debug`, and the connection's end
//! gives the count. An entry holds no time, no operator and no `commandId`.
//!
//! The channel checks a verified instruction's job ID against the received-jobs ledger next
//! (ADR-489); a duplicate is counted here ([`Gate::duplicate`], [`Gate::earlier_generation`]) and
//! logged with the job ID and the generation only, the first one for an earlier generation per
//! connection as a `warn`, the rest as `debug`.
//!
//! What a verified instruction goes to is an [`Intake`]. The gate opens no link and keeps no job
//! state; the answer to the server is the caller's.

use std::future::Future;
use std::pin::Pin;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use shared_crypto::PublicKey;
use shared_crypto::registration::Preset;
use shared_proto::{FailureCode, InstructionRejectReason};

use crate::instruction::{self, Refusal, VerifiedInstruction};

/// Why an [`Intake`] could not take an instruction. It carries nothing: the cause is the intake's
/// own to log, and the channel's entry names no job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the intake could not take the instruction")]
pub struct IntakeError;

/// What [`Intake::take`] returns: a future the channel awaits, raced with the agent's stop.
pub type Taken<'a> = Pin<Box<dyn Future<Output = Result<(), IntakeError>> + Send + 'a>>;

/// Where a verified job instruction goes. The control channel hands it over after the job's record
/// in the received-jobs ledger is durable (ADR-489) and before the `received` event and the
/// `commandAck`. An instruction whose operator the gate does not allow (ADR-483), whose start
/// deadline has passed (ADR-486) or whose job ID the ledger holds under the same or a later
/// generation is not handed over: the intake never sees it.
///
/// An intake is called at most once per job ID and generation, returns promptly (the channel's
/// loop waits for it, so a call that blocks stalls every connection's frames, though not the
/// agent's stop) and tolerates being dropped before it finishes, which a stop does. An error
/// leaves the job recorded and ends the connection with 1011; the same command is not handed
/// over again, as its redelivery is a duplicate.
pub trait Intake: Send + Sync {
    /// Takes the instruction.
    fn take(&self, instruction: VerifiedInstruction) -> Taken<'_>;
}

/// The default intake, connected to no job runner: it logs the job ID at `debug` and drops the
/// instruction.
#[derive(Debug)]
pub struct LogIntake;

impl Intake for LogIntake {
    fn take(&self, instruction: VerifiedInstruction) -> Taken<'_> {
        tracing::debug!(
            job_id = %instruction.instruction().job_id.0,
            "a verified job instruction was dropped: no job runner is connected"
        );
        Box::pin(std::future::ready(Ok(())))
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
    /// `issued_at` is the instruction's issue time, which still raises the highest one (ADR-486).
    OperatorNotAllowed { job_id: String, issued_at: u64 },
}

impl Admitted {
    /// The verified instruction's issue time, Unix seconds.
    pub(super) fn issued_at(&self) -> u64 {
        match self {
            Admitted::Accepted(verified) => verified.instruction().issued_at,
            Admitted::OperatorNotAllowed { issued_at, .. } => *issued_at,
        }
    }
}

/// Refusals so far on one connection, one counter per reason. A field per command refusal reason,
/// bumped through one exhaustive `match`, so a new reason fails the build here and cannot index out
/// of range. The operator counter is not a command refusal reason: [`Gate::accept`] bumps it.
#[derive(Default)]
struct Refused {
    malformed: u64,
    unknown_key: u64,
    bad_signature: u64,
    unsupported_version: u64,
    not_for_this_agent: u64,
    /// Verified instructions whose operator the policy did not allow (not a command refusal).
    operator_not_allowed: u64,
    /// Verified, allowed instructions past their start deadline (not a command refusal).
    expired: u64,
    /// Verified instructions whose job ID the ledger holds under the same generation (not a
    /// command refusal: the command is acknowledged).
    duplicate: u64,
    /// Verified instructions whose job ID the ledger holds under a later generation.
    earlier_generation: u64,
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
            && self.expired == 0
            && self.duplicate == 0
            && self.earlier_generation == 0
    }
}

/// One connection's gate.
pub(super) struct Gate<'a> {
    key: &'a PublicKey,
    agent_id: &'a str,
    owner_id: &'a str,
    policy: OperatorPolicy,
    refused: Refused,
    /// Whether the mark's own warning (the device clock is behind) was written on this connection.
    mark_decided: bool,
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
            mark_decided: false,
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
            return Err(self.refuse(&Refusal::from(InstructionRejectReason::Malformed), 0));
        };
        let Ok(bytes) = STANDARD.decode(text) else {
            let refusal = Refusal::from(InstructionRejectReason::Malformed);
            return Err(self.refuse(&refusal, text.len()));
        };
        let verified = instruction::open_unlogged(&bytes, self.key, self.agent_id)
            .map_err(|refusal| self.refuse(&refusal, bytes.len()))?;
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
        let issued_at = verified.instruction().issued_at;
        let code = FailureCode::OperatorNotAllowed.wire_name();
        if self.refused.operator_not_allowed == 1 {
            tracing::warn!(job_id = %job_id, code, "a job was refused: its operator is not allowed");
        } else {
            tracing::debug!(job_id = %job_id, code, "a job was refused: its operator is not allowed");
        }
        Admitted::OperatorNotAllowed { job_id, issued_at }
    }

    /// Counts a job that is past its start deadline (ADR-486) and writes its entry: a `warn` the
    /// first time on the connection, `debug` after that. `by_mark` is true when the highest issue
    /// time decided it and the device clock alone would not have: that is warned once more per
    /// connection, since the device clock is then probably behind.
    pub(super) fn expired(&mut self, job_id: &str, by_mark: bool) {
        self.refused.expired += 1;
        if self.refused.expired == 1 {
            tracing::warn!(job_id = %job_id, state = "expired", "a job was not started: its start deadline has passed");
        } else {
            tracing::debug!(job_id = %job_id, state = "expired", "a job was not started: its start deadline has passed");
        }
        if by_mark {
            if self.mark_decided {
                tracing::debug!(job_id = %job_id, "the start deadline was judged by the highest issue time: the device clock is behind it");
            } else {
                self.mark_decided = true;
                tracing::warn!(job_id = %job_id, "the start deadline was judged by the highest issue time: the device clock is behind it");
            }
        }
    }

    /// Counts a command whose job ID the ledger holds under the same generation (ADR-489) and
    /// writes its entry at `debug`: a redelivery is routine. The entry holds the job ID and the
    /// generation only.
    pub(super) fn duplicate(&mut self, job_id: &str, generation: u64) {
        self.refused.duplicate += 1;
        tracing::debug!(job_id = %job_id, generation, "a duplicate job command: the job is already recorded");
    }

    /// Counts a command whose job ID the ledger holds under a later generation: a `warn` the first
    /// time on the connection (the server should not send one), `debug` after that.
    pub(super) fn earlier_generation(&mut self, job_id: &str, generation: u64) {
        self.refused.earlier_generation += 1;
        if self.refused.earlier_generation == 1 {
            tracing::warn!(job_id = %job_id, generation, "a duplicate job command: a later generation of the job is recorded");
        } else {
            tracing::debug!(job_id = %job_id, generation, "a duplicate job command: a later generation of the job is recorded");
        }
    }

    /// Counts the refusal, writes its entry (a warning the first time per reason) and returns
    /// the reason.
    fn refuse(&mut self, refusal: &Refusal, envelope_len: usize) -> InstructionRejectReason {
        let count = self.refused.bump(refusal.reason);
        instruction::log_refusal(refusal, envelope_len, count == 1);
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
            expired,
            duplicate,
            earlier_generation,
        } = self.refused;
        tracing::info!(
            malformed,
            unknown_key,
            bad_signature,
            unsupported_version,
            not_for_this_agent,
            operator_not_allowed,
            expired,
            duplicate,
            earlier_generation,
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
            Ok(Admitted::OperatorNotAllowed { job_id, .. }) => {
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

    // W1, W3: an accepted instruction and an operator refusal both carry the instruction's issue
    // time, which the channel raises the highest issue time with.
    #[test]
    fn an_admitted_instruction_carries_its_issue_time_whichever_way_the_operator_check_went() {
        let key = SigningKey::generate().unwrap();
        let public = key.public_key();
        let mut gate = Gate::new(&public, AGENT, OWNER, "personal");
        let accepted = gate.admit(Some(&sealed(&key, AGENT, OWNER))).ok().unwrap();
        assert!(matches!(accepted, Admitted::Accepted(_)));
        assert_eq!(accepted.issued_at(), 1);
        let refused = gate
            .admit(Some(&sealed(&key, AGENT, "someone-else")))
            .ok()
            .unwrap();
        assert!(matches!(refused, Admitted::OperatorNotAllowed { .. }));
        assert_eq!(refused.issued_at(), 1);
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

    // O7: a command that fails verification is refused for that, whatever operator it names; the
    // operator is read only from an instruction that verified.
    #[test]
    fn a_command_that_fails_verification_is_refused_for_that_whatever_its_operator() {
        let key = SigningKey::generate().unwrap();
        let public = key.public_key();
        let foreign = SigningKey::generate().unwrap();
        for operator in [OWNER, "someone-else"] {
            let mut gate = Gate::new(&public, AGENT, OWNER, "personal");
            let text = sealed(&foreign, AGENT, operator);
            assert!(matches!(
                gate.admit(Some(&text)),
                Err(InstructionRejectReason::UnknownKey)
            ));
            let mut bytes = STANDARD.decode(sealed(&key, AGENT, operator)).unwrap();
            *bytes.last_mut().unwrap() ^= 1;
            assert!(matches!(
                gate.admit(Some(&STANDARD.encode(bytes))),
                Err(InstructionRejectReason::BadSignature)
            ));
            assert_eq!(gate.refused.operator_not_allowed, 0);
        }
    }
}
