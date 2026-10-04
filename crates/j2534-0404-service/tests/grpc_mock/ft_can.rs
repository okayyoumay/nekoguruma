//! End-to-end coverage for SAE J2534-2 clause 20 Fault-Tolerant CAN (ISO
//! 11898-3, ADR-168, Phase 6): connecting an FT resource row's mandatory
//! internal `SET_CONFIG(CONFIG_J1962_PINS)` (clause 20.2.1's explicit-pin-only
//! model, same "always explicit, never implicit" connect
//! ADR-164's SWCAN precedent established), the row's own two-pin (pin 1/HI,
//! pin 9/LOW) default, and the caller's ability to override to the second
//! documented pin-pair (pins 3/11) via ordinary Pin Selection. Mirrors
//! `sw_can.rs`'s structure and helpers wherever the underlying mechanism is
//! shared (many helpers live in `harness.rs` and are reused here, not
//! duplicated).
//!
//! Unlike SWCAN (ADR-164 Decision 2/3), this phase adds no new ComParam
//! translations or IOCTLs (ADR-168 Decision 5) -- so, unlike `sw_can.rs`,
//! there is no `CP_ChangeSpeed*`/`CP_SwCan_HighVoltage`/`SW_CAN_HS`/`SW_CAN_NS`
//! analog to cover here.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest,
    GetResourceIdsRequest, GetResourceStatusRequest, ModuleAndResourceId, ModuleHandle, PinData,
    ResourceData, create_com_logical_link_request, module_and_resource_id, resource_data,
};

use crate::harness::*;

/// Resource id of the FTCAN sibling of the native `ISO_15765_2` resource
/// (0x0206) -- `resources.rs` row 0x0235, `hw_protocol_override =
/// PROTOCOL_FT_ISO15765_PS`. The primary link used by the tests in this
/// file, mirroring `sw_can.rs`'s own `SW_ISO15765_RESOURCE_ID` choice.
const FT_ISO15765_RESOURCE_ID: u32 = 0x0235;

/// Resource id of the FTCAN sibling of the native `ISO_11898_RAW` resource
/// (0x0201) -- `resources.rs` row 0x0230, `hw_protocol_override =
/// PROTOCOL_FT_CAN_PS`. Used by the `GetResourceStatus` cross-occupancy
/// regression pair below, mirroring `sw_can.rs`'s own `SW_CAN_RAW_RESOURCE_ID`.
const FT_CAN_RAW_RESOURCE_ID: u32 = 0x0230;

/// A representative ISO 11898-3 Fault-Tolerant CAN baud rate; the mock does
/// not validate baud rate values, so any nonzero value would work, but a
/// domain-plausible one (Table B.21's 125 kbit/s default) keeps these tests
/// self-documenting.
const FTCAN_BAUD_RATE: u32 = 125_000;

/// Builds a `RscData` resource selecting `resource_id` via the numeric
/// resource-id route, with the given typed `(pin_number, pin_type_name)`
/// pairs as `dlc_pin_data` -- same shape as `pin_selection.rs`'s own
/// `resource_with_protocol_id_and_pins`, specialized to a resource id rather
/// than a raw hardware protocol id (both resolve through the same
/// `resolve_protocol_id`/`resolve_pin_selection` path).
fn resource_with_resource_id_and_pins(resource_id: u32, pins: &[(u32, &str)]) -> ResourceData {
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
        protocol: Some(resource_data::Protocol::ProtocolId(resource_id)),
    }
}

/// Creates (but does not connect) a CLL for `resource_id`/`pins`, on module 1
/// -- same shape as `sw_can.rs`'s `create_cll_for_resource_id`, extended to
/// carry `dlc_pin_data` so a caller-supplied pin pair can be exercised too.
async fn create_cll_for_resource_id_and_pins(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_id: u32,
    pins: &[(u32, &str)],
    _cll_tag: u64,
) -> ComLogicalLinkHandle {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(vci_service_interface::ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_resource_id_and_pins(resource_id, pins),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// Like [`create_cll_for_resource_id_and_pins`], but also stages `params` and
/// connects -- same shape as `sw_can.rs`'s
/// `create_and_connect_cll_for_resource_id`.
async fn create_and_connect_cll_for_resource_id_and_pins(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_id: u32,
    pins: &[(u32, &str)],
    params: &[(u32, u32)],
) -> ComLogicalLinkHandle {
    let cll_handle = create_cll_for_resource_id_and_pins(client, resource_id, pins, 1).await;

    for &(com_param_id, value) in params {
        set_com_param_unum32(client, cll_handle, com_param_id, value).await;
    }

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every FT resource row requires this opt-in (`names.rs`'s FT arm in
/// `resolve_pin_selection`, mirroring the SWCAN opt-in gate).
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

/// Item 1/3: connecting via an FT resource id (one of the nine mirrored
/// ISO15765 variants, `ISO_15765_2_FTCAN`) succeeds, resolves and connects
/// with `PROTOCOL_FT_ISO15765_PS` as the hardware protocol id, and the
/// connect sequence includes the mandatory internal
/// `SET_CONFIG(CONFIG_J1962_PINS)` -- clause 20.2.1's explicit-pin-only
/// model (ADR-168 Decision 1/2),
/// mirroring ADR-164 Decision 1's identical SWCAN requirement. With no
/// caller-supplied `dlc_pin_data`, the row's own two-pin default (pin 1/HI,
/// pin 9/LOW) is what gets assigned, packed `0x0000_0109`.
#[tokio::test]
#[serial]
async fn connecting_an_ft_resource_emits_the_internal_pins_set_config() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_resource_id_and_pins(
        &mut client,
        FT_ISO15765_RESOURCE_ID,
        &[],
        &[(j2534_0404::DATA_RATE, FTCAN_BAUD_RATE)],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FT_ISO15765_PS,
        "connecting resource 0x0235 should open a native FT_ISO15765_PS channel"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0109,
        "an unqualified FT connect (no dlc_pin_data) should assign the row's own default pin \
         pair (pin 1/HI, pin 9/LOW), packed 0x0000PPSS"
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "CONFIG_J1962_PINS should have been SET_CONFIG'd during connect; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Item 1 (second pin-pair): a caller requesting clause 20.2.1's other
/// documented pin-pair (pins 3/11) via ordinary Pin Selection gets exactly
/// that pin pair assigned, not the row's own default -- confirming the
/// resource row's default doesn't silently override an explicit caller
/// choice, and that FTCAN's two-pin (not SWCAN's single-pin) `dlc_pin_data`
/// shape round-trips correctly through `compute_pin_select`.
#[tokio::test]
#[serial]
async fn connecting_an_ft_resource_with_explicit_pins_uses_the_caller_supplied_pin_pair() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_resource_id_and_pins(
        &mut client,
        FT_ISO15765_RESOURCE_ID,
        &[(3, "HI"), (11, "LOW")],
        &[(j2534_0404::DATA_RATE, FTCAN_BAUD_RATE)],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FT_ISO15765_PS
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_030B,
        "an explicit (3/HI, 11/LOW) Pin Selection request should override the row's own \
         (1/HI, 9/LOW) default"
    );

    server.shutdown().await;
}

/// Regression test (edge-case-hunter close-out finding on PR #63's Honda
/// DIAG-H round-4 fix, `names.rs::row_needs_dynamic_pin_selection`): the same
/// shared predicate that fixed Honda DIAG-H's canonical-name-plus-non-default-pin
/// connect also fixes this identical latent gap for FT-CAN -- naming the
/// canonical `protocol_name` ("ISO_11898_RAW_FTCAN", row 0x0230) with
/// `dlc_pin_data` selecting the *alternate* documented pin-pair (3/HI,
/// 11/LOW, clause 20.2.1's second option, not row 0x0230's own (1/HI, 9/LOW)
/// default) must succeed, not be rejected against the row's own fixed pin
/// data before `resolve_pin_selection`'s own correct closed-set check ever
/// runs (mirrors `connecting_an_ft_resource_with_explicit_pins_uses_the_caller_supplied_pin_pair`
/// above, but via the canonical-name route instead of the resource-id route).
#[tokio::test]
#[serial]
async fn connecting_via_canonical_name_with_the_alternate_pin_pair_succeeds() {
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
                            dlc_pin_number: 3,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "HI".to_string(),
                                ),
                            ),
                        },
                        PinData {
                            dlc_pin_number: 11,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "LOW".to_string(),
                                ),
                            ),
                        },
                    ],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolName(
                        "ISO_11898_RAW_FTCAN".to_string(),
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

    set_com_param_unum32(
        &mut client,
        cll_handle,
        j2534_0404::DATA_RATE,
        FTCAN_BAUD_RATE,
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_030B,
        "the canonical-name route must assign the caller's own explicit alternate pin-pair, not \
         reject it against the resource-table row's (1/HI, 9/LOW) default"
    );

    server.shutdown().await;
}

/// Item 2 (regression for the guard `rpc_link::J2534Service::apply_fd_mode`
/// gained for FT, mirroring its pre-existing SWCAN guard, ADR-164 Decision
/// 1): staging FD ComParams (`CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate`) on
/// an FT-connected link and then connecting is rejected outright, not
/// silently substituted to `FD_ISO15765_PS` -- `base_protocol_id`'s FT arms
/// (needed so `comparam_id::to_j2534_config_id` treats an FT link as
/// CAN-family for free, ADR-168 Decision 5) would otherwise let an FT link
/// fall into the same CAN-FD substitution branch a SWCAN link does.
#[tokio::test]
#[serial]
async fn staging_fd_comparams_on_an_ft_link_is_rejected_not_substituted() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle =
        create_cll_for_resource_id_and_pins(&mut client, FT_ISO15765_RESOURCE_ID, &[], 1).await;
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
            "connecting an FT_ISO15765_PS-resolved CLL with FD ComParams staged must be \
             rejected, not silently substituted to FD_ISO15765_PS",
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

/// Item 4 (edge-case-hunter finding, BLOCKING, fixed): `GetResourceStatus`'s
/// `matches_status_hw_id` closure (`rpc_link.rs`) drops the normalized-`base`
/// candidate from matching when the *queried* resource's own hardware id is
/// an SW/FT-family id, so a query naming the plain dual-wire CAN resource
/// (0x0201) does not fall back to matching an FT-connected link's `base`
/// (which `base_protocol_id`'s new FT arms, ADR-168 Decision 1, normalize
/// down to plain `CAN` -- the same collapse SWCAN already produces). The
/// guard originally only checked `is_sw_protocol_id(hw_id)` (ADR-164's own
/// Bug 1 fix); this diff's `base_protocol_id` FT arms newly exposed the same
/// hazard class to FT links, and the guard was not extended to also check
/// `is_ft_protocol_id(hw_id)` until this fix. Mirrors `sw_can.rs`'s own
/// pinned regression pair for the identical SWCAN hazard.
#[tokio::test]
#[serial]
async fn get_resource_status_does_not_let_a_connected_ft_can_link_occupy_its_dual_wire_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _ft_cll = create_and_connect_cll_for_resource_id_and_pins(
        &mut client,
        FT_CAN_RAW_RESOURCE_ID,
        &[],
        &[(j2534_0404::DATA_RATE, FTCAN_BAUD_RATE)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(0x0201)), // ISO_11898_RAW (plain dual-wire CAN)
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
        "the plain dual-wire CAN resource (0x0201) must NOT report bit 0 (in use) while only \
         its FTCAN sibling (0x0230) is connected -- they are physically distinct buses (ADR-168)"
    );

    server.shutdown().await;
}

/// Item 4 (reverse direction): connecting only the plain dual-wire CAN
/// resource must NOT make `GetResourceStatus` report the FTCAN resource
/// (0x0230) as "in use" either. Kept alongside the previous test so both
/// directions of the FT-vs-dual-wire physical-resource distinction are
/// pinned in one place, mirroring `sw_can.rs`'s own pair.
#[tokio::test]
#[serial]
async fn get_resource_status_does_not_let_a_connected_dual_wire_can_link_occupy_its_ft_can_sibling()
{
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _can_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    FT_CAN_RAW_RESOURCE_ID,
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
        "the FTCAN resource (0x0230) must NOT report bit 0 (in use) while only its plain \
         dual-wire CAN sibling (0x0201) is connected -- they are physically distinct buses \
         (ADR-168)"
    );

    server.shutdown().await;
}

/// Item 5 (Codex review round 2 finding, P2, fixed): naming the raw
/// `FT_ISO15765_PS` hardware protocol id directly (bypassing the resource
/// table entirely -- `resolve_protocol_id` finds no matching row for this
/// numeric value, so `resource_row` is `None`) with no `dlc_pin_data` must
/// be rejected, not silently connected with the resource-table row's own
/// two-pin default (`0x0000_0109`) -- that default only reflects a choice
/// this diff's *table row* made, not a choice the caller made; clause
/// 20.2.1 itself has no default pin-pair at all. Confirms
/// `resolve_pin_selection`'s FT arm now gates the hardcoded default on an
/// actual resource-table row match, not merely on the hardware id matching
/// the FT family.
#[tokio::test]
#[serial]
async fn connecting_the_raw_ft_protocol_id_directly_without_pins_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_FT_ISO15765_PS,
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "naming the raw FT_ISO15765_PS id directly with no dlc_pin_data must be rejected, \
             not silently defaulted to the resource-table row's own pin pair",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("no default pin"),
        "the rejection should explain that clause 20.2.1 has no default pin-pair: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 9 (Codex review round 6/design-advisor, P2, fixed): SAE J2534-2
/// clause 20.2.1 defines FT-CAN (ISO 11898-3) as connectable on exactly two
/// pin-pairs -- (1,HI)+(9,LOW) or (3,HI)+(11,LOW) -- unlike the general
/// clause 6 mechanism, which stays permissive for CAN_PS/ISO15765_PS/FD
/// variants. A single explicit pin (physically incomplete: no secondary
/// wire) previously packed fine via `compute_pin_select` (a zero secondary
/// byte, e.g. `0x0000_0300` for a lone pin 3/HI) and was accepted outright.
/// `resolve_pin_selection`'s FT arm now rejects any packed result other
/// than the two documented pairs.
#[tokio::test]
#[serial]
async fn connecting_an_ft_resource_with_a_single_explicit_pin_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_resource_id_and_pins(FT_ISO15765_RESOURCE_ID, &[(3, "HI")]),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "a single explicit pin has no secondary wire, so it does not form either of clause \
             20.2.1's two documented FT-CAN pin-pairs and must be rejected",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("20.2.1"),
        "the rejection should cite clause 20.2.1's two documented pin-pairs: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 9 (mismatched pair, Codex review round 6/design-advisor, P2, fixed):
/// a pair combining one pin from each documented pin-pair (pin 1/HI, the
/// first pair's primary, with pin 11/LOW, the second pair's secondary)
/// packs to a well-formed `0x0000_010B` per the generic pin-typing rules in
/// `compute_pin_select`, but is not one of clause 20.2.1's two documented
/// pin-pairs and must still be rejected.
#[tokio::test]
#[serial]
async fn connecting_an_ft_resource_with_a_mismatched_pin_pair_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_resource_id_and_pins(
                    FT_ISO15765_RESOURCE_ID,
                    &[(1, "HI"), (11, "LOW")],
                ),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "pin 1/HI + pin 11/LOW mixes primaries/secondaries from the two different \
             documented pin-pairs and must be rejected, not silently accepted just because it \
             packs to a well-formed bitmask",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("20.2.1"),
        "the rejection should cite clause 20.2.1's two documented pin-pairs: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 6 (Codex review round 3 finding, P2, fixed): querying
/// `GetResourceStatus` with the raw `FT_ISO15765_PS` hardware protocol id
/// directly as a `ResourceId` (bypassing the resource table entirely --
/// `find_by_resource_id` finds no matching row for this numeric value)
/// must correctly report bit 0 ("in use") when an FT link is actually
/// connected. Before the fix, the query-side fallback normalized this raw
/// id down to plain `ISO15765` with no override, so `status_hw_id`/
/// `hardware_native_hw_id` collapsed to `ISO15765` -- combined with the
/// `matches_status_hw_id` guard (Item 4 above) refusing to let an FT
/// link's normalized `base` candidate match, the connected FT link was
/// never found, silently reporting the raw-id query as idle.
#[tokio::test]
#[serial]
async fn get_resource_status_with_the_raw_ft_protocol_id_finds_a_connected_ft_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _ft_cll = create_and_connect_cll_for_resource_id_and_pins(
        &mut client,
        FT_ISO15765_RESOURCE_ID,
        &[],
        &[(j2534_0404::DATA_RATE, FTCAN_BAUD_RATE)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_FT_ISO15765_PS,
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
        "querying the raw FT_ISO15765_PS id directly must report bit 0 (in use) when an FT \
         link is actually connected, not silently normalize the query to plain ISO15765 and \
         miss it"
    );

    server.shutdown().await;
}

/// Item 6 (reverse direction): a plain dual-wire ISO15765 link must NOT
/// satisfy a raw `FT_ISO15765_PS` `ResourceId` query -- the query-side fix
/// above preserves the raw FT id as the query candidate's own override, so
/// a plain-family link (whose `hw_protocol_id` isn't FT) no longer matches
/// it via the normalized-base fallback either.
#[tokio::test]
#[serial]
async fn get_resource_status_with_the_raw_ft_protocol_id_does_not_match_a_plain_dual_wire_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _can_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_FT_ISO15765_PS,
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
        "querying the raw FT_ISO15765_PS id directly must NOT report bit 0 (in use) when only \
         a plain dual-wire ISO15765 link is connected -- they are physically distinct buses"
    );

    server.shutdown().await;
}

/// Item 10 (final close-out `edge-case-hunter` pass, should-fix coverage
/// gap): the round-5 `GetResourceIds(protocol_id = PROTOCOL_FT_ISO15765_PS)`
/// fix (Item 8 below) and the round-6/7 pin-pair validation fix (Item 9
/// above) are each pinned only in isolation -- neither test chains a
/// discovery-returned resource_id into a subsequent connect that exercises
/// pin-pair validation. This test does: `GetResourceIds` resolves the FT
/// ISO15765 rows via the raw hardware protocol id, one of the returned
/// resource_ids is then used to (a) connect with a valid documented pin pair
/// (3/HI, 11/LOW) and succeed, and (b) attempt a second `CreateComLogicalLink`
/// on that same discovered resource_id with a mismatched pair (1/HI, 11/LOW)
/// and be rejected -- confirming discovery and pin-pair validation compose
/// correctly, not just work independently.
#[tokio::test]
#[serial]
async fn discovered_ft_resource_id_then_connect_validates_pin_pairs() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let ft_iso15765_ids =
        get_resource_ids_by_protocol_id(&mut client, j2534_0404::PROTOCOL_FT_ISO15765_PS).await;
    let discovered_resource_id = *ft_iso15765_ids
        .first()
        .expect("PROTOCOL_FT_ISO15765_PS discovery should return at least one FTCAN row");

    // (a) a valid documented pin pair on the discovered resource_id succeeds.
    let _cll = create_and_connect_cll_for_resource_id_and_pins(
        &mut client,
        discovered_resource_id,
        &[(3, "HI"), (11, "LOW")],
        &[(j2534_0404::DATA_RATE, FTCAN_BAUD_RATE)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FT_ISO15765_PS,
        "connecting a discovery-returned resource_id should still open a native \
         FT_ISO15765_PS channel"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_030B,
        "the (3/HI, 11/LOW) pair should be assigned on a discovery-returned resource_id, same \
         as it is via the FT_ISO15765_RESOURCE_ID constant used elsewhere in this file"
    );

    // (b) a mismatched pair on a second CLL against that same discovered
    // resource_id must still be rejected, not silently accepted because the
    // resource_id came from discovery rather than a hardcoded constant.
    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_resource_id_and_pins(
                    discovered_resource_id,
                    &[(1, "HI"), (11, "LOW")],
                ),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "a mismatched pin pair on a discovery-returned resource_id must be rejected, the \
             same as it is via the FT_ISO15765_RESOURCE_ID constant used elsewhere in this file",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("20.2.1"),
        "the rejection should cite clause 20.2.1's two documented pin-pairs: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 11 (final close-out `edge-case-hunter` pass, minor coverage gap):
/// `compute_pin_select` classifies each `dlc_pin_data` entry by its own
/// `DlcPinType` (HI/LOW), not by its position in the array -- every other
/// valid-pin-pair test in this file supplies pins in HI-then-LOW order, so
/// this pins that LOW-then-HI order still packs identically.
#[tokio::test]
#[serial]
async fn connecting_an_ft_resource_with_pins_in_low_then_hi_order_packs_the_same_value() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_resource_id_and_pins(
        &mut client,
        FT_ISO15765_RESOURCE_ID,
        &[(9, "LOW"), (1, "HI")],
        &[(j2534_0404::DATA_RATE, FTCAN_BAUD_RATE)],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FT_ISO15765_PS
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0109,
        "a LOW-then-HI pin order should pack identically to the HI-then-LOW order used \
         elsewhere in this file -- classification is pin-TYPE-based, not array-position-based"
    );

    server.shutdown().await;
}

/// Item 10 (this PR's brief, generalizing the Honda DIAG-H/SAE J1708
/// `rpc_create_com_logical_link` fallback arms to FT-CAN via
/// `resources::bustype_default_name_for_hw_protocol_id`): naming the raw
/// `FT_CAN_PS` hardware protocol id directly (bypassing the resource table
/// entirely, `resource_row == None`) with no caller-supplied `bus_type_name`
/// and no `SetComParam(DATA_RATE, ...)` staged must still receive Table
/// B.21's own 125_000bps ISO 11898-3 DW-FT-CAN default (ADR-130) -- before
/// this fix, the Working ComParamSet stayed empty on this route and
/// `PassThruConnect` received a native baud rate of 0.
#[tokio::test]
#[serial]
async fn connecting_the_raw_ft_protocol_id_seeds_the_125000_baud_rate_default() {
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
                            dlc_pin_number: 1,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "HI".to_string(),
                                ),
                            ),
                        },
                        PinData {
                            dlc_pin_number: 9,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "LOW".to_string(),
                                ),
                            ),
                        },
                    ],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_FT_CAN_PS,
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
        125_000,
        "the raw-id no-resource-row FT connect route must still receive Table B.21's own \
         125_000bps ISO_11898_3_DWFTCAN default, not the empty Working set's default of 0"
    );

    server.shutdown().await;
}

/// Item 11 (sibling of Item 10, edge-case-hunter-style regression): the same
/// raw `FT_CAN_PS` connect, but WITH a caller-supplied, unrelated,
/// well-formed `bus_type_name` (`RscData.bus_type`/`.protocol` are
/// independent fields with no cross-validation) -- this FT link's own fixed
/// bustype identity must win, not the mismatched name's own bustype
/// defaults.
#[tokio::test]
#[serial]
async fn connecting_the_raw_ft_protocol_id_ignores_a_mismatched_bus_type_name() {
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
                            dlc_pin_number: 1,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "HI".to_string(),
                                ),
                            ),
                        },
                        PinData {
                            dlc_pin_number: 9,
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
                        j2534_0404::PROTOCOL_FT_CAN_PS,
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
        125_000,
        "a mismatched bus_type_name must not override this FT link's own ISO_11898_3_DWFTCAN \
         125_000bps default"
    );

    server.shutdown().await;
}

/// Item 12 (ISO15765 sub-variant landmine, this PR's design-consult
/// finding): `PROTOCOL_FT_ISO15765_PS`'s own `ChannelProtocol` normalizes to
/// plain `ISO15765` (the generic CAN-family identity,
/// `resources::base_protocol_id`), NOT the FT family -- so a fix keyed on
/// `protocol.j2534_protocol_id()` alone would silently never recognize this
/// sub-variant. `FT_CAN_PS` (Item 10 above) is equally affected by the same
/// landmine -- its own `base_protocol_id` normalizes to plain `CAN`, not its
/// own raw id either -- so this dedicated ISO15765-variant test exists to
/// pin the identical landmine for the ISO15765 framing specifically, proving
/// the fix is keyed on `pin_selection`'s own `ps_protocol_id`, not the
/// normalized `protocol`, for both variants.
#[tokio::test]
#[serial]
async fn connecting_the_raw_ft_iso15765_protocol_id_seeds_the_125000_baud_rate_default() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_resource_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_FT_ISO15765_PS,
        &[(3, "HI"), (11, "LOW")],
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        125_000,
        "the raw-id no-resource-row FT_ISO15765_PS connect route must still receive the \
         125_000bps ISO_11898_3_DWFTCAN default, proving the fix keys on pin_selection's own \
         ps_protocol_id, not the normalized-to-base protocol identity"
    );

    server.shutdown().await;
}

/// Issues `GetResourceIds` filtered on `resource_data::Protocol::ProtocolId(id)`
/// and returns the resolved resource IDs in table order -- same shape as
/// `resources.rs`'s own `get_resource_ids` helper, specialized to this file's
/// module handle constant.
async fn get_resource_ids_by_protocol_id(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    protocol_id: u32,
) -> Vec<u32> {
    client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource_data: Some(ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(resource_data::Protocol::ProtocolId(protocol_id)),
            }),
        })
        .await
        .expect("get_resource_ids should succeed")
        .into_inner()
        .resource_id_list
        .and_then(|list| list.resource_id_data_array.into_iter().next())
        .map(|data| data.resource_id_array)
        .unwrap_or_default()
}

/// Item 8 (Codex review finding, `candidates_matching_protocol`'s `ProtocolId`
/// fallback never consulted `row.hw_protocol_override`, so a raw `_PS`
/// hardware protocol id -- the only place the FT `_PS` ids live for every FT
/// resource row, `resources.rs` rows 0x0230-0x0239 -- matched zero table rows
/// even though it can be used directly with `CreateComLogicalLink`/
/// `ConnectComLogicalLink`/`GetResourceStatus`, as this file's other tests
/// show): `GetResourceIds(protocol_id = PROTOCOL_FT_CAN_PS)` must resolve the
/// raw-CAN FTCAN row (0x0230), and `GetResourceIds(protocol_id =
/// PROTOCOL_FT_ISO15765_PS)` must resolve every ISO15765-based FTCAN row
/// (0x0231-0x0239), mirroring `names.rs`'s `legacy_bustype_hw_id` fallback
/// shape (which already consulted `hw_protocol_override`) that
/// `candidates_matching_protocol` lacked.
#[tokio::test]
#[serial]
async fn get_resource_ids_by_raw_ft_protocol_id_resolves_the_ft_rows() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let ft_can_ids =
        get_resource_ids_by_protocol_id(&mut client, j2534_0404::PROTOCOL_FT_CAN_PS).await;
    assert_eq!(
        ft_can_ids,
        vec![FT_CAN_RAW_RESOURCE_ID],
        "PROTOCOL_FT_CAN_PS should resolve only the raw-CAN FTCAN row (0x0230), via \
         hw_protocol_override, not the empty list the pre-fix fallback returned"
    );

    let ft_iso15765_ids =
        get_resource_ids_by_protocol_id(&mut client, j2534_0404::PROTOCOL_FT_ISO15765_PS).await;
    assert_eq!(
        ft_iso15765_ids,
        vec![
            0x0231, 0x0232, 0x0233, 0x0234, 0x0235, 0x0236, 0x0237, 0x0238, 0x0239
        ],
        "PROTOCOL_FT_ISO15765_PS should resolve every ISO15765-based FTCAN row, via \
         hw_protocol_override, not the empty list the pre-fix fallback returned"
    );

    server.shutdown().await;
}

/// Item 7 (edge-case-hunter finding, coverage gap): Items 5 and 6 above are
/// each tested in isolation -- Item 5 only exercises the *rejected*
/// no-pins half of naming the raw `FT_ISO15765_PS` id directly, and Item 6
/// connects via the resource-*table* id (`FT_ISO15765_RESOURCE_ID`) rather
/// than the raw bypass route before querying by raw id. This test
/// exercises both fixes together: connecting via the raw `FT_ISO15765_PS`
/// hardware protocol id directly (bypassing the resource table entirely,
/// `resource_row: None`) WITH explicit `dlc_pin_data` supplied succeeds
/// (the accepted half of Item 5 -- only the no-pins case is rejected), and
/// the resulting connected link is then correctly found by a
/// `GetResourceStatus` query keyed on that same raw id (Item 6's
/// query-side fix).
#[tokio::test]
#[serial]
async fn connecting_the_raw_ft_protocol_id_with_explicit_pins_then_querying_by_raw_id_finds_the_link()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_resource_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_FT_ISO15765_PS,
        &[(3, "HI"), (11, "LOW")],
        &[(j2534_0404::DATA_RATE, FTCAN_BAUD_RATE)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FT_ISO15765_PS,
        "connecting the raw FT_ISO15765_PS id directly with explicit pins should still open a \
         native FT_ISO15765_PS channel"
    );

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_FT_ISO15765_PS,
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
        "a link connected via the raw FT_ISO15765_PS id with explicit dlc_pin_data (no \
         resource-table row match) must still report bit 0 (in use) when queried by that same \
         raw id, not be silently missed"
    );

    server.shutdown().await;
}
