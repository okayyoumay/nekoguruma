# ADR-012: Physical Channel Rollback on Concurrent CLL Destroy During Connect

**Date:** 2026-06-28  
**Status:** Accepted  
**Affects:**
- `j2534-0404-service/src/service/rpc_link.rs` (`rpc_connect_com_logical_link`)

## Context

`rpc_connect_com_logical_link` acquires `shared_channels` to perform a
check-create-insert sequence (see the existing comment at that lock site), then
releases the lock before updating `logical_links`.  This produces a window — the
gap between `drop(chans)` and `self.logical_links.lock().await` — in which a
concurrent `DestroyComLogicalLink` or `ModuleDisconnect` call can remove the
CLL from `logical_links`.

**Bug (M-NEW-1):**  
Before this fix, the code called `get_mut(&handle).ok_or_else(...)? ` to update
the CLL.  When the CLL was gone, the `?` caused an early return without touching
`shared_channels`.  The consequence:

- For a **new** channel (`is_new_channel = true`): a `SharedChannel` entry with
  `ref_count = 1` was left in `shared_channels` with no owning CLL.  Its
  `poll_cancel` oneshot sender was never dropped, so the poll task ran
  indefinitely.  The J2534 physical channel was never disconnected.
- For a **joining** channel: `ref_count` was incremented but the CLL's
  `channel_key` was never set to `Some(channel_key)`, so
  `DisconnectComLogicalLink` for that CLL (if it were somehow re-created) could
  not decrement it.  The reference count leak was permanent until
  `ModuleDisconnect`.

## Decision

The `else` branch of the `get_mut` check now performs a rollback:

```rust
} else {
    // The CLL was destroyed concurrently in the window between releasing
    // shared_channels and re-acquiring logical_links.  Roll back the
    // ref_count bump (or the newly created channel) to prevent the physical
    // channel and its poll task from leaking.  See ADR-012.
    drop(links);
    let mut chans = self.shared_channels.lock().await;
    if let Some(sc) = chans.get_mut(&channel_key) {
        sc.ref_count -= 1;
        if sc.ref_count == 0 {
            if let Some(sc) = chans.remove(&channel_key) {
                drop(chans);
                let api = self.api.lock().await;
                let _ = api.disconnect(sc.channel_id);
            }
        }
    }
    return Err(Status::not_found(format!(
        "cll_handle {handle} was destroyed while ConnectComLogicalLink was in progress"
    )));
}
```

Key properties of this rollback:
1. `logical_links` lock is released (`drop(links)`) before re-acquiring
   `shared_channels` to maintain a consistent lock-ordering (shared_channels
   can be nested under logical_links, but not the reverse).
2. If `ref_count` reaches zero, the channel is fully removed and
   `PassThruDisconnect` is called — which also drops `poll_cancel` and stops the
   poll task via the channel drop.
3. `api.disconnect` failure is silently ignored (best-effort); the channel entry
   is removed regardless so the ref_count does not underflow.

## Alternatives Considered

1. **Hold `shared_channels` lock across the `logical_links` update** — Would
   eliminate the window entirely.  Rejected because the two locks would be held
   simultaneously for the duration of the update, and no other code path nests
   them this way (potential future deadlock risk if a new code path ever takes
   `shared_channels` while holding `logical_links`).

2. **Re-check CLL existence before bumping `ref_count`** — Would narrow the
   window but not close it: the race would still exist between the re-check and
   the increment.

3. **Require `DestroyComLogicalLink` to wait until any in-progress `connect` completes** —
   Would require per-CLL in-flight state (a condition variable or a per-handle
   lock), significantly increasing complexity.

## Consequences

- `ConnectComLogicalLink` now returns `not_found` when the CLL is destroyed
  concurrently, instead of leaking the physical channel.
- The rollback path is rare in practice (requires a concurrent destroy during
  connect), but its correctness prevents resource exhaustion in adversarial or
  fault-injection scenarios.
- The `api.disconnect` call in the rollback path acquires the `api` lock while
  NOT holding either `logical_links` or `shared_channels`, which is consistent
  with the rest of the codebase.
