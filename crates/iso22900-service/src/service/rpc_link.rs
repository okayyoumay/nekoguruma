use tonic::{Request, Response, Status};
use tracing::{debug, info, warn};

use iso22900::{E_PDU_OBJT, ObjectId, ObjectType, Resource};
use vci_service_interface::{
    ComLogicalLinkResponse, ComParamResponse, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest,
    GetComParamRequest, LockResourceRequest, PduParamClass, Response as VciServiceResponse,
    SetComParamRequest, UnlockResourceRequest,
    create_com_logical_link_request::Resource as CreateComLogicalLinkResource,
};

use crate::error::{api_not_initialized_status, map_runtime_error, map_runtime_error_for_link};
use crate::service::convert::{
    borrowed_param_to_item, cll_create_flag_to_iso, resolve_param_item_id, to_iso_param,
    to_resource_descriptor,
};
use crate::service::handles::{
    require_cll_message, require_module_message, resource_id, to_proto_cll_handle,
};
use crate::service::rpc::{API_NOT_INITIALIZED, Iso22900Service};

impl Iso22900Service {
    pub(super) async fn rpc_create_com_logical_link(
        &self,
        request: Request<CreateComLogicalLinkRequest>,
    ) -> Result<Response<ComLogicalLinkResponse>, Status> {
        let request = request.into_inner();
        let h_mod = require_module_message(request.module_handle, "module_handle")?;
        let creation_flags = cll_create_flag_to_iso(request.cll_create_flag);

        // Hold the lock throughout resource resolution and link creation.
        let api = self.api.lock().await;
        let api = api
            .as_ref()
            .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;

        let descriptor = match request.resource {
            Some(CreateComLogicalLinkResource::RscData(data)) => {
                let resource = to_resource_descriptor(data, |object_type, name| {
                    api.get_object_id(object_type, name)
                        .map_err(map_runtime_error)
                })?;
                Resource::ByDescriptor(resource)
            }
            Some(CreateComLogicalLinkResource::ResourceId(value)) => {
                Resource::ById(resource_id(value))
            }
            Some(CreateComLogicalLinkResource::ResourceName(name)) => {
                let resolved = api
                    .get_object_id(ObjectType(E_PDU_OBJT::PDU_OBJT_RESOURCE), name.as_str())
                    .map_err(map_runtime_error)?;
                Resource::ById(resource_id(resolved.0))
            }
            None => {
                return Err(Status::invalid_argument(
                    "resource must be set to either rsc_data or resource_id",
                ));
            }
        };

        let handle = api
            .create_com_logical_link(h_mod, descriptor, None, creation_flags)
            .map_err(map_runtime_error)?;

        debug!(
            module_handle = h_mod.0,
            cll_handle = handle.0,
            "CreateComLogicalLink"
        );
        Ok(Response::new(ComLogicalLinkResponse {
            cll_handle: Some(to_proto_cll_handle(h_mod, handle)),
        }))
    }

    pub(super) async fn rpc_destroy_com_logical_link(
        &self,
        request: Request<DestroyComLogicalLinkRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = require_cll_message(request.cll_handle, "cll_handle")?;

        // `self.api` is acquired manually here (rather than via
        // `with_api_for_link`) and held across both the native teardown
        // call and the `cop_tags` sweep below -- not released in between
        // -- per ADR-205 Decision item 2, mirroring
        // `rpc_start_com_primitive`'s own established shape. This repo's
        // ISO mock always reuses the same numeric CLL handle after
        // destroy, so without this, a concurrent
        // `CreateComLogicalLink`+`StartComPrimitive` landing in the gap
        // between releasing `self.api` and running the sweep could insert
        // a legitimate new tag under this same `(module, cll)` prefix,
        // which the stale sweep would then incorrectly erase. Holding
        // `self.api` throughout closes the gap structurally: no successor
        // CLL/COP can be minted (every such path acquires `self.api`
        // first) until this block releases it, by which point the sweep
        // has already completed. `terminate_subscription` is unaffected
        // by this ADR -- it stays after release, per ADR-205's accepted
        // residual on `rpc_subscribe_event`.
        {
            let api_guard = self.api.lock().await;
            let api = api_guard
                .as_ref()
                .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;
            api.destroy_com_logical_link(h_mod, h_cll)
                .map_err(|error| map_runtime_error_for_link(api, h_mod, h_cll, error))?;
            self.terminate_cop_tags_for_link(h_mod, h_cll).await;
        }
        self.terminate_subscription(h_mod, h_cll).await;

        debug!(
            module_handle = h_mod.0,
            cll_handle = h_cll.0,
            "DestroyComLogicalLink"
        );
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_connect_com_logical_link(
        &self,
        request: Request<ConnectComLogicalLinkRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = require_cll_message(request.cll_handle, "cll_handle")?;
        self.with_api_for_link(h_mod, h_cll, |api| api.connect_com_logical_link(h_mod, h_cll))
            .await
            .inspect_err(|err| warn!(module_handle = h_mod.0, cll_handle = h_cll.0, %err, "ConnectComLogicalLink failed"))?;
        info!(
            module_handle = h_mod.0,
            cll_handle = h_cll.0,
            "ConnectComLogicalLink"
        );
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_disconnect_com_logical_link(
        &self,
        request: Request<DisconnectComLogicalLinkRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = require_cll_message(request.cll_handle, "cll_handle")?;
        self.with_api_for_link(h_mod, h_cll, |api| {
            api.disconnect_com_logical_link(h_mod, h_cll)
        })
        .await?;
        info!(
            module_handle = h_mod.0,
            cll_handle = h_cll.0,
            "DisconnectComLogicalLink"
        );
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_lock_resource(
        &self,
        request: Request<LockResourceRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = require_cll_message(request.cll_handle, "cll_handle")?;
        self.with_api_for_link(h_mod, h_cll, |api| {
            api.lock_resource(h_mod, h_cll, request.lock_mask)
        })
        .await?;
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_unlock_resource(
        &self,
        request: Request<UnlockResourceRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = require_cll_message(request.cll_handle, "cll_handle")?;
        self.with_api_for_link(h_mod, h_cll, |api| {
            api.unlock_resource(h_mod, h_cll, request.lock_mask)
        })
        .await?;
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_get_com_param(
        &self,
        request: Request<GetComParamRequest>,
    ) -> Result<Response<ComParamResponse>, Status> {
        use vci_service_interface::get_com_param_request::Param;
        let request = request.into_inner();
        let (h_mod, h_cll) = require_cll_message(request.cll_handle, "cll_handle")?;
        let api = self.api.lock().await;
        let api = api
            .as_ref()
            .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;
        let param_id = match request
            .param
            .ok_or_else(|| Status::invalid_argument("param is required"))?
        {
            Param::ParamId(id) => ObjectId(id),
            Param::ParamName(name) => api
                .get_object_id(ObjectType(E_PDU_OBJT::PDU_OBJT_COMPARAM), &name)
                .map_err(|e| map_runtime_error_for_link(api, h_mod, h_cll, e))?,
        };
        let param = api
            .get_com_param(h_mod, h_cll, param_id)
            .map_err(|e| map_runtime_error_for_link(api, h_mod, h_cll, e))?;

        let borrowed = param.borrowed();
        let mut resolve_vendor_entry_size =
            |struct_type| self.resolve_vendor_struct_entry_size(struct_type);
        let param_item = borrowed_param_to_item(borrowed, &mut resolve_vendor_entry_size)?;

        Ok(Response::new(ComParamResponse {
            param_item: Some(param_item),
        }))
    }

    pub(super) async fn rpc_set_com_param(
        &self,
        request: Request<SetComParamRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = require_cll_message(request.cll_handle, "cll_handle")?;
        let mut param_item = request
            .param_item
            .ok_or_else(|| Status::invalid_argument("param_item is required"))?;

        let api = self.api.lock().await;
        let api = api
            .as_ref()
            .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;

        let param_id = resolve_param_item_id(param_item.id.clone(), |name| {
            api.get_object_id(ObjectType(E_PDU_OBJT::PDU_OBJT_COMPARAM), name)
                .map_err(|e| map_runtime_error_for_link(api, h_mod, h_cll, e))
        })?;

        // The real D-PDU API expects a concrete class -- `to_iso_param`'s
        // class mapping below unconditionally rejects PDU_PC_SPECIFIED, so
        // there is no "let the sentinel through" fallback to fall back to.
        // When the caller leaves it unspecified, fill it in from the
        // param's current class via GetComParam before calling SetComParam;
        // if that Get itself fails, surface its real error (e.g. the param
        // was never readable on this CLL) instead of letting the call fail
        // later with `to_iso_param`'s generic, misleading rejection message.
        if param_item.com_param_class == PduParamClass::PduPcSpecified as i32 {
            let current = api
                .get_com_param(h_mod, h_cll, param_id)
                .map_err(|e| map_runtime_error_for_link(api, h_mod, h_cll, e))?;
            param_item.com_param_class = current.borrowed().class().0 as i32;
        }

        // ADR-218 Decision item 4 (as amended): a non-empty vendor
        // STRUCTFIELD write's declared `size_of_entry` is validated against
        // this same per-library operator config `GetComParam`/
        // `GetUniqueRespIdTable` consult on read -- there is no
        // process-lifetime write cache to populate anymore.
        let mut resolve_vendor_entry_size =
            |struct_type| self.resolve_vendor_struct_entry_size(struct_type);
        let param = to_iso_param(param_item, param_id, &mut resolve_vendor_entry_size)?;
        api.set_com_param(h_mod, h_cll, param)
            .map_err(|e| map_runtime_error_for_link(api, h_mod, h_cll, e))?;

        Ok(Self::empty_response())
    }
}

// Relies on the debug-only runtime VCI_CONFIG_PATH override (see ADR-073),
// so the whole module is debug-only, matching `rpc.rs`'s own test module.
#[cfg(all(test, debug_assertions))]
mod destroy_cop_tag_race_tests {
    use super::*;
    use crate::config::StartupConfig;
    use serial_test::serial;
    use vci_service_interface::{ComOperationType, ResourceData, StartComPrimitiveRequest};
    use vci_service_launcher::vci_server::VciServer;

    /// Duplicates `rpc.rs::tests::set_mock_library_path_config` (private to
    /// that module) rather than sharing it -- matches this repo's own
    /// established precedent of each test surface owning its copy of this
    /// setup (see `tests/grpc_mock.rs::TestServer::start`'s doc comment).
    fn set_mock_library_path_config() {
        let lib_path = iso22900_mock::mock_library_path()
            .expect("mock cdylib should be discoverable after build");
        let config_path = std::env::temp_dir().join(format!(
            "iso22900-service-rpc-link-destroy-race-test-config-{}.toml",
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

    fn default_resource_data() -> ResourceData {
        ResourceData {
            dlc_pin_data: vec![],
            bus_type: Some(vci_service_interface::resource_data::BusType::BusTypeId(1)),
            protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
                2,
            )),
        }
    }

    async fn create_default_cll(
        service: &Iso22900Service,
    ) -> vci_service_interface::ComLogicalLinkHandle {
        service
            .rpc_create_com_logical_link(Request::new(CreateComLogicalLinkRequest {
                module_handle: Some(vci_service_interface::ModuleHandle {
                    module_handle: 1001,
                }),
                resource: Some(CreateComLogicalLinkResource::RscData(
                    default_resource_data(),
                )),
                cll_create_flag: None,
            }))
            .await
            .expect("create_com_logical_link should succeed")
            .into_inner()
            .cll_handle
            .expect("cll_handle should be present")
    }

    /// ADR-205 Decision item 2 regression: `rpc_destroy_com_logical_link`'s
    /// `cop_tags` sweep must run before `self.api` is released, closing the
    /// handle-reuse race where a concurrent
    /// `CreateComLogicalLink`+`StartComPrimitive` (reusing this repo's
    /// mock's constant CLL handle) inserts a legitimate new tag under the
    /// same `(module, cll)` prefix that the stale sweep would otherwise
    /// erase.
    ///
    /// **Deterministic, not timing-dependent.** `self.subscriptions` is
    /// fenced (held by this test) before the destroy task is even spawned.
    /// Both the pre-fix and post-fix code eventually call
    /// `terminate_subscription`, but at different points relative to the
    /// `cop_tags` sweep: pre-fix, `terminate_subscription` runs BEFORE the
    /// sweep, so the destroy task parks on this fence with the sweep still
    /// pending; post-fix, the sweep runs inside the same `self.api` guard
    /// as the native call, strictly BEFORE `terminate_subscription`, so the
    /// destroy task only parks on this fence once the sweep has already
    /// completed. Performing the concurrent create+start once the destroy
    /// task is confirmed parked therefore lands the successor's insert
    /// before the sweep only in the pre-fix code -- exactly the bug this
    /// test targets. In both cases `self.api` is already released by the
    /// time the task parks here (the native call and, post-fix, the sweep
    /// both complete in an inner block that ends before
    /// `terminate_subscription` is ever reached), so the concurrent
    /// create+start below never contends with the parked task for `self.api`
    /// itself.
    ///
    /// Fail-without/pass-with, verified directly: temporarily reverting
    /// `rpc_destroy_com_logical_link` to acquire `self.api` via
    /// `with_api_for_link` and run the `cop_tags` sweep afterward (the
    /// pre-fix shape) makes this test fail (the successor's tag is
    /// missing, evicted by the stale sweep); restoring the fix makes it
    /// pass.
    #[tokio::test]
    #[serial]
    async fn destroy_com_logical_link_sweep_does_not_erase_a_racing_successors_cop_tag() {
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

        let original_cll_handle = create_default_cll(&service).await;

        // Fence: hold `subscriptions` before the destroy task is even
        // spawned/polled, forcing its `terminate_subscription` call to park
        // as the first (only) waiter.
        let fence = service.subscriptions.lock().await;

        let destroy_service = service.clone();
        let destroy_request_handle = original_cll_handle;
        let destroy_task = tokio::spawn(async move {
            destroy_service
                .rpc_destroy_com_logical_link(Request::new(DestroyComLogicalLinkRequest {
                    cll_handle: Some(destroy_request_handle),
                }))
                .await
                .expect("destroy_com_logical_link should succeed")
        });

        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !destroy_task.is_finished(),
            "the destroy task must be parked waiting for `subscriptions` before it can finish"
        );

        // While parked: mint a successor CLL (the mock always returns the
        // same numeric handle after destroy) and start a tagged COP on it --
        // exactly the concurrent request sequence ADR-205 describes.
        let successor_cll_handle = create_default_cll(&service).await;
        assert_eq!(
            successor_cll_handle.cll_handle, original_cll_handle.cll_handle,
            "the mock is expected to reuse the same numeric CLL handle"
        );

        let tag = b"successor-tag".to_vec();
        let successor_cop_handle = service
            .rpc_start_com_primitive(Request::new(StartComPrimitiveRequest {
                cll_handle: Some(successor_cll_handle),
                cop_type: ComOperationType::CoptSendrecv as i32,
                cop_data: vec![0x12, 0x34],
                cop_ctrl_data: None,
                cop_tag: Some(tag.clone()),
            }))
            .await
            .expect("start_com_primitive should succeed")
            .into_inner()
            .cop_handle
            .expect("cop_handle should be present");

        // Release the fence: the destroy task's `terminate_subscription`
        // proceeds; pre-fix, its still-pending `cop_tags` sweep then runs
        // and (bug) erases the successor's just-inserted tag.
        drop(fence);
        destroy_task.await.expect("destroy task must not panic");

        let key = (
            successor_cop_handle.module_handle,
            successor_cop_handle.cll_handle,
            successor_cop_handle.cop_handle,
        );
        let cop_tags = service.cop_tags.lock().await;
        assert_eq!(
            cop_tags.get(&key),
            Some(&tag),
            "the successor COP's tag must survive the destroyed predecessor's cop_tags sweep"
        );
    }
}
