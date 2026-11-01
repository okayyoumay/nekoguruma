//! ADR-398: an adapter refusal of a point-to-point filter while a
//! `ConnectComLogicalLink` installs the CLL's connect-time UniqueRespIdTable
//! fails the connect and rolls it back, instead of leaving the link up without
//! that response path.
//!
//! The mock caps `PASS_FILTER`s at ten per channel, and native-mixed mode
//! installs one `PASS_FILTER` per UUDT-only entry, so an eleven-entry table is
//! refused at its last entry after ten were accepted. The mock does not check
//! `FLOW_CONTROL_FILTER`s unless a test asks (`set_flow_control_filter_checks`),
//! which the USDT tests below do.

use std::os::raw::c_long;

use serial_test::serial;
use tonic::Code;
use tonic::transport::Channel;
use vci_service_interface::vci_service_client::VciServiceClient;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, DisconnectComLogicalLinkRequest,
    EcuUniqueRespData, PduError, PduErrorEvent, error_detail_from_status,
};

use crate::harness::*;

/// `count` UUDT-only entries with distinct response ids.
fn uudt_only_table(count: u32) -> Vec<EcuUniqueRespData> {
    (0..count)
        .map(|i| ecu_entry(i + 1, vec![unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E0 + i)]))
        .collect()
}

/// Rows A1, A2 and E3: the eleventh filter is refused after ten were accepted.
/// The connect fails with `Internal`, every accepted filter was stopped, the
/// channel this connect opened is closed, and a ten-entry connect on the same
/// link then succeeds.
#[tokio::test]
#[serial]
async fn refused_table_filter_fails_connect_and_rolls_back() {
    let server = TestServer::start_with_can_mode(Some("native-mixed")).await;
    let mut client = server.client().await;

    let cll = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll, j2534_0404::DATA_RATE, 500_000).await;
    set_unique_resp_table(&mut client, cll, uudt_only_table(11)).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll),
        })
        .await
        .expect_err("an eleven-entry table must be refused at connect");
    assert_eq!(status.code(), Code::Internal);
    assert!(
        error_detail_from_status(&status).is_some(),
        "the refusal carries an ErrorDetail"
    );

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.stop_filter_count(),
        10,
        "the ten accepted filters were stopped"
    );
    assert_eq!(
        server.backdoor.disconnect_count(),
        1,
        "the channel this connect opened was closed"
    );
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);

    // The same link connects with a table the adapter accepts.
    set_unique_resp_table(&mut client, cll, uudt_only_table(10)).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll),
        })
        .await
        .expect("a ten-entry table connects");
    assert_eq!(server.backdoor.connect_count(), 2);
    // The mock numbers channels in connect order: the retry's is the second.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID + 1), 10);

    server.shutdown().await;
}

/// Row A5: a link joining a channel another link holds is refused the same
/// way; the sibling stays connected and the channel stays open.
#[tokio::test]
#[serial]
async fn refused_table_filter_on_a_joining_link_leaves_the_sibling_untouched() {
    let server = TestServer::start_with_can_mode(Some("native-mixed")).await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_unique_resp_table(&mut client, cll_b, uudt_only_table(11)).await;
    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect_err("the joining link's table must be refused");
    assert_eq!(status.code(), Code::Internal);

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "B joined, it opened no channel"
    );
    assert_eq!(server.backdoor.stop_filter_count(), 10);
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);
    assert_eq!(
        server.backdoor.disconnect_count(),
        0,
        "A still holds the channel"
    );

    // B's join was rolled back, so A alone closes the channel.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("A disconnects");
    assert_eq!(server.backdoor.disconnect_count(), 1);

    server.shutdown().await;
}

/// `count` USDT entries, each with its own response and request identifier.
fn usdt_table(count: u32) -> Vec<EcuUniqueRespData> {
    (0..count)
        .map(|i| {
            ecu_entry(
                i + 1,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8 + i),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0 + i),
                ],
            )
        })
        .collect()
}

/// Creates a link on a 500 kbit/s ISO 15765 channel with `table` staged.
async fn staged_link(
    client: &mut VciServiceClient<Channel>,
    table: Vec<EcuUniqueRespData>,
) -> ComLogicalLinkHandle {
    let cll = create_cll(client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(client, cll, j2534_0404::DATA_RATE, 500_000).await;
    set_unique_resp_table(client, cll, table).await;
    cll
}

async fn connect(
    client: &mut VciServiceClient<Channel>,
    cll: ComLogicalLinkHandle,
) -> Result<(), tonic::Status> {
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll),
        })
        .await
        .map(|_| ())
}

/// Row A3 for the USDT `FLOW_CONTROL_FILTER` branch: a pattern identifier another
/// filter of the channel uses is refused as not unique, which the connect reports as
/// invalid parameters after it stopped the filter installed before.
#[tokio::test]
#[serial]
async fn refused_usdt_flow_control_filter_not_unique_fails_connect() {
    let server = TestServer::start().await;
    server.backdoor.set_flow_control_filter_checks(10);
    let mut client = server.client().await;

    // Two ECUs that answer on one response identifier.
    let table = vec![
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
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E1),
            ],
        ),
    ];
    let cll = staged_link(&mut client, table).await;
    let status = connect(&mut client, cll)
        .await
        .expect_err("the second entry must be refused");
    assert_eq!(status.code(), Code::Internal);
    let detail = error_detail_from_status(&status).expect("an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrInvalidParameters as i32);
    assert_eq!(
        server.backdoor.stop_filter_count(),
        1,
        "the first was stopped"
    );
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);
    assert_eq!(server.backdoor.disconnect_count(), 1);

    server.shutdown().await;
}

/// Row A2 for the USDT branch: the channel takes two `FLOW_CONTROL_FILTER`s, the third
/// entry is over the limit.
#[tokio::test]
#[serial]
async fn refused_usdt_flow_control_filter_over_the_limit_fails_connect() {
    let server = TestServer::start().await;
    server.backdoor.set_flow_control_filter_checks(2);
    let mut client = server.client().await;

    let cll = staged_link(&mut client, usdt_table(3)).await;
    let status = connect(&mut client, cll)
        .await
        .expect_err("the third entry must be refused");
    assert_eq!(status.code(), Code::Internal);
    let detail = error_detail_from_status(&status).expect("an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrResourceError as i32);
    assert_eq!(server.backdoor.stop_filter_count(), 2);
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);

    // Two entries fit.
    set_unique_resp_table(&mut client, cll, usdt_table(2)).await;
    connect(&mut client, cll)
        .await
        .expect("two entries connect");
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID + 1), 2);

    server.shutdown().await;
}

/// A table whose USDT and UUDT response identifiers share one physical request identifier.
fn shared_request_id_table() -> Vec<EcuUniqueRespData> {
    vec![ecu_entry(
        1,
        vec![
            unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
            unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
            unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
        ],
    )]
}

/// ADR-047 with ADR-398: in `auto` mode the capability probe runs before the connect-time
/// filters are installed, so a device that holds a companion channel serves the UUDT id
/// there and gets no second flow-control filter on the shared request id, which an adapter
/// enforcing unique flow-control ids would refuse.
#[tokio::test]
#[serial]
async fn auto_mode_probes_before_the_strict_install_and_connects_dual_channel() {
    let server = TestServer::start_with_can_mode(Some("auto")).await;
    server.backdoor.set_flow_control_filter_checks(10);
    let mut client = server.client().await;

    let cll = staged_link(&mut client, shared_request_id_table()).await;
    connect(&mut client, cll)
        .await
        .expect("a dual-channel-capable adapter connects");

    // Primary, the probe's channel (released again), and the UUDT companion.
    assert_eq!(server.backdoor.connect_count(), 3);
    assert_eq!(server.backdoor.disconnect_count(), 1, "the probe's channel");
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "only the USDT flow-control filter is on the primary channel"
    );

    server.shutdown().await;
}

/// ADR-047 with ADR-398: a connect that joins a channel whose creator's `auto` probe is still
/// in flight gets the probe's result, so it installs through the companion channel. Both
/// tables share a request id between a USDT and a UUDT response id, which the adapter (unique
/// flow-control ids on) refuses as soon as a UUDT id gets the flow-control workaround, so both
/// connects succeed only with the dual-channel mode.
///
/// A's probe channel (the connect after its primary one) is held. B then connects onto A's
/// channel, and must not finish before the probe does. The native connect blocks its thread
/// while held, hence the multi-thread runtime.
///
/// The held connect blocks a runtime worker, and that worker may be the one that last drove
/// the runtime's timers and I/O: tokio hands the driver to another worker only when one is
/// woken, and the other workers may all be parked with nothing to wake them. A runtime timer
/// in the test body could then never fire, and the hold would never be released. So the
/// test body, which runs on the test's own thread and not on a worker, waits with
/// `std::thread::sleep` until it has released the hold: blocking that thread stalls nothing
/// else. Spawning B wakes a parked worker, which then drives the timers and I/O for B's
/// connect.
///
/// The mock cannot hold B between its first read of the mode and its install: B's first read
/// of the mode waits on the lock A's probe holds (`resolved_can_channel_mode`). A joiner
/// holding the pre-probe mode is therefore a race window of a few awaits, which the
/// install-time re-read closes by construction (no snapshot is used for the install), not a
/// state this test can pin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn auto_mode_joiner_waits_for_the_creators_probe_before_installing() {
    struct ReleaseOnDrop<'a>(&'a TestServer);
    impl Drop for ReleaseOnDrop<'_> {
        fn drop(&mut self) {
            self.0.backdoor.release_connect_hold();
        }
    }

    let server = TestServer::start_with_can_mode(Some("auto")).await;
    server.backdoor.set_flow_control_filter_checks(10);
    let mut client = server.client().await;
    let mut client_a = server.client().await;
    let mut client_b = server.client().await;

    let table_b = vec![ecu_entry(
        1,
        vec![
            unum32_param(CP_CAN_RESP_USDT_ID, 0x7E9),
            unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E9),
            unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E1),
        ],
    )];
    let cll_a = staged_link(&mut client, shared_request_id_table()).await;
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_unique_resp_table(&mut client, cll_b, table_b).await;

    // The connect after the primary one is A's probe channel: hold it.
    server.backdoor.arm_connect_hold(1);
    let release = ReleaseOnDrop(&server);
    let connect_a = tokio::spawn(async move { connect(&mut client_a, cll_a).await });
    for _ in 0..200 {
        if server.backdoor.connect_hold_engaged() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        server.backdoor.connect_hold_engaged(),
        "A's probe was never reached"
    );

    let connect_b = tokio::spawn(async move { connect(&mut client_b, cll_b).await });
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        !connect_b.is_finished(),
        "B finished while A's probe was held"
    );

    server.backdoor.release_connect_hold();
    connect_a
        .await
        .expect("A's task")
        .expect("A connects dual-channel");
    connect_b
        .await
        .expect("B's task")
        .expect("B connects dual-channel through the companion channel");
    drop(release);

    // Primary, the probe's channel (released again), and one shared UUDT companion.
    assert_eq!(server.backdoor.connect_count(), 3);
    assert_eq!(server.backdoor.disconnect_count(), 1, "the probe's channel");
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        2,
        "one USDT flow-control filter per link on the primary channel"
    );

    server.shutdown().await;
}

/// ADR-047 with ADR-398: when the probe finds no second channel, `auto` is single-channel,
/// the UUDT id gets the flow-control workaround as with an explicit `single-channel`, and
/// an adapter that refuses the repeated request id fails the connect like any other
/// refused table filter.
#[tokio::test]
#[serial]
async fn auto_mode_single_channel_install_is_refused_like_explicit_single_channel() {
    let server = TestServer::start_with_can_mode(Some("auto")).await;
    server.backdoor.set_max_channels(1);
    server.backdoor.set_flow_control_filter_checks(10);
    let mut client = server.client().await;

    let cll = staged_link(&mut client, shared_request_id_table()).await;
    let status = connect(&mut client, cll)
        .await
        .expect_err("the second flow-control filter must be refused");
    assert_eq!(status.code(), Code::Internal);
    let detail = error_detail_from_status(&status).expect("an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrInvalidParameters as i32);
    assert_eq!(server.backdoor.connect_count(), 1, "no companion opened");
    assert_eq!(
        server.backdoor.stop_filter_count(),
        1,
        "the first was stopped"
    );
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);
    assert_eq!(server.backdoor.disconnect_count(), 1);

    server.shutdown().await;
}

/// A device that is gone while a table filter is installed is the lost VCI
/// (`FAILED_PRECONDITION` with `PDU_ERR_EVT_LOST_COMM_TO_VCI`), which the agent reopens on,
/// not a refusal of the table it would halve.
#[tokio::test]
#[serial]
async fn a_lost_device_at_a_table_filter_is_the_lost_vci() {
    let server = TestServer::start().await;
    server
        .backdoor
        .set_start_filter_error(Some(j2534_0404::ERR_DEVICE_NOT_CONNECTED as c_long));
    let mut client = server.client().await;

    let cll = staged_link(&mut client, usdt_table(1)).await;
    let status = connect(&mut client, cll)
        .await
        .expect_err("the install fails");
    assert_eq!(status.code(), Code::FailedPrecondition);
    let detail = error_detail_from_status(&status).expect("an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrCommPcToVciFailed as i32);
    assert_eq!(
        detail
            .error_event_data
            .expect("the lost-VCI event")
            .error_event,
        PduErrorEvent::PduErrEvtLostCommToVci as i32
    );
    assert_eq!(
        server.backdoor.disconnect_count(),
        1,
        "the channel was closed"
    );

    server.backdoor.set_start_filter_error(None);
    connect(&mut client, cll)
        .await
        .expect("the link connects once the device is back");

    server.shutdown().await;
}

/// Any other error of the install fails the connect, but as `UNKNOWN` with
/// `PDU_ERR_FCT_FAILED`, which a client never reads as the worker's answer about the table.
#[tokio::test]
#[serial]
async fn another_error_at_a_table_filter_is_not_a_refusal() {
    let server = TestServer::start().await;
    server
        .backdoor
        .set_start_filter_error(Some(j2534_0404::ERR_FAILED as c_long));
    let mut client = server.client().await;

    let cll = staged_link(&mut client, usdt_table(1)).await;
    let status = connect(&mut client, cll)
        .await
        .expect_err("the install fails");
    assert_eq!(status.code(), Code::Unknown);
    let detail = error_detail_from_status(&status).expect("an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrFctFailed as i32);
    assert!(detail.error_event_data.is_none());
    assert_eq!(
        server.backdoor.disconnect_count(),
        1,
        "the channel was closed"
    );

    server.shutdown().await;
}

/// Row A8: a stop the adapter fails while the refused connect is rolled back is logged and
/// does not replace the refusal. The failed stops leave the filters on the channel the sibling
/// holds, as a failed stop always does; the sibling is untouched.
#[tokio::test]
#[serial]
async fn a_failed_stop_in_the_rollback_does_not_replace_the_refusal() {
    let server = TestServer::start_with_can_mode(Some("native-mixed")).await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    server
        .backdoor
        .set_stop_filter_error(Some(j2534_0404::ERR_FAILED as c_long));

    let cll_b = staged_link(&mut client, uudt_only_table(11)).await;
    let status = connect(&mut client, cll_b)
        .await
        .expect_err("the eleventh filter is refused");
    assert_eq!(status.code(), Code::Internal);
    let detail = error_detail_from_status(&status).expect("an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrResourceError as i32,
        "the refusal, not the failed stop"
    );
    assert_eq!(server.backdoor.stop_filter_count(), 0, "every stop failed");
    assert_eq!(server.backdoor.disconnect_count(), 0, "A holds the channel");

    server.backdoor.set_stop_filter_error(None);
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("A disconnects");
    assert_eq!(server.backdoor.disconnect_count(), 1);

    server.shutdown().await;
}
