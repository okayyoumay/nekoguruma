use super::*;

/// A `CP_EnableConcatenation` buffer's contents at the moment it was
/// finalized (`finalize_concat_buffers`), ready to become one `ReceivedFrame`
/// delivery. As of the ADR-148 Amendment, finalization only ever happens at
/// the receive-phase deadline (`wait_for_expected_response_inner`) -- never
/// on a differing-key frame arriving -- so every `ConcatDelivery` is built by
/// that one path, possibly several per deadline expiry (one per still-open
/// buffer). `bind_frame`'s own `finalized_concat` out-parameter is kept as a
/// `Vec<ConcatDelivery>` purely so the empty-payload-match arm (edge-case-
/// hunter Finding 3) can still force-finalize every open buffer inline
/// before treating the empty match as its own completion.
pub(super) struct ConcatDelivery {
    pub(super) cop_handle: u32,
    pub(super) acceptance_id: u32,
    pub(super) timestamp: u32,
    pub(super) header_bytes: Vec<u8>,
    pub(super) footer_bytes: Vec<u8>,
    pub(super) unique_resp_identifier: u32,
    pub(super) rx_status_flags: u8,
    pub(super) data: Vec<u8>,
}

impl ConcatDelivery {
    /// `cop_tag` (ADR-204) is resolved by the caller against
    /// `self.cop_handle` (always `Some` here -- a `ConcatDelivery` is always
    /// already bound to a specific COP) before this buffer is consumed,
    /// since this method has no lock access of its own -- either freshly
    /// from `primitives` (`finalize_and_deliver_concat_buffers_if_live`), or
    /// from a per-pass `CllRxEntry::cop_tags` snapshot already captured
    /// earlier (`deliver_concat_batch_if_live`, ADR-204 Codex review round
    /// 3) -- see each caller's own doc comment for which, and why.
    fn into_received_frame(self, cop_tag: Option<Vec<u8>>) -> ReceivedFrame {
        ReceivedFrame {
            timestamp: self.timestamp,
            data: self.data,
            header_bytes: self.header_bytes,
            footer_bytes: self.footer_bytes,
            unique_resp_identifier: self.unique_resp_identifier,
            acceptance_id: self.acceptance_id,
            cop_handle: Some(self.cop_handle),
            cop_tag,
            rx_status_flags: self.rx_status_flags,
            // Accepted residual (ADR-146/148 interaction, merge finding, PR
            // #17): a finalized `CP_EnableConcatenation` buffer's own RxFlag
            // never carries `ECU_TIMING_CHANGE`, even though the buffer's
            // OPENING segment may have already been paired by
            // `observe_registrant_timing_change` (the ComParam side effect
            // itself -- `r.pending_timing_change` -- is captured at open
            // time regardless, so hardware application is unaffected; only
            // this specific delivered frame's own diagnostic flag is
            // conservatively `false`). Retrofitting the flag onto a
            // finalized delivery would need `ConcatBuf` to carry its own
            // opening segment's `(ecu_timing_change, superseded_timing_seq)`
            // through to finalization -- not done here since a qualifying
            // `0xC3` response never actually needs concatenation in
            // practice (see `observe_registrant_timing_change`'s own doc
            // comment).
            ecu_timing_change: false,
            // ISO15765 concatenation reassembly is never SW-CAN-native
            // (SW-CAN does not use `CP_EnableConcatenation`), so this is
            // always correctly `false` -- not itself an accepted residual
            // the way `ecu_timing_change: false` above is (ADR-191).
            sw_can_hv_rx: false,
        }
    }
}

impl ConcatBuf {
    /// Consumes this buffer into a delivery-ready [`ConcatDelivery`] -- the
    /// one conversion both `finalize_concat_buffers` (deadline/empty-payload
    /// triggers) and `finalize_one_concat_buffer` (ADR-148 third Amendment
    /// Fix 2's per-buffer cap trigger) share, so the field mapping is defined
    /// exactly once.
    fn into_delivery(self, cop_handle: u32) -> ConcatDelivery {
        ConcatDelivery {
            cop_handle,
            acceptance_id: self.acceptance_id,
            timestamp: self.timestamp,
            header_bytes: self.header_bytes,
            footer_bytes: self.footer_bytes,
            unique_resp_identifier: self.unique_resp_identifier,
            rx_status_flags: self.rx_status_flags,
            data: self.data,
        }
    }
}

/// Finalizes ALL of `r`'s open `CP_EnableConcatenation` buffers (if any) as
/// completed logical matches: drains `r.concat` (in `Vec` order, i.e.
/// first-opened first), incrementing `r.matches_got` by 1 per buffer (same
/// field a normal, non-concat accepted match increments -- each merged
/// multi-segment response counts as exactly one match toward
/// `matches_needed`), and returns every buffer's contents ready for
/// delivery. Returns an empty `Vec` when no buffer is open.
///
/// Deliberately does NOT run the normal accepted-match side effects
/// (`migrate_on_first_match` tier flip, `cyclic_deadline` restart) that
/// `bind_registrant`'s non-concat acceptance arm runs: both are structurally
/// unreachable for a concat-eligible registrant (`concat_enabled` excludes
/// every IS-CYCLIC and created-receive-only registrant -- see
/// `CopRegistrant::concat_enabled`'s own doc comment -- which are the only
/// registrants that ever have `migrate_on_first_match` or `cyclic_timeout_ms`
/// set), asserted defensively below rather than silently duplicated.
pub(super) fn finalize_concat_buffers(r: &mut CopRegistrant) -> Vec<ConcatDelivery> {
    if r.concat.is_empty() {
        return Vec::new();
    }
    debug_assert!(
        !r.migrate_on_first_match,
        "a concat-eligible registrant is never the true IS-CYCLIC shape"
    );
    debug_assert!(
        r.cyclic_timeout_ms.is_none(),
        "a concat-eligible registrant is never created-receive-only"
    );
    let cop_handle = r.cop_handle;
    let mut deliveries = Vec::with_capacity(r.concat.len());
    for buf in r.concat.drain(..) {
        r.matches_got += 1;
        deliveries.push(buf.into_delivery(cop_handle));
    }
    deliveries
}

/// ADR-148 third Amendment (Fix 2): force-finalizes the ONE buffer at
/// `r.concat[pos]` -- removing it from `concat` and incrementing
/// `r.matches_got` by 1, same per-buffer bookkeeping `finalize_concat_buffers`
/// does per drained entry -- used when an absorb would push that single
/// buffer's `data.len()` over `CONCAT_MAX_BUF_BYTES` or its own `segments`
/// count over `CONCAT_MAX_BUF_SEGMENTS`. Unlike `finalize_concat_buffers`,
/// this NEVER touches any other currently-open buffer on the same
/// registrant -- a cap hit on one ECU's buffer must not force-finalize a
/// different ECU's still-legitimately-accumulating one.
pub(super) fn finalize_one_concat_buffer(r: &mut CopRegistrant, pos: usize) -> ConcatDelivery {
    let cop_handle = r.cop_handle;
    let buf = r.concat.remove(pos);
    r.matches_got += 1;
    buf.into_delivery(cop_handle)
}

/// `finalize_concat_buffers`, but gated atomically on the ADR-086
/// `channel_id`/`connect_generation` staleness guard against the LIVE
/// `LogicalLinkState` -- the guard check, the drain/finalize (which commits
/// the live registrant's own `matches_got`, once per buffer), AND the
/// delivery of each finalized buffer into the link's `rx_buf` queue all run
/// under ONE `logical_links` lock acquisition. This closes the round-7
/// Codex finding against the prior split (guard+drain under the lock, then
/// delivery after releasing it): a `DisconnectComLogicalLink`/reconnect
/// landing in the await gap between "drain the buffers" and "deliver them"
/// could clear the `channel_id`/cancel the COP, yet the already-drained
/// batch would still land in the retained `rx_buf` queue -- which
/// `DisconnectComLogicalLink` does not destroy, only `DestroyComLogicalLink`
/// does -- reaching a live or reconnected subscriber as a stale
/// post-cancellation delivery. Holding `logical_links` across the
/// `deliver_or_enqueue` `.await` calls is safe: that function only ever
/// acquires the innermost per-CLL queue lock, never `logical_links`, so this
/// does not reverse the documented lock order (`logical_links ->
/// subscriptions -> per-CLL queue`, see the ordering note near
/// service.rs:3532-3535) and is precedented by `cancel_link_cops`, which
/// calls `send_cop_status` (itself an enqueue) while holding `primitives`.
///
/// (History: an earlier two-acquisition version -- guard+drain in one
/// acquisition, deliver in a second -- had already been fixed once, per the
/// ADR-148 Amendment, to stop unconditionally committing `matches_got` on a
/// guard failure. That version is what this rewrite folds into a single
/// acquisition.)
///
/// Returns `(delivered_count, live_concat_segments_got)`. `delivered_count`
/// is the number of buffers delivered this pass -- `0` if the guard failed
/// (`cll_handle` is missing, or its `channel_id`/`connect_generation` no
/// longer match) or if the registrant had no open buffers. Callers must
/// only advance `matches_got`/evaluate completion when this is `> 0`,
/// exactly as before. `live_concat_segments_got` is the registrant's own
/// `concat_segments_got` read fresh in this SAME lock acquisition (`0` when
/// `delivered_count` is `0`, meaningless in that case) -- ADR-148 Amendment
/// 8's second fix (edge-case-hunter finding on the round-9 `Matched`-delta
/// fix), so the caller can resync its own local baseline to this absolute
/// value on deadline expiry instead of a delta computed from
/// `delivered_count` alone (buffers, not segments -- cannot reconstruct
/// it). **Currently defense-in-depth, not a live-race fix** (design-advisor
/// correction, same day, post-approval; see ADR-148 Amendment 8's own
/// Correction paragraph for the full trace): the scenario originally cited
/// to justify this -- a sibling COP's own poll pass absorbing a segment
/// into THIS registrant's buffer unseen -- is unreachable today, since
/// every `bind_frame` call for a channel runs on that channel's single
/// `poll_channel_events` task (dispatch handlers and wait loops are all
/// awaited inline in it, never spawned), the one real second poller (a
/// UUDT companion task) is CAN-only while concat requires KWP/J1850, and a
/// concat registrant only exists while its own wait loop is running. Every
/// live `concat_segments_got` advance therefore already reaches this loop's
/// own baseline via `check_match_against_baseline`'s report before this
/// function ever runs, making this resync currently a no-op -- kept because
/// it is cheap and would become load-bearing if any of those premises
/// changes (e.g. this ADR's own flagged possible extension of concat to
/// tier-2/detached registrants). Reading `concat_segments_got` is safe
/// here regardless: the drain (`finalize_concat_buffers`) never touches
/// that field, only `concat`/`matches_got`.
pub(super) async fn finalize_and_deliver_concat_buffers_if_live(
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    primitives: &Arc<Mutex<HashMap<u32, CopEntry>>>,
    cll_handle: u32,
    channel_id: ChannelId,
    connect_generation: u64,
    cop_handle: u32,
) -> (u32, u32) {
    let mut links = logical_links.lock().await;
    let Some(l) = links
        .get_mut(&cll_handle)
        .filter(|l| l.channel_id == Some(channel_id) && l.connect_generation == connect_generation)
    else {
        return (0, 0);
    };
    let rx_buf = Arc::clone(&l.rx_buf);
    let Some(r) = l
        .registrants
        .iter_mut()
        .find(|r| r.cop_handle == cop_handle)
    else {
        return (0, 0);
    };
    // ADR-204/ADR-205 follow-up (design-advisor + edge-case-hunter review,
    // round 5): `primitives` liveness is checked BEFORE `finalize_concat_
    // buffers(r)` below, not after -- draining is irreversible (it empties
    // `r.concat` and bumps `r.matches_got`), so bailing on an absent entry
    // AFTER already draining would silently discard the buffered ECU
    // response while still reporting `matches_got` as if it had been
    // delivered (round 5's first attempt at this fix made exactly that
    // mistake). This mirrors the channel_id/generation guard immediately
    // above, which likewise bails before touching `r` at all. `primitives`
    // is acquired while `links` is still held -- this crate's documented
    // lock order is `logical_links -> primitives`, never the reverse, so
    // this is safe and preserves this function's established "hold `links`
    // across the whole delivery loop" invariant (see
    // `deliver_concat_batch_if_live`'s own doc comment, which mirrors this
    // function's pattern). Unlike the old `resolve_cop_tag` helper this
    // replaced, an absent `primitives` entry bails the WHOLE batch rather
    // than flattening to `cop_tag: None` and still delivering: an absent
    // entry means a concurrent cancel/teardown already claimed this COP and
    // emitted its own terminal status, so `r`'s buffered data must not be
    // drained or delivered for it either. This is enforced unconditionally,
    // not only when reachable through the `channel_id`/`connect_generation`
    // filter above -- it encodes the same general invariant as every other
    // EXECUTING/data-delivery gate in this file, not a fix scoped to one
    // reachability window.
    let prims = primitives.lock().await;
    let Some(entry) = prims.get(&cop_handle) else {
        return (0, 0);
    };
    let cop_tag = entry.cop_tag.clone();
    drop(prims);
    let finalized = finalize_concat_buffers(r);
    let live_concat_segments_got = r.concat_segments_got;
    let delivered_count = finalized.len() as u32;
    for delivery in finalized {
        deliver_or_enqueue(
            &rx_buf,
            cll_handle,
            CllQueueItem::Frame(delivery.into_received_frame(cop_tag.clone())),
        )
        .await;
    }
    (delivered_count, live_concat_segments_got)
}

/// Delivers an already-drained batch of `ConcatDelivery`s -- populated by
/// `bind_registrant`'s inline (non-deadline-expiry) force-finalize triggers
/// (the empty-payload-match arm's force-finalize-all, and the byte/segment
/// cap hit's force-finalize-one) against a per-poll-pass SNAPSHOT registrant,
/// not live state -- behind the same ADR-086 `channel_id`/`connect_generation`
/// staleness guard `finalize_and_deliver_concat_buffers_if_live` already
/// applies to the deadline-expiry path (ADR-148 Amendment 6). Closes the
/// separate, concat-specific gap that fix didn't cover: a concurrent
/// `DisconnectComLogicalLink` landing between `build_cll_rx_entries`'s
/// snapshot and this poll pass's frame loop reaching this delivery could
/// otherwise push an already-cancelled COP's buffered response into a queue
/// a live/reconnected subscriber then receives (ADR-148 Amendment 9).
///
/// On guard failure, the WHOLE batch is silently dropped -- never partially
/// delivered. This is self-consistent with the snapshot/writeback lifecycle:
/// whatever teardown caused the guard to fail also makes
/// `merge_registrant_writeback` discard this pass's entire `matches_got`
/// delta, since `matches_got` was only ever advanced on the SNAPSHOT copy by
/// these same force-finalize calls -- so "delivery dropped" and "matches_got
/// contribution discarded" always travel together, never diverging. Two
/// distinct teardown shapes both hold this: a reconnect bumps
/// `connect_generation`, failing THIS guard directly, and the writeback's
/// own per-registrant lookup then also misses (a fresh registrant list after
/// reconnect has no entry matching this pass's snapshot); a disconnect
/// WITHOUT reconnect instead clears `channel_id` (leaving
/// `connect_generation` itself unchanged) and `cancel_link_cops` clears
/// `registrants` outright, so this guard's `channel_id` comparison fails
/// directly, and the writeback separately no-ops via its own
/// registrant-not-found branch, not a generation mismatch -- different
/// mechanisms on each side, same outcome (both drop) either way.
///
/// The `logical_links` guard is held across the ENTIRE delivery loop below
/// (mirroring `finalize_and_deliver_concat_buffers_if_live`'s own
/// established pattern) -- do not clone `l.rx_buf` and drop the guard before
/// delivering, that would silently reintroduce the exact race this closes.
/// Safe to hold across `deliver_or_enqueue`'s `.await` calls: `deliver_or_enqueue`
/// only ever acquires the innermost per-CLL queue lock, never `logical_links`
/// itself, so this doesn't reverse the documented `logical_links ->
/// subscriptions -> per-CLL queue` lock order.
///
/// `channel_id` here is the caller's own polling identity (`ctx.channel_id`
/// at the call site), compared against `l.channel_id` -- for a normal
/// (non-companion) `CllRxEntry` these are the same channel, but a UUDT
/// companion poll pass's `ctx.channel_id` is deliberately the COMPANION
/// channel, not `l.channel_id` (`build_cll_rx_entries`'s own
/// `RxEntryKind::Companion` branch), so this guard would always fail for a
/// companion-sourced batch. Currently harmless only because `concat_enabled`
/// is KWP/J1850-only while a UUDT companion is CAN-only (ADR-046) -- the two
/// never coexist today, so `finalized_concat` is never non-empty on a
/// Companion entry. Would need revisiting if concat's scope is ever widened
/// to CAN/ISO15765 (the same possible-future-extension case
/// `finalize_and_deliver_concat_buffers_if_live`'s own doc comment flags).
///
/// `cop_tags` (ADR-204 Codex review, PR #116, round 3): the caller's own
/// per-pass `CllRxEntry::cop_tags` snapshot, NOT a fresh `primitives` lookup
/// -- this batch's `ConcatDelivery`s were drained from a registrant matched
/// against that SAME snapshot (`bind_registrant`, via `bind_frame`'s
/// `finalized_concat` out-parameter), so resolving fresh here, after the
/// `.await` points already elapsed earlier this same frame iteration (the
/// `CP_SuspendQueueOnError` eager-publish re-lock, this very function's own
/// `logical_links` re-acquisition), could otherwise race a concurrent
/// `CancelComPrimitive`/link-teardown path that already removed the entry.
/// See `CllRxEntry::cop_tags`'s own doc comment for the full argument --
/// this is the concat-path counterpart to `resolve_frame_cop_tag`.
pub(super) async fn deliver_concat_batch_if_live(
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    cop_tags: &HashMap<u32, Vec<u8>>,
    cll_handle: u32,
    channel_id: ChannelId,
    connect_generation: u64,
    batch: Vec<ConcatDelivery>,
) {
    let links = logical_links.lock().await;
    let Some(l) = links
        .get(&cll_handle)
        .filter(|l| l.channel_id == Some(channel_id) && l.connect_generation == connect_generation)
    else {
        return;
    };
    for delivery in batch {
        let cop_tag = cop_tags.get(&delivery.cop_handle).cloned();
        deliver_or_enqueue(
            &l.rx_buf,
            cll_handle,
            CllQueueItem::Frame(delivery.into_received_frame(cop_tag)),
        )
        .await;
    }
}

/// Evaluates the SAME ADR-086 `channel_id`/`connect_generation` liveness
/// guard `wait_for_expected_response_inner`'s RC21/23 retransmit arm already
/// used (pre-existing inline check, now folded into this helper), and
/// ADDITIONALLY discards (never delivers) every open `CP_EnableConcatenation`
/// buffer on `cop_handle`'s registrant when the guard holds (ADR-148
/// Amendment 10).
///
/// Why discard, not finalize: a retransmit means the ORIGINAL request is
/// about to be re-sent, so any buffer still open at this point is an
/// interrupted partial answer to a request attempt that's about to be
/// superseded. Delivering it as a completed `ResultData` would present
/// truncated data as though it were a complete logical response, and --
/// decisively -- would consume `matches_needed`'s quota: for the common
/// `matches_needed: Some(1)` case, finalizing here would satisfy the whole
/// COP with truncated data, and the eventual real, complete post-retry
/// answer would then never be delivered. This is a deliberate asymmetry with
/// the deadline-expiry finalize path (`finalize_and_deliver_concat_buffers_if_live`):
/// at deadline expiry the phase is ending and no better data will ever come,
/// so partial data is better than none; at the RC21/23 retry boundary, a
/// complete replacement answer is specifically expected imminently, so the
/// partial has a strictly better substitute on the way.
///
/// Scope is ALL open buffers on the registrant, not just whichever ECU's
/// response triggered the NRC: IS-MULTIPLE's single shared registrant
/// (ADR-148 Amendment 1) holds buffers for multiple ECUs, and the retransmit
/// re-sends the ONE functional/broadcast request every one of those buffers
/// was answering, so every buffer crosses the same attempt boundary. Applies
/// identically to both 0x21 and 0x23 (no behavioral distinction between the
/// two codes here); NRC 0x78 is exempt by construction since it never
/// retransmits (no new request attempt exists, so no cross-attempt merge
/// hazard).
///
/// Touches ONLY `r.concat` -- deliberately leaves `r.matches_got` and
/// `r.concat_segments_got` untouched. Those must stay exactly as they are:
/// the monotone-baseline diff contract `check_match_against_baseline` relies
/// on (and Amendment 8's resync) depends on `concat_segments_got` never
/// decreasing, and the caller's own local `concat_segments_got` variable in
/// the wait loop already reflects everything absorbed so far, unaffected by
/// discarding the buffer contents here.
///
/// Returns `true` whenever the guard holds, REGARDLESS of whether a
/// registrant with `cop_handle` was actually found -- matching the exact
/// behavior of the inline guard check this replaces, which also never
/// looked at the registrant, only at `channel_id`/`connect_generation` on
/// the link itself. The caller's downstream decision of whether to actually
/// call `transmit_request` must behave identically to before, aside from
/// the added discard side-effect.
pub(super) async fn discard_concat_buffers_for_retransmit_if_live(
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    cll_handle: u32,
    channel_id: ChannelId,
    connect_generation: u64,
    cop_handle: u32,
) -> bool {
    let mut links = logical_links.lock().await;
    let Some(l) = links
        .get_mut(&cll_handle)
        .filter(|l| l.channel_id == Some(channel_id) && l.connect_generation == connect_generation)
    else {
        return false;
    };
    if let Some(r) = l
        .registrants
        .iter_mut()
        .find(|r| r.cop_handle == cop_handle)
    {
        r.concat.clear();
    }
    true
}
