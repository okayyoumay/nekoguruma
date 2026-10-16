# ADR-306: Comparing a Variant Pattern with a Decoded Field

**Date:** 2026-10-16
**Status:** Accepted
**Affects:** `diag-ir` (`src/decode.rs`), `diag-frontend` (`src/tables/resolve.rs`,
`src/convert/services.rs`), `crates/diag-frontend/docs/csv-tables.md` 4.2, design 8.9.1

## Context

Variant identification (design 8.9.1) runs identification services and selects the variant whose
responses match its patterns. The IR's `VariantPattern` names a service, a field of its positive
response and an `expected` byte string, which `ir.fbs` describes as a prefix match. That
description fits a field that decodes to bytes, and nothing else said how `expected` compares with
a field that decodes to an integer (possibly little-endian on the wire), to text in some encoding,
or to a float from a conversion. Nothing in the code compared them at all.

ODX (ISO 22901-1 (2008) 7.3.2.8) compares a matching parameter's expected value, written as a
string, with the physical value of the response parameter, and the parameter matches when the two
are equal. The IR keeps `expected` as bytes, so a rule is needed for each kind of decoded value.

## Decision

1. **The comparison is made on the decoded value.** `diag_ir::decode::pattern_matches` takes a
   pattern and its service's decoded positive response (`decode_positive_response`), finds the
   field `field_id` among the response's top-level fields, and compares `expected` with that
   field's value as decoded. The wire encoding (byte order, bit position, text encoding, BCD)
   therefore plays no part. A pattern whose field is missing does not match.
2. **Bytes and text match by prefix.** For a bytes value, `expected` must be a prefix of the bytes.
   For text, it must be a prefix of the text's UTF-8 encoding, whichever of ASCII, UTF-8 or UTF-16
   the field is sent in. ODX compares for equality; the prefix keeps `ir.fbs`'s existing rule and
   lets a pattern ignore padding or a version's trailing parts. A pattern that needs the whole
   value writes all of it, which matches equal values and longer ones that start with it.
3. **Integers match by number.** For an unsigned value (`Uint`, or `Bcd` as the number its digits
   spell), `expected` is read as a big-endian unsigned number of one to eight bytes and must equal
   the value. For a signed value it is read as a big-endian two's complement number of its own
   width, sign-extended, so `FE` and `FF FE` both mean -2. A little-endian field's `expected` is
   therefore written big-endian, as the number. An integer is never compared by prefix, since a
   prefix of a number's bytes is not a meaningful part of it. An empty `expected`, or one longer
   than eight bytes, does not match an integer.
4. **A text table's internal value is compared, not its text.** The decoder keeps a text table's
   internal value and names the text by id (ADR-297), so a pattern on such a field compares that
   number. Texts are resolved on the server (design 9.2) and may be translated, so they are not a
   stable thing to identify an ECU by.
5. **Floats are never compared.** A float field, or a field with a numeric conversion, decodes to a
   float, which a byte string cannot match exactly. `pattern_matches` treats it as no match, and
   the CSV tables reject such a pattern at ingestion (`CSV_BAD_VALUE`), together with an integer
   field's `expected` of more than eight bytes.
6. **The CSV writes a BCD field's `expected` as its digits.** In `variant_patterns.csv` a `bcd`
   field's `expected` is written as the field's digits, two per byte, most significant first
   (`12 34` for 1234), and each digit must be 0 to 9. The conversion writes the number those
   digits spell into the IR, as eight big-endian bytes, which item 3 then compares. Other integer
   fields' `expected` is the number, big-endian.

## Consequences

- An ODX pattern that compares a physical float (a scaled voltage, say) has no CSV equivalent; it
  has to be written on the raw field without a conversion, or on another field.
- A text pattern also matches longer values that start with it, which ODX would not. A definition
  that must tell `1.2` from `1.20` needs a field that ends where the value ends, such as a
  terminated one.
- How a variant's several patterns combine (all of them, or any of them) and which variant is
  selected when several match are not decided here; `pattern_matches` answers for one pattern.
