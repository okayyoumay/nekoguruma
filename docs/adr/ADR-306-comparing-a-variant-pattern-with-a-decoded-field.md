# ADR-306: Comparing a Variant Pattern with a Decoded Field

**Date:** 2026-10-16
**Status:** Accepted (how entries combine and which variant wins decided by ADR-315; the comparison also judges a field rule, through `expected_matches`, by ADR-403)
**Affects:** `diag-ir` (`src/decode.rs`), `diag-frontend` (`src/tables/resolve.rs`,
`src/convert/services.rs`), `crates/diag-frontend/docs/csv-tables.md` 4.2, design 8.9.1

## Context

Variant identification (design 8.9.1) runs identification services and selects the variant whose
responses match its patterns. The IR's `VariantPattern` names a service, a field of its positive
response and an `expected` byte string, which `ir.fbs` describes as a prefix match. That
description fits a field that decodes to bytes, and nothing else said how `expected` compares with
a field that decodes to an integer (possibly little-endian on the wire), to text in some encoding,
or to a float from a conversion. Nothing in the code compared them at all.

ODX (ISO 22901-1 (2008) 7.4.3, with the model in 7.3.2.7) compares a matching parameter's
expected value, written as a string, with the physical value of the response parameter, and the
parameter matches when the two are equal. The IR keeps `expected` as bytes, so a rule is needed for each kind of decoded value.

## Decision

1. **The comparison is made on the decoded value.** `diag_ir::decode::pattern_matches` takes a
   pattern and its service's decoded positive response (`decode_positive_response`), finds the
   field `field_id` among the response's top-level fields, and compares `expected` with that
   field's value as decoded. The wire encoding (byte order, bit position, text encoding, BCD)
   therefore plays no part. A pattern whose field is missing does not match, and neither does
   an absent or empty `expected`, which would otherwise match every bytes or text value.
2. **Bytes and text match by prefix.** For a bytes value, `expected` must be a prefix of the bytes.
   For text, it must be a prefix of the text's UTF-8 encoding, whichever of ASCII, UTF-8 or UTF-16
   the field is sent in; the decoder reads an ASCII byte of 0x80 or above as that code point, so
   its UTF-8 encoding takes two bytes. ODX compares for equality; the prefix keeps `ir.fbs`'s existing rule and
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
   the CSV tables reject such a pattern at ingestion (`CSV_BAD_VALUE`). They also reject, as
   patterns that could never match: an integer field's `expected` of more than eight bytes, a
   value outside what a fixed-width integer or BCD field can hold (leading zero bytes or digits
   are allowed), a fixed-length bytes field shorter than `expected`, and a text field's
   `expected` that is not UTF-8.
6. **The CSV writes a BCD field's `expected` as its digits.** In `variant_patterns.csv` a `bcd`
   field's `expected` is written as the field's digits, two per byte, most significant first
   (`12 34` for 1234), and each digit must be 0 to 9. The converter writes the number those
   digits spell into the IR, as eight big-endian bytes, which item 3 then compares. Other integer
   fields' `expected` is the number, big-endian.

## Consequences

- An ODX pattern that compares a physical float (a scaled voltage, say) has no CSV equivalent; it
  has to be written on the raw field without a conversion, or on another field.
- A text or bytes pattern also matches longer values that start with it, which ODX would not. A
  terminated field does not help: the decoder drops the terminator, so `1.2` matches both
  `1.2` and `1.20`. Only a fixed-length field padded after its value can tell them apart, with
  the padding written into `expected` (`31 2E 32 00`). Otherwise telling such variants apart is
  left to how a variant is selected when several match.
- How a variant's several patterns combine (all of them, or any of them) and which variant is
  selected when several match are not decided here; `pattern_matches` answers for one pattern.
