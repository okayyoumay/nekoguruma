//! Manual, opt-in integration test that drives the full ISO 22900-2 gRPC
//! surface of `j2534-0404-service` against a **real** J2534 device and a
//! real vehicle, following the "Typical gRPC Client Flow" documented in
//! `docs/j2534-0404-architecture.md` §11:
//!
//! `GetModuleIds` → `ModuleConnect` → `GetVersion` → `GetObjectId` (ComParam
//! shortname → MDF id resolution) → `GetResourceIds` →
//! `CreateComLogicalLink` → `SetComParam` (baud rate) →
//! `SetUniqueRespIdTable` (ECU addressing) → `ConnectComLogicalLink` →
//! `SubscribeEvent` → `StartComPrimitive` (`CoptStartcomm` → `CoptSendrecv`
//! functionally addressed, then `CoptSendrecv` physically addressed →
//! `CoptStopcomm`) → `DisconnectComLogicalLink` →
//! `DestroyComLogicalLink` → `ModuleDisconnect`.
//!
//! # This test is not part of the automated suite
//!
//! It is skipped unless `J2534_LIVE_SERVICE_TEST=1` is set, and further
//! requires selecting a connection target at runtime via one of:
//!
//! - `J2534_DLL_PATH` — a filesystem path to a real vendor J2534 DLL/`.so`.
//!   The test writes a temporary `config.toml` `library_path` override
//!   pointing at it (the same mechanism `grpc_mock.rs` uses to redirect
//!   resolution at the compiled mock) and starts the service against that.
//!   Debug builds only: the redirect relies on the runtime
//!   `VCI_CONFIG_PATH` override, which is compiled out of release builds
//!   (see ADR-073).
//! - `J2534_LIVE_LIBRARY_NAME` — a library name already resolvable through
//!   the platform's normal discovery (the Windows registry, or an existing
//!   deployed `config.toml`), used as-is with no override. Ignored if
//!   `J2534_DLL_PATH` is also set.
//!
//! If neither is set the test is skipped even when the opt-in flag is on.
//!
//! # Scope
//!
//! Primarily targets CAN-family protocols (`CAN`, `ISO15765`, or any
//! `ISO_..._ON_ISO_15765_*` short name) — `SetUniqueRespIdTable` addressing
//! below uses `CP_CanFuncReqId`/`CP_CanPhysReqId`/`CP_CanRespUSDTId`, which
//! are `PDU_PC_UNIQUE_ID`-class only for the CAN protocol family (ADR-042).
//! K-line (ISO9141/ISO14230) is also driven, with reduced coverage: each
//! `SetComParam` call that uses a CAN-class ComParam falls back to the
//! equivalent KWP-class one (`CP_FuncReqTargetAddr`, `CP_PhysReqTargetAddr` —
//! see ADR-050/ADR-054) when the CAN-class call is rejected. `CP_CanRespUSDTId`
//! has no KWP fallback here: `SetUniqueRespIdTable` simply omits any
//! K-line-specific entry on a `CP_CanRespUSDTId` rejection, leaving the
//! table **empty** (no-table "deliver unconditionally" mode) rather than
//! installing a `CP_EcuRespSourceAddress`-keyed entry — a simplification,
//! not a required workaround: as of ADR-203, `events_rx_routing.rs::route_frame`
//! DOES have a matching tier for `CP_EcuRespSourceAddress` on KWP
//! (ISO9141/ISO14230) and J1850 (VPW/PWM), so a K-line entry keyed on it
//! would now route correctly (and, per that ADR, restricts delivery to a
//! matching source address rather than wildcarding); this harness still
//! does not bother installing one, since the empty-table wildcard already
//! exercises the flow end-to-end without needing to know the ECU's real
//! K-line source address up front. TX addressing is unaffected (it falls
//! back to `CP_PhysReqTargetAddr` when no table entry sets
//! `CP_EcuRespSourceAddress`). SCI still uses different addressing
//! ComParams and is not covered here.
//!
//! # Example invocation
//!
//! ```sh
//! J2534_LIVE_SERVICE_TEST=1 \
//! J2534_DLL_PATH="C:\Program Files\Drew Technologies\J2534\op20pt32.dll" \
//! cargo test -p j2534-0404-service --test live_grpc_flow -- --nocapture
//! ```
//!
//! Defaults target ECU #1 on UDS-on-CAN (`ISO_14229_3_on_ISO_15765_2`) with a
//! legislated OBD-II functional broadcast request (`0x7DF`, Mode 1 PID 0
//! "supported PIDs `[01-20]`") followed by a physically addressed UDS
//! `ReadDataByIdentifier` request for DID `0xF190` (VIN, request ID `0x7E0`,
//! response ID `0x7E8`) — the functional half runs unmodified against nearly
//! any OBD-II-legislated vehicle, but the physical half requires UDS support
//! on the target ECU. Override via the `J2534_LIVE_*` variables below to
//! target a specific ECU, baud rate, or UDS/OBD request:
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `J2534_LIVE_PROTOCOL_NAME` | `ISO_14229_3_on_ISO_15765_2` | Protocol name passed to `GetResourceIds` |
//! | `J2534_LIVE_BUS_TYPE_NAME` | `ISO_11898_2_DWCAN` | Bus type name passed to `GetResourceIds` (e.g. `ISO_14230_1_UART` for K-line) |
//! | `J2534_LIVE_PIN_1_NAME` / `J2534_LIVE_PIN_1_NUMBER` | `HI` / `6` | First DLC pin (name/number) passed to `GetResourceIds`; number `0` omits the pin |
//! | `J2534_LIVE_PIN_2_NAME` / `J2534_LIVE_PIN_2_NUMBER` | `LOW` / `14` | Second DLC pin (name/number); number `0` omits the pin |
//! | `J2534_LIVE_BAUD_RATE` | `500000` | `CP_Baudrate` |
//! | `J2534_LIVE_PHYS_REQ_ID` | `0x7E0` | `CP_CanPhysReqId` / `CP_PhysReqTargetAddr` (tester's physical request id) |
//! | `J2534_LIVE_RESP_ID` | `0x7E8` | `CP_CanRespUSDTId` / `CP_EcuRespSourceAddress` (ECU's response id) |
//! | `J2534_LIVE_FUNC_REQ_ID` | `0x7DF` | `CP_CanFuncReqId` / `CP_FuncReqTargetAddr` (tester's functional request id) |
//! | `J2534_LIVE_REQUEST_DATA` | `22,F1,90` | `CoptSendrecv` payload, comma-separated hex bytes |
//! | `J2534_LIVE_FUNC_REQUEST_DATA` | `01,00` | functional `CoptSendrecv` payload, comma-separated hex bytes |
//! | `J2534_LIVE_INIT_DATA` | (empty) | `CoptStartcomm` init payload; only meaningful for K-line protocols |
//! | `J2534_LIVE_INIT_SETTING` | `2` | `CP_InitializationSettings` (K-line init sequence selection, ADR-074) |
//! | `J2534_LIVE_RESPONSE_TIMEOUT_MS` | `2000` | Response window (sets `CP_P2Max`, ADR-053) and client-side event wait |

use std::time::Duration;

use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest,
    EcuUniqueRespData, EventNotification, ExpectedResponseData, GetModuleIdsRequest,
    GetObjectIdRequest, GetResourceIdsRequest, GetVersionRequest, ModuleConnectRequest,
    ModuleDisconnectRequest, ModuleHandle, ObjectType, ParamItem, PduParamClass, ResourceData,
    SetComParamRequest, SetUniqueRespIdTableRequest, StartComPrimitiveRequest,
    SubscribeEventRequest, UniqueRespIdTableItem, create_com_logical_link_request, event_item,
    event_notification, param_item, resource_data, subscribe_event_request,
    vci_service_client::VciServiceClient, vci_service_server::VciServiceServer,
};
use vci_service_launcher::vci_server::VciServer;

use j2534_0404_service::service::J2534Service;

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn parse_u32_env(name: &str, default: u32) -> u32 {
    let Ok(raw) = std::env::var(name) else {
        return default;
    };
    let value = raw.trim();
    let parsed = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => value.parse::<u32>(),
    };
    parsed.unwrap_or_else(|err| {
        panic!("{name} must be a valid u32 (decimal or 0x-prefixed hex): {err}")
    })
}

fn parse_data_bytes_env(name: &str, default: &[u8]) -> Vec<u8> {
    let Ok(raw) = std::env::var(name) else {
        return default.to_vec();
    };
    let value = raw.trim();
    if value.is_empty() {
        return default.to_vec();
    }
    value
        .split(',')
        .map(|part| {
            let token = part.trim();
            let token = token
                .strip_prefix("0x")
                .or_else(|| token.strip_prefix("0X"))
                .unwrap_or(token);
            u8::from_str_radix(token, 16)
                .unwrap_or_else(|err| panic!("invalid byte '{part}' in {name}: {err}"))
        })
        .collect()
}

/// Resolve a ComParam shortname to its `pdu_object_id` via `GetObjectId`,
/// as a real D-PDU client would instead of hardcoding MDF IDs.
async fn comparam_object_id(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    shortname: &str,
) -> u32 {
    let resp = client
        .get_object_id(GetObjectIdRequest {
            object_type: ObjectType::ObjtComparam as i32,
            shortname: shortname.to_string(),
        })
        .await
        .unwrap_or_else(|e| panic!("get_object_id({shortname}) should succeed: {e}"))
        .into_inner();
    println!("{shortname} object_id = {:#x}", resp.pdu_object_id);
    resp.pdu_object_id
}

/// Resolves the connection target from the environment into a `library_name`
/// for the service startup argument. Returns `None` when nothing is
/// configured (the caller should skip the test in that case).
fn resolve_library_name() -> Option<String> {
    // The J2534_DLL_PATH mode relies on the debug-only runtime
    // VCI_CONFIG_PATH override (see ADR-073), so it only exists in debug
    // builds. The J2534_LIVE_LIBRARY_NAME mode below resolves through the
    // registry or an already deployed config.toml and works in both
    // profiles.
    #[cfg(debug_assertions)]
    if let Ok(dll_path) = std::env::var("J2534_DLL_PATH") {
        // Per-process-unique path (ADR-195): "runs alone in its own process"
        // below only rules out concurrent threads, not another unrelated
        // process also touching a fixed path.
        let config_path = std::env::temp_dir().join(format!(
            "j2534-0404-service-live-test-config-{}.toml",
            std::process::id()
        ));
        let config_toml =
            format!("[config.apis.j2534-0404.libs.live-target]\nlibrary_path = {dll_path:?}\n",);
        std::fs::write(&config_path, &config_toml)
            .unwrap_or_else(|err| panic!("{} should be writable: {err}", config_path.display()));
        // SAFETY: this test runs alone in its own process (a distinct `cargo
        // test` integration-test binary), so no other thread reads env vars
        // concurrently with this write.
        unsafe {
            std::env::set_var("VCI_CONFIG_PATH", &config_path);
        }
        println!("target: J2534_DLL_PATH={dll_path:?} (via temporary config.toml override)");
        return Some("live-target".to_string());
    }
    // Fail loudly rather than silently falling through to the name mode (or
    // skipping) when the caller asked for a mode this profile cannot honor.
    #[cfg(not(debug_assertions))]
    if std::env::var("J2534_DLL_PATH").is_ok() {
        panic!(
            "J2534_DLL_PATH requires a debug build: its temporary config.toml \
             redirect uses the runtime VCI_CONFIG_PATH override, which release \
             builds compile out (ADR-073); use J2534_LIVE_LIBRARY_NAME against \
             the registry or a deployed config.toml instead"
        );
    }
    if let Ok(name) = std::env::var("J2534_LIVE_LIBRARY_NAME") {
        println!(
            "target: J2534_LIVE_LIBRARY_NAME={name:?} (resolved via registry/existing config.toml)"
        );
        return Some(name);
    }
    None
}

fn print_event_item(item: &vci_service_interface::EventItem) {
    match &item.data {
        Some(event_item::Data::ResultData(result)) => println!(
            "  event: ResultData data={:02x?} unique_resp_id={}",
            result.data_bytes, result.unique_resp_identifier
        ),
        Some(event_item::Data::CllStatus(status)) => println!("  event: CllStatus({status})"),
        Some(event_item::Data::CopStatus(status)) => println!("  event: CopStatus({status})"),
        Some(event_item::Data::ErrorData(err)) => println!("  event: ErrorData({err})"),
        Some(event_item::Data::ModuleStatus(status)) => println!("  event: ModuleStatus({status})"),
        Some(event_item::Data::InfoData(info)) => println!("  event: InfoData({info})"),
        None => println!("  event: (empty)"),
    }
}

/// Polls `events` until `predicate` returns `true` for an `EventItem`, or
/// `timeout_ms` elapses (returns `false` on timeout, stream end, or a
/// stream error). Every item seen along the way is printed — this is a
/// manual/diagnostic test, so visibility into the whole event sequence
/// matters more than terseness.
async fn wait_for_event<F>(
    events: &mut tonic::Streaming<EventNotification>,
    timeout_ms: u32,
    mut predicate: F,
) -> bool
where
    F: FnMut(&vci_service_interface::EventItem) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let Ok(poll_result) = tokio::time::timeout(remaining, events.message()).await else {
            return false; // timed out waiting for the next message
        };
        let Ok(Some(notification)) = poll_result else {
            return false; // stream ended or errored
        };
        if let Some(event_notification::EventData::Item(item)) = notification.event_data {
            print_event_item(&item);
            if predicate(&item) {
                return true;
            }
        }
    }
}

/// Drives `CoptSendrecv` to completion via the event stream: collects the
/// last `ResultData` seen before the terminal `CopStatus` (Finished or
/// Cancelled), or `None` if nothing arrived before timeout.
async fn wait_for_send_recv_response(
    events: &mut tonic::Streaming<EventNotification>,
    timeout_ms: u32,
) -> Option<Vec<u8>> {
    let mut response: Option<Vec<u8>> = None;
    let finished = wait_for_event(events, timeout_ms, |item| match &item.data {
        Some(event_item::Data::ResultData(result)) => {
            response = Some(result.data_bytes.clone());
            false
        }
        Some(event_item::Data::CopStatus(status)) => {
            *status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
                || *status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        }
        _ => false,
    })
    .await;
    if !finished {
        println!("  (timed out waiting for CoptSendrecv completion)");
    }
    response
}

#[tokio::test]
#[serial_test::serial]
async fn manual_typical_grpc_flow_against_real_device() {
    if !env_flag("J2534_LIVE_SERVICE_TEST") {
        eprintln!("skipping: set J2534_LIVE_SERVICE_TEST=1 to run this manual test");
        return;
    }
    let Some(library_name) = resolve_library_name() else {
        eprintln!(
            "skipping: set J2534_DLL_PATH (a vendor DLL/.so path) or \
             J2534_LIVE_LIBRARY_NAME (an already-registered library name) \
             to select the connection target"
        );
        return;
    };

    let protocol_name = env_string("J2534_LIVE_PROTOCOL_NAME", "ISO_14229_3_on_ISO_15765_2"); // UDS on CAN
    let bus_type_name = env_string("J2534_LIVE_BUS_TYPE_NAME", "ISO_11898_2_DWCAN");
    let pin_1_name = env_string("J2534_LIVE_PIN_1_NAME", "HI");
    let pin_1_number = parse_u32_env("J2534_LIVE_PIN_1_NUMBER", 6);
    let pin_2_name = env_string("J2534_LIVE_PIN_2_NAME", "LOW");
    let pin_2_number = parse_u32_env("J2534_LIVE_PIN_2_NUMBER", 14);
    let baud_rate = parse_u32_env("J2534_LIVE_BAUD_RATE", 500_000);
    let func_req_id = parse_u32_env("J2534_LIVE_FUNC_REQ_ID", 0x7DF);
    let phys_req_id = parse_u32_env("J2534_LIVE_PHYS_REQ_ID", 0x7E0);
    let resp_id = parse_u32_env("J2534_LIVE_RESP_ID", 0x7E8);
    let func_request_data = parse_data_bytes_env("J2534_LIVE_FUNC_REQUEST_DATA", &[0x01, 0x00]);
    let request_data = parse_data_bytes_env("J2534_LIVE_REQUEST_DATA", &[0x22, 0xF1, 0x90]);
    let init_data = parse_data_bytes_env("J2534_LIVE_INIT_DATA", &[]);
    let init_setting = parse_u32_env("J2534_LIVE_INIT_SETTING", 2);
    let response_timeout_ms = parse_u32_env("J2534_LIVE_RESPONSE_TIMEOUT_MS", 2000);
    let pins = [(pin_1_name, pin_1_number), (pin_2_name, pin_2_number)]
        .iter()
        .filter(|(_, number)| *number != 0)
        .map(|(name, number)| vci_service_interface::PinData {
            dlc_pin_number: *number,
            dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                name.to_string(),
            )),
        })
        .collect::<Vec<_>>();

    println!(
        "config: protocol={protocol_name} baud={baud_rate} phys_req_id={phys_req_id:#x} \
         resp_id={resp_id:#x} request={request_data:02x?} timeout={response_timeout_ms}ms"
    );

    // ── Start the real service, backed by the resolved target library ──────
    let (service_shutdown_tx, service_shutdown_rx) = tokio::sync::watch::channel(false);
    let startup_arg = format!("j2534-0404:{library_name}");
    let service = J2534Service::new(
        J2534Service::get_startup_config(["j2534-0404-service", &startup_arg])
            .expect("startup config should parse"),
        service_shutdown_rx,
    )
    .await
    .unwrap_or_else(|err| panic!("service should initialize against {library_name:?}: {err}"));

    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("listener should bind");
    let port = listener
        .local_addr()
        .expect("listener should have a port")
        .port();
    let incoming = TcpListenerStream::new(listener);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .add_service(VciServiceServer::new(service))
            .serve_with_incoming_shutdown(incoming, async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let mut client = VciServiceClient::connect(format!("http://127.0.0.1:{port}"))
        .await
        .expect("client should connect to the local service");

    // ── GetModuleIds → ModuleConnect → GetVersion ──────────────────────────
    let module_ids = client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("get_module_ids should succeed")
        .into_inner();
    let module_handle: ModuleHandle = module_ids
        .module_id_list
        .and_then(|list| list.module_data.into_iter().next())
        .and_then(|data| data.module_handle)
        .expect("at least one module should be reported");
    println!("module_handle = {}", module_handle.module_handle);

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("module_connect should succeed — is the device plugged in and free?");

    let version = client
        .get_version(GetVersionRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("get_version should succeed")
        .into_inner();
    if let Some(v) = version.version_data {
        println!(
            "device: firmware={:?} dll={:?} api_sw={:?} (parsed api_sw_version={})",
            v.hw_name, v.fw_name, v.pdu_api_sw_name, v.pdu_api_sw_version
        );
    }

    let cp_baudrate = comparam_object_id(&mut client, "CP_Baudrate").await;
    let cp_p2max = comparam_object_id(&mut client, "CP_P2Max").await;
    let cp_can_phys_req_id = comparam_object_id(&mut client, "CP_CanPhysReqId").await;
    let cp_can_resp_usdt_id = comparam_object_id(&mut client, "CP_CanRespUSDTId").await;
    let cp_can_func_req_id = comparam_object_id(&mut client, "CP_CanFuncReqId").await;
    let cp_request_addr_mode = comparam_object_id(&mut client, "CP_RequestAddrMode").await;
    let cp_func_req_target_addr = comparam_object_id(&mut client, "CP_FuncReqTargetAddr").await;
    let cp_phys_req_target_addr = comparam_object_id(&mut client, "CP_PhysReqTargetAddr").await;
    let cp_initial_setting = comparam_object_id(&mut client, "CP_InitializationSettings").await;

    // ── GetResourceIds(protocol name) ──────────────────────────────────────
    let resource_ids = client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(module_handle),
            resource_data: Some(ResourceData {
                dlc_pin_data: pins.clone(),
                bus_type: Some(resource_data::BusType::BusTypeName(bus_type_name.clone())),
                protocol: Some(resource_data::Protocol::ProtocolName(protocol_name.clone())),
            }),
        })
        .await
        .expect("get_resource_ids should succeed")
        .into_inner();
    let resource_id = resource_ids
        .resource_id_list
        .and_then(|list| list.resource_id_data_array.into_iter().next())
        .and_then(|data| data.resource_id_array.into_iter().next())
        .unwrap_or_else(|| panic!("no resource id resolved for protocol {protocol_name:?}"));
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
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner();
    let cll_handle: ComLogicalLinkHandle = cll_response
        .cll_handle
        .expect("cll_handle should be present");
    println!("cll_handle = {}", cll_handle.cll_handle);

    // ── SetComParam(CP_CanFuncReqId) ──────────────────────────────────────
    match client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    cp_can_func_req_id,
                )),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(func_req_id)),
            }),
        })
        .await
    {
        Ok(_) => println!("set_com_param(CP_CanFuncReqId) succeeded"),
        Err(e) => {
            eprintln!("WARNING: set_com_param(CP_CanFuncReqId) failed: {e}");
            client
                .set_com_param(SetComParamRequest {
                    cll_handle: Some(cll_handle),
                    param_item: Some(ParamItem {
                        id: Some(vci_service_interface::param_item::Id::ParamId(
                            cp_func_req_target_addr,
                        )),
                        com_param_class: PduParamClass::PduPcCom as i32,
                        param_data: Some(param_item::ParamData::Unum32(func_req_id)),
                    }),
                })
                .await
                .expect("set_com_param(CP_FuncReqTargetAddr) should succeed");
        }
    }
    match client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    cp_phys_req_target_addr,
                )),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(phys_req_id)),
            }),
        })
        .await
    {
        Ok(_) => println!("set_com_param(CP_PhysReqTargetAddr) succeeded"),
        Err(e) => {
            eprintln!("WARNING: set_com_param(CP_PhysReqTargetAddr) failed: {e}");
        }
    }

    // ── SetComParam(CP_InitializationSettings) ─────────────────────────────
    match client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    cp_initial_setting,
                )),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(init_setting)),
            }),
        })
        .await
    {
        Ok(_) => println!("set_com_param(CP_InitializationSettings) succeeded"),
        Err(e) => {
            eprintln!("WARNING: set_com_param(CP_InitializationSettings) failed: {e}");
        }
    }
    // ── SetComParam(CP_Baudrate) ─────────────────────────────────────────
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(cp_baudrate)),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(baud_rate)),
            }),
        })
        .await
        .expect("set_com_param(CP_Baudrate) should succeed");

    // ── SetComParam(CP_P2Max): the CoptSendrecv response window (ADR-053),
    // in µs — sized from J2534_LIVE_RESPONSE_TIMEOUT_MS so a slow ECU still
    // fits (PDU_COP_CTRL_DATA.Time is the cyclic-send cycle time, not the
    // response timeout) ─────────────────────────────────────────────────────
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(cp_p2max)),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(
                    response_timeout_ms.saturating_mul(1000),
                )),
            }),
        })
        .await
        .expect("set_com_param(CP_P2Max) should succeed");

    // ── SetUniqueRespIdTable(ECU addressing) — before Connect, so it takes
    // effect immediately at connect time with no pass-all fallback (ADR-048) ─
    match client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![EcuUniqueRespData {
                    unique_resp_identifier: 1,
                    params: vec![
                        ParamItem {
                            id: Some(vci_service_interface::param_item::Id::ParamId(
                                cp_can_phys_req_id,
                            )),
                            com_param_class: PduParamClass::PduPcUniqueId as i32,
                            param_data: Some(param_item::ParamData::Unum32(phys_req_id)),
                        },
                        ParamItem {
                            id: Some(vci_service_interface::param_item::Id::ParamId(
                                cp_can_resp_usdt_id,
                            )),
                            com_param_class: PduParamClass::PduPcUniqueId as i32,
                            param_data: Some(param_item::ParamData::Unum32(resp_id)),
                        },
                    ],
                }],
            }),
        })
        .await
    {
        Ok(_) => println!("set_unique_resp_id_table succeeded"),
        Err(e) => {
            // On K-line, `CP_CanRespUSDTId` is rejected (it's a CAN-only
            // PDU_PC_UNIQUE_ID param, ADR-042). Deliberately leave the
            // table empty here rather than installing a K-line entry keyed
            // only on `CP_EcuRespSourceAddress`: as of ADR-203, RX routing
            // (`events_rx_routing.rs::route_frame`) DOES have a matching
            // tier for `CP_EcuRespSourceAddress` on KWP/J1850, so such an
            // entry would now route correctly rather than silently
            // dropping every response frame the way it used to before that
            // ADR -- but it would also RESTRICT delivery to only a frame
            // whose source address matches, which this harness has no
            // reliable value to configure up front. An empty table keeps
            // RX in its no-table "deliver unconditionally" mode instead,
            // the simpler choice for a generic smoke test; TX addressing
            // still resolves correctly since it falls back to
            // `CP_PhysReqTargetAddr` (already set above) when no
            // UniqueRespIdTable entry provides `CP_EcuRespSourceAddress`.
            eprintln!(
                "WARNING: set_unique_resp_id_table failed: {e} \
                     (leaving table empty — no-table wildcard delivery, \
                     not because K-line RX routing lacks a \
                     CP_EcuRespSourceAddress tier; see ADR-203)"
            );
        }
    }

    // ── ConnectComLogicalLink ────────────────────────────────────────────
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    println!("connected");

    // ── SubscribeEvent ───────────────────────────────────────────────────
    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // ── CoptStartcomm ────────────────────────────────────────────────────
    println!("starting communication with init_data={init_data:02x?}");
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: init_data,
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    wait_for_event(&mut events, response_timeout_ms, |item| {
        matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        )
    })
    .await;

    // ── SetComParam(CP_RequestAddrMode) ─────────────────────────────────────
    println!("setting CP_RequestAddrMode=2 (functional addressing)");
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    cp_request_addr_mode,
                )),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(2)),
            }),
        })
        .await
        .expect("set_com_param(CP_RequestAddrMode) should succeed");

    // ── CoptSendrecv (functional request) ─────────────────────────────────
    println!("sending functional request with temp_param_update=1: {func_request_data:02x?}");
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: func_request_data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: -2,
                temp_param_update: 1,
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
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    match wait_for_send_recv_response(&mut events, response_timeout_ms + 500).await {
        Some(bytes) => println!("ECU response received: {bytes:02x?}"),
        None => println!(
            "WARNING: no response received within {response_timeout_ms} ms \
             (this can be expected if the configured ECU/address is not present \
             on this vehicle — check J2534_LIVE_PHYS_REQ_ID / J2534_LIVE_RESP_ID)"
        ),
    }

    // ── CoptSendrecv ─────────────────────────────────────────────────────
    println!("sending request {request_data:02x?}");
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: request_data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                // Time is the cyclic-send cycle time (ADR-053), unused for a
                // single send cycle; the response window is CP_P2Max, set
                // above from J2534_LIVE_RESPONSE_TIMEOUT_MS.
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    acceptance_id: 0,
                    response_type: 0,
                    // mask 0x00 on the first payload byte = wildcard: any
                    // response from the configured ECU finishes the COP, so
                    // the request payload stays freely overridable via
                    // J2534_LIVE_REQUEST_DATA.
                    mask_data: vec![0x00],
                    pattern_data: vec![0x00],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    match wait_for_send_recv_response(&mut events, response_timeout_ms + 500).await {
        Some(bytes) => println!("ECU response received: {bytes:02x?}"),
        None => println!(
            "WARNING: no response received within {response_timeout_ms} ms \
             (this can be expected if the configured ECU/address is not present \
             on this vehicle — check J2534_LIVE_PHYS_REQ_ID / J2534_LIVE_RESP_ID)"
        ),
    }

    // ── CoptStopcomm ─────────────────────────────────────────────────────
    println!("stopping communication");
    match client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
    {
        Ok(_) => println!("start_com_primitive(CoptStopcomm) succeeded"),
        Err(e) => {
            eprintln!("WARNING: start_com_primitive(CoptStopcomm) failed: {e}");
        }
    }

    wait_for_event(&mut events, response_timeout_ms, |item| {
        matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        )
    })
    .await;

    // ── Teardown: Disconnect → Destroy → ModuleDisconnect ───────────────
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");
    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("destroy_com_logical_link should succeed");
    client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("module_disconnect should succeed");

    println!("flow completed successfully");

    let _ = shutdown_tx.send(());
    let _ = service_shutdown_tx.send(true);
    server_handle
        .await
        .expect("server task should complete")
        .expect("server should stop cleanly");
}
