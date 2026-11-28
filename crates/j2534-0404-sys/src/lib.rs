#![expect(
    clippy::undocumented_unsafe_blocks,
    reason = "staged adoption, ADR-502"
)]
#![allow(
    unsafe_code,
    reason = "raw FFI bindings to the J2534 v04.04 API and the run-time loader over a vendor library (ADR-502)"
)]
pub mod bindings;
pub mod libloading;
// Run-time `unsigned long` width facade (docs/worker-crates.md).
pub mod abi;
pub use abi::{J2534Api0404, LONG_SIZE_ENV, LongSize};
