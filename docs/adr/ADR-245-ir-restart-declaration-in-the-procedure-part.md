# ADR-245: The IR's Restart Declaration Lives in the Procedure Part

**Date:** 2026-10-07
**Status:** Accepted
**Affects:** `crates/diag-ir` (`src/recovery.rs`, `Program`, `VmState`, `IR_SCHEMA_VERSION`, `schema/ir.fbs` `FlashSession`), `crates/agent` (`check_program`), `docs/system-architecture.md` (8.2.5, 8.9)

## Context

ADR-229 restarts an interrupted transfer in a fixed order, and that order reads eleven inputs
the IR did not carry:
- the ECU's session timeout and the margin added to it before a passive teardown is confirmed;
- the response that means no valid application is present;
- the retry limit for the recovery version read;
- the ECU's startup time after a reset, and the session-confirmation window;
- a resume limit per stage;
- whether an external power supply is required;
- where the VIN, the hardware identity, the software version and the engine, vehicle-speed and
  ignition states are read from, with the values that satisfy them;
- where a flash session's replayable steps, erase, RequestTransferExit and post-transfer steps
  sit in the bytecode;
- a way to declare that no step requires on-site intervention.

The declaration part (`schema/ir.fbs`) had a `FlashSession` with precondition flags and
`recovery_required_from_step`, a step number that defaults to the start of the erase and has no
value for "none". No crate decodes the declaration part yet; its loader is milestone M2 work.
The agent's runner reads only the procedure part (`Program`, postcard), which `ngr-agent run`
takes as JSON. The restart order is an M1 exit criterion, so every input it needs must reach the
runner in M1.

## Decision

1. **Procedure-part data.** The restart declaration is part of `Program`, in three fields:
   - `identity`: where the VIN, the hardware part number and the software version are read from;
   - `preconditions`: voltage, external supply, ignition, engine and vehicle speed, each with
     the values that satisfy it and a source per session;
   - `flash`: one `FlashRecovery` plan per flash session.

   A plan names its `FlashSession` by id. In `ir.fbs`, `FlashSession` keeps only the flash
   description (steps and segments). Its precondition flags and `recovery_required_from_step`
   are removed, so every field has one home and nothing has to be kept in sync between the two
   parts. The boundaries are bytecode positions, and design 8.2.1 places retries in the
   procedure part, so the procedure part is their natural home as well. One `EcuDocument`
   carries one variant and one program, so per-ECU values such as the session timeout are not
   repeated across procedures.
2. **Sources.** A source is either an ECU service and one of its response fields, by the
   declaration's ids, or a named runtime input from a closed set:
   - supply voltage;
   - external supply connected;
   - ignition;
   - engine running;
   - vehicle speed.

   A precondition has one source for the default session and one for the programming
   session; they may be the same. The values that satisfy it form an inclusive range of
   internal values, a single value when both ends are equal.
3. **Recovery-required point.** `RecoveryRequired::Never` or `RecoveryRequired::FromPc(pc)`
   replaces the step number.
   - A plan allows a restart when it is `Never`, or when its point lies after the erase.
   - At run time, on-site intervention applies when the interruption point is at or after
     that point, or inside a section marked `RecoveryRequired` (8.10.1); the stricter rule wins.
   - A plan that contradicts a section is refused when the program is loaded.
4. **Boundaries.** A plan has four positions in the bytecode:
   - `entry_pc`: where the replayable pre-erase steps begin;
   - `erase_pc`: the first erase request, or the RequestDownload when the procedure does not
     erase. The transfer-start marker is committed before it;
   - `transfer_exit_pc`: the RequestTransferExit, which its marker precedes;
   - `post_transfer_end_pc`: reaching it journals the post-transfer steps as complete.

   A jump from inside the erase-to-end range back before `entry_pc` is refused, so a position
   orders the interruption point as the journal's step count does.
5. **Resume limit per stage.** Each plan names the journal stage it counts against (ADR-244)
   and that stage's limit. `VmState` loses `checkpoint` and `resume_count`, which the journal
   owns (ADR-244 item 6, ADR-233 item 3).
6. **Checked at every load.** `Program::validate` checks the declaration, and the agent's
   `check_program` runs it before the program reaches a worker. It checks that:
   - the boundaries are ordered and inside the code;
   - `erase_pc` and `transfer_exit_pc` are diagnostic requests;
   - plans do not overlap or repeat a flash session or a stage;
   - the recovery-required point lies inside the plan;
   - plans and sections do not contradict each other;
   - ranges are not empty and ids are not zero;
   - a runtime input reports the state it is mapped to.

   When any plan allows a restart, every identity source must be declared, every declared
   precondition must have a source for both sessions, and the plan must give a session timeout
   and a resume limit.
7. **Schema version 2.** The postcard encodings of `Program` and `VmState` change, so
   `IR_SCHEMA_VERSION` becomes 2. A program or a journaled VM state of version 1 is refused,
   not decoded by accident. The new fields default when absent from JSON.

## Consequences

- Until the declaration part has a decoder, a runner that reads a source of the form "service
  and field" needs a resolver for the request and the field. Part 3 of the restart work supplies
  one for its tests.
- At M2 the loader also checks each plan's flash-session id against the variant's flash
  sessions.
- A plan has one RequestTransferExit, so a flash session that downloads several regions still
  cannot be declared.
- A tool that reads only the FlatBuffers declaration does not see the restart parameters; the
  JSON form of the program (8.2.2) shows them.
- A frontend writes preconditions into the program, not into the declaration part.
- The response that means "no application" is an enum with one form, a negative response code,
  so a positive-response form can be appended.
