//! The job runner's write-job journal (design 5.5, 8.2.5; ADR-229, ADR-244, ADR-252).
//!
//! For a procedure with a flash recovery plan, the runner commits to the journal at the plan's
//! boundaries, each time execution arrives at one and before its instruction runs:
//!
//! - at `entry_pc`: the VM state, on a step record of the step that brought execution there
//!   (none when the job starts there: the state is then the program's initial one); and,
//!   while no transfer has started in this job, the ECU hardware part number and software
//!   version, read through the sources the program declares;
//! - at `erase_pc`: the transfer-start marker;
//! - after each `FlashTransfer` the ECU confirmed: the block, by the host's running index;
//! - at `transfer_exit_pc`: the RequestTransferExit marker;
//! - at `post_transfer_end_pc`: the post-transfer completion.
//!
//! A commit that fails, or an identity that cannot be read, ends the job before the instruction
//! at that boundary runs, so the request a marker guards is never sent without it (ADR-244
//! item 7).

use std::path::PathBuf;

use diag_ir::{DiagHost, IdentityKind, Op, Program, VmState};

use crate::host::{HostError, TransferProgress};
use crate::inputs::{ServiceSources, read_field_bytes};
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
}

impl JobJournal {
    /// Creates the job's journal. An existing journal of the same key is an error: resuming
    /// one is the restart's work, not a first run's.
    pub(crate) fn create(setup: JournalSetup) -> Result<Self, JournalError> {
        Ok(Self {
            journal: Journal::create(&setup.dir, &setup.key)?,
            sources: setup.sources,
        })
    }
}

impl<S: Store> JobJournal<S> {
    #[cfg(test)]
    pub(crate) fn new(journal: Journal<S>, sources: ServiceSources) -> Self {
        Self { journal, sources }
    }

    #[cfg(test)]
    pub(crate) fn journal(&self) -> &Journal<S> {
        &self.journal
    }

    /// Commits what the boundaries at `state.pc` call for. Called once each time execution
    /// arrives at an instruction, before it runs; `previous` is the step that brought it there,
    /// `None` at the start of the job.
    pub(crate) fn arrive<H>(
        &mut self,
        program: &Program,
        state: &VmState,
        previous: Option<StepRef>,
        host: &mut H,
    ) -> Result<(), JobError>
    where
        H: DiagHost<Error = HostError>,
    {
        let pc = state.pc;
        let at = StepRef {
            pc,
            steps: state.steps,
        };
        // A plan's end can be the next plan's entry: complete the one before entering the next.
        for plan in &program.flash {
            if pc == plan.boundaries.post_transfer_end_pc && self.post_transfer_open(plan.stage) {
                self.journal.commit_post_transfer_complete()?;
            }
        }
        for plan in &program.flash {
            let b = &plan.boundaries;
            if pc == b.entry_pc {
                if let Some(previous) = previous {
                    let encoded = postcard::to_allocvec(state)
                        .map_err(|_| JournalError::Invariant("the VM state does not encode"))?;
                    self.journal.commit_step(previous, Some(&encoded))?;
                }
                // The pre-erase version belongs before the job's first transfer (ADR-244 item 4).
                if self.journal.state().facts.transfer.is_none() {
                    self.record_identity(program, pc, host)?;
                }
            }
            if pc == b.erase_pc {
                self.journal
                    .commit_transfer_start(StageId(plan.stage), at)?;
            }
            if pc == b.transfer_exit_pc {
                self.journal.commit_transfer_exit_intent(at)?;
            }
        }
        Ok(())
    }

    /// Commits what the instruction at `pc` calls for once it completed: a `FlashTransfer`
    /// records the block the ECU confirmed.
    pub(crate) fn completed<H: TransferProgress>(
        &mut self,
        program: &Program,
        pc: u32,
        host: &H,
    ) -> Result<(), JobError> {
        if let Some(Op::FlashTransfer { .. }) = program.code.get(pc as usize) {
            let block = block_number(host.transfer_block_index())?;
            self.journal.commit_block(block)?;
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
    /// compares the ECU against these values (ADR-229 item 2), so nothing is erased without them.
    fn record_identity<H>(
        &mut self,
        program: &Program,
        pc: u32,
        host: &mut H,
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
            let value = read_field_bytes(source, &self.sources, host)
                .map_err(|source| JobError::Host { pc, source })?
                .ok_or(JobError::IdentityUnreadable { pc, identity })?;
            match identity {
                IdentityKind::HardwarePartNumber => {
                    self.journal.commit_ecu_hardware_part_number(&value)?;
                }
                _ => self.journal.commit_pre_erase_software_version(&value)?,
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
