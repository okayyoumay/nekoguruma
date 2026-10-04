# SubscribeEvent Shutdown Implementation Note

Related maintenance note: [implementation-notes.md](implementation-notes.md)

Date: 2026-05-20
Audience: Maintainers of `iso22900-service`

## Purpose

This note documents the current implementation intent around `SubscribeEvent` stream termination when the gRPC server is shutting down, including assumptions, policy decisions, and follow-up work.

## Scope

This note is specific to:
- `iso22900-service` gRPC event streaming (`SubscribeEvent`)
- shutdown paths driven by JSON-RPC stdio (`stop`, stdin close, process exit)
- subscription map lifecycle and stream sender termination behavior

## Assumptions

1. `destroy_com_logical_link` and `module_disconnect` in the underlying API stack unregister related event callbacks automatically.
2. Dropping `DPduApi` unregisters all remaining callbacks.
3. During process shutdown, sending a terminal stream status and removing sender handles is sufficient from the service layer.
4. On shutdown, maintainers prefer `Status::cancelled("Subscription terminated")` for active `SubscribeEvent` streams.

If any assumption changes, review the Policy section below.

## Current Policy

1. On service-initiated stream termination, send one terminal item:
   - `Err(Status::cancelled("Subscription terminated"))`
2. Remove subscription sender(s) from `subscriptions` map.
3. Do not explicitly call `unregister_event_callback` from `terminate_subscription*` helpers.
4. Keep callback unregister logic in the stream finalizer path (`rpc_subscribe_event`) for normal stream-lifecycle cases.

## Implementation Snapshot

- Subscription stream termination helper:
  - `terminate_stream_sender(...)` in `src/service/rpc.rs`
- Per-link termination:
  - `terminate_subscription(...)` in `src/service/rpc.rs`
- Per-module termination:
  - `terminate_subscriptions_for_module(...)` in `src/service/rpc.rs`
- Global termination at shutdown:
  - `Iso22900Service::new` (the `VciServer::new` impl, `src/service/rpc.rs`) spawns a task that awaits
    `shutdown_rx.changed()`, then calls `remove_subscriptions(...)`
    (`src/service/rpc.rs`) directly on the subscriptions map. This is the
    actual shutdown path; `terminate_all_subscriptions(...)`
    (`src/service/rpc.rs`) is a separate helper exercised only by a test
    (`src/service/rpc.rs`, `#[cfg(test)]`), not called from the shutdown
    path itself.
- Stream finalizer unregister behavior:
  - `rpc_subscribe_event(...)` in `src/service/rpc_primitive.rs`

## Behavioral Guarantees (Current)

1. If a subscription sender is still alive when shutdown starts, service attempts to send cancelled status and closes sender by dropping it.
2. Subscription map entries are removed during termination helpers.
3. Process shutdown does not block indefinitely on stream drain in JSON-RPC shutdown path.

## Known Trade-offs

1. If receiver side is already dropped or blocked, terminal status may not be observed by client even though sender is removed.
2. Because explicit unregister in `terminate_subscription*` is omitted by policy, callback cleanup timing relies on underlying API guarantees and finalizer path.
3. Concurrent races are possible between finalizer unregister and disconnect/destroy side-effects, but are acceptable under current assumptions.

## Operational Checks

When debugging stream shutdown behavior:
1. Confirm the shutdown task in `Iso22900Service::new` (`src/service/rpc.rs`) still calls `remove_subscriptions(...)` after `shutdown_rx.changed()`.
2. Confirm `subscriptions` map is empty after termination.
3. Confirm stream finalizer task executes and unregister path remains healthy.
4. Validate client receives terminal status in representative tooling (not all CLIs display stream terminal status identically).

## Change Control

Any change to status code (`cancelled`/`aborted`), callback ownership, or unregister responsibility should update:
- this note
- tests in `src/service.rs` and integration tests under `tests/`
- user-visible behavior docs in `docs/`
