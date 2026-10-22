//! The `CheckState` default and the `@idempotency` annotations of the functions a flash plan
//! calls, against the example ECU's services (subset 4.6 and 4.9, ADR-336 item 3): the lowering
//! of a plan expands them in place, and their requests stay writes that a restart may repeat.

use std::path::PathBuf;

use diag_frontend::convert::lower_procedures;
use diag_frontend::tables::{read_files, resolve};
use diag_ir::{Op, Program};

const PROCEDURE: &str = r#"
def reset_ecu():
    diag.request(0x11, {"reset": 0x01})

def write_vin_back():
    # @idempotency safe
    diag.request(0x2E, {"did": 0xF190, "vin": "WVWZZZ1JZXW000001"})

def deeper():
    reset_ecu()

# @section uninterruptible expected=120000
# @flashPlan session=1
def write_application():
    diag.request(0x10, {"sub": 0x02})
    diag.security_access(0x01)
    # @erase
    diag.routine(0xFF00, 0x01, {})
    exit = diag.flash_transfer(1)
    if not exit["ok"]:
        diag.fail("TRANSFER_EXIT_REFUSED", "the transfer exit was refused")
    write_vin_back()
    diag.request(0x2E, {"did": 0xF190, "vin": "WVWZZZ1JZXW000002"})
    deeper()

# @preconditions voltage externalSupply ignition
def main():
    write_application()
"#;

fn program() -> Program {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/example-ecu");
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .expect("example directory")
        .map(|entry| {
            let entry = entry.expect("entry");
            let name = entry.file_name().into_string().expect("UTF-8 name");
            let text = std::fs::read_to_string(entry.path())
                .expect("a text file")
                .replace("\r\n", "\n");
            (name, text.into_bytes())
        })
        .collect();
    files.push(("procedure.star".to_owned(), PROCEDURE.as_bytes().to_vec()));
    files.sort();
    let definition = resolve(read_files(files).unwrap_or_else(|e| panic!("reads: {e:?}")))
        .unwrap_or_else(|e| panic!("resolves: {e:?}"));
    let procedures =
        lower_procedures(&definition).unwrap_or_else(|e| panic!("does not lower: {e:?}"));
    let procedure = procedures
        .iter()
        .find(|p| p.file == "procedure.star")
        .expect("the procedure");
    postcard::from_bytes(&procedure.programs[0]).expect("a program")
}

/// The idempotency of each section that is a single instruction at a request to `service`.
fn requests_to(program: &Program, service: u16) -> Vec<Vec<String>> {
    (0..program.code.len())
        .filter(|&pc| program.code[pc] == (Op::ServiceRequest { service }))
        .map(|pc| {
            program
                .sections
                .iter()
                .filter(|s| s.start_pc as usize == pc && s.end_pc as usize == pc + 1)
                .map(|s| format!("{:?}", s.idempotency))
                .collect()
        })
        .collect()
}

#[test]
fn a_write_request_in_a_callee_is_check_state_unless_the_callee_annotates_it() {
    let program = program();
    program.validate().expect("valid");
    // A reset two calls deep: the plan's default, not `Unsafe`.
    assert_eq!(requests_to(&program, 0x11), [["CheckState"]]);
    // The annotated write in a callee is `Safe`; the plain write of the plan's own is not.
    assert_eq!(
        requests_to(&program, 0x2E),
        [["Safe"], ["CheckState"]],
        "{:?}",
        program.code
    );
}
