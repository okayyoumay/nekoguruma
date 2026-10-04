use super::*;

fn make_cll_notification(
    cll_handle: u32,
    cop_handle: Option<u32>,
    timestamp: u32,
    data: EventItemData,
    // ADR-204: echoed verbatim on `EventItem.cop_tag` iff `cop_handle` is
    // `Some`. `send_cll_status`'s no-`LogicalLinkState` fallback has no COP
    // in play and passes `None`; `send_error_event`'s analogous fallback
    // passes the tag it already looked up from `primitives` (via
    // `cop_handle`) before calling here.
    cop_tag: Option<Vec<u8>>,
) -> EventNotification {
    EventNotification {
        handle: Some(EventNotificationHandle::CllHandle(ComLogicalLinkHandle {
            module_handle: DEFAULT_MODULE_HANDLE,
            cll_handle,
        })),
        event_data: Some(EventNotificationData::Item(EventItem {
            cop_handle: cop_handle.map(|cop_handle| ComPrimitiveHandle {
                module_handle: DEFAULT_MODULE_HANDLE,
                cll_handle,
                cop_handle,
            }),
            timestamp,
            data: Some(data),
            cop_tag,
        })),
    }
}

/// Builds a `PDU_EVT_DATA_LOST` notification for `cll_handle` (ADR-115):
/// edge-triggered, SubscribeEvent-only signal that `push_cll_event` just
/// evicted or discarded an entry for this CLL's event queue. Carries no
/// `EventItem` -- ISO 22900-2's `PDU_EVT_DATA_LOST` is "information only, no
/// event data stored in the event queue", and the shared proto's
/// `EventItem.data` has no Lost variant, so this is unrepresentable on the
/// `GetEventItem` path; only a live `SubscribeEvent` listener ever observes
/// it. Mirrors `iso22900-service`'s native ISO 22900 D-PDU path
/// (`iso22900-service/src/service/events.rs`), which already emits the same
/// `EventNotificationData::Lost(LostEventItemNotification {})` shape.
pub(super) fn make_lost_notification(cll_handle: u32) -> EventNotification {
    EventNotification {
        handle: Some(EventNotificationHandle::CllHandle(ComLogicalLinkHandle {
            module_handle: DEFAULT_MODULE_HANDLE,
            cll_handle,
        })),
        event_data: Some(EventNotificationData::Lost(LostEventItemNotification {})),
    }
}

/// Converts one buffered `CllQueueItem` into the wire-level `EventItem`
/// shape: applies `PDU_IOCTL_SET_BUFFER_SIZE`'s `result_buffer_limit`
/// truncation to a frame's `data_bytes` and ADR-051's header/footer
/// `extra_info` split, exactly like `rpc_get_event_item`
/// (`rpc_primitive.rs`) always has.
///
/// Takes `item` by reference (cloning only the `Vec<u8>` fields a frame
/// actually needs) rather than consuming it, so a caller that might need to
/// push `item` back onto `rx_buf` unchanged on a failed live send (see
/// `deliver_or_enqueue` below) never has to reconstruct it.
///
/// Shared by `GetEventItem` (`rpc_get_event_item`) and the drain-then-send
/// live-delivery path below (`deliver_or_enqueue`) -- ADR-115's correction:
/// a live `SubscribeEvent` subscriber IS this queue's drain, so it now
/// applies the exact same `result_buffer_limit` truncation `GetEventItem`
/// always has (superseding `ioctl_set_buffer_size`'s prior "SubscribeEvent
/// live fan-out sites are intentionally left unaffected" note in
/// `rpc_misc.rs`, now stale).
pub(in crate::service) fn cll_queue_item_to_event_item(
    cll_handle: u32,
    result_buffer_limit: Option<u32>,
    item: &CllQueueItem,
) -> EventItem {
    match item {
        CllQueueItem::Frame(frame) => {
            // PDU_IOCTL_SET_BUFFER_SIZE (a size limit on the item structure
            // returned as a result): caps data_bytes -- the actual result
            // payload -- to at most result_buffer_limit bytes.
            // header_bytes/footer_bytes are protocol framing, not "the
            // result," so they are left untouched.
            let mut data = frame.data.clone();
            if let Some(limit) = result_buffer_limit {
                data.truncate(limit as usize);
            }
            // ADR-051: CAN/ISO15765/ISO9141/ISO14230/J1850 frames carry
            // their header/footer bytes (empty for every unaffected
            // protocol) split out of data_bytes by the poll task.
            let extra_info = (!frame.header_bytes.is_empty() || !frame.footer_bytes.is_empty())
                .then_some(vci_service_interface::ExtraInfo {
                    header_bytes: frame.header_bytes.clone(),
                    footer_bytes: frame.footer_bytes.clone(),
                });
            EventItem {
                cop_handle: frame.cop_handle.map(|cop_handle| ComPrimitiveHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle,
                    cop_handle,
                }),
                timestamp: frame.timestamp,
                data: Some(EventItemData::ResultData(ResultData {
                    rx_flag: rx_flag_bytes(
                        frame.rx_status_flags,
                        RxFlagExtras {
                            ecu_timing_change: frame.ecu_timing_change,
                            sw_can_hv_rx: frame.sw_can_hv_rx,
                        },
                    ),
                    unique_resp_identifier: frame.unique_resp_identifier,
                    acceptance_id: frame.acceptance_id,
                    tx_msg_done_timestamp: (frame.rx_status_flags & RX_TX_INDICATION as u8 != 0)
                        .then_some(frame.timestamp),
                    start_msg_timestamp: (frame.rx_status_flags & RX_START_OF_MESSAGE as u8 != 0)
                        .then_some(frame.timestamp),
                    extra_info,
                    data_bytes: data,
                })),
                // ADR-204: echoed verbatim whenever `cop_handle` is present,
                // resolved by `ReceivedFrame::cop_tag` at the time this frame
                // was bound to its COP (see that field's own doc comment) --
                // a `ResultData` frame is the primary `cop_tag` use case, the
                // response to a `CoptSendrecv` COP.
                cop_tag: frame.cop_tag.clone(),
            }
        }
        // Async error event queued by send_error_event (ADR-105 revision):
        // same shape SubscribeEvent already sends its subscribers, now also
        // visible to GetEventItem pollers.
        CllQueueItem::Error(tracked) => EventItem {
            cop_handle: tracked.cop.map(|c| ComPrimitiveHandle {
                module_handle: DEFAULT_MODULE_HANDLE,
                cll_handle: c.cll_handle,
                cop_handle: c.cop_handle,
            }),
            timestamp: tracked.timestamp,
            data: Some(EventItemData::ErrorData(tracked.event as i32)),
            // ADR-204: echoed verbatim whenever `cop_handle` (i.e.
            // `tracked.cop`) is present -- `send_error_event` resolved this
            // from `primitives` at record time (see `TrackedError::cop_tag`'s
            // own doc comment).
            cop_tag: tracked.cop_tag.clone(),
        },
        // CLL/COP status transition queued by send_cll_status/send_cop_status
        // (ADR-105 P2 follow-up): same shape SubscribeEvent already sends its
        // subscribers, now also visible to GetEventItem pollers.
        CllQueueItem::Status(tracked) => match &tracked.event {
            StatusEvent::Cll(status) => EventItem {
                cop_handle: None,
                timestamp: tracked.timestamp,
                data: Some(EventItemData::CllStatus(*status as i32)),
                cop_tag: None,
            },
            StatusEvent::Cop {
                cop_handle,
                status,
                cop_tag,
            } => EventItem {
                cop_handle: Some(ComPrimitiveHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle,
                    cop_handle: *cop_handle,
                }),
                timestamp: tracked.timestamp,
                data: Some(EventItemData::CopStatus(*status as i32)),
                cop_tag: cop_tag.clone(),
            },
        },
        // Both consumers of this function (`drain_queue_live` and
        // `rpc_get_event_item`) gate on `PendingTimingFrame` before ever
        // calling this function -- an unfinalized reservation must never be
        // delivered live or popped by GetEventItem. Reaching this arm means
        // that gate was bypassed, which is a real bug, not something to
        // paper over with a silent conversion.
        CllQueueItem::PendingTimingFrame { .. } => {
            unreachable!(
                "PendingTimingFrame must never reach cll_queue_item_to_event_item -- both drain_queue_live and rpc_get_event_item gate on it"
            )
        }
    }
}

/// Wraps a converted `EventItem` into the `EventNotification` shape a
/// `SubscribeEvent` subscriber's stream carries, for `cll_handle`.
pub(super) fn notification_from_event_item(cll_handle: u32, item: EventItem) -> EventNotification {
    EventNotification {
        handle: Some(EventNotificationHandle::CllHandle(ComLogicalLinkHandle {
            module_handle: DEFAULT_MODULE_HANDLE,
            cll_handle,
        })),
        event_data: Some(EventNotificationData::Item(item)),
    }
}

/// Sends a `cll_status` event to the subscriber for `cll_handle`, if any, and
/// enqueues it into that CLL's `rx_buf` for `GetEventItem` pollers (ADR-105
/// P2 follow-up: closes the same subscription-only gap `send_error_event`
/// already closed for async error events -- see `CllQueueItem`'s own doc
/// comment). Routed through `deliver_or_enqueue`/`push_cll_event`, the exact
/// mechanism `send_error_event` already uses, rather than a second
/// independent "push to rx_buf, then separately notify the subscriber" path:
/// `CllEventQueue::live_sender` is kept in lockstep with `subscriptions` by
/// `rpc_subscribe_event`/`terminate_subscription` (see their own doc
/// comments), so reading it here is behaviorally identical to the old direct
/// `subscriptions.get(...)` lookup, while also getting `push_cll_event`'s
/// cap/eviction enforcement and the drain-under-one-lock guarantee that
/// prevents this item from silently accumulating in `rx_buf` forever
/// whenever a live subscriber is attached but never calls `GetEventItem`.
/// The `logical_links` snapshot below and `deliver_or_enqueue`'s own queue
/// lock are two separate acquisitions, not one atomic critical section, but
/// this reopens no new window: `terminate_subscription` clears
/// `live_sender` under the same queue lock `deliver_or_enqueue` reads it
/// under, so a delayed call here can never deliver after cancellation, and
/// the destroy-path race this leaves open (delivery landing between
/// `Offline` and the eventual `terminate_subscription`) is the identical
/// accepted matched-precedent gap already documented elsewhere in this file
/// (search `matched-precedent gap`) -- design-advisor-verified equivalent to
/// the pre-ADR-105-P2 bare `subscriptions.get(...)` behavior, not a
/// widening of it.
pub(in crate::service) async fn send_cll_status(
    subscriptions: &Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    cll_handle: u32,
    status: PduComLogicalLinkStatus,
) {
    let timestamp = module_timestamp_us();
    let queue_info = {
        let links = logical_links.lock().await;
        links.get(&cll_handle).map(|link| Arc::clone(&link.rx_buf))
    };
    let Some(rx_buf) = queue_info else {
        // No `LogicalLinkState` for this CLL -- this is the EXPECTED path for
        // an `Offline` transition sent from `rpc_destroy_com_logical_link`,
        // which removes the `logical_links` entry before calling this
        // function: there is nothing left to enqueue into for an already-
        // destroyed CLL. A live subscriber (if any) still gets this event's
        // own notification directly, matching send_error_event's analogous
        // fallback.
        let subs = subscriptions.lock().await;
        if let Some(sub) = subs.get(&(DEFAULT_MODULE_HANDLE, cll_handle)) {
            let n = make_cll_notification(
                cll_handle,
                None,
                timestamp,
                EventItemData::CllStatus(status as i32),
                None,
            );
            let _ = sub.send(Ok(n));
        }
        return;
    };
    deliver_or_enqueue(
        &rx_buf,
        cll_handle,
        CllQueueItem::Status(TrackedStatus {
            event: StatusEvent::Cll(status),
            timestamp,
        }),
    )
    .await;
}

/// Sends a `cop_status` event to the subscriber for `cll_handle`, if any,
/// and enqueues it into that CLL's `rx_buf` for `GetEventItem` pollers
/// (closing the second half of the ADR-105 P2 follow-up that
/// `send_cll_status` already closed for CLL-scoped status).
///
/// **Why `queue_target` is a caller-supplied parameter instead of a
/// `logical_links` lookup done here** (unlike `send_cll_status`, which looks
/// `logical_links` up itself): virtually every call site of this function --
/// funneled almost entirely through `emit_terminal_if_live` (ADR-128) plus a
/// handful of direct callers like `cancel_link_cops` -- holds `primitives`
/// locked across the call, by design, to keep the `primitives` removal and
/// `terminal_cops.record` in one atomic critical section (A2-23). At least
/// one of those call sites (`dispatch_tx_item`'s WAITING/CANCELLED tail, this
/// file) *also* already holds `logical_links` across the same call. This
/// crate's documented lock hierarchy (`J2534Service::logical_links`'s doc
/// comment, `service.rs`) is `logical_links -> primitives`, never the
/// reverse; having this function acquire `logical_links` itself while a
/// caller already holds `primitives` would violate that at every
/// `emit_terminal_if_live`-routed call site, and at the `dispatch_tx_item`
/// site specifically it would self-deadlock outright (re-locking
/// `logical_links` while the calling task already holds it --
/// `tokio::sync::Mutex` is not reentrant).
///
/// The resolution: every caller resolves the queue target
/// ([`resolve_queue_target`]/[`CllQueueTarget::from_link`]) from
/// `logical_links` *before* `primitives` is ever acquired (or, when the
/// caller has no reason to touch `primitives` at all, immediately before the
/// call), then passes the already-resolved `CllQueueTarget` in. This keeps
/// the lookup strictly on the `logical_links`-first side of the hierarchy
/// while still letting `primitives`/`terminal_cops.record` stay one
/// continuous critical section exactly as ADR-128 requires -- this function
/// itself never acquires `logical_links`. `None` means "no `LogicalLinkState`
/// for this CLL" (e.g. `emit_terminal_if_live`/callers race a destroy that
/// already removed the entry, or ran before `resolve_queue_target` found it)
/// and falls back to the same direct-only delivery `send_cll_status` uses in
/// its own analogous no-`LogicalLinkState` branch.
pub(in crate::service) async fn send_cop_status(
    subscriptions: &Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    terminal_cops: &Arc<Mutex<TerminalCopsLedger>>,
    queue_target: Option<&CllQueueTarget>,
    cll_handle: u32,
    cop_handle: u32,
    status: PduComPrimitiveStatus,
    // ADR-204: `cop_handle`'s `CopEntry::cop_tag`, resolved by the caller at
    // the same time it read/removed the `primitives` entry for this
    // `cop_handle` -- never re-resolved here, since a terminal caller
    // (`emit_terminal_if_live`) has already removed the entry from
    // `primitives` by the time this function runs.
    cop_tag: Option<Vec<u8>>,
) {
    // A2-23 (ADR-128): record every terminal transition so a COP that has
    // already left `primitives` remains resolvable (CancelComPrimitive
    // success, GetStatus) until this CLL is destroyed. Lock acquired and
    // released here, never nested with `subscriptions` below.
    // `TerminalCopsLedger::record` is itself the atomic-with-purge guard
    // (Codex-review round 1): a no-op if `cll_handle` was already marked
    // destroyed by a concurrent `DestroyComLogicalLink`/`ModuleDisconnect`
    // purge that won the race against this insert.
    if matches!(
        status,
        PduComPrimitiveStatus::PduCopstFinished | PduComPrimitiveStatus::PduCopstCancelled
    ) {
        terminal_cops
            .lock()
            .await
            .record(cop_handle, cll_handle, status);
    }
    match queue_target {
        Some(target) => {
            deliver_or_enqueue(
                &target.rx_buf,
                cll_handle,
                CllQueueItem::Status(TrackedStatus {
                    event: StatusEvent::Cop {
                        cop_handle,
                        status,
                        cop_tag,
                    },
                    timestamp: module_timestamp_us(),
                }),
            )
            .await;
        }
        None => {
            // No `LogicalLinkState` for this CLL (destroy-path race) --
            // matches `send_cll_status`'s identical fallback: nothing to
            // enqueue into, still give a live subscriber (if any) this one
            // notification directly.
            let subs = subscriptions.lock().await;
            if let Some(sub) = subs.get(&(DEFAULT_MODULE_HANDLE, cll_handle)) {
                let n = EventNotification {
                    handle: Some(EventNotificationHandle::CllHandle(ComLogicalLinkHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                        cll_handle,
                    })),
                    event_data: Some(EventNotificationData::Item(EventItem {
                        cop_handle: Some(ComPrimitiveHandle {
                            module_handle: DEFAULT_MODULE_HANDLE,
                            cll_handle,
                            cop_handle,
                        }),
                        timestamp: module_timestamp_us(),
                        data: Some(EventItemData::CopStatus(status as i32)),
                        cop_tag,
                    })),
                };
                let _ = sub.send(Ok(n));
            }
        }
    }
}

/// Terminal-status counterpart to the crate-wide "first-wins through
/// `primitives`" idiom (ADR-128, Codex review round 2): removes
/// `cop_handle` from `primitives` and, only if THIS call wins that removal,
/// emits `status` via `send_cop_status` -- holding the SAME `primitives`
/// `MutexGuard` across the whole operation (never dropped and reacquired)
/// so the removal and `send_cop_status`'s `terminal_cops.record` land in
/// one atomic critical section. Without this, a concurrent
/// `CancelComPrimitive`/`GetStatus` landing in the gap between a bare
/// `if primitives.lock().await.remove(...).is_some() { send_cop_status(...).await; }`'s
/// temporary guard dropping and `send_cop_status`'s own `terminal_cops`
/// acquisition would see a miss on BOTH maps and wrongly conclude the
/// handle never existed -- reproducing the exact A2-23 bug this ledger
/// exists to fix. Deadlock-free: extends the crate's documented
/// `primitives -> terminal_cops -> subscriptions` edge (`J2534Service::
/// terminal_cops`'s doc comment, `service.rs`) from momentary to
/// call-spanning; no site anywhere in this crate acquires `primitives`
/// while already holding `subscriptions` or `terminal_cops` (verified,
/// ADR-128).
///
/// **Terminal statuses only.** Never route `PduCopstExecuting`/
/// `PduCopstWaiting` through this -- those must not touch `primitives` at
/// all (a COP must stay tracked while running).
///
/// Not a universal replacement for every terminal-emission site in this
/// file: `cancel_link_cops` (batch removal, needs its own critical section
/// spanning multiple COPs) and `dispatch_tx_item`'s WAITING/CANCELLED tail
/// (already holds `logical_links` and `primitives` together for an
/// unrelated invariant) have their own hand-written equivalents; see their
/// own doc comments.
///
/// **Drains `cancelled_cops` on the winning call** (P2 backlog fix, PR #78
/// edge-case-hunter follow-up): once THIS call has won
/// the `primitives` removal above, the `primitives` guard is dropped and
/// `logical_links` is separately (never nested -- this crate's documented
/// lock order is `logical_links` before `primitives`, never the reverse)
/// reacquired to remove any stale `cancelled_cops` mark for `cop_handle` on
/// `cll_handle`. This subsumes `drain_cancelled_cop_if_finalized`'s own
/// former use at the `reap_expired_cyclic_registrants` terminal-emission
/// site (removed; see that function's doc comment for the remaining call
/// sites) and closes the leak's normal-completion half: previously, a
/// `rpc_cancel_com_primitive`/batch-cancel mark that lost the race against a
/// normal-completion emission here (`handle_send_recv`'s `remaining == 0`
/// arm, `handle_delay`) never got drained by anything, stranding it in
/// `link.cancelled_cops` for the CLL's remaining lifetime.
pub(in crate::service) async fn emit_terminal_if_live(
    primitives: &Arc<Mutex<HashMap<u32, CopEntry>>>,
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    subscriptions: &Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    terminal_cops: &Arc<Mutex<TerminalCopsLedger>>,
    cll_handle: u32,
    cop_handle: u32,
    status: PduComPrimitiveStatus,
) {
    let queue_target = resolve_queue_target(logical_links, cll_handle).await;
    let removed = {
        let mut prims = primitives.lock().await;
        let removed_entry = prims.remove(&cop_handle);
        let removed = removed_entry.is_some();
        if let Some(entry) = removed_entry {
            send_cop_status(
                subscriptions,
                terminal_cops,
                queue_target.as_ref(),
                cll_handle,
                cop_handle,
                status,
                entry.cop_tag,
            )
            .await;
        }
        removed
    };
    if removed && let Some(link) = logical_links.lock().await.get_mut(&cll_handle) {
        link.cancelled_cops.remove(&cop_handle);
    }
}

/// Nonterminal-status counterpart to [`emit_terminal_if_live`] (Codex
/// review, P2, PR #101, round 12, ADR-192): closes the same
/// check-then-emit race for a status that must NOT remove `cop_handle`
/// from `primitives` -- unlike its terminal sibling, this never mutates
/// `primitives`, it only gates the emission on the entry still being
/// present there, holding the SAME `primitives` `MutexGuard` across both
/// the liveness check and the `send_cop_status` call (never dropped and
/// reacquired in between) so that no concurrent terminal-finalization path
/// -- every one of which also mutates `primitives` under its own lock --
/// can remove the entry and emit its own terminal status in the gap.
///
/// **The race this closes**: `rpc_primitive.rs`'s
/// `finalize_or_orphan_broadcast_periodic_start_locked` commits a broadcast-
/// periodic COP's real entry into `logical_links` (replacing the
/// `None`-sentinel in-flight reservation) under `logical_links`, then
/// releases that lock -- and, as of ADR-193, `self.api` too -- before its
/// bookkeeping half reports `PduCopstExecuting`. Once the
/// sentinel is gone, a concurrent `CLEAR_PERIODIC_MSGS`/suspension-
/// termination/`CoptCancel` can see the freshly-committed entry, take it,
/// stop it natively, remove the COP from `primitives`, and emit a terminal
/// status -- all before that deferred `Executing` report runs. An
/// unconditional `send_cop_status` there would then surface `Executing`
/// AFTER the terminal event, violating status monotonicity. Gating the
/// emission on `primitives` still containing `cop_handle`, under a lock
/// held across the send, closes that gap the same way
/// `emit_terminal_if_live` already closes the analogous terminal-side gap.
///
/// **Why `primitives` containment alone is sufficient here** (no
/// additional `cancelled_cops`/`terminal_cops` check needed): round 10's
/// Fix A already established -- and every `cancelled_cops.extend`/
/// `.insert` site in this crate excludes -- that a broadcast-periodic COP's
/// own `cop_handle` is never marked `cancelled_cops` while still genuinely
/// live and un-finalized, matching how every other terminal-finalization
/// site in this file already treats bare `primitives` containment as
/// authoritative.
///
/// Not a replacement for [`send_cop_status`]'s other direct callers: those
/// that don't race a concurrent removal of the SAME `cop_handle` (or that
/// already hold `primitives` themselves for an unrelated invariant) have no
/// need for this extra gate.
pub(in crate::service) async fn emit_nonterminal_if_live(
    primitives: &Arc<Mutex<HashMap<u32, CopEntry>>>,
    subscriptions: &Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    terminal_cops: &Arc<Mutex<TerminalCopsLedger>>,
    queue_target: Option<&CllQueueTarget>,
    cll_handle: u32,
    cop_handle: u32,
    status: PduComPrimitiveStatus,
) {
    let prims = primitives.lock().await;
    if let Some(entry) = prims.get(&cop_handle) {
        send_cop_status(
            subscriptions,
            terminal_cops,
            queue_target,
            cll_handle,
            cop_handle,
            status,
            entry.cop_tag.clone(),
        )
        .await;
    }
}

/// ADR-182 follow-up fix (Codex review round, PR #78; batch-cancel sites
/// added in a later PR #78 edge-case-hunter follow-up): drains a possibly
/// stale `cancelled_cops` mark for `cop_handle` once `primitives` no longer
/// contains it. Shared by four call sites that each close one instance of
/// the same leak class, all stemming from a read-then-mark structure (two
/// separate lock acquisitions, or a lookup separated from a later extend)
/// racing the maintenance reap (`reap_expired_cyclic_registrants`):
///
/// - `rpc_cancel_com_primitive` itself, right after its own mark-and-defer
///   `cancelled_cops.insert` -- covers the reap having already fully
///   finished (including the `primitives` removal) while that insert was
///   in flight.
/// - `CoptStopcomm`'s cancel-all block (`rpc_primitive.rs`), right after its
///   `cancelled_cops.extend` -- same race, batch form.
/// - `PDU_IOCTL_CLEAR_TX_QUEUE`'s handler (`rpc_misc.rs`), right after its
///   `cancelled_cops.extend` -- same race, batch form.
/// - `cancel_send_recv_cops_for_cll` (`events_j1939_claim.rs`), right after
///   its own `cancelled_cops.extend` -- same race, batch form; the cop
///   handles it drains are the same IS-CYCLIC/tier-2-eligible ones the
///   maintenance reap operates on, per that function's own doc comment.
///
/// `reap_expired_cyclic_registrants`'s own terminal-emission site used to be
/// a fifth call site (right after its `emit_terminal_if_live` call) but no
/// longer needs one: `emit_terminal_if_live` now performs the equivalent
/// drain internally on its own winning call (see its doc comment), which
/// covers every normal-completion caller, this reap site included.
///
/// A `primitives` miss unconditionally means this COP has already reached a
/// terminal outcome by some path (this call's own removal, a sibling reap,
/// an explicit cancel, or destruction) -- design-advisor's traced
/// interleaving (ADR-182's Consequences section) established this is
/// legally equivalent to ADR-128's already-documented already-terminal
/// no-op-success outcome, never a silently-overridden cancel. The mark
/// itself is otherwise inert (never consulted independently of
/// `primitives`/`registrants` state) but leaks a `u32` in `link.
/// cancelled_cops` forever if nothing ever drains it -- this closes that
/// leak. Two sequential, non-nested lock acquisitions (primitives, then
/// logical_links); the crate's documented logical_links-before-primitives
/// order governs nesting only, which never occurs here. Returns `true` only
/// when a mark was actually found and removed (log-only; neither caller
/// branches on this for its own control flow).
pub(in crate::service) async fn drain_cancelled_cop_if_finalized(
    primitives: &Arc<Mutex<HashMap<u32, CopEntry>>>,
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    cll_handle: u32,
    cop_handle: u32,
) -> bool {
    if primitives.lock().await.contains_key(&cop_handle) {
        return false;
    }
    logical_links
        .lock()
        .await
        .get_mut(&cll_handle)
        .is_some_and(|link| link.cancelled_cops.remove(&cop_handle))
}

/// Records `error` in the link state and sends an error event to the subscriber
/// for `cll_handle`, if any. `cop` is the ComPrimitive this error pertains to
/// -- its handle paired with its already-captured `CopEntry::cop_tag`
/// (`ErrorCop`) -- if any (ISO 22900-2 §9.4.7 c) / §9.6.2: an error
/// associated with a specific ComPrimitive must carry that ComPrimitive's
/// handle, mirroring native `PDU_EVENT_ITEM.hCop`) -- `None` for a module- or
/// CLL-scoped error (native `PDU_HANDLE_UNDEF`).
///
/// **ADR-205 Decision item 1: this function performs no `primitives` lookup
/// of its own.** It used to (ADR-204): a fresh, internal, call-time
/// `resolve_cop_tag` inside this function, correct only when nothing could
/// have removed `cop_handle`'s `primitives` entry between the caller's own
/// liveness decision and this call. Across four independent Codex review
/// rounds on ADR-204's own PR #116, that assumption was falsified four times
/// at four different call sites -- each fixed narrowly, each time review
/// found another instance (see ADR-205's Context for the full list). Rather
/// than keep auditing call sites one review round at a time, this function's
/// former two-function split (a fresh-lookup `send_error_event` delegating to
/// a caller-supplied-tag `send_error_event_with_tag`) was collapsed into this
/// single signature: the compiler now enumerates and forces every call site
/// in the crate to supply a pre-captured `ErrorCop`, the same fix shape the
/// narrowly-scoped point-fixes already applied one site at a time.
///
/// This is cheap because `CopEntry::cop_tag` is write-once (set only at
/// `StartComPrimitive` insertion, `rpc_primitive.rs`, never mutated again for
/// that COP's lifetime): any earlier read of it, from whenever the calling
/// code already had the `CopEntry` in hand for some other reason (almost
/// always available from whatever read decided this event should fire at
/// all), remains valid forever -- there is no "the tag changed since I read
/// it" case to guard against, only "was this read before or after the entry
/// might have been removed."
///
/// **Never inline a fresh `resolve_cop_tag(...).await` immediately before
/// calling this function** to make a call site compile -- that reproduces
/// the exact bug this signature exists to prevent, just with extra steps
/// (ADR-205's own "Risks & checks" section calls this out explicitly). Source
/// `cop` from whatever read already decided this event should fire.
pub(in crate::service) async fn send_error_event(
    subscriptions: &Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    cll_handle: u32,
    error: PduErrorEvent,
    cop: Option<ErrorCop>,
) {
    let (cop_handle, cop_tag) = match cop {
        Some((cop_handle, cop_tag)) => (Some(cop_handle), cop_tag),
        None => (None, None),
    };
    let timestamp = module_timestamp_us();
    let cop = cop_handle.map(|cop_handle| CopRef {
        cll_handle,
        cop_handle,
    });
    // Persist the error (with the time it occurred) so a later failing RPC
    // on this CLL can surface it as ErrorDetail.error_event_data without a
    // subscription (ADR-105), and snapshot the queue this CLL's
    // GetEventItem pollers drain from -- cloned under this same
    // `logical_links` critical section, then locked separately below (the
    // established clone-then-drop-then-lock pattern this crate already uses
    // for `rx_buf`, e.g. `rpc_get_event_item`/`ioctl_set_event_queue_properties`;
    // no new lock ordering).
    let rx_buf = {
        let mut links = logical_links.lock().await;
        links.get_mut(&cll_handle).map(|link| {
            link.last_error = Some(TrackedError {
                event: error,
                timestamp,
                cop,
                cop_tag: cop_tag.clone(),
            });
            Arc::clone(&link.rx_buf)
        })
    };
    let Some(rx_buf) = rx_buf else {
        // No `LogicalLinkState` for this CLL (defensive -- should not
        // normally happen): nothing to enqueue into and no backlog to
        // drain, but a live subscriber (if any) still gets this event's own
        // notification directly, matching pre-this-fix behavior for this
        // corner case.
        let subs = subscriptions.lock().await;
        if let Some(sub) = subs.get(&(DEFAULT_MODULE_HANDLE, cll_handle)) {
            let n = make_cll_notification(
                cll_handle,
                cop_handle,
                timestamp,
                EventItemData::ErrorData(error as i32),
                cop_tag,
            );
            let _ = sub.send(Ok(n));
        }
        return;
    };
    // Enqueue the error event for GetEventItem pollers too, not just
    // SubscribeEvent subscribers -- both are the promised replacement for
    // the removed GetLastError polling path (ADR-105 revision). Shares
    // rx_buf's existing FIFO/capacity policy with received frames, matching
    // ISO 22900-2's single typed per-handle event queue. `deliver_or_enqueue`
    // implements the ADR-115 round-3 correction: this item is always pushed
    // through the cap/mode-enforcing queue first, then reads the queue's own
    // `live_sender` (ADR-115 round 6 -- no subscriber is captured here ahead
    // of time) and, if present, opportunistically drains rx_buf's full
    // current contents live in the same critical section.
    deliver_or_enqueue(
        &rx_buf,
        cll_handle,
        CllQueueItem::Error(TrackedError {
            event: error,
            timestamp,
            cop,
            cop_tag,
        }),
    )
    .await;
}

/// Sends a `PDU_IT_INFO` event to the system-handle subscriber, if any,
/// and pushes the event into `system_event_buf` for `GetEventItem` pollers.
///
/// Called when the set of available VCI modules changes (device opened or lost).
/// System-level subscribers call `GetModuleIds` after receiving this event to
/// learn the updated module list.
pub(in crate::service) async fn send_system_info(
    subscriptions: &Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    system_event_buf: &Arc<Mutex<VecDeque<EventItem>>>,
    info: PduInfo,
) {
    let timestamp = module_timestamp_us();
    let item = EventItem {
        cop_handle: None,
        timestamp,
        data: Some(EventItemData::InfoData(info as i32)),
        cop_tag: None,
    };
    {
        let mut buf = system_event_buf.lock().await;
        if buf.len() >= SYSTEM_EVENT_BUF_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(item.clone());
    }
    let subs = subscriptions.lock().await;
    if let Some(sub) = subs.get(&SYSTEM_SUBSCRIPTION_KEY) {
        let n = EventNotification {
            handle: Some(EventNotificationHandle::SystemHandle(SystemHandle {})),
            event_data: Some(EventNotificationData::Item(item)),
        };
        let _ = sub.send(Ok(n));
    }
}

/// Sends a `module_status` event to the module-level subscriber, if any,
/// and pushes the event into `module_event_buf` for `GetEventItem` pollers.
pub(super) async fn send_module_status(
    subscriptions: &Arc<Mutex<HashMap<SubscriptionKey, SubscriptionSender>>>,
    module_event_buf: &Arc<Mutex<VecDeque<EventItem>>>,
    status: PduModuleStatus,
) {
    let timestamp = module_timestamp_us();
    let item = EventItem {
        cop_handle: None,
        timestamp,
        data: Some(EventItemData::ModuleStatus(status as i32)),
        cop_tag: None,
    };
    {
        let mut buf = module_event_buf.lock().await;
        if buf.len() >= MODULE_EVENT_BUF_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(item.clone());
    }
    let subs = subscriptions.lock().await;
    if let Some(sub) = subs.get(&(DEFAULT_MODULE_HANDLE, PDU_HANDLE_UNDEF)) {
        let n = EventNotification {
            handle: Some(EventNotificationHandle::ModuleHandle(ModuleHandle {
                module_handle: DEFAULT_MODULE_HANDLE,
            })),
            event_data: Some(EventNotificationData::Item(item)),
        };
        let _ = sub.send(Ok(n));
    }
}
