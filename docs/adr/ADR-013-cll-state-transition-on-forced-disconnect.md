# ADR-013: CLL State Transition Sequence on Forced Disconnect

**Date:** 2026-06-28  
**Status:** Superseded by ADR-019  
**Affects:**
- `j2534-0404-service/src/service/rpc_link.rs` (`rpc_disconnect_com_logical_link`)

> **Note (2026-06-29):** The decision in this ADR — emitting a synthetic
> `PduCllstOnline` before `PduCllstOffline` when `was_comm_started` — was found
> to violate the ISO 22900-2 Use Case "PDU_CLLST_OFFLINE from any state", which
> requires a **direct** transition to Offline with no intermediate Online.
> The `if was_comm_started { send Online }` block has been removed.
> See ADR-019 for the authoritative event-ordering decision.

## Context

ISO 22900-2 §9.3.2.4 defines the legal state transitions for a ComLogicalLink:

```
Offline → Online (ConnectComLogicalLink)
Online  → CommStarted (CoptStartcomm)
CommStarted → Online (CoptStopcomm)
Online  → Offline (DisconnectComLogicalLink)
```

A `DisconnectComLogicalLink` call when the CLL is in `CommStarted` state
represents an implicit stop-comm followed by a disconnect.

**Bug (L-NEW-3):**  
Before this fix, `rpc_disconnect_com_logical_link` emitted `PduCllstOffline`
directly when `comm_started = true`, producing the transition:

```
CommStarted → Offline   ← missing intermediate Online event
```

`SubscribeEvent` clients that tracked CLL state using the event stream would
never see `PduCllstOnline`, making it impossible to know that the comm session
was terminated as part of the disconnect rather than via an explicit
`CoptStopcomm`.  This affected:
- Diagnostic tools that clean up session state (e.g., close a UDS session) on
  the `Online → Offline` transition.
- State machines that require a `CoptStopcomm` acknowledgement before
  proceeding.

## Decision

`rpc_disconnect_com_logical_link` now captures `was_comm_started` from the link
state before clearing it, and emits `PduCllstOnline` before `PduCllstOffline`
when the CLL was in comm-started state:

```rust
if was_comm_started {
    events::send_cll_status(&self.subscriptions, handle, PduCllstOnline).await;
}
events::send_cll_status(&self.subscriptions, handle, PduCllstOffline).await;
```

This produces the complete transition sequence:

```
CommStarted → Online (implicit StopComm) → Offline (disconnect)
```

Note that the tester-present periodic message is already stopped via the
`periodic_id_to_stop` path (ADR-010), and `cancel_link_cops` cancels any queued
COPs before the events are emitted.  The `PduCllstOnline` event therefore
arrives after all queued COPs have been cancelled.

## Alternatives Considered

1. **Emit `PduCopstFinished` for the tester-present COP when forced-stopping** —
   There is no COP handle for the internal tester-present mechanism; it is managed
   as a J2534 periodic message, not a D-PDU primitive.

2. **Apply this fix to `rpc_destroy_com_logical_link` as well** — `Destroy`
   calls `terminate_subscription` immediately after `cancel_link_cops`, which
   sends `Status::cancelled` and closes the stream.  There is no subscriber to
   receive further events.  Emitting `PduCllstOnline` to a closed stream would
   be silently dropped, so the fix is not applied there.

3. **Apply this fix to `rpc_module_disconnect`** — `ModuleDisconnect` sends
   `PduCllstOffline` for every connected CLL and then calls
   `terminate_all_subscriptions`.  Adding `PduCllstOnline` for comm-started CLLs
   before the `PduCllstOffline` would be consistent with this ADR.  Left as a
   future improvement; `ModuleDisconnect` is a coarse-grained operation and the
   current transition (`CommStarted → Offline`) is a common convention for
   "everything is shutting down."

## Consequences

- `SubscribeEvent` clients now observe the complete `CommStarted → Online →
  Offline` sequence when `DisconnectComLogicalLink` is called while comm is
  active.
- The `PduCllstOnline` event is synthetic (no actual `CoptStopcomm` was
  executed), but it carries the same semantic meaning: the CLL is no longer in
  comm-started state.
- Clients that previously assumed a `CommStarted → Offline` transition would
  not be broken — they will simply see an additional `Online` event before
  `Offline`.
