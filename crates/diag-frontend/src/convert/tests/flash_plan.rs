//! The lowering of flash plans against the example ECU (subset 4.9, ADR-336 items 2, 4, 8, 10 and
//! 13): the boundaries and `FlashRecovery` of the lowered plan, the in-plan idempotency default,
//! and a run against a host that serves a fake artifact.

use diag_ir::{
    FlashRecovery, Interruptible, NoApplication, Op, Program, RecoveryBoundaries, RecoveryRequired,
    RecoveryTiming,
};

use super::restart::identity_table;
use super::services::appended;
use super::*;
use crate::star::StarCode;

/// A plan for flash session 1 of the example (which erases), then a check routine, called by
/// `main`.
const PLAN: &str = "\
# @section uninterruptible expected=180000
# @flashPlan session=1
def write():
    diag.request(0x10, {\"sub\": 0x02})
    diag.security_access(0x01)
    # @erase
    diag.routine(0xFF00, 0x01, {})
    res = diag.flash_transfer(1)
    if not res[\"ok\"]:
        diag.fail(\"TRANSFER_EXIT_REFUSED\", \"\")
    diag.routine(0xFF01, 0x01, {})

# @preconditions voltage externalSupply ignition
def main():
    write()
";

/// The same for a session without an erase step: no `@erase`, no erase routine.
const PLAN_NO_ERASE: &str = "\
# @section uninterruptible expected=180000
# @flashPlan session=1
def write():
    diag.request(0x10, {\"sub\": 0x02})
    diag.security_access(0x01)
    res = diag.flash_transfer(1)

# @preconditions voltage externalSupply ignition
def main():
    write()
";

/// The example's `flash_sessions.csv` with its session's steps replaced by `steps`.
fn with_steps(steps: &str) -> Vec<(String, Vec<u8>)> {
    with(
        example(),
        "flash_sessions.csv",
        &format!(
            "id,name,steps,session_timeout_ms,teardown_margin_ms,ecu_startup_ms,\
             confirmation_window_ms,version_read_retries,no_application_nrc\n\
             1,application,{steps},5000,1000,2000,3000,3,0x22\n"
        ),
    )
}

const NO_ERASE_STEPS: &str = "enter-programming-session;security-access;request-download;\
                              transfer-data;transfer-exit";

/// The example's `flash_segments.csv` with a segment of `length` bytes in blocks of `block_size`.
fn with_segment(
    files: Vec<(String, Vec<u8>)>,
    start: &str,
    length: &str,
    block_size: &str,
    format: &str,
) -> Vec<(String, Vec<u8>)> {
    with(
        files,
        "flash_segments.csv",
        &format!(
            "session,name,start_address,length,artifact_offset,block_size,data_format,\
             address_and_length_format\napplication,app,{start},{length},4,{block_size},0x00,\
             {format}\n"
        ),
    )
}

/// What lowering `source` for every variant of `files` gives: a program for each, or the errors.
fn lowered(
    files: Vec<(String, Vec<u8>)>,
    source: &str,
) -> Result<Vec<Program>, Vec<crate::star::StarError>> {
    let definition =
        resolve(read_files(with(files, "main.star", source)).expect("reads")).expect("resolves");
    lower_procedures(&definition)
        .map(|procedures| {
            procedures[0]
                .programs
                .iter()
                .map(|bytes| postcard::from_bytes(bytes).expect("a program"))
                .collect()
        })
        .map_err(|rejected| rejected.errors)
}

/// The program of the first variant, which must be valid, and the plan it holds.
fn one_plan(files: Vec<(String, Vec<u8>)>, source: &str) -> (Program, FlashRecovery) {
    let programs = lowered(files, source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    for program in &programs {
        program.validate().expect("valid");
        assert_eq!(program.flash.len(), 1);
    }
    let program = programs.into_iter().next().expect("a variant");
    let plan = program.flash[0].clone();
    (program, plan)
}

/// The pc of the first instruction at or after `from` that satisfies `is`.
fn find(program: &Program, from: usize, is: impl Fn(&Op) -> bool) -> u32 {
    (from..program.code.len())
        .find(|&pc| is(&program.code[pc]))
        .unwrap_or_else(|| panic!("no such instruction: {:?}", program.code)) as u32
}

fn routine(id: u16) -> impl Fn(&Op) -> bool {
    move |op| matches!(op, Op::RoutineControl { routine, .. } if *routine == id)
}

fn service(sid: u16) -> impl Fn(&Op) -> bool {
    move |op| matches!(op, Op::ServiceRequest { service } if *service == sid)
}

/// The timings the example's row gives.
const TIMING: RecoveryTiming = RecoveryTiming {
    session_timeout_millis: 5000,
    teardown_margin_millis: 1000,
    ecu_startup_millis: 2000,
    confirmation_window_millis: 3000,
};

#[test]
fn a_plan_with_the_default_recovery_point_requires_intervention_from_the_erase() {
    let (program, plan) = one_plan(example(), PLAN);
    // `main` is at pc 0 and its first statement is the plan, whose first instruction is the
    // session request; no call leads anywhere.
    assert!(!program.code.iter().any(|op| matches!(op, Op::Call(_))));
    let erase = find(&program, 0, routine(0xFF00));
    let exit = find(&program, 0, service(0x37));
    assert_eq!(
        plan.boundaries,
        RecoveryBoundaries {
            entry_pc: 0,
            erase_pc: erase,
            transfer_exit_pc: exit,
            // The pc of `main`'s closing return, after the plan's last statement.
            post_transfer_end_pc: program.code.len() as u32 - 1,
        }
    );
    assert_eq!(program.code.last(), Some(&Op::Ret));
    assert!(erase < find(&program, 0, service(0x34)) && find(&program, 0, service(0x34)) < exit);
    assert_eq!(plan.recovery_required, RecoveryRequired::FromPc(erase));
    assert!(!plan.allows_restart());
    // The session's row of the example's `flash_sessions.csv`.
    assert_eq!(
        (plan.flash_session, plan.stage, plan.max_resumes),
        (1, 1, 3)
    );
    assert_eq!(plan.timing, TIMING);
    assert_eq!(plan.version_read_retries, 3);
    assert_eq!(plan.no_application, Some(NoApplication::Nrc(0x22)));
    // The identity sources are the variant's table, the preconditions the named ones.
    assert!(program.identity.vin.is_some());
    assert!(program.preconditions.voltage_mv.is_some());
    assert!(program.preconditions.engine.is_none());
}

#[test]
fn recovery_never_and_a_marker_after_the_erase_allow_a_restart_with_the_identity_rows() {
    let never = PLAN.replace("session=1", "session=1 maxResumes=7 recovery=never");
    let (program, plan) = one_plan(identity_table(), &never);
    assert_eq!(plan.recovery_required, RecoveryRequired::Never);
    assert_eq!(plan.max_resumes, 7);
    assert!(plan.allows_restart());
    assert!(program.identity.hardware_part_number.is_some());
    assert!(program.identity.software_version.is_some());
    // Without the identity rows the plan is refused before anything is lowered.
    let errors = lowered(example(), &never).expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| e.code).collect::<Vec<_>>(),
        [StarCode::PlanShape]
    );

    // `@recoveryRequired` on the statement after the transfer: its first instruction.
    let marked = PLAN.replace(
        "    diag.routine(0xFF01, 0x01, {})",
        "    # @recoveryRequired\n    diag.routine(0xFF01, 0x01, {})",
    );
    let (program, plan) = one_plan(identity_table(), &marked);
    let check = find(&program, 0, routine(0xFF01));
    // The statement begins with its operand, pushed before the routine.
    assert!(matches!(program.code[check as usize - 1], Op::PushBytes(_)));
    assert_eq!(plan.recovery_required, RecoveryRequired::FromPc(check - 1));
    assert!(plan.boundaries.transfer_exit_pc < check - 1);
    assert!(plan.allows_restart());
}

#[test]
fn a_session_without_an_erase_step_has_the_download_as_its_erase_point() {
    let (program, plan) = one_plan(with_steps(NO_ERASE_STEPS), PLAN_NO_ERASE);
    let download = find(&program, 0, service(0x34));
    assert_eq!(plan.boundaries.erase_pc, download);
    assert_eq!(plan.recovery_required, RecoveryRequired::FromPc(download));
    assert_eq!(plan.no_application, Some(NoApplication::Nrc(0x22)));
    assert!(
        !program
            .code
            .iter()
            .any(|op| matches!(op, Op::RoutineControl { .. }))
    );
    // The erase marker is required with the step and refused without it.
    for (files, source) in [
        (with_steps(NO_ERASE_STEPS), PLAN),
        (example(), PLAN_NO_ERASE),
    ] {
        let errors = lowered(files, source).expect_err("refused");
        assert!(
            errors.iter().all(|e| e.code == StarCode::PlanShape),
            "{errors:?}"
        );
    }
}

#[test]
fn a_recovery_required_section_requires_intervention_from_the_entry() {
    let source = PLAN.replace("uninterruptible expected=180000", "recoveryRequired");
    let (program, plan) = one_plan(example(), &source);
    assert_eq!(plan.recovery_required, RecoveryRequired::FromPc(0));
    assert!(!plan.allows_restart());
    // The section covers the whole expansion, with the plan's attributes in every piece.
    let pieces: Vec<_> = program
        .sections
        .iter()
        .filter(|s| s.end_pc <= plan.boundaries.post_transfer_end_pc)
        .collect();
    assert_eq!(pieces[0].start_pc, 0);
    assert!(
        pieces
            .iter()
            .all(|s| matches!(s.interruptible, Interruptible::RecoveryRequired))
    );
    assert_eq!(
        pieces.last().map(|s| s.end_pc),
        Some(plan.boundaries.post_transfer_end_pc)
    );
}

#[test]
fn an_unannotated_request_in_a_plan_is_check_state_and_outside_it_unsafe() {
    let source = "\
# @section uninterruptible expected=180000
# @flashPlan session=1
def write():
    diag.request(0x10, {\"sub\": 0x02})
    # @erase
    diag.routine(0xFF00, 0x01, {})
    res = diag.flash_transfer(1)
    diag.request(0x11, {\"reset\": 1})
    # @idempotency unsafe
    diag.request(0x11, {\"reset\": 2})
    # @idempotency safe
    diag.request(0x11, {\"reset\": 3})

# @preconditions voltage externalSupply ignition
def main():
    write()
    diag.request(0x11, {\"reset\": 4})
";
    let (program, plan) = one_plan(example(), source);
    let idempotency_of = |pc: u32| {
        let covering: Vec<_> = program
            .sections
            .iter()
            .filter(|s| (s.start_pc..s.end_pc).contains(&pc))
            .collect();
        let [section] = covering[..] else {
            panic!("one section covers {pc}");
        };
        assert_eq!((section.start_pc, section.end_pc), (pc, pc + 1));
        format!("{:?}", section.idempotency)
    };
    let mut resets = Vec::new();
    let mut from = 0;
    while let Some(pc) = (from..program.code.len()).find(|&pc| service(0x11)(&program.code[pc])) {
        resets.push(pc as u32);
        from = pc + 1;
    }
    let [unannotated, unsafe_, safe, after] = resets[..] else {
        panic!("four resets: {resets:?}");
    };
    // The session change of the plan defaults to `Safe`, in a plan or not (ADR-336 item 8).
    assert_eq!(idempotency_of(find(&program, 0, service(0x10))), "Safe");
    assert_eq!(idempotency_of(unannotated), "CheckState");
    assert_eq!(idempotency_of(unsafe_), "Unsafe");
    assert_eq!(idempotency_of(safe), "Safe");
    // The default point at the erase allows no restart, so the unsafe one validates; the request
    // after the plan is outside it.
    assert!(!plan.allows_restart());
    assert!(after > plan.boundaries.post_transfer_end_pc);
    assert_eq!(idempotency_of(after), "Unsafe");
}

#[test]
fn an_unsafe_request_in_a_restartable_plan_is_refused_by_validation() {
    let source = "\
# @section uninterruptible expected=180000
# @flashPlan session=1 recovery=never
def write():
    # @erase
    diag.routine(0xFF00, 0x01, {})
    res = diag.flash_transfer(1)
    # @idempotency unsafe
    diag.request(0x11, {\"reset\": 2})

# @preconditions voltage externalSupply ignition
def main():
    write()
";
    // An unannotated reset is `CheckState` in the plan and validates; the unsafe one is refused,
    // as `Program::validate` finds an unsafe section in the replay.
    let unannotated = source.replace("    # @idempotency unsafe\n", "");
    one_plan(identity_table(), &unannotated);
    let errors = lowered(identity_table(), source).expect_err("refused");
    // The refusal names a section, not a pc: it stays not lowerable, at the file's first
    // statement.
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::NotLowerable, 3)]
    );
    assert!(errors[0].hint.contains("unsafe"), "{}", errors[0].hint);
}

#[test]
fn a_session_with_two_segments_is_not_lowerable() {
    let files = appended(
        example(),
        "flash_segments.csv",
        &["application,second,0x00020000,0x100,0x10000,0x100,0x00,0x44"],
    );
    let errors = lowered(files, PLAN).expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::NotLowerable, 8)]
    );
}

#[test]
fn the_documented_flash_plan_lowers() {
    // The example of `starlark-subset.md` 4.9, with the identity rows `recovery=never` needs
    // and an ECU reset service for its last request.
    let source = "\
# @section uninterruptible expected=180000
# @flashPlan session=1 maxResumes=3 recovery=never
def write_application():
    diag.request(0x10, {\"sub\": 0x02})
    diag.security_access(0x11)
    # @erase
    diag.routine(0xFF00, 0x01, {})
    res = diag.flash_transfer(1)
    if not res[\"ok\"]:
        diag.fail(\"TRANSFER_EXIT_REFUSED\", \"\")
    diag.routine(0xFF01, 0x01, {})
    diag.request(0x11, {\"reset\": 0x01})

# @preconditions voltage externalSupply ignition engine vehicleSpeed
def main():
    write_application()
";
    let (program, plan) = one_plan(identity_table(), source);
    assert_eq!(plan.recovery_required, RecoveryRequired::Never);
    assert!(program.preconditions.engine.is_some());
    assert!(program.preconditions.vehicle_speed.is_some());
    // The reset after the transfer is inside the plan.
    let reset = find(&program, 0, service(0x11));
    assert!(
        reset > plan.boundaries.transfer_exit_pc && reset < plan.boundaries.post_transfer_end_pc
    );
}

#[test]
fn a_plan_that_calls_a_function_is_refused_as_not_lowered_yet() {
    let source = PLAN.replace(
        "    res = diag.flash_transfer(1)",
        "    helper()\n    res = diag.flash_transfer(1)",
    ) + "\ndef helper():\n    diag.wait(1)\n";
    let errors = lowered(example(), &source).expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::NotLowerable, 8)]
    );
}

#[test]
fn a_transfer_outside_a_plan_function_is_a_plan_shape_error() {
    let errors =
        lowered(example(), "def main():\n    diag.flash_transfer(1)\n").expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::PlanShape, 2)]
    );
}

/// A host that answers positively, serves `artifact` and records what it is sent.
struct PlanHost {
    artifact: Vec<u8>,
    log: Vec<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl diag_ir::DiagHost for PlanHost {
    type Error = NotSent;

    fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, NotSent> {
        self.log
            .push(format!("request {service:02x} {}", hex(payload)));
        // Positive: the SID plus 0x40, which the seed step reads past two bytes of.
        Ok(vec![service as u8 + 0x40, 0x01])
    }
    fn send_receive(&mut self, _: u32, _: &[u8]) -> Result<Vec<u8>, NotSent> {
        Err(NotSent)
    }
    fn read_dtc(&mut self, _: u8) -> Result<Vec<u8>, NotSent> {
        Err(NotSent)
    }
    fn routine_control(
        &mut self,
        routine: u16,
        sub: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, NotSent> {
        self.log
            .push(format!("routine {routine:04x} {sub:02x} {}", hex(payload)));
        Ok(vec![0x71, sub, (routine >> 8) as u8, routine as u8])
    }
    fn security_access(&mut self, _: u64, level: u8, _: &[u8]) -> Result<Option<Vec<u8>>, NotSent> {
        self.log.push(format!("key {level:02x}"));
        Ok(Some(vec![0xAA]))
    }
    fn flash_transfer(&mut self, _: u32, data: &[u8]) -> Result<(), NotSent> {
        self.log.push(format!("transfer {}", hex(data)));
        Ok(())
    }
    fn wait(&mut self, _: u64, _: u32) -> Result<bool, NotSent> {
        Ok(true)
    }
    fn hmi_request(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, NotSent> {
        Err(NotSent)
    }
    fn record_input(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, NotSent> {
        Err(NotSent)
    }
    fn monitor_capture(&mut self, _: u32) -> Result<(), NotSent> {
        Err(NotSent)
    }
    fn artifact_bytes(&mut self, offset: u64, len: u32) -> Result<Vec<u8>, NotSent> {
        let from = offset as usize;
        Ok(self.artifact[from..from + len as usize].to_vec())
    }
    fn runtime_input(&mut self, _: diag_ir::RuntimeInput) -> Result<i64, NotSent> {
        Ok(1)
    }
    fn log(&mut self, _: u8, _: &str) {}
}

#[test]
fn a_lowered_plan_serves_the_blocks_from_the_artifact_in_order() {
    // 0x25 bytes from artifact offset 4 in blocks of 0x10: blocks of 16, 16 and 5 bytes, for an
    // address of 0x08000000 and the default format 0x44.
    let files = with_segment(example(), "0x08000000", "0x25", "0x10", "0x44");
    let converted = convert(with(files, "main.star", PLAN));
    let document = load_ecu_document(&converted.documents[0].bytes).expect("the document loads");
    let program: Program =
        postcard::from_bytes(document.procedure().expect("a program").bytes()).expect("a program");
    program.validate().expect("valid");
    let artifact: Vec<u8> = (0..64u8)
        .map(|byte| byte.wrapping_mul(5).wrapping_add(1))
        .collect();
    let mut host = PlanHost {
        artifact: artifact.clone(),
        log: Vec::new(),
    };
    let mut vm = diag_ir::Vm::new(&program);
    for _ in 0..10_000 {
        match vm.step_with_declarations(&program, document.variant().as_ref(), &mut host) {
            Ok(diag_ir::StepOutcome::Continue) => {}
            Ok(diag_ir::StepOutcome::Finished) => break,
            other => panic!("{other:?}"),
        }
    }
    let block = |from: usize, to: usize| format!("transfer {}", hex(&artifact[from..to]));
    let log = host.log;
    let download = log
        .iter()
        .position(|entry| entry.starts_with("request 34"))
        .expect("the download");
    assert_eq!(
        log[download..download + 5],
        [
            // The data format, the format byte, the address and the length.
            "request 34 00440800000000000025".to_owned(),
            block(4, 20),
            block(20, 36),
            block(36, 41),
            // The transfer exit is the service id alone.
            "request 37 ".to_owned(),
        ]
    );
    // The erase came before the download and the check after the exit.
    assert_eq!(
        log.iter()
            .position(|entry| entry.starts_with("routine ff00")),
        Some(download - 1)
    );
    assert_eq!(log.last().map(String::as_str), Some("routine ff01 01 "));
}

#[test]
fn a_dry_run_serves_zero_blocks_to_a_lowered_plan() {
    use diag_ir::dry_run::{DryRunEnd, DryRunEvent, Script, dry_run};
    // The dry run answers a request through a declared service: the example declares the
    // download and the exit.
    let files = with_segment(example(), "0x08000000", "0x25", "0x10", "0x44");
    let source = "\
# @section uninterruptible expected=180000
# @flashPlan session=1
def write():
    # @erase
    diag.routine(0xFF00, 0x01, {})
    res = diag.flash_transfer(1)

# @preconditions voltage externalSupply ignition
def main():
    write()
";
    let converted = convert(with(files, "main.star", source));
    let document = load_ecu_document(&converted.documents[0].bytes).expect("the document loads");
    let program: Program =
        postcard::from_bytes(document.procedure().expect("a program").bytes()).expect("a program");
    let script = Script::new()
        .response("eraseMemory.start", [0x71, 0x01, 0xFF, 0x00])
        .response("requestDownload", [0x74, 0x20, 0x01, 0x00])
        .response("requestTransferExit", [0x77]);
    let report = dry_run(
        &program,
        &document.variant().expect("a variant"),
        &script,
        100_000,
    );
    assert_eq!(report.end, DryRunEnd::Finished, "{report:?}");
    let blocks: Vec<usize> = report
        .events
        .iter()
        .filter_map(|event| match event {
            DryRunEvent::FlashTransfer { data, .. } => {
                assert!(data.iter().all(|&byte| byte == 0));
                Some(data.len())
            }
            _ => None,
        })
        .collect();
    assert_eq!(blocks, [16, 16, 5]);
}

const ERASE_STEPS: &str = "enter-programming-session;security-access;erase-memory;\
                           request-download;transfer-data;transfer-exit";

#[test]
fn a_plan_that_names_the_second_of_two_sessions_takes_its_facts_and_segment() {
    let files = with(
        example(),
        "flash_sessions.csv",
        &format!(
            "id,name,steps,session_timeout_ms,teardown_margin_ms,ecu_startup_ms,\
             confirmation_window_ms,version_read_retries,no_application_nrc\n\
             1,application,{ERASE_STEPS},5000,1000,2000,3000,3,\n\
             2,calibration,{ERASE_STEPS},7000,100,200,300,1,0x22\n"
        ),
    );
    let files = appended(
        files,
        "flash_segments.csv",
        &["calibration,cal,0x20000000,0x40,8,0x20,0x00,0x24"],
    );
    let source = PLAN
        .replace("session=1", "session=2")
        .replace("flash_transfer(1)", "flash_transfer(2)");
    let (program, plan) = one_plan(files.clone(), &source);
    assert_eq!((plan.flash_session, plan.stage), (2, 2));
    assert_eq!(
        plan.timing,
        RecoveryTiming {
            session_timeout_millis: 7000,
            teardown_margin_millis: 100,
            ecu_startup_millis: 200,
            confirmation_window_millis: 300,
        }
    );
    assert_eq!(plan.version_read_retries, 1);
    assert_eq!(plan.no_application, Some(NoApplication::Nrc(0x22)));
    // The download is built from the second session's segment (format 0x24: a four-byte
    // address, a two-byte length), and its blocks are read from that session's segment 0.
    let download = find(&program, 0, service(0x34)) as usize;
    let Op::PushBytes(operand) = program.code[download - 1] else {
        panic!("the operand is pushed before the request");
    };
    assert_eq!(
        program.constants[operand as usize],
        [0x00, 0x24, 0x20, 0, 0, 0, 0, 0x40]
    );
    assert!(program.code.contains(&Op::FlashBlock {
        session: 2,
        segment: 0
    }));
    // And a plan for session 1 in the same variant still takes the first one's.
    let (_, first) = one_plan(files, PLAN);
    assert_eq!(first.timing, TIMING);
}

#[test]
fn a_session_row_that_leaves_a_recovery_fact_empty_is_plan_shape_at_the_annotation() {
    // The annotation is on line 2 of `PLAN`.
    for row in [
        // No timeout, no retry count, and no startup time.
        format!("1,application,{ERASE_STEPS},,1000,2000,3000,3,"),
        format!("1,application,{ERASE_STEPS},5000,1000,2000,3000,,"),
        format!("1,application,{ERASE_STEPS},5000,1000,,3000,3,"),
    ] {
        let files = with(
            example(),
            "flash_sessions.csv",
            &format!(
                "id,name,steps,session_timeout_ms,teardown_margin_ms,ecu_startup_ms,\
                 confirmation_window_ms,version_read_retries,no_application_nrc\n{row}\n"
            ),
        );
        let errors = lowered(files, PLAN).expect_err("refused");
        assert_eq!(
            errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
            [(StarCode::PlanShape, 2)],
            "{row}"
        );
    }
}

/// The example write procedure (`examples/sequence.example.star`) lowers for every variant of
/// the example ECU to a valid program with the plan its session's columns give, and a dry run
/// carries it through the plan with the example's own declared services.
#[test]
fn the_example_write_procedure_lowers_and_dry_runs_for_every_variant() {
    use diag_ir::RuntimeInput;
    use diag_ir::dry_run::{DryRunEnd, DryRunEvent, Script, dry_run};
    let source = include_str!("../../../examples/sequence.example.star");
    let converted = convert(with(example(), "main.star", source));
    assert_eq!(converted.documents.len(), 2);
    for converted in &converted.documents {
        let document = load_ecu_document(&converted.bytes).expect("the document loads");
        let program: Program =
            postcard::from_bytes(document.procedure().expect("a program").bytes())
                .expect("a program");
        program.validate().expect("valid");
        // One plan, the example's session 1, expanded in place: no call inside it (the
        // read-back after it calls a function).
        assert_eq!(program.flash.len(), 1, "{}", converted.name);
        let plan = &program.flash[0];
        let inside =
            plan.boundaries.entry_pc as usize..plan.boundaries.post_transfer_end_pc as usize;
        assert!(
            !program.code[inside]
                .iter()
                .any(|op| matches!(op, Op::Call(_)))
        );
        assert!(program.code.iter().any(|op| matches!(op, Op::Call(_))));
        assert_eq!(
            (plan.flash_session, plan.stage, plan.max_resumes),
            (1, 1, 3)
        );
        assert_eq!(plan.timing, TIMING);
        assert_eq!(plan.version_read_retries, 3);
        assert_eq!(plan.no_application, Some(NoApplication::Nrc(0x22)));
        let erase = find(&program, 0, routine(0xFF00));
        assert_eq!(plan.boundaries.erase_pc, erase);
        assert_eq!(plan.recovery_required, RecoveryRequired::FromPc(erase));
        assert!(!plan.allows_restart());
        // The plan's section is uninterruptible, one piece per primitive, all inside the plan.
        let pieces: Vec<_> = program
            .sections
            .iter()
            .filter(|s| s.end_pc <= plan.boundaries.post_transfer_end_pc)
            .collect();
        assert_eq!(pieces[0].start_pc, plan.boundaries.entry_pc);
        for section in pieces {
            assert!(matches!(section.interruptible, Interruptible::No));
            assert_eq!(section.expected_millis, 120_000);
        }
        assert!(program.preconditions.voltage_mv.is_some());
        assert!(program.preconditions.external_supply.is_some());
        assert!(program.preconditions.ignition.is_some());
        assert!(program.preconditions.engine.is_none());
        assert!(program.preconditions.vehicle_speed.is_none());

        let script = Script::new()
            .input(RuntimeInput::SupplyVoltageMillivolts, 13_000)
            .response("programmingSession", [0x50, 0x02, 0x00, 0x32, 0x01, 0xF4])
            .key([0x12, 0x34, 0x56, 0x78])
            .response("securityAccess", [0x67, 0x01, 0x00, 0x00, 0x00, 0x00])
            .response("securityAccess", [0x67, 0x02])
            .response("eraseMemory.start", [0x71, 0x01, 0xFF, 0x00])
            .response("requestDownload", [0x74, 0x20, 0x01, 0x00])
            .response("requestTransferExit", [0x77])
            .response(
                "checkProgrammingDependencies.start",
                [0x71, 0x01, 0xFF, 0x01, 0x00],
            )
            .response("ecuReset", [0x51, 0x01])
            .response("softwareVersion.read", [0x62, 0xF1, 0x89, b'1', b'.', b'0']);
        let report = dry_run(
            &program,
            &document.variant().expect("a variant"),
            &script,
            1_000_000,
        );
        assert_eq!(report.end, DryRunEnd::Finished, "{report:?}");
        // The example's segment is 0x10000 bytes in blocks of 0x100.
        let blocks = report
            .events
            .iter()
            .filter(|event| matches!(event, DryRunEvent::FlashTransfer { .. }))
            .count();
        assert_eq!(blocks, 256);
        // The read-back waits for the ECU's restart after the reset.
        let waits: Vec<u32> = report
            .events
            .iter()
            .filter_map(|event| match event {
                DryRunEvent::Wait { millis } => Some(*millis),
                _ => None,
            })
            .collect();
        assert_eq!(waits, [2000]);
    }
}

/// `%` formatting of a run-time int is not lowered: the error names the instructions it waits
/// for (subset 2.5), so the example procedure's messages are constant text.
#[test]
fn run_time_percent_formatting_is_not_lowerable() {
    let source = "\
def main():
    res = diag.request(0x22, {\"did\": 0xF186})
    diag.log(1, \"NRC=%d\" % res[\"fields\"][\"session\"])
";
    let errors = procedures(with(example(), "main.star", source))
        .expect_err("rejected")
        .errors;
    assert!(!errors.is_empty());
    for error in &errors {
        assert_eq!(error.code, StarCode::NotLowerable, "{error:?}");
        assert!(
            error.hint.contains("string formatting instructions"),
            "{error:?}"
        );
    }
}
