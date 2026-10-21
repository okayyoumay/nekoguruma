//! The flash artifact a job reads its blocks from (design 8.2.5, 11.3; ADR-336 items 1 and 12).
//!
//! The IR names no file: a flash segment of the variant's document is a range of the artifact
//! (`artifact_offset`, `length`), and `Op::FlashBlock` reads one block of it through
//! `DiagHost::artifact_bytes`. The job names the artifact by its SHA-256, and [`Artifact`] is
//! what the agent holds: the whole file's bytes and their hash.

use std::io;
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

/// The length of a SHA-256 hash in bytes.
pub const SHA256_LEN: usize = 32;

/// A flash artifact: the bytes of a file, read once, and their SHA-256.
///
/// The bytes stay in memory for the life of the job. A restart within the job therefore reads
/// exactly the bytes that were hashed, and a file that changes on disk after the check cannot
/// change a block that is sent. Cloning shares the bytes.
#[derive(Clone)]
pub struct Artifact {
    bytes: Arc<[u8]>,
    sha256: [u8; SHA256_LEN],
}

impl std::fmt::Debug for Artifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Artifact")
            .field("len", &self.bytes.len())
            .field("sha256", &to_hex(&self.sha256))
            .finish()
    }
}

impl Artifact {
    /// Reads the whole file at `path` once.
    pub fn read(path: &Path) -> io::Result<Self> {
        Ok(Self::from_bytes(std::fs::read(path)?))
    }

    /// An artifact of `bytes`.
    pub fn from_bytes(bytes: impl Into<Arc<[u8]>>) -> Self {
        let bytes = bytes.into();
        let sha256 = Sha256::digest(&bytes).into();
        Self { bytes, sha256 }
    }

    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The SHA-256 of the bytes.
    pub fn sha256(&self) -> &[u8; SHA256_LEN] {
        &self.sha256
    }

    /// `len` bytes from `offset`, or `None` when the range is not inside the artifact.
    pub fn slice(&self, offset: u64, len: u32) -> Option<&[u8]> {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(usize::try_from(len).ok()?)?;
        self.bytes.get(start..end)
    }
}

/// `bytes` as lowercase hexadecimal digits.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Parses exactly 64 hexadecimal digits, in either case, as a SHA-256.
pub fn parse_sha256(text: &str) -> Result<[u8; SHA256_LEN], String> {
    let digits = text.as_bytes();
    if digits.len() != 2 * SHA256_LEN {
        return Err(format!(
            "a SHA-256 is {} hexadecimal digits, not {}",
            2 * SHA256_LEN,
            digits.len()
        ));
    }
    let mut hash = [0u8; SHA256_LEN];
    for (byte, pair) in hash.iter_mut().zip(digits.chunks_exact(2)) {
        let digit = |c: u8| {
            char::from(c)
                .to_digit(16)
                .ok_or_else(|| "a SHA-256 holds only hexadecimal digits".to_owned())
        };
        *byte = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_slices_the_bytes() {
        let artifact = Artifact::from_bytes(b"abc".to_vec());
        assert_eq!(
            to_hex(artifact.sha256()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(artifact.len(), 3);
        assert_eq!(artifact.slice(1, 2), Some(&b"bc"[..]));
        assert_eq!(artifact.slice(3, 0), Some(&b""[..]));
        assert_eq!(artifact.slice(2, 2), None);
        assert_eq!(artifact.slice(u64::MAX, 1), None);
    }

    #[test]
    fn reads_a_file() {
        let dir = std::env::temp_dir().join(format!("ngr-artifact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.bin");
        std::fs::write(&path, [1, 2, 3]).unwrap();
        let artifact = Artifact::read(&path).unwrap();
        assert_eq!(artifact.slice(0, 3), Some(&[1, 2, 3][..]));
        std::fs::write(&path, [9, 9, 9]).unwrap();
        assert_eq!(artifact.slice(0, 3), Some(&[1, 2, 3][..]), "held in memory");
        assert!(Artifact::read(&dir.join("missing")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_hex_in_either_case_and_only_64_digits() {
        let hash = Artifact::from_bytes(b"abc".to_vec());
        let text = to_hex(hash.sha256());
        assert_eq!(parse_sha256(&text), Ok(*hash.sha256()));
        assert_eq!(parse_sha256(&text.to_uppercase()), Ok(*hash.sha256()));
        assert!(parse_sha256(&text[1..]).is_err());
        assert!(parse_sha256(&format!("{text}0")).is_err());
        assert!(parse_sha256(&format!("g{}", &text[1..])).is_err());
        assert!(parse_sha256(&format!("+{}", &text[1..])).is_err());
        assert!(parse_sha256("").is_err());
    }
}
