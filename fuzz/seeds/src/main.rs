//! Writes seed inputs for the fuzz targets into `<dir>/ngr-fuzz-ecu-document` and
//! `<dir>/ngr-fuzz-vm`: the
//! documents of `diag-frontend`'s example definition, and a few small programs with their
//! starting states, encoded as `ngr-fuzz-vm` reads them. Seeds are generated rather than
//! committed, so they always follow the current schema.

use std::path::{Path, PathBuf};

use diag_frontend::convert::to_ir;
use diag_frontend::tables::{read_directory, resolve};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Vm};

fn main() {
    let dir = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .expect("usage: ngr-fuzz-seeds <corpus dir>"),
    );
    let example = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/diag-frontend/examples/example-ecu");
    let tables = read_directory(&example).expect("the example definition reads");
    let definition = resolve(tables).expect("the example definition resolves");
    let documents = dir.join("ngr-fuzz-ecu-document");
    std::fs::create_dir_all(&documents).expect("corpus directory");
    for document in to_ir(&definition).documents {
        std::fs::write(
            documents.join(format!("{}.bin", document.name)),
            &document.bytes,
        )
        .expect("seed file");
    }

    let programs = dir.join("ngr-fuzz-vm");
    std::fs::create_dir_all(&programs).expect("corpus directory");
    for (name, code, constants) in [
        (
            "arithmetic",
            vec![Op::PushI64(6), Op::PushI64(7), Op::Mul, Op::Pop],
            vec![],
        ),
        (
            "read-vin",
            vec![
                Op::PushBytes(0),
                Op::ServiceRequest { service: 0x22 },
                Op::Pop,
            ],
            vec![vec![0xF1, 0x90]],
        ),
        (
            "loop",
            // Counts 3 down to 0, then pops the counter.
            vec![
                Op::PushI64(3),
                Op::Dup,
                Op::PushI64(0),
                Op::CmpGt,
                Op::JumpIfFalse(8),
                Op::PushI64(1),
                Op::Sub,
                Op::Jump(1),
                Op::Pop,
            ],
            vec![],
        ),
    ] {
        let program = Program {
            schema_version: IR_SCHEMA_VERSION,
            code,
            constants,
            sections: Vec::new(),
            source_map: Vec::new(),
            identity: Default::default(),
            preconditions: Default::default(),
            flash: Vec::new(),
        };
        let state = Vm::new(&program).state;
        let bytes = postcard::to_allocvec(&(&program, &state)).expect("a program encodes");
        std::fs::write(programs.join(format!("{name}.bin")), bytes).expect("seed file");
    }
}
