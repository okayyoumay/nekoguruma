//! A read-only job document for the simulated ECU and the worker layout the agent selects from,
//! shared through `#[path]` by `agent_intake_end_to_end.rs`, `ngr_agent_end_to_end.rs` and the
//! server's `job_end_to_end.rs`.

use std::path::Path;

use diag_ir::{IR_SCHEMA_VERSION, Op, Program};

/// Reads the VIN, then a DID the simulated ECU does not know.
pub fn program() -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::PushBytes(1),
            Op::ServiceRequest { service: 0x22 },
        ],
        constants: vec![vec![0xF1, 0x90], vec![0x12, 0x34]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
        targets: Vec::new(),
    }
}

/// A variant's document whose procedure is `procedure`: UDS over ISO 15765 at 500 kbit/s on
/// 7E0 / 7E8, the link the job opens (ADR-324).
pub fn document(procedure: &Program) -> Vec<u8> {
    use diag_ir::declaration::{EcuDocumentT, EcuVariantT, ProtocolT, encode_ecu_document};
    let mut protocol = ProtocolT::default();
    protocol.short_name = Some("ISO15765".into());
    protocol.application_layer = Some("UDS".into());
    protocol.baud_rate = 500_000;
    protocol.tx_id = 0x7E0;
    protocol.rx_id = 0x7E8;
    let mut variant = EcuVariantT::default();
    variant.id = 1;
    variant.short_name = Some("sim".into());
    variant.protocols = Some(vec![protocol]);
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("csv-starlark".into());
    document.source_digest = Some("00".into());
    document.variant = Some(Box::new(variant));
    document.procedure = Some(postcard::to_allocvec(procedure).expect("program should serialize"));
    encode_ecu_document(&document)
}

/// Installs `service`, the `j2534-0404-service` binary, as
/// `<workers>/<ABI name>/j2534-0404-service`, the layout the agent selects from.
pub fn install_worker(service: &Path, workers: &Path, abi_name: &str) {
    let dir = workers.join(abi_name);
    std::fs::create_dir_all(&dir).expect("workers directory should be writable");
    let target = dir.join(service.file_name().expect("service binary has a file name"));
    if std::fs::hard_link(service, &target).is_err() {
        std::fs::copy(service, &target).expect("service binary should be copyable");
    }
}
