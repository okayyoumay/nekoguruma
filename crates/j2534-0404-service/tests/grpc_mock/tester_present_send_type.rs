//! ADR-083: `CP_TesterPresentTime` microsecond-to-millisecond conversion
//! (matching the ADR-072 `us_to_ms` policy applied to every other
//! microsecond-denominated timing ComParam), `CP_TesterPresentSendType`'s
//! two dispatch modes (`0` = periodic, `1` = idle-triggered -- both
//! software-driven as of this diff; mode 0 was previously hardware-
//! autonomous via `PassThruStartPeriodicMsg`), and the mode-0 arm-time send's
//! `CP_P3Func`/`CP_P3Phys` gate.

use serial_test::serial;
use vci_service_interface::{
    CancelComPrimitiveRequest, ComLogicalLinkHandle, ComPrimitiveCtrlData,
    ConnectComLogicalLinkRequest, DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest,
    ExpectedResponseData, GetObjectIdRequest, IoCtlRequest, ObjectType, StartComPrimitiveRequest,
    event_item, io_ctl_request, subscribe_event_request, vci_service_client::VciServiceClient,
};

use crate::harness::*;

const CP_P3_FUNC: u32 = 0x80B3;
const CP_P3_PHYS: u32 = 0x80B4;

/// Waits for the next `PduCopstFinished` on an already-open event stream.
/// The stream must be subscribed BEFORE the COP being waited on is started,
/// so its `PduCopstFinished` cannot be missed.
async fn wait_for_cop_finished(
    events: &mut tonic::Streaming<vci_service_interface::EventNotification>,
) {
    assert!(
        wait_for_event(events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "expected a PduCopstFinished event"
    );
}

/// Starts a `CoptSendrecv` with explicit `NumReceiveCycles` and returns
/// immediately (does not wait for completion) -- mirrors `p3_gap.rs`'s
/// `start_send_recv`, duplicated locally so this file's idle-timer-reset
/// timing assertions are not coupled to another test module's helper.
async fn start_send_recv(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
    cop_data: Vec<u8>,
    num_receive_cycles: i32,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles,
                temp_param_update: 0,
                expected_response_array: if num_receive_cycles == 0 {
                    vec![]
                } else {
                    vec![ExpectedResponseData {
                        response_type: 0,
                        acceptance_id: 0,
                        mask_data: vec![0x00],
                        pattern_data: vec![0x00],
                        unique_resp_ids: vec![],
                    }]
                },
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
}

async fn start_comm(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) -> vci_service_interface::ComPrimitiveHandle {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
}

/// Like `wait_for_cop_finished`, but qualified to `cop_handle` -- for callers
/// where an earlier, unrelated COP (e.g. `set_can_phys_req_id_and_promote`'s
/// own `CoptUpdateparam`, or a preceding `start_send_recv`, both started and
/// finished before the subscriber attached) has its own backlogged
/// `PduCopstFinished` sitting ahead of the COP under test in the same
/// live-flush FIFO (P2 backlog follow-up, `docs/implementation-notes.md`),
/// which an unqualified wait would match first.
async fn wait_for_cop_finished_matching(
    events: &mut tonic::Streaming<vci_service_interface::EventNotification>,
    cop_handle: vci_service_interface::ComPrimitiveHandle,
) {
    assert!(
        wait_for_event(events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cop_handle.cop_handle))
        .await,
        "expected a PduCopstFinished event for cop_handle {cop_handle:?}"
    );
}

/// Issues `CoptStopcomm` on `cll_handle` and waits for `PduCopstFinished` --
/// needed to legally re-arm via a second `CoptStartcomm`, since
/// `rpc_start_com_primitive` rejects `CoptStartcomm` on an already-started
/// CLL (`comm_started` is left `true` by `PDU_IOCTL_CLEAR_PERIODIC_MSGS`,
/// which only clears software `tester_present_state`).
async fn stop_comm(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
    events: &mut tonic::Streaming<vci_service_interface::EventNotification>,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed");
    wait_for_cop_finished(events).await;
}

/// Issues `PDU_IOCTL_CLEAR_PERIODIC_MSGS` on `cll_handle` -- transitions this
/// CLL's `tester_present_state` from `Armed` to `Cleared` (or leaves
/// `None`/`Cleared` untouched, per `rpc_misc.rs`'s handler).
async fn clear_periodic_msgs(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) {
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                j2534_0404::CLEAR_PERIODIC_MSGS,
            )),
            input_data: None,
            has_output: false,
        })
        .await
        .expect("io_ctl(CLEAR_PERIODIC_MSGS) should succeed");
}

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)`, mirroring `pdu_ioctl.rs`'s helper of the
/// same name (duplicated locally so this file's ADR-095 audit-round tests
/// are not coupled to another test module's helper).
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

/// Issues `IoCtl` for `cmd_id` against `cll_handle`, mirroring `pdu_ioctl.rs`'s
/// helper of the same name.
async fn io_ctl_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cmd_id: u32,
) -> Result<(), tonic::Status> {
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: None,
            has_output: false,
        })
        .await
        .map(|_| ())
}

/// Polls the mock's written-message log on `channel_id` until a frame whose
/// first 4 bytes (the CAN ID header) equal `can_id` has been written, or
/// panics after ~2s. Used instead of `wait_for_written_count` wherever two
/// CLLs share a physical channel and a sibling CLL's own tester-present
/// sends can interleave with the specific write a test is waiting for,
/// making a raw count fragile.
async fn wait_for_can_id_frame(server: &TestServer, channel_id: u32, can_id: u32) {
    let expected = can_id.to_be_bytes();
    for _ in 0..200 {
        let total = server.backdoor.written_count(channel_id);
        for i in 0..total {
            if server.backdoor.written_data(channel_id, i)[..4] == expected {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("expected a written frame with CAN ID {can_id:#x} on channel {channel_id}");
}

/// A non-default, non-round `CP_TesterPresentTime` (microseconds) converts to
/// the nearest millisecond via `us_to_ms` (ADR-072/ADR-083) and governs the
/// software due-check cadence for mode 0 (`CP_TesterPresentSendType = 0`,
/// left at its default here) at that resolution -- 123_500 us rounds to
/// 124 ms, not the raw 123_500 (the pre-ADR-083 bug, originally observed via
/// `PassThruStartPeriodicMsg`'s now-removed `TimeInterval` argument -- mode 0
/// is software-driven as of this diff, so the same conversion is now proven
/// via the second software send's timing instead). `start_periodic_count()`
/// stays `0` throughout: this service no longer calls
/// `PassThruStartPeriodicMsg` at all (ADR-083/this diff).
#[tokio::test]
#[serial]
async fn tester_present_time_us_converts_to_software_due_check_ms() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    // Functional addressing avoids needing a UniqueRespIdTable entry
    // (ADR-054). Staged to Working -- promote to Active (ADR-067/068) so
    // CoptStartcomm's call-time Active snapshot actually sees it.
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 123_500).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Anchored before `start_comm` itself, same reasoning as
    // `send_type_1_no_periodic_start_and_fires_one_shot_after_idle`: the
    // immediate arm-time send happens internally at essentially this same
    // instant.
    let started = std::time::Instant::now();
    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server.backdoor.start_periodic_count(),
        0,
        "mode 0 must not start a hardware periodic message (ADR-083/this diff)"
    );

    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.push(0x3E);

    // ADR-084 (extended to mode 0 by this diff): CoptStartcomm's own arm
    // sends the first frame immediately, synchronously, before
    // PduCopstFinished.
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "CoptStartcomm should have sent the first tester-present frame immediately"
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    // The SECOND frame is mode 0's own due-triggered send, appearing once
    // the CP_TesterPresentTime-derived 124ms interval (123_500us via
    // us_to_ms) has elapsed since the first send -- not the raw,
    // unconverted 123_500ms a pre-ADR-083 bug would have used.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(100),
        "the second (mode-0 due-triggered) tester-present send fired too early for a 124ms \
         interval (waited {elapsed:?})"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(2000),
        "the second tester-present send took far longer than the configured 124ms interval \
         (waited {elapsed:?}) -- the CP_TesterPresentTime us-to-ms conversion may not have taken \
         effect"
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 1), expected);
    assert_eq!(
        server.backdoor.start_periodic_count(),
        0,
        "mode 0 must never start a hardware periodic message, even after its second due-triggered \
         send"
    );

    drop(events);
    server.shutdown().await;
}

/// A non-zero `CP_TesterPresentTime` that rounds to 0 ms (sub-500 us) is
/// rejected synchronously at `StartComPrimitive` (`INVALID_ARGUMENT`) rather
/// than silently collapsing the configured keep-alive into "disabled"
/// (ADR-083). The raw stored `0` sentinel ("disabled") is unaffected --
/// covered separately by the "no tester-present configured" behavior
/// elsewhere in this suite.
#[tokio::test]
#[serial]
async fn tester_present_time_sub_500us_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // 499 us: non-zero, but (499 + 500) / 1000 == 0 ms.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 499).await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (disabled), which now short-circuits `resolve_tester_present` before
    // this validation ever runs -- must be explicitly enabled so this test
    // still exercises the interval-rounding rejection it's meant to cover.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err("CoptStartcomm should reject a CP_TesterPresentTime that rounds to 0 ms");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(server.backdoor.start_periodic_count(), 0);

    server.shutdown().await;
}

/// `CP_TesterPresentSendType` only has two defined ISO 22900-2 values (`0`:
/// periodic, `1`: idle-triggered). `SetComParam` does not range-check it, so
/// an out-of-range value (`2`) must be rejected synchronously at
/// `StartComPrimitive` (`INVALID_ARGUMENT`) rather than silently falling into
/// the dispatch logic's idle-mode (`else`) branch (ADR-083 round-11 review).
#[tokio::test]
#[serial]
async fn tester_present_send_type_out_of_range_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 2).await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (disabled), which now short-circuits `resolve_tester_present` before
    // this validation ever runs -- must be explicitly enabled so this test
    // still exercises the send-type-range rejection it's meant to cover.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err("CoptStartcomm should reject an out-of-range CP_TesterPresentSendType");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(server.backdoor.start_periodic_count(), 0);

    server.shutdown().await;
}

/// `CP_TesterPresentSendType = 1` (idle-triggered): `CoptStartcomm` starts no
/// hardware periodic message. `CoptStartcomm` itself sends the first
/// tester-present frame immediately, synchronously, before completing
/// (ADR-084); a SECOND, idle-triggered one-shot write matching the same
/// payload then appears only once the configured interval has elapsed with
/// no other bus traffic. Uses the `ISO_14230_3_on_ISO_15765_2` resource
/// (0x0204) -- whose preset defaults `CP_TesterPresentSendType` to `1` -- to
/// prove this is not exercised only via manual ComParam overrides (ADR-083's
/// motivating real-world preset).
#[tokio::test]
#[serial]
async fn send_type_1_no_periodic_start_and_fires_one_shot_after_idle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, 0x0204, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    // Functional addressing (CP_CanFuncReqId defaults to 0x7DF from the
    // preset) avoids needing a UniqueRespIdTable entry.
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    // Shrink the preset's 2s default interval so the test does not have to
    // wait that long. 300ms (widened from an originally-tried 50ms,
    // Codex-review fix) gives generous headroom above the measured
    // `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS` RPC/event round-trip
    // overhead so the immediate-send check below can safely assert `== 1`
    // rather than a weaker `>= 1` that would no longer distinguish a genuine
    // synchronous arm-time send from a regressed, timer-only one racing the
    // same check.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Anchored before `start_comm` itself (not after `wait_for_cop_finished`
    // returns): the immediate arm-time send happens internally at essentially
    // this same instant, but the client only observes `PduCopstFinished`
    // (and thus learns the immediate send has happened) after an additional
    // event-propagation/gRPC round trip -- anchoring after that round trip
    // would eat an unpredictable slice of the second frame's own idle window
    // and make the "not fired too early" check below flaky.
    let started = std::time::Instant::now();
    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server.backdoor.start_periodic_count(),
        0,
        "CP_TesterPresentSendType = 1 must not start a hardware periodic message"
    );

    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.push(0x3E);

    // ADR-084: CoptStartcomm's own arm sends the first frame immediately,
    // synchronously, before PduCopstFinished -- so by the time
    // wait_for_cop_finished returns, exactly one frame has gone out. This
    // must assert `== 1`, not a weaker `>= 1`: if `CoptStartcomm` ever
    // regressed to only sending on the idle timer, and `wait_for_cop_finished`
    // took longer than the configured interval, a `>= 1` check would still
    // pass on that delayed frame and the payload check below would still
    // match its content, silently no longer distinguishing the synchronous
    // arm-time send this asserts from a regressed timer-only one
    // (Codex-review finding). The 300ms interval above gives `== 1` enough
    // margin over the measured `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS`
    // of RPC/event round-trip overhead `wait_for_cop_finished` can take, so
    // this stays a meaningful check rather than racing it.
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "CoptStartcomm should have sent the first tester-present frame immediately, not waited a \
         full idle interval"
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    // The SECOND frame is the idle-triggered one-shot, appearing only once
    // the configured interval has elapsed (from the first, immediate send)
    // with no other bus traffic.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = started.elapsed();
    // 225ms is 75% of the 300ms window, matching the ratio used by this
    // file's other idle-timer tests.
    assert!(
        elapsed >= std::time::Duration::from_millis(225),
        "the second (idle-triggered) tester-present send fired too early for a 300ms idle window \
         (waited {elapsed:?})"
    );
    // 1500ms sits comfortably above the ~300-400ms expected under load, but
    // well below the un-overridden preset default (2s) -- still catches a
    // regression where CP_TesterPresentTime's override silently fails to
    // take effect and the CLL falls back to that default.
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "the second (idle-triggered) tester-present send took far longer than the configured \
         300ms idle window (waited {elapsed:?}) -- the CP_TesterPresentTime override may not have \
         taken effect"
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 1), expected);

    drop(events);
    server.shutdown().await;
}

/// Same idle-triggered behavior as
/// `send_type_1_no_periodic_start_and_fires_one_shot_after_idle`, but via an
/// explicit `SetComParam(CP_TesterPresentSendType, 1)` override on a plain
/// `ISO15765` CLL rather than a preset default -- confirms mode 1 is honored
/// independent of which resource/preset it came from (ADR-083).
#[tokio::test]
#[serial]
async fn send_type_1_manual_override_on_iso15765_fires_one_shot_after_idle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 50_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(server.backdoor.start_periodic_count(), 0);
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.push(0x3E);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    drop(events);
    server.shutdown().await;
}

/// `CP_TesterPresentSendType = 1`: a live `CoptUpdateparam` that promotes an
/// entirely unrelated ComParam (`CP_Loopback`) on an already comm-started
/// mode-1 CLL must NOT re-send a tester-present frame or reset the idle
/// clock -- only a `CoptUpdateparam` that actually changes tester-present's
/// own resolved on-wire behavior should do that (ADR-084). An earlier draft
/// of the live re-arm re-resolved and re-armed unconditionally on every
/// successful promotion, which this test pins against regressing.
#[tokio::test]
#[serial]
async fn send_type_1_unrelated_updateparam_does_not_resend_or_rearm() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, 0x0204, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let started = std::time::Instant::now();
    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // An unrelated ComParam promotion: LOOPBACK has no bearing on
    // tester-present's resolved data/interval/tx_flags/framing/send_type.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::LOOPBACK, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "an unrelated CoptUpdateparam must not re-send a tester-present frame"
    );

    // The idle-triggered second frame must still arrive on the ORIGINAL
    // 300ms schedule (anchored to the immediate first send), not be reset by
    // the unrelated promotion above.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = started.elapsed();
    assert!(
        (250..800).contains(&elapsed.as_millis()),
        "the second frame should fire ~300ms after the original arm, not be reset by the \
         unrelated CoptUpdateparam (waited {elapsed:?})"
    );

    drop(events);
    server.shutdown().await;
}

/// `CP_TesterPresentSendType = 1`: a live `CoptUpdateparam` that DOES change
/// tester-present's own resolved on-wire behavior (here, `CP_TesterPresentTime`)
/// on an already comm-started mode-1 CLL immediately sends a frame and
/// re-arms from the new configuration -- the positive counterpart to
/// `send_type_1_unrelated_updateparam_does_not_resend_or_rearm` (ADR-084).
#[tokio::test]
#[serial]
async fn send_type_1_relevant_updateparam_resends_and_rearms() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, 0x0204, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A genuinely tester-present-relevant change: a different
    // CP_TesterPresentMessage changes the resolved `data`. The interval is
    // deliberately left at the original 300ms (rather than shortened) so the
    // natural next idle-triggered fire cannot land inside
    // `promote_via_update_param`'s own ~100ms wait and inflate the count.
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x01],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a CoptUpdateparam that changes tester-present's resolved behavior should immediately \
         send a frame and re-arm, the same \"just became enabled\" contract CoptStartcomm gives"
    );
    let mut expected_new = 0x7DF_u32.to_be_bytes().to_vec();
    expected_new.push(0x01);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_new,
        "the re-armed send should carry the newly-promoted message content"
    );

    drop(events);
    server.shutdown().await;
}

/// `CP_TesterPresentSendType = 1`: other bus traffic (a `CoptSendrecv`)
/// shortly before the idle deadline resets the shared channel's
/// `last_bus_activity` clock, pushing the one-shot tester-present send out
/// further rather than letting it fire on the original schedule.
#[tokio::test]
#[serial]
async fn send_type_1_idle_timer_resets_on_prior_bus_traffic() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, 0x0204, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    // 300ms (widened from an originally-tried 80ms) gives generous absolute
    // headroom against RPC/event round-trip overhead -- see the anchored
    // `sleep_until` deadlines below, mirroring the stable fix pattern from
    // `kline_five_baud_init_stamps_last_bus_activity_deferring_mode_1_sibling`.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // `started` anchors the `sleep_until` deadlines below to (just before)
    // arm, so RPC/event round-trip overhead already spent by the time each
    // deadline is reached doesn't erode the margins asserted further down,
    // the way summing blind relative sleeps did previously.
    let started = tokio::time::Instant::now();
    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // ADR-084: CoptStartcomm's own arm already sent the first tester-present
    // frame immediately, synchronously, before PduCopstFinished.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // At t=150ms since `started` (half of the 300ms window), send an ordinary
    // CoptSendrecv (same functional addressing) -- this is bus activity too,
    // and must reset the idle clock.
    tokio::time::sleep_until(started + tokio::time::Duration::from_millis(150)).await;
    let reset_at = tokio::time::Instant::now();
    start_send_recv(&mut client, cll_handle, vec![0x01, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // At t=380ms since `started` -- 80ms past the ORIGINAL 300ms deadline
    // from arm, but well before a new 300ms window counted from `reset_at`
    // (~150ms + 300ms = ~450ms since `started`) -- the tester-present send
    // must still not have fired.
    tokio::time::sleep_until(started + tokio::time::Duration::from_millis(380)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "the idle timer should have been reset by the CoptSendrecv, delaying the tester-present send"
    );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let elapsed = reset_at.elapsed();
    // 225ms is 75% of the 300ms window, matching the reference test's ratio.
    assert!(
        elapsed >= tokio::time::Duration::from_millis(225),
        "the tester-present send should fire ~300ms after the reset, not the original deadline (waited {elapsed:?})"
    );

    drop(events);
    server.shutdown().await;
}

/// This diff (unifying mode 0 onto the same software send path as mode 1,
/// folding in ADR-083's `count_as_bus_activity = false` rationale for mode 0
/// too): a mode-0 (`CP_TesterPresentSendType = 0`) CLL's own arm-time send
/// must NOT stamp the shared per-physical-channel `last_bus_activity` clock
/// -- unlike the pre-this-diff, hardware-autonomous `PassThruStartPeriodicMsg`
/// behavior this test used to pin (a successful periodic start used to be
/// treated as genuine external bus activity, deferring a mode-1 sibling's
/// idle window). Now that mode 0 goes through the identical
/// `transmit_request(.., count_as_bus_activity = false, ..)` call mode 1
/// already used, cll_b's mode-0 arm-time send must NOT defer cll_a's
/// mode-1 idle-triggered send -- mirrors
/// `send_type_1_sibling_cll_arming_does_not_reset_another_clls_idle_window`,
/// substituting a real mode-0 send (not just a no-op arm) as the would-be
/// activity source. `start_periodic_count()` stays `0` throughout: this
/// service no longer calls `PassThruStartPeriodicMsg` at all.
#[tokio::test]
#[serial]
async fn mode_0_own_send_does_not_stamp_last_bus_activity_for_mode_1_sibling() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a: mode-1 (idle-triggered), armed first with a 150ms interval
    // (matching `send_type_1_sibling_cll_arming_does_not_reset_another_clls_idle_window`'s
    // own margin, which this test otherwise mirrors) -- a large enough
    // absolute window that ordinary test-harness scheduling jitter cannot
    // close the gap between "fired on schedule" (~150ms) and "would have
    // been reset" (~225ms).
    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 150_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // cll_b: mode-0 (the default send type), sharing cll_a's physical
    // channel. No CP_P3Func/CP_P3Phys is seeded, so its arm-time send is not
    // gated -- no P3 wait to confound the timing asserted below.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_b, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_b, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    // Deliberately huge: cll_b's own (mode-0) due-triggered re-send must not
    // enter the picture again during this test -- only its one arm-time send
    // is under test.
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    // cll_b is a joining CLL on an already-open shared channel, so its
    // Working set does not auto-promote to Active at Connect (only the
    // channel-creating CLL gets that) -- CoptStartcomm always resolves
    // tester-present from the call-time Active snapshot (ADR-067 claim C),
    // so an explicit promotion is required here for cll_b's mode-0 arm to
    // actually resolve. Done well before the timing-sensitive section below,
    // so its ~100ms settle sleep does not eat into the asserted windows.
    promote_via_update_param(&mut client, cll_b).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let started = std::time::Instant::now();
    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;

    // ADR-084: cll_a's own CoptStartcomm arm already sent its first
    // tester-present frame immediately, synchronously, before
    // PduCopstFinished -- essentially at `started`, since nothing gates it.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Halfway through cll_a's 150ms window, start cll_b's mode-0 arm-time
    // send -- pre-this-diff, this used to be genuine external bus activity
    // that reset cll_a's idle clock (`mode_0_periodic_start_stamps_last_bus_
    // activity_deferring_mode_1_sibling`, this test's predecessor); as of
    // this diff it must NOT.
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    start_comm(&mut client, cll_b).await;
    wait_for_cop_finished(&mut events_b).await;
    assert_eq!(
        server.backdoor.start_periodic_count(),
        0,
        "mode 0 must not start a hardware periodic message (ADR-083/this diff)"
    );
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let mut expected_b = 0x7DF_u32.to_be_bytes().to_vec();
    expected_b.push(0x3E);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_b,
        "cll_b's own arm-time send should be the second frame on the wire"
    );

    // cll_a's own SECOND (idle-triggered) send should still fire ~150ms after
    // its own arm (`started`), NOT ~150ms after cll_b's later send at
    // started+75ms (which would put it at ~225ms).
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(210),
        "cll_a's idle-triggered send should fire ~150ms after its own arm, not ~225ms after \
         cll_b's later mode-0 send (waited {elapsed:?})"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(130),
        "cll_a's idle-triggered send fired too early for a 150ms idle window (waited {elapsed:?})"
    );
    let mut expected_a = 0x7DF_u32.to_be_bytes().to_vec();
    expected_a.push(0x11);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2),
        expected_a,
        "the frame that fired should be cll_a's tester-present message, not cll_b's"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// Codex-review Finding 1 (PR #82, round 11, ADR-083): a K-line 5-baud/
/// fast-init wakeup sequence is genuine external bus activity, but
/// `run_protocol_init` talks to the adapter directly via `ctx.api` rather
/// than going through `transmit_request`/`write_can_frame`, so none of those
/// paths' `last_bus_activity` stamps ever saw it before this fix. A mode-1
/// (idle-triggered) sibling CLL on the same physical channel must still have
/// its idle clock deferred by a sibling's real K-line init the same way it
/// would be by an ordinary `CoptSendrecv` write (unlike a mode-0 CLL's own
/// tester-present send, which -- as of this diff -- is deliberately excluded
/// from this same clock, see
/// `mode_0_own_send_does_not_stamp_last_bus_activity_for_mode_1_sibling`).
/// Mirrors that test's structure, substituting a real spec-mandated 5-baud
/// init (`CP_InitializationSettings == 1`) on cll_b for the mode-0 send as
/// the activity source.
/// cll_b is left with no `CP_TesterPresentMessage` configured so its own
/// tester-present resolves to "disabled" (`TesterPresentState::None`) and
/// never enters the picture, isolating cll_b's init as the sole activity
/// source. cll_a uses `CP_InitializationSettings == 3` (no init sequence) so
/// its own `CoptStartcomm` arms mode-1 immediately without running a K-line
/// init of its own.
#[tokio::test]
#[serial]
async fn kline_five_baud_init_stamps_last_bus_activity_deferring_mode_1_sibling() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a: mode-1 (idle-triggered), armed first with a 300ms interval, on
    // ISO9141. CP_InitializationSettings = 3 skips its own init sequence, so
    // arming happens with no K-line traffic of its own. 300ms (widened from
    // an originally-tried 80ms) gives generous absolute headroom against the
    // RPC/event round-trip overhead of arming cll_a below (the measured
    // `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS`) -- though see the
    // anchored `sleep_until` deadlines further down, which are what actually
    // make this test's margins robust rather than the interval size alone.
    let cll_a = create_cll(&mut client, j2534_0404::ISO9141, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 9_600).await;
    set_com_param_unum32(&mut client, cll_a, CP_INIT_SETTINGS, 3).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 300_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // cll_b: shares cll_a's physical channel (same protocol/baud rate) and
    // runs a real spec-mandated 5-baud init on its own CoptStartcomm.
    let cll_b = create_cll(&mut client, j2534_0404::ISO9141, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 9_600).await;
    set_com_param_unum32(&mut client, cll_b, CP_INIT_SETTINGS, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_com_param_unum32(&mut client, cll_b, CP_5BAUD_ADDR_PHYS, 0x2A).await;
    // cll_b is a joining CLL on an already-open shared channel, so its
    // Working set does not auto-promote to Active at Connect (only the
    // channel-creating CLL gets that) -- CoptStartcomm always resolves the
    // 5-baud init address from the call-time Active snapshot. Done well
    // before the timing-sensitive section below.
    promote_via_update_param(&mut client, cll_b).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // `started` anchors every deadline below to (just before) cll_a's arm,
    // so the `sleep_until` calls that follow measure absolute time-since-arm
    // rather than chaining blind relative sleeps on top of each other. Each
    // `sleep_until` only waits out whatever time actually remains until its
    // target, so RPC/event round-trip overhead already spent by the time
    // it's reached (arming cll_a, waiting for `PduCopstFinished`, and
    // waiting for its first tester-present frame below -- measured up to
    // the `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS` combined) does not
    // silently erode the margins asserted further down, the way summing
    // fixed blind sleeps did previously.
    let started = tokio::time::Instant::now();
    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;

    // ADR-084: cll_a's own CoptStartcomm arm already sent its first
    // tester-present frame immediately, synchronously, before
    // PduCopstFinished.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Run cll_b's real 5-baud init -- genuine external bus activity that
    // must reset cll_a's idle clock the same way an ordinary CoptSendrecv
    // would (unlike a mode-0 CLL's own tester-present send, which this diff
    // deliberately excludes from this clock) -- at t=150ms since `started`
    // (half of cll_a's 300ms window). That leaves 150ms of margin above the
    // `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS` of overhead already
    // spent above, comfortably before cll_a's ORIGINAL 300ms deadline.
    tokio::time::sleep_until(started + tokio::time::Duration::from_millis(150)).await;
    let reset_at = tokio::time::Instant::now();
    start_comm(&mut client, cll_b).await;
    wait_for_cop_finished(&mut events_b).await;
    assert_eq!(
        server.backdoor.five_baud_init_count(),
        1,
        "cll_b's spec 5-baud init should have run"
    );

    // At t=380ms since `started` -- 80ms past cll_a's ORIGINAL 300ms
    // deadline from arm, but well before a new 300ms window counted from
    // `reset_at` (~150ms + 300ms = ~450ms since `started`, i.e. ~70ms of
    // margin) -- cll_a's idle-mode send must still not have fired again.
    tokio::time::sleep_until(started + tokio::time::Duration::from_millis(380)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "cll_a's idle timer should have been reset by cll_b's 5-baud init, delaying cll_a's \
         tester-present send"
    );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = reset_at.elapsed();
    // 225ms is 75% of the 300ms window: comfortably above the ~150ms that
    // would have elapsed since `reset_at` had cll_a (incorrectly) fired at
    // its ORIGINAL from-arm deadline instead of being reset, and comfortably
    // below the ~300ms expected from a correct from-`reset_at` fire.
    assert!(
        elapsed >= tokio::time::Duration::from_millis(225),
        "cll_a's tester-present send should fire ~300ms after cll_b's 5-baud init, not the \
         original deadline (waited {elapsed:?})"
    );
    // The K-line KWP header format (format/target/source bytes ahead of the
    // payload) is not asserted here -- only that the fired frame carries
    // cll_a's configured tester-present payload byte, not cll_b's (which has
    // none configured).
    assert!(
        server
            .backdoor
            .written_data(MOCK_CHANNEL_ID, 1)
            .ends_with(&[0x11]),
        "the frame that fired should be cll_a's tester-present message"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// Codex review Finding 9 (PR #82, ninth round, ADR-083): a software-ISO-TP
/// multi-frame `CoptSendrecv` that writes its FirstFrame and then never gets
/// a FlowControl reply (N_Bs timeout, `PduErrEvtRxTimeout`) still put real
/// activity on the bus with that FirstFrame, even though the send's overall
/// `Result` is `Err`. A mode-1 sibling CLL sharing the same physical channel
/// must still have its idle clock deferred by that FirstFrame -- not see a
/// stale `last_bus_activity` and fire immediately after the failed send.
///
/// Mirrors `mode_0_own_send_does_not_stamp_last_bus_activity_for_mode_1_sibling`'s
/// structure, substituting a software-ISO-TP multi-frame `CoptSendrecv` that
/// fails on FlowControl timeout for the mode-0 send as the activity source
/// (a genuine, ordinary `CoptSendrecv`, unlike a mode-0 CLL's own
/// tester-present send, which this diff deliberately excludes from this
/// clock).
#[tokio::test]
#[serial]
async fn isotp_partial_tx_then_flow_control_timeout_still_defers_mode_1_sibling() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    // cll_a: mode-1 (idle-triggered), armed first with a 200ms interval,
    // functional addressing (short enough to stay a SingleFrame).
    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 200_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // cll_b: joins the same physical channel, physically addressed
    // (CP_CanPhysReqId), with a short N_Bs so the FlowControl-timeout
    // failure below resolves quickly.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_N_BS, 30_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;

    // ADR-084: cll_a's own CoptStartcomm arm already sent its first (SF
    // -framed) tester-present frame immediately, synchronously, before
    // PduCopstFinished.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Well before cll_a's original 200ms deadline, start cll_b's multi-frame
    // send: a 20-byte payload segments into FirstFrame + ConsecutiveFrames
    // (`software_isotp_mode_segments_multi_frame_request_with_flow_control`),
    // but no FlowControl is ever injected here, so the FirstFrame is the only
    // frame that reaches the wire before N_Bs (30ms) expires and the COP
    // fails with PduErrEvtRxTimeout / PduCopstFinished.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let reset_at = std::time::Instant::now();
    let payload: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    wait_for_cop_finished(&mut events_b).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "no FlowControl was ever injected -- only cll_a's immediate arm-time send and cll_b's \
         FirstFrame should have reached the wire"
    );

    // Well past cll_a's ORIGINAL 200ms deadline (from CoptStartcomm) but well
    // before a new 200ms window measured from cll_b's FirstFrame write,
    // cll_a's idle-mode send must still not have fired again: the FirstFrame
    // that reached the wire before the later FlowControl-timeout failure must
    // have deferred cll_a's clock just like a fully successful send would.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "cll_a's idle timer should have been deferred by cll_b's FirstFrame, even though cll_b's \
         send later failed on FlowControl timeout"
    );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let elapsed = reset_at.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(150),
        "cll_a's tester-present send should fire ~200ms after cll_b's FirstFrame, not the \
         original deadline (waited {elapsed:?})"
    );
    // Software-ISO-TP framing means cll_a's fire carries a PCI byte (and
    // possibly padding) after the CAN ID that `frame_tester_present_data`
    // built at arm time -- check only the CAN ID prefix (cll_a's own
    // functional address), not the exact frame length, to stay independent
    // of this CLL's default padding posture.
    assert_eq!(
        &server.backdoor.written_data(MOCK_CHANNEL_ID, 2)[..4],
        &0x7DF_u32.to_be_bytes(),
        "the third frame on the wire should be cll_a's second tester-present send"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// `CP_TesterPresentSendType = 0` (mode-0, software-driven as of this diff):
/// the arm-time send is gated against a pending `CP_P3Phys` gap left by a
/// prior physically-addressed `CoptSendrecv` on the same shared channel, the
/// same way an ordinary `CoptSendrecv` write is (ADR-083) -- mirrors
/// `p3_gap.rs`'s structure. `start_periodic_count()` stays `0` throughout:
/// this service no longer calls `PassThruStartPeriodicMsg` at all.
#[tokio::test]
#[serial]
async fn mode_0_arm_time_send_respects_pending_p3_phys_gap_from_prior_sendrecv() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Staged before connect (not via `create_and_connect_cll`, which only
    // supports Unum32 params) so the first CLL's connect-time Working->Active
    // promotion picks up the Bytefield tester-present message too, without
    // an extra `promote_via_update_param` round trip's ~100ms sleep eating
    // into the CP_P3Phys window this test measures below.
    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_P3_PHYS, 300_000).await;
    // CP_TesterPresentSendType is left at its default (0, periodic).
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 100_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // A prior physically-addressed send requiring no response seeds the
    // CP_P3Phys gap bucket.
    start_send_recv(&mut client, cll_handle, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let started = std::time::Instant::now();
    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed >= std::time::Duration::from_millis(280),
        "the mode-0 arm-time send should wait ~300ms for CP_P3Phys, not send immediately (waited {elapsed:?})"
    );
    assert_eq!(
        server.backdoor.start_periodic_count(),
        0,
        "mode 0 must not start a hardware periodic message (ADR-083/this diff)"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "the mode-0 arm-time send should have gone out via PassThruWriteMsgs once the CP_P3Phys \
         gap cleared"
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 1), {
        let mut expected = 0x7E0_u32.to_be_bytes().to_vec();
        expected.push(0x3E);
        expected
    });

    drop(events);
    server.shutdown().await;
}

/// `PduErrEvtTesterPresentError` predicate for [`wait_for_event`], mirroring
/// `cop_ctrl_cycles.rs`'s `is_cancelled` helper.
fn is_tester_present_error(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(event_item::Data::ErrorData(error))
            if error == vci_service_interface::PduErrorEvent::PduErrEvtTesterPresentError as i32
    )
}

/// Fix 1 (Codex-review fix, ADR-083), revised for ADR-084's mode-1 immediate
/// arm-time send: `handle_start_comm`'s unified arm branch performs its own
/// `wait_for_p3_gap` wait (non-cancellable) BEFORE the immediate send and
/// before Step 3's write-back -- so, unlike before ADR-084, a pending
/// `CP_P3Func`/`CP_P3Phys` gap now blocks `CoptStartcomm` itself, not just a
/// later `dispatch_due_tester_present` dispatch. A `DisconnectComLogicalLink`
/// landing on cll_a from a *different* tokio task while the poll task is
/// asleep inside THIS wait -- a genuine concurrent race, not just an
/// end-state check, since `DisconnectComLogicalLink` mutates `logical_links`
/// directly (outside the poll task's `TxItem` queue) -- must not result in a
/// tester-present frame going out for cll_a, nor an error event; the COP must
/// instead observe `PduCopstCancelled`. Mirrors
/// `mode_0_disconnect_racing_the_p3_gap_wait_suppresses_the_arm_time_send`'s
/// structure for the mode-0 analog of this same race (both modes now share
/// the identical code path). cll_b shares the physical channel purely to
/// keep the poll task alive after cll_a disconnects (disconnecting the sole
/// CLL on a channel tears the channel, and the poll task, down entirely).
#[tokio::test]
#[serial]
async fn send_type_1_disconnect_racing_the_p3_gap_wait_suppresses_the_send() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_P3_FUNC, 250_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 50_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // Kept alive (not disconnected) so the shared physical channel -- and
    // its poll task -- survives cll_a's disconnect below.
    let _cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    // Seed the shared CP_P3Func gap bucket: a functionally-addressed,
    // no-response send from cll_a. This is what makes cll_a's OWN upcoming
    // CoptStartcomm arm (below) have to wait ~250ms inside wait_for_p3_gap
    // before its immediate send and PduCopstFinished (ADR-084) -- unlike
    // before ADR-084, when arming was instant regardless of this bucket.
    start_send_recv(&mut client, cll_a, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    // Comfortably inside the ~250ms CP_P3Func wait: the poll task should be
    // asleep inside handle_start_comm's own (non-cancellable) wait_for_p3_gap
    // call for cll_a's mode-1 arm right now.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // A genuinely concurrent disconnect: this runs on a different tokio task
    // than the channel poll task, and DisconnectComLogicalLink mutates
    // `logical_links` directly, outside the poll task's TxItem queue.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // Past the full 250ms CP_P3Func deadline (measured from the seed send)
    // with margin: wait_for_p3_gap has returned Ready by now, giving the
    // re-validation a chance to run (and, pre-fix, giving a stale send a
    // chance to go out).
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "no tester-present frame should have been sent for cll_a, which disconnected while its \
         own CoptStartcomm arm was waiting on CP_P3Func -- only the seed send should be present"
    );

    // The COP must observe PduCopstCancelled instead of silently vanishing
    // or completing as PduCopstFinished with a phantom arm. Checked BEFORE
    // the tester-present-error absence check below: wait_for_event drains
    // (and discards) every non-matching event it reads while waiting, so
    // checking absence-of-error first would silently consume this
    // already-arrived PduCopstCancelled (emitted by DisconnectComLogicalLink's
    // own cancel_link_cops call, well before this point) without ever seeing
    // it.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cop_handle))
        .await,
        "the mode-1 CoptStartcomm arm should have received PduCopstCancelled instead of \
         PduCopstFinished after the disconnect race"
    );

    assert!(
        !wait_for_event(&mut events, 200, is_tester_present_error).await,
        "no PduErrEvtTesterPresentError should fire for a CLL that disconnected mid-wait -- \
         the send is silently skipped, not treated as a failure"
    );

    drop(events);
    server.shutdown().await;
}

/// Fix 2 (Codex-review fix, ADR-083): the mode-0 `CP_P3Func`/`CP_P3Phys`
/// arm-time gate in `handle_start_comm` must be non-cancellable -- a
/// `CancelComPrimitive` racing the wait must not abort the COP.
///
/// This reuses `mode_0_arm_time_send_respects_pending_p3_phys_gap_from_prior_sendrecv`'s
/// CAN/ISO15765 CP_P3Phys setup to force a real, multi-hundred-millisecond
/// wait inside `wait_for_p3_gap`, then cancels the COP while it is waiting.
/// Note: this does NOT exercise a real K-line five-baud/fast-init ECU
/// handshake ahead of the gate -- `can_functional` (and therefore any
/// `wait_for_p3_gap` wait at all) is structurally `None` for ISO9141/ISO14230
/// (`resolve_tester_present`/`resolve_send_recv_tx` gate it on
/// `protocol.is_can_family()`), so a real K-line init sequence and a real
/// P3-gate wait can never occur in the same `CoptStartcomm` call under the
/// current addressing model. This test exercises the actual code path and
/// call site Fix 2 changed (the mode-0 `wait_for_p3_gap(.., false, ctx)`
/// call), which is protocol-agnostic, and directly proves the required
/// property -- cancellation during this wait does not produce
/// `PduCopstCancelled` -- but it is a weaker proxy for the ADR's specific
/// "already committed a real ECU handshake" motivating scenario than a
/// K-line reproduction would be.
#[tokio::test]
#[serial]
async fn mode_0_p3_gate_is_not_cancellable() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_P3_PHYS, 300_000).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 100_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // A prior physically-addressed send requiring no response seeds the
    // CP_P3Phys gap bucket, so the mode-0 start gate below has to wait
    // ~300ms.
    start_send_recv(&mut client, cll_handle, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    // Comfortably inside the ~300ms P3Phys wait: cancel the still-executing
    // CoptStartcomm.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    // The COP must still finish normally -- the non-cancellable gate ignores
    // the cancellation and proceeds to send the tester-present frame.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cop_handle.cop_handle))
        .await,
        "CoptStartcomm should finish as PduCopstFinished despite a CancelComPrimitive racing \
         the non-cancellable mode-0 P3-gate wait, not end as PduCopstCancelled"
    );
    assert_eq!(
        server.backdoor.start_periodic_count(),
        0,
        "mode 0 must not start a hardware periodic message (ADR-083/this diff)"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "tester-present should still have been sent (arm-time send via PassThruWriteMsgs) \
         despite the race"
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 1), {
        let mut expected = 0x7E0_u32.to_be_bytes().to_vec();
        expected.push(0x3E);
        expected
    });

    drop(events);
    server.shutdown().await;
}

/// Fix 3 (Codex-review fix, ADR-083): arming a mode-1 CLL must not stamp the
/// shared channel's `last_bus_activity` clock -- doing so would silently
/// push out a sibling CLL's already-counting idle window. cll_a arms first
/// and counts down from a 150ms interval; cll_b, sharing the same physical
/// channel, arms its own (much longer-interval) mode-1 tester-present
/// halfway through cll_a's window. cll_a must still fire at ~150ms from its
/// own arm time, not ~150ms from cll_b's later arm.
#[tokio::test]
#[serial]
async fn send_type_1_sibling_cll_arming_does_not_reset_another_clls_idle_window() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 150_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_b, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_b, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    // Deliberately huge: cll_b must not fire its own tester-present during
    // this test -- only its arm (and whether that arm perturbs cll_a) is
    // under test.
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_TIME, 100_000_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let started = std::time::Instant::now();
    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;

    // ADR-084: cll_a's own CoptStartcomm arm already sent its first
    // tester-present frame immediately, synchronously, before
    // PduCopstFinished -- essentially at `started`, since nothing gates it.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let mut expected_a = 0x7DF_u32.to_be_bytes().to_vec();
    expected_a.push(0x3E);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_a,
        "cll_a's own immediate arm-time send should be the first frame"
    );

    // Halfway through cll_a's 150ms window, cll_b arms. cll_b is a joining
    // CLL on cll_a's already-open shared channel, so its Working set (the
    // tester-present ComParams staged above) never auto-promotes to Active
    // at Connect and is never explicitly promoted here either -- cll_b's own
    // CoptStartcomm therefore resolves tester-present as disabled (empty
    // message) and produces no frame of its own (`TesterPresentState::None`,
    // not `Idle`); only its arm as bus activity (or lack thereof) is under
    // test. Pre-fix, arming used to stamp the shared last_bus_activity
    // clock regardless, pushing cll_a's fire out to
    // ~started+75ms+150ms=225ms.
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    start_comm(&mut client, cll_b).await;
    wait_for_cop_finished(&mut events_b).await;

    // cll_a's own SECOND (idle-triggered) send should still fire ~150ms
    // after its own arm, not ~225ms after cll_b's later arm.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_millis(210),
        "cll_a's idle-triggered send should fire ~150ms after its own arm, not ~225ms after \
         cll_b's later arm (waited {elapsed:?})"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(130),
        "cll_a's idle-triggered send fired too early for a 150ms idle window (waited {elapsed:?})"
    );

    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_a,
        "the frame that fired should be cll_a's tester-present message, not cll_b's"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// Fix 4 (Codex-review fix, ADR-083): the mode-0 `CP_P3Func`/`CP_P3Phys`
/// arm-time gate in `handle_start_comm` is non-cancellable
/// (`mode_0_p3_gate_is_not_cancellable`), but a `DestroyComLogicalLink`
/// landing on the CLL while that wait is still in progress is not gated by
/// `cancelled_cops` at all -- it mutates `logical_links` directly, outside
/// the poll task's `TxItem` queue, the same way
/// `send_type_1_disconnect_racing_the_p3_gap_wait_suppresses_the_send` races
/// `dispatch_due_tester_present`. The re-validation this test pins prevents a
/// stale send for a CLL no longer on this channel: `still_on_this_channel` is
/// re-checked (and the send skipped if it fails) right before the send goes
/// out, mirroring mode 1's own equivalent guard (both modes share the same
/// unified branch as of this diff). cll_b shares the physical channel purely
/// to keep the poll task alive after cll_a is destroyed (destroying the sole
/// CLL on a channel tears the channel, and the poll task, down entirely,
/// which would make this test unable to distinguish "nothing sent" from
/// "nothing left to observe").
///
/// `DisconnectComLogicalLink` cannot stand in for `DestroyComLogicalLink`
/// here the way it does in the mode-1 sibling test above: disconnect leaves
/// the CLL's entry in `logical_links` in place (merely marking it
/// `connected = false`), and the fix's re-validation is a `contains_key`
/// check, not a `connected` check -- only an actual removal exercises it.
#[tokio::test]
#[serial]
async fn mode_0_destroy_racing_the_p3_gap_wait_suppresses_the_arm_time_send() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_P3_PHYS, 300_000).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 100_000).await;
    // CP_TesterPresentSendType is left at its default (0, periodic).
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    // Kept alive (not destroyed) so the shared physical channel -- and its
    // poll task -- survives cll_a's destroy below.
    let _cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    // A prior physically-addressed send requiring no response seeds the
    // CP_P3Phys gap bucket, so the mode-0 start gate below has to wait
    // ~300ms.
    start_send_recv(&mut client, cll_a, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    // Comfortably inside the ~300ms P3Phys wait: the poll task should be
    // asleep inside wait_for_p3_gap's polling loop right now.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // A genuinely concurrent destroy: this runs on a different tokio task
    // than the channel poll task, and DestroyComLogicalLink removes cll_a
    // from `logical_links` directly, outside the poll task's TxItem queue.
    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("destroy_com_logical_link should succeed");

    // Past the full 300ms CP_P3Phys deadline (measured from the seed send)
    // with margin: wait_for_p3_gap has returned Ready by now, giving the
    // re-validation a chance to run (and, pre-fix, giving a stale send a
    // chance to go out for a CLL that no longer exists).
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    assert_eq!(
        server.backdoor.start_periodic_count(),
        0,
        "mode 0 must not start a hardware periodic message (ADR-083/this diff)"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "no tester-present frame should have been sent for cll_a, which was destroyed while its \
         own CoptStartcomm arm-time send was waiting on CP_P3Phys -- only the seed send should \
         be present"
    );

    drop(events);
    server.shutdown().await;
}

/// Fix 5 / starvation-bug regression (Codex-review fix, ADR-083): when two
/// same-interval mode-1 CLLs on the same physical channel become due in the
/// same poll tick, each CLL tracks its own `last_fired` anchor and is
/// excluded from the shared `last_bus_activity` clock for its own send
/// (`transmit_request`'s `count_as_bus_activity = false` at the idle-mode
/// call site). Neither CLL's own keep-alive may defer the other's schedule:
/// both fire close together (P3-gap-spaced, not a full interval apart), and
/// -- the actual regression this test guards against -- both keep firing
/// every subsequent interval too. An earlier draft let a CLL's own send
/// re-stamp the shared clock, which permanently resynchronized both CLLs'
/// due-instants to the winner's; since `HashMap` iteration order is stable
/// within one process run, the same CLL won every batch and the other was
/// starved of tester-present indefinitely after the very first collision
/// (caught by Codex review, not by the original design). Asserting only the
/// first pair of sends would not have caught that: the bug's symptom is the
/// *second* interval onward going silent for one CLL, not a one-time delay.
///
/// cll_a and cll_b share one physical channel, are armed with the same
/// `CP_TesterPresentTime` interval back-to-back (no bus traffic between the
/// two arms), so both become due at essentially the same wall-clock moment
/// and are very likely to land in the very same 10 ms poll tick
/// (`POLL_INTERVAL_MS`) on every interval, not just the first.
#[tokio::test]
#[serial]
async fn send_type_1_two_clls_due_in_the_same_tick_both_keep_firing_every_interval() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    const INTERVAL_MS: u32 = 150;
    const ROUNDS: usize = 3;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(
        &mut client,
        cll_a,
        CP_TESTER_PRESENT_TIME,
        INTERVAL_MS * 1000,
    )
    .await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_b, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_b, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    // Same interval as cll_a -- both must become due at essentially the same
    // moment, every interval.
    set_com_param_unum32(
        &mut client,
        cll_b,
        CP_TESTER_PRESENT_TIME,
        INTERVAL_MS * 1000,
    )
    .await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    // cll_b joins the physical channel cll_a already opened: only the
    // channel-creating CLL's Working set auto-promotes to Active at Connect
    // (ADR-067/068) -- a joining CLL's Active stays default until it issues
    // its own CoptUpdateparam, so cll_b's tester-present ComParams above
    // would otherwise resolve as unset (no message, 0 ms interval) when its
    // own CoptStartcomm reads Active below.
    promote_via_update_param(&mut client, cll_b).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Arm back-to-back -- both `StartComPrimitive` calls are issued before
    // waiting on either one's `PduCopstFinished`, so both `TxItem::StartComm`
    // entries land in the shared channel's queue (and are processed by the
    // single poll task) only microseconds apart, with no bus traffic between
    // them -- both land in essentially the same idle window. Waiting for
    // cll_a's `PduCopstFinished` before issuing cll_b's `StartComPrimitive`
    // (as most of this file's other tests do) would add a full gRPC
    // round-trip of separation between the two `armed_at` timestamps --
    // comfortably larger than one `POLL_INTERVAL_MS` tick -- and defeat the
    // "due in the same tick" setup this test needs.
    let mut client_b = client.clone();
    tokio::join!(
        start_comm(&mut client, cll_a),
        start_comm(&mut client_b, cll_b)
    );
    wait_for_cop_finished(&mut events_a).await;
    wait_for_cop_finished(&mut events_b).await;

    let mut msg_a = 0x7DF_u32.to_be_bytes().to_vec();
    msg_a.push(0x3E);
    let mut msg_b = 0x7DF_u32.to_be_bytes().to_vec();
    msg_b.push(0x11);

    // Each round should produce exactly one send from cll_a and one from
    // cll_b, close together in time (P3-gap-spaced, not a full interval
    // apart), and this must hold for every round, not just the first --
    // that is the actual starvation bug's regression surface.
    for round in 0..ROUNDS {
        let first_index = round * 2 + 1;
        let second_index = round * 2 + 2;

        wait_for_written_count(&server, MOCK_CHANNEL_ID, first_index).await;
        let first_at = std::time::Instant::now();
        wait_for_written_count(&server, MOCK_CHANNEL_ID, second_index).await;
        let gap = first_at.elapsed();

        assert!(
            gap < std::time::Duration::from_millis(80),
            "round {round}: two same-interval CLLs due in the same tick should fire close \
             together (P3-gap-spaced), not ~1 interval apart -- one CLL's own send must not \
             defer the other's schedule (gap was {gap:?})"
        );

        let frame_1 = server
            .backdoor
            .written_data(MOCK_CHANNEL_ID, first_index - 1);
        let frame_2 = server
            .backdoor
            .written_data(MOCK_CHANNEL_ID, second_index - 1);
        let frames = [frame_1, frame_2];
        assert!(
            frames.contains(&msg_a),
            "round {round}: cll_a should still be receiving its own keep-alive (starvation \
             regression check) -- frames were {frames:?}"
        );
        assert!(
            frames.contains(&msg_b),
            "round {round}: cll_b should still be receiving its own keep-alive (starvation \
             regression check) -- frames were {frames:?}"
        );
    }

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// Codex-review Finding 1 (PR #82, ninth round, ADR-083): `CONFIG_LOOPBACK`
/// echoes an idle-mode CLL's own tester-present send back through
/// `PassThruReadMsgs` with `RxStatus`'s `RX_TX_MSG_TYPE` bit set (the mock
/// simulates this in `PassThruWriteMsgs`, `j2534-0404-mock/src/lib.rs`).
/// `poll_rx_inner` must not treat that TX echo as genuine external bus
/// activity when deciding whether to stamp the shared `last_bus_activity`
/// clock -- doing so would bypass `transmit_request`'s
/// `count_as_bus_activity = false` gate for the idle-mode call site via this
/// separate RX path, reintroducing the same-interval-sibling starvation
/// `send_type_1_two_clls_due_in_the_same_tick_both_keep_firing_every_interval`
/// already guards against, via a different mechanism.
///
/// A nonzero shared `CP_P3Func` is the key ingredient that makes the
/// loopback path (as opposed to the already-fixed `transmit_request` path)
/// actually reproduce a *differential* starvation, not just a uniform delay:
/// `wait_for_p3_gap`'s own wait loop calls the shared `poll_rx` on every
/// `POLL_INTERVAL_MS` tick while it blocks. cll_a and cll_b are armed
/// back-to-back with the same interval so both land in one
/// `dispatch_due_tester_present` batch; whichever CLL that batch
/// processes first fires immediately (no prior `last_func_tx` stamp to gate
/// on) and its `PassThruWriteMsgs` loopback echo lands in the mock's RX
/// queue right away, while the second CLL -- gated behind the first's fresh
/// `last_func_tx` stamp -- must actually wait out `CP_P3Func` and, in doing
/// so, drains that still-unprocessed echo via its own `wait_for_p3_gap`
/// poll, mid-batch, before its own `still_due` re-check runs. Pre-fix, that
/// re-check sees a `last_bus_activity` just re-stamped by the first CLL's
/// own echo and defers the second CLL for a full interval -- every round,
/// since `HashMap` iteration order is stable within one process run --
/// reproducing the exact "same CLL wins every batch" starvation ADR-083's
/// Consequences section documents for the (already-fixed)
/// `transmit_request` path.
#[tokio::test]
#[serial]
async fn send_type_1_loopback_echo_does_not_starve_a_same_interval_sibling() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    const INTERVAL_MS: u32 = 150;
    const ROUNDS: usize = 3;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    // CP_Loopback is a hardware SET_CONFIG param pushed for the whole shared
    // physical channel at cll_a's connect (below) -- no per-CLL promotion
    // needed for cll_b to be affected by it too.
    set_com_param_unum32(&mut client, cll_a, j2534_0404::LOOPBACK, 1).await;
    // Small, shared CP_P3Func: forces whichever CLL is processed second in a
    // due batch to actually wait inside `wait_for_p3_gap` (see doc comment
    // above) instead of resolving instantly, which is what lets that CLL's
    // own `still_due` re-check observe a `last_bus_activity` freshly
    // re-stamped by the first CLL's own loopback echo, mid-batch.
    set_com_param_unum32(&mut client, cll_a, CP_P3_FUNC, 25_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(
        &mut client,
        cll_a,
        CP_TESTER_PRESENT_TIME,
        INTERVAL_MS * 1000,
    )
    .await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_P3_FUNC, 25_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_b, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_b, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    // Same interval as cll_a -- both must become due at essentially the same
    // moment, every interval.
    set_com_param_unum32(
        &mut client,
        cll_b,
        CP_TESTER_PRESENT_TIME,
        INTERVAL_MS * 1000,
    )
    .await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    // cll_b joins the physical channel cll_a already opened: only the
    // channel-creating CLL's Working set auto-promotes to Active at Connect
    // (ADR-067/068) -- a joining CLL's Active stays default until it issues
    // its own CoptUpdateparam, so cll_b's tester-present ComParams above
    // would otherwise resolve as unset (no message, 0 ms interval) when its
    // own CoptStartcomm reads Active below.
    promote_via_update_param(&mut client, cll_b).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Arm back-to-back, same reasoning as
    // `send_type_1_two_clls_due_in_the_same_tick_both_keep_firing_every_interval`:
    // both TxItem::StartComm entries land in the shared channel's queue only
    // microseconds apart, so both become due in essentially the same poll
    // tick on every interval.
    let mut client_b = client.clone();
    tokio::join!(
        start_comm(&mut client, cll_a),
        start_comm(&mut client_b, cll_b)
    );
    wait_for_cop_finished(&mut events_a).await;
    wait_for_cop_finished(&mut events_b).await;

    let mut msg_a = 0x7DF_u32.to_be_bytes().to_vec();
    msg_a.push(0x3E);
    let mut msg_b = 0x7DF_u32.to_be_bytes().to_vec();
    msg_b.push(0x11);

    // Each round should produce exactly one send from cll_a and one from
    // cll_b -- if the loopback echo of the batch's first-processed CLL were
    // allowed to re-stamp last_bus_activity while the second CLL waits out
    // CP_P3Func (see doc comment above), the second CLL would be silently
    // starved from round 2 onward, not just delayed. Unlike the
    // loopback-disabled sibling test, the two sends are not asserted to be
    // close together here -- CP_P3Func deliberately spaces them by design.
    for round in 0..ROUNDS {
        let first_index = round * 2 + 1;
        let second_index = round * 2 + 2;

        wait_for_written_count(&server, MOCK_CHANNEL_ID, first_index).await;
        wait_for_written_count(&server, MOCK_CHANNEL_ID, second_index).await;

        let frame_1 = server
            .backdoor
            .written_data(MOCK_CHANNEL_ID, first_index - 1);
        let frame_2 = server
            .backdoor
            .written_data(MOCK_CHANNEL_ID, second_index - 1);
        let frames = [frame_1, frame_2];
        assert!(
            frames.contains(&msg_a),
            "round {round}: cll_a should still be receiving its own keep-alive (loopback-echo \
             starvation regression check) -- frames were {frames:?}"
        );
        assert!(
            frames.contains(&msg_b),
            "round {round}: cll_b should still be receiving its own keep-alive (loopback-echo \
             starvation regression check) -- frames were {frames:?}"
        );
    }

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// Second-round Codex-review fix (Gap 1, revised): the mode-0
/// `CP_P3Func`/`CP_P3Phys` arm-time gate's post-wait re-validation in
/// `handle_start_comm` must compare `channel_id`, not merely check the CLL
/// still has an entry in `logical_links`. `DisconnectComLogicalLink` (unlike
/// `DestroyComLogicalLink`, covered by
/// `mode_0_destroy_racing_the_p3_gap_wait_suppresses_the_arm_time_send`)
/// deliberately leaves the CLL's entry in `logical_links` in place -- it only
/// clears `channel_id`/`connected`/`comm_started`/`tester_present_state` --
/// so a bare `contains_key` check (the first, insufficient attempt at this
/// fix) still returned `true` for a disconnected CLL, and would go on to send
/// a stale tester-present frame on the shared physical channel for a CLL no
/// longer there. cll_b shares the physical channel purely to keep the poll
/// task alive after cll_a disconnects (disconnecting the sole CLL on a
/// channel tears the channel, and the poll task, down entirely).
#[tokio::test]
#[serial]
async fn mode_0_disconnect_racing_the_p3_gap_wait_suppresses_the_arm_time_send() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_P3_PHYS, 300_000).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 100_000).await;
    // CP_TesterPresentSendType is left at its default (0, periodic).
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    // Kept alive (not disconnected) so the shared physical channel -- and its
    // poll task -- survives cll_a's disconnect below.
    let _cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    // A prior physically-addressed send requiring no response seeds the
    // CP_P3Phys gap bucket, so the mode-0 start gate below has to wait
    // ~300ms.
    start_send_recv(&mut client, cll_a, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    // Comfortably inside the ~300ms P3Phys wait: the poll task should be
    // asleep inside wait_for_p3_gap's polling loop right now.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // A genuinely concurrent disconnect: this runs on a different tokio task
    // than the channel poll task. DisconnectComLogicalLink does NOT remove
    // cll_a's entry from `logical_links` (unlike DestroyComLogicalLink) --
    // it only clears channel_id/connected/comm_started/tester_present_state,
    // leaving `logical_links.contains_key(&cll_a)` true throughout.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // Past the full 300ms CP_P3Phys deadline (measured from the seed send)
    // with margin: wait_for_p3_gap has returned Ready by now, giving the
    // re-validation a chance to run (and, pre-fix, giving a stale send a
    // chance to go out for a CLL whose entry is still present in
    // `logical_links` but no longer on this channel).
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    assert_eq!(
        server.backdoor.start_periodic_count(),
        0,
        "mode 0 must not start a hardware periodic message (ADR-083/this diff)"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "no tester-present frame should have been sent for cll_a, which disconnected while its \
         own CoptStartcomm arm-time send was waiting on CP_P3Phys -- even though cll_a's entry is \
         still present in logical_links (DisconnectComLogicalLink does not remove it, unlike \
         DestroyComLogicalLink) -- only the seed send should be present"
    );

    // Verified bug fix regression (Codex PR #82): the COP must not silently
    // vanish from `PduCopstExecuting` with no terminal status -- the bare
    // `return` this test originally caught (via `start_periodic_count`
    // alone) left a real gap, since `dispatch_tx_item` unconditionally
    // removes the COP from `primitives` once `handle_start_comm` returns,
    // regardless of whether any event was ever sent for it. A subscriber
    // must actually observe `PduCopstCancelled` for this exact cop_handle.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cop_handle))
        .await,
        "the mode-0 start COP should have received PduCopstCancelled instead of silently \
         disappearing after the disconnect race"
    );

    // Whichever side wins the race between this re-validation and
    // `DisconnectComLogicalLink`'s own `cancel_link_cops` (first to remove
    // the entry from `primitives` emits), the other must not also emit
    // `PduCopstCancelled` for the same cop_handle.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cop_handle))
        .await,
        "PduCopstCancelled must be emitted exactly once for this cop_handle, not twice"
    );

    drop(events);
    server.shutdown().await;
}

/// Second-round Codex-review fix (Gap 2, revised), reframed for ADR-084's
/// mode-1 immediate arm-time send: `handle_start_comm`'s own mode-1 P3-gate
/// wait (not, as before ADR-084, a later `dispatch_due_tester_present`
/// dispatch) is what can now be raced by a disconnect-then-reconnect-and-
/// rearm. The post-wait re-validation there must require both that the CLL
/// is still on the poll task's own channel AND still the same arm generation
/// -- merely re-checking "connected somewhere" is not enough: if a
/// `DisconnectComLogicalLink` clears the old arm mid-wait and the same CLL
/// handle is then reconnected and re-armed (`CoptStartcomm` mode 1 again) on
/// a *different* shared physical channel before the re-check runs, a bare
/// existence/connectedness check would wrongly accept it -- sending the
/// *stale* pre-wait `protocol_id`/`tx_flags`/`data` (captured before the
/// wait, from the old arm) on the poll task's own (now-stale) channel.
///
/// cll_a and cll_b share one physical channel (channel 1). cll_a's FIRST
/// `CoptStartcomm` arm is seeded with a pending `CP_P3Func` gap (from a prior
/// `CoptSendrecv`), so the arm's own immediate send has to wait inside
/// `handle_start_comm`'s (non-cancellable) `wait_for_p3_gap` call -- the COP
/// does not reach `PduCopstFinished` yet. While that wait is in progress,
/// cll_a is disconnected, reassigned a different `DATA_RATE` (changing its
/// `ChannelKey`), and reconnected -- landing on a brand-new physical channel
/// (channel 2, with cll_a as sole/creator CLL, so its Working set
/// auto-promotes to Active at Connect) -- then re-armed (a SECOND
/// `CoptStartcomm`) with a different tester-present message so the stale
/// arm's payload is distinguishable from the new arm's; channel 2 has no
/// `CP_P3Func` seed of its own, so this second arm's immediate send is not
/// gated and completes right away. cll_b keeps channel 1's poll task alive
/// across cll_a's disconnect (mirrors
/// `mode_0_disconnect_racing_the_p3_gap_wait_suppresses_the_arm_time_send`
/// / `send_type_1_disconnect_racing_the_p3_gap_wait_suppresses_the_send`).
#[tokio::test]
#[serial]
async fn send_type_1_reconnect_and_rearm_racing_the_p3_gap_wait_does_not_send_stale_arm() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_P3_FUNC, 250_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 50_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // Kept alive (not disconnected) so channel 1 -- and its poll task --
    // survives cll_a's disconnect below.
    let _cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share physical channel 1"
    );

    // Seed the shared CP_P3Func gap bucket: a functionally-addressed,
    // no-response send from cll_a. This is what makes cll_a's own FIRST
    // CoptStartcomm arm (below) have to wait ~250ms inside
    // handle_start_comm's own wait_for_p3_gap call before completing
    // (ADR-084).
    start_send_recv(&mut client, cll_a, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // FIRST arm on channel 1: does not complete yet -- its own immediate
    // send is gated behind the still-pending CP_P3Func window seeded above.
    let cop_handle_1 = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    // Comfortably inside the ~250ms CP_P3Func wait: channel 1's poll task
    // should be asleep inside handle_start_comm's own (non-cancellable)
    // wait_for_p3_gap call for cll_a's first arm right now.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // A genuinely concurrent disconnect: this runs while channel 1's poll
    // task is mid-wait. Clears cll_a's tester_present_state back to None and
    // its channel_id to None, but leaves cll_a's entry in `logical_links`.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // DisconnectComLogicalLink's own cancel_link_cops call cancels
    // cop_handle_1 immediately (first-wins over handle_start_comm's own,
    // later, re-validation bail-out) -- mirrors
    // send_type_1_disconnect_racing_the_p3_gap_wait_suppresses_the_send.
    // Checked here, before any further event-draining wait_for_event calls
    // below, so it is not silently discarded by one of them.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cop_handle_1))
        .await,
        "cll_a's first (stale) CoptStartcomm arm should have received PduCopstCancelled after \
         the disconnect race"
    );

    // Reassign cll_a to a different DATA_RATE, changing its ChannelKey, then
    // reconnect: since no other CLL shares (ISO15765, 250_000), this opens a
    // brand-new physical channel (channel 2) with cll_a as its sole/creator
    // CLL, so cll_a's Working set (including the new tester-present message
    // below) auto-promotes to Active at Connect.
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 250_000).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x77]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("reconnect_com_logical_link should succeed for cll_a");
    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "cll_a's reconnect at a different DATA_RATE should open a second physical channel"
    );

    // Re-arm cll_a's idle clock on the NEW channel (channel 2) -- a fresh
    // armed_at, distinct from the stale first arm channel 1's poll task is
    // still waiting on behalf of. Channel 2 has no CP_P3Func seed of its
    // own, so this immediate send is not gated and the COP completes right
    // away.
    let cop_handle_2 = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;
    assert!(
        wait_for_event(&mut events, 2000, |item| is_cop_finished_for(
            item,
            cop_handle_2
        ))
        .await,
        "cll_a's second (new) CoptStartcomm arm on channel 2 should complete promptly -- no \
         CP_P3Func gap is pending on this fresh physical channel"
    );

    // The new arm's immediate send already carries the NEW message (0x77),
    // not the stale 0x3E -- proven right after the second arm's own
    // PduCopstFinished, well before channel 1's stale wait even elapses.
    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.push(0x77);
    assert_eq!(
        server.backdoor.written_data(2, 0),
        expected,
        "the frame from cll_a's new arm on channel 2 should carry the new arm's message, not \
         the stale pre-wait one"
    );

    // Past the full 250ms CP_P3Func deadline (measured from the seed send on
    // channel 1) with margin: channel 1's wait_for_p3_gap has returned Ready
    // by now, giving the stale re-validation a chance to run (and, pre-fix,
    // giving the stale pre-wait 0x3E payload a chance to go out on channel 1
    // for a CLL that is no longer there).
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "no stale tester-present frame should have gone out on channel 1 -- only the CP_P3Func \
         seed send should be present; the stale arm (disconnected, then reconnected+rearmed on \
         a different channel) must be suppressed by the channel_id/armed_at re-validation"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-086: unlike
/// `send_type_1_reconnect_and_rearm_racing_the_p3_gap_wait_does_not_send_stale_arm`
/// above (which reconnects cll_a onto a *different* physical channel, so the
/// stale arm's post-wait re-validation is caught by `channel_id` mismatch
/// alone), this reconnects cll_a onto the SAME shared physical channel (same
/// `DATA_RATE`, cll_b still holding it): the stale arm's `channel_id ==
/// Some(ctx.channel_id)` check would incorrectly pass again after such a
/// same-channel reconnect, so only `connect_generation` (bumped by the
/// reconnect even though `channel_id` matches again) can catch it. Mirrors
/// `stopcomm_disconnect_then_reconnect_same_channel_suppresses_stale_final_transmit`'s
/// (`stopcomm_data_tx.rs`) structure for `handle_start_comm`'s identical,
/// pre-existing flaw (same guard shape, added in an earlier PR-88 round,
/// unrelated to the PR-90 StopComm fix but sharing the same root cause and
/// mechanism per design-advisor).
///
/// The reconnected (joining) cll_a's own Active ComParamSet resets to default
/// (`finalize_connected_link`: only the channel-creator's Working promotes to
/// Active at Connect), so a second arm on the rejoined channel would resolve
/// an empty tester-present message and never reach the wire -- this test
/// does not need a second arm at all: the FIRST (stale) arm's own
/// `tester_present.data` was already resolved eagerly, before its wait,
/// against the still-valid pre-disconnect Active set (ADR-067), so if the
/// guard failed to catch the stale `connect_generation` it would still
/// attempt to send that captured (valid-looking) payload on the
/// still-existing physical channel.
#[tokio::test]
#[serial]
async fn send_type_1_disconnect_then_reconnect_same_channel_suppresses_stale_arm() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_P3_FUNC, 250_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 50_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // Kept alive (not disconnected) so the shared physical channel -- and its
    // poll task -- survives cll_a's disconnect/reconnect below.
    let _cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    // Seed the shared CP_P3Func gap bucket: a functionally-addressed,
    // no-response send from cll_a. This is what makes cll_a's own
    // CoptStartcomm arm (below) have to wait ~250ms inside
    // handle_start_comm's own wait_for_p3_gap call before completing
    // (ADR-084).
    start_send_recv(&mut client, cll_a, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // The arm: does not complete yet -- its own immediate send is gated
    // behind the still-pending CP_P3Func window seeded above.
    let cop_handle_1 = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    // Comfortably inside the ~250ms CP_P3Func wait: the poll task should be
    // asleep inside handle_start_comm's own (non-cancellable) wait_for_p3_gap
    // call for cll_a's arm right now.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // A genuinely concurrent disconnect, immediately followed by a reconnect
    // of the SAME cll_handle at the SAME DATA_RATE -- rejoining the same
    // shared physical channel (still kept alive by cll_b) and getting back
    // the identical ChannelId, but a freshly-bumped connect_generation.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // DisconnectComLogicalLink's own cancel_link_cops call cancels
    // cop_handle_1 immediately (first-wins over handle_start_comm's own,
    // later, re-validation bail-out). Checked here, before any further
    // event-draining wait_for_event calls below, so it is not silently
    // discarded by one of them.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cop_handle_1))
        .await,
        "cll_a's (stale) CoptStartcomm arm should have received PduCopstCancelled after the \
         disconnect race"
    );

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("reconnecting cll_a on the same shared channel should succeed");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a's reconnect at the same DATA_RATE should rejoin the same physical channel -- no \
         second PassThruConnect"
    );

    // Past the full 250ms CP_P3Func deadline (measured from the seed send)
    // with margin: wait_for_p3_gap has returned Ready by now, giving the
    // stale arm's post-wait re-validation a chance to run (and, pre-fix,
    // giving its stale pre-wait 0x3E payload a chance to go out on the
    // rejoined channel for what is now cll_a's brand-new session).
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "no stale tester-present frame should have gone out -- only the CP_P3Func seed send \
         should be present; the stale arm (disconnected, then reconnected onto the SAME shared \
         channel) must be suppressed by connect_generation, since channel_id alone matches again"
    );

    // No further CopStatus event at all for the stale arm's cop_handle: the
    // disconnect's own cancel_link_cops already won the first-wins race for
    // PduCopstCancelled, so handle_start_comm's own bail-out must stay
    // silent, not emit a duplicate or a stray PduCopstFinished.
    assert!(
        !wait_for_event(&mut events, 200, |item| item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cop_handle_1))
        .await,
        "no further CopStatus event should be emitted for the stale arm's cop_handle"
    );

    drop(events);
    server.shutdown().await;
}

/// `PduCopstFinished` predicate for a specific `cop_handle`, used below to
/// prove a COP has *not yet* completed (as opposed to `wait_for_cop_finished`,
/// which waits for it to).
fn is_cop_finished_for(item: &vci_service_interface::EventItem, cop_handle: u32) -> bool {
    matches!(
        item.data,
        Some(event_item::Data::CopStatus(status))
            if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
    ) && item
        .cop_handle
        .as_ref()
        .is_some_and(|h| h.cop_handle == cop_handle)
}

/// Codex-review finding (seventh round, ADR-083, "Driven from every
/// long-running per-tick poll loop..."): `dispatch_due_tester_present`
/// was originally only called from `poll_channel_events`'s outer
/// `tokio::select!` sleep arm, which runs only *between* `TxItem`s --
/// `handle_delay` (`CoptDelay`) has its own internal poll loop that can hold
/// the channel poll task for the client-controlled delay duration without
/// ever returning to the outer loop, during which a mode-1 sibling CLL on
/// the same physical channel could never get its idle-triggered
/// tester-present dispatched, however overdue.
///
/// cll_a arms mode-1 with a short interval; cll_b (sharing cll_a's physical
/// channel) then issues a single `CoptDelay` long enough to span several of
/// cll_a's intervals with generous margin to spare. `StartComPrimitive` for
/// the delay returns as soon as the item is enqueued -- well before the
/// delay itself elapses on the poll task -- so everything below runs while
/// the poll task is actually inside `handle_delay`'s own loop, not after it
/// returns.
///
/// Timing assertions here deliberately avoid anchoring to the wall-clock
/// instant the delay was issued: RPC round trips and prior setup calls can
/// eat an unpredictable (and, under a loaded test run, sometimes
/// non-trivial) amount of cll_a's idle window before the delay is even
/// issued, so asserting "the Nth fire lands near N*interval since the delay
/// started" is flaky. What is robust regardless of that startup jitter:
/// every fire resets cll_a's own `last_fired` anchor, so the *gap between
/// consecutive fires* should track the configured interval, and -- the
/// direct, timing-independent proof this test relies on -- cll_b's
/// `CoptDelay` COP must still be executing (no `PduCopstFinished` observed
/// for it) once several fires have already landed.
#[tokio::test]
#[serial]
async fn send_type_1_sibling_fires_during_a_long_copt_delay_not_after_it() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    const INTERVAL_MS: u32 = 50;
    const DELAY_MS: u32 = 800;
    const FIRES_TO_OBSERVE: usize = 4;

    // cll_a: mode-1 (idle-triggered), short interval.
    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(
        &mut client,
        cll_a,
        CP_TESTER_PRESENT_TIME,
        INTERVAL_MS * 1000,
    )
    .await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // cll_b: shares cll_a's physical channel, used only to pin the poll
    // task's dispatch loop inside handle_delay's own internal loop for the
    // whole duration below.
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    // Subscribed before the CoptDelay is started (required by
    // wait_for_cop_finished's own doc comment, and equally required here so
    // its PduCopstFinished cannot be missed).
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;

    // ADR-084: cll_a's own CoptStartcomm arm already sent its first
    // tester-present frame immediately, synchronously, before
    // PduCopstFinished -- well before the observation loop below even
    // starts. That immediate arm-time frame is deliberately excluded from
    // the "fires during the delay" timing analysis below (it predates the
    // CoptDelay entirely), but is still asserted as the sole first frame.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Issue the long CoptDelay on cll_b -- StartComPrimitive returns as soon
    // as the item is enqueued, well before the delay itself elapses, so the
    // assertions below run concurrently with the poll task's own
    // handle_delay loop, not after it returns.
    let delay_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptDelay as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: DELAY_MS,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptDelay) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    // Record the wall-clock instant each of cll_a's *idle-triggered* fires
    // (i.e. excluding the immediate arm-time frame already observed above)
    // actually landed, relative to the previous fire (not to when the delay
    // was issued -- see the function doc comment on why an absolute anchor
    // is flaky here). `target` starts at 2 (written_count already at 1 from
    // the immediate arm-time frame).
    let mut fire_times = Vec::with_capacity(FIRES_TO_OBSERVE);
    let observation_started = std::time::Instant::now();
    for target in 2..=(FIRES_TO_OBSERVE + 1) {
        wait_for_written_count(&server, MOCK_CHANNEL_ID, target).await;
        fire_times.push(observation_started.elapsed());
    }

    // The direct, timing-independent proof: cll_b's CoptDelay must still be
    // executing after several of cll_a's fires have already gone out --
    // dispatch happened WHILE the delay was in progress, not only once it
    // completed and control returned to poll_channel_events's outer loop.
    assert!(
        !wait_for_event(&mut events_b, 20, |item| is_cop_finished_for(
            item,
            delay_cop_handle
        ))
        .await,
        "cll_b's {DELAY_MS}ms CoptDelay must still be executing after {FIRES_TO_OBSERVE} of \
         cll_a's tester-present fires were observed -- if it had already finished, these fires \
         could just as well have been dispatched from the outer select loop after handle_delay \
         returned, which would not exercise the fix"
    );

    // Consecutive fires should be spaced close to cll_a's own interval --
    // proving sustained periodic firing through the wait, not a single delayed
    // burst. Each fire resets cll_a's own last_fired anchor, so this gap is
    // independent of how much of the interval had already elapsed before the
    // delay was even issued.
    for pair in fire_times.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            gap >= std::time::Duration::from_millis((INTERVAL_MS / 2) as u64)
                && gap <= std::time::Duration::from_millis((INTERVAL_MS * 3) as u64),
            "consecutive tester-present fires from cll_a should be spaced close to its own \
             {INTERVAL_MS}ms interval, not bunched together or spread arbitrarily far apart \
             (observed gap {gap:?}; fire times were {fire_times:?})"
        );
    }

    // Every observed frame (including the immediate arm-time one at index 0)
    // is cll_a's own tester-present message -- cll_b never armed mode-1 at
    // all.
    let mut expected_frame = 0x7DF_u32.to_be_bytes().to_vec();
    expected_frame.push(0x3E);
    for index in 0..=FIRES_TO_OBSERVE {
        assert_eq!(
            server.backdoor.written_data(MOCK_CHANNEL_ID, index),
            expected_frame,
            "fire {index} should be cll_a's tester-present message"
        );
    }

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// Eighth-round Codex-review finding (ADR-083, "Never RX-polls unprobed from
/// inside `wait_for_expected_response`'s match-sensitive context"):
/// `dispatch_due_tester_present`'s own per-item `wait_for_p3_gap` call
/// drains RX via a plain, unprobed `poll_rx` inside its wait loop. When that
/// call is driven from inside `wait_for_expected_response`'s own poll loop
/// (rather than the outer select loop or `handle_delay`), a due, gap-blocked
/// idle-mode send could otherwise drain the very ECU response an active
/// `CoptSendrecv` is waiting to match, consuming it as unattributed
/// background RX -- `wait_for_p3_gap`'s new `defer_if_blocked` parameter
/// (`true` only at this call site) makes it return `Deferred` immediately
/// instead, without ever polling RX.
///
/// cll_a (mode-1 idle tester-present, functional addressing) shares a
/// physical channel with cll_b (physically addressed). A `CP_P3Func` gap is
/// seeded on cll_a's own addressing so that once its short idle interval
/// elapses, its due send is gated behind a still-pending, multi-hundred-ms
/// `CP_P3Func` window. cll_b then runs an IS-CYCLIC `CoptSendrecv`, whose
/// response phase never returns to `poll_channel_events`'s outer select loop
/// on its own -- every RX poll for the rest of the test happens from inside
/// `wait_for_expected_response`'s loop, the same context that drives
/// `dispatch_due_tester_present`. A real, matching ECU response for
/// cll_b's COP is injected while cll_a is due but its `CP_P3Func` gap has
/// not yet elapsed; the response must still be delivered as a match.
#[tokio::test]
#[serial]
async fn idle_tester_present_gap_wait_does_not_swallow_a_concurrent_copt_sendrecv_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a: mode-1 idle tester-present, functional addressing, will be
    // seeded with a pending CP_P3Func gap below.
    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_P3_FUNC, 300_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 50_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // cll_b: shares cll_a's physical channel, physically addressed (default
    // addressing mode -- untouched by cll_a's functional CP_P3Func bucket),
    // runs the long IS-CYCLIC CoptSendrecv that pins the poll task inside
    // wait_for_expected_response for the rest of the test.
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_unique_resp_table_and_promote(
        &mut client,
        cll_b,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // Seed the shared CP_P3Func gap bucket: a functionally-addressed,
    // no-response send from cll_a.
    start_send_recv(&mut client, cll_a, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Arms cll_a's idle clock (armed_at ~ now, close behind the seed send
    // above); no P3-gate involved in arming mode 1.
    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;

    // cll_b's IS-CYCLIC CoptSendrecv: no response window, so its receive
    // phase keeps polling (via wait_for_expected_response) until cancelled
    // -- it never returns control to the outer select loop on its own.
    let cop_handle_b = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x22, 0xF1, 0x90],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: -1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 42,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x62],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // Past cll_a's 50ms idle threshold -- dispatch_due_tester_present,
    // now being driven from cll_b's wait_for_expected_response loop on every
    // non-terminal iteration, sees cll_a as due and calls wait_for_p3_gap for
    // it -- but comfortably inside the still-pending 300ms CP_P3Func window
    // seeded above, so (pre-fix) that nested call would itself be asleep
    // inside its own unprobed poll_rx loop for a good while longer.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Inject cll_b's matching ECU response while cll_a's idle send is gated
    // behind the pending CP_P3Func gap.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );

    let mut matched: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events_b, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                matched = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "cll_b's CoptSendrecv should still see the injected ECU response as a match, even though \
         cll_a's idle tester-present send was concurrently due and gated behind a pending \
         CP_P3Func wait -- a plain (unprobed) poll_rx inside that nested wait_for_p3_gap wait \
         would otherwise drain this response as unattributed background RX and never deliver it \
         to cll_b's COP"
    );
    assert_eq!(
        matched.expect("captured").acceptance_id,
        42,
        "the delivered match should be cll_b's own expected response"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle_b),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// ADR-094 mechanism (Codex review, PR #97): `poll_channel_events`'s
/// per-physical-channel `tokio::select!` used to construct
/// `tokio::time::sleep(interval)` fresh on every loop iteration, inline as a
/// `select!` arm listed after `tx_rx.recv()`. A continuously-non-empty TX
/// queue -- e.g. a sustained stream of zero-response `CoptSendrecv`s that
/// each spend real wall-clock time inside `wait_for_p3_gap` (which
/// deliberately never calls `dispatch_due_tester_present` itself, to avoid a
/// mutual async-fn recursion cycle -- see that function's own doc comment)
/// -- kept the RX-poll/tester-present tick starved indefinitely: the sleep
/// arm never got a chance to elapse, since it was recreated
/// (never-yet-elapsed) on every pass through the loop, and `poll_rx`/
/// `dispatch_due_tester_present` were only reachable from inside that one
/// arm. This is exercised for mode 0 (`CP_TesterPresentSendType = 0`)
/// specifically because mode 0's due-check formula
/// (`tester_present_due_reference`) ignores `last_bus_activity` entirely --
/// so cll_b's own pressure traffic below cannot legitimately defer cll_a's
/// tester-present the way it could for mode 1, isolating any observed
/// starvation as purely a scheduling bug, not legitimate deferral.
#[tokio::test]
#[serial]
async fn mode_0_periodic_tick_not_starved_by_continuous_tx_queue_pressure() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a: mode-0 (the default, left unset), functionally addressed, 100ms
    // CP_TesterPresentTime.
    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 100_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // cll_b: shares cll_a's physical channel, physically addressed, with a
    // short 25ms CP_P3Phys gap -- each zero-response CoptSendrecv queued
    // below spends ~25ms inside wait_for_p3_gap's own poll loop, holding the
    // shared channel's poll task busy and its tx_rx queue continuously
    // non-empty for the whole pressure window.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_P3_PHYS, 25_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    // cll_b is a joining CLL on an already-open shared channel, so its
    // Working set (including CP_P3Phys staged above) does not auto-promote
    // to Active at Connect -- stage physical addressing and promote both
    // before the timing-sensitive section below.
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // ADR-084: cll_a's own CoptStartcomm arm sends the first tester-present
    // frame immediately, synchronously, before PduCopstFinished.
    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Pre-queue ~30 physically-addressed, zero-response CoptSendrecvs on
    // cll_b (fire-and-forget: StartComPrimitive enqueues the TxItem and
    // returns without waiting for dispatch). Each one occupies the shared
    // channel's poll task for ~25ms inside wait_for_p3_gap, keeping tx_rx
    // continuously non-empty for ~(30 - 1) * 25ms =~ 725ms.
    for _ in 0..30 {
        start_send_recv(&mut client, cll_b, vec![0x01, 0x00], 0).await;
    }

    // Comfortably longer than the ~725ms pressure window this backlog takes
    // to drain, so every queued cll_b send -- and every mode-0 due-triggered
    // tick that should have fired for cll_a during that window -- has had
    // its chance to run.
    tokio::time::sleep(std::time::Duration::from_millis(950)).await;

    let expected_a = {
        let mut e = 0x7DF_u32.to_be_bytes().to_vec();
        e.push(0x11);
        e
    };
    let total_written = server.backdoor.written_count(MOCK_CHANNEL_ID);
    let a_frame_count = (0..total_written)
        .filter(|&i| server.backdoor.written_data(MOCK_CHANNEL_ID, i) == expected_a)
        .count();

    // 1 arm-time send (already proven above via wait_for_written_count(..,
    // 1)) plus at least 5 due-triggered sends: a 100ms interval over a
    // ~725ms pressure window should yield ~6-7 additional sends if
    // unstarved; a conservative lower bound of 5 avoids flakiness. Pre-fix,
    // only the single arm-time frame ever appears during the whole pressure
    // window -- the sleep(interval) arm, recreated fresh on every loop
    // iteration, never gets a chance to elapse while tx_rx stays
    // continuously non-empty (this is NOT fixed by removing `biased`: an
    // unbiased select between one always-ready arm and one never-yet-elapsed
    // arm still always picks the ready one).
    assert!(
        a_frame_count >= 6,
        "expected at least 5 due-triggered mode-0 tester-present sends (plus the 1 arm-time send) \
         during the ~725ms TX-queue pressure window, got {a_frame_count} total cll_a frames -- the \
         RX-poll/tester-present tick appears to be starved by continuous tx_rx pressure"
    );

    drop(events_a);
    server.shutdown().await;
}

/// Third Codex-review finding on this mechanism (regression pair, mode 0):
/// `PDU_IOCTL_CLEAR_PERIODIC_MSGS` explicitly clears an armed mode-0
/// tester-present (`TesterPresentState::Cleared`). A later, entirely
/// UNRELATED `CoptUpdateparam` (promoting `CP_Loopback`) must not resurrect
/// it -- pre-fix, the re-arm gate's `token == TesterPresentToken::None`
/// check could not tell "never configured" apart from "explicitly cleared",
/// so an unrelated promotion saw a still-non-empty tester-present payload
/// and treated it as a legitimate first-enable (ADR-084), re-arming it. Mode
/// 0 is a NEWLY reachable regression here: ADR-083 removed mode 0's old
/// `PassThruStartPeriodicMsg`-backed "a clear is durable at the hardware
/// level, with no live re-arm path at all" behavior. Mode 1 already had this
/// gate reachable before this fix -- covered by the companion
/// `send_type_1_clear_periodic_msgs_then_unrelated_updateparam_does_not_resurrect`
/// below.
#[tokio::test]
#[serial]
async fn mode_0_clear_periodic_msgs_then_unrelated_updateparam_does_not_resurrect() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // Long enough that neither a mode-0 due-triggered re-send nor ordinary
    // test-harness scheduling jitter can land inside the assertions below.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    clear_periodic_msgs(&mut client, cll_handle).await;
    assert!(
        wait_for_event(&mut events, 2000, is_tester_present_error).await,
        "PDU_IOCTL_CLEAR_PERIODIC_MSGS should emit PduErrEvtTesterPresentError for the CLL it \
         actually cleared"
    );

    // An entirely unrelated ComParam promotion on the now-Cleared CLL: LOOPBACK
    // has no bearing on tester-present's resolved data/interval/tx_flags.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::LOOPBACK, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "an unrelated CoptUpdateparam must not resurrect a Cleared tester-present"
    );

    // Wait past more than the configured 300ms interval: a resurrected
    // tester-present would due-fire somewhere in this window; a correctly
    // Cleared one stays silent regardless of elapsed time.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a Cleared tester-present must stay silent even after its old interval has elapsed"
    );

    drop(events);
    server.shutdown().await;
}

/// Mode-1 companion to
/// `mode_0_clear_periodic_msgs_then_unrelated_updateparam_does_not_resurrect`
/// -- same finding, pre-existing reachability for mode 1 (which could
/// already reach this re-arm gate before this fix's own PR, unlike mode 0).
#[tokio::test]
#[serial]
async fn send_type_1_clear_periodic_msgs_then_unrelated_updateparam_does_not_resurrect() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    clear_periodic_msgs(&mut client, cll_handle).await;
    assert!(
        wait_for_event(&mut events, 2000, is_tester_present_error).await,
        "PDU_IOCTL_CLEAR_PERIODIC_MSGS should emit PduErrEvtTesterPresentError for the CLL it \
         actually cleared"
    );

    set_com_param_unum32(&mut client, cll_handle, j2534_0404::LOOPBACK, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "an unrelated CoptUpdateparam must not resurrect a Cleared tester-present"
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a Cleared tester-present must stay silent even after its old interval has elapsed"
    );

    drop(events);
    server.shutdown().await;
}

/// `CP_TesterPresentSendType = 1`: a live `CoptUpdateparam` that actually
/// reconfigures tester-present's own resolved wire content (a new
/// `CP_TesterPresentMessage`) on a CLL whose tester-present was previously
/// `Cleared` via `PDU_IOCTL_CLEAR_PERIODIC_MSGS` must still re-arm --
/// "reconfiguring tester-present after a clear is itself a re-enable" is
/// deliberate (ADR-093), not a case this fix should over-tighten into
/// "sticky forever". Proves the fix's `old_resolved` re-arm-gate baseline
/// (now sourced from `Cleared`'s carried-forward `resolved`, not `None`)
/// still lets `same_wire_behavior` correctly report "changed" for a
/// promotion that actually alters wire content -- the positive counterpart
/// to the two regression tests above.
#[tokio::test]
#[serial]
async fn send_type_1_relevant_updateparam_after_clear_resends_and_rearms() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    clear_periodic_msgs(&mut client, cll_handle).await;
    assert!(
        wait_for_event(&mut events, 2000, is_tester_present_error).await,
        "PDU_IOCTL_CLEAR_PERIODIC_MSGS should emit PduErrEvtTesterPresentError"
    );

    // Anchored right before the reconfiguring promotion: the re-arm's
    // immediate send happens synchronously inside it.
    let started = std::time::Instant::now();
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x01],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a CoptUpdateparam that changes tester-present's resolved behavior should immediately \
         re-arm a Cleared CLL, sending a frame with the new payload"
    );
    let mut expected_new = 0x7DF_u32.to_be_bytes().to_vec();
    expected_new.push(0x01);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_new,
        "the re-armed send should carry the newly-promoted message content"
    );

    // Subsequent interval-based sends continue from the re-arm -- this fix
    // must not over-tighten "Cleared" into a state nothing can ever escape.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let elapsed = started.elapsed();
    assert!(
        (250..800).contains(&elapsed.as_millis()),
        "the third frame should fire ~300ms after the re-arm, not be suppressed by the prior \
         Cleared state (waited {elapsed:?})"
    );

    drop(events);
    server.shutdown().await;
}

/// `PDU_IOCTL_CLEAR_PERIODIC_MSGS` followed by a full `CoptStopcomm` + fresh
/// `CoptStartcomm` cycle re-arms tester-present normally -- the
/// always-legitimate re-enable path `handle_start_comm`'s arm gives
/// unconditionally (ADR-084), regardless of whatever `tester_present_state`
/// variant (`Cleared`, `None`, or `Armed`) preceded it.
#[tokio::test]
#[serial]
async fn mode_0_fresh_startcomm_after_clear_rearms_normally() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // Deliberately huge: no due-triggered re-send should enter the picture
    // while this test is exercising the clear/stop/start sequence.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    clear_periodic_msgs(&mut client, cll_handle).await;
    assert!(
        wait_for_event(&mut events, 2000, is_tester_present_error).await,
        "PDU_IOCTL_CLEAR_PERIODIC_MSGS should emit PduErrEvtTesterPresentError"
    );

    stop_comm(&mut client, cll_handle, &mut events).await;

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.push(0x3E);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected,
        "the fresh CoptStartcomm should re-arm and immediately send, the same \"just became \
         enabled\" contract any first enable gets"
    );

    drop(events);
    server.shutdown().await;
}

/// A second `PDU_IOCTL_CLEAR_PERIODIC_MSGS` on an already-`Cleared` CLL is a
/// no-op: `PduErrEvtTesterPresentError` fires exactly once, for the CLL that
/// actually had something to clear -- matching the pre-existing behavior for
/// a repeat clear on an already-`None` CLL (an unconfigured CLL never
/// resulted in a second error event either).
#[tokio::test]
#[serial]
async fn clear_periodic_msgs_twice_emits_error_event_once() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    clear_periodic_msgs(&mut client, cll_handle).await;
    assert!(
        wait_for_event(&mut events, 2000, is_tester_present_error).await,
        "the first CLEAR_PERIODIC_MSGS should emit PduErrEvtTesterPresentError"
    );

    clear_periodic_msgs(&mut client, cll_handle).await;
    assert!(
        !wait_for_event(&mut events, 300, is_tester_present_error).await,
        "a repeat CLEAR_PERIODIC_MSGS on an already-Cleared CLL must not re-emit the error event"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-095 audit round (4th Codex-review finding on this mechanism plus a
/// full-surface re-audit): `isotp_send`'s FlowControl (N_Bs) wait loop must
/// itself dispatch due tester-present sends for a SIBLING CLL sharing the
/// same physical channel, or a long N_Bs wait (default 1000ms, `CP_Bs`)
/// silently starves mode-0 (now software-dispatched, ADR-093/090)
/// tester-present for the whole wait. cll_b never receives an injected
/// FlowControl, so its FirstFrame is the only frame it ever gets onto the
/// wire before N_Bs expires with `PduErrEvtRxTimeout` -- the entire ~1000ms
/// gap is spent inside the fixed loop this test targets.
#[tokio::test]
#[serial]
async fn isotp_fc_wait_dispatches_due_tester_present_for_a_sibling_cll() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    // cll_a: mode-0 (default, left unset), functionally addressed, 100ms
    // CP_TesterPresentTime.
    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 100_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // cll_b: shares cll_a's physical channel, physically addressed. `CP_Bs`
    // (N_Bs) is deliberately left unset so the FC-wait below runs for the
    // full default 1000ms window.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A 20-byte payload segments into FirstFrame + ConsecutiveFrames; no
    // FlowControl is ever injected, so the FirstFrame is the only frame from
    // this send that reaches the wire before N_Bs expires.
    let payload: Vec<u8> = (1..=20).collect();
    let cll_b_cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_can_id_frame(&server, MOCK_CHANNEL_ID, 0x7E0).await;

    // No FlowControl is ever injected, so cll_b's COP only finishes once
    // N_Bs (default 1000ms) expires with PduErrEvtRxTimeout. Qualified by
    // `cll_b_cop`: `set_can_phys_req_id_and_promote(cll_b, ...)` above ran
    // (and finished) before `events_b` subscribed, so its own
    // `CoptUpdateparam`'s backlogged `PduCopstFinished` is delivered live
    // first (P2 backlog follow-up, `docs/implementation-notes.md`); an
    // unqualified wait would match that instead of this COP.
    wait_for_cop_finished_matching(&mut events_b, cll_b_cop).await;

    let expected_a_prefix = 0x7DF_u32.to_be_bytes();
    let total_written = server.backdoor.written_count(MOCK_CHANNEL_ID);
    let a_frame_count = (0..total_written)
        .filter(|&i| server.backdoor.written_data(MOCK_CHANNEL_ID, i)[..4] == expected_a_prefix)
        .count();

    // 1 arm-time send plus at least 3 due-triggered sends over the ~1000ms
    // N_Bs wait at a 100ms interval.
    assert!(
        a_frame_count >= 4,
        "expected at least 3 due-triggered mode-0 tester-present sends (plus the 1 arm-time \
         send) from cll_a while cll_b's isotp_send was blocked in its FlowControl (N_Bs) wait, \
         got {a_frame_count} total cll_a frames -- isotp_send's FC-wait loop appears not to be \
         dispatching due tester-present sends"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// PR #97 8th Codex review round (ADR-095 amendment): the FC-wait loop above
/// must not let a dispatched tester-present send's own real write I/O widen
/// the window between a probed poll and the N_Bs timeout decision. Before the
/// fix, the loop checked the deadline AFTER dispatching; since
/// `dispatch_due_tester_present`'s send is real (simulated) hardware write
/// I/O that takes wall-clock time, an FC that lands on the wire *during*
/// that write could sit uncaptured while the loop broke out on a
/// now-past deadline, without ever polling again to see it -- a spurious
/// timeout despite the real FlowControl having already arrived. This test
/// uses `arm_write_rx_injection` to land cll_b's FlowControl exactly during
/// cll_a's due tester-present write, with a write hold longer than the
/// remaining N_Bs budget, and asserts the fixed ordering's one-poll grace
/// period still catches it.
#[tokio::test]
#[serial]
async fn isotp_fc_wait_catches_fc_landing_during_a_dispatched_tester_present_write() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    // cll_a: mode-0 (default, left unset), functionally addressed, 100ms
    // CP_TesterPresentTime -- same setup as the sibling-starvation test
    // above, so its due tester-present dispatch lands mid-way through
    // cll_b's N_Bs (default 1000ms) window.
    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 100_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // cll_b: shares cll_a's physical channel, physically addressed to
    // 0x7E0. `CP_Bs` (N_Bs) is deliberately left unset (default 1000ms).
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A 20-byte payload segments into FirstFrame + ConsecutiveFrames.
    let payload: Vec<u8> = (1..=20).collect();
    let cll_b_cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    // Wait for cll_b's FirstFrame to land, then arm a one-shot RX injection:
    // the NEXT write on this channel (expected to be cll_a's next due
    // tester-present send, around the ~100ms mark) pushes the FlowControl
    // into the RX queue and then blocks the mock write call for 1500ms --
    // comfortably longer than the remaining N_Bs (1000ms) budget -- so
    // wall-clock time is pushed past the N_Bs deadline before the fix's
    // grace poll runs.
    wait_for_can_id_frame(&server, MOCK_CHANNEL_ID, 0x7E0).await;
    let fc = can_frame(0x7E8, &[0x30, 0x00, 0x00]);
    server
        .backdoor
        .arm_write_rx_injection(MOCK_CHANNEL_ID, &fc, j2534_0404::CAN, 1500);

    // With the fix, cll_b's COP must finish successfully (the grace poll
    // after the overrunning dispatch catches the FC) -- no PduErrEvtRxTimeout
    // along the way.
    let mut saw_rx_timeout = false;
    assert!(
        wait_for_event(&mut events_b, 4000, |item| {
            if matches!(
                item.data,
                Some(event_item::Data::ErrorData(error))
                    if error == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32
            ) {
                saw_rx_timeout = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
            ) && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cll_b_cop.cop_handle)
        })
        .await,
        "cll_b's COP should finish once the FC-wait loop's grace poll catches the injected FC"
    );
    assert!(
        !saw_rx_timeout,
        "the FC landed during cll_a's dispatched tester-present write and must be caught by the \
         fix's post-dispatch grace poll, not lost to a spurious N_Bs timeout"
    );

    // Proof the transfer actually proceeded past the FlowControl: a
    // ConsecutiveFrame addressed to 0x7E0 must be on the wire.
    let expected_cf_prefix = 0x7E0_u32.to_be_bytes();
    let total_written = server.backdoor.written_count(MOCK_CHANNEL_ID);
    let saw_cf = (0..total_written).any(|i| {
        let frame = server.backdoor.written_data(MOCK_CHANNEL_ID, i);
        frame.len() >= 5 && frame[..4] == expected_cf_prefix && frame[4] == 0x21
    });
    assert!(
        saw_cf,
        "expected a ConsecutiveFrame (PCI 0x21) addressed to 0x7E0 on the wire, proving the \
         injected FC was actually captured and the transfer proceeded"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// ADR-095 audit round: the SAME `isotp_send` FC-wait fix above must NOT
/// fire a tester-present armed on the CLL whose OWN in-flight transfer is
/// creating the wait -- `exclude_cll` (paired with `isotp_send`'s
/// `Some(cll_handle)` argument) suppresses exactly this CLL's own due-check.
/// Without it, a same-CAN-ID single-frame tester-present spliced into the
/// middle of this CLL's own FirstFrame/FlowControl sequence would look like
/// an unexpected SF to the receiving ECU (ISO 15765-2), corrupting the very
/// transfer the dispatch call is running inside of.
#[tokio::test]
#[serial]
async fn isotp_fc_wait_excludes_the_in_flight_clls_own_tester_present() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    // cll_b: mode-0 tester-present armed on ITSELF (100ms interval),
    // physically addressed -- the same CLL that shortly runs its own
    // multi-frame ISO-TP send below.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_bytes(&mut client, cll_b, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_TIME, 100_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_b).await;
    wait_for_cop_finished(&mut events_b).await;
    // ADR-084: the arm-time send fires immediately, synchronously.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A 20-byte payload segments into FirstFrame + ConsecutiveFrames; no
    // FlowControl is ever injected, so the FirstFrame is the only frame from
    // this send that reaches the wire before N_Bs (default 1000ms) expires.
    let payload: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    // Sample well BEFORE N_Bs (default 1000ms) can have expired --
    // comfortably inside isotp_send's own FlowControl wait, so exclude_cll's
    // suppression is still active for the whole 800ms. Sampling any later
    // (e.g. after wait_for_cop_finished) would race the COP's own terminal
    // event against the entirely legitimate next due-fire that resumes for
    // cll_b the instant its transfer completes and control returns to the
    // outer poll loop -- exclude_cll intentionally no longer applies once
    // isotp_send has returned, so that later fire is not a violation and
    // must not be conflated with one here.
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    let total_written = server.backdoor.written_count(MOCK_CHANNEL_ID);
    assert_eq!(
        total_written, 2,
        "cll_b's own mode-0 tester-present must not have fired for itself while its own \
         FirstFrame/FlowControl (N_Bs) wait was in flight -- exclude_cll should have suppressed \
         every due-check for this CLL during that window; got {total_written} frames written \
         (expected exactly the arm-time send and the FirstFrame)"
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1)[..4],
        0x7E0_u32.to_be_bytes(),
        "the second frame should be cll_b's own FirstFrame"
    );

    // Let the COP finish (N_Bs eventually expires with PduErrEvtRxTimeout) so
    // the server can be shut down cleanly.
    wait_for_cop_finished(&mut events_b).await;

    drop(events_b);
    server.shutdown().await;
}

/// ADR-095 audit round: `isotp_send`'s STmin-paced ConsecutiveFrame loop
/// (the SECOND `isotp_send` insertion, distinct from the FC-wait loop above)
/// must itself dispatch due tester-present sends for a sibling CLL --
/// interleaved AMONG the ConsecutiveFrame writes, not merely once before the
/// first or after the last (which a fix only at the FC-wait site, or a fix
/// only at the outer poll loop, would not prove).
#[tokio::test]
#[serial]
async fn isotp_stmin_loop_interleaves_due_tester_present_among_consecutive_frames() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 100_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // 111 bytes: Normal addressing's FirstFrame carries 6, leaving exactly
    // 15 ConsecutiveFrames of 7 bytes each.
    let payload: Vec<u8> = (1..=111).collect();
    let cll_b_cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_can_id_frame(&server, MOCK_CHANNEL_ID, 0x7E0).await;

    // ECU answers with FlowControl: ContinueToSend, BS=0 (send all 15 CFs
    // back-to-back), STmin=127ms -- ~15 * 127ms =~ 1.9s total CF-sending time.
    let mut fc = 0x7E8_u32.to_be_bytes().to_vec();
    fc.extend_from_slice(&[0x30, 0x00, 0x7F]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fc, j2534_0404::CAN);

    // cll_b's COP finishes as soon as the last ConsecutiveFrame is written
    // (NumReceiveCycles == 0, the default here). A longer-than-usual timeout
    // (this file's own wait_for_cop_finished hardcodes 2000ms) to comfortably
    // cover the ~1.9s CF-sending window plus overhead.
    assert!(
        wait_for_event(&mut events_b, 4000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cll_b_cop.cop_handle))
        .await,
        "expected cll_b's CoptSendrecv to finish after all 15 ConsecutiveFrames were sent"
    );

    let total_written = server.backdoor.written_count(MOCK_CHANNEL_ID);
    let mut cf_indices = Vec::new();
    let mut a_indices = Vec::new();
    for i in 0..total_written {
        let data = server.backdoor.written_data(MOCK_CHANNEL_ID, i);
        if data[..4] == 0x7E0_u32.to_be_bytes() && (data[4] & 0xF0) == 0x20 {
            cf_indices.push(i);
        } else if data[..4] == 0x7DF_u32.to_be_bytes() {
            a_indices.push(i);
        }
    }
    assert_eq!(
        cf_indices.len(),
        15,
        "expected all 15 ConsecutiveFrames to have been written, got indices {cf_indices:?}"
    );
    let first_cf = *cf_indices.first().unwrap();
    let last_cf = *cf_indices.last().unwrap();
    assert!(
        a_indices.iter().any(|&i| i > first_cf && i < last_cf),
        "expected at least one cll_a tester-present frame interleaved strictly between the \
         first ({first_cf}) and last ({last_cf}) ConsecutiveFrame writes, not just before/after \
         -- got cll_a frames at indices {a_indices:?} and ConsecutiveFrames at {cf_indices:?}"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// PR #97 sixth Codex review round (design-advisor-approved fix): a
/// DIFFERENT sibling CLL sharing the same physical channel AND configured
/// with the SAME target CAN ID as an in-flight `isotp_send` transfer must
/// also be excluded from firing its own tester-present mid-transfer --
/// `exclude_cll` alone (paired with `isotp_send`'s `Some(cll_handle)`
/// argument) only excludes the in-flight CLL's OWN tester-present by CLL
/// handle, not a same-CAN-ID sibling's. Without `exclude_isotp_target`
/// (compared against `isotp_send`'s own frozen TX wire bytes, not
/// `resolved.target_can_ids` -- see `InFlightIsoTpTarget`'s doc comment in
/// `events.rs`), cll_a's tester-present would still splice a same-CAN-ID
/// SingleFrame into the middle of cll_b's FirstFrame/FlowControl (N_Bs)
/// sequence, aborting the ECU's in-progress ISO-TP reassembly -- ADR-095's
/// same failure mode, but for a sibling rather than the in-flight CLL
/// itself.
#[tokio::test]
#[serial]
async fn isotp_fc_wait_excludes_a_sibling_clls_tester_present_at_the_same_wire_target() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    // cll_a: mode-0 tester-present armed on ITSELF, physically addressed to
    // the SAME CAN ID (0x7E0) cll_b's in-flight transfer below targets --
    // the specific collision this fix closes. A short 300ms interval so a
    // regression (the pre-fix behavior) would visibly fire at least twice
    // during the ~700ms sample window below.
    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 300_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    // cll_b: shares cll_a's physical channel, ALSO physically addressed to
    // 0x7E0. `CP_Bs` (N_Bs) is deliberately left unset so the FC-wait below
    // runs for the full default 1000ms window.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;
    // ADR-084: the arm-time send fires immediately, synchronously.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A 20-byte payload segments into FirstFrame + ConsecutiveFrames; no
    // FlowControl is ever injected, so the FirstFrame is the only frame from
    // this send that reaches the wire before N_Bs (default 1000ms) expires.
    let payload: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    // cll_a's tester-present and cll_b's FirstFrame share the same CAN-ID
    // prefix (0x7E0) on the wire, so `wait_for_can_id_frame` cannot
    // distinguish them -- wait for the total written count to reach 2 (the
    // arm-time send plus cll_b's FirstFrame) instead, and snapshot from
    // there: the assertion below compares the count-delta since this
    // snapshot, not a re-filter on the shared CAN-ID prefix.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let snapshot = server.backdoor.written_count(MOCK_CHANNEL_ID);

    // Sleep comfortably inside the ~1000ms N_Bs window: long enough that, at
    // cll_a's 300ms interval, a regression would fire at least twice more.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;

    let total_written = server.backdoor.written_count(MOCK_CHANNEL_ID);
    assert_eq!(
        total_written, snapshot,
        "cll_a's tester-present must not have fired for the same wire target (0x7E0) cll_b's own \
         in-flight FirstFrame/FlowControl (N_Bs) wait was targeting -- exclude_isotp_target should \
         have suppressed every due-check collision during that window; got {total_written} frames \
         written, expected the {snapshot} already on the wire when this sample started"
    );

    // Let the COP finish (N_Bs eventually expires with PduErrEvtRxTimeout) so
    // the server can be shut down cleanly.
    wait_for_cop_finished(&mut events_b).await;

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// PR #97 sixth Codex review round (companion to the same-wire-target test
/// above): `exclude_isotp_target` must only suppress a sibling CLL whose own
/// tester-present would collide with the in-flight transfer's actual wire
/// target -- proven here with the same physical-addressing construction as
/// the test above, but with cll_a targeting a DIFFERENT CAN ID (0x7E4) than
/// cll_b's in-flight transfer (0x7E0): cll_a's tester-present must keep
/// firing normally throughout cll_b's FlowControl (N_Bs) wait, proving the
/// new exclusion is scoped to actual same-target collisions and not overly
/// broad.
#[tokio::test]
#[serial]
async fn isotp_fc_wait_still_dispatches_a_sibling_at_a_different_wire_target() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    // cll_a: mode-0, physically addressed to 0x7E4 -- a DIFFERENT CAN ID
    // than cll_b's in-flight transfer target below.
    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 100_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E4).await;

    // cll_b: shares cll_a's physical channel, physically addressed to 0x7E0.
    // `CP_Bs` (N_Bs) is deliberately left unset so the FC-wait below runs
    // for the full default 1000ms window.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let startcomm_cop = start_comm(&mut client, cll_a).await;
    wait_for_cop_finished_matching(&mut events_a, startcomm_cop).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A 20-byte payload segments into FirstFrame + ConsecutiveFrames; no
    // FlowControl is ever injected, so the FirstFrame is the only frame from
    // this send that reaches the wire before N_Bs expires.
    let payload: Vec<u8> = (1..=20).collect();
    let cll_b_cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_can_id_frame(&server, MOCK_CHANNEL_ID, 0x7E0).await;

    // No FlowControl is ever injected, so cll_b's COP only finishes once
    // N_Bs (default 1000ms) expires with PduErrEvtRxTimeout.
    wait_for_cop_finished_matching(&mut events_b, cll_b_cop).await;

    let expected_a_prefix = 0x7E4_u32.to_be_bytes();
    let total_written = server.backdoor.written_count(MOCK_CHANNEL_ID);
    let a_frame_count = (0..total_written)
        .filter(|&i| server.backdoor.written_data(MOCK_CHANNEL_ID, i)[..4] == expected_a_prefix)
        .count();

    // 1 arm-time send plus at least 3 due-triggered sends over the ~1000ms
    // N_Bs wait at a 100ms interval.
    assert!(
        a_frame_count >= 4,
        "expected at least 3 due-triggered mode-0 tester-present sends (plus the 1 arm-time \
         send) from cll_a (0x7E4) while cll_b's isotp_send was blocked in its FlowControl (N_Bs) \
         wait targeting a DIFFERENT CAN ID (0x7E0) -- exclude_isotp_target must be scoped to \
         actual same-target collisions, not overly broad; got {a_frame_count} total cll_a frames"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// ADR-095 audit round (Fix D): a `PDU_IOCTL_SUSPEND_TX_QUEUE` /
/// `PDU_IOCTL_RESUME_TX_QUEUE` backlog drain runs entirely inside
/// `drain_tx_held_backlog`'s own loop, triggered by a single `TxItem::ResumeWake`
/// -- unlike the ADR-094 mechanism this reuses the technique of (a
/// continuous stream of zero-response `CoptSendrecv`s, each paced by
/// `CP_P3Phys`, occupying the shared channel's poll task), this test routes
/// the pressure through `tx_held` so it never even touches the outer
/// `tx_rx.recv()` select arm; only `drain_tx_held_backlog`'s own per-item
/// `run_due_tick_duties` call (threaded through as of this fix) can keep
/// cll_a's mode-0 tester-present alive during the drain.
#[tokio::test]
#[serial]
async fn mode_0_periodic_tick_not_starved_by_tx_held_backlog_drain() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 50_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_P3_PHYS, 25_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_b, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    // Pre-queue DRAIN_ITEMS physically-addressed, zero-response CoptSendrecvs
    // on the suspended cll_b: each is dequeued from tx_rx almost immediately
    // and diverted straight into tx_held (never reaching the mock while
    // suspended), rather than occupying tx_rx/the outer select the way the
    // ADR-094 test's un-suspended pressure does. Each drained item then waits
    // out cll_b's 25ms CP_P3Phys.
    //
    // The assertion below is on the ORDER of frames on the wire, not on how
    // many cll_a frames arrive in a wall-clock window. A count over a window
    // depends on the platform timer (a 25ms wait stretches to about 31ms with
    // the 15.6ms Windows granularity) and on how long a loaded runner stalls
    // the poll task, so any threshold loose enough to be reliable there also
    // lets a degraded drain through. A stall lengthens one item's wait but
    // leaves the order of the frames unchanged.
    const DRAIN_ITEMS: usize = 60;
    for _ in 0..DRAIN_ITEMS {
        start_send_recv(&mut client, cll_b, vec![0x01, 0x00], 0).await;
    }

    // Give the poll task ample time to dequeue and divert all of them into
    // cll_b's tx_held backlog. cll_a's own mode-0 tester-present keeps
    // firing throughout this window (sharing the same physical channel), so
    // check for the ABSENCE of cll_b's own CAN ID specifically, not a raw
    // written_count -- a bare count would conflate cll_a's expected
    // continued firing with a (would-be) suspend-queue regression.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let expected_b_prefix = 0x7E0_u32.to_be_bytes();
    let total_written_while_suspended = server.backdoor.written_count(MOCK_CHANNEL_ID);
    assert!(
        (0..total_written_while_suspended)
            .all(|i| server.backdoor.written_data(MOCK_CHANNEL_ID, i)[..4] != expected_b_prefix),
        "no cll_b sends should reach the wire while its TX queue is suspended"
    );

    // Only frames written from here on belong to the drain.
    let first_drain_index = server.backdoor.written_count(MOCK_CHANNEL_ID);

    io_ctl_cll(&mut client, cll_b, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    // Wait for the whole backlog to reach the wire. The drain runs inside one
    // `drain_tx_held_backlog` call, so the outer loop's own post-select tick
    // cannot run until it returns: any cll_a frame between two cll_b frames
    // can only have come from the per-item `run_due_tick_duties` call.
    let drain_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let drain_frames: Vec<bool> = loop {
        let total = server.backdoor.written_count(MOCK_CHANNEL_ID);
        let frames: Vec<bool> = (first_drain_index..total)
            .map(|i| server.backdoor.written_data(MOCK_CHANNEL_ID, i)[..4] == expected_b_prefix)
            .collect();
        if frames.iter().filter(|&&is_b| is_b).count() >= DRAIN_ITEMS {
            break frames;
        }
        assert!(
            tokio::time::Instant::now() < drain_deadline,
            "cll_b's backlog should drain after PDU_IOCTL_RESUME_TX_QUEUE, got {} of \
             {DRAIN_ITEMS} frames",
            frames.iter().filter(|&&is_b| is_b).count()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };

    // Split the drain into runs of cll_b frames separated by cll_a frames:
    // the leading run (before the first cll_a frame), the interior runs
    // (between two cll_a frames) and the trailing run.
    //
    // Why 3 is the hard bound for every run: cll_b's CP_P3Phys wait is
    // anchored on the previous cll_b write, not on the start of the item and
    // not on cll_a's send. Right after a cll_a send, the next cll_b item may
    // already be partly waited out, and the due check after the second item
    // (50ms after the send) races with the overhead of the tick itself, so a
    // run of 2 is the usual result and a run of 3 happens when a hiccup of a
    // few milliseconds lands in that window (the same thing happens once at
    // the start, depending on how long before the resume cll_a last sent).
    // After a third item the send is always due, so no run is longer.
    const MAX_B_RUN: usize = 3;
    let mut runs = Vec::new();
    let mut current_run = 0;
    for &is_b in &drain_frames {
        if is_b {
            current_run += 1;
        } else {
            runs.push(current_run);
            current_run = 0;
        }
    }
    let trailing_run = current_run;
    let a_frames = runs.len();
    assert!(
        a_frames > 0,
        "cll_a's tester-present should send during the drain, but {DRAIN_ITEMS} cll_b frames \
         went out with none between them -- drain_tx_held_backlog is starving the \
         RX-poll/tester-present tick while it drains cll_b's backlog"
    );
    let leading_run = runs[0];
    let interior_runs = &runs[1..];
    let longest_b_run = runs
        .iter()
        .copied()
        .chain([trailing_run])
        .max()
        .unwrap_or_default();
    assert!(
        longest_b_run <= MAX_B_RUN,
        "expected a due mode-0 tester-present send from cll_a at least every {MAX_B_RUN} drained \
         cll_b items (50ms period, 25ms per item), but {longest_b_run} cll_b frames went out in a \
         row (leading {leading_run}, interior {interior_runs:?}, trailing {trailing_run}) -- \
         drain_tx_held_backlog appears to be starving the RX-poll/tester-present tick while it \
         drains cll_b's backlog"
    );

    // A drain that ticks only after every 3rd item gives B,B,B,A over and
    // over: every run is 3, which the hard bound above allows. The unregressed
    // service reaches 3 only when a hiccup lands in that window, a few times
    // at most in 29 cycles, and never because of the order of the work, which
    // a stalled runner does not change. Allow at most a quarter of the
    // interior runs to be 3 (the same hiccup, or the pre-resume phase noted
    // above); a throttled tick makes all of them 3.
    let interior_threes = interior_runs
        .iter()
        .filter(|&&run| run == MAX_B_RUN)
        .count();
    assert!(
        interior_threes * 4 <= interior_runs.len(),
        "expected most of the cll_b runs between two cll_a tester-present sends to be 2 (50ms \
         period, 25ms per item), but {interior_threes} of {} were {MAX_B_RUN} (leading \
         {leading_run}, interior {interior_runs:?}, trailing {trailing_run}) -- \
         drain_tx_held_backlog appears to run the RX-poll/tester-present tick only every \
         {MAX_B_RUN}rd item",
        interior_runs.len()
    );

    drop(events_a);
    server.shutdown().await;
}

/// ADR-095 audit round (Fix C, lower priority): the RC21/RC23
/// `request_time_ms` chunked sleep inside `wait_for_expected_response` must
/// itself dispatch due tester-present sends for a sibling CLL, the same as
/// the enclosing function's own top-level per-iteration call already does
/// (this fix closes the gap in the chunked-sleep sub-loop specifically).
#[tokio::test]
#[serial]
async fn rc21_request_time_wait_dispatches_due_tester_present_for_a_sibling_cll() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    const CP_RC21_COMPLETION_TIMEOUT: u32 = 0x8020;
    const CP_RC21_HANDLING: u32 = 0x8021;
    const CP_RC21_REQUEST_TIME: u32 = 0x8022;
    const CP_RC_BYTE_OFFSET: u32 = 0x8028;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 100_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    // Deliberately much larger than the RC21 request_time_ms wait below, so
    // the base response window can never be what ends the COP.
    set_com_param_unum32(&mut client, cll_b, j2534_0404::P2_MAX, 10_000_000).await;
    set_com_param_unum32(&mut client, cll_b, CP_RC21_HANDLING, 1).await;
    set_com_param_unum32(&mut client, cll_b, CP_RC_BYTE_OFFSET, 2).await;
    set_com_param_unum32(&mut client, cll_b, CP_RC21_REQUEST_TIME, 800).await;
    set_com_param_unum32(&mut client, cll_b, CP_RC21_COMPLETION_TIMEOUT, 5000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_unique_resp_table_and_promote(
        &mut client,
        cll_b,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
            ],
        )],
    )
    .await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x22, 0xF1, 0x90],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 1,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x62],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
    wait_for_can_id_frame(&server, MOCK_CHANNEL_ID, 0x7E0).await;

    // NRC 0x21 (BusyRepeatRequest): triggers the CP_RC21RequestTime (800ms)
    // chunked sleep before cll_b's re-request.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x22, 0x21]),
        j2534_0404::ISO15765,
    );

    // Well within the 800ms request_time_ms wait (before cll_b's re-request
    // adds another frame), cll_a's mode-0 tester-present should have fired
    // several times if the chunked sleep is dispatching due tester-present
    // sends.
    tokio::time::sleep(std::time::Duration::from_millis(750)).await;

    let expected_a_prefix = 0x7DF_u32.to_be_bytes();
    let total_written = server.backdoor.written_count(MOCK_CHANNEL_ID);
    let a_frame_count = (0..total_written)
        .filter(|&i| server.backdoor.written_data(MOCK_CHANNEL_ID, i)[..4] == expected_a_prefix)
        .count();

    assert!(
        a_frame_count >= 4,
        "expected at least 3 due-triggered mode-0 tester-present sends (plus the 1 arm-time \
         send) from cll_a during cll_b's 800ms RC21 request_time_ms wait, got {a_frame_count} \
         total cll_a frames -- the RC21 chunked sleep appears not to be dispatching due \
         tester-present sends"
    );

    drop(events_a);
    server.shutdown().await;
}

/// Pre-init tester-present top-up (PR #97 fifth Codex review round,
/// design-advisor-approved): `run_protocol_init` is one opaque blocking
/// `PassThruIoctl` call held under this shared physical channel's `ctx.api`
/// mutex for its whole duration, which would otherwise pause
/// `dispatch_due_tester_present`'s own per-tick dispatch for every OTHER CLL
/// sharing the channel -- silently stalling an already-armed mode-0 sibling's
/// keepalive on top of however much of its own interval had already elapsed.
/// `handle_start_comm` now force-fires every currently-armed mode-0 sibling
/// once, unconditionally, immediately before its own `run_protocol_init` call.
///
/// Mirrors `kline_five_baud_init_stamps_last_bus_activity_deferring_mode_1_
/// sibling`'s shared-K-line-channel setup, but with cll_a armed mode-0
/// (`CP_TesterPresentSendType == 0`) instead of mode-1, and a deliberately
/// long 300ms interval so that only ~75ms has elapsed by the time cll_b's
/// real 5-baud init runs -- nowhere near due by the ordinary due-check. If
/// the pre-init top-up fires, cll_a's second frame appears on the wire
/// synchronously, as part of cll_b's own `CoptStartcomm` processing, well
/// before cll_a's own 300ms interval could ever have elapsed naturally.
#[tokio::test]
#[serial]
async fn kline_five_baud_init_force_fires_mode_0_sibling_pre_init_topup() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a: mode-0 (periodic), armed first with a 300ms interval, on
    // ISO9141. CP_InitializationSettings = 3 skips its own init sequence, so
    // arming happens with no K-line traffic of its own.
    let cll_a = create_cll(&mut client, j2534_0404::ISO9141, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 9_600).await;
    set_com_param_unum32(&mut client, cll_a, CP_INIT_SETTINGS, 3).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 300_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 0).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // cll_b: shares cll_a's physical channel (same protocol/baud rate) and
    // runs a real spec-mandated 5-baud init on its own CoptStartcomm. No
    // CP_TesterPresentMessage configured, so its own tester-present resolves
    // to "disabled" and never enters the picture -- isolating cll_b's init as
    // the sole trigger under test.
    let cll_b = create_cll(&mut client, j2534_0404::ISO9141, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 9_600).await;
    set_com_param_unum32(&mut client, cll_b, CP_INIT_SETTINGS, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_com_param_unum32(&mut client, cll_b, CP_5BAUD_ADDR_PHYS, 0x2A).await;
    promote_via_update_param(&mut client, cll_b).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;

    // ADR-084: cll_a's own CoptStartcomm arm already sent its first
    // tester-present frame immediately, synchronously, before
    // PduCopstFinished.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Only ~75ms into cll_a's 300ms window -- nowhere near due by the
    // ordinary due-check -- start cll_b's real 5-baud init.
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    let forced_at = tokio::time::Instant::now();
    start_comm(&mut client, cll_b).await;
    wait_for_cop_finished(&mut events_b).await;
    assert_eq!(
        server.backdoor.five_baud_init_count(),
        1,
        "cll_b's spec 5-baud init should have run"
    );

    // The pre-init top-up runs synchronously, as part of cll_b's own
    // CoptStartcomm processing, strictly before run_protocol_init -- so by
    // the time cll_b's PduCopstFinished has already been observed above,
    // cll_a's forced second frame must already be on the wire. Asserted
    // without any further wait: only ~75ms (plus RPC overhead) of cll_a's
    // 300ms interval has elapsed, so this frame cannot be cll_a's own
    // ordinary due-triggered send.
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "cll_b's CoptStartcomm should have force-fired cll_a's mode-0 tester-present exactly \
         once, immediately before running its own 5-baud init"
    );
    assert!(
        server
            .backdoor
            .written_data(MOCK_CHANNEL_ID, 1)
            .ends_with(&[0x11]),
        "the forced frame should carry cll_a's tester-present payload"
    );

    // cll_a's subsequent scheduling continues normally from the forced-send
    // instant, neither stalled nor double-fired: its next due-triggered send
    // should land ~300ms after `forced_at`, not ~300ms after its original arm
    // (which would already have elapsed) and not immediately (double-fire).
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let elapsed = forced_at.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(220),
        "cll_a's next due-triggered send fired too early relative to the forced-send instant -- \
         expected it to self-correct to ~300ms after the forced send, not fire immediately \
         (waited {elapsed:?})"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(2000),
        "cll_a's next due-triggered send appears stalled well past its ~300ms interval since the \
         forced send (waited {elapsed:?})"
    );
    assert!(
        server
            .backdoor
            .written_data(MOCK_CHANNEL_ID, 2)
            .ends_with(&[0x11]),
        "the third frame should still be cll_a's tester-present message"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// Companion to `kline_five_baud_init_force_fires_mode_0_sibling_pre_init_topup`:
/// mode-1 (idle-triggered) siblings must NOT be force-fired by the same
/// pre-init call -- `force` only bypasses the due-check interval predicate for
/// `resolved.send_type == 0`. Mode 1 is already correctly deferred by
/// `run_protocol_init`'s own `last_bus_activity` stamp (ADR-083); force-firing
/// it here as well would be a double-fire, not a fix.
///
/// Same shared-K-line-channel setup as
/// `kline_five_baud_init_stamps_last_bus_activity_deferring_mode_1_sibling`,
/// but asserts the narrower, more immediate property that this fifth-round
/// fix targets: no premature cll_a frame appears purely from cll_b's init
/// starting, checked synchronously right after cll_b's CoptStartcomm
/// completes (a stronger, more immediate check than that test's own later
/// from-arm-vs-from-reset-instant timing assertions).
#[tokio::test]
#[serial]
async fn kline_five_baud_init_does_not_force_fire_mode_1_sibling() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a: mode-1 (idle-triggered), armed first with a 300ms interval --
    // clearly longer than the mock's (effectively instantaneous) 5-baud init
    // duration. CP_InitializationSettings = 3 skips its own init sequence.
    let cll_a = create_cll(&mut client, j2534_0404::ISO9141, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 9_600).await;
    set_com_param_unum32(&mut client, cll_a, CP_INIT_SETTINGS, 3).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x11]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 300_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // cll_b: shares cll_a's physical channel and runs a real 5-baud init.
    let cll_b = create_cll(&mut client, j2534_0404::ISO9141, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 9_600).await;
    set_com_param_unum32(&mut client, cll_b, CP_INIT_SETTINGS, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_com_param_unum32(&mut client, cll_b, CP_5BAUD_ADDR_PHYS, 0x2A).await;
    promote_via_update_param(&mut client, cll_b).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events_a).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Only ~75ms into cll_a's 300ms window, start cll_b's real 5-baud init.
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    start_comm(&mut client, cll_b).await;
    wait_for_cop_finished(&mut events_b).await;
    assert_eq!(
        server.backdoor.five_baud_init_count(),
        1,
        "cll_b's spec 5-baud init should have run"
    );

    // Unlike the mode-0 companion test, no second frame should appear here:
    // mode-1 is deferred by run_protocol_init's own last_bus_activity stamp
    // (ADR-083), not force-fired by the pre-init top-up.
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "cll_b's 5-baud init must not force-fire a mode-1 (idle-triggered) sibling's \
         tester-present -- only mode-0 siblings are ever force-fired"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

// ── ADR-137: CP_TesterPresentHandling master switch ───────────────────────

/// ADR-137 (1): `CP_TesterPresentHandling = 0` (the spec default for a plain
/// ISO15765 CLL -- `ISO_15765_4 = 0` in the ComParam table) gates
/// tester-present off at `CoptStartcomm` even when a message and a non-zero
/// interval are configured -- the master enable switch is independent of
/// `CP_TesterPresentTime`'s own cadence-disable sentinel.
#[tokio::test]
#[serial]
async fn handling_0_suppresses_arm_at_startcomm() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 100_000).await;
    // Explicit, though a plain ISO15765 CLL already defaults to 0 (the
    // ISO_15765_4 spec default) -- spelled out so the test documents the
    // gate rather than relying on an implicit preset value.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server.backdoor.start_periodic_count(),
        0,
        "CP_TesterPresentHandling = 0 must never start a hardware periodic message"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "CoptStartcomm's arm-time send must not fire when CP_TesterPresentHandling = 0"
    );

    // Past the configured 100ms interval, confirm no due-triggered send
    // appeared either -- the CLL never armed at all.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "no tester-present frame should ever appear while CP_TesterPresentHandling = 0"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-137 (2): a live `CoptUpdateparam` that promotes
/// `CP_TesterPresentHandling` from `1` to `0` on an already-`Armed` CLL
/// disarms it -- the spec's only in-band mechanism to stop a running
/// tester-present keep-alive without a full `CoptStopcomm`/`CoptStartcomm`
/// cycle (a scoped exception to ADR-084's "keep the stale keep-alive
/// running" fallthrough, which still governs every other implicit-disable
/// case, e.g. `CP_TesterPresentTime` promoted to `0`). No frame is sent by
/// the disarm itself, and no `PduErrEvtTesterPresentError` fires -- this is
/// a successful reconfiguration, not a failure.
#[tokio::test]
#[serial]
async fn handling_1_to_0_live_updateparam_disarms_running_tester_present() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Resource 0x0204 (ISO_14230_3_on_ISO_15765_2) defaults
    // CP_TesterPresentHandling to 1, matching the spec table.
    let cll_handle = create_cll(&mut client, 0x0204, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // The live disarm: promoting CP_TesterPresentHandling to 0 while Armed.
    // `set_com_param_unum32`/`promote_via_update_param` themselves panic on
    // any RPC error, so a clean return here already confirms no synchronous
    // error from the CoptUpdateparam call.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert!(
        !wait_for_event(&mut events, 200, is_tester_present_error).await,
        "a live CP_TesterPresentHandling 1->0 disarm is a successful reconfiguration, not a \
         PduErrEvtTesterPresentError"
    );

    // Past the original 300ms schedule, confirm no further frame appeared --
    // the disarm actually stopped the periodic send, not merely delayed it.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "no further tester-present frame should appear after CP_TesterPresentHandling is live-\
         disarmed to 0 -- only the original CoptStartcomm arm-time send should be present"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-137 (3): `CP_TesterPresentHandling` promoted live from `0` to `1` via
/// `CoptUpdateparam`, on a comm-started CLL that never armed at
/// `CoptStartcomm` (handling was `0` then), needs no special-casing: since a
/// CLL can only be `Armed` with `handling_enabled == true`, this promotion
/// always finds `old_resolved == None` and falls through to the existing
/// "no prior armed state" re-arm path, sending immediately -- the same
/// "just became enabled -> send now" contract `CoptStartcomm`'s own arm
/// gives (ADR-084).
#[tokio::test]
#[serial]
async fn handling_0_to_1_live_updateparam_arms_and_sends_immediately() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;
    // CP_TesterPresentHandling defaults to 0 for a plain ISO15765 CLL (the
    // ISO_15765_4 spec default) -- left unset here, so CoptStartcomm below
    // never arms.
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "CoptStartcomm should not have armed tester-present while CP_TesterPresentHandling = 0"
    );

    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a live CP_TesterPresentHandling 0->1 promotion should immediately send a tester-present \
         frame and arm"
    );
    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.push(0x3E);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    // Confirm the CLL is genuinely armed (not just a one-shot send): the
    // second, due-triggered frame appears on the newly-armed 300ms schedule.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    drop(events);
    server.shutdown().await;
}

/// ADR-137 (4, regression guard): a live `CoptUpdateparam` that promotes an
/// entirely unrelated ComParam (`CP_Loopback`) on an already-`Armed` CLL
/// must not disarm, resend, or re-arm tester-present -- `handling_enabled`
/// must not spuriously participate in the "did anything relevant change"
/// comparison beyond the new leading `!resolved.handling_enabled` match arm,
/// which only fires when the freshly-resolved value is actually `false`.
/// Mirrors `send_type_1_unrelated_updateparam_does_not_resend_or_rearm`'s
/// structure.
#[tokio::test]
#[serial]
async fn handling_unrelated_updateparam_does_not_disarm_or_rearm() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Resource 0x0204 (ISO_14230_3_on_ISO_15765_2) defaults
    // CP_TesterPresentHandling to 1, matching the spec table.
    let cll_handle = create_cll(&mut client, 0x0204, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let started = std::time::Instant::now();
    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // An unrelated ComParam promotion: LOOPBACK has no bearing on
    // tester-present's resolved data/interval/tx_flags/framing/send_type/
    // handling_enabled.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::LOOPBACK, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    // Checked immediately after the promote (matching
    // `send_type_1_unrelated_updateparam_does_not_resend_or_rearm`'s own
    // timing convention): the mode-1 (idle-triggered) second frame is due
    // ~300ms after the first, so this check -- and the (short,
    // budget-conscious) no-error check right after it -- must land well
    // before that deadline, not after.
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "an unrelated CoptUpdateparam must not re-send a tester-present frame"
    );
    assert!(
        !wait_for_event(&mut events, 50, is_tester_present_error).await,
        "an unrelated CoptUpdateparam must not disarm tester-present or emit \
         PduErrEvtTesterPresentError"
    );

    // The second (due-triggered) frame must still arrive on the ORIGINAL
    // 300ms schedule, confirming the CLL stayed Armed rather than being
    // spuriously disarmed or re-armed by the unrelated promotion.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = started.elapsed();
    assert!(
        (250..800).contains(&elapsed.as_millis()),
        "the second frame should fire ~300ms after the original arm, confirming the CLL was not \
         disarmed or re-armed by the unrelated CoptUpdateparam (waited {elapsed:?})"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-137 (edge-case-hunter gap 1): the new disarm arm's write-back
/// (`events.rs:8259-8286`) checks `channel_id`/`connect_generation`/
/// `comm_started`/token identity, exactly the same re-validation discipline
/// as the sibling re-arm write-back -- but it does NOT branch on which
/// `TesterPresentState` variant it found (`Armed` or `Cleared`), it just
/// requires the token to still match. So a live `CP_TesterPresentHandling`
/// `1 -> 0` promotion while a CLL is `Cleared` (not `Armed`) still passes the
/// guard and still writes `TesterPresentState::None` -- externally a no-op
/// (no send, no error: `Cleared` was already not transmitting), even though
/// internally it is a real transition (`Cleared` -> `None`; the code's own
/// comment at `events.rs:8268-8269` only calls out `None` -> `None` as "a
/// no-op if the CLL was already `TesterPresentState::None`", not `Cleared`).
/// The second half below then promotes `CP_TesterPresentHandling` back to
/// `1` on that same link and confirms it re-arms with an immediate send --
/// proving the disarm-while-`Cleared` no-op does not somehow "stick" and
/// suppress the following legitimate re-enable (ADR-093's "reconfiguring
/// after a clear is itself a re-enable" semantics, the same finding
/// `send_type_1_relevant_updateparam_after_clear_resends_and_rearms` proves
/// for a non-handling promotion).
///
/// Note on which `old_resolved` branch this actually exercises (found while
/// implementing this test, matching this brief's own "match real behavior
/// rather than the ADR's prose description" instruction): because the first
/// (`1 -> 0`) promotion's guard passes and unconditionally writes `None`
/// (per `events.rs:8284`, unconditional on the current variant), the second
/// (`0 -> 1`) promotion's `old_resolved` is read back from the NOW-`None`
/// state (`events.rs:8127`'s `TesterPresentState::None => None` arm), not
/// from the `Cleared { resolved, .. }` carry-forward arm the ADR references
/// (`events.rs:8124-8126`) -- that carry-forward arm can only feed a
/// re-arm's `old_resolved` when the CLL is *still* `Cleared` at promotion
/// time, which requires the immediately-preceding promotion's own resolution
/// to NOT have disarmed it (i.e. `CP_TesterPresentHandling` staying `1`
/// throughout, exactly `send_type_1_relevant_updateparam_after_clear_resends_and_rearms`'s
/// existing scenario). A handling-driven `Cleared` -> re-arm transition that
/// still finds `old_resolved` sourced from `Cleared` at write time would
/// require the disarm write to be raced/skipped -- see the next test's doc
/// comment for why that is not constructible with this harness. This test
/// still exercises real, previously-uncovered behavior (the `Cleared`-state
/// disarm no-op, and that a disarm-then-re-enable sequence through a prior
/// clear is not spuriously suppressed or double-counted) -- just via the
/// `None` old-resolved arm for its second half, not the literal `Cleared`
/// arm.
#[tokio::test]
#[serial]
async fn handling_1_to_0_then_0_to_1_on_a_cleared_link_no_ops_then_rearms() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // Long enough that no due-triggered re-send or ordinary scheduling
    // jitter can land inside the no-op assertions below.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    clear_periodic_msgs(&mut client, cll_handle).await;
    assert!(
        wait_for_event(&mut events, 2000, is_tester_present_error).await,
        "PDU_IOCTL_CLEAR_PERIODIC_MSGS should emit PduErrEvtTesterPresentError for the CLL it \
         actually cleared"
    );

    // Part A: a live CP_TesterPresentHandling 1 -> 0 promotion while Cleared
    // (not Armed) -- must be a no-op from the outside: no send, no error.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert!(
        !wait_for_event(&mut events, 200, is_tester_present_error).await,
        "a CP_TesterPresentHandling 1->0 promotion on an already-Cleared CLL must not emit \
         PduErrEvtTesterPresentError -- there is nothing to disarm, but the write-back is still \
         a successful reconfiguration, not a failure"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a CP_TesterPresentHandling 1->0 promotion on an already-Cleared CLL must not send a \
         frame -- only the original CoptStartcomm arm-time send should be present"
    );

    // Past the original 300ms schedule: confirms the Cleared CLL stayed
    // silent, not merely delayed.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a Cleared-then-disarmed tester-present must stay silent even after its old interval has \
         elapsed"
    );

    // Part B: CP_TesterPresentHandling promoted back 0 -> 1 on the same
    // link -- must re-arm with an immediate send, not stay stuck disabled.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a CP_TesterPresentHandling 0->1 promotion on a previously-Cleared-then-disarmed CLL \
         should immediately re-arm and send a tester-present frame"
    );
    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.push(0x3E);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected,
        "the re-armed send should carry the tester-present message"
    );

    // Confirm the CLL is genuinely re-armed (not a one-shot send): the next
    // due-triggered frame appears on the newly-armed 300ms schedule.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;

    drop(events);
    server.shutdown().await;
}

/// ADR-137 (edge-case-hunter gap 2): mirrors
/// `send_type_1_disconnect_racing_the_p3_gap_wait_suppresses_the_send`'s
/// pattern -- a live promotion in flight, a concurrent Disconnect landing
/// between the promotion's own state capture and its write-back's
/// re-validation -- but for the NEW disarm arm (`events.rs:8259-8286`)
/// instead of the sibling re-arm arm immediately after it.
///
/// This is NOT a genuine race, and is deliberately not dressed up as one
/// (per this test's own brief: "if you find you genuinely cannot construct
/// a real race here ... say so explicitly"). The re-arm path's sibling test
/// above can force the interleaving because `handle_start_comm`'s /
/// `handle_update_param`'s re-arm write-back sits behind a real,
/// substantial `wait_for_p3_gap` `.await` (up to the full `CP_P3Func`/
/// `CP_P3Phys` value) between its token capture and the write, giving a
/// `tokio::time::sleep`-timed concurrent Disconnect a real window to land in
/// (the poll task is genuinely suspended in a timer wait, which DOES yield
/// to the executor). The disarm arm has no equivalent: its write-back sits
/// directly after `resolve_tester_present` (confirmed synchronous -- no
/// internal `.await`s), with only `promote_unique_resp_id_table`'s hardware
/// I/O `.await` between the token capture (part of `rearm_snapshot`, taken
/// before that call) and the write. For a CLL with no
/// `unique_resp_id_table` configured (this test's setup, matching every
/// other test in this file), that call takes its early-return path
/// (`rpc_link.rs:2397-2406`'s `unique_resp_id_tables_equal` short-circuit
/// when old and new tables are both empty): one uncontended
/// `logical_links` lock take/release, no real hardware call and no timer.
///
/// This codebase has an existing, explicit finding for exactly this class
/// of window: `locks_and_param_classes.rs`'s ADR-110 "Regression-test
/// feasibility note" documents that a throwaway diagnostic test racing a
/// second client-issued RPC against a `CoptUpdateparam` with no intervening
/// real delay produced the IDENTICAL (non-interleaved) outcome five times in
/// a row, because `StartComPrimitive` merely enqueues the work item and
/// returns -- the poll task then runs the whole handler (every uncontended
/// lock take and near-instant mocked I/O call inside it) to completion
/// before the driving test task's own next `.await` ever reaches the wire,
/// deterministically, on this crate's `current_thread` `#[tokio::test]`
/// harness. That note also records that this harness's only "make a
/// hardware call take real wall-clock time" mechanism
/// (`arm_write_rx_injection`'s hold hook) blocks the entire OS thread via
/// `std::thread::sleep` inside the mock's C ABI and so cannot be used to
/// create a genuine two-task interleaving window either, and that adding a
/// new test-only production-code pause/gate hook to force one is explicitly
/// out of scope. The same reasoning applies unchanged here: no hold hook
/// exists for `promote_unique_resp_id_table`'s early-return path (or its
/// real `FLOW_CONTROL_FILTER`-install path, which is equally a single
/// uncontended `api` lock take followed by synchronous, non-awaited mock
/// calls -- see `rpc_link.rs:2239-2254`'s `install_point_to_point_fc_filters`
/// -- so a non-empty `unique_resp_id_table` would not change this finding
/// either), so a genuine race is not constructible for this arm without
/// that same out-of-scope hook.
///
/// Per this test's own fallback instruction, this instead directly exercises
/// the disarm write-back's re-validation guard's PURPOSE (not a forced,
/// misleading "race" that can't actually race): the disarming
/// `CoptUpdateparam` and a same-CLL `DisconnectComLogicalLink` (picked, like
/// the sibling re-arm race tests, over `CoptStopcomm`/a second
/// `CoptUpdateparam`, for direct comparability) are dispatched back-to-back
/// with no artificial delay beyond promotion settling, then the CLL is
/// reconnected and freshly re-armed with new tester-present content. The
/// assertion that matters is the one the guard exists to guarantee: the
/// reconnected session's `Armed` state must stay continuously armed across
/// several due-triggered ticks. If the guard were ever bypassed by a future
/// change that introduces a real yield point into this specific write-back
/// (making the currently-impossible interleaving above possible), a stale
/// disarm write landing after the reconnect would silently wipe the fresh
/// session's `Armed` state back to `None` and this schedule would go silent
/// partway through -- this is the regression guard for that failure mode.
#[tokio::test]
#[serial]
async fn handling_1_to_0_disconnect_immediately_after_the_disarm_updateparam_does_not_corrupt_a_fresh_reconnect()
 {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 300_000).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // Kept alive (not disconnected) so the shared physical channel survives
    // cll_a's disconnect below, letting cll_a's later reconnect (at the same
    // DATA_RATE) reuse the SAME channel_id (MOCK_CHANNEL_ID) instead of
    // opening a second physical channel -- keeps this test's written-frame
    // bookkeeping on one channel throughout.
    let _cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // The disarm-triggering promotion, immediately followed (no artificial
    // sleep) by a Disconnect of the same CLL -- see the doc comment above
    // for why this cannot actually interleave with this arm's write-back on
    // this harness.
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll_a).await;

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    assert!(
        !wait_for_event(&mut events, 200, is_tester_present_error).await,
        "neither the disarm promotion nor the immediately-following disconnect should emit \
         PduErrEvtTesterPresentError"
    );

    // Reconnect cll_a with a fresh, freshly-armed tester-present session --
    // new message content and a short interval, so a stale write landing
    // here would be unambiguous.
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x77]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, 100_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("reconnect_com_logical_link should succeed for cll_a");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a's reconnect at the same DATA_RATE, with cll_b still holding the channel open, \
         should reuse the same physical channel, not open a second one"
    );
    // cll_a is now rejoining an ALREADY-OPEN shared channel (cll_b never
    // disconnected) rather than creating a brand-new one, so its Working set
    // does not auto-promote to Active at Connect (same reason
    // `send_type_1_sibling_cll_arming_does_not_reset_another_clls_idle_window`'s
    // joining cll_b needs an explicit promote too) -- an explicit
    // CoptUpdateparam is required before the new tester-present content
    // takes effect.
    promote_via_update_param(&mut client, cll_a).await;

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events).await;

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let mut expected_new = 0x7DF_u32.to_be_bytes().to_vec();
    expected_new.push(0x77);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_new,
        "cll_a's fresh reconnect arm should send the new message"
    );

    // The reconnected session must stay continuously armed across several
    // due-triggered ticks -- a stale disarm write landing after the
    // reconnect would silently wipe it back to None and this schedule would
    // go silent partway through instead.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 4).await;
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 3),
        expected_new,
        "the fourth frame should still carry the reconnected session's message"
    );

    drop(events);
    server.shutdown().await;
}

// ── ADR-137 Codex-review fix: handling checked before fallible resolution ──

/// Codex review round 1 finding: `CP_TesterPresentHandling = 0` combined with
/// an out-of-range `CP_TesterPresentSendType` (which `resolve_tester_present`
/// would otherwise reject) must still let `CoptStartcomm` succeed -- handling
/// is the master switch, so an explicit disable must not require other,
/// now-irrelevant tester-present configuration to be valid. Before the fix,
/// `handling_enabled` was computed AFTER `CP_TesterPresentSendType`'s range
/// check, so this combination made `resolve_tester_present` return `Err`
/// before ever reading `CP_TesterPresentHandling`, rejecting `CoptStartcomm`
/// with `INVALID_ARGUMENT` even though tester-present was supposed to be off.
#[tokio::test]
#[serial]
async fn handling_0_with_invalid_send_type_still_succeeds_at_startcomm() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 100_000).await;
    // Out of ISO 22900-2's defined {0, 1} range -- would otherwise fail
    // `resolve_tester_present`'s own CP_TesterPresentSendType validation.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 2).await;
    // A plain ISO15765 CLL already defaults CP_TesterPresentHandling to 0
    // (ISO_15765_4's spec default); spelled out explicitly so the test
    // documents the master-switch short-circuit rather than relying on an
    // implicit preset value.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // `start_comm` itself panics on any RPC error -- a clean return already
    // confirms CoptStartcomm was NOT rejected despite the invalid
    // CP_TesterPresentSendType sitting in Active.
    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "CP_TesterPresentHandling = 0 must suppress the arm-time send regardless of \
         CP_TesterPresentSendType's validity"
    );

    drop(events);
    server.shutdown().await;
}

/// Codex review round 1 finding, live-`CoptUpdateparam` counterpart: an
/// `Armed` CLL where a single promotion sets `CP_TesterPresentHandling` to
/// `0` AND `CP_TesterPresentSendType` to an out-of-range value together must
/// still disarm cleanly -- not fail resolution and leave the prior `Armed`
/// state (and its stale keep-alive) running. Before the fix, this exact
/// combination made `resolve_tester_present` return `Err` (the invalid
/// send-type check ran before `handling_enabled` was ever read), so
/// `handle_update_param`'s `Err` arm fired instead of the new disarm arm:
/// a spurious `PduErrEvtTesterPresentError` and the previous keep-alive left
/// transmitting despite the explicit disable.
#[tokio::test]
#[serial]
async fn handling_1_to_0_disarms_despite_simultaneously_invalid_send_type() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Resource 0x0204 (ISO_14230_3_on_ISO_15765_2) defaults
    // CP_TesterPresentHandling to 1, matching the spec table.
    let cll_handle = create_cll(&mut client, 0x0204, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // The live disarm, bundled with an out-of-range CP_TesterPresentSendType
    // in the SAME promotion -- `set_com_param_unum32`/`promote_via_update_param`
    // themselves panic on any RPC error, so a clean return here already
    // confirms no synchronous error from the CoptUpdateparam call itself.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert!(
        !wait_for_event(&mut events, 200, is_tester_present_error).await,
        "a live CP_TesterPresentHandling 1->0 disarm must succeed even when \
         CP_TesterPresentSendType is simultaneously out of range -- the master switch must not \
         depend on other, now-irrelevant tester-present configuration being valid"
    );

    // Past the original 300ms schedule, confirm no further frame appeared --
    // the disarm actually stopped the periodic send, not merely delayed it
    // (and definitely didn't leave it running due to a resolution error).
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "no further tester-present frame should appear after the disarm -- only the original \
         CoptStartcomm arm-time send should be present"
    );

    drop(events);
    server.shutdown().await;
}

// ── ADR-137 second Codex-review fix: residual discard window survives ──────
// ── disarm/clear ─────────────────────────────────────────────────────────

/// Creates an ISO15765 CLL, connects it, and configures functional
/// (broadcast) addressing (`CP_RequestAddrMode = 2`, `CP_CanFuncReqId =
/// 0x7DF`) plus a non-empty `CP_TesterPresentMessage`, `CP_TesterPresentReqRsp
/// = 1` with `CP_TesterPresentExpPosResp = [0x7E]`, and
/// `CP_TesterPresentHandling = 1` -- shared setup for the residual-discard-
/// window tests below (ADR-137 second Codex-review fix). Deliberately leaves
/// the `UniqueRespIdTable` EMPTY (ADR-007 no-table wildcard mode routes every
/// CAN ID unconditionally), so `target_can_ids` resolves `None`
/// (unrestricted) and any injected CAN ID reaches the discard check --
/// mirrors `tester_present_reqrsp.rs`'s `create_functional_no_table_cll`,
/// duplicated locally per this file's own convention of not coupling to
/// another test module's helper. Callers stage `CP_TesterPresentTime`/
/// `CP_P2Max` and promote once before arming.
async fn create_functional_reqrsp_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
) -> vci_service_interface::ComLogicalLinkHandle {
    let cll_handle = create_and_connect_cll(
        client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_com_param_unum32(client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(client, cll_handle, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    set_com_param_unum32(client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    cll_handle
}

/// ADR-137 second Codex-review fix, core new behavior (disarm half): the new
/// disarm arm in `handle_update_param` (`events.rs`) used to unconditionally
/// replace `Armed` with `TesterPresentState::None`, silently dropping a
/// still-open `discard_until` window from the last arm-time send. A live
/// `CP_TesterPresentHandling` 1->0 disarm now freezes that window into
/// `TesterPresentState::Disarmed`'s own `residual` field instead, so a
/// delayed ECU response arriving while it is still open must still be
/// discarded, not delivered as an unsolicited `ResultData`, even though the
/// CLL's software state is no longer `Armed`.
#[tokio::test]
#[serial]
async fn handling_1_to_0_disarm_with_open_window_still_discards_delayed_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_reqrsp_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    // Generous CP_P2Max (5s): the disarm-then-inject sequence below all
    // happens well inside it, no timing pressure on this still-open half of
    // the pair (mirrors `create_routed_cll`'s own generous default in
    // `tester_present_reqrsp.rs`).
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Live disarm while the CP_P2Max window opened by the arm-time send
    // above is still open.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a delayed ECU response arriving inside the still-open CP_P2Max window must still be \
         discarded after a live CP_TesterPresentHandling 1->0 disarm -- TesterPresentState::\
         Disarmed's own `residual` field must not drop it (ADR-137 second Codex-review fix)"
    );

    drop(events);
    server.shutdown().await;
}

/// Third Codex-review round finding on this same mechanism: the disarm arm's
/// residual-carry match (`handle_update_param`, `events.rs`) handled
/// replacing `Armed`/`Cleared` correctly but mapped `TesterPresentState::
/// Disarmed { .. } => None` for the state-being-replaced case too --
/// dropping an already-carried residual whenever a SECOND `CoptUpdateparam`
/// lands (still resolving `CP_TesterPresentHandling = 0`) while the first
/// disarm's residual window is still open. Reuses `LOOPBACK`, this file's
/// established "entirely unrelated ComParam" (see
/// `send_type_1_unrelated_updateparam_does_not_resend_or_rearm`), promoted a
/// second time after the initial disarm to trigger the disarm arm again
/// without touching anything tester-present-relevant.
#[tokio::test]
#[serial]
async fn handling_1_to_0_repeated_disable_promotion_preserves_open_residual() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_reqrsp_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    // Generous CP_P2Max (5s): both promotions below, and the delayed-response
    // injection, all happen well inside it.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // First disarm: CP_TesterPresentHandling 1->0, opening TesterPresentState::
    // Disarmed's `residual` from the arm-time send's still-open window.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;

    // Second promotion while handling is still 0: re-enters the disarm arm's
    // `Ok(resolved) if !resolved.handling_enabled` match again (it does not
    // check whether anything actually changed), which used to rebuild
    // `Disarmed` from scratch with `residual: None`.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::LOOPBACK, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a delayed ECU response arriving inside the still-open CP_P2Max window must still be \
         discarded after a SECOND CoptUpdateparam re-enters the disarm arm while handling stays \
         0 -- the residual carried from the first disarm must survive being replaced by another \
         Disarmed{{residual: None}} (third Codex-review round finding)"
    );

    drop(events);
    server.shutdown().await;
}

/// Companion to `handling_1_to_0_disarm_with_open_window_still_discards_delayed_response`:
/// once the residual window carried into `TesterPresentState::Disarmed` has
/// actually expired, a matching frame is delivered as ordinary traffic, not
/// discarded -- the residual is correctly treated as inert once expired,
/// mirroring `reqrsp_1_mode_1_window_expired_does_not_discard`'s structure
/// for the plain `Armed` case.
#[tokio::test]
#[serial]
async fn handling_1_to_0_disarm_after_window_expires_delivers_delayed_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_reqrsp_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    // Short, deterministic 50ms CP_P2Max for this expired half of the pair.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 50_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the frame below
    // delivered under the unbound-discard model.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;

    // Comfortably past the 50ms CP_P2Max window.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let payload = vec![0x7E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );

    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "a frame arriving after the residual CP_P2Max window has expired should be delivered, \
         not discarded, even though a live CP_TesterPresentHandling 1->0 disarm carried the \
         window forward as TesterPresentState::Disarmed's own `residual`"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}

/// `PDU_IOCTL_CLEAR_PERIODIC_MSGS` companion to
/// `handling_1_to_0_disarm_with_open_window_still_discards_delayed_response`:
/// `TesterPresentState::Cleared`'s own doc comment previously claimed there
/// was nothing to carry forward from the `Armed` state it replaces besides
/// `resolved` -- that was the identical pre-existing bug, not a reviewed
/// acceptance. A delayed ECU response arriving while the cleared window is
/// still open must still be discarded.
#[tokio::test]
#[serial]
async fn clear_periodic_msgs_with_open_window_still_discards_delayed_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_reqrsp_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    clear_periodic_msgs(&mut client, cll_handle).await;
    assert!(
        wait_for_event(&mut events, 2000, is_tester_present_error).await,
        "PDU_IOCTL_CLEAR_PERIODIC_MSGS should emit PduErrEvtTesterPresentError for the CLL it \
         actually cleared"
    );

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a delayed ECU response arriving inside the still-open CP_P2Max window must still be \
         discarded after PDU_IOCTL_CLEAR_PERIODIC_MSGS -- TesterPresentState::Cleared's own \
         `residual` field must not drop it (ADR-137 second Codex-review fix)"
    );

    drop(events);
    server.shutdown().await;
}

/// Companion to `clear_periodic_msgs_with_open_window_still_discards_delayed_response`:
/// once the residual window carried into `TesterPresentState::Cleared` has
/// actually expired, a matching frame is delivered as ordinary traffic.
#[tokio::test]
#[serial]
async fn clear_periodic_msgs_after_window_expires_delivers_delayed_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_reqrsp_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    // Short, deterministic 50ms CP_P2Max for this expired half of the pair.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 50_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the frame below
    // delivered under the unbound-discard model.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    clear_periodic_msgs(&mut client, cll_handle).await;
    assert!(
        wait_for_event(&mut events, 2000, is_tester_present_error).await,
        "PDU_IOCTL_CLEAR_PERIODIC_MSGS should emit PduErrEvtTesterPresentError for the CLL it \
         actually cleared"
    );

    // Comfortably past the 50ms CP_P2Max window.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let payload = vec![0x7E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );

    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "a frame arriving after the residual CP_P2Max window has expired should be delivered, \
         not discarded, even though PDU_IOCTL_CLEAR_PERIODIC_MSGS carried the window forward as \
         TesterPresentState::Cleared's own `residual`"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}

/// Regression guard (design-advisor's own flagged hazard): `Disarmed`
/// deliberately carries no `resolved` baseline (unlike `Cleared`, which
/// carries one for its own re-arm-gate purpose) -- otherwise a
/// `CP_TesterPresentHandling` 0->1 promotion following a disarm would compare
/// equal against a stale `resolved` snapshot (since `handling_enabled` is
/// excluded from `same_wire_behavior`, ADR-137) and never re-arm, breaking
/// the spec's "once enabled, sent immediately" contract (ADR-084). Starts
/// from a genuinely `TesterPresentState::Disarmed` CLL (via a live 1->0
/// disarm, not merely never-armed) and confirms a following 0->1 promotion
/// still sends immediately and re-arms -- extends
/// `handling_0_to_1_live_updateparam_arms_and_sends_immediately`'s coverage,
/// which only exercises the never-armed (`TesterPresentState::None`) case.
#[tokio::test]
#[serial]
async fn handling_0_to_1_after_disarm_rearms_and_sends_immediately() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Disarm live -- the CLL is now genuinely `TesterPresentState::Disarmed`
    // (not merely never-armed), the scenario
    // `handling_0_to_1_live_updateparam_arms_and_sends_immediately` above
    // does not exercise.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the disarm itself must not resend a tester-present frame"
    );

    // Re-enable: `Disarmed` deliberately carries no `resolved` baseline (the
    // hazard design-advisor flagged), so this promotion's `old_resolved` is
    // `None` and always compares "changed" -- the same immediate-send
    // contract as a genuinely never-armed CLL, not spuriously suppressed by
    // the wire content being identical to what was armed before the disarm.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a live CP_TesterPresentHandling 0->1 promotion on a Disarmed CLL should immediately \
         send a fresh tester-present frame and re-arm"
    );
    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.push(0x3E);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 1), expected);

    // Confirm genuine re-arm (not a one-shot): the due-triggered third frame
    // appears on the freshly-armed 300ms schedule.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;

    drop(events);
    server.shutdown().await;
}

// ── ADR-137 fourth Codex-review fix (round-4 restructure): open discard ────
// ── windows are a per-CLL list (`LogicalLinkState::open_tp_discards`), not ─
// ── a single slot embedded in `TesterPresentState` ──────────────────────────
//
// Rounds 2-3 (immediately above) forced per-send data (`DiscardWindow`/
// `ResidualTesterPresentDiscard`) to survive per-configuration
// `TesterPresentState` transitions by hand-copying it through each
// transition arm. This round moves open windows out of the enum entirely
// into `LogicalLinkState.open_tp_discards`, fixing the rounds 1-3 storage
// mismatch AND two latent bugs of the exact same class found in the
// pre-existing (pre-ADR-137, ADR-088-era) per-tick sender: (i) a promotion
// excluded from `same_wire_behavior` used to let the next send's window
// silently replace a still-open prior window with a different signature;
// (ii) a failed send used to unconditionally drop a still-open prior window.
// See `docs/adr/ADR-137-cp-tester-present-handling-master-switch.md`'s
// "Fourth Codex-review fix (restructure)" section.

/// Round-4 repro (`Disarmed` -> re-arm variant): disarm while a window from
/// the arm-time send is still open, then a `CoptUpdateparam` that BOTH
/// promotes handling back to `1` (0->1 re-enable) AND changes
/// `CP_TesterPresentExpPosResp` (excluded from `same_wire_behavior`, so it
/// plays no role in the re-arm decision itself -- `Disarmed` carries no
/// `resolved` baseline at all, ADR-137, so this always re-arms regardless of
/// what changed). A delayed response matching the PRE-disable send's
/// ORIGINAL signature, arriving after the re-enable's own fresh send (which
/// opens a window with a DIFFERENT signature), must still be discarded --
/// the rounds 1-3 single-slot design would have had the fresh re-arm's
/// write-back silently replace the pre-disable window.
#[tokio::test]
#[serial]
async fn handling_1_to_0_then_0_to_1_with_changed_signature_still_discards_pre_disable_window() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_reqrsp_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    // Generous CP_P2Max (5s): both windows in this test stay open for its
    // whole duration, no timing pressure.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await; // send #1: pos = [0x7E]

    // Disarm while send #1's window is still open.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the disarm itself must not resend"
    );

    // Re-enable AND change CP_TesterPresentExpPosResp in the SAME
    // CoptUpdateparam.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7F],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a live 0->1 re-enable should send immediately"
    );

    // A delayed response matching send #1's ORIGINAL signature (pos =
    // [0x7E]) -- not send #2's new pos ([0x7F]) -- arriving after the
    // re-enable's own fresh send, must still be discarded.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a delayed response matching the pre-disable send's own signature must still be \
         discarded after a 0->1 re-enable that also changed the wire signature for future sends \
         -- open discard windows are a per-CLL list (LogicalLinkState::open_tp_discards), not a \
         single slot the fresh re-arm's write-back can silently overwrite (ADR-137 fourth \
         Codex-review fix / round-4 restructure)"
    );

    drop(events);
    server.shutdown().await;
}

/// Round-4 repro (`Cleared` -> re-arm variant): the `PDU_IOCTL_
/// CLEAR_PERIODIC_MSGS` counterpart to the `Disarmed` case above. Unlike
/// `Disarmed`, `Cleared` DOES carry a `resolved` baseline for the re-arm
/// gate's own `same_wire_behavior` comparison (ADR-093), so this re-arm is
/// forced via `CP_TesterPresentMessage` (a field that DOES participate in
/// `same_wire_behavior` -- the same technique
/// `send_type_1_relevant_updateparam_after_clear_resends_and_rearms` above
/// uses), promoted in the SAME `CoptUpdateparam` as a
/// `CP_TesterPresentExpPosResp` change (excluded from `same_wire_behavior`,
/// so it plays no role in the re-arm decision, but does give the fresh
/// send's own window a different signature).
#[tokio::test]
#[serial]
async fn clear_periodic_msgs_then_rearm_with_changed_signature_still_discards_pre_clear_window() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_reqrsp_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await; // send #1: pos = [0x7E]

    clear_periodic_msgs(&mut client, cll_handle).await;
    assert!(
        wait_for_event(&mut events, 2000, is_tester_present_error).await,
        "PDU_IOCTL_CLEAR_PERIODIC_MSGS should emit PduErrEvtTesterPresentError"
    );

    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x01],
    )
    .await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7F],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a wire-content-changing CoptUpdateparam should immediately re-arm a Cleared CLL"
    );

    // A delayed response matching send #1's ORIGINAL signature (pos =
    // [0x7E]) must still be discarded after the re-arm's own fresh send
    // (whose window now carries pos = [0x7F]).
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a delayed response matching the pre-clear send's own signature must still be discarded \
         after PDU_IOCTL_CLEAR_PERIODIC_MSGS followed by a re-arm with a different signature \
         (ADR-137 fourth Codex-review fix / round-4 restructure)"
    );

    drop(events);
    server.shutdown().await;
}

/// Round-4 repro, the direct `events.rs`-per-tick-sender case: a
/// `CoptUpdateparam` that changes only `CP_TesterPresentExpPosResp`
/// (excluded from `same_wire_behavior`, so it never re-arms -- picked up
/// live by the NEXT per-tick send instead, per `dispatch_due_tester_present`'s
/// own `StillDueSnapshot` re-read discipline) leaves the CLL continuously
/// `Armed`, with NO `TesterPresentState` transition at all. The pre-existing
/// (pre-ADR-137, ADR-088-era) single-slot `*discard_until = ...` overwrite at
/// the per-tick sender's own write-back would have silently replaced the
/// FIRST send's still-open window with the SECOND send's, losing the ability
/// to discard a delayed reply to the first send.
#[tokio::test]
#[serial]
async fn continuously_armed_unrelated_field_promotion_still_discards_prior_send_window() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_reqrsp_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;
    // Generous CP_P2Max (5s): both sends' windows stay open for the whole
    // test.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await; // send #1: pos = [0x7E]

    // Excluded from `same_wire_behavior` -- does not re-arm, no resend.
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7F],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a CP_TesterPresentExpPosResp-only promotion must not resend or re-arm"
    );

    // The next periodic tick's own send re-reads CP_TesterPresentExpPosResp
    // live from Active (`dispatch_due_tester_present`'s `StillDueSnapshot`),
    // so send #2 opens a window with pos = [0x7F] while send #1's still-open
    // pos = [0x7E] window (5s CP_P2Max) remains untouched.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a delayed reply matching the FIRST send's pre-promotion signature must still be \
         discarded after the SECOND per-tick send opened a window with a different signature \
         while the CLL stayed continuously Armed (no state transition at all) -- \
         dispatch_due_tester_present's per-tick write-back must push into a list, not overwrite \
         a single slot (ADR-137 fourth Codex-review fix / round-4 restructure)"
    );

    drop(events);
    server.shutdown().await;
}

/// Companion to
/// `continuously_armed_unrelated_field_promotion_still_discards_prior_send_window`:
/// once BOTH windows opened there have actually expired, a matching frame is
/// delivered as ordinary traffic, not discarded -- `open_tp_discards`'
/// expired-entry pruning (both the read-side filter in
/// `build_cll_rx_entries` and the prune-then-push discipline at every push
/// site) does not leave stale entries permanently blocking delivery.
#[tokio::test]
#[serial]
async fn continuously_armed_unrelated_field_promotion_windows_expire_then_delivers() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_reqrsp_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 300_000).await;
    // Short, deterministic 100ms CP_P2Max: both sends' windows expire well
    // before this test injects its delayed frame.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 100_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the frame below
    // delivered under the unbound-discard model.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7F],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // Comfortably past the 100ms CP_P2Max window for both sends.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let payload = vec![0x7E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );

    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "a frame arriving after every open discard window has expired should be delivered, not \
         discarded"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}
