//! What a run's end says about the job's state (design 5.6, ADR-475).
//!
//! [`outcome`] maps the run's result to the state the agent reports for the job, or to "open"
//! (the job is resumed like after a crash, so no terminal state is reported) or to nothing
//! (another run owns the job). It matches on [`JobError`] variants only, never on a worker
//! kind, an API or a vendor's code, so the agent does not branch on the worker (design
//! principle 6). Which [`JobError`] a worker's failure becomes is decided below it.
//!
//! The mapping does not send anything. The reporter that does must also emit nothing terminal
//! while the agent shuts down: a run that finds the runtime gone before its thread starts
//! ends as [`JobError::Cancelled`], which the result alone cannot tell from a user's cancel.
//!
//! A detail is [`LogSafe::log_text`] cut to [`MAX_FAILURE_DETAIL_LEN`] bytes, never a `Display`
//! or `log_external` text (ADR-348 items 6 and 8, design 16.2).

use std::path::Path;

use diag_ir::{IdentityKind, Program, VmState};
use shared_proto::{FailureCode, JobState, MAX_PROCEDURE_CODE_LEN};

use crate::journal::{JobKey, Journal, JournalError, JournalState, MAX_FAILURE_DETAIL_LEN};
use crate::oplog::LogSafe;
use crate::restart::{NotResumableCause, OnSiteReason, reached_flash_plan};
use crate::runner::{JobError, truncated};

/// Whether the ECU or the job's progress could have changed since the journal's last record
/// that a later resume would not see: the test for ending a run's interruption as a state
/// (ADR-475 item 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalStanding {
    /// No journal, or one that has not reached a flash plan and holds no request marker for a
    /// primitive that is not safe to repeat (ADR-335 item 2): a resume would lose nothing.
    Clean,
    /// Anything else, including a journal that cannot be read back: the run is resumed.
    MayHaveChanged,
}

impl JournalStanding {
    /// The standing of the journal `journal` of a job of `program`, as read after the run. A
    /// missing journal is clean; a journal that is unreadable is not. The request markers
    /// themselves are the journal's record of the primitives that were not safe to repeat
    /// (`restart::needs_request_marker` decides which get one), so the check reads them rather
    /// than the program. A marker withdrawn as provably unsent still counts: the standing errs
    /// toward a resume.
    pub fn of(program: &Program, journal: Result<&JournalState, &JournalError>) -> Self {
        match journal {
            Ok(state) => {
                if reached_flash_plan(program, &state.facts) || state.facts.first_request.is_some()
                {
                    Self::MayHaveChanged
                } else {
                    Self::Clean
                }
            }
            Err(JournalError::NotFound) => Self::Clean,
            Err(_) => Self::MayHaveChanged,
        }
    }

    /// The standing of the journal of `key` in `dir`, read without changing it (`Journal::read`
    /// takes no lock, so it works while another run holds the journal).
    pub fn read(dir: &Path, key: &JobKey, program: &Program) -> Self {
        Self::of(program, Journal::read(dir, key).as_ref())
    }
}

/// What the agent reports for a run's end.
#[derive(Debug, Clone, PartialEq)]
pub enum JobOutcome {
    /// The job's terminal state.
    Terminal(JobState),
    /// The run ended with the job not finished and its journal possibly changed: report no
    /// terminal state, and resume the job as after a crash. The resume limits end it in
    /// `NeedsOnSiteRecovery`.
    Open,
    /// Another run owns the job (ADR-255): this run reports nothing.
    None,
}

/// The outcome of a run that ended with `result`. `first_run` says the run was the job's first
/// (no earlier run of it on this agent); `standing` is the journal's after the run.
///
/// A procedure's own verdict, an identity mismatch, an on-site ending and a stop the VM or the
/// step limit made are terminal whatever the standing: the runner journals them, so a resume
/// would end the same way. Every other ending is terminal only while the standing is clean, and
/// open otherwise. A refusal or a failure before anything was sent on a first run is a
/// precondition failure; on a resume the same causes are failures.
pub fn outcome(
    result: &Result<VmState, JobError>,
    first_run: bool,
    standing: JournalStanding,
) -> JobOutcome {
    let error = match result {
        Ok(_) => return JobOutcome::Terminal(JobState::Completed),
        Err(error) => error,
    };
    let failed = |code: FailureCode| JobState::Failed {
        code,
        detail: detail(error),
        procedure_code: None,
    };
    // Terminal whatever the standing.
    match error {
        JobError::ProcedureFailed {
            code, detail: text, ..
        } => {
            return JobOutcome::Terminal(JobState::Failed {
                code: FailureCode::Procedure,
                detail: truncated(text.clone(), MAX_FAILURE_DETAIL_LEN),
                procedure_code: Some(truncated(code.clone(), MAX_PROCEDURE_CODE_LEN))
                    .filter(|code| !code.is_empty()),
            });
        }
        JobError::IdentityMismatch { identity } => {
            return JobOutcome::Terminal(failed(match identity {
                IdentityKind::Vin => FailureCode::VinMismatch,
                IdentityKind::HardwarePartNumber => FailureCode::VariantMismatch,
                // The runner does not end a run on this one today; the code claims no more
                // than the job's own expectation failed.
                IdentityKind::SoftwareVersion => FailureCode::Other,
            }));
        }
        JobError::StepLimit(_) | JobError::Vm { .. } => {
            return JobOutcome::Terminal(failed(FailureCode::ProcedureError));
        }
        JobError::OnSiteInterventionRequired(reason) => {
            return JobOutcome::Terminal(JobState::NeedsOnSiteRecovery {
                detail: truncated(on_site_text(reason), MAX_FAILURE_DETAIL_LEN),
            });
        }
        // Another run owns the journal: that run reports the job.
        JobError::Journal(JournalError::InUse | JournalError::AlreadyExists) => {
            return JobOutcome::None;
        }
        _ => {}
    }
    if standing == JournalStanding::MayHaveChanged {
        return JobOutcome::Open;
    }
    // A refusal or failure that precedes everything the run sends is a precondition failure on a
    // first run.
    let before_first_request = |code: FailureCode| {
        if first_run {
            JobState::PreconditionFailed {
                code,
                detail: detail(error),
            }
        } else {
            failed(code)
        }
    };
    JobOutcome::Terminal(match error {
        JobError::Cancelled => JobState::Cancelled,
        JobError::Program(_)
        | JobError::SeveralTargets { .. }
        | JobError::Undeclared { .. }
        | JobError::NoDeclarations
        | JobError::Sources(_)
        | JobError::Refused { .. }
        | JobError::NoArtifact
        | JobError::ArtifactMismatch { .. }
        | JobError::SegmentOutsideArtifact { .. }
        | JobError::ArtifactDiffersFromJournal => before_first_request(FailureCode::JobRefused),
        JobError::Link(_) if first_run => before_first_request(FailureCode::LinkNotOpened),
        JobError::LinkUnconfirmed => before_first_request(FailureCode::DeviceLocked),
        JobError::NoReprogrammingSlot
        | JobError::CurrentThreadRuntime
        | JobError::CommLogMismatch
        | JobError::NoCommLogDir
        | JobError::GuardsMissing
        | JobError::Panicked
        | JobError::Journal(_) => before_first_request(FailureCode::Internal),
        // Interruptions with nothing changed. The exchange failed; the log says whether the
        // vehicle, the bus or the system is to blame.
        JobError::Link(_)
        | JobError::Host { .. }
        | JobError::IdentityRead { .. }
        | JobError::IdentityUnreadable { .. }
        | JobError::VehicleLock(_) => failed(FailureCode::ExchangeFailed),
        JobError::Unanswerable(_) => failed(FailureCode::Other),
        // Returned above.
        JobError::ProcedureFailed { .. }
        | JobError::IdentityMismatch { .. }
        | JobError::StepLimit(_)
        | JobError::Vm { .. }
        | JobError::OnSiteInterventionRequired(_) => unreachable!("decided before the standing"),
    })
}

/// The error's `log_text`, cut to the detail's bound.
fn detail(error: &JobError) -> String {
    truncated(error.log_text(), MAX_FAILURE_DETAIL_LEN)
}

/// The reason's name and its plain numeric fields, written by hand: a `Debug` rendering would
/// carry the journal's text and a voltage the VCI read, which the detail leaves out. The nested
/// teardown and confirmation outcomes are left to the operational log.
fn on_site_text(reason: &OnSiteReason) -> String {
    let at = |at: &crate::journal::StepRef| format!("pc {} step {}", at.pc, at.steps);
    match reason {
        OnSiteReason::UnreadableJournal(_) => "UnreadableJournal".to_owned(),
        OnSiteReason::MissingJournal => "MissingJournal".to_owned(),
        OnSiteReason::RecoveryRequiredPoint {
            flash_session,
            at: step,
        } => format!(
            "RecoveryRequiredPoint: flash session {flash_session}, {}",
            at(step)
        ),
        OnSiteReason::RecoveryRequiredSection { section, at: step } => {
            format!("RecoveryRequiredSection: section {section}, {}", at(step))
        }
        OnSiteReason::UnknownStage(stage) => format!("UnknownStage: stage {stage}"),
        OnSiteReason::MissingEntryState { flash_session } => {
            format!("MissingEntryState: flash session {flash_session}")
        }
        OnSiteReason::InvalidEntryState(_) => "InvalidEntryState".to_owned(),
        OnSiteReason::UnconfirmedRequest { at: step } => {
            format!("UnconfirmedRequest: {}", at(step))
        }
        OnSiteReason::NotResumable { cause } => match cause {
            NotResumableCause::UnsafePrimitiveAhead { pc } => {
                format!("NotResumable: UnsafePrimitiveAhead, pc {pc}")
            }
            NotResumableCause::RecoveryRequiredAhead { section, pc } => {
                format!("NotResumable: RecoveryRequiredAhead, section {section}, pc {pc}")
            }
            NotResumableCause::PlanAhead { flash_session } => {
                format!("NotResumable: PlanAhead, flash session {flash_session}")
            }
            NotResumableCause::UninterruptibleSection { pc } => {
                format!("NotResumable: UninterruptibleSection, pc {pc}")
            }
            NotResumableCause::NoIdentity => "NotResumable: NoIdentity".to_owned(),
        },
        OnSiteReason::ProcedureResumeLimitReached { resumes, max } => {
            format!("ProcedureResumeLimitReached: {resumes} of {max} resumes")
        }
        OnSiteReason::ProcedureIdentityNotEstablished => {
            "ProcedureIdentityNotEstablished".to_owned()
        }
        OnSiteReason::RestartNotAllowed { flash_session } => {
            format!("RestartNotAllowed: flash session {flash_session}")
        }
        OnSiteReason::ResumeLimitReached {
            flash_session,
            resumes,
            max,
        } => {
            format!("ResumeLimitReached: flash session {flash_session}, {resumes} of {max} resumes")
        }
        OnSiteReason::SupplyVoltage {
            flash_session,
            millivolts,
        } => format!(
            "SupplyVoltage: flash session {flash_session}, {}",
            if millivolts.is_some() {
                "outside the declared range"
            } else {
                "no reading"
            }
        ),
        OnSiteReason::TargetVinDiffers => "TargetVinDiffers".to_owned(),
        OnSiteReason::IntendedVersionDiffers => "IntendedVersionDiffers".to_owned(),
        OnSiteReason::ArtifactDiffers => "ArtifactDiffers".to_owned(),
        #[cfg(test)]
        OnSiteReason::StoppedBeforeErase { flash_session, .. } => {
            format!("StoppedBeforeErase: flash session {flash_session}")
        }
        OnSiteReason::UnsafeContinuation {
            flash_session, pc, ..
        } => format!("UnsafeContinuation: flash session {flash_session}, pc {pc}"),
        OnSiteReason::MissingPlanEndState { flash_session, .. } => {
            format!("MissingPlanEndState: flash session {flash_session}")
        }
        OnSiteReason::SoftwareVersionNotEstablished { flash_session, .. } => {
            format!("SoftwareVersionNotEstablished: flash session {flash_session}")
        }
        OnSiteReason::UnexpectedSoftwareVersion { flash_session, .. } => {
            format!("UnexpectedSoftwareVersion: flash session {flash_session}")
        }
        OnSiteReason::PreconditionNotMet { flash_session, .. } => {
            format!("PreconditionNotMet: flash session {flash_session}")
        }
        OnSiteReason::PreconditionNotMetBeforeErase { flash_session, .. } => {
            format!("PreconditionNotMetBeforeErase: flash session {flash_session}")
        }
        OnSiteReason::IdentityNotEstablished {
            flash_session,
            identity,
            ..
        } => format!(
            "IdentityNotEstablished: flash session {flash_session}, {}",
            match identity {
                IdentityKind::Vin => "VIN",
                IdentityKind::HardwarePartNumber => "hardware part number",
                IdentityKind::SoftwareVersion => "software version",
            }
        ),
        OnSiteReason::DefaultSessionNotConfirmed { flash_session, .. } => {
            format!("DefaultSessionNotConfirmed: flash session {flash_session}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guards::GuardError;
    use crate::host::HostError;
    use crate::journal::StepRef;
    use diag_ir::{Op, Vm, VmError, WaitingOn};
    use std::path::PathBuf;

    const MARKER: &str = "MARKER-external-text";

    fn program() -> Program {
        crate::document::tests::empty_program()
    }

    fn run(error: JobError, first_run: bool, standing: JournalStanding) -> JobOutcome {
        outcome(&Err(error), first_run, standing)
    }

    fn state(error: JobError, first_run: bool, standing: JournalStanding) -> JobState {
        match run(error, first_run, standing) {
            JobOutcome::Terminal(state) => state,
            other => panic!("not terminal: {other:?}"),
        }
    }

    fn code_of(state: &JobState) -> &FailureCode {
        match state {
            JobState::Failed { code, .. } | JobState::PreconditionFailed { code, .. } => code,
            other => panic!("carries no code: {other:?}"),
        }
    }

    fn detail_of(state: &JobState) -> &str {
        match state {
            JobState::Failed { detail, .. }
            | JobState::PreconditionFailed { detail, .. }
            | JobState::NeedsOnSiteRecovery { detail } => detail,
            other => panic!("carries no detail: {other:?}"),
        }
    }

    fn rpc() -> HostError {
        HostError::Rpc {
            rpc: "StartComPrimitive",
            status: Box::new(tonic::Status::unavailable(MARKER)),
        }
    }

    use JournalStanding::{Clean, MayHaveChanged};

    #[test]
    fn a_finished_run_is_completed() {
        let program = program();
        let finished = Ok(Vm::new(&program).state);
        assert_eq!(
            outcome(&finished, true, Clean),
            JobOutcome::Terminal(JobState::Completed)
        );
        assert_eq!(
            outcome(&finished, false, MayHaveChanged),
            JobOutcome::Terminal(JobState::Completed)
        );
    }

    #[test]
    fn a_procedure_failure_carries_its_own_code_and_detail_and_never_an_agent_code() {
        // A procedure may name its code after an agent's; it stays the procedure's.
        for code in ["internal", "VinMismatch", "linkNotOpened", "x"] {
            for standing in [Clean, MayHaveChanged] {
                let error = JobError::ProcedureFailed {
                    pc: 3,
                    code: code.to_owned(),
                    detail: "the check failed".to_owned(),
                };
                assert_eq!(
                    state(error, false, standing),
                    JobState::Failed {
                        code: FailureCode::Procedure,
                        detail: "the check failed".to_owned(),
                        procedure_code: Some(code.to_owned()),
                    }
                );
            }
        }
        // The procedure's detail is cut to the bound, on a character boundary.
        let long = "é".repeat(MAX_FAILURE_DETAIL_LEN);
        let JobState::Failed { detail, .. } = state(
            JobError::ProcedureFailed {
                pc: 0,
                code: "c".to_owned(),
                detail: long,
            },
            true,
            Clean,
        ) else {
            panic!("not failed");
        };
        assert_eq!(detail.len(), MAX_FAILURE_DETAIL_LEN);
    }

    #[test]
    fn identity_mismatches_name_the_kind_and_never_a_value() {
        let vin = state(
            JobError::IdentityMismatch {
                identity: IdentityKind::Vin,
            },
            false,
            MayHaveChanged,
        );
        assert_eq!(code_of(&vin), &FailureCode::VinMismatch);
        assert!(detail_of(&vin).contains("Vin"));
        let hardware = state(
            JobError::IdentityMismatch {
                identity: IdentityKind::HardwarePartNumber,
            },
            false,
            Clean,
        );
        assert_eq!(code_of(&hardware), &FailureCode::VariantMismatch);
        let software = state(
            JobError::IdentityMismatch {
                identity: IdentityKind::SoftwareVersion,
            },
            false,
            Clean,
        );
        assert_eq!(code_of(&software), &FailureCode::Other);
    }

    #[test]
    fn a_stop_the_vm_made_is_a_procedure_error_whatever_the_journal() {
        for standing in [Clean, MayHaveChanged] {
            for error in [
                JobError::StepLimit(9),
                JobError::Vm {
                    pc: 2,
                    source: VmError::StackUnderflow,
                },
            ] {
                let s = state(error, true, standing);
                assert_eq!(code_of(&s), &FailureCode::ProcedureError);
                assert!(matches!(s, JobState::Failed { .. }));
            }
        }
    }

    #[test]
    fn an_on_site_ending_names_the_reason_and_its_fields() {
        let s = state(
            JobError::OnSiteInterventionRequired(OnSiteReason::ResumeLimitReached {
                flash_session: 2,
                resumes: 3,
                max: 3,
            }),
            false,
            MayHaveChanged,
        );
        assert_eq!(
            s,
            JobState::NeedsOnSiteRecovery {
                detail: "ResumeLimitReached: flash session 2, 3 of 3 resumes".to_owned()
            }
        );
        // The strings and the voltage a reason holds stay out.
        for reason in [
            OnSiteReason::UnreadableJournal(MARKER.to_owned()),
            OnSiteReason::InvalidEntryState(MARKER.to_owned()),
            OnSiteReason::SupplyVoltage {
                flash_session: 1,
                millivolts: Some(12_345_678),
            },
        ] {
            let s = state(JobError::OnSiteInterventionRequired(reason), true, Clean);
            assert!(!detail_of(&s).contains(MARKER));
            assert!(!detail_of(&s).contains("12345678"));
        }
        let at = StepRef { pc: 4, steps: 9 };
        let s = state(
            JobError::OnSiteInterventionRequired(OnSiteReason::UnconfirmedRequest { at }),
            false,
            Clean,
        );
        assert_eq!(detail_of(&s), "UnconfirmedRequest: pc 4 step 9");
    }

    #[test]
    fn a_cancel_is_cancelled_while_the_journal_is_clean_and_open_after() {
        for first_run in [true, false] {
            assert_eq!(
                state(JobError::Cancelled, first_run, Clean),
                JobState::Cancelled
            );
            assert_eq!(
                run(JobError::Cancelled, first_run, MayHaveChanged),
                JobOutcome::Open
            );
        }
    }

    #[test]
    fn a_refusal_before_the_first_request_is_a_precondition_failure() {
        let errors = || {
            [
                JobError::SeveralTargets { count: 2 },
                JobError::NoDeclarations,
                JobError::NoArtifact,
                JobError::ArtifactMismatch {
                    named: "a".to_owned(),
                    actual: "b".to_owned(),
                },
                JobError::SegmentOutsideArtifact {
                    pc: 1,
                    session: 1,
                    segment: 1,
                    end: 9,
                    artifact_len: 4,
                },
                JobError::Undeclared {
                    pc: 0,
                    source: VmError::StackUnderflow,
                },
                JobError::Refused {
                    pc: 0,
                    source: HostError::NoResponse,
                },
                JobError::Program(diag_ir::ProgramError::BoundaryOutOfOrder { flash_session: 1 }),
            ]
        };
        for error in errors() {
            let s = state(error, true, Clean);
            assert!(matches!(s, JobState::PreconditionFailed { .. }), "{s:?}");
            assert_eq!(code_of(&s), &FailureCode::JobRefused);
            assert!(!detail_of(&s).is_empty());
        }
        // On a resume the same refusal is a failure, and the resume of a journal that may
        // have changed stays open.
        for error in errors() {
            let s = state(error, false, Clean);
            assert!(matches!(s, JobState::Failed { .. }), "{s:?}");
            assert_eq!(code_of(&s), &FailureCode::JobRefused);
        }
        for error in errors() {
            assert_eq!(run(error, true, MayHaveChanged), JobOutcome::Open);
        }
        let s = state(JobError::ArtifactDiffersFromJournal, false, Clean);
        assert_eq!(code_of(&s), &FailureCode::JobRefused);
    }

    #[test]
    fn a_link_that_did_not_open_is_not_an_exchange_failure() {
        let s = state(JobError::Link(HostError::NoResponse), true, Clean);
        assert!(matches!(s, JobState::PreconditionFailed { .. }));
        assert_eq!(code_of(&s), &FailureCode::LinkNotOpened);
        // On a resume the link is one more exchange.
        let s = state(JobError::Link(HostError::NoResponse), false, Clean);
        assert!(matches!(s, JobState::Failed { .. }));
        assert_eq!(code_of(&s), &FailureCode::ExchangeFailed);
        let s = state(JobError::LinkUnconfirmed, true, Clean);
        assert!(matches!(s, JobState::PreconditionFailed { .. }));
        assert_eq!(code_of(&s), &FailureCode::DeviceLocked);
    }

    #[test]
    fn only_a_fault_of_the_agent_is_internal() {
        for error in [
            JobError::NoReprogrammingSlot,
            JobError::CurrentThreadRuntime,
            JobError::CommLogMismatch,
            JobError::NoCommLogDir,
            JobError::GuardsMissing,
            JobError::Panicked,
            JobError::Journal(JournalError::Poisoned),
        ] {
            let s = state(error, true, Clean);
            assert!(matches!(s, JobState::PreconditionFailed { .. }), "{s:?}");
            assert_eq!(code_of(&s), &FailureCode::Internal);
        }
        let s = state(JobError::Panicked, false, Clean);
        assert!(matches!(s, JobState::Failed { .. }));
        assert_eq!(code_of(&s), &FailureCode::Internal);
        // A failed exchange claims neither side.
        for error in [
            JobError::Host {
                pc: 1,
                source: HostError::NoResponse,
            },
            JobError::IdentityUnreadable {
                pc: 1,
                identity: IdentityKind::Vin,
            },
            JobError::IdentityRead {
                pc: 1,
                identity: IdentityKind::Vin,
                source: HostError::NoResponse,
            },
            JobError::VehicleLock(GuardError::OtherVehicleHeld),
        ] {
            let s = state(error, true, Clean);
            assert!(matches!(s, JobState::Failed { .. }), "{s:?}");
            assert_eq!(code_of(&s), &FailureCode::ExchangeFailed);
        }
        let s = state(JobError::Unanswerable(WaitingOn::Hmi), false, Clean);
        assert_eq!(code_of(&s), &FailureCode::Other);
    }

    #[test]
    fn a_run_that_may_have_changed_the_journal_is_open() {
        for first_run in [true, false] {
            for error in [
                JobError::Host {
                    pc: 1,
                    source: HostError::NoResponse,
                },
                JobError::Link(HostError::NoResponse),
                JobError::LinkUnconfirmed,
                JobError::Panicked,
                JobError::Journal(JournalError::Poisoned),
                JobError::NoArtifact,
                JobError::VehicleLock(GuardError::OtherVehicleHeld),
                JobError::Unanswerable(WaitingOn::Timer),
                JobError::ArtifactDiffersFromJournal,
            ] {
                assert_eq!(run(error, first_run, MayHaveChanged), JobOutcome::Open);
            }
        }
    }

    #[test]
    fn a_job_another_run_owns_reports_nothing() {
        for error in [JournalError::InUse, JournalError::AlreadyExists] {
            for standing in [Clean, MayHaveChanged] {
                assert_eq!(
                    run(JobError::Journal(error_like(&error)), true, standing),
                    JobOutcome::None
                );
            }
        }
    }

    fn error_like(error: &JournalError) -> JournalError {
        match error {
            JournalError::InUse => JournalError::InUse,
            _ => JournalError::AlreadyExists,
        }
    }

    #[test]
    fn no_detail_carries_external_text() {
        let guard = || GuardError::UnsafeDir(PathBuf::from(MARKER));
        let errors = [
            JobError::Link(rpc()),
            JobError::Host {
                pc: 1,
                source: rpc(),
            },
            JobError::Refused {
                pc: 1,
                source: rpc(),
            },
            JobError::IdentityRead {
                pc: 1,
                identity: IdentityKind::Vin,
                source: rpc(),
            },
            JobError::VehicleLock(guard()),
            JobError::Journal(JournalError::Io(std::io::Error::other(MARKER))),
        ];
        for error in errors {
            for first_run in [true, false] {
                let s = state(error_copy(&error), first_run, Clean);
                assert!(!detail_of(&s).is_empty());
                assert!(
                    !detail_of(&s).contains(MARKER),
                    "{}: {}",
                    error.log_text(),
                    detail_of(&s)
                );
            }
        }
    }

    /// A copy of a marker-carrying error: `JobError` is not `Clone`.
    fn error_copy(error: &JobError) -> JobError {
        match error {
            JobError::Link(_) => JobError::Link(rpc()),
            JobError::Host { pc, .. } => JobError::Host {
                pc: *pc,
                source: rpc(),
            },
            JobError::Refused { pc, .. } => JobError::Refused {
                pc: *pc,
                source: rpc(),
            },
            JobError::IdentityRead { pc, identity, .. } => JobError::IdentityRead {
                pc: *pc,
                identity: *identity,
                source: rpc(),
            },
            JobError::VehicleLock(_) => {
                JobError::VehicleLock(GuardError::UnsafeDir(PathBuf::from(MARKER)))
            }
            JobError::Journal(_) => {
                JobError::Journal(JournalError::Io(std::io::Error::other(MARKER)))
            }
            _ => unreachable!("not a marker-carrying error"),
        }
    }

    #[test]
    fn a_detail_is_cut_to_the_bound() {
        let long = "x".repeat(4 * MAX_FAILURE_DETAIL_LEN);
        let s = state(
            JobError::ArtifactMismatch {
                named: long.clone(),
                actual: long,
            },
            true,
            Clean,
        );
        assert!(detail_of(&s).len() <= MAX_FAILURE_DETAIL_LEN);
    }

    fn at(pc: u32) -> StepRef {
        StepRef { pc, steps: 1 }
    }

    fn key() -> JobKey {
        JobKey {
            job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e6f".to_owned()),
            generation: 1,
        }
    }

    #[test]
    fn the_standing_is_read_from_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let mut program = program();
        program.code = vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::Pop,
        ];
        // No journal at all.
        assert_eq!(JournalStanding::read(dir.path(), &key(), &program), Clean);
        // One that holds no request marker.
        let mut journal = Journal::create(dir.path(), &key(), None, None, None).unwrap();
        assert_eq!(JournalStanding::read(dir.path(), &key(), &program), Clean);
        assert_eq!(JournalStanding::of(&program, Ok(journal.state())), Clean);
        // One that holds a marker, which a resume must judge.
        journal.commit_request(at(1)).unwrap();
        assert_eq!(
            JournalStanding::read(dir.path(), &key(), &program),
            MayHaveChanged
        );
        // A journal that cannot be read back may have changed.
        assert_eq!(
            JournalStanding::of(&program, Err(&JournalError::Poisoned)),
            MayHaveChanged
        );
        assert_eq!(
            JournalStanding::of(&program, Err(&JournalError::NotFound)),
            Clean
        );
    }
}
