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
use crate::restart::{NotResumableCause, OnSiteReason, RestartDecision, classify};
use crate::runner::{JobError, truncated};

// The procedure's code reaches the wire as the journal holds it.
const _: () = assert!(MAX_PROCEDURE_CODE_LEN == diag_ir::MAX_FAILURE_CODE_LEN);
const _: () = assert!(MAX_PROCEDURE_CODE_LEN == crate::journal::MAX_FAILURE_CODE_LEN);

/// What the next resume of the job would decide from its journal, as the run's end sees it
/// (ADR-475 item 1). It is [`restart::classify`]'s answer, not a second set of rules.
#[derive(Debug, Clone, PartialEq)]
pub enum JournalStanding {
    /// A resume would start the program afresh and lose nothing: no journal where nothing can
    /// have been sent, or one `classify` calls a plain start (it has not reached a flash plan
    /// and holds no request for a primitive that is not safe to repeat, ADR-335 item 2).
    Clean,
    /// A resume would go on from the journal: a restart or a checkpoint resume.
    Resumable,
    /// The journal holds a terminal decision (a procedure failure, an identity abort or a run's
    /// stop, ADR-280, ADR-286, ADR-288): a resume would end as it says without sending anything.
    /// The runs that end that way are terminal as their own result.
    Decided,
    /// A resume would end in on-site intervention for this reason, which includes a journal
    /// that is missing where one is due and one that cannot be read back.
    OnSite(OnSiteReason),
}

impl JournalStanding {
    /// The standing of the journal `journal` of a job of `program`, as read after the run.
    /// `first_run` says the run was the job's first: its journal is created before anything is
    /// sent, so a journal that is not there is clean, whereas on a resume it is
    /// [`OnSiteReason::MissingJournal`] unless the program is a plain start. A job ID that cannot
    /// name a journal is clean on any run: no journal could have been written.
    pub fn of(
        program: &Program,
        journal: Result<&JournalState, &JournalError>,
        first_run: bool,
    ) -> Self {
        match journal {
            Err(JournalError::NotFound) if first_run => return Self::Clean,
            Err(JournalError::InvalidJobId(_)) => return Self::Clean,
            _ => {}
        }
        match classify(program, journal) {
            RestartDecision::PlainStart => Self::Clean,
            RestartDecision::OnSiteInterventionRequired(reason) => Self::OnSite(reason),
            RestartDecision::Restart(_) | RestartDecision::ResumeFromCheckpoint(_) => {
                Self::Resumable
            }
            RestartDecision::Failed { .. }
            | RestartDecision::Aborted { .. }
            | RestartDecision::Stopped(_) => Self::Decided,
        }
    }

    /// The standing of the journal of `key` in `dir`, read without changing it (`Journal::read`
    /// takes no lock, so it works while another run holds the journal).
    pub fn read(dir: &Path, key: &JobKey, program: &Program, first_run: bool) -> Self {
        Self::of(program, Journal::read(dir, key).as_ref(), first_run)
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
/// would end the same way (a VM error only while no resume would go on from the journal). For every other ending the standing decides (ADR-475 item 1):
/// - decided, the open job's resume reports what the journal holds;
/// - on-site, the next resume would end in on-site intervention, so the job does: terminal at
///   once, whatever the error;
/// - resumable, a refusal that every resume would repeat ends in on-site intervention, since
///   the job cannot go on by itself; any other error leaves the job open;
/// - clean, the error is terminal. A refusal or a failure before anything was sent on a first
///   run is a precondition failure; on a resume the same causes are failures.
pub fn outcome(
    result: &Result<VmState, JobError>,
    first_run: bool,
    standing: &JournalStanding,
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
        JobError::StepLimit(_) => {
            return JobOutcome::Terminal(failed(FailureCode::ProcedureError));
        }
        // The runner journals a stop its run made, so the standing is then `Decided`; a VM error
        // of a check before the run on a journal a resume would go on from is not a stop.
        JobError::Vm { .. }
            if matches!(standing, JournalStanding::Clean | JournalStanding::Decided) =>
        {
            return JobOutcome::Terminal(failed(FailureCode::ProcedureError));
        }
        JobError::OnSiteInterventionRequired(reason) => {
            return JobOutcome::Terminal(on_site(reason));
        }
        // Another run owns the journal: that run reports the job.
        JobError::Journal(JournalError::InUse | JournalError::AlreadyExists) => {
            return JobOutcome::None;
        }
        _ => {}
    }
    match standing {
        JournalStanding::OnSite(reason) => return JobOutcome::Terminal(on_site(reason)),
        JournalStanding::Resumable => {
            return if repeats_on_every_resume(error) {
                JobOutcome::Terminal(JobState::NeedsOnSiteRecovery {
                    detail: detail(error),
                })
            } else {
                JobOutcome::Open
            };
        }
        // The resume would end as the journal says: the open job reports it then.
        JournalStanding::Decided => return JobOutcome::Open,
        JournalStanding::Clean => {}
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
        | JobError::GuardsMissing => before_first_request(FailureCode::Internal),
        // These can happen mid-run, so they are failures on a first run too.
        JobError::Panicked | JobError::Journal(_) => failed(FailureCode::Internal),
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

/// The state of a job that needs on-site intervention for `reason`.
fn on_site(reason: &OnSiteReason) -> JobState {
    JobState::NeedsOnSiteRecovery {
        detail: truncated(on_site_text(reason), MAX_FAILURE_DETAIL_LEN),
    }
}

/// Whether `error` is a refusal that the next resume of the same job would meet again: it comes
/// from the program, the job's data or the agent's configuration, not from the vehicle, the
/// link or the moment, so leaving the job open would only repeat it until the resume limit.
fn repeats_on_every_resume(error: &JobError) -> bool {
    match error {
        JobError::Program(_)
        | JobError::Vm { .. }
        | JobError::SeveralTargets { .. }
        | JobError::Undeclared { .. }
        | JobError::NoDeclarations
        | JobError::Sources(_)
        | JobError::Refused { .. }
        | JobError::NoArtifact
        | JobError::ArtifactMismatch { .. }
        | JobError::SegmentOutsideArtifact { .. }
        | JobError::ArtifactDiffersFromJournal
        | JobError::CommLogMismatch
        | JobError::NoCommLogDir
        | JobError::CurrentThreadRuntime
        | JobError::GuardsMissing => true,
        // A failed exchange, a lock or link held for now, a fault that need not recur and an
        // interruption: the next resume may go through.
        JobError::Link(_)
        | JobError::Host { .. }
        | JobError::IdentityRead { .. }
        | JobError::IdentityUnreadable { .. }
        | JobError::NoReprogrammingSlot
        | JobError::VehicleLock(_)
        | JobError::LinkUnconfirmed
        | JobError::Journal(_)
        | JobError::Panicked
        | JobError::Cancelled
        | JobError::Unanswerable(_) => false,
        // Terminal before the standing is read.
        JobError::ProcedureFailed { .. }
        | JobError::IdentityMismatch { .. }
        | JobError::StepLimit(_)
        | JobError::OnSiteInterventionRequired(_) => false,
    }
}

/// The error's `log_text`, cut to the detail's bound.
fn detail(error: &JobError) -> String {
    truncated(error.log_text(), MAX_FAILURE_DETAIL_LEN)
}

/// The reason's name and its plain numeric fields, written by hand: a `Debug` rendering would
/// carry the journal's text and a voltage the VCI read, which the detail leaves out. The nested
/// teardown and confirmation outcomes are left to the operational log.
pub(crate) fn on_site_text(reason: &OnSiteReason) -> String {
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
    use crate::inputs::TableError;
    use crate::journal::StepRef;
    use diag_ir::{Idempotency, Interruptible, Op, Section, Vm, VmError, WaitingOn};
    use std::path::PathBuf;

    const MARKER: &str = "MARKER-external-text";

    fn program() -> Program {
        crate::document::tests::empty_program()
    }

    fn run(error: JobError, first_run: bool, standing: JournalStanding) -> JobOutcome {
        outcome(&Err(error), first_run, &standing)
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

    /// The `index`th error of `list`, built afresh: `JobError` is not `Clone`.
    fn nth(list: fn() -> Vec<JobError>, index: usize) -> JobError {
        list().swap_remove(index)
    }

    /// An on-site standing for a reason that holds no text.
    fn on_site_standing() -> JournalStanding {
        JournalStanding::OnSite(OnSiteReason::ResumeLimitReached {
            flash_session: 1,
            resumes: 2,
            max: 2,
        })
    }

    use JournalStanding::{Clean, Decided, Resumable};

    /// A refusal every resume would meet again: one of each variant that `repeats_on_every_resume`
    /// takes.
    fn deterministic_errors() -> Vec<JobError> {
        vec![
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
            JobError::Sources(TableError::Duplicate {
                service_id: 1,
                field_id: 2,
            }),
            JobError::Vm {
                pc: 2,
                source: VmError::StackUnderflow,
            },
            JobError::ArtifactDiffersFromJournal,
            JobError::CommLogMismatch,
            JobError::NoCommLogDir,
            JobError::CurrentThreadRuntime,
            JobError::GuardsMissing,
        ]
    }

    /// Interruptions the next resume may get past.
    fn transient_errors() -> Vec<JobError> {
        vec![
            JobError::Host {
                pc: 1,
                source: HostError::NoResponse,
            },
            JobError::Link(HostError::NoResponse),
            JobError::IdentityRead {
                pc: 1,
                identity: IdentityKind::Vin,
                source: HostError::NoResponse,
            },
            JobError::IdentityUnreadable {
                pc: 1,
                identity: IdentityKind::Vin,
            },
            JobError::NoReprogrammingSlot,
            JobError::LinkUnconfirmed,
            JobError::Panicked,
            JobError::Journal(JournalError::Poisoned),
            JobError::Journal(JournalError::Io(std::io::Error::other("x"))),
            JobError::VehicleLock(GuardError::OtherVehicleHeld),
            JobError::Unanswerable(WaitingOn::Timer),
            JobError::Cancelled,
        ]
    }

    #[test]
    fn a_finished_run_is_completed() {
        let program = program();
        let finished = Ok(Vm::new(&program).state);
        assert_eq!(
            outcome(&finished, true, &Clean),
            JobOutcome::Terminal(JobState::Completed)
        );
        assert_eq!(
            outcome(&finished, false, &Resumable),
            JobOutcome::Terminal(JobState::Completed)
        );
    }

    #[test]
    fn a_procedure_failure_carries_its_own_code_and_detail_and_never_an_agent_code() {
        // A procedure may name its code after an agent's; it stays the procedure's.
        for code in ["internal", "VinMismatch", "linkNotOpened", "x"] {
            for standing in [Clean, Resumable, Decided, on_site_standing()] {
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
            Resumable,
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
        for standing in [Clean, Resumable, Decided, on_site_standing()] {
            let s = state(JobError::StepLimit(9), true, standing);
            assert_eq!(code_of(&s), &FailureCode::ProcedureError);
        }
        // A VM error is the run's own stop once the journal holds it (or holds nothing).
        for standing in [Clean, Decided] {
            let s = state(
                JobError::Vm {
                    pc: 2,
                    source: VmError::StackUnderflow,
                },
                true,
                standing,
            );
            assert_eq!(code_of(&s), &FailureCode::ProcedureError);
            assert!(matches!(s, JobState::Failed { .. }));
        }
    }

    #[test]
    fn a_vm_error_on_a_journal_a_resume_goes_on_from_needs_on_site_recovery() {
        let error = || JobError::Vm {
            pc: 2,
            source: VmError::StackUnderflow,
        };
        let s = state(error(), false, Resumable);
        assert!(matches!(s, JobState::NeedsOnSiteRecovery { .. }), "{s:?}");
        assert!(!detail_of(&s).is_empty());
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
            Resumable,
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
    fn an_on_site_standing_ends_the_job_at_once_whatever_the_error() {
        let expected = JobState::NeedsOnSiteRecovery {
            detail: "ResumeLimitReached: flash session 1, 2 of 2 resumes".to_owned(),
        };
        let lists: [fn() -> Vec<JobError>; 2] = [deterministic_errors, transient_errors];
        for list in lists {
            for index in 0..list().len() {
                for first_run in [true, false] {
                    let error = nth(list, index);
                    let name = format!("{error:?}");
                    assert_eq!(
                        run(error, first_run, on_site_standing()),
                        JobOutcome::Terminal(expected.clone()),
                        "{name}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_cancel_is_cancelled_while_the_journal_is_clean_and_open_after() {
        for first_run in [true, false] {
            assert_eq!(
                state(JobError::Cancelled, first_run, Clean),
                JobState::Cancelled
            );
            assert_eq!(
                run(JobError::Cancelled, first_run, Resumable),
                JobOutcome::Open
            );
        }
    }

    #[test]
    fn a_refusal_before_the_first_request_is_a_precondition_failure() {
        let errors = || {
            deterministic_errors()
                .into_iter()
                // A VM error is a stop, not a refusal, on a clean journal.
                .filter(|error| !matches!(error, JobError::Vm { .. }))
                .filter(|error| {
                    // These four are the agent's own faults, not the job's.
                    !matches!(
                        error,
                        JobError::CommLogMismatch
                            | JobError::NoCommLogDir
                            | JobError::CurrentThreadRuntime
                            | JobError::GuardsMissing
                    )
                })
        };
        for error in errors() {
            let s = state(error, true, Clean);
            assert!(matches!(s, JobState::PreconditionFailed { .. }), "{s:?}");
            assert_eq!(code_of(&s), &FailureCode::JobRefused);
            assert!(!detail_of(&s).is_empty());
        }
        // On a resume the same refusal is a failure.
        for error in errors() {
            let s = state(error, false, Clean);
            assert!(matches!(s, JobState::Failed { .. }), "{s:?}");
            assert_eq!(code_of(&s), &FailureCode::JobRefused);
        }
    }

    #[test]
    fn a_refusal_on_a_journal_a_resume_goes_on_from_needs_on_site_recovery() {
        for index in 0..deterministic_errors().len() {
            for first_run in [true, false] {
                let error = nth(deterministic_errors, index);
                let name = format!("{error:?}");
                let JobOutcome::Terminal(s) = run(error, first_run, Resumable) else {
                    panic!("{name}: not terminal");
                };
                assert!(matches!(s, JobState::NeedsOnSiteRecovery { .. }), "{name}");
                assert!(!detail_of(&s).is_empty(), "{name}");
            }
        }
        let s = state(JobError::ArtifactDiffersFromJournal, false, Resumable);
        assert!(matches!(s, JobState::NeedsOnSiteRecovery { .. }));
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
        ] {
            let s = state(error, true, Clean);
            assert!(matches!(s, JobState::PreconditionFailed { .. }), "{s:?}");
            assert_eq!(code_of(&s), &FailureCode::Internal);
        }
        let s = state(JobError::NoCommLogDir, false, Clean);
        assert!(matches!(s, JobState::Failed { .. }));
        // A panic and a journal fault can happen mid-run, so they are failures on a first run too.
        for first_run in [true, false] {
            for error in [
                JobError::Panicked,
                JobError::Journal(JournalError::Poisoned),
                JobError::Journal(JournalError::Io(std::io::Error::other(MARKER))),
            ] {
                let s = state(error, first_run, Clean);
                assert!(matches!(s, JobState::Failed { .. }), "{s:?}");
                assert_eq!(code_of(&s), &FailureCode::Internal);
            }
        }
        let s = state(
            JobError::Journal(JournalError::Io(std::io::Error::other(MARKER))),
            true,
            Clean,
        );
        assert!(matches!(s, JobState::Failed { .. }), "{s:?}");
        assert_eq!(code_of(&s), &FailureCode::Internal);
        assert!(!detail_of(&s).contains(MARKER));
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
    fn a_transient_error_on_a_journal_a_resume_goes_on_from_is_open() {
        for first_run in [true, false] {
            for index in 0..transient_errors().len() {
                let error = nth(transient_errors, index);
                let name = format!("{error:?}");
                assert_eq!(run(error, first_run, Resumable), JobOutcome::Open, "{name}");
            }
        }
        // A journal that holds a decision ends the resume as it says: the run stays open.
        assert_eq!(run(JobError::Cancelled, false, Decided), JobOutcome::Open);
    }

    #[test]
    fn a_job_another_run_owns_reports_nothing() {
        for standing in [Clean, Resumable, Decided, on_site_standing()] {
            for first_run in [true, false] {
                assert_eq!(
                    run(
                        JobError::Journal(JournalError::InUse),
                        first_run,
                        standing.clone()
                    ),
                    JobOutcome::None
                );
                assert_eq!(
                    run(
                        JobError::Journal(JournalError::AlreadyExists),
                        first_run,
                        standing.clone()
                    ),
                    JobOutcome::None
                );
            }
        }
    }

    /// Errors that carry text NGR does not write: a worker's, the system's or the document's.
    fn marker_errors() -> Vec<JobError> {
        vec![
            JobError::Link(rpc()),
            JobError::Link(HostError::WorkerStart {
                spawn_failed: true,
                reason: MARKER.to_owned(),
            }),
            JobError::Link(HostError::DeclaredLink(MARKER.to_owned())),
            JobError::Host {
                pc: 1,
                source: rpc(),
            },
            JobError::Host {
                pc: 1,
                source: HostError::StreamFailedBeforeSend(Box::new(HostError::DeclaredLink(
                    MARKER.to_owned(),
                ))),
            },
            JobError::Refused {
                pc: 1,
                source: rpc(),
            },
            JobError::Refused {
                pc: 1,
                source: HostError::DeclaredLink(MARKER.to_owned()),
            },
            JobError::IdentityRead {
                pc: 1,
                identity: IdentityKind::Vin,
                source: rpc(),
            },
            JobError::VehicleLock(GuardError::UnsafeDir(PathBuf::from(MARKER))),
            JobError::VehicleLock(GuardError::UnlistableDir(PathBuf::from(MARKER))),
            JobError::VehicleLock(GuardError::NotAFile(PathBuf::from(MARKER))),
            JobError::VehicleLock(GuardError::InvalidVci(MARKER.to_owned())),
            JobError::VehicleLock(GuardError::Io(std::io::Error::other(MARKER))),
            JobError::Journal(JournalError::Io(std::io::Error::other(MARKER))),
            JobError::Journal(JournalError::InvalidJobId(MARKER.to_owned())),
            JobError::Sources(TableError::Document {
                service_id: 1,
                field_id: 2,
                reason: MARKER.to_owned(),
            }),
            JobError::OnSiteInterventionRequired(OnSiteReason::UnreadableJournal(
                MARKER.to_owned(),
            )),
            JobError::OnSiteInterventionRequired(OnSiteReason::InvalidEntryState(
                MARKER.to_owned(),
            )),
        ]
    }

    #[test]
    fn no_detail_carries_external_text() {
        let standings = || {
            [
                Clean,
                Resumable,
                Decided,
                JournalStanding::OnSite(OnSiteReason::UnreadableJournal(MARKER.to_owned())),
                JournalStanding::OnSite(OnSiteReason::InvalidEntryState(MARKER.to_owned())),
            ]
        };
        let count = marker_errors().len();
        for index in 0..count {
            for first_run in [true, false] {
                for standing in standings() {
                    let error = marker_errors().swap_remove(index);
                    let text = error.log_text();
                    assert!(!text.contains(MARKER), "log_text: {text}");
                    if let JobOutcome::Terminal(s) = run(error, first_run, standing) {
                        assert!(!detail_of(&s).contains(MARKER), "{text}: {s:?}");
                    }
                }
            }
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

    fn request_program() -> Program {
        let mut program = program();
        program.code = vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::Pop,
        ];
        program
    }

    fn recovery_required_section() -> Section {
        Section {
            start_pc: 0,
            end_pc: 3,
            interruptible: Interruptible::RecoveryRequired,
            idempotency: Idempotency::Safe,
            expected_millis: 0,
        }
    }

    #[test]
    fn a_missing_journal_is_clean_on_a_first_run_and_judged_by_restart_on_a_resume() {
        let dir = tempfile::tempdir().unwrap();
        // No primitive, section or plan: a plain start, so a missing journal loses nothing.
        let plain = program();
        let mut guarded = request_program();
        guarded.sections = vec![recovery_required_section()];
        for first_run in [true, false] {
            assert_eq!(
                JournalStanding::read(dir.path(), &key(), &plain, first_run),
                Clean
            );
        }
        assert_eq!(
            JournalStanding::read(dir.path(), &key(), &guarded, true),
            Clean
        );
        // The runner creates the journal before it sends anything, so a resume that finds none
        // of a program that is not a plain start cannot tell what was sent.
        assert_eq!(
            JournalStanding::read(dir.path(), &key(), &guarded, false),
            JournalStanding::OnSite(OnSiteReason::MissingJournal)
        );
        // A job ID that cannot name a journal never had one.
        for first_run in [true, false] {
            assert_eq!(
                JournalStanding::of(
                    &guarded,
                    Err(&JournalError::InvalidJobId("x".to_owned())),
                    first_run
                ),
                Clean
            );
        }
    }

    #[test]
    fn an_intent_in_a_recovery_required_section_without_a_marker_needs_on_site_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let mut program = request_program();
        program.sections = vec![recovery_required_section()];
        let mut journal = Journal::create(dir.path(), &key(), None, None, None).unwrap();
        assert_eq!(
            JournalStanding::read(dir.path(), &key(), &program, true),
            Clean
        );
        journal.commit_intent(at(1)).unwrap();
        let standing = JournalStanding::read(dir.path(), &key(), &program, true);
        assert!(
            matches!(
                standing,
                JournalStanding::OnSite(OnSiteReason::RecoveryRequiredSection { section: 0, .. })
            ),
            "{standing:?}"
        );
        // Whatever the run's error, the job ends in on-site recovery.
        let s = state(JobError::Cancelled, true, standing);
        assert!(matches!(s, JobState::NeedsOnSiteRecovery { .. }), "{s:?}");
        assert!(detail_of(&s).starts_with("RecoveryRequiredSection: section 0"));
    }

    #[test]
    fn the_standing_is_what_the_next_resume_would_decide() {
        let dir = tempfile::tempdir().unwrap();
        let program = request_program();
        // One that holds no request marker.
        let mut journal = Journal::create(dir.path(), &key(), None, None, None).unwrap();
        assert_eq!(
            JournalStanding::read(dir.path(), &key(), &program, false),
            Clean
        );
        assert_eq!(
            JournalStanding::of(&program, Ok(journal.state()), true),
            Clean
        );
        // One that holds a marker no state confirms: a resume needs someone on site.
        journal.commit_request(at(1)).unwrap();
        assert!(matches!(
            JournalStanding::read(dir.path(), &key(), &program, true),
            JournalStanding::OnSite(OnSiteReason::UnconfirmedRequest { .. })
        ));
        // A journal that cannot be read back is on-site as well, on a resume and on a first run.
        for first_run in [true, false] {
            assert!(matches!(
                JournalStanding::of(&program, Err(&JournalError::Poisoned), first_run),
                JournalStanding::OnSite(OnSiteReason::UnreadableJournal(_))
            ));
        }
        // A journal that holds a decision.
        journal
            .commit_procedure_failed(at(1), "c", "detail")
            .unwrap();
        assert_eq!(
            JournalStanding::read(dir.path(), &key(), &program, false),
            Decided
        );
    }
}
