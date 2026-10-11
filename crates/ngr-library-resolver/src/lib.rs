//! Shared resolution of a VCI name to its vendor library (7.1, 7.2; ADR-228 Decision item 4).
//!
//! One module per standard and version (`j2534_0404` for J2534 v04.04, `iso22900` for the
//! D-PDU API, ISO 22900-2:2022 clause 8.7) holds its discovery chain; the operating-system
//! differences stay inside it, so a caller writes the same call on every platform. Parts that do
//! not depend on the standard (the 7.2 pre-load checks) belong at the crate root.
//!
//! Registry lookups take an explicit [`RegistryView`] (ADR-277); the `iso22900` module takes one.
//!
//! Resolution itself never loads or trusts a library. The writability check of 7.2
//! ([`check_writability`], ADR-270) is here; the signer check of 7.2 is not performed by this
//! crate, and the caller decides what to do with a finding (device mode refuses, user mode warns).

pub mod iso22900;
pub mod j2534_0404;
mod registry_view;
mod writability;

pub use registry_view::RegistryView;
pub use writability::{
    Finding, Policy, Reason, Role, WritabilityError, check_writability, check_writability_with,
};
