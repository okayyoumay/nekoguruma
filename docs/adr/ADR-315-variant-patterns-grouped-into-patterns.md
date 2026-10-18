# ADR-315: Variant Pattern Entries Grouped into Patterns

**Date:** 2026-10-16
**Status:** Accepted (item 3's first matching variant: several matching variants reported to the operator in identification jobs, and a job that changes an ECU refused unless exactly the identified variant on its guard list matches, by ADR-327)
**Affects:** `diag-ir` (`schema/ir.fbs`, `src/decode.rs`, `IR_SCHEMA_VERSION` 10),
`diag-frontend` (`src/tables/rows.rs`, `src/convert/services.rs`, the IR golden files),
`crates/diag-frontend/docs/csv-tables.md` 4.2, design 8.9.1; ADR-306

## Context

ADR-306 defines when one `VariantPattern` entry matches a decoded identification response, but
not how a variant's entries combine, nor which variant is selected when several match.
`EcuVariant.patterns` is one flat list per variant, and each variant is its own document.

ISO 22901-1 (2008) 7.4.3 groups matching parameters into ECU variant patterns. A pattern matches
when all of its parameters match, and a variant is detected when at least one of its patterns
matches. Identification stops at the first variant that matches. Several patterns per variant are
needed when ECU samples with different identifications behave alike and are described as one
variant. A flat list can express only one of the two operators: reading it as one pattern loses
the alternatives, and reading every entry as its own pattern loses the conjunction. The ODX
frontend of M7 has to carry ODX's grouping into the IR.

## Decision

1. **Entries carry a pattern number.** `VariantPattern` gains `pattern: uint16`, appended to the
   table. A variant's entries with the same number form one pattern. The numbers only group
   entries: their values and order carry no meaning. `IR_SCHEMA_VERSION` becomes 10, since a
   version 9 runtime would read every entry as belonging to one pattern.
2. **The ODX rule.**
   - A pattern matches when every one of its entries matches as ADR-306 defines.
   - A variant matches when any of its patterns matches.
   - A variant without entries is never identified.
   - An entry whose identification service gave no decoded positive response does not match.

   `diag_ir::decode::variant_matches` implements this.
3. **The first matching variant by name is selected.**
   - `diag_ir::decode::select_variant` tries the variants in rising order of their
     `short_name`, compared byte by byte, and returns the first that matches, or none. This is
     the order 7.4.3 lays down so that every implementation picks the same variant. It does not
     depend on the order the caller gives, which the per-variant documents (8.2.6) do not record
     anyway.
   - The standard makes patterns disjoint, so two matching variants mean an authoring error. The
     variant first by name still wins, and the function does not try to detect the overlap.
   - Each variant's document numbers its own services, so the function asks for a response by
     variant and service.
4. **The CSV format writes the number as an optional column.** `variant_patterns.csv` gains an
   optional `pattern` column: an integer from 1 to 65535 that defaults to 1. Existing tables
   therefore keep meaning one pattern per variant.

## Consequences

- Programs and documents of version 9 are refused by a version 10 runtime (ADR-296 item 5). The
  agent and the frontend are released together, as for every earlier version change.
- Running the identification services and decoding their responses, which the agent's pre-check
  (8.9.1) does before a job starts, is not part of this decision. The functions take the decoded
  responses as given.
- A definition with overlapping patterns gets a deterministic but possibly unintended variant.
  Checking patterns for overlap at ingestion would need their value ranges compared and is left
  out. Prefix matching (ADR-306) makes overlaps easier to write than ODX's equality: a pattern
  expecting the text "1.2" also matches "1.20". The variant first by name then wins, so a
  definition that has to tell such values apart reads them from a fixed-length field padded
  after its value, whose padding the pattern then includes (ADR-306).
- Two parts of 7.4.3 are not supported:
  - A matching parameter inside a repeated structure, which ODX matches when any element does.
    `pattern_matches` never matches a shaped value, and the CSV format rejects such fields.
  - A matching parameter resolved in a negative response. The IR's negative responses have no
    fields, so an entry matches only a decoded positive response.
