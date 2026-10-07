//! The simulated ECU outlives the process that loads `sim-vci` when `NGR_SIM_ECU_STATE` names a
//! state file (ADR-241), as a vehicle outlives a crashed worker.
//!
//! The test runs each step in a child process that loads the cdylib through the `j2534-0404`
//! wrapper: the test executable starts itself again with [`STEP_ENV`] set. One child starts a
//! download and exits without closing anything, as a crashing worker would; the next ones read
//! the flash state (DID FD00) and the session (DID F186) the ECU kept, before and after
//! tS3_Server has run out. A fault armed by a control command in one process fires in the next.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use j2534_0404::{ChannelId, FLOW_CONTROL_FILTER, ISO15765, J2534Api0404, PassThruMessage};

/// Set in a child process to the step it runs.
const STEP_ENV: &str = "NGR_TEST_SIM_VCI_RESTART_STEP";
/// This test's name, which a child process runs alone.
const TEST_NAME: &str = "sim_ecu_state_survives_a_restart_of_the_loading_process";

const REQUEST_ID: u32 = 0x7E0;
const RESPONSE_ID: u32 = 0x7E8;
/// tS3_Server of the configured ECU, in milliseconds.
const S3_MS: u64 = 5_000;
const BLOCK_LEN: usize = 16;

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
fn long_size() -> usize {
    if cfg!(windows) {
        4
    } else {
        std::mem::size_of::<std::os::raw::c_ulong>()
    }
}

/// Removes the temporary directory however the test ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn message(data: &[u8]) -> PassThruMessage {
    PassThruMessage::new(ISO15765, 0, 0, 0, 0, data).expect("message fits")
}

fn can(id: u32, payload: &[u8]) -> Vec<u8> {
    let mut data = id.to_be_bytes().to_vec();
    data.extend_from_slice(payload);
    data
}

/// Opens the device and an ISO 15765 channel with a flow-control filter for the ECU.
fn connect(api: &J2534Api0404) -> ChannelId {
    let device = api.open(None).expect("the device should open");
    let channel = api
        .connect(device, ISO15765, 0, 500_000)
        .expect("the channel should connect");
    api.start_message_filter(
        channel,
        FLOW_CONTROL_FILTER,
        &mut message(&[0xFF, 0xFF, 0xFF, 0xFF]),
        &mut message(&RESPONSE_ID.to_be_bytes()),
        Some(&mut message(&REQUEST_ID.to_be_bytes())),
    )
    .expect("the filter should start");
    channel
}

/// Sends a physically addressed request and returns the ECU's response, without the CAN ID.
fn request(api: &J2534Api0404, channel: ChannelId, payload: &[u8]) -> Vec<u8> {
    api.write_messages(channel, &mut [message(&can(REQUEST_ID, payload))], 1_000)
        .expect("the request should be sent");
    let responses = api
        .read_messages(channel, 1, 2_000)
        .expect("the ECU should answer");
    let data = responses[0].data().expect("response data");
    assert_eq!(data[..4], RESPONSE_ID.to_be_bytes(), "{data:02X?}");
    data[4..].to_vec()
}

/// Reads a DID and returns its record.
fn read_did(api: &J2534Api0404, channel: ChannelId, did: u16) -> Vec<u8> {
    let [high, low] = did.to_be_bytes();
    let response = request(api, channel, &[0x22, high, low]);
    assert_eq!(response[..3], [0x62, high, low], "{response:02X?}");
    response[3..].to_vec()
}

/// DID FD00's record: phase code, block number, bytes received (big endian).
fn flash_state(phase: u8, block: u32, received: u32) -> Vec<u8> {
    let mut record = vec![phase];
    record.extend_from_slice(&block.to_be_bytes());
    record.extend_from_slice(&received.to_be_bytes());
    record
}

/// Runs one step in this process, which a parent started.
fn run_step(step: &str) {
    let api = J2534Api0404::from_path(sim_vci_path()).expect("sim-vci should load");
    let channel = connect(&api);
    match step {
        "download" => {
            assert_eq!(request(&api, channel, &[0x10, 0x02])[..2], [0x50, 0x02]);
            assert_eq!(
                request(&api, channel, &[0x31, 0x01, 0xFF, 0x00]),
                [0x71, 0x01, 0xFF, 0x00]
            );
            let size = (3 * BLOCK_LEN) as u32;
            let mut download = vec![0x34, 0x00, 0x44, 0, 0, 0, 0];
            download.extend_from_slice(&size.to_be_bytes());
            assert_eq!(request(&api, channel, &download)[0], 0x74);
            for bsc in 1..=2u8 {
                let mut transfer = vec![0x36, bsc];
                transfer.extend_from_slice(&[bsc; BLOCK_LEN]);
                assert_eq!(request(&api, channel, &transfer), [0x76, bsc]);
            }
            // The worker crashes: nothing is closed or unloaded.
            std::process::exit(0);
        }
        "within-s3" => {
            // The new process sees the transfer still running, in the programming session.
            assert_eq!(read_did(&api, channel, 0xF186), [0x02]);
            assert_eq!(
                read_did(&api, channel, 0xFD00),
                flash_state(0x02, 3, 2 * BLOCK_LEN as u32)
            );
        }
        "after-s3" => {
            // tS3_Server ran out while no process had the library loaded: the session ended
            // and the transfer was interrupted after block 2.
            assert_eq!(read_did(&api, channel, 0xF186), [0x01]);
            assert_eq!(
                read_did(&api, channel, 0xFD00),
                flash_state(0x05, 2, 2 * BLOCK_LEN as u32)
            );
        }
        "arm" => {
            // The control file is applied at the start of the next J2534 call; this one reads
            // nothing.
            let control =
                Path::new(&std::env::var_os("NGR_SIM_VCI_CONTROL_DIR").expect("set")).to_path_buf();
            let tmp = control.join("001.tmp");
            std::fs::write(
                &tmp,
                r#"{"command": "inject_fault", "fault": "drop_response"}"#,
            )
            .expect("control file should be writable");
            std::fs::rename(&tmp, control.join("001.json")).expect("control file renamed");
            let _ = api.read_messages(channel, 1, 0);
            assert!(
                std::fs::read_dir(&control)
                    .expect("control directory")
                    .next()
                    .is_none(),
                "the command should be applied"
            );
            std::process::exit(0);
        }
        "armed" => {
            // The fault armed in the previous process drops this response, and only this one.
            api.write_messages(
                channel,
                &mut [message(&can(REQUEST_ID, &[0x3E, 0x00]))],
                1_000,
            )
            .expect("the request should be sent");
            assert!(api.read_messages(channel, 1, 500).is_err(), "dropped");
            assert_eq!(request(&api, channel, &[0x3E, 0x00]), [0x7E, 0x00]);
        }
        other => panic!("unknown step {other:?}"),
    }
}

/// Runs `step` in a new process with the ECU's config and state files.
fn run_child(step: &str, dir: &Path) {
    let status = Command::new(std::env::current_exe().expect("test executable path"))
        .args([TEST_NAME, "--exact", "--nocapture"])
        .env(STEP_ENV, step)
        .env("NGR_SIM_ECU_CONFIG", dir.join("ecu.json"))
        .env("NGR_SIM_ECU_STATE", dir.join("ecu.state"))
        .env("NGR_J2534_LONG_SIZE", long_size().to_string())
        .env("NGR_SIM_VCI_CONTROL_DIR", dir.join("control"))
        .status()
        .expect("the child process should start");
    assert!(status.success(), "step {step:?} failed: {status}");
}

#[test]
fn sim_ecu_state_survives_a_restart_of_the_loading_process() {
    if let Ok(step) = std::env::var(STEP_ENV) {
        run_step(&step);
        return;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let dir = TempDir(std::env::temp_dir().join(format!("sim-vci-restart-{nanos}")));
    std::fs::create_dir_all(dir.0.join("control")).expect("temporary directory should be created");
    std::fs::write(
        dir.0.join("ecu.json"),
        format!(
            r#"{{"vin": "NGRSIMECU00000002", "part_number": "NGR-SIM-ECU", "sw_version": "1.0.0",
                "response_delay_ms": 0, "drop_at_block": null, "require_security_access": false,
                "require_gateway_auth": false, "fail_checksum": false, "s3_server_ms": {S3_MS}}}"#
        ),
    )
    .expect("the ECU configuration should be written");

    run_child("download", &dir.0);
    assert!(
        dir.0.join("ecu.state").is_file(),
        "the state should be kept"
    );
    run_child("within-s3", &dir.0);
    // The last request of the previous step restarted tS3_Server; let it run out with no
    // process holding the library.
    std::thread::sleep(Duration::from_millis(S3_MS + 500));
    run_child("after-s3", &dir.0);
    run_child("arm", &dir.0);
    run_child("armed", &dir.0);
}
