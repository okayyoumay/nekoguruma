//! Shared resolution of a VCI name to its vendor library (7.1, 7.2; ADR-228 Decision item 4).
//!
//! One module per standard and version (`j2534_0404` for J2534 v04.04) holds its discovery
//! chain; the operating-system
//! differences stay inside it, so a caller writes the same call on every platform. Parts that do
//! not depend on the standard (the 7.2 pre-load checks) belong at the crate root.
//!
//! This crate only resolves. The writability and signer checks of 7.2 are not performed here.

pub mod j2534_0404;
