//! The agent's job runner through [`GuardedClient`] against a fake worker (ADR-334): a read-only
//! job passes the guard on every call, and a state-changing request is refused before it reaches
//! the worker.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::guards::{GuardSetup, JobGuards};
#[cfg(debug_assertions)]
use agent::{HostError, JobError};
use agent::{JobLimits, JobRun, LinkConfig, run_program};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program};
use tokio::sync::mpsc;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use vci_service_interface as vci;
use vci_service_interface::vci_service_server::{VciService, VciServiceServer};

use super::*;
use crate::profile::{Setup, parse};

type Events = mpsc::Sender<Result<vci::EventNotification, tonic::Status>>;
type Reply<T> = Result<tonic::Response<T>, tonic::Status>;

fn unimplemented<T>() -> Reply<T> {
    Err(tonic::Status::unimplemented("not part of the fake"))
}

/// A worker that answers what the agent's link set-up and one send-receive need, and writes
/// every RPC it receives (with the names of the ComParams it is given) into `log`.
#[derive(Clone, Default)]
struct FakeWorker {
    log: Arc<Mutex<Vec<String>>>,
    /// ComParam names by the id given out for them.
    ids: Arc<Mutex<BTreeMap<String, u32>>>,
    events: Arc<Mutex<Option<Events>>>,
}

impl FakeWorker {
    fn note(&self, entry: impl Into<String>) {
        self.log.lock().unwrap().push(entry.into());
    }

    fn name_of(&self, id: u32) -> String {
        self.ids
            .lock()
            .unwrap()
            .iter()
            .find(|(_, v)| **v == id)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| format!("#{id}"))
    }

    fn names(&self, items: &[vci::ParamItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match &item.id {
                Some(vci::param_item::Id::ParamId(id)) => self.name_of(*id),
                Some(vci::param_item::Id::ParamName(name)) => name.to_ascii_lowercase(),
                None => String::new(),
            })
            .collect()
    }
}

#[tonic::codegen::async_trait]
impl VciService for FakeWorker {
    async fn get_module_ids(
        &self,
        _: tonic::Request<vci::GetModuleIdsRequest>,
    ) -> Reply<vci::ModuleIdsResponse> {
        self.note("GetModuleIds");
        unimplemented()
    }
    async fn module_connect(
        &self,
        _: tonic::Request<vci::ModuleConnectRequest>,
    ) -> Reply<vci::Response> {
        self.note("ModuleConnect");
        Ok(tonic::Response::new(Default::default()))
    }
    async fn module_disconnect(
        &self,
        _: tonic::Request<vci::ModuleDisconnectRequest>,
    ) -> Reply<vci::Response> {
        self.note("ModuleDisconnect");
        Ok(tonic::Response::new(Default::default()))
    }
    async fn get_version(
        &self,
        _: tonic::Request<vci::GetVersionRequest>,
    ) -> Reply<vci::VersionResponse> {
        self.note("GetVersion");
        // The simulator's identity, which the agent's policy takes for a VCI that may be
        // written to in a debug build (`policy::identify`): the guard, not the policy, has to
        // be what refuses a write here.
        Ok(tonic::Response::new(vci::VersionResponse {
            version_data: Some(vci::VersionData {
                vendor_name: "j2534-0404".to_owned(),
                hw_name: "NGR-SIM 1".to_owned(),
                fw_name: "sim-vci 1".to_owned(),
                pdu_api_sw_name: "J2534".to_owned(),
                ..Default::default()
            }),
        }))
    }
    async fn get_timestamp(
        &self,
        _: tonic::Request<vci::GetTimestampRequest>,
    ) -> Reply<vci::TimestampResponse> {
        unimplemented()
    }
    async fn get_resource_status(
        &self,
        _: tonic::Request<vci::GetResourceStatusRequest>,
    ) -> Reply<vci::ResourceStatusResponse> {
        unimplemented()
    }
    async fn get_resource_ids(
        &self,
        _: tonic::Request<vci::GetResourceIdsRequest>,
    ) -> Reply<vci::ResourceIdsResponse> {
        self.note("GetResourceIds");
        Ok(tonic::Response::new(vci::ResourceIdsResponse {
            resource_id_list: Some(vci::ResourceIdItem {
                resource_id_data_array: vec![vci::ResourceIdItemData {
                    module_handle: None,
                    resource_id_array: vec![1],
                }],
            }),
        }))
    }
    async fn get_conflicting_resources(
        &self,
        _: tonic::Request<vci::GetConflictingResourcesRequest>,
    ) -> Reply<vci::ConflictingResourcesResponse> {
        unimplemented()
    }
    async fn create_com_logical_link(
        &self,
        _: tonic::Request<vci::CreateComLogicalLinkRequest>,
    ) -> Reply<vci::ComLogicalLinkResponse> {
        self.note("CreateComLogicalLink");
        Ok(tonic::Response::new(vci::ComLogicalLinkResponse {
            cll_handle: Some(vci::ComLogicalLinkHandle {
                module_handle: 1,
                cll_handle: 1,
            }),
        }))
    }
    async fn destroy_com_logical_link(
        &self,
        _: tonic::Request<vci::DestroyComLogicalLinkRequest>,
    ) -> Reply<vci::Response> {
        self.note("DestroyComLogicalLink");
        Ok(tonic::Response::new(Default::default()))
    }
    async fn connect_com_logical_link(
        &self,
        _: tonic::Request<vci::ConnectComLogicalLinkRequest>,
    ) -> Reply<vci::Response> {
        self.note("ConnectComLogicalLink");
        Ok(tonic::Response::new(Default::default()))
    }
    async fn disconnect_com_logical_link(
        &self,
        _: tonic::Request<vci::DisconnectComLogicalLinkRequest>,
    ) -> Reply<vci::Response> {
        self.note("DisconnectComLogicalLink");
        Ok(tonic::Response::new(Default::default()))
    }
    async fn lock_resource(
        &self,
        _: tonic::Request<vci::LockResourceRequest>,
    ) -> Reply<vci::Response> {
        unimplemented()
    }
    async fn unlock_resource(
        &self,
        _: tonic::Request<vci::UnlockResourceRequest>,
    ) -> Reply<vci::Response> {
        unimplemented()
    }
    async fn get_com_param(
        &self,
        _: tonic::Request<vci::GetComParamRequest>,
    ) -> Reply<vci::ComParamResponse> {
        unimplemented()
    }
    async fn set_com_param(
        &self,
        request: tonic::Request<vci::SetComParamRequest>,
    ) -> Reply<vci::Response> {
        let item = request.into_inner().param_item.unwrap_or_default();
        self.note(format!("SetComParam:{}", self.names(&[item]).join(",")));
        Ok(tonic::Response::new(Default::default()))
    }
    async fn start_com_primitive(
        &self,
        request: tonic::Request<vci::StartComPrimitiveRequest>,
    ) -> Reply<vci::ComPrimitiveResponse> {
        let request = request.into_inner();
        self.note(format!("StartComPrimitive:{:02X?}", request.cop_data));
        let cop = vci::ComPrimitiveHandle {
            module_handle: 1,
            cll_handle: 1,
            cop_handle: 1,
        };
        // The ECU's answer to a DID read, and the end of the primitive.
        let sender = self.events.lock().unwrap().clone();
        if let Some(sender) = sender {
            let item = |data| {
                Ok(vci::EventNotification {
                    handle: None,
                    event_data: Some(vci::event_notification::EventData::Item(vci::EventItem {
                        cop_handle: Some(cop),
                        timestamp: 0,
                        data: Some(data),
                        cop_tag: None,
                        extra_error_info: None,
                    })),
                })
            };
            let result = vci::ResultData {
                data_bytes: vec![0x62, 0xF1, 0x90, 0x41],
                ..Default::default()
            };
            let _ = sender
                .send(item(vci::event_item::Data::ResultData(result)))
                .await;
            let _ = sender
                .send(item(vci::event_item::Data::CopStatus(
                    vci::PduComPrimitiveStatus::PduCopstFinished as i32,
                )))
                .await;
        }
        Ok(tonic::Response::new(vci::ComPrimitiveResponse {
            cop_handle: Some(cop),
        }))
    }
    async fn cancel_com_primitive(
        &self,
        _: tonic::Request<vci::CancelComPrimitiveRequest>,
    ) -> Reply<vci::Response> {
        self.note("CancelComPrimitive");
        Ok(tonic::Response::new(Default::default()))
    }
    async fn get_status(
        &self,
        _: tonic::Request<vci::GetStatusRequest>,
    ) -> Reply<vci::StatusResponse> {
        unimplemented()
    }
    async fn get_event_item(
        &self,
        _: tonic::Request<vci::GetEventItemRequest>,
    ) -> Reply<vci::EventItemResponse> {
        unimplemented()
    }
    type SubscribeEventStream = ReceiverStream<Result<vci::EventNotification, tonic::Status>>;
    async fn subscribe_event(
        &self,
        _: tonic::Request<vci::SubscribeEventRequest>,
    ) -> Reply<Self::SubscribeEventStream> {
        self.note("SubscribeEvent");
        let (sender, receiver) = mpsc::channel(16);
        *self.events.lock().unwrap() = Some(sender);
        Ok(tonic::Response::new(ReceiverStream::new(receiver)))
    }
    async fn io_ctl(&self, _: tonic::Request<vci::IoCtlRequest>) -> Reply<vci::IoCtlResponse> {
        unimplemented()
    }
    async fn get_object_id(
        &self,
        request: tonic::Request<vci::GetObjectIdRequest>,
    ) -> Reply<vci::ObjectIdResponse> {
        let name = request.into_inner().shortname.to_ascii_lowercase();
        self.note(format!("GetObjectId:{name}"));
        let mut ids = self.ids.lock().unwrap();
        let next = u32::try_from(ids.len() + 1).unwrap();
        let id = *ids.entry(name).or_insert(next);
        Ok(tonic::Response::new(vci::ObjectIdResponse {
            pdu_object_id: id,
        }))
    }
    async fn get_unique_resp_id_table(
        &self,
        _: tonic::Request<vci::GetUniqueRespIdTableRequest>,
    ) -> Reply<vci::UniqueRespIdTableResponse> {
        unimplemented()
    }
    async fn set_unique_resp_id_table(
        &self,
        request: tonic::Request<vci::SetUniqueRespIdTableRequest>,
    ) -> Reply<vci::Response> {
        let table = request
            .into_inner()
            .unique_resp_id_table
            .unwrap_or_default();
        let names: Vec<String> = table
            .unique_data
            .iter()
            .flat_map(|entry| self.names(&entry.params))
            .collect();
        self.note(format!("SetUniqueRespIdTable:{}", names.join(",")));
        Ok(tonic::Response::new(Default::default()))
    }
}

/// Serves the fake on a loopback port; returns a client for it.
async fn serve(fake: &FakeWorker) -> worker_host::client::WorkerClient {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let incoming = tonic::codegen::tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(VciServiceServer::new(fake.clone()))
            .serve_with_incoming(incoming),
    );
    let channel = tonic::transport::Endpoint::from_shared(format!("http://127.0.0.1:{port}"))
        .unwrap()
        .connect_lazy();
    vci::vci_service_client::VciServiceClient::with_interceptor(
        channel,
        worker_host::client::BearerAuth::new([0; 32], "test"),
    )
}

/// A T3 profile whose link pins a functional identifier and padding.
fn t3() -> Profile {
    parse(
        "class = \"T3\"\n[vci]\napi = \"j2534-0404\"\nlibrary = \"Vendor VCI\"\nmodule = \"VCI-1\"\n\
         label = \"bench\"\npname = \"Vendor VCI 1\"\nfirmware_version = \"1.2.3\"\n\
         library_version = \"4.5\"\n[link]\nprotocol = \"ISO_14229_3_on_ISO_15765_2\"\n\
         baud_rate = 500000\npins = [6, 14]\nrequest_id = 0x7E0\nresponse_id = 0x7E8\n\
         functional_id = 0x7DF\nid_bits = 11\naddressing = \"normal\"\npadding = 0xCC\n",
    )
    .unwrap()
}

/// A program that sends one request (`service` with the DID F190) and ends.
fn program(service: u16) -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![Op::PushBytes(0), Op::ServiceRequest { service }],
        constants: vec![vec![0xF1, 0x90]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    }
}

fn guards() -> JobGuards {
    take_guards(false)
}

#[cfg(debug_assertions)]
fn guards_with_writes() -> JobGuards {
    take_guards(true)
}

fn take_guards(writes: bool) -> JobGuards {
    let setup = GuardSetup {
        dir: std::env::temp_dir().join(format!(
            "ngr-hw-runner-guards-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )),
        vci: "VCI-1".to_owned(),
    };
    let (poll, never) = (
        Duration::from_millis(1),
        std::sync::atomic::AtomicBool::new(false),
    );
    if writes {
        JobGuards::take(&setup, poll, &never)
    } else {
        JobGuards::take_vci_only(&setup, poll, &never)
    }
    .expect("the guards are free")
}

struct Setup_ {
    fake: FakeWorker,
    client: GuardedClient,
    run: RunGuard,
    _dir: tempfile::TempDir,
}

async fn setup() -> Setup_ {
    let fake = FakeWorker::default();
    let profile = t3();
    let Setup::Link { vci, .. } = &profile.setup else {
        unreachable!()
    };
    let dir = tempfile::tempdir().unwrap();
    let run = RunGuard::start_in(&profile, "hw_runner", dir.path().join("reports"));
    let client = GuardedClient::attach(
        &profile,
        vci,
        serve(&fake).await,
        ModuleHandle { module_handle: 1 },
        HandleOptions::default(),
        &run,
    )
    .await
    .expect("the client holds the module");
    Setup_ {
        fake,
        client,
        run,
        _dir: dir,
    }
}

fn granted(trace: &Trace) -> Vec<&'static str> {
    trace
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, decision)| decision.is_ok())
        .map(|(rpc, _)| *rpc)
        .collect()
}

fn rpc_names(log: &[String]) -> Vec<&str> {
    log.iter()
        .map(|entry| entry.split(':').next().unwrap())
        .collect()
}

/// A read-only job through the guarded client: every worker call is one the guard granted, in
/// order; the agent neither connects nor disconnects the module; the profile's ComParams the
/// agent does not set (and the rest of the response table) are injected before the connect.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_job_passes_the_guard_on_every_call() {
    let Setup_ {
        fake,
        client,
        mut run,
        ..
    } = setup().await;
    let JobRun {
        result,
        guards: _guards,
        ..
    } = run_program(
        client.clone(),
        &LinkConfig::iso15765(0x7E0, 0x7E8),
        program(0x22),
        JobLimits::default(),
        guards(),
    )
    .await;
    result.expect("the read passes the guard");

    let log = fake.log.lock().unwrap().clone();
    // The worker saw exactly what the guard granted, in the same order.
    assert_eq!(rpc_names(&log), granted(&client.trace()), "{log:#?}");
    // The harness owns the module: the job opened and closed none.
    assert!(!log.iter().any(|e| e.starts_with("ModuleConnect")));
    assert!(!log.iter().any(|e| e.starts_with("ModuleDisconnect")));
    assert!(!log.iter().any(|e| e.starts_with("GetModuleIds")));
    // The injected ComParams and the completed table come before the connect.
    let at = |prefix: &str| {
        log.iter()
            .position(|e| e.starts_with(prefix))
            .unwrap_or_else(|| panic!("{prefix} not in {log:#?}"))
    };
    let connect = at("ConnectComLogicalLink");
    for injected in [
        "SetComParam:cp_canfuncreqid",
        "SetComParam:cp_testerpresentmessage",
        "SetComParam:cp_canfillerbyte",
        "SetComParam:cp_requestaddrmode",
    ] {
        assert!(at(injected) < connect, "{injected}");
    }
    let table = log
        .iter()
        .find(|e| e.starts_with("SetUniqueRespIdTable:"))
        .unwrap();
    for name in [
        "cp_canphysreqid",
        "cp_canphysreqformat",
        "cp_canrespusdtid",
        "cp_canrespusdtformat",
    ] {
        assert!(table.contains(name), "{table}");
    }
    assert!(at("SetUniqueRespIdTable") < connect);
    assert!(
        log.iter()
            .any(|e| e.starts_with("StartComPrimitive:[22, F1, 90]"))
    );

    // The run ends: the device-close step disconnects the module, once.
    run.pass();
    assert!(run.run_teardown().is_empty());
    let after = fake.log.lock().unwrap().clone();
    assert_eq!(after.iter().filter(|e| *e == "ModuleDisconnect").count(), 1);
}

/// A long job starts many primitives, none of whose events the handle reads: the guarded
/// disconnect cancels at most one (the latest), not one per primitive inside the runner's
/// deadline.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_with_many_primitives_costs_one_cancel_at_the_disconnect() {
    let Setup_ {
        fake,
        client,
        mut run,
        ..
    } = setup().await;
    let many = Program {
        code: (0..50)
            .flat_map(|_| [Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }])
            .collect(),
        ..program(0x22)
    };
    let JobRun {
        result,
        guards: _guards,
        ..
    } = run_program(
        client.clone(),
        &LinkConfig::iso15765(0x7E0, 0x7E8),
        many,
        JobLimits::default(),
        guards(),
    )
    .await;
    result.expect("the reads pass the guard");
    let log = fake.log.lock().unwrap().clone();
    let count = |prefix: &str| log.iter().filter(|e| e.starts_with(prefix)).count();
    assert_eq!(count("StartComPrimitive"), 50);
    assert_eq!(count("CancelComPrimitive"), 1, "{log:#?}");
    // And the trace still matches what the worker saw.
    assert_eq!(rpc_names(&log), granted(&client.trace()));
    run.pass();
    assert!(run.run_teardown().is_empty());
}

/// A job that asks for the module it does not own is refused, and the refusal (not a grant, since
/// nothing is sent) is what the trace records.
#[tokio::test(flavor = "multi_thread")]
async fn a_module_request_of_a_job_is_refused_and_traced_as_a_refusal() {
    use agent::client::WorkerApi;
    let Setup_ {
        fake,
        mut client,
        mut run,
        ..
    } = setup().await;
    let connect = client
        .module_connect(vci::ModuleConnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await;
    assert_eq!(connect.unwrap_err().code(), tonic::Code::PermissionDenied);
    // Granted for the held module's own handle, but nothing is sent, so nothing is recorded.
    let disconnect = client
        .module_disconnect(vci::ModuleDisconnectRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
        })
        .await;
    assert!(disconnect.is_err());
    assert!(fake.log.lock().unwrap().is_empty());
    let trace = client.trace().lock().unwrap().clone();
    assert_eq!(trace.len(), 1, "{trace:?}");
    assert_eq!(trace[0].0, "ModuleConnect");
    assert!(trace[0].1.is_err());
    run.pass();
    assert!(run.run_teardown().is_empty());
}

/// A write is refused by the guard before it reaches the worker, for a VCI the agent's own
/// policy takes for the simulator (so the policy does not stop it first).
#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn a_state_changing_request_is_refused_before_it_reaches_the_worker() {
    let Setup_ {
        fake,
        client,
        mut run,
        ..
    } = setup().await;
    // A write needs the reprogramming slot of the job's guards.
    let JobRun {
        result,
        guards: _guards,
        ..
    } = run_program(
        client.clone(),
        &LinkConfig::iso15765(0x7E0, 0x7E8),
        program(0x2E),
        JobLimits::default(),
        guards_with_writes(),
    )
    .await;
    let Err(JobError::Host {
        source: HostError::Rpc { rpc, status },
        ..
    }) = result
    else {
        panic!("the write is refused, not {result:?}");
    };
    assert_eq!(rpc, "StartComPrimitive");
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    let log = fake.log.lock().unwrap().clone();
    assert!(
        !log.iter().any(|e| e.starts_with("StartComPrimitive")),
        "{log:#?}"
    );
    // The refusal is in the trace, and the link was still closed.
    assert!(
        client
            .trace()
            .lock()
            .unwrap()
            .iter()
            .any(|(rpc, decision)| *rpc == "StartComPrimitive" && decision.is_err())
    );
    assert!(log.iter().any(|e| e == "DestroyComLogicalLink"));
    run.pass();
    assert!(run.run_teardown().is_empty());
}
