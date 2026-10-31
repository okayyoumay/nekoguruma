//! ADR-398: an adapter refusal of a point-to-point filter while a
//! `ConnectComLogicalLink` installs the CLL's connect-time UniqueRespIdTable
//! fails the connect and rolls it back, instead of leaving the link up without
//! that response path.
//!
//! The mock caps `PASS_FILTER`s at ten per channel, and native-mixed mode
//! installs one `PASS_FILTER` per UUDT-only entry, so an eleven-entry table is
//! refused at its last entry after ten were accepted.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ConnectComLogicalLinkRequest, DisconnectComLogicalLinkRequest, EcuUniqueRespData,
    error_detail_from_status,
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
