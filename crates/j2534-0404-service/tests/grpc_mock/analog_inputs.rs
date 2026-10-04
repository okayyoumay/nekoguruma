//! End-to-end coverage for SAE J2534-2 clause 10 Analog Inputs (ADR-177,
//! Phase 15; revised by ADR-178): `CP_AnalogSampleRate`'s requirement/
//! exclusivity, the connect-time `SET_CONFIG(CONFIG_SAMPLE_RATE)` call, the
//! mock's synthetic millivolt readings once a channel is armed, the
//! write/filter rejection on a connected Analog Input link, and the
//! join-mismatch/`CoptUpdateparam` guards that key off the recorded APPLIED
//! rate rather than any live Working ComParam state. Mirrors `j1708.rs`/
//! `honda_diagh.rs`'s structure for the connect coverage; a few small
//! per-file-local helpers are duplicated the same way those files duplicate
//! their own (this codebase's existing convention for this shape of helper,
//! not shared via `harness.rs`).
//!
//! `CP_AnalogSampleRate` (`CP_ANALOG_SAMPLE_RATE` = `0x80C4`, ADR-178) is a
//! project-minted, service-level ComParam -- staged via `SetComParam` on the
//! CLL's Working set and validated at `ConnectComLogicalLink` time, not a
//! `CreateComLogicalLinkRequest` field. It used to be exactly that field
//! (ADR-177), and before that it lived inside `ResourceData` (reachable only
//! via the `RscData` variant, which made every one of the 32 Analog Input
//! resources unreachable through their own discoverable ids -- Codex
//! review, PR #66; see ADR-177's "Codex review correction" note). Moving it
//! to a ComParam makes it apply uniformly regardless of which `resource`
//! oneof variant created the CLL, the same way every other ComParam already
//! does -- `connect_via_bare_resource_id_with_top_level_sample_rate_succeeds`/
//! `connect_via_top_level_resource_name_with_sample_rate_succeeds` below are
//! the regression tests for that original finding, now expressed against the
//! ComParam route.
//!
//! Unlike every other resource-table family, the 32 `ANALOG_IN_1`..
//! `ANALOG_IN_32` rows share ONE `ChannelProtocol::ANALOG_IN` identity
//! (`resources.rs`'s own doc comment) but each override onto its own native
//! `PROTOCOL_ANALOG_IN_x` id via `hw_protocol_override` -- the SAE J2610 SCI
//! shape, not the UART Echo Byte/Honda DIAG-H/J1708 "_PS id IS the identity"
//! shape. Clause 10 has no pin concept at all, so unlike every other J2534-2
//! phase this service supports, an Analog Input connect issues no
//! `SET_CONFIG(CONFIG_J1962_PINS)` at all -- only `CONFIG_SAMPLE_RATE`.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DataItem, ExpectedResponseData, GetObjectIdRequest, IoCtlRequest,
    IoFilter, IoFilterList, ModuleHandle, ObjectType, ParamItem, PduError, PduFilter,
    PduParamClass, ResourceData, SetComParamRequest, StartComPrimitiveRequest,
    create_com_logical_link_request, data_item, error_detail_from_status, io_ctl_request,
    param_item, resource_data,
};

use crate::harness::*;

/// Resource id of the ordinary (non-Analog) CAN row, reused by the
/// `CP_AnalogSampleRate`-exclusivity test.
const CAN_RESOURCE_ID: u32 = 0x0201;

/// Resource id of `ANALOG_IN_1`'s own resources.rs row, reused by the
/// bare-`Resource::ResourceId` regression test.
const ANALOG_IN_1_RESOURCE_ID: u32 = 0x023D;

/// A plausible nonzero clause 10.3.3.2.2 SAMPLE_RATE value for tests that
/// need one (samples per second) -- the exact value is not spec-mandated,
/// only "nonzero" (ADR-177 Decision), so this is this file's own choice.
const SAMPLE_RATE: u32 = 1_000;

/// The mock's own fixed synthetic reading (`MOCK_ANALOG_READING_MV`,
/// `j2534-0404-mock/src/lib.rs`) -- duplicated here since the mock does not
/// export it, matching this codebase's existing convention of hardcoding a
/// mock's own well-known constant in the test that depends on it.
const MOCK_ANALOG_READING_MV: i32 = 2_500;

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every Analog Input resource row requires this opt-in
/// (`names.rs`'s dedicated arm in `resolve_pin_selection`, mirroring the
/// SWCAN/FTCAN/UART Echo Byte/Honda DIAG-H/J1708 opt-in gates).
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

/// Assembles a `CreateComLogicalLinkRequest` from `resource`, filling in
/// this file's fixed `module_handle`/`cll_tag`/`cll_create_flag`
/// boilerplate -- `CP_AnalogSampleRate` is no longer part of this request
/// (ADR-178: it is staged via `SetComParam` after create, not passed here).
fn cll_request(resource: create_com_logical_link_request::Resource) -> CreateComLogicalLinkRequest {
    CreateComLogicalLinkRequest {
        module_handle: Some(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        resource: Some(resource),
        cll_create_flag: None,
    }
}

/// Builds a `CreateComLogicalLinkRequest` selecting `protocol_id` via the raw
/// hardware-protocol-id route (wrapped in `RscData`) -- same shape as
/// `j1708.rs`'s own `resource_with_protocol_id_and_pins`, minus the pins.
fn resource_with_protocol_id(protocol_id: u32) -> CreateComLogicalLinkRequest {
    cll_request(create_com_logical_link_request::Resource::RscData(
        ResourceData {
            dlc_pin_data: vec![],
            bus_type: None,
            protocol: Some(resource_data::Protocol::ProtocolId(protocol_id)),
        },
    ))
}

/// Like [`resource_with_protocol_id`], but selects the protocol by canonical
/// resource name (via the `RscData`-wrapped `ProtocolName` route) instead of
/// raw id -- used by the resource-name resolution test. NOT the same as the
/// top-level `Resource::ResourceName` variant
/// `connect_via_top_level_resource_name_with_sample_rate_succeeds` below
/// exercises.
fn resource_with_protocol_name(name: &str) -> CreateComLogicalLinkRequest {
    cll_request(create_com_logical_link_request::Resource::RscData(
        ResourceData {
            dlc_pin_data: vec![],
            bus_type: None,
            protocol: Some(resource_data::Protocol::ProtocolName(name.to_string())),
        },
    ))
}

/// Issues `CreateComLogicalLink` for `request` and returns the resulting
/// `Result` without unwrapping -- lets a test assert either success or a
/// specific rejection.
async fn try_create_cll(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    request: CreateComLogicalLinkRequest,
) -> Result<ComLogicalLinkHandle, tonic::Status> {
    client
        .create_com_logical_link(request)
        .await
        .map(|response| {
            response
                .into_inner()
                .cll_handle
                .expect("cll_handle should be present")
        })
}

/// Issues `SetComParam(CP_AnalogSampleRate, value)` on `cll_handle` and
/// returns the resulting `Result` without unwrapping -- the fallible
/// counterpart to `harness::set_com_param_unum32`, needed here because
/// `CP_AnalogSampleRate` staging is expected to fail for the exclusivity
/// tests below (non-Analog-Input resources reject it structurally via
/// `comparam_support::is_param_allowed`'s ANALOG_IN allowlist, ADR-178).
async fn try_set_analog_sample_rate(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: ComLogicalLinkHandle,
    value: u32,
) -> Result<(), tonic::Status> {
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(CP_ANALOG_SAMPLE_RATE)),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(value)),
            }),
        })
        .await
        .map(|_| ())
}

/// Creates a CLL for `protocol_id`, stages `CP_AnalogSampleRate =
/// sample_rate` via `SetComParam`, and connects it, expecting both steps to
/// succeed -- same shape as `j1708.rs`'s own
/// `create_and_connect_cll_for_protocol_id_and_pins`.
async fn create_and_connect_cll_for_protocol_id_and_sample_rate(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    protocol_id: u32,
    sample_rate: u32,
) -> ComLogicalLinkHandle {
    let cll_handle = try_create_cll(client, resource_with_protocol_id(protocol_id))
        .await
        .expect("create_com_logical_link should succeed");

    set_com_param_unum32(client, cll_handle, CP_ANALOG_SAMPLE_RATE, sample_rate).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

/// Item 1: connecting to an Analog Input resource with a valid nonzero
/// `CP_AnalogSampleRate` staged succeeds, opens the row's own native
/// `PROTOCOL_ANALOG_IN_1` channel, and SET_CONFIGs `CONFIG_SAMPLE_RATE`
/// immediately after `PassThruConnect` -- with no
/// `SET_CONFIG(CONFIG_J1962_PINS)` at all, unlike every other J2534-2
/// standalone protocol this service supports (clause 10 has no pin concept).
#[tokio::test]
#[serial]
async fn connect_with_valid_sample_rate_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        "connecting the row's own protocol id should open that exact native channel"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_SAMPLE_RATE),
        SAMPLE_RATE,
        "CONFIG_SAMPLE_RATE should have been SET_CONFIG'd to the requested rate during connect"
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_SAMPLE_RATE),
        "CONFIG_SAMPLE_RATE should have been SET_CONFIG'd during connect; log was {log:#x?}"
    );
    assert!(
        !log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "clause 10 has no pin concept -- CONFIG_J1962_PINS should never be SET_CONFIG'd for an \
         Analog Input connect; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Item 2 (ADR-178): connecting to an Analog Input resource with no
/// `CP_AnalogSampleRate` staged at all (Working default `0`, seeded by
/// `comparam_defaults::analog_in`) is rejected `invalid_argument` at
/// `ConnectComLogicalLink` time, not `CreateComLogicalLink` -- clause
/// 10.3.3.2.2's own zero default means the acquisition subsystem would be
/// disabled, so this service treats a nonzero staged rate as required for
/// this resource family before it will connect.
#[tokio::test]
#[serial]
async fn connect_with_zero_sample_rate_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed -- the rate requirement is checked at connect");

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "connecting with no CP_AnalogSampleRate staged on an Analog Input resource must be \
             rejected",
        );

    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("CP_AnalogSampleRate"),
        "the rejection should name CP_AnalogSampleRate: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "a rejected ConnectComLogicalLink must never reach PassThruConnect"
    );

    server.shutdown().await;
}

/// Item 3 (ADR-178): staging `CP_AnalogSampleRate` (nonzero) on a
/// NON-Analog-Input resource is rejected `invalid_argument` at `SetComParam`
/// itself -- enforced structurally by `comparam_support::is_param_allowed`'s
/// ANALOG_IN allowlist (which now allows exactly this one ComParam, and only
/// for an ANALOG_IN_x protocol), so the rejection moves earlier than the old
/// `CreateComLogicalLink`-time exclusivity check.
#[tokio::test]
#[serial]
async fn connect_with_sample_rate_on_non_analog_resource_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(&mut client, resource_with_protocol_id(j2534_0404::CAN))
        .await
        .expect("create_com_logical_link should succeed");

    let status = try_set_analog_sample_rate(&mut client, cll_handle, SAMPLE_RATE)
        .await
        .expect_err(
            "staging CP_AnalogSampleRate on a non-Analog-Input resource (plain CAN) must be \
             rejected",
        );

    assert_eq!(status.code(), Code::InvalidArgument);
    let detail =
        error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrComparamNotSupported as i32,
        "the rejection should report PDU_ERR_COMPARAM_NOT_SUPPORTED"
    );

    server.shutdown().await;
}

/// Item 4 (ADR-178): staging `CP_AnalogSampleRate` on resource id 0x0201
/// (plain CAN, exercised through the ordinary resource-table route rather
/// than `ProtocolId` directly) is rejected the same way -- confirms the
/// allowlist rejection applies regardless of which resource-resolution route
/// created the CLL, not just `RscData::ProtocolId`.
#[tokio::test]
#[serial]
async fn connect_with_sample_rate_on_a_table_row_by_resource_id_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(
        &mut client,
        cll_request(create_com_logical_link_request::Resource::ResourceId(
            CAN_RESOURCE_ID,
        )),
    )
    .await
    .expect("create_com_logical_link should succeed");

    let status = try_set_analog_sample_rate(&mut client, cll_handle, SAMPLE_RATE)
        .await
        .expect_err("staging CP_AnalogSampleRate on resource 0x0201 (plain CAN) must be rejected");

    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Item 5: after a successful connect + rate set, a receive-only monitor
/// (ADR-100 Decision §5 (S8)'s migration path, the same one every other
/// passive-listener test in this suite uses) retrieves the mock's synthetic
/// millivolt reading.
#[tokio::test]
#[serial]
async fn read_after_connect_retrieves_synthetic_millivolt_reading() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;
    let result = wait_for_result_data(&mut client, cll_handle).await;

    assert_eq!(
        result.data_bytes,
        MOCK_ANALOG_READING_MV.to_le_bytes(),
        "the delivered reading should be the mock's fixed 4-byte signed-LE millivolt value"
    );

    server.shutdown().await;
}

/// Item 6: a write (`CoptSendrecv` with actual TX data, i.e. a nonzero
/// `NumSendCycles`) on a connected Analog Input link is rejected with
/// `PDU_ERR_ID_NOT_SUPPORTED` -- clause 10 defines this protocol as
/// read-only.
#[tokio::test]
#[serial]
async fn write_on_connected_link_is_rejected_with_id_not_supported() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01, 0x02, 0x03],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect_err("a write (CoptSendrecv with TX data) on an Analog Input link must be rejected");

    assert_eq!(status.code(), Code::InvalidArgument);
    let detail =
        error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrIdNotSupported as i32,
        "the rejection should report PDU_ERR_ID_NOT_SUPPORTED"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the rejected write must never reach the native PassThruWriteMsgs call"
    );

    server.shutdown().await;
}

/// Item 7: `PDU_IOCTL_START_MSG_FILTER` on a connected Analog Input link is
/// rejected with `PDU_ERR_ID_NOT_SUPPORTED` -- clause 10 defines no filter
/// concept at all.
#[tokio::test]
#[serial]
async fn start_msg_filter_on_connected_link_is_rejected_with_id_not_supported() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err("PDU_IOCTL_START_MSG_FILTER on a connected Analog Input link must be rejected");

    assert_eq!(status.code(), Code::InvalidArgument);
    let detail =
        error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrIdNotSupported as i32,
        "the rejection should report PDU_ERR_ID_NOT_SUPPORTED"
    );
    assert_eq!(
        server.backdoor.start_filter_count(),
        0,
        "the rejected filter request must never reach the native PassThruStartMsgFilter call"
    );

    server.shutdown().await;
}

/// Item 8 (optional-but-nice per the phase brief): connecting via a resource
/// *name* (`"ANALOG_IN_17"`, resource 0x024D) resolves correctly, mirroring
/// existing name-resolution tests for other phases (e.g.
/// `resources.rs`'s own `create_com_logical_link_resolves_an_unambiguous_
/// resource_name`-shaped coverage). This exercises the `RscData`-wrapped
/// `ProtocolName` route -- see
/// `connect_via_top_level_resource_name_with_sample_rate_succeeds` below for
/// the top-level `Resource::ResourceName` variant instead.
#[tokio::test]
#[serial]
async fn connecting_via_resource_name_resolves() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(&mut client, resource_with_protocol_name("ANALOG_IN_17"))
        .await
        .expect("create_com_logical_link by resource name should succeed");

    set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_SAMPLE_RATE, SAMPLE_RATE).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_ANALOG_IN_17,
        "\"ANALOG_IN_17\" should resolve to resource 0x024D's own native protocol id"
    );

    server.shutdown().await;
}

/// Item 9 (`edge-case-hunter` finding, Phase 15 PR): connecting to an Analog
/// Input resource on a module that has NOT opted into SAE J2534-2 (clause 5)
/// is rejected -- `names.rs`'s `resolve_pin_selection` gates this protocol's
/// opt-in requirement the same choke point SWCAN/FT-CAN/UART Echo
/// Byte/Honda DIAG-H/J1708 use, at `CreateComLogicalLink` time, independent
/// of `CP_AnalogSampleRate` staging (which never happens here -- Create
/// itself fails first).
#[tokio::test]
#[serial]
async fn connect_rejects_module_not_opted_into_j2534_2() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect_err(
        "an Analog Input connect on a module that has not opted into SAE J2534-2 must be \
         rejected",
    );

    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "a rejected CreateComLogicalLink must never reach PassThruConnect"
    );

    server.shutdown().await;
}

/// `edge-case-hunter` finding, PR #102 round-2 close-out: the opt-in
/// rejection above only ever exercised the `ProtocolId` route, which
/// `names.rs`'s `resolve_pin_selection` already correctly gated. The
/// `RscData::ProtocolName` route resolves a table row directly
/// (`table_row_matched = true`) and, since Analog Inputs is excluded from
/// `row_needs_dynamic_pin_selection` (clause 10 has no pin concept),
/// skipped `resolve_pin_selection` entirely before the fix -- reaching
/// `resolve_channel_selection`'s `_CHx`-only-scoped opt-in check's no-op
/// early return with no rejection at all, letting a non-opted-in module
/// connect an Analog Input CLL through this route alone.
#[tokio::test]
#[serial]
async fn connect_rejects_module_not_opted_into_j2534_2_via_protocol_name() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = try_create_cll(&mut client, resource_with_protocol_name("ANALOG_IN_1"))
        .await
        .expect_err(
            "an Analog Input connect via ProtocolName on a module that has not opted into SAE \
             J2534-2 must be rejected",
        );

    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "a rejected CreateComLogicalLink must never reach PassThruConnect"
    );

    server.shutdown().await;
}

/// Item 10 (`edge-case-hunter` finding, Phase 15 PR): connecting to an
/// Analog Input resource with non-empty `dlc_pin_data` is rejected -- clause
/// 10 has no clause-6 Pin Selection vocabulary at all, so a caller-supplied
/// pin selection has nothing to apply to. Independent of
/// `CP_AnalogSampleRate` -- this is a `CreateComLogicalLink`-time rejection.
#[tokio::test]
#[serial]
async fn connect_rejects_nonempty_dlc_pin_data() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = try_create_cll(
        &mut client,
        cll_request(create_com_logical_link_request::Resource::RscData(
            ResourceData {
                dlc_pin_data: vec![vci_service_interface::PinData {
                    dlc_pin_number: 1,
                    dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeId(
                        2000,
                    )),
                }],
                bus_type: None,
                protocol: Some(resource_data::Protocol::ProtocolId(
                    j2534_0404::PROTOCOL_ANALOG_IN_1,
                )),
            },
        )),
    )
    .await
    .expect_err(
        "a non-empty dlc_pin_data on an Analog Input resource must be rejected -- clause 10 has \
         no pin concept",
    );

    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "a rejected CreateComLogicalLink must never reach PassThruConnect"
    );

    server.shutdown().await;
}

/// Item 11 (`edge-case-hunter` finding, Phase 15 PR, regression test for the
/// join-time rate-mismatch check): a second CLL connecting to the SAME
/// `ANALOG_IN_x` resource with a DIFFERENT `CP_AnalogSampleRate` than the
/// rate actually applied by the first, already-connected CLL is rejected --
/// `CP_AnalogSampleRate` is not part of `ChannelKey` (unlike
/// `fd_data_phase_rate`), so without this check the second CLL would
/// silently join the first CLL's physical channel with its own rate request
/// never applied.
#[tokio::test]
#[serial]
async fn second_cll_with_a_different_sample_rate_is_rejected_from_joining() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _first = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    let second_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed -- the conflict is detected at connect time");
    set_com_param_unum32(
        &mut client,
        second_handle,
        CP_ANALOG_SAMPLE_RATE,
        SAMPLE_RATE * 2,
    )
    .await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(second_handle),
        })
        .await
        .expect_err(
            "a second CLL requesting a different CP_AnalogSampleRate on an already-connected \
             ANALOG_IN_x channel must be rejected, not silently joined",
        );

    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "the rejected join must never issue a second native PassThruConnect"
    );

    server.shutdown().await;
}

/// Item 12 (companion to Item 11): a second CLL requesting the SAME
/// `CP_AnalogSampleRate` as the rate actually applied by the first,
/// already-connected CLL on the same `ANALOG_IN_x` resource joins normally
/// (no conflict, since both CLLs agree on the rate).
#[tokio::test]
#[serial]
async fn second_cll_with_the_same_sample_rate_joins_successfully() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _first = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;
    let _second = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "both CLLs agreeing on the same rate should share one physical channel, not open a \
         second one"
    );

    server.shutdown().await;
}

/// Item 13 (Codex review finding, PR #66): `PDU_IOCTL_START_REPEAT_MESSAGE`
/// is rejected with `PDU_ERR_ID_NOT_SUPPORTED` on a connected Analog Input
/// link -- Repeat Messaging is a device-autonomous TRANSMIT mechanism, and
/// clause 10 defines this protocol as strictly read-only. Unlike the
/// write/filter rejections (items 6/7 above), this call doesn't route
/// through `rpc_start_com_primitive`'s `transmits` gate at all -- it is a
/// separate IOCTL dispatch path (`rpc_misc.rs::ioctl_start_repeat_message`),
/// which is why the original write/filter-only rejection missed it.
#[tokio::test]
#[serial]
async fn start_repeat_message_on_connected_link_is_rejected_with_id_not_supported() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(start_id)),
            input_data: Some(DataItem {
                data: Some(data_item::Data::BytearrayData(
                    vci_service_interface::IoBytearray {
                        data: pack_repeat_message_setup(1000, 0, &[0x01, 0x02], &[], &[], &[]),
                    },
                )),
            }),
            has_output: true,
        })
        .await
        .expect_err(
            "PDU_IOCTL_START_REPEAT_MESSAGE on a connected Analog Input link must be rejected",
        );

    assert_eq!(status.code(), Code::InvalidArgument);
    let detail =
        error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrIdNotSupported as i32,
        "the rejection should report PDU_ERR_ID_NOT_SUPPORTED"
    );

    server.shutdown().await;
}

/// Item 14 (Codex review finding, PR #66): connecting via the top-level
/// `Resource::ResourceId` variant -- no `RscData` at all, exactly the shape
/// `GetResourceIds` returns and the normal discovery flow then feeds
/// straight back into `CreateComLogicalLink` -- to resource 0x023D
/// (`ANALOG_IN_1`'s own row), then staging `CP_AnalogSampleRate` via
/// `SetComParam` and connecting, succeeds, opens the row's own native
/// `PROTOCOL_ANALOG_IN_1` channel, and SET_CONFIGs `CONFIG_SAMPLE_RATE`
/// correctly. `SetComParam` operates on `cll_handle` alone, so it was never
/// actually coupled to which `resource` oneof variant created the CLL --
/// this test remains useful as basic regression coverage for the bare-id
/// discovery route now that the original `ResourceData`-scoped-field defect
/// this test was written for (ADR-177's "Codex review correction") is
/// structurally impossible under the ComParam design (ADR-178).
#[tokio::test]
#[serial]
async fn connect_via_bare_resource_id_with_top_level_sample_rate_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(
        &mut client,
        cll_request(create_com_logical_link_request::Resource::ResourceId(
            ANALOG_IN_1_RESOURCE_ID,
        )),
    )
    .await
    .expect("create_com_logical_link via a bare resource_id should succeed");

    set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_SAMPLE_RATE, SAMPLE_RATE).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        "resource 0x023D should open its own native ANALOG_IN_1 channel"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_SAMPLE_RATE),
        SAMPLE_RATE,
        "CONFIG_SAMPLE_RATE should have been SET_CONFIG'd to the requested rate during connect"
    );

    server.shutdown().await;
}

/// Item 15 (Codex review finding, PR #66, sibling to item 14): connecting via
/// the top-level `Resource::ResourceName` variant (`"ANALOG_IN_17"`, NOT the
/// `RscData`-wrapped `ProtocolName` route `connecting_via_resource_name_
/// resolves` above exercises), then staging `CP_AnalogSampleRate` and
/// connecting, also succeeds -- confirms `SetComParam`-based staging applies
/// uniformly across every `resource` oneof variant, not just `ResourceId`.
#[tokio::test]
#[serial]
async fn connect_via_top_level_resource_name_with_sample_rate_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(
        &mut client,
        cll_request(create_com_logical_link_request::Resource::ResourceName(
            "ANALOG_IN_17".to_string(),
        )),
    )
    .await
    .expect("create_com_logical_link via a top-level resource_name should succeed");

    set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_SAMPLE_RATE, SAMPLE_RATE).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_ANALOG_IN_17,
        "\"ANALOG_IN_17\" should resolve to resource 0x024D's own native protocol id"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_SAMPLE_RATE),
        SAMPLE_RATE,
        "CONFIG_SAMPLE_RATE should have been SET_CONFIG'd to the requested rate during connect"
    );

    server.shutdown().await;
}

/// Item 16 (ADR-178 regression test: the whole reason for recording the
/// APPLIED rate on `SharedChannel` instead of comparing live per-CLL/Working
/// state): CLL A connects at `SAMPLE_RATE`, then re-stages its OWN Working
/// `CP_AnalogSampleRate` to a different value WITHOUT reconnecting -- a
/// staged ComParam is re-stageable post-connect, so this must not affect
/// what hardware is actually running. CLL B, staged at the ORIGINAL applied
/// rate (matching what is actually running), must still be allowed to join
/// -- a join check comparing against CLL A's live Working state instead of
/// the recorded applied value would incorrectly reject this.
#[tokio::test]
#[serial]
async fn join_after_owner_restages_a_different_rate_without_reconnecting_still_uses_the_applied_rate()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    // CLL A (the owner) re-stages Working to a DIFFERENT, not-yet-applied
    // rate, without reconnecting.
    let restaged_rate = SAMPLE_RATE * 3;
    set_com_param_unum32(&mut client, cll_a, CP_ANALOG_SAMPLE_RATE, restaged_rate).await;

    // CLL B, staged at the ORIGINALLY applied rate (SAMPLE_RATE, matching
    // what is actually running on hardware), must still succeed joining.
    let cll_b = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");
    set_com_param_unum32(&mut client, cll_b, CP_ANALOG_SAMPLE_RATE, SAMPLE_RATE).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect(
            "CLL B staged at the originally-applied rate should join successfully, even though \
             CLL A has since re-staged a different, not-yet-applied rate on its own Working set",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL B should join the existing channel, not open a second native PassThruConnect"
    );

    server.shutdown().await;
}

/// Item 17 (companion to Item 16, proving the join check is specifically
/// keyed to the recorded applied value, not merely permissive): a CLL
/// staged at CLL A's newly-staged-but-NOT-yet-applied rate (rather than the
/// originally applied one) is still rejected from joining.
#[tokio::test]
#[serial]
async fn join_with_owners_newly_staged_but_unapplied_rate_is_still_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    let restaged_rate = SAMPLE_RATE * 3;
    set_com_param_unum32(&mut client, cll_a, CP_ANALOG_SAMPLE_RATE, restaged_rate).await;

    let cll_b = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");
    set_com_param_unum32(&mut client, cll_b, CP_ANALOG_SAMPLE_RATE, restaged_rate).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect_err(
            "a CLL staged at CLL A's newly-staged-but-not-yet-applied rate must still be \
             rejected from joining -- only the recorded APPLIED rate is a valid match",
        );

    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "the rejected join must never issue a second native PassThruConnect"
    );

    server.shutdown().await;
}

/// Item 18 (ADR-178, `CoptUpdateparam` guard): re-staging
/// `CP_AnalogSampleRate` away from the rate actually applied to an
/// already-connected analog channel, then issuing `CoptUpdateparam`, is
/// rejected outright -- mirrors the CAN FD `CoptUpdateparam` guard's own
/// "reject rather than silently ignore" precedent (`fd_can.rs`'s
/// `coptupdateparam_rejects_promoting_fd_comparams_on_a_classic_connected_link`).
#[tokio::test]
#[serial]
async fn coptupdateparam_rejects_restaging_analog_sample_rate_on_a_connected_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    set_com_param_unum32(
        &mut client,
        cll_handle,
        CP_ANALOG_SAMPLE_RATE,
        SAMPLE_RATE * 3,
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
            "CoptUpdateparam promoting a re-staged CP_AnalogSampleRate onto an already-connected \
             Analog Input link must be rejected, not silently promoted",
        );

    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Item 19 (Codex review, PR #70 round 2): `CP_AnalogSampleRate` must be
/// `PDU_PC_BUSTYPE`-classified (`comparam_support::BUSTYPE_UNUM32`) so
/// `bustype_params_differ` -- the guard `temp_param_update` rejection relies
/// on -- can actually see a staged rate differing from the connect-latched
/// Active value. Without that classification, a `temp_param_update=1` COP
/// would silently stage a different sample rate than the one actually
/// applied to hardware via `SET_CONFIG(CONFIG_SAMPLE_RATE)` at connect time
/// -- exactly the kind of Active/hardware desync the `CoptUpdateparam` guard
/// above (`coptupdateparam_rejects_restaging_analog_sample_rate_on_a_connected_link`)
/// exists to prevent for the promotion path, but `temp_param_update` is a
/// different code path (`bustype_params_differ`, not the `CoptUpdateparam`
/// arm's own guard) that needed its own fix.
#[tokio::test]
#[serial]
async fn temp_param_update_rejects_a_mismatched_analog_sample_rate_on_a_connected_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    // Working now differs from Active's connect-latched CP_AnalogSampleRate.
    set_com_param_unum32(
        &mut client,
        cll_handle,
        CP_ANALOG_SAMPLE_RATE,
        SAMPLE_RATE * 3,
    )
    .await;

    // Same receive-only-monitor shape as `harness::arm_receive_only_monitor`,
    // but with `temp_param_update: 1` -- mirroring
    // `locks_and_param_classes.rs`'s
    // `tester_present_guard_rejects_temp_param_update_for_sendrecv_and_startcomm`.
    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -1,
                temp_param_update: 1,
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
        .expect_err(
            "CoptSendrecv temp_param_update=1 must be rejected when Working's \
             CP_AnalogSampleRate differs from the connect-latched Active value -- silently \
             accepting it would desync Active from what SET_CONFIG(CONFIG_SAMPLE_RATE) actually \
             applied to hardware",
        );

    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("PDU_ERR_TEMPPARAM_NOT_ALLOWED"),
        "{}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 20 (Codex review, PR #70): `CP_AnalogSampleRate` must be resolvable
/// by NAME, not just by its numeric `ParamId` (`0x80C4`) -- every test above
/// stages it via `harness::set_com_param_unum32`/`try_set_analog_sample_rate`,
/// both of which use `param_item::Id::ParamId` exclusively, so a gap in
/// `names.rs::map_comparam_name` (this ComParam minted without a
/// corresponding shortname entry) went unexercised by every other test in
/// this file. `SetComParam`/`GetComParam` both document named-ComParam
/// support, so a client using `ParamName("CP_AnalogSampleRate")` -- the
/// documented alternative to the numeric id -- must not be rejected as an
/// unknown name.
#[tokio::test]
#[serial]
async fn set_com_param_by_name_resolves_cp_analog_sample_rate() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");

    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamName("CP_AnalogSampleRate".to_string())),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(SAMPLE_RATE)),
            }),
        })
        .await
        .expect(
            "SetComParam(ParamName(\"CP_AnalogSampleRate\")) must resolve to the same ComParam \
             as the numeric ParamId(0x80C4) route, not be rejected as an unknown name",
        );

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "the rate staged via the named SetComParam route must actually take effect -- \
             connecting should succeed exactly as it would via the numeric ParamId route",
        );

    server.shutdown().await;
}

// ── ADR-216: Analog Inputs remaining parameters ─────────────────────────────
//
// SAE J2534-2 clause 10 Analog Inputs (ADR-216): the seven remaining
// project-minted ComParams (four writable, three read-only) -- allowlist,
// range validation, the connect-time GET_CONFIG readback (both a fresh
// connect and a join), the connect-ordering fix that lets the two
// rate-gated writable params ride the generic connect-time batch, the
// `CoptUpdateparam` re-stage guard, and BUSTYPE classification.

/// Item 21: `SetComParam`/`GetComParam` round-trips a writable Analog Inputs
/// ComParam (`CP_AnalogAveragingMethod`, which carries no service-side range
/// check).
#[tokio::test]
#[serial]
async fn analog_averaging_method_round_trips() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");

    set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_AVERAGING_METHOD, 3).await;
    let readback = get_com_param_unum32(&mut client, cll_handle, CP_ANALOG_AVERAGING_METHOD).await;
    assert_eq!(readback, 3);

    server.shutdown().await;
}

/// Item 22 (ADR-216 Decision item 5): `CP_AnalogReadingsPerMsg` outside
/// `1..=0x408` is rejected at `SetComParam`, and `CP_AnalogSamplesPerReading
/// == 0` is rejected too -- the zero-value disarm special case is
/// intentionally not offered through either of these two ComParams
/// (`CP_AnalogSampleRate` remains the sole arming/disarming knob).
#[tokio::test]
#[serial]
async fn analog_readings_and_samples_range_rejections() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");

    let status =
        try_set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_READINGS_PER_MSG, 0x409)
            .await
            .expect_err("CP_AnalogReadingsPerMsg above 0x408 must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    let status = try_set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_READINGS_PER_MSG, 0)
        .await
        .expect_err("CP_AnalogReadingsPerMsg == 0 must be rejected (must be >= 1)");
    assert_eq!(status.code(), Code::InvalidArgument);

    let status =
        try_set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_SAMPLES_PER_READING, 0)
            .await
            .expect_err("CP_AnalogSamplesPerReading == 0 must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    // A valid value for each is accepted.
    set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_READINGS_PER_MSG, 0x408).await;
    set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_SAMPLES_PER_READING, 1).await;

    server.shutdown().await;
}

/// Item 23 (ADR-216 Decision item 5): each of the three genuinely read-only
/// Analog Inputs ComParams rejects any `SetComParam` attempt outright with
/// `PDU_ERR_COMPARAM_NOT_SUPPORTED`.
#[tokio::test]
#[serial]
async fn read_only_analog_comparams_reject_set_com_param() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");

    for &param_id in &[
        CP_ANALOG_SAMPLE_RESOLUTION,
        CP_ANALOG_INPUT_RANGE_LOW,
        CP_ANALOG_INPUT_RANGE_HIGH,
    ] {
        let status = try_set_com_param_unum32(&mut client, cll_handle, param_id, 1)
            .await
            .expect_err(&format!(
                "SetComParam({param_id:#x}) on a read-only Analog Inputs ComParam must be rejected"
            ));
        assert_eq!(status.code(), Code::InvalidArgument);
        let detail =
            error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
        assert_eq!(
            detail.pdu_error,
            PduError::PduErrComparamNotSupported as i32,
            "the rejection should report PDU_ERR_COMPARAM_NOT_SUPPORTED for {param_id:#x}"
        );
    }

    server.shutdown().await;
}

/// Item 23b (ADR-216 Decision item 8, edge-case-hunter finding, round 2
/// correction): `CP_AnalogInputRangeLow`/`CP_AnalogInputRangeHigh` are
/// reported via the `Snum32` oneof arm at `GetComParam` time -- a client
/// naturally round-tripping that response straight back through
/// `SetComParam` sends the `Snum32` arm, not `Unum32`, so the read-only
/// rejection above must also be reachable via that arm, not just `Unum32`.
#[tokio::test]
#[serial]
async fn read_only_analog_range_comparams_reject_snum32_set_com_param() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");

    for &param_id in &[CP_ANALOG_INPUT_RANGE_LOW, CP_ANALOG_INPUT_RANGE_HIGH] {
        let status = try_set_com_param_snum32(&mut client, cll_handle, param_id, -1)
            .await
            .expect_err(&format!(
                "SetComParam({param_id:#x}, Snum32) on a read-only Analog Inputs ComParam must \
                 be rejected"
            ));
        assert_eq!(status.code(), Code::InvalidArgument);
        let detail =
            error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
        assert_eq!(
            detail.pdu_error,
            PduError::PduErrComparamNotSupported as i32,
            "the rejection should report PDU_ERR_COMPARAM_NOT_SUPPORTED for {param_id:#x} via \
             the Snum32 arm too"
        );
    }

    server.shutdown().await;
}

/// Item 24 (ADR-216 Decision item 6): connecting an Analog Inputs CLL
/// performs a one-time connect-time `GET_CONFIG` readback of
/// `CP_AnalogActiveChannels`/`CP_AnalogSampleResolution`/
/// `CP_AnalogInputRangeLow`/`CP_AnalogInputRangeHigh`. Before connect, the
/// three read-only ones read `0` (not yet known, per the ADR's own
/// Consequences section); after connect, they report the mock's seeded
/// values (`ChannelState::new`'s Table-16-example seed:
/// `SAMPLE_RESOLUTION=12`, `INPUT_RANGE_LOW=-20000`,
/// `INPUT_RANGE_HIGH=20000`). `CP_AnalogInputRangeLow` reports through the
/// `Snum32` oneof variant, not `Unum32` (ADR-216 Decision item 8), and its
/// negative seed value round-trips correctly through the two's-complement
/// encoding.
#[tokio::test]
#[serial]
async fn connect_time_readback_populates_the_capability_comparams() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");

    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_ANALOG_SAMPLE_RESOLUTION).await,
        0,
        "before connect, CP_AnalogSampleResolution should read 0 (not yet known)"
    );

    set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_SAMPLE_RATE, SAMPLE_RATE).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_ANALOG_SAMPLE_RESOLUTION).await,
        12,
        "after connect, CP_AnalogSampleResolution should report the mock's seeded value"
    );
    assert_eq!(
        get_com_param_snum32(&mut client, cll_handle, CP_ANALOG_INPUT_RANGE_LOW).await,
        -20_000,
        "CP_AnalogInputRangeLow should report the mock's seeded negative value via the Snum32 \
         oneof variant"
    );
    assert_eq!(
        get_com_param_snum32(&mut client, cll_handle, CP_ANALOG_INPUT_RANGE_HIGH).await,
        20_000,
        "CP_AnalogInputRangeHigh should report the mock's seeded value via the Snum32 oneof \
         variant"
    );
    // CP_AnalogActiveChannels is also read back, even though this mock does
    // not seed a nonzero default for it -- confirming the readback ran (not
    // merely a static default) is covered by the join test below, which
    // checks a second CLL also receives it.
    let _ = get_com_param_unum32(&mut client, cll_handle, CP_ANALOG_ACTIVE_CHANNELS).await;

    server.shutdown().await;
}

/// Item 25 (ADR-216 Decision item 6): a second CLL joining an already-open
/// `ANALOG_IN_x` channel also receives the connect-time readback, not just a
/// fresh connect -- a joiner never pushes its own Working->Active values
/// today for anything else, but these four are read directly from hardware.
#[tokio::test]
#[serial]
async fn join_onto_an_already_open_channel_also_receives_the_readback() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _first = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    let second_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");
    set_com_param_unum32(
        &mut client,
        second_handle,
        CP_ANALOG_SAMPLE_RATE,
        SAMPLE_RATE,
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(second_handle),
        })
        .await
        .expect("the second CLL should join the existing channel successfully");

    assert_eq!(
        get_com_param_unum32(&mut client, second_handle, CP_ANALOG_SAMPLE_RESOLUTION).await,
        12,
        "a joining CLL should also receive the connect-time readback, not just the CLL that \
         opened the physical channel"
    );
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "the second CLL should join, not open a second physical channel"
    );

    server.shutdown().await;
}

/// Item 26 (ADR-216 Decision item 9, the connect-ordering fix): staging
/// `CP_AnalogReadingsPerMsg` to a non-default value alongside the required
/// nonzero `CP_AnalogSampleRate` on the SAME connect must succeed -- without
/// the fix (the generic ComParam batch applying BEFORE
/// `SET_CONFIG(CONFIG_SAMPLE_RATE)` arms the rate), this mock's own
/// `IOCTL_SET_CONFIG` handler (mirroring SAE J2534-2 clause 10.3.3.2.4's
/// rate-must-be-zero requirement) would reject the connect's generic batch
/// with `ERR_NOT_SUPPORTED` the moment `CONFIG_READINGS_PER_MSG` rides along
/// while `CONFIG_SAMPLE_RATE` is already nonzero.
#[tokio::test]
#[serial]
async fn staging_readings_per_msg_alongside_a_nonzero_sample_rate_connects_successfully() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");

    set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_SAMPLE_RATE, SAMPLE_RATE).await;
    let non_default_readings_per_msg = 5;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        CP_ANALOG_READINGS_PER_MSG,
        non_default_readings_per_msg,
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connecting with a non-default CP_AnalogReadingsPerMsg staged alongside a nonzero \
             CP_AnalogSampleRate must succeed -- proves the generic batch applies before the \
             rate is armed (ADR-216 Decision item 9)",
        );

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_READINGS_PER_MSG),
        non_default_readings_per_msg,
        "CONFIG_READINGS_PER_MSG should have been SET_CONFIG'd to the staged value"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_SAMPLE_RATE),
        SAMPLE_RATE
    );

    server.shutdown().await;
}

/// Item 27 (ADR-216 Decision item 10, corrected shape): `CoptUpdateparam`
/// re-staging `CP_AnalogSamplesPerReading` away from the value actually
/// applied to an already-connected Analog Input link is rejected -- mirrors
/// `coptupdateparam_rejects_restaging_analog_sample_rate_on_a_connected_link`
/// above exactly, just targeting the sibling ComParam:
/// `apply_params_to_hardware_locked` (events.rs) never re-forwards
/// `CONFIG_SAMPLES_PER_READING` to hardware post-connect at all (it is
/// connect-time-latched, like `CONFIG_SAMPLE_RATE` itself), so this guard is
/// what actually prevents `GetComParam` from reporting a Working value
/// hardware was never reconfigured to match.
#[tokio::test]
#[serial]
async fn coptupdateparam_rejects_restaging_analog_samples_per_reading_on_a_connected_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    // Connect applied the default CP_AnalogSamplesPerReading = 1
    // (comparam_defaults.rs's own analog_in() seed); re-stage it to a
    // genuinely different value.
    set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_SAMPLES_PER_READING, 4).await;

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
            "CoptUpdateparam promoting a re-staged CP_AnalogSamplesPerReading onto an \
             already-connected Analog Input link must be rejected, not silently promoted",
        );

    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Item 27b (ADR-216 Decision item 10, corrected shape -- the fix's direct
/// proof): a `CoptUpdateparam` that does NOT change
/// `CP_AnalogSamplesPerReading`/`CP_AnalogReadingsPerMsg` away from their
/// applied values -- here, one that only re-stages `CP_AnalogAveragingMethod`
/// -- succeeds on an already-connected Analog Input link, per the ADR's own
/// last sentence ("`CP_AnalogActiveChannels`/`CP_AnalogAveragingMethod`
/// carry no such restriction"). Before the fix, the guard rejected every
/// `CoptUpdateparam` on any already-connected analog link unconditionally,
/// which this test would have caught.
#[tokio::test]
#[serial]
async fn coptupdateparam_succeeds_for_an_unrelated_update_on_a_connected_analog_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    // Re-stage only CP_AnalogAveragingMethod -- CP_AnalogSamplesPerReading/
    // CP_AnalogReadingsPerMsg stay at their connect-applied defaults (1, 1).
    set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_AVERAGING_METHOD, 3).await;

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
            "CoptUpdateparam that only touches CP_AnalogAveragingMethod must succeed on an \
             already-connected Analog Input link -- CP_AnalogSamplesPerReading/ \
             CP_AnalogReadingsPerMsg are unchanged from their applied values",
        );

    server.shutdown().await;
}

/// Item 28 (ADR-216 Decision item 7): `CP_AnalogAveragingMethod` (one of the
/// four newly-BUSTYPE-classed writable Analog Inputs ComParams) draws the
/// existing `PDU_ERR_TEMPPARAM_NOT_ALLOWED` rejection when a `temp_param_
/// update=1` COP's Working value differs from Active on an already-connected
/// link -- mirrors `temp_param_update_rejects_a_mismatched_analog_sample_
/// rate_on_a_connected_link` above, for a different BUSTYPE-classed member.
#[tokio::test]
#[serial]
async fn temp_param_update_rejects_a_mismatched_averaging_method_on_a_connected_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_protocol_id_and_sample_rate(
        &mut client,
        j2534_0404::PROTOCOL_ANALOG_IN_1,
        SAMPLE_RATE,
    )
    .await;

    // Working now differs from Active's connect-time default (0) for
    // CP_AnalogAveragingMethod.
    set_com_param_unum32(&mut client, cll_handle, CP_ANALOG_AVERAGING_METHOD, 7).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -1,
                temp_param_update: 1,
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
        .expect_err(
            "CoptSendrecv temp_param_update=1 must be rejected when Working's \
             CP_AnalogAveragingMethod differs from Active on a connected Analog Input link",
        );

    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("PDU_ERR_TEMPPARAM_NOT_ALLOWED"),
        "{}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 29 (ADR-216, join-time-sync fix -- the exact edge-case-hunter
/// repro): a second CLL joining an already-open `ANALOG_IN_x` channel now
/// also adopts `CP_AnalogSamplesPerReading`/`CP_AnalogReadingsPerMsg` from
/// the channel's own applied acquisition configuration, not its own seeded
/// Working default -- closing a gap the connect-time capability readback
/// alone never covered (the batching-values half of `finalize_connected_
/// link`'s own `analog_connect_sync` write, `rpc_link.rs`, since the
/// Codex-review Finding 1 amendment folded this sync into that function's
/// critical section).
/// Before the fix: CLL A opens staging `CP_AnalogSamplesPerReading = 4`;
/// CLL B joins with the seeded default `1` still in its own Working set;
/// B's `CoptUpdateparam` touching only `CP_AnalogAveragingMethod` was
/// wrongly rejected by the `rpc_primitive.rs` guard, which compares B's OWN
/// staged `SamplesPerReading` against the channel's recorded applied value
/// -- contradicting ADR-216 Decision item 10's own guarantee that an
/// unrelated update succeeds. After the fix, B's Working is synced to the
/// channel's applied value at join time, so the guard's value-comparison
/// passes for B just as it already did for A.
#[tokio::test]
#[serial]
async fn join_onto_an_already_open_channel_syncs_samples_per_reading_and_unrelated_update_succeeds()
{
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let non_default_samples_per_reading = 4;
    let first_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");
    set_com_param_unum32(
        &mut client,
        first_handle,
        CP_ANALOG_SAMPLE_RATE,
        SAMPLE_RATE,
    )
    .await;
    set_com_param_unum32(
        &mut client,
        first_handle,
        CP_ANALOG_SAMPLES_PER_READING,
        non_default_samples_per_reading,
    )
    .await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(first_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // Joins with CP_AnalogSamplesPerReading left at its seeded default (1),
    // NOT staged to match the first CLL's applied value of 4.
    let second_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");
    set_com_param_unum32(
        &mut client,
        second_handle,
        CP_ANALOG_SAMPLE_RATE,
        SAMPLE_RATE,
    )
    .await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(second_handle),
        })
        .await
        .expect("the second CLL should join the existing channel successfully");

    assert_eq!(
        get_com_param_unum32(&mut client, second_handle, CP_ANALOG_SAMPLES_PER_READING).await,
        non_default_samples_per_reading,
        "the joining CLL's own GetComParam(CP_AnalogSamplesPerReading) should report the \
         channel's applied value, not its own seeded default -- proving the join-time sync \
         actually ran"
    );

    // The exact repro: an update that touches only CP_AnalogAveragingMethod
    // must succeed on the joiner now that its own Working state matches the
    // channel's applied SamplesPerReading/ReadingsPerMsg.
    set_com_param_unum32(&mut client, second_handle, CP_ANALOG_AVERAGING_METHOD, 3).await;
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(second_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "a joining CLL's CoptUpdateparam touching only CP_AnalogAveragingMethod must \
             succeed -- it never touches CP_AnalogSamplesPerReading/CP_AnalogReadingsPerMsg, \
             and the join-time sync means its own Working values for those two now match the \
             channel's applied ones",
        );

    assert_eq!(
        get_com_param_unum32(&mut client, second_handle, CP_ANALOG_SAMPLES_PER_READING).await,
        non_default_samples_per_reading,
        "CP_AnalogSamplesPerReading should still report the channel's applied value after the \
         unrelated update"
    );

    server.shutdown().await;
}

/// Item 30 (ADR-216, sibling gap the same fix closes): a second CLL joining
/// an already-open `ANALOG_IN_x` channel adopts `CP_AnalogAveragingMethod`
/// from the channel's own live value (via `read_analog_capability_params`'s
/// `CONFIG_AVERAGING_METHOD` read, written by `finalize_connected_link`),
/// not its own seeded Working default of `0` -- and a later unrelated
/// `CoptUpdateparam` on the joiner does not clobber the channel's hardware
/// value back to that stale default. Before the fix:
/// `apply_params_to_hardware_locked` (`events.rs`) blanket-forwards every
/// translated `unum32` Working param on ANY `CoptUpdateparam` (clause
/// 10.3.3.2.5 has no rate-gate on `CP_AnalogAveragingMethod`, unlike
/// SamplesPerReading/ReadingsPerMsg), so the joiner's stale Working `0`
/// would silently overwrite the channel's actual applied `2`.
#[tokio::test]
#[serial]
async fn join_onto_an_already_open_channel_syncs_averaging_method_and_unrelated_update_does_not_clobber_it()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let non_default_averaging_method = 2;
    let first_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");
    set_com_param_unum32(
        &mut client,
        first_handle,
        CP_ANALOG_SAMPLE_RATE,
        SAMPLE_RATE,
    )
    .await;
    set_com_param_unum32(
        &mut client,
        first_handle,
        CP_ANALOG_AVERAGING_METHOD,
        non_default_averaging_method,
    )
    .await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(first_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_AVERAGING_METHOD),
        non_default_averaging_method,
        "CONFIG_AVERAGING_METHOD should have been SET_CONFIG'd to the staged value during connect"
    );

    // Joins with CP_AnalogAveragingMethod left at its seeded default (0),
    // NOT staged to match the first CLL's applied value of 2.
    let second_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");
    set_com_param_unum32(
        &mut client,
        second_handle,
        CP_ANALOG_SAMPLE_RATE,
        SAMPLE_RATE,
    )
    .await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(second_handle),
        })
        .await
        .expect("the second CLL should join the existing channel successfully");

    assert_eq!(
        get_com_param_unum32(&mut client, second_handle, CP_ANALOG_AVERAGING_METHOD).await,
        non_default_averaging_method,
        "the joining CLL's own GetComParam(CP_AnalogAveragingMethod) should report the \
         channel's live value, not the seeded default of 0"
    );

    // An unrelated CoptUpdateparam on the joiner (it never explicitly
    // re-stages CP_AnalogAveragingMethod itself) must not clobber the
    // channel's hardware value back to the joiner's stale default.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(second_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("the joiner's CoptUpdateparam should succeed");

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_AVERAGING_METHOD),
        non_default_averaging_method,
        "the joiner's blanket CoptUpdateparam forward must not clobber CONFIG_AVERAGING_METHOD \
         back to the joiner's own stale seeded default -- the join-time readback sync means its \
         Working value already matches the channel's live one"
    );

    server.shutdown().await;
}

/// Item 30b (ADR-216 Decision item 10 amendment; Codex review, PR #130,
/// Finding 2): the cross-sibling stale-value clobber the change-only-forward
/// fix closes -- distinct from the test just above, which only proves a
/// joiner's OWN unrelated update does not revert the value it inherited at
/// join time. The actual Codex repro needs a LIVE post-connect change made
/// by ONE CLL, made AFTER a sibling already joined and synced its own
/// now-stale copy of the pre-change value:
///
/// 1. CLL A opens the channel; CLL B joins (both now have
///    `CP_AnalogAveragingMethod = 0`, the default, via the join-time sync
///    the test above pins).
/// 2. A THEN changes `CP_AnalogAveragingMethod` to a new value via
///    `SetComParam` + `CoptUpdateparam` -- a genuine live post-connect
///    change (ADR-216 Decision item 10's own "stays live-changeable"
///    framing), happening AFTER B already joined and synced its own
///    now-stale `0`.
/// 3. B THEN issues an UNRELATED `CoptUpdateparam` (touching no analog
///    ComParam at all). Before this fix, `apply_params_to_hardware_locked`'s
///    blanket-forward would re-push B's own stale `CP_AnalogAveragingMethod
///    = 0`, silently reverting A's live change on hardware even though A's
///    own `GetComParam` still (wrongly) reports the newer value.
///
/// Hand-revert-verified (temporarily commented out the
/// `strip_unchanged_analog_channel_wide_keys` call in `events.rs`'s
/// `handle_update_param`): this test fails without the fix -- the mock's
/// hardware value reverts to `0` after B's unrelated update -- and passes
/// with it restored.
#[tokio::test]
#[serial]
async fn live_averaging_method_change_survives_an_unrelated_update_from_a_sibling_that_already_joined()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let live_averaging_method = 2;

    // 1. CLL A opens the channel; CLL B joins. Both inherit the seeded
    // default (0) for CP_AnalogAveragingMethod -- A because it never staged
    // anything else before connecting, B via the join-time sync.
    let handle_a = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");
    set_com_param_unum32(&mut client, handle_a, CP_ANALOG_SAMPLE_RATE, SAMPLE_RATE).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(handle_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for CLL A");

    let handle_b = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");
    set_com_param_unum32(&mut client, handle_b, CP_ANALOG_SAMPLE_RATE, SAMPLE_RATE).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(handle_b),
        })
        .await
        .expect("CLL B should join the existing channel successfully");

    assert_eq!(
        get_com_param_unum32(&mut client, handle_b, CP_ANALOG_AVERAGING_METHOD).await,
        0,
        "CLL B's own Working/Active should have synced the channel's default 0 at join time"
    );

    // 2. A THEN changes CP_AnalogAveragingMethod live, AFTER B already
    // joined and synced its own now-stale copy of 0.
    set_com_param_unum32(
        &mut client,
        handle_a,
        CP_ANALOG_AVERAGING_METHOD,
        live_averaging_method,
    )
    .await;
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(handle_a),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("CLL A's live CP_AnalogAveragingMethod update should succeed");

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_AVERAGING_METHOD),
        live_averaging_method,
        "A's live post-connect change should have reached hardware"
    );

    // 3. B THEN issues an UNRELATED CoptUpdateparam (touching no analog
    // ComParam at all). Before the change-only-forward fix, B's own stale
    // Working copy of CP_AnalogAveragingMethod (0) would be blanket-forwarded
    // here, reverting A's just-applied live change.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(handle_b),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("B's unrelated CoptUpdateparam should succeed");

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_AVERAGING_METHOD),
        live_averaging_method,
        "B's unrelated CoptUpdateparam must not revert A's live CP_AnalogAveragingMethod change \
         back to B's own stale copy -- the change-only-forward fix (ADR-216 Decision item 10 \
         amendment) strips a value B itself never touched from B's own hardware push"
    );

    server.shutdown().await;
}

/// Item 31 (ADR-216, accepted-overwrite behavior extended to the batching
/// ComParams): a second CLL joining an already-open `ANALOG_IN_x` channel
/// WITHOUT explicitly staging `CP_AnalogReadingsPerMsg` reads back the
/// CHANNEL's applied value via `GetComParam`, not its own seeded default --
/// documenting the same "live value wins" accepted-overwrite precedent
/// ADR-216's Consequences section already establishes for
/// `CP_AnalogActiveChannels`/`CP_AnalogSamplesPerReading`, now shown for the
/// sibling batching ComParam.
#[tokio::test]
#[serial]
async fn join_onto_an_already_open_channel_without_staging_readings_per_msg_adopts_the_channels_value()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let non_default_readings_per_msg = 6;
    let first_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");
    set_com_param_unum32(
        &mut client,
        first_handle,
        CP_ANALOG_SAMPLE_RATE,
        SAMPLE_RATE,
    )
    .await;
    set_com_param_unum32(
        &mut client,
        first_handle,
        CP_ANALOG_READINGS_PER_MSG,
        non_default_readings_per_msg,
    )
    .await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(first_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // Joins without ever staging CP_AnalogReadingsPerMsg -- stays at its
    // seeded default (1) in the request, but the join-time sync should
    // overwrite it with the channel's own applied value (6).
    let second_handle = try_create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
    )
    .await
    .expect("create_com_logical_link should succeed");
    set_com_param_unum32(
        &mut client,
        second_handle,
        CP_ANALOG_SAMPLE_RATE,
        SAMPLE_RATE,
    )
    .await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(second_handle),
        })
        .await
        .expect("the second CLL should join the existing channel successfully");

    assert_eq!(
        get_com_param_unum32(&mut client, second_handle, CP_ANALOG_READINGS_PER_MSG).await,
        non_default_readings_per_msg,
        "a joining CLL that never staged CP_AnalogReadingsPerMsg should read back the channel's \
         own applied value, not its own seeded default of 1"
    );

    server.shutdown().await;
}

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- same per-file local helper shape as
/// `j1708.rs`'s own (not shared via `harness.rs`, matching this codebase's
/// existing convention).
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
