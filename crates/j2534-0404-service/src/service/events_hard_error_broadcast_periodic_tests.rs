//! Fix B (design-advisor consult, Codex review round 3, ADR-192 Decision
//! item 2): `handle_channel_hard_error`'s new push of a live TP2.0 broadcast
//! periodic's `PeriodicMessageId` onto the dead `SharedChannel` entry's own
//! `leaked_periodic_message_ids`.
//!
//! **Why this is a direct unit test, not a `tests/grpc_mock` end-to-end
//! one:** a hard channel error unconditionally marks the whole module
//! `PduModstNotAvail` (`handle_channel_hard_error`'s own unconditional
//! `state.status = PduModstNotAvail` at the end of the function), and
//! ADR-131/ADR-134 make that sticky until an explicit `ModuleDisconnect` --
//! which `rpc_module_disconnect` implements by unconditionally
//! `links.clear()`-ing the ENTIRE `logical_links` map. There is no way
//! through this crate's gRPC surface to hard-error one physical channel and
//! then, from a FRESH CLL, exercise `LOCK_PHYSICAL_TX_QUEUE` against the
//! now-dead `SharedChannel` entry without an intervening `ModuleDisconnect`
//! that would tear the whole scenario down first -- the same class of
//! harness limit `tp20.rs`'s own `hard_error_then_reconnect_on_the_same_
//! channel_is_reported_as_module_not_avail` documents for the neighboring
//! `tp20_connection` reset (see that test's own doc comment). So, mirroring
//! that test's own resolution (`rpc_link.rs::tests::finalize_connected_link_
//! resets_a_stale_tp20_connection`, which calls the mechanism directly on
//! hand-built state), this test calls `handle_channel_hard_error` directly
//! against a hand-built `ChannelPollCtx` -- no device open or channel
//! connect needed. `handle_channel_hard_error` still never attempts a
//! native call through `ctx.api` (the channel is dead; no native stop is
//! attempted for the same reason the pre-existing `leaked_repeat_message_
//! ids` drain a few lines above it in the same function doesn't attempt
//! one either), but as of ADR-193's round-16 amendment it DOES
//! conditionally acquire (then immediately release) `ctx.api` once per
//! sweep -- only when the sweep took at least one `None`-sentinel
//! in-flight-start reservation off a CLL it just swept -- purely as a
//! serialization fence against a racing start, never to issue a native
//! call. See `hard_channel_error_takes_an_in_flight_start_reservation_
//! without_panicking` and `hard_channel_error_waits_for_the_fence_before_
//! reporting_the_swept_cop_terminal` below for direct coverage of that
//! fence.

use super::super::{SharedChannel, Tp20BroadcastPeriodic};
use super::*;

const HEBP_TEST_CLL: u32 = 1;
const HEBP_COP_HANDLE: u32 = 501;
const HEBP_CHANNEL_ID: ChannelId = ChannelId(7);
const HEBP_CHANNEL_KEY: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);

/// A `LogicalLinkState` connected on `HEBP_CHANNEL_ID`/`HEBP_CHANNEL_KEY`
/// (this is this CLL's PRIMARY channel, `was_primary` in `handle_channel_
/// hard_error`'s own terms) with a live broadcast periodic COP tracked.
/// Mirrors this file's sibling test modules' own `minimal_link()` shape.
fn link_with_a_live_broadcast_periodic() -> LogicalLinkState {
    LogicalLinkState {
        channel_id: Some(HEBP_CHANNEL_ID),
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
        channel_key: Some(HEBP_CHANNEL_KEY),
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
        repeat_message_ids: Vec::new(),
        pending_client_filters: HashMap::new(),
        registrants: Vec::new(),
        next_registrant_seq: 0,
        j1939_claimed_address: None,
        j1939_claim_cursor: 0,
        j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
        tp20_connection: None,
        tp20_broadcast_periodic: Some(Tp20BroadcastPeriodic {
            cop_handle: HEBP_COP_HANDLE,
            message_id: Some(j2534_0404::PeriodicMessageId(777)),
            started_epoch: 0,
            pending_clear_generation: 0,
        }),
    }
}

/// Builds a full `ChannelPollCtx` around one CLL (`link_with_a_live_
/// broadcast_periodic`) and a matching `SharedChannel` entry -- mirrors
/// `j1939_claim::tests::ctx_with_a_send_recv_cop`'s own construction shape.
/// `ctx.api` is a real, loaded mock handle (never opened/connected), mainly
/// to satisfy `J2534Service`'s field type -- for THIS ctx specifically
/// (an already-committed `Some(message_id)` periodic, no `None`-sentinel
/// anywhere), `handle_channel_hard_error` never touches it: as of ADR-193's
/// round-16 amendment the function only conditionally acquires (then
/// immediately releases) `ctx.api`, once per sweep, when the sweep took at
/// least one `None`-sentinel in-flight-start reservation -- never
/// unconditionally, and never as a native call. See
/// `ctx_with_an_in_flight_broadcast_periodic_reservation` below for the ctx
/// shape that DOES exercise that conditional acquisition.
async fn ctx_with_a_live_broadcast_periodic() -> ChannelPollCtx {
    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");

    let mut logical_links = HashMap::new();
    logical_links.insert(HEBP_TEST_CLL, link_with_a_live_broadcast_periodic());
    let logical_links = Arc::new(Mutex::new(logical_links));

    let mut primitives = HashMap::new();
    primitives.insert(
        HEBP_COP_HANDLE,
        CopEntry {
            cll_handle: HEBP_TEST_CLL,
            dispatched: true,
            transmits: true,
            is_send_recv: true,
            cop_tag: None,
        },
    );
    let primitives = Arc::new(Mutex::new(primitives));

    let mut shared_channels = HashMap::new();
    shared_channels.insert(
        HEBP_CHANNEL_KEY,
        SharedChannel {
            channel_id: HEBP_CHANNEL_ID,
            ref_count: 1,
            tx_queue: tokio::sync::mpsc::unbounded_channel().0,
            executing_cop: Arc::new(Mutex::new(None)),
            _poll_cancel: tokio::sync::oneshot::channel().0,
            connect_flags: 0,
            dead: false,
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
    let shared_channels = Arc::new(Mutex::new(shared_channels));

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    std::mem::forget(shutdown_tx);

    let service = J2534Service {
        api: Arc::new(Mutex::new(api)),
        startup_config: Arc::new(crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap()),
        can_channel_mode: CanChannelMode::default(),
        resolved_can_channel_mode: Arc::new(Mutex::new(None)),
        modules: Arc::new(vec![crate::config::ModuleEntry {
            label: "j2534-0404".to_string(),
            pname: None,
        }]),
        device_id: Arc::new(Mutex::new(None)),
        vendor_ioctls: Arc::new(HashMap::new()),
        device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
        periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
        logical_links: Arc::clone(&logical_links),
        drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
        shared_channels,
        primitives: Arc::clone(&primitives),
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
    };

    ChannelPollCtx {
        channel_id: HEBP_CHANNEL_ID,
        primitives,
        api: Arc::clone(&service.api),
        logical_links,
        drain_watermarks: Arc::clone(&service.drain_watermarks),
        subscriptions: Arc::clone(&service.subscriptions),
        module_state: Arc::clone(&service.module_state),
        module_event_buf: Arc::clone(&service.module_event_buf),
        system_event_buf: Arc::clone(&service.system_event_buf),
        executing_cop: Arc::new(Mutex::new(None)),
        last_func_tx: Arc::new(Mutex::new(None)),
        last_phys_tx: Arc::new(Mutex::new(None)),
        last_bus_activity: Arc::new(Mutex::new(tokio::time::Instant::now())),
        rx_supported: true,
        service,
    }
}

/// A `LogicalLinkState` connected on `HEBP_CHANNEL_ID`/`HEBP_CHANNEL_KEY`
/// carrying a `None`-sentinel in-flight-start reservation -- the state
/// `reserve_tp20_broadcast_periodic` leaves behind while
/// `rpc_start_com_primitive`'s native `PassThruStartPeriodicMsg` bracket is
/// still in flight -- instead of `link_with_a_live_broadcast_periodic`'s
/// already-committed `Some(message_id)` entry.
fn link_with_an_in_flight_start_reservation() -> LogicalLinkState {
    LogicalLinkState {
        tp20_broadcast_periodic: Some(Tp20BroadcastPeriodic {
            cop_handle: HEBP_COP_HANDLE,
            message_id: None,
            started_epoch: 0,
            pending_clear_generation: 0,
        }),
        ..link_with_a_live_broadcast_periodic()
    }
}

/// Same construction as `ctx_with_a_live_broadcast_periodic`, but the CLL
/// carries `link_with_an_in_flight_start_reservation`'s `None`-sentinel
/// entry instead of an already-committed one.
async fn ctx_with_an_in_flight_broadcast_periodic_reservation() -> ChannelPollCtx {
    let ctx = ctx_with_a_live_broadcast_periodic().await;
    let mut links = ctx.logical_links.lock().await;
    links.insert(HEBP_TEST_CLL, link_with_an_in_flight_start_reservation());
    drop(links);
    ctx
}

/// Clones every field of `ctx` -- every field is an `Arc` handle cheap to
/// clone, plus `J2534Service`'s own `#[derive(Clone)]` -- so the SAME
/// underlying handles (most importantly `ctx.api`, the ADR-193 fence) can be
/// shared into a `tokio::spawn`ed task. Mirrors `rpc_primitive.rs`'s own
/// `service.clone()` fence tests (`tp20_broadcast_periodic_api_fence_tests`);
/// `ChannelPollCtx` itself is not `Clone` (it is not needed anywhere else),
/// so this is a manual field-by-field clone rather than a derive.
fn clone_ctx(ctx: &ChannelPollCtx) -> ChannelPollCtx {
    ChannelPollCtx {
        channel_id: ctx.channel_id,
        primitives: Arc::clone(&ctx.primitives),
        api: Arc::clone(&ctx.api),
        logical_links: Arc::clone(&ctx.logical_links),
        drain_watermarks: Arc::clone(&ctx.drain_watermarks),
        subscriptions: Arc::clone(&ctx.subscriptions),
        module_state: Arc::clone(&ctx.module_state),
        module_event_buf: Arc::clone(&ctx.module_event_buf),
        system_event_buf: Arc::clone(&ctx.system_event_buf),
        executing_cop: Arc::clone(&ctx.executing_cop),
        last_func_tx: Arc::clone(&ctx.last_func_tx),
        last_phys_tx: Arc::clone(&ctx.last_phys_tx),
        last_bus_activity: Arc::clone(&ctx.last_bus_activity),
        rx_supported: ctx.rx_supported,
        service: ctx.service.clone(),
    }
}

/// ADR-193 amendment (Codex review round 16, P1, PR #101), fence-acquisition
/// isolation: while this test task holds `ctx.api` (standing in for an
/// in-flight start bracket that has not yet reached its own revalidation),
/// a concurrently spawned `handle_channel_hard_error` sweep that took a
/// `None`-sentinel reservation off the same CLL must make no observable
/// progress reporting that CLL's COPs terminal. Mirrors
/// `rpc_primitive.rs::tp20_broadcast_periodic_api_fence_tests::
/// cancel_waits_for_the_fence_before_taking_the_sentinel`'s own technique:
/// deterministic on this crate's `current_thread` test runtime, since every
/// other await on the sweep's path (aside from the fence) is an uncontended
/// `Mutex::lock`.
///
/// This test also subsumes what used to be a separate, narrower test
/// (`..._takes_an_in_flight_start_reservation_without_panicking`, deleted
/// as vacuous, `edge-case-hunter` finding: it passed even with the fence
/// acquisition itself stubbed out to a no-op, since "the entry is taken,
/// no leak-track push happens, and nothing panics" was already true of the
/// PRE-amendment code for a `None`-sentinel, and so did not actually
/// exercise anything new). The only NEW, externally observable behavior of
/// the `None` arm is that it sets `in_flight_start_taken`, which is only
/// observable via the fence acquisition it triggers -- so the panic-free
/// take-and-complete assertions are folded into THIS test instead, after
/// the fence releases, where they actually discriminate the fenced sweep
/// from a disabled one (verified by temporarily stubbing the fence
/// acquisition to a no-op and confirming this test's assertions fail).
#[tokio::test]
async fn hard_channel_error_waits_for_the_fence_before_reporting_the_swept_cop_terminal() {
    let ctx = ctx_with_an_in_flight_broadcast_periodic_reservation().await;
    let api = ctx.api.lock().await;

    let spawned_ctx = clone_ctx(&ctx);
    let sweep = tokio::spawn(async move {
        handle_channel_hard_error(HEBP_CHANNEL_ID, &spawned_ctx).await;
    });

    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !sweep.is_finished(),
        "the sweep must be parked on ctx.api before it may report the swept CLL's COPs terminal"
    );
    // The take itself needs no fence (ADR-193 Decision item 3's reasons
    // (a)/(b)/(c), extended to this call site by this amendment): it must
    // already be visible even while the sweep is parked on the fence below.
    assert!(
        ctx.logical_links
            .lock()
            .await
            .get(&HEBP_TEST_CLL)
            .unwrap()
            .tp20_broadcast_periodic
            .is_none(),
        "the None-sentinel take needs no fence and must be visible immediately"
    );
    assert!(
        ctx.service
            .terminal_cops
            .lock()
            .await
            .lookup(HEBP_COP_HANDLE)
            .is_none(),
        "no terminal status may be reported while the fence is held elsewhere"
    );

    drop(api);
    sweep.await.expect("the sweep must not panic");
    assert_eq!(
        ctx.service
            .terminal_cops
            .lock()
            .await
            .lookup(HEBP_COP_HANDLE),
        Some((HEBP_TEST_CLL, PduComPrimitiveStatus::PduCopstCancelled)),
        "once the fence is released the sweep reports the swept COP Cancelled as before"
    );

    // Folded in from the deleted `..._takes_an_in_flight_start_reservation_
    // without_panicking` test (see this test's own doc comment): the take
    // completed cleanly, with nothing left behind to leak-track and the CLL
    // taken offline, exactly as before the fence was added.
    let chans = ctx.service.shared_channels.lock().await;
    let sc = chans
        .get(&HEBP_CHANNEL_KEY)
        .expect("the SharedChannel entry must survive a hard error (ADR-134: not removed here)");
    assert!(
        sc.leaked_periodic_message_ids.is_empty(),
        "a None-sentinel reservation has no real PeriodicMessageId to leak-track"
    );
    drop(chans);

    let links = ctx.logical_links.lock().await;
    let link = links
        .get(&HEBP_TEST_CLL)
        .expect("the CLL entry itself is not removed by a hard error, only taken offline");
    assert!(
        link.tp20_broadcast_periodic.is_none(),
        "the None-sentinel entry must be taken off the link, recorded via \
         in_flight_start_taken, not left behind"
    );
    assert!(!link.connected, "a hard-errored CLL must be taken offline");
}

/// Core assertion: `handle_channel_hard_error` never attempts a native
/// `PassThruStopPeriodicMsg` (no `ctx.api` call at all for this), and
/// instead takes the live entry off the dying link and pushes its real
/// `PeriodicMessageId` onto the dead `SharedChannel`'s own `leaked_
/// periodic_message_ids` -- mirroring the pre-existing `leaked_repeat_
/// message_ids` drain in the same function exactly.
#[tokio::test]
async fn hard_channel_error_leaks_a_live_broadcast_periodic_onto_the_dead_shared_channel() {
    let ctx = ctx_with_a_live_broadcast_periodic().await;

    handle_channel_hard_error(HEBP_CHANNEL_ID, &ctx).await;

    let chans = ctx.service.shared_channels.lock().await;
    let sc = chans
        .get(&HEBP_CHANNEL_KEY)
        .expect("the SharedChannel entry must survive a hard error (ADR-134: not removed here)");
    assert!(
        sc.dead,
        "handle_channel_hard_error must mark the SharedChannel entry dead"
    );
    assert_eq!(
        sc.leaked_periodic_message_ids,
        vec![(j2534_0404::PeriodicMessageId(777), 0)],
        "the live broadcast periodic's real PeriodicMessageId, paired with its own \
         started_epoch, must be pushed onto the dead SharedChannel entry's own leak-tracking \
         list"
    );
    drop(chans);

    let links = ctx.logical_links.lock().await;
    let link = links
        .get(&HEBP_TEST_CLL)
        .expect("the CLL entry itself is not removed by a hard error, only taken offline");
    assert!(
        link.tp20_broadcast_periodic.is_none(),
        "the CLL's own tracking entry must be cleared -- it is now owned by the dead \
         SharedChannel entry instead"
    );
    assert!(!link.connected, "a hard-errored CLL must be taken offline");
}
