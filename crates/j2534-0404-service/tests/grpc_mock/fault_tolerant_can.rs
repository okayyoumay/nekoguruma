//! End-to-end coverage for SAE J2534-2 clause 7 Additional Channels (`_CHx`)
//! support on Fault-Tolerant CAN (ISO 11898-3, clause 20, ADR-211) -- the
//! shared CAN-collapse-family infrastructure fix's own pathfinder round.
//! Mirrors `j1708.rs`'s/`honda_diagh.rs`'s own `_CHx`-coverage structure
//! (raw-id connect, compound-name connect, and a family-specific behavioral
//! regression), reusing `ft_can.rs`'s own `_PS`-level helpers/constants
//! wherever the underlying mechanism is shared rather than duplicating them.
//!
//! Unlike every other family this series has closed (ADR-206 through
//! ADR-210), Fault-Tolerant CAN is a CAN-collapse family:
//! `resources::base_protocol_id` normalizes `FT_CAN_PS`/`FT_ISO15765_PS` onto
//! the true `CAN`/`ISO15765` base rather than self-identifying. The two
//! tests in the "shared infrastructure fix" section below are the ones that
//! specifically fail without ADR-211's `base_protocol_id`
//! recursion/`resolve_channel_selection` raw-row-id fixes -- see each test's
//! own doc comment for exactly which fix it pins.
//!
//! `connect_discovery_check`'s/`bustype_default_name_for_hw_protocol_id`'s
//! own `_CHx` coverage is verified as `resources.rs` unit tests instead of
//! here (`connect_discovery_check_covers_ft_can_chx_the_same_as_its_ps_sibling`,
//! `bustype_default_name_for_hw_protocol_id_covers_every_in_scope_family`'s
//! new `_CH1` cases), matching this codebase's own established convention
//! for this kind of check (see `j1708.rs`'s/`tp20.rs`'s own equivalents,
//! which are likewise `resources.rs` unit tests, not `grpc_mock` integration
//! tests).

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest,
    GetResourceStatusRequest, ModuleAndResourceId, ModuleHandle, ResourceData,
    create_com_logical_link_request, module_and_resource_id, resource_data,
};

use crate::harness::*;

/// Resource id of the FTCAN sibling of the native `ISO_11898_RAW` resource
/// (0x0201) -- `resources.rs` row 0x0230, `protocol_name =
/// "ISO_11898_RAW_FTCAN"`, `hw_protocol_override = PROTOCOL_FT_CAN_PS`. Used
/// by the compound-name connect test below (as
/// `"ISO_11898_RAW_FTCAN_CH1"`).
const FT_CAN_RAW_RESOURCE_NAME: &str = "ISO_11898_RAW_FTCAN";

/// Resource id of the FTCAN sibling of the native `ISO_15765_2` resource
/// (0x0206) -- `resources.rs` row 0x0235, `protocol_name =
/// "ISO_15765_2_FTCAN"`, `hw_protocol_override = PROTOCOL_FT_ISO15765_PS`.
/// Used by the compound-name connect test below (as
/// `"ISO_15765_2_FTCAN_CH1"`).
const FT_ISO15765_RESOURCE_NAME: &str = "ISO_15765_2_FTCAN";

const FT_ISO15765_RESOURCE_ID: u32 = 0x0235;

/// A representative ISO 11898-3 Fault-Tolerant CAN baud rate; the mock does
/// not validate baud rate values, so any nonzero value would work, but a
/// domain-plausible one (Table B.21's 125 kbit/s default) keeps these tests
/// self-documenting -- same constant `ft_can.rs` uses.
const FTCAN_BAUD_RATE: u32 = 125_000;

/// Builds a `RscData` resource selecting `protocol_id` via the raw
/// hardware-protocol-id route, with no `dlc_pin_data` -- a `_CHx` id has no
/// J1962 pin concept at all (clause 7's vendor-connector model), so every
/// test in this file that connects a `_CHx` id directly supplies none.
fn resource_with_protocol_id(protocol_id: u32) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: None,
        protocol: Some(resource_data::Protocol::ProtocolId(protocol_id)),
    }
}

/// Builds a `RscData` resource selecting a resource by `protocol_name`, with
/// no `dlc_pin_data` -- used for the compound `_CHx`-suffixed name route.
fn resource_with_protocol_name(name: &str) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: None,
        protocol: Some(resource_data::Protocol::ProtocolName(name.to_string())),
    }
}

/// Creates (but does not connect) a CLL for `resource`, on
/// [`MOCK_MODULE_HANDLE`].
async fn create_cll(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource: ResourceData,
) -> ComLogicalLinkHandle {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(resource)),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every FT resource row requires this opt-in (`names.rs`'s FT arm in
/// `resolve_pin_selection`), same as `ft_can.rs`'s own helper.
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

// ── ADR-211: shared CAN-collapse-family infrastructure fix ─────────────────

/// A directly-named `PROTOCOL_FT_CAN_CH1` id establishes its own physical
/// channel independently of a `PROTOCOL_FT_CAN_PS` sibling on the same
/// module -- confirms `resources::chx_block_base`'s new
/// `PROTOCOL_FT_CAN_PS => Some(PROTOCOL_FT_CAN_CH1)` entry resolves end to
/// end, and that a `_CHx` id (no J1962 pin concept at all) connects with no
/// `dlc_pin_data`. Mirrors `j1708.rs`'s own
/// `j1708_ch1_establishes_independently_alongside_a_j1708_ps_sibling`.
#[tokio::test]
#[serial]
async fn ft_can_ch1_establishes_independently_alongside_a_ft_can_ps_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let ps_cll = create_cll(
        &mut client,
        resource_with_protocol_id(FT_ISO15765_RESOURCE_ID),
    )
    .await;
    set_com_param_unum32(&mut client, ps_cll, j2534_0404::DATA_RATE, FTCAN_BAUD_RATE).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(ps_cll),
        })
        .await
        .expect("connect_com_logical_link should succeed for the _PS sibling");
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FT_ISO15765_PS
    );

    let chx_cll = create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_FT_CAN_CH1),
    )
    .await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(chx_cll),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for a directly-named PROTOCOL_FT_CAN_CH1 id",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the _PS sibling and its own _CH1 Additional Channel should open two distinct physical \
         channels"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID + 1),
        j2534_0404::PROTOCOL_FT_CAN_CH1,
        "the second connect should open a native PROTOCOL_FT_CAN_CH1 channel, not a plain \
         PROTOCOL_CAN_CH1 id -- this is the assertion that would have failed without \
         base_protocol_id's ADR-211 recursion fix feeding chx_block_base a CAN-collapse-keyed \
         block"
    );

    server.shutdown().await;
}

/// Connecting via the compound `_CHx`-suffixed `protocol_name` grammar
/// (`"ISO_11898_RAW_FTCAN_CH1"`, the FT_CAN_PS resource row's own
/// `protocol_name` with a `_CH1` suffix) succeeds, and -- critically --
/// resolves to the native `PROTOCOL_FT_CAN_CH1` id, not a plain
/// `PROTOCOL_CAN_CH1` id. This is the test that would fail without EITHER of
/// ADR-211's two fixes landing together: a broken `names.rs`
/// `requested_index.is_some()` bypass (Decision item 4) would reject this
/// connect outright as "mutually exclusive" (clause 6 Pin Selection vs.
/// clause 7 Additional Channel); a broken `resolve_channel_selection`
/// raw-row-id fix (Decision item 3) would let the connect succeed but
/// silently downgrade it to plain `CAN_CH1` -- only the native protocol id
/// asserted via the mock backdoor below discriminates between "succeeded
/// with the right id" and "succeeded with the wrong (downgraded) id".
#[tokio::test]
#[serial]
async fn ft_can_connecting_via_compound_chx_name_succeeds_with_the_correct_native_id() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(
        &mut client,
        resource_with_protocol_name(&format!("{FT_CAN_RAW_RESOURCE_NAME}_CH1")),
    )
    .await;

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
        j2534_0404::PROTOCOL_FT_CAN_CH1,
        "the compound-name route must open a native PROTOCOL_FT_CAN_CH1 channel -- a plain \
         PROTOCOL_CAN_CH1 here would mean resolve_channel_selection silently downgraded the \
         caller's FT intent (the exact ADR-211 Decision item 3 bug)"
    );

    server.shutdown().await;
}

/// Same as above, for the ISO15765-based FT sibling
/// (`"ISO_15765_2_FTCAN_CH1"`) -- confirms the fix holds for both of
/// `chx_block_base`'s new entries, not just the raw-CAN one.
#[tokio::test]
#[serial]
async fn ft_iso15765_connecting_via_compound_chx_name_succeeds_with_the_correct_native_id() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(
        &mut client,
        resource_with_protocol_name(&format!("{FT_ISO15765_RESOURCE_NAME}_CH1")),
    )
    .await;

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
        j2534_0404::PROTOCOL_FT_ISO15765_CH1,
        "the compound-name route must open a native PROTOCOL_FT_ISO15765_CH1 channel, not a \
         plain PROTOCOL_ISO15765_CH1 id"
    );

    server.shutdown().await;
}

/// Codex review finding on PR #124 (ADR-211): a directly-named
/// `PROTOCOL_FT_CAN_CH1` connect (no resource-table row, no `pin_selection`
/// at all -- a `_CHx` id has no J1962 pin concept) with no explicit
/// `SetComParam(DATA_RATE, ...)` staged beforehand must still receive Table
/// B.21's own 125_000bps `ISO_11898_3_DWFTCAN` default, the same as
/// `ft_can.rs`'s own `_PS`-level
/// `connecting_the_raw_ft_protocol_id_seeds_the_125000_baud_rate_default`
/// already proves for `PROTOCOL_FT_CAN_PS`. Before this fix, `raw_hw_id`
/// (`rpc_link.rs`, the connect-time value fed to
/// `resources::bustype_default_name_for_hw_protocol_id`) only ever consulted
/// `pin_selection`/`protocol.j2534_protocol_id()`, never `channel_selection`
/// -- for a `_CHx`-only connect that falls to
/// `protocol.j2534_protocol_id()`, which for Fault-Tolerant CAN (a
/// CAN-collapse family, unlike every other `_CHx`-enabled standalone family)
/// normalizes to the generic `CAN`/`ISO15765` id, never matching
/// `is_ft_family_protocol_id`. The Working ComParam set stayed empty on this
/// route and `PassThruConnect` silently received a native baud rate of 0
/// instead of the FT bustype's own real default -- this is exactly the
/// assertion (`baud_rate`, not just RPC success or the connected native
/// protocol id) that would have caught it.
#[tokio::test]
#[serial]
async fn connecting_the_raw_ft_can_chx_id_seeds_the_125000_baud_rate_default() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_FT_CAN_CH1),
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for a directly-named PROTOCOL_FT_CAN_CH1 \
             id with no DATA_RATE staged",
        );

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        FTCAN_BAUD_RATE,
        "the raw-id _CHx FT connect route must still receive Table B.21's own 125_000bps \
         ISO_11898_3_DWFTCAN default, not the empty Working set's default of 0 -- this is the \
         Codex review finding on PR #124 (ADR-211): raw_hw_id must also consult \
         channel_selection, not just pin_selection/protocol.j2534_protocol_id()"
    );

    server.shutdown().await;
}

/// Sibling of the test above, for the ISO15765 FT sub-variant
/// (`PROTOCOL_FT_ISO15765_CH1`) -- confirms the fix holds for both `_CHx`
/// blocks the same way `ft_can.rs`'s own `_PS`-level
/// `connecting_the_raw_ft_iso15765_protocol_id_seeds_the_125000_baud_rate_default`
/// does for `PROTOCOL_FT_ISO15765_PS`.
#[tokio::test]
#[serial]
async fn connecting_the_raw_ft_iso15765_chx_id_seeds_the_125000_baud_rate_default() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_FT_ISO15765_CH1),
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for a directly-named \
             PROTOCOL_FT_ISO15765_CH1 id with no DATA_RATE staged",
        );

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        FTCAN_BAUD_RATE,
        "the raw-id _CHx FT_ISO15765 connect route must still receive the 125_000bps \
         ISO_11898_3_DWFTCAN default, not the empty Working set's default of 0"
    );

    server.shutdown().await;
}

// ── ADR-211 second Codex review-round correction: `check_chx_capacity` keys
// FT-CAN's own `_CHx` capacity check on `DEVICE_INFO_FT_CAN_SUPPORTED`/
// `DEVICE_INFO_FT_ISO15765_SUPPORTED`, not the generic
// `DEVICE_INFO_CAN_SUPPORTED`/`DEVICE_INFO_ISO15765_SUPPORTED` ─────────────

/// Edge-case-hunter finding on this same PR's final review sweep: the two
/// unit tests covering this fix (`resources.rs`'s
/// `chx_device_info_supported_parameter_keys_ft_can_by_the_ft_flag_not_the_generic_can_flag`
/// and `discovery.rs`'s
/// `chx_capacity_keys_ft_can_by_the_raw_ft_id_not_the_collapsed_base`) both
/// call the fixed functions directly -- neither one exercises the actual
/// call site in `rpc_link.rs::rpc_connect_com_logical_link` that this fix
/// changed (`check_chx_capacity`'s third argument, `base_proto_id` ->
/// `j2534_proto_id`). Reverting just that one argument back to
/// `base_proto_id` was confirmed to leave every other `fault_tolerant_can.rs`/
/// `additional_channels.rs` test passing, because `j2534-0404-mock`'s
/// `chx_capacity_override` was, until this test, a single value shared by
/// every packed-capacity `DEVICE_INFO_*` flag -- `DEVICE_INFO_CAN_SUPPORTED`
/// and `DEVICE_INFO_FT_CAN_SUPPORTED` always reported the identical count, so
/// accept/reject was structurally indistinguishable through the real
/// gRPC/mock stack regardless of which flag `check_chx_capacity` actually
/// consulted.
///
/// This test closes that gap using the new, independent
/// `__mock_set_ft_can_chx_capacity` override (distinct from
/// `__mock_set_chx_capacity`): the generic families' (and native
/// `PassThruConnect`-time) capacity is set HIGH (10, comfortably above index
/// 5), while FT-CAN's own Discovery-reported capacity is set LOW (1). Under
/// the fix, `check_chx_capacity` consults `DEVICE_INFO_FT_CAN_SUPPORTED`
/// (capacity 1) and synchronously rejects index 5 before any native
/// `PassThruConnect` attempt. Under the bug this fix corrects,
/// `check_chx_capacity` would have consulted `DEVICE_INFO_CAN_SUPPORTED`
/// (capacity 10) instead, let index 5 through the precheck, and reached a
/// native connect that itself succeeds (native `PassThruConnect` enforcement
/// is keyed on the shared `chx_capacity_override`, also 10) -- i.e. the
/// connect would wrongly SUCCEED instead of being rejected.
#[tokio::test]
#[serial]
async fn connect_rejects_a_chx_index_within_the_generic_capacity_but_above_ft_cans_own() {
    let server = start_j2534_2_server().await;
    server.backdoor.set_chx_capacity(10);
    server.backdoor.set_ft_can_chx_capacity(1);
    let mut client = server.client().await;

    let cll_handle = create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_FT_CAN_CH1 + 4), // _CH5
    )
    .await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "_CH5 is within the generic families' cached capacity (10) but exceeds \
             Fault-Tolerant CAN's own (1) -- must be rejected synchronously by \
             check_chx_capacity consulting DEVICE_INFO_FT_CAN_SUPPORTED, not \
             DEVICE_INFO_CAN_SUPPORTED",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the capacity precheck must reject before any native PassThruConnect is attempted -- \
         if this fires, check_chx_capacity consulted the generic (higher) capacity instead of \
         Fault-Tolerant CAN's own, exactly the bug this fix corrects"
    );

    server.shutdown().await;
}

// ── ADR-211 Decision item 6: `rpc_link.rs`'s `GetResourceStatus` query-
// candidate-matching sites (`rpc_link.rs:1352`/`:1642`), re-keyed to
// `is_ft_family_protocol_id`, correctly find a `_CHx`-connected FT link ────

/// A directly-named `PROTOCOL_FT_ISO15765_CH1` id is found by a
/// `GetResourceStatus` query using that SAME raw `_CHx` id -- edge-case-
/// hunter finding: this new call-site class (`rpc_link.rs`'s query-resolution
/// `None if ... || resources::is_ft_family_protocol_id(id)` branch,
/// `rpc_link.rs:1352`) had no dedicated regression test despite ADR-211
/// Decision item 6 requiring one. Mirrors `ft_can.rs`'s own
/// `get_resource_status_with_the_raw_ft_protocol_id_finds_a_connected_ft_link`
/// (the `_PS` case), adapted to a `_CHx`-connected link: before the
/// `is_ft_protocol_id` -> `is_ft_family_protocol_id` re-key at that call
/// site, a raw `_CHx` query id would fall through to the generic
/// `hw_override: None` branch instead of preserving the queried id as its
/// own override, so `status_hw_id`/`hardware_native_hw_id` would never equal
/// the connected link's own raw `_CHx` `hw_protocol_id`, and the sibling
/// `matches_status_hw_id` skip-guard (`rpc_link.rs:1642`, ADR-164/ADR-168's
/// "Bug 1" fix) would additionally refuse to let the link's normalized
/// `base` (`ISO15765`) satisfy the query either -- so the connected link
/// would be silently reported idle.
#[tokio::test]
#[serial]
async fn get_resource_status_with_the_raw_ft_chx_id_finds_a_connected_chx_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_FT_ISO15765_CH1),
    )
    .await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for a directly-named \
             PROTOCOL_FT_ISO15765_CH1 id",
        );
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_FT_ISO15765_CH1,
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
        1,
        "querying the raw PROTOCOL_FT_ISO15765_CH1 id directly must report bit 0 (in use) when \
         the matching _CHx-connected FT link is actually connected, not silently fail to match \
         it and report idle"
    );

    server.shutdown().await;
}

/// Reverse direction of the test above: a plain (non-FT) `ISO15765_CH1`
/// link connected at the same channel index must NOT satisfy a
/// `GetResourceStatus` query for the raw `PROTOCOL_FT_ISO15765_CH1` id --
/// confirms the query-side fix does not over-broaden the match into treating
/// every `_CH1` link of the family as interchangeable, mirroring `ft_can.rs`'s
/// own `_PS`-level
/// `get_resource_status_with_the_raw_ft_protocol_id_does_not_match_a_plain_dual_wire_link`.
#[tokio::test]
#[serial]
async fn get_resource_status_with_the_raw_ft_chx_id_does_not_match_a_plain_chx_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_ISO15765_CH1),
    )
    .await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for a plain PROTOCOL_ISO15765_CH1 id");
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_FT_ISO15765_CH1,
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
        0,
        "querying the raw PROTOCOL_FT_ISO15765_CH1 id must not report a plain (non-FT) \
         PROTOCOL_ISO15765_CH1 link at the same index as satisfying it"
    );

    server.shutdown().await;
}

// ── ADR-211: `apply_fd_mode`'s FT-vs-FD rejection guard, re-keyed to
// `is_ft_family_protocol_id`, still fires for a `_CHx`-connected link ───────

/// Regression test for `rpc_link.rs`'s `apply_fd_mode` re-keying (ADR-211):
/// staging FD ComParams (`CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate`) on a
/// CLL resolved to a directly-named `PROTOCOL_FT_CAN_CH1` id and then
/// connecting is rejected outright, the same as `ft_can.rs`'s own
/// `staging_fd_comparams_on_an_ft_link_is_rejected_not_substituted` already
/// proves for the `_PS` case -- without the `is_ft_protocol_id` ->
/// `is_ft_family_protocol_id` re-keying, `base_protocol_id`'s recursion fix
/// (this same round) would let a `_CHx`-connected FT link's `hw_protocol_id`
/// resolve to plain `CAN` for `apply_fd_mode`'s own outer match, and the
/// narrower `is_ft_protocol_id` guard would then never recognize
/// `PROTOCOL_FT_CAN_CH1` as FT at all -- silently promoting it to
/// `FD_CAN_PS` instead of rejecting it.
#[tokio::test]
#[serial]
async fn staging_fd_comparams_on_a_chx_connected_ft_link_is_rejected_not_substituted() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(
        &mut client,
        resource_with_protocol_id(j2534_0404::PROTOCOL_FT_CAN_CH1),
    )
    .await;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        j2534_0404::DATA_RATE,
        FTCAN_BAUD_RATE,
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_BAUDRATE, 2_000_000).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "connecting a PROTOCOL_FT_CAN_CH1-resolved CLL with FD ComParams staged must be \
             rejected, not silently substituted to FD_CAN_PS",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("Fault-Tolerant CAN"),
        "the rejection should name the FTCAN-vs-FD conflict: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejection must happen before any native PassThruConnect is attempted"
    );

    server.shutdown().await;
}

/// Same as above, for a compound-name `_CHx` connect
/// (`"ISO_15765_2_FTCAN_CH1"`) rather than a directly-named raw `_CHx` id --
/// confirms the guard fires regardless of which of the two `_CHx` routes
/// resolved the link.
#[tokio::test]
#[serial]
async fn staging_fd_comparams_on_a_compound_name_chx_connected_ft_link_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(
        &mut client,
        resource_with_protocol_name(&format!("{FT_ISO15765_RESOURCE_NAME}_CH1")),
    )
    .await;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        j2534_0404::DATA_RATE,
        FTCAN_BAUD_RATE,
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_BAUDRATE, 2_000_000).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "connecting a compound-_CH1-name-resolved FT_ISO15765 CLL with FD ComParams staged \
             must be rejected, not silently substituted to FD_ISO15765_PS",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("Fault-Tolerant CAN"),
        "the rejection should name the FTCAN-vs-FD conflict: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejection must happen before any native PassThruConnect is attempted"
    );

    server.shutdown().await;
}
