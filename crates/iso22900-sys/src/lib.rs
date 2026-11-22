#![allow(
    unsafe_code,
    reason = "raw FFI bindings to the D-PDU API and the run-time loader over a vendor library (ADR-502)"
)]
pub mod bindings;
pub mod libloading;
