//! `run_identification` against a scripted worker: a gRPC server on a loopback port that answers
//! the link's RPCs, refuses what a test tells it to, and gives each started primitive the
//! events its script lists (ADR-396).

use std::collections::VecDeque;
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

use super::tests::{checked, document, named};
use super::*;
use crate::comm_log::CommLogSetup;
use crate::guards::GuardSetup;

/// One event a started primitive gives, in order.
#[derive(Clone, Debug)]
enum Item {
    /// A result bound to the primitive, on the response identifier in its header.
    Answer(u32, &'static [u8]),
    /// A result bound to an earlier primitive: a late answer.
    Late(u32, &'static [u8]),
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
        ok()
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
        state.starts.push((request.cop_data, cycles));
        state.cops += 1;
        let cop_handle = 100 + state.cops;
        let script = state
            .scripts
            .pop_front()
            .unwrap_or_else(|| vec![Item::Finish]);
        let handle = ComPrimitiveHandle {
            module_handle: 1,
            cll_handle: 1,
            cop_handle,
        };
        let sender = state.events.clone();
        for item in script {
            let (cop, data) = match item {
                Item::Answer(id, bytes) => (Some(handle), result(id, bytes)),
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
    let run = run_identification(
        serve(fake.clone()).await,
        plan,
        limits,
        guards,
        Some(setup.clone()),
        Arc::new(AtomicBool::new(cancel)),
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
