//! `CllCreateFlag` RawMode/ChecksumMode support extended to hardware K-line
//! (ADR-198 Phase 2): RawMode=ON is now accepted for `ISO9141`/`ISO14230`
//! (`rpc_link::rpc_create_com_logical_link`'s protocol allowlist, extending
//! ADR-196 Phase 1's CAN/ISO15765-only scope), and ChecksumMode gets its
//! first real (non-no-op) semantics -- the native
//! `CONNECT_FLAG_ISO9141_NO_CHECKSUM` connect flag is set iff `raw_mode &&
//! !checksum_mode` (`rpc_link::connect_flags`), and the SAE J2534-1 §8.3
//! "ISO14230 (Manual Checksum)" widened message-size range applies when
//! that effective bit is set.
//!
//! This service never computes/verifies a K-line checksum itself in any
//! mode -- TX/RX passthrough under RawMode is identical regardless of
//! ChecksumMode's value (the client's own bytes go out, and whatever comes
//! back is delivered whole, unsplit); ChecksumMode only changes the native
//! connect flag (see `connect_flags.rs`) and the message-size bound.
//!
//! `raw_mode.rs` covers the CAN/ISO15765 (Phase 1) baseline and the
//! internal fixed-offset anchors (`RcHandlingConfig`, `classify_queue_error`,
//! `SessionTimingConfig`) this phase's K-line extension reuses generically;
//! `rx_header_split.rs` covers the non-RawMode K-line RX baseline this
//! file's RX tests are contrasted against.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{ComPrimitiveCtrlData, ExpectedResponseData, StartComPrimitiveRequest};

use crate::harness::*;

/// ADR-198 Phase 2: RawMode=ON is now accepted at `CreateComLogicalLink`
/// for both hardware K-line protocols -- Phase 1's protocol allowlist
/// covered only base CAN/hardware ISO15765; `raw_mode.rs`'s own former
/// K-line rejection test is repurposed to J1850 now that K-line itself is
/// allowlisted (K-line's own rejection coverage moved here, as acceptance).
#[tokio::test]
#[serial]
async fn raw_mode_on_is_accepted_for_iso9141_and_iso14230_at_create_time() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _iso9141 = create_cll_raw_mode(&mut client, j2534_0404::ISO9141, 1).await;
    let _iso14230 = create_cll_raw_mode(&mut client, j2534_0404::ISO14230, 2).await;

    server.shutdown().await;
}

/// ADR-198 Phase 2: with ChecksumMode=ON, the client sends header-only
/// bytes (no checksum) and this service transmits them byte-for-byte --
/// `tx_header::build_tx_message`'s unconditional RawMode early return
/// applies regardless of ChecksumMode (this service never synthesizes a
/// checksum in any mode).
#[tokio::test]
#[serial]
async fn checksum_mode_on_tx_sends_header_only_bytes_with_no_synthesized_checksum() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        true,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    // format 0x83 (unaddressed, embedded length 3) + 3 payload bytes -- NO
    // trailing checksum byte.
    let cop_data = vec![0x83, 0x22, 0xF1, 0x90];
    send_data(&mut client, cll_handle, cop_data.clone(), vec![]).await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        cop_data,
        "the client's header-only bytes must reach the wire unchanged, with no checksum \
         byte appended by this service"
    );

    server.shutdown().await;
}

/// ADR-198 Phase 2: with ChecksumMode=ON, an ECU response is delivered
/// whole (no header/footer split, Decision item 3 unaffected by
/// ChecksumMode) -- the interface is trusted to manage the checksum, so
/// this service does not attempt to strip or verify one.
#[tokio::test]
#[serial]
async fn checksum_mode_on_rx_delivers_whole_frame_unsplit() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        true,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let frame = vec![0xC3, 0x62, 0xF1, 0x90];
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO14230);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &[], &[], &frame);

    server.shutdown().await;
}

/// ADR-198 Phase 2: with ChecksumMode=OFF, the client's full frame --
/// header plus its own manually-managed checksum byte -- passes through
/// unchanged on TX, exactly like the ChecksumMode=ON case (this service
/// never touches the checksum byte in either mode; only the native connect
/// flag differs, see `connect_flags.rs`).
#[tokio::test]
#[serial]
async fn checksum_mode_off_tx_sends_full_frame_including_client_checksum_unchanged() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        false,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    // format 0x83 (unaddressed, embedded length 3) + 3 payload bytes + a
    // trailing checksum byte the CLIENT computed and appended itself.
    let cop_data = vec![0x83, 0x22, 0xF1, 0x90, 0x37];
    send_data(&mut client, cll_handle, cop_data.clone(), vec![]).await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        cop_data,
        "the client's own checksum byte must pass through unchanged -- this service never \
         computes or verifies it"
    );

    server.shutdown().await;
}

/// ADR-198 Phase 2: with ChecksumMode=OFF, an ECU response (including its
/// own trailing checksum byte) is delivered whole, unsplit -- identical RX
/// shape to the ChecksumMode=ON case, since Decision item 3's RX split
/// skip is keyed on RawMode alone, not ChecksumMode.
#[tokio::test]
#[serial]
async fn checksum_mode_off_rx_delivers_full_frame_including_checksum_unsplit() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        false,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let frame = vec![0xC3, 0x62, 0xF1, 0x90, 0x37];
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO14230);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &[], &[], &frame);

    server.shutdown().await;
}

/// ADR-198 Phase 2: SAE J2534-1 §8.3 Figure 42's "ISO14230 (Manual
/// Checksum)" row widens the Max Tx bound from 259 to 260 bytes -- a
/// 260-byte message is rejected under ChecksumMode=ON (interface-managed,
/// range stays 1..=259) but accepted under ChecksumMode=OFF (client-managed,
/// range widens to 1..=260).
#[tokio::test]
#[serial]
async fn iso14230_manual_checksum_size_range_widens_max_tx_by_one_byte() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Different baud rates -- separate `ChannelKey`s, separate physical
    // channels (`ChannelKey` includes baud) -- so these two differing
    // effective NO_CHECKSUM CLLs never attempt to share one channel; the
    // join-compatibility check (`iso14230_join_rejects_disagreeing_no_
    // checksum_bit`, `connect_flags.rs`) is exercised separately.
    let checksum_on_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        true,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    let checksum_off_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        false,
        2,
        &[(j2534_0404::DATA_RATE, 4_800)],
    )
    .await;

    let at_260_bytes = vec![0x00u8; 260];

    let status =
        send_data_expect_rejected(&mut client, checksum_on_handle, at_260_bytes.clone()).await;
    assert_eq!(status.code(), Code::InvalidArgument);

    send_data(
        &mut client,
        checksum_off_handle,
        at_260_bytes.clone(),
        vec![],
    )
    .await;
    // The second CLL's own physical channel (different baud -> different
    // `ChannelKey` -> `MOCK_CHANNEL_ID + 1`, the mock's established
    // second-channel convention).
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID + 1), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID + 1, 0),
        at_260_bytes
    );

    server.shutdown().await;
}

/// ADR-198's own "Known risk to verify" (`events.rs`'s
/// `kwp_header_and_payload_len` doc comment): the CARB/ISO9141-2
/// addressing residual (ADR-167) stays dormant under K-line RawMode -- a
/// CARB-addressed (`format & 0xC0 == 0x40`) RawMode frame, TX and RX, is
/// delivered whole and unsplit exactly like any other RawMode frame,
/// despite the format byte's own ambiguity about a trailing checksum
/// byte's presence (`header_footer_len`'s KWP arm is never even consulted
/// for the actual split under RawMode -- only for a classification-only
/// `raw_prefix`, which is fixed at 3 for CARB regardless).
#[tokio::test]
#[serial]
async fn carb_addressed_raw_mode_frame_is_unaffected_by_the_checksum_ambiguity_residual() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO9141,
        false,
        1,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // CARB format 0x6C (0x40 | 0x2C, low bits nonzero and NOT a length
    // field under CARB addressing), target 0x6A, source 0xF1, 3 payload
    // bytes, and a trailing byte that could be a genuine checksum -- or
    // could be a 4th payload byte; CARB's own format byte cannot
    // distinguish the two (ADR-167). Confirms the whole buffer round-trips
    // unchanged either way.
    let mut cop_data = vec![0x6C, 0x6A, 0xF1];
    cop_data.extend_from_slice(&[0x22, 0xF1, 0x90, 0x37]);
    send_data(&mut client, cll_handle, cop_data.clone(), vec![]).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), cop_data);

    let mut frame = vec![0x6C, 0x6A, 0xF1];
    frame.extend_from_slice(&[0x62, 0xF1, 0x90, 0x37]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO9141);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &[], &[], &frame);

    server.shutdown().await;
}

/// A RawMode ISO14230 CLL created via [`create_and_connect_cll_raw_mode`]
/// (ChecksumMode unset -- `CllCreateFlagBits` carries only
/// `CllCreateFlagRawMode`, so it defaults OFF, same as
/// `resolve_cll_create_flag`'s documented `(false, false)` fallback for a
/// wholly-absent flag) resolves ChecksumMode=OFF, so the widened `1..=260`
/// manual-checksum range applies -- 261 bytes is still outside even that.
#[tokio::test]
#[serial]
async fn raw_mode_kline_tx_rejects_cop_data_outside_size_range() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_mode(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    // ChecksumMode defaults OFF when unset by `create_cll_raw_mode`'s own
    // `CllCreateFlagBits` (only `CllCreateFlagRawMode` is set) -- so the
    // effective NO_CHECKSUM bit is 1 and the widened 1..=260 range applies;
    // 261 bytes is still outside even that.
    let too_long = vec![0x00u8; 261];
    let status = send_data_expect_rejected(&mut client, cll_handle, too_long).await;
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// A `StartComPrimitive(CoptStartcomm)` with a SID 0x83 Access Timing
/// Parameter request (ADR-146) at a RawMode=ON K-line CLL: verifies
/// `AccessTimingConfig::with_request`'s `tx_prefix`-anchored SID/TPI
/// capture (ADR-198 Phase 2, extending ADR-196 Decision item 3b) does not
/// crash or misclassify a genuine SID 0x83 request whose bytes sit after
/// the client's own KWP header, by confirming the COP completes and the
/// service applies no unexpected rejection. `service.rs`'s
/// `timing_change_config_with_request_extracts_tpi_at_nonzero_tx_prefix`
/// and `events_bind_frame_tests.rs`'s `observe_timing_response_tpi2_*`/
/// `bind_frame_recognizes_tpi2_response_at_nonzero_raw_prefix` unit tests
/// cover the actual anchor-rebasing logic directly, against the exact
/// production call chain -- this test is the end-to-end confirmation that
/// the whole COP completes normally through the real RPC surface.
#[tokio::test]
#[serial]
async fn raw_mode_kline_access_timing_request_completes_without_misclassification() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_raw_checksum_mode(
        &mut client,
        j2534_0404::ISO14230,
        false,
        1,
        &[(j2534_0404::DATA_RATE, 10_400), (CP_MODIFY_TIMING, 1)],
    )
    .await;

    // CARB-addressed 3-byte header (format, target, source) ahead of a
    // genuine SID 0x83 TPI=2 (read active values) request -- tx_prefix
    // resolves to 3 for this frame shape.
    let cop_data = vec![0x40, 0x6A, 0xF1, 0x83, 0x02];
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
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect(
            "a genuine SID 0x83 Access Timing Parameter request at a RawMode K-line CLL must \
             be accepted and complete, not panic or be misclassified",
        );

    server.shutdown().await;
}
