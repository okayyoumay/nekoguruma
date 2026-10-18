//! Dry runs of the example ECU's definition (design 12.1, ADR-328): each procedure of
//! `examples/end-to-end`, converted with the example ECU, run by `diag_ir::dry_run` with
//! vehicle access mocked.

use std::path::PathBuf;

use diag_frontend::convert::{lower_procedures, to_ir};
use diag_frontend::tables::{read_files, resolve};
use diag_ir::declaration::load_ecu_document;
use diag_ir::decode::PhysicalValue;
use diag_ir::dry_run::{DryRunEnd, DryRunError, DryRunEvent, DryRunReport, Script, dry_run};
use diag_ir::{Failure, IR_SCHEMA_VERSION, Op, Program};

const MAX_STEPS: u64 = 100_000;

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples")
}

/// The example ECU's document for `variant` with `procedure`, a file of `examples/end-to-end`.
fn document(procedure: &str, variant: &str) -> Vec<u8> {
    let read = |path: PathBuf| {
        // A Windows checkout may have CRLF line ends.
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .replace("\r\n", "\n")
            .into_bytes()
    };
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(examples().join("example-ecu"))
        .expect("example directory")
        .map(|entry| {
            let entry = entry.expect("entry");
            let name = entry.file_name().into_string().expect("UTF-8 name");
            (name, read(entry.path()))
        })
        .collect();
    files.push((
        procedure.to_owned(),
        read(examples().join("end-to-end").join(procedure)),
    ));
    files.sort();
    let tables = read_files(files).unwrap_or_else(|e| panic!("does not read: {e:?}"));
    let definition = resolve(tables).unwrap_or_else(|e| panic!("does not resolve: {e:?}"));
    let procedures =
        lower_procedures(&definition).unwrap_or_else(|e| panic!("does not lower: {e:?}"));
    to_ir(&definition, &procedures)
        .documents
        .into_iter()
        .find(|document| document.name == variant)
        .unwrap_or_else(|| panic!("no document for {variant}"))
        .bytes
}

/// Dry-runs `program`, or the document's own procedure, on the document's variant.
fn run(document: &[u8], program: Option<&Program>, script: &Script) -> DryRunReport {
    let loaded = load_ecu_document(document).expect("the document loads");
    let own: Program;
    let program = match program {
        Some(program) => program,
        None => {
            own = postcard::from_bytes(loaded.procedure().expect("a procedure").bytes())
                .expect("a program");
            &own
        }
    };
    dry_run(
        program,
        &loaded.variant().expect("a variant"),
        script,
        MAX_STEPS,
    )
}

fn bare_program(code: Vec<Op>, constants: Vec<Vec<u8>>) -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code,
        constants,
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    }
}

/// The requests of `report`: the service each went to and whether its answer was built.
fn requests(report: &DryRunReport) -> Vec<(&str, bool)> {
    report
        .events
        .iter()
        .filter_map(|event| match event {
            DryRunEvent::Request { service, built, .. } => Some((service.as_str(), *built)),
            _ => None,
        })
        .collect()
}

/// The value of field `name` decoded from the answer to the request at `index`.
fn decoded(report: &DryRunReport, index: usize, name: &str) -> PhysicalValue {
    let Some(DryRunEvent::Request { decoded, .. }) = report
        .events
        .iter()
        .filter(|event| matches!(event, DryRunEvent::Request { .. }))
        .nth(index)
    else {
        panic!("no request {index}");
    };
    let fields = decoded
        .as_ref()
        .expect("a positive response is declared")
        .as_ref()
        .expect("the answer decodes");
    fields
        .iter()
        .find(|field| field.short_name == name)
        .unwrap_or_else(|| panic!("no field {name} in {fields:?}"))
        .value
        .clone()
}

fn read_did(did: u16, data: &[u8]) -> Vec<u8> {
    let mut response = vec![0x62];
    response.extend(did.to_be_bytes());
    response.extend_from_slice(data);
    response
}

/// The answers the simulated ECU of `ecu.json` gives the read procedure.
fn ecu_script() -> Script {
    ecu_script_with_dtcs(&[
        0x59, 0x02, 0xFF, 0x12, 0x34, 0x56, 0x09, 0xAB, 0xCD, 0xEF, 0x2F,
    ])
}

/// [`ecu_script`] with `dtcs` as the answer to the DTC read.
fn ecu_script_with_dtcs(dtcs: &[u8]) -> Script {
    Script::new()
        .response("activeSession.read", read_did(0xF186, &[0x01]))
        .response("sparePartNumber.read", read_did(0xF187, b"NGR-SIM-ECU"))
        .response("softwareVersion.read", read_did(0xF189, b"1.0.0"))
        .response("vin.read", read_did(0xF190, b"NGRSIMECU00000001"))
        .response("readDtcByStatusMask", dtcs)
}

#[test]
fn the_read_procedure_runs_with_scripted_answers() {
    let report = run(&document("read.star", "exampleEcuV1"), None, &ecu_script());
    assert_eq!(report.end, DryRunEnd::Finished, "{report:#?}");
    assert_eq!(
        requests(&report),
        [
            ("activeSession.read", false),
            ("sparePartNumber.read", false),
            ("softwareVersion.read", false),
            ("vin.read", false),
            ("readDtcByStatusMask", false),
        ]
    );
    let DryRunEvent::Request { request, .. } = &report.events[3] else {
        panic!("the VIN request: {:?}", report.events[3]);
    };
    assert_eq!(request, &[0x22, 0xF1, 0x90]);
    let DryRunEvent::Request { request, .. } = &report.events[4] else {
        panic!("the DTC request: {:?}", report.events[4]);
    };
    assert_eq!(request, &[0x19, 0x02, 0x08]);
    assert_eq!(
        decoded(&report, 3, "vin"),
        PhysicalValue::Text("NGRSIMECU00000001".to_owned())
    );
    assert_eq!(
        report.events.last(),
        Some(&DryRunEvent::Log {
            level: 1,
            message: "read".to_owned()
        })
    );
}

#[test]
fn answers_built_from_the_declarations_reach_the_procedure() {
    // Without a script every answer is built: the active session reads 0, which the procedure
    // refuses, so the run ends at its first check.
    let report = run(&document("read.star", "exampleEcuV1"), None, &Script::new());
    assert_eq!(requests(&report), [("activeSession.read", true)]);
    let DryRunEvent::Request { answer, .. } = &report.events[0] else {
        panic!("{:?}", report.events[0]);
    };
    assert_eq!(answer, &[0x62, 0xF1, 0x86, 0x00]);
    assert_eq!(decoded(&report, 0, "session"), PhysicalValue::Unsigned(0));
    let DryRunEnd::Failed(Failure { code, .. }) = &report.end else {
        panic!("{report:#?}");
    };
    assert_eq!(code, "SESSION");

    // With the session scripted, the spare part number is built as the fewest bytes its
    // terminated field decodes from, which the procedure's check then refuses.
    let script = Script::new().response("activeSession.read", read_did(0xF186, &[0x01]));
    let report = run(&document("read.star", "exampleEcuV1"), None, &script);
    assert_eq!(
        requests(&report),
        [
            ("activeSession.read", false),
            ("sparePartNumber.read", true)
        ]
    );
    let DryRunEnd::Failed(Failure { code, .. }) = &report.end else {
        panic!("{report:#?}");
    };
    assert_eq!(code, "PART_NUMBER");
}

#[test]
fn a_scripted_answer_takes_the_procedures_failure_path() {
    let script = Script::new()
        .response("activeSession.read", read_did(0xF186, &[0x01]))
        .response("sparePartNumber.read", read_did(0xF187, b"OTHER"));
    let report = run(&document("read.star", "exampleEcuV1"), None, &script);
    let DryRunEnd::Failed(Failure { code, .. }) = &report.end else {
        panic!("{report:#?}");
    };
    assert_eq!(code, "PART_NUMBER");

    // One confirmed DTC instead of two.
    let script = ecu_script_with_dtcs(&[0x59, 0x02, 0xFF, 0x12, 0x34, 0x56, 0x09]);
    let report = run(&document("read.star", "exampleEcuV1"), None, &script);
    let DryRunEnd::Failed(Failure { code, .. }) = &report.end else {
        panic!("{report:#?}");
    };
    assert_eq!(code, "DTC_COUNT");
}

#[test]
fn the_routine_procedure_runs_with_a_scripted_negative_response() {
    let script = Script::new()
        .response("programmingSession", [0x50, 0x02, 0x00, 0x32, 0x01, 0xF4])
        .response("checkProgrammingDependencies.start", [0x7F, 0x31, 0x24]);
    let report = run(&document("routine.star", "exampleEcuV1"), None, &script);
    assert_eq!(report.end, DryRunEnd::Finished, "{report:#?}");
    assert_eq!(
        requests(&report),
        [
            ("programmingSession", false),
            ("checkProgrammingDependencies.start", false)
        ]
    );
    let DryRunEvent::Request {
        request, decoded, ..
    } = &report.events[1]
    else {
        panic!("{:?}", report.events[1]);
    };
    assert_eq!(request, &[0x31, 0x01, 0xFF, 0x01]);
    assert!(
        matches!(decoded, Some(Err(_))),
        "a negative response does not decode"
    );
}

#[test]
fn a_list_is_answered_in_order_then_with_its_last_answer() {
    let document = document("read.star", "exampleEcuV1");
    let send = |did: u16| {
        vec![
            Op::PushBytes(u32::from(did == 0xF190)),
            Op::ServiceRequest { service: 0x22 },
            Op::Pop,
        ]
    };
    let mut code = send(0xF186);
    code.extend(send(0xF186));
    code.extend(send(0xF186));
    let program = bare_program(code, vec![vec![0xF1, 0x86], vec![0xF1, 0x90]]);
    let script = Script::new()
        .response("activeSession.read", read_did(0xF186, &[0x01]))
        .response("activeSession.read", read_did(0xF186, &[0x02]));
    let report = run(&document, Some(&program), &script);
    assert_eq!(report.end, DryRunEnd::Finished, "{report:#?}");
    let sessions: Vec<PhysicalValue> = (0..3).map(|i| decoded(&report, i, "session")).collect();
    assert_eq!(
        sessions,
        [
            PhysicalValue::Unsigned(1),
            PhysicalValue::Unsigned(2),
            PhysicalValue::Unsigned(2)
        ]
    );
}

#[test]
fn a_request_no_service_declares_ends_the_run() {
    let document = document("read.star", "exampleEcuV1");
    // ReadDataByIdentifier with a DID the example ECU does not declare.
    let program = bare_program(
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
        vec![vec![0x12, 0x34]],
    );
    let report = run(&document, Some(&program), &Script::new());
    assert_eq!(
        report.end,
        DryRunEnd::Host(DryRunError::Undeclared {
            request: vec![0x22, 0x12, 0x34]
        })
    );
    assert!(report.events.is_empty());

    let program = bare_program(
        vec![Op::PushBytes(0), Op::SendReceive { service: 9999 }],
        vec![vec![0x22, 0xF1, 0x90]],
    );
    let report = run(&document, Some(&program), &Script::new());
    assert_eq!(
        report.end,
        DryRunEnd::Host(DryRunError::UnknownService { service: 9999 })
    );
}

#[test]
fn an_unscripted_operator_answer_ends_the_run() {
    let document = document("read.star", "exampleEcuV1");
    let program = bare_program(vec![Op::HmiRequest { form: 0 }], vec![b"form".to_vec()]);
    let report = run(&document, Some(&program), &Script::new());
    assert_eq!(
        report.end,
        DryRunEnd::Host(DryRunError::NotScripted { what: "HMI answer" })
    );

    let report = run(
        &document,
        Some(&program),
        &Script::new().form(b"ok".to_vec()),
    );
    assert_eq!(report.end, DryRunEnd::Finished, "{report:#?}");
    assert_eq!(
        report.events,
        [DryRunEvent::Hmi {
            form: b"form".to_vec(),
            answer: b"ok".to_vec()
        }]
    );
}

#[test]
fn waits_pass_at_once_and_a_loop_stops_at_the_step_limit() {
    let document = document("read.star", "exampleEcuV1");
    let program = bare_program(vec![Op::Wait { millis: 60_000 }, Op::Jump(0)], Vec::new());
    let report = run(&document, Some(&program), &Script::new());
    assert_eq!(report.end, DryRunEnd::StepLimit);
    assert_eq!(report.steps, MAX_STEPS);
    assert_eq!(report.events.len() as u64, MAX_STEPS / 2);
    assert_eq!(report.events[0], DryRunEvent::Wait { millis: 60_000 });
}
