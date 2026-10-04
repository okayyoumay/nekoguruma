//! Config-declared multi-module device selection (ADR-107): `GetModuleIds`/
//! `ModuleConnect` over N configured `[[...modules]]` entries, `pname`
//! reaching `PassThruOpen`, the single-open-device-at-a-time reject-on-switch
//! guard, and `modules = []`/invalid-`pname` startup failures.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    CancelComPrimitiveRequest, ComPrimitiveCtrlData, CreateComLogicalLinkRequest, DataItem,
    ExpectedResponseData, GetModuleIdsRequest, GetObjectIdRequest, GetStatusRequest, IoCtlRequest,
    IoFilter, IoFilterList, ModuleConnectRequest, ModuleDisconnectRequest, ModuleHandle,
    ObjectType, PduComPrimitiveStatus, PduError, PduErrorEvent, PduFilter, PduModuleStatus,
    StartComPrimitiveRequest, create_com_logical_link_request, data_item, error_detail_from_status,
    event_item, get_status_request, io_ctl_request, status_response, subscribe_event_request,
    vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- mirrors `pdu_ioctl.rs`'s private helper
/// of the same name; each `tests/grpc_mock/*.rs` file is its own module, so
/// it cannot be shared without promoting it into `harness.rs`.
async fn resolve_ioctl_id(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    name: &str,
) -> u32 {
    client
        .get_object_id(GetObjectIdRequest {
            object_type: ObjectType::ObjtIoCtrl as i32,
            shortname: name.to_string(),
        })
        .await
        .unwrap_or_else(|err| panic!("get_object_id({name}) should succeed: {err}"))
        .into_inner()
        .pdu_object_id
}

/// Issues a module-scoped `IoCtl` (e.g. `PDU_IOCTL_RESET`) addressed to
/// `module_handle`.
async fn io_ctl_module(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    module_handle: u32,
    cmd_id: u32,
) -> Result<(), tonic::Status> {
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: None,
            has_output: false,
        })
        .await
        .map(|_| ())
}

/// `TestServer` doesn't implement `Debug` (its `Server`/`JoinHandle` fields
/// don't), so `Result::expect_err` can't be used directly on a startup
/// `Result<TestServer, _>` -- this extracts the error, panicking with `msg`
/// if startup unexpectedly succeeded.
fn expect_startup_err(
    result: Result<TestServer, vci_service_launcher::BoxError>,
    msg: &str,
) -> vci_service_launcher::BoxError {
    match result {
        Ok(_) => panic!("{msg}"),
        Err(err) => err,
    }
}

/// (a) No `modules` configured: `GetModuleIds`/`ModuleConnect` behavior is
/// byte-identical to the pre-ADR-107 single-synthetic-module baseline.
#[tokio::test]
#[serial]
async fn no_modules_configured_matches_pre_adr106_baseline() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let module_ids = client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("get_module_ids should succeed")
        .into_inner();
    let rows = module_ids
        .module_id_list
        .expect("module_id_list should be present")
        .module_data;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].module_handle,
        Some(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE
        })
    );
    assert_eq!(rows[0].vendor_module_name, "j2534-0404");
    assert_eq!(rows[0].vendor_additional_info, "mock-lib");

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");
    assert_eq!(server.backdoor.open_count(), 1);
    assert_eq!(
        server.backdoor.open_pname(),
        None,
        "PassThruOpen should receive a NULL pName for the synthetic default module"
    );

    server.shutdown().await;
}

/// (b) `modules = []` (present but empty): a startup error.
#[tokio::test]
#[serial]
async fn empty_modules_array_fails_startup() {
    let result = TestServer::try_start_with_extra_config("modules = []\n").await;
    let err = expect_startup_err(result, "an empty modules array should fail startup");
    assert!(
        err.to_string().contains("empty"),
        "error should mention the empty array: {err}"
    );
}

/// (c) N configured entries: `GetModuleIds` returns N rows, handles `1..=N`.
#[tokio::test]
#[serial]
async fn n_configured_entries_returns_n_module_rows() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    let module_ids = client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("get_module_ids should succeed")
        .into_inner();
    let rows = module_ids
        .module_id_list
        .expect("module_id_list should be present")
        .module_data;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].module_handle,
        Some(ModuleHandle { module_handle: 1 })
    );
    assert_eq!(rows[0].vendor_module_name, "Bench 1");
    assert_eq!(rows[0].vendor_additional_info, "mock-lib");
    assert_eq!(
        rows[1].module_handle,
        Some(ModuleHandle { module_handle: 2 })
    );
    assert_eq!(rows[1].vendor_module_name, "Bench 2");

    server.shutdown().await;
}

/// (d) `ModuleConnect` with an out-of-range handle: error.
#[tokio::test]
#[serial]
async fn module_connect_out_of_range_handle_is_rejected() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    let status = client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 3 }),
        })
        .await
        .expect_err("module_handle 3 is out of range for 2 configured entries");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// (e) `ModuleConnect(h)` opens the device using that entry's `pname`.
#[tokio::test]
#[serial]
async fn module_connect_opens_device_with_the_entrys_pname() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 2 }),
        })
        .await
        .expect("module_connect should succeed");

    assert_eq!(server.backdoor.open_count(), 1);
    assert_eq!(server.backdoor.open_pname(), Some(b"USB:2".to_vec()));

    server.shutdown().await;
}

/// (f) Same-handle repeat `ModuleConnect` is a no-op: `PassThruOpen` is
/// called exactly once.
#[tokio::test]
#[serial]
async fn same_handle_repeat_module_connect_is_a_noop() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    for _ in 0..2 {
        client
            .module_connect(ModuleConnectRequest {
                module_handle: Some(ModuleHandle { module_handle: 2 }),
            })
            .await
            .expect("module_connect should succeed");
    }

    assert_eq!(server.backdoor.open_count(), 1);

    server.shutdown().await;
}

/// (g) A different-handle `ModuleConnect` while another module's device is
/// open: `FailedPrecondition`/`PDU_ERR_RESOURCE_BUSY`, `PassThruOpen` is NOT
/// called again.
#[tokio::test]
#[serial]
async fn different_handle_module_connect_while_open_is_rejected() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_connect(1) should succeed");
    assert_eq!(server.backdoor.open_count(), 1);

    let status = client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 2 }),
        })
        .await
        .expect_err("module_connect(2) should be rejected while module 1 is open");
    assert_eq!(status.code(), Code::FailedPrecondition);
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrResourceBusy as i32);
    assert_eq!(
        server.backdoor.open_count(),
        1,
        "PassThruOpen must not be called again for the rejected switch"
    );

    server.shutdown().await;
}

/// (h) An embedded-NUL `pname` in config: a startup error.
#[tokio::test]
#[serial]
async fn embedded_nul_pname_fails_startup() {
    let result = TestServer::try_start_with_extra_config(
        "\n[[config.apis.j2534-0404.libs.mock-lib.modules]]\nlabel = \"Bench 1\"\npname = \"USB:1\\u0000x\"\n",
    )
    .await;
    let err = expect_startup_err(result, "an embedded NUL byte in pname should fail startup");
    assert!(
        err.to_string().contains("NUL"),
        "error should mention the embedded NUL: {err}"
    );
}

/// (h) A non-ASCII `pname` in config: a startup error.
#[tokio::test]
#[serial]
async fn non_ascii_pname_fails_startup() {
    let result = TestServer::try_start_with_extra_config(
        "\n[[config.apis.j2534-0404.libs.mock-lib.modules]]\nlabel = \"Bench 1\"\npname = \"USB:\\u00e9\"\n",
    )
    .await;
    let err = expect_startup_err(result, "a non-ASCII pname should fail startup");
    assert!(
        err.to_string().contains("ASCII"),
        "error should mention ASCII: {err}"
    );
}

/// (i) `CreateComLogicalLink(module_handle=2)` with no prior `ModuleConnect`
/// on a 2-module config opens module 2's device -- not module 1's. Regression
/// test for the ADR-107 follow-up fix: `CreateComLogicalLink` used to
/// validate the requested `module_handle` via `require_module_handle` and
/// then discard it, calling the non-selecting `ensure_open_device()`, which
/// opened whatever was already open or defaulted to module 1
/// (`DEFAULT_MODULE_HANDLE`) -- silently opening the wrong physical device
/// for any handle other than 1.
#[tokio::test]
#[serial]
async fn create_com_logical_link_opens_the_requested_modules_device() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle { module_handle: 2 }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(j2534_0404::ISO15765),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link(module_handle=2) should succeed");

    assert_eq!(server.backdoor.open_count(), 1);
    assert_eq!(
        server.backdoor.open_pname(),
        Some(b"USB:2".to_vec()),
        "CreateComLogicalLink(module_handle=2) must open module 2's device, not module 1's"
    );

    server.shutdown().await;
}

/// (j) After `ModuleConnect(1)` opens module 1's device, a subsequent
/// `CreateComLogicalLink(module_handle=2)` is rejected with
/// `FailedPrecondition`/`PDU_ERR_RESOURCE_BUSY` rather than silently being
/// served by module 1's already-open device (same ADR-107 follow-up fix as
/// `create_com_logical_link_opens_the_requested_modules_device` above).
#[tokio::test]
#[serial]
async fn create_com_logical_link_for_a_different_open_module_is_rejected() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_connect(1) should succeed");
    assert_eq!(server.backdoor.open_count(), 1);

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle { module_handle: 2 }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(j2534_0404::ISO15765),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "create_com_logical_link(module_handle=2) should be rejected while module 1 is open",
        );
    assert_eq!(status.code(), Code::FailedPrecondition);
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrResourceBusy as i32);
    assert_eq!(
        server.backdoor.open_count(),
        1,
        "PassThruOpen must not be called again for the rejected module switch"
    );

    server.shutdown().await;
}

/// (k) After `ModuleConnect(1)` opens module 1's device, `ModuleDisconnect`
/// addressed to a DIFFERENT, in-range module (2) is rejected with
/// `FailedPrecondition`/`PDU_ERR_RESOURCE_BUSY`, and module 1's device stays
/// open -- verified by a follow-up `ModuleConnect(1)` staying a no-op
/// (`PassThruOpen` not called again). ADR-107 follow-up fix: `ModuleDisconnect`
/// used to validate only the range of `module_handle` and then unconditionally
/// tear down whatever device was actually open (Codex review, PR #110).
#[tokio::test]
#[serial]
async fn module_disconnect_for_a_different_open_module_is_rejected() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_connect(1) should succeed");
    assert_eq!(server.backdoor.open_count(), 1);

    let status = client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 2 }),
        })
        .await
        .expect_err("module_disconnect(2) should be rejected while module 1 is open");
    assert_eq!(status.code(), Code::FailedPrecondition);
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrResourceBusy as i32);

    // Module 1's device is still open: a repeat ModuleConnect(1) must stay a
    // no-op (PassThruOpen not called again).
    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_connect(1) should still succeed as a no-op");
    assert_eq!(
        server.backdoor.open_count(),
        1,
        "module 1's device must still be open -- ModuleDisconnect(2) must not have closed it"
    );

    server.shutdown().await;
}

/// (l) `ModuleDisconnect` addressed to the SAME handle as the currently open
/// module succeeds and actually closes the device -- verified by a follow-up
/// `ModuleConnect(1)` calling `PassThruOpen` again (proving the device was
/// closed, not left open).
#[tokio::test]
#[serial]
async fn module_disconnect_for_the_open_modules_own_handle_succeeds() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_connect(1) should succeed");
    assert_eq!(server.backdoor.open_count(), 1);

    client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_disconnect(1) should succeed for the currently open module's own handle");

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_connect(1) should succeed again after disconnect");
    assert_eq!(
        server.backdoor.open_count(),
        2,
        "PassThruOpen should be called again -- the device was actually closed by \
         ModuleDisconnect(1), not left open"
    );

    server.shutdown().await;
}

/// ADR-128 Codex-review round 1 (A2-23 finding): `ModuleDisconnect`'s
/// force-cleanup must purge `J2534Service::terminal_cops` for every CLL it
/// tears down, the same way an individual `DestroyComLogicalLink` does --
/// otherwise a COP that reached a terminal status before the module
/// disconnected stays wrongly "resolvable" (`CancelComPrimitive` succeeding
/// as a no-op) forever, even though its CLL (and the whole module session)
/// is now fully gone. Starts a `CoptSendrecv` that finishes immediately
/// (`num_send_cycles = 1`, `num_receive_cycles = 0`, no response wait),
/// polls `GetStatus` until it reports `Finished`, then disconnects the
/// module and confirms `CancelComPrimitive` on that same `cop_handle` now
/// fails with `PDU_ERR_INVALID_HANDLE` -- before this fix it would have
/// wrongly succeeded, since the stale `terminal_cops` entry survived the
/// teardown untouched.
#[tokio::test]
#[serial]
async fn module_disconnect_purges_terminal_cops_for_every_torn_down_cll() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01, 0x02],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    // Poll GetStatus until Finished (bounded, mirrors cop_ctrl_cycles.rs's
    // wait_for_cop_status helper -- inlined here rather than shared, since
    // this file has no existing COP-status polling helper of its own).
    let mut finished = false;
    for _ in 0..200 {
        let status = client
            .get_status(GetStatusRequest {
                handle: Some(get_status_request::Handle::CopHandle(cop_handle)),
            })
            .await
            .expect("get_status(COP) should succeed")
            .into_inner();
        if let Some(status_response::Status::CopStatus(s)) = status.status
            && s == PduComPrimitiveStatus::PduCopstFinished as i32
        {
            finished = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(finished, "the single-cycle COP should reach Finished");

    client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_disconnect should succeed");

    let err = client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect_err(
            "cancelling a cop_handle whose module (and CLL) has been fully torn down must fail",
        );
    assert_eq!(err.code(), Code::NotFound);

    server.shutdown().await;
}

/// (m) With nothing open at all, `ModuleDisconnect` addressed to an in-range
/// (but not currently open) module handle succeeds as a no-op -- unchanged
/// pre-existing behavior for the nothing-open case.
#[tokio::test]
#[serial]
async fn module_disconnect_with_nothing_open_is_a_noop() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 2 }),
        })
        .await
        .expect("module_disconnect(2) should succeed as a no-op when nothing is open");
    assert_eq!(
        server.backdoor.open_count(),
        0,
        "PassThruOpen should never have been called"
    );

    server.shutdown().await;
}

/// ADR-107 addendum (Codex review P1 finding on PR #110): `j1850_bus_flavor`
/// is a device-derived probe cache (ADR-070) -- it must NOT survive a
/// `ModuleDisconnect`/`ModuleConnect` switch to a DIFFERENT physical device,
/// or the second device would silently inherit the first device's stale
/// probed flavor instead of being probed itself. Connects module 1, runs the
/// SAE_J1850 auto-detect probe on it (populating the cache), disconnects,
/// connects module 2, and confirms a bus-agnostic SAE_J1850 CLL on module 2
/// triggers its OWN fresh probe (more `PassThruConnect` calls) rather than
/// reusing module 1's cached conclusion -- mirroring
/// `j1850_autodetect.rs::inconclusive_fallback_is_not_cached_and_a_later_active_probe_still_runs`'s
/// connect-count-delta technique for detecting a skipped-vs-real probe.
#[tokio::test]
#[serial]
async fn module_switch_reprobes_j1850_flavor_instead_of_reusing_stale_cache() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850VPW));

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_connect(1) should succeed");

    // Bus-agnostic SAE_J1850 CLL on module 1: runs the real probe (1 probe
    // candidate connect + 1 real connect, since the VPW candidate is
    // conclusive here) and populates `j1850_bus_flavor`.
    let _cll1 = create_and_connect_cll_for_module(&mut client, 1, 0x021C, &[]).await;
    let connects_after_module1 = server.backdoor.connect_count();
    assert_eq!(
        connects_after_module1, 2,
        "module 1's first bus-agnostic CLL should probe (1 candidate connect) then real-connect \
         (1 more) -- sanity check that the probe actually ran"
    );

    client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_disconnect(1) should succeed");

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 2 }),
        })
        .await
        .expect("module_connect(2) should succeed");
    assert_eq!(
        server.backdoor.open_count(),
        2,
        "module 2 should have opened its own device (PassThruOpen called again)"
    );

    // A bus-agnostic SAE_J1850 CLL on module 2 must run its OWN probe --
    // if `j1850_bus_flavor` had survived the switch, this would skip
    // straight to a single real connect (delta of 1, not 2).
    let _cll2 = create_and_connect_cll_for_module(&mut client, 2, 0x021C, &[]).await;
    let connects_after_module2 = server.backdoor.connect_count();
    assert_eq!(
        connects_after_module2 - connects_after_module1,
        2,
        "module 2's first bus-agnostic CLL must re-run the full probe (1 candidate connect + 1 \
         real connect), not reuse module 1's cached flavor (which would cost only 1 connect)"
    );

    server.shutdown().await;
}

/// The `resolved_can_channel_mode` counterpart to
/// `module_switch_reprobes_j1850_flavor_instead_of_reusing_stale_cache`
/// above: `can_channel_mode = "auto"`'s dual/single-channel capability probe
/// (ADR-046 addendum) is the other device-derived cache that must reset on a
/// module switch. Mirrors `can_mode.rs`'s own
/// `auto_can_channel_mode_resolves_to_dual_channel_when_capable`'s
/// connect-count-delta technique (2 connects for a real probe: 1 probe-open
/// + 1 real connect) for detecting whether the probe actually ran.
#[tokio::test]
#[serial]
async fn module_switch_reprobes_can_channel_mode_instead_of_reusing_stale_cache() {
    let server = TestServer::try_start_with_extra_config(&format!(
        "can_channel_mode = \"auto\"\n{}",
        modules_toml(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")])
    ))
    .await
    .expect("service should initialize with a valid modules + auto can_channel_mode config");
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_connect(1) should succeed");

    // `create_and_connect_cll_for_module`'s own `ConnectComLogicalLink` runs
    // the dual-channel capability probe automatically right after the
    // primary channel connects (`auto_can_channel_mode_resolves_to_dual_channel_when_capable`'s
    // own `connect_count() == 2` assertion, immediately after its
    // `create_and_connect_cll` call, established this same timing).
    assert_eq!(server.backdoor.connect_count(), 0);
    let _cll1 = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let connects_after_module1 = server.backdoor.connect_count();
    assert_eq!(
        connects_after_module1, 2,
        "module 1's first ISO15765 CLL should run the real connect (1) plus the dual-channel \
         capability probe's own open+close (1 more) -- sanity check that the probe actually ran"
    );

    client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_disconnect(1) should succeed");

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 2 }),
        })
        .await
        .expect("module_connect(2) should succeed");
    assert_eq!(
        server.backdoor.open_count(),
        2,
        "module 2 should have opened its own device (PassThruOpen called again)"
    );

    // A fresh ISO15765 CLL on module 2 must run its OWN capability probe --
    // if `resolved_can_channel_mode` had survived the switch, this would
    // skip straight to a single real connect (delta of 1, not 2).
    let _cll2 = create_and_connect_cll_for_module(
        &mut client,
        2,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let connects_after_module2 = server.backdoor.connect_count();
    assert_eq!(
        connects_after_module2 - connects_after_module1,
        2,
        "module 2's first ISO15765 CLL must re-run the capability probe (real connect + probe \
         open/close), not reuse module 1's cached resolution (which would cost only 1 connect)"
    );

    server.shutdown().await;
}

/// Installs a single `PDU_FLT_BLOCK` message filter (`FilterNumber` 1) on
/// `cll_handle` via `PDU_IOCTL_START_MSG_FILTER`, so a subsequent
/// `PDU_IOCTL_RESET` has live per-CLL state to (not) touch.
async fn install_block_filter(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) {
    let start_filter_id = resolve_ioctl_id(client, "PDU_IOCTL_START_MSG_FILTER").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("installing FilterNumber 1 should succeed");
}

/// (n) After `ModuleConnect(1)` opens module 1's device and a CLL on module 1
/// has a live filter installed, `PDU_IOCTL_RESET` addressed to a DIFFERENT,
/// in-range module (2) is rejected with `FailedPrecondition`/
/// `PDU_ERR_RESOURCE_BUSY`, and module 1's live state is left untouched --
/// verified by the installed filter's hardware `PassThruStopMsgFilter` call
/// count staying at zero. ADR-107 follow-up fix: `PDU_IOCTL_RESET` used to
/// validate only the range of its `module_handle` and then discard it,
/// always resetting whichever module was actually open (Codex review, PR
/// #110).
#[tokio::test]
#[serial]
async fn ioctl_reset_for_a_different_open_module_is_rejected() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_connect(1) should succeed");

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    install_block_filter(&mut client, cll_handle).await;
    let stop_filter_count_before = server.backdoor.stop_filter_count();

    let reset_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESET").await;
    let status = io_ctl_module(&mut client, 2, reset_id)
        .await
        .expect_err("PDU_IOCTL_RESET(module_handle=2) should be rejected while module 1 is open");
    assert_eq!(status.code(), Code::FailedPrecondition);
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrResourceBusy as i32);
    assert_eq!(
        server.backdoor.stop_filter_count(),
        stop_filter_count_before,
        "module 1's live filter must be untouched by a PDU_IOCTL_RESET rejected for module 2"
    );

    server.shutdown().await;
}

/// (o) `PDU_IOCTL_RESET` addressed to the SAME handle as the currently open
/// module succeeds and resets that module's live state as today -- verified
/// by the installed filter actually being stopped.
#[tokio::test]
#[serial]
async fn ioctl_reset_for_the_open_modules_own_handle_succeeds() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_connect(1) should succeed");

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    install_block_filter(&mut client, cll_handle).await;
    let stop_filter_count_before = server.backdoor.stop_filter_count();

    let reset_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESET").await;
    io_ctl_module(&mut client, 1, reset_id).await.expect(
        "PDU_IOCTL_RESET(module_handle=1) should succeed for the currently open module's \
                 own handle",
    );
    assert!(
        server.backdoor.stop_filter_count() > stop_filter_count_before,
        "PDU_IOCTL_RESET(module_handle=1) should have stopped the installed filter"
    );

    server.shutdown().await;
}

/// `GetStatus(ModuleHandle)`, unwrapped to the bare `PduModuleStatus`
/// (ADR-132 Amendment 3 -- `rpc_get_status`'s `ModuleHandle` branch).
async fn module_status(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    module_handle: u32,
) -> PduModuleStatus {
    let response = client
        .get_status(GetStatusRequest {
            handle: Some(get_status_request::Handle::ModuleHandle(ModuleHandle {
                module_handle,
            })),
        })
        .await
        .expect("get_status(ModuleHandle) should succeed")
        .into_inner();
    match response.status {
        Some(status_response::Status::ModuleStatus(s)) => PduModuleStatus::try_from(s)
            .unwrap_or_else(|_| panic!("unexpected PduModuleStatus value {s}")),
        other => panic!("get_status(ModuleHandle) returned unexpected status: {other:?}"),
    }
}

/// (f) ADR-132 Amendment 3 / Codex review P1: with nothing open,
/// `GetStatus(ModuleHandle)` must agree with `GetModuleIds` -- both report
/// `PDU_MODST_AVAIL` for every configured handle, not `GetStatus` falling
/// back to `module_state`'s default `PDU_MODST_READY`.
#[tokio::test]
#[serial]
async fn get_status_module_handle_matches_get_module_ids_when_nothing_is_open() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    for handle in [1, 2] {
        assert_eq!(
            module_status(&mut client, handle).await,
            PduModuleStatus::PduModstAvail,
            "GetStatus(module_handle={handle}) must report AVAIL, matching GetModuleIds, \
             when nothing is open"
        );
    }

    server.shutdown().await;
}

/// (g) ADR-132 Amendment 3 / Codex review P1: with module 1 open,
/// `GetStatus(ModuleHandle)` must agree with `GetModuleIds` per-handle --
/// the open handle reports the real tracked status, every other handle
/// stays `PDU_MODST_AVAIL`, not the shared `module_state.status` leaking
/// onto an unrelated, unopened handle.
#[tokio::test]
#[serial]
async fn get_status_module_handle_matches_get_module_ids_for_the_open_handle_only() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await
        .expect("module_connect(1) should succeed");

    assert_eq!(
        module_status(&mut client, 1).await,
        PduModuleStatus::PduModstReady,
        "GetStatus(module_handle=1) must report the real tracked status for the open handle"
    );
    assert_eq!(
        module_status(&mut client, 2).await,
        PduModuleStatus::PduModstAvail,
        "GetStatus(module_handle=2) must stay AVAIL -- it is not the open handle, so it must \
         not inherit handle 1's tracked status"
    );

    let module_ids = client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("get_module_ids should succeed")
        .into_inner();
    let rows = module_ids
        .module_id_list
        .expect("module_id_list should be present")
        .module_data;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].module_status,
        PduModuleStatus::PduModstReady as i32,
        "GetModuleIds row for handle 1 must match GetStatus(module_handle=1)"
    );
    assert_eq!(
        rows[1].module_status,
        PduModuleStatus::PduModstAvail as i32,
        "GetModuleIds row for handle 2 must match GetStatus(module_handle=2)"
    );

    server.shutdown().await;
}

/// ADR-105 backlog (`docs/implementation-notes.md`): a hard channel error
/// detected by the background poll task (`poll_rx_inner` ->
/// `handle_channel_hard_error`, triggered when `PassThruReadMsgs` fails with
/// anything other than `ERR_BUFFER_EMPTY`) must land in
/// `LogicalLinkState::last_error` -- and therefore in a later failing
/// CLL-scoped RPC's `ErrorDetail.error_event_data` -- with no `SubscribeEvent`
/// listener required to observe it. Uses the new `__mock_set_read_msgs_error`
/// backdoor to force a deterministic, non-buffer-empty `PassThruReadMsgs`
/// failure, then waits (via an already-open `SubscribeEvent` stream, not a
/// raw sleep) for the resulting `PDU_CLLST_OFFLINE` transition --
/// `handle_channel_hard_error` records this CLL's `last_error` (inside
/// `send_error_event`) strictly before it sends that same CLL's
/// `PDU_CLLST_OFFLINE` notification, so observing the event on the
/// subscription proves the tracked error is already committed. A subsequent,
/// unrelated `StartComPrimitive` on the now-disconnected CLL is then expected
/// to fail with `PDU_ERR_CLL_NOT_CONNECTED`, carrying the already-tracked
/// `PDU_ERR_EVT_LOST_COMM_TO_VCI` in its `ErrorDetail.error_event_data`.
#[tokio::test]
#[serial]
async fn hard_channel_error_populates_error_event_data_on_a_later_unrelated_rpc() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    server.backdoor.set_read_msgs_error(Some(
        j2534_0404::ERR_DEVICE_NOT_CONNECTED as std::os::raw::c_long,
    ));

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOffline as i32
        ))
        .await,
        "expected the background poll task's hard-channel-error path to take this CLL offline"
    );

    let err = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "start_com_primitive on a CLL whose channel just suffered a hard error must fail",
        );
    assert_eq!(err.code(), Code::FailedPrecondition);

    let detail = error_detail_from_status(&err).expect("ErrorDetail should be attached");
    assert_eq!(detail.pdu_error, PduError::PduErrCllNotConnected as i32);
    let event_data = detail
        .error_event_data
        .expect("error_event_data should be populated from the already-tracked hard error");
    assert_eq!(
        event_data.error_event,
        PduErrorEvent::PduErrEvtLostCommToVci as i32,
        "the tracked error must be the hard-channel-error's own PDU_ERR_EVT_LOST_COMM_TO_VCI"
    );
    assert!(
        event_data.cop_handle.is_none(),
        "a CLL-scoped hard-channel-error must not fabricate a ComPrimitiveHandle"
    );

    drop(events);
    server.shutdown().await;
}
