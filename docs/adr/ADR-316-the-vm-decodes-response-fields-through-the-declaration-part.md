# ADR-316: The VM Decodes Response Fields Through the Declaration Part

**Date:** 2026-10-16
**Status:** Accepted
**Affects:** `diag-ir` (`src/lib.rs`: `Op::DecodeField`, `Vm::step_with_declarations`, four
`VmError` variants, `IR_SCHEMA_VERSION` 11), `agent` (`src/policy.rs`, `src/journal.rs`), the IR
golden files, design 8.2.4; ADR-233, ADR-297

## Context

A Starlark procedure reads a decoded response value as `res["fields"]["name"]` (subset 2.5 and
3.4). The decode plans of every response field live in the declaration part: the FlatBuffers
`EcuVariant` of the variant's document. ADR-297 already decodes a positive response through them
(`diag_ir::decode::decode_positive_response`).

The procedure part's VM has no instruction that turns response bytes into a field's value, and
it sees only its `Program` and a `DiagHost`. There were two places to put the decoding:

- **The VM**, given the declaration part. Decoding is a pure function of the declarations and
  the bytes, so the VM would be the only implementation.
- **The host**, as a new `DiagHost` method. The VM would stay unaware of the declarations, but
  every host, mocks included, would decode, and the result of a step would depend on the host.

The maintainer chose the first.

## Decision

1. **`Op::DecodeField { service, field }`** is appended after `FloatToInt` (ADR-233).
   - It replaces the `Bytes` on top of the stack with the value of field `field` (`Field.id`).
     The bytes must be a whole positive response starting with its SID. The field must be one
     of the top-level fields of service `service`'s (`Service.id`) declared positive response.
   - Decoding goes through `decode_positive_response`, so the response SID, the prefix and every
     field's length and conversion are checked as ADR-297 checks them. The response is decoded
     as a whole: a field reads only from a response that decodes, so a truncated or malformed
     response, or a field with a conversion the decoder does not run, makes every field of it
     unreadable rather than letting a procedure act on part of a response that is wrong.
   - It is not a diagnostic primitive: it sends nothing, goes through no host, and is pure like
     the arithmetic instructions.
2. **The VM is stepped with the declarations.**
   - `Vm::step_with_declarations(program, declarations, host)` takes the variant's `EcuVariant`.
     `Vm::step` is that call without declarations, so existing callers are unchanged until they
     need decoding.
   - The declarations are not part of the VM state, so the journaled state stays the same. The
     caller passes the variant's document for the job. The agent's job runner does not hold one
     yet and still calls `Vm::step` (Consequences).
3. **Values map onto the VM's value types.**

   | Decoded value | VM value |
   |---|---|
   | Unsigned or signed integer, BCD, a text table's internal value | `I64` |
   | Float, or a field with a numeric conversion | `F64` |
   | Text | its UTF-8 bytes as `Bytes` (the VM has no string type) |
   | Bytes | `Bytes` |

4. **Errors are VM errors**, appended to `VmError`. As for every VM error, the state is left
   unchanged.
   - `NoDeclarations`: the VM was stepped without declarations.
   - `UnknownField`: the service or the top-level field is not declared.
   - `ShapedField`: the field is a nested layout, whose value has no VM type. This and
     `UnknownField` depend only on the declarations, so they are checked before decoding and
     reported whatever the response holds.
   - `Undecodable`: the bytes do not decode as that positive response. A negative response is
     one such case.
   - An unsigned value above `i64::MAX` is `Overflow`.
   - `IR_SCHEMA_VERSION` becomes 11, as for the earlier instruction additions.

## Consequences

- The agent's runner does not hold a declaration part yet, so it steps without one. A program
  using `DecodeField` fails there with `NoDeclarations` until the runner loads the job's
  document. Nothing lowers to the instruction yet: the field-read lowering is separate work.
- A procedure has to check that a response is positive before reading a field of it, or the read
  fails the run as `Undecodable`. The lowering of `res["fields"]` has to account for this.
- Nested layouts (`group`, `repeat`, `select`) cannot be read from a procedure until the VM has a
  value type for them.
- `Undecodable` does not say why the response failed to decode. ADR-297's errors are not kept,
  so the journal cannot tell a truncated response from an unsupported conversion.
- Old agents cannot read journals that record one of the new errors. Earlier `VmError` additions
  had the same limit.
