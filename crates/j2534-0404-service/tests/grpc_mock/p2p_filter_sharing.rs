//! ADR-398: links on one ISO 15765 channel whose receive tables name the same pair share the
//! one point-to-point filter, and the filter is stopped when the last link using it leaves
//! (`Disconnect`, `Destroy`, a promoted table). The mock checks `FLOW_CONTROL_FILTER`s as an
//! adapter does (`set_flow_control_filter_checks`), so a filter installed twice, or one left
//! behind by a link that went away, is refused at the next connect.

use serial_test::serial;
use tonic::transport::Channel;
use vci_service_interface::vci_service_client::VciServiceClient;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, DestroyComLogicalLinkRequest,
    DisconnectComLogicalLinkRequest, EcuUniqueRespData, IoCtlRequest, io_ctl_request,
};

use crate::harness::*;

/// The channel limit the mock enforces for these tests.
const FILTER_LIMIT: usize = 10;

/// One USDT entry per `(response, request)` pair.
fn table(pairs: &[(u32, u32)]) -> Vec<EcuUniqueRespData> {
    pairs
        .iter()
        .zip(1..)
        .map(|(&(resp, req), id)| {
            ecu_entry(
                id,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, resp),
                    unum32_param(CP_CAN_PHYS_REQ_ID, req),
                ],
            )
        })
        .collect()
}

async fn staged(
    client: &mut VciServiceClient<Channel>,
    pairs: &[(u32, u32)],
) -> ComLogicalLinkHandle {
    let cll = create_cll(client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(client, cll, j2534_0404::DATA_RATE, 500_000).await;
    set_unique_resp_table(client, cll, table(pairs)).await;
    cll
}

async fn connect(client: &mut VciServiceClient<Channel>, cll: ComLogicalLinkHandle) {
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll),
        })
        .await
        .expect("the link should connect");
}

async fn disconnect(client: &mut VciServiceClient<Channel>, cll: ComLogicalLinkHandle) {
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll),
        })
        .await
        .expect("the link should disconnect");
}

const P0: (u32, u32) = (0x7E8, 0x7E0);
const P1: (u32, u32) = (0x7E9, 0x7E1);
const P2: (u32, u32) = (0x7EA, 0x7E2);

/// Two links with one table connect, one filter serves both, and it is stopped only when the
/// second one leaves.
#[tokio::test]
#[serial]
async fn links_with_one_pair_share_its_filter_until_the_last_leaves() {
    let server = TestServer::start().await;
    server.backdoor.set_flow_control_filter_checks(FILTER_LIMIT);
    let mut client = server.client().await;

    let a = staged(&mut client, &[P0]).await;
    connect(&mut client, a).await;
    let b = staged(&mut client, &[P0]).await;
    connect(&mut client, b).await;
    assert_eq!(server.backdoor.connect_count(), 1, "B joined A's channel");
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);

    disconnect(&mut client, a).await;
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "B still uses it"
    );
    assert_eq!(server.backdoor.stop_filter_count(), 0);

    disconnect(&mut client, b).await;
    assert_eq!(
        server.backdoor.stop_filter_count(),
        1,
        "the last stopped it"
    );
    assert_eq!(server.backdoor.disconnect_count(), 1);

    server.shutdown().await;
}

/// A link that leaves and reconnects while a sibling holds the channel finds the channel as
/// it left it: its own filters were stopped, the shared one stayed. More cycles than the
/// channel has filters show that none piles up.
#[tokio::test]
#[serial]
async fn a_reconnect_beside_a_sibling_does_not_pile_filters_up() {
    let server = TestServer::start().await;
    server.backdoor.set_flow_control_filter_checks(FILTER_LIMIT);
    let mut client = server.client().await;

    let a = staged(&mut client, &[P0]).await;
    connect(&mut client, a).await;
    let b = staged(&mut client, &[P0, P1]).await;
    for cycle in 0..=FILTER_LIMIT + 2 {
        connect(&mut client, b).await;
        assert_eq!(
            server.backdoor.filter_count(MOCK_CHANNEL_ID),
            2,
            "cycle {cycle}: A's filter shared, B's own installed"
        );
        disconnect(&mut client, b).await;
        assert_eq!(
            server.backdoor.filter_count(MOCK_CHANNEL_ID),
            1,
            "cycle {cycle}: only A's is left"
        );
    }
    assert_eq!(server.backdoor.connect_count(), 1);

    server.shutdown().await;
}

/// `Destroy` gives a link's filters back as `Disconnect` does.
#[tokio::test]
#[serial]
async fn destroying_a_link_gives_its_filters_back() {
    let server = TestServer::start().await;
    server.backdoor.set_flow_control_filter_checks(FILTER_LIMIT);
    let mut client = server.client().await;

    let a = staged(&mut client, &[P0]).await;
    connect(&mut client, a).await;
    let b = staged(&mut client, &[P0, P1]).await;
    connect(&mut client, b).await;
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(b),
        })
        .await
        .expect("the link should be destroyed");
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);

    // A new link with B's table connects: nothing of B's is left on the channel.
    let c = staged(&mut client, &[P0, P1]).await;
    connect(&mut client, c).await;
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    server.shutdown().await;
}

/// A table promoted on a connected link gives back the filters of the old table that no
/// sibling uses and keeps the shared one.
#[tokio::test]
#[serial]
async fn a_promoted_table_gives_back_the_old_tables_filters() {
    let server = TestServer::start().await;
    server.backdoor.set_flow_control_filter_checks(FILTER_LIMIT);
    let mut client = server.client().await;

    let a = staged(&mut client, &[P0]).await;
    connect(&mut client, a).await;
    let b = staged(&mut client, &[P0, P1]).await;
    connect(&mut client, b).await;
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    // B swaps P0 and P1 for P2: P1 (B's alone) goes, P0 stays for A.
    set_unique_resp_table_and_promote(&mut client, b, table(&[P2])).await;
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    // A leaves: P0 is nobody's now.
    disconnect(&mut client, a).await;
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0)[3],
        (P2.0 & 0xFF) as u8,
        "B's filter is the one left"
    );

    server.shutdown().await;
}

/// `CLEAR_MSG_FILTERS` wipes the channel's filters; the rebuild installs the pair two links
/// share once, not once per link.
#[tokio::test]
#[serial]
async fn the_rebuild_after_a_clear_installs_a_shared_pair_once() {
    let server = TestServer::start().await;
    server.backdoor.set_flow_control_filter_checks(FILTER_LIMIT);
    let mut client = server.client().await;

    let a = staged(&mut client, &[P0]).await;
    connect(&mut client, a).await;
    let b = staged(&mut client, &[P0]).await;
    connect(&mut client, b).await;

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(b)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                j2534_0404::CLEAR_MSG_FILTERS,
            )),
            input_data: None,
            has_output: false,
        })
        .await
        .expect("CLEAR_MSG_FILTERS should succeed");
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);

    // Both links still count as users: the filter goes only with the second.
    disconnect(&mut client, a).await;
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    disconnect(&mut client, b).await;
    assert_eq!(server.backdoor.stop_filter_count(), 1);

    server.shutdown().await;
}
