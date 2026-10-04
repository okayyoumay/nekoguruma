//! `CllCreateFlag` RawMode extended to SAE J1939, ADR-200 Phase 3: unlike
//! CAN/ISO15765/K-line/J1850 (which all reuse the generic RawMode
//! passthrough), J1939 gets its own dedicated shim -- the D-PDU raw wire
//! shape (ISO 22900-2:2022 line 774/Table 80: 4-byte 29-bit CAN ID +
//! payload) and the native J2534-2 wire shape (SAE J2534-2 §16.4.3/Table 62:
//! 4-byte CAN ID + 1-byte destination address (DA) + payload) genuinely
//! differ for this one protocol. `tx_header::raw_j1939_tx_message` derives
//! and inserts the DA byte on TX (mechanically, from the client's own CAN-ID
//! PF/PS bytes -- no ComParam consulted); `events::
//! raw_j1939_rx_drop_destination_address` drops it back out on RX before the
//! client ever sees it. ChecksumMode is inert for J1939 (no message-level
//! checksum concept, only the CAN controller's own hardware CRC applies) --
//! not separately tested here, the same "no dedicated ChecksumMode test"
//! precedent `raw_mode.rs`'s own CAN/ISO15765 coverage already sets.
//!
//! Mirrors `j1939.rs`'s own helper conventions (opted-in module, explicit
//! `dlc_pin_data`, `CP_J1939AddressNegotiationRule` bit 1 to suppress
//! address-claim negotiation so `CoptSendrecv`/Repeat Messaging tests don't
//! need to exercise the claim state machine at all -- ADR-200's own J1939
//! claim-machinery-interaction analysis confirms the RawMode DA shim never
//! touches claim state, so this suppression is orthogonal, not a
//! simplification that hides anything the shim itself does).

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    CllCreateFlag, CllCreateFlagBit, ComLogicalLinkHandle, ComPrimitiveCtrlData,
    ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, DataItem, ExpectedResponseData,
    IoBytearray, ModuleHandle, PinData, ResourceData, StartComPrimitiveRequest,
    create_com_logical_link_request, data_item, io_ctl_request, resource_data,
};

use crate::harness::*;

/// Resource id of `"ISO_OBD_on_SAE_J1939_73"` -- same row `j1939.rs` uses
/// (`resources.rs` row 0x025D, `protocol: ChannelProtocol::J1939_PS`).
const J1939_ISO_OBD_RESOURCE_ID: u32 = 0x025D;

/// D-PDU `CP_J1939AddressNegotiationRule` ComParam ID (0x8086). Bit 1 set
/// (`0b10`) suppresses the address-claim negotiation `CoptStartcomm` would
/// otherwise run -- see `j1939.rs`'s identical constant/doc comment.
const CP_J1939_ADDR_NEG_RULE: u32 = 0x8086;

/// D-PDU `CP_J1939TargetAddress` ComParam ID (0x808C): must be a real
/// one-byte value (not the `0xFFFF` "not configured" sentinel) before
/// `CoptStartcomm` can succeed on any J1939 CLL, RawMode or not (ADR-179
/// Decision 4 -- a COP-level gate, orthogonal to RawMode).
const CP_J1939_TARGET_ADDRESS: u32 = 0x808C;

/// The standard two-pin (CAN_H/CAN_L on the J1962 connector, pins 6/14)
/// selection -- same as `j1939.rs`'s own `CAN_PINS`.
const CAN_PINS: &[(u32, &str)] = &[(6, "HI"), (14, "LOW")];

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every J1939 resource row requires this opt-in.
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

/// Builds a `RscData` resource selecting `resource_id` with explicit
/// `dlc_pin_data` -- clause 16.3.2.1 keeps J1939's physical layer
/// pin-unassigned until an explicit pin selection (ADR-179 Decision 2),
/// same shape as `j1939.rs`'s own `resource_with_protocol_id_and_pins`.
fn resource_with_protocol_id_and_pins(resource_id: u32, pins: &[(u32, &str)]) -> ResourceData {
    ResourceData {
        dlc_pin_data: pins
            .iter()
            .map(|&(number, type_name)| PinData {
                dlc_pin_number: number,
                dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                    type_name.to_string(),
                )),
            })
            .collect(),
        bus_type: None,
        protocol: Some(resource_data::Protocol::ProtocolId(resource_id)),
    }
}

/// Creates (but does not connect) a RawMode J1939 CLL.
async fn create_j1939_cll_raw_mode(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_id: u32,
    pins: &[(u32, &str)],
    _cll_tag: u64,
) -> ComLogicalLinkHandle {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(resource_id, pins),
            )),
            cll_create_flag: Some(
                create_com_logical_link_request::CllCreateFlag::CllCreateFlagBits(CllCreateFlag {
                    bits: vec![CllCreateFlagBit::CllCreateFlagRawMode as i32],
                }),
            ),
        })
        .await
        .expect("create_com_logical_link with RawMode=ON should succeed on SAE J1939 (ADR-200)")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// Creates and connects a RawMode J1939 CLL, staging `unum32_params` before
/// `ConnectComLogicalLink` -- mirrors `j1939.rs`'s own
/// `create_and_connect_j1939_cll`.
async fn create_and_connect_j1939_cll_raw_mode(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_id: u32,
    pins: &[(u32, &str)],
    unum32_params: &[(u32, u32)],
    cll_tag: u64,
) -> ComLogicalLinkHandle {
    let cll_handle = create_j1939_cll_raw_mode(client, resource_id, pins, cll_tag).await;

    for &(com_param_id, value) in unum32_params {
        set_com_param_unum32(client, cll_handle, com_param_id, value).await;
    }

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

/// Issues `CoptStartcomm` with no optional message -- non-negotiated
/// (`CP_J1939_ADDR_NEG_RULE = 0b10` already staged by the caller), so this
/// completes without ever attempting an address claim.
async fn start_comm(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: ComLogicalLinkHandle,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
}

/// ADR-200: RawMode=ON is now accepted at `CreateComLogicalLink` for SAE
/// J1939.
#[tokio::test]
#[serial]
async fn raw_mode_on_is_accepted_for_j1939_at_create_time() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll_handle =
        create_j1939_cll_raw_mode(&mut client, J1939_ISO_OBD_RESOURCE_ID, CAN_PINS, 1).await;

    server.shutdown().await;
}

/// ADR-200: PDU1 (PF < 240) -- the derived DA equals the client's own PS
/// byte, inserted at wire position 4 ahead of the native `PassThruWriteMsgs`
/// call. `CP_J1939AddressNegotiationRule` bit 1 suppresses the address-claim
/// negotiation entirely -- the DA derivation never consults claim state or
/// any ComParam, only the client's own raw CAN-ID bytes.
#[tokio::test]
#[serial]
async fn raw_mode_j1939_tx_derives_da_from_ps_byte_for_pdu1() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll_raw_mode(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_ADDR_NEG_RULE, 0b10),
            (CP_J1939_TARGET_ADDRESS, 0x00),
        ],
        1,
    )
    .await;
    start_comm(&mut client, cll_handle).await;

    // Client-composed raw CAN ID: byte0 = 0x18 (priority/data-page bits),
    // PF (byte1) = 0x00 (< 240, PDU1), PS (byte2) = 0x21 (=> derived DA),
    // source (byte3) = 0x80. Payload: [0x01, 0x02].
    let cop_data = vec![0x18, 0x00, 0x21, 0x80, 0x01, 0x02];
    send_data(&mut client, cll_handle, cop_data, vec![]).await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        vec![0x18, 0x00, 0x21, 0x80, 0x21, 0x01, 0x02],
        "DA (wire byte 4) must equal the client's own PS byte (byte 2) for PDU1"
    );

    server.shutdown().await;
}

/// ADR-200: PDU2 (PF >= 240) -- the derived DA is always `0xFF`
/// (BAM/broadcast segmentation, clause 16.4.4), regardless of the client's
/// own PS byte value.
#[tokio::test]
#[serial]
async fn raw_mode_j1939_tx_derives_da_as_broadcast_for_pdu2() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll_raw_mode(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_ADDR_NEG_RULE, 0b10),
            (CP_J1939_TARGET_ADDRESS, 0x00),
        ],
        1,
    )
    .await;
    start_comm(&mut client, cll_handle).await;

    // PF (byte1) = 0xF0 (>= 240, PDU2); PS (byte2) = 0x99 must be IGNORED.
    let cop_data = vec![0x18, 0xF0, 0x99, 0x80, 0x01, 0x02];
    send_data(&mut client, cll_handle, cop_data, vec![]).await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        vec![0x18, 0xF0, 0x99, 0x80, 0xFF, 0x01, 0x02],
        "DA (wire byte 4) must be 0xFF for PDU2 regardless of the PS byte"
    );

    server.shutdown().await;
}

/// ADR-200: a `cop_data` shorter than 4 bytes has no CAN-ID prefix to
/// derive a DA from -- rejected synchronously, not silently truncated or
/// panicking.
#[tokio::test]
#[serial]
async fn raw_mode_j1939_tx_rejects_cop_data_shorter_than_four_bytes() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll_raw_mode(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_ADDR_NEG_RULE, 0b10),
            (CP_J1939_TARGET_ADDRESS, 0x00),
        ],
        1,
    )
    .await;
    start_comm(&mut client, cll_handle).await;

    let status = send_data_expect_rejected(&mut client, cll_handle, vec![0x18, 0x00, 0x21]).await;
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("4 bytes"), "{}", status.message());

    server.shutdown().await;
}

/// ADR-200: a native 5-byte-header (CAN ID + DA + payload) received frame
/// is delivered to the client as 4 bytes (CAN ID + payload) -- the DA byte
/// does not leak into `ResultData.data_bytes`.
#[tokio::test]
#[serial]
async fn raw_mode_j1939_rx_drops_destination_address() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll_raw_mode(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_ADDR_NEG_RULE, 0b10),
            (CP_J1939_TARGET_ADDRESS, 0x00),
        ],
        1,
    )
    .await;
    start_comm(&mut client, cll_handle).await;
    // A receive-only monitor: NumSendCycles == 0, so its own 4-byte dummy
    // cop_data is never actually transmitted (still validated synchronously
    // against the >= 4-byte RawMode J1939 floor, ADR-100 Decision §5 (S8)'s
    // established pattern).
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x18, 0x00, 0x21, 0x80],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 0,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv, raw-mode receive-only monitor) should succeed");

    // Native 5-byte frame: CAN ID (0x18, 0x00, 0x21, 0x91) + DA (0x21) +
    // payload (0x01, 0x02).
    let frame = vec![0x18, 0x00, 0x21, 0x91, 0x21, 0x01, 0x02];
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::PROTOCOL_J1939_PS);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &[], &[], &[0x18, 0x00, 0x21, 0x91, 0x01, 0x02]);

    server.shutdown().await;
}

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id`.
/// Local copy, matching `j1939.rs`'s/`repeat_message.rs`'s own per-file
/// convention.
async fn resolve_ioctl_id(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
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

/// Issues `IoCtl` against a `cll_handle`. Local copy, matching
/// `j1939.rs`'s/`repeat_message.rs`'s own per-file convention.
async fn io_ctl_cll(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
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

fn repeat_message_setup(
    time_interval: u32,
    condition: u32,
    repeat_msg_data: Vec<u8>,
    mask_data: Vec<u8>,
    pattern_data: Vec<u8>,
) -> DataItem {
    DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup(
                time_interval,
                condition,
                &repeat_msg_data,
                &mask_data,
                &pattern_data,
                &[],
            ),
        })),
    }
}

async fn start_repeat_message(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
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

/// ADR-200: SAE J1939 Repeat Messaging under RawMode -- the client's own raw
/// (4-byte, DA-less) `mask_data`/`pattern_data` template gets a zeroed
/// (don't-care) byte inserted at position 4 before being handed to the
/// native `REPEAT_MSG_SETUP` call, so device-side matching against a real
/// native 5-byte (CAN-ID+DA) frame stays aligned from index 4 onward.
/// Mirrors `repeat_message.rs`'s `start_repeat_message_succeeds_on_a_raw_mode_can_cll_with_header_inclusive_template`'s
/// `inject_rx`-based match/non-match proof style (ADR-199).
///
/// Uses `inject_rx_with_status(..., CAN_29BIT_ID_STATUS)`, not plain
/// `inject_rx` -- `edge-case-hunter` review, this PR's own close-out pass:
/// an earlier draft of this test used the default (`RxStatus = 0`)
/// `inject_rx`, which coincidentally "matched" even though
/// `response_tx_flags` never carried `TX_EXTENDED_ID` at all for J1939
/// under RawMode (a real bug, since J1939 is unconditionally 29-bit --
/// unlike CAN/ISO15765, there is no legitimate 11-bit J1939 frame for a
/// missing `TX_EXTENDED_ID` to correctly reject). `RxStatus = 0` masked
/// that gap the same way `repeat_message.rs`'s own
/// `condition_one_slot_terminates_immediately_on_a_frame_with_the_wrong_response_format`
/// doc comment warns a byte-content-only proof can. `CAN_29BIT_ID_STATUS`
/// makes this test a genuine proof that a RawMode J1939 CLL's response
/// template format-matches a real, honestly-29-bit-flagged native frame.
#[tokio::test]
#[serial]
async fn raw_mode_j1939_repeat_message_mask_pattern_matches_a_native_five_byte_frame() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll_raw_mode(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_ADDR_NEG_RULE, 0b10),
            (CP_J1939_TARGET_ADDRESS, 0x00),
        ],
        1,
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    // Client-supplied raw repeat_msg_data: 4-byte CAN ID + 1 payload byte.
    let repeat_msg_data = vec![0x18, 0x00, 0x21, 0x80, 0x01];
    // Raw mask/pattern template: 4-byte CAN-ID-only shape (D-PDU raw
    // contract) -- the service must insert a zeroed byte at position 4
    // before this is compared against the native 5-byte frame below.
    let mask_data = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    let pattern_data = vec![0x18, 0x00, 0x21, 0x91, 0xAA];

    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(5000, 1, repeat_msg_data, mask_data, pattern_data),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a RawMode SAE J1939 ComLogicalLink");
    assert_ne!(msg_id, 0);

    // A native 5-byte frame matching the CAN ID (bytes 0-3) and the last
    // payload byte (0xAA), with an honest CAN_29BIT_ID_STATUS RxStatus (a
    // real J1939 frame is always 29-bit) -- the DA byte (index 4, value
    // 0x21 here) must be a don't-care and must NOT prevent this from
    // matching, and the format-status bit must agree with the template's
    // own (now-forced) TX_EXTENDED_ID for the match to register at all.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x18, 0x00, 0x21, 0x91, 0x21, 0xAA],
        j2534_0404::PROTOCOL_J1939_PS,
        j2534_0404::CAN_29BIT_ID_STATUS,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "a matching, honestly-29-bit-flagged frame (DA byte don't-care) must be recognized as \
         a MATCH and must not terminate a Condition == 1 slot"
    );

    // Sanity: a genuinely non-matching frame (different last payload byte)
    // still terminates the slot -- proof the position-4 insert didn't
    // accidentally make the WHOLE template a don't-care.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x18, 0x00, 0x21, 0x91, 0x21, 0x00],
        j2534_0404::PROTOCOL_J1939_PS,
        j2534_0404::CAN_29BIT_ID_STATUS,
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
        "sanity: a genuinely non-matching frame must still terminate the slot"
    );

    server.shutdown().await;
}
