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
    DisconnectComLogicalLinkRequest, EcuUniqueRespData, EventNotification, ExpectedResponseData,
    GetComParamRequest, GetConflictingResourcesRequest, GetModuleIdsRequest, GetObjectIdRequest,
    GetResourceIdsRequest, GetResourceStatusRequest, GetStatusRequest, GetTimestampRequest,
    GetUniqueRespIdTableRequest, GetVersionRequest, IoCtlRequest, IoCtlResponse,
    LockResourceRequest, ModuleDisconnectRequest, ModuleHandle, ModuleIdsResponse,
    ObjectIdResponse, ObjectType, ParamItem, PduComPrimitiveStatus, PduErrorEvent, PduParamClass,
    PinData, ResourceData, ResourceIdsResponse, ResourceStatusResponse, SetComParamRequest,
    SetUniqueRespIdTableRequest, StartComPrimitiveRequest, StatusResponse, SubscribeEventRequest,
    TimestampResponse, UniqueRespIdTableItem, UniqueRespIdTableResponse, UnlockResourceRequest,
    VersionResponse, create_com_logical_link_request, event_item, event_notification, param_item,
    pin_data, resource_data, subscribe_event_request,
};
use worker_host::client::WorkerClient;

use super::link::{BUS_TYPE, PIN_TYPES, required, table_required};
use super::{BoundLink, Context, Grant, GuardRefusal, Learned, LinkTable, Request, decide};
use crate::comparam::canonical;
use crate::open::OpenedDevice;
use crate::profile::{Addressing, Class, Link, Profile, Vci};
use crate::report::RunGuard;
use crate::teardown::TeardownHandle;

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
    /// A worker request failed; `call` names it.
    #[error("{call} failed: {message}")]
    Rpc { call: &'static str, message: String },
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

type Events = Arc<AsyncMutex<Streaming<EventNotification>>>;
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
}

/// The part of the handle the teardown steps share.
struct Shared {
    profile: Profile,
    vci: Vci,
    module: u32,
    options: HandleOptions,
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is changed in short steps that cannot panic midway, so a poisoned lock holds
        // consistent data.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The guard's decision on `request`, over the current state.
    fn admit(&self, request: &Request<'_>) -> Result<Grant, HandleError> {
        let state = self.lock();
        let ctx = Context {
            profile: &self.profile,
            module: self.module,
            vci: &self.vci,
            links: &state.links,
            learned: &state.learned,
        };
        decide(&ctx, request).map_err(|refusal| HandleError::Refused {
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
            Ok(Err(status)) => Err(HandleError::Rpc {
                call,
                message: status.to_string(),
            }),
            Ok(Ok(response)) => Ok(response.into_inner()),
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
        let events = self
            .lock()
            .events
            .get(&cll.cll_handle)
            .cloned()
            .ok_or_else(|| HandleError::Setup("the link has no event stream".to_owned()))?;
        let timeout = self.wait_for(cll);
        let timeout = cap.map_or(timeout, |cap| timeout.min(cap));
        let expected = Expectation::of(request);
        let mut response = None;
        let mut error = None;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut stream = events.lock().await;
        loop {
            let notification = match tokio::time::timeout_at(deadline, stream.message()).await {
                Ok(notification) => notification,
                Err(_) => {
                    // Nothing is left running after the caller gave up; a failed cancel (the
                    // primitive ended meanwhile) changes nothing.
                    let cancel = CancelComPrimitiveRequest {
                        cop_handle: Some(cop),
                    };
                    if self.admit(&Request::CancelComPrimitive(&cancel)).is_ok() {
                        let _ = self
                            .send("CancelComPrimitive", client.cancel_com_primitive(cancel))
                            .await;
                    }
                    return Err(HandleError::NoResponse);
                }
            }
            .map_err(|status| HandleError::Rpc {
                call: "SubscribeEvent",
                message: status.to_string(),
            })?
            .ok_or(HandleError::EventStreamEnded)?;
            let item = match notification.event_data {
                Some(event_notification::EventData::Item(item)) => item,
                Some(event_notification::EventData::Lost(_)) => {
                    return Err(HandleError::EventsLost);
                }
                None => continue,
            };
            // Events of earlier primitives, and of none, are not this request's.
            if item.cop_handle != Some(cop) {
                continue;
            }
            match item.data {
                Some(event_item::Data::ResultData(result))
                    if expected.answers(&result.data_bytes) =>
                {
                    response = Some(result.data_bytes);
                }
                // The ECU did not answer: reported as no response when the primitive ends.
                Some(event_item::Data::ErrorData(code))
                    if code == PduErrorEvent::PduErrEvtRxTimeout as i32 => {}
                Some(event_item::Data::ErrorData(code)) => error = Some(code),
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
                        || status == PduComPrimitiveStatus::PduCopstCancelled as i32 =>
                {
                    return match (response, error) {
                        (Some(response), _) => Ok(response),
                        (None, Some(code)) => Err(HandleError::PrimitiveFailed(code)),
                        (None, None) => Err(HandleError::NoResponse),
                    };
                }
                _ => {}
            }
        }
    }
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

/// The teardown step of a link: disconnects and destroys it, unless the check destroyed it.
async fn close_link(
    shared: Arc<Shared>,
    mut client: WorkerClient,
    cll: ComLogicalLinkHandle,
) -> Result<(), String> {
    if !shared.lock().closing.remove(&cll.cll_handle) {
        return Ok(());
    }
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
/// start request and the cancel request, and a margin; never less than the teardown's default.
fn session_reset_timeout(wait: Duration, request_timeout: Duration) -> Duration {
    (wait + request_timeout * 2 + Duration::from_secs(1)).max(crate::teardown::DEFAULT_STEP_TIMEOUT)
}

/// The teardown step that returns the ECU on `cll` to the default session, once.
async fn return_to_default_session(
    shared: Arc<Shared>,
    mut client: WorkerClient,
    cll: ComLogicalLinkHandle,
    wait: Duration,
) -> Result<(), String> {
    {
        let mut state = shared.lock();
        if !state.session_back.remove(&cll.cll_handle) || !state.links.contains_key(&cll.cll_handle)
        {
            return Ok(());
        }
    }
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
    run.await.map(drop).map_err(|e| e.to_string())
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
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            p2_max: 1_000_000,
            p2_star: 5_000_000,
            rc78_completion: 25_000_000,
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
        micros(self.p2_max, Duration::from_millis(50))
            + micros(self.p2_star, Duration::from_secs(5))
            + micros(self.rc78_completion, UNBOUNDED_COMPLETION_BOUND)
            + Duration::from_secs(2)
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
    let name = ["cp_p2max", "cp_p2star", "cp_rc78completiontimeout"]
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
/// It borrows the run's teardown, so the check ends its use of the handle (by dropping it)
/// before it records its result on the [`RunGuard`].
pub struct GuardedHandle<'r> {
    client: WorkerClient,
    shared: Arc<Shared>,
    teardown: TeardownHandle<'r>,
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

impl<'r> GuardedHandle<'r> {
    /// Opens the profile's links on a device `open_device` opened and matched with `vci`, and
    /// returns the handle. Registers the device's closing (module disconnect, worker stop) with
    /// `run`, then each link's, before any of them is used. On an error the steps registered so
    /// far stay with `run` and run when the check ends.
    pub async fn open(
        profile: &Profile,
        vci: &Vci,
        device: OpenedDevice,
        options: HandleOptions,
        run: &'r mut RunGuard,
    ) -> Result<GuardedHandle<'r>, HandleError> {
        let client = device.client();
        let module = device.module_handle();
        let close: Step = Box::pin(async move {
            device
                .close()
                .await
                .map(drop)
                .map_err(|error| error.to_string())
        });
        Self::start(profile, vci, client, module, options, run, close).await
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
        run: &'r mut RunGuard,
    ) -> Result<GuardedHandle<'r>, HandleError> {
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
        Self::start(profile, vci, client, module, options, run, close).await
    }

    async fn start(
        profile: &Profile,
        vci: &Vci,
        client: WorkerClient,
        module: ModuleHandle,
        options: HandleOptions,
        run: &'r mut RunGuard,
        close_device: Step,
    ) -> Result<GuardedHandle<'r>, HandleError> {
        let mut teardown = run.teardown();
        teardown.register_async("close the device", close_device);
        let mut handle = GuardedHandle {
            client,
            shared: Arc::new(Shared {
                profile: profile.clone(),
                vci: vci.clone(),
                module: module.module_handle,
                options,
                state: Mutex::new(State::default()),
            }),
            teardown,
            links: Vec::new(),
        };
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
        let stream = self
            .shared
            .send(
                "SubscribeEvent",
                self.client.clone().subscribe_event(SubscribeEventRequest {
                    handle: Some(subscribe_event_request::Handle::CllHandle(cll)),
                }),
            )
            .await?;
        self.shared
            .lock()
            .events
            .insert(cll.cll_handle, Arc::new(AsyncMutex::new(stream)));
        Ok(())
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
            self.teardown.register_async(
                format!("close link {}", cll.cll_handle),
                close_link(self.shared.clone(), self.client.clone(), cll),
            );
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
            self.subscribe(cll).await?;
        }
        Ok(response)
    }

    /// `DestroyComLogicalLink`; the link is dropped from the table.
    pub async fn destroy_com_logical_link(
        &mut self,
        request: DestroyComLogicalLinkRequest,
    ) -> Result<vci_service_interface::Response, HandleError> {
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
            match name {
                "cp_p2max" => timings.staged.p2_max = value,
                "cp_p2star" => timings.staged.p2_star = value,
                _ => timings.staged.rc78_completion = value,
            }
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
        if let (Grant::EntersSession(_), Some(cll)) = (&grant, cll)
            && self.shared.lock().session_back.insert(cll.cll_handle)
        {
            // The step waits no longer than the link's response wait is now, and has the time
            // for that, so it ends by itself and cancels instead of being dropped.
            let wait = self.shared.wait_for(cll);
            self.teardown.register_async_with_timeout(
                format!("return link {} to the default session", cll.cll_handle),
                session_reset_timeout(wait, self.shared.options.request_timeout),
                return_to_default_session(self.shared.clone(), self.client.clone(), cll, wait),
            );
        }
        let updates = request.cop_type == ComOperationType::CoptUpdateparam as i32;
        let response = self
            .shared
            .send(
                "StartComPrimitive",
                self.client.clone().start_com_primitive(request),
            )
            .await?;
        if updates
            && let Some(cll) = cll
            && let Some(timings) = self.shared.lock().timings.get_mut(&cll.cll_handle)
        {
            timings.active = timings.staged;
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
        let cop = self
            .start_com_primitive(send_receive_request(cll, request))
            .await?
            .cop_handle
            .ok_or_else(|| {
                HandleError::Setup("StartComPrimitive returned no primitive handle".to_owned())
            })?;
        self.shared
            .await_result(&mut self.client.clone(), cll, cop, request, None)
            .await
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
        /// `ConnectComLogicalLink`, which the guard allows once the profile's ComParams are set.
        connect_com_logical_link(ConnectComLogicalLink, ConnectComLogicalLinkRequest) -> vci_service_interface::Response = "ConnectComLogicalLink";
        /// `DisconnectComLogicalLink`.
        disconnect_com_logical_link(DisconnectComLogicalLink, DisconnectComLogicalLinkRequest) -> vci_service_interface::Response = "DisconnectComLogicalLink";
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
        /// `IoCtl`, of the IO controls the guard lets through.
        io_ctl(IoCtl, IoCtlRequest) -> IoCtlResponse = "IoCtl";
        /// `GetUniqueRespIdTable`.
        get_unique_resp_id_table(GetUniqueRespIdTable, GetUniqueRespIdTableRequest) -> UniqueRespIdTableResponse = "GetUniqueRespIdTable";
    }
}

#[cfg(test)]
mod tests {
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
             [endpoint.link]\nprotocol = \"ISO_14229_3_on_ISO_15765_2\"\nbaud_rate = 500000\n\
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
        assert_eq!(
            unbounded,
            UNBOUNDED_COMPLETION_BOUND + Duration::from_secs(1 + 5 + 2)
        );
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
        assert!(session_reset_timeout(long, request) > long + request * 2);
        assert!(session_reset_timeout(long, request) > crate::teardown::DEFAULT_STEP_TIMEOUT);
        // A short wait keeps the teardown's default.
        let short = Duration::from_secs(3);
        assert_eq!(
            session_reset_timeout(short, request),
            crate::teardown::DEFAULT_STEP_TIMEOUT
        );
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
