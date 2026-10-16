# ADR-313: Selecting a Request's Service from an Index of the Tables

**Date:** 2026-10-16
**Status:** Accepted
**Affects:** `diag-frontend` (`src/convert.rs`, `src/convert/services.rs`, `src/star.rs`,
`src/star/lower.rs`), `crates/diag-frontend/docs/csv-tables.md` 5.3,
`crates/diag-frontend/docs/starlark-subset.md` 3.1, 3.2 and 3.4; ADR-311 item 4

## Context

A `diag.request(sid, params)` call means the one service of the variant whose SID matches,
whose request constants all appear in `params` with equal values, and whose other request
fields are exactly the remaining keys (`csv-tables.md` 5.3). ADR-311 item 4 has the lowering
read the variant's declarations as the IR, so that what it selects is what the agent holds.
The IR, however, keeps a request's constants only as the bytes of `Request.prefix`: their
names, widths and boundaries are gone, so the rule cannot be applied to the IR alone. Three
ways were open: add the constants' names to the IR schema, pass an index of the tables beside
the IR, or have the lowering read the CSV rows directly.

## Decision

1. **An index beside the IR.** For each variant the conversion builds a `RequestIndex` from
   the tables: for every service of the variant its IR `Service.id`, its SID, whether it is a
   generated routine service, and its request constants by name and value (a DID service's
   generated `did`, then the request container's declared constants; a `uint` constant by its
   integer, a `bytes` constant by its bytes). `star::lower` takes it with the variant's
   `EcuVariant`. The IR schema does not change.
2. **Selection by the index, fields and encoding from the IR.** The lowering applies the 5.3
   rule with the index for the SID and the constants, and the IR request named by the indexed
   service id for the other fields (their `Field.short_name`s). It encodes the operand from
   that IR request with `diag_ir::encode::encode_request` (ADR-308), so the bytes sent are
   those the agent's declarations describe. Routine services are never selected.
3. **No match, or several, is one error.** A call that selects no service or more than one is
   rejected with the new code `STAR_UNKNOWN_SERVICE`, whose hint names the SID and the keys.
   Several matches cannot arise from accepted tables (5.3 rejects such pairs), but the lowering
   does not rely on that. A call some variants cannot serve is reported once, naming those
   variants (ADR-311 item 3).
4. **Arguments at ingestion.** The SID must be a constant from 0x00 to 0xFF and `params` a
   dict known at ingestion. A value known only at run time under a key some service with that
   SID holds as a constant is `STAR_NON_CONSTANT_ARGUMENT`; one under any other key is not
   lowered yet (`STAR_NOT_LOWERABLE`), since the request is encoded at ingestion. A value the
   field cannot hold is `STAR_ARGUMENT_OUT_OF_RANGE`.
5. **A statement until the response has a shape.** The call lowers to the operand pushed from
   the constant pool, `ServiceRequest` with the SID, and a `Pop` of the response; using the
   call's value is not lowered until the response has a run-time shape (subset 3.4).

ADR-311 item 4 is amended: the lowering reads the variant's declarations as the IR, and in
addition the index the tables give for that variant, which carries only what the IR does not.

## Consequences

- The IR's `Request.prefix` and the index describe the same constants twice; both come from
  the same rows in one conversion, so they cannot disagree within a document, and the index is
  never stored.
- A later frontend for another source format must build the same index to lower
  `diag.request`.
- Run-time field values need an encoding that runs in the VM (or one built from the existing
  byte instructions) before they can be lowered.
