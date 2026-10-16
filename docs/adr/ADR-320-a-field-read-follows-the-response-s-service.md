# ADR-320: A Field Read Follows the Response's Service

**Date:** 2026-10-16
**Status:** Accepted
**Affects:** `diag-frontend` (`src/star/lower.rs`: `Tys`, `Ty::Fields`, `field_key`;
`src/convert/tests.rs`), `diag-ir` (`decode::scalar_kind`),
`crates/diag-frontend/docs/starlark-subset.md` 3.4, design 8.4; ADR-316, ADR-317

## Context

A procedure reads a decoded response value as `res["fields"]["name"]` (subset 3.4). ADR-316
gives the VM the instruction, `Op::DecodeField { service, field }`, which names the declared
service and the field by id. ADR-317 keeps a response as its bytes at run time, typed as a
response by the lowering. The bytes say nothing about which declared service they answer, and
a name means a field only within one service's positive response. So the lowering has to know,
at the read, which `diag.request` call the response came from. It also has to give the value a
type at ingestion, as for every run-time value (subset 2.5, ADR-304).

## Decision

1. **The response type carries its service.** The lowering's inferred types carry, beside
   the type bits, the `Service.id` of the request a response comes from:
   - one known service;
   - several, when paths that join carry responses of different services;
   - unknown, when the call was already reported as an error.

   A value assigned, passed or returned keeps it, as it keeps its type. A union of two
   different services is "several".
2. **`fields` is a value of its own.** `res["fields"]` is the same bytes, typed as the
   response's fields with the same service, so it can be named (`f = res["fields"]`) and read
   later. Its only use is a literal key. Any other use is `STAR_NOT_LOWERABLE`, as for a
   response.
3. **A read decodes one declared field.**
   - The key must be a string literal.
   - It must name a top-level field of the service's positive response, matched by the
     field's short name. Another name is `STAR_UNSUPPORTED_SYNTAX`.
   - The read lowers to the response bytes and `DecodeField` with the service's and the
     field's ids.
   - A response whose service is "several" cannot be read (`STAR_NOT_LOWERABLE`): the same
     name could mean different fields.
4. **The declaration gives the type.** `diag_ir::decode::scalar_kind` decides, from the
   field's data kind and conversion alone, what the decoder gives:
   - an integer for unsigned, signed and BCD numbers, also under a text table;
   - a float for a float field or a numeric conversion;
   - a string for a text field;
   - bytes for a bytes field.

   The lowering types the value that way. A field it cannot type is `STAR_NOT_LOWERABLE`:
   - a nested layout;
   - a bytecode conversion, which the decoder does not run;
   - a numeric conversion of bytes or text.

## Consequences

- The type mapping lives beside the decoder, so a change to what the decoder gives also has
  to change `scalar_kind`. The decode tests check that the two agree.
- Reading a field of a negative response stops the run with `VmError::Undecodable`
  (ADR-316). A procedure tests `res["ok"]` first. The lowering does not force that.
- A procedure that reads the same response through two services' paths has to read the
  fields inside each path.
- Fields inside nested layouts, and a text table's text rather than its number, cannot be
  read yet.
