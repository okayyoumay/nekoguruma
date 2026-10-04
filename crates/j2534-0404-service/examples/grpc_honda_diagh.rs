//! Demonstrates SAE J2534-2 clause 13 Honda DIAG-H Protocol access
//! (`ChannelProtocol::HONDA_DIAGH_PS`, native `HONDA_DIAGH_UART` bus,
//! ADR-174/Phase 10) through the `j2534-0404-service` gRPC interface:
//! resolves the `HONDA_DIAGH` resource and drives a single `CoptSendrecv`
//! carrying Honda's proprietary "92 Hm/2" diagnostic message format.
//!
//! This is a **pure gRPC client** -- it connects to an already-running
//! `j2534-0404-service` process (see `crates/j2534-0404-service/docs/startup-spec.md`); it does not
//! start a server itself. To see a real response you need a real
//! J2534-compatible interface wired to a Honda DIAG-H-capable ECU
//! (pre-CAN Honda/Acura OBD-II vehicles).
//!
//! # ComParam addressing notes
//!
//! Verified against `comparam_support.rs`'s `is_honda_diagh_param` allowlist:
//! `CP_Loopback`/`CP_P1Max`/`CP_P3Min`/`CP_P4Min` are the ONLY settable
//! ComParams -- `CP_Baudrate` is deliberately excluded (clause 13.3.1 fixes
//! the bit rate at 9600 bps internally; `comparam_defaults.rs`'s
//! `honda_diagh_uart` bus preset seeds it for the internal connect call, but
//! it stays unreachable via `SetComParam`/`GetComParam`). There is no
//! addressing ComParam of any kind (no target address, no
//! `SetUniqueRespIdTable`) -- clause 13.2.4 fixes this protocol to a single
//! J1962 pin (14 by default, or pin 1 as the only alternative), so which ECU
//! you reach is determined by physical wiring, not addressing configuration.
//! `P1_MAX`/`P3_MIN`/`P4_MIN` reuse the existing ISO9141 K-line timing
//! parameters per clause 13.3.1's own text (rather than defining new ones);
//! this example re-stages them at their already-seeded defaults purely to
//! demonstrate the mechanism.
//!
//! Unlike the K-line protocols (`grpc_iso9141.rs`/`grpc_iso14230.rs`), this
//! protocol is classified `is_kline == false` (ADR-174 Decision 5, verified
//! against `rpc_primitive.rs`): clause 13.3.1 defines no init process at
//! all, so `CoptStartcomm`'s `cop_data` -- left empty here -- would be
//! treated as a genuine optional message (the same fire-and-forget path
//! `grpc_can.rs` uses), not a 5-baud/fast-init address byte.
//!
//! # Usage
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_honda_diagh -- <service-addr> \
//!     [request-data]
//! ```
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_honda_diagh -- http://127.0.0.1:60124
//! ```
//!
//! Default: `request-data=00,f0,00` (this codebase defines no documented
//! Honda DIAG-H application-layer request; this is an arbitrary placeholder
//! byte sequence, not a real diagnostic request -- padded to 3 bytes, this
//! protocol's own `PROTOCOL_HONDA_DIAGH_PS` TX minimum, `protocol.rs::
//! tx_message_size_range`; `build_tx_message` (`tx_header.rs`) prepends no
//! header for this protocol, so `request-data` IS the complete wire frame).

#[path = "common/mod.rs"]
mod common;

use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, ExpectedResponseData, GetResourceIdsRequest, ParamItem,
    PduParamClass, PinData, ResourceData, SetComParamRequest, StartComPrimitiveRequest,
    create_com_logical_link_request, param_item, pin_data, resource_data,
};

const PROTOCOL_NAME: &str = "HONDA_DIAGH";
const BUS_TYPE_NAME: &str = "HONDA_DIAGH_UART";
// `PROTOCOL_HONDA_DIAGH_PS`'s TX minimum (`protocol.rs::
// tx_message_size_range`). `build_tx_message` (`tx_header.rs`) prepends no
// header for this protocol -- unlike CAN/ISO15765/KWP/J1850/J1939, whose
// addressing ComParams build a header this service prepends automatically
// -- so `request-data` below IS the complete wire frame, and a shorter one
// is synchronously rejected by `resolve_send_recv_tx` before ever reaching
// the adapter.
const MIN_REQUEST_DATA_LEN: usize = 3;

async fn run(bin: &str, mut args: std::env::Args) -> common::Result<()> {
    let extra_usage = "[request-data]";
    let common_args = common::parse_common_args(bin, &mut args, extra_usage);

    let request_data = args
        .next()
        .map(|raw| common::parse_data_bytes_arg(bin, extra_usage, "request-data", &raw))
        .unwrap_or_else(|| vec![0x00, 0xF0, 0x00]);
    if request_data.len() < MIN_REQUEST_DATA_LEN {
        return Err(format!(
            "request-data {request_data:02x?} is only {} byte(s), but PROTOCOL_HONDA_DIAGH_PS \
             requires at least {MIN_REQUEST_DATA_LEN} -- this protocol has no header this \
             service prepends automatically, so request-data must already be a complete, \
             protocol-valid frame",
            request_data.len()
        )
        .into());
    }

    println!("config: protocol={PROTOCOL_NAME} request={request_data:02x?}");

    let (mut client, module_handle) =
        common::connect_and_open_module(&common_args.service_addr).await?;

    let cp_loopback = common::comparam_object_id(&mut client, "CP_Loopback").await?;
    let cp_p1max = common::comparam_object_id(&mut client, "CP_P1Max").await?;
    let cp_p3min = common::comparam_object_id(&mut client, "CP_P3Min").await?;
    let cp_p4min = common::comparam_object_id(&mut client, "CP_P4Min").await?;

    // ── GetResourceIds(protocol="HONDA_DIAGH", bus="HONDA_DIAGH_UART") ──
    let pins = vec![PinData {
        dlc_pin_number: 14,
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

    // ── SetComParam: loopback, K-line timing (already-seeded defaults) ─────
    for (id, value) in [
        (cp_loopback, 0),
        (cp_p1max, 20_000),
        (cp_p3min, 55_000),
        (cp_p4min, 5_000),
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

    // ── CoptStartcomm (no init sequence for this protocol) ───────────────
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
            "WARNING: no response received -- expected if no Honda DIAG-H-capable ECU answers \
             on this vehicle"
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
    let bin = args
        .next()
        .unwrap_or_else(|| "grpc_honda_diagh".to_string());
    if let Err(err) = run(&bin, args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
