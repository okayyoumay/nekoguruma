//! The fictional vehicle's engine-controller coding change as a job document, and the control
//! file that tells `sim-vci` something between two requests, shared through `#[path]` by
//! `agent_write_coding.rs` and the server's `job_end_to_end.rs`. Both sit in `crates/<package>`,
//! so the example's location is the same relative path from either manifest directory.

use std::path::{Path, PathBuf};

use diag_frontend::convert::{lower_procedures, to_ir};
use diag_frontend::tables::{read_files, resolve};
use diag_ir::declaration::load_ecu_document;

/// The fictional vehicle's example directory.
pub fn vehicle() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../diag-frontend/examples/fictional-vehicle")
}

/// A text file with its line endings made `\n`, so a Windows checkout reads the same bytes.
fn read_text(path: &Path) -> Vec<u8> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
        .into_bytes()
}

/// The ECM definition's document for variant `ecmV1` (the simulated ECU's software version
/// 1.0.1), with the coding procedure as its procedure.
pub fn coding_document() -> Vec<u8> {
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(vehicle().join("ecm"))
        .expect("the ECM directory")
        .map(|entry| {
            let entry = entry.expect("entry");
            let name = entry.file_name().into_string().expect("UTF-8 name");
            (name, read_text(&entry.path()))
        })
        .collect();
    files.push((
        "write-coding.star".to_owned(),
        read_text(&vehicle().join("write-coding.star")),
    ));
    files.sort();
    let tables = read_files(files).unwrap_or_else(|e| panic!("does not read: {e:?}"));
    let definition = resolve(tables).unwrap_or_else(|e| panic!("does not resolve: {e:?}"));
    let procedures =
        lower_procedures(&definition).unwrap_or_else(|e| panic!("does not lower: {e:?}"));
    to_ir(&definition, &procedures)
        .documents
        .into_iter()
        .find(|document| {
            load_ecu_document(&document.bytes)
                .expect("the document loads")
                .variant()
                .expect("a variant")
                .short_name()
                == Some("ecmV1")
        })
        .expect("the ecmV1 document")
        .bytes
}

/// Writes `command` as the control file `name`, under a temporary name first so `sim-vci`
/// never reads it half-written. It takes effect at the next J2534 call.
pub fn send(control_dir: &Path, name: &str, command: &str) {
    let tmp = control_dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, command).expect("control file should be writable");
    std::fs::rename(&tmp, control_dir.join(format!("{name}.json")))
        .expect("control file should be renamed");
}
