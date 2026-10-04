use super::*;
use crate::service::ChannelProtocol;

const CHANNEL: ChannelId = ChannelId(1);
const PRESENT_CLL: u32 = 1;
const ABSENT_CLL: u32 = 2;

/// An empty `primitives` map -- every test above this file's own
/// `cop_tags`-focused tests (below) only exercises `build_cll_rx_entries`'s
/// routing-table/snapshot behavior, not `CllRxEntry::cop_tags` itself (ADR-204
/// Codex review, PR #116, round 3).
fn no_cop_tags() -> Arc<Mutex<HashMap<u32, CopEntry>>> {
    Arc::new(Mutex::new(HashMap::new()))
}

/// A `LogicalLinkState` with every field at its `CreateComLogicalLink`
/// default, on `CHANNEL` as its primary channel -- mirrors
/// `registrant_lifecycle_tests::minimal_link`.
fn minimal_link() -> LogicalLinkState {
    LogicalLinkState {
        channel_id: Some(CHANNEL),
        protocol: ChannelProtocol::CAN,
        hw_protocol_id: 0,
        software_isotp: false,
        uudt_channel_id: None,
        uudt_channel_key: None,
        isotp_rx: Arc::new(Mutex::new(HashMap::new())),
        connect_in_flight: std::sync::Weak::new(),
        connected: true,
        comm_started: true,
        raw_mode: false,
        checksum_mode: false,
        connect_generation: 7,
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
        error_set_seq: 42, // deliberately different from the snapshot value below
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

/// A CLL present in the pre-read snapshot must be stamped `Some(the
/// snapshotted value)` -- NOT the live `error_set_seq` (`42` in
/// `minimal_link`), proving this function reads the snapshot, not live
/// state, per the sixth amendment.
#[tokio::test]
async fn present_cll_is_stamped_from_the_snapshot_not_live_state() {
    let logical_links = Arc::new(Mutex::new(HashMap::from([(PRESENT_CLL, minimal_link())])));
    let snapshot: HashMap<u32, u64> = HashMap::from([(PRESENT_CLL, 7)]);

    let entries = build_cll_rx_entries(
        CHANNEL,
        &logical_links,
        &no_cop_tags(),
        &snapshot,
        CanChannelMode::SingleChannel,
    )
    .await;

    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].set_seq_at_read,
        Some(7),
        "must stamp from the pre-read snapshot, not live error_set_seq (42)"
    );
}

/// A CLL absent from the pre-read snapshot (connected in the narrow
/// window between the snapshot and this function running) must be
/// stamped `None` -- the apply gate then always discards a `Positive`
/// classification for it this pass (ADR-147 sixth amendment item 3).
#[tokio::test]
async fn absent_cll_is_stamped_none() {
    let logical_links = Arc::new(Mutex::new(HashMap::from([(ABSENT_CLL, minimal_link())])));
    let snapshot: HashMap<u32, u64> = HashMap::new(); // ABSENT_CLL not present

    let entries = build_cll_rx_entries(
        CHANNEL,
        &logical_links,
        &no_cop_tags(),
        &snapshot,
        CanChannelMode::SingleChannel,
    )
    .await;

    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].set_seq_at_read, None,
        "a CLL absent from the pre-read snapshot must stamp None, never a live fallback"
    );
}

/// ADR-217 Codex-review fix, PR #132 round 2: a qualified (pin-selected)
/// ISO15765 link on a native-mixed-family-configured module must get
/// `native_mixed: false` -- previously missing here (`rpc_link.rs`'s own
/// native-mixed call sites all exclude a qualified link via
/// `link_pin_select.is_none() && link_channel_index.is_none()`, but this
/// field's own computation lacked the same term). Harmless before round 2
/// (a qualified link's frames are always ISO15765-tagged, and the only
/// reader that cared about `native_mixed` was the raw-CAN-tagged first arm,
/// which never fires for one anyway), but round 2's new `uudt_eligible =
/// !native_mixed` gate on the SECOND (ISO-tagged) arm makes the field
/// load-bearing for a qualified link too -- see `RxEntryKind::Hardware`'s
/// own doc comment.
#[tokio::test]
async fn qualified_link_on_a_native_mixed_module_gets_native_mixed_false() {
    let link = LogicalLinkState {
        hw_protocol_id: j2534_0404::ISO15765,
        pin_select: Some(3),
        ..minimal_link()
    };
    let logical_links = Arc::new(Mutex::new(HashMap::from([(PRESENT_CLL, link)])));
    let snapshot: HashMap<u32, u64> = HashMap::from([(PRESENT_CLL, 7)]);

    let entries = build_cll_rx_entries(
        CHANNEL,
        &logical_links,
        &no_cop_tags(),
        &snapshot,
        CanChannelMode::NativeMixed,
    )
    .await;

    assert_eq!(entries.len(), 1);
    assert!(
        matches!(
            entries[0].kind,
            RxEntryKind::Hardware {
                native_mixed: false,
                ..
            }
        ),
        "a pin-selected (qualified) link never gets clause 8 SET_CONFIG'd or a PASS_FILTER at \
         connect time, even on a native-mixed-family-configured module"
    );
}

/// The unqualified counterpart to the test above: otherwise-identical link,
/// no `pin_select`/`channel_index`, on a native-mixed-family-configured
/// module -- gets `native_mixed: true`, proving the new `!qualified` term
/// doesn't just always return `false`.
#[tokio::test]
async fn unqualified_link_on_a_native_mixed_module_gets_native_mixed_true() {
    let link = LogicalLinkState {
        hw_protocol_id: j2534_0404::ISO15765,
        ..minimal_link() // pin_select: None, channel_index: None
    };
    let logical_links = Arc::new(Mutex::new(HashMap::from([(PRESENT_CLL, link)])));
    let snapshot: HashMap<u32, u64> = HashMap::from([(PRESENT_CLL, 7)]);

    let entries = build_cll_rx_entries(
        CHANNEL,
        &logical_links,
        &no_cop_tags(),
        &snapshot,
        CanChannelMode::NativeMixed,
    )
    .await;

    assert_eq!(entries.len(), 1);
    assert!(matches!(
        entries[0].kind,
        RxEntryKind::Hardware {
            native_mixed: true,
            ..
        }
    ));
}

/// ADR-184: an `active_unique_resp_id_table` entry keyed ONLY by
/// `CP_J1939SourceAddress` (no `CP_CanRespUSDTId`/`CP_CanRespUUDTId`) now
/// survives `build_cll_rx_entries`'s own CAN-ID-routable filter and carries
/// its SA through into `UniqueRespIdKey::j1939_source_address` -- the
/// companion fix to the pre-existing "filter these out entirely" comment
/// (PR #72 round 1): that filter widened to admit a THIRD CAN-ID-routable
/// field instead of continuing to treat a J1939-only entry as
/// CAN-ID-routeless.
#[tokio::test]
async fn j1939_source_address_only_entry_survives_the_filter() {
    let mut params = ComParamSet::default();
    params.unum32.insert(PARAM_J1939_SOURCE_ADDRESS, 0x80);
    let link = LogicalLinkState {
        active_unique_resp_id_table: vec![EcuUniqueRespEntry {
            unique_resp_identifier: 9,
            params,
        }],
        ..minimal_link()
    };
    let logical_links = Arc::new(Mutex::new(HashMap::from([(PRESENT_CLL, link)])));
    let snapshot: HashMap<u32, u64> = HashMap::from([(PRESENT_CLL, 7)]);

    let entries = build_cll_rx_entries(
        CHANNEL,
        &logical_links,
        &no_cop_tags(),
        &snapshot,
        CanChannelMode::SingleChannel,
    )
    .await;

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].unique_resp_ids.len(), 1);
    let key = &entries[0].unique_resp_ids[0];
    assert_eq!(key.unique_resp_identifier, 9);
    assert_eq!(key.can_resp_usdt_id, None);
    assert_eq!(key.can_resp_uudt_id, None);
    assert_eq!(key.j1939_source_address, Some(0x80));
}

/// ADR-203: an `active_unique_resp_id_table` entry keyed ONLY by
/// `CP_EcuRespSourceAddress` on a KWP (ISO14230) CLL now survives
/// `build_cll_rx_entries`'s filter and carries its address through into
/// `UniqueRespIdKey::ecu_resp_source_addr`.
#[tokio::test]
async fn ecu_resp_source_addr_only_entry_survives_the_filter_for_a_kwp_cll() {
    let mut params = ComParamSet::default();
    params.unum32.insert(PARAM_ECU_RESP_SOURCE_ADDR, 0x10);
    let link = LogicalLinkState {
        hw_protocol_id: j2534_0404::ISO14230,
        active_unique_resp_id_table: vec![EcuUniqueRespEntry {
            unique_resp_identifier: 9,
            params,
        }],
        ..minimal_link()
    };
    let logical_links = Arc::new(Mutex::new(HashMap::from([(PRESENT_CLL, link)])));
    let snapshot: HashMap<u32, u64> = HashMap::from([(PRESENT_CLL, 7)]);

    let entries = build_cll_rx_entries(
        CHANNEL,
        &logical_links,
        &no_cop_tags(),
        &snapshot,
        CanChannelMode::SingleChannel,
    )
    .await;

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].unique_resp_ids.len(), 1);
    let key = &entries[0].unique_resp_ids[0];
    assert_eq!(key.unique_resp_identifier, 9);
    assert_eq!(key.can_resp_usdt_id, None);
    assert_eq!(key.can_resp_uudt_id, None);
    assert_eq!(key.j1939_source_address, None);
    assert_eq!(key.ecu_resp_source_addr, Some(0x10));
}

/// ADR-203, the critical regression test: `CP_EcuRespSourceAddress` is ALSO
/// a legal, deliberately inert SURPLUS param on a CAN `CAN_UNIQUE_ID_UNUM32`
/// entry (documented in `docs/implementation-notes.md`'s 2022-edition-delta-
/// audit section). A CAN CLL with an entry keyed ONLY by
/// `CP_EcuRespSourceAddress` (no USDT/UUDT/J1939SA) must still be filtered
/// out entirely -- proving `build_cll_rx_entries`'s new retention gate
/// (`sa_routable`) stays scoped to KWP/J1850 CLLs only, never applied
/// unconditionally. Without this gate, this entry would survive but be
/// unmatchable by any of CAN's own routing tiers, silently blackholing
/// every frame for this CLL instead of the correct wildcard-0 fallback --
/// the exact PR #72 round-1 bug class this same filter's own comment
/// (above, in `events_rx_routing.rs`) already describes.
#[tokio::test]
async fn ecu_resp_source_addr_only_surplus_entry_is_filtered_out_for_a_can_cll() {
    let mut params = ComParamSet::default();
    params.unum32.insert(PARAM_ECU_RESP_SOURCE_ADDR, 0x10);
    let link = LogicalLinkState {
        active_unique_resp_id_table: vec![EcuUniqueRespEntry {
            unique_resp_identifier: 9,
            params,
        }],
        ..minimal_link() // protocol: CAN, hw_protocol_id: 0 (not KWP/J1850)
    };
    let logical_links = Arc::new(Mutex::new(HashMap::from([(PRESENT_CLL, link)])));
    let snapshot: HashMap<u32, u64> = HashMap::from([(PRESENT_CLL, 7)]);

    let entries = build_cll_rx_entries(
        CHANNEL,
        &logical_links,
        &no_cop_tags(),
        &snapshot,
        CanChannelMode::SingleChannel,
    )
    .await;

    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].unique_resp_ids.is_empty(),
        "an SA-only surplus entry on a CAN CLL must be filtered out \
         entirely, not retained as an unmatchable entry"
    );
    // Confirm the end-to-end effect: route_frame wildcards every frame for
    // this CLL, exactly as it did before this ADR.
    assert_eq!(
        route_frame(&entries[0], Some(0x7E8), None, true, false),
        Some(0)
    );
    assert_eq!(route_frame(&entries[0], None, None, true, false), Some(0));
}

/// A minimal `LogicalLinkState` for a TP2.0 CLL (`hw_protocol_id` set to the
/// native `PROTOCOL_TP2_0_PS` id, so `resources::is_tp2_0_protocol_id`
/// recognizes it) -- otherwise identical to `minimal_link`.
fn minimal_tp20_link(tp20_connection: Option<Tp20Connection>) -> LogicalLinkState {
    LogicalLinkState {
        protocol: ChannelProtocol::TP2_0_PS,
        hw_protocol_id: j2534_0404::PROTOCOL_TP2_0_PS,
        tp20_connection,
        ..minimal_link()
    }
}

/// Codex review regression (PR #97, ADR-188 Fix B): a TP2.0 CLL whose
/// connection has reached `Established` gets the real routing entry
/// (`tp20_rx_id: Some(requested_rx_id)`), unchanged from before this fix --
/// and (Codex review, PR #97 round 9) `tp20_tx_id: Some(established_tx_id)`,
/// so a `CP_Loopback`-enabled write's own device-generated echo (addressed
/// by our TX-ID, not our RX-ID) routes back to this same CLL instead of
/// being dropped or misdelivered to a sibling.
#[tokio::test]
async fn tp20_established_cll_gets_its_real_rx_id_and_tx_id_entry() {
    let link = minimal_tp20_link(Some(Tp20Connection {
        requested_rx_id: 0x0321,
        established_tx_id: Some(0x1000_0321),
        phase: Tp20ConnectionPhase::Established,
        passive: false,
    }));
    let logical_links = Arc::new(Mutex::new(HashMap::from([(PRESENT_CLL, link)])));
    let snapshot: HashMap<u32, u64> = HashMap::from([(PRESENT_CLL, 7)]);

    let entries = build_cll_rx_entries(
        CHANNEL,
        &logical_links,
        &no_cop_tags(),
        &snapshot,
        CanChannelMode::SingleChannel,
    )
    .await;

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].unique_resp_ids.len(), 1);
    let key = &entries[0].unique_resp_ids[0];
    assert_eq!(key.tp20_rx_id, Some(0x0321));
    assert_eq!(key.tp20_tx_id, Some(0x1000_0321));
    assert_eq!(key.can_resp_usdt_id, None);
    assert_eq!(key.can_resp_uudt_id, None);
    assert_eq!(key.j1939_source_address, None);
}

/// Codex review regression (PR #97, ADR-188 Fix B): a TP2.0 CLL whose
/// connection is NOT `Established` (`Requested`, `Lost`, or no
/// `tp20_connection` at all) must still get exactly one `unique_resp_ids`
/// entry -- a deliberately UNMATCHABLE sentinel (every field `None`) -- so
/// `route_frame`'s empty-table wildcard fallback ("no table configured,
/// deliver every frame unconditionally") can never apply to it. Before this
/// fix, `unique_resp_ids` stayed empty for exactly this case, wildcard-
/// delivering every frame on the shared physical channel to it, including
/// frames addressed to an established SIBLING CLL's own connection -- a
/// cross-CLL data leak (see this codebase's own regression note in
/// `tests/grpc_mock/tp20.rs`'s
/// `non_established_sibling_does_not_disrupt_an_established_peers_delivery`
/// for why an end-to-end proof of the leak itself isn't constructible via
/// that harness; this unit test is the actual proof of the fix).
#[tokio::test]
async fn tp20_non_established_cll_gets_an_unmatchable_sentinel_entry_not_an_empty_table() {
    for connection in [
        None,
        Some(Tp20Connection {
            requested_rx_id: 0x0321,
            established_tx_id: None,
            phase: Tp20ConnectionPhase::Requested,
            passive: false,
        }),
        Some(Tp20Connection {
            requested_rx_id: 0x0321,
            established_tx_id: None,
            phase: Tp20ConnectionPhase::Lost,
            passive: false,
        }),
    ] {
        let link = minimal_tp20_link(connection);
        let logical_links = Arc::new(Mutex::new(HashMap::from([(PRESENT_CLL, link)])));
        let snapshot: HashMap<u32, u64> = HashMap::from([(PRESENT_CLL, 7)]);

        let entries = build_cll_rx_entries(
            CHANNEL,
            &logical_links,
            &no_cop_tags(),
            &snapshot,
            CanChannelMode::SingleChannel,
        )
        .await;

        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].unique_resp_ids.len(),
            1,
            "must never be empty for a TP2.0 CLL (connection={connection:?}) -- an empty table \
             falls into route_frame's wildcard-delivery fallback"
        );
        let key = &entries[0].unique_resp_ids[0];
        assert_eq!(key.tp20_rx_id, None, "connection={connection:?}");
        assert_eq!(key.tp20_tx_id, None, "connection={connection:?}");
        assert_eq!(key.can_resp_usdt_id, None, "connection={connection:?}");
        assert_eq!(key.can_resp_uudt_id, None, "connection={connection:?}");
        assert_eq!(key.j1939_source_address, None, "connection={connection:?}");
    }
}

/// A minimal `ActiveSendReceive` registrant carrying only `cop_handle` --
/// every other field is a benign placeholder these `cop_tags`-focused tests
/// never read (mirrors `events_bind_frame_tests.rs`'s own `registrant`
/// helper, duplicated here rather than shared since that helper is private
/// to its own test module).
fn minimal_registrant(cop_handle: u32) -> CopRegistrant {
    CopRegistrant {
        cop_handle,
        registration_seq: 0,
        tier: RegistrantTier::ActiveSendReceive,
        expected: Vec::new(),
        rc_cfg: None,
        request_sid: None,
        matches_needed: Some(1),
        matches_got: 0,
        pending_rc: None,
        connect_generation: 7,
        cyclic_deadline: None,
        cyclic_timeout_ms: None,
        migrate_on_first_match: false,
        timing_cfg: None,
        timing_accumulator: None,
        pending_timing_change: None,
        concat_enabled: false,
        concat: Vec::new(),
        concat_segments_got: 0,
    }
}

const COP_TAG_TEST_COP_HANDLE: u32 = 100;

/// A `primitives` map with one live `CopEntry` for `COP_TAG_TEST_COP_HANDLE`,
/// carrying `tag`.
fn primitives_with_tag(tag: Vec<u8>) -> Arc<Mutex<HashMap<u32, CopEntry>>> {
    Arc::new(Mutex::new(HashMap::from([(
        COP_TAG_TEST_COP_HANDLE,
        CopEntry {
            cll_handle: PRESENT_CLL,
            dispatched: true,
            transmits: false,
            is_send_recv: true,
            cop_tag: Some(tag),
        },
    )])))
}

/// ADR-204 Codex review (PR #116, round 3): `CllRxEntry::cop_tags` captures
/// this registrant's `cop_tag` from `primitives` in the SAME
/// `build_cll_rx_entries` critical section that clones `registrants` --
/// proves the plain "still live at snapshot time" case works before the
/// next test proves the actual race fix.
#[tokio::test]
async fn cop_tags_captures_the_live_tag_at_snapshot_time() {
    let tag = b"snapshot-tag".to_vec();
    let link = LogicalLinkState {
        registrants: vec![minimal_registrant(COP_TAG_TEST_COP_HANDLE)],
        ..minimal_link()
    };
    let logical_links = Arc::new(Mutex::new(HashMap::from([(PRESENT_CLL, link)])));
    let primitives = primitives_with_tag(tag.clone());
    let snapshot: HashMap<u32, u64> = HashMap::from([(PRESENT_CLL, 7)]);

    let entries = build_cll_rx_entries(
        CHANNEL,
        &logical_links,
        &primitives,
        &snapshot,
        CanChannelMode::SingleChannel,
    )
    .await;

    assert_eq!(entries.len(), 1);
    assert_eq!(
        resolve_frame_cop_tag(&entries[0], Some(COP_TAG_TEST_COP_HANDLE)),
        Some(tag)
    );
    assert_eq!(
        resolve_frame_cop_tag(&entries[0], None),
        None,
        "an unbound frame (no cop_handle) never carries a tag"
    );
    assert_eq!(
        resolve_frame_cop_tag(&entries[0], Some(999)),
        None,
        "a cop_handle absent from the snapshot resolves to no tag"
    );
}

/// The genuinely discriminating regression test for Codex's PR #116 round-3
/// finding: `frame_cop_handle`'s eventual `cop_tag` must survive a
/// concurrent `CancelComPrimitive`/link-teardown path removing this COP's
/// `primitives` entry AFTER `build_cll_rx_entries` already captured
/// `cop_tags` alongside `registrants` -- the exact gap the pre-fix code left
/// open between this snapshot (where the registrant match that eventually
/// produces this `cop_handle` is decided from) and its own separately-
/// locked, much-later `resolve_cop_tag(&ctx.primitives, ...)` call at actual
/// frame-construction time.
///
/// Unlike `events_reap_expired_cyclic_registrants_cop_tag_tests.rs`'s
/// analogous fix (21fdb4e/77d9c38), no `tokio::sync::Mutex` FIFO-fencing is
/// needed to force this interleaving deterministically: the whole point of
/// this fix is that `resolve_frame_cop_tag` is a plain, synchronous, lock-
/// free read of `entry.cop_tags` (`CllRxEntry::cop_tags`'s own doc comment)
/// -- there is no remaining `.await` between the snapshot and the
/// resolution for a concurrent removal to race against, so the removal can
/// simply be sequenced directly between the two calls below and the
/// interleaving is exact every time, not merely probable.
///
/// **Fail-without/pass-with, verified directly against this fix (same
/// session)**: temporarily reverted `resolve_frame_cop_tag` to
/// unconditionally `None` (mimicking a fresh `primitives` lookup performed
/// AFTER the removal below, i.e. exactly the pre-fix `resolve_cop_tag(&ctx.
/// primitives, frame_cop_handle).await` call site's real-world outcome in
/// this exact race window) -- against that reversion this test FAILS
/// (`resolve_frame_cop_tag` returns `None`, not `Some(tag)`). Separately,
/// temporarily reverted `build_cll_rx_entries`'s `cop_tags` capture to
/// always produce an empty map (simulating the pre-fix code, which never
/// read `primitives` in this function at all) -- against that reversion
/// this test ALSO fails, for the same reason. Restoring either fix in
/// isolation restores a pass; both fixes together are what is committed.
#[tokio::test]
async fn cop_tags_snapshot_survives_a_primitives_removal_racing_the_later_resolution() {
    let tag = b"race-token".to_vec();
    let link = LogicalLinkState {
        registrants: vec![minimal_registrant(COP_TAG_TEST_COP_HANDLE)],
        ..minimal_link()
    };
    let logical_links = Arc::new(Mutex::new(HashMap::from([(PRESENT_CLL, link)])));
    let primitives = primitives_with_tag(tag.clone());
    let snapshot: HashMap<u32, u64> = HashMap::from([(PRESENT_CLL, 7)]);

    // Step 1: `build_cll_rx_entries` snapshots `registrants` and `cop_tags`
    // together, while the COP is still live -- mirrors the real call site
    // (`poll_rx_inner`), which runs this well before `bind_frame` ever
    // decides this frame's own `cop_handle`.
    let entries = build_cll_rx_entries(
        CHANNEL,
        &logical_links,
        &primitives,
        &snapshot,
        CanChannelMode::SingleChannel,
    )
    .await;
    assert_eq!(entries.len(), 1);

    // Step 2: a concurrent `CancelComPrimitive`/link-teardown path removes
    // this COP's `primitives` entry -- landing strictly AFTER the snapshot
    // above, exactly the gap Codex's review identified between the
    // registrant-match decision and the (pre-fix) later resolution.
    primitives.lock().await.remove(&COP_TAG_TEST_COP_HANDLE);

    // Step 3: the real per-frame resolution call (`poll_rx_inner`'s
    // `resolve_frame_cop_tag(entry, frame_cop_handle)` call site) must still
    // recover the tag from the snapshot, not the now-empty `primitives`.
    assert_eq!(
        resolve_frame_cop_tag(&entries[0], Some(COP_TAG_TEST_COP_HANDLE)),
        Some(tag),
        "the tag captured at snapshot time must survive a concurrent primitives removal \
         landing after the snapshot but before this frame's own resolution"
    );
}
