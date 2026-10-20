//! Restart declaration of the procedure part (8.10.1, ADR-229, ADR-245).
//!
//! The restart order of ADR-229 needs facts only the procedure's author knows: where the ECU
//! identity is read from, how each flash precondition is read back, and which instructions
//! bound the stages of a flash session. They travel in [`Program`] and are checked by
//! [`Program::validate`] before the program reaches the bus. Nothing here executes anything;
//! the job runner (and the restart logic built on it) reads the declaration.

use serde::{Deserialize, Serialize};

use crate::{Idempotency, Interruptible, MAX_FAILURE_CODE_LEN, Op, Program};

// ---------------------------------------------------------------- Sources

/// Where a value is read from when the agent checks the vehicle before a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Source {
    /// A field of a diagnostic service response. Both ids start at 1 (`Service` and `Field` in
    /// the declaration part, `ir.fbs`).
    EcuService { service_id: u32, field_id: u32 },
    /// A value the agent measures or derives itself.
    RuntimeInput(RuntimeInput),
}

/// A vehicle fact the agent supplies without asking the ECU. The agent's write-job journal stores
/// one in the run-stop record (`VmError::InputNotEstablished`, ADR-338), encoded by variant
/// index, so the variant order is part of that journal's format: append new variants, never
/// reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RuntimeInput {
    SupplyVoltageMillivolts,
    ExternalSupplyConnected,
    IgnitionOn,
    EngineRunning,
    VehicleSpeedKmh,
}

impl RuntimeInput {
    /// Whether this input carries the fact `kind` checks.
    pub fn reports(self, kind: PreconditionKind) -> bool {
        matches!(
            (self, kind),
            (
                RuntimeInput::SupplyVoltageMillivolts,
                PreconditionKind::Voltage
            ) | (
                RuntimeInput::ExternalSupplyConnected,
                PreconditionKind::ExternalSupply
            ) | (RuntimeInput::IgnitionOn, PreconditionKind::Ignition)
                | (RuntimeInput::EngineRunning, PreconditionKind::Engine)
                | (
                    RuntimeInput::VehicleSpeedKmh,
                    PreconditionKind::VehicleSpeed
                )
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum PreconditionKind {
    Voltage,
    ExternalSupply,
    Ignition,
    Engine,
    VehicleSpeed,
}

/// An identity of the ECU a restart re-verifies. The agent's write-job journal stores it in its
/// identity-abort record, encoded by variant index, so the variant order is part of that journal's
/// format too (ADR-286): append new variants, never reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum IdentityKind {
    Vin,
    HardwarePartNumber,
    SoftwareVersion,
}

/// The diagnostic session a precondition source is valid in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum SessionKind {
    Default,
    Programming,
}

/// Where the identity of the ECU is read from. These must be service sources: a runtime input
/// says nothing about the ECU.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentitySources {
    pub vin: Option<Source>,
    pub hardware_part_number: Option<Source>,
    pub software_version: Option<Source>,
}

/// A flash precondition (8.9) as the restart reads it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Precondition {
    /// The internal values that count as satisfied, inclusive on both ends. A single value has
    /// `lower == upper`.
    pub satisfied: Satisfied,
    /// Where the value is read in the default session.
    pub default_session: Option<Source>,
    /// Where the value is read in the programming session: the source the restart's second check
    /// reads immediately before the erase, after the plan's steps from its entry have run
    /// (ADR-229 item 2 step 4). It must be usable in the programming session. The agent tracks
    /// no session: a plan whose `[entry_pc, erase_pc)` re-enters no programming session leaves
    /// the ECU in its default session at that point, and a source that answers only in the
    /// programming session then fails the check, with nothing erased.
    pub programming_session: Option<Source>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Satisfied {
    pub lower: i64,
    pub upper: i64,
}

/// Preconditions a program declares. A precondition that is `None` is not checked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preconditions {
    pub voltage_mv: Option<Precondition>,
    pub external_supply: Option<Precondition>,
    pub ignition: Option<Precondition>,
    pub engine: Option<Precondition>,
    pub vehicle_speed: Option<Precondition>,
}

impl Preconditions {
    fn declared(&self) -> [(PreconditionKind, Option<&Precondition>); 5] {
        [
            (PreconditionKind::Voltage, self.voltage_mv.as_ref()),
            (
                PreconditionKind::ExternalSupply,
                self.external_supply.as_ref(),
            ),
            (PreconditionKind::Ignition, self.ignition.as_ref()),
            (PreconditionKind::Engine, self.engine.as_ref()),
            (PreconditionKind::VehicleSpeed, self.vehicle_speed.as_ref()),
        ]
    }
}

// ---------------------------------------------------------------- Flash recovery plan

/// The recovery plan of one flash session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlashRecovery {
    /// `FlashSession.id` in the declaration part (`ir.fbs`).
    pub flash_session: u32,
    /// The stage of the agent's journal this plan drives (ADR-229).
    pub stage: u32,
    /// How often the stage may be resumed.
    pub max_resumes: u16,
    pub recovery_required: RecoveryRequired,
    pub boundaries: RecoveryBoundaries,
    pub timing: RecoveryTiming,
    /// How often the software version is read again when the read fails.
    pub version_read_retries: u8,
    /// What an ECU without a valid application answers; `None` when it is not declared.
    pub no_application: Option<NoApplication>,
}

impl FlashRecovery {
    /// Whether the agent may restart this stage on its own at all: either an interruption never
    /// needs on-site intervention, or it does only from an instruction after the erase.
    ///
    /// At runtime on-site intervention applies when the interruption is at or after the
    /// `FromPc` position, or inside a section marked [`Interruptible::RecoveryRequired`];
    /// the stricter of the two wins.
    pub fn allows_restart(&self) -> bool {
        match self.recovery_required {
            RecoveryRequired::Never => true,
            RecoveryRequired::FromPc(pc) => pc > self.boundaries.erase_pc,
        }
    }
}

/// From where an interruption needs on-site intervention (8.10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RecoveryRequired {
    /// No interruption of the session needs it.
    Never,
    /// An interruption at or after this instruction does. Must lie in
    /// `entry_pc..post_transfer_end_pc`.
    FromPc(u32),
}

/// Instruction positions that divide a flash session into the stages the restart distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBoundaries {
    /// Where the replayable steps before the erase begin; a restart replays from here.
    pub entry_pc: u32,
    /// The first erase request, or the download request when the procedure does not erase. The
    /// transfer-start marker is committed before it.
    pub erase_pc: u32,
    /// The request that ends the transfer. Its marker is committed before it.
    pub transfer_exit_pc: u32,
    /// Exclusive. The post-transfer completion is journaled when the pc reaches it.
    pub post_transfer_end_pc: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Durations in milliseconds, as `u32`. Part 3 of the restart work adds further timings, in
/// `u64`, next to these.
pub struct RecoveryTiming {
    /// How long the ECU keeps the session alive without traffic.
    pub session_timeout_millis: u32,
    /// Time added after the session timeout before a passive teardown is confirmed, so the
    /// ECU's session has certainly expired (ADR-229). Never subtracted from the timeout.
    pub teardown_margin_millis: u32,
    /// How long the ECU takes to answer again after a reset.
    pub ecu_startup_millis: u32,
    /// How long the restart keeps retrying to confirm that the ECU is back in its default
    /// session (data identifier F186), after the startup time or a passive teardown (ADR-229).
    pub confirmation_window_millis: u32,
}

/// How an ECU without a valid application answers the identity read. An enum so a
/// positive-response form can be added later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum NoApplication {
    /// A negative response with this code.
    Nrc(u8),
}

// ---------------------------------------------------------------- Validation

/// What a source belongs to, for [`ProgramError::ZeroId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceOwner {
    Identity(IdentityKind),
    Precondition {
        kind: PreconditionKind,
        session: SessionKind,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProgramError {
    #[error(
        "instruction {pc}: the code of a declared failure must be a constant of 1 to {MAX_FAILURE_CODE_LEN} bytes of UTF-8"
    )]
    BadFailureCode { pc: u32 },
    #[error(
        "flash session {flash_session}: boundaries must satisfy entry <= erase < transfer exit < post-transfer end <= code length"
    )]
    BoundaryOutOfOrder { flash_session: u32 },
    #[error(
        "flash session {flash_session}: instruction {pc} is not the request this boundary needs (erase: a routine control or RequestDownload; transfer exit: RequestTransferExit)"
    )]
    WrongBoundaryInstruction { flash_session: u32, pc: u32 },
    #[error("flash session {0} has more than one recovery plan")]
    DuplicateFlashSession(u32),
    #[error("stage {0} is used by more than one recovery plan")]
    DuplicateStage(u32),
    #[error("recovery plans of flash sessions {a} and {b} overlap")]
    OverlappingFlashRecoveries { a: u32, b: u32 },
    #[error("flash session {flash_session}: recovery position {pc} is outside the plan")]
    RecoveryRequiredOutOfRange { flash_session: u32, pc: u32 },
    #[error(
        "flash session {flash_session}: section {section} requires recovery inside the range the plan allows to restart"
    )]
    ContradictoryInterruptibility { flash_session: u32, section: usize },
    #[error("flash session {flash_session}: the jump at {pc} leaves the recovery range backwards")]
    JumpOutOfRecovery { flash_session: u32, pc: u32 },
    #[error(
        "flash session {flash_session}: the jump or call at {pc} crosses the recovery point backwards"
    )]
    BackwardJumpAcrossRecovery { flash_session: u32, pc: u32 },
    #[error("flash session {flash_session}: the call or return at {pc} is inside the plan")]
    CallOrReturnInPlan { flash_session: u32, pc: u32 },
    #[error(
        "flash session {flash_session}: the jump or call at {pc} enters the plan past its entry"
    )]
    ControlFlowIntoPlan { flash_session: u32, pc: u32 },
    #[error("the download instruction at {pc} is outside every plan's transfer range")]
    DownloadOutsidePlan { pc: u32 },
    #[error("the flash block read at {pc} is outside every plan's code range")]
    FlashBlockOutsidePlan { pc: u32 },
    #[error(
        "the flash block read at {pc} names flash session {session} but sits in the plan of flash session {flash_session}"
    )]
    FlashBlockSessionMismatch {
        pc: u32,
        session: u32,
        flash_session: u32,
    },
    #[error(
        "flash session {flash_session}: section {section} is unsafe to replay but the restart replays it"
    )]
    UnsafeSectionInReplay { flash_session: u32, section: usize },
    #[error(
        "flash session {flash_session}: {nrc:#04x} is not a usable no-application response code"
    )]
    InvalidNoApplicationNrc { flash_session: u32, nrc: u8 },
    #[error("{0:?} precondition is a flag; its range must lie within 0..=1")]
    FlagRangeOutOfDomain(PreconditionKind),
    #[error("{0:?} precondition has an empty range")]
    EmptyRange(PreconditionKind),
    #[error("{kind:?} precondition cannot be read from {input:?}")]
    InputDoesNotReport {
        kind: PreconditionKind,
        input: RuntimeInput,
    },
    #[error("{0:?} must be read from the ECU, not from a runtime input")]
    IdentityFromRuntimeInput(IdentityKind),
    #[error("service and field ids start at 1 ({owner:?})")]
    ZeroId { owner: SourceOwner },
    #[error("{0:?} source is missing")]
    MissingIdentitySource(IdentityKind),
    #[error("{kind:?} precondition has no source for the {session:?} session")]
    UnmappedPrecondition {
        kind: PreconditionKind,
        session: SessionKind,
    },
    #[error("flash session {flash_session}: the session timeout is zero")]
    MissingSessionTimeout { flash_session: u32 },
    #[error("flash session {flash_session}: the resume limit is zero")]
    ZeroResumeLimit { flash_session: u32 },
    #[error("section {section} is empty, reversed or reaches past the code")]
    InvalidSection { section: usize },
    #[error(
        "flash session {flash_session}: instruction {pc} is a second RequestDownload or a RequestTransferExit the plan does not declare"
    )]
    UndeclaredTransferBoundary { flash_session: u32, pc: u32 },
    #[error(
        "flash session {flash_session}: the plan has no RequestDownload before its transfer exit"
    )]
    MissingRequestDownload { flash_session: u32 },
    #[error("flash session {flash_session}: the transfer at {pc} comes before the RequestDownload")]
    TransferBeforeRequestDownload { flash_session: u32, pc: u32 },
    #[error(
        "flash session {flash_session}: service {service:#04x} at {pc} ends the transfer between the RequestDownload and the RequestTransferExit"
    )]
    TransferInterrupted {
        flash_session: u32,
        pc: u32,
        service: u16,
    },
    #[error(
        "flash session {flash_session}: the subroutine called at {pc} can reach the plan; a plan runs only at the top level"
    )]
    PlanInSubroutine { flash_session: u32, pc: u32 },
    #[error(
        "the program declares {count} recovery plans; at most {MAX_FLASH_RECOVERIES} are allowed"
    )]
    TooManyFlashRecoveries { count: usize },
    #[error(
        "flash session {flash_session}: stage {stage} is reserved for the plain procedure's resume count"
    )]
    ReservedStage { flash_session: u32, stage: u32 },
}

impl ProgramError {
    /// The instruction the error names, when it names one: the position a front end can map back
    /// to its source. It may lie outside the code (`RecoveryRequiredOutOfRange`).
    pub fn pc(&self) -> Option<u32> {
        match self {
            Self::BadFailureCode { pc }
            | Self::WrongBoundaryInstruction { pc, .. }
            | Self::RecoveryRequiredOutOfRange { pc, .. }
            | Self::JumpOutOfRecovery { pc, .. }
            | Self::BackwardJumpAcrossRecovery { pc, .. }
            | Self::CallOrReturnInPlan { pc, .. }
            | Self::ControlFlowIntoPlan { pc, .. }
            | Self::DownloadOutsidePlan { pc }
            | Self::FlashBlockOutsidePlan { pc }
            | Self::FlashBlockSessionMismatch { pc, .. }
            | Self::UndeclaredTransferBoundary { pc, .. }
            | Self::TransferBeforeRequestDownload { pc, .. }
            | Self::TransferInterrupted { pc, .. }
            | Self::PlanInSubroutine { pc, .. } => Some(*pc),
            Self::TooManyFlashRecoveries { .. }
            | Self::BoundaryOutOfOrder { .. }
            | Self::DuplicateFlashSession(_)
            | Self::DuplicateStage(_)
            | Self::OverlappingFlashRecoveries { .. }
            | Self::ContradictoryInterruptibility { .. }
            | Self::UnsafeSectionInReplay { .. }
            | Self::InvalidNoApplicationNrc { .. }
            | Self::FlagRangeOutOfDomain(_)
            | Self::EmptyRange(_)
            | Self::InputDoesNotReport { .. }
            | Self::IdentityFromRuntimeInput(_)
            | Self::ZeroId { .. }
            | Self::MissingIdentitySource(_)
            | Self::UnmappedPrecondition { .. }
            | Self::MissingSessionTimeout { .. }
            | Self::ZeroResumeLimit { .. }
            | Self::InvalidSection { .. }
            | Self::MissingRequestDownload { .. }
            | Self::ReservedStage { .. } => None,
        }
    }
}

/// The stage id the agent's journal keeps the resume count of a plain procedure under, the step
/// ranges outside every plan (ADR-335 item 6). No recovery plan may use it
/// ([`ProgramError::ReservedStage`]).
pub const PLAIN_PROCEDURE_STAGE: u32 = u32::MAX;

/// The most recovery plans a program may declare. Each plan's checks pass over the whole code
/// once, so the cap keeps loading linear in the code's length; a procedure has one plan per
/// flash session, and an ECU has a handful of those at most.
pub const MAX_FLASH_RECOVERIES: usize = 64;

fn is_erase(op: Option<&Op>) -> bool {
    matches!(
        op,
        Some(Op::RoutineControl { .. } | Op::ServiceRequest { service: 0x34 })
    )
}

fn is_transfer_exit(op: Option<&Op>) -> bool {
    matches!(op, Some(Op::ServiceRequest { service: 0x37 }))
}

/// Instructions that belong to a download, which only a plan's transfer range may hold.
fn is_download(op: &Op) -> bool {
    matches!(
        op,
        Op::ServiceRequest {
            service: 0x34 | 0x36 | 0x37
        } | Op::FlashTransfer { .. }
    )
}

fn check_ids(source: Source, owner: SourceOwner) -> Result<(), ProgramError> {
    match source {
        Source::EcuService {
            service_id,
            field_id,
        } if service_id == 0 || field_id == 0 => Err(ProgramError::ZeroId { owner }),
        _ => Ok(()),
    }
}

impl Program {
    /// Checks the restart declaration (ADR-245): the sections, each plan's boundaries and the
    /// instructions at them, where download instructions sit, the control flow into, inside
    /// and out of each plan, and the identity and precondition sources. It does not check the
    /// schema version or the program's size ([`crate::Vm::check_state`]) or each instruction's
    /// operands, which fail when the VM reaches them.
    ///
    /// The restart rules (identity sources, session mapping, timeout, resume limit) apply only
    /// when some plan allows a restart ([`FlashRecovery::allows_restart`]); a program whose
    /// plans never allow one is not asked for what only a restart reads.
    pub fn validate(&self) -> Result<(), ProgramError> {
        if self.flash.len() > MAX_FLASH_RECOVERIES {
            return Err(ProgramError::TooManyFlashRecoveries {
                count: self.flash.len(),
            });
        }
        // The code of a declared failure is a constant the dry run can check (ADR-280).
        for (pc, op) in self.code.iter().enumerate() {
            if let Op::Fail { code } = op
                && !self
                    .constants
                    .get(*code as usize)
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .is_some_and(|text| (1..=MAX_FAILURE_CODE_LEN).contains(&text.len()))
            {
                return Err(ProgramError::BadFailureCode { pc: pc as u32 });
            }
        }
        // The overlap checks below would read a reversed section as empty.
        for (section, s) in self.sections.iter().enumerate() {
            if s.start_pc >= s.end_pc || s.end_pc as usize > self.code.len() {
                return Err(ProgramError::InvalidSection { section });
            }
        }
        for (index, plan) in self.flash.iter().enumerate() {
            if plan.stage == PLAIN_PROCEDURE_STAGE {
                return Err(ProgramError::ReservedStage {
                    flash_session: plan.flash_session,
                    stage: plan.stage,
                });
            }
            self.validate_plan(plan)?;
            for other in &self.flash[..index] {
                if other.flash_session == plan.flash_session {
                    return Err(ProgramError::DuplicateFlashSession(plan.flash_session));
                }
                if other.stage == plan.stage {
                    return Err(ProgramError::DuplicateStage(plan.stage));
                }
                let (a, b) = (&other.boundaries, &plan.boundaries);
                if a.entry_pc < b.post_transfer_end_pc && b.entry_pc < a.post_transfer_end_pc {
                    return Err(ProgramError::OverlappingFlashRecoveries {
                        a: other.flash_session,
                        b: plan.flash_session,
                    });
                }
            }
        }

        for (index, op) in self.code.iter().enumerate() {
            let pc = index as u32;
            let in_transfer = self.flash.iter().any(|plan| {
                (plan.boundaries.erase_pc..=plan.boundaries.transfer_exit_pc).contains(&pc)
            });
            if is_download(op) && !in_transfer {
                return Err(ProgramError::DownloadOutsidePlan { pc });
            }
            // A block read names the session of the plan it sits in (ADR-336): the plan's range
            // is what the agent runs with that session's artifact and recovery rules.
            if let Op::FlashBlock { session, .. } = *op {
                let Some(plan) = self.flash.iter().find(|plan| {
                    (plan.boundaries.entry_pc..plan.boundaries.post_transfer_end_pc).contains(&pc)
                }) else {
                    return Err(ProgramError::FlashBlockOutsidePlan { pc });
                };
                if plan.flash_session != session {
                    return Err(ProgramError::FlashBlockSessionMismatch {
                        pc,
                        session,
                        flash_session: plan.flash_session,
                    });
                }
            }
        }

        let identity = [
            (IdentityKind::Vin, self.identity.vin),
            (
                IdentityKind::HardwarePartNumber,
                self.identity.hardware_part_number,
            ),
            (
                IdentityKind::SoftwareVersion,
                self.identity.software_version,
            ),
        ];
        for (kind, source) in identity {
            match source {
                Some(Source::RuntimeInput(_)) => {
                    return Err(ProgramError::IdentityFromRuntimeInput(kind));
                }
                Some(source) => check_ids(source, SourceOwner::Identity(kind))?,
                None => {}
            }
        }
        for (kind, precondition) in self.preconditions.declared() {
            let Some(precondition) = precondition else {
                continue;
            };
            if precondition.satisfied.lower > precondition.satisfied.upper {
                return Err(ProgramError::EmptyRange(kind));
            }
            let is_flag = matches!(
                kind,
                PreconditionKind::ExternalSupply
                    | PreconditionKind::Ignition
                    | PreconditionKind::Engine
            );
            if is_flag && (precondition.satisfied.lower < 0 || precondition.satisfied.upper > 1) {
                return Err(ProgramError::FlagRangeOutOfDomain(kind));
            }
            let sessions = [
                (SessionKind::Default, precondition.default_session),
                (SessionKind::Programming, precondition.programming_session),
            ];
            // Every declared precondition is checked before the procedure starts, in the
            // default session; the programming-session source is read only by a restart.
            if precondition.default_session.is_none() {
                return Err(ProgramError::UnmappedPrecondition {
                    kind,
                    session: SessionKind::Default,
                });
            }
            for (session, source) in sessions {
                match source {
                    Some(Source::RuntimeInput(input)) if !input.reports(kind) => {
                        return Err(ProgramError::InputDoesNotReport { kind, input });
                    }
                    Some(source) => check_ids(source, SourceOwner::Precondition { kind, session })?,
                    None => {}
                }
            }
        }

        if self.flash.iter().any(FlashRecovery::allows_restart) {
            for (kind, source) in identity {
                if source.is_none() {
                    return Err(ProgramError::MissingIdentitySource(kind));
                }
            }
            for (kind, precondition) in self.preconditions.declared() {
                let Some(precondition) = precondition else {
                    continue;
                };
                for (session, source) in [
                    (SessionKind::Default, precondition.default_session),
                    (SessionKind::Programming, precondition.programming_session),
                ] {
                    if source.is_none() {
                        return Err(ProgramError::UnmappedPrecondition { kind, session });
                    }
                }
            }
            for plan in self.flash.iter().filter(|plan| plan.allows_restart()) {
                if plan.timing.session_timeout_millis == 0 {
                    return Err(ProgramError::MissingSessionTimeout {
                        flash_session: plan.flash_session,
                    });
                }
                if plan.max_resumes == 0 {
                    return Err(ProgramError::ZeroResumeLimit {
                        flash_session: plan.flash_session,
                    });
                }
            }
        }
        Ok(())
    }

    /// For each instruction, whether execution that stands at `pc` with the suspended callers'
    /// return positions `return_pcs` (outermost first, innermost last, as
    /// `VmState::call_stack[i].return_pc`) can reach it, `pc` itself included (ADR-335 item 3,
    /// condition S). It is the forward closure over the VM's control flow:
    /// - an instruction falls through to the next one; a jump goes to its target; a conditional
    ///   jump goes to its target and to the next instruction;
    /// - a call explores the callee as a nested call, whose return ends that path because the
    ///   caller goes on, and also goes on at the next instruction at the current level;
    /// - a return at the level `pc` stands in goes on at the innermost suspended caller's return
    ///   position, then outwards through `return_pcs`; with none left it ends the program;
    /// - a declared failure ends the path, and so does a position at or past the end of the code
    ///   (the program ends there), so an out-of-range target is ignored.
    ///
    /// Each instruction is visited once per level (the checkpoint's own frame and each caller
    /// above it, plus the nested level), so the cost is linear in the code's length times
    /// `return_pcs.len() + 2`. The result is conservative where the VM's data would decide
    /// (both ways of a conditional jump are taken), and reaches nothing when `pc` is out of range.
    /// `return_pcs` should come from a state that passed `Vm::check_state`, which bounds the call
    /// depth; an absurd length that would overflow the table's size reaches every instruction.
    pub fn reachable_from(&self, pc: u32, return_pcs: &[u32]) -> Vec<bool> {
        let len = self.code.len();
        // Levels 0..=callers are the frames (a level's number is how many callers are still
        // suspended above it); `nested` is any call made on the way, whose callers are not
        // tracked since its return ends the path.
        let callers = return_pcs.len();
        let nested = callers + 1;
        let levels = callers + 2;
        // Callers pass a state `Vm::check_state` accepted, whose call depth is bounded; a size
        // that would wrap anyway is answered conservatively, every instruction reached, instead
        // of allocating or panicking.
        let Some(states) = len.checked_mul(levels) else {
            return vec![true; len];
        };
        let mut visited = vec![false; states];
        let mut reached = vec![false; len];
        let mut pending = vec![(pc, callers)];
        while let Some((pc, level)) = pending.pop() {
            let index = pc as usize;
            if index >= len || std::mem::replace(&mut visited[index * levels + level], true) {
                continue;
            }
            reached[index] = true;
            match self.code[index] {
                Op::Fail { .. } => {}
                Op::Ret => {
                    if level != nested && level > 0 {
                        pending.push((return_pcs[level - 1], level - 1));
                    }
                }
                Op::Jump(to) => pending.push((to, level)),
                Op::JumpIfFalse(to) => {
                    pending.push((to, level));
                    pending.push((pc + 1, level));
                }
                Op::Call(to) => {
                    pending.push((to, nested));
                    pending.push((pc + 1, level));
                }
                _ => pending.push((pc + 1, level)),
            }
        }
        reached
    }

    /// The checks of one plan that need only the plan and the code.
    fn validate_plan(&self, plan: &FlashRecovery) -> Result<(), ProgramError> {
        let flash_session = plan.flash_session;
        let b = &plan.boundaries;
        if !(b.entry_pc <= b.erase_pc
            && b.erase_pc < b.transfer_exit_pc
            && b.transfer_exit_pc < b.post_transfer_end_pc
            && b.post_transfer_end_pc as usize <= self.code.len())
        {
            return Err(ProgramError::BoundaryOutOfOrder { flash_session });
        }
        if !is_erase(self.code.get(b.erase_pc as usize)) {
            return Err(ProgramError::WrongBoundaryInstruction {
                flash_session,
                pc: b.erase_pc,
            });
        }
        if !is_transfer_exit(self.code.get(b.transfer_exit_pc as usize)) {
            return Err(ProgramError::WrongBoundaryInstruction {
                flash_session,
                pc: b.transfer_exit_pc,
            });
        }
        // One RequestDownload and one RequestTransferExit per plan: the journal's markers
        // guard only the declared boundaries.
        let mut downloads = 0;
        for pc in b.erase_pc..=b.transfer_exit_pc {
            let undeclared = match self.code[pc as usize] {
                Op::ServiceRequest { service: 0x34 } => {
                    downloads += 1;
                    downloads > 1
                }
                Op::ServiceRequest { service: 0x37 } => pc != b.transfer_exit_pc,
                Op::ServiceRequest { service: 0x36 } | Op::FlashTransfer { .. }
                    if downloads == 0 =>
                {
                    return Err(ProgramError::TransferBeforeRequestDownload { flash_session, pc });
                }
                // A session change, a reset, an upload or a file transfer ends the transfer the
                // agent's host tracks (ADR-250), so a later block would fail after the erase.
                Op::ServiceRequest {
                    service: service @ (0x10 | 0x11 | 0x35 | 0x38),
                } if downloads > 0 => {
                    return Err(ProgramError::TransferInterrupted {
                        flash_session,
                        pc,
                        service,
                    });
                }
                _ => false,
            };
            if undeclared {
                return Err(ProgramError::UndeclaredTransferBoundary { flash_session, pc });
            }
        }
        // An erase that is not a RequestDownload must be followed by one.
        if downloads == 0 {
            return Err(ProgramError::MissingRequestDownload { flash_session });
        }
        if let Some(NoApplication::Nrc(nrc @ (0x00 | 0x78))) = plan.no_application {
            return Err(ProgramError::InvalidNoApplicationNrc { flash_session, nrc });
        }
        let from = match plan.recovery_required {
            RecoveryRequired::Never => b.post_transfer_end_pc,
            RecoveryRequired::FromPc(pc) => {
                if pc < b.entry_pc || pc >= b.post_transfer_end_pc {
                    return Err(ProgramError::RecoveryRequiredOutOfRange { flash_session, pc });
                }
                pc
            }
        };
        for (section, s) in self.sections.iter().enumerate() {
            if matches!(s.interruptible, Interruptible::RecoveryRequired)
                && s.start_pc.max(b.entry_pc) < s.end_pc.min(from)
            {
                return Err(ProgramError::ContradictoryInterruptibility {
                    flash_session,
                    section,
                });
            }
        }
        // A restart replays everything from the entry up to the recovery point: the pre-erase
        // steps, the transfer and the post-transfer steps it redoes.
        if plan.allows_restart() {
            for (section, s) in self.sections.iter().enumerate() {
                if matches!(s.idempotency, Idempotency::Unsafe)
                    && s.start_pc.max(b.entry_pc) < s.end_pc.min(from)
                {
                    return Err(ProgramError::UnsafeSectionInReplay {
                        flash_session,
                        section,
                    });
                }
            }
        }
        self.validate_control_flow(plan)
    }

    /// The plan's range is a single-entry region whose stages follow in order: no call or
    /// return inside, no way in except at the entry, and jumps inside only where the stage they
    /// sit in allows.
    /// For each pc, whether execution from there can reach a pc for which `target` holds,
    /// following fallthrough and jumps and stopping at a return; a call made on the way returns
    /// to the next instruction, so it continues there. One backward pass over the code, so a
    /// program with many calls is checked in linear time.
    fn reaching(&self, target: &impl Fn(u32) -> bool) -> Vec<bool> {
        let len = self.code.len();
        let mut predecessors = vec![Vec::new(); len];
        for (index, op) in self.code.iter().enumerate() {
            let pc = index as u32;
            let successors: &[u32] = match *op {
                // A return or a declared failure ends the path (ADR-280).
                Op::Ret | Op::Fail { .. } => &[],
                Op::Jump(to) => &[to],
                Op::JumpIfFalse(to) => &[to, pc + 1],
                _ => &[pc + 1],
            };
            for &next in successors {
                if (next as usize) < len {
                    predecessors[next as usize].push(pc);
                }
            }
        }
        let mut reaches = vec![false; len];
        let mut pending: Vec<u32> = (0..len as u32).filter(|&pc| target(pc)).collect();
        for &pc in &pending {
            reaches[pc as usize] = true;
        }
        while let Some(pc) = pending.pop() {
            for &before in &predecessors[pc as usize] {
                if !std::mem::replace(&mut reaches[before as usize], true) {
                    pending.push(before);
                }
            }
        }
        reaches
    }

    fn validate_control_flow(&self, plan: &FlashRecovery) -> Result<(), ProgramError> {
        let flash_session = plan.flash_session;
        let b = &plan.boundaries;
        let inside = |pc: u32| (b.entry_pc..b.post_transfer_end_pc).contains(&pc);
        // The plan's one RequestDownload (`validate_plan` checks there is exactly one).
        let download_pc = (b.erase_pc..=b.transfer_exit_pc)
            .find(|&pc| matches!(self.code[pc as usize], Op::ServiceRequest { service: 0x34 }))
            .unwrap_or(b.erase_pc);
        for (index, op) in self.code.iter().enumerate() {
            let pc = index as u32;
            let from_inside = inside(pc);
            // Neither a jump nor a call may take execution from at or after the recovery
            // point back before it: the pc would no longer order the interruption point.
            if let Op::Jump(target) | Op::JumpIfFalse(target) | Op::Call(target) = *op
                && let RecoveryRequired::FromPc(from) = plan.recovery_required
                && pc >= from
                && target < from
            {
                return Err(ProgramError::BackwardJumpAcrossRecovery { flash_session, pc });
            }
            match *op {
                Op::Call(_) | Op::Ret if from_inside => {
                    return Err(ProgramError::CallOrReturnInPlan { flash_session, pc });
                }
                Op::Call(target) if inside(target) => {
                    return Err(ProgramError::ControlFlowIntoPlan { flash_session, pc });
                }
                Op::Jump(target) | Op::JumpIfFalse(target) => {
                    if !from_inside {
                        if target > b.entry_pc && target < b.post_transfer_end_pc {
                            return Err(ProgramError::ControlFlowIntoPlan { flash_session, pc });
                        }
                    } else {
                        let allowed = if pc < b.erase_pc {
                            target >= b.entry_pc && target <= b.erase_pc
                        } else if pc < download_pc {
                            // Between an erase and the RequestDownload after it, a jump may
                            // not skip the RequestDownload.
                            target >= b.erase_pc && target <= download_pc
                        } else if pc < b.transfer_exit_pc {
                            // Within the transfer, a jump stays after the RequestDownload: going
                            // back to it or before it would send a second RequestDownload, or
                            // a request that ends the transfer, mid-transfer (ADR-250).
                            target > download_pc && target <= b.transfer_exit_pc
                        } else {
                            target == b.erase_pc
                                || (target > b.transfer_exit_pc && target <= b.post_transfer_end_pc)
                        };
                        if !allowed {
                            return Err(ProgramError::JumpOutOfRecovery { flash_session, pc });
                        }
                    }
                }
                _ => {}
            }
        }
        // A plan runs only at the top level: no subroutine may reach it, by falling through or
        // jumping. Its return would otherwise take execution back to the call site, before the
        // recovery point, after the plan had run.
        let reaches_plan = self.reaching(&inside);
        for (index, op) in self.code.iter().enumerate() {
            if let Op::Call(target) = *op
                && reaches_plan.get(target as usize).copied().unwrap_or(false)
            {
                return Err(ProgramError::PlanInSubroutine {
                    flash_session,
                    pc: index as u32,
                });
            }
        }
        Ok(())
    }
}
