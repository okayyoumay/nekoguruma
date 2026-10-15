# ADR-302: Decoded Shape of Nested Layouts

**Date:** 2026-10-15
**Status:** Accepted
**Affects:** `crates/diag-ir/src/decode.rs` (`PhysicalValue`, `decode_positive_response`, `decode_dtc_fields`, `DecodeError`), `crates/diag-ir/schema/README.md`, design 8.2.2

## Context

ADR-242 lets a response field carry a shape (`Group`, `Repeat`, `Select`) that refers to a
nested layout, and ADR-297's decoder rejected such fields. Following them needs four choices that
neither ADR makes: what a decoded group, repetition or selection looks like to the caller; how
the decoder reaches the layouts, which live in `EcuVariant.layouts` rather than in the response;
which occurrence a reference to a length, count or key field means when the same field id is
decoded more than once; and how the limits of ADR-242 item 7 are counted.

## Decision

1. **A shaped field decodes to the fields of its layout.** `PhysicalValue` gains three variants,
   so a `DecodedField` keeps its id, name and unit whatever its shape:
   - `Group(Vec<DecodedField>)`: the fields of the one layout occurrence;
   - `Repeat(Vec<Vec<DecodedField>>)`: each item's fields, in message order;
   - `Select { case, fields }`: `case` is the index of the matching `SelectCase` in
     `Select.cases`, or `None` for the default case, and `fields` are the chosen layout's fields
     (empty for a case or default without a layout).
   The key, count and marker fields are not repeated inside these values: a count or key is an
   ordinary field decoded before the shaped one (ADR-242 item 6), and a marker is left for the
   next field.
2. **The caller passes the layouts.** `decode_positive_response` and `decode_dtc_fields` take
   the variant's `layouts` (`decode::Layouts`) and look a layout up by id with the binary search
   the loader's id-order check allows. A layout id that names no layout is a broken plan.
3. **Positions, bounds and ends.** A nested layout's `Absolute` positions count from its first
   byte, and its first `AfterPrevious` field starts there. Its fields must end within its
   `byte_size` when that is set, within a `Fixed` item's `item_byte_size`, and within the
   enclosing layout otherwise; a field reaching further is `Truncated`, which now names the end of
   the message or of the field's layout. A layout with a `byte_size` ends there; one without ends
   after its field that ends last. A shaped field ends where its layout, its last item or its
   chosen case ends, and must start at a byte edge.
4. **References find the nearest occurrence.** A `FromField` length, a `Repeat` count and a
   `Select` key name a field decoded earlier in the same layout occurrence or an enclosing one;
   the innermost occurrence wins, and within one occurrence the latest. Inside a repeated item
   this is the same item's field, as `ir.fbs` says. A length or count is the field's physical
   value; a key or marker is its internal value as an integer, with a byte or string value of up
   to 8 bytes read as a big-endian integer stored bit for bit in an `i64` (ADR-242 item 5). A
   float, or a longer byte string, cannot be a key.
5. **Repetitions.** A `Fixed` repetition places item n at n times `item_byte_size`; an item past
   the enclosing layout is `Truncated`. The other kinds start each item at the byte edge after
   the previous one. `UntilMarker` reads the marker before each item and stops when its value is
   within the range or the layout has no bytes left; too few bytes for a marker mean no marker.
   `UntilEndOfPdu` stops when the layout has no bytes left. Every kind stops at an item that
   consumes no bytes, which is not part of the result.
6. **Limits.** More than 16 nested layouts below the top level fail with `TooDeep`, naming the
   shaped field whose layout is the seventeenth. Every decoded field counts as one value, nested
   ones and the shaped fields themselves included, and more than 65,536 fail with
   `TooManyValues`. Items are not counted on their own: each consumes at least one byte, so the
   message bounds them.
7. A key that matches no case of a `Select` without a default is an invalid value of the
   message (`InvalidValue`), not a broken plan.

## Consequences

- Callers of the two decoding functions now pass the variant's layouts; the fuzz target over
  the declaration part does, so arbitrary documents exercise the nested decoder too.
- A Starlark response's `fields` still exposes top-level scalars only (subset 3.4); reading
  repeated or selected fields from a procedure is a separate item.
- References search a small stack of per-layout maps, at most 17 deep, so a document with many
  length, count or key references stays linear in the message.
- `PhysicalValue` is no longer only a scalar: code that converts or compares values must treat
  the three new variants as not numeric, as the conversion and reference code here does.
