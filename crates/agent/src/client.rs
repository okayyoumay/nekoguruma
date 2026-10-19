//! The worker client the runner talks to, as a trait (design 13.5 "Guarded VCI access",
//! ADR-334).
//!
//! The runner, the host and the link set-up call the worker through [`WorkerApi`] instead of the
//! concrete gRPC client, so the hardware harness can give a job a client that passes every call
//! through its guard (`ngr_hw_checks::guard::GuardedClient`). The production path uses
//! [`worker_host::client::WorkerClient`], which implements the trait by delegating to the
//! generated gRPC client: nothing about a call changes.
//!
//! The signatures are those of the generated client, so a call keeps its `tonic` result and a
//! status the worker returns reaches the runner's classification unchanged. A client that holds
//! the worker's module itself says so with [`WorkerApi::held_module`]: the link set-up then
//! neither connects nor disconnects it.

use std::future::Future;

use tonic::Streaming;
use vci_service_interface::vci_service_client::VciServiceClient;
use vci_service_interface::{
    ComLogicalLinkResponse, ComPrimitiveResponse, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest,
    EventNotification, GetModuleIdsRequest, GetObjectIdRequest, GetResourceIdsRequest,
    GetVersionRequest, IoCtlRequest, IoCtlResponse, ModuleConnectRequest, ModuleDisconnectRequest,
    ModuleHandle, ModuleIdsResponse, ObjectIdResponse, ResourceIdsResponse, Response,
    SetComParamRequest, SetUniqueRespIdTableRequest, StartComPrimitiveRequest,
    SubscribeEventRequest, VersionResponse,
};
use worker_host::client::WorkerClient;

/// What the runner's calls to one RPC return.
pub type Reply<T> = Result<tonic::Response<T>, tonic::Status>;

macro_rules! worker_api {
    ($($(#[$doc:meta])* $name:ident($request:ty) -> $response:ty;)*) => {
        /// The RPCs of `service VciService` the runner, the host and the link set-up use, with
        /// the generated client's signatures.
        pub trait WorkerApi: Clone + Send + 'static {
            $(
                $(#[$doc])*
                fn $name(
                    &mut self,
                    request: $request,
                ) -> impl Future<Output = Reply<$response>> + Send;
            )*

            /// The module this client holds for the job, if the caller owns it: the link set-up
            /// then uses it without `GetModuleIds` and `ModuleConnect`, never disconnects it,
            /// and leaves closing it to its owner. `None` for a client that does not hold one
            /// (every production client): the job opens and closes the worker's only module.
            fn held_module(&self) -> Option<ModuleHandle> {
                None
            }
        }

        impl WorkerApi for WorkerClient {
            $(
                fn $name(
                    &mut self,
                    request: $request,
                ) -> impl Future<Output = Reply<$response>> + Send {
                    VciServiceClient::$name(self, request)
                }
            )*
        }
    };
}

worker_api! {
    get_module_ids(GetModuleIdsRequest) -> ModuleIdsResponse;
    module_connect(ModuleConnectRequest) -> Response;
    module_disconnect(ModuleDisconnectRequest) -> Response;
    get_version(GetVersionRequest) -> VersionResponse;
    get_resource_ids(GetResourceIdsRequest) -> ResourceIdsResponse;
    get_object_id(GetObjectIdRequest) -> ObjectIdResponse;
    create_com_logical_link(CreateComLogicalLinkRequest) -> ComLogicalLinkResponse;
    destroy_com_logical_link(DestroyComLogicalLinkRequest) -> Response;
    connect_com_logical_link(ConnectComLogicalLinkRequest) -> Response;
    disconnect_com_logical_link(DisconnectComLogicalLinkRequest) -> Response;
    set_com_param(SetComParamRequest) -> Response;
    set_unique_resp_id_table(SetUniqueRespIdTableRequest) -> Response;
    start_com_primitive(StartComPrimitiveRequest) -> ComPrimitiveResponse;
    subscribe_event(SubscribeEventRequest) -> Streaming<EventNotification>;
    io_ctl(IoCtlRequest) -> IoCtlResponse;
}
