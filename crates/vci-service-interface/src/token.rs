//! Bearer-token format of the worker gRPC listener (ADR-221). The agent
//! (`worker-host`) mints tokens from the per-instance key it provisions over
//! the stdio control channel; `vci-service-launcher`'s shared listener
//! verifies them with the same key. Both sides must agree byte-for-byte on
//! this format, so it lives in exactly one place: this crate, which both the
//! client and the service already depend on for the gRPC interface.
//!
//! Token shape: `vci1.<base64url(payload_json)>.<base64url(HMAC-SHA256(key, payload_json))>`
//! where `payload_json` is `{"exp": <unix_secs>, "iat": <unix_secs>, "sub": "<opaque label>"}`.
//! The MAC is computed over the raw, undecoded base64url payload segment
//! bytes (not the decoded JSON) so verification never depends on a
//! decode-then-reencode round trip being byte-stable.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Token lifetime from mint time, in seconds (10 minutes).
pub const TOKEN_TTL_SECS: u64 = 600;

const TOKEN_VERSION: &str = "vci1";

#[derive(Debug, Serialize, Deserialize)]
struct Payload {
    exp: u64,
    iat: u64,
    sub: String,
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mints a bearer token for `sub`, signed with `key`, valid from `now` for
/// [`TOKEN_TTL_SECS`] seconds. Returns the token together with the exact
/// whole-second `exp` (Unix seconds) embedded in it -- callers that need to
/// advertise an expiry to a client (e.g. as milliseconds) MUST derive it
/// from this returned value rather than independently recomputing
/// `now + TOKEN_TTL_SECS` at their own precision: `now` is floored to whole
/// seconds internally (`unix_secs`), so a caller using sub-second precision
/// would advertise an expiry up to 999ms later than what `verify` actually
/// honors, letting a client be surprised by an early `UNAUTHENTICATED` near
/// the advertised deadline.
pub fn mint(key: &[u8; 32], sub: &str, now: SystemTime) -> (String, u64) {
    let iat = unix_secs(now);
    let exp = iat.saturating_add(TOKEN_TTL_SECS);
    let payload = Payload {
        exp,
        iat,
        sub: sub.to_string(),
    };
    // `Payload` is a fixed, all-primitive struct we control; serialization
    // cannot fail.
    let payload_json =
        serde_json::to_vec(&payload).expect("token payload always serializes to JSON");
    let payload_b64 = URL_SAFE_NO_PAD.encode(&payload_json);

    // The key is always exactly 32 bytes, which HMAC-SHA256 always accepts.
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(payload_b64.as_bytes());
    let mac_b64 = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());

    (format!("{TOKEN_VERSION}.{payload_b64}.{mac_b64}"), exp)
}

/// Why a token was refused. The texts are short and generic; the cause a
/// library reported is the error's source.
///
/// Callers that might expose this to an untrusted client (e.g. a gRPC
/// interceptor) should map every variant to the same generic
/// `UNAUTHENTICATED` message rather than surfacing it, since the variants
/// are diagnostic detail, not something a caller should rely on
/// distinguishing.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// The token is not three dot-separated segments starting with the
    /// version.
    #[error("malformed token")]
    Malformed,
    /// The payload or MAC segment is not base64url; the source is the
    /// decoder's error.
    #[error("invalid base64")]
    InvalidBase64(#[source] base64::DecodeError),
    /// The key was refused by the MAC; the source is the MAC's error.
    #[error("invalid key")]
    InvalidKey(#[source] hmac::digest::InvalidLength),
    /// The MAC does not match; the source is the MAC's error.
    #[error("invalid mac")]
    InvalidMac(#[source] hmac::digest::MacError),
    /// The payload is not the expected JSON; the source is the parser's
    /// error.
    #[error("invalid payload")]
    InvalidPayload(#[source] serde_json::Error),
    /// The token's expiry is not after the verification time.
    #[error("expired")]
    Expired,
}

/// Verifies `token` against `key` as of `now`. Fails with the [`TokenError`]
/// naming the first check that did not hold.
pub fn verify(key: &[u8; 32], token: &str, now: SystemTime) -> Result<(), TokenError> {
    let mut parts = token.split('.');
    let version = parts.next().ok_or(TokenError::Malformed)?;
    let payload_b64 = parts.next().ok_or(TokenError::Malformed)?;
    let mac_b64 = parts.next().ok_or(TokenError::Malformed)?;
    if parts.next().is_some() {
        return Err(TokenError::Malformed);
    }
    if version != TOKEN_VERSION {
        return Err(TokenError::Malformed);
    }

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(TokenError::InvalidBase64)?;
    let mac_bytes = URL_SAFE_NO_PAD
        .decode(mac_b64)
        .map_err(TokenError::InvalidBase64)?;

    let mut mac = HmacSha256::new_from_slice(key).map_err(TokenError::InvalidKey)?;
    // Recompute over the raw, undecoded payload segment bytes -- see module
    // doc comment for why.
    mac.update(payload_b64.as_bytes());
    // `verify_slice` is `hmac`'s constant-time comparison; never use `==` on
    // raw MAC bytes.
    mac.verify_slice(&mac_bytes)
        .map_err(TokenError::InvalidMac)?;

    let payload: Payload =
        serde_json::from_slice(&payload_bytes).map_err(TokenError::InvalidPayload)?;

    if payload.exp <= unix_secs(now) {
        return Err(TokenError::Expired);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const KEY: [u8; 32] = [7u8; 32];
    const OTHER_KEY: [u8; 32] = [9u8; 32];

    #[test]
    fn mint_then_verify_round_trips() {
        let now = SystemTime::now();
        let (token, _exp) = mint(&KEY, "test-client", now);
        verify(&KEY, &token, now).unwrap();
    }

    #[test]
    fn tampered_payload_fails() {
        let now = SystemTime::now();
        let (token, _exp) = mint(&KEY, "test-client", now);
        let mut parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let mut payload_b64 = parts[1].to_string();
        // Flip one character in the base64 payload segment.
        let flipped_char = if payload_b64.as_bytes()[0] == b'a' {
            'b'
        } else {
            'a'
        };
        payload_b64.replace_range(0..1, &flipped_char.to_string());
        parts[1] = &payload_b64;
        let tampered = parts.join(".");

        let err = verify(&KEY, &tampered, now).expect_err("tampered payload should fail");
        assert!(matches!(err, TokenError::InvalidMac(_)), "{err:?}");
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn expired_token_fails() {
        // Mint far in the past relative to `now` passed to `verify`.
        let long_ago = SystemTime::now() - Duration::from_secs(TOKEN_TTL_SECS * 10);
        let (token, _exp) = mint(&KEY, "test-client", long_ago);
        let err = verify(&KEY, &token, SystemTime::now()).expect_err("expired token should fail");
        assert!(matches!(err, TokenError::Expired), "{err:?}");
    }

    #[test]
    fn token_minted_under_one_key_fails_under_another() {
        let now = SystemTime::now();
        let (token, _exp) = mint(&KEY, "test-client", now);
        assert!(verify(&OTHER_KEY, &token, now).is_err());
    }

    /// A caller must be able to derive an accurate advertised expiry from
    /// `mint`'s returned `exp` alone -- verification at exactly that instant
    /// must still succeed (not reject early because `mint` internally
    /// floored `now` to whole seconds while the caller advertised a
    /// sub-second-precision deadline of its own).
    #[test]
    fn returned_exp_matches_what_verify_actually_honors() {
        let now = SystemTime::now();
        let (token, exp) = mint(&KEY, "test-client", now);
        let just_before_exp = UNIX_EPOCH + Duration::from_secs(exp - 1);
        assert!(
            verify(&KEY, &token, just_before_exp).is_ok(),
            "token should still verify one second before its own returned exp"
        );
        let at_exp = UNIX_EPOCH + Duration::from_secs(exp);
        assert!(
            verify(&KEY, &token, at_exp).is_err(),
            "token should be expired exactly at its own returned exp"
        );
    }

    #[test]
    fn malformed_strings_fail_without_panicking() {
        let now = SystemTime::now();
        for bad in [
            "",
            "not-a-token",
            "vci1.onlyonepart",
            "vci1.a.b.c",
            "vci1..",
            "wrongversion.a.b",
            "vci1.not!base64!.alsonot!base64!",
        ] {
            assert!(verify(&KEY, bad, now).is_err(), "expected {bad:?} to fail");
        }
    }
}
