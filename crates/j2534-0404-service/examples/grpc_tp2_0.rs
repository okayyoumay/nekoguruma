//! Demonstrates SAE J2534-2 clause 19 TP2.0 Protocol access
//! (`ChannelProtocol::TP2_0_PS`, native `TP2_0_DWCAN` bus,
//! ADR-188/Phase 7 Stage 7a) through the `j2534-0404-service` gRPC
//! interface: resolves the `SAE_J2819_TP2_0` resource, stages TP2.0's five
//! mandatory active-connection ComParams, drives `CoptStartcomm`'s
//! connection-establishment handshake, and a single `CoptSendrecv`.
//!
//! SAE J2819 TP2.0 is VW/Audi's connection-oriented CAN transport (K-line's
//! successor on that manufacturer's CAN-based vehicles); this codebase
//! defines no client-facing KWP2000-on-TP2.0 message decoder for this
//! example to target, so `request-data` below is an arbitrary placeholder
//! byte sequence, not a documented diagnostic request.
//!
//! This is a **pure gRPC client** -- it connects to an already-running
//! `j2534-0404-service` process (see `crates/j2534-0404-service/docs/startup-spec.md`); it does not
//! start a server itself. To see a real response you need a real
//! J2534-compatible interface wired to a live TP2.0-capable CAN bus.
//!
//! # ComParam addressing notes -- five mandatory params staged before connect
//!
//! Verified against `events_tp20_connection.rs` and
//! `tests/grpc_mock/tp20.rs`: five `PARAM_TP20_*` ComParams (Table 78's
//! connection-request fields) must ALL be staged via `SetComParam` before
//! `CoptStartcomm` -- `run_tp20_connection_request` rejects the call
//! synchronously if any is missing:
//!
//! - `CP_TP20ChannelSetupCanId` -- the setup/handshake CAN identifier.
//! - `CP_TP20DestinationAddress` -- the target ECU's TP2.0 address (1 byte).
//! - `CP_TP20TxIdProposal`/`CP_TP20RxIdProposal` -- this CLL's proposed
//!   TX/RX CAN IDs for the connection once established (2 bytes each).
//! - `CP_TP20ApplicationType` -- the diagnostic application class (1 byte).
//!
//! `comparam_defaults.rs`'s `tp2_0_dwcan` preset deliberately leaves all
//! five unset (no spec-mandated default, ADR-188 Decision item 4) -- unlike
//! every other example in this directory, connecting without staging them
//! first is a genuine error, not merely a suboptimal default.
//!
//! **These five ComParams have NO `GetObjectId`/name-based resolution at
//! all** -- verified directly against `names.rs`'s `map_comparam_name_*`
//! functions (no `"cp_tp20*"` entry anywhere) and confirmed by
//! `tests/grpc_mock/tp20.rs`, which addresses them by raw numeric
//! `ComParamId` (`0x80C9`-`0x80CD`, `service_params.rs`) rather than through
//! `GetObjectId`/`common::comparam_object_id` the way every other ComParam
//! in this file (and every other example) is resolved. This example follows
//! the same raw-numeric-ID approach the test suite uses, since
//! `common::comparam_object_id("CP_TP20ChannelSetupCanId")` would fail with
//! "unrecognized ComParam shortname" against this codebase as it stands
//! (see the Prioritized Backlog for
//! this gap).
//!
//! `CP_Baudrate` IS settable and name-resolvable (clause 19.3.1's fixed
//! 500 kbit/s is still a "default", not a hard-wired value,
//! `comparam_support::is_tp20_param`); this example re-stages it at its
//! already-seeded default purely to demonstrate the mechanism.
//!
//! # Usage
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_tp2_0 -- <service-addr> \
//!     [destination-address] [request-data]
//! ```
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_tp2_0 -- http://127.0.0.1:60124
//! ```
//!
//! Defaults: `destination-address=0x10`, `request-data=10,81` (arbitrary
//! KWP2000-shaped placeholder, see above). The channel-setup CAN ID
//! (`0x700`), TX-ID proposal (`0x300`), RX-ID proposal (`0x321`), and
//! application type (`1`) are fixed in this example -- override them by
//! editing the constants below if your bench setup needs different values.

#[path = "common/mod.rs"]
mod common;

use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, ExpectedResponseData, GetResourceIdsRequest, ParamItem,
    PduParamClass, PinData, ResourceData, SetComParamRequest, StartComPrimitiveRequest,
    create_com_logical_link_request, param_item, pin_data, resource_data,
};

const PROTOCOL_NAME: &str = "SAE_J2819_TP2_0";
const BUS_TYPE_NAME: &str = "TP2_0_DWCAN";
const FIXED_BAUD_RATE: u32 = 500_000;

/// Wait timeout for the `CoptStartcomm` TP2.0 connection handshake, in place
/// of `common::DEFAULT_RESPONSE_TIMEOUT_MS` (2000ms) -- mirrors
/// `grpc_iso9141.rs`'s own `INIT_TIMEOUT_MS` rationale. The service itself
/// (`events_tp20_connection.rs`'s `TP20_CONNECTION_TIMEOUT_MS`) bounds the
/// handshake to exactly 2000ms, but that deadline only starts once the poll
/// task dequeues this COP -- an unbounded-in-practice-but-real delay this
/// client-side wait must additionally cover, on top of the 2000ms itself.
/// 5000ms gives generous headroom over "2000ms starting late" rather than
/// racing it with an equal or barely-larger client timeout.
const STARTCOMM_TIMEOUT_MS: u32 = 5000;

// `PARAM_TP20_*` raw numeric ComParam IDs (`service_params.rs`) -- see this
// file's own doc comment for why these bypass `common::comparam_object_id`.
const CP_TP20_CHANNEL_SETUP_CAN_ID: u32 = 0x80C9;
const CP_TP20_DESTINATION_ADDRESS: u32 = 0x80CA;
const CP_TP20_TX_ID_PROPOSAL: u32 = 0x80CB;
const CP_TP20_RX_ID_PROPOSAL: u32 = 0x80CC;
const CP_TP20_APPLICATION_TYPE: u32 = 0x80CD;

const CHANNEL_SETUP_CAN_ID: u32 = 0x0000_0700;
const TX_ID_PROPOSAL: u32 = 0x0300;
const RX_ID_PROPOSAL: u32 = 0x0321;
const APPLICATION_TYPE: u32 = 1;

async fn run(bin: &str, mut args: std::env::Args) -> common::Result<()> {
    let extra_usage = "[destination-address] [request-data]";
    let common_args = common::parse_common_args(bin, &mut args, extra_usage);

    let destination_address = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "destination-address", &raw))
        .unwrap_or(0x10);
    if destination_address > 0xFF {
        return Err(format!(
            "destination-address {destination_address:#x} must fit in one byte (0x00-0xFF)"
        )
        .into());
    }
    let request_data = args
        .next()
        .map(|raw| common::parse_data_bytes_arg(bin, extra_usage, "request-data", &raw))
        .unwrap_or_else(|| vec![0x10, 0x81]);

    println!(
        "config: protocol={PROTOCOL_NAME} baud={FIXED_BAUD_RATE} \
         destination_address={destination_address:#x} request={request_data:02x?}"
    );

    let (mut client, module_handle) =
        common::connect_and_open_module(&common_args.service_addr).await?;

    let cp_baudrate = common::comparam_object_id(&mut client, "CP_Baudrate").await?;

    // ── GetResourceIds(protocol="SAE_J2819_TP2_0", bus="TP2_0_DWCAN") ──
    let pins = vec![
        PinData {
            dlc_pin_number: 6,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName("HI".to_string())),
        },
        PinData {
            dlc_pin_number: 14,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName("LOW".to_string())),
        },
    ];
    let resource_ids = client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(module_handle),
            resource_data: Some(ResourceData {
                dlc_pin_data: pins,
                bus_type: Some(resource_data::BusType::BusTypeName(
                    BUS_TYPE_NAME.to_string(),
                )),
                protocol: Some(resource_data::Protocol::ProtocolName(
                    PROTOCOL_NAME.to_string(),
                )),
            }),
        })
        .await?
        .into_inner();
    let resource_id = resource_ids
        .resource_id_list
        .and_then(|list| list.resource_id_data_array.into_iter().next())
        .and_then(|data| data.resource_id_array.into_iter().next())
        .ok_or(format!(
            "no resource id resolved for protocol {PROTOCOL_NAME:?}"
        ))?;
    println!("resource_id = {resource_id:#x}");

    // ── CreateComLogicalLink ─────────────────────────────────────────────
    let cll_response = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(module_handle),
            resource: Some(create_com_logical_link_request::Resource::ResourceId(
                resource_id,
            )),
            cll_create_flag: None,
        })
        .await?
        .into_inner();
    let cll_handle = cll_response
        .cll_handle
        .ok_or("cll_handle missing from CreateComLogicalLink response")?;
    println!("cll_handle = {}", cll_handle.cll_handle);

    // ── SetComParam: baud plus the five mandatory TP2.0 connection params ──
    for (id, value) in [
        (cp_baudrate, FIXED_BAUD_RATE),
        (CP_TP20_CHANNEL_SETUP_CAN_ID, CHANNEL_SETUP_CAN_ID),
        (CP_TP20_DESTINATION_ADDRESS, destination_address),
        (CP_TP20_TX_ID_PROPOSAL, TX_ID_PROPOSAL),
        (CP_TP20_RX_ID_PROPOSAL, RX_ID_PROPOSAL),
        (CP_TP20_APPLICATION_TYPE, APPLICATION_TYPE),
    ] {
        client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(param_item::Id::ParamId(id)),
                    com_param_class: PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(value)),
                }),
            })
            .await?;
    }

    // ── ConnectComLogicalLink → SubscribeEvent ──────────────────────────
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await?;
    println!("connected");
    let mut events = common::subscribe(&mut client, cll_handle).await?;

    // ── CoptStartcomm (establishes the TP2.0 connection) ─────────────────
    println!("starting communication (destination_address={destination_address:#x})");
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: Some(b"startcomm".to_vec()),
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await?;
    if !common::wait_for_cop_finished(&mut events, STARTCOMM_TIMEOUT_MS, b"startcomm").await {
        return Err(
            "TP2.0 connection handshake did not complete successfully -- aborting before \
             CoptSendrecv"
                .into(),
        );
    }

    // ── CoptSendrecv ─────────────────────────────────────────────────────
    println!("sending request {request_data:02x?}");
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: Some(b"sendrecv".to_vec()),
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptSendrecv as i32,
            cop_data: request_data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    acceptance_id: 0,
                    response_type: 0,
                    mask_data: vec![0x00],
                    pattern_data: vec![0x00],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await?;
    match common::wait_for_send_recv_response(
        &mut events,
        common::DEFAULT_RESPONSE_TIMEOUT_MS + 500,
        b"sendrecv",
    )
    .await
    {
        Some(bytes) => println!("ECU response received: {bytes:02x?}"),
        None => println!(
            "WARNING: no response received -- expected if no TP2.0-capable ECU answers at \
             destination_address={destination_address:#x} on this vehicle"
        ),
    }

    // ── CoptStopcomm ─────────────────────────────────────────────────────
    match client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: Some(b"stopcomm".to_vec()),
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
    {
        Ok(_) => println!("start_com_primitive(CoptStopcomm) succeeded"),
        Err(e) => eprintln!("WARNING: start_com_primitive(CoptStopcomm) failed: {e}"),
    }
    let _ = common::wait_for_cop_finished(
        &mut events,
        common::DEFAULT_RESPONSE_TIMEOUT_MS,
        b"stopcomm",
    )
    .await;

    // ── Teardown ─────────────────────────────────────────────────────────
    common::teardown(&mut client, cll_handle, module_handle).await?;

    println!("flow completed successfully");
    Ok(())
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args();
    let bin = args.next().unwrap_or_else(|| "grpc_tp2_0".to_string());
    if let Err(err) = run(&bin, args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
