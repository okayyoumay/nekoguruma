use std::borrow::Borrow;

use tonic::{Request, Response, Status};
use tracing::info;

use iso22900::{BorrowedResourceStatusItem, E_PDU_OBJT, ObjectType};
use vci_service_interface::{
    ConflictingResourcesResponse, GetConflictingResourcesRequest, GetModuleIdsRequest,
    GetResourceIdsRequest, GetResourceStatusRequest, GetTimestampRequest, GetVersionRequest,
    ModuleConnectRequest, ModuleData, ModuleDisconnectRequest, ModuleHandle, ModuleIdsResponse,
    ModuleItem, ResourceConflictData, ResourceConflictItem, ResourceIdItem, ResourceIdItemData,
    ResourceIdsResponse, ResourceStatusData, ResourceStatusItem, ResourceStatusResponse,
    Response as VciServiceResponse, TimestampResponse, VersionData, VersionResponse,
    get_conflicting_resources_request::Resource as ConflictingResource,
    module_and_resource_id::Resource as ModuleAndResourceId,
};

use crate::error::{api_not_initialized_status, map_runtime_error};
use crate::service::convert::to_resource_descriptor;
use crate::service::handles::{module_handle, require_module_message, resource_id};
use crate::service::rpc::{API_NOT_INITIALIZED, Iso22900Service};

impl Iso22900Service {
    pub(super) async fn rpc_get_module_ids(
        &self,
        _request: Request<GetModuleIdsRequest>,
    ) -> Result<Response<ModuleIdsResponse>, Status> {
        let api = self.api.lock().await;
        let api = api
            .as_ref()
            .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;
        let modules = api.get_module_ids().map_err(map_runtime_error)?;
        let module_data = modules
            .borrowed()
            .entries()
            .map_err(map_runtime_error)?
            .iter()
            .map(|module| ModuleData {
                module_type_id: module.module_type_id(),
                module_handle: Some(ModuleHandle {
                    module_handle: module.module_handle(),
                }),
                vendor_module_name: module.vendor_module_name().unwrap_or_default().to_owned(),
                vendor_additional_info: module
                    .vendor_additional_info()
                    .unwrap_or_default()
                    .to_owned(),
                module_status: module.module_status().0 as i32,
            })
            .collect();

        Ok(Response::new(ModuleIdsResponse {
            module_id_list: Some(ModuleItem { module_data }),
        }))
    }

    pub(super) async fn rpc_module_connect(
        &self,
        request: Request<ModuleConnectRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        let request = request.into_inner();
        let h_mod = require_module_message(request.module_handle, "module_handle")?;
        self.with_api(|api| api.module_connect(h_mod)).await?;
        info!(module_handle = h_mod.0, "ModuleConnect");
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_module_disconnect(
        &self,
        request: Request<ModuleDisconnectRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        let request = request.into_inner();
        let h_mod = require_module_message(request.module_handle, "module_handle")?;

        // `self.api` is acquired manually here (rather than via
        // `with_api`) and held across both the native teardown call and
        // the `cop_tags` sweep below -- not released in between -- per
        // ADR-205 Decision item 2, mirroring
        // `rpc_destroy_com_logical_link`'s identical restructuring: no
        // successor CLL/COP for this module can be minted (every such
        // path acquires `self.api` first) until this block releases it,
        // by which point the sweep has already completed.
        // `terminate_subscriptions_for_module` is unaffected by this ADR
        // -- it stays after release.
        {
            let api_guard = self.api.lock().await;
            let api = api_guard
                .as_ref()
                .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;
            api.module_disconnect(h_mod).map_err(map_runtime_error)?;
            self.terminate_cop_tags_for_module(h_mod).await;
        }
        self.terminate_subscriptions_for_module(h_mod).await;

        info!(module_handle = h_mod.0, "ModuleDisconnect");
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_get_version(
        &self,
        request: Request<GetVersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        let request = request.into_inner();
        let h_mod = require_module_message(request.module_handle, "module_handle")?;
        let version = self.with_api(|api| api.get_version(h_mod)).await?;

        Ok(Response::new(VersionResponse {
            version_data: Some(VersionData {
                mvci_part1_standard_version: version.mvci_part_1_standard_version,
                mvci_part2_standard_version: version.mvci_part_2_standard_version,
                hw_serial_number: version.hardware_serial_number,
                hw_name: version.hardware_name,
                hw_version: version.hardware_version,
                hw_date: version.hardware_date,
                hw_interface: version.hardware_interface,
                fw_name: version.firmware_name,
                fw_version: version.firmware_version,
                fw_date: version.firmware_date,
                vendor_name: version.vendor_name,
                pdu_api_sw_name: version.api_software_name,
                pdu_api_sw_version: version.api_software_version,
                pdu_api_sw_date: version.api_software_date,
            }),
        }))
    }

    pub(super) async fn rpc_get_timestamp(
        &self,
        request: Request<GetTimestampRequest>,
    ) -> Result<Response<TimestampResponse>, Status> {
        let request = request.into_inner();
        let h_mod = require_module_message(request.module_handle, "module_handle")?;
        let timestamp = self.with_api(|api| api.get_timestamp(h_mod)).await?;
        Ok(Response::new(TimestampResponse { timestamp }))
    }

    pub(super) async fn rpc_get_resource_status(
        &self,
        request: Request<GetResourceStatusRequest>,
    ) -> Result<Response<ResourceStatusResponse>, Status> {
        let request = request.into_inner();
        let api = self.api.lock().await;
        let api = api
            .as_ref()
            .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;
        let mut resources = Vec::with_capacity(request.resources.len());
        for (index, entry) in request.resources.into_iter().enumerate() {
            let module = entry.module_handle.ok_or_else(|| {
                Status::invalid_argument(format!("resources[{index}].module_handle is required"))
            })?;
            let resource = entry.resource.ok_or_else(|| {
                Status::invalid_argument(format!("resources[{index}].resource is required"))
            })?;

            let rsc_id = match resource {
                ModuleAndResourceId::ResourceId(id) => id,
                ModuleAndResourceId::ResourceName(name) => api
                    .get_object_id(ObjectType(E_PDU_OBJT::PDU_OBJT_RESOURCE), name.as_str())
                    .map(|id| id.0)
                    .map_err(map_runtime_error)?,
            };

            resources.push((module_handle(module.module_handle), resource_id(rsc_id)));
        }

        let item = api
            .get_resource_status(resources)
            .map_err(map_runtime_error)?;
        let borrowed: &BorrowedResourceStatusItem = item.borrow();

        Ok(Response::new(ResourceStatusResponse {
            resource_status: Some(ResourceStatusItem {
                resource_status_data: borrowed
                    .entries()
                    .map_err(map_runtime_error)?
                    .iter()
                    .map(|entry| ResourceStatusData {
                        module_handle: Some(ModuleHandle {
                            module_handle: entry.module_handle().0,
                        }),
                        resource_id: entry.resource_id().0,
                        resource_status: entry.resource_status(),
                    })
                    .collect(),
            }),
        }))
    }

    pub(super) async fn rpc_get_resource_ids(
        &self,
        request: Request<GetResourceIdsRequest>,
    ) -> Result<Response<ResourceIdsResponse>, Status> {
        let request = request.into_inner();
        let api = self.api.lock().await;
        let api = api
            .as_ref()
            .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;
        let resource_data = request
            .resource_data
            .ok_or_else(|| Status::invalid_argument("resource_data is required"))?;
        let descriptor = to_resource_descriptor(resource_data, |object_type, name| {
            api.get_object_id(object_type, name)
                .map_err(map_runtime_error)
        })?;
        let items = api
            .get_resource_ids(
                require_module_message(request.module_handle, "module_handle")?,
                &descriptor,
            )
            .map_err(map_runtime_error)?;

        Ok(Response::new(ResourceIdsResponse {
            resource_id_list: Some(ResourceIdItem {
                resource_id_data_array: items
                    .borrowed()
                    .modules()
                    .map_err(map_runtime_error)?
                    .iter()
                    .map(|entry| {
                        Ok(ResourceIdItemData {
                            module_handle: Some(ModuleHandle {
                                module_handle: entry.module_handle(),
                            }),
                            resource_id_array: entry
                                .resource_ids()
                                .map_err(map_runtime_error)?
                                .to_vec(),
                        })
                    })
                    .collect::<Result<Vec<_>, Status>>()?,
            }),
        }))
    }

    pub(super) async fn rpc_get_conflicting_resources(
        &self,
        request: Request<GetConflictingResourcesRequest>,
    ) -> Result<Response<ConflictingResourcesResponse>, Status> {
        let request = request.into_inner();
        let modules = request
            .input_module_list
            .map(|item| {
                item.module_data
                    .into_iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let handle = entry.module_handle.ok_or_else(|| {
                            Status::invalid_argument(format!(
                                "input_module_list.module_data[{index}].module_handle is required"
                            ))
                        })?;
                        Ok(module_handle(handle.module_handle))
                    })
                    .collect::<Result<Vec<_>, Status>>()
            })
            .transpose()?
            .unwrap_or_default();

        // Hold the lock through name resolution and conflict query to avoid TOCTOU.
        let api = self.api.lock().await;
        let api = api
            .as_ref()
            .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;

        let resource_id_value = match request.resource {
            Some(ConflictingResource::ResourceId(id)) => id,
            Some(ConflictingResource::ResourceName(name)) => api
                .get_object_id(ObjectType(E_PDU_OBJT::PDU_OBJT_RESOURCE), name.as_str())
                .map(|id| id.0)
                .map_err(map_runtime_error)?,
            None => {
                return Err(Status::invalid_argument(
                    "resource is required (resource_id or resource_name)",
                ));
            }
        };

        let conflicts = api
            .get_conflicting_resources(resource_id(resource_id_value), &modules)
            .map_err(map_runtime_error)?;

        Ok(Response::new(ConflictingResourcesResponse {
            conflict_list: Some(ResourceConflictItem {
                resource_conflict_data: conflicts
                    .borrowed()
                    .entries()
                    .map_err(map_runtime_error)?
                    .iter()
                    .map(|entry| ResourceConflictData {
                        module_handle: Some(ModuleHandle {
                            module_handle: entry.module_handle().0,
                        }),
                        resource_id: entry.resource_id().0,
                    })
                    .collect(),
            }),
        }))
    }
}

// Relies on the debug-only runtime VCI_CONFIG_PATH override (see ADR-073),
// so the whole module is debug-only, matching `rpc.rs`'s own test module.
#[cfg(all(test, debug_assertions))]
mod module_disconnect_cop_tag_race_tests {
    use super::*;
    use crate::config::StartupConfig;
    use serial_test::serial;
    use vci_service_interface::{
        ComOperationType, CreateComLogicalLinkRequest, ResourceData, StartComPrimitiveRequest,
        create_com_logical_link_request::Resource as CreateComLogicalLinkResource,
    };
    use vci_service_launcher::vci_server::VciServer;

    /// Duplicates `rpc.rs::tests::set_mock_library_path_config` (private to
    /// that module) rather than sharing it -- matches this repo's own
    /// established precedent of each test surface owning its copy of this
    /// setup (see `tests/grpc_mock.rs::TestServer::start`'s doc comment).
    fn set_mock_library_path_config() {
        let lib_path = iso22900_mock::mock_library_path()
            .expect("mock cdylib should be discoverable after build");
        let config_path = std::env::temp_dir().join(format!(
            "iso22900-service-rpc-module-disconnect-race-test-config-{}.toml",
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
                module_handle: Some(ModuleHandle {
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

    /// ADR-205 Decision item 2 regression: `rpc_module_disconnect`'s
    /// `cop_tags` sweep must run before `self.api` is released, mirroring
    /// `rpc_destroy_com_logical_link`'s identical restructuring (see
    /// `rpc_link.rs::destroy_cop_tag_race_tests` for the full mechanism --
    /// duplicated here at module scope rather than CLL scope). Closes the
    /// handle-reuse race where a concurrent
    /// `CreateComLogicalLink`+`StartComPrimitive` for the same module
    /// (reusing this repo's mock's constant handles) inserts a legitimate
    /// new tag that the stale sweep would otherwise erase.
    ///
    /// **Deterministic, not timing-dependent**, via the same
    /// `self.subscriptions` fence and reasoning as
    /// `destroy_com_logical_link_sweep_does_not_erase_a_racing_successors_
    /// cop_tag`.
    ///
    /// Fail-without/pass-with, verified directly: temporarily reverting
    /// `rpc_module_disconnect` to acquire `self.api` via `with_api` and run
    /// the `cop_tags` sweep afterward (the pre-fix shape) makes this test
    /// fail (the successor's tag is missing, evicted by the stale sweep);
    /// restoring the fix makes it pass.
    #[tokio::test]
    #[serial]
    async fn module_disconnect_sweep_does_not_erase_a_racing_successors_cop_tag() {
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

        // Fence: hold `subscriptions` before the disconnect task is even
        // spawned/polled, forcing its `terminate_subscriptions_for_module`
        // call to park as the first (only) waiter.
        let fence = service.subscriptions.lock().await;

        let disconnect_service = service.clone();
        let disconnect_task = tokio::spawn(async move {
            disconnect_service
                .rpc_module_disconnect(Request::new(ModuleDisconnectRequest {
                    module_handle: Some(ModuleHandle {
                        module_handle: 1001,
                    }),
                }))
                .await
                .expect("module_disconnect should succeed")
        });

        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !disconnect_task.is_finished(),
            "the disconnect task must be parked waiting for `subscriptions` before it can finish"
        );

        // While parked: mint a successor CLL for the same module (the mock
        // always returns the same numeric handles) and start a tagged COP
        // on it -- exactly the concurrent request sequence ADR-205
        // describes, at module scope.
        let successor_cll_handle = create_default_cll(&service).await;

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

        // Release the fence: the disconnect task's
        // `terminate_subscriptions_for_module` proceeds; pre-fix, its
        // still-pending `cop_tags` sweep then runs and (bug) erases the
        // successor's just-inserted tag.
        drop(fence);
        disconnect_task
            .await
            .expect("disconnect task must not panic");

        let key = (
            successor_cop_handle.module_handle,
            successor_cop_handle.cll_handle,
            successor_cop_handle.cop_handle,
        );
        let cop_tags = service.cop_tags.lock().await;
        assert_eq!(
            cop_tags.get(&key),
            Some(&tag),
            "the successor COP's tag must survive the disconnected module's cop_tags sweep"
        );
    }
}
