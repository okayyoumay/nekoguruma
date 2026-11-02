# ADR-410: The Restart Declaration Names Its Target on Each Source and Plan

**Date:** 2026-11-02
**Status:** Accepted
**Affects:** `diag-ir` (`Source::EcuService::target`, `FlashRecovery::target`, `Program::reprogramming_target`, `Program::validate`, six `ProgramError` variants, `IR_SCHEMA_VERSION` 22), `agent` (`inputs::send_source_request`), `diag-frontend` (emits 0), design 8.2.4, glossary; ADR-325 items 4, 5, 9 and 13, ADR-245, ADR-336 item 7, ADR-397, ADR-409

## Context

ADR-325 item 4 decides that the restart declaration's identity sources, precondition sources and
flash plans name their target, and lists what `Program::validate` refuses. ADR-409 gave the
target list its IR form. Left open: where the index lives, what a runtime input names, what the
reprogramming target is for a program without a flash plan, which access target is the first,
what an index outside the list does, and what an agent that serves one link does with its own
read of a source on another target.

## Decision

1. The target is a `u8` index into `Program::targets`, on `Source::EcuService` (next to the
   service and field ids it qualifies, which index one target's declaration part) and on
   `FlashRecovery`. Both default to 0, the primary, when a JSON program omits them. A
   `RuntimeInput` names no target: the agent measures it.
2. The reprogramming target is the target every flash plan names
   (`Program::reprogramming_target`); `validate` refuses plans on two targets
   (`FlashPlansOnSeveralTargets`, keyed by flash session like `OverlappingFlashRecoveries`). A
   program without a plan has none, and the rule that the hardware identity and software
   version sources lie on it does not apply there: the rule's reason is the restart's version
   comparison around the erase, and the agent reads those two sources only at a plan's entry
   and in the flash restart.
3. `validate` refuses, in this order:
   - a target index at or past `target_count()` on a plan (`PlanTargetOutOfRange`) or on any
     identity or precondition source (`SourceTargetOutOfRange`); `TargetOutOfRange` stays for
     instructions;
   - a plan on an access target (`FlashPlanOnAccessTarget`);
   - the hardware identity or software version source off the reprogramming target
     (`SourceOffReprogrammingTarget`), whether or not the plan allows a restart, since the
     plan's entry record reads both;
   - a missing VIN source when the program lists more than one target or any access target, a
     lone access primary included (the existing `MissingIdentitySource(Vin)`);
   - when there is an access target, a VIN source anywhere but on the first one by index
     (`VinOffFirstAccessTarget`). With several targets and no access target the VIN may be on
     any target, the reprogramming target included.
4. A precondition source needs only to be in range: it may name any target, an access target
   included, since a read is what an access target allows. The programming-session remark on
   `Precondition` concerns the reprogramming target; a source on another target is read in
   whatever session that target is in.
5. The agent's own reads of a source (the restart's and the plan-entry record's) send the
   request through one function, `inputs::send_source_request`, called by `resolve_source`,
   `resolve_identity` and `read_field_bytes` (ADR-397). It refuses a source whose target is not
   0 with `HostError::Unsupported` and sends nothing, as the host refuses a request the program
   addresses to another target, until one link per target exists (ADR-325 item 8). Today
   `check_program` and `check_job` already refuse a program that lists several targets, so with
   range checking every source is on target 0 at the agent; the refusal is defence in depth and
   the one place the later per-target routing attaches.
6. `IR_SCHEMA_VERSION` is 22. `ir.fbs` and the journal's records do not change.

## Consequences

A version-21 program, document or journal state is refused with no migration (design 8.2.6). The
golden files and the fuzz seeds are regenerated. The frontend emits target 0 everywhere and no
target list, so every converted program passes the new rules; ADR-336 item 7 stays its rule
(plan's handle target, VIN and preconditions from the primary's tables) until it can declare
targets. The agent's source table stays keyed by service and field id until it holds one
declaration part per target.

Not done here: a VIN source whose service is not a read in a candidate variant, a download
service on another target, and other-target instructions between a plan's erase and its
post-transfer end (they need the declaration part or control-flow tracking of the current
target); the dry run's per-target declarations.

Rejected: a target per `IdentitySources` field (two sources of one precondition could not
differ); a target only per plan (a source's ids would stay ambiguous); `select_target` before
each own read (it would change the target the VM believes the host is on, which it tells once
per run); widening `TargetOutOfRange` with a holder enum (it changes an existing variant);
requiring a precondition source on the reprogramming target (ADR-336 item 7's interim rule).
