//! The real-hardware check harness's guarded VCI handle (`ngr_hw_checks::guard::GuardedHandle`,
//! design 13.5 "Guarded VCI access" and "Cleanup", ADR-275 item 4, ADR-333) on this service and
//! `j2534-0404-mock`, in process, so the mock's counters are readable.
//!
//! - A T3 profile's link is created, set up with the profile's ComParams and connected through the
//!   handle, a DID is read through it, and a write is refused before the worker sees it.
//! - The teardown steps run once after a panic: the return to the default session, the link's
//!   disconnect and destroy, and the device's close.
//! - A T0 link transmits the RawMode frame the guard decodes: the CAN identifier, the extended
//!   address byte under extended addressing, then the payload, as the literal `cop_data`.
//!
//! The handle opens an `OpenedDevice` of a worker process in `hw_open.rs`.

use std::panic::AssertUnwindSafe;

use ngr_hw_checks::guard::{GuardRefusal, GuardedHandle, HandleError, HandleOptions};
use ngr_hw_checks::profile::{self, Profile, Setup};
use ngr_hw_checks::report::RunGuard;
use serial_test::serial;
use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, ModuleHandle, StartComPrimitiveRequest, TxFlag,
    TxFlagBit, com_primitive_ctrl_data,
};

use crate::harness::*;

const VCI: &str = "api = \"j2534-0404\"\nlibrary = \"mock-lib\"\nmodule = \"Bench 1\"\n\
                   label = \"Bench 1\"\npname = \"J2534-2:mock\"\nfirmware_version = \"1.0.0\"\n\
                   library_version = \"1.0.0\"\n";
const LINK: &str = "protocol = \"ISO_14229_3_on_ISO_15765_2\"\nbaud_rate = 500000\n\
                    pins = [6, 14]\nrequest_id = 0x7E0\nresponse_id = 0x7E8\nid_bits = 11\n\
                    addressing = \"normal\"\n";

fn t3() -> Profile {
    profile::parse(&format!(
        "class = \"T3\"\n[vci]\n{VCI}\n[link]\n{LINK}functional_id = 0x7DF\npadding = 0xCC\n\
         [link.comparams]\nCP_BlockSize = 0\n"
    ))
    .expect("a T3 profile")
}

/// A T0 loopback with the mock at one end (the VCI's request identifier 0x124) and a SocketCAN
/// interface at the other.
fn t0(addressing: &str) -> Profile {
    let address = if addressing == "normal" {
        ""
    } else {
        "address = 0x55\n"
    };
    profile::parse(&format!(
        "class = \"T0\"\n\
         [[endpoint]]\nsocketcan = \"can0\"\n[endpoint.link]\nprotocol = \"CAN\"\n\
         baud_rate = 500000\npins = [6, 14]\nrequest_id = 0x123\nresponse_id = 0x124\n\
         id_bits = 11\naddressing = \"{addressing}\"\n{address}\
         [[endpoint]]\n[endpoint.vci]\n{VCI}\n[endpoint.link]\nprotocol = \
         \"ISO_14229_3_on_ISO_15765_2\"\nbaud_rate = 500000\npins = [6, 14]\n\
         request_id = 0x124\nresponse_id = 0x123\nid_bits = 11\naddressing = \"{addressing}\"\n\
         {address}"
    ))
    .expect("a T0 profile")
}

fn vci(profile: &Profile) -> profile::Vci {
    match &profile.setup {
        Setup::Link { vci, .. } => vci.clone(),
        Setup::Loopback(ends) => ends.iter().find_map(|e| e.vci.clone()).expect("a VCI end"),
    }
}

fn options() -> HandleOptions {
    HandleOptions {
        request_timeout: std::time::Duration::from_secs(5),
        response_timeout: std::time::Duration::from_secs(5),
    }
}

fn module() -> ModuleHandle {
    ModuleHandle {
        module_handle: MOCK_MODULE_HANDLE,
    }
}

/// A directory for the run's report, which the harness creates private to the user.
struct Reports(tempfile::TempDir);

impl Reports {
    fn new() -> Self {
        Self(tempfile::tempdir().expect("temporary directory"))
    }

    fn dir(&self) -> std::path::PathBuf {
        self.0.path().join("reports")
    }
}

/// The module and one link are open after the handle is: the mock saw one open, one connect.
const OPEN: usize = 1;

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_t3_handle_reads_a_did_and_refuses_a_write_before_the_worker() {
    let server = TestServer::start().await;
    let profile = t3();
    let reports = Reports::new();
    let mut run = RunGuard::start_in(&profile, "hw_guarded_handle", reports.dir());
    let mut handle = GuardedHandle::attach(
        &profile,
        &vci(&profile),
        server.worker_client().await,
        module(),
        options(),
        &mut run,
    )
    .await
    .expect("the handle opens the profile's link");

    assert_eq!(server.backdoor.connect_count(), OPEN);
    let [cll] = *handle.links() else {
        panic!("one link, not {:?}", handle.links());
    };
    // The link is bound to the profile's endpoint with every pinned ComParam set (the guard
    // refuses the connect otherwise), and the ids the handle asked for are learned.
    let bound = handle.bound_links();
    assert_eq!(bound[&cll.cll_handle].endpoint, 0);
    assert!(!bound[&cll.cll_handle].raw_mode);
    for name in [
        "cp_baudrate",
        "cp_canphysreqid",
        "cp_canrespusdtid",
        "cp_canfuncreqid",
        "cp_canfillerbyte",
        "cp_blocksize",
        "cp_testerpresentmessage",
    ] {
        assert!(bound[&cll.cll_handle].set.contains(name), "{name} is set");
    }
    let learned = handle.learned();
    assert!(!learned.resources.is_empty());
    assert!(
        learned
            .objects
            .values()
            .any(|name| name == "CP_CanPhysReqId")
    );

    // A DID read reaches the worker and the answer comes back.
    let answer = [0x62, 0xF1, 0x90, 0x41, 0x42];
    server.backdoor.arm_write_rx_injection(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &answer),
        j2534_0404::ISO15765,
        50,
    );
    let response = handle
        .send_receive(cll, &[0x22, 0xF1, 0x90])
        .await
        .expect("the DID is read");
    assert_eq!(response, answer);
    let written = server.backdoor.write_msgs_count();
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        can_frame(0x7E0, &[0x22, 0xF1, 0x90]),
        "the worker framed the request with the profile's request identifier"
    );

    // A write is refused before it reaches the worker: the mock saw no further write.
    let refused = handle
        .send_receive(cll, &[0x2E, 0xF1, 0x90, 0x00])
        .await
        .expect_err("a WriteDataByIdentifier is refused");
    assert!(
        matches!(
            refused,
            HandleError::Refused {
                rpc: "StartComPrimitive",
                refusal: GuardRefusal::Request { sid: 0x2E, .. }
            }
        ),
        "{refused:?}"
    );
    assert_eq!(server.backdoor.write_msgs_count(), written);

    // The teardown runs once: the link is disconnected and destroyed, the module is closed.
    drop(handle);
    run.pass();
    assert!(run.run_teardown().is_empty());
    assert_eq!(server.backdoor.disconnect_count(), 1);
    assert_eq!(server.backdoor.close_count(), 1);
    assert!(run.run_teardown().is_empty());
    assert_eq!(server.backdoor.disconnect_count(), 1);
    assert_eq!(server.backdoor.close_count(), 1);
    drop(run);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn the_teardown_steps_run_once_after_a_panic() {
    let server = TestServer::start().await;
    let profile = t3();
    let reports = Reports::new();
    let backdoor = &server.backdoor;
    let client = server.worker_client().await;
    let handle = tokio::runtime::Handle::current();

    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        tokio::task::block_in_place(|| {
            handle.block_on(async {
                let mut run = RunGuard::start_in(&profile, "hw_guarded_handle", reports.dir());
                let mut handle = GuardedHandle::attach(
                    &profile,
                    &vci(&profile),
                    client,
                    module(),
                    options(),
                    &mut run,
                )
                .await
                .expect("the handle opens the profile's link");
                let [cll] = *handle.links() else {
                    panic!("one link");
                };
                // Enter the extended session: the ECU answers, and the return to the default
                // session is registered.
                backdoor.arm_write_rx_injection(
                    MOCK_CHANNEL_ID,
                    &can_frame(0x7E8, &[0x50, 0x03, 0x00, 0x32, 0x01, 0xF4]),
                    j2534_0404::ISO15765,
                    50,
                );
                handle
                    .send_receive(cll, &[0x10, 0x03])
                    .await
                    .expect("the session starts");
                assert_eq!(backdoor.write_msgs_count(), 1);
                // The ECU answers the return to the default session during teardown.
                backdoor.arm_write_rx_injection(
                    MOCK_CHANNEL_ID,
                    &can_frame(0x7E8, &[0x50, 0x01, 0x00, 0x32, 0x01, 0xF4]),
                    j2534_0404::ISO15765,
                    50,
                );
                panic!("the check fails");
            })
        })
    }));
    assert!(outcome.is_err(), "the check panicked");

    // Dropping the run guard during the unwind ran each step once: `10 01` was sent (the mock
    // saw a second write), the link disconnected, the module closed.
    assert_eq!(server.backdoor.write_msgs_count(), 2);
    assert_eq!(server.backdoor.disconnect_count(), 1);
    assert_eq!(server.backdoor.close_count(), 1);
    // The report says the check panicked.
    let report = std::fs::read_dir(reports.dir())
        .expect("report directory")
        .next()
        .expect("a report was written")
        .expect("report entry")
        .path();
    let text = std::fs::read_to_string(report).expect("report text");
    assert!(text.contains("panicked"), "{text}");
    server.shutdown().await;
}

/// A RawMode send on `cll` with `data` as the literal frame, no response expected.
fn raw_send(
    cll: vci_service_interface::ComLogicalLinkHandle,
    data: &[u8],
    flags: &[TxFlagBit],
) -> StartComPrimitiveRequest {
    StartComPrimitiveRequest {
        cop_tag: None,
        cll_handle: Some(cll),
        cop_type: ComOperationType::CoptSendrecv as i32,
        cop_data: data.to_vec(),
        cop_ctrl_data: Some(ComPrimitiveCtrlData {
            time: 0,
            num_send_cycles: 1,
            num_receive_cycles: 0,
            temp_param_update: 0,
            expected_response_array: vec![],
            tx_flag: Some(com_primitive_ctrl_data::TxFlag::TxFlagBits(TxFlag {
                bits: flags.iter().map(|&f| f as i32).collect(),
            })),
        }),
    }
}

/// The form the guard decodes on T0 is what the worker transmits: `cop_data` of a RawMode link is
/// the whole `PassThruMessage.Data`, the 4-byte CAN identifier (most significant byte first),
/// then, with extended addressing (the `ISO15765_ADDR_TYPE` flag), the address byte, then the
/// payload, which the ISO 15765 channel segments itself, so it holds no transport PCI.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_t0_raw_frame_is_transmitted_as_the_guard_reads_it() {
    for (addressing, frame, flags) in [
        (
            "normal",
            can_frame(0x124, &[0x01, 0x02, 0x03]),
            Vec::<TxFlagBit>::new(),
        ),
        (
            "extended",
            can_frame(0x124, &[0x55, 0x01, 0x02, 0x03]),
            vec![TxFlagBit::TxFlagIso15765AddrType],
        ),
    ] {
        let server = TestServer::start().await;
        let profile = t0(addressing);
        let reports = Reports::new();
        let mut run = RunGuard::start_in(&profile, "hw_guarded_handle", reports.dir());
        let mut handle = GuardedHandle::attach(
            &profile,
            &vci(&profile),
            server.worker_client().await,
            module(),
            options(),
            &mut run,
        )
        .await
        .expect("the handle opens the T0 link in RawMode");
        let [cll] = *handle.links() else {
            panic!("one link");
        };
        assert!(handle.bound_links()[&cll.cll_handle].raw_mode);

        handle
            .start_com_primitive(raw_send(cll, &frame, &flags))
            .await
            .expect("the guard passes the frame to its own request identifier");
        // The worker hands the frame to the library as it is.
        let written = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while server.backdoor.written_count(MOCK_CHANNEL_ID) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(written.is_ok(), "the frame was written");
        assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), frame);

        // Another identifier, or the other address byte, is refused.
        let mut other_id = frame.clone();
        other_id[3] ^= 0x01;
        let mut requests = vec![other_id];
        if addressing != "normal" {
            let mut other_address = frame.clone();
            other_address[4] ^= 0x01;
            requests.push(other_address);
        }
        for request in requests {
            let refused = handle
                .start_com_primitive(raw_send(cll, &request, &flags))
                .await
                .expect_err("a frame to another header is refused");
            assert!(
                matches!(refused, HandleError::Refused { .. }),
                "{refused:?}"
            );
        }
        assert_eq!(server.backdoor.write_msgs_count(), 1);

        drop(handle);
        run.pass();
        assert!(run.run_teardown().is_empty());
        drop(run);
        server.shutdown().await;
    }
}
