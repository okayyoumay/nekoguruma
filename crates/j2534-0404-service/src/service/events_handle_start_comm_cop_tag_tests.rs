//! ADR-205 Decision item 1 (Codex review, PR #116, fourth round): the
//! previously-merged `send_error_event`/`send_error_event_with_tag` split
//! left `send_error_event`'s ordinary fresh-lookup form as the default most
//! call sites used, on the strength of an unverified prose claim ("every
//! other call site calls it immediately... unaffected") -- falsified by this
//! round's finding that `handle_start_comm`'s `CoptStartcomm` failure path
//! (the `ParamBinding::Temp` apply-failure arm, `events.rs`) calls plain
//! `send_error_event` with no same-critical-section tag snapshot, even
//! though `handle_start_comm` itself already captures `cop_tag` once, at the
//! top of the function, for `send_cop_status`'s own EXECUTING emission. This
//! file provides a discriminating regression test driving `handle_start_comm`
//! directly (not through `tests/grpc_mock`, which has no hook to remove a
//! `primitives` entry independently of a full COP lifecycle -- same rationale
//! `events_reap_expired_cyclic_registrants_cop_tag_tests.rs`'s own module doc
//! comment gives for its analogous test).
//!
//! **Why this can be sequenced deterministically.** Between `handle_start_
//! comm`'s own initial `cop_tag` capture (its first `ctx.primitives.lock()
//! .await`) and the `send_error_event` call under test, the function never
//! reacquires `primitives` again on either the pre-fix or post-fix code path
//! -- so the reap test's own "hold `primitives`, let the callee queue behind
//! it, then race a removal in behind it" fence has no second acquisition to
//! anchor on here. A DIFFERENT lock in the same gap serves the same purpose:
//! `apply_params_to_hardware_capturing` (called for a `ParamBinding::Temp`
//! COP, immediately after the initial capture and Guard A) is the very next
//! `.await` point, and it acquires `ctx.api` -- a lock this test's own task
//! can hold BEFORE `handle_start_comm` is ever polled, forcing that
//! acquisition to queue deterministically (this crate's `current_thread` test
//! runtime resolves an uncontended `.lock().await` immediately, so nothing
//! upstream of `apply_params_to_hardware_capturing` -- `resolve_queue_target`,
//! the initial `primitives` capture, `send_cop_status`, Guard A's
//! `logical_links` read -- can itself block first). While `handle_start_comm`
//! is parked waiting for `ctx.api`, this test removes the `primitives` entry
//! directly (a separate, uncontended lock, no deadlock risk) -- exactly the
//! same "concurrent `CancelComPrimitive`/`cancel_link_cops`/hard-error path"
//! shape ADR-205's Context describes, just anchored on `ctx.api` instead of a
//! second `primitives` acquisition. Releasing `ctx.api` then lets `handle_
//! start_comm` proceed: `apply_params_to_hardware_capturing`'s own native
//! `PassThruIoctl SET_CONFIG` call fails outright (`ctx.channel_id` below is
//! never actually opened via `PassThruConnect` against the loaded mock
//! library, so every native call against it returns `ERR_INVALID_CHANNEL_ID`)
//! -- reaching this test's target `send_error_event` call by the same
//! `ParamBinding::Temp` apply-failure path Codex named, with the `primitives`
//! entry already gone by the time it runs.
//!
//! **Fail-without/pass-with, verified directly against this fix (2026-09-01,
//! same session):** temporarily reverted the target call site back to a
//! bare `Some(cop_handle)` with an inlined fresh `resolve_cop_tag(&ctx.
//! primitives, Some(cop_handle)).await` immediately before it (the exact
//! "lazy mechanical conversion" ADR-205's own Risks & checks section warns
//! against). Against the reverted code this test FAILS (`last_error.cop_tag`
//! observed as `None`, since the fresh lookup runs AFTER this test's own
//! concurrent removal). Restoring the fix, it PASSES.

use super::*;
use crate::service::ChannelProtocol;

const HSC_TEST_CHANNEL_ID: ChannelId = ChannelId(4343);
const HSC_TEST_CLL: u32 = 1;
const HSC_TEST_COP_HANDLE: u32 = 9101;
const HSC_TEST_CONNECT_GENERATION: u64 = 3;

/// A minimal `LogicalLinkState` connected on `HSC_TEST_CHANNEL_ID` at
/// `HSC_TEST_CONNECT_GENERATION` -- matches `events_reap_expired_cyclic_
/// registrants_cop_tag_tests.rs::link_with_the_expired_registrant`'s own
/// field-for-field default shape (this test needs no registrants).
fn minimal_started_link() -> LogicalLinkState {
    LogicalLinkState {
        channel_id: Some(HSC_TEST_CHANNEL_ID),
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
        connect_generation: HSC_TEST_CONNECT_GENERATION,
        stop_comm_pending: false,
        channel_key: None,
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
        tp20_broadcast_periodic: None,
    }
}

/// A `ResolvedTesterPresent` with tester-present handling disabled --
/// `handle_start_comm` never reads any of its other fields on the
/// `ParamBinding::Temp` apply-failure path this test drives (it returns
/// before reaching the tester-present arm), so only `handling_enabled`'s
/// value actually matters.
fn tester_present_disabled() -> rpc_primitive::ResolvedTesterPresent {
    rpc_primitive::ResolvedTesterPresent {
        data: Vec::new(),
        interval_ms: 0,
        tx_flags: 0,
        isotp_framing: None,
        send_type: 0,
        can_functional: None,
        addr_mode_functional: false,
        base_tx_flags: 0,
        target_can_ids: None,
        expects_response: false,
        p2_max_ms: 0,
        exp_pos_resp: Vec::new(),
        exp_neg_resp: Vec::new(),
        handling_enabled: false,
    }
}

/// See this file's own module doc comment for the full mechanism and its
/// fail-without/pass-with verification.
#[tokio::test]
async fn cop_tag_capture_survives_a_primitives_removal_racing_the_temp_apply_failure_path() {
    let tag = b"startcomm-race-token".to_vec();

    let logical_links = Arc::new(Mutex::new(HashMap::from([(
        HSC_TEST_CLL,
        minimal_started_link(),
    )])));
    let primitives = Arc::new(Mutex::new(HashMap::from([(
        HSC_TEST_COP_HANDLE,
        CopEntry {
            cll_handle: HSC_TEST_CLL,
            dispatched: true,
            transmits: false,
            is_send_recv: false,
            cop_tag: Some(tag.clone()),
        },
    )])));
    let drain_watermarks = Arc::new(Mutex::new(HashMap::new()));
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));

    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
    let api = Arc::new(Mutex::new(api));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    std::mem::forget(shutdown_tx);

    let service = J2534Service {
        api: Arc::clone(&api),
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
        drain_watermarks: Arc::clone(&drain_watermarks),
        shared_channels: Arc::new(Mutex::new(HashMap::new())),
        primitives: Arc::clone(&primitives),
        terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
        next_cll_handle: Arc::new(Mutex::new(0)),
        next_cop_handle: Arc::new(Mutex::new(0)),
        next_connect_generation: Arc::new(Mutex::new(0)),
        next_occupancy_epoch: Arc::new(Mutex::new(0)),
        subscriptions: Arc::clone(&subscriptions),
        shutdown: shutdown_rx,
        module_state: Arc::new(Mutex::new(ModuleState::default())),
        module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
        system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
        j1850_bus_flavor: Arc::new(Mutex::new(None)),
        prog_voltage: Arc::new(Mutex::new(HashMap::new())),
        discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
        discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
    };

    // A `ParamBinding::Temp` with one universally-mapped ComParam (`CP_
    // Loopback`, ADR-011/comparam_id.rs: `to_j2534_config_id` maps it on
    // every protocol) so `apply_params_to_hardware_locked`'s own `configs`
    // list is non-empty and a real `PassThruIoctl SET_CONFIG` call is
    // actually attempted -- against `HSC_TEST_CHANNEL_ID`, which this test
    // never opens via `PassThruConnect`, so the mock library rejects it with
    // `ERR_INVALID_CHANNEL_ID`, giving `applied = false` for free with no
    // dedicated mock error-injection backdoor needed.
    let mut effective = ComParamSet::default();
    effective.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 1);

    let params = StartCommParams {
        protocol_id: j2534_0404::CAN,
        base_protocol_id: j2534_0404::CAN,
        tester_present: tester_present_disabled(),
        init_tx_flags: 0,
        tx: None,
        five_baud: None,
        fast_init: None,
        binding: ParamBinding::Temp { effective },
        connect_generation: HSC_TEST_CONNECT_GENERATION,
    };

    // Fence: hold `ctx.api` before `handle_start_comm` is ever polled --
    // this is the first lock it contends on after its own initial `cop_tag`
    // capture (which uses `primitives`, never blocked by this guard).
    let api_guard = api.lock().await;

    let task = {
        let primitives = Arc::clone(&primitives);
        let logical_links = Arc::clone(&logical_links);
        let drain_watermarks = Arc::clone(&drain_watermarks);
        let subscriptions = Arc::clone(&subscriptions);
        let service = service.clone();
        tokio::spawn(async move {
            let ctx = ChannelPollCtx {
                channel_id: HSC_TEST_CHANNEL_ID,
                primitives,
                api: Arc::clone(&service.api),
                logical_links,
                drain_watermarks,
                subscriptions,
                module_state: Arc::clone(&service.module_state),
                module_event_buf: Arc::clone(&service.module_event_buf),
                system_event_buf: Arc::clone(&service.system_event_buf),
                executing_cop: Arc::new(Mutex::new(None)),
                last_func_tx: Arc::new(Mutex::new(None)),
                last_phys_tx: Arc::new(Mutex::new(None)),
                last_bus_activity: Arc::new(Mutex::new(tokio::time::Instant::now())),
                rx_supported: true,
                service,
            };
            handle_start_comm(HSC_TEST_COP_HANDLE, HSC_TEST_CLL, params, &ctx).await;
        })
    };

    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !task.is_finished(),
        "handle_start_comm must be parked waiting for ctx.api (inside \
         apply_params_to_hardware_capturing) before this test's own fence is released"
    );

    // Simulate a concurrent `CancelComPrimitive`/`cancel_link_cops`/hard-error
    // path removing this COP's `primitives` entry in the exact gap this fix
    // closes -- `primitives` is a separate lock from `ctx.api`, so this
    // cannot deadlock against the parked task above.
    primitives.lock().await.remove(&HSC_TEST_COP_HANDLE);

    drop(api_guard);
    task.await.expect("handle_start_comm must not panic");

    let links = logical_links.lock().await;
    let last_error = links
        .get(&HSC_TEST_CLL)
        .unwrap()
        .last_error
        .as_ref()
        .expect("PDU_ERR_EVT_PROT_ERR should have been recorded");
    assert_eq!(
        last_error.cop_tag,
        Some(tag),
        "the tag captured at the top of handle_start_comm must survive a concurrent \
         primitives removal landing before the later send_error_event call"
    );
}

/// A sixth instance of the same bug class (orchestrator-fixed directly,
/// 2026-09-01, `events.rs` ~line 10679, inside the K-line/five-baud
/// fast-init keybyte delivery arm): a stray `resolve_cop_tag(&ctx.
/// primitives, Some(cop_handle)).await` re-resolved the tag fresh, long
/// after `run_protocol_init`'s real hardware I/O, instead of reusing this
/// function's own hoisted `cop_tag` (captured once near the function's
/// start, specifically so it survives every intervening `.await` -- see
/// that capture's own doc comment). This test provides the discriminating
/// regression coverage for that call site, mirroring the sibling test
/// above as closely as this arm's own preconditions allow.
///
/// **Why a different fence anchor than the sibling test above.** This arm
/// is reached via `ParamBinding::Plain` (no `apply_params_to_hardware_
/// capturing` call, so no `ctx.api` acquisition before `run_protocol_
/// init`) with `five_baud` set -- the FIRST `ctx.api` acquisition anywhere
/// on this path is `run_protocol_init`'s own `five_baud_init` call.
/// Holding `ctx.api` before `handle_start_comm` is ever polled therefore
/// forces that same call to queue deterministically, exactly like the
/// sibling test's fence, just anchored on a different (here, the only)
/// `ctx.api` acquisition on this path. While parked there, this test
/// removes the `primitives` entry -- landing the race in the gap between
/// the initial `cop_tag` capture and this arm's `ReceivedFrame`
/// construction, with `run_protocol_init`'s real (mocked) K-line wakeup
/// traffic actually in between, exactly as ADR-205's Context describes for
/// this call site.
///
/// Unlike the sibling test, this test's channel is a genuinely
/// `PassThruConnect`ed ISO9141 channel (via the same mocked `api` the poll
/// task itself uses) -- `run_protocol_init`'s `five_baud_init` call must
/// actually SUCCEED to reach the keybyte-delivery arm at all, whereas the
/// sibling test's target arm is reached via a deliberate hardware-apply
/// FAILURE instead. `PassThruConnect` ignores its `DeviceId` argument
/// entirely (no `PassThruOpen` call needed first), and a plain (non-`_PS`)
/// `ISO9141` channel starts with `pins_assigned == true`, so no additional
/// pin-assignment IOCTL is needed either.
///
/// **Fail-without/pass-with, verified directly against this fix
/// (2026-09-01, same session):** temporarily reverted the target call site
/// (`events.rs` ~line 10696, inside the `deliver_keybytes` arm) back to a
/// bare `Some(cop_handle)` with an inlined fresh `resolve_cop_tag(&ctx.
/// primitives, Some(cop_handle)).await` immediately before the
/// `deliver_or_enqueue` call, and the `ReceivedFrame` literal's field back
/// to bare `cop_tag,`. Against the reverted code this test FAILS (the
/// delivered frame's `cop_tag` observed as `None`, since the fresh lookup
/// runs AFTER this test's own concurrent removal). Restoring the fix
/// (`cop_tag: cop_tag.clone()`, reusing the hoisted binding), it PASSES.
#[tokio::test]
async fn cop_tag_capture_survives_a_primitives_removal_racing_the_kline_fast_init_keybyte_arm() {
    let tag = b"startcomm-kline-race-token".to_vec();

    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
    let api = Arc::new(Mutex::new(api));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    std::mem::forget(shutdown_tx);

    // Establish a real ISO9141 channel on the same (mocked) native API this
    // poll task will use -- see this test's own doc comment above for why
    // `run_protocol_init` must actually succeed here, unlike the sibling
    // test's deliberately-unconnected `HSC_TEST_CHANNEL_ID`.
    let channel_id = {
        let api = api.lock().await;
        api.connect(j2534_0404::DeviceId(0), j2534_0404::ISO9141, 0, 10_400)
            .expect("mock PassThruConnect should succeed for a plain ISO9141 channel")
    };

    let mut link = minimal_started_link();
    link.channel_id = Some(channel_id);
    link.protocol = ChannelProtocol::ISO9141;
    link.hw_protocol_id = j2534_0404::ISO9141;

    let logical_links = Arc::new(Mutex::new(HashMap::from([(HSC_TEST_CLL, link)])));
    let primitives = Arc::new(Mutex::new(HashMap::from([(
        HSC_TEST_COP_HANDLE,
        CopEntry {
            cll_handle: HSC_TEST_CLL,
            dispatched: true,
            transmits: false,
            is_send_recv: false,
            cop_tag: Some(tag.clone()),
        },
    )])));
    let drain_watermarks = Arc::new(Mutex::new(HashMap::new()));
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));

    let service = J2534Service {
        api: Arc::clone(&api),
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
        drain_watermarks: Arc::clone(&drain_watermarks),
        shared_channels: Arc::new(Mutex::new(HashMap::new())),
        primitives: Arc::clone(&primitives),
        terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
        next_cll_handle: Arc::new(Mutex::new(0)),
        next_cop_handle: Arc::new(Mutex::new(0)),
        next_connect_generation: Arc::new(Mutex::new(0)),
        next_occupancy_epoch: Arc::new(Mutex::new(0)),
        subscriptions: Arc::clone(&subscriptions),
        shutdown: shutdown_rx,
        module_state: Arc::new(Mutex::new(ModuleState::default())),
        module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
        system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
        j1850_bus_flavor: Arc::new(Mutex::new(None)),
        prog_voltage: Arc::new(Mutex::new(HashMap::new())),
        discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
        discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
    };

    // A five-baud init with `deliver_keybytes: true` (the legacy ISO9141
    // heuristic path's own default, `FiveBaudInit`'s own doc comment) so
    // this call actually reaches the `deliver_keybytes` arm under test.
    let params = StartCommParams {
        protocol_id: j2534_0404::ISO9141,
        base_protocol_id: j2534_0404::ISO9141,
        tester_present: tester_present_disabled(),
        init_tx_flags: 0,
        tx: None,
        five_baud: Some(FiveBaudInit {
            address: 0x33,
            deliver_keybytes: true,
        }),
        fast_init: None,
        binding: ParamBinding::Plain(ComParamSet::default()),
        connect_generation: HSC_TEST_CONNECT_GENERATION,
    };

    // Fence: hold `ctx.api` before `handle_start_comm` is ever polled --
    // the ONLY `ctx.api` acquisition anywhere on this `ParamBinding::Plain`
    // + `five_baud` path before `run_protocol_init`'s own `five_baud_init`
    // call (a `Plain` binding never runs `apply_params_to_hardware_
    // capturing`).
    let api_guard = api.lock().await;

    let task = {
        let primitives = Arc::clone(&primitives);
        let logical_links = Arc::clone(&logical_links);
        let drain_watermarks = Arc::clone(&drain_watermarks);
        let subscriptions = Arc::clone(&subscriptions);
        let service = service.clone();
        tokio::spawn(async move {
            let ctx = ChannelPollCtx {
                channel_id,
                primitives,
                api: Arc::clone(&service.api),
                logical_links,
                drain_watermarks,
                subscriptions,
                module_state: Arc::clone(&service.module_state),
                module_event_buf: Arc::clone(&service.module_event_buf),
                system_event_buf: Arc::clone(&service.system_event_buf),
                executing_cop: Arc::new(Mutex::new(None)),
                last_func_tx: Arc::new(Mutex::new(None)),
                last_phys_tx: Arc::new(Mutex::new(None)),
                last_bus_activity: Arc::new(Mutex::new(tokio::time::Instant::now())),
                rx_supported: true,
                service,
            };
            handle_start_comm(HSC_TEST_COP_HANDLE, HSC_TEST_CLL, params, &ctx).await;
        })
    };

    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !task.is_finished(),
        "handle_start_comm must be parked waiting for ctx.api (inside \
         run_protocol_init's five_baud_init call) before this test's own fence is released"
    );

    // Simulate a concurrent `CancelComPrimitive`/`cancel_link_cops`/hard-error
    // path removing this COP's `primitives` entry in the exact gap this fix
    // closes -- `primitives` is a separate lock from `ctx.api`, so this
    // cannot deadlock against the parked task above.
    primitives.lock().await.remove(&HSC_TEST_COP_HANDLE);

    drop(api_guard);
    task.await.expect("handle_start_comm must not panic");

    let links = logical_links.lock().await;
    let link = links
        .get(&HSC_TEST_CLL)
        .expect("the link must still be present");
    let rx_buf = link.rx_buf.lock().await;
    let frame = rx_buf
        .items
        .iter()
        .find_map(|item| match item {
            CllQueueItem::Frame(frame) => Some(frame),
            _ => None,
        })
        .expect("a ReceivedFrame should have been delivered for the fast-init keybyte response");
    assert_eq!(
        frame.cop_tag,
        Some(tag),
        "the tag captured at the top of handle_start_comm must survive a concurrent \
         primitives removal landing before the K-line fast-init keybyte delivery arm's \
         ReceivedFrame construction"
    );
}

/// design-advisor review (round 5, ADR-205 follow-up): the two sibling tests
/// above both prove the hoisted `cop_tag` SURVIVES a `primitives` removal
/// that lands AFTER the initial capture. Neither proves the capture ITSELF
/// bails when the entry is ALREADY gone by the time it runs -- the pre-fix
/// code (`.get(&cop_handle).and_then(|entry| entry.cop_tag.clone())`, no
/// bail on `None`) would flatten that case to `cop_tag: None` and proceed as
/// if the COP were still live, unconditionally emitting `PduCopstExecuting`
/// and running every downstream side effect for a COP a concurrent
/// cancel/teardown already claimed and terminally emitted for. This test
/// drives exactly that: the `primitives` entry is removed BEFORE
/// `handle_start_comm`'s own initial capture ever runs, and it asserts the
/// function bails with no `PduCopstExecuting` (or anything else) emitted,
/// rather than proceeding with `cop_tag: None`.
///
/// **Fence.** The initial capture's own first `.await` is `resolve_queue_
/// target`'s `logical_links.lock().await` (`events.rs`, top of `handle_
/// start_comm`) -- strictly before `ctx.primitives` is ever touched. Holding
/// `logical_links` here before spawning the task forces it to queue behind
/// this test's own guard at that exact point (`tokio::sync::Mutex`'s
/// FIFO-fair semaphore semantics, same idiom as every other test in this
/// file and in `events_reap_expired_cyclic_registrants_cop_tag_tests.rs`).
/// While parked there, this test removes the `primitives` entry directly (a
/// separate lock, no deadlock risk against the parked task). Releasing
/// `logical_links` then lets `resolve_queue_target` complete and the task
/// proceed straight into its own `ctx.primitives.lock().await` /
/// `prims.get(&cop_handle)` check, landing on the now-already-missing entry
/// -- exactly the window this fix closes.
///
/// **Fail-without/pass-with, verified directly against this fix (2026-09-01,
/// same session)**: temporarily reverted `handle_start_comm`'s initial
/// capture block back to the pre-fix flattening form (`resolve_queue_target`
/// then a separate `ctx.primitives.lock().await.get(&cop_handle).and_then(
/// |entry| entry.cop_tag.clone())`, unconditional `send_cop_status(...,
/// PduCopstExecuting, cop_tag.clone())`, no bail). Against the reverted code
/// this test FAILS: `rx_buf.items` is NOT empty (a `PduCopstExecuting`
/// `CllQueueItem::Status` is enqueued for the already-gone COP) and the task
/// proceeds past Guard A into real side effects. Restoring the fix (bail via
/// `let Some(entry) = prims.get(&cop_handle) else { return; };` before any
/// emission), it PASSES.
#[tokio::test]
async fn handle_start_comm_bails_when_primitives_entry_is_already_gone_before_the_initial_capture()
{
    let tag = b"startcomm-precheck-race-token".to_vec();

    let logical_links = Arc::new(Mutex::new(HashMap::from([(
        HSC_TEST_CLL,
        minimal_started_link(),
    )])));
    let primitives = Arc::new(Mutex::new(HashMap::from([(
        HSC_TEST_COP_HANDLE,
        CopEntry {
            cll_handle: HSC_TEST_CLL,
            dispatched: true,
            transmits: false,
            is_send_recv: false,
            cop_tag: Some(tag),
        },
    )])));
    let drain_watermarks = Arc::new(Mutex::new(HashMap::new()));
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));

    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
    let api = Arc::new(Mutex::new(api));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    std::mem::forget(shutdown_tx);

    let service = J2534Service {
        api: Arc::clone(&api),
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
        drain_watermarks: Arc::clone(&drain_watermarks),
        shared_channels: Arc::new(Mutex::new(HashMap::new())),
        primitives: Arc::clone(&primitives),
        terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
        next_cll_handle: Arc::new(Mutex::new(0)),
        next_cop_handle: Arc::new(Mutex::new(0)),
        next_connect_generation: Arc::new(Mutex::new(0)),
        next_occupancy_epoch: Arc::new(Mutex::new(0)),
        subscriptions: Arc::clone(&subscriptions),
        shutdown: shutdown_rx,
        module_state: Arc::new(Mutex::new(ModuleState::default())),
        module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
        system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
        j1850_bus_flavor: Arc::new(Mutex::new(None)),
        prog_voltage: Arc::new(Mutex::new(HashMap::new())),
        discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
        discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
    };

    let params = StartCommParams {
        protocol_id: j2534_0404::CAN,
        base_protocol_id: j2534_0404::CAN,
        tester_present: tester_present_disabled(),
        init_tx_flags: 0,
        tx: None,
        five_baud: None,
        fast_init: None,
        binding: ParamBinding::Plain(ComParamSet::default()),
        connect_generation: HSC_TEST_CONNECT_GENERATION,
    };

    // Fence: hold `logical_links` before `handle_start_comm` is ever polled --
    // its own initial capture's FIRST `.await` is `resolve_queue_target`'s
    // `logical_links.lock().await`, strictly before `ctx.primitives` is ever
    // touched.
    let links_guard = logical_links.lock().await;

    let task = {
        let primitives = Arc::clone(&primitives);
        let logical_links = Arc::clone(&logical_links);
        let drain_watermarks = Arc::clone(&drain_watermarks);
        let subscriptions = Arc::clone(&subscriptions);
        let service = service.clone();
        tokio::spawn(async move {
            let ctx = ChannelPollCtx {
                channel_id: HSC_TEST_CHANNEL_ID,
                primitives,
                api: Arc::clone(&service.api),
                logical_links,
                drain_watermarks,
                subscriptions,
                module_state: Arc::clone(&service.module_state),
                module_event_buf: Arc::clone(&service.module_event_buf),
                system_event_buf: Arc::clone(&service.system_event_buf),
                executing_cop: Arc::new(Mutex::new(None)),
                last_func_tx: Arc::new(Mutex::new(None)),
                last_phys_tx: Arc::new(Mutex::new(None)),
                last_bus_activity: Arc::new(Mutex::new(tokio::time::Instant::now())),
                rx_supported: true,
                service,
            };
            handle_start_comm(HSC_TEST_COP_HANDLE, HSC_TEST_CLL, params, &ctx).await;
        })
    };

    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !task.is_finished(),
        "handle_start_comm must be parked waiting for logical_links (inside \
         resolve_queue_target, its own initial capture's first await point) before this \
         test's own fence is released"
    );

    // Simulate a concurrent `CancelComPrimitive`/`cancel_link_cops`/hard-error
    // path removing this COP's `primitives` entry BEFORE `handle_start_comm`'s
    // own initial capture ever reaches its `prims.get(&cop_handle)` check --
    // `primitives` is a separate lock from `logical_links`, so this cannot
    // deadlock against the parked task above.
    primitives.lock().await.remove(&HSC_TEST_COP_HANDLE);

    drop(links_guard);
    task.await.expect("handle_start_comm must not panic");

    let links = logical_links.lock().await;
    let link = links
        .get(&HSC_TEST_CLL)
        .expect("the link must still be present");
    assert!(
        !link.comm_started,
        "handle_start_comm must bail before Guard A/comm_started is ever set when its own \
         primitives entry is already gone"
    );
    let rx_buf = link.rx_buf.lock().await;
    assert!(
        rx_buf.items.is_empty(),
        "no PduCopstExecuting (or any other) status/frame should be enqueued once the \
         primitives entry is already gone before the initial liveness check -- the COP that \
         removed it already emitted its own terminal status"
    );
}
