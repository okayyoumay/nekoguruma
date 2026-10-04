// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]

use std::path::Path;

use iso22900_service::service::Iso22900Service;
use iso22900_sys::bindings::{E_PDU_ERR_EVT, E_PDU_ERROR, T_PDU_ERR_EVT, T_PDU_ERROR, UNUM32};
use iso22900_sys::libloading::{Library, Symbol};
use serial_test::serial;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Server};
use vci_service_interface::{
    CancelComPrimitiveRequest, ComLogicalLinkHandle, ComPrimitiveHandle,
    ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, DestroyComLogicalLinkRequest,
    DisconnectComLogicalLinkRequest, EventItem, EventNotification, GetComParamRequest,
    GetConflictingResourcesRequest, GetEventItemRequest, GetModuleIdsRequest, GetObjectIdRequest,
    GetResourceIdsRequest, GetResourceStatusRequest, GetStatusRequest, GetTimestampRequest,
    GetUniqueRespIdTableRequest, GetVersionRequest, IoCtlRequest, LockResourceRequest,
    ModuleConnectRequest, ModuleDisconnectRequest, ModuleHandle, ObjectType, ParamItem,
    ParamSessionTiming, ParamSessionTimingList, ParamStructfield, ParamVendorSpecificStruct,
    PduError, PduErrorEvent, PduParamClass, ResourceData, SetComParamRequest,
    SetUniqueRespIdTableRequest, StartComPrimitiveRequest, SubscribeEventRequest, SystemHandle,
    UnlockResourceRequest, create_com_logical_link_request, error_detail_from_status, event_item,
    get_com_param_request, get_status_request, param_item, param_structfield,
    vci_service_client::VciServiceClient, vci_service_server::VciServiceServer,
};
use vci_service_launcher::vci_server::VciServer;

const MOCK_MODULE_HANDLE: u32 = 1001;
const MOCK_CLL_HANDLE: u32 = 3001;
const MOCK_COP_HANDLE: u32 = 4001;
const MOCK_RESOURCE_ID: u32 = 2001;
const MOCK_TIMESTAMP: u32 = 4242;

struct TestServer {
    port: u16,
    shutdown_tx: Option<oneshot::Sender<()>>,
    _service_shutdown_tx: tokio::sync::watch::Sender<bool>,
    server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_extra_config("").await
    }

    /// Like [`Self::start`], but appends `extra_config_toml` (e.g. a
    /// `[config.apis.iso22900.libs."TestLib".vendor_struct_types]` table,
    /// ADR-218 Decision item 4a) to the generated `config.toml` before the
    /// service loads it.
    async fn start_with_extra_config(extra_config_toml: &str) -> Self {
        let lib_path = iso22900_mock::mock_library_path()
            .expect("mock cdylib should be discoverable after build");
        // Per-process-unique path (ADR-195): closes a cross-process race where
        // two separate `cargo test` processes could otherwise write/read the
        // same fixed config file path concurrently.
        let config_path = std::env::temp_dir().join(format!(
            "iso22900-service-grpc-mock-test-config-{}.toml",
            std::process::id()
        ));
        let config_toml = format!(
            "[config.apis.iso22900.libs.\"TestLib\"]\nlibrary_path = {:?}\n{extra_config_toml}",
            lib_path.display().to_string()
        );
        std::fs::write(&config_path, config_toml).expect("config file should be writable");
        unsafe {
            std::env::set_var("VCI_CONFIG_PATH", &config_path);
        }
        let (service_shutdown_tx, service_shutdown_rx) = tokio::sync::watch::channel(false);
        let service = Iso22900Service::new(
            Iso22900Service::get_startup_config(["iso22900-service", "iso22900:TestLib"])
                .expect("startup config"),
            service_shutdown_rx,
        )
        .await
        .expect("service should initialize with mock library");

        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener should bind");
        let port = listener
            .local_addr()
            .expect("listener should have a port")
            .port();
        let incoming = TcpListenerStream::new(listener);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle = tokio::spawn(async move {
            Server::builder()
                .add_service(VciServiceServer::new(service))
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        Self {
            port,
            shutdown_tx: Some(shutdown_tx),
            _service_shutdown_tx: service_shutdown_tx,
            server_handle,
        }
    }

    async fn client(&self) -> VciServiceClient<Channel> {
        VciServiceClient::connect(format!("http://127.0.0.1:{}", self.port))
            .await
            .expect("client should connect")
    }

    async fn shutdown(mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }

        self.server_handle
            .await
            .expect("server task should complete")
            .expect("server should stop cleanly");
    }
}

/// Dynamically loads the mock library and calls its `__mock_*` back-door FFI
/// exports directly, bypassing the `iso22900-mock` rlib's own (separate)
/// copy of the process-global mock state. `TestServer` loads the mock cdylib
/// via `libloading` inside `iso22900-service`'s own D-PDU API layer, and this
/// test binary also links `iso22900-mock` as an rlib dev-dependency -- those
/// are two distinct copies of `MockState`'s `OnceLock`. Loading the identical
/// library file path here resolves (via the dynamic linker) to the same
/// loaded module the service is actually talking to, so calls through this
/// struct observe/control the state the service produces (mirrors
/// `j2534-0404-service`'s `tests/grpc_mock/harness.rs::MockBackdoor`).
struct MockBackdoor {
    lib: Library,
}

impl MockBackdoor {
    fn open(path: &Path) -> Self {
        let lib = unsafe { Library::new(path) }.expect("mock library should be loadable");
        Self { lib }
    }

    /// Resets all mock state (including the error-injection overrides below)
    /// to their `__mock_reset` defaults.
    ///
    /// Loaded as `extern "system"`, not `extern "C"`: `iso22900-mock`'s
    /// `exported_fn!` macro exports every `__mock_*` backdoor (like every
    /// native D-PDU entry point) as `extern "stdcall"` on the
    /// `i686-pc-windows-gnullvm` target (Windows x86, matching the real
    /// D-PDU API's calling convention on that platform) and `extern "C"`
    /// everywhere else. `extern "system"` resolves to the same
    /// platform-correct choice on the caller side, so this stays correct on
    /// both this crate's `x86_64-unknown-linux-gnu` test-execution target
    /// and the `i686-pc-windows-gnullvm` cross-check target -- a mismatched
    /// hardcoded `extern "C"` here would corrupt the stack on 32-bit
    /// Windows (Codex review, PR #153).
    fn reset(&self) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> T_PDU_ERROR> = self
                .lib
                .get(b"__mock_reset\0")
                .expect("__mock_reset should be exported");
            f();
        }
    }

    /// Forces `PDULockResource` to fail with `code` on every subsequent
    /// call. `code == PDU_STATUS_NOERROR` clears the override. See
    /// `reset`'s doc comment for why this is `extern "system"`, not
    /// `extern "C"`.
    fn set_lock_resource_error(&self, code: T_PDU_ERROR) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(T_PDU_ERROR) -> T_PDU_ERROR> = self
                .lib
                .get(b"__mock_set_lock_resource_error\0")
                .expect("__mock_set_lock_resource_error should be exported");
            f(code);
        }
    }

    /// Configures the response `PDUGetLastError` reports on every subsequent
    /// call. See `reset`'s doc comment for why this is `extern "system"`,
    /// not `extern "C"`.
    fn set_last_error(
        &self,
        error_code: T_PDU_ERR_EVT,
        cop_handle: UNUM32,
        timestamp: UNUM32,
        extra_error_info: UNUM32,
    ) {
        unsafe {
            let f: Symbol<
                unsafe extern "system" fn(T_PDU_ERR_EVT, UNUM32, UNUM32, UNUM32) -> T_PDU_ERROR,
            > = self
                .lib
                .get(b"__mock_set_last_error\0")
                .expect("__mock_set_last_error should be exported");
            f(error_code, cop_handle, timestamp, extra_error_info);
        }
    }
}

fn default_resource_data() -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: Some(vci_service_interface::resource_data::BusType::BusTypeId(1)),
        protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
            2,
        )),
    }
}

async fn create_default_cll(client: &mut VciServiceClient<Channel>) -> ComLogicalLinkHandle {
    let cll_response = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                default_resource_data(),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner();

    let cll_handle = cll_response
        .cll_handle
        .expect("cll_handle should be present");
    assert_eq!(cll_handle.module_handle, MOCK_MODULE_HANDLE);
    assert_eq!(cll_handle.cll_handle, MOCK_CLL_HANDLE);
    cll_handle
}

fn assert_result_event_item(event_item: &EventItem, expected_cop_handle: u32) {
    assert_eq!(event_item.timestamp, MOCK_TIMESTAMP);
    assert_eq!(
        event_item
            .cop_handle
            .as_ref()
            .expect("cop_handle should be present")
            .cop_handle,
        expected_cop_handle
    );

    match &event_item.data {
        Some(event_item::Data::ResultData(result_data)) => {
            assert_eq!(result_data.unique_resp_identifier, 1);
            assert_eq!(result_data.acceptance_id, 1);
            assert!(result_data.data_bytes.is_empty());
        }
        other => panic!("expected result data event, got {other:?}"),
    }
}

fn assert_result_notification(notification: EventNotification) {
    let handle = notification
        .handle
        .expect("notification handle should be present");
    match handle {
        vci_service_interface::event_notification::Handle::CllHandle(streamed_handle) => {
            assert_eq!(streamed_handle.module_handle, MOCK_MODULE_HANDLE);
            assert_eq!(streamed_handle.cll_handle, MOCK_CLL_HANDLE);
        }
        other => panic!("expected CLL handle, got {other:?}"),
    }

    match notification
        .event_data
        .expect("notification event should be present")
    {
        vci_service_interface::event_notification::EventData::Item(event_item) => {
            assert_result_event_item(&event_item, MOCK_COP_HANDLE);
        }
        other => panic!("expected item event, got {other:?}"),
    }
}

#[tokio::test]
#[serial]
async fn grpc_server_responds_through_mock_library() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let response = client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("get_module_ids should succeed")
        .into_inner();

    let module_ids = response
        .module_id_list
        .expect("module ids should be present");
    assert_eq!(module_ids.module_data.len(), 1);
    assert_eq!(
        module_ids.module_data[0]
            .module_handle
            .as_ref()
            .map(|handle| handle.module_handle),
        Some(MOCK_MODULE_HANDLE)
    );
    assert_eq!(module_ids.module_data[0].module_type_id, 9001);

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_disconnect should succeed");

    let version = client
        .get_version(GetVersionRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("get_version should succeed")
        .into_inner();

    let version_data = version
        .version_data
        .expect("version data should be present");
    assert_eq!(version_data.hw_name, "Mock Hardware");
    assert_eq!(version_data.pdu_api_sw_name, "iso22900-mock");

    let status = client
        .get_status(GetStatusRequest {
            handle: Some(get_status_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
        })
        .await
        .expect("get_status should succeed")
        .into_inner();
    assert!(status.status.is_some());

    let timestamp = client
        .get_timestamp(GetTimestampRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("get_timestamp should succeed")
        .into_inner();
    assert_eq!(timestamp.timestamp, MOCK_TIMESTAMP);

    let cll_handle = create_default_cll(&mut client).await;

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_handle),
            lock_mask: 0x01,
        })
        .await
        .expect("lock_resource should succeed");

    client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_handle),
            lock_mask: 0x01,
        })
        .await
        .expect("unlock_resource should succeed");

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    let com_param = client
        .get_com_param(GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(get_com_param_request::Param::ParamId(7)),
        })
        .await
        .expect("get_com_param should succeed")
        .into_inner();

    let param_item = com_param.param_item.expect("param_item should be present");
    assert_eq!(
        param_item.id,
        Some(vci_service_interface::param_item::Id::ParamId(7))
    );
    assert!(matches!(
        param_item.param_data,
        Some(param_item::ParamData::Unum32(0))
    ));

    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(vci_service_interface::ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(7)),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(55)),
            }),
        })
        .await
        .expect("set_com_param should succeed");

    let com_param_after_set = client
        .get_com_param(GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(get_com_param_request::Param::ParamId(7)),
        })
        .await
        .expect("get_com_param after set should succeed")
        .into_inner();

    let param_item_after_set = com_param_after_set
        .param_item
        .expect("param_item should be present after set");
    assert_eq!(
        param_item_after_set.id,
        Some(vci_service_interface::param_item::Id::ParamId(7))
    );
    assert!(matches!(
        param_item_after_set.param_data,
        Some(param_item::ParamData::Unum32(0))
    ));

    // Test SetComParam addressed by param_name instead of a numeric param_id
    // -- the mock's PDUGetObjectId resolves "mock-com-param" to id 3201 for
    // OBJT_COMPARAM.
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(vci_service_interface::ParamItem {
                id: Some(param_item::Id::ParamName("mock-com-param".to_string())),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(123)),
            }),
        })
        .await
        .expect("set_com_param by name should succeed");

    let com_param_by_name = client
        .get_com_param(GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(get_com_param_request::Param::ParamId(3201)),
        })
        .await
        .expect("get_com_param should succeed")
        .into_inner();
    assert_eq!(
        com_param_by_name
            .param_item
            .expect("param_item should be present")
            .id,
        Some(param_item::Id::ParamId(3201))
    );

    // An unresolvable param_name must be rejected outright, not silently
    // defaulted to some other ComParam id.
    let unresolved_name_result = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(vci_service_interface::ParamItem {
                id: Some(param_item::Id::ParamName("cp_does_not_exist".to_string())),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(1)),
            }),
        })
        .await;
    assert!(unresolved_name_result.is_err());

    let unique_resp_table = client
        .get_unique_resp_id_table(GetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("get_unique_resp_id_table should succeed")
        .into_inner();

    let table = unique_resp_table
        .unique_resp_id_table
        .expect("unique response table should be present");
    assert_eq!(table.unique_data.len(), 1);
    assert_eq!(table.unique_data[0].unique_resp_identifier, 1);
    assert!(table.unique_data[0].params.is_empty());

    // Test SetUniqueRespIdTable: update the unique response ID table
    client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(table.clone()),
        })
        .await
        .expect("set_unique_resp_id_table should succeed");

    let primitive_response = client
        .start_com_primitive(StartComPrimitiveRequest {
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x12, 0x34],
            cop_ctrl_data: None,
            cop_tag: None,
        })
        .await
        .expect("start_com_primitive should succeed")
        .into_inner();

    let primitive_cll_handle = primitive_response
        .cop_handle
        .expect("primitive response should echo cll_handle");
    assert_eq!(primitive_cll_handle.module_handle, MOCK_MODULE_HANDLE);
    assert_eq!(primitive_cll_handle.cll_handle, MOCK_CLL_HANDLE);

    let event_item = client
        .get_event_item(GetEventItemRequest {
            handle: Some(
                vci_service_interface::get_event_item_request::Handle::CllHandle(cll_handle),
            ),
        })
        .await
        .expect("get_event_item should succeed")
        .into_inner();

    let event_item = event_item.event_item.expect("event_item should be present");
    assert_result_event_item(&event_item, MOCK_COP_HANDLE);
    // ADR-204: no cop_tag was supplied at StartComPrimitive above, so the
    // echo must be absent, never a zero-length sentinel.
    assert!(event_item.cop_tag.is_none());

    // Test CancelComPrimitive: cancel the primitive operation
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(ComPrimitiveHandle {
                module_handle: MOCK_MODULE_HANDLE,
                cll_handle: MOCK_CLL_HANDLE,
                cop_handle: MOCK_COP_HANDLE,
            }),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    // Test GetObjectId: query a resource object ID
    let object_id_response = client
        .get_object_id(GetObjectIdRequest {
            object_type: ObjectType::ObjtResource as i32,
            shortname: "mock-resource".to_string(),
        })
        .await
        .expect("get_object_id should succeed")
        .into_inner();
    assert_eq!(object_id_response.pdu_object_id, MOCK_RESOURCE_ID);

    // Test IoCtl: query event queue property
    let ioctl_response = client
        .io_ctl(IoCtlRequest {
            handle: Some(vci_service_interface::io_ctl_request::Handle::SystemHandle(
                SystemHandle {},
            )),
            io_ctrl_command: Some(
                vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandId(4100),
            ), // E_PDU_IT_PDU_IT_IO_EVENT_QUEUE_PROPERTY
            input_data: None,
            has_output: true,
        })
        .await
        .expect("io_ctl should succeed")
        .into_inner();

    // Validate that IoCtl returned output data
    let _output_data = ioctl_response
        .output_data
        .expect("io_ctl output_data should be present");

    // Test GetResourceIds: get available resource IDs for the module
    let resource_ids_response = client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource_data: Some(default_resource_data()),
        })
        .await
        .expect("get_resource_ids should succeed")
        .into_inner();

    let resource_id_list = resource_ids_response
        .resource_id_list
        .expect("resource_id_list should be present");
    assert_eq!(resource_id_list.resource_id_data_array.len(), 1);
    assert_eq!(
        resource_id_list.resource_id_data_array[0]
            .module_handle
            .as_ref()
            .map(|handle| handle.module_handle),
        Some(MOCK_MODULE_HANDLE)
    );
    assert_eq!(
        resource_id_list.resource_id_data_array[0]
            .resource_id_array
            .len(),
        1
    );
    assert_eq!(
        resource_id_list.resource_id_data_array[0].resource_id_array[0],
        MOCK_RESOURCE_ID
    );

    // Test GetConflictingResources: get conflicting resources for resource 2001
    let conflicting_response = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            resource: Some(
                vci_service_interface::get_conflicting_resources_request::Resource::ResourceId(
                    MOCK_RESOURCE_ID,
                ),
            ),
            input_module_list: None,
        })
        .await
        .expect("get_conflicting_resources should succeed")
        .into_inner();

    let conflict_list = conflicting_response
        .conflict_list
        .expect("conflict_list should be present");
    assert_eq!(conflict_list.resource_conflict_data.len(), 1);
    assert_eq!(
        conflict_list.resource_conflict_data[0]
            .module_handle
            .as_ref()
            .map(|handle| handle.module_handle),
        Some(MOCK_MODULE_HANDLE)
    );
    assert_eq!(
        conflict_list.resource_conflict_data[0].resource_id,
        MOCK_RESOURCE_ID
    );

    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("destroy_com_logical_link should succeed");

    server.shutdown().await;
}

// Replaces the removed GetLastError RPC coverage (see ADR-105): a failing
// RPC's `Status` now carries the `ErrorDetail` a follow-up `GetLastError`
// call would previously have provided. `GetResourceStatus` with an empty
// `resources` list deterministically fails in the mock library (its
// `PDUGetResourceStatus` rejects a zero-entry item with
// `PDU_ERR_INVALID_PARAMETERS`), giving a real native-API failure to assert
// against without needing any mock-specific failure back-door.
#[tokio::test]
#[serial]
async fn failing_rpc_carries_error_detail() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .get_resource_status(GetResourceStatusRequest { resources: vec![] })
        .await
        .expect_err("empty resources list should fail the native call");

    let detail = error_detail_from_status(&status)
        .expect("failing RPC's Status should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrInvalidParameters as i32);
    assert!(detail.error_event_data.is_none());

    server.shutdown().await;
}

// Closes the ADR-105 P2 backlog item (iso22900-service/docs/implementation-notes.md):
// exercises `with_api_for_link`/`map_runtime_error_for_link`'s best-effort
// `error_event_data` fetch end-to-end through a real CLL-scoped native
// failure (`PDULockResource`, forced via the `iso22900-mock`
// `__mock_set_lock_resource_error`/`__mock_set_last_error` back doors), not
// just the `error_event_data_for` unit test in `error.rs`.
#[tokio::test]
#[serial]
async fn link_scoped_failure_carries_error_event_data_from_last_error() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let lib_path = iso22900_mock::mock_library_path().expect("mock cdylib should be discoverable");
    let backdoor = MockBackdoor::open(&lib_path);
    backdoor.reset();

    let cll_handle = create_default_cll(&mut client).await;

    backdoor.set_last_error(E_PDU_ERR_EVT::PDU_ERR_EVT_LOST_COMM_TO_VCI, 99, 555, 7);
    backdoor.set_lock_resource_error(E_PDU_ERROR::PDU_ERR_FCT_FAILED);

    let status = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_handle),
            lock_mask: 0x01,
        })
        .await
        .expect_err("PDULockResource forced failure should fail the RPC");

    let detail = error_detail_from_status(&status)
        .expect("failing RPC's Status should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrFctFailed as i32);
    let error_event_data = detail
        .error_event_data
        .expect("a real tracked last-error should be surfaced");
    assert_eq!(
        error_event_data.error_event,
        PduErrorEvent::PduErrEvtLostCommToVci as i32
    );
    assert_eq!(error_event_data.timestamp, 555);
    assert_eq!(error_event_data.extra_error_info, 7);
    let cop_handle = error_event_data
        .cop_handle
        .expect("cop_handle should be present");
    assert_eq!(cop_handle.module_handle, MOCK_MODULE_HANDLE);
    assert_eq!(cop_handle.cll_handle, MOCK_CLL_HANDLE);
    assert_eq!(cop_handle.cop_handle, 99);

    // Restore mock state for subsequent tests in this (serialized) binary.
    backdoor.reset();

    server.shutdown().await;
}

// Companion to `link_scoped_failure_carries_error_event_data_from_last_error`:
// the same real call path, but with `PDUGetLastError` left at its default
// `PDU_ERR_EVT_NOERROR` response, closing the missing end-to-end coverage for
// `error_event_data_for`'s `PDU_ERR_EVT_NOERROR` filter (already unit-tested
// in isolation as `error_event_data_for_omits_noerror`, error.rs).
#[tokio::test]
#[serial]
async fn link_scoped_failure_omits_error_event_data_for_noerror_last_error() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let lib_path = iso22900_mock::mock_library_path().expect("mock cdylib should be discoverable");
    let backdoor = MockBackdoor::open(&lib_path);
    backdoor.reset();

    let cll_handle = create_default_cll(&mut client).await;

    // PDUGetLastError is left at its default no-error response.
    backdoor.set_lock_resource_error(E_PDU_ERROR::PDU_ERR_FCT_FAILED);

    let status = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_handle),
            lock_mask: 0x01,
        })
        .await
        .expect_err("PDULockResource forced failure should fail the RPC");

    let detail = error_detail_from_status(&status)
        .expect("failing RPC's Status should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrFctFailed as i32);
    assert!(
        detail.error_event_data.is_none(),
        "PDU_ERR_EVT_NOERROR must not be surfaced as if it were a real tracked error"
    );

    backdoor.reset();

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn grpc_subscribe_event_streams_mock_event() {
    let server = TestServer::start().await;
    let mut setup_client = server.client().await;

    let cll_handle = create_default_cll(&mut setup_client).await;

    let mut stream_client = server.client().await;
    let mut event_stream = stream_client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(
                vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_handle),
            ),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut command_client = server.client().await;
    command_client
        .start_com_primitive(StartComPrimitiveRequest {
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x12, 0x34],
            cop_ctrl_data: None,
            cop_tag: None,
        })
        .await
        .expect("start_com_primitive should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let notification = timeout(Duration::from_secs(5), event_stream.message())
        .await
        .expect("timed out waiting for streamed event")
        .expect("stream should remain open")
        .expect("streamed event should be present");

    assert_result_notification(notification);

    // Ensure the streaming RPC is closed before server shutdown.
    drop(event_stream);
    drop(stream_client);
    drop(command_client);
    drop(setup_client);

    server.shutdown().await;
}

// ADR-204 regression: `cop_tag` exists precisely because handle-based
// correlation alone leaves a client unable to attribute an event that
// arrives before its own matching `StartComPrimitiveResponse` resolves.
// This test reproduces that race deliberately -- `tokio::join!` polls the
// streamed-event read and the unary Start call concurrently, with neither
// awaited to completion first -- and confirms the streamed `EventItem`
// still carries the client's own `cop_tag` verbatim. The mock's
// `PDUStartComPrimitive` queues the event (and invokes the registered
// callback) synchronously, before the native call itself returns, so the
// notification can genuinely reach `event_stream` before the Start unary
// response finishes unwinding through tonic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn subscribe_event_echoes_cop_tag_racing_start_response() {
    let server = TestServer::start().await;
    let mut setup_client = server.client().await;

    let cll_handle = create_default_cll(&mut setup_client).await;

    let mut stream_client = server.client().await;
    let mut event_stream = stream_client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(
                vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_handle),
            ),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut command_client = server.client().await;
    let cop_tag = vec![0xDE, 0xAD, 0xBE, 0xEF];

    let (event_result, start_result) = tokio::join!(
        timeout(Duration::from_secs(5), event_stream.message()),
        command_client.start_com_primitive(StartComPrimitiveRequest {
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x12, 0x34],
            cop_ctrl_data: None,
            cop_tag: Some(cop_tag.clone()),
        }),
    );

    start_result.expect("start_com_primitive should succeed");

    let notification = event_result
        .expect("timed out waiting for streamed event")
        .expect("stream should remain open")
        .expect("streamed event should be present");

    let handle = notification
        .handle
        .expect("notification handle should be present");
    match handle {
        vci_service_interface::event_notification::Handle::CllHandle(streamed_handle) => {
            assert_eq!(streamed_handle.module_handle, MOCK_MODULE_HANDLE);
            assert_eq!(streamed_handle.cll_handle, MOCK_CLL_HANDLE);
        }
        other => panic!("expected CLL handle, got {other:?}"),
    }

    match notification
        .event_data
        .expect("notification event should be present")
    {
        vci_service_interface::event_notification::EventData::Item(event_item) => {
            assert_result_event_item(&event_item, MOCK_COP_HANDLE);
            assert_eq!(event_item.cop_tag.as_deref(), Some(cop_tag.as_slice()));
        }
        other => panic!("expected item event, got {other:?}"),
    }

    drop(event_stream);
    drop(stream_client);
    drop(command_client);
    drop(setup_client);

    server.shutdown().await;
}

// ADR-204: the tag is echoed on every event for a COP's lifetime, so an
// unbounded tag is a per-event amplification hazard -- StartComPrimitive
// enforces a documented maximum size (64 bytes, see
// `rpc_primitive.rs::MAX_COP_TAG_LEN`) before the tag ever reaches the
// native call.
#[tokio::test]
#[serial]
async fn start_com_primitive_rejects_oversized_cop_tag() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_default_cll(&mut client).await;

    let oversized_tag = vec![0u8; 65];
    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x12, 0x34],
            cop_ctrl_data: None,
            cop_tag: Some(oversized_tag),
        })
        .await
        .expect_err("oversized cop_tag should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

// Boundary companion to `start_com_primitive_rejects_oversized_cop_tag`: a
// tag of exactly `MAX_COP_TAG_LEN` (64) bytes is accepted, not rejected --
// only tags strictly larger than the documented maximum are.
#[tokio::test]
#[serial]
async fn start_com_primitive_accepts_max_length_cop_tag() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_default_cll(&mut client).await;

    let max_tag = vec![0u8; 64];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x12, 0x34],
            cop_ctrl_data: None,
            cop_tag: Some(max_tag),
        })
        .await
        .expect("a cop_tag exactly at MAX_COP_TAG_LEN should be accepted");

    server.shutdown().await;
}

// ADR-218: wires `ParamStructfield` through `SetComParam` end-to-end over a
// real gRPC connection, proving the three previously-`unimplemented` arms
// this ADR removes are actually reachable and no longer reject every
// STRUCTFIELD ComParam outright. `iso22900-mock`'s `PDUSetComParam` ignores
// its payload entirely (always returns `no_error()`, matching this file's
// existing `Unum32` `SetComParam` coverage, whose readback assertions never
// depend on what was actually written) so this only exercises the
// write-side encode path (`to_iso_param`/`structfield_from_proto`), not a
// value round trip -- see `service::convert::tests` (unit tests, in-crate)
// for the standard/vendor struct byte-level round-trip and read-side
// resolution-order coverage a real `GetComParam` readback can't exercise
// through this mock. The vendor write below needs a matching
// `vendor_struct_types` config entry (ADR-218 Decision item 4, as amended):
// config is the sole source of a vendor struct type's entry size, for reads
// AND writes, since the process-lifetime write cache this test used to rely
// on implicitly (write first, no config needed) was removed as a
// heap-disclosure fix -- see `service::rpc::tests` for the unconfigured/
// mismatched rejection coverage.
#[tokio::test]
#[serial]
async fn set_com_param_accepts_standard_and_vendor_structfield_values() {
    let server = TestServer::start_with_extra_config(
        "\n[config.apis.iso22900.libs.\"TestLib\".vendor_struct_types]\n\"0x80000001\" = 2\n",
    )
    .await;
    let mut client = server.client().await;
    let cll_handle = create_default_cll(&mut client).await;

    // Standard struct type (session_timing).
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(7)),
                com_param_class: PduParamClass::PduPcTiming as i32,
                param_data: Some(param_item::ParamData::Structfield(ParamStructfield {
                    data: Some(param_structfield::Data::SessionTiming(
                        ParamSessionTimingList {
                            entries: vec![ParamSessionTiming {
                                session: 1,
                                p2_max: 300,
                                p2_star: 1500,
                            }],
                        },
                    )),
                })),
            }),
        })
        .await
        .expect("a standard structfield SetComParam should reach the mock, not unimplemented");

    // Well-formed vendor struct type.
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(7)),
                com_param_class: PduParamClass::PduPcTiming as i32,
                param_data: Some(param_item::ParamData::Structfield(ParamStructfield {
                    data: Some(param_structfield::Data::VendorSpecific(
                        ParamVendorSpecificStruct {
                            type_url: "pdu-cpst:0x80000001".to_string(),
                            size_of_entry: 2,
                            count_of_entry: 2,
                            value: vec![1, 2, 3, 4],
                        },
                    )),
                })),
            }),
        })
        .await
        .expect("a well-formed vendor structfield SetComParam should succeed");

    server.shutdown().await;
}

// Regression test: a zero-length vendor STRUCTFIELD write (`size_of_entry:
// 0, count_of_entry: 0, value: []`) is a legitimate write per ADR-218
// Decision item 3's `size_of_entry > 0` requirement applying only when
// `count_of_entry` is nonzero. `service::convert::vendor_struct_from_proto`
// already accepted this shape, but `iso22900::encode::EncodedComParam::
// new_owned`'s vendor arm unconditionally rejected `entry_size == 0`
// regardless of count, so this previously failed downstream with an opaque
// `Code::Internal` even though the RPC boundary validated it as OK. This is
// distinct from `vendor_struct_rejects_zero_size_of_entry_with_nonzero_count`
// (in `service::convert::tests`), which covers the still-and-correctly
// rejected `size_of_entry: 0` with a nonzero `count_of_entry`.
#[tokio::test]
#[serial]
async fn set_com_param_accepts_zero_length_vendor_structfield() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_default_cll(&mut client).await;

    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(7)),
                com_param_class: PduParamClass::PduPcTiming as i32,
                param_data: Some(param_item::ParamData::Structfield(ParamStructfield {
                    data: Some(param_structfield::Data::VendorSpecific(
                        ParamVendorSpecificStruct {
                            type_url: "pdu-cpst:0x80000001".to_string(),
                            size_of_entry: 0,
                            count_of_entry: 0,
                            value: vec![],
                        },
                    )),
                })),
            }),
        })
        .await
        .expect("a zero-length vendor structfield SetComParam should succeed");

    server.shutdown().await;
}

// Companion to `set_com_param_accepts_standard_and_vendor_structfield_values`:
// the gRPC boundary itself must reject a malformed vendor structfield
// (ADR-218 Decision item 3) before it ever reaches the native call, not just
// in `service::convert::tests`' unit coverage of the same validation logic.
#[tokio::test]
#[serial]
async fn set_com_param_rejects_malformed_vendor_structfield_at_the_rpc_boundary() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_default_cll(&mut client).await;

    let bad_grammar = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(7)),
                com_param_class: PduParamClass::PduPcTiming as i32,
                param_data: Some(param_item::ParamData::Structfield(ParamStructfield {
                    data: Some(param_structfield::Data::VendorSpecific(
                        ParamVendorSpecificStruct {
                            type_url: "not-a-type-url".to_string(),
                            size_of_entry: 1,
                            count_of_entry: 1,
                            value: vec![0],
                        },
                    )),
                })),
            }),
        })
        .await
        .expect_err("malformed type_url grammar should be rejected at the RPC boundary");
    assert_eq!(bad_grammar.code(), tonic::Code::InvalidArgument);

    let standard_type_under_vendor_path = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(7)),
                com_param_class: PduParamClass::PduPcTiming as i32,
                param_data: Some(param_item::ParamData::Structfield(ParamStructfield {
                    data: Some(param_structfield::Data::VendorSpecific(
                        ParamVendorSpecificStruct {
                            type_url: "pdu-cpst:0x00000001".to_string(),
                            size_of_entry: 6,
                            count_of_entry: 1,
                            value: vec![0; 6],
                        },
                    )),
                })),
            }),
        })
        .await
        .expect_err("a standard ComParamStructType under the vendor path should be rejected");
    assert_eq!(
        standard_type_under_vendor_path.code(),
        tonic::Code::InvalidArgument
    );

    server.shutdown().await;
}

// ADR-218 Decision item 4 (as amended): a non-empty vendor STRUCTFIELD write
// for a struct type with no `vendor_struct_types` config entry must fail
// loudly at the RPC boundary, not silently succeed the way it did before the
// process-lifetime write cache was removed (Codex-review-found local
// heap-disclosure fix).
#[tokio::test]
#[serial]
async fn set_com_param_rejects_unconfigured_vendor_structfield_write_at_the_rpc_boundary() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_default_cll(&mut client).await;

    let err = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(7)),
                com_param_class: PduParamClass::PduPcTiming as i32,
                param_data: Some(param_item::ParamData::Structfield(ParamStructfield {
                    data: Some(param_structfield::Data::VendorSpecific(
                        ParamVendorSpecificStruct {
                            type_url: "pdu-cpst:0x80000001".to_string(),
                            size_of_entry: 2,
                            count_of_entry: 2,
                            value: vec![1, 2, 3, 4],
                        },
                    )),
                })),
            }),
        })
        .await
        .expect_err(
            "a non-empty vendor structfield write with no vendor_struct_types config entry \
             should be rejected at the RPC boundary",
        );
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    server.shutdown().await;
}

// Companion to the unconfigured case above: a configured entry size that
// does not match the write's own declared `size_of_entry` must also be
// rejected at the RPC boundary, naming the configured size.
#[tokio::test]
#[serial]
async fn set_com_param_rejects_mismatched_vendor_structfield_size_at_the_rpc_boundary() {
    let server = TestServer::start_with_extra_config(
        "\n[config.apis.iso22900.libs.\"TestLib\".vendor_struct_types]\n\"0x80000001\" = 4\n",
    )
    .await;
    let mut client = server.client().await;
    let cll_handle = create_default_cll(&mut client).await;

    let err = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(7)),
                com_param_class: PduParamClass::PduPcTiming as i32,
                param_data: Some(param_item::ParamData::Structfield(ParamStructfield {
                    data: Some(param_structfield::Data::VendorSpecific(
                        ParamVendorSpecificStruct {
                            type_url: "pdu-cpst:0x80000001".to_string(),
                            size_of_entry: 2,
                            count_of_entry: 2,
                            value: vec![1, 2, 3, 4],
                        },
                    )),
                })),
            }),
        })
        .await
        .expect_err(
            "a declared size_of_entry that does not match the configured value should be \
             rejected at the RPC boundary",
        );
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains('4'),
        "error message should name the configured size: {}",
        err.message()
    );

    server.shutdown().await;
}
