//! ADR-051: received frames are delivered with `ResultData.data_bytes`
//! payload-only — the per-protocol header (CAN ID, KWP2000 format bytes,
//! J1850 format/target/source) and any trailing checksum/CRC are split out
//! into `ResultData.extra_info` instead.
//!
//! Also covers ADR-097 (superseded by ADR-098): `START_OF_MESSAGE` (`RxStatus`
//! bit 1) frames route through this same header/footer split — header-only
//! for ISO15765, wholly empty for ISO9141/ISO14230 — and additionally
//! populate `ResultData.rx_flag`. Includes both native-ISO15765 and
//! `software-isotp`-mode (`can_channel_mode`, ADR-046) CLLs.
//!
//! ADR-098 extends the same `rx_flag` treatment to all 5 low `RxStatus` bits
//! (`TX_MSG_TYPE`, `START_OF_MESSAGE`, `RX_BREAK`, `TX_INDICATION`,
//! `ISO15765_PADDING_ERROR`), unconditionally bit-copied into `rx_flag` byte
//! 3 -- see the tests below for TxDone, Loopback, RxBreak, and RxPadError.

use serial_test::serial;

use crate::harness::*;

/// ADR-051: CAN's ResultData.data_bytes is payload-only too (not just
/// ISO15765) — the raw 4-byte CAN ID is split into `extra_info.header_bytes`.
/// No UniqueRespIdTable is configured, so routing falls back to
/// unconditional delivery (unique_resp_identifier 0); the split still
/// applies since it is keyed on the CLL's protocol, not on the table. A
/// receive-only monitor COP (ADR-100 Decision §5, S8) is armed so the
/// injected frame still binds to something and is delivered instead of
/// discarded as unbound -- this test is about the header/footer split, not
/// attribution, so the monitor's own vacuous match keeps that split
/// observable exactly as before ADR-100. `CP_RequestAddrMode`/
/// `CP_CanFuncReqId` give the monitor COP (never actually transmitted,
/// `NumSendCycles == 0`) resolvable TX addressing without requiring a
/// UniqueRespIdTable entry, preserving this test's own "no table" premise
/// for RX routing (see `arm_receive_only_monitor`'s own doc comment).
#[tokio::test]
#[serial]
async fn can_protocol_splits_can_id_header_into_extra_info() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x01, 0x02, 0x03, 0x04];
    let mut frame = 0x123_u32.to_be_bytes().to_vec();
    frame.extend_from_slice(&payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload);

    server.shutdown().await;
}

/// ADR-051: on a hardware (non-software-ISO-TP) ISO15765 channel, a response
/// whose matched UniqueRespIdTable entry uses extended addressing widens the
/// split header to 5 bytes (4-byte CAN ID + 1 Address Extension byte) — the
/// AE byte is still embedded in the raw frame the mock delivers here, unlike
/// the software-ISO-TP reassembly case
/// (`software_isotp_mode_extended_addressing_segments_and_reassembles`),
/// where it is already consumed during reassembly and the header stays 4
/// bytes.
///
/// ADR-197 regression check: this frame is injected via the plain `inject_rx`
/// helper, i.e. native `RxStatus` bit 7 (`ISO15765_ADDR_TYPE_STATUS`) is
/// clear -- this pins that the `usdt_addressing_by_id` table lookup (ADR-217
/// Codex-review fix: role-split from the original single `can_addressing_by_id`)
/// alone still correctly widens the header when the RxStatus signal is
/// absent, so ADR-197's added `rx_ext_addr ||` term does not regress the
/// pre-existing table-only-signals-extended case.
#[tokio::test]
#[serial]
async fn iso15765_hardware_extended_addressing_widens_rx_header_to_five_bytes() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    const RESP_EXT_ADDR: u8 = 0xF1;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            9,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_USDT_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_RESP_USDT_EXT_ADDR, RESP_EXT_ADDR as u32),
            ],
        )],
    )
    .await;
    // This test's own UniqueRespIdTable entry (above) has no
    // CP_CanPhysReqId, so the receive-only monitor COP below (never
    // actually transmitted) instead needs functional addressing to resolve
    // (ADR-100 Decision §5, S8 -- see `arm_receive_only_monitor`'s own doc
    // comment). Set/promoted after connect and after the table, via
    // CoptUpdateparam, so it cannot perturb ConnectComLogicalLink's own
    // flags/filter derivation (ADR-065) -- this test is about the RX
    // header/footer split, not connect-time behavior.
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    promote_via_update_param(&mut client, cll_handle).await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x62, 0xF1, 0x90];
    let mut frame = 0x7E8_u32.to_be_bytes().to_vec();
    frame.push(RESP_EXT_ADDR);
    frame.extend_from_slice(&payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO15765);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    let mut expected_header = 0x7E8_u32.to_be_bytes().to_vec();
    expected_header.push(RESP_EXT_ADDR);
    assert_result_data(&result, &expected_header, &[], &payload);
    assert_eq!(result.unique_resp_identifier, 9);

    server.shutdown().await;
}

/// ADR-197: a "no-table wildcard" ISO15765 CLL -- no `SetUniqueRespIdTable`
/// entry at all, so `header_footer_len`'s addressing-table lookup (whichever
/// of `usdt_addressing_by_id`/`uudt_addressing_by_id` the delivery selects,
/// ADR-217 Codex-review fix) structurally cannot detect extended addressing
/// -- still correctly widens
/// the split header to 5 bytes when the injected frame's own native
/// `RxStatus` bit 7 (`ISO15765_ADDR_TYPE_STATUS`) is set. Before ADR-197's
/// fix, this CLL always fell back to a 4-byte header, leaving the Address
/// Extension byte in `data_bytes[0]` instead of `extra_info.header_bytes`.
#[tokio::test]
#[serial]
async fn iso15765_no_table_wildcard_rx_status_bit_widens_header_to_five_bytes() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            // No UniqueRespIdTable entry for the responding CAN ID below --
            // the receive-only monitor COP needs functional addressing to
            // resolve instead (ADR-100 Decision §5, S8).
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    const RESP_EXT_ADDR: u8 = 0xF1;
    let payload = vec![0x62, 0xF1, 0x90];
    let mut frame = 0x7E8_u32.to_be_bytes().to_vec();
    frame.push(RESP_EXT_ADDR);
    frame.extend_from_slice(&payload);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::ISO15765,
        j2534_0404::ISO15765_ADDR_TYPE_STATUS,
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    let mut expected_header = 0x7E8_u32.to_be_bytes().to_vec();
    expected_header.push(RESP_EXT_ADDR);
    assert_result_data(&result, &expected_header, &[], &payload);

    server.shutdown().await;
}

/// Builds a `RscData` selecting `protocol_id` via the unambiguous
/// `ProtocolId` route with explicit `dlc_pin_data` (SAE J2534-2 clause 6 Pin
/// Selection), mirroring `pin_selection.rs`'s own private
/// `resource_with_protocol_id_and_pins` helper -- duplicated locally rather
/// than shared, since that helper isn't `pub(crate)` and this is this file's
/// only Pin-Selection test.
fn resource_with_protocol_id_and_pins(
    protocol_id: u32,
    pins: &[(u32, &str)],
) -> vci_service_interface::ResourceData {
    vci_service_interface::ResourceData {
        dlc_pin_data: pins
            .iter()
            .map(|&(number, type_name)| vci_service_interface::PinData {
                dlc_pin_number: number,
                dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                    type_name.to_string(),
                )),
            })
            .collect(),
        bus_type: None,
        protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
            protocol_id,
        )),
    }
}

/// Creates and connects a pin-selected `_PS` CLL on `module_handle`,
/// staging `params` (e.g. `DATA_RATE`) before `ConnectComLogicalLink` --
/// the Pin-Selection counterpart to `create_and_connect_cll_for_module`,
/// which always passes empty `dlc_pin_data` and so can never resolve to a
/// `_PS` hardware protocol id.
async fn create_and_connect_ps_cll(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    module_handle: u32,
    protocol_id: u32,
    pins: &[(u32, &str)],
    params: &[(u32, u32)],
) -> vci_service_interface::ComLogicalLinkHandle {
    let cll_handle = client
        .create_com_logical_link(vci_service_interface::CreateComLogicalLinkRequest {
            module_handle: Some(vci_service_interface::ModuleHandle { module_handle }),
            resource: Some(
                vci_service_interface::create_com_logical_link_request::Resource::RscData(
                    resource_with_protocol_id_and_pins(protocol_id, pins),
                ),
            ),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should resolve non-default pins to a _PS hardware id")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    for &(com_param_id, value) in params {
        set_com_param_unum32(client, cll_handle, com_param_id, value).await;
    }

    client
        .connect_com_logical_link(vci_service_interface::ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for a pin-selected _PS link");

    cll_handle
}

/// Design-advisor fix (ADR-171 follow-up Decision item): `CllRxEntry::
/// header_protocol` now derives from `resources::base_protocol_id(l.
/// hw_protocol_id)` for a `Hardware`/`Companion` entry, instead of the old
/// blanket `l.protocol.j2534_protocol_id()` read. `base_protocol_id`'s
/// `_PS`/`_CHx` normalization is exactly what lets a pin-selected
/// `ISO15765_PS` link's raw hardware frame still be recognized as
/// `j2534_0404::ISO15765` by `header_footer_len`'s numeric match -- pinning
/// this specifically guards against a future accidental simplification to
/// raw `l.hw_protocol_id` (without normalization), which would silently
/// regress this case to the `header_footer_len` catch-all (`(0, 0)`,
/// deliver the whole frame as payload with no header split at all), since
/// `PROTOCOL_ISO15765_PS` never equals the literal `ISO15765` constant.
#[tokio::test]
#[serial]
async fn iso15765_ps_qualified_link_still_splits_can_id_header_on_rx() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_ps_cll(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[(3, "HI"), (11, "LOW")],
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x62, 0xF1, 0x90];
    let mut frame = 0x7E8_u32.to_be_bytes().to_vec();
    frame.extend_from_slice(&payload);
    let channel_id = server.backdoor.connect_count() as u32;
    server
        .backdoor
        .inject_rx(channel_id, &frame, j2534_0404::ISO15765);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x7E8_u32.to_be_bytes(), &[], &payload);

    server.shutdown().await;
}

/// ADR-051: ISO14230 (KWP2000) responses can use any of the standard's 1-4
/// byte header encodings — this service always sends the 4-byte "physical
/// addressing, length in a separate byte" variant on TX (ADR-050,
/// `tx_header::kwp_header_bytes`, format `0x80`), but an ECU's response is
/// free to use a shorter one, so the header length is parsed from each
/// response's own format byte rather than assumed fixed.
#[tokio::test]
#[serial]
async fn iso14230_protocol_parses_variable_length_kwp_header_on_rx() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps every injected
    // frame below delivered under the new unbound-discard model.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // 4-byte header: format 0x80 (addressed, separate length byte follows)
    // + target + source + length byte.
    let payload_a = vec![0x62, 0xF1, 0x90];
    let mut frame_a = vec![0x80, 0x10, 0xF1, payload_a.len() as u8];
    frame_a.extend_from_slice(&payload_a);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_a, j2534_0404::ISO14230);
    let result_a = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result_a, &frame_a[..4], &[], &payload_a);

    // 1-byte header: format 0x03 (no addressing; length embedded in the
    // format byte's low 6 bits).
    let payload_b = vec![0x41, 0x42, 0x43];
    let mut frame_b = vec![0x03];
    frame_b.extend_from_slice(&payload_b);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_b, j2534_0404::ISO14230);
    let result_b = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result_b, &frame_b[..1], &[], &payload_b);

    // 3-byte header: format 0x83 (addressed; length embedded in the low 6
    // bits) + target + source.
    let payload_c = vec![0x51, 0x52, 0x53];
    let mut frame_c = vec![0x83, 0x10, 0xF1];
    frame_c.extend_from_slice(&payload_c);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_c, j2534_0404::ISO14230);
    let result_c = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result_c, &frame_c[..3], &[], &payload_c);

    // 2-byte header: format 0x00 (no addressing, separate length byte
    // follows).
    let payload_d = vec![0x61, 0x62];
    let mut frame_d = vec![0x00, payload_d.len() as u8];
    frame_d.extend_from_slice(&payload_d);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_d, j2534_0404::ISO14230);
    let result_d = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result_d, &frame_d[..2], &[], &payload_d);

    // 4-byte header plus a trailing 1-byte checksum: the header's own
    // declared length (3) leaves one byte unaccounted for at the end of
    // the frame, which is reported as `extra_info.footer_bytes` rather
    // than being appended to `data_bytes`.
    let payload_e = vec![0x62, 0xF1, 0x90];
    let checksum_e = 0x37u8;
    let mut frame_e = vec![0x80, 0x10, 0xF1, payload_e.len() as u8];
    frame_e.extend_from_slice(&payload_e);
    frame_e.push(checksum_e);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_e, j2534_0404::ISO14230);
    let result_e = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result_e, &frame_e[..4], &[checksum_e], &payload_e);

    server.shutdown().await;
}

/// ADR-051: ISO9141 shares ISO14230's KWP2000 header/footer handling in
/// this service (both use `tx_header::kwp_header_bytes` on TX) — a response
/// with a 4-byte header (format 0x80: addressed, separate length byte) and
/// a trailing checksum byte is split the same way.
#[tokio::test]
#[serial]
async fn iso9141_protocol_splits_kwp_header_and_checksum_footer_on_rx() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x61, 0x00, 0x01];
    let checksum = 0x42u8;
    let mut frame = vec![0x80, 0x10, 0xF1, payload.len() as u8];
    frame.extend_from_slice(&payload);
    frame.push(checksum);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO9141);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &frame[..4], &[checksum], &payload);

    server.shutdown().await;
}

/// ADR-167: a CARB/ISO9141-2 exception-addressing response (format byte
/// `format & 0xC0 == 0x40`) is split with a fixed 3-byte header (format,
/// target, source) and the entire remainder as payload -- no footer is ever
/// reported for this address mode, unlike the two self-describing KWP
/// encodings covered above. Exercised with two different qualifying format
/// bytes: `0x40` (the "plain" CARB case) and `0x6C` (whose low 6 bits,
/// `0x2C`, are nonzero and would have been misread by the pre-ADR-167 code
/// as an embedded-length addressed/unaddressed header of length 1 -- this
/// confirms the CARB branch is checked before that older bit-7 logic, not
/// after it).
#[tokio::test]
#[serial]
async fn iso9141_carb_address_mode_treats_entire_remainder_as_payload_on_rx() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload_a = vec![0x61, 0x00, 0x01, 0x02];
    let mut frame_a = vec![0x40, 0x10, 0xF1];
    frame_a.extend_from_slice(&payload_a);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_a, j2534_0404::ISO9141);
    let result_a = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result_a, &frame_a[..3], &[], &payload_a);

    let payload_b = vec![0x51, 0x52, 0x53];
    let mut frame_b = vec![0x6C, 0x10, 0xF1];
    frame_b.extend_from_slice(&payload_b);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_b, j2534_0404::ISO9141);
    let result_b = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result_b, &frame_b[..3], &[], &payload_b);

    server.shutdown().await;
}

/// ADR-167: a CARB frame that is exactly the fixed 3-byte header long (no
/// payload bytes at all) is still split as header-only, not misreported as a
/// too-short degenerate frame -- `data.len() - 3 == 0` is a valid, zero-length
/// payload, distinct from the "frame shorter than 3 bytes" case below.
#[tokio::test]
#[serial]
async fn iso9141_carb_address_mode_exactly_three_byte_frame_has_empty_payload_and_footer() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let frame = vec![0x40, 0x10, 0xF1];
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO9141);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &frame, &[], &[]);

    server.shutdown().await;
}

/// ADR-167: a CARB-format frame shorter than the fixed 3-byte header (only 2
/// bytes) cannot even contain its own header -- `kwp_header_and_payload_len`
/// returns `None`, and `header_footer_len`'s existing too-short fallback
/// reports the whole frame as header with no payload/footer split, same as
/// it already does for the other KWP encodings' too-short cases.
#[tokio::test]
#[serial]
async fn iso9141_carb_address_mode_frame_shorter_than_header_falls_back_to_whole_frame_as_header() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let frame = vec![0x40, 0x10]; // 2 bytes: shorter than the 3-byte CARB header
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO9141);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &frame, &[], &[]);

    server.shutdown().await;
}

/// ADR-051: J1850 (PWM/VPW) splits its fixed 3-byte header
/// (format/priority + target + source) and, when present, a trailing
/// zero-length footer into `extra_info` (ADR-171): J1850's wire checksum is
/// already verified and stripped by the interface before delivery, so a
/// received frame with no reported IFR bytes (the mock's default
/// `ExtraDataIndex == DataSize`, per `inject_rx`'s plain five-argument
/// symbol) is entirely header + payload — no fabricated 1-byte CRC footer.
/// This regression-tests the bug ADR-171 fixes: the last byte here used to
/// be wrongly stripped into `footer_bytes` by the old fixed-1-byte-CRC
/// logic.
#[tokio::test]
#[serial]
async fn j1850_protocol_delivers_full_payload_with_no_ifr_bytes_on_rx() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850VPW,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // Header (format/target/source) + genuine payload, with no separate
    // "CRC" byte -- the byte the old buggy code stripped (0x99) is now
    // correctly part of the payload.
    let frame = vec![0x68, 0x10, 0xF1, 0x41, 0x00, 0x99];
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::J1850VPW);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &frame[..3], &[], &frame[3..]);

    server.shutdown().await;
}

/// ADR-171: a J1850 PWM response reporting genuine trailing IFR bytes (via
/// an explicit `ExtraDataIndex` less than `DataSize`) delivers them in
/// `footer_bytes`, distinct from the header and payload. Two trailing bytes
/// (not one) deliberately rules out any "still secretly assumes 1 byte"
/// regression in the new `ExtraDataIndex`-derived footer logic.
#[tokio::test]
#[serial]
async fn j1850pwm_protocol_splits_genuine_ifr_footer_on_rx() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850PWM,
        &[(j2534_0404::DATA_RATE, 41_600)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let header = vec![0x61, 0x6B, 0x10];
    let payload = vec![0x41, 0x00];
    let ifr = vec![0xAA, 0xBB];
    let mut frame = header.clone();
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&ifr);
    let extra_data_index = (frame.len() - ifr.len()) as u32;
    server.backdoor.inject_rx_with_edi(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::J1850PWM,
        0,
        extra_data_index,
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &header, &ifr, &payload);

    server.shutdown().await;
}

/// ADR-171 point 3: `ExtraDataIndex` is an untrusted, FFI-crossing value —
/// a device (or, here, the mock) reporting one outside `header..=len` must
/// not corrupt the split. This injects `ExtraDataIndex > DataSize` (the mock
/// permits it by design so this is exercisable) and asserts
/// `header_footer_len`'s defensive clamp degrades to "no footer, everything
/// is payload" rather than propagating the out-of-range value.
///
/// Codex review round 2 (PR #58): must use PWM, not VPW -- after the round-1
/// fix split the shared match arm, VPW unconditionally ignores
/// `ExtraDataIndex` regardless of range, so a VPW version of this test would
/// pass vacuously even if the clamp itself were broken or removed from the
/// PWM branch it's meant to exercise.
#[tokio::test]
#[serial]
async fn j1850pwm_protocol_clamps_out_of_range_extra_data_index_on_rx() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850PWM,
        &[(j2534_0404::DATA_RATE, 41_600)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let frame = vec![0x61, 0x6B, 0x10, 0x41, 0x00];
    let out_of_range_edi = (frame.len() + 1) as u32;
    server.backdoor.inject_rx_with_edi(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::J1850PWM,
        0,
        out_of_range_edi,
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &frame[..3], &[], &frame[3..]);

    server.shutdown().await;
}

/// Codex review round 1 (PR #58): VPW has no IFR mechanism at all, so a
/// non-conforming or stale adapter reporting an in-range (not just
/// out-of-range) `ExtraDataIndex` on a VPW response must still be ignored --
/// unlike PWM, VPW never derives a footer from it. Without this
/// discrimination, an in-range-but-wrong value would silently reintroduce
/// the exact payload-truncation bug ADR-171 fixes, just sourced from
/// `ExtraDataIndex` instead of a fabricated 1-byte CRC.
#[tokio::test]
#[serial]
async fn j1850vpw_protocol_ignores_in_range_extra_data_index_on_rx() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850VPW,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // A stale/non-conforming ExtraDataIndex that is in-range (unlike the
    // out-of-range clamp test above) and would, if honored, wrongly move the
    // last real payload byte into footer_bytes.
    let frame = vec![0x68, 0x10, 0xF1, 0x41, 0x00, 0xBE];
    let in_range_but_wrong_edi = (frame.len() - 1) as u32;
    server.backdoor.inject_rx_with_edi(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::J1850VPW,
        0,
        in_range_but_wrong_edi,
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &frame[..3], &[], &frame[3..]);

    server.shutdown().await;
}

/// ADR-171 boundary case: `ExtraDataIndex == header` (the lower edge of the
/// clamp's valid `header..=len` range) means zero real payload bytes -- every
/// byte after the header is IFR. Distinct from the out-of-range clamp test
/// above (which exercises the invalid side of the boundary); this proves the
/// valid edge itself is handled correctly rather than assumed by inspection.
#[tokio::test]
#[serial]
async fn j1850pwm_protocol_treats_entire_post_header_span_as_ifr_when_edi_equals_header_len() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850PWM,
        &[(j2534_0404::DATA_RATE, 41_600)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let header = vec![0x61, 0x6B, 0x10];
    let ifr = vec![0xAA, 0xBB];
    let mut frame = header.clone();
    frame.extend_from_slice(&ifr);
    let extra_data_index = header.len() as u32;
    server.backdoor.inject_rx_with_edi(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::J1850PWM,
        0,
        extra_data_index,
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &header, &ifr, &[]);

    server.shutdown().await;
}

/// ADR-051: SCI has no header/footer concept in this service (mirroring
/// `tx_header::build_tx_message`'s "SCI: unchanged" TX-side stance) — the
/// same split mechanism is applied uniformly, but for SCI it is always a
/// no-op: `data_bytes` carries the frame exactly as received and
/// `extra_info` is absent.
#[tokio::test]
#[serial]
async fn sci_protocol_is_unaffected_by_the_header_footer_split() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::SCI_A_ENGINE,
        &[(j2534_0404::DATA_RATE, 7_812)],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let frame = vec![0x01, 0x02, 0x03, 0x04];
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::SCI_A_ENGINE);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &[], &[], &frame);

    server.shutdown().await;
}

/// ADR-097: an ISO15765 `START_OF_MESSAGE` indication (`RxStatus` bit 1 set)
/// carries a header-only `Data` -- just the 4-byte CAN ID, no payload. The
/// existing ADR-051 header/footer split already treats the whole frame as
/// header for a 4-byte ISO15765 frame, so `data_bytes` ends up empty; the
/// new behavior this ADR adds is `rx_flag == [0, 0, 0, 2]`, derived from the
/// `RxStatus` bit rather than from the frame being short.
#[tokio::test]
#[serial]
async fn iso15765_start_of_message_reports_header_only_data_and_rx_flag() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        // ADR-151: CP_StartMsgIndEnable defaults to disabled -- explicitly
        // enabled here so the SOM indication this test exercises actually
        // reaches the client.
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_START_MSG_IND_ENABLE, 1),
        ],
    )
    .await;

    let can_id = 0x7E8_u32;
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_id.to_be_bytes(),
        j2534_0404::ISO15765,
        0x0000_0002, // START_OF_MESSAGE
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(
        &result,
        &can_id.to_be_bytes(),
        &[],
        &[],
        &[0x00, 0x00, 0x00, 0x02],
    );

    server.shutdown().await;
}

/// ADR-097: the same START_OF_MESSAGE indication, but for a `UniqueRespIdTable`
/// entry using extended addressing -- the header widens to 5 bytes (4-byte
/// CAN ID + 1 Address Extension byte), mirroring
/// `iso15765_hardware_extended_addressing_widens_rx_header_to_five_bytes`.
/// `rx_flag` behavior is identical to the 4-byte case: it comes from the
/// `RxStatus` bit, not from the header width.
#[tokio::test]
#[serial]
async fn iso15765_start_of_message_extended_addressing_widens_header_to_five_bytes() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        // ADR-151: CP_StartMsgIndEnable defaults to disabled -- explicitly
        // enabled here so the SOM indication this test exercises actually
        // reaches the client.
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_START_MSG_IND_ENABLE, 1),
        ],
    )
    .await;

    const RESP_EXT_ADDR: u8 = 0xF1;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            9,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_USDT_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_RESP_USDT_EXT_ADDR, RESP_EXT_ADDR as u32),
            ],
        )],
    )
    .await;

    let mut frame = 0x7E8_u32.to_be_bytes().to_vec();
    frame.push(RESP_EXT_ADDR);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::ISO15765,
        0x0000_0002, // START_OF_MESSAGE
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(&result, &frame, &[], &[], &[0x00, 0x00, 0x00, 0x02]);
    assert_eq!(result.unique_resp_identifier, 9);

    server.shutdown().await;
}

/// ADR-097: `rx_flag` classification comes from the `RxStatus` bit, not from
/// the frame's length -- a normal, complete ISO15765 response that happens
/// to be exactly 4 bytes long (indistinguishable on the wire from a
/// CAN-ID-only START_OF_MESSAGE frame) still gets an empty `rx_flag` when
/// `RxStatus == 0`.
#[tokio::test]
#[serial]
async fn iso15765_four_byte_frame_without_start_of_message_bit_has_empty_rx_flag() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            // No UniqueRespIdTable is configured, so the receive-only
            // monitor COP below needs functional addressing to resolve
            // (ADR-100 Decision §5, S8 -- see `arm_receive_only_monitor`'s
            // own doc comment).
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let can_id = 0x7E8_u32;
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &can_id.to_be_bytes(), j2534_0404::ISO15765);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &can_id.to_be_bytes(), &[], &[]);

    server.shutdown().await;
}

/// ADR-097: a `software-isotp`-mode CLL (`RxEntryKind::SoftwareIsoTp`) gets
/// the same `rx_flag` treatment as a native ISO15765 channel -- the
/// per-message `start_of_message` computation in `poll_rx_inner` reads
/// `PASSTHRU_MSG.RxStatus` directly and is not specific to the RX entry kind.
/// A 4-byte CAN-ID-only SOM frame carries no post-ID payload for
/// `isotp::parse_frame` to parse, so it falls through to the raw-delivery arm
/// of `process_frame_for_entry` (same as any other non-ISO-TP raw-CAN
/// traffic on this shared channel) with `start_of_message` still attached.
/// The test then sends a real segmented (FF+CF) response on the same CLL and
/// confirms its `rx_flag` comes back empty -- the SOM indication does not
/// leak state into the following reassembled message.
#[tokio::test]
#[serial]
async fn software_isotp_mode_start_of_message_reports_rx_flag_without_leaking_into_reassembly() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        // ADR-151: CP_StartMsgIndEnable defaults to disabled -- explicitly
        // enabled here so the SOM indication this test exercises actually
        // reaches the client.
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_START_MSG_IND_ENABLE, 1),
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            7,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    // ADR-100 Decision §5 (S8): the reassembled response below is a content
    // frame with no COP waiting on it; a receive-only monitor keeps it
    // delivered under the new unbound-discard model (the SOM indication
    // above is unaffected either way -- indication frames keep their
    // pre-existing delivery path).
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // SOM indication: a raw 4-byte CAN-ID-only frame from the ECU's USDT
    // response ID, RxStatus's START_OF_MESSAGE bit set. Injected as CAN
    // (the raw-CAN protocol id of the physical channel software-isotp mode
    // actually connects), matching how every other frame is injected on this
    // CLL in `can_mode.rs`.
    let can_id = 0x7E8_u32;
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_id.to_be_bytes(),
        j2534_0404::CAN,
        0x0000_0002, // START_OF_MESSAGE
    );

    let som_result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(
        &som_result,
        &can_id.to_be_bytes(),
        &[],
        &[],
        &[0x00, 0x00, 0x00, 0x02],
    );
    assert_eq!(som_result.unique_resp_identifier, 7);

    // A real segmented response (FF + CF) follows on the same CLL, with no
    // RxStatus bit set -- its rx_flag must come back empty, confirming the
    // SOM indication above left no state behind that would keep flagging
    // subsequent messages.
    let response_payload: Vec<u8> = (0x60..0x60 + 10).collect();

    let mut ff = 0x7E8_u32.to_be_bytes().to_vec();
    ff.extend_from_slice(&[0x10, 0x0A]);
    ff.extend_from_slice(&response_payload[..6]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &ff, j2534_0404::CAN);

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut cf = 0x7E8_u32.to_be_bytes().to_vec();
    cf.push(0x21);
    cf.extend_from_slice(&response_payload[6..10]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &cf, j2534_0404::CAN);

    let reassembled_result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(
        &reassembled_result,
        &0x7E8_u32.to_be_bytes(),
        &[],
        &response_payload,
    );
    assert_eq!(reassembled_result.unique_resp_identifier, 7);

    server.shutdown().await;
}

/// ADR-097: a K-line (ISO14230) START_OF_MESSAGE indication carries a
/// completely empty `Data` -- no CAN ID to route on, so it is delivered
/// through the same unconditional (no-table) path as any other K-line
/// frame, without panicking on the empty payload, and with `rx_flag ==
/// [0, 0, 0, 2]`.
#[tokio::test]
#[serial]
async fn iso14230_start_of_message_reports_empty_data_and_rx_flag() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        // ADR-151: CP_StartMsgIndEnable defaults to disabled -- explicitly
        // enabled here so the SOM indication this test exercises actually
        // reaches the client.
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (CP_START_MSG_IND_ENABLE, 1),
        ],
    )
    .await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[],
        j2534_0404::ISO14230,
        0x0000_0002, // START_OF_MESSAGE
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(&result, &[], &[], &[], &[0x00, 0x00, 0x00, 0x02]);

    server.shutdown().await;
}

/// ADR-098: a TxDone indication (`RxStatus` bits 3+0 = `TX_INDICATION` |
/// `TX_MSG_TYPE`, `0x09`) is delivered with the full byte value in
/// `rx_flag`, not just a single bit.
#[tokio::test]
#[serial]
async fn iso15765_tx_done_reports_rx_flag_0x09() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        // ADR-151: CP_TransmitIndEnable defaults to disabled -- explicitly
        // enabled here so the TxDone indication this test exercises actually
        // reaches the client.
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_TRANSMIT_IND_ENABLE, 1),
        ],
    )
    .await;

    let payload = vec![0x02, 0x10, 0x03];
    let mut frame = 0x7E0_u32.to_be_bytes().to_vec();
    frame.extend_from_slice(&payload);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::ISO15765,
        0x0000_0009, // TX_INDICATION | TX_MSG_TYPE
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(
        &result,
        &0x7E0_u32.to_be_bytes(),
        &[],
        &payload,
        &[0x00, 0x00, 0x00, 0x09],
    );

    server.shutdown().await;
}

/// ADR-098: a Loopback-only indication (`RxStatus` bit 0 = `TX_MSG_TYPE`,
/// `0x01`, no other bit set) is delivered with `rx_flag == [0, 0, 0, 0x01]`.
#[tokio::test]
#[serial]
async fn iso15765_loopback_only_reports_rx_flag_0x01() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let payload = vec![0x62, 0xF1, 0x90];
    let mut frame = 0x7E8_u32.to_be_bytes().to_vec();
    frame.extend_from_slice(&payload);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::ISO15765,
        0x0000_0001, // TX_MSG_TYPE
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(
        &result,
        &0x7E8_u32.to_be_bytes(),
        &[],
        &payload,
        &[0x00, 0x00, 0x00, 0x01],
    );

    server.shutdown().await;
}

/// ADR-098: an ISO14230 (K-line) RxBreak indication (`RxStatus` bit 2 =
/// `RX_BREAK`, `0x04`) carries a completely empty `Data`, same shape as the
/// ISO14230 SOM case above -- delivered without panicking, `rx_flag ==
/// [0, 0, 0, 0x04]`.
#[tokio::test]
#[serial]
async fn iso14230_rx_break_reports_empty_data_and_rx_flag_0x04() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[],
        j2534_0404::ISO14230,
        0x0000_0004, // RX_BREAK
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(&result, &[], &[], &[], &[0x00, 0x00, 0x00, 0x04]);

    server.shutdown().await;
}

/// ADR-098: an ISO15765 frame with `RxStatus` bit 4 (`ISO15765_PADDING_ERROR`,
/// `0x10`) set -- a CAN frame with fewer than 8 data bytes received under
/// ISO15765 -- routes normally (4-byte CAN-ID header + short payload) and
/// carries `rx_flag == [0, 0, 0, 0x10]`.
#[tokio::test]
#[serial]
async fn iso15765_padding_error_short_frame_reports_rx_flag_0x10() {
    let server = TestServer::start().await;
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
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // 4-byte CAN ID + 2 payload bytes = 6 total (fewer than 8 CAN data
    // bytes -- the condition ISO15765_PADDING_ERROR flags).
    let payload = vec![0x61, 0x00];
    let mut frame = 0x7E8_u32.to_be_bytes().to_vec();
    frame.extend_from_slice(&payload);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::ISO15765,
        0x0000_0010, // ISO15765_PADDING_ERROR
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(
        &result,
        &0x7E8_u32.to_be_bytes(),
        &[],
        &payload,
        &[0x00, 0x00, 0x00, 0x10],
    );

    server.shutdown().await;
}

/// ADR-098: a degenerate ISO15765 frame shorter than the 4-byte CAN-ID
/// header, with `RxStatus` bit 4 (`ISO15765_PADDING_ERROR`) set, does not
/// panic -- `header_footer_len`'s existing too-short-for-header fallback
/// (`(data.len(), 0)`) reports the whole frame as header, same as it already
/// does for any other RxStatus value.
#[tokio::test]
#[serial]
async fn iso15765_degenerate_short_frame_with_padding_error_does_not_panic() {
    let server = TestServer::start().await;
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
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let frame = vec![0x00, 0x01, 0x02]; // shorter than the 4-byte CAN-ID header
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::ISO15765,
        0x0000_0010, // ISO15765_PADDING_ERROR
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(&result, &frame, &[], &[], &[0x00, 0x00, 0x00, 0x10]);

    server.shutdown().await;
}

/// ADR-098 regression check: a Normal Message (`RxStatus == 0`, all 5 low
/// bits clear) still yields an empty `rx_flag`, even when injected via the
/// same `inject_rx_with_status` path the other tests above use (rather than
/// the `inject_rx` convenience wrapper that already defaults to 0).
#[tokio::test]
#[serial]
async fn iso15765_normal_message_with_explicit_zero_status_has_empty_rx_flag() {
    let server = TestServer::start().await;
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
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x62, 0xF1, 0x90];
    let mut frame = 0x7E8_u32.to_be_bytes().to_vec();
    frame.extend_from_slice(&payload);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame,
        j2534_0404::ISO15765,
        0x0000_0000,
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x7E8_u32.to_be_bytes(), &[], &payload);

    server.shutdown().await;
}
