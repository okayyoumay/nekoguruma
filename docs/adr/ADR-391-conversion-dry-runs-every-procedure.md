# ADR-391: Conversion Dry-Runs Every Procedure

**Date:** 2026-10-30
**Status:** Accepted
**Affects:** `diag-frontend` conversion (`convert.rs` `lower_procedures`, `dry_run_verdict`;
`StarCode::DryRunStopped`), `diag-ir` dry run (`Script::built_defaults`, `DryRunReport::pc`,
`DEFAULT_MAX_STEPS`, `DryRunEvent::FlashTransfer`), `agent` (`JobLimits::default`); `crates/diag-frontend/docs/starlark-subset.md`
5 and 6; design 12.1; ADR-328 Consequences

## Context

ADR-328 built the dry run and left to its caller what counts as a pass: on answers built from
the declarations a procedure may end in its own failure, which is a valid definition, and a
response whose built answer cannot decode stops a field read with a VM error although the
definition may be valid. No caller ran it, so a procedure that sends what its variant does not
declare, or stops on a VM error, was found only on a vehicle.

A dry run at conversion has no server key, no operator and no vehicle inputs, so a procedure
that unlocks a variant, asks the operator or checks a precondition stopped for want of an
answer, and a loop needs a bound.

## Decision

1. **Conversion dry-runs every lowered procedure on every variant.** Right after a procedure
   lowers for a variant, conversion runs it on that variant's declarations and judges the end
   in one place. A refusal is reported per variant as ADR-311's other conversion errors are.
2. **The procedure's own ends pass; every other stop refuses.** A run passes when it finishes,
   when it ends in a `Fail` (the author's or one the lowering emits, such as an exhausted loop
   limit), or when a field read stops because a response answered from its declaration does
   not decode (ADR-328 item 3). Every other VM error, every host error, the step limit and
   every other end refuse the definition with `STAR_DRY_RUN_STOPPED`, at the source of the
   instruction where the run stopped.
3. **The other answers are zeros too.** Conversion scripts a key of four zero bytes, an empty
   positive answer to every HMI form and record, and zero for every runtime input, the same rule
   as the built responses. A check of such a value takes the procedure's own failure path,
   which passes.
4. **The step budget is the agent's.** A run is bounded by the step budget the agent's job
   limits use by default, one million instructions. A run that reaches it is refused: every
   loop is bounded, and the agent would stop it too.
5. **Transfer data does not fill the report.** A flash transfer event holds the block's length
   and a digest of its bytes, not the bytes, so the report's budget (ADR-328 item 7) does not
   bound the size of an image the dry run serves. The step budget still does.

The maintainer chose zero answers over nominal values, refusing at the step budget over passing
as inconclusive, and the rest of this decision (2026-10-30).

## Consequences

- A definition that sends an undeclared request, or that indexes, divides or converts a value
  read from the vehicle without guarding it into a VM error, is refused at conversion. Built
  answers are zeros, so such code must guard its reads.
- A value out of range for a parameter or an unsigned field ends in a failure the lowering
  emits, and so passes, as it would end the procedure on a vehicle. Only a stop that is no end
  of the procedure refuses.
- A flash plan's body runs at conversion: its preconditions are checked by the agent before the
  run, not by the program, so the zero inputs do not stop it. A path behind a check of a value
  read from the vehicle takes the procedure's own failure under the zero answers and is not
  exercised.
- ADR-328's open pass criterion is decided here, and its item 7's transfer data is held as a
  length and a digest.
