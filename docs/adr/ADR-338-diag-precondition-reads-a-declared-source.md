# ADR-338: `diag.precondition` Reads the Row's Default-Session Source

**Date:** 2026-10-20
**Status:** Accepted
**Affects:** `diag-ir` (`Op::ReadInput`, `DiagHost::runtime_input`, `VmError::InputNotEstablished`,
`IR_SCHEMA_VERSION` 17, `dry_run::Script::input`), `diag-frontend` (the lowering of
`diag.precondition`), `agent` (`policy::check_program`, `host.rs`, the journal's run-stop
record), Starlark subset 3.1, 3.2, 4.6 and 5, design 8.2.4 and 8.9; ADR-233, ADR-245, ADR-254,
ADR-316, ADR-321, ADR-328, ADR-336

## Context

`diag.precondition(name)` is in the `diag` API of the Starlark subset (3.2) with no instruction,
so the transpiler rejects it (`STAR_API_NOT_AVAILABLE`). ADR-254 listed it among the calls
waiting for an instruction, and ADR-336 item 6 gave the annotation `# @preconditions` a meaning
without touching the call. The two are different things. The annotation names the preconditions
a program checks before it runs (design 8.9, the restart's checks of ADR-229); the call reads a
value while the program runs, for example to decide whether to continue.

The data the call needs already exists. `preconditions.csv` (`csv-tables.md` 4.14) gives each kind
a default-session source: a field of a generated DID read, or a runtime input the agent supplies
(`diag_ir::Source`). The agent reads both kinds for a restart, as a `Reading`, through its
`RuntimeInputs` trait and its source table.

Open choices:
- which name the call takes, and whether it depends on `# @preconditions`;
- which instructions the two kinds of source lower to, so that the VM, the agent's policy, the
  journal and the dry run stay as they are for what they already know;
- what the call does when the value cannot be established;
- what it does for the dry run, which sends nothing.

## Decision

1. **The name is a string constant naming a kind**, in the spelling `# @preconditions` uses:
   `voltage`, `externalSupply`, `ignition`, `engine` or `vehicleSpeed`. A name that is not a
   constant `str` is `STAR_NON_CONSTANT_ARGUMENT`, or `STAR_UNSUPPORTED_SYNTAX` when it is a
   constant of another type; a `str` that is no kind is `STAR_ARGUMENT_OUT_OF_RANGE`. A kind with
   no row in the variant's `preconditions.csv` is `STAR_UNKNOWN_PRECONDITION`, the code that
   `# @preconditions` gives for the same fault. The call does not need the name in
   `# @preconditions`: the annotation declares what the program checks before it runs, and a
   procedure may read the supply voltage without declaring a restart check of it.
2. **The source is the row's default-session source, chosen at conversion.** The
   programming-session source belongs to the restart's second check and is not read here. A row
   with no default-session source is `STAR_UNKNOWN_PRECONDITION`; the tables always give one, so
   only a hand-built request index can meet it.
   - A `Source::EcuService { service_id, field_id }` lowers to a `PushBytes` of the request, the
     existing `SendReceive { service }`, then `DecodeField { service, field }`: the instructions a
     `diag.request` and a field read lower to (ADR-316, ADR-321). The request is the declared
     service's SID and prefix, which for a DID read is the whole request; a request with fields
     is not lowerable. The field must decode to an integer (`ScalarKind::Integer`), which the
     table's source rules ask for; a field with a numeric conversion decodes to a float and is
     `STAR_NOT_LOWERABLE`. The `SendReceive` is a diagnostic primitive like any other, so it is
     journaled, the policy treats it as the read it is, and it has `Safe` idempotency by 4.6's
     default for `precondition`.
   - A `Source::RuntimeInput(input)` lowers to a new `Op::ReadInput { input }`.
3. **`Op::ReadInput { input: RuntimeInput }` is appended after `FlashBlock` (ADR-233), with
   `IR_SCHEMA_VERSION` 16 to 17.** It pushes an `I64`. The VM calls the new
   `DiagHost::runtime_input(input) -> Result<Option<i64>, Self::Error>`. The value is in the unit
   the precondition checks use: millivolts for the supply voltage, 0 or 1 for the flags, km/h for
   the speed. `Ok(None)`, an input the host cannot establish, is the appended
   `VmError::InputNotEstablished { input }`; nothing is pushed and the step can be repeated, as
   after a host error. The VM checks the stack's room before the host is called, so an answer is
   never discarded. The instruction sends nothing and is no diagnostic primitive, as
   `DecodeField` and `FlashBlock` are not, so it has no section and no intent marker. It needs
   no declaration part. Every policy class allows it.
4. **The value is an `int`** in Starlark, the subset's type for an `I64`. A flag is 0 or 1.
5. **The agent's `WorkerHost::runtime_input` reads through its `RuntimeInputs`.** `Reading::Value`
   is the answer; `CannotBeEstablished` and `CannotBeDecoded` are `None`; a text reading, which
   no input gives, is `None` too; a failure to reach the worker is a host error. Today the host
   has a source for the supply voltage only (`inputs.rs`), so a procedure that reads another
   input stops with `InputNotEstablished`. A restart's checks and the instruction use the same
   source, so they cannot disagree about a value.
6. **The dry run answers from its script.** `Script::input(input, value)` sets what
   `ReadInput` reads. An input the script does not set is `None`, so the run ends with
   `InputNotEstablished` as on a vehicle without that input: a dry run builds a positive answer
   for a request from the declaration (ADR-328), but it invents no measurement. The read leaves no
   event and charges nothing against the report budget, since it sends and allocates nothing.
7. **Both orders are part of the journal format.** The agent's run-stop record holds a
   `VmError` by variant index (ADR-288), and `InputNotEstablished` holds a `RuntimeInput`, so
   `VmError` is appended to (as before) and the order of `RuntimeInput`'s variants joins the
   formats that are only appended to. The journal's encoding test lists both.

## Alternatives rejected

- **One `Op::ReadPrecondition { kind }` that the VM or the host resolves to a source.** The
  sources are in `Program.preconditions`, which holds only the kinds `# @preconditions` names, so
  the call would need the annotation or a second copy of the sources in the program. The host
  would also interpret an ECU service inside one instruction, and a request sent that way would
  be journaled, checked by the policy and given an idempotency by none of the machinery that
  knows `SendReceive`.
- **An op that takes a `Source` operand for both kinds.** The same hiding of a request, for the
  same reason, and it would give `Source` a second use in the instruction set.
- **Requiring the name to be in `# @preconditions`.** It would couple a run-time read to the
  restart's declaration: reading the supply voltage would force a restart check of it, and a
  procedure with no plan would carry rows it never uses.
- **A call that gives whether the precondition is satisfied.** It loses the value, which a
  procedure may want to log or compare with its own limit. The range stays in the table for the
  restart; a procedure that wants it can compare against its own constant.
- **Giving 0, or a sentinel, for an unestablished input.** The subset has no `None`, and a
  sentinel would let a procedure treat "unknown" as a reading. ADR-229 already treats an input
  that cannot be established as a failed check, and the instruction stops the run the same way.
- **Reading the programming-session source.** It is meant for the one place the agent knows the
  ECU is in the programming session, the restart's second check (ADR-229 item 2); a call has no
  such guarantee.

## Consequences

- `diag.precondition` lowers; `diag.ecu_info` is the one `diag` call left without an
  instruction. ADR-254's remark about the two calls is out of date for this one.
- The schema version changes: a runtime of version 16 cannot decode a program that uses
  `ReadInput`, so the version a server matches against the agent's `capabilities` (9.5) changes,
  as for every appended instruction (ADR-233). The IR golden files change only in their version.
- Every `DiagHost` gains `runtime_input`, test hosts included; the fuzz target's host answers 0.
- A procedure that reads an input the VCI cannot supply (every input but the supply voltage, today)
  stops with `InputNotEstablished`; the agent's sources for the other inputs are separate work.
- A service-backed precondition costs one request per call, and the answer is not cached: two
  calls send twice.
