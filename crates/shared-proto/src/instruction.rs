//! The body of a signed job instruction (ADR-461, design 11.1).
//!
//! Design document sections: 5.3 handling disconnections / 11.1 job instructions
//!
//! The server signs the exact bytes of [`JobInstruction::encode`] (the envelope and the
//! signature are in `shared_crypto::instruction`), and the agent verifies those bytes before it
//! calls [`JobInstruction::decode`]. Nothing may re-encode a decoded instruction and compare or
//! hash the result: postcard accepts some non-canonical inputs, so the bytes that were signed
//! are the only bytes that mean anything.
//!
//! The layout is the postcard encoding of the struct, fields in declaration order, with
//! [`INSTRUCTION_VERSION`] first. Anything the agent acts on is a field of this struct, so a
//! new thing to act on is a new version, never a sibling of the signed bytes.

use crate::{AgentId, JobId, JobKind, VciId, Vin};
use serde::{Deserialize, Serialize};

/// The layout version this crate writes and the only one it reads.
pub const INSTRUCTION_VERSION: u32 = 1;

/// Longest instruction payload, in bytes (64 KiB), the same bound as the envelope's.
pub const MAX_INSTRUCTION_LEN: usize = 64 * 1024;

/// Longest string field, in bytes.
pub const MAX_STRING_LEN: usize = 1024;

/// Length of a canonical job ID: a lowercase, hyphenated UUID.
pub const JOB_ID_LEN: usize = 36;

/// What the server tells an agent to do with one job, signed with the instruction key.
///
/// Times are Unix seconds. The hashes are SHA-256.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobInstruction {
    /// [`INSTRUCTION_VERSION`]. Kept first so a reader can learn the layout before it reads
    /// the rest.
    pub version: u32,
    /// The job, as a canonical UUID text (36 lowercase characters, hyphenated). It reaches
    /// file paths, so decoding refuses anything else.
    pub job_id: JobId,
    /// The ownership generation of the job (ADR-229).
    pub ownership_generation: u64,
    /// The agent the instruction is for (ADR-229).
    pub agent_id: AgentId,
    /// The operator who created the job (design 6.6).
    pub operator_id: String,
    pub kind: JobKind,
    /// The VCI to use: the opaque identifier the agent itself reported.
    pub vci: VciId,
    /// Target vehicle. None before it is known (two-stage locking, design 8.8).
    pub vin: Option<Vin>,
    /// The job's IR documents by SHA-256, strictly ascending, so a set has one encoding
    /// (ADR-454).
    pub document_hashes: Vec<[u8; 32]>,
    /// The ECU distribution artifact of a write job (ADR-336).
    pub artifact_hash: Option<[u8; 32]>,
    /// The identification plan of an identification job; this hash is the plan's signature
    /// (ADR-395 item 2).
    pub plan_hash: Option<[u8; 32]>,
    /// When the server issued the instruction.
    pub issued_at: u64,
    /// The job is not started after this time (design 5.3).
    pub start_deadline: u64,
}

/// Why an instruction payload was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum InstructionError {
    /// Longer than [`MAX_INSTRUCTION_LEN`].
    #[error("instruction is {0} bytes, over the {MAX_INSTRUCTION_LEN}-byte limit")]
    TooLong(usize),
    /// The layout version is not [`INSTRUCTION_VERSION`].
    #[error("unsupported instruction version {found}, expected {INSTRUCTION_VERSION}")]
    UnsupportedVersion { found: u32 },
    /// The bytes are not the encoding of an instruction (this includes an unknown variant of a
    /// closed enum).
    #[error("instruction does not decode: {0}")]
    Format(postcard::Error),
    /// Bytes remain after the instruction.
    #[error("{0} bytes follow the instruction")]
    TrailingBytes(usize),
    /// A field is outside what the layout allows.
    #[error("instruction field {field}: {why}")]
    Invalid {
        field: &'static str,
        why: &'static str,
    },
}

impl JobInstruction {
    /// The bytes to sign. Refuses an instruction that [`decode`](Self::decode) would refuse.
    pub fn encode(&self) -> Result<Vec<u8>, InstructionError> {
        self.validate()?;
        let bytes = postcard::to_allocvec(self).map_err(InstructionError::Format)?;
        if bytes.len() > MAX_INSTRUCTION_LEN {
            return Err(InstructionError::TooLong(bytes.len()));
        }
        Ok(bytes)
    }

    /// Reads the payload of a verified envelope: the size bound, then the version alone, then
    /// the whole struct with no bytes left over, then [`validate`](Self::validate).
    pub fn decode(bytes: &[u8]) -> Result<Self, InstructionError> {
        if bytes.len() > MAX_INSTRUCTION_LEN {
            return Err(InstructionError::TooLong(bytes.len()));
        }
        let (found, _) =
            postcard::take_from_bytes::<u32>(bytes).map_err(InstructionError::Format)?;
        if found != INSTRUCTION_VERSION {
            return Err(InstructionError::UnsupportedVersion { found });
        }
        let (instruction, rest) =
            postcard::take_from_bytes::<Self>(bytes).map_err(InstructionError::Format)?;
        if !rest.is_empty() {
            return Err(InstructionError::TrailingBytes(rest.len()));
        }
        instruction.validate()?;
        Ok(instruction)
    }

    /// The checks the layout cannot express: the version, the canonical job ID, the string
    /// bounds and the order of the document hashes.
    pub fn validate(&self) -> Result<(), InstructionError> {
        let invalid = |field, why| InstructionError::Invalid { field, why };
        if self.version != INSTRUCTION_VERSION {
            return Err(InstructionError::UnsupportedVersion {
                found: self.version,
            });
        }
        if !is_canonical_uuid(&self.job_id.0) {
            return Err(invalid(
                "job_id",
                "not a lowercase hyphenated UUID of 36 characters",
            ));
        }
        for (field, value) in [
            ("agent_id", &self.agent_id.0),
            ("operator_id", &self.operator_id),
            ("vci", &self.vci.0),
        ] {
            if value.is_empty() || value.len() > MAX_STRING_LEN {
                return Err(invalid(field, "must be 1 to 1024 bytes"));
            }
        }
        if let Some(vin) = &self.vin
            && (vin.0.is_empty() || vin.0.len() > MAX_STRING_LEN)
        {
            return Err(invalid("vin", "must be 1 to 1024 bytes when present"));
        }
        if !self.document_hashes.windows(2).all(|w| w[0] < w[1]) {
            return Err(invalid(
                "document_hashes",
                "must be strictly ascending, with no duplicate",
            ));
        }
        Ok(())
    }
}

/// 36 characters, lowercase hexadecimal digits and hyphens at 8, 13, 18 and 23.
fn is_canonical_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == JOB_ID_LEN
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => matches!(b, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// Why an agent refused an instruction, sent back keyed by the command's ID (ADR-461).
///
/// The reasons are coarse on purpose: they tell the server what to do next, not how a forger
/// came close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InstructionRejectReason {
    /// The envelope, its signature or the body is missing or does not parse.
    Malformed,
    /// The envelope names a key other than the one the agent trusts. After the server rotated
    /// its key, this tells it to ask the agent to register again.
    UnknownKey,
    /// The signature does not verify.
    BadSignature,
    /// The envelope format or the body version is not one the agent knows.
    UnsupportedVersion,
    /// The instruction verified but names another agent.
    NotForThisAgent,
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_crypto::{Signer, SigningKey, instruction as envelope};

    const JOB: &str = "0193c8a0-7b1e-7c2d-9a4f-3e5d6c7b8a90";

    fn base() -> JobInstruction {
        JobInstruction {
            version: INSTRUCTION_VERSION,
            job_id: JobId(JOB.to_owned()),
            ownership_generation: 3,
            agent_id: AgentId("agent-1".to_owned()),
            operator_id: "op".to_owned(),
            kind: JobKind::RunSequence,
            vci: VciId("vci-a".to_owned()),
            vin: Some(Vin("V1".to_owned())),
            document_hashes: vec![[1; 32], [2; 32]],
            artifact_hash: Some([3; 32]),
            plan_hash: None,
            issued_at: 1_000,
            start_deadline: 2_000,
        }
    }

    fn decode_err(i: &JobInstruction) -> InstructionError {
        // Encode through postcard directly so the encoder's own checks do not hide the case.
        let bytes = postcard::to_allocvec(i).unwrap();
        JobInstruction::decode(&bytes).unwrap_err()
    }

    #[test]
    fn the_body_has_the_documented_layout() {
        let i = JobInstruction {
            version: 1,
            job_id: JobId(JOB.to_owned()),
            ownership_generation: 300,
            agent_id: AgentId("a".to_owned()),
            operator_id: "o".to_owned(),
            kind: JobKind::Identify,
            vci: VciId("v".to_owned()),
            vin: None,
            document_hashes: vec![[7; 32]],
            artifact_hash: None,
            plan_hash: Some([9; 32]),
            issued_at: 5,
            start_deadline: 6,
        };
        let mut want = vec![0x01, 36];
        want.extend(JOB.as_bytes());
        want.extend([0xac, 0x02]); // 300 as a varint
        want.extend([1, b'a', 1, b'o']);
        want.push(6); // Identify, the seventh kind
        want.extend([1, b'v']);
        want.push(0); // no VIN
        want.push(1); // one document hash
        want.extend([7; 32]);
        want.push(0); // no artifact hash
        want.push(1);
        want.extend([9; 32]);
        want.extend([5, 6]);
        assert_eq!(i.encode().unwrap(), want);
        assert_eq!(JobInstruction::decode(&want).unwrap(), i);
    }

    #[test]
    fn a_signed_instruction_round_trips() {
        let key = SigningKey::generate().unwrap();
        let i = base();
        let payload = i.encode().unwrap();
        let blob = envelope::seal(&key, &payload).unwrap();
        let env = envelope::Envelope::split(&blob).unwrap();
        envelope::verify(&key.public_key(), env.payload(), &env.signature().unwrap()).unwrap();
        let back = JobInstruction::decode(env.payload()).unwrap();
        assert_eq!(back, i);
        // The signed bytes are the only bytes: re-encoding reproduces them here, but the agent
        // must not rely on that.
        assert_eq!(back.encode().unwrap(), env.payload());
    }

    #[test]
    fn a_changed_byte_of_the_payload_fails_verification() {
        let key = SigningKey::generate().unwrap();
        let mut blob = envelope::seal(&key, &base().encode().unwrap()).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 1;
        let env = envelope::Envelope::split(&blob).unwrap();
        assert!(
            envelope::verify(&key.public_key(), env.payload(), &env.signature().unwrap()).is_err()
        );
    }

    #[test]
    fn the_length_bound_matches_the_envelopes() {
        assert_eq!(
            MAX_INSTRUCTION_LEN,
            shared_crypto::instruction::MAX_PAYLOAD_LEN
        );
    }

    #[test]
    fn postcard_accepts_a_non_canonical_varint_so_nothing_may_re_encode() {
        // The generation 3 written as a two-byte varint (0x83 0x00) decodes as 3, but encodes
        // back as one byte: the decoded value does not reproduce the signed bytes. The agent
        // therefore keeps and hashes the received bytes and never re-encodes.
        let i = base();
        let bytes = i.encode().unwrap();
        let at = 1 + 1 + JOB_ID_LEN;
        assert_eq!(bytes[at], 3);
        let mut loose = bytes[..at].to_vec();
        loose.extend([0x83, 0x00]);
        loose.extend(&bytes[at + 1..]);
        let back = JobInstruction::decode(&loose).unwrap();
        assert_eq!(back, i);
        assert_ne!(back.encode().unwrap(), loose);
    }

    #[test]
    fn an_option_tag_other_than_0_or_1_is_refused() {
        let mut i = base();
        i.vin = None;
        let mut bytes = i.encode().unwrap();
        // The VIN tag follows the version, the job ID, the generation, three strings and the kind.
        let at = 1 + 1 + JOB_ID_LEN + 1 + (1 + 7) + (1 + 2) + 1 + (1 + 5);
        assert_eq!(bytes[at], 0);
        bytes[at] = 2;
        assert!(matches!(
            JobInstruction::decode(&bytes),
            Err(InstructionError::Format(_))
        ));
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = base().encode().unwrap();
        bytes.push(0);
        assert_eq!(
            JobInstruction::decode(&bytes),
            Err(InstructionError::TrailingBytes(1))
        );
    }

    #[test]
    fn an_unknown_version_is_refused_before_the_rest_is_read() {
        let mut bytes = base().encode().unwrap();
        bytes[0] = 2;
        assert_eq!(
            JobInstruction::decode(&bytes),
            Err(InstructionError::UnsupportedVersion { found: 2 })
        );
        // Even when what follows the version is not an instruction at all.
        assert_eq!(
            JobInstruction::decode(&[2, 0xff]),
            Err(InstructionError::UnsupportedVersion { found: 2 })
        );
        let mut v = base();
        v.version = 2;
        assert!(v.encode().is_err());
    }

    #[test]
    fn an_oversize_payload_is_refused() {
        assert_eq!(
            JobInstruction::decode(&vec![1; MAX_INSTRUCTION_LEN + 1]),
            Err(InstructionError::TooLong(MAX_INSTRUCTION_LEN + 1))
        );
        // Many hashes, in order, past the bound.
        let mut i = base();
        i.document_hashes = (0..=(MAX_INSTRUCTION_LEN / 32) as u32)
            .map(|n| {
                let mut h = [0u8; 32];
                h[..4].copy_from_slice(&n.to_be_bytes());
                h
            })
            .collect();
        assert!(matches!(i.encode(), Err(InstructionError::TooLong(_))));
    }

    #[test]
    fn unsorted_or_repeated_document_hashes_are_refused() {
        for hashes in [
            vec![[2; 32], [1; 32]],
            vec![[1; 32], [1; 32]],
            vec![[1; 32], [3; 32], [2; 32]],
        ] {
            let mut i = base();
            i.document_hashes = hashes;
            let err = decode_err(&i);
            assert!(
                matches!(
                    err,
                    InstructionError::Invalid {
                        field: "document_hashes",
                        ..
                    }
                ),
                "{err:?}"
            );
            assert!(i.encode().is_err());
        }
        let mut none = base();
        none.document_hashes.clear();
        assert!(none.encode().is_ok());
    }

    #[test]
    fn a_job_id_must_be_a_canonical_uuid() {
        for bad in [
            "",
            "0193C8A0-7B1E-7C2D-9A4F-3E5D6C7B8A90", // upper case
            "0193c8a07b1e7c2d9a4f3e5d6c7b8a90",     // no hyphens
            "{0193c8a0-7b1e-7c2d-9a4f-3e5d6c7b8a90}",
            "0193c8a0-7b1e-7c2d-9a4f-3e5d6c7b8a9",    // short
            "0193c8a0-7b1e-7c2d-9a4f-3e5d6c7b8a9g",   // not hexadecimal
            "../../etc/passwd-0000-0000-0000-000000", // 36 bytes of path
            "0193c8a0-7b1e-7c2d-9a4f-3e5d6c7b8a90\n",
        ] {
            let mut i = base();
            i.job_id = JobId(bad.to_owned());
            assert!(
                matches!(
                    decode_err(&i),
                    InstructionError::Invalid {
                        field: "job_id",
                        ..
                    }
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn strings_are_bounded() {
        let long = "x".repeat(MAX_STRING_LEN + 1);
        let ok = "x".repeat(MAX_STRING_LEN);
        type Setter = fn(&mut JobInstruction, String);
        let cases: [(&str, Setter); 4] = [
            ("agent_id", |i, s| i.agent_id = AgentId(s)),
            ("operator_id", |i, s| i.operator_id = s),
            ("vci", |i, s| i.vci = VciId(s)),
            ("vin", |i, s| i.vin = Some(Vin(s))),
        ];
        for (field, set) in cases {
            let mut i = base();
            set(&mut i, long.clone());
            assert!(
                matches!(decode_err(&i), InstructionError::Invalid { field: f, .. } if f == field),
                "{field}"
            );
            let mut i = base();
            set(&mut i, ok.clone());
            assert_eq!(JobInstruction::decode(&i.encode().unwrap()).unwrap(), i);
            let mut i = base();
            set(&mut i, String::new());
            assert!(i.encode().is_err(), "{field} empty");
        }
    }

    #[test]
    fn an_unknown_job_kind_is_refused() {
        let i = base();
        let mut bytes = i.encode().unwrap();
        // The kind follows the version, the job ID, the generation and two strings.
        let at = 1 + 1 + JOB_ID_LEN + 1 + (1 + 7) + (1 + 2);
        assert_eq!(bytes[at], 3, "RunSequence");
        bytes[at] = 7;
        assert!(matches!(
            JobInstruction::decode(&bytes),
            Err(InstructionError::Format(_))
        ));
    }

    #[test]
    fn every_kind_round_trips() {
        for kind in [
            JobKind::Acquire,
            JobKind::MonitorSession,
            JobKind::MonitorCapture,
            JobKind::RunSequence,
            JobKind::WriteSettings,
            JobKind::Reprogram,
            JobKind::Identify,
        ] {
            let mut i = base();
            i.kind = kind;
            assert_eq!(JobInstruction::decode(&i.encode().unwrap()).unwrap(), i);
        }
    }

    #[test]
    fn reject_reasons_use_camel_case_names() {
        let names: Vec<String> = [
            InstructionRejectReason::Malformed,
            InstructionRejectReason::UnknownKey,
            InstructionRejectReason::BadSignature,
            InstructionRejectReason::UnsupportedVersion,
            InstructionRejectReason::NotForThisAgent,
        ]
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
        assert_eq!(
            names,
            [
                "\"malformed\"",
                "\"unknownKey\"",
                "\"badSignature\"",
                "\"unsupportedVersion\"",
                "\"notForThisAgent\""
            ]
        );
    }
}
