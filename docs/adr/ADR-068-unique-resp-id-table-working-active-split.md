# ADR-068: UniqueRespIdTable Gets a Working/Active Split, Mirroring ComParamSet (ADR-067)

**Date:** 2026-07-07
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (`LogicalLinkState`, `TxItem::UpdateParam`,
             `unique_resp_id_tables_equal`), `j2534-0404-service/src/service/rpc_link.rs`
             (`rpc_connect_com_logical_link`, `promote_unique_resp_id_table`,
             `reinstall_iso15765_channel_filters_after_clear`), `j2534-0404-service/src/service/rpc_misc.rs`
             (`rpc_get_unique_resp_id_table`, `rpc_set_unique_resp_id_table`),
             `j2534-0404-service/src/service/rpc_primitive.rs` (`update_param_working_snapshot`,
             `rpc_start_com_primitive`), `j2534-0404-service/src/service/events.rs`
             (`build_cll_rx_entries`, `handle_update_param`, `handle_restore_param`,
             `spawn_channel_poll_task`, `poll_channel_events`, `dispatch_tx_item`)

## Context

`LogicalLinkState::unique_resp_id_table` was a single `Vec<EcuUniqueRespEntry>`
field, written by `SetUniqueRespIdTable` and read directly by everything else:
`GetUniqueRespIdTable`, TX header construction (`tx_header::build_tx_message`,
ADR-050), RX routing (`events::build_cll_rx_entries`, ADR-007), and ISO15765
`FLOW_CONTROL_FILTER` installation (ADR-039). `SetUniqueRespIdTable` itself
performed the filter install/remove, the pass-all-fallback resync (ADR-039),
and the dual-channel-mode UUDT companion-channel open/close (ADR-046) inline,
synchronously, as soon as the RPC was called.

ADR-067 gave `ComParamSet` a `working`/`active` split: `SetComParam` stages
Working only; a ComPrimitive resolves against a call-time-bound snapshot
(Active normally, or Working for the duration of a `temp_param_update`
transaction); promotion (Working → Active, plus the hardware
`PassThruIoctl SET_CONFIG` push) happens only at `ConnectComLogicalLink` (for
the channel's creator) or at `CoptUpdateparam`. The UniqueRespIdTable never
got the same treatment — ADR-067 §G explicitly called this out as unchanged,
deliberately out of scope for that ADR.

This asymmetry is not just cosmetic: `SetUniqueRespIdTable` doing real
hardware I/O (`PassThruStartMsgFilter`/`PassThruStopMsgFilter`, ADR-039)
synchronously, on every call, means there is no way to stage several ECUs'
addressing changes and apply them atomically the way `SetComParam` +
`CoptUpdateparam` allows for ordinary ComParams — every `SetUniqueRespIdTable`
call is immediately live, mid-configuration, even if the caller intended to
build up a whole table across several calls before committing it.

## Decision

`LogicalLinkState::unique_resp_id_table` becomes two fields:

- `working_unique_resp_id_table` — written by `SetUniqueRespIdTable`, read by
  `GetUniqueRespIdTable` (mirrors `ComParamSet::working`).
- `active_unique_resp_id_table` — the table actually reflected by installed
  ISO15765 `FLOW_CONTROL_FILTER`s, by RX routing, and by what a ComPrimitive
  resolves TX addressing against (mirrors `ComParamSet::active`).

`unique_resp_id_tables_equal` (`service.rs`) compares two tables
order-insensitively, keyed by `unique_resp_identifier`, used as the diff gate
described below.

### Set / Get

`SetUniqueRespIdTable` (`rpc_misc.rs`) now does nothing but validate
(`PDU_PC_UNIQUE_ID` class boundary, ISO 22900-2 §9.3.3.6, ADR-042) and write
`working_unique_resp_id_table`. It performs **no hardware I/O** — the
`FLOW_CONTROL_FILTER` install/remove, the pass-all-fallback resync, and the
UUDT companion-channel open/close it used to perform inline all move to
promotion time. The `LOCK_PHYSICAL_COM_PARAMS` check on ISO15765 (ADR-043)
stays at Set/stage time — see the amendment note below.

`GetUniqueRespIdTable` reads `working_unique_resp_id_table` (mirrors
`GetComParam` reading Working).

### Promotion

A new helper, `J2534Service::promote_unique_resp_id_table(handle, new_table)`
(`rpc_link.rs`, next to `install_point_to_point_fc_filters`/
`sync_channel_fc_pass_all_filter`/the UUDT companion-channel helpers it
calls): sets `active_unique_resp_id_table = new_table`, and — unless
`new_table` is element-wise equal to the OLD active table
(`unique_resp_id_tables_equal`) — reconciles ISO15765
`FLOW_CONTROL_FILTER`s (stop filters derived from the old active table,
install filters derived from the new one, the exact per-entry derivation
ADR-039/040/041 already established), re-syncs the channel-wide pass-all
fallback, and re-syncs the dual-channel-mode UUDT companion channel (ADR-046)
against the new active table. The diff gate means a `CoptUpdateparam` that
only changed plain ComParam values (table snapshot unchanged) does zero
filter I/O.

Promotion happens at exactly two points:

1. **This CLL's own `ConnectComLogicalLink`.** Every connecting CLL — not
   just the physical channel's creator — copies
   `working_unique_resp_id_table` into `active_unique_resp_id_table` for
   itself, then installs point-to-point filters directly from the
   just-promoted Active table (ADR-048's connect-time behavior, now sourced
   from Active). This diverges from `ComParamSet`, whose Active promotion at
   Connect happens only for the channel's creator (a joining CLL's `active`
   stays default until its own `CoptUpdateparam`): ISO15765
   `FLOW_CONTROL_FILTER`s are per-CLL (ADR-039), not a property of the shared
   physical channel, so every CLL's own addressing must take effect at its
   own Connect regardless of who created the channel. The promotion write
   itself is in-memory only and happens before the physical `PassThruConnect`
   attempt (needed so `connect_flags`/the UUDT pre-check, which read the
   table, see the promoted value) — harmless if the physical connect
   subsequently fails, since no filter I/O has happened yet in that case.
   Connect does **not** call the shared `promote_unique_resp_id_table`
   helper: it installs filters directly and deliberately does not run the
   pass-all-fallback resync, preserving ADR-048's "no unconditional pass-all
   fallback at Connect" behavior (running it here would spuriously reinstate
   the fallback whenever a joining CLL's sibling has not yet configured its
   own table).
2. **`CoptUpdateparam` execution.** `rpc_start_com_primitive` snapshots the
   Working ComParam set AND `working_unique_resp_id_table` together, in a
   SINGLE `logical_links` lock acquisition (`update_param_working_snapshot`),
   at call time (`TxItem::UpdateParam` gains a `unique_resp_id_table` field).
   Taking both under one lock, rather than two separate reads, matters: a
   `SetComParam`/`SetUniqueRespIdTable` landing between two reads could
   otherwise hand this COP a mixed-time (params, table) pair that never
   actually existed together at any single instant — the same TOCTOU class
   ADR-067 claim A closes for `bound_comparams`, and the fix this ADR uses is
   the identical single-critical-section pattern. `handle_update_param`
   (`events.rs`), only on hardware success, calls
   `promote_unique_resp_id_table` with that snapshot.

### COP table binding: unchanged, still Active-only

`rpc_start_com_primitive` binds `bound_active_table` — the Active
UniqueRespIdTable, used by `CoptSendrecv`/`CoptStartcomm` to resolve TX
addressing (ADR-050) — in the SAME critical section as `bound_comparams`
(ADR-067 claim A), unconditionally (not gated on `temp_eligible`), rather
than via a separate helper call inside each branch. This closes the same
class of TOCTOU as claim A/point 2 above: Active can change independently of
this RPC call (another CLL's `ConnectComLogicalLink`, or this CLL's own
`CoptUpdateparam` executing), and reading it in a second, later lock
acquisition could hand the COP a comparams-from-T1/table-from-T2 mixed
snapshot. The *timing* is otherwise unchanged from before this ADR
(ADR-067 §G): the snapshot is taken at `StartComPrimitive` call time,
**regardless of `temp_param_update`**. Unlike `ComParamSet`, there is no
Working-side table a temp COP can borrow — `ParamBinding::Temp`'s
`effective` (Working) has no table analogue. This is the one deliberate
asymmetry from `ComParamSet`'s split: a `temp_param_update` `CoptSendrecv`
resolves its ComParams from Working but its addressing table from Active,
unconditionally.

`events::build_cll_rx_entries` (RX routing / software-ISO-TP FlowControl
addressing) also reads `active_unique_resp_id_table`, not Working: RX must
match what is actually installed on hardware (the Active-derived
`FLOW_CONTROL_FILTER`s) and what TX/COP resolution uses, not a staged table
that has not taken effect yet.

### RestoreParam

`handle_restore_param` (`events.rs`), already copying `active → working` for
`ComParamSet`, now also does
`working_unique_resp_id_table = active_unique_resp_id_table.clone()`. No
filter I/O — Active (and therefore hardware) is unchanged. ISO 22900-2 §9.4
does not explicitly mandate that `CoptRestoreParam` covers the
UniqueRespIdTable; this is consistent completion of the Working/Active split
this ADR introduces, not a spec mandate.

### No Working reset on temp COP completion

ADR-067 claim D's `temp_param_update` writeback (`working = active.clone()`
for `ComParamSet`, right before `rpc_start_com_primitive` returns) is **not**
extended to the table. There is nothing to "consume": the table was never
borrowed from Working by the temp COP in the first place (it always reads
Active, see above), so resetting Working here would only discard a staged
table for no reason — the opposite of what the writeback is for.

## Rationale for the amendments to ADR-039/043/044

- **ADR-039** (filter derivation): the per-entry filter-building logic itself
  is unchanged; only *when* it runs moved from `SetUniqueRespIdTable` call
  time to promotion time.
- **ADR-043** (lock check timing): staying at Set/stage time is the earliest
  possible rejection point, consistent with how `SetComParam` checks the lock
  before writing Working rather than waiting for a promotion that may never
  come. The lock still protects the actual hardware I/O too, since that I/O
  is now gated at promotion by ADR-044 (`CoptUpdateparam`'s own
  `LOCK_PHYSICAL_COM_PARAMS` check) and by `ConnectComLogicalLink`'s existing
  connect-time lock check (ADR-045).
- **ADR-044** (`CoptUpdateparam`'s lock check): already covers the table's
  hardware I/O now that it happens inside `CoptUpdateparam` execution — no
  amendment needed there beyond a cross-reference.

## Consequences

- `j2534-0404-service/tests/grpc_mock/flow_control_filters.rs`'s four
  post-connect `SetUniqueRespIdTable` tests now assert no filter I/O at Set
  time and add an explicit `CoptUpdateparam` (via the new
  `harness::promote_via_update_param` helper) before asserting filter state.
  The two pre-connect tests (table staged before `ConnectComLogicalLink`) are
  unaffected, since Connect already promotes.
- Every other pre-existing `grpc_mock` test that calls
  `set_unique_resp_table`/`set_can_phys_req_id` on an already-connected CLL
  and expects it to take effect immediately (the overwhelming majority of
  this service's addressing-dependent tests) now uses the new
  `harness::set_unique_resp_table_and_promote`/`set_can_phys_req_id_and_promote`
  helpers, which additionally issue a `CoptUpdateparam` and wait for it. This
  is a mechanical consequence of the split (matching ADR-067's own
  "Consequences" precedent of needing extra `CoptUpdateparam` steps in
  existing lock tests), not a change to what those tests otherwise verify.
- New tests, `tests/grpc_mock/unique_resp_id_table_binding.rs`: `Get` reads
  Working before promotion; a `CoptSendrecv` uses the OLD Active table until
  a `CoptUpdateparam` promotes the new one; `temp_param_update=1` still reads
  Active, never the staged Working table; a temp COP's completion does not
  wipe a staged Working table; `CoptRestoreParam` copies Active into Working
  with no filter I/O; a `CoptUpdateparam` with an unchanged table does no
  filter churn (diff gate).
- The poll task (`events::poll_channel_events`/`dispatch_tx_item`) now
  receives a cloned `J2534Service` handle (cheap: every field is an `Arc`),
  threaded from `spawn_channel_poll_task`, purely so
  `handle_update_param` can call `promote_unique_resp_id_table` — which needs
  `shared_channels`/`can_channel_mode`/`resolved_can_channel_mode`, state the
  poll task otherwise has no access to.
- `docs/j2534-0404-architecture.md` and
  `j2534-0404-service/docs/implementation-notes.md` are updated alongside this
  ADR to describe the split instead of the single-field model.
- **Two known, accepted best-effort behaviors of `promote_unique_resp_id_table`:**
  - **Hardware-before-memory ordering across separate lock acquisitions.**
    `remove_point_to_point_fc_filters`/`install_point_to_point_fc_filters`
    run (and release `logical_links`) before `active_unique_resp_id_table`
    is written in its own, later lock acquisition. In that narrow window, a
    concurrent poll-task read of the RX routing table (`build_cll_rx_entries`,
    e.g. for a dual-channel-mode UUDT companion channel or software-ISO-TP
    FlowControl addressing) can transiently observe the OLD Active table
    while hardware already carries the NEW filters. This is self-resolving
    (the next `logical_links` read sees the fully-updated Active table) and
    matches this codebase's existing best-effort posture toward
    multi-step, non-atomic state transitions elsewhere (e.g. ADR-008).
  - **Partial hardware failure still promotes Active in full.** Per-entry
    `FLOW_CONTROL_FILTER` installation remains best-effort (ADR-008/ADR-039):
    an individual `PassThruStartMsgFilter` failure is logged and skipped,
    not propagated. `active_unique_resp_id_table` is set to the complete
    `new_table` regardless of whether every entry's filter actually
    installed — identical to the pre-split behavior, where
    `SetUniqueRespIdTable` unconditionally stored the full table in
    `LogicalLinkState` even when some entries' filters failed to install.

See ADR-039 (filter derivation), ADR-043 (lock check), ADR-044
(`CoptUpdateparam` lock check), ADR-067 (the `ComParamSet` split this ADR
mirrors, including the divergence this ADR carves out of §G).
