//! `CllCreateFlag` RawMode (ADR-196, Phase 1): when a CLL is created with
//! `CLL_CREATE_FLAG_RAW_MODE` set, this service treats `cop_data`/
//! `CP_TesterPresentMsg` as the literal `PassThruMessage.Data` the client
//! already assembled (CAN ID in Table D.4 bytes 0-3) instead of building a
//! header itself (Decision item 2), makes `TX_FLAG_CAN_29BIT_ID`/
//! `TX_FLAG_ISO15765_ADDR_TYPE` client-authoritative instead of
//! service-derived (Decision item 2), and skips the RX header/footer split
//! entirely, delivering the whole received frame in `data_bytes` with
//! `extra_info` absent (Decision item 3). Phase 1 restricts RawMode=ON to
//! base CAN and hardware ISO15765 CLLs (Decision item 1); every other
//! protocol, and any CLL resolving to software ISO-TP, is rejected at
//! `CreateComLogicalLink`. ADR-198 Phase 2 extends RawMode=ON to hardware
//! K-line (ISO9141/ISO14230) and gives ChecksumMode its first real semantics
//! -- see `kline_raw_mode.rs` for that coverage. ADR-200 Phase 3 extends
//! RawMode=ON further to SAE J1850 (VPW/PWM, with a ChecksumMode=ON
//! requirement) and SAE J1939 (via a dedicated destination-address shim) --
//! see `j1850_raw_mode.rs`/`j1939_raw_mode.rs` for that coverage -- and
//! closes out TP2.0 as a PERMANENT RawMode exclusion (not a residual); this
//! file's own former J1850 rejection test (J1850 is now allowlisted) is
//! repurposed below as the TP2.0 permanent-exclusion test, the "every other
//! protocol" representative.
//!
//! `rx_header_split.rs` covers the non-RawMode (RawMode=OFF, the default)
//! baseline this file's RX test is contrasted against -- both files share
//! `assert_result_data`'s `(header, footer, payload)` shape.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    CllCreateFlag, CllCreateFlagBit, ComPrimitiveCtrlData, ComPrimitiveHandle, EventNotification,
    ExpectedResponseData, ModuleHandle, PinData, ResourceData, StartComPrimitiveRequest,
    SubscribeEventRequest, TxFlagBit, create_com_logical_link_request, event_item, resource_data,
    subscribe_event_request, vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Like [`arm_receive_only_monitor`], but with a 4-byte dummy `cop_data`
/// (`[0x00; 4]`) instead of that helper's 1-byte one -- a RawMode CLL's
/// `cop_data` IS the literal `PassThruMessage.Data` (Decision item 2), so
/// the same SAE J2534-1 TX size-range validation (ADR-049) that always ran
/// against the constructed message now runs against this raw buffer
/// directly, and CAN's own range floor (4 bytes, the CAN ID width) rejects
/// `arm_receive_only_monitor`'s shorter 1-byte dummy. This COP is never
/// actually transmitted (`NumSendCycles == 0`), so the dummy bytes' content
/// is otherwise inert -- see `arm_receive_only_monitor`'s own doc comment
/// for the full ADR-100 Decision §5 (S8) rationale this mirrors.
async fn arm_receive_only_monitor_raw_mode(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x00; 4],
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
}

/// ADR-196 Decision item 2: a RawMode=ON base-CAN CLL sends `cop_data`
/// unchanged onto the wire -- the client already prefixed its own 4-byte CAN
/// ID (Table D.4), so `build_tx_message` must not prepend a second one from
/// ComParams (which are not even staged here: no `CP_CanPhysReqId`/
/// `UniqueRespIdTable`, unlike every non-RawMode TX test in `comparam_tx.rs`
/// -- RawMode's whole point is that this service no longer needs to resolve
/// addressing at all).
#[tokio::test]
#[serial]
async fn raw_mode_can_tx_sends_client_constructed_payload_unchanged() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let mut cop_data = 0x123_u32.to_be_bytes().to_vec();
    cop_data.extend_from_slice(&[0x02, 0x10, 0x03]);
    send_data(&mut client, cll_handle, cop_data.clone(), vec![]).await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), cop_data);
    assert_eq!(server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0), 0);

    server.shutdown().await;
}

/// ADR-196 Decision item 2: under RawMode, `TX_FLAG_CAN_29BIT_ID` (ISO
/// 22900-2:2022 Annex D.2.1 Table D.4, "RAW_MODE Only") becomes client-authoritative --
/// `compute_j2534_tx_flags` maps the client's own `TxFlagBit` straight
/// through to native `TX_EXTENDED_ID` instead of the non-RawMode path's
/// discard-and-rederive-from-ComParams (ADR-062), which would have produced
/// `0` here anyway since no addressing ComParam is staged at all.
#[tokio::test]
#[serial]
async fn raw_mode_can_tx_maps_client_29bit_flag_to_tx_extended_id() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let mut cop_data = 0x18DAF110_u32.to_be_bytes().to_vec();
    cop_data.extend_from_slice(&[0x02, 0x10, 0x03]);
    send_data(
        &mut client,
        cll_handle,
        cop_data.clone(),
        vec![TxFlagBit::TxFlagCan29bitId],
    )
    .await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), cop_data);
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::TX_EXTENDED_ID
    );

    server.shutdown().await;
}

/// ADR-196 Decision item 3: a RawMode=ON CLL's received frame is delivered
/// whole -- `data_bytes` carries the CAN ID prefix AND the payload together,
/// `extra_info` is absent -- unlike the RawMode=OFF baseline
/// (`rx_header_split.rs`'s `can_protocol_splits_can_id_header_into_extra_info`),
/// which splits the same 4-byte CAN ID out into `extra_info.header_bytes`
/// for the identical CAN protocol.
#[tokio::test]
#[serial]
async fn raw_mode_can_rx_delivers_full_frame_with_no_header_split() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the injected
    // frame below delivered under the unbound-discard model. Unlike
    // `rx_header_split.rs`'s equivalent CAN test, no CP_RequestAddrMode/
    // CP_CanFuncReqId staging is needed for this monitor's own TX
    // resolution to succeed -- RawMode's `build_tx_message` never resolves
    // addressing at all (see the TX test above). Uses the raw-mode variant
    // of the monitor helper -- see its own doc comment for why.
    arm_receive_only_monitor_raw_mode(&mut client, cll_handle).await;

    let payload = vec![0x01, 0x02, 0x03, 0x04];
    let mut frame = 0x123_u32.to_be_bytes().to_vec();
    frame.extend_from_slice(&payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &[], &[], &frame);

    server.shutdown().await;
}

/// Builds a `RscData` resource selecting `protocol_id` via the raw
/// hardware-protocol-id route, with the given typed `(pin_number,
/// pin_type_name)` pairs as `dlc_pin_data` -- same shape as `tp20.rs`'s/
/// `j1939.rs`'s own `resource_with_protocol_id_and_pins` (this crate's
/// existing per-file-duplication convention for this helper).
fn resource_with_protocol_id_and_pins(protocol_id: u32, pins: &[(u32, &str)]) -> ResourceData {
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
        protocol: Some(resource_data::Protocol::ProtocolId(protocol_id)),
    }
}

/// ADR-200 (Phase 3): RawMode=ON on a TP2.0 CLL is a PERMANENT exclusion,
/// not a deferred residual like J1850/J1939 were before this ADR -- ISO
/// 22900-2:2022 defines no RawMode wire shape for TP2.0 at all (no Table 80
/// row), and this service's own dispatch logic rewrites the live,
/// service-negotiated TX-ID at every send (`events.rs`, ADR-188), which is
/// fundamentally incompatible with a client-owned raw header. Uses a real
/// opted-in-module + correctly-pinned TP2.0 resource (mirroring `tp20.rs`'s
/// own `create_tp20_cll`) so the rejection is proven to come from the
/// RawMode allowlist check itself, not an earlier pin/opt-in failure.
///
/// This test previously exercised J1850VPW as Phase 2's "every other
/// protocol" representative -- repurposed to TP2.0 now that J1850 itself is
/// allowlisted (ADR-200); J1850's own acceptance tests live in
/// `j1850_raw_mode.rs`.
#[tokio::test]
#[serial]
async fn raw_mode_on_a_tp2_0_protocol_is_permanently_rejected_at_create_time() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(vci_service_interface::CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(
                    j2534_0404::PROTOCOL_TP2_0_PS,
                    &[(6, "HI"), (14, "LOW")],
                ),
            )),
            cll_create_flag: Some(
                create_com_logical_link_request::CllCreateFlag::CllCreateFlagBits(CllCreateFlag {
                    bits: vec![CllCreateFlagBit::CllCreateFlagRawMode as i32],
                }),
            ),
        })
        .await
        .expect_err("RawMode=ON on TP2.0 should be permanently rejected (ADR-200)");

    assert_eq!(status.code(), Code::Unimplemented);
    assert!(status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));

    server.shutdown().await;
}

/// ADR-200 (Phase 3) regression sweep: widening the RawMode allowlist to
/// J1850/J1939 must not accidentally loosen any OTHER still-excluded
/// protocol's rejection -- SAE J1708 (a plain UART-family protocol with no
/// RawMode support of its own) is representative.
#[tokio::test]
#[serial]
async fn raw_mode_on_a_j1708_protocol_is_still_rejected_at_create_time() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(vci_service_interface::CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_J1708_PS, &[(1, "PLUS")]),
            )),
            cll_create_flag: Some(
                create_com_logical_link_request::CllCreateFlag::CllCreateFlagBits(CllCreateFlag {
                    bits: vec![CllCreateFlagBit::CllCreateFlagRawMode as i32],
                }),
            ),
        })
        .await
        .expect_err("RawMode=ON on J1708 should still be rejected -- ADR-200 does not touch it");

    assert_eq!(status.code(), Code::Unimplemented);
    assert!(status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));

    server.shutdown().await;
}

/// ADR-196 Decision item 1: RawMode=ON is rejected at `CreateComLogicalLink`
/// for a CLL that resolves to `can_channel_mode = "software-isotp"` --
/// `hw_protocol_id` alone (the raw CAN channel underneath) would otherwise
/// pass the bare base-CAN allowlist check, so this is checked separately via
/// `!software_isotp`.
#[tokio::test]
#[serial]
async fn raw_mode_on_a_software_isotp_link_is_rejected_at_create_time() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(vci_service_interface::CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(j2534_0404::ISO15765),
            )),
            cll_create_flag: Some(
                create_com_logical_link_request::CllCreateFlag::CllCreateFlagBits(CllCreateFlag {
                    bits: vec![CllCreateFlagBit::CllCreateFlagRawMode as i32],
                }),
            ),
        })
        .await
        .expect_err("RawMode=ON on a software-ISO-TP-mode CLL should be rejected in Phase 1");

    assert_eq!(status.code(), Code::Unimplemented);
    assert!(status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));

    server.shutdown().await;
}

/// ADR-196 Decision item 1: a reserved bit (byte 0 bit 5, Table D.6 Unused)
/// in `cll_create_flag_raw` is rejected `invalid_argument` at create time --
/// this phase must not repeat the silently-ignored-flag defect it closes for
/// bits 6/7 by silently accepting a bit it does not itself understand.
#[tokio::test]
#[serial]
async fn raw_mode_flag_rejects_a_reserved_bit_in_the_raw_byte_form() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(vci_service_interface::CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(j2534_0404::CAN),
            )),
            cll_create_flag: Some(
                create_com_logical_link_request::CllCreateFlag::CllCreateFlagRaw(
                    vec![0x20], // byte 0 bit 5 -- Table D.6 Unused
                ),
            ),
        })
        .await
        .expect_err("a reserved cll_create_flag_raw bit should be rejected");

    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// ADR-196 Decision item 1: an unrecognized/out-of-range `CllCreateFlagBit`
/// enum value in the named-bits form is rejected `invalid_argument` the same
/// way a reserved raw byte bit is -- the bits and raw-byte representations
/// must reject an unrecognized flag identically.
#[tokio::test]
#[serial]
async fn raw_mode_flag_rejects_an_unrecognized_enum_value_in_the_bits_form() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(vci_service_interface::CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(j2534_0404::CAN),
            )),
            cll_create_flag: Some(
                create_com_logical_link_request::CllCreateFlag::CllCreateFlagBits(CllCreateFlag {
                    bits: vec![99],
                }),
            ),
        })
        .await
        .expect_err("an unrecognized cll_create_flag_bits value should be rejected");

    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Confirms RawMode=OFF (the implicit default, `cll_create_flag: None`) is
/// byte-for-byte unaffected by ADR-196 -- an ordinary CAN CoptSendrecv still
/// gets its CAN ID header prepended by the service, exactly as every
/// existing (pre-ADR-196) `comparam_tx.rs` test already pins.
#[tokio::test]
#[serial]
async fn raw_mode_off_is_unaffected_can_tx_still_gets_a_service_built_header() {
    let server = TestServer::start().await;
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
        vec![ecu_entry(1, vec![unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0)])],
    )
    .await;

    let payload = vec![0x02, 0x10, 0x03];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected = 0x7E0_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    server.shutdown().await;
}

/// `CP_RCByteOffset`/`CP_RC78Handling` ComParam IDs -- not exported by the
/// crate, so duplicated here as literals, mirroring `rc_handling.rs`'s own
/// identical local constants.
const CP_RC_BYTE_OFFSET: u32 = 0x8028;
const CP_RC78_HANDLING: u32 = 0x8027;

/// A single-shot `CoptSendrecv` with `num_receive_cycles: 1` and a VACUOUS
/// expected-response descriptor (empty mask/pattern -- matches any payload,
/// `ExpectedResponse::is_vacuous`) -- deliberately chosen so that, absent
/// correct pending-RC detection, ANY delivered frame (including a genuine
/// pending-RC negative response) would spuriously complete this COP via the
/// Tier1Vacuous scan. Mirrors `rc_handling.rs`'s own `start_send_recv`
/// helper, generalized to accept the full raw `cop_data` a RawMode CLL needs
/// (CAN-ID prefix included, ADR-196 Decision item 2).
async fn start_send_recv_raw(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
    cop_data: Vec<u8>,
) -> ComPrimitiveHandle {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 1,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
}

async fn subscribe(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) -> tonic::Streaming<EventNotification> {
    client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner()
}

fn is_finished(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(event_item::Data::CopStatus(status))
            if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
    )
}

/// ADR-196 Decision item 3b: the concrete end-to-end proof this fix closes
/// the bug -- a RawMode ISO15765 CLL with `CP_RC78Handling`/`CP_RCByteOffset`
/// configured, whose native response is shaped
/// `[CAN_ID(4 bytes), 0x7F, SID, 0x78]` (a genuine pending-RC negative
/// response, delivered whole per Decision item 3's RX split skip), must
/// extend the pending-RC wait (P2* behavior) instead of the response being
/// wrongly treated as a completed positive result via the vacuous
/// `expected_response` match. Before this fix, `RcHandlingConfig::
/// detect_pending_rc`'s own `0x7F`/SID-echo anchors were hardcoded at
/// absolute `data[0]`/`data[1]` -- the frame's own CAN-ID prefix bytes, never
/// `0x7F` -- so the pending-RC shape check always failed and the vacuous
/// descriptor bound the frame as an ordinary positive match instead.
#[tokio::test]
#[serial]
async fn raw_mode_iso15765_pending_rc_extends_wait_instead_of_completing_as_positive() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            // Deliberately much larger than the assertion windows below, so
            // the base response timeout can never be what keeps the COP from
            // finishing -- only correct pending-RC detection can.
            (j2534_0404::P2_MAX, 10_000_000),
            (CP_RC78_HANDLING, 1),
            // The standard UDS `0x7F <SID> <NRC>` shape re-based by the
            // RawMode raw CAN-ID prefix width (raw_prefix = 4 for a plain,
            // non-extended-addressing ISO15765 frame): NRC at raw_prefix + 2.
            (CP_RC_BYTE_OFFSET, 6),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // Client-composed raw request: CAN ID 0x7E0 prefix + SID 0x22 request --
    // `tx_prefix` resolves to 4 (no ISO15765 Address Extension bit set), so
    // `request_sid` is captured as 0x22, matching the response's own SID
    // echo below.
    let mut cop_data = 0x7E0_u32.to_be_bytes().to_vec();
    cop_data.extend_from_slice(&[0x22, 0xF1, 0x90]);
    let cop = start_send_recv_raw(&mut client, cll_handle, cop_data).await;
    let is_finished_for_cop = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop.cop_handle)
    };
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Native response: [CAN_ID(4 bytes), 0x7F, SID, 0x78] -- delivered whole,
    // with no header/footer split (Decision item 3), so this is exactly the
    // `data_bytes` shape `detect_pending_rc` must classify.
    let mut frame = 0x7E8_u32.to_be_bytes().to_vec();
    frame.extend_from_slice(&[0x7F, 0x22, 0x78]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO15765);

    assert!(
        !wait_for_event(&mut events, 300, is_finished_for_cop).await,
        "a pending-RC (NRC 0x78) response must extend the wait, not be misclassified as a \
         completed positive result via the vacuous expected_response match -- this is exactly \
         the bug ADR-196 Decision item 3b closes"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-197: the RawMode counterpart of
/// `rx_header_split.rs`'s `iso15765_no_table_wildcard_rx_status_bit_widens_header_to_five_bytes`
/// -- a no-table wildcard RawMode=ON ISO15765 CLL, with the incoming NRC's
/// native `RxStatus` bit 7 (`ISO15765_ADDR_TYPE_STATUS`) set, must re-base
/// `raw_prefix` to 5 (CAN ID + Address Extension byte) instead of the plain
/// 4-byte width `raw_mode_iso15765_pending_rc_extends_wait_instead_of_completing_as_positive`
/// exercises. Before ADR-197's fix, `header_footer_len` had no way to learn
/// this frame was extended-addressed (no `UniqueRespIdTable` entry to
/// consult), so `raw_prefix` stayed 4, `detect_pending_rc` misread the
/// Address Extension byte (`0xF1`) as if it were the `0x7F` pending-RC
/// marker, the shape check failed, and the frame was wrongly finalized as a
/// completed positive result via the vacuous `expected_response` match.
#[tokio::test]
#[serial]
async fn raw_mode_iso15765_pending_rc_extended_addressing_rebases_nrc_anchor_past_five_byte_prefix()
{
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            // Deliberately much larger than the assertion window below, so
            // the base response timeout can never be what keeps the COP from
            // finishing -- only correct pending-RC detection can.
            (j2534_0404::P2_MAX, 10_000_000),
            (CP_RC78_HANDLING, 1),
            // The standard UDS `0x7F <SID> <NRC>` shape re-based by the
            // RawMode raw CAN-ID prefix width (raw_prefix = 5 for extended
            // addressing, ADR-197): NRC at raw_prefix + 2 = 7.
            (CP_RC_BYTE_OFFSET, 7),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // Client-composed raw request: CAN ID 0x7E0 prefix + SID 0x22 request --
    // `tx_prefix` (TX-side, unrelated to this fix) resolves to 4 (no
    // ISO15765 Address Extension bit set on this request), so `request_sid`
    // is captured as 0x22, matching the response's own SID echo below.
    let mut cop_data = 0x7E0_u32.to_be_bytes().to_vec();
    cop_data.extend_from_slice(&[0x22, 0xF1, 0x90]);
    let cop = start_send_recv_raw(&mut client, cll_handle, cop_data).await;
    let is_finished_for_cop = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop.cop_handle)
    };
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Native response: [CAN_ID(4 bytes), AE(1 byte), 0x7F, SID, 0x78] --
    // delivered whole (Decision item 3), with native RxStatus bit 7 set
    // (ADR-197) since no UniqueRespIdTable entry exists for this CLL to
    // consult instead.
    const RESP_EXT_ADDR: u8 = 0xF1;
    let mut frame = 0x7E8_u32.to_be_bytes().to_vec();
    frame.push(RESP_EXT_ADDR);
    frame.extend_from_slice(&[0x7F, 0x22, 0x78]);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::ISO15765,
        j2534_0404::ISO15765_ADDR_TYPE_STATUS,
    );

    assert!(
        !wait_for_event(&mut events, 300, is_finished_for_cop).await,
        "a pending-RC (NRC 0x78) response on an extended-addressed, no-table \
         wildcard RawMode CLL must extend the wait, not be misclassified as a \
         completed positive result -- this is exactly the bug ADR-197 fixes \
         for raw_prefix"
    );

    drop(events);
    server.shutdown().await;
}

/// Codex review, PR #107 round 2: `resolve_send_recv_tx`'s ISO15765-2
/// functional-addressing Single Frame limit (ADR-055) used to compare
/// `cop_data.len()` directly against the payload-only `max_sf_payload`
/// limit -- under RawMode, `cop_data` is the client's own literal
/// `[CAN ID][payload]` frame (ADR-196 Decision item 2), not a payload-only
/// buffer, so a genuinely in-range RawMode request was rejected purely for
/// carrying its own required 4-byte CAN-ID prefix. Fixed by subtracting the
/// RawMode `tx_prefix` (`compute_tx_prefix`, the same shared helper ADR-196
/// Decision item 3b already established elsewhere in this file) before
/// comparing against the limit.
#[tokio::test]
#[serial]
async fn raw_mode_functional_addressing_single_frame_limit_excludes_the_can_id_prefix() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;

    // Normal addressing's Single Frame max is 7 bytes
    // (comparam_tx.rs::iso15765_functional_addressing_rejects_multi_frame_payload).
    // Under RawMode, cop_data also carries its own 4-byte CAN-ID prefix (no
    // ISO15765 Address Extension bit set), so an 8-byte payload needs 12
    // total raw bytes to be rejected, and a 7-byte payload needs 11 total
    // raw bytes to be accepted -- proving the fix compares the PAYLOAD
    // portion, not the raw total, against the limit.
    let mut too_long = 0x7DF_u32.to_be_bytes().to_vec();
    too_long.extend_from_slice(&[0u8; 8]);
    let status = send_data_expect_rejected(&mut client, cll_handle, too_long).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("Single Frame"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    let mut at_max = 0x7DF_u32.to_be_bytes().to_vec();
    at_max.extend_from_slice(&[0u8; 7]);
    send_data(&mut client, cll_handle, at_max.clone(), vec![]).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), at_max);

    server.shutdown().await;
}

/// Codex review, PR #107 round 3: `compute_j2534_tx_flags`'s RawMode
/// `TxFlagCan29bitId`/`TxFlagIso15765AddrType` passthrough (ADR-196
/// Decision item 2) used to key only on `raw_mode`, with no protocol check
/// -- but Decision item 1's RawMode allowlist also accepts Analog
/// Inputs/SCI as a documented no-op. Setting these CAN-specific bits on a
/// RawMode=ON SCI CLL must NOT leak `TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE`
/// into the native SCI TxFlags -- doing so would violate Decision item 1's
/// own "RawMode ON/OFF are observably identical" guarantee for SCI.
#[tokio::test]
#[serial]
async fn raw_mode_sci_tx_does_not_leak_can_specific_flags() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::SCI_A_ENGINE,
        &[(j2534_0404::DATA_RATE, 7_812)],
    )
    .await;

    send_data(
        &mut client,
        cll_handle,
        vec![0x01, 0x02],
        vec![
            TxFlagBit::TxFlagCan29bitId,
            TxFlagBit::TxFlagIso15765AddrType,
        ],
    )
    .await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::TX_NORMAL_TRANSMIT,
        "CAN-specific TxFlagCan29bitId/TxFlagIso15765AddrType must not reach \
         a RawMode SCI link's native TxFlags"
    );

    server.shutdown().await;
}

/// Codex review, PR #107 round 3: the functional Single Frame limit fix
/// from round 2 subtracted the RawMode prefix width from `cop_data.len()`,
/// but still compared it against `max_sf_payload` derived from the
/// ComParam-driven `tx_addressing` -- which defaults to `Normal` (7-byte
/// limit) under RawMode, since a RawMode client has no reason to configure
/// `CP_CanFuncReqFormat` (ADR-196 Decision item 2). An extended-addressed
/// RawMode request (5-byte prefix) genuinely has only 6 bytes of Single
/// Frame payload capacity, not 7 -- this proves a 7-byte payload is now
/// correctly rejected and a 6-byte payload is correctly accepted.
#[tokio::test]
#[serial]
async fn raw_mode_functional_addressing_single_frame_limit_reflects_extended_addressing() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;

    // Extended-addressed: [CAN ID(4)][AE(1)][payload] = 5-byte prefix, via
    // the client-authoritative TxFlagIso15765AddrType bit (Decision item 2).
    let mut too_long = 0x7DF_u32.to_be_bytes().to_vec();
    too_long.push(0xF1); // Address Extension byte
    too_long.extend_from_slice(&[0u8; 7]); // 7-byte payload: only 6 fit extended
    let status = send_data_expect_rejected_with_flags(
        &mut client,
        cll_handle,
        too_long,
        vec![TxFlagBit::TxFlagIso15765AddrType],
    )
    .await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("Single Frame"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    let mut at_max = 0x7DF_u32.to_be_bytes().to_vec();
    at_max.push(0xF1);
    at_max.extend_from_slice(&[0u8; 6]); // exactly 6-byte payload: fits extended
    send_data(
        &mut client,
        cll_handle,
        at_max.clone(),
        vec![TxFlagBit::TxFlagIso15765AddrType],
    )
    .await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), at_max);

    server.shutdown().await;
}

/// Codex review, PR #107 round 4: `resolve_can_addressing`'s functional arm
/// requires `CP_CanFuncReqId` to be configured (`?`-returns `None`
/// otherwise) even though `use_functional_addressing` already detected
/// `CP_RequestAddrMode = 2` -- but a RawMode client has no reason to set
/// `CP_CanFuncReqId` at all, since it already embeds the literal CAN ID in
/// `cop_data` (Decision item 2), making that ComParam redundant. Before this
/// fix, `can_addressing` resolved to `None` in exactly this configuration,
/// silently skipping the whole functional Single Frame check. This proves
/// the check now fires anyway, reading functional intent directly from
/// `CP_RequestAddrMode`.
#[tokio::test]
#[serial]
async fn raw_mode_functional_single_frame_limit_applies_without_can_func_req_id() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            // Deliberately no CP_CAN_FUNC_REQ_ID -- the RawMode client
            // embeds the CAN ID directly in cop_data instead.
        ],
    )
    .await;

    let mut too_long = 0x7DF_u32.to_be_bytes().to_vec();
    too_long.extend_from_slice(&[0u8; 8]);
    let status = send_data_expect_rejected(&mut client, cll_handle, too_long).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("Single Frame"),
        "expected a Single Frame rejection, got: {}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    let mut at_max = 0x7DF_u32.to_be_bytes().to_vec();
    at_max.extend_from_slice(&[0u8; 7]);
    send_data(&mut client, cll_handle, at_max.clone(), vec![]).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), at_max);

    server.shutdown().await;
}

/// Codex review, PR #107 round 5: `resolve_send_recv_tx`'s `can_functional`
/// (`ResolvedSendRecvTx::can_functional`) used to stay `None` whenever
/// `can_addressing` failed to resolve -- which, under RawMode with no
/// `CP_CanFuncReqId` configured (a supported configuration, since the
/// client's own raw `cop_data` already embeds the CAN ID), it always did,
/// even when `CP_RequestAddrMode` genuinely requested functional
/// addressing. `wait_for_p3_gap`'s `CP_P3Func` enforcement keys off this
/// field being `Some(true)`; with it silently `None`, the gap was never
/// enforced. This proves the gap now applies to a RawMode functional send
/// with no `CP_CanFuncReqId` configured, mirroring
/// `p3_gap.rs::func_gap_enforced_when_upcoming_func_send_requires_no_response`'s
/// non-RawMode counterpart.
#[tokio::test]
#[serial]
async fn raw_mode_functional_p3_gap_enforced_without_can_func_req_id() {
    const CP_P3_FUNC: u32 = 0x80B3;

    // Fire-and-forget `CoptSendrecv`, deliberately WITHOUT `send_data`'s own
    // fixed 100ms settle sleep before returning -- that sleep is fine for a
    // one-shot "did it write" check, but it would eat into the very P3Func
    // gap window this test times, corrupting the measurement (P3-gap.rs's
    // own timing tests use the identical fire-and-forget shape for the same
    // reason).
    async fn start_send_recv_no_wait(
        client: &mut VciServiceClient<tonic::transport::Channel>,
        cll_handle: vci_service_interface::ComLogicalLinkHandle,
        cop_data: Vec<u8>,
    ) {
        client
            .start_com_primitive(StartComPrimitiveRequest {
                cop_tag: None,
                cll_handle: Some(cll_handle),
                cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
                cop_data,
                cop_ctrl_data: Some(ComPrimitiveCtrlData {
                    time: 0,
                    num_send_cycles: 1,
                    num_receive_cycles: 0,
                    temp_param_update: 0,
                    expected_response_array: vec![],
                    tx_flag: None,
                }),
            })
            .await
            .expect("start_com_primitive should succeed");
    }

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_P3_FUNC, 300_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (j2534_0404::P2_MAX, 50_000),
            // Deliberately no CP_CAN_FUNC_REQ_ID -- the RawMode client
            // embeds the CAN ID directly in cop_data instead.
        ],
    )
    .await;

    let mut cop_data = 0x7DF_u32.to_be_bytes().to_vec();
    cop_data.extend_from_slice(&[0x01, 0x00]);

    start_send_recv_no_wait(&mut client, cll_handle, cop_data.clone()).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let started = std::time::Instant::now();
    start_send_recv_no_wait(&mut client, cll_handle, cop_data.clone()).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed >= std::time::Duration::from_millis(280),
        "the second RawMode functional send should wait ~300ms for \
         CP_P3Func even with no CP_CanFuncReqId configured (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// Waits for the next `PduCopstFinished` on an already-open event stream.
/// Local copy of `tester_present_reqrsp.rs`'s identical helper -- the stream
/// must be subscribed BEFORE the COP being waited on is started, so its
/// `PduCopstFinished` cannot be missed.
async fn wait_for_cop_finished(events: &mut tonic::Streaming<EventNotification>) {
    assert!(
        wait_for_event(events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "expected a PduCopstFinished event"
    );
}

/// Local copy of `tester_present_reqrsp.rs`'s identical helper.
async fn start_comm(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
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

/// Codex review, PR #107 round 5's `raw_mode_functional_p3_gap_enforced_without_can_func_req_id`
/// above proves `can_functional`'s fix for `wait_for_p3_gap`; this test
/// proves the same fix for `tester_present_tx_can_id`'s TX-side echo
/// discard -- `resolve_tester_present`'s `can_functional` used to stay
/// `None` for a RawMode tester-present CLL with no `CP_CanFuncReqId`/
/// physical `UniqueRespIdTable` entry configured, a supported RawMode
/// configuration since the client's own `CP_TesterPresentMessage` bytes
/// already embed the literal CAN ID (ADR-196 Decision item 2), making that
/// ComParam redundant. With `can_functional` wrongly `None`,
/// `tester_present_tx_can_id` refused to extract tester-present's own TX
/// CAN ID, so a TX_DONE/TX_INDICATION/CONFIG_LOOPBACK loopback echo of
/// tester-present's own periodic send was never recognized and discarded --
/// it leaked into the client's subscribed event stream as an unrelated
/// `ResultData`. Mirrors `tester_present_reqrsp.rs`'s
/// `reqrsp_0_discards_tx_side_echo_of_own_send`/
/// `create_functional_no_table_cll`, but under RawMode: `CP_TesterPresentMessage`
/// holds the client's own literal `[CAN ID][payload]` frame instead of a
/// payload-only buffer (Decision item 2), and deliberately no
/// `CP_CanFuncReqId` is staged -- the whole point is proving the fix works
/// without it. The `UniqueRespIdTable` is also left empty, for the same
/// no-table-wildcard reasoning `create_functional_no_table_cll`'s own doc
/// comment explains (ADR-007): with a table, tester-present's own TX CAN ID
/// would never itself be a table entry, so routing would drop the injected
/// frame before it ever reached the discard check under test.
#[tokio::test]
#[serial]
async fn raw_mode_functional_tester_present_discards_own_tx_side_echo_without_can_func_req_id() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            // Deliberately no CP_CAN_FUNC_REQ_ID -- the RawMode client
            // embeds the CAN ID directly in CP_TesterPresentMessage instead.
            (CP_TESTER_PRESENT_ADDR_MODE, 1),
            (CP_TESTER_PRESENT_TIME, 5_000_000),
            (j2534_0404::P2_MAX, 5_000_000),
            (CP_TESTER_PRESENT_HANDLING, 1),
        ],
    )
    .await;
    // RawMode's CP_TesterPresentMessage/cop_data IS the literal
    // PassThruMessage.Data (ADR-196 Decision item 2) -- the CAN ID prefix
    // plus payload, not just the payload the non-RawMode model test uses.
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        can_frame(0x7DF, &[0x3E]),
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // TX_INDICATION | TX_MSG_TYPE echo of tester-present's own send, on its
    // own TX CAN ID (0x7DF, embedded in CP_TesterPresentMessage), empty
    // payload, inside the still-open window.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7DF, &[]),
        j2534_0404::ISO15765,
        0x0000_0009, // TX_INDICATION | TX_MSG_TYPE
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a TX_DONE/loopback echo of a RawMode functional tester-present's \
         own send, with no CP_CanFuncReqId configured, should be discarded \
         -- proving can_functional is now derived from \
         CP_TesterPresentAddrMode even without that ComParam"
    );

    drop(events);
    server.shutdown().await;
}

/// Codex review, PR #107 round 6: `compute_j2534_tx_flags`'s RawMode
/// `TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE` passthrough gates BOTH bits on
/// "CAN-family" alike -- but `tx_header::can_addressing_tx_flags`'s own
/// pre-existing (non-RawMode) invariant already establishes that
/// `ISO15765_ADDR_TYPE` is an ISO15765-specific extended-addressing
/// indicator that must never reach a plain raw-CAN link, unlike
/// `TX_EXTENDED_ID` (11-/29-bit CAN ID width), which genuinely applies to
/// any CAN-family protocol. A RawMode base-CAN CLL setting the client-
/// authoritative `TxFlagIso15765AddrType` bit must still drop it.
#[tokio::test]
#[serial]
async fn raw_mode_can_tx_does_not_leak_iso15765_addr_type_flag() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let mut cop_data = 0x123_u32.to_be_bytes().to_vec();
    cop_data.extend_from_slice(&[0x02, 0x10, 0x03]);
    send_data(
        &mut client,
        cll_handle,
        cop_data.clone(),
        vec![
            TxFlagBit::TxFlagCan29bitId,
            TxFlagBit::TxFlagIso15765AddrType,
        ],
    )
    .await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::TX_EXTENDED_ID,
        "TxFlagCan29bitId applies to any CAN-family link and should still map \
         through, but TxFlagIso15765AddrType must not reach a plain CAN link's \
         native TxFlags"
    );

    server.shutdown().await;
}

/// Codex review, PR #107 round 6: the general SAE J2534-1 TX message
/// size-range check (`hw_protocol.tx_message_size_range(extended_addressing)`)
/// used to derive `extended_addressing` from the ComParam-driven
/// `tx_addressing`, which defaults to `Normal` under RawMode (no reason for
/// a RawMode client to configure `CP_Can*Format`) -- broader than the
/// documented ADR-196 residual (which only covered the 4100-byte upper
/// boundary): a 4-byte CAN-ID-only buffer, missing its required Address
/// Extension byte despite the client's own `TxFlagIso15765AddrType` bit
/// claiming extended addressing, would incorrectly pass the Normal range's
/// `4..=4099` floor instead of being rejected against the Extended range's
/// `5..=4100` floor. This proves the floor is now correctly enforced.
#[tokio::test]
#[serial]
async fn raw_mode_iso15765_extended_addressing_flag_enforces_the_five_byte_tx_floor() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // 4 bytes: CAN ID only, no Address Extension byte -- claims extended
    // addressing via TxFlagIso15765AddrType, but is one byte short of the
    // Extended range's 5-byte floor.
    let too_short = 0x7E0_u32.to_be_bytes().to_vec();
    let status = send_data_expect_rejected_with_flags(
        &mut client,
        cll_handle,
        too_short,
        vec![TxFlagBit::TxFlagIso15765AddrType],
    )
    .await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("TX message size range"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    // 5 bytes: CAN ID + Address Extension byte -- fits the Extended range's
    // floor exactly.
    let at_floor = 0x7E0_u32.to_be_bytes().to_vec();
    let mut at_floor_with_ae = at_floor.clone();
    at_floor_with_ae.push(0xF1);
    send_data(
        &mut client,
        cll_handle,
        at_floor_with_ae.clone(),
        vec![TxFlagBit::TxFlagIso15765AddrType],
    )
    .await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        at_floor_with_ae
    );

    server.shutdown().await;
}

/// Codex review, PR #107 round 7: unlike the non-RawMode path (whose
/// `build_tx_message` always prepends a real header derived from ComParams,
/// so the result is inherently well-formed), RawMode's `resolve_tester_present`
/// returned `CP_TesterPresentMessage` byte-for-byte unchanged with no size
/// validation at all -- ordinary CoptSendrecv sends get a synchronous SAE
/// J2534-1 TX size-range check, but `CoptStartcomm` had no equivalent,
/// letting it arm a message shorter than its own required raw CAN-ID prefix
/// (breaking `tester_present_tx_can_id`'s CAN-ID extraction and risking a
/// malformed native message reaching the adapter on every periodic send).
/// This proves a too-short `CP_TesterPresentMessage` is now rejected
/// synchronously at `CoptStartcomm`, and one at the 4-byte CAN-ID minimum is
/// accepted.
#[tokio::test]
#[serial]
async fn raw_mode_tester_present_rejects_a_message_shorter_than_the_can_id_prefix() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    // 3 bytes: one short of the 4-byte CAN-ID prefix a RawMode base-CAN CLL
    // requires.
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x00, 0x01, 0x02],
    )
    .await;
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
        .expect_err("CoptStartcomm should reject a too-short RawMode CP_TesterPresentMessage");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("CP_TesterPresentMessage"),
        "{}",
        status.message()
    );

    // Exactly 4 bytes (the CAN-ID prefix, no payload) is accepted.
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x00, 0x00, 0x07, 0xDF],
    )
    .await;
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
        .expect("CoptStartcomm should accept a CP_TesterPresentMessage at the 4-byte minimum");

    server.shutdown().await;
}
