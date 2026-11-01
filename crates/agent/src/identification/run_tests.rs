//! `run_identification` against a scripted worker: a gRPC server on a loopback port that answers
//! the link's RPCs, refuses what a test tells it to, and gives each started primitive the
//! events its script lists (ADR-396).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diag_ir::plan::Sha256;
use vci_service_interface::event_item::Data;
use vci_service_interface::event_notification::EventData;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveHandle, EventItem, EventNotification, ExtraInfo,
    ModuleHandle, PduComPrimitiveStatus, PduErrorEvent, ResultData,
};
use worker_host::client::WorkerClient;

use super::tests::{
    CandidateEntries, CandidateFieldRule, checked, checked_vin, checked_vin_reading, document,
    document_reading, ecu_plan, ecu_plan_of, ecu_plan_ruled, ecu_plan_with_rules, edited,
    identified_by, named, two_field_service, with_service,
};
use super::*;
use crate::comm_log::{CommLogSetup, VciFacts};
use crate::document::tests::read_service;
use crate::guards::GuardSetup;
use diag_ir::declaration::DataKind;

/// One event a started primitive gives, in order.
#[derive(Clone, Debug)]
enum Item {
    /// A result bound to the primitive, on the response identifier in its header.
    Answer(u32, &'static [u8]),
    /// A result bound to an earlier primitive: a late answer.
    Late(u32, &'static [u8]),
    /// A result bound to the primitive, on the response identifier, with owned bytes.
    Reply(u32, Vec<u8>),
    /// Answer the primitive as the simulated ECU of the link's request identifier would, if the
    /// fake has ECUs (`State::ecus`); the script of a primitive that has none is this when the
    /// script queue is empty and the link is physical.
    Routed,
    /// The primitive's finished status.
    Finish,
    /// The worker reports lost events.
    LostEvents,
    /// A receive error bound to the primitive.
    RxError,
    /// The worker reports the VCI lost: an error of no primitive.
    VciLost,
}

type Sender = tokio::sync::mpsc::UnboundedSender<Result<EventNotification, tonic::Status>>;

#[derive(Default)]
struct State {
    /// The worker's answer to the receive table: refuse a table of more pairs than this.
    refuse_tables_over: Option<usize>,
    /// The status every table call fails with, a refusal or not.
    table_status: Option<tonic::Status>,
    /// The statuses the first table calls fail with, one per call, ahead of `table_status`.
    table_script: VecDeque<tonic::Status>,
    /// What the first `DestroyComLogicalLink` calls answer (`None`: success), one per call.
    destroy_script: VecDeque<Option<tonic::Status>>,
    /// How many of the first `ModuleConnect` and `ModuleDisconnect` calls hang.
    hang_connect: usize,
    hang_disconnect: usize,
    /// The statuses the first `ModuleConnect` and `ModuleDisconnect` calls fail with.
    connect_script: VecDeque<tonic::Status>,
    disconnect_script: VecDeque<tonic::Status>,
    /// The status `CreateComLogicalLink` fails with.
    create_status: Option<tonic::Status>,
    /// The n-th (from 1) `CreateComLogicalLink`, `ConnectComLogicalLink` and `ModuleDisconnect`
    /// call alone fails with the status, or (the last) hangs.
    create_fail_at: Option<(usize, tonic::Status)>,
    connect_fail_at: Option<(usize, tonic::Status)>,
    hang_disconnect_at: Option<usize>,
    /// A `ModuleDisconnect` hung (it is hanging now, or was).
    disconnect_hung: bool,
    /// The statuses the first `ConnectComLogicalLink` calls fail with, one per call.
    link_connect_script: VecDeque<tonic::Status>,
    /// The events of the n-th started primitive; one that has no script finishes at once.
    scripts: VecDeque<Vec<Item>>,
    calls: Vec<&'static str>,
    tables: Vec<Vec<(u32, u32)>>,
    starts: Vec<(Vec<u8>, i32)>,
    set: Vec<(String, u32)>,
    comparams: Vec<String>,
    open_links: usize,
    max_open_links: usize,
    cops: u32,
    events: Option<Sender>,
    /// Simulated ECUs by physical request identifier (the link's table of one pair). Each physical primitive whose script is
    /// [`Item::Routed`] (or that has none, once the script queue is empty) is answered by the
    /// ECU of the link's request identifier, with the response identifier the link was set to;
    /// a link of an identifier without an ECU, and a functional link, get no answer.
    ecus: HashMap<u32, sim_ecu::SimEcu>,
    /// The request identifiers and the requests the ECUs were given, in order.
    served: Vec<(u32, Vec<u8>)>,
    /// What the link being opened was set to.
    current: LinkIds,
    /// Sets the flag when the n-th primitive (from 1) starts.
    cancel_at_start: Option<(u32, Arc<AtomicBool>)>,
}

/// What the link being opened was set to: the pair of its receive table when it has one, and
/// whether it is functional.
#[derive(Default)]
struct LinkIds {
    request: Option<u32>,
    response: Option<u32>,
    functional: bool,
}

impl State {
    /// The events of the simulated ECU's answer to `request` on the current link, if it is a
    /// physical link of an identifier that has an ECU.
    fn route(&mut self, request: &[u8]) -> Option<Vec<Item>> {
        let (tx, rx) = (self.current.request?, self.current.response?);
        if self.current.functional {
            return None;
        }
        let ecu = self.ecus.get_mut(&tx)?;
        self.served.push((tx, request.to_vec()));
        let answer = ecu.request_with(sim_ecu::Addressing::Physical, request);
        let mut items: Vec<Item> = answer
            .to_bytes()
            .map(|bytes| Item::Reply(rx, bytes))
            .into_iter()
            .collect();
        items.push(Item::Finish);
        Some(items)
    }
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<State>>);

impl Fake {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }
    fn note(&self, rpc: &'static str) {
        self.state().calls.push(rpc);
    }
    fn count(&self, rpc: &str) -> usize {
        self.state()
            .calls
            .iter()
            .filter(|call| **call == rpc)
            .count()
    }
}

/// Takes one of `left` hanging calls, if any.
fn hangs(left: &mut usize) -> bool {
    let hangs = *left > 0;
    *left = left.saturating_sub(1);
    hangs
}

fn unsupported<T>() -> Result<tonic::Response<T>, tonic::Status> {
    Err(tonic::Status::unimplemented("not part of the fake"))
}

fn ok<T: Default>() -> Result<tonic::Response<T>, tonic::Status> {
    Ok(tonic::Response::new(T::default()))
}

#[tonic::codegen::async_trait]
impl vci_service_interface::vci_service_server::VciService for Fake {
    async fn get_module_ids(
        &self,
        _: tonic::Request<vci_service_interface::GetModuleIdsRequest>,
    ) -> Result<tonic::Response<vci_service_interface::ModuleIdsResponse>, tonic::Status> {
        self.note("GetModuleIds");
        Ok(tonic::Response::new(
            vci_service_interface::ModuleIdsResponse {
                module_id_list: Some(vci_service_interface::ModuleItem {
                    module_data: vec![vci_service_interface::ModuleData {
                        module_handle: Some(ModuleHandle { module_handle: 1 }),
                        ..Default::default()
                    }],
                }),
            },
        ))
    }
    async fn module_connect(
        &self,
        _: tonic::Request<vci_service_interface::ModuleConnectRequest>,
    ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
        self.note("ModuleConnect");
        if hangs(&mut self.state().hang_connect) {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        match self.state().connect_script.pop_front() {
            Some(status) => Err(status),
            None => ok(),
        }
    }
    async fn module_disconnect(
        &self,
        _: tonic::Request<vci_service_interface::ModuleDisconnectRequest>,
    ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
        self.note("ModuleDisconnect");
        let nth = self.count("ModuleDisconnect");
        if self.state().hang_disconnect_at == Some(nth) {
            self.state().disconnect_hung = true;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        if hangs(&mut self.state().hang_disconnect) {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        match self.state().disconnect_script.pop_front() {
            Some(status) => Err(status),
            None => ok(),
        }
    }
    async fn get_version(
        &self,
        _: tonic::Request<vci_service_interface::GetVersionRequest>,
    ) -> Result<tonic::Response<vci_service_interface::VersionResponse>, tonic::Status> {
        unsupported()
    }
    async fn get_timestamp(
        &self,
        _: tonic::Request<vci_service_interface::GetTimestampRequest>,
    ) -> Result<tonic::Response<vci_service_interface::TimestampResponse>, tonic::Status> {
        unsupported()
    }
    async fn get_resource_status(
        &self,
        _: tonic::Request<vci_service_interface::GetResourceStatusRequest>,
    ) -> Result<tonic::Response<vci_service_interface::ResourceStatusResponse>, tonic::Status> {
        unsupported()
    }
    async fn get_resource_ids(
        &self,
        _: tonic::Request<vci_service_interface::GetResourceIdsRequest>,
    ) -> Result<tonic::Response<vci_service_interface::ResourceIdsResponse>, tonic::Status> {
        Ok(tonic::Response::new(
            vci_service_interface::ResourceIdsResponse {
                resource_id_list: Some(vci_service_interface::ResourceIdItem {
                    resource_id_data_array: vec![vci_service_interface::ResourceIdItemData {
                        module_handle: None,
                        resource_id_array: vec![1],
                    }],
                }),
            },
        ))
    }
    async fn get_conflicting_resources(
        &self,
        _: tonic::Request<vci_service_interface::GetConflictingResourcesRequest>,
    ) -> Result<tonic::Response<vci_service_interface::ConflictingResourcesResponse>, tonic::Status>
    {
        unsupported()
    }
    async fn create_com_logical_link(
        &self,
        _: tonic::Request<vci_service_interface::CreateComLogicalLinkRequest>,
    ) -> Result<tonic::Response<vci_service_interface::ComLogicalLinkResponse>, tonic::Status> {
        self.note("CreateComLogicalLink");
        let nth = self.count("CreateComLogicalLink");
        let mut state = self.state();
        if let Some((_, status)) = state.create_fail_at.clone().filter(|(at, _)| *at == nth) {
            return Err(status);
        }
        if let Some(status) = state.create_status.clone() {
            return Err(status);
        }
        state.open_links += 1;
        state.max_open_links = state.max_open_links.max(state.open_links);
        state.current = LinkIds::default();
        Ok(tonic::Response::new(
            vci_service_interface::ComLogicalLinkResponse {
                cll_handle: Some(ComLogicalLinkHandle {
                    module_handle: 1,
                    cll_handle: 1,
                }),
                pending_forwarding: vci_service_interface::PendingForwarding::Unspecified as i32,
            },
        ))
    }
    async fn destroy_com_logical_link(
        &self,
        _: tonic::Request<vci_service_interface::DestroyComLogicalLinkRequest>,
    ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
        self.note("DestroyComLogicalLink");
        let mut state = self.state();
        state.open_links = state.open_links.saturating_sub(1);
        match state.destroy_script.pop_front().flatten() {
            Some(status) => Err(status),
            None => ok(),
        }
    }
    async fn connect_com_logical_link(
        &self,
        _: tonic::Request<vci_service_interface::ConnectComLogicalLinkRequest>,
    ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
        self.note("ConnectComLogicalLink");
        let nth = self.count("ConnectComLogicalLink");
        if let Some((_, status)) = self
            .state()
            .connect_fail_at
            .clone()
            .filter(|(at, _)| *at == nth)
        {
            return Err(status);
        }
        match self.state().link_connect_script.pop_front() {
            Some(status) => Err(status),
            None => ok(),
        }
    }
    async fn disconnect_com_logical_link(
        &self,
        _: tonic::Request<vci_service_interface::DisconnectComLogicalLinkRequest>,
    ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
        self.note("DisconnectComLogicalLink");
        ok()
    }
    async fn lock_resource(
        &self,
        _: tonic::Request<vci_service_interface::LockResourceRequest>,
    ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
        unsupported()
    }
    async fn unlock_resource(
        &self,
        _: tonic::Request<vci_service_interface::UnlockResourceRequest>,
    ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
        unsupported()
    }
    async fn get_com_param(
        &self,
        _: tonic::Request<vci_service_interface::GetComParamRequest>,
    ) -> Result<tonic::Response<vci_service_interface::ComParamResponse>, tonic::Status> {
        unsupported()
    }
    async fn set_com_param(
        &self,
        request: tonic::Request<vci_service_interface::SetComParamRequest>,
    ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
        use vci_service_interface::param_item::{Id, ParamData};
        let item = request.into_inner().param_item.unwrap_or_default();
        if let (Some(Id::ParamId(id)), Some(ParamData::Unum32(value))) = (item.id, item.param_data)
        {
            let mut state = self.state();
            let name = state.comparams[usize::try_from(id - 1).unwrap()].clone();
            if name == "cp_canfuncreqid" {
                state.current.functional = true;
            }
            state.set.push((name, value));
        }
        ok()
    }
    async fn start_com_primitive(
        &self,
        request: tonic::Request<vci_service_interface::StartComPrimitiveRequest>,
    ) -> Result<tonic::Response<vci_service_interface::ComPrimitiveResponse>, tonic::Status> {
        self.note("StartComPrimitive");
        let request = request.into_inner();
        let mut state = self.state();
        let cycles = request
            .cop_ctrl_data
            .map_or(0, |ctrl| ctrl.num_receive_cycles);
        state.starts.push((request.cop_data.clone(), cycles));
        state.cops += 1;
        let cop_handle = 100 + state.cops;
        if let Some((nth, flag)) = &state.cancel_at_start
            && *nth == state.cops
        {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let script = match state.scripts.pop_front() {
            Some(script) if !matches!(script.as_slice(), [Item::Routed]) => script,
            _ => state
                .route(&request.cop_data)
                .unwrap_or_else(|| vec![Item::Finish]),
        };
        let handle = ComPrimitiveHandle {
            module_handle: 1,
            cll_handle: 1,
            cop_handle,
        };
        let sender = state.events.clone();
        for item in script {
            let (cop, data) = match item {
                Item::Answer(id, bytes) => (Some(handle), result(id, bytes)),
                Item::Reply(id, bytes) => (Some(handle), result(id, &bytes)),
                Item::Routed => continue,
                Item::Late(id, bytes) => (
                    Some(ComPrimitiveHandle {
                        cop_handle: cop_handle - 1,
                        ..handle
                    }),
                    result(id, bytes),
                ),
                Item::Finish => (
                    Some(handle),
                    Data::CopStatus(PduComPrimitiveStatus::PduCopstFinished as i32),
                ),
                Item::RxError => (
                    Some(handle),
                    Data::ErrorData(vci_service_interface::ErrorData {
                        error_event: PduErrorEvent::PduErrEvtRxError as i32,
                        ..Default::default()
                    }),
                ),
                Item::VciLost => (
                    None,
                    Data::ErrorData(vci_service_interface::ErrorData {
                        error_event: PduErrorEvent::PduErrEvtLostCommToVci as i32,
                        ..Default::default()
                    }),
                ),
                Item::LostEvents => {
                    if let Some(sender) = &sender {
                        let _ = sender.send(Ok(EventNotification {
                            event_data: Some(EventData::Lost(Default::default())),
                            ..Default::default()
                        }));
                    }
                    continue;
                }
            };
            if let Some(sender) = &sender {
                let _ = sender.send(Ok(EventNotification {
                    event_data: Some(EventData::Item(EventItem {
                        cop_handle: cop,
                        timestamp: 1_000,
                        data: Some(data),
                        ..Default::default()
                    })),
                    ..Default::default()
                }));
            }
        }
        Ok(tonic::Response::new(
            vci_service_interface::ComPrimitiveResponse {
                cop_handle: Some(handle),
            },
        ))
    }
    async fn cancel_com_primitive(
        &self,
        _: tonic::Request<vci_service_interface::CancelComPrimitiveRequest>,
    ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
        self.note("CancelComPrimitive");
        ok()
    }
    async fn get_status(
        &self,
        _: tonic::Request<vci_service_interface::GetStatusRequest>,
    ) -> Result<tonic::Response<vci_service_interface::StatusResponse>, tonic::Status> {
        unsupported()
    }
    async fn get_event_item(
        &self,
        _: tonic::Request<vci_service_interface::GetEventItemRequest>,
    ) -> Result<tonic::Response<vci_service_interface::EventItemResponse>, tonic::Status> {
        unsupported()
    }
    type SubscribeEventStream = tonic::codegen::tokio_stream::wrappers::UnboundedReceiverStream<
        Result<EventNotification, tonic::Status>,
    >;
    async fn subscribe_event(
        &self,
        _: tonic::Request<vci_service_interface::SubscribeEventRequest>,
    ) -> Result<tonic::Response<Self::SubscribeEventStream>, tonic::Status> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.state().events = Some(sender);
        Ok(tonic::Response::new(
            tonic::codegen::tokio_stream::wrappers::UnboundedReceiverStream::new(receiver),
        ))
    }
    async fn io_ctl(
        &self,
        _: tonic::Request<vci_service_interface::IoCtlRequest>,
    ) -> Result<tonic::Response<vci_service_interface::IoCtlResponse>, tonic::Status> {
        unsupported()
    }
    async fn get_object_id(
        &self,
        request: tonic::Request<vci_service_interface::GetObjectIdRequest>,
    ) -> Result<tonic::Response<vci_service_interface::ObjectIdResponse>, tonic::Status> {
        let name = request.into_inner().shortname.to_ascii_lowercase();
        let mut state = self.state();
        let at = state.comparams.iter().position(|known| *known == name);
        let at = at.unwrap_or_else(|| {
            state.comparams.push(name);
            state.comparams.len() - 1
        });
        Ok(tonic::Response::new(
            vci_service_interface::ObjectIdResponse {
                pdu_object_id: u32::try_from(at + 1).unwrap(),
            },
        ))
    }
    async fn get_unique_resp_id_table(
        &self,
        _: tonic::Request<vci_service_interface::GetUniqueRespIdTableRequest>,
    ) -> Result<tonic::Response<vci_service_interface::UniqueRespIdTableResponse>, tonic::Status>
    {
        unsupported()
    }
    async fn set_unique_resp_id_table(
        &self,
        request: tonic::Request<vci_service_interface::SetUniqueRespIdTableRequest>,
    ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
        use vci_service_interface::param_item::ParamData;
        self.note("SetUniqueRespIdTable");
        let value = |item: &vci_service_interface::ParamItem| match item.param_data {
            Some(ParamData::Unum32(value)) => value,
            _ => 0,
        };
        let table: Vec<(u32, u32)> = request
            .into_inner()
            .unique_resp_id_table
            .unwrap_or_default()
            .unique_data
            .iter()
            .map(|entry| (value(&entry.params[0]), value(&entry.params[1])))
            .collect();
        let mut state = self.state();
        let len = table.len();
        // A physical link's table is its one pair, which routes its primitives to an ECU.
        if let [(request, response)] = table[..] {
            state.current.request = Some(request);
            state.current.response = Some(response);
        }
        state.tables.push(table);
        if let Some(status) = state.table_script.pop_front() {
            return Err(status);
        }
        if let Some(status) = state.table_status.clone() {
            return Err(status);
        }
        if state.refuse_tables_over.is_some_and(|most| len > most) {
            return Err(tonic::Status::invalid_argument("too many entries"));
        }
        ok()
    }
}

fn result(id: u32, bytes: &[u8]) -> Data {
    Data::ResultData(ResultData {
        extra_info: Some(ExtraInfo {
            header_bytes: id.to_be_bytes().to_vec(),
            footer_bytes: Vec::new(),
        }),
        data_bytes: bytes.to_vec(),
        ..Default::default()
    })
}

async fn serve(fake: Fake) -> WorkerClient {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let incoming = tonic::codegen::tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(vci_service_interface::vci_service_server::VciServiceServer::new(fake))
            .serve_with_incoming(incoming),
    );
    let channel = tonic::transport::Endpoint::from_shared(format!("http://127.0.0.1:{port}"))
        .unwrap()
        .connect_lazy();
    vci_service_interface::vci_service_client::VciServiceClient::with_interceptor(
        channel,
        worker_host::client::BearerAuth::new([0; 32], "test"),
    )
}

fn limits() -> JobLimits {
    JobLimits {
        wait_poll: Duration::from_millis(1),
        reopen_poll: Duration::from_millis(1),
        vci_return_wait: Duration::from_millis(300),
        ..JobLimits::default()
    }
}

/// A plan over `endpoints` (a physical pair each, on one functional identifier unless `None`).
fn plan_of(endpoints: &[(u32, u32)], functional_id: Option<u32>) -> CheckedPlan {
    let documents: Vec<(Sha256, Vec<u8>)> = endpoints
        .iter()
        .enumerate()
        .map(|(at, &(tx, rx))| {
            named(document(0x90 + at as u8, tx, rx, |protocol| {
                protocol.functional_id = functional_id.unwrap_or(0)
            }))
        })
        .collect();
    checked(&documents).expect("a valid plan")
}

const A: (u32, u32) = (0x7E0, 0x7E8);
const B: (u32, u32) = (0x7E1, 0x7E9);
const C: (u32, u32) = (0x7E2, 0x7EA);
const D: (u32, u32) = (0x7E3, 0x7EB);
const POSITIVE: &[u8] = &[0x7E, 0x00];
const NEGATIVE: &[u8] = &[0x7F, 0x3E, 0x22];

struct Ran {
    run: IdentificationRun,
    log: Vec<serde_json::Value>,
    _dir: tempfile::TempDir,
}

async fn run_with(plan: CheckedPlan, fake: Fake, limits: JobLimits, cancel: bool) -> Ran {
    run_shared(
        plan,
        fake,
        limits,
        Arc::new(AtomicBool::new(cancel)),
        tempfile::tempdir().unwrap(),
    )
    .await
}

/// [`run_with`] with a cancel flag the caller keeps and a directory it may take locks in
/// (`dir/locks`).
async fn run_shared(
    plan: CheckedPlan,
    fake: Fake,
    limits: JobLimits,
    cancel: Arc<AtomicBool>,
    dir: tempfile::TempDir,
) -> Ran {
    let setup = CommLogSetup::for_run(dir.path().join("log")).unwrap();
    let guards = JobGuards::take_vci_only(
        &GuardSetup {
            dir: dir.path().join("locks"),
            vci: "fake".to_owned(),
        },
        Duration::from_millis(1),
        &AtomicBool::new(false),
    )
    .unwrap();
    let run = run_identification(
        serve(fake.clone()).await,
        plan,
        limits,
        guards,
        Some(setup.clone()),
        Some(VciFacts {
            api: crate::launch::Api::J2534,
            long_size: 8,
        }),
        cancel,
    )
    .await;
    let log = std::fs::read_to_string(setup.path().unwrap())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    Ran {
        run,
        log,
        _dir: dir,
    }
}

async fn run(plan: CheckedPlan, fake: Fake) -> Ran {
    run_with(plan, fake, limits(), false).await
}

impl Ran {
    fn report(&self) -> &IdentificationReport {
        self.run
            .result
            .as_ref()
            .unwrap_or_else(|error| panic!("{error}"))
    }
    fn heard(&self) -> Vec<(u32, Vec<u8>)> {
        self.report()
            .presence()
            .iter()
            .map(|answer| (answer.rx_id, answer.bytes.clone()))
            .collect()
    }
    fn exchanges(&self) -> Vec<&serde_json::Value> {
        self.log
            .iter()
            .filter(|line| line["event"] == "exchange")
            .collect()
    }
}

/// ADR-396 and ADR-327 item 2: one functional presence request per group collects every answer
/// on the receive table of all the bus's pairs; each answer marks every endpoint with its pair
/// (two alternatives sharing a response identifier are both marked); a negative answer is
/// presence; the exchange lists the answers.
#[tokio::test(flavor = "multi_thread")]
async fn one_functional_request_collects_the_answers_of_the_whole_table() {
    let fake = Fake::default();
    fake.state().scripts = VecDeque::from([vec![
        Item::Answer(A.1, POSITIVE),
        Item::Answer(B.1, NEGATIVE),
        Item::Finish,
    ]]);
    // The fourth endpoint shares the third's response identifier, with another request one.
    let ran = run(plan_of(&[A, B, C, (0x7E4, C.1)], Some(0x7DF)), fake.clone()).await;
    let report = ran.report();
    assert_eq!(
        ran.heard(),
        [(A.1, POSITIVE.to_vec()), (B.1, NEGATIVE.to_vec())]
    );
    let answered: Vec<bool> = report
        .endpoints()
        .iter()
        .map(|e| report.answered(&e.key))
        .collect();
    assert_eq!(answered, [true, true, false, false]);
    assert_eq!(report.presence_links_opened(), 1);
    assert!(report.refused_tables().is_empty() && !report.presence_interrupted());
    let state = fake.state();
    assert_eq!(state.tables, [vec![A, B, C, (0x7E4, C.1)]]);
    assert_eq!(state.starts, [(vec![0x3E, 0x00], -2)]);
    for set in [("cp_requestaddrmode", 2), ("cp_canfuncreqid", 0x7DF)] {
        assert!(
            state.set.contains(&(set.0.to_owned(), set.1)),
            "{:?}",
            state.set
        );
    }
    drop(state);
    // The log: the functional link, and one exchange listing both answers.
    let opened = ran
        .log
        .iter()
        .find(|line| line["event"] == "link_opened")
        .unwrap();
    assert_eq!(opened["functional"]["functional_id"], 0x7DF);
    assert_eq!(opened["functional"]["table"].as_array().unwrap().len(), 4);
    let exchanges = ran.exchanges();
    assert_eq!(exchanges.len(), 1, "{:#?}", ran.log);
    assert_eq!(exchanges[0]["origin"], "identification");
    assert_eq!(exchanges[0]["target"]["tx_id"], 0x7DF);
    let ids: Vec<u64> = exchanges[0]["answers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|answer| answer["rx_id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, [u64::from(A.1), u64::from(B.1)]);
    assert_eq!(ran.run.comm_log, CommLogStatus::Complete);
    assert!(!ran.run.guards.link_unconfirmed());
}

/// ADR-398 and ADR-397: the header names the job as an identification job, with the plan's hash
/// and no document, and `vci_connected` carries the API and the width the caller passed, as a
/// procedure's does.
#[tokio::test(flavor = "multi_thread")]
async fn the_header_names_an_identification_job_with_its_plan_and_vci() {
    let fake = Fake::default();
    fake.state().scripts = VecDeque::from([vec![Item::Answer(A.1, POSITIVE), Item::Finish]]);
    let plan = plan_of(&[A, B], Some(0x7DF));
    let expected = hex(&plan.plan_sha256);
    let ran = run(plan, fake).await;
    let header = ran
        .log
        .iter()
        .find(|line| line["event"] == "header")
        .expect("a header");
    assert_eq!(header["job"]["kind"], "identification", "{header}");
    assert_eq!(header["job"]["plan_sha256"], expected.as_str(), "{header}");
    assert!(header["job"].get("document_sha256").is_none(), "{header}");
    let connected = ran
        .log
        .iter()
        .find(|line| line["event"] == "vci_connected")
        .expect("a vci_connected line");
    assert_eq!(connected["api"], "j2534_0404", "{connected}");
    assert_eq!(connected["unsigned_long_bytes"], 8, "{connected}");
    let start = ran
        .log
        .iter()
        .find(|line| line["event"] == "job_start")
        .expect("a job_start");
    assert_eq!(start["writes"], false, "{start}");
}

/// A window with no answer is the silent outcome: nothing is presence, nothing is dropped, and
/// the exchange is a no-response.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_window_marks_nothing() {
    let ran = run(plan_of(&[A, B], Some(0x7DF)), Fake::default()).await;
    let report = ran.report();
    assert!(report.presence().is_empty());
    assert!(report.endpoints().iter().all(|e| !report.answered(&e.key)));
    assert_eq!(ran.exchanges()[0]["outcome"]["result"], "no_response");
    assert_eq!(report.presence_links_opened(), 1);
}

/// ADR-396: a receive table the worker answered with a refusal is halved in declaration order,
/// one link and one request per part and one link open at a time, down to single pairs.
#[tokio::test(flavor = "multi_thread")]
async fn a_table_the_worker_refuses_is_halved_one_link_at_a_time() {
    let fake = Fake::default();
    {
        let mut state = fake.state();
        state.refuse_tables_over = Some(2);
        state.scripts = VecDeque::from([
            vec![Item::Answer(A.1, POSITIVE), Item::Finish],
            vec![Item::Answer(D.1, POSITIVE), Item::Finish],
        ]);
    }
    let ran = run(plan_of(&[A, B, C, D], Some(0x7DF)), fake.clone()).await;
    let report = ran.report();
    let state = fake.state();
    assert_eq!(state.tables, [vec![A, B, C, D], vec![A, B], vec![C, D]]);
    assert_eq!(state.starts.len(), 2);
    assert_eq!(state.max_open_links, 1);
    drop(state);
    assert_eq!(fake.count("ModuleConnect"), 3);
    assert_eq!(
        report
            .refused_tables()
            .iter()
            .map(|t| t.table.clone())
            .collect::<Vec<_>>(),
        [vec![A, B, C, D]]
    );
    assert_eq!(report.presence_links_opened(), 2);
    assert_eq!(
        ran.heard(),
        [(A.1, POSITIVE.to_vec()), (D.1, POSITIVE.to_vec())]
    );
}

/// The halving goes on while a part is refused, and an odd table gives the first part the
/// larger half.
#[tokio::test(flavor = "multi_thread")]
async fn halving_goes_down_to_single_pairs_in_declaration_order() {
    let fake = Fake::default();
    fake.state().refuse_tables_over = Some(1);
    let ran = run(plan_of(&[A, B, C], Some(0x7DF)), fake.clone()).await;
    assert_eq!(
        fake.state().tables,
        [vec![A, B, C], vec![A, B], vec![A], vec![B], vec![C]]
    );
    assert_eq!(ran.report().presence_links_opened(), 3);
    assert_eq!(fake.state().max_open_links, 1);
}

/// A single pair the worker still refuses refuses the job, in the worker's own error.
#[tokio::test(flavor = "multi_thread")]
async fn a_single_pair_that_is_still_refused_refuses_the_job() {
    let fake = Fake::default();
    fake.state().refuse_tables_over = Some(0);
    let ran = run(plan_of(&[A, B], Some(0x7DF)), fake.clone()).await;
    assert!(
        matches!(
            &ran.run.result,
            Err(JobError::Link(HostError::Rpc {
                rpc: "SetUniqueRespIdTable",
                ..
            }))
        ),
        "{:?}",
        ran.run.result
    );
    assert_eq!(fake.state().tables, [vec![A, B], vec![A]]);
    assert!(fake.state().starts.is_empty());
}

/// A table or connect that gets no answer from the worker (a connection that failed) is not a
/// refusal: the table is not halved, and the open is tried again within the bounds, after which
/// the run reports the presence section interrupted.
#[tokio::test(flavor = "multi_thread")]
async fn a_table_that_gets_no_answer_is_not_halved() {
    let fake = Fake::default();
    fake.state().table_status = Some(tonic::Status::unavailable("the worker is gone"));
    let ran = run(plan_of(&[A, B, C, D], Some(0x7DF)), fake.clone()).await;
    let report = ran.report();
    assert!(report.presence_interrupted());
    assert!(report.refused_tables().is_empty());
    // No link ever opened.
    assert_eq!(report.presence_links_opened(), 0);
    let tables = fake.state().tables.clone();
    assert!(tables.len() > 1, "the open was tried again");
    assert!(
        tables.iter().all(|table| *table == [A, B, C, D]),
        "{tables:?}"
    );
    assert!(fake.state().starts.is_empty());
}

/// A link refused at its creation (not the table) fails the job as the runner does.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_that_cannot_be_created_fails_the_job() {
    let fake = Fake::default();
    fake.state().create_status = Some(tonic::Status::permission_denied("no"));
    let ran = run(plan_of(&[A], Some(0x7DF)), fake.clone()).await;
    assert!(
        matches!(
            &ran.run.result,
            Err(JobError::Link(HostError::Rpc {
                rpc: "CreateComLogicalLink",
                ..
            }))
        ),
        "{:?}",
        ran.run.result
    );
    assert!(fake.state().starts.is_empty());
    assert_eq!(ran.run.comm_log, CommLogStatus::Complete);
}

/// ADR-396 and ADR-392: events lost in the window end the primitive, and the request is sent
/// again on the same link with the origin `repeat`; the answers heard before stay. A late answer
/// to the first primitive that arrives meanwhile is no presence, and the log keeps it
/// unattached.
#[tokio::test(flavor = "multi_thread")]
async fn lost_events_resend_the_request_and_keep_the_earlier_answers() {
    let fake = Fake::default();
    fake.state().scripts = VecDeque::from([
        vec![Item::Answer(A.1, POSITIVE), Item::LostEvents],
        vec![
            Item::Late(C.1, POSITIVE),
            Item::Answer(B.1, POSITIVE),
            Item::Finish,
        ],
    ]);
    let ran = run(plan_of(&[A, B, C], Some(0x7DF)), fake.clone()).await;
    assert_eq!(
        ran.heard(),
        [(A.1, POSITIVE.to_vec()), (B.1, POSITIVE.to_vec())]
    );
    assert!(!ran.report().presence_interrupted());
    assert_eq!(fake.state().starts.len(), 2);
    assert_eq!(
        fake.count("ModuleConnect"),
        1,
        "the same link carried the resend"
    );
    let origins: Vec<&str> = ran
        .exchanges()
        .iter()
        .map(|exchange| exchange["origin"].as_str().unwrap())
        .collect();
    assert_eq!(origins, ["identification", "repeat"]);
    let late: Vec<_> = ran
        .log
        .iter()
        .filter(|line| line["event"] == "unattached" && line["identifier"] == C.1)
        .collect();
    assert_eq!(late.len(), 1, "{:#?}", ran.log);
}

/// ADR-396: a VCI lost in the window opens the link again within the run's bounds and sends the
/// request again with the origin `repeat`; the answers heard before the loss count.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_vci_reopens_the_link_and_resends() {
    let fake = Fake::default();
    fake.state().scripts = VecDeque::from([
        vec![Item::Answer(A.1, POSITIVE), Item::VciLost],
        vec![Item::Answer(B.1, NEGATIVE), Item::Finish],
    ]);
    let ran = run(plan_of(&[A, B], Some(0x7DF)), fake.clone()).await;
    assert_eq!(
        ran.heard(),
        [(A.1, POSITIVE.to_vec()), (B.1, NEGATIVE.to_vec())]
    );
    assert!(!ran.report().presence_interrupted());
    assert_eq!(fake.count("ModuleConnect"), 2, "the link was opened again");
    assert_eq!(fake.state().max_open_links, 1);
    let origins: Vec<&str> = ran
        .exchanges()
        .iter()
        .map(|exchange| exchange["origin"].as_str().unwrap())
        .collect();
    assert_eq!(origins, ["identification", "repeat"]);
    // The new link is the functional one again.
    let opened: Vec<_> = ran
        .log
        .iter()
        .filter(|line| line["event"] == "link_opened")
        .collect();
    assert_eq!(opened.len(), 2);
    assert!(
        opened
            .iter()
            .all(|line| line["functional"]["functional_id"] == 0x7DF)
    );
    assert_eq!(fake.state().tables, [vec![A, B], vec![A, B]]);
}

/// The resend is bounded by `max_primitive_repeats` (ADR-392): when every try is lost, the
/// request is sent `1 + max_primitive_repeats` times at most, and the run reports the presence
/// section interrupted with what it heard.
#[tokio::test(flavor = "multi_thread")]
async fn the_resend_is_bounded_and_what_was_heard_is_kept() {
    let fake = Fake::default();
    fake.state().scripts = (0..5)
        // The primitive ends after the lost notice, as a real one does, so the run's end does not
        // wait out the agent's deadline for it.
        .map(|_| vec![Item::Answer(A.1, POSITIVE), Item::LostEvents, Item::Finish])
        .collect();
    let ran = run_with(
        plan_of(&[A, B], Some(0x7DF)),
        fake.clone(),
        JobLimits {
            max_primitive_repeats: 1,
            ..limits()
        },
        false,
    )
    .await;
    assert_eq!(fake.state().starts.len(), 2);
    assert!(ran.report().presence_interrupted());
    assert_eq!(ran.heard(), [(A.1, POSITIVE.to_vec())]);
}

/// A bus on which no declared link has a functional identifier gets no request and no link, and
/// the report names the bus.
#[tokio::test(flavor = "multi_thread")]
async fn a_bus_without_a_presence_group_sends_nothing() {
    let fake = Fake::default();
    let ran = run(plan_of(&[A, B], None), fake.clone()).await;
    assert!(fake.state().calls.is_empty(), "{:?}", fake.state().calls);
    let report = ran.report();
    assert_eq!(report.buses_without_presence().len(), 1);
    assert!(report.presence().is_empty() && report.presence_groups().is_empty());
    assert_eq!(report.presence_links_opened(), 0);
}

/// A cancel taken before the run starts ends it with nothing sent or opened.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_run_opens_nothing() {
    let fake = Fake::default();
    let ran = run_with(plan_of(&[A], Some(0x7DF)), fake.clone(), limits(), true).await;
    assert!(matches!(ran.run.result, Err(JobError::Cancelled)));
    assert!(fake.state().calls.is_empty());
}

/// ADR-396 and ADR-235 item 8: only `presence` sends TesterPresent. On a link the job holds
/// (read-only: the fake reports no VCI), the general send path refuses 0x3E, `presence` refuses
/// anything but `3E 00`, and neither starts a primitive; `3E 00` through `presence` does.
#[tokio::test(flavor = "multi_thread")]
async fn only_the_presence_request_sends_tester_present() {
    use diag_ir::DiagHost;
    let fake = Fake::default();
    let client = serve(fake.clone()).await;
    let handle = Handle::current();
    let guards: GuardSlot = Arc::new(Mutex::new(None));
    let config = LinkConfig::iso15765(A.0, A.1);
    let checks = tokio::task::spawn_blocking(move || {
        let link = open_job_link(
            &handle,
            client.clone(),
            Box::new(client),
            &config,
            None,
            None,
            None,
            &guards,
            Timings::for_link(&config),
        )
        .expect("the link opens");
        let crate::runner::JobLink {
            mut host,
            open_link,
            path_lost,
            ..
        } = link;
        let general = host.service_request(0x3E, &[0x00]);
        let other = host.presence(&[0x3E, 0x80]).end;
        let sent = host.presence(&PRESENCE_REQUEST);
        close_job_link(host, open_link, &path_lost);
        (general, other, sent.end)
    })
    .await
    .unwrap();
    assert!(
        matches!(checks.0, Err(HostError::NotAllowed(0x3E))),
        "{:?}",
        checks.0
    );
    assert!(
        matches!(checks.1, Err(HostError::NotAllowed(0x3E))),
        "{:?}",
        checks.1
    );
    assert!(checks.2.is_ok(), "{:?}", checks.2);
    assert_eq!(fake.state().starts, [(vec![0x3E, 0x00], -2)]);
    assert_eq!(fake.count("StartComPrimitive"), 1);
}

/// A table refusal whose link then fails to close leaves the guards marked (ADR-258): the table
/// is not halved and nothing more opens.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_whose_link_does_not_close_ends_the_job() {
    let fake = Fake::default();
    {
        let mut state = fake.state();
        state.refuse_tables_over = Some(1);
        state.destroy_script = VecDeque::from([Some(tonic::Status::internal("stuck"))]);
    }
    let ran = run(plan_of(&[A, B], Some(0x7DF)), fake.clone()).await;
    assert!(
        matches!(
            &ran.run.result,
            Err(JobError::Link(HostError::Rpc {
                rpc: "SetUniqueRespIdTable",
                ..
            }))
        ),
        "{:?}",
        ran.run.result
    );
    assert!(ran.run.guards.link_unconfirmed());
    assert_eq!(fake.state().tables, [vec![A, B]]);
}

/// A part whose link does not close marks the guards, and the next part does not open.
#[tokio::test(flavor = "multi_thread")]
async fn a_part_whose_link_does_not_close_ends_the_run() {
    let fake = Fake::default();
    {
        let mut state = fake.state();
        state.refuse_tables_over = Some(2);
        // The refused table's link closes; the first part's does not.
        state.destroy_script = VecDeque::from([None, Some(tonic::Status::internal("stuck"))]);
    }
    let ran = run(plan_of(&[A, B, C, D], Some(0x7DF)), fake.clone()).await;
    assert!(
        matches!(ran.run.result, Err(JobError::LinkUnconfirmed)),
        "{:?}",
        ran.run.result
    );
    assert!(ran.run.guards.link_unconfirmed());
    assert_eq!(fake.state().tables, [vec![A, B, C, D], vec![A, B]]);
}

/// ADR-294's form of a VCI lost while the link opens (a refusal naming the lost VCI) is a lost
/// path, not a refusal: the link is opened again, and the table is not halved.
#[tokio::test(flavor = "multi_thread")]
async fn a_vci_lost_while_the_link_opens_opens_it_again() {
    use vci_service_interface::{ErrorDetail, ErrorEventData, PduError};
    let fake = Fake::default();
    {
        let mut state = fake.state();
        state.table_script = VecDeque::from([vci_service_interface::status_with_error_detail(
            tonic::Code::FailedPrecondition,
            "the VCI is lost",
            ErrorDetail {
                pdu_error: PduError::PduErrCllNotConnected as i32,
                error_event_data: Some(ErrorEventData {
                    error_event: PduErrorEvent::PduErrEvtLostCommToVci as i32,
                    cop_handle: None,
                    timestamp: 1,
                    extra_error_info: 0,
                }),
                detail_text: None,
                refusal: None,
            },
        )]);
        state.scripts = VecDeque::from([vec![Item::Answer(A.1, POSITIVE), Item::Finish]]);
    }
    let ran = run(plan_of(&[A, B], Some(0x7DF)), fake.clone()).await;
    let report = ran.report();
    assert_eq!(fake.state().tables, [vec![A, B], vec![A, B]]);
    assert!(report.refused_tables().is_empty() && !report.presence_interrupted());
    assert_eq!(report.presence_links_opened(), 1);
    assert_eq!(ran.heard(), [(A.1, POSITIVE.to_vec())]);
}

/// ADR-398: the J2534 worker answers a device lost while the connect installs the receive
/// table with the lost-VCI form (`FAILED_PRECONDITION` naming `PDU_ERR_EVT_LOST_COMM_TO_VCI`),
/// so the link is opened again, with the same table, not halved as a refusal is.
#[tokio::test(flavor = "multi_thread")]
async fn a_vci_lost_while_the_connect_installs_the_table_is_not_halved() {
    use vci_service_interface::{ErrorDetail, ErrorEventData, PduError};
    let fake = Fake::default();
    {
        let mut state = fake.state();
        state.link_connect_script =
            VecDeque::from([vci_service_interface::status_with_error_detail(
                tonic::Code::FailedPrecondition,
                "PassThruStartMsgFilter failed: the device is not connected",
                ErrorDetail {
                    pdu_error: PduError::PduErrCommPcToVciFailed as i32,
                    error_event_data: Some(ErrorEventData {
                        error_event: PduErrorEvent::PduErrEvtLostCommToVci as i32,
                        cop_handle: None,
                        timestamp: 1,
                        extra_error_info: 0,
                    }),
                    detail_text: None,
                    refusal: None,
                },
            )]);
        state.scripts = VecDeque::from([vec![Item::Answer(A.1, POSITIVE), Item::Finish]]);
    }
    let ran = run(plan_of(&[A, B], Some(0x7DF)), fake.clone()).await;
    let report = ran.report();
    assert_eq!(fake.state().tables, [vec![A, B], vec![A, B]]);
    assert!(report.refused_tables().is_empty() && !report.presence_interrupted());
    assert_eq!(report.presence_links_opened(), 1);
    assert_eq!(ran.heard(), [(A.1, POSITIVE.to_vec())]);
}

/// ADR-398: the J2534 worker answers a filter error that is neither a refusal of the filter nor
/// a lost device as `UNKNOWN` with `PDU_ERR_FCT_FAILED`, which is not a refusal of the table:
/// it is not halved.
#[tokio::test(flavor = "multi_thread")]
async fn another_error_while_the_connect_installs_the_table_is_not_halved() {
    use vci_service_interface::{ErrorDetail, PduError};
    let fake = Fake::default();
    fake.state().link_connect_script =
        VecDeque::from([vci_service_interface::status_with_error_detail(
            tonic::Code::Unknown,
            "PassThruStartMsgFilter failed: ERR_FAILED",
            ErrorDetail {
                pdu_error: PduError::PduErrFctFailed as i32,
                error_event_data: None,
                detail_text: None,
                refusal: None,
            },
        )]);
    let ran = run(plan_of(&[A, B, C, D], Some(0x7DF)), fake.clone()).await;
    let report = ran.report();
    assert!(report.refused_tables().is_empty());
    let tables = fake.state().tables.clone();
    assert!(
        tables.iter().all(|table| *table == [A, B, C, D]),
        "{tables:?}"
    );
}

/// Only a response identifier of the receive table marks presence; the log lists every result.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_outside_the_table_is_listed_but_marks_nothing() {
    let fake = Fake::default();
    fake.state().scripts = VecDeque::from([vec![
        Item::Answer(0x6FF, POSITIVE),
        Item::Answer(A.1, POSITIVE),
        Item::Finish,
    ]]);
    let ran = run(plan_of(&[A, B], Some(0x7DF)), fake).await;
    assert_eq!(ran.heard(), [(A.1, POSITIVE.to_vec())]);
    let ids: Vec<u64> = ran.exchanges()[0]["answers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|answer| answer["rx_id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, [0x6FF, u64::from(A.1)]);
}

/// A source that tells its n-th status question that the worker has exited, as a launcher that
/// owns the process does (ADR-394).
#[derive(Clone)]
struct ExitsOnce(WorkerClient, Arc<Mutex<u32>>, u32, bool);

impl WorkerSource for ExitsOnce {
    type Client = WorkerClient;
    fn client(&mut self) -> Result<WorkerClient, HostError> {
        Ok(self.0.clone())
    }
    fn status(&mut self) -> WorkerStatus {
        let mut calls = self.1.lock().unwrap();
        *calls += 1;
        if *calls == self.2 {
            WorkerStatus::Exited(WorkerExit {
                pid: Some(7),
                how: crate::client::ExitHow::Code(1),
            })
        } else {
            WorkerStatus::Alive
        }
    }
    fn end(&mut self) -> Result<WorkerExit, HostError> {
        if self.3 {
            Ok(WorkerExit {
                pid: Some(7),
                how: crate::client::ExitHow::KilledByAgent,
            })
        } else {
            Err(HostError::Setup("the worker cannot be ended"))
        }
    }
    fn pid(&self) -> Option<u32> {
        Some(8)
    }
}

/// A worker that exited between two parts is logged (`worker_exit`, then `worker_restart`)
/// before the next part opens, not started again silently (ADR-394).
#[tokio::test(flavor = "multi_thread")]
async fn a_worker_that_exits_between_parts_is_logged() {
    let fake = Fake::default();
    fake.state().refuse_tables_over = Some(1);
    let dir = tempfile::tempdir().unwrap();
    let setup = CommLogSetup::for_run(dir.path().join("log")).unwrap();
    let guards = JobGuards::take_vci_only(
        &GuardSetup {
            dir: dir.path().join("locks"),
            vci: "fake".to_owned(),
        },
        Duration::from_millis(1),
        &AtomicBool::new(false),
    )
    .unwrap();
    let source = ExitsOnce(serve(fake).await, Arc::new(Mutex::new(0)), 2, false);
    let run = run_identification(
        source,
        plan_of(&[A, B], Some(0x7DF)),
        limits(),
        guards,
        Some(setup.clone()),
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    assert!(run.result.is_ok(), "{:?}", run.result);
    let events: Vec<String> = std::fs::read_to_string(setup.path().unwrap())
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|line| line["event"].as_str().map(str::to_owned))
        .filter(|event| event.starts_with("worker_") || event == "link_opened")
        .collect();
    assert_eq!(
        events,
        [
            "worker_exit",
            "worker_restart",
            "link_opened",
            "link_opened"
        ]
    );
}

/// ADR-396: a receive error that ends the window after an answer is not a clean window: the part
/// is interrupted with its answer, and the request is not sent again.
#[tokio::test(flavor = "multi_thread")]
async fn a_receive_error_after_an_answer_interrupts_the_part_and_keeps_the_answer() {
    let fake = Fake::default();
    fake.state().scripts = VecDeque::from([vec![
        Item::Answer(A.1, POSITIVE),
        Item::RxError,
        Item::Finish,
    ]]);
    let ran = run(plan_of(&[A, B], Some(0x7DF)), fake.clone()).await;
    assert_eq!(ran.heard(), [(A.1, POSITIVE.to_vec())]);
    assert!(ran.report().presence_interrupted());
    assert_eq!(ran.report().presence_links_opened(), 1);
    assert_eq!(fake.state().starts.len(), 1);
    assert_eq!(
        ran.exchanges()[0]["answers"].as_array().map(Vec::len),
        Some(1)
    );
}

/// What a run does when the worker's process ends during `ModuleConnect`, so that its cleanup
/// fails too: `exits` is the status question that finds it exited (0: never).
async fn run_with_worker_lost_in_setup(exits: u32) -> (IdentificationRun, Vec<String>) {
    let fake = Fake::default();
    {
        let mut state = fake.state();
        state.connect_script = VecDeque::from([tonic::Status::unavailable("the worker is gone")]);
        state.disconnect_script =
            VecDeque::from([tonic::Status::unavailable("the worker is gone")]);
        state.scripts = VecDeque::from([vec![Item::Answer(A.1, POSITIVE), Item::Finish]]);
    }
    let dir = tempfile::tempdir().unwrap();
    let setup = CommLogSetup::for_run(dir.path().join("log")).unwrap();
    let guards = JobGuards::take_vci_only(
        &GuardSetup {
            dir: dir.path().join("locks"),
            vci: "fake".to_owned(),
        },
        Duration::from_millis(1),
        &AtomicBool::new(false),
    )
    .unwrap();
    let source = ExitsOnce(serve(fake).await, Arc::new(Mutex::new(0)), exits, false);
    let run = run_identification(
        source,
        plan_of(&[A, B], Some(0x7DF)),
        limits(),
        guards,
        Some(setup.clone()),
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    let events = std::fs::read_to_string(setup.path().unwrap())
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|line| line["event"].as_str().map(str::to_owned))
        .filter(|event| event.starts_with("worker_") || event == "link_opened")
        .collect();
    (run, events)
}

/// ADR-394: a worker that exited during the link's setup confirms the link closed, though the
/// cleanup got no answer: the exit is logged, the guards' mark cleared, and the link opened again
/// on a new worker.
#[tokio::test(flavor = "multi_thread")]
async fn a_worker_that_exits_during_the_setup_is_replaced() {
    // Question 1 is the probe before the open, question 2 the one after its failure.
    let (run, events) = run_with_worker_lost_in_setup(2).await;
    let report = run
        .result
        .as_ref()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(report.presence_links_opened(), 1);
    assert_eq!(report.presence()[0].rx_id, A.1);
    assert!(!run.guards.link_unconfirmed());
    assert_eq!(events, ["worker_exit", "worker_restart", "link_opened"]);
}

/// A worker that is alive or unknown leaves the unclean failure as it was: the guards stay marked
/// and the run ends (ADR-258).
#[tokio::test(flavor = "multi_thread")]
async fn a_setup_failure_that_leaves_the_worker_alive_ends_the_run() {
    let (run, events) = run_with_worker_lost_in_setup(0).await;
    assert!(
        matches!(
            run.result,
            Err(JobError::Link(HostError::Rpc {
                rpc: "ModuleConnect",
                ..
            }))
        ),
        "{:?}",
        run.result
    );
    assert!(run.guards.link_unconfirmed());
    assert!(events.is_empty(), "{events:?}");
}

/// A worker that is alive and hangs at `ModuleConnect`, its cleanup hanging too, is ended by the
/// agent when it can (ADR-394): the mark is cleared, the link opens again on a new worker and the
/// answer is heard; when it cannot be ended the run ends with the guards marked (ADR-258).
async fn run_with_a_hung_worker(ends: bool) -> (IdentificationRun, Vec<String>) {
    let fake = Fake::default();
    {
        let mut state = fake.state();
        state.hang_connect = 1;
        state.hang_disconnect = 1;
        state.scripts = VecDeque::from([vec![Item::Answer(A.1, POSITIVE), Item::Finish]]);
    }
    let dir = tempfile::tempdir().unwrap();
    let setup = CommLogSetup::for_run(dir.path().join("log")).unwrap();
    let guards = JobGuards::take_vci_only(
        &GuardSetup {
            dir: dir.path().join("locks"),
            vci: "fake".to_owned(),
        },
        Duration::from_millis(1),
        &AtomicBool::new(false),
    )
    .unwrap();
    let source = ExitsOnce(serve(fake).await, Arc::new(Mutex::new(0)), 0, ends);
    let run = run_identification(
        source,
        plan_of(&[A, B], Some(0x7DF)),
        limits(),
        guards,
        Some(setup.clone()),
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    let events = std::fs::read_to_string(setup.path().unwrap())
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|line| line["event"].as_str().map(str::to_owned))
        .filter(|event| event.starts_with("worker_") || event == "link_opened")
        .collect();
    (run, events)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_worker_that_can_be_ended_is_replaced() {
    let (run, events) = run_with_a_hung_worker(true).await;
    let report = run
        .result
        .as_ref()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(report.presence()[0].rx_id, A.1);
    assert!(!run.guards.link_unconfirmed());
    assert_eq!(events, ["worker_exit", "worker_restart", "link_opened"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_worker_that_cannot_be_ended_ends_the_run_with_the_guards_marked() {
    let (run, events) = run_with_a_hung_worker(false).await;
    assert!(
        matches!(
            run.result,
            Err(JobError::Link(HostError::WorkerUnresponsive(
                "ModuleConnect"
            )))
        ),
        "{:?}",
        run.result
    );
    assert!(run.guards.link_unconfirmed());
    assert!(events.is_empty(), "{events:?}");
}

// ---------------------------------------------------------------- The VIN phase (ADR-399)

const VIN_A: &str = "WVWZZZ1JZ3W386752";
const VIN_B: &str = "1HGCM82633A004352";

/// A fake whose physical primitives are answered by simulated ECUs, one per request identifier,
/// each with its VIN.
fn fake_with(ecus: &[(u32, &str)]) -> Fake {
    let fake = Fake::default();
    for &(tx, vin) in ecus {
        fake.state().ecus.insert(
            tx,
            sim_ecu::SimEcu::new(sim_ecu::EcuConfig {
                vin: vin.to_owned(),
                ..Default::default()
            }),
        );
    }
    fake
}

/// A document on `endpoint` (with the functional identifier of the presence tests) whose
/// service 1 reads data identifier F1`did` as 17 bytes of text. F190 is the simulated ECU's VIN.
fn reader(did: u8, endpoint: (u32, u32)) -> (Sha256, Vec<u8>) {
    named(document_reading(did, 17, endpoint.0, endpoint.1, |p| {
        p.functional_id = 0x7DF
    }))
}

/// A plan of `Candidates` over `documents`, with a VIN rule for the first position of each
/// document listed in `rules`, expecting the text given.
fn vin_plan_of(documents: &[(Sha256, Vec<u8>)], rules: &[(usize, &str)]) -> CheckedPlan {
    let rules: Vec<(usize, u8, u8, &str)> = rules
        .iter()
        .map(|&(at, expected)| (at, 1, 1, expected))
        .collect();
    checked_vin(documents, Selection::Candidates, &rules).expect("a valid plan")
}

/// A plan reading the VIN of each document, whatever the first character is.
fn reading_all(documents: &[(Sha256, Vec<u8>)]) -> CheckedPlan {
    let rules: Vec<(usize, &str)> = (0..documents.len()).map(|at| (at, "W")).collect();
    vin_plan_of(documents, &rules)
}

fn vin(text: &str) -> crate::journal::Vin {
    crate::journal::Vin::new(text.to_owned())
}

fn outcomes(report: &IdentificationReport) -> Vec<Vec<VinOutcome>> {
    report
        .vin_reads()
        .iter()
        .map(|read| read.sources.iter().map(|s| s.outcome.clone()).collect())
        .collect()
}

fn origins(ran: &Ran) -> Vec<&str> {
    ran.exchanges()
        .iter()
        .map(|exchange| exchange["origin"].as_str().unwrap())
        .collect()
}

/// ADR-399: a plan that names no VIN rule opens no VIN link and sends no VIN request, and
/// establishes nothing (no lock is taken).
#[tokio::test(flavor = "multi_thread")]
async fn a_plan_without_a_vin_rule_reads_no_vin() {
    let fake = fake_with(&[(A.0, VIN_A)]);
    let ran = run(plan_of(&[A, B], Some(0x7DF)), fake.clone()).await;
    let report = ran.report();
    assert!(report.vin_reads().is_empty() && report.vin().is_none());
    assert!(report.vins_seen().is_empty() && report.vin_rules().is_empty());
    assert!(!report.vin_interrupted() && !report.vehicle_lock_held());
    assert!(fake.state().served.is_empty());
    assert_eq!(fake.state().starts.len(), 1, "only the presence request");
    assert_eq!(fake.count("ModuleConnect"), 1);
    assert!(!ran.run.guards.holds_vehicle());
}

/// ADR-399 items 1 and 3: a first read that does not decode does not block a second that does:
/// the VIN is the second's, each read had its own link (one open at a time), the VIN went to the
/// communication log's `identity` event as it was read, the rules stand to it as the report
/// says, and the job holds the vehicle's lock when it returns.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_read_that_decodes_gives_the_vin_when_the_first_does_not() {
    // F186 answers one byte, which a 17-byte field does not decode from.
    let documents = [reader(0x86, A), reader(0x90, B)];
    let fake = fake_with(&[(A.0, VIN_A), (B.0, VIN_B)]);
    let plan = vin_plan_of(&documents, &[(0, "W"), (1, "1")]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(report.vin(), Some(&vin(VIN_B)));
    assert_eq!(report.vins_seen(), [vin(VIN_B)]);
    assert_eq!(
        outcomes(report),
        [
            vec![VinOutcome::Undecodable],
            vec![VinOutcome::Decoded(vin(VIN_B))]
        ]
    );
    assert_eq!(report.vin_reads()[0].accepted, None);
    assert_eq!(report.vin_reads()[1].accepted, Some(vin(VIN_B)));
    assert_eq!(
        report.vin_reads()[1].endpoint.as_ref().map(|e| e.tx_id),
        Some(B.0)
    );
    let results: Vec<_> = report.vin_rules().iter().map(|r| r.outcome).collect();
    assert_eq!(
        results,
        [RuleOutcome::NotMatched, RuleOutcome::Matched],
        "reported, and nothing dropped"
    );
    assert_eq!(report.candidates(), ["c1"]);
    assert!(!report.vin_interrupted() && report.vehicle_lock_held());
    assert!(ran.run.guards.holds_vehicle());
    let state = fake.state();
    assert_eq!(
        state.served,
        [(A.0, vec![0x22, 0xF1, 0x86]), (B.0, vec![0x22, 0xF1, 0x90])]
    );
    assert_eq!(state.max_open_links, 1);
    assert_eq!(state.open_links, 0);
    drop(state);
    assert_eq!(fake.count("ModuleConnect"), 3);
    assert_eq!(origins(&ran), ["identification"; 3]);
    let identities: Vec<_> = ran
        .log
        .iter()
        .filter(|line| line["event"] == "identity")
        .collect();
    assert_eq!(identities.len(), 2, "{:#?}", ran.log);
    assert_eq!(identities[0]["outcome"], "unreadable");
    assert_eq!(identities[1]["kind"], "vin");
    assert_eq!(identities[1]["text"], VIN_B);
    let json = serde_json::to_value(report).unwrap();
    assert_eq!(json["vin"], VIN_B);
    assert_eq!(json["vehicle_lock_held"], true);
}

/// ADR-399 item 3: two reads that give different well-formed VINs establish none, both are
/// reported, no lock is taken, and the rules are not evaluated.
#[tokio::test(flavor = "multi_thread")]
async fn reads_that_give_different_vins_establish_none() {
    let documents = [reader(0x90, A), reader(0x90, B)];
    let fake = fake_with(&[(A.0, VIN_A), (B.0, VIN_B)]);
    let ran = run(reading_all(&documents), fake).await;
    let report = ran.report();
    assert_eq!(report.vin(), None);
    assert_eq!(report.vins_seen(), [vin(VIN_A), vin(VIN_B)]);
    assert_eq!(
        outcomes(report),
        [
            vec![VinOutcome::Decoded(vin(VIN_A))],
            vec![VinOutcome::Decoded(vin(VIN_B))]
        ]
    );
    assert!(!report.vin_interrupted() && !report.vehicle_lock_held());
    assert!(
        report
            .vin_rules()
            .iter()
            .all(|r| r.outcome == RuleOutcome::Unevaluated)
    );
    assert!(!ran.run.guards.holds_vehicle());
}

/// ADR-399 item 1: the one response of a read is decoded with every declaration of it, sent once,
/// and the read is accepted when all give one well-formed VIN. (The ADR-345 table admits only a
/// 17-byte text field that ends the response, so declarations of one request read the same bytes
/// and cannot disagree; `accepted_vin`'s test covers the disagreement the rule guards against.)
#[tokio::test(flavor = "multi_thread")]
async fn one_response_is_decoded_with_every_declaration_of_its_read() {
    let first = reader(0x90, A);
    let second = named(super::tests::edited(&first.1, |variant| {
        variant.short_name = Some("other".into())
    }));
    let fake = fake_with(&[(A.0, VIN_A)]);
    let ran = run(reading_all(&[first, second]), fake.clone()).await;
    let report = ran.report();
    assert_eq!(report.vin_reads().len(), 1, "one request, one read");
    assert_eq!(
        outcomes(report),
        [vec![
            VinOutcome::Decoded(vin(VIN_A)),
            VinOutcome::Decoded(vin(VIN_A))
        ]]
    );
    assert_eq!(report.vin_reads()[0].accepted, Some(vin(VIN_A)));
    assert_eq!(report.vin(), Some(&vin(VIN_A)));
    assert_eq!(
        fake.state().served.len(),
        1,
        "sent once for both declarations"
    );
    let identities = ran
        .log
        .iter()
        .filter(|line| line["event"] == "identity")
        .count();
    assert_eq!(identities, 2, "each declaration's reading is noted");
}

#[test]
fn a_read_is_accepted_when_all_its_sources_gave_one_vin() {
    let decoded = |text| VinOutcome::Decoded(vin(text));
    assert_eq!(accepted_vin(&[]), None);
    assert_eq!(accepted_vin(&[decoded(VIN_A)]), Some(vin(VIN_A)));
    assert_eq!(
        accepted_vin(&[decoded(VIN_A), decoded(VIN_A)]),
        Some(vin(VIN_A))
    );
    assert_eq!(accepted_vin(&[decoded(VIN_A), decoded(VIN_B)]), None);
    assert_eq!(
        accepted_vin(&[decoded(VIN_A), VinOutcome::NotWellFormed]),
        None
    );
    assert_eq!(
        accepted_vin(&[VinOutcome::NoUsableAnswer, decoded(VIN_A)]),
        None
    );
}

/// ADR-399 item 4: a source whose document declares a link ADR-324 refuses is reported
/// `LinkRefused`, no link is opened for it, and the other sources decide.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_link_is_reported_and_the_other_sources_decide() {
    let refused = named(document_reading(0x90, 17, B.0, B.1, |p| {
        p.functional_id = 0x7DF;
        p.extended_addressing = true;
    }));
    let documents = [refused, reader(0x90, A)];
    let fake = fake_with(&[(A.0, VIN_A), (B.0, VIN_B)]);
    let ran = run(reading_all(&documents), fake.clone()).await;
    let report = ran.report();
    assert_eq!(
        outcomes(report),
        [
            vec![VinOutcome::LinkRefused],
            vec![VinOutcome::Decoded(vin(VIN_A))]
        ]
    );
    assert_eq!(report.vin(), Some(&vin(VIN_A)));
    assert_eq!(report.vin_reads()[0].endpoint, None);
    assert_eq!(
        fake.count("ModuleConnect"),
        2,
        "the presence link and one read"
    );
    assert_eq!(fake.state().served.len(), 1);
    assert!(report.vehicle_lock_held());
}

/// ADR-399 item 1 and ADR-392: a request nobody answers is sent again within
/// `max_primitive_repeats` (origin `repeat`) and reported `NoUsableAnswer`; a negative response is
/// reported with its code; neither blocks the read that decodes.
#[tokio::test(flavor = "multi_thread")]
async fn no_answer_and_a_negative_response_do_not_block_the_read_that_decodes() {
    // No ECU answers on A; B has no such data identifier; C has the VIN.
    let documents = [reader(0x90, A), reader(0x77, B), reader(0x90, C)];
    let fake = fake_with(&[(B.0, VIN_B), (C.0, VIN_A)]);
    let ran = run(reading_all(&documents), fake.clone()).await;
    let report = ran.report();
    assert_eq!(
        outcomes(report),
        [
            vec![VinOutcome::NoUsableAnswer],
            vec![VinOutcome::Negative(0x31)],
            vec![VinOutcome::Decoded(vin(VIN_A))]
        ]
    );
    assert_eq!(report.vin(), Some(&vin(VIN_A)));
    assert!(!report.vin_interrupted());
    // The presence request, three sends to A (the limit's two repeats), one each to B and C.
    assert_eq!(fake.state().starts.len(), 6);
    assert_eq!(
        origins(&ran),
        [
            "identification",
            "identification",
            "repeat",
            "repeat",
            "identification",
            "identification"
        ]
    );
}

/// ADR-399 and ADR-396 item 6: a presence phase whose bounds ran out leaves the VIN phase not
/// run: no VIN link, every source `NotRead`, `vin_interrupted`, nothing established.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_presence_phase_leaves_the_vin_phase_unrun() {
    let documents = [reader(0x90, A), reader(0x90, B)];
    let fake = fake_with(&[(A.0, VIN_A), (B.0, VIN_A)]);
    fake.state().scripts = (0..5)
        .map(|_| vec![Item::Answer(A.1, POSITIVE), Item::LostEvents, Item::Finish])
        .collect();
    let limits = JobLimits {
        max_primitive_repeats: 1,
        ..limits()
    };
    let ran = run_with(reading_all(&documents), fake.clone(), limits, false).await;
    let report = ran.report();
    assert!(report.presence_interrupted() && report.vin_interrupted());
    assert_eq!(
        outcomes(report),
        [vec![VinOutcome::NotRead], vec![VinOutcome::NotRead]]
    );
    assert!(report.vin().is_none() && !report.vehicle_lock_held());
    assert!(fake.state().served.is_empty());
    assert_eq!(fake.count("ModuleConnect"), 1);
}

/// ADR-399 item 3: the bounds running out in a VIN read (the VCI lost again after the request
/// was sent again) leaves that read and the ones after it `NotRead`, establishes no VIN even
/// though an earlier read accepted one, and takes no lock.
#[tokio::test(flavor = "multi_thread")]
async fn bounds_that_run_out_in_a_vin_read_establish_nothing() {
    let documents = [reader(0x90, A), reader(0x90, B), reader(0x90, C)];
    let fake = fake_with(&[(A.0, VIN_A), (B.0, VIN_A), (C.0, VIN_A)]);
    // The presence request; the first read, served by its ECU; the second read, whose VCI is lost
    // on the request and again on the one sent again.
    fake.state().scripts = VecDeque::from([
        vec![Item::Finish],
        vec![Item::Routed],
        vec![Item::VciLost],
        vec![Item::VciLost],
    ]);
    let limits = JobLimits {
        max_primitive_repeats: 1,
        ..limits()
    };
    let ran = run_with(reading_all(&documents), fake.clone(), limits, false).await;
    let report = ran.report();
    assert_eq!(
        outcomes(report),
        [
            vec![VinOutcome::Decoded(vin(VIN_A))],
            vec![VinOutcome::NotRead],
            vec![VinOutcome::NotRead]
        ]
    );
    assert_eq!(report.vins_seen(), [vin(VIN_A)]);
    assert!(report.vin().is_none() && report.vin_interrupted());
    assert!(!report.presence_interrupted() && !report.vehicle_lock_held());
    assert!(!ran.run.guards.holds_vehicle());
    assert_eq!(fake.state().served.len(), 1, "the third read was not made");
    assert_eq!(fake.state().open_links, 0);
}

/// ADR-399 and ADR-393: a VCI lost in a VIN read opens the link again and sends the request
/// again with the origin `repeat`, and the read then decodes.
#[tokio::test(flavor = "multi_thread")]
async fn a_vci_lost_in_a_vin_read_reopens_the_link_and_sends_it_again() {
    let fake = fake_with(&[(A.0, VIN_A)]);
    fake.state().scripts =
        VecDeque::from([vec![Item::Finish], vec![Item::VciLost], vec![Item::Routed]]);
    let ran = run(reading_all(&[reader(0x90, A)]), fake.clone()).await;
    let report = ran.report();
    assert_eq!(report.vin(), Some(&vin(VIN_A)));
    assert_eq!(fake.count("ModuleConnect"), 3, "the link was opened again");
    assert_eq!(fake.state().max_open_links, 1);
    assert_eq!(
        origins(&ran),
        ["identification", "identification", "repeat"]
    );
}

/// ADR-396 item 6 and ADR-399: the link reopens are counted over the run, not per phase: a limit
/// of one, used by the presence phase, leaves none for a VIN read that loses the VCI.
#[tokio::test(flavor = "multi_thread")]
async fn link_reopens_are_counted_over_the_presence_and_the_vin_phases() {
    let fake = fake_with(&[(A.0, VIN_A)]);
    fake.state().scripts = VecDeque::from([
        vec![Item::VciLost],
        vec![Item::Finish],
        vec![Item::VciLost],
        vec![Item::Routed],
    ]);
    let limits = JobLimits {
        max_link_reopens: 1,
        ..limits()
    };
    let ran = run_with(reading_all(&[reader(0x90, A)]), fake.clone(), limits, false).await;
    let report = ran.report();
    assert!(
        !report.presence_interrupted(),
        "the presence phase reopened once"
    );
    assert!(report.vin_interrupted() && report.vin().is_none());
    assert_eq!(outcomes(report), [vec![VinOutcome::NotRead]]);
    assert!(fake.state().served.is_empty());
}

/// ADR-399: a cancel that comes in once the presence phase has sent ends the run in
/// `Cancelled` before any VIN link opens, and no lock is held.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_before_the_vin_phase_ends_the_run_with_no_vin_link() {
    let fake = fake_with(&[(A.0, VIN_A)]);
    let cancel = Arc::new(AtomicBool::new(false));
    fake.state().cancel_at_start = Some((1, Arc::clone(&cancel)));
    let ran = run_shared(
        reading_all(&[reader(0x90, A)]),
        fake.clone(),
        limits(),
        cancel,
        tempfile::tempdir().unwrap(),
    )
    .await;
    assert!(matches!(ran.run.result, Err(JobError::Cancelled)));
    assert_eq!(fake.count("ModuleConnect"), 1);
    assert!(fake.state().served.is_empty());
    assert!(!ran.run.guards.holds_vehicle());
}

/// ADR-399 and design 8.8.2: a definition the operator chose by hand reads the VIN its rule names
/// like any plan: read, reported, and the vehicle's lock taken.
#[tokio::test(flavor = "multi_thread")]
async fn a_manual_plan_with_a_vin_rule_reads_reports_and_takes_the_lock() {
    let documents = [reader(0x90, A)];
    let plan = checked_vin(
        &documents,
        Selection::ChosenDefinition,
        &[(0, 10, 10, "3"), (0, 1, 3, "WVX")],
    )
    .unwrap();
    let ran = run(plan, fake_with(&[(A.0, VIN_A)])).await;
    let report = ran.report();
    assert_eq!(report.selection(), Selection::ChosenDefinition);
    assert_eq!(report.vin(), Some(&vin(VIN_A)));
    let results: Vec<_> = report.vin_rules().iter().map(|r| r.outcome).collect();
    assert_eq!(results, [RuleOutcome::Matched, RuleOutcome::NotMatched]);
    assert!(report.vehicle_lock_held() && ran.run.guards.holds_vehicle());
}

/// A run that reads the VIN while another job (of another VCI) holds the vehicle's lock, stopped
/// at the wait: the other job's guards, the run's task, its cancel flag and the fake.
struct Waiting {
    holder: JobGuards,
    task: tokio::task::JoinHandle<Ran>,
    cancel: Arc<AtomicBool>,
    fake: Fake,
}

async fn waiting_for_the_vehicle() -> Waiting {
    waiting_for_the_vehicle_with(reading_all(&[reader(0x90, A)])).await
}

/// [`waiting_for_the_vehicle`] for `plan`, which reads the VIN of the document on `A`.
async fn waiting_for_the_vehicle_with(plan: CheckedPlan) -> Waiting {
    let dir = tempfile::tempdir().unwrap();
    let never = AtomicBool::new(false);
    let poll = Duration::from_millis(1);
    let mut holder = JobGuards::take_vci_only(
        &GuardSetup {
            dir: dir.path().join("locks"),
            vci: "other".to_owned(),
        },
        poll,
        &never,
    )
    .unwrap();
    holder.take_vehicle(&vin(VIN_A), poll, &never).unwrap();
    let fake = fake_with(&[(A.0, VIN_A)]);
    let cancel = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(run_shared(
        plan,
        fake.clone(),
        limits(),
        Arc::clone(&cancel),
        dir,
    ));
    // The read is made and its link closed (the presence link's disconnect and the read's), then
    // the only step left is the wait for the holder.
    let mut closed = false;
    for _ in 0..1000 {
        if !fake.state().served.is_empty()
            && fake.state().open_links == 0
            && fake.count("ModuleDisconnect") == 2
        {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(closed, "the read was made and its link closed in time");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!task.is_finished(), "the run waits for the holder");
    assert_eq!(fake.state().open_links, 0, "no link is open while it waits");
    Waiting {
        holder,
        task,
        cancel,
        fake,
    }
}

/// ADR-399 item 3 and design 8.8.1: a vehicle another job holds is waited for, with no VCI link
/// open, and the lock is taken once the holder lets go.
#[tokio::test(flavor = "multi_thread")]
async fn the_vehicle_lock_another_job_holds_is_waited_for() {
    let waiting = waiting_for_the_vehicle().await;
    drop(waiting.holder);
    let ran = tokio::time::timeout(Duration::from_secs(10), waiting.task)
        .await
        .expect("the run goes on once the holder let go")
        .unwrap();
    assert_eq!(ran.report().vin(), Some(&vin(VIN_A)));
    assert!(ran.report().vehicle_lock_held() && ran.run.guards.holds_vehicle());
    assert_eq!(waiting.fake.state().open_links, 0);
}

/// ADR-399 item 3: a cancel during the wait for the vehicle's lock ends the run in `Cancelled`,
/// and the guards come back without the lock.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_during_the_wait_for_the_vehicle_ends_the_run() {
    let waiting = waiting_for_the_vehicle().await;
    waiting
        .cancel
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let ran = tokio::time::timeout(Duration::from_secs(10), waiting.task)
        .await
        .expect("the wait sees the cancel")
        .unwrap();
    assert!(matches!(ran.run.result, Err(JobError::Cancelled)));
    assert!(!ran.run.guards.holds_vehicle());
    assert!(!ran.run.guards.link_unconfirmed());
    drop(waiting.holder);
}

/// ADR-399 item 3: 17 printable characters that are not a well-formed VIN (here an `I`) are
/// `NotWellFormed`: no VIN, no lock; the log still has what was read.
#[tokio::test(flavor = "multi_thread")]
async fn text_that_is_not_a_well_formed_vin_establishes_nothing() {
    let fake = fake_with(&[(A.0, "WVWZZZ1JZ3W38675I")]);
    let ran = run(reading_all(&[reader(0x90, A)]), fake).await;
    let report = ran.report();
    assert_eq!(outcomes(report), [vec![VinOutcome::NotWellFormed]]);
    assert!(report.vin().is_none() && report.vins_seen().is_empty());
    assert_eq!(report.vin_reads()[0].accepted, None);
    assert!(!report.vehicle_lock_held() && !ran.run.guards.holds_vehicle());
    assert!(!report.vin_interrupted());
}

/// ADR-399 item 5 and ADR-392: a receive error on a VIN read sends it again (a read is
/// idempotent), with the origin `repeat`, and the answer then decodes.
#[tokio::test(flavor = "multi_thread")]
async fn a_receive_error_on_a_vin_read_sends_it_again() {
    let fake = fake_with(&[(A.0, VIN_A)]);
    fake.state().scripts = VecDeque::from([
        vec![Item::Finish],
        vec![Item::RxError, Item::Finish],
        vec![Item::Routed],
    ]);
    let ran = run(reading_all(&[reader(0x90, A)]), fake.clone()).await;
    let report = ran.report();
    assert_eq!(outcomes(report), [vec![VinOutcome::Decoded(vin(VIN_A))]]);
    assert_eq!(report.vin(), Some(&vin(VIN_A)));
    assert_eq!(
        origins(&ran),
        ["identification", "identification", "repeat"]
    );
    assert_eq!(
        fake.count("ModuleConnect"),
        2,
        "the same link carried the resend"
    );
}

/// ADR-399 item 3: a link the worker refuses when the read is made (the connect, or any other
/// failure to open that leaves nothing open) is `WorkerRefused`, not `LinkRefused`, and the other
/// reads decide.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_the_worker_refuses_is_reported_and_the_other_reads_decide() {
    for connect in [true, false] {
        let documents = [reader(0x90, A), reader(0x90, B)];
        let fake = fake_with(&[(A.0, VIN_A), (B.0, VIN_A)]);
        let refusal = tonic::Status::invalid_argument("refused");
        // The presence link is the first of each; the first read's link is the second.
        if connect {
            fake.state().connect_fail_at = Some((2, refusal));
        } else {
            fake.state().create_fail_at = Some((2, refusal));
        }
        let ran = run(reading_all(&documents), fake.clone()).await;
        let report = ran.report();
        assert_eq!(
            outcomes(report),
            [
                vec![VinOutcome::WorkerRefused],
                vec![VinOutcome::Decoded(vin(VIN_A))]
            ],
            "connect {connect}"
        );
        assert_eq!(report.vin(), Some(&vin(VIN_A)));
        assert!(!report.vin_interrupted() && report.vehicle_lock_held());
        assert_eq!(fake.state().served.len(), 1);
        assert_eq!(fake.state().open_links, 0);
        assert!(!ran.run.guards.link_unconfirmed());
    }
}

/// A source whose worker exits as the last read's link is closed: the status question after the
/// close hanging says so once, as a launcher that owns the process does (ADR-394).
struct ExitsWhenTheCloseHangs(WorkerClient, Fake, Arc<Mutex<bool>>);

impl WorkerSource for ExitsWhenTheCloseHangs {
    type Client = WorkerClient;
    fn client(&mut self) -> Result<WorkerClient, HostError> {
        Ok(self.0.clone())
    }
    fn status(&mut self) -> WorkerStatus {
        let mut reported = self.2.lock().unwrap();
        if self.1.state().disconnect_hung && !*reported {
            *reported = true;
            WorkerStatus::Exited(WorkerExit {
                pid: Some(7),
                how: crate::client::ExitHow::Code(1),
            })
        } else {
            WorkerStatus::Alive
        }
    }
    fn end(&mut self) -> Result<WorkerExit, HostError> {
        Err(HostError::Setup("the worker cannot be ended"))
    }
    fn pid(&self) -> Option<u32> {
        Some(8)
    }
}

/// Runs a plan reading one VIN where the close of the read's link (the second disconnect) hangs,
/// so the guards are marked, on a source `build` makes from the worker and the fake. The
/// document on `A` is read at the VIN's data identifier.
async fn run_with_the_last_close_hanging<S: WorkerSource>(
    build: impl FnOnce(WorkerClient, Fake) -> S,
) -> IdentificationRun {
    run_with_the_last_close_hanging_plan(reading_all(&[reader(0x90, A)]), 2, build).await
}

/// [`run_with_the_last_close_hanging`] over `plan`, where the `hang_at`-th disconnect hangs.
async fn run_with_the_last_close_hanging_plan<S: WorkerSource>(
    plan: CheckedPlan,
    hang_at: usize,
    build: impl FnOnce(WorkerClient, Fake) -> S,
) -> IdentificationRun {
    let fake = fake_with(&[(A.0, VIN_A)]);
    fake.state().hang_disconnect_at = Some(hang_at);
    let dir = tempfile::tempdir().unwrap();
    let guards = JobGuards::take_vci_only(
        &GuardSetup {
            dir: dir.path().join("locks"),
            vci: "fake".to_owned(),
        },
        Duration::from_millis(1),
        &AtomicBool::new(false),
    )
    .unwrap();
    let source = build(serve(fake.clone()).await, fake);
    run_identification(
        source,
        plan,
        limits(),
        guards,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .await
}

/// ADR-399 item 4 and ADR-258: a worker that exited while the last VIN link was closed is found
/// by the settle step before the promotion, which clears the mark, and the lock is taken.
#[tokio::test(flavor = "multi_thread")]
async fn a_worker_that_exited_in_the_last_close_does_not_keep_the_lock_from_the_vin() {
    let run = run_with_the_last_close_hanging(|worker, fake| {
        ExitsWhenTheCloseHangs(worker, fake, Arc::new(Mutex::new(false)))
    })
    .await;
    let report = run
        .result
        .as_ref()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(report.vin(), Some(&vin(VIN_A)));
    assert!(report.vehicle_lock_held() && run.guards.holds_vehicle());
    assert!(!run.guards.link_unconfirmed());
}

/// ADR-399 item 4 (as ADR-401 amends it) and ADR-258: a live worker whose close of the last VIN
/// link failed leaves the guards marked; with no ECU read left to open, the VIN stays established
/// and reported, no lock is taken and the report comes back with the mark kept.
#[tokio::test(flavor = "multi_thread")]
async fn guards_still_marked_after_the_last_close_take_no_vehicle_lock() {
    let run = run_with_the_last_close_hanging(|worker, _| {
        ExitsOnce(worker, Arc::new(Mutex::new(0)), 0, false)
    })
    .await;
    let report = run
        .result
        .as_ref()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(report.vin(), Some(&vin(VIN_A)));
    assert!(!report.vehicle_lock_held() && !run.guards.holds_vehicle());
    assert!(run.guards.link_unconfirmed(), "the mark stays");
    assert_eq!(report.vin_rules()[0].outcome, RuleOutcome::Matched);
    assert!(!report.ecu_reads_interrupted());
}

/// ADR-401 (amending ADR-399 item 4) and ADR-396 item 6: guards still marked after the VIN's last
/// close, with an ECU read to open, end the run in `LinkUnconfirmed` before any ECU link opens;
/// the mark stays.
#[tokio::test(flavor = "multi_thread")]
async fn guards_still_marked_after_the_last_close_end_the_run_before_an_ecu_read() {
    let documents = [reader(0x90, A)];
    let plan = checked_vin_reading(&documents, Selection::Candidates, &[(0, 1, 1, "W")]).unwrap();
    let fake_ref = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&fake_ref);
    let run = run_with_the_last_close_hanging_plan(plan, 2, move |worker, fake| {
        *seen.lock().unwrap() = Some(fake);
        ExitsOnce(worker, Arc::new(Mutex::new(0)), 0, false)
    })
    .await;
    assert!(
        matches!(run.result, Err(JobError::LinkUnconfirmed)),
        "{:?}",
        run.result
    );
    assert!(run.guards.link_unconfirmed() && !run.guards.holds_vehicle());
    let fake = fake_ref.lock().unwrap().take().unwrap();
    assert_eq!(
        fake.count("ModuleConnect"),
        2,
        "the presence link and the VIN read, no ECU link"
    );
    assert_eq!(fake.state().served.len(), 1, "only the VIN request");
}

// ---------------------------------------------------------------- The ECU reads (ADR-401)

const PART_NUMBER: &str = "PA123";
const SW_VERSION: &str = "SW123";

/// A fake whose physical primitives are answered by simulated ECUs, one per request identifier,
/// each with its part number (F187) and software version (F189).
fn fake_ecus(ecus: &[(u32, &str, &str)]) -> Fake {
    let fake = Fake::default();
    for &(tx, part_number, sw_version) in ecus {
        fake.state().ecus.insert(
            tx,
            sim_ecu::SimEcu::new(sim_ecu::EcuConfig {
                part_number: part_number.to_owned(),
                sw_version: sw_version.to_owned(),
                ..Default::default()
            }),
        );
    }
    fake
}

/// A document on `endpoint` whose variant `name` reads F187 and expects a part number starting
/// with `expected`.
fn part_reader(name: &str, expected: &[u8], endpoint: (u32, u32)) -> (Sha256, Vec<u8>) {
    identified_by(0x87, 11, expected, name, endpoint, |_| {})
}

const ENGINE: &[(&str, bool, &[&str])] = &[("engine", false, &["D"])];

/// The identification requests the ECUs were given, as `(request identifier, request)`.
fn served(fake: &Fake) -> Vec<(u32, Vec<u8>)> {
    fake.state().served.clone()
}

fn declarations(report: &IdentificationReport, read: usize) -> Vec<&RequestOutcome> {
    report.ecu_reads()[read]
        .requests
        .iter()
        .flat_map(|request| &request.declarations)
        .map(|declaration| &declaration.outcome)
        .collect()
}

fn text(value: &str) -> RequestOutcome {
    RequestOutcome::Decoded(vec![IdentificationField {
        id: 1,
        short_name: "value".into(),
        value: FieldValue::Text(value.to_owned()),
    }])
}

fn sorted(mut hashes: Vec<Sha256>) -> Vec<Sha256> {
    hashes.sort();
    hashes
}

/// ADR-401 item 2: two variants of one definition on one link that both match the ECU's answer
/// send one request, whose one response both declarations decode; every match is reported, the
/// read is conclusive and the entry identified with both variants.
#[tokio::test(flavor = "multi_thread")]
async fn two_matching_variants_are_reported_in_full_from_one_request() {
    let documents = [part_reader("v1", b"P", A), part_reader("v2", b"PA", A)];
    let plan = ecu_plan(&documents, &[("D", &[0, 1])], &[("c1", ENGINE)]);
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(served(&fake), [(A.0, vec![0x22, 0xF1, 0x87])]);
    let read = &report.ecu_reads()[0];
    assert_eq!(report.ecu_reads().len(), 1);
    assert_eq!(read.requests.len(), 1);
    assert_eq!(
        declarations(report, 0),
        [&text("PA123      "), &text("PA123      ")]
    );
    assert_eq!(
        sorted(read.matching.clone()),
        sorted(vec![documents[0].0, documents[1].0])
    );
    assert!(read.conclusive && read.answered && !report.ecu_reads_interrupted());
    let EntryState::Identified {
        definition,
        variants,
    } = &report.entries()[0].state
    else {
        panic!("{:?}", report.entries());
    };
    assert_eq!(definition, "D");
    assert_eq!(
        sorted(variants.clone()),
        sorted(vec![documents[0].0, documents[1].0])
    );
    assert_eq!(origins(&ran), ["identification", "identification"]);
}

/// ADR-401 item 1: variants of one definition on two declared links are each identified on their
/// own link, one link open at a time, in plan order.
#[tokio::test(flavor = "multi_thread")]
async fn variants_on_two_declared_links_are_each_identified_on_their_own_link() {
    let documents = [part_reader("v1", b"PA", A), part_reader("v2", b"PB", B)];
    let plan = ecu_plan(&documents, &[("D", &[0, 1])], &[("c1", ENGINE)]);
    let fake = fake_ecus(&[(A.0, "PA1", SW_VERSION), (B.0, "PB1", SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(
        served(&fake),
        [(A.0, vec![0x22, 0xF1, 0x87]), (B.0, vec![0x22, 0xF1, 0x87])]
    );
    assert_eq!(report.ecu_reads().len(), 2);
    assert_eq!(report.ecu_reads()[0].matching, [documents[0].0]);
    assert_eq!(report.ecu_reads()[1].matching, [documents[1].0]);
    assert_eq!(
        fake.count("ModuleConnect"),
        3,
        "the presence link and two reads"
    );
    assert_eq!(fake.state().max_open_links, 1);
    assert_eq!(
        report.entries()[0].state,
        EntryState::Identified {
            definition: "D".into(),
            variants: vec![documents[0].0, documents[1].0]
        }
    );
}

/// ADR-401 item 4 and ADR-324: a variant whose declared link is refused leaves its definition
/// inconclusive at every endpoint, reported as a read with no endpoint; the entry is answered but
/// not identified, and a match is still a match.
#[tokio::test(flavor = "multi_thread")]
async fn a_variant_whose_link_is_refused_leaves_its_definition_inconclusive() {
    let refused = identified_by(0x87, 11, b"P", "v2", B, |p| p.extended_addressing = true);
    for (expected, identified) in [(b"Q", false), (b"P", true)] {
        let documents = [part_reader("v1", expected, A), refused.clone()];
        let plan = ecu_plan(&documents, &[("D", &[0, 1])], &[("c1", ENGINE)]);
        let fake = fake_ecus(&[
            (A.0, PART_NUMBER, SW_VERSION),
            (B.0, PART_NUMBER, SW_VERSION),
        ]);
        let ran = run(plan, fake.clone()).await;
        let report = ran.report();
        assert_eq!(report.ecu_reads().len(), 2);
        assert_eq!(declarations(report, 0), [&text("PA123      ")]);
        assert!(!report.ecu_reads()[0].conclusive);
        assert_eq!(report.ecu_reads()[1].endpoint, None);
        assert_eq!(declarations(report, 1), [&RequestOutcome::LinkRefused]);
        assert_eq!(
            served(&fake),
            [(A.0, vec![0x22, 0xF1, 0x87])],
            "no link for the refused one"
        );
        match (&report.entries()[0].state, identified) {
            (EntryState::Identified { variants, .. }, true) => {
                assert_eq!(variants, &[documents[0].0]);
            }
            (EntryState::AnsweredNotIdentified, false) => {}
            (state, _) => panic!("{state:?}"),
        }
    }
}

/// ADR-327 item 3 and ADR-401: two alternatives of one entry that both match make it ambiguous,
/// and both definitions are read.
#[tokio::test(flavor = "multi_thread")]
async fn two_matching_alternatives_make_the_entry_ambiguous() {
    let documents = [part_reader("v1", b"P", A), part_reader("v2", b"PA", A)];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[("c1", &[("engine", false, &["D1", "D2"])])],
    );
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(served(&fake).len(), 2, "neither definition is skipped");
    assert_eq!(report.ecu_reads()[0].matching, [documents[0].0]);
    assert_eq!(report.ecu_reads()[1].matching, [documents[1].0]);
    assert_eq!(
        report.entries()[0].state,
        EntryState::Ambiguous {
            definitions: vec!["D1".into(), "D2".into()]
        }
    );
}

/// ADR-327 item 3 and ADR-401: a negative response is inconclusive (not resent), so an entry read
/// only so is answered but not identified.
#[tokio::test(flavor = "multi_thread")]
async fn an_entry_read_inconclusively_is_answered_but_not_identified() {
    // The simulated ECU has no data identifier F1A0 and answers a request-out-of-range code.
    let documents = [identified_by(0xA0, 4, b"P", "v1", A, |_| {})];
    let plan = ecu_plan(&documents, &[("D", &[0])], &[("c1", ENGINE)]);
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(declarations(report, 0), [&RequestOutcome::Negative(0x31)]);
    let read = &report.ecu_reads()[0];
    assert!(read.answered && !read.conclusive && read.matching.is_empty());
    assert_eq!(
        served(&fake).len(),
        1,
        "a negative response is not sent again"
    );
    assert!(!origins(&ran).contains(&"repeat"));
    assert_eq!(report.entries()[0].state, EntryState::AnsweredNotIdentified);
}

/// ADR-327 item 3: a conclusive read without a match is an unrecognized variant at the endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn a_conclusive_read_without_a_match_is_an_unrecognized_variant() {
    let documents = [part_reader("v1", b"Q", A)];
    let plan = ecu_plan(&documents, &[("D", &[0])], &[("c1", ENGINE)]);
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(declarations(report, 0), [&text("PA123      ")]);
    assert!(report.ecu_reads()[0].conclusive && report.ecu_reads()[0].matching.is_empty());
    let EntryState::UnrecognizedVariant { endpoints } = &report.entries()[0].state else {
        panic!("{:?}", report.entries());
    };
    assert_eq!(endpoints.len(), 1);
    assert_eq!((endpoints[0].tx_id, endpoints[0].rx_id), A);
}

/// ADR-326 item 3 and ADR-392: a definition read at an address nothing answers is sent again
/// within the repeats and reported as no usable answer: not answering for a mandatory entry, not
/// fitted for an optional one.
#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_entry_is_not_answering_or_not_fitted() {
    let documents = [part_reader("v1", b"P", C)];
    let plan = ecu_plan(
        &documents,
        &[("D", &[0])],
        &[
            ("c1", &[("engine", false, &["D"])]),
            ("c2", &[("engine", true, &["D"])]),
        ],
    );
    let ran = run(plan, Fake::default()).await;
    let report = ran.report();
    assert_eq!(declarations(report, 0), [&RequestOutcome::NoUsableAnswer]);
    assert!(!report.ecu_reads()[0].answered);
    assert_eq!(report.entries()[0].state, EntryState::NotAnswering);
    assert_eq!(report.entries()[1].state, EntryState::NotFitted);
    let repeats = usize::from(limits().max_primitive_repeats);
    let mut expected = vec!["identification", "identification"];
    expected.extend(vec!["repeat"; repeats]);
    assert_eq!(origins(&ran), expected);
}

/// ADR-327 item 8 and ADR-401: a plan for a definition chosen by hand reads every definition it
/// names, and reports its entries as any plan does.
#[tokio::test(flavor = "multi_thread")]
async fn a_manual_plan_reads_every_definition() {
    let documents = [part_reader("v1", b"P", A), part_reader("v2", b"Q", B)];
    let plan = ecu_plan_of(
        Selection::ChosenDefinition,
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[("c1", &[("one", false, &["D1"]), ("two", false, &["D2"])])],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(report.selection(), Selection::ChosenDefinition);
    assert_eq!(report.candidates(), ["c1"]);
    assert_eq!(served(&fake).len(), 2);
    assert!(matches!(
        &report.entries()[0].state,
        EntryState::Identified { definition, .. } if definition == "D1"
    ));
    assert!(matches!(
        &report.entries()[1].state,
        EntryState::UnrecognizedVariant { .. }
    ));
}

/// ADR-327 items 4 and 8: a plan for every ECU has no entries and identifies each definition on
/// its own, in the order of item 4 (here the lowest identifier first, since nothing else
/// separates them), while `ecu_reads` stays in plan order (ADR-402).
#[tokio::test(flavor = "multi_thread")]
async fn a_plan_for_every_ecu_reads_each_definition_in_the_item_4_order() {
    let documents = [part_reader("v1", b"P", A), part_reader("v2", b"P", B)];
    let plan = ecu_plan(&documents, &[("D2", &[1]), ("D1", &[0])], &[]);
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(report.selection(), Selection::EveryEcu);
    assert!(report.entries().is_empty());
    assert_eq!(
        served(&fake).iter().map(|(tx, _)| *tx).collect::<Vec<_>>(),
        [A.0, B.0]
    );
    let definitions: Vec<_> = report
        .ecu_reads()
        .iter()
        .map(|r| r.definition.as_str())
        .collect();
    assert_eq!(definitions, ["D2", "D1"]);
    assert_eq!(read_order(report), [("D1", 0), ("D2", 0)]);
    assert!(
        report
            .ecu_reads()
            .iter()
            .all(|read| read.matching.len() == 1)
    );
}

/// ADR-327 item 3: a positive response that does not decode (here a longer field than the ECU
/// sends) is answered, undecodable and inconclusive.
#[tokio::test(flavor = "multi_thread")]
async fn a_positive_response_that_does_not_decode_is_undecodable_and_inconclusive() {
    let documents = [identified_by(0x89, 17, b"S", "v1", A, |_| {})];
    let plan = ecu_plan(&documents, &[("D", &[0])], &[("c1", ENGINE)]);
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(declarations(report, 0), [&RequestOutcome::Undecodable]);
    let read = &report.ecu_reads()[0];
    assert!(read.answered && !read.conclusive);
    assert_eq!(report.entries()[0].state, EntryState::AnsweredNotIdentified);
}

/// ADR-393 and ADR-401: a VCI lost during an ECU read opens the link again and sends the request
/// again with the origin `repeat`.
#[tokio::test(flavor = "multi_thread")]
async fn a_vci_lost_in_an_ecu_read_reopens_the_link_and_sends_it_again() {
    let documents = [part_reader("v1", b"P", A)];
    let plan = ecu_plan(&documents, &[("D", &[0])], &[("c1", ENGINE)]);
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    fake.state().scripts =
        VecDeque::from([vec![Item::Finish], vec![Item::VciLost], vec![Item::Routed]]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(report.ecu_reads()[0].matching, [documents[0].0]);
    assert_eq!(fake.count("ModuleConnect"), 3, "the link was opened again");
    assert_eq!(fake.state().max_open_links, 1);
    assert_eq!(
        origins(&ran),
        ["identification", "identification", "repeat"]
    );
}

/// ADR-396 item 6 and ADR-401: bounds that run out in an ECU read leave it and the ones after it
/// not read, report the phase interrupted and keep what the earlier phases heard.
#[tokio::test(flavor = "multi_thread")]
async fn bounds_that_run_out_in_an_ecu_read_leave_the_rest_not_read() {
    let documents = [part_reader("v1", b"P", A), part_reader("v2", b"P", B)];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[("c1", &[("engine", false, &["D1", "D2"])])],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    fake.state().scripts = VecDeque::from([
        vec![Item::Answer(A.1, POSITIVE), Item::Finish],
        vec![Item::VciLost],
    ]);
    let limits = JobLimits {
        max_link_reopens: 0,
        ..limits()
    };
    let ran = run_with(plan, fake.clone(), limits, false).await;
    let report = ran.report();
    assert!(report.ecu_reads_interrupted());
    assert_eq!(declarations(report, 0), [&RequestOutcome::NotRead]);
    assert_eq!(declarations(report, 1), [&RequestOutcome::NotRead]);
    assert_eq!(report.entries()[0].state, EntryState::NotRead);
    assert_eq!(
        ran.heard(),
        [(A.1, POSITIVE.to_vec())],
        "the presence section is kept"
    );
    assert!(served(&fake).is_empty());
    assert_eq!(fake.state().open_links, 0);
}

/// ADR-401: an interrupted presence phase, and a VIN phase whose bounds ran out, leave the ECU
/// phase unrun: no ECU link opens and every declaration is not read.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_presence_or_vin_phase_leaves_the_ecu_phase_unrun() {
    // Presence: the lost events run the repeats out.
    let documents = [reader(0x90, A), reader(0x90, B)];
    let plan = |documents: &[(Sha256, Vec<u8>)]| {
        checked_vin_reading(
            documents,
            Selection::Candidates,
            &[(0, 1, 1, "W"), (1, 1, 1, "W")],
        )
        .unwrap()
    };
    let fake = fake_with(&[(A.0, VIN_A), (B.0, VIN_A)]);
    fake.state().scripts = (0..5)
        .map(|_| vec![Item::Answer(A.1, POSITIVE), Item::LostEvents, Item::Finish])
        .collect();
    let tight = JobLimits {
        max_primitive_repeats: 1,
        ..limits()
    };
    let ran = run_with(plan(&documents), fake.clone(), tight, false).await;
    let report = ran.report();
    assert!(report.presence_interrupted() && report.ecu_reads_interrupted());
    assert!(
        report
            .ecu_reads()
            .iter()
            .flat_map(|read| &read.requests)
            .flat_map(|request| &request.declarations)
            .all(|declaration| declaration.outcome == RequestOutcome::NotRead)
    );
    assert!(fake.state().served.is_empty());
    assert_eq!(fake.count("ModuleConnect"), 1);
    // VIN: the second read loses the VCI on the request and again on the one sent again.
    let documents = [reader(0x90, A), reader(0x90, B)];
    let fake = fake_with(&[(A.0, VIN_A), (B.0, VIN_A)]);
    fake.state().scripts = VecDeque::from([
        vec![Item::Finish],
        vec![Item::Routed],
        vec![Item::VciLost],
        vec![Item::VciLost],
    ]);
    let ran = run_with(plan(&documents), fake.clone(), tight, false).await;
    let report = ran.report();
    assert!(report.vin_interrupted() && report.ecu_reads_interrupted());
    assert!(
        report
            .ecu_reads()
            .iter()
            .flat_map(|read| &read.requests)
            .flat_map(|request| &request.declarations)
            .all(|declaration| declaration.outcome == RequestOutcome::NotRead)
    );
    assert_eq!(
        fake.state().served.len(),
        1,
        "only the first VIN read was made"
    );
    assert_eq!(report.entries()[0].state, EntryState::NotRead);
}

/// ADR-399 item 3 as ADR-401 applies it: a link the worker refuses when an ECU read is made marks
/// that group, and the next definition is still read.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_the_worker_refuses_marks_the_group_and_the_next_definition_is_read() {
    let documents = [part_reader("v1", b"P", A), part_reader("v2", b"P", B)];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[("c1", &[("engine", false, &["D1", "D2"])])],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    // The presence link is the first connect; the first read's link is the second.
    fake.state().connect_fail_at = Some((2, tonic::Status::invalid_argument("refused")));
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(declarations(report, 0), [&RequestOutcome::WorkerRefused]);
    assert_eq!(declarations(report, 1), [&text("PA123      ")]);
    assert!(!report.ecu_reads_interrupted());
    assert_eq!(served(&fake), [(B.0, vec![0x22, 0xF1, 0x87])]);
    // The refused alternative had no physical read: the entry is not read, and the match of the
    // other alternative stays listed (ADR-401 item 6).
    assert_eq!(report.entries()[0].state, EntryState::NotRead);
    assert_eq!(report.ecu_reads()[1].matching, [documents[1].0]);
    assert_eq!(fake.state().open_links, 0);
}

/// ADR-399 item 3 and design 8.8.1 as ADR-401 applies them: no ECU request is sent while another
/// job holds the vehicle; the reads run once the holder lets go, with the lock held.
#[tokio::test(flavor = "multi_thread")]
async fn the_ecu_reads_wait_for_the_vehicle_lock_another_job_holds() {
    let documents = [reader(0x90, A)];
    let plan = checked_vin_reading(&documents, Selection::Candidates, &[(0, 1, 1, "W")]).unwrap();
    let waiting = waiting_for_the_vehicle_with(plan).await;
    assert_eq!(
        waiting.fake.state().served.len(),
        1,
        "only the VIN request while waiting"
    );
    drop(waiting.holder);
    let ran = tokio::time::timeout(Duration::from_secs(10), waiting.task)
        .await
        .expect("the run goes on once the holder let go")
        .unwrap();
    let report = ran.report();
    assert!(report.vehicle_lock_held() && ran.run.guards.holds_vehicle());
    assert_eq!(waiting.fake.state().served.len(), 2, "then the ECU read");
    assert_eq!(declarations(report, 0).len(), 1);
    assert!(report.ecu_reads()[0].answered);
}

/// ADR-401: a cancel that comes in during an ECU read ends the run in `Cancelled` before the next
/// link opens.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_during_an_ecu_read_ends_the_run() {
    let documents = [part_reader("v1", b"P", A), part_reader("v2", b"P", B)];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[("c1", &[("engine", false, &["D1", "D2"])])],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    let cancel = Arc::new(AtomicBool::new(false));
    // The presence request is the first primitive, the first read the second.
    fake.state().cancel_at_start = Some((2, Arc::clone(&cancel)));
    let ran = run_shared(
        plan,
        fake.clone(),
        limits(),
        cancel,
        tempfile::tempdir().unwrap(),
    )
    .await;
    assert!(matches!(ran.run.result, Err(JobError::Cancelled)));
    assert_eq!(served(&fake).len(), 1, "the second read was not made");
    assert_eq!(fake.state().open_links, 0);
}

/// ADR-401 item 8: a definition whose variants declare no pattern has no request, so no link
/// opens for it and its read is reported empty.
#[tokio::test(flavor = "multi_thread")]
async fn a_definition_without_identification_requests_opens_no_link() {
    let bare = named(edited(&part_reader("v1", b"P", A).1, |v| v.patterns = None));
    let documents = [bare];
    let plan = ecu_plan(&documents, &[("D", &[0])], &[("c1", ENGINE)]);
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    let read = &report.ecu_reads()[0];
    assert!(read.requests.is_empty() && read.matching.is_empty());
    assert!(!read.conclusive && !read.answered);
    assert_eq!(fake.count("ModuleConnect"), 1, "the presence link only");
    assert!(served(&fake).is_empty());
    assert_eq!(report.entries()[0].state, EntryState::NotRead);
}

/// ADR-401 item 6: an interruption in the second definition leaves every entry not read, though
/// the first definition matched; its match stays listed.
#[tokio::test(flavor = "multi_thread")]
async fn an_interruption_in_the_second_definition_leaves_the_entry_not_read() {
    let documents = [part_reader("v1", b"P", A), part_reader("v2", b"P", B)];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[("c1", &[("engine", false, &["D1", "D2"])])],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    fake.state().scripts =
        VecDeque::from([vec![Item::Finish], vec![Item::Routed], vec![Item::VciLost]]);
    let limits = JobLimits {
        max_link_reopens: 0,
        ..limits()
    };
    let ran = run_with(plan, fake, limits, false).await;
    let report = ran.report();
    assert!(report.ecu_reads_interrupted());
    assert_eq!(report.ecu_reads()[0].matching, [documents[0].0]);
    assert_eq!(declarations(report, 1), [&RequestOutcome::NotRead]);
    assert_eq!(report.entries()[0].state, EntryState::NotRead);
}

/// ADR-401 item 6: the same after the first definition went unanswered.
#[tokio::test(flavor = "multi_thread")]
async fn an_interruption_after_an_unanswered_definition_leaves_the_entry_not_read() {
    let documents = [part_reader("v1", b"P", C), part_reader("v2", b"P", B)];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[("c1", &[("engine", true, &["D1", "D2"])])],
    );
    let fake = fake_ecus(&[(B.0, PART_NUMBER, SW_VERSION)]);
    // The presence request, the three sends to C (the limit's two repeats), then the VCI lost.
    fake.state().scripts = VecDeque::from([
        vec![Item::Finish],
        vec![Item::Finish],
        vec![Item::Finish],
        vec![Item::Finish],
        vec![Item::VciLost],
    ]);
    let limits = JobLimits {
        max_link_reopens: 0,
        ..limits()
    };
    let ran = run_with(plan, fake, limits, false).await;
    let report = ran.report();
    assert!(report.ecu_reads_interrupted());
    assert_eq!(declarations(report, 0), [&RequestOutcome::NoUsableAnswer]);
    assert_eq!(report.entries()[0].state, EntryState::NotRead);
}

/// ADR-401 item 6 and ADR-324: a refused-link alternative is an inconclusive read. Beside an
/// unanswered one an optional entry is not fitted; beside a conclusive read without a match the
/// entry is answered but not identified; alone it had no physical read, so the entry is not read.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_link_alternative_makes_the_entry_inconclusive() {
    let refused = identified_by(0x87, 11, b"P", "v2", B, |p| p.extended_addressing = true);
    let mandatory: &[(&str, bool, &[&str])] = &[("engine", false, &["D1", "D2"])];
    let optional: &[(&str, bool, &[&str])] = &[("engine", true, &["D1", "D2"])];
    // A: optional {D1 at C, D2 refused}.
    let documents = [part_reader("v1", b"P", C), refused.clone()];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[("c1", optional)],
    );
    let ran = run(plan, Fake::default()).await;
    assert_eq!(ran.report().entries()[0].state, EntryState::NotFitted);
    // B: {D1 at A, no match; D2 refused}.
    let documents = [part_reader("v1", b"Q", A), refused.clone()];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[("c1", mandatory)],
    );
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    assert_eq!(
        ran.report().entries()[0].state,
        EntryState::AnsweredNotIdentified
    );
    // C: the only alternative is refused.
    let documents = [refused];
    let plan = ecu_plan(
        &documents,
        &[("D2", &[0])],
        &[("c1", &[("engine", false, &["D2"])])],
    );
    let ran = run(plan, Fake::default()).await;
    assert_eq!(ran.report().entries()[0].state, EntryState::NotRead);
}

/// ADR-401 item 1: a definition with two link groups at one endpoint (ComParams other than the
/// timings differ) opens a link for each and puts each group's outcomes on its own requests.
#[tokio::test(flavor = "multi_thread")]
async fn a_definition_with_two_link_groups_at_one_endpoint_reads_each_group() {
    let block = |size: i64| {
        move |p: &mut diag_ir::declaration::ProtocolT| {
            p.functional_id = 0;
            let mut param = diag_ir::declaration::ComParamT::default();
            param.short_name = Some("CP_BlockSize".into());
            param.value = size;
            p.params.get_or_insert_default().push(param);
        }
    };
    let documents = [
        identified_by(0x87, 11, b"PA", "v1", A, block(8)),
        identified_by(0x89, 5, b"SW", "v2", A, block(16)),
    ];
    let plan = ecu_plan(&documents, &[("D", &[0, 1])], &[("c1", ENGINE)]);
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(report.ecu_reads().len(), 1);
    let read = &report.ecu_reads()[0];
    assert_eq!(read.requests.len(), 2);
    assert_eq!(read.requests[0].request, [0x22, 0xF1, 0x87]);
    assert_eq!(read.requests[1].request, [0x22, 0xF1, 0x89]);
    assert_eq!(
        declarations(report, 0),
        [&text("PA123      "), &text("SW123")]
    );
    assert_eq!(read.matching.len(), 2);
    assert_eq!(fake.count("ModuleConnect"), 2, "one link per group");
    assert_eq!(fake.state().max_open_links, 1);
}

/// ADR-401 item 7: a plan with no VIN rule whose last presence close leaves the guards marked
/// ends the run in `LinkUnconfirmed` before an ECU link opens.
#[tokio::test(flavor = "multi_thread")]
async fn guards_marked_by_the_last_presence_close_end_the_run_before_an_ecu_read() {
    let documents = [part_reader("v1", b"P", A)];
    let plan = ecu_plan(&documents, &[("D", &[0])], &[("c1", ENGINE)]);
    let fake_ref = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&fake_ref);
    let run = run_with_the_last_close_hanging_plan(plan, 1, move |worker, fake| {
        *seen.lock().unwrap() = Some(fake);
        ExitsOnce(worker, Arc::new(Mutex::new(0)), 0, false)
    })
    .await;
    assert!(
        matches!(run.result, Err(JobError::LinkUnconfirmed)),
        "{:?}",
        run.result
    );
    assert!(run.guards.link_unconfirmed());
    let fake = fake_ref.lock().unwrap().take().unwrap();
    assert_eq!(fake.count("ModuleConnect"), 1, "the presence link only");
    assert!(fake.state().served.is_empty());
}

// ------------------------------------------- Narrowing and the next ECU (ADR-402, ADR-327)

/// The candidates dropped, each with the definition whose read completed it.
fn dropped(report: &IdentificationReport) -> Vec<(&str, Option<&str>)> {
    report
        .dropped()
        .iter()
        .map(|d| (d.candidate.as_str(), d.after.as_deref()))
        .collect()
}

/// The definitions in the order read, with the worst case that chose each.
fn read_order(report: &IdentificationReport) -> Vec<(&str, usize)> {
    report
        .read_order()
        .iter()
        .map(|step| (step.definition.as_str(), step.worst_case))
        .collect()
}

const F187: &[u8] = &[0x22, 0xF1, 0x87];
const F189: &[u8] = &[0x22, 0xF1, 0x89];

/// A document on `endpoint` whose variant `name` reads the software version (F189) and expects
/// one starting with `expected`.
fn sw_reader(name: &str, expected: &[u8], endpoint: (u32, u32)) -> (Sha256, Vec<u8>) {
    identified_by(0x89, 5, expected, name, endpoint, |_| {})
}

/// Two candidates whose engines are the definitions D1 and D2 at one endpoint, which reads the
/// part number for D1 and the software version for D2.
fn two_engines_at_a(d1: (Sha256, Vec<u8>)) -> CheckedPlan {
    let documents = [d1, sw_reader("v2", b"SW", A)];
    ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[
            ("c1", &[("engine", false, &["D1"])]),
            ("c2", &[("engine", false, &["D2"])]),
        ],
    )
}

fn a_key(report: &IdentificationReport) -> EndpointKey {
    report.endpoints()[0].key.clone()
}

/// ADR-327 items 3 and 5, ADR-402: a read that identifies a definition no candidate lists at an
/// endpoint drops the candidate whose own definition there was read conclusively without a match;
/// the reads go in the tie-break order and the report says why and after which read.
#[tokio::test(flavor = "multi_thread")]
async fn an_identified_unlisted_definition_drops_the_candidate_whose_own_definition_was_ruled_out()
{
    let plan = two_engines_at_a(part_reader("v1", b"Q", A));
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(served(&fake), [(A.0, F187.to_vec()), (A.0, F189.to_vec())]);
    assert_eq!(read_order(report), [("D1", 2), ("D2", 2)]);
    assert_eq!(report.candidates(), ["c1", "c2"]);
    assert_eq!(report.remaining(), ["c2"]);
    assert_eq!(dropped(report), [("c1", Some("D2"))]);
    assert_eq!(
        report.dropped()[0].contradictions,
        [Contradiction::UnlistedDefinition {
            definition: "D2".into(),
            endpoint: a_key(report),
            ruled_out: vec!["D1".into()],
        }]
    );
    assert_eq!(
        report.entries()[0].state,
        EntryState::UnrecognizedVariant {
            endpoints: vec![a_key(report)]
        },
        "a dropped candidate's entry is judged from the same reads"
    );
    assert!(matches!(
        &report.entries()[1].state,
        EntryState::Identified { definition, .. } if definition == "D2"
    ));
}

/// ADR-327 item 3, ADR-402: a candidate whose own definition at the endpoint stayed inconclusive
/// (the ECU refused the request) is kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_candidate_whose_own_definition_is_inconclusive_at_the_endpoint_is_kept() {
    // The simulated ECU has no data identifier F1A0 and answers a request-out-of-range code.
    let plan = two_engines_at_a(identified_by(0xA0, 4, b"P", "v1", A, |_| {}));
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(declarations(report, 0), [&RequestOutcome::Negative(0x31)]);
    assert_eq!(report.remaining(), ["c1", "c2"]);
    assert!(report.dropped().is_empty());
    assert_eq!(report.entries()[0].state, EntryState::AnsweredNotIdentified);
    assert!(matches!(
        &report.entries()[1].state,
        EntryState::Identified { .. }
    ));
}

/// ADR-327 item 3, ADR-402: two candidates whose definitions both match are both kept, whether
/// they list one definition each or one lists both.
#[tokio::test(flavor = "multi_thread")]
async fn two_candidates_whose_definitions_both_match_are_both_kept() {
    let documents = [part_reader("v1", b"P", A), part_reader("v2", b"PA", A)];
    let definitions: &[(&str, &[usize])] = &[("D1", &[0]), ("D2", &[1])];
    let plan = ecu_plan(
        &documents,
        definitions,
        &[
            ("c1", &[("engine", false, &["D1"])]),
            ("c2", &[("engine", false, &["D2"])]),
        ],
    );
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(report.remaining(), ["c1", "c2"]);
    assert!(report.dropped().is_empty());
    assert!(
        report
            .entries()
            .iter()
            .all(|e| matches!(e.state, EntryState::Identified { .. }))
    );
    // The same with one candidate listing both: ambiguous for it, identified for the other.
    let plan = ecu_plan(
        &documents,
        definitions,
        &[
            ("c1", &[("engine", false, &["D1", "D2"])]),
            ("c2", &[("engine", false, &["D2"])]),
        ],
    );
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(report.remaining(), ["c1", "c2"]);
    assert!(report.dropped().is_empty());
    assert!(matches!(
        &report.entries()[0].state,
        EntryState::Ambiguous { definitions } if definitions == &["D1", "D2"]
    ));
    assert!(matches!(
        &report.entries()[1].state,
        EntryState::Identified { .. }
    ));
}

/// ADR-327 item 3, ADR-402: an ECU identified at an address no entry of a candidate uses drops
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlisted_definition_at_an_address_no_entry_uses_drops_nothing() {
    let documents = [part_reader("v1", b"P", A), part_reader("v3", b"P", B)];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D3", &[1])],
        &[
            ("c1", &[("engine", false, &["D1"])]),
            (
                "c2",
                &[("engine", false, &["D1"]), ("body", false, &["D3"])],
            ),
        ],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(served(&fake).len(), 2);
    assert_eq!(report.remaining(), ["c1", "c2"]);
    assert!(report.dropped().is_empty());
    assert_eq!(read_order(report), [("D1", 2), ("D3", 2)]);
}

/// ADR-327 item 4, ADR-402: the order is named by the most remaining candidates, then the most
/// endpoints that answered presence, then the lowest identifier; the plan's order does not count.
#[tokio::test(flavor = "multi_thread")]
async fn the_read_order_follows_the_tie_breaks_not_the_plan() {
    let documents = [
        part_reader("va", b"Q", A),
        part_reader("vc", b"P", B),
        part_reader("vb", b"P", A),
    ];
    // Plan order Da, Dc, Db. Db is named by two candidates, Dc and Da by one each.
    let plan = ecu_plan(
        &documents,
        &[("Da", &[0]), ("Dc", &[1]), ("Db", &[2])],
        &[
            ("c1", &[("x", false, &["Db"]), ("y", false, &["Dc"])]),
            ("c2", &[("x", false, &["Db"])]),
            ("c3", &[("x", false, &["Da"])]),
        ],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    // The presence request: only the ECU at B answers it.
    fake.state().scripts = VecDeque::from([vec![Item::Answer(B.1, POSITIVE), Item::Finish]]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(read_order(report), [("Db", 3), ("Dc", 3), ("Da", 3)]);
    assert_eq!(
        served(&fake).iter().map(|(tx, _)| *tx).collect::<Vec<_>>(),
        [A.0, B.0, A.0]
    );
    // Da, read last, is ruled out where Db was identified: c3, which lists only Da, is dropped.
    assert_eq!(dropped(report), [("c3", Some("Da"))]);
    let planned: Vec<_> = report
        .ecu_reads()
        .iter()
        .map(|r| r.definition.as_str())
        .collect();
    assert_eq!(planned, ["Da", "Dc", "Db"], "ecu_reads stays in plan order");
}

/// ADR-327 item 8, ADR-402: a plan for a definition chosen by hand whose definition a read
/// contradicts still reads every definition and drops nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_manual_plan_whose_definition_a_read_contradicts_reads_every_definition() {
    let documents = [part_reader("v1", b"Q", A), sw_reader("v2", b"SW", A)];
    let plan = ecu_plan_of(
        Selection::ChosenDefinition,
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[("c1", &[("engine", false, &["D1"])])],
    );
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(served(&fake).len(), 2);
    assert_eq!(report.remaining(), ["c1"]);
    assert!(report.dropped().is_empty());
    assert_eq!(read_order(report), [("D1", 1), ("D2", 1)]);
    assert!(report.ecu_reads()[1].matching.len() == 1);
    assert!(matches!(
        &report.entries()[0].state,
        EntryState::UnrecognizedVariant { .. }
    ));
}

/// ADR-327 item 4, ADR-402: a definition no remaining candidate names is not read: its
/// declarations are not in play, no link opens for it, and an entry that lists it is not read.
#[tokio::test(flavor = "multi_thread")]
async fn a_definition_no_remaining_candidate_names_is_not_read() {
    let documents = [
        part_reader("v1", b"Q", A),
        part_reader("v2", b"P", A),
        part_reader("v3", b"P", B),
        part_reader("v4", b"P", C),
    ];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1]), ("D3", &[2]), ("D4", &[3])],
        &[
            ("c1", &[("x", false, &["D1"]), ("y", false, &["D4"])]),
            ("c2", &[("x", false, &["D2"]), ("y", false, &["D3"])]),
        ],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(dropped(report), [("c1", Some("D2"))]);
    assert_eq!(read_order(report), [("D1", 2), ("D2", 2), ("D3", 1)]);
    assert_eq!(declarations(report, 3), [&RequestOutcome::NotInPlay]);
    assert_eq!(
        fake.count("ModuleConnect"),
        4,
        "the presence link and the three reads; none to C"
    );
    assert!(matches!(&report.entries()[1].state, EntryState::NotRead));
    assert!(matches!(
        &report.entries()[0].state,
        EntryState::UnrecognizedVariant { .. }
    ));
    assert!(matches!(
        &report.entries()[3].state,
        EntryState::Identified { .. }
    ));
}

/// ADR-327 items 3 and 5, ADR-402: a fact of a definition that has left play still counts, and the
/// loop ends when no candidate remains.
#[tokio::test(flavor = "multi_thread")]
async fn a_fact_of_a_definition_that_left_play_still_counts_and_the_loop_ends_with_no_candidate() {
    // A is a model of Da, B of Db: a vehicle that mixes two candidates.
    let documents = [
        part_reader("va", b"P", A),
        part_reader("vb", b"P", B),
        part_reader("vc", b"Q", A),
        part_reader("vd", b"Q", B),
    ];
    let plan = ecu_plan(
        &documents,
        &[("Da", &[0]), ("Db", &[1]), ("Dc", &[2]), ("Dd", &[3])],
        &[
            ("c1", &[("x", false, &["Dc"]), ("y", false, &["Db"])]),
            ("c2", &[("x", false, &["Da"]), ("y", false, &["Dd"])]),
        ],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(
        read_order(report),
        [("Da", 2), ("Db", 2), ("Dc", 2), ("Dd", 1)]
    );
    assert_eq!(dropped(report), [("c1", Some("Dc")), ("c2", Some("Dd"))]);
    assert_eq!(
        report.dropped()[1].contradictions,
        [Contradiction::UnlistedDefinition {
            definition: "Db".into(),
            endpoint: EndpointKey {
                tx_id: B.0,
                rx_id: B.1,
                ..a_key(report)
            },
            ruled_out: vec!["Dd".into()],
        }]
    );
    assert!(report.remaining().is_empty());
}

/// ADR-327 item 5, ADR-392, ADR-402: an endpoint nothing answers does not skip the other
/// definition that uses it: each is tried after its own read.
#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_endpoint_does_not_skip_the_other_definition_using_it() {
    let documents = [part_reader("v1", b"P", C), part_reader("v2", b"P", C)];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &[
            ("c1", &[("engine", false, &["D1"])]),
            ("c2", &[("engine", false, &["D2"])]),
        ],
    );
    let ran = run(plan, Fake::default()).await;
    let report = ran.report();
    assert_eq!(declarations(report, 0), [&RequestOutcome::NoUsableAnswer]);
    assert_eq!(declarations(report, 1), [&RequestOutcome::NoUsableAnswer]);
    assert_eq!(read_order(report), [("D1", 2), ("D2", 2)]);
    assert!(report.dropped().is_empty());
    assert!(
        report
            .entries()
            .iter()
            .all(|e| e.state == EntryState::NotAnswering)
    );
}

/// ADR-401 item 6, ADR-402: an interruption after a drop keeps the drop and what remained, leaves
/// the definition it ended in and the ones still in play not read, the definitions no remaining
/// candidate names not in play, and every entry not read.
#[tokio::test(flavor = "multi_thread")]
async fn an_interruption_keeps_the_candidates_dropped_so_far() {
    let documents = [
        part_reader("v1", b"Q", A),
        part_reader("v2", b"P", A),
        part_reader("v3", b"P", B),
        part_reader("v4", b"P", B),
    ];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1]), ("D3", &[2]), ("D4", &[3])],
        &[
            (
                "c1",
                &[("engine", false, &["D1"]), ("extra", false, &["D4"])],
            ),
            (
                "c2",
                &[("engine", false, &["D2"]), ("body", false, &["D3"])],
            ),
        ],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    // The presence request and the first two reads, then the VCI is lost in the third read.
    fake.state().scripts = VecDeque::from([
        vec![Item::Finish],
        vec![Item::Routed],
        vec![Item::Routed],
        vec![Item::VciLost],
    ]);
    let limits = JobLimits {
        max_link_reopens: 0,
        ..limits()
    };
    let ran = run_with(plan, fake, limits, false).await;
    let report = ran.report();
    assert!(report.ecu_reads_interrupted());
    assert_eq!(dropped(report), [("c1", Some("D2"))]);
    assert_eq!(report.remaining(), ["c2"]);
    assert_eq!(read_order(report).len(), 3);
    assert_eq!(declarations(report, 2), [&RequestOutcome::NotRead]);
    assert_eq!(declarations(report, 3), [&RequestOutcome::NotInPlay]);
    assert!(
        report
            .entries()
            .iter()
            .all(|e| e.state == EntryState::NotRead)
    );
}

/// ADR-327 item 3, ADR-399 item 6, ADR-402: a candidate none of whose VIN rules for a component
/// matched the established VIN is dropped before any ECU read, and the definitions only it names
/// are not in play; a rule that matched, an unevaluated rule and a manual plan keep the
/// candidate.
#[tokio::test(flavor = "multi_thread")]
async fn a_vin_rule_that_does_not_match_drops_the_candidate_before_the_ecu_reads() {
    // The definitions read F190, the data identifier the simulated ECUs give their VIN in, and
    // expect a VIN starting with "W"; the first document is also the VIN rules' source. The VIN's
    // tenth character is "3".
    let documents = [
        identified_by(0x90, 17, b"W", "v1", A, |_| {}),
        identified_by(0x90, 17, b"W", "v2", B, |_| {}),
    ];
    let definitions: &[(&str, &[usize])] = &[("D1", &[0]), ("D2", &[1])];
    let engines: &[CandidateEntries<'_>] = &[
        ("c1", &[("engine", false, &["D1"])]),
        ("c2", &[("engine", false, &["D2"])]),
    ];
    let f190 = vec![0x22, 0xF1, 0x90];
    let ecus = |ecus: &[u32]| {
        let fake = Fake::default();
        for &tx in ecus {
            fake.state().ecus.insert(
                tx,
                sim_ecu::SimEcu::new(sim_ecu::EcuConfig {
                    vin: VIN_A.to_owned(),
                    ..Default::default()
                }),
            );
        }
        fake
    };
    let plan = ecu_plan_ruled(
        Selection::Candidates,
        &documents,
        definitions,
        engines,
        &[("c1", 0, 10, 10, "L"), ("c2", 0, 10, 10, "3")],
    );
    let fake = ecus(&[A.0, B.0]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(report.vin(), Some(&vin(VIN_A)));
    assert_eq!(dropped(report), [("c1", None)]);
    assert_eq!(
        report.dropped()[0].contradictions,
        [Contradiction::VinRules {
            component: Component::ModelYear
        }]
    );
    assert_eq!(report.remaining(), ["c2"]);
    // The matched rule's value is a component value of the report (ADR-403).
    assert_eq!(
        report.component_values(),
        [ComponentValue {
            component: Component::ModelYear,
            value: "2020".into()
        }]
    );
    assert_eq!(read_order(report), [("D2", 1)]);
    assert_eq!(declarations(report, 0), [&RequestOutcome::NotInPlay]);
    assert_eq!(
        served(&fake),
        [(A.0, f190.clone()), (B.0, f190.clone())],
        "the VIN read, then D2's; D1 was not read"
    );
    assert_eq!(report.entries()[0].state, EntryState::NotRead);
    assert!(matches!(
        &report.entries()[1].state,
        EntryState::Identified { .. }
    ));

    // A manual plan keeps its candidate and reads the definition.
    let manual = ecu_plan_ruled(
        Selection::ChosenDefinition,
        &documents,
        definitions,
        &engines[..1],
        &[("c1", 0, 10, 10, "L")],
    );
    let ran = run(manual, ecus(&[A.0, B.0])).await;
    let report = ran.report();
    assert_eq!(report.vin_rules()[0].outcome, RuleOutcome::NotMatched);
    assert_eq!(report.remaining(), ["c1"]);
    assert!(report.dropped().is_empty());
    assert!(matches!(
        &report.entries()[0].state,
        EntryState::Identified { .. }
    ));

    // Per component, not per rule: one rule of the component matched.
    let one_matched = ecu_plan_ruled(
        Selection::Candidates,
        &documents,
        definitions,
        engines,
        &[("c1", 0, 10, 10, "L"), ("c1", 0, 10, 10, "3")],
    );
    let ran = run(one_matched, ecus(&[A.0, B.0])).await;
    let report = ran.report();
    assert_eq!(report.remaining(), ["c1", "c2"]);
    assert!(report.dropped().is_empty());

    // No VIN established (nothing answers the VIN read): the rule stays unevaluated and keeps its
    // candidate.
    let unevaluated = ecu_plan_ruled(
        Selection::Candidates,
        &documents,
        definitions,
        engines,
        &[("c1", 0, 10, 10, "L")],
    );
    let ran = run(unevaluated, ecus(&[B.0])).await;
    let report = ran.report();
    assert_eq!(report.vin(), None);
    assert_eq!(report.vin_rules()[0].outcome, RuleOutcome::Unevaluated);
    assert!(report.component_values().is_empty());
    assert_eq!(report.remaining(), ["c1", "c2"]);
    assert!(report.dropped().is_empty());
}

/// ADR-401 item 4, ADR-327 item 3, ADR-402: a definition of the candidate whose only variant has
/// a refused link is listed at every endpoint the candidate uses and is never ruled out, so an
/// identified unlisted definition does not drop the candidate.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_link_definition_keeps_its_candidate() {
    let refused = identified_by(0x87, 11, b"P", "v9", B, |p| p.extended_addressing = true);
    let documents = [
        part_reader("v1", b"Q", A),
        sw_reader("v2", b"SW", A),
        refused,
    ];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1]), ("D9", &[2])],
        &[
            ("c1", &[("engine", false, &["D1", "D9"])]),
            ("c2", &[("engine", false, &["D2"])]),
        ],
    );
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(
        read_order(report).iter().map(|s| s.0).collect::<Vec<_>>(),
        ["D1", "D2"]
    );
    assert_eq!(report.remaining(), ["c1", "c2"]);
    assert!(report.dropped().is_empty());
    assert_eq!(report.entries()[0].state, EntryState::AnsweredNotIdentified);
}

/// ADR-327 item 4, ADR-402: a definition that can never be conclusive has only the identified
/// outcome, which can drop a candidate, so its worst case is lower and it is read before a
/// definition that cannot drop anything.
#[tokio::test(flavor = "multi_thread")]
async fn the_worst_case_can_decide_the_read_order() {
    let refused = identified_by(0x87, 11, b"P", "vz2", C, |p| p.extended_addressing = true);
    let documents = [
        part_reader("v1", b"Q", A),
        part_reader("vz", b"P", A),
        refused,
        part_reader("vb", b"P", B),
    ];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("Dz", &[1, 2]), ("Db", &[3])],
        &[
            ("c1", &[("engine", false, &["D1"])]),
            (
                "c2",
                &[("engine", false, &["Dz"]), ("body", false, &["Db"])],
            ),
        ],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    let ran = run(plan, fake).await;
    let report = ran.report();
    assert_eq!(read_order(report), [("D1", 2), ("Dz", 1), ("Db", 1)]);
    assert_eq!(dropped(report), [("c1", Some("Dz"))]);
}

/// ADR-402: a match from a partly read definition counts as a fact when the bounds run out, so the
/// drop it completes is kept and the definitions only the dropped candidate names are not in play.
#[tokio::test(flavor = "multi_thread")]
async fn an_interruption_narrows_on_the_part_read() {
    let documents = [
        part_reader("v1", b"Q", A),
        part_reader("v2a", b"P", A),
        part_reader("v2b", b"P", B),
        part_reader("v9", b"P", C),
    ];
    let plan = ecu_plan(
        &documents,
        &[("D1", &[0]), ("D2", &[1, 2]), ("D9", &[3])],
        &[
            ("c1", &[("x", false, &["D1"]), ("w", false, &["D9"])]),
            ("c2", &[("x", false, &["D2"])]),
        ],
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    // The presence request, D1, D2 at A, then the VCI is lost in D2 at B.
    fake.state().scripts = VecDeque::from([
        vec![Item::Finish],
        vec![Item::Routed],
        vec![Item::Routed],
        vec![Item::VciLost],
    ]);
    let limits = JobLimits {
        max_link_reopens: 0,
        ..limits()
    };
    let ran = run_with(plan, fake, limits, false).await;
    let report = ran.report();
    assert!(report.ecu_reads_interrupted());
    assert_eq!(dropped(report), [("c1", Some("D2"))]);
    assert_eq!(report.remaining(), ["c2"]);
    assert_eq!(declarations(report, 2), [&RequestOutcome::NotRead]);
    assert_eq!(declarations(report, 3), [&RequestOutcome::NotInPlay]);
    assert!(
        report
            .entries()
            .iter()
            .all(|e| e.state == EntryState::NotRead)
    );
}

// ------------------------------------------------------ The rule fields (ADR-327 item 4, ADR-403)

/// The candidate and outcome of each rule on rule field `k`.
fn rule_outcomes(report: &IdentificationReport, k: usize) -> Vec<(&str, RuleOutcome)> {
    report.rule_fields()[k]
        .rules
        .iter()
        .map(|rule| (rule.candidate.as_str(), rule.outcome))
        .collect()
}

/// A `Text` value as the report shows an established field.
fn text_value(value: &str) -> Option<FieldValue> {
    Some(FieldValue::Text(value.to_owned()))
}

/// A document at `endpoint` whose service 1 reads the external-supply flag (FD06) as an unsigned
/// byte and whose pattern expects 1.
fn supply_reader(name: &str, endpoint: (u32, u32)) -> (Sha256, Vec<u8>) {
    let base = part_reader(name, b"P", endpoint);
    named(edited(&base.1, |variant| {
        variant.services = Some(vec![read_service(
            1,
            "supply",
            [0xFD, 0x06],
            DataKind::Uint,
            1,
        )]);
        variant.patterns.as_mut().unwrap()[0].expected = Some(vec![1]);
    }))
}

/// ADR-327 item 4, ADR-403: two rule sources for one component on one request, two fields of one
/// kind and length at different offsets, decode the one response to different values and leave
/// the component unevaluated, narrow nothing, and are both reported.
#[tokio::test(flavor = "multi_thread")]
async fn rule_sources_that_decode_a_response_to_different_values_leave_the_component_unevaluated() {
    let at_offset = |name: &str, bit_offset: u32| {
        named(edited(&part_reader(name, b"P", A).1, |variant| {
            let service = &mut variant.services.as_mut().unwrap()[0];
            let field = &mut service.positive.as_mut().unwrap().fields.as_mut().unwrap()[0];
            field.placement = diag_ir::declaration::Placement::Absolute;
            field.bit_offset = bit_offset;
            field.bit_length = 80;
        }))
    };
    // The response is `62 F1 87` and eleven bytes: ten from the first, ten from the next.
    let as_bytes = at_offset("v2", 32);
    let documents = [at_offset("v1", 24), as_bytes];
    let rules: [CandidateFieldRule<'_>; 2] = [
        ("c1", 0, 1, 1, Component::ModelYear, "2021", b"PA"),
        ("c2", 1, 1, 1, Component::ModelYear, "2022", b"A1"),
    ];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        &documents,
        &[("D", &[0, 1])],
        &[("c1", ENGINE), ("c2", ENGINE)],
        &[],
        &rules,
    );
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(
        served(&fake),
        [(A.0, F187.to_vec())],
        "one request, one send"
    );
    let read = &report.ecu_reads()[0];
    assert_eq!(read.requests.len(), 1);
    assert!(read.requests[0].declarations.iter().all(|d| d.pattern));
    assert_eq!(read.requests[0].declarations.len(), 2);
    assert!(read.conclusive);
    let field = &report.rule_fields()[0];
    assert_eq!(report.rule_fields().len(), 1);
    assert_eq!(
        field
            .sources
            .iter()
            .map(|s| s.outcome.clone())
            .collect::<Vec<_>>(),
        [
            RuleSourceOutcome::Decoded(FieldValue::Text("PA123     ".into())),
            RuleSourceOutcome::Decoded(FieldValue::Text("A123      ".into())),
        ]
    );
    assert_eq!(field.established, None);
    assert_eq!(
        rule_outcomes(report, 0),
        [
            ("c1", RuleOutcome::Unevaluated),
            ("c2", RuleOutcome::Unevaluated)
        ]
    );
    assert_eq!(report.remaining(), ["c1", "c2"]);
    assert!(report.dropped().is_empty() && report.component_values().is_empty());
}

/// ADR-327 item 4, ADR-403: rules for the model and for the model year on one request are each
/// established from the one response, judged apart, and a candidate with no rule of a component
/// matching on its established field is dropped naming that field.
#[tokio::test(flavor = "multi_thread")]
async fn rules_for_two_components_on_one_request_are_each_established_from_the_one_response() {
    let two_fields = named(edited(&part_reader("v1", b"P", A).1, |variant| {
        variant.services = Some(vec![two_field_service(1)]);
    }));
    let rules: [CandidateFieldRule<'_>; 4] = [
        ("c1", 0, 1, 1, Component::VehicleModel, "model-a", b"PA"),
        ("c1", 0, 1, 2, Component::ModelYear, "2023", b"123"),
        ("c2", 0, 1, 1, Component::VehicleModel, "model-b", b"PB"),
        ("c2", 0, 1, 2, Component::ModelYear, "2023", b"123"),
    ];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        std::slice::from_ref(&two_fields),
        &[("D", &[0])],
        &[("c1", ENGINE), ("c2", ENGINE)],
        &[],
        &rules,
    );
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(served(&fake), [(A.0, F187.to_vec())]);
    assert_eq!(report.ecu_reads()[0].requests.len(), 1);
    assert_eq!(report.rule_fields().len(), 2);
    let fields = report.rule_fields();
    assert_eq!(fields[0].component, Component::VehicleModel);
    assert_eq!(fields[0].established, text_value("PA"));
    assert_eq!(fields[1].component, Component::ModelYear);
    assert_eq!(fields[1].established, text_value("123      "));
    assert_eq!(
        rule_outcomes(report, 0),
        [
            ("c1", RuleOutcome::Matched),
            ("c2", RuleOutcome::NotMatched)
        ]
    );
    assert_eq!(
        rule_outcomes(report, 1),
        [("c1", RuleOutcome::Matched), ("c2", RuleOutcome::Matched)]
    );
    assert_eq!(dropped(report), [("c2", Some("D"))]);
    assert_eq!(
        report.dropped()[0].contradictions,
        [Contradiction::FieldRules {
            component: Component::VehicleModel,
            definition: "D".into(),
            endpoint: a_key(report),
            group: 0,
            request: F187.to_vec(),
        }]
    );
    assert_eq!(report.remaining(), ["c1"]);
    let values: Vec<_> = report
        .component_values()
        .iter()
        .map(|v| (v.component, v.value.as_str()))
        .collect();
    assert_eq!(
        values,
        [
            (Component::VehicleModel, "model-a"),
            (Component::ModelYear, "2023")
        ]
    );
    assert!(matches!(
        &report.entries()[0].state,
        EntryState::Identified { definition, .. } if definition == "D"
    ));
}

/// ADR-327 item 3 and 4, ADR-403: a rule on a service no pattern names is read with the
/// definition on its link (the request is sent once, flagged as no pattern's), the definition is
/// still conclusive, and a candidate whose rule field was established outside its own value is
/// dropped; a manual plan judges the rule and drops nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_rule_field_established_outside_a_candidates_values_drops_it() {
    let document = with_service(
        &part_reader("v1", b"P", A),
        read_service(2, "readSw", [0xF1, 0x89], DataKind::Ascii, 5),
    );
    let rules: [CandidateFieldRule<'_>; 2] = [
        ("c1", 0, 2, 1, Component::ModelYear, "2021", b"SW1"),
        ("c2", 0, 2, 1, Component::ModelYear, "2022", b"SW2"),
    ];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        std::slice::from_ref(&document),
        &[("D", &[0])],
        &[("c1", ENGINE), ("c2", ENGINE)],
        &[],
        &rules,
    );
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(served(&fake), [(A.0, F187.to_vec()), (A.0, F189.to_vec())]);
    assert_eq!(
        fake.count("ModuleConnect"),
        2,
        "the presence link and one read link"
    );
    let read = &report.ecu_reads()[0];
    assert_eq!(
        read.requests
            .iter()
            .map(|r| (r.request.as_slice(), r.declarations[0].pattern))
            .collect::<Vec<_>>(),
        [(F187, true), (F189, false)]
    );
    assert!(read.conclusive && read.answered);
    assert_eq!(report.rule_fields()[0].established, text_value("SW123"));
    assert_eq!(
        rule_outcomes(report, 0),
        [
            ("c1", RuleOutcome::Matched),
            ("c2", RuleOutcome::NotMatched)
        ]
    );
    assert_eq!(dropped(report), [("c2", Some("D"))]);
    assert_eq!(
        report.dropped()[0].contradictions,
        [Contradiction::FieldRules {
            component: Component::ModelYear,
            definition: "D".into(),
            endpoint: a_key(report),
            group: 0,
            request: F189.to_vec(),
        }]
    );
    assert_eq!(report.remaining(), ["c1"]);
    assert_eq!(report.component_values().len(), 1);
    assert_eq!(report.component_values()[0].value, "2021");

    // A manual plan judges the rule and keeps its candidate.
    let manual = ecu_plan_with_rules(
        Selection::ChosenDefinition,
        std::slice::from_ref(&document),
        &[("D", &[0])],
        &[("c2", ENGINE)],
        &[],
        &rules[1..],
    );
    let ran = run(manual, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(rule_outcomes(report, 0), [("c2", RuleOutcome::NotMatched)]);
    assert_eq!(report.remaining(), ["c2"]);
    assert!(report.dropped().is_empty());
    assert!(report.component_values().is_empty());
}

/// ADR-327 item 4, ADR-403: a definition whose rule field separates the candidates is read before
/// one that cannot drop anything, though the other sorts first and is named by as many candidates.
#[tokio::test(flavor = "multi_thread")]
async fn a_definition_whose_rule_field_separates_the_candidates_is_read_first() {
    let documents = [part_reader("v1", b"P", A), sw_reader("v2", b"S", B)];
    let rules: [CandidateFieldRule<'_>; 3] = [
        ("c1", 1, 1, 1, Component::ModelYear, "2021", b"SW1"),
        ("c2", 1, 1, 1, Component::ModelYear, "2022", b"SW2"),
        ("c3", 1, 1, 1, Component::ModelYear, "2023", b"SW3"),
    ];
    let entries: [CandidateEntries<'_>; 3] = [
        (
            "c1",
            &[("engine", false, &["D1"]), ("body", false, &["D2"])],
        ),
        (
            "c2",
            &[("engine", false, &["D1"]), ("body", false, &["D2"])],
        ),
        (
            "c3",
            &[("engine", false, &["D1"]), ("body", false, &["D2"])],
        ),
    ];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &entries,
        &[],
        &rules,
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(read_order(report), [("D2", 1), ("D1", 1)]);
    assert_eq!(served(&fake), [(B.0, F189.to_vec()), (A.0, F187.to_vec())]);
    assert_eq!(dropped(report), [("c2", Some("D2")), ("c3", Some("D2"))]);
    assert_eq!(report.remaining(), ["c1"]);
}

/// ADR-403: a rule's request the ECU refuses leaves its component unevaluated and the definition's
/// read conclusive (a rule-only request counts for nothing in conclusiveness), though the ECU is
/// reported as having answered.
#[tokio::test(flavor = "multi_thread")]
async fn a_rule_request_the_ecu_refuses_leaves_the_component_unevaluated_and_the_read_conclusive() {
    let document = with_service(
        &part_reader("v1", b"P", A),
        read_service(2, "readX", [0xF1, 0xA0], DataKind::Ascii, 4),
    );
    let rules: [CandidateFieldRule<'_>; 1] = [("c1", 0, 2, 1, Component::ModelYear, "2021", b"X")];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        std::slice::from_ref(&document),
        &[("D", &[0])],
        &[("c1", ENGINE), ("c2", ENGINE)],
        &[],
        &rules,
    );
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(
        report.rule_fields()[0].sources[0].outcome,
        RuleSourceOutcome::Negative(0x31)
    );
    assert_eq!(report.rule_fields()[0].established, None);
    assert_eq!(rule_outcomes(report, 0), [("c1", RuleOutcome::Unevaluated)]);
    let read = &report.ecu_reads()[0];
    assert!(read.conclusive && read.answered && !read.matching.is_empty());
    assert!(report.dropped().is_empty());
    assert_eq!(report.remaining(), ["c1", "c2"]);
}

/// ADR-403: a rule field that was not read says why: its definition was not in play, its link was
/// refused, the worker refused the link, or nothing answered; each rule stays unevaluated and
/// nothing is dropped on it.
#[tokio::test(flavor = "multi_thread")]
async fn an_unread_rule_field_reports_why() {
    let only = |report: &IdentificationReport| {
        let field = &report.rule_fields()[0];
        assert_eq!(field.established, None);
        assert!(
            field
                .rules
                .iter()
                .all(|rule| rule.outcome == RuleOutcome::Unevaluated)
        );
        field.sources[0].outcome.clone()
    };
    // Not in play: a VIN rule drops the candidate that names the definition.
    let documents = [
        identified_by(0x90, 17, b"W", "v1", A, |_| {}),
        identified_by(0x90, 17, b"W", "v2", B, |_| {}),
    ];
    let engines: [CandidateEntries<'_>; 2] = [
        ("c1", &[("engine", false, &["D1"])]),
        ("c2", &[("engine", false, &["D2"])]),
    ];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &engines,
        &[("c1", 0, 10, 10, "L"), ("c2", 0, 10, 10, "3")],
        &[("c1", 0, 1, 1, Component::VehicleModel, "m", b"W")],
    );
    let ran = run(plan, fake_with(&[(A.0, VIN_A), (B.0, VIN_A)])).await;
    let report = ran.report();
    assert_eq!(dropped(report), [("c1", None)]);
    assert_eq!(only(report), RuleSourceOutcome::NotInPlay);

    // A refused link: the field has no endpoint and its source says so.
    let refused = identified_by(0x87, 11, b"P", "v9", B, |p| p.extended_addressing = true);
    let documents = [part_reader("v1", b"P", A), refused];
    let rule: [CandidateFieldRule<'_>; 1] = [("c1", 1, 1, 1, Component::VehicleModel, "m", b"PA")];
    let both: [CandidateEntries<'_>; 2] = [
        ("c1", &[("engine", false, &["D1", "D9"])]),
        ("c2", &[("engine", false, &["D1"])]),
    ];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        &documents,
        &[("D1", &[0]), ("D9", &[1])],
        &both,
        &[],
        &rule,
    );
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(report.rule_fields()[0].endpoint, None);
    assert_eq!(only(report), RuleSourceOutcome::LinkRefused);
    assert!(report.dropped().is_empty());

    // The worker refuses the read's link (the presence link is the first connect).
    let documents = [part_reader("v1", b"P", A), part_reader("v2", b"P", B)];
    let rule: [CandidateFieldRule<'_>; 1] = [("c1", 0, 1, 1, Component::VehicleModel, "m", b"PA")];
    let engines: [CandidateEntries<'_>; 2] = [
        ("c1", &[("engine", false, &["D1"])]),
        ("c2", &[("engine", false, &["D2"])]),
    ];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        &documents,
        &[("D1", &[0]), ("D2", &[1])],
        &engines,
        &[],
        &rule,
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    fake.state().connect_fail_at = Some((2, tonic::Status::invalid_argument("refused")));
    let ran = run(plan, fake).await;
    let report = ran.report();
    assert_eq!(only(report), RuleSourceOutcome::WorkerRefused);
    assert!(report.dropped().is_empty());

    // No ECU at the endpoint.
    let documents = [part_reader("v1", b"P", C)];
    let rule: [CandidateFieldRule<'_>; 1] = [("c1", 0, 1, 1, Component::VehicleModel, "m", b"PA")];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        &documents,
        &[("D", &[0])],
        &[("c1", ENGINE), ("c2", ENGINE)],
        &[],
        &rule,
    );
    let ran = run(plan, Fake::default()).await;
    let report = ran.report();
    assert_eq!(only(report), RuleSourceOutcome::NoUsableAnswer);
    assert!(report.dropped().is_empty());
}

/// ADR-306 item 3, ADR-403: an integer rule field is compared by number, whatever width the rule
/// writes its expected value in.
#[tokio::test(flavor = "multi_thread")]
async fn an_integer_rule_field_is_compared_by_number() {
    let documents = [supply_reader("v1", A)];
    let rules: [CandidateFieldRule<'_>; 3] = [
        ("c1", 0, 1, 1, Component::ModelYear, "2021", &[1]),
        ("c2", 0, 1, 1, Component::ModelYear, "2022", &[0, 1]),
        ("c3", 0, 1, 1, Component::ModelYear, "2023", &[2]),
    ];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        &documents,
        &[("D", &[0])],
        &[("c1", ENGINE), ("c2", ENGINE), ("c3", ENGINE)],
        &[],
        &rules,
    );
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(
        report.rule_fields()[0].established,
        Some(FieldValue::Unsigned(1))
    );
    assert_eq!(
        rule_outcomes(report, 0),
        [
            ("c1", RuleOutcome::Matched),
            ("c2", RuleOutcome::Matched),
            ("c3", RuleOutcome::NotMatched)
        ]
    );
    assert_eq!(dropped(report), [("c3", Some("D"))]);
    assert_eq!(report.remaining(), ["c1", "c2"]);
}

/// ADR-401 item 8 as ADR-403 amends it: a definition with no pattern that a field rule names is
/// read for the rule: its link opens and the request is sent, the read is never conclusive and
/// never matches, the rule is judged, and the entry is judged as if the definition had no
/// declaration.
#[tokio::test(flavor = "multi_thread")]
async fn a_field_rule_on_a_definition_without_patterns_is_still_read() {
    let document = named(edited(&part_reader("v1", b"P", A).1, |variant| {
        variant.patterns = None;
    }));
    let rules: [CandidateFieldRule<'_>; 2] = [
        ("c1", 0, 1, 1, Component::ModelYear, "2021", b"PA"),
        ("c2", 0, 1, 1, Component::ModelYear, "2022", b"XX"),
    ];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        std::slice::from_ref(&document),
        &[("D", &[0])],
        &[("c1", ENGINE), ("c2", ENGINE)],
        &[],
        &rules,
    );
    let fake = fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)]);
    let ran = run(plan, fake.clone()).await;
    let report = ran.report();
    assert_eq!(served(&fake), [(A.0, F187.to_vec())]);
    assert_eq!(fake.count("ModuleConnect"), 2);
    let read = &report.ecu_reads()[0];
    assert!(!read.conclusive && read.matching.is_empty() && read.answered);
    assert!(!read.requests[0].declarations[0].pattern);
    assert_eq!(
        rule_outcomes(report, 0),
        [
            ("c1", RuleOutcome::Matched),
            ("c2", RuleOutcome::NotMatched)
        ]
    );
    assert_eq!(dropped(report), [("c2", Some("D"))]);
    assert_eq!(report.entries()[0].state, EntryState::NotRead);
}

/// ADR-402 as ADR-403 extends it: an interruption after a rule field was established keeps the
/// drop it gave, leaves the sources not sent not read, and the definitions only the dropped
/// candidate names not in play.
#[tokio::test(flavor = "multi_thread")]
async fn an_interruption_keeps_the_drop_of_an_established_rule_field() {
    let documents = [
        sw_reader("v2a", b"SW", A),
        sw_reader("v2b", b"SW", B),
        part_reader("v9", b"P", C),
    ];
    let rules: [CandidateFieldRule<'_>; 4] = [
        ("c1", 0, 1, 1, Component::ModelYear, "2021", b"XX"),
        ("c1", 1, 1, 1, Component::ModelYear, "2021", b"XX"),
        ("c2", 0, 1, 1, Component::ModelYear, "2022", b"SW"),
        ("c2", 1, 1, 1, Component::ModelYear, "2022", b"SW"),
    ];
    let entries: [CandidateEntries<'_>; 2] = [
        ("c1", &[("x", false, &["D2"]), ("w", false, &["D9"])]),
        ("c2", &[("x", false, &["D2"])]),
    ];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        &documents,
        &[("D2", &[0, 1]), ("D9", &[2])],
        &entries,
        &[],
        &rules,
    );
    let fake = fake_ecus(&[
        (A.0, PART_NUMBER, SW_VERSION),
        (B.0, PART_NUMBER, SW_VERSION),
    ]);
    // The presence request, D2 at A, then the VCI is lost in D2 at B.
    fake.state().scripts =
        VecDeque::from([vec![Item::Finish], vec![Item::Routed], vec![Item::VciLost]]);
    let limits = JobLimits {
        max_link_reopens: 0,
        ..limits()
    };
    let ran = run_with(plan, fake, limits, false).await;
    let report = ran.report();
    assert!(report.ecu_reads_interrupted());
    assert_eq!(report.rule_fields()[0].established, text_value("SW123"));
    assert_eq!(
        report.rule_fields()[1].sources[0].outcome,
        RuleSourceOutcome::NotRead
    );
    assert_eq!(dropped(report), [("c1", Some("D2"))]);
    assert_eq!(report.remaining(), ["c2"]);
    assert_eq!(declarations(report, 2), [&RequestOutcome::NotInPlay]);
}

/// ADR-403: the component values are those of the rules of the candidates that remain; a dropped
/// candidate's matched rule gives none, though its source still counted in establishing the field.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_candidates_matched_rule_gives_no_component_value() {
    let two_fields = named(edited(&part_reader("v1", b"P", A).1, |variant| {
        variant.services = Some(vec![two_field_service(1)]);
    }));
    let rules: [CandidateFieldRule<'_>; 4] = [
        ("c1", 0, 1, 1, Component::VehicleModel, "model-a", b"PA"),
        ("c1", 0, 1, 2, Component::ModelYear, "2023", b"123"),
        // c2's model matches but its model year does not: it is dropped.
        ("c2", 0, 1, 1, Component::VehicleModel, "model-x", b"PA"),
        ("c2", 0, 1, 2, Component::ModelYear, "2099", b"999"),
    ];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        std::slice::from_ref(&two_fields),
        &[("D", &[0])],
        &[("c1", ENGINE), ("c2", ENGINE)],
        &[],
        &rules,
    );
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(dropped(report), [("c2", Some("D"))]);
    assert_eq!(
        rule_outcomes(report, 0),
        [("c1", RuleOutcome::Matched), ("c2", RuleOutcome::Matched)]
    );
    let values: Vec<_> = report
        .component_values()
        .iter()
        .map(|v| v.value.as_str())
        .collect();
    assert_eq!(values, ["model-a", "2023"]);
}

/// ADR-403 as it amends ADR-401 item 4: a variant with no pattern whose link ADR-324 refuses and
/// that carries only a request read for a field rule does not make its definition inconclusive,
/// so the definition's read at its endpoint stays conclusive.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_variant_with_only_a_rule_request_leaves_the_definition_conclusive() {
    let refused = named(edited(
        &identified_by(0x87, 11, b"P", "v9", B, |p| p.extended_addressing = true).1,
        |variant| variant.patterns = None,
    ));
    let documents = [part_reader("v1", b"Q", A), refused];
    let rules: [CandidateFieldRule<'_>; 1] = [("c1", 1, 1, 1, Component::ModelYear, "2021", b"P")];
    let plan = ecu_plan_with_rules(
        Selection::Candidates,
        &documents,
        &[("D", &[0, 1])],
        &[("c1", ENGINE), ("c2", ENGINE)],
        &[],
        &rules,
    );
    let ran = run(plan, fake_ecus(&[(A.0, PART_NUMBER, SW_VERSION)])).await;
    let report = ran.report();
    assert_eq!(report.ecu_reads()[1].endpoint, None);
    assert_eq!(
        report.rule_fields()[0].sources[0].outcome,
        RuleSourceOutcome::LinkRefused
    );
    let read = &report.ecu_reads()[0];
    assert!(read.matching.is_empty());
    assert!(
        read.conclusive,
        "the rule-only refused variant rules nothing out"
    );
}
