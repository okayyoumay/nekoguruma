//! The two values an operator gives `ngr-agent register` that name the server: `--server`, the
//! origin the agent connects to, and `--server-key`, the server key ID (design 6.4, 13.3,
//! ADR-470).
//!
//! The local profile pins `http://127.0.0.1:<port>` (ADR-456), and `https` waits for the standard
//! profile's TLS choice (ADR-463 Decision item 13), so the agent accepts exactly that form. The
//! text goes through [`Audience::canonicalise`] once, and the canonical text must then be the
//! IP literal, with or without a port, compared for equality and never by prefix. The canonical
//! text is the audience the connection proof signs later (ADR-466 Decision item 4), so the
//! record stores it as it is. No error repeats the value it refused.

use shared_crypto::proof::{Audience, AudienceError};

/// The canonical text of the origin without a port.
const BASE: &str = "http://127.0.0.1";

/// Why `--server` is refused. The text names the rule, never the value.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OriginError {
    #[error("--server: https is not supported yet")]
    Https,
    #[error("--server: {0}")]
    Audience(AudienceError),
    #[error("--server must be http://127.0.0.1, with an optional port")]
    NotLocal,
}

/// The server's origin in canonical form, and the port the agent connects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerOrigin {
    audience: Audience,
    port: u16,
}

impl ServerOrigin {
    /// Reads `--server`.
    pub fn parse(text: &str) -> Result<Self, OriginError> {
        if text
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
        {
            return Err(OriginError::Https);
        }
        let audience = Audience::canonicalise(text).map_err(OriginError::Audience)?;
        let port = match audience.as_str().strip_prefix(BASE) {
            Some("") => 80,
            Some(rest) => rest
                .strip_prefix(':')
                .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|digits| digits.parse::<u16>().ok())
                .ok_or(OriginError::NotLocal)?,
            None => return Err(OriginError::NotLocal),
        };
        Ok(Self { audience, port })
    }

    /// The canonical origin: what the record stores and the connection proof signs.
    pub fn as_str(&self) -> &str {
        self.audience.as_str()
    }

    /// The port the agent connects to (80 when the origin has none).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The `Host` header's value: the port is left out when it is 80.
    pub fn host_header(&self) -> String {
        if self.port == 80 {
            "127.0.0.1".to_owned()
        } else {
            format!("127.0.0.1:{}", self.port)
        }
    }
}

/// Reads `--server-key`: 64 hexadecimal digits in either case, as 32 bytes.
pub fn parse_server_key(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        let digits = std::str::from_utf8(pair).ok()?;
        *byte = u8::from_str_radix(digits, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted(text: &str) -> ServerOrigin {
        ServerOrigin::parse(text).unwrap_or_else(|error| panic!("{text:?}: {error}"))
    }

    fn refused(text: &str) -> OriginError {
        ServerOrigin::parse(text).expect_err(text)
    }

    /// S1 to S3.
    #[test]
    fn the_pinned_origin_is_accepted_in_every_spelling() {
        let origin = accepted("http://127.0.0.1:8080");
        assert_eq!(origin.as_str(), "http://127.0.0.1:8080");
        assert_eq!(origin.port(), 8080);
        assert_eq!(origin.host_header(), "127.0.0.1:8080");
        for same in [
            "http://127.0.0.1:8080/",
            "HTTP://127.0.0.1:8080",
            "Http://127.0.0.1:8080/",
        ] {
            assert_eq!(accepted(same), origin, "{same}");
        }
        for default in [
            "http://127.0.0.1:80",
            "http://127.0.0.1",
            "http://127.0.0.1/",
        ] {
            let origin = accepted(default);
            assert_eq!(origin.as_str(), "http://127.0.0.1", "{default}");
            assert_eq!(origin.port(), 80);
            assert_eq!(origin.host_header(), "127.0.0.1");
        }
    }

    /// S4.
    #[test]
    fn localhost_is_refused() {
        for text in [
            "http://localhost:8080",
            "http://LocalHost:8080",
            "http://app.localhost:8080",
            "http://localhost",
        ] {
            assert_eq!(
                refused(text),
                OriginError::Audience(AudienceError::Localhost),
                "{text}"
            );
        }
    }

    /// S5.
    #[test]
    fn https_is_refused_whatever_the_host() {
        for text in [
            "https://127.0.0.1:8443",
            "HTTPS://127.0.0.1",
            "https://localhost",
            "https://example.com",
            "https://[::1]:1",
        ] {
            assert_eq!(refused(text), OriginError::Https, "{text}");
        }
    }

    /// S6.
    #[test]
    fn a_path_a_query_a_fragment_and_userinfo_are_refused() {
        for text in [
            "http://127.0.0.1:8080/api",
            "http://127.0.0.1:8080//",
            "http://127.0.0.1:8080?a=b",
            "http://127.0.0.1:8080/?a=b",
            "http://127.0.0.1:8080#f",
            "http://user@127.0.0.1:8080",
            "http://user:pw@127.0.0.1:8080",
        ] {
            assert!(
                matches!(
                    refused(text),
                    OriginError::Audience(AudienceError::NotAnOrigin | AudienceError::Userinfo)
                ),
                "{text}"
            );
        }
    }

    /// S7.
    #[test]
    fn only_the_loopback_literal_is_accepted() {
        for text in [
            "http://[::1]:8080",
            "http://127.0.0.2:8080",
            "http://192.168.1.10:8080",
            "http://10.0.0.1",
            "http://example.com:8080",
            "http://127.0.0.1.example.com:8080",
        ] {
            assert_eq!(refused(text), OriginError::NotLocal, "{text}");
        }
    }

    /// S8.
    #[test]
    fn number_like_hosts_are_refused() {
        for text in [
            "http://127.1",
            "http://0x7f000001",
            "http://127.000.0.1",
            "http://127.0.0.1.",
            "http://2130706433",
        ] {
            assert!(
                matches!(refused(text), OriginError::Audience(AudienceError::BadHost)),
                "{text}"
            );
        }
    }

    /// S9.
    #[test]
    fn a_bad_port_scheme_or_character_is_refused() {
        for text in [
            "http://127.0.0.1:0",
            "http://127.0.0.1:080",
            "http://127.0.0.1:65536",
            "http://127.0.0.1:",
            "http://127.0.0.1:+80",
            "127.0.0.1:8080",
            "//127.0.0.1:8080",
            "ftp://127.0.0.1:8080",
            "http:// 127.0.0.1:8080",
            "http://127.0.0.1:8080 ",
            " http://127.0.0.1:8080",
            "http://127.0.0.1:80\n",
            "http://127.0.0.1:8080\u{e9}",
            "http://１２７.0.0.1:8080",
            "",
        ] {
            assert!(
                matches!(refused(text), OriginError::Audience(_)),
                "{text:?}: {:?}",
                refused(text)
            );
        }
    }

    #[test]
    fn a_refusal_never_repeats_the_value() {
        let secret = "http://secret-host.example:1234/secret";
        let text = refused(secret).to_string();
        assert!(!text.contains("secret"), "{text}");
    }

    /// K1 and K2.
    #[test]
    fn the_server_key_is_64_hex_digits_in_either_case() {
        let lower = "0123456789abcdef".repeat(4);
        let upper = lower.to_ascii_uppercase();
        let bytes = parse_server_key(&lower).unwrap();
        assert_eq!(parse_server_key(&upper), Some(bytes));
        assert_eq!(bytes[0], 0x01);
        assert_eq!(bytes[31], 0xef);
        for bad in [
            "",
            &lower[..63],
            &format!("{lower}0"),
            &format!("{}g", &lower[..63]),
            &format!(" {}", &lower[..63]),
            &format!("{}\n", &lower[..63]),
            &"é".repeat(32),
        ] {
            assert_eq!(parse_server_key(bad), None, "{bad:?}");
        }
    }
}
