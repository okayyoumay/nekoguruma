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
//! through the declared sources. `teardown` then ends the interrupted download on the gates'
//! decision and the journal's exclusions (step 2b-1, ADR-264): an ECUReset, or a passive wait for
//! the ECU's session to expire. `confirm_default_session` then waits out the ECU's startup time
//! and confirms by reading F186 that the ECU is back in its default session (step 2b-2,
//! ADR-265). `check_identity` reads the VIN and the hardware identity again (step 3a, ADR-267),
//! and `check_state` reads the software version and decides between redoing the transfer and the
//! read-back verification (step 3b-2, ADR-268 item 5).
//!
//! The interruption point is the latest of the last completed step and the requests the
//! journal wrote ahead (the transfer-start and RequestTransferExit markers, and the request
//! intent of ADR-253), by step count, so a crash after a guarded request was sent but before its
//! response was recorded is placed at that request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use diag_ir::{
    DiagHost, FlashRecovery, IdentityKind, Interruptible, NoApplication, Precondition,
    PreconditionKind, Program, RecoveryRequired, RecoveryTiming, RuntimeInput, Source, Vm, VmError,
    VmState,
};

use crate::host::HostError;
use crate::inputs::{FieldBytes, Reading, RuntimeInputs, ServiceSources};
use crate::inputs::{read_field_bytes, resolve_source};
use crate::journal::Vin;
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
    /// The resume names another target VIN than the journal recorded at the job's first run
    /// (none included), so the job's own data changed between runs. Nothing is sent and no
    /// resume is counted (ADR-261).
    TargetVinDiffers,
    /// The resume names another intended software version than the journal recorded at the
    /// job's first run (none included), so the job's own data changed between runs. Nothing is
    /// sent and no resume is counted (ADR-268).
    IntendedVersionDiffers,
    /// The restart passed step 1 (the checks that need no ECU service, and its resume was
    /// counted), the gates of step 2, the teardown of step 2b-1 (`teardown`), the
    /// default-session confirmation of step 2b-2 (`confirmed`) and the identity checks of step 3a
    /// (`check_identity`) and the ECU state check of step 3b-2 (`state`, `check_state`). Steps 1
    /// to 3 passed. Step 4 (the replay to the erase) and the read-back verification do not run in
    /// this agent yet, so the job stops before anything that changes the ECU (ADR-255, ADR-261,
    /// ADR-264, ADR-265, ADR-268 item 5).
    RestartOrderUnavailable {
        flash_session: u32,
        teardown: Teardown,
        confirmed: Confirmation,
        state: StateCheck,
    },
    /// Step 3b-2 could not establish the ECU's software version (ADR-229 item 2 step 3,
    /// ADR-268 item 5): there is no declared source, or every read (the first and the plan's
    /// retries) gave no answer, a negative response other than the declared no-application one,
    /// a value that does not decode, or a worker failure. `teardown` is how the download was
    /// ended first.
    SoftwareVersionNotEstablished {
        flash_session: u32,
        teardown: Teardown,
    },
    /// Step 3b-2 read a software version that is neither the one recorded before the erase nor the
    /// intended one (ADR-268 item 5): the ECU holds an image the job cannot account for. The read
    /// is conclusive and not retried. `teardown` is how the download was ended first.
    UnexpectedSoftwareVersion {
        flash_session: u32,
        teardown: Teardown,
    },
    /// Step 3a could not establish an identity in the default session (ADR-229 item 2 step 3):
    /// the ECU gave no answer, a negative response or an undecodable value, or the journal or the
    /// program lacks what the comparison needs. `identity` is the one that failed; the VIN is
    /// checked first, the hardware identity only after the VIN matched. `teardown` is how the
    /// download was ended first, so the technician knows whether an ECUReset was sent. A decoded
    /// identity that differs is not this reason but [`JobError::IdentityMismatch`].
    IdentityNotEstablished {
        flash_session: u32,
        identity: IdentityKind,
        teardown: Teardown,
    },
    /// The ECU could not be confirmed back in its default session (ADR-229 item 2 step 2b-2,
    /// ADR-265): the confirmation failed, and so did the one after the passive teardown, or the
    /// teardown was passive already and its confirmation failed. Someone on site must check the
    /// ECU. `teardown` is how the download was ended first; after `Reset` and `CompletedPath` the
    /// passive wait ran between the two confirmations, after `Passive` it ran before the only one.
    DefaultSessionNotConfirmed {
        flash_session: u32,
        teardown: Teardown,
    },
}

/// What the ECU state check of ADR-229 item 2 step 3b-2 found, so what the restart does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateCheck {
    /// The ECU holds no valid application, or the software version it reports is the one read
    /// before the erase, or the intended one without the journal showing the post-transfer steps
    /// complete: the transfer is done again.
    RedoTransfer,
    /// The journal shows the post-transfer steps complete and the ECU reports the intended
    /// software version: the transfer is not redone; the read-back verification of ADR-268
    /// item 5 follows.
    ReadBackVerification,
}

/// What the gates of ADR-229 item 2 step 2 allow the restart's teardown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeardownGate {
    /// The VIN and the hardware identity match and every declared precondition holds: the gates
    /// do not rule out an ECUReset. The teardown still applies the journal's exclusions of
    /// ADR-229 item 2 step 2 (no reset once RequestTransferExit was journaled without the
    /// post-transfer steps complete, and none on the completed path); those are the
    /// teardown's, not the gates'.
    ResetAllowed,
    /// A gate did not pass: the teardown is passive (it waits out the session timeout), with no
    /// ECUReset.
    PassiveOnly(PassiveReason),
}

/// The first gate that did not pass. No VIN is kept in it (design 16.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassiveReason {
    /// The ECU's VIN could not be compared with the job's: the job names none or a VIN that is
    /// not well-formed, the program declares no source, the table does not map it, or the read
    /// gave no answer, a negative response, a value that is not a well-formed VIN, or the
    /// worker failed.
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

/// How the restart's teardown (ADR-229 item 2 step 2b-1, ADR-264) ended the interrupted download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Teardown {
    /// The ECU accepted an ECUReset (hardReset). The teardown itself did not wait: the ECU's
    /// startup time and the confirmation that it is back in its default session belong to the
    /// next step (`confirm_default_session`, ADR-265).
    Reset,
    /// No accepted ECUReset ended the download: the reset was ruled out, refused or got no
    /// usable answer. The agent then waited out the ECU's session timeout plus the plan's
    /// margin, sending nothing further, so the ECU's session has expired.
    Passive(PassiveCause),
    /// The journal shows the post-transfer steps complete: no reset and no wait, since the
    /// default-session confirmation of step 2b-2 comes first (it still waits the ECU's startup
    /// time, which may follow a reset of the procedure) and only its failure makes the teardown
    /// passive (ADR-265).
    CompletedPath,
}

/// Why a teardown was passive. No VIN is kept in it (design 16.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassiveCause {
    /// A gate of step 2 did not pass.
    Gate(PassiveReason),
    /// A RequestTransferExit intent is journaled and the post-transfer steps are not complete:
    /// the request may have reached the ECU, which a reset could disturb.
    TransferExitJournaled,
    /// The ECU answered the ECUReset negatively with this response code.
    ResetRefused { nrc: u8 },
    /// Whether the ECU reset is not known: the request got no answer or failed, or the answer
    /// is neither a positive response to the reset nor a final negative one.
    ResetOutcomeUnknown,
}

/// That the ECU was confirmed back in its default session (ADR-229 item 2 step 2b-2, ADR-265).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Confirmation {
    /// The first confirmation failed, the passive teardown ran after it, and the second one
    /// confirmed. Always false after a teardown that was passive already.
    pub after_passive_retry: bool,
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
/// - the VIN: the ECU's, read through the program's source, must equal the VIN the job
///   targets, which the journal recorded at the job's first run (`RecoveryFacts::target_vin`).
///   A job that names none or a VIN that is not well-formed (`Vin::is_well_formed`), a program that
///   declares no source, a source the table does not map, and a read that gives no well-formed
///   VIN (no answer, a negative response, an undecodable, padded or lower-case field, a worker
///   failure) leave the vehicle unidentified: [`PassiveReason::VinNotEstablished`], and nothing
///   is read for a job with no usable VIN. Only a well-formed VIN that differs aborts the job
///   ([`JobError::IdentityMismatch`]), since the ECU is another vehicle's;
/// - the hardware identity: the ECU's, read as raw field bytes, must equal the journal's, which
///   was recorded before the erase;
/// - each declared precondition, in the order voltage, external supply, ignition, engine,
///   vehicle speed: its value must lie in the declared range. The ECU's session is not known
///   yet (it may still be in its programming session), so the default-session source is read
///   first and the programming-session source only when that gives no value. A value outside
///   the range fails at once.
///
/// `ResetAllowed` rules out nothing but the gates: the teardown still applies the journal's
/// exclusions of step 2 (no reset once RequestTransferExit was journaled without the
/// post-transfer steps complete, none on the completed path).
///
/// `promote` takes the per-vehicle lock (ADR-263, design 8.2.5, 8.8). It is called once, with the
/// job's target VIN, right after the ECU's VIN read matched it and before the hardware identity
/// read, so no ECU request lies between the match and the lock. It is not called when the VIN is
/// not established or differs. Its error is returned as it is, and ends the gates.
///
/// A cancel stops it at the start, before and right after every read. Nothing here sends anything but
/// ReadDataByIdentifier requests through the declared sources, and reads of runtime inputs. No
/// VIN is put in a log message or a result.
pub(crate) fn check_gates<H>(
    program: &Program,
    point: &RestartPoint,
    sources: &ServiceSources,
    host: &mut H,
    cancelled: &AtomicBool,
    promote: impl FnOnce(&Vin) -> Result<(), JobError>,
) -> Result<TeardownGate, JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs,
{
    let passive = |reason| Ok(TeardownGate::PassiveOnly(reason));
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }

    // The VIN.
    let (Some(target_vin), Some(source)) = (
        point
            .facts
            .target_vin
            .as_ref()
            .filter(|vin| Vin::is_well_formed_text(vin.as_str())),
        program.identity.vin,
    ) else {
        return passive(PassiveReason::VinNotEstablished);
    };
    let target = target_vin.as_str();
    match cancellable(cancelled, || resolve_source(source, sources, host))? {
        Ok(Reading::Text(text)) if Vin::is_well_formed_text(&text) && text == target => {
            // The vehicle is identified: take its lock before any further ECU request
            // (design 8.2.5, 8.8; ADR-263).
            promote(target_vin)?;
        }
        Ok(Reading::Text(text)) if Vin::is_well_formed_text(&text) => {
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

/// The identity checks of ADR-229 item 2 step 3a, run in the default session after
/// `confirm_default_session`. In this order, stopping at the first that does not pass:
/// - the VIN, required again even when step 2 matched it: the ECU's, read through the program's
///   source, must equal the journal's target VIN. A well-formed equal VIN calls `promote` with
///   the target VIN (always: guards that already hold the vehicle return at once, and guards that
///   do not, because step 2 could not read the VIN, take the lock here); a well-formed VIN that
///   differs is [`JobError::IdentityMismatch`]; anything else (no target VIN or no declared
///   source, no answer, a negative response, a value that is not a well-formed VIN, a worker
///   failure) is [`OnSiteReason::IdentityNotEstablished`];
/// - the hardware identity, only after the VIN matched: the raw field bytes must equal the
///   journal's. Different bytes are [`JobError::IdentityMismatch`]; anything else is
///   [`OnSiteReason::IdentityNotEstablished`].
///
/// `teardown` is the step 2b-1 outcome, which every [`OnSiteReason::IdentityNotEstablished`]
/// carries. A cancel stops it at the start, before and right after every read. Nothing here sends
/// anything but ReadDataByIdentifier requests through the declared sources. No VIN is put in a log message
/// or a result (design 16.2).
pub(crate) fn check_identity<H>(
    program: &Program,
    point: &RestartPoint,
    teardown: &Teardown,
    sources: &ServiceSources,
    host: &mut H,
    cancelled: &AtomicBool,
    promote: impl FnOnce(&Vin) -> Result<(), JobError>,
) -> Result<(), JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs,
{
    let not_established = |identity| {
        Err(JobError::OnSiteInterventionRequired(
            OnSiteReason::IdentityNotEstablished {
                flash_session: point.flash_session,
                identity,
                teardown: teardown.clone(),
            },
        ))
    };
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }

    // The VIN.
    let (Some(target_vin), Some(source)) = (
        point
            .facts
            .target_vin
            .as_ref()
            .filter(|vin| Vin::is_well_formed_text(vin.as_str())),
        program.identity.vin,
    ) else {
        return not_established(IdentityKind::Vin);
    };
    let target = target_vin.as_str();
    match cancellable(cancelled, || resolve_source(source, sources, host))? {
        Ok(Reading::Text(text)) if Vin::is_well_formed_text(&text) && text == target => {
            promote(target_vin)?;
        }
        Ok(Reading::Text(text)) if Vin::is_well_formed_text(&text) => {
            return Err(JobError::IdentityMismatch {
                identity: IdentityKind::Vin,
            });
        }
        Ok(_) => return not_established(IdentityKind::Vin),
        Err(error) => {
            tracing::warn!(%error, "the ECU's VIN could not be read after the restart");
            return not_established(IdentityKind::Vin);
        }
    }

    // The hardware identity.
    let (Some(recorded), Some(source)) = (
        point.facts.ecu_hardware_part_number.as_deref(),
        program.identity.hardware_part_number,
    ) else {
        return not_established(IdentityKind::HardwarePartNumber);
    };
    match cancellable(cancelled, || read_field_bytes(source, sources, host))? {
        Ok(FieldBytes::Field(bytes)) if bytes == recorded => Ok(()),
        Ok(FieldBytes::Field(_)) => Err(JobError::IdentityMismatch {
            identity: IdentityKind::HardwarePartNumber,
        }),
        Ok(_) => not_established(IdentityKind::HardwarePartNumber),
        Err(error) => {
            tracing::warn!(%error, "the ECU's hardware identity could not be read after the restart");
            not_established(IdentityKind::HardwarePartNumber)
        }
    }
}

/// The ECU state check of ADR-229 item 2 step 3b-2 (ADR-268 item 5), run in the default session
/// after `check_identity`. It reads the software version through the program's source with
/// `read_field_bytes`, at most `1 + plan.version_read_retries` times, waiting `poll` (cancel-checked,
/// sending nothing) between an inconclusive read and the next, and compares the raw bytes with
/// the journal's facts:
/// - the intended version (`intended_software_version`, if the job names one): with the
///   post-transfer steps journaled complete for the interrupted pass,
///   [`StateCheck::ReadBackVerification`]; otherwise [`StateCheck::RedoTransfer`]. "Complete for
///   the interrupted pass" means the completion is journaled and the interruption point (as
///   `interruption_point` places it) is no later than the completed pass's last post-transfer
///   step, the same test `effective_point` applies. A program that stepped back into the plan's
///   entry after the completion has a later point, so a crash in that later pass does not skip
///   it (this covers an intended version equal to the pre-erase one:
///   only the journal's record tells a finished transfer from an unstarted one);
/// - the pre-erase version (`pre_erase_software_version`): [`StateCheck::RedoTransfer`];
/// - any other decoded version: [`OnSiteReason::UnexpectedSoftwareVersion`], conclusive and not
///   retried;
/// - a negative response with the plan's declared no-application code: `RedoTransfer`,
///   conclusive;
/// - anything else (another negative response, a field that does not decode, a worker failure)
///   is inconclusive and read again; after the last read [`OnSiteReason::SoftwareVersionNotEstablished`].
///
/// A program that declares no software-version source is `SoftwareVersionNotEstablished` without
/// a read. `teardown` is carried by both on-site reasons. A cancel stops it at the start, before
/// and right after every read. Nothing here sends anything but ReadDataByIdentifier requests
/// through the declared source.
#[expect(
    clippy::too_many_arguments,
    reason = "the restart's whole context, split only by what the caller owns"
)]
pub(crate) fn check_state<H>(
    program: &Program,
    point: &RestartPoint,
    plan: &FlashRecovery,
    teardown: &Teardown,
    sources: &ServiceSources,
    host: &mut H,
    poll: Duration,
    cancelled: &AtomicBool,
) -> Result<StateCheck, JobError>
where
    H: DiagHost<Error = HostError>,
{
    let on_site = |reason| Err(JobError::OnSiteInterventionRequired(reason));
    let not_established = || {
        on_site(OnSiteReason::SoftwareVersionNotEstablished {
            flash_session: point.flash_session,
            teardown: teardown.clone(),
        })
    };
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    let Some(source) = program.identity.software_version else {
        return not_established();
    };
    let facts = &point.facts;
    let post_transfer_complete = completed_pass_interrupted(facts);
    let reads = u32::from(plan.version_read_retries) + 1;
    for read in 0..reads {
        if read > 0 {
            // Not before the first read, and not after the last: only between reads.
            wait_quietly(poll, poll, cancelled)?;
        }
        match cancellable(cancelled, || read_field_bytes(source, sources, host))? {
            Ok(FieldBytes::Field(bytes)) => {
                let version = Some(bytes.as_slice());
                return if version == facts.intended_software_version.as_deref() {
                    Ok(if post_transfer_complete {
                        StateCheck::ReadBackVerification
                    } else {
                        StateCheck::RedoTransfer
                    })
                } else if version == facts.pre_erase_software_version.as_deref() {
                    Ok(StateCheck::RedoTransfer)
                } else {
                    on_site(OnSiteReason::UnexpectedSoftwareVersion {
                        flash_session: point.flash_session,
                        teardown: teardown.clone(),
                    })
                };
            }
            Ok(FieldBytes::Negative(nrc))
                if plan.no_application == Some(NoApplication::Nrc(nrc)) =>
            {
                return Ok(StateCheck::RedoTransfer);
            }
            Ok(other) => {
                tracing::warn!(?other, "the ECU's software version was not established");
            }
            Err(error) => {
                tracing::warn!(%error, "the ECU's software version could not be read");
            }
        }
    }
    not_established()
}

/// Whether the interruption is at the end of a pass whose post-transfer steps are journaled
/// complete: the completion is recorded and the interruption point is no later than the
/// completed pass's last post-transfer step (the test of `effective_point`). A step back into the
/// plan after the completion puts the point later, so this is false for a later pass.
fn completed_pass_interrupted(facts: &RecoveryFacts) -> bool {
    let Some(exit) = facts
        .transfer
        .as_ref()
        .and_then(|transfer| transfer.exit.as_ref())
        .filter(|exit| exit.complete)
    else {
        return false;
    };
    let last = exit.last_post_step.unwrap_or(exit.intent_at);
    interruption_point(facts).is_some_and(|at| at.steps <= last.steps)
}

/// The restart's teardown (ADR-229 item 2 step 2b-1, ADR-264), run on the gates' decision `gate`.
/// In this order:
/// - the journal's exclusions, which apply on top of a `ResetAllowed` gate: post-transfer steps
///   journaled complete (the completed path) send nothing and wait for nothing
///   ([`Teardown::CompletedPath`]); a RequestTransferExit intent without that completion makes
///   the teardown passive ([`PassiveCause::TransferExitJournaled`]);
/// - a `PassiveOnly` gate makes it passive ([`PassiveCause::Gate`]);
/// - otherwise an ECUReset (hardReset, positive response required) is sent. A positive response
///   ends in [`Teardown::Reset`] with no wait. A negative response, a failed or unanswered
///   request, and any other answer make the teardown passive
///   ([`PassiveCause::ResetRefused`], [`PassiveCause::ResetOutcomeUnknown`]).
///
/// A passive teardown waits `timing.session_timeout_millis + timing.teardown_margin_millis` and
/// sends nothing further meanwhile (after a refused or unknown reset, the reset was the last
/// request): the agent runs no TesterPresent, so there is none to stop. The wait
/// sleeps in steps of `poll` and checks `cancelled` at the start of each; a cancel ends it in
/// [`JobError::Cancelled`]. A cancel set when the teardown starts ends it before anything, on
/// the completed path too, and sends no reset; one that arrives while
/// the reset is on its way does not hide an accepted reset.
pub(crate) fn teardown<H>(
    gate: TeardownGate,
    point: &RestartPoint,
    timing: &RecoveryTiming,
    host: &mut H,
    poll: Duration,
    cancelled: &AtomicBool,
) -> Result<Teardown, JobError>
where
    H: DiagHost<Error = HostError>,
{
    // Nothing has been sent yet, so a cancel ends the teardown here on every path.
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    let exit = point
        .facts
        .transfer
        .as_ref()
        .and_then(|transfer| transfer.exit.as_ref());
    let cause = if exit.is_some_and(|exit| exit.complete) {
        return Ok(Teardown::CompletedPath);
    } else if exit.is_some() {
        PassiveCause::TransferExitJournaled
    } else if let TeardownGate::PassiveOnly(reason) = gate {
        PassiveCause::Gate(reason)
    } else {
        // ECUReset, sub-function hardReset; the suppress bit is not set, so a positive
        // response comes back. Once it is sent, a cancel no longer hides its outcome: an
        // accepted reset is reported as such, and a cancel ends only the wait that follows a
        // refused or unknown one.
        match host.service_request(0x11, &[0x01]) {
            Ok(response) => match response.as_slice() {
                // The positive response of a hard reset carries nothing after its sub-function.
                [0x51, 0x01] => return Ok(Teardown::Reset),
                [0x7F, 0x11, nrc] if *nrc != 0x78 => PassiveCause::ResetRefused { nrc: *nrc },
                other => {
                    tracing::warn!(answer = ?other, "the ECUReset got no usable answer");
                    PassiveCause::ResetOutcomeUnknown
                }
            },
            Err(error) => {
                tracing::warn!(%error, "the ECUReset got no usable answer");
                PassiveCause::ResetOutcomeUnknown
            }
        }
    };
    passive_wait(timing, poll, cancelled)?;
    Ok(Teardown::Passive(cause))
}

/// The passive teardown's wait: `timing.session_timeout_millis + timing.teardown_margin_millis`,
/// sending nothing. It sleeps in steps of `poll`, checking `cancelled` at the start of each.
fn passive_wait(
    timing: &RecoveryTiming,
    poll: Duration,
    cancelled: &AtomicBool,
) -> Result<(), JobError> {
    wait_quietly(
        Duration::from_millis(
            u64::from(timing.session_timeout_millis) + u64::from(timing.teardown_margin_millis),
        ),
        poll,
        cancelled,
    )
}

/// Sleeps for `total` in steps of `poll`, sending nothing. A cancel, checked at the start of
/// every step and when `total` is zero, ends it in [`JobError::Cancelled`].
fn wait_quietly(total: Duration, poll: Duration, cancelled: &AtomicBool) -> Result<(), JobError> {
    let deadline = Instant::now() + total;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(JobError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(poll));
    }
}

/// The default-session confirmation (ADR-229 item 2 step 2b-2, ADR-265) after `teardown` ended
/// the interrupted download of plan `flash_session`. One attempt waits
/// `timing.ecu_startup_millis` sending nothing (the ECU may be restarting, after the teardown's
/// reset or a reset of the procedure that was journaled), then reads DID F186 until it reports
/// the default session or `timing.confirmation_window_millis` (counted from the end of the
/// startup wait) has passed, sleeping `poll` between reads. At least one read is made. Only the
/// positive response carrying the default session value confirms; a negative response, no answer,
/// another session and any other answer are failed reads, retried while the window lasts.
///
/// The first attempt that fails makes:
/// - after [`Teardown::Reset`] and [`Teardown::CompletedPath`], where no passive wait has run:
///   the passive teardown (the wait of `teardown`) and one more attempt, whose result is final;
/// - after [`Teardown::Passive`], where that wait has run: the failure.
///
/// A failure is [`OnSiteReason::DefaultSessionNotConfirmed`]. The waits sleep in steps of `poll`;
/// `cancelled` is checked before every wait step and every read, and a cancel is
/// [`JobError::Cancelled`]. No VIN is put in a log message or a result.
pub(crate) fn confirm_default_session<H>(
    flash_session: u32,
    teardown: Teardown,
    timing: &RecoveryTiming,
    host: &mut H,
    poll: Duration,
    cancelled: &AtomicBool,
) -> Result<Confirmation, JobError>
where
    H: DiagHost<Error = HostError>,
{
    if confirmation_attempt(timing, host, poll, cancelled)? {
        return Ok(Confirmation {
            after_passive_retry: false,
        });
    }
    if !matches!(teardown, Teardown::Passive(_)) {
        passive_wait(timing, poll, cancelled)?;
        if confirmation_attempt(timing, host, poll, cancelled)? {
            return Ok(Confirmation {
                after_passive_retry: true,
            });
        }
    }
    Err(JobError::OnSiteInterventionRequired(
        OnSiteReason::DefaultSessionNotConfirmed {
            flash_session,
            teardown,
        },
    ))
}

/// One confirmation attempt of [`confirm_default_session`]: the startup wait, then the windowed
/// F186 reads. True when a read reported the default session.
fn confirmation_attempt<H>(
    timing: &RecoveryTiming,
    host: &mut H,
    poll: Duration,
    cancelled: &AtomicBool,
) -> Result<bool, JobError>
where
    H: DiagHost<Error = HostError>,
{
    wait_quietly(
        Duration::from_millis(timing.ecu_startup_millis.into()),
        poll,
        cancelled,
    )?;
    let deadline = Instant::now() + Duration::from_millis(timing.confirmation_window_millis.into());
    let mut failed_reads = 0u32;
    loop {
        // ReadDataByIdentifier F186 (ActiveDiagnosticSession); 0x01 is the default session.
        // The cancel is checked before the read and, so that it wins over the answer, right
        // after it.
        match cancellable(cancelled, || host.service_request(0x22, &[0xF1, 0x86]))? {
            Ok(response) if response == [0x62, 0xF1, 0x86, 0x01] => return Ok(true),
            Ok(other) => log_failed_read(failed_reads, format_args!("answer {other:02X?}")),
            Err(error) => log_failed_read(failed_reads, format_args!("{error}")),
        }
        failed_reads += 1;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::warn!(
                failed_reads,
                "the default session was not confirmed within the window"
            );
            return Ok(false);
        }
        std::thread::sleep(remaining.min(poll));
    }
}

/// Logs a failed F186 read: the first of an attempt at warn, later ones at debug, so a long
/// window does not flood the log. `failed_before` counts the attempt's earlier failed reads.
fn log_failed_read(failed_before: u32, what: std::fmt::Arguments<'_>) {
    if failed_before == 0 {
        tracing::warn!("the session read did not confirm the default session: {what}");
    } else {
        tracing::debug!("the session read did not confirm the default session: {what}");
    }
}

/// Whether `precondition` holds: the first source that gives a value decides, the
/// default-session one before the programming-session one (skipped when it is the same source).
/// Only a reading that is not a value falls back to the other source; a failure to use the
/// worker fails the gate at once (ADR-261 item 5).
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
                return Ok(false);
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

    use std::sync::Arc;

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

    // ------------------------------------------------------------ teardown against sim-ecu

    /// A `DiagHost` over a simulated ECU: a request it does not answer is `NoResponse`. Only
    /// `service_request` is used by the teardown.
    struct SimHost(sim_ecu::SimEcu, Option<Arc<AtomicBool>>);

    impl DiagHost for SimHost {
        type Error = HostError;

        fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, HostError> {
            let mut request = vec![u8::try_from(service).unwrap()];
            request.extend_from_slice(payload);
            let response = self.0.request(&request).to_bytes();
            // A test's cancel that arrives while the request is on its way.
            if let Some(cancel) = &self.1 {
                cancel.store(true, Ordering::Relaxed);
            }
            response.ok_or(HostError::NoResponse)
        }
        fn read_dtc(&mut self, _: u8) -> Result<Vec<u8>, HostError> {
            Err(HostError::Unsupported("ReadDtc"))
        }
        fn routine_control(&mut self, _: u16, _: u8, _: &[u8]) -> Result<Vec<u8>, HostError> {
            Err(HostError::Unsupported("RoutineControl"))
        }
        fn security_access(
            &mut self,
            _: u64,
            _: u8,
            _: &[u8],
        ) -> Result<Option<Vec<u8>>, HostError> {
            Err(HostError::Unsupported("SecurityAccess"))
        }
        fn flash_transfer(&mut self, _: u32, _: &[u8]) -> Result<(), HostError> {
            Err(HostError::Unsupported("FlashTransfer"))
        }
        fn wait(&mut self, _: u64, _: u32) -> Result<bool, HostError> {
            Ok(true)
        }
        fn hmi_request(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, HostError> {
            Ok(None)
        }
        fn record_input(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, HostError> {
            Err(HostError::Unsupported("RecordInput"))
        }
        fn monitor_capture(&mut self, _: u32) -> Result<(), HostError> {
            Err(HostError::Unsupported("MonitorCapture"))
        }
        fn log(&mut self, _: u8, _: &str) {}
    }

    /// A simulated ECU in its extended session, as an interrupted download leaves it.
    fn ecu_in_session() -> SimHost {
        ecu_in_session_with(sim_ecu::EcuConfig::default())
    }

    /// [`ecu_in_session`] with `config`.
    fn ecu_in_session_with(config: sim_ecu::EcuConfig) -> SimHost {
        let mut ecu = sim_ecu::SimEcu::new(config);
        let response = ecu.request(&[0x10, 0x03]);
        assert!(
            matches!(response, sim_ecu::SimResponse::Positive(_)),
            "{response:?}"
        );
        assert_eq!(ecu.session, sim_ecu::Session::Extended);
        SimHost(ecu, None)
    }

    /// The restart point of a journal interrupted at the erase, or at RequestTransferExit.
    fn point_at(upto: u32) -> (Program, RestartPoint) {
        let program = program(RecoveryRequired::Never);
        let j = journal_until(&program, upto);
        let RestartDecision::Restart(point) = decide(&program, &j) else {
            panic!("a restart point");
        };
        (program, *point)
    }

    const TIMING: RecoveryTiming = RecoveryTiming {
        session_timeout_millis: 15,
        teardown_margin_millis: 5,
        ecu_startup_millis: 0,
        confirmation_window_millis: 0,
    };

    fn run_teardown(
        gate: TeardownGate,
        point: &RestartPoint,
        host: &mut SimHost,
    ) -> (Result<Teardown, JobError>, Duration) {
        let started = Instant::now();
        let result = teardown(
            gate,
            point,
            &TIMING,
            host,
            Duration::from_millis(1),
            &AtomicBool::new(false),
        );
        (result, started.elapsed())
    }

    #[test]
    fn an_accepted_ecu_reset_power_cycles_the_simulated_ecu() {
        let (_, point) = point_at(ERASE);
        let mut host = ecu_in_session();
        let (result, elapsed) = run_teardown(TeardownGate::ResetAllowed, &point, &mut host);
        assert_eq!(result.unwrap(), Teardown::Reset);
        assert_eq!(host.0.power_cycles(), 1);
        assert_eq!(host.0.session, sim_ecu::Session::Default);
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }

    /// A cancel set before the teardown ends it on the completed path too, where nothing would be
    /// sent anyway.
    #[test]
    fn a_cancel_before_the_teardown_ends_the_completed_path() {
        let (_, point) = point_at(EXIT);
        let mut point = point;
        point
            .facts
            .transfer
            .as_mut()
            .and_then(|transfer| transfer.exit.as_mut())
            .expect("a journal at RequestTransferExit has an exit")
            .complete = true;
        let mut host = ecu_in_session();
        let result = teardown(
            TeardownGate::ResetAllowed,
            &point,
            &TIMING,
            &mut host,
            Duration::from_millis(1),
            &AtomicBool::new(true),
        );
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
        let result = teardown(
            TeardownGate::ResetAllowed,
            &point,
            &TIMING,
            &mut host,
            Duration::from_millis(1),
            &AtomicBool::new(false),
        );
        assert_eq!(result.unwrap(), Teardown::CompletedPath);
    }

    /// A cancel that arrives while an F186 read is on its way wins over a read that confirms.
    #[test]
    fn a_cancel_during_a_confirming_read_cancels() {
        let cancelled = Arc::new(AtomicBool::new(false));
        // A fresh simulated ECU is in its default session, so the read would confirm.
        let mut host = SimHost(
            sim_ecu::SimEcu::new(sim_ecu::EcuConfig::default()),
            Some(Arc::clone(&cancelled)),
        );
        let result = confirm_default_session(
            1,
            Teardown::CompletedPath,
            &TIMING,
            &mut host,
            Duration::from_millis(1),
            &cancelled,
        );
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
    }

    /// A cancel set before the teardown sends no reset.
    #[test]
    fn a_cancel_before_the_reset_sends_none() {
        let (_, point) = point_at(ERASE);
        let mut host = ecu_in_session();
        let result = teardown(
            TeardownGate::ResetAllowed,
            &point,
            &TIMING,
            &mut host,
            Duration::from_millis(1),
            &AtomicBool::new(true),
        );
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
        assert_eq!(host.0.power_cycles(), 0);
        assert_eq!(host.0.session, sim_ecu::Session::Extended);
    }

    /// A cancel that arrives while the reset is on its way does not hide that the ECU accepted
    /// it; after a refused reset it ends the wait.
    #[test]
    fn a_cancel_during_the_reset_keeps_its_outcome() {
        let (_, point) = point_at(ERASE);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut host = ecu_in_session();
        host.1 = Some(Arc::clone(&cancelled));
        let result = teardown(
            TeardownGate::ResetAllowed,
            &point,
            &TIMING,
            &mut host,
            Duration::from_millis(1),
            &cancelled,
        );
        assert_eq!(result.unwrap(), Teardown::Reset);
        assert_eq!(host.0.power_cycles(), 1);

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut host = ecu_in_session();
        host.0.inject(sim_ecu::Fault::NegativeResponse {
            nrc: sim_ecu::Nrc::ConditionsNotCorrect,
        });
        host.1 = Some(Arc::clone(&cancelled));
        let result = teardown(
            TeardownGate::ResetAllowed,
            &point,
            &TIMING,
            &mut host,
            Duration::from_millis(1),
            &cancelled,
        );
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
    }

    /// Timeout and margin are added without wrapping or saturating at 32 bits.
    #[test]
    fn the_passive_wait_is_not_cut_short_by_large_timings() {
        let (_, point) = point_at(ERASE);
        let timing = RecoveryTiming {
            session_timeout_millis: u32::MAX,
            teardown_margin_millis: u32::MAX,
            ..TIMING
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let canceller = {
            let cancelled = Arc::clone(&cancelled);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                cancelled.store(true, Ordering::Relaxed);
            })
        };
        let mut host = ecu_in_session();
        let result = teardown(
            TeardownGate::PassiveOnly(PassiveReason::VinNotEstablished),
            &point,
            &timing,
            &mut host,
            Duration::from_millis(1),
            &cancelled,
        );
        canceller.join().unwrap();
        // Still waiting when cancelled: the wait was not computed as zero or wrapped short.
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
    }

    #[test]
    fn a_refused_ecu_reset_leaves_the_simulated_ecu_and_waits() {
        let (_, point) = point_at(ERASE);
        let mut host = ecu_in_session();
        host.0.inject(sim_ecu::Fault::NegativeResponse {
            nrc: sim_ecu::Nrc::ConditionsNotCorrect,
        });
        let (result, elapsed) = run_teardown(TeardownGate::ResetAllowed, &point, &mut host);
        assert_eq!(
            result.unwrap(),
            Teardown::Passive(PassiveCause::ResetRefused { nrc: 0x22 })
        );
        assert_eq!(host.0.power_cycles(), 0);
        assert_eq!(host.0.session, sim_ecu::Session::Extended);
        assert!(elapsed >= Duration::from_millis(20), "{elapsed:?}");
    }

    #[test]
    fn an_unanswered_ecu_reset_is_an_unknown_outcome() {
        for fault in [sim_ecu::Fault::BusError, sim_ecu::Fault::DropResponse] {
            let (_, point) = point_at(ERASE);
            let mut host = ecu_in_session();
            host.0.inject(fault);
            let (result, elapsed) = run_teardown(TeardownGate::ResetAllowed, &point, &mut host);
            assert_eq!(
                result.unwrap(),
                Teardown::Passive(PassiveCause::ResetOutcomeUnknown),
                "{fault:?}"
            );
            assert!(
                elapsed >= Duration::from_millis(20),
                "{fault:?}: {elapsed:?}"
            );
            // A bus error loses the request; a dropped response still carried out the reset.
            let cycles = u64::from(fault == sim_ecu::Fault::DropResponse);
            assert_eq!(host.0.power_cycles(), cycles, "{fault:?}");
        }
    }

    /// With a RequestTransferExit journaled, or a passive gate, nothing reaches the ECU.
    #[test]
    fn no_request_reaches_the_simulated_ecu_when_the_teardown_is_passive() {
        let (_, exit_point) = point_at(EXIT);
        let (_, erase_point) = point_at(ERASE);
        let cases = [
            (
                TeardownGate::ResetAllowed,
                &exit_point,
                PassiveCause::TransferExitJournaled,
            ),
            (
                TeardownGate::PassiveOnly(PassiveReason::VinNotEstablished),
                &erase_point,
                PassiveCause::Gate(PassiveReason::VinNotEstablished),
            ),
        ];
        for (gate, point, cause) in cases {
            // The ECU's session timeout is the one the plan declares, so the wait outlasts it.
            let mut host = ecu_in_session_with(sim_ecu::EcuConfig {
                s3_server_ms: Some(TIMING.session_timeout_millis),
                ..sim_ecu::EcuConfig::default()
            });
            // Armed to show whether any request reaches the ECU: it stays armed if none does.
            host.0.inject(sim_ecu::Fault::BusError);
            let (result, elapsed) = run_teardown(gate, point, &mut host);
            assert_eq!(result.unwrap(), Teardown::Passive(cause));
            assert_eq!(host.0.armed_faults(), &[sim_ecu::Fault::BusError]);
            assert!(elapsed >= Duration::from_millis(20), "{elapsed:?}");
            // The passive teardown let the session run out: the ECU is back in its default
            // session without any request.
            host.0.check_timers();
            assert_eq!(host.0.session, sim_ecu::Session::Default);
        }
    }

    // ------------------------------------------------ confirmation against sim-ecu

    /// Short timings for the confirmation: startup 10 ms, window 2 s, passive wait 20 ms.
    const CONFIRM_TIMING: RecoveryTiming = RecoveryTiming {
        ecu_startup_millis: 10,
        confirmation_window_millis: 2_000,
        ..TIMING
    };

    fn confirm(
        teardown: Teardown,
        timing: &RecoveryTiming,
        host: &mut SimHost,
        cancelled: &AtomicBool,
    ) -> Result<Confirmation, JobError> {
        confirm_default_session(
            1,
            teardown,
            timing,
            host,
            Duration::from_millis(1),
            cancelled,
        )
    }

    fn not_confirmed(result: Result<Confirmation, JobError>) -> Teardown {
        match result {
            Err(JobError::OnSiteInterventionRequired(
                OnSiteReason::DefaultSessionNotConfirmed {
                    flash_session: 1,
                    teardown,
                },
            )) => teardown,
            other => panic!("{other:?}"),
        }
    }

    const CONFIRMED: Confirmation = Confirmation {
        after_passive_retry: false,
    };

    /// An ECU that refuses the reset leaves its session to expire during the passive teardown,
    /// and is confirmed afterwards.
    #[test]
    fn a_passive_teardown_then_the_default_session_is_confirmed() {
        let (_, point) = point_at(ERASE);
        let mut host = ecu_in_session_with(sim_ecu::EcuConfig {
            s3_server_ms: Some(TIMING.session_timeout_millis),
            ..sim_ecu::EcuConfig::default()
        });
        host.0.inject(sim_ecu::Fault::NegativeResponse {
            nrc: sim_ecu::Nrc::ConditionsNotCorrect,
        });
        let (result, _) = run_teardown(TeardownGate::ResetAllowed, &point, &mut host);
        let teardown = result.unwrap();
        assert_eq!(
            teardown,
            Teardown::Passive(PassiveCause::ResetRefused { nrc: 0x22 })
        );
        let confirmed = confirm(
            teardown,
            &CONFIRM_TIMING,
            &mut host,
            &AtomicBool::new(false),
        );
        assert_eq!(confirmed.unwrap(), CONFIRMED);
    }

    /// The ECU acknowledges the reset and is silent for longer than the declared startup time,
    /// within the window: the retried reads still confirm.
    #[test]
    fn an_ecu_silent_after_acknowledging_the_reset_is_still_confirmed() {
        let (_, point) = point_at(ERASE);
        let mut host = ecu_in_session_with(sim_ecu::EcuConfig {
            startup_ms: Some(100),
            ..sim_ecu::EcuConfig::default()
        });
        let (result, _) = run_teardown(TeardownGate::ResetAllowed, &point, &mut host);
        assert_eq!(result.unwrap(), Teardown::Reset);
        // The declared startup covers the ECU's silence, so the startup wait alone takes the
        // confirmation past it: without that wait the reads would succeed earlier.
        let timing = RecoveryTiming {
            ecu_startup_millis: 150,
            ..CONFIRM_TIMING
        };
        let started = Instant::now();
        let confirmed = confirm(Teardown::Reset, &timing, &mut host, &AtomicBool::new(false));
        assert_eq!(confirmed.unwrap(), CONFIRMED);
        assert!(started.elapsed() >= Duration::from_millis(150));
    }

    /// The ECUReset's response is lost while the ECU restarts: the outcome is unknown, the
    /// teardown passive, and the ECU is confirmed in its default session.
    #[test]
    fn a_lost_reset_response_while_the_ecu_restarts_is_confirmed_after_the_passive_teardown() {
        let (_, point) = point_at(ERASE);
        let mut host = ecu_in_session_with(sim_ecu::EcuConfig {
            startup_ms: Some(30),
            ..sim_ecu::EcuConfig::default()
        });
        host.0.inject(sim_ecu::Fault::DropResponse);
        let (result, _) = run_teardown(TeardownGate::ResetAllowed, &point, &mut host);
        let teardown = result.unwrap();
        assert_eq!(
            teardown,
            Teardown::Passive(PassiveCause::ResetOutcomeUnknown)
        );
        assert_eq!(host.0.power_cycles(), 1);
        let timing = RecoveryTiming {
            ecu_startup_millis: 80,
            ..CONFIRM_TIMING
        };
        let started = Instant::now();
        let confirmed = confirm(teardown, &timing, &mut host, &AtomicBool::new(false));
        assert_eq!(confirmed.unwrap(), CONFIRMED);
        // The startup wait is not skipped, whatever the silence left.
        assert!(started.elapsed() >= Duration::from_millis(80));
    }

    /// The completed path after the procedure's own ECUReset and an agent crash: the ECU is
    /// still starting up when the restart begins, and the startup wait and the retried reads
    /// confirm it.
    #[test]
    fn the_completed_path_after_a_procedure_reset_waits_for_the_startup() {
        let mut host = ecu_in_session_with(sim_ecu::EcuConfig {
            startup_ms: Some(120),
            ..sim_ecu::EcuConfig::default()
        });
        let started = Instant::now();
        // The procedure's reset, whose recording was the last thing the first run did.
        assert!(matches!(
            host.0.request(&[0x11, 0x01]),
            sim_ecu::SimResponse::Positive(_)
        ));
        let timing = RecoveryTiming {
            ecu_startup_millis: 150,
            ..CONFIRM_TIMING
        };
        let confirmed = confirm(
            Teardown::CompletedPath,
            &timing,
            &mut host,
            &AtomicBool::new(false),
        );
        assert_eq!(confirmed.unwrap(), CONFIRMED);
        assert!(started.elapsed() >= Duration::from_millis(150));
    }

    /// An ECU that stays in a non-default session or never answers cannot be confirmed; the
    /// failure names the teardown.
    #[test]
    fn an_ecu_that_cannot_be_confirmed_needs_on_site_intervention() {
        let window = RecoveryTiming {
            confirmation_window_millis: 30,
            ..CONFIRM_TIMING
        };
        // The refused reset left the extended session running (default tS3_Server).
        let (_, point) = point_at(ERASE);
        let mut host = ecu_in_session();
        host.0.inject(sim_ecu::Fault::NegativeResponse {
            nrc: sim_ecu::Nrc::ConditionsNotCorrect,
        });
        let (result, _) = run_teardown(TeardownGate::ResetAllowed, &point, &mut host);
        let teardown = result.unwrap();
        let failed = confirm(
            teardown.clone(),
            &window,
            &mut host,
            &AtomicBool::new(false),
        );
        assert_eq!(not_confirmed(failed), teardown);

        // An ECU that answers nothing at all, after the reset path and on the completed path.
        for teardown in [Teardown::Reset, Teardown::CompletedPath] {
            let mut host = ecu_in_session();
            host.0.inject(sim_ecu::Fault::PowerLoss);
            let failed = confirm(
                teardown.clone(),
                &window,
                &mut host,
                &AtomicBool::new(false),
            );
            assert_eq!(not_confirmed(failed), teardown);
        }
    }

    /// A passive teardown that was run already is not repeated when its confirmation fails: the
    /// failure comes long before a second passive wait of this length would end.
    #[test]
    fn a_passive_teardowns_failed_confirmation_ends_at_once() {
        let timing = RecoveryTiming {
            session_timeout_millis: 60_000,
            confirmation_window_millis: 0,
            ..CONFIRM_TIMING
        };
        let teardown = Teardown::Passive(PassiveCause::ResetOutcomeUnknown);
        let mut host = ecu_in_session();
        let started = Instant::now();
        let failed = confirm(
            teardown.clone(),
            &timing,
            &mut host,
            &AtomicBool::new(false),
        );
        assert_eq!(not_confirmed(failed), teardown);
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    /// Only the positive response with the default session value confirms; a session the ECU
    /// reports otherwise is a failed read.
    #[test]
    fn a_non_default_session_does_not_confirm() {
        let window = RecoveryTiming {
            confirmation_window_millis: 0,
            ..CONFIRM_TIMING
        };
        let mut host = ecu_in_session();
        // Programming-capable extended session, as the interrupted download left it.
        let failed = confirm(
            Teardown::Passive(PassiveCause::ResetOutcomeUnknown),
            &window,
            &mut host,
            &AtomicBool::new(false),
        );
        assert!(failed.is_err());
        assert_eq!(host.0.session, sim_ecu::Session::Extended);
    }

    /// A cancel during the startup wait, and one during the read retries, end the confirmation.
    #[test]
    fn a_cancel_ends_the_startup_wait_and_the_read_retries() {
        let cases = [
            (
                RecoveryTiming {
                    ecu_startup_millis: 60_000,
                    ..CONFIRM_TIMING
                },
                false,
            ),
            (
                RecoveryTiming {
                    confirmation_window_millis: 60_000,
                    ..CONFIRM_TIMING
                },
                true,
            ),
        ];
        for (timing, silent) in cases {
            let cancelled = Arc::new(AtomicBool::new(false));
            let canceller = {
                let cancelled = Arc::clone(&cancelled);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(50));
                    cancelled.store(true, Ordering::Relaxed);
                })
            };
            let mut host = ecu_in_session();
            if silent {
                // No read is answered, so the window keeps retrying.
                host.0.inject(sim_ecu::Fault::PowerLoss);
            }
            let result = confirm(Teardown::Reset, &timing, &mut host, &cancelled);
            canceller.join().unwrap();
            assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
        }
    }

    /// A cancel set at the start sends no read.
    #[test]
    fn a_cancel_before_the_confirmation_sends_no_read() {
        let mut host = ecu_in_session();
        host.0.inject(sim_ecu::Fault::BusError);
        let result = confirm(
            Teardown::Reset,
            &CONFIRM_TIMING,
            &mut host,
            &AtomicBool::new(true),
        );
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
        assert_eq!(host.0.armed_faults(), &[sim_ecu::Fault::BusError]);
    }
}
