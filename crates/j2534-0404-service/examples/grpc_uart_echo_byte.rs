//! Demonstrates SAE J2534-2 clause 12 UART Echo Byte Protocol access
//! (`ChannelProtocol::UART_ECHO_BYTE_PS`, native `UART_ECHO_BYTE_UART` bus,
//! ADR-170/Phase 9) through the `j2534-0404-service` gRPC interface:
//! resolves the `UART_ECHO_BYTE` resource, performs the protocol's mandatory
//! 5-baud-init address byte via `CoptStartcomm`, and drives a single
//! `CoptSendrecv`.
//!
//! This protocol backs Honda ABS/VSA diagnostics (SAE J2809) or KWP1281
//! (SAE J2818) depending on the target vehicle -- clause 12 itself defines
//! only the shared UART physical/init layer, not an application-layer
//! message format, so `request-data` below is a placeholder, not a real
//! diagnostic request.
//!
//! This is a **pure gRPC client** -- it connects to an already-running
//! `j2534-0404-service` process (see `crates/j2534-0404-service/docs/startup-spec.md`); it does not
//! start a server itself. To see a real response you need a real
//! J2534-compatible interface wired to a UART Echo Byte-capable ECU.
//!
//! # ComParam addressing notes
//!
//! Verified against `comparam_support.rs`'s `is_universal_param` short-circuit
//! (checked before every protocol-specific allowlist): this protocol accepts
//! `CP_Baudrate`/`CP_Loopback` only, per clause 12.3.4.1's closed parameter
//! list -- there is NO addressing ComParam of any kind (no `SetComParam`
//! target address, no `SetUniqueRespIdTable`). `comparam_defaults.rs`'s
//! `uart_echo_byte_uart` bus preset already seeds the fixed 9600 bps clause
//! 12.3.4.1 specifies; this example re-stages `CP_Baudrate` anyway purely to
//! demonstrate the mechanism.
//!
//! Addressing instead happens via the single-byte 5-baud init address this
//! protocol's `CoptStartcomm` mandates: verified against
//! `rpc_primitive.rs`'s `PROTOCOL_UART_ECHO_BYTE_PS` arm -- clause 12 defines
//! neither fast-init nor an init-less start, so `cop_data` on `CoptStartcomm`
//! MUST be exactly one byte (the init address); any other length is
//! rejected synchronously, before `cop_handle` allocation. Message payloads
//! belong in the following `CoptSendrecv`, not `CoptStartcomm`.
//! `cop_ctrl_data.num_receive_cycles` on that same `CoptStartcomm` call
//! selects whether the ECU's key bytes are delivered as `ResultData`
//! (`1`) or the init still runs but key bytes are suppressed (`0` or
//! absent) -- this example always requests delivery.
//!
//! # Usage
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_uart_echo_byte -- <service-addr> \
//!     [init-address] [request-data]
//! ```
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_uart_echo_byte -- http://127.0.0.1:60124
//! ```
//!
//! Defaults: `init-address=0x33` (a commonly-used K-line-family init byte;
//! not spec-mandated -- clause 12 leaves the address vehicle-specific),
//! `request-data=01,00,00,00` (arbitrary placeholder, see above -- padded to
//! 4 bytes, this protocol's own `PROTOCOL_UART_ECHO_BYTE_PS` TX minimum,
//! `protocol.rs::tx_message_size_range`; `build_tx_message` (`tx_header.rs`)
//! prepends no header for this protocol, so `request-data` IS the complete
//! wire frame).

#[path = "common/mod.rs"]
mod common;

use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, ExpectedResponseData, GetResourceIdsRequest, ParamItem,
    PduComPrimitiveStatus, PduParamClass, PinData, ResourceData, SetComParamRequest,
    StartComPrimitiveRequest, create_com_logical_link_request, event_item, param_item, pin_data,
    resource_data,
};

const PROTOCOL_NAME: &str = "UART_ECHO_BYTE";
const BUS_TYPE_NAME: &str = "UART_ECHO_BYTE_UART";
const FIXED_BAUD_RATE: u32 = 9_600;

/// Wait timeout for the `CoptStartcomm` 5-baud init, in place of
/// `common::DEFAULT_RESPONSE_TIMEOUT_MS` (2000ms). Transmitting the
/// single address byte at 5 baud alone takes roughly the entire default
/// window, before the synchronization/key-byte phase can even start; 6000ms
/// gives generous headroom over a realistic full init sequence.
const INIT_TIMEOUT_MS: u32 = 6000;

// `PROTOCOL_UART_ECHO_BYTE_PS`'s TX minimum (`protocol.rs::
// tx_message_size_range`). `build_tx_message` (`tx_header.rs`) prepends no
// header for this protocol, so `request-data` below IS the complete wire
// frame, and a shorter one is synchronously rejected by
// `resolve_send_recv_tx` before ever reaching the adapter.
const MIN_REQUEST_DATA_LEN: usize = 4;

async fn run(bin: &str, mut args: std::env::Args) -> common::Result<()> {
    let extra_usage = "[init-address] [request-data]";
    let common_args = common::parse_common_args(bin, &mut args, extra_usage);

    let init_address = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "init-address", &raw))
        .unwrap_or(0x33);
    if init_address > 0xFF {
        return Err(
            format!("init-address {init_address:#x} must fit in one byte (0x00-0xFF)").into(),
        );
    }
    let request_data = args
        .next()
        .map(|raw| common::parse_data_bytes_arg(bin, extra_usage, "request-data", &raw))
        .unwrap_or_else(|| vec![0x01, 0x00, 0x00, 0x00]);
    if request_data.len() < MIN_REQUEST_DATA_LEN {
        return Err(format!(
            "request-data {request_data:02x?} is only {} byte(s), but \
             PROTOCOL_UART_ECHO_BYTE_PS requires at least {MIN_REQUEST_DATA_LEN} -- this \
             protocol has no header this service prepends automatically, so request-data must \
             already be a complete, protocol-valid frame",
            request_data.len()
        )
        .into());
    }

    println!(
        "config: protocol={PROTOCOL_NAME} baud={FIXED_BAUD_RATE} init_address={init_address:#x} \
         request={request_data:02x?}"
    );

    let (mut client, module_handle) =
        common::connect_and_open_module(&common_args.service_addr).await?;

    let cp_baudrate = common::comparam_object_id(&mut client, "CP_Baudrate").await?;

    // ── GetResourceIds(protocol="UART_ECHO_BYTE", bus="UART_ECHO_BYTE_UART") ──
    let pins = vec![PinData {
        dlc_pin_number: 7,
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

    // ── CoptStartcomm (mandatory single-byte 5-baud init address) ───────
    println!("starting communication (init_address={init_address:#x})");
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: Some(b"startcomm".to_vec()),
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptStartcomm as i32,
            cop_data: vec![init_address as u8],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0, // the init address is not an ordinary "send"
                num_receive_cycles: 1, // deliver the ECU's key bytes as ResultData
                temp_param_update: 0,
                expected_response_array: vec![],
                tx_flag: None,
            }),
        })
        .await?;
    // `common::wait_for_send_recv_response` alone is not completion-aware
    // enough here: it returns `Some(bytes)` as soon as ANY `ResultData`
    // arrives, even if the primitive's own terminal status later reports
    // `Cancelled` or a tagged `ErrorData` follows -- the service emits the
    // key bytes as their own event before `comm_started` is set and
    // `Finished` is emitted, so a caller that only checks `Some`/`None`
    // can start `CoptSendrecv` on an unfinished or offline link. This
    // local predicate (mirroring `grpc_j1939.rs`'s `CoptSendrecv` wait)
    // tracks the key bytes AND the terminal status AND any async error
    // together, and only proceeds when the wait ended via an error-free
    // `Finished`.
    let mut key_bytes: Option<Vec<u8>> = None;
    let mut saw_error: Option<i32> = None;
    let mut terminal_status: Option<i32> = None;
    common::wait_for_event(&mut events, INIT_TIMEOUT_MS, |item| {
        if item.cop_tag.as_deref() != Some(b"startcomm") {
            return false;
        }
        match &item.data {
            Some(event_item::Data::ResultData(result)) => {
                key_bytes = Some(result.data_bytes.clone());
                false
            }
            Some(event_item::Data::ErrorData(err)) => {
                saw_error = Some(*err);
                false
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32
                    || *status == PduComPrimitiveStatus::PduCopstCancelled as i32 =>
            {
                terminal_status = Some(*status);
                true
            }
            _ => false,
        }
    })
    .await;
    if terminal_status == Some(PduComPrimitiveStatus::PduCopstFinished as i32)
        && saw_error.is_none()
    {
        println!(
            "ECU key bytes received: {:02x?}",
            key_bytes.unwrap_or_default()
        );
    } else if terminal_status == Some(PduComPrimitiveStatus::PduCopstCancelled as i32) {
        return Err(
            "5-baud initialization was Cancelled -- typically a hard channel failure -- \
             aborting before CoptSendrecv"
                .into(),
        );
    } else if let Some(err) = saw_error {
        return Err(format!(
            "5-baud initialization reported PduErrorEvent({err}) -- aborting before CoptSendrecv"
        )
        .into());
    } else {
        return Err(
            "5-baud initialization did not complete within the timeout -- aborting before \
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
            "WARNING: no response received -- expected if no UART Echo Byte-capable ECU answers \
             at init_address={init_address:#x} on this vehicle"
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
        .unwrap_or_else(|| "grpc_uart_echo_byte".to_string());
    if let Err(err) = run(&bin, args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
