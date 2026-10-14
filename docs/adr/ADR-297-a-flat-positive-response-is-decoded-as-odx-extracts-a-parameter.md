# ADR-297: A Flat Positive Response Is Decoded as ODX Extracts a Parameter

**Date:** 2026-10-14
**Status:** Accepted
**Affects:** `diag-ir` (`src/decode.rs`, `schema/README.md`), design 8.2.2,
`crates/diag-frontend/docs/csv-tables.md` 4.5

## Context

The IR's declaration part (design 8.2.2) describes each positive response as a flat array of
`Field`s with a placement, a length kind, a data kind, a byte order and a conversion (`Compu`), and
ADR-296 made it readable. Nothing turned a response into values with it: Starlark's
`res["fields"]`, the dry-run validation and the end-to-end test of a CSV and Starlark definition
all need a decoder.

`ir.fbs` follows ODX (ISO 22901-1 (2008)), so the decoder follows ODX's data extraction (7.3.6.3)
and computational methods (7.3.6.6) where the schema states nothing else. Some choices are left
open by both: how the response's fixed prefix is checked (ADR-235 item 3 leaves it to the
declaration-part decoder; ADR-295 puts sub-functions and echoed identifiers there), whether a
`LinearScale` range includes its upper limit, what a failing field does to the rest of the
response, and what the decoder does with a plan that breaks a schema rule only a frontend checks.

## Decision

1. **One entry point checks the header.** `diag_ir::decode::decode_positive_response` takes the
   whole response from its SID. The SID must be `PositiveResponse.sid` and the bytes after it
   must start with `PositiveResponse.prefix`; otherwise the response is rejected before any
   field is read. Positions count from the SID, an `AfterPrevious` first field starts after the
   prefix, and bytes after the last field are not read and are not an error.
2. **Bits are extracted as ODX extracts a parameter.** A field reads the bytes it touches from
   its start byte, reverses them when it is little-endian, shifts them down by its bit position
   and masks its width (7.3.6.3.2 b to d). The data kinds are read as:
   - `Int` in two's complement, `Uint` unsigned, both 1 to 64 bits;
   - `Float` as a single- or double-precision binary floating-point bit pattern, 32 or 64 bits;
   - `Bcd` as packed BCD (one digit per 4 bits, most significant first after the byte-order
     normalization); a digit above 9 fails;
   - `Ascii` as the 8-bit character set ODX gives it (7.3.6.2), so every byte is the character
     of the same code point;
   - `Utf8` as UTF-8, failing on invalid bytes;
   - `Unicode2` as 2-byte characters in the field's byte order, failing on an odd length and on a
     UTF-16 surrogate, which is not a character on its own;
   - `Bytes` as they are.
3. **Lengths follow the schema's length kinds.** A `FromField` length is the physical value of a
   field decoded earlier in the same array, in bits; a negative, fractional or too large value
   fails. `LeadingLength` and `Terminated` apply only to `Bytes` and the string kinds, as ODX
   allows (7.3.6.2). A terminator counts only from `min_bytes` on and, in a `Unicode2` field, only
   as a whole character; it is dropped from the value but consumed. A terminated value otherwise
   ends at `max_bytes` or at the end of the message, and a message that ends before `min_bytes`
   fails.
4. **Conversions give a float, except identity and text tables.** `CompuIdentity`, or no
   conversion, keeps the internal value. `CompuLinear`, `CompuScaleLinear`, `CompuTableInterp` and
   `CompuRational` compute in double precision and give a float, as the CSV format already states
   (ADR-295); unlike ODX (7.3.6.6.1), the calculation type does not depend on the internal and
   physical types. A `LinearScale` range includes both limits, ODX's default closed interval
   (7.3.6.6.1), and the first matching scale applies. An interpolation table needs points whose
   internal values ascend strictly, checked over the whole table; a value on a point gives that
   point's physical value exactly (7.3.6.6.8), and a value outside the table has no result. A text
   table keeps the internal value and reports the text id of the first entry whose range holds it,
   else the default text; with no default text (0) the value has no result, as in ODX (7.3.6.6.1).
   A rational function whose denominator is zero for the value, or any result that is not finite
   (an overflow), fails; a linear or scale-linear denominator is a constant, so zero there is a
   broken plan.
5. **A field that fails fails the response.** The decoder returns every field or an error naming
   the failing field, never a partial result. Errors are classed by who has to act:
   - `InvalidValue` and `Conversion`: the message holds something the plan cannot decode (the
     ECU's side);
   - `Truncated`: the message ends inside a field;
   - `InvalidPlan`: the plan breaks a schema rule that a frontend must enforce, such as a
     16-bit float, a terminated integer, a numeric conversion on a string, a constant zero
     denominator or a scale-linear conversion without scales. The loader
     (ADR-296) checks only what the schema states as types and tags, so the decoder rejects such
     a plan where it meets it rather than guessing;
   - `Unsupported`: a field with a shape (a nested layout, ADR-242) or a `CompuBytecode`
     conversion, which this decoder does not handle yet.

## Consequences

- A Starlark procedure's `fields`, the dry-run validation and the end-to-end test can decode the
  flat part of a response. The nested layouts of ADR-242, DTC records and variant patterns build
  on the same extraction.
- A `Uint` above 2^53 loses precision once a numeric conversion turns it into a float; an
  identity conversion keeps it exact.
- An ECU that reports a value no text entry, scale or interpolation range covers makes the whole
  response fail. That is ODX's behaviour (ISO 22901-1 (2008) 7.3.6.6.1); a caller that wants the
  other fields has no partial result to fall back on.
- A `Float` field whose bytes are NaN or an infinity passes through an identity conversion
  unchanged; a numeric conversion of it has no result (`Conversion`), and a text table on it is
  a broken plan.
- The flat decoder sets no limit of its own on the number of fields or the bytes it copies: a
  field array is bounded by the verifier's limits on the document (ADR-296) and a value by the
  message, so a document with many fields that each read the rest of a long message copies it
  once per field. The document is signed (8.2.6); the decoded-value limit of ADR-242 applies
  when nested layouts are decoded.
- `ir.fbs` does not state the scale-linear rule: a change to the schema file goes with
  regenerated bindings and a schema version bump, so the rule is recorded here,
  in `crates/diag-ir/schema/README.md` and in the CSV format.
