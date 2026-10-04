//! `CllCreateFlag` RawMode extended to SAE J1850 (VPW/PWM), ADR-200 Phase 3:
//! RawMode=ON is now accepted at `CreateComLogicalLink` for `J1850VPW`/
//! `J1850PWM` (`rpc_link::rpc_create_com_logical_link`'s protocol allowlist),
//! via the SAME generic RawMode passthrough CAN/ISO15765/K-line already use
//! (`tx_header::build_tx_message`'s unconditional early return needs no
//! J1850-specific code -- unlike SAE J1939, see `j1939_raw_mode.rs`) --
//! **with a new constraint**: RawMode=ON on a J1850 CLL additionally
//! requires ChecksumMode=ON, since SAE J2534-1 v04.04 has no J1850
//! CRC-suppression connect flag (unlike `ISO9141_NO_CHECKSUM` for K-line) --
//! the interface always computes/verifies/strips J1850's CRC regardless of
//! ChecksumMode, so ChecksumMode=OFF cannot be honestly honored and is
//! rejected with a distinct error from the general "protocol doesn't support
//! RawMode at all" rejection.
//!
//! `raw_mode.rs` covers the CAN/ISO15765 (Phase 1) baseline this file's RX
//! test is contrasted against, and now also carries the TP2.0
//! permanent-exclusion test (this file's own former "every other protocol"
//! representative before ADR-200 allowlisted J1850). `kline_raw_mode.rs`
//! covers K-line's own analogous ChecksumMode mechanism (a real
//! interface-managed/client-managed choice, unlike J1850's forced ON).
//! `j1850_autodetect.rs` covers the non-RawMode J1850 baseline this ADR
//! leaves byte-for-byte unaffected (only the RawMode=ON branch is new code).

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    CllCreateFlag, CllCreateFlagBit, ModuleHandle, StartComPrimitiveRequest,
};

use crate::harness::*;

/// ADR-200: RawMode=ON + ChecksumMode=ON is now accepted at
/// `CreateComLogicalLink` for both J1850VPW and J1850PWM.
#[tokio::test]
#[serial]
async fn raw_mode_on_with_checksum_mode_on_is_accepted_for_j1850vpw_and_pwm_at_create_time() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _vpw = create_cll_raw_checksum_mode(&mut client, j2534_0404::J1850VPW, 1, true).await;
    let _pwm = create_cll_raw_checksum_mode(&mut client, j2534_0404::J1850PWM, 2, true).await;

    server.shutdown().await;
}

/// ADR-200: RawMode=ON + ChecksumMode=OFF is REJECTED for J1850, with a
/// distinct message from the general protocol-allowlist rejection -- RawMode
/// itself IS supported for J1850 (unlike, say, TP2.0); this specific
/// ChecksumMode combination is not, because v04.04 gives J1850 no
/// CRC-suppression connect flag to honor ChecksumMode=OFF with.
#[tokio::test]
#[serial]
async fn raw_mode_on_with_checksum_mode_off_is_rejected_at_create_time() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(vci_service_interface::CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(vci_service_interface::create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(j2534_0404::J1850VPW),
            )),
            cll_create_flag: Some(
                vci_service_interface::create_com_logical_link_request::CllCreateFlag::CllCreateFlagBits(
                    CllCreateFlag {
                        bits: vec![CllCreateFlagBit::CllCreateFlagRawMode as i32],
                    },
                ),
            ),
        })
        .await
        .expect_err("RawMode=ON + ChecksumMode=OFF on J1850VPW should be rejected");

    assert_eq!(status.code(), Code::Unimplemented);
    assert!(status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));
    assert!(
        status.message().contains("ChecksumMode"),
        "the rejection must name ChecksumMode specifically, distinguishing it from the \
         general protocol-allowlist rejection: {}",
        status.message()
    );

    server.shutdown().await;
}

/// ADR-200: with RawMode=ON/ChecksumMode=ON, the client's own raw J1850
/// frame (3-byte header the client itself constructed, per
/// `tx_header::j1850_header_bytes`'s own shape) reaches the wire unchanged --
/// `build_tx_message`'s existing, protocol-agnostic RawMode early return
/// needs no J1850-specific code.
#[tokio::test]
#[serial]
async fn raw_mode_j1850_tx_sends_client_constructed_payload_unchanged() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::J1850VPW,
        true,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    // Client-constructed J1850 frame: format 0x68 (J1850VPW default),
    // target 0x6A, source 0xF1, followed by payload -- no service-side
    // header resolution at all (no CP_HeaderFormatJ1850/UniqueRespIdTable
    // staged).
    let cop_data = vec![0x68, 0x6A, 0xF1, 0x02, 0x10, 0x03];
    send_data(&mut client, cll_handle, cop_data.clone(), vec![]).await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), cop_data);

    server.shutdown().await;
}

/// ADR-200: a RawMode=ON J1850 CLL's received frame is delivered whole --
/// `data_bytes` carries the 3-byte header AND the payload together,
/// `extra_info` absent -- the same "generic RawMode passthrough" shape
/// CAN/ISO15765/K-line already get, mirroring `raw_mode.rs`'s own CAN RX
/// test.
#[tokio::test]
#[serial]
async fn raw_mode_j1850_rx_delivers_full_frame_with_no_header_split() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::J1850VPW,
        true,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let frame = vec![0x68, 0x6A, 0xF1, 0x01, 0x02, 0x03];
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::J1850VPW);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &[], &[], &frame);

    server.shutdown().await;
}

/// `edge-case-hunter` finding (ADR-215 review round): the general SAE
/// J2534-1 composed-message size-range check ADR-215 adds to
/// `resolve_tester_present` (`sae_tx_size_range`) is only ever tested
/// non-RawMode elsewhere -- but it applies uniformly regardless of RawMode
/// (mirroring `resolve_send_recv_tx`'s own unconditional-of-`raw_mode`
/// application), and J1850PWM's own SAE ceiling (`3..=10` composed) sits
/// BELOW the ISO 22900-2 `ParamMaxLen = 12` cap `SetComParam` alone enforces
/// -- so an 11-byte RawMode `CP_TesterPresentMessage` on J1850PWM is
/// ISO-22900-2-legal at `SetComParam` time but must still be rejected at
/// `CoptStartcomm` by the newer, narrower SAE check. This is the RawMode
/// analogue of `tester_present_message_length.rs`'s own non-RawMode
/// `j1850pwm_tester_present_composed_message_over_size_range_is_rejected_at_startcomm`.
#[tokio::test]
#[serial]
async fn raw_mode_j1850pwm_tester_present_over_sae_ceiling_is_rejected_at_startcomm() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::J1850PWM,
        true,
        1,
        &[(j2534_0404::DATA_RATE, 41_600)],
    )
    .await;

    // 11 bytes: within ISO 22900-2's 12-byte ParamMaxLen (accepted by
    // SetComParam), but over J1850PWM's own SAE J2534-1 `3..=10` composed
    // ceiling (RawMode's own message IS the composed message -- no service-
    // side header is prepended).
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3Eu8; 11],
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
            "an 11-byte RawMode J1850PWM tester-present message exceeds the 10-byte SAE \
             J2534-1 ceiling, even though it is within ISO 22900-2's 12-byte ParamMaxLen",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("3..=10"), "{}", status.message());
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    server.shutdown().await;
}
