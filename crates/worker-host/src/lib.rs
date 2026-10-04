//! Agent-side host for worker processes (3.3 / 3.4 / 7.3 / 7.4).
//!
//! Workers are the service binaries (`j2534-0404-service`, `iso22900-service`),
//! one process per vendor library, built once per ABI. This crate decides which build to
//! launch for a library and drives the process lifecycle:
//!
//! - [`abi`]: determine the ABI of a vendor library from its PE / ELF header (7.3)
//! - [`service`]: locate the matching service binary, launch it, provision its auth key and
//!   obtain its gRPC endpoints over the stdio JSON-RPC control channel (7.4)
//!
//! The gRPC client (the D-PDU API surface of `vci-service-interface`) is not
//! part of this crate yet.

pub mod abi;
pub mod service;
