use sha2::{Digest, Sha256};

/// The SHA-256 of a certificate's DER encoding.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CertPin([u8; 32]);

/// The text is not a pin: a pin is 64 lowercase hexadecimal digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinError;

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a certificate pin is 64 lowercase hexadecimal digits")
    }
}

impl std::error::Error for PinError {}

impl CertPin {
    /// The pin of the certificate whose DER encoding is `der`.
    pub fn of_certificate(der: &[u8]) -> Self {
        Self(Sha256::digest(der).into())
    }

    /// The pin written as exactly 64 lowercase hexadecimal digits, as [`Display`] writes it.
    ///
    /// [`Display`]: std::fmt::Display
    pub fn from_hex(text: &str) -> Result<Self, PinError> {
        let digits = text.as_bytes();
        if digits.len() != 64 {
            return Err(PinError);
        }
        let mut bytes = [0u8; 32];
        let (pairs, _) = digits.as_chunks::<2>();
        for (byte, pair) in bytes.iter_mut().zip(pairs) {
            *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Ok(Self(bytes))
    }
}

fn nibble(digit: u8) -> Result<u8, PinError> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        _ => Err(PinError),
    }
}

impl std::fmt::Display for CertPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl std::fmt::Debug for CertPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pin_of_a_certificate_is_its_sha256_in_lowercase_hex() {
        // The digest of the empty input, a published test vector.
        let pin = CertPin::of_certificate(b"");
        assert_eq!(
            pin.to_string(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(format!("{pin:?}"), pin.to_string());
    }

    #[test]
    fn text_written_by_display_reads_back() {
        let pin = CertPin::of_certificate(b"a certificate");
        assert_eq!(CertPin::from_hex(&pin.to_string()), Ok(pin));
    }

    #[test]
    fn text_that_is_not_64_lowercase_hex_digits_is_refused() {
        let good = CertPin::of_certificate(b"x").to_string();
        let upper = good.to_ascii_uppercase();
        assert_ne!(upper, good, "the sample has a letter to uppercase");
        let wide = format!("{}é", &good[..62]);
        assert_eq!(wide.len(), 64, "64 bytes, 63 characters");
        let cases = [
            ("empty", String::new()),
            ("63 digits", good[..63].to_owned()),
            ("65 digits", format!("{good}0")),
            ("uppercase", upper),
            ("not hex", format!("{}g", &good[..63])),
            ("a space", format!(" {}", &good[..63])),
            ("trailing newline", format!("{}\n", &good[..63])),
            ("a prefix", format!("0x{}", &good[..62])),
            ("non-ASCII", wide),
        ];
        for (name, text) in cases {
            assert_eq!(CertPin::from_hex(&text), Err(PinError), "{name}");
        }
    }
}
