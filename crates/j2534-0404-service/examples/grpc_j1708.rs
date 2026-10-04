//! Demonstrates SAE J2534-2 clause 17 SAE J1708 Protocol access
//! (`ChannelProtocol::J1708_PS`, native `SAE_J1708_UART` bus,
//! ADR-175/Phase 11) through the `j2534-0404-service` gRPC interface:
//! resolves the `SAE_J1708` resource and drives a single `CoptSendrecv`.
//!
//! SAE J1708 is a heavy-duty-truck serial bus (engine/transmission/ABS
//! controllers on commercial vehicles); this codebase defines no
//! client-facing SAE J1587-on-J1708 message decoder for this example to
//! target, so `request-data` below is an arbitrary placeholder byte
//! sequence, not a documented J1708/J1587 message.
//!
//! This is a **pure gRPC client** -- it connects to an already-running
//! `j2534-0404-service` process (see `crates/j2534-0404-service/docs/startup-spec.md`); it does not
//! start a server itself. To see a real response you need a real
//! J2534-compatible interface wired to a SAE J1708 bus.
//!
//! # ComParam addressing notes
//!
//! Verified against `comparam_support.rs`'s `is_j1708_param` allowlist:
//! `CP_Baudrate`/`CP_Loopback`/`CP_MessagePriority` are the only settable
//! ComParams -- unlike Honda DIAG-H/UART Echo Byte, `CP_Baudrate` genuinely
//! IS settable here (clause 17.2.2 gives only a minimum-support default, not
//! a fixed value). There is no addressing ComParam at all (no target
//! address, no `SetUniqueRespIdTable`) -- SAE J1708 is a broadcast serial
//! bus with no per-ECU addressing concept at the J2534 layer.
//! `CP_MessagePriority` (clause 17.4.5) selects the SAE J1708 message
//! priority class carried in `TxFlags`, not a destination; `8` (the lowest
//! priority) is `comparam_defaults.rs`'s own seeded default.
//!
//! # Response-matching caveat
//!
//! Like SAE J1939 (see `grpc_j1939.rs`), SAE J1708 is a multi-node bus where
//! several controllers broadcast periodically, so this example's
//! zero-length `mask_data`/`pattern_data` `CoptSendrecv` descriptor accepts
//! the first inbound frame that arrives, from whichever node sends it.
//! Unlike J1939, though, there is no way to narrow that here:
//! `events_rx_routing.rs` (verified) defines no J1708/MID-based RX routing
//! tier at all, so `SetUniqueRespIdTable` has no field to constrain a J1708
//! CLL's inbound routing by originating node. This is a genuine, documented
//! limitation of this example (and of the service's current J1708
//! RX-routing capability), not a bug fixed here.
//!
//! # Usage
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_j1708 -- <service-addr> \
//!     [baud-rate] [message-priority] [request-data]
//! ```
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_j1708 -- http://127.0.0.1:60124
//! ```
//!
//! Defaults: `baud-rate=9600` (clause 17.2.2's minimum-support rate),
//! `message-priority=8` (lowest priority, `comparam_defaults.rs`'s own
//! default), `request-data=00,ff` (arbitrary placeholder, see above).

#[path = "common/mod.rs"]
mod common;

use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, ExpectedResponseData, GetResourceIdsRequest, ParamItem,
    PduParamClass, PinData, ResourceData, SetComParamRequest, StartComPrimitiveRequest,
    create_com_logical_link_request, param_item, pin_data, resource_data,
};

const PROTOCOL_NAME: &str = "SAE_J1708";
const BUS_TYPE_NAME: &str = "SAE_J1708_UART";

async fn run(bin: &str, mut args: std::env::Args) -> common::Result<()> {
    let extra_usage = "[baud-rate] [message-priority] [request-data]";
    let common_args = common::parse_common_args(bin, &mut args, extra_usage);

    let baud_rate = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "baud-rate", &raw))
        .unwrap_or(9_600);
    let message_priority = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "message-priority", &raw))
        .unwrap_or(8);
    if !(1..=8).contains(&message_priority) {
        // Clause 17.4.5's Table 68: MSG_PRIORITY_VALUE is valid 1..=8; the
        // DEVICE itself silently treats 0 or anything above 8 as priority 8
        // (`ComParamSet::msg_priority_tx_flags` implements this same
        // device-level clamp, service.rs) -- so an out-of-range value here
        // would still transmit successfully, just silently AT priority 8
        // rather than the value this example just printed as
        // `message_priority`. Rejected explicitly instead of letting the
        // printed config diverge from what's actually sent on the wire.
        return Err(format!(
            "message-priority {message_priority} is outside SAE J1708's valid 1..=8 range \
             (clause 17.4.5) -- the device would silently transmit at priority 8 instead, not \
             the requested value; supply a value in 1..=8"
        )
        .into());
    }
    let request_data = args
        .next()
        .map(|raw| common::parse_data_bytes_arg(bin, extra_usage, "request-data", &raw))
        .unwrap_or_else(|| vec![0x00, 0xFF]);

    println!(
        "config: protocol={PROTOCOL_NAME} baud={baud_rate} message_priority={message_priority} \
         request={request_data:02x?}"
    );

    let (mut client, module_handle) =
        common::connect_and_open_module(&common_args.service_addr).await?;

    let cp_baudrate = common::comparam_object_id(&mut client, "CP_Baudrate").await?;
    let cp_message_priority = common::comparam_object_id(&mut client, "CP_MessagePriority").await?;

    // ── GetResourceIds(protocol="SAE_J1708", bus="SAE_J1708_UART") ──────
    let pins = vec![
        PinData {
            dlc_pin_number: 3,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName("PLUS".to_string())),
        },
        PinData {
            dlc_pin_number: 11,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName("MINUS".to_string())),
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

    // ── SetComParam(CP_Baudrate, CP_MessagePriority) ────────────────────
    for (id, value) in [
        (cp_baudrate, baud_rate),
        (cp_message_priority, message_priority),
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

    // ── CoptStartcomm → CoptSendrecv → CoptStopcomm ─────────────────────
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: Some(b"startcomm".to_vec()),
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await?;
    let _ = common::wait_for_cop_finished(
        &mut events,
        common::DEFAULT_RESPONSE_TIMEOUT_MS,
        b"startcomm",
    )
    .await;

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
            "WARNING: no response received -- expected if no SAE J1708 ECU answers on this bus"
        ),
    }

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
    let bin = args.next().unwrap_or_else(|| "grpc_j1708".to_string());
    if let Err(err) = run(&bin, args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
