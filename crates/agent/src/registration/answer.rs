//! The server's answer to a registration, checked (design 6.4, 6.7; ADR-463 Decision item 21,
//! ADR-470). Nothing from a 201 is stored until every check here has passed.
//!
//! The order is the case table's (V1 to V7): the shape of the body, the instruction key as a
//! canonical SPKI PEM, its fingerprint against `--server-key` (a difference is the answer of
//! someone else, exit 6), the key ID, the assigned values, the signature's encoding, and last the
//! response signature against the instruction key the answer carried, over the code's hash, the
//! agent key's fingerprint and the assigned values (exit 6 when it does not verify). The body of
//! a refusal is never read, so only a 201 comes here.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::de::{self, MapAccess, Visitor};
use shared_crypto::registration::{
    MAX_FIELD_LEN, Preset, RegistrationCode, ResponseTranscript, verify_response,
};
use shared_crypto::{PublicKey, Signature};

use super::Exit;
use crate::server_http::MAX_BODY;

/// The signature field's longest text, as the server's own limit for the proof.
const MAX_SIGNATURE_TEXT: usize = 128;
const SIGNATURE_BYTES: usize = 64;

/// Why an answer is refused, with the exit status it gives.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AnswerError {
    /// V1: the body is not the shape of a registration answer.
    #[error("not a registration answer")]
    NotAnAnswer,
    /// V2, V4, V5, V6: a field breaks its rule. The text names the field.
    #[error("not a registration answer: the {0} is not valid")]
    BadField(&'static str),
    /// V3: the key the answer carries is not the one `--server-key` names.
    #[error(
        "the answer's instruction key (fingerprint {local}) is not the one --server-key names; \
         nothing was stored"
    )]
    KeyMismatch { local: String },
    /// V7: the response signature does not verify.
    #[error("the answer's signature does not verify; nothing was stored")]
    BadSignature,
}

impl AnswerError {
    /// Exit status 6 for an answer that is not authentic, 1 for one that is not an answer.
    pub fn exit_code(&self) -> u8 {
        self.exit_kind().code()
    }

    pub(crate) fn exit_kind(&self) -> Exit {
        match self {
            AnswerError::KeyMismatch { .. } | AnswerError::BadSignature => Exit::NotAuthentic,
            AnswerError::NotAnAnswer | AnswerError::BadField(_) => Exit::CannotRegister,
        }
    }
}

/// What the check needs from the request.
pub struct Expected<'a> {
    pub code: &'a RegistrationCode,
    pub agent_key: &'a PublicKey,
    /// `--server-key`, as bytes.
    pub server_key_id: [u8; 32],
}

/// An answer that passed every check.
#[derive(Debug, PartialEq, Eq)]
pub struct Verified {
    pub agent_id: String,
    pub owner_id: String,
    pub preset: Preset,
    pub allow_remote: bool,
    pub instruction_key_pem: String,
    pub instruction_key_id: String,
}

/// The seven fields, each exactly once.
struct Fields {
    agent_id: String,
    owner_id: String,
    preset: String,
    allow_remote: bool,
    instruction_key: String,
    instruction_key_id: String,
    signature: String,
}

struct FieldsVisitor;

impl<'de> Visitor<'de> for FieldsVisitor {
    type Value = Fields;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an object with the registration answer's fields")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Fields, A::Error> {
        let mut agent_id = None;
        let mut owner_id = None;
        let mut preset = None;
        let mut allow_remote = None;
        let mut instruction_key = None;
        let mut instruction_key_id = None;
        let mut signature = None;
        fn put<T, E: de::Error>(slot: &mut Option<T>, value: T) -> Result<(), E> {
            if slot.replace(value).is_some() {
                return Err(E::custom("duplicate field"));
            }
            Ok(())
        }
        while let Some(name) = map.next_key::<String>()? {
            match name.as_str() {
                "agentId" => put(&mut agent_id, map.next_value::<String>()?)?,
                "ownerId" => put(&mut owner_id, map.next_value::<String>()?)?,
                "preset" => put(&mut preset, map.next_value::<String>()?)?,
                "allowRemote" => put(&mut allow_remote, map.next_value::<bool>()?)?,
                "instructionKey" => put(&mut instruction_key, map.next_value::<String>()?)?,
                "instructionKeyId" => put(&mut instruction_key_id, map.next_value::<String>()?)?,
                "signature" => put(&mut signature, map.next_value::<String>()?)?,
                _ => return Err(de::Error::custom("unknown field")),
            }
        }
        let missing = || de::Error::custom("missing field");
        Ok(Fields {
            agent_id: agent_id.ok_or_else(missing)?,
            owner_id: owner_id.ok_or_else(missing)?,
            preset: preset.ok_or_else(missing)?,
            allow_remote: allow_remote.ok_or_else(missing)?,
            instruction_key: instruction_key.ok_or_else(missing)?,
            instruction_key_id: instruction_key_id.ok_or_else(missing)?,
            signature: signature.ok_or_else(missing)?,
        })
    }
}

/// Reads the body as exactly one JSON object with the seven fields.
fn parse_fields(body: &[u8]) -> Option<Fields> {
    use serde::de::Deserializer as _;
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let fields = deserializer.deserialize_map(FieldsVisitor).ok()?;
    deserializer.end().ok()?;
    Some(fields)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks a 201's content type and body, in the order V1 to V7.
pub fn check(
    json_content_type: bool,
    body: &[u8],
    expected: &Expected<'_>,
) -> Result<Verified, AnswerError> {
    // V1
    if !json_content_type || body.len() > MAX_BODY {
        return Err(AnswerError::NotAnAnswer);
    }
    let fields = parse_fields(body).ok_or(AnswerError::NotAnAnswer)?;
    // V2: the key must be the canonical encoding of a P-256 key, so what is stored is what is
    // fingerprinted.
    let key = PublicKey::from_spki_pem(&fields.instruction_key)
        .ok()
        .filter(|key| key.to_spki_pem() == fields.instruction_key)
        .ok_or(AnswerError::BadField("instruction key"))?;
    // V3
    let fingerprint = key.fingerprint();
    if fingerprint != expected.server_key_id {
        return Err(AnswerError::KeyMismatch {
            local: hex(&fingerprint),
        });
    }
    // V4
    if fields.instruction_key_id != hex(&fingerprint) {
        return Err(AnswerError::BadField("instruction key ID"));
    }
    // V5
    if !shared_proto::is_canonical_uuid(&fields.agent_id) {
        return Err(AnswerError::BadField("agent ID"));
    }
    if fields.owner_id.is_empty() || fields.owner_id.len() > MAX_FIELD_LEN {
        return Err(AnswerError::BadField("owner ID"));
    }
    let preset = Preset::parse(&fields.preset).ok_or(AnswerError::BadField("preset"))?;
    // V6
    let signature_bytes = Some(&fields.signature)
        .filter(|text| text.len() <= MAX_SIGNATURE_TEXT)
        .and_then(|text| STANDARD.decode(text).ok())
        .filter(|bytes| bytes.len() == SIGNATURE_BYTES)
        .ok_or(AnswerError::BadField("signature"))?;
    // V7: a 64-byte value that is no signature fails here, as one that does not verify.
    let signature =
        Signature::from_bytes(&signature_bytes).map_err(|_| AnswerError::BadSignature)?;
    let transcript = ResponseTranscript::new(
        expected.code,
        expected.agent_key,
        &fields.agent_id,
        &fields.owner_id,
        preset,
        fields.allow_remote,
    )
    .map_err(|_| AnswerError::NotAnAnswer)?;
    verify_response(&key, &transcript, &signature).map_err(|_| AnswerError::BadSignature)?;
    Ok(Verified {
        agent_id: fields.agent_id,
        owner_id: fields.owner_id,
        preset,
        allow_remote: fields.allow_remote,
        instruction_key_pem: fields.instruction_key,
        instruction_key_id: fields.instruction_key_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_crypto::registration::sign_response;
    use shared_crypto::{Signer, SigningKey};

    const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
    const OWNER_ID: &str = "7b1d0c2e-4a5f-4c6d-9e8f-0a1b2c3d4e5f";

    struct Fixture {
        code: RegistrationCode,
        agent: PublicKey,
        server: SigningKey,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                code: RegistrationCode::generate().unwrap(),
                agent: SigningKey::generate().unwrap().public_key(),
                server: SigningKey::generate().unwrap(),
            }
        }

        fn expected(&self) -> Expected<'_> {
            Expected {
                code: &self.code,
                agent_key: &self.agent,
                server_key_id: self.server.public_key().fingerprint(),
            }
        }

        /// A valid answer as a JSON value.
        fn answer(&self) -> serde_json::Value {
            let key = self.server.public_key();
            let transcript = ResponseTranscript::new(
                &self.code,
                &self.agent,
                AGENT_ID,
                OWNER_ID,
                Preset::Personal,
                false,
            )
            .unwrap();
            let signature = sign_response(&self.server, &transcript).unwrap();
            serde_json::json!({
                "agentId": AGENT_ID,
                "ownerId": OWNER_ID,
                "preset": "personal",
                "allowRemote": false,
                "instructionKey": key.to_spki_pem(),
                "instructionKeyId": key.fingerprint_hex(),
                "signature": STANDARD.encode(signature.to_bytes()),
            })
        }

        fn check(&self, value: &serde_json::Value) -> Result<Verified, AnswerError> {
            check(true, value.to_string().as_bytes(), &self.expected())
        }
    }

    #[test]
    fn a_valid_answer_passes() {
        let f = Fixture::new();
        let verified = f.check(&f.answer()).unwrap();
        assert_eq!(verified.agent_id, AGENT_ID);
        assert_eq!(verified.owner_id, OWNER_ID);
        assert_eq!(verified.preset, Preset::Personal);
        assert!(!verified.allow_remote);
    }

    /// V1.
    #[test]
    fn a_body_that_is_not_an_answer_is_refused() {
        let f = Fixture::new();
        let good = f.answer().to_string();
        let expected = f.expected();
        assert_eq!(
            check(false, good.as_bytes(), &expected),
            Err(AnswerError::NotAnAnswer)
        );
        let padded = format!("{good}{}", " ".repeat(MAX_BODY));
        let mut cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"[]".to_vec(),
            b"null".to_vec(),
            b"\"x\"".to_vec(),
            b"{".to_vec(),
            format!("{good} x").into_bytes(),
            format!("{good}{good}").into_bytes(),
            padded.into_bytes(),
        ];
        let object = f.answer();
        for name in object.as_object().unwrap().keys() {
            let mut missing = object.clone();
            missing.as_object_mut().unwrap().remove(name);
            cases.push(missing.to_string().into_bytes());
            let mut wrong = object.clone();
            wrong[name] = serde_json::json!(7);
            cases.push(wrong.to_string().into_bytes());
        }
        let mut extra = object.clone();
        extra["extra"] = "x".into();
        cases.push(extra.to_string().into_bytes());
        // A duplicate field.
        cases.push(
            good.replacen(
                "\"preset\":\"personal\"",
                "\"preset\":\"personal\",\"preset\":\"personal\"",
                1,
            )
            .into_bytes(),
        );
        let mut flag = object.clone();
        flag["allowRemote"] = "false".into();
        cases.push(flag.to_string().into_bytes());
        for body in cases {
            assert_eq!(
                check(true, &body, &expected),
                Err(AnswerError::NotAnAnswer),
                "{}",
                String::from_utf8_lossy(&body[..body.len().min(80)])
            );
        }
    }

    /// V2.
    #[test]
    fn an_instruction_key_that_is_not_a_canonical_pem_is_refused() {
        let f = Fixture::new();
        for key in [
            "".to_owned(),
            "not a key".to_owned(),
            format!("{}\n", f.server.public_key().to_spki_pem()),
            format!("x{}", f.server.public_key().to_spki_pem()),
            f.server.public_key().to_spki_pem().replace('\n', "\r\n"),
        ] {
            let mut answer = f.answer();
            answer["instructionKey"] = key.into();
            assert_eq!(
                f.check(&answer),
                Err(AnswerError::BadField("instruction key"))
            );
        }
    }

    /// V3, K4.
    #[test]
    fn another_servers_key_is_exit_6_with_the_local_fingerprint() {
        let f = Fixture::new();
        let other = SigningKey::generate().unwrap();
        let transcript = ResponseTranscript::new(
            &f.code,
            &f.agent,
            AGENT_ID,
            OWNER_ID,
            Preset::Personal,
            false,
        )
        .unwrap();
        let mut answer = f.answer();
        answer["instructionKey"] = other.public_key().to_spki_pem().into();
        answer["instructionKeyId"] = other.public_key().fingerprint_hex().into();
        answer["signature"] = STANDARD
            .encode(sign_response(&other, &transcript).unwrap().to_bytes())
            .into();
        let error = f.check(&answer).unwrap_err();
        assert_eq!(error.exit_code(), 6);
        assert!(
            error
                .to_string()
                .contains(&other.public_key().fingerprint_hex())
        );
    }

    /// V3 comes before V4, V5 and V6: another key is exit 6 whatever else is wrong.
    #[test]
    fn the_key_check_comes_before_the_later_field_checks() {
        let f = Fixture::new();
        let impostor = SigningKey::generate().unwrap();
        let key = impostor.public_key();
        type Change = fn(&mut serde_json::Value, &str);
        let changes: [Change; 5] = [
            // The key ID is the one the operator gave.
            |v, given| v["instructionKeyId"] = given.into(),
            |v, _| v["agentId"] = "not-a-uuid".into(),
            |v, _| v["ownerId"] = "".into(),
            |v, _| v["preset"] = "other".into(),
            |v, _| v["signature"] = "AAAA".into(),
        ];
        for change in changes {
            let mut answer = f.answer();
            answer["instructionKey"] = key.to_spki_pem().into();
            answer["instructionKeyId"] = key.fingerprint_hex().into();
            change(&mut answer, &f.server.public_key().fingerprint_hex());
            let error = f.check(&answer).unwrap_err();
            assert_eq!(error.exit_code(), 6, "{answer}");
            assert!(matches!(error, AnswerError::KeyMismatch { .. }));
        }
    }

    /// V4 and V5.
    #[test]
    fn the_key_id_and_the_assigned_values_are_checked() {
        let f = Fixture::new();
        let fingerprint = f.server.public_key().fingerprint_hex();
        let cases = [
            (
                "instructionKeyId",
                fingerprint.to_uppercase(),
                "instruction key ID",
            ),
            ("instructionKeyId", "0".repeat(64), "instruction key ID"),
            ("agentId", AGENT_ID.to_uppercase(), "agent ID"),
            ("agentId", String::new(), "agent ID"),
            ("agentId", format!("{AGENT_ID}0"), "agent ID"),
            ("ownerId", String::new(), "owner ID"),
            ("ownerId", "o".repeat(257), "owner ID"),
            ("preset", "Personal".to_owned(), "preset"),
            ("preset", "other".to_owned(), "preset"),
        ];
        for (field, value, name) in cases {
            let mut answer = f.answer();
            answer[field] = value.into();
            assert_eq!(
                f.check(&answer),
                Err(AnswerError::BadField(name)),
                "{field}"
            );
        }
    }

    /// V6.
    #[test]
    fn a_signature_that_is_not_padded_base64_of_64_bytes_is_refused() {
        let f = Fixture::new();
        let good = f.answer()["signature"].as_str().unwrap().to_owned();
        let url_safe = good.replace('+', "-").replace('/', "_");
        for signature in [
            String::new(),
            good.trim_end_matches('=').to_owned(),
            format!("{good}="),
            good[..good.len() - 4].to_owned(),
            STANDARD.encode([1u8; 63]),
            STANDARD.encode([1u8; 65]),
            "A".repeat(200),
            format!("{good}\n"),
            "!".repeat(88),
        ] {
            let mut answer = f.answer();
            answer["signature"] = signature.clone().into();
            assert_eq!(
                f.check(&answer),
                Err(AnswerError::BadField("signature")),
                "{signature}"
            );
        }
        // The URL-safe alphabet differs only when the signature has '+' or '/'.
        if url_safe != good {
            let mut answer = f.answer();
            answer["signature"] = url_safe.into();
            assert_eq!(f.check(&answer), Err(AnswerError::BadField("signature")));
        }
    }

    /// V7.
    #[test]
    fn a_signature_that_does_not_verify_is_exit_6() {
        let f = Fixture::new();
        // Another code, another agent key, another value: each changes the transcript.
        let other_code = RegistrationCode::generate().unwrap();
        let other_agent = SigningKey::generate().unwrap().public_key();
        for (code, agent) in [(&other_code, &f.agent), (&f.code, &other_agent)] {
            let g = Fixture {
                code: code.clone(),
                agent: *agent,
                server: SigningKey::from_pkcs8_der(&f.server.to_pkcs8_der()).unwrap(),
            };
            // The answer was made for `f`'s code and agent; `g` is what the agent holds.
            let error = g.check(&f.answer()).unwrap_err();
            assert_eq!(error, AnswerError::BadSignature);
            assert_eq!(error.exit_code(), 6);
        }
        for (field, value) in [
            (
                "agentId",
                serde_json::json!("0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f"),
            ),
            ("ownerId", "someone-else".into()),
            ("preset", "shared".into()),
            ("allowRemote", true.into()),
        ] {
            let mut answer = f.answer();
            answer[field] = value;
            assert_eq!(f.check(&answer), Err(AnswerError::BadSignature), "{field}");
        }
        // 64 bytes that are no signature at all.
        let mut answer = f.answer();
        answer["signature"] = STANDARD.encode([0u8; 64]).into();
        assert_eq!(f.check(&answer), Err(AnswerError::BadSignature));
    }
}
