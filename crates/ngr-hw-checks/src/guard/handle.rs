//! The guarded VCI handle (design 13.5 "Guarded VCI access" and "Cleanup", ADR-275 item 4,
//! ADR-333): what a hardware check gets of the worker, in place of a worker client.
//!
//! The handle owns the worker client and the state [`decide`] rests on (the [`LinkTable`] and the
//! [`Learned`] ids). It does these things for the check:
//!
//! - Every worker call goes through [`decide`] first. A refusal is returned as
//!   [`HandleError::Refused`] and nothing is sent. The handle exposes a method for each RPC a
//!   check can use, and none that sends without a decision. `ModuleConnect` and `ModuleDisconnect`
//!   are not among them, since the harness owns the device; `SubscribeEvent` and `GetEventItem`
//!   are not either, because the handle subscribes to the events of each link itself and reads
//!   them in [`GuardedHandle::send_receive`]: a second reader would take events away from it.
//! - The answers of `GetResourceIds` and `GetObjectId` are recorded into [`Learned`], so a later
//!   request that names a resource or ComParam by id can be compared with the profile.
//! - The `cll_handle` of a `CreateComLogicalLink` that was granted as [`Grant::CreatesLink`] is
//!   bound to its profile endpoint, the ComParams of a [`Grant::SetsComParams`] are recorded once
//!   the worker accepted them, and a [`Grant::DestroysLink`] drops the link.
//! - A [`Grant::EntersSession`] registers, before the request is sent, a teardown step that sends
//!   DiagnosticSessionControl to the default session on that link (once per link).
//! - The device, each link and each session are released by teardown steps registered with the
//!   run's [`RunGuard`] as they are acquired, so they run in reverse when the check ends, panics
//!   included.
//!
//! The handle creates the profile's links itself ([`GuardedHandle::open`] and
//! [`GuardedHandle::attach`]): per worker endpoint of the profile, `GetResourceIds` and
//! `CreateComLogicalLink` (RawMode on T0 only), the ComParams `link::required` lists with the
//! profile's values (those a response table holds in `SetUniqueRespIdTable` with one entry, the
//! others in `SetComParam`), the same timing and response-pending defaults the agent's link sets
//! (unless the profile pins them), `ConnectComLogicalLink` and a subscription to the link's
//! events. Every one of these requests is itself decided, like a check's.
//!
//! The handle also serves the agent's job runner (`GuardedClient`, ADR-334): `open_with` and
//! `attach_with` can hold the device without creating links, subscribe only when the handle reads
//! events itself, and `set_table_completed` and `connect_completing` add the profile's own
//! ComParams to the job's link set-up.
//!
//! What the handle does not do: switching power on or off. A profile's power commands belong to
//! a check that uses them, which must register the power-off step before it switches power on
//! (design 13.5 "Cleanup"); no step is registered for them here.
//!
//! Teardown steps drive their requests on a runtime of their own (see [`crate::teardown`]) while
//! the thread that runs them waits, so the worker's connection must be served by other threads:
//! a check that uses the handle runs on a multi-thread tokio runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;
use tonic::Streaming;
use vci_service_interface::{
    CancelComPrimitiveRequest, CllCreateFlag, CllCreateFlagBit, ComLogicalLinkHandle,
    ComLogicalLinkResponse, ComOperationType, ComParamResponse, ComPrimitiveCtrlData,
    ComPrimitiveHandle, ComPrimitiveResponse, ConflictingResourcesResponse,
    ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, DestroyComLogicalLinkRequest,
    DisconnectComLogicalLinkRequest, EcuUniqueRespData, EventItem, EventNotification,
    ExpectedResponseData, GetComParamRequest, GetConflictingResourcesRequest, GetModuleIdsRequest,
    GetObjectIdRequest, GetResourceIdsRequest, GetResourceStatusRequest, GetStatusRequest,
    GetTimestampRequest, GetUniqueRespIdTableRequest, GetVersionRequest, IoCtlRequest,
    IoCtlResponse, LockResourceRequest, ModuleDisconnectRequest, ModuleHandle, ModuleIdsResponse,
    ObjectIdResponse, ObjectType, ParamItem, PduComPrimitiveStatus, PduError, PduErrorEvent,
    PduParamClass, PinData, ResourceData, ResourceIdsResponse, ResourceStatusResponse,
    SetComParamRequest, SetUniqueRespIdTableRequest, StartComPrimitiveRequest, StatusResponse,
    SubscribeEventRequest, TimestampResponse, TxFlag, TxFlagBit, UniqueRespIdTableItem,
    UniqueRespIdTableResponse, UnlockResourceRequest, VersionResponse, com_primitive_ctrl_data,
    create_com_logical_link_request, event_item, event_notification, io_ctl_request, param_item,
    pin_data, resource_data, subscribe_event_request,
};
use worker_host::client::WorkerClient;
use worker_host::service::Stopped;

use super::link::{BUS_TYPE, PIN_TYPES, required, table_required};
use super::{BoundLink, Context, Grant, GuardRefusal, Learned, LinkTable, Request, decide};
use crate::comparam::canonical;
use crate::open::OpenedDevice;
use crate::profile::{Addressing, Class, Link, Profile, Vci};
use crate::report::RunGuard;
use crate::teardown::{TeardownClosed, TeardownHandle};

/// What `GetObjectId` answers for a name the worker does not know (`PDU_ID_UNDEF`, ISO 22900-2
/// (2022) 8.4.23).
const PDU_ID_UNDEF: u32 = 0xFFFF_FFFE;

/// The timing and response-pending defaults of a link on T1 to T3, as the agent's link sets them
/// (`crates/agent/src/link.rs`): P2 1 s, P2* 5 s, the 0x78 handling on with the response code at
/// byte 2, and a 25 s limit for a whole 0x78 chain. Times are in microseconds. A profile that
/// pins one of them keeps its own value.
const DEFAULTS: [(&str, u32); 5] = [
    ("CP_P2Max", 1_000_000),
    ("CP_P2Star", 5_000_000),
    ("CP_RC78Handling", 1),
    ("CP_RCByteOffset", 2),
    ("CP_RC78CompletionTimeout", 25_000_000),
];

/// The spelling the workers and ISO 22900-2 (2022) Annex B give the ComParams the handle sets for
/// the profile's typed fields, by canonical name. A `[link.comparams]` entry is set under the
/// key the profile wrote.
const SPELLINGS: [(&str, &str); 15] = [
    ("cp_baudrate", "CP_Baudrate"),
    ("cp_canphysreqid", "CP_CanPhysReqId"),
    ("cp_canrespusdtid", "CP_CanRespUSDTId"),
    ("cp_canfuncreqid", "CP_CanFuncReqId"),
    ("cp_requestaddrmode", "CP_RequestAddrMode"),
    ("cp_testerpresentaddrmode", "CP_TesterPresentAddrMode"),
    ("cp_canphysreqformat", "CP_CanPhysReqFormat"),
    ("cp_canrespusdtformat", "CP_CanRespUSDTFormat"),
    ("cp_canfuncreqformat", "CP_CanFuncReqFormat"),
    ("cp_canphysreqextaddr", "CP_CanPhysReqExtAddr"),
    ("cp_canrespusdtextaddr", "CP_CanRespUSDTExtAddr"),
    ("cp_canfuncreqextaddr", "CP_CanFuncReqExtAddr"),
    ("cp_canfillerbytehandling", "CP_CanFillerByteHandling"),
    ("cp_canfillerbyte", "CP_CanFillerByte"),
    ("cp_testerpresentmessage", "CP_TesterPresentMessage"),
];

/// The tester-present request the handle sets as the link's message: TesterPresent with the
/// suppress-response bit set (ISO 14229-1 (2026) 9.8, the sub-function is zero).
const TESTER_PRESENT: [u8; 2] = [0x3E, 0x80];

/// Why a handle call or the opening of the handle failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandleError {
    /// The guard refused the request; nothing was sent to the worker.
    #[error("{rpc} refused by the guard before it reached the worker: {refusal}")]
    Refused {
        rpc: &'static str,
        refusal: GuardRefusal,
    },
    /// A worker request failed in a way that does not say whether the worker applied it: a
    /// transport failure, or a status the worker does not use to reject a request. `call` names
    /// it.
    #[error("{call} failed: {message}")]
    Rpc {
        call: &'static str,
        code: tonic::Code,
        message: String,
    },
    /// The worker rejected the request, with a status it uses for that (see
    /// [`is_definite_refusal`]): it did not apply it.
    #[error("{call} rejected by the worker: {message}")]
    WorkerRefused {
        call: &'static str,
        code: tonic::Code,
        message: String,
    },
    /// A worker request got no answer within the request timeout.
    #[error("{call} got no answer within {timeout:?}")]
    Timeout {
        call: &'static str,
        timeout: Duration,
    },
    /// The link or the worker did not behave as the handle needs.
    #[error("{0}")]
    Setup(String),
    /// The primitive ended without a response to the request.
    #[error("the primitive ended without a response")]
    NoResponse,
    /// The VCI was lost: the worker reported the loss of communication with it.
    #[error("the VCI was lost")]
    VciLost,
    /// The run had already ended its teardown, so the step that releases a resource could not
    /// be registered.
    #[error(transparent)]
    TeardownClosed(#[from] TeardownClosed),
    /// The link's receive-only monitor ended (a nonzero `CP_CyclicRespTimeout` can end it, or it
    /// was cancelled), so nothing more arrives on the link.
    #[error("the link's monitor ended; nothing more arrives on the link")]
    MonitorEnded,
    /// A raw frame was not transmitted: the worker reported a transmit error, cancelled the send,
    /// or did not confirm it in time.
    #[error("the frame was not sent: {0}")]
    SendFailed(String),
    /// The monitor's buffer is full of frames nobody asked for.
    #[error("the link's monitor received more than {cap} unrelated frames")]
    RawBufferFull { cap: usize },
    /// The primitive failed with this D-PDU error event code.
    #[error("the primitive failed with error event {0:#x}")]
    PrimitiveFailed(i32),
    /// The worker reported lost events, so the end of the primitive cannot be known.
    #[error("the worker lost events of the link")]
    EventsLost,
    /// The link's event stream ended.
    #[error("the link's event stream ended")]
    EventStreamEnded,
}

/// The timeouts of a handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandleOptions {
    /// How long a unary worker request may take.
    pub request_timeout: Duration,
    /// How long [`GuardedHandle::send_receive`] waits for the end of a primitive. `None` (the
    /// default) is the link's own ceiling: the P2, P2* and 0x78 completion timeout the link is
    /// set to (the profile's when it pins them) plus a margin, so an ECU that keeps answering
    /// response-pending within its completion timeout is waited for. When the wait ends, the
    /// primitive is cancelled before the call returns.
    pub response_timeout: Option<Duration>,
}

impl Default for HandleOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(5),
            response_timeout: None,
        }
    }
}

/// One decision of the guard: the RPC and what it was told.
pub type Decision = (&'static str, Result<Grant, GuardRefusal>);

/// The record of the guard's decisions (see [`GuardedHandle::enable_trace`]).
pub type Trace = Arc<Mutex<Vec<Decision>>>;

type Events = Arc<AsyncMutex<Streaming<EventNotification>>>;
/// How the device is closed: the step, and the time it may take.
struct DeviceClose {
    step: Step,
    budget: Duration,
    /// Create the profile's links (not when the caller creates its own).
    links: bool,
}

type Step = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// What the decisions rest on, and what the teardown steps still have to do.
#[derive(Default)]
struct State {
    links: LinkTable,
    learned: Learned,
    /// The event stream of each link, by `cll_handle`.
    events: BTreeMap<u32, Events>,
    /// Links whose disconnect and destroy step has not run and that no one destroyed.
    closing: BTreeSet<u32>,
    /// Links whose return to the default session has been registered and has not run.
    session_back: BTreeSet<u32>,
    /// The response timings of each link, which decide how long a send-receive may take.
    timings: BTreeMap<u32, LinkTimings>,
    /// Links whose return-to-default-session step is registered and has not started.
    session_registered: BTreeSet<u32>,
    /// The primitive being waited for on each link, while a wait is under way. A wait whose
    /// future is dropped leaves its entry, and the next operation or the teardown cancels it.
    inflight: BTreeMap<u32, ComPrimitiveHandle>,
    /// The receive-only primitive listening on each link (`raw_listen`).
    monitors: BTreeMap<u32, ComPrimitiveHandle>,
    /// Frames a link's monitor received that no `raw_receive` has taken yet.
    raw_frames: BTreeMap<u32, std::collections::VecDeque<Vec<u8>>>,
    /// The primitives started on each link through `start_com_primitive` that the handle has
    /// not seen end: cancelled before the link's session is settled, it is disconnected or
    /// destroyed, and by the teardown (cancelling one that ended succeeds and changes nothing).
    started: BTreeMap<u32, Vec<ComPrimitiveHandle>>,
    /// The requests that resume the transmit queues a check suspended.
    suspended: Vec<IoCtlRequest>,
    /// The step that resumes them is registered and has not started.
    resume_registered: bool,
    /// Every decision of the guard, when a check or the harness asked for the record.
    trace: Option<Trace>,
}

/// The part of the handle the teardown steps share.
struct Shared {
    profile: Profile,
    vci: Vci,
    module: u32,
    options: HandleOptions,
    /// Subscribe to the events of a link the handle creates: not when something else (the
    /// agent's job) subscribes, since a worker keeps one subscription per link.
    subscribes: bool,
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is changed in short steps that cannot panic midway, so a poisoned lock holds
        // consistent data.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The guard's decision on `request`, over the current state, recorded in the trace.
    fn admit(&self, request: &Request<'_>) -> Result<Grant, HandleError> {
        self.decide_request(request, true)
    }

    /// The decision on `request` without a record: a first look, before steps that must only
    /// run for a request the guard will allow, which is then decided (and recorded) when sent.
    fn check(&self, request: &Request<'_>) -> Result<Grant, HandleError> {
        self.decide_request(request, false)
    }

    fn decide_request(&self, request: &Request<'_>, record: bool) -> Result<Grant, HandleError> {
        let state = self.lock();
        let ctx = Context {
            profile: &self.profile,
            module: self.module,
            vci: &self.vci,
            links: &state.links,
            learned: &state.learned,
        };
        let decision = decide(&ctx, request);
        // A first look records only a refusal: the request is not decided again when it is
        // refused, and a granted one is recorded when it is sent.
        if let (true, Some(trace)) = (record || decision.is_err(), &state.trace) {
            trace
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((request.rpc(), decision.clone()));
        }
        decision.map_err(|refusal| HandleError::Refused {
            rpc: request.rpc(),
            refusal,
        })
    }

    /// Awaits a worker request within the request timeout.
    async fn send<T>(
        &self,
        call: &'static str,
        request: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    ) -> Result<T, HandleError> {
        let timeout = self.options.request_timeout;
        match tokio::time::timeout(timeout, request).await {
            Err(_) => Err(HandleError::Timeout { call, timeout }),
            // A request refused on a link the worker took offline after a lost VCI is the lost
            // VCI: the worker attaches the link's last error event to the status.
            Ok(Err(status))
                if status.code() == tonic::Code::FailedPrecondition
                    && vci_service_interface::error_detail_from_status(&status)
                        .and_then(|detail| detail.error_event_data)
                        .is_some_and(|data| {
                            data.error_event == PduErrorEvent::PduErrEvtLostCommToVci as i32
                        }) =>
            {
                Err(HandleError::VciLost)
            }
            Ok(Err(status)) => Err(worker_error(call, &status)),
            Ok(Ok(response)) => Ok(response.into_inner()),
        }
    }

    /// Keeps the pending return to the default session in step with the session the ECU is
    /// observed to be in: a positive answer to DiagnosticSessionControl to the default session
    /// clears it, one to another session sets it (the step itself is registered when the
    /// request is granted).
    fn observe_session(&self, cll: ComLogicalLinkHandle, request: &[u8], response: &[u8]) {
        if let ([0x10, ..], [0x50, session, ..]) = (request, response) {
            let mut state = self.lock();
            if session & 0x7F == 0x01 {
                state.session_back.remove(&cll.cll_handle);
            } else if state.links.contains_key(&cll.cll_handle) {
                state.session_back.insert(cll.cll_handle);
            }
        }
    }

    /// The staged timings of `cll` are the ones in effect.
    fn promote_timings(&self, cll: ComLogicalLinkHandle) {
        if let Some(timings) = self.lock().timings.get_mut(&cll.cll_handle) {
            timings.active = timings.staged;
        }
    }

    /// Applies what a granted request changes, after the worker accepted it.
    fn apply(&self, grant: &Grant) {
        let mut state = self.lock();
        match grant {
            Grant::SetsComParams { cll, names } => {
                if let Some(link) = state.links.get_mut(cll) {
                    link.set.extend(names.iter().cloned());
                }
            }
            Grant::DestroysLink(cll) => {
                state.links.remove(cll);
                state.events.remove(cll);
                state.closing.remove(cll);
                state.session_back.remove(cll);
                state.timings.remove(cll);
                state.session_registered.remove(cll);
                state.inflight.remove(cll);
                state.monitors.remove(cll);
                state.raw_frames.remove(cll);
                state.started.remove(cll);
                state.suspended.retain(|saved| {
                    !matches!(&saved.handle, Some(io_ctl_request::Handle::CllHandle(c)) if c.cll_handle == *cll)
                });
            }
            Grant::Pass | Grant::CreatesLink { .. } | Grant::EntersSession(_) => {}
        }
    }

    /// Decides and sends a `StartComPrimitive`, and returns its grant and primitive handle.
    async fn start(
        &self,
        client: &mut WorkerClient,
        request: StartComPrimitiveRequest,
    ) -> Result<(Grant, ComPrimitiveHandle), HandleError> {
        let grant = self.admit(&Request::StartComPrimitive(&request))?;
        let response = self
            .send("StartComPrimitive", client.start_com_primitive(request))
            .await?;
        let cop = response.cop_handle.ok_or_else(|| {
            HandleError::Setup("StartComPrimitive returned no primitive handle".to_owned())
        })?;
        Ok((grant, cop))
    }

    /// How long a send-receive on `cll` waits now: the option, else the link's ceiling.
    fn wait_for(&self, cll: ComLogicalLinkHandle) -> Duration {
        self.options.response_timeout.unwrap_or_else(|| {
            self.lock()
                .timings
                .get(&cll.cll_handle)
                .map_or_else(|| Timings::default().ceiling(), LinkTimings::ceiling)
        })
    }

    /// Waits for the end of primitive `cop` on link `cll` and returns the response to `request`.
    async fn await_result(
        &self,
        client: &mut WorkerClient,
        cll: ComLogicalLinkHandle,
        cop: ComPrimitiveHandle,
        request: &[u8],
        cap: Option<Duration>,
    ) -> Result<Vec<u8>, HandleError> {
        self.await_primitive(client, cll, cop, Some(Expectation::of(request)), cap)
            .await
    }

    /// Cancels primitive `cop`, through the guard.
    async fn cancel(
        &self,
        client: &mut WorkerClient,
        cop: ComPrimitiveHandle,
    ) -> Result<(), HandleError> {
        let cancel = CancelComPrimitiveRequest {
            cop_handle: Some(cop),
        };
        self.admit(&Request::CancelComPrimitive(&cancel))?;
        self.send("CancelComPrimitive", client.cancel_com_primitive(cancel))
            .await
            .map(drop)
    }

    /// Waits for the end of primitive `cop`: its response to `expected`, or with `None` (a
    /// primitive that has no response, such as an update of ComParams) its successful finish,
    /// which returns no bytes.
    ///
    /// Every way out while the primitive may still be running, whatever the reason (the wait
    /// ran out, events were lost, the event stream failed or ended, no stream), goes through
    /// the one cancel here: [`Shared::read_outcome`] says what happened, and this cancels unless
    /// the primitive reached a terminal status. The worker answers a cancel of a primitive that
    /// ended with success, so a cancel that fails is the error returned: the primitive may still
    /// be running.
    async fn await_primitive(
        &self,
        client: &mut WorkerClient,
        cll: ComLogicalLinkHandle,
        cop: ComPrimitiveHandle,
        expected: Option<Expectation>,
        cap: Option<Duration>,
    ) -> Result<Vec<u8>, HandleError> {
        self.lock().inflight.insert(cll.cll_handle, cop);
        let subscribed = self.ensure_events(client, cll).await;
        let outcome = match subscribed {
            Ok(()) => self.read_outcome(cll, cop, expected, cap).await,
            Err(error) => Outcome::Running(error),
        };
        let result = match outcome {
            Outcome::Ended(result) => result,
            Outcome::Running(error) => {
                // A cancel that fails leaves the primitive recorded as running.
                self.cancel(client, cop).await?;
                Err(error)
            }
        };
        let mut state = self.lock();
        if state.inflight.get(&cll.cll_handle) == Some(&cop) {
            state.inflight.remove(&cll.cll_handle);
        }
        if let Some(started) = state.started.get_mut(&cll.cll_handle) {
            started.retain(|started| *started != cop);
        }
        drop(state);
        result
    }

    /// Subscribes to the events of `cll` through the guard when the handle holds no stream for
    /// it (the agent's job subscribed itself while the handle did not, and ended; or the link
    /// was created by a caller that reads events itself).
    async fn ensure_events(
        &self,
        client: &mut WorkerClient,
        cll: ComLogicalLinkHandle,
    ) -> Result<(), HandleError> {
        if self.lock().events.contains_key(&cll.cll_handle) {
            return Ok(());
        }
        let request = SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll)),
        };
        self.admit(&Request::SubscribeEvent(&request))?;
        let stream = self
            .send("SubscribeEvent", client.subscribe_event(request))
            .await?;
        self.lock()
            .events
            .insert(cll.cll_handle, Arc::new(AsyncMutex::new(stream)));
        Ok(())
    }

    /// Cancels the primitive a dropped wait left running on `cll`, if there is one.
    async fn cancel_stale(
        &self,
        client: &mut WorkerClient,
        cll: ComLogicalLinkHandle,
    ) -> Result<(), HandleError> {
        let stale = self.lock().inflight.get(&cll.cll_handle).copied();
        if let Some(cop) = stale {
            self.cancel(client, cop).await?;
            self.lock().inflight.remove(&cll.cll_handle);
        }
        Ok(())
    }

    /// Cancels a primitive the handle never saw end, which may well have ended. The J2534
    /// worker answers a cancel of an ended primitive with success; a worker that names the
    /// handle as invalid in the error detail (`PDU_ERR_INVALID_HANDLE`, as `j2534-0404-service`
    /// does for a forgotten one and `iso22900-service` passes on from the vendor library) has
    /// nothing left to cancel either, and that is success too. Any other failure is returned.
    async fn cancel_maybe_ended(
        &self,
        client: &mut WorkerClient,
        cop: ComPrimitiveHandle,
    ) -> Result<(), HandleError> {
        let cancel = CancelComPrimitiveRequest {
            cop_handle: Some(cop),
        };
        self.admit(&Request::CancelComPrimitive(&cancel))?;
        let timeout = self.options.request_timeout;
        match tokio::time::timeout(timeout, client.cancel_com_primitive(cancel)).await {
            Err(_) => Err(HandleError::Timeout {
                call: "CancelComPrimitive",
                timeout,
            }),
            Ok(Ok(_)) => Ok(()),
            Ok(Err(status)) if names_invalid_handle(&status) => Ok(()),
            Ok(Err(status)) => Err(worker_error("CancelComPrimitive", &status)),
        }
    }

    /// Cancels everything outstanding on `cll`: the primitive a dropped wait left running and
    /// every primitive started through the handle that it has not seen end. Tries all, and
    /// returns the first failure.
    async fn cancel_outstanding(
        &self,
        client: &mut WorkerClient,
        cll: ComLogicalLinkHandle,
    ) -> Result<(), HandleError> {
        let mut first = self.cancel_stale(client, cll).await.err();
        let started = self
            .lock()
            .started
            .remove(&cll.cll_handle)
            .unwrap_or_default();
        for (index, cop) in started.iter().enumerate() {
            if let Err(error) = self.cancel_maybe_ended(client, *cop).await {
                // Still outstanding: this one and the rest not yet tried.
                self.lock()
                    .started
                    .entry(cll.cll_handle)
                    .or_default()
                    .extend(started[index..].iter().copied());
                first.get_or_insert(error);
                break;
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Resumes the transmit queue a check suspended on `cll` itself, before the link stops being
    /// usable: the guard refuses an IO control on a link that was destroyed, so a resume left for
    /// the teardown would fail there.
    async fn resume_link(
        &self,
        client: &mut WorkerClient,
        cll: ComLogicalLinkHandle,
    ) -> Result<(), HandleError> {
        let on_link = |saved: &IoCtlRequest| matches!(&saved.handle, Some(io_ctl_request::Handle::CllHandle(c)) if c.cll_handle == cll.cll_handle);
        loop {
            let found = {
                let mut state = self.lock();
                let at = state.suspended.iter().position(on_link);
                at.map(|at| state.suspended.remove(at))
            };
            let Some(request) = found else {
                return Ok(());
            };
            let sent = async {
                self.admit(&Request::IoCtl(&request))?;
                self.send("IoCtl", client.io_ctl(request.clone())).await
            }
            .await;
            if let Err(error) = sent {
                self.lock().suspended.push(request);
                return Err(error);
            }
        }
    }

    /// How many requests the steps that end a link may send before their own: a cancel for each
    /// outstanding primitive, and a resume for each suspended queue.
    fn outstanding_requests(&self, cll: ComLogicalLinkHandle) -> usize {
        let state = self.lock();
        usize::from(state.inflight.contains_key(&cll.cll_handle))
            + state.started.get(&cll.cll_handle).map_or(0, Vec::len)
            + state.suspended.len()
    }

    /// Resumes the transmit queues a check suspended, each through the guard. A suspended
    /// queue would hold back the return to the default session.
    async fn resume_queues(&self, client: &mut WorkerClient) -> Result<(), HandleError> {
        loop {
            let Some(request) = self.lock().suspended.pop() else {
                return Ok(());
            };
            let sent = async {
                self.admit(&Request::IoCtl(&request))?;
                self.send("IoCtl", client.io_ctl(request.clone())).await
            }
            .await;
            if let Err(error) = sent {
                self.lock().suspended.push(request);
                return Err(error);
            }
        }
    }

    /// Reads the link's events until primitive `cop` ends or the wait does. Sends nothing.
    async fn read_outcome(
        &self,
        cll: ComLogicalLinkHandle,
        cop: ComPrimitiveHandle,
        expected: Option<Expectation>,
        cap: Option<Duration>,
    ) -> Outcome {
        let Some(events) = self.lock().events.get(&cll.cll_handle).cloned() else {
            return Outcome::Running(HandleError::Setup(
                "the link has no event stream".to_owned(),
            ));
        };
        let timeout = self.wait_for(cll);
        let timeout = cap.map_or(timeout, |cap| timeout.min(cap));
        let mut progress = Progress::default();
        let now = tokio::time::Instant::now();
        // A timeout too long to add is, in effect, none.
        let deadline = now
            .checked_add(timeout)
            .unwrap_or_else(|| now + Duration::from_secs(86_400 * 365));
        let mut stream = events.lock().await;
        loop {
            let notification = match tokio::time::timeout_at(deadline, stream.message()).await {
                Err(_) => return Outcome::Running(HandleError::NoResponse),
                Ok(Err(status)) => {
                    return Outcome::Running(HandleError::Rpc {
                        call: "SubscribeEvent",
                        code: status.code(),
                        message: status.to_string(),
                    });
                }
                Ok(Ok(None)) => return Outcome::Running(HandleError::EventStreamEnded),
                Ok(Ok(Some(notification))) => notification,
            };
            let item = match notification.event_data {
                Some(event_notification::EventData::Item(item)) => item,
                // The end of the primitive may be among the lost events.
                Some(event_notification::EventData::Lost(_)) => {
                    return Outcome::Running(HandleError::EventsLost);
                }
                None => continue,
            };
            if let Some(outcome) = progress.on_item(item, cop, expected.as_ref()) {
                return outcome;
            }
        }
    }
}

/// What the events of one primitive have shown so far.
#[derive(Default)]
struct Progress {
    response: Option<Vec<u8>>,
    error: Option<i32>,
}

impl Progress {
    /// Takes one event; returns the outcome once it ends the wait. Mirrors the agent host's
    /// classification (`crates/agent/src/host.rs`).
    fn on_item(
        &mut self,
        item: EventItem,
        cop: ComPrimitiveHandle,
        expected: Option<&Expectation>,
    ) -> Option<Outcome> {
        // A lost VCI ends the primitive at once. The worker reports it for the whole link, with
        // no primitive handle, before it cancels the primitives; one bound to this primitive
        // counts the same. It comes before the primitive filter below, which would drop it.
        if item.cop_handle.is_none_or(|handle| handle == cop)
            && matches!(
                item.data,
                Some(event_item::Data::ErrorData(code))
                    if code == PduErrorEvent::PduErrEvtLostCommToVci as i32
            )
        {
            return Some(Outcome::Ended(Err(HandleError::VciLost)));
        }
        // Events of earlier primitives, and of none, are not this request's.
        if item.cop_handle != Some(cop) {
            return None;
        }
        match item.data {
            Some(event_item::Data::ResultData(result))
                if expected.is_some_and(|expected| expected.answers(&result.data_bytes)) =>
            {
                self.response = Some(result.data_bytes);
            }
            // The ECU did not answer: reported as no response when the primitive ends.
            Some(event_item::Data::ErrorData(code))
                if code == PduErrorEvent::PduErrEvtRxTimeout as i32 => {}
            Some(event_item::Data::ErrorData(code)) => self.error = Some(code),
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
                    || status == PduComPrimitiveStatus::PduCopstCancelled as i32 =>
            {
                let finished = status == PduComPrimitiveStatus::PduCopstFinished as i32;
                return Some(Outcome::Ended(match (self.response.take(), self.error) {
                    (Some(response), _) => Ok(response),
                    (None, None) if expected.is_none() && finished => Ok(Vec::new()),
                    (None, Some(code)) => Err(HandleError::PrimitiveFailed(code)),
                    (None, None) => Err(HandleError::NoResponse),
                }));
            }
            _ => {}
        }
        None
    }
}

/// How waiting for a primitive ended.
enum Outcome {
    /// The primitive reached a terminal status; this is its result.
    Ended(Result<Vec<u8>, HandleError>),
    /// The wait ended with the primitive possibly still running, for this reason.
    Running(HandleError),
}

/// What `raw_wait` waits for.
enum RawWait<'a> {
    /// A frame on the monitor, equal to this.
    Frame(&'a [u8]),
    /// The end of this send.
    SendEnd(ComPrimitiveHandle),
}

/// How a response to a UDS request begins, so the worker ends the primitive on an answer to this
/// request only.
struct Expectation {
    sid: u8,
    positive: Vec<u8>,
}

impl Expectation {
    fn of(request: &[u8]) -> Self {
        let sid = request.first().copied().unwrap_or(0);
        // The sub-function is echoed without its suppress-response bit, a DID as requested (ISO
        // 14229-1 (2026) 9.4, 10.2 and 11.3).
        let positive = match request {
            // A DID is matched only for a single-DID read: the ECU may return several in any order,
            // so a read of more matches the service only (as the agent's matcher does).
            [0x22, hi, lo] => vec![0x62, *hi, *lo],
            [0x19 | 0x3E | 0x10, sub, ..] => vec![sid.wrapping_add(0x40), sub & 0x7F],
            [sid, ..] => vec![sid.wrapping_add(0x40)],
            [] => Vec::new(),
        };
        Self { sid, positive }
    }

    /// Whether `response` is a final answer: a positive response that begins as expected, or a
    /// negative response to the request's service that is not a response-pending (0x78).
    fn answers(&self, response: &[u8]) -> bool {
        match response {
            [0x7F, sid, code, ..] => *sid == self.sid && *code != 0x78,
            _ => response.starts_with(&self.positive),
        }
    }

    /// The expected-response descriptors: the positive prefix and the negative response.
    fn descriptors(&self) -> Vec<ExpectedResponseData> {
        let expected = |response_type, pattern: Vec<u8>| ExpectedResponseData {
            response_type,
            acceptance_id: 0,
            mask_data: vec![0xFF; pattern.len()],
            pattern_data: pattern,
            unique_resp_ids: vec![],
        };
        vec![
            expected(0, self.positive.clone()),
            expected(1, vec![0x7F, self.sid]),
        ]
    }
}

fn send_receive_request(cll: ComLogicalLinkHandle, data: &[u8]) -> StartComPrimitiveRequest {
    StartComPrimitiveRequest {
        cop_tag: None,
        cll_handle: Some(cll),
        cop_type: ComOperationType::CoptSendrecv as i32,
        cop_data: data.to_vec(),
        cop_ctrl_data: Some(ComPrimitiveCtrlData {
            time: 0,
            num_send_cycles: 1,
            num_receive_cycles: 1,
            temp_param_update: 0,
            expected_response_array: Expectation::of(data).descriptors(),
            tx_flag: None,
        }),
    }
}

/// The time the step that closes the device is given: what closing may take (`budget`: the
/// module disconnect's request timeout, and for a worker process its stop grace period) and a
/// margin; never less than the teardown's default.
fn device_close_timeout(budget: Duration) -> Duration {
    budget
        .saturating_add(Duration::from_secs(1))
        .max(crate::teardown::DEFAULT_STEP_TIMEOUT)
}

/// The time the step that closes a link is given: its disconnect and destroy requests and one
/// request for each primitive to cancel and queue to resume before them (`before`), each bounded by
/// `request_timeout`, and a margin; never less than the teardown's default.
fn close_link_timeout(request_timeout: Duration, before: usize) -> Duration {
    let requests = u32::try_from(before.saturating_add(3)).unwrap_or(u32::MAX);
    request_timeout
        .saturating_mul(requests)
        .saturating_add(Duration::from_secs(1))
        .max(crate::teardown::DEFAULT_STEP_TIMEOUT)
}

/// The teardown step of a link: disconnects and destroys it, unless the check destroyed it.
async fn close_link(
    shared: Arc<Shared>,
    mut client: WorkerClient,
    cll: ComLogicalLinkHandle,
) -> Result<(), String> {
    if !shared.lock().closing.remove(&cll.cll_handle) {
        return Ok(());
    }
    // A primitive a dropped wait left running is stopped first; the destroy below ends any that
    // the cancel does not reach.
    let _ = shared.cancel_outstanding(&mut client, cll).await;
    let disconnect = DisconnectComLogicalLinkRequest {
        cll_handle: Some(cll),
    };
    // A link that was never connected refuses the disconnect; destroying it still works.
    let disconnected = async {
        shared.admit(&Request::DisconnectComLogicalLink(&disconnect))?;
        shared
            .send(
                "DisconnectComLogicalLink",
                client.disconnect_com_logical_link(disconnect),
            )
            .await
    }
    .await;
    let destroy = DestroyComLogicalLinkRequest {
        cll_handle: Some(cll),
    };
    let destroyed = async {
        let grant = shared.admit(&Request::DestroyComLogicalLink(&destroy))?;
        shared
            .send(
                "DestroyComLogicalLink",
                client.destroy_com_logical_link(destroy),
            )
            .await?;
        shared.apply(&grant);
        Ok::<_, HandleError>(())
    }
    .await;
    match (disconnected, destroyed) {
        (_, Ok(())) => Ok(()),
        (Ok(_), Err(error)) => Err(error.to_string()),
        (Err(first), Err(error)) => Err(format!("{first}; {error}")),
    }
}

/// The time the step that returns a link to the default session is given: the wait for the
/// response (`wait`, which the step never exceeds and after which it cancels the primitive), the
/// start request and the cancel request, a request for each outstanding primitive and suspended
/// queue (`before`), and a margin; never less than the teardown's default.
fn session_reset_timeout(wait: Duration, request_timeout: Duration, before: usize) -> Duration {
    // The start and cancel requests of the return itself, and one request for each primitive to
    // cancel and queue to resume first.
    let requests = u32::try_from(before.saturating_add(2)).unwrap_or(u32::MAX);
    wait.saturating_add(request_timeout.saturating_mul(requests))
        .saturating_add(Duration::from_secs(1))
        .max(crate::teardown::DEFAULT_STEP_TIMEOUT)
}

/// The teardown step that returns the ECU on `cll` to the default session, once.
async fn return_to_default_session(
    shared: Arc<Shared>,
    mut client: WorkerClient,
    cll: ComLogicalLinkHandle,
) -> Result<(), String> {
    {
        let mut state = shared.lock();
        state.session_registered.remove(&cll.cll_handle);
        if !state.session_back.remove(&cll.cll_handle) || !state.links.contains_key(&cll.cll_handle)
        {
            return Ok(());
        }
    }
    // Still to be done unless this succeeds, however it ends, a dropped future included: a later
    // run of the step (the teardown's, after a refused or abandoned destroy) tries again.
    let mut pending = PendingSession {
        shared: &shared,
        cll,
        done: false,
    };
    // What would hold the return back goes first: a primitive a dropped wait left running, a
    // transmit queue a check suspended.
    shared
        .cancel_outstanding(&mut client, cll)
        .await
        .map_err(|e| e.to_string())?;
    shared
        .resume_queues(&mut client)
        .await
        .map_err(|e| e.to_string())?;
    // The events of the link are read from before the request goes out; a job that read them
    // itself may have left the handle with no stream.
    shared
        .ensure_events(&mut client, cll)
        .await
        .map_err(|e| e.to_string())?;
    // The link's response wait as it is now, active and staged timings alike.
    let wait = shared.wait_for(cll);
    // DiagnosticSessionControl to the default session (ISO 14229-1 (2026) 9.2).
    let request = [0x10, 0x01];
    let run = async {
        let (_, cop) = shared
            .start(&mut client, send_receive_request(cll, &request))
            .await?;
        shared
            .await_result(&mut client, cll, cop, &request, Some(wait))
            .await
    };
    // Only the positive response to the request ends the session: a negative response, or any
    // other answer, leaves the ECU where it was.
    let result = run.await.map_err(|e| e.to_string()).and_then(|response| {
        match response.as_slice() {
            [0x50, 0x01, ..] => Ok(()),
            other => Err(format!(
                "the ECU did not accept the return to the default session: it answered {other:02X?}"
            )),
        }
    });
    pending.done = result.is_ok();
    result
}

/// Whether a status is one the workers return for a request they reject without applying it:
/// `InvalidArgument` (a malformed or unsupported value), `FailedPrecondition` (the wrong state,
/// such as a link that is not connected), `NotFound` (an unknown handle), `ResourceExhausted`,
/// `Unimplemented` and `AlreadyExists`, which `j2534-0404-service` and `iso22900-service` return
/// before they act, and `Unauthenticated`, which the listener returns before the request reaches
/// the service. Every other status is ambiguous: `Internal` is a library error that can come
/// after part of the work was done, `Cancelled` can be the worker cancelling work or the
/// transport dropping the call, and `Unavailable`, `Unknown`, `DeadlineExceeded` and the
/// transport's own errors say nothing about the worker's state.
fn is_definite_refusal(code: tonic::Code) -> bool {
    use tonic::Code::{
        AlreadyExists, FailedPrecondition, InvalidArgument, NotFound, ResourceExhausted,
        Unauthenticated, Unimplemented,
    };
    matches!(
        code,
        InvalidArgument
            | FailedPrecondition
            | NotFound
            | ResourceExhausted
            | Unimplemented
            | AlreadyExists
            | Unauthenticated
    )
}

/// How many frames a link's monitor keeps for a later `raw_receive`.
const RAW_BUFFER_CAP: usize = 1024;

/// How long `raw_send` waits for the worker to confirm the transmission of a frame.
const RAW_SEND_BOUND: Duration = Duration::from_secs(2);

/// Whether a received frame is an echo of the link's own transmission, by its RxFlag: a loopback
/// copy (`TX_MSG_TYPE`, bit 0 of the last byte) or a TX indication (bit 3), as SAE J2534-1
/// (v04.04) 8.7.2 defines the RxStatus bits the worker copies.
fn is_echo(rx_flag: &[u8]) -> bool {
    const TX_MSG_TYPE: u8 = 0x01;
    const TX_INDICATION: u8 = 0x08;
    rx_flag
        .last()
        .is_some_and(|byte| byte & (TX_MSG_TYPE | TX_INDICATION) != 0)
}

/// Whether a status says, in its error detail, that a handle is invalid (unknown, or forgotten
/// after the primitive ended).
fn names_invalid_handle(status: &tonic::Status) -> bool {
    vci_service_interface::error_detail_from_status(status)
        .is_some_and(|detail| detail.pdu_error == PduError::PduErrInvalidHandle as i32)
}

/// The error of a failed worker call, by whether the worker rejected the request.
fn worker_error(call: &'static str, status: &tonic::Status) -> HandleError {
    let (code, message) = (status.code(), status.to_string());
    if is_definite_refusal(code) {
        HandleError::WorkerRefused {
            call,
            code,
            message,
        }
    } else {
        HandleError::Rpc {
            call,
            code,
            message,
        }
    }
}

/// Whether an error says the worker did not apply the request, so state kept in case it did can
/// be dropped: only a rejection by the worker, or the lost VCI it reports for an offline link.
/// A transport failure, a timeout and every other status leave it unknown.
fn undoes_request(error: &HandleError) -> bool {
    matches!(
        error,
        HandleError::WorkerRefused { .. } | HandleError::VciLost
    )
}

/// Marks the return to the default session of a link pending while a session change is on its
/// way to the worker, and keeps the mark unless the worker refused the change, also when the
/// future that sent it is dropped (the worker may already hold the request). A link that was
/// already pending stays so whatever happens.
struct ProvisionalSession<'a> {
    shared: &'a Shared,
    cll: ComLogicalLinkHandle,
    /// The mark was not there before, so a refusal may take it away.
    added: bool,
    refused: bool,
}

impl<'a> ProvisionalSession<'a> {
    fn new(shared: &'a Shared, cll: ComLogicalLinkHandle) -> Self {
        let added = shared.lock().session_back.insert(cll.cll_handle);
        Self {
            shared,
            cll,
            added,
            refused: false,
        }
    }
}

impl Drop for ProvisionalSession<'_> {
    fn drop(&mut self) {
        if self.added && self.refused {
            self.shared.lock().session_back.remove(&self.cll.cll_handle);
        }
    }
}

/// Puts a link's pending return to the default session back unless the return succeeded.
struct PendingSession<'a> {
    shared: &'a Shared,
    cll: ComLogicalLinkHandle,
    done: bool,
}

impl Drop for PendingSession<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.shared.lock().session_back.insert(self.cll.cll_handle);
        }
    }
}

/// The value of a ComParam the handle sets.
enum Value {
    Number(u32),
    Signed(i32),
    Bytes(Vec<u8>),
}

impl Value {
    fn item(&self, id: u32, class: PduParamClass) -> ParamItem {
        ParamItem {
            id: Some(param_item::Id::ParamId(id)),
            com_param_class: class as i32,
            param_data: Some(match self {
                Value::Number(v) => param_item::ParamData::Unum32(*v),
                Value::Signed(v) => param_item::ParamData::Snum32(*v),
                Value::Bytes(v) => param_item::ParamData::Bytefield(v.clone()),
            }),
        }
    }
}

/// The CAN ID format value of the profile's link (ISO 22900-2 (2022) Tables B.13 and B.14): the
/// segmented transport, the identifier width and the addressing scheme. The padding bits are
/// left to the filler handling ComParam.
fn format_value(link: &Link) -> u32 {
    let mut format = 0x04;
    if link.id_bits == 29 {
        format |= 0x02;
    }
    match link.addressing {
        Addressing::Normal => {}
        Addressing::Extended => format |= 0x08,
        Addressing::Mixed => format |= 0x08 | 0x40,
    }
    format
}

/// The profile's value of the canonical ComParam `name`.
fn value_of(name: &str, link: &Link) -> Result<Value, HandleError> {
    let functional = || {
        link.functional_id
            .ok_or_else(|| HandleError::Setup(format!("{name} needs a functional identifier")))
    };
    Ok(Value::Number(match name {
        "cp_baudrate" => link.baud_rate,
        "cp_canphysreqid" => link.request_id,
        "cp_canrespusdtid" => link.response_id,
        "cp_canfuncreqid" => functional()?,
        // Requests go to the ECU's physical address (1); so does tester present (0), whatever
        // the worker's preset (functional).
        "cp_requestaddrmode" => 1,
        "cp_testerpresentaddrmode" => 0,
        "cp_canphysreqformat" | "cp_canfuncreqformat" | "cp_canrespusdtformat" => {
            format_value(link)
        }
        "cp_canphysreqextaddr" | "cp_canfuncreqextaddr" | "cp_canrespusdtextaddr" => {
            u32::from(link.address.unwrap_or(0))
        }
        "cp_canfillerbytehandling" => u32::from(link.padding.is_some()),
        "cp_canfillerbyte" => u32::from(link.padding.unwrap_or(0)),
        "cp_testerpresentmessage" => return Ok(Value::Bytes(TESTER_PRESENT.to_vec())),
        other => {
            let (_, &value) = link
                .comparams
                .iter()
                .find(|(key, _)| canonical(key) == other)
                .ok_or_else(|| HandleError::Setup(format!("no profile value for {other}")))?;
            return match (u32::try_from(value), i32::try_from(value)) {
                (Ok(v), _) => Ok(Value::Number(v)),
                (_, Ok(v)) => Ok(Value::Signed(v)),
                _ => Err(HandleError::Setup(format!(
                    "the profile's value of {other} does not fit 32 bits"
                ))),
            };
        }
    }))
}

/// The name the worker is asked for: the spelling of a typed ComParam, else the profile's own
/// key.
fn spelling(name: &str, link: &Link) -> String {
    SPELLINGS
        .iter()
        .find(|(canonical, _)| *canonical == name)
        .map(|(_, spelled)| (*spelled).to_owned())
        .or_else(|| {
            link.comparams
                .keys()
                .find(|key| canonical(key) == name)
                .cloned()
        })
        .unwrap_or_else(|| name.to_owned())
}

/// The time ceiling of a send-receive with no finite 0x78 completion timeout: the worker never
/// ends such a primitive for taking too long (`CP_RC78CompletionTimeout` zero means no ceiling),
/// so the handle bounds the wait itself, long enough that no real response-pending chain
/// reaches it and short enough that a hung ECU cannot hold a hardware run forever. The
/// primitive is cancelled when it passes.
const UNBOUNDED_COMPLETION_BOUND: Duration = Duration::from_secs(600);

/// The three ComParams that decide how long a primitive runs, in microseconds as set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Timings {
    p2_max: u32,
    p2_star: u32,
    rc78_completion: u32,
    /// The worker re-requests after a busy (0x21) or not-ready (0x23) answer, each within its own
    /// completion timeout, when its handling is on (`CP_RC21Handling`, `CP_RC23Handling`). The
    /// handle sets none of them, and the guard refuses a check that does, so these keep the
    /// worker's preset (off) unless the guard's list is widened.
    rc21_handling: u32,
    rc21_completion: u32,
    rc23_handling: u32,
    rc23_completion: u32,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            p2_max: 1_000_000,
            p2_star: 5_000_000,
            rc78_completion: 25_000_000,
            rc21_handling: 0,
            rc21_completion: 0,
            rc23_handling: 0,
            rc23_completion: 0,
        }
    }
}

impl Timings {
    /// What the plan sets (a value it lacks is the handle's default).
    fn of(own: &[(String, Value, PduParamClass)]) -> Self {
        let get = |name: &str, default: u32| {
            own.iter()
                .find_map(|(n, value, _)| match value {
                    Value::Number(v) if canonical(n) == canonical(name) => Some(*v),
                    _ => None,
                })
                .unwrap_or(default)
        };
        let default = Timings::default();
        Timings {
            p2_max: get("CP_P2Max", default.p2_max),
            p2_star: get("CP_P2Star", default.p2_star),
            rc78_completion: get("CP_RC78CompletionTimeout", default.rc78_completion),
            rc21_handling: get("CP_RC21Handling", 0),
            rc21_completion: get("CP_RC21CompletionTimeout", 0),
            rc23_handling: get("CP_RC23Handling", 0),
            rc23_completion: get("CP_RC23CompletionTimeout", 0),
        }
    }

    /// Sets the timing the canonical ComParam `name` stands for (see [`TIMING_COMPARAMS`]).
    fn set(&mut self, name: &str, value: u32) {
        match name {
            "cp_p2max" => self.p2_max = value,
            "cp_p2star" => self.p2_star = value,
            "cp_rc78completiontimeout" => self.rc78_completion = value,
            "cp_rc21handling" => self.rc21_handling = value,
            "cp_rc21completiontimeout" => self.rc21_completion = value,
            "cp_rc23handling" => self.rc23_handling = value,
            "cp_rc23completiontimeout" => self.rc23_completion = value,
            _ => {}
        }
    }

    /// How long a send-receive may take: P2, the 0x78 completion timeout and one more P2* (the
    /// agent's `send_recv_ceiling`), and a margin for the worker and the transmission. A zero
    /// is read as `j2534-0404-service` reads it: P2 zero is its 50 ms, P2* zero its 5 s, and
    /// a zero completion timeout is no ceiling, for which the wait is
    /// [`UNBOUNDED_COMPLETION_BOUND`].
    fn ceiling(&self) -> Duration {
        let micros = |v: u32, zero: Duration| {
            if v == 0 {
                zero
            } else {
                Duration::from_micros(u64::from(v))
            }
        };
        if self.rc78_completion == 0 {
            // No ceiling in the worker: the bound is the whole wait, whatever P2 and P2* are.
            return UNBOUNDED_COMPLETION_BOUND;
        }
        micros(self.p2_max, Duration::from_millis(50))
            .saturating_add(micros(self.p2_star, Duration::from_secs(5)))
            .saturating_add(micros(self.rc78_completion, Duration::ZERO))
            // A re-request chain keeps its own ceiling, anchored at its first occurrence; a zero
            // completion timeout is the worker's 5 s.
            .saturating_add(if self.rc21_handling != 0 {
                micros(self.rc21_completion, Duration::from_secs(5))
            } else {
                Duration::ZERO
            })
            .saturating_add(if self.rc23_handling != 0 {
                micros(self.rc23_completion, Duration::from_secs(5))
            } else {
                Duration::ZERO
            })
            .saturating_add(Duration::from_secs(2))
    }
}

/// The timings of a link: those in effect, and those a `SetComParam` staged. The worker applies
/// a staged value at the connect or at `COPT_UPDATEPARAM`; the handle waits for the longer of
/// the two until an update primitive is accepted, then for the staged one.
#[derive(Debug, Clone, Copy)]
struct LinkTimings {
    active: Timings,
    staged: Timings,
}

impl LinkTimings {
    fn ceiling(&self) -> Duration {
        self.active.ceiling().max(self.staged.ceiling())
    }
}

/// The timing ComParam a `SetComParam` item sets, by name or learned id, and its value.
/// Whether a resume sent on `resumed` resumes a queue suspended on `saved`: the same link or
/// module, a module's resume also covering its links, and the system's covering all.
fn resume_covers(
    resumed: &Option<io_ctl_request::Handle>,
    saved: &Option<io_ctl_request::Handle>,
) -> bool {
    use io_ctl_request::Handle::{CllHandle, ModuleHandle, SystemHandle};
    match (resumed, saved) {
        (Some(SystemHandle(_)), _) => true,
        (Some(ModuleHandle(m)), Some(CllHandle(c))) => c.module_handle == m.module_handle,
        (resumed, saved) => resumed == saved,
    }
}

/// The name of the IO control a request carries, by name or learned id.
fn ioctl_name(request: &IoCtlRequest, learned: &Learned) -> Option<String> {
    match request.io_ctrl_command.as_ref()? {
        io_ctl_request::IoCtrlCommand::IoCtrlCommandName(name) => Some(name.clone()),
        io_ctl_request::IoCtrlCommand::IoCtrlCommandId(id) => {
            learned.objects.get(&(ObjectType::ObjtIoCtrl, *id)).cloned()
        }
    }
}

/// The ComParams that decide how long a primitive may run, by canonical name.
const TIMING_COMPARAMS: [&str; 7] = [
    "cp_p2max",
    "cp_p2star",
    "cp_rc78completiontimeout",
    "cp_rc21handling",
    "cp_rc21completiontimeout",
    "cp_rc23handling",
    "cp_rc23completiontimeout",
];

fn timing_change(item: &ParamItem, learned: &Learned) -> Option<(&'static str, u32)> {
    let name = match item.id.as_ref()? {
        param_item::Id::ParamName(name) => name.clone(),
        param_item::Id::ParamId(id) => learned
            .objects
            .get(&(ObjectType::ObjtComparam, *id))?
            .clone(),
    };
    let Some(param_item::ParamData::Unum32(value)) = item.param_data else {
        return None;
    };
    let name = TIMING_COMPARAMS
        .into_iter()
        .find(|known| *known == canonical(&name))?;
    Some((name, value))
}

/// The ComParams a link is set up with, by the name the worker is asked for.
struct Plan {
    /// Set one by one with `SetComParam`.
    own: Vec<(String, Value, PduParamClass)>,
    /// Set in the response table's one entry.
    table: Vec<(String, Value)>,
}

/// Every ComParam `link::required` lists with the profile's value, split into the table's and
/// the others, then (on T1 to T3) the agent's timing and response-pending defaults for those the
/// profile does not pin.
fn plan(link: &Link, class: Class) -> Result<Plan, HandleError> {
    let in_table = table_required(link);
    let mut plan = Plan {
        own: Vec::new(),
        table: Vec::new(),
    };
    for name in required(link, class) {
        let value = value_of(&name, link)?;
        if in_table.contains(&name) {
            plan.table.push((spelling(&name, link), value));
        } else {
            plan.own
                .push((spelling(&name, link), value, PduParamClass::PduPcSpecified));
        }
    }
    if class != Class::T0 {
        for (name, value) in DEFAULTS {
            let pinned = link
                .comparams
                .keys()
                .any(|key| canonical(key) == canonical(name));
            if !pinned {
                plan.own.push((
                    name.to_owned(),
                    Value::Number(value),
                    PduParamClass::PduPcSpecified,
                ));
            }
        }
    }
    Ok(plan)
}

/// The resource of a profile link: its protocol (or the channel of an additional channel), the
/// dual-wire CAN bus type and the two pins, by ISO 22900-2 (2022) Annex B short names.
fn resource_for(link: &Link, channel: Option<&str>) -> ResourceData {
    let pin = |number: u8, kind: &str| PinData {
        dlc_pin_number: u32::from(number),
        dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName(kind.to_owned())),
    };
    ResourceData {
        bus_type: Some(resource_data::BusType::BusTypeName(BUS_TYPE.to_owned())),
        protocol: Some(resource_data::Protocol::ProtocolName(
            channel.unwrap_or(&link.protocol).to_owned(),
        )),
        dlc_pin_data: vec![
            pin(link.pins[0], PIN_TYPES[0]),
            pin(link.pins[1], PIN_TYPES[1]),
        ],
    }
}

/// The handle of a hardware check on its VCI. See the module documentation.
///
/// It registers its steps with the run's teardown through a clone of the run's handle, so it does
/// not borrow the [`RunGuard`]: several handles (the two VCI modules of a T0 loopback) can be open
/// on one run, and the steps of all run in one global reverse registration order.
pub struct GuardedHandle {
    client: WorkerClient,
    shared: Arc<Shared>,
    teardown: TeardownHandle,
    links: Vec<ComLogicalLinkHandle>,
}

macro_rules! passthrough {
    ($($(#[$doc:meta])* $name:ident($variant:ident, $request:ty) -> $response:ty = $call:literal;)*) => {$(
        $(#[$doc])*
        pub async fn $name(&mut self, request: $request) -> Result<$response, HandleError> {
            self.shared.admit(&Request::$variant(&request))?;
            self.shared.send($call, self.client.clone().$name(request)).await
        }
    )*};
}

impl GuardedHandle {
    /// Opens the profile's links on a device `open_device` opened and matched with `vci`, and
    /// returns the handle. Registers the device's closing (module disconnect, worker stop) with
    /// `run`, then each link's, before any of them is used. On an error the steps registered so
    /// far stay with `run` and run when the check ends.
    pub async fn open(
        profile: &Profile,
        vci: &Vci,
        device: OpenedDevice,
        options: HandleOptions,
        run: &RunGuard,
    ) -> Result<GuardedHandle, HandleError> {
        Self::open_with(profile, vci, device, options, run, true).await
    }

    /// [`GuardedHandle::open`], with or without creating the profile's links. Without them the
    /// handle holds the device only (and closes it with the run); whoever drives the handle
    /// creates the links, through the handle's guarded methods.
    pub(super) async fn open_with(
        profile: &Profile,
        vci: &Vci,
        device: OpenedDevice,
        options: HandleOptions,
        run: &RunGuard,
        links: bool,
    ) -> Result<GuardedHandle, HandleError> {
        let client = device.client();
        let module = device.module_handle();
        let budget = device.close_budget();
        let close: Step = Box::pin(async move {
            match device.close().await {
                Ok(Stopped::Exited) => Ok(()),
                // The device is released, but the worker did not follow its stop request.
                Ok(Stopped::Killed) => {
                    Err("the worker did not exit after its stop request and was killed".to_owned())
                }
                Err(error) => Err(error.to_string()),
            }
        });
        Self::start(
            profile,
            vci,
            client,
            module,
            options,
            run,
            DeviceClose {
                step: close,
                budget,
                links,
            },
        )
        .await
    }

    /// Like [`GuardedHandle::open`], for a worker the caller hosts: `client` talks to it and
    /// `module` is the module already connected on it. The device's closing is the module
    /// disconnect only; the caller stops the worker.
    pub async fn attach(
        profile: &Profile,
        vci: &Vci,
        client: WorkerClient,
        module: ModuleHandle,
        options: HandleOptions,
        run: &RunGuard,
    ) -> Result<GuardedHandle, HandleError> {
        Self::attach_with(profile, vci, client, module, options, run, true).await
    }

    /// [`GuardedHandle::attach`], with or without creating the profile's links (see
    /// [`GuardedHandle::open_with`]).
    pub(super) async fn attach_with(
        profile: &Profile,
        vci: &Vci,
        client: WorkerClient,
        module: ModuleHandle,
        options: HandleOptions,
        run: &RunGuard,
        links: bool,
    ) -> Result<GuardedHandle, HandleError> {
        let mut closing = client.clone();
        let timeout = options.request_timeout;
        let close: Step = Box::pin(async move {
            let request = ModuleDisconnectRequest {
                module_handle: Some(module),
            };
            match tokio::time::timeout(timeout, closing.module_disconnect(request)).await {
                Err(_) => Err(format!("ModuleDisconnect got no answer within {timeout:?}")),
                Ok(Err(status)) => Err(format!("ModuleDisconnect failed: {status}")),
                Ok(Ok(_)) => Ok(()),
            }
        });
        let budget = options.request_timeout;
        Self::start(
            profile,
            vci,
            client,
            module,
            options,
            run,
            DeviceClose {
                step: close,
                budget,
                links,
            },
        )
        .await
    }

    async fn start(
        profile: &Profile,
        vci: &Vci,
        client: WorkerClient,
        module: ModuleHandle,
        options: HandleOptions,
        run: &RunGuard,
        close: DeviceClose,
    ) -> Result<GuardedHandle, HandleError> {
        let links = close.links;
        let mut teardown = run.teardown();
        teardown.register_async_with_timeout(
            "close the device",
            device_close_timeout(close.budget),
            close.step,
        )?;
        let mut handle = GuardedHandle {
            client,
            shared: Arc::new(Shared {
                profile: profile.clone(),
                vci: vci.clone(),
                module: module.module_handle,
                options,
                subscribes: links,
                state: Mutex::new(State::default()),
            }),
            teardown,
            links: Vec::new(),
        };
        if !links {
            // The device is held; the links are created by whoever drives the handle.
            return Ok(handle);
        }
        let endpoints: Vec<(usize, Link, Option<String>)> = {
            let state = handle.shared.lock();
            let ctx = Context {
                profile: &handle.shared.profile,
                module: handle.shared.module,
                vci: &handle.shared.vci,
                links: &state.links,
                learned: &state.learned,
            };
            ctx.worker_endpoints()
                .into_iter()
                .map(|(index, link, channel)| (index, link.clone(), channel.map(str::to_owned)))
                .collect()
        };
        if endpoints.is_empty() {
            return Err(HandleError::Setup(
                "the profile describes no link on this VCI".to_owned(),
            ));
        }
        for (_, link, channel) in endpoints {
            let cll = handle.open_link(&link, channel.as_deref()).await?;
            handle.links.push(cll);
        }
        Ok(handle)
    }

    /// The links the handle created, in the order of the profile's endpoints.
    pub fn links(&self) -> &[ComLogicalLinkHandle] {
        &self.links
    }

    /// The handle of the module the check opened.
    pub fn module_handle(&self) -> ModuleHandle {
        ModuleHandle {
            module_handle: self.shared.module,
        }
    }

    /// The links the harness has bound so far.
    pub fn bound_links(&self) -> LinkTable {
        self.shared.lock().links.clone()
    }

    /// How long [`GuardedHandle::send_receive`] waits on `cll` by default (when
    /// [`HandleOptions::response_timeout`] is `None`), from the timings the link is set to.
    pub fn response_ceiling(&self, cll: ComLogicalLinkHandle) -> Option<Duration> {
        self.shared
            .lock()
            .timings
            .get(&cll.cll_handle)
            .map(LinkTimings::ceiling)
    }

    /// The ids the harness has learned from the worker's answers.
    pub fn learned(&self) -> Learned {
        self.shared.lock().learned.clone()
    }

    /// Creates, sets up and connects the link of one profile endpoint, and registers its
    /// teardown as soon as it exists.
    async fn open_link(
        &mut self,
        link: &Link,
        channel: Option<&str>,
    ) -> Result<ComLogicalLinkHandle, HandleError> {
        let module = self.module_handle();
        let class = self.shared.profile.class;
        // Planned first, so that a link the handle cannot set up is not created.
        let Plan { own, table } = plan(link, class)?;
        let timings = Timings::of(&own);
        let found = self
            .get_resource_ids(GetResourceIdsRequest {
                module_handle: Some(module),
                resource_data: Some(resource_for(link, channel)),
            })
            .await?;
        let resource = found
            .resource_id_list
            .and_then(|list| list.resource_id_data_array.into_iter().next())
            .and_then(|data| data.resource_id_array.into_iter().next())
            .ok_or_else(|| {
                HandleError::Setup("the worker has no resource for the link".to_owned())
            })?;
        let cll = self
            .create_com_logical_link(CreateComLogicalLinkRequest {
                module_handle: Some(module),
                resource: Some(create_com_logical_link_request::Resource::ResourceId(
                    resource,
                )),
                // T0 exchanges raw frames only (the guard refuses RawMode elsewhere).
                cll_create_flag: (class == Class::T0).then(|| {
                    create_com_logical_link_request::CllCreateFlag::CllCreateFlagBits(
                        CllCreateFlag {
                            bits: vec![CllCreateFlagBit::CllCreateFlagRawMode as i32],
                        },
                    )
                }),
            })
            .await?
            .cll_handle
            .ok_or_else(|| HandleError::Setup("no logical link handle".to_owned()))?;

        self.shared.lock().timings.insert(
            cll.cll_handle,
            LinkTimings {
                active: timings,
                staged: timings,
            },
        );
        for (name, value, param_class) in own {
            let id = self.param_id(&name).await?;
            self.set_com_param(SetComParamRequest {
                cll_handle: Some(cll),
                param_item: Some(value.item(id, param_class)),
            })
            .await?;
        }
        let mut params = Vec::new();
        for (name, value) in table {
            let id = self.param_id(&name).await?;
            params.push(value.item(id, PduParamClass::PduPcUniqueId));
        }
        self.set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![EcuUniqueRespData {
                    unique_resp_identifier: 1,
                    params,
                }],
            }),
        })
        .await?;
        self.connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll),
        })
        .await?;
        Ok(cll)
    }

    /// The id the worker gives a ComParam, learned on the way.
    async fn param_id(&mut self, name: &str) -> Result<u32, HandleError> {
        let id = self
            .get_object_id(GetObjectIdRequest {
                object_type: ObjectType::ObjtComparam as i32,
                shortname: name.to_owned(),
            })
            .await?
            .pdu_object_id;
        if id == PDU_ID_UNDEF {
            return Err(HandleError::Setup(format!(
                "the worker does not know ComParam {name}"
            )));
        }
        Ok(id)
    }

    /// Subscribes to the events of `cll` and keeps the stream for [`GuardedHandle::send_receive`].
    /// The subscription is not a request a check makes, and changes nothing on the device.
    async fn subscribe(&mut self, cll: ComLogicalLinkHandle) -> Result<(), HandleError> {
        self.shared
            .ensure_events(&mut self.client.clone(), cll)
            .await
    }

    /// The profile's link a bound link serves, and the pinned ComParams already set on it.
    fn profile_link(
        &self,
        cll: ComLogicalLinkHandle,
    ) -> Result<(Link, BTreeSet<String>), HandleError> {
        let state = self.shared.lock();
        let bound = state
            .links
            .get(&cll.cll_handle)
            .ok_or_else(|| HandleError::Setup("the link is not bound".to_owned()))?;
        let ctx = Context {
            profile: &self.shared.profile,
            module: self.shared.module,
            vci: &self.shared.vci,
            links: &state.links,
            learned: &state.learned,
        };
        let link = ctx
            .endpoint_links()
            .get(bound.endpoint)
            .map(|link| (*link).clone())
            .ok_or_else(|| HandleError::Setup("the link has no profile endpoint".to_owned()))?;
        Ok((link, bound.set.clone()))
    }

    /// `SetUniqueRespIdTable` for a caller that sets up the link itself (the agent's job), with
    /// the profile's own entries added: the caller gives the two identifiers, and the table
    /// replaces the old one, so the formats and address bytes the profile pins are appended
    /// (their ids resolved first) before the guard decides the whole table. The caller's own
    /// values are compared with the profile's unchanged, so a mismatch is still refused. A
    /// table that is not one entry is left as it is, for the guard to refuse.
    pub(super) async fn set_table_completed(
        &mut self,
        mut request: SetUniqueRespIdTableRequest,
    ) -> Result<vci_service_interface::Response, HandleError> {
        if let Some(cll) = request.cll_handle {
            let (link, _) = self.profile_link(cll)?;
            let given: BTreeSet<String> = match request
                .unique_resp_id_table
                .as_ref()
                .map(|table| table.unique_data.as_slice())
            {
                Some([entry]) => {
                    let state = self.shared.lock();
                    entry
                        .params
                        .iter()
                        .filter_map(|item| match item.id.as_ref()? {
                            param_item::Id::ParamName(name) => Some(canonical(name)),
                            param_item::Id::ParamId(id) => state
                                .learned
                                .objects
                                .get(&(ObjectType::ObjtComparam, *id))
                                .map(|name| canonical(name)),
                        })
                        .collect()
                }
                _ => return self.set_unique_resp_id_table(request).await,
            };
            let mut added = Vec::new();
            for name in table_required(&link) {
                if !given.contains(&name) {
                    let id = self.param_id(&spelling(&name, &link)).await?;
                    added.push(value_of(&name, &link)?.item(id, PduParamClass::PduPcUniqueId));
                }
            }
            if let Some([entry]) = request
                .unique_resp_id_table
                .as_mut()
                .map(|table| table.unique_data.as_mut_slice())
            {
                entry.params.extend(added);
            }
        }
        self.set_unique_resp_id_table(request).await
    }

    /// `ConnectComLogicalLink` for a caller that sets up the link itself (the agent's job): the
    /// pinned ComParams the caller did not set (address modes, the functional identifier, the
    /// filler, the tester-present message, the profile's own) are set at the profile's value
    /// first, each decided like any request, so the guard lets the connect through.
    pub(super) async fn connect_completing(
        &mut self,
        request: ConnectComLogicalLinkRequest,
    ) -> Result<vci_service_interface::Response, HandleError> {
        if let Some(cll) = request.cll_handle {
            let (link, set) = self.profile_link(cll)?;
            for name in required(&link, self.shared.profile.class) {
                if set.contains(&name) {
                    continue;
                }
                let value = value_of(&name, &link)?;
                let id = self.param_id(&spelling(&name, &link)).await?;
                self.set_com_param(SetComParamRequest {
                    cll_handle: Some(cll),
                    param_item: Some(value.item(id, PduParamClass::PduPcSpecified)),
                })
                .await?;
            }
        }
        self.connect_com_logical_link(request).await
    }

    /// The profile's link of a RawMode link (T0 only), for the raw API.
    fn raw_link(&self, cll: ComLogicalLinkHandle) -> Result<Link, HandleError> {
        let (link, _) = self.profile_link(cll)?;
        let raw = self
            .shared
            .lock()
            .links
            .get(&cll.cll_handle)
            .is_some_and(|bound| bound.raw_mode);
        if raw && self.shared.profile.class == Class::T0 {
            Ok(link)
        } else {
            Err(HandleError::Refused {
                rpc: "StartComPrimitive",
                refusal: GuardRefusal::Frame(
                    "raw frames are a T0 tool: this link is not a RawMode link of a T0 profile",
                ),
            })
        }
    }

    /// The TxFlags of a RawMode send on `cll`, from the profile: the 29-bit identifier flag, the
    /// extended-addressing flag when the addressing is not normal, the frame-padding flag when
    /// the profile pads. A RawMode worker takes the frame's format from these alone, and the guard
    /// requires each to equal the profile's.
    pub fn raw_flags(&self, cll: ComLogicalLinkHandle) -> Result<Vec<TxFlagBit>, HandleError> {
        let link = self.raw_link(cll)?;
        let mut flags = Vec::new();
        if link.id_bits == 29 {
            flags.push(TxFlagBit::TxFlagCan29bitId);
        }
        if link.addressing != Addressing::Normal {
            flags.push(TxFlagBit::TxFlagIso15765AddrType);
        }
        if link.padding.is_some() {
            flags.push(TxFlagBit::TxFlagIso15765FramePad);
        }
        Ok(flags)
    }

    /// The RawMode frame the end of `cll` sends for `payload`: its request identifier in four
    /// bytes, most significant first, the profile's address byte under extended or mixed
    /// addressing, then the payload (design 13.5; the data is the literal `PassThruMessage.Data`
    /// of the J2534 worker, and a receiver gets the same bytes whole). With an empty payload it is
    /// the end's own header.
    pub fn raw_frame(
        &self,
        cll: ComLogicalLinkHandle,
        payload: &[u8],
    ) -> Result<Vec<u8>, HandleError> {
        let link = self.raw_link(cll)?;
        let mut frame = link.request_id.to_be_bytes().to_vec();
        if link.addressing != Addressing::Normal {
            frame.push(link.address.unwrap_or(0));
        }
        frame.extend_from_slice(payload);
        Ok(frame)
    }

    /// Sends one RawMode frame with `payload` from the end of `cll` (a send, no receive), through
    /// the guard. Returns the primitive; it is recorded like any started primitive.
    pub async fn raw_send(
        &mut self,
        cll: ComLogicalLinkHandle,
        payload: &[u8],
    ) -> Result<ComPrimitiveHandle, HandleError> {
        let request = StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll),
            cop_type: ComOperationType::CoptSendrecv as i32,
            cop_data: self.raw_frame(cll, payload)?,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: vec![],
                tx_flag: Some(com_primitive_ctrl_data::TxFlag::TxFlagBits(TxFlag {
                    bits: self
                        .raw_flags(cll)?
                        .iter()
                        .map(|&flag| flag as i32)
                        .collect(),
                })),
            }),
        };
        let cop = self
            .start_com_primitive(request)
            .await?
            .cop_handle
            .ok_or_else(|| {
                HandleError::Setup("StartComPrimitive returned no primitive handle".to_owned())
            })?;
        // The send's own events are read here, so a transmit error is the call's error (and the
        // frames the monitor receives meanwhile are kept for `raw_receive`).
        match self
            .raw_wait(cll, RawWait::SendEnd(cop), RAW_SEND_BOUND)
            .await
        {
            Ok(_) => Ok(cop),
            Err(HandleError::NoResponse) => Err(HandleError::SendFailed(format!(
                "no confirmation within {RAW_SEND_BOUND:?}"
            ))),
            Err(error) => Err(error),
        }
    }

    /// Starts the receive-only primitive of `cll` (no send, receives until cancelled), whose
    /// frames `raw_receive` reads. Its data is the receiving end's own header: the guard reads any
    /// non-empty data of a RawMode primitive as a frame, and a header of the end's own is the
    /// frame it accepts. Its one expected response matches any frame, so every frame of the link
    /// reaches it. It stays recorded as started, so the link's closing cancels it. At most one per link.
    pub async fn raw_listen(
        &mut self,
        cll: ComLogicalLinkHandle,
    ) -> Result<ComPrimitiveHandle, HandleError> {
        // One monitor per link: the one that listens is returned, not replaced.
        let existing = self.shared.lock().monitors.get(&cll.cll_handle).copied();
        if let Some(cop) = existing {
            return Ok(cop);
        }
        let request = StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll),
            cop_type: ComOperationType::CoptSendrecv as i32,
            cop_data: self.raw_frame(cll, &[])?,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -1,
                temp_param_update: 0,
                // One descriptor that matches any frame, as the worker's receive-only monitors
                // take it: without one nothing is delivered to the primitive.
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 0,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: Some(com_primitive_ctrl_data::TxFlag::TxFlagBits(TxFlag {
                    bits: self
                        .raw_flags(cll)?
                        .iter()
                        .map(|&flag| flag as i32)
                        .collect(),
                })),
            }),
        };
        let cop = self
            .start_com_primitive(request)
            .await?
            .cop_handle
            .ok_or_else(|| {
                HandleError::Setup("StartComPrimitive returned no primitive handle".to_owned())
            })?;
        self.shared.lock().monitors.insert(cll.cll_handle, cop);
        Ok(cop)
    }

    /// Waits up to `timeout` for the frame `expected` (the whole frame, as the sender's
    /// `raw_frame` built it) on the monitor of `cll`, and returns it. Echoes of the end's own
    /// transmissions (TX indications and loopback copies) are skipped. Frames of the monitor that
    /// are not `expected` stay buffered (up to 1024 per link, then
    /// [`HandleError::RawBufferFull`]) for a later call, so the order of the calls does not lose
    /// them. [`HandleError::NoResponse`] when the deadline passes, [`HandleError::MonitorEnded`]
    /// when the monitor ended.
    pub async fn raw_receive(
        &mut self,
        cll: ComLogicalLinkHandle,
        expected: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, HandleError> {
        self.raw_wait(cll, RawWait::Frame(expected), timeout)
            .await?
            .ok_or(HandleError::NoResponse)
    }

    /// Discards what the monitor of `cll` has received so far: the frames already buffered, and
    /// those its events deliver until the link stays quiet for 50 ms. A check calls it before a
    /// send, so a frame received afterwards cannot be an earlier copy of the one it awaits.
    pub async fn raw_drain(&mut self, cll: ComLogicalLinkHandle) -> Result<(), HandleError> {
        const QUIET: Duration = Duration::from_millis(50);
        // A busy bus can fill the buffer while it drains; each full buffer is discarded in turn.
        for _ in 0..8 {
            self.shared.lock().raw_frames.remove(&cll.cll_handle);
            // An empty frame is never received, so the wait only buffers what arrives.
            match self.raw_wait(cll, RawWait::Frame(&[]), QUIET).await {
                Err(HandleError::NoResponse) => {
                    self.shared.lock().raw_frames.remove(&cll.cll_handle);
                    return Ok(());
                }
                Err(HandleError::RawBufferFull { .. }) => continue,
                Err(error) => return Err(error),
                Ok(_) => continue,
            }
        }
        Err(HandleError::RawBufferFull {
            cap: RAW_BUFFER_CAP,
        })
    }

    /// Reads the link's events until `wait` is met or `timeout` passes: the monitor's frames go
    /// into the link's buffer (a `Frame` wait takes its frame out), and the events of the send a
    /// `SendEnd` wait is for end it or fail it. Events of other primitives are dropped.
    async fn raw_wait(
        &self,
        cll: ComLogicalLinkHandle,
        wait: RawWait<'_>,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, HandleError> {
        let monitor = self.shared.lock().monitors.get(&cll.cll_handle).copied();
        if matches!(wait, RawWait::Frame(_)) && monitor.is_none() {
            return Err(HandleError::Setup(
                "the link has no monitor (raw_listen)".to_owned(),
            ));
        }
        let events = self
            .shared
            .lock()
            .events
            .get(&cll.cll_handle)
            .cloned()
            .ok_or_else(|| HandleError::Setup("the link has no event stream".to_owned()))?;
        let now = tokio::time::Instant::now();
        let deadline = now
            .checked_add(timeout)
            .unwrap_or_else(|| now + Duration::from_secs(86_400 * 365));
        let mut stream = events.lock().await;
        loop {
            if let RawWait::Frame(expected) = wait {
                let mut state = self.shared.lock();
                if let Some(frames) = state.raw_frames.get_mut(&cll.cll_handle)
                    && let Some(at) = frames.iter().position(|frame| frame == expected)
                {
                    return Ok(frames.remove(at));
                }
            }
            let notification = match tokio::time::timeout_at(deadline, stream.message()).await {
                Err(_) => return Err(HandleError::NoResponse),
                Ok(Err(status)) => return Err(worker_error("SubscribeEvent", &status)),
                Ok(Ok(None)) => return Err(HandleError::EventStreamEnded),
                Ok(Ok(Some(notification))) => notification,
            };
            let item = match notification.event_data {
                Some(event_notification::EventData::Item(item)) => item,
                Some(event_notification::EventData::Lost(_)) => {
                    return Err(HandleError::EventsLost);
                }
                None => continue,
            };
            let send = match wait {
                RawWait::SendEnd(cop) => Some(cop),
                RawWait::Frame(_) => None,
            };
            let ours = |handle: Option<ComPrimitiveHandle>| {
                handle.is_some() && (handle == monitor || handle == send)
            };
            match item.data {
                Some(event_item::Data::ErrorData(code))
                    if code == PduErrorEvent::PduErrEvtLostCommToVci as i32
                        && (item.cop_handle.is_none() || ours(item.cop_handle)) =>
                {
                    return Err(HandleError::VciLost);
                }
                Some(event_item::Data::ResultData(result))
                    if item.cop_handle == monitor && !is_echo(&result.rx_flag) =>
                {
                    // The awaited frame is returned before the cap is applied: only frames kept
                    // for later count against it.
                    if let RawWait::Frame(expected) = wait
                        && result.data_bytes == expected
                    {
                        return Ok(Some(result.data_bytes));
                    }
                    let mut state = self.shared.lock();
                    let frames = state.raw_frames.entry(cll.cll_handle).or_default();
                    if frames.len() >= RAW_BUFFER_CAP {
                        return Err(HandleError::RawBufferFull {
                            cap: RAW_BUFFER_CAP,
                        });
                    }
                    frames.push_back(result.data_bytes);
                }
                Some(event_item::Data::ErrorData(code))
                    if send.is_some()
                        && item.cop_handle == send
                        && code != PduErrorEvent::PduErrEvtRxTimeout as i32 =>
                {
                    return Err(HandleError::SendFailed(format!(
                        "transmit error event {code:#x}"
                    )));
                }
                Some(event_item::Data::CopStatus(status))
                    if send.is_some() && item.cop_handle == send =>
                {
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32 {
                        return Ok(None);
                    }
                    if status == PduComPrimitiveStatus::PduCopstCancelled as i32 {
                        return Err(HandleError::SendFailed("the send was cancelled".to_owned()));
                    }
                }
                Some(event_item::Data::CopStatus(status))
                    if item.cop_handle == monitor
                        && (status == PduComPrimitiveStatus::PduCopstFinished as i32
                            || status == PduComPrimitiveStatus::PduCopstCancelled as i32) =>
                {
                    // The monitor ended: nothing more arrives on this link.
                    self.shared.lock().monitors.remove(&cll.cll_handle);
                    return Err(HandleError::MonitorEnded);
                }
                _ => {}
            }
        }
    }

    /// Turns the trace of the guard's decisions on and returns it: every decision from now on is
    /// appended, whoever asked (a check, the handle's own set-up, a teardown step).
    pub fn enable_trace(&self) -> Trace {
        self.shared
            .lock()
            .trace
            .get_or_insert_with(Trace::default)
            .clone()
    }

    /// `SubscribeEvent` for a caller that reads the events itself (the agent's job). The stream
    /// is the caller's: the handle keeps none, and so reads no events of the link meanwhile.
    pub(super) async fn subscribe_event(
        &mut self,
        request: SubscribeEventRequest,
    ) -> Result<Streaming<EventNotification>, HandleError> {
        self.shared.admit(&Request::SubscribeEvent(&request))?;
        self.shared
            .send(
                "SubscribeEvent",
                self.client.clone().subscribe_event(request),
            )
            .await
    }

    /// The decision on a request the handle never sends (the harness owns the module), for the
    /// trace; always a refusal.
    pub(super) fn decide_module_request(&self, request: &Request<'_>) -> HandleError {
        // Nothing is sent for a granted one, so only a refusal is recorded: the trace keeps
        // matching what the worker received.
        match self.shared.check(request) {
            Err(error) => error,
            Ok(_) => HandleError::Setup(format!("{} is not offered by the handle", request.rpc())),
        }
    }

    /// `GetResourceIds`; the resource each returned id stands for is learned.
    pub async fn get_resource_ids(
        &mut self,
        request: GetResourceIdsRequest,
    ) -> Result<ResourceIdsResponse, HandleError> {
        self.shared.admit(&Request::GetResourceIds(&request))?;
        let looked_up = request.resource_data.clone();
        let response = self
            .shared
            .send(
                "GetResourceIds",
                self.client.clone().get_resource_ids(request),
            )
            .await?;
        if let (Some(resource), Some(list)) = (looked_up, &response.resource_id_list) {
            let mut state = self.shared.lock();
            for data in &list.resource_id_data_array {
                for &id in &data.resource_id_array {
                    state.learned.resources.insert(id, resource.clone());
                }
            }
        }
        Ok(response)
    }

    /// `GetObjectId`; the name the id was looked up for is learned, unless the worker does not
    /// know the name.
    pub async fn get_object_id(
        &mut self,
        request: GetObjectIdRequest,
    ) -> Result<ObjectIdResponse, HandleError> {
        self.shared.admit(&Request::GetObjectId(&request))?;
        let (object_type, shortname) = (request.object_type, request.shortname.clone());
        let response = self
            .shared
            .send("GetObjectId", self.client.clone().get_object_id(request))
            .await?;
        if response.pdu_object_id != PDU_ID_UNDEF
            && let Ok(object_type) = ObjectType::try_from(object_type)
        {
            self.shared
                .lock()
                .learned
                .objects
                .insert((object_type, response.pdu_object_id), shortname);
        }
        Ok(response)
    }

    /// `CreateComLogicalLink`. The handle of the answer is bound to the profile endpoint the
    /// guard granted, and the link's closing is registered with the run.
    pub async fn create_com_logical_link(
        &mut self,
        request: CreateComLogicalLinkRequest,
    ) -> Result<ComLogicalLinkResponse, HandleError> {
        let grant = self
            .shared
            .admit(&Request::CreateComLogicalLink(&request))?;
        let response = self
            .shared
            .send(
                "CreateComLogicalLink",
                self.client.clone().create_com_logical_link(request),
            )
            .await?;
        if let (Grant::CreatesLink { endpoint, raw_mode }, Some(cll)) = (grant, response.cll_handle)
        {
            {
                let mut state = self.shared.lock();
                state
                    .links
                    .insert(cll.cll_handle, BoundLink::new(endpoint, raw_mode));
                state.closing.insert(cll.cll_handle);
            }
            let shared = self.shared.clone();
            if let Err(closed) = self.teardown.register_async_with_timeout_fn(
                format!("close link {}", cll.cll_handle),
                move || {
                    close_link_timeout(
                        shared.options.request_timeout,
                        shared.outstanding_requests(cll),
                    )
                },
                close_link(self.shared.clone(), self.client.clone(), cll),
            ) {
                // No step will release the link, so it is released now.
                let _ = close_link(self.shared.clone(), self.client.clone(), cll).await;
                return Err(closed.into());
            }
            // Whatever creates a link through the handle gets what a send-receive needs: its
            // event stream, and timings to wait by (the handle's defaults until the check sets
            // others; `open_link` replaces them with what it sets).
            self.shared
                .lock()
                .timings
                .entry(cll.cll_handle)
                .or_insert(LinkTimings {
                    active: Timings::default(),
                    staged: Timings::default(),
                });
            if self.shared.subscribes {
                self.subscribe(cll).await?;
            }
        }
        Ok(response)
    }

    /// `IoCtl`, of the IO controls the guard lets through. A suspension of the transmit queue
    /// registers its resumption with the run first, since a suspended queue holds back the
    /// return to the default session.
    pub async fn io_ctl(&mut self, request: IoCtlRequest) -> Result<IoCtlResponse, HandleError> {
        self.shared.admit(&Request::IoCtl(&request))?;
        let name = ioctl_name(&request, &self.shared.lock().learned);
        let is = |wanted: &str| {
            name.as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(wanted))
        };
        let resumed_scope = request.handle;
        let suspends = is("PDU_IOCTL_SUSPEND_TX_QUEUE");
        let resumes = is("PDU_IOCTL_RESUME_TX_QUEUE");
        if suspends {
            // The worker takes an IO control by its id only.
            let resume_id = self
                .get_object_id(GetObjectIdRequest {
                    object_type: ObjectType::ObjtIoCtrl as i32,
                    shortname: "PDU_IOCTL_RESUME_TX_QUEUE".to_owned(),
                })
                .await?
                .pdu_object_id;
            if resume_id == PDU_ID_UNDEF {
                return Err(HandleError::Setup(
                    "the worker cannot resume the transmit queue, so it is not suspended"
                        .to_owned(),
                ));
            }
            let resume = IoCtlRequest {
                handle: request.handle,
                io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(resume_id)),
                input_data: None,
                has_output: false,
            };
            let register = {
                let mut state = self.shared.lock();
                state.suspended.push(resume);
                !std::mem::replace(&mut state.resume_registered, true)
            };
            if register {
                let shared = self.shared.clone();
                let mut client = self.client.clone();
                if let Err(closed) =
                    self.teardown
                        .register_async("resume the transmit queue", async move {
                            shared.lock().resume_registered = false;
                            shared
                                .resume_queues(&mut client)
                                .await
                                .map_err(|e| e.to_string())
                        })
                {
                    // Nothing was sent: the suspension is not recorded either.
                    let mut state = self.shared.lock();
                    state.suspended.pop();
                    state.resume_registered = false;
                    return Err(closed.into());
                }
            }
        }
        let sent = self
            .shared
            .send("IoCtl", self.client.clone().io_ctl(request))
            .await;
        // A rejected suspension suspended nothing; with any other failure it may have. The
        // record was made before the call, so it is dropped only for a rejection.
        if suspends && matches!(&sent, Err(error) if undoes_request(error)) {
            self.shared.lock().suspended.pop();
        }
        if resumes && sent.is_ok() {
            // Only what the resume covers is resumed.
            let resumed = resumed_scope;
            self.shared
                .lock()
                .suspended
                .retain(|saved| !resume_covers(&resumed, &saved.handle));
        }
        sent
    }

    /// `ConnectComLogicalLink`, which the guard allows once the profile's ComParams are set. A
    /// connect puts the staged timings into effect.
    pub async fn connect_com_logical_link(
        &mut self,
        request: ConnectComLogicalLinkRequest,
    ) -> Result<vci_service_interface::Response, HandleError> {
        self.shared
            .admit(&Request::ConnectComLogicalLink(&request))?;
        let cll = request.cll_handle;
        let response = self
            .shared
            .send(
                "ConnectComLogicalLink",
                self.client.clone().connect_com_logical_link(request),
            )
            .await?;
        if let Some(cll) = cll {
            self.shared.promote_timings(cll);
        }
        Ok(response)
    }

    /// Settles a pending return to the default session before `cll` stops being usable. Every
    /// operation that ends a link's ability to talk (disconnect, destroy, stopping
    /// communication) calls this first: a link that entered another session is sent `10 01` and
    /// must answer `50 01`, or the operation is refused with `what` named, and the return stays
    /// registered with the run. A link with nothing pending passes at once.
    async fn settle_session(
        &self,
        cll: Option<ComLogicalLinkHandle>,
        what: &str,
    ) -> Result<(), HandleError> {
        let Some(cll) = cll else {
            return Ok(());
        };
        if !self.shared.lock().session_back.contains(&cll.cll_handle) {
            return Ok(());
        }
        return_to_default_session(self.shared.clone(), self.client.clone(), cll)
            .await
            .map_err(|reason| {
                HandleError::Setup(format!(
                    "the link is not {what}: it could not be returned to the default session \
                     first: {reason}"
                ))
            })
    }

    /// `DisconnectComLogicalLink`, after a pending return to the default session.
    pub async fn disconnect_com_logical_link(
        &mut self,
        request: DisconnectComLogicalLinkRequest,
    ) -> Result<vci_service_interface::Response, HandleError> {
        self.shared
            .check(&Request::DisconnectComLogicalLink(&request))?;
        if let Some(cll) = request.cll_handle {
            self.shared
                .cancel_outstanding(&mut self.client.clone(), cll)
                .await?;
            self.shared
                .resume_link(&mut self.client.clone(), cll)
                .await?;
        }
        self.settle_session(request.cll_handle, "disconnected")
            .await?;
        self.shared
            .admit(&Request::DisconnectComLogicalLink(&request))?;
        self.shared
            .send(
                "DisconnectComLogicalLink",
                self.client.clone().disconnect_com_logical_link(request),
            )
            .await
    }

    /// `DestroyComLogicalLink`; the link is dropped from the table.
    pub async fn destroy_com_logical_link(
        &mut self,
        request: DestroyComLogicalLinkRequest,
    ) -> Result<vci_service_interface::Response, HandleError> {
        self.shared
            .check(&Request::DestroyComLogicalLink(&request))?;
        if let Some(cll) = request.cll_handle {
            self.shared
                .cancel_outstanding(&mut self.client.clone(), cll)
                .await?;
            self.shared
                .resume_link(&mut self.client.clone(), cll)
                .await?;
        }
        self.settle_session(request.cll_handle, "destroyed").await?;
        let grant = self
            .shared
            .admit(&Request::DestroyComLogicalLink(&request))?;
        let response = self
            .shared
            .send(
                "DestroyComLogicalLink",
                self.client.clone().destroy_com_logical_link(request),
            )
            .await?;
        self.shared.apply(&grant);
        Ok(response)
    }

    /// `SetComParam`; the profile's ComParams it set are recorded for the connect check.
    pub async fn set_com_param(
        &mut self,
        request: SetComParamRequest,
    ) -> Result<vci_service_interface::Response, HandleError> {
        let grant = self.shared.admit(&Request::SetComParam(&request))?;
        let change = request.cll_handle.zip(
            request
                .param_item
                .as_ref()
                .and_then(|item| timing_change(item, &self.shared.lock().learned)),
        );
        let response = self
            .shared
            .send("SetComParam", self.client.clone().set_com_param(request))
            .await?;
        self.shared.apply(&grant);
        if let Some((cll, (name, value))) = change
            && let Some(timings) = self.shared.lock().timings.get_mut(&cll.cll_handle)
        {
            timings.staged.set(name, value);
        }
        Ok(response)
    }

    /// `SetUniqueRespIdTable`; the profile's ComParams it holds are recorded.
    pub async fn set_unique_resp_id_table(
        &mut self,
        request: SetUniqueRespIdTableRequest,
    ) -> Result<vci_service_interface::Response, HandleError> {
        let grant = self
            .shared
            .admit(&Request::SetUniqueRespIdTable(&request))?;
        let response = self
            .shared
            .send(
                "SetUniqueRespIdTable",
                self.client.clone().set_unique_resp_id_table(request),
            )
            .await?;
        self.shared.apply(&grant);
        Ok(response)
    }

    /// `StartComPrimitive`. A request that enters a diagnostic session registers the link's
    /// return to the default session before it is sent: the ECU may have changed session even
    /// when the answer is lost.
    pub async fn start_com_primitive(
        &mut self,
        request: StartComPrimitiveRequest,
    ) -> Result<ComPrimitiveResponse, HandleError> {
        let cll = request.cll_handle;
        let grant = self.shared.admit(&Request::StartComPrimitive(&request))?;
        if let Some(cll) = cll {
            // A primitive a dropped wait left running goes first.
            self.shared
                .cancel_stale(&mut self.client.clone(), cll)
                .await?;
        }
        // Stopping communication ends the tester present that keeps a session up.
        if request.cop_type == ComOperationType::CoptStopcomm as i32 {
            if let Some(cll) = cll {
                self.shared
                    .cancel_outstanding(&mut self.client.clone(), cll)
                    .await?;
            }
            self.settle_session(cll, "taken out of communication")
                .await?;
        }
        let entering = match (&grant, cll) {
            (Grant::EntersSession(_), Some(cll)) => Some(cll),
            _ => None,
        };
        if let Some(cll) = entering
            && self.shared.lock().session_registered.insert(cll.cll_handle)
        {
            // The step waits no longer than the link's response wait is when it runs, and has
            // the time for that (read at the same moment), so it ends by itself and cancels
            // instead of being dropped.
            let shared = self.shared.clone();
            if let Err(closed) = self.teardown.register_async_with_timeout_fn(
                format!("return link {} to the default session", cll.cll_handle),
                move || {
                    session_reset_timeout(
                        shared.wait_for(cll),
                        shared.options.request_timeout,
                        shared.outstanding_requests(cll),
                    )
                },
                return_to_default_session(self.shared.clone(), self.client.clone(), cll),
            ) {
                // Nothing was sent, so nothing needs returning.
                self.shared
                    .lock()
                    .session_registered
                    .remove(&cll.cll_handle);
                return Err(closed.into());
            }
        }
        let updates = request.cop_type == ComOperationType::CoptUpdateparam as i32;
        // The ECU may be in the session as soon as the request is on its way: the return is
        // pending from before the call, whether it succeeds, times out or is dropped. Only a
        // refusal by the worker (the link is not connected, say) takes the marker back.
        let mut provisional = entering.map(|cll| ProvisionalSession::new(&self.shared, cll));
        let sent = self
            .shared
            .send(
                "StartComPrimitive",
                self.client.clone().start_com_primitive(request),
            )
            .await;
        if let (Some(provisional), Err(error)) = (provisional.as_mut(), &sent) {
            provisional.refused = undoes_request(error);
        }
        drop(provisional);
        let response = sent?;
        if let (Some(cll), Some(cop)) = (cll, response.cop_handle) {
            let mut state = self.shared.lock();
            let started = state.started.entry(cll.cll_handle).or_default();
            if self.shared.subscribes {
                started.push(cop);
            } else {
                // The handle does not read this link's events (a job does), so it never sees a
                // primitive end and would cancel every one a long job started, one request each,
                // at the disconnect. Only the latest is kept: cancelling it stops what may still
                // run, and the link's disconnect ends the rest.
                *started = vec![cop];
            }
        }
        // The staged timings are in effect once the update has finished successfully; until
        // then (and when it fails, which is returned) the longer of the two is waited for.
        if updates && let (Some(cll), Some(cop)) = (cll, response.cop_handle) {
            // An update that fails, is cancelled or times out is the call's error, and the
            // timings stay as they were.
            self.shared
                .await_primitive(&mut self.client.clone(), cll, cop, None, None)
                .await?;
            self.shared.promote_timings(cll);
        }
        Ok(response)
    }

    /// Sends the UDS request `request` on `cll` as a send-receive primitive and returns the
    /// final response (a negative response included), through [`GuardedHandle::start_com_primitive`].
    /// Ends in [`HandleError::NoResponse`] when the ECU does not answer within the response
    /// timeout.
    pub async fn send_receive(
        &mut self,
        cll: ComLogicalLinkHandle,
        request: &[u8],
    ) -> Result<Vec<u8>, HandleError> {
        // The events are read from before the request goes out.
        self.shared
            .ensure_events(&mut self.client.clone(), cll)
            .await?;
        let cop = self
            .start_com_primitive(send_receive_request(cll, request))
            .await?
            .cop_handle
            .ok_or_else(|| {
                HandleError::Setup("StartComPrimitive returned no primitive handle".to_owned())
            })?;
        let response = self
            .shared
            .await_result(&mut self.client.clone(), cll, cop, request, None)
            .await?;
        self.shared.observe_session(cll, request, &response);
        Ok(response)
    }

    passthrough! {
        /// `GetModuleIds`.
        get_module_ids(GetModuleIds, GetModuleIdsRequest) -> ModuleIdsResponse = "GetModuleIds";
        /// `GetVersion` of the check's module.
        get_version(GetVersion, GetVersionRequest) -> VersionResponse = "GetVersion";
        /// `GetTimestamp` of the check's module.
        get_timestamp(GetTimestamp, GetTimestampRequest) -> TimestampResponse = "GetTimestamp";
        /// `GetResourceStatus`.
        get_resource_status(GetResourceStatus, GetResourceStatusRequest) -> ResourceStatusResponse = "GetResourceStatus";
        /// `GetConflictingResources`.
        get_conflicting_resources(GetConflictingResources, GetConflictingResourcesRequest) -> ConflictingResourcesResponse = "GetConflictingResources";
        /// `LockResource`.
        lock_resource(LockResource, LockResourceRequest) -> vci_service_interface::Response = "LockResource";
        /// `UnlockResource`.
        unlock_resource(UnlockResource, UnlockResourceRequest) -> vci_service_interface::Response = "UnlockResource";
        /// `GetComParam`.
        get_com_param(GetComParam, GetComParamRequest) -> ComParamResponse = "GetComParam";
        /// `CancelComPrimitive`.
        cancel_com_primitive(CancelComPrimitive, CancelComPrimitiveRequest) -> vci_service_interface::Response = "CancelComPrimitive";
        /// `GetStatus`.
        get_status(GetStatus, GetStatusRequest) -> StatusResponse = "GetStatus";
        /// `GetUniqueRespIdTable`.
        get_unique_resp_id_table(GetUniqueRespIdTable, GetUniqueRespIdTableRequest) -> UniqueRespIdTableResponse = "GetUniqueRespIdTable";
    }
}

#[cfg(test)]
mod tests {
    use vci_service_interface::ModuleHandle as ModuleHandle_;
    use vci_service_interface::ResultData;

    use super::*;
    use crate::profile::{Setup, parse};

    const VCI: &str = "api = \"j2534-0404\"\nlibrary = \"Vendor VCI\"\nmodule = \"VCI-1\"\n\
                       label = \"bench\"\npname = \"Vendor VCI 1\"\nfirmware_version = \"1.2.3\"\n\
                       library_version = \"4.5\"\n";

    fn profile(class: &str, link: &str) -> Profile {
        parse(&format!(
            "class = \"{class}\"\n[vci]\n{VCI}\n[link]\nprotocol = \"ISO_14229_3_on_ISO_15765_2\"\n\
             baud_rate = 500000\npins = [6, 14]\nrequest_id = 0x7E0\nresponse_id = 0x7E8\n{link}"
        ))
        .unwrap()
    }

    /// Profile link shapes: every combination of what `required` branches on.
    fn shapes() -> Vec<Profile> {
        [
            "id_bits = 11\naddressing = \"normal\"\n",
            "id_bits = 11\naddressing = \"normal\"\nfunctional_id = 0x7DF\npadding = 0xCC\n\
             [link.comparams]\nCP_BlockSize = 0\nCP_STmin = 5\nCP_P2Max = 500000\n",
            "id_bits = 29\naddressing = \"extended\"\naddress = 0x55\nfunctional_id = 0x18DB33F1\n",
            "id_bits = 29\naddressing = \"mixed\"\naddress = 0x12\npadding = 0\n",
        ]
        .iter()
        .flat_map(|link| ["T1", "T2", "T3"].map(|class| profile(class, link)))
        .collect()
    }

    fn link_of(profile: &Profile) -> &Link {
        match &profile.setup {
            Setup::Link { link, .. } => link,
            Setup::Loopback(ends) => &ends[0].link,
        }
    }

    fn by_name(name: &str, value: &Value, class: PduParamClass) -> ParamItem {
        ParamItem {
            id: Some(param_item::Id::ParamName(name.to_owned())),
            ..value.item(0, class)
        }
    }

    /// What the handle sets for each shape of profile satisfies the guard: every ComParam is
    /// accepted at the profile's value, the table is accepted as the one entry, and the
    /// connect is then allowed. The test uses the handle's own plan and the guard's `decide`.
    #[test]
    fn the_plan_of_every_link_shape_satisfies_the_guard() {
        for profile in shapes() {
            let link = link_of(&profile);
            let Setup::Link { vci, .. } = &profile.setup else {
                unreachable!()
            };
            let Plan { own, table } = plan(link, profile.class).unwrap();
            let learned = Learned::default();
            let mut links = LinkTable::new();
            links.insert(10, BoundLink::new(0, false));
            let cll = ComLogicalLinkHandle {
                module_handle: 1,
                cll_handle: 10,
            };
            let apply = |request: Request<'_>, links: &mut LinkTable| {
                let ctx = Context {
                    profile: &profile,
                    module: 1,
                    vci,
                    links,
                    learned: &learned,
                };
                let grant = decide(&ctx, &request).unwrap_or_else(|e| {
                    panic!(
                        "{:?} {link:?}: {} refused: {e}",
                        profile.class,
                        request.rpc()
                    )
                });
                if let Grant::SetsComParams { cll, names } = grant {
                    links.get_mut(&cll).unwrap().set.extend(names);
                }
            };
            for (name, value, class) in &own {
                let request = SetComParamRequest {
                    cll_handle: Some(cll),
                    param_item: Some(by_name(name, value, *class)),
                };
                apply(Request::SetComParam(&request), &mut links);
            }
            let request = SetUniqueRespIdTableRequest {
                cll_handle: Some(cll),
                unique_resp_id_table: Some(UniqueRespIdTableItem {
                    unique_data: vec![EcuUniqueRespData {
                        unique_resp_identifier: 1,
                        params: table
                            .iter()
                            .map(|(name, value)| by_name(name, value, PduParamClass::PduPcUniqueId))
                            .collect(),
                    }],
                }),
            };
            apply(Request::SetUniqueRespIdTable(&request), &mut links);
            let connect = ConnectComLogicalLinkRequest {
                cll_handle: Some(cll),
            };
            apply(Request::ConnectComLogicalLink(&connect), &mut links);
        }
    }

    #[test]
    fn the_plan_sets_each_pinned_comparam_once_and_the_defaults_unless_pinned() {
        let profile = &shapes()[3 + 2]; // the second shape on T3
        let link = link_of(profile);
        let Plan { own, table } = plan(link, profile.class).unwrap();
        let names: Vec<&str> = own.iter().map(|(n, ..)| n.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len(), "{names:?}");
        // The profile pins CP_P2Max, so the default is not set over it; the others are set.
        assert_eq!(names.iter().filter(|n| **n == "CP_P2Max").count(), 1);
        assert!(names.contains(&"CP_RC78Handling"));
        assert!(names.contains(&"CP_BlockSize") && names.contains(&"CP_STmin"));
        let table: Vec<&str> = table.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            table,
            [
                "CP_CanPhysReqFormat",
                "CP_CanPhysReqId",
                "CP_CanRespUSDTFormat",
                "CP_CanRespUSDTId"
            ]
        );
    }

    #[test]
    fn a_t0_link_sets_no_timing_defaults_and_no_tester_present() {
        let t0 = parse(&format!(
            "class = \"T0\"\n[[endpoint]]\nsocketcan = \"can0\"\n[endpoint.link]\nprotocol = \"CAN\"\n\
             baud_rate = 500000\npins = [6, 14]\nrequest_id = 0x123\nresponse_id = 0x124\n\
             id_bits = 11\naddressing = \"normal\"\n[[endpoint]]\n[endpoint.vci]\n{VCI}\n\
             [endpoint.link]\nprotocol = \"CAN\"\nbaud_rate = 500000\n\
             pins = [6, 14]\nrequest_id = 0x124\nresponse_id = 0x123\nid_bits = 11\n\
             addressing = \"normal\"\n"
        ))
        .unwrap();
        let Setup::Loopback(ends) = &t0.setup else {
            unreachable!()
        };
        let Plan { own, .. } = plan(&ends[1].link, Class::T0).unwrap();
        let names: Vec<&str> = own.iter().map(|(n, ..)| n.as_str()).collect();
        assert!(!names.contains(&"CP_P2Max") && !names.contains(&"CP_TesterPresentMessage"));
    }

    fn ceiling_of(profile: &Profile) -> Duration {
        let Plan { own, .. } = plan(link_of(profile), profile.class).unwrap();
        Timings::of(&own).ceiling()
    }

    #[test]
    fn the_default_deadline_covers_the_completion_timeout_the_link_is_set_to() {
        // The handle's own defaults: P2 1 s + 0x78 chain 25 s + P2* 5 s, and a margin.
        let t3 = &shapes()[2];
        assert_eq!(ceiling_of(t3), Duration::from_secs(33));
        // A profile that pins the timings is waited for as pinned.
        let pinned = profile(
            "T3",
            "id_bits = 11\naddressing = \"normal\"\n[link.comparams]\nCP_P2Max = 2000000\n\
             CP_P2Star = 3000000\nCP_RC78CompletionTimeout = 60000000\n",
        );
        assert_eq!(ceiling_of(&pinned), Duration::from_secs(67));
    }

    #[test]
    fn zero_timings_are_read_as_the_worker_reads_them() {
        let zeros = |extra: &str| {
            profile(
                "T3",
                &format!("id_bits = 11\naddressing = \"normal\"\n[link.comparams]\n{extra}"),
            )
        };
        // P2* zero is the worker's 5 s: the same as leaving the default.
        assert_eq!(
            ceiling_of(&zeros("CP_P2Star = 0\n")),
            Duration::from_secs(33)
        );
        // P2 zero is the worker's 50 ms.
        assert_eq!(
            ceiling_of(&zeros("CP_P2Max = 0\n")),
            Duration::from_millis(50) + Duration::from_secs(5 + 25 + 2)
        );
        // A zero completion timeout is no ceiling in the worker: the handle's bound, far above
        // any finite one, and not a deadline of zero.
        let unbounded = ceiling_of(&zeros("CP_RC78CompletionTimeout = 0\n"));
        assert_eq!(unbounded, UNBOUNDED_COMPLETION_BOUND);
    }

    #[test]
    fn a_timing_change_grows_the_deadline_until_an_update_and_follows_it_after() {
        let t3 = &shapes()[2];
        let Plan { own, .. } = plan(link_of(t3), t3.class).unwrap();
        let initial = Timings::of(&own);
        let mut link = LinkTimings {
            active: initial,
            staged: initial,
        };
        let learned = Learned::default();
        let set = |name: &str, value: u32| {
            let item = ParamItem {
                id: Some(param_item::Id::ParamName(name.to_owned())),
                com_param_class: PduParamClass::PduPcSpecified as i32,
                param_data: Some(param_item::ParamData::Unum32(value)),
            };
            timing_change(&item, &learned).unwrap()
        };
        let (name, value) = set("CP_RC78CompletionTimeout", 60_000_000);
        assert_eq!(name, "cp_rc78completiontimeout");
        link.staged.rc78_completion = value;
        assert_eq!(link.ceiling(), Duration::from_secs(68));
        // A smaller staged value does not shorten the wait before the update.
        link.staged.rc78_completion = 1_000_000;
        link.staged.p2_max = set("cp_p2max", 3_000_000).1;
        assert_eq!(link.ceiling(), Duration::from_secs(33));
        link.active = link.staged;
        assert_eq!(link.ceiling(), Duration::from_secs(11));
        // Anything but the three timings is not a change.
        let other = ParamItem {
            id: Some(param_item::Id::ParamName("CP_BlockSize".to_owned())),
            param_data: Some(param_item::ParamData::Unum32(1)),
            ..ParamItem::default()
        };
        assert!(timing_change(&other, &learned).is_none());
    }

    #[test]
    fn the_session_reset_step_has_time_for_the_longest_wait() {
        let request = Duration::from_secs(5);
        let long = Duration::from_secs(68);
        assert!(session_reset_timeout(long, request, 0) > long + request * 2);
        // Each outstanding primitive and suspended queue is one more request before the return.
        assert!(session_reset_timeout(long, request, 4) > long + request * 6);
        // Saturating, not overflowing.
        assert!(session_reset_timeout(long, Duration::MAX, usize::MAX) >= long);
        assert!(session_reset_timeout(long, request, 0) > crate::teardown::DEFAULT_STEP_TIMEOUT);
        // A short wait keeps the teardown's default.
        let short = Duration::from_secs(3);
        assert_eq!(
            session_reset_timeout(short, request, 0),
            crate::teardown::DEFAULT_STEP_TIMEOUT
        );
    }

    #[test]
    fn an_unbounded_completion_timeout_caps_the_whole_wait() {
        // Large P2 and P2* do not add to the bound.
        let big = profile(
            "T3",
            "id_bits = 11\naddressing = \"normal\"\n[link.comparams]\nCP_P2Max = 4000000000\n\
             CP_P2Star = 4000000000\nCP_RC78CompletionTimeout = 0\n",
        );
        assert_eq!(ceiling_of(&big), UNBOUNDED_COMPLETION_BOUND);
        // A finite one is the sum, saturating rather than overflowing.
        let finite = profile(
            "T3",
            "id_bits = 11\naddressing = \"normal\"\n[link.comparams]\nCP_P2Max = 4000000000\n\
             CP_P2Star = 4000000000\nCP_RC78CompletionTimeout = 4000000000\n",
        );
        assert_eq!(ceiling_of(&finite), Duration::from_secs(12_002));
    }

    fn event(cop: Option<ComPrimitiveHandle>, data: event_item::Data) -> EventItem {
        EventItem {
            cop_handle: cop,
            timestamp: 0,
            data: Some(data),
            cop_tag: None,
        }
    }

    const COP: ComPrimitiveHandle = ComPrimitiveHandle {
        module_handle: 1,
        cll_handle: 10,
        cop_handle: 5,
    };

    #[test]
    fn a_lost_vci_ends_the_wait_whoever_it_is_bound_to() {
        let lost = PduErrorEvent::PduErrEvtLostCommToVci as i32;
        let other = ComPrimitiveHandle {
            cop_handle: 4,
            ..COP
        };
        let ended = |cop: Option<ComPrimitiveHandle>| {
            Progress::default().on_item(event(cop, event_item::Data::ErrorData(lost)), COP, None)
        };
        // For the whole link, with no primitive handle, or for this primitive.
        for cop in [None, Some(COP)] {
            assert!(matches!(
                ended(cop),
                Some(Outcome::Ended(Err(HandleError::VciLost)))
            ));
        }
        // Bound to an earlier primitive it is not this request's.
        assert!(ended(Some(other)).is_none());
        // Another link-wide error is not a lost VCI, and not this primitive's either.
        let tx = PduErrorEvent::PduErrEvtTxError as i32;
        assert!(
            Progress::default()
                .on_item(event(None, event_item::Data::ErrorData(tx)), COP, None)
                .is_none()
        );
    }

    #[test]
    fn a_primitive_ends_with_its_response_or_its_error() {
        let mut progress = Progress::default();
        let expected = Expectation::of(&[0x22, 0xF1, 0x90]);
        let on = |p: &mut Progress, data| p.on_item(event(Some(COP), data), COP, Some(&expected));
        assert!(
            on(
                &mut progress,
                event_item::Data::ResultData(ResultData {
                    data_bytes: vec![0x62, 0xF1, 0x90, 1],
                    ..ResultData::default()
                })
            )
            .is_none()
        );
        let done = on(
            &mut progress,
            event_item::Data::CopStatus(PduComPrimitiveStatus::PduCopstFinished as i32),
        );
        assert!(matches!(done, Some(Outcome::Ended(Ok(r))) if r == [0x62, 0xF1, 0x90, 1]));
        let mut failed = Progress::default();
        let tx = PduErrorEvent::PduErrEvtTxError as i32;
        assert!(on(&mut failed, event_item::Data::ErrorData(tx)).is_none());
        let done = on(
            &mut failed,
            event_item::Data::CopStatus(PduComPrimitiveStatus::PduCopstCancelled as i32),
        );
        assert!(matches!(
            done,
            Some(Outcome::Ended(Err(HandleError::PrimitiveFailed(c)))) if c == tx
        ));
    }

    #[test]
    fn a_requested_chain_adds_its_completion_timeout_when_its_handling_is_on() {
        let mut timings = Timings::default();
        assert_eq!(timings.ceiling(), Duration::from_secs(33));
        timings.set("cp_rc21handling", 1);
        timings.set("cp_rc21completiontimeout", 1_300_000);
        timings.set("cp_rc23handling", 2);
        // A zero completion timeout is the worker's 5 s.
        assert_eq!(
            timings.ceiling(),
            Duration::from_secs(33 + 1 + 5) + Duration::from_millis(300)
        );
        // With the handling off nothing is added.
        timings.set("cp_rc21handling", 0);
        timings.set("cp_rc23handling", 0);
        assert_eq!(timings.ceiling(), Duration::from_secs(33));
        let item = |name: &str| ParamItem {
            id: Some(param_item::Id::ParamName(name.to_owned())),
            param_data: Some(param_item::ParamData::Unum32(1)),
            ..ParamItem::default()
        };
        assert_eq!(
            timing_change(&item("CP_RC23Handling"), &Learned::default()),
            Some(("cp_rc23handling", 1))
        );
    }

    fn shared() -> Shared {
        let profile = &shapes()[2];
        let Setup::Link { vci, .. } = &profile.setup else {
            unreachable!()
        };
        Shared {
            profile: profile.clone(),
            vci: vci.clone(),
            module: 1,
            options: HandleOptions::default(),
            subscribes: true,
            state: Mutex::new(State::default()),
        }
    }

    #[test]
    fn a_session_change_in_flight_stays_pending_unless_the_worker_refused_it() {
        let cll = ComLogicalLinkHandle {
            module_handle: 1,
            cll_handle: 10,
        };
        let pending = |shared: &Shared| shared.lock().session_back.contains(&10);
        // Dropped mid-call (or answered, or timed out): the worker may hold the request.
        let shared = shared();
        drop(ProvisionalSession::new(&shared, cll));
        assert!(pending(&shared));
        // Refused by the worker: nothing happened.
        let shared = self::shared();
        let mut mark = ProvisionalSession::new(&shared, cll);
        assert!(pending(&shared));
        mark.refused = true;
        drop(mark);
        assert!(!pending(&shared));
        // A return that was already pending stays so after a refusal of a later change.
        let shared = self::shared();
        shared.lock().session_back.insert(10);
        let mut mark = ProvisionalSession::new(&shared, cll);
        mark.refused = true;
        drop(mark);
        assert!(pending(&shared));
    }

    #[test]
    fn only_the_statuses_a_worker_rejects_with_are_definite_refusals() {
        use tonic::Code::*;
        for code in [
            InvalidArgument,
            FailedPrecondition,
            NotFound,
            ResourceExhausted,
            Unimplemented,
            AlreadyExists,
            Unauthenticated,
        ] {
            assert!(is_definite_refusal(code), "{code:?}");
            assert!(matches!(
                worker_error("X", &tonic::Status::new(code, "no")),
                HandleError::WorkerRefused { .. }
            ));
        }
        for code in [
            Ok,
            Cancelled,
            Unknown,
            DeadlineExceeded,
            PermissionDenied,
            Aborted,
            OutOfRange,
            Internal,
            Unavailable,
            DataLoss,
        ] {
            assert!(!is_definite_refusal(code), "{code:?}");
            assert!(matches!(
                worker_error("X", &tonic::Status::new(code, "?")),
                HandleError::Rpc { .. }
            ));
        }
    }

    #[test]
    fn only_a_rejection_undoes_what_was_kept_in_case_the_worker_applied_it() {
        let cll = ComLogicalLinkHandle {
            module_handle: 1,
            cll_handle: 10,
        };
        let rejected = HandleError::WorkerRefused {
            call: "X",
            code: tonic::Code::NotFound,
            message: String::new(),
        };
        let ambiguous = [
            HandleError::Rpc {
                call: "X",
                code: tonic::Code::Unavailable,
                message: String::new(),
            },
            HandleError::Timeout {
                call: "X",
                timeout: Duration::from_secs(1),
            },
            HandleError::EventStreamEnded,
        ];
        assert!(undoes_request(&rejected));
        assert!(undoes_request(&HandleError::VciLost));
        assert!(ambiguous.iter().all(|e| !undoes_request(e)));
        // The session mark: kept for every ambiguous outcome, taken back for a rejection.
        for (error, kept) in
            std::iter::once((&rejected, false)).chain(ambiguous.iter().map(|e| (e, true)))
        {
            let shared = shared();
            let mut mark = ProvisionalSession::new(&shared, cll);
            mark.refused = undoes_request(error);
            drop(mark);
            assert_eq!(shared.lock().session_back.contains(&10), kept, "{error:?}");
        }
        // The saved resume follows the same rule (the io_ctl record is dropped only when
        // `undoes_request` says so).
        for (error, kept) in
            std::iter::once((&rejected, false)).chain(ambiguous.iter().map(|e| (e, true)))
        {
            let mut saved = vec![1];
            if undoes_request(error) {
                saved.pop();
            }
            assert_eq!(!saved.is_empty(), kept, "{error:?}");
        }
    }

    #[test]
    fn a_resume_covers_only_what_it_resumes() {
        use io_ctl_request::Handle::{CllHandle, ModuleHandle, SystemHandle};
        let link = |module, cll| {
            Some(CllHandle(ComLogicalLinkHandle {
                module_handle: module,
                cll_handle: cll,
            }))
        };
        let module = |module| {
            Some(ModuleHandle(ModuleHandle_ {
                module_handle: module,
            }))
        };
        assert!(resume_covers(&link(1, 10), &link(1, 10)));
        assert!(!resume_covers(&link(1, 10), &link(1, 11)));
        assert!(resume_covers(&module(1), &link(1, 11)));
        assert!(!resume_covers(&module(2), &link(1, 11)));
        assert!(!resume_covers(&link(1, 10), &module(1)));
        assert!(resume_covers(
            &Some(SystemHandle(Default::default())),
            &link(1, 10)
        ));
        // Two saved resumes, one link resumed: the other stays saved.
        let mut saved: Vec<Option<io_ctl_request::Handle>> = vec![link(1, 10), link(1, 11)];
        saved.retain(|s| !resume_covers(&link(1, 10), s));
        assert_eq!(saved, [link(1, 11)]);
    }

    #[test]
    fn the_device_close_step_follows_the_requests_it_makes() {
        let default = crate::teardown::DEFAULT_STEP_TIMEOUT;
        // A short budget keeps the default; a longer request timeout lengthens the step.
        assert_eq!(device_close_timeout(Duration::from_secs(5)), default);
        let long = Duration::from_secs(90);
        assert_eq!(device_close_timeout(long), long + Duration::from_secs(1));
        assert_eq!(device_close_timeout(Duration::MAX), Duration::MAX);
    }

    #[test]
    fn a_frame_is_an_echo_when_its_rx_flag_has_a_tx_bit() {
        // `TX_MSG_TYPE` and `TX_INDICATION` are the last byte's bits 0 and 3.
        assert!(is_echo(&[0, 0, 0, 0x01]));
        assert!(is_echo(&[0, 0, 0, 0x08]));
        assert!(is_echo(&[0, 0, 0, 0x09]));
        // Other bits (start of message, break) and an empty flag are not echoes.
        assert!(!is_echo(&[0, 0, 0, 0x02]));
        assert!(!is_echo(&[0, 0, 0, 0x04]));
        assert!(!is_echo(&[]));
    }

    #[test]
    fn the_close_step_has_time_for_its_requests() {
        let request = Duration::from_secs(20);
        assert!(close_link_timeout(request, 0) > request * 3);
        assert!(close_link_timeout(request, 2) > request * 5);
        assert_eq!(
            close_link_timeout(Duration::from_secs(1), 0),
            crate::teardown::DEFAULT_STEP_TIMEOUT
        );
        // Saturating, not overflowing.
        assert_eq!(close_link_timeout(Duration::MAX, usize::MAX), Duration::MAX);
    }

    #[test]
    fn a_read_of_several_dids_matches_the_service_only() {
        let positive = |request: &[u8]| {
            Expectation::of(request).descriptors()[0]
                .pattern_data
                .clone()
        };
        assert_eq!(positive(&[0x22, 0xF1, 0x90]), [0x62, 0xF1, 0x90]);
        assert_eq!(positive(&[0x22, 0xF1, 0x90, 0xF1, 0x91]), [0x62]);
        // Records in the other order still answer the read.
        assert!(
            Expectation::of(&[0x22, 0xF1, 0x90, 0xF1, 0x91])
                .answers(&[0x62, 0xF1, 0x91, 1, 0xF1, 0x90, 2])
        );
    }

    #[test]
    fn every_comparam_is_sent_as_specified_so_the_worker_resolves_its_class() {
        for profile in shapes() {
            let Plan { own, table } = plan(link_of(&profile), profile.class).unwrap();
            assert!(
                own.iter()
                    .all(|(_, _, class)| *class == PduParamClass::PduPcSpecified)
            );
            assert!(!table.is_empty());
        }
    }

    #[test]
    fn the_format_value_follows_width_and_addressing() {
        let format = |link: &str| {
            let p = profile("T3", link);
            format_value(link_of(&p))
        };
        assert_eq!(format("id_bits = 11\naddressing = \"normal\"\n"), 0x04);
        assert_eq!(format("id_bits = 29\naddressing = \"normal\"\n"), 0x06);
        assert_eq!(
            format("id_bits = 11\naddressing = \"extended\"\naddress = 1\n"),
            0x0C
        );
        assert_eq!(
            format("id_bits = 29\naddressing = \"mixed\"\naddress = 1\n"),
            0x4E
        );
    }

    #[test]
    fn a_positive_response_begins_as_the_request_says() {
        let answers = |request: &[u8], response: &[u8]| Expectation::of(request).answers(response);
        assert!(answers(&[0x22, 0xF1, 0x90], &[0x62, 0xF1, 0x90, 1]));
        assert!(!answers(&[0x22, 0xF1, 0x90], &[0x62, 0xF1, 0x91, 1]));
        // The suppress-response bit is not echoed.
        assert!(answers(&[0x3E, 0x80], &[0x7E, 0x00]));
        assert!(answers(&[0x10, 0x03], &[0x50, 0x03, 0, 0x32, 1, 0xF4]));
        // A negative response to this service ends it, a response pending does not.
        assert!(answers(&[0x22, 0xF1, 0x90], &[0x7F, 0x22, 0x31]));
        assert!(!answers(&[0x22, 0xF1, 0x90], &[0x7F, 0x22, 0x78]));
        assert!(!answers(&[0x22, 0xF1, 0x90], &[0x7F, 0x19, 0x31]));
    }

    #[test]
    fn the_resource_names_the_two_pins_and_the_channel_when_there_is_one() {
        let profile = profile("T3", "id_bits = 11\naddressing = \"normal\"\n");
        let link = link_of(&profile);
        let resource = resource_for(link, None);
        assert_eq!(
            resource.protocol,
            Some(resource_data::Protocol::ProtocolName(
                "ISO_14229_3_on_ISO_15765_2".to_owned()
            ))
        );
        assert_eq!(resource.dlc_pin_data.len(), 2);
        let channel = resource_for(link, Some("CAN_CH2"));
        assert_eq!(
            channel.protocol,
            Some(resource_data::Protocol::ProtocolName("CAN_CH2".to_owned()))
        );
    }
}
