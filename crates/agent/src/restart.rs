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
//! read-back verification (step 3b-2, ADR-268 item 5). When it decides to redo the transfer,
//! `check_reentry` checks every declared precondition again before anything is replayed (step 4a,
//! ADR-229 item 2 step 4); after the replay reached the erase, `check_before_erase` checks them
//! once more through the programming-session sources (step 4b-3, ADR-245 item 6), and the
//! runner then goes on from the erase as a first run does (step 4c, ADR-276). When it
//! decides the read-back verification, `unsafe_continuation` and `plan_end_state` decide
//! whether the program can go on after the plan (ADR-271 items 4 and 5).
//!
//! A journal that ends in a declared failure (ADR-280) or in an identity abort (ADR-286, the
//! gates' or step 3a's mismatch, journaled by the runner) is final: `classify` decides it first
//! and the job is not resumed.
//!
//! The interruption point is the latest of the last completed step and the requests the
//! journal wrote ahead (the transfer-start and RequestTransferExit markers, and the request
//! intent of ADR-253), by step count, so a crash after a guarded request was sent but before its
//! response was recorded is placed at that request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use diag_ir::{
    DiagHost, FlashRecovery, Idempotency, IdentityKind, Interruptible, NoApplication, Op,
    Precondition, PreconditionKind, Program, RecoveryRequired, RecoveryTiming, RuntimeInput,
    Source, Vm, VmError, VmState,
};

use crate::host::{HostError, OwnRead, ProcedurePosition, ResendClass, SendState};
use crate::inputs::{FieldBytes, Reading, RuntimeInputs, ServiceSources};
use crate::inputs::{read_identity_bytes, resolve_identity, resolve_source};
use crate::journal::Vin;
use crate::journal::{
    Journal, JournalError, JournalState, RecoveryFacts, StageId, StepRef, StoppedRun, Store,
};
use crate::oplog::LogSafe;
use crate::runner::{JobError, JobLimits};

/// How a job whose journal exists goes on.
#[derive(Debug, Clone, PartialEq)]
pub enum RestartDecision {
    /// No transfer was started and nothing rules a restart out: the program runs from its start.
    PlainStart,
    /// A transfer was started: it is redone in the restart order of ADR-229 item 2.
    Restart(Box<RestartPoint>),
    /// A plain procedure (no transfer started) sent requests that changed or may have changed the
    /// ECU, all of them confirmed, and what lies ahead of its latest checkpoint is safe to run
    /// again: it goes on from the checkpoint (ADR-335 item 3).
    ResumeFromCheckpoint(Box<CheckpointResume>),
    /// The job must not go on automatically (design 5.6, 8.10.1).
    OnSiteInterventionRequired(OnSiteReason),
    /// The procedure ended itself as failed (`Op::Fail`, ADR-280): the job is over and is not
    /// resumed, whatever its transfer state. Nothing is sent and no resume is counted.
    Failed {
        at: StepRef,
        code: String,
        detail: String,
    },
    /// A resume ended on a failed identity check and journaled it (ADR-286): the job is over and
    /// is not resumed, whatever its transfer state. `identity` is the one that differed. Nothing
    /// is sent and no resume is counted.
    Aborted { identity: IdentityKind },
    /// A run stopped on the step limit or on an instruction the VM refused, and journaled it
    /// (ADR-288): the job is over and is not resumed, whatever its transfer state. Nothing is
    /// sent and no resume is counted.
    Stopped(StoppedRun),
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

/// What a checkpoint resume starts from (ADR-335 item 3).
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointResume {
    /// The journal step of the checkpoint record the state comes from.
    pub at: StepRef,
    /// The VM state of that checkpoint, which the resumed run starts from. It stands at the step
    /// about to run, was checked with `Vm::check_state` against the program, and has its step
    /// count set to [`next_steps`], since the journal orders records by it and refuses one that
    /// does not come after the last.
    pub state: VmState,
}

/// Why a plain procedure's journal cannot be resumed from its latest checkpoint (ADR-335
/// item 3), in the order `classify` checks the conditions: the point is interruptible, nothing
/// ahead may not be repeated, and the vehicle is identified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotResumableCause {
    /// A diagnostic primitive reachable from the checkpoint is not safe to repeat
    /// ([`safe_to_repeat`]); `pc` is the lowest such one. It may be reachable only through a
    /// loop back or a call.
    UnsafePrimitiveAhead { pc: u32 },
    /// A diagnostic primitive reachable from the checkpoint lies in a `RecoveryRequired`
    /// section (`section` indexes `Program::sections`); `pc` is the lowest such one.
    RecoveryRequiredAhead { section: usize, pc: u32 },
    /// An instruction of a flash recovery plan's range is reachable from the checkpoint: a plan
    /// has its own journal and restart order, which a continuation from a plain checkpoint must
    /// not enter.
    PlanAhead { flash_session: u32 },
    /// The checkpoint stands where a cancel is not taken (ADR-330, [`interruptible_point`]):
    /// inside a section that is not `Yes`, or on a `Ret`, which is not an interruptible point
    /// either (ADR-330 item 2). A plain procedure declares no re-entry point for such a section
    /// (ADR-335 item 5), and ADR-335 item 3 gives a checkpoint on the return after a `No`
    /// section's last primitive this cause too.
    UninterruptibleSection { pc: u32 },
    /// The job names no target VIN or the program declares no VIN source, so a resume could not
    /// tell that it runs against the vehicle the procedure started on (ADR-335 item 3,
    /// ADR-261).
    NoIdentity,
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
    /// The VM state at the entry does not decode, or fails `Vm::check_state` (ADR-245 item 7). A
    /// plain procedure's latest checkpoint state that does not decode or fails the check is this
    /// too (ADR-335 item 3).
    InvalidEntryState(String),
    /// A plain procedure sent a request ahead of a primitive that is not safe to repeat
    /// (`Request`, ADR-335 item 2) and the journal holds no state newer than it, so that
    /// primitive may have reached the ECU and its outcome is unknown (ADR-335 items 3 and 4).
    /// `at` is the request. It holds for every later resume until a person acts.
    UnconfirmedRequest { at: StepRef },
    /// A plain procedure that sent requests cannot be resumed from its latest checkpoint
    /// (ADR-335 items 3 and 5); `cause` says which condition fails.
    NotResumable { cause: NotResumableCause },
    /// A plain procedure was started again or resumed as often as the agent allows
    /// (`JobLimits::max_procedure_resumes`, ADR-335 item 6): `resumes` were counted under the
    /// plain procedure's stage. Nothing is sent and no resume is counted.
    ProcedureResumeLimitReached { resumes: u16, max: u16 },
    /// A checkpoint resume of a plain procedure could not establish the vehicle (ADR-335 items 6
    /// and 7, ADR-261): the VIN read through the program's source gave no answer, a negative
    /// response, a value that is not a well-formed VIN, or failed in the worker. Nothing but that
    /// read was sent. The resume stays counted, and the job can be resumed again within the limit
    /// (ADR-290). A job with no well-formed target VIN, or a program with no source, never gets
    /// this far, since classification makes it `NotResumable` (`NoIdentity`); the check keeps the
    /// same ending for it defensively. A well-formed VIN that differs is not this reason but
    /// [`JobError::IdentityMismatch`].
    ProcedureIdentityNotEstablished,
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
    /// The resume was given another flash artifact than the journal recorded at the job's first
    /// run (none on one side included), and the journal had reached a flash plan: it holds a
    /// transfer-start marker, or its last step or request intent lies within a plan's range, so
    /// the erase may already have destroyed the application and the journal's transfer facts
    /// belong to the first file. Nothing is sent and no resume is counted (ADR-344). Before a plan the same
    /// difference is [`JobError::ArtifactDiffersFromJournal`].
    ArtifactDiffers,
    /// Test builds only: the restart order of a redone transfer passed steps 1 to 4b-3 and the
    /// tests' `resume_to_erase` stopped it there, before the erase (step 4c), so the tests can see
    /// the teardown's outcome (`teardown`), the default-session confirmation (`confirmed`) and the
    /// state check (`state`, always [`StateCheck::RedoTransfer`]). A job never ends in it: the
    /// runner goes on with the erase (ADR-276).
    #[cfg(test)]
    StoppedBeforeErase {
        flash_session: u32,
        teardown: Teardown,
        confirmed: Confirmation,
        state: StateCheck,
    },
    /// The read-back verification passed (step 3b-2 found the intended software version and the
    /// journal the post-transfer steps complete, ADR-271 item 1), so the image is verified, but
    /// the program cannot go on after the plan (ADR-271 item 4, `unsafe_continuation`): the
    /// diagnostic primitive at `pc` may have reached the ECU before the interruption and must not
    /// be sent again. It is outside every plan's range, or in the plan that begins at this plan's
    /// end, before its erase or its recovery-required point, whichever comes first, and in no
    /// section, in one that is not `Safe`, or in one marked
    /// `RecoveryRequired` (for that next plan, when it allows a restart, only under an `Unsafe`
    /// section). Nothing after the verification was sent.
    /// `teardown` is how the interrupted download was ended (step 2b-1) and `confirmed` how the
    /// default session was confirmed (step 2b-2).
    UnsafeContinuation {
        flash_session: u32,
        pc: u32,
        teardown: Teardown,
        confirmed: Confirmation,
    },
    /// The read-back verification passed, so the image is verified, but the journal holds no
    /// usable VM state at the plan's end to go on from (ADR-271 item 5, ADR-274 item 3): none
    /// was recorded, a state recorded since (an entry state or a run start) made it stale, or it
    /// does not pass `Vm::check_state`. Nothing after the verification was sent. `teardown` and
    /// `confirmed` are the outcomes of steps 2b-1 and 2b-2.
    MissingPlanEndState {
        flash_session: u32,
        teardown: Teardown,
        confirmed: Confirmation,
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
    /// A precondition the program declares did not hold at the start of step 4 (step 4a,
    /// `check_reentry`; ADR-229 item 2 step 4, design 8.9): its default-session source gave a
    /// value outside the declared range, no value, or could not be used. Nothing of step 4 was
    /// replayed. `precondition` is the first that failed; `teardown` is how the download was
    /// ended first.
    PreconditionNotMet {
        flash_session: u32,
        precondition: PreconditionKind,
        teardown: Teardown,
    },
    /// A precondition the program declares did not hold in the second check immediately before
    /// the erase (step 4b-3, `check_before_erase`; ADR-229 item 2 step 4, ADR-245 item 6): its
    /// programming-session source gave a value outside the declared range, no value, or could
    /// not be used, or the program declares none. Unlike [`OnSiteReason::PreconditionNotMet`], the
    /// plan's steps from its entry were replayed (step 4b-2, which normally puts the ECU in its
    /// programming session), and no erase was sent. `precondition` is the first that failed; `teardown` is
    /// how the interrupted download was ended before the replay.
    PreconditionNotMetBeforeErase {
        flash_session: u32,
        precondition: PreconditionKind,
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
    /// software version: the transfer is not redone. The match is the read-back verification
    /// (ADR-268 item 5, ADR-271 item 1), and the program goes on after the plan (ADR-271 item 3).
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
/// for a program with no flash recovery plan, no `RecoveryRequired` section and no primitive that
/// is not safe to repeat ([`missing_journal_is_plain_start`]). For any other it is on-site
/// intervention: the runner creates the journal before it sends anything, so a missing one cannot
/// be told from one lost after an erase, inside the section or after a request that changed the
/// ECU (ADR-293, ADR-335 item 1). A journal that holds a procedure failure is
/// decided first, as [`RestartDecision::Failed`], whatever else it holds (ADR-280); one that holds an
/// identity abort is decided next, as [`RestartDecision::Aborted`] (ADR-286), and one that holds a
/// run's stop after that, as [`RestartDecision::Stopped`] (ADR-288). A journal with a started
/// transfer takes the flash restart order; one without is a plain procedure's and goes through
/// [`classify_plain_procedure`] (ADR-335 item 3).
pub fn classify(
    program: &Program,
    journal: Result<&JournalState, &JournalError>,
) -> RestartDecision {
    let state = match journal {
        Ok(state) => state,
        Err(JournalError::NotFound) if missing_journal_is_plain_start(program) => {
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
    // A declared failure is final: it is checked before anything that could restart the job
    // (ADR-280).
    if let Some(failure) = &facts.procedure_failed {
        return RestartDecision::Failed {
            at: failure.at,
            code: failure.code.clone(),
            detail: failure.detail.clone(),
        };
    }
    // So is an identity abort (ADR-286), even with a transfer open at a recovery-required point.
    if let Some(identity) = facts.identity_mismatch {
        return RestartDecision::Aborted { identity };
    }
    // So is a run's stop on the step limit or on a refused instruction (ADR-288).
    if let Some(stopped) = &facts.run_stopped {
        return RestartDecision::Stopped(stopped.clone());
    }
    let interrupted_at = interruption_point(facts);
    if let Some(at) = interrupted_at
        && let Some(reason) = recovery_required(program, effective_point(program, facts, at))
    {
        return RestartDecision::OnSiteInterventionRequired(reason);
    }
    // A pass through a `RecoveryRequired` section outside the plans that no transfer-start
    // marker followed would run again: a plain start runs the program from its start, and a
    // restart of an older transfer goes on past its plan (ADR-291).
    if let Some(intent) = facts.last_intent
        && in_section_outside_plans(program, intent.pc)
        && facts
            .transfer
            .as_ref()
            .is_none_or(|transfer| transfer.started_at.steps < intent.steps)
        && let Some(reason) = recovery_required(program, intent)
    {
        return RestartDecision::OnSiteInterventionRequired(reason);
    }
    let Some(transfer) = &facts.transfer else {
        return classify_plain_procedure(program, state);
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
    let entry_state = match entry_state(program, state, plan) {
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

/// The classification of a job whose journal holds no transfer-start marker (ADR-335 items 3 to
/// 5), after the checks `classify` makes first. The journal holds a plain procedure's requests
/// and checkpoints (ADR-335 item 2). In this order:
/// - no request ever recorded, or every one withdrawn: [`RestartDecision::PlainStart`], on the same journal, whatever
///   the journal's states (a plain start needs none). The caller counts it (ADR-335 item 6);
/// - a withdrawn `Request` (its run ended with the request provably unsent, ADR-392) counts as no
///   request: the request judged below is the newest one that is not withdrawn
///   (`RecoveryFacts::standing_request`), and when there is none the answer is
///   [`RestartDecision::PlainStart`];
/// - a `Request` newer than the newest state record is [`OnSiteReason::UnconfirmedRequest`]: the
///   primitive it guards may have reached the ECU, and the VM state after it was never journaled.
///   The newest state record is the latest checkpoint, whose own step count says which step runs
///   next whatever the record's kind: a checkpoint or a step state is taken after the completed
///   step, so its count is one past the record's, and a run start's is the step about to run.
///   Comparing the request with that count therefore needs no case per kind. A cancel while a
///   request's marker was committed leaves a marker with no later state, which reads as
///   unconfirmed too unless the runner withdrew the marker, and so does a journal that holds a request and no checkpoint;
/// - a latest checkpoint state that does not decode, or fails `Vm::check_state` with its step
///   count set to [`next_steps`], is [`OnSiteReason::InvalidEntryState`]. It is checked before
///   the conditions below, which read the state's pc and call stack;
/// - otherwise a checkpoint resume from the latest checkpoint, when all of these hold, checked in
///   this order, each failure a [`OnSiteReason::NotResumable`]:
///   (I) the checkpoint's pc is an interruptible point ([`interruptible_point`]): not inside a
///   section that is not `Yes` and not on a `Ret`, otherwise
///   [`NotResumableCause::UninterruptibleSection`], so a `No` section is never resumed from
///   (ADR-335 items 3 and 5);
///   (S) nothing reachable from the checkpoint ([`Program::reachable_from`], with the state's
///   suspended callers) is a hazard: the lowest-pc instruction that lies in a plan's range, is a
///   diagnostic primitive in a `RecoveryRequired` section, or is a diagnostic primitive that is
///   not safe to repeat ([`safe_to_repeat`]; one outside every section is not);
///   (identified) the job names a target VIN and the program declares a VIN source.
///
///   The answer is then [`RestartDecision::ResumeFromCheckpoint`], with the checked state.
///
/// Nothing is contacted. The VIN read, the lock and the resume count are the caller's.
fn classify_plain_procedure(program: &Program, state: &JournalState) -> RestartDecision {
    let on_site = |reason| RestartDecision::OnSiteInterventionRequired(reason);
    let not_resumable = |cause| on_site(OnSiteReason::NotResumable { cause });
    let facts = &state.facts;
    // The newest request that is not withdrawn (ADR-392): a request whose run ended with it
    // provably unsent never reached the ECU, so none standing means none was sent.
    let Some(request) = facts.standing_request else {
        return RestartDecision::PlainStart;
    };
    let Some((at, bytes)) = &state.latest_checkpoint else {
        return on_site(OnSiteReason::UnconfirmedRequest { at: request });
    };
    let mut resumed = match decode_state(bytes) {
        Ok(decoded) => decoded,
        Err(EntryStateError::Invalid(why)) => return on_site(OnSiteReason::InvalidEntryState(why)),
        // `decode_state` has no other failure.
        Err(EntryStateError::Missing) => {
            return on_site(OnSiteReason::UnconfirmedRequest { at: request });
        }
    };
    if request.steps >= resumed.steps {
        return on_site(OnSiteReason::UnconfirmedRequest { at: request });
    }
    // A damaged state is refused before the checks below read its pc and call stack.
    resumed.steps = next_steps(state);
    if let Err(error) = Vm::resume(resumed.clone()).check_state(program) {
        return on_site(OnSiteReason::InvalidEntryState(error.to_string()));
    }
    let pc = resumed.pc;
    if !interruptible_point(program, pc) {
        return not_resumable(NotResumableCause::UninterruptibleSection { pc });
    }
    let return_pcs: Vec<u32> = resumed
        .call_stack
        .iter()
        .map(|frame| frame.return_pc)
        .collect();
    if let Some(cause) = continuation_hazard(program, pc, &return_pcs) {
        return not_resumable(cause);
    }
    // A target VIN that is not well-formed could never match a VIN read, so it is no identity.
    if !facts
        .target_vin
        .as_ref()
        .is_some_and(|vin| Vin::is_well_formed_text(vin.as_str()))
        || program.identity.vin.is_none()
    {
        return not_resumable(NotResumableCause::NoIdentity);
    }
    RestartDecision::ResumeFromCheckpoint(Box::new(CheckpointResume {
        at: *at,
        state: resumed,
    }))
}

/// Condition (S) of a continuation from `pc` (ADR-335 item 3): the first hazard among the
/// instructions reachable from `pc` with the suspended callers `return_pcs`
/// ([`Program::reachable_from`]), the lowest pc first. A hazard is an instruction in a plan's
/// range, a diagnostic primitive in a `RecoveryRequired` section, or a diagnostic primitive that
/// is not safe to repeat ([`safe_to_repeat`]; one outside every section is not). `None` when the
/// program can go on from there with every request safe to send again.
fn continuation_hazard(
    program: &Program,
    pc: u32,
    return_pcs: &[u32],
) -> Option<NotResumableCause> {
    let reachable = program.reachable_from(pc, return_pcs);
    for ahead in (0..program.code.len() as u32).filter(|&ahead| reachable[ahead as usize]) {
        if let Some(plan) = program.flash.iter().find(|plan| {
            (plan.boundaries.entry_pc..plan.boundaries.post_transfer_end_pc).contains(&ahead)
        }) {
            return Some(NotResumableCause::PlanAhead {
                flash_session: plan.flash_session,
            });
        }
        if !program.code[ahead as usize].is_diagnostic_primitive() {
            continue;
        }
        if let Some(section) = program.sections.iter().position(|section| {
            matches!(section.interruptible, Interruptible::RecoveryRequired)
                && (section.start_pc..section.end_pc).contains(&ahead)
        }) {
            return Some(NotResumableCause::RecoveryRequiredAhead { section, pc: ahead });
        }
        if !safe_to_repeat(program, ahead) {
            return Some(NotResumableCause::UnsafePrimitiveAhead { pc: ahead });
        }
    }
    None
}

/// Whether a run whose link was lost goes on in the same run on a new one, as far as the
/// program decides (design 8.2.5, ADR-393): ADR-335 item 3's conditions, judged at the VM's
/// current pc. (I) the pc is an interruptible point ([`interruptible_point`]), and (S) nothing
/// reachable from it is a hazard ([`continuation_hazard`]). `state` is the VM's state, whose pc is
/// the primitive that failed. A primitive whose request provably never left (`unsent`) is
/// sent whatever its idempotency, so (S) looks from the instruction after it (an exchange falls
/// through; a loop back to it is a hazard again). The identity (the third condition) is read
/// from the ECU by the caller, and only for a job with a journal.
pub(crate) fn continues_after_loss(program: &Program, state: &VmState, unsent: bool) -> bool {
    let pc = state.pc;
    if !interruptible_point(program, pc) {
        return false;
    }
    let return_pcs: Vec<u32> = state
        .call_stack
        .iter()
        .map(|frame| frame.return_pc)
        .collect();
    let from = if unsent { pc + 1 } else { pc };
    continuation_hazard(program, from, &return_pcs).is_none()
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
                tracing::warn!(
                    error = %error.log_text(),
                    external = error.log_external(),
                    "the supply voltage could not be read"
                );
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
    H: DiagHost<Error = HostError> + RuntimeInputs + ProcedurePosition,
{
    let passive = |reason| Ok(TeardownGate::PassiveOnly(reason));
    // The identity reads first, then the preconditions', each logged as such (design 8.11);
    // the caller ends the own reads.
    host.own_reads(Some(OwnRead::Identification));
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
    match cancellable(cancelled, || {
        resolve_identity(IdentityKind::Vin, source, sources, host)
    })? {
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
            tracing::warn!(
                error = %error.log_text(),
                external = error.log_external(),
                "the ECU's VIN could not be read"
            );
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
    match cancellable(cancelled, || {
        read_identity_bytes(IdentityKind::HardwarePartNumber, source, sources, host)
    })? {
        Ok(FieldBytes::Field(bytes)) if bytes == recorded => {}
        Ok(FieldBytes::Field(_)) => return passive(PassiveReason::HardwareIdentityDiffers),
        Ok(_) => return passive(PassiveReason::HardwareIdentityNotEstablished),
        Err(error) => {
            tracing::warn!(
                error = %error.log_text(),
                external = error.log_external(),
                "the ECU's hardware identity could not be read"
            );
            return passive(PassiveReason::HardwareIdentityNotEstablished);
        }
    }

    // The safety preconditions.
    host.own_reads(Some(OwnRead::Precondition));
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
    match cancellable(cancelled, || {
        resolve_identity(IdentityKind::Vin, source, sources, host)
    })? {
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
            tracing::warn!(
                error = %error.log_text(),
                external = error.log_external(),
                "the ECU's VIN could not be read after the restart"
            );
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
    match cancellable(cancelled, || {
        read_identity_bytes(IdentityKind::HardwarePartNumber, source, sources, host)
    })? {
        Ok(FieldBytes::Field(bytes)) if bytes == recorded => Ok(()),
        Ok(FieldBytes::Field(_)) => Err(JobError::IdentityMismatch {
            identity: IdentityKind::HardwarePartNumber,
        }),
        Ok(_) => not_established(IdentityKind::HardwarePartNumber),
        Err(error) => {
            tracing::warn!(
                error = %error.log_text(),
                external = error.log_external(),
                "the ECU's hardware identity could not be read after the restart"
            );
            not_established(IdentityKind::HardwarePartNumber)
        }
    }
}

/// The identity check of a checkpoint resume of a plain procedure (ADR-335 items 6 and 7,
/// ADR-261, ADR-263): the VIN half of [`check_identity`]. The ECU's VIN is read through the
/// program's source and must equal `target_vin`, the VIN the journal recorded at the job's first
/// run:
/// - a well-formed equal VIN calls `promote` with the target VIN, which takes the per-vehicle lock;
///   its error is returned as it is;
/// - a well-formed VIN that differs is [`JobError::IdentityMismatch`], which the caller journals
///   (ADR-286);
/// - anything else (no target VIN or no declared source, no answer, a negative response, a value
///   that is not a well-formed VIN, a worker failure) is
///   [`OnSiteReason::ProcedureIdentityNotEstablished`].
///
/// A cancel stops it at the start and before and right after the read. Nothing here sends anything
/// but the ReadDataByIdentifier request of the source. No VIN is put in a log message or a result
/// (design 16.2).
pub(crate) fn check_procedure_identity<H>(
    program: &Program,
    target_vin: Option<&Vin>,
    sources: &ServiceSources,
    host: &mut H,
    cancelled: &AtomicBool,
    promote: impl FnOnce(&Vin) -> Result<(), JobError>,
) -> Result<(), JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs,
{
    let not_established = || {
        Err(JobError::OnSiteInterventionRequired(
            OnSiteReason::ProcedureIdentityNotEstablished,
        ))
    };
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    let (Some(target_vin), Some(source)) = (
        target_vin.filter(|vin| Vin::is_well_formed_text(vin.as_str())),
        program.identity.vin,
    ) else {
        return not_established();
    };
    match cancellable(cancelled, || {
        resolve_identity(IdentityKind::Vin, source, sources, host)
    })? {
        Ok(Reading::Text(text))
            if Vin::is_well_formed_text(&text) && text == target_vin.as_str() =>
        {
            promote(target_vin)
        }
        Ok(Reading::Text(text)) if Vin::is_well_formed_text(&text) => {
            Err(JobError::IdentityMismatch {
                identity: IdentityKind::Vin,
            })
        }
        Ok(_) => not_established(),
        Err(error) => {
            tracing::warn!(
                error = %error.log_text(),
                external = error.log_external(),
                "the ECU's VIN could not be read for a checkpoint resume"
            );
            not_established()
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
///   it (ADR-269; this covers an intended version equal to the pre-erase one:
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
        match cancellable(cancelled, || {
            read_identity_bytes(IdentityKind::SoftwareVersion, source, sources, host)
        })? {
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
                tracing::warn!(
                    answer = field_bytes_text(&other),
                    "the ECU's software version was not established"
                );
            }
            Err(error) => {
                tracing::warn!(
                    error = %error.log_text(),
                    external = error.log_external(),
                    "the ECU's software version could not be read"
                );
            }
        }
    }
    not_established()
}

/// The full precondition check at the start of ADR-229 item 2 step 4 (step 4a), run after
/// `check_state` decided [`StateCheck::RedoTransfer`] and before anything of step 4 is replayed:
/// the conditions may have changed while the agent was down. Every precondition the program
/// declares is checked, in the order voltage, external supply, ignition, engine, vehicle speed,
/// stopping at the first that does not hold. The ECU is in its default session (step 2b-2
/// confirmed it), so each is read through its default-session source only, which
/// `Program::validate` requires for every declared precondition; the programming-session source
/// is for the second check before the erase (`check_before_erase`). A value outside the declared range, a reading that
/// is not a value, a source the table does not map and a failure to use the worker all end in
/// [`OnSiteReason::PreconditionNotMet`], which carries the precondition and `teardown`.
///
/// The software-version match of design 8.9.1 is not checked again: during a restart the rules of
/// step 3b-2 (`check_state`) replace it. Nor are the VIN and the hardware identity, which step 3a
/// established. A cancel stops it at the start, before and right after every read. Nothing here
/// sends anything but ReadDataByIdentifier requests through the declared sources, and reads of
/// runtime inputs.
pub(crate) fn check_reentry<H>(
    program: &Program,
    point: &RestartPoint,
    teardown: &Teardown,
    sources: &ServiceSources,
    host: &mut H,
    cancelled: &AtomicBool,
) -> Result<(), JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs,
{
    let failed = check_preconditions(
        program,
        sources,
        host,
        cancelled,
        |precondition| precondition.default_session,
        "at re-entry",
    )?;
    match failed {
        None => Ok(()),
        Some(precondition) => Err(JobError::OnSiteInterventionRequired(
            OnSiteReason::PreconditionNotMet {
                flash_session: point.flash_session,
                precondition,
                teardown: teardown.clone(),
            },
        )),
    }
}

/// The second check of the mutable conditions at ADR-229 item 2 step 4 (step 4b-3, ADR-245
/// item 6), run when the replay of step 4b-2 stopped at the plan's erase and before anything is
/// erased. Every precondition the program declares (supply voltage, external supply, ignition,
/// engine, vehicle speed; the VIN and the hardware identity are not preconditions and are not
/// read) is checked in that order, stopping at the first that does not hold. Each is read through
/// its programming-session source only, which `Program::validate` requires for every declared
/// precondition of a restartable plan; a missing one is not established, so not met. That
/// source must be usable in the programming session (ADR-229 item 2 step 4), which the plan's
/// steps from its entry normally enter. The agent tracks no session: when those steps re-enter no
/// programming session, the ECU is still in its default session here, and a source that answers
/// only in the programming session fails the check, with nothing erased. A value outside the
/// declared range, a
/// reading that is not a value, a source the table does not map and a failure to use the worker
/// all end in [`OnSiteReason::PreconditionNotMetBeforeErase`], which carries the precondition and
/// `teardown`. Each is read once, with no retry.
///
/// A cancel stops it at the start, before and right after every read. Nothing here sends anything
/// but ReadDataByIdentifier requests through the declared sources, and reads of runtime inputs.
pub(crate) fn check_before_erase<H>(
    program: &Program,
    point: &RestartPoint,
    teardown: &Teardown,
    sources: &ServiceSources,
    host: &mut H,
    cancelled: &AtomicBool,
) -> Result<(), JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs,
{
    let failed = check_preconditions(
        program,
        sources,
        host,
        cancelled,
        |precondition| precondition.programming_session,
        "before the erase",
    )?;
    match failed {
        None => Ok(()),
        Some(precondition) => Err(JobError::OnSiteInterventionRequired(
            OnSiteReason::PreconditionNotMetBeforeErase {
                flash_session: point.flash_session,
                precondition,
                teardown: teardown.clone(),
            },
        )),
    }
}

/// The loop shared by `check_reentry` and `check_before_erase`: every declared precondition in
/// the order voltage, external supply, ignition, engine, vehicle speed, each read once through
/// the source `pick_source` selects, stopping at the first that does not hold (a missing source,
/// a value outside the declared range, a reading that is not a value, or a failed read all mean
/// not met). Returns that precondition, or `None` when all hold. A cancel stops it at the start,
/// before and right after every read. `log_context` completes the warning logged for a failed
/// read.
fn check_preconditions<H>(
    program: &Program,
    sources: &ServiceSources,
    host: &mut H,
    cancelled: &AtomicBool,
    pick_source: fn(&Precondition) -> Option<Source>,
    log_context: &str,
) -> Result<Option<PreconditionKind>, JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs,
{
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
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
        let holds = match pick_source(precondition) {
            Some(source) => {
                let range = precondition.satisfied.lower..=precondition.satisfied.upper;
                match cancellable(cancelled, || resolve_source(source, sources, host))? {
                    Ok(Reading::Value(value)) => range.contains(&value),
                    Ok(_) => false,
                    Err(error) => {
                        tracing::warn!(
                            error = %error.log_text(),
                            external = error.log_external(),
                            ?kind,
                            "a precondition could not be read {log_context}"
                        );
                        false
                    }
                }
            }
            // `Program::validate` refuses this; not established, so not met.
            None => false,
        };
        if !holds {
            return Ok(Some(kind));
        }
    }
    Ok(None)
}

/// Whether the interruption is at the end of a pass whose post-transfer steps are journaled
/// complete: the completion is recorded and the interruption point is no later than the
/// completed pass's last post-transfer step (the test of `effective_point`). A step back into the
/// plan after the completion puts the point later, so this is false for a later pass.
pub(crate) fn completed_pass_interrupted(facts: &RecoveryFacts) -> bool {
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
///   journaled complete for the interrupted pass (the completed path, by the rule of
///   [`completed_pass_interrupted`] that the state check applies, ADR-269 item 2) send nothing
///   and wait for nothing ([`Teardown::CompletedPath`]); a RequestTransferExit intent without the
///   completion makes the teardown passive ([`PassiveCause::TransferExitJournaled`]). A
///   completion journaled for an earlier pass, which a step back into the plan or the replay of
///   a redone transfer (ADR-273) has left behind, is neither: that pass's RequestTransferExit
///   was answered and its post-transfer steps finished, so the gate decides as if no
///   RequestTransferExit had been journaled;
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
    let cause = if completed_pass_interrupted(&point.facts) {
        return Ok(Teardown::CompletedPath);
    } else if exit.is_some_and(|exit| !exit.complete) {
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
                    tracing::warn!(
                        answer_len = other.len(),
                        "the ECUReset got no usable answer"
                    );
                    PassiveCause::ResetOutcomeUnknown
                }
            },
            Err(error) => {
                tracing::warn!(
                    error = %error.log_text(),
                    external = error.log_external(),
                    "the ECUReset got no usable answer"
                );
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
            Ok(other) => log_failed_read(failed_reads, Ok(&other)),
            Err(error) => log_failed_read(failed_reads, Err(&error)),
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
/// window does not flood the log. `failed_before` counts the attempt's earlier failed reads. An
/// answer is logged as its length, or its NRC when negative, not its bytes (ADR-348 item 6).
fn log_failed_read(failed_before: u32, read: Result<&[u8], &HostError>) {
    let (answer_len, nrc, error, external) = match read {
        Ok(answer) => {
            let nrc = match answer {
                [0x7F, _, nrc] => Some(*nrc),
                _ => None,
            };
            (Some(answer.len()), nrc, None, None)
        }
        Err(error) => (None, None, Some(error.log_text()), error.log_external()),
    };
    if failed_before == 0 {
        tracing::warn!(
            answer_len,
            nrc,
            error,
            external,
            "the session read did not confirm the default session"
        );
    } else {
        tracing::debug!(
            answer_len,
            nrc,
            error,
            external,
            "the session read did not confirm the default session"
        );
    }
}

/// A [`FieldBytes`] as the log carries it: the kind and the field's length, or the NRC, never
/// the field's bytes, which are the vehicle's (ADR-348 item 6).
fn field_bytes_text(read: &FieldBytes) -> String {
    match read {
        FieldBytes::Field(bytes) => format!("a field of {} bytes", bytes.len()),
        FieldBytes::Negative(nrc) => format!("a negative response with NRC {nrc:#04x}"),
        FieldBytes::Unreadable => "nothing readable".to_owned(),
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
                tracing::warn!(
                    error = %error.log_text(),
                    external = error.log_external(),
                    ?kind,
                    "a safety precondition could not be read"
                );
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
        facts.last_request,
        state.last_vm_state.as_ref().map(|(at, _)| *at),
        state.latest_checkpoint.as_ref().map(|(at, _)| *at),
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
/// a plan's entry, at a completed plan's end, or at the intent ahead of the first primitive of a
/// pass through a `RecoveryRequired` section outside every plan (ADR-291). `classify` also
/// refuses on such an intent that is not the point, when no transfer-start marker followed it.
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

/// Whether the diagnostic primitive at `pc` is safe to repeat (ADR-335): at least one section
/// covers it and every section that covers it is `Safe`. A primitive outside every section, or
/// covered by a `Safe` and a non-`Safe` section at once (hand-built IR, which `Program::validate`
/// accepts), is not, as the readers combine covering sections conservatively (ADR-332 item 5).
pub(crate) fn safe_to_repeat(program: &Program, pc: u32) -> bool {
    let mut covering = program
        .sections
        .iter()
        .filter(|section| (section.start_pc..section.end_pc).contains(&pc))
        .peekable();
    covering.peek().is_some()
        && covering.all(|section| matches!(section.idempotency, Idempotency::Safe))
}

/// What the runner does with a host error of the diagnostic primitive it just ran (ADR-392).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RepeatDecision {
    /// Send the same request again in place, without arriving at the instruction again.
    Repeat,
    /// The path to the VCI was lost (`ResendClass::PathLost`): send the request again, but on a
    /// link opened again, if the runner can continue there (`continues_after_loss`, ADR-393);
    /// otherwise the run ends in the host error.
    ReopenThenRepeat,
    /// The run ends in the host error.
    End,
    /// The run ends in on-site intervention with this reason, the one the next resume of the
    /// journal derives. Nothing is journaled for the ending (ADR-290).
    OnSite(OnSiteReason),
}

/// The one place that decides whether a host error of the primitive at `at` is sent again in
/// the run (ADR-392). `attempts` is how many times this arrival's request failed before this
/// error (0 for the first), `journal` the job's journal as it now stands, or `None` without one.
///
/// - Only a request to the ECU outside every plan's range is considered: `ServiceRequest`,
///   `ReadDtc`, `RoutineControl` and `SendReceive`. Everything else ends the run.
/// - A refusal before the start that passes (`ResendClass::Transient`) is sent again whatever
///   the idempotency, as nothing was sent.
/// - An ECU- or bus-level error of a request that may have reached the ECU
///   (`ResendClass::EcuLevel`) is sent again only when [`safe_to_repeat`] says so. Otherwise the
///   verdict is [`classify`]'s on the journal as it stands: on-site intervention with its reason
///   when it refuses a resume, else the run ends in the host error (a completed flash transfer
///   resumes by ADR-229's restart). Without a journal, which no resume reads, it is an
///   unconfirmed request.
/// - A lost path to the VCI (`ResendClass::PathLost`, ADR-393) is the same decision with a link to
///   open first: a request provably not sent is [`RepeatDecision::ReopenThenRepeat`] whatever the
///   idempotency, one that may have reached the ECU only when [`safe_to_repeat`] says so, and
///   otherwise the verdict above applies, without a new link.
/// - Every send counts against `JobLimits::max_primitive_repeats`; the error after the last
///   one ends the run.
pub(crate) fn repeat_decision(
    program: &Program,
    at: StepRef,
    error: &HostError,
    attempts: u8,
    limits: &JobLimits,
    journal: Option<&JournalState>,
) -> RepeatDecision {
    if !is_exchange(program, at.pc) || in_plan(program, at.pc) {
        return RepeatDecision::End;
    }
    let within_bound = attempts < limits.max_primitive_repeats;
    match (error.send_state(), error.resend_class()) {
        (SendState::NotSent, ResendClass::Transient) if within_bound => RepeatDecision::Repeat,
        (SendState::NotSent, ResendClass::PathLost) if within_bound => {
            RepeatDecision::ReopenThenRepeat
        }
        (SendState::MayHaveReached, class @ (ResendClass::EcuLevel | ResendClass::PathLost))
            if safe_to_repeat(program, at.pc) =>
        {
            match (within_bound, class) {
                (false, _) => RepeatDecision::End,
                (true, ResendClass::PathLost) => RepeatDecision::ReopenThenRepeat,
                (true, _) => RepeatDecision::Repeat,
            }
        }
        (SendState::MayHaveReached, ResendClass::EcuLevel | ResendClass::PathLost) => match journal
        {
            // The verdict of the restart's classification on the journal as it stands; where
            // it would not refuse a resume (a completed flash transfer takes ADR-229's
            // restart), the run ends as before.
            Some(state) => match classify(program, Ok(state)) {
                RestartDecision::OnSiteInterventionRequired(reason) => {
                    RepeatDecision::OnSite(reason)
                }
                _ => RepeatDecision::End,
            },
            // No journal, which no resume reads: the request is unconfirmed.
            None => RepeatDecision::OnSite(OnSiteReason::UnconfirmedRequest { at }),
        },
        _ => RepeatDecision::End,
    }
}

/// Whether the instruction at `pc` is one request to the ECU and its answer (`ServiceRequest`,
/// `ReadDtc`, `RoutineControl`, `SendReceive`): the primitives whose failed attempt can be sent
/// again, or withdrawn when it never left (ADR-392). `SecurityAccess` is two exchanges and
/// `FlashTransfer` a sequence of blocks, so neither is one.
pub(crate) fn is_exchange(program: &Program, pc: u32) -> bool {
    matches!(
        program.code.get(pc as usize),
        Some(
            Op::ServiceRequest { .. }
                | Op::ReadDtc { .. }
                | Op::RoutineControl { .. }
                | Op::SendReceive { .. }
        )
    )
}

/// Whether `pc` lies inside a flash recovery plan's range (`entry_pc` up to, not including,
/// `post_transfer_end_pc`).
pub(crate) fn in_plan(program: &Program, pc: u32) -> bool {
    program
        .flash
        .iter()
        .any(|plan| (plan.boundaries.entry_pc..plan.boundaries.post_transfer_end_pc).contains(&pc))
}

/// Whether the journal had reached a flash plan (ADR-344): it records a transfer-start marker, or
/// its last completed step or its newest request intent lies within a plan's range.
pub(crate) fn reached_flash_plan(program: &Program, facts: &RecoveryFacts) -> bool {
    facts.transfer.is_some()
        || [facts.last_step, facts.last_intent]
            .into_iter()
            .flatten()
            .any(|at| in_plan(program, at.pc))
}

/// Whether `pc` is an interruptible point in ADR-330's sense: a cancel is taken there. It is not
/// inside a section whose `interruptible` is not `Yes` (an uninterruptible or a
/// recovery-required one), and not a `Ret`, so the returns after a procedure's last section do
/// not count. The runner's `cancel_held_at` and `holds_cancel` are the same rule, computed once
/// per run; a checkpoint resume (ADR-335 item 3 condition I, item 5) may start only here.
pub(crate) fn interruptible_point(program: &Program, pc: u32) -> bool {
    !matches!(program.code.get(pc as usize), Some(Op::Ret))
        && !in_uninterruptible_section(program, pc)
}

/// Whether `pc` lies inside a section whose `interruptible` is not `Yes`.
fn in_uninterruptible_section(program: &Program, pc: u32) -> bool {
    program.sections.iter().any(|section| {
        !matches!(section.interruptible, Interruptible::Yes)
            && (section.start_pc..section.end_pc).contains(&pc)
    })
}

/// Whether the diagnostic primitive at `pc` gets a request marker before it is sent: it lies
/// outside every plan's range and is not safe to repeat (ADR-335 item 2).
pub(crate) fn needs_request_marker(program: &Program, pc: u32) -> bool {
    program
        .code
        .get(pc as usize)
        .is_some_and(Op::is_diagnostic_primitive)
        && !in_plan(program, pc)
        && !safe_to_repeat(program, pc)
}

/// Whether a missing journal of a job of `program` is a plain start (ADR-335 item 1, ADR-293):
/// the program has no flash recovery plan, no section marked `RecoveryRequired` and no
/// diagnostic primitive that is not safe to repeat. Every journaled job creates its journal
/// before it sends anything, so for any other program a missing one cannot be told from one lost
/// after a request that changed the ECU.
pub(crate) fn missing_journal_is_plain_start(program: &Program) -> bool {
    program.flash.is_empty()
        && !program
            .sections
            .iter()
            .any(|section| matches!(section.interruptible, Interruptible::RecoveryRequired))
        && (0..program.code.len() as u32).all(|pc| {
            !program.code[pc as usize].is_diagnostic_primitive() || safe_to_repeat(program, pc)
        })
}

/// Whether `pc` lies in a section marked `RecoveryRequired` and outside every plan's range
/// (`entry_pc` up to, not including, `post_transfer_end_pc`): where the runner writes an intent
/// ahead of a pass's first primitive (ADR-291).
pub(crate) fn in_section_outside_plans(program: &Program, pc: u32) -> bool {
    let in_plan = program
        .flash
        .iter()
        .any(|plan| (plan.boundaries.entry_pc..plan.boundaries.post_transfer_end_pc).contains(&pc));
    !in_plan
        && program.sections.iter().any(|section| {
            matches!(section.interruptible, Interruptible::RecoveryRequired)
                && (section.start_pc..section.end_pc).contains(&pc)
        })
}

/// The first diagnostic primitive, in pc order, that the continuation after `verified`'s
/// read-back verification could send again unsafely (ADR-271 item 4, with its annotations).
/// Sections may overlap, and every one that covers a primitive counts. `None` when the program
/// can go on after `verified`. Two kinds of primitive are looked at:
/// - one outside every plan's range (`entry_pc` up to, not including, `post_transfer_end_pc`),
///   statically over the whole program, so a jump back to code before a plan is covered too:
///   the journal records no step there, so it may have reached the ECU before the interruption.
///   It must be covered, and only by sections that are `Safe` and not marked `RecoveryRequired`,
///   since the journal cannot rule out that the interruption was there;
/// - one in the window of the plan that begins at `verified`'s `post_transfer_end_pc`, from its
///   `entry_pc` up to its `erase_pc` or its recovery-required point, whichever comes first. A
///   crash with such a request in flight, before that plan's first journaled step, leaves the
///   same journal as a crash at `verified`'s end, so the continuation runs it again; the first
///   request at or after the recovery-required point gets an intent before it is sent, which
///   places any later interruption past that point. For a plan that allows a restart, that is
///   the replay of ADR-273 item 5, and only an `Unsafe` section refuses; `Program::validate`
///   already excludes one there, so for a validated program this branch never refuses. For a
///   plan that does not, no replay is ever sanctioned, and the window is checked like the
///   primitives outside the plans. The whole window is checked, although a crash after its first
///   completed primitive is journaled and no longer reaches the continuation: the check stays
///   static, and refuses on the safe side.
///
/// A later plan reached through that plan is not looked at: its erase commits a transfer-start
/// marker first, so an interruption there restarts that plan, not `verified`.
pub(crate) fn unsafe_continuation(program: &Program, verified: &FlashRecovery) -> Option<u32> {
    let covering = |pc: u32| {
        program
            .sections
            .iter()
            .filter(move |section| (section.start_pc..section.end_pc).contains(&pc))
    };
    let not_safe = |pc: u32| {
        !safe_to_repeat(program, pc)
            || covering(pc)
                .any(|section| matches!(section.interruptible, Interruptible::RecoveryRequired))
    };
    let end = verified.boundaries.post_transfer_end_pc;
    let next = program
        .flash
        .iter()
        .find(|plan| plan.boundaries.entry_pc == end)
        .map(|plan| {
            let from = match plan.recovery_required {
                RecoveryRequired::Never => plan.boundaries.post_transfer_end_pc,
                RecoveryRequired::FromPc(pc) => pc,
            };
            (
                plan,
                plan.boundaries.entry_pc..from.min(plan.boundaries.erase_pc),
            )
        });
    (0..program.code.len() as u32).find(|&pc| {
        if !program.code[pc as usize].is_diagnostic_primitive() {
            return false;
        }
        let in_plan = program.flash.iter().any(|plan| {
            (plan.boundaries.entry_pc..plan.boundaries.post_transfer_end_pc).contains(&pc)
        });
        if !in_plan {
            return not_safe(pc);
        }
        match &next {
            Some((plan, window)) if window.contains(&pc) => {
                if plan.allows_restart() {
                    covering(pc).any(|section| matches!(section.idempotency, Idempotency::Unsafe))
                } else {
                    not_safe(pc)
                }
            }
            _ => false,
        }
    })
}

#[derive(Debug)]
pub(crate) enum EntryStateError {
    Missing,
    Invalid(String),
}

/// Decodes a journaled VM state, without checking it against the program.
fn decode_state(bytes: &[u8]) -> Result<VmState, EntryStateError> {
    postcard::from_bytes::<VmState>(bytes)
        .map_err(|error| EntryStateError::Invalid(format!("the VM state does not decode: {error}")))
}

/// Checks `restored` with `Vm::check_state` and that it stands at `pc`.
fn state_standing_at(
    program: &Program,
    restored: VmState,
    pc: u32,
) -> Result<VmState, EntryStateError> {
    let vm = Vm::resume(restored);
    vm.check_state(program)
        .map_err(|error: VmError| EntryStateError::Invalid(error.to_string()))?;
    if vm.state.pc != pc {
        return Err(EntryStateError::Missing);
    }
    Ok(vm.state)
}

/// The VM state a redo of `plan`'s transfer resumes from: the newest state standing at its
/// `entry_pc`. That is either the state the runner took on the step into the entry (ADR-252) or
/// the state a run started from, which every journaled run from instruction 0 commits first
/// (ADR-272); the restart's replay commits a run start carrying this same state (ADR-274). A run
/// start sits newer than an earlier run's states, so a plain start on an existing journal never
/// inherits them.
///
/// The newest state may be the plan's end state (ADR-271 item 2), committed after the entry's
/// when the pass ran through, and a crash after that state still redoes the transfer: then the
/// state before it is the entry's (ADR-274). That fallback applies only when the newest state
/// stands at the plan's end, it is the current attempt's end-state record (the transfer's last
/// post-transfer step is the step that recorded it) and the interruption point is that step, so
/// nothing was journaled after it. Any other newest state not at the entry is
/// `EntryStateError::Missing`: for adjacent plans, a later plan's pre-erase primitive journaled
/// after the first plan's completion is refused (ADR-253). Only a journal that holds no state,
/// one written before the run-start record, falls back to the program's initial state for an
/// entry at pc 0. The state used is checked with `Vm::check_state` before it is returned.
fn entry_state(
    program: &Program,
    state: &JournalState,
    plan: &FlashRecovery,
) -> Result<VmState, EntryStateError> {
    let entry_pc = plan.boundaries.entry_pc;
    let Some((_, bytes)) = &state.last_vm_state else {
        if entry_pc == 0 {
            return state_standing_at(program, Vm::new(program).state, entry_pc);
        }
        return Err(EntryStateError::Missing);
    };
    let newest = decode_state(bytes)?;
    if newest.pc == entry_pc {
        return state_standing_at(program, newest, entry_pc);
    }
    // The fallback is for the current attempt's end-state record only: the step that recorded it
    // is the plan's last post-transfer step and the interruption point, nothing journaled after.
    let at = state.last_vm_state.as_ref().map(|(at, _)| *at);
    let is_end_record = newest.pc == plan.boundaries.post_transfer_end_pc
        && at.is_some()
        && state
            .facts
            .transfer
            .as_ref()
            .and_then(|transfer| transfer.exit.as_ref())
            .and_then(|exit| exit.last_post_step)
            == at
        && interruption_point(&state.facts) == at;
    if is_end_record && let Some((_, previous)) = &state.previous_vm_state {
        return state_standing_at(program, decode_state(previous)?, entry_pc);
    }
    Err(EntryStateError::Missing)
}

/// The VM state at `plan`'s `post_transfer_end_pc` that the journal holds for the pass the
/// interruption is in, if any: what the continuation after a verified restart resumes from
/// (ADR-271 items 2 and 3). It is the newest state, and only when it stands at the plan's end: a
/// state recorded at the entry or at a run start since (a new pass) makes an earlier end state
/// stale, so none is returned. `EntryStateError::Missing` is "no such state".
pub(crate) fn plan_end_state(
    program: &Program,
    state: &JournalState,
    plan: &FlashRecovery,
) -> Result<VmState, EntryStateError> {
    let end_pc = plan.boundaries.post_transfer_end_pc;
    let Some((_, bytes)) = &state.last_vm_state else {
        return Err(EntryStateError::Missing);
    };
    let newest = decode_state(bytes)?;
    if newest.pc != end_pc {
        return Err(EntryStateError::Missing);
    }
    state_standing_at(program, newest, end_pc)
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

    /// The VM state at pc `pc` after step count `steps`.
    fn state_at_pc(program: &Program, pc: u32, steps: u64) -> VmState {
        let mut state = Vm::new(program).state;
        state.pc = pc;
        state.steps = steps;
        state
    }

    /// A journal of a pass that ran to the step before the end, then commits the step that
    /// reaches the end with the VM state there (ADR-271 item 2).
    fn journal_with_end_state(program: &Program) -> Journal<Memory> {
        let mut j = journal_until(program, CHECK);
        j.commit_step(
            at(CHECK),
            Some(&state_bytes(&state_at_pc(program, END, u64::from(END)))),
        )
        .unwrap();
        j
    }

    /// A journal cut after the plan-end state but before the completion still redoes the
    /// transfer from the entry state; the end state does not replace it.
    #[test]
    fn a_journal_cut_after_the_end_state_redoes_the_transfer_from_the_entry_state() {
        let program = program(RecoveryRequired::Never);
        let j = journal_with_end_state(&program);
        let plan = &program.flash[0];
        assert_eq!(
            super::plan_end_state(&program, j.state(), plan).unwrap().pc,
            END
        );
        let RestartDecision::Restart(point) = decide(&program, &j) else {
            panic!("{:?}", decide(&program, &j));
        };
        assert_eq!(point.entry_state.pc, ENTRY);
        assert_eq!(point.interrupted_at, Some(at(CHECK)));
        // The counter goes on after the end state's record.
        assert_eq!(point.entry_state.steps, next_steps(j.state()));
        // The same with the completion committed: the entry state is still the redo's.
        let mut j = j;
        j.commit_post_transfer_complete().unwrap();
        let RestartDecision::Restart(point) = decide(&program, &j) else {
            panic!("{:?}", decide(&program, &j));
        };
        assert_eq!(point.entry_state.pc, ENTRY);
        assert_eq!(
            super::plan_end_state(&program, j.state(), plan).unwrap().pc,
            END
        );
    }

    /// An entry state recorded after an end state (a new pass) makes the end state stale: the
    /// plan-end accessor returns none, and the entry state is the newest one.
    #[test]
    fn an_entry_state_after_an_end_state_makes_the_end_state_stale() {
        let program = program(RecoveryRequired::Never);
        let mut j = journal_with_end_state(&program);
        j.commit_post_transfer_complete().unwrap();
        let next = next_steps(j.state());
        j.commit_step(
            StepRef {
                pc: ENTRY - 1,
                steps: next,
            },
            Some(&state_bytes(&state_at_pc(&program, ENTRY, next + 1))),
        )
        .unwrap();
        let plan = &program.flash[0];
        assert!(matches!(
            super::plan_end_state(&program, j.state(), plan),
            Err(EntryStateError::Missing)
        ));
        let entry = super::entry_state(&program, j.state(), plan).unwrap();
        assert_eq!((entry.pc, entry.steps), (ENTRY, next + 1));
    }

    /// Only a newest state at the plan's end gives way to the previous one: an older entry state
    /// behind a run start (a state elsewhere) is not the entry state.
    #[test]
    fn an_older_entry_state_behind_another_state_is_not_used() {
        let program = program(RecoveryRequired::Never);
        let mut j = journal_with_end_state(&program);
        let next = next_steps(j.state());
        j.commit_run_start(
            StepRef { pc: 0, steps: next },
            &state_bytes(&state_at_pc(&program, 0, next)),
        )
        .unwrap();
        let plan = &program.flash[0];
        assert!(matches!(
            super::entry_state(&program, j.state(), plan),
            Err(EntryStateError::Missing)
        ));
        assert!(matches!(
            super::plan_end_state(&program, j.state(), plan),
            Err(EntryStateError::Missing)
        ));
    }

    /// The fallback to the state before the newest needs the newest to stand at the plan's end:
    /// a newest state elsewhere, even as the last step's record, leaves no entry state.
    #[test]
    fn a_newest_state_not_at_the_plans_end_is_not_given_way_from() {
        let program = program(RecoveryRequired::Never);
        let mut j = journal_until(&program, CHECK);
        j.commit_step(
            at(CHECK),
            Some(&state_bytes(&state_at_pc(
                &program,
                CHECK,
                u64::from(CHECK),
            ))),
        )
        .unwrap();
        let facts = &j.state().facts;
        assert_eq!(interruption_point(facts), Some(at(CHECK)));
        assert_eq!(
            facts
                .transfer
                .as_ref()
                .unwrap()
                .exit
                .as_ref()
                .unwrap()
                .last_post_step,
            Some(at(CHECK))
        );
        assert!(matches!(
            super::entry_state(&program, j.state(), &program.flash[0]),
            Err(EntryStateError::Missing)
        ));
        assert_eq!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingEntryState {
                flash_session: 1
            })
        );
    }

    /// The fallback needs the newest end state to be the current attempt's end-state record, the
    /// transfer's last post-transfer step: an end-standing state recorded by a later step, after
    /// the completion stopped the journal updating that step, is not.
    #[test]
    fn an_end_state_not_recorded_by_the_last_post_transfer_step_is_not_given_way_from() {
        let program = program(RecoveryRequired::Never);
        let mut j = journal_until(&program, CHECK);
        j.commit_step(at(CHECK), None).unwrap();
        j.commit_post_transfer_complete().unwrap();
        j.commit_step(
            at(END),
            Some(&state_bytes(&state_at_pc(&program, END, u64::from(END)))),
        )
        .unwrap();
        let facts = &j.state().facts;
        assert_eq!(interruption_point(facts), Some(at(END)));
        assert_eq!(
            facts
                .transfer
                .as_ref()
                .unwrap()
                .exit
                .as_ref()
                .unwrap()
                .last_post_step,
            Some(at(CHECK))
        );
        assert!(matches!(
            super::entry_state(&program, j.state(), &program.flash[0]),
            Err(EntryStateError::Missing)
        ));
        assert_eq!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingEntryState {
                flash_session: 1
            })
        );
    }

    /// Adjacent plans: the first one's end is the second one's entry, and one record serves both.
    #[test]
    fn a_shared_end_and_entry_state_serves_both_plans() {
        let mut program = program(RecoveryRequired::Never);
        let mut second = program.flash[0].clone();
        second.stage = 8;
        second.boundaries = RecoveryBoundaries {
            entry_pc: END,
            erase_pc: END,
            transfer_exit_pc: END,
            post_transfer_end_pc: END + 1,
        };
        program.flash.push(second);
        let j = journal_with_end_state(&program);
        let state = j.state();
        let shared = state_at_pc(&program, END, u64::from(END));
        assert_eq!(
            super::plan_end_state(&program, state, &program.flash[0]).unwrap(),
            shared
        );
        assert_eq!(
            super::entry_state(&program, state, &program.flash[1]).unwrap(),
            shared
        );
        // The first plan's redo still finds its own entry state before the shared one.
        assert_eq!(
            super::entry_state(&program, state, &program.flash[0])
                .unwrap()
                .pc,
            ENTRY
        );
    }

    /// [`program`] with a second plan that starts where the first ends (stage 8, session 2).
    fn adjacent_plans() -> Program {
        let mut program = program(RecoveryRequired::Never);
        let mut second = program.flash[0].clone();
        second.flash_session = 2;
        second.stage = 8;
        second.boundaries = RecoveryBoundaries {
            entry_pc: END,
            erase_pc: END,
            transfer_exit_pc: END,
            post_transfer_end_pc: END + 1,
        };
        program.flash.push(second);
        program
    }

    /// Adjacent plans, the first complete and a pre-erase primitive of the second journaled: the
    /// shared state is the newest state, but it is no longer the interruption point, so the first
    /// plan's entry state is not taken from behind it (ADR-253, ADR-274).
    #[test]
    fn a_later_plans_pre_erase_primitive_after_a_completion_needs_on_site_intervention() {
        let program = adjacent_plans();
        let mut j = journal_with_end_state(&program);
        j.commit_post_transfer_complete().unwrap();
        j.commit_step(at(END), None).unwrap();
        assert!(matches!(
            super::entry_state(&program, j.state(), &program.flash[0]),
            Err(EntryStateError::Missing)
        ));
        assert_eq!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingEntryState {
                flash_session: 1
            })
        );
    }

    /// Adjacent plans, a crash right after the first plan's completion: the first plan restarts
    /// from its own entry state, and the shared state is its end state.
    #[test]
    fn a_crash_right_after_the_first_of_two_adjacent_plans_restarts_it_from_its_entry() {
        let program = adjacent_plans();
        let mut j = journal_with_end_state(&program);
        j.commit_post_transfer_complete().unwrap();
        let RestartDecision::Restart(point) = decide(&program, &j) else {
            panic!("{:?}", decide(&program, &j));
        };
        assert_eq!(point.flash_session, 1);
        assert_eq!(point.entry_state.pc, ENTRY);
        assert_eq!(
            super::plan_end_state(&program, j.state(), &program.flash[0]).unwrap(),
            state_at_pc(&program, END, u64::from(END))
        );
    }

    /// Adjacent plans, the second reached its end state and the crash came before its completion:
    /// it restarts from the shared state.
    #[test]
    fn a_second_adjacent_plan_that_reached_its_end_restarts_from_the_shared_state() {
        let program = adjacent_plans();
        let mut j = journal_with_end_state(&program);
        j.commit_post_transfer_complete().unwrap();
        let end = u64::from(END);
        j.commit_transfer_start(
            StageId(8),
            StepRef {
                pc: END,
                steps: end + 1,
            },
        )
        .unwrap();
        j.commit_transfer_exit_intent(StepRef {
            pc: END,
            steps: end + 2,
        })
        .unwrap();
        j.commit_step(
            StepRef {
                pc: END,
                steps: end + 3,
            },
            Some(&state_bytes(&state_at_pc(&program, END + 1, end + 4))),
        )
        .unwrap();
        let RestartDecision::Restart(point) = decide(&program, &j) else {
            panic!("{:?}", decide(&program, &j));
        };
        assert_eq!(point.flash_session, 2);
        assert_eq!(point.entry_state.pc, END);
        assert_eq!(point.entry_state.steps, next_steps(j.state()));
        assert_eq!(
            super::plan_end_state(&program, j.state(), &program.flash[1])
                .unwrap()
                .pc,
            END + 1
        );
    }

    #[test]
    fn a_job_with_no_transfer_starts_plainly() {
        let program = program(RecoveryRequired::Never);
        assert_eq!(
            decide(&program, &journal_until(&program, ENTRY)),
            RestartDecision::PlainStart
        );
        // No journal: a plain start only for a program with no plan, no `RecoveryRequired`
        // section and no primitive that is not safe to repeat (ADR-335 item 1).
        assert_eq!(
            classify(&program, Err(&JournalError::NotFound)),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingJournal)
        );
        let section = |start_pc, end_pc, interruptible, idempotency| Section {
            start_pc,
            end_pc,
            interruptible,
            idempotency,
            expected_millis: 0,
        };
        let mut no_plan = program.clone();
        no_plan.flash.clear();
        // The primitives of the plan's code, outside every section, are not safe to repeat.
        assert_eq!(
            classify(&no_plan, Err(&JournalError::NotFound)),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingJournal)
        );
        let all = no_plan.code.len() as u32;
        no_plan
            .sections
            .push(section(0, all, Interruptible::Yes, Idempotency::Safe));
        assert_eq!(
            classify(&no_plan, Err(&JournalError::NotFound)),
            RestartDecision::PlainStart
        );
        // One `Unsafe` section over a single primitive, or one overlapping a `Safe` one, is not.
        let mut unsafe_one = no_plan.clone();
        unsafe_one.sections.push(section(
            ERASE,
            ERASE + 1,
            Interruptible::Yes,
            Idempotency::Unsafe,
        ));
        assert_eq!(
            classify(&unsafe_one, Err(&JournalError::NotFound)),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingJournal)
        );
        // A primitive left outside every section.
        let mut uncovered = no_plan.clone();
        uncovered.sections[0].end_pc = ERASE;
        assert_eq!(
            classify(&uncovered, Err(&JournalError::NotFound)),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingJournal)
        );
        // A `RecoveryRequired` section keeps a journal without a plan too (ADR-293).
        no_plan.sections.push(section(
            0,
            2,
            Interruptible::RecoveryRequired,
            Idempotency::Safe,
        ));
        assert_eq!(
            classify(&no_plan, Err(&JournalError::NotFound)),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingJournal)
        );
    }

    /// A primitive is safe to repeat only when a section covers it and every covering section is
    /// `Safe` (ADR-335); the request marker is for the primitives outside the plans that are not.
    #[test]
    fn safe_to_repeat_combines_the_covering_sections_conservatively() {
        let mut program = program(RecoveryRequired::Never);
        let section = |start_pc, end_pc, idempotency| Section {
            start_pc,
            end_pc,
            interruptible: Interruptible::Yes,
            idempotency,
            expected_millis: 0,
        };
        // Outside every section.
        assert!(!safe_to_repeat(&program, ERASE));
        program.sections = vec![
            section(0, 5, Idempotency::Safe),
            section(3, 4, Idempotency::Unsafe),
            section(5, 7, Idempotency::Safe),
            section(6, 7, Idempotency::CheckState),
        ];
        assert!(safe_to_repeat(&program, 2));
        // Covered by a `Safe` and an `Unsafe` section at once.
        assert!(!safe_to_repeat(&program, 3));
        assert!(safe_to_repeat(&program, 4));
        assert!(safe_to_repeat(&program, 5));
        assert!(!safe_to_repeat(&program, 6));
        assert!(!safe_to_repeat(&program, 8));
        // A request marker needs a primitive outside every plan that is not safe to repeat: pc 3
        // is the erase, inside the plan (entry 2 up to 8).
        assert!(!needs_request_marker(&program, 3));
        program.flash.clear();
        assert!(needs_request_marker(&program, 3));
        assert!(!needs_request_marker(&program, 4));
        // The Pop at 8 is no primitive.
        assert!(!needs_request_marker(&program, 8));
        assert!(needs_request_marker(&program, EXIT));
    }

    /// An identity abort is final (ADR-286): it is decided before the interruption point is
    /// looked at, so an open transfer at a recovery-required point is the abort, not on-site
    /// intervention, and an open transfer that would restart is the abort too.
    #[test]
    fn an_identity_abort_is_decided_before_the_interruption() {
        let program = program(RecoveryRequired::FromPc(EXIT));
        let mut j = journal_until(&program, EXIT);
        assert!(matches!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::RecoveryRequiredPoint { .. })
        ));
        j.commit_identity_mismatch(IdentityKind::HardwarePartNumber)
            .unwrap();
        assert_eq!(
            decide(&program, &j),
            RestartDecision::Aborted {
                identity: IdentityKind::HardwarePartNumber
            }
        );

        let program = super::tests::program(RecoveryRequired::Never);
        let mut j = journal_until(&program, ERASE);
        assert!(matches!(decide(&program, &j), RestartDecision::Restart(_)));
        j.commit_identity_mismatch(IdentityKind::Vin).unwrap();
        assert_eq!(
            decide(&program, &j),
            RestartDecision::Aborted {
                identity: IdentityKind::Vin
            }
        );
    }

    /// A run's stop is final too (ADR-288): decided before the interruption point, so an open
    /// transfer at a recovery-required point is the stop, not on-site intervention, and an open
    /// transfer that would restart is the stop too.
    #[test]
    fn a_run_stop_is_decided_before_the_interruption() {
        use crate::journal::{RunStop, StoppedRun};
        let after_last = |j: &Journal<Memory>| StepRef {
            pc: 0,
            steps: j.state().facts.last_step.map_or(0, |last| last.steps + 1),
        };
        let program = program(RecoveryRequired::FromPc(EXIT));
        let mut j = journal_until(&program, EXIT);
        assert!(matches!(
            decide(&program, &j),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::RecoveryRequiredPoint { .. })
        ));
        let at = after_last(&j);
        j.commit_run_stopped(at, RunStop::StepLimit).unwrap();
        assert_eq!(
            decide(&program, &j),
            RestartDecision::Stopped(StoppedRun {
                at,
                reason: RunStop::StepLimit
            })
        );

        let program = super::tests::program(RecoveryRequired::Never);
        let mut j = journal_until(&program, ERASE);
        assert!(matches!(decide(&program, &j), RestartDecision::Restart(_)));
        let at = after_last(&j);
        let reason = RunStop::Vm(diag_ir::VmError::TypeMismatch);
        j.commit_run_stopped(at, reason.clone()).unwrap();
        assert_eq!(
            decide(&program, &j),
            RestartDecision::Stopped(StoppedRun { at, reason })
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

    /// A section intent outside the plans refuses while no transfer-start marker follows it,
    /// even when a later record is the interruption point, since a plain start, or a restart of
    /// an older transfer that goes on past its plan, would run the section again (ADR-291).
    #[test]
    fn a_section_intent_without_a_later_transfer_needs_on_site_intervention() {
        let section = |start_pc, end_pc| Section {
            start_pc,
            end_pc,
            interruptible: Interruptible::RecoveryRequired,
            idempotency: Idempotency::Safe,
            expected_millis: 0,
        };
        let refused_at = |decision: RestartDecision, pc| {
            matches!(
                decision,
                RestartDecision::OnSiteInterventionRequired(OnSiteReason::RecoveryRequiredSection {
                    section: 0,
                    at,
                }) if at.pc == pc
            )
        };
        let entry = |j: &mut Journal<Memory>, program: &Program, steps: u64| {
            let mut state = Vm::new(program).state;
            state.pc = ENTRY;
            state.steps = steps;
            j.commit_step(
                StepRef {
                    pc: ENTRY - 1,
                    steps,
                },
                Some(&state_bytes(&state)),
            )
            .unwrap();
        };

        // Before the plan, pc 0: the step into the entry and a step in the plan come later.
        let mut program = program(RecoveryRequired::Never);
        program.sections.push(section(0, 1));
        let mut j = new_journal();
        j.commit_intent(at(0)).unwrap();
        entry(&mut j, &program, 1);
        j.commit_step(at(ENTRY), None).unwrap();
        let decision = decide(&program, &j);
        assert!(refused_at(decision.clone(), 0), "{decision:?}");
        // The erase's marker follows it: the restart replays from the entry only.
        j.commit_transfer_start(StageId(7), at(ERASE)).unwrap();
        let decision = decide(&program, &j);
        assert!(
            matches!(decision, RestartDecision::Restart(_)),
            "{decision:?}"
        );

        // After the plan, pc 8: the transfer before it is older, and a step back into the
        // entry comes later.
        let mut program = super::tests::program(RecoveryRequired::Never);
        program.sections.push(section(END, END + 1));
        let mut j = journal_until(&program, END);
        j.commit_intent(at(END)).unwrap();
        entry(&mut j, &program, u64::from(END) + 1);
        let decision = decide(&program, &j);
        assert!(refused_at(decision.clone(), END), "{decision:?}");
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

    /// `program` with its plan's entry at pc 0, as a job that starts at the entry has it.
    fn program_entered_at_zero() -> Program {
        let mut program = program(RecoveryRequired::Never);
        program.flash[0].boundaries.entry_pc = 0;
        program
    }

    fn new_journal() -> Journal<Memory> {
        Journal::on_store(
            Memory,
            JobKey {
                job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e6f".to_owned()),
                generation: 1,
            },
        )
    }

    fn state_bytes(state: &VmState) -> Vec<u8> {
        postcard::to_allocvec(state).unwrap()
    }

    /// A state of the entry-at-zero program that a run left behind on a jump back to pc 0: not
    /// the initial one (a value on the stack).
    fn jump_state(program: &Program, steps: u64) -> VmState {
        let mut state = Vm::new(program).state;
        state.stack.push(diag_ir::Value::I64(9));
        state.steps = steps;
        state
    }

    /// The done-when case (ADR-272): run 1 jumped back to pc 0 (state S1) and crashed before the
    /// erase; run 2, a plain start, committed its run start and was interrupted in the transfer.
    /// The restart's entry state is run 2's initial state, not S1.
    #[test]
    fn a_run_start_is_the_entry_state_of_a_run_that_started_at_the_entry() {
        let program = program_entered_at_zero();
        let mut j = new_journal();
        j.commit_run_start(at(0), &state_bytes(&Vm::new(&program).state))
            .unwrap();
        j.commit_step(
            StepRef { pc: 2, steps: 1 },
            Some(&state_bytes(&jump_state(&program, 2))),
        )
        .unwrap();
        // Run 2: plain start on this journal, with the step count the journal says is next.
        let next = next_steps(j.state());
        let mut initial = Vm::new(&program).state;
        initial.steps = next;
        j.commit_run_start(StepRef { pc: 0, steps: next }, &state_bytes(&initial))
            .unwrap();
        let erase = StepRef {
            pc: ERASE,
            steps: next + 1,
        };
        j.commit_transfer_start(StageId(7), erase).unwrap();
        let RestartDecision::Restart(point) = decide(&program, &j) else {
            panic!("{:?}", decide(&program, &j));
        };
        let mut expected = Vm::new(&program).state;
        expected.steps = next_steps(j.state());
        assert_eq!(point.entry_state, expected);
        assert!(point.entry_state.stack.is_empty());
    }

    /// A jump back to the entry after the run start is newer and wins over it.
    #[test]
    fn a_jump_back_to_the_entry_wins_over_the_run_start() {
        let program = program_entered_at_zero();
        let mut j = new_journal();
        j.commit_run_start(at(0), &state_bytes(&Vm::new(&program).state))
            .unwrap();
        let jumped = jump_state(&program, 2);
        j.commit_step(StepRef { pc: 2, steps: 1 }, Some(&state_bytes(&jumped)))
            .unwrap();
        j.commit_transfer_start(StageId(7), at(ERASE)).unwrap();
        let RestartDecision::Restart(point) = decide(&program, &j) else {
            panic!("{:?}", decide(&program, &j));
        };
        let mut expected = jumped;
        expected.steps = next_steps(j.state());
        assert_eq!(point.entry_state, expected);
    }

    /// A journal written before the run-start record holds no VM state at all: for a plan
    /// entered at pc 0 the entry state is the program's initial one.
    #[test]
    fn a_journal_without_any_vm_state_restarts_from_the_initial_state_at_pc_0() {
        let program = program_entered_at_zero();
        let mut j = new_journal();
        j.commit_transfer_start(StageId(7), at(ERASE)).unwrap();
        assert!(j.state().last_vm_state.is_none());
        let RestartDecision::Restart(point) = decide(&program, &j) else {
            panic!("{:?}", decide(&program, &j));
        };
        let mut expected = Vm::new(&program).state;
        expected.steps = next_steps(j.state());
        assert_eq!(point.entry_state, expected);
    }

    /// An old-style jump-back state at the entry, then a newer run start: the run start wins.
    #[test]
    fn a_run_start_wins_over_an_older_jump_back_state() {
        let program = program_entered_at_zero();
        let mut j = new_journal();
        j.commit_step(
            StepRef { pc: 2, steps: 1 },
            Some(&state_bytes(&jump_state(&program, 2))),
        )
        .unwrap();
        let next = next_steps(j.state());
        let mut initial = Vm::new(&program).state;
        initial.steps = next;
        j.commit_run_start(StepRef { pc: 0, steps: next }, &state_bytes(&initial))
            .unwrap();
        j.commit_transfer_start(StageId(7), at(ERASE)).unwrap();
        let RestartDecision::Restart(point) = decide(&program, &j) else {
            panic!("{:?}", decide(&program, &j));
        };
        let mut expected = Vm::new(&program).state;
        expected.steps = next_steps(j.state());
        assert_eq!(point.entry_state, expected);
        assert!(point.entry_state.stack.is_empty());
    }

    #[test]
    fn a_run_start_that_is_unusable_needs_on_site_intervention() {
        let program = program_entered_at_zero();
        let restart_with = |program: &Program, run_start: &[u8]| {
            let mut j = new_journal();
            j.commit_run_start(at(0), run_start).unwrap();
            j.commit_transfer_start(StageId(7), at(ERASE)).unwrap();
            decide(program, &j)
        };
        assert!(matches!(
            restart_with(&program, b"not a state"),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::InvalidEntryState(_))
        ));
        let mut wrong_schema = Vm::new(&program).state;
        wrong_schema.schema_version = 1;
        assert!(matches!(
            restart_with(&program, &state_bytes(&wrong_schema)),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::InvalidEntryState(_))
        ));
        // A plan entered at pc 2: the run start at pc 0 is no state at the entry.
        let program = self::program(RecoveryRequired::Never);
        assert_eq!(
            restart_with(&program, &state_bytes(&Vm::new(&program).state)),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::MissingEntryState {
                flash_session: 1
            })
        );
    }

    /// A run start names no request: it does not move the interruption point, and it counts for
    /// the next run's step count.
    #[test]
    fn a_run_start_moves_no_interruption_point_but_counts() {
        let program = program(RecoveryRequired::Never);
        // A journal from before the erase: a run start never follows the transfer-start marker.
        let mut j = journal_until(&program, ENTRY);
        let point = interruption_point(&j.state().facts);
        let next = next_steps(j.state());
        j.commit_run_start(
            StepRef { pc: 0, steps: next },
            &state_bytes(&Vm::new(&program).state),
        )
        .unwrap();
        assert_eq!(interruption_point(&j.state().facts), point);
        assert_eq!(next_steps(j.state()), next + 1);
        // A run that crashed right after its run start still advances the count on its own.
        let mut j = new_journal();
        j.commit_run_start(at(0), &state_bytes(&Vm::new(&program).state))
            .unwrap();
        assert_eq!(interruption_point(&j.state().facts), None);
        assert_eq!(next_steps(j.state()), 1);
    }

    /// A checkpoint newer than every request and step counts for the next run's step count: the
    /// next run's first record comes after it.
    #[test]
    fn a_checkpoint_newer_than_the_last_request_counts() {
        let program = program(RecoveryRequired::Never);
        let mut j = new_journal();
        let bytes = state_bytes(&Vm::new(&program).state);
        j.commit_run_start(at(0), &bytes).unwrap();
        j.commit_request(at(3)).unwrap();
        j.commit_checkpoint(at(3), &bytes).unwrap();
        assert_eq!(next_steps(j.state()), 4);
        // A primitive that is safe to repeat leaves a checkpoint and no marker.
        j.commit_checkpoint(at(4), &bytes).unwrap();
        assert_eq!(j.state().facts.last_request, Some(at(3)));
        assert_eq!(next_steps(j.state()), 5);
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
        fn send_receive(&mut self, _: u32, _: &[u8]) -> Result<Vec<u8>, HostError> {
            Err(HostError::Unsupported("SendReceive"))
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
        fn artifact_bytes(&mut self, _: u64, _: u32) -> Result<Vec<u8>, HostError> {
            Err(HostError::Unsupported("FlashBlock"))
        }
        fn runtime_input(&mut self, _: diag_ir::RuntimeInput) -> Result<i64, HostError> {
            Err(HostError::Unsupported("ReadInput"))
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

    /// A completion an earlier pass left behind, with the interruption in a later pass, is not
    /// the completed path (ADR-269 item 2): a gate that did not pass makes the teardown passive,
    /// and it waits.
    #[test]
    fn an_earlier_passs_completion_leaves_the_teardown_to_the_gate() {
        let (_, mut point) = point_at(EXIT);
        let exit = point
            .facts
            .transfer
            .as_mut()
            .and_then(|transfer| transfer.exit.as_mut())
            .expect("the exit intent");
        exit.complete = true;
        let completed_at = exit.last_post_step.unwrap_or(exit.intent_at);
        // A step of the later pass, after the completion.
        point.facts.last_step = Some(StepRef {
            pc: 0,
            steps: completed_at.steps + 1,
        });
        assert!(!completed_pass_interrupted(&point.facts));
        let mut host = ecu_in_session();
        let (result, elapsed) = run_teardown(
            TeardownGate::PassiveOnly(PassiveReason::VinNotEstablished),
            &point,
            &mut host,
        );
        assert_eq!(
            result.unwrap(),
            Teardown::Passive(PassiveCause::Gate(PassiveReason::VinNotEstablished))
        );
        assert_eq!(host.0.power_cycles(), 0);
        assert!(elapsed >= Duration::from_millis(20), "{elapsed:?}");
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

    /// A Starlark section function that calls a helper also called outside it (ADR-301): the
    /// helper's instructions are in the section only on the section's path, so the agent treats
    /// an interruption in the section's copy as inside a recovery-required section and one in the
    /// copy `main` calls directly as outside every section.
    #[test]
    fn a_lowered_section_covers_its_helper_only_on_its_own_path() {
        use diag_frontend::star::{RequestIndex, check, lower};
        use diag_ir::declaration::{
            EcuDocumentT, EcuVariantT, encode_ecu_document, load_ecu_document,
        };
        let source = "\
def helper():
    diag.log(1, \"helper\")

# @section recoveryRequired
def write():
    helper()

def main():
    helper()
    write()
";
        let checked = check("t.star", source).expect("checks");
        let mut document = EcuDocumentT::default();
        document.schema_version = IR_SCHEMA_VERSION;
        document.source_format = Some("test".to_owned());
        document.source_digest = Some(String::new());
        document.variant = Some(Box::new(EcuVariantT::default()));
        let bytes = encode_ecu_document(&document);
        let variant = load_ecu_document(&bytes).unwrap().variant().unwrap();
        let program = lower("t.star", &checked, variant, &RequestIndex::default())
            .expect("lowers")
            .program;
        program.validate().expect("valid");
        let helper = program
            .constants
            .iter()
            .position(|c| c.as_slice() == b"helper")
            .unwrap() as u32;
        let logs: Vec<bool> = (0..program.code.len() as u32)
            .filter(|&pc| matches!(program.code[pc as usize], Op::Log { message, .. } if message == helper))
            .map(|pc| in_section_outside_plans(&program, pc))
            .collect();
        let mut sorted = logs.clone();
        sorted.sort();
        assert_eq!(sorted, [false, true], "{program:#?}");
        // Each call reaches the copy on its own side: `main`'s the one outside the section, and
        // `write`'s the one inside it.
        let logs_at: Vec<u32> = (0..program.code.len() as u32)
            .filter(|&pc| matches!(program.code[pc as usize], Op::Log { message, .. } if message == helper))
            .collect();
        let mut calls: Vec<(bool, bool)> = program
            .code
            .iter()
            .enumerate()
            .filter_map(|(pc, op)| match op {
                Op::Call(target) => {
                    let end = (*target..program.code.len() as u32)
                        .find(|&at| matches!(program.code[at as usize], Op::Ret))?;
                    logs_at
                        .iter()
                        .any(|log| (*target..end).contains(log))
                        .then(|| {
                            (
                                in_section_outside_plans(&program, pc as u32),
                                in_section_outside_plans(&program, *target),
                            )
                        })
                }
                _ => None,
            })
            .collect();
        calls.sort();
        assert_eq!(calls, [(false, false), (true, true)]);
    }

    /// The first variant's program of `main.star` lowered against the example ECU, which declares
    /// flash session 1, and the plan it holds.
    fn lowered_example_plan(main: &str) -> (Program, FlashRecovery) {
        use diag_frontend::convert::lower_procedures;
        use diag_frontend::tables::{read_files, resolve};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../diag-frontend/examples/example-ecu");
        let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
            .expect("the example directory")
            .map(|entry| {
                let entry = entry.expect("an entry");
                let name = entry.file_name().into_string().expect("a UTF-8 name");
                // A Windows checkout may have CRLF line ends.
                let text = std::fs::read_to_string(entry.path())
                    .expect("a text file")
                    .replace("\r\n", "\n");
                (name, text.into_bytes())
            })
            .collect();
        files.retain(|(name, _)| name != "main.star");
        files.push(("main.star".to_owned(), main.as_bytes().to_vec()));
        files.sort();
        let definition = resolve(read_files(files).expect("reads")).expect("resolves");
        let procedures = lower_procedures(&definition).expect("lowers");
        let program: Program = postcard::from_bytes(&procedures[0].programs[0]).expect("a program");
        program.validate().expect("valid");
        let [plan] = &program.flash[..] else {
            panic!("one plan: {program:#?}");
        };
        let plan = plan.clone();
        (program, plan)
    }

    /// A flash plan of the example ECU's session 1, then the `after` statements in `main`.
    fn plan_then(after: &str) -> String {
        format!(
            "\
# @section uninterruptible expected=180000
# @flashPlan session=1
def write():
    diag.request(0x10, {{\"sub\": 0x02}})
    # @erase
    diag.routine(0xFF00, 0x01, {{}})
    res = diag.flash_transfer(1)

# @preconditions voltage externalSupply ignition
def main():
    write()
{after}"
        )
    }

    /// The pc of the first `ServiceRequest` of `service` after the plan.
    fn request_after_plan(program: &Program, plan: &FlashRecovery, service: u16) -> u32 {
        (plan.boundaries.post_transfer_end_pc..program.code.len() as u32)
            .find(|&pc| matches!(program.code[pc as usize], Op::ServiceRequest { service: s } if s == service))
            .unwrap_or_else(|| panic!("no request {service:#x} after the plan: {program:#?}"))
    }

    /// ADR-332 item 1 on the restart's continuation check: a read after a lowered flash plan that
    /// carries no `@idempotency` is a covered `Safe` primitive, so `unsafe_continuation` passes it.
    #[test]
    fn a_lowered_unannotated_read_after_a_plan_is_a_safe_continuation() {
        let (program, plan) =
            lowered_example_plan(&plan_then("    diag.request(0x22, {\"did\": 0xF190})\n"));
        let read = request_after_plan(&program, &plan, 0x22);
        assert!(safe_to_repeat(&program, read), "{program:#?}");
        assert_eq!(unsafe_continuation(&program, &plan), None, "{program:#?}");
    }

    /// A primitive after the plan that is marked `@idempotency unsafe` is the continuation's
    /// refusal, and the unannotated read before it is not.
    #[test]
    fn a_lowered_unsafe_annotated_request_after_a_plan_is_an_unsafe_continuation() {
        let (program, plan) = lowered_example_plan(&plan_then(
            "    diag.request(0x22, {\"did\": 0xF190})\n    \
             # @idempotency unsafe\n    diag.request(0x11, {\"reset\": 1})\n",
        ));
        let read = request_after_plan(&program, &plan, 0x22);
        let reset = request_after_plan(&program, &plan, 0x11);
        assert!(read < reset);
        assert!(safe_to_repeat(&program, read), "{program:#?}");
        assert!(!safe_to_repeat(&program, reset), "{program:#?}");
        assert_eq!(
            unsafe_continuation(&program, &plan),
            Some(reset),
            "{program:#?}"
        );
    }

    // ------------------------------------------------ a plain procedure (ADR-335 item 3)

    const VIN: &str = "WDB12345678901234";

    fn plain_section(
        start_pc: u32,
        end_pc: u32,
        idempotency: Idempotency,
        interruptible: Interruptible,
    ) -> Section {
        Section {
            start_pc,
            end_pc,
            interruptible,
            idempotency,
            expected_millis: 0,
        }
    }

    /// Three steps of an operand, a request and a Pop: the requests are at 1, 4 and 7. The first is
    /// under a `Safe` section, the second under none, the third under a `Safe` one; the VIN is
    /// read from a source.
    fn plain_program() -> Program {
        let mut code = Vec::new();
        for service in [0x22, 0x10, 0x3E] {
            code.extend([Op::PushBytes(0), Op::ServiceRequest { service }, Op::Pop]);
        }
        Program {
            schema_version: IR_SCHEMA_VERSION,
            code,
            constants: vec![vec![1]],
            sections: vec![
                plain_section(0, 3, Idempotency::Safe, Interruptible::Yes),
                plain_section(6, 9, Idempotency::Safe, Interruptible::Yes),
            ],
            source_map: Vec::new(),
            identity: diag_ir::IdentitySources {
                vin: Some(diag_ir::Source::EcuService {
                    service_id: 1,
                    field_id: 3,
                }),
                ..Default::default()
            },
            preconditions: Default::default(),
            flash: Vec::new(),
        }
    }

    /// A journal of a run that targets the VIN (when `vin`), started at pc 0 and sent a request
    /// at pc 1 that it saw confirmed by a checkpoint: the request is at step 2, the checkpoint's
    /// record at step 6 carries `state`.
    fn plain_journal(program: &Program, vin: bool, state: &VmState) -> Journal<Memory> {
        let mut journal = Journal::on_store(Memory, plain_key());
        if vin {
            journal
                .commit_target_vin(&crate::journal::Vin::new(VIN.to_owned()))
                .unwrap();
        }
        journal
            .commit_run_start(at(0), &state_bytes(&state_at_pc(program, 0, 0)))
            .unwrap();
        journal.commit_request(at(2)).unwrap();
        journal
            .commit_checkpoint(at(6), &state_bytes(state))
            .unwrap();
        journal
    }

    fn not_resumable(cause: NotResumableCause) -> RestartDecision {
        RestartDecision::OnSiteInterventionRequired(OnSiteReason::NotResumable { cause })
    }

    fn plain_key() -> JobKey {
        JobKey {
            job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e6f".to_owned()),
            generation: 1,
        }
    }

    /// A journal with no request makes a plain start, and one with a request newer than the
    /// newest state record, or no state record at all, is unconfirmed.
    #[test]
    fn a_plain_journal_is_a_plain_start_until_a_request_is_recorded() {
        let program = plain_program();
        let mut none = Journal::on_store(Memory, plain_key());
        none.commit_run_start(at(0), &state_bytes(&state_at_pc(&program, 0, 0)))
            .unwrap();
        none.commit_checkpoint(at(1), &state_bytes(&state_at_pc(&program, 2, 2)))
            .unwrap();
        assert_eq!(decide(&program, &none), RestartDecision::PlainStart);
        // A request with no state record at all: unconfirmed.
        let mut bare = Journal::on_store(Memory, plain_key());
        bare.commit_request(at(4)).unwrap();
        assert_eq!(
            decide(&program, &bare),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnconfirmedRequest {
                at: at(4)
            })
        );
        // A request at step 8, newer than the checkpoint's state (step 3).
        let mut journal = plain_journal(&program, true, &state_at_pc(&program, 5, 3));
        journal.commit_request(at(8)).unwrap();
        assert_eq!(
            decide(&program, &journal),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnconfirmedRequest {
                at: at(8)
            })
        );
    }

    /// A withdrawn request (its run ended with it provably unsent, ADR-392) counts as no request:
    /// alone it is a plain start; with an earlier confirmed request it is not unconfirmed, and the
    /// checks after that point still apply; a newer request that is not withdrawn is unconfirmed
    /// again.
    #[test]
    fn a_withdrawn_request_counts_as_none() {
        let program = plain_program();
        let mut alone = Journal::on_store(Memory, plain_key());
        alone
            .commit_run_start(at(0), &state_bytes(&state_at_pc(&program, 0, 0)))
            .unwrap();
        alone.commit_request(at(4)).unwrap();
        assert!(matches!(
            decide(&program, &alone),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnconfirmedRequest { .. })
        ));
        alone.commit_request_not_sent(at(4)).unwrap();
        assert_eq!(decide(&program, &alone), RestartDecision::PlainStart);

        // After a confirmed request at step 2 (the checkpoint's state is step 3), a request at
        // step 8 that is withdrawn is no longer unconfirmed.
        let mut journal = plain_journal(&program, true, &state_at_pc(&program, 5, 3));
        journal.commit_request(at(8)).unwrap();
        journal.commit_request_not_sent(at(8)).unwrap();
        let decision = decide(&program, &journal);
        assert!(
            !matches!(
                decision,
                RestartDecision::OnSiteInterventionRequired(
                    OnSiteReason::UnconfirmedRequest { .. }
                )
            ),
            "{decision:?}"
        );
        // A newer request is not covered by the older withdrawal.
        journal.commit_request(at(9)).unwrap();
        assert_eq!(
            decide(&program, &journal),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnconfirmedRequest {
                at: at(9)
            })
        );
    }

    /// Two withdrawn requests in a row (a first run and a resume, each refused before the send)
    /// are no request: the next resume starts plain. A withdrawal after a confirmed request
    /// leaves the verdict that request gave; one after an unconfirmed request (a crash) leaves
    /// the earlier one unconfirmed.
    #[test]
    fn withdrawn_requests_leave_the_verdict_of_the_one_that_stands() {
        let program = plain_program();
        let start = state_bytes(&state_at_pc(&program, 0, 0));
        let mut twice = Journal::on_store(Memory, plain_key());
        twice.commit_run_start(at(0), &start).unwrap();
        twice.commit_request(at(4)).unwrap();
        twice.commit_request_not_sent(at(4)).unwrap();
        twice.commit_run_start(at(5), &start).unwrap();
        twice.commit_request(at(9)).unwrap();
        twice.commit_request_not_sent(at(9)).unwrap();
        assert_eq!(twice.state().facts.standing_request, None);
        assert_eq!(decide(&program, &twice), RestartDecision::PlainStart);

        let confirmed = plain_journal(&program, true, &state_at_pc(&program, 5, 3));
        let before = decide(&program, &confirmed);
        let mut withdrawn = confirmed;
        withdrawn.commit_request(at(8)).unwrap();
        withdrawn.commit_request_not_sent(at(8)).unwrap();
        // The same verdict; the resumed state's step count moves on past the withdrawn marker.
        match (before, decide(&program, &withdrawn)) {
            (
                RestartDecision::ResumeFromCheckpoint(a),
                RestartDecision::ResumeFromCheckpoint(b),
            ) => {
                assert_eq!((a.at, a.state.pc), (b.at, b.state.pc));
                assert!(b.state.steps > a.state.steps);
            }
            (a, b) => panic!("{a:?} then {b:?}"),
        }

        let mut crashed = Journal::on_store(Memory, plain_key());
        crashed.commit_run_start(at(0), &start).unwrap();
        crashed.commit_request(at(4)).unwrap();
        crashed.commit_request(at(8)).unwrap();
        crashed.commit_request_not_sent(at(8)).unwrap();
        assert_eq!(
            decide(&program, &crashed),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnconfirmedRequest {
                at: at(4)
            })
        );
    }

    /// The repeat decision (ADR-392), arm by arm. The program: requests at 1 (`Safe`), 4 (under
    /// no section) and 7 (`Safe`, in a `RecoveryRequired` section); a `Wait` at 9.
    #[test]
    fn the_repeat_decision_by_error_and_idempotency() {
        use crate::runner::JobLimits;
        let mut program = plain_program();
        program.sections[1].interruptible = Interruptible::RecoveryRequired;
        program.code.extend([Op::Wait { millis: 1 }]);
        let limits = JobLimits::default();
        // A journal with the request at pc 4 committed and no state after it.
        let mut journal = Journal::on_store(Memory, plain_key());
        journal
            .commit_run_start(at(0), &state_bytes(&state_at_pc(&program, 0, 0)))
            .unwrap();
        journal.commit_request(at(4)).unwrap();
        let some = journal.state().clone();
        let decide = |pc: u32, error: &HostError, attempts: u8, state: Option<&JournalState>| {
            repeat_decision(&program, at_pc(pc), error, attempts, &limits, state)
        };
        // `Safe`: an ECU-level error is sent again until the bound, then ends.
        for error in [
            HostError::NoResponse,
            HostError::EventsLost,
            HostError::PrimitiveFailed(3),
        ] {
            assert_eq!(decide(1, &error, 0, Some(&some)), RepeatDecision::Repeat);
            assert_eq!(decide(1, &error, 1, None), RepeatDecision::Repeat);
            assert_eq!(decide(1, &error, 2, Some(&some)), RepeatDecision::End);
        }
        // Not safe: on site with the reason a resume derives; without a journal, an
        // unconfirmed request.
        assert_eq!(
            decide(4, &HostError::NoResponse, 0, Some(&some)),
            RepeatDecision::OnSite(OnSiteReason::UnconfirmedRequest { at: at_pc(4) })
        );
        assert_eq!(
            decide(4, &HostError::NoResponse, 0, None),
            RepeatDecision::OnSite(OnSiteReason::UnconfirmedRequest { at: at_pc(4) })
        );
        // `Safe` inside a `RecoveryRequired` section outside the plans is sent again too.
        assert_eq!(
            decide(7, &HostError::NoResponse, 0, Some(&some)),
            RepeatDecision::Repeat
        );
        // A transient refusal is sent again whatever the idempotency, within the bound.
        assert_eq!(
            decide(4, &HostError::LinkHeld, 1, Some(&some)),
            RepeatDecision::Repeat
        );
        assert_eq!(
            decide(4, &HostError::LinkHeld, 2, Some(&some)),
            RepeatDecision::End
        );
        // Refusals that do not pass, and errors of the link's set-up, end the run.
        for error in [HostError::Setup("link"), HostError::NotAllowed(0x10)] {
            assert_eq!(
                decide(1, &error, 0, Some(&some)),
                RepeatDecision::End,
                "{error:?}"
            );
        }
        // A lost path to the VCI (ADR-393): a request that may have reached the ECU is sent
        // again, on a new link, only when it is safe to repeat, within the bound.
        for error in [
            HostError::WorkerUnresponsive("StartComPrimitive"),
            HostError::EventStreamEnded,
            HostError::VciLost {
                refused_after_loss: false,
            },
        ] {
            assert_eq!(
                decide(1, &error, 0, Some(&some)),
                RepeatDecision::ReopenThenRepeat,
                "{error:?}"
            );
            assert_eq!(
                decide(1, &error, 1, None),
                RepeatDecision::ReopenThenRepeat,
                "{error:?}"
            );
            assert_eq!(
                decide(1, &error, 2, Some(&some)),
                RepeatDecision::End,
                "{error:?}"
            );
            // Not safe: the verdict of the restart's classification, as for an ECU-level error.
            assert_eq!(
                decide(4, &error, 0, Some(&some)),
                RepeatDecision::OnSite(OnSiteReason::UnconfirmedRequest { at: at_pc(4) }),
                "{error:?}"
            );
            assert_eq!(
                decide(4, &error, 0, None),
                RepeatDecision::OnSite(OnSiteReason::UnconfirmedRequest { at: at_pc(4) }),
                "{error:?}"
            );
        }
        // A request the worker refused after the loss was never sent: sent again whatever the
        // idempotency, within the bound.
        let refused = HostError::VciLost {
            refused_after_loss: true,
        };
        for pc in [1, 4, 7] {
            assert_eq!(
                decide(pc, &refused, 1, Some(&some)),
                RepeatDecision::ReopenThenRepeat,
                "pc {pc}"
            );
            assert_eq!(
                decide(pc, &refused, 2, Some(&some)),
                RepeatDecision::End,
                "pc {pc}"
            );
        }
        assert_eq!(
            decide(9, &HostError::NoResponse, 0, Some(&some)),
            RepeatDecision::End
        );
    }

    /// ADR-393: the program's part of a continuation after a lost path, judged at the VM's
    /// current pc. `plain_program`: requests at 1 and 7 are `Safe` in their sections, the one at
    /// 4 is under no section and so not safe to repeat.
    #[test]
    fn a_continuation_after_a_loss_needs_an_interruptible_point_and_nothing_unsafe_ahead() {
        let program = plain_program();
        let at = |pc| state_at_pc(&program, pc, u64::from(pc));
        // From the last step, only `Safe` code lies ahead.
        assert!(continues_after_loss(&program, &at(7), false));
        assert!(continues_after_loss(&program, &at(7), true));
        // The first step reaches the request at 4, which is not safe to repeat.
        assert!(!continues_after_loss(&program, &at(1), false));
        assert!(!continues_after_loss(&program, &at(1), true));
        // The unsafe request itself: sent again only when it never left, as its successors are
        // safe; one that may have reached the ECU rules the continuation out.
        assert!(!continues_after_loss(&program, &at(4), false));
        assert!(continues_after_loss(&program, &at(4), true));
        // A section that is not interruptible, or recovery-required, is no point to continue at.
        for interruptible in [Interruptible::No, Interruptible::RecoveryRequired] {
            let mut program = plain_program();
            program.sections[1].interruptible = interruptible;
            let state = state_at_pc(&program, 7, 7);
            assert!(!continues_after_loss(&program, &state, false));
            assert!(!continues_after_loss(&program, &state, true));
        }
    }

    fn at_pc(pc: u32) -> StepRef {
        at_step(pc, u64::from(pc))
    }

    fn at_step(pc: u32, steps: u64) -> StepRef {
        StepRef { pc, steps }
    }

    /// Inside a `RecoveryRequired` section outside the plans, an `Unsafe` request that may have
    /// reached the ECU ends in the section's reason, naming the pass's intent as `classify` does.
    #[test]
    fn the_repeat_decision_names_the_recovery_required_section() {
        use crate::runner::JobLimits;
        let mut program = plain_program();
        program.sections[1] =
            plain_section(6, 9, Idempotency::Unsafe, Interruptible::RecoveryRequired);
        let mut journal = Journal::on_store(Memory, plain_key());
        journal.commit_intent(at(6)).unwrap();
        let decision = repeat_decision(
            &program,
            at_step(7, 7),
            &HostError::NoResponse,
            0,
            &JobLimits::default(),
            Some(journal.state()),
        );
        assert_eq!(
            decision,
            RepeatDecision::OnSite(OnSiteReason::RecoveryRequiredSection {
                section: 1,
                at: at(6)
            })
        );
    }

    /// The comparison is with the state's own step count: a checkpoint is taken after the
    /// completed step, so its state is one past its record, and a request at the record's own step
    /// is confirmed by it, while one at the state's step is not.
    #[test]
    fn a_request_is_confirmed_by_a_state_after_its_step() {
        let program = plain_program();
        // The request at step 2; the checkpoint's state says step 3: confirmed.
        let journal = plain_journal(&program, true, &state_at_pc(&program, 5, 3));
        assert!(matches!(
            decide(&program, &journal),
            RestartDecision::ResumeFromCheckpoint(_)
                | RestartDecision::OnSiteInterventionRequired(_)
        ));
        assert!(!matches!(
            decide(&program, &journal),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnconfirmedRequest { .. })
        ));
        // A state at step 2 is the step about to run, the request's own: unconfirmed.
        let journal = plain_journal(&program, true, &state_at_pc(&program, 5, 2));
        assert_eq!(
            decide(&program, &journal),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnconfirmedRequest {
                at: at(2)
            })
        );
    }

    /// The checkpoint stands at pc 2, ahead of the requests at 4 (under no section) and 7.
    #[test]
    fn the_lowest_hazard_ahead_is_named_whatever_its_kind() {
        let state = |program: &Program| state_at_pc(program, 2, 7);
        // The unsafe request at 4 comes before the recovery-required section at 6 to 8.
        let mut program = plain_program();
        program.sections[1].interruptible = Interruptible::RecoveryRequired;
        let journal = plain_journal(&program, true, &state(&program));
        assert_eq!(
            decide(&program, &journal),
            not_resumable(NotResumableCause::UnsafePrimitiveAhead { pc: 4 })
        );
        // Covered by a `Safe` section, the request at 4 is fine, and the section at 6 refuses.
        program
            .sections
            .push(plain_section(3, 6, Idempotency::Safe, Interruptible::Yes));
        let journal = plain_journal(&program, true, &state(&program));
        assert_eq!(
            decide(&program, &journal),
            not_resumable(NotResumableCause::RecoveryRequiredAhead { section: 1, pc: 7 })
        );
        // A primitive in a recovery-required section that is also `Unsafe` is that, not unsafe.
        program.sections[1].idempotency = Idempotency::Unsafe;
        let journal = plain_journal(&program, true, &state(&program));
        assert_eq!(
            decide(&program, &journal),
            not_resumable(NotResumableCause::RecoveryRequiredAhead { section: 1, pc: 7 })
        );
        // Everything ahead safe: the checkpoint resume is decided.
        let mut program = plain_program();
        program
            .sections
            .push(plain_section(3, 6, Idempotency::Safe, Interruptible::Yes));
        let journal = plain_journal(&program, true, &state(&program));
        let RestartDecision::ResumeFromCheckpoint(point) = decide(&program, &journal) else {
            panic!("{:?}", decide(&program, &journal));
        };
        assert_eq!(point.at, at(6));
        assert_eq!(point.state.pc, 2);
        // The step count goes on after the journal's records.
        assert_eq!(point.state.steps, next_steps(journal.state()));
    }

    /// Reachability follows the checkpoint's suspended callers: a return goes on in the caller,
    /// where an unsafe request lies, but only with the frame in the state.
    #[test]
    fn a_return_to_the_caller_counts_toward_what_lies_ahead() {
        // 0 calls the function at 5; the caller goes on at 1 with an unsafe request at 2; the
        // function reads at 6 under a `Safe` section and returns at 8.
        let mut program = plain_program();
        program.code = vec![
            Op::Call(5),
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x10 },
            Op::Pop,
            Op::Ret,
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::Pop,
            Op::Ret,
        ];
        program.sections = vec![plain_section(5, 8, Idempotency::Safe, Interruptible::Yes)];
        let mut inside = state_at_pc(&program, 7, 7);
        // With no suspended caller the return at 8 ends the program: nothing unsafe ahead.
        let journal = plain_journal(&program, true, &inside);
        assert!(matches!(
            decide(&program, &journal),
            RestartDecision::ResumeFromCheckpoint(_)
        ));
        inside.call_stack = vec![diag_ir::Frame {
            return_pc: 1,
            locals: Vec::new(),
        }];
        let journal = plain_journal(&program, true, &inside);
        assert_eq!(
            decide(&program, &journal),
            not_resumable(NotResumableCause::UnsafePrimitiveAhead { pc: 2 })
        );
    }

    #[test]
    fn a_plan_ahead_is_named_before_the_primitives_in_it() {
        // The checkpoint at 0 of the plan fixture: its entry (2) is ahead, and so is its erase (3).
        let mut program = program(RecoveryRequired::Never);
        program.identity.vin = Some(diag_ir::Source::EcuService {
            service_id: 1,
            field_id: 3,
        });
        let journal = plain_journal(&program, true, &state_at_pc(&program, 0, 7));
        assert_eq!(
            decide(&program, &journal),
            not_resumable(NotResumableCause::PlanAhead { flash_session: 1 })
        );
    }

    /// The checks come in order: an uninterruptible point before what lies ahead, what lies ahead
    /// before the identity; a state that fails `Vm::check_state` or does not decode is an invalid
    /// state.
    #[test]
    fn the_checkpoint_resume_conditions_are_checked_in_order() {
        let mut program = plain_program();
        program
            .sections
            .push(plain_section(3, 6, Idempotency::Safe, Interruptible::Yes));
        // No VIN named and an unsafe request ahead (4 is under no section here): the request.
        let mut open = program.clone();
        open.sections.pop();
        let journal = plain_journal(&open, false, &state_at_pc(&open, 2, 7));
        assert_eq!(
            decide(&open, &journal),
            not_resumable(NotResumableCause::UnsafePrimitiveAhead { pc: 4 })
        );
        // Nothing unsafe ahead: no VIN named, then no VIN source.
        let journal = plain_journal(&program, false, &state_at_pc(&program, 2, 7));
        assert_eq!(
            decide(&program, &journal),
            not_resumable(NotResumableCause::NoIdentity)
        );
        let mut no_source = program.clone();
        no_source.identity.vin = None;
        let journal = plain_journal(&no_source, true, &state_at_pc(&no_source, 2, 7));
        assert_eq!(
            decide(&no_source, &journal),
            not_resumable(NotResumableCause::NoIdentity)
        );
        // An uninterruptible point first, even with no VIN named.
        let mut held = program.clone();
        held.sections
            .push(plain_section(2, 3, Idempotency::Safe, Interruptible::No));
        let journal = plain_journal(&held, false, &state_at_pc(&held, 2, 7));
        assert_eq!(
            decide(&held, &journal),
            not_resumable(NotResumableCause::UninterruptibleSection { pc: 2 })
        );
        // A state the VM refuses: its pc is far past the code.
        let journal = plain_journal(&program, true, &state_at_pc(&program, 99, 7));
        assert!(matches!(
            decide(&program, &journal),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::InvalidEntryState(_))
        ));
        // A state that does not decode.
        let mut journal = plain_journal(&program, true, &state_at_pc(&program, 2, 7));
        journal.commit_checkpoint(at(7), b"junk").unwrap();
        assert!(matches!(
            decide(&program, &journal),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::InvalidEntryState(_))
        ));
    }

    /// A damaged state is an invalid state before the conditions that read it, so a bad pc is not
    /// reported as a missing identity and a call stack past the limit is not walked.
    #[test]
    fn a_damaged_checkpoint_state_is_invalid_before_the_conditions() {
        let program = plain_program();
        let invalid = |journal: Journal<Memory>| {
            assert!(
                matches!(
                    decide(&program, &journal),
                    RestartDecision::OnSiteInterventionRequired(OnSiteReason::InvalidEntryState(_))
                ),
                "{:?}",
                decide(&program, &journal)
            );
        };
        // A bad pc, with no target VIN named.
        invalid(plain_journal(
            &program,
            false,
            &state_at_pc(&program, 99, 7),
        ));
        // A call stack longer than the VM allows.
        let mut deep = state_at_pc(&program, 2, 7);
        deep.call_stack = vec![
            diag_ir::Frame {
                return_pc: 0,
                locals: Vec::new(),
            };
            diag_ir::MAX_CALL_DEPTH + 1
        ];
        invalid(plain_journal(&program, false, &deep));
    }

    /// A checkpoint on a `Ret` is not resumed from, outside a section as inside one that is not
    /// `Yes` (ADR-330 item 2, ADR-335 item 3).
    #[test]
    fn a_checkpoint_on_a_return_is_not_resumable() {
        let mut program = plain_program();
        program.code[2] = Op::Ret;
        let journal = plain_journal(&program, true, &state_at_pc(&program, 2, 7));
        assert_eq!(
            decide(&program, &journal),
            not_resumable(NotResumableCause::UninterruptibleSection { pc: 2 })
        );
        program
            .sections
            .push(plain_section(2, 3, Idempotency::Safe, Interruptible::No));
        let journal = plain_journal(&program, true, &state_at_pc(&program, 2, 7));
        assert_eq!(
            decide(&program, &journal),
            not_resumable(NotResumableCause::UninterruptibleSection { pc: 2 })
        );
    }

    /// ADR-330: an instruction is an interruptible point unless it lies in a section that is not
    /// `Yes` or is a `Ret`; a position past the code has neither.
    #[test]
    fn interruptible_points_follow_the_sections_and_returns() {
        let mut program = plain_program();
        program.code[2] = Op::Ret;
        program.sections = vec![
            plain_section(3, 4, Idempotency::Safe, Interruptible::No),
            plain_section(4, 5, Idempotency::Safe, Interruptible::RecoveryRequired),
            plain_section(5, 6, Idempotency::Safe, Interruptible::Yes),
        ];
        let points: Vec<bool> = (0..=program.code.len() as u32)
            .map(|pc| interruptible_point(&program, pc))
            .collect();
        assert_eq!(
            points,
            [
                true, true, false, false, false, true, true, true, true, true
            ]
        );
    }
}
