//! End-to-end coverage for SAE J2534-2 clause 14 Repeat Messaging
//! (ADR-165/Phase 12): `PDU_IOCTL_START_REPEAT_MESSAGE`/`_QUERY_REPEAT_MESSAGE`/
//! `_STOP_REPEAT_MESSAGE` forwarded through the `IoCtl` RPC to the mock's own
//! clause-14 state machine, the J2534-2 opt-in gate, per-CLL `MsgId`
//! ownership (`require_owned_repeat_message`, ADR-165 Decision 4) across two
//! sibling CLLs sharing one physical channel, and slot teardown on
//! `DestroyComLogicalLink`/`DisconnectComLogicalLink`. Mirrors `sw_can.rs`'s
//! structure (this file's own originating brief); many setup helpers are
//! reused from `harness.rs` rather than duplicated.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, DataItem, DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest,
    IoBytearray, LockResourceRequest, PduError, StartComPrimitiveRequest, TxFlagBit, data_item,
    error_detail_from_status, io_ctl_request, vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// D-PDU `CP_PhysRespFormatPriorityType` ComParam ID (`PDU_PC_UNIQUE_ID`
/// class): the KWP/ISO14230/J1850 physical response format/priority byte.
/// Not exported by the crate, so duplicated here as a literal for the test
/// (same convention `rc_handling.rs`'s local `CP_RC_BYTE_OFFSET` etc. use).
const CP_PHYS_RESP_FORMAT_PRIORITY: u32 = 0x8077;

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// required for `PDU_IOCTL_START_REPEAT_MESSAGE` (clause 14 applies
/// channel-wide, not per-protocol, so no SW/FD-style connect-time gate to
/// piggyback on -- checked directly against the opted-in `pname`, ADR-165
/// Decision 1).
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- same per-file local helper shape as
/// `sw_can.rs`'s/`pdu_ioctl.rs`'s own (not shared via `harness.rs`, matching
/// this codebase's existing convention).
async fn resolve_ioctl_id(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    name: &str,
) -> u32 {
    client
        .get_object_id(vci_service_interface::GetObjectIdRequest {
            object_type: vci_service_interface::ObjectType::ObjtIoCtrl as i32,
            shortname: name.to_string(),
        })
        .await
        .unwrap_or_else(|err| panic!("get_object_id({name}) should succeed: {err}"))
        .into_inner()
        .pdu_object_id
}

/// Issues `IoCtl` against a `cll_handle` with an optional `input_data` and
/// `has_output` flag, returning the raw `output_data` on success.
async fn io_ctl_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cmd_id: u32,
    input_data: Option<DataItem>,
    has_output: bool,
) -> Result<Option<DataItem>, tonic::Status> {
    client
        .io_ctl(vci_service_interface::IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data,
            has_output,
        })
        .await
        .map(|resp| resp.into_inner().output_data)
}

/// Builds a `bytearray_data` (`IOBytearray`) `input_data` for
/// `PDU_IOCTL_START_REPEAT_MESSAGE`, packed per ADR-178's byte layout (the
/// removed `IORepeatMessageSetup` proto message's replacement carrier).
/// `condition = 0` (unconditional continued retransmission, ADR-165
/// Decision 6) is used throughout this file unless a test specifically
/// needs the `condition = 1` stop behavior, since that behavior is already
/// covered end-to-end by `j2534-0404-mock`'s own unit tests (this file's
/// originating brief) -- this file's job is the gRPC/ownership/opt-in
/// plumbing around the IOCTLs, not clause 14's own stop-condition
/// semantics.
fn repeat_message_setup(
    time_interval: u32,
    condition: u32,
    repeat_msg_data: Vec<u8>,
    mask_data: Vec<u8>,
    pattern_data: Vec<u8>,
) -> DataItem {
    repeat_message_setup_with_tx_flags(
        time_interval,
        condition,
        repeat_msg_data,
        mask_data,
        pattern_data,
        vec![],
    )
}

/// Like `repeat_message_setup`, but also sets `tx_flag_bits` (ADR-165 PR #42
/// round 7, Finding 2) -- the pass-through TX flags requested for the
/// transmitted `RepeatMsgData[0]` message. `repeat_message_setup` itself
/// always passes an empty `tx_flag_bits`, matching this file's existing
/// convention of factoring an optional/rarely-varied field into its own
/// builder rather than widening every call site's argument list.
fn repeat_message_setup_with_tx_flags(
    time_interval: u32,
    condition: u32,
    repeat_msg_data: Vec<u8>,
    mask_data: Vec<u8>,
    pattern_data: Vec<u8>,
    tx_flag_bits: Vec<i32>,
) -> DataItem {
    DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup(
                time_interval,
                condition,
                &repeat_msg_data,
                &mask_data,
                &pattern_data,
                &tx_flag_bits,
            ),
        })),
    }
}

/// Like `repeat_message_setup_with_tx_flags`, but also encodes ADR-214's
/// optional v2 trailing section (`response_tx_flag_bits`) -- the mask/
/// pattern response template's own, independent addressing basis. `None`
/// produces the byte-for-byte identical payload
/// `repeat_message_setup_with_tx_flags` itself already produces.
fn repeat_message_setup_with_response_tx_flag_bits(
    time_interval: u32,
    condition: u32,
    repeat_msg_data: Vec<u8>,
    mask_data: Vec<u8>,
    pattern_data: Vec<u8>,
    tx_flag_bits: Vec<i32>,
    response_tx_flag_bits: Option<Vec<i32>>,
) -> DataItem {
    DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup_with_response_tx_flag_bits(
                time_interval,
                condition,
                &repeat_msg_data,
                &mask_data,
                &pattern_data,
                &tx_flag_bits,
                response_tx_flag_bits.as_deref(),
            ),
        })),
    }
}

fn unum32_input(value: u32) -> DataItem {
    DataItem {
        data: Some(data_item::Data::Unum32Value(value)),
    }
}

/// Issues `PDU_IOCTL_START_REPEAT_MESSAGE` and extracts the returned `MsgId`
/// from `output_data` (`unum32_value`, ADR-165 Decision 2).
async fn start_repeat_message(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cmd_id: u32,
    setup: DataItem,
) -> Result<u32, tonic::Status> {
    let output = io_ctl_cll(client, cll_handle, cmd_id, Some(setup), true).await?;
    match output.and_then(|d| d.data) {
        Some(data_item::Data::Unum32Value(msg_id)) => Ok(msg_id),
        other => panic!(
            "PDU_IOCTL_START_REPEAT_MESSAGE should return a Unum32Value MsgId, got {other:?}"
        ),
    }
}

/// Issues `PDU_IOCTL_QUERY_REPEAT_MESSAGE` and extracts the returned status
/// from `output_data` (`unum32_value`).
async fn query_repeat_message(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cmd_id: u32,
    msg_id: u32,
) -> Result<u32, tonic::Status> {
    let output = io_ctl_cll(client, cll_handle, cmd_id, Some(unum32_input(msg_id)), true).await?;
    match output.and_then(|d| d.data) {
        Some(data_item::Data::Unum32Value(status)) => Ok(status),
        other => panic!(
            "PDU_IOCTL_QUERY_REPEAT_MESSAGE should return a Unum32Value status, got {other:?}"
        ),
    }
}

/// Issues `PDU_IOCTL_STOP_REPEAT_MESSAGE` (no output).
async fn stop_repeat_message(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cmd_id: u32,
    msg_id: u32,
) -> Result<(), tonic::Status> {
    io_ctl_cll(
        client,
        cll_handle,
        cmd_id,
        Some(unum32_input(msg_id)),
        false,
    )
    .await
    .map(|_| ())
}

/// Configures `cll_handle`'s UniqueRespIdTable with a single ECU entry
/// carrying `CP_CanPhysReqId` (TX addressing, `tx_header::build_tx_message`)
/// and the expected-response header `PDU_IOCTL_START_REPEAT_MESSAGE`
/// prepends to the client's mask/pattern (`tx_header::response_header_bytes`,
/// ADR-165 Decision 3), and promotes it to Active -- both are required for
/// `START` to resolve addressing at all. Sets BOTH `CP_CanRespUSDTId` and
/// `CP_CanRespUUDTId` to `resp_id` (Finding 1 fix, Codex review PR #42):
/// `response_header_bytes` reads USDT for `ChannelProtocol::ISO15765` and
/// UUDT for plain `ChannelProtocol::CAN` -- setting both lets this one
/// helper serve every test in this file regardless of which base protocol
/// `create_and_connect_cll` connected (every current caller uses plain CAN,
/// so only the UUDT half is load-bearing today, but the USDT half is kept
/// too so this helper stays correct if a future test connects ISO15765).
async fn set_can_addressing(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    phys_req_id: u32,
    resp_id: u32,
) {
    set_unique_resp_table_and_promote(
        client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, resp_id),
                unum32_param(CP_CAN_RESP_UUDT_ID, resp_id),
                unum32_param(CP_CAN_PHYS_REQ_ID, phys_req_id),
            ],
        )],
    )
    .await;
}

/// Item 1: `START` on a connected, J2534-2-opted-in, addressed CAN CLL
/// returns a `MsgId`; a subsequent `QUERY` with that `MsgId` succeeds and
/// reports the mock's "still live" status (`1`, ADR-173 Decision 3's Table
/// 53 polarity -- superseding ADR-165's original "0 == still active"
/// reading).
#[tokio::test]
#[serial]
async fn start_then_query_succeeds_on_a_connected_opted_in_can_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect(
        "PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a connected, opted-in, addressed \
             CAN CLL",
    );

    let status = query_repeat_message(&mut client, cll_handle, query_id, msg_id)
        .await
        .expect("PDU_IOCTL_QUERY_REPEAT_MESSAGE should succeed for a MsgId this CLL just started");
    assert_eq!(
        status, 1,
        "the mock reports status 1 (\"live\") for a still-live slot (ADR-173 Table 53 polarity)"
    );

    server.shutdown().await;
}

/// Item 2: `START` on a module NOT opted into J2534-2 is rejected --
/// `ioctl_start_repeat_message`'s exact mapping is `Code::InvalidArgument`
/// with a `PDU_ERR_ID_NOT_SUPPORTED` message (unlike `PDU_IOCTL_SW_CAN_HS`/
/// `_NS`'s `Code::Unimplemented`, since this gate gRPC-maps the emulated
/// PDU_ERR_ID_NOT_SUPPORTED status differently at this call site).
#[tokio::test]
#[serial]
async fn start_is_rejected_on_a_module_not_opted_into_j2534_2() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE should be rejected on a module that has not opted into \
         SAE J2534-2 (clause 5)",
    );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"),
        "{}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 3: `QUERY`/`STOP` with a `MsgId` belonging to a sibling CLL sharing
/// the same physical channel is rejected -- `require_owned_repeat_message`'s
/// per-CLL ownership check (ADR-165 Decision 4), verified end-to-end (not
/// just at the mock layer, which has no notion of CLL identity at all).
#[tokio::test]
#[serial]
async fn query_and_stop_reject_a_msg_id_owned_by_a_sibling_cll_on_the_shared_channel() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "sanity: cll_a and cll_b must actually share one physical channel for this test to \
         exercise the cross-CLL ownership check at all"
    );
    set_can_addressing(&mut client, cll_a, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    let msg_id = start_repeat_message(
        &mut client,
        cll_a,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on cll_a");

    let query_status = query_repeat_message(&mut client, cll_b, query_id, msg_id)
        .await
        .expect_err("PDU_IOCTL_QUERY_REPEAT_MESSAGE should reject a MsgId owned by a sibling CLL");
    assert_eq!(query_status.code(), Code::NotFound);
    assert!(
        query_status.message().contains("PDU_ERR_INVALID_MSG_ID"),
        "{}",
        query_status.message()
    );

    let stop_status = stop_repeat_message(&mut client, cll_b, stop_id, msg_id)
        .await
        .expect_err("PDU_IOCTL_STOP_REPEAT_MESSAGE should reject a MsgId owned by a sibling CLL");
    assert_eq!(stop_status.code(), Code::NotFound);
    assert!(
        stop_status.message().contains("PDU_ERR_INVALID_MSG_ID"),
        "{}",
        stop_status.message()
    );

    // cll_a, the actual owner, can still query its own MsgId -- the sibling
    // rejections above did not disturb it.
    let status = query_repeat_message(&mut client, cll_a, query_id, msg_id)
        .await
        .expect("cll_a should still be able to query the MsgId it actually owns");
    assert_eq!(status, 1, "still live (ADR-173 Table 53 polarity)");

    server.shutdown().await;
}

/// Item 4: `STOP` on a valid, owned `MsgId` succeeds, and a subsequent
/// `QUERY` on the same `MsgId` fails -- the slot is gone.
#[tokio::test]
#[serial]
async fn stop_succeeds_and_a_subsequent_query_on_the_same_msg_id_fails() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed on a valid, owned MsgId");

    let status = query_repeat_message(&mut client, cll_handle, query_id, msg_id)
        .await
        .expect_err("PDU_IOCTL_QUERY_REPEAT_MESSAGE should fail once the slot has been stopped");
    assert_eq!(status.code(), Code::NotFound);
    assert!(status.message().contains("PDU_ERR_INVALID_MSG_ID"));

    server.shutdown().await;
}

/// Coverage item 15 (ADR-173): `QUERY` through the full gRPC stack reports
/// the correct status transition (`1` -> `0`) after self-termination with no
/// explicit `STOP` (clause 14.2.2.3's `MsgId`-retention rule, Table 53's
/// QUERY status semantics), and the `MsgId` remains valid for a subsequent
/// `STOP` afterward -- only that explicit `STOP` finally makes a later
/// `QUERY` report `PDU_ERR_INVALID_MSG_ID`.
#[tokio::test]
#[serial]
async fn query_repeat_message_reports_the_live_to_terminated_status_transition_after_self_termination()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // Condition == 1, a short TimeInterval, and no matching frame ever
    // injected -- self-terminates on silence (ADR-173 Decision 1).
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(20, 1, vec![0x01, 0x02], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    let live_status = query_repeat_message(&mut client, cll_handle, query_id, msg_id)
        .await
        .expect("QUERY on a freshly started slot should succeed");
    assert_eq!(live_status, 1, "freshly started: live");

    // Poll through the gRPC surface itself (not the backdoor) until the
    // transition to terminated is observed.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    let mut terminated_status = None;
    while std::time::Instant::now() < deadline {
        let status = query_repeat_message(&mut client, cll_handle, query_id, msg_id)
            .await
            .expect("QUERY on a still-tracked, terminated-but-unstopped MsgId should succeed");
        if status == 0 {
            terminated_status = Some(status);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        terminated_status,
        Some(0),
        "QUERY must report the transition to status 0 (terminated-but-unstopped) once the \
         slot self-terminates, via a successful QUERY (not an error) -- the MsgId remains valid"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect(
            "STOP on the terminated-but-unstopped MsgId must still succeed -- the MsgId is \
             retained until this explicit STOP (clause 14.2.2.3)",
        );

    let after_stop = query_repeat_message(&mut client, cll_handle, query_id, msg_id)
        .await
        .expect_err("QUERY after the explicit STOP must now fail");
    assert_eq!(after_stop.code(), Code::NotFound);
    assert!(after_stop.message().contains("PDU_ERR_INVALID_MSG_ID"));

    server.shutdown().await;
}

/// Item 5 (first half): `DestroyComLogicalLink` on a CLL with a live,
/// unstopped repeat slot cleans it up (ADR-165 Decision 4's best-effort STOP
/// loop, `rpc_link.rs`) -- verified directly via the mock's own state
/// (`MockBackdoor::repeat_message_exists`, a raw `PassThruIoctl(QUERY_
/// REPEAT_MESSAGE)` call) since the CLL -- and any gRPC handle to query the
/// slot through the service -- is gone by the time this asserts.
#[tokio::test]
#[serial]
async fn destroy_com_logical_link_cleans_up_a_live_repeat_slot() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");
    assert!(
        server
            .backdoor
            .repeat_message_exists(MOCK_CHANNEL_ID, msg_id),
        "sanity: the slot should exist device-side right after START"
    );

    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("destroy_com_logical_link should succeed");

    assert!(
        !server
            .backdoor
            .repeat_message_exists(MOCK_CHANNEL_ID, msg_id),
        "DestroyComLogicalLink should have stopped this CLL's still-live repeat slot"
    );

    server.shutdown().await;
}

/// Item 5 (second half): `DisconnectComLogicalLink` does the same, via its
/// own (separate) best-effort STOP loop in `rpc_link.rs` (run before
/// `release_shared_channel_ref`, ADR-165 Decision 4).
#[tokio::test]
#[serial]
async fn disconnect_com_logical_link_cleans_up_a_live_repeat_slot() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");
    assert!(
        server
            .backdoor
            .repeat_message_exists(MOCK_CHANNEL_ID, msg_id)
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    assert!(
        !server
            .backdoor
            .repeat_message_exists(MOCK_CHANNEL_ID, msg_id),
        "DisconnectComLogicalLink should have stopped this CLL's still-live repeat slot"
    );

    server.shutdown().await;
}

/// Codex review, PR #42 round 22: `require_owned_repeat_message` checks
/// connection state BEFORE `MsgId` ownership -- `DisconnectComLogicalLink`
/// drains `repeat_message_ids` to empty as part of tearing down (see the
/// test just above), but leaves the CLL handle itself alive in
/// `logical_links` (only `Destroy` removes it), so a QUERY/STOP issued on
/// a disconnected-but-not-destroyed CLL must report `PDU_ERR_CLL_NOT_
/// CONNECTED` -- not the misleading `PDU_ERR_INVALID_MSG_ID` a
/// connection-agnostic ownership check would report for every `MsgId`,
/// including one this same CLL genuinely owned immediately before
/// disconnect.
#[tokio::test]
#[serial]
async fn query_after_disconnect_reports_not_connected_not_invalid_msg_id() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    let query_status = query_repeat_message(&mut client, cll_handle, query_id, msg_id)
        .await
        .expect_err(
            "QUERY on a disconnected CLL should fail, not silently succeed against a torn-down \
             channel",
        );
    assert_eq!(query_status.code(), Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&query_status).unwrap().pdu_error,
        PduError::PduErrCllNotConnected as i32,
        "a disconnected CLL must report PDU_ERR_CLL_NOT_CONNECTED, not PDU_ERR_INVALID_MSG_ID, \
         for a MsgId it genuinely owned immediately before disconnect"
    );

    let stop_status = stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect_err("STOP on a disconnected CLL should fail the same way QUERY does");
    assert_eq!(stop_status.code(), Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&stop_status).unwrap().pdu_error,
        PduError::PduErrCllNotConnected as i32
    );

    server.shutdown().await;
}

/// Companion to `query_after_disconnect_reports_not_connected_not_invalid_
/// msg_id` (round-22 edge-case-hunter follow-up): unlike `Disconnect`,
/// `DestroyComLogicalLink` removes the CLL's own entry from
/// `logical_links` outright (`rpc_link.rs`) rather than merely clearing
/// `channel_id` and draining `repeat_message_ids` while leaving the entry
/// alive -- `require_owned_repeat_message`'s `unknown_handle_status` guard
/// (its very first check, `rpc_misc.rs`) catches this before either the
/// connection check or the ownership check the round-22 reorder touches
/// ever runs, on both orderings. Pins that a destroyed CLL reports
/// PDU_ERR_INVALID_HANDLE (an unknown handle), not PDU_ERR_CLL_NOT_
/// CONNECTED -- a different, correct-by-construction outcome from the
/// disconnected-but-not-destroyed case the round-22 fix actually changed.
#[tokio::test]
#[serial]
async fn query_after_destroy_reports_unknown_handle_not_not_connected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("destroy_com_logical_link should succeed");

    let query_status = query_repeat_message(&mut client, cll_handle, query_id, msg_id)
        .await
        .expect_err("QUERY on a destroyed CLL handle should fail");
    assert_eq!(query_status.code(), Code::NotFound);
    assert_eq!(
        error_detail_from_status(&query_status).unwrap().pdu_error,
        PduError::PduErrInvalidHandle as i32,
        "a destroyed CLL handle is unrecognized entirely -- unknown_handle_status's own \
         PDU_ERR_INVALID_HANDLE, not the connection-state PDU_ERR_CLL_NOT_CONNECTED \
         disconnect-but-not-destroy reports"
    );

    let stop_status = stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect_err("STOP on a destroyed CLL handle should fail the same way QUERY does");
    assert_eq!(stop_status.code(), Code::NotFound);
    assert_eq!(
        error_detail_from_status(&stop_status).unwrap().pdu_error,
        PduError::PduErrInvalidHandle as i32
    );

    server.shutdown().await;
}

/// Item 6: missing/wrong-shaped `input_data` is rejected with
/// `invalid_argument` -- `START` without a `PDU_IT_IO_REPEAT_MESSAGE_SETUP`
/// (neither absent nor the wrong oneof variant), and `QUERY`/`STOP` without a
/// `PDU_IT_IO_UNUM32`.
#[tokio::test]
#[serial]
async fn wrong_shaped_input_data_is_rejected_for_all_three_commands() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // START with no input_data at all.
    let status = io_ctl_cll(&mut client, cll_handle, start_id, None, true)
        .await
        .expect_err("PDU_IOCTL_START_REPEAT_MESSAGE should reject a missing input_data");
    assert_eq!(status.code(), Code::InvalidArgument);

    // START with the wrong oneof variant (a bare Unum32Value, as QUERY/STOP
    // expect, rather than bytearray_data).
    let status = io_ctl_cll(
        &mut client,
        cll_handle,
        start_id,
        Some(unum32_input(1)),
        true,
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE should reject a Unum32Value input_data (wrong oneof \
             variant)",
    );
    assert_eq!(status.code(), Code::InvalidArgument);

    // QUERY with no input_data at all.
    let status = io_ctl_cll(&mut client, cll_handle, query_id, None, true)
        .await
        .expect_err("PDU_IOCTL_QUERY_REPEAT_MESSAGE should reject a missing input_data");
    assert_eq!(status.code(), Code::InvalidArgument);

    // QUERY with the wrong oneof variant (bytearray_data, as START expects,
    // rather than a bare Unum32Value).
    let status = io_ctl_cll(
        &mut client,
        cll_handle,
        query_id,
        Some(repeat_message_setup(1000, 0, vec![0x01], vec![], vec![])),
        true,
    )
    .await
    .expect_err(
        "PDU_IOCTL_QUERY_REPEAT_MESSAGE should reject a bytearray_data input_data (wrong \
         oneof variant)",
    );
    assert_eq!(status.code(), Code::InvalidArgument);

    // STOP with no input_data at all.
    let status = io_ctl_cll(&mut client, cll_handle, stop_id, None, false)
        .await
        .expect_err("PDU_IOCTL_STOP_REPEAT_MESSAGE should reject a missing input_data");
    assert_eq!(status.code(), Code::InvalidArgument);

    // STOP with the wrong oneof variant.
    let status = io_ctl_cll(
        &mut client,
        cll_handle,
        stop_id,
        Some(repeat_message_setup(1000, 0, vec![0x01], vec![], vec![])),
        false,
    )
    .await
    .expect_err(
        "PDU_IOCTL_STOP_REPEAT_MESSAGE should reject a bytearray_data input_data (wrong \
         oneof variant)",
    );
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Item 7 (Bug 3 regression, edge-case-hunter): `ioctl_start_repeat_message`'s
/// inlined TX-flags composition (copied out of `rpc_primitive::
/// apply_resolved_tx_flags` because that helper is private to
/// `rpc_primitive`) had omitted the FD-specific flags `rpc_primitive.rs`
/// always adds for an ordinary CoptSendrecv TX on an FD-connected link
/// (SAE J2534-2 21.4.4/Tables 99-100). An FD-connected link's repeat message
/// TX must carry `TX_FD_CAN_FORMAT`, and `TX_FD_CAN_BRS` too when a nonzero
/// `CP_CANFDBaudrate` is staged.
#[tokio::test]
#[serial]
async fn start_repeat_message_on_an_fd_connected_link_carries_tx_fd_can_format() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: staged FD ComParams should have substituted the native connect id to \
         FD_CAN_PS (ADR-158) -- otherwise this test would not actually be exercising an \
         FD-connected link"
    );
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on an FD-connected link");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_FORMAT,
        0,
        "an FD-connected link's repeat message TX must carry TX_FD_CAN_FORMAT (SAE J2534-2 \
         21.4.4)"
    );
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_BRS,
        0,
        "a nonzero staged CP_CANFDBaudrate requests the data-phase bit-rate switch"
    );

    // Round 15 (Codex review, ADR-165 PR #42, design-advisor consult): the
    // mask/pattern `PassThruMessage`s' own `TxFlags` (`RepeatMsgData[1]`/
    // `[2]`) must carry `TX_FD_CAN_FORMAT` too -- not for comparison purposes
    // (SAE J2534-2 21.2.2(g)/22.2.2(d) require FD-capable-channel filtering
    // to ignore CAN message format), but because 21.4.4 lets a device cap a
    // template's DataSize at 12 bytes without it. `TX_FD_CAN_BRS` must NOT be
    // set on the mask/pattern -- it is a bit-timing property with no
    // analogous DataSize-validity coupling on a never-transmitted template
    // (Table 93).
    let mask_pattern_tx_flags = server
        .backdoor
        .repeat_slot_mask_pattern_tx_flags(MOCK_CHANNEL_ID, msg_id);
    assert_ne!(
        mask_pattern_tx_flags & j2534_0404::TX_FD_CAN_FORMAT,
        0,
        "an FD-connected link's repeat mask/pattern TxFlags must carry TX_FD_CAN_FORMAT (SAE \
         J2534-2 21.4.4 template-DataSize validity): {mask_pattern_tx_flags:#06x}"
    );
    assert_eq!(
        mask_pattern_tx_flags & j2534_0404::TX_FD_CAN_BRS,
        0,
        "the repeat mask/pattern TxFlags must NOT carry TX_FD_CAN_BRS -- BRS is a bit-timing \
         property (Table 93), not a format discriminator, with no template-validity coupling: \
         {mask_pattern_tx_flags:#06x}"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 15 regression test (Codex review, ADR-165 PR #42 round 15, design-
/// advisor consult), corrected for ADR-173: SAE J2534-2 21.2.2(g)/22.2.2(d)
/// require FD-capable-channel filtering/matching to ignore CAN message
/// format entirely -- a `Condition == 1` repeat slot on an FD-connected
/// link must still recognize a MATCH when the incoming response frame's
/// `RxStatus` carries BOTH `RX_FLAG_FD_CAN_FORMAT` and `RX_FLAG_FD_CAN_BRS`,
/// exactly as it would without those bits. This is the load-bearing
/// regression test for the `j2534-0404-mock` masking fix
/// (`REPEAT_RESPONSE_FORMAT_RX_STATUS_MASK`, applied to
/// `RepeatSlot::response_format_rx_bits` at slot-creation time): without it,
/// `tx_format_flags_to_rx_status_bits`'s FD-bit translation would store
/// `RX_FLAG_FD_CAN_FORMAT` in `response_format_rx_bits` (since
/// `rpc_misc.rs`'s round-15 fix sets `TX_FD_CAN_FORMAT` on the mask/pattern's
/// `TxFlags` for every FD-link slot), which would never equal the masked
/// incoming `rx_status` `note_rx_frame_for_repeat_slots` compares against --
/// silently misclassifying every matching FD response as a NON-match and
/// terminating the slot immediately (ADR-173 Decision 1: a Condition == 1
/// slot terminates on the first non-matching frame).
///
/// **ADR-173 correction:** under the corrected `Condition == 1` semantics, a
/// MATCH does not terminate the slot (only a non-match or silence does) --
/// so "recognized as a match" is now proven by the slot staying LIVE, not by
/// it completing. A genuinely non-matching frame (different address) is
/// injected afterward to confirm this isn't merely "nothing ever
/// terminates" -- proof the match/non-match evaluation is actually running.
#[tokio::test]
#[serial]
async fn condition_one_slot_on_an_fd_connected_link_does_not_terminate_on_a_matching_frame_despite_fd_format_rx_status_bits()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: staged FD ComParams should have substituted the native connect id to \
         FD_CAN_PS (ADR-158) -- otherwise this test would not actually be exercising an \
         FD-connected link"
    );
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // TimeInterval is long enough that the slot cannot self-complete via
    // silence within this test's own wait below. Empty mask_data/
    // pattern_data: the full mask/pattern is exactly `response_header_
    // bytes`'s own 4-byte resolved response CAN Id header.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 1, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on an FD-connected link");

    // Matching address+data (0x7E8's 4-byte big-endian encoding), but with
    // BOTH FD RxStatus bits set -- as if the response genuinely arrived as
    // an FD frame with the bit-rate switch active.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        j2534_0404::FD_CAN_FORMAT_STATUS | j2534_0404::FD_CAN_BRS_STATUS,
    );

    // Generous margin past the mock's own 5ms repeat-worker poll step -- if
    // the FD bits were incorrectly compared as part of the match, this
    // matching-address+data frame would be misclassified as a non-match and
    // terminate the slot well within this window.
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "a matching-address+data response frame must be recognized as a MATCH regardless of \
         its RxStatus's FD_CAN_FORMAT/FD_CAN_BRS bits (SAE J2534-2 21.2.2(g)/22.2.2(d)), and a \
         match must not terminate a Condition == 1 slot (ADR-173 Decision 1)"
    );

    // Sanity: a genuinely non-matching frame (different address) still
    // terminates the slot -- proof match evaluation is actually running,
    // not merely "this slot never terminates at all".
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x123, &[]),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        j2534_0404::FD_CAN_FORMAT_STATUS | j2534_0404::FD_CAN_BRS_STATUS,
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    while server
        .backdoor
        .repeat_message_status(MOCK_CHANNEL_ID, msg_id)
        != Some(0)
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(0),
        "sanity: a genuinely non-matching frame (different address) must still terminate the \
         slot"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Backlog fix (round 20 sibling gap to Finding B), corrected for ADR-173: a
/// `Condition == 1` repeat slot on a KWP/ISO9141 link whose configured
/// response format uses the embedded-length encoding (`format & 0x3F != 0`,
/// e.g. `CP_PhysRespFormatPriorityType == 0x81`) must still recognize a
/// MATCH on a genuine ECU response whose own embedded LEN value differs from
/// that ComParam-configured one -- `response_header_bytes` wildcards the
/// format byte's low 6 bits in the stop-condition mask (mirrors
/// `tx_header.rs`'s unit test `response_header_bytes_kwp_embedded_len_
/// wildcards_low_six_bits_of_format_byte`, but exercised end-to-end through
/// the mock's own repeat-slot matching, `repeat_mask_pattern_matches`).
/// Before the original round-20 fix the format byte was an exact-match
/// position, so a real ECU response's own (unpredictable) embedded length
/// would never match at all. `CP_PhysRespFormatPriorityType` is
/// `PDU_PC_UNIQUE_ID`-classified (per-ECU) -- like every other test in this
/// file that configures a `PDU_PC_UNIQUE_ID` param, it must go through
/// `SetUniqueRespIdTable`/`set_unique_resp_table_and_promote`, not a plain
/// connect-time `SetComParam` (which `is_unique_id_param` rejects for this
/// ComParam).
///
/// **ADR-173 correction:** a MATCH no longer terminates a `Condition == 1`
/// slot -- "recognized as a match" is now proven by the slot staying LIVE
/// after the genuine-but-differently-embedded-length response, contrasted
/// with a genuinely non-matching response (wrong target byte) that DOES
/// terminate it, proving the wildcard match logic is actually running.
#[tokio::test]
#[serial]
async fn condition_one_slot_on_kwp_embedded_length_format_does_not_terminate_despite_differing_embedded_len()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![unum32_param(CP_PHYS_RESP_FORMAT_PRIORITY, 0x81)], // physical, embedded LEN = 1
        )],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // TimeInterval long enough that the slot cannot self-complete via
    // silence within this test's own wait below. Empty mask_data/
    // pattern_data: the full stop-condition template is exactly
    // response_header_bytes's own composed header.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 1, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a KWP embedded-length-format link");

    // A genuine ECU response: format 0x84 (physical, embedded LEN = 4 --
    // deliberately DIFFERENT from the ComParam-configured 0x81), target 0xF1
    // (tester, default), source 0x10 (ECU, default), 4-byte payload.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &[0x84, 0xF1, 0x10, 0x61, 0x00, 0x01, 0x02],
        j2534_0404::ISO9141,
    );

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "a genuine ECU response whose embedded-length format byte differs from the \
         ComParam-configured one must be recognized as a MATCH (the low 6 bits are a \
         wildcarded, not exact-match, mask position), and a match must not terminate a \
         Condition == 1 slot (ADR-173 Decision 1)"
    );

    // Sanity: a genuinely non-matching response (wrong target byte, 0xF2
    // instead of 0xF1) still terminates the slot.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &[0x84, 0xF2, 0x10, 0x61, 0x00, 0x01, 0x02],
        j2534_0404::ISO9141,
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    while server
        .backdoor
        .repeat_message_status(MOCK_CHANNEL_ID, msg_id)
        != Some(0)
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(0),
        "sanity: a genuinely non-matching response must still terminate the slot"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Accepted-residual pin (Codex review, PR #46; design-advisor consult; see
/// ADR-166's Consequences), corrected for ADR-173: a `Condition == 1` slot
/// on a KWP link configured with an embedded-length response format
/// (`CP_PhysRespFormatPriorityType` low 6 bits nonzero, e.g. `0x81`) has no
/// way to distinguish a genuine same-shape response (3-byte header,
/// embedded LEN) from a response that exceeds the embedded field's 63-byte
/// capacity and so uses the OTHER wire shape instead (4-byte header: format
/// with low 6 bits zero, target, source, an explicit length byte, THEN
/// payload) -- SAE J2534-2 clause 14's stop condition is a single
/// fixed-offset mask/pattern template and cannot express "low 6 bits
/// nonzero" as a match predicate, nor match two different header lengths at
/// once (this is an inherent limitation of the primitive, not a bug in this
/// service -- every alternative considered breaks the common embedded case
/// to guard this rarer one).
///
/// This test pins the resulting behavior as *documented*, not *fixed*: a
/// response using the other (explicit-length-byte) wire shape gets its
/// explicit length byte compared against the client's own first
/// mask/pattern byte instead of the real first payload byte, since the
/// service's 3-byte template only knows about the embedded-shape framing it
/// was configured for. Chosen so the explicit length byte (`0x62`) equals
/// the client's own expected first payload byte, while the REAL first
/// payload byte (`0x00`) does not -- demonstrating the byte-level template
/// misfires on the misaligned byte, not the genuine payload.
///
/// **ADR-173 correction to the residual's OBSERVABLE consequence** (the
/// underlying template-misalignment limitation itself is unchanged): under
/// the corrected `Condition == 1` semantics, this misaligned byte
/// coincidentally satisfying the template is a MATCH, and a match no longer
/// terminates the slot (only a non-match or silence does) -- so the
/// residual's symptom flips from "the slot completes on the wrong grounds"
/// (ADR-165) to "the slot incorrectly stays alive, treating the
/// wrong-shape response as if it matched, when a spec-conforming
/// implementation would have needed to recognize the real payload never
/// matched and kept running for that (correct) reason instead" -- the same
/// underlying inherent limitation, a differently-shaped observable outcome.
#[tokio::test]
#[serial]
async fn condition_one_slot_on_kwp_embedded_length_format_accepted_residual_shifts_on_alternate_wire_shape()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![unum32_param(CP_PHYS_RESP_FORMAT_PRIORITY, 0x81)], // physical, embedded LEN = 1
        )],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // Client-supplied pattern expects the real first payload byte to be
    // 0x62 (a plausible "positive response SID" byte) -- the full template
    // is [0xC0, 0xFF, 0xFF] (response_header_bytes) + [0xFF] (client mask)
    // over [format & 0xC0, target, source, client_pattern[0]].
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 1, vec![0x01, 0x02], vec![0xFF], vec![0x62]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a KWP embedded-length-format link");

    // A response using the OTHER wire shape: format 0x80 (physical, low 6
    // bits ZERO -- explicit length byte follows, e.g. because the real
    // payload is >= 64 bytes), target 0xF1, source 0x10, explicit length
    // byte 0x62 (deliberately equal to the client's own expected first
    // payload byte), then a real payload whose actual first byte (0x00)
    // does NOT match the client's expectation.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &[0x80, 0xF1, 0x10, 0x62, 0x00, 0x01, 0x02],
        j2534_0404::ISO9141,
    );

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "documents the accepted residual under ADR-173: the alternate wire shape's explicit \
         length byte (0x62) lands at the position the client's own pattern checks and is \
         treated as a MATCH, even though the real first payload byte (0x00) never matched -- a \
         match does not terminate a Condition == 1 slot, so the slot incorrectly stays live \
         instead of correctly recognizing this as a non-match and stopping -- see ADR-166's \
         Consequences"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 15 regression test, matching counterpart (Codex review, ADR-165 PR
/// #42 round 15, design-advisor consult), corrected for ADR-173: the
/// identical FD-connected `Condition == 1` slot setup as above, but the
/// injected response frame's `RxStatus` carries NO FD bits at all
/// (classic-format framing), still with matching address+data. Pins the
/// spec-required format-agnostic-matching behavior (21.2.2(g)/22.2.2(d))
/// against any future attempt to reintroduce an FD-format comparison into
/// the mask/pattern match -- proven, under ADR-173's corrected semantics, by
/// the slot staying LIVE (a match no longer terminates a `Condition == 1`
/// slot) rather than by it completing.
#[tokio::test]
#[serial]
async fn condition_one_slot_on_an_fd_connected_link_does_not_terminate_with_classic_format_rx_status()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: staged FD ComParams should have substituted the native connect id to \
         FD_CAN_PS (ADR-158) -- otherwise this test would not actually be exercising an \
         FD-connected link"
    );
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 1, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on an FD-connected link");

    // Matching address+data, RxStatus = 0 (no FD bits at all -- as if the
    // response arrived as a classic-format frame).
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        0,
    );

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "a matching-address+data response frame must be recognized as a MATCH even when its \
         RxStatus carries no FD bits at all -- SAE J2534-2 21.2.2(g)/22.2.2(d) require \
         FD-capable-channel matching to ignore CAN message format, and a match must not \
         terminate a Condition == 1 slot (ADR-173 Decision 1)"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Item 8 (coverage gap, edge-case-hunter): `ERR_EXCEEDED_LIMIT` at the
/// device's 10-slot-per-channel minimum, through the FULL gRPC `IoCtl` stack
/// -- previously only covered by the mock's own direct-FFI unit test
/// (`start_repeat_message_enforces_max_slots_per_channel`,
/// `j2534-0404-mock/src/lib.rs`). `ioctl_start_repeat_message` forwards the
/// native `ERR_EXCEEDED_LIMIT` unmodified (ADR-165 Decision 4: no new error-
/// code mapping needed, it is a pre-existing v04.04 code) via
/// `map_native_error_for_link`, which maps it to `PDU_ERR_RESOURCE_ERROR`
/// under an outer `Code::Internal` (`error::pdu_error_for`/
/// `map_native_error_as`: only `PDU_ERR_INVALID_HANDLE` gets a different
/// outer code).
#[tokio::test]
#[serial]
async fn start_repeat_message_reports_exceeded_limit_through_the_full_grpc_stack() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // The device's own minimum (MAX_REPEAT_SLOTS_PER_CHANNEL in
    // j2534-0404-mock/src/lib.rs) is 10 -- hard-coded here rather than
    // imported since it is private to that crate; SAE J2534-2 clause 14's
    // own minimum this mirrors.
    let mut msg_ids = Vec::new();
    for _ in 0..10 {
        let msg_id = start_repeat_message(
            &mut client,
            cll_handle,
            start_id,
            repeat_message_setup(5000, 0, vec![0x01, 0x02], vec![], vec![]),
        )
        .await
        .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed up to the device's 10-slot minimum");
        msg_ids.push(msg_id);
    }

    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect_err(
        "an 11th PDU_IOCTL_START_REPEAT_MESSAGE on the same physical channel should be \
         rejected once the device's slot minimum is exhausted",
    );
    // ADR-185 Stage 2: this module is opted into SAE J2534-2
    // (`start_j2534_2_server`), so the 11th attempt is now caught by the
    // Discovery-cache `ProtocolCapacity` precheck (cached
    // `PROTOCOL_INFO_MAX_REPEAT_MESSAGING` = 10, `needed` = 11) BEFORE the
    // native `PassThruIoctl(START_REPEAT_MESSAGE)` call is even reached --
    // `Code::FailedPrecondition` (`state_guard_status`), not the native
    // path's `Code::Internal` (`map_native_error_for_link`). The `PDUError`
    // itself is unchanged (ADR-185 Decision 5): only the outer gRPC code and
    // latency differ between the early Discovery rejection and the native
    // fallback this same scenario used to hit before Stage 2.
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrResourceError as i32
    );

    for msg_id in msg_ids {
        stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
            .await
            .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");
    }

    server.shutdown().await;
}

/// Coverage-gap investigation (edge-case-hunter, ADR-165 Consequences: "a
/// repeat response that also satisfies a pending COP's expected-response
/// wait" and the loopback/ADR-099 TX-discard interplay). Confirmed benign,
/// no code change: ADR-165 Decision 5 leaves `poll_rx_inner`/
/// `dispatch_due_tester_present` untouched, so a repeat message's own
/// response is delivered through the SAME `bind_frame` attribution
/// precedence table as any other unsolicited RX frame -- there is no
/// "this frame is a repeat's response" bit anywhere in that path for a
/// concurrently-running repeat slot to disturb. Separately, the repeat TX
/// itself never becomes visible to this service's poll loop at all in the
/// first place (`j2534-0404-mock`'s `spawn_repeat_worker` pushes directly to
/// `written_msgs`, never through `PassThruWriteMsgs`'s own
/// `CONFIG_LOOPBACK` echo path that `written_msgs`/`rx_queue` both feed off
/// of for an ordinary TX) -- matching ADR-165 Consequences' own
/// "device-autonomous repeat TX does not stamp last_bus_activity" bullet, so
/// there is no ADR-099 TX-discard interaction to exercise here either: the
/// repeat TX is simply never offered to that machinery as an RX frame to
/// begin with. This test proves the first half end-to-end: an ordinary
/// receive-only COP wait on a CLL still receives its expected response
/// correctly while a repeat slot on the very same channel is concurrently
/// running (including one whose own mask/pattern happens to ALSO match the
/// same injected frame, exercising the two side by side).
#[tokio::test]
#[serial]
async fn a_running_repeat_slot_does_not_disrupt_ordinary_cop_response_delivery() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    // A long-interval repeat slot whose mask/pattern (0xFF/0x62, matched
    // against the payload's leading byte after this CLL's own expected-
    // response header is prepended -- ADR-165 Decision 3) ALSO matches the
    // frame injected below, so this exercises both mechanisms observing the
    // same frame side by side, not just their mere coexistence.
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 0, vec![0x01, 0x02], vec![0xFF], vec![0x62]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x62, 0xF1, 0x90, 0x41];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::CAN,
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_eq!(
        result.data_bytes, payload,
        "the receive-only COP's wait must still be satisfied normally by the injected frame \
         while a repeat slot -- including one whose own mask/pattern also matches this exact \
         frame -- is concurrently running on the same channel"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Finding 1 regression test (Codex review, ADR-165 PR #42):
/// `PDU_IOCTL_START_REPEAT_MESSAGE` on a plain (non-ISO15765) CAN link only
/// ever has `CP_CanRespUUDTId`/`_Format`/`_ExtAddr` configured -- a raw CAN
/// preset never sets the ISO15765-specific USDT/FlowControl fields
/// `response_header_bytes` incorrectly required regardless of protocol
/// before this fix. `START` must succeed here even though only UUDT fields
/// are set on the UniqueRespIdTable entry.
#[tokio::test]
#[serial]
async fn start_succeeds_on_a_plain_can_link_with_only_uudt_response_addressing() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x7E8),
            ],
        )],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect(
        "PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a plain CAN link whose \
         UniqueRespIdTable entry only sets CP_CanRespUUDTId (not the ISO15765-specific \
         CP_CanRespUSDTId)",
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// ADR-173 Decision 4 (coverage item 11, reverses the round-7 Finding 1
/// condition-0 carve-out, Codex review ADR-165 PR #42 round 7): SAE
/// J2534-2 clause 14.2.2.1's `Condition == 0` (`REPEAT_MESSAGE_UNTIL_MATCH`)
/// DOES have the device evaluate the mask/pattern against incoming traffic
/// -- a matching frame is precisely what stops it -- so a `Condition == 0`
/// link now needs a resolvable response header exactly as much as
/// `Condition == 1` already correctly required
/// (`start_with_condition_one_still_rejects_without_a_resolvable_response_
/// header`, just below, is the condition-1 twin this test now mirrors). A
/// link with only `CP_CanPhysReqId` configured (no response-id ComParam at
/// all, via `set_can_phys_req_id_and_promote`) is now rejected under
/// `condition == 0` too -- **client-visible behavior change** (ADR-173
/// Decision 4's own note): previously silently accepted since the old code
/// never composed a header for this condition.
#[tokio::test]
#[serial]
async fn start_with_condition_zero_now_rejects_without_a_resolvable_response_header() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    // Only CP_CanPhysReqId is set -- no CP_CanRespUUDTId/_USDTId, so
    // `response_header_bytes` cannot resolve a response header for this
    // link.
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE with condition == 0 must now be rejected when the \
         response header cannot be resolved -- ADR-173 Decision 4 reverses the prior \
         condition-0 carve-out",
    );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("CP_CanRespUUDTId"),
        "{}",
        status.message()
    );

    server.shutdown().await;
}

/// Round 7 Finding 1 regression test, condition-1 counterpart: the same
/// under-configured link (only `CP_CanPhysReqId`, no response-id ComParam)
/// must still be rejected when `condition == 1` (stop-on-match-or-timeout),
/// since that mode genuinely needs a resolvable response header to build the
/// mask/pattern.
#[tokio::test]
#[serial]
async fn start_with_condition_one_still_rejects_without_a_resolvable_response_header() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 1, vec![0x01, 0x02], vec![0xFF], vec![0x01]),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE with condition == 1 should still be rejected when the \
         response header cannot be resolved -- that mode genuinely needs it",
    );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("CP_CanRespUUDTId"),
        "{}",
        status.message()
    );

    server.shutdown().await;
}

/// Round 7 Finding 2 regression test (Codex review, ADR-165 PR #42 round 7):
/// `RepeatMessageSetup.tx_flag_bits` (packed into `bytearray_data` per
/// ADR-178) lets a client request pass-through TX
/// flags (mirroring `ComPrimitiveCtrlData.tx_flag`'s `TxFlagBits` for an
/// ordinary CoptSendrecv TX) on the transmitted `RepeatMsgData[0]` message --
/// here, `TX_ISO15765_FRAME_PAD` on an ISO15765-connected link.
#[tokio::test]
#[serial]
async fn start_repeat_message_with_tx_flag_bits_carries_iso15765_frame_pad() {
    let server = start_j2534_2_server().await;
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
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
            ],
        )],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup_with_tx_flags(
            1000,
            0,
            vec![0x01, 0x02],
            vec![],
            vec![],
            vec![vci_service_interface::TxFlagBit::TxFlagIso15765FramePad as i32],
        ),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE with tx_flag_bits should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_ne!(
        tx_flags & j2534_0404::TX_ISO15765_FRAME_PAD,
        0,
        "tx_flag_bits: [TxFlagIso15765FramePad] should carry TX_ISO15765_FRAME_PAD onto the \
         transmitted RepeatMsgData[0] message"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Finding 3 regression test (Codex review, ADR-165 PR #42; condition-0
/// carve-out added round 8): `PDU_IOCTL_START_REPEAT_MESSAGE` rejects a
/// `mask_data`/`pattern_data` length mismatch up front when `condition == 1`
/// -- SAE J2534-2 clause 14's `RepeatMsgData[1]`/`[2]` is a matched
/// mask/pattern pair that `condition == 1` (stop-on-match-or-timeout)
/// genuinely evaluates, and this guarantees the mock never sees a mismatched
/// pair to (mis)compare against in the first place.
#[tokio::test]
#[serial]
async fn start_rejects_mismatched_length_mask_and_pattern_data_under_condition_one() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 1, vec![0x01, 0x02], vec![0xFF, 0xFF], vec![0x55]),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE should reject mask_data/pattern_data of different \
         lengths when condition == 1",
    );
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// ADR-173 Decision 4 (coverage item 12, reverses the round-8 Finding 3
/// condition-0 carve-out, Codex review ADR-165 PR #42 round 8):
/// `condition == 0` now evaluates `RepeatMsgData[1]`/`[2]` (mask/pattern)
/// against incoming traffic exactly like `condition == 1` does (clause
/// 14.2.2.1), so a mismatched-length `mask_data`/`pattern_data` pair is now
/// rejected for `condition == 0` too -- mirrors
/// `start_rejects_mismatched_length_mask_and_pattern_data_under_condition_one`
/// above, now unconditional.
#[tokio::test]
#[serial]
async fn start_with_condition_zero_now_rejects_mismatched_length_mask_and_pattern_data() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![0x01]),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE must now reject mismatched-length mask_data/ \
         pattern_data under condition == 0 too -- ADR-173 Decision 4 reverses the prior \
         condition-0 carve-out",
    );
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Finding C regression test (Codex review, ADR-165 PR #42 round 2): a
/// `MsgId` this service still tracks as owned, but that the DEVICE has
/// already forgotten (`ERR_INVALID_MSG_ID`) for a reason this service was
/// never told about, must have its `MsgId` pruned from `LogicalLinkState::
/// repeat_message_ids` the first time this service learns about the
/// mismatch (a `QUERY`/`STOP` call that hits `ERR_INVALID_MSG_ID`), not just
/// on a successful `STOP`.
///
/// **ADR-173 note:** under the corrected clause 14 model, the mock itself
/// never spontaneously forgets a slot -- self-termination is *retained*
/// (QUERY status `0`), not removed, until an explicit STOP (Decision 3), so
/// "a `Condition == 1` slot self-completes device-side" (this test's
/// pre-ADR-173 trigger) no longer produces `ERR_INVALID_MSG_ID` at all. The
/// staleness this test exercises is manufactured directly instead, via
/// `MockBackdoor::stop_repeat_message_directly` (a raw STOP issued straight
/// against the mock, bypassing this service's own tracking) -- simulating
/// what a real, non-thin-forwarded vendor DLL genuinely forgetting a slot
/// for an out-of-band reason would look like from this service's point of
/// view: `repeat_message_ids` still claims it, the device does not.
///
/// Proven observably through the gRPC surface alone (no direct field
/// introspection available at this layer): a SECOND `QUERY` on the exact
/// same, already-device-forgotten `MsgId` must be rejected by THIS
/// SERVICE'S OWN ownership check (`require_owned_repeat_message`, whose
/// rejection text explicitly says "was not started by this
/// ComLogicalLink") rather than by forwarding to the native call again
/// (whose failure text would instead read "PassThruIoctl
/// QUERY_REPEAT_MESSAGE failed: ..."). The FIRST query still reaches the
/// native call (the stale entry has not been pruned yet) and gets the
/// native failure text; the fix's effect is only observable on the SECOND
/// query, once the first one has pruned the entry.
#[tokio::test]
#[serial]
async fn a_device_forgotten_condition_one_slot_is_pruned_on_the_first_query_that_notices() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 1, vec![0x01, 0x02], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    // Manufacture the "device forgot this MsgId without telling this
    // service" scenario directly -- see this test's own doc comment.
    assert!(
        server
            .backdoor
            .stop_repeat_message_directly(MOCK_CHANNEL_ID, msg_id),
        "sanity: the direct STOP against the mock should succeed while the slot is still live"
    );
    assert!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id)
            .is_none(),
        "sanity: the device should now genuinely have forgotten this MsgId"
    );

    // First QUERY: this service still has the stale MsgId tracked, so the
    // ownership check passes and the call reaches the native layer, which
    // reports ERR_INVALID_MSG_ID. This call site is exactly where the fix
    // prunes the stale entry from repeat_message_ids.
    let first_query_status = query_repeat_message(&mut client, cll_handle, query_id, msg_id)
        .await
        .expect_err("QUERY on a device-forgotten MsgId should fail");
    assert_eq!(first_query_status.code(), Code::NotFound);
    assert!(
        first_query_status
            .message()
            .contains("PassThruIoctl QUERY_REPEAT_MESSAGE"),
        "the FIRST query should still reach the native call (not yet pruned): {}",
        first_query_status.message()
    );

    // Second QUERY on the exact same MsgId: without the fix, this would
    // still be "owned" (never pruned) and reach the native call again,
    // producing the identical native-failure text as the first query. With
    // the fix, the first query already pruned it, so this one is rejected
    // by this service's own ownership check instead -- a materially
    // different, directly observable error message.
    let second_query_status = query_repeat_message(&mut client, cll_handle, query_id, msg_id)
        .await
        .expect_err("QUERY on the same, now-pruned MsgId should still fail");
    assert_eq!(second_query_status.code(), Code::NotFound);
    assert!(
        second_query_status
            .message()
            .contains("was not started by this ComLogicalLink"),
        "the SECOND query must be rejected by this service's own ownership check -- proof the \
         first query's ERR_INVALID_MSG_ID pruned the stale MsgId from repeat_message_ids \
         instead of leaving it tracked forever: {}",
        second_query_status.message()
    );

    server.shutdown().await;
}

/// Finding C, STOP counterpart: the same self-completion/pruning behavior,
/// but observed via `PDU_IOCTL_STOP_REPEAT_MESSAGE` instead of `_QUERY_`
/// (`ioctl_stop_repeat_message` has its own, separate prune-on-
/// `ERR_INVALID_MSG_ID` fix). See the QUERY counterpart's doc comment
/// (`a_device_forgotten_condition_one_slot_is_pruned_on_the_first_query_
/// that_notices`) for why this uses `MockBackdoor::
/// stop_repeat_message_directly` rather than self-completion (ADR-173).
#[tokio::test]
#[serial]
async fn a_device_forgotten_condition_one_slot_is_pruned_on_the_first_stop_that_notices() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 1, vec![0x01, 0x02], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    assert!(
        server
            .backdoor
            .stop_repeat_message_directly(MOCK_CHANNEL_ID, msg_id),
        "sanity: the direct STOP against the mock should succeed while the slot is still live"
    );
    assert!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id)
            .is_none(),
        "sanity: the device should now genuinely have forgotten this MsgId"
    );

    // First STOP: reaches the native call (not yet pruned), fails with the
    // native ERR_INVALID_MSG_ID mapping -- and this is where the fix prunes
    // the stale entry.
    let first_stop_status = stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect_err("STOP on a device-forgotten MsgId should fail");
    assert_eq!(first_stop_status.code(), Code::NotFound);
    assert!(
        first_stop_status
            .message()
            .contains("PassThruIoctl STOP_REPEAT_MESSAGE"),
        "the FIRST stop should still reach the native call (not yet pruned): {}",
        first_stop_status.message()
    );

    // Second STOP on the exact same MsgId: rejected by this service's own
    // ownership check instead, proving the first STOP pruned it.
    let second_stop_status = stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect_err("STOP on the same, now-pruned MsgId should still fail");
    assert_eq!(second_stop_status.code(), Code::NotFound);
    assert!(
        second_stop_status
            .message()
            .contains("was not started by this ComLogicalLink"),
        "the SECOND stop must be rejected by this service's own ownership check: {}",
        second_stop_status.message()
    );

    server.shutdown().await;
}

/// Finding H regression test (Codex review, ADR-165 PR #42 round 4): SAE
/// J2534-2 clause 14's device-autonomous retransmission model has no way to
/// drive the per-retransmission ISO-TP segmentation/flow-control a
/// software-ISO-TP link (`can_channel_mode = "software-isotp"`, ADR-046)
/// requires -- `START` must be rejected outright on such a link, not silently
/// accepted and sent as a malformed raw-CAN frame with no ISO-TP header.
/// Mirrors `fd_can.rs`'s `fd_mode_rejected_when_link_is_software_isotp` setup
/// (the analogous connect-time rejection for the FD + software-ISO-TP
/// incompatibility) -- unlike that case, `ConnectComLogicalLink` itself must
/// still succeed here (there is no reason to reject software-ISO-TP itself at
/// connect time, only Repeat Messaging on such a link), so the rejection is
/// only observable at `START_REPEAT_MESSAGE` time.
#[tokio::test]
#[serial]
async fn start_is_rejected_on_a_software_isotp_link() {
    let server = TestServer::try_start_with_extra_config(&format!(
        "can_channel_mode = \"software-isotp\"\n{}",
        modules_toml(&[("Bench 1", "J2534-2:mock")])
    ))
    .await
    .expect("service should initialize with a valid modules + can_channel_mode config");
    let mut client = server.client().await;

    // ISO15765 is the protocol family software-ISO-TP mode actually applies
    // to (ADR-046); no UniqueRespIdTable addressing is configured, since the
    // rejection must happen before any addressing resolution is attempted.
    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE must be rejected on a software-ISO-TP link -- clause \
         14's device-autonomous retransmission model cannot drive software ISO-TP framing",
    );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"),
        "{}",
        status.message()
    );
    assert!(
        status.message().contains("software-ISO-TP"),
        "error message should explain the rejection is about software-ISO-TP: {}",
        status.message()
    );

    server.shutdown().await;
}

/// ADR-186 regression test (supersedes the former Finding I test, Codex
/// review ADR-165 PR #42 round 4, which asserted the OLD, nonconformant
/// behavior this fix removes): `RepeatMsgData[0]` is a periodic message --
/// SAE J2534-1 v04.04 §7.2.7 caps it at a single frame (<DataSize> <= 12
/// bytes) regardless of protocol, NOT relaxed by TX_FD_CAN_FORMAT (clause
/// 21.2.2(h)). A raw CAN FD (`FD_CAN_PS`) repeat message therefore has no
/// payload headroom beyond 8 bytes to pad into at all -- the round-4 padding
/// step this test used to pin (growing a 9-byte payload up to a
/// wire-legal-but-oversized 12-byte CAN FD DLC and sending it) was itself
/// nonconformant and has been deleted; a real conforming device would reject
/// such a start with `ERR_INVALID_MSG` (clause 14.2.2.1). A 9-byte
/// `RepeatMsgData[0]` payload (DataSize 13) must now be rejected with
/// `InvalidArgument` before composition, not padded and sent.
#[tokio::test]
#[serial]
async fn start_repeat_message_on_an_fd_connected_link_rejects_a_payload_over_the_periodic_cap() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: staged FD ComParams should have substituted the native connect id to \
         FD_CAN_PS (ADR-158) -- otherwise this test would not actually be exercising an \
         FD-connected link"
    );
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    // 9 bytes -- produces a 13-byte full message (4-byte CAN ID header + 9
    // payload bytes), one over the periodic-message cap's 12-byte ceiling
    // for the raw CAN family (FD or classic).
    let payload: Vec<u8> = (1..=9).collect();
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, payload, vec![], vec![]),
    )
    .await
    .expect_err(
        "a 9-byte repeat_msg_data on an FD-connected raw CAN link must be rejected -- SAE \
         J2534-1 7.2.7's periodic-message cap is not relaxed by TX_FD_CAN_FORMAT",
    );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("7.2.7"),
        "the rejection should cite SAE J2534-1 7.2.7's periodic-message cap: {}",
        status.message()
    );

    server.shutdown().await;
}

/// ADR-186 companion test: exactly AT the periodic-message cap (an 8-byte
/// payload, DataSize 12) must still be accepted, still carry
/// `TX_FD_CAN_FORMAT` (SAE J2534-2 21.4.4 template/message-DataSize
/// validity), and be sent WITHOUT any padding -- proving the boundary itself
/// is correct (not off-by-one in either direction) and that the deleted
/// round-4 padding step is not missed for an already wire-legal payload.
#[tokio::test]
#[serial]
async fn start_repeat_message_on_an_fd_connected_link_accepts_a_payload_at_the_periodic_cap() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: staged FD ComParams should have substituted the native connect id to \
         FD_CAN_PS (ADR-158) -- otherwise this test would not actually be exercising an \
         FD-connected link"
    );
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // 8 bytes -- exactly the periodic cap's 12-byte DataSize ceiling (4-byte
    // CAN ID header + 8 payload bytes), and already a legal CAN FD DLC size,
    // so no padding is needed or expected.
    let payload: Vec<u8> = (1..=8).collect();
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, payload.clone(), vec![], vec![]),
    )
    .await
    .expect("an 8-byte repeat_msg_data (DataSize 12) must be accepted at the periodic cap");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    let mut expected = 0x7E0_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);
    assert_eq!(
        written, expected,
        "an already-at-cap payload must be sent verbatim, with no padding appended: {written:?}"
    );

    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_FORMAT,
        0,
        "an FD-connected link's repeat message TX must still carry TX_FD_CAN_FORMAT (SAE \
         J2534-2 21.4.4)"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Finding J regression test (Codex review, ADR-165 PR #42 round 5): an
/// oversized raw CAN FD (`FD_CAN_PS`) repeat message payload must be
/// cleanly rejected with `InvalidArgument`, not crash the service. The
/// original round-5 concern (`fd_can_padded_data_len` in `comparam_id.rs`
/// panicking on an unvalidated, out-of-range `data_len`) is now structurally
/// unreachable for this family: ADR-186's periodic-message cap check (SAE
/// J2534-1 7.2.7) rejects any raw CAN family payload over 8 bytes long
/// before the deleted padding step (which used to call
/// `fd_can_padded_data_len`) would ever have run, and a `debug_assert!` at
/// that step's former call site now guards the invariant instead. Kept as a
/// belt-and-suspenders "does not crash on a very oversized payload"
/// regression rather than deleted outright. Uses a 100-byte payload, well
/// over both the wire ceiling and this link's staged `CP_CANFDTxMaxDataLength`
/// (also 64 here). A subsequent, ordinary `START_REPEAT_MESSAGE` with a valid
/// payload succeeding on the SAME connection afterward confirms the
/// service process itself is still alive and the gRPC connection intact --
/// not merely that this one call happened to return an error status.
#[tokio::test]
#[serial]
async fn start_repeat_message_rejects_an_oversized_raw_can_fd_payload_instead_of_panicking() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: staged FD ComParams should have substituted the native connect id to \
         FD_CAN_PS (ADR-158) -- otherwise this test would not actually be exercising an \
         FD-connected link"
    );
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // 100 bytes -- over both the 64-byte CAN FD wire ceiling and this
    // link's staged CP_CANFDTxMaxDataLength (also 64 here).
    let oversized_payload: Vec<u8> = (0..100u32).map(|i| i as u8).collect();
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, oversized_payload, vec![], vec![]),
    )
    .await
    .expect_err(
        "an oversized raw CAN FD repeat message payload must be cleanly rejected, not \
         accepted or crash the service",
    );
    assert_eq!(status.code(), Code::InvalidArgument);

    // The service process itself must still be alive and the connection
    // intact -- an ordinary, valid START_REPEAT_MESSAGE on the same
    // connection must still succeed.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect(
        "the service must still be alive and the connection intact after rejecting the \
         oversized payload -- a valid START_REPEAT_MESSAGE on the same connection must \
         still succeed",
    );
    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Finding K regression test (Codex review, ADR-165 PR #42 round 5): a
/// plain (non-ISO15765) CAN link with extended addressing configured on its
/// physical request ComParams must NOT have `ISO15765_ADDR_TYPE` set on the
/// actually-TRANSMITTED `RepeatMsgData[0]` message's `TxFlags` -- that flag
/// is an ISO15765-only extended-addressing indicator (SAE J2534-1 Table
/// B.13), and a conforming raw-CAN adapter may reject a message that sets
/// it. Mirrors round 3's Finding D regression test for
/// `response_header_bytes` (`tx_header.rs`'s
/// `response_header_bytes_can_extended_addressing_never_sets_iso15765_addr_type`),
/// but for this function's own separate, actually-transmitted-message
/// composition. Asserts on the real transmitted frame's `TxFlags` via
/// `MockBackdoor::written_tx_flags`, the same introspection the round-4
/// FD-padding test above uses. CP_CanPhysReqFormat 0x0A (29-bit + extended
/// addressing) is the same format value `tx_header.rs`'s own
/// `response_header_bytes_can_appends_ae_byte_under_extended_addressing`
/// test uses for its response-side equivalent.
#[tokio::test]
#[serial]
async fn start_repeat_message_on_a_raw_can_link_never_sets_iso15765_addr_type() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x18DAF110),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x18DAF110),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x18DA10F1),
                unum32_param(CP_CAN_PHYS_REQ_FORMAT, 0x0A), // 29-bit + extended addressing
                unum32_param(CP_CAN_PHYS_REQ_EXT_ADDR, 0xF1),
            ],
        )],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on an extended-addressing raw-CAN link");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        tx_flags & j2534_0404::ISO15765_ADDR_TYPE,
        0,
        "ISO15765_ADDR_TYPE must never be set on a raw-CAN (non-ISO15765) repeat message's \
         TxFlags, even under extended addressing: {tx_flags:#06x}"
    );
    assert_ne!(
        tx_flags & j2534_0404::TX_EXTENDED_ID,
        0,
        "TX_EXTENDED_ID (29-bit CAN ID width) must still be set -- unconditional on \
         protocol: {tx_flags:#06x}"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Finding L regression test 1 (Codex review, ADR-165 PR #42 round 5,
/// ADR-123): a sibling CLL holding `LOCK_PHYSICAL_TX_QUEUE` on the shared
/// physical resource must block `PDU_IOCTL_START_REPEAT_MESSAGE` -- Repeat
/// Messaging is a genuinely autonomous, device-driven TX stream with no
/// "queue and wait for the lock" fallback the way an ordinary COP has under
/// ADR-123, so it hard-rejects instead, mirroring
/// `ioctl_clear_tx_queue`'s own `LOCK_PHYSICAL_TX_QUEUE` gate (see
/// `locks_and_param_classes.rs`'s established `LockResource` setup pattern,
/// mirrored here).
#[tokio::test]
#[serial]
async fn start_repeat_message_is_rejected_while_a_sibling_holds_the_physical_tx_queue_lock() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "sanity: cll_a and cll_b must actually share one physical channel for this test to \
         exercise the cross-CLL lock check at all"
    );

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("lock_resource should succeed");

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let status = start_repeat_message(
        &mut client,
        cll_b,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect_err(
        "START_REPEAT_MESSAGE from a non-holding sibling CLL should be rejected while another \
         CLL holds LOCK_PHYSICAL_TX_QUEUE on the shared physical resource",
    );
    assert_eq!(status.code(), Code::ResourceExhausted);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrRscLockedByOtherCll as i32
    );

    server.shutdown().await;
}

/// Finding L regression test 2 (Codex review, ADR-165 PR #42 round 5,
/// ADR-123): a live repeat-message slot on a sibling CLL sharing the
/// physical resource must block a `LockResource(LOCK_PHYSICAL_TX_QUEUE)`
/// grant -- Repeat Messaging never goes through `executing_cop`/
/// `primitives` at all (ADR-165), so `rpc_lock_resource`'s pre-existing
/// Fix C (ADR-123) active-transmission scan cannot see it on its own; this
/// is the companion check added directly over `repeat_message_ids`.
#[tokio::test]
#[serial]
async fn lock_resource_is_rejected_while_a_sibling_has_a_live_repeat_message_slot() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "sanity: cll_a and cll_b must actually share one physical channel for this test to \
         exercise the cross-CLL lock check at all"
    );
    set_can_addressing(&mut client, cll_a, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_a,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on cll_a");

    let status = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_b),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect_err(
            "LockResource(LOCK_PHYSICAL_TX_QUEUE) should be rejected while a sibling CLL has a \
             live repeat-message slot actively transmitting on the shared physical resource",
        );
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrFctFailed as i32
    );

    stop_repeat_message(&mut client, cll_a, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 6 regression test (Codex review, ADR-165 PR #42 round 6): before
/// this fix, `ioctl_start_repeat_message` had exactly one size-range check
/// anywhere in the function -- the FD_CAN_PS-specific one (Finding J,
/// round 5) nested inside `if fd_link { if base_protocol_id == CAN { ... } }`
/// -- so a classic (non-FD) raw CAN link had NO general SAE J2534-1 TX size
/// check (ADR-049) at all. `ChannelProtocol::CAN.tx_message_size_range(false)`
/// is `4..=12` (4-byte CAN ID + up to 8 payload bytes), so a 9-byte
/// `repeat_msg_data` payload composes a 13-byte `full_message` -- this must
/// now be cleanly rejected with `InvalidArgument`, not silently forwarded to
/// `PassThruMessage::new`/the native IOCTL. A subsequent, ordinary
/// `START_REPEAT_MESSAGE` with a valid payload succeeding on the same
/// connection afterward confirms the service process itself is still alive,
/// mirroring the round-5 Finding J regression test's own follow-up-call
/// pattern above.
#[tokio::test]
#[serial]
async fn start_repeat_message_rejects_an_oversized_classic_can_payload() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_ne!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: this link must be classic (non-FD) CAN for this test to exercise the general \
         (non-FD-specific) size-range check"
    );
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // 9 payload bytes -- one over classic CAN's 8-byte Max Tx payload
    // (4..=12 header-inclusive range), so the composed 13-byte full_message
    // must be rejected.
    let oversized_payload: Vec<u8> = (1..=9).collect();
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, oversized_payload, vec![], vec![]),
    )
    .await
    .expect_err(
        "an oversized classic (non-FD) CAN repeat message payload must be cleanly rejected, \
         not silently forwarded to the native adapter",
    );
    assert_eq!(status.code(), Code::InvalidArgument);

    // The service process itself must still be alive and the connection
    // intact -- an ordinary, valid START_REPEAT_MESSAGE on the same
    // connection must still succeed.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect(
        "the service must still be alive and the connection intact after rejecting the \
         oversized payload -- a valid START_REPEAT_MESSAGE on the same connection must \
         still succeed",
    );
    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 6 regression test (Codex review, ADR-165 PR #42 round 6): the
/// boundary companion to
/// `start_repeat_message_rejects_an_oversized_classic_can_payload` above --
/// an 8-byte payload (the maximum legal classic CAN payload, producing
/// exactly the 12-byte upper bound of `ChannelProtocol::CAN
/// .tx_message_size_range(false)`'s `4..=12` range) must still be accepted,
/// confirming the new general size-range check does not over-reject at the
/// legal boundary.
#[tokio::test]
#[serial]
async fn start_repeat_message_accepts_a_classic_can_payload_at_the_exact_size_boundary() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // 8 payload bytes -- classic CAN's maximum legal payload, producing
    // exactly a 12-byte full_message (the upper bound of the 4..=12 range).
    let boundary_payload: Vec<u8> = (1..=8).collect();
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, boundary_payload.clone(), vec![], vec![]),
    )
    .await
    .expect(
        "an 8-byte classic CAN repeat message payload (producing exactly the 12-byte upper \
         bound) must be accepted, not rejected as oversized",
    );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    let mut expected = 0x7E0_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&boundary_payload);
    assert_eq!(
        written, expected,
        "the boundary-size payload must be transmitted verbatim, with no padding or \
         truncation: {written:?}"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 6 regression test (Codex review, ADR-165 PR #42 round 6, third
/// occurrence of the staleness bug class after round 2 Finding C and round 3
/// Finding E), corrected for ADR-173 (coverage item 14): a sibling
/// `Condition == 1` repeat slot that has self-terminated but NOT been
/// explicitly stopped (QUERY status `0`) must not block a
/// `LockResource(LOCK_PHYSICAL_TX_QUEUE)` grant -- ADR-173 Decision 3's
/// `prune_stale_repeat_message_ids` polarity split: status 0 is retained in
/// `repeat_message_ids` (the claim stays valid until an explicit STOP) but
/// does NOT count as "actively transmitting" for this grant check.
///
/// **ADR-173 note (supersedes this test's pre-ADR-173 premise):** under the
/// corrected clause 14 model, self-termination no longer makes the mock
/// forget the slot (`ERR_INVALID_MSG_ID`) -- it is retained at status 0
/// until an explicit STOP (Decision 3). So unlike the pre-ADR-173 version of
/// this test, the grant's probe does NOT prune `msg_id` from cll_a's
/// tracking: a subsequent QUERY from cll_a still succeeds (status 0), and
/// the MsgId remains valid for a later explicit STOP.
#[tokio::test]
#[serial]
async fn lock_resource_is_granted_once_a_siblings_repeat_message_slot_has_self_terminated_but_not_stopped()
 {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "sanity: cll_a and cll_b must actually share one physical channel for this test to \
         exercise the cross-CLL lock check at all"
    );
    set_can_addressing(&mut client, cll_a, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // Condition == 1, a short TimeInterval, and no matching frame ever
    // injected -- self-terminates on silence (ADR-173 Decision 1), leaving a
    // status-0 (terminated-but-unstopped) MsgId in cll_a's repeat_message_ids.
    let msg_id = start_repeat_message(
        &mut client,
        cll_a,
        start_id,
        repeat_message_setup(20, 1, vec![0x01, 0x02], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on cll_a");

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    while server
        .backdoor
        .repeat_message_status(MOCK_CHANNEL_ID, msg_id)
        != Some(0)
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(0),
        "sanity: the Condition == 1 slot should have self-terminated (status 0) within the \
         poll bound, but remain tracked (not gone)"
    );

    // The grant must be GRANTED: a status-0 slot is not "actively
    // transmitting" (ADR-173 Decision 3), even though cll_a's
    // repeat_message_ids still bears the MsgId at the moment the grant is
    // requested.
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_b),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect(
            "LockResource(LOCK_PHYSICAL_TX_QUEUE) must be granted when the only sibling \
             repeat-message slot on this physical resource has self-terminated (status 0), \
             even though it has not been explicitly stopped",
        );

    // The MsgId must still be VALID (not pruned) -- ADR-173 Decision 3: the
    // claim stays valid until an explicit STOP.
    let status = query_repeat_message(&mut client, cll_a, query_id, msg_id)
        .await
        .expect("QUERY on the status-0 MsgId must still succeed -- it is retained, not pruned");
    assert_eq!(status, 0);

    stop_repeat_message(&mut client, cll_a, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed on the still-valid MsgId");

    server.shutdown().await;
}

/// Round 6 regression guard (Codex review, ADR-165 PR #42 round 6): a
/// sibling CLL's genuinely LIVE (`Condition == 0`, still retransmitting)
/// repeat-message slot must still block a `LockResource(LOCK_PHYSICAL_TX_QUEUE)`
/// grant after the round-6 probe-and-prune fix -- the new
/// `prune_stale_repeat_message_ids` probe must keep (not discard) a MsgId
/// the device still reports `Ok` for, so this behavior stays unchanged from
/// before the fix (companion to
/// `lock_resource_is_rejected_while_a_sibling_has_a_live_repeat_message_slot`,
/// re-asserted here as a dedicated regression guard for the fix in this PR).
#[tokio::test]
#[serial]
async fn lock_resource_is_still_rejected_while_a_sibling_has_a_genuinely_live_repeat_message_slot()
{
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "sanity: cll_a and cll_b must actually share one physical channel for this test to \
         exercise the cross-CLL lock check at all"
    );
    set_can_addressing(&mut client, cll_a, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // Condition == 0 keeps retransmitting indefinitely (ADR-165 Decision 6)
    // -- genuinely live for the whole test, so the device's QUERY probe must
    // report it `Ok`, and the grant must still be rejected.
    let msg_id = start_repeat_message(
        &mut client,
        cll_a,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on cll_a");

    let status = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_b),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect_err(
            "LockResource(LOCK_PHYSICAL_TX_QUEUE) should still be rejected while a sibling CLL \
             has a genuinely live repeat-message slot actively transmitting on the shared \
             physical resource",
        );
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrFctFailed as i32
    );

    stop_repeat_message(&mut client, cll_a, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 6 regression test (Codex review, ADR-165 PR #42 round 6): the
/// self-skip removal -- a requester's own live repeat-message slot must now
/// block its own `LockResource(LOCK_PHYSICAL_TX_QUEUE)` grant, exactly like
/// a sibling's would (Fix J's `same_physical_resource` reading of ISO
/// 22900-2 Section 9.4.13.2 b): the active-transmissions clause carries no
/// "other" qualifier).
#[tokio::test]
#[serial]
async fn lock_resource_is_rejected_by_the_requesters_own_live_repeat_message_slot() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    let status = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_handle),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect_err(
            "LockResource(LOCK_PHYSICAL_TX_QUEUE) must be rejected by the requester's own live \
             repeat-message slot -- the active-transmissions clause has no self-exemption",
        );
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrFctFailed as i32
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 9 regression test (Codex review, ADR-165 PR #42 round 9, corrected
/// in round 14 to inject via `RxStatus` rather than `TxFlags` -- see
/// `RepeatSlot::response_format_rx_bits`'s doc comment,
/// `j2534-0404-mock/src/lib.rs`), corrected again for ADR-173: a
/// `Condition == 1` slot's mask/pattern match must also require the incoming
/// frame's actual `RxStatus` (masked to `CAN_29BIT_ID_STATUS`/
/// `ISO15765_ADDR_TYPE_STATUS`) to agree with the slot's own
/// `response_format_rx_bits`. `tx_header::can_header_bytes` always encodes a
/// numeric CAN ID as the same 4-byte big-endian `Data` prefix regardless of
/// whether it is meant as an 11-bit or 29-bit identifier, so a
/// byte-level-only match would incorrectly treat this frame as a MATCH
/// despite its actual wire format (11-bit CAN ID, here) not matching what
/// this CLL's response addressing resolved to (29-bit, via
/// `CP_CanRespUUDTFormat`'s bit 1).
///
/// **ADR-173 correction:** a `Condition == 1` slot terminates on the first
/// NON-matching frame (Decision 1) -- this frame's Data bytes coincide but
/// its format does not, so it is a non-match and now MUST terminate the
/// slot promptly (the reverse of the pre-ADR-173 "must not complete"
/// expectation, which rested on the old "terminate on match" model).
#[tokio::test]
#[serial]
async fn condition_one_slot_terminates_immediately_on_a_frame_with_the_wrong_response_format() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x7E8),
                // Bit 1 (CAN ID Type, SAE J2534-1 Table B.13): 29-bit
                // response CAN Id, so `response_header_bytes` resolves
                // `response_tx_flags` to `TX_EXTENDED_ID`.
                unum32_param(CP_CAN_RESP_UUDT_FORMAT, 0x02),
            ],
        )],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // TimeInterval is long enough that the slot cannot self-terminate via
    // silence within this test's own wait below -- if it terminates, it can
    // only be because of the (correctly recognized) non-match under test.
    // Empty mask_data/pattern_data: the full mask/pattern is exactly
    // `response_header_bytes`'s own 4-byte resolved response CAN Id header.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 1, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    // Same Data bytes a real 29-bit-Id response would carry (0x7E8's 4-byte
    // big-endian encoding), but WITHOUT CAN_29BIT_ID_STATUS -- as if this
    // were actually an 11-bit-Id frame that merely happens to encode to the
    // same numeric value. This is a NON-match (format bits disagree).
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::CAN,
        0,
    );

    assert!(
        wait_for_repeat_status(&server, msg_id, 0, 2000).await,
        "a frame with the wrong response-format RxStatus (missing CAN_29BIT_ID_STATUS) is a \
         non-match despite matching Data bytes, and must terminate a Condition == 1 slot \
         immediately (ADR-173 Decision 1)"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 9 regression test, matching counterpart, corrected for ADR-173: the
/// identical setup as above, but the injected frame's `RxStatus` correctly
/// carries `CAN_29BIT_ID_STATUS` (round 14: sourced from the RX-status side,
/// which is what actually flows through the fix -- same numeric value as the
/// wrapper crate's `TX_EXTENDED_ID`, but semantically distinct) -- a genuine
/// MATCH, which under ADR-173's corrected semantics must NOT terminate a
/// `Condition == 1` slot (only a non-match or silence does).
#[tokio::test]
#[serial]
async fn condition_one_slot_does_not_terminate_on_a_frame_with_the_correct_response_format() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_FORMAT, 0x02),
            ],
        )],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // A long TimeInterval again, so the assertion below can only be
    // attributed to the match, not a coincidental silence timeout.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 1, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::CAN,
        j2534_0404::CAN_29BIT_ID_STATUS,
    );

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "a frame with the correct response-format RxStatus (CAN_29BIT_ID_STATUS) and matching \
         Data bytes is a genuine MATCH, and a match must not terminate a Condition == 1 slot \
         (ADR-173 Decision 1)"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 14 regression test (Codex review, ADR-165 PR #42 round 14),
/// redesigned for ADR-173: a loopback-tagged echo of the CLL's own
/// `PassThruWriteMsgs` transmission (`RxStatus` bit 0, SAE J2534-1 Figure
/// 43) is not a message received from another node, so it must never
/// satisfy a `Condition == 1` repeat slot's stop condition. `j2534_0404::
/// LOOPBACK` is the same `CP_Loopback` SET_CONFIG ComParam
/// `send_type_1_loopback_echo_does_not_starve_a_same_interval_sibling`
/// (`tester_present_send_type.rs`) uses to arm loopback on the shared
/// physical channel at connect time. The physical request and response CAN
/// IDs are deliberately set EQUAL (both `0x7E8`): `ioctl_start_repeat_message`
/// always prepends the *response*-side CAN-ID header to the mask/pattern
/// (`tx_header::response_header_bytes`, resolved from `CP_CanRespUUDTId`),
/// while a loopback echo carries the *physical-request*-side CAN ID the
/// message was actually written with -- distinct request/response IDs would
/// make the byte-level CAN-ID header alone reject the echo, leaving this
/// test unable to tell whether the round-14 `RxStatus` eligibility check is
/// doing anything at all.
///
/// **ADR-173 redesign:** under the corrected `Condition == 1` semantics, an
/// empty (vacuously-matching-anything) mask/pattern is no longer a useful
/// discriminator here -- even a WRONGLY-included echo would be evaluated as
/// a MATCH against a vacuous pattern, and a match no longer terminates
/// `Condition == 1` either way, so that design stopped being able to tell a
/// correct exclusion from a bug. Uses a mask/pattern the echo's own payload
/// (`[0x01, 0x02, 0x03]`) does NOT satisfy instead: a wrongly-included echo
/// would be evaluated as a NON-match and terminate the slot immediately
/// (ADR-173 Decision 1); a correctly-excluded echo leaves the slot live.
#[tokio::test]
#[serial]
async fn condition_one_slot_does_not_terminate_on_its_own_loopback_echo() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000), (j2534_0404::LOOPBACK, 1)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E8, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // mask/pattern (beyond the shared 4-byte response CAN-ID header):
    // requires the first payload byte to be 0x99 -- the echo's actual first
    // payload byte is 0x01 (the CoptSendrecv's own cop_data below), so this
    // is a genuine non-match if the echo were (incorrectly) evaluated.
    // TimeInterval is long enough that the slot cannot self-terminate via
    // silence within this test's own wait below.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 1, vec![0x01, 0x02], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    // Triggers exactly one PassThruWriteMsgs; loopback is enabled, so the
    // mock echoes this write straight back into the RX queue with RxStatus's
    // RX_TX_MSG_TYPE bit set.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01, 0x02, 0x03],
            cop_ctrl_data: Some(vci_service_interface::ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Generous margin past the mock's own 5ms repeat-worker poll step, same
    // as the format tests above.
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "a loopback echo of the CLL's own transmission must never be evaluated as a received \
         frame at all (neither a match nor a non-match) -- if it were incorrectly evaluated, \
         its non-matching payload would have terminated this Condition == 1 slot immediately"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 20 regression test (Codex review, ADR-165 PR #42 round 20): the
/// repeat worker's own autonomous transmit (`j2534-0404-mock`'s
/// `spawn_repeat_worker`) must emit a loopback echo when `CONFIG_LOOPBACK`
/// is enabled, exactly like `PassThruWriteMsgs`'s own established echo path
/// -- otherwise a client observes loopback echoes for ordinary writes but
/// never for repeat traffic, purely because of which code path happened to
/// transmit an otherwise-identical frame. `Condition == 0` (unconditional,
/// free-running retransmission) is used so the worker's first transmit
/// fires immediately with no RX-matching involved at all -- this test is
/// only about the echo itself, not clause 14's stop-condition semantics
/// (already covered by `j2534-0404-mock`'s own unit tests and the other
/// tests in this file). Request and response CAN IDs are deliberately set
/// EQUAL (both `0x7E8`), same reasoning as
/// `condition_one_slot_does_not_complete_on_its_own_loopback_echo` above:
/// the repeat worker's TX addressing resolves from the *physical-request*
/// side (`CP_CanPhysReqId`), while a receive-only monitor's header
/// recognition resolves from the *response* side (`CP_CanRespUUDTId`) --
/// keeping them equal lets the same frame be recognized on both sides so
/// this test can assert a deterministic, header-stripped `data_bytes`
/// rather than merely "some ResultData arrived, containing who-knows-what".
#[tokio::test]
#[serial]
async fn repeat_worker_autonomous_transmit_emits_a_loopback_echo() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000), (j2534_0404::LOOPBACK, 1)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E8, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // A short TimeInterval so the worker's first (immediate) transmit is
    // promptly followed by further ones if the first is somehow missed by
    // the receive-only monitor armed just below -- Condition == 0 keeps
    // retransmitting indefinitely regardless of RX, so there is no race
    // against a stop-on-match slot completing before the monitor can see it.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(100, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    // A receive-only monitor with an empty mask/pattern matches any Data at
    // all -- the only traffic on this channel is the repeat worker's own
    // autonomous transmissions (and their loopback echoes), so a delivered
    // ResultData here can only be attributed to that echo. Without the
    // round-20 fix, `spawn_repeat_worker` never pushes anything into
    // `rx_queue` at all, and this would time out waiting for a ResultData
    // event that never arrives.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_eq!(
        result.data_bytes,
        vec![0x01, 0x02],
        "the repeat worker's own autonomous transmission must produce a loopback echo the \
         client can observe, mirroring PassThruWriteMsgs's own existing echo path"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 10 Finding 1 regression test (Codex review, ADR-165 PR #42 round
/// 10), corrected for ADR-173: a `Condition == 1` slot's mask/pattern match
/// must also require the incoming frame's actual `ProtocolID` to agree with
/// the slot's own `response_protocol_id`. A native-mixed CAN channel
/// (`CanChannelMode::NativeMixed`, ADR-160/162) can carry both raw-CAN and
/// ISO15765 frames on one physical channel, distinguished only by each
/// frame's own `ProtocolID` -- a raw-CAN frame whose `Data` bytes
/// coincidentally match an ISO15765 slot's mask/pattern is still a NON-match
/// once `ProtocolID` disagrees.
///
/// **ADR-173 correction:** a `Condition == 1` slot terminates on the first
/// NON-matching frame (Decision 1) -- this frame's Data bytes coincide but
/// its `ProtocolID` does not, so it is a non-match and now MUST terminate
/// the slot promptly (the reverse of the pre-ADR-173 "must not complete"
/// expectation, which rested on the old "terminate on match" model).
#[tokio::test]
#[serial]
async fn condition_one_iso15765_slot_terminates_immediately_on_a_raw_can_frame_with_matching_data()
{
    let server = start_j2534_2_server().await;
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
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
            ],
        )],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // TimeInterval is long enough that the slot cannot self-terminate via
    // silence within this test's own wait below -- if it terminates, it can
    // only be because of the (correctly recognized) non-match under test.
    // Empty mask_data/pattern_data: the full mask/pattern is exactly
    // `response_header_bytes`'s own 4-byte resolved response CAN Id header
    // (normal 11-bit addressing, no TxFlags).
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 1, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed");

    // Same Data bytes and RxStatus an ISO15765 response would carry (0x7E8's
    // 4-byte big-endian encoding, no addressing flags), but tagged with the
    // mock's raw-CAN ProtocolID instead of ISO15765's -- as if a
    // native-mixed CAN channel delivered this frame on the raw-CAN side
    // rather than the ISO15765 side. This is a NON-match (ProtocolID
    // disagrees).
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::CAN,
        0,
    );

    assert!(
        wait_for_repeat_status(&server, msg_id, 0, 2000).await,
        "a frame carrying the mock's raw-CAN ProtocolID is a non-match despite matching Data \
         bytes, and must terminate a Condition == 1 ISO15765 repeat slot immediately (ADR-173 \
         Decision 1)"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 10 Finding 2 regression test (Codex review, ADR-165 PR #42 round
/// 10): SAE J2534-2 clause 14 only defines `Condition == 0`
/// (`REPEAT_MESSAGE_UNTIL_MATCH`) and `Condition == 1`
/// (`REPEAT_MESSAGE_WHILE_MATCH`, ADR-173 Decision 1) -- a `Condition`
/// outside `{0, 1}` is rejected by the mock's own `ERR_INVALID_IOCTL_VALUE`,
/// forwarded through `map_native_error_for_link` (`error::pdu_error_for`) to
/// `PDU_ERR_VALUE_NOT_SUPPORTED` under an outer `Code::Internal` (only
/// `PDU_ERR_INVALID_HANDLE` gets a different outer code,
/// `map_native_error_as`), through the FULL gRPC `IoCtl` stack.
#[tokio::test]
#[serial]
async fn start_repeat_message_rejects_an_out_of_range_condition_through_the_full_grpc_stack() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    // `condition == 2` goes through `response_header_bytes`'s resolution
    // just like both defined condition values do (ADR-173 Decision 4 made
    // this unconditional), so addressing must be set up here too, otherwise
    // the failure observed would be the unrelated "missing UniqueRespIdTable
    // entry" `InvalidArgument`, not this fix's `ERR_INVALID_IOCTL_VALUE`.
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 2, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE with Condition == 2 (outside the {0, 1} SAE J2534-2 \
         clause 14 defines) should be rejected",
    );
    assert_eq!(status.code(), Code::Internal);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrValueNotSupported as i32
    );

    server.shutdown().await;
}

/// Round 11 regression test (Codex review, ADR-165 PR #42 round 11),
/// corrected for ADR-173: a leaked repeat-message slot (`SharedChannel::
/// leaked_repeat_message_ids`) that the device has already forgotten must
/// not permanently block a sibling's `LockResource(LOCK_PHYSICAL_TX_QUEUE)`
/// grant -- the same staleness probe (`prune_stale_repeat_message_ids`)
/// already applied to a sibling CLL's own `repeat_message_ids` (round 6)
/// must also cover `leaked_repeat_message_ids`.
///
/// **ADR-173 note:** the pre-ADR-173 version of this test manufactured the
/// "device already forgot this MsgId" precondition via self-completion (a
/// short-interval `Condition == 1` slot with no matching frame). Under the
/// corrected clause 14 model, self-termination no longer makes the mock
/// forget the slot (it is retained at status 0 until an explicit STOP,
/// Decision 3), so that technique no longer reproduces the scenario -- this
/// test now uses `MockBackdoor::stop_repeat_message_directly` (a raw STOP
/// issued straight against the mock, bypassing this service's own tracking)
/// instead, exactly like the analogous `a_device_forgotten_condition_one_
/// slot_is_pruned_on_the_first_query_that_notices` fix.
#[tokio::test]
#[serial]
async fn lock_resource_is_granted_once_a_leaked_repeat_message_slot_is_pruned() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "sanity: cll_a and cll_b must actually share one physical channel for this test to \
         exercise the leaked-slot lock check at all"
    );
    set_can_addressing(&mut client, cll_a, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let msg_id = start_repeat_message(
        &mut client,
        cll_a,
        start_id,
        repeat_message_setup(1000, 1, vec![0x01, 0x02], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on cll_a");

    // Manufacture "the device already forgot this MsgId" directly -- see
    // this test's own doc comment.
    assert!(
        server
            .backdoor
            .stop_repeat_message_directly(MOCK_CHANNEL_ID, msg_id),
        "sanity: the direct STOP against the mock should succeed while the slot is still live"
    );
    assert!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id)
            .is_none(),
        "sanity: the device should now genuinely have forgotten this MsgId"
    );

    // cll_a's best-effort STOP for this already-gone MsgId fails
    // (ERR_INVALID_MSG_ID) during teardown; cll_b still holds the physical
    // channel open, so the failed id lands in the SharedChannel's
    // leaked_repeat_message_ids rather than being silently dropped.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link(cll_a) should succeed");

    // Before the fix, the leaked slot was never scanned at all here, so this
    // grant already passed vacuously; after the fix, it is scanned and must
    // still be granted once the probe discovers the device has already
    // forgotten this MsgId.
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_b),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect(
            "LockResource(LOCK_PHYSICAL_TX_QUEUE) must be granted once the only leaked \
             repeat-message slot on this physical resource's shared channel has \
             self-completed device-side",
        );

    server.shutdown().await;
}

/// Round 17 regression test (Codex review, ADR-165 PR #42 round 17): before
/// this fix, the round-6 general TX message size-range check only validated
/// `full_message` (`RepeatMsgData[0]`) -- the composed mask/pattern
/// (`RepeatMsgData[1]`/`[2]`, `mask_data`/`pattern_data`, built by prepending
/// the response header to the client's own `setup.mask_data`/
/// `setup.pattern_data`) was never checked against the protocol's real TX
/// message size range at all. A 9-byte client `mask_data`/`pattern_data` on a
/// classic CAN link becomes a 13-byte composed template (4-byte CAN-ID header
/// + 9 bytes) once `set_can_addressing` resolves the response header under
/// `condition == 1` -- one over classic CAN's `4..=12` range -- and must now
/// be cleanly rejected with `InvalidArgument`, not silently forwarded to
/// `PassThruMessage::new`/the native IOCTL.
#[tokio::test]
#[serial]
async fn start_repeat_message_rejects_an_oversized_mask_and_pattern_template_under_condition_one() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // 9 bytes of client mask/pattern data -- once the 4-byte response CAN-ID
    // header is prepended (condition == 1 resolves the response header), the
    // composed 13-byte template exceeds classic CAN's 12-byte cap.
    let oversized: Vec<u8> = (1..=9).collect();
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 1, vec![0x01, 0x02], oversized.clone(), oversized),
    )
    .await
    .expect_err(
        "an oversized mask/pattern template (once the response header is prepended) must be \
         cleanly rejected, not silently forwarded to the native adapter",
    );
    assert_eq!(status.code(), Code::InvalidArgument);

    // The service process itself must still be alive and the connection
    // intact -- an ordinary, valid START_REPEAT_MESSAGE on the same
    // connection must still succeed.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect(
        "the service must still be alive and the connection intact after rejecting the \
         oversized mask/pattern template",
    );
    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// ADR-173 Decision 4 (coverage item 13, reverses the round-17 condition-0
/// carve-out, Codex review ADR-165 PR #42 round 17): companion to
/// `start_repeat_message_rejects_an_oversized_mask_and_pattern_template_under_condition_one`
/// above -- the mask/pattern size-range check is now unconditional, so the
/// SAME 9-byte client mask/pattern payload that test rejects under
/// `condition == 1` must now be rejected under `condition == 0` too, since
/// `condition == 0` now resolves the response header exactly like
/// `condition == 1` does (ADR-173 Decision 4) and so composes the
/// identical 13-byte (4-byte header + 9 bytes) over-cap template.
#[tokio::test]
#[serial]
async fn start_with_condition_zero_now_rejects_an_oversized_mask_and_pattern_template() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // 9 bytes of client mask/pattern data -- once the 4-byte response CAN-ID
    // header is prepended (condition == 0 now resolves the response header
    // too, ADR-173 Decision 4), the composed 13-byte template exceeds
    // classic CAN's 12-byte cap.
    let oversized: Vec<u8> = (1..=9).collect();
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], oversized.clone(), oversized),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE must now reject an oversized mask/pattern template \
         under condition == 0 too -- ADR-173 Decision 4 reverses the prior condition-0 \
         carve-out",
    );
    assert_eq!(status.code(), Code::InvalidArgument);

    // The service process itself must still be alive and the connection
    // intact -- an ordinary, valid START_REPEAT_MESSAGE on the same
    // connection must still succeed.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![], vec![]),
    )
    .await
    .expect(
        "the service must still be alive and the connection intact after rejecting the \
         oversized mask/pattern template",
    );
    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 17 regression test (Codex review, ADR-165 PR #42 round 17): the
/// boundary companion to
/// `start_repeat_message_rejects_an_oversized_mask_and_pattern_template_under_condition_one`
/// above -- an 8-byte client mask/pattern payload (producing exactly the
/// 12-byte upper bound of classic CAN's `4..=12` TX message size range once
/// the 4-byte response header is prepended) must still be accepted, pinning
/// the new check's inclusive upper bound.
#[tokio::test]
#[serial]
async fn start_repeat_message_accepts_a_mask_and_pattern_template_at_the_exact_size_boundary() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // 8 bytes of client mask/pattern data -- the 4-byte response CAN-ID
    // header prepended produces exactly the 12-byte upper bound of classic
    // CAN's 4..=12 TX message size range.
    let boundary: Vec<u8> = (1..=8).collect();
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 1, vec![0x01, 0x02], boundary.clone(), boundary),
    )
    .await
    .expect(
        "a mask/pattern template at exactly the 12-byte upper bound must be accepted, not \
         rejected as oversized",
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// Round 18 regression test (Codex review, ADR-165 PR #42 round 18): round
/// 17's mask/pattern size-range check (added just above) used
/// `fd_can_tx_message_size_range` for an FD_CAN_PS link's `Some(j2534_0404::
/// CAN)` arm -- the SAME function `full_message`'s own (correct) check uses.
/// That function's own doc comment (`rpc_link.rs`) is explicit that its
/// upper bound comes from `CP_CANFDTxMaxDataLength`, the TESTER's own
/// declared max TX length -- but `mask_data`/`pattern_data` are never
/// transmitted, so an ECU can validly send a longer response than the
/// tester's own configured TX_DL. With `CP_CANFDBaudrate` staged nonzero
/// (selecting FD mode) but `CP_CANFDTxMaxDataLength` left at its `8`-byte
/// floor, round 17's check wrongly capped the composed mask/pattern template
/// at `4..=12` bytes -- even though round 15's fix already sets
/// `TX_FD_CAN_FORMAT` on this exact template specifically to make it valid
/// up to the full CAN FD frame limit (`4..=68`). A 20-byte client
/// `mask_data`/`pattern_data` composes (with the 4-byte response header
/// prepended) to a 24-byte template -- over round 17's wrong `12`-byte cap,
/// but well within the correct `68`-byte CAN FD structural cap -- and must
/// now be accepted.
#[tokio::test]
#[serial]
async fn start_repeat_message_accepts_an_oversized_by_tx_dl_mask_and_pattern_template_on_an_fd_link()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 8),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: staged FD ComParams should have substituted the native connect id to \
         FD_CAN_PS (ADR-158) -- otherwise this test would not actually be exercising an \
         FD-connected link"
    );
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // 20 bytes of client mask/pattern data -- the 4-byte response CAN-ID
    // header prepended produces a 24-byte composed template: over round 17's
    // wrong TX_DL-bounded 12-byte cap, but within the correct 68-byte CAN FD
    // structural cap (4 + 64).
    let oversized_by_tx_dl: Vec<u8> = (1..=20).collect();
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(
            1000,
            1,
            vec![0x01, 0x02],
            oversized_by_tx_dl.clone(),
            oversized_by_tx_dl,
        ),
    )
    .await
    .expect(
        "a 24-byte mask/pattern template on an FD-connected link must be accepted -- it is \
         never transmitted, so it must not be capped by the tester's own staged \
         CP_CANFDTxMaxDataLength, only by the CAN FD frame's structural maximum",
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// ADR-055/ADR-169, reason updated by ADR-186: `ioctl_start_repeat_message`
/// closes the gap ADR-169's Consequences section documented as an accepted
/// residual -- a functionally addressed ISO15765 repeat message set up via
/// `START_REPEAT_MESSAGE` had no Single Frame check at all. An 8-byte
/// `repeat_msg_data` still exceeds the fixed Classic Normal-addressing
/// 7-byte Single Frame limit this gap fix (ADR-169) targets -- but ADR-186's
/// new periodic-message cap check (SAE J2534-1 7.2.7) numerically coincides
/// with that same 7-byte boundary for classic ISO15765 and runs EARLIER in
/// `ioctl_start_repeat_message`'s own ordering, so it is now the check that
/// actually fires first at this boundary, not the functional-addressing-
/// specific Single Frame check this test originally pinned. The rejection
/// still happens at the identical byte count either way.
#[tokio::test]
#[serial]
async fn start_repeat_message_rejects_an_oversized_functional_iso15765_single_frame() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    // 8 bytes exceeds both the 7-byte Normal-addressing Single Frame limit
    // (ADR-169) and, at the identical boundary, ADR-186's periodic-message
    // cap (4..=11 bytes for classic ISO15765) -- the latter runs first.
    let too_long_payload: Vec<u8> = (1..=8).collect();
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, too_long_payload, vec![], vec![]),
    )
    .await
    .expect_err(
        "an 8-byte repeat_msg_data on a functionally addressed ISO15765 link must be rejected \
         -- SAE J2534-1 7.2.7's periodic-message cap (ADR-186) numerically coincides with, and \
         now runs ahead of, ISO 15765-2's functionally-addressed single-frame requirement \
         (ADR-055/ADR-169)",
    );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("7.2.7"),
        "the rejection should cite SAE J2534-1 7.2.7's periodic-message cap: {}",
        status.message()
    );

    server.shutdown().await;
}

/// ADR-186 regression test (supersedes the former ADR-055/ADR-169/ADR-173
/// FD-widened-Single-Frame-limit test this replaces, which asserted the OLD
/// behavior no longer reachable): a native FD-substituted ISO15765 link's
/// functional-addressing Single Frame limit used to widen with the link's
/// staged `CP_CANFDTxMaxDataLength` (up to 62 bytes at TX_DL=64), tracking
/// `rpc_primitive.rs`'s `CoptSendrecv` path. ADR-186's periodic-message cap
/// (SAE J2534-1 7.2.7 / SAE J2534-2 clause 22.2.2(h)) is NOT relaxed the
/// same way -- it stays fixed at 11 bytes DataSize (7-byte payload,
/// non-extended addressing) regardless of FD-ness or `CP_CANFDTxMaxDataLength`,
/// which is now materially TIGHTER than the old widened Single Frame limit
/// and runs earlier in `ioctl_start_repeat_message`'s own ordering. The old
/// widened-limit boundary (a 62-byte payload clearing the SF check to reach
/// the response-header-required rejection, ADR-173 Decision 4) is therefore
/// no longer reachable at all: any payload over the new 7-byte periodic-cap
/// boundary is now rejected there first. This test instead pins the two
/// checks' new relative ordering directly: a payload AT the periodic cap
/// still clears it and reaches the (functional-addressing-unreachable)
/// header-required rejection, while a payload one byte OVER the periodic cap
/// is rejected by the periodic-cap check itself, before the header
/// requirement or the (now practically unreachable) FD-widened Single Frame
/// check ever run.
#[tokio::test]
#[serial]
async fn start_repeat_message_functional_addressing_on_fd_link_periodic_cap_precedes_widened_single_frame_limit()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_PS,
        "sanity: cll must actually be FD-substituted for the (now superseded) widened limit's \
         old code path to be the one under test"
    );

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    // 7 bytes -- exactly ADR-186's periodic-cap boundary (DataSize 11, 4-byte
    // header + 7 payload bytes, non-extended addressing). Clears the
    // periodic-cap check AND the FD-widened Single Frame check (62-byte
    // limit at TX_DL=64), so this must fail at response-header resolution
    // instead (ADR-173 Decision 4) -- NOT a periodic-cap rejection.
    let at_periodic_cap_payload = vec![0u8; 7];
    let at_cap_status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, at_periodic_cap_payload, vec![], vec![]),
    )
    .await
    .expect_err(
        "a within-periodic-cap functional payload must still be rejected -- functional \
         addressing can never provide a resolvable response header (ADR-054), which ADR-173 \
         Decision 4 made required for every Condition value",
    );
    assert_eq!(at_cap_status.code(), Code::InvalidArgument);
    assert!(
        !at_cap_status.message().contains("7.2.7"),
        "the within-periodic-cap case must NOT be rejected for the periodic-cap reason -- proof \
         that check did not fire here: {}",
        at_cap_status.message()
    );

    // 8 bytes -- one byte over ADR-186's periodic cap. Still well within the
    // old FD-widened Single Frame limit (62 bytes) and the header-required
    // rejection would reject it too, but the periodic-cap check now runs
    // FIRST and is what actually fires.
    let over_periodic_cap_payload = vec![0u8; 8];
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, over_periodic_cap_payload, vec![], vec![]),
    )
    .await
    .expect_err("an 8-byte repeat_msg_data must exceed ADR-186's periodic-message cap");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("7.2.7"),
        "the rejection should cite SAE J2534-1 7.2.7's periodic-message cap, proving it (not \
         the header requirement or the old widened Single Frame limit) is what fired: {}",
        status.message()
    );

    server.shutdown().await;
}

/// ADR-055/ADR-169 (continued), reason updated by ADR-186: this check is
/// functional-addressing-scoped, not a blanket size cap -- but ADR-186's own
/// periodic-message cap (SAE J2534-1 7.2.7) IS a blanket size cap that
/// applies regardless of addressing mode, and now runs first. The SAME
/// 8-byte payload `start_repeat_message_rejects_an_oversized_functional_
/// iso15765_single_frame` rejects above is therefore now ALSO rejected on a
/// PHYSICALLY addressed Classic ISO15765 link -- proving the periodic cap,
/// unlike the old functional-addressing-only Single Frame check this test
/// used to pin as never firing for physical addressing, applies
/// unconditionally. A 7-byte payload (at the periodic cap) is accepted,
/// confirming the boundary itself.
#[tokio::test]
#[serial]
async fn start_repeat_message_rejects_an_oversized_classic_iso15765_payload_even_when_physically_addressed()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // 8 bytes -- one over ADR-186's periodic cap (4..=11 bytes DataSize,
    // non-extended addressing) -- must now be rejected even though physical
    // (not functional) addressing means the SAE 15765-2 functional-only
    // Single Frame check (ADR-055) does not apply here.
    let too_long_payload: Vec<u8> = (1..=8).collect();
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, too_long_payload, vec![], vec![]),
    )
    .await
    .expect_err(
        "an 8-byte repeat_msg_data on a PHYSICALLY addressed classic ISO15765 link must now be \
         rejected -- SAE J2534-1 7.2.7's periodic-message cap (ADR-186) applies regardless of \
         addressing mode, unlike the functional-only Single Frame check (ADR-055)",
    );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("7.2.7"),
        "the rejection should cite SAE J2534-1 7.2.7's periodic-message cap: {}",
        status.message()
    );

    // 7 bytes -- exactly at the periodic cap -- must still be accepted.
    let at_cap_payload: Vec<u8> = (1..=7).collect();
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, at_cap_payload, vec![], vec![]),
    )
    .await
    .expect("a 7-byte repeat_msg_data (DataSize 11) must be accepted at the periodic cap");
    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// ADR-186 boundary test: SAE J2534-2 clause 22.2.2(h)'s 11-byte
/// FD-ISO15765 periodic-message DataSize cap, driven on a PHYSICALLY
/// addressed `FD_ISO15765_PS` link (so a resolvable response header is
/// available and the boundary itself, not the unrelated ADR-173
/// header-required rejection, is what this test observes). An 8-byte
/// payload (DataSize 12) exceeds the 11-byte cap and is rejected; a 7-byte
/// payload (DataSize 11) is accepted.
#[tokio::test]
#[serial]
async fn start_repeat_message_on_fd_iso15765_enforces_the_eleven_byte_periodic_cap() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_PS,
        "sanity: staged FD ComParams should have substituted the native connect id to \
         FD_ISO15765_PS (ADR-159) -- otherwise this test would not actually be exercising an \
         FD-connected link"
    );
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // 8 bytes -- DataSize 12, one over clause 22.2.2(h)'s 11-byte cap. Not
    // relaxed by TX_FD_CAN_FORMAT, unlike the ordinary-TX 21.4.4 allowance.
    let too_long_payload: Vec<u8> = (1..=8).collect();
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, too_long_payload, vec![], vec![]),
    )
    .await
    .expect_err(
        "an 8-byte repeat_msg_data (DataSize 12) on an FD_ISO15765_PS link must be rejected -- \
         SAE J2534-2 clause 22.2.2(h) caps a periodic FD-ISO15765 message at 11 bytes, not \
         relaxed by TX_FD_CAN_FORMAT",
    );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("7.2.7") || status.message().contains("14.2.1"),
        "the rejection should cite the periodic-message cap: {}",
        status.message()
    );

    // 7 bytes -- exactly at the 11-byte DataSize cap -- must be accepted.
    let at_cap_payload: Vec<u8> = (1..=7).collect();
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, at_cap_payload, vec![], vec![]),
    )
    .await
    .expect("a 7-byte repeat_msg_data (DataSize 11) must be accepted at the periodic cap");
    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

// ── ADR-178 byte-packing revert: bytearray_data-specific failure modes ────
//
// The two tests below cover failure shapes the old typed-proto
// `IORepeatMessageSetup` message made structurally impossible to construct
// at all -- a hand-packed byte payload can be truncated or lie about its
// own length in ways a typed proto message never could. Mirrors
// `pdu_ioctl.rs`'s `device_config_malformed_bytearray_payload_is_rejected`/
// `device_config_wrong_data_item_variant_is_rejected` precedent from
// Device Configuration's own ADR-178 revert.

/// A `bytearray_data` payload truncated (or lying about a claimed length)
/// at each of the format's independent cut points -- the fixed 8-byte
/// header, each of the three length-prefixed byte spans, and the trailing
/// `tx_flag_bits_count`/array -- is rejected `INVALID_ARGUMENT` rather than
/// misread, silently truncated, or read out of bounds. So is a
/// structurally-valid payload with extra trailing bytes appended past the
/// end (edge-case-hunter, PR #3-series round 1) -- the removed typed
/// `IORepeatMessageSetup` proto message made an "extra field data" shape
/// structurally impossible, and this hand-packed format must reject it
/// explicitly rather than silently accepting it.
#[tokio::test]
#[serial]
async fn start_repeat_message_malformed_bytearray_payload_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    // A fully well-formed payload (repeat_msg_data/mask_data/pattern_data
    // all 1 byte, no tx_flag_bits) -- byte offsets: [0..4)=time_interval,
    // [4..8)=condition, [8..12)=repeat_msg_data len, [12..13)=repeat_msg_data,
    // [13..17)=mask_data len, [17..18)=mask_data, [18..22)=pattern_data len,
    // [22..23)=pattern_data, [23..27)=tx_flag_bits_count.
    let well_formed = pack_repeat_message_setup(1000, 0, &[0x01], &[0xFF], &[0x01], &[]);
    assert_eq!(
        well_formed.len(),
        27,
        "sanity: the offsets in this test's comments assume this exact length"
    );

    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "fewer than 8 bytes -- not even time_interval+condition",
            vec![0x01, 0x02, 0x03],
        ),
        (
            "exactly the 8-byte header, truncated before repeat_msg_data's own length prefix",
            well_formed[..8].to_vec(),
        ),
        (
            "repeat_msg_data's claimed length exceeds what actually follows",
            {
                let mut b = 1000u32.to_le_bytes().to_vec();
                b.extend_from_slice(&0u32.to_le_bytes()); // condition
                b.extend_from_slice(&u32::MAX.to_le_bytes()); // repeat_msg_data len
                b
            },
        ),
        ("truncated within mask_data's claimed span", {
            let mut b = 1000u32.to_le_bytes().to_vec();
            b.extend_from_slice(&0u32.to_le_bytes()); // condition
            b.extend_from_slice(&0u32.to_le_bytes()); // repeat_msg_data len = 0
            b.extend_from_slice(&4u32.to_le_bytes()); // mask_data len = 4
            b.extend_from_slice(&[0x00, 0x00]); // only 2 of the claimed 4 bytes
            b
        }),
        (
            "valid through pattern_data, truncated before tx_flag_bits_count",
            well_formed[..23].to_vec(),
        ),
        (
            "tx_flag_bits_count claims more bits than actually follow",
            {
                // Base on the payload truncated right before the (0-valued)
                // tx_flag_bits_count field baked into `well_formed` -- appending
                // after the real count instead would just add trailing bytes
                // the parser never reads, since a 0 count consumes nothing more.
                let mut b = well_formed[..23].to_vec();
                b.extend_from_slice(&u32::MAX.to_le_bytes());
                b
            },
        ),
        ("truncated within the claimed tx_flag_bits array", {
            let mut b = well_formed[..23].to_vec();
            b.extend_from_slice(&2u32.to_le_bytes()); // claims 2 bits
            b.extend_from_slice(&31u32.to_le_bytes()); // only 1 of the claimed 2 follow
            b
        }),
        (
            "trailing garbage bytes after an otherwise well-formed payload",
            {
                let mut b = well_formed.clone();
                b.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00]);
                b
            },
        ),
        // ADR-214: the optional v2 trailing section (response_tx_flag_bits)
        // has the identical count+array shape as tx_flag_bits, and must be
        // malformed-checked the same way.
        (
            "1-3 stray bytes after tx_flag_bits -- not enough for a full \
             response_tx_flag_bits_count u32",
            {
                let mut b = well_formed.clone();
                b.extend_from_slice(&[0x01, 0x02, 0x03]);
                b
            },
        ),
        (
            "response_tx_flag_bits_count claims more entries than actually follow",
            {
                let mut b = well_formed.clone();
                b.extend_from_slice(&u32::MAX.to_le_bytes());
                b
            },
        ),
        (
            "trailing bytes after an otherwise well-formed v2 response_tx_flag_bits section",
            {
                let mut b = pack_repeat_message_setup_with_response_tx_flag_bits(
                    1000,
                    0,
                    &[0x01],
                    &[0xFF],
                    &[0x01],
                    &[],
                    Some(&[31]),
                );
                b.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
                b
            },
        ),
    ];

    for (description, malformed) in cases {
        let input = DataItem {
            data: Some(data_item::Data::BytearrayData(IoBytearray {
                data: malformed,
            })),
        };
        let result = start_repeat_message(&mut client, cll_handle, start_id, input).await;
        let status = match result {
            Err(status) => status,
            Ok(msg_id) => panic!(
                "PDU_IOCTL_START_REPEAT_MESSAGE should reject a malformed bytearray_data \
                 payload ({description}), got success with MsgId {msg_id}"
            ),
        };
        assert_eq!(status.code(), Code::InvalidArgument, "{description}");
    }

    server.shutdown().await;
}

/// ADR-214 Decision item 2: an explicit `Some(vec![])` v2 section (exactly
/// 4 trailing zero bytes, `response_tx_flag_bits_count == 0`) is a
/// meaningful, distinct value from `None` -- "the response template
/// carries neither addressing bit" -- and must be ACCEPTED, not rejected as
/// trailing garbage the way an arbitrary/malformed trailer would be.
#[tokio::test]
#[serial]
async fn start_repeat_message_accepts_an_explicit_empty_response_tx_flag_bits_section() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    let input = DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup_with_response_tx_flag_bits(
                1000,
                0,
                &[0x01],
                &[0xFF],
                &[0x01],
                &[],
                Some(&[]),
            ),
        })),
    };
    let msg_id = start_repeat_message(&mut client, cll_handle, start_id, input)
        .await
        .expect(
            "an explicit empty response_tx_flag_bits v2 section (4 trailing zero bytes) must \
             be accepted, not rejected as trailing garbage",
        );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// The wrong `DataItem` oneof variant (`unum32_value`, as QUERY/STOP expect,
/// rather than `bytearray_data`) is rejected `INVALID_ARGUMENT` with a
/// message that names `bytearray_data` -- distinct from
/// `wrong_shaped_input_data_is_rejected_for_all_three_commands` above (which
/// only pins the status code, not the message content) and from the
/// malformed-payload cases above (a wrong oneof variant is a different
/// failure than a malformed-but-correctly-shaped `bytearray_data` payload).
#[tokio::test]
#[serial]
async fn start_repeat_message_wrong_data_item_variant_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_addressing(&mut client, cll_handle, 0x7E0, 0x7E8).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let status = start_repeat_message(&mut client, cll_handle, start_id, unum32_input(1))
        .await
        .expect_err(
            "PDU_IOCTL_START_REPEAT_MESSAGE should reject a non-bytearray_data DataItem variant",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("bytearray_data"),
        "{}",
        status.message()
    );

    server.shutdown().await;
}

/// ADR-199: `PDU_IOCTL_START_REPEAT_MESSAGE` on a RawMode=ON CAN CLL now
/// succeeds, composing `repeat_msg_data`/`mask_data`/`pattern_data` from the
/// client's own header-inclusive bytes with NO service-derived CAN-ID prefix
/// added on top (ISO 22900-2:2022 Table 80's RawMode expected-response
/// template shape, extended from ADR-196 Decision item 3's ordinary
/// expected-response matching to Repeat Messaging's mask/pattern). Proven two
/// ways: the returned `MsgId` is nonzero, and a genuinely matching response
/// frame is recognized as a MATCH by the mock's own byte-level mask/pattern
/// comparison against the FULL received wire frame
/// (`j2534-0404-mock::repeat_mask_pattern_matches`) -- if the service had
/// still prepended its own service-derived CAN-ID header on top of the
/// client's own already-complete raw mask/pattern bytes (the pre-ADR-199
/// double-header bug this replaces the old unconditional rejection for), the
/// composed template's byte offsets would no longer line up with a
/// genuinely-received frame and this match would spuriously fail.
#[tokio::test]
#[serial]
async fn start_repeat_message_succeeds_on_a_raw_mode_can_cll_with_header_inclusive_template() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    // Client-supplied raw repeat_msg_data: 4-byte CAN-ID prefix (0x7E0) + 2
    // payload bytes -- the client's own complete frame, header included.
    let mut repeat_msg_data = 0x7E0_u32.to_be_bytes().to_vec();
    repeat_msg_data.extend_from_slice(&[0x01, 0x02]);

    // Raw mask/pattern template: exact-match mask over the expected
    // response's own CAN-ID prefix (0x7E8), unprefixed -- the client's own
    // bytes are the complete template already.
    let mask_data = vec![0xFF, 0xFF, 0xFF, 0xFF];
    let pattern_data = 0x7E8_u32.to_be_bytes().to_vec();

    // Condition == 1 (WHILE_MATCH, ADR-173 Decision 1): terminates on a
    // NON-match, so a genuine match keeps the slot alive -- the same shape
    // `start_repeat_message_on_an_fd_connected_link_carries_tx_fd_can_format`
    // uses for its own match/non-match proof.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 1, repeat_msg_data, mask_data, pattern_data),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a RawMode CAN ComLogicalLink");
    assert_ne!(msg_id, 0);

    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &can_frame(0x7E8, &[]), j2534_0404::CAN);
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "a matching-address response frame, evaluated against the client's own unprefixed \
         RawMode mask/pattern template, must be recognized as a MATCH and must not terminate a \
         Condition == 1 slot"
    );

    // Sanity: a genuinely non-matching frame (different address) still
    // terminates the slot -- proof match evaluation is actually running
    // against the real template, not merely "this slot never terminates".
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &can_frame(0x123, &[]), j2534_0404::CAN);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    while server
        .backdoor
        .repeat_message_status(MOCK_CHANNEL_ID, msg_id)
        != Some(0)
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(0),
        "sanity: a genuinely non-matching frame must still terminate the slot"
    );

    server.shutdown().await;
}

/// ADR-199: mirrors the CAN test above, but for a RawMode K-line (ISO14230)
/// CLL -- `ioctl_start_repeat_message` now passes the real `raw_mode` value
/// through to `tx_header::build_tx_message`, so the client's own KWP
/// header-inclusive `repeat_msg_data` is no longer doubled up with a
/// service-derived one (the double-header bug the old, now-removed,
/// unconditional RawMode rejection existed to guard against).
#[tokio::test]
#[serial]
async fn start_repeat_message_succeeds_on_a_raw_mode_kline_cll() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(
            1000,
            0,
            vec![0x80, 0x10, 0xF1, 0x01, 0x02],
            vec![0xFF, 0xFF, 0xFF],
            vec![0x80, 0x10, 0xF1],
        ),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a RawMode K-line ComLogicalLink");
    assert_ne!(msg_id, 0);

    server.shutdown().await;
}

/// ADR-199: under RawMode, the periodic-cap check's ISO15765 `4..=11`/
/// `5..=11` extended-addressing split derives from the client's own
/// `TxFlagIso15765AddrType` `tx_flag_bits` bit (via
/// `rpc_primitive::raw_mode_tx_flag_bit_to_j2534`), not the ComParam-derived
/// `CP_Can*Format` a RawMode client has no reason to configure -- mirroring
/// `rpc_primitive::resolve_send_recv_tx`'s identical RawMode
/// `extended_addressing` derivation (proven there by
/// `raw_mode_iso15765_extended_addressing_flag_enforces_the_five_byte_tx_floor`
/// in `raw_mode.rs`, mirrored here for Repeat Messaging).
#[tokio::test]
#[serial]
async fn start_repeat_message_raw_mode_iso15765_extended_addressing_enforces_five_byte_floor() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    // 4-byte CAN-ID-only prefix, no Address Extension byte -- claims
    // extended addressing via TxFlagIso15765AddrType, but is one byte short
    // of the extended-addressing periodic-cap floor (5..=11, ADR-186).
    let too_short = 0x7E0_u32.to_be_bytes().to_vec();
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup_with_tx_flags(
            1000,
            0,
            too_short,
            vec![0xFF, 0xFF, 0xFF, 0xFF],
            0x7E8_u32.to_be_bytes().to_vec(),
            vec![TxFlagBit::TxFlagIso15765AddrType as i32],
        ),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE should reject a repeat_msg_data one byte short of the \
         5-byte extended-addressing floor when TxFlagIso15765AddrType is set",
    );
    assert_eq!(status.code(), Code::InvalidArgument);

    // 5-byte prefix (CAN ID + AE byte), at the extended-addressing floor --
    // succeeds.
    let mut at_floor = 0x7E0_u32.to_be_bytes().to_vec();
    at_floor.push(0xF1);
    let mut mask_at_floor = 0x7E8_u32.to_be_bytes().to_vec();
    mask_at_floor.push(0xF1);
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup_with_tx_flags(
            1000,
            0,
            at_floor,
            vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            mask_at_floor,
            vec![TxFlagBit::TxFlagIso15765AddrType as i32],
        ),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed at the extended-addressing floor");
    assert_ne!(msg_id, 0);

    server.shutdown().await;
}

/// ADR-198 Decision item 7's ISO14230 manual-checksum `+1`-byte widening
/// (259 -> 260, SAE J2534-1 §8.3 Figure 42) now also applies to Repeat
/// Messaging's mask/pattern TEMPLATE size-range check (unlike the flat
/// 12-byte periodic-cap check on `repeat_msg_data`, which the cap already
/// makes moot for this widening) -- mirrors
/// `raw_mode_kline_tx_rejects_cop_data_outside_size_range`'s style in
/// `kline_raw_mode.rs`, but for `mask_data`/`pattern_data`.
#[tokio::test]
#[serial]
async fn start_repeat_message_raw_mode_kline_checksum_mode_off_widens_template_range() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let repeat_msg_data = vec![0x80, 0x10, 0xF1, 0x01, 0x02];
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    // ChecksumMode=OFF: the widened 1..=260 range applies -- a 260-byte
    // mask/pattern template is accepted.
    let cll_handle_off = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        false,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle_off,
        start_id,
        repeat_message_setup(
            1000,
            0,
            repeat_msg_data.clone(),
            vec![0xFFu8; 260],
            vec![0x00u8; 260],
        ),
    )
    .await
    .expect(
        "PDU_IOCTL_START_REPEAT_MESSAGE should accept a 260-byte mask/pattern template on a \
         RawMode ISO14230 ComLogicalLink with ChecksumMode=OFF",
    );
    assert_ne!(msg_id, 0);

    // A K-line physical channel can only be joined by CLLs sharing the same
    // resolved RawMode/ChecksumMode connect flag (ADR-198) -- disconnect
    // first (which also cleans up the still-live repeat slot just started,
    // mirroring `disconnect_com_logical_link_cleans_up_a_live_repeat_slot`)
    // before connecting the ChecksumMode=ON CLL below.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle_off),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // ChecksumMode=ON: the same 260-byte template is one byte over the
    // unwidened 1..=259 range and is rejected.
    let cll_handle_on = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        true,
        2,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    let status = start_repeat_message(
        &mut client,
        cll_handle_on,
        start_id,
        repeat_message_setup(
            1000,
            0,
            repeat_msg_data,
            vec![0xFFu8; 260],
            vec![0x00u8; 260],
        ),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE should reject a 260-byte mask/pattern template on a \
         RawMode ISO14230 ComLogicalLink with ChecksumMode=ON (unwidened 1..=259 range)",
    );
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// ADR-199: a RawMode client's own `tx_flag_bits` requesting
/// `TxFlagCan29bitId` is mapped through to the native `TX_EXTENDED_ID` flag
/// (`rpc_primitive::raw_mode_tx_flag_bit_to_j2534`) on BOTH the transmitted
/// `repeat_msg_data` message and the mask/pattern response template -- not
/// silently dropped, which is what the old, now-corrected,
/// `raw_mode_tx_flag_bit_to_j2534` doc comment used to claim this call site
/// did unconditionally. ADR-214: this test omits the v2
/// `response_tx_flag_bits` section entirely (`None`), so it doubles as the
/// "`None` preserves today's behavior exactly" regression -- the response
/// template must still inherit the request-side `tx_flag_bits` fold, and
/// this test now asserts that explicitly on both the transmitted message's
/// own TxFlags (`written_tx_flags`) and the response template's
/// (`repeat_slot_mask_pattern_tx_flags`), rather than only the latter.
#[tokio::test]
#[serial]
async fn start_repeat_message_raw_mode_tx_flag_bits_are_honored() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let mut repeat_msg_data = 0x123_u32.to_be_bytes().to_vec();
    repeat_msg_data.extend_from_slice(&[0x01, 0x02]);
    let mask_data = vec![0xFF, 0xFF, 0xFF, 0xFF];
    let pattern_data = 0x456_u32.to_be_bytes().to_vec();

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup_with_tx_flags(
            5000,
            0,
            repeat_msg_data,
            mask_data,
            pattern_data,
            vec![TxFlagBit::TxFlagCan29bitId as i32],
        ),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a RawMode CAN ComLogicalLink");
    assert_ne!(msg_id, 0);

    assert_eq!(
        server
            .backdoor
            .repeat_slot_mask_pattern_tx_flags(MOCK_CHANNEL_ID, msg_id)
            & j2534_0404::TX_EXTENDED_ID,
        j2534_0404::TX_EXTENDED_ID,
        "a RawMode client's own TxFlagCan29bitId bit must be mapped through to the native \
         TX_EXTENDED_ID flag, not silently dropped, on the mask/pattern response template -- \
         with no v2 response_tx_flag_bits section present (None), ADR-214 requires this to \
         still inherit the request-side tx_flag_bits fold identically to before this ADR"
    );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0) & j2534_0404::TX_EXTENDED_ID,
        j2534_0404::TX_EXTENDED_ID,
        "the transmitted repeat_msg_data message's own TxFlags must also carry TX_EXTENDED_ID"
    );

    server.shutdown().await;
}

/// ADR-214's own discriminating regression: a RawMode CAN link whose request
/// is 29-bit-addressed (`tx_flag_bits = [TxFlagCan29bitId]`) but whose real
/// ECU responds on the 11-bit/normal address space
/// (`response_tx_flag_bits = Some(vec![])`, an explicit empty v2 section)
/// must compose a mask/pattern response template WITHOUT `TX_EXTENDED_ID`,
/// while the transmitted message's own TxFlags still carries it -- proving
/// the two are now genuinely independent, not just that the response side
/// changed in isolation.
#[tokio::test]
#[serial]
async fn start_repeat_message_raw_mode_response_tx_flag_bits_override_the_request_side() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let mut repeat_msg_data = 0x123_u32.to_be_bytes().to_vec();
    repeat_msg_data.extend_from_slice(&[0x01, 0x02]);
    let mask_data = vec![0xFF, 0xFF, 0xFF, 0xFF];
    let pattern_data = 0x456_u32.to_be_bytes().to_vec();

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup_with_response_tx_flag_bits(
            5000,
            0,
            repeat_msg_data,
            mask_data,
            pattern_data,
            vec![TxFlagBit::TxFlagCan29bitId as i32],
            Some(vec![]),
        ),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a RawMode CAN ComLogicalLink");
    assert_ne!(msg_id, 0);

    assert_eq!(
        server
            .backdoor
            .repeat_slot_mask_pattern_tx_flags(MOCK_CHANNEL_ID, msg_id)
            & j2534_0404::TX_EXTENDED_ID,
        0,
        "an explicit empty response_tx_flag_bits v2 section must make the response template's \
         own addressing basis carry no TX_EXTENDED_ID, regardless of the request-side \
         tx_flag_bits value (ADR-214)"
    );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0) & j2534_0404::TX_EXTENDED_ID,
        j2534_0404::TX_EXTENDED_ID,
        "the transmitted repeat_msg_data message's own TxFlags must still carry TX_EXTENDED_ID \
         -- response_tx_flag_bits must never apply to the transmitted message (ADR-214 \
         Decision item 3)"
    );

    server.shutdown().await;
}

/// Inverse of the test above: request-side empty (11-bit/normal), response
/// side explicitly 29-bit (`response_tx_flag_bits = Some(vec![TxFlagCan29bitId])`)
/// -- the response template must carry `TX_EXTENDED_ID` while the
/// transmitted message does not.
#[tokio::test]
#[serial]
async fn start_repeat_message_raw_mode_response_tx_flag_bits_add_addressing_the_request_lacks() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let mut repeat_msg_data = 0x123_u32.to_be_bytes().to_vec();
    repeat_msg_data.extend_from_slice(&[0x01, 0x02]);
    let mask_data = vec![0xFF, 0xFF, 0xFF, 0xFF];
    let pattern_data = 0x456_u32.to_be_bytes().to_vec();

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup_with_response_tx_flag_bits(
            5000,
            0,
            repeat_msg_data,
            mask_data,
            pattern_data,
            vec![],
            Some(vec![TxFlagBit::TxFlagCan29bitId as i32]),
        ),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a RawMode CAN ComLogicalLink");
    assert_ne!(msg_id, 0);

    assert_eq!(
        server
            .backdoor
            .repeat_slot_mask_pattern_tx_flags(MOCK_CHANNEL_ID, msg_id)
            & j2534_0404::TX_EXTENDED_ID,
        j2534_0404::TX_EXTENDED_ID,
        "an explicit response_tx_flag_bits v2 section requesting TxFlagCan29bitId must make \
         the response template carry TX_EXTENDED_ID even though the request side is empty \
         (ADR-214)"
    );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0) & j2534_0404::TX_EXTENDED_ID,
        0,
        "the transmitted repeat_msg_data message's own TxFlags must NOT carry TX_EXTENDED_ID \
         -- the request-side tx_flag_bits was empty, and response_tx_flag_bits must never \
         apply to the transmitted message (ADR-214 Decision item 3)"
    );

    server.shutdown().await;
}

/// `edge-case-hunter` finding (ADR-214 review round): the two tests above
/// only exercise a plain (non-FD-substituted) RawMode CAN link. `raw_mode_
/// can_family`/`raw_mode_iso15765` (`rpc_misc.rs`) are computed from
/// `resources::base_protocol_id`, which normalizes `FD_CAN_PS` down to
/// plain `CAN` before either gate is evaluated -- so an FD-connected
/// RawMode link should fold `response_tx_flag_bits` identically to a
/// plain-CAN one, with no separate FD-specific gap. This test proves that
/// directly rather than leaving it as an unverified inference: an
/// FD-substituted (ADR-158) RawMode CAN link with an explicit empty
/// `response_tx_flag_bits` v2 section must still compose a response
/// template WITHOUT `TX_EXTENDED_ID`, even though the request side is
/// 29-bit-addressed and the transmitted message also carries
/// `TX_FD_CAN_FORMAT`.
#[tokio::test]
#[serial]
async fn start_repeat_message_raw_mode_response_tx_flag_bits_apply_on_an_fd_connected_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: staged FD ComParams should have substituted the native connect id to \
         FD_CAN_PS (ADR-158) -- otherwise this test would not actually be exercising an \
         FD-connected RawMode link"
    );

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let mut repeat_msg_data = 0x123_u32.to_be_bytes().to_vec();
    repeat_msg_data.extend_from_slice(&[0x01, 0x02]);
    let mask_data = vec![0xFF, 0xFF, 0xFF, 0xFF];
    let pattern_data = 0x456_u32.to_be_bytes().to_vec();

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup_with_response_tx_flag_bits(
            5000,
            0,
            repeat_msg_data,
            mask_data,
            pattern_data,
            vec![TxFlagBit::TxFlagCan29bitId as i32],
            Some(vec![]),
        ),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on an FD-connected RawMode CAN CLL");
    assert_ne!(msg_id, 0);

    assert_eq!(
        server
            .backdoor
            .repeat_slot_mask_pattern_tx_flags(MOCK_CHANNEL_ID, msg_id)
            & j2534_0404::TX_EXTENDED_ID,
        0,
        "an explicit empty response_tx_flag_bits v2 section must make the response template's \
         own addressing basis carry no TX_EXTENDED_ID on an FD-substituted link too, regardless \
         of the 29-bit request-side tx_flag_bits (ADR-214, raw_mode_can_family is unaffected by \
         FD substitution since base_protocol_id normalizes FD_CAN_PS to CAN first)"
    );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0) & j2534_0404::TX_EXTENDED_ID,
        j2534_0404::TX_EXTENDED_ID,
        "the transmitted repeat_msg_data message's own TxFlags must still carry TX_EXTENDED_ID \
         -- response_tx_flag_bits must never apply to the transmitted message (ADR-214 \
         Decision item 3), FD substitution included"
    );

    server.shutdown().await;
}

/// ADR-214's own end-to-end matching regression -- the actual bug this ADR
/// fixes, not just TxFlags composition: a RawMode CAN link with genuinely
/// asymmetric request/response widths (29-bit request, 11-bit response, via
/// an explicit empty `response_tx_flag_bits`), where a `Condition == 1`
/// slot's stop condition must be evaluated against the RESPONSE side's own
/// width, not the request side's. Mirrors
/// `condition_one_slot_does_not_terminate_on_a_frame_with_the_correct_response_format`'s
/// own RxStatus-injection technique.
#[tokio::test]
#[serial]
async fn start_repeat_message_raw_mode_asymmetric_addressing_matches_on_the_response_width() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // Request side: 29-bit-addressed (asserted via tx_flag_bits). Response
    // side: explicitly empty (11-bit/normal), via the v2 section -- mask is
    // the full 4-byte CAN-ID header, pattern is the expected 11-bit-style
    // response id 0x456 encoded the same 4-byte big-endian way
    // `tx_header::can_header_bytes` always uses regardless of width.
    let mut repeat_msg_data = 0x123_u32.to_be_bytes().to_vec();
    repeat_msg_data.extend_from_slice(&[0x01, 0x02]);
    let mask_data = vec![0xFF, 0xFF, 0xFF, 0xFF];
    let pattern_data = 0x456_u32.to_be_bytes().to_vec();

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup_with_response_tx_flag_bits(
            5000,
            1,
            repeat_msg_data,
            mask_data,
            pattern_data,
            vec![TxFlagBit::TxFlagCan29bitId as i32],
            Some(vec![]),
        ),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a RawMode CAN ComLogicalLink");
    assert_ne!(msg_id, 0);

    // A frame with the matching Data bytes (0x456's 4-byte big-endian
    // encoding) and NO CAN_29BIT_ID_STATUS -- the correct width for a
    // response side deliberately configured as empty/11-bit. Under the
    // OLD (pre-ADR-214) behavior, the response template would have
    // inherited the 29-bit request-side fold, misclassifying this
    // genuinely-matching frame as a non-match and terminating the slot
    // immediately (ADR-173 Decision 1). Under ADR-214's fix, this is a
    // genuine MATCH, so the slot must NOT terminate.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x456, &[]),
        j2534_0404::CAN,
        0,
    );

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "a frame with the correct RESPONSE-side width (11-bit, matching this test's explicit \
         empty response_tx_flag_bits) and matching Data bytes is a genuine MATCH and must not \
         terminate a Condition == 1 slot -- ADR-214 fixes the response template's addressing \
         basis to derive from response_tx_flag_bits, not the 29-bit request-side tx_flag_bits"
    );

    stop_repeat_message(&mut client, cll_handle, stop_id, msg_id)
        .await
        .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed");

    server.shutdown().await;
}

/// ADR-196 Decision item 1's documented no-op exception (Codex review, PR
/// #107 round 1): Analog Inputs/SCI are RawMode's OTHER allowlist member,
/// and `tx_header::build_tx_message`'s catch-all arm already returns the
/// payload unchanged for them regardless of `raw_mode`, so there is no
/// double-header-composition risk to guard against. A RawMode=ON SCI CLL
/// must therefore be able to start a repeat message just like a RawMode=OFF
/// one would.
#[tokio::test]
#[serial]
async fn start_repeat_message_succeeds_on_a_raw_mode_sci_cll() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::SCI_A_ENGINE,
        &[(j2534_0404::DATA_RATE, 7_812)],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    // Non-empty, equal-length mask/pattern data: SCI's `response_header_bytes`
    // contributes no header bytes of its own (ADR-051's "SCI: always a
    // no-op" -- `tx_header.rs`'s catch-all arm), so an empty mask_data here
    // (unlike the CAN test above, whose composed template is padded out by a
    // real CAN-ID header regardless) would compose a 0-byte J2534 mask
    // template and fail this function's own unrelated TX-size-range check
    // before ever reaching the RawMode gate this test exists to exercise.
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(1000, 0, vec![0x01, 0x02], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a RawMode SCI ComLogicalLink");
    assert_ne!(msg_id, 0);

    server.shutdown().await;
}
