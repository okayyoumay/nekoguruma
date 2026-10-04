# ADR-139: `handle_channel_hard_error`/`rpc_module_disconnect` Hold `shared_channels` Across Their `cancel_link_cops` Loop

**Date:** 2026-07-27
**Status:** Accepted
**Affects:**
- `j2534-0404-service/src/service/events.rs` (`handle_channel_hard_error`)
- `j2534-0404-service/src/service/rpc_module.rs` (`rpc_module_disconnect`)

## Context

`cancel_link_cops` (`events.rs`) cancels a CLL's in-flight COPs by scanning
`primitives` for every entry whose `entry.cll_handle == cll_handle` and
emitting `PduCopstCancelled` for each. It has no notion of connection
identity beyond the raw `cll_handle` — `CopEntry` (`service.rs`) carries no
`connect_generation` field, unlike `TxItem::StartComm`/`StopComm`/
`SendRecv`/`UpdateParam`/`Delay`/`RestoreParam`, which all gained one across
ADR-086's many rounds specifically to prevent a reconnected session's COP
from being treated as belonging to the connection that just went away.
`cancel_link_cops` was never given the same treatment because its three
callers were assumed to make the underlying race structurally impossible:

- `rpc_destroy_com_logical_link` removes the CLL's `logical_links` entry
  before calling `cancel_link_cops`; the `cll_handle` can never be reused
  (`next_logical_link_handle`'s allocator), so nothing can ever reconnect
  onto it again.
- `rpc_disconnect_com_logical_link` acquires `shared_channels` (`chans`) and
  holds it continuously across its own `cancel_link_cops` call (ADR-080:
  `shared_channels` is the outermost lock). Every path that can publish a
  CLL as `connected = true` on a new `connect_generation`
  (`finalize_connected_link`, for both the primary channel and a UUDT
  companion channel) itself requires `shared_channels`, so a concurrent
  reconnect of the same `cll_handle` cannot complete until this function's
  `cancel_link_cops` call — and the whole teardown — has already finished.

`handle_channel_hard_error` and `rpc_module_disconnect` had neither
protection. Both leave the CLL's `logical_links` entry in place (only
`connected`/`channel_id`/etc. are cleared), so a client can call
`ConnectComLogicalLink` again for the same `cll_handle`. And both released
their `shared_channels` guard (or, for `rpc_module_disconnect`, never
acquired one at all at this point) before running their own per-CLL
`cancel_link_cops` loop:

- `handle_channel_hard_error` locked `shared_channels` and `logical_links`
  together only to snapshot `cll_handles`, mark the affected `SharedChannel`
  entry `dead` (ADR-134), and clear each CLL's connection fields — then
  dropped both guards before looping over `cll_handles` to call
  `send_error_event` → `cancel_link_cops` → `send_cll_status` for each one.
- `rpc_module_disconnect` held only `device_id` (via `lock_device_for`)
  across its own connected-handles snapshot and `cancel_link_cops` loop;
  `device_id` alone does not block a connect past
  `finalize_connected_link`'s point, since that call already drops the
  `device_id` guard before publishing `connected = true`.

### The race

1. The poll task detects a hard channel error on `channel_id` X and enters
   `handle_channel_hard_error`. It marks the matching `SharedChannel` entry
   `dead = true` and clears CLL H's `connected`/`channel_id`/etc., then
   drops `shared_channels`/`logical_links`.
2. Before the per-CLL loop reaches `cancel_link_cops(..., H)`, a client
   calls `ConnectComLogicalLink` for CLL H again. ADR-134's `dead` flag only
   rejects *joining the same, now-dead* `SharedChannel` entry — it does not
   cover a reconnect that lands on a brand-new physical channel (a
   different protocol/baud triggers `spawn_new_shared_channel`, an entirely
   separate map entry). This connect bumps CLL H's `connect_generation`
   (ADR-086) and succeeds; ADR-134's own round-5 accepted residual already
   documents that `module_state` can still read `Ready` for a brief window
   after a hard error starts, since `handle_channel_hard_error` only sets
   `PduModstNotAvail` near the end of its work.
3. The client calls `StartComPrimitive` on the freshly reconnected CLL H.
   `rpc_primitive.rs` inserts a brand-new `CopEntry { cll_handle: H, .. }`
   into `primitives`. `rpc_start_com_primitive` never needs
   `shared_channels` itself to do this — it only needs the CLL to already be
   connected, which the prior step already established.
4. The still-in-flight `handle_channel_hard_error` (from step 1) reaches
   `cancel_link_cops(..., H)`. It matches `primitives` on `cll_handle`
   alone, finds the brand-new COP from step 3, removes it, and emits
   `PduCopstCancelled` for a COP that is legitimately live on the client's
   new session — a session that never asked to be cancelled and has no way
   to know its COP was just silently killed.

`rpc_module_disconnect` has the identical shape: a connect that gets past
`finalize_connected_link`'s `device_id` release can complete and start a COP
before this function's own `cancel_link_cops` loop runs.

This is the COP-level, disjoint-physical-channel counterpart of the
"zombie join" gap ADR-134 closed at the CLL/channel level with
`SharedChannel::dead` — `dead` structurally cannot cover it, since it lives
on the *old* `SharedChannel` entry, and this race is about a *new* one.

## Decision

Widen `shared_channels`'s hold duration in both functions to span their
entire per-CLL `cancel_link_cops` loop, mirroring
`rpc_disconnect_com_logical_link`'s existing discipline exactly, instead of
adding a `connect_generation` field to `CopEntry` and threading an optional
filter through `cancel_link_cops`. Lock-scope widening is preferred here
because the race is not limited to COP cancellation: a reconnect landing in
the same window also gets a spurious `PduErrEvtLostCommToVci` +
`PduCllstOffline` pair emitted for its live new session, and has its
freshly-inserted response-binding registrants wrongly cleared
(`cancel_link_cops`'s `registrants.clear()` step) — a `CopEntry`-only fix
would leave both of those open. Widening the lock that already gates every
`connected = true` publication closes all three at once, with no new field
and no new parameter threaded through `cancel_link_cops`'s three call sites.

**`handle_channel_hard_error`:** `chans` (`shared_channels.lock().await`) is
now acquired once at the top of the function and held continuously through
the per-CLL loop (`send_error_event` → `cancel_link_cops` →
`send_cll_status` for every affected `cll_h`). `logical_links` is still
locked and dropped in its own narrower inner block, unchanged, since nothing
downstream needs it held externally — each callee (`send_error_event`,
`cancel_link_cops`, `send_cll_status`) takes its own `logical_links.lock()`
internally. `chans` is dropped with an explicit `drop(chans)` immediately
after the loop, **before** `module_state` is ever acquired: this crate's
established nested-lock order is `device_id -> module_state ->
shared_channels` (ADR-107 addendum, re-affirmed by ADR-134), so
`shared_channels` must never still be held while acquiring `module_state`.

**`rpc_module_disconnect`:** `shared_channels` is acquired immediately after
`slot` (the `device_id` guard from `lock_device_for`) and held across the
connected/`cll_handles` snapshot plus the `cancel_link_cops` loop, then
dropped with an explicit `drop(chans)` right after the loop — a second,
separate acquisition of `shared_channels` later in the function (unchanged)
still drains the map to close the physical channels. These are deliberately
two independent acquisitions, not one held across the whole function: the
first must release before the `if let Some(...) = slot.take()` block's own
`module_state.lock()` read, for the same ordering reason as above.

## Consequences

- The race is closed by construction: any connect/join/spawn path that can
  publish a CLL as `connected = true` requires `shared_channels`
  (`finalize_connected_link`, both primary and UUDT-companion paths), so a
  reconnect of the same `cll_handle` cannot complete — and therefore cannot
  insert a live `CopEntry` — until the corresponding `cancel_link_cops` call
  (and the rest of the per-CLL loop) has already finished.
- No new lock-ordering edge: `send_error_event`, `cancel_link_cops` (and its
  own `cancel_held_tx_items` sub-call), and `send_cll_status` never acquire
  `shared_channels` or `api` themselves, so holding `chans` across them
  cannot self-deadlock. `poll_rx_inner` (the sole caller of
  `handle_channel_hard_error`) already drops `api` before calling it,
  satisfying ADR-080's "`shared_channels` outermost whenever held alongside
  `api`/`logical_links`" rule.
- Both widened sections are pure in-memory work (`HashMap`/`Vec` operations,
  unbounded-`mpsc` sends, lock acquisitions) with no FFI or blocking I/O, so
  the extra time any *other* `shared_channels` user (a connect on a healthy,
  unrelated channel; a sibling CLL's disconnect) can be made to wait is
  bounded by the affected CLL count — cheaper than the sections ADR-080
  already accepts holding `shared_channels` across (which include native
  `PassThruConnect`/`PassThruClose` calls).
- **Accepted residual, unchanged from ADR-134:** a fresh `ConnectComLogicalLink`
  that passes its `module_state` gate (still `Ready`) but then blocks on the
  now-held `shared_channels` mutex will proceed once this function's loop
  releases it — `module_state` may already read `NotAvail` by then, or may
  not yet if `handle_channel_hard_error` hasn't reached that update. This
  ADR does not change that window (doing so would require holding
  `shared_channels` across the `module_state` write, which the established
  lock order forbids); it only guarantees that whichever COP the reconnect
  goes on to start is never retroactively cancelled by the hard-error/
  module-disconnect sweep that raced it.
- Regression test coverage for this window faces the same construction
  problem documented across ADR-086's many rounds: `handle_channel_hard_error`
  and `rpc_module_disconnect` run to completion within a single `.await`
  chain with no natural preemption point this crate's single-threaded
  (`current_thread`) mock-harness test runtime can interleave a reconnect
  into. No dedicated regression test was attempted for this reason; the
  full existing test suite was confirmed to still pass with both fixes in
  place, and the fix's correctness was independently verified by a
  `design-advisor` review (crate-wide audit of every `shared_channels.lock`
  site, confirming no other caller holds `primitives`/`terminal_cops`/
  `logical_links` across a `shared_channels` acquisition) before being
  implemented.

## Alternatives Considered

1. **Add `connect_generation` to `CopEntry`, give `cancel_link_cops` an
   optional expected-generation filter.** Mirrors ADR-086's `TxItem`-side
   mechanism, but only closes the COP-cancellation leg of the race, leaving
   the spurious CLL-status-event and registrant-clear legs open; would also
   force three callers that need unconditional cancellation (`Destroy`,
   voluntary `Disconnect`, module `Disconnect`) to pass a filter they don't
   need. Rejected in favor of the strictly more complete lock-widening fix,
   which is also the established pattern this codebase already uses for
   every prior instance of this exact gap shape (`Disconnect`/`Destroy`/
   filter teardown).
2. **Extend `shared_channels`'s hold in `handle_channel_hard_error` over the
   `module_state` update too, closing ADR-134's fresh-connect residual as
   well.** Rejected: inverts the documented `device_id -> module_state ->
   shared_channels` nested-lock order (module_state must never be acquired
   while `shared_channels` is held).
3. **Set `module_state = PduModstNotAvail` before acquiring `shared_channels`
   in `handle_channel_hard_error`.** Narrows but does not close ADR-134's
   residual (a connect already past its own `module_state` gate check still
   resumes against a module that has since gone `NotAvail`), while changing
   this function's persisted-status-vs-event ordering for no closure gained.
   Not pursued.
