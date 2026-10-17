# ADR-307: Packaging Starlark Procedures into Documents

**Date:** 2026-10-16
**Status:** Accepted (Decision item 2's single lowering amended by ADR-311; item 1's one document per variant and procedure and item 5's digest amended by ADR-325)
**Affects:** `diag-frontend` (`src/convert.rs`, `src/tables/mod.rs`, `src/star.rs`),
`crates/diag-frontend/docs/csv-tables.md` sections 1 and 6, `starlark-subset.md` 2.1, ADR-295 item 1, design
8.2.6

## Context

A definition directory holds the CSV tables of one ECU and any number of Starlark procedure files
(`*.star`, one procedure per file, subset 2.1). The table conversion (ADR-295) produced one
`EcuDocument` per variant, with no procedure. `EcuDocument` holds exactly one `procedure`, the
lowered program, and `csv-tables.md` section 6 already defined `source_digest` over the tables and
"the procedure file the document carries". Nothing said which documents carry which procedure, so
`star::lower` (ADR-304) had no way into a document.

Design 8.2.6 distributes one file per ECU variant, identified by its hash, and the agent fetches
the documents of the target variant. A job runs one procedure against one ECU, whose variant is
identified at run time (design 8.9.1, ADR-306).

## Decision

1. **One document per variant and procedure.** When the directory has procedure files, the
   conversion produces one document for each pair of a variant (`variants.csv` row) and a
   procedure file. Each carries the variant's whole declaration part and the one procedure. The
   documents come in `variants.csv` row order, and within a variant in byte order of the
   procedure file names. A directory without procedure files keeps one document per variant,
   with no procedure, as before.
2. **Every procedure goes with every variant.** A procedure is not scoped to some variants. It
   is checked and lowered once, and its program is attached to each variant's document. When the
   lowering comes to depend on a variant's declarations (`diag.request` resolving a service), a
   procedure must lower for each variant of the directory; limiting a procedure to some variants
   is left to a later change.
3. **The procedure is named by its file name.** The converter reports which procedure a document
   carries by the file name, `.star` included (`VariantDocument.procedure`). The files are read
   as stored; a file past the size bound reports that first, and one within it that is not UTF-8
   is rejected with `STAR_SYNTAX_ERROR` at its first invalid byte (subset 2.1, 2.8). Every
   file's errors are listed together, with the warnings of the files that lowered (subset
   section 5).
4. **The program is stored as postcard bytes.** `EcuDocument.procedure` holds the postcard
   encoding of the lowered `diag_ir::Program`, the encoding `ir.fbs` names for that field.
5. **Each document's digest covers its own procedure only.** `source_digest` covers the table
   files and the one procedure file the document carries (section 6), so editing one procedure
   changes only the documents that carry it, and the others keep their hash (design 8.2.6).

## Consequences

- The declaration part is repeated in every procedure's document of a variant. The documents of
  one variant differ only in their procedure and digest, so a directory with many procedures
  stores and distributes its declarations many times; storing procedures apart from the
  declaration part would need a schema change.
- A job names a variant and a procedure, which together pick one document.
- Nothing caps the number of variants or procedure files, so a directory produces as many
  documents as the product of the two, each with the whole declaration part. The digest is taken
  once per procedure, but the documents are all held in memory. A cap on what one directory may
  produce is a backlog item.
- Scoping a procedure to some variants, and sharing code between procedures (`load`, which the
  subset rejects), are open; both are backlog items.
