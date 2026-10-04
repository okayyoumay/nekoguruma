# ADR-019: CLL/COP Event Ordering on Disconnect and Hard Error

**Date:** 2026-06-29  
**Status:** Accepted  
**Supersedes:** ADR-013 (state-transition decision), ADR-015 (state-transition decision), ADR-016 (state-transition decision)  
**Affects:**
- `j2534-0404-service/src/service/events.rs` (`poll_rx`, `poll_rx_and_check_match`, `wait_for_expected_response`, new `handle_channel_hard_error`)
- `j2534-0404-service/src/service/rpc_link.rs` (`rpc_disconnect_com_logical_link`, `rpc_destroy_com_logical_link`)

## Context

ADR-013, ADR-015, and ADR-016 all introduced a synthetic `PduCllstOnline` event
before `PduCllstOffline` when a CLL was in `CommStarted` state during voluntary
disconnect, destroy, or hard channel error.  This was intended to represent an
"implicit stop-comm" and produce the `CommStarted → Online → Offline` sequence.

The ISO 22900-2 Use Case "PDU_CLLST_OFFLINE from any state" specifies the opposite:
any forced de-activation — whether voluntary (Disconnect/Destroy) or involuntary
(loss of VCI comms) — transitions **directly** from any state to `PDU_CLLST_OFFLINE`
with no intermediate `Online` step.  The intermediate `Online` is only emitted by a
voluntary `CoptStopcomm` that succeeds while the CLL remains connected.

Additionally, ADR-016 partially deferred two gaps that are now addressed:

| Gap | Previous state | New state |
|-----|---------------|-----------|
| COP cancellation before `PduCllstOffline` on hard error | Not done | Done via `cancel_link_cops` |
| `PDU_MODST_NOT_AVAIL` after `PduCllstOffline` on hard error | Not done | Done via `send_module_status` |
| `LogicalLinkState` cleanup in poll task on hard error | Not done | `connected`, `comm_started`, `channel_id` cleared |
| `HardError` arm in `wait_for_expected_response` | Emitted `PduCopstFinished` | No additional event (cancel_link_cops covered it) |

## Decision

### 1 — No intermediate `PduCllstOnline` on forced de-activation

For `DisconnectComLogicalLink`, `DestroyComLogicalLink`, and hard channel error,
the CLL transitions directly to `PDU_CLLST_OFFLINE` regardless of its prior state.
The `if was_comm_started { send Online }` blocks are removed from all three paths.

The correct intermediate `Online` for a voluntary stop-comm is emitted by
`handle_stop_comm`, not by the disconnect/destroy/error paths.

### 2 — Canonical event order for hard channel errors

ISO 22900-2 specifies the following sequence for VCI communication loss:

```
PDU_ERR_EVT_LOST_COMM_TO_VCI  (per affected CLL)
PDU_COPST_CANCELLED × N         (per active COP on that CLL)
PDU_CLLST_OFFLINE               (per affected CLL)
PDU_MODST_NOT_AVAIL             (once, module level)
```

`handle_channel_hard_error()` in `events.rs` encapsulates this sequence and is
called from both `poll_rx` and `poll_rx_and_check_match`:

```rust
async fn handle_channel_hard_error(channel_id, primitives, logical_links, subscriptions) {
    // 1. Mark links offline atomically (prevents re-emission on the next tick).
    let cll_handles: Vec<u32> = {
        let mut links = logical_links.lock().await;
        links.iter_mut()
            .filter(|(_, l)| l.channel_id == Some(channel_id))
            .map(|(h, l)| { l.connected = false; l.comm_started = false; l.channel_id = None; *h })
            .collect()
    };
    // 2. Per-CLL: error event → COP cancellations → Offline.
    for cll_h in cll_handles {
        send_error_event(subscriptions, logical_links, cll_h, PduErrEvtLostCommToVci).await;
        cancel_link_cops(primitives, subscriptions, cll_h).await;
        send_cll_status(subscriptions, cll_h, PduCllstOffline).await;
    }
    // 3. Module-level not-available.
    send_module_status(subscriptions, PduModstNotAvail).await;
}
```

### 3 — `LogicalLinkState` is updated inside the poll task

Setting `connected = false`, `comm_started = false`, and `channel_id = None` in
the poll task (step 1 above) prevents a second invocation of
`handle_channel_hard_error` for the same channel if `poll_rx` is called again
before the poll loop exits.  The `channel_id = None` assignment is the key guard:
the filter `l.channel_id == Some(channel_id)` will no longer match.

This departs from ADR-016's original decision not to update `LogicalLinkState` from
the poll task.  The additional locking is acceptable because all mutation is
performed in a single `logical_links.lock().await` scope before releasing the lock.

### 4 — `HardError` arm in `wait_for_expected_response` emits nothing

`handle_channel_hard_error` calls `cancel_link_cops`, which removes the currently
executing COP from `primitives` and emits `PduCopstCancelled` for it (along with
all other queued COPs).  The `HardError` arm in `wait_for_expected_response`
therefore emits no additional status — doing so would duplicate the cancel event.

The outer item handler's `primitives.remove(&item_cop)` is a no-op in this case
(already removed by `cancel_link_cops`), which is safe.

### 5 — `send_module_status` and the module-level subscription

`PDU_MODST_NOT_AVAIL` is sent to the subscriber registered under
`(DEFAULT_MODULE_HANDLE, PDU_HANDLE_UNDEF)`, which is the key used by
`SubscribeEvent` callers that subscribed at the module level (passing
`PDU_HANDLE_UNDEF` as the `cll_handle`).

## Corrections to Prior ADRs

| Prior ADR | Incorrect claim | Correction |
|-----------|----------------|------------|
| ADR-013 | `CommStarted → Online → Offline` required for Disconnect | Direct to Offline per Use Case |
| ADR-015 | `CommStarted → Online → Offline` required for Destroy | Direct to Offline per Use Case |
| ADR-016 | `CommStarted → Online → Offline` required for hard error | Direct to Offline per Use Case |
| ADR-016 | No `cancel_link_cops` on hard error | Now called before `PduCllstOffline` |
| ADR-016 | No `PDU_MODST_NOT_AVAIL` on hard error | Now emitted after `PduCllstOffline` |
| ADR-016 | `LogicalLinkState` not updated in poll task | Now cleared atomically |

ADR-016's decision to emit `PduErrEvtLostCommToVci` and to use that error code
specifically (rather than `PduErrEvtVciHardwareFault` or `PduErrEvtProtErr`) is
**preserved** and not changed.

## Alternatives Considered

1. **Keep the synthetic `PduCllstOnline` on forced de-activation** — Matches some
   intuition about state machine hygiene ("every CommStarted must pass through Online
   before Offline").  Rejected: contradicts the ISO 22900-2 Use Case; the intermediate
   Online is only required for voluntary `CoptStopcomm`.

2. **Emit `PDU_MODST_NOT_AVAIL` only when all CLLs are offline** — Would require
   tracking how many CLLs share the physical module.  The current implementation
   emits it on any hard read error, which is correct because one channel error
   implies the entire VCI adapter is unreachable.

3. **Not updating `LogicalLinkState` in the poll task** (ADR-016 original) — Causes
   a second `handle_channel_hard_error` invocation on the next timer tick for the
   same channel, producing duplicate events.  Rejected.

## Consequences

- `SubscribeEvent` clients observe `PDU_COPST_CANCELLED × N → PDU_CLLST_OFFLINE`
  directly from any state on forced de-activation, with no synthetic `Online`.
- Clients that previously relied on the synthetic `PduCllstOnline` as a stop-comm
  signal must be updated to handle the direct transition.
- `GetLastError` returns `PduErrEvtLostCommToVci` on hard channel errors (unchanged
  from ADR-016).
- Module-level subscribers now receive `PDU_MODST_NOT_AVAIL` after hard channel
  errors, enabling them to detect VCI loss without polling.
- The Delay COP hard-error path also benefits: `cancel_link_cops` emits the cancel
  event for the Delay COP, and the delay loop exits without emitting a duplicate
  `PduCopstCancelled` (see ADR-003 update).
