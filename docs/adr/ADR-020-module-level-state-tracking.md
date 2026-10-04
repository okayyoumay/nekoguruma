# ADR-020: Module-Level State Tracking (ModuleState)

**Date:** 2026-06-29  
**Status:** Accepted (GetLastError read path amended by ADR-105)  
**Affects:**
- `j2534-0404-service/src/service.rs` (`ModuleState` struct, `SharedChannel.executing_cop`, `J2534Service.module_state`)
- `j2534-0404-service/src/service/events.rs` (`handle_channel_hard_error`, `poll_rx`, `send_module_status`)
- `j2534-0404-service/src/service/rpc_link.rs` (`rpc_connect_com_logical_link`)
- `j2534-0404-service/src/service/rpc_primitive.rs` (`GetStatus`, `GetLastError`, `GetEventItem`)

## Context

`handle_channel_hard_error` (introduced in ADR-019) correctly emits a
`PDU_MODST_NOT_AVAIL` event to any active `SubscribeEvent` stream when a
physical channel encounters a hard error.  However, three polling-style RPCs
remained broken for the module handle:

- **`GetStatus(module_handle)`** — always returned `PduModstReady` regardless of
  the actual VCI adapter state.
- **`GetLastError(module_handle)`** — always returned `PduErrEvtNoerror` even
  after a hard error caused `PduErrEvtLostCommToVci` to be emitted.
- **`GetEventItem(module_handle)`** — always returned a static `PduModstReady`
  item instead of reflecting the current module status.

Polling-style clients that do not use `SubscribeEvent` (e.g., clients that call
`GetStatus` / `GetLastError` on a timer) had no way to detect VCI adapter
failures.

Additionally, `GetEventItem(cll_handle)` rejected calls when
`link.connected == false`, preventing clients from draining frames that arrived
in `rx_buf` before `DisconnectComLogicalLink` was called.

## Decision

### `ModuleState` struct

A new `pub(super) struct ModuleState` is added to `service.rs`:

```rust
pub(super) struct ModuleState {
    pub(super) status: PduModuleStatus,   // initially PduModstReady
    pub(super) last_error: PduErrorEvent, // initially PduErrEvtNoerror
}
```

`J2534Service` holds `module_state: Arc<Mutex<ModuleState>>` initialized with
`ModuleState::default()`.

### Write path: `handle_channel_hard_error`

After emitting CLL-level events, `handle_channel_hard_error` now:

1. Locks `module_state` and sets `status = PduModstNotAvail`,
   `last_error = PduErrEvtLostCommToVci`.
2. Calls `send_module_status` (SubscribeEvent notification, unchanged).

`module_state` is threaded through `poll_rx`, `poll_rx_and_check_match`, and
`wait_for_expected_response` so all hard-error code paths update it.

### Reset path: `rpc_connect_com_logical_link`

When a new physical channel is opened (`PassThruConnect` succeeds), module state
is reset to `(PduModstReady, PduErrEvtNoerror)` before the new `SharedChannel`
is inserted into the map.  This ensures that after a VCI recovery (user
reconnects the adapter), polling clients observe the correct Ready state without
requiring an explicit re-open call.

No SubscribeEvent notification is sent on recovery — the channel creation success
is observable through the RPC return code.

### Read path: `GetStatus`, `GetLastError`, `GetEventItem` (module handle)

All three handlers now read from `module_state` instead of returning hardcoded
values:

```rust
// GetStatus(module)
let status = self.module_state.lock().await.status;

// GetLastError(module)
let error = self.module_state.lock().await.last_error;

// GetEventItem(module)
let status = self.module_state.lock().await.status;
// returns EventItem { data: ModuleStatus(status as i32) }
```

`GetEventItem(module)` returns the **current** module status as a snapshot
(not a queue drain).  Unlike CLL event items (which are frames dequeued from
`rx_buf`), module status changes are rare and the single-value model is
consistent with the existing `GetStatus` semantics.

### `GetEventItem(cll_handle)` — disconnected CLL guard removed

The `if !link.connected { return failed_precondition }` guard is removed.
`GetEventItem(cll)` now returns frames from `rx_buf` as long as the CLL exists
in `logical_links` (i.e., has not been destroyed).  Frames that arrived before
`DisconnectComLogicalLink` are no longer silently discarded.

The link is still validated via `.ok_or_else(|| not_found(...))` — calling
`GetEventItem` on a destroyed CLL (after `DestroyComLogicalLink`) still returns
an error.

## Alternatives Considered

1. **Queue-based `GetEventItem(module)` with event drain** — A separate
   `VecDeque<PduModuleStatus>` would mirror the per-CLL `rx_buf` pattern.
   Rejected as overly complex for a value that changes at most once per hard
   error; the snapshot model is sufficient for polling clients.

2. **Reset module state on any successful RPC, not just channel open** — Broader
   reset points (e.g., `ModuleConnect`) were considered.  Channel open was chosen
   because it is the first operation that proves the VCI adapter is responsive
   after a failure.

3. **Track `module_last_error` separately in `LogicalLinkState`** — Already done
   at the CLL level (`last_error: Option<PduErrorEvent>`).  A separate
   module-level field is cleaner than overloading per-CLL storage.

## Consequences

- `GetStatus(module)`, `GetLastError(module)`, and `GetEventItem(module)` now
  reflect VCI adapter state for polling-style clients.
- `GetEventItem(cll)` allows frame drain after `DisconnectComLogicalLink`.
- `ModuleState` adds one `Arc<Mutex<ModuleState>>` allocation per `J2534Service`
  instance (negligible overhead).
- `handle_channel_hard_error` acquires an additional lock (`module_state`); this
  does not affect lock ordering since `module_state` is not held anywhere else
  simultaneously.

## Addendum: `handle_stop_comm` error event (unimplemented-4)

Also addressed in the same change set: `handle_stop_comm` in `events.rs` now
emits `PduErrEvtTesterPresentError` when `stop_periodic_message` fails:

```rust
if let Err(err) = api.stop_periodic_message(channel_id, pid) {
    warn!(%err, "stop_periodic_message failed");
    send_error_event(..., PduErrorEvent::PduErrEvtTesterPresentError).await;
}
```

Previously only a `warn!` log was produced.  This is consistent with:
- `handle_start_comm`, which already emits `PduErrEvtTesterPresentError` on
  `start_periodic_message` failure.
- `IoCtl CLEAR_PERIODIC_MSGS` in `rpc_misc.rs`, which also emits
  `PduErrEvtTesterPresentError` on `stop_periodic_message` failure.

Note: `send_error_event` acquires `logical_links` to persist `last_error`, but
the lock is released before emitting to the subscriber, so there is no deadlock
risk with the existing lock already dropped before this call.
