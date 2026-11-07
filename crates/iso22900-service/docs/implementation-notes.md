# iso22900-service Implementation Note

## Scope

Hosts gRPC API operations and manages lifecycle through stdio JSON-RPC and local server-state shutdown primitives.

## Assumptions

- Startup argument contract is iso22900:<library name>?mode=<device|user>&port=<u16>&... with required scheme, non-empty library name and operating mode (ADR-281 Decision item 2).
- Local gRPC server starts before JSON-RPC request loop begins.
- JSON-RPC stdio framing uses one-line JSON messages.
- Shutdown paths terminate active subscriptions and avoid indefinite drain.
- The active runtime path is local-instance lifecycle only (no remote keyed-stop orchestration in the module graph).
- Vendor STRUCTFIELD ComParam entry sizes (ADR-218, as amended) are resolved from a single source: a per-library `vendor_struct_types` table in `config.toml` (read via `vci-service-config`'s `find_vendor_struct_type_size`), consulted identically by `GetComParam`/`GetUniqueRespIdTable` (read) and `SetComParam`/`SetUniqueRespIdTable` (write, which additionally requires a non-empty write's declared `size_of_entry` to match the configured value). There is no process-lifetime write cache — an earlier design had one, removed after Codex review found it let an unverified client-declared write size get trusted on a later read (a local heap-disclosure path, since a successful native write never validates the declared size against the connected library's real layout).

## Implementation Policy

- Keep module boundaries explicit across transport and lifecycle layers.
- Preserve local-instance ownership semantics for stop behavior.
- Update lifecycle behavior and docs together when changing shutdown/startup flows.
- Maintain deterministic subscription termination behavior under local stop and process-exit paths.
- `get_status`'s `error` field and any construction/resolve-error `Status` returned from `error.rs` must stay sanitized: never format a raw `libloading::Error`/`io::Error`/underlying resolver error (Display or Debug) into client- or launcher-facing text, since these can contain local filesystem paths. Log the full error server-side via `tracing::error!` instead (ADR-089).
- `StartComPrimitive` ORs ENABLE_EXTRA_INFO (ISO 22900-2:2022 D.2.1, byte 0 bit 5) into the TxFlag after converting the caller's control data, once, in `rpc_start_com_primitive` (`service/convert.rs` `with_extra_info`, `control_for_start`; ADR-428). Every other caller bit and any raw length above four bytes are kept; shorter flags are zero-padded to four (D.2). It applies to every path: named bits, raw bytes, no `tx_flag`, no `cop_ctrl_data`.
- A result item with `hCop` undefined and a non-empty extra info header breaks ISO 22900-2:2022 10.1.4.11 and 10.1.4.11.4 (D.2.1): it is a sign of a non-conforming library, for design 8.11 relies on the API's own tester present giving the agent no identifier to match (ADR-430). `to_proto_event_item` (stream and `GetEventItem`) forwards it unchanged and logs a warning on the first such item per link and kind (transmit indication, start of message, other) with the header's identifier (four bytes) or length, counts the rest in `CopTagMap` under the `cop_tags` lock, and logs one summary line with the count when it is above one at `DestroyComLogicalLink` or `ModuleDisconnect`. A bound item with a header (ADR-428) is not logged.

## Change Checklist

1. Run lifecycle integration tests after JSON-RPC dispatch or startup parsing changes.
2. Validate stop and stdin-close paths terminate subscriptions and release server resources.
3. Verify local-instance stop behavior remains functionally equivalent.
4. Sync updates with revised spec and shutdown design notes in docs folder.
