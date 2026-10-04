# ADR-010: Stop Tester-Present Periodic Message on Shared-Channel Disconnect/Destroy

**Date:** 2026-06-28  
**Status:** Superseded by ADR-093 — mode 0 no longer starts a native periodic message at all
(`PassThruStartPeriodicMsg` is no longer used for tester-present), so there is nothing left for this
ADR's `stop_periodic_message` mechanism to leak or need to stop. Teardown for tester-present is now
an unconditional state clear at both call sites below, regardless of `ref_count`. Kept for historical
context only.  
**Affects:**
- `j2534-0404-service/src/service/rpc_link.rs` (`rpc_disconnect_com_logical_link`,
  `rpc_destroy_com_logical_link`)

## Context

ADR-005 established that the service uses shared J2534 physical channels
(`SharedChannel`, keyed by `(protocol_id, baud_rate)`) when multiple CLLs connect
with the same protocol and baud rate.  `PassThruDisconnect` is deferred until the
last CLL on that channel disconnects (`ref_count` reaches zero).

A CLL can start a periodic tester-present message via `CoptStartcomm`.  The handle
for that message (`tester_present_periodic_id`) is stored in `LogicalLinkState`.

**Bug (M-AUDIT-1):**  
When a CLL with an active tester-present periodic message called
`DisconnectComLogicalLink` or `DestroyComLogicalLink` while sharing a channel with
other CLLs (`ref_count > 0` after decrement), the old code set
`link.tester_present_periodic_id = None` without calling `PassThruStopPeriodicMsg`.

- The physical channel was NOT disconnected (other CLLs still use it).
- The periodic message therefore continued transmitting on the bus indefinitely.
- The handle was lost, making it impossible to stop the message without
  disconnecting and reconnecting the entire physical channel.

## Decision

Before clearing `link.tester_present_periodic_id`, the implementations of
`rpc_disconnect_com_logical_link` and `rpc_destroy_com_logical_link` now:

1. **Capture** `link.tester_present_periodic_id` and `link.channel_id` before any
   mutation of the link state.
2. After decrementing `ref_count`:
   - If `ref_count == 0` (last CLL): proceed with `PassThruDisconnect` as before.
     `PassThruDisconnect` automatically stops all periodic messages on the channel.
   - If `ref_count > 0` (shared channel stays open) **and** a periodic ID was
     captured: call `api.stop_periodic_message(channel_id, periodic_id)` explicitly.
     A `warn!` log is emitted on failure, but the disconnect proceeds regardless.

```rust
// DisconnectComLogicalLink — extract handles before clearing
let (channel_key, periodic_id_to_stop, channel_id_for_tp) = {
    let mut links = self.logical_links.lock().await;
    let link = links.get_mut(&handle).ok_or_else(...)? ;
    let key = link.channel_key.take();
    let periodic_id = link.tester_present_periodic_id.take(); // take, not = None
    let channel_id = link.channel_id;
    link.channel_id = None;
    link.connected = false;
    link.comm_started = false;
    link.held_lock_mask = 0;
    (key, periodic_id, channel_id)
};

// After ref_count decrement
if let Some(channel_id) = disconnected {
    api.disconnect(channel_id)?; // stops all periodic msgs automatically
} else if let (Some(periodic_id), Some(channel_id)) = (periodic_id_to_stop, channel_id_for_tp) {
    // shared channel stays open — stop the leaked TP message
    let _ = api.stop_periodic_message(channel_id, periodic_id);
}
```

The same pattern is applied in `rpc_destroy_com_logical_link` using the captured
fields from the removed `LogicalLinkState`.

## Alternatives Considered

1. **Enqueue `TxItem::StopComm` through the poll task** — Cleaner serialization but
   adds async latency; the disconnect confirmation would need to wait for the poll
   task to process the item.  Direct `api.stop_periodic_message` is simpler and
   already safe because the `api` Mutex serializes all J2534 API calls.

2. **Require callers to issue `CoptStopcomm` before disconnecting** — The D-PDU
   API specification does not make this mandatory.  The service should clean up
   hardware state on disconnect regardless of caller behavior.

3. **Track leaked periodic IDs at the channel level** — Adds complexity to
   `SharedChannel` with no benefit; the CLL state already tracks the ID.

## Consequences

- Tester-present periodic messages are now always stopped when their owning CLL
  disconnects, even on a shared channel.
- `stop_periodic_message` failure is non-fatal: the disconnect still completes and a
  `warn!` log is emitted.  This handles edge cases such as the adapter having already
  cancelled the periodic message internally (e.g., after a bus error).
- `rpc_destroy_com_logical_link` follows the same pattern, using the fields of the
  already-removed `LogicalLinkState` before the struct is consumed.
