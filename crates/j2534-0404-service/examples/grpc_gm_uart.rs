//! Demonstrates SAE J2534-2 clause 11 GM UART Protocol access (SAE J2740,
//! `ChannelProtocol::GM_UART_PS`, native `GM_UART_UART` bus,
//! ADR-189/Phase 8) through the `j2534-0404-service` gRPC interface:
//! resolves the `GM_UART` resource and drives a single `CoptSendrecv`.
//!
//! This codebase defines no client-facing GM UART application-layer message
//! decoder for this example to target, so `request-data` below is an
//! arbitrary placeholder byte sequence, not a documented diagnostic
//! request.
//!
//! This is a **pure gRPC client** -- it connects to an already-running
//! `j2534-0404-service` process (see `crates/j2534-0404-service/docs/startup-spec.md`); it does not
//! start a server itself. To see a real response you need a real
//! J2534-compatible interface wired to a pre-CAN GM UART-capable ECU.
//!
//! # ComParam addressing notes
//!
//! Verified against `comparam_defaults.rs`'s `gm_uart_uart` bus preset and
//! its own doc comment: clause 11.3's Win32 API section defines NO ComParam
//! concept whatsoever -- no baud rate, no loopback, no timing parameters.
//! `CP_Baudrate` seeds to `0` (unset) by default and MUST be explicitly
//! `SetComParam`'d to a real value before `ConnectComLogicalLink` --
//! confirmed unusual enough to double-check directly against source rather
//! than assume, since every other UART-family protocol in this codebase
//! seeds a real default; this service performs no additional validation
//! rejecting a still-zero baud rate at connect time (unlike, say, Analog
//! Inputs' `CP_AnalogSampleRate` zero-rejection), but real hardware will not
//! accept a 0 bps connect. `GM_UART_PS` has no dedicated ComParam allowlist
//! branch (`comparam_support.rs`, ADR-189 leaves this unscoped) and falls
//! through to that function's generic "Unknown protocol -- allow" tail, so
//! every ComParam is technically settable/gettable on this link, not just
//! `CP_Baudrate`. There is no addressing ComParam of any kind -- GM UART is
//! a master/slave poll-message bus, not an addressed request/response one.
//!
//! Clause 11's own poll-message/poll-response bus-mastership handshake is
//! exposed through this codebase as two dedicated `IoCtl` commands,
//! `PDU_IOCTL_SET_POLL_RESPONSE`/`PDU_IOCTL_BECOME_MASTER` (verified against
//! `tests/grpc_mock/gm_uart.rs`) -- both ARE gRPC-reachable via the generic
//! `IoCtl` RPC, but configuring them is out of scope for this basic
//! connect+send/receive example, which sticks to the same
//! `CoptStartcomm`/`CoptSendrecv`/`CoptStopcomm` shape every other example
//! in this directory uses.
//!
//! # Usage
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_gm_uart -- <service-addr> \
//!     [baud-rate] [request-data]
//! ```
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_gm_uart -- http://127.0.0.1:60124
//! ```
//!
//! Defaults: `baud-rate=8192` (a commonly-cited real-world GM UART bit rate
//! in third-party literature -- NOT spec-mandated, since clause 11.3 defines
//! no default; this example's own pick, override for your bench setup),
//! `request-data=f4,01,00` (arbitrary placeholder, see above -- padded to 3
//! bytes, this protocol's own `PROTOCOL_GM_UART_PS` TX minimum,
//! `protocol.rs::tx_message_size_range`: the destination/source/length
//! header bytes GM UART messages require; `build_tx_message`
//! (`tx_header.rs`) prepends no header for this protocol, so `request-data`
//! IS the complete wire frame).

#[path = "common/mod.rs"]
mod common;

use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, ExpectedResponseData, GetResourceIdsRequest, ParamItem,
    PduParamClass, PinData, ResourceData, SetComParamRequest, StartComPrimitiveRequest,
    create_com_logical_link_request, param_item, pin_data, resource_data,
};

const PROTOCOL_NAME: &str = "GM_UART";
const BUS_TYPE_NAME: &str = "GM_UART_UART";
// `PROTOCOL_GM_UART_PS`'s TX minimum (`protocol.rs::tx_message_size_range`):
// the destination/source/length header bytes every GM UART message
// requires. `build_tx_message` (`tx_header.rs`) prepends no header for this
// protocol, so `request-data` below IS the complete wire frame, and a
// shorter one is synchronously rejected by `resolve_send_recv_tx` before
// ever reaching the adapter.
const MIN_REQUEST_DATA_LEN: usize = 3;

async fn run(bin: &str, mut args: std::env::Args) -> common::Result<()> {
    let extra_usage = "[baud-rate] [request-data]";
    let common_args = common::parse_common_args(bin, &mut args, extra_usage);

    let baud_rate = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "baud-rate", &raw))
        .unwrap_or(8_192);
    let request_data = args
        .next()
        .map(|raw| common::parse_data_bytes_arg(bin, extra_usage, "request-data", &raw))
        .unwrap_or_else(|| vec![0xF4, 0x01, 0x00]);
    if request_data.len() < MIN_REQUEST_DATA_LEN {
        return Err(format!(
            "request-data {request_data:02x?} is only {} byte(s), but PROTOCOL_GM_UART_PS \
             requires at least {MIN_REQUEST_DATA_LEN} (destination/source/length header bytes) \
             -- this protocol has no header this service prepends automatically, so \
             request-data must already be a complete, protocol-valid frame",
            request_data.len()
        )
        .into());
    }

    println!("config: protocol={PROTOCOL_NAME} baud={baud_rate} request={request_data:02x?}");

    let (mut client, module_handle) =
        common::connect_and_open_module(&common_args.service_addr).await?;

    let cp_baudrate = common::comparam_object_id(&mut client, "CP_Baudrate").await?;

    // ── GetResourceIds(protocol="GM_UART", bus="GM_UART_UART") ─────────
    let pins = vec![PinData {
        dlc_pin_number: 9,
        dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName("K".to_string())),
    }];
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

    // ── SetComParam(CP_Baudrate) -- mandatory, clause 11.3 seeds no default ──
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(cp_baudrate)),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(baud_rate)),
            }),
        })
        .await?;

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
            "WARNING: no response received -- expected if no GM UART-capable ECU answers on \
             this vehicle"
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
    let bin = args.next().unwrap_or_else(|| "grpc_gm_uart".to_string());
    if let Err(err) = run(&bin, args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
