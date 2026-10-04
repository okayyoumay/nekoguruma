//! End-to-end coverage for SAE J2534-2 clause 6 Pin Selection (ADR-156
//! Decision 2, Phase 2a) through the full gRPC stack: `CreateComLogicalLink`
//! resolution (the `RscData::ProtocolId` route, per the ADR's Corrections
//! section -- a canonical ISO 22900-2 resource name keeps its pre-existing
//! all-or-nothing pin-narrowing contract and is NOT covered here, see
//! `resources.rs`'s
//! `create_com_logical_link_rejects_pin_narrowing_a_unique_name_to_zero_rows`),
//! the J2534-2 opt-in gate (clause 5), and `ConnectComLogicalLink`'s internal
//! `PassThruConnect(_PS)` -> `PassThruIoctl(SET_CONFIG, CONFIG_J1962_PINS)`
//! sequence -- which is invisible to the client on success, so these tests
//! use `server.backdoor.config_value`/`set_config_count`/`connect_count` to
//! observe the mock's actual channel state (`CONFIG_J1962_PINS` has no
//! ComParam equivalent and is never exposed over the RPC surface).
//!
//! All non-default pin combinations here use CAN's own clause 6.3.1 example
//! (pins 3/HI and 11/LOW, differing from CAN's default 6/14) or a second,
//! distinct non-default pair (1/HI and 9/LOW), matching
//! `names.rs`'s own `resolve_pin_selection_computes_pin_select_for_an_in_scope_protocol`
//! unit test so the expected `pin_select` values (`0x0000030B`/`0x00000109`)
//! are cross-checked against that unit-level coverage rather than freshly
//! derived here.

use serial_test::serial;
use vci_service_interface::{
    ComLogicalLinkHandle, ComOperationType, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DataItem, EventNotification, ExpectedResponseData,
    GetObjectIdRequest, GetResourceStatusRequest, IoCtlRequest, IoFilter, IoFilterList,
    LockResourceRequest, ModuleAndResourceId, ModuleHandle, ObjectType, PduFilter, PinData,
    ResourceData, StartComPrimitiveRequest, SubscribeEventRequest, create_com_logical_link_request,
    data_item, event_item, event_notification, io_ctl_request, module_and_resource_id,
    resource_data, subscribe_event_request,
};

use crate::harness::*;

/// Polls `server.backdoor.config_value(channel_id, param_id)` until it
/// equals `expected`, or panics after ~2s -- same poll-until-observed
/// pattern as `harness.rs`'s `wait_for_written_count`. Needed for the
/// `ParamBinding::Temp` hardware REVERT (`revert_hardware_to_live_active`),
/// which runs shortly after, but not synchronously with, the write
/// `wait_for_written_count` observes -- polling avoids assuming an exact
/// completion order between the two async steps of one poll-task item.
async fn wait_for_config_value(server: &TestServer, channel_id: u32, param_id: u32, expected: u32) {
    for _ in 0..200 {
        if server.backdoor.config_value(channel_id, param_id) == expected {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    panic!(
        "expected config value {expected} for param {param_id:#x} on channel {channel_id}, got {}",
        server.backdoor.config_value(channel_id, param_id)
    );
}

/// Builds a `RscData` resource selecting `protocol_id` via the unambiguous
/// `ProtocolId` route (ADR-156 Corrections: `ProtocolId` always layers Pin
/// Selection on top, unlike a `ProtocolName` matching a canonical resources
/// table row) with the given typed `(pin_number, pin_type_name)` pairs as
/// `dlc_pin_data`.
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

/// Issues `CreateComLogicalLink` for `module_handle` with `resource`,
/// returning the raw `Result` so callers can assert either a handle or a
/// rejection -- ADR-156 Decision 4's opt-in gate, and `compute_pin_select`'s
/// Table 3 validation, both reject synchronously here, before any
/// `ConnectComLogicalLink` is attempted.
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

/// Like `create_and_connect_cll_for_module`, but for a request carrying
/// typed `dlc_pin_data` -- the harness helper always passes an empty pin
/// list, which cannot exercise Pin Selection at all.
async fn create_and_connect_cll_with_pins(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    module_handle: u32,
    protocol_id: u32,
    baud_rate: u32,
    pins: &[(u32, &str)],
    cll_tag: u64,
) -> ComLogicalLinkHandle {
    let cll_handle = try_create_cll_with_resource(
        client,
        module_handle,
        resource_with_protocol_id_and_pins(protocol_id, pins),
        cll_tag,
    )
    .await
    .expect("create_com_logical_link should succeed for a well-formed Pin Selection request");

    set_com_param_unum32(client, cll_handle, j2534_0404::DATA_RATE, baud_rate).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

/// Default (omitted) `dlc_pin_data` on a J2534-2-opted-in module resolves to
/// the base (non-`_PS`) protocol: connects successfully, and no
/// `CONFIG_J1962_PINS` (or any) `SET_CONFIG` call happens at all -- Pin
/// Selection never activates for an unqualified connect.
#[tokio::test]
#[serial]
async fn default_dlc_pin_data_connects_without_pin_selection() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let baseline_set_config = server.backdoor.set_config_count();

    let _cll = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    // `compute_pin_select` never produces 0x00000000 (every packed pin
    // number is nonzero), so a readback of 0 unambiguously means
    // CONFIG_J1962_PINS was never written to this channel.
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0,
        "no pin-selection SET_CONFIG should have run for a default-pins connect"
    );
    assert_eq!(
        server.backdoor.set_config_count(),
        baseline_set_config,
        "no SET_CONFIG call of any kind should happen for this connect (only DATA_RATE was \
         staged, which is applied via PassThruConnect's own argument, not SET_CONFIG)"
    );

    server.shutdown().await;
}

/// Non-default `dlc_pin_data` (CAN's own clause 6.3.1 example: pins 3/HI and
/// 11/LOW, differing from CAN's default 6/HI-14/LOW) on an opted-in module
/// resolves to `CAN_PS`, connects successfully, and the mock's channel state
/// confirms the pins were actually assigned via the internal
/// `SET_CONFIG(CONFIG_J1962_PINS)` call with the expected packed value.
#[tokio::test]
#[serial]
async fn non_default_dlc_pin_data_resolves_to_ps_and_assigns_pins() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let baseline_set_config = server.backdoor.set_config_count();

    let _cll = create_and_connect_cll_with_pins(
        &mut client,
        1,
        j2534_0404::CAN,
        500_000,
        &[(3, "HI"), (11, "LOW")],
        1,
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_030B,
        "pin 3 (HI) / pin 11 (LOW) should pack into pin_select 0x0000030B (ADR-156 Decision 2)"
    );
    assert_eq!(
        server.backdoor.set_config_count(),
        baseline_set_config + 1,
        "exactly one SET_CONFIG call (CONFIG_J1962_PINS) should happen for this connect"
    );

    server.shutdown().await;
}

/// Regression test (Codex review, PR #28): CAN's own default pin *numbers*
/// (6, 14) supplied with their primary/secondary roles swapped (6 typed
/// `LOW`, 14 typed `HI` -- the opposite of CAN's actual default wiring) must
/// still resolve to `CAN_PS` and actually assign the caller's (swapped)
/// polarity via `SET_CONFIG(CONFIG_J1962_PINS)`, not be silently treated as
/// "default pins, no Pin Selection" just because the requested pin-number
/// set happens to equal the default pin-number set.
#[tokio::test]
#[serial]
async fn swapped_default_pin_roles_resolve_to_ps_not_default() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let baseline_set_config = server.backdoor.set_config_count();

    let _cll = create_and_connect_cll_with_pins(
        &mut client,
        1,
        j2534_0404::CAN,
        500_000,
        &[(6, "LOW"), (14, "HI")],
        1,
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0E06,
        "pin 14 (HI) / pin 6 (LOW) -- CAN's default numbers with swapped roles -- should pack \
         into pin_select 0x00000E06, not be silently treated as default wiring"
    );
    assert_eq!(
        server.backdoor.set_config_count(),
        baseline_set_config + 1,
        "swapped-role default pin numbers must trigger Pin Selection's SET_CONFIG call, exactly \
         like any other non-default pin request"
    );

    server.shutdown().await;
}

/// Two `ConnectComLogicalLink` calls requesting the SAME non-default pins on
/// the same protocol/baud rate share one physical channel: `ChannelKey`'s
/// `(hw_protocol_id, baud_rate, pin_select, ..)` fields (4-tuple as of
/// ADR-158; only these 3 are relevant here) treat them as identical, so the
/// joiner skips the pins `SET_CONFIG` entirely (the channel already has it
/// applied).
#[tokio::test]
#[serial]
async fn same_non_default_pins_share_one_physical_channel() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll_a = create_and_connect_cll_with_pins(
        &mut client,
        1,
        j2534_0404::CAN,
        500_000,
        &[(3, "HI"), (11, "LOW")],
        1,
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    let set_config_after_a = server.backdoor.set_config_count();

    let _cll_b = create_and_connect_cll_with_pins(
        &mut client,
        1,
        j2534_0404::CAN,
        500_000,
        &[(3, "HI"), (11, "LOW")],
        2,
    )
    .await;

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "a second CLL requesting the same protocol/baud/pin_select should join cll_a's \
         already-open physical channel, not open a second one"
    );
    assert_eq!(
        server.backdoor.set_config_count(),
        set_config_after_a,
        "the joining CLL must not re-issue CONFIG_J1962_PINS -- the channel already has it \
         applied"
    );

    server.shutdown().await;
}

/// Two `ConnectComLogicalLink` calls requesting DIFFERENT non-default pins
/// on the same protocol/baud rate get distinct physical channels -- clause
/// 6.3.2.1 explicitly allows multiple simultaneous `_PS` opens differing
/// only in pin assignment, and `ChannelKey`'s pin_select element (still present after ADR-158's later widening) keeps them
/// separated even though `(hw_protocol_id, baud_rate)` alone would collide.
#[tokio::test]
#[serial]
async fn different_non_default_pins_get_distinct_physical_channels() {
    const CHANNEL_A: u32 = 1;
    const CHANNEL_B: u32 = 2;

    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll_a = create_and_connect_cll_with_pins(
        &mut client,
        1,
        j2534_0404::CAN,
        500_000,
        &[(3, "HI"), (11, "LOW")],
        1,
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let _cll_b = create_and_connect_cll_with_pins(
        &mut client,
        1,
        j2534_0404::CAN,
        500_000,
        &[(1, "HI"), (9, "LOW")],
        2,
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "differing pin_select values must open a second, distinct physical channel even though \
         protocol and baud rate are identical"
    );

    assert_eq!(
        server
            .backdoor
            .config_value(CHANNEL_A, j2534_0404::CONFIG_J1962_PINS),
        0x0000_030B
    );
    assert_eq!(
        server
            .backdoor
            .config_value(CHANNEL_B, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0109
    );

    server.shutdown().await;
}

/// Non-default `dlc_pin_data` on a module that has NOT opted into J2534-2
/// (its `pname` lacks the `"J2534-2:"` prefix -- the default single-module
/// `TestServer::start()` config synthesizes a module with `pname = None`,
/// clause 5's "J2534-1-only" default) is rejected with `invalid_argument` at
/// `CreateComLogicalLink` time -- the client never gets far enough to
/// attempt a connect at all.
#[tokio::test]
#[serial]
async fn non_default_pins_rejected_at_create_when_module_not_opted_in() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = try_create_cll_with_resource(
        &mut client,
        MOCK_MODULE_HANDLE,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, &[(3, "HI"), (11, "LOW")]),
        1,
    )
    .await
    .expect_err(
        "a non-default dlc_pin_data on a module without the \"J2534-2:\" pname prefix must be \
         rejected at CreateComLogicalLink, before any ConnectComLogicalLink is attempted",
    );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    // No physical channel was ever opened -- the rejection happened before
    // ConnectComLogicalLink could even be attempted.
    assert_eq!(server.backdoor.connect_count(), 0);

    server.shutdown().await;
}

/// Regression test (Codex review, PR #28, the round after Codex's clean
/// approval of commit `99d32f4b`): a caller can name a `_PS` hardware
/// protocol id directly via `protocol_id` (e.g. `PROTOCOL_CAN_PS`) instead
/// of requesting non-default pins on the base `CAN` id. With no
/// `dlc_pin_data` supplied at all, this must be rejected at
/// `CreateComLogicalLink` -- before the fix, `resolve_pin_selection` never
/// normalized the raw `_PS` id, so `dlc_pin_data.is_empty()` short-circuited
/// to `Ok(None)` and a later `ConnectComLogicalLink` would have opened a
/// `_PS` channel with no `SET_CONFIG(CONFIG_J1962_PINS)` ever issued -- a
/// channel whose DLC pins could never be assigned.
#[tokio::test]
#[serial]
async fn raw_ps_protocol_id_with_no_pins_rejected_at_create() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let status = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_CAN_PS, &[]),
        1,
    )
    .await
    .expect_err(
        "a raw _PS protocol_id with no dlc_pin_data must be rejected at CreateComLogicalLink, \
         since this service can never assign its DLC pins",
    );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(server.backdoor.connect_count(), 0);

    server.shutdown().await;
}

/// Same raw `_PS` protocol id as above, but with `dlc_pin_data` supplied --
/// and, notably, supplying exactly CAN's normal default pins (6/HI, 14/LOW).
/// This must canonicalize to an ordinary base-`CAN` connect: no
/// `SET_CONFIG(CONFIG_J1962_PINS)` call, and -- the actual regression this
/// pins (Codex review / design-advisor, PR #28, a later round) -- it must
/// share ONE physical channel with a second CLL created via the plain
/// `protocol_id = CAN` route at the same baud rate, not open a second one.
/// An earlier version of this fix always computed a real `pin_select` here
/// (reasoning that a `_PS` channel starts pin-unassigned regardless of what
/// its pins equal, true per clause 6.3.3.2's sequencing but incomplete): that
/// gave this connect a different `ChannelKey`
/// (`(PROTOCOL_CAN_PS, baud, 0x060E)`) than an ordinary default-pins `CAN`
/// connect (`(CAN, baud, 0)`), so the two never shared a channel and a
/// physical ComParam/TX lock held by one never protected the other, despite
/// being the same electrical configuration.
#[tokio::test]
#[serial]
async fn raw_ps_protocol_id_with_default_pins_canonicalizes_and_shares_the_base_channel() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let baseline_set_config = server.backdoor.set_config_count();

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_CAN_PS, &[(6, "HI"), (14, "LOW")]),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve a raw _PS protocol_id with default pins");

    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for a raw _PS protocol_id");

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0,
        "a directly-named _PS protocol_id whose pins equal the base protocol's defaults must \
         canonicalize to an ordinary connect -- no CONFIG_J1962_PINS SET_CONFIG at all"
    );
    assert_eq!(
        server.backdoor.set_config_count(),
        baseline_set_config,
        "no SET_CONFIG call of any kind should happen for this canonicalized connect"
    );

    // A second CLL, created via the plain protocol_id = CAN route at the
    // same baud rate, must share the same physical channel -- connect_count
    // stays 1 -- proving the two requests resolved to the same ChannelKey.
    let _second_cll = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "a plain default-pins CAN connect must share the physical channel the directly-named \
         _PS request (with default-equal pins) already opened, not open a second one"
    );

    server.shutdown().await;
}

/// Regression test (Codex review, PR #28, a later round on the same
/// mechanism): the bare `Resource::ResourceId` variant -- no `RscData`, so no
/// `dlc_pin_data` field exists at all -- can still numerically equal a raw
/// `_PS` hardware protocol id, via the identical `ChannelProtocol::from_raw`
/// fallback the `RscData::ProtocolId` route above hits. This route has no
/// way to ever supply pins, so it must reject a raw `_PS` id outright rather
/// than silently connecting an unpinned channel.
#[tokio::test]
#[serial]
async fn raw_ps_resource_id_rejected_at_create() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
            resource: Some(create_com_logical_link_request::Resource::ResourceId(
                j2534_0404::PROTOCOL_CAN_PS,
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "a raw _PS resource_id must be rejected at CreateComLogicalLink -- this route has \
             no dlc_pin_data field to ever assign its pins with",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(server.backdoor.connect_count(), 0);

    server.shutdown().await;
}

/// A representative invalid pin combination -- pin 4, one of SAE J2534-2
/// clause 6.3.3.2 Table 3's excluded J1962 pins regardless of protocol
/// (ADR-156 Corrections) -- is rejected with `invalid_argument` at
/// `CreateComLogicalLink` time even on an opted-in module. This duplicates
/// `names.rs`'s own `compute_pin_select_rejects_each_excluded_j1962_pin`
/// unit test; kept here as one end-to-end confirmation that the rejection
/// actually surfaces through the full RPC stack, not as exhaustive coverage
/// of every invalid combination (see that unit test for the rest).
#[tokio::test]
#[serial]
async fn excluded_j1962_pin_is_rejected_at_create() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let status = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, &[(4, "HI")]),
        1,
    )
    .await
    .expect_err(
        "pin 4 is excluded from CONFIG_J1962_PINS by SAE J2534-2 clause 6.3.3.2 Table 3, \
         regardless of protocol",
    );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    assert_eq!(server.backdoor.connect_count(), 0);

    server.shutdown().await;
}

/// Regression test (Codex review, PR #28, 5th gap in this pipeline;
/// ADR-156's Corrections list the prior four): a single-wire protocol
/// (J1850VPW, whose only default pin is 2/PLUS) supplying that same pin
/// number but explicitly typed secondary (LOW) must be rejected at
/// `CreateComLogicalLink` time -- `compute_pin_select`'s single-pin branch
/// must resolve and check the pin's type, not pack it into the primary `PP`
/// byte unchecked. Before the fix this connected successfully and packed
/// `0x00000200` (a primary-line assignment) despite the caller explicitly
/// requesting the secondary line.
#[tokio::test]
#[serial]
async fn single_wire_protocol_rejects_a_secondary_typed_single_pin() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let status = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::J1850VPW, &[(2, "LOW")]),
        1,
    )
    .await
    .expect_err(
        "a single-wire protocol's only supplied pin explicitly typed secondary (LOW) has no \
         primary byte to pair with and must be rejected, not silently packed as primary",
    );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    // No physical channel was ever opened -- the rejection happened before
    // ConnectComLogicalLink could even be attempted.
    assert_eq!(server.backdoor.connect_count(), 0);

    server.shutdown().await;
}

// A canonical ISO 22900-2 resource name (e.g. "ISO_15765_2") with
// non-matching dlc_pin_data keeps the pre-existing all-or-nothing
// pin-narrowing rejection, NOT Pin Selection treatment (ADR-156
// Corrections) -- already covered by
// `resources.rs`'s `create_com_logical_link_rejects_pin_narrowing_a_unique_name_to_zero_rows`,
// which this file's full-suite run re-confirms; not duplicated here.

// ── ADR-157: hw_protocol_id _PS normalization regression tests ─────────────
//
// Phase 2a (this file's tests above) confirmed `_PS` resolution and the
// internal pin-`SET_CONFIG` sequence; these tests confirm every downstream
// Plane B consumer of `hw_protocol_id` (ComParam support, filter-family
// gates, resource-status occupancy, header format) correctly recognizes a
// `_PS` id as belonging to its base protocol family, per ADR-157's fix to a
// confirmed regression Phase 2a's implementation left behind.

/// Before ADR-157, `apply_j2534_params`'s `ComParamId::to_j2534_config_id`
/// call received the raw `PROTOCOL_CAN_PS` id, which never matches `CAN` in
/// that function's per-protocol support match (ADR-028) -- so `CP_BitSamplePoint`
/// was silently dropped instead of reaching `PassThruIoctl SET_CONFIG`.
#[tokio::test]
#[serial]
async fn can_ps_connect_applies_bit_sample_point_com_param() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, &[(3, "HI"), (11, "LOW")]),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve non-default CAN pins to CAN_PS");

    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::BIT_SAMPLE_POINT, 80).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for a CAN_PS link");

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BIT_SAMPLE_POINT),
        80,
        "CP_BitSamplePoint must reach PassThruIoctl SET_CONFIG for a CAN_PS link (ADR-157)"
    );

    server.shutdown().await;
}

/// Before ADR-157, `connect_new_physical_channel`'s pass-all-filter gate
/// (`j2534_proto_id != ISO15765`) compared the raw `_PS` id, which is never
/// equal to the literal `ISO15765` constant -- so an `ISO15765_PS` connect
/// illegally ran the pass-all-filter install path (ADR-048 violation). With
/// an empty `UniqueRespIdTable`, the fixed behavior installs no filter at
/// all (no pass-all fallback, and nothing to build point-to-point FC filters
/// from), so `filter_count` distinguishes correct (`0`) from buggy (`>= 1`,
/// from the illegal pass-all install) behavior directly.
#[tokio::test]
#[serial]
async fn iso15765_ps_connect_installs_no_pass_all_filter() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    // ISO15765's default CAN pins are 6 (HI) / 14 (LOW); 3/11 differ.
    let _cll = create_and_connect_cll_with_pins(
        &mut client,
        1,
        j2534_0404::ISO15765,
        500_000,
        &[(3, "HI"), (11, "LOW")],
        1,
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        0,
        "an ISO15765_PS connect with an empty UniqueRespIdTable must install no filters at all \
         (ADR-048/ADR-157)"
    );

    server.shutdown().await;
}

/// ADR-038's ban on `PDU_IOCTL_START_MSG_FILTER` for an ISO15765 link (its
/// filter types have no `FLOW_CONTROL_FILTER` equivalent) must still apply
/// to an `ISO15765_PS` link. Before ADR-157, `hw_protocol_id ==
/// j2534_0404::ISO15765` never matched the raw `_PS` id, silently bypassing
/// the ban. Runs pre-connect (ADR-038's check is unconditional, before any
/// connection-state branch), so no `ConnectComLogicalLink` is needed here.
#[tokio::test]
#[serial]
async fn start_msg_filter_ban_still_applies_to_iso15765_ps_link() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::ISO15765, &[(3, "HI"), (11, "LOW")]),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve non-default ISO15765 pins to ISO15765_PS");

    let start_filter_id = client
        .get_object_id(GetObjectIdRequest {
            object_type: ObjectType::ObjtIoCtrl as i32,
            shortname: "PDU_IOCTL_START_MSG_FILTER".to_string(),
        })
        .await
        .expect("get_object_id should succeed")
        .into_inner()
        .pdu_object_id;

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
        .expect_err(
            "PDU_IOCTL_START_MSG_FILTER's ADR-038 ban must still apply to an ISO15765_PS link",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "sanity check: this rejection must happen without ever connecting a physical channel"
    );

    server.shutdown().await;
}

/// A `CAN_PS` link's `protocol` field is `ChannelProtocol::CAN`, so
/// `GetResourceStatus`'s active-link match already treats it as a candidate
/// for the plain `CAN` resource (0x0201) -- but before ADR-157, the in-use
/// bit's own comparison (`link.hw_protocol_id == hw_id`) used the raw `_PS`
/// id directly against the resource table's base id, so the query reported
/// resource 0x0201 as idle while a `CAN_PS` link actually occupied it (an
/// internally inconsistent response, ADR-106).
#[tokio::test]
#[serial]
async fn get_resource_status_sees_a_can_ps_link_as_occupying_the_can_resource() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_with_pins(
        &mut client,
        1,
        j2534_0404::CAN,
        500_000,
        &[(3, "HI"), (11, "LOW")],
        1,
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(0x0201)), // ISO_11898_RAW (plain CAN)
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();

    let resource_status_data = response
        .resource_status
        .expect("response should carry a ResourceStatusItem")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status & 0x01,
        0x01,
        "the plain CAN resource (0x0201) must report bit 0 (in use) while a CAN_PS link is \
         connected (ADR-157)"
    );

    server.shutdown().await;
}

/// Regression test (design-advisor, PR #28, the query-side sibling of the
/// `parse_protocol_id_from_resource` normalization fix): querying
/// `GetResourceStatus` with `resource_id` set to the raw `_PS` numeric value
/// itself (no `resources` table row exists for one) must still see a link
/// connected with that exact `_PS` id as occupying the resource. Before this
/// fix, the query built `ChannelProtocol::from_raw(id)` un-normalized, which
/// could never match a connected link's now-always-base `protocol` field in
/// the `active_candidate` comparison, falsely reporting the resource idle.
#[tokio::test]
#[serial]
async fn get_resource_status_by_raw_ps_id_sees_the_connected_link() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_CAN_PS, &[(6, "HI"), (14, "LOW")]),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve a raw CAN_PS protocol_id with pins");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for a raw CAN_PS protocol_id");
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_CAN_PS,
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();

    let resource_status_data = response
        .resource_status
        .expect("response should carry a ResourceStatusItem")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status & 0x01,
        0x01,
        "querying by the raw _PS id directly must report bit 0 (in use) while the link \
         connected with that exact id is active"
    );

    server.shutdown().await;
}

/// Regression test (Codex review, PR #28): on a module configured for
/// `software-isotp` `can_channel_mode` (ADR-046), `GetResourceStatus`'s
/// `status_hw_id` substitutes the mode's raw-`CAN` hardware mapping for the
/// ISO15765 resource -- but Pin Selection is a per-link override of that
/// module-wide mode (ADR-157's "`_PS` links are hardware-ISO-TP only"
/// residual): a pin-selected `ISO15765_PS` link's `software_isotp` is always
/// `false`, so it stays on the natural ISO15765 hardware identity regardless
/// of the module's software-ISO-TP setting. Querying the ISO15765 resource
/// (0x0206) while such a link is connected must still report it as
/// occupying that resource.
#[tokio::test]
#[serial]
async fn get_resource_status_sees_a_pin_selected_iso15765_link_in_software_isotp_mode() {
    let server = TestServer::try_start_with_extra_config(&format!(
        "can_channel_mode = \"software-isotp\"\n{}",
        modules_toml(&[("Bench 1", "J2534-2:mock")])
    ))
    .await
    .expect("service should initialize with software-isotp mode and a J2534-2 module");
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_with_pins(
        &mut client,
        MOCK_MODULE_HANDLE,
        j2534_0404::ISO15765,
        500_000,
        &[(3, "HI"), (11, "LOW")],
        1,
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(0x0206)), // ISO15765
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();

    let resource_status_data = response
        .resource_status
        .expect("response should carry a ResourceStatusItem")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status & 0x01,
        0x01,
        "the ISO15765 resource must report bit 0 (in use) while a pin-selected ISO15765_PS \
         link -- which always uses hardware ISO-TP, never the module's software-isotp mode -- \
         is connected"
    );

    server.shutdown().await;
}

/// `rpc_link.rs`'s resource-status candidate match (Codex review, PR #28,
/// Bug A): a pin-selected `SAE_J2610_on_SAE_J2610_SCI` link's
/// `hw_protocol_id` is the consolidated `PROTOCOL_J2610_PS` id, not the
/// exact SCI variant a table row's `hw_protocol_override` names -- so the
/// candidate match must compare against `link.base_hw_protocol_id()` (which
/// correctly resolves the exact variant via `base_hw_protocol_override`,
/// ADR-157's Correction), not the raw `hw_protocol_id` directly. Before this
/// fix, no `hw_override` value ever equaled the raw `PROTOCOL_J2610_PS` id,
/// so the ambiguous `"SAE_J2610_on_SAE_J2610_SCI"` query fell back to the
/// first table row (`SCI_A_ENGINE`, 0x021E) instead of the actually-connected
/// `SCI_B_TRANS` configuration (0x0221) -- both in the echoed `resource_id`
/// and the "in use" bit (which shares this same candidate's resolved hw id
/// downstream).
///
/// Connects via resource 0x0221 itself (the row whose own
/// `hw_protocol_override` is `SCI_B_TRANS`) rather than 0x021E, specifically
/// so the expected resource id (0x0221) differs from the buggy fallback's
/// (0x021E, the first `SAE_J2610_on_SAE_J2610_SCI` row) -- `resolve_pin
/// _selection` keeps a `ProtocolId`-route connect's base hw id as the
/// resolved resource row's OWN override regardless of which pins are
/// actually supplied (only `pin_select`'s bitmask reflects the requested
/// pins), so this is the only way to land on a non-`SCI_A_ENGINE` variant
/// via this route.
#[tokio::test]
#[serial]
async fn get_resource_status_ambiguous_sci_name_finds_pin_selected_variant() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    // Resource 0x0221 (SAE_J2610_on_SAE_J2610_SCI / SCI_B_TRANS)'s own
    // default pins are (9, TX)/(15, RX); supplying different pins -- (6,
    // TX)/(7, RX) -- requests non-default pins, triggering Pin Selection and
    // resolving to `PROTOCOL_J2610_PS` with `base_hw_protocol_override
    // == SCI_B_TRANS` (0x0221's own override, preserved regardless of which
    // pins were actually requested).
    let _cll =
        create_and_connect_cll_with_pins(&mut client, 1, 0x0221, 7_812, &[(6, "TX"), (7, "RX")], 1)
            .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceName(
                    "SAE_J2610_on_SAE_J2610_SCI".to_string(),
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();

    let resource_status_data = response
        .resource_status
        .expect("response should carry a ResourceStatusItem")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_id, 0x0221,
        "the ambiguous SAE_J2610_on_SAE_J2610_SCI query must echo the actually-connected \
         pin-selected variant's own id (SCI_B_TRANS, 0x0221), not fall back to the first table \
         row (SCI_A_ENGINE, 0x021E)"
    );
    assert_eq!(
        resource_status_data[0].resource_status & 0x01,
        0x01,
        "the SCI_B_TRANS configuration (0x0221) must report bit 0 (in use) while the \
         pin-selected link actually occupies it"
    );

    server.shutdown().await;
}

/// Regression test (Codex review, PR #28, a later round): querying
/// `GetResourceStatus` with the raw, unqualified `PROTOCOL_J2610_PS` id
/// itself (not a table resource id, not the ambiguous name above) must
/// match a pin-selected link using ANY of the four native SCI variants, not
/// just `resources::base_protocol_id`'s single representative
/// (`SCI_A_ENGINE`). Connects via resource 0x0221 (`SCI_B_TRANS`) --
/// deliberately not the representative variant -- so a fix that only
/// special-cases `SCI_A_ENGINE` would still fail this test.
#[tokio::test]
#[serial]
async fn get_resource_status_by_raw_j2610_ps_id_matches_any_sci_variant() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll =
        create_and_connect_cll_with_pins(&mut client, 1, 0x0221, 7_812, &[(6, "TX"), (7, "RX")], 1)
            .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_J2610_PS,
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();

    let resource_status_data = response
        .resource_status
        .expect("response should carry a ResourceStatusItem")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status & 0x01,
        0x01,
        "querying the raw consolidated PROTOCOL_J2610_PS id must report bit 0 (in use) for a \
         link using SCI_B_TRANS, not just the SCI_A_ENGINE representative"
    );

    server.shutdown().await;
}

/// A `J1850VPW_PS` link's TX header must still use VPW's `0x68` default
/// format byte, not PWM's `0x61` -- `resolve_send_recv_tx`'s `hw_protocol`
/// (derived via `ChannelProtocol::from_raw`) drives header-format selection,
/// and before ADR-157 a raw `_PS` id passed to `from_raw` would not resolve
/// to a recognized J1850 variant at all.
#[tokio::test]
#[serial]
async fn j1850vpw_ps_link_uses_vpw_header_format_not_pwm() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    // J1850VPW's only default pin is 2 (PLUS); pin 3 (PLUS) is a non-default
    // single-pin selection that triggers Pin Selection without adding a
    // secondary pin -- J1850VPW is genuinely single-wire (ADR-201's
    // `SecondaryPinRequirement::NeverPresent`), so a two-pin selection like
    // the pre-ADR-201 version of this test used (pin 2 + pin 10) is now
    // correctly rejected as physically incomplete for this bus.
    let cll_handle = create_and_connect_cll_with_pins(
        &mut client,
        1,
        j2534_0404::J1850VPW,
        10_400,
        &[(3, "PLUS")],
        1,
    )
    .await;

    let payload = vec![0x01, 0x00];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected = vec![0x68, 0x10, 0xF1];
    expected.extend_from_slice(&payload);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected,
        "a J1850VPW_PS link's header default-format byte must be 0x68 (matching plain \
         J1850VPW), not the PWM 0x61 value (ADR-157)"
    );

    server.shutdown().await;
}

/// ADR-157 (coordinator follow-up): `handle_send_recv`'s `ParamBinding::Temp`
/// hardware apply -- reached by any `StartComPrimitive` with
/// `temp_param_update=1` -- must also normalize to the base protocol id.
/// Before this fix, the raw (possibly `_PS`) `protocol_id` was passed to
/// `apply_params_to_hardware` directly, so a K-line-specific ComParam like
/// `CP_TIdle` was silently dropped for an `ISO9141_PS` link's temp bracket,
/// reproducing the exact bug ADR-157 targets on a second code path (temp
/// apply/revert, not just connect). `CP_TIdle` is COM-class, not
/// `PDU_PC_BUSTYPE` (`comparam_support::BUSTYPE_UNUM32`), so
/// `temp_param_update`'s BUSTYPE guard does not reject staging a different
/// Working value for it.
#[tokio::test]
#[serial]
async fn iso9141_ps_temp_param_update_sendrecv_applies_tidle_com_param() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    // ISO9141's default pins are 7 (K) / 15 (L); 7/9 differ.
    let cll_handle = create_and_connect_cll_with_pins(
        &mut client,
        1,
        j2534_0404::ISO9141,
        10_400,
        &[(7, "K"), (9, "L")],
        1,
    )
    .await;

    // Staged on Working only, after connect -- must reach hardware via the
    // temp-apply bracket below, never having been pushed at connect time.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::TIDLE, 300_000).await;

    let payload = vec![0x01, 0x00];
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
        .expect("start_com_primitive(CoptSendrecv, temp_param_update=1) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Active never had a CP_TIdle key, so the later revert's SET_CONFIG
    // (built from Active) never includes TIDLE at all and cannot clobber
    // this assertion regardless of exactly when the revert runs relative to
    // this check -- see `iso9141_ps_temp_param_update_sendrecv_reverts_tidle_to_active`
    // below for the revert path itself.
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TIDLE),
        300,
        "CP_TIdle must reach PassThruIoctl SET_CONFIG during an ISO9141_PS link's \
         temp_param_update=1 apply bracket (ADR-157)"
    );

    server.shutdown().await;
}

/// Second half of the coordinator follow-up: `handle_send_recv`'s hardware
/// REVERT (`revert_hardware_to_live_active`, run after the temp apply above)
/// must also normalize to the base protocol id -- otherwise its own
/// `apply_params_to_hardware` call silently no-ops for a `_PS` link
/// (`to_j2534_config_id` never matches the raw `_PS` id), leaving hardware
/// stuck on the temp-applied value instead of reverting to Active.
#[tokio::test]
#[serial]
async fn iso9141_ps_temp_param_update_sendrecv_reverts_tidle_to_active() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::ISO9141, &[(7, "K"), (9, "L")]),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve non-default ISO9141 pins to ISO9141_PS");

    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    // Becomes Active at connect (already-fixed connect-time path) -- the
    // value the revert below must restore hardware to.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::TIDLE, 100_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for an ISO9141_PS link");

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TIDLE),
        100,
        "sanity check: CP_TIdle=100_000us (100ms) should already have been applied at connect"
    );

    // Stage a DIFFERENT Working value than Active's -- the temp bracket
    // below applies this one temporarily, then must revert hardware back to
    // Active's 100.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::TIDLE, 300_000).await;

    let payload = vec![0x01, 0x00];
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
        .expect("start_com_primitive(CoptSendrecv, temp_param_update=1) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    wait_for_config_value(&server, MOCK_CHANNEL_ID, j2534_0404::TIDLE, 100).await;

    server.shutdown().await;
}

/// Third affected code path from the coordinator follow-up: `handle_start_comm`'s
/// `ParamBinding::Temp` bracket (`CoptStartcomm`'s init transaction) must also
/// normalize to the base protocol id, mirroring
/// `startcomm_comparam.rs`'s `iso9141_temp_param_update_startcomm_borrows_working_for_init_then_reverts`
/// exactly but for an `ISO9141_PS` link. `set_config_count() == baseline + 2`
/// is the load-bearing assertion here (not just the final reverted value):
/// `apply_params_to_hardware_locked` silently skips the native `SET_CONFIG`
/// call entirely when its `to_j2534_config_id`-filtered config list is empty
/// -- exactly what an unnormalized raw `_PS` id produces for a K-line-only
/// param like `CP_P1Max` -- so a buggy build would leave `P1_MAX` untouched
/// at its already-correct (connect-time-applied) value of 20 and this test's
/// final `config_value` check alone would be a false pass; the count catches
/// both the skipped apply AND the skipped revert.
#[tokio::test]
#[serial]
async fn iso9141_ps_temp_param_update_startcomm_applies_and_reverts_p1_max() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::ISO9141, &[(7, "K"), (9, "L")]),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve non-default ISO9141 pins to ISO9141_PS");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    // ADR-072: 10_000 us converts to native P1_MAX = 20 (0.5 ms steps).
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P1_MAX, 10_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for an ISO9141_PS link");

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P1_MAX),
        20,
        "sanity check: P1_MAX should already have been applied at connect"
    );
    let baseline_set_config = server.backdoor.set_config_count();

    // Stage (Working only) a different P1_MAX -- Active stays 20 until
    // CoptUpdateparam is called explicitly. 49_500 us converts to native
    // P1_MAX = 99, distinct from 20 so the borrow/revert below is
    // observable.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P1_MAX, 49_500).await;

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // ISO9141 always uses five-baud init (deterministic in the mock,
    // IOCTL_FIVE_BAUD_INIT always succeeds); a single address byte triggers it.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x33],
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
        .expect("start_com_primitive(CoptStartcomm, temp_param_update=1) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    // Two SET_CONFIG calls bracket the init step: apply Working (P1_MAX=99),
    // then revert to Active (P1_MAX=20) -- see this test's doc comment for
    // why this count, not just the final value, is the regression signal.
    assert_eq!(
        server.backdoor.set_config_count(),
        baseline_set_config + 2,
        "ADR-157: an ISO9141_PS link's CoptStartcomm temp bracket must issue both the apply \
         and the revert SET_CONFIG call, exactly like a plain ISO9141 link does"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P1_MAX),
        20,
        "hardware must be left at Active (20), not stuck on the borrowed Working value (99)"
    );

    // Must be dropped before `server.shutdown()`, which waits for every
    // in-flight event stream (harness.rs module doc) -- an open
    // `SubscribeEvent` stream here would hang the shutdown indefinitely.
    drop(events);
    server.shutdown().await;
}

/// Regression test (Codex review, PR #28): a pin-selected `ISO9141_PS`
/// link's fast-init response must still get its KWP header/footer split off
/// (ADR-051/ADR-075), same as a plain `ISO9141` link -- mirrors
/// `startcomm_comparam.rs`'s `iso9141_explicit_fast_init_setting_succeeds`,
/// whose expected split values this test reuses verbatim (the mock's canned
/// `MOCK_FAST_INIT_RESPONSE` is the same regardless of protocol). Before
/// this fix, `header_footer_len` was called with the raw (un-normalized)
/// `_PS` protocol id, whose catch-all match arm returns `(0, 0)` -- the
/// entire response would have landed in `data_bytes` with no header split
/// at all, instead of `extra_info`/`data_bytes` as asserted below.
#[tokio::test]
#[serial]
async fn iso9141_ps_fast_init_response_splits_kwp_header() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::ISO9141, &[(7, "K"), (9, "L")]),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve non-default ISO9141 pins to ISO9141_PS");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 2).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for an ISO9141_PS link");

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
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
            cop_data: vec![0x81],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    let result = wait_for_cop_finished_and_result_data(&mut events, 2000).await;

    assert_eq!(
        server.backdoor.fast_init_count(),
        1,
        "CP_InitializationSettings=2 should force fast-init, which succeeds on ISO9141_PS"
    );

    // Same canned MOCK_FAST_INIT_RESPONSE ([0x83, 0xF1, 0x10, 0xC1, 0xE9,
    // 0x8F]) `iso9141_explicit_fast_init_setting_succeeds` asserts for the
    // base (non-`_PS`) protocol -- the mock's response is protocol-agnostic,
    // so an un-split delivery here (header/footer collapsed into payload)
    // unambiguously means the `_PS` id reached `header_footer_len`
    // un-normalized.
    assert_result_data(&result, &[0x83, 0xF1, 0x10], &[], &[0xC1, 0xE9, 0x8F]);

    drop(events);
    server.shutdown().await;
}

/// ADR-157/Bug 1 regression: `LogicalLinkState::base_hw_protocol_id()`
/// previously derived a `_PS` link's base id via `protocol.j2534_protocol_id()`,
/// which collapses every SAE J2610 SCI `_PS` variant onto the single shared
/// `SCI_MODE` value (ADR-023) instead of the exact `SCI_A_ENGINE`/`_A_TRANS`/
/// `_B_ENGINE`/`_B_TRANS` variant a non-`_PS` connect to the same resource
/// row would have used. `SCI_MODE` is not one of `to_j2534_config_id`'s four
/// recognized SCI ids, so every SCI timing ComParam (`T1_MAX`-`T5_MAX`) was
/// silently dropped for a pin-selected SCI link's `ParamBinding::Temp`
/// hardware apply (`handle_send_recv`'s `apply_params_to_hardware`, fed by
/// `TxItem::SendRecv::base_protocol_id`, itself captured from
/// `link.base_hw_protocol_id()` at `StartComPrimitive` call time).
///
/// This mirrors `iso9141_ps_temp_param_update_sendrecv_applies_tidle_com_param`
/// exactly, but for the SCI family -- NOT the plain connect-time technique
/// `can_ps_connect_applies_bit_sample_point_com_param` uses: the connect-time
/// physical-channel-creation apply (`connect_new_physical_channel`) has no
/// `LogicalLinkState` in scope and already used the free function
/// `resources::base_protocol_id`, which correctly collapses onto one of the
/// four recognized SCI ids regardless of this bug -- so only a post-connect
/// site that reads `link.base_hw_protocol_id()` actually exercises the fix.
///
/// `CoptUpdateparam` (not a `temp_param_update` bracket) is used here rather
/// than mirroring `iso9141_ps_temp_param_update_sendrecv_applies_tidle_com_param`
/// literally: a `Temp` binding's hardware apply is immediately followed, in
/// the same poll-task pass, by an unconditional revert to Active -- and
/// since the SAE_J2610_on_SAE_J2610_SCI protocol default (unlike ISO9141's
/// empty-Working legacy route) always pre-populates Active with `T1_MAX`,
/// that revert converges the hardware value back to Active's `T1_MAX`
/// regardless of whether the apply itself worked, so a post-completion
/// `config_value` check can never distinguish the bug from the fix on this
/// route. `CoptUpdateparam` promotes Working -> Active permanently, with no
/// revert, so the hardware's resulting `T1_MAX` value directly reflects
/// whether `to_j2534_config_id` recognized `base_protocol_id` as a member of
/// the SCI family.
#[tokio::test]
#[serial]
async fn sci_ps_update_param_applies_t1_max_com_param() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    // Resource 0x021E is the SAE_J2610_on_SAE_J2610_SCI / SCI_A_ENGINE
    // configuration, default pins (6, TX)/(7, RX); supplying SCI_A_TRANS's
    // pins (14, TX)/(7, RX) instead requests non-default pins and triggers
    // Pin Selection, resolving to PROTOCOL_J2610_PS.
    let cll_handle = create_and_connect_cll_with_pins(
        &mut client,
        1,
        0x021E,
        7_812,
        &[(14, "TX"), (7, "RX")],
        1,
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::T1_MAX),
        20,
        "sanity check: T1_MAX's 20_000us protocol default should already have been applied at \
         connect -- unaffected by this bug, since connect-time apply uses the always-correct \
         free function resources::base_protocol_id, not the buggy accessor"
    );

    // Staged on Working only -- must reach hardware (and Active) via
    // CoptUpdateparam below.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::T1_MAX, 90_000).await;

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptUpdateparam should finish"
    );

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::T1_MAX),
        90,
        "CP_T1Max must reach PassThruIoctl SET_CONFIG via CoptUpdateparam for a pin-selected \
         SCI link (ADR-157/Bug 1 fix): before the fix, base_hw_protocol_id() returned SCI_MODE \
         for this link, which to_j2534_config_id does not recognize as a member of the SCI \
         family, silently dropping this ComParam"
    );

    // Must be dropped before `server.shutdown()`, which waits for every
    // in-flight event stream (harness.rs module doc) -- an open
    // `SubscribeEvent` stream here would hang the shutdown indefinitely.
    drop(events);
    server.shutdown().await;
}

// ── ADR-156/Bug 2: J1850 autodetect must preserve a pin-selected link's `_PS`
// id ──────────────────────────────────────────────────────────────────────
//
// `autodetect_sae_j1850_flavor` (`rpc_link.rs`) runs at connect time for the
// two bus-agnostic J1850 protocols (resources 0x021A/0x021C/0x021D) and
// previously unconditionally overwrote `hw_protocol_id` with the resolved
// base (non-`_PS`) `J1850VPW`/`J1850PWM` id -- silently reverting a
// pin-selected link's `_PS` variant while `pin_select` stayed `Some(_)`. The
// connect path then opened a plain (non-`_PS`) channel and illegally
// attempted `PassThruIoctl(SET_CONFIG, CONFIG_J1962_PINS)` on it, which the
// mock's `pins_assigned` gating rejects with `ERR_CHANNEL_IN_USE` (a
// non-`_PS` channel is already considered to have its pins "assigned").

/// Resource 0x021C (`ISO_15031_5_on_SAE_J1850`)'s default pins are 2
/// (PLUS)/10 (MINUS); supplying only pin 2 (PLUS) is non-default and
/// triggers Pin Selection, resolving (via the VPW "initial candidate"
/// `resolve_pin_selection` resolves against, ADR-070) to
/// `PROTOCOL_J1850VPW_PS`. The bus then actually responds as VPW
/// (`set_j1850_bus_flavor`), so the autodetect probe's conclusion agrees
/// with the pre-probe resolution -- exercising the bug's VPW arm.
#[tokio::test]
#[serial]
async fn pin_selected_bus_agnostic_j1850_autodetect_preserves_vpw_ps_variant() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850VPW));

    let _cll =
        create_and_connect_cll_with_pins(&mut client, 1, 0x021C, 10_400, &[(2, "PLUS")], 1).await;

    // The real connect's channel id is the most recent successful
    // PassThruConnect (`j1850_autodetect.rs`'s own doc comment on this
    // pattern): the VPW candidate's active probe connects (and disconnects)
    // its own channel first, so the real channel is NOT `MOCK_CHANNEL_ID`.
    let channel_id = server.backdoor.connect_count() as u32;
    assert_eq!(
        server
            .backdoor
            .config_value(channel_id, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0200,
        "pin 2 (PLUS) alone should pack into pin_select 0x00000200 and must actually have been \
         applied -- proving the real channel opened as the _PS variant (ADR-156/Bug 2 fix), not \
         the plain VPW id the pre-fix autodetect overwrite would have left behind. If the \
         pre-fix bug were present, this SET_CONFIG would instead have failed \
         (ERR_CHANNEL_IN_USE) and the pins would never have been applied"
    );

    server.shutdown().await;
}

/// Same scenario, but the bus actually responds as PWM -- exercising the
/// bug's other arm (`resolved == PROTOCOL_J1850PWM_PS` after the fix, not
/// the initial `PROTOCOL_J1850VPW_PS` candidate `resolve_pin_selection`
/// resolved against before the probe ran).
#[tokio::test]
#[serial]
async fn pin_selected_bus_agnostic_j1850_autodetect_preserves_pwm_ps_variant() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850PWM));

    let _cll =
        create_and_connect_cll_with_pins(&mut client, 1, 0x021C, 41_600, &[(2, "PLUS")], 1).await;

    // The VPW candidate is probed first (inconclusive, since the bus only
    // answers PWM) and disconnected, then the PWM candidate connects
    // conclusively and is also disconnected, before the real connect opens
    // its own channel -- the most recent successful PassThruConnect.
    let channel_id = server.backdoor.connect_count() as u32;
    assert_eq!(
        server
            .backdoor
            .config_value(channel_id, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0200,
        "pin 2 (PLUS) alone should pack into pin_select 0x00000200 and must actually have been \
         applied -- proving the real channel opened as the PWM _PS variant (ADR-156/Bug 2 fix)"
    );

    server.shutdown().await;
}

// ── ADR-156/157 Bug B: SAE J1850 autodetect must probe with the caller's
// selected pins actually applied, and must never let the shared,
// module-wide `j1850_bus_flavor` cache cross a pin-selected/non-pin-selected
// boundary ───────────────────────────────────────────────────────────────
//
// `probe_sae_j1850_flavor` previously always opened its VPW/PWM candidate
// channels with the plain (non-`_PS`) protocol id and never applied
// `PassThruIoctl(SET_CONFIG, CONFIG_J1962_PINS)` to either one, so a
// pin-selected connecting link was probed on its DEFAULT wiring instead of
// the caller's actually-requested pins -- a bus reachable only via
// non-default pins was missed entirely, and the real connect that follows
// (which DOES honor pin selection) ended up open on the wrong flavor.

/// The mock is configured (via `set_j1850_bus_flavor_requiring_pins`) so the
/// bus only answers a PWM connect once the probe channel is opened as
/// `J1850PWM_PS` AND has EXACTLY the caller's selected pins
/// (`0x00000200`, pin 2/PLUS alone) applied -- unreachable on default
/// wiring. Before the fix, the probe always opened the plain (non-`_PS`)
/// `J1850PWM` candidate and never applied any pins, so it could never
/// observe this response and fell back to the VPW default; the real connect
/// then opened at VPW's 10.4k baud instead of PWM's 41.6k, even though the
/// real connect itself correctly threads pin selection through (ADR-156
/// Decision 2 / ADR-157's Bug 2 fix) -- the bug is specific to the probe.
#[tokio::test]
#[serial]
async fn pin_selected_j1850_autodetect_probes_with_selected_pins_applied() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor_requiring_pins(j2534_0404::J1850PWM, 0x0000_0200);

    // Deliberately stage VPW's baud rate (10.4k), NOT PWM's, before
    // connecting: `autodetect_sae_j1850_flavor` only force-overwrites the
    // Working `DATA_RATE` ComParam on an actual PWM win
    // (`comparam_defaults::sae_j1850_pwm_override_params`), leaving it
    // untouched on a VPW fallback. Staging PWM's own baud here would make
    // the assertion below pass vacuously regardless of what the probe
    // actually detected.
    let _cll =
        create_and_connect_cll_with_pins(&mut client, 1, 0x021C, 10_400, &[(2, "PLUS")], 1).await;

    // The most recent successful PassThruConnect is the real channel (same
    // pattern as `j1850_autodetect.rs`'s own doc comment): both probe
    // candidates connect-then-disconnect before it.
    let channel_id = server.backdoor.connect_count() as u32;
    assert_eq!(
        server.backdoor.baud_rate(channel_id),
        41_600,
        "the probe must apply the caller's selected pins to its PWM candidate channel before \
         reading, or it can never observe a response reachable only via those pins (ADR-156/157 \
         Bug B) -- a pre-fix build always falls back to VPW's 10.4k here instead, since it never \
         binds any pins on either candidate"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(channel_id, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0200,
        "the real connect's own channel must also have the selected pins applied (unaffected by \
         this bug, but re-checked here for an internally consistent connect)"
    );

    server.shutdown().await;
}

// ── ADR-157 Bug C: PWM autodetection must refresh
// `base_hw_protocol_override` alongside `hw_protocol_id` ──────────────
//
// `autodetect_sae_j1850_flavor`'s write-back correctly remapped a
// pin-selected link's `hw_protocol_id` to the detected flavor's `_PS` id
// (the ADR-157 "second Correction" bug fixed above), but left
// `base_hw_protocol_override` -- the field
// `LogicalLinkState::base_hw_protocol_id()` actually reads for every Plane B
// decision -- stuck at the initial VPW candidate set at `CreateComLogicalLink`
// time. After a PWM detection, `base_hw_protocol_id()` kept reporting
// `J1850VPW`, so `GetComParam`/`SetComParam`'s allowlist check
// (`comparam_support::check_param_allowed`, keyed on `base_hw_protocol_id()`)
// incorrectly rejected PWM-only ComParams like `CP_NetworkLine` even though
// the link had actually connected as `J1850PWM_PS`.

/// Same PWM-detection scenario as
/// `pin_selected_bus_agnostic_j1850_autodetect_preserves_pwm_ps_variant`, but
/// asserts the fix for a different field: after the PWM detection,
/// `GetComParam(CP_NetworkLine)` (PWM-only, ADR-062) must succeed -- proving
/// `base_hw_protocol_id()` reports `J1850PWM`, not the stale `J1850VPW`
/// initial candidate, post-connect.
#[tokio::test]
#[serial]
async fn pin_selected_j1850_pwm_autodetect_refreshes_base_hw_protocol_id() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850PWM));

    let cll_handle =
        create_and_connect_cll_with_pins(&mut client, 1, 0x021C, 41_600, &[(2, "PLUS")], 1).await;

    let response = client
        .get_com_param(vci_service_interface::GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(
                vci_service_interface::get_com_param_request::Param::ParamId(
                    j2534_0404::NETWORK_LINE,
                ),
            ),
        })
        .await;
    assert!(
        response.is_ok(),
        "GetComParam(CP_NetworkLine) must succeed after a pin-selected link autodetects PWM \
         (ADR-157 Bug C fix): before the fix, base_hw_protocol_id() kept reporting the stale \
         J1850VPW initial candidate, so this PWM-only param was rejected as unsupported. Got: \
         {response:?}"
    );

    server.shutdown().await;
}

/// A pin-selected probe's conclusive result must never populate the shared,
/// module-wide `j1850_bus_flavor` cache (a single `(epoch, flavor)` slot,
/// not keyed by pin selection) -- a different pin assignment can mean a
/// genuinely different physical bus segment (ADR-156 Decision 2's own
/// reasoning for widening `ChannelKey`), so a later, unrelated
/// non-pin-selected connect must never silently inherit a conclusion reached
/// over different wiring. Confirmed by observing that the second (plain,
/// non-pin-selected) connect below still runs its own full 2-candidate
/// active probe, exactly as if no cache existed -- a wrongly-cached read
/// would instead let it skip straight to a single real connect, the same
/// dedup `second_cll_on_same_module_skips_reprobe_and_shares_the_channel`
/// (`j1850_autodetect.rs`) exercises for a genuine (correct, non-pin-
/// selected) cache hit.
#[tokio::test]
#[serial]
async fn pin_selected_probe_result_is_never_cached_for_a_later_plain_connect() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;
    // The plain (non-pin-gated) flavor knob: since ADR-157's mock fix, it
    // answers a `_PS` candidate exactly like a plain one, so the first,
    // pin-selected connect below reaches a genuine Conclusive(PWM) result
    // (proving this test isn't just re-exercising the probe pin-application
    // fix above) -- the point under test is whether that conclusion leaks
    // into the shared cache.
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850PWM));

    let _cll1 =
        create_and_connect_cll_with_pins(&mut client, 1, 0x021C, 41_600, &[(2, "PLUS")], 1).await;
    let connects_after_first = server.backdoor.connect_count();

    // A different, plain (non-pin-selected) bus-agnostic resource on the
    // same module. The flavor knob is still globally active, so a live
    // probe here also lands on PWM -- but it must be a LIVE probe (2 more
    // candidate connects + 1 real connect = 3), not a cache hit (which would
    // add only the 1 real connect).
    let _cll2 = create_and_connect_cll_for_module(&mut client, 1, 0x021A, &[]).await;
    let connects_after_second = server.backdoor.connect_count();

    assert_eq!(
        connects_after_second - connects_after_first,
        3,
        "a plain connect following a pin-selected one must still run its own full active probe \
         (2 candidate connects + 1 real connect) -- a wrongly-cached read of the pin-selected \
         connect's PWM conclusion would instead skip both candidates and add only 1 (the real \
         connect)"
    );
    let channel_id = connects_after_second as u32;
    assert_eq!(
        server.backdoor.baud_rate(channel_id),
        41_600,
        "the plain connect's own live probe should reach the same PWM conclusion via the \
         still-active flavor knob -- confirming the assertion above isn't vacuously true because \
         the second connect failed outright"
    );

    server.shutdown().await;
}

// ── ADR-157 Bug D: the pre-connect physical-lock check must not ignore a
// pending pin selection ─────────────────────────────────────────────────────
//
// `service.rs`'s `same_physical_resource` falls back to comparing raw
// `hw_protocol_id` alone whenever either side of the comparison lacks a
// `channel_key` yet (a not-yet-connected CLL, or a pre-connect
// `LockResource` reservation). For two `_PS` links sharing the same
// consolidated `_PS` protocol id (e.g. both `CAN_PS`) but genuinely
// different `pin_select` values, this incorrectly treated them as the same
// physical resource -- even though `ChannelKey`'s pin_select element (ADR-156
// Decision 2) says they are distinct channels. `find_physical_lock_holder`
// now also compares `pin_select`, so an already-connected-and-locked `_PS`
// link with one pin assignment no longer falsely blocks a different CLL
// connecting with a different pin assignment on the same `_PS` protocol id.

/// `cll_a` connects as `CAN_PS` on pins 3(HI)/11(LOW) and locks
/// `LOCK_PHYSICAL_COM_PARAMS`. `cll_b` then attempts to connect as `CAN_PS`
/// on the SAME protocol/baud but DIFFERENT pins (1(HI)/9(LOW)) -- clause
/// 6.3.2.1 explicitly allows this to succeed as a distinct physical channel
/// (mirroring `different_non_default_pins_get_distinct_physical_channels`
/// above), and it must not be rejected as a lock conflict just because the
/// pre-connect fallback check only had `cll_b`'s raw `hw_protocol_id` (not
/// yet a `channel_key`) to compare against `cll_a`'s locked resource.
#[tokio::test]
#[serial]
async fn different_pin_selections_are_not_falsely_blocked_by_a_sibling_physical_lock() {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll_with_pins(
        &mut client,
        1,
        j2534_0404::CAN,
        500_000,
        &[(3, "HI"), (11, "LOW")],
        1,
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("lock_resource(LOCK_PHYSICAL_COM_PARAMS) should succeed on cll_a");

    // cll_b: same protocol/baud, DIFFERENT pins -- must connect successfully,
    // opening its own distinct physical channel, despite cll_a's lock.
    let cll_b_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, &[(1, "HI"), (9, "LOW")]),
        2,
    )
    .await
    .expect("create_com_logical_link should succeed for cll_b's differing pin selection");
    set_com_param_unum32(&mut client, cll_b_handle, j2534_0404::DATA_RATE, 500_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b_handle),
        })
        .await
        .expect(
            "connect_com_logical_link for cll_b must succeed (ADR-157 Bug D fix): a differing \
             pin_select must open a distinct physical channel from cll_a's locked one, not be \
             falsely rejected as a lock conflict on the shared CAN_PS hw_protocol_id alone",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "cll_b's differing pin selection must open a second, distinct physical channel"
    );

    server.shutdown().await;
}

// ── Codex review, PR #28, Fix 1: point-to-point FLOW_CONTROL_FILTER messages
// must carry the connecting link's actual (possibly `_PS`) protocol id ──────
//
// `can_filter_message` (`rpc_link.rs`) previously hard-coded plain
// `j2534_0404::ISO15765` as every mask/pattern/flow-control message's
// `ProtocolID`, regardless of the channel's actual connect-time
// `hw_protocol_id` -- for an `ISO15765_PS` link this mismatches the literal
// id the channel was opened with (ADR-157 Plane A), which a conforming
// adapter may reject with `ERR_MSG_PROTOCOL_ID`.

/// An `ISO15765_PS` link's installed FC filter messages must carry
/// `ProtocolID = ISO15765_PS`, not the hard-coded plain `ISO15765` constant.
#[tokio::test]
#[serial]
async fn iso15765_ps_fc_filter_carries_ps_protocol_id() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::ISO15765, &[(3, "HI"), (11, "LOW")]),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve non-default ISO15765 pins to ISO15765_PS");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    // A point-to-point USDT filter needs both CP_CanPhysReqId and
    // CP_CanRespUSDTId (ADR-039); set before connect so it takes effect
    // immediately (rpc_link.rs's own comment on this sequencing).
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
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
        .expect("connect_com_logical_link should succeed for an ISO15765_PS link");

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server
            .backdoor
            .filter_pattern_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::PROTOCOL_ISO15765_PS,
        "the FC filter's ProtocolID must be the link's actual connect-time hw_protocol_id \
         (ISO15765_PS), not the hard-coded plain ISO15765 constant (Codex review, PR #28)"
    );

    server.shutdown().await;
}

/// Regression test (Codex review, PR #28, a later round on the same
/// mechanism): when `PROTOCOL_ISO15765_PS` is named directly via
/// `protocol_id` (rather than resolved from non-default pins on the base
/// `ISO15765` id), `rpc_connect_com_logical_link`'s own `base_proto_id`
/// derivation used `protocol.j2534_protocol_id()` whenever `pin_select` was
/// set -- but `protocol` here is `ChannelProtocol::from_raw(PROTOCOL_ISO15765_PS)`
/// itself, whose `j2534_protocol_id()` passes the raw `_PS` id through
/// unchanged instead of normalizing it. The ISO15765 FC-filter-installation
/// branch (gated on `base_proto_id == ISO15765`) was therefore silently
/// skipped for such a link -- while `connect_new_physical_channel`'s own
/// (correctly normalized) pass-all-filter gate still correctly suppressed
/// the pass-all fallback -- leaving the channel with NO filters installed at
/// all despite a configured UniqueRespIdTable, silently losing every
/// response. `filter_count` distinguishes correct (`1`, the point-to-point
/// FC filter) from buggy (`0`) behavior directly.
#[tokio::test]
#[serial]
async fn raw_iso15765_ps_protocol_id_installs_fc_filter() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(
            j2534_0404::PROTOCOL_ISO15765_PS,
            &[(3, "HI"), (11, "LOW")],
        ),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve a raw ISO15765_PS protocol_id with pins");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
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
        .expect("connect_com_logical_link should succeed for a raw ISO15765_PS protocol_id");

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "a raw ISO15765_PS protocol_id with a configured UniqueRespIdTable must still install \
         the point-to-point FC filter, not silently end up with zero filters at all"
    );
    assert_eq!(
        server
            .backdoor
            .filter_pattern_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::PROTOCOL_ISO15765_PS
    );

    server.shutdown().await;
}

// ── Codex review, PR #28, Fix 2: CP_EnableConcatenation eligibility must
// normalize a pin-selected KWP/J1850 protocol id ────────────────────────────
//
// `wait_for_expected_response`'s `CP_EnableConcatenation` eligibility gate
// (ADR-148) derived `ChannelProtocol::from_raw(wait.protocol_id)` directly
// from the raw, un-normalized protocol id -- for a pin-selected KWP/J1850
// link, `wait.protocol_id` is the raw `_PS` id, which `from_raw` never
// recognizes as belonging to its base family, silently disabling
// concatenation (previously an ADR-157 accepted residual; fixed here).
// Mirrors `concat.rs`'s own
// `concat_enabled_merges_two_same_sid_segments_into_one_response` test, but
// on an `ISO14230_PS` link (non-default pins 7 (K) / 9 (L), differing from
// ISO14230's default 7 (K) / 15 (L)).

const CP_ENABLE_CONCATENATION: u32 = 0x807B;

/// A 4-byte KWP2000 header (format `0x80`, target `0x10`, source `0xF1`,
/// then the length byte) -- same shape `concat.rs`'s own `kwp_frame` uses.
fn kwp_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x80, 0x10, 0xF1, payload.len() as u8];
    frame.extend_from_slice(payload);
    frame
}

/// Collects `ResultData` items off a live `SubscribeEvent` stream until
/// `count` have arrived, or panics after `timeout_ms` -- duplicated from
/// `concat.rs`'s own helper of the same name/shape (no cross-module reuse
/// convention exists in this test suite; see `harness.rs`'s siblings).
async fn collect_result_data(
    events: &mut tonic::Streaming<EventNotification>,
    count: usize,
    timeout_ms: u64,
) -> Vec<vci_service_interface::ResultData> {
    let mut results = Vec::new();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
    while results.len() < count {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {count} ResultData item(s); got {}",
            results.len()
        );
        let Ok(Ok(Some(notification))) = tokio::time::timeout(remaining, events.message()).await
        else {
            panic!("event stream ended or errored before {count} ResultData item(s) arrived");
        };
        if let Some(event_notification::EventData::Item(item)) = notification.event_data
            && let Some(event_item::Data::ResultData(result)) = item.data
        {
            results.push(result);
        }
    }
    results
}

/// Two segments sharing the same SID must merge into ONE delivered
/// `ResultData` for a pin-selected `ISO14230_PS` link with
/// `CP_EnableConcatenation = 1` -- before the fix, the gate's unnormalized
/// `ChannelProtocol::from_raw(wait.protocol_id)` never recognized
/// `ISO14230_PS` as KWP-family, so this would fail to merge (two separate
/// `ResultData` items instead of one).
#[tokio::test]
#[serial]
async fn concat_enabled_merges_segments_for_a_pin_selected_iso14230_ps_link() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::ISO14230, &[(7, "K"), (9, "L")]),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve non-default ISO14230 pins to ISO14230_PS");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_ENABLE_CONCATENATION, 1).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for an ISO14230_PS link");

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 1,
                    mask_data: Vec::new(),
                    pattern_data: Vec::new(),
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let segment_1 = vec![0x62, 0xAA, 0xBB];
    let segment_2 = vec![0x62, 0xCC, 0xDD];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&segment_1),
        j2534_0404::ISO14230,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&segment_2),
        j2534_0404::ISO14230,
    );

    let results = collect_result_data(&mut events, 1, 2_000).await;
    assert_eq!(
        results.len(),
        1,
        "the two segments must merge into ONE delivery for a pin-selected KWP link \
         (ADR-157 Correction 4 / Codex review, PR #28)"
    );
    assert_eq!(
        results[0].data_bytes,
        vec![0x62, 0xAA, 0xBB, 0xCC, 0xDD],
        "the merged payload is segment 1 in full, then segment 2 with its own leading SID byte \
         dropped"
    );

    drop(events);
    server.shutdown().await;
}

// ── Codex review, PR #28, Fix 3: dual-channel-mode UUDT companion channel
// must be skipped, not attempted, for a pin-selected link ───────────────────
//
// `ensure_uudt_companion_channel` always opens a plain (non-`_PS`) raw-CAN
// companion channel on the link's default pins, discarding `pin_select`
// entirely -- for a pin-selected `ISO15765_PS` link this would route UUDT
// traffic to the wrong physical pins. Rather than building full pin-aware
// companion-channel support, the connect-time dual-channel companion-open
// block is simply skipped for a pin-selected link (mirroring the existing
// `probe_can_channel_mode` skip just above it, and the ADR-157
// software-ISO-TP x Pin Selection accepted residual).

/// A pin-selected `ISO15765_PS` link with dual-channel mode configured and a
/// UUDT response ID already set (before connect, so `has_uudt_ids` is true
/// at connect time) must still connect cleanly -- no companion-channel open
/// is attempted (`connect_count` stays at 1), and no error surfaces.
#[tokio::test]
#[serial]
async fn dual_channel_mode_skips_uudt_companion_for_pin_selected_link() {
    let server = TestServer::try_start_with_extra_config(&format!(
        "can_channel_mode = \"dual-channel\"\n{}",
        modules_toml(&[("Bench 1", "J2534-2:mock")])
    ))
    .await
    .expect("service should initialize with a valid modules + can_channel_mode config");
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::ISO15765, &[(3, "HI"), (11, "LOW")]),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve non-default ISO15765 pins to ISO15765_PS");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    // Set the UUDT response ID BEFORE connect, so it is already Active
    // (Working -> Active promotion happens at Connect) and `has_uudt_ids` is
    // true at connect time -- exactly the condition that reaches the
    // dual-channel companion-open block under test.
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
            "connect_com_logical_link must succeed for a pin-selected dual-channel-mode link \
             with UUDT IDs configured -- the companion-channel open must be skipped, not \
             attempted or failed (Codex review, PR #28)",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "no companion CAN channel should have been opened for a pin-selected link"
    );

    server.shutdown().await;
}

// ── edge-case-hunter, PR #28 final pre-merge pass: `install_point_to_point_
// fc_filters` must also know a companion channel never exists for a
// pin-selected link ───────────────────────────────────────────────────────
//
// The companion-open skip proven by the test above only stops the companion
// channel from being (mis-)opened. `install_point_to_point_fc_filters`
// separately decides whether to install a point-to-point `FLOW_CONTROL_
// FILTER` fallback for a UUDT response id on the main channel, and — before
// this fix — skipped that fallback whenever the module-wide dual-channel
// flag was set, without checking whether THIS link is pin-selected (and
// therefore never gets a companion either way). Net effect before the fix:
// a pin-selected link in dual-channel mode got neither a companion channel
// nor the fallback filter -- total, silent loss of UUDT response capture.

/// A pin-selected `ISO15765_PS` link with dual-channel mode configured and a
/// UUDT response ID already set (before connect) must get the point-to-point
/// `FLOW_CONTROL_FILTER` fallback installed on its main channel, since no
/// companion channel is ever opened for it (mirroring the `filter_count == 1`
/// single-channel-mode fallback behavior `auto_can_channel_mode_falls_back_
/// to_single_channel_when_incapable` establishes for "no companion exists").
#[tokio::test]
#[serial]
async fn dual_channel_mode_installs_fallback_filter_for_pin_selected_link() {
    let server = TestServer::try_start_with_extra_config(&format!(
        "can_channel_mode = \"dual-channel\"\n{}",
        modules_toml(&[("Bench 1", "J2534-2:mock")])
    ))
    .await
    .expect("service should initialize with a valid modules + can_channel_mode config");
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::ISO15765, &[(3, "HI"), (11, "LOW")]),
        1,
    )
    .await
    .expect("create_com_logical_link should resolve non-default ISO15765 pins to ISO15765_PS");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

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
            "connect_com_logical_link must succeed for a pin-selected dual-channel-mode link \
             with UUDT IDs configured",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "no companion CAN channel should have been opened for a pin-selected link"
    );
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "a pin-selected link never gets a companion channel, so it must always get the \
         point-to-point FLOW_CONTROL_FILTER fallback for its UUDT response id, regardless of \
         the module's dual-channel-mode setting"
    );

    server.shutdown().await;
}

// A second instance of the same gap exists in `promote_unique_resp_id_table`
// (reached when a UUDT id is added via `SetUniqueRespIdTable` AFTER connect,
// promoted to Active at a later `CoptUpdateparam`-style execution point --
// found while implementing the fix above, and fixed the same way, mirroring
// this same `link_pin_select.is_none()` guard). Not covered by a dedicated
// end-to-end test here: `promote_unique_resp_id_table` only runs from the
// poll task's own param-promotion cycle (`events.rs`), not synchronously
// from the `SetUniqueRespIdTable` RPC, so proving it requires driving a full
// `StartComPrimitive`/`CoptUpdateparam` sequence -- correctness here is by
// direct code inspection and by the identical pattern already proven at the
// connect-time site above, not a separate integration test.
