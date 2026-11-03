# ADR-412: A Flash Plan Selects Its Target Before the Erase and Keeps It to the Post-Transfer End

**Date:** 2026-11-03
**Status:** Accepted
**Affects:** `diag-ir` (`Program::validate`, `ProgramError::PlanTargetNotSelected`, `ProgramError::OtherTargetInPlan`), `diag-frontend` (a rule for the lowering once it emits targets), `agent` (tests), design 8.2.4; ADR-325 items 3, 4 and 9, ADR-245 items 4 and 6, ADR-345, ADR-409, ADR-410, ADR-411

## Context

ADR-325 item 4 lists three refusals that ADR-410 left open because they need the target in force
at an instruction or a declaration part. `validate_control_flow` already makes a plan a
single-entry region (ADR-245 item 4): the window from the erase to the post-transfer end is
entered only at the erase, from the pre-erase steps or the redo jump. The frontend emits no
targets, and a restored state's target is journaled (ADR-409).

## Decision

1. `Program::validate` refuses, per plan and after the control-flow rules, a `SelectTarget` of
   another target between `erase_pc` and `post_transfer_end_pc` (`OtherTargetInPlan`), and a
   pre-erase window on whose paths from `entry_pc` to `erase_pc` the last `SelectTarget` is not
   the plan's target (`PlanTargetNotSelected`), the target before the entry counting as unknown.
   The second check is a forward dataflow over the pre-erase window alone, its state a bitmask
   of at most eight targets. Together they make every instruction of the window run on the
   plan's target (item 4's third refusal). A `SelectTarget` of the plan's own target inside the
   window is allowed. A program with one target satisfies both trivially.
2. A lowering that emits a target list begins every flash plan's expansion with `SelectTarget`
   of the plan's target, whatever is current before it. This narrows item 3's "only where not
   certainly current" at this one point, because the validator does not trust the target
   before the plan.
3. The download instructions (RequestDownload, TransferData, RequestTransferExit,
   `FlashTransfer`) lie in a plan's transfer range (ADR-245 item 6), inside the window, so item
   4's second refusal holds by construction. RequestUpload and RequestFileTransfer outside a
   plan stay the conversion's refusal (item 9), since the IR tracks no target outside a plan.
4. The VIN-read rule lives where a document is checked, not in `Program::validate`, which has
   no declarations (this amends the letter of item 4). The conversion's identity table accepts
   only a generated DID read, and the agent's source table (ADR-345) only a single-DID
   ReadDataByIdentifier of the job's document, so `check_job` refuses another service before a
   link opens. A job that holds every candidate variant's declaration-only document (item 7)
   builds that table against each of them.
5. Both errors carry the instruction's pc for the frontend's source mapping.

## Consequences

No schema or encoding change; golden files are unchanged. A hand-built program with several
targets must select the plan's target inside the plan before the erase even when nothing ever
selected another target. The agent still refuses several targets (ADR-409 item 6), so the new
refusals reach it only through `validate`, which precedes that refusal; the dry run refuses such
a program before its first step.

Rejected: a whole-program dataflow (cost and precision through calls, coupling to the
lowering's analysis); refusing every `SelectTarget` in the window; requiring the `SelectTarget`
literally at `entry_pc` (forbids other-target steps before the erase, which item 4 allows); a
`check_vin_source` in `diag_ir` now (no caller the table does not cover; it joins
`check_declarations` when that takes one part per target).
