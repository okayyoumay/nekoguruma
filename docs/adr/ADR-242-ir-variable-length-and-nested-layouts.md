# ADR-242: IR Variable-Length Fields and Nested Layouts

**Date:** 2026-10-07
**Status:** Accepted
**Affects:** `crates/diag-ir/schema/ir.fbs` (`Field`, `LengthKind`, `Termination`, `Placement`, `Layout`, `Repeat`, `Select`, `TextEntry`, `EcuVariant.layouts`), `crates/diag-ir/schema/README.md`, design 8.2.2

## Context

The IR declaration part (design 8.2.2) expands every response into a flat array of fields with
an absolute bit offset and a fixed width, so that monitoring only walks an array. Three questions
were left open until ODX data was available:

1. ODX length specifications other than "length taken from another field": a leading length
   prefix, a terminator, and "all remaining bytes".
2. Whether an ODX TABLE needs more than `CompuTextTable`.
3. How nested DIDs, which a flat `Field` list cannot express, are represented.

The first available sample is the `somersault` PDX that the MIT-licensed odxtools project ships
as its example (ODX 2.2.0, a fictional ECU). It is not stored in this repository. It answers
question 2 concretely: its `report_status` response carries a TABLE-KEY parameter followed by a
TABLE-STRUCT, and the key selects at runtime between a single value (key 0) and three different
STRUCTUREs (keys 3, 5, 10). Its TEXTTABLE conversion is written as COMPU-SCALEs with a lower and
an upper limit. It uses only fixed-length coded types, so for question 1 the rules come from
ISO 22901-1 (2008) 7.3.6.2 and 7.3.6.10 alone.

Reading ISO 22901-1 against the old schema:

- An ODX TABLE (7.3.6.11) is not a value-to-text conversion. It maps a key (typically a DID) to
  the layout of the data that follows; it is a multiplexer like MUX (7.3.6.10.7), and cascading
  tables (7.3.6.12) nest such selections. `CompuTextTable` is the IR form of the TEXTTABLE
  COMPU-METHOD, a different thing, and it held single values where ODX allows ranges.
- After a variable-length field, the position of the next field is known only at runtime, so an
  absolute bit offset cannot describe it. ODX expresses this with parameters that have no
  BYTE-POSITION.
- Repetitions whose count comes from the message (DYNAMIC-LENGTH-FIELD, DYNAMIC-ENDMARKER-FIELD,
  END-OF-PDU-FIELD) cannot be flattened at import either.

## Decision

1. **Length kinds follow ODX's DIAG-CODED-TYPE kinds.** `Field.length_kind` is `Fixed`,
   `FromField` (the referenced field's physical value, in bits, as ODX defines it),
   `LeadingLength` (a prefix of `bit_length` bits holds the byte length and is not part of the
   value) or `Terminated` (`min_bytes` to `max_bytes`, ended early by `EndOfPdu`, `Zero` or
   `HexFF`). "All remaining bytes" is `Terminated` with `EndOfPdu` and no maximum, so it needs no
   kind of its own. The old `length_from_field` becomes `length_field_id`, used only with
   `FromField`. `Utf8` and `Unicode2` are added to `DataKind`, because termination is defined per
   character for 2-byte strings.
2. **Relative placement.** `Field.placement` is `Absolute` (bit offset from the start of the
   layout) or `AfterPrevious` (the byte edge after the previous field, plus a bit position),
   matching an ODX parameter without BYTE-POSITION. Positions inside a layout are relative to
   that layout, as ODX positions inside a complex DOP are.
3. **Flatten what is static, nest only what depends on the message.** A STRUCTURE whose
   position and size are known at import is inlined into the enclosing layout, as before.
   Everything else becomes a field with a `shape`: a `Repeat` (STATIC-FIELD, DYNAMIC-LENGTH-FIELD,
   DYNAMIC-ENDMARKER-FIELD, END-OF-PDU-FIELD) or a `Select` (MUX, TABLE-KEY with TABLE-STRUCT).
   Both refer by id to a `Layout` in `EcuVariant.layouts`, not by embedding, so a layout used in
   several places (the same DID in several services) is stored once. Nested DIDs and cascading
   tables are a `Select` inside a selected layout; `Field` gets no parent-child link.
4. **Monitoring stays flat.** `MonitorSet` fields must have a fixed position, a `Fixed` length
   and no shape. The frontend rejects a monitor set that needs more, so the 10 ms walk of design
   10.3 is unchanged.
5. **Keys and markers are compared as internal values.** The frontend converts ODX's physical
   KEYs, MUX case limits and end-marker TERMINATION-VALUE through the key's COMPU-METHOD into
   inclusive ranges of internal values, and rejects a key or marker it cannot convert. The agent
   then needs no inverse conversion at runtime. A static TABLE-KEY (one that names a single
   TABLE-ROW) is a constant: the frontend inlines the key bytes and the row's data and emits no
   `Select`.
6. **Counts and keys are ordinary fields decoded first.** ODX places the item count of a
   DYNAMIC-LENGTH-FIELD and the switch key of a MUX inside the complex DOP. The frontend emits
   them as ordinary fields ahead of the `Repeat` or `Select` and refers to them by id, so the
   runtime has a single rule: a referenced field is decoded before the field that refers to it.
7. **Repetitions always end.** An item that consumes no bytes ends a `Repeat`, so an empty
   layout under `UntilEndOfPdu` or `UntilMarker` cannot loop.
8. **TEXTTABLE entries are ranges.** `TextEntry` holds `lower` and `upper` (inclusive) instead
   of one value, matching COMPU-SCALEs with differing limits.

## Consequences

- The declaration part is not generated or decoded anywhere yet (`src/generated` does not exist),
  so the schema is reshaped rather than extended, and `IR_SCHEMA_VERSION`, which guards encoded
  procedure-part programs and journaled VM states, is not bumped.
- The decoder that the declaration part needs (development plan, M2) has to handle nested layouts
  recursively. The frontend rejects layout cycles (ODX forbids infinite recursion), and the
  decoder must still bound nesting depth and check every length and count against the remaining
  message, since an IR document is data from outside the agent.
- The frontend carries most of the ODX knowledge: inlining static STRUCTUREs and static
  TABLE-KEYs, converting keys and markers to internal ranges, and hoisting counts and switch keys.
- The somersault sample is synthetic and covers fixed-length types and one TABLE only. Variable
  length and the FIELD kinds are designed from the standard, not checked against OEM data.
