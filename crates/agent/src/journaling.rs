//! The job runner's write-job journal (design 5.5, 8.2.5; ADR-229, ADR-244, ADR-252).
//!
//! For a procedure with a flash recovery plan, the runner commits to the journal in two places.
//!
//! When a run from instruction 0 starts, before anything is sent ([`JobJournal::run_start`]): the
//! VM state it starts from, so the journal marks where each such run began (ADR-272). The
//! restart's replay to the erase commits one too, carrying the entry state it starts from
//! (ADR-274); the continuation after it commits none.
//!
//! When execution arrives at an instruction, before it runs and once the VM's own checks of it
//! passed (`Vm::current_op`, ADR-233 item 3) ([`JobJournal::arrive`]):
//! - at a plan's `entry_pc`, once per job and before its first transfer: the ECU hardware part
//!   number and software version, read through the sources the program declares;
//! - at `erase_pc`: the transfer-start marker;
//! - at `transfer_exit_pc`: the RequestTransferExit marker;
//! - at the first diagnostic primitive at or past a plan's recovery-required point that is
//!   neither of those, once per pass through the plan: a request intent (ADR-253);
//! - at the first diagnostic primitive of a pass through a section marked `RecoveryRequired`
//!   outside every plan: a request intent (ADR-291);
//! - at every diagnostic primitive outside every plan's range (`entry_pc` up to, not including,
//!   `post_transfer_end_pc`) that is not safe to repeat: a request marker (ADR-335 item 2).
//!
//! When an instruction completes ([`JobJournal::completed`]):
//! - a `FlashTransfer`: the block, by the host's running index;
//! - a diagnostic primitive inside a plan's range: a step record, so the journal's last step
//!   orders the interruption point (ADR-229 item 1, ADR-245 item 4);
//! - a step that brings execution to a plan's `entry_pc`: a step record with the VM state
//!   after it (a job that starts at the entry has its run start instead);
//! - a step from inside a plan (`entry_pc..post_transfer_end_pc`) that brings execution to its
//!   `post_transfer_end_pc`, whatever its instruction: a step record with the VM state after it
//!   (ADR-271 item 2, ADR-274; one record when that pc is also a plan's entry; a jump to the end
//!   from outside the plan records nothing), then, after the plan's exit marker, the
//!   post-transfer completion;
//! - a diagnostic primitive outside every plan's range: a checkpoint with the VM state after it
//!   (ADR-335 item 2), except where the step brings execution to a plan's `entry_pc`, whose
//!   step record carries that state already. A state too large for a frame leaves no checkpoint
//!   and does not end the job.
//!
//! When a run ends with the request marker it committed provably unsent (a refusal before the
//! start, or a cancel before the send), the marker is withdrawn: a request-not-sent record
//! ([`JobJournal::withdraw_request`], ADR-392).
//!
//! When the procedure ends itself as failed (`Op::Fail`): a procedure-failure record, the job's
//! last ([`JobJournal::procedure_failed`], ADR-280). A restart's identity abort is committed by
//! the runner on the journal itself (`Journal::commit_identity_mismatch`, ADR-286).
//!
//! A commit that fails, or an identity that cannot be read, ends the job before the next
//! instruction runs, so the request a marker guards is never sent without it (ADR-244 item 7).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use diag_ir::{
    DiagHost, FlashRecovery, IdentityKind, NoApplication, Op, Program, RecoveryRequired, VmState,
};

use crate::host::{HostError, TransferProgress};
use crate::inputs::{FieldBytes, ServiceSources, read_identity_bytes};
use crate::journal::{
    FileStore, JobKey, Journal, JournalError, JournalState, RunStop, StageId, StepRef, Store, Vin,
};
use crate::restart::{in_section_outside_plans, needs_request_marker};
use crate::runner::JobError;

/// Where a job keeps its journal, under which key, and how it reads the ECU's identity.
#[derive(Debug, Clone)]
pub struct JournalSetup {
    /// The directory of the journal file (ADR-244 item 7).
    pub dir: PathBuf,
    pub key: JobKey,
    /// The table that maps the program's identity sources to requests (`inputs`). Used only for
    /// a job without a document: a job with one reads its sources through its document, and the
    /// runner replaces this table with the one built from it
    /// (`ServiceSources::from_document`, ADR-345).
    pub sources: ServiceSources,
    /// The VIN the job targets. A first run records it as the journal's first record, and a
    /// resume must name the same one (ADR-261); the restart's gates compare the ECU's VIN
    /// against it. `None` when the job names none: a restart then never counts the vehicle as
    /// identified. It is personal data (design 16.2) and is never logged or put in an error.
    pub vin: Option<Vin>,
    /// The software version the job intends to write (the procedure's software-version field,
    /// raw bytes). A first run records it right after the target VIN in the creating write, and
    /// a resume must name the same one, none included (ADR-268). `None` when the job names none.
    pub intended_software_version: Option<Vec<u8>>,
}

/// A job's open journal and what it needs to commit at the plan's boundaries.
pub(crate) struct JobJournal<S = FileStore> {
    journal: Journal<S>,
    sources: ServiceSources,
    /// The identity was read for this job.
    identity_read: bool,
    /// Stages whose recovery-required point the journal already places the interruption at or
    /// past (an intent, or the erase or RequestTransferExit marker at or past it).
    past_recovery_point: Vec<u32>,
    /// Execution is in a pass through a `RecoveryRequired` section outside every plan whose
    /// intent the journal holds (ADR-291).
    in_recovery_section: bool,
}

impl JobJournal {
    /// Creates the job's journal. An existing journal of the same key is an error: a job that
    /// ran before goes on through `resume_program_journaled`, not a first run.
    ///
    /// `artifact_sha256` is the hash of the job's flash artifact, recorded in the creating write
    /// so every resume can compare it with the artifact it is given (ADR-344).
    pub(crate) fn create(
        setup: JournalSetup,
        artifact_sha256: Option<&[u8; 32]>,
    ) -> Result<Self, JournalError> {
        Ok(Self::new(
            Journal::create(
                &setup.dir,
                &setup.key,
                setup.vin.as_ref(),
                setup.intended_software_version.as_deref(),
                artifact_sha256,
            )?,
            setup.sources,
        ))
    }
}

impl<S: Store> JobJournal<S> {
    /// Commits a job's run to `journal`: a new one, or an existing one a plain start goes on
    /// with (ADR-255).
    pub(crate) fn new(journal: Journal<S>, sources: ServiceSources) -> Self {
        Self {
            journal,
            sources,
            identity_read: false,
            past_recovery_point: Vec::new(),
            in_recovery_section: false,
        }
    }

    /// The journal, for the commits of a run that goes on after its link was lost (ADR-393).
    pub(crate) fn journal_mut(&mut self) -> &mut Journal<S> {
        &mut self.journal
    }

    /// The table that maps the program's identity sources to requests.
    pub(crate) fn sources(&self) -> &ServiceSources {
        &self.sources
    }

    #[cfg(test)]
    pub(crate) fn journal(&self) -> &Journal<S> {
        &self.journal
    }

    #[cfg(test)]
    pub(crate) fn into_journal(self) -> Journal<S> {
        self.journal
    }

    /// Commits the VM state the run starts from, at the step count it starts at, as the run's
    /// first record (ADR-272). Called before the run's first arrival, so before any request.
    pub(crate) fn run_start(&mut self, state: &VmState) -> Result<(), JobError> {
        let encoded = postcard::to_allocvec(state)
            .map_err(|_| JournalError::Invariant("the VM state does not encode"))?;
        self.journal.commit_run_start(
            StepRef {
                pc: state.pc,
                steps: state.steps,
            },
            &encoded,
        )?;
        Ok(())
    }

    /// Commits the procedure's declared failure at `at`, the `Fail` instruction, as the job's
    /// last record (ADR-280). The caller bounds the text. A failed commit is the job's error, so
    /// the journal never claims a run end it does not hold.
    pub(crate) fn procedure_failed(
        &mut self,
        at: StepRef,
        code: &str,
        detail: &str,
    ) -> Result<(), JobError> {
        self.journal.commit_procedure_failed(at, code, detail)?;
        Ok(())
    }

    /// Commits a run's stop at `at` for `reason` as the job's last record (ADR-288). A failed
    /// commit is the job's error, as for [`Self::procedure_failed`].
    pub(crate) fn run_stopped(&mut self, at: StepRef, reason: RunStop) -> Result<(), JobError> {
        self.journal.commit_run_stopped(at, reason)?;
        Ok(())
    }

    /// Commits the markers and the identity the boundaries at `state.pc` call for. Called once
    /// each time execution arrives at an instruction, before it runs. A cancel stops it before
    /// each identity read (the runner's contract: no request after the one in flight).
    pub(crate) fn arrive<H>(
        &mut self,
        program: &Program,
        state: &VmState,
        host: &mut H,
        cancelled: &AtomicBool,
    ) -> Result<(), JobError>
    where
        H: DiagHost<Error = HostError>,
    {
        let pc = state.pc;
        let at = StepRef {
            pc,
            steps: state.steps,
        };
        for plan in &program.flash {
            let b = &plan.boundaries;
            // Execution that comes back to the entry runs the plan again, recovery point included.
            if pc == b.entry_pc {
                self.past_recovery_point
                    .retain(|stage| *stage != plan.stage);
            }
            // The pre-erase version belongs before the job's first transfer (ADR-244 item 4),
            // and a second read could fall in a session the ECU refuses it in.
            if pc == b.entry_pc
                && !self.identity_read
                && self.journal.state().facts.transfer.is_none()
            {
                self.record_identity(program, plan, pc, host, cancelled)?;
                self.identity_read = true;
            }
            if pc == b.erase_pc {
                self.journal
                    .commit_transfer_start(StageId(plan.stage), at)?;
            }
            if pc == b.transfer_exit_pc {
                self.journal.commit_transfer_exit_intent(at)?;
            }
            // The first primitive at or past the recovery-required point is written ahead, so a
            // crash before its response still places the interruption there (ADR-253). The
            // validator keeps execution from going back across the point (ADR-245 item 4), so
            // once is enough; the erase and RequestTransferExit markers already do it.
            if let RecoveryRequired::FromPc(from) = plan.recovery_required
                && (from..b.post_transfer_end_pc).contains(&pc)
                && !self.past_recovery_point.contains(&plan.stage)
            {
                let marked = pc == b.erase_pc || pc == b.transfer_exit_pc;
                if !marked && is_primitive(program, pc) {
                    self.intent(at, cancelled)?;
                }
                if marked || self.journal.state().facts.last_intent == Some(at) {
                    self.past_recovery_point.push(plan.stage);
                }
            }
        }
        // The first primitive of a pass through a `RecoveryRequired` section outside every plan
        // is written ahead too, so a crash before its response places the interruption in the
        // section (ADR-291). Inside a plan, the validator allows such a section only from the
        // plan's recovery-required point (ADR-245 item 3), which the loop above covers.
        if in_section_outside_plans(program, pc) {
            if !self.in_recovery_section && is_primitive(program, pc) {
                self.intent(at, cancelled)?;
                self.in_recovery_section = true;
            }
        } else {
            self.in_recovery_section = false;
        }
        // Outside the plans, a primitive that is not safe to repeat is written ahead as well, so
        // a crash before its response shows that it may have been sent (ADR-335 item 2).
        if needs_request_marker(program, pc) {
            self.request(at, cancelled)?;
        }
        Ok(())
    }

    /// Commits a request marker at `at`, before the request there is sent (ADR-335 item 2).
    fn request(&mut self, at: StepRef, cancelled: &AtomicBool) -> Result<(), JobError> {
        // As for an intent: a cancelled job sends nothing more, so it leaves no marker either.
        if cancelled.load(Ordering::Relaxed) {
            return Err(JobError::Cancelled);
        }
        self.journal.commit_request(at)?;
        Ok(())
    }

    /// The journal as folded so far.
    pub(crate) fn state(&self) -> &JournalState {
        self.journal.state()
    }

    /// Whether the journal's newest request marker is the one at `at`, and not withdrawn: the
    /// run is at a request it committed.
    pub(crate) fn holds_request(&self, at: StepRef) -> bool {
        let facts = &self.journal.state().facts;
        facts.last_request == Some(at) && facts.last_request_unsent != Some(at)
    }

    /// Withdraws the request marker at `at`, because the run ends with its request provably
    /// unsent (ADR-392). The caller knows that no attempt may have reached the ECU.
    pub(crate) fn withdraw_request(&mut self, at: StepRef) -> Result<(), JobError> {
        self.journal.commit_request_not_sent(at)?;
        Ok(())
    }

    /// Commits a request intent at `at`, before the request there is sent (ADR-253).
    fn intent(&mut self, at: StepRef, cancelled: &AtomicBool) -> Result<(), JobError> {
        // A cancelled job sends nothing more, so it leaves no intent either.
        if cancelled.load(Ordering::Relaxed) {
            return Err(JobError::Cancelled);
        }
        self.journal.commit_intent(at)?;
        Ok(())
    }

    /// Commits what the step `at` calls for once it completed; `after` is the VM state after it.
    pub(crate) fn completed<H: TransferProgress>(
        &mut self,
        program: &Program,
        at: StepRef,
        after: &VmState,
        host: &H,
    ) -> Result<(), JobError> {
        let op = program.code.get(at.pc as usize);
        if let Some(Op::FlashTransfer { .. }) = op {
            let block = block_number(host.transfer_block_index())?;
            self.journal.commit_block(block)?;
        }
        // A state is recorded where a restart resumes: a plan's entry (a redo of the transfer),
        // reached by any step, and its end (the continuation, ADR-271 item 2), reached by a step
        // from inside the plan only (ADR-274): a jump from outside, such as a polling loop after
        // the plan, journals nothing. One record serves a pc that is both.
        let enters = program.flash.iter().any(|plan| {
            let b = &plan.boundaries;
            b.entry_pc == after.pc
                || (b.post_transfer_end_pc == after.pc
                    && (b.entry_pc..b.post_transfer_end_pc).contains(&at.pc))
        });
        let inside = program.flash.iter().any(|plan| {
            (plan.boundaries.entry_pc..plan.boundaries.post_transfer_end_pc).contains(&at.pc)
        });
        let primitive = op.is_some_and(Op::is_diagnostic_primitive);
        if enters {
            let encoded = postcard::to_allocvec(after)
                .map_err(|_| JournalError::Invariant("the VM state does not encode"))?;
            self.journal.commit_step(at, Some(&encoded))?;
        } else if inside {
            if primitive {
                self.journal.commit_step(at, None)?;
            }
        } else if primitive {
            // A plain procedure's resume origin (ADR-335 item 2); the flash-plan facts do not
            // read it.
            let encoded = postcard::to_allocvec(after)
                .map_err(|_| JournalError::Invariant("the VM state does not encode"))?;
            // A state too large for a frame is skipped, not the job's end: `commit_checkpoint`
            // refuses it before anything is appended, so the journal is unchanged and the job
            // goes on. A missing checkpoint only makes a restart more conservative (ADR-335):
            // the latest checkpoint stays an older one, from which a resume re-runs only
            // primitives that are safe to repeat, and a completed request that is not safe to
            // repeat, with no state after it, counts as unconfirmed.
            match self.journal.commit_checkpoint(at, &encoded) {
                Err(JournalError::TooLarge) => {
                    tracing::warn!(
                        pc = at.pc,
                        "the VM state is too large for a checkpoint; none is recorded"
                    );
                }
                result => result?,
            }
        }
        for plan in &program.flash {
            if after.pc == plan.boundaries.post_transfer_end_pc
                && self.post_transfer_open(plan.stage)
            {
                self.journal.commit_post_transfer_complete()?;
            }
        }
        Ok(())
    }

    /// Whether the job's transfer of `stage` has its RequestTransferExit marker and its
    /// post-transfer steps are not complete yet.
    fn post_transfer_open(&self, stage: u32) -> bool {
        self.journal
            .state()
            .facts
            .transfer
            .as_ref()
            .is_some_and(|transfer| {
                transfer.stage == StageId(stage)
                    && !transfer.interrupted
                    && transfer.exit.as_ref().is_some_and(|exit| !exit.complete)
            })
    }

    /// Reads and commits the hardware part number and the software version from the sources
    /// the program declares. A declared source that gives no value ends the job: a restart
    /// compares the ECU against these values (ADR-229 item 2), so nothing is erased without
    /// them. The one exception is a software version answered with the plan's declared
    /// "no valid application" response: there is no version to record, and the job goes on.
    fn record_identity<H>(
        &mut self,
        program: &Program,
        plan: &FlashRecovery,
        pc: u32,
        host: &mut H,
        cancelled: &AtomicBool,
    ) -> Result<(), JobError>
    where
        H: DiagHost<Error = HostError>,
    {
        let declared = [
            (
                IdentityKind::HardwarePartNumber,
                program.identity.hardware_part_number,
            ),
            (
                IdentityKind::SoftwareVersion,
                program.identity.software_version,
            ),
        ];
        for (identity, source) in declared {
            let Some(source) = source else { continue };
            if cancelled.load(Ordering::Relaxed) {
                return Err(JobError::Cancelled);
            }
            let read =
                read_identity_bytes(identity, source, &self.sources, host).map_err(|source| {
                    JobError::IdentityRead {
                        pc,
                        identity,
                        source,
                    }
                })?;
            match (identity, read) {
                (IdentityKind::HardwarePartNumber, FieldBytes::Field(value)) => {
                    self.journal.commit_ecu_hardware_part_number(&value)?;
                }
                (IdentityKind::SoftwareVersion, FieldBytes::Field(value)) => {
                    self.journal.commit_pre_erase_software_version(&value)?;
                }
                (IdentityKind::SoftwareVersion, FieldBytes::Negative(nrc))
                    if plan.no_application == Some(NoApplication::Nrc(nrc)) => {}
                _ => return Err(JobError::IdentityUnreadable { pc, identity }),
            }
        }
        Ok(())
    }
}

/// Whether the instruction at `pc` is a diagnostic primitive.
fn is_primitive(program: &Program, pc: u32) -> bool {
    program
        .code
        .get(pc as usize)
        .is_some_and(Op::is_diagnostic_primitive)
}

/// The journal's block number for the host's running index of a confirmed block (ADR-250): a
/// 1-based count. Zero or no index after a confirmed block, and a count beyond `u32`, are errors,
/// never a guessed number.
fn block_number(index: Option<u64>) -> Result<u32, JournalError> {
    match index {
        Some(0) | None => Err(JournalError::Invariant(
            "the host has no index for a confirmed block",
        )),
        Some(index) => u32::try_from(index)
            .map_err(|_| JournalError::Invariant("the block count is beyond the journal's range")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_number_is_the_running_index() {
        assert_eq!(block_number(Some(1)).unwrap(), 1);
        assert_eq!(block_number(Some(256)).unwrap(), 256);
        assert_eq!(block_number(Some(u64::from(u32::MAX))).unwrap(), u32::MAX);
        for index in [None, Some(0), Some(u64::from(u32::MAX) + 1)] {
            assert!(
                matches!(block_number(index), Err(JournalError::Invariant(_))),
                "{index:?}"
            );
        }
    }
}
