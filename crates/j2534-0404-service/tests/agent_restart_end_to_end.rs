// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073. Killing the agent's
// process group is unix-only.
#![cfg(all(debug_assertions, unix))]
//! Restart end to end, an agent killed or its worker crashed mid-transfer or after
//! RequestTransferExit (design 5.3, 5.6, 8.2.5; ADR-229, ADR-276): the agent runs a flash job
//! against a real `j2534-0404-service` worker and `sim-vci` as a child process, is killed with
//! SIGKILL after a block of the transfer was journaled, or after RequestTransferExit was sent,
//! and the job is resumed at library level (`resume_program_journaled`) with the same journal
//! directory. In the worker-crash scenario the agent runs in the test process and
//! only its worker is killed; the agent's next run resumes on a new worker with the guards the
//! failed run gave back.
//!
//! The harness (`tests/common/restart.rs`) keeps the simulated ECU's state in a state file
//! (ADR-241), so the ECU outlives the killed agent and its timers go on running. The restarted
//! agent redoes the transfer, in this order: the VIN and the ECU's hardware identity are read, the
//! safety preconditions checked, the ECUReset sent, the VIN and hardware identity read again
//! and matched, the software version checked, the preconditions checked again (8.9), the steps
//! before the erase replayed, the preconditions checked once more, and then the erase,
//! RequestDownload, blocks, RequestTransferExit and the post-transfer steps follow as on a first
//! run. The tests observe that order request by request in `sim-vci`'s request trace
//! ([`EXPECTED_RESTART`]), and the ECU enforces the transfer part of it, since it refuses the
//! erase while a download runs, refuses a download without an erase and a RequestTransferExit
//! with blocks missing. Its power-cycle count (DID FD01) shows that the restart reset the ECU
//! exactly once, or not at all after a passive teardown.
//!
//! The agent-kill scenarios differ in the ECU's session timer (tS3_Server): the restart comes
//! while the ECU is still in its programming session with the transfer running, and after the
//! timer ran out, so the ECU fell back to its default session with the transfer interrupted.
//! Both end with the job complete: the restart sends an ECUReset first in either state, so the
//! ECU's state at the reconnection does not change the path. Neither ends in on-site
//! intervention, since the conditions that lead there (an identity or software version that is
//! not the job's, a precondition that fails) do not hold.
//!
//! The scenarios after RequestTransferExit kill the agent once the journal holds the
//! RequestTransferExit marker: before the response was recorded (the ECU keeps the agent
//! waiting with response-pending messages, injected through `sim-vci`'s control directory) and
//! after it was, before the integrity check ran. The marker makes the restart's teardown
//! passive: it sends no ECUReset and waits out the ECU's session timer, which the ECU's
//! configuration keeps at the plan's declared 3 s. Both redo the transfer, so RequestTransferExit
//! and the integrity check run again, in order.
//!
//! The tests are serial, the entry test included: the restart sets process-wide environment
//! variables for the worker it launches, and the entry test reads one.

#[path = "common/restart.rs"]
mod restart;

use std::time::Duration;

use agent::inputs::{Encoding, ServiceField, ServiceSources};
use agent::journal::{JobKey, StageId, StepRef};
use diag_ir::{
    FlashRecovery, IR_SCHEMA_VERSION, IdentitySources, Op, Precondition, Program,
    RecoveryBoundaries, RecoveryRequired, RecoveryTiming, Satisfied, Source, Value,
};
use restart::{EcuProbe, Env, JobSpec};
use serial_test::serial;

/// The libtest name of [`restart_child_entry`], which the harness runs in its child processes.
const ENTRY: &str = "restart_child_entry";

/// The requests of the restart, in order (design 8.2.5, ADR-229 item 2), as `sim-vci` traces
/// them: the supply voltage read before anything is sent to the ECU, the gates (VIN, hardware
/// part number, voltage), the ECUReset (hardReset), the default-session confirmation (F186,
/// repeated until the ECU answers; consecutive repeats are collapsed before the comparison),
/// the identity again (VIN, hardware part number), the software version, the preconditions of
/// step 4a (voltage), the programming session of the replay, the preconditions before the
/// erase (voltage), the erase, RequestDownload, blocks 1 to 8, RequestTransferExit and the
/// integrity check.
const EXPECTED_RESTART: &[&str] = &[
    "vbatt",
    "uds 22 F190",
    "uds 22 F187",
    "vbatt",
    "uds 11 01",
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
/// first run.
const KILL_AFTER_BLOCK: u32 = 4;
/// How long the program waits after that block. The harness kills the agent or the worker in
/// this time, and
/// the redone transfer waits it out again, so it must stay far below the ECU's tS3_Server.
const PARK_MS: u32 = 1_000;
/// The plan's stage.
const STAGE: u32 = 7;
/// The erase, the first instruction the restart does not replay.
const ERASE_PC: u32 = 4;

const WAIT_FOR_BLOCK: Duration = Duration::from_secs(60);

/// A programming session (the step before the erase), the erase, RequestDownload of the image,
/// one `FlashTransfer` per block with a `Wait` after block [`KILL_AFTER_BLOCK`] and after the
/// last block, RequestTransferExit, a `Wait`, and the integrity check as the post-transfer step.
/// The waits send nothing; they are where the tests kill the agent or inject a fault. The plan starts at
/// instruction 0, so a restart replays the session change; the supply voltage must stay within
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
            session_timeout_millis: 3_000,
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

fn spec() -> JobSpec {
    JobSpec {
        program: program(),
        sources: sources(),
        key: JobKey {
            job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e71".to_owned()),
            generation: 1,
        },
        vin: VIN.to_owned(),
    }
}

/// The ECU's configuration, with tS3_Server of `s3_server_ms`. No security access, so the
/// program needs no seed-key computation.
fn ecu_config(s3_server_ms: u32) -> String {
    format!(
        r#"{{"vin": "{VIN}", "part_number": "NGR-SIM-ECU", "sw_version": "1.0.0",
            "response_delay_ms": 0, "drop_at_block": null, "require_security_access": false,
            "require_gateway_auth": false, "fail_checksum": false, "s3_server_ms": {s3_server_ms}}}"#
    )
}

/// The entry of the harness's child processes: the agent that is killed, and the ECU probe. A
/// test run without the harness's environment does nothing.
#[test]
#[serial]
fn restart_child_entry() {
    restart::child_main(spec);
}

/// Runs the first run until `KILL_AFTER_BLOCK` is journaled and kills the agent there; returns
/// where the first attempt's transfer started. Sends
/// nothing to the ECU afterwards: a probe's request would restart its session timer.
fn kill_mid_transfer(env: &Env, spec: &JobSpec) -> StepRef {
    let agent = env.spawn_agent();
    let journal = env.wait_for_block(spec, KILL_AFTER_BLOCK, WAIT_FOR_BLOCK);
    let status = agent.kill();
    assert!(
        restart::died_of_sigkill(status),
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
/// ADR-264): with an ECUReset, or, once the RequestTransferExit marker is in the journal, by
/// waiting out the ECU's session timer without one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Teardown {
    Reset,
    Passive,
}

/// Checks the restart's requests against [`EXPECTED_RESTART`], without its ECUReset for a
/// passive teardown. TesterPresent requests are left out and consecutive F186 reads collapsed,
/// since the confirmation reads until the ECU answers; every other request must be there, once,
/// in order. The ECUReset is counted on its own too.
fn assert_restart_order(trace: &[String], teardown: Teardown) {
    let mut seen: Vec<&str> = Vec::new();
    for line in trace.iter().map(String::as_str) {
        if line.starts_with("uds 3E") || (line == "uds 22 F186" && seen.last() == Some(&line)) {
            continue;
        }
        seen.push(line);
    }
    let resets = usize::from(teardown == Teardown::Reset);
    assert_eq!(
        trace.iter().filter(|line| *line == "uds 11 01").count(),
        resets,
        "{resets} ECUReset: {trace:#?}"
    );
    let expected: Vec<&str> = EXPECTED_RESTART
        .iter()
        .copied()
        .filter(|line| teardown == Teardown::Reset || *line != "uds 11 01")
        .collect();
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
    // restart reset it once, or not at all after a passive teardown. The session is checked
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
            power_cycles: power_cycles_before + u32::from(teardown == Teardown::Reset),
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
        Teardown::Reset,
        result,
    );
}

/// The agent is restarted after tS3_Server ran out: the ECU fell back to its default session and
/// interrupted the transfer while nothing was running. The restart redoes the transfer and
/// completes as well.
#[test]
#[serial]
fn an_agent_killed_mid_transfer_redoes_it_after_the_session_timer_expired() {
    const S3_MS: u32 = 3_000;
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
    // The 3 s timer can run out again while the worker stops, so either session is possible.
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::Reset,
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
    // transfer: the run ends there with the worker's loss as a failed primitive, neither a
    // result nor an on-site intervention, which would end the job instead of leaving it to the
    // restart. A kill that came after the wait would fail here, not later.
    let next_block_pc = spec
        .program
        .code
        .iter()
        .position(|op| {
            *op == Op::FlashTransfer {
                block: KILL_AFTER_BLOCK + 1,
            }
        })
        .expect("the next block's transfer") as u32;
    assert!(
        matches!(crash.result, Err(agent::JobError::Host { pc, .. }) if pc == next_block_pc),
        "the first run should end on the lost worker at {next_block_pc}: {:?}",
        crash.result
    );
    // The surviving job keeps its guards across the runs (design 8.2.5): nobody else can take
    // them while it holds them.
    assert!(
        env.guards_are_held(),
        "the failed run should keep its guards"
    );
    assert!(
        crash.link_unconfirmed,
        "a link on a dead worker cannot be confirmed closed"
    );
    // The journal the restart reads, after the failed run ended, agrees with the one at the
    // kill: block 4 the last, no RequestTransferExit, no resume yet.
    for journal in [
        crash.journal,
        agent::journal::Journal::read(&env.journal_dir(), &spec.key).expect("journal"),
    ] {
        let transfer = journal.facts.transfer.expect("a transfer attempt");
        assert_eq!(transfer.started_at.pc, ERASE_PC);
        assert_eq!(transfer.last_block, Some(KILL_AFTER_BLOCK));
        assert!(transfer.exit.is_none(), "no RequestTransferExit yet");
        assert!(journal.facts.resume_counts.is_empty());
    }
    let first_attempt = agent::journal::Journal::read(&env.journal_dir(), &spec.key)
        .expect("journal")
        .facts
        .transfer
        .expect("a transfer attempt")
        .started_at;
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
        Teardown::Reset,
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

/// What the journal and the ECU hold when the agent was killed after RequestTransferExit: the
/// exit marker, every block, no completed post-transfer steps and no resume yet, with the ECU
/// in its programming session and the download closed but not verified. Returns where the
/// first attempt's transfer started.
fn assert_killed_after_exit(env: &Env, spec: &JobSpec, response_recorded: bool) -> StepRef {
    let journal = agent::journal::Journal::read(&env.journal_dir(), &spec.key).expect("journal");
    assert!(journal.facts.resume_counts.is_empty());
    let transfer = journal.facts.transfer.expect("a transfer attempt");
    assert_eq!(transfer.started_at.pc, ERASE_PC);
    assert_eq!(transfer.last_block, Some(BLOCKS));
    let exit = transfer.exit.expect("the RequestTransferExit marker");
    assert_eq!(exit.intent_at.pc, exit_pc(spec));
    assert!(!exit.complete, "the post-transfer steps did not complete");
    assert_eq!(
        exit.last_post_step.map(|step| step.pc),
        response_recorded.then(|| exit_pc(spec)),
        "the RequestTransferExit's response should be recorded: {response_recorded}"
    );
    let probe = env.probe();
    assert_eq!(
        (probe.session, probe.flash_phase),
        (2, 3),
        "the ECU should be in its programming session with the download closed, not verified: \
         {probe:?}"
    );
    transfer.started_at
}

/// The agent is killed after it sent RequestTransferExit and before the response reached the
/// journal: the ECU answers it with four response-pending messages 200 ms apart, which keeps
/// the agent waiting while the ECU has already closed the download. The ECU's session timer
/// restarts when its final response goes out, so the sequence is kept shorter than the
/// restart's passive wait. The restart cannot tell
/// whether the download closed. With the RequestTransferExit marker journaled it sends no
/// ECUReset: it waits out the ECU's session timer (the plan declares the ECU's 3 s), confirms
/// the default session, and redoes the transfer, so RequestTransferExit and the integrity check
/// run again, in order.
#[test]
#[serial]
fn a_kill_before_the_transfer_exit_response_was_recorded_redoes_the_transfer() {
    let spec = spec();
    let env = Env::new("exit-unanswered", ENTRY, &ecu_config(3_000));
    let agent = env.spawn_agent();
    // The program waits after the last block, so the fault is armed before the
    // RequestTransferExit goes out, and nothing else is sent in between to take it.
    env.wait_for_block(&spec, BLOCKS, WAIT_FOR_BLOCK);
    env.inject(
        r#"{"command": "inject_fault", "fault": {"response_pending": {"count": 4, "interval_ms": 200}}}"#,
    );
    env.wait_for(&spec, "RequestTransferExit sent", WAIT_FOR_BLOCK, |state| {
        state
            .facts
            .transfer
            .as_ref()
            .and_then(|transfer| transfer.exit.as_ref())
            .is_some()
            && env.trace().iter().any(|line| line == "uds 37")
    });
    // The trace line is written as the request is sent, just before the ECU handles it; the
    // kill waits until the ECU has (closed the download and armed its response-pending
    // sequence), well within that sequence's 800 ms.
    std::thread::sleep(Duration::from_millis(200));
    let status = agent.kill();
    assert!(
        restart::died_of_sigkill(status),
        "the agent should be killed while it waits for the response: {status}"
    );
    assert!(env.commands_applied(), "the fault should have been applied");
    let first_attempt = assert_killed_after_exit(&env, &spec, false);
    let before = env.probe();
    let result = env.restart(&spec);
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::Passive,
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
    let env = Env::new("post-transfer", ENTRY, &ecu_config(3_000));
    let agent = env.spawn_agent();
    // The program waits after the RequestTransferExit, before the integrity check.
    env.wait_for(
        &spec,
        "RequestTransferExit answered",
        WAIT_FOR_BLOCK,
        |state| {
            state
                .facts
                .transfer
                .as_ref()
                .and_then(|transfer| transfer.exit.as_ref())
                .is_some_and(|exit| exit.last_post_step.is_some())
        },
    );
    let status = agent.kill();
    assert!(
        restart::died_of_sigkill(status),
        "the agent should be killed during the program's wait: {status}"
    );
    let first_attempt = assert_killed_after_exit(&env, &spec, true);
    let before = env.probe();
    let result = env.restart(&spec);
    assert_completed(
        &env,
        &spec,
        first_attempt,
        before.power_cycles,
        &[1, 2],
        Teardown::Passive,
        result,
    );
}
