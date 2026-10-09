//! Restart classification (design 5.6 `Interrupted -> Writing`, 8.2.5, 8.10.1; ADR-229 items 1
//! and 2 step 1, ADR-245 items 3, 5 and 7, ADR-253).
//!
//! [`classify`] reads what a job's write-job journal says about the job's last run, together
//! with the program, and decides how the job may go on: a plain start, the restart order of
//! ADR-229 item 2 for an interrupted transfer, or on-site intervention. It contacts nothing; the
//! restart itself acts on its answer. `check_before_ecu` makes the restart's checks that need
//! no ECU service (the resume limit and the supply voltage) and counts the resume (ADR-255).
//! `check_gates` then makes the identity and safety gates of ADR-229 item 2 step 2 (ADR-261):
//! the VIN, the hardware identity and the declared preconditions, which decide whether the
//! teardown may use an ECUReset or must be passive. It sends only ReadDataByIdentifier requests
//! through the declared sources.
//!
//! The interruption point is the latest of the last completed step and the requests the
//! journal wrote ahead (the transfer-start and RequestTransferExit markers, and the request
//! intent of ADR-253), by step count, so a crash after a guarded request was sent but before its
//! response was recorded is placed at that request.

use std::sync::atomic::{AtomicBool, Ordering};

use diag_ir::{
    DiagHost, IdentityKind, Interruptible, Precondition, PreconditionKind, Program,
    RecoveryRequired, RuntimeInput, Source, Vm, VmError, VmState,
};

use crate::host::HostError;
use crate::inputs::{FieldBytes, Reading, RuntimeInputs, ServiceSources};
use crate::inputs::{read_field_bytes, resolve_source};
use crate::journal::{Journal, JournalError, JournalState, RecoveryFacts, StageId, StepRef, Store};
use crate::runner::JobError;

/// How a job whose journal exists goes on.
#[derive(Debug, Clone, PartialEq)]
pub enum RestartDecision {
    /// No transfer was started and nothing rules a restart out: the program runs from its start.
    PlainStart,
    /// A transfer was started: it is redone in the restart order of ADR-229 item 2.
    Restart(Box<RestartPoint>),
    /// The job must not go on automatically (design 5.6, 8.10.1).
    OnSiteInterventionRequired(OnSiteReason),
}

/// What a restart starts from.
#[derive(Debug, Clone, PartialEq)]
pub struct RestartPoint {
    /// The plan whose transfer was started (`FlashRecovery::flash_session`).
    pub flash_session: u32,
    /// The interruption point, if the journal records one.
    pub interrupted_at: Option<StepRef>,
    /// The VM state at the plan's entry, checked against the program, which the restart's
    /// replay starts from. Its step count continues after the journal's last record
    /// ([`next_steps`]), since the journal orders records by it and refuses one that does not
    /// come after the last.
    pub entry_state: VmState,
    /// The journal's facts, which the later restart steps compare against.
    pub facts: RecoveryFacts,
}

/// Why a job needs on-site intervention.
#[derive(Debug, Clone, PartialEq)]
pub enum OnSiteReason {
    /// The journal cannot be read back: corrupt, of an unknown format version, of another job,
    /// or unreadable (ADR-244 item 5).
    UnreadableJournal(String),
    /// A program with a flash recovery plan has no journal. The runner creates one before it
    /// sends anything, so a missing one may have been lost after an erase.
    MissingJournal,
    /// The interruption point is at or past the plan's recovery-required point and before the
    /// plan's end (ADR-245 item 3).
    RecoveryRequiredPoint { flash_session: u32, at: StepRef },
    /// The interruption point lies in a section marked `RecoveryRequired` (8.10.1).
    RecoveryRequiredSection { section: usize, at: StepRef },
    /// The transfer's stage names no plan of this program.
    UnknownStage(u32),
    /// The journal has no VM state for the plan's entry, or the one it has is not the state
    /// at that entry.
    MissingEntryState { flash_session: u32 },
    /// The VM state at the entry does not decode, or fails `Vm::check_state` (ADR-245 item 7).
    InvalidEntryState(String),
    /// The transfer's plan never allows a restart (`FlashRecovery::allows_restart`), so the
    /// program need not declare what a restart reads (ADR-245 item 6).
    RestartNotAllowed { flash_session: u32 },
    /// The stage was resumed as often as its plan allows (design 8.2.5, ADR-229 item 2 step 1).
    ResumeLimitReached {
        flash_session: u32,
        resumes: u16,
        max: u16,
    },
    /// The supply voltage the VCI reports is outside the range the program declares, or the VCI
    /// gives none (`None`): no reading, an undecodable one, or a failure to ask the worker.
    SupplyVoltage {
        flash_session: u32,
        millivolts: Option<i64>,
    },
    /// The restart passed step 1 (the checks that need no ECU service, and its resume was
    /// counted) and the gates of step 2, which gave `teardown`. The teardown and the rest of
    /// ADR-229's restart order (the ECU state check, the replay to the erase) do not run in this
    /// agent, so the job stops before it sends anything that changes the ECU (ADR-255, ADR-261).
    RestartOrderUnavailable {
        flash_session: u32,
        teardown: TeardownGate,
    },
}

/// What the gates of ADR-229 item 2 step 2 allow the restart's teardown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeardownGate {
    /// The VIN and the hardware identity match and every declared precondition holds: the
    /// teardown may end the download with an ECUReset.
    ResetAllowed,
    /// A gate did not pass: the teardown is passive (the agent stops TesterPresent and waits
    /// out the session timeout), with no ECUReset.
    PassiveOnly(PassiveReason),
}

/// The first gate that did not pass. No VIN is kept in it (design 16.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassiveReason {
    /// The ECU's VIN could not be compared with the job's: the job names none, the program
    /// declares no source, or the read gave no answer, a negative response, or a value that is
    /// not text, or the worker failed.
    VinNotEstablished,
    /// The hardware identity could not be compared: no source is declared, the journal holds
    /// none, or the read gave no answer, a negative response, an unreadable field, or the
    /// worker failed.
    HardwareIdentityNotEstablished,
    /// The ECU's hardware identity differs from the one the journal recorded.
    HardwareIdentityDiffers,
    /// The precondition failed or could not be established, including a source the agent cannot
    /// resolve.
    Precondition(PreconditionKind),
}

/// Decides how the job of `program` goes on, from its journal as `Journal::read` (or
/// `Journal::open`) gave it. No journal at all (`JournalError::NotFound`) is a plain start only
/// for a program without a flash recovery plan, which keeps none. For a program with one it is
/// on-site intervention: the runner creates the journal before it sends anything, so a missing
/// one cannot be told from one lost after an erase.
pub fn classify(
    program: &Program,
    journal: Result<&JournalState, &JournalError>,
) -> RestartDecision {
    let state = match journal {
        Ok(state) => state,
        Err(JournalError::NotFound) if program.flash.is_empty() => {
            return RestartDecision::PlainStart;
        }
        Err(JournalError::NotFound) => {
            return RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingJournal);
        }
        Err(error) => {
            return RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnreadableJournal(
                error.to_string(),
            ));
        }
    };
    let facts = &state.facts;
    let interrupted_at = interruption_point(facts);
    if let Some(at) = interrupted_at
        && let Some(reason) = recovery_required(program, effective_point(program, facts, at))
    {
        return RestartDecision::OnSiteInterventionRequired(reason);
    }
    let Some(transfer) = &facts.transfer else {
        return RestartDecision::PlainStart;
    };
    let Some(plan) = program
        .flash
        .iter()
        .find(|plan| plan.stage == transfer.stage.0)
    else {
        return RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnknownStage(
            transfer.stage.0,
        ));
    };
    if !plan.allows_restart() {
        return RestartDecision::OnSiteInterventionRequired(OnSiteReason::RestartNotAllowed {
            flash_session: plan.flash_session,
        });
    }
    let entry_state = match entry_state(program, state, plan.boundaries.entry_pc) {
        Ok(entry_state) => entry_state,
        Err(reason) => {
            return RestartDecision::OnSiteInterventionRequired(match reason {
                EntryStateError::Missing => OnSiteReason::MissingEntryState {
                    flash_session: plan.flash_session,
                },
                EntryStateError::Invalid(why) => OnSiteReason::InvalidEntryState(why),
            });
        }
    };
    // The restart's records come after the journal's last one; the counter must stay usable.
    let mut entry_state = entry_state;
    entry_state.steps = next_steps(state);
    if let Err(error) = Vm::resume(entry_state.clone()).check_state(program) {
        return RestartDecision::OnSiteInterventionRequired(OnSiteReason::InvalidEntryState(
            error.to_string(),
        ));
    }
    RestartDecision::Restart(Box::new(RestartPoint {
        flash_session: plan.flash_session,
        interrupted_at,
        entry_state,
        facts: facts.clone(),
    }))
}

/// The checks of ADR-229 item 2 step 1 that need no ECU service, then the resume count, for an
/// agent without a server (ADR-255). In this order:
/// - the resume limit: a stage resumed `max_resumes` times needs on-site intervention;
/// - the supply voltage, read through the VCI (`RuntimeInput::SupplyVoltageMillivolts`), when
///   the program declares a voltage range: a reading outside it, or no reading, needs on-site
///   intervention. A program that declares none is not checked here;
/// - the resume count, incremented and committed to `journal` with no attempt key, since a
///   standalone agent reserves nothing on a server.
///
/// A failed check counts no resume. A cancel stops it before the voltage read, right after it
/// (whatever it gave) and before the commit. Returns the new count. Nothing here contacts the ECU.
pub(crate) fn check_before_ecu<H, S>(
    program: &Program,
    point: &RestartPoint,
    journal: &mut Journal<S>,
    host: &mut H,
    cancelled: &AtomicBool,
) -> Result<u16, JobError>
where
    H: RuntimeInputs,
    S: Store,
{
    let flash_session = point.flash_session;
    let plan = program
        .flash
        .iter()
        .find(|plan| plan.flash_session == flash_session)
        // `classify` took the point from one of the program's plans.
        .ok_or(JobError::Journal(JournalError::Invariant(
            "the restart point names no plan of the program",
        )))?;
    let stage = StageId(plan.stage);
    let resumes = journal.state().facts.resume_count(stage);
    if resumes >= plan.max_resumes {
        return Err(JobError::OnSiteInterventionRequired(
            OnSiteReason::ResumeLimitReached {
                flash_session,
                resumes,
                max: plan.max_resumes,
            },
        ));
    }
    if let Some(range) = program
        .preconditions
        .voltage_mv
        .as_ref()
        .map(|p| p.satisfied)
    {
        if cancelled.load(Ordering::Relaxed) {
            return Err(JobError::Cancelled);
        }
        let reading = host.read(RuntimeInput::SupplyVoltageMillivolts);
        // A cancel during the read is a cancel, whatever the read gave.
        if cancelled.load(Ordering::Relaxed) {
            return Err(JobError::Cancelled);
        }
        let millivolts = match reading {
            Ok(Reading::Value(millivolts)) => Some(millivolts),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(%error, "the supply voltage could not be read");
                None
            }
        };
        if !millivolts.is_some_and(|mv| (range.lower..=range.upper).contains(&mv)) {
            return Err(JobError::OnSiteInterventionRequired(
                OnSiteReason::SupplyVoltage {
                    flash_session,
                    millivolts,
                },
            ));
        }
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    Ok(journal.commit_resume(stage, None)?)
}

/// Runs `read` with a cancel check before and after it, as `check_before_ecu` does: a cancel
/// during the read is a cancel, whatever the read gave.
fn cancellable<T>(cancelled: &AtomicBool, read: impl FnOnce() -> T) -> Result<T, JobError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    let result = read();
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    Ok(result)
}

/// The identity and safety gates of ADR-229 item 2 step 2 (ADR-261), which decide whether the
/// restart's teardown may end the download with an ECUReset. In this order, stopping at the
/// first that does not pass:
/// - the VIN: the ECU's, read through the program's source, must equal `vin`, the VIN the job
///   targets. A job that names none, a program that declares no source, and a read that gives
///   no text (no answer, a negative response, an undecodable field, a worker failure) leave the
///   vehicle unidentified: [`PassiveReason::VinNotEstablished`], and nothing is read for a job
///   with no VIN. A VIN that decodes but differs aborts the job
///   ([`JobError::IdentityMismatch`]), since the ECU is another vehicle's;
/// - the hardware identity: the ECU's, read as raw field bytes, must equal the journal's, which
///   was recorded before the erase;
/// - each declared precondition, in the order voltage, external supply, ignition, engine,
///   vehicle speed: its value must lie in the declared range. The ECU's session is not known
///   yet (it may still be in its programming session), so the default-session source is read
///   first and the programming-session source only when that gives no value. A value outside
///   the range fails at once.
///
/// A cancel stops it before and right after every read. Nothing here sends anything but
/// ReadDataByIdentifier requests through the declared sources, and reads of runtime inputs. No
/// VIN is put in a log message or a result.
pub(crate) fn check_gates<H>(
    program: &Program,
    point: &RestartPoint,
    sources: &ServiceSources,
    vin: Option<&str>,
    host: &mut H,
    cancelled: &AtomicBool,
) -> Result<TeardownGate, JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs,
{
    let passive = |reason| Ok(TeardownGate::PassiveOnly(reason));

    // The VIN.
    let (Some(target), Some(source)) = (vin, program.identity.vin) else {
        return passive(PassiveReason::VinNotEstablished);
    };
    match cancellable(cancelled, || resolve_source(source, sources, host))? {
        Ok(Reading::Text(text)) if text == target => {}
        Ok(Reading::Text(_)) => {
            return Err(JobError::IdentityMismatch {
                identity: IdentityKind::Vin,
            });
        }
        Ok(_) => return passive(PassiveReason::VinNotEstablished),
        Err(error) => {
            tracing::warn!(%error, "the ECU's VIN could not be read");
            return passive(PassiveReason::VinNotEstablished);
        }
    }

    // The hardware identity.
    let (Some(recorded), Some(source)) = (
        point.facts.ecu_hardware_part_number.as_deref(),
        program.identity.hardware_part_number,
    ) else {
        return passive(PassiveReason::HardwareIdentityNotEstablished);
    };
    match cancellable(cancelled, || read_field_bytes(source, sources, host))? {
        Ok(FieldBytes::Field(bytes)) if bytes == recorded => {}
        Ok(FieldBytes::Field(_)) => return passive(PassiveReason::HardwareIdentityDiffers),
        Ok(_) => return passive(PassiveReason::HardwareIdentityNotEstablished),
        Err(error) => {
            tracing::warn!(%error, "the ECU's hardware identity could not be read");
            return passive(PassiveReason::HardwareIdentityNotEstablished);
        }
    }

    // The safety preconditions.
    let declared = &program.preconditions;
    for (kind, precondition) in [
        (PreconditionKind::Voltage, &declared.voltage_mv),
        (PreconditionKind::ExternalSupply, &declared.external_supply),
        (PreconditionKind::Ignition, &declared.ignition),
        (PreconditionKind::Engine, &declared.engine),
        (PreconditionKind::VehicleSpeed, &declared.vehicle_speed),
    ] {
        let Some(precondition) = precondition else {
            continue;
        };
        if !precondition_holds(precondition, kind, sources, host, cancelled)? {
            return passive(PassiveReason::Precondition(kind));
        }
    }
    Ok(TeardownGate::ResetAllowed)
}

/// Whether `precondition` holds: the first source that gives a value decides, the
/// default-session one before the programming-session one (skipped when it is the same source).
fn precondition_holds<H>(
    precondition: &Precondition,
    kind: PreconditionKind,
    sources: &ServiceSources,
    host: &mut H,
    cancelled: &AtomicBool,
) -> Result<bool, JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs,
{
    let range = precondition.satisfied.lower..=precondition.satisfied.upper;
    let mut previous: Option<Source> = None;
    for source in [
        precondition.default_session,
        precondition.programming_session,
    ]
    .into_iter()
    .flatten()
    {
        if previous == Some(source) {
            continue;
        }
        previous = Some(source);
        match cancellable(cancelled, || resolve_source(source, sources, host))? {
            Ok(Reading::Value(value)) => return Ok(range.contains(&value)),
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, ?kind, "a safety precondition could not be read");
            }
        }
    }
    Ok(false)
}

/// The step count a run that goes on with `state`'s journal starts from: one more than any
/// record's, so its first record comes after the last one (ADR-244 item 4), whether it is a
/// restart or a plain start on an existing journal. `StepRef::steps` keeps counting across
/// resumes.
pub fn next_steps(state: &JournalState) -> u64 {
    let facts = &state.facts;
    let transfer = facts.transfer.as_ref();
    let exit = transfer.and_then(|t| t.exit.as_ref());
    [
        facts.last_step,
        transfer.map(|t| t.started_at),
        exit.map(|e| e.intent_at),
        exit.and_then(|e| e.last_post_step),
        facts.last_intent,
        state.last_vm_state.as_ref().map(|(at, _)| *at),
    ]
    .into_iter()
    .flatten()
    .map(|at| at.steps.saturating_add(1))
    .max()
    .unwrap_or(0)
}

/// The latest of the last completed step and the requests written ahead, by step count. A
/// marker names the request it guards, which may have been sent; a completed step names a
/// request that was.
pub fn interruption_point(facts: &RecoveryFacts) -> Option<StepRef> {
    let transfer = facts.transfer.as_ref();
    [
        facts.last_step,
        transfer.map(|t| t.started_at),
        transfer.and_then(|t| t.exit.as_ref()).map(|e| e.intent_at),
        facts.last_intent,
    ]
    .into_iter()
    .flatten()
    .max_by_key(|at| at.steps)
}

/// Where the interruption at `at` actually is. The journal records no steps after a plan, so a
/// plan that ran to its end leaves its last step inside the plan; its post-transfer completion,
/// committed with the step that reached the end, says execution got past it. A point no later
/// than that step (the completed transfer's last post-transfer step, which the journal stops
/// updating at the completion) is at the plan's end. A later point, such as a step back into the
/// plan's entry, stands as it is. A resume record changes no step count, so it does not undo
/// this.
fn effective_point(program: &Program, facts: &RecoveryFacts, at: StepRef) -> StepRef {
    let Some(transfer) = &facts.transfer else {
        return at;
    };
    let Some(exit) = transfer.exit.as_ref().filter(|exit| exit.complete) else {
        return at;
    };
    let last = exit.last_post_step.unwrap_or(exit.intent_at);
    match program
        .flash
        .iter()
        .find(|plan| plan.stage == transfer.stage.0)
    {
        Some(plan) if at.steps <= last.steps => StepRef {
            pc: plan.boundaries.post_transfer_end_pc,
            steps: at.steps,
        },
        _ => at,
    }
}

/// Why an interruption at `at` rules out a restart, if it does: at or past a plan's
/// recovery-required point and before its end, or inside a section marked `RecoveryRequired`.
/// Sections are found only where the journal can place a point: inside a plan, on the step into
/// a plan's entry, or at a completed plan's end.
fn recovery_required(program: &Program, at: StepRef) -> Option<OnSiteReason> {
    for plan in &program.flash {
        if let RecoveryRequired::FromPc(from) = plan.recovery_required
            && (from..plan.boundaries.post_transfer_end_pc).contains(&at.pc)
        {
            return Some(OnSiteReason::RecoveryRequiredPoint {
                flash_session: plan.flash_session,
                at,
            });
        }
    }
    program
        .sections
        .iter()
        .position(|section| {
            matches!(section.interruptible, Interruptible::RecoveryRequired)
                && (section.start_pc..section.end_pc).contains(&at.pc)
        })
        .map(|section| OnSiteReason::RecoveryRequiredSection { section, at })
}

enum EntryStateError {
    Missing,
    Invalid(String),
}

/// The VM state at `entry_pc`: the newest state the journal holds, which the runner takes on the
/// step into a plan's entry (ADR-252), or the program's initial state when the job started at
/// the entry and so recorded none. Either is checked with `Vm::check_state` before it is used.
fn entry_state(
    program: &Program,
    state: &JournalState,
    entry_pc: u32,
) -> Result<VmState, EntryStateError> {
    let restored = match &state.last_vm_state {
        Some((_, bytes)) => postcard::from_bytes::<VmState>(bytes).map_err(|error| {
            EntryStateError::Invalid(format!("the VM state does not decode: {error}"))
        })?,
        None if entry_pc == 0 => Vm::new(program).state,
        None => return Err(EntryStateError::Missing),
    };
    let vm = Vm::resume(restored);
    vm.check_state(program)
        .map_err(|error: VmError| EntryStateError::Invalid(error.to_string()))?;
    if vm.state.pc != entry_pc {
        return Err(EntryStateError::Missing);
    }
    Ok(vm.state)
}

#[cfg(test)]
mod tests {
    use diag_ir::{
        FlashRecovery, IR_SCHEMA_VERSION, Idempotency, Op, RecoveryBoundaries, RecoveryTiming,
        Section,
    };

    use super::*;
    use crate::journal::{JobKey, Journal, StageId, Store};

    struct Memory;

    impl Store for Memory {
        fn append_sync(&mut self, _: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
    }

    const ENTRY: u32 = 2;
    const ERASE: u32 = 3;
    const EXIT: u32 = 6;
    const CHECK: u32 = 7;
    const END: u32 = 8;

    /// Two steps before the plan, then entry 2, erase 3, RequestDownload 4, a block 5,
    /// RequestTransferExit 6, a post-transfer check 7, end 8.
    fn program(recovery_required: RecoveryRequired) -> Program {
        let mut code = vec![Op::Pop; 10];
        code[ERASE as usize] = Op::RoutineControl { routine: 1, sub: 1 };
        code[4] = Op::ServiceRequest { service: 0x34 };
        code[5] = Op::FlashTransfer { block: 1 };
        code[EXIT as usize] = Op::ServiceRequest { service: 0x37 };
        code[CHECK as usize] = Op::RoutineControl { routine: 2, sub: 1 };
        Program {
            schema_version: IR_SCHEMA_VERSION,
            code,
            constants: Vec::new(),
            sections: Vec::new(),
            source_map: Vec::new(),
            identity: Default::default(),
            preconditions: Default::default(),
            flash: vec![FlashRecovery {
                flash_session: 1,
                stage: 7,
                max_resumes: 3,
                recovery_required,
                boundaries: RecoveryBoundaries {
                    entry_pc: ENTRY,
                    erase_pc: ERASE,
                    transfer_exit_pc: EXIT,
                    post_transfer_end_pc: END,
                },
                timing: RecoveryTiming {
                    session_timeout_millis: 5000,
                    teardown_margin_millis: 0,
                    ecu_startup_millis: 0,
                    confirmation_window_millis: 0,
                },
                version_read_retries: 0,
                no_application: None,
            }],
        }
    }

    fn at(pc: u32) -> StepRef {
        // One step per instruction.
        StepRef {
            pc,
            steps: u64::from(pc),
        }
    }

    fn entry_state(program: &Program, schema_version: u32) -> Vec<u8> {
        let mut state = Vm::new(program).state;
        state.pc = ENTRY;
        state.steps = u64::from(ENTRY);
        state.schema_version = schema_version;
        postcard::to_allocvec(&state).unwrap()
    }

    /// A journal of a run that reached `upto` (the instruction it was about to run, or ran into
    /// a lost response on), with the runner's records up to there.
    fn journal_until(program: &Program, upto: u32) -> Journal<Memory> {
        let mut j = Journal::on_store(
            Memory,
            JobKey {
                job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e6f".to_owned()),
                generation: 1,
            },
        );
        j.commit_step(
            at(ENTRY - 1),
            Some(&entry_state(program, IR_SCHEMA_VERSION)),
        )
        .unwrap();
        for pc in ENTRY..upto {
            if pc == ERASE {
                j.commit_transfer_start(StageId(7), at(pc)).unwrap();
            }
            if pc == EXIT {
                j.commit_transfer_exit_intent(at(pc)).unwrap();
            }
            if pc >= ENTRY {
                j.commit_step(at(pc), None).unwrap();
            }
            if pc == 5 {
                j.commit_block(1).unwrap();
            }
        }
        // The request at `upto` was sent; its response was lost.
        if upto == ERASE {
            j.commit_transfer_start(StageId(7), at(upto)).unwrap();
        }
        if upto == EXIT {
            j.commit_transfer_exit_intent(at(upto)).unwrap();
        }
        j
    }

    fn decide(program: &Program, j: &Journal<Memory>) -> RestartDecision {
        classify(program, Ok(j.state()))
    }

    /// The done-when case: the transfer-start marker alone makes a lost erase response an
    /// interrupted transfer.
    #[test]
    fn a_lost_erase_response_is_an_interrupted_transfer() {
        let program = program(RecoveryRequired::Never);
        let j = journal_until(&program, ERASE);
        let RestartDecision::Restart(point) = decide(&program, &j) else {
            panic!("{:?}", decide(&program, &j));
        };
        assert_eq!(point.flash_session, 1);
        assert_eq!(point.interrupted_at, Some(at(ERASE)));
        assert_eq!(point.entry_state.pc, ENTRY);
        // The step count goes on after the journal's last record, the marker at the erase.
        assert_eq!(point.entry_state.steps, u64::from(ERASE) + 1);
        assert_eq!(next_steps(j.state()), u64::from(ERASE) + 1);
    }

    /// The point found by review: a plan with a recovery-required point that ran to its end
    /// leaves its last step on its last primitive, past the point; the completion record says
    /// execution got beyond it, so a later crash is not refused for the point.
    #[test]
    fn a_completed_plan_does_not_need_on_site_intervention_for_its_point() {
        let program = program(RecoveryRequired::FromPc(CHECK));
        // The run reached the end: its last step is the check, past the point.
        let mut j = journal_until(&program, END);
        assert!(matches!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::RecoveryRequiredPoint { .. })
        ));
        j.commit_post_transfer_complete().unwrap();
        assert!(matches!(decide(&program, &j), RestartDecision::Restart(_)));
    }

    #[test]
    fn a_job_with_no_transfer_starts_plainly() {
        let program = program(RecoveryRequired::Never);
        assert_eq!(
            decide(&program, &journal_until(&program, ENTRY)),
            RestartDecision::PlainStart
        );
        // No journal: a plain start only for a program without a plan, which keeps none.
        assert_eq!(
            classify(&program, Err(&JournalError::NotFound)),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingJournal)
        );
        let mut no_plan = program.clone();
        no_plan.flash.clear();
        assert_eq!(
            classify(&no_plan, Err(&JournalError::NotFound)),
            RestartDecision::PlainStart
        );
    }

    /// The done-when case: a lost response to the request at the recovery-required point ends in
    /// on-site intervention, whether the point is RequestTransferExit (its marker) or another
    /// request (its intent, ADR-253); one step earlier, the restart goes ahead.
    #[test]
    fn a_lost_response_at_the_recovery_point_needs_on_site_intervention() {
        let program = program(RecoveryRequired::FromPc(EXIT));
        assert!(matches!(
            decide(&program, &journal_until(&program, EXIT)),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::RecoveryRequiredPoint {
                flash_session: 1,
                at
            }) if at == super::tests::at(EXIT)
        ));
        assert!(matches!(
            decide(&program, &journal_until(&program, EXIT - 1)),
            RestartDecision::Restart(_)
        ));

        let program = super::tests::program(RecoveryRequired::FromPc(CHECK));
        let mut j = journal_until(&program, CHECK);
        assert!(matches!(decide(&program, &j), RestartDecision::Restart(_)));
        j.commit_intent(at(CHECK)).unwrap();
        assert!(matches!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::RecoveryRequiredPoint { .. })
        ));
    }

    #[test]
    fn a_point_in_a_recovery_required_section_needs_on_site_intervention() {
        // The section ends at the plan's entry; the journal places the point on the step into
        // the entry (pc 1), the only place before a plan it records.
        let mut program = program(RecoveryRequired::Never);
        program.sections.push(Section {
            start_pc: 0,
            end_pc: 2,
            interruptible: Interruptible::RecoveryRequired,
            idempotency: Idempotency::Safe,
            expected_millis: 0,
        });
        let j = journal_until(&program, ENTRY);
        assert!(matches!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::RecoveryRequiredSection {
                section: 0,
                ..
            })
        ));
    }

    /// The done-when case: an unreadable journal ends in on-site intervention.
    #[test]
    fn an_unreadable_journal_needs_on_site_intervention() {
        let program = program(RecoveryRequired::Never);
        for error in [
            JournalError::Corrupt {
                offset: 12,
                reason: "checksum",
            },
            JournalError::UnsupportedVersion(9),
            JournalError::WrongJob,
        ] {
            assert!(
                matches!(
                    classify(&program, Err(&error)),
                    RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnreadableJournal(_))
                ),
                "{error:?}"
            );
        }
    }

    /// The done-when case: a version 1 VM state still decodes but fails `Vm::check_state`
    /// (ADR-245 item 7).
    #[test]
    fn an_entry_state_that_fails_its_check_needs_on_site_intervention() {
        let program = program(RecoveryRequired::Never);
        let mut j = Journal::on_store(
            Memory,
            JobKey {
                job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e6f".to_owned()),
                generation: 1,
            },
        );
        j.commit_step(at(ENTRY - 1), Some(&entry_state(&program, 1)))
            .unwrap();
        j.commit_transfer_start(StageId(7), at(ERASE)).unwrap();
        assert!(matches!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::InvalidEntryState(_))
        ));

        let mut j = Journal::on_store(
            Memory,
            JobKey {
                job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e6f".to_owned()),
                generation: 1,
            },
        );
        j.commit_step(at(ENTRY - 1), Some(b"not a state")).unwrap();
        j.commit_transfer_start(StageId(7), at(ERASE)).unwrap();
        assert!(matches!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::InvalidEntryState(_))
        ));
    }

    #[test]
    fn a_transfer_without_the_entry_state_needs_on_site_intervention() {
        let program = program(RecoveryRequired::Never);
        let mut j = Journal::on_store(
            Memory,
            JobKey {
                job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e6f".to_owned()),
                generation: 1,
            },
        );
        j.commit_transfer_start(StageId(7), at(ERASE)).unwrap();
        assert_eq!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingEntryState {
                flash_session: 1
            })
        );
        // A transfer of a stage the program does not declare.
        let mut other = program.clone();
        other.flash[0].stage = 8;
        let j = journal_until(&program, ERASE);
        assert_eq!(
            decide(&other, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnknownStage(7))
        );
    }

    /// A resume record after the completion does not undo it: the restart that committed the
    /// resume and crashed before any new record is still past the plan's end.
    #[test]
    fn a_resume_after_the_completion_keeps_the_plan_left() {
        let program = program(RecoveryRequired::FromPc(CHECK));
        let mut j = journal_until(&program, END);
        j.commit_post_transfer_complete().unwrap();
        j.commit_resume(StageId(7), None).unwrap();
        assert!(matches!(decide(&program, &j), RestartDecision::Restart(_)));
    }

    /// Coming back into the plan after it completed puts the point inside it again.
    #[test]
    fn a_step_back_into_a_completed_plan_counts_again() {
        let mut program = program(RecoveryRequired::FromPc(ENTRY));
        program.code[9] = Op::Jump(ENTRY);
        let mut j = journal_until(&program, END);
        j.commit_post_transfer_complete().unwrap();
        // The jump at 9 into the entry, then the entry's own primitive.
        j.commit_step(
            StepRef { pc: 9, steps: 9 },
            Some(&entry_state(&program, IR_SCHEMA_VERSION)),
        )
        .unwrap();
        j.commit_step(
            StepRef {
                pc: ENTRY,
                steps: 10,
            },
            None,
        )
        .unwrap();
        assert!(matches!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::RecoveryRequiredPoint { .. })
        ));
    }

    /// A `RecoveryRequired` section over a completed plan's tail does not catch a crash after
    /// the plan's end.
    #[test]
    fn a_section_over_a_completed_plans_tail_does_not_refuse_the_restart() {
        let mut program = program(RecoveryRequired::FromPc(CHECK));
        program.sections.push(Section {
            start_pc: CHECK,
            end_pc: END,
            interruptible: Interruptible::RecoveryRequired,
            idempotency: Idempotency::Safe,
            expected_millis: 0,
        });
        let mut j = journal_until(&program, END);
        assert!(matches!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(_)
        ));
        j.commit_post_transfer_complete().unwrap();
        assert!(matches!(decide(&program, &j), RestartDecision::Restart(_)));
    }

    /// A plan that never allows a restart gets none, even past its end.
    #[test]
    fn a_plan_that_never_restarts_needs_on_site_intervention() {
        let program = program(RecoveryRequired::FromPc(ERASE));
        let mut j = journal_until(&program, END);
        j.commit_post_transfer_complete().unwrap();
        assert_eq!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::RestartNotAllowed {
                flash_session: 1
            })
        );
    }

    /// A step count the journal leaves no room after is refused before anything is sent.
    #[test]
    fn an_exhausted_step_count_needs_on_site_intervention() {
        let program = program(RecoveryRequired::Never);
        let mut j = journal_until(&program, EXIT);
        j.commit_step(
            StepRef {
                pc: EXIT,
                steps: u64::MAX - 1,
            },
            None,
        )
        .unwrap();
        assert!(matches!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::InvalidEntryState(_))
        ));
    }

    /// The interruption point is the latest of the last step and the requests written ahead.
    #[test]
    fn the_interruption_point_is_the_latest_record() {
        let program = program(RecoveryRequired::Never);
        let j = journal_until(&program, EXIT);
        assert_eq!(interruption_point(&j.summary()), Some(at(EXIT)));
        let j = journal_until(&program, EXIT - 1);
        assert_eq!(interruption_point(&j.summary()), Some(at(EXIT - 2)));
    }
}
