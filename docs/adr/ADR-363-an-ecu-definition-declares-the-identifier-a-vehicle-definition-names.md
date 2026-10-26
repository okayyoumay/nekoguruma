# ADR-363: An ECU Definition Declares the Identifier a Vehicle Definition Names

**Date:** 2026-10-26
**Status:** Accepted
**Affects:** `diag-ir` (`schema/ir.fbs` `EcuDocument.definition_id`, `IR_SCHEMA_VERSION` 20),
`diag-frontend` (`definition.csv`, `tables::rows::DefinitionRow`, `convert::to_ir`),
`crates/diag-frontend/docs/csv-tables.md` 4.15, the example ECU; design 8.8.2; ADR-326 (its
Consequences), ADR-307

## Context

A vehicle definition refers to ECU definitions by their identifiers (ADR-326 item 1), both for
the alternatives of its ECU entries and for the VIN sources and field rules its identification
uses (ADR-327 items 4 and 5). Nothing defined such an identifier:

- one CSV directory converts to one document per variant and procedure (ADR-307);
- a document carried its variant, its procedure and its source digest, but nothing that names
  the definition it came from.

The server and the vehicle definition's converter see signed documents, not directories, so a
directory's name cannot serve: renaming a directory would break every reference to it, and the
name does not survive into what the server ingests. A variant's name cannot serve either, since
one definition has several variants. ODX names diagnostic layers by their short names, and its
vehicle information refers to them that way.

The maintainer decided the form below. It contradicts ADR-326's Consequence that the IR schema
does not change.

## Decision

1. **The identifier is declared, optionally.** A CSV ECU definition directory may hold
   `definition.csv`, with one row and an `id` column holding a name. A directory without it
   declares none and converts as before. A second row is refused (`CSV_DUPLICATE`), and so is a
   value that is not a name.
2. **Every document carries it.** `EcuDocument` gains `definition_id`, a string, absent when the
   definition declares none. Every document a directory converts to carries the same value. The
   IR schema version becomes 20.
3. **Uniqueness is checked where definitions meet.** Two ECU definitions with one identifier are
   a conflict only for a vehicle definition converted with both, so the vehicle definition's
   conversion checks it (its own backlog item), not the ECU definition's.
4. **The agent ignores it.** A job's documents still run as they did. The identifier is for the
   vehicle definition's conversion and for the server.

## Consequences

- ADR-326's Consequence that "the IR schema does not change" no longer holds for the document
  header. The topology and identification still add nothing the agent runs.
- Every document converted after this change carries schema version 20, and a document of
  version 19 is refused by the existing version check. Documents are converted from their
  sources again, as for every schema change.
- An ECU definition that declares no identifier cannot be named by a vehicle definition. That
  is the author's choice, and the vehicle definition's conversion reports it as an unknown
  identifier.
