//! Direct unit-level coverage of the round-17 fix (Codex review, P1, PR
//! #101, ADR-192/Phase 7 Stage 7c amendment): `handle_send_recv`'s
//! `ParamBinding::Temp` + `isotp_tx.is_none()` bracket now acquires
//! `ctx.api` ONCE and holds it CONTINUOUSLY across the hardware apply, the
//! native `PassThruWriteMsgs` write, and the revert to live Active -- the
//! same "acquire once, hold across the whole apply/native-call/revert
//! bracket" fence `rpc_primitive.rs::rpc_start_com_primitive`'s round-8 fix
//! already established for the TP2.0 broadcast-periodic-start path (see
//! `rpc_primitive.rs::tp20_broadcast_periodic_api_fence_tests`'s own module
//! doc comment for that precedent).
//!
//! **Technique.** Same established "test task holds `ctx.api` while the
//! code under test runs as a spawned task on this crate's `current_thread`
//! test runtime" fence pattern used by
//! `rpc_primitive.rs::tp20_broadcast_periodic_api_fence_tests` and
//! `events_hard_error_broadcast_periodic_tests.rs`'s round-16 fence tests:
//! every `.await` `handle_send_recv` reaches before its own
//! `ctx.api.lock().await` is an uncontended `Mutex::lock` (`primitives`,
//! `logical_links`; `wait_for_p3_gap` is a no-op here since this cycle's
//! `can_functional` is `None`), so on this crate's `current_thread` runtime
//! a bounded burst of `tokio::task::yield_now` calls deterministically runs
//! the spawned task exactly up to (and no further than) the contended
//! `ctx.api` acquisition, never past it.
//!
//! **Why a direct unit test, not a `tests/grpc_mock` one.** Mirrors
//! `rpc_primitive.rs::tp20_broadcast_periodic_api_fence_tests`'s own
//! rationale exactly: proving "nothing observable happens while a sibling
//! holds `ctx.api`, and the whole bracket completes atomically once it is
//! released" needs a test task that can itself hold `ctx.api` across the
//! call under test, which the gRPC surface has no way to arrange. A literal
//! two-sided clobber repro (a genuinely concurrent sibling CLL applying a
//! DIFFERENT value mid-bracket) is not attempted here either, for the same
//! reason `rpc_primitive.rs`'s own round-8 fix accepted for its analogous
//! bracket (`ADR-192`'s Consequences, Fix 5's round-8 update): this crate's
//! `current_thread` test runtime has no production yield hook to force that
//! interleaving deterministically. What is asserted instead is the fence
//! property that rules the clobber out by construction: the entire
//! apply -> write -> revert sequence is provably inseparable from a single
//! `ctx.api` acquisition, so nothing else touching `ctx.api` (a sibling's
//! own bracket, included) can ever observe or land inside it.
//!
//! **Round 18 addition (edge-case-hunter finding, PR #101):** the module's
//! original headline test below only proved "no progress while a sibling
//! ALREADY holds `ctx.api`" -- a property the pre-fix split-bracket code
//! also satisfies (it too blocks entirely on `ctx.api` for its own apply
//! step), so it does not by itself discriminate this fix from the bug it
//! fixes. `temp_bound_cycle_excludes_a_racing_sibling_from_the_apply_write_gap`
//! closes that gap: rather than the sibling ALREADY holding `ctx.api` before
//! the cycle even starts, it is deterministically queued BEHIND the cycle's
//! own first acquisition attempt (still no literal two-sided value clobber,
//! per the paragraph above, but a genuine "can anything land in the gap"
//! proof) -- see that test's own doc comment for the full technique and its
//! mandatory negative-control verification.
//!
//! **Round 18 update (Codex review, P2, PR #101, ADR-192 Decision item 3
//! amendment):** `CP_TP20BroadcastInterval` is `CHANNEL_WIDE_UNUM32`-class
//! (channel-wide, hardware-resident, but deliberately not `PDU_PC_BUSTYPE`),
//! so the revert this bracket performs no longer restores this CLL's own
//! per-CLL `active` copy for it -- it restores the captured PRE-BRACKET
//! HARDWARE value instead (`events::capture_channel_wide_hardware_locked`,
//! called under the same continuously-held `ctx.api` guard this module's
//! whole fence exists to prove, strictly before the temp apply below). The
//! tests below now seed a distinct pre-bracket hardware baseline
//! (`HSRF_PRE_BRACKET_HARDWARE_BROADCAST_INTERVAL`) via `connect_mock_tp20_channel`
//! so the property is actually observable -- if the revert wrongly fell back
//! to `active`'s own value (`HSRF_ACTIVE_BROADCAST_INTERVAL`), that would be
//! a different value the assertions below would catch.

use super::super::PARAM_TP20_BROADCAST_INTERVAL;
use super::*;

const HSRF_TEST_CLL: u32 = 1;
const HSRF_COP_HANDLE: u32 = 900;
const HSRF_CONNECT_GENERATION: u64 = 9;

/// Active's own `CP_TP20BroadcastInterval` (ComParam class, not
/// `PDU_PC_BUSTYPE` -- see `events_revert_hardware_to_live_active_locked_tests.rs`'s
/// own regression-fence tests for that classification). No longer the
/// revert's own restore target (round 18: `CP_TP20BroadcastInterval` is also
/// `CHANNEL_WIDE_UNUM32`-class, so `active`'s own copy of it is stripped
/// before the revert's `active` push) -- kept only to prove that stale,
/// per-CLL value is NOT what hardware ends up holding; see
/// `HSRF_PRE_BRACKET_HARDWARE_BROADCAST_INTERVAL` for the actual restore
/// target.
const HSRF_ACTIVE_BROADCAST_INTERVAL: u32 = 20;
/// The bound Working snapshot's own value for the same param -- what the
/// apply half of the bracket temporarily pushes to hardware for this one
/// cycle's native write.
const HSRF_WORKING_BROADCAST_INTERVAL: u32 = 250;
/// The channel's real pre-bracket HARDWARE value for `CP_TP20BroadcastInterval`,
/// seeded via `connect_mock_tp20_channel` (round 18, Codex review, P2, PR
/// #101, ADR-192 Decision item 3 amendment) -- deliberately distinct from
/// both `HSRF_ACTIVE_BROADCAST_INTERVAL` and `HSRF_WORKING_BROADCAST_INTERVAL`
/// so the revert's actual restore target (this captured hardware baseline,
/// not this CLL's own `active`) is unambiguously observable.
const HSRF_PRE_BRACKET_HARDWARE_BROADCAST_INTERVAL: u32 = 33;

fn config_value(lib_path: &std::path::Path, channel_id: u32, param_id: u32) -> u32 {
    unsafe {
        let lib = j2534_0404_sys::libloading::Library::new(lib_path)
            .expect("mock library should be loadable");
        let f: j2534_0404_sys::libloading::Symbol<
            unsafe extern "system" fn(u32, u32, *mut u32) -> std::os::raw::c_long,
        > = lib
            .get(b"__mock_get_config_value\0")
            .expect("__mock_get_config_value should be exported");
        let mut value = 0u32;
        let rc = f(channel_id, param_id, &mut value);
        assert_eq!(
            rc,
            j2534_0404_sys::bindings::STATUS_NOERROR as std::os::raw::c_long,
            "__mock_get_config_value should succeed for a connected channel"
        );
        value
    }
}

/// Same shape as `events_revert_hardware_to_live_active_locked_tests.rs`'s
/// own `set_config_param_log`.
fn set_config_param_log(lib_path: &std::path::Path, channel_id: u32) -> Vec<u32> {
    unsafe {
        let lib = j2534_0404_sys::libloading::Library::new(lib_path)
            .expect("mock library should be loadable");
        let count_fn: j2534_0404_sys::libloading::Symbol<unsafe extern "system" fn(u32) -> usize> =
            lib.get(b"__mock_get_set_config_param_log_count\0")
                .expect("__mock_get_set_config_param_log_count should be exported");
        let entry_fn: j2534_0404_sys::libloading::Symbol<
            unsafe extern "system" fn(u32, usize, *mut u32) -> std::os::raw::c_long,
        > = lib
            .get(b"__mock_get_set_config_param_log_entry\0")
            .expect("__mock_get_set_config_param_log_entry should be exported");
        let count = count_fn(channel_id);
        (0..count)
            .map(|index| {
                let mut value = 0u32;
                let rc = entry_fn(channel_id, index, &mut value);
                assert_eq!(
                    rc,
                    j2534_0404_sys::bindings::STATUS_NOERROR as std::os::raw::c_long,
                    "__mock_get_set_config_param_log_entry should succeed within count"
                );
                value
            })
            .collect()
    }
}

/// Channel-scoped native write count (`PassThruWriteMsgs`), mirroring
/// `tests/grpc_mock/harness.rs`'s own `MockBackdoor::written_count` --
/// re-implemented here via a fresh `Library` handle since this unit test
/// lives outside the `grpc_mock` harness (same technique this file's sibling
/// test modules already use for their own backdoor accessors).
fn written_count(lib_path: &std::path::Path, channel_id: u32) -> usize {
    unsafe {
        let lib = j2534_0404_sys::libloading::Library::new(lib_path)
            .expect("mock library should be loadable");
        let f: j2534_0404_sys::libloading::Symbol<unsafe extern "system" fn(u32) -> usize> = lib
            .get(b"__mock_get_written_msg_count\0")
            .expect("__mock_get_written_msg_count should be exported");
        f(channel_id)
    }
}

/// A connected TP2.0_PS CLL whose Active carries
/// `HSRF_ACTIVE_BROADCAST_INTERVAL` -- deliberately NOT the revert target
/// (round 18: `CP_TP20BroadcastInterval` is `CHANNEL_WIDE_UNUM32`-class, so
/// this stale per-CLL copy is stripped before the revert's `active` push);
/// see `HSRF_PRE_BRACKET_HARDWARE_BROADCAST_INTERVAL` for the actual restore
/// target.
fn link_on(channel_id: ChannelId) -> LogicalLinkState {
    let mut active = ComParamSet::default();
    active.unum32.insert(
        PARAM_TP20_BROADCAST_INTERVAL,
        HSRF_ACTIVE_BROADCAST_INTERVAL,
    );

    LogicalLinkState {
        channel_id: Some(channel_id),
        protocol: ChannelProtocol::TP2_0_PS,
        hw_protocol_id: j2534_0404::PROTOCOL_TP2_0_PS,
        software_isotp: false,
        uudt_channel_id: None,
        uudt_channel_key: None,
        isotp_rx: Arc::new(Mutex::new(HashMap::new())),
        connect_in_flight: std::sync::Weak::new(),
        connected: true,
        comm_started: true,
        raw_mode: false,
        checksum_mode: false,
        connect_generation: HSRF_CONNECT_GENERATION,
        stop_comm_pending: false,
        channel_key: Some((j2534_0404::PROTOCOL_TP2_0_PS, 500_000, 0, 0)),
        pin_select: None,
        channel_index: None,
        base_hw_protocol_override: None,
        rx_buf: Arc::new(Mutex::new(CllEventQueue {
            event_queue_cap: 16,
            ..CllEventQueue::default()
        })),
        working: ComParamSet::default(),
        active,
        tester_present_state: TesterPresentState::None,
        tester_present_base_tx_flags: 0,
        open_tp_discards: Vec::new(),
        working_unique_resp_id_table: Vec::new(),
        active_unique_resp_id_table: Vec::new(),
        unique_resp_filter_ids: Vec::new(),
        cancelled_cops: HashSet::new(),
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

/// Opens a real mock channel and binds `CONFIG_J1962_PINS` (SAE J2534-2
/// clause 6.3.3.2 requires this before a `_PS` channel accepts any I/O) --
/// mirrors `rpc_primitive.rs`'s own `connect_mock_tp20_channel`. Also seeds
/// the channel's real pre-bracket hardware value for `CONFIG_TP2_0_T_BR_INT`
/// (round 18, Codex review, P2, PR #101, ADR-192 Decision item 3
/// amendment) -- distinct from this CLL's own `active` copy
/// (`HSRF_ACTIVE_BROADCAST_INTERVAL`), so a test proving the revert restores
/// the captured hardware baseline (not `active`) has an observable
/// difference to assert on. Seeded here, BEFORE any test captures its own
/// `baseline_log`/`baseline_written` counters, so it never shows up in a
/// test's own delta.
fn connect_mock_tp20_channel(api: &j2534_0404::J2534Api0404) -> ChannelId {
    let device = api.open(None).expect("mock PassThruOpen should succeed");
    let channel = api
        .connect(device, j2534_0404::PROTOCOL_TP2_0_PS, 0, 500_000)
        .expect("mock PassThruConnect should succeed");
    api.set_config(channel, &[(j2534_0404::CONFIG_J1962_PINS, 0x0000_0106)])
        .expect("binding J1962 pins on a _PS channel should succeed");
    api.set_config_u32(
        channel,
        j2534_0404::CONFIG_TP2_0_T_BR_INT,
        HSRF_PRE_BRACKET_HARDWARE_BROADCAST_INTERVAL,
    )
    .expect("seeding the pre-bracket hardware baseline should succeed");
    channel
}

/// Builds a full `ChannelPollCtx` around one connected TP2.0_PS CLL and a
/// matching `primitives` entry -- mirrors
/// `events_j1939_claim.rs::tests::ctx_with_a_send_recv_cop`'s and
/// `events_hard_error_broadcast_periodic_tests.rs::ctx_with_a_live_broadcast_periodic`'s
/// own construction shape.
async fn ctx_with_a_connected_link() -> (ChannelPollCtx, ChannelId, std::path::PathBuf) {
    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
    let channel_id = connect_mock_tp20_channel(&api);

    let mut logical_links = HashMap::new();
    logical_links.insert(HSRF_TEST_CLL, link_on(channel_id));
    let logical_links = Arc::new(Mutex::new(logical_links));

    let mut primitives = HashMap::new();
    primitives.insert(
        HSRF_COP_HANDLE,
        CopEntry {
            cll_handle: HSRF_TEST_CLL,
            dispatched: true,
            transmits: true,
            is_send_recv: true,
            cop_tag: None,
        },
    );
    let primitives = Arc::new(Mutex::new(primitives));

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
        shared_channels: Arc::new(Mutex::new(HashMap::new())),
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

    let ctx = ChannelPollCtx {
        channel_id,
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
    };
    (ctx, channel_id, lib_path)
}

/// Clones every field of `ctx` -- every field is an `Arc` handle cheap to
/// clone, plus `J2534Service`'s own `#[derive(Clone)]` -- so the SAME
/// underlying handles (most importantly `ctx.api`) can be shared into a
/// `tokio::spawn`ed task. Mirrors
/// `events_hard_error_broadcast_periodic_tests.rs::clone_ctx` exactly.
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

/// A single-shot (`send_cycles_remaining == 1`), receive-nothing
/// (`num_receive_cycles == 0`), `ParamBinding::Temp`, `isotp_tx.is_none()`
/// cycle -- exactly the scope this fix covers (design-advisor consult:
/// every hardware-transport `ParamBinding::Temp` send, TP2.0 or otherwise).
/// `can_functional: None` keeps `wait_for_p3_gap` a no-op so the only
/// contended `.await` on the path to the fence is `ctx.api.lock().await`
/// itself.
fn temp_bound_cycle() -> SendRecvCycle {
    let mut effective = ComParamSet::default();
    effective.unum32.insert(
        PARAM_TP20_BROADCAST_INTERVAL,
        HSRF_WORKING_BROADCAST_INTERVAL,
    );

    SendRecvCycle {
        cop_handle: HSRF_COP_HANDLE,
        cll_handle: HSRF_TEST_CLL,
        protocol_id: j2534_0404::PROTOCOL_TP2_0_PS,
        logical_protocol: ChannelProtocol::TP2_0_PS,
        base_protocol_id: j2534_0404::PROTOCOL_TP2_0_PS,
        tx: SendRecvTx {
            data: vec![0xAA, 0x00, 0x00, 0x00, 0x01, 0x02],
            tx_flags: 0,
            isotp_tx: None,
            can_functional: None,
            request_sid: None,
            access_timing_request: None,
            j1939_tx_source: None,
            tp20_established_tx_id: None,
            tp20_is_broadcast: false,
            tx_prefix: 0,
        },
        binding: ParamBinding::Temp { effective },
        expected_response: Vec::new(),
        cycle_time_ms: 0,
        send_cycles_remaining: 1,
        num_receive_cycles: 0,
        connect_generation: HSRF_CONNECT_GENERATION,
    }
}

/// While this test task holds `ctx.api` (standing in for a sibling operation
/// already in progress, or about to run, on the same physical channel), a
/// concurrently spawned `handle_send_recv` for a `ParamBinding::Temp` +
/// `isotp_tx.is_none()` cycle must make NO observable progress at all -- not
/// even its own hardware apply. Once the guard is released, the whole apply
/// -> native write -> revert bracket completes in one shot, correctly.
///
/// **What this test does NOT prove** (edge-case-hunter finding, round 18,
/// PR #101): "no progress while a sibling already holds `ctx.api`" alone
/// does not discriminate this fix from the OLD, buggy split-bracket shape --
/// the old code ALSO blocked entirely on `ctx.api` for its own apply step,
/// so it passes this exact test too. The property that actually
/// discriminates the fix -- that nothing can be interposed BETWEEN this
/// cycle's own apply and its own native write once the fix's continuous
/// bracket is in place -- is proven separately below, by
/// `temp_bound_cycle_excludes_a_racing_sibling_from_the_apply_write_gap`.
/// This test is kept as a simpler, complementary regression fence for the
/// "no partial progress while blocked" property, which remains true and
/// worth pinning on its own.
#[tokio::test]
async fn temp_bound_cycle_makes_no_progress_while_a_sibling_holds_the_fence() {
    let (ctx, channel_id, lib_path) = ctx_with_a_connected_link().await;
    let baseline_log = set_config_param_log(&lib_path, channel_id.0).len();
    let baseline_written = written_count(&lib_path, channel_id.0);
    // `connect_mock_tp20_channel` already seeded the pre-bracket hardware
    // baseline (`HSRF_PRE_BRACKET_HARDWARE_BROADCAST_INTERVAL`) before this
    // baseline read -- the load-bearing claim here is "unchanged from
    // whatever it started as while fenced off", not a specific value.
    let baseline_value = config_value(&lib_path, channel_id.0, j2534_0404::CONFIG_TP2_0_T_BR_INT);

    let api = ctx.api.lock().await;
    let spawned_ctx = clone_ctx(&ctx);
    let task =
        tokio::spawn(async move { handle_send_recv(temp_bound_cycle(), &spawned_ctx).await });

    for _ in 0..48 {
        tokio::task::yield_now().await;
    }
    assert!(
        !task.is_finished(),
        "handle_send_recv must be parked on ctx.api before it may apply/write/revert anything"
    );
    assert_eq!(
        set_config_param_log(&lib_path, channel_id.0).len(),
        baseline_log,
        "no SET_CONFIG call -- not even the apply half of the bracket -- may happen while a \
         sibling holds ctx.api"
    );
    assert_eq!(
        written_count(&lib_path, channel_id.0),
        baseline_written,
        "no native write may happen while a sibling holds ctx.api"
    );
    assert_eq!(
        config_value(&lib_path, channel_id.0, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        baseline_value,
        "hardware must be unchanged while the bracket is fenced off -- not even the apply half \
         may run"
    );

    drop(api);
    let continuation = task.await.expect("handle_send_recv must not panic");
    assert!(
        continuation.is_none(),
        "a single-shot (send_cycles_remaining == 1) cycle ends here, no continuation"
    );

    assert_eq!(
        written_count(&lib_path, channel_id.0),
        baseline_written + 1,
        "exactly one native write must have happened once the fence was released"
    );
    assert_eq!(
        config_value(&lib_path, channel_id.0, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        HSRF_PRE_BRACKET_HARDWARE_BROADCAST_INTERVAL,
        "hardware must be reverted to the captured PRE-BRACKET HARDWARE baseline for \
         CP_TP20BroadcastInterval once the bracket completes, not left on the temp-applied \
         Working value, and not this CLL's own (stale, per-CLL) Active copy either \
         (HSRF_ACTIVE_BROADCAST_INTERVAL) -- round 18, Codex review, P2, PR #101, ADR-192 \
         Decision item 3 amendment"
    );

    // Codex review round 8 Finding 1's own adjacency check, mirrored here
    // for this second call site (`tests/grpc_mock/tp20.rs`'s
    // `broadcast_periodic_temp_param_update_applies_and_reverts_broadcast_interval`):
    // the apply batch and the revert batch pushed by this ONE cycle must be
    // adjacent, identical-id sequences -- nothing else may interleave
    // between them.
    let full_log = set_config_param_log(&lib_path, channel_id.0);
    let delta = &full_log[baseline_log..];
    assert!(
        !delta.is_empty() && delta.len().is_multiple_of(2),
        "expected exactly two same-length SET_CONFIG batches (apply, revert) in the delta, got \
         {delta:?}"
    );
    let batch_len = delta.len() / 2;
    assert_eq!(
        delta[..batch_len],
        delta[batch_len..],
        "the apply batch and the revert batch must be adjacent, identical-id sequences -- no \
         other channel-affecting SET_CONFIG op may interleave between them"
    );
}

/// **The discriminating fence test** (edge-case-hunter finding, round 18,
/// PR #101). Unlike the headline test above -- which only proves "no
/// progress while `ctx.api` is held elsewhere," a property the OLD,
/// pre-fix split-bracket code ALSO satisfies, since it too blocks entirely
/// on `ctx.api` for its own apply step -- this test proves the property
/// that actually distinguishes the fix: once a racing sibling has been
/// queued BEHIND this cycle's own first `ctx.api` acquisition attempt, it
/// can never be granted the lock again until the cycle's entire
/// apply -> native write -> revert bracket has completed. Under the OLD
/// split-bracket shape (apply, write, and revert each separately locking
/// and releasing `ctx.api`), a sibling queued this way would win the lock
/// the moment the apply half releases it -- landing its own distinguishable
/// SET_CONFIG call BETWEEN this cycle's own apply batch and its own revert
/// batch. Under the fix, it cannot land until strictly after both.
///
/// **Technique.** `tokio::sync::Mutex` (what `ctx.api` is) grants its
/// internal semaphore permit to already-queued waiters in FIFO registration
/// order -- a released permit goes to whichever task is already parked
/// `Pending` on `.lock().await` first, even ahead of a brand-new
/// `.lock().await` call issued "synchronously" (same task poll, no
/// intervening yield) by whoever just released it. So driving BOTH the
/// cycle-under-test task and the sibling task to a `Pending` `ctx.api.lock()`
/// -- in that order -- before this test releases its own initial hold
/// deterministically reproduces "sibling queued behind the cycle's first
/// acquisition" on this crate's `current_thread` test runtime, using the
/// same bounded-`yield_now`-burst parking technique the headline test above
/// (and `rpc_primitive.rs::tp20_broadcast_periodic_api_fence_tests`) already
/// use.
///
/// The sibling's own observable action is a direct, distinguishable
/// `PassThruIoctl SET_CONFIG` call (`LOOPBACK`, a param ID never touched by
/// this cycle's own apply/revert, which only ever push `CONFIG_TP2_0_T_BR_INT`
/// -- and, unlike `CONFIG_J1962_PINS`, one the mock accepts repeatedly rather
/// than rejecting after the first bind) against the SAME channel, so its
/// position in `set_config_param_log`'s ordered sequence directly reveals
/// whether it landed inside the cycle's own apply/revert pair or strictly
/// after it.
///
/// **Negative-control verification (mandatory per this fix's own brief):**
/// confirmed to FAIL when the fix is disabled by temporarily inserting a
/// `tokio::task::yield_now().await` immediately after
/// `apply_params_to_hardware_locked` succeeds and before
/// `transmit_request_locked` inside `events.rs`'s `isotp_tx.is_none()`
/// bracket -- simulating the old split-bracket's release-and-reacquire gap
/// without needing the exact pre-fix diff. With that yield inserted, this
/// test's own final assertion fails: the marker lands at `delta[1]` (between
/// the apply and revert entries) instead of `delta[2]` (strictly after
/// both). Removing the simulated gap restores a passing test. (The headline
/// test above was re-run under the same simulated gap too, exactly as the
/// edge-case-hunter's own repro did, and -- as expected, since it does not
/// discriminate -- it still passed.)
#[tokio::test]
async fn temp_bound_cycle_excludes_a_racing_sibling_from_the_apply_write_gap() {
    let (ctx, channel_id, lib_path) = ctx_with_a_connected_link().await;
    let baseline_log = set_config_param_log(&lib_path, channel_id.0).len();

    let api = ctx.api.lock().await;

    // The cycle under test -- queued first.
    let spawned_ctx = clone_ctx(&ctx);
    let cycle_task =
        tokio::spawn(async move { handle_send_recv(temp_bound_cycle(), &spawned_ctx).await });
    for _ in 0..48 {
        tokio::task::yield_now().await;
    }
    assert!(
        !cycle_task.is_finished(),
        "the cycle must be parked on ctx.api, queued first, before the racing sibling below is \
         even spawned"
    );

    // The racing sibling -- queued second, strictly behind the cycle.
    let sibling_api = Arc::clone(&ctx.api);
    let sibling_task = tokio::spawn(async move {
        let api = sibling_api.lock().await;
        api.set_config(channel_id, &[(j2534_0404::LOOPBACK, 1)])
            .expect("sibling SET_CONFIG should succeed");
    });
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !sibling_task.is_finished(),
        "the sibling must also be parked on ctx.api, queued strictly behind the cycle, before \
         the test releases its own hold"
    );

    // Release the test's own hold: the cycle (queued first) wins the lock
    // next, the sibling (queued second) only after the cycle's whole
    // bracket releases it.
    drop(api);
    let continuation = cycle_task.await.expect("handle_send_recv must not panic");
    assert!(
        continuation.is_none(),
        "a single-shot (send_cycles_remaining == 1) cycle ends here, no continuation"
    );
    sibling_task.await.expect("the sibling task must not panic");

    let full_log = set_config_param_log(&lib_path, channel_id.0);
    let delta = &full_log[baseline_log..];
    assert_eq!(
        delta.len(),
        3,
        "expected exactly 3 SET_CONFIG param ids in the delta: the cycle's own apply, its own \
         revert, and the sibling's marker -- got {delta:?}"
    );
    assert_eq!(
        delta[0], delta[1],
        "the cycle's own apply and revert batches must push the SAME param id (the apply pushes \
         effective's single key, CP_TP20BroadcastInterval; the revert pushes the captured \
         channel-wide restore pair for the same config id -- round 18, ADR-192 Decision item 3 \
         amendment)"
    );
    assert_eq!(
        delta[0],
        j2534_0404::CONFIG_TP2_0_T_BR_INT,
        "the cycle's own apply/revert param id must be the raw config id CP_TP20BroadcastInterval \
         maps to on a TP2.0 link"
    );
    assert_eq!(
        delta[2],
        j2534_0404::LOOPBACK,
        "the racing sibling's own distinguishable SET_CONFIG must land STRICTLY AFTER the \
         cycle's complete apply+revert pair, never interposed between them -- this is the \
         property that actually discriminates the continuous-bracket fix from the old \
         split-bracket shape (see this test's own doc comment)"
    );
}

/// Steady-state sanity check with no external fence held: the same cycle
/// completes correctly end to end on its own, confirming the assertions
/// above are pinning genuine behavior and not an artifact of the held-guard
/// setup.
#[tokio::test]
async fn temp_bound_cycle_applies_writes_and_reverts_when_unfenced() {
    let (ctx, channel_id, lib_path) = ctx_with_a_connected_link().await;
    let baseline_written = written_count(&lib_path, channel_id.0);

    let continuation = handle_send_recv(temp_bound_cycle(), &ctx).await;
    assert!(continuation.is_none());

    assert_eq!(written_count(&lib_path, channel_id.0), baseline_written + 1);
    assert_eq!(
        config_value(&lib_path, channel_id.0, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        HSRF_PRE_BRACKET_HARDWARE_BROADCAST_INTERVAL,
        "hardware must end up back on the captured PRE-BRACKET HARDWARE baseline, not the \
         temp-applied Working value, and not this CLL's own (stale, per-CLL) Active copy either \
         -- round 18, Codex review, P2, PR #101, ADR-192 Decision item 3 amendment"
    );
    assert!(
        !ctx.primitives.lock().await.contains_key(&HSRF_COP_HANDLE),
        "a single-shot cycle's own primitives entry must be cleaned up once it finishes"
    );
}
