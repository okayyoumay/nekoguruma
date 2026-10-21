# ADR-341: Request Params Known Only at Run Time Are Written into the Ingestion-Time Encoding

**Date:** 2026-10-21
**Status:** Accepted
**Affects:** `diag-frontend` (`src/star/lower.rs`: `request_params`, `request_operand`),
`crates/diag-frontend/docs/starlark-subset.md` 3.1 and 3.2; ADR-308 (Consequences), ADR-313
item 4

## Context

`diag.request` and `diag.routine` select a declared service from the SID, the keys of `params`
and the values the service holds as constants (ADR-313), and send an operand that
`diag_ir::encode::encode_request` builds at ingestion (ADR-308). ADR-313 item 4 refused a value
known only at run time under a field's key (`STAR_NOT_LOWERABLE`), and ADR-308 left the run-time
encoding to later work. A procedure that writes a value it read or computed (a counter, a
length, a value taken from an earlier response) could not be expressed.

The VM has the byte instructions this needs (`IndexGet`, `IndexSet`, the shifts and the bitwise
operators, `Fail`); the run-time `write_uint` of subset 2.4 is built from them. Open choices:
whether to add an instruction that runs the encoder in the VM or to emit those instructions;
how the operand's layout is known for the run-time field; how a value the field cannot hold is
reported; and in what order the values, the checks and the send happen.

## Decision

1. **Selection does not change.** A value known only at run time under a key some candidate
   service holds as a constant stays `STAR_NON_CONSTANT_ARGUMENT` (ADR-313 item 4). Under any
   other key it is a field's value: 0 stands in for it in `params` while the service is
   selected and the request encoded, which is safe because selection compares only the keys and
   the constants' values.
2. **Only a fixed-length `uint` field takes such a value for now**, and the value must be an
   `int` (`STAR_UNSUPPORTED_SYNTAX` for another type, `STAR_NOT_LOWERABLE` for a value with
   several possible types, ADR-304). Another field kind is `STAR_NOT_LOWERABLE`. A `uint`
   needs no conversion, so its internal value is the value itself.
3. **The layout is the encoder's, found by encoding each value bit alone.** At ingestion the
   lowering encodes the request once with 0 in each run-time field (the base operand), then
   once more per value bit with only that bit set, and records which operand bit changed. A
   field whose bits do not each change exactly one operand bit is `STAR_NOT_LOWERABLE`. Bit 63
   of a 64-bit field is not probed: the range check of item 4 keeps it 0. Each probe is
   charged to the folding budget (`MAX_FOLD_WORK`) at twice the operand's length, so a large
   request repeated by unrolling cannot make ingestion unbounded. The encoder stays the one
   description of the layout: byte order, bit position and the bytes around the field are
   whatever it writes, and no instruction is added.
4. **Order at run time: values, then checks, then the operand, then the send.** The values'
   code runs in source order, each stored in a temporary. Each value is then checked against
   its field: `(v >> min(width, 63)) == 0` with the VM's logical shift, so a negative value
   fails at every width. A value that fails ends the procedure with `Fail` code
   `PARAM_OUT_OF_RANGE` and a detail naming the param, its width and the line, before the
   request is sent; a `diag` call inside a value has already run, as Starlark's evaluation
   order requires. The operand is then the base operand pushed from the constant pool, with
   each byte a run-time field touches rebuilt by `IndexSet` as the base byte ORed with each run
   of the value's bits, shifted and masked into place. The instruction (`ServiceRequest` or
   `RoutineControl`) then sends it.

ADR-313 item 4 is amended: a field's value known only at run time is encoded at run time as
above, and only a fixed-length `uint` field takes one. ADR-308's Consequences are answered by
items 3 and 4.

## Consequences

- No IR change: programs that use the feature run on any VM of this schema version.
- `Fail` with `PARAM_OUT_OF_RANGE` is terminal like every declared failure (ADR-280), not a host
  error to resume.
- Ingestion does one encode per value bit of each run-time field, at most 63 per field and
  charged to the folding budget.
- The CSV tables place request values at byte edges in whole bytes (`csv-tables.md` 5.5), so
  every run is a whole byte today; the shifted and partial-byte paths serve another frontend's
  layouts and are not reached from the tables.
- Other field kinds (`int`, `bcd`, `float`, bytes and text, and fields with a leading length or
  a termination) stay a backlog item: the fixed-width ones need their own conversion and range
  check in instructions, and a value whose length varies moves the fields after it.
