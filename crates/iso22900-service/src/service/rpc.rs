use std::{collections::HashMap, pin::Pin, sync::Arc};

use tokio::sync::{Mutex, mpsc, watch::Receiver};

use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use iso22900::{
    ComLogicalLinkHandle, DPduApi, DPduApiError, E_PDU_CPST, EventNotification, ModuleHandle,
};
use vci_service_interface::{
    CancelComPrimitiveRequest, ComLogicalLinkResponse, ComParamResponse, ComPrimitiveResponse,
    ConflictingResourcesResponse, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest,
    DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest, EventItemResponse,
    EventNotification as VciServiceEventNotification, GetComParamRequest,
    GetConflictingResourcesRequest, GetEventItemRequest, GetModuleIdsRequest, GetObjectIdRequest,
    GetResourceIdsRequest, GetResourceStatusRequest, GetStatusRequest, GetTimestampRequest,
    GetUniqueRespIdTableRequest, GetVersionRequest, IoCtlRequest, IoCtlResponse,
    LockResourceRequest, ModuleConnectRequest, ModuleDisconnectRequest, ModuleIdsResponse,
    ObjectIdResponse, ResourceIdsResponse, ResourceStatusResponse, Response as VciServiceResponse,
    SetComParamRequest, SetUniqueRespIdTableRequest, StartComPrimitiveRequest, StatusResponse,
    SubscribeEventRequest, TimestampResponse, UniqueRespIdTableResponse, UnlockResourceRequest,
    VersionResponse, vci_service_server::VciService,
};
use vci_service_launcher::vci_server::VciServer;

use crate::config::StartupConfig;
use crate::error::{
    api_not_initialized_status, map_construct_error, map_registry_error, map_runtime_error,
    map_runtime_error_for_link,
};
#[cfg(test)]
use crate::service::convert::cpst_from_raw;
use crate::service::convert::cpst_raw;
use crate::service::events::run_event_subscription_task;

pub(super) type SubscriptionKey = (u32, u32);
pub(super) type SubscriptionSender =
    mpsc::UnboundedSender<Result<VciServiceEventNotification, Status>>;
pub(super) type SubscriptionMap = HashMap<SubscriptionKey, SubscriptionSender>;
pub(super) type EventStream =
    Pin<Box<dyn Stream<Item = Result<VciServiceEventNotification, Status>> + Send>>;

pub(super) const API_NOT_INITIALIZED: &str = "API not initialized";

/// Keyed by the same `(module_handle, cll_handle, cop_handle)` numeric
/// triple `to_proto_cop_handle` uses elsewhere (`handles.rs`). Populated at
/// `StartComPrimitive` (ADR-204) and evicted the moment a terminal
/// `PDU_COPST_FINISHED`/`PDU_COPST_CANCELLED` status event is observed for
/// that COP -- see `events.rs::to_proto_event_item`.
pub(super) type CopTagMap = HashMap<(u32, u32, u32), Vec<u8>>;

#[derive(Clone)]
pub struct Iso22900Service {
    pub(super) startup_config: StartupConfig,
    pub(super) api: Arc<Mutex<Option<DPduApi>>>,
    pub(super) event_notifications: mpsc::UnboundedSender<iso22900::EventNotification>,
    pub(super) subscriptions: Arc<Mutex<SubscriptionMap>>,
    pub(super) cop_tags: Arc<Mutex<CopTagMap>>,
    pub(super) shutdown: tokio::sync::watch::Receiver<bool>,
}

impl Iso22900Service {
    pub(super) async fn terminate_subscription(
        &self,
        h_mod: ModuleHandle,
        h_cll: ComLogicalLinkHandle,
    ) {
        let key: SubscriptionKey = (h_mod.0, h_cll.0);
        let mut subscriptions = self.subscriptions.lock().await;
        if let Some(sender_tx) = subscriptions.remove(&key) {
            terminate_stream_sender(sender_tx);
        }
    }

    pub(super) async fn terminate_subscriptions_for_module(&self, h_mod: ModuleHandle) {
        let mut subscriptions = self.subscriptions.lock().await;

        let keys = subscriptions
            .keys()
            .filter(|(mod_id, _)| *mod_id == h_mod.0)
            .copied()
            .collect::<Vec<_>>();
        remove_subscriptions(&mut subscriptions, keys);
    }

    /// Bounds a COP tag's leak (ADR-204 Consequences) to at most the owning
    /// CLL's lifetime: sweeps every `self.cop_tags` entry whose
    /// `(module, cll)` prefix matches, for a COP whose terminal event was
    /// never fetched before its CLL was torn down.
    pub(super) async fn terminate_cop_tags_for_link(
        &self,
        h_mod: ModuleHandle,
        h_cll: ComLogicalLinkHandle,
    ) {
        let mut cop_tags = self.cop_tags.lock().await;
        cop_tags.retain(|(module, cll, _), _| !(*module == h_mod.0 && *cll == h_cll.0));
    }

    /// Like [`Self::terminate_cop_tags_for_link`], but for a whole module
    /// tear-down (`ModuleDisconnect`): sweeps every `self.cop_tags` entry
    /// whose `module` component matches, regardless of which CLL it belongs
    /// to.
    pub(super) async fn terminate_cop_tags_for_module(&self, h_mod: ModuleHandle) {
        let mut cop_tags = self.cop_tags.lock().await;
        cop_tags.retain(|(module, _, _), _| *module != h_mod.0);
    }

    // Only exercised by a test that relies on the debug-only runtime
    // VCI_CONFIG_PATH override (see ADR-073), so this helper itself is
    // debug-only to avoid a dead-code warning under `cargo test --release`.
    #[cfg(all(test, debug_assertions))]
    pub(super) async fn terminate_all_subscriptions(&self) {
        let mut subscriptions = self.subscriptions.lock().await;
        let active_keys = subscriptions.keys().copied().collect::<Vec<_>>();
        remove_subscriptions(&mut subscriptions, active_keys);
    }

    pub(super) async fn with_api<T>(
        &self,
        f: impl FnOnce(&DPduApi) -> Result<T, DPduApiError>,
    ) -> Result<T, Status> {
        let api = self.api.lock().await;
        let api = api
            .as_ref()
            .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;
        f(api).map_err(map_runtime_error)
    }

    /// Like [`Self::with_api`], but for a call that already has a Module+CLL
    /// handle in scope: on failure, best-effort fetches the native
    /// `PDUGetLastError`-equivalent (while the lock is still held) to fill
    /// in the returned `Status`'s `ErrorDetail.error_event_data`. Handlers
    /// with only a module handle (or no handle at all) in scope should keep
    /// using plain [`Self::with_api`] -- `error_event_data` simply stays
    /// absent for those, which is documented, expected behavior.
    pub(super) async fn with_api_for_link<T>(
        &self,
        module: ModuleHandle,
        cll: ComLogicalLinkHandle,
        f: impl FnOnce(&DPduApi) -> Result<T, DPduApiError>,
    ) -> Result<T, Status> {
        let api = self.api.lock().await;
        let api = api
            .as_ref()
            .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;
        f(api).map_err(|error| map_runtime_error_for_link(api, module, cll, error))
    }

    pub(super) fn empty_response() -> Response<VciServiceResponse> {
        Response::new(VciServiceResponse {})
    }

    /// ADR-218 Decision item 4's (as amended) entry-size resolution for a
    /// vendor STRUCTFIELD `size_of_entry`: per-library operator config is
    /// the SOLE source, for both read and write. Never called for a
    /// standard struct type; on read, never called for an empty
    /// (`ParamActEntries == 0`) table (`convert::read_borrowed_param_value`'s
    /// doc comment); on write, never called for an empty
    /// (`count_of_entry == 0`) write (`convert::vendor_struct_from_proto`'s
    /// doc comment).
    ///
    /// There used to be a second source here -- a process-lifetime cache
    /// populated by this process's own prior successful writes of a struct
    /// type. It was removed (Codex-review-found local heap-disclosure,
    /// ADR-218 amendment): a successful native `PDUSetComParam` never
    /// validates a caller-declared entry size against the connected
    /// library's real layout, so a size learned from a write is never more
    /// trustworthy than the request it came from, and trusting it on a
    /// later read constructed an unchecked slice over a native allocation
    /// whose true length the D-PDU API never exposes.
    ///
    /// A configured size of `0` is treated the same as "not configured" --
    /// `0` is never a valid per-entry byte layout for a nonempty table/write.
    pub(super) fn resolve_vendor_struct_entry_size(
        &self,
        struct_type: E_PDU_CPST,
    ) -> Result<u32, Status> {
        let struct_type_raw = cpst_raw(struct_type);

        if let Some(size) = vci_service_launcher::config::find_vendor_struct_type_size(
            "iso22900",
            vci_service_launcher::vci_server::current_arch(),
            &self.startup_config.library_name,
            struct_type_raw,
        )
        .filter(|&size| size != 0)
        {
            return Ok(size);
        }

        Err(Status::failed_precondition(format!(
            "vendor ComParamStructType 0x{struct_type_raw:08x} has no known entry size: \
             configure vendor_struct_types for this library"
        )))
    }
}

fn terminate_stream_sender(sender_tx: SubscriptionSender) {
    if !sender_tx.is_closed() {
        let _ = sender_tx.send(Err(Status::cancelled("Subscription terminated")));
    }
}

fn remove_subscriptions(
    subscriptions: &mut SubscriptionMap,
    keys: impl IntoIterator<Item = SubscriptionKey>,
) {
    for key in keys {
        if let Some(sender_tx) = subscriptions.remove(&key) {
            terminate_stream_sender(sender_tx);
        }
    }
}

#[tonic::async_trait]
impl VciService for Iso22900Service {
    async fn get_module_ids(
        &self,
        request: Request<GetModuleIdsRequest>,
    ) -> Result<Response<ModuleIdsResponse>, Status> {
        self.rpc_get_module_ids(request).await
    }

    async fn module_connect(
        &self,
        request: Request<ModuleConnectRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        self.rpc_module_connect(request).await
    }

    async fn module_disconnect(
        &self,
        request: Request<ModuleDisconnectRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        self.rpc_module_disconnect(request).await
    }

    async fn get_version(
        &self,
        request: Request<GetVersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        self.rpc_get_version(request).await
    }

    async fn get_timestamp(
        &self,
        request: Request<GetTimestampRequest>,
    ) -> Result<Response<TimestampResponse>, Status> {
        self.rpc_get_timestamp(request).await
    }

    async fn get_resource_status(
        &self,
        request: Request<GetResourceStatusRequest>,
    ) -> Result<Response<ResourceStatusResponse>, Status> {
        self.rpc_get_resource_status(request).await
    }

    async fn get_resource_ids(
        &self,
        request: Request<GetResourceIdsRequest>,
    ) -> Result<Response<ResourceIdsResponse>, Status> {
        self.rpc_get_resource_ids(request).await
    }

    async fn get_conflicting_resources(
        &self,
        request: Request<GetConflictingResourcesRequest>,
    ) -> Result<Response<ConflictingResourcesResponse>, Status> {
        self.rpc_get_conflicting_resources(request).await
    }

    async fn create_com_logical_link(
        &self,
        request: Request<CreateComLogicalLinkRequest>,
    ) -> Result<Response<ComLogicalLinkResponse>, Status> {
        self.rpc_create_com_logical_link(request).await
    }

    async fn destroy_com_logical_link(
        &self,
        request: Request<DestroyComLogicalLinkRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        self.rpc_destroy_com_logical_link(request).await
    }

    async fn connect_com_logical_link(
        &self,
        request: Request<ConnectComLogicalLinkRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        self.rpc_connect_com_logical_link(request).await
    }

    async fn disconnect_com_logical_link(
        &self,
        request: Request<DisconnectComLogicalLinkRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        self.rpc_disconnect_com_logical_link(request).await
    }

    async fn lock_resource(
        &self,
        request: Request<LockResourceRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        self.rpc_lock_resource(request).await
    }

    async fn unlock_resource(
        &self,
        request: Request<UnlockResourceRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        self.rpc_unlock_resource(request).await
    }

    async fn get_com_param(
        &self,
        request: Request<GetComParamRequest>,
    ) -> Result<Response<ComParamResponse>, Status> {
        self.rpc_get_com_param(request).await
    }

    async fn set_com_param(
        &self,
        request: Request<SetComParamRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        self.rpc_set_com_param(request).await
    }

    async fn start_com_primitive(
        &self,
        request: Request<StartComPrimitiveRequest>,
    ) -> Result<Response<ComPrimitiveResponse>, Status> {
        self.rpc_start_com_primitive(request).await
    }

    async fn cancel_com_primitive(
        &self,
        request: Request<CancelComPrimitiveRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        self.rpc_cancel_com_primitive(request).await
    }

    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        self.rpc_get_status(request).await
    }

    async fn get_event_item(
        &self,
        request: Request<GetEventItemRequest>,
    ) -> Result<Response<EventItemResponse>, Status> {
        self.rpc_get_event_item(request).await
    }

    type SubscribeEventStream = EventStream;

    async fn subscribe_event(
        &self,
        request: Request<SubscribeEventRequest>,
    ) -> Result<Response<Self::SubscribeEventStream>, Status> {
        self.rpc_subscribe_event(request).await
    }

    async fn io_ctl(
        &self,
        request: Request<IoCtlRequest>,
    ) -> Result<Response<IoCtlResponse>, Status> {
        self.rpc_io_ctl(request).await
    }

    async fn get_object_id(
        &self,
        request: Request<GetObjectIdRequest>,
    ) -> Result<Response<ObjectIdResponse>, Status> {
        self.rpc_get_object_id(request).await
    }

    async fn get_unique_resp_id_table(
        &self,
        request: Request<GetUniqueRespIdTableRequest>,
    ) -> Result<Response<UniqueRespIdTableResponse>, Status> {
        self.rpc_get_unique_resp_id_table(request).await
    }

    async fn set_unique_resp_id_table(
        &self,
        request: Request<SetUniqueRespIdTableRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        self.rpc_set_unique_resp_id_table(request).await
    }
}

impl VciServer for Iso22900Service {
    type StartupConfig = StartupConfig;

    fn get_startup_config(
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self::StartupConfig, vci_service_launcher::BoxError> {
        Ok(crate::config::parse_startup_config(args)?)
    }

    fn service_identity(
        config: &Self::StartupConfig,
    ) -> vci_service_launcher::vci_server::ServiceIdentity {
        vci_service_launcher::vci_server::ServiceIdentity {
            api_name: "iso22900",
            library_name: config.library_name.clone(),
            arch: vci_service_launcher::vci_server::current_arch(),
        }
    }

    async fn new(
        startup_config: Self::StartupConfig,
        shutdown: Receiver<bool>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // A `library_path` configured for this library name takes priority over
        // RDF-based auto-discovery, letting deployers add libraries manually and
        // letting the library be resolved without an RDF file present at all.
        let library_path = iso22900_registry::resolve_library_path(
            vci_service_launcher::vci_server::current_arch(),
            &startup_config.library_name,
        )
        .map_err(map_registry_error)?;
        // Fail fast on a noncanonical `vendor_struct_types` config key
        // (ADR-218): `resolve_vendor_struct_entry_size` only ever looks up
        // one canonical `"0x<8 lowercase hex digits>"` key per call, so an
        // uppercase or wrong-width sibling key would otherwise stay
        // silently unreachable -- every nonempty read/write of that struct
        // type would fail `FAILED_PRECONDITION` claiming "unconfigured"
        // even though an operator believes they configured it. Mirrors
        // `j2534-0404-service`'s identical `resolve_vendor_ioctls` fail-fast
        // validation for its sibling `vendor_ioctls` table (ADR-219, as
        // amended).
        vci_service_launcher::config::validate_vendor_struct_type_keys(
            "iso22900",
            vci_service_launcher::vci_server::current_arch(),
            &startup_config.library_name,
        )
        .map_err(|msg| -> Box<dyn std::error::Error> { msg.into() })?;
        let api = DPduApi::new(&library_path).map_err(map_construct_error)?;
        let api = Arc::new(Mutex::new(Some(api)));
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let cop_tags = Arc::new(Mutex::new(HashMap::new()));
        let (event_notifications_tx, event_notifications_rx) =
            mpsc::unbounded_channel::<EventNotification>();

        tokio::spawn(run_event_subscription_task(
            event_notifications_rx,
            Arc::clone(&api),
            Arc::clone(&subscriptions),
            Arc::clone(&cop_tags),
        ));

        let subs_for_shutdown = Arc::clone(&subscriptions);
        let mut shutdown_rx = shutdown.clone();
        tokio::spawn(async move {
            let _ = shutdown_rx.changed().await;
            let mut subs = subs_for_shutdown.lock().await;
            let keys: Vec<_> = subs.keys().copied().collect();
            remove_subscriptions(&mut subs, keys);
        });

        Ok(Self {
            startup_config,
            api,
            event_notifications: event_notifications_tx,
            subscriptions,
            cop_tags,
            shutdown,
        })
    }

    fn get_requested_port(&self) -> Option<u16> {
        self.startup_config.requested_port
    }
}

// All tests below rely on the debug-only runtime VCI_CONFIG_PATH override
// (see ADR-073) to redirect `iso22900-registry`'s library-path resolution at
// the compiled mock, so the whole module is debug-only and does not exist
// under `cargo test --release`.
#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::config::StartupConfig;
    use serial_test::serial;
    use vci_service_interface::ModuleHandle as VciServiceModuleHandle;

    /// Points the shared `config.toml`'s library-path config (resolved by
    /// `iso22900-registry`) at the compiled `iso22900-mock` cdylib for
    /// `"TestLib"` (via `VCI_CONFIG_PATH`), so `Iso22900Service::new` loads
    /// the mock without needing an RDF file.
    ///
    /// Callers must be `#[serial]`: this writes a shared config file and
    /// sets a process-global env var, both of which race under concurrent
    /// test execution within this process. The config path is also
    /// per-process-unique (via `std::process::id()`, ADR-195), closing a
    /// separate cross-process race that `#[serial]` -- itself only ever an
    /// intra-process convention -- could never address on its own: two
    /// separate `cargo test` processes would otherwise still race on the
    /// exact same fixed file path.
    fn set_mock_library_path_config() {
        let lib_path = iso22900_mock::mock_library_path()
            .expect("mock cdylib should be discoverable after build");
        let config_path = std::env::temp_dir().join(format!(
            "iso22900-service-rpc-test-config-{}.toml",
            std::process::id()
        ));
        let config_toml = format!(
            "[config.apis.iso22900.libs.\"TestLib\"]\nlibrary_path = {:?}\n",
            lib_path.display().to_string()
        );
        std::fs::write(&config_path, config_toml).expect("config file should be writable");
        unsafe {
            std::env::set_var("VCI_CONFIG_PATH", &config_path);
        }
    }

    #[tokio::test]
    #[serial]
    async fn get_module_ids_uses_mock_library() {
        set_mock_library_path_config();
        let service = Iso22900Service::new(
            StartupConfig {
                library_name: "TestLib".into(),
                requested_port: None,
            },
            tokio::sync::watch::channel(false).1,
        )
        .await
        .expect("service should initialize with mock library");

        let response = service
            .rpc_get_module_ids(Request::new(GetModuleIdsRequest {}))
            .await
            .expect("module id rpc should succeed");

        let module_ids = response
            .into_inner()
            .module_id_list
            .expect("module ids should be present");

        assert_eq!(module_ids.module_data.len(), 1);
        let module = &module_ids.module_data[0];
        assert_eq!(
            module
                .module_handle
                .as_ref()
                .map(|handle| handle.module_handle),
            Some(1001)
        );
        assert_eq!(module.module_type_id, 9001);
        assert_eq!(module.vendor_module_name, "iso22900-mock");
        assert_eq!(module.vendor_additional_info, "mock-vehicle");
    }

    #[tokio::test]
    #[serial]
    async fn get_version_uses_mock_library() {
        set_mock_library_path_config();
        let service = Iso22900Service::new(
            StartupConfig {
                library_name: "TestLib".into(),
                requested_port: None,
            },
            tokio::sync::watch::channel(false).1,
        )
        .await
        .expect("service should initialize with mock library");

        let response = service
            .rpc_get_version(Request::new(GetVersionRequest {
                module_handle: Some(VciServiceModuleHandle {
                    module_handle: 1001,
                }),
            }))
            .await
            .expect("version rpc should succeed");

        let version = response
            .into_inner()
            .version_data
            .expect("version data should be present");

        assert_eq!(version.hw_name, "Mock Hardware");
        assert_eq!(version.fw_name, "Mock Firmware");
        assert_eq!(version.vendor_name, "Mock Vendor");
        assert_eq!(version.pdu_api_sw_name, "iso22900-mock");
    }

    #[tokio::test]
    #[serial]
    async fn terminate_all_subscriptions_sends_cancelled_and_clears_map() {
        set_mock_library_path_config();
        let service = Iso22900Service::new(
            StartupConfig {
                library_name: "TestLib".into(),
                requested_port: None,
            },
            tokio::sync::watch::channel(false).1,
        )
        .await
        .expect("service should initialize with mock library");

        let (tx, mut rx) = mpsc::unbounded_channel::<Result<VciServiceEventNotification, Status>>();
        service.subscriptions.lock().await.insert((1001, 3001), tx);

        service.terminate_all_subscriptions().await;

        let item = rx
            .recv()
            .await
            .expect("subscription stream should receive terminal item");
        let err = item.expect_err("terminal item should be a cancelled status");
        assert_eq!(err.code(), tonic::Code::Cancelled);
        assert!(service.subscriptions.lock().await.is_empty());
    }

    // ── ADR-218 Decision item 4 (as amended): vendor entry-size resolution
    // is config-only, for both read and write. The process-lifetime write
    // cache these tests used to exercise was removed (Codex-review-found
    // local heap-disclosure via an unverified client-declared entry size;
    // see `resolve_vendor_struct_entry_size`'s doc comment) ─────────────

    /// Like [`set_mock_library_path_config`], but also configures a single
    /// `vendor_struct_types` entry for `"TestLib"` (ADR-218 Decision item
    /// 4a).
    fn set_mock_library_path_config_with_vendor_struct_type(struct_type_key: &str, size: u32) {
        let lib_path = iso22900_mock::mock_library_path()
            .expect("mock cdylib should be discoverable after build");
        let config_path = std::env::temp_dir().join(format!(
            "iso22900-service-rpc-test-config-{}.toml",
            std::process::id()
        ));
        let config_toml = format!(
            "[config.apis.iso22900.libs.\"TestLib\"]\nlibrary_path = {:?}\n\n[config.apis.iso22900.libs.\"TestLib\".vendor_struct_types]\n{:?} = {}\n",
            lib_path.display().to_string(),
            struct_type_key,
            size,
        );
        std::fs::write(&config_path, config_toml).expect("config file should be writable");
        unsafe {
            std::env::set_var("VCI_CONFIG_PATH", &config_path);
        }
    }

    async fn new_test_service() -> Iso22900Service {
        Iso22900Service::new(
            StartupConfig {
                library_name: "TestLib".into(),
                requested_port: None,
            },
            tokio::sync::watch::channel(false).1,
        )
        .await
        .expect("service should initialize with mock library")
    }

    /// Like [`set_mock_library_path_config_with_vendor_struct_type`], but
    /// writes `struct_type_key` verbatim (not necessarily canonical) so
    /// tests can exercise `Iso22900Service::new`'s startup
    /// `validate_vendor_struct_type_keys` check (ADR-218) against a
    /// noncanonical key.
    fn set_mock_library_path_config_with_raw_vendor_struct_type_key(
        struct_type_key: &str,
        size: u32,
    ) {
        let lib_path = iso22900_mock::mock_library_path()
            .expect("mock cdylib should be discoverable after build");
        let config_path = std::env::temp_dir().join(format!(
            "iso22900-service-rpc-test-config-{}.toml",
            std::process::id()
        ));
        let config_toml = format!(
            "[config.apis.iso22900.libs.\"TestLib\"]\nlibrary_path = {:?}\n\n[config.apis.iso22900.libs.\"TestLib\".vendor_struct_types]\n\"{struct_type_key}\" = {size}\n",
            lib_path.display().to_string(),
        );
        std::fs::write(&config_path, config_toml).expect("config file should be writable");
        unsafe {
            std::env::set_var("VCI_CONFIG_PATH", &config_path);
        }
    }

    #[tokio::test]
    #[serial]
    async fn new_fails_startup_when_vendor_struct_type_key_is_uppercase() {
        set_mock_library_path_config_with_raw_vendor_struct_type_key("0x8000000A", 12);

        let result = Iso22900Service::new(
            StartupConfig {
                library_name: "TestLib".into(),
                requested_port: None,
            },
            tokio::sync::watch::channel(false).1,
        )
        .await;
        // `Iso22900Service` (the `Ok` variant) does not implement `Debug`, so
        // `Result::expect_err` cannot be used here.
        let err = match result {
            Ok(_) => panic!("an uppercase vendor_struct_types key must fail startup"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("0x8000000A"),
            "startup error must name the offending key: {err}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn new_fails_startup_when_vendor_struct_type_key_is_wrong_width() {
        set_mock_library_path_config_with_raw_vendor_struct_type_key("0x800000a", 12);

        let result = Iso22900Service::new(
            StartupConfig {
                library_name: "TestLib".into(),
                requested_port: None,
            },
            tokio::sync::watch::channel(false).1,
        )
        .await;
        // `Iso22900Service` (the `Ok` variant) does not implement `Debug`, so
        // `Result::expect_err` cannot be used here.
        let err = match result {
            Ok(_) => panic!("a wrong-width vendor_struct_types key must fail startup"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("0x800000a"),
            "startup error must name the offending key: {err}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn new_succeeds_when_vendor_struct_types_table_is_absent() {
        set_mock_library_path_config();
        // Regression guard: startup validation must not reject a config
        // with no vendor_struct_types table at all.
        let _service = new_test_service().await;
    }

    #[tokio::test]
    #[serial]
    async fn new_succeeds_when_vendor_struct_type_keys_are_canonical() {
        // Regression guard: startup validation must not reject a
        // canonically-keyed vendor_struct_types table.
        set_mock_library_path_config_with_vendor_struct_type("0x80000001", 12);
        let _service = new_test_service().await;
    }

    #[tokio::test]
    #[serial]
    async fn resolve_vendor_struct_entry_size_fails_precondition_when_unconfigured() {
        set_mock_library_path_config();
        let service = new_test_service().await;

        let err = service
            .resolve_vendor_struct_entry_size(cpst_from_raw(0x8000_0001))
            .expect_err("an unconfigured vendor struct type must fail loudly");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    #[serial]
    async fn resolve_vendor_struct_entry_size_returns_configured_value() {
        set_mock_library_path_config_with_vendor_struct_type("0x80000001", 12);
        let service = new_test_service().await;

        let size = service
            .resolve_vendor_struct_entry_size(cpst_from_raw(0x8000_0001))
            .expect("a configured vendor_struct_types entry should resolve");
        assert_eq!(size, 12);
    }

    #[tokio::test]
    #[serial]
    async fn resolve_vendor_struct_entry_size_fails_precondition_when_configured_zero() {
        set_mock_library_path_config_with_vendor_struct_type("0x80000001", 0);
        let service = new_test_service().await;

        let err = service
            .resolve_vendor_struct_entry_size(cpst_from_raw(0x8000_0001))
            .expect_err(
                "a zero-configured vendor struct type must still fail loudly, not resolve to 0",
            );
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    // ── ADR-218 Decision item 4 (as amended): write-side entry-size
    // validation, shared by `SetComParam` (`rpc_link.rs`) and
    // `SetUniqueRespIdTable` (`rpc_misc.rs`) via the identical
    // `resolve_vendor_struct_entry_size` resolver `GetComParam`/
    // `GetUniqueRespIdTable` consult on read ────────────────────────────

    fn vendor_structfield_param_item(
        param_id: u32,
        struct_type_hex: &str,
        size_of_entry: u32,
        count_of_entry: u32,
        value: Vec<u8>,
    ) -> vci_service_interface::ParamItem {
        vci_service_interface::ParamItem {
            id: Some(vci_service_interface::param_item::Id::ParamId(param_id)),
            com_param_class: vci_service_interface::PduParamClass::PduPcTiming as i32,
            param_data: Some(vci_service_interface::param_item::ParamData::Structfield(
                vci_service_interface::ParamStructfield {
                    data: Some(
                        vci_service_interface::param_structfield::Data::VendorSpecific(
                            vci_service_interface::ParamVendorSpecificStruct {
                                type_url: format!("pdu-cpst:{struct_type_hex}"),
                                size_of_entry,
                                count_of_entry,
                                value,
                            },
                        ),
                    ),
                },
            )),
        }
    }

    fn set_com_param_request(
        param_item: vci_service_interface::ParamItem,
    ) -> Request<SetComParamRequest> {
        Request::new(SetComParamRequest {
            cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                module_handle: 1001,
                cll_handle: 1,
            }),
            param_item: Some(param_item),
        })
    }

    #[tokio::test]
    #[serial]
    async fn set_com_param_vendor_structfield_write_fails_precondition_when_unconfigured() {
        set_mock_library_path_config();
        let service = new_test_service().await;

        let err = service
            .rpc_set_com_param(set_com_param_request(vendor_structfield_param_item(
                7,
                "0x80000001",
                2,
                2,
                vec![1, 2, 3, 4],
            )))
            .await
            .expect_err(
                "a non-empty vendor STRUCTFIELD write with no vendor_struct_types config \
                 should fail loudly, not silently succeed and get trusted later",
            );
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    #[serial]
    async fn set_com_param_vendor_structfield_write_succeeds_when_size_matches_config() {
        set_mock_library_path_config_with_vendor_struct_type("0x80000001", 2);
        let service = new_test_service().await;

        service
            .rpc_set_com_param(set_com_param_request(vendor_structfield_param_item(
                7,
                "0x80000001",
                2,
                2,
                vec![1, 2, 3, 4],
            )))
            .await
            .expect(
                "a vendor STRUCTFIELD write whose size_of_entry matches the configured value \
                 should succeed",
            );
    }

    #[tokio::test]
    #[serial]
    async fn set_com_param_vendor_structfield_write_rejects_mismatched_size_of_entry() {
        set_mock_library_path_config_with_vendor_struct_type("0x80000001", 4);
        let service = new_test_service().await;

        let err = service
            .rpc_set_com_param(set_com_param_request(vendor_structfield_param_item(
                7,
                "0x80000001",
                2,
                2,
                vec![1, 2, 3, 4],
            )))
            .await
            .expect_err(
                "a declared size_of_entry that does not match the configured value should be \
                 rejected, not silently forwarded to the native call",
            );
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            err.message().contains('4'),
            "error message should name the configured size: {}",
            err.message()
        );
    }

    #[tokio::test]
    #[serial]
    async fn set_com_param_empty_vendor_structfield_write_succeeds_without_config() {
        set_mock_library_path_config();
        let service = new_test_service().await;

        service
            .rpc_set_com_param(set_com_param_request(vendor_structfield_param_item(
                7,
                "0x80000001",
                0,
                0,
                vec![],
            )))
            .await
            .expect(
                "an empty (count_of_entry: 0) vendor STRUCTFIELD write needs no \
                 vendor_struct_types config -- this must not regress",
            );
    }

    /// Regression guard for the empty-write-poison exploit path (Codex
    /// review finding, closed by removing the process-lifetime write
    /// cache): an empty write carrying an arbitrary, untrustworthy
    /// `size_of_entry` must succeed (it is harmless -- zero entries never
    /// reach the native call in any size-dependent way) but must never be
    /// trusted for anything afterward. With the cache gone there is nothing
    /// left for it to poison, so a later read of the same struct type must
    /// still require config exactly as if the empty write had never
    /// happened.
    #[tokio::test]
    #[serial]
    async fn set_com_param_empty_write_poison_does_not_satisfy_a_later_reads_config_requirement() {
        set_mock_library_path_config();
        let service = new_test_service().await;

        service
            .rpc_set_com_param(set_com_param_request(vendor_structfield_param_item(
                7,
                "0x80000001",
                0xFFFF_FFFF,
                0,
                vec![],
            )))
            .await
            .expect("an empty vendor STRUCTFIELD write succeeds regardless of size_of_entry");

        let err = service
            .resolve_vendor_struct_entry_size(cpst_from_raw(0x8000_0001))
            .expect_err(
                "a prior empty write must not satisfy a later read's config requirement, even \
                 one that declared an arbitrary size_of_entry",
            );
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }
}
