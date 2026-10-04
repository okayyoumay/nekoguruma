//! End-to-end coverage for SAE J2534-2 clause 7 Additional Channels (ADR-156
//! Decision 3, Phase 2b) through the full gRPC stack: `CreateComLogicalLink`
//! resolution of a directly-named `_CHx` hardware protocol id, the J2534-2
//! opt-in gate (clause 5), and the qualifier-gate widening `rpc_link.rs`
//! needed once a link can be qualified by a `_CHx` id as well as
//! `pin_select` (ADR-156 Decision 3 addendum). Mirrors `pin_selection.rs`'s
//! structure/helpers for the equivalent `_PS` coverage.
//!
//! ADR-178 removed `ResourceData.channel_index`, the field route that used
//! to be one of two ways to select an Additional Channel -- every test here
//! now selects a target `_CHx` channel the remaining way, by directly naming
//! its raw native hardware protocol id (e.g. `PROTOCOL_CAN_CH1 + (index -
//! 1)`) via `ResourceData.protocol_id`, which `names.rs`'s
//! `resolve_channel_selection` decomposes to the identical
//! `(base_hw_protocol_id, chx_id, index)` tuple the removed field route
//! produced.

use serial_test::serial;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest,
    GetResourceStatusRequest, ModuleAndResourceId, ModuleHandle, ResourceData,
    create_com_logical_link_request, module_and_resource_id, resource_data,
};

use crate::harness::*;

/// Builds a `RscData` resource directly naming `raw_protocol_id` via the
/// unambiguous `ProtocolId` route (ADR-178: the sole remaining Additional
/// Channels selection mechanism -- name the raw `_CHx` id directly, rather
/// than the removed `channel_index` field).
fn resource_with_protocol_id(raw_protocol_id: u32) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: None,
        protocol: Some(resource_data::Protocol::ProtocolId(raw_protocol_id)),
    }
}

/// Same helper shape as `pin_selection.rs`'s `try_create_cll_with_resource`.
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

/// A directly-named `_CHx` hardware protocol id resolves cleanly for an
/// in-scope protocol, on a module opted into J2534-2.
#[tokio::test]
#[serial]
async fn create_com_logical_link_resolves_a_directly_named_chx_id() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_CAN_CH1 + 2), // _CH3
        1,
    )
    .await
    .expect("a directly-named _CH3 id should resolve to a _CHx hardware variant for CAN");

    server.shutdown().await;
}

/// A module NOT opted into J2534-2 rejects a directly-named `_CHx` id
/// (clause 5), mirroring the equivalent Pin Selection opt-in gate.
#[tokio::test]
#[serial]
async fn create_com_logical_link_rejects_a_directly_named_chx_id_when_not_opted_in() {
    let server = TestServer::start_with_modules(&[("Bench 1", "mock")]).await;
    let mut client = server.client().await;

    let status = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_CAN_CH1 + 2), // _CH3
        1,
    )
    .await
    .expect_err("a non-opted-in module must reject a directly-named _CHx id");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// Two `_CHx` links at different indices get distinct physical channels
/// (the hardware id itself already differs per index -- ADR-156 Decision 3
/// needs no further `ChannelKey` widening).
#[tokio::test]
#[serial]
async fn two_chx_links_at_different_indices_get_distinct_channels() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_a = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_CAN_CH1), // _CH1
        1,
    )
    .await
    .unwrap();
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect should succeed for _CH1");

    let cll_b = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_CAN_CH1 + 1), // _CH2
        2,
    )
    .await
    .unwrap();
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect should succeed for _CH2");

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "two different _CHx indices must open two distinct physical channels"
    );

    server.shutdown().await;
}

// ── ADR-156 Decision 3 addendum: the software-ISO-TP/dual-channel-mode
// qualifier skips (and their paired point-to-point-filter condition) widen
// from `pin_select.is_some()` to "any qualifier present" -- mirrors
// `pin_selection.rs`'s `dual_channel_mode_skips_uudt_companion_for_pin_
// selected_link`/`dual_channel_mode_installs_fallback_filter_for_pin_
// selected_link` pair for the directly-named `_CHx` qualifier. edge-case-hunter
// (Phase 2a) flagged these two conditions drifting apart as a known hazard
// -- exercised here for `_CHx` too.

/// A directly-named `ISO15765_CHx`-qualified link with dual-channel mode
/// configured and a UUDT response ID already set (before connect, so
/// `has_uudt_ids` is true at connect time) must still connect cleanly -- no
/// companion-channel open is attempted (`connect_count` stays at 1).
#[tokio::test]
#[serial]
async fn dual_channel_mode_skips_uudt_companion_for_a_chx_qualified_link() {
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
        resource_with_protocol_id(j2534_0404::PROTOCOL_ISO15765_CH1), // _CH1
        1,
    )
    .await
    .expect("a directly-named _CH1 id should resolve to ISO15765_CH1");
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
            "connect_com_logical_link must succeed for a _CHx-qualified dual-channel-mode link \
             with UUDT IDs configured -- the companion-channel open must be skipped, not \
             attempted or failed (ADR-156 Decision 3 addendum)",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "no companion CAN channel should have been opened for a _CHx-qualified link"
    );

    server.shutdown().await;
}

/// Same setup as above: since no companion channel is ever opened for a
/// `_CHx`-qualified link, `install_point_to_point_fc_filters` must still
/// install the point-to-point `FLOW_CONTROL_FILTER` fallback on the main
/// channel -- the two conditions (companion-open skip, fallback-filter
/// install) must stay in sync, or this link silently loses UUDT response
/// capture entirely (the exact bug edge-case-hunter found for `_PS` in
/// Phase 2a).
#[tokio::test]
#[serial]
async fn dual_channel_mode_installs_fallback_filter_for_a_chx_qualified_link() {
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
        resource_with_protocol_id(j2534_0404::PROTOCOL_ISO15765_CH1), // _CH1
        1,
    )
    .await
    .unwrap();
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

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
        .expect("connect should succeed");

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "the point-to-point FLOW_CONTROL_FILTER fallback must be installed on the main channel \
         for a _CHx-qualified link, since it never gets a companion channel either way"
    );

    server.shutdown().await;
}

/// Issues `GetResourceStatus` for a raw resource id and returns bit 0
/// ("in use") of the single resulting entry.
async fn resource_in_use(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    raw_resource_id: u32,
) -> bool {
    let status = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle { module_handle: 1 }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    raw_resource_id,
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();
    let data = status
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(data.len(), 1);
    data[0].resource_status & 0x01 != 0
}

/// edge-case-hunter finding 1: a literal `_CHx` query must be scoped to its
/// exact index outside the J2610/SCI family too -- not just via the
/// `PROTOCOL_J2610_PS`/`_CHx` SCI-variant broadening, which used to be the
/// only place index-scoping was actually applied. Before the fix, a raw
/// `_CHx` query id in any of the other six in-scope families fell through
/// `find_by_resource_id` to `resources::base_protocol_id`'s Phase 2b `_CHx`
/// funnel, which normalizes every index of a family onto the same base id --
/// so `CAN_CH99` falsely matched a connected `CAN_CH1` link on base-id alone,
/// with no index check ever consulted.
#[tokio::test]
#[serial]
async fn get_resource_status_scopes_a_literal_chx_query_to_its_exact_index_outside_sci() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_CAN_CH1), // _CH1
        1,
    )
    .await
    .expect("a directly-named _CH1 id should resolve to CAN's own _CHx block at index 1");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect should succeed for CAN_CH1");

    // A different index of the same family (CAN_CH99) must NOT report
    // in-use, even though `base_protocol_id`'s Phase 2b `_CHx` funnel
    // normalizes both to the same base CAN id.
    let different_index_raw = j2534_0404::PROTOCOL_CAN_CH1 + 98; // _CH99
    assert!(
        !resource_in_use(&mut client, different_index_raw).await,
        "a literal CAN_CH99 query must not match a connected CAN_CH1 link -- index scoping \
         must apply outside the J2610/SCI family too"
    );

    // The actually-connected index (CAN_CH1) IS in-use -- proves this test
    // can't pass by accident (e.g. a bug that always returns false).
    let same_index_raw = j2534_0404::PROTOCOL_CAN_CH1; // _CH1
    assert!(
        resource_in_use(&mut client, same_index_raw).await,
        "a literal CAN_CH1 query must match the connected CAN_CH1 link"
    );

    server.shutdown().await;
}

/// edge-case-hunter finding 3: `rpc_link.rs`'s own `mod tests` never calls
/// `rpc_get_resource_status` at all, and `tests/grpc_mock/resources.rs` has
/// no `_CHx`/SCI case -- so the J2610_CHx-block SCI-variant matching
/// (`_CHx` analog of the `PROTOCOL_J2610_PS` broadening, ADR-156 Decision 3
/// addendum) had zero coverage. A literal `_CHx` query id inside the
/// `PROTOCOL_J2610_CHx` block decomposes to one representative SCI variant
/// (`SCI_A_ENGINE`) regardless of which native SCI variant is actually
/// connected, so it must match ANY connected SCI variant at the same index
/// (here, a link connected as `SCI_B_TRANS`) -- but must still respect index
/// scoping (Finding 1's fix) for a different index of the same family.
#[tokio::test]
#[serial]
async fn get_resource_status_j2610_chx_matches_any_sci_variant_at_the_same_index() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    // `SCI_B_TRANS`'s own `_CHx` block collapses to the same representative
    // `PROTOCOL_J2610_CHx` id any other SCI variant's block would
    // (`resources::chx_base_protocol_id`'s single-representative collapse),
    // so naming the representative id directly at index 3 exercises exactly
    // the same resolution a `SCI_B_TRANS`-specific `_CHx` id would.
    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_J2610_CH1 + 2), // _CH3
        1,
    )
    .await
    .expect("a directly-named J2610 _CH3 id should resolve to SCI_A_ENGINE's own _CHx block");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 7_812).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect should succeed for the J2610 _CH3 link");

    // Same index: the raw _CHx id (any SCI variant decomposes to the same
    // one, per `resources::chx_base_protocol_id`'s single-representative
    // collapse) must match the connected link.
    let same_index_raw = j2534_0404::PROTOCOL_J2610_CH1 + 2; // _CH3
    assert!(
        resource_in_use(&mut client, same_index_raw).await,
        "a literal J2610 _CH3 query must match the connected _CH3 link"
    );

    // Different index (_CH5): must NOT match, even though it is still the
    // J2610/SCI family -- index scoping still applies.
    let different_index_raw = j2534_0404::PROTOCOL_J2610_CH1 + 4; // _CH5
    assert!(
        !resource_in_use(&mut client, different_index_raw).await,
        "a literal J2610 _CH5 query must not match a link connected at _CH3"
    );

    server.shutdown().await;
}

/// edge-case-hunter finding 2: `check_chx_capacity`'s `channel_index >
/// cached_available_count` rejection (`discovery.rs`) only had unit-level
/// coverage against a directly-constructed `J2534Service` -- this drives the
/// same precheck through the real gRPC/mock stack end-to-end:
/// `ConnectComLogicalLink` populates the Discovery cache as a side effect of
/// its own cache-miss native `GET_DEVICE_INFO` call, then immediately
/// rejects synchronously (no native `PassThruConnect` attempt at all) once
/// the resolved index exceeds the answer's packed `_CHx` count.
#[tokio::test]
#[serial]
async fn connect_rejects_a_chx_index_exceeding_the_discovery_cached_capacity() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    // A known, deliberately small cached count -- distinct from the mock's
    // own `DEFAULT_CHX_CAPACITY` default, so this test does not silently
    // depend on that default staying what it is today.
    server.backdoor.set_chx_capacity(2);
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_CAN_CH1 + 4), // _CH5
        1,
    )
    .await
    .expect(
        "create_com_logical_link only checks that the named _CHx id falls inside the full \
         clause-24 region for an in-scope family, so _CH5 resolves fine at create time -- the \
         capacity precheck runs at connect",
    );
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "_CH5 exceeds the cached _CHx capacity (2) -- must be rejected synchronously by \
             check_chx_capacity, not attempted natively",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the capacity precheck must reject before any native PassThruConnect is attempted -- \
         proving this is a synchronous precheck rejection, not a native-layer error"
    );

    server.shutdown().await;
}
