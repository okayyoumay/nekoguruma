//! Agent registration: the code, the agent's proof of possession and the server's signed
//! answer (ADR-463, design 6.4 and 6.7).
//!
//! An agent registers with a single-use registration code, and the request carries its public
//! key. Two signatures protect the exchange:
//!
//! - the **request proof**, made with the key being registered, shows that the sender holds the
//!   private half, so the server never records a key its owner cannot use. The connection proof
//!   ([`crate::proof`]) cannot serve here: it names the agent ID and a server challenge, and
//!   neither exists before registration. The freshness comes from the code, which the server
//!   redeems once;
//! - the **response signature**, made with the server's instruction key, covers the code's hash,
//!   the agent key's fingerprint and what the server assigned (agent ID, owner ID, preset,
//!   remote use). It stops a process that holds the port from replaying the request to the real
//!   server within the code's life and rewriting the answer: the agent verifies it against the
//!   instruction key it is about to pin.
//!
//! # The code
//!
//! [`RegistrationCode`] is the one parser for both sides. A code is 128 random bits written as
//! the prefix `ngr1-` and 26 Crockford base32 digits (130 bits, so the first digit is 0 to 7).
//! The canonical text is the prefix in lower case and the digits in upper case. Parsing trims
//! ASCII whitespace at both ends (a pasted line break), reads the prefix and the digits without
//! regard to case, and reads `I` and `L` as `1` and `O` as `0`, as Crockford's alphabet allows;
//! it accepts no separator inside the code and no `U`. The code's hash is the SHA-256 of the
//! canonical text, so every typed variant of one code hashes alike.
//!
//! # Encoding and domain separation
//!
//! Each transcript body is the layout version ([`PROTOCOL_VERSION`], 4 bytes big-endian, always
//! written by this crate), then its fields, each a 4-byte big-endian length followed by its
//! bytes, so two different field sets never encode to the same bytes. The request transcript
//! holds the code's SHA-256, the agent key's fingerprint, the mode as UTF-8 text and the
//! server key ID (32 bytes). The response transcript holds the code's SHA-256, the agent key's
//! fingerprint, the agent ID and the owner ID as UTF-8 text, the preset as UTF-8 text and one
//! byte for remote use.
//!
//! Each body is signed through [`sign_tagged`] under its own tag ([`DOMAIN_TAG`],
//! [`RESPONSE_DOMAIN_TAG`]), and neither equals the connection proof's or the job
//! instruction's. A new signed purpose must pick a new tag and never reuse one of these. The
//! version lives only in the body, so a layout change bumps [`PROTOCOL_VERSION`] and the tags
//! stay.

use std::fmt;

use sha2::{Digest, Sha256};

use crate::{
    PublicKey, RandomError, SignError, Signature, Signer, VerifyError, random_32, sign_tagged,
    verify_tagged,
};

/// The tag that names the request proof in every signed transcript.
pub const DOMAIN_TAG: &[u8] = b"nekoguruma agent registration";

/// The tag that names the server's response signature in every signed transcript.
pub const RESPONSE_DOMAIN_TAG: &[u8] = b"nekoguruma agent registration response";

/// The transcript layout version this crate writes into every transcript.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest agent ID or owner ID accepted, in bytes.
pub const MAX_FIELD_LEN: usize = 256;

/// The version prefix of a registration code, in its canonical (lower-case) form.
pub const CODE_PREFIX: &str = "ngr1-";

/// The number of digits after the prefix.
pub const CODE_DIGITS: usize = 26;

/// Crockford's base32 digits, in canonical (upper-case) form.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A registration code that could not be read.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodeError {
    /// The text does not start with the version prefix.
    #[error("the registration code lacks its version prefix")]
    Prefix,
    /// The text after the prefix is not 26 characters long.
    #[error("the registration code has the wrong length")]
    Length,
    /// A character is not a base32 digit, or the first digit is above 7.
    #[error("the registration code has a character that is not allowed")]
    Alphabet,
}

/// A registration code in its canonical form. The text is a secret: [`Debug`](fmt::Debug)
/// shows only the hash's first bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct RegistrationCode(String);

impl RegistrationCode {
    /// Makes a code from 128 random bits of operating-system randomness.
    pub fn generate() -> Result<Self, RandomError> {
        let bytes = random_32()?;
        let mut head = [0u8; 16];
        head.copy_from_slice(&bytes[..16]);
        Ok(Self::from_bits(u128::from_be_bytes(head)))
    }

    fn from_bits(value: u128) -> Self {
        let mut text = String::with_capacity(CODE_PREFIX.len() + CODE_DIGITS);
        text.push_str(CODE_PREFIX);
        for i in 0..CODE_DIGITS {
            let shift = 5 * (CODE_DIGITS - 1 - i);
            text.push(char::from(ALPHABET[((value >> shift) & 31) as usize]));
        }
        Self(text)
    }

    /// Reads a code as typed or pasted. See the module's rules.
    pub fn parse(input: &str) -> Result<Self, CodeError> {
        let input = input.trim_matches(|c: char| c.is_ascii_whitespace());
        let (prefix, digits) = input
            .split_at_checked(CODE_PREFIX.len())
            .ok_or(CodeError::Prefix)?;
        if !prefix.eq_ignore_ascii_case(CODE_PREFIX) {
            return Err(CodeError::Prefix);
        }
        if digits.len() != CODE_DIGITS {
            return Err(CodeError::Length);
        }
        let mut text = String::with_capacity(input.len());
        text.push_str(CODE_PREFIX);
        for (i, byte) in digits.bytes().enumerate() {
            let canonical = match byte.to_ascii_uppercase() {
                b'I' | b'L' => b'1',
                b'O' => b'0',
                other => other,
            };
            let value = ALPHABET
                .iter()
                .position(|&d| d == canonical)
                .ok_or(CodeError::Alphabet)?;
            if i == 0 && value > 7 {
                return Err(CodeError::Alphabet);
            }
            text.push(char::from(canonical));
        }
        Ok(Self(text))
    }

    /// The canonical text, which the operator copies to the device.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The SHA-256 of the canonical text: the key of the server's record, and what the
    /// transcripts cover.
    pub fn sha256(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_bytes()).into()
    }
}

impl fmt::Debug for RegistrationCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hash = self.sha256();
        write!(
            f,
            "RegistrationCode(sha256 {:02x}{:02x}{:02x}{:02x}..)",
            hash[0], hash[1], hash[2], hash[3]
        )
    }
}

/// The mode an agent registers in (design 6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The agent runs for one user.
    User,
    /// The agent runs for the device.
    Device,
}

impl Mode {
    /// The mode's name in the Web API and on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::User => "user",
            Mode::Device => "device",
        }
    }

    /// Reads a mode name; any other text is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "user" => Some(Mode::User),
            "device" => Some(Mode::Device),
            _ => None,
        }
    }
}

/// The connection restriction preset the server assigns (design 6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Owner only, remote denied.
    Personal,
    /// Owner only, remote allowed.
    PersonalRemote,
    /// Managed by the allowed-operator list.
    Shared,
}

impl Preset {
    /// The preset's name in the Web API.
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::Personal => "personal",
            Preset::PersonalRemote => "personalRemote",
            Preset::Shared => "shared",
        }
    }

    /// Reads a preset name; any other text is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "personal" => Some(Preset::Personal),
            "personalRemote" => Some(Preset::PersonalRemote),
            "shared" => Some(Preset::Shared),
            _ => None,
        }
    }
}

/// A transcript could not be built.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TranscriptError {
    /// The agent ID or the owner ID is empty, or longer than [`MAX_FIELD_LEN`].
    #[error("{field} must be 1 to {MAX_FIELD_LEN} bytes")]
    BadField {
        /// `agent_id` or `owner_id`.
        field: &'static str,
    },
}

fn put_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("fields are length-checked");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// What a registration request's proof covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationTranscript {
    code_hash: [u8; 32],
    agent_key_fingerprint: [u8; 32],
    mode: Mode,
    server_key_id: [u8; 32],
}

impl RegistrationTranscript {
    /// Builds a transcript. `agent_key` is the key being registered and `server_key_id` the
    /// fingerprint of the server's instruction key, which the operator gave the agent.
    pub fn new(
        code: &RegistrationCode,
        agent_key: &PublicKey,
        mode: Mode,
        server_key_id: [u8; 32],
    ) -> Self {
        Self {
            code_hash: code.sha256(),
            agent_key_fingerprint: agent_key.fingerprint(),
            mode,
            server_key_id,
        }
    }

    /// The body that is signed under [`DOMAIN_TAG`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 4 * 4 + 3 * 32 + 6);
        out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        put_prefixed(&mut out, &self.code_hash);
        put_prefixed(&mut out, &self.agent_key_fingerprint);
        put_prefixed(&mut out, self.mode.as_str().as_bytes());
        put_prefixed(&mut out, &self.server_key_id);
        out
    }
}

/// Signs `transcript` with the key being registered.
pub fn sign_registration<S: Signer + ?Sized>(
    signer: &S,
    transcript: &RegistrationTranscript,
) -> Result<Signature, SignError> {
    sign_tagged(signer, DOMAIN_TAG, &transcript.encode())
}

/// Verifies a registration proof against the public key the request carried. A transcript that
/// was built for another key fails, whatever the signature.
pub fn verify_registration(
    key: &PublicKey,
    transcript: &RegistrationTranscript,
    signature: &Signature,
) -> Result<(), VerifyError> {
    if transcript.agent_key_fingerprint != key.fingerprint() {
        return Err(VerifyError::Mismatch);
    }
    verify_tagged(key, DOMAIN_TAG, &transcript.encode(), signature)
}

/// What the server's response signature covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseTranscript {
    code_hash: [u8; 32],
    agent_key_fingerprint: [u8; 32],
    agent_id: String,
    owner_id: String,
    preset: Preset,
    allow_remote: bool,
}

impl ResponseTranscript {
    /// Builds a transcript of what the server returns for the request made with `code` and
    /// `agent_key`.
    pub fn new(
        code: &RegistrationCode,
        agent_key: &PublicKey,
        agent_id: &str,
        owner_id: &str,
        preset: Preset,
        allow_remote: bool,
    ) -> Result<Self, TranscriptError> {
        for (field, value) in [("agent_id", agent_id), ("owner_id", owner_id)] {
            if value.is_empty() || value.len() > MAX_FIELD_LEN {
                return Err(TranscriptError::BadField { field });
            }
        }
        Ok(Self {
            code_hash: code.sha256(),
            agent_key_fingerprint: agent_key.fingerprint(),
            agent_id: agent_id.to_owned(),
            owner_id: owner_id.to_owned(),
            preset,
            allow_remote,
        })
    }

    /// The body that is signed under [`RESPONSE_DOMAIN_TAG`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            4 + 6 * 4 + 2 * 32 + self.agent_id.len() + self.owner_id.len() + 15 + 1,
        );
        out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        put_prefixed(&mut out, &self.code_hash);
        put_prefixed(&mut out, &self.agent_key_fingerprint);
        put_prefixed(&mut out, self.agent_id.as_bytes());
        put_prefixed(&mut out, self.owner_id.as_bytes());
        put_prefixed(&mut out, self.preset.as_str().as_bytes());
        put_prefixed(&mut out, &[u8::from(self.allow_remote)]);
        out
    }
}

/// Signs `transcript` with the server's instruction key.
pub fn sign_response<S: Signer + ?Sized>(
    signer: &S,
    transcript: &ResponseTranscript,
) -> Result<Signature, SignError> {
    sign_tagged(signer, RESPONSE_DOMAIN_TAG, &transcript.encode())
}

/// Verifies the server's response signature against the instruction key the response carried
/// (after the agent compared its fingerprint with the server key ID it was given).
pub fn verify_response(
    key: &PublicKey,
    transcript: &ResponseTranscript,
    signature: &Signature,
) -> Result<(), VerifyError> {
    verify_tagged(key, RESPONSE_DOMAIN_TAG, &transcript.encode(), signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SignErrorKind, SigningKey, instruction, proof};

    fn code() -> RegistrationCode {
        RegistrationCode::generate().unwrap()
    }

    fn server_key_id() -> [u8; 32] {
        SigningKey::generate().unwrap().public_key().fingerprint()
    }

    fn uuid_text() -> String {
        let b = random_32().unwrap();
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-8{:01x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0],
            b[1],
            b[2],
            b[3],
            b[4],
            b[5],
            b[6] & 15,
            b[7],
            b[8] & 15,
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15]
        )
    }

    // ---------------------------------------------------------------- code

    #[test]
    fn a_generated_code_parses_to_itself() {
        let c = code();
        assert!(c.as_str().starts_with(CODE_PREFIX));
        assert_eq!(c.as_str().len(), CODE_PREFIX.len() + CODE_DIGITS);
        assert_eq!(RegistrationCode::parse(c.as_str()).unwrap(), c);
        assert_ne!(code(), code(), "codes are random");
        assert!(!format!("{c:?}").contains(&c.as_str()[CODE_PREFIX.len()..]));
    }

    #[test]
    fn the_encoding_is_big_endian_crockford() {
        assert_eq!(
            RegistrationCode::from_bits(0).as_str(),
            "ngr1-00000000000000000000000000"
        );
        assert_eq!(
            RegistrationCode::from_bits(u128::MAX).as_str(),
            "ngr1-7ZZZZZZZZZZZZZZZZZZZZZZZZZ"
        );
        assert_eq!(
            RegistrationCode::from_bits(1).as_str(),
            "ngr1-00000000000000000000000001"
        );
    }

    #[test]
    fn typed_variants_hash_alike() {
        let c = code();
        let canonical = c.as_str().to_owned();
        let digits = &canonical[CODE_PREFIX.len()..];
        let variants = [
            canonical.to_uppercase(),
            canonical.to_lowercase(),
            format!("{canonical}\n"),
            format!("  \t{canonical}\r\n"),
            format!("NGR1-{digits}"),
            format!("Ngr1-{}", digits.to_lowercase()),
        ];
        for v in variants {
            let parsed = RegistrationCode::parse(&v).unwrap();
            assert_eq!(parsed.sha256(), c.sha256(), "{v:?}");
            assert_eq!(parsed.as_str(), canonical);
        }
    }

    #[test]
    fn confusable_letters_read_as_crockford_says() {
        let a = RegistrationCode::parse("ngr1-0110abcdefghjkmnpqrstvwxyz").unwrap();
        let b = RegistrationCode::parse("ngr1-OIl0abcdefghjkmnpqrstvwxyz").unwrap();
        let c = RegistrationCode::parse("ngr1-oLi0ABCDEFGHJKMNPQRSTVWXYZ").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn malformed_codes_are_refused() {
        let good = code();
        let digits = good.as_str()[CODE_PREFIX.len()..].to_owned();
        let bad: [(String, CodeError); 10] = [
            (String::new(), CodeError::Prefix),
            (digits.clone(), CodeError::Prefix),
            (format!("ngr2-{digits}"), CodeError::Prefix),
            (format!("ngr1{digits}"), CodeError::Prefix),
            (format!("ngr1-{}", &digits[1..]), CodeError::Length),
            (format!("ngr1-{digits}0"), CodeError::Length),
            (format!("ngr1-U{}", &digits[1..]), CodeError::Alphabet),
            (format!("ngr1-{}-", &digits[..25]), CodeError::Alphabet),
            (format!("ngr1-8{}", &digits[1..]), CodeError::Alphabet),
            (format!("ngr1- {}", &digits[1..]), CodeError::Alphabet),
        ];
        for (text, want) in bad {
            assert_eq!(RegistrationCode::parse(&text), Err(want), "{text:?}");
        }
        // Non-ASCII text, including one that splits a character at the prefix length.
        assert!(RegistrationCode::parse("ngr1\u{e9}0000000000000000000000000").is_err());
        assert!(RegistrationCode::parse("\u{e9}\u{e9}\u{e9}").is_err());
    }

    // ------------------------------------------------------------- request

    fn request(c: &RegistrationCode, key: &PublicKey, id: [u8; 32]) -> RegistrationTranscript {
        RegistrationTranscript::new(c, key, Mode::Device, id)
    }

    #[test]
    fn a_registration_proof_verifies() {
        let key = SigningKey::generate().unwrap();
        let t = request(&code(), &key.public_key(), server_key_id());
        let sig = sign_registration(&key, &t).unwrap();
        verify_registration(&key.public_key(), &t, &sig).unwrap();
    }

    #[test]
    fn the_request_encoding_is_the_documented_layout() {
        let key = SigningKey::generate().unwrap();
        let c = code();
        let id = server_key_id();
        let t = RegistrationTranscript::new(&c, &key.public_key(), Mode::User, id);
        let mut want = Vec::new();
        want.extend(PROTOCOL_VERSION.to_be_bytes());
        want.extend(32u32.to_be_bytes());
        want.extend(Sha256::digest(c.as_str().as_bytes()));
        want.extend(32u32.to_be_bytes());
        want.extend(key.public_key().fingerprint());
        want.extend(4u32.to_be_bytes());
        want.extend(b"user");
        want.extend(32u32.to_be_bytes());
        want.extend(id);
        assert_eq!(t.encode(), want);
    }

    #[test]
    fn every_changed_request_field_fails() {
        let key = SigningKey::generate().unwrap();
        let pk = key.public_key();
        let (c, id) = (code(), server_key_id());
        let t = request(&c, &pk, id);
        let sig = sign_registration(&key, &t).unwrap();
        let other_key = SigningKey::generate().unwrap().public_key();
        let cases = [
            request(&code(), &pk, id),
            request(&c, &other_key, id),
            RegistrationTranscript::new(&c, &pk, Mode::User, id),
            request(&c, &pk, server_key_id()),
        ];
        for changed in cases {
            assert_eq!(
                verify_registration(&pk, &changed, &sig),
                Err(VerifyError::Mismatch),
                "{changed:?}"
            );
        }
        // Another key's signature over the same transcript fails too.
        let foreign = sign_registration(&SigningKey::generate().unwrap(), &t).unwrap();
        assert_eq!(
            verify_registration(&pk, &t, &foreign),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn a_transcript_for_another_key_is_refused_even_with_that_keys_signature() {
        let signer = SigningKey::generate().unwrap();
        let claimed = SigningKey::generate().unwrap().public_key();
        // The transcript names `claimed`, but `signer` made the signature and is the key given.
        let t = request(&code(), &claimed, server_key_id());
        let sig = sign_registration(&signer, &t).unwrap();
        assert_eq!(
            verify_registration(&signer.public_key(), &t, &sig),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn a_changed_request_version_fails() {
        let key = SigningKey::generate().unwrap();
        let t = request(&code(), &key.public_key(), server_key_id());
        let mut body = t.encode();
        body[..4].copy_from_slice(&(PROTOCOL_VERSION + 1).to_be_bytes());
        let sig = sign_tagged(&key, DOMAIN_TAG, &body).unwrap();
        assert!(verify_registration(&key.public_key(), &t, &sig).is_err());
    }

    #[test]
    fn mode_and_preset_names_round_trip_and_refuse_others() {
        for m in [Mode::User, Mode::Device] {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
        }
        for p in [Preset::Personal, Preset::PersonalRemote, Preset::Shared] {
            assert_eq!(Preset::parse(p.as_str()), Some(p));
        }
        for bad in ["", "machine", "User", "DEVICE", "device "] {
            assert_eq!(Mode::parse(bad), None, "{bad:?}");
        }
        for bad in ["", "Personal", "personal_remote", "admin"] {
            assert_eq!(Preset::parse(bad), None, "{bad:?}");
        }
    }

    // ------------------------------------------------------------ response

    struct Fixture {
        code: RegistrationCode,
        agent: PublicKey,
        agent_id: String,
        owner_id: String,
    }

    fn fixture() -> Fixture {
        Fixture {
            code: code(),
            agent: SigningKey::generate().unwrap().public_key(),
            agent_id: uuid_text(),
            owner_id: uuid_text(),
        }
    }

    fn response(f: &Fixture) -> ResponseTranscript {
        ResponseTranscript::new(
            &f.code,
            &f.agent,
            &f.agent_id,
            &f.owner_id,
            Preset::Personal,
            false,
        )
        .unwrap()
    }

    #[test]
    fn a_response_signature_verifies() {
        let server = SigningKey::generate().unwrap();
        let f = fixture();
        let sig = sign_response(&server, &response(&f)).unwrap();
        verify_response(&server.public_key(), &response(&f), &sig).unwrap();
        // Another server key does not verify it.
        let other = SigningKey::generate().unwrap().public_key();
        assert_eq!(
            verify_response(&other, &response(&f), &sig),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn the_response_encoding_is_the_documented_layout() {
        let f = fixture();
        let t = ResponseTranscript::new(&f.code, &f.agent, "a", "bc", Preset::PersonalRemote, true)
            .unwrap();
        let mut want = Vec::new();
        want.extend(PROTOCOL_VERSION.to_be_bytes());
        want.extend(32u32.to_be_bytes());
        want.extend(f.code.sha256());
        want.extend(32u32.to_be_bytes());
        want.extend(f.agent.fingerprint());
        want.extend(1u32.to_be_bytes());
        want.extend(b"a");
        want.extend(2u32.to_be_bytes());
        want.extend(b"bc");
        want.extend(14u32.to_be_bytes());
        want.extend(b"personalRemote");
        want.extend(1u32.to_be_bytes());
        want.push(1);
        assert_eq!(t.encode(), want);
    }

    #[test]
    fn every_changed_response_field_fails() {
        let server = SigningKey::generate().unwrap();
        let pk = server.public_key();
        let f = fixture();
        let sig = sign_response(&server, &response(&f)).unwrap();
        let make = |c: &RegistrationCode, k: &PublicKey, a: &str, o: &str, p, r| {
            ResponseTranscript::new(c, k, a, o, p, r).unwrap()
        };
        let cases = [
            make(
                &code(),
                &f.agent,
                &f.agent_id,
                &f.owner_id,
                Preset::Personal,
                false,
            ),
            make(
                &f.code,
                &SigningKey::generate().unwrap().public_key(),
                &f.agent_id,
                &f.owner_id,
                Preset::Personal,
                false,
            ),
            make(
                &f.code,
                &f.agent,
                &uuid_text(),
                &f.owner_id,
                Preset::Personal,
                false,
            ),
            make(
                &f.code,
                &f.agent,
                &f.agent_id,
                &uuid_text(),
                Preset::Personal,
                false,
            ),
            make(
                &f.code,
                &f.agent,
                &f.agent_id,
                &f.owner_id,
                Preset::Shared,
                false,
            ),
            make(
                &f.code,
                &f.agent,
                &f.agent_id,
                &f.owner_id,
                Preset::Personal,
                true,
            ),
            // Agent ID and owner ID swapped.
            make(
                &f.code,
                &f.agent,
                &f.owner_id,
                &f.agent_id,
                Preset::Personal,
                false,
            ),
        ];
        for changed in cases {
            assert_eq!(
                verify_response(&pk, &changed, &sig),
                Err(VerifyError::Mismatch),
                "{changed:?}"
            );
        }
    }

    #[test]
    fn response_fields_are_length_checked_and_unambiguous() {
        let f = fixture();
        let new = |a: &str, o: &str| {
            ResponseTranscript::new(&f.code, &f.agent, a, o, Preset::Personal, false)
        };
        assert_eq!(
            new("", "o"),
            Err(TranscriptError::BadField { field: "agent_id" })
        );
        assert_eq!(
            new("a", ""),
            Err(TranscriptError::BadField { field: "owner_id" })
        );
        let long = "x".repeat(MAX_FIELD_LEN + 1);
        assert!(new(&long, "o").is_err());
        assert!(new("a", &long).is_err());
        assert!(new(&"x".repeat(MAX_FIELD_LEN), "o").is_ok());
        assert_ne!(
            new("ab", "c").unwrap().encode(),
            new("a", "bc").unwrap().encode()
        );
    }

    // --------------------------------------------------------- cross-tag

    #[test]
    fn the_four_tags_are_distinct_and_signatures_do_not_cross() {
        let tags: [&[u8]; 4] = [
            DOMAIN_TAG,
            RESPONSE_DOMAIN_TAG,
            proof::DOMAIN_TAG,
            instruction::DOMAIN_TAG,
        ];
        for (i, a) in tags.iter().enumerate() {
            for b in &tags[i + 1..] {
                assert_ne!(a, b);
            }
        }
        let key = SigningKey::generate().unwrap();
        let pk = key.public_key();
        let f = fixture();
        let req = request(&f.code, &f.agent, server_key_id());
        let resp = response(&f);
        let req_sig = sign_registration(&key, &req).unwrap();
        let resp_sig = sign_response(&key, &resp).unwrap();
        // A signature of one purpose does not verify as the other registration purpose.
        assert!(verify_response(&pk, &resp, &req_sig).is_err());
        assert!(verify_tagged(&pk, RESPONSE_DOMAIN_TAG, &req.encode(), &req_sig).is_err());
        assert!(verify_tagged(&pk, DOMAIN_TAG, &resp.encode(), &resp_sig).is_err());
        // Nor under the proof's or the instruction's tag, and theirs are not accepted here.
        for tag in [proof::DOMAIN_TAG, instruction::DOMAIN_TAG] {
            assert!(verify_tagged(&pk, tag, &req.encode(), &req_sig).is_err());
            assert!(verify_tagged(&pk, tag, &resp.encode(), &resp_sig).is_err());
            let other_req = sign_tagged(&key, tag, &req.encode()).unwrap();
            let other_resp = sign_tagged(&key, tag, &resp.encode()).unwrap();
            assert!(verify_registration(&pk, &req, &other_req).is_err());
            assert!(verify_response(&pk, &resp, &other_resp).is_err());
        }
        // A request transcript for the signer's own key, signed under another tag.
        let own = RegistrationTranscript::new(&f.code, &pk, Mode::Device, [0; 32]);
        let foreign = sign_tagged(&key, proof::DOMAIN_TAG, &own.encode()).unwrap();
        assert!(verify_registration(&pk, &own, &foreign).is_err());
        // The same bodies signed untagged do not verify either.
        let raw = key.sign(&resp.encode()).unwrap();
        assert!(verify_response(&pk, &resp, &raw).is_err());
    }

    // ------------------------------------------------------- signatures

    #[test]
    fn wrong_signature_lengths_are_refused() {
        let key = SigningKey::generate().unwrap();
        let t = request(&code(), &key.public_key(), server_key_id());
        let bytes = sign_registration(&key, &t).unwrap().to_bytes();
        assert!(Signature::from_bytes(&bytes[..63]).is_err());
        let mut long = bytes.to_vec();
        long.push(0);
        assert!(Signature::from_bytes(&long).is_err());
        assert!(Signature::from_bytes(&[]).is_err());
    }

    #[test]
    fn signing_failure_is_reported_without_detail() {
        struct Broken(SigningKey);
        impl Signer for Broken {
            fn public_key(&self) -> PublicKey {
                self.0.public_key()
            }
            fn sign(&self, _: &[u8]) -> Result<Signature, SignError> {
                Err(SignError::with_source(
                    SignErrorKind::Unavailable,
                    "collection locked",
                ))
            }
        }
        let broken = Broken(SigningKey::generate().unwrap());
        let f = fixture();
        let err =
            sign_registration(&broken, &request(&f.code, &f.agent, server_key_id())).unwrap_err();
        assert_eq!(err.kind(), SignErrorKind::Unavailable);
        assert!(!err.to_string().contains("collection"));
        let err = sign_response(&broken, &response(&f)).unwrap_err();
        assert_eq!(err.kind(), SignErrorKind::Unavailable);
        assert!(
            std::error::Error::source(&err)
                .unwrap()
                .to_string()
                .contains("collection")
        );
    }
}
