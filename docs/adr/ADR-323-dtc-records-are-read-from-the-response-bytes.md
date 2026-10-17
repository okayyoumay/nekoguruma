# ADR-323: DTC Records Are Read from the Response Bytes

**Date:** 2026-10-17
**Status:** Accepted
**Affects:** `diag-frontend` Starlark lowering (`res["fields"]["dtcs"]`, `len`, indexing, a
record's `code` and `status`), `crates/diag-frontend/docs/starlark-subset.md` 3.4; ADR-233,
ADR-314, ADR-316

## Context

ADR-314 item 4 gives `diag.read_dtc`'s result a `dtcs` field: a list of records, each with a
`code` and a `status`. The IR's run-time values (`diag_ir::Value`) are integers, floats, booleans
and bytes, and `Op::DecodeField` decodes one scalar field, so a list has no value to live in, and
`dtcs` was refused at ingestion.

Two ways were open:

- **A list value.** Append a list (or record) variant to `Value` and an instruction that decodes
  a repeated field into it. This changes the VM state that the journal records, raises the IR's
  schema version (ADR-233), and every instruction that takes values has to decide what a list
  means to it.
- **Per-record access to bytes.** Keep the records as the bytes they are and read a record's
  fields where the procedure reads them.

The profile already fixes the records' layout. At ingestion `diag.read_dtc` checks that the
selected response holds, after its mask, a repeat until the end of the message of unpadded
records: a code then a status, each a whole-byte big-endian `uint` without a conversion
(`dtc_response_fits`). Their position in the message is known when the program is lowered.

## Decision

1. **`dtcs` is the response's record bytes.** `res["fields"]["dtcs"]` lowers to the response
   bytes from the records' first byte to the end of the message (`Op::Slice`). Before slicing,
   the mask is decoded and dropped (`Op::DecodeField`). Decoding checks the whole message
   against the declaration, so the result is the same as for any field read: a negative
   response stops the run, and so do a response to another service and one whose last record
   is cut short (ADR-316). After that the records are whole.
2. **The procedure reads them by count and index.**
   - `len(dtcs)` is the bytes' length over a record's size.
   - `dtcs[i]` is the `i`-th record's bytes. Its first byte is read before slicing, so an index
     outside the records, a negative one included, stops the run as an index out of range, as an
     index into bytes does.
   - A record's `code` and `status` are read with literal keys, each as a big-endian `uint` of
     the profile's width, built from the record's bytes with `IndexGet`, shifts and `BitOr`.
   - Any other key is `STAR_NOT_IN_PROFILE`, and a key that is not a literal is
     `STAR_DYNAMIC_KEY_NOT_ALLOWED`.
   - The records and a record are their own lowering types, as a response and its fields are.
     Their truth value, a comparison, an index that is not an `int`, and `for d in dtcs` are
     `STAR_NOT_LOWERABLE`. A loop runs over `range(len(dtcs))`, which takes the subset's loop
     limit (4.2).
3. **No IR change.** The lowering uses instructions that exist, so the schema version, the VM
   state and the journal stay as they are.

## Consequences

- A procedure reads DTCs as `for i in range(len(dtcs)): dtc = dtcs[i]`, not by iterating the
  records directly. Iterating them would need a lowering of `for` over a value known at run
  time, which no other value has either.
- A record is read field by field, so a profile whose records hold other fields than a code and
  a status would need its own keys here; the UDS profile's records hold only those two.
- A list value in the IR remains possible later, for responses whose repeats the profile does not
  fix; nothing here stands in its way.
