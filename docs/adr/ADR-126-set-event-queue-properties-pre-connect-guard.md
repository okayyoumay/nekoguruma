# ADR-126: SET_EVENT_QUEUE_PROPERTIES Pre-Connect Guard

**Date:** 2026-07-24
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_misc.rs` (`ioctl_set_event_queue_properties`),
             `j2534-0404-service/src/service/rpc_link.rs` (`rpc_connect_com_logical_link`,
             round 2's `connect_in_flight` token claim),
             `j2534-0404-service/src/service.rs` (`LogicalLinkState::connect_in_flight`,
             `pdu_connect_begun`, round 2)

## Context

Conformance-audit finding A2-5 (`j2534-0404-service/docs/iso22900-2-conformance-audit.md`)
identified that `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES` had no pre-Connect guard at all: any
CLL, connected or not, could call it to reconfigure its live event queue
(`LogicalLinkState::event_queue_cap`/`event_queue_mode`) at any time. ISO 22900-2:2009(E)
§9.5.16 (`vehicle-comm-specs` repo,
`iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md`, line 3620) is explicit: the
`PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES` IOCTL is only permitted before `PDUConnect` has been
called, and on a ComLogicalLink that is already connected the function answers with
`PDU_ERR_CLL_CONNECTED`. A hard connect-state gate of this kind is not the sort of clause
a 2009→2022 edition revision would change, so this finding is treated as stable across
editions without a "verify against 2022" annotation.

With no guard, a client could shrink `event_queue_cap` on a CLL that was actively receiving
traffic, changing eviction behavior underneath in-flight `push_cll_event` calls — a
correctness and conformance gap, not merely a missing validation nicety.

## Decision

`ioctl_set_event_queue_properties` now rejects the call up front when `link.connected == true`,
before anything is mutated, returning `PDU_ERR_CLL_CONNECTED` (`Code::FailedPrecondition`) via
`state_guard_status` — the same helper and error used by this crate's other connect-state
guards (e.g. `rpc_primitive.rs:670-676`'s identical `PduError::PduErrCllConnected`/
`Code::FailedPrecondition` pair for the `CoptStartcomm`-while-already-started guard). The check
reads `link.connected` and `link.last_error` in the **same** `logical_links` critical section
that writes `event_queue_cap`/`event_queue_mode` and clones `rx_buf` — not a separate, earlier
lock acquisition.

**Codex-review fix (round 1): the check and the write must share one critical section, not
two.** The first cut of this guard ran the `connected` check under its own, short-lived
`logical_links.lock()`, released before a second, later `logical_links.lock()` performed the
actual write. That left a TOCTOU window: a concurrent `ConnectComLogicalLink` could acquire the
lock and have `finalize_connected_link` publish `connected = true` in the gap between the two
acquisitions, so the IOCTL's own check would see `connected == false`, release the lock, and
then its later write would proceed unconditionally — reconfiguring (and immediately trimming)
the event queue of a CLL that is connected by the time the change actually applies, exactly the
live-shrink behavior this ADR exists to prevent. Fixed by moving the check inside the same
`links.get_mut(&cll_handle)` block that performs the write, so no other task can observe or
change `connected` between the check and the mutation.

**Codex-review fix (round 2): `connected` alone leaves the whole in-flight-Connect window
open.** §9.5.16 gates on whether `PDUConnect` has been called yet — call time, not
completion time — but `link.connected` only becomes `true` inside `finalize_connected_link`
(`rpc_link.rs:1367-1468`, the write at line 1380), well after `rpc_connect_com_logical_link`
(`rpc_link.rs:1482-1687`) has already accepted the call and run several `.await`s:
`autodetect_sae_j1850_flavor` (line 1499), `ensure_open_device` (line 1537), the actual
`PassThruConnect` hardware call inside `connect_new_physical_channel` (lines 1579-1588), and
`finalize_connected_link`'s own lock acquisitions. Throughout that whole window `connected` is
still `false`, so a concurrent `SET_EVENT_QUEUE_PROPERTIES` call could still slip through even
though the client had already called `PDUConnect`.

Fixed with a `Weak<()>` "connect in flight" token, not a `bool`. `LogicalLinkState` gains
`connect_in_flight: std::sync::Weak<()>`; `rpc_connect_com_logical_link` installs it in a new,
first block (before any `.await`, run before the pre-existing `autodetect_sae_j1850_flavor`
call): under one `logical_links` lock acquisition, an unknown handle errors as usual, an
already-`connected` CLL takes the pre-existing idempotent no-op (`return Ok(...)`, moved here
from its old location), a live `connect_in_flight.upgrade()` rejects with
`PDU_ERR_CLL_CONNECTED` (a second concurrent Connect attempt on the same handle), and otherwise
an `Arc::new(())` is created, downgraded into `link.connect_in_flight`, and the owning `Arc` is
returned and bound to a local (`_connect_token`) that lives for the rest of the RPC handler's
stack frame. `LogicalLinkState` gains a helper, `pdu_connect_begun(&self) -> bool { self.connected
|| self.connect_in_flight.upgrade().is_some() }`; `ioctl_set_event_queue_properties`'s guard
condition changes from `link.connected` to `link.pdu_connect_begun()` — same single critical
section, same error, only the condition widens.

A `Weak` token, rather than a `connecting: bool` explicitly cleared on every exit path, was
chosen specifically because it is cancellation-safe: `rpc_connect_com_logical_link` has several
distinct exit paths before `connected = true` is written (an unknown-handle error, `ensure_open_
device` failing, the shared-channel-filtered rejection, `connect_new_physical_channel` failing,
and `finalize_connected_link`'s own ADR-012 destroyed-mid-connect rollback), and a client can
retry `ConnectComLogicalLink` after any of them — nothing in the code prevents it, and
`connected` correctly stays `false` on every failure. A `bool` would need an explicit clear on
every one of those paths, replaying exactly the class of bookkeeping bug the round-1 TOCTOU fix
above was about; worse, if the gRPC runtime ever drops the RPC's future mid-`.await` (a client
cancelling the call, a dropped connection), a `bool` set-then-clear pattern has no code path left
to run the clear at all, permanently stranding the flag `true` and permanently rejecting both
this guard and every future Connect attempt for that CLL. The `Weak` token needs no clear
anywhere: its owning `Arc<()>` is a plain local, and Rust drops it — making
`connect_in_flight.upgrade()` return `None` again — on every one of those exits automatically,
success or failure, `?` or future-cancellation alike.

**Closing the window also serializes two concurrent `ConnectComLogicalLink` calls for the same
`cll_handle`, previously unguarded.** Before this fix, nothing prevented two concurrent Connect
RPCs for one handle from both running past the (single) `connected` check and racing each other
through hardware setup — able to double-bump a shared channel's `ref_count` (leaked on a later
single disconnect) or issue two `PassThruConnect`s for the same CLL. The token's install-time
check (a live `connect_in_flight` rejects a second concurrent attempt) closes this as a direct
side effect of the same mechanism, not a separate fix.

**The pre-existing immediate-trim-on-lower-cap logic (`while queue.items.len() > new_cap {
queue.items.pop_front(); }`) is retained, not removed.** It would be tempting to assume the new
guard makes a live-trim scenario unreachable — every trim would then happen on an already-empty
or fresh `rx_buf` — but that is not the case. A CLL can be connected, receive frames into
`rx_buf`, then get `DisconnectComLogicalLink`'d: disconnect sets `connected = false` and clears
`channel_id` (`rpc_link.rs:2521-2522`) but does **not** clear `rx_buf`. Calling
`SET_EVENT_QUEUE_PROPERTIES` in that disconnected-but-still-holding-stale-data state is legal
under the new guard, and the immediate trim must still apply — otherwise a client that
disconnects, lowers the cap, and reconnects would find `rx_buf` still over the new cap. This is
why the existing trim-behavior regression test was restructured around a disconnect (see
Consequences below) rather than deleted.

## Alternatives Considered

1. **Guard on some CLL state other than `connected`.** Rejected: `connected: bool` on
   `LogicalLinkState` is exactly what the spec's "prior to calling PDUConnect" / "already
   connected" language maps to, and is the same field every other connect-state guard in this
   crate already keys off of (e.g. `rpc_primitive.rs`'s pre-primitive connected check).
   Introducing a second, parallel notion of "connected enough to reject this IOCTL" would be an
   unmotivated divergence.
2. **Delete the immediate-trim-on-lower-cap logic, on the assumption that a live shrink is no
   longer reachable once the guard exists.** Rejected: as described in Decision above, the trim
   is still reachable via a disconnect → reconfigure → reconnect sequence, since disconnect does
   not clear `rx_buf`. Deleting it would silently reintroduce the original A2-5-adjacent
   post-fix, and the fix (once trimmed) would degrade to lazy, per-push eviction, exactly the
   behavior the pre-existing trim logic was added to avoid (see the PR #80 round-3 regression
   this logic traces back to).
3. **A plain `connecting: bool`, explicitly set at RPC entry and cleared on every exit path.**
   Rejected (round 2): not cancellation-safe. `rpc_connect_com_logical_link` has multiple
   distinct exit paths before `connected = true` is written, and a client can retry Connect
   after any failure; a `bool` needs a correct clear on every one of them, and has no path left
   to run that clear at all if the gRPC runtime drops the RPC future mid-`.await` (client
   cancellation, dropped connection) — permanently stranding the CLL as "connecting forever,"
   rejecting both this guard and all future Connect retries. A `Weak<()>` token self-heals on
   every exit, including cancellation, with no explicit clear anywhere.
4. **A drop-guard type that `tokio::spawn`s a task to clear a `bool` on drop.** Rejected (round
   2): achieves the same cancellation-safety as the `Weak` token but with strictly more
   machinery (a spawned task, its own lifetime to reason about) for an equivalent guarantee.
5. **A per-`cll_handle` `Mutex`/semaphore serializing the whole `ConnectComLogicalLink` call.**
   Rejected (round 2): heavier than the finding requires, changes Connect's concurrency story
   more broadly than closing this one guard's window needs, and still requires a separate,
   visible "Connect has begun" signal for `ioctl_set_event_queue_properties` to check — the
   `Weak` token already is that signal, more cheaply.
6. **Set `connected = true` early (at RPC entry) and roll it back on failure**, instead of
   introducing a second field. Rejected (round 2): `connected` is read by many other call sites
   as "this CLL has a live, usable physical channel" (e.g. `rpc_primitive.rs:649`,
   `events.rs:2648`) — setting it early would falsify all of those for the whole connecting
   window, an unacceptably broad blast radius for closing one IOCTL's guard.

## Consequences

- **Client-visible behavior change:** a client that previously could reconfigure
  `SET_EVENT_QUEUE_PROPERTIES` at any time, including post-connect, now receives
  `PDU_ERR_CLL_CONNECTED` (`Code::FailedPrecondition`) if it attempts to do so on a connected
  CLL, and must `DisconnectComLogicalLink` first.
- Five pre-existing tests in `j2534-0404-service/tests/grpc_mock/pdu_ioctl.rs` were restructured
  to keep exercising their original scenarios under the new pre-Connect-only constraint:
  - `subscribe_only_limited_mode_delivers_every_frame_live_with_no_loss`
  - `no_subscriber_limited_mode_drops_per_push_cll_event_semantics`
  - `backlog_evicted_then_subscribe_emits_exactly_one_lost_then_drains_fifo`
  - `backlog_discarded_then_subscribe_emits_exactly_one_lost_then_drains_fifo`

  Each of these four now creates its CLL without connecting (`create_cll`), applies the same
  ComParams `create_and_connect_cll` would have applied, calls
  `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES` while still unconnected, then connects
  (`ConnectComLogicalLink`) before continuing — `event_queue_cap`/`event_queue_mode` are not
  touched by `finalize_connected_link`/`rpc_connect_com_logical_link`, so Connect does not
  disturb the values set pre-connect.

  - `set_event_queue_properties_trims_rx_buf_immediately_when_lowering_the_cap` now connects
    normally, injects traffic, disconnects, and only then lowers the cap — exercising the
    disconnect-then-shrink path described in Decision above, instead of a live-connected shrink.
    The core assertion (the trim happens immediately, not lazily on the next push) is unchanged.
- A new regression test, `set_event_queue_properties_rejects_when_connected`, asserts the guard
  itself: connects a CLL via `create_and_connect_cll`, calls
  `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`, and asserts the call fails with
  `Code::FailedPrecondition` and `PduError::PduErrCllConnected` (via `error_detail_from_status`,
  the pattern already used elsewhere in this test suite for asserting a specific `PDU_ERR_*`
  from a failed RPC).
- **The Codex-review round-1 race (check-then-separately-write) is closed structurally, not by a
  new regression test.** Forcing the exact interleaving — a concurrent `ConnectComLogicalLink`
  landing in the gap between two separate `logical_links` lock acquisitions — would require a
  test-only pause hook this codebase's mock harness does not have (same
  regression-test-feasibility precedent as ADR-123's Findings A/E/G). The fix is verified by the
  code-structure argument instead: both the check and the write now execute while holding the
  single `logical_links` lock acquired for the write, so no other task can observe or mutate
  `connected` in between.
- **Round 2 (`connect_in_flight`): the full concurrency race is closed structurally, not by a
  forced-interleaving regression test — but the token's own toggle logic IS unit-tested.**
  Confirmed: the mock harness's only existing hold/pause hook (`__mock_arm_write_rx_injection`'s
  `hold_ms`) fires inside `PassThruWriteMsgs`, not `PassThruConnect` — `j2534-0404-mock`'s
  `PassThruConnect` implementation has no hold point of any kind, so nothing can currently force
  a concurrent `SET_EVENT_QUEUE_PROPERTIES` call to land between this RPC claiming
  `connect_in_flight` and `finalize_connected_link` publishing `connected = true`. Adding one
  would be a mock-infrastructure change, out of scope for this fix (same class of deferral as
  ADR-123's Findings A/E/G); that specific gap is closed by the code-structure argument instead
  — the token is claimed under the same `logical_links` lock used by every reader of
  `connected`/`connect_in_flight`, and its `Weak`/`Arc` pairing self-heals on every exit path
  (see Decision above). **`edge-case-hunter` follow-up:** that argument doesn't excuse leaving
  `pdu_connect_begun()`'s own OR-condition logic untested, though — a plain, synchronous unit
  test (`rpc_link.rs::tests::pdu_connect_begun_reflects_the_connect_in_flight_token`, no async
  runtime or mock cdylib needed) now confirms it correctly reads a live `connect_in_flight` as
  "begun," correctly reverts to `false` once the owning `Arc` drops with no explicit clear
  anywhere, and correctly treats `connected: true` as an independent, sufficient condition on its
  own. `set_event_queue_properties_rejects_when_connected` continues to cover the
  fully-`connected`-via-the-gRPC-IOCTL-call case unmodified.
- **Accepted residual (round 2): the `connect_in_flight` token outlives `finalize_connected_link`
  by a few more `.await`s.** `rpc_connect_com_logical_link` keeps doing work after `connected =
  true` is published — installing ISO15765 flow-control filters, `probe_can_channel_mode`,
  `ensure_uudt_companion_channel` (`rpc_link.rs:1627-1686`) — and the `_connect_token` local
  isn't dropped until the whole function returns, at the very end of that tail. A
  `DisconnectComLogicalLink` landing in that narrow tail window would find `connected == false`
  again but `pdu_connect_begun()` still `true` (the token not yet dropped), so
  `SET_EVENT_QUEUE_PROPERTIES` stays rejected for a few more milliseconds than strictly
  necessary. Conservative and spec-defensible ("a PDUConnect is in progress" remains
  true-in-spirit for this tail), not fixed.
- **Latent gaps noticed while designing this fix, out of scope here, recorded in
  `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog rather than fixed in
  this PR:** an RPC-cancellation channel-ref leak in `rpc_connect_com_logical_link`'s
  `spawn_new_shared_channel`/ref-count-bump window, and a `SetComParam`-during-Connect silent-op
  window in `finalize_connected_link`'s Working-set snapshot. See the backlog entries for the
  full description of each.
- **A pre-existing, separately-tracked latent gap closes as a side effect, discovered during an
  adversarial re-review of this PR's own diff (Codex review was unavailable at the time due to
  an account usage-limit block, so this review substituted for it).** An `edge-case-hunter`
  finding from earlier in this PR's own review loop (before round 2 existed) had flagged
  `autodetect_sae_j1850_flavor`'s write-back (`rpc_link.rs:2276-2308`) as reachable by a stale
  write if two concurrent `ConnectComLogicalLink` calls for the same handle interleaved just
  right. That exact interleaving requires two concurrent Connects on one handle — precisely what
  `connect_in_flight`'s install-time check now prevents (see above). The finding is marked
  **CLOSED** in `implementation-notes.md`'s backlog rather than left open, since fixing this ADR's
  own target scenario incidentally fixed it too — a smaller, positive instance of the same
  "verify the fix's actual blast radius before closing a review round" discipline the two Codex
  rounds on this PR already exercised.
- Reference: finding A2-5,
  `j2534-0404-service/docs/iso22900-2-conformance-audit.md`.
