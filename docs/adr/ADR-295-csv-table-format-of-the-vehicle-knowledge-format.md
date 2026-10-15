# ADR-295: The CSV Table Format of the Proprietary Vehicle-Knowledge Format

**Date:** 2026-10-14
**Status:** Accepted (Decision item 11 amended by ADR-298)
**Affects:** `diag-frontend` (`docs/csv-tables.md`, `examples/example-ecu/`), design 8.4

## Context

The proprietary vehicle-knowledge format pairs CSV tables, converted directly into the IR's
declaration part, with Starlark procedures (design 8.4, ADR-254). The Starlark side has a
specification; the CSV side was only named. Its converter, the IR golden tests and the M2
end-to-end test all need the tables defined first.

The declaration part (`crates/diag-ir/schema/ir.fbs`) is shaped by ODX (ISO 22901-1 (2008)):
services with request and response layouts (7.3.5.3 to 7.3.5.6), fields with bit positions,
length kinds and nested layouts (7.3.6, ADR-242), conversions like its computational methods
(7.3.6.6), DTCs, protocols and ComParams, flash sessions and variant patterns.
The format has to reach all of it, be writable by hand or in a spreadsheet, convert
deterministically (the IR is content-addressed and signed, 8.2.6), and leave the differential test
against ODX (8.4) possible.

The Starlark side constrains the names: a procedure calls `diag.request(sid, params)` with a dict
of parameter names, and reads a response's `fields` by `Field.short_name` (subset spec 3.1, 3.4).
`ServiceRequest` in the IR carries only the SID (ADR-235 item 2), so the frontend has to find the
declared service from the SID and the parameter names when it lowers the call.

## Decision

1. **One directory per ECU, one fixed file name per table.** Nineteen tables cover the
   declaration part; only `variants.csv` and `protocols.csv` are required. Unknown `.csv` files
   are rejected, other files (the Starlark procedures) are not tables. Each variant becomes its
   own `EcuDocument` (8.2.6).
2. **Plain CSV (the record format of RFC 4180 section 2), columns by header name.** UTF-8 with a tolerated byte order mark, so
   spreadsheet exports work; columns in any order; optional columns may be left out of the
   header; every table accepts a `note` column. Unknown columns are rejected, so a misspelled
   column cannot silently fall back to a default. Integers may be hexadecimal (`0x`), bytes are
   hex digit pairs, lists are `;`-separated.
3. **Rows refer to each other by name; the converter assigns internal ids.** Field, layout and
   service ids are numbered from 1 in row order over the whole definition, so a row keeps its id in
   every variant's document and the output is the same for the same input. Ids that something
   outside the document uses are written explicitly: variant ids, text and unit ids (resolved on
   the server, 9.2) and flash session ids (taken by `diag.flash_transfer`).
4. **Variants by an optional `variants` column.** A scoped row with the column empty belongs to
   every variant; otherwise to the listed ones. Names are unique across the definition, so a
   variant that needs a different layout for the same DID uses a second row with another name.
   This replaces ODX's value inheritance between diagnostic layers (ISO 22901-1 (2008) 7.3.2.4)
   with one flat rule.
5. **One field table for every layout.** `fields.csv` holds the fields of every request,
   response, DID record, routine record and nested layout, each row naming its container. Row
   order is decode order. Positions count from the container's own data (the byte after the SID,
   the DID or the routine identifier), so an author never counts the header bytes; the converter
   adds them. Repetition counts and selection cases, which need columns no other field uses, are
   in `repeats.csv` and `select_cases.csv`.
6. **DIDs and routines generate their services.** A `dids.csv` row generates a read service
   (`<name>.read`) and optionally a write service with one record definition; a `routines.csv`
   row generates one service per supported sub-function. This mirrors the usual ODX modelling,
   where each DID or routine sub-function is its own diagnostic service (ISO 22901-1 (2008)
   7.3.5.3) whose request carries the identifier as a coded constant parameter (7.3.5.4), so
   the differential test against ODX compares like with like. Other services are written in
   `services.csv`.
7. **Leading constants become the IR prefix and identify the service for Starlark.** A field
   with `const` must be one of the first fields of its container, byte-aligned and fixed. The
   converter moves constants into `Request.prefix` and `PositiveResponse.prefix`, which is where
   the IR expects a sub-function or an echoed identifier. The DID and routine services get
   generated constants named `did`, `sub` and `rid`, which their fields may not reuse. Request
   parameters are encoded from internal values, and conversions apply only to decoded values. In
   the frontend the constants keep their names: `diag.request(sid, params)`
   selects the service of that SID whose every constant appears in `params` with its value and
   whose other request fields are exactly the remaining keys, and exactly one service must match. The
   generated DID services carry a constant named `did`, so `diag.request(0x22, {"did": 0xF190})`
   reads the VIN, as the subset specification's examples already write it. A `params` value for a
   constant's key must be a constant, since the selection happens at ingestion. Two services of one
   variant with the same SID are rejected when one call could match both: when their request
   fields have the same names and no name is a constant in both with different values.
8. **Conversions are named and shared.** `conversions.csv` names a conversion and its kind;
   kinds with arrays (scale-linear, text table, interpolation) take one `conversion_scales.csv`
   row per entry, in IR array order, which keeps a text table's first-match order visible.
   `CompuBytecode` has no table: code belongs to the procedure part.
9. **A repetition marker is an unsigned big-endian integer.** `repeats.csv` gives its width and
   value range; the IR compares byte and string markers of up to 8 bytes as exactly such an
   integer (`ir.fbs`, `Repeat`), so one form covers them all.
10. **DTC status, snapshot and extended data are layouts.** A DTC row names the layouts whose
    fields the converter copies into its definition. The status byte's bits are therefore data
    in the definition, not a list built into the converter.
11. **Every error is listed, with its file and, where they exist, its line and column.** As for
    Starlark (subset spec 5), the conversion reports all errors at once, with codes `CSV_*`; an
    error about a whole file or its header leaves out the location it does not have. It checks
    every rule `ir.fbs` states, so nothing reaches the agent that the IR forbids (8.3).
12. **Left out of this version:** monitor sets (the converter emits none), `CompuBytecode`,
    29-bit CAN identifiers (as in the agent's link, ADR-235 item 4),
    constants that are not leading or sit in a nested layout, request fields
    without a single encoding of a procedure's value (shapes, lengths from another field, `zero`
    and `ff` terminators), a `ref` to a field outside the
    row's own container (a layout reached from several places could not name one field), and a table for a procedure's restart declaration, which
    belongs to the procedure part (ADR-245) and gets its syntax with the Starlark subset.

The tables, their columns and the conversion rules are specified in
`crates/diag-frontend/docs/csv-tables.md`; `crates/diag-frontend/examples/example-ecu/` is an
example definition of one ECU that uses every table.

## Consequences

- The converter has a complete specification and an example to test against. The example's
  DIDs, DTC read and routines are those `sim-ecu` answers, apart from two DIDs `sim-ecu` does not
  answer: a supply voltage with a linear conversion and a last reset reason with a keyed
  selection.
- A procedure cannot read a constant (`res["fields"]["sub"]` is rejected at ingestion):
  constants are part of the prefix and not IR fields. The host checks only the response SID and, for some services, the
  echoed identifier (ADR-235 item 3), so comparing the rest of a response's prefix falls to the
  declaration-part decoder.
- Because names are unique across the definition rather than per variant, a DID whose layout
  differs between variants appears under two row names. A procedure is not affected by that: it
  selects the DID by number and reads fields by their names in the record, which the two rows may
  share. The alternative, per-variant overrides of one name, would make a table row's meaning
  depend on which variant's document is being read.
- A procedure is lowered against one variant's document, so one that reads a DID scoped to some
  variants only (the example's supply voltage) fails to lower for the others, where the service
  does not exist.
- Generating a service per DID assumes the ODX frontend also turns a statically known DID key
  into a service of its own, so that the differential test (8.4) compares like with like; the
  ODX frontend has to confirm that.
- Service lookup by SID and constants is the frontend's rule, not the IR's: the IR still holds
  only the SID in `ServiceRequest` and the bytes in the prefix. An ODX frontend that lowers OTX
  calls needs a rule of its own.
- Monitoring (design 10.2) needs a monitor-set table in a later version of the format.
- The format has no inheritance between ECUs: shared definitions (the same DTC status layout in
  every ECU) are copied per directory. Content addressing (8.2.6) deduplicates identical output,
  not identical input.
