// Relies on the debug-only NGR_J2534_DEFINITION_DIR override; see ADR-282.
#![cfg(debug_assertions)]
//! Links that share an ISO 15765 channel share the point-to-point filters their receive
//! tables name (ADR-398), end to end without hardware (design 13.4): `worker-host` launches the
//! real `j2534-0404-service` binary against the `sim-vci` cdylib, whose adapter refuses a
//! filter that uses a pattern or flow-control identifier another filter of the channel uses
//! and takes ten filters per channel. The scenarios:
//!
//! - a link that disconnects and reconnects while a sibling holds the channel (the same table
//!   in part) connects every time, more often than the channel has filters, so no filter of
//!   the departed link is left behind;
//! - a functional link and a physical link whose tables share a pair connect on one channel,
//!   and the shared pair's filter is stopped only when the last of them leaves, which a new
//!   link naming that pair shows by connecting.
//!
//! This file holds a single test, so the process-wide `VCI_CONFIG_PATH` and
//! `NGR_J2534_DEFINITION_DIR` it sets for the spawned service cannot race with another test.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest,
    DisconnectComLogicalLinkRequest, EcuUniqueRespData, GetModuleIdsRequest, GetObjectIdRequest,
    GetResourceIdsRequest, ModuleConnectRequest, ModuleHandle, ObjectType, ParamItem,
    PduParamClass, ResourceData, SetComParamRequest, SetUniqueRespIdTableRequest,
    UniqueRespIdTableItem, create_com_logical_link_request, param_item, resource_data,
};
use worker_host::client::{ConnectOptions, WorkerClient};
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The channel's limit of filters (`sim_vci::MAX_FILTERS`; the library is built as a cdylib
/// only, so its constant cannot be linked).
const FILTER_LIMIT: usize = 10;

/// A pair: the ECU's response identifier and the tester's request identifier.
type Pair = (u32, u32);
const P0: Pair = (0x7E8, 0x7E0);
const P1: Pair = (0x7E9, 0x7E1);
const P2: Pair = (0x7EA, 0x7E2);

/// `sim-vci` is a dev-dependency, so cargo builds its cdylib into the same `deps` directory
/// as this test executable.
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
        id: Some(param_item::Id::ParamId(id)),
        com_param_class: class as i32,
        param_data: Some(param_item::ParamData::Unum32(value)),
    }
}

/// Removes a temporary path however the test ends.
struct TempPath(PathBuf);

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The client, the module and the ISO 15765 resource the scenarios create links on.
struct Bench {
    client: WorkerClient,
    module: ModuleHandle,
    resource: u32,
}

impl Bench {
    async fn comparam_id(&mut self, shortname: &str) -> u32 {
        self.client
            .get_object_id(GetObjectIdRequest {
                object_type: ObjectType::ObjtComparam as i32,
                shortname: shortname.to_owned(),
            })
            .await
            .unwrap_or_else(|e| panic!("get_object_id({shortname}): {e}"))
            .into_inner()
            .pdu_object_id
    }

    async fn set_comparam(
        &mut self,
        cll: ComLogicalLinkHandle,
        name: &str,
        class: PduParamClass,
        value: u32,
    ) {
        let id = self.comparam_id(name).await;
        self.client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll),
                param_item: Some(param(id, class, value)),
            })
            .await
            .unwrap_or_else(|e| panic!("set_com_param({name}): {e}"));
    }

    /// A link on the 500 kbit/s channel, with a receive table of `pairs`; a functional one
    /// also addresses 0x7DF.
    async fn link(&mut self, pairs: &[Pair], functional: bool) -> ComLogicalLinkHandle {
        let cll = self
            .client
            .create_com_logical_link(CreateComLogicalLinkRequest {
                module_handle: Some(self.module),
                resource: Some(create_com_logical_link_request::Resource::ResourceId(
                    self.resource,
                )),
                cll_create_flag: None,
            })
            .await
            .expect("create_com_logical_link")
            .into_inner()
            .cll_handle
            .expect("a link");
        self.set_comparam(cll, "CP_Baudrate", PduParamClass::PduPcCom, 500_000)
            .await;
        if functional {
            self.set_comparam(cll, "CP_RequestAddrMode", PduParamClass::PduPcCom, 2)
                .await;
            self.set_comparam(cll, "CP_CanFuncReqId", PduParamClass::PduPcCom, 0x7DF)
                .await;
        }
        let phys_req = self.comparam_id("CP_CanPhysReqId").await;
        let resp = self.comparam_id("CP_CanRespUSDTId").await;
        let unique_data = pairs
            .iter()
            .zip(1..)
            .map(
                |(&(resp_id, req_id), unique_resp_identifier)| EcuUniqueRespData {
                    unique_resp_identifier,
                    params: vec![
                        param(phys_req, PduParamClass::PduPcUniqueId, req_id),
                        param(resp, PduParamClass::PduPcUniqueId, resp_id),
                    ],
                },
            )
            .collect();
        self.client
            .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
                cll_handle: Some(cll),
                unique_resp_id_table: Some(UniqueRespIdTableItem { unique_data }),
            })
            .await
            .expect("set_unique_resp_id_table");
        cll
    }

    async fn connect(&mut self, cll: ComLogicalLinkHandle, what: &str) {
        self.client
            .connect_com_logical_link(ConnectComLogicalLinkRequest {
                cll_handle: Some(cll),
            })
            .await
            .unwrap_or_else(|e| panic!("{what}: connect_com_logical_link: {e}"));
    }

    async fn disconnect(&mut self, cll: ComLogicalLinkHandle) {
        self.client
            .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
                cll_handle: Some(cll),
            })
            .await
            .expect("disconnect_com_logical_link");
    }
}

#[tokio::test]
async fn links_on_one_channel_share_their_filters() {
    // A wedged service must fail the test, not hang the CI job.
    tokio::time::timeout(Duration::from_secs(120), scenarios())
        .await
        .expect("the scenarios should finish in time");
}

async fn scenarios() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let pid = std::process::id();
    let library = sim_vci_path();
    let config_path =
        std::env::temp_dir().join(format!("j2534-0404-service-{pid}-{nanos}-sharing.toml"));
    let _config = TempPath(config_path.clone());
    std::fs::write(&config_path, "").expect("test config file should be writable");
    let definition_dir = std::env::temp_dir().join(format!(
        "j2534-0404-service-{pid}-{nanos}-sharing-definitions"
    ));
    let _definitions = TempPath(definition_dir.clone());
    let _ = std::fs::remove_dir_all(&definition_dir);
    std::fs::create_dir_all(&definition_dir).expect("definition directory");
    j2534_0404_mock::write_definition(&definition_dir, LIBRARY_NAME, &library)
        .expect("definition file should be writable");
    // SAFETY: this test binary runs only this test, so no other thread reads or writes the
    // environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definition_dir);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        std::env::remove_var("NGR_SIM_ECU_CONFIG");
    }

    let worker = WorkerProcess::launch(
        std::path::Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        LIBRARY_NAME,
        OperatingMode::User,
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
    let module = client
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
            module_handle: Some(module),
        })
        .await
        .expect("module_connect");
    let resource = client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(module),
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
    let mut bench = Bench {
        client,
        module,
        resource,
    };

    reconnect_beside_a_sibling(&mut bench).await;
    functional_and_physical_links_share_a_pair(&mut bench).await;

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

/// A holds the channel with P0; B's table is P0 and P1. B connects and disconnects more often
/// than the channel has filters: each connect shares A's P0 and installs P1, and each
/// disconnect stops P1 and leaves P0, or the adapter would refuse P1 as not unique, or P0 as
/// over the limit, some time.
async fn reconnect_beside_a_sibling(bench: &mut Bench) {
    let a = bench.link(&[P0], false).await;
    bench.connect(a, "A").await;
    let b = bench.link(&[P0, P1], false).await;
    for cycle in 0..=FILTER_LIMIT + 2 {
        bench.connect(b, &format!("B, cycle {cycle}")).await;
        bench.disconnect(b).await;
    }
    bench.disconnect(a).await;
}

/// A functional presence link (P0 and P1) and a physical link (P0) share a channel. P1 goes
/// with the functional link; P0 stays for the physical one until it leaves. A link that names
/// the pair connects only if the adapter no longer holds it.
async fn functional_and_physical_links_share_a_pair(bench: &mut Bench) {
    let functional = bench.link(&[P0, P1], true).await;
    bench.connect(functional, "functional").await;
    let physical = bench.link(&[P0], false).await;
    bench.connect(physical, "physical").await;

    // P1 was the functional link's alone: it is free once that link leaves, while P0 is held.
    bench.disconnect(functional).await;
    let takes_p1 = bench.link(&[P1], false).await;
    bench
        .connect(takes_p1, "a link naming the freed pair P1")
        .await;
    bench.disconnect(takes_p1).await;

    // P0 is held by the physical link: a link that names it shares it (and still connects).
    let shares_p0 = bench.link(&[P0], false).await;
    bench
        .connect(shares_p0, "a link sharing the held pair P0")
        .await;
    bench.disconnect(shares_p0).await;

    // The physical link leaves while another link keeps the channel open: P0 is freed, and a
    // link with the same response identifier but another request identifier, which the
    // adapter refuses while P0 stands, connects.
    let keeps_open = bench.link(&[P2], false).await;
    bench
        .connect(keeps_open, "a link keeping the channel open")
        .await;
    bench.disconnect(physical).await;
    let reuses_response = bench.link(&[(P0.0, P2.1 + 0x10)], false).await;
    bench
        .connect(reuses_response, "a link naming P0's response identifier")
        .await;
    bench.disconnect(reuses_response).await;
    bench.disconnect(keeps_open).await;
}
