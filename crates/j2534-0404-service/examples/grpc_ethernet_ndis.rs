//! Demonstrates SAE J2534-2 clause 24 Ethernet_NDIS access
//! (`ChannelProtocol::ETHERNET_NDIS`, native `IEEE_802_3` bus,
//! ADR-194/Phase 16) through the `j2534-0404-service` gRPC interface:
//! resolves the `ETHERNET_NDIS` resource, connects, and queries the
//! adapter's identity/status via `PDU_IOCTL_GET_NDIS_ADAPTER_INFO`.
//!
//! This protocol is structurally different from every other example in
//! this directory -- verified directly against `rpc_primitive.rs`: clause
//! 24 bars EVERY `ComPrimitive` type unconditionally (`PassThruReadMsgs`/
//! `WriteMsgs`/`StartPeriodicMsg`/`StartMsgFilter` are all rejected at the
//! native layer, clause 24.2.5.2-5.5), so there is no `CoptStartcomm`/
//! `CoptSendrecv`/`CoptStopcomm` flow to demonstrate at all -- calling
//! `StartComPrimitive` with ANY `cop_type` on an Ethernet_NDIS CLL fails
//! synchronously with `PDU_ERR_ID_NOT_SUPPORTED`. Clause 24 defines this
//! protocol as connect/disconnect plus one info IOCTL only, so that is what
//! this example demonstrates: `GetResourceIds` -> `CreateComLogicalLink` ->
//! `ConnectComLogicalLink` -> `PDU_IOCTL_GET_NDIS_ADAPTER_INFO` ->
//! teardown. Ethernet traffic itself never crosses this gRPC surface --
//! clause 24.2.5.5's own payload path is the host OS's NDIS/RNDIS network
//! stack, not `PassThruReadMsgs`/`WriteMsgs`.
//!
//! This is a **pure gRPC client** -- it connects to an already-running
//! `j2534-0404-service` process (see `crates/j2534-0404-service/docs/startup-spec.md`); it does not
//! start a server itself. To see real adapter info you need a real
//! J2534-compatible interface with an NDIS/RNDIS Ethernet adapter attached.
//!
//! # Pin option (NOT a ComParam-free connect flag)
//!
//! Verified directly against `rpc_link.rs`: which Ethernet Tx pin pair
//! connects (Table 103's Option 1 vs. Option 2) IS driven by a ComParam,
//! `CP_NdisPinOption` (`PARAM_NDIS_PIN_OPTION`), which `ConnectComLogicalLink`
//! then translates into the native `CONNECT_FLAG_NDIS_PINS_OPTION1`/`OPTION2`
//! bit -- the client never sets a raw connect flag directly. `0`
//! (unset/default, "auto") sets neither flag bit;`1`/`2` select Option 1/2
//! explicitly. `CP_NdisPinOption` is the ONLY ComParam this protocol's
//! allowlist accepts (`comparam_support.rs`).
//!
//! # Usage
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_ethernet_ndis -- <service-addr> \
//!     [pin-option]
//! ```
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_ethernet_ndis -- http://127.0.0.1:60124
//! ```
//!
//! Default: `pin-option=0` (auto -- neither `CONNECT_FLAG_NDIS_PINS_OPTION1`
//! nor `OPTION2`); pass `1` or `2` to select a specific pin option.

#[path = "common/mod.rs"]
mod common;

use vci_service_interface::{
    ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, GetObjectIdRequest,
    GetResourceIdsRequest, IoCtlRequest, ObjectType, ParamItem, PduParamClass, PinData,
    ResourceData, SetComParamRequest, create_com_logical_link_request, data_item, io_ctl_request,
    param_item, resource_data, vci_service_client::VciServiceClient,
};

const PROTOCOL_NAME: &str = "ETHERNET_NDIS";
const BUS_TYPE_NAME: &str = "IEEE_802_3";

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id`
/// via `GetObjectId(OBJT_IO_CTRL, ...)` -- a named IOCTL command cannot be
/// passed directly to `IoCtl` (`rpc_misc.rs` rejects
/// `io_ctrl_command_name` with `unimplemented`, ADR-079), so every caller
/// must resolve it first. Per-file-local helper, matching this workspace's
/// existing `tests/grpc_mock/*.rs` convention (not shared via
/// `examples/common/mod.rs`, since only this example needs it).
async fn resolve_ioctl_id(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    name: &str,
) -> common::Result<u32> {
    let resp = client
        .get_object_id(GetObjectIdRequest {
            object_type: ObjectType::ObjtIoCtrl as i32,
            shortname: name.to_string(),
        })
        .await
        .map_err(|e| format!("get_object_id({name}) failed: {e}"))?
        .into_inner();
    println!("{name} object_id = {:#x}", resp.pdu_object_id);
    Ok(resp.pdu_object_id)
}

/// Decodes the 226-byte hand-packed `NDIS_ADAPTER_INFORMATION` layout
/// `PDU_IOCTL_GET_NDIS_ADAPTER_INFO` returns (`rpc_misc.rs`'s
/// `pack_ndis_adapter_info` doc comment): `AdapterUniqueID` (128B),
/// `AdapterName` (64B), `Status` (4B LE u32), `MAC_Address` (6B),
/// `IPV6_Address` (16B), `IPV4_Address` (4B), `EthernetPinConfig` (4B LE
/// u32).
fn print_ndis_adapter_info(bytes: &[u8]) {
    if bytes.len() != 226 {
        println!(
            "  (unexpected NDIS_ADAPTER_INFORMATION length {}; expected 226)",
            bytes.len()
        );
        return;
    }
    let adapter_name = String::from_utf8_lossy(&bytes[128..192])
        .trim_end_matches('\0')
        .to_string();
    let status = u32::from_le_bytes(bytes[192..196].try_into().unwrap());
    let mac = &bytes[196..202];
    let ipv4 = &bytes[218..222];
    let pin_config = u32::from_le_bytes(bytes[222..226].try_into().unwrap());
    println!(
        "  adapter_name={adapter_name:?} status={status} mac={mac:02x?} ipv4={ipv4:?} \
         ethernet_pin_config={pin_config}"
    );
}

async fn run(bin: &str, mut args: std::env::Args) -> common::Result<()> {
    let extra_usage = "[pin-option: 0|1|2]";
    let common_args = common::parse_common_args(bin, &mut args, extra_usage);

    let pin_option = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "pin-option", &raw))
        .unwrap_or(0);
    if pin_option > 2 {
        return Err(format!("pin-option {pin_option} must be 0 (auto), 1, or 2").into());
    }

    println!("config: protocol={PROTOCOL_NAME} pin_option={pin_option}");

    let (mut client, module_handle) =
        common::connect_and_open_module(&common_args.service_addr).await?;

    let cp_ndis_pin_option = common::comparam_object_id(&mut client, "CP_NdisPinOption").await?;

    // ── GetResourceIds(protocol="ETHERNET_NDIS", bus="IEEE_802_3") ─────
    let resource_ids = client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(module_handle),
            resource_data: Some(ResourceData {
                dlc_pin_data: Vec::<PinData>::new(),
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

    // ── SetComParam(CP_NdisPinOption) -- unless auto (0), the default ──
    if pin_option != 0 {
        client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(param_item::Id::ParamId(cp_ndis_pin_option)),
                    com_param_class: PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(pin_option)),
                }),
            })
            .await?;
    }

    // ── ConnectComLogicalLink (no SubscribeEvent -- no COP-level API \
    //    surface exists for this protocol to report events about) ──────
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await?;
    println!("connected");

    // ── PDU_IOCTL_GET_NDIS_ADAPTER_INFO -- the one info IOCTL clause 24 \
    //    defines; no CoptStartcomm/CoptSendrecv/CoptStopcomm exists here ──
    let get_ndis_adapter_info_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_NDIS_ADAPTER_INFO").await?;
    let ioctl_response = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                get_ndis_adapter_info_id,
            )),
            input_data: None,
            has_output: true,
        })
        .await?
        .into_inner();
    match ioctl_response.output_data.and_then(|d| d.data) {
        Some(data_item::Data::BytearrayData(bytes)) => {
            println!("PDU_IOCTL_GET_NDIS_ADAPTER_INFO succeeded:");
            print_ndis_adapter_info(&bytes.data);
        }
        other => println!("WARNING: unexpected PDU_IOCTL_GET_NDIS_ADAPTER_INFO output: {other:?}"),
    }

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
        .unwrap_or_else(|| "grpc_ethernet_ndis".to_string());
    if let Err(err) = run(&bin, args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
