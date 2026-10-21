# ADR-308: Encoding a Request Operand

**Date:** 2026-10-16
**Status:** Accepted (the Consequences' run-time parameter decided by ADR-341)
**Affects:** `diag-ir` (`src/encode.rs`, `schema/README.md`), design 8.2.2,
`crates/diag-frontend/docs/csv-tables.md` 5.3

## Context

A Starlark `diag.request` or `diag.routine` sends an operand that the lowering builds from the
call's parameters: `Request.prefix` followed by the request's fields, each holding a parameter's
internal value (`csv-tables.md` 5.3). The response side has a decoder that extracts each field as
ODX extracts a parameter (ISO 22901-1 (2008) 7.3.6.3, ADR-297), but nothing wrote a value into
a field. The schema says where a field is and how it is coded; it does not say what an encoder
does with a value that does not fill a fixed-length field, a string that contains its
terminator, or a field whose length comes from another field.

## Decision

1. **The encoder is the decoder's inverse.** `diag_ir::encode::encode_request` takes a `Request`
   and a value per field `short_name`, writes the SID, the prefix and each field at the position
   and in the byte order the decoder reads it from, and returns the bytes after the SID. A value
   written and read back through the same fields gives the same value; the tests check that round
   trip for every data and length kind.
2. **Every scalar field needs a value, and every value a field.** A missing value or a name that
   is no field of the request is an error, and so is a field without a name or with another
   field's name, since values are given by name. This is the rule `csv-tables.md` 5.3 applies when
   it selects a service, so a selected service always encodes.
3. **A value must fit as given; nothing is padded or cut.**
   - An integer must be within the field's width (unsigned, two's complement, or the number of
     BCD digits). Values are given as 64-bit signed integers, so a 64-bit unsigned field cannot
     be given a value of 2^63 or more.
   - A float field takes a float, or an integer the float holds exactly; a 32-bit field only a
     value a 32-bit float holds exactly.
   - A fixed-length bytes or string value must have exactly the field's length; a leading-length
     value must fit its length prefix.
   - An `Ascii` value takes characters up to U+00FF, one byte each, since the decoder reads each
     byte as that code point; a `Unicode2` value takes characters up to U+FFFF.
4. **A terminated value is written as the decoder will find it.** Its length must be within
   `min_bytes` and `max_bytes`. The terminator follows it unless the value fills `max_bytes`,
   where the decoder stops without one. A value that holds its terminator at or after `min_bytes`
   is refused, since the decoder would end it there. A value that runs to the end of the message
   (`EndOfPdu`) must be the last thing in the message, even when it fills its `max_bytes`.
5. **What the encoder does not cover is refused, not guessed.** A field with a nested layout, or
   with a length taken from another field, is `Unsupported`. A field that breaks a rule of
   `ir.fbs` (a width its type cannot have, an unknown kind) is `InvalidPlan`. A message longer than
   16 MiB is `TooLarge`, so a field placed far out cannot make the encoder allocate without bound.
6. **Fields do not overlap.** A field may not write a bit another field, the SID or the prefix
   has written (`InvalidPlan`); bit fields that share a byte without sharing bits are fine. Bytes
   no field covers are 0.

## Consequences

- The Starlark lowering can build a constant `diag.request` operand at ingestion with this
  function. A parameter known only at run time needs the same encoding done by instructions,
  which is a backlog item.
- Requests with a nested layout (an end-of-PDU repetition, ISO 22901-1 (2008) 7.3.6.10) or with a
  length from another field cannot be built yet.
- Strictness moves errors to ingestion: a definition that relied on padding a short fixed-length
  string has to give the whole value.
