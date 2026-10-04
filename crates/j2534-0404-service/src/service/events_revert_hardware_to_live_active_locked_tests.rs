//! Direct unit-level coverage of `revert_hardware_to_live_active_locked`
//! (Codex review round 8 Finding 1, PR #101, ADR-192/Phase 7 Stage 7c) --
//! no direct test coverage previously existed for `revert_hardware_to_live_
//! active`/`_locked` (confirmed by search before writing this file). This
//! pins the core contract standalone, against an already-held mock `api`
//! reference (the function's own contract -- it takes `&J2534Api0404`, not
//! the `Arc<Mutex<...>>` wrapper, so simply holding `&api` here already
//! demonstrates the "caller already holds `api`" shape the function is
//! written for).
//!
//! Construction mirrors `rpc_primitive.rs`'s `service_with_a_started_link`
//! (`api.open`/`api.connect` against the mock cdylib) and its sibling
//! `finalize_or_orphan_broadcast_periodic_start_tests::stop_periodic_call_
//! count`'s own documented rationale for reading mock state back through a
//! *fresh* `libloading::Library` handle on the identical path, rather than
//! a statically-linked `j2534_0404_mock` accessor: a fresh `Library::new`
//! on the same path resolves to the SAME dynamically-loaded shared object
//! `api` itself mutates.

use std::os::raw::c_long;

use j2534_0404_sys::libloading::{Library, Symbol};

use super::super::PARAM_TP20_BROADCAST_INTERVAL;
use super::*;

const RHTLAT_CLL: u32 = 1;

fn config_value(lib_path: &std::path::Path, channel_id: u32, param_id: u32) -> u32 {
    unsafe {
        let lib = Library::new(lib_path).expect("mock library should be loadable");
        let f: Symbol<unsafe extern "system" fn(u32, u32, *mut u32) -> c_long> = lib
            .get(b"__mock_get_config_value\0")
            .expect("__mock_get_config_value should be exported");
        let mut value = 0u32;
        let rc = f(channel_id, param_id, &mut value);
        assert_eq!(
            rc,
            j2534_0404_sys::bindings::STATUS_NOERROR as c_long,
            "__mock_get_config_value should succeed for a connected channel"
        );
        value
    }
}

/// The `Parameter` id of every successfully applied `IOCTL_SET_CONFIG`
/// entry on `channel_id`, in call order -- same backdoor shape as
/// `tests/grpc_mock/harness.rs`'s `MockBackdoor::set_config_param_log`,
/// re-implemented here via a fresh `Library` handle since this unit test
/// lives outside the `grpc_mock` harness.
fn set_config_param_log(lib_path: &std::path::Path, channel_id: u32) -> Vec<u32> {
    unsafe {
        let lib = Library::new(lib_path).expect("mock library should be loadable");
        let count_fn: Symbol<unsafe extern "system" fn(u32) -> usize> = lib
            .get(b"__mock_get_set_config_param_log_count\0")
            .expect("__mock_get_set_config_param_log_count should be exported");
        let entry_fn: Symbol<unsafe extern "system" fn(u32, usize, *mut u32) -> c_long> = lib
            .get(b"__mock_get_set_config_param_log_entry\0")
            .expect("__mock_get_set_config_param_log_entry should be exported");
        let count = count_fn(channel_id);
        (0..count)
            .map(|index| {
                let mut value = 0u32;
                let rc = entry_fn(channel_id, index, &mut value);
                assert_eq!(
                    rc,
                    j2534_0404_sys::bindings::STATUS_NOERROR as c_long,
                    "__mock_get_set_config_param_log_entry should succeed within count"
                );
                value
            })
            .collect()
    }
}

/// A hand-seeded `LogicalLinkState` connected on `channel_id` with `active`
/// holding both a `PDU_PC_BUSTYPE`-class param (`BIT_SAMPLE_POINT`) and the
/// deliberately non-BUSTYPE, pacing-class `PARAM_TP20_BROADCAST_INTERVAL`
/// (Codex review round 8 Finding 2 -- see `comparam_support.rs`'s own
/// regression-fence tests for that classification decision), set to
/// `broadcast_interval` -- parameterized (round 18, Codex review, P2, PR
/// #101, ADR-192 Decision item 3 amendment) so
/// `sibling_cll_channel_wide_clobber_is_avoided_by_the_captured_restore`
/// below can build two distinct CLLs sharing one physical channel with two
/// distinct `active` copies of the same channel-wide key.
fn link_with_active(channel_id: ChannelId, broadcast_interval: u32) -> LogicalLinkState {
    let mut active = ComParamSet::default();
    active
        .unum32
        .insert(ComParamId(j2534_0404::BIT_SAMPLE_POINT), 80);
    active
        .unum32
        .insert(PARAM_TP20_BROADCAST_INTERVAL, broadcast_interval);

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
        connect_generation: 1,
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

/// Core contract: called with an already-held (here, merely borrowed --
/// this function never itself locks `api`) mock `api` reference, this
/// pushes `active` to hardware with every `PDU_PC_BUSTYPE`-class key
/// stripped (ADR-110) and every `CHANNEL_WIDE_UNUM32` key stripped ONLY
/// when a successfully-captured restore pair actually exists for it in
/// `channel_wide_restore` (round 18, Codex review, P2, PR #101, ADR-192
/// Decision item 3 amendment; corrected by edge-case-hunter adversarial
/// review, PR #101, BLOCKING Finding 1): `BIT_SAMPLE_POINT` (BUSTYPE) must
/// NOT reach hardware at all. `PARAM_TP20_BROADCAST_INTERVAL` is
/// deliberately non-BUSTYPE (Finding 2) but is `CHANNEL_WIDE_UNUM32`-class
/// -- with an EMPTY `channel_wide_restore` here (no captured pair for it),
/// it must fall back to its pre-round-18 behavior: pushed from this CLL's
/// own `active` copy like any other non-channel-wide key, NOT dropped with
/// nothing to replace it (the BLOCKING bug: an unconditional strip left a
/// capture-failed key with no restore source at all, leaking the bracket's
/// temp-bound value forever). See `restores_captured_channel_wide_value_
/// over_stale_active` below for the companion test proving what happens
/// when a captured pair DOES exist (the captured value wins over `active`,
/// and the key IS stripped from this push).
#[tokio::test]
async fn pushes_active_with_bustype_stripped_and_channel_wide_key_falls_back_to_active_when_uncaptured()
 {
    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
    let device_id = api.open(None).expect("mock open");
    let channel_id = api
        .connect(device_id, j2534_0404::PROTOCOL_TP2_0_PS, 0, 500_000)
        .expect("mock connect");

    let mut logical_links = HashMap::new();
    logical_links.insert(RHTLAT_CLL, link_with_active(channel_id, 77));
    let logical_links = Arc::new(Mutex::new(logical_links));

    revert_hardware_to_live_active_locked(
        &api,
        &logical_links,
        RHTLAT_CLL,
        channel_id,
        j2534_0404::PROTOCOL_TP2_0_PS,
        &[],
    )
    .await;

    let log = set_config_param_log(&lib_path, channel_id.0);
    assert!(
        !log.contains(&j2534_0404::BIT_SAMPLE_POINT),
        "a PDU_PC_BUSTYPE-class key (BIT_SAMPLE_POINT) must never be pushed to hardware by a \
         revert -- ADR-110 (ISO 22900-2 §9.4.16.2.1 c) NOTE 2 / d))"
    );
    assert_eq!(
        config_value(&lib_path, channel_id.0, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        77,
        "a CHANNEL_WIDE_UNUM32-class key with NO captured restore pair (empty \
         channel_wide_restore) must fall back to being restored from this CLL's own `active` \
         (77), same as the pre-round-18 behavior -- it must never be left completely \
         unrestored (the BLOCKING capture-failure leak this fallback closes)"
    );
}

/// The BLOCKING-Finding-1 fallback, proven directly against a simulated
/// `capture_channel_wide_hardware_locked` `GET_CONFIG` failure (edge-case-
/// hunter adversarial review, PR #101): passing an empty `channel_wide_
/// restore` while hardware and this CLL's `active` both hold a live value
/// for `PARAM_TP20_BROADCAST_INTERVAL` stands in for "the capture attempt
/// failed to read this key back" -- the revert must still restore the key,
/// from `active`, rather than leaving whatever the bracket's own temp apply
/// left on hardware in place forever. This directly discriminates the fix
/// from the pre-fix BLOCKING bug: seed hardware with a temp-bound value
/// that differs from `active`, then confirm the revert overwrites it with
/// `active`'s value rather than leaving the temp value in place.
#[tokio::test]
async fn capture_failure_falls_back_to_restoring_from_active_instead_of_leaking_temp_value() {
    const ACTIVE_BROADCAST_INTERVAL: u32 = 77;
    const TEMP_BOUND_BROADCAST_INTERVAL: u32 = 250;

    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
    let device_id = api.open(None).expect("mock open");
    let channel_id = api
        .connect(device_id, j2534_0404::PROTOCOL_TP2_0_PS, 0, 500_000)
        .expect("mock connect");

    // Stands in for the bracket's own temp-bound apply, run before this
    // test's revert call -- exactly like a real apply -> ... -> revert
    // bracket, hardware is left holding a value that must not survive the
    // revert.
    api.set_config_u32(
        channel_id,
        j2534_0404::CONFIG_TP2_0_T_BR_INT,
        TEMP_BOUND_BROADCAST_INTERVAL,
    )
    .expect("seeding the temp-bound hardware value should succeed");

    let mut logical_links = HashMap::new();
    logical_links.insert(
        RHTLAT_CLL,
        link_with_active(channel_id, ACTIVE_BROADCAST_INTERVAL),
    );
    let logical_links = Arc::new(Mutex::new(logical_links));

    // Empty channel_wide_restore simulates capture_channel_wide_hardware_
    // locked's GET_CONFIG having failed for this key.
    revert_hardware_to_live_active_locked(
        &api,
        &logical_links,
        RHTLAT_CLL,
        channel_id,
        j2534_0404::PROTOCOL_TP2_0_PS,
        &[],
    )
    .await;

    assert_eq!(
        config_value(&lib_path, channel_id.0, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        ACTIVE_BROADCAST_INTERVAL,
        "on a simulated capture failure (empty channel_wide_restore), the revert must fall back \
         to restoring from this CLL's own active (77), not leave the bracket's temp-bound value \
         (250) live on hardware forever"
    );
}

/// Proves the actual fix (round 18, Codex review, P2, PR #101, ADR-192
/// Decision item 3 amendment): when `channel_wide_restore` carries a
/// captured pre-bracket hardware value for `CONFIG_TP2_0_T_BR_INT` that
/// differs from this CLL's own (stale) `active` copy, the revert restores
/// the CAPTURED value, not `active`'s -- proving the per-CLL Active
/// snapshot is never consulted for a `CHANNEL_WIDE_UNUM32` key once a
/// captured restore pair exists for it.
#[tokio::test]
async fn restores_captured_channel_wide_value_over_stale_active() {
    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
    let device_id = api.open(None).expect("mock open");
    let channel_id = api
        .connect(device_id, j2534_0404::PROTOCOL_TP2_0_PS, 0, 500_000)
        .expect("mock connect");

    // `active` holds 77 (this CLL's own, potentially stale, per-CLL copy),
    // but the captured pre-bracket hardware value is 42 -- proving the
    // restore uses the captured value, not `active`.
    let mut logical_links = HashMap::new();
    logical_links.insert(RHTLAT_CLL, link_with_active(channel_id, 77));
    let logical_links = Arc::new(Mutex::new(logical_links));

    revert_hardware_to_live_active_locked(
        &api,
        &logical_links,
        RHTLAT_CLL,
        channel_id,
        j2534_0404::PROTOCOL_TP2_0_PS,
        &[(j2534_0404::CONFIG_TP2_0_T_BR_INT, 42)],
    )
    .await;

    assert_eq!(
        config_value(&lib_path, channel_id.0, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        42,
        "hardware must end up holding the captured pre-bracket value (42), not this CLL's own \
         stale active copy (77)"
    );
}

/// No `active` entry for this CLL (e.g. a vanished/never-populated
/// `LogicalLinkState`) is a silent no-op for the Active-push half -- mirrors
/// `revert_hardware_to_live_active`'s own doc comment: nothing to revert
/// to, so nothing is pushed. `channel_wide_restore` is `&[]` here too, so
/// the restore half has nothing to do either -- see
/// `restores_channel_wide_value_even_when_cll_absent` below for the
/// companion test proving the restore half runs independently of this one.
#[tokio::test]
async fn no_op_when_cll_absent() {
    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
    let device_id = api.open(None).expect("mock open");
    let channel_id = api
        .connect(device_id, j2534_0404::PROTOCOL_TP2_0_PS, 0, 500_000)
        .expect("mock connect");

    let logical_links = Arc::new(Mutex::new(HashMap::new()));

    revert_hardware_to_live_active_locked(
        &api,
        &logical_links,
        RHTLAT_CLL,
        channel_id,
        j2534_0404::PROTOCOL_TP2_0_PS,
        &[],
    )
    .await;

    assert!(
        set_config_param_log(&lib_path, channel_id.0).is_empty(),
        "no CLL entry and an empty channel_wide_restore means nothing to revert to -- no \
         SET_CONFIG call should be issued"
    );
}

/// Requirement 4 of the fix design (round 18, Codex review, P2, PR #101,
/// ADR-192 Decision item 3 amendment): even when this CLL's own `active`
/// never had the key at all (here, no CLL entry exists at all), a captured
/// `channel_wide_restore` pair must still be restored -- the bracket's own
/// temp apply may have written a temp-bound value for this key that needs
/// restoring regardless of what this CLL's Active says.
#[tokio::test]
async fn restores_channel_wide_value_even_when_cll_absent() {
    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
    let device_id = api.open(None).expect("mock open");
    let channel_id = api
        .connect(device_id, j2534_0404::PROTOCOL_TP2_0_PS, 0, 500_000)
        .expect("mock connect");

    let logical_links = Arc::new(Mutex::new(HashMap::new()));

    revert_hardware_to_live_active_locked(
        &api,
        &logical_links,
        RHTLAT_CLL,
        channel_id,
        j2534_0404::PROTOCOL_TP2_0_PS,
        &[(j2534_0404::CONFIG_TP2_0_T_BR_INT, 55)],
    )
    .await;

    assert_eq!(
        config_value(&lib_path, channel_id.0, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        55,
        "a captured channel_wide_restore pair must be restored even when no CLL entry (and so \
         no active) exists at all"
    );
}

/// The actual sibling-clobber scenario (round 18, Codex review, P2, PR #101,
/// ADR-192 Decision item 3 amendment): two CLLs, `RHTLAT_CLL` ("A") and
/// `SIBLING_CLL` ("B"), sharing ONE physical channel. B's own `active` has
/// already been promoted to hardware (99) -- standing in for a prior,
/// already-completed `CoptUpdateparam` -- so the channel's real, currently-
/// live value is 99, even though A's own (stale, per-CLL) `active` copy of
/// the same key is a completely different value (77). A then runs a
/// `ParamBinding::Temp` bracket of its own: `capture_channel_wide_hardware_
/// locked` captures the channel's real pre-bracket value BEFORE A's apply
/// (exactly as every real call site does, under one continuously-held `api`
/// guard), A's own Working snapshot is temp-applied, and A's own revert runs
/// with the captured restore threaded through.
///
/// Before this fix, the revert would have pushed A's own stale `active`
/// (77), clobbering B's real, live, already-promoted value (99). This test
/// proves hardware ends up back on 99 -- B's value -- never 77.
#[tokio::test]
async fn sibling_cll_channel_wide_clobber_is_avoided_by_the_captured_restore() {
    const SIBLING_CLL: u32 = 2;
    /// B's own promoted, currently-live channel-wide value -- the pre-bracket
    /// hardware baseline A's bracket must restore.
    const SIBLING_LIVE_BROADCAST_INTERVAL: u32 = 99;
    /// A's own (stale, per-CLL) `active` copy -- must NEVER be what hardware
    /// ends up holding once A's bracket completes.
    const A_STALE_ACTIVE_BROADCAST_INTERVAL: u32 = 77;
    /// A's Working snapshot's own value -- what the apply half of A's
    /// bracket temporarily pushes to hardware.
    const A_WORKING_BROADCAST_INTERVAL: u32 = 250;

    let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
    let device_id = api.open(None).expect("mock open");
    let channel_id = api
        .connect(device_id, j2534_0404::PROTOCOL_TP2_0_PS, 0, 500_000)
        .expect("mock connect");

    // B has already promoted its own value to hardware -- the channel's real,
    // currently-live state before A's bracket ever starts.
    api.set_config_u32(
        channel_id,
        j2534_0404::CONFIG_TP2_0_T_BR_INT,
        SIBLING_LIVE_BROADCAST_INTERVAL,
    )
    .expect("seeding the sibling's live hardware value should succeed");

    let mut logical_links = HashMap::new();
    logical_links.insert(
        RHTLAT_CLL,
        link_with_active(channel_id, A_STALE_ACTIVE_BROADCAST_INTERVAL),
    );
    logical_links.insert(
        SIBLING_CLL,
        link_with_active(channel_id, SIBLING_LIVE_BROADCAST_INTERVAL),
    );
    let logical_links = Arc::new(Mutex::new(logical_links));

    // A's own bracket: capture (before apply, under the same held `api`
    // guard every real call site uses) -> apply A's Working -> revert with
    // the captured restore threaded through.
    let channel_wide_restore =
        capture_channel_wide_hardware_locked(&api, channel_id, j2534_0404::PROTOCOL_TP2_0_PS).await;
    assert_eq!(
        channel_wide_restore,
        vec![(
            j2534_0404::CONFIG_TP2_0_T_BR_INT,
            SIBLING_LIVE_BROADCAST_INTERVAL
        )],
        "the capture must read the channel's real, currently-live hardware value (B's, 99), not \
         either CLL's own per-CLL active"
    );

    let mut a_effective = ComParamSet::default();
    a_effective
        .unum32
        .insert(PARAM_TP20_BROADCAST_INTERVAL, A_WORKING_BROADCAST_INTERVAL);
    let applied = apply_params_to_hardware_locked(
        &api,
        channel_id,
        j2534_0404::PROTOCOL_TP2_0_PS,
        &a_effective,
    )
    .await;
    assert!(applied, "A's own temp apply should succeed");

    revert_hardware_to_live_active_locked(
        &api,
        &logical_links,
        RHTLAT_CLL,
        channel_id,
        j2534_0404::PROTOCOL_TP2_0_PS,
        &channel_wide_restore,
    )
    .await;

    assert_eq!(
        config_value(&lib_path, channel_id.0, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        SIBLING_LIVE_BROADCAST_INTERVAL,
        "hardware must end up back on B's real, pre-bracket live value (99), never A's own \
         stale per-CLL active copy (77) -- this is the sibling-CLL clobber bug this fix closes"
    );
}
