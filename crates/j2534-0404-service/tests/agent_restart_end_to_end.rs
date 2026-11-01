// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! Restart end to end, an agent killed, its worker crashed, the ECU powered off or the VCI
//! unplugged mid-transfer, or an agent killed after RequestTransferExit (design 5.3, 5.6, 8.2.5;
//! ADR-229, ADR-276): the agent runs a flash job against a real `j2534-0404-service` worker and
//! `sim-vci` as a child process, is killed with its worker (SIGKILL to its process group on unix,
//! `TerminateJobObject` on Windows) after a block of the transfer was
//! journaled, or after RequestTransferExit was sent, and the job is resumed at library level
//! (`resume_program_journaled`) with the same journal directory. In the worker-crash, power-loss
//! and VCI-disconnect scenarios the agent runs in the test process and survives; the agent's
//! next run resumes on a new worker with the guards the failed run gave back.
//!
//! The harness (`tests/common/restart.rs`) keeps the simulated ECU's state in a state file
//! (ADR-241), so the ECU outlives the killed agent and its timers go on running. The restarted
//! agent redoes the transfer, in this order: the VIN and the ECU's hardware identity are read, the
//! safety preconditions checked, the ECUReset sent, the VIN and hardware identity read again and
//! matched, the software version checked, the preconditions checked again (8.9), the steps before
//! the erase replayed, the preconditions checked once more, and then the erase, RequestDownload,
//! blocks, RequestTransferExit and the post-transfer steps follow as on a first run. The tests
//! observe that order request by request in `sim-vci`'s request trace ([`GATE_REQUESTS`], the
//! teardown, [`REDO_REQUESTS`]), and the ECU enforces the transfer part of it, since it refuses the
//! erase while a download runs, refuses a download without an erase and a RequestTransferExit with
//! blocks missing. Its power-cycle count (DID FD01) shows that the restart reset the ECU exactly
//! once, or not at all when the restart sends no ECUReset.
//!
//! The mid-transfer agent-kill scenarios differ in the ECU's session timer (tS3_Server): the
//! restart comes while the ECU is still in its programming session with the transfer running, and
//! after the timer ran out, so the ECU fell back to its default session with the transfer
//! interrupted. Both end with the job complete: the restart sends an ECUReset first in either
//! state, so the ECU's state at the reconnection does not change the path. Neither ends in on-site
//! intervention, since the conditions that lead there (an identity or software version that is not
//! the job's, a precondition that fails) do not hold.
//!
//! Four more scenarios interrupt the first run the same way, but leave the worker alive: while the
//! agent waits after block 4, the ECU loses power (`sim_ecu::Fault::PowerLoss`) or the VCI is
//! unplugged (`disconnect_vci`, ADR-238), each handed to the worker through the control directory
//! (`Env::fault_mid_run`). The next block's transfer gets no answer, or finds the link offline,
//! since the worker's receive poll met the lost device (which the host reports as
//! `HostError::VciLost`, ADR-294), and the run ends there with its guards and journal kept, as after a worker crash. Power comes
//! back (`reconnect_ecu`) at the next run's first J2534 call, and that run's new worker finds the
//! VCI plugged in again, since an unplugged VCI is the state of the worker process that saw it.
//! A third makes `sim-vci` refuse the next block's write (`fail_writes`), which the worker
//! reports as a transmit error on the primitive. A fourth crashes the VCI (`crash_vci`, ADR-292):
//! every J2534 call fails, and the worker's receive poll takes the link offline as for a lost
//! device.
//! All four complete the job in the same restart order. A fifth scenario leaves the VCI unplugged for
//! the next run as well: its worker cannot open the device, so the run ends before its link
//! opens, sending nothing, counting no resume and keeping its guards, and the run after it, with
//! the VCI back, completes the job.
//!
//! The scenarios after RequestTransferExit kill the agent once the journal holds the
//! RequestTransferExit marker: before its response was recorded, after it was but before the
//! integrity check ran, and after the integrity check was sent but before its response was
//! recorded. Where a response is pending at the kill, the ECU keeps the agent waiting with
//! response-pending messages, injected through `sim-vci`'s control directory. The marker makes
//! the restart's teardown passive: it sends no ECUReset and waits out the ECU's session timer,
//! which the ECU's configuration keeps at the plan's declared timeout ([`S3_MS`]). All three redo the transfer,
//! so RequestTransferExit and the integrity check run again, in order.
//!
//! The teardown scenarios kill the agent mid-transfer and hand the restart's worker a fault
//! through the control directory, aimed at one service (`SimEcu::inject_for_service`): the
//! ECU refuses the ECUReset, or resets and its response is lost, so the teardown turns passive
//! (`PassiveCause::ResetRefused`, `ResetOutcomeUnknown`); or it refuses the first VIN read, so
//! the gate makes the teardown passive before any ECUReset (`PassiveCause::Gate`). A fourth
//! acknowledges the ECUReset and then answers nothing for longer than the confirmation's window,
//! so the confirmation succeeds only after the passive retry. All complete the job. The number
//! of F186 reads tells the paths apart: a passive wait leaves the ECU in its default session for
//! the first read, while an ECU that is still in its programming session, or not yet answering,
//! is read again.
//!
//! The completed-path scenarios run a variant of the job (`COMPLETED_PATH`) that waits after
//! its plan and confirms the default session with a single read per attempt, and kill the agent
//! once the post-transfer steps are journaled complete. The teardown then sends nothing and waits
//! for nothing (`Teardown::CompletedPath`), so the confirmation's first read decides: it reports
//! the programming session, the ECU refuses it, or it gets no answer (a fault aimed at the F186
//! read, `SimEcu::inject_for_request`), and the read after the passive retry confirms. Each
//! completes the job with exactly two F186 reads.
//!
//! The read-back scenario runs a variant (`READ_BACK`) that declares the software version it
//! writes, which the ECU installs at the plan's last post-transfer step, an ECUReset, and kills
//! the agent after the plan as the completed-path scenarios do. The restart reads that version
//! with the post-transfer steps journaled complete, which is the read-back verification (ADR-271):
//! it redoes nothing of the plan and goes on after it from the VM state journaled at its end.
//!
//! The converted scenario runs a procedure converted from Starlark (`CONVERTED`) instead of a
//! hand-built program: the example ECU definition with a flash plan, whose blocks come from the
//! job's artifact, killed mid-transfer as above. The definition is copied with its external-supply
//! and ignition preconditions read from the ECU (`sim-ecu`'s DIDs FD06 and FD07), since the agent
//! reads no runtime input but the supply voltage, and a restart's gates need every precondition
//! the plan declares. The restart resets the ECU, redoes the whole transfer and completes.
//!
//! The tests are serial, the entry test included: the restart sets process-wide environment
//! variables for the worker it launches, and the entry test reads one.

#[path = "common/restart.rs"]
mod restart;

use std::time::Duration;

use agent::host::HostError;
use agent::inputs::{Encoding, ServiceField, ServiceSources};
use agent::journal::{JobKey, StageId, StepRef};
use diag_ir::{
    FlashRecovery, IR_SCHEMA_VERSION, Idempotency, IdentitySources, Interruptible, Op,
    Precondition, Program, RecoveryBoundaries, RecoveryRequired, RecoveryTiming, Satisfied,
    Section, Source, Value,
};
use restart::{EcuProbe, Env, JobSpec};
use serial_test::serial;
use vci_service_interface::PduErrorEvent;

/// The libtest name of [`restart_child_entry`], which the harness runs in its child processes.
const ENTRY: &str = "restart_child_entry";

/// The requests of the restart before its teardown, in order (design 8.2.5, ADR-229 item 2),
/// as `sim-vci` traces them: the supply voltage read before anything is sent to the ECU, and the
/// gates (VIN, hardware part number, voltage).
const GATE_REQUESTS: &[&str] = &["vbatt", "uds 22 F190", "uds 22 F187", "vbatt"];

/// The requests of the restart after its teardown, in order: the default-session confirmation
/// (F186, repeated until the ECU answers; consecutive repeats are collapsed before the
/// comparison), the identity again (VIN, hardware part number), the software version, the
/// preconditions of step 4a (voltage), the programming session of the replay, the
/// preconditions before the erase (voltage), the erase, RequestDownload, blocks 1 to 8,
/// RequestTransferExit and the integrity check.
const REDO_REQUESTS: &[&str] = &[
    "uds 22 F186",
    "uds 22 F190",
    "uds 22 F187",
    "uds 22 F189",
    "vbatt",
    "uds 10 02",
    "vbatt",
    "uds 31 01 FF00",
    "uds 34",
    "uds 36 01",
    "uds 36 02",
    "uds 36 03",
    "uds 36 04",
    "uds 36 05",
    "uds 36 06",
    "uds 36 07",
    "uds 36 08",
    "uds 37",
    "uds 31 01 FF01",
];

/// The simulated ECU's erase-memory routine (`sim_ecu::RID_ERASE_MEMORY`) and its integrity
/// check (`sim_ecu::RID_CHECK_PROGRAMMING_DEPENDENCIES`).
const RID_ERASE_MEMORY: u16 = 0xFF00;
const RID_CHECK_PROGRAMMING_DEPENDENCIES: u16 = 0xFF01;

/// The VIN the job targets and the ECU answers: well-formed, as the restart's gates need.
const VIN: &str = "WDB12345678901234";
const PART_NUMBER: &[u8] = b"NGR-SIM-ECU";
const SW_VERSION: &[u8] = b"1.0.0";

const BLOCKS: u32 = 8;
const BLOCK_LEN: usize = 16;
/// The agent, or in the worker-crash scenario its worker, is killed after this block of the
/// first run; the power-loss and VCI-disconnect scenarios inject their fault there.
const KILL_AFTER_BLOCK: u32 = 4;
/// How long each of the program's waits lasts: after block [`KILL_AFTER_BLOCK`], after the last
/// block and after RequestTransferExit. The harness kills the agent or the worker, or injects a
/// fault, in this time, and the redone transfer waits them out again, so it must stay far below
/// the ECU's tS3_Server.
const PARK_MS: u32 = 1_000;
/// The plan's stage.
const STAGE: u32 = 7;
/// The erase, the first instruction the restart does not replay.
const ERASE_PC: u32 = 4;

const WAIT_FOR_BLOCK: Duration = Duration::from_secs(60);

/// The plan's declared session timeout, and the ECU's tS3_Server in the scenarios that wait it
/// out: the passive teardown waits out the declared value, so the ECU's timer must not be longer.
/// It must outlast every step of those scenarios on a slow runner, or the ECU leaves its
/// programming session where the scenario needs it there: the gap between two blocks of the
/// first run, which includes a [`PARK_MS`] wait, and the time from the probe to the restart's
/// first request, which includes starting a worker. Both took longer than 3 s on the Windows
/// runner.
const S3_MS: u32 = 10_000;

/// A programming session (the step before the erase), the erase, RequestDownload of the image, one
/// `FlashTransfer` per block with a `Wait` after block [`KILL_AFTER_BLOCK`] and after the last
/// block, RequestTransferExit, a `Wait`, and the integrity check as the post-transfer step. The
/// waits send nothing; they are where the tests kill the agent or inject a fault. The plan starts
/// at instruction 0, so a restart replays the session change; the supply voltage must stay within
/// 11 to 15 V, read from the VCI.
fn program() -> Program {
    let image_len = BLOCKS as usize * BLOCK_LEN;
    let mut constants = vec![
        // 0: the programming session (ISO 14229-1:2026 clause 9.2).
        vec![0x02],
        // 1, 3, 4: no parameters.
        Vec::new(),
        // 2: dataFormatIdentifier, address and size lengths (4 and 4), address 0, size.
        [
            &[0x00, 0x44, 0, 0, 0, 0][..],
            &(image_len as u32).to_be_bytes(),
        ]
        .concat(),
        Vec::new(),
        Vec::new(),
    ];
    let first_block = constants.len() as u32;
    constants.extend((0..BLOCKS).map(|i| vec![i as u8 + 1; BLOCK_LEN]));
    let mut code = vec![
        Op::PushBytes(0),
        Op::ServiceRequest { service: 0x10 },
        Op::Pop,
        Op::PushBytes(1),
        Op::RoutineControl {
            routine: RID_ERASE_MEMORY,
            sub: 0x01,
        },
        Op::Pop,
        Op::PushBytes(2),
        Op::ServiceRequest { service: 0x34 },
        Op::Pop,
    ];
    for i in 0..BLOCKS {
        code.push(Op::PushBytes(first_block + i));
        code.push(Op::FlashTransfer { block: i + 1 });
        if i + 1 == KILL_AFTER_BLOCK || i + 1 == BLOCKS {
            code.push(Op::Wait { millis: PARK_MS });
        }
    }
    code.push(Op::PushBytes(3));
    let exit_pc = code.len() as u32;
    code.extend([
        Op::ServiceRequest { service: 0x37 },
        Op::Wait { millis: PARK_MS },
        Op::PushBytes(4),
        Op::RoutineControl {
            routine: RID_CHECK_PROGRAMMING_DEPENDENCIES,
            sub: 0x01,
        },
    ]);
    let ecu = |field_id| Source::EcuService {
        service_id: 1,
        field_id,
    };
    let vbatt = Some(Source::RuntimeInput(
        diag_ir::RuntimeInput::SupplyVoltageMillivolts,
    ));
    let mut program = Program {
        schema_version: IR_SCHEMA_VERSION,
        code,
        constants,
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: IdentitySources {
            vin: Some(ecu(3)),
            hardware_part_number: Some(ecu(1)),
            software_version: Some(ecu(2)),
        },
        preconditions: Default::default(),
        flash: Vec::new(),
        targets: Vec::new(),
    };
    program.preconditions.voltage_mv = Some(Precondition {
        satisfied: Satisfied {
            lower: 11_000,
            upper: 15_000,
        },
        default_session: vbatt,
        programming_session: vbatt,
    });
    program.flash.push(FlashRecovery {
        flash_session: 1,
        stage: STAGE,
        max_resumes: 2,
        recovery_required: RecoveryRequired::Never,
        boundaries: RecoveryBoundaries {
            entry_pc: 0,
            erase_pc: ERASE_PC,
            transfer_exit_pc: exit_pc,
            post_transfer_end_pc: program.code.len() as u32,
        },
        timing: RecoveryTiming {
            session_timeout_millis: S3_MS,
            teardown_margin_millis: 500,
            ecu_startup_millis: 100,
            confirmation_window_millis: 2_000,
        },
        version_read_retries: 0,
        no_application: None,
    });
    program.validate().expect("the fixture is a valid program");
    program
}

/// The identity fields: ReadDataByIdentifier F190, F187 and F189.
fn sources() -> ServiceSources {
    let field = |field_id, did: u16, length| ServiceField {
        service_id: 1,
        field_id,
        request: [&[0x22][..], &did.to_be_bytes()].concat(),
        offset: 2,
        length,
        encoding: Encoding::Ascii,
    };
    ServiceSources::new(vec![
        field(1, 0xF187, PART_NUMBER.len()),
        field(2, 0xF189, SW_VERSION.len()),
        field(3, 0xF190, VIN.len()),
    ])
    .expect("identity table")
}

/// The job variant of the completed-path scenarios ([`completed_path_program`]).
const COMPLETED_PATH: &str = "completed-path";

/// [`program`] with a `Wait` after the plan, where the completed-path scenarios kill the agent
/// once the post-transfer steps are journaled complete, and a confirmation window of a single
/// read: the default-session confirmation's first read alone decides whether its first attempt
/// fails (`restart::confirm_default_session`), as the completed path's triggers need.
fn completed_path_program() -> Program {
    let mut program = program();
    program.code.push(Op::Wait { millis: PARK_MS });
    program.flash[0].timing.confirmation_window_millis = 0;
    program.validate().expect("the fixture is a valid program");
    program
}

/// The job variant of the read-back scenario ([`read_back_program`]).
const READ_BACK: &str = "read-back";

/// The software version the read-back variant writes. The ECU installs it at the first power
/// cycle after its integrity check passed (`downloaded_sw_version`), which the variant's own
/// ECUReset is. It has the same byte length as [`SW_VERSION`], since the identity table's F189
/// field has a fixed length and must decode both versions.
const INSTALLED_SW_VERSION: &str = "2.0.0";

/// [`program`] with an ECUReset as the plan's last post-transfer step, which makes the ECU run
/// the image it verified and report [`INSTALLED_SW_VERSION`], and two steps after the plan: a
/// `Wait`, where the read-back scenario kills the agent once the post-transfer steps are
/// journaled complete, and a read of the ECU's power-cycle count (DID FD01) in a `Safe` section,
/// so the continuation after the plan sends a request the trace shows and the result holds.
fn read_back_program() -> Program {
    let mut program = program();
    let reset = program.constants.len() as u32;
    program.constants.push(vec![0x01]);
    let power_cycles = program.constants.len() as u32;
    program.constants.push(0xFD01_u16.to_be_bytes().to_vec());
    program.code.extend([
        Op::PushBytes(reset),
        Op::ServiceRequest { service: 0x11 },
        Op::Pop,
    ]);
    let plan_end = program.code.len() as u32;
    program.flash[0].boundaries.post_transfer_end_pc = plan_end;
    program.code.extend([
        Op::Wait { millis: PARK_MS },
        Op::PushBytes(power_cycles),
        Op::ServiceRequest { service: 0x22 },
    ]);
    program.sections.push(Section {
        start_pc: plan_end,
        end_pc: program.code.len() as u32,
        interruptible: Interruptible::Yes,
        idempotency: Idempotency::Safe,
        expected_millis: PARK_MS + 1_000,
    });
    program.validate().expect("the fixture is a valid program");
    program
}

/// The job variant of the converted-procedure scenario ([`converted_spec`]).
const CONVERTED: &str = "converted";

/// The converted scenario's procedure: a flash plan (Starlark subset 4.9) that enters the
/// programming session, erases, transfers the session's one segment and runs the integrity
/// check, restartable from the erase on (`recovery=never`). A plan is an uninterruptible section
/// (subset 4.5).
const CONVERTED_PROCEDURE: &str = "\
# @section uninterruptible expected=180000
# @flashPlan session=1 recovery=never
def write_application():
    diag.request(0x10, {\"sub\": 0x02})
    # @erase
    diag.routine(0xFF00, 0x01, {})
    exit = diag.flash_transfer(1)
    if not exit[\"ok\"]:
        diag.fail(\"TRANSFER_EXIT_REFUSED\", \"the transfer exit was refused\")
    check = diag.routine(0xFF01, 0x01, {})
    if not check[\"ok\"]:
        diag.fail(\"CHECK_FAILED\", \"the integrity check failed\")

# @preconditions voltage externalSupply ignition
def main():
    write_application()
";

/// The converted session's one segment, of [`CONVERTED_BLOCKS`] blocks of the example's 0x100
/// bytes: the test's copy of `flash_segments.csv` shortens the example's 0x10000 bytes, whose
/// 256 blocks the restart redoes in about 16 s on a Linux debug build, too close to the
/// harness's 120 s guard on a loaded Windows runner. 32 blocks still leave the first run's
/// transfer running for a while after block [`KILL_AFTER_BLOCK`], where the kill comes.
const CONVERTED_IMAGE_LEN: usize = 0x2000;
const CONVERTED_BLOCKS: u32 = 0x20;

/// `text` with `from` replaced by `to`; `from` must be in it, so a change to the example's
/// tables fails here rather than leaving the copy as it was.
fn replaced(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "the example's table holds {from:?}");
    text.replace(from, to)
}

/// The example ECU definition (`crates/diag-frontend/examples/example-ecu`) with
/// [`CONVERTED_PROCEDURE`], its external-supply and ignition preconditions read from the ECU
/// (`sim-ecu`'s DIDs FD06 and FD07) instead of the runtime inputs the agent cannot read
/// (`WorkerHost` reads the supply voltage alone), and its segment shortened to
/// [`CONVERTED_IMAGE_LEN`] bytes, converted as the server would.
fn converted_document() -> Vec<u8> {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../diag-frontend/examples/example-ecu");
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .expect("the example directory")
        .map(|entry| {
            let entry = entry.expect("entry");
            let name = entry.file_name().into_string().expect("UTF-8 name");
            // A Windows checkout may have CRLF line ends.
            let text = std::fs::read_to_string(entry.path())
                .expect("a table")
                .replace("\r\n", "\n");
            (name, text)
        })
        .map(|(name, text)| {
            let text = match name.as_str() {
                "dids.csv" => {
                    text + ",externalSupply,0xFD06,yes,no,,,\n,ignition,0xFD07,yes,no,,,\n"
                }
                "fields.csv" => {
                    text + "did:externalSupply,externalSupply,0,,uint,8,,,,,,,,,\n\
                            did:ignition,ignition,0,,uint,8,,,,,,,,,\n"
                }
                "preconditions.csv" => replaced(
                    &replaced(
                        &text,
                        ",external_supply,1,1,input:external_supply,,",
                        ",external_supply,1,1,externalSupply.read.externalSupply,,",
                    ),
                    ",ignition,1,1,input:ignition_on,,",
                    ",ignition,1,1,ignition.read.ignition,,",
                ),
                "flash_segments.csv" => replaced(
                    &text,
                    "application,app,0x00000000,0x00010000,0,0x0100,",
                    &format!("application,app,0x00000000,{CONVERTED_IMAGE_LEN:#010X},0,0x0100,")
                        .replace("0X", "0x"),
                ),
                _ => text,
            };
            (name, text.into_bytes())
        })
        .collect();
    files.push((
        "write.star".to_owned(),
        CONVERTED_PROCEDURE.as_bytes().to_vec(),
    ));
    files.sort();
    let tables =
        diag_frontend::tables::read_files(files).unwrap_or_else(|e| panic!("does not read: {e:?}"));
    let definition = diag_frontend::tables::resolve(tables)
        .unwrap_or_else(|e| panic!("does not resolve: {e:?}"));
    let procedures = diag_frontend::convert::lower_procedures(&definition)
        .unwrap_or_else(|e| panic!("does not lower: {e:?}"));
    // The first variant's, whose software version (1.0.0) is the simulated ECU's.
    let mut documents = diag_frontend::convert::to_ir(&definition, &procedures).documents;
    documents.remove(0).bytes
}

/// The converted scenario's job: [`converted_document`]'s procedure, its identity table from
/// the document (ADR-345) and an image of [`CONVERTED_IMAGE_LEN`] bytes.
fn converted_spec() -> JobSpec {
    let document = converted_document();
    let program = agent::Job::from_document(
        agent::JobDocument::new(document.clone()).expect("the document loads"),
    )
    .expect("the document carries its procedure")
    .program()
    .clone();
    let sources = ServiceSources::from_document(
        &agent::JobDocument::new(document.clone()).expect("the document loads"),
        &program,
    )
    .expect("the document's identity table");
    JobSpec {
        program,
        document: Some(document),
        artifact: Some((0..CONVERTED_IMAGE_LEN).map(|i| (i % 251) as u8).collect()),
        sources,
        ..spec()
    }
}

/// The job the harness's agent child runs, by the scenario's variant ([`Env::with_variant`]).
fn spec_for(variant: &str) -> JobSpec {
    match variant {
        "" => spec(),
        CONVERTED => converted_spec(),
        COMPLETED_PATH => JobSpec {
            program: completed_path_program(),
            ..spec()
        },
        READ_BACK => JobSpec {
            program: read_back_program(),
            intended_software_version: Some(INSTALLED_SW_VERSION.as_bytes().to_vec()),
            ..spec()
        },
        other => panic!("unknown job variant {other:?}"),
    }
}

fn spec() -> JobSpec {
    JobSpec {
        program: program(),
        document: None,
        artifact: None,
        sources: sources(),
        key: JobKey {
            job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e71".to_owned()),
            generation: 1,
        },
        vin: VIN.to_owned(),
        intended_software_version: None,
    }
}

/// The ECU's configuration, with tS3_Server of `s3_server_ms`. No security access, so the
/// program needs no seed-key computation.
fn ecu_config(s3_server_ms: u32) -> String {
    ecu_config_with_startup(s3_server_ms, None)
}

/// [`ecu_config`], with the ECU answering nothing for `startup_ms` after an ECUReset.
fn ecu_config_with_startup(s3_server_ms: u32, startup_ms: Option<u32>) -> String {
    ecu_config_full(s3_server_ms, startup_ms, None)
}

/// [`ecu_config`], with the ECU installing `version` at the first power cycle after a verified
/// download.
fn ecu_config_installing(s3_server_ms: u32, version: &str) -> String {
    ecu_config_full(s3_server_ms, None, Some(version))
}

fn ecu_config_full(s3_server_ms: u32, startup_ms: Option<u32>, installs: Option<&str>) -> String {
    let startup_ms = startup_ms.map_or("null".to_owned(), |ms| ms.to_string());
    let installs = installs.map_or("null".to_owned(), |version| format!("\"{version}\""));
    format!(
        r#"{{"vin": "{VIN}", "part_number": "NGR-SIM-ECU", "sw_version": "1.0.0",
            "response_delay_ms": 0, "drop_at_block": null, "require_security_access": false,
            "require_gateway_auth": false, "fail_checksum": false, "s3_server_ms": {s3_server_ms},
            "startup_ms": {startup_ms}, "downloaded_sw_version": {installs}}}"#
    )
}

/// The entry of the harness's child processes: the agent that is killed, and the ECU probe. A
/// test run without the harness's environment does nothing.
#[test]
#[serial]
fn restart_child_entry() {
    restart::child_main(spec_for);
}

/// Runs the first run until `KILL_AFTER_BLOCK` is journaled and kills the agent there; returns
/// where the first attempt's transfer started. Sends
/// nothing to the ECU afterwards: a probe's request would restart its session timer.
fn kill_mid_transfer(env: &Env, spec: &JobSpec) -> StepRef {
    let agent = env.spawn_agent();
    let journal = env.wait_for_block(spec, KILL_AFTER_BLOCK, WAIT_FOR_BLOCK);
    let status = agent.kill();
    assert!(
        restart::died_of_kill(status),
        "the agent should be killed during the program's wait, not end by itself: {status}"
    );
    // The journal and the ECU agree on where the kill came: block 4 was confirmed by the ECU and
    // journaled, and the parked agent had sent nothing after it.
    let transfer = journal.facts.transfer.expect("a transfer attempt");
    assert_eq!(transfer.started_at.pc, ERASE_PC);
    assert_eq!(transfer.last_block, Some(KILL_AFTER_BLOCK));
    assert!(transfer.exit.is_none(), "no RequestTransferExit yet");
    assert!(journal.facts.resume_counts.is_empty());
    transfer.started_at
}

/// How the restart tears down the interrupted attempt before it redoes the transfer (ADR-229,
/// ADR-264), as the tests observe it: the requests before the teardown, whether it sends the
/// ECUReset, and whether the ECU reset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Teardown {
    /// The requests the restart sends before the teardown.
    gates: &'static [&'static str],
    /// The teardown sends the ECUReset.
    reset_sent: bool,
    /// The ECU reset (one more power cycle).
    ecu_reset: bool,
    /// The default-session confirmation's first F186 read confirms: the ECU is back in its
    /// default session when the confirmation starts (after an accepted reset or a passive wait).
    /// Otherwise the confirmation reads again until it does.
    first_read_confirms: bool,
}

impl Teardown {
    /// The gates pass and the ECU accepts the ECUReset.
    const RESET: Self = Self {
        gates: GATE_REQUESTS,
        reset_sent: true,
        ecu_reset: true,
        first_read_confirms: true,
    };
    /// The RequestTransferExit marker is in the journal: a passive wait, no ECUReset.
    const PASSIVE: Self = Self {
        gates: GATE_REQUESTS,
        reset_sent: false,
        ecu_reset: false,
        first_read_confirms: true,
    };
}

/// Checks the restart's requests: `teardown.gates`, the ECUReset if the teardown sends one, then
/// [`REDO_REQUESTS`]. TesterPresent requests are left out and consecutive F186 reads collapsed,
/// since the confirmation reads until the ECU answers; every other request must be there, once,
/// in order. The ECUReset and the F186 reads are counted on their own too.
fn assert_restart_order(trace: &[String], teardown: Teardown) {
    let mut seen: Vec<&str> = Vec::new();
    for line in trace.iter().map(String::as_str) {
        if line.starts_with("uds 3E") || (line == "uds 22 F186" && seen.last() == Some(&line)) {
            continue;
        }
        seen.push(line);
    }
    let confirmation_reads = trace.iter().filter(|line| *line == "uds 22 F186").count();
    assert_eq!(
        confirmation_reads == 1,
        teardown.first_read_confirms,
        "{confirmation_reads} F186 reads: {trace:#?}"
    );
    let resets = usize::from(teardown.reset_sent);
    assert_eq!(
        trace.iter().filter(|line| *line == "uds 11 01").count(),
        resets,
        "{resets} ECUReset: {trace:#?}"
    );
    let mut expected: Vec<&str> = teardown.gates.to_vec();
    if teardown.reset_sent {
        expected.push("uds 11 01");
    }
    expected.extend(REDO_REQUESTS);
    assert_eq!(seen, expected, "the restart's requests: {trace:#?}");
}

/// What a completed job leaves: the journal records one resume of the stage and a new
/// transfer attempt from the erase that took all blocks and the post-transfer steps, and the ECU
/// verified the full image.
fn assert_completed(
    env: &Env,
    spec: &JobSpec,
    first_attempt: StepRef,
    power_cycles_before: u32,
    final_sessions: &[u8],
    teardown: Teardown,
    result: Result<diag_ir::VmState, agent::JobError>,
) {
    let state = result.expect("the restarted job should complete");
    // The integrity check's positive response is the job's last result: routine 0xFF01 started,
    // status 00 (image correct).
    assert_eq!(
        state.stack.last(),
        Some(&Value::Bytes(vec![0x71, 0x01, 0xFF, 0x01, 0x00])),
        "{state:?}"
    );
    let journal = agent::journal::Journal::read(&env.journal_dir(), &spec.key).expect("journal");
    assert_eq!(journal.facts.resume_counts, [(StageId(STAGE), 1)]);
    let transfer = journal.facts.transfer.expect("a transfer attempt");
    // The redo's own transfer-start marker, at the erase, with every block and the completed
    // post-transfer steps; not the first run's, which stopped at block 4.
    assert_eq!(transfer.started_at.pc, ERASE_PC);
    assert!(
        transfer.started_at.steps > first_attempt.steps,
        "a new attempt"
    );
    assert_eq!(transfer.last_block, Some(BLOCKS));
    assert!(!transfer.interrupted);
    assert!(transfer.exit.expect("an exit marker").complete);

    assert_restart_order(&env.trace(), teardown);

    // The ECU holds the whole image, verified by its integrity check, which it passes only
    // after a RequestTransferExit with every block in order on a freshly erased flash. The
    // restart reset it once, or not at all when it sent no ECUReset. The session is checked
    // apart: the worker's stop can take longer than a short session timer.
    let probe = env.probe();
    assert!(
        final_sessions.contains(&probe.session),
        "session {} is not one of {final_sessions:?}",
        probe.session
    );
    assert_eq!(
        EcuProbe {
            session: 0,
            ..probe
        },
        EcuProbe {
            session: 0,
            flash_phase: 4,
            // A verified download is closed: no block is pending and none is counted.
            flash_block: 0,
            received: 0,
            power_cycles: power_cycles_before + u32::from(teardown.ecu_reset),
        }
    );
}

/// The agent is restarted while the ECU is in its programming session with the transfer
/// running: the restart resets the ECU, redoes the transfer and completes.
#[test]
#[serial]
fn an_agent_killed_mid_transfer_redoes_it_within_the_session_timer() {
    let spec = spec();
    // A timer far longer than the restart takes, so the ECU is still in its session.
    let env = Env::new("within-s3", ENTRY, &ecu_config(60_000));
    let first_attempt = kill_mid_transfer(&env, &spec);
    let before = env.probe();
    assert_eq!(
        before,
        EcuProbe {
            session: 2,
            flash_phase: 2,
            flash_block: KILL_AFTER_BLOCK + 1,
            received: KILL_AFTER_BLOCK * BLOCK_LEN as u32,
            power_cycles: before.power_cycles,
        },
        "the ECU should still be in its programming session, transferring"
    );
    let result = env.restart(&spec);
    // The final session is not what this scenario tests (the probe above showed it starts in
    // the programming session), and a slow runner may outlast even the 60 s timer.
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::RESET,
        result,
    );
}

/// The agent is restarted after tS3_Server ran out: the ECU fell back to its default session and
/// interrupted the transfer while nothing was running. The restart redoes the transfer and
/// completes as well.
#[test]
#[serial]
fn an_agent_killed_mid_transfer_redoes_it_after_the_session_timer_expired() {
    let spec = spec();
    let env = Env::new("after-s3", ENTRY, &ecu_config(S3_MS));
    let first_attempt = kill_mid_transfer(&env, &spec);
    // Taken after the kill, not before the spawn: starting the agent can take seconds on a busy
    // machine, and the timer runs from the last request, which came just before the kill.
    let killed_at = std::time::Instant::now();
    // The ECU's timer runs on the wall clock while no process has the library loaded (ADR-241),
    // so the time to wait is the timer's remainder.
    // Waiting longer than needed is safe: an expired timer stays expired.
    let remaining =
        Duration::from_millis(u64::from(S3_MS) + 500).saturating_sub(killed_at.elapsed());
    std::thread::sleep(remaining);
    // The ECU is in its default session, which has no timer, so the probe changes nothing.
    let before = env.probe();
    assert_eq!(
        before,
        EcuProbe {
            session: 1,
            flash_phase: 5,
            flash_block: KILL_AFTER_BLOCK,
            received: KILL_AFTER_BLOCK * BLOCK_LEN as u32,
            power_cycles: before.power_cycles,
        },
        "the ECU should be in its default session with the transfer interrupted"
    );
    let result = env.restart(&spec);
    // The timer can run out again while the worker stops, so either session is possible.
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::RESET,
        result,
    );
}

/// The worker alone crashes while the ECU is in its programming session with the transfer
/// running: the agent survives, its run ends with the worker's loss, and its next run on a new
/// worker, on the guards it kept, takes the same restart order as after an agent kill and
/// completes.
#[test]
#[serial]
fn a_worker_crashed_mid_transfer_is_restarted_like_a_killed_agent() {
    let spec = spec();
    let env = Env::new("worker-crash", ENTRY, &ecu_config(60_000));
    let crash = env.crash_worker(&spec, KILL_AFTER_BLOCK, WAIT_FOR_BLOCK);
    // The `Wait` sends nothing, so the first request to the dead worker is the next block's
    // transfer, which cannot reach it: a kill that came after the wait would fail here, not
    // later.
    let first_attempt = assert_interrupted_at_the_next_block(
        &env,
        &spec,
        &crash,
        |cause| matches!(cause, HostError::Rpc { status, .. } if status.code() == tonic::Code::Unavailable),
    );
    assert!(
        crash.link_unconfirmed,
        "a link on a dead worker cannot be confirmed closed"
    );
    let before = env.probe();
    assert_eq!(
        before,
        EcuProbe {
            session: 2,
            flash_phase: 2,
            flash_block: KILL_AFTER_BLOCK + 1,
            received: KILL_AFTER_BLOCK * BLOCK_LEN as u32,
            power_cycles: before.power_cycles,
        },
        "the ECU should still be in its programming session, transferring"
    );
    let result = env.restart_on(&spec, crash.guards);
    // The restart released them when it ended, which also shows the check above can fail.
    assert!(
        !env.guards_are_held(),
        "the ended job should release its guards"
    );
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::RESET,
        result,
    );
}

/// The instruction of the transfer of the block after [`KILL_AFTER_BLOCK`].
fn next_block_pc(spec: &JobSpec) -> u32 {
    spec.program
        .code
        .iter()
        .position(|op| {
            *op == Op::FlashTransfer {
                block: KILL_AFTER_BLOCK + 1,
            }
        })
        .expect("the next block's transfer") as u32
}

/// Checks how an interrupted first run ended: on the next block's transfer, with a failed
/// primitive (neither a result nor an on-site intervention, which would end the job instead of
/// leaving it to the restart) whose cause matches `cause`, with its guards kept, and with the journal at block
/// [`KILL_AFTER_BLOCK`], no RequestTransferExit and no resume, before and after the run ended.
/// Returns where the first attempt's transfer started.
fn assert_interrupted_at_the_next_block(
    env: &Env,
    spec: &JobSpec,
    run: &restart::InterruptedRun,
    cause: fn(&HostError) -> bool,
) -> StepRef {
    let next_block_pc = next_block_pc(spec);
    assert!(
        matches!(&run.result, Err(agent::JobError::Host { pc, source })
            if *pc == next_block_pc && cause(source)),
        "the first run should end on the next block's transfer at {next_block_pc}, for the \
         interruption's cause: {:?}",
        run.result
    );
    // The surviving job keeps its guards across the runs (design 8.2.5): nobody else can take
    // them while it holds them.
    assert!(
        env.guards_are_held(),
        "the failed run should keep its guards"
    );
    for journal in [
        run.journal.clone(),
        agent::journal::Journal::read(&env.journal_dir(), &spec.key).expect("journal"),
    ] {
        let transfer = journal.facts.transfer.expect("a transfer attempt");
        assert_eq!(transfer.started_at.pc, ERASE_PC);
        assert_eq!(transfer.last_block, Some(KILL_AFTER_BLOCK));
        assert!(transfer.exit.is_none(), "no RequestTransferExit yet");
        assert!(journal.facts.resume_counts.is_empty());
    }
    run.journal
        .facts
        .transfer
        .as_ref()
        .expect("a transfer attempt")
        .started_at
}

/// The ECU loses power while the agent waits after block 4 (design 5.6 "power loss", 8.2.5;
/// ADR-229 item 1): the ECU answers nothing from then on, so the next block's transfer gets no
/// response and the run ends there, keeping its guards and its journal. Power comes back before
/// the agent's next run, which takes the same restart order as after an agent kill and
/// completes. The power loss and the return of power are one power cycle each, and the restart's
/// ECUReset a third.
#[test]
#[serial]
fn an_ecu_power_loss_mid_transfer_is_restarted_once_power_returns() {
    let spec = spec();
    let env = Env::new("power-loss", ENTRY, &ecu_config(60_000));
    // Probed before the first run: a silent ECU cannot be probed.
    let initial = env.probe();
    let run = env.fault_mid_run(
        &spec,
        KILL_AFTER_BLOCK,
        WAIT_FOR_BLOCK,
        r#"{"command": "inject_fault", "fault": "power_loss"}"#,
    );
    assert!(env.commands_applied(), "the power loss should be applied");
    assert!(
        !run.link_unconfirmed,
        "the VCI is still there, so the link closes"
    );
    let first_attempt = assert_interrupted_at_the_next_block(&env, &spec, &run, |cause| {
        // The silent ECU leaves the transfer unanswered.
        matches!(cause, HostError::NoResponse)
    });
    // Power returns: the restart's worker applies it at its first J2534 call, before it sends
    // anything to the ECU.
    env.inject(r#"{"command": "reconnect_ecu"}"#);
    let result = env.restart_on(&spec, run.guards);
    assert!(env.commands_applied(), "the reconnection should be applied");
    assert!(
        !env.guards_are_held(),
        "the ended job should release its guards"
    );
    assert_completed(
        &env,
        &spec,
        first_attempt,
        initial.power_cycles + 2,
        &[1, 2],
        Teardown::RESET,
        result,
    );
}

/// The VCI is unplugged while the agent waits after block 4 (design 5.6, 8.2.5; ADR-229 item 1,
/// ADR-238): the next block's transfer fails on the lost device and the run ends there, keeping
/// its guards and its journal; the link's first close fails on the lost device, and the second
/// finds it gone and confirms it (ADR-393).
/// The ECU, untouched, stays in its programming session with the transfer running. The agent's
/// next run, on a new worker with the VCI plugged back in, takes the same restart order as after
/// an agent kill and completes, with the restart's ECUReset the only power cycle.
#[test]
#[serial]
fn a_vci_disconnect_mid_transfer_is_restarted_once_the_vci_is_back() {
    let spec = spec();
    let env = Env::new("vci-disconnect", ENTRY, &ecu_config(60_000));
    let run = env.fault_mid_run(
        &spec,
        KILL_AFTER_BLOCK,
        WAIT_FOR_BLOCK,
        r#"{"command": "disconnect_vci"}"#,
    );
    assert!(env.commands_applied(), "the disconnect should be applied");
    assert!(
        !run.link_unconfirmed,
        "the lost path's second close finds the link gone and the module's disconnect succeeds"
    );
    let first_attempt = assert_interrupted_at_the_next_block(&env, &spec, &run, |cause| {
        // The worker's receive poll met the lost device and took the link offline, so the
        // transfer finds no connected link; the worker's refusal carries the lost VCI, which
        // the host reports as such (ADR-294).
        matches!(cause, HostError::VciLost { .. })
    });
    // The probe loads the library in a process of its own, with the VCI present: the
    // disconnect belonged to the stopped worker's process.
    let before = env.probe();
    assert_eq!(
        before,
        EcuProbe {
            session: 2,
            flash_phase: 2,
            flash_block: KILL_AFTER_BLOCK + 1,
            received: KILL_AFTER_BLOCK * BLOCK_LEN as u32,
            power_cycles: before.power_cycles,
        },
        "the ECU should still be in its programming session, transferring"
    );
    let result = env.restart_on(&spec, run.guards);
    assert!(
        !env.guards_are_held(),
        "the ended job should release its guards"
    );
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::RESET,
        result,
    );
}

/// The VCI fails to send while the agent waits after block 4 (design 5.6, 13.4; ADR-229 item 1):
/// `sim-vci` refuses the next block's write, so that transfer fails and the run ends there,
/// keeping its guards and its journal. The VCI stays plugged in and the link closes. The ECU,
/// which never saw the block, stays in its programming session with the transfer running. The
/// agent's next run, on a new worker whose writes succeed, takes the same restart order as after
/// an agent kill and completes, with the restart's ECUReset the only power cycle.
#[test]
#[serial]
fn a_write_failure_mid_transfer_is_restarted() {
    let spec = spec();
    let env = Env::new("write-failure", ENTRY, &ecu_config(60_000));
    let run = env.fault_mid_run(
        &spec,
        KILL_AFTER_BLOCK,
        WAIT_FOR_BLOCK,
        r#"{"command": "fail_writes", "times": 1}"#,
    );
    assert!(
        env.commands_applied(),
        "the write failure should be applied"
    );
    assert!(
        !run.link_unconfirmed,
        "the VCI is still there, so the link closes"
    );
    let first_attempt = assert_interrupted_at_the_next_block(&env, &spec, &run, |cause| {
        // The worker reports the refused write as a transmit error on the primitive.
        matches!(cause, HostError::PrimitiveFailed(code) if *code == PduErrorEvent::PduErrEvtTxError as i32)
    });
    let before = env.probe();
    assert_eq!(
        before,
        EcuProbe {
            session: 2,
            flash_phase: 2,
            flash_block: KILL_AFTER_BLOCK + 1,
            received: KILL_AFTER_BLOCK * BLOCK_LEN as u32,
            power_cycles: before.power_cycles,
        },
        "the ECU should still be in its programming session, transferring"
    );
    let result = env.restart_on(&spec, run.guards);
    assert!(
        !env.guards_are_held(),
        "the ended job should release its guards"
    );
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::RESET,
        result,
    );
}

/// The VCI crashes while the agent waits after block 4 (design 5.6, 13.4; ADR-229 item 1,
/// ADR-292): it stays plugged in, but every J2534 call fails, so the next block's transfer fails
/// and the run ends there, keeping its guards and its journal. The ECU, untouched, stays in its
/// programming session with the transfer running. The agent's next run, on a new worker whose
/// VCI answers again, takes the same restart order as after an agent kill and completes, with
/// the restart's ECUReset the only power cycle.
#[test]
#[serial]
fn a_vci_crash_mid_transfer_is_restarted_once_the_vci_answers_again() {
    let spec = spec();
    let env = Env::new("vci-crash", ENTRY, &ecu_config(60_000));
    let run = env.fault_mid_run(
        &spec,
        KILL_AFTER_BLOCK,
        WAIT_FOR_BLOCK,
        r#"{"command": "crash_vci"}"#,
    );
    assert!(env.commands_applied(), "the crash should be applied");
    assert!(
        !run.link_unconfirmed,
        "the lost path's second close finds the link gone and the module's disconnect succeeds"
    );
    let first_attempt = assert_interrupted_at_the_next_block(&env, &spec, &run, |cause| {
        // The worker's receive poll met the failing VCI and took the link offline, as for a lost
        // device, so the transfer finds no connected link and the host reports the lost VCI
        // (ADR-294).
        matches!(cause, HostError::VciLost { .. })
    });
    let before = env.probe();
    assert_eq!(
        before,
        EcuProbe {
            session: 2,
            flash_phase: 2,
            flash_block: KILL_AFTER_BLOCK + 1,
            received: KILL_AFTER_BLOCK * BLOCK_LEN as u32,
            power_cycles: before.power_cycles,
        },
        "the ECU should still be in its programming session, transferring"
    );
    let result = env.restart_on(&spec, run.guards);
    assert!(
        !env.guards_are_held(),
        "the ended job should release its guards"
    );
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::RESET,
        result,
    );
}

/// The RequestTransferExit's instruction.
fn exit_pc(spec: &JobSpec) -> u32 {
    spec.program
        .code
        .iter()
        .position(|op| *op == Op::ServiceRequest { service: 0x37 })
        .expect("a RequestTransferExit") as u32
}

/// The response-pending sequence the ECU answers the unanswered request with: long enough to
/// leave time for the kill, which comes [`KILL_DELAY`] after the request was sent.
const PENDING_COUNT: u32 = 10;
const PENDING_INTERVAL_MS: u32 = 200;
const KILL_DELAY: Duration = Duration::from_millis(200);

/// What the journal and the ECU hold when the agent was killed after RequestTransferExit: the
/// exit marker, every block, no completed post-transfer steps and no resume yet, with the ECU
/// in its programming session and the download closed (`flash_phase` 3) or, after the
/// integrity check, verified (4). `last_post_step` is the pc of the last step completed after
/// the marker. Returns where the first attempt's transfer started.
fn assert_killed_after_exit(
    env: &Env,
    spec: &JobSpec,
    last_post_step: Option<u32>,
    flash_phase: u8,
) -> StepRef {
    let journal = agent::journal::Journal::read(&env.journal_dir(), &spec.key).expect("journal");
    assert!(journal.facts.resume_counts.is_empty());
    let transfer = journal.facts.transfer.expect("a transfer attempt");
    assert_eq!(transfer.started_at.pc, ERASE_PC);
    assert_eq!(transfer.last_block, Some(BLOCKS));
    let exit = transfer.exit.expect("the RequestTransferExit marker");
    assert_eq!(exit.intent_at.pc, exit_pc(spec));
    assert!(!exit.complete, "the post-transfer steps did not complete");
    // The last step completed after the marker: none before the RequestTransferExit's response
    // arrived, then that request, then the wait after it.
    assert_eq!(
        exit.last_post_step.map(|step| step.pc),
        last_post_step,
        "the last completed post-transfer step: {exit:?}"
    );
    let probe = env.probe();
    assert_eq!(
        (probe.session, probe.flash_phase),
        (2, flash_phase),
        "the ECU should be in its programming session: {probe:?}"
    );
    transfer.started_at
}

/// Kills the agent while the ECU keeps it waiting for the response to `request` (a trace line):
/// once `parked` holds, while the program waits right before that request, a response-pending
/// fault is handed to the worker; once the request was sent the agent is killed. Returns at once,
/// with the instant the ECU's final response goes out, after which its session timer runs.
fn kill_while_unanswered(
    env: &Env,
    spec: &JobSpec,
    parked: impl Fn(&agent::journal::JournalState) -> bool,
    request: &str,
) -> std::time::Instant {
    let agent = env.spawn_agent();
    // The program waits right before the request, so the fault is armed before it goes out,
    // and nothing else is sent in between to take it.
    env.wait_for(spec, "the wait before the request", WAIT_FOR_BLOCK, &parked);
    env.inject(&format!(
        concat!(
            r#"{{"command": "inject_fault", "fault": {{"response_pending": "#,
            r#"{{"count": {}, "interval_ms": {}}}}}}}"#
        ),
        PENDING_COUNT, PENDING_INTERVAL_MS
    ));
    env.wait_for(spec, request, WAIT_FOR_BLOCK, |state| {
        // A fault still waiting when the program has moved on was missed: fail on that. The
        // trace is read first: `sim-vci` applies and deletes a command before the request it
        // then sends is traced, so once the request is seen sent, the directory read after it
        // shows whether the command went first.
        let sent = env.trace().iter().any(|line| line == request);
        let applied = env.commands_applied();
        assert!(
            applied || !sent,
            "{request} was sent before the fault was applied (or the command was rejected)"
        );
        parked(state) && sent && applied
    });
    let sent_at = std::time::Instant::now();
    // The trace line is written as the request is sent, just before the ECU handles it; the
    // kill waits until the ECU has, well within the response-pending sequence.
    std::thread::sleep(KILL_DELAY);
    let status = agent.kill();
    assert!(
        restart::died_of_kill(status),
        "the agent should be killed while it waits for the response: {status}"
    );
    sent_at + Duration::from_millis(u64::from(PENDING_COUNT * PENDING_INTERVAL_MS))
}

/// Restarts once the ECU's response-pending sequence is over. The ECU restarts its session
/// timer when its final response goes out, and the restart's passive teardown waits the plan's
/// declared timeout from its last request before the wait: its requests must come after the
/// sequence ended, as
/// they would for a restart that takes longer than the sequence (a longer sequence is a backlog
/// item).
fn restart_after(
    env: &Env,
    spec: &JobSpec,
    sequence_end: std::time::Instant,
) -> Result<diag_ir::VmState, agent::JobError> {
    std::thread::sleep(sequence_end.saturating_duration_since(std::time::Instant::now()));
    env.restart(spec)
}

/// Whether the journal records the RequestTransferExit's response.
fn exit_answered(state: &agent::journal::JournalState) -> bool {
    state
        .facts
        .transfer
        .as_ref()
        .and_then(|transfer| transfer.exit.as_ref())
        .is_some_and(|exit| exit.last_post_step.is_some())
}

/// The agent is killed after it sent RequestTransferExit and before the response reached the
/// journal: the ECU answers it with response-pending messages, which keeps the agent waiting
/// while the ECU has already closed the download. The restart cannot tell whether the download
/// closed. With the RequestTransferExit marker journaled it sends no ECUReset: it waits out the
/// ECU's session timer (the plan declares the ECU's [`S3_MS`]), confirms the default session, and
/// redoes the transfer, so RequestTransferExit and the integrity check run again, in order.
#[test]
#[serial]
fn a_kill_before_the_transfer_exit_response_was_recorded_redoes_the_transfer() {
    let spec = spec();
    let env = Env::new("exit-unanswered", ENTRY, &ecu_config(S3_MS));
    // The program waits after the last block.
    let sequence_end = kill_while_unanswered(
        &env,
        &spec,
        |state| {
            state
                .facts
                .transfer
                .as_ref()
                .is_some_and(|transfer| transfer.last_block == Some(BLOCKS))
        },
        "uds 37",
    );
    let first_attempt = assert_killed_after_exit(&env, &spec, None, 3);
    let before = env.probe();
    let result = restart_after(&env, &spec, sequence_end);
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::PASSIVE,
        result,
    );
}

/// The agent is killed after the RequestTransferExit's response was journaled and before the
/// integrity check ran: the post-transfer steps did not complete, so the restart tears down
/// passively as above and redoes the transfer, and RequestTransferExit and the integrity check
/// run again, in order.
#[test]
#[serial]
fn a_kill_before_the_post_transfer_steps_completed_redoes_the_transfer() {
    let spec = spec();
    let env = Env::new("post-transfer", ENTRY, &ecu_config(S3_MS));
    let agent = env.spawn_agent();
    // The program waits after the RequestTransferExit, before the integrity check.
    env.wait_for(
        &spec,
        "RequestTransferExit answered",
        WAIT_FOR_BLOCK,
        exit_answered,
    );
    let status = agent.kill();
    assert!(
        restart::died_of_kill(status),
        "the agent should be killed during the program's wait: {status}"
    );
    let first_attempt = assert_killed_after_exit(&env, &spec, Some(exit_pc(&spec)), 3);
    let before = env.probe();
    let result = env.restart(&spec);
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::PASSIVE,
        result,
    );
}

/// The agent is killed after it sent the integrity check (the post-transfer step) and before
/// its response reached the journal: the ECU has verified the image, but the journal shows the
/// post-transfer steps incomplete, so the restart tears down passively and redoes the transfer
/// from the erase, and RequestTransferExit and the integrity check run again, in order.
#[test]
#[serial]
fn a_kill_before_the_integrity_check_response_was_recorded_redoes_the_transfer() {
    let spec = spec();
    let env = Env::new("check-unanswered", ENTRY, &ecu_config(S3_MS));
    // The program waits after the RequestTransferExit's response.
    let sequence_end = kill_while_unanswered(&env, &spec, exit_answered, "uds 31 01 FF01");
    // The wait after the RequestTransferExit completed; the integrity check did not.
    let first_attempt = assert_killed_after_exit(&env, &spec, Some(exit_pc(&spec) + 1), 4);
    let before = env.probe();
    let result = restart_after(&env, &spec, sequence_end);
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::PASSIVE,
        result,
    );
}

/// Kills the agent mid-transfer, hands `fault` (a `sim-vci` control command) to the restart's
/// worker, restarts, and checks that the job completed after `teardown`. The plan declares the
/// ECU's session timeout as [`S3_MS`], which the passive teardowns wait out, so `ecu_config` keeps it.
fn redo_after_teardown(tag: &str, ecu_config: &str, fault: Option<&str>, teardown: Teardown) {
    let spec = spec();
    let env = Env::new(tag, ENTRY, ecu_config);
    let first_attempt = kill_mid_transfer(&env, &spec);
    let before = env.probe();
    if let Some(command) = fault {
        env.inject(command);
    }
    let result = env.restart(&spec);
    assert!(env.commands_applied(), "the fault should have been applied");
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        teardown,
        result,
    );
}

/// The ECU refuses the restart's ECUReset (NRC 22, without effect): the teardown turns passive
/// (`PassiveCause::ResetRefused`), waits out the session timer, and the job completes.
#[test]
#[serial]
fn a_refused_ecu_reset_makes_the_teardown_passive() {
    redo_after_teardown(
        "reset-refused",
        &ecu_config(S3_MS),
        Some(
            r#"{"command": "inject_fault", "service": 17,
                "fault": {"negative_response": {"nrc": "conditions_not_correct"}}}"#,
        ),
        Teardown {
            gates: GATE_REQUESTS,
            reset_sent: true,
            ecu_reset: false,
            // The ECU stays in its programming session until the passive wait has run.
            first_read_confirms: true,
        },
    );
}

/// The ECU resets but the response to the restart's ECUReset is lost: the teardown cannot tell
/// whether it reset and turns passive (`PassiveCause::ResetOutcomeUnknown`), and the job
/// completes. The ECU starts up for 1.5 s after the reset, so a confirmation that followed the
/// reset at once (as after an accepted one) would read during the startup and read again; after
/// the passive wait the first read confirms.
#[test]
#[serial]
fn a_lost_ecu_reset_response_makes_the_teardown_passive() {
    redo_after_teardown(
        "reset-unanswered",
        &ecu_config_with_startup(S3_MS, Some(1_500)),
        Some(r#"{"command": "inject_fault", "service": 17, "fault": "drop_response"}"#),
        Teardown {
            gates: GATE_REQUESTS,
            reset_sent: true,
            ecu_reset: true,
            first_read_confirms: true,
        },
    );
}

/// The ECU acknowledges the restart's ECUReset and then answers nothing for 6 s while it starts
/// up. A confirmation attempt waits 0.1 s and then reads for 2 s, and an unanswered read takes
/// up to the 1 s P2 timeout, so the first attempt ends by about 3.1 s; a second attempt right
/// after it would read until about 5.2 s and fail too, while the one after the passive wait
/// ([`S3_MS`] and the 0.5 s margin) reads from about 13.7 s on. So the job completes only if the passive wait runs between
/// the attempts.
#[test]
#[serial]
fn an_ecu_silent_after_acknowledging_the_reset_is_confirmed_after_the_passive_wait() {
    redo_after_teardown(
        "reset-silent",
        &ecu_config_with_startup(S3_MS, Some(6_000)),
        None,
        Teardown {
            first_read_confirms: false,
            ..Teardown::RESET
        },
    );
}

/// The ECU refuses the restart's first VIN read: the vehicle is not identified, so the gate
/// makes the teardown passive (`PassiveCause::Gate`, `VinNotEstablished`) without the further
/// gate reads or an ECUReset, and once the ECU is back in its default session the identity
/// matches and the job completes.
#[test]
#[serial]
fn an_unreadable_vin_makes_the_teardown_passive() {
    redo_after_teardown(
        "gate-vin",
        &ecu_config(S3_MS),
        Some(
            r#"{"command": "inject_fault", "service": 34,
                "fault": {"negative_response": {"nrc": "conditions_not_correct"}}}"#,
        ),
        Teardown {
            gates: &["vbatt", "uds 22 F190"],
            reset_sent: false,
            ecu_reset: false,
            // As after the refused reset: only the passive wait brings the default session.
            first_read_confirms: true,
        },
    );
}

/// Runs the completed-path or the read-back job until its post-transfer steps are journaled
/// complete and kills the agent in the `Wait` after the plan. Returns where the transfer started.
fn kill_after_the_plan(env: &Env, spec: &JobSpec) -> StepRef {
    let agent = env.spawn_agent();
    env.wait_for(
        spec,
        "the post-transfer steps complete",
        WAIT_FOR_BLOCK,
        |state| {
            state
                .facts
                .transfer
                .as_ref()
                .and_then(|transfer| transfer.exit.as_ref())
                .is_some_and(|exit| exit.complete)
        },
    );
    let status = agent.kill();
    assert!(
        restart::died_of_kill(status),
        "the agent should be killed during the wait after the plan: {status}"
    );
    let journal = agent::journal::Journal::read(&env.journal_dir(), &spec.key).expect("journal");
    assert!(journal.facts.resume_counts.is_empty());
    let transfer = journal.facts.transfer.expect("a transfer attempt");
    assert_eq!(transfer.last_block, Some(BLOCKS));
    assert!(transfer.exit.expect("the exit marker").complete);
    transfer.started_at
}

/// Kills the completed-path job after its plan, waits for the ECU's session timer to run out if
/// `ecu_in_default_session`, hands `fault` (a `sim-vci` control command) to the restart's
/// worker, restarts, and checks the job completed after the passive retry.
///
/// The teardown takes the completed path (`Teardown::CompletedPath`): no ECUReset and no wait.
/// The confirmation's first attempt is one F186 read, which fails, so the passive wait runs and
/// the second attempt's read confirms: exactly two F186 reads, both before the identity is read
/// again. The restart then finds the ECU's software version unchanged, as the job declares no
/// version to write, and redoes the transfer, as after the other teardowns.
fn complete_on_the_completed_path(tag: &str, ecu_in_default_session: bool, fault: Option<&str>) {
    let spec = spec_for(COMPLETED_PATH);
    let env = Env::with_variant(tag, ENTRY, COMPLETED_PATH, &ecu_config(S3_MS));
    let first_attempt = kill_after_the_plan(&env, &spec);
    if ecu_in_default_session {
        // The last request came before the 1 s wait the agent was killed in.
        std::thread::sleep(Duration::from_millis(u64::from(S3_MS) + 500));
    }
    // A session still running is held until the restart's first request, however long the
    // restart takes to start.
    let before = if ecu_in_default_session {
        env.probe()
    } else {
        env.probe_holding_session()
    };
    assert_eq!(
        (before.session, before.flash_phase),
        (if ecu_in_default_session { 1 } else { 2 }, 4),
        "the ECU's session and its verified download before the restart"
    );
    if let Some(command) = fault {
        env.inject(command);
    }
    let result = env.restart(&spec);
    assert!(env.commands_applied(), "the fault should have been applied");
    let trace = env.trace();
    assert_eq!(
        trace.iter().filter(|line| *line == "uds 22 F186").count(),
        2,
        "the failed read and the one after the passive retry: {trace:#?}"
    );
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown {
            gates: GATE_REQUESTS,
            reset_sent: false,
            ecu_reset: false,
            first_read_confirms: false,
        },
        result,
    );
}

/// The ECU is still in its programming session when the restart reads F186: the first read
/// reports another session. The read restarts the ECU's session timer, which the passive wait
/// then outlasts. The probe holds the session until the restart's first request
/// ([`restart::Env::probe_holding_session`]), so the time the restart takes to start does not
/// count against the timer. The timer cannot be longer than the plan's declared timeout, which
/// the passive wait waits out, so both are [`S3_MS`].
#[test]
#[serial]
fn a_non_default_session_on_the_completed_path_is_confirmed_after_the_passive_retry() {
    complete_on_the_completed_path("completed-non-default", false, None);
}

/// The ECU is back in its default session but refuses the restart's first F186 read.
#[test]
#[serial]
fn a_refused_session_read_on_the_completed_path_is_confirmed_after_the_passive_retry() {
    complete_on_the_completed_path(
        "completed-refused",
        true,
        Some(
            r#"{"command": "inject_fault", "request": [34, 241, 134],
                "fault": {"negative_response": {"nrc": "conditions_not_correct"}}}"#,
        ),
    );
}

/// The ECU is back in its default session but the restart's first F186 read is lost on the bus,
/// so it gets no answer.
#[test]
#[serial]
fn an_unanswered_session_read_on_the_completed_path_is_confirmed_after_the_passive_retry() {
    complete_on_the_completed_path(
        "completed-unanswered",
        true,
        Some(r#"{"command": "inject_fault", "request": [34, 241, 134], "fault": "bus_error"}"#),
    );
}

/// The read-back job is killed after its plan, with the ECU already running the image it
/// verified, since the plan's own ECUReset installed it. The restart's teardown takes the completed
/// path (no ECUReset, no wait), the default-session confirmation's first read confirms, the
/// identity is read again, and the ECU reports the software version the job writes: with the
/// post-transfer steps journaled complete, that is the read-back verification (ADR-271 item 1).
/// The transfer is not redone: no erase, RequestDownload, TransferData or RequestTransferExit,
/// no integrity check and no ECUReset. The program goes on from the VM state journaled at the
/// plan's end (ADR-271 item 3), so its one request after the plan is sent and its result sits on
/// that state's stack, above the first run's integrity-check response.
#[test]
#[serial]
fn an_agent_killed_after_a_verified_plan_continues_after_the_read_back_verification() {
    let spec = spec_for(READ_BACK);
    let env = Env::with_variant(
        "read-back",
        ENTRY,
        READ_BACK,
        &ecu_config_installing(60_000, INSTALLED_SW_VERSION),
    );
    let first_attempt = kill_after_the_plan(&env, &spec);
    let before = env.probe();
    // The plan's ECUReset put the ECU in its default session with the verified image.
    assert_eq!(
        (before.session, before.flash_phase),
        (1, 4),
        "the ECU's session and its verified download before the restart"
    );

    let state = env
        .restart(&spec)
        .expect("the restarted job should complete");
    let power_cycles = [&[0x62, 0xFD, 0x01][..], &before.power_cycles.to_be_bytes()].concat();
    let integrity_check = vec![0x71, 0x01, 0xFF, 0x01, 0x00];
    assert_eq!(
        state.stack[state.stack.len().saturating_sub(2)..],
        [Value::Bytes(integrity_check), Value::Bytes(power_cycles)],
        "{state:?}"
    );

    let mut expected: Vec<&str> = GATE_REQUESTS.to_vec();
    expected.extend([
        "uds 22 F186",
        "uds 22 F190",
        "uds 22 F187",
        "uds 22 F189",
        "uds 22 FD01",
    ]);
    let trace = env.trace();
    let seen: Vec<&str> = trace
        .iter()
        .map(String::as_str)
        .filter(|line| !line.starts_with("uds 3E"))
        .collect();
    assert_eq!(seen, expected, "the restart's requests: {trace:#?}");

    let journal = agent::journal::Journal::read(&env.journal_dir(), &spec.key).expect("journal");
    assert_eq!(journal.facts.resume_counts, [(StageId(STAGE), 1)]);
    // The first run's transfer attempt, with no new transfer-start marker.
    let transfer = journal.facts.transfer.expect("a transfer attempt");
    assert_eq!(transfer.started_at, first_attempt);
    assert_eq!(transfer.last_block, Some(BLOCKS));
    assert!(transfer.exit.expect("an exit marker").complete);

    // Nothing reset the ECU or touched its flash.
    assert_eq!(env.probe(), before);
}

/// The VCI is unplugged mid-transfer, as in
/// [`a_vci_disconnect_mid_transfer_is_restarted_once_the_vci_is_back`], and is still unplugged
/// when the agent's next run starts (design 5.6 Interrupted, 8.2.5; ADR-238, ADR-258). That run's
/// worker fails to open the device (`PassThruOpen` returns `ERR_DEVICE_NOT_CONNECTED`), so the
/// run ends in `JobError::Link` before its link opens: nothing is sent to the ECU, no resume is
/// counted, and the journal is unchanged. The run gives its guards back still held, with no
/// unconfirmed link, since none was opened. A further run, with the VCI plugged back in, takes the
/// restart order and completes the job, with one resume counted.
#[test]
#[serial]
fn a_next_run_with_the_vci_still_unplugged_keeps_the_job_for_a_later_run() {
    let spec = spec();
    let env = Env::new("vci-still-unplugged", ENTRY, &ecu_config(60_000));
    let run = env.fault_mid_run(
        &spec,
        KILL_AFTER_BLOCK,
        WAIT_FOR_BLOCK,
        r#"{"command": "disconnect_vci"}"#,
    );
    assert!(env.commands_applied(), "the disconnect should be applied");
    let first_attempt = assert_interrupted_at_the_next_block(&env, &spec, &run, |cause| {
        matches!(cause, HostError::VciLost { .. })
    });
    let before = env.probe();
    let journal_before =
        agent::journal::Journal::read(&env.journal_dir(), &spec.key).expect("journal");

    // The next run's worker finds the VCI unplugged at its first J2534 call, the open.
    env.inject(r#"{"command": "disconnect_vci"}"#);
    let second = env.restart_keeping(&spec, run.guards);
    assert!(env.commands_applied(), "the disconnect should be applied");
    assert!(
        matches!(
            &second.result,
            Err(agent::JobError::Link(error)) if error.to_string().contains("ERR_DEVICE_NOT_CONNECTED")
        ),
        "the run should end on the unplugged VCI before its link opens: {:?}",
        second.result
    );
    assert_eq!(env.trace(), Vec::<String>::new(), "nothing reaches the ECU");
    assert_eq!(
        second.journal, journal_before,
        "the journal is unchanged, with no resume counted"
    );
    assert!(!second.link_unconfirmed, "no link was opened");
    assert!(env.guards_are_held(), "the run should keep its guards");
    assert_eq!(env.probe(), before, "the ECU is untouched");

    // The VCI is back for the run after: the restart order completes the job.
    let result = env.restart_on(&spec, second.guards);
    assert!(
        !env.guards_are_held(),
        "the ended job should release its guards"
    );
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::RESET,
        result,
    );
}

/// A procedure converted from Starlark, not built by hand (ADR-336, design 8.2.5): the agent
/// running [`converted_spec`]'s plan is killed mid-transfer while the ECU is in its programming
/// session, and the restart resets the ECU, redoes the transfer from the erase and completes.
/// The converted plan's transfer is one expansion with no `Wait` to park in, so the ECU holds
/// the agent instead: a response-pending sequence armed, before the agent starts, for the
/// TransferData of the block after [`KILL_AFTER_BLOCK`], and the kill comes while it runs.
#[test]
#[serial]
fn a_converted_starlark_procedure_killed_mid_transfer_is_restarted() {
    let spec = converted_spec();
    let env = Env::with_variant("converted", ENTRY, CONVERTED, &ecu_config(60_000));
    let held = KILL_AFTER_BLOCK + 1;
    let held_request = format!("uds 36 {held:02X}");
    // The first run's worker applies the command before its first request, and the fault then
    // waits for the held block's TransferData (SID 0x36, its block counter).
    env.inject(&format!(
        concat!(
            r#"{{"command": "inject_fault", "request": [54, {}], "#,
            r#""fault": {{"response_pending": {{"count": {}, "interval_ms": {}}}}}}}"#
        ),
        held, PENDING_COUNT, PENDING_INTERVAL_MS
    ));
    let agent = env.spawn_agent();
    env.wait_for(&spec, &held_request, WAIT_FOR_BLOCK, |state| {
        let sent = env.trace().contains(&held_request);
        state
            .facts
            .transfer
            .as_ref()
            .is_some_and(|transfer| transfer.last_block == Some(KILL_AFTER_BLOCK))
            && sent
            && env.commands_applied()
    });
    let sent_at = std::time::Instant::now();
    // The kill waits until the ECU has the request, well within the response-pending sequence.
    std::thread::sleep(KILL_DELAY);
    let status = agent.kill();
    assert!(
        restart::died_of_kill(status),
        "the agent should be killed while the ECU holds the block, not end by itself: {status}"
    );
    let journal = agent::journal::Journal::read(&env.journal_dir(), &spec.key).expect("journal");
    assert!(journal.facts.resume_counts.is_empty());
    let first_attempt = journal.facts.transfer.expect("a transfer attempt");
    let killed_at = first_attempt.last_block.expect("a journaled block");
    assert_eq!(
        killed_at, KILL_AFTER_BLOCK,
        "the held block was not journaled"
    );
    assert!(first_attempt.exit.is_none(), "no RequestTransferExit yet");
    // The restart comes after the ECU's sequence is over, as on a restart that takes longer.
    std::thread::sleep(
        Duration::from_millis(u64::from(PENDING_COUNT * PENDING_INTERVAL_MS))
            .saturating_sub(sent_at.elapsed()),
    );
    let plan = &spec.program.flash[0];
    assert!(plan.allows_restart(), "the plan allows a restart");
    assert_eq!(first_attempt.started_at.pc, plan.boundaries.erase_pc);
    // The ECU took the held block, whose response the kill kept from the journal, or is still
    // waiting for it.
    let before = env.probe();
    assert_eq!(
        (before.session, before.flash_phase),
        (2, 2),
        "the ECU should still be in its programming session, transferring: {before:?}"
    );
    assert!(
        [killed_at + 1, killed_at + 2].contains(&before.flash_block),
        "the ECU expects block {} after block {killed_at} was journaled",
        before.flash_block
    );

    let state = env
        .restart(&spec)
        .expect("the restarted job should complete");
    assert!(state.call_stack.is_empty(), "{state:?}");
    let journal = agent::journal::Journal::read(&env.journal_dir(), &spec.key).expect("journal");
    assert_eq!(journal.facts.resume_counts, [(StageId(plan.stage), 1)]);
    let transfer = journal.facts.transfer.expect("a transfer attempt");
    assert_eq!(transfer.started_at.pc, plan.boundaries.erase_pc);
    assert!(
        transfer.started_at.steps > first_attempt.started_at.steps,
        "a new attempt"
    );
    assert_eq!(transfer.last_block, Some(CONVERTED_BLOCKS));
    assert!(!transfer.interrupted);
    assert!(transfer.exit.expect("an exit marker").complete);

    // The restart's requests in order, as [`assert_restart_order`] reads them: the gates, each
    // precondition check reading the external supply and the ignition from the ECU after the
    // supply voltage, the ECUReset, the default-session confirmation, the identity and the
    // software version, the replayed programming session, the erase, the whole transfer and
    // the integrity check.
    const PRECONDITIONS: [&str; 3] = ["vbatt", "uds 22 FD06", "uds 22 FD07"];
    let mut expected = vec!["vbatt", "uds 22 F190", "uds 22 F187"];
    expected.extend(PRECONDITIONS);
    expected.extend([
        "uds 11 01",
        "uds 22 F186",
        "uds 22 F190",
        "uds 22 F187",
        "uds 22 F189",
    ]);
    expected.extend(PRECONDITIONS);
    expected.push("uds 10 02");
    expected.extend(PRECONDITIONS);
    expected.extend(["uds 31 01 FF00", "uds 34"]);
    let blocks: Vec<String> = (1..=CONVERTED_BLOCKS)
        .map(|block| format!("uds 36 {block:02X}"))
        .collect();
    expected.extend(blocks.iter().map(String::as_str));
    expected.extend(["uds 37", "uds 31 01 FF01"]);
    let trace = env.trace();
    let mut seen: Vec<&str> = Vec::new();
    for line in trace.iter().map(String::as_str) {
        if line.starts_with("uds 3E") || (line == "uds 22 F186" && seen.last() == Some(&line)) {
            continue;
        }
        seen.push(line);
    }
    assert_eq!(seen, expected, "the restart's requests: {trace:#?}");
    assert_restart_origins(&env, &spec);

    // The ECU verified the whole image: a closed download, reset once.
    let probe = env.probe();
    assert_eq!(
        (probe.flash_phase, probe.flash_block, probe.received),
        (4, 0, 0),
        "{probe:?}"
    );
    assert_eq!(probe.power_cycles, before.power_cycles + 1);
}

/// The communication log the restart wrote beside the journal (design 8.11) gives each of its
/// own reads the origin of the check that sent it, and the replayed and the redone steps the
/// procedure's, in the order [`a_converted_starlark_procedure_killed_mid_transfer_is_restarted`]
/// checks the requests in. A supply voltage read is no exchange, and tester presents and a
/// repeated confirmation read are left out as there.
fn assert_restart_origins(env: &Env, spec: &JobSpec) {
    let setup = agent::CommLogSetup {
        dir: env.journal_dir(),
        key: spec.key.clone(),
    };
    let text = std::fs::read_to_string(setup.path().expect("the log's path")).expect("the log");
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    // The restart's run is the last one in the log.
    let run = lines
        .iter()
        .rposition(|line| line["event"] == "job_start")
        .expect("a job start");
    // The first run's own reads are the identity read at the plan's entry (8.9.1), before the
    // erase; everything else it sent is the procedure's.
    let first: Vec<(&str, &str)> = lines[..run]
        .iter()
        .filter(|line| line["event"] == "exchange" && line["origin"] != "procedure")
        .map(|line| {
            (
                line["origin"].as_str().expect("an origin"),
                line["request"]["bytes"]["clear"]
                    .as_str()
                    .expect("a request"),
            )
        })
        .collect();
    assert!(!first.is_empty(), "{text}");
    assert!(
        first.iter().all(|(origin, _)| *origin == "identification"),
        "{first:?}"
    );
    let mut seen: Vec<(String, String)> = Vec::new();
    for entry in lines[run..]
        .iter()
        .filter(|line| line["event"] == "exchange")
    {
        let origin = entry["origin"].as_str().expect("an origin").to_owned();
        let request = entry["request"]["bytes"]["clear"]
            .as_str()
            .expect("the request in clear")
            .to_owned();
        assert_eq!(
            entry["at"].is_object(),
            origin == "procedure",
            "only the procedure's requests have a VM position: {entry}"
        );
        let repeated = seen.last() == Some(&(origin.clone(), request.clone()));
        if request.starts_with("3e") || (request == "22f186" && repeated) {
            continue;
        }
        seen.push((origin, request));
    }
    fn preconditions(origin: &'static str) -> [(&'static str, &'static str); 2] {
        [(origin, "22fd06"), (origin, "22fd07")]
    }
    // The gates: the identity, then the preconditions.
    let mut expected = vec![("identification", "22f190"), ("identification", "22f187")];
    expected.extend(preconditions("precondition"));
    expected.extend([
        ("teardown", "1101"),
        ("restart_check", "22f186"),
        ("identification", "22f190"),
        ("identification", "22f187"),
        ("guard", "22f189"),
    ]);
    expected.extend(preconditions("precondition"));
    expected.push(("procedure", "1002"));
    expected.extend(preconditions("precondition"));
    expected.push(("procedure", "3101ff00"));
    let own = expected.len();
    let seen_own: Vec<(&str, &str)> = seen[..own.min(seen.len())]
        .iter()
        .map(|(origin, request)| (origin.as_str(), request.as_str()))
        .collect();
    assert_eq!(seen_own, expected, "{text}");
    // The download, the whole transfer, its exit and the integrity check are the procedure's.
    assert_eq!(
        seen.len(),
        own + 1 + CONVERTED_BLOCKS as usize + 2,
        "{text}"
    );
    assert!(
        seen[own..].iter().all(|(origin, _)| origin == "procedure"),
        "{text}"
    );
}
