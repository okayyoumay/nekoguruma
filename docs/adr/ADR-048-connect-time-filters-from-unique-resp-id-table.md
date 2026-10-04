# ADR-048: `ConnectComLogicalLink` Builds ISO15765 Filters from the CLL's Current UniqueRespIdTable

**Date:** 2026-07-03
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`install_pass_all_filter`,
             `connect_new_physical_channel`, `spawn_new_shared_channel`,
             `rpc_connect_com_logical_link`), `j2534-0404-service/tests/grpc_mock.rs`

> **Amended by ADR-122**: this ADR removed the zero-mask pass-all
> `FLOW_CONTROL_FILTER` fallback specifically at `ConnectComLogicalLink`.
> ADR-122 extends that removal to the two remaining lifecycle points
> (`CoptUpdateparam` promotion, `CLEAR_MSG_FILTERS`), so the fallback no
> longer exists anywhere in this service. This ADR's connect-time filter
> derivation itself is otherwise unchanged.

## Context

ADR-039 established a two-step filter lifecycle for ISO15765 channels:

1. `ConnectComLogicalLink` unconditionally installs a zero-mask/pattern
   pass-all `FLOW_CONTROL_FILTER` (`install_pass_all_filter`,
   `connect_new_physical_channel`), regardless of whether the connecting
   CLL's `UniqueRespIdTable` is already configured.
2. `SetUniqueRespIdTable` — a separate RPC, typically called *after*
   `ConnectComLogicalLink` in the documented flow — replaces the pass-all
   filter with spec-conformant point-to-point filters built from the table.

`SetUniqueRespIdTable` does not require the CLL to be connected: calling it
before `ConnectComLogicalLink` is accepted and stores the table, but installs
no hardware filter (the filter-install branch is gated on `channel_id` being
`Some`). Combined with step 1 above, a client that configures addressing
*before* connecting still received the pass-all fallback at connect and had
to call `SetUniqueRespIdTable` again afterward for it to take effect on
hardware — the pre-connect call's addressing information was ignored for
filtering purposes until a redundant post-connect call repeated it.

## Decision

`ConnectComLogicalLink` no longer installs any filter for a new ISO15765
physical channel. Instead, after the connecting CLL is finalized (channel
recorded on `LogicalLinkState`), it builds point-to-point
`FLOW_CONTROL_FILTER`s directly from that CLL's `unique_resp_id_table` as
currently stored — reusing `install_point_to_point_fc_filters`, the same
function `SetUniqueRespIdTable` uses — and records the result in
`LogicalLinkState::unique_resp_filter_ids`, exactly as `SetUniqueRespIdTable`
would. This runs unconditionally for every ISO15765 connect, whether the
physical channel is newly created or being joined by another CLL: filter
installation is per-CLL, driven by that CLL's own table, not a one-time
channel-level default.

Two behavioural consequences follow directly from this:

- A table configured via `SetUniqueRespIdTable` **before** `Connect` now
  takes effect immediately at connect time, with no redundant post-connect
  call required.
- A CLL that connects with an **empty** table gets no filter of its own —
  connect no longer falls back to a pass-all `FLOW_CONTROL_FILTER`, matching
  the same "install exactly what the current configuration calls for, no
  wide-open default" that `SetUniqueRespIdTable` already applied post-connect.
  This CLL still receives whatever frames the adapter forwards because of
  *other* CLLs' filters on a shared channel (service-level routing in
  `events.rs` treats an empty `unique_resp_id_table` as "deliver everything
  the channel forwards" — see `route_frame`), but nothing of its own until it
  configures addressing.

`install_pass_all_filter` is simplified to the non-ISO15765 `PASS_FILTER`
case only (its ISO15765 `FLOW_CONTROL_FILTER` branch is now unreachable from
this call site); `SharedChannel::fc_pass_all_filter_id` starts `None` for
every newly spawned physical channel instead of being threaded through from
connect.

**Scope:** this decision applies to ISO15765 hardware channels only.
Non-ISO15765 channels (raw CAN — including the ADR-046 dual-channel
companion and `software-isotp` raw channel — plus K-line/J1850/SCI) keep
installing a wide-open pass-all `PASS_FILTER` at connect, unchanged. Those
protocol families have no existing point-to-point filter builder analogous
to `install_point_to_point_fc_filters`, and for raw CAN specifically, the
`software-isotp` and dual-channel-companion designs (ADR-046/047)
intentionally rely on the hardware channel being wide-open, with narrowing
done at the service level (`RxEntryKind::SoftwareIsoTp`'s raw-frame
passthrough, `RxEntryKind::Companion`'s UUDT-only routing) — narrowing the
hardware filter to known addresses there would conflict with that design.

**Deliberately not addressed:** what happens if a physical channel already
has a stale pass-all filter installed (e.g. from a prior CLL that later
cleared its table, triggering `sync_channel_fc_pass_all_filter` via
`SetUniqueRespIdTable`) when a new, fully-addressed CLL joins at connect.
This decision does not attempt to detect and remove that filter at connect
time — `sync_channel_fc_pass_all_filter` is not called from
`ConnectComLogicalLink` at all — so the stale pass-all filter remains until
the next `SetUniqueRespIdTable` or `CLEAR_MSG_FILTERS` call on that channel
re-evaluates coverage. This is a narrow edge case (coexistence of a pass-all
and point-to-point `FLOW_CONTROL_FILTER` is spec-permitted, just redundant)
and keeping `ConnectComLogicalLink` free of any pass-all-related logic was
judged more valuable than closing it.

## Consequences

- `iso15765_set_unique_resp_id_table_installs_point_to_point_flow_control_filter`
  and `auto_can_channel_mode_falls_back_to_single_channel_when_incapable` in
  `grpc_mock.rs` were updated: both asserted `filter_count == 1` (the
  pass-all fallback) immediately after connect with an empty table; both now
  assert `filter_count == 0`.
- Two new tests cover the behaviour this ADR adds:
  `iso15765_pre_connect_unique_resp_id_table_installs_filter_at_connect`
  (table configured before `Connect` takes effect immediately, no pass-all
  ever appears) and
  `iso15765_joining_cll_with_preconfigured_table_installs_own_filter_at_connect`
  (a CLL joining an already-connected shared channel gets its own
  point-to-point filter from its own table at connect, independent of the
  first CLL's state).
- `SetUniqueRespIdTable`'s own behaviour (ADR-039/040/041/042/043) is
  unchanged: it still rebuilds filters and still calls
  `sync_channel_fc_pass_all_filter` to install/remove the pass-all fallback
  dynamically in response to *its own* calls. Only the unconditional
  connect-time install is removed.
- A deployment that never calls `SetUniqueRespIdTable` on an ISO15765 CLL
  (previously relying on the ADR-005/039 pass-all fallback to receive
  anything) now receives nothing from that CLL's own filters — this is a
  behaviour change for that specific usage pattern, accepted as part of this
  decision (ADR-039's Alternative #3, "drop the pass-all fallback entirely,"
  was rejected there for the general case but is effectively adopted here
  for the connect-time-specific case).
