//! The guarded worker client for the agent's job runner (design 13.5 "Guarded VCI access",
//! ADR-334).
//!
//! [`GuardedClient`] implements [`agent::client::WorkerApi`], the client the runner, host and
//! link set-up call, over a [`GuardedHandle`]: every call the job makes goes through the handle,
//! so the guard decides it (and the profile's rules apply to a job as to a direct check). Three
//! things differ from a direct check's handle.
//!
//! - The harness owns the module. [`WorkerApi::held_module`] is the module the device was
//!   opened with, so the job's link set-up neither lists, connects nor disconnects it, and the
//!   device-close step the handle registered disconnects it once. A job that asks for the module
//!   itself is refused.
//! - The job's link set-up is completed from the profile, the one place a call is added to
//!   rather than passed or refused. The runner's set-up gives the baud rate, timing, the two
//!   identifiers and nothing else, while the guard requires every ComParam the profile pins
//!   before a link connects. On `SetUniqueRespIdTable` the profile's remaining table entries
//!   (formats, address bytes) are appended; on `ConnectComLogicalLink` the other pinned ComParams
//!   (address modes, functional identifier, filler, tester-present message, the profile's own)
//!   are set first. Each added request is decided like any other, and the job's own values are
//!   compared with the profile's unchanged, so a mismatch is still refused.
//! - The handle holds no event stream for the job's link (the job subscribes itself, and a worker
//!   keeps one subscription per link); it subscribes again only when it has to read events
//!   itself, such as the return to the default session after the job.
//!
//! A refusal reaches the runner as a `PermissionDenied` status, the lost VCI as the status the
//! worker uses (so the host still reports it), a worker's own status as it was, and a timeout as
//! `DeadlineExceeded`.

use std::sync::Arc;
use std::time::Duration;

use agent::client::{Reply, WorkerApi};
use tokio::sync::Mutex as AsyncMutex;
use tonic::Streaming;
use vci_service_interface::{
    CancelComPrimitiveRequest, ComLogicalLinkResponse, ComPrimitiveResponse,
    ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, DestroyComLogicalLinkRequest,
    DisconnectComLogicalLinkRequest, ErrorDetail, ErrorEventData, EventNotification,
    GetModuleIdsRequest, GetObjectIdRequest, GetResourceIdsRequest, GetTimestampRequest,
    GetVersionRequest, IoCtlRequest, IoCtlResponse, ModuleConnectRequest, ModuleDisconnectRequest,
    ModuleHandle, ModuleIdsResponse, ObjectIdResponse, PduError, PduErrorEvent,
    ResourceIdsResponse, Response, SetComParamRequest, SetUniqueRespIdTableRequest,
    StartComPrimitiveRequest, SubscribeEventRequest, TimestampResponse, VersionResponse,
    status_with_error_detail,
};

use super::{GuardedHandle, HandleError, HandleOptions, Request, Trace};
use crate::open::OpenedDevice;
use crate::profile::{Profile, Vci};
use crate::report::RunGuard;

/// The deadline the runner gives a unary worker call (`Timings::for_link`). The handle's request
/// timeout is at least this, since each worker request of a guarded call is bounded by it.
const RUNNER_UNARY_DEADLINE: Duration = Duration::from_secs(5);

/// The agent runner's worker client, guarded. Cloning shares the handle.
#[derive(Clone)]
pub struct GuardedClient {
    handle: Arc<AsyncMutex<GuardedHandle>>,
    module: ModuleHandle,
    trace: Trace,
}

impl GuardedClient {
    /// Holds the device `open_device` opened and matched with `vci`, for a job. No link is
    /// created; the job creates its own, through the guard.
    pub async fn open(
        profile: &Profile,
        vci: &Vci,
        device: OpenedDevice,
        options: HandleOptions,
        run: &RunGuard,
    ) -> Result<GuardedClient, HandleError> {
        let handle =
            GuardedHandle::open_with(profile, vci, device, bounded(options), run, false).await?;
        Ok(Self::from_handle(handle))
    }

    /// Like [`GuardedClient::open`], for a worker the caller hosts: `client` talks to it and
    /// `module` is the module already connected on it.
    pub async fn attach(
        profile: &Profile,
        vci: &Vci,
        client: worker_host::client::WorkerClient,
        module: ModuleHandle,
        options: HandleOptions,
        run: &RunGuard,
    ) -> Result<GuardedClient, HandleError> {
        let handle =
            GuardedHandle::attach_with(profile, vci, client, module, bounded(options), run, false)
                .await?;
        Ok(Self::from_handle(handle))
    }

    fn from_handle(handle: GuardedHandle) -> Self {
        let trace = handle.enable_trace();
        let module = handle.module_handle();
        Self {
            handle: Arc::new(AsyncMutex::new(handle)),
            module,
            trace,
        }
    }

    /// Every decision of the guard so far, in order: the job's calls, what the client added to
    /// them, and the handle's own teardown requests.
    pub fn trace(&self) -> Trace {
        Arc::clone(&self.trace)
    }
}

/// `options` with a request timeout of at least the runner's unary deadline.
fn bounded(mut options: HandleOptions) -> HandleOptions {
    options.request_timeout = options.request_timeout.max(RUNNER_UNARY_DEADLINE);
    options
}

/// The status the runner sees for a failed guarded call.
fn status_of(error: HandleError) -> tonic::Status {
    use tonic::Code;
    let message = error.to_string();
    match error {
        HandleError::Refused { .. } => tonic::Status::permission_denied(message),
        // The status the workers use for a request on a link taken offline by a lost VCI, which
        // the host turns into its lost-VCI error.
        HandleError::VciLost => status_with_error_detail(
            Code::FailedPrecondition,
            message,
            ErrorDetail {
                pdu_error: PduError::PduErrCllNotConnected as i32,
                error_event_data: Some(ErrorEventData {
                    error_event: PduErrorEvent::PduErrEvtLostCommToVci as i32,
                    cop_handle: None,
                    timestamp: 0,
                    extra_error_info: 0,
                }),
                detail_text: None,
                refusal: None,
            },
        ),
        HandleError::Rpc { code, message, .. }
        | HandleError::WorkerRefused { code, message, .. } => tonic::Status::new(code, message),
        HandleError::Timeout { .. } => tonic::Status::deadline_exceeded(message),
        _ => tonic::Status::internal(message),
    }
}

fn reply<T>(result: Result<T, HandleError>) -> Reply<T> {
    result.map(tonic::Response::new).map_err(status_of)
}

macro_rules! through {
    ($($name:ident($request:ty) -> $response:ty = |$handle:ident, $req:ident| $call:expr;)*) => {$(
        fn $name(
            &mut self,
            request: $request,
        ) -> impl Future<Output = Reply<$response>> + Send {
            let handle = Arc::clone(&self.handle);
            async move {
                let mut $handle = handle.lock().await;
                let $req = request;
                reply($call.await)
            }
        }
    )*};
}

impl WorkerApi for GuardedClient {
    through! {
        get_module_ids(GetModuleIdsRequest) -> ModuleIdsResponse = |h, r| h.get_module_ids(r);
        get_version(GetVersionRequest) -> VersionResponse = |h, r| h.get_version(r);
        get_timestamp(GetTimestampRequest) -> TimestampResponse = |h, r| h.get_timestamp(r);
        get_resource_ids(GetResourceIdsRequest) -> ResourceIdsResponse = |h, r| h.get_resource_ids(r);
        get_object_id(GetObjectIdRequest) -> ObjectIdResponse = |h, r| h.get_object_id(r);
        create_com_logical_link(CreateComLogicalLinkRequest) -> ComLogicalLinkResponse = |h, r| h.create_com_logical_link(r);
        destroy_com_logical_link(DestroyComLogicalLinkRequest) -> Response = |h, r| h.destroy_com_logical_link(r);
        connect_com_logical_link(ConnectComLogicalLinkRequest) -> Response = |h, r| h.connect_completing(r);
        disconnect_com_logical_link(DisconnectComLogicalLinkRequest) -> Response = |h, r| h.disconnect_com_logical_link(r);
        set_com_param(SetComParamRequest) -> Response = |h, r| h.set_com_param(r);
        set_unique_resp_id_table(SetUniqueRespIdTableRequest) -> Response = |h, r| h.set_table_completed(r);
        start_com_primitive(StartComPrimitiveRequest) -> ComPrimitiveResponse = |h, r| h.start_com_primitive(r);
        cancel_com_primitive(CancelComPrimitiveRequest) -> Response = |h, r| h.cancel_com_primitive_settling(r);
        subscribe_event(SubscribeEventRequest) -> Streaming<EventNotification> = |h, r| h.subscribe_event(r);
        io_ctl(IoCtlRequest) -> IoCtlResponse = |h, r| h.io_ctl(r);
    }

    // The harness opened the module and closes it: a job that asks for either is refused (the
    // job does not, with a held module, unless it is another client's idea of the link set-up).
    fn module_connect(
        &mut self,
        request: ModuleConnectRequest,
    ) -> impl Future<Output = Reply<Response>> + Send {
        let handle = Arc::clone(&self.handle);
        async move {
            let handle = handle.lock().await;
            Err(status_of(
                handle.decide_module_request(&Request::ModuleConnect(&request)),
            ))
        }
    }

    fn module_disconnect(
        &mut self,
        request: ModuleDisconnectRequest,
    ) -> impl Future<Output = Reply<Response>> + Send {
        let handle = Arc::clone(&self.handle);
        async move {
            let handle = handle.lock().await;
            Err(status_of(handle.decide_module_request(
                &Request::ModuleDisconnect(&request),
            )))
        }
    }

    fn held_module(&self) -> Option<ModuleHandle> {
        Some(self.module)
    }
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
