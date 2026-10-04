# ADR-115: `PDU_EVT_DATA_LOST` Emission on CLL Event-Queue Overflow

**Date:** 2026-07-23 (corrected same-PR, round 2, 2026-07-23; corrected again
same-PR, round 3, 2026-07-23; corrected again same-PR, round 4, 2026-07-23;
corrected again same-PR, round 5, 2026-07-23; corrected again same-PR, round
6, 2026-07-23; corrected again same-PR, round 7, 2026-07-23)
**Status:** Accepted
**Affects:**
- `j2534-0404-service/src/service/events.rs` (`push_cll_event`, `PushOutcome`
  enum, `make_lost_notification`, `cll_queue_item_to_event_item`,
  `notification_from_event_item`, `deliver_or_enqueue` -- round 6: dropped its
  subscriber parameter entirely, reads `queue.live_sender` fresh instead, adds
  the send-failure self-heal --, `send_error_event`, `poll_rx_inner`,
  `handle_start_comm`, `build_cll_rx_entries` -- round 6: no longer touches
  `subscriptions` at all --, `CllRxEntry` -- round 6: `sub` field removed --;
  `deliver_or_enqueue_live_sender_tests`, round 6, renamed and rewritten from
  `deliver_or_enqueue_generation_tests`)
- `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_get_event_item`,
  now calling the shared `events::cll_queue_item_to_event_item`;
  `rpc_subscribe_event`, round 4, round 6: writes `live_sender` instead of a
  generation; `reconcile_stale_cll_subscription`, round 6: `same_channel`
  identity instead of generation equality;
  `rpc_subscribe_event_live_sender_tests`, round 6, renamed and rewritten
  from `rpc_subscribe_event_generation_tests`, plus a new
  `terminate_subscription_clears_live_sender_and_stops_future_delivery` test)
- `j2534-0404-service/src/service/rpc_misc.rs`
  (`ioctl_set_buffer_size`'s doc comment; `ioctl_reset`/`ioctl_clear_rx_queue`/
  `ioctl_set_event_queue_properties`'s `rx_buf.items` accesses, round 4)
- `j2534-0404-service/src/service/rpc_link.rs`
  (`rpc_create_com_logical_link`'s queue construction, round 4, round 6:
  seeds `live_sender` instead of a generation)
- `j2534-0404-service/src/service.rs` (`CllEventQueue`, round 6: `live_sender:
  Option<SubscriptionSender>` replaces `live_sub_generation: u64`;
  `SubscriberRef` and `J2534Service::next_cll_sub_generation`, round 4,
  removed entirely round 6 -- `subscriptions`'s value type reverts to plain
  `SubscriptionSender`; `terminate_subscription`/`terminate_all_subscriptions`,
  round 6: clear the backing queue's `live_sender` on removal; round 7: take
  the affected queue `Arc`(s) as a parameter from the caller instead of
  re-deriving them from `logical_links`, since round 6's self-lookup was dead
  code at both real call sites)
- `j2534-0404-service/src/service/rpc_module.rs` (`rpc_module_disconnect`,
  round 7: captures every CLL's queue `Arc` before clearing `logical_links`,
  passes the map to `terminate_all_subscriptions`)
- `j2534-0404-service/docs/implementation-notes.md` (backlog note for
  `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`'s trim path; round 5 test-coverage
  residual note, round 6: updated for the renamed tests; round 7: new
  backlog note for the untested `ModuleDisconnect` path)
- `j2534-0404-service/tests/grpc_mock/pdu_ioctl.rs`,
  `cop_ctrl_cycles.rs`, `startcomm_comparam.rs`, `harness.rs`
- `docs/j2534-0404-architecture.md` (lock-order table, `LogicalLinkState::rx_buf`
  doc, round 6)

## Context

Conformance audit finding A2-6
(`j2534-0404-service/docs/iso22900-2-conformance-audit.md`) identified that
`j2534-0404-service` never generated ISO 22900-2's `PDU_EVT_DATA_LOST` event.
Per ISO 22900-2:2009(E) §9.5.16
(`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:3634`),
a ComLogicalLink that reaches a queue-full state generates a dedicated
`PDU_EVT_DATA_LOST` event instead of a normal result item — no `PDU_IT_ERROR`
(or any other) item is ever placed on the event queue for the drop itself.

D.1.8 (line ~6383-6385) reinforces this is a callback-only, information-only
signal that carries no event data of its own.

Prior to this change, `push_cll_event` (`events.rs`) silently dropped an
item on overflow — either evicting the oldest buffered entry
(`OverwriteOldest` mode) or discarding the new item
(`DiscardNewest`/`Limited` mode) — with zero signal to any client. A
`GetEventItem`-polling client and a `SubscribeEvent`-subscribed client both
had no way to learn data had been lost.

### Why this isn't a straightforward "add the event" fix

`iso22900-service` (the native ISO 22900 D-PDU API adapter) already emits an
analogous `EventNotificationData::Lost(LostEventItemNotification {})` for
its own `DataLost` signal
(`iso22900-service/src/service/events.rs:158-164`). That precedent doesn't
transplant directly: `iso22900-service`'s live notification is produced by
literally draining the native D-PDU API's own event queue on demand
(`get_event_item` inside `run_event_subscription_task`), so there is exactly
one queue and a live subscriber necessarily observes the same overflow a
polling client would.

`j2534-0404-service` has two structurally independent delivery paths for one
incoming item: `push_cll_event` maintains `entry.rx_buf`, the bounded queue
`GetEventItem` (`rpc_primitive.rs::rpc_get_event_item`) drains via
`pop_front()`; separately, a live `SubscribeEvent` subscriber (an
`mpsc::UnboundedSender`, if attached) receives that item's own notification
unconditionally, independent of `rx_buf`'s capacity or eviction — a design
predating this ADR. Because that channel is unbounded, a subscribed client
never drops anything on its own stream; only a `GetEventItem`-only poller
(no live subscription) draining `rx_buf` later can actually experience loss
from `rx_buf` overflow.

A first implementation attempt sent `Lost` to the `SubscribeEvent`
subscriber unconditionally on every drop, while still sending that dropped
item's own full notification unconditionally too — for `DiscardNewest`, this
meant a subscriber received `Lost` immediately followed by the complete
payload of the very item just declared lost, self-contradicting the signal.
An `edge-case-hunter` verification pass caught this by running the pass's
own new test and observing the payload actually delivered.

The shared proto's `EventItem.data` (the `GetEventItem` wire type) has no
`Lost` variant — confirmed against `vci-service-interface`'s proto
definitions — so extending `GetEventItem` to carry this signal to the
poll-only audience who genuinely experiences the loss was considered and
rejected (see Decision below): per §9.5.16/D.1.8 above, the spec defines
`PDU_EVT_DATA_LOST` as a callback-channel-only signal carrying no event data
of its own, which structurally cannot appear as a queue item, matching why
native `T_PDU_IT` has no Lost item type either.

## Decision

`PDU_EVT_DATA_LOST` is emitted as an **edge-triggered, per-drop,
`SubscribeEvent`-only notification**, using the existing shared proto
`EventNotificationData::Lost(LostEventItemNotification {})` variant
(already used by `iso22900-service`, no proto change needed).

`push_cll_event` returns a `PushOutcome` enum instead of a bare `bool`,
distinguishing the two ways a drop can happen because they imply different
delivery obligations for the item's own notification, not just whether
`Lost` is owed:

```rust
pub(super) enum PushOutcome {
    /// Inserted without needing to make room. No `Lost` notification.
    Inserted,
    /// `OverwriteOldest`: evicted the oldest buffered entry, then inserted
    /// `item`. Send `Lost` and `item`'s own notification.
    Evicted,
    /// `DiscardNewest`: buffer was already at `cap`; `item` was discarded,
    /// not inserted. Send `Lost` only -- never `item`'s own notification.
    Discarded,
}
```

**Original (round 1) delivery rule — since corrected, kept for context, see
"Correction (round 2)" below** — an item's own live notification was sent
**iff it was actually inserted into `rx_buf`**:

- **`DiscardNewest`/`Limited` mode** (`PushOutcome::Discarded`): send
  exactly one `Lost` notification **instead of** the item's own
  notification. The discarded item's payload never reaches the
  `SubscribeEvent` stream — this now genuinely matches D.1.8's
  no-event-data-stored rule for the one audience that *can* observe it.
- **`OverwriteOldest` mode** (`PushOutcome::Evicted`): send one `Lost`
  **plus** the newly-arrived item's own notification. Nothing to suppress —
  the new item was genuinely delivered; `Lost` marks the loss of the older,
  evicted item from the poll queue (which a live subscriber already received
  in real time at its own arrival, so this is informational-only
  over-delivery, not a contradiction).
- **`Inserted`**: unchanged, just the item's own notification.

Applied identically at all three call sites that push onto a CLL's event
queue: `poll_rx_inner`'s ordinary frame fan-out, `handle_start_comm`'s
synthetic fast-init response frame, and `send_error_event`'s async error
events. `Lost` is sent first, in the same critical section as the
call site's own notification, so it can never be reordered relative to it on
that subscriber's stream.

`send_error_event`'s unconditional `last_error`/`TrackedError` snapshot
write (the ADR-112 RPC-fallback mechanism) is **not** gated on
`PushOutcome` — it is not a queue item, and the RPC-fallback path is
intentionally left out of this ADR's scope entirely (see below).

**Scoped per-CLL**, matching where `rx_buf`/`entry.sub` live
(`LogicalLinkState`).

### Correction (round 2, same PR, Codex-review regression + design-advisor fix)

A Codex review of the round-1 diff above (still on the same open PR, before
merge) found a real regression: gating an item's own live notification on
`PushOutcome::inserted()` (whether `push_cll_event` put it in `rx_buf`) only
makes sense if `rx_buf` is *also* what a subscribed client eventually
drains from — but round 1 left `push_cll_event` inserting into `rx_buf`
**unconditionally**, regardless of whether a live subscriber existed, while
*separately* still sending the live notification independently, never
draining the item back out. For a subscribe-only client (never calls
`GetEventItem`), `rx_buf` fills to `event_queue_cap` and stays full forever:
`Circular`/`OverwriteOldest` mode then emits a false `Lost` before every
subsequent frame permanently (the frame is still delivered, but the `Lost`
signal is spurious), and `Limited`/`DiscardNewest` mode permanently stalls
all further live delivery to that subscriber (`PushOutcome::inserted()`
never becomes true again once `rx_buf` is pinned at `cap`).

**Corrected rule: the CLL event queue has single-consumer semantics.** A
live, healthy `SubscribeEvent` subscriber IS the drain — matching the
proto's documented `SubscribeEvent` contract
(`vci-service-interface/src/proto/service.proto`'s `SubscribeEventRequest`
doc comment: "drains all pending event items via `GetEventItem` and streams
them back") and `iso22900-service`'s own precedent
(`run_event_subscription_task`, which literally calls `get_event_item()` to
pull from the same queue it streams out — the exact behavior this ADR's
original Context section noted `j2534-0404-service` structurally lacked).
`rx_buf` now only ever accumulates backlog while nobody is actively
consuming it live.

At each of the three call sites (unchanged from round 1: `poll_rx_inner`'s
frame fan-out, `send_error_event`, `handle_start_comm`'s fast-init synthetic
frame), delivery now goes through `events::deliver_or_enqueue`:

1. **No subscriber registered**: unchanged from round 1 (and from the
   pre-ADR-115 baseline) — `push_cll_event` per the CLL's configured queue
   mode/cap. No `Lost` is ever sent — there is no live channel to receive it
   (the "What is deliberately not covered" section below, unchanged).
2. **A subscriber IS registered**: under `rx_buf`'s lock, first drains any
   existing backlog, FIFO (`pop_front`), converting each item
   (`events::cll_queue_item_to_event_item` — the same conversion
   `rpc_get_event_item` uses for `GetEventItem`, now factored out and
   shared, including `PDU_IOCTL_SET_BUFFER_SIZE`'s `result_buffer_limit`
   truncation and the ADR-051 header/footer `extra_info` split) and sending
   it live. A failed send (receiver dropped — deterministic for
   `mpsc::UnboundedSender`) re-queues that item at the FRONT of `rx_buf` and
   stops draining, falling through to (3) for the new item too. On a fully
   successful drain, the new item's own live notification is sent the same
   way; on success it is delivered live, full stop — **never** written to
   `rx_buf` at all (this supersedes round 1's `inserted()`-gates-delivery
   rule: `push_cll_event` is no longer even called on this path).
3. **Send failed** (no live subscriber reached the item, or a send just
   failed): falls back to `push_cll_event` exactly like (1), and still
   attempts `Lost` on a reported drop (a no-op if the channel is dead, kept
   for symmetry/future-proofing).

`PushOutcome`/`push_cll_event` themselves are unchanged as the queue-mode/
cap mechanism (cases 1 and 3 above still use them); `PushOutcome::inserted()`
was removed as dead code — no caller reaches `push_cll_event` at all until
after the new item's own live-delivery attempt has already been decided (and,
in the fallback path, already failed), so gating a *second* send decision on
it no longer has a caller.

**Net effect**: a subscribe-only client now receives every item live
indefinitely, with `rx_buf` staying empty the whole time it is actively
draining — no false `Lost`, no stall. `GetEventItem` while a live
subscription is active will now typically find `rx_buf` empty (everything
already streamed out live) — this is the proto-documented contract, not a
bug (see `docs/rpc-api-guide.md`'s `SubscribeEvent` section); a mixed
poll+subscribe client naturally gets everything via the live stream instead
of double-delivery. A poll-only client (never subscribes) is completely
unaffected.

### Correction (round 3, same PR, second Codex-review regression + design-advisor fix)

A second Codex review (round 3, still the same open PR) found that round 2's
"live send first, `push_cll_event` only as a fallback" design made
`push_cll_event` — and hence `Lost` — **permanently unreachable dead code**
for any live, healthy subscriber. `SubscriptionSender` is an
`mpsc::UnboundedSender`; its `send()` only ever fails once the receiver is
permanently gone (a genuinely transient failure does not exist for this
channel type). So round 2's "try the live send first, fall back to
`push_cll_event` only if that fails" order meant the fallback branch — the
only place `push_cll_event`/`Lost` could run — was never entered while a
subscriber was alive and draining. Verified empirically: pushing 10 items
through round 2's `deliver_or_enqueue` with a live receiver attached
delivered all 10 live, with **zero** `Lost` notifications, even at
`event_queue_cap = 1`. The entire A2-6 feature was dead code for the
duration of any live subscription.

**Corrected rule: `push_cll_event` runs first, unconditionally, on every
`deliver_or_enqueue` call — regardless of whether a subscriber exists.**
Only after that does a live subscriber, if any, get an opportunistic chance
to drain `rx_buf`'s entire current contents out live, FIFO, in the same
critical section (no `.await` gap between the push and the drain):

```rust
let mut buf = rx_buf.lock().await;
let outcome = push_cll_event(&mut buf, event_queue_cap, event_queue_mode, item);
let Some(tx) = tx else { return; };
if outcome.lost() {
    let _ = tx.send(Ok(make_lost_notification(cll_handle)));
}
while let Some(front) = buf.pop_front() {
    let n = notification_from_event_item(
        cll_handle, cll_queue_item_to_event_item(cll_handle, result_buffer_limit, &front));
    if tx.send(Ok(n)).is_err() { buf.push_front(front); break; }
}
```

This collapses round 2's three-case structure (no subscriber / subscriber
present / send failed) into one uniform path: `push_cll_event` first,
unconditional; drain second, only if a subscriber exists, stopping at the
first failed send and leaving the remainder (which may include the item
just pushed) in `rx_buf` for a later poll or drain attempt. Round 2's
`delivered_live` flag, its duplicated item-conversion inline logic, and its
separate single-item fallback-push special case are all subsumed by this one
drain loop.

**Why this fixes all three rounds' findings at once:**

- **Round 3's own finding (this correction)**: `push_cll_event` is evaluated
  on every single push, so `Lost` is genuinely reachable again — specifically
  when the queue was already full *at insert time* (a backlog had built up:
  either no subscriber was ever attached, or a prior drain left items
  stranded in `rx_buf` after a failed send).
- **Round 2's original complaint (false `Lost`/permanent stall)**: since
  every push immediately triggers a full-buffer drain in the same critical
  section, a healthy/keeping-up subscriber's `rx_buf` oscillates
  empty -> one-item -> empty every cycle — it never gets stuck full, so
  `Lost` does not spuriously fire and delivery never permanently stalls.
- **Round 1's original self-contradiction** (declaring an item lost while
  also delivering its exact payload): still structurally impossible —
  `DiscardNewest`'s discarded item never enters `buf`, so it is never in the
  drain loop to be delivered; `OverwriteOldest`'s `Lost` corresponds to the
  genuinely-evicted older item, and the item that replaced it is delivered
  normally via the same drain loop, not conflated with the `Lost` signal.

`send_error_event`'s unconditional `last_error`/`TrackedError` snapshot
write is untouched by this correction, exactly as in every prior round.

Applied identically at all three call sites (`poll_rx_inner`,
`send_error_event`, `handle_start_comm`), unchanged in structure from round
2: the subscriber sender is still cloned from the subscriptions map before
(or alongside) taking the `rx_buf` lock, preserving the established lock
ordering (`logical_links` -> release -> `subscriptions` -> release ->
`rx_buf`, never nested).

`j2534-0404-service/tests/grpc_mock/pdu_ioctl.rs` gained a new test,
`backlog_evicted_then_subscribe_emits_exactly_one_lost_then_drains_fifo`,
covering the transition case: a backlog builds up with no subscriber
attached (some entries evicted per cap, with no live channel to report
`Lost` to at the time), a subscriber then attaches, and the next push
crosses the cap again — this time with a live channel, so it reports exactly
one `Lost`, followed by the full surviving buffer draining live in FIFO
order, after which further traffic delivers clean. The round-2 test
`subscribe_only_limited_mode_delivers_every_frame_live_with_no_loss` still
passes, now for the *correct* reason — nothing is ever actually dropped
under a keeping-up live subscriber, not because `Lost` is structurally
unreachable (round 2's bug).

### Correction (round 4, same PR, third Codex-review finding + design-advisor fix)

A third Codex review (round 4, still the same open PR) found a real, distinct
race in round 3's `deliver_or_enqueue`, independent of the round 1-3 findings
above: `rpc_subscribe_event` replaces a CLL's subscription by swapping the
`subscriptions` map entry and sending a `Cancelled` `Err` down the OLD
`SubscriptionSender` -- but this does not, and cannot, *close* that old
`mpsc::UnboundedSender`. The receiver half is owned by tonic's stream
plumbing, not by this service, and an `UnboundedSender` has no API to force
its paired receiver to stop existing; the old client's stream merely stops
being *read* once it observes the `Cancelled` item.

`deliver_or_enqueue` (round 3) clones the subscriber's sender from the
`subscriptions` map once, before taking the `rx_buf` lock, and holds that
clone for its whole push-then-drain duration. `rpc_subscribe_event` only ever
touches the `subscriptions` lock, never the `rx_buf` lock -- so it can run
fully concurrently with an in-flight `deliver_or_enqueue` call. If a
subscription is replaced in that window, the drain loop's `tx.send()` on the
now-stale clone still returns `Ok` (the channel is orphaned, not closed) --
and `deliver_or_enqueue` treats `send() == Ok` as "genuinely delivered, safe
to pop permanently". The item is popped from `rx_buf` and gone: invisible to
the new subscriber and unrecoverable via `GetEventItem`. This is a
server-induced loss distinct from every round 1-3 finding, all of which
concerned when/whether `Lost` fires for a *single, stable* subscription --
this one concerned a captured sender silently outliving the subscription it
was captured from.

**Corrected rule: a generation counter lives inside the SAME mutex the drain
loop already holds, and subscription replacement writes it under that same
lock.** This serializes replacement against any in-flight drain -- the drain
loop already holds the queue lock for its whole duration with no `.await`
inside it -- so a single post-lock-acquisition validation is provably
sufficient; no per-item revalidation is needed, and no new lock-ordering risk
is introduced (nothing ever nests the `subscriptions` lock and the queue
lock).

Concretely:

1. **`LogicalLinkState::rx_buf`'s type changes** from
   `Arc<Mutex<VecDeque<CllQueueItem>>>` to `Arc<Mutex<CllEventQueue>>`, where

   ```rust
   pub(super) struct CllEventQueue {
       pub(super) items: VecDeque<CllQueueItem>,
       pub(super) live_sub_generation: u64,
   }
   ```

   bundles the backlog with the generation of whichever subscription
   currently counts as this CLL's live one. Every existing `rx_buf` access
   (`rpc_get_event_item`, `ioctl_reset`/`ioctl_clear_rx_queue`/
   `ioctl_set_event_queue_properties`'s trim loop, `deliver_or_enqueue`, and
   every test constructor) now goes through `.items` instead of dereferencing
   the `VecDeque` directly.

2. **A new global counter**, `J2534Service::next_cll_sub_generation:
   Arc<AtomicU64>` (placed alongside `device_epoch`, the other service-wide
   atomic counter), starts at `1` -- `0` is a reserved sentinel meaning "no
   CLL-keyed subscription has ever touched this queue" (a freshly-created
   `CllEventQueue`'s default, and the value an unused module-/system-handle
   `SubscriberRef::generation` always carries). Global rather than per-CLL, so
   a generation stays unambiguous across a CLL's destroy/recreate and handle
   reuse.

3. **`J2534Service::subscriptions`'s value type changes** from
   `SubscriptionSender` to `SubscriberRef { tx: SubscriptionSender,
   generation: u64 }`. Every capture site that used to clone just the sender
   (`build_cll_rx_entries`/`poll_rx_inner`'s frame fan-out, `send_error_event`,
   `handle_start_comm`'s fast-init synthetic frame -- the same three call
   sites round 1-3 already touched) now clones the whole `SubscriberRef`,
   under the same `subscriptions` lock acquisition as before.

**Lock order invariant, stated once here and documented at the field level
(`J2534Service::logical_links`'s own doc comment, `service.rs`)**:
`logical_links` -> `subscriptions` -> a per-CLL queue lock
(`CllEventQueue`'s own `Mutex`, i.e. `LogicalLinkState::rx_buf`). Any site may
skip levels; never reverse an order two of these locks are held in together.
This was already the order `build_cll_rx_entries` (`events.rs`) and
`deliver_or_enqueue` (`events.rs`, holding a queue lock for its whole
push-then-drain duration) established; points 4 and 5 below are written to
honor it, not to introduce it.

4. **`rpc_subscribe_event`** (CLL-keyed handle case only -- module-/
   system-handle subscriptions are unaffected, see point 6): draws a fresh
   generation `G`, then:
   1. **Pre-resolves the queue `Arc`** *before* touching `subscriptions`:
      locks `logical_links`, clones the CLL's queue `Arc` if the link
      exists (`None` otherwise), drops that guard immediately.
   2. **One critical section under `subscriptions`** -- this is the actual
      atomicity fix: locks `subscriptions`, inserts `(tx, G)` (capturing
      whatever `SubscriberRef` this displaces), and -- if step 1 found a
      queue -- locks that SAME queue *while still holding the
      `subscriptions` guard* (order: `subscriptions` -> queue, per the
      invariant above) and stamps `live_sub_generation = G` into it, then
      drops the queue guard, then drops the `subscriptions` guard. Because
      every concurrent `SubscribeEvent` call for the same or a different key
      serializes through this one `subscriptions` lock acquisition, "decide
      who is in the map" and "stamp their queue" are atomic with respect to
      one another -- two racing calls for the same `cll_handle` can never
      leave the map's stored generation and the queue's stamped generation
      disagreeing, regardless of interleaving order.
   3. After dropping the locks from step 2, sends `Cancelled` to whatever
      `SubscriberRef` was displaced.
   4. **Reconciliation, run UNCONDITIONALLY** -- not only when step 1 found
      no queue (an interim draft of this fix, caught internally before it
      ever reached Codex review, made this mistake: a destroy+recreate race
      landing between steps 1 and 2 can make step 1's `Arc` stale even when
      it WAS `Some`, since it would then reference the old, discarded
      queue). Re-resolves the queue `Arc` the same way step 1 did (lock
      `logical_links`, clone, drop). If the newly-resolved `Arc` differs from
      step 1's (no queue before but one now, or a different queue `Arc` now,
      compared via `Arc::ptr_eq`) -- meaning the link changed underneath
      step 2 -- then: locks `subscriptions`, checks this call's own `(tx, G)`
      entry is STILL the current one for this key (`subs.get(key)`'s stored
      generation still equals `G` -- generations are drawn from a single
      global counter, so they are unique, making equality a sufficient
      "am I still current" identity check), and only if so, locks the
      re-resolved queue *while still holding the `subscriptions` guard* and
      stamps it with `G`, then drops both guards in that order. If the
      still-current check fails (a later `SubscribeEvent` call already
      displaced this one), this step is a no-op -- a delayed reconciliation
      must never stomp a newer, correctly-stamped generation.

   A naive alternative considered and rejected: holding `subscriptions` for
   this call's *entire* sequence, including the initial queue lookup. That
   would require acquiring `logical_links` *inside* an already-held
   `subscriptions` lock -- the reverse of `build_cll_rx_entries`'s
   established `logical_links` -> `subscriptions` order -- a classic AB/BA
   deadlock against a concurrent `build_cll_rx_entries` poll pass. Step 1's
   separate, dropped-before-continuing `logical_links` acquisition, followed
   by a `subscriptions`-only critical section, is what avoids that while
   still making the map-insert and the queue-stamp atomic with each other.

5. **`rpc_create_com_logical_link`** gets the mirror-image reconciliation for
   the same race, and it is likewise a single atomic critical section rather
   than two separate lock acquisitions: locks `logical_links`, then
   nested-locks `subscriptions` (order: `logical_links` -> `subscriptions`,
   matching `build_cll_rx_entries`'s existing order -- this is NOT a reversal)
   to read the current `subscriptions` entry for the new CLL's key (if any)
   and seed `live_sub_generation` from it (`0` sentinel if absent), builds the
   new `LogicalLinkState`/`CllEventQueue` with that seed, inserts the link
   into `logical_links`, then drops both guards. An interim draft (caught
   internally before it reached Codex review) read the seed and performed the
   `logical_links` insert as two separate critical sections; that gap could
   leave point 4 step 4's reconciliation missing the not-yet-inserted link
   AND predate point 4 step 2's `subscriptions` insert at the same time,
   stranding both sides permanently unreconciled -- the same bug shape as the
   `rpc_subscribe_event` race this whole fix addresses, just on the create
   side. Making the seed-read and the insert one atomic section (rather than
   holding `subscriptions` across the `logical_links` insert, which would
   reverse the stated order) closes it. Between this and point 4 step 4's
   re-check, whichever call lands second observes the other's already-durable
   write, so a subscription installed before its CLL exists is never
   permanently stranded at a generation the queue can never match.

6. **`deliver_or_enqueue`** gets exactly one new check, immediately after
   acquiring the queue lock (where it already unconditionally runs
   `push_cll_event`): if the captured `SubscriberRef`'s `generation` does not
   equal `queue.live_sub_generation`, the captured subscriber is treated as
   absent for this call -- the live-delivery/drain attempt is skipped
   entirely, falling through to the plain enqueue path `push_cll_event`
   already provides. No send is ever attempted on a stale sender. No other
   change to the drain loop's shape (still push-first-then-drain-if-current,
   from round 3).

7. **Module-/system-handle subscriptions are untouched.** `send_module_status`/
   `send_system_info`/`send_cll_status`/`send_cop_status` never capture a
   sender ahead of a separate lock acquisition the way `deliver_or_enqueue`'s
   three call sites do -- each looks up the CURRENT `subscriptions` entry and
   sends within one held lock, so there is no captured-then-stale window for
   them to begin with, and no backing queue for a generation to gate delivery
   into in the first place. Their `SubscriberRef::generation` is always the
   unused `0` sentinel.

**Why per-item revalidation inside the drain loop is unnecessary**: once
`deliver_or_enqueue` acquires the queue lock, `live_sub_generation` cannot
change again until it releases that lock (`rpc_subscribe_event` only ever
writes it while holding the same lock) -- and `deliver_or_enqueue` never
`.await`s while holding it. So a single check made right after acquisition is
valid for the entire push-then-drain duration; there is no window inside the
critical section where the generation could still change out from under an
already-validated `captured`.

**Why the single-check ("is `captured.generation` still `queue.live_sub_generation`")
argument itself is sound**, restated after the atomicity fix above: it relies
on the map's stored generation for a key and that key's queue's stamped
generation never observably disagreeing for a *live, currently-installed*
subscription. That property holds because (a) `rpc_subscribe_event`'s
map-insert and its own-queue-stamp are now one atomic critical section under
`subscriptions` (point 4 step 2 above), so two concurrent `SubscribeEvent`
calls for the same `cll_handle` can never leave the map and the queue
disagreeing about which of them "won"; and (b) every *out-of-band* stamp --
point 4 step 4's reconciliation, and `rpc_create_com_logical_link`'s seed
(point 5) -- is guarded by a still-current check (re-reading `subscriptions`
immediately before writing the queue) before it writes anything, so a
delayed/stale out-of-band write can never overwrite a newer, already-correct
generation with an older one. Without both (a) and (b), a `captured` ref's
generation could match `queue.live_sub_generation` by coincidence after a
losing writer's stale stamp overwrote a winning writer's correct one --
exactly the bug an interim draft of this fix reintroduced (caught internally
before it reached Codex review) by treating step 1's queue lookup and step 2's
map insert as non-atomic, and by running point 4 step 4's reconciliation only
when step 1 found no queue instead of unconditionally with a still-current
guard.

`j2534-0404-service/src/service/events.rs` gained
`deliver_or_enqueue_generation_tests`, covering the stale-capture scenario
directly against `deliver_or_enqueue` (the real-world race -- a subscription
replacement landing in the narrow window between `build_cll_rx_entries`'s
clone and `deliver_or_enqueue`'s own lock acquisition -- is not naturally
reproducible in this crate's single-threaded `current_thread` test runtime,
so this mirrors `rpc_primitive::rollback_stop_comm_pending_tests`'s
established "drive the exact guard directly" pattern for this class of
narrow-window race): a captured `SubscriberRef` at a stale generation never
receives the item, which lands in the queue instead and is retrieved live by
a subsequent call with the correct generation. `pdu_ioctl.rs` gained
`subscribe_before_create_reconciles_and_delivers_live_after_creation`, an
end-to-end test of point 5's reconciliation -- `SubscribeEvent` is called for
a `cll_handle` before `CreateComLogicalLink` ever runs for it (confirmed
reachable: `rpc_subscribe_event` never validates the CLL exists), and the
first frame delivered after creation is observed arriving live on that
pre-existing subscription rather than only being retrievable via
`GetEventItem`. **Precision correction (round 5, see below): this test only
covers the non-racing "subscribe fully completes, then create runs" ordering
-- `SubscribeEvent` is `.await`ed to completion before `CreateComLogicalLink`
is ever called, so it verifies the seed correctly copies an
already-installed subscription's generation but does not exercise point 5's
seed-read-and-insert being atomic against a genuinely concurrent
`SubscribeEvent` call.** The racing ordering is covered separately by
`rpc_primitive.rs`'s
`concurrent_subscribe_and_create_for_the_same_not_yet_existing_cll_handle_keeps_map_and_queue_generation_in_agreement`
(round 5, see below).

### Correction (round 5, same PR, edge-case-hunter test-rigor verification pass)

A final edge-case-hunter pass, run specifically to verify round 4's atomicity
fix before this PR was committed, confirmed the fix's own concurrency logic
correct by independent induction proof (not just by the existing tests
passing) but found two test-rigor gaps in round 4's own regression coverage,
neither a production bug:

1. **No test exercised the concurrent-subscribe-during-create gap.**
   `rpc_create_com_logical_link`'s seed-read-and-insert (point 5 above) is a
   single atomic critical section, but the only test touching that path
   (`subscribe_before_create_reconciles_and_delivers_live_after_creation`,
   `pdu_ioctl.rs`) is purely sequential -- see the precision correction
   above. Closed by a new test,
   `rpc_primitive.rs::rpc_subscribe_event_generation_tests::concurrent_subscribe_and_create_for_the_same_not_yet_existing_cll_handle_keeps_map_and_queue_generation_in_agreement`,
   which races a `SubscribeEvent` call against a `CreateComLogicalLink` call
   for the same not-yet-existing `cll_handle`, forcing both genuinely
   in-flight together via `tokio::spawn` and an externally-held
   `logical_links` lock (the first lock both paths contend on) before
   releasing it and letting both race to completion.
2. **The existing concurrent-subscribe race test
   (`concurrent_subscribe_for_existing_cll_keeps_map_and_queue_generation_in_agreement`)
   may not reliably falsify a reintroduced regression.** Because
   `rpc_subscribe_event`'s critical section is now genuinely atomic (the
   whole point of the fix), whichever task wins the shared lock runs its
   entire stamp-then-insert sequence in one unbroken span before the other
   can proceed -- so the test's assertion holds by construction whenever the
   fix is correct, but was not known to reliably *fail* against a
   reintroduced non-atomic split.

Both tests were strengthened with a `tokio::sync::Barrier` forcing both
spawned tasks to have genuinely started before either proceeds into its own
RPC call, rather than relying solely on a fixed `yield_now` count. Both
tests' actual detection power was then checked empirically, not just
reasoned about: each fix (`rpc_subscribe_event`'s atomic insert-and-stamp,
`rpc_create_com_logical_link`'s atomic seed-read-and-insert) was manually
reverted to the historical two-separate-critical-sections shape in turn, and
the corresponding test was run 5/5 times in isolation against the reverted
code. **Both tests still passed every time against their own reverted code**
-- confirming, empirically, that neither reliably detects this exact
regression shape. Root cause, traced and confirmed: this crate's
single-threaded `current_thread` test runtime never preempts a task
mid-poll except at a genuine `Pending` suspension, so once both tasks are
queued as waiters on the externally-held lock these tests use to force
overlap, the first-queued waiter runs its ENTIRE remaining sequence --
including the operation whose ordering-relative-to-the-other-task actually
matters -- to completion before the second task is polled again at all,
collapsing the intended race into a deterministic, bug-shape-insensitive
ordering. A fully deterministic fault-injection test for this exact
historical gap would need a test-only pause/gate hook inside
`rpc_subscribe_event`/`rpc_create_com_logical_link` themselves -- the same
class of test-only instrumentation this crate's other infeasibility notes
(e.g. `rollback_stop_comm_pending_tests`'s own doc comment) describe
elsewhere -- judged not worth the added invasiveness and risk for this fix.
Both tests are kept as-is: each still closes its own literal "no test
exercises this interleaving at all" gap (both RPCs are now genuinely
concurrent, not sequential, in both tests) and both remain meaningful
regression coverage for the CURRENT, correct code, with their own doc
comments and a `j2534-0404-service/docs/implementation-notes.md` Prioritized
Backlog entry recording this residual honestly rather than overstating what
either test proves. See each test's own doc comment (`rpc_primitive.rs`) for
the full per-test trace.

### Correction (round 6, same PR, Codex re-review finding + design-advisor redesign)

A fourth Codex review (round 6, still the same open PR) found a real, distinct
gap in round 4/5's generation-gate fix, independent of every finding above:
`deliver_or_enqueue`'s three callers (`poll_rx_inner`, `send_error_event`,
`handle_start_comm`) each captured `(tx, generation)` from the `subscriptions`
map *before* acquiring the queue lock (round 3's original design, preserved
unchanged by round 4/5's fix -- only the mismatch-handling logic changed).
If a `SubscribeEvent` replacement landed in the gap between that capture and
the queue-lock acquisition -- bumping the generation and atomically stamping
the queue's `live_sub_generation` to the new value, exactly as round 4/5
intended -- the producer's captured generation went stale relative to the
queue. Round 4/5's mismatch check correctly refused to send to the dead
captured `tx` (the bug that check *was* designed to catch), but then did
nothing further: no delivery was attempted to whoever the CURRENT, live,
correctly-registered subscriber actually was. The item just sat in `rx_buf`
until some later, unrelated push happened to trigger another drain. Worse:
if THIS push's own `push_cll_event` call reported a drop, its `Lost`
notification was computed but never sent (the stale `tx` was treated as
absent) and was never buffered anywhere either -- `Lost` is never stored in
`rx_buf` by design (see `make_lost_notification`'s own doc comment) -- so it
was permanently lost, not merely deferred like the round 4/5 fix's own
documented "deferred, not lost" guarantee promised.

Root cause, traced: round 4/5's generation counter was a **parallel copy** of
subscriber identity, compared against a **separately captured** snapshot.
Comparing two copies can only ever detect that they disagree; it cannot, by
itself, produce the one thing a producer actually needs at the moment of
delivery -- a live reference to whoever the CURRENT subscriber is. Every
correctness argument round 4/5 built (the atomic insert-and-stamp critical
section, the always-run reconciliation, the single-check-suffices proof) was
sound for what it was proving (map and queue generations never disagree for a
live subscription) but that property was never actually sufficient to fix the
bug class: even a queue whose generation exactly matches the map's current
generation is useless to a caller holding a *different* value from an earlier
capture.

**Design-advisor's redesign, implemented exactly: eliminate the separate
capture step entirely, rather than adding another mechanism to detect when it
goes stale.** `CllEventQueue` gains `live_sender: Option<SubscriptionSender>`
directly, replacing `live_sub_generation: u64`. The queue itself becomes the
single source of truth for "who is the current live subscriber" -- there is
no longer anything for a caller to capture ahead of time, and therefore no
capture-then-possibly-stale window for a concurrent replacement to land in.
`J2534Service::subscriptions`'s value type reverts from `SubscriberRef { tx,
generation }` to a plain `SubscriptionSender` (the pre-round-4 shape) --
`generation` is not retained anywhere, not even as a vestigial parallel copy
"for observability": once `live_sender` is read fresh, under the same lock,
at the point of use, a parallel generation copy would only reintroduce the
exact divergence class (a captured value drifting from live truth) this
redesign exists to eliminate. `J2534Service::next_cll_sub_generation` (the
global `AtomicU64` counter) is removed along with it.

Concretely, relative to round 4/5's mechanism (see that section above for the
parts that carry over unchanged in shape):

1. **`rpc_subscribe_event`'s atomic critical section is unchanged in shape**
   -- pre-resolve the queue `Arc` before touching `subscriptions`, then one
   critical section under `subscriptions` that inserts the new `tx` into the
   map AND, if a queue was found, writes `queue.live_sender = Some(tx.clone())`
   into it, both under the single `subscriptions` lock acquisition (order:
   `subscriptions` -> queue). Only what gets written changed: a plain sender
   clone instead of a generation number.
2. **The always-run reconciliation step is unchanged in shape** -- still
   re-resolves the queue after the atomic section, still runs unconditionally
   (not only when no queue was found before), still re-checks "am I still
   current" before writing out-of-band. Only the identity check changed:
   `reconcile_stale_cll_subscription` now checks
   `subs.get(key).is_some_and(|current| current.same_channel(tx))` instead of
   generation equality. A clone of the same `mpsc::UnboundedSender` shares
   channel identity with the original; a distinct `SubscribeEvent` call always
   produces a distinct channel -- so `same_channel` is a sufficient and
   unambiguous identity test, exactly as generation equality was, without
   needing a counter to manufacture uniqueness.
3. **`rpc_create_com_logical_link`'s atomic seed+insert is unchanged in
   shape** -- still one critical section (`logical_links` then nested
   `subscriptions`) that reads whatever `subscriptions` entry already exists
   for the new CLL's key and seeds the new `CllEventQueue` from it before
   inserting into `logical_links`. Only what gets seeded changed:
   `live_sender: subs.get(key).cloned()` (a plain `Option<SubscriptionSender>`
   field init) instead of a generation number, defaulting to `None` (the
   `CllEventQueue::default()` value) rather than the old `0` sentinel.
4. **`deliver_or_enqueue` drops its subscriber parameter entirely.** Callers
   (`poll_rx_inner`, `send_error_event`, `handle_start_comm`) now pass only
   the queue `Arc` -- nothing captured from `subscriptions` beforehand. After
   locking the queue (the same lock it already took for
   `push_cll_event`/drain), `deliver_or_enqueue` reads `queue.live_sender.clone()`
   directly -- always fresh, guarded by the same lock it is already holding,
   for the entire duration of this call. `build_cll_rx_entries` (`events.rs`)
   no longer touches `subscriptions` at all for the same reason: its `sub`
   field (a per-CLL subscriber snapshot, cloned once per poll pass and reused
   across every frame in that pass -- an even wider capture-then-stale window
   than the original per-item capture) is removed from `CllRxEntry` entirely.
5. **Send-failure self-heal, inside `deliver_or_enqueue`, under the same
   queue lock it already holds**: if a `tx.send()` fails -- deterministic for
   `mpsc::UnboundedSender` once the paired receiver is dropped, not transient
   -- `queue.live_sender` is cleared to `None` right there, and the triggering
   item stays enqueued exactly as round 3's existing fallback-to-enqueue
   behavior already provided. This applies uniformly to a failed `Lost` send
   and to a failed send inside the drain loop, and covers every route to a
   dead sender, including a create-seed or reconciliation write that happened
   to copy an already-dead map entry (a case round 4/5's generation gate had
   no answer for either, since a dead-but-still-generation-matching sender
   would have passed that check and then failed the send anyway, without ever
   correcting `live_sub_generation` for the next call).
6. **`terminate_subscription`/`terminate_all_subscriptions` gain the fix
   design-advisor flagged as the additional gap round 4/5 never closed at
   all** (**superseded by round 7 below** -- this point describes what was
   *written* in round 6, which turned out to be dead code at both real call
   sites; see "Correction (round 7, ...)" for the actual, working fix):
   previously these sent `Cancelled` and removed the `subscriptions`
   map entry but never touched any CLL's queue, leaving `live_sender` (or, in
   round 4/5's terms, the map's generation) pointing at an orphaned-but-open
   channel -- a `tx.send()` to it still returns `Ok` until tonic drops the
   receiver, so a future push would believe it delivered live when nobody was
   reading anymore. Both functions now resolve the relevant queue `Arc`(s) via
   `logical_links` first (clone, then drop that guard), then, under
   `subscriptions`, remove the entry and clear `queue.live_sender` when it
   `same_channel`s the sender just removed, before sending `Cancelled`. Where
   the CLL has already been removed from `logical_links` (e.g. mid-
   `DestroyComLogicalLink`, which removes its own `logical_links` entry before
   calling `terminate_subscription` -- see `rpc_link.rs`'s destroy sequence)
   there is no queue to resolve at all -- harmless, not a gap: that queue
   (owned by the now-detached `LogicalLinkState`) is unreachable by any future
   producer regardless of whether `live_sender` is cleared on it.

**The "no `subscriptions` lock inside a held queue lock" rule**, implicit but
never previously stated as an explicit rule of its own: `deliver_or_enqueue`
holds a per-CLL queue lock for its whole push-then-drain duration and must
NEVER acquire `subscriptions` while holding it. This is the one lock-order
reversal that would deadlock against `rpc_subscribe_event`'s own
`subscriptions` -> queue nesting (point 1 above). Round 6's redesign makes
this trivially true by construction -- `deliver_or_enqueue` has no remaining
reason to touch `subscriptions` at all, since `live_sender` is everything it
reads -- but it is now stated explicitly, both in this ADR and as a doc
comment on `deliver_or_enqueue` and on `J2534Service::logical_links` (the
lock-order table's home, `service.rs`), so a future edit that reintroduces a
`subscriptions` read inside `deliver_or_enqueue` has an explicit warning to
violate, not just an implicit invariant to rediscover by tracing call sites.

**Why `live_sub_generation` was removed rather than kept "for observability"
alongside `live_sender`**: a parallel copy of the same information, updated by
a different code path than the one that reads it, is exactly the shape of bug
this whole redesign exists to eliminate -- keeping one here, even unused by
the actual delivery decision, would be an attractive nuisance for a future
edit to accidentally start trusting again instead of `live_sender` itself.
Once `live_sender` is read fresh under the same lock at the point of use,
there is nothing left for a generation counter to usefully add: it cannot
detect anything `live_sender` itself does not already make impossible by
construction.

**Tests**: `rpc_primitive.rs`'s `rpc_subscribe_event_generation_tests` module
is renamed `rpc_subscribe_event_live_sender_tests`; its two concurrent
interleaving tests
(`concurrent_subscribe_for_existing_cll_keeps_map_and_queue_live_sender_in_agreement`,
`concurrent_subscribe_and_create_for_the_same_not_yet_existing_cll_handle_keeps_map_and_queue_live_sender_in_agreement`)
are ported to assert `same_channel` identity instead of generation equality,
with the same empirically-checked test-rigor caveat round 5 already recorded
(still applicable: the atomic-critical-section shape these tests exercise
did not change, only what gets written into it) --
`j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog entry
is updated for the new names rather than duplicated. `events.rs`'s
`deliver_or_enqueue_generation_tests` module is renamed
`deliver_or_enqueue_live_sender_tests` and gains
`replacement_subscriber_receives_live_delivery_including_lost_from_push_around_attachment`
(Codex's round-6 scenario is moot as a literal race to set up now that there
is no separate capture step to race against -- so this instead confirms a
replacement subscriber attached concurrently with pushes correctly starts
receiving live delivery, including a `Lost` for a push that overflowed right
around the attachment) and
`dead_sender_send_failure_clears_live_sender_and_preserves_item` (asserts
`live_sender` specifically becomes `None` after a send failure, not just that
the item survives -- round 3's existing fallback-to-enqueue coverage already
proved the latter). `rpc_subscribe_event_live_sender_tests` also gains
`terminate_subscription_clears_live_sender_and_stops_future_delivery`,
covering point 6 above end to end: subscribe, terminate, then push via
`send_error_event`, asserting `live_sender` becomes `None` and the push lands
in the queue rather than being delivered to the orphaned channel.

### Correction (round 7, same PR, edge-case-hunter finding on round 6's own fix)

Round 6's point 6 above describes `terminate_subscription`/`terminate_all_subscriptions`
resolving the affected queue `Arc`(s) via a self-lookup against `self.logical_links`. That
description matches what was *written*, but a verification pass (before this ever reached
Codex — the same self-caught-before-review pattern as round 5) found it was dead code at
**both** of the mechanism's only production call sites: `rpc_link.rs`'s
`rpc_destroy_com_logical_link` removes its CLL from `logical_links`
(`logical_links.lock().await.remove(&handle)`) *before* calling `terminate_subscription`, and
`rpc_module.rs`'s `rpc_module_disconnect` clears the entire `logical_links` map
(`logical_links.lock().await.clear()`) *before* calling `terminate_all_subscriptions`. A
self-lookup performed *inside* either termination function therefore always found nothing —
`live_sender` was never actually cleared by either function at either real call site, only in
a test that called `terminate_subscription`/`terminate_all_subscriptions` directly with
`logical_links` still deliberately left populated (a context that never occurs in production).
Concretely, a straggling `poll_rx_inner` pass holding a pre-teardown `rx_buf` `Arc` clone
(`build_cll_rx_entries` snapshots this once per poll pass, reused across every frame in that
pass, with `.await`s between them) could still deliver a live notification through the
orphaned `live_sender` after the client had already received `Err(Status::cancelled(...))` —
exactly the "contradictory terminal status after Cancelled" symptom this mechanism exists to
prevent.

**Fix**: both functions no longer resolve the queue themselves. `terminate_subscription` gains
a `queue: Option<&Arc<Mutex<CllEventQueue>>>` parameter; `terminate_all_subscriptions` gains a
`queues: &HashMap<u32, Arc<Mutex<CllEventQueue>>>` parameter. Each caller passes its own
already-in-scope handle instead: `rpc_destroy_com_logical_link` still holds `link` (the value
returned by its own `logical_links.remove(&handle)` call) at the point it calls
`terminate_subscription`, so it passes `Some(&link.rx_buf)` directly — no re-lookup needed,
since the caller never lost the reference in the first place. `rpc_module_disconnect` now
captures every CLL's queue `Arc` into a `HashMap` *before* calling `logical_links.clear()`
(replacing the bare `.clear()` with a scoped block that collects then clears), then passes that
map to `terminate_all_subscriptions`. Both call sites' `live_sender` clearing is now genuinely
reachable, verified end to end — see Tests below.

**Why the caller, not a smarter self-lookup**: any self-lookup performed by
`terminate_subscription`/`terminate_all_subscriptions` after their own map mutation runs is
structurally too late by construction — the queue's only remaining reference at that point
lives in whichever caller already extracted it (or already discarded it, as `.clear()`
originally did). There is no lookup key left that resolves it; the fix has to be "don't lose
the reference in the first place," not "look harder for it."

**Tests**: `rpc_subscribe_event_live_sender_tests` gains
`destroy_com_logical_link_clears_live_sender_via_the_real_call_path` — drives the fix through
the actual `rpc_destroy_com_logical_link` RPC handler (not `terminate_subscription` directly),
holding an independent `Arc` clone of the queue from before destroy so its `live_sender` can
still be inspected afterward (mirroring exactly the straggling-poll-task scenario above).
Verified to discriminate the bug: manually reverting `rpc_destroy_com_logical_link`'s call site
back to passing `None` (simulating round 6's original self-lookup finding nothing) makes this
test fail; the actual fix passes it. `terminate_subscription_clears_live_sender_and_stops_future_delivery`
(round 6's original test, calling `terminate_subscription` directly with `logical_links` still
populated) is kept unchanged as a lower-level mechanism check, with its own doc comment
clarifying it is not, by itself, a production-call-path test — that role now belongs to the new
end-to-end test above. No end-to-end test was added for `terminate_all_subscriptions`/
`rpc_module_disconnect`'s analogous fix (the multi-CLL `ModuleDisconnect` path); this is a
tracked residual, not silently skipped — see `j2534-0404-service/docs/implementation-notes.md`'s
backlog.

### What is deliberately *not* covered

- **`GetEventItem` / `ErrorDetail` RPC-fallback**: no `Lost` variant exists
  on `EventItem.data`, and none is added. A poll-only client (no
  `SubscribeEvent` subscription) receives no loss signal — this mirrors a
  native D-PDU client that registers no event callback and only polls
  `PDUGetEventItem`, which likewise never sees `PDU_EVT_DATA_LOST`. This is
  the spec's own audience model (callback-only), not an accepted gap
  awaiting future work.
- **`PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`'s immediate-trim-on-cap-lowering
  path** (`rpc_misc.rs::ioctl_set_event_queue_properties`, the
  `while buf.len() > new_cap { buf.pop_front(); }` loop): bypasses
  `push_cll_event` entirely and silently discards buffered `rx_buf` items
  with no signal at all, even to a live subscriber. Out of scope for this
  ADR: that loss is a synchronous, client-commanded resize with its own
  successful ioctl return as the signal, not a "buffer (queue) overrun" per
  D.1.8 — a different failure mode than what `PDU_EVT_DATA_LOST` describes.
  Recorded as an accepted-residual backlog item in
  `j2534-0404-service/docs/implementation-notes.md` rather than fixed here.

## Consequences

- Closes conformance audit finding A2-6 for the `SubscribeEvent` live path:
  a subscribed client now learns, in real time, whenever its CLL's event
  queue drops data, for both queue modes -- with the round-2 correction
  ensuring a subscribe-only client never sees a *false* `Lost` or a
  permanent stall, and the round-3 correction ensuring `Lost` is not
  permanently dead code for a live subscriber (the round-1 regression and
  the round-2 regression, respectively, both caught before merge).
- (Round 3) The CLL event queue's cap/mode now bounds occupancy the exact
  same way regardless of subscription state -- `push_cll_event` runs first,
  unconditionally, on every push. This is native-faithful: a real D-PDU
  callback client's event queue is still capped in the native model; the
  callback is an opportunistic fast drain of that same bounded queue, not
  infinite bypass capacity layered in front of it. A live, healthy,
  keeping-up subscriber still practically never sees `Lost` -- each push's
  immediate same-critical-section full drain means `rx_buf` oscillates
  empty -> one-item -> empty every cycle, so the cap is essentially never
  crossed in steady state -- but this is now an emergent property of the
  drain being fast enough, not a structural guarantee that `Lost` can never
  fire while a subscriber is attached (round 2's bug was exactly that: it
  could never fire).
- (Round 3) A subscribe-transition-with-stale-backlog (a backlog accumulated
  with no subscriber attached, at or over cap, then a subscriber attaches)
  can cause **at most one genuine drop** with an honest, accompanying
  `Lost` -- the first push after the transition that still finds the queue
  at cap. After that one drop, the full remaining backlog drains live, FIFO,
  in the same critical section, and delivery is clean (no further `Lost`)
  from then on, as long as the subscriber keeps up. See
  `backlog_evicted_then_subscribe_emits_exactly_one_lost_then_drains_fifo`
  (`pdu_ioctl.rs`) for the covering test.
- (Round 3) `Lost`'s position in a subscriber's notification stream carries
  no ordering meaning relative to the item payloads around it beyond "a drop
  happened around here" -- ISO 22900-2 D.1.8 defines `PDU_EVT_DATA_LOST` as
  informational-only, carrying no event data, so there is no mode-dependent
  ordering guarantee to invent (e.g. exactly where in a multi-item drain a
  `Lost` for an `OverwriteOldest` eviction appears relative to unrelated,
  never-evicted items is not a spec-defined contract, and this
  implementation does not attempt to manufacture one beyond "the drain that
  observed the drop sends `Lost` before draining its own buffer").
- `GetEventItem` while a live `SubscribeEvent` subscription is active on the
  same CLL will typically find `rx_buf` empty -- items are streamed out live
  instead of buffered. This is the proto-documented `SubscribeEvent`
  contract (`service.proto`'s own doc comment, and `docs/rpc-api-guide.md`),
  not a regression; a poll-only client with no subscription is completely
  unaffected. Existing `grpc_mock` tests that subscribed AND then polled
  `GetEventItem` for the same item on the same CLL needed updating to read
  from the live stream instead (`startcomm_comparam.rs`,
  `cop_ctrl_cycles.rs`; see `harness.rs`'s new
  `wait_for_cop_finished_and_result_data`).
- A poll-only (`GetEventItem`-only, no `SubscribeEvent`) client still
  receives no loss signal at all, by design — see "What is deliberately not
  covered" above. This is a real, permanent limitation of the current
  shared proto's `EventItem` shape, not a bug; extending it would require a
  `vci-service-interface` proto change, which was considered and rejected
  because the spec itself defines `PDU_EVT_DATA_LOST` as unrepresentable on
  that wire.
- `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`'s trim-on-lower-cap path remains an
  accepted, documented gap (`implementation-notes.md` backlog) — not fixed by
  this ADR.
- `push_cll_event`'s return type changed from `bool` to `PushOutcome`; all
  production call sites were updated (round 2: they now all go through
  `deliver_or_enqueue`, rather than calling `push_cll_event` directly; round
  3: `deliver_or_enqueue` calls `push_cll_event` first, unconditionally,
  rather than only as a fallback). `PushOutcome::inserted()` was removed
  (round 2) once its only callers -- round 1's now-superseded
  delivery-gating logic -- were replaced; `PushOutcome::lost()` still uses
  `matches!` rather than an exhaustive `match` -- a future 4th variant would
  silently fall through it rather than fail to compile; noted for a future
  editor, not a present defect (only 3 variants exist today).
- `ioctl_set_buffer_size`'s (`rpc_misc.rs`) `PDU_IOCTL_SET_BUFFER_SIZE`
  truncation now also applies to items delivered live via `SubscribeEvent`
  when they were drained from backlog or sent as the triggering item
  (round 2: `cll_queue_item_to_event_item` is shared by both
  `rpc_get_event_item` and `deliver_or_enqueue`) -- previously that limit
  was documented as intentionally not applying to the live fan-out path;
  that note is now stale and was corrected in place.
- (Round 4, accepted residual) `send() == Ok` on a `SubscriptionSender` still
  only means "queued into the channel/HTTP2 send buffers," not "the client
  actually received it." On a client's own unilateral disconnect (as opposed
  to a server-initiated replacement, which round 4 now closes), items already
  queued in the channel or in HTTP/2 buffers between the server and a client
  that then vanishes can still be lost with no `rx_buf` copy remaining once
  the drain loop has already popped them. This is inherent to any buffered
  streaming transport and matches the native D-PDU callback model's own
  delivery guarantee (a callback invocation that races the client process
  exiting is no more durable) -- round 4's generation gate specifically
  eliminates the *server-induced* variant of this loss (a subscription
  replacement racing a captured sender), which is the only variant within
  this service's own control; a client-induced loss on the client's own
  unilateral disconnect is not, and is not attempted here.
- (Round 4, accepted residual) Module- and system-handle subscriptions have
  no backing queue (`send_module_status`/`send_system_info` never pop from
  any per-subscription backlog on send) and therefore need no generation
  tracking at all -- `SubscriberRef::generation` is carried but never
  consulted for these two subscription kinds, always the unused `0`
  sentinel. This is a scope boundary, not a gap: the round-4 race is
  structurally specific to `deliver_or_enqueue`'s capture-then-lock pattern,
  which only the CLL event queue uses.
