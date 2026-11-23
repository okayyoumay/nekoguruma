//! The rule an object URL (a part URL or a download URL) must meet before the agent sends a byte
//! to it (ADR-493 item 13, ADR-505 item 4).
//!
//! The server answers a presigned URL, and a presigned URL is a bearer for its object, so the
//! agent must not be led anywhere else by one. The rule is a byte-prefix check, not a URL
//! parser: the text must begin with `https://` in lowercase and the origin's host exactly as the
//! record stores it (so `http://`, `HTTPS://`, `127.0.0.10`, `127.0.0.1.example`, another case and
//! any userinfo do not match), and then either a `/`, which means port 443, or a `:`, a canonical
//! port from 1 to 65535 and a `/`. Everything from that `/` on is the request target and goes
//! out unchanged, since the signature covers it; `Host` is the authority as the URL gives it.
//!
//! The target also holds the pin of the certificate the object server presents, which the same
//! proved answer carries: the connection is TLS with that certificate or none.
//!
//! There is no redirect handling and no name resolution behind the rule: the dial is a loopback
//! literal (`control::dial`), so even a URL that passed could not lead off the machine.

use ngr_pinned_tls::CertPin;

use crate::registration::origin::ServerOrigin;

/// The scheme every object URL has, in lowercase.
const SCHEME: &str = "https://";

/// The longest URL accepted, in bytes. A presigned URL is a few hundred.
pub const MAX_URL_LEN: usize = 4096;

/// An object URL that met the rule, with the pin of the certificate its server must present.
/// Only [`check`] makes one. The target carries the URL's signature, so `Debug` shows the port
/// and the lengths only.
#[derive(Clone, PartialEq, Eq)]
pub struct PartTarget {
    port: u16,
    host: String,
    target: String,
    pin: CertPin,
}

impl PartTarget {
    /// The port the request connects to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The pin of the certificate the server at the URL must present.
    pub fn pin(&self) -> &CertPin {
        &self.pin
    }

    /// The `Host` header's value: the URL's authority as given.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The request target: from the first `/` after the authority, unchanged.
    pub fn target(&self) -> &str {
        &self.target
    }
}

impl std::fmt::Debug for PartTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartTarget")
            .field("port", &self.port)
            .field("target_len", &self.target.len())
            .finish()
    }
}

/// Why a URL was refused. The text of the URL is never repeated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("the part URL is over the length limit")]
    TooLong,
    #[error("the part URL has a byte that is not visible ASCII, or a fragment")]
    Bytes,
    #[error("the part URL does not begin with https:// and the registered host")]
    Origin,
    #[error("the part URL's port is not a canonical number from 1 to 65535")]
    Port,
    #[error("the part URL has no path after its authority")]
    NoPath,
}

/// Judges `url` against `origin`; `pin` is the pin its answer carried.
pub fn check(origin: &ServerOrigin, url: &str, pin: CertPin) -> Result<PartTarget, Refused> {
    if url.len() > MAX_URL_LEN {
        return Err(Refused::TooLong);
    }
    if !url.bytes().all(|b| b.is_ascii_graphic()) || url.contains('#') {
        return Err(Refused::Bytes);
    }
    let rest = url.strip_prefix(SCHEME).ok_or(Refused::Origin)?;
    let host = origin.host();
    let rest = rest.strip_prefix(host).ok_or(Refused::Origin)?;
    let (port, authority_len) = match rest.as_bytes().first() {
        Some(b'/') => (443, 0),
        Some(b':') => {
            let digits = &rest[1..];
            let end = digits.find('/').ok_or(Refused::NoPath)?;
            (port(&digits[..end])?, 1 + end)
        }
        // The host continues (`127.0.0.10`, `127.0.0.1.x`, `127.0.0.1@x`) or the URL ends.
        _ => return Err(Refused::Origin),
    };
    Ok(PartTarget {
        port,
        host: url[SCHEME.len()..SCHEME.len() + host.len() + authority_len].to_owned(),
        target: rest[authority_len..].to_owned(),
        pin,
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
