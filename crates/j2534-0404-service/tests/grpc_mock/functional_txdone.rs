//! ADR-421: the transmit indication of a functional request reaches the link
//! that sent it, even when the link has a UniqueRespIdTable that names no row
//! for the functional ID, and only while that request's receive phase runs.

use serial_test::serial;
use vci_service_interface::{
    ComPrimitiveCtrlData, ExpectedResponseData, StartComPrimitiveRequest, event_item,
    subscribe_event_request, vci_service_client::VciServiceClient,
};

use crate::harness::*;

const FUNC_ID: u32 = 0x7DF;
/// TX_INDICATION | TX_MSG_TYPE: the transmit-done indication of a send.
const TX_DONE: u32 = 0x0000_0009;

/// An ISO15765 link with a table (one ECU at `resp`/`phys`), functional
/// request addressing on `FUNC_ID`, the transmit indication enabled and a
/// 3 s `CP_P2Max`. Several of these share one channel.
async fn table_link_with_functional_requests(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    resp: u32,
    phys: u32,
) -> vci_service_interface::ComLogicalLinkHandle {
    let cll = create_and_connect_cll(
        client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_unique_resp_table(
        client,
        cll,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, resp),
                unum32_param(CP_CAN_PHYS_REQ_ID, phys),
            ],
        )],
    )
    .await;
    set_com_param_unum32(client, cll, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(client, cll, CP_CAN_FUNC_REQ_ID, FUNC_ID).await;
    set_com_param_unum32(client, cll, CP_TRANSMIT_IND_ENABLE, 1).await;
    set_com_param_unum32(client, cll, j2534_0404::P2_MAX, 3_000_000).await;
    promote_via_update_param(client, cll).await;
    cll
}

async fn subscribe(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll: vci_service_interface::ComLogicalLinkHandle,
) -> tonic::Streaming<vci_service_interface::EventNotification> {
    client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner()
}

/// A send-receive that no frame can answer, so the receive phase lasts the
/// whole `CP_P2Max` unless the test injects a match.
async fn start_functional_sendrecv(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll: vci_service_interface::ComLogicalLinkHandle,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 77,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x7E],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");
}

fn is_result_data(item: &vci_service_interface::EventItem) -> bool {
    matches!(item.data, Some(event_item::Data::ResultData(_)))
}

/// The sending link gets its functional request's transmit indication while
/// the receive phase runs; a sibling with its own table and the same
/// functional ID does not; and once the phase has ended (the ECU answered),
/// a further indication on that ID reaches neither.
#[tokio::test]
#[serial]
async fn functional_send_receive_delivers_its_tx_done_to_the_sender_only() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let sender = table_link_with_functional_requests(&mut client, 0x7E8, 0x7E0).await;
    let sibling = table_link_with_functional_requests(&mut client, 0x7E9, 0x7E1).await;
    assert_eq!(server.backdoor.connect_count(), 1, "one shared channel");
    let mut sender_events = subscribe(&mut client, sender).await;
    let mut sibling_events = subscribe(&mut client, sibling).await;

    start_functional_sendrecv(&mut client, sender).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0)[0..4],
        FUNC_ID.to_be_bytes(),
        "the request went out on the functional ID"
    );

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(FUNC_ID, &[]),
        j2534_0404::ISO15765,
        TX_DONE,
    );
    assert!(
        wait_for_event(&mut sender_events, 2000, is_result_data).await,
        "the sender's functional TxDone should be delivered (ADR-421)"
    );
    assert!(
        !wait_for_event(&mut sibling_events, 300, is_result_data).await,
        "a sibling that did not send the request must not get the TxDone"
    );

    // The ECU answers: the receive phase ends and the marker with it.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );
    assert!(wait_for_event(&mut sender_events, 2000, is_result_data).await);
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(FUNC_ID, &[]),
        j2534_0404::ISO15765,
        TX_DONE,
    );
    assert!(
        !wait_for_event(&mut sender_events, 300, is_result_data).await,
        "after the receive phase the marker is gone and the indication is not routed"
    );
    assert!(!wait_for_event(&mut sibling_events, 300, is_result_data).await);

    drop(sender_events);
    drop(sibling_events);
    server.shutdown().await;
}

/// A functional tester-present is not marked (ADR-099 handles its
/// indication): on a link with a table its TxDone is not delivered.
#[tokio::test]
#[serial]
async fn functional_tester_present_tx_done_is_still_not_delivered() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll = table_link_with_functional_requests(&mut client, 0x7E8, 0x7E0).await;
    set_com_param_unum32(&mut client, cll, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_bytes(&mut client, cll, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(&mut client, cll, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(&mut client, cll, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll).await;
    let mut events = subscribe(&mut client, cll).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(FUNC_ID, &[]),
        j2534_0404::ISO15765,
        TX_DONE,
    );
    assert!(
        !wait_for_event(&mut events, 300, is_result_data).await,
        "a tester-present TxDone is not a functional request's and stays undelivered"
    );

    drop(events);
    server.shutdown().await;
}
