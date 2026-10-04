# vci.service Proto Rules

This document summarizes the naming and field-definition conventions used by `vci-service-interface/src/proto/service.proto`.

Related maintenance note: [implementation-notes.md](implementation-notes.md)

## Naming

- Use `PascalCase` for messages, enums, enum variants, and RPC names.
- Use `snake_case` for proto field names.
- Prefer full descriptive names over abbreviations.
- Keep paired request/response types symmetric where possible, for example `GetXRequest` and `XResponse`.
- Use suffixes such as `Handle`, `Item`, `Data`, `List`, `Request`, and `Response` consistently for shape clarity.

## Field Definition Policy

- Number fields starting at `1` and keep tags stable once published.
- Do not reuse tag numbers.
- If a tag is intentionally skipped, document the reason or reserve it explicitly.
- Prefer grouped, sequential numbering inside each message.
- Keep related fields close together in declaration order, even when oneof tags are interleaved.

## Presence and Shape

- Use `oneof` when exactly one representation is valid.
- Use `optional` for nullable single fields where presence matters.
- Use plain singular fields for always-present scalar values.
- Use message wrappers for structured payloads instead of flattening nested data into request or response messages.

## Handle Representation

- Handle-bearing request messages use dedicated handle wrapper messages such as `SystemHandle`, `ModuleHandle`, `ComLogicalLinkHandle`, and `ComPrimitiveHandle`.
- Use `oneof handle` when a request may accept more than one handle kind.
- Keep handle wrapper shapes minimal and stable.

## Data Collection Shapes

- Use `Item` messages for container-style records with repeated entries.
- Use `Data` messages for individual element payloads.
- Use `List` messages for repeated structured values.
- Keep field names descriptive and aligned with the semantic object being represented, such as `module_data`, `resource_id_array`, and `unique_data`.

## Flag and Bitmask Representation

- Represent bitmask-style values with both named-bit messages and raw byte payloads when the protocol needs both forms.
- Use a `oneof` for mutually exclusive named-bit vs raw-byte representations.
- Keep bit-position enums separate from payload messages.

## Comments and Documentation

- Keep comments short and descriptive.
- Use comments to explain protocol meaning, conversion rules, or representation constraints.
- Avoid comments that restate the field name without adding useful context.

## Rich Error Detail Messages

- A message intended to be attached to a failing RPC's `Status` via the rich
  error model (rather than appearing as a field in any successful `Response`
  message) is still named and shaped per the rules above (e.g. `ErrorDetail`,
  `ErrorEventData`), but is encoded/decoded through
  `status_with_error_detail`/`error_detail_from_status` in `src/rich_error.rs`,
  not through a normal RPC request/response pair. See ADR-105.
- Not every RPC failure needs to carry one of these messages: attach it only
  where the failure corresponds to something the underlying protocol
  actually defines a return code for, not to pure request-shape/transport
  validation failures.

## Existing Exceptions

- A few fields may use reserved or skipped tag numbers for compatibility reasons.
- Some message shapes use `optional` even when a plain nested message could be used; that is acceptable when presence is part of the API contract.
