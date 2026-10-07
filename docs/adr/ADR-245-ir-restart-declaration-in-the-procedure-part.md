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
   are marked deprecated (their slots stay reserved), so every field has one home and nothing
   has to be kept in sync between the two parts. The boundaries are bytecode positions, and design 8.2.1 places retries in the
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

   A precondition has a source for the default session, which the check before the procedure
   starts reads and which is always required, and a source for the programming session, which
   only a restart reads and which is required when a plan allows one; they may be the same. The values that satisfy it form an inclusive range of
   internal values, a single value when both ends are equal.
3. **Recovery-required point.** `RecoveryRequired::Never` or `RecoveryRequired::FromPc(pc)`
   replaces the step number.
   - A plan allows a restart when it is `Never`, or when its point lies after the erase.
   - At run time, on-site intervention applies when the interruption point is at or after
     that point and before the plan's end, or inside a section marked `RecoveryRequired`
     (8.10.1); the stricter rule wins. The point lies inside the plan, before its end.
   - A plan that contradicts a section is refused when the program is loaded.
4. **Boundaries.** A plan has four positions in the bytecode:
   - `entry_pc`: where the replayable pre-erase steps begin;
   - `erase_pc`: the first erase request (a routine control), or the RequestDownload when the
     procedure does not erase. The transfer-start marker is committed before it;
   - `transfer_exit_pc`: the RequestTransferExit, which its marker precedes;
   - `post_transfer_end_pc`: reaching it journals the post-transfer steps as complete.

   A plan's range is a region execution enters only at `entry_pc` and leaves only at
   `post_transfer_end_pc`: it contains no call or return, nothing jumps or calls into it
   except to `entry_pc`, and nothing inside jumps out of it. Inside, no jump skips the erase,
   leaves the transfer backwards past the erase, returns from the post-transfer steps to
   anything but the erase (a redone transfer), or crosses the recovery-required point
   backwards. So every erase passes its transfer-start marker, every RequestTransferExit its
   own marker, every run reaches the end boundary, and a position orders the interruption
   point as the journal's step count does.
5. **Resume limit per stage.** Each plan names the journal stage it counts against (ADR-244)
   and that stage's limit. `VmState` loses `checkpoint` and `resume_count`, which the journal
   owns (ADR-244 item 6, ADR-233 item 3).
6. **Checked at every load.** `Program::validate` checks the declaration, and the agent's
   `check_program` runs it before the program reaches a worker. It checks that:
   - every section lies inside the code with its start before its end;
   - the boundaries are ordered and inside the code;
   - a plan holds one RequestDownload and no RequestTransferExit but its declared one;
   - `erase_pc` is a routine control or a RequestDownload, and `transfer_exit_pc` a
     RequestTransferExit;
   - download requests and flash transfers appear only between a plan's erase and its
     RequestTransferExit, so a program that downloads without a plan is refused;
   - control flow keeps each plan a single-entry region as item 4 describes;
   - plans do not overlap or repeat a flash session or a stage;
   - the recovery-required point lies inside the plan;
   - plans and sections do not contradict each other;
   - ranges are not empty, a yes/no state's range lies within 0 and 1, and ids are not zero;
   - a runtime input reports the state it is mapped to;
   - the no-application response is not 0x00 or the response-pending code.

   When any plan allows a restart, every identity source must be declared, every declared
   precondition must have a source for both sessions, the plan must give a session timeout
   and a resume limit, and no section marked unsafe to repeat (ADR-233) may lie in the range a
   restart replays. Unknown fields in a JSON program are refused, so a misspelt key cannot
   drop a precondition silently.
7. **Schema version 2.** The postcard encodings of `Program` and `VmState` change, so
   `IR_SCHEMA_VERSION` becomes 2. A version 1 program fails to decode or is refused by its
   version. A journaled version 1 VM state can still decode, since postcard ignores the bytes
   of the removed fields, so it is refused by `Vm::check_state`, which a restart runs on any
   state it restores. The new fields default when absent from JSON.

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
- The restart order adds timings in 64-bit arithmetic; the declaration does not bound their sum.
