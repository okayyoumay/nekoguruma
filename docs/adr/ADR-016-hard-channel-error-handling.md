# ADR-016: Hard Channel Error: State Transitions and Error Event

**Date:** 2026-06-28  
**Status:** Superseded by ADR-019  
**Affects:**
- `j2534-0404-service/src/service/events.rs` (`poll_rx`, `poll_rx_and_check_match`)

> **Note (2026-06-29):** This ADR correctly introduced `PduErrEvtLostCommToVci`
> (GetLastError fix) but its state-transition decision — emitting `PduCllstOnline`
> for `CommStarted` CLLs — violates the ISO 22900-2 Use Case "PDU_CLLST_OFFLINE
> from any state".  Additionally, three gaps deferred here (COP cancellation before
> Offline, `PDU_MODST_NOT_AVAIL`, and `LogicalLinkState` cleanup in the poll task)
> are now addressed.  The code has been rewritten around the new
> `handle_channel_hard_error()` helper.  See ADR-019 for the full replacement.

## Context

When `PassThruReadMsgs` returns a hard error (anything other than
`ERR_BUFFER_EMPTY`), the poll task cannot continue reading from the channel.
This is treated as a permanent channel failure.

Before this fix, both `poll_rx` and `poll_rx_and_check_match` handled the error
identically:

```rust
// Before
for cll_h in cll_handles {
    send_cll_status(subscriptions, cll_h, PduCllstOffline).await;
}
```

This had two deficiencies:

**Bug L-NEW-8** — State transition inconsistency:  
For CLLs in `CommStarted` state, the code emitted `PduCllstOffline` directly,
producing the transition `CommStarted → Offline`.  ISO 22900-2 §9.3.2.4 requires
`CommStarted → Online → Offline`.  ADR-013 fixed the same gap for voluntary
`DisconnectComLogicalLink`; the error path was overlooked.

**Bug L-NEW-9** — Missing error event:  
No call to `send_error_event` was made.  `LogicalLinkState.last_error` was never
updated, so a polling client calling `GetLastError` after observing `PduCllstOffline`
would receive `PduErrEvtNoerror` — no indication of why the CLL went offline.

## Decision

Both `poll_rx` and `poll_rx_and_check_match` now collect `(cll_handle, comm_started)`
pairs and, for each affected CLL:

1. Call `send_error_event` with `PduErrEvtLostCommToVci` — persists the failure code
   in `LogicalLinkState.last_error` so `GetLastError` returns a meaningful value.
2. If `comm_started`: emit `PduCllstOnline` (synthetic implicit stop-comm).
3. Emit `PduCllstOffline`.

```rust
let cll_state: Vec<(u32, bool)> = {
    let links = logical_links.lock().await;
    links.iter()
        .filter(|(_, l)| l.channel_id == Some(channel_id))
        .map(|(&h, l)| (h, l.comm_started))
        .collect()
};
for (cll_h, comm_started) in cll_state {
    send_error_event(subscriptions, logical_links, cll_h, PduErrorEvent::PduErrEvtLostCommToVci).await;
    if comm_started {
        send_cll_status(subscriptions, cll_h, PduComLogicalLinkStatus::PduCllstOnline).await;
    }
    send_cll_status(subscriptions, cll_h, PduComLogicalLinkStatus::PduCllstOffline).await;
}
```

**Choice of error event: `PduErrEvtLostCommToVci`**  
`PassThruReadMsgs` can fail for several reasons (device disconnected, handle
invalidated, driver failure).  The common root cause is that the service can no
longer communicate with the VCI adapter.  `PduErrEvtLostCommToVci` (0x106) is the
most semantically accurate available code; `PduErrEvtVciHardwareFault` (0x107) was
also considered but is more specific to hardware damage rather than communication
loss.

**Note on `LogicalLinkState` cleanup:**  
This fix does NOT set `link.connected = false` or `link.comm_started = false` in
`logical_links`.  After a hard error, callers are expected to observe `PduCllstOffline`
and call `DisconnectComLogicalLink`, which performs the full state cleanup.  Updating
`LogicalLinkState` from inside the poll task would require additional locking and
could race with concurrent disconnect calls.  The `GetStatus` inconsistency (reporting
`CommStarted` after `PduCllstOffline` has been sent) is a pre-existing limitation
and is outside the scope of this fix.

## Alternatives Considered

1. **Use `PduErrEvtProtErr` (0x105)** — `PduErrEvtProtErr` was already used for
   `PassThruIoctl SET_CONFIG` failures.  Using it for a channel read error would
   conflate two different failure categories.  Rejected.

2. **Use `PduErrEvtVciHardwareFault` (0x107)** — Implies a hardware defect rather
   than a communication loss.  `PduErrEvtLostCommToVci` is more precisely descriptive
   of the observable symptom.  Rejected.

3. **Update `LogicalLinkState.connected`/`comm_started` from the poll task** —
   Increases complexity (lock ordering, races with disconnect/destroy).  The existing
   pattern is for callers to respond to `PduCllstOffline` by calling
   `DisconnectComLogicalLink`.  Deferred.

## Consequences

- `GetLastError` now returns `PduErrEvtLostCommToVci` after a hard channel read
  failure, enabling polling clients to detect and diagnose the failure.
- `SubscribeEvent` clients observe the complete `CommStarted → Online → Offline`
  sequence (or `Online → Offline`) on hard channel errors, consistent with the
  behavior of voluntary disconnect (ADR-013) and destroy (ADR-015).
- The fix applies to both `poll_rx` (background timer path) and
  `poll_rx_and_check_match` (active `CoptSendrecv` path), covering all poll contexts
  in which a hard error can occur.
