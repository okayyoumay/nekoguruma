//! End-to-end coverage for SAE J2534-2 clause 22 ISO15765-2-on-CAN-FD's
//! connect-time protocol substitution and ComParam mapping (ADR-159, Phase 3b),
//! continuing directly from [`fd_can.rs`]'s clause 21 coverage (ADR-158, Phase
//! 3a) -- mirrors that file's structure/helpers wherever the underlying
//! mechanism is shared (many helpers live in `harness.rs` and are reused
//! here, not duplicated): staged Working ComParams (`CP_CANFDTxMaxDataLength`/
//! `CP_CANFDBaudrate`) driving `ConnectComLogicalLink`'s internal
//! `PassThruIoctl(SET_CONFIG, CONFIG_FD_CAN_DATA_PHASE_RATE)` ->
//! `PassThruConnect(FD_ISO15765_PS)` -> `PassThruIoctl(SET_CONFIG,
//! CONFIG_J1962_PINS)` sequence -- invisible to the client on success, so
//! these tests use `server.backdoor.channel_protocol_id`/`config_value`/
//! `set_config_param_log` to observe the mock's actual channel state, exactly
//! as `fd_can.rs` does for `FD_CAN_PS`.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DisconnectComLogicalLinkRequest, ExpectedResponseData,
    ModuleHandle, ResourceData, StartComPrimitiveRequest, create_com_logical_link_request,
};

use crate::harness::*;

/// Same helper shape as `fd_can.rs`'s/`pin_selection.rs`'s/
/// `additional_channels.rs`'s `try_create_cll_with_resource` -- a per-file
/// local helper, not shared via `harness.rs`, matching this codebase's
/// existing convention for this specific shape.
async fn try_create_cll_with_resource(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    module_handle: u32,
    resource: ResourceData,
    _cll_tag: u64,
) -> Result<ComLogicalLinkHandle, tonic::Status> {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle { module_handle }),
            resource: Some(create_com_logical_link_request::Resource::RscData(resource)),
            cll_create_flag: None,
        })
        .await
        .map(|r| {
            r.into_inner()
                .cll_handle
                .expect("cll_handle should be present")
        })
}

fn sendrecv_ctrl_data() -> ComPrimitiveCtrlData {
    ComPrimitiveCtrlData {
        time: 0,
        num_send_cycles: 1,
        num_receive_cycles: 0,
        temp_param_update: 0,
        expected_response_array: Vec::<ExpectedResponseData>::new(),
        tx_flag: None,
    }
}

/// Item 1: staging FD ComParams on an ISO15765-connected CLL now substitutes
/// to `FD_ISO15765_PS` -- was rejected outright before this stage (ADR-158's
/// PR #30 round-1 correction).
#[tokio::test]
#[serial]
async fn fd_iso15765_connect_substitutes_native_protocol_id() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_PS,
        "staged FD ComParams on an ISO15765-connected CLL should substitute the native connect \
         id to FD_ISO15765_PS"
    );

    server.shutdown().await;
}

/// Item 2: mode is recomputed fresh on every connect, never sticky --
/// reconnecting after clearing the FD-triggering ComParam must flip the link
/// back to plain ISO15765 on a brand-new physical channel.
#[tokio::test]
#[serial]
async fn fd_iso15765_reconnect_after_clearing_fd_comparams_reverts_to_classic_iso15765() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
        ],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_PS
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 0).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("reconnecting after clearing the FD ComParam should succeed");

    const RECONNECT_CHANNEL_ID: u32 = 2;
    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the reconnect's differing native protocol id (ISO15765 vs FD_ISO15765_PS) should open \
         a brand-new physical channel, not rejoin the FD one"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(RECONNECT_CHANNEL_ID),
        j2534_0404::ISO15765,
        "clearing the FD-triggering ComParam must flip the reconnect back to plain ISO15765"
    );

    server.shutdown().await;
}

/// Item 3: clause 22's connect-sequencing rule (mirrors clause 21.3.2.5.1)
/// requires `CONFIG_FD_CAN_DATA_PHASE_RATE` before `CONFIG_J1962_PINS`, on
/// the same reused mechanism ADR-158 already implemented -- the mock's own
/// sequencing enforcement makes a wrong order fail the connect outright.
#[tokio::test]
#[serial]
async fn fd_iso15765_connect_applies_data_phase_rate_before_pins() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_PS
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_FD_CAN_DATA_PHASE_RATE),
        2_000_000,
        "the data phase rate should be CP_CANFDBaudrate (nonzero, so it wins over CP_Baudrate)"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_060E,
        "an unqualified FD connect (no dlc_pin_data) should assign ISO15765's own default \
         packed pins (6/HI, 14/LOW, same defaults as CAN's)"
    );

    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    let rate_index = log
        .iter()
        .position(|&p| p == j2534_0404::CONFIG_FD_CAN_DATA_PHASE_RATE)
        .expect("CONFIG_FD_CAN_DATA_PHASE_RATE should have been SET_CONFIG'd");
    let pins_index = log
        .iter()
        .position(|&p| p == j2534_0404::CONFIG_J1962_PINS)
        .expect("CONFIG_J1962_PINS should have been SET_CONFIG'd");
    assert!(
        rate_index < pins_index,
        "CONFIG_FD_CAN_DATA_PHASE_RATE must be SET_CONFIG'd before CONFIG_J1962_PINS; log was \
         {log:#x?}"
    );

    server.shutdown().await;
}

/// Item 4: `CP_CANFDTxMaxDataLength`/`CP_Cr`/`CP_CanFillerByte` staged on an
/// `FD_ISO15765_PS` link reach the mock as `CONFIG_FD_ISO15765_TX_DATA_LENGTH`/
/// `CONFIG_N_CR_MAX`/`CONFIG_ISO15765_PAD_VALUE` with correctly converted
/// values -- ADR-159's first non-identity `to_j2534_config_id` translations.
#[tokio::test]
#[serial]
async fn fd_iso15765_forwards_comparams_to_their_native_fd_targets() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
            (CP_N_CR, 1_000_000),
            (CP_CAN_FILLER_BYTE, 0xAA),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_PS,
        "sanity: cll must actually be FD-substituted for these translations to fire at all"
    );

    assert_eq!(
        server.backdoor.config_value(
            MOCK_CHANNEL_ID,
            j2534_0404::CONFIG_FD_ISO15765_TX_DATA_LENGTH
        ),
        64,
        "CP_CANFDTxMaxDataLength should forward to CONFIG_FD_ISO15765_TX_DATA_LENGTH unchanged"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_N_CR_MAX),
        1000,
        "CP_Cr's 1_000_000 us default should forward to CONFIG_N_CR_MAX converted to 1000 ms"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_ISO15765_PAD_VALUE),
        0xAA,
        "CP_CanFillerByte should forward to CONFIG_ISO15765_PAD_VALUE unchanged (identity)"
    );

    server.shutdown().await;
}

/// Item 4 (continued): a staged `CP_CANFDTxMaxDataLength` of `0` (unset --
/// explicitly staged here via `SetComParam` so the Working map genuinely
/// carries a `0` entry to forward, rather than relying on whether this
/// route's bus-type defaults happen to pre-populate one; `CP_CANFDBaudrate`
/// alone still triggers FD mode) floors to `8` on the native side, rather
/// than letting Table 97's native default (`64`) silently win.
#[tokio::test]
#[serial]
async fn fd_iso15765_tx_data_length_floors_at_eight_when_unset() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 0),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_PS,
        "sanity: CP_CANFDBaudrate alone should still trigger FD substitution"
    );
    assert_eq!(
        server.backdoor.config_value(
            MOCK_CHANNEL_ID,
            j2534_0404::CONFIG_FD_ISO15765_TX_DATA_LENGTH
        ),
        8,
        "a staged CP_CANFDTxMaxDataLength of 0 (unset) should floor to 8, not forward as 0"
    );

    server.shutdown().await;
}

/// Item 5: a TX message up to 4128 bytes (SAE J2534-2 Table 98's shared
/// maximum) is accepted on an `FD_ISO15765_PS` link under normal (non-AE)
/// addressing -- a size this stage's constant range widens far past
/// pre-ADR-159's `4..=4099`. Confirms NO padding is applied: the recorded
/// write length equals the input length plus the 4-byte header exactly,
/// unlike `FD_CAN_PS`'s DLC-rounding padding behavior.
#[tokio::test]
#[serial]
async fn fd_iso15765_accepts_max_size_payload_and_applies_no_padding() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // 4-byte header + 4124-byte payload == 4128, Table 98's shared maximum.
    let max_payload = vec![0x11u8; 4124];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: max_payload.clone(),
            cop_ctrl_data: Some(sendrecv_ctrl_data()),
        })
        .await
        .expect(
            "a 4124-byte payload (4128-byte message) should be accepted on an FD_ISO15765_PS \
             link",
        );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.len(),
        4 + 4124,
        "no padding should be applied on an FD_ISO15765_PS link -- the native adapter pads its \
         own ISO15765 frames, driven by TX_ISO15765_FRAME_PAD/CONFIG_ISO15765_PAD_VALUE"
    );
    assert_eq!(&written[4..], max_payload.as_slice());

    // One byte over the shared maximum must be rejected.
    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x22u8; 4125],
            cop_ctrl_data: Some(sendrecv_ctrl_data()),
        })
        .await
        .expect_err(
            "a 4125-byte payload (4129-byte message) exceeds Table 98's shared maximum and must \
             be rejected",
        );
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Item 5 (continued): the extended-addressing row of Table 98 (5-byte
/// header, one more than the normal row) accepts the same 4128-byte overall
/// maximum with a correspondingly smaller payload cap, and rejects one byte
/// over.
#[tokio::test]
#[serial]
async fn fd_iso15765_accepts_max_size_payload_under_extended_addressing() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_PHYS_REQ_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_PHYS_REQ_EXT_ADDR, 0x01),
            ],
        )],
    )
    .await;

    // 5-byte header (4-byte CAN ID + 1 AE byte) + 4123-byte payload == 4128.
    let max_payload = vec![0x33u8; 4123];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: max_payload.clone(),
            cop_ctrl_data: Some(sendrecv_ctrl_data()),
        })
        .await
        .expect(
            "a 4123-byte payload (4128-byte message under extended addressing) should be \
             accepted",
        );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.len(),
        5 + 4123,
        "no padding should be applied under extended addressing either"
    );

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x44u8; 4124],
            cop_ctrl_data: Some(sendrecv_ctrl_data()),
        })
        .await
        .expect_err("one byte over the extended-addressing max must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Item 6: `TX_FD_CAN_FORMAT`/`TX_FD_CAN_BRS` are set on `FD_ISO15765_PS`
/// sends, exactly like `FD_CAN_PS` (SAE J2534-2 Tables 99-100 apply these
/// flags to every ISO15765-on-CAN-FD message the same way they apply to
/// plain FD CAN messages).
#[tokio::test]
#[serial]
async fn fd_iso15765_send_carries_fd_format_and_brs_flags() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
            cop_ctrl_data: Some(sendrecv_ctrl_data()),
        })
        .await
        .expect("start_com_primitive should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_FORMAT,
        0,
        "an FD_ISO15765_PS link's TX must carry TX_FD_CAN_FORMAT"
    );
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_BRS,
        0,
        "a nonzero staged CP_CANFDBaudrate requests the data-phase bit-rate switch"
    );

    server.shutdown().await;
}

/// Item 7: the round-3 `CoptUpdateparam` FD-crossing rejection mechanism
/// (ADR-158) also fires correctly on an ISO15765 link -- a Classic-connected
/// CLL promoting FD-triggering ComParams must be rejected, not silently
/// promoted.
#[tokio::test]
#[serial]
async fn fd_iso15765_coptupdateparam_rejects_promoting_fd_comparams_on_a_classic_connected_link() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::ISO15765,
        "sanity: cll_handle must be Classic-connected for this scenario to apply"
    );

    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_BAUDRATE, 2_000_000).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "CoptUpdateparam promoting FD-triggering ComParams onto a Classic-connected \
             ISO15765 link must be rejected, not silently promoted",
        );
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Item 7 (continued): the mirror-image direction -- an FD_ISO15765_PS
/// -connected CLL promoting ComParams that no longer signal FD must also be
/// rejected.
#[tokio::test]
#[serial]
async fn fd_iso15765_coptupdateparam_rejects_promoting_classic_comparams_on_an_fd_connected_link() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
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
        "sanity: cll_handle must be FD-connected for this scenario to apply"
    );

    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 0).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_BAUDRATE, 0).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "CoptUpdateparam promoting Classic-signaling ComParams onto an FD_ISO15765_PS link \
             must be rejected, not silently promoted",
        );
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Item 8: a directly-named `PROTOCOL_FD_ISO15765_PS` protocol id is rejected
/// at `CreateComLogicalLink` time -- never silently normalized to a plain
/// ISO15765 connect (mirrors `fd_can.rs`'s `direct_fd_can_protocol_id_naming_
/// is_rejected`).
#[tokio::test]
#[serial]
async fn direct_fd_iso15765_ps_protocol_id_naming_is_rejected() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let status = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol(j2534_0404::PROTOCOL_FD_ISO15765_PS),
        1,
    )
    .await
    .expect_err("a directly-named FD_ISO15765_PS protocol id must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejection must happen at CreateComLogicalLink, before any connect is attempted"
    );

    server.shutdown().await;
}

/// Item 8 (continued), corrected for ADR-213/Round 3: a directly-named
/// `FD_ISO15765_CHx` id (here, the block's first entry) is STILL rejected --
/// but the reason changed. Before ADR-213, `chx_base_protocol_id` had no
/// in-scope mapping for either FD family at all, so this was rejected as an
/// out-of-scope `_CHx` family before ever reaching the FD-specific guard.
/// ADR-213 brings CAN FD into `_CHx` scope (`chx_base_protocol_id` now
/// resolves `FD_ISO15765_CH1` to `(PROTOCOL_FD_ISO15765_PS, 1)`), so this id
/// is now caught instead by `names.rs`'s existing direct-FD-naming rejection
/// guard, widened to recognize `_CHx` too (`resources::is_fd_protocol_id`,
/// section 2b) -- proving that widening didn't accidentally let a `_CHx` FD
/// id slip through unrejected. The outcome (still `InvalidArgument`, still
/// rejected before `CreateComLogicalLink` completes) is unchanged; only the
/// rejection's own internal reason and message (now mentioning `_CHx`) are.
#[tokio::test]
#[serial]
async fn direct_fd_iso15765_chx_protocol_id_naming_is_rejected() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let status = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol(j2534_0404_sys::bindings::PROTOCOL_FD_ISO15765_CH1),
        1,
    )
    .await
    .expect_err("a directly-named FD_ISO15765_CH1 protocol id must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("_CHx"),
        "the rejection message should now mention _CHx as well as _PS: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejection must happen at CreateComLogicalLink, before any connect is attempted"
    );

    server.shutdown().await;
}

/// ADR-213/Round 3 core promotion test, mirroring `fd_can.rs`'s
/// `fd_mode_combined_with_a_directly_named_chx_id_promotes_to_fd_can_chx`: FD
/// mode combined with a directly-named `_CHx`-qualified ISO15765 link now
/// PROMOTES to the corresponding native `FD_ISO15765_CHx` id (this
/// supersedes ADR-159 Decision item 5's `channel_index.is_some()`
/// rejection, ADR-213 Decision item 1).
#[tokio::test]
#[serial]
async fn fd_mode_combined_with_a_directly_named_iso15765_chx_id_promotes_to_fd_iso15765_chx() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol(j2534_0404::PROTOCOL_ISO15765_CH1 + 2), // _CH3
        1,
    )
    .await
    .expect("a directly-named ISO15765 _CH3 id alone should resolve to a _CHx hardware variant");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "FD mode combined with a _CHx-qualified ISO15765 link should now promote (ADR-213), \
             not be rejected",
        );

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_CH1 + 2,
        "the connect should substitute to the native FD_ISO15765_CH3 id, not plain ISO15765_CH3 \
         or bare ISO15765"
    );

    server.shutdown().await;
}

/// ADR-213/Round 3 regression test (edge-case-hunter finding, PR review):
/// `CP_CANFDTxMaxDataLength`/`CP_Cr`/`CP_CanFillerByte` staged on a `_CHx`-
/// connected FD ISO15765 link must reach the mock as
/// `CONFIG_FD_ISO15765_TX_DATA_LENGTH`/`CONFIG_N_CR_MAX`/
/// `CONFIG_ISO15765_PAD_VALUE`, mirroring `fd_iso15765_forwards_comparams_
/// to_their_native_fd_targets`'s own `_PS` case exactly. Before the fix,
/// `comparam_id::to_j2534_config_id`'s own translation block for these three
/// ComParams checked `hw_protocol_id == PROTOCOL_FD_ISO15765_PS` (an exact,
/// `_CHx`-blind equality) rather than the widened `_CHx`-inclusive range this
/// round's other FD checks already use, so on a `_CHx`-connected FD
/// ISO15765 link none of the three ever translated at all -- they silently
/// fell through to the generic identity/`None` table (which has no entry
/// for any of them), so `SET_CONFIG` was never issued and the link silently
/// ran on native/mock defaults instead of the client's staged values, with
/// no error surfaced anywhere.
#[tokio::test]
#[serial]
async fn fd_iso15765_chx_forwards_comparams_to_their_native_fd_targets() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol(j2534_0404::PROTOCOL_ISO15765_CH1 + 2), // _CH3
        1,
    )
    .await
    .expect("a directly-named ISO15765 _CH3 id alone should resolve to a _CHx hardware variant");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_BAUDRATE, 2_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_N_CR, 1_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FILLER_BYTE, 0xAA).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("FD mode combined with a _CHx-qualified ISO15765 link should promote (ADR-213)");
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_CH1 + 2,
        "sanity: cll must actually be FD-substituted (to the _CH3 sibling) for these \
         translations to fire at all"
    );

    assert_eq!(
        server.backdoor.config_value(
            MOCK_CHANNEL_ID,
            j2534_0404::CONFIG_FD_ISO15765_TX_DATA_LENGTH
        ),
        64,
        "CP_CANFDTxMaxDataLength should forward to CONFIG_FD_ISO15765_TX_DATA_LENGTH unchanged \
         on a _CHx-connected FD ISO15765 link too"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_N_CR_MAX),
        1000,
        "CP_Cr's 1_000_000 us default should forward to CONFIG_N_CR_MAX converted to 1000 ms \
         on a _CHx-connected FD ISO15765 link too"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_ISO15765_PAD_VALUE),
        0xAA,
        "CP_CanFillerByte should forward to CONFIG_ISO15765_PAD_VALUE unchanged (identity) on a \
         _CHx-connected FD ISO15765 link too"
    );

    server.shutdown().await;
}

/// ADR-213/Round 3 reversion test, mirroring `fd_can.rs`'s own
/// `reconnect_after_clearing_fd_comparams_on_a_chx_link_flips_back_to_can_
/// chx_not_bare_can`: reconnecting a `_CHx` FD-ISO15765 link after its
/// ComParams stop signaling FD must land back on the SAME `ISO15765_CH<n>`
/// id it started from, not bare `ISO15765`.
#[tokio::test]
#[serial]
async fn reconnect_after_clearing_fd_comparams_on_an_iso15765_chx_link_flips_back_to_iso15765_chx_not_bare()
 {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol(j2534_0404::PROTOCOL_ISO15765_CH1 + 2), // _CH3
        1,
    )
    .await
    .expect("a directly-named ISO15765 _CH3 id alone should resolve to a _CHx hardware variant");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect should succeed and promote to FD_ISO15765_CH3");
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_CH1 + 2
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 0).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("reconnecting after clearing the FD ComParam should succeed");

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the reconnect's differing native protocol id (ISO15765_CH3 vs FD_ISO15765_CH3) should \
         open a brand-new physical channel, not rejoin the FD one"
    );
    const RECONNECT_CHANNEL_ID: u32 = 2;
    assert_eq!(
        server.backdoor.channel_protocol_id(RECONNECT_CHANNEL_ID),
        j2534_0404::PROTOCOL_ISO15765_CH1 + 2,
        "clearing the FD-triggering ComParam on a _CHx link must revert to that SAME \
         ISO15765_CH3 id -- ADR-213's three-way revert -- not bare ISO15765"
    );

    server.shutdown().await;
}

/// ADR-213/Round 3 capacity-cap discriminating test, mirroring `fd_can.rs`'s
/// own `connect_rejects_a_chx_index_within_the_generic_capacity_but_above_
/// fd_cans_own`, for the ISO15765-on-CAN-FD family and its own dedicated
/// `DEVICE_INFO_FD_ISO15765_SUPPORTED` capacity flag.
#[tokio::test]
#[serial]
async fn connect_rejects_an_iso15765_chx_index_within_the_generic_capacity_but_above_fd_iso15765s_own()
 {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    server.backdoor.set_chx_capacity(10);
    server.backdoor.set_fd_iso15765_chx_capacity(1);
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol(j2534_0404::PROTOCOL_ISO15765_CH1 + 4), // _CH5
        1,
    )
    .await
    .expect("a directly-named ISO15765 _CH5 id alone should resolve to a _CHx hardware variant");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "_CH5 is within the generic families' cached capacity (10) but exceeds \
             ISO15765-on-CAN-FD's own (1) -- must be rejected synchronously by \
             check_chx_capacity consulting DEVICE_INFO_FD_ISO15765_SUPPORTED, not \
             DEVICE_INFO_ISO15765_SUPPORTED",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the capacity precheck must reject before any native PassThruConnect is attempted"
    );

    server.shutdown().await;
}

/// ADR-213 Decision item 3, mirroring `fd_can.rs`'s own
/// `fd_can_chx_connect_does_not_synthesize_j1962_pins`: a `_CHx`-connected
/// FD-ISO15765 link's connect must NOT issue `SET_CONFIG(CONFIG_J1962_PINS)`
/// -- unlike an unqualified `_PS` FD-ISO15765 connect
/// (`fd_iso15765_connect_applies_data_phase_rate_before_pins` above, which
/// DOES synthesize a default pin assignment), a `_CHx` channel is already
/// vendor-pin-preassigned per clause 22.3.2.6.1. The data-phase-rate
/// SET_CONFIG must still fire, unconditionally.
#[tokio::test]
#[serial]
async fn fd_iso15765_chx_connect_does_not_synthesize_j1962_pins() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol(j2534_0404::PROTOCOL_ISO15765_CH1 + 2), // _CH3
        1,
    )
    .await
    .expect("a directly-named ISO15765 _CH3 id alone should resolve to a _CHx hardware variant");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect should succeed and promote to FD_ISO15765_CH3");

    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_CH1 + 2
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_FD_CAN_DATA_PHASE_RATE),
        "the data-phase rate SET_CONFIG must still fire for a _CHx FD-ISO15765 connect -- it's \
         the step that attaches the channel per clause 22.3.2.6.1"
    );
    assert!(
        !log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "a _CHx FD-ISO15765 connect must NOT synthesize a CONFIG_J1962_PINS call -- the channel \
         is already vendor-pin-preassigned"
    );

    server.shutdown().await;
}

/// Item 10 (ADR-169): a native `FD_ISO15765_PS` link's functional-addressing
/// Single Frame limit (ADR-055) tracks the link's staged
/// `CP_CANFDTxMaxDataLength` instead of the fixed Classic-CAN 7-byte limit --
/// at `TX_DL = 64` the Normal-addressing limit widens to 62 bytes (ADR-169's
/// table), and one byte over is still rejected, naming
/// `CP_CANFDTxMaxDataLength` in the error.
#[tokio::test]
#[serial]
async fn fd_iso15765_functional_addressing_widens_single_frame_limit_with_staged_tx_dl() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
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
        "sanity: cll must actually be FD-substituted for the widened limit to apply"
    );

    // 62 bytes fits ADR-169's Normal-addressing max SF at TX_DL=64.
    let at_max_payload = vec![0u8; 62];
    send_data(&mut client, cll_handle, at_max_payload.clone(), vec![]).await;
    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&at_max_payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    // One byte over must still be rejected, naming CP_CANFDTxMaxDataLength.
    let too_long_payload = vec![0u8; 63];
    let status = send_data_expect_rejected(&mut client, cll_handle, too_long_payload).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("Single Frame"),
        "{}",
        status.message()
    );
    assert!(
        status.message().contains("CP_CANFDTxMaxDataLength"),
        "the rejection should name CP_CANFDTxMaxDataLength so a client sees which ComParam to \
         check: {}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    server.shutdown().await;
}

/// Item 10 (continued): an `FD_ISO15765_PS` link with `CP_CANFDTxMaxDataLength`
/// left unset (`CP_CANFDBaudrate` alone still triggers FD substitution)
/// floors its effective TX_DL at 8 (ADR-169's `effective_fd_tx_dl`, shared
/// with `fd_can_tx_message_size_range`'s existing fallback) -- so the
/// functional-addressing Single Frame limit stays the Classic 7 bytes, same
/// as a plain ISO15765 link.
#[tokio::test]
#[serial]
async fn fd_iso15765_functional_addressing_keeps_seven_byte_limit_when_tx_dl_unset() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_BAUDRATE, 2_000_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_PS,
        "sanity: CP_CANFDBaudrate alone should still trigger FD substitution"
    );

    // 7 bytes still fits (unchanged Classic limit).
    let at_max_payload = vec![0u8; 7];
    send_data(&mut client, cll_handle, at_max_payload.clone(), vec![]).await;
    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&at_max_payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    // 8 bytes is still rejected -- an unset CP_CANFDTxMaxDataLength must not
    // silently widen the limit past Classic CAN's own capacity.
    let too_long_payload = vec![0u8; 8];
    let status = send_data_expect_rejected(&mut client, cll_handle, too_long_payload).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("Single Frame"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    server.shutdown().await;
}

/// Item 9 (highest risk): a dual-channel-mode-eligible FD-substituted
/// ISO15765 link must NOT open a Classic-CAN UUDT companion channel (it
/// would route UUDT traffic to the wrong, Classic-format resource), but MUST
/// get the point-to-point `FLOW_CONTROL_FILTER` fallback installed on its own
/// main channel instead -- exercises Decision 4's three gates (the
/// `install_point_to_point_fc_filters` `qualified` argument and both
/// companion-channel skips), not just Decision 1's substitution. Mirrors
/// `pin_selection.rs`'s `dual_channel_mode_skips_uudt_companion_for_pin_
/// selected_link`/`dual_channel_mode_installs_fallback_filter_for_pin_
/// selected_link` pair, combined into one test since both assertions share
/// one connect.
#[tokio::test]
#[serial]
async fn fd_iso15765_dual_channel_mode_skips_uudt_companion_but_installs_fallback_filter() {
    let server = TestServer::try_start_with_extra_config(&format!(
        "can_channel_mode = \"dual-channel\"\n{}",
        modules_toml(&[("Bench 1", "J2534-2:mock")])
    ))
    .await
    .expect("service should initialize with a valid modules + can_channel_mode config");
    let mut client = server.client().await;

    let cll_handle = create_cll_for_module(&mut client, 1, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;

    // Set the UUDT response ID BEFORE connect, so it is already Active
    // (Working -> Active promotion happens at Connect) and `has_uudt_ids` is
    // true at connect time -- exactly the condition that reaches both the
    // companion-open block and the fallback-filter decision under test.
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connect_com_logical_link must succeed for a dual-channel-mode FD-substituted \
             ISO15765 link with UUDT IDs configured -- the companion-channel open must be \
             skipped, not attempted or failed",
        );

    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_PS,
        "sanity: this link must actually be FD-substituted for Decision 4's gates to be \
         exercised at all, not just Decision 1's"
    );
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "no Classic-CAN UUDT companion channel should have been opened for an FD-substituted \
         link"
    );
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "an FD-substituted link never gets a companion channel, so it must always get the \
         point-to-point FLOW_CONTROL_FILTER fallback for its UUDT response id instead"
    );

    server.shutdown().await;
}
