//! Agent-side host for worker processes (3.3 / 3.4 / 7.3 / 7.4).
//!
//! Workers are the service binaries (`j2534-0404-service`, `iso22900-service`),
//! one process per vendor library, built once per ABI. This crate decides which build to
//! launch for a library and drives the process lifecycle:
//!
//! - [`abi`]: determine the ABI of a vendor library from its PE / ELF header (7.3)
//! - [`service`]: locate the matching service binary, launch it, provision its auth key and
//!   obtain its gRPC endpoints over the stdio JSON-RPC control channel (7.4)
//! - [`client`]: connect a D-PDU API gRPC client (`vci-service-interface`) to a running worker,
//!   authenticated with bearer tokens minted from its auth key (7.4, ADR-221)

pub mod abi;
pub mod client;
pub mod service;
