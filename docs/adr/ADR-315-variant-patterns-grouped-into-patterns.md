# ADR-315: Variant Pattern Entries Grouped into Patterns

**Date:** 2026-10-16
**Status:** Accepted
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
3. **The first matching variant is selected.**
   - `diag_ir::decode::select_variant` tries the variants in the order given and returns the
     first that matches, or none. For a definition directory that order is `variants.csv`'s row
     order.
   - The standard makes patterns disjoint, so two matching variants mean an authoring error. The
     earlier variant still wins, as the standard's first-match rule says, and the function does
     not try to detect the overlap.
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
  out.
