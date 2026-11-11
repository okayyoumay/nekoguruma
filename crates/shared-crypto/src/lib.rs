//! Signatures and hashes shared by the server and the agent (design 6.4, 11.1).
//!
//! This crate is the only place that touches a signature library, so replacing the primitive
//! changes one crate (ADR-459). Today it provides:
//!
//! - the agent key's primitive: ECDSA over P-256 with SHA-256. A [`SigningKey`] makes
//!   signatures, a [`PublicKey`] verifies them, and the [`Signer`] trait lets a key that lives
//!   in an operating-system store or a TPM sign without exposing its secret;
//! - the connection proof ([`proof`]): the agent signs a transcript that names the server, the
//!   agent and a fresh server challenge, and the server verifies it (design 6.4, 14.4).
//!
//! The same primitive will sign job instructions (design 11.1). Every message signed for a
//! purpose starts with that purpose's own tag, so a signature made for one purpose never
//! verifies for another; see [`proof`] for the rule.
//!
//! # Encodings
//!
//! - A public key is a DER SubjectPublicKeyInfo (SPKI), which names its algorithm; the API
//!   carries it as PEM. The encoding is canonical (uncompressed point), and
//!   [`PublicKey::fingerprint`] is the SHA-256 of that DER.
//! - A private key is a PKCS#8 DER document, for the stores only. It never appears in
//!   [`Debug`](std::fmt::Debug) output and is wiped when dropped.
//! - A signature is 64 bytes, the fixed-size r then s. The signer produces the low-S form and
//!   the verifier accepts only that form, so a signature has one valid encoding.

mod key;
pub mod proof;

pub use key::{
    KeyError, PublicKey, RandomError, SIGNATURE_LEN, SignError, Signature, SignatureError, Signer,
    SigningKey, VerifyError, random_32, verify,
};
