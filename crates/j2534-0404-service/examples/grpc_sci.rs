//! Demonstrates SAE J2610 SCI (Chrysler Single-Wire, native `SAE_J2610_SCI`)
//! access through the `j2534-0404-service` gRPC interface: resolves one of
//! the four `SAE_J2610_SCI` resource rows (`SCI_A_ENGINE`/`SCI_A_TRANS`/
//! `SCI_B_ENGINE`/`SCI_B_TRANS`, native `SAE_J2610_UART` bus,
//! 7.8125 kbit/s), and drives a single `CoptSendrecv`.
//!
//! Unlike every other example in this directory, SCI's four native
//! configurations are covered by ONE file with a CLI-selectable variant --
//! `resources.rs`'s resource table groups them under one shared
//! `protocol_name` ("SAE_J2610_SCI") differing only by `config_name`/DLC
//! pin wiring/`hw_protocol_override` (verified directly against
//! `j2534-0404-service/src/service/resources.rs`'s `SAE_J2610_SCI` block).
//! Since `GetResourceIds`' wire request has no `config_name` selector (only
//! `bus_type`/`protocol`/`dlc_pin_data`, `vci-service-interface`'s
//! `ResourceData` message), the DLC pin wiring itself is what disambiguates
//! which configuration connects -- see `select_config`'s pins below, and
//! `names.rs`'s `retain_rows_matching_all_pins` for the narrowing mechanism.
//!
//! This is a **pure gRPC client** -- it connects to an already-running
//! `j2534-0404-service` process (see `crates/j2534-0404-service/docs/startup-spec.md`); it does not
//! start a server itself. To see a real response you need a real
//! J2534-compatible interface wired to a Chrysler SCI bus (pre-CAN
//! Chrysler/Jeep engine or transmission diagnostics, roughly 1990s-2000s
//! model years) via the SCI configuration selected on the command line.
//!
//! # ComParam addressing notes
//!
//! SCI has NO addressing ComParam at all -- verified against
//! `comparam_support.rs`'s `is_sci_param` allowlist (SCI timing/transmit-mode
//! params only) and `comparam_defaults.rs`'s `sae_j2610_on_sae_j2610_sci`
//! preset (no `CP_Phys*`/`CP_Func*`/`CP_Can*`-style entry anywhere): which
//! ECU you reach is selected entirely by which native SCI configuration
//! (and therefore which physical pin pair) you connect through, not by a
//! ComParam or `SetUniqueRespIdTable` entry. Baud rate (`CP_Baudrate`) IS
//! settable here (`is_universal_param` short-circuits before the SCI-specific
//! allowlist even applies), but the bus preset already seeds SCI's fixed
//! 7.8125 kbit/s rate, so this example only re-stages it to demonstrate the
//! mechanism -- SAE J2610's own bit rate is not something real hardware lets
//! you change.
//!
//! This codebase defines no client-facing SCI application-layer message
//! format (unlike UDS-on-CAN's `22 F1 90` or KWP2000's `10 81`) -- the
//! `request-data` default below is this example's own arbitrary placeholder
//! byte sequence, not a documented SCI diagnostic request.
//!
//! # Usage
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_sci -- <service-addr> \
//!     [config] [request-data]
//! ```
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_sci -- http://127.0.0.1:60124 a-engine
//! ```
//!
//! Defaults: `config=a-engine` (one of `a-engine`/`a-trans`/`b-engine`/
//! `b-trans`, case-insensitive), `request-data=33,01` (arbitrary placeholder,
//! see above).

#[path = "common/mod.rs"]
mod common;

use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, ExpectedResponseData, GetResourceIdsRequest, ParamItem,
    PduParamClass, PinData, ResourceData, SetComParamRequest, StartComPrimitiveRequest,
    create_com_logical_link_request, param_item, pin_data, resource_data,
};

const PROTOCOL_NAME: &str = "SAE_J2610_SCI";
const BUS_TYPE_NAME: &str = "SAE_J2610_UART";
const FIXED_BAUD_RATE: u32 = 7_812;

/// Resolves a CLI config name to its `(config_name, dlc_pins)` -- matching
/// `resources.rs`'s `PINS_SCI_A_ENGINE`/`PINS_SCI_A_TRANS`/
/// `PINS_SCI_B_ENGINE`/`PINS_SCI_B_TRANS` exactly.
fn select_config(name: &str) -> Option<(&'static str, &'static [(u32, &'static str)])> {
    match name.to_ascii_lowercase().as_str() {
        "a-engine" => Some(("SCI_A_ENGINE", &[(6, "TX"), (7, "RX")])),
        "a-trans" => Some(("SCI_A_TRANS", &[(14, "TX"), (7, "RX")])),
        "b-engine" => Some(("SCI_B_ENGINE", &[(12, "TX"), (7, "RX")])),
        "b-trans" => Some(("SCI_B_TRANS", &[(9, "TX"), (15, "RX")])),
        _ => None,
    }
}

async fn run(bin: &str, mut args: std::env::Args) -> common::Result<()> {
    let extra_usage = "[config: a-engine|a-trans|b-engine|b-trans] [request-data]";
    let common_args = common::parse_common_args(bin, &mut args, extra_usage);

    let config_arg = args.next().unwrap_or_else(|| "a-engine".to_string());
    let (config_name, pins) = select_config(&config_arg).ok_or_else(|| {
        format!(
            "invalid config {config_arg:?}: expected one of a-engine, a-trans, b-engine, b-trans"
        )
    })?;
    let request_data = args
        .next()
        .map(|raw| common::parse_data_bytes_arg(bin, extra_usage, "request-data", &raw))
        .unwrap_or_else(|| vec![0x33, 0x01]);

    println!(
        "config: protocol={PROTOCOL_NAME} sci_config={config_name} baud={FIXED_BAUD_RATE} \
         request={request_data:02x?}"
    );

    let (mut client, module_handle) =
        common::connect_and_open_module(&common_args.service_addr).await?;

    let cp_baudrate = common::comparam_object_id(&mut client, "CP_Baudrate").await?;

    // ── GetResourceIds(protocol="SAE_J2610_SCI", bus="SAE_J2610_UART", \
    //    dlc_pin_data=<config's own pins>) ──────────────────────────────
    let pin_data: Vec<PinData> = pins
        .iter()
        .map(|&(number, type_name)| PinData {
            dlc_pin_number: number,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName(type_name.to_string())),
        })
        .collect();
    let resource_ids = client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(module_handle),
            resource_data: Some(ResourceData {
                dlc_pin_data: pin_data,
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
            "no resource id resolved for protocol {PROTOCOL_NAME:?} config {config_name:?}"
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

    // ── SetComParam(CP_Baudrate) ─────────────────────────────────────────
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(cp_baudrate)),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(FIXED_BAUD_RATE)),
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

    println!("sending SCI request {request_data:02x?}");
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
            "WARNING: no response received -- expected if no SCI-capable ECU is wired to the \
             {config_name} pin pair on this vehicle"
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
    let bin = args.next().unwrap_or_else(|| "grpc_sci".to_string());
    if let Err(err) = run(&bin, args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
