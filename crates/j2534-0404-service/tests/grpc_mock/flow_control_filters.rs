//! `FLOW_CONTROL_FILTER` installation on hardware ISO15765 channels, driven
//! by each CLL's `UniqueRespIdTable`: point-to-point filters per configured
//! response address (ADR-039), Table B.13 `CP_Can*Format` decoding (ADR-040),
//! UUDT filters (ADR-041), and connect-time installation from a
//! pre-configured table with no unconditional pass-all fallback (ADR-048).
//!
//! ADR-068: `SetUniqueRespIdTable` only stages the Working table -- it never
//! does filter I/O itself. Filter installation happens at promotion time:
//! this CLL's own `ConnectComLogicalLink` (when the table was staged before
//! connecting), or `CoptUpdateparam` execution (when staged afterward, on an
//! already-connected CLL, as most tests below do). Each post-connect
//! `set_unique_resp_table` call below is followed by an explicit
//! `promote_via_update_param` call, and the first test additionally asserts
//! that `SetUniqueRespIdTable` alone installs no filter.

use serial_test::serial;
use vci_service_interface::ConnectComLogicalLinkRequest;

use crate::harness::*;

/// Verifies ADR-048 and ADR-068: an ISO15765 channel installs no filter at
/// all at `ConnectComLogicalLink` while the CLL's `UniqueRespIdTable` is
/// still empty (no pass-all fallback of any kind); `SetUniqueRespIdTable`
/// with a full `CP_CanRespUSDTId` / `CP_CanPhysReqId` address pair stages the
/// Working table only and installs NO filter by itself; only once
/// `CoptUpdateparam` promotes it to Active does a spec-conformant
/// point-to-point filter appear: `pMaskMsg` all-$FF, `pPatternMsg` = the
/// ECU's response CAN ID, `pFlowControlMsg` = the tester's physical request
/// CAN ID. Clearing the table back to empty and promoting again removes that
/// point-to-point filter and leaves the channel with NO filter at all — the
/// zero-mask pass-all fallback this used to fall back to has been removed
/// entirely as spec-non-conformant.
#[tokio::test]
#[serial]
async fn iso15765_set_unique_resp_id_table_installs_point_to_point_flow_control_filter() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // No UniqueRespIdTable configured yet: no filter installed at Connect (ADR-048).
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);

    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // ADR-068: SetUniqueRespIdTable alone (no CoptUpdateparam yet) does no
    // filter I/O -- it only staged the Working table.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);

    promote_via_update_param(&mut client, cll_handle).await;

    // Exactly one point-to-point filter installed for the configured ECU,
    // now that CoptUpdateparam promoted the table to Active.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER
    );
    assert_eq!(
        server.backdoor.filter_mask(MOCK_CHANNEL_ID, 0),
        vec![0xFF, 0xFF, 0xFF, 0xFF]
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x7E8_u32.to_be_bytes()
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 0),
        Some(0x7E0_u32.to_be_bytes().to_vec())
    );
    // No CP_Can*Format was set on the entry, so ADR-040's default (normal
    // 11-bit addressing) applies: no CAN_29BIT_ID / ISO15765_ADDR_TYPE flags.
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 0),
        0
    );
    assert_eq!(
        server
            .backdoor
            .filter_flow_control_tx_flags(MOCK_CHANNEL_ID, 0),
        0
    );

    // Clearing the table stages an empty Working table; still no filter I/O
    // until the following CoptUpdateparam promotes it, which removes the
    // now-unaddressable point-to-point filter. No pass-all fallback of any
    // kind replaces it -- that fallback was spec-non-conformant and has been
    // removed entirely.
    set_unique_resp_table(&mut client, cll_handle, vec![]).await;
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "Set alone must not touch filters"
    );

    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        0,
        "no filter of any kind remains once the table is cleared and promoted -- \
         no pass-all fallback replaces the removed point-to-point filter"
    );

    server.shutdown().await;
}

/// Verifies ADR-041: an entry that carries both `CP_CanRespUSDTId` and
/// `CP_CanRespUUDTId` alongside `CP_CanPhysReqId` gets two point-to-point
/// `FLOW_CONTROL_FILTER`s — one per response address — both sharing the same
/// `CP_CanPhysReqId` flow-control CAN ID. The UUDT filter is installed even
/// though `CP_CanRespUUDTFormat` is left unset (bit 0 / Flow Control clear by
/// ADR-040's format-absent default would skip a USDT filter, but that gate is
/// not applied to the UUDT filter — see ADR-041).
#[tokio::test]
#[serial]
async fn iso15765_set_unique_resp_id_table_installs_uudt_point_to_point_filter() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    // ADR-068: Set alone stages Working only; CoptUpdateparam promotes it to
    // Active and installs the filters.
    promote_via_update_param(&mut client, cll_handle).await;

    // One filter per configured response address: USDT first, then UUDT
    // (installation order). No pass-all fallback of any kind is ever
    // installed on this channel (ADR-122).
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER
    );
    assert_eq!(
        server.backdoor.filter_mask(MOCK_CHANNEL_ID, 0),
        vec![0xFF, 0xFF, 0xFF, 0xFF]
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x7E8_u32.to_be_bytes()
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 0),
        Some(0x7E0_u32.to_be_bytes().to_vec())
    );

    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::FLOW_CONTROL_FILTER
    );
    assert_eq!(
        server.backdoor.filter_mask(MOCK_CHANNEL_ID, 1),
        vec![0xFF, 0xFF, 0xFF, 0xFF]
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 1),
        0x5E8_u32.to_be_bytes()
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 1),
        Some(0x7E0_u32.to_be_bytes().to_vec())
    );

    server.shutdown().await;
}

/// Backlog fix (round 19 follow-up, Codex review PR #42): ISO 22900-2 Table
/// 76's `0xFFFFFFFF` "not used" sentinel for `CP_CanRespUSDTId` must be
/// treated as absent, exactly like the identical sentinel on
/// `CP_CanRespUUDTId` already was -- `install_point_to_point_fc_filters`'s
/// USDT branch used to read the ComParam via a raw, unfiltered key lookup,
/// so an entry left at the sentinel (spec-legal, unenforced by
/// `SetUniqueRespIdTable`) installed a real `PassThruStartMsgFilter
/// (FLOW_CONTROL_FILTER)` for CAN ID `0xFFFFFFFF` -- outside the valid
/// 11/29-bit range -- instead of being skipped. An entry with ONLY the
/// sentinel USDT id and a `CP_CanPhysReqId` (no UUDT id at all) must install
/// no filter whatsoever.
#[tokio::test]
#[serial]
async fn iso15765_set_unique_resp_id_table_skips_sentinel_usdt_id() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0xFFFF_FFFF),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        0,
        "a sentinel-valued CP_CanRespUSDTId (0xFFFFFFFF, ISO 22900-2 Table 76's \"not used\" \
         marker) must never reach PassThruStartMsgFilter as a real CAN ID"
    );

    server.shutdown().await;
}

/// Companion to the sentinel test above: an entry with a sentinel USDT id
/// alongside a REAL UUDT id must install exactly one filter (the UUDT one)
/// -- proving the USDT sentinel filter doesn't also suppress its sibling
/// UUDT branch, and mirrors
/// `iso15765_set_unique_resp_id_table_installs_uudt_point_to_point_filter`'s
/// two-real-ids case with the USDT side sentineled instead.
#[tokio::test]
#[serial]
async fn iso15765_set_unique_resp_id_table_sentinel_usdt_id_does_not_suppress_uudt_filter() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0xFFFF_FFFF),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x5E8_u32.to_be_bytes()
    );

    server.shutdown().await;
}

/// edge-case-hunter coverage-gap follow-up: the two sentinel tests above
/// only exercise `install_point_to_point_fc_filters`'s promote-time call
/// site (`CoptUpdateparam` promoting a post-connect table). This proves the
/// identical sentinel-skip behavior at its OTHER call site -- a table
/// configured before `ConnectComLogicalLink`, which installs filters
/// directly at connect time (mirrors
/// `iso15765_pre_connect_unique_resp_id_table_installs_filter_at_connect`'s
/// setup, with the USDT id sentineled instead of real).
#[tokio::test]
#[serial]
async fn iso15765_pre_connect_sentinel_usdt_id_installs_no_filter_at_connect() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0xFFFF_FFFF),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        0,
        "a sentinel-valued CP_CanRespUSDTId must install no filter at connect time either, not \
         just at promote time"
    );

    server.shutdown().await;
}

/// Verifies ADR-041's extended-addressing handling for the UUDT filter:
/// `CP_CanRespUUDTExtAddr` supplies the extension byte for a 5-byte
/// `pPatternMsg` when `CP_CanRespUUDTFormat` sets the extended-addressing bit,
/// independently of the USDT side.
#[tokio::test]
#[serial]
async fn iso15765_set_unique_resp_id_table_uudt_filter_honors_ext_addr_format() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x18DAF101),
                unum32_param(
                    CP_CAN_RESP_UUDT_FORMAT,
                    can_id_format::EXTENDED_29BIT_FC_ENABLED,
                ),
                unum32_param(CP_CAN_RESP_UUDT_EXT_ADDR, 0xF1),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x18DA01F1),
                unum32_param(
                    CP_CAN_PHYS_REQ_FORMAT,
                    can_id_format::EXTENDED_29BIT_FC_ENABLED,
                ),
                unum32_param(CP_CAN_PHYS_REQ_EXT_ADDR, 0x01),
            ],
        )],
    )
    .await;
    // ADR-068: promote Working to Active before checking filters.
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER
    );

    let expected_mask = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    let expected_pattern = {
        let mut bytes = 0x18DAF101_u32.to_be_bytes().to_vec();
        bytes.push(0xF1);
        bytes
    };
    let expected_flow_control = {
        let mut bytes = 0x18DA01F1_u32.to_be_bytes().to_vec();
        bytes.push(0x01);
        bytes
    };
    assert_eq!(
        server.backdoor.filter_mask(MOCK_CHANNEL_ID, 0),
        expected_mask
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        expected_pattern
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 0),
        Some(expected_flow_control)
    );

    let expected_tx_flags = j2534_0404::TX_EXTENDED_ID | j2534_0404::ISO15765_ADDR_TYPE;
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 0),
        expected_tx_flags
    );
    assert_eq!(
        server
            .backdoor
            .filter_flow_control_tx_flags(MOCK_CHANNEL_ID, 0),
        expected_tx_flags
    );

    server.shutdown().await;
}

/// Verifies ADR-040 (Table B.13 `CP_Can*Format` decoding):
///
/// - An entry whose `CP_CanRespUSDTFormat`/`CP_CanPhysReqFormat` set the
///   extended-addressing bit (3) and the 29-bit-CAN-Id bit (1) produces a
///   5-byte `pMaskMsg`/`pPatternMsg`/`pFlowControlMsg` (CAN Id + the
///   `CP_Can*ExtAddr` extension byte), with `CAN_29BIT_ID` and
///   `ISO15765_ADDR_TYPE` set in `TxFlags`.
/// - An entry whose `CP_CanRespUSDTFormat` clears the flow-control bit (0) is
///   skipped: no `FLOW_CONTROL_FILTER` is installed for it.
#[tokio::test]
#[serial]
async fn iso15765_set_unique_resp_id_table_honors_can_id_format_bits() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x18DAF101),
                    unum32_param(
                        CP_CAN_RESP_USDT_FORMAT,
                        can_id_format::EXTENDED_29BIT_FC_ENABLED,
                    ),
                    unum32_param(CP_CAN_RESP_USDT_EXT_ADDR, 0xF1),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x18DA01F1),
                    unum32_param(
                        CP_CAN_PHYS_REQ_FORMAT,
                        can_id_format::EXTENDED_29BIT_FC_ENABLED,
                    ),
                    unum32_param(CP_CAN_PHYS_REQ_EXT_ADDR, 0x01),
                ],
            ),
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E9),
                    unum32_param(CP_CAN_RESP_USDT_FORMAT, can_id_format::NORMAL_FC_DISABLED),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E1),
                ],
            ),
        ],
    )
    .await;
    // ADR-068: promote Working to Active before checking filters.
    promote_via_update_param(&mut client, cll_handle).await;

    // Entry 2 (flow control disabled) contributes no filter of its own,
    // leaving exactly entry 1's point-to-point filter. No pass-all fallback
    // of any kind is ever installed on this channel (ADR-122).
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER
    );

    let expected_mask = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    let expected_pattern = {
        let mut bytes = 0x18DAF101_u32.to_be_bytes().to_vec();
        bytes.push(0xF1);
        bytes
    };
    let expected_flow_control = {
        let mut bytes = 0x18DA01F1_u32.to_be_bytes().to_vec();
        bytes.push(0x01);
        bytes
    };
    assert_eq!(
        server.backdoor.filter_mask(MOCK_CHANNEL_ID, 0),
        expected_mask
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        expected_pattern
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 0),
        Some(expected_flow_control)
    );

    let expected_tx_flags = j2534_0404::TX_EXTENDED_ID | j2534_0404::ISO15765_ADDR_TYPE;
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 0),
        expected_tx_flags
    );
    assert_eq!(
        server
            .backdoor
            .filter_flow_control_tx_flags(MOCK_CHANNEL_ID, 0),
        expected_tx_flags
    );

    server.shutdown().await;
}

/// ADR-048: `SetUniqueRespIdTable` called *before* `ConnectComLogicalLink`
/// has its point-to-point `FLOW_CONTROL_FILTER` installed the moment the
/// channel connects, built directly from the already-configured table — no
/// unconditional pass-all fallback appears first, and no second
/// `SetUniqueRespIdTable` call after connecting is needed to replace one.
#[tokio::test]
#[serial]
async fn iso15765_pre_connect_unique_resp_id_table_installs_filter_at_connect() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    // Configure ECU addressing BEFORE connecting.
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // Not connected yet: nothing has been installed on hardware.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // The point-to-point filter is installed immediately at connect, built
    // from the table configured before connecting — no pass-all fallback
    // ever appears on this channel (ADR-048).
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER
    );
    assert_eq!(
        server.backdoor.filter_mask(MOCK_CHANNEL_ID, 0),
        vec![0xFF, 0xFF, 0xFF, 0xFF]
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x7E8_u32.to_be_bytes()
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 0),
        Some(0x7E0_u32.to_be_bytes().to_vec())
    );

    server.shutdown().await;
}

/// ADR-048: a second CLL joining an already-connected ISO15765 channel with
/// its own pre-configured `UniqueRespIdTable` gets its own point-to-point
/// filter installed at connect too — filter installation is per-CLL, driven
/// by that CLL's own table, not just a one-time channel-level fallback.
#[tokio::test]
#[serial]
async fn iso15765_joining_cll_with_preconfigured_table_installs_own_filter_at_connect() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    // CLL A has no table yet: no filter installed for it.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);

    // CLL B: same protocol/baud rate, so it shares CLL A's physical channel.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_unique_resp_table(
        &mut client,
        cll_b,
        vec![ecu_entry(
            9,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7EA),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E2),
            ],
        )],
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for the joining CLL");

    // Only one physical channel was ever opened (CLL B shares CLL A's).
    assert_eq!(server.backdoor.connect_count(), 1);

    // CLL B's own filter is now installed on the shared channel, built from
    // its own table — CLL A's table is still empty and contributes none.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x7EA_u32.to_be_bytes()
    );

    server.shutdown().await;
}

/// A 4-byte (or 5-byte) all-zero mask is the signature of the removed
/// zero-mask pass-all `FLOW_CONTROL_FILTER` fallback -- a real point-to-point
/// filter always has an all-$FF mask (ADR-039/ADR-048). Asserts no filter on
/// `channel_id` ever has an all-zero mask.
fn assert_no_zero_mask_filter(server: &TestServer, channel_id: u32) {
    let count = server.backdoor.filter_count(channel_id);
    for i in 0..count {
        let mask = server.backdoor.filter_mask(channel_id, i);
        assert!(
            mask.iter().any(|&b| b != 0),
            "filter {i} on channel {channel_id} has an all-zero mask -- the removed \
             zero-mask pass-all fallback must never reappear"
        );
    }
}

/// Companion to
/// `iso15765_shared_channel_with_unaddressed_sibling_never_shows_zero_mask_fallback`,
/// covering the reverse join order (conformance-audit item B4): the
/// unaddressed CLL (B) connects *first* and creates the physical channel,
/// then the addressed CLL (A) joins the same channel afterward. No zero-mask
/// filter ever appears at any point, B never gets a filter of its own, and A
/// gets its point-to-point filter once addressed and promoted.
#[tokio::test]
#[serial]
async fn iso15765_unaddressed_first_then_addressed_joiner_never_shows_zero_mask_fallback() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // CLL B connects first, unaddressed: it creates the physical channel.
    let _cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);
    assert_no_zero_mask_filter(&server, MOCK_CHANNEL_ID);

    // CLL A joins the same physical channel (same protocol/baud) afterward
    // and gets addressed via SetUniqueRespIdTable + CoptUpdateparam promotion.
    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for the joining CLL");

    // Only one physical channel was ever opened (A shares B's).
    assert_eq!(server.backdoor.connect_count(), 1);

    // A is still unaddressed at this point: no filter of any kind yet, and
    // still no zero-mask fallback.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);
    assert_no_zero_mask_filter(&server, MOCK_CHANNEL_ID);

    set_unique_resp_table_and_promote(
        &mut client,
        cll_a,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // A's point-to-point filter is now installed; B still has none of its
    // own, and no zero-mask filter has ever appeared on the shared channel.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x7E8_u32.to_be_bytes()
    );
    assert_no_zero_mask_filter(&server, MOCK_CHANNEL_ID);

    server.shutdown().await;
}

/// Regression test for the removal of the ISO15765 zero-mask pass-all
/// `FLOW_CONTROL_FILTER` fallback (conformance-audit item B4): on a channel
/// shared by two CLLs where only one (A) is fully addressed and the other (B)
/// never configures a `UniqueRespIdTable`, no zero-mask filter ever appears
/// -- neither while B is unaddressed, nor across a further promotion on A
/// that changes its own point-to-point filter set. B never gets a fallback
/// filter of its own either.
#[tokio::test]
#[serial]
async fn iso15765_shared_channel_with_unaddressed_sibling_never_shows_zero_mask_fallback() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // CLL A: fully addressed from the start.
    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_a,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_no_zero_mask_filter(&server, MOCK_CHANNEL_ID);

    // CLL B joins the same physical channel (same protocol/baud) but never
    // configures a UniqueRespIdTable -- it stays unaddressed indefinitely.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for the joining CLL");

    // Only one physical channel was ever opened (B shares A's).
    assert_eq!(server.backdoor.connect_count(), 1);

    // B joining unaddressed does not install any fallback filter of its own
    // -- only A's point-to-point filter is present, and no zero-mask filter
    // ever appears.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x7E8_u32.to_be_bytes()
    );
    assert_no_zero_mask_filter(&server, MOCK_CHANNEL_ID);

    // A further promotion on A (adding a second addressed entry) must still
    // never surface a zero-mask filter, regardless of B's continued lack of
    // addressing.
    set_unique_resp_table_and_promote(
        &mut client,
        cll_a,
        vec![
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E9),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E1),
                ],
            ),
        ],
    )
    .await;

    // A now has two point-to-point filters; B still has none of its own, and
    // still no zero-mask filter has ever appeared on the shared channel.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);
    assert_no_zero_mask_filter(&server, MOCK_CHANNEL_ID);

    server.shutdown().await;
}
