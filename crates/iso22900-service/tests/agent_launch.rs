// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_PDU_API_ROOT_FILE overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! The agent's launch path for a D-PDU API VCI: `agent::launch::launch_worker` with
//! `Api::Dpdu` resolves the `iso22900-mock` library through a root description file, picks the
//! `iso22900-service` build for the library's ABI from a workers directory, starts it with the
//! name and the device's mode, and connects a client that reaches the mock's module. Through
//! that client, the resource the agent names a link by (ISO 22900-2 (2022) Annex B.1 and
//! B.2 short names, ADR-319) resolves and a logical link is created on it.

use std::path::Path;
use std::time::Duration;

use agent::LinkConfig;
use agent::launch::{Api, LaunchError, host_runs, launch_worker};
use serial_test::serial;
use vci_service_interface::{
    CreateComLogicalLinkRequest, GetModuleIdsRequest, GetResourceIdsRequest, ModuleConnectRequest,
    create_com_logical_link_request,
};
use worker_host::abi::Abi;
use worker_host::service::{LaunchOptions, OperatingMode, Stopped, WorkerLayout};

const LIBRARY_NAME: &str = "NGR Mock D-PDU API";

/// Installs the service binary as `<workers>/<ABI name>/iso22900-service`, the layout the agent
/// selects from.
fn install_worker(workers: &Path, abi_name: &str) {
    let service = Path::new(env!("CARGO_BIN_EXE_iso22900-service"));
    let dir = workers.join(abi_name);
    std::fs::create_dir_all(&dir).expect("workers directory should be writable");
    let target = dir.join(service.file_name().expect("service binary has a file name"));
    if std::fs::hard_link(service, &target).is_err() {
        std::fs::copy(service, &target).expect("service binary should be copyable");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn the_agent_launches_iso22900_service_for_a_d_pdu_api_vci() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let root = temp.path();
    let library = iso22900_mock::mock_library_path().expect("the mock cdylib should be built");
    let abi = worker_host::abi::detect_file(&library).expect("the mock should have a known ABI");
    let workers = WorkerLayout {
        root: root.join("workers"),
    };
    install_worker(&workers.root, abi.name());
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, "").expect("test config file should be writable");
    // The agent resolves in this process and the worker in its own, which inherits these.
    let root_file = iso22900_mock::write_root_file(root, LIBRARY_NAME, &library)
        .expect("root description file should be writable");
    unsafe {
        std::env::set_var("NGR_PDU_API_ROOT_FILE", &root_file);
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
    }

    let mut worker = launch_worker(
        Api::Dpdu,
        LIBRARY_NAME,
        OperatingMode::User,
        &workers,
        &LaunchOptions::default(),
        None,
    )
    .await
    .expect("the agent should launch iso22900-service for the mock");
    assert_eq!(worker.abi, abi);
    let modules = worker
        .client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("the worker should list the mock's modules")
        .into_inner()
        .module_id_list
        .map(|list| list.module_data)
        .unwrap_or_default();
    assert_eq!(modules.len(), 1, "{modules:?}");

    // The agent's resource data: the standard names resolve, and a link is created on the
    // resource they select.
    let module_handle = modules[0].module_handle;
    worker
        .client
        .module_connect(ModuleConnectRequest { module_handle })
        .await
        .expect("the module should connect");
    let resource_id = worker
        .client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle,
            resource_data: Some(agent::link::resource_data(&LinkConfig::iso15765(
                0x7E0, 0x7E8,
            ))),
        })
        .await
        .expect("the standard names should resolve")
        .into_inner()
        .resource_id_list
        .and_then(|list| list.resource_id_data_array.into_iter().next())
        .and_then(|data| data.resource_id_array.into_iter().next())
        .expect("a resource for UDS on CAN");
    let link = worker
        .client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle,
            resource: Some(create_com_logical_link_request::Resource::ResourceId(
                resource_id,
            )),
            cll_create_flag: None,
        })
        .await
        .expect("a link should be created on the resource")
        .into_inner();
    assert!(link.cll_handle.is_some(), "{link:?}");
    assert_eq!(
        worker.process.stop(Duration::from_secs(5)).await.ok(),
        Some(Stopped::Exited)
    );

    // A name the root file does not list fails the resolution, with the build's reason.
    let error = launch_worker(
        Api::Dpdu,
        "Unknown D-PDU API",
        OperatingMode::User,
        &workers,
        &LaunchOptions::default(),
        None,
    )
    .await
    .err()
    .expect("an unknown name should not launch")
    .to_string();
    assert!(error.contains("Unknown D-PDU API"), "{error}");
}

/// Points the agent's lookup at the mock, as the launch test above does; returns the workers
/// directory (holding the host-ABI worker) and the mock's ABI.
fn set_up_mock(root: &Path) -> (WorkerLayout, Abi) {
    let library = iso22900_mock::mock_library_path().expect("the mock cdylib should be built");
    let abi = worker_host::abi::detect_file(&library).expect("the mock should have a known ABI");
    let workers = WorkerLayout {
        root: root.join("workers"),
    };
    install_worker(&workers.root, abi.name());
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, "").expect("test config file should be writable");
    let root_file = iso22900_mock::write_root_file(root, LIBRARY_NAME, &library)
        .expect("root description file should be writable");
    // SAFETY: the tests of this file are `#[serial]`, so none reads the environment meanwhile.
    unsafe {
        std::env::set_var("NGR_PDU_API_ROOT_FILE", &root_file);
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
    }
    (workers, abi)
}

/// ADR-415: naming the library's own ABI launches the worker, and it reports that ABI.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn naming_the_mocks_own_abi_launches_its_worker() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let (workers, abi) = set_up_mock(temp.path());
    let worker = launch_worker(
        Api::Dpdu,
        LIBRARY_NAME,
        OperatingMode::User,
        &workers,
        &LaunchOptions::default(),
        Some(abi),
    )
    .await
    .expect("the mock's own ABI should launch");
    assert_eq!(worker.abi, abi);
    assert_eq!(
        worker.process.stop(Duration::from_secs(5)).await.ok(),
        Some(Stopped::Exited)
    );
}

/// ADR-415: another runnable ABI is refused for a library built for the mock's, with the
/// build's reason, and the worker is never started (a launch would also need its binary, which
/// the directory does not hold for that ABI).
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn naming_another_abi_is_refused_without_a_fallback() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let (workers, abi) = set_up_mock(temp.path());
    let Some(other) = Abi::ALL
        .into_iter()
        .find(|&other| other != abi && host_runs(other))
    else {
        return;
    };
    let error = launch_worker(
        Api::Dpdu,
        LIBRARY_NAME,
        OperatingMode::User,
        &workers,
        &LaunchOptions::default(),
        Some(other),
    )
    .await
    .err()
    .expect("another ABI should not launch");
    assert!(matches!(error, LaunchError::Resolve { .. }), "{error}");
    assert!(error.to_string().contains("is built for"), "{error}");
}

/// ADR-415: an ABI the agent build cannot run is refused before any lookup.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn naming_an_abi_the_host_cannot_run_is_refused_first() {
    // An ABI this agent build cannot run: every host leaves out at least the other OS's ABIs.
    let foreign = Abi::ALL
        .into_iter()
        .find(|abi| !host_runs(*abi))
        .expect("every host leaves out some ABI");
    let temp = tempfile::tempdir().expect("temporary directory");
    let (workers, _) = set_up_mock(temp.path());
    let error = launch_worker(
        Api::Dpdu,
        LIBRARY_NAME,
        OperatingMode::User,
        &workers,
        &LaunchOptions::default(),
        Some(foreign),
    )
    .await
    .err()
    .expect("an ABI this agent build cannot run should not launch");
    assert!(
        matches!(error, LaunchError::AbiNotRunnable { abi, .. } if abi == foreign),
        "{error}"
    );
}
