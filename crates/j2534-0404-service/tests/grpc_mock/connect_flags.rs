//! `PassThruConnect` `Flags` derivation from the creator CLL's Working
//! ComParam set and `UniqueRespIdTable` (ADR-065): raw `CAN` always
//! `CAN_ID_BOTH` (widths are unknowable at connect time on a channel shared
//! by `(CAN, baud)` alone) with `CAN_29BIT_ID` as the priority bit from the
//! physical-request format; `ISO15765` derives `CAN_29BIT_ID`/`CAN_ID_BOTH`
//! from Table B.13 `CP_Can*Format` across the configured addresses; and
//! `ISO9141_K_LINE_ONLY` for ISO9141/ISO14230 from `CP_K_L_LineInit`; and
//! `ISO9141_NO_CHECKSUM` for a RawMode=ON/ChecksumMode=OFF ISO9141/ISO14230
//! CLL (ADR-198 Phase 2), including the shared-channel join-compatibility
//! check for two K-line CLLs with disagreeing effective values of that bit.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::ConnectComLogicalLinkRequest;

use crate::harness::*;

/// Default Working set (no `UniqueRespIdTable`, no `CP_Can*Format` set): no
/// CAN-ID-type observation exists, so the connect `Flags` stay `0`.
#[tokio::test]
#[serial]
async fn iso15765_default_connect_flags_are_zero() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    assert_eq!(server.backdoor.connect_flags_log(), vec![0]);

    server.shutdown().await;
}

/// A `UniqueRespIdTable` entry configuring 29-bit `CP_CanPhysReqId`/
/// `CP_CanRespUSDTId` addresses, alongside the link-level `CP_CanFuncReqId`'s
/// 11-bit `CP_CanFuncReqFormat` (always consulted, per-entry or not), makes
/// the observed CAN-ID types disagree: `CAN_ID_BOTH` (bit 11) is set, with
/// `CAN_29BIT_ID` (bit 8) as the priority bit because the physical-request
/// address (the priority source) is the 29-bit one. Expected Flags:
/// `0x900` = `CAN_ID_BOTH` (0x800) | `CAN_29BIT_ID` (0x100).
#[tokio::test]
#[serial]
async fn iso15765_mixed_can_id_format_sets_can_id_both_with_29bit_priority() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    // Link-level functional-request format: Table B.13 bit 1 clear (11-bit).
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_FORMAT, 0x05).await;

    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x18DAF101),
                // Table B.13 bit 1 set: 29-bit CAN Id.
                unum32_param(CP_CAN_RESP_USDT_FORMAT, 0x02),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x18DA01F1),
                unum32_param(CP_CAN_PHYS_REQ_FORMAT, 0x02),
            ],
        )],
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // 0x900 = CAN_ID_BOTH (0x800) | CAN_29BIT_ID (0x100)
    assert_eq!(server.backdoor.connect_flags_log(), vec![0x900]);

    server.shutdown().await;
}

/// Raw `CAN` (not `ISO15765`) always connects `CAN_ID_BOTH` regardless of any
/// observed CAN-ID width, since raw-CAN channels are shared by `(CAN, baud)`
/// alone and the set of widths they will ever carry is unknowable at connect
/// time (ADR-065). With no addressing configured, the priority bit
/// (`CAN_29BIT_ID`) defaults to 11-bit. Expected Flags: `0x800` = `CAN_ID_BOTH`.
#[tokio::test]
#[serial]
async fn raw_can_default_connect_flags_are_can_id_both() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // 0x800 = CAN_ID_BOTH
    assert_eq!(server.backdoor.connect_flags_log(), vec![0x800]);

    server.shutdown().await;
}

/// Raw `CAN` still always connects `CAN_ID_BOTH`, but the priority bit
/// (`CAN_29BIT_ID`) follows the physical-request address's format: a
/// `UniqueRespIdTable` entry with a 29-bit `CP_CanPhysReqFormat` on its
/// `CP_CanPhysReqId` makes the physical-request address 29-bit. Expected
/// Flags: `0x900` = `CAN_ID_BOTH` (0x800) | `CAN_29BIT_ID` (0x100).
#[tokio::test]
#[serial]
async fn raw_can_29bit_phys_req_sets_can_id_both_with_29bit_priority() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x18DA01F1),
                // Table B.13 bit 1 set: 29-bit CAN Id.
                unum32_param(CP_CAN_PHYS_REQ_FORMAT, 0x02),
            ],
        )],
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // 0x900 = CAN_ID_BOTH (0x800) | CAN_29BIT_ID (0x100)
    assert_eq!(server.backdoor.connect_flags_log(), vec![0x900]);

    server.shutdown().await;
}

/// `CP_K_L_LineInit = 1`, set before connect, sets `ISO9141_K_LINE_ONLY`
/// (0x1000) in the connect `Flags` for an ISO14230 channel.
#[tokio::test]
#[serial]
async fn iso14230_k_line_init_sets_k_line_only_flag() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_K_L_LINE_INIT, 1).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // 0x1000 = ISO9141_K_LINE_ONLY
    assert_eq!(server.backdoor.connect_flags_log(), vec![0x1000]);

    server.shutdown().await;
}

/// Without `CP_K_L_LineInit` set (the `0` default), the ISO14230 connect
/// `Flags` stay `0` -- K and L line, matching the service's default.
#[tokio::test]
#[serial]
async fn iso14230_default_k_line_init_leaves_connect_flags_zero() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    assert_eq!(server.backdoor.connect_flags_log(), vec![0]);

    server.shutdown().await;
}

/// ADR-198 Phase 2: a RawMode=ON/ChecksumMode=OFF ISO14230 CLL sets
/// `ISO9141_NO_CHECKSUM` (0x200) in the connect `Flags` -- the client
/// manages its own checksum, so the interface is told to leave it alone.
#[tokio::test]
#[serial]
async fn iso14230_raw_mode_checksum_mode_off_sets_no_checksum_flag() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        false,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    // 0x200 = ISO9141_NO_CHECKSUM
    assert_eq!(server.backdoor.connect_flags_log(), vec![0x200]);

    server.shutdown().await;
}

/// ADR-198 Phase 2: a RawMode=ON/ChecksumMode=ON ISO14230 CLL does NOT set
/// `ISO9141_NO_CHECKSUM` -- Table D.6's ChecksumMode=ON semantics mean the
/// D-PDU API/vendor interface still manages the checksum even under
/// RawMode, so the interface must still be told to handle it, same as
/// RawMode=OFF.
#[tokio::test]
#[serial]
async fn iso14230_raw_mode_checksum_mode_on_leaves_no_checksum_flag_unset() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        true,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    assert_eq!(server.backdoor.connect_flags_log(), vec![0]);

    server.shutdown().await;
}

/// ADR-198 Phase 2: `ISO9141_NO_CHECKSUM` (0x200) ORs together with
/// `ISO9141_K_LINE_ONLY` (0x1000) when both apply to the same connect --
/// confirms the two bits are independently derived, not mutually
/// exclusive branches of the same match arm.
#[tokio::test]
#[serial]
async fn iso14230_raw_mode_no_checksum_ors_with_k_line_only_flag() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle =
        create_cll_raw_checksum_mode(&mut client, j2534_0404::ISO14230, 1, false).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_K_L_LINE_INIT, 1).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // 0x1200 = ISO9141_K_LINE_ONLY (0x1000) | ISO9141_NO_CHECKSUM (0x200)
    assert_eq!(server.backdoor.connect_flags_log(), vec![0x1200]);

    server.shutdown().await;
}

/// ADR-198 Phase 2: two ISO14230 CLLs on the same physical channel (same
/// baud, so the same `ChannelKey`) with disagreeing effective
/// `ISO9141_NO_CHECKSUM` bits -- the second `ConnectComLogicalLink` is
/// rejected by the shared-channel join-compatibility check, mirroring the
/// Ethernet_NDIS pin-option join guard's own rejection shape.
#[tokio::test]
#[serial]
async fn iso14230_join_rejects_disagreeing_no_checksum_bit() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // First CLL: RawMode=ON, ChecksumMode=OFF -- effective NO_CHECKSUM = 1.
    let _first = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        false,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    // Second CLL: RawMode=OFF -- effective NO_CHECKSUM = 0, disagreeing
    // with the first CLL's already-open physical channel.
    let second_handle = create_cll(&mut client, j2534_0404::ISO14230, 2).await;
    set_com_param_unum32(&mut client, second_handle, j2534_0404::DATA_RATE, 10_400).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(second_handle),
        })
        .await
        .expect_err(
            "a RawMode=OFF CLL must not be able to join a channel whose effective \
             ISO9141_NO_CHECKSUM bit is 1",
        );

    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(status.message().contains("ISO9141_NO_CHECKSUM"));

    server.shutdown().await;
}
