# ADR-082: `client_filters` Non-Empty Implies Sole Channel Ownership — No Orphaned-Filter Teardown Needed

**Date:** 2026-07-12
**Status:** Accepted (load-bearing check set amended by ADR-129 to include pre-connect `pending_client_filters`)
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`rpc_disconnect_com_logical_link`, `rpc_destroy_com_logical_link`)

## Context

A Codex automated review (PR #80) raised a concern: in `rpc_disconnect_com_logical_link`/`rpc_destroy_com_logical_link`, this CLL's `client_filters` are drained and each entry's `stop_message_filter` is attempted, but a failure is only logged (`warn!`), not retried or re-tracked. If the physical channel remains shared (not the last CLL, so `ref_count > 0` after the decrement), no `PassThruDisconnect` happens — and since that's the only other place a hardware filter would get torn down, a failed `stop_message_filter` could in principle leave a live filter (e.g. a `PDU_FLT_BLOCK`) on hardware with no `client_filters` entry anywhere to track or retry it, letting a later `ConnectComLogicalLink`/`ensure_uudt_companion_channel` join the channel believing it unfiltered.

## Decision

**This state is unreachable given invariants already established elsewhere in this same PR — no new mechanism is added.** `client_filters` being non-empty for a CLL implies that CLL is the *sole* owner of its physical channel (`ref_count == 1`):

- `ioctl_start_msg_filter` (`rpc_misc.rs`) is the only writer that ever inserts into `client_filters`, and it unconditionally rejects the request when the channel is already shared (`ref_count > 1`, ADR-079/ADR-080's shared-channel filter rejection).
- Both places a channel's `ref_count` can be incremented past 1 — `rpc_connect_com_logical_link`'s join path and `ensure_uudt_companion_channel`'s companion-channel join path — reciprocally reject the join if any current CLL on that channel already has a non-empty `client_filters` (Codex-review fixes from earlier rounds).
- Both directions run this check-then-mutate sequence under a continuously-held `shared_channels` guard (ADR-080's lock hierarchy), so install and join can never interleave to smuggle a filter onto a channel mid-join.

Therefore, a CLL disconnecting or being destroyed while its `client_filters` is non-empty is always the last CLL on its channel: the `ref_count` decrement always reaches 0, and `PassThruDisconnect` always fires on that same channel, tearing down every hardware filter on it regardless of whether the preceding `stop_message_filter` calls succeeded. The `warn!`-and-continue on a failed `stop_message_filter` is genuinely best-effort cleanup for a channel that is about to be fully torn down anyway, not a path that can leave an orphaned filter behind on a channel that stays open.

## Consequences

- No orphan-tracking state, channel-wide fallback clear, or retry mechanism is added to `rpc_disconnect_com_logical_link`/`rpc_destroy_com_logical_link` — the existing `warn!`-and-continue is correct as written.
- **This conclusion is load-bearing on three specific checks staying in place together**: `ioctl_start_msg_filter`'s `ref_count > 1` rejection, and the two reciprocal join guards in `rpc_connect_com_logical_link`/`ensure_uudt_companion_channel`. If any of the three is ever relaxed (e.g. a future change allows installing filters on an already-shared channel, or per-CLL software-side filtering is added per ADR-079's deferred PASS-filter discussion), this ADR must be revisited — the disconnect/destroy teardown gap this ADR closes would then reopen and would need one of: a channel-wide clear+rebuild fallback, or new `SharedChannel`-level orphan tracking consulted by the join guards.
- **Amended by ADR-129**: pre-connect filter configuration (`pending_client_filters`) added a *fourth* load-bearing check to the set above — `rpc_connect_com_logical_link`'s join guard also rejects joining an already-existing channel when the *connecting* CLL itself holds non-empty `pending_client_filters`, symmetric to the pre-existing "another CLL on the channel already has `client_filters`" check. This keeps the same sole-ownership invariant intact for filters that install at connect time instead of via a live `ioctl_start_msg_filter` call; see ADR-129 for the full design.
- `PassThruDisconnect` itself failing (a separate, pre-existing, non-filter-specific concern — the channel is already removed from `shared_channels` by that point) is out of scope for this ADR.
