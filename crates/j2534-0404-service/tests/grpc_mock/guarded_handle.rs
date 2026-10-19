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
    ComOperationType, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest,
    EcuUniqueRespData, ModuleHandle, ParamItem, PduParamClass, SetComParamRequest,
    SetUniqueRespIdTableRequest, StartComPrimitiveRequest, TxFlag, TxFlagBit,
    UniqueRespIdTableItem, com_primitive_ctrl_data, create_com_logical_link_request, param_item,
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
        response_timeout: Some(std::time::Duration::from_secs(5)),
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

/// A send-receive whose wait ends cancels the primitive: the worker reports it cancelled, not
/// still executing (the worker's own P2 is far longer than the handle's wait here).
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_send_receive_that_times_out_cancels_its_primitive() {
    let server = TestServer::start().await;
    let profile = t3();
    let reports = Reports::new();
    let mut run = RunGuard::start_in(&profile, "hw_guarded_handle", reports.dir());
    let mut handle = GuardedHandle::attach(
        &profile,
        &vci(&profile),
        server.worker_client().await,
        module(),
        HandleOptions {
            request_timeout: std::time::Duration::from_secs(5),
            response_timeout: Some(std::time::Duration::from_millis(300)),
        },
        &mut run,
    )
    .await
    .expect("the handle opens the profile's link");
    let [cll] = *handle.links() else {
        panic!("one link");
    };
    let silent = handle.send_receive(cll, &[0x22, 0xF1, 0x90]).await;
    assert_eq!(silent, Err(HandleError::NoResponse));

    // The first primitive of the worker is number 1.
    let status = handle
        .get_status(vci_service_interface::GetStatusRequest {
            handle: Some(
                vci_service_interface::get_status_request::Handle::CopHandle(
                    vci_service_interface::ComPrimitiveHandle {
                        module_handle: cll.module_handle,
                        cll_handle: cll.cll_handle,
                        cop_handle: 1,
                    },
                ),
            ),
        })
        .await
        .expect("the primitive's status");
    assert_eq!(
        status.status,
        Some(vci_service_interface::status_response::Status::CopStatus(
            vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
    );
    drop(handle);
    run.pass();
    assert!(run.run_teardown().is_empty());
    drop(run);
    server.shutdown().await;
}

/// A timing a check changes through the handle changes how long the default send-receive waits.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn the_default_deadline_follows_a_timing_the_check_changes() {
    let server = TestServer::start().await;
    let profile = t3();
    let reports = Reports::new();
    let mut run = RunGuard::start_in(&profile, "hw_guarded_handle", reports.dir());
    let mut handle = GuardedHandle::attach(
        &profile,
        &vci(&profile),
        server.worker_client().await,
        module(),
        HandleOptions::default(),
        &mut run,
    )
    .await
    .expect("the handle opens the profile's link");
    let [cll] = *handle.links() else {
        panic!("one link");
    };
    // P2 1 s, the 0x78 chain 25 s, P2* 5 s and the margin.
    let before = handle.response_ceiling(cll).expect("a ceiling");
    assert_eq!(before, std::time::Duration::from_secs(33));
    handle
        .set_com_param(vci_service_interface::SetComParamRequest {
            cll_handle: Some(cll),
            param_item: Some(vci_service_interface::ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamName(
                    "CP_RC78CompletionTimeout".to_owned(),
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcSpecified as i32,
                param_data: Some(vci_service_interface::param_item::ParamData::Unum32(
                    60_000_000,
                )),
            }),
        })
        .await
        .expect("the timing is set");
    assert_eq!(
        handle.response_ceiling(cll),
        Some(std::time::Duration::from_secs(68))
    );
    // A shorter timing is staged in place of the longer one: the link is still in effect on the
    // defaults (33 s), which are waited for until an update of the ComParams has finished.
    handle
        .set_com_param(vci_service_interface::SetComParamRequest {
            cll_handle: Some(cll),
            param_item: Some(vci_service_interface::ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamName(
                    "CP_RC78CompletionTimeout".to_owned(),
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcSpecified as i32,
                param_data: Some(vci_service_interface::param_item::ParamData::Unum32(
                    1_000_000,
                )),
            }),
        })
        .await
        .expect("the timing is set");
    assert_eq!(
        handle.response_ceiling(cll),
        Some(std::time::Duration::from_secs(33))
    );
    handle
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll),
            cop_type: ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("the update starts");
    assert_eq!(
        handle.response_ceiling(cll),
        Some(std::time::Duration::from_secs(9))
    );
    drop(handle);
    run.pass();
    assert!(run.run_teardown().is_empty());
    drop(run);
    server.shutdown().await;
}

fn named(name: &str, class: PduParamClass, data: param_item::ParamData) -> ParamItem {
    ParamItem {
        id: Some(param_item::Id::ParamName(name.to_owned())),
        com_param_class: class as i32,
        param_data: Some(data),
    }
}

/// A link a check destroys and creates again through the handle works like the first: it has
/// an event stream and timings, so a DID read on it succeeds once the check has set the
/// profile's ComParams and connected it.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_link_created_again_through_the_handle_reads_a_did() {
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
    let [first] = *handle.links() else {
        panic!("one link");
    };
    handle
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(first),
        })
        .await
        .expect("the link is destroyed");
    assert!(handle.bound_links().is_empty());

    // The same resource, found again.
    let resource = *handle
        .learned()
        .resources
        .keys()
        .next()
        .expect("a learned resource");
    let cll = handle
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(module()),
            resource: Some(create_com_logical_link_request::Resource::ResourceId(
                resource,
            )),
            cll_create_flag: None,
        })
        .await
        .expect("the link is created again")
        .cll_handle
        .expect("a handle");
    let com = PduParamClass::PduPcSpecified;
    let number = |v: u32| param_item::ParamData::Unum32(v);
    for (name, value) in [
        ("CP_Baudrate", 500_000),
        ("CP_RequestAddrMode", 1),
        ("CP_TesterPresentAddrMode", 0),
        ("CP_CanFuncReqId", 0x7DF),
        ("CP_CanFuncReqFormat", 4),
        ("CP_CanFuncReqExtAddr", 0),
        ("CP_CanFillerByteHandling", 1),
        ("CP_CanFillerByte", 0xCC),
        ("CP_BlockSize", 0),
        ("CP_RC78CompletionTimeout", 1_000_000),
    ] {
        handle
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll),
                param_item: Some(named(name, com, number(value))),
            })
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    handle
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll),
            param_item: Some(named(
                "CP_TesterPresentMessage",
                com,
                param_item::ParamData::Bytefield(vec![0x3E, 0x80]),
            )),
        })
        .await
        .expect("the tester-present message");
    let unique = PduParamClass::PduPcUniqueId;
    handle
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![EcuUniqueRespData {
                    unique_resp_identifier: 1,
                    params: [
                        ("CP_CanPhysReqId", 0x7E0),
                        ("CP_CanPhysReqFormat", 4),
                        ("CP_CanRespUSDTId", 0x7E8),
                        ("CP_CanRespUSDTFormat", 4),
                    ]
                    .map(|(name, value)| named(name, unique, number(value)))
                    .to_vec(),
                }],
            }),
        })
        .await
        .expect("the response table");
    // The link starts on the handle's defaults (1 s + 25 s + 5 s + 2 s); the shorter completion
    // timeout is staged and takes effect at the connect.
    assert_eq!(
        handle.response_ceiling(cll),
        Some(std::time::Duration::from_secs(33))
    );
    handle
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll),
        })
        .await
        .expect("the guard lets the connect through");
    assert_eq!(
        handle.response_ceiling(cll),
        Some(std::time::Duration::from_secs(9))
    );

    let channel = MOCK_CHANNEL_ID + 1;
    let answer = [0x62, 0xF1, 0x90, 0x41];
    server.backdoor.arm_write_rx_injection(
        channel,
        &can_frame(0x7E8, &answer),
        j2534_0404::ISO15765,
        50,
    );
    assert_eq!(
        handle.send_receive(cll, &[0x22, 0xF1, 0x90]).await,
        Ok(answer.to_vec())
    );
    drop(handle);
    run.pass();
    assert!(run.run_teardown().is_empty());
    drop(run);
    server.shutdown().await;
}

/// A link that entered another session is returned to the default one before it is destroyed,
/// and a destroy whose return fails is refused, so the teardown still owns the return.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn destroying_a_link_in_another_session_returns_it_to_the_default_one_first() {
    let server = TestServer::start().await;
    let profile = t3();
    let reports = Reports::new();
    let mut run = RunGuard::start_in(&profile, "hw_guarded_handle", reports.dir());
    let mut handle = GuardedHandle::attach(
        &profile,
        &vci(&profile),
        server.worker_client().await,
        module(),
        HandleOptions {
            request_timeout: std::time::Duration::from_secs(5),
            response_timeout: Some(std::time::Duration::from_millis(300)),
        },
        &mut run,
    )
    .await
    .expect("the handle opens the profile's link");
    let [cll] = *handle.links() else {
        panic!("one link");
    };
    let answer = |sub: u8| can_frame(0x7E8, &[0x50, sub, 0x00, 0x32, 0x01, 0xF4]);
    let arm = |sub: u8| {
        server.backdoor.arm_write_rx_injection(
            MOCK_CHANNEL_ID,
            &answer(sub),
            j2534_0404::ISO15765,
            50,
        )
    };
    arm(0x03);
    handle
        .send_receive(cll, &[0x10, 0x03])
        .await
        .expect("the session starts");
    assert_eq!(server.backdoor.write_msgs_count(), 1);

    // The ECU does not answer the return: the destroy is refused and the link stays.
    let destroy = || DestroyComLogicalLinkRequest {
        cll_handle: Some(cll),
    };
    let refused = handle.destroy_com_logical_link(destroy()).await;
    assert!(matches!(refused, Err(HandleError::Setup(_))), "{refused:?}");
    assert_eq!(server.backdoor.write_msgs_count(), 2);
    assert_eq!(handle.bound_links().len(), 1);

    // The ECU refuses the return (7F 10 22): that is no return either.
    server.backdoor.arm_write_rx_injection(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x10, 0x22]),
        j2534_0404::ISO15765,
        50,
    );
    let refused = handle.destroy_com_logical_link(destroy()).await;
    assert!(
        matches!(refused, Err(HandleError::Setup(ref m)) if m.contains("7F, 10, 22")),
        "{refused:?}"
    );
    assert_eq!(server.backdoor.write_msgs_count(), 3);
    assert_eq!(handle.bound_links().len(), 1);

    // It answers: the return is sent, then the link is destroyed.
    arm(0x01);
    handle
        .destroy_com_logical_link(destroy())
        .await
        .expect("the link is returned and destroyed");
    assert_eq!(server.backdoor.write_msgs_count(), 4);
    assert!(handle.bound_links().is_empty());

    // The teardown has nothing left to send for the session.
    drop(handle);
    run.pass();
    assert!(run.run_teardown().is_empty());
    assert_eq!(server.backdoor.write_msgs_count(), 4);
    drop(run);
    server.shutdown().await;
}

/// A disconnect of a link in another session is settled the same way: the return to the default
/// session goes first, on a link that is still connected, and a failed return refuses it.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn disconnecting_a_link_in_another_session_returns_it_to_the_default_one_first() {
    let server = TestServer::start().await;
    let profile = t3();
    let reports = Reports::new();
    let mut run = RunGuard::start_in(&profile, "hw_guarded_handle", reports.dir());
    let mut handle = GuardedHandle::attach(
        &profile,
        &vci(&profile),
        server.worker_client().await,
        module(),
        HandleOptions {
            request_timeout: std::time::Duration::from_secs(5),
            response_timeout: Some(std::time::Duration::from_millis(300)),
        },
        &mut run,
    )
    .await
    .expect("the handle opens the profile's link");
    let [cll] = *handle.links() else {
        panic!("one link");
    };
    let arm = |sub: u8| {
        server.backdoor.arm_write_rx_injection(
            MOCK_CHANNEL_ID,
            &can_frame(0x7E8, &[0x50, sub, 0x00, 0x32, 0x01, 0xF4]),
            j2534_0404::ISO15765,
            50,
        )
    };
    arm(0x03);
    handle
        .send_receive(cll, &[0x10, 0x03])
        .await
        .expect("the session starts");
    let disconnect = || DisconnectComLogicalLinkRequest {
        cll_handle: Some(cll),
    };

    // No answer to the return: the link stays connected.
    let refused = handle.disconnect_com_logical_link(disconnect()).await;
    assert!(matches!(refused, Err(HandleError::Setup(_))), "{refused:?}");
    assert_eq!(server.backdoor.disconnect_count(), 0);
    assert_eq!(server.backdoor.write_msgs_count(), 2);

    arm(0x01);
    handle
        .disconnect_com_logical_link(disconnect())
        .await
        .expect("returned, then disconnected");
    assert_eq!(server.backdoor.write_msgs_count(), 3);
    assert_eq!(server.backdoor.disconnect_count(), 1);

    drop(handle);
    run.pass();
    assert!(run.run_teardown().is_empty());
    assert_eq!(server.backdoor.write_msgs_count(), 3);
    drop(run);
    server.shutdown().await;
}

/// A check that returns the ECU to the default session itself leaves nothing pending: the
/// destroy and the teardown send no second `10 01`.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_return_to_the_default_session_by_the_check_clears_the_pending_one() {
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
    let [cll] = *handle.links() else {
        panic!("one link");
    };
    for session in [0x03, 0x01] {
        server.backdoor.arm_write_rx_injection(
            MOCK_CHANNEL_ID,
            &can_frame(0x7E8, &[0x50, session, 0x00, 0x32, 0x01, 0xF4]),
            j2534_0404::ISO15765,
            50,
        );
        handle
            .send_receive(cll, &[0x10, session])
            .await
            .expect("the session answers");
    }
    assert_eq!(server.backdoor.write_msgs_count(), 2);
    handle
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll),
        })
        .await
        .expect("nothing is pending, so the link is destroyed at once");
    assert_eq!(server.backdoor.write_msgs_count(), 2);
    drop(handle);
    run.pass();
    assert!(run.run_teardown().is_empty());
    assert_eq!(server.backdoor.write_msgs_count(), 2);
    drop(run);
    server.shutdown().await;
}

/// A destroy abandoned in the middle of the return to the default session (its caller gave up)
/// leaves the return pending: the teardown still sends `10 01`, after stopping the primitive the
/// abandoned wait left running.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_abandoned_destroy_leaves_the_return_to_the_teardown() {
    let server = TestServer::start().await;
    let profile = t3();
    let reports = Reports::new();
    let mut run = RunGuard::start_in(&profile, "hw_guarded_handle", reports.dir());
    let mut handle = GuardedHandle::attach(
        &profile,
        &vci(&profile),
        server.worker_client().await,
        module(),
        HandleOptions {
            request_timeout: std::time::Duration::from_secs(5),
            response_timeout: Some(std::time::Duration::from_secs(30)),
        },
        &mut run,
    )
    .await
    .expect("the handle opens the profile's link");
    let [cll] = *handle.links() else {
        panic!("one link");
    };
    let answer = |sub: u8| can_frame(0x7E8, &[0x50, sub, 0x00, 0x32, 0x01, 0xF4]);
    server.backdoor.arm_write_rx_injection(
        MOCK_CHANNEL_ID,
        &answer(0x03),
        j2534_0404::ISO15765,
        50,
    );
    handle
        .send_receive(cll, &[0x10, 0x03])
        .await
        .expect("the session starts");

    // The ECU is silent and the caller gives up after a short while.
    let abandoned = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        handle.destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll),
        }),
    )
    .await;
    assert!(abandoned.is_err(), "the destroy was still waiting");
    assert_eq!(server.backdoor.write_msgs_count(), 2);
    assert_eq!(handle.bound_links().len(), 1);

    // The teardown returns the ECU after all.
    drop(handle);
    run.pass();
    server.backdoor.arm_write_rx_injection(
        MOCK_CHANNEL_ID,
        &answer(0x01),
        j2534_0404::ISO15765,
        50,
    );
    assert!(run.run_teardown().is_empty());
    assert_eq!(server.backdoor.write_msgs_count(), 3);
    drop(run);
    server.shutdown().await;
}

/// A transmit queue a check suspended is resumed by the teardown, before the session is returned.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn the_teardown_resumes_a_suspended_transmit_queue() {
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
    let [cll] = *handle.links() else {
        panic!("one link");
    };
    let suspend = handle
        .get_object_id(vci_service_interface::GetObjectIdRequest {
            object_type: vci_service_interface::ObjectType::ObjtIoCtrl as i32,
            shortname: "PDU_IOCTL_SUSPEND_TX_QUEUE".to_owned(),
        })
        .await
        .expect("the id")
        .pdu_object_id;
    let ioctl = |id: u32| vci_service_interface::IoCtlRequest {
        handle: Some(vci_service_interface::io_ctl_request::Handle::CllHandle(
            cll,
        )),
        io_ctrl_command: Some(
            vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandId(id),
        ),
        input_data: None,
        has_output: false,
    };
    handle
        .io_ctl(ioctl(suspend))
        .await
        .expect("the queue is suspended");
    drop(handle);
    run.pass();
    // The steps ran: the resume went through the guard and the worker accepted it, so none failed.
    assert!(run.run_teardown().is_empty());
    drop(run);
    server.shutdown().await;
}

/// A session change the worker refuses (the link is not connected) leaves no return pending, so
/// the link can be destroyed.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_refused_session_change_leaves_no_return_pending() {
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
    let [cll] = *handle.links() else {
        panic!("one link");
    };
    handle
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll),
        })
        .await
        .expect("the link is disconnected");
    let refused = handle.send_receive(cll, &[0x10, 0x03]).await;
    assert!(
        matches!(refused, Err(HandleError::WorkerRefused { .. })),
        "the worker refuses a primitive on a link that is not connected: {refused:?}"
    );
    handle
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll),
        })
        .await
        .expect("nothing is pending");
    assert_eq!(server.backdoor.write_msgs_count(), 0);
    drop(handle);
    run.pass();
    assert!(run.run_teardown().is_empty());
    assert_eq!(server.backdoor.write_msgs_count(), 0);
    drop(run);
    server.shutdown().await;
}
