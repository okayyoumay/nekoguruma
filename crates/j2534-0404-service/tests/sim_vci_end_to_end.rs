// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! End to end without hardware (design 13.4): `worker-host` launches the real
//! `j2534-0404-service` binary against the `sim-vci` cdylib, with the
//! `unsigned long` width of the platform, and a gRPC client reads the VIN
//! (DID F190) from the simulated ECU behind it over an ISO 15765 link.
//!
//! On Linux x86_64 `unsigned long` is 8 bytes, so this also runs the service's
//! `long_size = 8` conversion against a real library.
//!
//! This file holds a single test, so the process-wide `VCI_CONFIG_PATH` it
//! sets for the spawned service cannot race with another test.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DisconnectComLogicalLinkRequest, EcuUniqueRespData,
    EventNotification, ExpectedResponseData, GetModuleIdsRequest, GetObjectIdRequest,
    GetResourceIdsRequest, GetVersionRequest, ModuleConnectRequest, ObjectType, ParamItem,
    PduComPrimitiveStatus, PduParamClass, ResourceData, SetComParamRequest,
    SetUniqueRespIdTableRequest, StartComPrimitiveRequest, SubscribeEventRequest,
    UniqueRespIdTableItem, create_com_logical_link_request, event_item, event_notification,
    param_item, resource_data, subscribe_event_request,
};
use worker_host::client::{ConnectOptions, WorkerClient};
use worker_host::service::{LaunchOptions, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's built-in VIN (`crates/sim-vci/docs/simulated-vci.md`).
const VIN: &[u8] = b"NGRSIMECU00000001";
const PHYS_REQ_ID: u32 = 0x7E0;
const RESP_ID: u32 = 0x7E8;

/// `sim-vci` is a dev-dependency, so cargo builds its cdylib into the same
/// `deps` directory as this test executable.
fn sim_vci_path() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let deps = exe.parent().expect("test executable has a directory");
    let name = format!(
        "{}sim_vci{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let path = deps.join(&name);
    assert!(path.is_file(), "{} should be built", path.display());
    path
}

/// Width of J2534 `unsigned long` in `sim-vci` on this platform.
fn long_size() -> u8 {
    if cfg!(windows) {
        4
    } else {
        std::mem::size_of::<std::os::raw::c_ulong>() as u8
    }
}

fn param(id: u32, class: PduParamClass, value: u32) -> ParamItem {
    ParamItem {
        id: Some(vci_service_interface::param_item::Id::ParamId(id)),
        com_param_class: class as i32,
        param_data: Some(param_item::ParamData::Unum32(value)),
    }
}

async fn comparam_id(client: &mut WorkerClient, shortname: &str) -> u32 {
    client
        .get_object_id(GetObjectIdRequest {
            object_type: ObjectType::ObjtComparam as i32,
            shortname: shortname.to_owned(),
        })
        .await
        .unwrap_or_else(|e| panic!("get_object_id({shortname}): {e}"))
        .into_inner()
        .pdu_object_id
}

/// Waits for the primitive to finish; returns the last result data seen.
async fn wait_finished(
    events: &mut tonic::Streaming<EventNotification>,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let mut result = None;
    let wait = async {
        while let Some(notification) = events.message().await.expect("event stream") {
            let Some(event_notification::EventData::Item(item)) = notification.event_data else {
                continue;
            };
            match item.data {
                Some(event_item::Data::ResultData(data)) => result = Some(data.data_bytes),
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
                {
                    return;
                }
                _ => {}
            }
        }
        panic!("event stream ended");
    };
    tokio::time::timeout(timeout, wait)
        .await
        .expect("the primitive should finish in time");
    result
}

#[tokio::test]
async fn service_reads_the_vin_from_sim_vci() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let config_path = std::env::temp_dir().join(format!("j2534-0404-service-{nanos}-sim-vci.toml"));
    std::fs::write(
        &config_path,
        format!(
            "[config.apis.j2534-0404.libs.{LIBRARY_NAME:?}]\nlibrary_path = {:?}\n",
            sim_vci_path().display().to_string()
        ),
    )
    .expect("test config file should be writable");
    // SAFETY: this test binary runs only this test, so no other thread reads
    // or writes the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        // The simulated ECU uses its built-in configuration.
        std::env::remove_var("NGR_SIM_ECU_CONFIG");
    }

    let worker = WorkerProcess::launch(
        std::path::Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        LIBRARY_NAME,
        // Generous timeouts for a debug binary on a busy CI runner.
        &LaunchOptions {
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(5),
            long_size: Some(long_size()),
            ..LaunchOptions::default()
        },
    )
    .await
    .expect("worker should launch");
    let mut client = worker
        .connect(&ConnectOptions::default())
        .await
        .expect("client should connect");

    let module_handle = client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("get_module_ids")
        .into_inner()
        .module_id_list
        .and_then(|list| list.module_data.into_iter().next())
        .and_then(|data| data.module_handle)
        .expect("a module");
    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("module_connect");

    // PassThruReadVersion's strings arrive through the service, so the
    // `unsigned long` width and string handling match on both sides.
    let version = client
        .get_version(GetVersionRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("get_version")
        .into_inner()
        .version_data
        .expect("version data");
    // Firmware string, DLL string, and API version "04.04" parsed to 4.
    assert_eq!(version.hw_name, "NGR-SIM 1.0", "{version:?}");
    assert!(version.fw_name.starts_with("sim-vci "), "{version:?}");
    assert_eq!(version.pdu_api_sw_version, 4, "{version:?}");

    let resource_id = client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(module_handle),
            resource_data: Some(ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(resource_data::Protocol::ProtocolName("ISO15765".to_owned())),
            }),
        })
        .await
        .expect("get_resource_ids")
        .into_inner()
        .resource_id_list
        .and_then(|list| list.resource_id_data_array.into_iter().next())
        .and_then(|data| data.resource_id_array.into_iter().next())
        .expect("an ISO15765 resource");
    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(module_handle),
            resource: Some(create_com_logical_link_request::Resource::ResourceId(
                resource_id,
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link")
        .into_inner()
        .cll_handle
        .expect("a link");

    for (name, value) in [("CP_Baudrate", 500_000), ("CP_P2Max", 2_000_000)] {
        let id = comparam_id(&mut client, name).await;
        client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(param(id, PduParamClass::PduPcCom, value)),
            })
            .await
            .unwrap_or_else(|e| panic!("set_com_param({name}): {e}"));
    }
    let phys_req = comparam_id(&mut client, "CP_CanPhysReqId").await;
    let resp = comparam_id(&mut client, "CP_CanRespUSDTId").await;
    // The table makes the service install the flow-control filter sim-vci
    // requires before it delivers any response.
    client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![EcuUniqueRespData {
                    unique_resp_identifier: 1,
                    params: vec![
                        param(phys_req, PduParamClass::PduPcUniqueId, PHYS_REQ_ID),
                        param(resp, PduParamClass::PduPcUniqueId, RESP_ID),
                    ],
                }],
            }),
        })
        .await
        .expect("set_unique_resp_id_table");
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link");

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event")
        .into_inner();

    // ReadDataByIdentifier VIN.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x22, 0xF1, 0x90],
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
        .await
        .expect("start_com_primitive(CoptSendrecv)");
    let response = wait_finished(&mut events, Duration::from_secs(10))
        .await
        .expect("a response from the simulated ECU");
    let mut expected = vec![0x62, 0xF1, 0x90];
    expected.extend_from_slice(VIN);
    assert!(
        response.ends_with(&expected),
        "unexpected response {response:02x?}"
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link");
    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
    let _ = std::fs::remove_file(&config_path);
}
