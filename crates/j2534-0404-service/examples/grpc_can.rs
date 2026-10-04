//! Demonstrates raw CAN access through the `j2534-0404-service` gRPC
//! interface: resolves the `ISO_11898_RAW` resource (native `CAN`,
//! `ISO_11898_2_DWCAN` bus), addresses one ECU, and drives a single
//! `CoptSendrecv` with an unformatted (non-ISO-TP) CAN payload.
//!
//! This is a **pure gRPC client** -- it connects to an already-running
//! `j2534-0404-service` process (see `crates/j2534-0404-service/docs/startup-spec.md`); it does not
//! start a server itself. To see a real response you need a real
//! J2534-compatible interface connected to a live CAN bus (a vehicle or
//! bench harness) with an ECU listening at the configured physical request
//! ID.
//!
//! # ComParam addressing notes
//!
//! Raw CAN carries unformatted frames, not ISO 15765-2 (ISO-TP) segmented
//! ones, so the per-ECU response id in `SetUniqueRespIdTable` uses
//! `CP_CanRespUUDTId` ("unformatted"), NOT `CP_CanRespUSDTId` (the
//! ISO-TP-segmented id `grpc_iso15765.rs` uses) -- see
//! `docs/j2534-0404-architecture.md`'s CAN-family addressing discussion and
//! `resources.rs`'s raw-CAN vs. ISO15765 resource rows. `CP_CanPhysReqId`
//! and `CP_CanRespUUDTId` are `PDU_PC_UNIQUE_ID`-class only (ADR-042): they
//! can only be set through `SetUniqueRespIdTable`, never a plain
//! `SetComParam`. `CP_CanFuncReqId` is the opposite -- COM-class, settable
//! directly, since it's a broadcast id shared by the whole link rather than
//! a per-ECU one.
//!
//! # Usage
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_can -- <service-addr> \
//!     [phys-req-id] [resp-id] [func-req-id] [baud-rate] [request-data]
//! ```
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_can -- http://127.0.0.1:60124
//! ```
//!
//! Defaults: `phys-req-id=0x7E0`, `resp-id=0x7E8`, `func-req-id=0x7DF`,
//! `baud-rate=500000`, `request-data=01,00` (an unformatted OBD-II Mode 1
//! PID 0 request byte pair -- only ECUs that also accept unformatted,
//! non-ISO-TP-prefixed requests will answer this; most modern ECUs expect
//! the ISO-TP single-frame encoding `grpc_iso15765.rs` sends instead).
//!
//! `phys-req-id`, `resp-id`, and `func-req-id` are 11-bit-only
//! (`0x000`-`0x7FF`): this example never stages `CP_CanPhysReqFormat`/
//! `CP_CanRespUUDTFormat`/`CP_CanFuncReqFormat`, so it has no way to mark an
//! id as a 29-bit (extended) CAN identifier -- a value above `0x7FF` is
//! rejected rather than silently sent as an unmarked 11-bit id.

#[path = "common/mod.rs"]
mod common;

use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, EcuUniqueRespData, ExpectedResponseData, GetResourceIdsRequest,
    ParamItem, PduParamClass, PinData, ResourceData, SetComParamRequest,
    SetUniqueRespIdTableRequest, StartComPrimitiveRequest, UniqueRespIdTableItem,
    create_com_logical_link_request, param_item, pin_data, resource_data,
};

const PROTOCOL_NAME: &str = "ISO_11898_RAW";
const BUS_TYPE_NAME: &str = "ISO_11898_2_DWCAN";

async fn run(bin: &str, mut args: std::env::Args) -> common::Result<()> {
    let extra_usage = "[phys-req-id] [resp-id] [func-req-id] [baud-rate] [request-data]";
    let common_args = common::parse_common_args(bin, &mut args, extra_usage);

    let phys_req_id = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "phys-req-id", &raw))
        .unwrap_or(0x7E0);
    if phys_req_id > 0x7FF {
        return Err(format!(
            "phys-req-id {phys_req_id:#x} exceeds the 11-bit CAN ID range (0x000-0x7FF) -- this \
             example leaves CP_CanPhysReqFormat at its 11-bit default, so a 29-bit (extended) \
             identifier here would be sent unmarked and likely rejected or misinterpreted"
        )
        .into());
    }
    let resp_id = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "resp-id", &raw))
        .unwrap_or(0x7E8);
    if resp_id > 0x7FF {
        return Err(format!(
            "resp-id {resp_id:#x} exceeds the 11-bit CAN ID range (0x000-0x7FF) -- this example \
             leaves CP_CanRespUUDTFormat at its 11-bit default, so a 29-bit (extended) \
             identifier here would be sent unmarked and likely rejected or misinterpreted"
        )
        .into());
    }
    let func_req_id = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "func-req-id", &raw))
        .unwrap_or(0x7DF);
    if func_req_id > 0x7FF {
        return Err(format!(
            "func-req-id {func_req_id:#x} exceeds the 11-bit CAN ID range (0x000-0x7FF) -- this \
             example leaves CP_CanFuncReqFormat at its 11-bit default, so a 29-bit (extended) \
             identifier here would be sent unmarked and likely rejected or misinterpreted"
        )
        .into());
    }
    let baud_rate = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "baud-rate", &raw))
        .unwrap_or(500_000);
    let request_data = args
        .next()
        .map(|raw| common::parse_data_bytes_arg(bin, extra_usage, "request-data", &raw))
        .unwrap_or_else(|| vec![0x01, 0x00]);

    println!(
        "config: protocol={PROTOCOL_NAME} baud={baud_rate} phys_req_id={phys_req_id:#x} \
         resp_id={resp_id:#x} func_req_id={func_req_id:#x} request={request_data:02x?}"
    );

    let (mut client, module_handle) =
        common::connect_and_open_module(&common_args.service_addr).await?;

    let cp_baudrate = common::comparam_object_id(&mut client, "CP_Baudrate").await?;
    let cp_p2max = common::comparam_object_id(&mut client, "CP_P2Max").await?;
    let cp_can_phys_req_id = common::comparam_object_id(&mut client, "CP_CanPhysReqId").await?;
    let cp_can_resp_uudt_id = common::comparam_object_id(&mut client, "CP_CanRespUUDTId").await?;
    let cp_can_func_req_id = common::comparam_object_id(&mut client, "CP_CanFuncReqId").await?;

    // ── GetResourceIds(protocol="ISO_11898_RAW", bus="ISO_11898_2_DWCAN") ──
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

    // ── SetComParam(CP_Baudrate, CP_P2Max, CP_CanFuncReqId) ────────────────
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
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(cp_p2max)),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(
                    common::DEFAULT_RESPONSE_TIMEOUT_MS.saturating_mul(1000),
                )),
            }),
        })
        .await?;
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(cp_can_func_req_id)),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(func_req_id)),
            }),
        })
        .await?;

    // ── SetUniqueRespIdTable(CP_CanPhysReqId, CP_CanRespUUDTId) ─────────────
    client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![EcuUniqueRespData {
                    unique_resp_identifier: 1,
                    params: vec![
                        ParamItem {
                            id: Some(param_item::Id::ParamId(cp_can_phys_req_id)),
                            com_param_class: PduParamClass::PduPcUniqueId as i32,
                            param_data: Some(param_item::ParamData::Unum32(phys_req_id)),
                        },
                        ParamItem {
                            id: Some(param_item::Id::ParamId(cp_can_resp_uudt_id)),
                            com_param_class: PduParamClass::PduPcUniqueId as i32,
                            param_data: Some(param_item::ParamData::Unum32(resp_id)),
                        },
                    ],
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
            "WARNING: no response received -- expected if no ECU on this bus answers \
             unformatted requests at phys_req_id={phys_req_id:#x}/resp_id={resp_id:#x}"
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
    let bin = args.next().unwrap_or_else(|| "grpc_can".to_string());
    if let Err(err) = run(&bin, args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
