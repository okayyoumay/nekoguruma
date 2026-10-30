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
    CancelComPrimitiveRequest, ComLogicalLinkResponse, ComPrimitiveResponse,
    ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, DestroyComLogicalLinkRequest,
    DisconnectComLogicalLinkRequest, EventNotification, GetModuleIdsRequest, GetObjectIdRequest,
    GetResourceIdsRequest, GetTimestampRequest, GetVersionRequest, IoCtlRequest, IoCtlResponse,
    ModuleConnectRequest, ModuleDisconnectRequest, ModuleHandle, ModuleIdsResponse,
    ObjectIdResponse, ResourceIdsResponse, Response, SetComParamRequest,
    SetUniqueRespIdTableRequest, StartComPrimitiveRequest, SubscribeEventRequest,
    TimestampResponse, VersionResponse,
};
use worker_host::client::WorkerClient;

/// Where a job gets its worker client: for the link it opens first, and for a link it opens
/// again after the path to the VCI was lost (design 8.2.5, ADR-393).
///
/// Every [`WorkerApi`] client is a source of itself: it hands out clones, so the same worker
/// carries the new link, and it says nothing of the worker's process ([`WorkerSource::status`]
/// is `Unknown`), so a worker that exited is not told from a VCI that was lost. A source that
/// owns the worker's process (`launch::RelaunchingWorker`, ADR-394) tells whether it has exited,
/// can end it, and starts a new worker when the current one is gone; the run entry points take
/// either.
pub trait WorkerSource: Send + 'static {
    /// The client the source gives.
    type Client: WorkerApi;

    /// The client for the next link. A source that owns the worker's process starts a new
    /// worker when the current one has exited. An error is a worker that cannot be reached
    /// (yet), or, as [`crate::host::HostError::WorkerStart`] says, one that cannot be started.
    fn client(&mut self) -> Result<Self::Client, crate::host::HostError>;

    /// How the worker's process stands now (ADR-394). `Unknown` when the source does not own it.
    /// A worker that exited stays `Exited` until [`WorkerSource::client`] starts a new one.
    fn status(&mut self) -> WorkerStatus {
        WorkerStatus::Unknown
    }

    /// Ends a worker that is alive and does not answer, and reaps it (ADR-394): `Ok` means no
    /// process of it is left holding the VCI; `Err` when that cannot be said, or when the source
    /// does not own the process.
    fn end(&mut self) -> Result<WorkerExit, crate::host::HostError> {
        Err(crate::host::HostError::Setup(
            "this source does not own the worker's process",
        ))
    }

    /// The process ID of the worker the source gives now, when it knows it.
    fn pid(&self) -> Option<u32> {
        None
    }
}

impl<C: WorkerApi> WorkerSource for C {
    type Client = C;

    fn client(&mut self) -> Result<C, crate::host::HostError> {
        Ok(self.clone())
    }
}

/// How a worker's process stands (ADR-394).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatus {
    /// The process runs.
    Alive,
    /// The process has exited and was reaped.
    Exited(WorkerExit),
    /// The source cannot say.
    Unknown,
}

/// A worker's end, as the communication log's `worker_exit` records it (design 8.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerExit {
    /// The process ID the worker had; `None` when the source did not know it.
    pub pid: Option<u32>,
    pub how: ExitHow,
}

/// How a worker's process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitHow {
    /// It exited with this code.
    Code(i32),
    /// The system ended it with this signal.
    Signal(i32),
    /// The agent ended it (ADR-394): it did not answer.
    KilledByAgent,
    /// The system gave neither a code nor a signal.
    Unknown,
}

impl ExitHow {
    /// The end `status` reports.
    pub fn of(status: std::process::ExitStatus) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Self::Signal(signal);
            }
        }
        status.code().map_or(Self::Unknown, Self::Code)
    }
}

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
    get_timestamp(GetTimestampRequest) -> TimestampResponse;
    get_resource_ids(GetResourceIdsRequest) -> ResourceIdsResponse;
    get_object_id(GetObjectIdRequest) -> ObjectIdResponse;
    create_com_logical_link(CreateComLogicalLinkRequest) -> ComLogicalLinkResponse;
    destroy_com_logical_link(DestroyComLogicalLinkRequest) -> Response;
    connect_com_logical_link(ConnectComLogicalLinkRequest) -> Response;
    disconnect_com_logical_link(DisconnectComLogicalLinkRequest) -> Response;
    set_com_param(SetComParamRequest) -> Response;
    set_unique_resp_id_table(SetUniqueRespIdTableRequest) -> Response;
    start_com_primitive(StartComPrimitiveRequest) -> ComPrimitiveResponse;
    cancel_com_primitive(CancelComPrimitiveRequest) -> Response;
    subscribe_event(SubscribeEventRequest) -> Streaming<EventNotification>;
    io_ctl(IoCtlRequest) -> IoCtlResponse;
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::*;

    #[test]
    fn an_exit_status_is_a_code_or_a_signal() {
        // The raw wait status: the code in the second byte, the signal in the low bits.
        assert_eq!(ExitHow::of(ExitStatus::from_raw(3 << 8)), ExitHow::Code(3));
        assert_eq!(ExitHow::of(ExitStatus::from_raw(0)), ExitHow::Code(0));
        assert_eq!(ExitHow::of(ExitStatus::from_raw(9)), ExitHow::Signal(9));
    }
}
