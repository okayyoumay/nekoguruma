//! The host rule a part URL must meet before the agent sends a byte to it (ADR-493 item 13).
//!
//! The server answers a presigned URL, and a presigned URL is a bearer for its object, so the
//! agent must not be led anywhere else by one. The rule is a byte-prefix check, not a URL
//! parser: the text must begin with the origin's scheme and host exactly as the record stores
//! them (so `127.0.0.10`, `127.0.0.1.example`, another case and any userinfo do not match), and
//! then either a `/`, which means port 80, or a `:`, a canonical port from 1 to 65535 and a `/`.
//! Everything from that `/` on is the request target and goes out unchanged, since the signature
//! covers it; `Host` is the authority as the URL gives it.
//!
//! There is no redirect handling and no name resolution behind the rule: the dial is a loopback
//! literal (`control::dial`), so even a URL that passed could not lead off the machine.

use crate::registration::origin::ServerOrigin;

/// The longest URL accepted, in bytes. A presigned URL is a few hundred.
pub const MAX_URL_LEN: usize = 4096;

/// A part URL that met the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartTarget {
    /// The port the PUT connects to.
    pub port: u16,
    /// The `Host` header's value: the URL's authority as given.
    pub host: String,
    /// The request target: from the first `/` after the authority, unchanged.
    pub target: String,
}

/// Why a URL was refused. The text of the URL is never repeated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("the part URL is over the length limit")]
    TooLong,
    #[error("the part URL has a byte that is not visible ASCII, or a fragment")]
    Bytes,
    #[error("the part URL does not begin with the registered scheme and host")]
    Origin,
    #[error("the part URL's port is not a canonical number from 1 to 65535")]
    Port,
    #[error("the part URL has no path after its authority")]
    NoPath,
}

/// Judges `url` against `origin`.
pub fn check(origin: &ServerOrigin, url: &str) -> Result<PartTarget, Refused> {
    if url.len() > MAX_URL_LEN {
        return Err(Refused::TooLong);
    }
    if !url.bytes().all(|b| b.is_ascii_graphic()) || url.contains('#') {
        return Err(Refused::Bytes);
    }
    let base = origin.scheme_host();
    let rest = url.strip_prefix(base).ok_or(Refused::Origin)?;
    let (port, authority_len) = match rest.as_bytes().first() {
        Some(b'/') => (80, 0),
        Some(b':') => {
            let digits = &rest[1..];
            let end = digits.find('/').ok_or(Refused::NoPath)?;
            (port(&digits[..end])?, 1 + end)
        }
        // The host continues (`127.0.0.10`, `127.0.0.1.x`, `127.0.0.1@x`) or the URL ends.
        _ => return Err(Refused::Origin),
    };
    // The authority starts after `scheme://`, which the origin's text always has.
    let authority_from = base.find("://").map_or(0, |at| at + 3);
    Ok(PartTarget {
        port,
        host: url[authority_from..base.len() + authority_len].to_owned(),
        target: rest[authority_len..].to_owned(),
    })
}

/// A port in canonical decimal: no sign, no leading zero, from 1 to 65535.
fn port(digits: &str) -> Result<u16, Refused> {
    if digits.is_empty()
        || digits.len() > 5
        || digits.starts_with('0')
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(Refused::Port);
    }
    digits.parse::<u16>().map_err(|_| Refused::Port)
}

#[cfg(test)]
mod tests;
