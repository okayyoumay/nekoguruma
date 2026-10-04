//! End-to-end coverage for SAE J2534-2 clause 16 SAE J1939 Protocol
//! (ADR-179, Phase 5): connecting the new standalone `J1939_PS` resource
//! rows' mandatory explicit `dlc_pin_data` (clause 16.3.2.1's "no default
//! pin identified" model -- unlike J1708/UART Echo Byte/Honda DIAG-H,
//! `resources.rs`'s own two J1939 rows carry no default pins at all, so
//! `dlc_pin_data` is required even when a resource-table row is matched),
//! the address-claim/defend state machine `CoptStartcomm` drives (ADR-179
//! Decision 3), the `CP_J1939TargetAddress == 0xFFFF` StartComPrimitive-time
//! rejection (Decision 4), the native `ERR_ADDRESS_NOT_CLAIMED` write
//! failure's surfaced event, and `tx_header::j1939_header_bytes`'s composed
//! 5-byte prefix reaching the wire (Decision 6). Mirrors `j1708.rs`'s
//! structure for the harness/helper conventions; a few small per-file-local
//! helpers are duplicated the same way `honda_diagh.rs`/`j1708.rs` duplicate
//! their own (this codebase's existing convention for this shape of helper).
//!
//! **Historical note: `GetComParam(CP_TesterSourceAddress)` readback was
//! initially unusable, now fixed.** ADR-179 Decision 3 states the claimed
//! address is "written back into `CP_TesterSourceAddress` for client
//! readback after `StartComm` completes." The write-back itself
//! (`events.rs`/`events_j1939_claim.rs` inserting into `NODE_ADDRESS`
//! directly) predates `comparam_support.rs::is_j1939_param` actually
//! admitting `ComParamId(j2534_0404::NODE_ADDRESS)` into J1939's allowlist --
//! that gap is now closed (`is_j1939_param` includes it, and
//! `comparam_support::tests::j1939_ps_allows_only_its_own_closed_param_list`
//! pins it as ALLOWED), so a live `GetComParam(CP_TesterSourceAddress)` call
//! against a J1939 CLL now returns the actual claimed address. Every test
//! below that needs to confirm which address was actually claimed still
//! does so through the one surface that is observable end to end regardless
//! of this fix: the claimed address is used as the source-address byte
//! (`tx_header::j1939_header_bytes` byte 3) of a subsequent `CoptSendrecv`
//! frame, read back via `MockBackdoor::written_data`. This also directly
//! serves this file's own message-framing coverage (item 6 below).
//!
//! **Item 2 (retry-over-the-candidate-list on a LOST first candidate) is
//! written using the backdoor, but is inherently timing-sensitive in a way
//! this harness cannot fully eliminate, and is flagged as such rather than
//! presented as a clean deterministic test.** `j2534-0404-mock`'s
//! `__mock_set_j1939_claim_lost` is a single GLOBAL toggle (every channel,
//! every address), not per-address and not call-counted -- there is no mock
//! surface to make ONLY the first of several `IOCTL_PROTECT_J1939_ADDR`
//! attempts lose. The retry loop (`run_j1939_claim_loop`) issues each
//! candidate back-to-back once the previous one resolves, and a candidate's
//! own resolution takes at least one internal `POLL_INTERVAL_MS` (10 ms)
//! tick -- there is no client-observable signal (RPC ack, `SubscribeEvent`)
//! precise enough to straddle that ~10 ms server-internal window reliably:
//! this harness's own documented round-trip ceiling under load
//! (`GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS`, ADR-149) is *larger* than the
//! window itself. The test below flips the toggle back to `false`
//! immediately after the `StartComPrimitive` RPC ack returns (the earliest
//! point this test can act), which should in practice land before the
//! second candidate's own attempt is issued, but is not a hard guarantee.
//! The assertions are structured so a missed window fails loudly (the COP
//! not finishing cleanly, or the wrong candidate being claimed) rather than
//! silently passing on the wrong candidate.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    CancelComPrimitiveRequest, ComLogicalLinkHandle, ComPrimitiveCtrlData, ComPrimitiveHandle,
    ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, DataItem,
    DisconnectComLogicalLinkRequest, ExpectedResponseData, GetObjectIdRequest, GetStatusRequest,
    IoBytearray, ModuleHandle, ObjectType, ParamItem, PduComPrimitiveStatus, PduError,
    PduErrorEvent, PinData, ResourceData, SetComParamRequest, SetUniqueRespIdTableRequest,
    StartComPrimitiveRequest, UniqueRespIdTableItem, create_com_logical_link_request, data_item,
    error_detail_from_status, event_item, get_status_request, io_ctl_request, param_item,
    resource_data, status_response, subscribe_event_request,
};

use crate::harness::*;

/// Resource id of `"ISO_OBD_on_SAE_J1939_73"` -- `resources.rs` row
/// 0x025D, `protocol: ChannelProtocol::J1939_PS`, `hw_protocol_override:
/// None`, `bus_type_name: "SAE_J1939_11_DWCAN"` (250 kbps default),
/// `dlc_pins: &[]` (no default pins -- ADR-179 Decision 2). Message
/// priority default is overridden to 0 by `comparam_defaults.rs`'s
/// `iso_obd_on_sae_j1939_73()`.
const J1939_ISO_OBD_RESOURCE_ID: u32 = 0x025D;

/// D-PDU `CP_J1939AddressNegotiationRule` ComParam ID
/// (`service_params::PARAM_J1939_ADDR_NEG_RULE`, 0x8086). Bit 1 clear
/// (the spec default, 0) requests a claim on `CoptStartcomm`; bit 1 set
/// (`0b10`) suppresses it (ADR-179 Decision 3/`events_j1939_claim.rs::
/// j1939_claim_requested`'s inverse-polarity note).
const CP_J1939_ADDR_NEG_RULE: u32 = 0x8086;

/// D-PDU `CP_J1939PreferredAddress` ComParam ID
/// (`service_params::PARAM_J1939_PREFERRED_ADDRESS`, 0x8092): a Bytefield
/// ComParam, the ordered candidate address list `CoptStartcomm`'s claim
/// loop retries over.
const CP_J1939_PREFERRED_ADDRESS: u32 = 0x8092;

/// D-PDU `CP_J1939Name` ComParam ID (`service_params::PARAM_J1939_NAME`,
/// 0x8094): an 8-byte Bytefield ComParam, this node's SAE J1939 NAME.
const CP_J1939_NAME: u32 = 0x8094;

/// D-PDU `CP_J1939AddrClaimTimeout` ComParam ID
/// (`service_params::PARAM_J1939_ADDR_CLAIM_TIMEOUT`, 0x8051): the
/// per-candidate bounded wait (microseconds) `run_j1939_claim_loop` uses --
/// staged large in ADR-180 Decision 21's own regression test so a
/// `CancelComPrimitive` sent mid-wait can be deterministically distinguished
/// from an ordinary timeout.
const CP_J1939_ADDR_CLAIM_TIMEOUT: u32 = 0x8051;

/// D-PDU `CP_J1939TargetAddress` ComParam ID
/// (`service_params::PARAM_J1939_TARGET_ADDRESS`, 0x808C): the message
/// destination address; `0xFFFF` is the "not configured" sentinel
/// `comparam_defaults.rs`'s `j1939_can_common` seeds by default, rejected
/// at `StartComPrimitive` time (ADR-179 Decision 4).
const CP_J1939_TARGET_ADDRESS: u32 = 0x808C;

/// D-PDU `CP_J1939PDUFormat` ComParam ID
/// (`service_params::PARAM_J1939_PDU_FORMAT`, 0x8089): the outgoing 29-bit
/// CAN identifier's PDU Format byte (`tx_header::j1939_header_bytes` byte
/// 1), a one-byte wire field.
const CP_J1939_PDU_FORMAT: u32 = 0x8089;

/// D-PDU `CP_J1939PDUSpecific` ComParam ID
/// (`service_params::PARAM_J1939_PDU_SPECIFIC`, 0x808A): the outgoing PDU
/// Specific byte (used for PDU2/group-broadcast addressing,
/// `tx_header::j1939_header_bytes` byte 2), a one-byte wire field.
const CP_J1939_PDU_SPECIFIC: u32 = 0x808A;

/// D-PDU `CP_MessagePriority` ComParam ID
/// (`service_params::PARAM_MESSAGE_PRIORITY`, 0x8083): packed into byte 0
/// bits 4-2 (`priority & 0x07`) of `tx_header::j1939_header_bytes`'s
/// composed 29-bit CAN identifier -- a 3-bit wire field, ADR-179 Decision 6
/// round-11 amendment.
const CP_MESSAGE_PRIORITY: u32 = 0x8083;

/// D-PDU `CP_J1939DataPage` ComParam ID (`service_params::PARAM_J1939_DATA_PAGE`,
/// 0x8087): its low bit is packed into byte 0 bit 0
/// (`data_page & 0x01`) of `tx_header::j1939_header_bytes`'s composed
/// 29-bit CAN identifier -- a 1-bit wire field, ADR-179 Decision 6 round-11
/// amendment.
const CP_J1939_DATA_PAGE: u32 = 0x8087;

/// D-PDU `CP_J1939SourceAddress` ComParam ID
/// (`service_params::PARAM_J1939_SOURCE_ADDRESS`, 0x808B): `PDU_PC_UNIQUE_ID`
/// class as of ADR-184 -- UniqueRespIdTable-only, rejected by
/// `Get`/`SetComParam` and used by RX routing (`events_rx_routing.rs::
/// UniqueRespIdKey::matched`) to disambiguate the responding ECU's SAE
/// J1939 source address (the low byte of the received 29-bit CAN id).
const CP_J1939_SOURCE_ADDRESS: u32 = 0x808B;

/// D-PDU `CP_J1939SourceName` ComParam ID
/// (`service_params::PARAM_J1939_SOURCE_NAME`, 0x8096): deliberately NOT
/// `PDU_PC_UNIQUE_ID` class (ADR-184's P3 residual) -- still a plain
/// bytefield ComParam, still rejected by `SetUniqueRespIdTable` (never
/// `PDU_PC_UNIQUE_ID` for J1939).
const CP_J1939_SOURCE_NAME: u32 = 0x8096;

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every J1939 resource row requires this opt-in (`names.rs`'s dedicated arm
/// in `resolve_pin_selection`, mirroring the SWCAN/FTCAN/UART Echo Byte/
/// Honda DIAG-H/J1708 opt-in gates).
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

/// Builds a `RscData` resource selecting `protocol_id` (a raw hardware
/// protocol id, OR a resource-table id resolved the same way -- see
/// `names.rs::resolve_pin_selection`) via the `ProtocolId` route, with the
/// given typed `(pin_number, pin_type_name)` pairs as `dlc_pin_data` -- same
/// shape as `j1708.rs`'s own `resource_with_protocol_id_and_pins`.
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

/// Creates (but does not connect) a J1939 CLL for `resource_id` (0x025D)
/// with the given explicit `dlc_pin_data` -- clause 16.3.2.1 keeps the
/// physical layer pin-unassigned until an explicit
/// `SET_CONFIG(CONFIG_J1962_PINS)`, so unlike every prior standalone
/// protocol this file's resource rows carry no default pins to fall back to
/// (ADR-179 Decision 2); `dlc_pin_data` is required even when the resource
/// id resolves a matching row.
async fn create_j1939_cll(
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
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// Creates and connects a J1939 CLL, staging `unum32_params`/
/// `bytefield_params` via `SetComParam` before `ConnectComLogicalLink` (so
/// they are already part of the Active snapshot an ordinary, non-
/// `temp_param_update` `CoptStartcomm` binds from -- ADR-067, mirroring
/// every other helper in this suite that stages params ahead of connect).
async fn create_and_connect_j1939_cll(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_id: u32,
    pins: &[(u32, &str)],
    unum32_params: &[(u32, u32)],
    bytefield_params: &[(u32, Vec<u8>)],
    cll_tag: u64,
) -> ComLogicalLinkHandle {
    let cll_handle = create_j1939_cll(client, resource_id, pins, cll_tag).await;

    for &(com_param_id, value) in unum32_params {
        set_com_param_unum32(client, cll_handle, com_param_id, value).await;
    }
    for (com_param_id, value) in bytefield_params {
        set_com_param_bytes(client, cll_handle, *com_param_id, value.clone()).await;
    }

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

/// Issues `CoptStartcomm` with no optional message and no
/// `temp_param_update` (the ordinary Active-snapshot path, ADR-067).
async fn start_comm(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: ComLogicalLinkHandle,
) -> Result<ComPrimitiveHandle, tonic::Status> {
    // Returns the cop handle so a wait can match this StartComm's own Finished instead of
    // a stale one still queued on the CLL (e.g. from promote_via_update_param's
    // CoptUpdateparam), which would let the caller send before the source address is set.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .map(|r| {
            r.into_inner()
                .cop_handle
                .expect("cop_handle should be present")
        })
}

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- same per-file local helper shape as
/// `repeat_message.rs`'s/`honda_diagh.rs`'s own (not shared via
/// `harness.rs`, matching this codebase's existing convention). Needed here
/// for round 9's `PDU_IOCTL_START_REPEAT_MESSAGE` regression tests (ADR-179
/// Decision 10 / ADR-180 Decision 10).
async fn resolve_ioctl_id(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    name: &str,
) -> u32 {
    client
        .get_object_id(GetObjectIdRequest {
            object_type: ObjectType::ObjtIoCtrl as i32,
            shortname: name.to_string(),
        })
        .await
        .unwrap_or_else(|err| panic!("get_object_id({name}) should succeed: {err}"))
        .into_inner()
        .pdu_object_id
}

/// Issues `IoCtl` against a `cll_handle` with an optional `input_data` and
/// `has_output` flag, returning the raw `output_data` on success. Mirrors
/// `repeat_message.rs`'s own `io_ctl_cll`.
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

/// Builds a `bytearray_data` (`IOBytearray`) `input_data` for
/// `PDU_IOCTL_START_REPEAT_MESSAGE`, packed via `harness::pack_repeat_message_setup`
/// (ADR-178's byte layout). Mirrors `repeat_message.rs`'s own
/// `repeat_message_setup`.
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

/// Issues `PDU_IOCTL_START_REPEAT_MESSAGE` and extracts the returned `MsgId`
/// from `output_data` (`unum32_value`, ADR-165 Decision 2). Mirrors
/// `repeat_message.rs`'s own `start_repeat_message`.
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

/// Polls `GetStatus(cop_handle)` until it reports `PduCopstExecuting`, or
/// panics after ~2s -- makes "the claim wait is actually in flight" a
/// structural precondition rather than a timing guess (mirrors
/// `locks_and_param_classes.rs`'s/`cop_ctrl_cycles.rs`'s identical
/// `wait_for_cop_executing`/`wait_for_cop_status` helpers, not shared across
/// files per this suite's existing per-file-duplication convention).
async fn wait_for_cop_executing(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cop_handle: ComPrimitiveHandle,
) {
    for _ in 0..200 {
        let response = client
            .get_status(GetStatusRequest {
                handle: Some(get_status_request::Handle::CopHandle(cop_handle)),
            })
            .await
            .expect("get_status(COP) should succeed")
            .into_inner();
        if let Some(status_response::Status::CopStatus(s)) = response.status
            && s == PduComPrimitiveStatus::PduCopstExecuting as i32
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("cop_handle {cop_handle:?} did not reach PduCopstExecuting within ~2s");
}

/// The standard two-pin (CAN_H/CAN_L on the J1962 connector, pins 6/14)
/// selection this file's tests use -- clause 6.3.3.2 documents no closed
/// pin table for J1939 (mirroring J1708's own "no closed-set pin
/// validation" precedent), so any well-formed pin pair is accepted; these
/// happen to be the real-world CAN_H/CAN_L pins.
const CAN_PINS: &[(u32, &str)] = &[(6, "HI"), (14, "LOW")];

/// Round-18 fix (Codex review, PR #72): SAE J1939-21's 29-bit extended CAN
/// identifier is unconditional -- `rpc_link::connect_flags` had no
/// `PROTOCOL_J1939_PS` arm at all, so a J1939 connect fell through to the
/// catch-all `0`, the same `Flags` an 11-bit-only channel would connect
/// with. Proves the connect now carries `CAN_29BIT_ID` (`0x100`), never
/// `CAN_ID_BOTH` (unlike raw CAN, ADR-065 -- J1939 never sends 11-bit
/// frames, so accepting them too would be wrong).
#[tokio::test]
#[serial]
async fn connecting_a_j1939_cll_sets_the_can_29bit_id_connect_flag() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    // 0x100 = CAN_29BIT_ID
    assert_eq!(server.backdoor.connect_flags_log(), vec![0x100]);

    server.shutdown().await;
}

/// Round-18 fix (Codex review, PR #72), second half: `can_filter_tx_flags`
/// only recognized raw `CAN` before this fix, so even with `connect_flags`
/// now correctly carrying `CAN_29BIT_ID` for J1939, the pass-all filter
/// `ConnectComLogicalLink` installs was still built at `TxFlags = 0` --
/// matching 11-bit CAN Ids only and leaving the channel deaf to its own
/// 29-bit J1939 traffic. Proves the installed filter now carries
/// `TX_EXTENDED_ID`, and that exactly one filter is installed (never the
/// two-filter `CAN_ID_BOTH` shape raw CAN uses -- J1939's own `connect_flags`
/// arm never sets that bit).
#[tokio::test]
#[serial]
async fn connecting_a_j1939_cll_installs_a_29bit_pass_all_filter() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::TX_EXTENDED_ID
    );

    server.shutdown().await;
}

/// Regression test (this PR's brief, generalizing the Honda DIAG-H/SAE J1708
/// `rpc_create_com_logical_link` fallback arms to SAE J1939 via
/// `resources::bustype_default_name_for_hw_protocol_id`): naming the raw
/// `J1939_PS` hardware protocol id directly (bypassing the resource table
/// entirely, `resource_row == None`) with the mandatory explicit
/// `dlc_pin_data` (clause 16.3.2.1's own no-default-pin model) but no
/// caller-supplied `bus_type_name` and no `SetComParam(DATA_RATE, ...)`
/// staged must still receive the 250_000bps SAE_J1939_11_DWCAN default --
/// before this fix, the Working ComParamSet stayed empty on this route and
/// `PassThruConnect` received a native baud rate of 0.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_seeds_the_250000_baud_rate_default() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_j1939_cll(
        &mut client,
        j2534_0404::PROTOCOL_J1939_PS,
        CAN_PINS,
        &[],
        &[],
        1,
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        250_000,
        "the raw-id no-resource-row J1939 connect route must still receive the 250_000bps \
         SAE_J1939_11_DWCAN default, not the empty Working set's default of 0"
    );

    server.shutdown().await;
}

/// Sibling regression test: the same raw `J1939_PS` connect, but WITH a
/// caller-supplied, unrelated, well-formed `bus_type_name`
/// (`RscData.bus_type`/`.protocol` are independent fields with no cross-
/// validation) -- this link's own fixed bustype identity must win, not the
/// mismatched name's own bustype defaults.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_ignores_a_mismatched_bus_type_name() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![
                        PinData {
                            dlc_pin_number: 6,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "HI".to_string(),
                                ),
                            ),
                        },
                        PinData {
                            dlc_pin_number: 14,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "LOW".to_string(),
                                ),
                            ),
                        },
                    ],
                    bus_type: Some(resource_data::BusType::BusTypeName(
                        "ISO_14230_1_UART".to_string(),
                    )),
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_J1939_PS,
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        250_000,
        "a mismatched bus_type_name must not override this J1939 link's own SAE_J1939_11_DWCAN \
         250_000bps default"
    );

    server.shutdown().await;
}

/// Item 1 (ADR-179 Decision 3's key behavioral proof): staging
/// `CP_J1939PreferredAddress`/`CP_J1939Name`, then `CoptStartcomm`,
/// completes successfully (`PduCopstFinished`, not `PduCopstCancelled`) and
/// claims the one staged candidate address -- confirmed observably via a
/// subsequent `CoptSendrecv` frame's source-address byte (see this file's
/// module doc on why `GetComParam(CP_TesterSourceAddress)` cannot be used
/// here).
#[tokio::test]
#[serial]
async fn connecting_and_starting_comm_claims_the_preferred_address() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_J1939_PS
    );
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        250_000,
        "SAE J2534-2 clause 16's 250 kbps default (SAE_J1939_11_DWCAN) should apply"
    );

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish (PduCopstFinished), not hang or get cancelled"
    );

    drop(events);

    send_data(&mut client, cll_handle, vec![0xAA, 0xBB, 0xCC], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.get(3).copied(),
        Some(0x80),
        "the one staged CP_J1939PreferredAddress candidate (0x80) should have been claimed and \
         used as the source-address byte of a subsequent send; got {written:#04x?}"
    );

    server.shutdown().await;
}

/// Item 2 (retry-over-the-candidate-list on a LOST first candidate) --
/// see this file's module doc for the inherent timing caveat.
/// `__mock_set_j1939_claim_lost(true)` is set before `CoptStartcomm`, forcing
/// the first `CP_J1939PreferredAddress` candidate (0x80) to lose, and
/// flipped back to `false` immediately once the RPC ack returns -- letting
/// the second candidate (0x81) claim successfully.
#[tokio::test]
#[serial]
async fn claim_retries_the_next_candidate_after_the_first_is_lost() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80, 0x81]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    server.backdoor.set_j1939_claim_lost(true);
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    // Flip back to `false` as soon as this test can possibly act, so the
    // SECOND candidate's own IOCTL_PROTECT_J1939_ADDR issue (which the
    // service dispatches back-to-back once the first candidate's Lost
    // outcome resolves, ~1 POLL_INTERVAL_MS/10ms later) evaluates under the
    // cleared toggle. See this file's module doc for why this cannot be a
    // hard guarantee with the mock's current (global, not per-address)
    // toggle.
    server.backdoor.set_j1939_claim_lost(false);

    let mut finished = false;
    let mut saw_init_error = false;
    wait_for_event(&mut events, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtInitError as i32 =>
            {
                saw_init_error = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "CoptStartcomm should finish -- if this fails, the retry likely raced past both \
         candidates before the backdoor was cleared (see this file's module doc)"
    );
    assert!(
        !saw_init_error,
        "the claim should have succeeded on the second candidate, not exhausted the list"
    );

    drop(events);

    send_data(&mut client, cll_handle, vec![0xAA, 0xBB, 0xCC], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.get(3).copied(),
        Some(0x81),
        "the SECOND candidate (0x81), not the first (0x80), should have been claimed; got \
         {written:#04x?}"
    );

    server.shutdown().await;
}

/// Item 3: every candidate losing (a single-candidate list, forced to lose
/// via the backdoor for the whole test -- no mid-flight timing dependency,
/// unlike item 2 above) exhausts the retry list and fails the StartComm COP:
/// `PduErrEvtInitError` followed by `PduCopstFinished`, mirroring
/// `startcomm_comparam.rs`'s own K-line init-failure assertion shape
/// (ADR-077).
#[tokio::test]
#[serial]
async fn claim_exhaustion_fails_startcomm_with_init_error() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    server.backdoor.set_j1939_claim_lost(true);

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await.expect(
        "start_com_primitive(CoptStartcomm) should succeed synchronously -- claim exhaustion \
             surfaces asynchronously as PduErrEvtInitError, not a synchronous error",
    );

    let mut saw_init_error = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "the exhausted CoptStartcomm should still finish (PduCopstFinished), not hang"
    );
    assert!(
        saw_init_error,
        "claim-list exhaustion should emit PduErrEvtInitError"
    );

    drop(events);
    server.shutdown().await;
}

/// edge-case-hunter finding (post-implementation verification pass): an
/// unstaged `CP_J1939Name` resolves to all-zero bytes (`resolve_j1939_claim_
/// params`'s own zero-padding of an absent Bytefield ComParam), which is
/// clause 16.3.3.2's wire-level CANCEL form of `PROTECT_J1939_ADDR`, not a
/// claim -- the mock's own `is_cancel` check confirms this by never queuing
/// an indication for it. Without a fast-fail guard, every candidate in
/// `CP_J1939PreferredAddress` would silently issue a no-op cancel, time out
/// one by one, and exhaust the list with no diagnostic pointing at the real
/// cause (an unconfigured NAME, not an unlucky claim). `run_j1939_claim_loop`
/// now checks for this and fails immediately -- this test proves the
/// immediate-failure shape (no `set_j1939_claim_lost` backdoor involved,
/// unlike the genuine-exhaustion test above, so a regression back to the
/// old per-candidate-timeout behavior would make this test's own 2-second
/// `wait_for_event` deadline meaningfully tighter to hit, not just wrong in
/// principle).
#[tokio::test]
#[serial]
async fn unconfigured_name_fails_the_claim_immediately_not_via_timeout() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        // Deliberately no CP_J1939_NAME staged -- resolves to all-zero.
        &[(CP_J1939_PREFERRED_ADDRESS, vec![0x80])],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut saw_init_error = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "an unconfigured-NAME claim should still finish (PduCopstFinished), not hang"
    );
    assert!(
        saw_init_error,
        "an unconfigured (all-zero) CP_J1939Name should fail the claim with PduErrEvtInitError"
    );

    drop(events);
    server.shutdown().await;
}

/// Item 4 (ADR-179 Decision 4): `CP_J1939TargetAddress` left at its seeded
/// default (`0xFFFF`, the "not configured" sentinel -- `comparam_defaults.rs`'s
/// `j1939_can_common`) fails `StartComPrimitive` synchronously, mirroring
/// the UART Echo Byte `cop_data` length check's assertion shape
/// (`rpc_primitive.rs`, right below this new gate) -- an `InvalidArgument`
/// `Status`, not a queued COP that fails later.
#[tokio::test]
#[serial]
async fn target_address_sentinel_rejects_startcomprimitive_synchronously() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[],
        &[],
        1,
    )
    .await;

    let status = start_comm(&mut client, cll_handle)
        .await
        .expect_err("CP_J1939TargetAddress left at the 0xFFFF sentinel must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("0xFFFF"),
        "the rejection should explain the 0xFFFF sentinel: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 5 (ADR-179 Decision 4): the native `ERR_ADDRESS_NOT_CLAIMED` write
/// failure (clause 16.5 -- a payload over 8 bytes sent with a source
/// address the device does not currently have claimed) is not duplicated
/// service-side; the native write's failure return is authoritative and
/// surfaces through this service's normal write-failure event path
/// (`PduErrEvtTxError`, mirroring every other native `PassThruWriteMsgs`
/// failure -- `events.rs::transmit_request` maps every write failure to
/// this same event, discarding the specific native status code; there is
/// no ERR_ADDRESS_NOT_CLAIMED-specific `PduError`/event variant to assert
/// against instead). `CP_J1939AddressNegotiationRule` bit 1 is set here so
/// `CoptStartcomm` completes WITHOUT claiming any address -- the default
/// (unclaimed) `NODE_ADDRESS` (0xF1) is then used as the source address,
/// which the mock has never added to its claimed set.
#[tokio::test]
#[serial]
async fn unclaimed_source_address_over_8_bytes_fails_the_send() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_ADDR_NEG_RULE, 0b10), // bit 1 set: do not negotiate
            (CP_J1939_TARGET_ADDRESS, 0x00),
        ],
        &[],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed even with no claim requested");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish without ever attempting a claim"
    );

    // A 9-byte payload (> clause 16.5's 8-byte threshold) sent with the
    // still-default, never-claimed NODE_ADDRESS (0xF1).
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01; 9],
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
        .expect("start_com_primitive(CoptSendrecv) should be accepted synchronously");

    let mut saw_tx_error = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtTxError as i32
            ) {
                saw_tx_error = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "the rejected send should still finish (PduCopstFinished), not hang"
    );
    assert!(
        saw_tx_error,
        "ERR_ADDRESS_NOT_CLAIMED should surface as PduErrEvtTxError, the generic write-failure \
         event every native PassThruWriteMsgs failure maps to"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the mock rejects the whole batch before writing/echoing anything on \
         ERR_ADDRESS_NOT_CLAIMED"
    );

    drop(events);
    server.shutdown().await;
}

/// Item 6 (ADR-179 Decision 6's key behavioral proof): `tx_header::
/// j1939_header_bytes`'s composed 5-byte prefix (29-bit CAN ID bytes 0-2,
/// claimed source address byte 3, `CP_J1939TargetAddress` byte 4) reaches
/// the wire ahead of the client payload, byte for byte.
#[tokio::test]
#[serial]
async fn message_header_reaches_the_wire_byte_for_byte() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        // "ISO_OBD_on_SAE_J1939_73" overrides CP_MessagePriority to 0
        // (comparam_defaults.rs::iso_obd_on_sae_j1939_73), and
        // CP_J1939DataPage/_PDUFormat/_PDUSpecific default to 0 --
        // byte0 = (0&7)<<2 | (0&1) = 0x00; byte1 = pdu_format = 0x00;
        // pdu_format (0) < 240, so byte2 = target_address truncated =
        // 0x10.
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x90]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    // Issue the send only after StartComm's own PduCopstFinished, via a
    // short subscribe/wait pair -- otherwise the send could race the claim
    // and use the pre-claim default source address instead.
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    // StartComm has very likely already finished (claims resolve in ~10ms);
    // wait_for_event returns immediately if the event already landed in the
    // CLL's queue (ADR-115 -- a fresh subscription still drains any
    // already-queued items), otherwise waits for it live.
    wait_for_event(&mut events, 2000, |item| {
        matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        )
    })
    .await;
    drop(events);

    send_data(&mut client, cll_handle, vec![0xAA, 0xBB, 0xCC], vec![]).await;

    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written,
        vec![0x00, 0x00, 0x10, 0x90, 0x10, 0xAA, 0xBB, 0xCC],
        "expected [byte0, pdu_format, ps_byte, claimed_source, target_address] + payload; got \
         {written:#04x?}"
    );

    server.shutdown().await;
}

/// Item 7 (lighter version, per this file's module doc / the brief's own
/// documented fallback): the mock's `j1939_claimed_addresses` set is not
/// exposed to this test harness, so this cannot directly observe that CLL
/// A's disconnect actually issued the cancel form of
/// `IOCTL_PROTECT_J1939_ADDR` for its claimed address. What IS observable:
/// two sibling CLLs sharing one physical channel (same resource id, same
/// pins), CLL A claims an address and then disconnects, and CLL B -- which
/// stayed connected throughout -- remains healthy (a plain `GetComParam`
/// still succeeds) and the physical channel itself is never torn down
/// (`disconnect_count` stays 0, `DestroyComLogicalLink`/last-owner teardown
/// never ran).
#[tokio::test]
#[serial]
async fn disconnect_while_sibling_survives_does_not_error_or_disturb_the_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x92]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;
    let cll_b = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[],
        &[],
        2,
    )
    .await;

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL B should share one physical channel (same resource id/pins)"
    );

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptStartcomm should finish and claim its one candidate address"
    );
    drop(events_a);

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect(
            "disconnecting CLL A while CLL B still shares the physical channel should not error \
             or hang",
        );

    assert_eq!(
        server.backdoor.disconnect_count(),
        0,
        "the physical channel must survive CLL A's disconnect while CLL B still holds it"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE).await,
        250_000,
        "CLL B should remain healthy (still able to GetComParam) after CLL A's disconnect"
    );

    server.shutdown().await;
}

/// Codex review finding #1 (PR #72 round 1): `header_footer_len` (ADR-051)
/// had no `PROTOCOL_J1939_PS` arm and fell through to the `_ => (0, 0)`
/// catch-all, so `tx_header::j1939_header_bytes`'s own 5-byte TX prefix
/// (4-byte 29-bit CAN identifier + 1-byte destination address, clause 16.4.3)
/// was never mirrored on the RX side: every ordinary J1939 response exposed
/// its own CAN-ID/destination prefix as if it were payload in
/// `ResultData.data_bytes`. This proves the fix: an injected raw frame is
/// split into a 5-byte `extra_info.header_bytes` and a payload-only
/// `data_bytes`, mirroring `rx_header_split.rs`'s own per-protocol
/// convention (this file, not that one, per this file's module doc's own
/// precedent of duplicating small per-protocol test helpers locally).
#[tokio::test]
#[serial]
async fn rx_prefix_is_split_into_extra_info_not_leaked_into_payload() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish before the receive-only monitor is armed"
    );
    drop(events);

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let header = [0x18, 0x00, 0x21, 0x80, 0x21];
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let mut frame = header.to_vec();
    frame.extend_from_slice(&payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::PROTOCOL_J1939_PS);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &header, &[], &payload);

    server.shutdown().await;
}

/// ADR-184: `SetUniqueRespIdTable` on a J1939 CLL now ACCEPTS
/// `CP_J1939SourceAddress` (previously rejected outright,
/// `PDU_ERR_COMPARAM_NOT_SUPPORTED`, for every unum32/bytefield param --
/// `comparam_support::unique_id_params` had no `J1939_PS` branch) and still
/// REJECTS `CP_J1939SourceName` (out of scope this pass, deliberately
/// unadvertised -- see `comparam_support.rs`'s own `CAN_UNIQUE_ID_BYTES`/
/// `J1939_UNIQUE_ID_UNUM32` doc comments).
#[tokio::test]
#[serial]
async fn unique_resp_id_table_accepts_j1939_source_address_and_rejects_source_name() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[],
        1,
    )
    .await;

    client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![ecu_entry(
                    1,
                    vec![unum32_param(CP_J1939_SOURCE_ADDRESS, 0x21)],
                )],
            }),
        })
        .await
        .expect("CP_J1939SourceAddress should now be accepted (ADR-184)");

    let status = client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![ecu_entry(
                    1,
                    vec![ParamItem {
                        id: Some(param_item::Id::ParamId(CP_J1939_SOURCE_NAME)),
                        com_param_class: vci_service_interface::PduParamClass::PduPcUniqueId as i32,
                        param_data: Some(param_item::ParamData::Bytefield(vec![
                            1, 2, 3, 4, 5, 6, 7, 8,
                        ])),
                    }],
                )],
            }),
        })
        .await
        .expect_err("CP_J1939SourceName should still be rejected (out of scope, P3 residual)");
    assert_eq!(status.code(), Code::InvalidArgument);
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrComparamNotSupported as i32
    );

    server.shutdown().await;
}

/// ADR-184: `Get`/`SetComParam` on a J1939 CLL now REJECTS
/// `CP_J1939SourceAddress` directly -- it moved to `PDU_PC_UNIQUE_ID` class
/// (UniqueRespIdTable-only, ISO 22900-2 §9.3.3.6), unlike its pre-ADR-184
/// acceptance via `is_j1939_param`'s allowlist.
#[tokio::test]
#[serial]
async fn set_com_param_rejects_j1939_source_address_directly() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[],
        1,
    )
    .await;

    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(CP_J1939_SOURCE_ADDRESS)),
                com_param_class: vci_service_interface::PduParamClass::PduPcUniqueId as i32,
                param_data: Some(param_item::ParamData::Unum32(0x21)),
            }),
        })
        .await
        .expect_err(
            "CP_J1939SourceAddress should be rejected by SetComParam (ADR-184: \
             PDU_PC_UNIQUE_ID class, UniqueRespIdTable-only)",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrComparamNotSupported as i32
    );

    server.shutdown().await;
}

/// ADR-184: a `SetUniqueRespIdTable` entry keyed by `CP_J1939SourceAddress`
/// routes a frame whose CAN id's low byte matches the configured SA, and
/// drops one that does not -- the routing gap this ADR closes.
#[tokio::test]
#[serial]
async fn unique_resp_id_table_source_address_routes_matching_frame_and_drops_others() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish before the receive-only monitor is armed"
    );
    drop(events);

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            5,
            vec![unum32_param(CP_J1939_SOURCE_ADDRESS, 0x21)],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    // Matching: byte 3 (source address) is 0x21.
    let matching_header = [0x18, 0x00, 0x00, 0x21, 0xFF];
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let mut matching_frame = matching_header.to_vec();
    matching_frame.extend_from_slice(&payload);
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &matching_frame,
        j2534_0404::PROTOCOL_J1939_PS,
    );
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_eq!(result.unique_resp_identifier, 5);
    assert_result_data(&result, &matching_header, &[], &payload);

    // Non-matching: byte 3 (source address) is 0x22 -- must be dropped, not
    // wildcard-delivered (a table IS configured for this CLL).
    let non_matching_header = [0x18, 0x00, 0x00, 0x22, 0xFF];
    let mut non_matching_frame = non_matching_header.to_vec();
    non_matching_frame.extend_from_slice(&payload);
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &non_matching_frame,
        j2534_0404::PROTOCOL_J1939_PS,
    );
    assert_no_result_data(
        &mut client,
        cll_handle,
        "a frame whose source address does not match the configured \
         CP_J1939SourceAddress entry must be dropped",
    )
    .await;

    server.shutdown().await;
}

/// ADR-184 accepted residual (ISO 22900-2's own NOTE on
/// `CP_J1939SourceAddress`'s `[0, 0xFFFF]` spec range, an 11-bit-CAN-ID
/// URID-assignment mode this codebase's J1939 CLLs never reach -- they
/// always connect flat `CAN_29BIT_ID`, `rpc_link.rs`'s own `connect_flags`
/// derivation): a table entry configuring an out-of-byte-range SA (here
/// `0x180`, spec-legal but `> 0xFF`) can never match any real frame's CAN id
/// low byte. This is DELIBERATE, not a bug -- an explicitly-configured
/// table is an explicit filter, distinct from the empty-table wildcard case
/// (`route_frame`'s own no-table "deliver unconditionally" mode); do not
/// "fix" this back to wildcard delivery. Every injected frame is dropped,
/// regardless of its own source-address byte.
#[tokio::test]
#[serial]
async fn unique_resp_id_table_unmatchable_extended_source_address_drops_every_frame() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish before the receive-only monitor is armed"
    );
    drop(events);

    // 0x180 is spec-legal (within [0, 0xFFFF]) but exceeds the one-byte
    // range a real frame's low byte can ever carry -- unmatchable in
    // practice.
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            5,
            vec![unum32_param(CP_J1939_SOURCE_ADDRESS, 0x180)],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    // Even a frame whose low byte happens to equal 0x180's own low byte
    // (0x80) does not match -- 0x180 != 0x80, and no real frame can ever
    // carry a CAN id whose low byte is 0x180.
    let header = [0x18, 0x00, 0x00, 0x80, 0xFF];
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let mut frame = header.to_vec();
    frame.extend_from_slice(&payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::PROTOCOL_J1939_PS);

    assert_no_result_data(
        &mut client,
        cll_handle,
        "an explicitly-configured table with an unmatchable (>0xFF) \
         CP_J1939SourceAddress must drop every frame, not wildcard-deliver",
    )
    .await;

    server.shutdown().await;
}

/// Codex review finding #3 (PR #72 round 1): before the
/// `owned_by_a_live_sibling` guard in `run_j1939_claim_loop`, two sibling
/// CLLs sharing one physical channel with overlapping
/// `CP_J1939PreferredAddress` lists would let the SECOND CLL's claim
/// attempt unconditionally overwrite the FIRST CLL's already-live
/// `j1939_claims` routing entry for the same address -- hijacking
/// address-claim-indication routing away from its rightful, already-
/// successful owner. This proves the fix: CLL B's candidate list starts
/// with the address CLL A already claimed; B must skip it without ever
/// attempting to claim it, landing on its own second candidate instead --
/// observed via each CLL's own subsequent `CoptSendrecv` source-address
/// byte (see this file's module doc on why `GetComParam(CP_TesterSourceAddress)`
/// cannot be used here).
#[tokio::test]
#[serial]
async fn sibling_cll_skips_an_address_a_live_sibling_already_claimed() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x50]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;
    let cll_b = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x50, 0x51]),
            (CP_J1939_NAME, vec![8, 7, 6, 5, 4, 3, 2, 1]),
        ],
        2,
    )
    .await;
    // CLL B joins the physical channel CLL A already opened; per ADR-044's
    // "shared-channel creator-decides rule" (see `finalize_connected_link`'s
    // own doc comment), only the CHANNEL CREATOR's Working set is promoted
    // to Active at connect time -- a joining CLL's own Active stays default
    // until it issues its own `CoptUpdateparam`, so CLL B's staged
    // `CP_J1939PreferredAddress`/`CP_J1939Name` need this explicit promotion
    // before `CoptStartcomm` can see them.
    promote_via_update_param(&mut client, cll_b).await;

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL B should share one physical channel (same resource id/pins)"
    );

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptStartcomm should finish and claim 0x50"
    );
    drop(events_a);

    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL B");
    let mut saw_init_error_b = false;
    assert!(
        wait_for_event(&mut events_b, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error_b = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "CLL B's CoptStartcomm should finish, having skipped 0x50 and claimed 0x51"
    );
    assert!(
        !saw_init_error_b,
        "CLL B's claim should succeed on its second candidate (0x51), not exhaust the list"
    );
    drop(events_b);

    send_data(&mut client, cll_a, vec![0xAA], vec![]).await;
    let written_a = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written_a.get(3).copied(),
        Some(0x50),
        "CLL A must keep its own claimed address 0x50, not have it overwritten by CLL B's claim \
         attempt; got {written_a:#04x?}"
    );

    send_data(&mut client, cll_b, vec![0xBB], vec![]).await;
    let written_b = server.backdoor.written_data(MOCK_CHANNEL_ID, 1);
    assert_eq!(
        written_b.get(3).copied(),
        Some(0x51),
        "CLL B must skip 0x50 (owned by a live sibling) and claim 0x51 instead; got \
         {written_b:#04x?}"
    );

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 3): before this fix, a claim-enabled
/// J1939 `CoptStartcomm`'s optional message (`cop_data`) was silently
/// dropped -- `tx` was resolved (at `StartComPrimitive` call time, before
/// the claim loop even ran) but then discarded rather than sent, since its
/// pre-composed header carried the pre-claim default source address
/// (`0xF1`), not the address the claim was about to win. This proves the
/// fix: the optional message now reaches the wire, with its 5-byte
/// `tx_header::j1939_header_bytes` prefix recomposed against the
/// JUST-claimed address rather than the stale pre-claim default.
#[tokio::test]
#[serial]
async fn optional_startcomm_message_sends_with_the_claimed_source_address() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x88]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0xAA, 0xBB],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm) with a claim-enabled optional message should \
             succeed synchronously",
        );

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish after claiming and sending the optional message"
    );
    drop(events);

    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    // "ISO_OBD_on_SAE_J1939_73" overrides CP_MessagePriority to 0
    // (comparam_defaults.rs::iso_obd_on_sae_j1939_73, same as
    // `message_header_reaches_the_wire_byte_for_byte` above), so
    // byte0 = (0 & 7) << 2 | (data_page(0, default) & 1) = 0x00;
    // pdu_format defaults to 0 (< 240, PDU1), so byte2 = target address
    // (0x10); byte3 = the claimed source address (0x88), NOT the pre-claim
    // default (0xF1); byte4 = target address (0x10) again.
    assert_eq!(
        written,
        vec![0x00, 0x00, 0x10, 0x88, 0x10, 0xAA, 0xBB],
        "expected [byte0, pdu_format, ps_byte, claimed_source, target_address] + the optional \
         message payload; got {written:#04x?}"
    );

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 4, P1): before this fix, a
/// StopComm -> `CP_J1939PreferredAddress` change -> StartComm cycle only
/// reset the CLL's own local `j1939_claim_cursor`/`j1939_claimed_address`
/// markers, leaving the address it had already claimed still registered in
/// `SharedChannel::j1939_claims` -- and still defended by the adapter --
/// forever (cancellation previously only ran from disconnect/destroy
/// teardown or a failed attempt, neither of which a StopComm/StartComm
/// cycle triggers). This proves the fix by making the leaked address's
/// effect on a SIBLING CLL observable end to end (this file's module doc's
/// documented pattern -- there is no direct backdoor into
/// `SharedChannel::j1939_claims`): CLL A claims 0x80, StopComm, moves its
/// own candidate to 0x81 and StartComm's again -- if 0x80 were still
/// registered to A, sibling CLL B's own single-candidate `[0x80]` claim
/// attempt would find it `owned_by_a_live_sibling` and immediately exhaust
/// (`PduErrEvtInitError`, no successful claim) without ever issuing a
/// native attempt for it. With the fix, A's second StartComm cancels and
/// deregisters 0x80 before claiming 0x81, so B's claim on 0x80 succeeds.
#[tokio::test]
#[serial]
async fn restarting_after_stopcomm_frees_the_previously_claimed_address_for_a_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;
    let cll_b = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![8, 7, 6, 5, 4, 3, 2, 1]),
        ],
        2,
    )
    .await;
    // CLL B joins the physical channel CLL A already opened -- ADR-044's
    // "shared-channel creator-decides rule" (see
    // `sibling_cll_skips_an_address_a_live_sibling_already_claimed` above)
    // means B's own staged candidate list needs this explicit promotion
    // before either of its `CoptStartcomm` calls can see it.
    promote_via_update_param(&mut client, cll_b).await;

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL B should share one physical channel (same resource id/pins)"
    );

    // -- CLL A: claim 0x80, confirmed via a send. --
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's first attempt");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's first CoptStartcomm should finish and claim 0x80"
    );

    send_data(&mut client, cll_a, vec![0xAA], vec![]).await;
    let written_first = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written_first.get(3).copied(),
        Some(0x80),
        "CLL A should have claimed 0x80 on its first attempt; got {written_first:#04x?}"
    );
    // Drain this CoptSendrecv's own PduCopstFinished from `events_a` before
    // dropping it -- per ADR-115, a per-CLL event queue persists across
    // subscribe/drop cycles, so leaving this undrained would let a LATER
    // fresh subscription's "any Finished" wait match this stale event
    // instead of the operation it actually meant to wait for (this bit an
    // earlier revision of this exact test: every later `wait_for_event`
    // ended up off by one, eventually letting the final assertion below
    // race ahead of CLL A's second claim actually completing).
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's first CoptSendrecv should finish"
    );
    drop(events_a);

    // -- CLL A: StopComm, move its own candidate to 0x81, StartComm again. --
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, empty cop_data) should succeed on CLL A");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptStopcomm should finish"
    );

    set_com_param_bytes(&mut client, cll_a, CP_J1939_PREFERRED_ADDRESS, vec![0x81]).await;
    promote_via_update_param(&mut client, cll_a).await;
    // Drain CoptUpdateparam's own PduCopstFinished from `events_a` (which has
    // stayed open since before StopComm) before waiting for the SECOND
    // CoptStartcomm's own Finished below -- otherwise `wait_for_event`'s
    // generic "any Finished" predicate would match CoptUpdateparam's
    // already-queued Finished event first, well before the second claim
    // actually completes.
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptUpdateparam should finish"
    );

    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's second attempt");
    let mut saw_init_error_a = false;
    assert!(
        wait_for_event(&mut events_a, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error_a = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "CLL A's second CoptStartcomm should finish and claim 0x81"
    );
    assert!(
        !saw_init_error_a,
        "CLL A's second claim attempt (0x81) should succeed, not exhaust"
    );
    drop(events_a);

    send_data(&mut client, cll_a, vec![0xCC], vec![]).await;
    let written_second = server.backdoor.written_data(MOCK_CHANNEL_ID, 1);
    assert_eq!(
        written_second.get(3).copied(),
        Some(0x81),
        "CLL A should have moved to 0x81 on its second attempt; got {written_second:#04x?}"
    );

    // -- CLL B: claim 0x80 -- only possible if A's re-StartComm cancelled
    // and deregistered its own stale 0x80 entry. --
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let startcomm_cop = start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL B");
    let mut saw_init_error_b = false;
    assert!(
        wait_for_event(&mut events_b, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error_b = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ) && item.cop_handle.as_ref().map(|c| c.cop_handle) == Some(startcomm_cop.cop_handle)
        })
        .await,
        "CLL B's CoptStartcomm should finish and claim 0x80, now that CLL A has moved off it"
    );
    assert!(
        !saw_init_error_b,
        "CLL B should successfully claim 0x80 -- if this fails, CLL A's stale claim on 0x80 was \
         never cancelled/deregistered when it moved to 0x81 (the P1 regression this test \
         guards against)"
    );
    drop(events_b);

    send_data(&mut client, cll_b, vec![0xEE], vec![]).await;
    let written_third = server.backdoor.written_data(MOCK_CHANNEL_ID, 2);
    assert_eq!(
        written_third.get(3).copied(),
        Some(0x80),
        "CLL B should have claimed 0x80, freed by CLL A's earlier move to 0x81; got \
         {written_third:#04x?}"
    );

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 5): `deliver_j1939_claim_indication`'s
/// "later, spontaneous `_LOST`" case (ADR-179 Decision 3) armed a fresh
/// reclaim from `j1939_claim_cursor = 0` but never removed the just-lost
/// address's own entry from `SharedChannel::j1939_claims` -- unlike an
/// in-flight wait's own `Lost` outcome, whose post-wait cleanup in
/// `run_j1939_claim_loop` already does. If the fresh reclaim lands on an
/// EARLIER candidate than the one just lost (exactly what this test
/// constructs), the lost address's stale entry survives forever, blocking
/// any sibling CLL from it. This proves the fix by injecting a raw
/// `RX_FLAG_J1939_ADDRESS_LOST` frame (`events.rs::RX_J1939_ADDRESS_LOST`,
/// `0x0002_0000`) via the backdoor -- the only way to synthesize a
/// spontaneous loss for a SPECIFIC address outside of an active
/// `protect_j1939_addr` wait, since `__mock_set_j1939_claim_lost` is a
/// global, not per-address, toggle.
#[tokio::test]
#[serial]
async fn spontaneous_loss_frees_the_lost_address_for_a_sibling_after_an_earlier_candidate_reclaims()
{
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    // CLL B claims 0x50 first (the channel creator), so CLL A's own initial
    // claim -- candidates [0x50, 0x51] -- skips 0x50 (owned by a live
    // sibling) and lands on 0x51.
    let cll_b = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x50]),
            (CP_J1939_NAME, vec![8, 7, 6, 5, 4, 3, 2, 1]),
        ],
        1,
    )
    .await;
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL B");
    assert!(
        wait_for_event(&mut events_b, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL B's CoptStartcomm should finish and claim 0x50"
    );
    drop(events_b);

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x50, 0x51]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        2,
    )
    .await;
    // Joining CLL -- ADR-044's creator-decides rule, same as the sibling
    // test above.
    promote_via_update_param(&mut client, cll_a).await;

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL B should share one physical channel (same resource id/pins)"
    );

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let startcomm_cop = start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A");
    let mut saw_init_error_a = false;
    assert!(
        wait_for_event(&mut events_a, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error_a = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ) && item.cop_handle.as_ref().map(|c| c.cop_handle) == Some(startcomm_cop.cop_handle)
        })
        .await,
        "CLL A's CoptStartcomm should finish, having skipped 0x50 and claimed 0x51"
    );
    assert!(
        !saw_init_error_a,
        "CLL A's claim should succeed on its second candidate (0x51), not exhaust the list"
    );
    drop(events_a);

    send_data(&mut client, cll_a, vec![0xAA], vec![]).await;
    let written_first = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written_first.get(3).copied(),
        Some(0x51),
        "CLL A should have skipped 0x50 (owned by CLL B) and claimed 0x51; got \
         {written_first:#04x?}"
    );

    // CLL B disconnects, freeing 0x50 -- before CLL A's spontaneous loss
    // below, so A's fresh reclaim (starting from cursor 0) finds 0x50
    // available and lands there instead of retrying 0x51.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect(
            "disconnecting CLL B while CLL A still shares the physical channel should not error",
        );
    assert_eq!(
        server.backdoor.disconnect_count(),
        0,
        "the physical channel must survive CLL B's disconnect while CLL A still holds it"
    );

    // Synthesize a spontaneous RX_FLAG_J1939_ADDRESS_LOST for 0x51 (CLL A's
    // currently claimed address) -- no active `run_j1939_claim_loop` wait is
    // in flight for A at this point, so this exercises `deliver_j1939_claim_
    // indication`'s own spontaneous-loss path, not the ordinary in-flight
    // `Lost` path `run_j1939_claim_loop`'s ITS OWN post-wait cleanup already
    // handles correctly.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x51],
        j2534_0404::PROTOCOL_J1939_PS,
        0x0002_0000, // RX_FLAG_J1939_ADDRESS_LOST, events.rs::RX_J1939_ADDRESS_LOST
    );

    // No client-visible event marks the spontaneous reclaim's completion
    // (it is not COP-driven) -- give the channel poll task's periodic
    // `run_due_tick_duties` several ticks (POLL_INTERVAL_MS = 10ms) to pick
    // up `j1939_reclaim_pending` and run the fresh claim loop to
    // completion.
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    send_data(&mut client, cll_a, vec![0xBB], vec![]).await;
    let written_second = server.backdoor.written_data(MOCK_CHANNEL_ID, 1);
    assert_eq!(
        written_second.get(3).copied(),
        Some(0x50),
        "CLL A's spontaneous reclaim should have landed on the now-free 0x50 (cursor reset to \
         0); got {written_second:#04x?}"
    );

    // The key check: a NEW sibling CLL C, with 0x51 as its only candidate,
    // must be able to claim it -- only possible if A's spontaneous-loss
    // handling actually removed 0x51's stale entry from `SharedChannel::
    // j1939_claims` when it reclaimed 0x50 instead of retrying 0x51.
    let cll_c = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x51]),
            (CP_J1939_NAME, vec![2, 4, 6, 8, 1, 3, 5, 7]),
        ],
        3,
    )
    .await;
    promote_via_update_param(&mut client, cll_c).await;

    let mut events_c = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_c)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let startcomm_cop = start_comm(&mut client, cll_c)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL C");
    let mut saw_init_error_c = false;
    assert!(
        wait_for_event(&mut events_c, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error_c = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ) && item.cop_handle.as_ref().map(|c| c.cop_handle) == Some(startcomm_cop.cop_handle)
        })
        .await,
        "CLL C's CoptStartcomm should finish and claim 0x51, now that CLL A has moved off it"
    );
    assert!(
        !saw_init_error_c,
        "CLL C should successfully claim 0x51 -- if this fails, CLL A's stale claim on 0x51 was \
         never cancelled/deregistered by the spontaneous-loss handling (the round-5 regression \
         this test guards against)"
    );
    drop(events_c);

    send_data(&mut client, cll_c, vec![0xCC], vec![]).await;
    let written_third = server.backdoor.written_data(MOCK_CHANNEL_ID, 2);
    assert_eq!(
        written_third.get(3).copied(),
        Some(0x51),
        "CLL C should have claimed 0x51, freed by CLL A's spontaneous loss; got \
         {written_third:#04x?}"
    );

    server.shutdown().await;
}

// Codex review round 6 raised two further findings on this same mechanism:
// (1) `tester_present_data` (built at `StartComPrimitive` call time,
// ADR-067, against the pre-claim default source address) has the identical
// staleness problem `tx.send.data` had before ADR-180 Decision 3, and (2) a
// live `TesterPresentState::Armed`'s cached `framed_data` is never touched
// by `run_j1939_reclaim_duties`'s own `Claimed` arm, so a spontaneous
// reclaim onto a different address would leave tester-present transmitting
// under the lost address forever. Both fixes are implemented (mirroring
// ADR-180 Decision 3's own recomposition, in `handle_start_comm`'s J1939
// `Claimed` arm and `run_j1939_reclaim_duties`'s own `Claimed` arm
// respectively) -- but while writing regression tests for them, both proved
// UNREACHABLE via the live RPC surface: `comparam_support::is_j1939_param`
// (J1939's closed ComParam allowlist) does not include
// `CP_TesterPresentHandling`/`CP_TesterPresentMessage`/`CP_TesterPresentTime`/
// `CP_TesterPresentSendType` at all, so `SetComParam` rejects every attempt
// to configure them on a J1939 CLL with `PDU_ERR_COMPARAM_NOT_SUPPORTED`
// (confirmed directly: attempting to set `CP_TesterPresentTime` in an
// earlier draft of this test failed exactly that way) -- and
// `comparam_defaults.rs::j1939_can_common` never seeds
// `PARAM_TESTER_PRESENT_HANDLING` either, so `ComParamSet::
// tester_present_handling()` always falls back to its own `0` (disabled)
// default. `resolve_tester_present` therefore always short-circuits at its
// very first check for a J1939 CLL, so `tester_present_data` is always
// empty in practice and neither of these two fixes' own code paths can
// currently be exercised by any real client. This is the same "found
// unreachable while writing a regression test, documented instead of
// adding a misleading one" precedent this file used earlier for the
// `PARAM_J1939_SOURCE_ADDRESS`-based routing gap (that gap, along with
// the matching `SetUniqueRespIdTable`-acceptance side, was later closed
// by ADR-184's `CllRxEntry::unique_resp_ids`/`unique_id_params` work,
// making it independently testable -- see this file's own
// `unique_resp_id_table_source_address_routes_matching_frame_and_drops_others`
// and its siblings).

/// Codex review finding (PR #72 round 6): `CP_J1939TargetAddress` is a
/// one-byte wire field (`target_address as u8` in `j1939_header_bytes`) --
/// an out-of-range value other than the `0xFFFF` "not configured" sentinel
/// (already rejected, see `target_address_sentinel_rejects_startcomprimitive_synchronously`
/// above) previously passed the StartComPrimitive-time gate silently and
/// was truncated at cast time. This proves the fix: `0x100` is rejected
/// synchronously, the same shape as the sentinel rejection.
#[tokio::test]
#[serial]
async fn target_address_over_one_byte_rejects_startcomprimitive_synchronously() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x100)],
        &[],
        1,
    )
    .await;

    let status = start_comm(&mut client, cll_handle)
        .await
        .expect_err("CP_J1939TargetAddress over one byte (0x100) must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("must fit in one byte"),
        "the rejection should explain the one-byte requirement: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 7): the StartComPrimitive-time
/// `CP_J1939TargetAddress` gate (both the `0xFFFF` sentinel check and the
/// round-6 one-byte-range check above) is not repeated when a running
/// J1939 CLL re-stages `CP_J1939TargetAddress` and promotes it live via
/// `CoptUpdateparam` -- the next `CoptSendrecv` would then read the
/// unvalidated Active value straight through `j1939_header_bytes`'s
/// `target_address as u8` cast. This proves the fix: an out-of-range
/// value (`0x100`) staged and promoted via `CoptUpdateparam` is rejected
/// synchronously, the same shape as the StartComPrimitive-time gate.
#[tokio::test]
#[serial]
async fn coptupdateparam_rejects_target_address_over_one_byte() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[],
        1,
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_J1939_TARGET_ADDRESS, 0x100).await;

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
            "CoptUpdateparam promoting an out-of-range CP_J1939TargetAddress (0x100) must be \
             rejected",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("must fit in one byte"),
        "the rejection should explain the one-byte requirement: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Codex review finding (PR #72): `CP_J1939Name` is a fixed 8-byte
/// Bytefield (clause 16.3.3.2's NAME field) -- a wrong-length staged value
/// previously passed silently, zero-padded/truncated by
/// `resolve_j1939_claim_params` to fit the native `[u8; 8]` claim
/// parameter, so the adapter claimed/arbitrated with a NAME the client's
/// own `GetComParam` readback disagreed with. This proves the fix: a
/// 5-byte NAME is rejected synchronously at `CoptStartcomm`, the same
/// shape as `CP_J1939TargetAddress`'s own one-byte-range rejection.
#[tokio::test]
#[serial]
async fn j1939_name_wrong_length_rejects_startcomprimitive_synchronously() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[(CP_J1939_NAME, vec![1, 2, 3, 4, 5])],
        1,
    )
    .await;

    let status = start_comm(&mut client, cll_handle)
        .await
        .expect_err("CP_J1939Name with a wrong length (5 bytes) must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("must be exactly 8 bytes"),
        "the rejection should explain the eight-byte requirement: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Codex review finding (PR #72), `CoptUpdateparam` counterpart to the test
/// above: the StartComPrimitive-time `CP_J1939Name` length gate is not
/// repeated when a running J1939 CLL re-stages `CP_J1939Name` and promotes
/// it live via `CoptUpdateparam` -- a later spontaneous-loss reclaim
/// (`run_j1939_reclaim_duties`) reads Active directly, bypassing the
/// StartComPrimitive gate entirely (no new StartComPrimitive call happens
/// for a spontaneous reclaim). This proves the fix: a 10-byte NAME staged
/// and promoted via `CoptUpdateparam` is rejected synchronously.
#[tokio::test]
#[serial]
async fn coptupdateparam_rejects_j1939_name_wrong_length() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[(CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8])],
        1,
    )
    .await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_J1939_NAME,
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
    )
    .await;

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
            "CoptUpdateparam promoting a wrong-length CP_J1939Name (10 bytes) must be rejected",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("must be exactly 8 bytes"),
        "the rejection should explain the eight-byte requirement: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 7), sentinel counterpart to the test
/// above: `CoptUpdateparam` promoting `CP_J1939TargetAddress` back to the
/// `0xFFFF` "not configured" sentinel on an ALREADY-STARTED CLL is rejected
/// the same way the StartComPrimitive-time gate rejects it. The CLL must
/// genuinely be started first -- `edge-case-hunter` (verification pass on
/// this same round's diff) found an earlier version of both this fix and
/// this test gated the sentinel rejection on `is_j1939_protocol_id` alone,
/// with no `comm_started` check, which incorrectly rejected an ordinary
/// `CoptUpdateparam` staging an unrelated ComParam on a connected-but-not-
/// yet-started CLL (where `CP_J1939TargetAddress` is still legitimately at
/// its documented default). See `coptupdateparam_before_startcomm_with_
/// unrelated_param_is_not_rejected_by_the_sentinel_guard` below for the
/// regression coverage on that not-yet-started case.
#[tokio::test]
#[serial]
async fn coptupdateparam_rejects_target_address_sentinel() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    // CP_J1939AddressNegotiationRule bit 1 set: skip claim negotiation
    // entirely (same as `unclaimed_source_address_over_8_bytes_fails_the_
    // send` above) -- this test only needs `comm_started` to become `true`
    // via a genuinely-finished StartComm, not an actual address claim.
    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_ADDR_NEG_RULE, 0b10),
            (CP_J1939_TARGET_ADDRESS, 0x10),
        ],
        &[],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle)
        .await
        .expect("a valid CP_J1939TargetAddress must let StartComm succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish (setting comm_started = true) before this test re-stages \
         CP_J1939TargetAddress"
    );
    drop(events);

    // Re-stage CP_J1939TargetAddress back to the 0xFFFF sentinel and
    // attempt to promote it live via CoptUpdateparam on the now-started CLL.
    set_com_param_unum32(&mut client, cll_handle, CP_J1939_TARGET_ADDRESS, 0xFFFF).await;

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
            "CoptUpdateparam promoting the CP_J1939TargetAddress 0xFFFF sentinel on an \
             already-started CLL must be rejected",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("0xFFFF"),
        "the rejection should explain the 0xFFFF sentinel: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 7, `edge-case-hunter` verification
/// pass): the `CP_J1939TargetAddress == 0xFFFF` sentinel rejection above
/// must NOT fire for a CLL that has never called `CoptStartcomm` at all --
/// `0xFFFF` is `CP_J1939TargetAddress`'s own documented default
/// (`comparam_defaults.rs::j1939_can_common`), so it is the normal,
/// legitimate value for every ComParam this CLL has not yet started
/// communicating with. An unrelated `CoptUpdateparam` (staging
/// `CP_J1939PreferredAddress`, never touching `CP_J1939TargetAddress`)
/// on a connected-but-not-yet-started J1939 CLL must succeed, the same
/// pattern `comparam_tx.rs::iso15765_plain_sendrecv_respects_queued_
/// updateparam_ordering` already exercises for a different protocol. An
/// earlier version of the round-7 fix rejected this unconditionally
/// (gated on `is_j1939_protocol_id` alone, with no `comm_started` check),
/// which this test would have caught.
#[tokio::test]
#[serial]
async fn coptupdateparam_before_startcomm_with_unrelated_param_is_not_rejected_by_the_sentinel_guard()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    // `CP_J1939TargetAddress` is left at its seeded 0xFFFF default; this
    // CLL never calls StartComm.
    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[],
        &[],
        1,
    )
    .await;

    // Stage an unrelated ComParam -- CP_J1939PreferredAddress is a plain
    // claim-candidate list entry, never read by the sentinel/range checks.
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_J1939_PREFERRED_ADDRESS,
        vec![0x20],
    )
    .await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptUpdateparam staging an unrelated ComParam on a not-yet-started J1939 CLL, \
             with CP_J1939TargetAddress still at its legitimate 0xFFFF default, must succeed",
        );

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 8): `CP_TesterSourceAddress`
/// (`NODE_ADDRESS`) is now client-writable on a J1939 CLL (the
/// `is_j1939_param` allowlist fix documented in this file's own module
/// doc comment above), and `tx_header::tester_addr` narrows it with `as
/// u8` the same way `j1850_header_bytes`/`kwp_header_bytes` narrow their
/// own addressing bytes -- but the pre-existing J1850/KWP-scoped
/// out-of-range guard in `rpc_set_com_param` never covered J1939. This
/// proves the fix: an out-of-range value (`0x180`) is rejected
/// synchronously by `SetComParam` itself, before it can ever reach the
/// wire.
#[tokio::test]
#[serial]
async fn set_com_param_rejects_out_of_range_tester_source_address_on_j1939() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[],
        &[],
        1,
    )
    .await;

    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    j2534_0404::NODE_ADDRESS,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(0x180)),
            }),
        })
        .await
        .expect_err("SetComParam(CP_TesterSourceAddress=0x180) on a J1939 CLL should be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("CP_TesterSourceAddress"),
        "the rejection should name CP_TesterSourceAddress: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 8): `CP_J1939PDUFormat`/
/// `CP_J1939PDUSpecific` are one-byte wire fields
/// (`tx_header::j1939_header_bytes` narrows each with `as u8`), the same
/// truncation-vs-rejection shape `CP_J1939TargetAddress` already closed in
/// rounds 6/7, but neither had an upper-range guard in `rpc_set_com_param`.
/// Proves both are now rejected synchronously.
#[tokio::test]
#[serial]
async fn set_com_param_rejects_out_of_range_pdu_format_and_specific_on_j1939() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[],
        &[],
        1,
    )
    .await;

    for (param_id, name) in [
        (CP_J1939_PDU_FORMAT, "CP_J1939PDUFormat"),
        (CP_J1939_PDU_SPECIFIC, "CP_J1939PDUSpecific"),
    ] {
        let status = client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(param_id)),
                    com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(0x1EA)),
                }),
            })
            .await
            .expect_err(&format!("SetComParam({name}=0x1EA) should be rejected"));
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(
            status.message().contains(name),
            "the rejection should name {name}: {}",
            status.message()
        );
    }

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 8, `design-advisor` consult): a
/// claim-enabled `CoptStartcomm` carrying an optional message can have its
/// post-claim transmit sequence cancelled (here: mid-receive-phase, via
/// `CancelComPrimitive`) without the just-succeeded J1939 claim ever being
/// cancelled -- `comm_started` is only set in the sequence's own success
/// tail, which cancellation never reaches, so the adapter kept defending
/// the address and the routing entry kept blocking sibling CLLs. Mirrors
/// `startcomm_optional_message_tx.rs::can_optional_message_cancel_during_
/// receive_phase_cancels_before_comm_started`'s own cancellation shape, but
/// proves the J1939-specific consequence the same way this file's other
/// cancel-owner tests do (module doc's documented pattern, since there is
/// no direct backdoor into `SharedChannel::j1939_claims`): CLL A claims
/// 0x80 via a StartComm whose optional message waits on a response that
/// never arrives; cancelling it must free 0x80 for sibling CLL B to claim.
#[tokio::test]
#[serial]
async fn cancelling_the_optional_startcomm_message_after_a_successful_claim_frees_it_for_a_sibling()
{
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;
    let cll_b = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![8, 7, 6, 5, 4, 3, 2, 1]),
        ],
        2,
    )
    .await;
    // ADR-044's "shared-channel creator-decides rule" (see
    // `sibling_cll_skips_an_address_a_live_sibling_already_claimed`): B's
    // own staged candidate list needs this explicit promotion before its
    // own `CoptStartcomm` can see it.
    promote_via_update_param(&mut client, cll_b).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL B should share one physical channel (same resource id/pins)"
    );

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // CP_TesterSourceAddress is the claimed 0x80 by the time this transmits
    // (recomposed by ADR-180 Decision 3); the mock never answers, so the
    // receive phase blocks until this test cancels it.
    let start_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x01],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 1,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC1],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm, claim-enabled optional message) should succeed \
             synchronously",
        )
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Cancel as soon as the transmit is observed, rather than first
    // asserting "still waiting" the way `startcomm_optional_message_tx.rs`'s
    // own CAN analog does (`CP_P2Max` staged to 3 s there) -- `CP_P2Max` is
    // NOT in J1939's closed `is_j1939_param` allowlist (`comparam_support.rs`),
    // so it cannot be extended past its fixed 200 ms seeded default
    // (`comparam_defaults.rs::j1939_can_common`) here, and an extra wait
    // this close to that ceiling would race the receive phase's own
    // (non-fatal, COMM_STARTED-reaching) timeout rather than reliably
    // precede it.
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(start_cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "CancelComPrimitive during the optional message's receive phase should cancel it"
    );
    drop(events_a);

    // CLL B (candidate [0x80]) can only successfully claim 0x80 if CLL A's
    // claim was actually cancelled and deregistered -- otherwise B's own
    // attempt finds it `owned_by_a_live_sibling` and exhausts immediately.
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let startcomm_cop = start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL B");
    assert!(
        wait_for_event(&mut events_b, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ) && item
            .cop_handle
            .as_ref()
            .map(|c| c.cop_handle)
            == Some(startcomm_cop.cop_handle))
        .await,
        "CLL B's CoptStartcomm should finish and claim 0x80 -- only possible if CLL A's \
         cancelled claim was actually released"
    );
    drop(events_b);

    send_data(&mut client, cll_b, vec![0xBB], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 1);
    assert_eq!(
        written.get(3).copied(),
        Some(0x80),
        "CLL B should have claimed 0x80, freed by CLL A's cancelled StartComm; got \
         {written:#04x?}"
    );

    server.shutdown().await;
}

// Codex review finding (PR #72 round 7): `run_j1939_claim_loop` now skips
// straight to the next candidate for a reserved address (254/255, clause
// 16.3.3.2) without ever issuing the native `PROTECT_J1939_ADDR` IOCTL --
// matching ADR-179 Decision 3's own documented design intent ("never
// byte[0] = 255... both 254 and 255 return ERR_INVALID_IOCTL_VALUE if ever
// issued as an explicit claim target"), which the original implementation
// never actually enforced. **No dedicated regression test**: the mock's own
// `IOCTL_PROTECT_J1939_ADDR` handler (`j2534-0404-mock/src/lib.rs`) already
// rejects 254/255 synchronously with `ERR_INVALID_IOCTL_VALUE` and no side
// effects (no counter, no `rx_queue` push -- confirmed by reading the
// handler directly), so from this harness's observable surface (wire
// writes, timing, event sequencing) skipping the candidate locally and
// issuing-then-rejecting natively are indistinguishable; there is no mock
// counter exposed for "was `PROTECT_J1939_ADDR` invoked" to tell them
// apart. Verified correct by code inspection and by confirming the mock's
// own `protect_j1939_addr_rejects_254_and_255` unit test (independently
// pinning the native-side behavior this fix no longer relies on) and the
// full J1939 integration suite still pass unchanged.

/// ADR-179 Decision 9 (design-advisor consult, Codex review PR #72 round 9,
/// Finding 1): `tx_header::response_header_bytes`'s new J1939 arm wildcards
/// bytes 0-2 (priority/PGN) and byte 4 (destination) but exact-matches byte
/// 3 (the responding ECU's own source address, this CLL's
/// `CP_J1939TargetAddress`). Proves both halves in one flow: a frame with
/// the WRONG source address never stops the slot even when its PDU Format
/// byte happens to equal this CLL's own configured `CP_J1939PDUFormat`, and
/// a frame with a DIFFERENT PDU Format/Specific/destination than anything
/// this CLL ever configured DOES stop it as long as the source address
/// matches -- confirming bytes 0-2/4 are genuinely wildcarded, not merely
/// defaulted to a value this fixture happens to satisfy.
#[tokio::test]
#[serial]
async fn repeat_slot_stop_condition_wildcards_pgn_and_destination_exact_matches_source_address() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x0A), (CP_J1939_PDU_FORMAT, 0xEE)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    // ADR-180 Decision 13 (round 11): `PDU_IOCTL_START_REPEAT_MESSAGE` now
    // requires a completed claim on a negotiation-enabled CLL -- wait for
    // the claim to genuinely finish (via `SubscribeEvent`/`PduCopstFinished`)
    // before starting the slot below, since `start_com_primitive`'s own RPC
    // call returns once the COP is enqueued, not once the claim negotiation
    // actually finishes on the poll task (the same timing lesson this
    // file's own `CoptUpdateparam`/repeat-message tests already apply).
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim 0x80");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80"
    );
    drop(events);

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a claimed J1939 CLL");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Wrong source address (0x0B, not this CLL's own 0x0A) but the SAME PDU
    // Format byte (0xEE) this CLL's own CP_J1939PDUFormat configures -- must
    // NOT stop the slot, proving byte 3 is exact-matched, not wildcarded.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x18, 0xEE, 0xFF, 0x0B, 0xFF, 0x99],
        j2534_0404::PROTOCOL_J1939_PS,
        0x100, // RX_FLAG_CAN_29BIT_ID
    );
    assert!(
        !wait_for_repeat_status(&server, msg_id, 0, 300).await,
        "a frame with the wrong source address (byte 3) must never stop the slot, regardless \
         of PDU Format/Specific agreement"
    );

    // Right source address (0x0A) but a DIFFERENT PDU Format/Specific
    // (0x00/0x00) and destination (0x00) than anything this CLL configured
    // -- must stop the slot, proving bytes 0-2/4 are genuinely wildcarded.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x00, 0x00, 0x00, 0x0A, 0x00, 0x99],
        j2534_0404::PROTOCOL_J1939_PS,
        0x100, // RX_FLAG_CAN_29BIT_ID
    );
    assert!(
        wait_for_repeat_status(&server, msg_id, 0, 1000).await,
        "a frame matching only the responding ECU's source address (byte 3) must stop the \
         slot, proving bytes 0-2 (priority/PGN) and byte 4 (destination) are wildcarded \
         (Finding 1, ADR-179 Decision 9)"
    );

    server.shutdown().await;
}

/// ADR-180 Decision 10 (design-advisor consult, Codex review PR #72 round 9,
/// Finding 2's primary site): a spontaneous `RX_FLAG_J1939_ADDRESS_LOST`
/// must stop this CLL's own live SAE J2534-2 clause 14 repeat slots --
/// otherwise the device keeps retransmitting `RepeatMsgData[0]` under an
/// address this CLL (or a sibling that has since claimed it) no longer
/// holds, indefinitely. Mirrors `spontaneous_loss_frees_the_lost_address_
/// for_a_sibling_after_an_earlier_candidate_reclaims`'s injection shape.
#[tokio::test]
#[serial]
async fn spontaneous_loss_stops_the_clls_own_live_repeat_slots() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim 0x80");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80 before the repeat slot is started"
    );
    drop(events);

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 1, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a claimed J1939 CLL");
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "sanity: the slot should be live immediately after START"
    );

    // Synthesize a spontaneous RX_FLAG_J1939_ADDRESS_LOST for 0x80 (this
    // CLL's currently claimed address) -- no active `run_j1939_claim_loop`
    // wait is in flight, so this exercises `deliver_j1939_claim_indication`'s
    // own spontaneous-loss path (ADR-179 Decision 3/ADR-180 Decision 5).
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x80],
        j2534_0404::PROTOCOL_J1939_PS,
        0x0002_0000, // RX_FLAG_J1939_ADDRESS_LOST, events.rs::RX_J1939_ADDRESS_LOST
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
    let mut stopped = false;
    while std::time::Instant::now() < deadline {
        if server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id)
            .is_none()
        {
            stopped = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        stopped,
        "a spontaneous address loss must stop this CLL's own live repeat slots (Finding 2, \
         ADR-180 Decision 10) -- the device would otherwise keep transmitting under an \
         address it no longer holds"
    );

    server.shutdown().await;
}

/// ADR-186 Decision item 1 (design-advisor consult, third round): SAE
/// J2534's `<DataSize>` field never carries a "wire bytes transmitted"
/// carve-out anywhere in the spec family -- clause 21.2.2(h)/22.2.2(h)'s own
/// CAN-FD periodic-message cap counts the 4-byte CAN ID toward `<DataSize>`
/// even though it is never a wire "data" byte either, and clause 16's Table
/// 62 gives a 0-data-byte J1939 message a minimum `<DataSize>` of 5 despite
/// that message sitting squarely inside clause 16.4.4's own don't-care
/// regime for the destination byte. SAE J2534-2 clause 16 never restates a
/// periodic-message cap for J1939, so it inherits SAE J2534-1 §7.2.7's flat
/// 12-byte `<DataSize>` cap literally, the same as every other protocol
/// without its own clause 21.2.2(h)/22.2.2(h)-style restatement -- so
/// DataSize 12 (5-byte header + 7-byte payload) is J1939's correct
/// single-frame `PDU_IOCTL_START_REPEAT_MESSAGE` boundary, and DataSize 13
/// (8-byte payload) must stay rejected.
#[tokio::test]
#[serial]
async fn repeat_message_accepts_datasize_12_and_rejects_datasize_13_on_j1939() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x0A), (CP_J1939_PDU_FORMAT, 0xEE)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    // ADR-180 Decision 13 (round 11): `PDU_IOCTL_START_REPEAT_MESSAGE`
    // requires a completed claim on a negotiation-enabled CLL -- wait for
    // the claim to genuinely finish before starting either slot below (same
    // timing lesson this file's other repeat-message tests already apply).
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim 0x80");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80 before either repeat slot is started"
    );
    drop(events);

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    // DataSize 12 (5-byte header + 7-byte payload) is the flat SAE
    // J2534-1 §7.2.7 periodic-message boundary J1939 inherits, same as
    // every other protocol without its own clause 21/22-style restatement
    // -- must be ACCEPTED.
    start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 0, vec![0u8; 7], vec![], vec![]),
    )
    .await
    .expect(
        "a 7-byte J1939 payload (DataSize 12) is within the flat §7.2.7 12-byte periodic cap \
         and must be accepted",
    );

    // DataSize 13 (8-byte payload) exceeds the flat 12-byte cap -- must
    // stay REJECTED.
    let err = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 0, vec![0u8; 8], vec![], vec![]),
    )
    .await
    .expect_err(
        "an 8-byte J1939 payload (DataSize 13) exceeds the flat §7.2.7 12-byte periodic cap \
         and must be rejected",
    );
    assert_eq!(err.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// ADR-180 Decision 10 (design-advisor consult, Codex review PR #72 round 9,
/// Finding 2's sibling site): a fresh `StartComm`'s own claim reset
/// (ADR-180 Decision 4's site) must also stop the repeat slots this CLL
/// started under its PREVIOUSLY claimed address -- a `StopComm` -> preferred-
/// address change -> `StartComm` cycle voluntarily relinquishes the old
/// address via `cancel_j1939_claims_for_cll`, and without this fix the old
/// slots would keep transmitting under it forever. Mirrors
/// `restarting_after_stopcomm_frees_the_previously_claimed_address_for_a_sibling`'s
/// StopComm/reconfigure/StartComm sequence.
#[tokio::test]
#[serial]
async fn restarting_after_stopcomm_stops_the_previously_claimed_addresss_own_repeat_slots() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on the first attempt");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the first CoptStartcomm should finish and claim 0x80"
    );

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 1, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a claimed J1939 CLL");
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "sanity: the slot should be live immediately after START"
    );

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, empty cop_data) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStopcomm should finish"
    );

    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_J1939_PREFERRED_ADDRESS,
        vec![0x81],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;
    // Drain CoptUpdateparam's own PduCopstFinished before the fresh
    // StartComm below (ADR-115 persistent-queue gotcha, same as
    // `restarting_after_stopcomm_frees_the_previously_claimed_address_for_a_sibling`).
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptUpdateparam should finish"
    );

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on the second attempt");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the fresh CoptStartcomm should finish and claim 0x81"
    );

    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        None,
        "a fresh StartComm's own claim reset must stop the repeat slot this CLL started under \
         its PREVIOUSLY claimed address (0x80) -- Finding 2's sibling site, ADR-180 Decision 10"
    );

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// ADR-180 Decision 10 (edge-case-hunter finding, round 9 verification pass
/// on this same round's own diff): unlike Decision 4's fresh-StartComm-reset
/// site and Decision 5's spontaneous-loss site (both already covered),
/// `cancel_j1939_claim_after_failed_startcomm`'s own cancellation (Decision
/// 9's site -- reached here via `CancelComPrimitive` mid-receive-phase,
/// exactly the way `cancelling_the_optional_startcomm_message_after_a_successful_
/// claim_frees_it_for_a_sibling` above exercises it) cancels a claim that DID
/// complete, unlike Decision 2's own never-completed-claim timeout case --
/// `PDU_IOCTL_START_REPEAT_MESSAGE` requires only a connected channel, not
/// `comm_started`, so a client can legitimately start a repeat slot under the
/// freshly-claimed address while the optional message's own transmit
/// sequence is still in flight. Proves the repeat slot is now stopped too,
/// not merely left tracked at the mock's "still live" status.
#[tokio::test]
#[serial]
async fn cancelling_the_optional_startcomm_message_after_a_successful_claim_stops_its_own_repeat_slots()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // CP_TesterSourceAddress is the claimed 0x80 by the time this transmits
    // (recomposed by ADR-180 Decision 3); the mock never answers, so the
    // receive phase blocks until this test cancels it.
    let start_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x01],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 1,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC1],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm, claim-enabled optional message) should succeed \
             synchronously",
        )
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // The claim already succeeded (proven by the optional message's own
    // transmit above, framed with the claimed address) even though the COP
    // itself is still in its receive phase -- comm_started is false, but
    // PDU_IOCTL_START_REPEAT_MESSAGE does not require it. Start a repeat
    // slot now, under the not-yet-committed claim.
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 1, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed while the claim is in flight");
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "sanity: the slot should be live immediately after START"
    );

    // Cancel as soon as the transmit is observed -- see the sibling test's
    // own comment on why `CP_P2Max` cannot be extended for J1939 here.
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(start_cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "CancelComPrimitive during the optional message's receive phase should cancel it"
    );

    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        None,
        "cancelling the optional message after a successful-but-not-yet-committed claim must \
         stop the repeat slot started under it -- ADR-180 Decision 10's third site \
         (cancel_j1939_claim_after_failed_startcomm)"
    );

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// ADR-180 Decision 11 (design-advisor consult, Codex review PR #72 round
/// 10, Finding 1's primary site): `events.rs::handle_send_recv`'s own doc
/// comment establishes that every cycle of a multi-cycle/cyclic
/// `CoptSendrecv` retransmits the SAME `SendRecvTx::data` resolved once at
/// `StartComPrimitive` call time -- for J1939, already framed with the
/// claimed source address live at that moment. Proves a spontaneous
/// address loss now cancels a still-running cyclic `CoptSendrecv` instead
/// of letting it keep transmitting under the lost address forever.
#[tokio::test]
#[serial]
async fn spontaneous_loss_cancels_a_live_cyclic_send_recv_under_the_lost_address() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim 0x80");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80"
    );

    // A cyclic CoptSendrecv (ADR-053): NumSendCycles = -1 (infinite),
    // NumReceiveCycles = 0 (no response required), Time = 20ms between
    // cycles.
    let send_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 20,
                num_send_cycles: -1,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: vec![],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv, cyclic) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // Synthesize a spontaneous RX_FLAG_J1939_ADDRESS_LOST for 0x80 (this
    // CLL's currently claimed address) -- deliver_j1939_claim_indication's
    // own spontaneous-loss path.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x80],
        j2534_0404::PROTOCOL_J1939_PS,
        0x0002_0000, // RX_FLAG_J1939_ADDRESS_LOST, events.rs::RX_J1939_ADDRESS_LOST
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "the live cyclic CoptSendrecv must be cancelled once its CLL's claimed address is \
         spontaneously lost (Finding 1, ADR-180 Decision 11) -- otherwise it keeps \
         transmitting under an address this CLL no longer holds"
    );

    // Stability check, not just the event: confirm the write count actually
    // stops growing (mirrors the design-advisor consult's own caution that
    // one already-dispatched cycle may legitimately land before the loss is
    // processed on the single poll task, so this checks stability-after,
    // not an exact count).
    let count_after_cancel = server.backdoor.written_count(MOCK_CHANNEL_ID);
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        count_after_cancel,
        "no further cycles should transmit after the CoptSendrecv was cancelled"
    );

    let _ = send_cop_handle;
    drop(events);
    server.shutdown().await;
}

/// ADR-180 Decision 11's round-10 `edge-case-hunter` correction (found on
/// round 10's own diff, ADR-180 Decision 11's amended text): `CopEntry::
/// transmits`/`is_send_recv` are computed once, statically, at
/// `StartComPrimitive` call time and never updated afterward, so an
/// IS-CYCLIC-shaped `CoptSendrecv` (`NumSendCycles = 1`, `NumReceiveCycles =
/// -1` -- `events.rs:12197`'s `migrate_on_first_match` condition) that has
/// since finished its one send cycle and detached to ADR-100 tier-2
/// (`ReceivePhaseOutcome::DetachedToTier2`, a pure receive-only
/// `RegistrantTier::ReceiveOnly` registrant) still read `transmits ==
/// true`/`is_send_recv == true` under the stale snapshot -- so
/// `cancel_send_recv_cops_for_cll` would wrongly cancel it on an unrelated
/// claim relinquishment even though it no longer puts anything on the bus
/// and has nothing stale to invalidate. Proves the fix (the same live
/// `RegistrantTier::ReceiveOnly` exclusion `rpc_misc.rs::
/// ioctl_clear_tx_queue`'s own ADR-100 S5 companion fix already applies):
/// migrate this COP to tier 2 via a real matched response, confirm it is
/// alive-but-detached, then synthesize the same spontaneous
/// `RX_FLAG_J1939_ADDRESS_LOST` the sibling test above uses and confirm it
/// does NOT receive `PduCopstCancelled`.
#[tokio::test]
#[serial]
async fn spontaneous_loss_does_not_cancel_a_detached_tier2_registrant_under_the_lost_address() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim 0x80");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80"
    );

    // NumSendCycles = 1 (finite send phase), NumReceiveCycles = -1
    // (IS-CYCLIC) -- exactly the shape `events.rs:12197`'s
    // `migrate_on_first_match` targets: a send phase that finishes, then a
    // receive phase that migrates to ADR-100 tier 2 on its first match.
    let send_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x22, 0xF1, 0x90],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: -1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 9,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x62],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv, migrate-on-first-match shape) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    // The claim itself does not transmit a WriteMsgs frame (it negotiates
    // via the native PROTECT_J1939_ADDR IOCTL) -- this is the COP's own
    // single send cycle.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A matching response: 5-byte J1939 header prefix (stripped from the
    // payload the same way `rx_prefix_is_split_into_extra_info_not_leaked_
    // into_payload` proves) plus a payload whose first byte (0x62) satisfies
    // the descriptor's mask/pattern above.
    let header = [0x18, 0x00, 0x21, 0x80, 0x21];
    let payload = vec![0x62, 0xF1, 0x90, 0x01];
    let mut frame = header.to_vec();
    frame.extend_from_slice(&payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::PROTOCOL_J1939_PS);

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 9
        ))
        .await,
        "the COP should accept its first (and only) response, migrating it to ADR-100 tier 2"
    );

    // Still alive -- only detached, not finished (mirrors
    // `cop_ctrl_cycles.rs::sendrecv_is_cyclic_detach_lets_a_sibling_cll_execute`'s
    // own proof that a tier-2 registrant does not finish on its own).
    assert!(
        !wait_for_event(&mut events, 200, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the detached tier-2 registrant must not finish on its own"
    );

    // Synthesize the same spontaneous RX_FLAG_J1939_ADDRESS_LOST for 0x80
    // the sibling test above uses.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x80],
        j2534_0404::PROTOCOL_J1939_PS,
        0x0002_0000, // RX_FLAG_J1939_ADDRESS_LOST, events.rs::RX_J1939_ADDRESS_LOST
    );

    // The actual regression check: a detached tier-2 registrant already
    // finished sending and has nothing stale to invalidate, so it must
    // survive a claim relinquishment on its own CLL (round-10
    // edge-case-hunter finding, ADR-180 Decision 11's amended filter).
    assert!(
        !wait_for_event(&mut events, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "a detached ADR-100 tier-2 CoptSendrecv registrant must NOT be cancelled by an unrelated \
         claim relinquishment on its CLL -- it already finished sending and poses none of the \
         staleness risk cancel_send_recv_cops_for_cll exists to close"
    );

    let _ = send_cop_handle;
    drop(events);
    server.shutdown().await;
}

/// ADR-180 Decision 12 (design-advisor consult, Codex review PR #72 round
/// 10, Finding 2): `CoptUpdateparam` promoting `CP_TesterSourceAddress`
/// (native `NODE_ADDRESS`) to a value DIFFERENT from the address this
/// CLL's live claim negotiation owns must be rejected -- otherwise every
/// subsequent transmission frames with a source the adapter is not
/// defending, while the adapter keeps defending the original claimed
/// address for a CLL that has stopped using it.
#[tokio::test]
#[serial]
async fn coptupdateparam_rejects_tester_source_address_off_the_live_claim() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim 0x80");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80 before CoptUpdateparam is issued"
    );
    drop(events);

    set_com_param_unum32(&mut client, cll_handle, j2534_0404::NODE_ADDRESS, 0x90).await;

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
            "CoptUpdateparam promoting CP_TesterSourceAddress off the live claim (0x80 -> 0x90) \
             must be rejected",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("CP_TesterSourceAddress"),
        "the rejection should name CP_TesterSourceAddress: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Negative counterpart to the test above: re-staging `CP_TesterSourceAddress`
/// back to the SAME value the live claim already owns (e.g. re-staging the
/// full ComParam set after a successful claim) must NOT be rejected --
/// proves the guard compares against the live Active value, not merely
/// whether the ComParam is staged at all.
#[tokio::test]
#[serial]
async fn coptupdateparam_allows_restaging_the_same_tester_source_address_as_the_live_claim() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim 0x80");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80 before CoptUpdateparam is issued"
    );
    drop(events);

    set_com_param_unum32(&mut client, cll_handle, j2534_0404::NODE_ADDRESS, 0x80).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptUpdateparam re-staging CP_TesterSourceAddress back to the already-claimed \
             value (0x80) must succeed",
        );

    server.shutdown().await;
}

/// Second negative counterpart: a J1939 CLL with NO address negotiation
/// requested at all (`CP_J1939AddressNegotiationRule` bit 1 set) never
/// registers a `SharedChannel::j1939_claims` entry, so `NODE_ADDRESS` stays
/// fully client-owned for it (round 8's own "made client-writable" change)
/// -- `CoptUpdateparam` promoting a new value must still succeed.
#[tokio::test]
#[serial]
async fn coptupdateparam_allows_tester_source_address_change_on_a_non_negotiated_cll() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_TARGET_ADDRESS, 0x00),
            (CP_J1939_ADDR_NEG_RULE, 0b10), // bit 1 set: negotiation NOT requested
        ],
        &[],
        1,
    )
    .await;
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed with no claim requested");

    set_com_param_unum32(&mut client, cll_handle, j2534_0404::NODE_ADDRESS, 0x55).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptUpdateparam changing CP_TesterSourceAddress on a non-negotiated J1939 CLL \
             (no live claim to drift from) must succeed",
        );

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 11, ADR-179 Decision 6 amendment):
/// `rpc_primitive.rs::apply_resolved_tx_flags` always cleared
/// `TX_EXTENDED_ID` for J1939 and re-derived it from
/// `tx_header::can_addressing_tx_flags(tx_header::resolve_can_addressing(..), ..)`
/// -- a CAN-family-only resolution keyed on `CP_CanPhysReqId`/
/// `CP_CanFuncReqId`, which J1939 never configures, so it always resolved
/// `None`/`0` and every J1939 send went out as an 11-bit transmission
/// despite clause 16.4.3's frames always being 29-bit. Proves an ordinary
/// `CoptSendrecv` frame is actually written to the wire with
/// `TX_EXTENDED_ID` set.
#[tokio::test]
#[serial]
async fn ordinary_send_sets_tx_extended_id() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x90]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    // Issue the send only after StartComm's own PduCopstFinished, mirroring
    // `message_header_reaches_the_wire_byte_for_byte`'s own ordering
    // rationale -- otherwise the send could race the claim.
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    wait_for_event(&mut events, 2000, |item| {
        matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        )
    })
    .await;
    drop(events);

    send_data(&mut client, cll_handle, vec![0xAA, 0xBB, 0xCC], vec![]).await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0) & j2534_0404::TX_EXTENDED_ID,
        j2534_0404::TX_EXTENDED_ID,
        "an ordinary J1939 CoptSendrecv frame must be written with TX_EXTENDED_ID set (clause \
         16.4.3: SAE J1939 frames are always 29-bit)"
    );

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 11, ADR-179 Decision 6 amendment):
/// `tx_header::j1939_header_bytes` packs `CP_MessagePriority` into byte 0
/// bits 4-2 (`priority & 0x07`), a 3-bit wire field, with no upper-range
/// guard in `SetComParam` -- an out-of-range value (e.g. `8`) silently
/// masked to a different in-range priority on the wire while `GetComParam`
/// kept reporting the untruncated value. Proves `SetComParam` now rejects
/// `8` synchronously, and pins the boundary: `7` (the top of the 3-bit
/// range) still succeeds.
#[tokio::test]
#[serial]
async fn coptsendrecv_rejects_out_of_range_message_priority() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[],
        &[],
        1,
    )
    .await;

    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    CP_MESSAGE_PRIORITY,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(8)),
            }),
        })
        .await
        .expect_err("SetComParam(CP_MessagePriority=8) on a J1939 CLL should be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("CP_MessagePriority"),
        "the rejection should name CP_MessagePriority: {}",
        status.message()
    );

    // Boundary: 7 (the top of the 3-bit priority field) must still succeed.
    set_com_param_unum32(&mut client, cll_handle, CP_MESSAGE_PRIORITY, 7).await;

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 11, ADR-179 Decision 6 amendment):
/// `tx_header::j1939_header_bytes` packs `CP_J1939DataPage`'s low bit into
/// byte 0 bit 0 (`data_page & 0x01`), a 1-bit wire field, with no
/// upper-range guard in `SetComParam` -- an out-of-range value (e.g. `2`)
/// silently masked to a different data page on the wire while
/// `GetComParam` kept reporting the untruncated value. Proves `SetComParam`
/// now rejects `2` synchronously, and pins the boundary: `1` (the top of
/// the 1-bit range) still succeeds.
#[tokio::test]
#[serial]
async fn coptsendrecv_rejects_out_of_range_data_page() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[],
        &[],
        1,
    )
    .await;

    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    CP_J1939_DATA_PAGE,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(2)),
            }),
        })
        .await
        .expect_err("SetComParam(CP_J1939DataPage=2) on a J1939 CLL should be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("CP_J1939DataPage"),
        "the rejection should name CP_J1939DataPage: {}",
        status.message()
    );

    // Boundary: 1 (the top of the 1-bit data-page field) still succeeds.
    set_com_param_unum32(&mut client, cll_handle, CP_J1939_DATA_PAGE, 1).await;

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 11, ADR-179 Decision 9 round-11
/// amendment): `tx_header::response_header_bytes`'s J1939 arm cast
/// `CP_J1939TargetAddress` to a byte (`addr as u8`) with no upper-bound
/// check of its own, unlike `StartComPrimitive`'s own round-6/7 gate this
/// arm's byte-3 resolution otherwise mirrors. `PDU_IOCTL_START_REPEAT_MESSAGE`
/// requires only a connected CLL, not `comm_started`, so this arm is
/// reachable before `StartComPrimitive`'s own range gate ever runs. Proves
/// an out-of-range `CP_J1939TargetAddress` (`0x100`) is now rejected
/// synchronously by the IOCTL itself, on a connected-but-not-comm_started
/// CLL.
#[tokio::test]
#[serial]
async fn repeat_message_setup_rejects_out_of_range_target_address() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    // Negotiation is deliberately left disabled (bit 1 set) so this test
    // stays isolated to the target-address range check alone -- it must
    // stay reachable on a connected-but-not-comm_started CLL, so it never
    // calls `start_comm`/claims an address, and must not be conflated with
    // any separate, unrelated precondition a negotiation-enabled CLL might
    // additionally require before Repeat Messaging (ADR-180 Decision 13).
    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_TARGET_ADDRESS, 0x100),
            (CP_J1939_ADDR_NEG_RULE, 0b10), // bit 1 set: do not negotiate
        ],
        &[],
        1,
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE with an out-of-range CP_J1939TargetAddress (0x100) \
         should be rejected",
    );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("CP_J1939TargetAddress"),
        "the rejection should name CP_J1939TargetAddress: {}",
        status.message()
    );

    server.shutdown().await;
}

/// ADR-180 Decision 13 (design-advisor consult, PR #72 round 11): the
/// START-time gate's first closed window -- a negotiation-enabled J1939 CLL
/// (`CP_J1939AddressNegotiationRule` bit 1 clear, the spec default) that has
/// never issued a `CoptStartcomm` at all (so `j1939_claimed_address` is
/// still `None`) must not be able to start a repeat slot -- there is no
/// claimed source address for the slot's own transmitted frame to be framed
/// against, and the previous behavior silently composed under the `0xF1`
/// pre-claim default instead of rejecting.
#[tokio::test]
#[serial]
async fn repeat_message_start_rejects_before_any_claim_on_a_negotiation_enabled_cll() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    // Deliberately never calls `start_comm` -- `j1939_claimed_address` stays
    // `None` for the whole test.
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE on a negotiation-enabled J1939 CLL with no claimed \
         address yet should be rejected",
    );
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("claimed a source address"),
        "the rejection should explain the missing claim: {}",
        status.message()
    );

    server.shutdown().await;
}

/// ADR-180 Decision 13's second closed window: a negotiation-enabled J1939
/// CLL whose `CoptStartcomm` claim attempt exhausted its whole candidate
/// list (never successfully claimed anything) must be rejected the same way
/// as before-any-claim -- there is no later claim-success writeback that
/// will ever arrive to make a stale slot's address current.
/// `server.backdoor.set_j1939_claim_lost(true)` forces every candidate the
/// claim loop tries to come back `_LOST`, the same deterministic exhaustion
/// trigger `claim_exhaustion_fails_startcomm_with_init_error` above uses.
#[tokio::test]
#[serial]
async fn repeat_message_start_rejects_after_claim_exhaustion() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    server.backdoor.set_j1939_claim_lost(true);

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle).await.expect(
        "start_com_primitive(CoptStartcomm) should succeed synchronously -- claim exhaustion \
         surfaces asynchronously",
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the exhausted CoptStartcomm should still finish"
    );
    drop(events);

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE after claim exhaustion should be rejected the same way \
         as before any claim -- j1939_claimed_address is still None",
    );
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("claimed a source address"),
        "the rejection should explain the missing claim: {}",
        status.message()
    );

    server.shutdown().await;
}

/// ADR-180 Decision 13's negative case for the START-time gate's own
/// negotiation-enabled scoping: a non-negotiated CLL (`CP_J1939AddressNegotiationRule`
/// bit 1 set) never runs the claim loop at all, so `j1939_claimed_address`
/// stays `None` for its entire life BY DESIGN -- `CP_TesterSourceAddress` is
/// client-managed instead (ADR-180 Decision 12's own precedent). The gate
/// must not reject Repeat Messaging on such a CLL just because
/// `j1939_claimed_address` happens to be `None`.
#[tokio::test]
#[serial]
async fn repeat_message_start_succeeds_on_non_negotiated_cll_with_no_claim() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_TARGET_ADDRESS, 0x10),
            (CP_J1939_ADDR_NEG_RULE, 0b10), // bit 1 set: do not negotiate
        ],
        &[],
        1,
    )
    .await;

    // Deliberately never calls `start_comm` -- a non-negotiated CLL has no
    // claim loop to run at all, so `j1939_claimed_address` stays `None`
    // regardless.
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect(
        "PDU_IOCTL_START_REPEAT_MESSAGE on a non-negotiated J1939 CLL must succeed even with no \
         claimed address -- the gate is scoped to negotiation-enabled CLLs only",
    );

    server.shutdown().await;
}

/// Positive/regression-safety case for ADR-180 Decision 13's START-time
/// gate: a negotiation-enabled J1939 CLL that HAS successfully claimed an
/// address must still be able to start a repeat slot normally -- the gate
/// must not regress the ordinary, already-claimed case every other J1939
/// repeat-message test in this file relies on.
#[tokio::test]
#[serial]
async fn repeat_message_start_succeeds_after_a_successful_claim() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim 0x80");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80"
    );
    drop(events);

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect(
        "PDU_IOCTL_START_REPEAT_MESSAGE after a successful claim must still succeed -- the new \
         gate only rejects the unclaimed case",
    );

    server.shutdown().await;
}

/// `edge-case-hunter` finding (verification pass on PR #72 round 11's own
/// diff): `ordinary_send_sets_tx_extended_id` above proves `TX_EXTENDED_ID`
/// now reaches an ordinary J1939 `CoptSendrecv`, but SAE J2534-2 clause 14
/// Repeat Messaging's own actually-transmitted `RepeatMsgData[0]` message is
/// composed independently, by `rpc_misc.rs::ioctl_start_repeat_message`'s
/// own inline TxFlags logic (mirroring that same function's own local
/// SW-CAN/J1708 gates, both already duplicated there for the identical
/// reason) -- it was missed by round 11's `apply_resolved_tx_flags` fix and
/// still went out as an 11-bit frame. Proves a J1939 repeat slot's
/// transmitted frame now carries `TX_EXTENDED_ID` too.
#[tokio::test]
#[serial]
async fn repeat_message_transmitted_frame_sets_tx_extended_id() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim 0x80");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80"
    );
    drop(events);

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(20, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a claimed J1939 CLL");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0) & j2534_0404::TX_EXTENDED_ID,
        j2534_0404::TX_EXTENDED_ID,
        "a SAE J1939 repeat slot's transmitted RepeatMsgData[0] frame must be written with \
         TX_EXTENDED_ID set (clause 16.4.3: SAE J1939 frames are always 29-bit)"
    );

    server.shutdown().await;
}

/// ADR-180 Decision 14 Finding 1 Part A (design-advisor consult, PR #72
/// round 12): mirrors this file's own
/// `repeat_message_start_rejects_before_any_claim_on_a_negotiation_enabled_cll`
/// (Decision 13's own analogous gate) for `CoptSendrecv` itself -- a
/// transmitting `CoptSendrecv` on a negotiation-enabled J1939 CLL that has
/// never had a successful claim is now rejected synchronously at
/// `StartComPrimitive` time, before `cop_handle` allocation, rather than
/// silently composing under the pre-claim `0xF1` default (or letting the
/// native `ERR_ADDRESS_NOT_CLAIMED` surface only asynchronously, as
/// `unclaimed_source_address_over_8_bytes_fails_the_send` above exercises
/// for a NON-negotiated CLL, where this new gate does not apply).
///
/// Finding 1 Part B (the transmit-time `SendRecvTx::j1939_tx_source`
/// re-check in `events.rs::handle_send_recv`, closing this gate's own
/// enqueue-to-dispatch TOCTOU) has no dedicated regression test here --
/// deterministically racing a live claim wait against a queued cyclic send's
/// own dispatch is the same class of ~10ms-internal-window race this file's
/// own module doc already documents as unobservable/uncontrollable from a
/// pure RPC round trip (see "Item 2" above); this mirrors ADR-180 Decision
/// 13's own accepted residual for its outcome-arm race -- verified by code
/// inspection instead (see ADR-180 Decision 14's own "Accepted residual"
/// note).
#[tokio::test]
#[serial]
async fn sendrecv_rejects_before_any_claim_on_a_negotiation_enabled_cll() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    // Deliberately never calls `start_comm` -- `j1939_claimed_address` stays
    // `None` for the whole test.
    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01, 0x02],
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
        .expect_err(
            "CoptSendrecv on a negotiation-enabled J1939 CLL with no claimed address yet \
             should be rejected synchronously",
        );
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("no claimed source address"),
        "the rejection should explain the missing claim: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the rejected send must never reach the mock"
    );

    server.shutdown().await;
}

/// Negative counterpart to the test above: a receive-only `CoptSendrecv`
/// (`NumSendCycles == 0`, ADR-059) puts nothing on the bus, so Finding 1
/// Part A's gate must not reject it even with no claim yet.
#[tokio::test]
#[serial]
async fn sendrecv_receive_only_is_not_gated_by_the_no_claim_check() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect(
            "a receive-only CoptSendrecv (NumSendCycles == 0) must not be rejected by the \
             no-claim-yet gate -- it puts nothing on the bus",
        );

    server.shutdown().await;
}

/// ADR-180 Decision 15 (design-advisor consult, PR #72 round 12, Finding
/// 2): closes the TOCTOU Decision 12's own `StartComPrimitive`-time
/// `CoptUpdateparam` guard cannot see. `PDU_IOCTL_SUSPEND_TX_QUEUE`/
/// `RESUME_TX_QUEUE` make the race deterministic (unlike a bare
/// back-to-back RPC race, which this file's own module doc already
/// documents as unobservable/uncontrollable): both a `CoptStartcomm` and a
/// `CoptUpdateparam` staging a DIFFERENT `CP_TesterSourceAddress` are
/// enqueued while the queue is held suspended -- so `CoptUpdateparam`'s own
/// `StartComPrimitive`-time enqueue check (Decision 12) sees NO live claim
/// at all yet (`SharedChannel::j1939_claims` is still empty for this CLL,
/// since `CoptStartcomm` has not been dispatched) and lets it through. Once
/// resumed, the queue drains strictly FIFO: `CoptStartcomm` dispatches
/// first, claims 0x80, and promotes Active `NODE_ADDRESS`; only THEN does
/// `CoptUpdateparam` dispatch, landing on Decision 15's new execution-time
/// check with a now-live claim and a now-genuine drift Decision 12's own
/// snapshot could not have seen.
#[tokio::test]
#[serial]
async fn coptupdateparam_execution_time_check_catches_a_claim_that_lands_after_enqueue() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_handle, suspend_id, None, false)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm) should be accepted synchronously (siphoned \
             into tx_held while suspended)",
        );

    // Decision 12's own enqueue-time guard passes here: no claim has been
    // attempted yet (CoptStartcomm is still held, not executed), so
    // `SharedChannel::j1939_claims` has no entry for this CLL at all.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::NODE_ADDRESS, 0x90).await;
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptUpdateparam should enqueue successfully -- Decision 12's own guard sees no \
             live claim yet",
        );

    io_ctl_cll(&mut client, cll_handle, resume_id, None, false)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    // CoptStartcomm dispatches first (FIFO), claims 0x80, and finishes.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80 before CoptUpdateparam executes"
    );

    // CoptUpdateparam dispatches next -- Decision 15's execution-time check
    // must now catch the drift Decision 12's own snapshot could not see:
    // staged NODE_ADDRESS (0x90) differs from Active (0x80, just claimed),
    // and SharedChannel::j1939_claims now owns this exact (cll_handle,
    // connect_generation).
    let mut saw_prot_err = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtProtErr as i32
            ) {
                saw_prot_err = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstCancelled as i32
            )
        })
        .await,
        "CoptUpdateparam should be cancelled by Decision 15's execution-time re-check"
    );
    assert!(
        saw_prot_err,
        "the cancellation should surface PDU_ERR_EVT_PROT_ERR"
    );

    // The claimed address must still be the one actually in use -- Active
    // NODE_ADDRESS was never corrupted by the rejected CoptUpdateparam's
    // Working value (0x90).
    drop(events);
    send_data(&mut client, cll_handle, vec![0xAA], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.get(3).copied(),
        Some(0x80),
        "a subsequent send must still use the live claimed address (0x80), not the rejected \
         promotion's 0x90; got {written:#04x?}"
    );

    server.shutdown().await;
}

/// CP_J1939TargetAddress analogue of the NODE_ADDRESS test above -- ADR-180
/// Decision 15's own established pattern, extended in round 15 to a second,
/// independent field. `rpc_primitive.rs`'s `CoptUpdateparam` handler
/// synchronously rejects staging the `0xFFFF` "not configured" sentinel
/// only when its OWN enqueue-time snapshot of `comm_started` already reads
/// `true`; a `CoptStartcomm` queued just ahead of an already-queued
/// `CoptUpdateparam` staging `0xFFFF` can flip `comm_started` from `false`
/// to `true` strictly between that snapshot and the `CoptUpdateparam`
/// promotion actually running, letting the sentinel slip through
/// unvalidated onto an already-started SAE J1939 ComLogicalLink. Same
/// `PDU_IOCTL_SUSPEND_TX_QUEUE`/`RESUME_TX_QUEUE` determinism trick as the
/// sibling test above: both COPs are enqueued while the queue is held
/// suspended, so the enqueue-time guard sees `comm_started == false` and
/// lets the `0xFFFF` staging through; once resumed, the queue drains FIFO
/// -- `CoptStartcomm` dispatches first and sets `comm_started = true` --
/// so the queued `CoptUpdateparam` must be caught by
/// `handle_update_param`'s new execution-time re-check instead.
///
/// Deliberately negotiation-DISABLED (`CP_J1939AddressNegotiationRule` bit
/// 1 set, unlike the sibling NODE_ADDRESS test above), so `handle_start_comm`
/// never runs the address-claim loop and `NODE_ADDRESS` is never written on
/// `CoptStartcomm` -- Working and Active `NODE_ADDRESS` stay equal
/// throughout, which keeps Decision 15's own adjacent `claim_owns_address`
/// re-check from ever tripping (it requires a drift plus an owned claim,
/// neither of which exist here) and isolates this test to the
/// `CP_J1939TargetAddress` re-check under test.
#[tokio::test]
#[serial]
async fn coptupdateparam_execution_time_check_catches_j1939_target_address_ffff_after_startcomm() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_TARGET_ADDRESS, 0x10),
            (CP_J1939_ADDR_NEG_RULE, 0b10), // bit 1 set: do not negotiate
        ],
        &[],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_handle, suspend_id, None, false)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm) should be accepted synchronously (siphoned \
             into tx_held while suspended)",
        );

    // The enqueue-time guard in `rpc_primitive.rs` passes here: its own
    // `comm_started` snapshot reads `false` (CoptStartcomm is still held,
    // not executed), so staging the 0xFFFF sentinel is not rejected.
    set_com_param_unum32(&mut client, cll_handle, CP_J1939_TARGET_ADDRESS, 0xFFFF).await;
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptUpdateparam should enqueue successfully -- the enqueue-time guard's own \
             comm_started snapshot reads false",
        );

    io_ctl_cll(&mut client, cll_handle, resume_id, None, false)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    // CoptStartcomm dispatches first (FIFO) and finishes, flipping live
    // comm_started to true before CoptUpdateparam executes.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish before CoptUpdateparam executes"
    );

    // CoptUpdateparam dispatches next -- the new execution-time re-check
    // must now catch what the enqueue-time guard could not see: live
    // comm_started is true and Working still stages the 0xFFFF sentinel.
    let mut saw_prot_err = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtProtErr as i32
            ) {
                saw_prot_err = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstCancelled as i32
            )
        })
        .await,
        "CoptUpdateparam should be cancelled by the execution-time re-check"
    );
    assert!(
        saw_prot_err,
        "the cancellation should surface PDU_ERR_EVT_PROT_ERR"
    );

    drop(events);
    server.shutdown().await;
}

/// Round-17 correction to ADR-180 Decision 15 (Codex review, PR #72): the
/// execution-time re-check above used to run AFTER
/// `apply_params_to_hardware_locked` had already pushed every OTHER staged
/// ComParam in the same `CoptUpdateparam`'s `hw_set` to the adapter, so a
/// rejection left the adapter holding new values for everything but the
/// conflicting NODE_ADDRESS while Active (and the mock) kept the old ones --
/// a partial apply Decision 15's own "terminate the whole COP" rejection
/// shape was never supposed to allow. This test stages the SAME
/// NODE_ADDRESS-off-the-live-claim conflict as
/// `coptupdateparam_execution_time_check_catches_a_claim_that_lands_after_enqueue`
/// above, but in the SAME `CoptUpdateparam` also stages an unrelated,
/// hardware-backed ComParam (`DATA_RATE`, unconditionally pushed via
/// SET_CONFIG since nothing else on this CLL holds
/// `LOCK_PHYSICAL_COM_PARAMS`) -- and asserts the mock's SET_CONFIG count
/// never advances past its pre-dispatch baseline, proving the round-17 fix
/// (both re-checks now run BEFORE `apply_params_to_hardware_locked`, not
/// after) actually prevents the hardware push rather than merely rejecting
/// the COP after the fact.
#[tokio::test]
#[serial]
async fn coptupdateparam_execution_time_rejection_pushes_no_hardware_write_for_the_cancelled_cop() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_handle, suspend_id, None, false)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm) should be accepted synchronously (siphoned \
             into tx_held while suspended)",
        );

    // Decision 12's own enqueue-time guard passes here: no claim has been
    // attempted yet, so `SharedChannel::j1939_claims` has no entry for this
    // CLL. Stages BOTH the conflicting NODE_ADDRESS AND an unrelated
    // hardware-backed DATA_RATE change in the same Working set.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::NODE_ADDRESS, 0x90).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 125_000).await;

    let baseline_set_config = server.backdoor.set_config_count();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptUpdateparam should enqueue successfully -- Decision 12's own guard sees no \
             live claim yet",
        );

    io_ctl_cll(&mut client, cll_handle, resume_id, None, false)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    // CoptStartcomm dispatches first (FIFO), claims 0x80, and finishes.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80 before CoptUpdateparam executes"
    );

    // CoptUpdateparam dispatches next -- the round-17-corrected execution-time
    // re-check must reject BEFORE `apply_params_to_hardware_locked` ever
    // runs, so the unrelated DATA_RATE change must never reach the adapter.
    let mut saw_prot_err = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtProtErr as i32
            ) {
                saw_prot_err = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstCancelled as i32
            )
        })
        .await,
        "CoptUpdateparam should be cancelled by the round-17-corrected execution-time re-check"
    );
    assert!(
        saw_prot_err,
        "the cancellation should surface PDU_ERR_EVT_PROT_ERR"
    );
    assert_eq!(
        server.backdoor.set_config_count(),
        baseline_set_config,
        "the round-17 fix must reject before any hardware push -- the unrelated DATA_RATE \
         change in the same CoptUpdateparam must never reach SET_CONFIG"
    );

    drop(events);
    server.shutdown().await;
}

// ADR-180 Decision 16 Finding 3 (design-advisor consult, PR #72 round 12):
// `dispatch_due_tester_present`'s filter now also excludes a negotiation-
// enabled J1939 CLL with no CURRENTLY claimed address
// (`j1939_negotiated_unclaimed`) -- see `events_j1939_claim::tests::
// negotiated_unclaimed_*` (`j2534-0404-service/src/service/events_j1939_
// claim.rs`) for unit coverage of that shared predicate itself. A live,
// end-to-end regression test of THIS integration point specifically
// (arming tester-present on a J1939 CLL, synthesizing a spontaneous
// `RX_FLAG_J1939_ADDRESS_LOST`, and confirming no further tester-present
// frame is written) is UNREACHABLE via this suite's RPC surface, for the
// exact same reason already documented above (search this file for
// "Codex review round 6 raised two further findings on this same
// mechanism"): `comparam_support::is_j1939_param`'s closed ComParam
// allowlist does not include `CP_TesterPresentHandling`/
// `CP_TesterPresentMessage`/`CP_TesterPresentTime`/`CP_TesterPresentSendType`
// at all, so `SetComParam` rejects every attempt to configure them on a
// J1939 CLL with `PDU_ERR_COMPARAM_NOT_SUPPORTED` -- `resolve_tester_present`
// therefore always short-circuits disabled for a J1939 CLL reachable from a
// real client, so `dispatch_due_tester_present`'s filter (old or new) never
// actually runs its `TesterPresentState::Armed` arm for one either. Same
// "found unreachable while writing a regression test, documented instead
// of adding a misleading one" precedent; see ADR-180 Decision 16's own
// "Accepted residual" note for the durable record.

/// `edge-case-hunter` finding (verification pass on PR #72 round 12's own
/// diff): the Decision 14 Part A enqueue-time gate used to read this CLL's
/// Active `CP_J1939AddressNegotiationRule` unconditionally, even for a
/// `temp_param_update = 1` `CoptSendrecv`, whose own resolution
/// (`resolve_send_recv_tx`) binds against the Working (`effective`)
/// snapshot instead (ADR-067) -- so the gate and the resolution could
/// disagree about whether this CLL even counts as "negotiated" for THIS
/// one Temp-bound send. Here: Active stays negotiation-enabled (bit 1
/// clear, the default) and unclaimed (no `CoptStartcomm` ever issued), but
/// Working is staged to non-negotiated (bit 1 set) via an ordinary
/// `SetComParam` after Connect -- mirroring `param_binding.rs`'s own
/// `claim9_sendrecv_pins_working_snapshot_at_call_time...` pattern for
/// constructing an Active/Working divergence. A `temp_param_update = 1`
/// `CoptSendrecv` must bind Working (non-negotiated), which never needs a
/// claim at all -- so it must succeed, even though this CLL's OWN Active
/// set alone would fail the gate. Before the fix, the gate read Active
/// unconditionally and wrongly rejected this send.
#[tokio::test]
#[serial]
async fn coptsendrecv_temp_binding_uses_workings_own_negotiation_rule_not_actives() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    // Active stays negotiation-enabled and unclaimed (never started). Stage
    // Working (only) to non-negotiated via an ordinary post-connect
    // SetComParam.
    set_com_param_unum32(&mut client, cll_handle, CP_J1939_ADDR_NEG_RULE, 0b10).await;

    let payload = vec![0x01];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect(
            "a temp_param_update CoptSendrecv must bind Working's own (non-negotiated) \
             CP_J1939AddressNegotiationRule, not this CLL's Active (negotiated, unclaimed) \
             value -- it must not be rejected by the Decision 14 enqueue-time gate",
        );

    server.shutdown().await;
}

/// ADR-180 Decision 18 (design-advisor consult, PR #72 round 15, P1
/// finding): the sibling/contrasting case to the test just above --
/// UNLIKE that one, this CLL DID issue a real `CoptStartcomm` that itself
/// requested negotiation (the Active default, never overridden), so its
/// `LogicalLinkState::j1939_negotiation_posture` is structurally `Engaged`.
/// No `CP_J1939PreferredAddress` is staged (this file's own bytefield
/// default, `comparam_defaults.rs`'s `j1939_can_common`, resolves to an
/// empty candidate list), so the claim loop exhausts immediately and this
/// CLL never successfully claims -- mirroring `claim_exhaustion_fails_
/// startcomm_with_init_error`'s shape, just via an empty list instead of
/// `set_j1939_claim_lost`. A client then stages a Working-ONLY
/// `CP_J1939AddressNegotiationRule` opt-out (never promoted to Active via
/// an ordinary `SetComParam`) and issues a `temp_param_update = 1`
/// `CoptSendrecv` -- before the fix, the enqueue-time gate would read only
/// this call's Working snapshot, see "not negotiation-managed", and let an
/// unclaimed send through under the default/unclaimed source address. The
/// widened `j1939_negotiated_unclaimed_for` predicate's `engaged` OR-branch
/// must still reject it.
#[tokio::test]
#[serial]
async fn coptsendrecv_temp_binding_cannot_spoof_a_negotiation_opt_out_on_an_engaged_unclaimed_cll()
{
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        // No CP_J1939_PREFERRED_ADDRESS staged -- empty candidate list,
        // claim loop exhausts immediately (see this test's own doc comment).
        &[],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await.expect(
        "start_com_primitive(CoptStartcomm) should succeed synchronously -- claim exhaustion \
         surfaces asynchronously as PduErrEvtInitError, not a synchronous error",
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the exhausted CoptStartcomm should still finish (PduCopstFinished), not hang -- this \
         CLL is now negotiation-engaged and unclaimed, the precondition this test needs"
    );
    drop(events);

    // The spoof: stage a Working-ONLY CP_J1939AddressNegotiationRule
    // opt-out, never promoted to Active.
    set_com_param_unum32(&mut client, cll_handle, CP_J1939_ADDR_NEG_RULE, 0b10).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect_err(
            "a Temp-bound CoptSendrecv must not bypass the no-claim-yet gate via a Working-only \
             CP_J1939AddressNegotiationRule opt-out on a CLL that structurally requested \
             negotiation at its own CoptStartcomm and has never claimed an address (ADR-180 \
             Decision 18)",
        );
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("no claimed source address"),
        "the rejection should explain the missing claim: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the rejected send must never reach the mock"
    );

    server.shutdown().await;
}

/// ADR-180 Decision 17 (Codex review, PR #72 round 13): `DisconnectComLogicalLink`
/// only clears the SHARED `SharedChannel::j1939_claims` routing entry
/// (`cancel_j1939_claims_for_cll` takes no `logical_links` reference at
/// all) -- the per-CLL LOCAL `j1939_claimed_address` field used to survive
/// a disconnect untouched, since `ConnectComLogicalLink`'s own finalization
/// reuses the same `LogicalLinkState` entry for a same-handle reconnect
/// (ADR-086) without resetting it. This proves the fix: a CLL claims 0x80,
/// disconnects (native claim cancelled), reconnects on the same handle
/// (never re-claiming), and Decision 13's own `PDU_IOCTL_START_REPEAT_MESSAGE`
/// gate must now see this CLL as unclaimed and reject the attempt -- before
/// the fix, the stale `Some(0x80)` marker made the gate wrongly treat it as
/// still claimed, permitting an autonomous repeat slot framed with an
/// address the adapter had already stopped defending.
#[tokio::test]
#[serial]
async fn reconnecting_the_same_cll_handle_clears_the_stale_claimed_address() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80"
    );
    drop(events);

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("reconnecting the same cll_handle should succeed");

    // Deliberately never re-issues CoptStartcomm after reconnecting --
    // `j1939_claimed_address` must be `None` again, not the stale `Some(0x80)`
    // left over from before the disconnect.
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let status = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect_err(
        "a reconnected CLL that never re-claimed must be rejected by Decision 13's gate the \
         same as any other never-yet-claimed negotiation-enabled CLL -- the pre-disconnect \
         claim must not leak through reconnect",
    );
    assert_eq!(status.code(), Code::FailedPrecondition);

    server.shutdown().await;
}

// Historical note: this file previously carried three regression tests for
// Codex review finding PR #72 round 13 (`create_rejects_a_deferred_
// additional_channel_id`, `_even_with_no_pins`, and `_compound_name`),
// proving `resolve_pin_selection`'s J1939 arm correctly rejected a raw or
// compound-name `_CHx` id as "Additional Channels are not yet supported."
// ADR-206 intentionally supersedes that design (see this file's own
// "SAE J2534-2 clause 7 Additional Channels (_CHx), ADR-206" section
// below): J1939 `_CHx` is no longer deferred, so those three tests'
// `expect_err` assertions no longer hold (two of the three scenarios now
// succeed outright; the third still errors, but for an unrelated,
// pre-existing reason -- the generic clause-6 Pin Selection fallback
// tail's own "no `_PS` variant in scope" rejection when a raw `_CHx` id's
// normalized base has none, the same fallback GM UART's own raw-`_CHx`
// route was already subject to before this ADR). Removed rather than kept
// as stale/misleading assertions; see the new section below for this
// file's current `_CHx` coverage.

/// Codex review finding (PR #72 round 14): the successful-claim recomposition
/// of the optional `CoptStartcomm` message's 5-byte header (round 3's own
/// fix, `events.rs`'s `J1939ClaimLoopOutcome::Claimed` arm) used to read
/// every field OTHER than the source address from `link.active`
/// unconditionally -- correct for a Plain-bound `CoptStartcomm`, but wrong
/// for a `temp_param_update` one, whose ORIGINAL header composition (at
/// `StartComPrimitive` call time) instead resolved those same fields from
/// `binding.resolved()`'s Working (`effective`) snapshot (ADR-067). This
/// proves the fix: Active stays at `CP_J1939TargetAddress = 0x10` (never
/// changed), Working is staged to a DIFFERENT target address (0x20) via an
/// ordinary post-connect `SetComParam`, then a `temp_param_update = 1`
/// `CoptStartcomm` with an optional message is issued -- the message must
/// reach the wire addressed to the Working-bound target (0x20), not the
/// stale Active one (0x10), while still carrying the freshly claimed source
/// address.
#[tokio::test]
#[serial]
async fn optional_startcomm_message_after_claim_uses_the_bound_snapshots_other_header_fields() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x88]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    // Active stays at 0x10. Stage Working (only) to a different target
    // address via an ordinary post-connect SetComParam.
    set_com_param_unum32(&mut client, cll_handle, CP_J1939_TARGET_ADDRESS, 0x20).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0xAA, 0xBB],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm, temp_param_update=1) with a claim-enabled \
             optional message should succeed synchronously",
        );

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish after claiming and sending the optional message"
    );
    drop(events);

    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    // byte0 = (priority(0, iso_obd_on_sae_j1939_73's own override) & 7) << 2
    // | (data_page(0, default) & 1) = 0x00; pdu_format defaults to 0 (< 240,
    // PDU1), so byte2 = target address; byte3 = the claimed source address
    // (0x88); byte4 = target address again -- both bytes 2 and 4 must be
    // 0x20 (Working-bound), never 0x10 (the stale Active value), proving the
    // recomposition read `binding.resolved()`, not `link.active`.
    assert_eq!(
        written,
        vec![0x00, 0x00, 0x20, 0x88, 0x20, 0xAA, 0xBB],
        "expected [byte0, pdu_format, ps_byte, claimed_source, target_address] + the optional \
         message payload, addressed to the Working-bound target (0x20), not Active's (0x10); \
         got {written:#04x?}"
    );

    server.shutdown().await;
}

/// Codex review finding (PR #72 round 16), closing ADR-180 Decision 18's own
/// documented accepted residual: a CLL that claims an address, then StopComms
/// and re-StartComms with `CP_J1939AddressNegotiationRule` promoted to
/// non-negotiated (`0b10`), used to clear only `j1939_negotiation_posture` --
/// the prior claim's native defense and `SharedChannel::j1939_claims`
/// registration survived untouched, permanently consuming that address (a
/// sibling CLL with the same candidate could never claim it) even though the
/// client now believes it has opted out and manages `NODE_ADDRESS` itself.
/// This proves the fix the same way this file's other leaked-claim tests do
/// (no direct backdoor into `SharedChannel::j1939_claims`, ADR-180's own
/// established proof pattern): CLL A claims 0x80, StopComms, promotes
/// negotiation to disabled, StartComms again (now taking the non-negotiated
/// branch) -- if 0x80 were still registered to A, sibling CLL B's own
/// single-candidate `[0x80]` claim attempt would find it `owned_by_a_live_
/// sibling` and immediately exhaust without ever issuing a native attempt.
#[tokio::test]
#[serial]
async fn opting_out_of_negotiation_after_a_claim_frees_the_address_for_a_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;
    let cll_b = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![8, 7, 6, 5, 4, 3, 2, 1]),
        ],
        2,
    )
    .await;
    promote_via_update_param(&mut client, cll_b).await;

    // -- CLL A: claim 0x80, confirmed via a send. --
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's first attempt");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's first CoptStartcomm should finish and claim 0x80"
    );

    send_data(&mut client, cll_a, vec![0xAA], vec![]).await;
    let written_first = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written_first.get(3).copied(),
        Some(0x80),
        "CLL A should have claimed 0x80; got {written_first:#04x?}"
    );
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's first CoptSendrecv should finish"
    );
    drop(events_a);

    // -- CLL A: StopComm, opt OUT of negotiation, StartComm again. --
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, empty cop_data) should succeed on CLL A");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptStopcomm should finish"
    );

    set_com_param_unum32(&mut client, cll_a, CP_J1939_ADDR_NEG_RULE, 0b10).await;
    promote_via_update_param(&mut client, cll_a).await;
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptUpdateparam (opting out of negotiation) should finish"
    );

    start_comm(&mut client, cll_a).await.expect(
        "start_com_primitive(CoptStartcomm) should succeed on CLL A's non-negotiated restart",
    );
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's non-negotiated CoptStartcomm should finish immediately (no claim attempt)"
    );
    drop(events_a);

    // -- CLL B: must now be able to claim 0x80 -- only possible if A's stale
    // claim was actually cancelled and deregistered. --
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL B");
    let mut saw_init_error_b = false;
    assert!(
        wait_for_event(&mut events_b, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error_b = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "CLL B's CoptStartcomm should finish"
    );
    assert!(
        !saw_init_error_b,
        "CLL B should successfully claim 0x80 -- only possible if CLL A's stale claim from \
         opting out of negotiation was actually cancelled and deregistered"
    );
    drop(events_b);

    server.shutdown().await;
}

/// Correction to ADR-180 Decision 18 (design-advisor consult, PR #72 review
/// round): the mirror-image gap the original two-state (`bool`)
/// `j1939_negotiation_engaged`/`_posture` design missed. A `temp_param_
/// update = 1` `CoptStartcomm` that explicitly opts OUT of negotiation
/// (Working-only `CP_J1939AddressNegotiationRule` opt-out, never promoted to
/// Active) correctly completes with no claim attempted -- but pre-fix, every
/// LATER ordinary (non-Temp) operation on this CLL still read `link.active`'s
/// stale negotiation-ENABLED default (since the old flag stayed `false`,
/// indistinguishable from "no real StartComm has decided yet") and wrongly
/// concluded "still negotiated and unclaimed", permanently blocking the CLL.
///
/// No claim indications are needed -- deterministic on the live RPC surface
/// alone. `CP_J1939PreferredAddress` is deliberately left unstaged (empty
/// candidate list): this file's own `claim_exhaustion_fails_startcomm_with_
/// init_error`/`unconfigured_name_fails_the_claim_immediately_not_via_
/// timeout` precedent establishes that ANY claim attempt with an empty
/// candidate list exhausts immediately and fails with `PduErrEvtInitError`
/// -- so watching for that error's ABSENCE alongside the opt-out
/// `CoptStartcomm`'s own successful finish is this suite's established
/// no-dedicated-call-counter substitute for asserting "zero native
/// `PROTECT_J1939_ADDR` claim writes were issued" (no such counter exists on
/// `harness.rs`'s mock backdoor; `set_j1939_claim_lost` toggles claim
/// *outcome*, not a call log).
#[tokio::test]
#[serial]
async fn temp_bound_negotiation_opt_out_does_not_wrongly_block_later_ordinary_sends() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    // Active default: CP_J1939AddressNegotiationRule bit 1 clear
    // (negotiation ENABLED) -- never overridden via an ordinary SetComParam,
    // so it stays at that default for the whole test, exactly the "stale
    // Active" precondition this bug depends on. No CP_J1939PreferredAddress
    // is staged (see doc comment above).
    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x10)],
        &[],
        1,
    )
    .await;

    // Stage a Working-only opt-out (bit 1 set) via an ordinary SetComParam --
    // never promoted to Active via CoptUpdateparam.
    set_com_param_unum32(&mut client, cll_handle, CP_J1939_ADDR_NEG_RULE, 0b10).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm, temp_param_update=1) opting out of negotiation \
             should succeed synchronously",
        );

    let mut saw_init_error = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "the temp-bound opt-out CoptStartcomm should finish"
    );
    assert!(
        !saw_init_error,
        "no claim should ever have been attempted for a temp-bound opt-out CoptStartcomm -- an \
         empty CP_J1939PreferredAddress candidate list would fail any attempted claim with \
         PduErrEvtInitError"
    );
    drop(events);

    // An ordinary (non-Temp) CoptSendrecv must now transmit successfully --
    // pre-fix, the stale Active negotiation-ENABLED default made the
    // send-gate (`rpc_primitive.rs`) wrongly conclude this CLL is still
    // negotiated-and-unclaimed, rejecting this call with
    // `FailedPrecondition` before it ever reached the mock.
    send_data(&mut client, cll_handle, vec![0x01], vec![]).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the ordinary CoptSendrecv should have actually reached the mock and transmitted"
    );

    // `PDU_IOCTL_START_REPEAT_MESSAGE` must also succeed on this CLL -- pre-
    // fix, `rpc_misc.rs`'s repeat-message-start gate would reject it the same
    // way as the send gate above.
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect(
        "PDU_IOCTL_START_REPEAT_MESSAGE on a temp-bound negotiation opt-out CLL must succeed -- \
         the CLL's real, structural posture is OptedOut, not stuck reading Active's stale \
         negotiation-enabled default",
    );

    server.shutdown().await;
}

/// ADR-180 Decision 19 (design-advisor consult, PR #72 round 16, Codex P1):
/// a `PDU_COPT_STOPCOMM` final message resolved and enqueued against a
/// claimed source address, whose CLL then spontaneously loses that address
/// (clause 16.4.6) before this COP actually dispatches, used to transmit its
/// pre-composed frame anyway -- `cancel_send_recv_cops_for_cll` deliberately
/// excludes `CoptStopcomm` from its cancellation sweep (ADR-085's
/// `stop_comm_pending` deadlock), so nothing else ever revisited this COP's
/// own stale header. The frame would go out on the wire still claiming
/// (byte 3 of the header) an address this CLL no longer owns, impersonating
/// whichever CLL -- or nothing -- claims it next.
///
/// Deterministic "already-lost-at-call-time" reproduction (design-advisor's
/// own recommended shape): claim 0x80, force every subsequent claim
/// candidate to resolve `_LOST` (`set_j1939_claim_lost`), inject a
/// spontaneous `RX_FLAG_J1939_ADDRESS_LOST` for 0x80, and wait for the
/// resulting reclaim attempt's own exhaustion (a CLL-scoped `PduErrEvtInitError`
/// with no `cop_handle`, `run_j1939_reclaim_duties`'s own exhaustion signal)
/// -- proof this CLL is now negotiation-engaged with no current claim. Only
/// then issue `CoptStopcomm` with a non-empty `cop_data` (so its own optional
/// final message resolves a real `j1939_tx_source`). Asserts: no additional
/// byte reaches the mock (the transmit itself is suppressed, not merely
/// mis-addressed), a `PduErrEvtProtErr` reports the drift, and the COP still
/// completes to `PduCllstOnline`/`PduCopstFinished` -- ADR-085's teardown
/// must never be blocked by this suppression.
#[tokio::test]
#[serial]
async fn stopcomm_final_message_after_a_spontaneous_loss_is_suppressed_not_sent() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim 0x80");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80"
    );

    // Force the reclaim attempt this spontaneous loss arms to exhaust
    // immediately (single candidate, `_LOST` every time) rather than
    // re-claim 0x80 -- this CLL must land, and stay, negotiation-engaged
    // with no current claim.
    server.backdoor.set_j1939_claim_lost(true);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x80],
        j2534_0404::PROTOCOL_J1939_PS,
        0x0002_0000, // RX_FLAG_J1939_ADDRESS_LOST, events.rs::RX_J1939_ADDRESS_LOST
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtInitError as i32
        ) && item.cop_handle.is_none())
        .await,
        "the spontaneous-loss-armed reclaim attempt should exhaust its single (forced-LOST) \
         candidate and report it CLL-scoped (no cop_handle) -- proof this CLL is now \
         negotiation-engaged with no current claim"
    );

    let written_before = server.backdoor.written_count(MOCK_CHANNEL_ID);

    let stopcomm_cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![0x01],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, non-empty cop_data) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    let mut saw_prot_err = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtProtErr as i32
            ) && item.cop_handle.as_ref().map(|c| c.cop_handle) == Some(stopcomm_cop.cop_handle)
            {
                saw_prot_err = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "CoptStopcomm must still complete to Finished even though its own optional final \
         message was suppressed (ADR-085: teardown must never be blocked)"
    );
    assert!(
        saw_prot_err,
        "the drift between this CLL's claimed source and its stale-composed final message \
         must be reported via PduErrEvtProtErr"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        written_before,
        "the stale final message must never reach the wire -- it would impersonate whichever \
         CLL (or nothing) now owns 0x80"
    );
    drop(events);

    server.shutdown().await;
}

/// ADR-180 Decision 20 (Codex review PR #72 round 20): a spontaneous
/// `RX_FLAG_J1939_ADDRESS_LOST`'s `stop_repeat_slots_for_cll` sweep can
/// itself fail to STOP a live repeat slot natively (e.g. a vendor DLL that
/// is momentarily unresponsive) -- before this decision, that failed
/// `MsgId` was silently dropped forever, leaving the device retransmitting
/// under a relinquished address for the rest of the session. This test
/// proves the fix end to end: with `__mock_set_stop_repeat_message_error`
/// armed, a spontaneous loss's STOP attempt fails and the slot stays live
/// (proving the failure is genuine, not a test artifact); once the
/// override is cleared, ANY subsequent repeat-message IOCTL on the same
/// physical channel -- issued here from a second, non-negotiated sibling
/// CLL sharing the channel, per `retry_leaked_repeat_message_stops`'s own
/// "top of every START/QUERY/STOP_REPEAT_MESSAGE handler" doc comment --
/// drains the leaked `MsgId` via the opportunistic retry mechanism
/// (`push_leaked_repeat_slots`/`record_leaked_repeat_slots`, ADR-165
/// Decision 6's pre-existing machinery), finally stopping the originally
/// leaked slot. Mirrors `spontaneous_loss_stops_the_clls_own_live_repeat_slots`'s
/// setup/injection shape.
#[tokio::test]
#[serial]
async fn spontaneous_loss_leaked_repeat_stop_is_drained_by_a_later_sibling_ioctl() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    // A second, non-negotiated sibling CLL sharing the same physical
    // channel (same resource id/pins) -- `CP_J1939_ADDR_NEG_RULE` bit 1 set
    // means it never runs the claim loop at all (`j1939_negotiated_unclaimed`
    // never blocks it), so it can freely issue `PDU_IOCTL_START_REPEAT_MESSAGE`
    // in step 7 below regardless of CLL A's own claim state, mirroring
    // `repeat_message_start_succeeds_on_non_negotiated_cll_with_no_claim`'s
    // setup. CLL A is the channel CREATOR here, so ADR-044's "shared-channel
    // creator-decides rule" means the sibling's own staged
    // `CP_J1939_ADDR_NEG_RULE` stays in Working, not promoted to Active at
    // connect time -- `promote_via_update_param` below explicitly promotes
    // it, same as `sibling_cll_skips_an_address_a_live_sibling_already_claimed`'s
    // own joining-CLL setup.
    let sibling_cll = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_TARGET_ADDRESS, 0x10),
            (CP_J1939_ADDR_NEG_RULE, 0b10), // bit 1 set: do not negotiate
        ],
        &[],
        2,
    )
    .await;
    promote_via_update_param(&mut client, sibling_cll).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and the sibling CLL should share one physical channel (same resource id/pins), \
         so the sibling's own repeat-message IOCTL below reaches the same leaked-id bookkeeping"
    );

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim 0x80");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80 before the repeat slot is started"
    );
    drop(events);

    // Condition 0 (`REPEAT_MESSAGE_UNTIL_MATCH`), NOT condition 1: condition
    // 1 (`REPEAT_MESSAGE_WHILE_MATCH`) self-terminates on any NON-matching
    // received frame (ADR-173 Decision 2), and the claim-loss injection
    // below (`[0x80]`, which never matches this slot's `pattern_data`
    // `[0x99]`) would otherwise self-terminate the slot on its own --
    // confounding this test's own forced-STOP-failure signal with an
    // unrelated self-termination path. Condition 0 only terminates on an
    // actual match, so the non-matching claim-loss frame leaves it alone,
    // isolating this test to the explicit STOP path under test.
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let msg_id = start_repeat_message(
        &mut client,
        cll_handle,
        start_id,
        repeat_message_setup(50, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a claimed J1939 CLL");
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "sanity: the slot should be live immediately after START"
    );

    // Arm the STOP failure: the spontaneous loss's own `stop_repeat_slots_for_cll`
    // sweep is about to hit this on every MsgId it tries to STOP.
    server
        .backdoor
        .set_stop_repeat_message_error(Some(j2534_0404::ERR_FAILED as std::os::raw::c_long));

    // Synthesize a spontaneous RX_FLAG_J1939_ADDRESS_LOST for 0x80 (this
    // CLL's currently claimed address) -- same injection
    // `spontaneous_loss_stops_the_clls_own_live_repeat_slots` uses, which
    // drives `deliver_j1939_claim_indication`'s spontaneous-loss branch
    // (`stop_repeat_slots_for_cll` then `record_leaked_repeat_slots`).
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x80],
        j2534_0404::PROTOCOL_J1939_PS,
        0x0002_0000, // RX_FLAG_J1939_ADDRESS_LOST, events.rs::RX_J1939_ADDRESS_LOST
    );

    // Give the spontaneous-loss handling ample time to run and attempt (and
    // fail) the STOP -- a fixed wait rather than a poll-until-condition
    // loop, since we are proving something did NOT happen (the slot must
    // stay live) and there is no eventual-success deadline to race here.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id),
        Some(1),
        "with the native STOP forced to fail, the spontaneous loss's own repeat-slot sweep must \
         leave the slot live rather than silently dropping the failed MsgId forever (the bug \
         ADR-180 Decision 20 fixes)"
    );

    // Disarm the failure, then trigger the opportunistic retry via ANY
    // repeat-message IOCTL on this physical channel -- here, the
    // non-negotiated sibling CLL's own START, which can succeed regardless
    // of CLL A's own claim state.
    server.backdoor.set_stop_repeat_message_error(None);
    start_repeat_message(
        &mut client,
        sibling_cll,
        start_id,
        repeat_message_setup(50, 1, vec![0x02], vec![0xFF], vec![0x77]),
    )
    .await
    .expect(
        "PDU_IOCTL_START_REPEAT_MESSAGE should succeed on the non-negotiated sibling CLL, \
         triggering retry_leaked_repeat_message_stops from the top of its own handler",
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
    let mut stopped = false;
    while std::time::Instant::now() < deadline {
        if server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id)
            .is_none()
        {
            stopped = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        stopped,
        "the sibling CLL's repeat-message IOCTL must drain the leaked STOP for CLL A's \
         originally leaked MsgId via the opportunistic retry (ADR-180 Decision 20 / ADR-165 \
         Decision 6) -- proving the leaked id was actually tracked and retried, not merely \
         forgotten"
    );

    server.shutdown().await;
}

/// ADR-180 Decision 21 (design-advisor consult, Codex review PR #72): the
/// SAE J1939 claim loop's bounded wait (`run_j1939_claim_loop`) must observe
/// an explicit `CancelComPrimitive` on the driving `CoptStartcomm` COP, not
/// merely run out to `CP_J1939AddrClaimTimeout` (staged here to 60s, well
/// beyond this test's own deadline) or exhaust the whole candidate list.
/// `CP_J1939AddrClaimTimeout` is staged large and `__mock_set_j1939_claim_
/// no_indication` is armed so the mock's `IOCTL_PROTECT_J1939_ADDR` claim
/// issue succeeds synchronously but never reports `J1939_ADDRESS_CLAIMED`/
/// `_LOST` -- unlike `__mock_set_j1939_claim_lost`'s immediate `_LOST` push
/// (`claim_retries_the_next_candidate_after_the_first_is_lost`), this is
/// what makes the claim loop's inner wait genuinely block (rather than
/// resolve within a single `POLL_INTERVAL_MS` tick), so a
/// `CancelComPrimitive` sent while it is in flight can be observed
/// deterministically instead of racing a ~10ms window. Proves both halves
/// of the fix: the COP reaches `PduCopstCancelled` promptly, and
/// `PduCopstFinished` (this protocol's `COMM_STARTED`-equivalent terminal
/// success) is never also reported for it.
#[tokio::test]
#[serial]
async fn cancel_com_primitive_during_the_claim_wait_ends_it_promptly_as_cancelled() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_TARGET_ADDRESS, 0x00),
            (CP_J1939_ADDR_CLAIM_TIMEOUT, 60_000_000),
        ],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    server.backdoor.set_j1939_claim_no_indication(true);

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Negotiation-enabled CLL, empty cop_data: drives the claim loop with no
    // optional StartComm message layered on top (isolates this test to
    // Decision 21's own claim-wait cancellation, not Decision 9's separate
    // post-claim transmit-phase cancel owner).
    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm, negotiation-enabled, empty cop_data) should \
             succeed synchronously",
        )
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_cop_executing(&mut client, cop_handle).await;

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    let mut saw_finished = false;
    let cancelled = wait_for_event(&mut events, 2000, |item| {
        if matches!(
            &item.data,
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32
        ) {
            saw_finished = true;
        }
        matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        )
    })
    .await;
    assert!(
        cancelled,
        "CancelComPrimitive during the claim loop's bounded wait should end it promptly with \
         PduCopstCancelled, well within this test's 2s deadline -- not silently hang until the \
         staged 60s CP_J1939AddrClaimTimeout elapses (the P1 regression Decision 21 fixes)"
    );
    assert!(
        !saw_finished,
        "a cancelled J1939 claim-driving CoptStartcomm must never also report PduCopstFinished \
         (this protocol's COMM_STARTED-equivalent) -- a cancelled COP must never look like it \
         succeeded"
    );

    drop(events);
    server.backdoor.set_j1939_claim_no_indication(false);
    server.shutdown().await;
}

/// ADR-180 Decision 22 (design-advisor consult, Codex review PR #72):
/// `cancel_j1939_claims_for_cll` used to remove a CLL's owned addresses from
/// `SharedChannel::j1939_claims` unconditionally, then attempt native
/// cancels, only ever LOGGING (never tracking) a failure -- freeing the
/// address for a new claimant (here: this SAME CLL's own imminent fresh
/// claim attempt, ADR-180 Decision 4) while the adapter may still be
/// defending it under the OLD claim: two claimants, one address. This
/// three-act test proves the fix end to end with a single CLL and a
/// single-candidate `[0x80]` list (no sibling needed, unlike this file's
/// other leak-observability tests -- `SharedChannel::leaked_j1939_claims`
/// blocks even THIS CLL's own next attempt at the same address):
///
/// 1. CLL A claims 0x80 normally (`COMM_STARTED`, proven via a send).
/// 2. `__mock_set_j1939_cancel_error` armed; `CoptStopcomm` then
///    `CoptStartcomm` again -- Decision 4's fresh-attempt sweep tries to
///    cancel 0x80, fails natively (the armed backdoor), and pushes it into
///    `leaked_j1939_claims` (Decision 22's own push site) while still
///    removing the routing entry unconditionally. The fresh claim loop's
///    own leaked-set guard (Decision 22's retry site) then tries ITS OWN
///    retry-cancel for the very same candidate, which ALSO fails (the
///    backdoor is still armed) -- the candidate is skipped, and the
///    single-candidate list exhausts: `PduErrEvtInitError` +
///    `PduCopstFinished`, no successful claim.
/// 3. Backdoor disarmed; `CoptStartcomm` once more (no intervening
///    `CoptStopcomm` needed -- the prior attempt never reached
///    `COMM_STARTED`, so nothing needs undoing first). Decision 4's own
///    fresh-attempt sweep is now a no-op (CLL A owns nothing in
///    `j1939_claims`, since step 2 already removed it), but the leaked-set
///    guard's retry-cancel for 0x80 now succeeds, removing it from
///    `leaked_j1939_claims` and falling through to a normal claim issue --
///    the claim succeeds (`COMM_STARTED`).
///
/// Fully synchronous against the mock (no `__mock_set_j1939_claim_no_
/// indication`, no timing windows) -- every step resolves within one
/// `POLL_INTERVAL_MS` tick, so this test should be fast and deterministic.
#[tokio::test]
#[serial]
async fn leaked_claim_cancel_blocks_a_fresh_attempt_until_the_native_cancel_finally_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    // -- Act 1: claim 0x80 normally. --
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's first attempt");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's first CoptStartcomm should finish and claim 0x80"
    );
    send_data(&mut client, cll_a, vec![0xAA], vec![]).await;
    let written_first = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written_first.get(3).copied(),
        Some(0x80),
        "CLL A should have claimed 0x80 on its first attempt; got {written_first:#04x?}"
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's first CoptSendrecv should finish"
    );

    // -- Act 2: arm the cancel-error backdoor, StopComm, StartComm again --
    // the fresh-attempt sweep's cancel of 0x80 fails and leaks it; the fresh
    // claim loop's own leaked-set guard retry ALSO fails (backdoor still
    // armed), so the single candidate is skipped and the list exhausts. --
    server.backdoor.set_j1939_cancel_error(true);

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, empty cop_data) should succeed on CLL A");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptStopcomm should finish"
    );

    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's second attempt");
    let mut saw_init_error = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "CLL A's second CoptStartcomm should still finish (PduCopstFinished), not hang"
    );
    assert!(
        saw_init_error,
        "CLL A's second claim attempt should fail closed while the leaked cancel for 0x80 is \
         still unresolved (both the fresh-attempt sweep's own cancel AND the leaked-set guard's \
         retry-cancel fail while the backdoor is armed) -- it must NOT silently re-claim 0x80 \
         out from under the still-in-doubt prior claim"
    );

    // -- Act 3: disarm the backdoor, StartComm once more -- the leaked-set
    // guard's retry-cancel now succeeds, 0x80 is removed from
    // leaked_j1939_claims, and the claim succeeds normally. --
    server.backdoor.set_j1939_cancel_error(false);

    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's third attempt");
    let mut saw_init_error_third = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error_third = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "CLL A's third CoptStartcomm should finish and claim 0x80, now that the leaked cancel \
         has finally succeeded"
    );
    assert!(
        !saw_init_error_third,
        "CLL A's third claim attempt should succeed once the leaked-set guard's retry-cancel \
         finally succeeds (the fix this test proves) -- if this fails, 0x80 stayed permanently \
         blocked by its own unresolved leaked cancel"
    );
    drop(events);

    send_data(&mut client, cll_a, vec![0xCC], vec![]).await;
    let written_third = server.backdoor.written_data(MOCK_CHANNEL_ID, 1);
    assert_eq!(
        written_third.get(3).copied(),
        Some(0x80),
        "CLL A should have re-claimed 0x80 on its third attempt; got {written_third:#04x?}"
    );

    server.shutdown().await;
}

/// ADR-180 Decision 22's round-23 correction (Codex review, PR #72;
/// `design-advisor` consult): the ORIGINAL leaked-set guard above only
/// checked whether the CURRENT candidate's own address was in
/// `SharedChannel::leaked_j1939_claims` -- a fresh attempt whose
/// `CP_J1939PreferredAddress` list does NOT include the leaked address
/// bypassed the guard entirely, issuing a native claim for an unrelated
/// address while the adapter might still defend the leaked one. This test
/// proves the fix: a channel-wide reconcile-then-gate at the top of every
/// outer-loop iteration, before any per-candidate logic runs at all.
///
/// 1. CLL A claims 0x80 (candidates `[0x80]`) normally.
/// 2. `__mock_set_j1939_cancel_error` armed; `CoptStopcomm`, then
///    `SetComParam CP_J1939PreferredAddress = [0x81]` (DISJOINT from
///    0x80 -- this is precisely the bypass the original guard missed),
///    then `CoptStartcomm` again -- Decision 4's fresh-attempt sweep tries
///    to cancel 0x80, fails, and leaks it. The new channel-wide gate
///    reconciles every leaked address BEFORE considering candidate 0x81 at
///    all: its own retry-cancel for 0x80 also fails (backdoor still
///    armed), so the WHOLE attempt fails closed -- `PduErrEvtInitError` +
///    `PduCopstFinished`, and critically 0x81 is NEVER claimed (the
///    regression this test guards: before the fix, this act would claim
///    0x81 while 0x80 stays possibly still defended).
/// 3. Backdoor disarmed; `CoptStartcomm` once more -- the gate reconciles
///    0x80 successfully, THEN proceeds to claim 0x81 normally
///    (`COMM_STARTED`), proven by a send whose source address is 0x81, not
///    0x80.
///
/// Fully synchronous against the mock, no timing windows.
#[tokio::test]
#[serial]
async fn leaked_claim_on_a_disjoint_candidate_list_still_blocks_the_whole_attempt() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    // -- Act 1: claim 0x80 normally. --
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's first attempt");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's first CoptStartcomm should finish and claim 0x80"
    );

    // -- Act 2: arm the cancel-error backdoor, StopComm, switch to a
    // candidate list that does NOT include 0x80, StartComm again -- the
    // fresh-attempt sweep leaks 0x80; the channel-wide gate's own
    // reconcile pass (not a per-candidate check) must still catch it even
    // though 0x81 was never itself leaked. --
    server.backdoor.set_j1939_cancel_error(true);

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, empty cop_data) should succeed on CLL A");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptStopcomm should finish"
    );

    set_com_param_bytes(&mut client, cll_a, CP_J1939_PREFERRED_ADDRESS, vec![0x81]).await;
    // `promote_via_update_param` issues its own `CoptUpdateparam` on this
    // same CLL and event subscription -- it settles via a plain sleep, not
    // by draining `events`, so its own `PduCopstFinished` notification is
    // still sitting unconsumed in the stream. The second `CoptStartcomm`
    // below is therefore captured by `cop_handle` and every predicate
    // filters on it explicitly, so a stale `Finished` left over from the
    // `CoptUpdateparam` can never be mistaken for this COP's own outcome.
    promote_via_update_param(&mut client, cll_a).await;

    let second_startcomm_cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's second attempt")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    let mut saw_init_error = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            let this_cop = item.cop_handle.as_ref().map(|c| c.cop_handle)
                == Some(second_startcomm_cop.cop_handle);
            if this_cop
                && matches!(
                    &item.data,
                    Some(event_item::Data::ErrorData(error))
                        if *error == PduErrorEvent::PduErrEvtInitError as i32
                )
            {
                saw_init_error = true;
            }
            this_cop
                && matches!(
                    item.data,
                    Some(event_item::Data::CopStatus(status))
                        if status == PduComPrimitiveStatus::PduCopstFinished as i32
                )
        })
        .await,
        "CLL A's second CoptStartcomm should still finish (PduCopstFinished), not hang"
    );
    assert!(
        saw_init_error,
        "CLL A's second claim attempt (candidates = [0x81], disjoint from the leaked 0x80) must \
         still fail closed while 0x80 remains unreconciled -- it must NOT silently claim 0x81 \
         while 0x80 might still be defended by the adapter under the same NAME"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "CLL A must not have transmitted anything claiming 0x81 while 0x80 is still leaked"
    );

    // -- Act 3: disarm the backdoor, StartComm once more -- the gate
    // reconciles 0x80, then proceeds to claim 0x81 normally. --
    server.backdoor.set_j1939_cancel_error(false);

    let third_startcomm_cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's third attempt")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    let mut saw_init_error_third = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            let this_cop = item.cop_handle.as_ref().map(|c| c.cop_handle)
                == Some(third_startcomm_cop.cop_handle);
            if this_cop
                && matches!(
                    &item.data,
                    Some(event_item::Data::ErrorData(error))
                        if *error == PduErrorEvent::PduErrEvtInitError as i32
                )
            {
                saw_init_error_third = true;
            }
            this_cop
                && matches!(
                    item.data,
                    Some(event_item::Data::CopStatus(status))
                        if status == PduComPrimitiveStatus::PduCopstFinished as i32
                )
        })
        .await,
        "CLL A's third CoptStartcomm should finish and claim 0x81, now that 0x80's leaked \
         cancel has finally succeeded"
    );
    assert!(
        !saw_init_error_third,
        "CLL A's third claim attempt should succeed once the gate's reconcile pass finally \
         succeeds for 0x80"
    );
    drop(events);

    send_data(&mut client, cll_a, vec![0xCC], vec![]).await;
    let written_third = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written_third.get(3).copied(),
        Some(0x81),
        "CLL A should have claimed 0x81 (not 0x80) on its third attempt; got \
         {written_third:#04x?}"
    );

    server.shutdown().await;
}

/// Round-24 correction to Decision 18 (Codex review, PR #72; `design-advisor`
/// consult): the negotiation opt-out branch discarded `cancel_j1939_claims_
/// for_cll`'s own outcome and unconditionally promoted this CLL to
/// `OptedOut`, even when the native cancel of its prior claim failed and
/// leaked the address into `SharedChannel::leaked_j1939_claims` -- letting
/// the client's StartComm complete and transmit under a NEW client-managed
/// source while the adapter might still defend the OLD address under the
/// same NAME. Unlike the negotiation-requesting branch, this branch never
/// calls `run_j1939_claim_loop`, so it never revisited round-23's own
/// channel-wide reconcile-then-gate. Proves the opt-out StartComm now fails
/// closed (posture stays gated, not promoted to `OptedOut`) while the leak
/// persists, and succeeds -- with the client-managed source address actually
/// taking effect -- once the shared `reconcile_leaked_j1939_claims` retry
/// (extracted from round-23's own block so both call sites share one
/// mechanism) clears it.
#[tokio::test]
#[serial]
async fn opt_out_startcomm_fails_closed_while_a_relinquished_claim_remains_leaked() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // -- Act 1: claim 0x80 normally. --
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's first attempt");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's first CoptStartcomm should finish and claim 0x80"
    );

    // -- Act 2: arm the cancel-error backdoor, StopComm, opt out of
    // negotiation, StartComm again -- the opt-out branch's own relinquish
    // cancel fails and leaks 0x80; the round-24 gate must fail this
    // StartComm closed instead of completing it and promoting `OptedOut`. --
    server.backdoor.set_j1939_cancel_error(true);

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, empty cop_data) should succeed on CLL A");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptStopcomm should finish"
    );

    set_com_param_unum32(&mut client, cll_a, CP_J1939_ADDR_NEG_RULE, 0b10).await;
    promote_via_update_param(&mut client, cll_a).await;
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptUpdateparam (opting out of negotiation) should finish"
    );

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's opt-out attempt");
    let mut saw_init_error = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "CLL A's opt-out CoptStartcomm should still finish (PduCopstFinished), not hang"
    );
    assert!(
        saw_init_error,
        "the opt-out StartComm must fail closed while 0x80 remains leaked -- it must NOT \
         silently promote OptedOut and let CLL A transmit under a new source while the adapter \
         may still defend 0x80 under the same NAME"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "no optional message or send should reach the mock while the opt-out StartComm is \
         failing closed"
    );

    // Posture was left untouched (never promoted to `OptedOut`), so
    // `j1939_negotiated_unclaimed_for` still reads this CLL as
    // negotiated-and-unclaimed -- an ordinary transmitting CoptSendrecv must
    // stay rejected, the same as before this CLL ever tried to opt out.
    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0xAA],
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
        .expect_err(
            "an ordinary CoptSendrecv must still be rejected -- the opt-out never actually took \
             effect while 0x80 remained leaked",
        );
    assert_eq!(status.code(), Code::FailedPrecondition);

    // -- Act 3: disarm the backdoor, opt out once more -- the gate
    // reconciles 0x80, the opt-out actually takes effect, and an ordinary
    // send under the client-managed source now succeeds. --
    server.backdoor.set_j1939_cancel_error(false);

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm) should succeed on CLL A's second opt-out \
             attempt",
        );
    let mut saw_init_error_2 = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error_2 = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "CLL A's second opt-out CoptStartcomm should finish"
    );
    assert!(
        !saw_init_error_2,
        "the opt-out should now succeed once the leaked 0x80 finally reconciles"
    );
    drop(events);

    set_com_param_unum32(&mut client, cll_a, j2534_0404::NODE_ADDRESS, 0x50).await;
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptUpdateparam changing CP_TesterSourceAddress should now succeed -- the opt-out \
             genuinely took effect",
        );

    send_data(&mut client, cll_a, vec![0xAA], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.get(3).copied(),
        Some(0x50),
        "CLL A should now send under its own client-managed source address; got {written:#04x?}"
    );

    server.shutdown().await;
}

/// ADR-180 Decision 23 (design-advisor consult, Codex review PR #72;
/// round-25 correction): `owned_by_a_live_sibling` only ever checks the
/// CURRENT candidate's own address, so two sibling CLLs sharing a
/// `CP_J1939Name` but configured with DISJOINT `CP_J1939PreferredAddress`
/// candidate lists could each successfully claim a different address --
/// the adapter defending one SAE J1939 identity at two source addresses
/// simultaneously. Proves the new NAME-collision gate fails CLL B's
/// StartComm closed while CLL A's claim under the same NAME survives, then
/// proves CLL B can claim once CLL A relinquishes.
#[tokio::test]
#[serial]
async fn name_collision_blocks_a_sibling_claiming_a_disjoint_address() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x60]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;
    let cll_b = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x61]),
            // Same NAME as CLL A -- disjoint candidate list (0x61, not
            // 0x60), so `owned_by_a_live_sibling` alone would never
            // intersect CLL A's own claimed address.
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        2,
    )
    .await;
    promote_via_update_param(&mut client, cll_b).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptStartcomm should finish and claim 0x60"
    );
    drop(events_a);

    // `promote_via_update_param` (called on CLL B before either CLL's
    // StartComm) issued its own `CoptUpdateparam` on this same CLL, which
    // emits its own `PduCopstFinished` -- settled via a plain sleep, not by
    // draining any stream, so it is still sitting unconsumed once `events_b`
    // subscribes. Both `CoptStartcomm` attempts below are therefore issued
    // directly (not via the `start_comm` helper, which discards the
    // returned `cop_handle`) and every `wait_for_event` predicate filters on
    // the captured `cop_handle`, so a stale `Finished` left over from
    // `promote_via_update_param` (or from CLL B's own first, rejected
    // attempt) can never be mistaken for either attempt's own outcome --
    // matching the exact precedent this file already established for
    // `leaked_claim_on_a_disjoint_candidate_list_still_blocks_the_whole_attempt`.
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let first_startcomm_cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL B")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    let mut saw_init_error_b = false;
    assert!(
        wait_for_event(&mut events_b, 2000, |item| {
            let this_cop = item.cop_handle.as_ref().map(|c| c.cop_handle)
                == Some(first_startcomm_cop.cop_handle);
            if this_cop
                && matches!(
                    &item.data,
                    Some(event_item::Data::ErrorData(error))
                        if *error == PduErrorEvent::PduErrEvtInitError as i32
                )
            {
                saw_init_error_b = true;
            }
            this_cop
                && matches!(
                    item.data,
                    Some(event_item::Data::CopStatus(status))
                        if status == PduComPrimitiveStatus::PduCopstFinished as i32
                )
        })
        .await,
        "CLL B's CoptStartcomm should still finish (PduCopstFinished), not hang"
    );
    assert!(
        saw_init_error_b,
        "CLL B must fail closed -- 0x61 was never itself claimed, but CLL B's NAME collides \
         with CLL A's live claim under a different address (0x60)"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "CLL B must not have transmitted anything claiming 0x61 while its NAME is already \
         claimed by CLL A"
    );

    send_data(&mut client, cll_a, vec![0xAA], vec![]).await;
    let written_a = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written_a.get(3).copied(),
        Some(0x60),
        "CLL A's own claim must be completely undisturbed by CLL B's rejected attempt; got \
         {written_a:#04x?}"
    );

    // CLL A relinquishes -- a plain CoptStopcomm alone does NOT cancel a
    // live claim (ADR-180 Decision 4 only cancels on a fresh re-StartComm
    // attempt for the SAME CLL); disconnecting is Decision 1's own
    // unconditional teardown cancel, the simplest way to actually free the
    // NAME for CLL B's retry.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnecting CLL A should not error or hang");

    let second_startcomm_cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL B's second attempt")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    let mut saw_init_error_b2 = false;
    assert!(
        wait_for_event(&mut events_b, 2000, |item| {
            let this_cop = item.cop_handle.as_ref().map(|c| c.cop_handle)
                == Some(second_startcomm_cop.cop_handle);
            if this_cop
                && matches!(
                    &item.data,
                    Some(event_item::Data::ErrorData(error))
                        if *error == PduErrorEvent::PduErrEvtInitError as i32
                )
            {
                saw_init_error_b2 = true;
            }
            this_cop
                && matches!(
                    item.data,
                    Some(event_item::Data::CopStatus(status))
                        if status == PduComPrimitiveStatus::PduCopstFinished as i32
                )
        })
        .await,
        "CLL B's second CoptStartcomm should finish"
    );
    assert!(
        !saw_init_error_b2,
        "CLL B should now claim 0x61 once CLL A has relinquished the shared NAME"
    );
    drop(events_b);

    server.shutdown().await;
}

/// ADR-180 Decision 23 (design-advisor consult, Codex review PR #72;
/// round-25 correction): `CoptUpdateparam` promoting `CP_J1939Name` to a
/// value DIFFERENT from the NAME this CLL's live claim negotiation owns
/// must be rejected -- the enqueue-time counterpart of the pre-existing
/// `CP_TesterSourceAddress` guard (Decision 12).
#[tokio::test]
#[serial]
async fn coptupdateparam_rejects_j1939_name_off_the_live_claim() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim under the NAME");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80 before CoptUpdateparam is issued"
    );
    drop(events);

    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_J1939_NAME,
        vec![8, 7, 6, 5, 4, 3, 2, 1],
    )
    .await;

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
            "CoptUpdateparam promoting CP_J1939Name to a different NAME while a claim is live \
             must be rejected",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("CP_J1939Name"),
        "the rejection should name CP_J1939Name: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Negative counterpart to the test above: re-staging `CP_J1939Name` back
/// to the SAME value the live claim already owns must NOT be rejected --
/// proves the guard compares against the live Active value, not merely
/// whether the ComParam is staged at all.
#[tokio::test]
#[serial]
async fn coptupdateparam_allows_restaging_the_same_j1939_name_as_the_live_claim() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed and claim under the NAME");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80 before CoptUpdateparam is issued"
    );
    drop(events);

    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_J1939_NAME,
        vec![1, 2, 3, 4, 5, 6, 7, 8],
    )
    .await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptUpdateparam re-staging CP_J1939Name back to the already-claimed value must \
             succeed",
        );

    server.shutdown().await;
}

/// ADR-180 Decision 23's execution-time counterpart (Decision 15's own
/// established TOCTOU-closing shape): a `CoptUpdateparam` staging a
/// different `CP_J1939Name` is enqueued while suspended, alongside a
/// `CoptStartcomm` that has not claimed anything yet -- Decision 23's own
/// enqueue-time guard sees no live claim and lets it through. Once resumed,
/// the queue drains FIFO: `CoptStartcomm` claims first, THEN
/// `CoptUpdateparam` reaches this execution-time check with a now-live
/// claim and a now-genuine NAME drift the enqueue-time snapshot could not
/// have seen.
#[tokio::test]
#[serial]
async fn coptupdateparam_execution_time_check_catches_a_name_change_that_lands_after_enqueue() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_handle, suspend_id, None, false)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm) should be accepted synchronously (siphoned \
             into tx_held while suspended)",
        );

    // Decision 23's own enqueue-time guard passes here: no claim has been
    // attempted yet (CoptStartcomm is still held, not executed), so
    // `SharedChannel::j1939_claims` has no entry for this CLL at all. Also
    // stage `CP_TesterSourceAddress` to the address the pending claim will
    // actually win (0x80, the CLL's only candidate) -- otherwise this
    // CoptUpdateparam's own Working snapshot would still carry the pre-claim
    // default (0xF1), and Decision 12/15's OWN pre-existing NODE_ADDRESS
    // execution-time re-check would fire first once CoptStartcomm's claim
    // write-back promotes Active's NODE_ADDRESS to 0x80 in the interim --
    // masking whether the NAME-specific re-check under test here is itself
    // load-bearing.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::NODE_ADDRESS, 0x80).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_J1939_NAME,
        vec![8, 7, 6, 5, 4, 3, 2, 1],
    )
    .await;
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptUpdateparam should enqueue successfully -- Decision 23's own guard sees no \
             live claim yet",
        );

    io_ctl_cll(&mut client, cll_handle, resume_id, None, false)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    // CoptStartcomm dispatches first (FIFO), claims 0x80 under the ORIGINAL
    // NAME, and finishes.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x80 before CoptUpdateparam executes"
    );

    // CoptUpdateparam dispatches next -- the execution-time check must now
    // catch the drift the enqueue-time snapshot could not see: staged
    // CP_J1939Name differs from Active (the just-claimed NAME), and
    // SharedChannel::j1939_claims now owns this exact (cll_handle,
    // connect_generation).
    let mut saw_prot_err = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtProtErr as i32
            ) {
                saw_prot_err = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstCancelled as i32
            )
        })
        .await,
        "CoptUpdateparam should be cancelled by the execution-time re-check"
    );
    assert!(
        saw_prot_err,
        "the cancellation should surface PDU_ERR_EVT_PROT_ERR"
    );

    // No hardware write ever reached the mock for the rejected COP.
    drop(events);
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the rejected CoptUpdateparam must never have pushed anything to hardware"
    );

    server.shutdown().await;
}

/// ADR-180 Decision 23 round-26 correction (design-advisor consult, Codex
/// review PR #72 round 26): a `temp_param_update = 1` `CoptStartcomm` can
/// claim a SAE J1939 address under a `CP_J1939Name` that only ever exists
/// in the Working/Temp-bound snapshot, never promoted to Active -- mirroring
/// how every other Temp-bound J1939 framing field in this file already
/// works (see `optional_startcomm_message_after_claim_uses_the_bound_
/// snapshots_other_header_fields`). `SharedChannel::j1939_claims`'s own
/// `J1939ClaimEntry.name` correctly records this actually-issued NAME, but
/// before this fix a later SPONTANEOUS reclaim (`run_j1939_reclaim_duties`)
/// rebuilt its claim params from Active's own `CP_J1939Name` instead,
/// silently reclaiming under the wrong identity. Proves the fix using this
/// file's own established "no direct backdoor into `SharedChannel::
/// j1939_claims`" proof pattern (see `opting_out_of_negotiation_after_a_
/// claim_frees_the_address_for_a_sibling`'s doc comment, and the NAME-
/// collision gate `name_collision_blocks_a_sibling_claiming_a_disjoint_
/// address` exercises directly): sibling CLL B claims a DIFFERENT address
/// (0x70) under the exact NAME CLL A's own (stale) Active carries but CLL A
/// never actually claims under -- if CLL A's spontaneous reclaim wrongly
/// rebuilt its NAME from Active, it would collide with B's live claim under
/// that gate and fail closed, permanently leaving CLL A unclaimed (and
/// therefore rejecting any further ordinary send, per `events_j1939_claim.
/// rs::j1939_negotiated_unclaimed_for`). With the fix, CLL A's reclaim
/// carries its own genuinely-claimed Temp-bound NAME, which does not
/// collide with B's, and CLL A successfully reclaims its own address
/// (0x71).
#[tokio::test]
#[serial]
async fn spontaneous_reclaim_after_a_temp_bound_claim_uses_the_temp_bound_name_not_active() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    const NAME_ACTIVE: [u8; 8] = [9, 9, 9, 9, 9, 9, 9, 9];
    const NAME_TEMP: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    // CLL B (channel creator): claims 0x70 under NAME_ACTIVE -- the exact
    // NAME CLL A's own Active snapshot will carry but never actually claim
    // under, the bait for the round-25 bug.
    let cll_b = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x70]),
            (CP_J1939_NAME, NAME_ACTIVE.to_vec()),
        ],
        1,
    )
    .await;
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL B");
    assert!(
        wait_for_event(&mut events_b, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL B's CoptStartcomm should finish and claim 0x70 under NAME_ACTIVE"
    );
    drop(events_b);

    // CLL A (joiner): Active carries NAME_ACTIVE too (staged before its own
    // join-time CoptUpdateparam promotion), but its actual claim runs under
    // a Working-ONLY, Temp-bound override (NAME_TEMP) that is never
    // promoted to Active.
    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x71]),
            (CP_J1939_NAME, NAME_ACTIVE.to_vec()),
        ],
        2,
    )
    .await;
    promote_via_update_param(&mut client, cll_a).await;

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL B should share one physical channel (same resource id/pins)"
    );

    // Working-only override: never promoted to Active by any further
    // CoptUpdateparam in this test.
    set_com_param_bytes(&mut client, cll_a, CP_J1939_NAME, NAME_TEMP.to_vec()).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let startcomm_cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStartcomm, temp_param_update=1) should succeed on CLL A")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    let mut saw_init_error_a = false;
    assert!(
        wait_for_event(&mut events_a, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error_a = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ) && item.cop_handle.as_ref().map(|c| c.cop_handle) == Some(startcomm_cop.cop_handle)
        })
        .await,
        "CLL A's Temp-bound CoptStartcomm should finish and claim 0x71 under NAME_TEMP"
    );
    assert!(
        !saw_init_error_a,
        "CLL A's initial Temp-bound claim under NAME_TEMP (distinct from CLL B's NAME_ACTIVE) \
         should not collide with CLL B's claim"
    );
    drop(events_a);

    send_data(&mut client, cll_a, vec![0xAA], vec![]).await;
    let written_first = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written_first.get(3).copied(),
        Some(0x71),
        "CLL A should have claimed 0x71 under its Temp-bound NAME_TEMP; got {written_first:#04x?}"
    );

    // Synthesize a spontaneous RX_FLAG_J1939_ADDRESS_LOST for 0x71 (CLL A's
    // currently claimed address) -- exercises `deliver_j1939_claim_
    // indication`'s spontaneous-loss path, which arms `SharedChannel::
    // j1939_reclaim_pending` for CLL A.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x71],
        j2534_0404::PROTOCOL_J1939_PS,
        0x0002_0000, // RX_FLAG_J1939_ADDRESS_LOST, events.rs::RX_J1939_ADDRESS_LOST
    );

    // No client-visible event marks the spontaneous reclaim's completion --
    // give the channel poll task's periodic `run_due_tick_duties` several
    // ticks (POLL_INTERVAL_MS = 10ms) to pick up `j1939_reclaim_pending` and
    // run the fresh claim loop to completion.
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // The key check: if the reclaim had wrongly rebuilt its NAME from
    // Active (NAME_ACTIVE, round-25's bug), it would collide with CLL B's
    // live claim under the SAME NAME and fail closed (the `name_owned_by_a_
    // live_sibling` gate), leaving CLL A permanently unclaimed -- a
    // subsequent ordinary CoptSendrecv on an unclaimed, negotiation-engaged
    // CLL is itself rejected synchronously, so `send_data` below would
    // panic on a regression, not merely assert the wrong byte.
    send_data(&mut client, cll_a, vec![0xBB], vec![]).await;
    let written_second = server.backdoor.written_data(MOCK_CHANNEL_ID, 1);
    assert_eq!(
        written_second.get(3).copied(),
        Some(0x71),
        "CLL A's spontaneous reclaim should have re-claimed 0x71 under its own Temp-bound \
         NAME_TEMP, not Active's NAME_ACTIVE (which would have collided with CLL B's live \
         claim); got {written_second:#04x?}"
    );

    server.shutdown().await;
}

/// ADR-180 Decision 23 round-26 correction, enqueue-time counterpart: the
/// `CoptUpdateparam` NAME-drift guard (`rpc_primitive.rs`) must compare a
/// restaged `CP_J1939Name` against the NAME this CLL's live claim actually
/// issued (`SharedChannel::j1939_claims`'s own entry), not Active's own
/// `CP_J1939Name` -- the two diverge for a `temp_param_update = 1`
/// `CoptStartcomm` that claimed under a Working/Temp-bound NAME never
/// promoted to Active (mirrors this file's `spontaneous_reclaim_after_a_
/// temp_bound_claim_uses_the_temp_bound_name_not_active` setup above).
/// Before this fix, the guard's own `active_j1939_name` snapshot would
/// wrongly (a) REJECT restaging the SAME Temp-bound-claimed NAME (since it
/// differs from stale Active), and (b) ALLOW restaging Active's own,
/// never-actually-claimed-under NAME (since it trivially matches itself) --
/// both mispredictions this test exercises directly.
#[tokio::test]
#[serial]
async fn coptupdateparam_j1939_name_guard_compares_against_the_temp_bound_claimed_name_not_active()
{
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    const NAME_ACTIVE: [u8; 8] = [9, 9, 9, 9, 9, 9, 9, 9];
    const NAME_TEMP: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    let cll_handle = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x72]),
            (CP_J1939_NAME, NAME_ACTIVE.to_vec()),
        ],
        1,
    )
    .await;

    // Working-only override: the claim below runs under NAME_TEMP, never
    // promoted to Active, which stays at NAME_ACTIVE.
    set_com_param_bytes(&mut client, cll_handle, CP_J1939_NAME, NAME_TEMP.to_vec()).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm, temp_param_update=1) should succeed and claim \
             0x72 under NAME_TEMP",
        );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish and claim 0x72 under NAME_TEMP before CoptUpdateparam is \
         issued"
    );
    drop(events);

    send_data(&mut client, cll_handle, vec![0xAA], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.get(3).copied(),
        Some(0x72),
        "CLL should have claimed 0x72 under NAME_TEMP; got {written:#04x?}"
    );

    // (a) Restaging the SAME Temp-bound-claimed NAME must succeed -- the
    // previously-mispredicted false-rejection case: the old Active-
    // comparison logic would have wrongly rejected this, since NAME_TEMP
    // differs from stale Active (NAME_ACTIVE).
    set_com_param_bytes(&mut client, cll_handle, CP_J1939_NAME, NAME_TEMP.to_vec()).await;
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptUpdateparam re-staging CP_J1939Name back to the actually-claimed Temp-bound \
             value (NAME_TEMP) must succeed",
        );

    // (b) Restaging Active's OWN NAME (NAME_ACTIVE, never actually claimed
    // under) must be REJECTED -- the old logic would have wrongly ALLOWED
    // this, since it matched stale Active.
    set_com_param_bytes(&mut client, cll_handle, CP_J1939_NAME, NAME_ACTIVE.to_vec()).await;
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
            "CoptUpdateparam re-staging CP_J1939Name to Active's own, never-actually-claimed \
             NAME (NAME_ACTIVE) must be rejected -- the live claim owns NAME_TEMP, not \
             NAME_ACTIVE",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("CP_J1939Name"),
        "the rejection should name CP_J1939Name: {}",
        status.message()
    );

    server.shutdown().await;
}

/// ADR-180 Decision 24 (round-27 correction to Decision 22, Codex review, PR
/// #72; `design-advisor` consult): `ioctl_start_repeat_message`'s own J1939
/// gate (`events::j1939_negotiated_unclaimed`, a few lines above the
/// `LOCK_PHYSICAL_TX_QUEUE` check) only ever covers the CALLING CLL's own
/// negotiation posture -- it says nothing about `SharedChannel::
/// leaked_j1939_claims`, a channel-wide leak a DIFFERENT, already-torn-down
/// sibling CLL's relinquishment left behind (Decision 22). An OPTED-OUT
/// sibling (`CP_J1939AddressNegotiationRule` bit 1 set, client-managed
/// source, `j1939_claimed_address` always `None` by design) sails straight
/// past that gate and, before this fix, could start an autonomous
/// device-side repeat slot while the adapter might still be defending a
/// leaked address on the SAME physical channel.
///
/// Setup mirrors `leaked_claim_cancel_blocks_a_fresh_attempt_until_the_
/// native_cancel_finally_succeeds`'s own `__mock_set_j1939_cancel_error`
/// leak-seeding shape:
/// 1. CLL A (negotiated) claims 0x80 normally.
/// 2. CLL B (opted-out sibling, SAME physical channel, client-managed
///    source) is created and connected -- deliberately never calls
///    `CoptStartcomm` at all, matching `repeat_message_start_succeeds_on_
///    non_negotiated_cll_with_no_claim`'s own "no claim loop ever runs for
///    this CLL" setup.
/// 3. The cancel-error backdoor armed, CLL A's `CoptStopcomm` then a fresh
///    `CoptStartcomm` -- the fresh-attempt sweep's own cancel of 0x80 fails
///    and leaks it (Decision 22); the claim loop's own leaked-set retry
///    ALSO fails (backdoor still armed), so CLL A's single-candidate list
///    exhausts. `SharedChannel::leaked_j1939_claims` now holds `[0x80]`.
/// 4. CLL B (the opted-out sibling, which never touched J1939 claim state
///    at all) attempts `PDU_IOCTL_START_REPEAT_MESSAGE` -- must now be
///    rejected by this fix's new gate (`FailedPrecondition`/
///    `PduErrCllNotStarted`), proving the leak blocks a CLL the OLD
///    (pre-fix) `j1939_negotiated_unclaimed`-only gate could never reach.
/// 5. The backdoor cleared, CLL B retries -- the gate's own reconcile
///    finally succeeds (0x80's retry-cancel clears), so the START now
///    succeeds (proof `leaked_j1939_claims` is empty afterward: the gate's
///    own contract, `reconcile_leaked_j1939_claims`'s returned `bool`, is
///    exactly "the list is empty once the retry completes" -- a successful
///    START is only reachable when that holds).
#[tokio::test]
#[serial]
async fn leaked_claim_blocks_repeat_message_start_on_an_opted_out_sibling_cll() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;
    let cll_b = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[
            (CP_J1939_TARGET_ADDRESS, 0x10),
            (CP_J1939_ADDR_NEG_RULE, 0b10), // bit 1 set: do not negotiate
        ],
        &[],
        2,
    )
    .await;
    // CLL B is a JOINING CLL on this shared channel (`rpc_link.rs`'s own
    // "Joining CLLs: Active stays default until the CLL issues
    // CoptUpdateparam" rule) -- its Active `CP_J1939AddressNegotiationRule`
    // stays at the seeded (negotiated) default until promoted, the same
    // promotion `name_collision_blocks_a_sibling_claiming_a_disjoint_
    // address` already needs for its own second sibling CLL.
    promote_via_update_param(&mut client, cll_b).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL B should share one physical channel (same resource id/pins)"
    );

    // -- Act 1: CLL A claims 0x80 normally. --
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's first attempt");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's first CoptStartcomm should finish and claim 0x80"
    );

    // -- Act 2: leak 0x80 -- backdoor armed, StopComm then a fresh
    // StartComm on CLL A exhausts (fresh-attempt sweep's own cancel fails
    // and leaks 0x80; the claim loop's own leaked-set retry-cancel for the
    // same address also fails while the backdoor stays armed). --
    server.backdoor.set_j1939_cancel_error(true);

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, empty cop_data) should succeed on CLL A");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptStopcomm should finish"
    );

    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A's second attempt");
    let mut saw_init_error_a = false;
    assert!(
        wait_for_event(&mut events_a, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error_a = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "CLL A's second CoptStartcomm should still finish (PduCopstFinished), not hang"
    );
    assert!(
        saw_init_error_a,
        "CLL A's second claim attempt should fail closed while its own cancel of 0x80 remains \
         unresolved, leaking it into SharedChannel::leaked_j1939_claims"
    );
    drop(events_a);

    // -- Act 3: CLL B -- the OPTED-OUT sibling, which never calls
    // CoptStartcomm and whose own `j1939_negotiated_unclaimed` gate can
    // therefore never fire -- attempts PDU_IOCTL_START_REPEAT_MESSAGE while
    // 0x80 is still leaked. This fix's new channel-wide reconcile-then-gate
    // must still reject it. --
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let status = start_repeat_message(
        &mut client,
        cll_b,
        start_id,
        repeat_message_setup(50, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect_err(
        "PDU_IOCTL_START_REPEAT_MESSAGE on an opted-out sibling CLL must be rejected while a \
         leaked SAE J1939 claim on the same physical channel remains unresolved, even though \
         this CLL's own negotiation posture never engages the claim machinery at all",
    );
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("leaked"),
        "the rejection should explain the leaked claim: {}",
        status.message()
    );

    // -- Act 4: clear the backdoor and retry -- the gate's own reconcile
    // finally succeeds, so the START now succeeds. A successful START is
    // only reachable when `reconcile_leaked_j1939_claims` returned `true`
    // (its own contract: "the list is empty once the retry completes"), so
    // this positively proves leaked_j1939_claims is empty afterward. --
    server.backdoor.set_j1939_cancel_error(false);

    start_repeat_message(
        &mut client,
        cll_b,
        start_id,
        repeat_message_setup(50, 0, vec![0x01], vec![0xFF], vec![0x99]),
    )
    .await
    .expect(
        "PDU_IOCTL_START_REPEAT_MESSAGE on the opted-out sibling should now succeed once the \
         leaked claim finally reconciles",
    );

    server.shutdown().await;
}

/// ADR-180 Decision 24 (Codex review, PR #72; `design-advisor` consult):
/// `run_j1939_reclaim_duties` always drove `run_j1939_claim_loop` with
/// `cancel_cop: None`, so the loop's own `cancelled_cops` cancellation check
/// (Decision 21) could never fire for it -- meaning a spontaneous reclaim
/// could never observe a concurrent `CoptStopcomm`, even though
/// `CoptStopcomm` sets `LogicalLinkState::stop_comm_pending = true`
/// synchronously (`rpc_primitive.rs`). Since the reclaim runs on the SAME
/// physical channel's single poll task as the queued `CoptStopcomm` work,
/// the queued StopComm could not execute until the reclaim either succeeded
/// or exhausted its whole candidate list -- a potentially multi-minute delay
/// against an unresponsive adapter.
///
/// Getting `stop_comm_pending == true` and a live, not-yet-serviced
/// `SharedChannel::j1939_reclaim_pending` entry to coexist at the SAME
/// instant needs care: this crate's physical-channel poll task is a single
/// sequential loop (`poll_channel_events`) -- an ordinary (not suspended)
/// `CoptStopcomm` with empty `cop_data` dispatches and completes almost
/// instantly once its `TxItem` reaches the front of the queue (clearing
/// `stop_comm_pending` again well before the channel's next periodic tick
/// would ever reach `run_j1939_reclaim_duties`), so a bare back-to-back RPC
/// race can never land reliably. `PDU_IOCTL_SUSPEND_TX_QUEUE` (this file's
/// own established determinism trick, e.g. `coptupdateparam_execution_
/// time_check_catches_a_claim_that_lands_after_enqueue`) makes it
/// deterministic instead: it siphons the queued `TxItem::StopComm` into
/// `tx_held` UNCONDITIONALLY (`dispatch_tx_item`'s own `tx_suspended_by_
/// ioctl` check has no StopComm carve-out), so `handle_stop_comm` never
/// actually runs -- and therefore never clears `stop_comm_pending` -- until
/// `PDU_IOCTL_RESUME_TX_QUEUE` is issued. `poll_rx` (channel-wide RX
/// polling, including spontaneous-loss delivery) is entirely unaffected by
/// per-CLL TX suspension, so the loss injection below still arms `j1939_
/// reclaim_pending` normally while `stop_comm_pending` stays pinned `true`.
///
/// Setup for the loss injection itself mirrors `spontaneous_reclaim_after_
/// a_temp_bound_claim_uses_the_temp_bound_name_not_active`'s own `inject_
/// rx_with_status`/`RX_FLAG_J1939_ADDRESS_LOST` shape (round 26).
///
/// This harness has no direct call-count on `IOCTL_PROTECT_J1939_ADDR`'s
/// claim form, so "no native claim was issued" is proven the same indirect
/// way several other Decision 22/23 regression tests in this file already
/// do: a sibling CLL (CLL C) whose own single candidate is the EXACT SAME
/// address CLL A had claimed (0x80) attempts its own claim afterward -- if
/// CLL A's aborted reclaim had wrongly registered 0x80 in `SharedChannel::
/// j1939_claims` (the bug this fix closes), CLL C's own attempt would be
/// blocked by `owned_by_a_live_sibling` and exhaust
/// (`PduErrEvtInitError`); with the fix, nothing was ever registered for
/// 0x80 by CLL A's aborted reclaim, so CLL C's claim succeeds normally.
///
/// `SharedChannel::j1939_reclaim_pending`'s own entry for CLL A being gone
/// afterward is not independently exercised by a runtime assertion here --
/// verified instead by code inspection: `run_j1939_reclaim_duties`'s own
/// `drain()` call empties the WHOLE channel-wide pending map
/// unconditionally as the very first step of its loop, strictly before this
/// fix's own `stop_comm_pending` check (or any other per-entry logic) ever
/// runs, so this half of the brief's contract holds by construction
/// regardless of which outcome arm the loop takes.
///
/// **Accepted residual, the same class Decision 21's own text already
/// documents for its sibling `Cancelled` checkpoint:** this test's single-
/// candidate list means the OUTER-loop-top `stop_comm_pending` check (fires
/// with nothing yet registered/in-flight) is not independently distinguished
/// from the INNER wait loop's own per-tick check (fires after a candidate
/// is registered, but then cancels and removes it as part of the same
/// cleanup block `TimedOut`/`Cancelled` already use) -- confirmed by
/// temporarily disabling ONLY the outer check and observing this test still
/// pass, since the inner check's own cleanup independently unregisters the
/// one candidate CLL A's list has before CLL C's own probe ever runs.
/// Disabling BOTH checks together (the whole fix) does fail this test as
/// expected (CLL C's probe blocks and exhausts instead). Closing the outer
/// checkpoint's own independent coverage would need a multi-candidate list
/// with the pending StopComm timed to land strictly between one candidate's
/// resolution and the next one's native issue -- the same "no natural
/// preemption point in this harness" residual Decision 21's own text
/// already accepts for the identical reason.
#[tokio::test]
#[serial]
async fn spontaneous_reclaim_aborts_on_a_pending_stopcomm_without_issuing_a_claim() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;
    // CLL C: a sibling on the SAME physical channel whose own single
    // candidate is the exact address CLL A claims -- the "was a claim
    // issued" probe described above. A joining CLL's own Active stays
    // default until promoted (`rpc_link.rs`'s "Joining CLLs" rule, the
    // same promotion `name_collision_blocks_a_sibling_claiming_a_disjoint_
    // address` already needs), so its own candidate/NAME must be promoted
    // before its own CoptStartcomm can use them.
    let cll_c = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![9, 9, 9, 9, 9, 9, 9, 9]),
        ],
        2,
    )
    .await;
    promote_via_update_param(&mut client, cll_c).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL C should share one physical channel (same resource id/pins)"
    );

    // -- Act 1: CLL A claims 0x80 normally. --
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptStartcomm should finish and claim 0x80"
    );
    drop(events_a);

    // -- Act 2: suspend CLL A's TX queue, then issue an empty-cop_data
    // CoptStopcomm -- its own critical section sets stop_comm_pending =
    // true synchronously before this call returns, but the queued
    // TxItem::StopComm is siphoned into tx_held (never dispatched) while
    // suspended, so stop_comm_pending stays true indefinitely until
    // resumed. --
    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_a, suspend_id, None, false)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStopcomm, empty cop_data) should be accepted synchronously \
             (siphoned into tx_held while suspended, but stop_comm_pending is already set)",
        );

    // -- Act 3: synthesize a spontaneous RX_FLAG_J1939_ADDRESS_LOST for 0x80
    // -- arms SharedChannel::j1939_reclaim_pending for CLL A. poll_rx is
    // channel-wide and unaffected by CLL A's own TX suspension, so this is
    // delivered normally. --
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x80],
        j2534_0404::PROTOCOL_J1939_PS,
        0x0002_0000, // RX_FLAG_J1939_ADDRESS_LOST, events.rs::RX_J1939_ADDRESS_LOST
    );

    // No client-visible event marks a spontaneous reclaim's own abort --
    // give the channel poll task's periodic run_due_tick_duties (10ms
    // ticks) several ticks to pick up j1939_reclaim_pending and run (and
    // abort) the fresh claim loop.
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // -- Act 4: the "was a claim issued" probe -- CLL C's own single
    // candidate is 0x80. If CLL A's reclaim wrongly issued and registered a
    // native claim for 0x80 (this fix disabled/absent), CLL C's own attempt
    // is blocked and exhausts; with the fix, nothing was ever registered,
    // so CLL C succeeds normally. --
    let mut events_c = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_c)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let startcomm_cop = start_comm(&mut client, cll_c)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL C");
    let mut saw_init_error_c = false;
    assert!(
        wait_for_event(&mut events_c, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error_c = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ) && item.cop_handle.as_ref().map(|c| c.cop_handle) == Some(startcomm_cop.cop_handle)
        })
        .await,
        "CLL C's CoptStartcomm should finish"
    );
    assert!(
        !saw_init_error_c,
        "CLL C should have claimed 0x80 without contention -- CLL A's spontaneous reclaim must \
         have aborted on its pending CoptStopcomm before issuing any native claim for 0x80, \
         proving no candidate was ever attempted"
    );
    drop(events_c);
    send_data(&mut client, cll_c, vec![0xAA], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.get(3).copied(),
        Some(0x80),
        "CLL C should have claimed 0x80; got {written:#04x?}"
    );

    // -- Cleanup: resume CLL A's TX queue so its held CoptStopcomm actually
    // dispatches and completes before shutdown. --
    io_ctl_cll(&mut client, cll_a, resume_id, None, false)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    server.shutdown().await;
}

/// ADR-180 Decision 24, round-27 own-round correction (`edge-case-hunter`
/// finding on this same round's own diff, repro-confirmed; `design-advisor`
/// consult): the sibling test above
/// (`spontaneous_reclaim_aborts_on_a_pending_stopcomm_without_issuing_a_claim`)
/// proves an abort on a pending `CoptStopcomm` issues no native claim -- but
/// while `stop_comm_pending` stays pinned `true` (the held `CoptStopcomm`
/// never dispatches), that was the extent of the original round-27 fix: the
/// abort was treated as final, and CLL A's `SharedChannel::j1939_reclaim_
/// pending` entry was never re-armed. Since `stop_comm_pending` is only
/// PROVISIONAL until `handle_stop_comm` actually runs the StopComm,
/// `CancelComPrimitive` against the held StopComm reverts it to `false`
/// WITHOUT the StopComm ever running -- and without a re-arm, CLL A's lost
/// address would be abandoned forever: no retry, no client-visible error.
///
/// This test proves the fix (the re-arm added to `run_j1939_reclaim_
/// duties`'s own `StopCommPending` arm): CLL A claims 0x80, suspends its TX
/// queue, issues an empty-`cop_data` `CoptStopcomm` (held, pinning `stop_
/// comm_pending = true`), and a synthesized spontaneous loss for 0x80 arms a
/// reclaim that a due-tick aborts (`StopCommPending`) -- same setup as the
/// sibling test above. Unlike that test, this one then *cancels* the held
/// `CoptStopcomm` via `CancelComPrimitive` (reverting `stop_comm_pending` to
/// `false` without it ever executing) and resumes the TX queue, giving the
/// NEXT due-tick a chance to retry the now-unblocked reclaim. Proven the
/// same indirect sibling-probe way the sibling test's own doc comment
/// describes ("no direct claim-issue counter exists in the mock -- uses a
/// sibling-claim probe instead"), but with the INVERSE expectation: CLL C's
/// own single-candidate claim for 0x80 must now FAIL (blocked/exhausted),
/// since CLL A's retried reclaim re-claims 0x80 first if (and only if) the
/// re-arm fix is present.
///
/// Confirmed load-bearing by temporarily reverting the re-arm change (back
/// to the original round-27 "abandon, nothing to remove" shape) and
/// observing this test fail exactly as the bug predicts: CLL C's probe
/// wrongly succeeds, since CLL A's reclaim was never retried and 0x80 was
/// never re-claimed. Restored afterward.
#[tokio::test]
#[serial]
async fn spontaneous_reclaim_resumes_after_a_cancelled_stopcomm_reverts_stop_comm_pending() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;
    // CLL C: a sibling on the SAME physical channel whose own single
    // candidate is the exact address CLL A claims -- the "was a claim
    // issued/retained" probe described above. A joining CLL's own Active
    // stays default until promoted (`rpc_link.rs`'s "Joining CLLs" rule),
    // so its own candidate/NAME must be promoted before its own
    // CoptStartcomm can use them.
    let cll_c = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![9, 9, 9, 9, 9, 9, 9, 9]),
        ],
        2,
    )
    .await;
    promote_via_update_param(&mut client, cll_c).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL C should share one physical channel (same resource id/pins)"
    );

    // -- Act 1: CLL A claims 0x80 normally. --
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL A");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's CoptStartcomm should finish and claim 0x80"
    );

    // -- Act 2: suspend CLL A's TX queue, then issue an empty-cop_data
    // CoptStopcomm -- its own critical section sets stop_comm_pending = true
    // synchronously before this call returns, but the queued TxItem::
    // StopComm is siphoned into tx_held (never dispatched) while suspended,
    // so stop_comm_pending stays true until resumed or cancelled. Capture
    // its own cop_handle -- needed below to cancel it directly. --
    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_a, suspend_id, None, false)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStopcomm, empty cop_data) should be accepted synchronously \
             (siphoned into tx_held while suspended, but stop_comm_pending is already set)",
        )
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    // -- Act 3: synthesize a spontaneous RX_FLAG_J1939_ADDRESS_LOST for 0x80
    // -- arms SharedChannel::j1939_reclaim_pending for CLL A. poll_rx is
    // channel-wide and unaffected by CLL A's own TX suspension, so this is
    // delivered normally. --
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x80],
        j2534_0404::PROTOCOL_J1939_PS,
        0x0002_0000, // RX_FLAG_J1939_ADDRESS_LOST, events.rs::RX_J1939_ADDRESS_LOST
    );

    // Give the channel poll task's periodic run_due_tick_duties (10ms ticks)
    // several ticks to pick up j1939_reclaim_pending and run (and abort --
    // stop_comm_pending is still pinned true) the fresh claim loop at least
    // once. With the fix, each abort re-arms the entry, so it keeps
    // aborting/re-arming harmlessly every tick until stop_comm_pending
    // actually reverts (Act 4 below).
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // -- Act 4: cancel the held CoptStopcomm directly -- this is the exact
    // "reverts stop_comm_pending WITHOUT the StopComm ever running" path
    // this test exists to exercise (`rpc_cancel_com_primitive`'s own
    // StopComm-only eager-extraction branch, ADR-085 round 5). --
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(stop_cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "the held CoptStopcomm should report PduCopstCancelled"
    );
    drop(events_a);

    // -- Act 5: resume CLL A's TX queue (nothing left queued for the
    // cancelled StopComm; this just restores ordinary operation) and give
    // the NEXT due-tick a chance to drain the re-armed entry and retry the
    // reclaim -- now unblocked, since stop_comm_pending is false. --
    io_ctl_cll(&mut client, cll_a, resume_id, None, false)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // -- Act 6: the "was CLL A's reclaim retried and re-claim 0x80" probe --
    // CLL C's own single candidate is 0x80. If the re-arm fix is present,
    // CLL A's retried reclaim re-claimed 0x80, so CLL C's own attempt is
    // blocked and exhausts; without the fix (the bug this test catches),
    // 0x80 was never re-claimed, so CLL C succeeds. --
    let mut events_c = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_c)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    // Capture this exact CoptStartcomm's own cop_handle and filter events on
    // it (mirroring `leaked_claim_cancel_blocks_a_fresh_attempt_until_the_
    // native_cancel_finally_succeeds`'s own shape) -- unlike `start_comm`'s
    // plain helper, this CLL's own event stream can also carry backlogged
    // items from its earlier `create_and_connect_j1939_cll`/`promote_via_
    // update_param` setup (queued before this subscription existed, drained
    // live on the first item pushed after subscribing), so matching "any"
    // `PduCopstFinished` without a cop_handle filter would wrongly stop on
    // one of those leftover items instead of this COP's own.
    let start_cop_handle_c = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_c),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on CLL C")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    let mut saw_init_error_c = false;
    assert!(
        wait_for_event(&mut events_c, 2000, |item| {
            let this_cop = item.cop_handle.as_ref().map(|c| c.cop_handle)
                == Some(start_cop_handle_c.cop_handle);
            if this_cop
                && matches!(
                    &item.data,
                    Some(event_item::Data::ErrorData(error))
                        if *error == PduErrorEvent::PduErrEvtInitError as i32
                )
            {
                saw_init_error_c = true;
            }
            this_cop
                && matches!(
                    item.data,
                    Some(event_item::Data::CopStatus(status))
                        if status == PduComPrimitiveStatus::PduCopstFinished as i32
                )
        })
        .await,
        "CLL C's CoptStartcomm should finish"
    );
    assert!(
        saw_init_error_c,
        "CLL C should have been blocked claiming 0x80 -- CLL A's spontaneous reclaim must have \
         been retried (re-armed by the StopCommPending fix) and re-claimed 0x80 once stop_comm_\
         pending reverted to false, not abandoned"
    );
    drop(events_c);

    server.shutdown().await;
}

// ── SAE J2534-2 clause 7 Additional Channels (_CHx), ADR-206 ──────────────

/// A directly-named `J1939_CH1` id establishes its own physical channel
/// independently of a `J1939_PS` sibling connected on the same module --
/// mirrors `gm_uart.rs`'s own `gm_uart_ch1_establishes_independently_
/// alongside_a_gm_uart_ps_sibling` (ADR-189 Decision 2's own precedent),
/// adapted for J1939's own resource row and required explicit
/// `dlc_pin_data` (clause 16.3.2.1 -- ADR-179 Decision 2), now extended to
/// `_CHx` by ADR-206.
#[tokio::test]
#[serial]
async fn j1939_ch1_establishes_independently_alongside_a_j1939_ps_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let ps_cll = create_and_connect_j1939_cll(
        &mut client,
        J1939_ISO_OBD_RESOURCE_ID,
        CAN_PINS,
        &[(CP_J1939_TARGET_ADDRESS, 0x00)],
        &[
            (CP_J1939_PREFERRED_ADDRESS, vec![0x80]),
            (CP_J1939_NAME, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ],
        1,
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_J1939_PS
    );

    let chx_cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_J1939_CH1, &[]), // _CH1
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(chx_cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for a directly-named J1939_CH1 id");

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the _PS sibling and its own _CH1 Additional Channel should open two distinct physical \
         channels"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID + 1),
        j2534_0404::PROTOCOL_J1939_CH1,
        "the second connect should open a native J1939_CH1 channel"
    );

    let _ = ps_cll;
    server.shutdown().await;
}

/// Regression test: before the mechanical extension of ADR-211's/ADR-212's
/// established pattern (`resources.rs::chx_device_info_supported_
/// parameter` gaining an SAE J1939 guard arm), this function returned
/// `None` for every J1939 id, so `check_chx_capacity`'s Discovery-cache
/// precheck silently no-opped for this family -- the SAE J2534-2 clause 7
/// `_CHx` channel-count cap was enforced only by `j2534-0404-mock`'s own
/// independent, generic native-`PassThruConnect`-time check (shared by
/// every in-scope `_CHx` family via the same `chx_capacity` override,
/// predating this fix), not by the Discovery precheck this fix adds.
/// Confirmed genuinely discriminating by hand-reverting the new guard arm:
/// the connect is still rejected either way (this mock's native capacity
/// check runs before its `connect_count` counter increments, so
/// `connect_count` alone cannot distinguish the two paths for this family
/// -- unlike FT-CAN/SW-CAN, which have their own dedicated override
/// breaking that symmetry, per `fault_tolerant_can.rs::connect_rejects_a_
/// chx_index_within_the_generic_capacity_but_above_ft_cans_own`), but the
/// reported gRPC `Code` differs: `InvalidArgument` from the Discovery
/// precheck's synchronous rejection with the fix, vs. a generic
/// native-error-mapped code without it. Simplified relative to that
/// FT-CAN test since J1939 shares the generic `chx_capacity` override (no
/// per-family override is needed -- J1939 is independently
/// self-identifying, unlike FT-CAN/SW-CAN, which collapse onto the generic
/// CAN/ISO15765 base via `base_protocol_id`).
#[tokio::test]
#[serial]
async fn connect_rejects_a_chx_index_above_the_cached_capacity() {
    let server = start_j2534_2_server().await;
    server.backdoor.set_chx_capacity(1);
    let mut client = server.client().await;

    let cll_handle = create_j1939_cll(
        &mut client,
        j2534_0404::PROTOCOL_J1939_CH1 + 1, // _CH2
        &[],
        1,
    )
    .await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err("_CH2 exceeds the cached capacity (1) -- the connect must be rejected");
    assert_eq!(
        status.code(),
        Code::InvalidArgument,
        "must be rejected by check_chx_capacity's Discovery precheck (Code::InvalidArgument), \
         not fall through to this mock's own independent native-level _CHx capacity check (a \
         different, non-InvalidArgument code) -- if this fires, chx_device_info_supported_\
         parameter returned None for J1939 (the bug this fix corrects)"
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "connect_count stays 0 regardless of which check rejected the connect -- this mock's own \
         native _CHx capacity check (shared chx_capacity override, present independently of this \
         fix) also runs before its connect counter increments, so this assertion alone does not \
         discriminate the fix; status.code() above is the actual discriminator"
    );

    server.shutdown().await;
}

/// Connecting via the compound `_CHx`-suffixed `protocol_name` grammar
/// ("ISO_OBD_on_SAE_J1939_73_CH1", `J1939_ISO_OBD_RESOURCE_ID`'s own
/// `protocol_name`) must succeed the same way the raw `PROTOCOL_J1939_CHx`
/// route above does -- mirrors `gm_uart.rs`'s own
/// `connecting_via_compound_chx_name_succeeds` (a regression test for Codex
/// finding PR #98 round 3 on GM UART), now extended to J1939 by ADR-206.
#[tokio::test]
#[serial]
async fn connecting_via_compound_chx_name_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolName(
                        "ISO_OBD_on_SAE_J1939_73_CH1".to_string(),
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for the compound _CH1-suffixed name, not \
             be rejected as combining Pin Selection with an Additional Channel",
        );

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_J1939_CH1,
        "the compound-name route should open a native J1939_CH1 channel, same as the raw \
         protocol-id route"
    );

    server.shutdown().await;
}

/// `edge-case-hunter` finding against this ADR's own diff, verified by
/// repro, pre-existing for GM UART since ADR-189/Phase 8 (an accepted
/// residual, recorded in the Prioritized Backlog and
/// ADR-206's own Consequences -- not fixed here): a raw `PROTOCOL_J1939_CH1`
/// id combined with non-empty `dlc_pin_data` no longer reaches
/// `resolve_pin_selection`'s own (now exact-`_PS`-match) J1939 arm at all --
/// it falls through to the function's generic clause-6 fallback tail
/// instead, which rejects it, but with the GENERIC "no clause 6 Pin
/// Selection variant" message rather than `resolve_channel_selection`'s own
/// accurate "clause 6/7 mutually exclusive" one. This test pins the current,
/// still-correct-but-imprecise behavior -- replacing the pre-ADR-206
/// `create_rejects_a_deferred_additional_channel_id` test this file used to
/// carry for the same input shape, which asserted a different (now
/// superseded) message.
#[tokio::test]
#[serial]
async fn raw_chx_id_combined_with_explicit_pins_is_still_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_J1939_CH1, CAN_PINS),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "a raw PROTOCOL_J1939_CH1 id combined with explicit dlc_pin_data must still be \
             rejected -- clause 7 Additional Channels have no J1962 pin concept",
        );

    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}
