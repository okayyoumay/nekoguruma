# IR schema notes

`ir.fbs` defines the IR declaration part. The procedure part (bytecode) is held by `Program` in `src/lib.rs`
and stored in `EcuDocument.procedure` in postcard-serialized form.

## Splitting and identification

- Files are **per ECU variant**. An entire vehicle model is never distributed as a whole (8.2.6)
- Each file is identified by hash, so definitions shared across vehicle models are naturally deduplicated
- A manifest (distinct from the extension manifest) holds each file's hash, and the signature is attached to the manifest

## Design decisions

**Pre-expand decode plans**
`Field` holds bit offset, width, endianness and conversion method flatly. ODX layouts are resolved
at import, and monitor sets are always flat, so 10ms-interval monitoring only needs to walk the
array. Other requests and responses can also refer to nested layouts, which the general decoder
follows (next section).

**Nest only what depends on the message (ADR-242)**
Field lengths follow ODX's coded-type kinds (fixed, from another field, leading length prefix,
terminated) and stay attributes of a flat field, which may be placed after the previous one
instead of at a fixed offset. Repetitions, keyed selections (ODX FIELDs, MUX, TABLE-KEY with
TABLE-STRUCT, which is how DID tables and nested DIDs appear) and structures placed or sized at
runtime refer to a `Layout` in `EcuVariant.layouts`. Static STRUCTUREs are still inlined, and
monitor sets stay fully flat.

**Separate conversion formulas by kind**
ODX COMPU-METHOD has a limited set of kinds, so there is no general-purpose expression engine.
Only procedural conversions (COMPUCODE) use an instruction subset of the procedure part VM, as `CompuBytecode`.

**Hold message text and units as IDs**
`message_id` and `unit_id` are resolved on the extension side (message text and unit systems in 9.2). This keeps the IR focused on
vehicle model definitions and separates display concerns. Localization does not require rebuilding the IR.

**Keep COMPARAM standard-neutral**
`ComParam` holds only values; the mapping to `PassThruConnect` flags and `PassThruIoctl` lives
on the worker side (8.5). Differences between the D-PDU API and J2534 are kept out of the IR.

**Restart declaration in the procedure part**
`FlashSession` holds only the flash description (steps and segments). The preconditions (voltage,
external supply, ignition, engine, vehicle speed) with their sources and satisfying values, the
identity sources, and each flash session's recovery plan (boundaries in the bytecode, the point
from which an interruption requires on-site intervention, timing, retry and resume limits) are
procedure-part data in `Program` (`src/recovery.rs`), checked by `Program::validate` when a
program is loaded (8.9, 8.10.1; ADR-245). A plan refers to its `FlashSession` by id.

## Generation

```
flatc --rust --gen-object-api -o src/generated schema/ir.fbs
```

Run it in `crates/diag-ir`. The output, `src/generated/ir_generated.rs`, is committed to the
repository (so the build environment does not require flatc) and is never edited by hand: change
`ir.fbs` and regenerate in the same commit. Use the `flatc` release whose version equals the
`flatbuffers` crate's version in the workspace `Cargo.toml` (25.12.19), since the generated code
calls that runtime. `rustfmt` skips the file (`#[rustfmt::skip]` on its module in
`src/declaration.rs`).

`declaration::load_ecu_document` is the only way a buffer is read: it checks the `DIR1` file
identifier, runs the FlatBuffers verifier with limits that grow with the buffer (so a small
buffer whose offsets share data cannot unpack into a huge tree), accepts only
`EcuDocument.schema_version == IR_SCHEMA_VERSION`, and then rejects what the verifier lets
through: enum values and union tags the schema does not define, a document without its variant,
source format or source digest, and layouts whose ids do not strictly increase. When the schema changes, bump
`IR_SCHEMA_VERSION` and have the agent report its supported version via `capabilities` (9.5).

## Decoding

`decode::decode_positive_response` decodes a response through its `PositiveResponse`: it checks
the SID and the prefix, extracts each flat field as ISO 22901-1 (2008) 7.3.6.3 describes, and
applies its conversion (ADR-297). A `LinearScale` range includes both its limits, and the first
scale whose range holds the value applies. `decode::decode_dtc_fields` decodes one of a
`DtcDefinition`'s field lists (status, snapshot or extended) the same way, with the list's
positions counted from a byte its caller states, since where a list starts depends on the service
that returned the DTC. Both take the variant's `layouts`, since a field with a shape follows its
nested layout there (ADR-242): it decodes to a `Group` of the layout's fields, a `Repeat` of each
item's fields or a `Select` naming the chosen case and its fields (ADR-302). A nested layout's
positions count from its first byte; a length, count or key refers to the nearest earlier
occurrence of its field, in the same item when it is repeated; and decoding fails past 16 nested
layouts or 65,536 decoded values. A `CompuBytecode` conversion is rejected as unsupported for now.
