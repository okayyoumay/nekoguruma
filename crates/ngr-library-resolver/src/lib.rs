//! Shared resolution of a VCI name to its vendor library (7.1, 7.2; ADR-228 Decision item 4).
//!
//! One module per standard holds that standard's discovery chain; the operating-system
//! differences stay inside it, so a caller writes the same call on every platform. Parts that do
//! not depend on the standard (the 7.2 pre-load checks) belong at the crate root.
//!
//! This crate only resolves. The writability and signer checks of 7.2 are not performed here.

pub mod j2534;
