//! Restart classification (design 5.6 `Interrupted -> Writing`, 8.2.5, 8.10.1; ADR-229 items 1
//! and 2 step 1, ADR-245 items 3, 5 and 7, ADR-253).
//!
//! [`classify`] reads what a job's write-job journal says about the job's last run, together
//! with the program, and decides how the job may go on: a plain start, the restart order of
//! ADR-229 item 2 for an interrupted transfer, or on-site intervention. It contacts nothing; the
//! restart itself (resume limit, voltage read, teardown) acts on its answer.
//!
//! The interruption point is the latest of the last completed step and the requests the
//! journal wrote ahead (the transfer-start and RequestTransferExit markers, and the request
//! intent of ADR-253), by step count, so a crash after a guarded request was sent but before its
//! response was recorded is placed at that request.

use diag_ir::{Interruptible, Program, RecoveryRequired, Vm, VmError, VmState};

use crate::journal::{JournalError, JournalState, RecoveryFacts, StepRef};

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
}

/// Decides how the job of `program` goes on, from its journal as `Journal::read` (or
/// `Journal::open`) gave it. No journal at all (`JournalError::NotFound`) is a job that never
/// started its journal: a plain start.
pub fn classify(
    program: &Program,
    journal: Result<&JournalState, &JournalError>,
) -> RestartDecision {
    let state = match journal {
        Ok(state) => state,
        Err(JournalError::NotFound) => return RestartDecision::PlainStart,
        Err(error) => {
            return RestartDecision::OnSiteInterventionRequired(OnSiteReason::UnreadableJournal(
                error.to_string(),
            ));
        }
    };
    let facts = &state.facts;
    let interrupted_at = interruption_point(facts);
    if let Some(at) = interrupted_at
        && let Some(reason) = recovery_required(program, facts, at)
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
    let mut entry_state = entry_state;
    entry_state.steps = next_steps(state);
    RestartDecision::Restart(Box::new(RestartPoint {
        flash_session: plan.flash_session,
        interrupted_at,
        entry_state,
        facts: facts.clone(),
    }))
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

/// Why an interruption at `at` rules out a restart, if it does: at or past a plan's
/// recovery-required point and before its end, or inside a section marked `RecoveryRequired`.
///
/// A plan whose post-transfer completion the journal records was left at its end: the journal
/// records no steps after a plan, so its last step stays on the plan's last primitive, and the
/// completion is what says execution got past it. Sections are checked only where the journal
/// can place a point: inside a plan, or on the step into a plan's entry.
fn recovery_required(
    program: &Program,
    facts: &RecoveryFacts,
    at: StepRef,
) -> Option<OnSiteReason> {
    let completed = |stage: u32| {
        facts.transfer.as_ref().is_some_and(|transfer| {
            transfer.stage.0 == stage
                && !transfer.interrupted
                && transfer.exit.as_ref().is_some_and(|exit| exit.complete)
        })
    };
    for plan in &program.flash {
        if let RecoveryRequired::FromPc(from) = plan.recovery_required
            && (from..plan.boundaries.post_transfer_end_pc).contains(&at.pc)
            && !completed(plan.stage)
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
        assert_eq!(
            classify(&program, Err(&JournalError::NotFound)),
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
