use std::collections::HashMap;

use tracing::{info, warn};
use vci_service_interface::PduComLogicalLinkStatus;

use super::{events, *};
use crate::error::map_native_error_for_link;

impl J2534Service {
    /// Returns one `ModuleData` row per configured `modules` entry, in
    /// config order (1-based `module_handle` == array position) — or the
    /// single synthetic row (byte-identical to pre-ADR-107 output) when
    /// `modules` is absent. `module_status` (ADR-132, closing ADR-107
    /// Accepted Residual #1 / conformance-audit A2-25) reflects the real
    /// tracked state for the entry matching whichever handle is currently
    /// open (`self.module_state.status` — the same value `GetStatus`'s
    /// `ModuleHandle` branch reads), and `PDU_MODST_AVAIL` (the spec's
    /// detected-but-not-yet-connected initial state, ISO 22900-2 §9.4.24.2.1
    /// Use Case (a)) for every other row: there is still no J2534
    /// enumeration API to probe a *closed* device's real health with, so an
    /// unopened entry is reported as merely detected, not vouched for as
    /// ready.
    ///
    /// Holds the `device_id` guard across the `module_state` read below
    /// (ADR-107 addendum's `device_id` -> `module_state` nested-lock order)
    /// instead of releasing it in between (Codex review on PR #145): a
    /// `ModuleDisconnect(handle A)` followed by a `ModuleConnect(handle B)`
    /// racing a released-then-reacquired pair of locks could otherwise pair
    /// handle A's row with handle B's freshly-reset `Ready` status -- not
    /// just stale, but actively self-contradictory across rows (A shown
    /// `Ready`, B shown `Avail`, when the reverse was true by the time the
    /// response went out). Holding one guard across both reads makes this
    /// call observe a single consistent instant, matching every other
    /// module-scoped RPC's discipline in this file.
    pub(super) async fn rpc_get_module_ids(
        &self,
        _request: Request<vci_service_interface::GetModuleIdsRequest>,
    ) -> Result<Response<vci_service_interface::ModuleIdsResponse>, Status> {
        let device_id_guard = self.device_id.lock().await;
        let open_handle = device_id_guard.as_ref().map(|(h, _)| *h);
        let open_status = match open_handle {
            Some(_) => Some(self.module_state.lock().await.status),
            None => None,
        };
        drop(device_id_guard);

        let module_data = self
            .modules
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let module_handle = index as u32 + 1;
                let module_status = if open_handle == Some(module_handle) {
                    open_status.expect("open_status is Some whenever open_handle is Some")
                } else {
                    vci_service_interface::PduModuleStatus::PduModstAvail
                };
                vci_service_interface::ModuleData {
                    module_type_id: 0,
                    module_handle: Some(vci_service_interface::ModuleHandle { module_handle }),
                    vendor_module_name: entry.label.clone(),
                    vendor_additional_info: self.startup_config.library_name.clone(),
                    module_status: module_status as i32,
                }
            })
            .collect();

        Ok(Response::new(vci_service_interface::ModuleIdsResponse {
            module_id_list: Some(vci_service_interface::ModuleItem { module_data }),
        }))
    }

    /// Opens the J2534 device for `request.module_handle`'s configured
    /// entry (ADR-107). A repeat call for the SAME handle while it is
    /// already open, AND still `PduModstReady`, is a no-op with respect to
    /// the device itself (pre-ADR-107 behavior, generalized). A call for a
    /// DIFFERENT handle while another is already open is rejected with
    /// `PDU_ERR_RESOURCE_BUSY`/`FailedPrecondition` — this service opens at
    /// most one device at a time; switching requires an explicit
    /// `ModuleDisconnect` first (ADR-107 Decision (b)/(d)).
    ///
    /// **`PDU_MODST_NOT_AVAIL` is sticky until `ModuleDisconnect` (ADR-131,
    /// amending ADR-107 Decision (d)).** A module `events::handle_channel_hard_error`
    /// has already marked `PduModstNotAvail` (device left open, per ISO
    /// 22900-2 §9.4.29.2 (e)/NOTE 2 — the handle stays valid specifically so
    /// queued event/error items remain retrievable) is NOT recovered by a
    /// repeat `ModuleConnect` call: this rejects with `PDU_ERR_FCT_FAILED`
    /// (Table 38) rather than reporting a false success, carrying the
    /// tracked `PDU_ERR_EVT_LOST_COMM_TO_VCI` in the `ErrorDetail`.
    /// `PDUModuleConnect` is not itself a loss-of-comm recovery path — ISO
    /// 22900-2 §9.4.29.2 Behaviour (a) requires the module to already be in
    /// `PDU_MODST_AVAIL` and "return error PDU_ERR_FCT_FAILED" if connection
    /// is not possible; NOTE 2 and §9.4.30 prescribe the only recovery
    /// sequence, `PDUModuleDisconnect` then `PDUModuleConnect` again, whose
    /// fresh `PassThruOpen` (inside `ensure_open_device_inner`, which resets
    /// `module_state` to `ModuleState::default()`) is what actually
    /// revalidates the device — J2534 v04.04 has no health-check/ping
    /// primitive to revalidate an already-open handle with. See ADR-131 for
    /// the two prior rounds that oscillated between "stuck forever" and
    /// "false success" before landing here.
    pub(super) async fn rpc_module_connect(
        &self,
        request: Request<vci_service_interface::ModuleConnectRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        let request = request.into_inner();
        Self::require_module_handle(request.module_handle, self.modules.len())?;
        let requested_handle = request
            .module_handle
            .expect("validated Some above")
            .module_handle;
        // Held across the module_state check below (ADR-107 addendum: the
        // nested-lock order is device_id -> module_state) so a concurrent
        // ModuleDisconnect can't close the device out from under this
        // decision between the two locks (TOCTOU). `PDU_INFO_MODULE_LIST_CHG`
        // is no longer emitted here on a first open -- `ensure_open_device_for`
        // (`ensure_open_device_inner`, `service.rs`) now emits it itself, on
        // every none-to-open transition regardless of which RPC triggered it
        // (ADR-132, Codex review on PR #145: `GetVersion`/`CreateComLogicalLink`
        // can also lazily open a device without this function ever running,
        // and `GetModuleIds` now reports real per-handle status that changes
        // on open, so every lazy-open path needs the same notification, not
        // just this one).
        let (_device_guard, _device_id) = self.ensure_open_device_for(requested_handle).await?;
        {
            let module_state = self.module_state.lock().await;
            if module_state.status != vci_service_interface::PduModuleStatus::PduModstReady {
                let last_error = module_state.last_error.clone();
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    format!(
                        "PDU_ERR_FCT_FAILED: module_handle {requested_handle} lost \
                         communication with the VCI -- call ModuleDisconnect, then \
                         ModuleConnect to reconnect"
                    ),
                    PduError::PduErrFctFailed,
                    Some(last_error),
                ));
            }
        }
        info!(library = %self.startup_config.library_name, requested_handle, "ModuleConnect");
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_module_disconnect(
        &self,
        request: Request<vci_service_interface::ModuleDisconnectRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        let request = request.into_inner();
        Self::require_module_handle(request.module_handle, self.modules.len())?;
        let requested_handle = request
            .module_handle
            .expect("validated Some above")
            .module_handle;
        // ADR-107 follow-up fix (Codex review, PR #110): a request for a
        // module OTHER than whatever is currently open must not silently
        // tear down that other module's live links/channels and close its
        // device. Nothing-open and matching-handle both fall through as the
        // pre-existing no-op-ish behavior below. `slot` is held for the
        // teardown sequence below (`device_id` is the outermost lock among
        // the nested set, ADR-107 addendum), then dropped once the close
        // outcome is captured. The two device-derived probe caches
        // (`j1850_bus_flavor`, `resolved_can_channel_mode`) are epoch-tagged
        // and self-validate on read against `device_epoch`, so this function
        // no longer needs to touch either of them at all -- see the epoch
        // bump right after `slot.take()` below and ADR-107 addendum (h).
        let mut slot = self.lock_device_for(requested_handle).await?;

        // ISO 22900-2 §9.3.3: ModuleDisconnect must force-cleanup all resources.
        //
        // Set connected=false on every link and collect their handles atomically
        // before calling cancel_link_cops.  The poll task checks l.connected to
        // decide whether to execute or skip a dequeued TxItem; without this step
        // it would see connected=true and execute items for which
        // cancel_link_cops has already emitted PduCopstCancelled, producing
        // the wrong event sequence (Cancelled → Executing → Finished).
        // This mirrors the pattern in rpc_disconnect_com_logical_link.
        //
        // ADR-139: `shared_channels` is acquired here and held across the
        // `cancel_link_cops` loop below, for the same reason
        // `events::handle_channel_hard_error` now does the same (ADR-139):
        // `slot`/`device_id` alone does not block a connect past its own
        // `finalize_connected_link` call (that call drops the `device_id`
        // guard before publishing `connected = true`), but every such
        // publish itself requires `shared_channels` -- holding it here
        // closes the same "reconnect lands mid-teardown, its live COP gets
        // wrongly cancelled" window this function would otherwise share with
        // the pre-fix `handle_channel_hard_error`. Released immediately
        // after the loop, before `self.shared_channels.lock()` is
        // reacquired below to drain the map -- two separate acquisitions,
        // not one held across the whole function, so this never overlaps
        // the later `module_state` read inside the `if let Some(...) =
        // slot.take()` block (`device_id -> module_state -> shared_channels`
        // order, ADR-107 addendum).
        //
        // ADR-193 amendment (round 17, Codex review, P1, PR #101): tracks
        // whether the loop below takes a `None`-sentinel in-flight-start
        // reservation off any CLL, so the batched `self.api` fence
        // acquisition further down (see its own doc comment) knows whether
        // it has anything to fence at all. Mirrors
        // `events::handle_channel_hard_error`'s own `in_flight_start_taken`
        // (`events.rs`, ADR-193 amendment round 16) declared the same way,
        // before its own `chans` acquisition.
        let mut in_flight_start_taken = false;
        // Round 18 (Codex review, P1, PR #101, ADR-193 amendment): `(channel_id,
        // message_id)`/`(channel_id, msg_id)` pairs for every committed
        // broadcast-periodic/repeat-message entry the loop below finds on a
        // still-live (non-`dead`) channel. Issued later in this same
        // function, as an explicit best-effort native stop, under the
        // `self.api` guard already held for the disconnect/close sequence --
        // see that call site's own doc comment for the full rationale.
        let mut periodic_stop_pairs: Vec<(ChannelId, j2534_0404::PeriodicMessageId)> = Vec::new();
        let mut repeat_stop_pairs: Vec<(ChannelId, u32)> = Vec::new();
        let chans = self.shared_channels.lock().await;
        let (cll_handles, connected_handles): (Vec<u32>, Vec<u32>) = {
            let mut links = self.logical_links.lock().await;
            let connected: Vec<u32> = links
                .iter()
                .filter(|(_, l)| l.connected)
                .map(|(&h, _)| h)
                .collect();
            for link in links.values_mut() {
                link.connected = false;
                // SAE J2534-2 clause 19.3.2.3 (ADR-193 amendment, round 17,
                // Codex review, P1, PR #101): `ModuleDisconnect` was the
                // fifth (and, per this amendment's own audit -- see the ADR
                // -- last) terminator path with no `tp20_broadcast_periodic`
                // handling at all -- `links.clear()` further down this
                // function used to silently wipe a live broadcast periodic
                // (and any in-flight-start reservation) with no fence and no
                // leak-tracking whatsoever.
                //
                // Round 18 (Codex review, P1, PR #101, ADR-193 amendment,
                // supersedes round 17's reasoning below for the `Some(_)`
                // arm): a committed broadcast periodic now gets an explicit
                // best-effort `api.stop_periodic_message` attempt of its
                // own, mirroring the shape
                // `DisconnectComLogicalLink`/`DestroyComLogicalLink`/
                // `handle_channel_hard_error` already use -- an explicit
                // native stop attempt first, not silent reliance on a
                // broader teardown call alone. Round 17 reasoned that the
                // imminent native teardown (`api.disconnect`/`api.close`,
                // further down this function, gated on `slot.take()`
                // returning `Some`) would clear a committed periodic
                // device-side regardless, citing SAE J2534-1 §7.2.4
                // (`PassThruDisconnect`) and §7.2.2 (`PassThruClose`) --
                // also restated together in §7.2.7
                // (`PassThruStartPeriodicMsg`)'s own periodic-message
                // semantics description. That reasoning did not account for
                // `api.disconnect`/`api.close` THEMSELVES failing
                // (edge-case-hunter finding, PR #101 round 18 follow-up):
                // both calls' results are otherwise discarded/only used for
                // this RPC's own return status, so a genuine double failure
                // there used to leave a committed periodic transmitting
                // device-side with no tracking anywhere and no way to ever
                // retry stopping it.
                //
                // The pair collected here (`periodic_stop_pairs`) is only
                // resolved and issued later in this same function, under
                // the `self.api` guard already held for the
                // disconnect/close sequence -- see that call site's own doc
                // comment for the native-call shape and failure handling.
                // Skips (does not collect) any entry whose resolved
                // `SharedChannel` is already `dead` -- mirrors
                // `handle_channel_hard_error`'s own precedent of never
                // attempting a native call against an already-dead channel.
                // `chans` (`shared_channels`) is already held across this
                // whole loop (see its acquisition's own doc comment above),
                // so this lookup is authoritative.
                //
                // Symmetric fix for `repeat_message_ids` (design-advisor
                // consult): round 17's `Some(_)`-arm reasoning leaned on "no
                // per-message `api.stop_repeat_message` attempt here
                // either, for the identical reason" as its own precedent
                // for periodic's (then) no-stop behavior. Now that periodic
                // gets an explicit stop, `repeat_message_ids` gets the
                // identical best-effort treatment below (collected into
                // `repeat_stop_pairs`, issued at the same later call site)
                // so that precedent argument does not go stale/asymmetric.
                //
                // Neither collection here leak-tracks a failed stop into
                // `SharedChannel::leaked_{periodic,repeat}_message_ids`,
                // unlike `DisconnectComLogicalLink`/`DestroyComLogicalLink`:
                // design-advisor confirmed there is nowhere left to
                // leak-track this into that any retry mechanism could ever
                // reach. `slot.take()` further down this function
                // unconditionally clears `self.device_id` regardless of
                // whether the native close that follows succeeds (ADR-107
                // addendum (h)), and the opportunistic
                // `retry_leaked_periodic_message_stops`/
                // `retry_leaked_repeat_message_stops` mechanisms
                // (`rpc_misc.rs`) only ever probe a LIVE `SharedChannel` on
                // an OPEN device -- once this function's own teardown
                // clears `device_id`, no retry mechanism can ever reach
                // this device again, committed or leaked. If the explicit
                // stop AND the later `api.disconnect` AND `api.close` all
                // fail, the message may keep transmitting device-side with
                // no tracking anywhere -- an ACCEPTED RESIDUAL, not a new
                // gap specific to periodic/repeat messages: every other
                // resource class on an abandoned device (filters, other
                // repeat slots, everything else) is equally unreachable the
                // moment `device_id` clears, per ADR-107 addendum (h)'s
                // existing deliberate device-abandonment design --
                // periodic/repeat messages just now share that same
                // already-accepted cost instead of being a silent,
                // undocumented exception to it.
                //
                // The `None`-sentinel in-flight-start-reservation arm is
                // unchanged from round 17: no real message exists yet to
                // stop, so it is tracked via `in_flight_start_taken` below
                // and resolved by the batched `self.api` fence acquisition
                // further down this function, not collected here.
                let live_channel_id = link
                    .channel_key
                    .and_then(|key| chans.get(&key))
                    .filter(|sc| !sc.dead)
                    .map(|sc| sc.channel_id);
                if let Some(periodic) = link.tp20_broadcast_periodic.take() {
                    match periodic.message_id {
                        Some(message_id) => {
                            if let Some(channel_id) = live_channel_id {
                                periodic_stop_pairs.push((channel_id, message_id));
                            }
                        }
                        None => {
                            in_flight_start_taken = true;
                        }
                    }
                }
                if let Some(channel_id) = live_channel_id {
                    for &msg_id in &link.repeat_message_ids {
                        repeat_stop_pairs.push((channel_id, msg_id));
                    }
                }
            }
            let all: Vec<u32> = links.keys().copied().collect();
            (all, connected)
        };
        // ADR-193 amendment (round 17, Codex review, P1, PR #101): the
        // batched acquire-then-release `self.api` serialization fence,
        // mirroring `events::handle_channel_hard_error`'s own fence
        // (`events.rs`, ADR-193 amendment round 16) in shape and rationale.
        // A `None`-sentinel reservation taken above means a racing start's
        // own `revalidate_tp20_broadcast_periodic_reservation`
        // (`rpc_primitive.rs`) has not yet resolved -- briefly acquiring,
        // then immediately releasing, `self.api` here (no native call
        // needed) guarantees the opposite ordering: either that start has
        // not yet revalidated (and will now find its reservation gone --
        // taken above -- and abort without transmitting), or it already
        // committed inside its own `api` bracket and that bracket's own
        // resolution has already orphan-stopped the message it started (via
        // `finalize_or_orphan_broadcast_periodic_start_locked`). This
        // function does NOT block waiting for that start to finish in the
        // first case -- the acquisition finds `self.api` uncontended and
        // returns immediately; it only actually blocks in the second case,
        // until that start's own bracket releases `self.api`. Either way,
        // this acquisition happens strictly before this function proceeds
        // to report any CLL in this sweep's COPs terminal via
        // `cancel_link_cops` below -- which is why this fence lives here,
        // inside the pre-existing `chans`-held window, rather than being
        // deferred past it.
        //
        // Lock order: `api` nests under the already-held `shared_channels`
        // (`chans`, still held here -- not dropped until after the
        // `cancel_link_cops` loop below), sanctioned by ADR-080.
        // `logical_links` is NOT held at this point -- the block above
        // already released it -- so ADR-110's `api`-outer/
        // `logical_links`-inner ordering isn't in play for this specific
        // acquisition. This function's own LATER `self.api.lock()` (for the
        // native disconnect/close calls, further below) is a separate,
        // subsequent acquisition entirely unrelated to this fence -- no
        // interaction, no double-lock.
        if in_flight_start_taken {
            drop(self.api.lock().await);
        }
        for &cll_h in &cll_handles {
            events::cancel_link_cops(
                &self.primitives,
                &self.logical_links,
                &self.subscriptions,
                &self.terminal_cops,
                cll_h,
            )
            .await;
        }
        drop(chans);

        // Notify CLLs that were connected that they are going offline.
        for cll_h in connected_handles {
            events::send_cll_status(
                &self.subscriptions,
                &self.logical_links,
                cll_h,
                PduComLogicalLinkStatus::PduCllstOffline,
            )
            .await;
        }

        // Drop all logical links -- but capture each one's queue `Arc` first
        // so `terminate_all_subscriptions` below can clear `live_sender`
        // correctly; a self-lookup there would find `logical_links` already
        // emptied (the same dead-at-the-real-call-site gap
        // `terminate_subscription`'s own doc comment describes).
        let queues: std::collections::HashMap<
            u32,
            std::sync::Arc<tokio::sync::Mutex<CllEventQueue>>,
        > = {
            let mut links = self.logical_links.lock().await;
            let queues = links
                .iter()
                .map(|(&h, l)| (h, std::sync::Arc::clone(&l.rx_buf)))
                .collect();
            links.clear();
            queues
        };

        // A2-23 (ADR-128, Codex-review round 1): every CLL just torn down
        // above is fully gone, the same as an individual
        // `DestroyComLogicalLink` -- purge (and mark destroyed)
        // `terminal_cops` for all of them, or their entries would linger
        // forever (never observable again, since none of these `cll_handle`s
        // will ever be individually destroyed) and, worse, a straggling
        // terminal emission racing this teardown could resurrect one without
        // the destroyed-marker `purge_many` sets. Mirrors
        // `rpc_destroy_com_logical_link`'s single-CLL `purge` call.
        self.terminal_cops.lock().await.purge_many(&cll_handles);

        // Drop all shared channels: this stops the poll tasks and disconnects
        // the physical channels.  Then close the device.
        let channels: HashMap<_, _> = std::mem::take(&mut *self.shared_channels.lock().await);
        let close_result = if let Some((_, id)) = slot.take() {
            // Every device CLOSE bumps the epoch, attempted or successful
            // (ADR-107 addendum (h)) -- bumped here, before the close is
            // even attempted, so a failed `PassThruClose` below still
            // invalidates `j1850_bus_flavor`/`resolved_can_channel_mode`
            // (both epoch-tagged and self-validating on read; this function
            // no longer clears them directly).
            self.device_epoch
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let api = self.api.lock().await;
            // Round 18 (Codex review, P1, PR #101, ADR-193 amendment):
            // explicit best-effort native stop for every committed
            // broadcast-periodic/repeat-message pair the sweep above
            // collected, issued BEFORE the general disconnect/close
            // teardown just below -- see the collection site's own doc
            // comment (in the `links.values_mut()` loop above) for the full
            // rationale, including why a failure here is never propagated
            // or leak-tracked: this is explicitly best-effort, matching how
            // `DisconnectComLogicalLink`/`DestroyComLogicalLink`/
            // `handle_channel_hard_error` treat a failed stop when there is
            // truly nowhere further to escalate to.
            for (channel_id, message_id) in periodic_stop_pairs {
                if let Err(err) = api.stop_periodic_message(channel_id, message_id) {
                    warn!(
                        channel_id = channel_id.0,
                        message_id = message_id.0,
                        %err,
                        "ModuleDisconnect: best-effort PassThruStopPeriodicMsg for a committed \
                         TP2.0 broadcast periodic COP failed"
                    );
                }
            }
            for (channel_id, msg_id) in repeat_stop_pairs {
                if let Err(err) = api.stop_repeat_message(channel_id, msg_id) {
                    warn!(
                        channel_id = channel_id.0,
                        msg_id,
                        %err,
                        "ModuleDisconnect: best-effort PassThruIoctl(STOP_REPEAT_MESSAGE) for a \
                         repeat-message slot failed"
                    );
                }
            }
            for (_, sc) in channels {
                let _ = api.disconnect(sc.channel_id);
            }
            let last_error = self.module_state.lock().await.last_error.clone();
            api.close(id)
                .map_err(|err| map_native_error_for_link("PassThruClose", &err, Some(last_error)))
        } else {
            // Pins the invariant the no-leak-track decision above rests on:
            // `slot == None` here implies `channels` (already `mem::take`n
            // above) was already empty -- see that comment for the full
            // reachability argument. Extended (round 18) to the same
            // invariant's direct corollary: if `shared_channels` was empty,
            // no link could have resolved a `live_channel_id` above either,
            // so both stop-pair collections must be empty too.
            debug_assert!(
                channels.is_empty(),
                "self.device_id was None but shared_channels was non-empty -- the invariant the \
                 ModuleDisconnect no-leak-track decision above rests on has decoupled"
            );
            debug_assert!(
                periodic_stop_pairs.is_empty() && repeat_stop_pairs.is_empty(),
                "self.device_id was None but a stop pair was collected above -- the same \
                 shared_channels-emptiness invariant this decision rests on has decoupled"
            );
            Ok(())
        };
        drop(slot);

        // Notify system subscribers that the module list has changed --
        // unconditionally, before propagating any `PassThruClose` failure
        // below (Codex review on PR #145). `slot.take()` above already
        // cleared `self.device_id` regardless of whether the native close
        // that follows succeeds; since ADR-132, `GetModuleIds` derives that
        // row's status from `device_id`, so the moment the slot clears, the
        // row has already changed (to `PDU_MODST_AVAIL`) whether or not
        // `close_result` ends up `Err`. Emitting this before `close_result?`
        // (rather than after, as previously) ensures a failed
        // `PassThruClose` still notifies subscribers of that real change
        // instead of silently leaving them on a stale module list -- this
        // still fires (as before) even when nothing was open at all
        // (`close_result` is trivially `Ok(())` in that case), unchanged
        // from the pre-existing no-op-disconnect behavior.
        events::send_system_info(
            &self.subscriptions,
            &self.system_event_buf,
            vci_service_interface::PduInfo::ModuleListChg,
        )
        .await;
        close_result?;

        self.terminate_all_subscriptions(&queues).await;
        info!(library = %self.startup_config.library_name, "ModuleDisconnect");
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_get_version(
        &self,
        request: Request<vci_service_interface::GetVersionRequest>,
    ) -> Result<Response<vci_service_interface::VersionResponse>, Status> {
        let request = request.into_inner();
        Self::require_module_handle(request.module_handle, self.modules.len())?;
        let requested_handle = request
            .module_handle
            .expect("validated Some above")
            .module_handle;

        let (device_guard, device_id) = self.ensure_open_device_for(requested_handle).await?;

        // ADR-134: ISO 22900-2 §9.4.29.2 NOTE 1 lists exactly four functions
        // callable before ModuleConnect (GetResourceIds/GetObjectId/
        // GetConflictingResources/GetStatus); GetVersion is not among them,
        // and its own Table 13 lists PDU_ERR_MODULE_NOT_CONNECTED as a legal
        // return. Reject here, still holding `device_guard`, if a hard
        // channel error has marked this module PduModstNotAvail -- otherwise
        // an already-open-but-stale device would let this lazily "succeed"
        // via ensure_open_device_for's no-op branch, returning stale version
        // info instead. Scoped block: dropped before `api` is locked below,
        // preserving the device_id -> module_state -> api lock order.
        {
            let module_state = self.module_state.lock().await;
            if module_state.status != vci_service_interface::PduModuleStatus::PduModstReady {
                let last_error = module_state.last_error.clone();
                return Err(Self::module_not_avail_status(requested_handle, last_error));
            }
        }

        let api = self.api.lock().await;
        let last_error = self.module_state.lock().await.last_error.clone();
        let version = api.read_version(device_id).map_err(|err| {
            map_native_error_for_link("PassThruReadVersion", &err, Some(last_error))
        })?;
        drop(device_guard);

        let hw_version = Self::parse_u32_token(&version.firmware);
        let fw_version = Self::parse_u32_token(&version.dll);
        let api_sw_version = Self::parse_u32_token(&version.api);

        Ok(Response::new(vci_service_interface::VersionResponse {
            version_data: Some(vci_service_interface::VersionData {
                mvci_part1_standard_version: 0,
                mvci_part2_standard_version: 0,
                hw_serial_number: 0,
                hw_name: version.firmware,
                hw_version,
                hw_date: 0,
                hw_interface: 0,
                fw_name: version.dll,
                fw_version,
                fw_date: 0,
                vendor_name: "j2534-0404".to_string(),
                pdu_api_sw_name: "J2534".to_string(),
                pdu_api_sw_version: api_sw_version,
                pdu_api_sw_date: 0,
            }),
        }))
    }

    pub(super) async fn rpc_get_timestamp(
        &self,
        request: Request<vci_service_interface::GetTimestampRequest>,
    ) -> Result<Response<vci_service_interface::TimestampResponse>, Status> {
        let request = request.into_inner();
        Self::require_module_handle(request.module_handle, self.modules.len())?;

        Ok(Response::new(vci_service_interface::TimestampResponse {
            timestamp: events::module_timestamp_us(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Arc;

    use serial_test::serial;
    use tokio::sync::Mutex;

    use super::*;

    /// Builds a minimal `J2534Service` backed by the mock J2534 cdylib, with
    /// no logical links registered -- for exercising `rpc_module_connect`
    /// directly. Mirrors `rpc_link.rs::tests::service_with_auto_can_mode_and_no_links`'s
    /// construction shape.
    async fn minimal_service() -> J2534Service {
        minimal_service_with_modules(vec![crate::config::ModuleEntry {
            label: "j2534-0404".to_string(),
            pname: None,
        }])
        .await
    }

    /// Same as [`minimal_service`], but with a caller-supplied `modules`
    /// list -- for exercising the multi-module `GetModuleIds` behavior
    /// (ADR-107/ADR-132) without going through the `grpc_mock` harness.
    async fn minimal_service_with_modules(
        modules: Vec<crate::config::ModuleEntry>,
    ) -> J2534Service {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::SingleChannel,
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(modules),
            device_id: Arc::new(Mutex::new(None)),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            logical_links: Arc::new(Mutex::new(HashMap::new())),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(HashMap::new())),
            primitives: Arc::new(Mutex::new(HashMap::new())),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(0)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            shutdown: shutdown_rx,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// ADR-131 (Codex review, PR #143, P1, second round on this mechanism):
    /// a repeat `ModuleConnect` for a handle a hard error already marked
    /// `PduModstNotAvail` must NOT silently report success and clear the
    /// flag -- `ensure_open_device_for`'s already-open branch never
    /// re-validates the device (no native call at all), so doing so would
    /// hand the client a false "READY" for a VCI that may still be
    /// unreachable. It must instead reject with `PDU_ERR_FCT_FAILED`,
    /// preserving `NotAvail`/`last_error` for the client to inspect and
    /// prompting the documented `ModuleDisconnect`-then-`ModuleConnect`
    /// recovery sequence (ISO 22900-2 §9.4.29.2 (a)/NOTE 2, §9.4.30).
    #[tokio::test]
    async fn module_connect_rejects_a_device_a_hard_error_marked_not_avail() {
        let service = minimal_service().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));
        service.module_state.lock().await.status =
            vci_service_interface::PduModuleStatus::PduModstNotAvail;
        service.module_state.lock().await.last_error = TrackedError {
            event: vci_service_interface::PduErrorEvent::PduErrEvtLostCommToVci,
            timestamp: 123,
            cop: None,
            cop_tag: None,
        };

        let status = service
            .rpc_module_connect(Request::new(vci_service_interface::ModuleConnectRequest {
                module_handle: Some(vci_service_interface::ModuleHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                }),
            }))
            .await
            .expect_err(
                "ModuleConnect on a device a hard error marked NotAvail must reject, not report \
                 a false success",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrFctFailed as i32);

        assert_eq!(
            service.module_state.lock().await.status,
            vci_service_interface::PduModuleStatus::PduModstNotAvail,
            "a rejected ModuleConnect must leave module_state.status untouched -- NotAvail is \
             sticky until an explicit ModuleDisconnect"
        );
        assert_eq!(
            service.module_state.lock().await.last_error.event,
            vci_service_interface::PduErrorEvent::PduErrEvtLostCommToVci,
            "last_error must survive the rejected ModuleConnect call for later retrieval \
             (ADR-105's error_event_data reporting), not be silently wiped"
        );

        let status = service
            .require_connected_device_for(DEFAULT_MODULE_HANDLE)
            .await
            .expect_err("a module-scoped IOCTL must still reject after the failed ModuleConnect");
        assert_eq!(status.code(), Code::FailedPrecondition);
    }

    /// ADR-131: the actual, spec-prescribed recovery sequence --
    /// `ModuleDisconnect` then `ModuleConnect` -- does clear `NotAvail` and
    /// restore module-scoped IOCTL access, since the second `ModuleConnect`
    /// takes `ensure_open_device_inner`'s fresh-`PassThruOpen` branch (which
    /// resets `module_state` to `ModuleState::default()`), not the
    /// already-open no-op branch the test above pins as rejecting.
    #[tokio::test]
    async fn module_disconnect_then_module_connect_recovers_a_device_marked_not_avail() {
        let service = minimal_service().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));
        service.module_state.lock().await.status =
            vci_service_interface::PduModuleStatus::PduModstNotAvail;

        service
            .rpc_module_disconnect(Request::new(
                vci_service_interface::ModuleDisconnectRequest {
                    module_handle: Some(vci_service_interface::ModuleHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                    }),
                },
            ))
            .await
            .expect("ModuleDisconnect should succeed even while the module was NotAvail");

        service
            .rpc_module_connect(Request::new(vci_service_interface::ModuleConnectRequest {
                module_handle: Some(vci_service_interface::ModuleHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                }),
            }))
            .await
            .expect("ModuleConnect after ModuleDisconnect should succeed via a fresh open");

        assert_eq!(
            service.module_state.lock().await.status,
            vci_service_interface::PduModuleStatus::PduModstReady,
            "a fresh PassThruOpen (ensure_open_device_inner) resets module_state to Ready"
        );
        let _ = service
            .require_connected_device_for(DEFAULT_MODULE_HANDLE)
            .await
            .expect(
                "a module-scoped IOCTL should succeed again after the Disconnect-then-Connect \
                 recovery sequence",
            );
    }

    /// A2-25 / ADR-132: with nothing open, every configured row must report
    /// `PDU_MODST_AVAIL` -- the spec's detected-but-not-connected state --
    /// not the previously hardcoded `PDU_MODST_READY`.
    #[tokio::test]
    async fn get_module_ids_reports_avail_for_every_row_when_nothing_is_open() {
        let service = minimal_service_with_modules(vec![
            crate::config::ModuleEntry {
                label: "Bench 1".to_string(),
                pname: None,
            },
            crate::config::ModuleEntry {
                label: "Bench 2".to_string(),
                pname: None,
            },
        ])
        .await;

        let rows = service
            .rpc_get_module_ids(Request::new(vci_service_interface::GetModuleIdsRequest {}))
            .await
            .expect("get_module_ids should succeed")
            .into_inner()
            .module_id_list
            .expect("module_id_list should be present")
            .module_data;

        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(
                row.module_status,
                vci_service_interface::PduModuleStatus::PduModstAvail as i32,
                "an unopened row must report PDU_MODST_AVAIL, not READY: {row:?}"
            );
        }
    }

    /// A2-25 / ADR-132: the row matching the currently-open handle must
    /// mirror the real tracked `module_state.status` -- the same value
    /// `GetStatus`'s `ModuleHandle` branch returns -- while every other
    /// (unopened) row stays `PDU_MODST_AVAIL`.
    #[tokio::test]
    async fn get_module_ids_reports_tracked_status_for_the_open_handle_only() {
        let service = minimal_service_with_modules(vec![
            crate::config::ModuleEntry {
                label: "Bench 1".to_string(),
                pname: None,
            },
            crate::config::ModuleEntry {
                label: "Bench 2".to_string(),
                pname: None,
            },
        ])
        .await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((2, device_id));
        service.module_state.lock().await.status =
            vci_service_interface::PduModuleStatus::PduModstNotAvail;

        let rows = service
            .rpc_get_module_ids(Request::new(vci_service_interface::GetModuleIdsRequest {}))
            .await
            .expect("get_module_ids should succeed")
            .into_inner()
            .module_id_list
            .expect("module_id_list should be present")
            .module_data;

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].module_status,
            vci_service_interface::PduModuleStatus::PduModstAvail as i32,
            "handle 1 is not open, so it must stay PDU_MODST_AVAIL"
        );
        assert_eq!(
            rows[1].module_status,
            vci_service_interface::PduModuleStatus::PduModstNotAvail as i32,
            "handle 2 is open and tracked NotAvail -- GetModuleIds must reflect that, not READY"
        );
    }

    /// A2-25 / ADR-132: boundary check on the other side from the test
    /// above -- handle 1 (the FIRST configured row, index 0) open, not
    /// handle 2 (the LAST), still correctly maps `module_handle == index +
    /// 1` for both rows.
    #[tokio::test]
    async fn get_module_ids_reports_tracked_status_for_module_1_when_open() {
        let service = minimal_service_with_modules(vec![
            crate::config::ModuleEntry {
                label: "Bench 1".to_string(),
                pname: None,
            },
            crate::config::ModuleEntry {
                label: "Bench 2".to_string(),
                pname: None,
            },
        ])
        .await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((1, device_id));
        service.module_state.lock().await.status =
            vci_service_interface::PduModuleStatus::PduModstNotAvail;

        let rows = service
            .rpc_get_module_ids(Request::new(vci_service_interface::GetModuleIdsRequest {}))
            .await
            .expect("get_module_ids should succeed")
            .into_inner()
            .module_id_list
            .expect("module_id_list should be present")
            .module_data;

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].module_status,
            vci_service_interface::PduModuleStatus::PduModstNotAvail as i32,
            "handle 1 is open and tracked NotAvail -- GetModuleIds must reflect that, not READY"
        );
        assert_eq!(
            rows[1].module_status,
            vci_service_interface::PduModuleStatus::PduModstAvail as i32,
            "handle 2 is not open, so it must stay PDU_MODST_AVAIL"
        );
    }

    /// A2-25 / ADR-132: the pre-existing single/default-module path (no
    /// `modules` configured, the synthetic ADR-107 default entry) must also
    /// report the real tracked status once its one device is open, not
    /// hardcoded `READY`.
    #[tokio::test]
    async fn get_module_ids_reports_tracked_status_for_the_default_single_module() {
        let service = minimal_service().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));
        service.module_state.lock().await.status =
            vci_service_interface::PduModuleStatus::PduModstNotAvail;

        let rows = service
            .rpc_get_module_ids(Request::new(vci_service_interface::GetModuleIdsRequest {}))
            .await
            .expect("get_module_ids should succeed")
            .into_inner()
            .module_id_list
            .expect("module_id_list should be present")
            .module_data;

        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].module_status,
            vci_service_interface::PduModuleStatus::PduModstNotAvail as i32,
            "the default single module's row must mirror module_state.status, not hardcode READY"
        );
    }

    /// ADR-134 (closing the residual ADR-131 left open): `GetVersion` used to
    /// call `ensure_open_device_for` directly without checking
    /// `module_state.status`, so a device left open by a hard error that
    /// marked the module `PduModstNotAvail` would still lazily "succeed"
    /// through the already-open no-op branch, returning stale version info.
    /// ISO 22900-2 §9.4.29.2 NOTE 1 lists exactly four functions callable
    /// before `ModuleConnect` and `GetVersion` is not one of them; its own
    /// Table 13 lists `PDU_ERR_MODULE_NOT_CONNECTED` as a legal return.
    #[tokio::test]
    async fn get_version_rejects_a_device_a_hard_error_marked_not_avail() {
        let service = minimal_service().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));
        service.module_state.lock().await.status =
            vci_service_interface::PduModuleStatus::PduModstNotAvail;

        let status = service
            .rpc_get_version(Request::new(vci_service_interface::GetVersionRequest {
                module_handle: Some(vci_service_interface::ModuleHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                }),
            }))
            .await
            .expect_err(
                "GetVersion on a device a hard error marked NotAvail must reject, not report \
                 stale version info",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrModuleNotConnected as i32);

        assert_eq!(
            service.module_state.lock().await.status,
            vci_service_interface::PduModuleStatus::PduModstNotAvail,
            "a rejected GetVersion must leave module_state.status untouched"
        );
    }

    // ADR-193 amendment (round 17, Codex review, P1, PR #101):
    // `rpc_module_disconnect`'s new `tp20_broadcast_periodic` fence + take.
    // Sibling coverage to `events_hard_error_broadcast_periodic_tests.rs`'s
    // round-16 fence tests for `handle_channel_hard_error`, but exercised
    // through the real gRPC entry point directly (`rpc_module_disconnect`
    // itself is reachable via this module's own `minimal_service()`
    // construction -- no hand-built `ChannelPollCtx` needed the way that
    // sibling file's own doc comment explains is required for the
    // `handle_channel_hard_error` case).
    const RMD_TEST_CLL: u32 = 1;
    const RMD_COP_HANDLE: u32 = 501;
    const RMD_CHANNEL_ID: ChannelId = ChannelId(7);
    const RMD_CHANNEL_KEY: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);
    /// Round 18 (Codex review, P1, PR #101, ADR-193 amendment): an arbitrary
    /// `repeat_message_ids` entry for the symmetric-repeat-fix coverage
    /// below -- never actually started via a real `IOCTL_START_REPEAT_
    /// MESSAGE` call, so the mock's own `PassThruIoctl(IOCTL_STOP_REPEAT_
    /// MESSAGE)` handler always fails against it (either `ERR_INVALID_
    /// CHANNEL_ID`, since `RMD_CHANNEL_ID` itself is never registered with
    /// the mock via a real `PassThruConnect`, or `ERR_INVALID_MSG_ID` if it
    /// ever were) -- exactly the failure-tolerance path that coverage needs.
    const RMD_REPEAT_MSG_ID: u32 = 42;

    /// A `LogicalLinkState` connected on `RMD_CHANNEL_ID`/`RMD_CHANNEL_KEY`
    /// carrying a `tp20_broadcast_periodic` entry -- `message_id` is the
    /// caller-supplied sentinel/committed distinction. Mirrors
    /// `events_hard_error_broadcast_periodic_tests.rs::link_with_a_live_
    /// broadcast_periodic`'s own construction shape (same struct, same
    /// field values throughout, only `tp20_broadcast_periodic.message_id`
    /// and (round 18) `repeat_message_ids` vary here).
    fn link_with_tp20_broadcast_periodic(
        message_id: Option<j2534_0404::PeriodicMessageId>,
        repeat_message_ids: Vec<u32>,
    ) -> LogicalLinkState {
        LogicalLinkState {
            channel_id: Some(RMD_CHANNEL_ID),
            protocol: ChannelProtocol::CAN,
            hw_protocol_id: j2534_0404::CAN,
            software_isotp: false,
            uudt_channel_id: None,
            uudt_channel_key: None,
            isotp_rx: Arc::new(Mutex::new(HashMap::new())),
            connect_in_flight: std::sync::Weak::new(),
            connected: true,
            comm_started: false,
            raw_mode: false,
            checksum_mode: false,
            connect_generation: 1,
            stop_comm_pending: false,
            channel_key: Some(RMD_CHANNEL_KEY),
            pin_select: None,
            channel_index: None,
            base_hw_protocol_override: None,
            rx_buf: Arc::new(Mutex::new(CllEventQueue {
                event_queue_cap: 16,
                ..CllEventQueue::default()
            })),
            working: ComParamSet::default(),
            active: ComParamSet::default(),
            tester_present_state: TesterPresentState::None,
            tester_present_base_tx_flags: 0,
            open_tp_discards: Vec::new(),
            working_unique_resp_id_table: Vec::new(),
            active_unique_resp_id_table: Vec::new(),
            unique_resp_filter_ids: Vec::new(),
            cancelled_cops: std::collections::HashSet::new(),
            held_lock_mask: 0,
            last_error: None,
            tx_held: VecDeque::new(),
            tx_suspended_by_ioctl: false,
            tx_suspended_by_lock: false,
            tx_suspended_by_error: false,
            error_clear_seq: 0,
            error_set_seq: 0,
            client_filters: HashMap::new(),
            repeat_message_ids,
            pending_client_filters: HashMap::new(),
            registrants: Vec::new(),
            next_registrant_seq: 0,
            j1939_claimed_address: None,
            j1939_claim_cursor: 0,
            j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
            tp20_connection: None,
            tp20_broadcast_periodic: Some(Tp20BroadcastPeriodic {
                cop_handle: RMD_COP_HANDLE,
                message_id,
                started_epoch: 0,
                pending_clear_generation: 0,
            }),
        }
    }

    /// Builds a `minimal_service()` with a device open, one CLL carrying
    /// `link_with_tp20_broadcast_periodic(message_id, repeat_message_ids)`, a
    /// matching `SharedChannel` entry (`dead` per the caller-supplied flag,
    /// round 18), and one dispatched `CopEntry` for `RMD_COP_HANDLE` owned by
    /// that CLL -- everything `rpc_module_disconnect` touches for this
    /// scenario.
    async fn service_with_a_broadcast_periodic_cll(
        message_id: Option<j2534_0404::PeriodicMessageId>,
    ) -> J2534Service {
        service_with_a_broadcast_periodic_cll_ext(message_id, Vec::new(), false).await
    }

    /// Full form of [`service_with_a_broadcast_periodic_cll`] (round 18):
    /// also seeds `repeat_message_ids` on the CLL and lets the caller mark
    /// the `SharedChannel` entry `dead`, for the symmetric-repeat-fix and
    /// dead-channel-skip coverage below.
    async fn service_with_a_broadcast_periodic_cll_ext(
        message_id: Option<j2534_0404::PeriodicMessageId>,
        repeat_message_ids: Vec<u32>,
        dead: bool,
    ) -> J2534Service {
        let service = minimal_service().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));

        service.logical_links.lock().await.insert(
            RMD_TEST_CLL,
            link_with_tp20_broadcast_periodic(message_id, repeat_message_ids),
        );

        service.primitives.lock().await.insert(
            RMD_COP_HANDLE,
            CopEntry {
                cll_handle: RMD_TEST_CLL,
                dispatched: true,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );

        service.shared_channels.lock().await.insert(
            RMD_CHANNEL_KEY,
            SharedChannel {
                channel_id: RMD_CHANNEL_ID,
                ref_count: 1,
                tx_queue: tokio::sync::mpsc::unbounded_channel().0,
                executing_cop: Arc::new(Mutex::new(None)),
                _poll_cancel: tokio::sync::oneshot::channel().0,
                connect_flags: 0,
                dead,
                occupancy_epoch: 0,
                leaked_repeat_message_ids: Vec::new(),
                leaked_periodic_message_ids: Vec::new(),
                applied_analog_sample_rate: None,
                applied_analog_samples_per_reading: None,
                applied_analog_readings_per_msg: None,
                j1939_claims: HashMap::new(),
                j1939_claim_results: HashMap::new(),
                j1939_reclaim_pending: HashMap::new(),
                leaked_j1939_claims: Vec::new(),
                tp20_connections: HashMap::new(),
                tp20_connection_results: HashMap::new(),
                become_master_in_flight: Arc::new(portable_atomic::AtomicBool::new(false)),
                tp20_passive: None,
            },
        );

        service
    }

    /// ADR-193 amendment (round 17, Codex review, P1, PR #101),
    /// fence-acquisition isolation: while this test task holds `service.api`
    /// (standing in for an in-flight start bracket that has not yet reached
    /// its own revalidation), a concurrently spawned `rpc_module_disconnect`
    /// call that took a `None`-sentinel reservation off the swept CLL must
    /// make no observable progress reporting that CLL's COP terminal.
    /// Mirrors `events_hard_error_broadcast_periodic_tests.rs::
    /// hard_channel_error_waits_for_the_fence_before_reporting_the_swept_
    /// cop_terminal`'s own technique -- deterministic on this crate's
    /// `current_thread` test runtime, since every other await on
    /// `rpc_module_disconnect`'s path up to the fence is an uncontended
    /// `Mutex::lock`.
    ///
    /// Unlike that sibling test, this function cannot use a final
    /// `terminal_cops.lookup` as proof the swept COP was reported: this is a
    /// full force-cleanup, so `purge_many` (called later in the same
    /// `rpc_module_disconnect` invocation, once every CLL is torn down)
    /// removes the very entry `cancel_link_cops` just recorded before this
    /// function ever gets to observe it. `primitives` membership is used
    /// instead -- `cancel_link_cops` removes the cop from `primitives`
    /// strictly before it reports the terminal status, so "no longer in
    /// `primitives`" is an equally direct proxy for "the fence has been
    /// released and the report ran", without depending on ledger state that
    /// does not survive this call.
    #[tokio::test]
    async fn module_disconnect_waits_for_the_fence_before_reporting_an_in_flight_starts_cop_terminal()
     {
        let service = service_with_a_broadcast_periodic_cll(None).await;
        let api_guard = service.api.lock().await;

        let cloned = service.clone();
        let disconnect = tokio::spawn(async move {
            cloned
                .rpc_module_disconnect(Request::new(
                    vci_service_interface::ModuleDisconnectRequest {
                        module_handle: Some(vci_service_interface::ModuleHandle {
                            module_handle: DEFAULT_MODULE_HANDLE,
                        }),
                    },
                ))
                .await
        });

        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !disconnect.is_finished(),
            "rpc_module_disconnect must park on self.api before reporting the swept CLL's COP \
             terminal, once it has taken a None-sentinel in-flight-start reservation"
        );
        assert!(
            service
                .logical_links
                .lock()
                .await
                .get(&RMD_TEST_CLL)
                .expect("the CLL entry is not removed until later in this same call")
                .tp20_broadcast_periodic
                .is_none(),
            "the None-sentinel take needs no fence and must be visible immediately"
        );
        assert!(
            service
                .primitives
                .lock()
                .await
                .contains_key(&RMD_COP_HANDLE),
            "cancel_link_cops must not have run yet while the fence is held elsewhere"
        );

        drop(api_guard);
        disconnect
            .await
            .expect("rpc_module_disconnect must not panic")
            .expect("rpc_module_disconnect should succeed once the fence releases");

        assert!(
            !service
                .primitives
                .lock()
                .await
                .contains_key(&RMD_COP_HANDLE),
            "once the fence is released rpc_module_disconnect reports the swept COP terminal \
             (cancel_link_cops removes it from primitives) as before"
        );
    }

    /// Round 18 (Codex review, P1, PR #101, ADR-193 amendment): local
    /// dlopen-based mock backdoor helpers for this module's own test suite --
    /// same "fresh `Library::new` on the identical path resolves to the SAME
    /// dynamically-loaded shared object `service.api` itself mutates, not a
    /// separate statically-linked copy of `MockState`" rationale
    /// `rpc_primitive.rs::finalize_or_orphan_broadcast_periodic_start_tests::
    /// stop_periodic_call_count` and
    /// `rpc_misc.rs::terminate_tp20_broadcast_periodic_for_suspension_tests::
    /// set_stop_periodic_message_error` already document for their own,
    /// separately-scoped copies of the identical two helpers.
    fn set_stop_periodic_message_error(code: Option<std::os::raw::c_long>) {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        unsafe {
            let lib = j2534_0404_sys::libloading::Library::new(&lib_path)
                .expect("mock library should be loadable");
            let f: j2534_0404_sys::libloading::Symbol<
                unsafe extern "system" fn(std::os::raw::c_long) -> std::os::raw::c_long,
            > = lib
                .get(b"__mock_set_stop_periodic_message_error\0")
                .expect("__mock_set_stop_periodic_message_error should be exported");
            f(code.unwrap_or(0));
        }
    }

    /// Count of the `PassThruStopPeriodicMsg` calls made so far on this
    /// thread, on any channel -- incremented unconditionally by the mock before
    /// it even checks whether `channel_id` is a channel it actually knows
    /// about (`j2534-0404-mock/src/lib.rs`'s own `PassThruStopPeriodicMsg`),
    /// so it is a faithful "was this native call actually issued" signal
    /// even against this file's fabricated `RMD_CHANNEL_ID` (never registered
    /// with the mock via a real `PassThruConnect`). Per-thread rather than
    /// the mock's process-wide count, so other tests running in parallel
    /// cannot move it (see `discovery.rs::tests::get_device_info_call_count`).
    /// The tests that read it stay in the `#[serial(tp20_stop_periodic_call_counter)]`
    /// group because the tests that arm the process-global
    /// `__mock_set_stop_periodic_message_error` injection are in it too, so
    /// none of them sees another's injected error. (Untagged callers of
    /// `PassThruStopPeriodicMsg` elsewhere in the crate are not covered.)
    fn stop_periodic_call_count() -> usize {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        unsafe {
            let lib = j2534_0404_sys::libloading::Library::new(&lib_path)
                .expect("mock library should be loadable");
            let f: j2534_0404_sys::libloading::Symbol<unsafe extern "system" fn() -> usize> = lib
                .get(b"__mock_get_stop_periodic_count_on_current_thread\0")
                .expect("__mock_get_stop_periodic_count_on_current_thread should be exported");
            f()
        }
    }

    /// Round 18 (Codex review, P1, PR #101, ADR-193 amendment): supersedes
    /// this test's own pre-round-18 name/doc
    /// (`module_disconnect_silently_drops_a_committed_broadcast_periodic_with_no_leak_track_and_no_panic`)
    /// -- a committed broadcast periodic is no longer silently dropped, it
    /// now gets an explicit best-effort `PassThruStopPeriodicMsg` attempt of
    /// its own (see `rpc_module_disconnect`'s own doc comment at the
    /// collection site for the full rationale/precedent this supersedes).
    /// Proves that attempt actually fires -- `stop_periodic_call_count()`
    /// increments by at least one -- while keeping every pre-existing
    /// assertion from the superseded test unchanged: the call still
    /// succeeds without panicking, and every map this teardown touches still
    /// ends up empty exactly as an ordinary `ModuleDisconnect` would leave
    /// them. Still does NOT leak-track a failed/skipped stop into
    /// `SharedChannel::leaked_periodic_message_ids` -- design-advisor
    /// confirmed (see the collection site's own doc comment) there is
    /// nowhere left to leak-track this into that any retry mechanism could
    /// ever reach, once this same call's own `slot.take()` unconditionally
    /// clears `self.device_id` (ADR-107 addendum (h)).
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn module_disconnect_issues_a_best_effort_periodic_stop_for_a_committed_broadcast_before_teardown()
     {
        let service =
            service_with_a_broadcast_periodic_cll(Some(j2534_0404::PeriodicMessageId(777))).await;
        let before = stop_periodic_call_count();

        service
            .rpc_module_disconnect(Request::new(
                vci_service_interface::ModuleDisconnectRequest {
                    module_handle: Some(vci_service_interface::ModuleHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                    }),
                },
            ))
            .await
            .expect(
                "rpc_module_disconnect should succeed even with a committed broadcast periodic \
                 live on the torn-down CLL",
            );

        assert_eq!(
            stop_periodic_call_count(),
            before + 1,
            "a committed broadcast periodic on a still-live (non-dead) SharedChannel must get \
             exactly one explicit best-effort PassThruStopPeriodicMsg attempt of its own"
        );
        assert!(
            service.logical_links.lock().await.is_empty(),
            "ModuleDisconnect must clear every logical link, including the one that owned the \
             committed broadcast periodic"
        );
        assert!(
            service.shared_channels.lock().await.is_empty(),
            "ModuleDisconnect must drain shared_channels entirely regardless of the explicit \
             best-effort stop attempt"
        );
        assert!(
            !service
                .primitives
                .lock()
                .await
                .contains_key(&RMD_COP_HANDLE),
            "the owning CLL's COP must still be reported terminal via cancel_link_cops, \
             unaffected by the broadcast-periodic best-effort stop"
        );
    }

    /// Item (b) of the round-18 brief: a failed explicit best-effort stop
    /// must never block or fail the overall `ModuleDisconnect` RPC -- it is
    /// explicitly best-effort, matching how
    /// `DisconnectComLogicalLink`/`DestroyComLogicalLink`/
    /// `handle_channel_hard_error` already treat a failed stop with nowhere
    /// further to escalate to. Forces every subsequent
    /// `PassThruStopPeriodicMsg` to fail via
    /// `__mock_set_stop_periodic_message_error` (the fake, mock-unregistered
    /// `RMD_CHANNEL_ID` this file's fixtures use would otherwise never
    /// itself produce a failure here -- unlike
    /// `PassThruIoctl(IOCTL_STOP_REPEAT_MESSAGE)`, `PassThruStopPeriodicMsg`
    /// succeeds unconditionally against an unknown `channel_id` in this
    /// mock).
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn module_disconnect_succeeds_even_when_the_best_effort_periodic_stop_fails() {
        let service =
            service_with_a_broadcast_periodic_cll(Some(j2534_0404::PeriodicMessageId(777))).await;
        set_stop_periodic_message_error(Some(j2534_0404::ERR_FAILED as std::os::raw::c_long));

        let result = service
            .rpc_module_disconnect(Request::new(
                vci_service_interface::ModuleDisconnectRequest {
                    module_handle: Some(vci_service_interface::ModuleHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                    }),
                },
            ))
            .await;

        set_stop_periodic_message_error(None);

        result.expect(
            "a failed best-effort PassThruStopPeriodicMsg must never block or fail the overall \
             ModuleDisconnect RPC",
        );
        assert!(
            service.logical_links.lock().await.is_empty(),
            "teardown must still complete fully despite the forced stop failure"
        );
        assert!(
            service.shared_channels.lock().await.is_empty(),
            "teardown must still complete fully despite the forced stop failure"
        );
    }

    /// A minimal `tracing::Subscriber` that records whether any WARN-level
    /// event with a target containing `module_path_needle` was observed.
    /// Observes the warning itself rather than a mock call count. Setting
    /// this subscriber as the THREAD-LOCAL default (`tracing::subscriber::
    /// set_default`) around one `#[tokio::test]`'s single, un-spawned
    /// `current_thread`-runtime task observes only events emitted by code
    /// running on THIS test's own OS thread -- every other concurrently
    /// running test in the suite runs on its own separate OS thread with
    /// its own separate tracing dispatch, so other tests cannot interfere.
    struct WarnCapture {
        saw_matching_warn: std::sync::Arc<std::sync::atomic::AtomicBool>,
        module_path_needle: &'static str,
    }

    impl tracing::Subscriber for WarnCapture {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let metadata = event.metadata();
            if *metadata.level() == tracing::Level::WARN
                && metadata.target().contains(self.module_path_needle)
            {
                self.saw_matching_warn
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// Item (c) of the round-18 brief: an entry whose owning `SharedChannel`
    /// is already `dead` must never get an explicit stop attempt at all --
    /// mirrors `handle_channel_hard_error`'s own precedent of never
    /// attempting a native call against an already-dead channel (see the
    /// collection site's own doc comment in `rpc_module_disconnect`). Forces
    /// every `PassThruStopPeriodicMsg` to fail (`set_stop_periodic_message_
    /// error`) so that an incorrectly-NOT-skipped attempt would be loudly
    /// `warn!`-logged from `rpc_module.rs` itself; a `WarnCapture` subscriber
    /// set as this test's own thread-local default proves no such warning
    /// fired -- see that struct's own doc comment for why this, not the
    /// shared `stop_periodic_call_count()` counter other tests in this
    /// module use, is the robust way to prove a skip (a negative claim)
    /// specifically.
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn module_disconnect_skips_the_best_effort_stop_for_an_already_dead_channel() {
        let service = service_with_a_broadcast_periodic_cll_ext(
            Some(j2534_0404::PeriodicMessageId(777)),
            Vec::new(),
            true,
        )
        .await;
        set_stop_periodic_message_error(Some(j2534_0404::ERR_FAILED as std::os::raw::c_long));
        let saw_matching_warn = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let subscriber = WarnCapture {
            saw_matching_warn: saw_matching_warn.clone(),
            module_path_needle: "rpc_module",
        };

        let guard = tracing::subscriber::set_default(subscriber);
        let result = service
            .rpc_module_disconnect(Request::new(
                vci_service_interface::ModuleDisconnectRequest {
                    module_handle: Some(vci_service_interface::ModuleHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                    }),
                },
            ))
            .await;
        drop(guard);
        set_stop_periodic_message_error(None);

        result.expect("rpc_module_disconnect should succeed against an already-dead channel");
        assert!(
            !saw_matching_warn.load(std::sync::atomic::Ordering::SeqCst),
            "an already-dead SharedChannel must never get an explicit best-effort stop attempt \
             at all -- if it had, the forced PassThruStopPeriodicMsg failure would have produced \
             a warn! from rpc_module.rs"
        );
    }

    /// Item (d) of the round-18 brief: the symmetric `repeat_message_ids`
    /// fix. `repeat_message_ids` shares the exact same `live_channel_id`
    /// gate `rpc_module_disconnect` computes once per link (see the
    /// collection site's own doc comment) that
    /// `module_disconnect_skips_the_best_effort_stop_for_an_already_dead_channel`
    /// above already proves periodic honors on a `dead` channel -- the
    /// identical `Option` feeds both collections, so that same proof covers
    /// `repeat_message_ids` too. Unlike `PassThruStopPeriodicMsg`, this
    /// mock's `PassThruIoctl(IOCTL_STOP_REPEAT_MESSAGE)` checks `channel_id`
    /// presence BEFORE anything else (`j2534-0404-mock/src/lib.rs`), and
    /// exposes no dedicated call-count backdoor the way
    /// `__mock_get_stop_periodic_count` does -- so this test instead relies
    /// on this file's fixtures' fabricated `RMD_CHANNEL_ID`
    /// (never registered with the mock via a real `PassThruConnect`) to
    /// deterministically force that native call to fail with
    /// `ERR_INVALID_CHANNEL_ID`, and asserts the overall RPC still succeeds
    /// -- the identical best-effort/non-propagated-failure contract
    /// `module_disconnect_succeeds_even_when_the_best_effort_periodic_stop_fails`
    /// above proves for periodic.
    ///
    /// Corrected (edge-case-hunter adversarial review, PR #101, should-fix
    /// Finding 2): the original version of this test only asserted the RPC
    /// returned `Ok` and the maps drained -- both also true if the
    /// repeat-message stop loop were never attempted at all (proven by the
    /// edge-case-hunter: removing BOTH the periodic and repeat-message stop
    /// loops entirely still leaves this test passing, even though it
    /// correctly fails the sibling `module_disconnect_issues_a_best_effort_
    /// periodic_stop_for_a_committed_broadcast_before_teardown` test above).
    /// Now uses the same `WarnCapture` thread-local subscriber
    /// `module_disconnect_skips_the_best_effort_stop_for_an_already_dead_
    /// channel` above uses, but as a POSITIVE assertion: this file's only
    /// two `warn!` call sites are the periodic-stop-failure one (line 453)
    /// and the repeat-message-stop-failure one (line 464) -- this scenario
    /// commits no periodic id (`None`), so only the repeat-message failure
    /// warn can plausibly fire, meaning "a warn targeting `rpc_module`
    /// fired" unambiguously proves the repeat-message stop was actually
    /// attempted and failed, not silently skipped.
    #[tokio::test]
    async fn module_disconnect_succeeds_even_when_the_best_effort_repeat_message_stop_fails() {
        let service =
            service_with_a_broadcast_periodic_cll_ext(None, vec![RMD_REPEAT_MSG_ID], false).await;
        let saw_matching_warn = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let subscriber = WarnCapture {
            saw_matching_warn: saw_matching_warn.clone(),
            module_path_needle: "rpc_module",
        };

        let guard = tracing::subscriber::set_default(subscriber);
        let result = service
            .rpc_module_disconnect(Request::new(
                vci_service_interface::ModuleDisconnectRequest {
                    module_handle: Some(vci_service_interface::ModuleHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                    }),
                },
            ))
            .await;
        drop(guard);

        result.expect(
            "a failed best-effort PassThruIoctl(STOP_REPEAT_MESSAGE) must never block or \
             fail the overall ModuleDisconnect RPC",
        );
        assert!(
            saw_matching_warn.load(std::sync::atomic::Ordering::SeqCst),
            "the best-effort PassThruIoctl(STOP_REPEAT_MESSAGE) attempt must actually have been \
             made (and failed, producing this file's repeat-message-stop-failure warn!) -- a \
             passing RPC result alone does not discriminate an attempted-and-failed stop from a \
             silently skipped one"
        );
        assert!(
            service.logical_links.lock().await.is_empty(),
            "teardown must still complete fully despite the forced repeat-message stop failure"
        );
        assert!(
            service.shared_channels.lock().await.is_empty(),
            "teardown must still complete fully despite the forced repeat-message stop failure"
        );
    }
}
