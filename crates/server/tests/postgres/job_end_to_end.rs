//! Jobs created through `POST /api/v1/jobs` run on the real agent and the simulators (design 5.3,
//! 12, 13.4; ADR-477, ADR-480, ADR-481, ADR-511): the server runs in the test's process over a
//! throwaway PostgreSQL and the bundled RustFS, and `ngr-agent connect` runs as a process whose
//! worker is the real `j2534-0404-service` against `sim-vci`, with the fictional vehicle's ECM
//! behind it. An operator signs in, stores the two documents through the Web API and creates an
//! `acquire` job for the vehicle, which reads the VIN. Once its end is applied and the vehicle's
//! lock is freed, a `writeSettings` job for the same VIN runs `write-coding.star` and changes the
//! ECM's coding (README 11 of the fictional vehicle).
//!
//! Each job is checked from what the server stored: its row, its events in the agent's order, its
//! result object read back from the object store, and the one `jobCreated` row in `audit_log`.
//! The write job is checked on the simulator's side too: the request trace shows the session, the
//! seed, the key, the write and the read-back, and the ECU's state file holds the new coding.
//!
//! The agent is a copy of the built binary with a `workers` directory beside it, which is where
//! `connect` looks for workers. A debug agent takes the simulator's registration definition and
//! the simulator's files from its environment, so this module is compiled in debug builds, and on
//! Unix, where the agent's stop signal is SIGINT.
//!
//! The ECU's state is kept in a file, since each job launches a worker of its own.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diag_ir::{Value, VmState};
use server::Server;
use server::config::Http;
use server::job_queue::Settings;
use server::storage::{MultipartStore, ObjectKey, S3Store, S3Tuning};
use server::uploads::Backend;
use sim_ecu::SimResponse;
use sim_ecu::persist::load_ecu_state;

use crate::agent_connect::Agent;
use crate::agent_register::ngr_agent_binary::cargo_artifact::{Artifact, build_artifact_isolated};
use crate::agent_register::{AgentEnv, fingerprint, run, text};
use crate::documents::{name_of, put_document};
use crate::jobs::{add_user, login, password, post_to};
use crate::redeem::{World, json};
use crate::serving::Running;
use crate::storage::{read_all, sha256};
use crate::uploads::hex;

#[path = "../../../j2534-0404-service/tests/common/fictional_ecm.rs"]
mod fictional_ecm;
#[path = "../../../j2534-0404-service/tests/common/sim_worker.rs"]
mod sim_worker;

/// The longest the test waits for one step.
const LIMIT: Duration = Duration::from_secs(60);
const LIBRARY_NAME: &str = "sim-vci";
/// The VIN of the fictional vehicle's ECM (`sim/ecm.json`).
const VIN: &str = "NGRMKSA10VK000123";
/// The answer to a read of the coding (DID 0x0120) before the write job and after it.
const CODING_BEFORE: [u8; 7] = [0x62, 0x01, 0x20, 0x00, 0x01, 0x03, 0x20];
const CODING_AFTER: [u8; 7] = [0x62, 0x01, 0x20, 0x00, 0x03, 0x03, 0x20];

/// The simulator's surroundings and the agent's executable, in a directory of their own.
struct Sim {
    root: tempfile::TempDir,
    agent_exe: PathBuf,
    definitions: PathBuf,
    config: PathBuf,
}

impl Sim {
    /// Builds what the agent needs and lays it out. Cargo is asked for the agent as the other tests
    /// of the workspace do. The service and the simulator are built in a target directory of their
    /// own: the nested build resolves their features without the workspace's, so in the main one
    /// it could replace the service binary that the service's own tests are starting.
    async fn new() -> Sim {
        let (agent, service, sim_vci) = tokio::task::spawn_blocking(|| {
            (
                crate::agent_register::ngr_agent_binary::ngr_agent_path(),
                build_artifact_isolated("j2534-0404-service", Artifact::Bin("j2534-0404-service")),
                build_artifact_isolated("sim-vci", Artifact::Cdylib),
            )
        })
        .await
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let agent_exe = bin.join("ngr-agent");
        if std::fs::hard_link(&agent, &agent_exe).is_err() {
            std::fs::copy(&agent, &agent_exe).unwrap();
        }
        let abi = worker_host::abi::detect_file(&sim_vci).expect("sim-vci has a known ABI");
        sim_worker::install_worker(&service, &bin.join("workers"), abi.name());
        let definitions = root.path().join("definitions");
        std::fs::create_dir_all(&definitions).unwrap();
        j2534_0404_mock::write_definition(&definitions, LIBRARY_NAME, &sim_vci).unwrap();
        std::fs::create_dir_all(root.path().join("control")).unwrap();
        let config = root.path().join("sim-vci.toml");
        std::fs::write(&config, "").unwrap();
        Sim {
            root,
            agent_exe,
            definitions,
            config,
        }
    }

    fn control_dir(&self) -> PathBuf {
        self.root.path().join("control")
    }

    fn trace(&self) -> PathBuf {
        self.root.path().join("trace.txt")
    }

    fn ecu_state(&self) -> PathBuf {
        self.root.path().join("ecu-state")
    }

    /// The requests the workers have sent so far, without the tester presents they send on their
    /// own.
    fn requests(&self) -> Vec<String> {
        std::fs::read_to_string(self.trace())
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.starts_with("uds 3E"))
            .map(str::to_owned)
            .collect()
    }

    /// The coding the ECU in the state file reads out now.
    fn coding(&self) -> Vec<u8> {
        let mut ecu = load_ecu_state(&self.ecu_state())
            .expect("the state file reads")
            .expect("the ECU has a state file");
        match ecu.request(&[0x22, 0x01, 0x20]) {
            SimResponse::Positive(bytes) => bytes,
            other => panic!("the coding read answered {other:?}"),
        }
    }
}

/// A server over a throwaway database and the real object server, with its delivery queue on, a
/// registered agent and the local user `alice`.
struct Lab {
    world: World,
    server: Option<Running>,
    hub: server::Hub,
    real: Option<server::object_server::ObjectServer>,
    _object: crate::object_server_rustfs::Real,
    store: Arc<S3Store>,
    env: AgentEnv,
    agent_id: String,
}

impl Lab {
    async fn new() -> Lab {
        let world = World::new().await;
        let hash = server::local_user::Hasher::new()
            .hash(password().to_owned())
            .await
            .unwrap();
        add_user(&world.pool, "alice", &["worker"], &hash).await;
        let mut object = crate::object_server_rustfs::Real::new();
        let real = object.start().await;
        let store = Arc::new(
            tokio::time::timeout(
                LIMIT,
                S3Store::open(
                    real.port(),
                    real.credentials(),
                    real.certificate_pin(),
                    "nekoguruma",
                    S3Tuning::default(),
                ),
            )
            .await
            .expect("open must not hang")
            .unwrap(),
        );
        let backend = Backend::new(Some(Arc::clone(&store) as Arc<dyn MultipartStore>));
        let http = Http {
            port: 0,
            shutdown_grace_secs: 2,
        };
        let server = Server::bind_with_store(&http, world.pg.pool(), world.key.clone(), backend)
            .await
            .unwrap();
        let hub = server.hub();
        let server = server.with_job_queue(Settings {
            period: Duration::from_millis(50),
            lease: LIMIT,
            passes: None,
            events: None,
        });
        let server = Running::spawn(server);
        let env = AgentEnv::new();
        let fingerprint = fingerprint(&env).await;
        let request =
            server::registration::IssueRequest::new("alice", "Bench PC", &fingerprint, None)
                .unwrap();
        let issued = server::registration::issue(&world.pool, &request)
            .await
            .unwrap();
        env.install(server.addr.port());
        let args: Vec<String> = [
            "register",
            "--mode",
            "user",
            "--server-key",
            &issued.server_key_id,
        ]
        .map(str::to_owned)
        .to_vec();
        let output = run(env.command(&args), issued.code.as_str().to_owned()).await;
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let agent_id = world.sql("SELECT id::text FROM agents").await;
        Lab {
            world,
            server: Some(server),
            hub,
            real: Some(real),
            _object: object,
            store,
            env,
            agent_id,
        }
    }

    fn addr(&self) -> std::net::SocketAddr {
        self.server.as_ref().unwrap().addr
    }

    /// Starts `ngr-agent connect` from the copy beside the workers, with the simulator's files in
    /// its environment (and so its worker's), its log going to a file for a failure's message.
    fn start_agent(&self, sim: &Sim) -> Agent {
        let log = self.env.home.path().join("connect.log");
        let stderr = std::fs::File::create(&log).unwrap();
        let args: Vec<String> = ["connect", "--mode", "user"].map(str::to_owned).to_vec();
        let child = self
            .env
            .command_with(&sim.agent_exe, &args)
            .env("VCI_CONFIG_PATH", &sim.config)
            .env("NGR_J2534_DEFINITION_DIR", &sim.definitions)
            .env_remove("VCI_SERVICE_INSECURE_NO_AUTH")
            .env(
                "NGR_SIM_ECU_CONFIG",
                fictional_ecm::vehicle().join("sim/ecm.json"),
            )
            .env("NGR_SIM_ECU_STATE", sim.ecu_state())
            .env("NGR_SIM_VCI_CONTROL_DIR", sim.control_dir())
            .env("NGR_SIM_VCI_TRACE", sim.trace())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap();
        Agent {
            child: Some(child),
            log,
        }
    }

    /// Polls `ready` every 20 ms until it holds, panicking after [`LIMIT`] or when the agent
    /// exits first, with the agent's log.
    async fn until<F, Fut>(&self, what: &str, agent: &mut Agent, mut ready: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = Instant::now() + LIMIT;
        while !ready().await {
            if let Some(status) = agent.child.as_mut().unwrap().try_wait().unwrap() {
                panic!(
                    "the agent exited ({status}) while waiting for {what}:\n{}",
                    agent.log_text()
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the agent says:\n{}",
                agent.log_text()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn is_connected(&self) -> bool {
        self.hub
            .connected()
            .iter()
            .any(|(agent, _)| agent.0 == self.agent_id)
    }

    /// Waits until the job has ended as completed and its vehicle's lock is freed, which is when
    /// the server has applied the agent's end report (ADR-480, ADR-481); a job that ended in
    /// another state fails the test at once, with what the row and the agent say.
    async fn until_completed(&self, job: &str, agent: &mut Agent) {
        // `until` holds the agent mutably, so the message reads the log by its path.
        let log = agent.log.clone();
        self.until(&format!("job {job} to complete"), agent, || async {
            let (state, code, detail, procedure): (
                String,
                Option<String>,
                Option<String>,
                Option<String>,
            ) = sqlx::query_as(
                "SELECT state, failure_code, failure_detail, procedure_code FROM jobs \
                 WHERE id = $1::uuid",
            )
            .bind(job)
            .fetch_one(&self.world.pool)
            .await
            .unwrap();
            assert!(
                !matches!(
                    state.as_str(),
                    "failed"
                        | "preconditionFailed"
                        | "needsOnSiteRecovery"
                        | "cancelled"
                        | "expired"
                ),
                "job {job} ended as {state}: failure code {code:?}, detail {detail:?}, procedure \
                 code {procedure:?}; the agent says:\n{}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            state == "completed" && self.locks(VIN).await == 0
        })
        .await;
    }

    async fn locks(&self, vin: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM vehicle_locks WHERE vin = $1")
            .bind(vin)
            .fetch_one(&self.world.pool)
            .await
            .unwrap()
    }

    /// The states of the events the agent sent for the job, in the agent's order.
    async fn event_states(&self, job: &str) -> Vec<String> {
        let bodies: Vec<String> = sqlx::query_scalar(
            "SELECT body FROM agent_events WHERE job_id = $1::uuid ORDER BY seq",
        )
        .bind(job)
        .fetch_all(&self.world.pool)
        .await
        .unwrap();
        bodies
            .iter()
            .map(|body| {
                let event: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(event["jobId"].as_str(), Some(job), "{body}");
                event["state"].as_str().unwrap().to_owned()
            })
            .collect()
    }

    /// The job's finished `result` object as the object store holds it, once the agent has
    /// uploaded it; its row's size and digest are those of the stored bytes.
    async fn result(&self, job: &str, agent: &mut Agent) -> VmState {
        self.until(
            &format!("job {job}'s result to be stored"),
            agent,
            || async {
                let state: Option<String> = sqlx::query_scalar(
                    "SELECT state FROM job_objects WHERE job_id = $1::uuid AND kind = 'result'",
                )
                .bind(job)
                .fetch_optional(&self.world.pool)
                .await
                .unwrap();
                state.as_deref() == Some("finished")
            },
        )
        .await;
        let (key, size, digest): (String, i64, String) = sqlx::query_as(
            "SELECT storage_key, size_bytes, digest FROM job_objects \
             WHERE job_id = $1::uuid AND kind = 'result'",
        )
        .bind(job)
        .fetch_one(&self.world.pool)
        .await
        .unwrap();
        let stored = read_all(&self.store, &ObjectKey::parse(&key).unwrap())
            .await
            .unwrap();
        assert_eq!((stored.len() as i64, hex(&sha256(&stored))), (size, digest));
        serde_json::from_slice(&stored).expect("the result is the job's final state")
    }

    async fn created_rows(&self, job: &str) -> String {
        self.world
            .sql(&format!(
                "SELECT count(*) FROM audit_log WHERE job_id = '{job}'::uuid \
                   AND action = 'jobCreated'"
            ))
            .await
    }

    async fn finish(mut self) {
        self.server.take().unwrap().finish().await;
        self.real.take().unwrap().stop(std::future::pending()).await;
        self.world.finish().await;
    }
}

/// The body of a job request for the vehicle of the test.
fn request(kind: &str, agent_id: &str, document: &[u8]) -> String {
    serde_json::json!({
        "kind": kind,
        "agentId": agent_id,
        "vciId": LIBRARY_NAME,
        "vin": VIN,
        "irDigests": [name_of(document)],
    })
    .to_string()
}

/// Creates a job through the route and returns its ID.
async fn create(lab: &Lab, cookie: &str, key: &str, body: &str) -> String {
    let reply = post_to(lab.addr(), cookie, key, body).await;
    assert_eq!(reply.status, 202, "{} {}", reply.head, reply.body);
    json(&reply)["jobId"].as_str().unwrap().to_owned()
}

/// An acquisition job and then a configuration write job for one VIN, each created through the Web
/// API, run on the agent and the simulators, and leave their events, their results and their one
/// audit row; the write job runs `write-coding.star` and changes the ECM's coding.
#[tokio::test(flavor = "multi_thread")]
async fn an_acquisition_job_and_a_write_job_created_through_the_web_api_run_on_the_simulators() {
    let sim = Sim::new().await;
    let lab = Lab::new().await;
    let addr = lab.addr();
    let cookie = login(addr, "alice").await;
    let acquisition = sim_worker::document(&sim_worker::program());
    let coding = fictional_ecm::coding_document();
    for bytes in [&acquisition, &coding] {
        let reply = put_document(addr, &cookie, bytes).await;
        assert_eq!(reply.status / 100, 2, "{} {}", reply.head, reply.body);
    }
    let mut agent = lab.start_agent(&sim);
    lab.until("the agent to connect", &mut agent, || async {
        lab.is_connected().await
    })
    .await;

    // The acquisition job reads the VIN and a DID the ECU does not know.
    let first = create(
        &lab,
        &cookie,
        "acquire",
        &request("acquire", &lab.agent_id, &acquisition),
    )
    .await;
    lab.until_completed(&first, &mut agent).await;
    assert_eq!(
        lab.event_states(&first).await,
        ["received", "running", "completed"]
    );
    let mut vin = vec![0x62, 0xF1, 0x90];
    vin.extend_from_slice(VIN.as_bytes());
    let acquired = lab.result(&first, &mut agent).await;
    assert_eq!(
        acquired.stack,
        [Value::Bytes(vin), Value::Bytes(vec![0x7F, 0x22, 0x31])]
    );
    assert_eq!(lab.created_rows(&first).await, "1");
    assert_eq!(
        sim.coding(),
        CODING_BEFORE,
        "the acquisition changed nothing"
    );
    let before_write = sim.requests().len();

    // The write needs the gateway's authentication, which the next worker reads from the control
    // directory at its first J2534 call and keeps in the ECU's state file.
    fictional_ecm::send(
        &sim.control_dir(),
        "001",
        r#"{"command": "set_gateway_authentication", "authenticated": true}"#,
    );
    // The first job's lock is freed, so the same VIN is accepted again.
    let second = create(
        &lab,
        &cookie,
        "write",
        &request("writeSettings", &lab.agent_id, &coding),
    )
    .await;
    assert_ne!(first, second);
    lab.until_completed(&second, &mut agent).await;
    assert_eq!(
        lab.event_states(&second).await,
        ["received", "running", "completed"]
    );
    // The result is the write procedure's own final state, not the acquisition's: it ran longer,
    // left nothing on the stack, and its first local holds the coding it saved before writing.
    let written = lab.result(&second, &mut agent).await;
    assert!(
        written.steps > acquired.steps,
        "{} steps, after the acquisition's {}",
        written.steps,
        acquired.steps
    );
    assert!(written.stack.is_empty(), "{:?}", written.stack);
    assert_eq!(
        written.locals.first(),
        Some(&Some(Value::Bytes(CODING_BEFORE.to_vec())))
    );
    assert_eq!(lab.created_rows(&second).await, "1");
    assert_eq!(lab.created_rows(&first).await, "1");

    // The worker that ran the write job, the only one that sent anything since the acquisition's
    // was reaped, read the coding, opened the extended session, took the seed, sent the key,
    // wrote the coding and read it back; the agent sent nothing else.
    assert_eq!(
        sim.requests().split_off(before_write),
        [
            "uds 22 0120",
            "uds 10 03",
            "uds 27",
            "uds 27",
            "uds 2E 0120",
            "uds 22 0120"
        ]
    );
    assert_eq!(sim.coding(), CODING_AFTER);

    let status = agent.stop().await;
    assert_eq!(status, Some(0), "{}", agent.log_text());
    lab.finish().await;
}
