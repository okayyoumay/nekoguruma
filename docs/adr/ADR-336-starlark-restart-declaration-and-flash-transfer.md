# ADR-336: The Starlark Restart Declaration and `diag.flash_transfer`

**Date:** 2026-10-20
**Status:** Accepted
**Affects:** `diag-frontend` (Starlark lowering, CSV conversion), `crates/diag-frontend/docs/starlark-subset.md`
3.1, 3.2, 4.5, 4.6, 4.9 and 5, `crates/diag-frontend/docs/csv-tables.md` 1, 4.12 and the new
tables, `diag-ir` (`Op::FlashBlock`, `DiagHost::artifact_bytes`, `FlashSegment`, `IR_SCHEMA_VERSION`),
`agent` (`WorkerHost`, `check_job`, `ngr-agent run`), design 8.2.4, 8.2.5, 8.4, 8.9, 11.3; ADR-229,
ADR-245, ADR-250, ADR-295, ADR-301, ADR-304, ADR-332, ADR-335

## Context

A flash write is the one procedure the design restarts after a crash (8.2.5), and the IR has
carried the restart declaration since ADR-245: sources for the identity and the preconditions
and one `FlashRecovery` plan per flash session. No lowering writes it. The Starlark lowering
(ADR-304) refuses `diag.flash_transfer`, so a procedure written in Starlark can neither transfer
data nor declare where a restart begins. What decides the shape:

- `Program::validate` refuses a plan that holds a `Call` or `Ret`, and a `Call` whose target
  reaches a plan (ADR-245 item 4). The lowering emits `Call(main); Ret` at pc 0 and 1 (ADR-304
  item 1) and copies-and-calls section functions (ADR-301), so no lowered plan can pass today.
- `Op::FlashTransfer` takes its bytes from the stack. No instruction reads artifact bytes, and
  the agent has no notion of an artifact.
- The host's tracked transfer begins only on a positive RequestDownload (ADR-250 item 3).
- ADR-325's targets are not in the IR yet, and the agent resolves `Source::EcuService` through
  a caller-supplied table, not through the document.
- One run has 1,000,000 steps (`JobLimits::max_steps`), and a block costs about nine.
- Subset 4.6 leaves `diag.request` for session control `Unsafe`, which ADR-335 item 8 and ADR-332
  (Consequences) expected to be revisited with this declaration.

## Decision

Facts about the ECU live in the definition directory's tables. Facts about the procedure are
comment annotations next to the code they describe.

1. **ECU facts are CSV tables, per variant like `services.csv`.** The converter copies them into
   every `Program` it lowers for the variant; `ir.fbs` gains no field for them (ADR-245 item 1's
   one home stays).
   - `identity.csv` names, for `vin`, `hardware_part_number` and `software_version`, a read
     service and the response field. A row whose service is not a read is refused. A plain
     procedure gets its VIN source from it too (ADR-335 item 6, ADR-327).
   - `preconditions.csv` names, for `voltage`, `external_supply`, `ignition`, `engine` and
     `vehicle_speed`, the satisfying range, a default-session source and an optional
     programming-session source. A source is `<service>.<field>` or `input:<name>` for one of
     the agent's own inputs.
   - `flash_sessions.csv` gains the session timeout, teardown margin, ECU startup time,
     confirmation window, version-read retries and the optional no-application response; they
     become the plan's `RecoveryTiming`, `version_read_retries` and `no_application`.
     `flash_segments.csv` gains `data_format` (default 0x00) and `address_and_length_format`
     (default 0x44), which become `FlashSegment` fields in `ir.fbs`, appended with defaults.
     `block_size` is the TransferData payload per block.
2. **A plan is a whole top-level function, expanded in place.** `# @flashPlan session=<int>
   [maxResumes=<int>] [recovery=never]` stands directly before a top-level `def`, under the
   placement rules of `@section`, and may share the `def` with one. `session` must name a
   `FlashSession` of the target the call addresses. `maxResumes` defaults to 3, and the stage is
   the session id. Each of these is a `STAR_PLAN_SHAPE` error: the function is not called
   exactly once; the call is not a statement of `main`'s body (an `if` is allowed, a `for` is
   not); the caller is another function, a plan or a section function; the plan has a `return`;
   the plan is the callee of a section function.
3. **Everything that runs during the plan's call is in the plan** (ADR-301 item 3). Every function
   the plan calls, directly or not, section functions included, is expanded in place: its `Ret`
   becomes a jump to the join, its locals take fresh slots in `main`'s frame, and its section
   pieces (ADR-332 item 4) are laid over the inlined range. Inlined instructions count toward
   the copy limits. A section function inlined in a plan is entered by fallthrough at its first
   piece, which amends ADR-332 item 6.
4. **Boundaries are derived from the lowered code.** The plan's entry is the first instruction of
   the expansion, and its post-transfer end is the pc after the last. The transfer exit is the
   `ServiceRequest 0x37` the expansion emits. The erase point is the `RoutineControl` of the
   top-level statement annotated `# @erase`, or the `ServiceRequest 0x34` when the session's steps
   have no `erase-memory`; `@erase` is required exactly when they have it. `recovery_required`
   is the erase point by default, `Never` with `recovery=never`, or the first instruction of the
   statement annotated `# @recoveryRequired` (exclusive with `recovery=never`). The `@erase` and
   `diag.flash_transfer` statements are top-level statements of the plan, so no jump skips them.
   A `Program::validate` refusal is `STAR_PLAN_INVALID` at the source span of the pc the error
   names; `STAR_NOT_LOWERABLE` stays for an error without one.
5. **Layout (amends ADR-304 item 1).** `main`'s body is emitted at pc 0 and ends in a `Ret`; the
   other functions follow. No `Call` leads to a plan any more.
6. **Preconditions a procedure requires** are `# @preconditions voltage ignition ...` before
   `def main()`. Each name needs a row in `preconditions.csv` (`STAR_UNKNOWN_PRECONDITION`). The
   annotation is optional; without it the program declares none.
7. **Targets until the IR carries them.** A plan's target is the target of the handle
   `diag.flash_transfer` is called on. Its hardware-part-number and software-version sources
   come from that target's `identity.csv`, the VIN source from the first access target's table
   (the primary's when there is none), and the precondition sources from the reprogramming
   target's table. This is a frontend rule that ADR-325's IR targets replace.
8. **Idempotency defaults (the 4.6 revisit owed by ADR-332 and ADR-335).** `diag.request` with
   DiagnosticSessionControl (0x10) or TesterPresent (0x3E) defaults to `Safe` everywhere: a repeat
   re-enters the wanted session and behaves in any residual state. Inside a plan's expansion an
   unannotated `diag.request` of any other non-read service defaults to `CheckState` (ADR-329's
   "not stated"), because the plan's redo rules govern repeats there and `Unsafe` would make
   every plan with a session change or a post-transfer reset fail `UnsafeSectionInReplay`.
   `# @idempotency unsafe` still forces a refusal.
9. **UDS only (ADR-309 item 6).** `diag.flash_transfer` or `@flashPlan` in a variant whose
   application layer is not UDS is `STAR_NOT_IN_PROFILE`.
10. **Expansion of `diag.flash_transfer(session)`**, inside a plan function only: a
    RequestDownload (`ServiceRequest` 0x34, operand built from the segment: the two format
    bytes, then address and length in the widths the format gives, ISO 14229-1 (2026) 14.2.2.3; a
    value that does not fit is a conversion error), a `for` loop over `ceil(length / block_size)`
    blocks (a constant, so no run-time loop limit, 4.2) of `FlashBlock` then `FlashTransfer`, and
    a RequestTransferExit (0x37, the service id alone). The call's value is the 0x37 response in
    the 3.4 shape with no `fields`. The 0x34 response is dropped: a negative one makes the first
    `FlashTransfer` a host error (no tracked transfer, ADR-250 item 3), which interrupts a step
    inside the plan and hands the run to ADR-229's restart within `maxResumes`. A terminal
    `Fail` there would strand an erased ECU.
11. **`Op::FlashBlock { session, segment }`**, appended after `LogFromStack` (ADR-233), pops an
    `I64` block index and pushes that block's `Bytes`. The VM, stepped with declarations
    (ADR-316), finds the segment on the current target, checks the index, computes the offset
    and length (the last block may be shorter), and calls a new
    `DiagHost::artifact_bytes(offset, len)`, which must return exactly `len` bytes. It sends
    nothing and is no diagnostic primitive (no section, intent or journal record). Design 8.2.4's
    "no file access" holds: the instruction names a declared segment, and the host decides what
    the artifact is. Conversion refuses a `block_size` above `MAX_BYTES_LEN - 2`.
12. **The agent's artifact.** The job names the artifact by content hash, as it names documents
    (ADR-318). `check_job` hashes the cached file before the first instruction (design 4.1, 11.1,
    11.3), refuses a mismatch and any plan segment outside the file, and hands the bytes to
    `WorkerHost`. `ngr-agent run` takes an `--artifact` path. The host reads the length format
    and maxNumberOfBlockLength from the positive 0x34 response (14.2.3.2) and refuses a block
    whose `2 + data.len()` exceeds it (`HostError::BlockTooLong`) before sending. Every policy
    class allows `FlashBlock`.
13. **One segment per session for now.** `validate_plan` admits one 0x34 and one 0x37 per plan
    (ADR-245 Consequences), so a session with several segments is `STAR_NOT_LOWERABLE`.
14. **Checksum and CheckMemory.** The expansion does not read `FlashSegment.checksum`: the
    artifact's integrity is the job's hash, and a per-version checksum does not belong in a
    per-variant table. CheckMemory stays the author's `diag.routine` with constant parameters.
    The author has no access to the 0x34 response.
15. **Dry run (ADR-328).** The script serves `FlashBlock` with zero bytes of the requested length.

Rejected alternatives:
- **An all-Starlark declaration** (sources and timings in annotations or a `diag.restart(...)`
  constant). It repeats ECU facts per procedure, needs a service-and-field selector expression,
  and leaves a plain procedure without its VIN source.
- **An all-CSV declaration** (a `flash_plans.csv` naming functions or pcs). Boundaries are
  positions in code, which tables cannot see; ADR-295 item 12 already placed them with the subset.
- **A plan as a copied-and-called subroutine.** ADR-245 item 4 forbids it, and relaxing it breaks
  the pc-ordered interruption point the journal and the restart depend on.
- **`@section`-style range markers inside `main`.** They reopen the mis-pairing ADR-301 closed.
- **One instruction that transfers a whole segment in the host.** It loses the per-block journal
  records (ADR-250 item 5, ADR-252 item 2), the cancel points and the step budget.
- **The host resolves session, segment and index itself.** It duplicates the declaration logic the
  VM has (ADR-316) and makes hand-built IR tests and the dry run need a document.
- **Adapting the block size to maxNumberOfBlockLength at run time.** The loop bound becomes a
  run-time value, and the declaration is the ECU's contract (ADR-229 item 4).
- **A `diag.erase(...)` call instead of `# @erase`.** It is a second way to send a routine.
- **Several RequestDownload rounds in one plan.** It needs an ADR-245 and journal change (one
  exit marker per plan).
- **A terminal `Fail` on a refused RequestDownload.** It forfeits the sanctioned restart after
  an erase.

## Consequences

- Eight choices were open when this was decided; the first alternative of each was not taken.
  Items 1 and 14 keep identity and precondition sources in CSV (alternative: Starlark). Item 4
  cross-checks `@erase` against the session's `erase-memory` step (alternative: trust either
  one). Item 8 defaults in-plan requests to `CheckState` (alternative: require explicit
  annotations). Item 6 makes `@preconditions` optional (alternative: required for a plan).
  Item 2's `maxResumes` defaults to 3 (alternative: required). Item 1 adds the two format fields
  to `FlashSegment` now, in the one schema bump `FlashBlock` needs (alternative: constants).
  Item 12 makes the host pre-check maxNumberOfBlockLength (alternative: let the ECU refuse).
  Item 14 gives the author no access to the 0x34 response (alternative: expose it). The
  maintainer may overrule any of them.
- Moving `main` to pc 0 changes every lowered program: golden files and tests that assume pc 0
  is a `Call` change (design 12.1's golden update).
- A run carries about 110,000 blocks at the default step limit, so an image larger than that
  times `block_size` needs a higher limit or several runs.
- `block_size + 2` is also capped by the worker's message size; J2534 v04.04 has a limit the
  agent does not yet check at the job.
- Follow-ups: several downloads per plan (needs the ADR-245 amendment), a host-computed digest for
  CheckMemory, a RequestTransferExit parameter record, precondition sources on another target, and
  the check of `block_size + 2` against the worker's message size.
- ADR-245, ADR-250, ADR-254, ADR-295, ADR-301, ADR-304, ADR-318, ADR-328, ADR-332 and ADR-335
  carry a status annotation pointing here. ADR-301's open question on a plan is answered by
  item 3, and ADR-335's remedy for session control by item 8. ADR-330 item 4's cancel held inside
  a plan becomes reachable once plans are emitted.
