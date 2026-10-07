# IR schema notes

`ir.fbs` defines the IR declaration part. The procedure part (bytecode) is held by `Program` in `src/lib.rs`
and stored in `EcuDocument.procedure` in postcard-serialized form.

## Splitting and identification

- Files are **per ECU variant**. An entire vehicle model is never distributed as a whole (8.2.6)
- Each file is identified by hash, so definitions shared across vehicle models are naturally deduplicated
- A manifest (distinct from the extension manifest) holds each file's hash, and the signature is attached to the manifest

## Design decisions

**Pre-expand decode plans**
`Field` holds bit offset, width, endianness and conversion method flatly. Because the layout is not
interpreted at runtime, even 10ms-interval monitoring only needs to walk the array.

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

**Hold flash preconditions as data**
Voltage range, ignition and vehicle speed conditions are declared in `FlashSession`. The runtime
verifies them before execution and does not execute if they are not met (8.9).

**recovery_required_from_step**
Declares from which step onward "an interruption requires on-site intervention". The default is from the start of erase onward.
This is the source for the section attributes (8.10.1).

## Generation

```
flatc --rust --gen-object-api -o src/generated schema/ir.fbs
```

Generated code is placed in `src/generated` and committed to the repository (so the build environment
does not require flatc). When the schema changes, bump `IR_SCHEMA_VERSION` and
have the agent report its supported version via `capabilities` (9.5). Until the declaration part
is first generated, nothing encodes it, so the schema changes without a version bump.
