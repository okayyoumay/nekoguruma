//! The body of `hw_t0_two_endpoint_loopback` (`ngr_hw_checks::checks::t0_loopback`) against
//! `j2534-0404-mock`, in process: a relay task plays the loopback by copying every frame written on
//! one channel into the other's receive queue, so the part of the check that needs no hardware
//! runs in CI (design 13.5).

use std::sync::Arc;
use std::time::Duration;

use ngr_hw_checks::checks::t0_loopback::{CheckError, DeviceFacts, Ends, run as run_check};
use ngr_hw_checks::guard::{GuardedHandle, HandleOptions};
use ngr_hw_checks::profile::{self, Profile, Setup};
use ngr_hw_checks::report::{Masker, RunGuard};
use serial_test::serial;
use vci_service_interface::ModuleHandle;
use worker_host::abi::detect_file;

use crate::harness::*;

const LINK: &str = "protocol = \"ISO_14229_3_on_ISO_15765_2\"\nbaud_rate = 500000\n\
                    pins = [6, 14]\nid_bits = 11\naddressing = \"normal\"\n";

fn vci_toml(label: &str, pname: &str) -> String {
    format!(
        "api = \"j2534-0404\"\nlibrary = \"mock-lib\"\nmodule = \"{label}\"\nlabel = \"{label}\"\n\
         pname = \"{pname}\"\nfirmware_version = \"1.0.0\"\nlibrary_version = \"1.0.0\"\n"
    )
}

/// A T0 loopback of two VCI modules; `first` and `second` are the two ends' descriptions.
fn two_modules() -> Profile {
    let end = |label: &str, pname: &str, request: u32, response: u32| {
        format!(
            "[[endpoint]]\n[endpoint.vci]\n{}[endpoint.link]\n{LINK}request_id = {request}\n\
             response_id = {response}\n",
            vci_toml(label, pname)
        )
    };
    profile::parse(&format!(
        "class = \"T0\"\n{}{}",
        end("Bench A", "J2534-2:a", 0x123, 0x124),
        end("Bench B", "J2534-2:b", 0x124, 0x123)
    ))
    .expect("a T0 profile of two modules")
}

fn vcis(profile: &Profile) -> [profile::Vci; 2] {
    let Setup::Loopback(ends) = &profile.setup else {
        panic!("a loopback");
    };
    [0, 1].map(|i| ends[i].vci.clone().expect("a VCI end"))
}

fn facts() -> DeviceFacts {
    let library = j2534_0404_mock::mock_library_path().expect("the mock");
    let abi = detect_file(&library).expect("the mock's ABI");
    DeviceFacts {
        library,
        abi,
        long_size: abi.default_long_size(),
        declared_long_size: None,
    }
}

/// Copies every frame written on channel 1 into channel 2 and the other way, except those
/// `drop` says to lose (given the channel the frame was written on, and the frame).
fn relay(
    drop: impl Fn(u32, &[u8]) -> bool + Send + 'static,
) -> (
    tokio::task::JoinHandle<()>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let library = j2534_0404_mock::mock_library_path().expect("the mock");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        let backdoor = MockBackdoor::open(&library);
        let mut relayed = [0usize; 2];
        while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
            for (at, channel) in [(0usize, MOCK_CHANNEL_ID), (1, MOCK_CHANNEL_ID + 1)] {
                let other = if at == 0 {
                    MOCK_CHANNEL_ID + 1
                } else {
                    MOCK_CHANNEL_ID
                };
                while relayed[at] < backdoor.written_count(channel) {
                    let frame = backdoor.written_data(channel, relayed[at]);
                    relayed[at] += 1;
                    if !drop(channel, &frame) {
                        backdoor.inject_rx(other, &frame, j2534_0404::ISO15765);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    (task, stop)
}

struct Opened {
    first: GuardedHandle,
    second: GuardedHandle,
    run: RunGuard,
    profile: Profile,
    servers: [TestServer; 2],
    _dir: tempfile::TempDir,
}

async fn open_two_modules() -> Opened {
    let servers = [TestServer::start().await, TestServer::start().await];
    let profile = two_modules();
    let [vci_a, vci_b] = vcis(&profile);
    let dir = tempfile::tempdir().unwrap();
    let run = RunGuard::start_in(
        &profile,
        "hw_t0_two_endpoint_loopback",
        dir.path().join("reports"),
    );
    let options = HandleOptions {
        request_timeout: Duration::from_secs(5),
        response_timeout: Some(Duration::from_secs(5)),
    };
    let module = ModuleHandle {
        module_handle: MOCK_MODULE_HANDLE,
    };
    let first = GuardedHandle::attach(
        &profile,
        &vci_a,
        servers[0].worker_client().await,
        module,
        options,
        &run,
    )
    .await
    .expect("the first module's end opens in RawMode");
    let second = GuardedHandle::attach(
        &profile,
        &vci_b,
        servers[1].worker_client().await,
        module,
        options,
        &run,
    )
    .await
    .expect("the second module's end opens in RawMode");
    Opened {
        first,
        second,
        run,
        profile,
        servers,
        _dir: dir,
    }
}

/// Two modules, a relay: every frame of 1 to 7 bytes arrives exactly, in both directions, and the
/// message names the library, the ABI and the width.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn the_loopback_check_passes_through_a_relay() {
    let mut opened = open_two_modules().await;
    let (task, stop) = relay(|_, _| false);
    let facts = facts();
    let message = run_check(
        &opened.profile,
        Ends::TwoModules(&mut opened.first, &mut opened.second),
        &[facts.clone(), facts.clone()],
        &Masker::unmasked(),
        Duration::from_secs(5),
    )
    .await
    .expect("the check passes");
    assert!(
        message.contains("7 frame lengths in both directions"),
        "{message}"
    );
    assert!(
        message.contains(&facts.library.display().to_string()),
        "{message}"
    );
    assert!(message.contains(facts.abi.name()), "{message}");
    assert!(
        message.contains(&format!("unsigned long {} bytes", facts.long_size)),
        "{message}"
    );
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    task.await.unwrap();
    drop((opened.first, opened.second));
    opened.run.pass();
    assert!(opened.run.run_teardown().is_empty());
    drop(opened.run);
    let [a, b] = opened.servers;
    a.shutdown().await;
    b.shutdown().await;
}

/// A frame that never arrives fails the check and names the frame.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_missing_frame_fails_the_loopback_check() {
    let mut opened = open_two_modules().await;
    // The relay loses the 3-byte frame the second end sends.
    let (task, stop) =
        relay(|channel, frame| channel == MOCK_CHANNEL_ID + 1 && frame.len() == 4 + 3);
    let facts = facts();
    let error = run_check(
        &opened.profile,
        Ends::TwoModules(&mut opened.first, &mut opened.second),
        &[facts.clone(), facts],
        &Masker::unmasked(),
        Duration::from_millis(500),
    )
    .await
    .expect_err("the lost frame fails the check");
    assert!(matches!(error, CheckError::Frame(_)), "{error}");
    assert!(error.to_string().contains("3-byte frame"), "{error}");
    assert_eq!(error.outcome(), ngr_hw_checks::report::Outcome::Failed);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    task.await.unwrap();
    drop((opened.first, opened.second));
    opened
        .run
        .finish(ngr_hw_checks::report::Outcome::Failed, &error.to_string());
    assert!(opened.run.run_teardown().is_empty());
    // The run is recorded as failed, so dropping it would fail this test: leave it in a
    // forgotten state is not possible, so the report of the failure is the expected one.
    let [a, b] = opened.servers;
    std::mem::forget(opened.run);
    a.shutdown().await;
    b.shutdown().await;
}

/// A T0 profile with one SocketCAN end and one VCI end.
fn socketcan_end() -> Profile {
    profile::parse(&format!(
        "class = \"T0\"\n[[endpoint]]\nsocketcan = \"can0\"\n[endpoint.link]\nprotocol = \"CAN\"\n\
         baud_rate = 500000\npins = [6, 14]\nid_bits = 11\naddressing = \"normal\"\n\
         request_id = 0x123\nresponse_id = 0x124\n\
         [[endpoint]]\n[endpoint.vci]\n{}[endpoint.link]\n{LINK}request_id = 0x124\nresponse_id = 0x123\n",
        vci_toml("Bench A", "J2534-2:a")
    ))
    .expect("a T0 profile with a SocketCAN end")
}

/// The check drives VCI ends only: a SocketCAN end is refused, with nothing sent.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_socketcan_end_is_refused_by_the_loopback_check() {
    let server = TestServer::start().await;
    let profile = socketcan_end();
    let Setup::Loopback(ends) = &profile.setup else {
        panic!("a loopback");
    };
    let vci = ends[1].vci.clone().expect("the VCI end");
    let dir = tempfile::tempdir().unwrap();
    let mut run = RunGuard::start_in(
        &profile,
        "hw_t0_two_endpoint_loopback",
        dir.path().join("reports"),
    );
    let mut handle = GuardedHandle::attach(
        &profile,
        &vci,
        server.worker_client().await,
        ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        },
        HandleOptions::default(),
        &run,
    )
    .await
    .expect("the VCI end opens");
    let written = server.backdoor.write_msgs_count();
    let error = run_check(
        &profile,
        Ends::OneModule(&mut handle),
        &[facts()],
        &Masker::unmasked(),
        Duration::from_millis(200),
    )
    .await
    .expect_err("a SocketCAN end is refused");
    assert!(matches!(error, CheckError::Refused(_)), "{error}");
    assert!(
        error.to_string().contains("SocketCAN end (can0)"),
        "{error}"
    );
    assert_eq!(error.outcome(), ngr_hw_checks::report::Outcome::Refused);
    assert_eq!(server.backdoor.write_msgs_count(), written);
    drop(handle);
    run.pass();
    assert!(run.run_teardown().is_empty());
    drop(run);
    server.shutdown().await;
}
