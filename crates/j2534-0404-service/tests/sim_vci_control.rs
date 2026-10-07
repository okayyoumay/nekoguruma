// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! Faults injected into `sim-vci` from outside the worker process (ADR-238): `worker-host`
//! launches the real `j2534-0404-service` binary against the `sim-vci` cdylib, with
//! `NGR_SIM_VCI_CONTROL_DIR` naming a directory, and this test drops control commands there
//! between agent jobs and while a link is open: a power loss of the ECU, its reconnection, a
//! battery voltage read through `READ_VBATT`, and VCI disconnects with the device closed and
//! open.
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned
//! service cannot race with another test.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::{JobError, JobLimits, Link, LinkConfig, link, run_program};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value, VmState};
use vci_service_interface::{
    GetObjectIdRequest, GetVersionRequest, IoCtlRequest, ObjectType, PduError, data_item,
    error_detail_from_status, io_ctl_request,
};
use worker_host::client::{ConnectOptions, WorkerClient};
use worker_host::service::{LaunchOptions, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's built-in VIN (`crates/sim-vci/docs/simulated-vci.md`).
const VIN: &[u8] = b"NGRSIMECU00000001";

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

/// Removes the temporary config file and control directory however the test ends.
struct TempPaths {
    config: PathBuf,
    control_dir: PathBuf,
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.config);
        let _ = std::fs::remove_dir_all(&self.control_dir);
    }
}

/// Writes `command` as the control file `name`, under a temporary name first so `sim-vci`
/// never reads it half-written. It takes effect at the next J2534 call.
fn send(control_dir: &Path, name: &str, command: &str) {
    let tmp = control_dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, command).expect("control file should be writable");
    std::fs::rename(&tmp, control_dir.join(format!("{name}.json")))
        .expect("control file should be renamed");
}

/// Reads the VIN.
fn read_vin() -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
        constants: vec![vec![0xF1, 0x90]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    }
}

/// The link each job opens.
fn link_config() -> LinkConfig {
    LinkConfig::iso15765(0x7E0, 0x7E8)
}
/// Deadline of each call while the test drives a link itself.
const DEADLINE: Duration = Duration::from_secs(5);

async fn run(client: &WorkerClient) -> Result<VmState, JobError> {
    run_program(
        client.clone(),
        &link_config(),
        read_vin(),
        JobLimits::default(),
    )
    .await
}

/// Battery voltage the worker reports through `PDU_IOCTL_READ_VBATT`, in millivolts.
async fn read_vbatt(client: &mut WorkerClient, link: &Link) -> u32 {
    let id = client
        .get_object_id(GetObjectIdRequest {
            object_type: ObjectType::ObjtIoCtrl as i32,
            shortname: "PDU_IOCTL_READ_VBATT".to_owned(),
        })
        .await
        .expect("PDU_IOCTL_READ_VBATT should be known")
        .into_inner()
        .pdu_object_id;
    let output = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(link.module_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(id)),
            input_data: None,
            has_output: true,
        })
        .await
        .expect("PDU_IOCTL_READ_VBATT should succeed")
        .into_inner()
        .output_data
        .and_then(|item| item.data);
    match output {
        Some(data_item::Data::Unum32Value(millivolts)) => millivolts,
        other => panic!("unexpected READ_VBATT output {other:?}"),
    }
}

/// `GetVersion` fails on a lost device. Either it reaches `PassThruReadVersion`, which returns
/// `ERR_DEVICE_NOT_CONNECTED`, or the service's own polling hit that error first and marked the
/// module as having lost the VCI (ADR-131), so it answers without calling the library. Which
/// one comes first is timing; both mean the device is lost. That the loss outlasts replugging
/// at the library level is checked without the service in `tests/sim_vci_library.rs`.
async fn assert_device_not_connected(client: &mut WorkerClient, link: &Link) {
    let result = client
        .get_version(GetVersionRequest {
            module_handle: Some(link.module_handle),
        })
        .await;
    let status = result.expect_err("GetVersion should fail on a lost device");
    // ERR_DEVICE_NOT_CONNECTED maps to PDU_ERR_COMM_PC_TO_VCI_FAILED; a module the service
    // marked as having lost the VCI is PDU_ERR_MODULE_NOT_CONNECTED.
    let pdu_error = error_detail_from_status(&status)
        .map(|detail| detail.pdu_error)
        .unwrap_or_else(|| panic!("no error detail in {status:?}"));
    assert!(
        pdu_error == PduError::PduErrCommPcToVciFailed as i32
            || pdu_error == PduError::PduErrModuleNotConnected as i32,
        "{status:?}"
    );
}

#[test]
fn control_commands_reach_sim_vci_in_the_worker() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let temp = std::env::temp_dir();
    let paths = TempPaths {
        config: temp.join(format!("sim-vci-control-{nanos}.toml")),
        control_dir: temp.join(format!("sim-vci-control-{nanos}")),
    };
    std::fs::write(
        &paths.config,
        format!(
            "[config.apis.j2534-0404.libs.{LIBRARY_NAME:?}]\nlibrary_path = {:?}\n",
            sim_vci_path().display().to_string()
        ),
    )
    .expect("test config file should be writable");
    std::fs::create_dir(&paths.control_dir).expect("control directory should be created");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &paths.config);
        std::env::set_var("NGR_SIM_VCI_CONTROL_DIR", &paths.control_dir);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        // The simulated ECU uses its built-in configuration.
        std::env::remove_var("NGR_SIM_ECU_CONFIG");
    }

    // The job's VM runs on a blocking thread that blocks on the runtime for every primitive,
    // so the runtime must be multi-threaded.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    // A wedged service must fail the test, not hang the CI job.
    runtime
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(120), inject(&paths.control_dir)).await
        })
        .expect("the end-to-end flow should finish in time");
    drop(paths);
}

async fn inject(control_dir: &Path) {
    let worker = WorkerProcess::launch(
        Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
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
    let client = worker
        .connect(&ConnectOptions::default())
        .await
        .expect("client should connect");
    let mut vin = vec![0x62, 0xF1, 0x90];
    vin.extend_from_slice(VIN);

    let state = run(&client).await.expect("the first job should finish");
    assert_eq!(state.stack, [Value::Bytes(vin.clone())], "{state:?}");

    // The ECU loses power: the request goes unanswered.
    send(
        control_dir,
        "001",
        r#"{"command": "inject_fault", "fault": "power_loss"}"#,
    );
    let silent = run(&client).await;
    assert!(
        matches!(silent, Err(JobError::Host { pc: 1, .. })),
        "{silent:?}"
    );
    send(control_dir, "002", r#"{"command": "reconnect_ecu"}"#);
    let state = run(&client)
        .await
        .expect("the job after reconnection should finish");
    assert_eq!(state.stack, [Value::Bytes(vin.clone())], "{state:?}");

    // The VCI is unplugged between jobs: opening the device fails.
    send(control_dir, "003", r#"{"command": "disconnect_vci"}"#);
    let unplugged = run(&client).await;
    assert!(
        format!("{unplugged:?}").contains("ERR_DEVICE_NOT_CONNECTED"),
        "{unplugged:?}"
    );
    send(control_dir, "004", r#"{"command": "connect_vci"}"#);
    let state = run(&client)
        .await
        .expect("the job after replugging should finish");
    assert_eq!(state.stack, [Value::Bytes(vin.clone())], "{state:?}");

    // The VCI is unplugged while a link is open, so the device is open when the command is
    // applied: the device is lost, and stays lost after the VCI is back (J2534-1 6.10.1).
    let mut link_client = client.clone();
    let link = link::open(&mut link_client, &link_config(), DEADLINE)
        .await
        .expect("the link should open");
    // READ_VBATT reaches sim-vci through PDU_IOCTL_READ_VBATT, and a control command changes
    // what it reports.
    assert_eq!(read_vbatt(&mut link_client, &link).await, 12_000);
    send(
        control_dir,
        "005",
        r#"{"command": "set_battery_voltage", "millivolts": 11500}"#,
    );
    assert_eq!(read_vbatt(&mut link_client, &link).await, 11_500);

    send(control_dir, "006", r#"{"command": "disconnect_vci"}"#);
    assert_device_not_connected(&mut link_client, &link).await;
    send(control_dir, "007", r#"{"command": "connect_vci"}"#);
    assert_device_not_connected(&mut link_client, &link).await;
    // Closing the link closes the lost device; the close itself still reports the loss.
    let _ = link::close(&mut link_client, link, DEADLINE).await;
    // The next job opens the device again, which gets a new device ID, and recovers.
    let state = run(&client)
        .await
        .expect("the job after recovery should finish");
    assert_eq!(state.stack, [Value::Bytes(vin)], "{state:?}");

    let left: Vec<_> = std::fs::read_dir(control_dir)
        .expect("control directory")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert!(left.is_empty(), "every command should be applied: {left:?}");

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}
