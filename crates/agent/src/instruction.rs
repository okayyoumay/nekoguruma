//! The agent's gate for signed job instructions (design 5.3, 11.1; ADR-461 Decision items 9
//! to 12).
//!
//! [`open`] is the only way to a [`VerifiedInstruction`]: it takes the envelope the server
//! delivered, the instruction key the agent trusts and the agent's own ID, and refuses
//! anything that does not verify, in this order:
//!
//! 1. the length and the format byte of the envelope;
//! 2. the key fingerprint against the trusted key's;
//! 3. the signature's encoding;
//! 4. the signature, under the instruction tag, over the payload bytes as delivered;
//! 5. the body (its version, then its layout and bounds);
//! 6. the agent ID in the body against the agent's own.
//!
//! The payload is not read before step 4 has passed, and nothing re-encodes the instruction.
//! The gate takes the trusted key as a parameter and reads it from nowhere (the registration
//! stores it, ADR-461 Decision item 11), takes no worker handle and opens no link. A refusal
//! writes one operational log entry (`docs/operational-log.md`) and returns the coarse reason
//! the control channel answers with (ADR-461 Decision item 10). The control channel's gate
//! calls [`open_unlogged`] instead and bounds the entries itself.

use shared_crypto::{PublicKey, instruction as envelope};
use shared_proto::{InstructionError, InstructionRejectReason, JobInstruction};

/// A job instruction whose envelope passed [`open`]. It has no public constructor and is not
/// `Clone`, so holding one means the gate verified it for this agent.
pub struct VerifiedInstruction {
    instruction: JobInstruction,
    envelope: Vec<u8>,
}

/// Prints the job ID and the envelope's length only: the VIN, the operator, the VCI and the
/// agent ID stay out of anything that formats the value.
impl std::fmt::Debug for VerifiedInstruction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedInstruction")
            .field("job_id", &self.instruction.job_id.0)
            .field("envelope_len", &self.envelope.len())
            .finish_non_exhaustive()
    }
}

impl VerifiedInstruction {
    /// The decoded instruction.
    pub fn instruction(&self) -> &JobInstruction {
        &self.instruction
    }

    /// The envelope this instruction came from, byte for byte: what a journal keeps and
    /// verifies again at resume (ADR-461 Decision item 9).
    pub fn envelope(&self) -> &[u8] {
        &self.envelope
    }
}

/// Verifies `envelope` against `trusted_key` and decodes its body for the agent
/// `own_agent_id`. A refusal is logged and returned as its coarse reason.
pub fn open(
    envelope: &[u8],
    trusted_key: &PublicKey,
    own_agent_id: &str,
) -> Result<VerifiedInstruction, InstructionRejectReason> {
    open_unlogged(envelope, trusted_key, own_agent_id).map_err(|refusal| {
        log_refusal(&refusal, envelope.len());
        refusal.reason
    })
}

/// [`open`] without the log entry: the refusal is returned whole, and whoever calls this bounds
/// and writes the entry (the control channel's gate does, once per reason per connection).
pub(crate) fn open_unlogged(
    envelope: &[u8],
    trusted_key: &PublicKey,
    own_agent_id: &str,
) -> Result<VerifiedInstruction, Refusal> {
    check(envelope, trusted_key, own_agent_id).map(|instruction| VerifiedInstruction {
        instruction,
        envelope: envelope.to_vec(),
    })
}

/// Why an envelope was refused.
pub(crate) struct Refusal {
    pub(crate) reason: InstructionRejectReason,
    /// The fingerprint the envelope offered, kept for an unknown key only.
    pub(crate) offered: Option<[u8; envelope::FINGERPRINT_LEN]>,
}

impl Refusal {
    /// The offered fingerprint as hex (a hash, never a key).
    pub(crate) fn offered_hex(&self) -> Option<String> {
        self.offered.as_ref().map(|fingerprint| hex(fingerprint))
    }
}

impl From<InstructionRejectReason> for Refusal {
    fn from(reason: InstructionRejectReason) -> Self {
        Self {
            reason,
            offered: None,
        }
    }
}

fn check(
    bytes: &[u8],
    trusted_key: &PublicKey,
    own_agent_id: &str,
) -> Result<JobInstruction, Refusal> {
    use InstructionRejectReason as Reason;
    let split = envelope::Envelope::split(bytes).map_err(|error| match error {
        envelope::EnvelopeError::UnknownFormat(_) => Reason::UnsupportedVersion,
        envelope::EnvelopeError::TooShort(_) | envelope::EnvelopeError::PayloadTooLong(_) => {
            Reason::Malformed
        }
    })?;
    if *split.fingerprint() != trusted_key.fingerprint() {
        return Err(Refusal {
            reason: Reason::UnknownKey,
            offered: Some(*split.fingerprint()),
        });
    }
    let signature = split.signature().map_err(|_| Reason::Malformed)?;
    envelope::verify(trusted_key, split.payload(), &signature).map_err(|_| Reason::BadSignature)?;
    let instruction = JobInstruction::decode(split.payload()).map_err(|error| match error {
        InstructionError::UnsupportedVersion { .. } => Reason::UnsupportedVersion,
        InstructionError::TooLong(_)
        | InstructionError::Format(_)
        | InstructionError::TrailingBytes(_)
        | InstructionError::Invalid { .. } => Reason::Malformed,
    })?;
    if instruction.agent_id.0 != own_agent_id {
        return Err(Reason::NotForThisAgent.into());
    }
    Ok(instruction)
}

/// The entry holds the reason, the envelope's length and, for an unknown key, the fingerprint
/// it offered (hex of a hash, never a key). It holds no job ID, agent ID or operator taken
/// from the payload: a refused payload is not the job's own word, and a verified one that
/// names another agent is that agent's business.
fn log_refusal(refusal: &Refusal, envelope_len: usize) {
    let reason = format!("{:?}", refusal.reason);
    match &refusal.offered {
        Some(fingerprint) => tracing::warn!(
            reason,
            envelope_len,
            offered_key = %hex(fingerprint),
            "job instruction refused"
        ),
        None => tracing::warn!(reason, envelope_len, "job instruction refused"),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_crypto::{SIGNATURE_LEN, Signer, SigningKey, proof, random_32, sign_tagged};
    use shared_proto::{AgentId, INSTRUCTION_VERSION, JobId, JobKind, VciId, Vin};
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    const AGENT: &str = "agent-under-test";
    const JOB: &str = "0193c8a0-7b1e-7c2d-9a4f-3e5d6c7b8a90";

    /// Serializes the tests that call [`open`]. A call made while no subscriber is set on any
    /// thread caches the refusal entry's callsite as disabled, and a call racing the log
    /// test's scoped subscriber can still see that cache, so the log test holds this lock and
    /// no other test calls `open` meanwhile.
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn gate(
        envelope: &[u8],
        key: &PublicKey,
        agent: &str,
    ) -> Result<VerifiedInstruction, InstructionRejectReason> {
        let _guard = GATE.lock().unwrap_or_else(|e| e.into_inner());
        open(envelope, key, agent)
    }

    fn body(agent: &str) -> JobInstruction {
        JobInstruction {
            version: INSTRUCTION_VERSION,
            job_id: JobId(JOB.to_owned()),
            ownership_generation: 7,
            agent_id: AgentId(agent.to_owned()),
            operator_id: "operator-1".to_owned(),
            kind: JobKind::RunSequence,
            vci: VciId("vci-a".to_owned()),
            vin: Some(Vin("VIN-UNDER-TEST".to_owned())),
            document_hashes: vec![random_32().unwrap(), random_32().unwrap()],
            artifact_hash: Some(random_32().unwrap()),
            plan_hash: None,
            issued_at: 1_000,
            start_deadline: 2_000,
        }
    }

    fn sorted(mut i: JobInstruction) -> JobInstruction {
        i.document_hashes.sort();
        i.document_hashes.dedup();
        i
    }

    fn sealed(key: &SigningKey, instruction: &JobInstruction) -> Vec<u8> {
        envelope::seal(key, &instruction.encode().unwrap()).unwrap()
    }

    /// An envelope from loose parts, for the cases `seal` would not build.
    fn assemble(format: u8, fingerprint: &[u8], signature: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = vec![format];
        out.extend_from_slice(fingerprint);
        out.extend_from_slice(signature);
        out.extend_from_slice(payload);
        out
    }

    fn setup() -> (SigningKey, JobInstruction) {
        (SigningKey::generate().unwrap(), sorted(body(AGENT)))
    }

    #[test]
    fn a_good_envelope_opens_and_every_field_is_readable() {
        let (key, want) = setup();
        let blob = sealed(&key, &want);
        let opened = gate(&blob, &key.public_key(), AGENT).unwrap();
        let got = opened.instruction();
        assert_eq!(got, &want);
        assert_eq!(got.version, INSTRUCTION_VERSION);
        assert_eq!(got.job_id.0, JOB);
        assert_eq!(got.ownership_generation, 7);
        assert_eq!(got.agent_id.0, AGENT);
        assert_eq!(got.operator_id, "operator-1");
        assert_eq!(got.kind, JobKind::RunSequence);
        assert_eq!(got.vci.0, "vci-a");
        assert_eq!(got.vin.as_ref().unwrap().0, "VIN-UNDER-TEST");
        assert_eq!(got.document_hashes, want.document_hashes);
        assert_eq!(got.artifact_hash, want.artifact_hash);
        assert_eq!(got.plan_hash, None);
        assert_eq!((got.issued_at, got.start_deadline), (1_000, 2_000));
        assert_eq!(opened.envelope(), blob.as_slice());
    }

    #[test]
    fn a_missing_or_truncated_envelope_is_malformed() {
        let (key, i) = setup();
        let blob = sealed(&key, &i);
        let trusted = key.public_key();
        for len in [0, 1, 20, envelope::HEADER_LEN - 1] {
            assert_eq!(
                gate(&blob[..len], &trusted, AGENT).unwrap_err(),
                InstructionRejectReason::Malformed,
                "{len}"
            );
        }
        // Cut inside the payload: the signature no longer covers what is left.
        assert_eq!(
            gate(&blob[..blob.len() - 1], &trusted, AGENT).unwrap_err(),
            InstructionRejectReason::BadSignature
        );
        // A payload over the bound.
        let mut long = blob.clone();
        long.resize(envelope::HEADER_LEN + envelope::MAX_PAYLOAD_LEN + 1, 0);
        assert_eq!(
            gate(&long, &trusted, AGENT).unwrap_err(),
            InstructionRejectReason::Malformed
        );
    }

    #[test]
    fn a_signature_that_does_not_parse_is_malformed() {
        let (key, i) = setup();
        let trusted = key.public_key();
        let blob = assemble(
            envelope::FORMAT_V1,
            &trusted.fingerprint(),
            &[0u8; SIGNATURE_LEN],
            &i.encode().unwrap(),
        );
        assert_eq!(
            gate(&blob, &trusted, AGENT).unwrap_err(),
            InstructionRejectReason::Malformed
        );
    }

    #[test]
    fn another_key_s_fingerprint_is_an_unknown_key() {
        let (key, i) = setup();
        let other = SigningKey::generate().unwrap();
        let blob = sealed(&other, &i);
        assert_eq!(
            gate(&blob, &key.public_key(), AGENT).unwrap_err(),
            InstructionRejectReason::UnknownKey
        );
    }

    #[test]
    fn the_pinned_fingerprint_with_another_key_s_signature_is_a_bad_signature() {
        let (key, i) = setup();
        let other = SigningKey::generate().unwrap();
        let payload = i.encode().unwrap();
        let signature = envelope::sign(&other, &payload).unwrap().to_bytes();
        let blob = assemble(
            envelope::FORMAT_V1,
            &key.public_key().fingerprint(),
            &signature,
            &payload,
        );
        assert_eq!(
            gate(&blob, &key.public_key(), AGENT).unwrap_err(),
            InstructionRejectReason::BadSignature
        );
    }

    #[test]
    fn a_changed_payload_byte_is_a_bad_signature() {
        let (key, i) = setup();
        let trusted = key.public_key();
        let blob = sealed(&key, &i);
        for at in [envelope::HEADER_LEN, blob.len() - 1] {
            let mut changed = blob.clone();
            changed[at] ^= 1;
            assert_eq!(
                gate(&changed, &trusted, AGENT).unwrap_err(),
                InstructionRejectReason::BadSignature,
                "{at}"
            );
        }
    }

    #[test]
    fn a_signature_under_the_proof_tag_is_a_bad_signature() {
        let (key, i) = setup();
        let payload = i.encode().unwrap();
        let signature = sign_tagged(&key, proof::DOMAIN_TAG, &payload)
            .unwrap()
            .to_bytes();
        let blob = assemble(
            envelope::FORMAT_V1,
            &key.public_key().fingerprint(),
            &signature,
            &payload,
        );
        assert_eq!(
            gate(&blob, &key.public_key(), AGENT).unwrap_err(),
            InstructionRejectReason::BadSignature
        );
    }

    #[test]
    fn an_unknown_format_byte_is_unsupported() {
        let (key, i) = setup();
        let mut blob = sealed(&key, &i);
        blob[0] = envelope::FORMAT_V1 + 1;
        assert_eq!(
            gate(&blob, &key.public_key(), AGENT).unwrap_err(),
            InstructionRejectReason::UnsupportedVersion
        );
    }

    #[test]
    fn an_unknown_body_version_is_unsupported() {
        let (key, mut i) = setup();
        i.version = INSTRUCTION_VERSION + 1;
        // Encoded without the encoder's checks, signed by the trusted key.
        let payload = postcard::to_allocvec(&i).unwrap();
        let blob = envelope::seal(&key, &payload).unwrap();
        assert_eq!(
            gate(&blob, &key.public_key(), AGENT).unwrap_err(),
            InstructionRejectReason::UnsupportedVersion
        );
    }

    #[test]
    fn a_signed_body_that_does_not_decode_is_malformed() {
        let (key, i) = setup();
        let mut payload = i.encode().unwrap();
        payload.push(0);
        let blob = envelope::seal(&key, &payload).unwrap();
        assert_eq!(
            gate(&blob, &key.public_key(), AGENT).unwrap_err(),
            InstructionRejectReason::Malformed
        );
    }

    #[test]
    fn another_agent_s_instruction_is_not_for_this_agent() {
        let (key, _) = setup();
        let blob = sealed(&key, &sorted(body("some-other-agent")));
        assert_eq!(
            gate(&blob, &key.public_key(), AGENT).unwrap_err(),
            InstructionRejectReason::NotForThisAgent
        );
    }

    #[test]
    fn a_foreign_fingerprint_is_refused_before_the_signature_is_read() {
        let (key, i) = setup();
        let other = SigningKey::generate().unwrap();
        let blob = assemble(
            envelope::FORMAT_V1,
            &other.public_key().fingerprint(),
            &[0u8; SIGNATURE_LEN],
            &i.encode().unwrap(),
        );
        assert_eq!(
            gate(&blob, &key.public_key(), AGENT).unwrap_err(),
            InstructionRejectReason::UnknownKey
        );
    }

    /// The signer makes low-S signatures; the same signature with s negated modulo the group
    /// order parses but is refused at the verification step.
    #[test]
    fn a_high_s_signature_by_the_trusted_key_is_a_bad_signature() {
        const ORDER: [u8; 32] = [
            0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2,
            0xfc, 0x63, 0x25, 0x51,
        ];
        let (key, i) = setup();
        let payload = i.encode().unwrap();
        let mut signature = envelope::sign(&key, &payload).unwrap().to_bytes();
        let mut borrow = 0i16;
        for at in (SIGNATURE_LEN / 2..SIGNATURE_LEN).rev() {
            let n = i16::from(ORDER[at - SIGNATURE_LEN / 2]);
            let mut d = n - i16::from(signature[at]) - borrow;
            borrow = i16::from(d < 0);
            if d < 0 {
                d += 256;
            }
            signature[at] = u8::try_from(d).unwrap();
        }
        let blob = assemble(
            envelope::FORMAT_V1,
            &key.public_key().fingerprint(),
            &signature,
            &payload,
        );
        assert_eq!(
            gate(&blob, &key.public_key(), AGENT).unwrap_err(),
            InstructionRejectReason::BadSignature
        );
    }

    #[test]
    fn a_short_envelope_of_an_unknown_format_is_unsupported() {
        let (key, _) = setup();
        assert_eq!(
            gate(&[envelope::FORMAT_V1 + 1], &key.public_key(), AGENT).unwrap_err(),
            InstructionRejectReason::UnsupportedVersion
        );
    }

    #[test]
    fn the_debug_form_leaves_out_the_payload_s_fields() {
        let (key, i) = setup();
        let opened = gate(&sealed(&key, &i), &key.public_key(), AGENT).unwrap();
        let text = format!("{opened:?}");
        assert!(text.contains(JOB), "{text}");
        for forbidden in [AGENT, "operator-1", "VIN-UNDER-TEST", "vci-a"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// A refusal writes one entry with the reason and no field of the refused payload; a good
    /// envelope writes none.
    #[test]
    fn a_refusal_is_logged_without_the_payload_s_fields() {
        let (key, i) = setup();
        let other = SigningKey::generate().unwrap();
        let _guard = GATE.lock().unwrap_or_else(|e| e.into_inner());
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .finish();
        let trusted = key.public_key();
        let good = sealed(&key, &i);
        let foreign = sealed(&other, &i);
        let wrong_agent = sealed(&key, &sorted(body("some-other-agent")));
        tracing::subscriber::with_default(subscriber, || {
            // The entry's callsites may be cached as disabled by an earlier test's call.
            tracing::callsite::rebuild_interest_cache();
            open(&good, &trusted, AGENT).unwrap();
            assert!(captured.0.lock().unwrap().is_empty());
            open(&foreign, &trusted, AGENT).unwrap_err();
            open(&wrong_agent, &trusted, AGENT).unwrap_err();
        });
        let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.contains("job instruction refused"), "{text}");
        assert!(text.contains("reason=\"UnknownKey\""), "{text}");
        assert!(text.contains("reason=\"NotForThisAgent\""), "{text}");
        assert!(
            text.contains(&hex(&other.public_key().fingerprint())),
            "{text}"
        );
        for forbidden in [
            JOB,
            "some-other-agent",
            AGENT,
            "operator-1",
            "VIN-UNDER-TEST",
            "vci-a",
        ] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
    }
}
