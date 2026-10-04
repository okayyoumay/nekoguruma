# ADR-015: CLL State Transition Events in DestroyComLogicalLink

**Date:** 2026-06-28  
**Status:** Superseded by ADR-019  
**Affects:**
- `j2534-0404-service/src/service/rpc_link.rs` (`rpc_destroy_com_logical_link`)

> **Note (2026-06-29):** The decision in this ADR — emitting a synthetic
> `PduCllstOnline` before `PduCllstOffline` when `was_comm_started` — was found
> to violate the ISO 22900-2 Use Case "PDU_CLLST_OFFLINE from any state", which
> requires a **direct** transition to Offline with no intermediate Online.
> The `if was_comm_started { send Online }` block has been removed.
> See ADR-019 for the authoritative event-ordering decision.

## Context

ADR-013 fixed `rpc_disconnect_com_logical_link` to emit `PduCllstOnline` before
`PduCllstOffline` when a CLL is in `CommStarted` state, satisfying the ISO 22900-2
`CommStarted → Online → Offline` state sequence.

ADR-013's "Alternatives Considered #2" explicitly decided NOT to apply the same fix
to `rpc_destroy_com_logical_link`, with the following reasoning:

> *"`Destroy` calls `terminate_subscription` immediately after `cancel_link_cops`,
> which sends `Status::cancelled` and closes the stream.  There is no subscriber
> to receive further events."*

**This reasoning was incorrect (bug L-NEW-4).**

`terminate_subscription` is called **after** `cancel_link_cops`, not before.
The `SubscribeEvent` stream is still open during `cancel_link_cops` — that is why
COP-cancel events emitted there are received by the subscriber.  Any events emitted
between `cancel_link_cops` and `terminate_subscription` are equally reachable.

As a result:
- `rpc_destroy_com_logical_link` for a connected CLL sent no `PduCllstOffline` event.
- Subscribers that tracked CLL state via the event stream could not observe the
  CLL going offline; they only received `Status::cancelled` closing the stream.
- For CLLs in `CommStarted` state, the `CommStarted → Online → Offline` sequence was
  missing entirely.

## Decision

`rpc_destroy_com_logical_link` now captures `was_connected` and `was_comm_started`
from the removed `LogicalLinkState` and emits the appropriate state events between
`cancel_link_cops` and `terminate_subscription`:

```rust
events::cancel_link_cops(&self.primitives, &self.subscriptions, handle).await;

if was_connected {
    if was_comm_started {
        events::send_cll_status(&self.subscriptions, handle, PduCllstOnline).await;
    }
    events::send_cll_status(&self.subscriptions, handle, PduCllstOffline).await;
}

// ... handle shared channel cleanup ...

self.terminate_subscription((DEFAULT_MODULE_HANDLE, handle)).await;
```

The subscriber receives:
- `PduCopstCancelled` for each queued COP (from `cancel_link_cops`)
- `PduCllstOnline` if the CLL was in `CommStarted` state (implicit stop-comm)
- `PduCllstOffline` if the CLL was connected
- `Status::cancelled` closing the gRPC stream (from `terminate_subscription`)

CLLs that were never connected (state = Offline) emit no state events; there is no
transition to report.

## Alternatives Considered

1. **Do not emit events on destroy** (the previous approach per ADR-013) — The
   stream is open between `cancel_link_cops` and `terminate_subscription`, so events
   are reachable.  Omitting them leaves subscribers without a clean state-transition
   sequence.  Rejected.

2. **Emit events after `terminate_subscription`** — `terminate_subscription` closes
   the stream; any events sent afterwards are silently dropped.  Rejected.

3. **Remove `terminate_subscription` from destroy and rely on stream close via GC** —
   Would leave zombie subscriptions in the map until the gRPC stream is garbage-
   collected.  Rejected.

## Consequences

- `SubscribeEvent` clients now observe the complete `CommStarted → Online → Offline`
  sequence (or `Online → Offline`) before the stream closes when `DestroyComLogicalLink`
  is called.
- The ordering guarantee is: COP cancel events arrive before state events, which
  arrive before the `Status::cancelled` stream close.
- Supersedes the destroy-path note in ADR-013 ("Apply this fix to
  `rpc_destroy_com_logical_link` as well — left as future improvement").
