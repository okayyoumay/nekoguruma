# ADR-311: Lowering Each Procedure Once per Variant

**Date:** 2026-10-16
**Status:** Accepted (Decision item 4 amended by ADR-313; item 1's per-variant lowering extended to combinations of target variants by ADR-325)
**Affects:** `diag-frontend` (`src/convert.rs`, `src/star/lower.rs`), `fuzz/seeds`,
`crates/diag-frontend/docs/csv-tables.md` section 1, `starlark-subset.md` section 5, ADR-307
item 2

## Context

ADR-307 packages every procedure file of a definition directory with every variant, and item 2
checks and lowers each file once, attaching the one program to every variant's document. It
foresaw that this changes once the lowering depends on a variant's declarations. That is now
needed: a `diag.request` call selects the declared service of the variant it runs on by the
call's constants (`csv-tables.md` 5.3) and encodes its operand from that service's request
(ADR-308), and two variants may declare the same service differently. The lowering therefore
needs the variant's declaration part, and a procedure's program may differ between variants.

## Decision

1. **Checked once, lowered per variant.** A procedure file is checked once, since the subset
   checks do not depend on any variant. A file that passes is lowered once for each variant,
   in `variants.csv` row order, against that variant's declaration part as the conversion
   produces it (the document without its procedure, read back through the loader the agent
   uses). Each variant's document carries the program lowered for that variant; the digests
   are unchanged, since they cover the files, not the programs (ADR-307 item 5).
2. **A procedure must lower for every variant.** If the lowering fails for any variant, the
   file is rejected, as ADR-307 item 2 already required. Scoping a procedure to some variants
   remains open.
3. **Each error once.** The errors and warnings of the per-variant lowerings are merged: each
   distinct one is reported once, in source order. One that only some variants give ends its
   hint with the names of those variants (`(in variant V1)`, `(in variants V1, V2)`), so an
   author sees which declarations it depends on without reading the same error once per
   variant. One that every variant gives carries no names.
4. **The lowering sees the declarations, not the tables.** `star::lower` takes the variant's
   declaration part as the IR (`EcuVariant`), the form the encoder and decoder read, rather
   than the CSV rows, so what the lowering selects is what the agent will hold.

## Consequences

- Lowering a directory costs the number of procedure files times the number of variants.
  Nothing caps either yet; a cap on what one directory may produce (ADR-307's consequences)
  would bound this too.
- The procedures can only be lowered once the tables resolve: `lower_procedures` takes the
  resolved definition, so a directory whose tables are rejected reports no Starlark errors
  until its tables are fixed.
- Until a lowering reads the declarations, every variant gets the same program; the merge of
  item 3 is exercised by its own test.
