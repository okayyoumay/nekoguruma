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
//! - the registration proof ([`registration`]): the agent signs what it sends with a registration
//!   code to show that it holds the key it registers (ADR-463, design 6.7);
//! - the job instruction ([`instruction`]): the server signs an instruction's payload bytes
//!   and the agent verifies them before it parses them; the module also joins and splits the
//!   envelope that carries the key fingerprint, the signature and the payload (ADR-461,
//!   design 11.1).
//!
//! Signing and verifying for a purpose go through [`sign_tagged`] and [`verify_tagged`], which
//! put the purpose's own tag in front of the message, so a signature made for one purpose
//! never verifies for another. The connection proof, the registration proof and the job instruction each
//! have a tag of their own. The untagged primitive is not exported.
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

pub mod instruction;
mod key;
pub mod proof;
pub mod registration;

pub use key::{
    KeyError, PublicKey, RandomError, SIGNATURE_LEN, SignError, SignErrorKind, Signature,
    SignatureError, Signer, SigningKey, VerifyError, random_32, sign_tagged, verify_tagged,
};
/// The buffer type that wipes its contents when dropped, which the key stores hand secrets over
/// in, re-exported so a user of this crate needs no direct dependency on the wiping crate.
pub use p256::elliptic_curve::zeroize::Zeroizing;
