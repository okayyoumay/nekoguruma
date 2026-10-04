//! ADR-215: `CP_TesterPresentMessage` length validation.
//!
//! - `SetComParam`-time: ISO 22900-2's own `ParamMaxLen = 12` cap on the raw
//!   Bytefield payload (`rpc_link::rpc_set_com_param`'s Bytefield arm).
//! - `CoptStartcomm`/`CoptUpdateparam`-resolution-time: SAE J2534-1's
//!   per-protocol composed-message (header + payload) TX size range
//!   (`rpc_primitive::resolve_tester_present`'s new `sae_tx_size_range` call),
//!   and the ISO 15765-2 Single Frame constraint for both software-ISO-TP and
//!   functionally-addressed hardware ISO15765 tester-present.
//!
//! `FD_CAN_PS` padding coverage lives in `fd_can.rs` alongside its
//! `CoptSendrecv` counterpart (`fd_can_connected_link_pads_a_non_dlc_aligned_payload`),
//! not here, matching this suite's existing convention of keeping FD-specific
//! coverage in that file.

use serial_test::serial;
use vci_service_interface::{ParamItem, SetComParamRequest, StartComPrimitiveRequest, param_item};

use crate::harness::*;

/// ISO 22900-2's `ParamMaxLen = 12` declaration for `CP_TesterPresentMessage`
/// (§B.3.3.1) is enforced synchronously at `SetComParam` time, independent of
/// protocol -- a 13-byte Bytefield is rejected before it ever reaches
/// `link.working.bytes`, and exactly 12 bytes (the declared maximum) is
/// accepted.
#[tokio::test]
#[serial]
async fn setcomparam_rejects_a_tester_present_message_over_twelve_bytes() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;

    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    CP_TESTER_PRESENT_MESSAGE,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Bytefield(vec![0x3Eu8; 13])),
            }),
        })
        .await
        .expect_err("a 13-byte CP_TesterPresentMessage exceeds ISO 22900-2's 12-byte ParamMaxLen");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    // Exactly 12 bytes (the declared ParamMaxLen) is accepted.
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3Eu8; 12],
    )
    .await;

    server.shutdown().await;
}

/// SAE J2534-1's CAN row (Min Tx 4, Max Tx 12: 4-byte header + up to 8
/// payload bytes) applies to a resolved tester-present message exactly the
/// way it already applies to an ordinary `CoptSendrecv` -- a 9-byte payload
/// (13-byte composed message) is rejected synchronously at `CoptStartcomm`.
#[tokio::test]
#[serial]
async fn can_tester_present_composed_message_over_size_range_is_rejected_at_startcomm() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    // Physical addressing (a UniqueRespIdTable entry) -- keeps `can_functional`
    // `Some(false)`, so this test exercises ONLY the general SAE size-range
    // check (ADR-215 Decision item 2), not the unrelated functional Single
    // Frame check (Decision item 4).
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3Eu8; 9],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "a 9-byte tester-present payload composes a 13-byte CAN message, exceeding the \
             12-byte SAE J2534-1 ceiling",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("4..=12"), "{}", status.message());
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    server.shutdown().await;
}

/// The boundary case of the test above: an 8-byte payload (12-byte composed
/// message) is exactly at the CAN ceiling and must be accepted.
#[tokio::test]
#[serial]
async fn can_tester_present_composed_message_at_size_range_boundary_is_accepted() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3Eu8; 8],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 0).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("an 8-byte payload composes exactly a 12-byte CAN message and must be accepted");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0).len(), 12);

    server.shutdown().await;
}

/// SAE J2534-1's J1850PWM row (Min Tx 3, Max Tx 10: 3-byte header + up to 7
/// payload bytes) applies to a resolved tester-present message the same way.
/// An 8-byte payload (11-byte composed message) is rejected synchronously at
/// `CoptStartcomm`.
#[tokio::test]
#[serial]
async fn j1850pwm_tester_present_composed_message_over_size_range_is_rejected_at_startcomm() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850PWM,
        &[(j2534_0404::DATA_RATE, 41_600)],
    )
    .await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3Eu8; 8],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "an 8-byte tester-present payload composes an 11-byte J1850PWM message, exceeding \
             the 10-byte SAE J2534-1 ceiling",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("3..=10"), "{}", status.message());
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    server.shutdown().await;
}

/// ADR-055's functional-addressing Single Frame constraint (already enforced
/// for an ordinary `CoptSendrecv` by `resolve_send_recv_tx`) now applies to a
/// functionally-addressed hardware ISO15765 tester-present message too
/// (ADR-215 Decision item 4): an 8-byte payload exceeds the 7-byte Single
/// Frame capacity for Normal addressing.
#[tokio::test]
#[serial]
async fn hardware_iso15765_functional_tester_present_over_single_frame_is_rejected() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    // ADR-138: CP_TesterPresentAddrMode (not CP_RequestAddrMode) selects
    // tester-present's own addressing.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3Eu8; 8],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "an 8-byte functionally-addressed payload exceeds the 7-byte ISO15765 Single \
             Frame limit for Normal addressing",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    server.shutdown().await;
}

/// Software ISO-TP mode (ADR-046): an 8-byte tester-present payload exceeds
/// the 7-byte Single Frame capacity for Normal addressing, the same limit
/// `frame_tester_present_data` (`events.rs`) has always enforced -- but now
/// rejected synchronously at `CoptStartcomm` (ADR-215 Decision item 4)
/// instead of only late, inside the poll task's arm-time framing, where the
/// failure was a logged warning that silently disabled tester-present rather
/// than a client-visible error.
#[tokio::test]
#[serial]
async fn software_isotp_tester_present_over_single_frame_is_rejected_at_startcomm() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3Eu8; 8],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "an 8-byte software-ISO-TP tester-present payload exceeds the 7-byte Single Frame \
             limit for Normal addressing",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    // Rejected synchronously at CoptStartcomm -- never armed, so no arm-time
    // send (or the async PduErrEvtTesterPresentError `frame_tester_present_data`
    // would have emitted had this reached its own now-defensive-only check)
    // ever happens.
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    server.shutdown().await;
}
