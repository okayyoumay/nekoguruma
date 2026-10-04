//! Demonstrates ISO 9141-2 (K-line) access through the `j2534-0404-service`
//! gRPC interface: resolves the `ISO_9141_2` resource (native `ISO9141`,
//! `ISO_9141_2_UART` bus, fixed 10.4 kbit/s per the K-line spec), performs
//! the protocol's wakeup/init sequence via `CoptStartcomm`, and drives a
//! single physically-addressed `CoptSendrecv`.
//!
//! This is a **pure gRPC client** -- it connects to an already-running
//! `j2534-0404-service` process (see `crates/j2534-0404-service/docs/startup-spec.md`); it does not
//! start a server itself. To see a real response you need a real
//! J2534-compatible interface wired to a K-line-capable ECU (older,
//! pre-CAN OBD-II vehicles, roughly 1996-2003 model years depending on
//! manufacturer).
//!
//! # ComParam addressing notes
//!
//! Unlike `grpc_can.rs`/`grpc_iso15765.rs`, this protocol's `SetComParam`
//! request-side addressing (`CP_PhysReqFormatPriorityType`/
//! `CP_PhysReqTargetAddr`, `CP_FuncReqFormatPriorityType`/
//! `CP_FuncReqTargetAddr`) is COM-class and set directly via
//! `SetComParam`, the same way `CP_Baudrate` is -- unlike
//! `SetUniqueRespIdTable`'s CAN-specific entries
//! (`CP_CanPhysReqId`/`CP_CanRespUSDTId`/`CP_CanRespUUDTId`), which are
//! `PDU_PC_UNIQUE_ID`-class only for the CAN protocol family (ADR-042). The
//! bare `ISO_9141_2` resource seeds no addressing/init defaults of its own
//! (only baud rate and UART framing -- see `comparam_defaults.rs`'s
//! `iso_9141_2_uart` bus preset); the format/target-addr/init-setting
//! values below are this example's own picks, informed by (but not
//! identical to) the `ISO_15031_5`-overlay presets' own K-line defaults
//! (`comparam_defaults.rs`'s `kwp_on_kline_common`).
//!
//! `CP_InitializationSettings` (ADR-075) selects the K-line wakeup
//! sequence: `1` = 5-baud init, `2` = fast init, `3` = skip init entirely
//! (link already initialized out-of-band).
//!
//! # Response matching
//!
//! K-line can still have more than one ECU sharing the bus, so this
//! example also calls `SetUniqueRespIdTable` with a single entry keyed on
//! the `PDU_PC_UNIQUE_ID`-class `CP_EcuRespSourceAddress` (the physically
//! addressed target ECU's own source address) and references it via
//! `unique_resp_ids` on the `CoptSendrecv`, so a response is only accepted
//! from that ECU rather than the first frame to arrive from any ECU.
//!
//! # Usage
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_iso9141 -- <service-addr> \
//!     [phys-target-addr] [func-target-addr] [init-settings] [request-data]
//! ```
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_iso9141 -- http://127.0.0.1:60124
//! ```
//!
//! Defaults: `phys-target-addr=0x10`, `func-target-addr=0x33`,
//! `init-settings=2` (fast init), `request-data=01,00` (legacy OBD-II Mode
//! 1 PID 0 request -- adjust for a manufacturer-specific K-line protocol).

#[path = "common/mod.rs"]
mod common;

use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, EcuUniqueRespData, ExpectedResponseData, GetResourceIdsRequest,
    ParamItem, PduParamClass, PinData, ResourceData, SetComParamRequest,
    SetUniqueRespIdTableRequest, StartComPrimitiveRequest, UniqueRespIdTableItem,
    create_com_logical_link_request, param_item, pin_data, resource_data,
};

const PROTOCOL_NAME: &str = "ISO_9141_2";
const BUS_TYPE_NAME: &str = "ISO_9141_2_UART";
const FIXED_BAUD_RATE: u32 = 10_400;

/// Wait timeout for the `CoptStartcomm` K-line init, in place of
/// `common::DEFAULT_RESPONSE_TIMEOUT_MS` (2000ms) -- mirrors
/// `grpc_uart_echo_byte.rs`'s own `INIT_TIMEOUT_MS` rationale. When
/// `init_settings` selects 5-baud initialization (`1`), the wakeup/init
/// sequence documented for this service can take roughly 2-5 seconds; 6000ms
/// gives generous headroom over that. Safe to use unconditionally even for
/// fast init (`init_settings=2`) or skipped init (`3`) since `wait_for_event`
/// returns as soon as the terminal event arrives -- a longer timeout never
/// makes a fast, successful init wait longer than it needs.
const INIT_TIMEOUT_MS: u32 = 6000;

async fn run(bin: &str, mut args: std::env::Args) -> common::Result<()> {
    let extra_usage = "[phys-target-addr] [func-target-addr] [init-settings] [request-data]";
    let common_args = common::parse_common_args(bin, &mut args, extra_usage);

    let phys_target_addr = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "phys-target-addr", &raw))
        .unwrap_or(0x10);
    let func_target_addr = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "func-target-addr", &raw))
        .unwrap_or(0x33);
    let init_settings = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "init-settings", &raw))
        .unwrap_or(2);
    let request_data = args
        .next()
        .map(|raw| common::parse_data_bytes_arg(bin, extra_usage, "request-data", &raw))
        .unwrap_or_else(|| vec![0x01, 0x00]);

    println!(
        "config: protocol={PROTOCOL_NAME} baud={FIXED_BAUD_RATE} \
         phys_target_addr={phys_target_addr:#x} func_target_addr={func_target_addr:#x} \
         init_settings={init_settings} request={request_data:02x?}"
    );

    let (mut client, module_handle) =
        common::connect_and_open_module(&common_args.service_addr).await?;

    let cp_baudrate = common::comparam_object_id(&mut client, "CP_Baudrate").await?;
    let cp_p2max = common::comparam_object_id(&mut client, "CP_P2Max").await?;
    let cp_request_addr_mode =
        common::comparam_object_id(&mut client, "CP_RequestAddrMode").await?;
    let cp_phys_req_target_addr =
        common::comparam_object_id(&mut client, "CP_PhysReqTargetAddr").await?;
    let cp_func_req_target_addr =
        common::comparam_object_id(&mut client, "CP_FuncReqTargetAddr").await?;
    let cp_init_settings =
        common::comparam_object_id(&mut client, "CP_InitializationSettings").await?;
    let cp_ecu_resp_source_address =
        common::comparam_object_id(&mut client, "CP_EcuRespSourceAddress").await?;

    // ── GetResourceIds(protocol="ISO_9141_2", bus="ISO_9141_2_UART") ────
    let pins = vec![
        PinData {
            dlc_pin_number: 7,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName("K".to_string())),
        },
        PinData {
            dlc_pin_number: 15,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName("L".to_string())),
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

    // ── SetComParam: baud, response window, addressing, init sequence ──────
    for (id, value) in [
        (cp_baudrate, FIXED_BAUD_RATE),
        (
            cp_p2max,
            common::DEFAULT_RESPONSE_TIMEOUT_MS.saturating_mul(1000),
        ),
        (cp_request_addr_mode, 1), // physical addressing
        (cp_phys_req_target_addr, phys_target_addr),
        (cp_func_req_target_addr, func_target_addr),
        (cp_init_settings, init_settings),
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

    // ── SetUniqueRespIdTable(CP_EcuRespSourceAddress) ───────────────────
    client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![EcuUniqueRespData {
                    unique_resp_identifier: 1,
                    params: vec![ParamItem {
                        id: Some(param_item::Id::ParamId(cp_ecu_resp_source_address)),
                        com_param_class: PduParamClass::PduPcUniqueId as i32,
                        param_data: Some(param_item::ParamData::Unum32(phys_target_addr)),
                    }],
                }],
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

    // ── CoptStartcomm (runs the K-line wakeup/init sequence) ────────────
    println!("starting communication (init_settings={init_settings})");
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: Some(b"startcomm".to_vec()),
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await?;
    if !common::wait_for_cop_finished(&mut events, INIT_TIMEOUT_MS, b"startcomm").await {
        return Err(
            "K-line initialization did not complete within the timeout -- aborting before \
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
                    unique_resp_ids: vec![1],
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
            "WARNING: no response received -- expected if no K-line ECU answers at \
             phys_target_addr={phys_target_addr:#x} on this vehicle"
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
    let bin = args.next().unwrap_or_else(|| "grpc_iso9141".to_string());
    if let Err(err) = run(&bin, args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
