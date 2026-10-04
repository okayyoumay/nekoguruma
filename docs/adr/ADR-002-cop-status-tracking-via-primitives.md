# ADR-002: COP Status Tracking via `primitives` Map

**Date:** 2026-06-28  
**Status:** Accepted  
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs`,
             `j2534-0404-service/src/service/events.rs`

## Context

`GetStatus` for a COP handle must return one of:
- `PduCopstExecuting` — queued or currently executing
- `PduCopstCancelled` — explicitly cancelled by the caller
- `PduCopstFinished`  — completed (or never started / already cleaned up)

The original implementation could not distinguish `PduCopstCancelled` from
`PduCopstFinished` because `CancelComPrimitive` removed the COP from the
`primitives` map before the poll task could observe the cancellation.

## Decision

`CancelComPrimitive` no longer removes the COP from `primitives`.  Instead it
only inserts the `cop_handle` into `LogicalLinkState.cancelled_cops`.

`GetStatus` now:
1. Looks up `cop_handle` in `primitives` to get `cll_handle`.
2. If found, checks `cll.cancelled_cops` — if present returns `PduCopstCancelled`,
   otherwise `PduCopstExecuting`.
3. If not found in `primitives`, returns `PduCopstFinished`.

The poll task retains the responsibility of removing the COP from `primitives`
after it emits `PduCopstCancelled` (for the explicit-cancel path) or
`PduCopstFinished` (for the normal-completion path).

`cancel_link_cops` (called by disconnect/destroy/hard-error) still removes directly
from `primitives` to avoid processing items from a link that is gone, and emits
`PduCopstCancelled` immediately for each removed COP.

`CoptStopcomm` cancellation uses the same `cancelled_cops` pattern as
`CancelComPrimitive`: it adds sibling COP handles to `cancelled_cops` **without**
removing them from `primitives`.  The poll task observes `cancelled_cops`, emits
`PduCopstCancelled`, and then removes from `primitives` — the same dequeue path
as an explicit cancel.  Removing from `primitives` early (as the original code did)
would cause `GetStatus` to return `PduCopstFinished` briefly before the poll task
could notify the subscriber.

## Rationale

This approach requires no additional data structure.  The `primitives` map already
acts as the source-of-truth for "still in flight"; the `cancelled_cops` set adds a
single bit of additional state per COP.

## Consequences

- `GetStatus` correctly returns `PduCopstCancelled` between the `CancelComPrimitive`
  call and the moment the poll task dequeues and discards the item.
- `PduCopstWaiting` (ISO 22900-2 §9.10.4) is now implemented via
  `SharedChannel.executing_cop` (see ADR-021).  `GetStatus` returns `PduCopstWaiting`
  for COPs that are queued but not yet started by the poll task.
