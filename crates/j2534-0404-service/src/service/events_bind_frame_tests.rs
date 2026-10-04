use super::*;

/// A tier-1 `ActiveSendReceive` registrant with no RC handling, one
/// vacuous (empty mask/pattern, match-anything) descriptor -- a real
/// vacuous COP always carries at least one `ExpectedResponse` entry (an
/// empty `expected_response_array` element), never an empty `expected`
/// vec -- and `matches_needed: Some(1)`, ready for per-test
/// customization via struct-update syntax.
fn registrant(cop_handle: u32, connect_generation: u64) -> CopRegistrant {
    CopRegistrant {
        cop_handle,
        registration_seq: 0,
        tier: RegistrantTier::ActiveSendReceive,
        expected: vec![ExpectedResponse {
            mask: Vec::new(),
            pattern: Vec::new(),
            unique_resp_ids: Vec::new(),
            acceptance_id: 0,
        }],
        rc_cfg: None,
        request_sid: None,
        matches_needed: Some(1),
        matches_got: 0,
        pending_rc: None,
        connect_generation,
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

/// A non-vacuous `ExpectedResponse` matching any payload starting with
/// `prefix`.
fn non_vacuous_expected(prefix: u8, acceptance_id: u32) -> ExpectedResponse {
    ExpectedResponse {
        mask: vec![0xFF],
        pattern: vec![prefix],
        unique_resp_ids: Vec::new(),
        acceptance_id,
    }
}

/// A fully-empty (`is_vacuous() == true`) `ExpectedResponse` matching any
/// payload -- for building a MIXED registrant alongside
/// `non_vacuous_expected` (ADR-100 Decision §3's round-5 addendum: a
/// registrant can carry both a specific and an empty descriptor at once).
fn vacuous_expected(acceptance_id: u32) -> ExpectedResponse {
    ExpectedResponse {
        mask: Vec::new(),
        pattern: Vec::new(),
        unique_resp_ids: Vec::new(),
        acceptance_id,
    }
}

/// A minimal `CllRxEntry` carrying only what `bind_frame` reads:
/// `connect_generation`, `registrants`, `tester_present_discard`. Every
/// other field is a benign placeholder `bind_frame` never touches --
/// including `registrant_baselines`, which only the writeback merge
/// (ADR-101), not `bind_frame`, ever reads.
fn test_entry(
    connect_generation: u64,
    registrants: Vec<CopRegistrant>,
    tester_present_discard: Vec<TesterPresentDiscard>,
) -> CllRxEntry {
    CllRxEntry {
        handle: 1,
        rx_buf: Arc::new(Mutex::new(CllEventQueue::default())),
        unique_resp_ids: Vec::new(),
        kind: RxEntryKind::Hardware {
            native_mixed: false,
            uudt_on_companion: false,
        },
        header_protocol: 0,
        raw_mode: false,
        usdt_addressing_by_id: Vec::new(),
        uudt_addressing_by_id: Vec::new(),
        connect_generation,
        suspend_seq: None,
        set_seq_at_read: Some(0),
        registrants,
        registrant_baselines: Vec::new(),
        tester_present_discard,
        queue_error_class: None,
        start_msg_ind_enable: false,
        transmit_ind_enable: false,
        cop_tags: HashMap::new(),
    }
}

/// A `UniqueRespIdKey` with only the CAN USDT/UUDT fields set --
/// `j1939_source_address: None` -- the shape every pre-ADR-184 test in this
/// file needs.
fn can_key(unique_resp_identifier: u32, usdt: Option<u32>, uudt: Option<u32>) -> UniqueRespIdKey {
    UniqueRespIdKey {
        unique_resp_identifier,
        can_resp_usdt_id: usdt,
        usdt_width_gate: CanIdWidthGate::Any,
        can_resp_uudt_id: uudt,
        uudt_width_gate: CanIdWidthGate::Any,
        j1939_source_address: None,
        tp20_rx_id: None,
        tp20_tx_id: None,
        ecu_resp_source_addr: None,
    }
}

/// ADR-150: `route_frame_matched_uudt` reports `true` only for a CAN ID
/// that resolves via the entry's `CP_CanRespUUDTId`, never via
/// `CP_CanRespUSDTId` -- even when a DIFFERENT entry in the same table
/// would have matched via USDT for a different CAN ID.
#[test]
fn route_frame_matched_uudt_true_only_for_a_uudt_id_match() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![can_key(1, Some(0x7E8), Some(0x7DF))],
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert!(
        route_frame_matched_uudt(&entry, Some(0x7DF), false),
        "0x7DF only matches this entry's own CP_CanRespUUDTId"
    );
    assert!(
        !route_frame_matched_uudt(&entry, Some(0x7E8), false),
        "0x7E8 matches CP_CanRespUSDTId -- not UUDT-routed"
    );
    assert!(
        !route_frame_matched_uudt(&entry, Some(0x1234), false),
        "an unrecognized CAN ID matches neither -- not UUDT-routed"
    );
}

/// The no-table wildcard (empty `unique_resp_ids`) and a too-short frame
/// (no CAN ID) both default to "not UUDT-routed" -- neither carries
/// enough information to tell USDT from UUDT, and `route_frame` itself
/// delivers both unconditionally.
#[test]
fn route_frame_matched_uudt_false_for_no_table_and_no_can_id() {
    let no_table_entry = CllRxEntry {
        unique_resp_ids: Vec::new(),
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert!(!route_frame_matched_uudt(
        &no_table_entry,
        Some(0x7DF),
        false
    ));

    let entry = CllRxEntry {
        unique_resp_ids: vec![can_key(1, Some(0x7E8), Some(0x7DF))],
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert!(!route_frame_matched_uudt(&entry, None, false));
}

/// ADR-217 Codex-review fix, PR #132 round 2: `route_frame_with_role`'s own
/// `uudt_eligible` gate, direct unit coverage (the integration-level
/// coverage lives in `tests/grpc_mock/mixed_format_can.rs`'s
/// `native_mixed_all_frames_mode_serves_a_cross_cll_uudt_usdt_collision` and
/// `can_mode.rs`'s ADR-041 regression -- these pin the exact `matched()`
/// precedence/skip behavior these higher-level tests exercise indirectly).
mod route_frame_with_role_tests {
    use super::*;

    /// The core Codex finding, isolated to `matched()`'s own precedence: two
    /// DIFFERENT keys on the same CLL, one with only `CP_CanRespUUDTId = X`
    /// (uid 1), one with only `CP_CanRespUSDTId = X` (uid 2) -- an ISO-tagged
    /// frame for X with `uudt_eligible: false` must skip key 1's UUDT tier
    /// entirely and resolve via key 2's USDT tier, reporting `uudt_routed:
    /// false`. This is the exact scenario a naive fix (gating `route_frame`
    /// alone, leaving `route_frame_matched_uudt` to compute independently)
    /// would get wrong -- reverting `uudt_eligible` to unconditional `true`
    /// here reproduces the pre-fix `(1, true)` misattribution.
    #[test]
    fn skips_uudt_tier_on_one_key_to_resolve_usdt_on_another() {
        let entry = CllRxEntry {
            unique_resp_ids: vec![can_key(1, None, Some(0x5E8)), can_key(2, Some(0x5E8), None)],
            ..test_entry(1, Vec::new(), Vec::new())
        };
        assert_eq!(
            route_frame_with_role(&entry, Some(0x5E8), None, true, false, false, false),
            Some((2, false)),
            "uudt_eligible: false skips key 1's UUDT tier, resolving via key 2's USDT tier"
        );
    }

    /// The ADR-041 single-channel workaround this fix must not regress:
    /// `uudt_eligible: true` (every pre-ADR-217 caller) still lets an
    /// ISO-tagged frame resolve via a UUDT-only key, exactly as before.
    #[test]
    fn uudt_eligible_true_still_matches_a_uudt_only_key() {
        let entry = CllRxEntry {
            unique_resp_ids: vec![can_key(1, None, Some(0x5E8))],
            ..test_entry(1, Vec::new(), Vec::new())
        };
        assert_eq!(
            route_frame_with_role(&entry, Some(0x5E8), None, true, false, true, false),
            Some((1, true)),
            "ADR-041's own single-channel FLOW_CONTROL_FILTER workaround relies on this"
        );
    }

    /// `uudt_eligible: false` against a UUDT-only key (no USDT, no other
    /// tier on that key) correctly drops the frame -- there is nothing else
    /// for it to resolve to, matching a native-mixed UUDT-only entry's own
    /// dedicated raw-CAN-tagged delivery path being the only legitimate one.
    #[test]
    fn uudt_eligible_false_drops_a_uudt_only_key_with_no_other_tier() {
        let entry = CllRxEntry {
            unique_resp_ids: vec![can_key(1, None, Some(0x5E8))],
            ..test_entry(1, Vec::new(), Vec::new())
        };
        assert_eq!(
            route_frame_with_role(&entry, Some(0x5E8), None, true, false, false, false),
            None
        );
    }

    /// Self-collision (ADR-217's own single-CLL case, ADR-162 Decision 2
    /// rejects this under `NativeMixed`/`ON` but `ALL_FRAMES` accepts it):
    /// ONE key with BOTH `CP_CanRespUSDTId = X` and `CP_CanRespUUDTId = X`.
    /// `matched()`'s own USDT-before-UUDT precedence means the ISO-tagged
    /// delivery resolves via USDT even with `uudt_eligible: false` -- the
    /// gate only ever matters when it would otherwise change which tier
    /// wins, and USDT already wins here regardless.
    #[test]
    fn self_collision_key_resolves_via_usdt_regardless_of_uudt_eligible() {
        let entry = CllRxEntry {
            unique_resp_ids: vec![can_key(1, Some(0x5E8), Some(0x5E8))],
            ..test_entry(1, Vec::new(), Vec::new())
        };
        assert_eq!(
            route_frame_with_role(&entry, Some(0x5E8), None, true, false, false, false),
            Some((1, false))
        );
        assert_eq!(
            route_frame_with_role(&entry, Some(0x5E8), None, true, false, true, false),
            Some((1, false)),
            "USDT tier wins precedence over UUDT on the same key either way"
        );
    }
}

/// ADR-184: a `CP_J1939SourceAddress`-only entry (both CAN fields `None`)
/// routes a frame whose CAN ID's low byte matches, and drops one whose low
/// byte does not -- `route_frame`'s new third matching tier.
#[test]
fn route_frame_matches_j1939_source_address() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![UniqueRespIdKey {
            unique_resp_identifier: 7,
            can_resp_usdt_id: None,
            usdt_width_gate: CanIdWidthGate::Any,
            can_resp_uudt_id: None,
            uudt_width_gate: CanIdWidthGate::Any,
            j1939_source_address: Some(0x80),
            tp20_rx_id: None,
            tp20_tx_id: None,
            ecu_resp_source_addr: None,
        }],
        ..test_entry(1, Vec::new(), Vec::new())
    };
    // 29-bit J1939 CAN id ...NNNN_1000_0000 -- low byte (source address) is
    // 0x80, matching the configured entry.
    assert_eq!(
        route_frame(&entry, Some(0x18DA_2180), None, true, false),
        Some(7)
    );
    // A different source address in the low byte does not match -- dropped.
    assert_eq!(
        route_frame(&entry, Some(0x18DA_2181), None, true, false),
        None
    );
}

/// Codex review fix (PR #97 round 9, `is_tx_side`-gated per round 10): a
/// TP2.0 entry's `tp20_tx_id` routes a TX-side frame (a `CP_Loopback`-
/// enabled write's own device-generated echo, `is_tx_side: true`) carrying
/// the established TX-ID to this CLL, exactly like `tp20_rx_id` routes a
/// real inbound frame (`is_tx_side: false`) -- both tiers are live
/// simultaneously on the same entry, each independently matchable, but only
/// for the frame kind each one actually means.
#[test]
fn route_frame_matches_tp20_tx_id_as_well_as_rx_id() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![UniqueRespIdKey {
            unique_resp_identifier: 0,
            can_resp_usdt_id: None,
            usdt_width_gate: CanIdWidthGate::Any,
            can_resp_uudt_id: None,
            uudt_width_gate: CanIdWidthGate::Any,
            j1939_source_address: None,
            tp20_rx_id: Some(0x0321),
            tp20_tx_id: Some(0x1000_0321),
            ecu_resp_source_addr: None,
        }],
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(
        route_frame(&entry, Some(0x0321), None, true, false),
        Some(0),
        "a real inbound frame addressed to our RX-ID is routed"
    );
    assert_eq!(
        route_frame(&entry, Some(0x1000_0321), None, true, true),
        Some(0),
        "our own write's loopback echo, addressed by our TX-ID and flagged \
         is_tx_side, is routed too"
    );
    assert_eq!(
        route_frame(&entry, Some(0x1234), None, true, false),
        None,
        "an unrelated CAN ID matches neither and is dropped"
    );
}

/// Codex review fix (PR #97 round 10): the fresh finding against round 9's
/// own fix -- `tp20_tx_id` must NEVER match a frame that is not flagged
/// `is_tx_side`, even when its CAN ID happens to equal the configured
/// TX-ID. Without this gate, a GENUINE inbound frame addressed to a
/// DIFFERENT sibling connection's own RX-ID -- not a loopback echo at all --
/// would also match this entry whenever that sibling's RX-ID happened to
/// equal this connection's TX-ID, leaking the sibling's real traffic here
/// too (this test constructs exactly that collision: `tp20_tx_id ==
/// 0x0322`, standing in for a sibling's `tp20_rx_id`).
#[test]
fn route_frame_never_matches_tp20_tx_id_on_a_non_tx_side_frame() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![UniqueRespIdKey {
            unique_resp_identifier: 0,
            can_resp_usdt_id: None,
            usdt_width_gate: CanIdWidthGate::Any,
            can_resp_uudt_id: None,
            uudt_width_gate: CanIdWidthGate::Any,
            j1939_source_address: None,
            tp20_rx_id: Some(0x0321),
            tp20_tx_id: Some(0x0322),
            ecu_resp_source_addr: None,
        }],
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(
        route_frame(&entry, Some(0x0322), None, true, false),
        None,
        "a genuine (non-tx-side) inbound frame must never match via \
         tp20_tx_id, even on an exact CAN ID collision with a sibling's own \
         RX-ID"
    );
    assert_eq!(
        route_frame(&entry, Some(0x0322), None, true, true),
        Some(0),
        "the same CAN ID, correctly flagged is_tx_side, is our own echo and \
         is routed"
    );
}

/// Codex review fix (PR #97 round 12): the symmetric finding round 10's own
/// fix left open -- `tp20_rx_id` must NEVER match a TX-side frame, even
/// when its CAN ID happens to equal the configured RX-ID. Without this
/// gate, a TX-side echo of a DIFFERENT connection's own write -- not
/// genuine inbound content addressed to this CLL at all -- would also
/// match this entry whenever that other connection's TX-ID happened to
/// equal this CLL's own RX-ID, leaking that other connection's transmit
/// echo here too (this test constructs exactly that collision: `tp20_rx_id
/// == 0x1000_0321`, standing in for a different connection's own
/// established TX-ID).
#[test]
fn route_frame_never_matches_tp20_rx_id_on_a_tx_side_frame() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![UniqueRespIdKey {
            unique_resp_identifier: 0,
            can_resp_usdt_id: None,
            usdt_width_gate: CanIdWidthGate::Any,
            can_resp_uudt_id: None,
            uudt_width_gate: CanIdWidthGate::Any,
            j1939_source_address: None,
            tp20_rx_id: Some(0x1000_0321),
            tp20_tx_id: Some(0x0322),
            ecu_resp_source_addr: None,
        }],
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(
        route_frame(&entry, Some(0x1000_0321), None, true, true),
        None,
        "a TX-side frame must never match via tp20_rx_id, even on an exact \
         CAN ID collision with a different connection's own TX-ID"
    );
    assert_eq!(
        route_frame(&entry, Some(0x1000_0321), None, true, false),
        Some(0),
        "the same CAN ID, correctly flagged non-tx-side, is genuine inbound \
         content addressed to our RX-ID and is routed"
    );
}

// ADR-192/Phase 7 Stage 7c (Codex review round 2 fix): a TP2.0 broadcast
// frame's TX-side echo is no longer classified at this `route_frame`/
// `UniqueRespIdKey::matched` layer at all -- it is dropped much earlier, in
// `events.rs`'s `poll_rx_inner`, directly from the raw frame's first data
// byte, before the frame ever reaches per-CLL routing (see that function's
// own doc comment on the check, and `tp20.rs`'s
// `broadcast_echo_is_dropped_not_misrouted_to_a_sibling_established_connection`
// for the end-to-end coverage, including the minimum-size 3-byte case).
// The unit-level coincidence tests that used to live here exercised a
// per-entry classifier (`(can_id >> 24) as u8 >= 0xF0`) that only ran when
// `frame_can_id` was `Some` -- i.e. only for a >= 4-byte frame -- which
// left the legal 3-byte-minimum broadcast echo unclassified and leaking to
// every sibling CLL; that classifier has been removed as dead code now
// that the early drop makes it unreachable for every legal broadcast
// length (`3..=8`).

/// ADR-184: an SA-only match is never reported as UUDT-routed --
/// `route_frame_matched_uudt` returns `false` by construction
/// (`MatchKind::J1939Sa != MatchKind::Uudt`), not via a special case.
#[test]
fn route_frame_matched_uudt_false_for_a_j1939_sa_match() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![UniqueRespIdKey {
            unique_resp_identifier: 7,
            can_resp_usdt_id: None,
            usdt_width_gate: CanIdWidthGate::Any,
            can_resp_uudt_id: None,
            uudt_width_gate: CanIdWidthGate::Any,
            j1939_source_address: Some(0x80),
            tp20_rx_id: None,
            tp20_tx_id: None,
            ecu_resp_source_addr: None,
        }],
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert!(!route_frame_matched_uudt(&entry, Some(0x18DA_2180), false));
}

/// ADR-184: `route_frame_uudt_only` (the dual-channel-mode Companion path,
/// ADR-046) never matches a J1939 SA-only entry -- it keeps its own
/// `CP_CanRespUUDTId`-specific check, deliberately not routed through
/// `UniqueRespIdKey::matched`'s SA tier (J1939 has no UUDT/Companion
/// concept; see this function's own doc comment in `events_rx_routing.rs`).
#[test]
fn route_frame_uudt_only_ignores_a_j1939_sa_only_entry() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![UniqueRespIdKey {
            unique_resp_identifier: 7,
            can_resp_usdt_id: None,
            usdt_width_gate: CanIdWidthGate::Any,
            can_resp_uudt_id: None,
            uudt_width_gate: CanIdWidthGate::Any,
            j1939_source_address: Some(0x80),
            tp20_rx_id: None,
            tp20_tx_id: None,
            ecu_resp_source_addr: None,
        }],
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(
        route_frame_uudt_only(&entry, Some(0x18DA_2180), false),
        None
    );
}

/// A `UniqueRespIdKey` with only `ecu_resp_source_addr` set -- the shape a
/// KWP/J1850 `CP_EcuRespSourceAddress`-keyed table entry produces
/// (ADR-203).
fn sa_key(unique_resp_identifier: u32, ecu_resp_source_addr: u8) -> UniqueRespIdKey {
    UniqueRespIdKey {
        unique_resp_identifier,
        can_resp_usdt_id: None,
        usdt_width_gate: CanIdWidthGate::Any,
        can_resp_uudt_id: None,
        uudt_width_gate: CanIdWidthGate::Any,
        j1939_source_address: None,
        tp20_rx_id: None,
        tp20_tx_id: None,
        ecu_resp_source_addr: Some(u32::from(ecu_resp_source_addr)),
    }
}

/// ADR-203: a KWP (ISO14230) CLL with a `CP_EcuRespSourceAddress`-keyed
/// entry routes a frame whose parsed source byte matches to that entry's
/// own `unique_resp_identifier`, not the wildcard `0`.
#[test]
fn route_frame_matches_kwp_ecu_resp_source_address() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![sa_key(9, 0x10)],
        header_protocol: j2534_0404::ISO14230,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(route_frame(&entry, None, Some(0x10), true, false), Some(9));
}

/// ADR-203: same as above, for J1850 (VPW).
#[test]
fn route_frame_matches_j1850_ecu_resp_source_address() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![sa_key(11, 0x21)],
        header_protocol: j2534_0404::J1850VPW,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(route_frame(&entry, None, Some(0x21), true, false), Some(11));
}

/// ADR-203: a KWP/J1850 frame whose parsed source byte matches NO
/// configured entry is DROPPED (`route_frame` returns `None`), not
/// delivered at the wildcard `0` -- ISO 22900-2:2022 §8.4.28.7.3's
/// unmatched-response model (paraphrased): once a table is explicitly
/// configured, an unmatched response is dropped unless a `PDU_ID_UNDEF`
/// catch-all entry exists (a separate, pre-existing, protocol-wide gap not
/// fixed by this ADR).
#[test]
fn route_frame_drops_a_kwp_frame_whose_source_addr_matches_no_entry() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![sa_key(9, 0x10)],
        header_protocol: j2534_0404::ISO14230,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(route_frame(&entry, None, Some(0x99), true, false), None);
}

/// ADR-203: a source-less frame (`frame_source_addr: None`, e.g. an
/// unaddressed-format KWP frame with no target/source bytes at all) is
/// DROPPED for a table-mode KWP/J1850 CLL, not delivered at `0`.
#[test]
fn route_frame_drops_a_source_less_kwp_frame_when_a_table_is_configured() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![sa_key(9, 0x10)],
        header_protocol: j2534_0404::ISO9141,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(route_frame(&entry, None, None, true, false), None);
}

/// ADR-203: a KWP/J1850 CLL with NO SA-keyed entries at all (empty
/// `unique_resp_ids`) still delivers every frame at wildcard `0`,
/// unchanged from before this ADR -- the empty-table check runs before the
/// new protocol-gated branch.
#[test]
fn route_frame_kwp_j1850_no_table_still_wildcards() {
    let entry = CllRxEntry {
        unique_resp_ids: Vec::new(),
        header_protocol: j2534_0404::J1850PWM,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(route_frame(&entry, None, Some(0x10), true, false), Some(0));
    assert_eq!(route_frame(&entry, None, None, true, false), Some(0));
}

/// ADR-203: two sibling KWP/J1850 CLLs sharing one physical channel, each
/// with a distinct configured `CP_EcuRespSourceAddress`, each correctly
/// receive only frames matching their own configured address -- mirrors
/// `route_frame_matches_j1939_source_address`'s J1939 sibling-
/// disambiguation shape.
#[test]
fn route_frame_disambiguates_kwp_siblings_by_source_address() {
    let sibling_a = CllRxEntry {
        unique_resp_ids: vec![sa_key(1, 0x10)],
        header_protocol: j2534_0404::ISO14230,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    let sibling_b = CllRxEntry {
        unique_resp_ids: vec![sa_key(2, 0x20)],
        header_protocol: j2534_0404::ISO14230,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(
        route_frame(&sibling_a, None, Some(0x10), true, false),
        Some(1)
    );
    assert_eq!(
        route_frame(&sibling_a, None, Some(0x20), true, false),
        None,
        "sibling_a must not receive a frame addressed to sibling_b's ECU"
    );
    assert_eq!(
        route_frame(&sibling_b, None, Some(0x20), true, false),
        Some(2)
    );
    assert_eq!(
        route_frame(&sibling_b, None, Some(0x10), true, false),
        None,
        "sibling_b must not receive a frame addressed to sibling_a's ECU"
    );
}

/// ADR-203 Decision item 7: a RawMode=ON KWP/J1850 CLL with an SA table
/// configured is still routed correctly by the wire byte -- `route_frame`
/// has no `RawMode` gate at all (ADR-196 Decision item 3 already
/// establishes that URID-table matching stays active under RawMode; that
/// convention applies to ComParam-derived TX composition and RX header
/// splitting, not to `route_frame`'s own client-configured matching).
#[test]
fn route_frame_matches_ecu_resp_source_address_under_raw_mode() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![sa_key(9, 0x10)],
        header_protocol: j2534_0404::ISO14230,
        raw_mode: true,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(route_frame(&entry, None, Some(0x10), true, false), Some(9));
}

/// ADR-203 Decision item 8: no `is_tx_side` gate on the new SA tier --
/// mirrors ADR-184's ungated J1939 SA tier. This test only documents that
/// `route_frame` itself applies the SA tier identically regardless of
/// `is_tx_side`; the reason a real TX-side echo naturally fails to match
/// (its own byte-2 is the TESTER's address, never a real
/// `CP_EcuRespSourceAddress` entry's value) is a property of the wire data,
/// not of this function.
#[test]
fn route_frame_ecu_resp_source_address_tier_is_not_is_tx_side_gated() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![sa_key(9, 0x10)],
        header_protocol: j2534_0404::ISO14230,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert_eq!(route_frame(&entry, None, Some(0x10), true, false), Some(9));
    assert_eq!(route_frame(&entry, None, Some(0x10), true, true), Some(9));
}

/// Codex review finding, this PR: a non-content (indication) frame on a
/// KWP/J1850 CLL must bypass the new SA-matching tier entirely, not be
/// treated as an unmatched ECU response and dropped. A SOM/RX_BREAK/TxDone/
/// loopback indication (ADR-097/ADR-098) routinely carries no data at all,
/// so `frame_source_addr` is always `None` for it -- without the
/// `is_content_frame` gate, a KWP/J1850 CLL with ANY SA-keyed entry would
/// have silently dropped every such indication (the frame never reaches
/// `bind_frame`'s own `indication_suppressed` policy at all when
/// `route_frame` returns `None`), regressing the pre-ADR-203 behavior where
/// these protocols always delivered every frame at the wildcard `0`
/// (`frame_can_id` is always `None` for KWP/J1850, so the old code
/// unconditionally hit the `None => Some(0)` fallback). `is_content_frame:
/// false` must still deliver at `0`, identically to the no-table case,
/// regardless of how the SA table is configured or whether a source byte
/// happens to be present.
#[test]
fn route_frame_non_content_frame_bypasses_the_sa_tier_and_still_wildcards() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![sa_key(9, 0x10)],
        header_protocol: j2534_0404::ISO14230,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    // Source-less (the realistic SOM/RX_BREAK case: empty data).
    assert_eq!(route_frame(&entry, None, None, false, false), Some(0));
    // Even a source byte that would otherwise match no configured entry, or
    // one that WOULD match, must not be consulted for a non-content frame.
    assert_eq!(route_frame(&entry, None, Some(0x99), false, false), Some(0));
    assert_eq!(route_frame(&entry, None, Some(0x10), false, false), Some(0));
}

/// Second Codex review finding, this PR: a non-content frame must never
/// fall through to the generic CAN-ID `.find()` below the KWP/J1850 branch
/// either, not just skip the SA-matching call. `frame_can_id` is computed
/// generically from a frame's own first 4 raw bytes with no protocol check
/// (`events.rs::poll_rx_inner`) -- a loopback/TxDone indication
/// (`is_content_frame: false`) carrying real echoed header+payload bytes
/// routinely produces a non-`None` `frame_can_id` (the K-line header bytes
/// misread as a u32), NOT the source-less case the sibling test above
/// covers. An earlier revision of this fix only gated the SA-matching call
/// itself, so this exact scenario fell through to
/// `UniqueRespIdKey::matched`, which can never succeed against an SA-keyed
/// entry (no `can_resp_usdt_id`/etc. populated for a KWP/J1850 CLL),
/// dropping the echo -- violating ADR-098's "loopback/TxDone must never be
/// filtered from delivery" invariant. Must still wildcard-deliver at `0`,
/// exactly like the source-less case.
#[test]
fn route_frame_non_content_frame_with_a_populated_frame_can_id_still_wildcards() {
    let entry = CllRxEntry {
        unique_resp_ids: vec![sa_key(9, 0x10)],
        header_protocol: j2534_0404::ISO14230,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    // A non-`None` frame_can_id that would NOT match any of this CLL's
    // (SA-only) entries via `UniqueRespIdKey::matched` -- exactly what a
    // misread K-line echo produces.
    assert_eq!(
        route_frame(&entry, Some(0x1080_10AA), None, false, true),
        Some(0)
    );
    // Also true for a non-tx-side non-content frame with a populated
    // frame_can_id (belt and suspenders -- this gate is not tied to
    // is_tx_side at all).
    assert_eq!(
        route_frame(&entry, Some(0x1080_10AA), None, false, false),
        Some(0)
    );
}

/// ADR-203: `kline_j1850_source_addr` for KWP's five `kwp_header_and_payload_len`
/// header shapes, plus a too-short/truncated frame for each.
#[test]
fn kline_j1850_source_addr_kwp_carb_header() {
    // CARB/ISO9141-2 exception addressing: fixed 3-byte header
    // [format, target, source], format & 0xC0 == 0x40.
    let data = [0x40, 0xAA, 0x11, 0x99];
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::ISO9141, &data),
        Some(0x11)
    );
    // Truncated: shorter than the fixed 3-byte header.
    let truncated = [0x40, 0xAA];
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::ISO9141, &truncated),
        None
    );
}

#[test]
fn kline_j1850_source_addr_kwp_addressed_embedded_len() {
    // Addressed, embedded length: format = 0x80 | len (len != 0).
    let data = [0x83, 0xBB, 0x22, 0x01, 0x02, 0x03];
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::ISO14230, &data),
        Some(0x22)
    );
    // Truncated: `kwp_header_and_payload_len` reports header_len == 3 for
    // this shape with NO bounds check against `data`'s actual length (the
    // gap this function's own `data.len() >= 3` guard exists to close) --
    // this frame is only 2 bytes long.
    let truncated = [0x83, 0xBB];
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::ISO14230, &truncated),
        None
    );
}

#[test]
fn kline_j1850_source_addr_kwp_addressed_separate_len_byte() {
    // Addressed, separate length byte: format = 0x80 (embedded len == 0).
    let data = [0x80, 0xCC, 0x33, 0x02, 0xAA, 0xBB];
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::ISO14230, &data),
        Some(0x33)
    );
    // Truncated to exactly 3 bytes -- the mandatory 4th length byte is
    // missing, but the source byte at index 2 is still physically present
    // and must still be extracted (edge-case-hunter finding, this PR): the
    // OLD implementation delegated to `kwp_header_and_payload_len`, whose
    // separate-length-byte arm requires that 4th byte to report a header
    // length at all, so it wrongly returned `None` here even though `0x33`
    // was right there in the bytes -- inconsistent with the
    // structurally-identical embedded-length shape above, which already
    // extracts the source byte correctly under the same 3-byte truncation.
    let truncated_at_source_byte = [0x80, 0xCC, 0x33];
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::ISO14230, &truncated_at_source_byte),
        Some(0x33)
    );
    // Truncated below the source byte itself: genuinely source-less.
    let truncated_before_source_byte = [0x80, 0xCC];
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::ISO14230, &truncated_before_source_byte),
        None
    );
}

#[test]
fn kline_j1850_source_addr_kwp_unaddressed_embedded_len() {
    // Unaddressed, embedded length: format's top 2 bits are 0, embedded
    // length != 0 -- no target/source bytes at all (header_len == 1), so
    // this is always source-less.
    let data = [0x02, 0xAA, 0xBB];
    assert_eq!(kline_j1850_source_addr(j2534_0404::ISO9141, &data), None);
    let truncated: [u8; 0] = [];
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::ISO9141, &truncated),
        None
    );
}

#[test]
fn kline_j1850_source_addr_kwp_unaddressed_separate_len_byte() {
    // Unaddressed, separate length byte: format == 0x00 -- no target/source
    // bytes at all (header_len == 2), always source-less.
    let data = [0x00, 0x03, 0xAA, 0xBB, 0xCC];
    assert_eq!(kline_j1850_source_addr(j2534_0404::ISO9141, &data), None);
    let truncated = [0x00];
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::ISO9141, &truncated),
        None
    );
}

#[test]
fn kline_j1850_source_addr_j1850_fixed_three_byte_header() {
    // J1850 (VPW/PWM) header is always exactly 3 bytes:
    // [format/priority, target, source] -- mirrors `header_footer_len`'s
    // own J1850PWM arm (`data.len().min(3)`).
    let data = [0x10, 0x20, 0x30, 0x40];
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::J1850PWM, &data),
        Some(0x30)
    );
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::J1850VPW, &data),
        Some(0x30)
    );
    // Too short (2 bytes or fewer): no source byte to read.
    let truncated = [0x10, 0x20];
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::J1850PWM, &truncated),
        None
    );
    assert_eq!(
        kline_j1850_source_addr(j2534_0404::J1850VPW, &truncated),
        None
    );
    let empty: [u8; 0] = [];
    assert_eq!(kline_j1850_source_addr(j2534_0404::J1850PWM, &empty), None);
}

/// `kline_j1850_source_addr` returns `None` for any protocol it does not
/// cover (e.g. CAN) -- consulted only via `poll_rx_inner`'s per-frame
/// pre-computation, which is a provable no-op on CAN/J1939/TP2.0/ISO15765
/// channels by construction.
#[test]
fn kline_j1850_source_addr_none_for_unrelated_protocol() {
    let data = [0x00, 0x00, 0x07, 0xDF, 0x02, 0x10, 0x01];
    assert_eq!(kline_j1850_source_addr(j2534_0404::CAN, &data), None);
}

/// A `TesterPresentDiscard` whose content/SOM-herald arms match any
/// payload/frame starting with `pos` -- unrestricted by
/// `target_can_ids`/`tx_can_id` (both `None`), the common case for these
/// tests.
fn discard_matching(pos: Vec<u8>) -> TesterPresentDiscard {
    TesterPresentDiscard {
        pos,
        neg: Vec::new(),
        target_can_ids: None,
        tx_can_id: None,
    }
}

fn acceptance_id_of(binding: &FrameBinding) -> Option<u32> {
    match binding {
        FrameBinding::Registrant { acceptance_id, .. } => Some(*acceptance_id),
        _ => None,
    }
}

fn cop_handle_of(binding: &FrameBinding) -> Option<u32> {
    match binding {
        FrameBinding::Registrant { cop_handle, .. } => Some(*cop_handle),
        _ => None,
    }
}

/// Step 2 outranks step 3: a specifically-expected client response is
/// never swallowed by tester-present discard, even when the same frame
/// also matches the discard's own signature (ADR-088's original worry,
/// reaffirmed by ADR-100).
#[test]
fn non_vacuous_registrant_beats_tester_present() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            expected: vec![non_vacuous_expected(0x7E, 77)],
            matches_needed: Some(1),
            ..registrant(100, 1)
        }],
        vec![discard_matching(vec![0x7E])],
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7E, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(cop_handle_of(&binding), Some(100));
    assert_eq!(acceptance_id_of(&binding), Some(77));
    assert_eq!(
        entry.registrants[0].matches_got, 1,
        "the winning registrant's matches_got must be incremented"
    );
}

/// Step 3 outranks step 4: THE field-bug regression fix. A
/// vacuous-descriptor COP no longer captures a tester-present reply --
/// tester-present's own identity-signature match wins instead.
#[test]
fn tester_present_beats_vacuous_registrant() {
    let mut entry = test_entry(
        1,
        vec![registrant(100, 1)], // vacuous: true (default)
        vec![discard_matching(vec![0x7E])],
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7E, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert!(
        matches!(binding, FrameBinding::TesterPresent),
        "tester-present must win over a vacuous registrant's claim"
    );
    assert_eq!(
        entry.registrants[0].matches_got, 0,
        "the losing vacuous registrant must not be credited with a match"
    );
}

/// Step 4 still works when nothing outranks it: with no tester-present
/// window armed at all, a vacuous registrant still receives an otherwise
/// unbound frame -- distinct from S8's discard flip (`poll_rx_inner`),
/// which only fires when `bind_frame` itself returns `Unbound`; a
/// vacuous registrant claim at step 4 never reaches that fallthrough.
#[test]
fn vacuous_registrant_receives_frame_when_no_tester_present_configured() {
    let mut entry = test_entry(1, vec![registrant(100, 1)], Vec::new());

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x11, 0x22],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(cop_handle_of(&binding), Some(100));
}

/// ADR-100 Decision §3's round-5 addendum (PR #103, Codex review round 5):
/// a MIXED registrant -- one specific (non-vacuous) descriptor plus one
/// fully-empty descriptor on the SAME registrant -- must not bind via its
/// empty descriptor to a frame that only matches tester-present's own
/// signature, not the specific descriptor. Before the per-descriptor
/// `is_vacuous()` filter, this registrant's registrant-level `vacuous`
/// flag was `false` (not every descriptor was empty), so it reached the
/// shared match-acceptance scan with no per-descriptor filter at all, and
/// its empty descriptor vacuously matched the tester-present reply --
/// reproducing the original field bug for the mixed-descriptor case.
/// Mirrors `tester_present_beats_vacuous_registrant`, but for a mixed
/// registrant instead of a fully-vacuous one.
#[test]
fn tester_present_beats_mixed_registrants_empty_descriptor() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            expected: vec![non_vacuous_expected(0x62, 77), vacuous_expected(99)],
            matches_needed: Some(1),
            ..registrant(100, 1)
        }],
        vec![discard_matching(vec![0x7E])],
    );

    // The frame is a tester-present reply: it matches the discard
    // signature but NOT the registrant's specific 0x62 descriptor.
    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7E, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert!(
        matches!(binding, FrameBinding::TesterPresent),
        "tester-present must win over a mixed registrant's empty descriptor"
    );
    assert_eq!(
        entry.registrants[0].matches_got, 0,
        "the losing mixed registrant must not be credited with a match"
    );
}

/// The same mixed registrant DOES still bind correctly via its specific
/// (non-vacuous) descriptor at step 2 (`Tier1NonVacuous`) when a frame
/// actually matches it -- the per-descriptor filter must not suppress a
/// genuine specific-descriptor match on a registrant that also happens to
/// carry an empty descriptor.
#[test]
fn mixed_registrant_binds_via_specific_descriptor_at_tier1_non_vacuous() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            expected: vec![non_vacuous_expected(0x62, 77), vacuous_expected(99)],
            matches_needed: Some(1),
            ..registrant(100, 1)
        }],
        vec![discard_matching(vec![0x7E])],
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x62, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(cop_handle_of(&binding), Some(100));
    assert_eq!(
        acceptance_id_of(&binding),
        Some(77),
        "must bind via the specific descriptor's own acceptance_id"
    );
    assert_eq!(entry.registrants[0].matches_got, 1);
}

/// The same mixed registrant's empty descriptor IS correctly considered
/// and can bind at the `Tier1Vacuous` step (step 4) for an unrelated
/// frame, when no tester-present signature is configured to intercept it
/// first. Before this fix, the now-deleted registrant-level gate
/// (`if scan == Tier1Vacuous && !r.vacuous { continue }`) would have
/// skipped this registrant entirely at step 4, since `vacuous` was `false`
/// for a mixed registrant -- an active under-match this fix also closes.
/// A direct `bind_registrant` call with `Tier1NonVacuous` first proves
/// step 2 does NOT claim this frame, so the eventual match is specifically
/// attributable to step 4.
#[test]
fn mixed_registrant_empty_descriptor_binds_at_tier1_vacuous_step() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            expected: vec![non_vacuous_expected(0x62, 77), vacuous_expected(99)],
            matches_needed: Some(1),
            ..registrant(100, 1)
        }],
        Vec::new(),
    );

    let mut probe_registrants = entry.registrants.clone();
    assert_eq!(
        bind_registrant(
            &mut probe_registrants,
            1,
            &[0x11, 0x22],
            0,
            AttributionScan::Tier1NonVacuous,
            0,
            ConcatFrameMeta::default(),
            &mut Vec::new(),
            &mut false,
        ),
        None,
        "Tier1NonVacuous must not claim this frame via the mixed registrant's empty descriptor"
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x11, 0x22],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(cop_handle_of(&binding), Some(100));
    assert_eq!(
        acceptance_id_of(&binding),
        Some(99),
        "must bind via the empty descriptor's own acceptance_id, proving Tier1Vacuous (step 4) claimed it"
    );
}

/// Step 1: indication frames (here, a START_OF_MESSAGE herald) never
/// bind to a registrant regardless of vacuousness, even with no
/// tester-present window armed to otherwise intercept them.
#[test]
fn indication_frame_never_binds_to_a_registrant() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            expected: vec![non_vacuous_expected(0x7E, 77)],
            matches_needed: Some(1),
            ..registrant(100, 1)
        }],
        Vec::new(),
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: false, // SOM/TxDone/loopback/RxBreak
            is_tx_side: false,
            frame_can_id: None,
            payload: &[],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: RX_START_OF_MESSAGE as u8,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert!(matches!(binding, FrameBinding::Unbound));
    assert_eq!(entry.registrants[0].matches_got, 0);
}

/// Step 1's bypass does not exempt an indication frame from step 3:
/// ADR-099's SOM-herald discard arm specifically matches indication-bit
/// frames, and must still run "on top of" the bypass (ADR-100 Decision
/// §3, step 1's own doc note).
#[test]
fn indication_frame_still_discarded_by_tester_present_som_herald() {
    let mut entry = test_entry(
        1,
        vec![registrant(100, 1)],
        vec![discard_matching(vec![0x7E])],
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: false,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[], // SOM heralds carry no payload
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: RX_START_OF_MESSAGE as u8,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert!(matches!(binding, FrameBinding::TesterPresent));
}

/// ADR-151: `bind_frame` itself never consults `start_msg_ind_enable`/
/// `transmit_ind_enable` -- ADR-099's tester-present SOM-herald discard
/// still claims a discardable TP-response herald via the `TesterPresent`
/// tier even with `CP_StartMsgIndEnable = 1`, since `indication_suppressed`
/// is only ever consulted by `poll_rx_inner`'s `Unbound` fallthrough --
/// which this frame never reaches.
#[test]
fn tester_present_som_herald_discard_unaffected_by_start_msg_ind_enable() {
    let mut entry = CllRxEntry {
        start_msg_ind_enable: true,
        ..test_entry(
            1,
            vec![registrant(100, 1)],
            vec![discard_matching(vec![0x7E])],
        )
    };

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: false,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[], // SOM heralds carry no payload
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: RX_START_OF_MESSAGE as u8,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert!(
        matches!(binding, FrameBinding::TesterPresent),
        "ADR-099 discard must win regardless of CP_StartMsgIndEnable -- the ADR-151 gate never runs for a frame bind_frame already claimed"
    );
}

/// ADR-151: `CP_StartMsgIndEnable = 0` (the spec default for every
/// protocol) withholds a pure SOM indication frame.
#[test]
fn start_msg_ind_disabled_by_default_suppresses_som_indication() {
    let entry = test_entry(1, Vec::new(), Vec::new());
    assert!(!entry.start_msg_ind_enable, "default must be disabled");
    assert!(indication_suppressed(&entry, RX_START_OF_MESSAGE as u8));
}

/// ADR-151: `CP_StartMsgIndEnable = 1` allows the same SOM indication
/// frame through.
#[test]
fn start_msg_ind_enabled_allows_som_indication() {
    let entry = CllRxEntry {
        start_msg_ind_enable: true,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert!(!indication_suppressed(&entry, RX_START_OF_MESSAGE as u8));
}

/// ADR-151: `CP_TransmitIndEnable = 0` (default) withholds a pure TxDone
/// indication frame (SAE J2534-1 §8.7.2's TxDone row sets both
/// `RX_TX_MSG_TYPE` and `RX_TX_INDICATION`).
#[test]
fn transmit_ind_disabled_by_default_suppresses_txdone_indication() {
    let entry = test_entry(1, Vec::new(), Vec::new());
    assert!(!entry.transmit_ind_enable, "default must be disabled");
    assert!(indication_suppressed(
        &entry,
        (RX_TX_MSG_TYPE | RX_TX_INDICATION) as u8
    ));
}

/// ADR-151: `CP_TransmitIndEnable = 1` allows the same TxDone indication
/// frame through.
#[test]
fn transmit_ind_enabled_allows_txdone_indication() {
    let entry = CllRxEntry {
        transmit_ind_enable: true,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert!(!indication_suppressed(
        &entry,
        (RX_TX_MSG_TYPE | RX_TX_INDICATION) as u8
    ));
}

/// ADR-151: a pure `RX_TX_MSG_TYPE`-only loopback echo (no
/// `RX_TX_INDICATION`) is never gated by either ComParam -- neither with
/// both disabled (the default) nor with both enabled.
#[test]
fn tx_msg_type_only_loopback_never_gated() {
    let disabled = test_entry(1, Vec::new(), Vec::new());
    assert!(!indication_suppressed(&disabled, RX_TX_MSG_TYPE as u8));

    let enabled = CllRxEntry {
        start_msg_ind_enable: true,
        transmit_ind_enable: true,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert!(!indication_suppressed(&enabled, RX_TX_MSG_TYPE as u8));
}

/// ADR-151: a pure `RX_BREAK`-only frame is never gated by either
/// ComParam, regardless of value.
#[test]
fn rx_break_only_never_gated() {
    let disabled = test_entry(1, Vec::new(), Vec::new());
    assert!(!indication_suppressed(&disabled, RX_BREAK as u8));

    let enabled = CllRxEntry {
        start_msg_ind_enable: true,
        transmit_ind_enable: true,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert!(!indication_suppressed(&enabled, RX_BREAK as u8));
}

/// ADR-151: the off-spec combination `RX_START_OF_MESSAGE |
/// RX_TX_INDICATION` (SAE J2534-1 §8.7.2's table does not enumerate this
/// combination) resolves via TX_INDICATION-dominant precedence -- with
/// `CP_TransmitIndEnable = 1` and `CP_StartMsgIndEnable = 0`, the frame
/// IS delivered, proving TX_INDICATION wins rather than a stricter
/// "both must be enabled" reading.
#[test]
fn off_spec_som_plus_tx_indication_governed_by_transmit_ind_only() {
    let entry = CllRxEntry {
        start_msg_ind_enable: false,
        transmit_ind_enable: true,
        ..test_entry(1, Vec::new(), Vec::new())
    };
    assert!(!indication_suppressed(
        &entry,
        (RX_START_OF_MESSAGE | RX_TX_INDICATION) as u8
    ));
}

/// ADR-151 (edge-case-hunter finding): `RX_BREAK` combined with
/// `RX_START_OF_MESSAGE`/`RX_TX_INDICATION` must still be delivered even
/// when the corresponding enable ComParam is disabled (the spec
/// default) -- `RX_BREAK` is never a governed indication type in its own
/// right and always defeats both gates, matching the same-file
/// precedent at `bind_frame`'s tester-present signature check.
#[test]
fn rx_break_combined_with_indication_bit_never_suppressed_even_when_disabled() {
    let disabled = test_entry(1, Vec::new(), Vec::new());
    assert!(!indication_suppressed(
        &disabled,
        (RX_START_OF_MESSAGE | RX_BREAK) as u8
    ));
    assert!(!indication_suppressed(
        &disabled,
        (RX_TX_INDICATION | RX_BREAK) as u8
    ));
}

/// ADR-151 (Codex review finding): a CONFIG_LOOPBACK echo of our own
/// SOM-tagged transmit (`RX_TX_MSG_TYPE | RX_START_OF_MESSAGE`, ADR-098's
/// Context -- a documented, reachable combination) must still be
/// delivered when `CP_StartMsgIndEnable` is disabled (the spec default):
/// `RX_TX_MSG_TYPE` is an independent delivery justification (ADR-098
/// never filters loopback), not something the SOM gate should swallow.
#[test]
fn loopback_echo_combined_with_som_never_suppressed_even_when_disabled() {
    let disabled = test_entry(1, Vec::new(), Vec::new());
    assert!(!indication_suppressed(
        &disabled,
        (RX_TX_MSG_TYPE | RX_START_OF_MESSAGE) as u8
    ));
}

/// ADR-086 generation gate: a frame arriving after this CLL's
/// `connect_generation` has advanced (a reconnect completed) must not
/// bind to a registrant left over from the OLD generation, even though
/// its descriptor would otherwise match.
#[test]
fn stale_generation_registrant_is_excluded() {
    let mut entry = test_entry(
        2, // live generation advanced past the registrant's own
        vec![CopRegistrant {
            expected: vec![non_vacuous_expected(0x7E, 77)],
            matches_needed: Some(1),
            ..registrant(100, 1) // registrant's own generation: 1 (stale)
        }],
        Vec::new(),
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7E, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert!(
        matches!(binding, FrameBinding::Unbound),
        "a stale-generation registrant must never bind, even to an otherwise-matching frame"
    );
}

/// IS-MULTIPLE match-count bounding: a registrant stops accepting
/// further matches once `matches_needed` is satisfied -- a later frame
/// that would otherwise match is left unbound by `bind_frame` itself
/// (this test only exercises `bind_frame`; S8's discard flip lives in
/// `poll_rx_inner`, the caller, and is covered separately).
#[test]
fn match_count_bounding_stops_after_satisfied() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            expected: vec![non_vacuous_expected(0x7E, 77)],
            matches_needed: Some(1),
            ..registrant(100, 1)
        }],
        Vec::new(),
    );

    let first = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7E, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );
    assert_eq!(cop_handle_of(&first), Some(100));
    assert_eq!(entry.registrants[0].matches_got, 1);

    let second = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7E, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );
    assert!(
        matches!(second, FrameBinding::Unbound),
        "a satisfied registrant must not accept a further match"
    );
    assert_eq!(entry.registrants[0].matches_got, 1);
}

/// Pending-RC detection still claims a frame and is evaluated for EVERY
/// tier-1 registrant regardless of its `vacuous` flag -- RC detection is
/// independent of expected-response mask/pattern vacuousness (ADR-100
/// Decision §3).
#[test]
fn pending_rc_detection_claims_frame_regardless_of_vacuous() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            rc_cfg: Some(RcHandlingConfig {
                rc_byte_offset: 2,
                rc78_handling: true,
                request_sid: Some(0x22),
                ..Default::default()
            }),
            ..registrant(100, 1) // vacuous: true (default)
        }],
        Vec::new(),
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7F, 0x22, 0x78],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(cop_handle_of(&binding), Some(100));
    assert_eq!(acceptance_id_of(&binding), Some(0));
    assert_eq!(entry.registrants[0].pending_rc, Some(0x78));
    assert_eq!(
        entry.registrants[0].matches_got, 0,
        "a pending-RC frame must not count toward matches_needed"
    );
}

/// A pending-RC frame beats tester-present too -- it is evaluated in
/// step 2, before step 3, exactly like a non-vacuous descriptor match.
#[test]
fn pending_rc_detection_beats_tester_present() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            rc_cfg: Some(RcHandlingConfig {
                rc_byte_offset: 2,
                rc78_handling: true,
                request_sid: Some(0x22),
                ..Default::default()
            }),
            ..registrant(100, 1)
        }],
        vec![discard_matching(vec![0x7F])],
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7F, 0x22, 0x78],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(cop_handle_of(&binding), Some(100));
    assert_eq!(entry.registrants[0].pending_rc, Some(0x78));
}

/// "First pending-RC code wins" within one pass: mirrors the old
/// per-call-fresh `MatchProbe`'s semantics -- a second, different
/// pending code detected later in the SAME pass (i.e. a second call
/// without an intervening reset, exactly as multiple frames in one
/// `PassThruReadMsgs` batch would drive) does not overwrite the first.
#[test]
fn pending_rc_first_code_wins_within_one_pass() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            rc_cfg: Some(RcHandlingConfig {
                rc_byte_offset: 2,
                rc78_handling: true,
                rc21_handling: true,
                request_sid: Some(0x22),
                ..Default::default()
            }),
            ..registrant(100, 1)
        }],
        Vec::new(),
    );

    bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7F, 0x22, 0x78],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );
    bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7F, 0x22, 0x21],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(entry.registrants[0].pending_rc, Some(0x78));
}

/// ADR-100 Decision §3, resolved (b) addendum: `detect_pending_rc`'s
/// claim is additionally gated on `unique_resp_ids`, mirroring the
/// positive-match scan's own ECU scoping -- a pending-RC frame from an
/// ECU none of the registrant's descriptors accept must not extend that
/// registrant's wait.
#[test]
fn pending_rc_ignored_when_unique_resp_id_out_of_scope() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![ExpectedResponse {
            mask: Vec::new(),
            pattern: Vec::new(),
            unique_resp_ids: vec![2],
            acceptance_id: 0,
        }],
        rc_cfg: Some(RcHandlingConfig {
            rc_byte_offset: 2,
            rc78_handling: true,
            request_sid: Some(0x22),
            ..Default::default()
        }),
        ..registrant(100, 1)
    }];

    let binding = bind_registrant(
        &mut registrants,
        1,
        &[0x7F, 0x22, 0x78],
        5, // a different ECU than this registrant's [2]-scoped descriptor
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );

    assert!(
        binding.is_none(),
        "a pending-RC frame from an out-of-scope ECU must not bind"
    );
    assert_eq!(registrants[0].pending_rc, None);
}

/// Companion to the above: the same registrant DOES have its wait
/// extended when the frame's `unique_resp_identifier` is one its
/// descriptor actually accepts.
#[test]
fn pending_rc_claims_frame_when_unique_resp_id_in_scope() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![ExpectedResponse {
            mask: Vec::new(),
            pattern: Vec::new(),
            unique_resp_ids: vec![2],
            acceptance_id: 0,
        }],
        rc_cfg: Some(RcHandlingConfig {
            rc_byte_offset: 2,
            rc78_handling: true,
            request_sid: Some(0x22),
            ..Default::default()
        }),
        ..registrant(100, 1)
    }];

    let binding = bind_registrant(
        &mut registrants,
        1,
        &[0x7F, 0x22, 0x78],
        2,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(binding, Some((0, 100, false, None, false, None)));
    assert_eq!(registrants[0].pending_rc, Some(0x78));
}

/// Control case: a registrant with an unrestricted (empty
/// `unique_resp_ids`) descriptor -- the common single-ECU / no-table
/// path -- still has its wait extended by a pending RC regardless of
/// `unique_resp_identifier`, proving no regression to that common case.
#[test]
fn pending_rc_claims_frame_regardless_of_unique_resp_id_when_unrestricted() {
    let mut registrants = vec![CopRegistrant {
        rc_cfg: Some(RcHandlingConfig {
            rc_byte_offset: 2,
            rc78_handling: true,
            request_sid: Some(0x22),
            ..Default::default()
        }),
        ..registrant(100, 1) // expected: one descriptor, unique_resp_ids: Vec::new()
    }];

    let binding = bind_registrant(
        &mut registrants,
        1,
        &[0x7F, 0x22, 0x78],
        5,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(binding, Some((0, 100, false, None, false, None)));
    assert_eq!(registrants[0].pending_rc, Some(0x78));
}

/// Intra-tier order is ascending `registration_seq` (mirrored here by
/// `registrants` Vec order, exactly as `build_cll_rx_entries`/insertion
/// order preserve it): when two non-vacuous registrants both match, the
/// earlier one wins.
#[test]
fn earlier_registration_seq_wins_when_both_match() {
    let mut entry = test_entry(
        1,
        vec![
            CopRegistrant {
                expected: vec![non_vacuous_expected(0x7E, 1)],
                matches_needed: Some(1),
                ..registrant(100, 1)
            },
            CopRegistrant {
                expected: vec![non_vacuous_expected(0x7E, 2)],
                matches_needed: Some(1),
                ..registrant(101, 1)
            },
        ],
        Vec::new(),
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7E, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(cop_handle_of(&binding), Some(100));
}

/// When only the LATER registrant's descriptor matches, it still wins --
/// order is a tie-break, not an exclusive first-registrant rule.
#[test]
fn later_registration_seq_wins_when_only_it_matches() {
    let mut entry = test_entry(
        1,
        vec![
            CopRegistrant {
                expected: vec![non_vacuous_expected(0x11, 1)],
                matches_needed: Some(1),
                ..registrant(100, 1)
            },
            CopRegistrant {
                expected: vec![non_vacuous_expected(0x7E, 2)],
                matches_needed: Some(1),
                ..registrant(101, 1)
            },
        ],
        Vec::new(),
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7E, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(cop_handle_of(&binding), Some(101));
}

/// Step 5 (tier-2 / Receive Only) binds when nothing else claims the
/// frame -- wired for correctness ahead of S5, which is the first step
/// that will actually construct a `RegistrantTier::ReceiveOnly`
/// registrant.
#[test]
fn tier2_registrant_binds_when_present() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            tier: RegistrantTier::ReceiveOnly,
            matches_needed: Some(1),
            ..registrant(200, 1)
        }],
        Vec::new(),
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x11, 0x22],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(cop_handle_of(&binding), Some(200));
}

/// Step 5 ranks below step 3: even a non-vacuous tier-2 registrant loses
/// to a tester-present signature match, since tier-2 is only scanned
/// after tester-present in the precedence order.
#[test]
fn tier2_registrant_loses_to_tester_present_even_when_non_vacuous() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            tier: RegistrantTier::ReceiveOnly,
            expected: vec![non_vacuous_expected(0x7E, 5)],
            matches_needed: Some(1),
            ..registrant(200, 1)
        }],
        vec![discard_matching(vec![0x7E])],
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7E, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert!(matches!(binding, FrameBinding::TesterPresent));
}

/// ADR-137 fourth Codex-review fix (round-4 restructure): `bind_frame`'s
/// step 3 discards a frame that matches ANY entry in
/// `tester_present_discard`, not just the first -- each entry is an
/// independently elicited send, not a precedence chain. A frame matching
/// only the SECOND entry's signature (the first entry's `pos` would never
/// match this payload) must still be discarded.
#[test]
fn tester_present_discard_matches_any_entry_not_just_the_first() {
    let mut entry = test_entry(
        1,
        vec![registrant(100, 1)],
        vec![discard_matching(vec![0x7E]), discard_matching(vec![0x7F])],
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7F, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert!(
        matches!(binding, FrameBinding::TesterPresent),
        "a frame matching only the second entry's signature must still be discarded -- \
             tester_present_discard is a list, matched by \"any entry\", not \"first entry wins\""
    );
}

/// A frame matching NEITHER entry's signature falls through to ordinary
/// registrant attribution -- an empty/no-match list must not swallow
/// everything.
#[test]
fn tester_present_discard_list_does_not_match_an_unrelated_frame() {
    let mut entry = test_entry(
        1,
        vec![registrant(100, 1)],
        vec![discard_matching(vec![0x7E]), discard_matching(vec![0x7F])],
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x11, 0x22],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(cop_handle_of(&binding), Some(100));
}

/// `push_open_tp_discard` (the shared prune-then-push-then-cap helper
/// used by all three `open_tp_discards` write sites) is a plain
/// synchronous function over owned data, tested directly here rather
/// than only indirectly through the `grpc_mock` integration suite.
fn discard_at(until_offset_ms: i64, pos: u8) -> ResidualTesterPresentDiscard {
    let base = tokio::time::Instant::now();
    let until = if until_offset_ms >= 0 {
        base + Duration::from_millis(until_offset_ms as u64)
    } else {
        base - Duration::from_millis((-until_offset_ms) as u64)
    };
    ResidualTesterPresentDiscard {
        window: DiscardWindow {
            until,
            pos: vec![pos],
            neg: Vec::new(),
        },
        target_can_ids: None,
        tx_can_id: None,
    }
}

/// Pushing `Some(entry)` prunes expired entries first, then appends the
/// new one -- still-open entries survive a push, expired ones do not.
#[test]
fn push_open_tp_discard_prunes_expired_then_pushes() {
    let mut list = vec![discard_at(-10, 0x01), discard_at(10_000, 0x02)];
    let now = tokio::time::Instant::now();

    push_open_tp_discard(&mut list, now, Some(discard_at(10_000, 0x03)));

    let pos_bytes: Vec<u8> = list.iter().map(|r| r.window.pos[0]).collect();
    assert_eq!(
        pos_bytes,
        vec![0x02, 0x03],
        "the expired entry (0x01) must be pruned; the still-open entry (0x02) must survive; \
             the new entry (0x03) must be appended"
    );
}

/// Pushing `None` (a failed send) still prunes expired entries, but adds
/// nothing -- this is the round-4 fix for the pre-existing "a failed send
/// unconditionally clears the slot" bug: a still-open prior window must
/// never be dropped by a later failure.
#[test]
fn push_open_tp_discard_none_prunes_but_never_drops_a_still_open_prior_entry() {
    let mut list = vec![discard_at(-10, 0x01), discard_at(10_000, 0x02)];
    let now = tokio::time::Instant::now();

    push_open_tp_discard(&mut list, now, None);

    let pos_bytes: Vec<u8> = list.iter().map(|r| r.window.pos[0]).collect();
    assert_eq!(
        pos_bytes,
        vec![0x02],
        "a failed send (entry = None) must not drop the still-open prior entry (0x02), and \
             must not itself add anything"
    );
}

/// Codex review round 5 finding: a numeric FIFO cap could evict an
/// unexpired entry to make room, silently reopening the leak this whole
/// mechanism exists to close. Pushing a SAME-signature entry (identical
/// `pos`/`neg`/`target_can_ids`/`tx_can_id`, only `until` differs) must
/// instead extend the existing entry's deadline in place -- the list
/// must not grow at all for the common case of repeated
/// identical-signature sends.
#[test]
fn push_open_tp_discard_same_signature_extends_existing_entry_in_place() {
    let mut list = vec![discard_at(1_000, 0x01)];
    let now = tokio::time::Instant::now();

    push_open_tp_discard(&mut list, now, Some(discard_at(10_000, 0x01)));

    assert_eq!(
        list.len(),
        1,
        "a same-signature push must extend the existing entry, not append a duplicate"
    );
    assert!(
        list[0].window.until >= now + Duration::from_millis(10_000),
        "the existing entry's deadline must be extended to the new push's (later) `until`"
    );
}

/// Companion to the extend-in-place test: no numeric cap exists any
/// more, so a run of many DISTINCT-signature pushes (the scenario Codex
/// round 5's finding constructed -- a long `CP_P2Max` combined with
/// frequent same-`same_wire_behavior`-excluded-field promotions between
/// sends) must all survive, however many there are, rather than the
/// oldest being evicted once some fixed capacity is reached.
#[test]
fn push_open_tp_discard_never_evicts_distinct_unexpired_entries() {
    let mut list: Vec<ResidualTesterPresentDiscard> = Vec::new();
    let now = tokio::time::Instant::now();

    // Well past the old OPEN_TP_DISCARDS_CAP (8) -- every one of these
    // has a distinct `pos`, so none of them dedupe against each other.
    for i in 0..20u8 {
        push_open_tp_discard(&mut list, now, Some(discard_at(10_000, i)));
    }

    assert_eq!(
        list.len(),
        20,
        "every distinct-signature entry must survive -- there is no cap that may evict an \
             unexpired, distinctly-signed window (Codex review finding, round 5)"
    );
    assert_eq!(
        list.first().map(|r| r.window.pos[0]),
        Some(0),
        "the first-pushed entry (would have been evicted under the old FIFO cap) must still \
             be present"
    );
}

// --- ADR-146: KWP Access Timing Parameter live-exchange tests ---

/// A `TimingChangeConfig` with `tpi` already attached to `tpi`, and no
/// `default_timing`/`override_timing`/`tpi3_request_bytes` -- for tests
/// that only care about response pairing/derivation (`from_params`/
/// `with_request` themselves are covered separately in
/// `service::tests`). `functional: true` since this helper's own callers
/// include the functional-addressing worst-case combination test, and a
/// single-response test folds to itself either way (a no-op fold).
fn access_timing_cfg(tpi: u8) -> AccessTimingConfig {
    AccessTimingConfig {
        tpi: Some(tpi),
        tpi3_request_bytes: None,
        default_timing: None,
        override_timing: None,
        functional: true,
    }
}

fn timing_cfg_tpi(tpi: u8) -> TimingChangeConfig {
    TimingChangeConfig::KwpAccess(access_timing_cfg(tpi))
}

/// Required test 1: a `7F 83 78` pending-RC response is still claimed by
/// the existing pending-RC path unaffected by ADR-146, and a subsequent
/// genuine `C3 02 <5 bytes>` binds normally afterward and derives
/// ComParams -- "coexists with existing pending-RC detection unchanged"
/// (ADR-146 Decision, "Detection and pairing").
#[test]
fn timing_change_pending_rc_then_genuine_tpi2_response_both_detected() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            rc_cfg: Some(RcHandlingConfig {
                rc_byte_offset: 2,
                rc78_handling: true,
                request_sid: Some(0x83),
                ..Default::default()
            }),
            timing_cfg: Some(timing_cfg_tpi(2)),
            ..registrant(100, 1)
        }],
        Vec::new(),
    );

    let first = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7F, 0x83, 0x78],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert_eq!(cop_handle_of(&first), Some(100));
    assert_eq!(entry.registrants[0].pending_rc, Some(0x78));
    assert!(
        entry.registrants[0].pending_timing_change.is_none(),
        "an interim pending-RC response must never be mistaken for a qualifying 0xC3"
    );

    let second = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0xC3, 0x02, 10, 20, 30, 40, 50],
            unique_resp_identifier: 0,
        },
        1,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert_eq!(cop_handle_of(&second), Some(100));
    let (seq, change) = entry.registrants[0]
        .pending_timing_change
        .as_ref()
        .expect("the genuine 0xC3 TPI=2 response must produce a pending timing change");
    assert_eq!(
        *seq, 1,
        "must record the frame_seq of the qualifying response"
    );
    assert!(
        change
            .derived
            .contains(&(ComParamId(j2534_0404::P2_MIN), 10 * 500))
    );
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [10, 20, 30, 40, 50]
        })
    );
}

/// ADR-146/148 interaction (merge finding, PR #17): a registrant with
/// BOTH `concat_enabled` and `timing_cfg` set (a plausible real config --
/// both features are scoped to ISO14230, so a client can enable
/// `CP_ModifyTiming` and `CP_EnableConcatenation` on the same KWP CLL) must
/// still pair a qualifying `0xC3` TPI=2 response even though it also
/// opens an ADR-148 concat buffer for it (the response is short enough to
/// never need a second segment, so it is always the buffer's OPENING
/// segment). Before this fix, `bind_registrant`'s ADR-146 pairing block
/// lived only in the non-concat match arm, so `r.pending_timing_change`
/// was silently never populated for such a registrant.
///
/// Fail-without-the-fix control: removing the
/// `observe_registrant_timing_change` call from the concat "open new
/// buffer" arm (restoring the old hardcoded `(false, None)`) makes this
/// test fail; restoring the call makes it pass.
#[test]
fn timing_change_pairs_even_when_response_opens_a_concat_buffer() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            concat_enabled: true,
            timing_cfg: Some(timing_cfg_tpi(2)),
            ..registrant(100, 1)
        }],
        Vec::new(),
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0xC3, 0x02, 10, 20, 30, 40, 50],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );

    match binding {
        FrameBinding::Registrant {
            cop_handle,
            ecu_timing_change,
            absorbed,
            ..
        } => {
            assert_eq!(cop_handle, 100);
            assert!(
                absorbed,
                "a short 0xC3 response always opens a fresh concat buffer"
            );
            assert!(
                ecu_timing_change,
                "the buffer's opening segment must still be paired against timing_cfg"
            );
        }
        _ => panic!("expected Registrant binding"),
    }
    assert_eq!(
        entry.registrants[0].concat.len(),
        1,
        "the response must still open a concat buffer, unaffected by the timing pairing"
    );
    let (seq, change) = entry.registrants[0].pending_timing_change.as_ref().expect(
        "the ComParam side effect must be captured even though delivery is deferred \
                 to the eventual concat finalize",
    );
    assert_eq!(*seq, 0);
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [10, 20, 30, 40, 50]
        })
    );
}

/// edge-case-hunter finding (round following the ADR-146/148 merge): an
/// already-open concat buffer is finalized only by the receive-phase
/// deadline or a cap overflow (`ConcatBuf`'s own doc comment) -- never by
/// "another matching frame arrived" -- so a SECOND, independent
/// qualifying `0xC3` response sharing the first one's exact buffer key
/// (a physically addressed ECU's retransmission or a repeat exchange)
/// is absorbed as if it were a continuation. The ComParam side effect
/// must still track the LATEST response, not freeze on the first one's
/// now-stale values -- matching this mechanism's own established
/// arrival-order/last-exchange-wins semantics elsewhere
/// (`select_latest_timing_changes`).
///
/// Fail-without-the-fix control: removing the
/// `observe_registrant_timing_change` call from the continuation fast
/// path's absorb arm (restoring the old hardcoded `(false, None)`) makes
/// this test fail (`pending_timing_change` would stay frozen at the
/// first response's seq/bytes); restoring the call makes it pass.
#[test]
fn timing_change_updates_to_the_latest_response_even_when_absorbed_as_a_continuation() {
    let physical_cfg = TimingChangeConfig::KwpAccess(AccessTimingConfig {
        tpi: Some(2),
        tpi3_request_bytes: None,
        default_timing: None,
        override_timing: None,
        functional: false,
    });
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            concat_enabled: true,
            timing_cfg: Some(physical_cfg),
            ..registrant(100, 1)
        }],
        Vec::new(),
    );

    bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0xC3, 0x02, 10, 20, 30, 40, 50],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );

    // A second, wholly independent qualifying response shares the first
    // one's exact buffer key -- collides into it as a "continuation".
    let second = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0xC3, 0x02, 99, 98, 97, 96, 95],
            unique_resp_identifier: 0,
        },
        1,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );

    match second {
        FrameBinding::Registrant {
            cop_handle,
            ecu_timing_change,
            superseded_timing_seq,
            absorbed,
            ..
        } => {
            assert_eq!(cop_handle, 100);
            assert!(absorbed, "the collision is absorbed, not a new buffer");
            assert!(
                ecu_timing_change,
                "the second response must still be paired against timing_cfg"
            );
            assert_eq!(
                superseded_timing_seq,
                Some(0),
                "a physically addressed registrant's earlier this-pass observation is \
                     fully superseded by this one, per round 3/5's established rule"
            );
        }
        _ => panic!("expected Registrant binding"),
    }
    assert_eq!(
        entry.registrants[0].concat.len(),
        1,
        "still just one buffer -- absorption, not a second buffer"
    );
    let (seq, change) = entry.registrants[0]
        .pending_timing_change
        .as_ref()
        .expect("the second response's own ComParam side effect must be captured");
    assert_eq!(*seq, 1, "must reflect the LATEST response, not the first");
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [99, 98, 97, 96, 95]
        })
    );
}

/// Sibling of the test above, covering the OTHER concat absorb arm:
/// `bind_registrant`'s "open new buffer" arm's own existing-buffer
/// lookup (edge-case-hunter finding, follow-up verification of the
/// merge fix, PR #17) -- reached when a registrant has BOTH a vacuous
/// AND a non-vacuous descriptor (mirroring
/// `concat_open_new_buffer_arm_absorbs_into_existing_same_key_buffer_instead_of_duplicating`'s
/// own shape), so a same-key second response is absorbed via the
/// non-vacuous descriptor's full match rather than the continuation
/// fast path. This call site was left untested by the sibling test
/// above, which only ever exercises the fast path.
///
/// Fail-without-the-fix control: removing the
/// `observe_registrant_timing_change` call from this specific arm
/// (`bind_registrant`'s full-match absorb branch, distinct from the
/// continuation fast path above it) makes this test fail; restoring it
/// makes it pass. Verified independently by reverting only this call
/// site (leaving the fast path's own call untouched) and re-running.
#[test]
fn timing_change_updates_when_absorbed_via_the_full_match_arm() {
    let physical_cfg = TimingChangeConfig::KwpAccess(AccessTimingConfig {
        tpi: Some(2),
        tpi3_request_bytes: None,
        default_timing: None,
        override_timing: None,
        functional: false,
    });
    let mut registrants = vec![CopRegistrant {
        expected: vec![vacuous_expected(55), non_vacuous_expected(0xC3, 88)],
        matches_needed: Some(1),
        concat_enabled: true,
        timing_cfg: Some(physical_cfg),
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // Opens via the vacuous descriptor (Tier1Vacuous scan).
    bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x02, 10, 20, 30, 40, 50],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(registrants[0].concat.len(), 1);

    // Same-key second response, scanned Tier1NonVacuous: the fast path
    // declines (scan-gated -- the buffer was opened vacuous), but R's
    // OWN non-vacuous descriptor matches this payload, landing in the
    // "open new buffer" arm's existing-buffer lookup -- absorbed there,
    // not via a second, duplicate buffer.
    let result = bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x02, 99, 98, 97, 96, 95],
        0,
        AttributionScan::Tier1NonVacuous,
        1,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        result,
        Some((
            55,
            100,
            true,
            Some(0),
            true,
            Some(QueueErrorClass::Positive)
        )),
        "acceptance_id must be the BUFFER's own (55, from its opening vacuous descriptor, \
             per ADR-148's own invariant), not the non-vacuous descriptor (88) that merely \
             identified this frame as belonging to the same buffer; still paired against \
             timing_cfg and superseding seq 0"
    );
    assert_eq!(registrants[0].concat.len(), 1, "still just one buffer");
    let (seq, change) = registrants[0]
        .pending_timing_change
        .as_ref()
        .expect("the second response's own ComParam side effect must be captured");
    assert_eq!(*seq, 1, "must reflect the LATEST response, not the first");
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [99, 98, 97, 96, 95]
        })
    );
}

/// Required test 2: TPI=2 with two simulated functional responses whose
/// P2Min/P2Max values differ in opposite directions -- the combined
/// result takes min(P2Min) and max(P2Max), not a uniform max (ADR-146
/// Decision, "Functional-addressing worst case").
#[test]
fn timing_change_tpi2_combines_functional_responses_min_and_max() {
    let mut registrants = vec![CopRegistrant {
        timing_cfg: Some(timing_cfg_tpi(2)),
        matches_needed: Some(2),
        ..registrant(100, 1)
    }];

    // First responder: P2Min=10, P2Max=20.
    bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x02, 10, 20, 30, 40, 50],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    // Second responder: P2Min=5 (smaller), P2Max=60 (larger).
    bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x02, 5, 60, 20, 45, 55],
        0,
        AttributionScan::Tier1Vacuous,
        1,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(
        registrants[0].timing_accumulator,
        Some(TimingAccumulator::Kwp([5, 60, 30, 45, 55])),
        "P2Min takes the minimum (5), P2Max/P3Min/P3Max/P4Min take the maximum"
    );
    let (seq, change) = registrants[0].pending_timing_change.as_ref().unwrap();
    assert_eq!(*seq, 1);
    assert!(
        change
            .derived
            .contains(&(ComParamId(j2534_0404::P2_MIN), 5 * 500))
    );
    assert!(
        change
            .derived
            .contains(&(ComParamId(j2534_0404::P2_MAX), 60 * 500))
    );
    // Codex review, PR #17: CP_AccessTiming_Ecu's TimingSet=1 entry must
    // record the combined worst-case bytes, not just the last
    // individual responder's own bytes -- otherwise a later TPI=1
    // reapply would undo the safe combined timing with one ECU's
    // potentially less-conservative values.
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [5, 60, 30, 45, 55]
        }),
        "ecu_entry must record the combined worst-case bytes, not just the last responder's"
    );
}

/// Codex review, PR #17, round 3: a PHYSICALLY addressed COP has exactly
/// one responder, so a second TPI=2 response (e.g. re-reading active
/// timing after a prior TPI=3 changed it) must reflect that ECU's
/// CURRENT values, not a min/max fold against the first response --
/// folding here would treat the same ECU's own before/after values as if
/// they were two distinct ECUs in a functional exchange.
#[test]
fn timing_change_tpi2_physical_addressing_does_not_combine_across_responses() {
    let mut registrants = vec![CopRegistrant {
        timing_cfg: Some(TimingChangeConfig::KwpAccess(AccessTimingConfig {
            functional: false,
            ..access_timing_cfg(2)
        })),
        matches_needed: Some(2),
        ..registrant(100, 1)
    }];

    // First response: P2Min=10, P2Max=20.
    bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x02, 10, 20, 30, 40, 50],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    // Second response, SAME (physical) ECU: P2Min=5, P2Max=60 -- its new
    // current values, not a second distinct responder.
    bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x02, 5, 60, 20, 45, 55],
        0,
        AttributionScan::Tier1Vacuous,
        1,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );

    let (seq, change) = registrants[0].pending_timing_change.as_ref().unwrap();
    assert_eq!(*seq, 1);
    assert!(
        change
            .derived
            .contains(&(ComParamId(j2534_0404::P2_MIN), 5 * 500)),
        "must reflect the second response's own P2Min, not a min-combine with the first"
    );
    assert!(
        change
            .derived
            .contains(&(ComParamId(j2534_0404::P2_MAX), 60 * 500))
    );
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [5, 60, 20, 45, 55]
        }),
        "ecu_entry must record the second response's own bytes verbatim, un-combined"
    );
}

/// Codex review, PR #17, round 6: an IS-CYCLIC (`NumReceiveCycles == -1`)
/// registrant's tier flips to `ReceiveOnly` INLINE, within
/// `bind_registrant`, the instant its first match is accepted --  but
/// `migrate_registrant_to_receive_only`'s own `timing_cfg = None` clear
/// (mirroring `rc_cfg`) only runs once the WHOLE poll pass has already
/// returned. Without also clearing `timing_cfg` at the INLINE flip
/// itself, a second qualifying `0xC3` response for the SAME registrant
/// later in the SAME `PassThruReadMsgs` batch would still bind (via the
/// `Tier2` scan, since tier already flipped) and re-enter the ADR-146
/// pairing block, making hardware/ComParam updates depend on adapter
/// batching -- directly contradicting this mechanism's own "tier-2
/// never runs it" invariant (ADR-146 Decision, "Detection and pairing").
/// The registrant's own migration-triggering frame (the first response)
/// must still get full ADR-146 handling; only the SECOND, tier-2-scanned
/// frame is affected.
#[test]
fn timing_change_migrating_registrant_only_handles_the_migration_triggering_frame() {
    let mut registrants = vec![CopRegistrant {
        timing_cfg: Some(timing_cfg_tpi(2)),
        migrate_on_first_match: true,
        matches_needed: None, // IS-CYCLIC: unbounded, like a real `NumReceiveCycles == -1` registrant.
        ..registrant(100, 1)
    }];

    // First response: the migration-triggering frame -- accepted via the
    // still-tier-1 scan, flips tier to ReceiveOnly INLINE, and must still
    // get full ADR-146 handling for THIS frame.
    let first = bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x02, 10, 20, 30, 40, 50],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert_eq!(
        first.map(|(_, _, ecu_timing_change, _, _, _)| ecu_timing_change),
        Some(true),
        "the migration-triggering frame itself must still be fully handled"
    );
    assert_eq!(registrants[0].tier, RegistrantTier::ReceiveOnly);
    assert!(
        registrants[0].timing_cfg.is_none(),
        "timing_cfg must already be cleared right after the migration-triggering frame's \
             own ADR-146 handling, not deferred to end-of-pass migrate_registrant_to_receive_only"
    );

    // Second response, SAME registrant, SAME batch -- now binds via the
    // Tier2 scan (tier already flipped). Must be fully inert: no
    // ecu_timing_change, and pending_timing_change must still reflect
    // only the first (migration-triggering) frame's own values.
    let second = bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x02, 5, 60, 20, 45, 55],
        0,
        AttributionScan::Tier2,
        1,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert_eq!(
        second.map(|(_, _, ecu_timing_change, _, _, _)| ecu_timing_change),
        Some(false),
        "a later frame in the same batch must not re-enter ADR-146 handling once migrated"
    );
    let (seq, change) = registrants[0].pending_timing_change.as_ref().unwrap();
    assert_eq!(
        *seq, 0,
        "pending_timing_change must still be the migration-triggering frame's own (seq 0), \
             untouched by the second, now-inert frame"
    );
    assert!(
        change
            .derived
            .contains(&(ComParamId(j2534_0404::P2_MIN), 10 * 500)),
        "must retain the first frame's own P2Min, not the second frame's"
    );
}

/// Codex review, PR #17, round 4: two DIFFERENT registrants on the SAME
/// CLL, each independently qualifying via its OWN SID 0x83 exchange
/// (routed here by distinct `unique_resp_ids`/`unique_resp_identifier`
/// rather than a shared functional broadcast), must each have `bind_
/// registrant`'s `frame_seq` parameter threaded through into their OWN
/// `pending_timing_change` untouched by the other's. The actual
/// last-arrival-wins FOLD across registrants is covered separately by
/// `select_latest_timing_changes_lets_the_later_change_win`; this test
/// only proves the sequence number is captured and stored correctly
/// per-registrant, end-to-end through `bind_registrant`.
#[test]
fn bind_registrant_threads_frame_seq_through_pending_timing_change_per_registrant() {
    let mut registrants = vec![
        CopRegistrant {
            expected: vec![ExpectedResponse {
                mask: Vec::new(),
                pattern: Vec::new(),
                unique_resp_ids: vec![10],
                acceptance_id: 0,
            }],
            timing_cfg: Some(TimingChangeConfig::KwpAccess(AccessTimingConfig {
                tpi: Some(3),
                tpi3_request_bytes: Some([1, 2, 3, 4, 5]),
                default_timing: None,
                override_timing: None,
                functional: false,
            })),
            ..registrant(100, 1)
        },
        CopRegistrant {
            expected: vec![ExpectedResponse {
                mask: Vec::new(),
                pattern: Vec::new(),
                unique_resp_ids: vec![20],
                acceptance_id: 0,
            }],
            timing_cfg: Some(TimingChangeConfig::KwpAccess(AccessTimingConfig {
                tpi: Some(3),
                tpi3_request_bytes: Some([9, 8, 7, 6, 5]),
                default_timing: None,
                override_timing: None,
                functional: false,
            })),
            ..registrant(101, 1)
        },
    ];

    // Registrant A's (cop_handle 100) own independent exchange qualifies
    // first in this pass, at frame_seq=0.
    let binding_a = bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x03],
        10,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert_eq!(
        binding_a.map(|(_, cop_handle, _, _, _, _)| cop_handle),
        Some(100)
    );

    // Registrant B's (cop_handle 101) own, wholly INDEPENDENT exchange
    // qualifies later in this SAME pass, at frame_seq=3.
    let binding_b = bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x03],
        20,
        AttributionScan::Tier1Vacuous,
        3,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert_eq!(
        binding_b.map(|(_, cop_handle, _, _, _, _)| cop_handle),
        Some(101)
    );

    let (seq_a, change_a) = registrants[0].pending_timing_change.as_ref().unwrap();
    assert_eq!(*seq_a, 0, "registrant A's own frame_seq must be recorded");
    assert_eq!(
        change_a.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 3,
            bytes: [1, 2, 3, 4, 5]
        })
    );

    let (seq_b, change_b) = registrants[1].pending_timing_change.as_ref().unwrap();
    assert_eq!(
        *seq_b, 3,
        "registrant B's own frame_seq must be recorded, independent of A's"
    );
    assert_eq!(
        change_b.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 3,
            bytes: [9, 8, 7, 6, 5]
        })
    );
}

/// ADR-146 (edge-case-hunter finding): two ISO14230 CLLs at the same
/// `(j2534_protocol_id, baud_rate)` can share one physical channel, so
/// more than one tier-1 registrant can independently observe the same
/// physical `0xC3` frame in a single `poll_rx_inner` pass.
/// `combine_derived_timing_value` must apply the same worst-case
/// direction table as `combine_worst_case_timing` (min for `CP_P2Min`,
/// max for everything else) so the single merged hardware push is
/// well-defined regardless of which registrant's observation is folded
/// in first, rather than a later independent push silently clobbering
/// an earlier one.
#[test]
fn combine_derived_timing_value_uses_the_same_direction_table_as_worst_case_combine() {
    assert_eq!(
        combine_derived_timing_value(ComParamId(j2534_0404::P2_MIN), 10 * 500, 5 * 500),
        5 * 500,
        "CP_P2Min takes the minimum"
    );
    assert_eq!(
        combine_derived_timing_value(ComParamId(j2534_0404::P2_MIN), 5 * 500, 10 * 500),
        5 * 500,
        "CP_P2Min takes the minimum regardless of argument order"
    );
    for id in [
        ComParamId(j2534_0404::P2_MAX),
        ComParamId(j2534_0404::P3_MIN),
        PARAM_P2_STAR,
        ComParamId(j2534_0404::P4_MIN),
    ] {
        assert_eq!(
            combine_derived_timing_value(id, 20 * 500, 60 * 500),
            60 * 500,
            "{id:?} takes the maximum"
        );
    }
}

/// Required test 3: TPI=2 with `CP_AccessTimingOverride` non-empty --
/// `CP_AccessTiming_Ecu` still records the ECU's actual observed bytes,
/// while the derived ComParams come from the override (ADR-146 Decision,
/// TPI=2 bullet).
#[test]
fn timing_change_tpi2_override_redirects_derived_but_not_ecu_entry() {
    let mut registrants = vec![CopRegistrant {
        timing_cfg: Some(TimingChangeConfig::KwpAccess(AccessTimingConfig {
            tpi: Some(2),
            tpi3_request_bytes: None,
            default_timing: None,
            override_timing: Some([1, 2, 3, 4, 5]),
            functional: false,
        })),
        ..registrant(100, 1)
    }];

    let binding = bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x02, 10, 20, 30, 40, 50],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert!(binding.is_some());
    let (_, change) = registrants[0].pending_timing_change.as_ref().unwrap();
    assert!(
        change
            .derived
            .contains(&(ComParamId(j2534_0404::P2_MIN), 500)),
        "derived ComParams must come from the override, not the observed bytes"
    );
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [10, 20, 30, 40, 50]
        }),
        "CP_AccessTiming_Ecu must still record the ECU's own observed bytes"
    );
}

/// Required test 4: a TPI=3 positive response carries no timing payload
/// of its own -- the derived ComParams come from the ORIGINAL REQUEST's
/// captured 5 bytes, and `CP_AccessTimingOverride` does not apply here
/// even when non-empty (ADR-146 Decision, TPI=3 bullet).
#[test]
fn timing_change_tpi3_derives_from_original_request_bytes_ignoring_override() {
    let mut registrants = vec![CopRegistrant {
        timing_cfg: Some(TimingChangeConfig::KwpAccess(AccessTimingConfig {
            tpi: Some(3),
            tpi3_request_bytes: Some([9, 8, 7, 6, 5]),
            default_timing: None,
            override_timing: Some([1, 2, 3, 4, 5]), // must be ignored for TPI=3
            functional: false,
        })),
        ..registrant(100, 1)
    }];

    // TPI=3's positive response carries no timing bytes at all.
    let binding = bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x03],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert!(binding.is_some());
    let (_, change) = registrants[0].pending_timing_change.as_ref().unwrap();
    assert!(
        change
            .derived
            .contains(&(ComParamId(j2534_0404::P2_MIN), 9 * 500)),
        "derived ComParams must come from the request's own captured bytes"
    );
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 3,
            bytes: [9, 8, 7, 6, 5]
        })
    );
}

/// Required test 5: `CP_ModifyTiming` disabled (`timing_cfg: None`,
/// mirroring what `TimingChangeConfig::from_params` returns when the
/// ComParam is 0) -- a passing `83`/`C3` exchange causes zero state
/// changes; the frame still binds as an ordinary response (ADR-146:
/// fully inert).
#[test]
fn timing_change_disabled_leaves_matching_response_fully_inert() {
    let mut registrants = vec![CopRegistrant {
        timing_cfg: None,
        ..registrant(100, 1)
    }];

    let binding = bind_registrant(
        &mut registrants,
        1,
        &[0xC3, 0x02, 10, 20, 30, 40, 50],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert!(
        binding.is_some(),
        "the frame must still bind normally as an ordinary response"
    );
    assert!(registrants[0].pending_timing_change.is_none());
    assert_eq!(registrants[0].timing_accumulator, None);
}

/// Required test 6: `rx_flag_bytes`'s `ECU_TIMING_CHANGE` bit (byte 1,
/// bit 1) is set only when `ecu_timing_change` is `true`, and never
/// collides with the existing byte-3 native-flag encoding (ADR-098/ADR-146).
#[test]
fn rx_flag_bytes_ecu_timing_change_bit_set_only_when_true_and_does_not_collide() {
    let extras = |ecu_timing_change: bool| RxFlagExtras {
        ecu_timing_change,
        sw_can_hv_rx: false,
    };
    assert_eq!(rx_flag_bytes(0, extras(false)), Vec::<u8>::new());
    assert_eq!(rx_flag_bytes(0, extras(true)), vec![0x00, 0x02, 0x00, 0x00]);
    assert_eq!(
        rx_flag_bytes(0x02, extras(false)),
        vec![0x00, 0x00, 0x00, 0x02],
        "an existing byte-3 native flag alone must not set byte 1"
    );
    assert_eq!(
        rx_flag_bytes(0x02, extras(true)),
        vec![0x00, 0x02, 0x00, 0x02],
        "both bytes must be independently set with no collision"
    );
}

/// ADR-191: `rx_flag_bytes`'s `sw_can_hv_rx` bit (byte 1, bit 0) is set only
/// when `RxFlagExtras::sw_can_hv_rx` is `true`, independently of
/// `ecu_timing_change` (byte 1, bit 1) and the byte-3 native-flag encoding.
#[test]
fn rx_flag_bytes_sw_can_hv_rx_bit_set_only_when_true_and_does_not_collide() {
    assert_eq!(
        rx_flag_bytes(
            0,
            RxFlagExtras {
                ecu_timing_change: false,
                sw_can_hv_rx: false,
            }
        ),
        Vec::<u8>::new()
    );
    assert_eq!(
        rx_flag_bytes(
            0,
            RxFlagExtras {
                ecu_timing_change: false,
                sw_can_hv_rx: true,
            }
        ),
        vec![0x00, 0x01, 0x00, 0x00]
    );
    assert_eq!(
        rx_flag_bytes(
            0x02,
            RxFlagExtras {
                ecu_timing_change: true,
                sw_can_hv_rx: true,
            }
        ),
        vec![0x00, 0x03, 0x00, 0x02],
        "both byte-1 bits must be independently set with no collision"
    );
}
// ── CP_EnableConcatenation (ADR-148, amended) ──

/// A concat-eligible registrant absorbs same-key (same URID/SID)
/// segments into one open buffer instead of delivering each one
/// separately: `matches_got` stays 0, `concat_segments_got` counts every
/// absorbed segment, and each call reports `absorbed: true`.
#[test]
fn concat_absorbs_same_key_segments_without_advancing_matches_got() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(1),
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    let first = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA, 0xBB],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        first,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert!(finalized.is_empty());
    assert_eq!(registrants[0].matches_got, 0);
    assert_eq!(registrants[0].concat_segments_got, 1);
    assert_eq!(registrants[0].concat.len(), 1, "exactly one buffer open");
    let buf = &registrants[0].concat[0];
    assert_eq!(buf.key, (0, None, 0x62));
    assert_eq!(buf.data, vec![0x62, 0xAA, 0xBB]);

    let second = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xCC],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        second,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert!(finalized.is_empty());
    assert_eq!(registrants[0].matches_got, 0);
    assert_eq!(registrants[0].concat_segments_got, 2);
    assert_eq!(
        registrants[0].concat[0].data,
        vec![0x62, 0xAA, 0xBB, 0xCC],
        "the second segment's SID (payload[0]) must not be duplicated in the merge"
    );
}

/// ADR-148 Amendment 10 (Codex round-14 finding on PR #18) -- the actual
/// protocol-correctness guarantee the fix delivers, pinned at the
/// `bind_registrant` level: once
/// `discard_concat_buffers_for_retransmit_if_live` has cleared a
/// registrant's open buffers (simulated here directly, matching exactly
/// what that helper does -- `r.concat.clear()`, nothing else), a
/// same-key (same URID/SID) frame arriving afterward must NOT be
/// absorbed by the continuation fast path as though it continued the
/// pre-retry buffer. With no open buffer left to key-match against, the
/// frame falls through to the normal "open a fresh buffer" arm instead
/// -- exactly the outcome that prevents bytes from a post-retransmit
/// response being silently merged onto a pre-retransmit partial (the
/// corruption Codex's finding described).
#[test]
fn discard_then_same_key_frame_opens_a_new_buffer_instead_of_continuing_the_old_one() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(1),
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // Pre-retry: two segments accumulate into one open buffer, same as
    // the sibling `concat_absorbs_same_key_segments_without_advancing_matches_got`
    // test above.
    let first = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA, 0xBB],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        first,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(registrants[0].concat.len(), 1, "one buffer open pre-retry");
    assert_eq!(registrants[0].concat[0].data, vec![0x62, 0xAA, 0xBB]);
    let segments_got_before_discard = registrants[0].concat_segments_got;

    // Simulate the RC21/23 retransmit boundary:
    // `discard_concat_buffers_for_retransmit_if_live` clearing this
    // registrant's `concat` and nothing else.
    registrants[0].concat.clear();

    // Post-retry: a fresh response from the SAME ECU/SID arrives. Absent
    // the fix, `bind_registrant`'s continuation fast path would have
    // nothing to key-match (the buffer is gone), so it must open a
    // genuinely NEW buffer rather than resuming the discarded one.
    let after_retry = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0x11, 0x22],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        after_retry,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(
        registrants[0].concat.len(),
        1,
        "exactly one (freshly opened) buffer, not a merge onto the discarded one"
    );
    assert_eq!(
        registrants[0].concat[0].data,
        vec![0x62, 0x11, 0x22],
        "the new buffer must hold ONLY the post-retry response bytes -- no trace of the \
             discarded pre-retry partial (0xAA, 0xBB) must survive"
    );
    assert_eq!(
        registrants[0].concat_segments_got,
        segments_got_before_discard + 1,
        "concat_segments_got keeps advancing normally for the new buffer's own segment"
    );
}

/// ADR-148 Amendment 5's precondition, pinned at the `bind_registrant`
/// level (design-advisor confirmed this mechanics already works
/// correctly today -- this test is expected to pass without the
/// Amendment 5 fix itself, which lives in
/// `observe_and_consume_pending_rc_outcome`): one batch containing both a
/// same-registrant absorbable continuation and a coexisting pending-RC
/// frame (from a different ECU, `unique_resp_identifier` 9 vs 0) must
/// advance `concat_segments_got` AND set `pending_rc`, neither one
/// clobbering the other -- the absorb-vs-RC-detect ordering at this
/// layer does not itself lose data; the bug this Amendment fixes was
/// entirely in the later observe-and-consume step.
#[test]
fn concat_absorb_and_pending_rc_coexist_in_one_batch() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(2),
        concat_enabled: true,
        rc_cfg: Some(RcHandlingConfig {
            rc_byte_offset: 2,
            rc78_handling: true,
            request_sid: Some(0x22),
            ..Default::default()
        }),
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // First frame: a continuation segment for this registrant's own open
    // (in fact, just-opened) concat buffer.
    let absorbed = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        absorbed,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(registrants[0].concat_segments_got, 1);
    assert_eq!(
        registrants[0].pending_rc, None,
        "the absorb must not itself set a pending RC"
    );

    // Second frame: a pending NRC from a DIFFERENT ECU
    // (`unique_resp_identifier` 9), for the same registrant.
    let rc_bound = bind_registrant(
        &mut registrants,
        1,
        &[0x7F, 0x22, 0x78],
        9,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        rc_bound,
        Some((0, 100, false, None, false, None)),
        "pending-RC detection claims the frame with acceptance_id 0, absorbed: false"
    );

    assert_eq!(
        registrants[0].concat_segments_got, 1,
        "the earlier absorb must survive the later pending-RC detection"
    );
    assert_eq!(
        registrants[0].pending_rc,
        Some(0x78),
        "the pending RC must be set, not clobbered by the earlier absorb"
    );
    assert_eq!(
        registrants[0].matches_got, 0,
        "neither event is a completed match"
    );
}

/// Backward-compatibility regression guard: with `concat_enabled: false`
/// (the default, mirroring `CP_EnableConcatenation` left at 0), each
/// same-signature frame is still delivered/matched separately -- the
/// existing byte-for-byte behavior, unaffected by this feature.
#[test]
fn concat_disabled_registrant_matches_each_segment_separately() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(2),
        concat_enabled: false,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    let first = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        first,
        Some((77, 100, false, None, false, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(registrants[0].matches_got, 1);
    assert!(registrants[0].concat.is_empty());

    let second = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xBB],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        second,
        Some((77, 100, false, None, false, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(registrants[0].matches_got, 2);
}

/// ADR-148 Amendment: a differing-key frame (different SID) arriving
/// while a buffer is open no longer force-finalizes anything -- it opens
/// its OWN fresh buffer (gated on quota) alongside the still-open old
/// one. Both buffers coexist, untouched, until the receive-phase
/// deadline finalizes them (`finalize_concat_buffers`, exercised by
/// separate tests).
#[test]
fn concat_differing_key_opens_a_new_buffer_without_finalizing_the_old_one() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![
            non_vacuous_expected(0x62, 77),
            non_vacuous_expected(0x6A, 78),
        ],
        matches_needed: Some(2),
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // First segment of SID 0x62's response.
    bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert!(finalized.is_empty());

    // A differing-SID frame (0x6A) arrives before the 0x62 buffer's
    // "final" segment ever does.
    let result = bind_registrant(
        &mut registrants,
        1,
        &[0x6A, 0xBB],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    assert_eq!(
        result,
        Some((78, 100, false, None, true, Some(QueueErrorClass::Positive))),
        "the new frame opens a fresh buffer of its own (quota allows a second buffer)"
    );
    assert!(
        finalized.is_empty(),
        "the amendment removes the differing-key force-finalize entirely -- nothing is \
             finalized by this call"
    );
    assert_eq!(
        registrants[0].matches_got, 0,
        "no buffer has been finalized, so no match has been counted yet"
    );
    assert_eq!(
        registrants[0].concat.len(),
        2,
        "both buffers must now coexist, neither one finalized"
    );
    assert_eq!(
        registrants[0].concat[0].key,
        (0, None, 0x62),
        "the original SID-0x62 buffer must still be present, in its original arrival slot"
    );
    assert_eq!(
        registrants[0]
            .concat
            .iter()
            .find(|b| b.key == (0, None, 0x62))
            .unwrap()
            .data,
        vec![0x62, 0xAA],
        "the old buffer's contents must be untouched"
    );
    assert_eq!(
        registrants[0]
            .concat
            .iter()
            .find(|b| b.key == (0, None, 0x6A))
            .unwrap()
            .data,
        vec![0x6A, 0xBB],
        "the new buffer must hold the differing-key frame's own data"
    );
}

/// ADR-148 Amendment / brief point 6: with a finite quota already fully
/// committed to the single open buffer (`matches_needed: Some(1)`, one
/// buffer already open), a differing-key frame must NOT open a second
/// buffer (opening one would let `matches_got + concat.len()` reach 2,
/// exceeding the quota of 1) -- it is simply not absorbed, and critically
/// the original buffer is neither finalized nor otherwise disturbed; it
/// is held open until the deadline, exactly like every other case now.
#[test]
fn concat_differing_key_with_finite_quota_is_held_open_not_finalized() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![
            non_vacuous_expected(0x62, 77),
            non_vacuous_expected(0x6A, 78),
        ],
        matches_needed: Some(1), // only one logical match total
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    let result = bind_registrant(
        &mut registrants,
        1,
        &[0x6A, 0xBB],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    assert_eq!(
        result, None,
        "quota does not allow a second buffer -- this frame is not absorbed"
    );
    assert!(
        finalized.is_empty(),
        "no finalize happens here any more -- a differing key is never a finalize trigger"
    );
    assert_eq!(registrants[0].matches_got, 0);
    assert_eq!(
        registrants[0].concat.len(),
        1,
        "the original buffer must still be open, alone"
    );
    assert_eq!(
        registrants[0].concat[0].data,
        vec![0x62, 0xAA],
        "the original buffer's contents must be completely intact"
    );
}

/// Guard: an empty payload is never treated as a continuation, even for
/// a concat-enabled registrant -- it falls through to the normal
/// (non-concat) match-acceptance path.
#[test]
fn concat_empty_payload_is_never_treated_as_a_continuation() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![
            non_vacuous_expected(0x62, 77),
            non_vacuous_expected(0x6A, 77),
        ],
        matches_needed: Some(1),
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    let result = bind_registrant(
        &mut registrants,
        1,
        &[],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    assert_eq!(
        result,
        Some((77, 100, false, None, false, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(registrants[0].matches_got, 1);
    assert!(registrants[0].concat.is_empty());
}

/// edge-case-hunter Finding 2 regression: a genuine continuation
/// segment's bytes past the SID are new data, not a repeat of the first
/// segment's payload -- a descriptor whose mask/pattern constrains more
/// than byte 0 must still absorb it. With the pre-fix code (matching
/// the full descriptor BEFORE checking the open buffer's key) this
/// second frame would fail `ExpectedResponse::matches` (byte 1 is
/// `0x99`, not the configured `0xF1`) and never be absorbed at all.
#[test]
fn concat_absorbs_a_continuation_even_when_it_fails_the_full_descriptor_match() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![ExpectedResponse {
            mask: vec![0xFF, 0xFF],
            pattern: vec![0x62, 0xF1],
            unique_resp_ids: Vec::new(),
            acceptance_id: 77,
        }],
        matches_needed: Some(1),
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    let first = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xF1, 0xAA],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        first,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert!(finalized.is_empty());

    // A real continuation: SID 0x62 matches the open buffer's key, but
    // byte 1 (`0x99`) does NOT match the configured descriptor's
    // pattern (`0xF1`) -- it must still be absorbed.
    let second = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0x99],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        second,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive))),
        "a genuine continuation must be absorbed even when its own bytes fail the full \
             descriptor match"
    );
    assert!(finalized.is_empty());
    assert_eq!(registrants[0].matches_got, 0);
    assert_eq!(registrants[0].concat_segments_got, 2);
    assert_eq!(registrants[0].concat[0].data, vec![0x62, 0xF1, 0xAA, 0x99]);
}

/// edge-case-hunter Finding 3 regression, extended by the ADR-148
/// Amendment: an empty-payload frame that matches via
/// `ExpectedResponse::matches`'s `cmp_len == 0` short-circuit must
/// force-finalize every still-open buffer FIRST -- otherwise the phase
/// can complete right here with the open buffer's accumulated data
/// silently discarded instead of delivered. This is still the ONE place
/// (besides the receive-phase deadline) where a finalize can happen.
/// `matches_needed: Some(2)` here so quota still has room after the
/// finalize consumes one slot -- the empty-payload frame is THEN also
/// accepted as its own (separate) match, same as the pre-amendment
/// behavior in this non-exhausted case.
#[test]
fn concat_empty_payload_match_finalizes_all_open_buffers_first_instead_of_dropping_them() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(2),
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    let first = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA, 0xBB],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        first,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(registrants[0].concat.len(), 1);

    // `non_vacuous_expected`'s own descriptor still matches an empty
    // payload via `matches`'s `cmp_len == 0` short-circuit (`data.len()
    // == 0` forces `cmp_len` to 0 regardless of mask/pattern length).
    let second = bind_registrant(
        &mut registrants,
        1,
        &[],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        second,
        Some((77, 100, false, None, false, Some(QueueErrorClass::Positive))),
        "quota still has room after the finalize (needed=2), so the empty-payload frame is \
             also accepted as its own match"
    );
    assert_eq!(
        finalized.len(),
        1,
        "the open buffer must be force-finalized and delivered, not silently discarded"
    );
    assert_eq!(finalized[0].data, vec![0x62, 0xAA, 0xBB]);
    assert!(registrants[0].concat.is_empty());
    assert_eq!(
        registrants[0].matches_got, 2,
        "the finalize's own +1 plus the empty-payload frame's own +1"
    );
}

/// Companion to the above: when the finalize(s) triggered by the
/// empty-payload match ALONE already meet `matches_needed`
/// (`Some(1)`, one open buffer), the empty-payload frame itself is NOT
/// also accepted as a separate match -- accepting it too would push
/// `matches_got` past `matches_needed`. `bind_registrant` returns `None`
/// for this call, while `finalized` still carries the just-finalized
/// buffer.
#[test]
fn concat_empty_payload_match_not_also_counted_when_finalize_alone_meets_quota() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(1),
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA, 0xBB],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    let second = bind_registrant(
        &mut registrants,
        1,
        &[],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        second, None,
        "quota (1) is already met by the finalize alone -- the empty-payload frame must not \
             ALSO be counted as its own match"
    );
    assert_eq!(
        finalized.len(),
        1,
        "the buffer must still be finalized and handed back for delivery"
    );
    assert_eq!(finalized[0].data, vec![0x62, 0xAA, 0xBB]);
    assert_eq!(registrants[0].matches_got, 1);
    assert!(registrants[0].concat.is_empty());
}

/// edge-case-hunter Finding 5 (RC frame): an RC21/RC23/RC78
/// pending-response-code frame arriving mid-accumulation must go
/// through the existing pending-RC path (extending the RC deadline) and
/// must NOT be treated as a concat continuation -- the open buffer is
/// left completely untouched.
#[test]
fn concat_pending_rc_frame_mid_accumulation_does_not_touch_the_open_buffer() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(2),
        concat_enabled: true,
        rc_cfg: Some(RcHandlingConfig {
            rc_byte_offset: 2,
            rc78_handling: true,
            request_sid: Some(0x62),
            ..Default::default()
        }),
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    let first = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        first,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );

    // `7F 62 78`: a pending-response (NRC 0x78) frame for this COP's
    // own request SID.
    let rc = bind_registrant(
        &mut registrants,
        1,
        &[0x7F, 0x62, 0x78],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        rc,
        Some((0, 100, false, None, false, None)),
        "a pending-RC frame must go through the existing pending-RC path, not the concat path"
    );
    assert_eq!(registrants[0].pending_rc, Some(0x78));
    assert!(finalized.is_empty());
    assert_eq!(
        registrants[0].concat[0].data,
        vec![0x62, 0xAA],
        "the open buffer must be left completely untouched by the pending-RC frame"
    );
    assert_eq!(registrants[0].concat_segments_got, 1);
}

/// edge-case-hunter Finding 5 (1-byte continuation): a SID-only segment
/// (no data bytes past the SID) as a continuation frame must not panic
/// on `payload[1..]` and must correctly contribute zero extra bytes.
#[test]
fn concat_absorbs_a_sid_only_one_byte_continuation_without_panicking() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(1),
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    let second = bind_registrant(
        &mut registrants,
        1,
        &[0x62],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    assert_eq!(
        second,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(registrants[0].concat_segments_got, 2);
    assert_eq!(
        registrants[0].concat[0].data,
        vec![0x62, 0xAA],
        "a 1-byte SID-only continuation must contribute zero extra bytes"
    );
}

/// edge-case-hunter Finding 5 (IS-MULTIPLE), rewritten for the ADR-148
/// Amendment with GENUINELY interleaved segments (A1, B1, A2, B2) rather
/// than fully-separated batches -- the interleaving is exactly the
/// scenario the pre-amendment "sole open buffer" bug mishandled. With
/// `matches_needed: None` (IS-MULTIPLE, `NumReceiveCycles == -2`) and two
/// ECUs replying with differing `unique_resp_identifier`, each ECU's
/// segments must accumulate into its OWN buffer, keyed off its own URID,
/// with BOTH buffers coexisting (no early finalize) until the deadline.
#[test]
fn concat_is_multiple_keeps_each_ecus_segments_in_its_own_buffer_by_urid() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: None, // IS-MULTIPLE: unbounded match count
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // A1: ECU 1 (URID 1)'s first segment.
    bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA],
        1,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    // B1: ECU 2 (URID 2)'s first segment, interleaved BEFORE ECU 1's
    // second segment ever arrives.
    let b1 = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xCC],
        2,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        b1,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive))),
        "ECU 2's first segment must open its own fresh buffer, not disturb ECU 1's"
    );
    assert!(
        finalized.is_empty(),
        "no finalize may happen just because a differing-URID frame arrived"
    );
    // A2: ECU 1's second segment.
    bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xBB],
        1,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    // B2: ECU 2's second segment.
    bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xDD],
        2,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    assert!(
        finalized.is_empty(),
        "still no finalize -- both buffers stay open through the whole interleaved burst"
    );
    assert_eq!(
        registrants[0].concat.len(),
        2,
        "both ECUs' buffers must coexist, one per URID"
    );
    assert_eq!(registrants[0].concat_segments_got, 4);
    assert_eq!(
        registrants[0]
            .concat
            .iter()
            .find(|b| b.unique_resp_identifier == 1)
            .expect("ECU 1's buffer must be present")
            .data,
        vec![0x62, 0xAA, 0xBB],
        "ECU 1's buffer must contain only its own (correctly ordered) segments"
    );
    assert_eq!(
        registrants[0]
            .concat
            .iter()
            .find(|b| b.unique_resp_identifier == 2)
            .expect("ECU 2's buffer must be present")
            .data,
        vec![0x62, 0xCC, 0xDD],
        "ECU 2's buffer must contain only its own (correctly ordered) segments"
    );

    // Simulate the receive-phase deadline expiring: both buffers
    // finalize together, in one pass, in first-opened-first (arrival)
    // order.
    let deliveries = finalize_concat_buffers(&mut registrants[0]);
    assert_eq!(
        deliveries.len(),
        2,
        "both open buffers must finalize together on deadline expiry"
    );
    assert_eq!(deliveries[0].unique_resp_identifier, 1);
    assert_eq!(deliveries[0].data, vec![0x62, 0xAA, 0xBB]);
    assert_eq!(deliveries[1].unique_resp_identifier, 2);
    assert_eq!(deliveries[1].data, vec![0x62, 0xCC, 0xDD]);
    assert!(registrants[0].concat.is_empty());
}

/// Brief point 6 (finite `NumReceiveCycles = 2`): two ECUs' buffers both
/// open (quota allows exactly 2, one per ECU) and both finalize at once
/// on deadline expiry (`finalize_concat_buffers`) -- `matches_got` must
/// land at EXACTLY `matches_needed` (2), and the real deadline-expiry
/// call site's own completion check
/// (`matches_needed.is_some_and(|needed| matches_got >= needed)`,
/// `wait_for_expected_response_inner`) must evaluate to `CycleComplete`
/// afterward -- confirming the quota invariant
/// (`matches_got + concat.len() <= matches_needed`) holds correctly
/// through a multi-buffer finalize in one shot.
#[test]
fn concat_finite_quota_two_finalizes_at_once_meets_quota_exactly() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(2), // NumReceiveCycles == 2
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // ECU 1 (URID 1) and ECU 2 (URID 2) each open their own buffer --
    // quota (`matches_got + concat.len() < needed`) allows both: 0+0<2,
    // then 0+1<2.
    bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA],
        1,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xBB],
        2,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(registrants[0].concat.len(), 2);
    assert_eq!(registrants[0].matches_got, 0);

    // Simulated deadline expiry: both buffers finalize together.
    let deliveries = finalize_concat_buffers(&mut registrants[0]);
    assert_eq!(deliveries.len(), 2);
    assert_eq!(
        registrants[0].matches_got, 2,
        "matches_got must land at EXACTLY matches_needed, not overshoot or undershoot"
    );
    assert!(
        registrants[0]
            .matches_needed
            .is_some_and(|needed| registrants[0].matches_got >= needed),
        "the real deadline-expiry call site's own completion check must now read \
             CycleComplete"
    );
}

/// edge-case-hunter Finding 3 (round following the ADR-148 Amendment):
/// every prior IS-MULTIPLE test maxes out at exactly 2 concurrently-open
/// buffers. This exercises 3 distinct ECUs (URIDs 1/2/3) each sending 2
/// segments, genuinely interleaved on the wire (not batched -- each
/// ECU's first segment arrives before ANY ECU's second segment, then the
/// three second segments arrive in a different order again), confirming
/// the linear-scan-over-`Vec<ConcatBuf>` design (ADR-148 Amendment (b))
/// scales past 2 without cross-contaminating a third buffer, and that
/// deadline-expiry finalize-all (`finalize_concat_buffers`) drains and
/// delivers all 3 together with `matches_got` advancing by exactly 3.
#[test]
fn concat_three_concurrently_open_buffers_all_finalize_together() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(3), // NumReceiveCycles == 3
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // Round 1, interleaved arrival order ECU 2, ECU 3, ECU 1: each
    // opens its own fresh buffer (quota allows all 3: 0+0<3, 0+1<3,
    // 0+2<3).
    for urid in [2u32, 3, 1] {
        let result = bind_registrant(
            &mut registrants,
            1,
            &[0x62, 0xA0 + urid as u8],
            urid,
            AttributionScan::Tier1NonVacuous,
            0,
            ConcatFrameMeta::default(),
            &mut finalized,
            &mut false,
        );
        assert_eq!(
            result,
            Some((77, 100, false, None, true, Some(QueueErrorClass::Positive))),
            "ECU {urid}'s first segment must open its own fresh buffer"
        );
    }
    assert!(
        finalized.is_empty(),
        "no finalize may happen just because a differing-URID frame arrived"
    );
    assert_eq!(registrants[0].concat.len(), 3, "all 3 buffers must coexist");

    // Round 2, a DIFFERENT interleaved order (ECU 1, ECU 3, ECU 2) --
    // genuinely interleaved, not the same batching pattern as round 1.
    for urid in [1u32, 3, 2] {
        let result = bind_registrant(
            &mut registrants,
            1,
            &[0x62, 0xB0 + urid as u8],
            urid,
            AttributionScan::Tier1NonVacuous,
            0,
            ConcatFrameMeta::default(),
            &mut finalized,
            &mut false,
        );
        assert_eq!(
            result,
            Some((77, 100, false, None, true, Some(QueueErrorClass::Positive))),
            "ECU {urid}'s second segment must be absorbed into its OWN buffer, not any \
                 other ECU's"
        );
    }
    assert!(
        finalized.is_empty(),
        "still no finalize -- all 3 buffers stay open through the whole interleaved burst"
    );
    assert_eq!(
        registrants[0].concat.len(),
        3,
        "all 3 buffers must still coexist after round 2"
    );
    assert_eq!(registrants[0].concat_segments_got, 6);
    for urid in [1u32, 2, 3] {
        let expected_first = 0xA0 + urid as u8;
        let expected_second = 0xB0 + urid as u8;
        assert_eq!(
            registrants[0]
                .concat
                .iter()
                .find(|b| b.unique_resp_identifier == urid)
                .unwrap_or_else(|| panic!("ECU {urid}'s buffer must be present"))
                .data,
            vec![0x62, expected_first, expected_second],
            "ECU {urid}'s buffer must contain only its own (correctly ordered) segments, \
                 uncontaminated by the other 2 ECUs' interleaved traffic"
        );
    }

    // Simulate the receive-phase deadline expiring: all 3 buffers
    // finalize together, in one pass, in first-opened-first (arrival)
    // order.
    let deliveries = finalize_concat_buffers(&mut registrants[0]);
    assert_eq!(
        deliveries.len(),
        3,
        "all 3 open buffers must finalize together on deadline expiry"
    );
    assert_eq!(deliveries[0].unique_resp_identifier, 2);
    assert_eq!(deliveries[1].unique_resp_identifier, 3);
    assert_eq!(deliveries[2].unique_resp_identifier, 1);
    assert!(registrants[0].concat.is_empty());
    assert_eq!(
        registrants[0].matches_got, 3,
        "matches_got must advance by exactly 3, one per finalized buffer"
    );
}

/// ADR-148 second Amendment (Codex round-3 finding) regression: a
/// concat buffer opened by a VACUOUS descriptor must not have its
/// continuation fast path fire during the `Tier1NonVacuous` scan pass --
/// doing so would let registrant R's buffer absorb a frame that a
/// DIFFERENT, non-vacuous registrant B should have claimed first per
/// ADR-100's non-vacuous-first precedence. R is registered before B
/// (registration order alone must not override the precedence).
#[test]
fn concat_fast_path_yields_to_a_non_vacuous_registrant_during_tier1_non_vacuous_scan() {
    let mut registrants = vec![
        CopRegistrant {
            expected: vec![vacuous_expected(55)],
            matches_needed: Some(1),
            concat_enabled: true,
            ..registrant(100, 1)
        },
        CopRegistrant {
            expected: vec![non_vacuous_expected(0x62, 99)],
            matches_needed: Some(1),
            concat_enabled: false,
            ..registrant(200, 1)
        },
    ];
    let mut finalized = Vec::new();

    // R opens its buffer via its vacuous descriptor during the
    // Tier1Vacuous pass (as it would in the real scan sequence).
    let opened = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        opened,
        Some((55, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(registrants[0].concat.len(), 1);
    assert!(registrants[0].concat[0].opened_vacuous);

    // A same-key continuation arrives; scanned as Tier1NonVacuous (as
    // `bind_frame` always does first). R's fast path must decline (the
    // buffer was opened vacuous, this scan pass is non-vacuous), R has
    // no non-vacuous descriptor of its own to match through the normal
    // path either, so control must reach B, whose non-vacuous
    // descriptor claims the frame instead.
    let result = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xCC],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        result,
        Some((99, 200, false, None, false, Some(QueueErrorClass::Positive))),
        "the non-vacuous registrant B must claim the frame, not R's vacuous-opened buffer"
    );
    assert_eq!(
        registrants[0].concat.len(),
        1,
        "R's buffer must not have been duplicated"
    );
    assert_eq!(
        registrants[0].concat[0].data,
        vec![0x62, 0xAA],
        "R's buffer must be left completely untouched by the frame B claimed"
    );
    assert_eq!(
        registrants[0].concat_segments_got, 1,
        "R must not record an extra absorbed segment for a frame it never claimed"
    );
}

/// ADR-148 second Amendment corner design-advisor flagged: registrant R
/// has BOTH a vacuous descriptor (which opened its buffer) and a
/// separate non-vacuous descriptor that also matches this
/// continuation's payload. During `Tier1NonVacuous`, the fast path
/// declines (scan-gated: the buffer was opened vacuous), but R's own
/// non-vacuous descriptor then matches in the "open new buffer" arm --
/// which must recognize the existing same-key buffer and ABSORB into it
/// rather than asserting or pushing a duplicate second buffer for the
/// same key.
#[test]
fn concat_open_new_buffer_arm_absorbs_into_existing_same_key_buffer_instead_of_duplicating() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![vacuous_expected(55), non_vacuous_expected(0x62, 88)],
        matches_needed: Some(1),
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // Opens via the vacuous descriptor (Tier1Vacuous scan: the
    // non-vacuous descriptor is excluded from this pass by its own
    // scan gate regardless of vec order).
    let opened = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        opened,
        Some((55, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(registrants[0].concat.len(), 1);
    assert!(registrants[0].concat[0].opened_vacuous);

    // Same-key continuation, scanned Tier1NonVacuous: the fast path
    // declines (scan-gated), but R's OWN non-vacuous descriptor matches
    // this payload -- the "open new buffer" arm must absorb into the
    // existing buffer instead of pushing a duplicate.
    let result = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xCC],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        result,
        Some((55, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(
        registrants[0].concat.len(),
        1,
        "must absorb into the existing buffer, not open a duplicate for the same key"
    );
    assert_eq!(
        registrants[0].concat[0].data,
        vec![0x62, 0xAA, 0xCC],
        "the continuation's data must be merged into the single existing buffer"
    );
    assert_eq!(registrants[0].concat_segments_got, 2);
}

/// ADR-148 second Amendment, edge-case-hunter test-coverage finding:
/// confirms `opened_vacuous`-based scan-gating is evaluated PER BUFFER,
/// not per registrant, when a single (IS-MULTIPLE-style, unbounded)
/// registrant has two concurrently-open buffers of MIXED vacuity origin
/// -- one opened via its vacuous descriptor, the other via its
/// non-vacuous descriptor. A continuation for the vacuous-opened buffer
/// must still decline during `Tier1NonVacuous` while a continuation for
/// the non-vacuous-opened buffer, arriving in the very same scan pass,
/// must still absorb -- proving each buffer's own flag governs it
/// independently.
#[test]
fn concat_mixed_vacuity_buffers_on_one_registrant_are_gated_independently() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![vacuous_expected(55), non_vacuous_expected(0x62, 88)],
        matches_needed: None, // IS-MULTIPLE-style: unbounded
        concat_enabled: true,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // Buffer A (URID 1, SID 0x99): only the vacuous descriptor matches
    // -- opens via Tier1Vacuous.
    let opened_a = bind_registrant(
        &mut registrants,
        1,
        &[0x99, 0xAA],
        1,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        opened_a,
        Some((55, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );

    // Buffer B (URID 2, SID 0x62): the non-vacuous descriptor matches --
    // opens via Tier1NonVacuous.
    let opened_b = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xBB],
        2,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        opened_b,
        Some((88, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(registrants[0].concat.len(), 2, "both buffers must coexist");
    assert!(registrants[0].concat[0].opened_vacuous);
    assert!(!registrants[0].concat[1].opened_vacuous);

    // A continuation for buffer A (vacuous-opened), scanned
    // Tier1NonVacuous: must decline -- no descriptor of R's own matches
    // [0x99, ..] non-vacuously either, so this pass claims nothing for R.
    let decline_a = bind_registrant(
        &mut registrants,
        1,
        &[0x99, 0xCC],
        1,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        decline_a, None,
        "buffer A's vacuous origin must decline this non-vacuous scan pass"
    );

    // A continuation for buffer B (non-vacuous-opened), same
    // Tier1NonVacuous pass: must absorb immediately -- proving buffer
    // B's gate is independent of buffer A's decline just above.
    let absorb_b = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xDD],
        2,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        absorb_b,
        Some((88, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );

    // Buffer A's continuation finally arrives on its matching scan pass.
    let absorb_a = bind_registrant(
        &mut registrants,
        1,
        &[0x99, 0xCC],
        1,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        absorb_a,
        Some((55, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );

    assert_eq!(registrants[0].concat.len(), 2, "still exactly two buffers");
    let buf_a = registrants[0]
        .concat
        .iter()
        .find(|b| b.key == (1, None, 0x99))
        .unwrap();
    let buf_b = registrants[0]
        .concat
        .iter()
        .find(|b| b.key == (2, None, 0x62))
        .unwrap();
    assert_eq!(buf_a.data, vec![0x99, 0xAA, 0xCC]);
    assert_eq!(buf_b.data, vec![0x62, 0xBB, 0xDD]);
    assert_eq!(registrants[0].concat_segments_got, 4);
}

// ── ADR-148 third Amendment: Fix 1 (source_id key component) ──

/// Fix 1 regression: a no-`UniqueRespIdTable` CLL delivers `unique_
/// resp_identifier == 0` for every frame (`route_frame`'s own doc
/// comment) -- the ONLY working RX mode for KWP/J1850, the concat-
/// eligible protocols. Two distinct ECUs answering the same broadcast/
/// functional request with the same SID must NOT collide into one
/// corrupted buffer just because their `unique_resp_identifier` is
/// identically `0` -- their differing split-header source-address byte
/// (`ConcatFrameMeta::source_id`, derived inside `bind_frame` itself
/// from `entry.header_protocol`/`header_bytes`, exercised here through
/// the real `bind_frame` call path rather than a hand-set `concat_meta`)
/// must keep them in two separate buffers.
#[test]
fn concat_source_id_prevents_urid_collision_across_two_ecus_no_table() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            expected: vec![non_vacuous_expected(0x62, 77)],
            matches_needed: None, // IS-MULTIPLE-style: unbounded
            concat_enabled: true,
            ..registrant(100, 1)
        }],
        Vec::new(),
    );
    entry.header_protocol = j2534_0404::ISO14230;

    // ECU A, source address 0x10, first segment.
    let binding_a = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x62, 0xAA],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[0x80, 0xF1, 0x10],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None, // overridden inside bind_frame itself
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );
    assert!(matches!(
        binding_a,
        FrameBinding::Registrant { absorbed: true, .. }
    ));

    // ECU B, source address 0x20, first segment -- same SID, same
    // (table-less) URID.
    let binding_b = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x62, 0xBB],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[0x80, 0xF1, 0x20],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );
    assert!(matches!(
        binding_b,
        FrameBinding::Registrant { absorbed: true, .. }
    ));

    assert_eq!(
        entry.registrants[0].concat.len(),
        2,
        "two distinct source addresses must open two separate buffers, not merge into one"
    );
    let keys: Vec<_> = entry.registrants[0].concat.iter().map(|b| b.key).collect();
    assert!(keys.contains(&(0, Some(0x10), 0x62)));
    assert!(keys.contains(&(0, Some(0x20), 0x62)));
    assert_eq!(entry.registrants[0].concat[0].data, vec![0x62, 0xAA]);
    assert_eq!(entry.registrants[0].concat[1].data, vec![0x62, 0xBB]);
}

/// Fix 1 residual: a headerless/too-short frame (`header_bytes.len() <
/// 3`, e.g. an empty split header) yields `source_id: None` from the
/// real `bind_frame` derivation -- no panic, and every such frame still
/// groups under the same `(unique_resp_identifier, None, SID)` key,
/// exactly like every frame did before this fix existed (no ECU-
/// identifying data is physically available at this layer for such a
/// frame -- an accepted residual, not a regression).
#[test]
fn concat_headerless_frame_source_id_groups_under_none_like_before_the_fix() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            expected: vec![non_vacuous_expected(0x62, 77)],
            matches_needed: Some(1),
            concat_enabled: true,
            ..registrant(100, 1)
        }],
        Vec::new(),
    );
    entry.header_protocol = j2534_0404::ISO14230;

    let first = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x62, 0xAA],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[], // too short: no source address available
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );
    assert!(matches!(
        first,
        FrameBinding::Registrant { absorbed: true, .. }
    ));
    assert_eq!(entry.registrants[0].concat.len(), 1);
    assert_eq!(entry.registrants[0].concat[0].key, (0, None, 0x62));

    // A second, still-headerless segment with the same SID must absorb
    // into the SAME buffer -- not panic, not open a second one.
    let second = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x62, 0xBB],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            timestamp: 0,
            header_bytes: &[],
            footer_bytes: &[],
            rx_status_flags: 0,
            source_id: None,
            uudt_routed: false,
            raw_prefix: 0,
        },
        &mut Vec::new(),
        &mut false,
    );
    assert!(matches!(
        second,
        FrameBinding::Registrant { absorbed: true, .. }
    ));
    assert_eq!(
        entry.registrants[0].concat.len(),
        1,
        "both headerless frames must group under the same None-keyed buffer"
    );
    assert_eq!(entry.registrants[0].concat[0].data, vec![0x62, 0xAA, 0xBB]);
}

// ── ADR-148 third Amendment: Fix 2 (per-buffer byte/segment caps) ──

/// Fix 2 regression: absorbing a segment that pushes a buffer's
/// `data.len()` over `CONCAT_MAX_BUF_BYTES` must force-finalize THAT ONE
/// buffer immediately (removed from `concat`, pushed into
/// `finalized_concat`, `matches_got` incremented by 1) instead of
/// holding it open indefinitely.
#[test]
fn concat_absorb_exceeding_max_buf_bytes_force_finalizes_that_one_buffer() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(1),
        concat_enabled: true,
        concat: vec![ConcatBuf {
            key: (0, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 0,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0xAA; CONCAT_MAX_BUF_BYTES], // already sitting at the cap
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: 1,
        }],
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    let result = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xFF], // one more byte pushes data.len() over the cap
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    assert_eq!(
        result,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive))),
        "the triggering segment is still absorbed, not rejected"
    );
    assert_eq!(
        registrants[0].concat.len(),
        0,
        "the buffer must be force-finalized (removed) once it exceeds the byte cap"
    );
    assert_eq!(
        registrants[0].matches_got, 1,
        "the force-finalize counts as one completed match"
    );
    assert_eq!(finalized.len(), 1);
    assert_eq!(finalized[0].data.len(), CONCAT_MAX_BUF_BYTES + 1);
}

/// Fix 2's degenerate-case regression: a run of SID-only 1-byte
/// payloads never grows `data.len()` at all (each absorb appends `payload[1..]`,
/// which is empty), so the byte cap alone would never trip -- the
/// segment-count cap (`CONCAT_MAX_BUF_SEGMENTS`) must still force-
/// finalize once `segments` would exceed it.
#[test]
fn concat_absorb_exceeding_max_buf_segments_force_finalizes_degenerate_sid_only_case() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(1),
        concat_enabled: true,
        concat: vec![ConcatBuf {
            key: (0, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 0,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0x62],
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: CONCAT_MAX_BUF_SEGMENTS, // already sitting at the cap
        }],
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    let result = bind_registrant(
        &mut registrants,
        1,
        &[0x62], // SID-only: appends zero bytes to `data`
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    assert_eq!(
        result,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(
        registrants[0].concat.len(),
        0,
        "the segment-count cap must force-finalize even though this absorb added 0 bytes"
    );
    assert_eq!(finalized.len(), 1);
    assert_eq!(
        finalized[0].data,
        vec![0x62],
        "no new bytes were appended by the triggering SID-only segment"
    );
}

/// Fix 2 boundary regression: an absorb that brings `data.len()` to
/// EXACTLY `CONCAT_MAX_BUF_BYTES` (not one over) must NOT force-finalize
/// -- only the NEXT absorb past the boundary does (covered by
/// `concat_absorb_exceeding_max_buf_bytes_force_finalizes_that_one_buffer`
/// above). Complements that test, which starts already AT the cap rather
/// than exercising the transition onto it.
#[test]
fn concat_absorb_reaching_exactly_max_buf_bytes_does_not_force_finalize() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(1),
        concat_enabled: true,
        concat: vec![ConcatBuf {
            key: (0, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 0,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0xAA; CONCAT_MAX_BUF_BYTES - 1], // one short of the cap
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: 1,
        }],
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    let result = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xFF], // lands data.len() at EXACTLY the cap
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    assert_eq!(
        result,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(
        registrants[0].concat.len(),
        1,
        "landing exactly at the byte cap must leave the buffer open, not force-finalize it"
    );
    assert_eq!(registrants[0].concat[0].data.len(), CONCAT_MAX_BUF_BYTES);
    assert!(finalized.is_empty());
}

/// Fix 2 boundary regression, segment-count cap: an absorb that brings
/// `segments` to EXACTLY `CONCAT_MAX_BUF_SEGMENTS` (not one over) must
/// NOT force-finalize -- mirrors
/// `concat_absorb_reaching_exactly_max_buf_bytes_does_not_force_finalize`
/// for the segment-count cap instead of the byte cap.
#[test]
fn concat_absorb_reaching_exactly_max_buf_segments_does_not_force_finalize() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: Some(1),
        concat_enabled: true,
        concat: vec![ConcatBuf {
            key: (0, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 0,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0x62],
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: CONCAT_MAX_BUF_SEGMENTS - 1, // one short of the cap
        }],
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    let result = bind_registrant(
        &mut registrants,
        1,
        &[0x62], // SID-only: appends zero bytes, but bumps segments to the cap
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    assert_eq!(
        result,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(
        registrants[0].concat.len(),
        1,
        "landing exactly at the segment cap must leave the buffer open, not force-finalize it"
    );
    assert_eq!(registrants[0].concat[0].segments, CONCAT_MAX_BUF_SEGMENTS);
    assert!(finalized.is_empty());
}

/// Fix 2 regression, SECOND absorb site: the "open new buffer" arm's
/// absorb-into-existing-same-key branch (reached, per Amendment 2, when a
/// same-key buffer was opened by a VACUOUS descriptor -- so the
/// continuation fast path above declines during a `Tier1NonVacuous` scan
/// -- and the frame instead satisfies R's separate non-vacuous
/// descriptor) must apply the SAME byte-cap check the fast path's own
/// absorb already has. Mirrors
/// `concat_open_new_buffer_arm_absorbs_into_existing_same_key_buffer_instead_of_duplicating`'s
/// setup to reach this second site, but with the existing buffer already
/// sitting at the byte cap so this absorb tips it over.
#[test]
fn concat_open_new_buffer_arm_absorb_exceeding_max_buf_bytes_force_finalizes_at_second_site() {
    let mut registrants = vec![CopRegistrant {
        expected: vec![vacuous_expected(55), non_vacuous_expected(0x62, 88)],
        matches_needed: Some(1),
        concat_enabled: true,
        concat: vec![ConcatBuf {
            key: (0, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: 0,
            acceptance_id: 55, // set by the vacuous descriptor that opened it
            rx_status_flags: 0,
            data: vec![0xAA; CONCAT_MAX_BUF_BYTES], // already sitting at the cap
            footer_bytes: Vec::new(),
            opened_vacuous: true,
            segments: 1,
        }],
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // Scanned `Tier1NonVacuous`: the fast path declines (the buffer was
    // opened via the vacuous descriptor), so this falls through to the
    // full descriptor match, which R's own non-vacuous descriptor
    // satisfies -- landing in the "open new buffer" arm's
    // absorb-into-existing branch, the SECOND site.
    let result = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xFF], // one more byte pushes data.len() over the cap
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );

    assert_eq!(
        result,
        Some((55, 100, false, None, true, Some(QueueErrorClass::Positive))),
        "the triggering segment is still absorbed, not rejected"
    );
    assert_eq!(
        registrants[0].concat.len(),
        0,
        "the second absorb site's cap check must also force-finalize (remove) the buffer"
    );
    assert_eq!(registrants[0].matches_got, 1);
    assert_eq!(finalized.len(), 1);
    assert_eq!(finalized[0].data.len(), CONCAT_MAX_BUF_BYTES + 1);
}

/// Fix 2 / Fix 3 interaction regression: force-finalizing a buffer via
/// Fix 2's byte cap must actually remove it from `r.concat` (not just
/// mark it), freeing a slot that lets a SUBSEQUENT, genuinely new-keyed
/// frame open a new buffer where Fix 3's `CONCAT_MAX_OPEN_BUFFERS` cap
/// would otherwise have declined it.
#[test]
fn concat_force_finalize_via_byte_cap_frees_a_slot_for_a_new_buffer() {
    let mut concat: Vec<ConcatBuf> = (0..CONCAT_MAX_OPEN_BUFFERS)
        .map(|i| ConcatBuf {
            key: (i, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: i,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0x62],
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: 1,
        })
        .collect();
    // Buffer 0 is already sitting at the byte cap, so its next absorb
    // tips it over and force-finalizes it.
    concat[0].data = vec![0xAA; CONCAT_MAX_BUF_BYTES];
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: None, // IS-MULTIPLE-style: unbounded
        concat_enabled: true,
        concat,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // Continuation for buffer 0 (URID 0) -- absorbed via the fast path,
    // then force-finalized for exceeding the byte cap, freeing a slot.
    let continuation = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xFF],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        continuation,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(
        registrants[0].concat.len(),
        (CONCAT_MAX_OPEN_BUFFERS - 1) as usize,
        "the byte-cap force-finalize must actually remove buffer 0, freeing a slot"
    );
    assert_eq!(registrants[0].matches_got, 1);
    assert_eq!(finalized.len(), 1);

    // A genuinely new key, previously blocked by the count cap, must now
    // be able to open a fresh buffer since a slot was freed.
    let opened = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xEE],
        CONCAT_MAX_OPEN_BUFFERS, // never-before-seen URID
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        opened,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive))),
        "the freed slot must let a genuinely new key open a buffer"
    );
    assert_eq!(
        registrants[0].concat.len(),
        CONCAT_MAX_OPEN_BUFFERS as usize,
        "the new buffer must have been opened, back up to the cap"
    );
}

// ── ADR-148 third Amendment: Fix 3 (open-buffer-count cap) ──

/// Fix 3 regression: an IS-MULTIPLE (`matches_needed: None`) registrant
/// already holding `CONCAT_MAX_OPEN_BUFFERS` distinct open buffers must
/// NOT open a `CONCAT_MAX_OPEN_BUFFERS + 1`-th buffer for a genuinely new
/// key -- the frame is simply not absorbed, same fall-through behavior as
/// today's quota-exhausted case. An already-open buffer's own
/// continuation must keep absorbing normally: the cap only blocks
/// opening NEW buffers, never absorbing into an existing one.
#[test]
fn concat_open_buffer_count_capped_new_key_beyond_cap_is_not_absorbed() {
    let concat: Vec<ConcatBuf> = (0..CONCAT_MAX_OPEN_BUFFERS)
        .map(|i| ConcatBuf {
            key: (i, None, 0x62),
            timestamp: 0,
            header_bytes: Vec::new(),
            unique_resp_identifier: i,
            acceptance_id: 77,
            rx_status_flags: 0,
            data: vec![0x62],
            footer_bytes: Vec::new(),
            opened_vacuous: false,
            segments: 1,
        })
        .collect();
    let mut registrants = vec![CopRegistrant {
        expected: vec![non_vacuous_expected(0x62, 77)],
        matches_needed: None, // IS-MULTIPLE-style: unbounded
        concat_enabled: true,
        concat,
        ..registrant(100, 1)
    }];
    let mut finalized = Vec::new();

    // A frame carrying a brand-new, never-before-seen URID -- the cap is
    // already at `CONCAT_MAX_OPEN_BUFFERS`, so this must decline to open
    // a new buffer.
    let result = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xEE],
        CONCAT_MAX_OPEN_BUFFERS,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        result, None,
        "the open-buffer-count cap must decline a genuinely new key beyond \
             CONCAT_MAX_OPEN_BUFFERS"
    );
    assert_eq!(
        registrants[0].concat.len(),
        CONCAT_MAX_OPEN_BUFFERS as usize,
        "no new buffer must have been added"
    );

    // An already-open buffer's own continuation (URID 0) must still
    // absorb normally -- the cap never applies to absorption.
    let continuation = bind_registrant(
        &mut registrants,
        1,
        &[0x62, 0xAA],
        0,
        AttributionScan::Tier1NonVacuous,
        0,
        ConcatFrameMeta::default(),
        &mut finalized,
        &mut false,
    );
    assert_eq!(
        continuation,
        Some((77, 100, false, None, true, Some(QueueErrorClass::Positive)))
    );
    assert_eq!(registrants[0].concat[0].data, vec![0x62, 0xAA]);
}

/// ADR-147 fifth amendment (split direction-specific anchors, a partial
/// revert of the fourth amendment's generalization) pins the EXACT
/// signal `poll_rx_inner` relies on to decide when to capture
/// `LogicalLinkState::error_clear_seq` into `CllRxEntry::suspend_seq`:
/// `bind_frame`'s `wrote_suspend` out-param must be `true` at the one
/// call that writes/rewrites `entry.queue_error_class` to
/// `Some(QueueErrorClass::Suspend)` specifically -- NEVER for a
/// `Positive` write (which now anchors at batch-read time instead, via
/// `CllRxEntry::set_seq_at_read`, and never participates in fold-time
/// capture at all) -- and `false` on every other call, including a call
/// whose frame binds but declines to classify at all (`None`).
/// `poll_rx_inner` itself is not directly unit-testable (see
/// `check_match_against_baseline_tests`'s own doc comment for why), so
/// this pins the decision `bind_frame` hands it -- the same role
/// `queue_error_class_to_apply_tests` plays for the pure apply-time
/// decision downstream of it. This is the "wrong implementation" shape
/// design-advisor specifically flagged as worth pinning: naively
/// re-capturing on every call that leaves `queue_error_class` at some
/// `Some(_)` value (rather than only the call that actually WROTE
/// `Suspend` this time) would wrongly extend a stale classification's
/// validity past a legitimate intervening synchronous state change --
/// Frame 5 below is the case that shape would get wrong.
#[test]
fn wrote_suspend_flag_set_only_on_the_frame_that_folds_suspend() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            rc_cfg: Some(RcHandlingConfig {
                rc_byte_offset: 2,
                rc78_handling: false, // RC78 disabled -> unhandled, not pending
                request_sid: Some(0x22),
                ..Default::default()
            }),
            expected: vec![
                non_vacuous_expected(0x7F, 77),
                non_vacuous_expected(0x62, 78),
            ],
            matches_needed: None, // unbounded: every call below may bind again
            ..registrant(100, 1)
        }],
        Vec::new(),
    );

    // Frame 1: a genuine unhandled negative (0x7F, matching SID, RC78
    // disabled) -- folds Suspend. This, and Frame 4 below, are the ONLY
    // calls in this test where `wrote_suspend` must be `true`.
    let mut wrote_suspend = false;
    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7F, 0x22, 0x78],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut wrote_suspend,
    );
    assert!(matches!(binding, FrameBinding::Registrant { .. }));
    assert_eq!(entry.queue_error_class, Some(QueueErrorClass::Suspend));
    assert!(
        wrote_suspend,
        "the frame that folds Suspend into the entry must report it"
    );

    // Frame 2: a genuine positive response -- rewrites the
    // classification to Positive. `wrote_suspend` must stay `false`
    // here (ADR-147 fifth amendment, a partial revert of the fourth
    // amendment's symmetric capture): `Positive` no longer participates
    // in fold-time capture at all, since its own anchor is batch-read
    // time (`set_seq_at_read`), captured once per pass, not per fold.
    let mut wrote_suspend = false;
    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x62, 0x22, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut wrote_suspend,
    );
    assert!(matches!(binding, FrameBinding::Registrant { .. }));
    assert_eq!(entry.queue_error_class, Some(QueueErrorClass::Positive));
    assert!(
        !wrote_suspend,
        "a Positive fold must never be reported via wrote_suspend -- it has its own, \
             separate batch-read anchor"
    );

    // Frame 3: binds the SAME 0x7F descriptor again, but with the WRONG
    // SID -- `is_unhandled_negative` cannot confirm it (SID doesn't
    // echo), so the accepted-match site declines to classify at all
    // (`None`) per the decline-to-classify heuristic. `queue_error_class`
    // must be left exactly as Frame 2 left it (`Positive`), and
    // `wrote_suspend` must stay `false` -- a bound frame that declines
    // to classify is exactly "a frame that doesn't classify at all".
    let mut wrote_suspend = false;
    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7F, 0x99, 0x78],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut wrote_suspend,
    );
    assert!(matches!(binding, FrameBinding::Registrant { .. }));
    assert_eq!(
        entry.queue_error_class,
        Some(QueueErrorClass::Positive),
        "a declined-to-classify accepted match must not touch the existing classification"
    );
    assert!(!wrote_suspend);

    // Frame 4: folds Suspend again (same shape as Frame 1) -- confirms
    // the flag re-arms correctly on a SECOND genuine Suspend fold within
    // the same pass, not just the first call ever.
    let mut wrote_suspend = false;
    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x7F, 0x22, 0x78],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut wrote_suspend,
    );
    assert!(matches!(binding, FrameBinding::Registrant { .. }));
    assert_eq!(entry.queue_error_class, Some(QueueErrorClass::Suspend));
    assert!(wrote_suspend);

    // Frame 5: an entirely unremarkable frame that neither binds any
    // descriptor nor sights an unhandled negative -- the classic
    // "irrelevant later frame in the same pass" this mechanism must NOT
    // let refresh a stale classification's validity. `queue_error_class`
    // stays `Suspend` (from Frame 4, untouched), and `wrote_suspend`
    // must be `false`: `poll_rx_inner` uses exactly this flag to gate a
    // `suspend_seq` re-capture, so a wrong `true` here would wrongly
    // extend Frame 4's Suspend fold's validity past a legitimate
    // intervening synchronous state change reacting to Frame 4's own
    // exposure.
    let mut wrote_suspend = false;
    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            payload: &[0x50, 0x00],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut wrote_suspend,
    );
    assert!(matches!(binding, FrameBinding::Unbound));
    assert_eq!(entry.queue_error_class, Some(QueueErrorClass::Suspend));
    assert!(
        !wrote_suspend,
        "an irrelevant later frame must not be reported as a fold of Suspend"
    );
}

// --- ADR-150: UDS Session Timing (SID 0x10/0x50) live-exchange tests ---

/// A `SessionTimingConfig` with `session` already attached, `100_000`
/// (µs) `CP_CanTransmissionTime` (matching the ISO15765-3-on-ISO15765-2
/// preset default, `comparam_defaults.rs`), and no override entries --
/// for tests that only care about response pairing/derivation
/// (`from_params`/`with_request` themselves are covered separately in
/// `service::tests`).
fn session_timing_cfg(session: u8, functional: bool) -> SessionTimingConfig {
    SessionTimingConfig {
        session: Some(session),
        override_entries: Vec::new(),
        can_transmission_time_us: 100_000,
        functional,
    }
}

/// Required test: SID 0x50 happy path derives `CP_P2Max`/`CP_P2Star`
/// using the SAME full-addition formula for both (no 0.5x factor on the
/// P2Star side -- the corrected formula per the design consult). Bytes
/// `[0x00, 0xC8]` = 200 (P2Server_max, ms); `[0x00, 0x0A]` = 10
/// (P2*Server_max, 10 ms units, i.e. 100 ms).
#[test]
fn session_timing_happy_path_derives_p2max_and_p2star_with_full_addition_formula() {
    let mut registrants = vec![CopRegistrant {
        timing_cfg: Some(TimingChangeConfig::UdsSession(session_timing_cfg(
            0x03, false,
        ))),
        ..registrant(100, 1)
    }];

    let binding = bind_registrant(
        &mut registrants,
        1,
        &[0x50, 0x03, 0x00, 0xC8, 0x00, 0x0A],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert!(binding.is_some());
    let (_, change) = registrants[0].pending_timing_change.as_ref().unwrap();
    assert!(
        change
            .derived
            .contains(&(ComParamId(j2534_0404::P2_MAX), 200 * 1_000 + 100_000)),
        "CP_P2Max = P2Server_max(ms) * 1000 + CP_CanTransmissionTime(us)"
    );
    assert!(
        change
            .derived
            .contains(&(PARAM_P2_STAR, 10 * 10_000 + 100_000)),
        "CP_P2Star must use the SAME full-addition formula as CP_P2Max, no 0.5x factor: \
             P2*Server_max(10ms units) * 10_000 + CP_CanTransmissionTime(us)"
    );
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::SessionTiming {
            session: 3,
            p2_max_ms: 200,
            p2_star_10ms: 10,
        })
    );
}

/// `CP_CanTransmissionTime` is an unvalidated client-supplied
/// `SetComParam` value (edge-case-hunter finding: no range check exists
/// on it anywhere in `rpc_set_com_param`) -- a client setting it near
/// `u32::MAX` must saturate the derived ComParam, not panic the whole
/// channel's poll task (`session_timing_to_comparams` used plain `+`
/// before this fix; confirmed failing with the expected
/// "attempt to add with overflow" panic message, then restored).
#[test]
fn session_timing_to_comparams_saturates_instead_of_overflowing() {
    let derived = session_timing_to_comparams(u16::MAX, u16::MAX, u32::MAX - 1_000);
    assert_eq!(
        derived,
        [
            (ComParamId(j2534_0404::P2_MAX), u32::MAX),
            (PARAM_P2_STAR, u32::MAX),
        ],
        "a near-u32::MAX CP_CanTransmissionTime must saturate the derived ComParam, \
             never panic or silently wrap"
    );
}

/// Required test: a response shorter than the 6 bytes a full SID 0x50
/// positive response needs is a documented no-op -- bounds-checked via
/// `.get()`, never a panic.
#[test]
fn observe_session_timing_response_short_response_is_a_noop_not_a_panic() {
    let cfg = session_timing_cfg(0x03, false);
    let mut accumulator = None;
    let result = observe_session_timing_response(&cfg, &[0x50, 0x03, 0x01], &mut accumulator, 0);
    assert!(result.is_none());
    assert!(accumulator.is_none());
}

/// Required test: a response whose session byte doesn't echo the
/// captured request's session type is a no-op.
#[test]
fn observe_session_timing_response_echo_mismatch_is_a_noop() {
    let cfg = session_timing_cfg(0x03, false);
    let mut accumulator = None;
    let result = observe_session_timing_response(
        &cfg,
        &[0x50, 0x04, 0x00, 0xC8, 0x00, 0x0A],
        &mut accumulator,
        0,
    );
    assert!(
        result.is_none(),
        "response session byte (0x04) does not echo the captured request session (0x03)"
    );
}

/// ADR-196 Decision item 3b: `observe_session_timing_response` re-bases its
/// own SID/session/timing-byte anchors by `raw_prefix` for a RawMode CLL,
/// whose `payload` still carries the raw 4-byte CAN-ID prefix ahead of the
/// actual SID 0x50 response -- the response derives correctly once the
/// anchors are shifted, exactly like the `raw_prefix == 0` happy path above.
#[test]
fn observe_session_timing_response_derives_correctly_at_nonzero_raw_prefix() {
    let cfg = session_timing_cfg(0x03, false);
    let mut accumulator = None;
    let result = observe_session_timing_response(
        &cfg,
        &[0x12, 0x34, 0x56, 0x78, 0x50, 0x03, 0x00, 0xC8, 0x00, 0x0A],
        &mut accumulator,
        4,
    );
    let change = result.expect("a raw-prefixed SID 0x50 response must still qualify");
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::SessionTiming {
            session: 3,
            p2_max_ms: 200,
            p2_star_10ms: 10,
        }),
        "the 4-byte CAN-ID prefix must not be misread as the SID/session/timing bytes"
    );
}

/// ADR-198 Phase 2: `observe_timing_response`'s TPI=2 arm re-bases its own
/// timing-byte anchor (`payload.get(2..7)` -> `payload.get(raw_prefix +
/// 2..raw_prefix + 7)`) for a RawMode K-line CLL -- a gap the original
/// ADR-196 Decision item 3b plan did not anticipate, since K-line RawMode
/// was unreachable in Phase 1 (`observe_timing_response`'s own doc comment
/// used to say so). At `raw_prefix == 0` this is byte-for-byte the
/// pre-ADR-198 behavior.
#[test]
fn observe_timing_response_tpi2_derives_correctly_at_zero_raw_prefix() {
    let cfg = access_timing_cfg(2);
    let mut accumulator = None;
    let change =
        observe_timing_response(&cfg, &[0xC3, 0x02, 10, 20, 30, 40, 50], &mut accumulator, 0)
            .expect("a genuine TPI=2 response must qualify");
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [10, 20, 30, 40, 50],
        })
    );
}

/// Same as above, but with a RawMode K-line CLL's own 3-byte KWP header
/// (CARB-addressed: format, target, source) ahead of the genuine `0xC3 02`
/// response -- the header bytes must not be misread as part of the 5 timing
/// bytes, and `observe_registrant_timing_change`'s own `0xC3`/TPI-echo match
/// guard (rebased to `raw_prefix`/`raw_prefix + 1` in the same fix) must
/// still recognize this as a qualifying response.
#[test]
fn observe_timing_response_tpi2_derives_correctly_at_nonzero_raw_prefix() {
    let cfg = access_timing_cfg(2);
    let mut accumulator = None;
    let change = observe_timing_response(
        &cfg,
        &[0x68, 0x6A, 0xF1, 0xC3, 0x02, 10, 20, 30, 40, 50],
        &mut accumulator,
        3,
    )
    .expect("a raw-prefixed TPI=2 response must still qualify");
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [10, 20, 30, 40, 50],
        }),
        "the 3-byte KWP header must not be misread as the timing bytes"
    );
}

/// ADR-198 Phase 2: end-to-end through `bind_frame` (mirroring
/// `timing_change_pending_rc_then_genuine_tpi2_response_both_detected`
/// above, but at a nonzero `raw_prefix`) -- `observe_registrant_timing_
/// change`'s own `0xC3`/TPI-echo match guard must read at `raw_prefix`, not
/// position 0: a header byte of `0xC3` at position 0 (which would
/// spuriously match the TPI=2 opcode under the pre-ADR-198 unconditional
/// `payload.first()` check) must NOT be mistaken for the genuine response,
/// which starts at `raw_prefix` (3).
#[test]
fn bind_frame_recognizes_tpi2_response_at_nonzero_raw_prefix() {
    let mut entry = test_entry(
        1,
        vec![CopRegistrant {
            timing_cfg: Some(timing_cfg_tpi(2)),
            ..registrant(100, 1)
        }],
        Vec::new(),
    );

    let binding = bind_frame(
        &mut entry,
        FrameContext {
            is_content_frame: true,
            is_tx_side: false,
            frame_can_id: None,
            // Header byte 0 is 0xC3 (would spuriously match the TPI=2
            // opcode at position 0 under the pre-ADR-198 unconditional
            // `payload.first()` check); the genuine response starts at
            // raw_prefix = 3.
            payload: &[0xC3, 0x10, 0xF1, 0xC3, 0x02, 10, 20, 30, 40, 50],
            unique_resp_identifier: 0,
        },
        0,
        ConcatFrameMeta {
            raw_prefix: 3,
            ..Default::default()
        },
        &mut Vec::new(),
        &mut false,
    );
    assert_eq!(cop_handle_of(&binding), Some(100));
    let (_, change) = entry.registrants[0]
        .pending_timing_change
        .as_ref()
        .expect("the raw-prefixed TPI=2 response must produce a pending timing change");
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::AccessTiming {
            timing_set: 1,
            bytes: [10, 20, 30, 40, 50]
        }),
        "the 3-byte header (including its spurious leading 0xC3 byte) must not be misread as \
         part of the response"
    );
}

/// ADR-196 Decision item 3b: `classify_queue_error`'s tier-2 (no `rc_cfg`)
/// `0x7F`-at-position-0 check moves to `payload.get(raw_prefix)` -- a
/// RawMode CLL's raw CAN-ID prefix byte at position 0 must never itself be
/// misread as a negative response's leading `0x7F`, and the REAL `0x7F` at
/// `raw_prefix` must still decline classification (`None`, not `Positive`)
/// exactly like the `raw_prefix == 0` case already does.
#[test]
fn classify_queue_error_checks_0x7f_at_raw_prefix_not_position_zero() {
    // The raw CAN-ID prefix bytes happen to include a literal 0x7F at
    // position 0 -- must not be mistaken for a negative response's leading
    // byte when raw_prefix is 4 (the genuine 0x7F is further in).
    let prefix_looks_like_0x7f = [0x7F, 0x00, 0x00, 0x00, 0x62, 0xF1, 0x90];
    assert_eq!(
        classify_queue_error(None, &prefix_looks_like_0x7f, 4),
        Some(QueueErrorClass::Positive),
        "a 0x7F byte INSIDE the raw prefix (not at raw_prefix itself) must not decline \
         classification"
    );

    // The genuine negative-response leading byte sits at raw_prefix.
    let real_negative_at_prefix = [0x12, 0x34, 0x56, 0x78, 0x7F, 0x22, 0x31];
    assert_eq!(
        classify_queue_error(None, &real_negative_at_prefix, 4),
        None,
        "a tier-2 (no rc_cfg) frame whose logical response starts with 0x7F at raw_prefix must \
         decline classification, mirroring the raw_prefix == 0 case"
    );
}

/// ADR-196 Decision item 3b: a raw frame shorter than `raw_prefix` itself
/// (no logical response bytes at all) must decline classification (`None`)
/// rather than fall through to `Positive`.
#[test]
fn classify_queue_error_declines_when_payload_shorter_than_raw_prefix() {
    assert_eq!(classify_queue_error(None, &[0x12, 0x34], 4), None);
}

/// Required test: functional addressing folds two responses' P2Max/P2Star
/// toward the maximum -- both are client-side timeout ceilings with no
/// P2Min-analog direction, unlike KWP's mixed min/max quintuple.
#[test]
fn session_timing_functional_addressing_folds_p2max_and_p2star_toward_the_maximum() {
    let mut registrants = vec![CopRegistrant {
        timing_cfg: Some(TimingChangeConfig::UdsSession(session_timing_cfg(
            0x03, true,
        ))),
        matches_needed: Some(2),
        ..registrant(100, 1)
    }];

    // First responder: P2Max=200ms, P2Star=10 (100ms).
    bind_registrant(
        &mut registrants,
        1,
        &[0x50, 0x03, 0x00, 0xC8, 0x00, 0x0A],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    // Second responder: P2Max=50ms (smaller), P2Star=15 (150ms, larger).
    bind_registrant(
        &mut registrants,
        1,
        &[0x50, 0x03, 0x00, 0x32, 0x00, 0x0F],
        0,
        AttributionScan::Tier1Vacuous,
        1,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );

    assert_eq!(
        registrants[0].timing_accumulator,
        Some(TimingAccumulator::Session {
            p2_ms: 200,
            p2_star_10ms: 15,
        }),
        "both P2Max and P2Star fold toward the maximum"
    );
    let (seq, change) = registrants[0].pending_timing_change.as_ref().unwrap();
    assert_eq!(*seq, 1);
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::SessionTiming {
            session: 3,
            p2_max_ms: 200,
            p2_star_10ms: 15,
        }),
        "must record the combined (max-folded) values, not just the latest response's own"
    );
}

/// Required test: physical addressing never accumulates -- a second
/// response from the SAME registrant supersedes (never merges with) the
/// first, mirroring the existing KWP physical-addressing test.
#[test]
fn session_timing_physical_addressing_same_registrant_supersession() {
    let mut registrants = vec![CopRegistrant {
        timing_cfg: Some(TimingChangeConfig::UdsSession(session_timing_cfg(
            0x03, false,
        ))),
        matches_needed: Some(2),
        ..registrant(100, 1)
    }];

    bind_registrant(
        &mut registrants,
        1,
        &[0x50, 0x03, 0x00, 0xC8, 0x00, 0x0A],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    let second = bind_registrant(
        &mut registrants,
        1,
        &[0x50, 0x03, 0x00, 0x32, 0x00, 0x0F],
        0,
        AttributionScan::Tier1Vacuous,
        1,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    match second {
        Some((_, cop_handle, ecu_timing_change, superseded_timing_seq, _, _)) => {
            assert_eq!(cop_handle, 100);
            assert!(ecu_timing_change);
            assert_eq!(
                superseded_timing_seq,
                Some(0),
                "physical addressing never accumulates -- the earlier this-pass \
                     observation is fully superseded by this one"
            );
        }
        None => panic!("expected Registrant binding"),
    }
    let (seq, change) = registrants[0].pending_timing_change.as_ref().unwrap();
    assert_eq!(*seq, 1, "must reflect the LATEST response, not the first");
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::SessionTiming {
            session: 3,
            p2_max_ms: 50,
            p2_star_10ms: 15,
        }),
        "must reflect the second response's own values, not a fold with the first"
    );
}

/// Required test: `CP_SessionTimingOverride` has a matching entry for
/// the echoed session -- derived ComParams use the override's values,
/// but `CP_SessionTiming_Ecu`'s recorded entry still reflects the
/// OBSERVED value (mirrors `CP_AccessTimingOverride`'s own
/// redirect-derivation-only semantics, ADR-146).
#[test]
fn session_timing_override_redirects_derived_but_not_ecu_entry() {
    let mut cfg = session_timing_cfg(0x03, false);
    cfg.override_entries = vec![(3, 111, 22)]; // (session, p2_max_ms, p2_star_10ms)
    let mut registrants = vec![CopRegistrant {
        timing_cfg: Some(TimingChangeConfig::UdsSession(cfg)),
        ..registrant(100, 1)
    }];

    let binding = bind_registrant(
        &mut registrants,
        1,
        &[0x50, 0x03, 0x00, 0xC8, 0x00, 0x0A],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert!(binding.is_some());
    let (_, change) = registrants[0].pending_timing_change.as_ref().unwrap();
    assert!(
        change
            .derived
            .contains(&(ComParamId(j2534_0404::P2_MAX), 111 * 1_000 + 100_000)),
        "derived CP_P2Max must come from the override, not the observed bytes"
    );
    assert!(
        change
            .derived
            .contains(&(PARAM_P2_STAR, 22 * 10_000 + 100_000)),
        "derived CP_P2Star must come from the override too"
    );
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::SessionTiming {
            session: 3,
            p2_max_ms: 200,
            p2_star_10ms: 10,
        }),
        "CP_SessionTiming_Ecu must still record the ECU's own OBSERVED values"
    );
}

/// Required test: `CP_SessionTimingOverride` is non-empty but has no
/// entry for the echoed session -- falls back to the observed value for
/// BOTH derivation and recording.
#[test]
fn session_timing_override_present_but_missing_echoed_session_falls_back_to_observed() {
    let mut cfg = session_timing_cfg(0x03, false);
    cfg.override_entries = vec![(9, 111, 22)]; // a different session -- no match for 3
    let mut registrants = vec![CopRegistrant {
        timing_cfg: Some(TimingChangeConfig::UdsSession(cfg)),
        ..registrant(100, 1)
    }];

    let binding = bind_registrant(
        &mut registrants,
        1,
        &[0x50, 0x03, 0x00, 0xC8, 0x00, 0x0A],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert!(binding.is_some());
    let (_, change) = registrants[0].pending_timing_change.as_ref().unwrap();
    assert!(
        change
            .derived
            .contains(&(ComParamId(j2534_0404::P2_MAX), 200 * 1_000 + 100_000)),
        "no matching override entry -- derivation falls back to the observed value"
    );
    assert!(
        change
            .derived
            .contains(&(PARAM_P2_STAR, 10 * 10_000 + 100_000))
    );
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::SessionTiming {
            session: 3,
            p2_max_ms: 200,
            p2_star_10ms: 10,
        })
    );
}

/// Required test (ADR-150, "UUDT interaction"): a frame routed via a
/// `CP_CanRespUUDTId` match must never pair against `timing_cfg`, even
/// when its payload would otherwise qualify (echoed session matches) --
/// a DiagnosticSessionControl response is definitionally unicast/USDT
/// traffic.
#[test]
fn session_timing_uudt_routed_frame_never_pairs_against_timing_cfg() {
    let mut registrants = vec![CopRegistrant {
        timing_cfg: Some(TimingChangeConfig::UdsSession(session_timing_cfg(
            0x03, false,
        ))),
        ..registrant(100, 1)
    }];

    let binding = bind_registrant(
        &mut registrants,
        1,
        &[0x50, 0x03, 0x00, 0xC8, 0x00, 0x0A],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta {
            uudt_routed: true,
            ..Default::default()
        },
        &mut Vec::new(),
        &mut false,
    );
    match binding {
        Some((_, cop_handle, ecu_timing_change, superseded_timing_seq, _, _)) => {
            assert_eq!(
                cop_handle, 100,
                "the frame must still bind normally as an ordinary response"
            );
            assert!(
                !ecu_timing_change,
                "a UUDT-routed frame must never pair against timing_cfg"
            );
            assert_eq!(superseded_timing_seq, None);
        }
        None => panic!("expected Registrant binding"),
    }
    assert!(
        registrants[0].pending_timing_change.is_none(),
        "no ComParam side effect for a UUDT-routed frame"
    );
}

/// Required test (ADR-150, "Concat (ADR-148) interaction"):
/// `concat_enabled` is gated to KWP/J1850 protocols only
/// (`CopRegistrant::concat_enabled`'s own doc comment,
/// `is_kwp_family() || is_j1850_family()`), so a dual-enabled
/// `CP_EnableConcatenation=1` + `CP_ModifyTiming=1` config on an
/// ISO15765 CLL can never actually produce `concat_enabled: true` --
/// this test forces it true anyway (bypassing the real gate) to prove
/// the defensive `observe_registrant_timing_change` call inside the
/// concat "open new buffer" arm still behaves correctly even though it
/// is currently unreachable for the UDS variant in practice.
#[test]
fn session_timing_still_pairs_when_absorbed_via_concat_open_new_buffer_arm() {
    let mut registrants = vec![CopRegistrant {
        concat_enabled: true,
        timing_cfg: Some(TimingChangeConfig::UdsSession(session_timing_cfg(
            0x03, false,
        ))),
        ..registrant(100, 1)
    }];

    let binding = bind_registrant(
        &mut registrants,
        1,
        &[0x50, 0x03, 0x00, 0xC8, 0x00, 0x0A],
        0,
        AttributionScan::Tier1Vacuous,
        0,
        ConcatFrameMeta::default(),
        &mut Vec::new(),
        &mut false,
    );
    assert!(binding.is_some());
    assert_eq!(
        registrants[0].concat.len(),
        1,
        "a vacuous descriptor with no existing buffer opens a fresh one"
    );
    let (_, change) = registrants[0].pending_timing_change.as_ref().expect(
        "even absorbed via the concat open-new-buffer arm, the ComParam side effect \
                 must still be captured",
    );
    assert_eq!(
        change.ecu_entry,
        Some(EcuTimingRecord::SessionTiming {
            session: 3,
            p2_max_ms: 200,
            p2_star_10ms: 10,
        })
    );
}
