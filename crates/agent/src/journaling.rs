//! The job runner's write-job journal (design 5.5, 8.2.5; ADR-229, ADR-244, ADR-252).
//!
//! For a procedure with a flash recovery plan, the runner commits to the journal in two places.
//!
//! When execution arrives at an instruction, before it runs and once the VM's own checks of it
//! passed (`Vm::current_op`, ADR-233 item 3) ([`JobJournal::arrive`]):
//! - at a plan's `entry_pc`, once per job and before its first transfer: the ECU hardware part
//!   number and software version, read through the sources the program declares;
//! - at `erase_pc`: the transfer-start marker;
//! - at `transfer_exit_pc`: the RequestTransferExit marker.
//!
//! When an instruction completes ([`JobJournal::completed`]):
//! - a `FlashTransfer`: the block, by the host's running index;
//! - a diagnostic primitive inside a plan's range: a step record, so the journal's last step
//!   orders the interruption point (ADR-229 item 1, ADR-245 item 4);
//! - a step that brings execution to a plan's `entry_pc`: a step record with the VM state
//!   after it (none when the job starts at the entry: the state is then the program's initial
//!   one);
//! - a step that brings execution to a plan's `post_transfer_end_pc` after its exit marker: the
//!   post-transfer completion.
//!
//! A commit that fails, or an identity that cannot be read, ends the job before the next
//! instruction runs, so the request a marker guards is never sent without it (ADR-244 item 7).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use diag_ir::{
    DiagHost, FlashRecovery, IdentityKind, NoApplication, Op, Program, RecoveryRequired, VmState,
};

use crate::host::{HostError, TransferProgress};
use crate::inputs::{FieldBytes, ServiceSources, read_field_bytes};
use crate::journal::{FileStore, JobKey, Journal, JournalError, StageId, StepRef, Store};
use crate::runner::JobError;

/// Where a job keeps its journal, under which key, and how it reads the ECU's identity.
#[derive(Debug, Clone)]
pub struct JournalSetup {
    /// The directory of the journal file (ADR-244 item 7).
    pub dir: PathBuf,
    pub key: JobKey,
    /// The table that maps the program's identity sources to requests (`inputs`).
    pub sources: ServiceSources,
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
}

impl JobJournal {
    /// Creates the job's journal. An existing journal of the same key is an error: resuming
    /// one is the restart's work, not a first run's.
    pub(crate) fn create(setup: JournalSetup) -> Result<Self, JournalError> {
        Ok(Self::with(
            Journal::create(&setup.dir, &setup.key)?,
            setup.sources,
        ))
    }
}

impl<S: Store> JobJournal<S> {
    fn with(journal: Journal<S>, sources: ServiceSources) -> Self {
        Self {
            journal,
            sources,
            identity_read: false,
            past_recovery_point: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(journal: Journal<S>, sources: ServiceSources) -> Self {
        Self::with(journal, sources)
    }

    #[cfg(test)]
    pub(crate) fn journal(&self) -> &Journal<S> {
        &self.journal
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
                if !marked
                    && program
                        .code
                        .get(pc as usize)
                        .is_some_and(Op::is_diagnostic_primitive)
                {
                    // A cancelled job sends nothing more, so it leaves no intent either.
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(JobError::Cancelled);
                    }
                    self.journal.commit_intent(at)?;
                }
                if marked || self.journal.state().facts.last_intent == Some(at) {
                    self.past_recovery_point.push(plan.stage);
                }
            }
        }
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
        let enters = program
            .flash
            .iter()
            .any(|plan| plan.boundaries.entry_pc == after.pc);
        let inside = program.flash.iter().any(|plan| {
            (plan.boundaries.entry_pc..plan.boundaries.post_transfer_end_pc).contains(&at.pc)
        });
        if enters {
            let encoded = postcard::to_allocvec(after)
                .map_err(|_| JournalError::Invariant("the VM state does not encode"))?;
            self.journal.commit_step(at, Some(&encoded))?;
        } else if inside && op.is_some_and(Op::is_diagnostic_primitive) {
            self.journal.commit_step(at, None)?;
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
            let read = read_field_bytes(source, &self.sources, host).map_err(|source| {
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
