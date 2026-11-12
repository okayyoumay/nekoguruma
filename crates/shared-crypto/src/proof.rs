//! The agent's connection proof (ADR-459, design 6.4 and 14.4).
//!
//! The agent proves it holds its key by signing a transcript that binds the server it talks to,
//! its own identity and two fresh values: a challenge the server just issued and a nonce the
//! agent chose. The transcript has no timestamp, because the device clock is untrusted
//! (design 16.3). The server issues each challenge once and refuses a repeat, which is what
//! stops replay.
//!
//! # Encoding and domain separation
//!
//! The transcript body is the protocol version ([`PROTOCOL_VERSION`], 4 bytes big-endian,
//! always written by this crate), the audience and the agent ID (each a 4-byte big-endian
//! length, then UTF-8 bytes), then the challenge and the nonce (32 bytes each). Variable-length
//! fields are length-prefixed, so two different field sets never encode to the same bytes.
//!
//! The body is signed through [`sign_tagged`] under [`DOMAIN_TAG`], which names this purpose
//! and is length-prefixed in front of the body. Job instructions are signed with the same
//! key primitive (design 11.1) through the same pair under a tag of their own
//! ([`crate::instruction::DOMAIN_TAG`]), and a verifier
//! of either purpose rebuilds its own message from its own tag. A proof signature therefore
//! does not verify as a job instruction, and the reverse. A new signed purpose must pick a new
//! tag and never reuse this one. The version lives only in the body, so a layout change bumps
//! [`PROTOCOL_VERSION`] and the tag stays.

use crate::{PublicKey, SignError, Signature, Signer, VerifyError, sign_tagged, verify_tagged};

/// The tag that names this purpose in every signed transcript.
pub const DOMAIN_TAG: &[u8] = b"nekoguruma agent connection proof";

/// The transcript layout version this crate writes into every transcript.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest audience or agent ID accepted, in bytes.
pub const MAX_FIELD_LEN: usize = 1024;

/// Length of the server challenge and of the agent nonce.
pub const VALUE_LEN: usize = 32;

/// An audience string was refused. The variants name the rule, never the input.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AudienceError {
    /// Empty, longer than [`MAX_FIELD_LEN`], or holding a character outside the allowed set
    /// (whitespace, control characters, non-ASCII).
    #[error("the audience is empty, too long or holds a character that is not allowed")]
    BadCharacter,
    /// The scheme is neither `http` nor `https`, or the `scheme://` prefix is missing.
    #[error("the audience's scheme must be http or https")]
    BadScheme,
    /// User information (`user@`) is present.
    #[error("the audience must not carry user information")]
    Userinfo,
    /// A path other than one trailing slash, a query or a fragment is present.
    #[error("the audience must be an origin: no path, query or fragment")]
    NotAnOrigin,
    /// The host is empty or not a host name, an IPv4 address or a bracketed IPv6 address.
    #[error("the audience's host is not valid, or an IP address not in its own canonical form")]
    BadHost,
    /// `localhost` is not an audience: the local profile pins an IP address (ADR-456).
    #[error("the audience must not name localhost")]
    Localhost,
    /// The port is not a number from 1 to 65535 written without leading zeros.
    #[error("the audience's port is not valid")]
    BadPort,
    /// The text is not in canonical form (strict parsing only).
    #[error("the audience is not in canonical form")]
    NotCanonical,
}

/// The audience of a connection proof: the origin of the server the agent talks to, in one
/// canonical text (design 14.4, ADR-466).
///
/// The canonical form is `scheme://host[:port]`: the scheme and host in lower case, the port
/// left out when it is the scheme's default (80 for `http`, 443 for `https`), and no path, no
/// trailing slash, no query, no fragment and no user information. It is the exact string the
/// transcript signs.
///
/// The type is syntactic. It accepts both `http` and `https`; whether an agent may use `https`
/// at all is the agent's own decision (it refuses it until the standard profile picks its TLS,
/// ADR-463 item 13). `localhost` is refused, never rewritten to `127.0.0.1`: the local profile
/// answers it with a redirect that an agent treats as an error (ADR-456, design 13.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audience(String);

impl Audience {
    /// Canonicalises `input`, for the one place that writes it (the registration record, from
    /// the `--server` origin an operator typed). Accepts one trailing slash, an explicit default
    /// port and upper-case letters in the scheme and host, and writes them away.
    pub fn canonicalise(input: &str) -> Result<Self, AudienceError> {
        if input.is_empty()
            || input.len() > MAX_FIELD_LEN
            || !input.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(AudienceError::BadCharacter);
        }
        let (scheme, rest) = input.split_once("://").ok_or(AudienceError::BadScheme)?;
        let (scheme, default_port) = if scheme.eq_ignore_ascii_case("http") {
            ("http", 80)
        } else if scheme.eq_ignore_ascii_case("https") {
            ("https", 443)
        } else {
            return Err(AudienceError::BadScheme);
        };
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.contains(['/', '?', '#']) {
            return Err(AudienceError::NotAnOrigin);
        }
        if authority.contains('@') {
            return Err(AudienceError::Userinfo);
        }
        let (host, port) = split_host_port(authority)?;
        let host = canonical_host(host)?;
        let mut out = format!("{scheme}://{host}");
        if let Some(port) = port {
            let port = parse_port(port)?;
            if port != default_port {
                out.push(':');
                out.push_str(&port.to_string());
            }
        }
        if out.len() > MAX_FIELD_LEN {
            return Err(AudienceError::BadCharacter);
        }
        Ok(Self(out))
    }

    /// Parses an audience that must already be canonical, as a verifier does with the text
    /// it builds itself: anything [`Audience::canonicalise`] would change is refused.
    pub fn parse_canonical(input: &str) -> Result<Self, AudienceError> {
        let audience = Self::canonicalise(input)?;
        if audience.0 == input {
            Ok(audience)
        } else {
            Err(AudienceError::NotCanonical)
        }
    }

    /// The canonical text, which is what the transcript signs.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Audience {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Splits `host[:port]`, keeping a bracketed IPv6 literal whole.
fn split_host_port(authority: &str) -> Result<(&str, Option<&str>), AudienceError> {
    if authority.starts_with('[') {
        let end = authority.find(']').ok_or(AudienceError::BadHost)?;
        let host = &authority[..=end];
        return match &authority[end + 1..] {
            "" => Ok((host, None)),
            rest => rest
                .strip_prefix(':')
                .map(|p| (host, Some(p)))
                .ok_or(AudienceError::BadHost),
        };
    }
    match authority.split_once(':') {
        Some((host, port)) => Ok((host, Some(port))),
        None => Ok((authority, None)),
    }
}

/// Lowers the case of `host` and checks it. An IPv6 literal is rewritten through `Ipv6Addr`'s
/// text form (brackets kept). A host whose last label is all digits must be an IPv4 address that
/// `Ipv4Addr` parses, which refuses leading zeros and short forms; it is refused otherwise
/// rather than rewritten, since a number-like name could mean different addresses to different
/// parsers.
fn canonical_host(host: &str) -> Result<String, AudienceError> {
    let host = host.to_ascii_lowercase();
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        let addr: std::net::Ipv6Addr = inner.parse().map_err(|_| AudienceError::BadHost)?;
        return Ok(format!("[{addr}]"));
    }
    if host.starts_with('[') {
        return Err(AudienceError::BadHost);
    }
    let name = host.strip_suffix('.').unwrap_or(&host);
    if name == "localhost" || name.ends_with(".localhost") {
        return Err(AudienceError::Localhost);
    }
    let labels_ok = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    if !labels_ok {
        return Err(AudienceError::BadHost);
    }
    let last = host.rsplit('.').next().unwrap_or("");
    if last.bytes().all(|b| b.is_ascii_digit()) {
        let addr: std::net::Ipv4Addr = host.parse().map_err(|_| AudienceError::BadHost)?;
        return Ok(addr.to_string());
    }
    Ok(host)
}

fn parse_port(text: &str) -> Result<u16, AudienceError> {
    if text.is_empty()
        || text.len() > 5
        || !text.bytes().all(|b| b.is_ascii_digit())
        || text.starts_with('0')
    {
        return Err(AudienceError::BadPort);
    }
    text.parse::<u16>().map_err(|_| AudienceError::BadPort)
}

/// A transcript could not be built.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TranscriptError {
    /// The agent ID is empty, or longer than [`MAX_FIELD_LEN`].
    #[error("{field} must be 1 to {MAX_FIELD_LEN} bytes")]
    BadField {
        /// `agent_id`.
        field: &'static str,
    },
}

/// What a connection proof covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofTranscript {
    audience: String,
    agent_id: String,
    challenge: [u8; VALUE_LEN],
    nonce: [u8; VALUE_LEN],
}

impl ProofTranscript {
    /// Builds a transcript for a canonical [`Audience`]. Both sides build it this way: the
    /// agent from its registration record and the server from its own pinned origin, so the
    /// signed text is the same bytes and nothing can sign an audience that is not canonical
    /// (design 14.4, ADR-466). `agent_id` is the server-assigned agent ID.
    pub fn new(
        audience: &Audience,
        agent_id: &str,
        challenge: [u8; VALUE_LEN],
        nonce: [u8; VALUE_LEN],
    ) -> Result<Self, TranscriptError> {
        if agent_id.is_empty() || agent_id.len() > MAX_FIELD_LEN {
            return Err(TranscriptError::BadField { field: "agent_id" });
        }
        Ok(Self {
            audience: audience.as_str().to_owned(),
            agent_id: agent_id.to_owned(),
            challenge,
            nonce,
        })
    }

    /// The body that is signed under [`DOMAIN_TAG`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            4 + DOMAIN_TAG.len()
                + 4
                + 8
                + self.audience.len()
                + self.agent_id.len()
                + 2 * VALUE_LEN,
        );
        out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        put_prefixed(&mut out, self.audience.as_bytes());
        put_prefixed(&mut out, self.agent_id.as_bytes());
        out.extend_from_slice(&self.challenge);
        out.extend_from_slice(&self.nonce);
        out
    }
}

fn put_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("fields are length-checked");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Signs `transcript` with the agent's key.
pub fn sign_proof<S: Signer + ?Sized>(
    signer: &S,
    transcript: &ProofTranscript,
) -> Result<Signature, SignError> {
    sign_tagged(signer, DOMAIN_TAG, &transcript.encode())
}

/// Verifies a connection proof against the agent's registered public key.
pub fn verify_proof(
    key: &PublicKey,
    transcript: &ProofTranscript,
    signature: &Signature,
) -> Result<(), VerifyError> {
    verify_tagged(key, DOMAIN_TAG, &transcript.encode(), signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SignErrorKind, SigningKey, instruction};

    const AUDIENCE: &str = "https://ngr.example.com";
    const AGENT: &str = "3f1c2a52-6a0e-4c0d-9d3e-0a1b2c3d4e5f";

    /// A fresh 32-byte value for a challenge or nonce. The tests make their values at run time
    /// rather than writing fixed ones.
    fn fresh() -> [u8; 32] {
        crate::random_32().unwrap()
    }

    /// A challenge and a nonce that differ from each other.
    fn pair() -> ([u8; 32], [u8; 32]) {
        let challenge = fresh();
        loop {
            let nonce = fresh();
            if nonce != challenge {
                return (challenge, nonce);
            }
        }
    }

    fn transcript(challenge: [u8; 32], nonce: [u8; 32]) -> ProofTranscript {
        ProofTranscript::new(&aud(AUDIENCE), AGENT, challenge, nonce).unwrap()
    }

    fn aud(text: &str) -> Audience {
        Audience::canonicalise(text).unwrap()
    }

    #[test]
    fn a_proof_verifies() {
        let (c, n) = pair();
        let key = SigningKey::generate().unwrap();
        let sig = sign_proof(&key, &transcript(c, n)).unwrap();
        verify_proof(&key.public_key(), &transcript(c, n), &sig).unwrap();
    }

    #[test]
    fn encoding_is_the_documented_layout() {
        let (c, n) = pair();
        let t = ProofTranscript::new(&aud("http://ab"), "c", c, n).unwrap();
        let mut want = Vec::new();
        want.extend(PROTOCOL_VERSION.to_be_bytes());
        want.extend(9u32.to_be_bytes());
        want.extend(b"http://ab");
        want.extend(1u32.to_be_bytes());
        want.extend(b"c");
        want.extend(c);
        want.extend(n);
        assert_eq!(t.encode(), want);
    }

    #[test]
    fn every_changed_field_fails() {
        let (c, n) = pair();
        let key = SigningKey::generate().unwrap();
        let sig = sign_proof(&key, &transcript(c, n)).unwrap();
        let pk = key.public_key();
        let mut other_c = c;
        other_c[0] ^= 1;
        let mut other_n = n;
        other_n[31] ^= 1;
        let cases = [
            ProofTranscript::new(&aud("https://ngr.example.org"), AGENT, c, n).unwrap(),
            ProofTranscript::new(&aud(AUDIENCE), "other", c, n).unwrap(),
            transcript(other_c, n),
            transcript(c, other_n),
            // Challenge and nonce swapped.
            transcript(n, c),
        ];
        for t in cases {
            assert_eq!(
                verify_proof(&pk, &t, &sig),
                Err(VerifyError::Mismatch),
                "{t:?}"
            );
        }
    }

    #[test]
    fn a_changed_version_fails() {
        let (c, n) = pair();
        // The version is internal, so build the bytes of another version by hand.
        let key = SigningKey::generate().unwrap();
        let mut body = transcript(c, n).encode();
        body[..4].copy_from_slice(&(PROTOCOL_VERSION + 1).to_be_bytes());
        let sig = sign_tagged(&key, DOMAIN_TAG, &body).unwrap();
        assert!(verify_proof(&key.public_key(), &transcript(c, n), &sig).is_err());
    }

    #[test]
    fn field_boundaries_are_unambiguous() {
        // Moving a byte between audience and agent ID must change the encoding.
        let (c, n) = pair();
        let a = ProofTranscript::new(&aud("http://ab"), "c", c, n).unwrap();
        let b = ProofTranscript::new(&aud("http://a"), "bc", c, n).unwrap();
        assert_ne!(a.encode(), b.encode());
    }

    #[test]
    fn another_key_or_signature_form_fails() {
        let (c, n) = pair();
        let key = SigningKey::generate().unwrap();
        let sig = sign_proof(&key, &transcript(c, n)).unwrap();
        let other = SigningKey::generate().unwrap().public_key();
        assert_eq!(
            verify_proof(&other, &transcript(c, n), &sig),
            Err(VerifyError::Mismatch)
        );
        let bytes = sig.to_bytes();
        assert!(Signature::from_bytes(&bytes[..63]).is_err());
        let mut long = bytes.to_vec();
        long.push(0);
        assert!(Signature::from_bytes(&long).is_err());
    }

    #[test]
    fn a_signature_without_the_tag_is_not_a_proof() {
        let (c, n) = pair();
        // The same body signed untagged (the raw primitive) does not verify as a proof.
        let key = SigningKey::generate().unwrap();
        let t = transcript(c, n);
        let raw = key.sign(&t.encode()).unwrap();
        assert!(verify_proof(&key.public_key(), &t, &raw).is_err());
    }

    #[test]
    fn a_signature_under_another_tag_is_not_a_proof() {
        let (c, n) = pair();
        // What a job instruction signer would do: the same body under its own tag.
        let key = SigningKey::generate().unwrap();
        let t = transcript(c, n);
        let other = sign_tagged(&key, instruction::DOMAIN_TAG, &t.encode()).unwrap();
        assert!(verify_proof(&key.public_key(), &t, &other).is_err());
        verify_tagged(
            &key.public_key(),
            instruction::DOMAIN_TAG,
            &t.encode(),
            &other,
        )
        .unwrap();
        // And a proof does not verify under the other tag.
        let proof = sign_proof(&key, &t).unwrap();
        assert!(
            verify_tagged(
                &key.public_key(),
                instruction::DOMAIN_TAG,
                &t.encode(),
                &proof
            )
            .is_err()
        );
    }

    #[test]
    fn empty_or_oversized_fields_are_refused() {
        let (c, n) = pair();
        let bad = |g: &str| ProofTranscript::new(&aud(AUDIENCE), g, c, n);
        assert_eq!(
            bad(""),
            Err(TranscriptError::BadField { field: "agent_id" })
        );
        let long = "x".repeat(MAX_FIELD_LEN + 1);
        assert!(bad(&long).is_err());
        assert!(bad(&"x".repeat(MAX_FIELD_LEN)).is_ok());
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
        let err = sign_proof(
            &Broken(SigningKey::generate().unwrap()),
            &transcript(fresh(), fresh()),
        )
        .unwrap_err();
        assert_eq!(err.kind(), SignErrorKind::Unavailable);
        assert!(!err.to_string().contains("collection"));
        assert!(
            std::error::Error::source(&err)
                .unwrap()
                .to_string()
                .contains("collection")
        );
    }

    fn canon(s: &str) -> Result<String, AudienceError> {
        Audience::canonicalise(s).map(|a| a.as_str().to_owned())
    }

    #[test]
    fn audience_canonical_forms_are_byte_exact() {
        for (input, want) in [
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080"),
            ("http://127.0.0.1:8080/", "http://127.0.0.1:8080"),
            ("http://127.0.0.1:80", "http://127.0.0.1"),
            ("HTTP://127.0.0.1:80/", "http://127.0.0.1"),
            ("https://NGR.Example.COM:443", "https://ngr.example.com"),
            (
                "https://ngr.example.com:8443",
                "https://ngr.example.com:8443",
            ),
            ("http://[::1]:80", "http://[::1]"),
            ("http://[0:0::1]", "http://[::1]"),
            (
                "http://[2001:DB8:0:0:0:0:0:1]:8080",
                "http://[2001:db8::1]:8080",
            ),
            ("http://[::1]:9000/", "http://[::1]:9000"),
            // 443 is not http's default, nor 80 https's.
            ("http://h.example:443", "http://h.example:443"),
            ("https://h.example:80", "https://h.example:80"),
        ] {
            assert_eq!(canon(input).as_deref(), Ok(want), "{input}");
        }
    }

    #[test]
    fn audience_refusals() {
        use AudienceError::*;
        for (input, want) in [
            ("", BadCharacter),
            ("http://127.0.0.1 ", BadCharacter),
            ("http://127.0.0.1\n", BadCharacter),
            ("http://h\u{e9}.example", BadCharacter),
            ("127.0.0.1:8080", BadScheme),
            ("ftp://127.0.0.1", BadScheme),
            ("ws://127.0.0.1", BadScheme),
            ("//127.0.0.1", BadScheme),
            ("http://user@127.0.0.1", Userinfo),
            ("http://user:pw@127.0.0.1", Userinfo),
            ("http://127.0.0.1/api", NotAnOrigin),
            ("http://127.0.0.1//", NotAnOrigin),
            ("http://127.0.0.1?x=1", NotAnOrigin),
            ("http://127.0.0.1#f", NotAnOrigin),
            ("http://localhost:8080", Localhost),
            ("http://LocalHost", Localhost),
            ("http://localhost.:8080", Localhost),
            ("http://app.localhost", Localhost),
            ("http://", BadHost),
            ("http://:8080", BadHost),
            ("http://-a.example", BadHost),
            ("http://a..example", BadHost),
            ("https://ngr.example.com.", BadHost),
            ("http://a_b.example", BadHost),
            ("http://127.000.0.1", BadHost),
            ("http://0x7f.0.0.1", BadHost),
            ("http://127.1", BadHost),
            ("http://1.2.3.256", BadHost),
            ("http://1.2.3.4.5", BadHost),
            ("http://a.1", BadHost),
            ("http://[fe80::1%25eth0]", BadHost),
            ("http://[]", BadHost),
            ("http://[zz]", BadHost),
            ("http://[::1", BadHost),
            ("http://[::1]x", BadHost),
            ("http://127.0.0.1:", BadPort),
            ("http://127.0.0.1:0", BadPort),
            ("http://127.0.0.1:080", BadPort),
            ("http://127.0.0.1:65536", BadPort),
            ("http://127.0.0.1:80x", BadPort),
            ("http://127.0.0.1:+80", BadPort),
        ] {
            assert_eq!(canon(input), Err(want), "{input:?}");
        }
        let long = format!("https://{}.example", "a".repeat(MAX_FIELD_LEN));
        assert_eq!(canon(&long), Err(BadCharacter));
    }

    #[test]
    fn strict_parsing_accepts_only_the_canonical_text() {
        for ok in [
            "http://127.0.0.1:8080",
            "http://127.0.0.1",
            "https://ngr.example.com",
        ] {
            assert_eq!(Audience::parse_canonical(ok).unwrap().as_str(), ok);
        }
        for changed in [
            "http://127.0.0.1:8080/",
            "http://127.0.0.1:80",
            "HTTP://127.0.0.1",
            "https://Ngr.example.com",
        ] {
            assert_eq!(
                Audience::parse_canonical(changed),
                Err(AudienceError::NotCanonical),
                "{changed}"
            );
        }
        assert_eq!(
            Audience::parse_canonical("http://localhost"),
            Err(AudienceError::Localhost)
        );
    }

    #[test]
    fn an_audience_signs_as_its_canonical_bytes() {
        let (c, n) = pair();
        let a = Audience::canonicalise("http://127.0.0.1:80/").unwrap();
        let t = ProofTranscript::new(&a, AGENT, c, n).unwrap();
        let mut want = Vec::new();
        want.extend(PROTOCOL_VERSION.to_be_bytes());
        want.extend(16u32.to_be_bytes());
        want.extend(b"http://127.0.0.1");
        want.extend((AGENT.len() as u32).to_be_bytes());
        want.extend(AGENT.as_bytes());
        want.extend(c);
        want.extend(n);
        assert_eq!(t.encode(), want);
        // The record-write and the verifier sides meet on the same bytes.
        let other = Audience::canonicalise("http://127.0.0.1").unwrap();
        assert_eq!(t, ProofTranscript::new(&other, AGENT, c, n).unwrap());
    }
}
