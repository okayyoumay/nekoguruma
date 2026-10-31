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

use super::tests::{checked, checked_vin, document, document_reading, named};
use super::*;
use crate::comm_log::{CommLogSetup, VciFacts};
use crate::guards::GuardSetup;

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
        let mut state = self.state();
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
        [VinRuleOutcome::NotMatched, VinRuleOutcome::Matched],
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
            .all(|r| r.outcome == VinRuleOutcome::Unevaluated)
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
    assert_eq!(accepted_vin(&[VinOutcome::NoAnswer, decoded(VIN_A)]), None);
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
/// `max_primitive_repeats` (origin `repeat`) and reported `NoAnswer`; a negative response is
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
            vec![VinOutcome::NoAnswer],
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
    assert_eq!(
        results,
        [VinRuleOutcome::Matched, VinRuleOutcome::NotMatched]
    );
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
        reading_all(&[reader(0x90, A)]),
        fake.clone(),
        limits(),
        Arc::clone(&cancel),
        dir,
    ));
    // The read is made and its link closed, then the run waits.
    for _ in 0..1000 {
        if !fake.state().served.is_empty() && fake.state().open_links == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
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
