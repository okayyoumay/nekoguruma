//! `CLEAR_MSG_FILTERS` IoCtl on non-ISO15765 (raw CAN) channels: the rebuild
//! path reuses `install_pass_all_filter` with the flags persisted on
//! `SharedChannel` at connect time (ADR-065), so a channel connected
//! `CAN_ID_BOTH` gets its full two-filter (TxFlags 0 + `TX_EXTENDED_ID`) set
//! back after a clear -- not a single TxFlags-0 filter, which would silently
//! stop delivering 29-bit frames.
//!
//! The ISO15765 clear-path rebuild (`reinstall_iso15765_channel_filters_after_clear`,
//! ADR-039) on a genuine hardware-ISO15765 channel (`hw_protocol_id ==
//! ISO15765` -- NOT software-isotp mode, which is raw CAN under the hood and
//! goes through the `PASS_FILTER` rebuild above instead) is covered here too:
//! `CLEAR_MSG_FILTERS` must rebuild the point-to-point `FLOW_CONTROL_FILTER`
//! from each addressed CLL's `UniqueRespIdTable`, never the removed zero-mask
//! pass-all fallback (ADR-122).

use serial_test::serial;
use vci_service_interface::{ConnectComLogicalLinkRequest, IoCtlRequest, io_ctl_request};

use crate::harness::*;

async fn clear_msg_filters(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) {
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                j2534_0404::CLEAR_MSG_FILTERS,
            )),
            input_data: None,
            has_output: false,
        })
        .await
        .expect("io_ctl(CLEAR_MSG_FILTERS) should succeed");
}

/// A raw CAN CLL connects `CAN_ID_BOTH` (ADR-065) and gets two pass-all
/// `PASS_FILTER`s, one per CAN-ID type. `CLEAR_MSG_FILTERS` wipes both, and
/// the rebuild must reinstall exactly the same two-filter set -- not a
/// single TxFlags-0 filter -- so 29-bit frames keep being delivered.
#[tokio::test]
#[serial]
async fn raw_can_clear_msg_filters_reinstalls_both_pass_all_filters() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    assert_eq!(server.backdoor.connect_flags_log(), vec![0x800]);
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 0),
        0
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 1),
        j2534_0404::TX_EXTENDED_ID
    );

    clear_msg_filters(&mut client, cll_handle).await;

    // Same two-filter set is rebuilt from the persisted connect flags, not a
    // single TxFlags-0 filter that would go deaf to 29-bit traffic.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 0),
        0
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 1),
        j2534_0404::TX_EXTENDED_ID
    );

    server.shutdown().await;
}

/// software-isotp mode: the raw CAN channel underlying an ISO15765 CLL is
/// itself non-ISO15765 from the adapter's point of view (`hw_protocol_id ==
/// CAN`), so `CLEAR_MSG_FILTERS` on it goes through the same PASS_FILTER
/// rebuild path, using the connect flags persisted for this channel
/// (ADR-046).
#[tokio::test]
#[serial]
async fn software_isotp_clear_msg_filters_reinstalls_both_pass_all_filters() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    clear_msg_filters(&mut client, cll_handle).await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 0),
        0
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 1),
        j2534_0404::TX_EXTENDED_ID
    );

    server.shutdown().await;
}

/// A 4-byte all-zero mask is the signature of the removed zero-mask pass-all
/// `FLOW_CONTROL_FILTER` fallback -- a real point-to-point filter always has
/// an all-$FF mask (ADR-039/ADR-048/ADR-122). Equivalent to the helper of the
/// same name in `flow_control_filters.rs`.
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

/// A genuine hardware-ISO15765 channel (`hw_protocol_id == ISO15765`, plain
/// `j2534_0404::ISO15765` -- not software-isotp mode) addresses a single ECU
/// via a full `CP_CanRespUSDTId`/`CP_CanPhysReqId` pair and gets its
/// point-to-point `FLOW_CONTROL_FILTER` installed at promotion (ADR-039/068).
/// `CLEAR_MSG_FILTERS` wipes it along with everything else on the channel,
/// and the clear-path rebuild
/// (`reinstall_iso15765_channel_filters_after_clear`) must reinstall the
/// identical point-to-point filter -- never the removed zero-mask pass-all
/// fallback (ADR-122).
#[tokio::test]
#[serial]
async fn iso15765_clear_msg_filters_rebuilds_point_to_point_filter() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_unique_resp_table_and_promote(
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

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    let mask_before = server.backdoor.filter_mask(MOCK_CHANNEL_ID, 0);
    let pattern_before = server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0);
    assert_no_zero_mask_filter(&server, MOCK_CHANNEL_ID);

    clear_msg_filters(&mut client, cll_handle).await;

    // The point-to-point filter is rebuilt with the same mask/pattern as
    // before the clear -- not a zero-mask pass-all fallback, which must never
    // reappear (ADR-122).
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER
    );
    assert_eq!(server.backdoor.filter_mask(MOCK_CHANNEL_ID, 0), mask_before);
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        pattern_before
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 0),
        Some(0x7E0_u32.to_be_bytes().to_vec())
    );
    assert_no_zero_mask_filter(&server, MOCK_CHANNEL_ID);

    server.shutdown().await;
}

/// The shared-channel case this diff (ADR-122) is specifically about: on a
/// hardware-ISO15765 channel shared by two CLLs where only A is addressed and
/// B never configures a `UniqueRespIdTable`, `CLEAR_MSG_FILTERS` -- issued via
/// either CLL's handle, since it operates on `channel_id` and is not scoped
/// to the calling CLL -- rebuilds A's point-to-point filter, leaves B with
/// none of its own, and never surfaces the removed zero-mask pass-all
/// fallback.
#[tokio::test]
#[serial]
async fn iso15765_clear_msg_filters_on_shared_channel_rebuilds_only_addressed_cll_filter() {
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

    // CLL B joins the same physical channel (same protocol/baud) but never
    // configures a UniqueRespIdTable.
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

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    let mask_before = server.backdoor.filter_mask(MOCK_CHANNEL_ID, 0);
    let pattern_before = server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0);
    assert_no_zero_mask_filter(&server, MOCK_CHANNEL_ID);

    // Call CLEAR_MSG_FILTERS via B's handle: the IOCTL operates on
    // channel_id, not the calling CLL, so it must still clear and rebuild A's
    // filter even though B is the one issuing it.
    clear_msg_filters(&mut client, cll_b).await;

    // A's point-to-point filter is rebuilt exactly as before the clear; B
    // still has none of its own, and no zero-mask fallback ever appears.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER
    );
    assert_eq!(server.backdoor.filter_mask(MOCK_CHANNEL_ID, 0), mask_before);
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        pattern_before
    );
    assert_no_zero_mask_filter(&server, MOCK_CHANNEL_ID);

    server.shutdown().await;
}
