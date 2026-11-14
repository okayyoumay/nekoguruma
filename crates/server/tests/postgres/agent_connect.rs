//! `ngr-agent connect` against a real local-profile server (design 5.3, 6.4, 13.3, ADR-466,
//! ADR-468, ADR-471): the agent binary registers and connects as a process with the plain-file key
//! store and its own state directory, and the server runs in the test's process over a throwaway
//! PostgreSQL. The test is the server's side of the control channel: it sends signed job commands
//! through the hub and reads the agent's answers from the hub's inbox.
//!
//! The agent has no job runner behind the session yet, so a verified command is acknowledged and
//! dropped (`LogIntake`); the answers are the observable behaviour.
//!
//! A release agent has no plain-file store, so this module is compiled in debug builds only. The
//! stop signal is SIGINT on Unix; elsewhere the child is killed and its exit status is not
//! asserted.

use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use server::config::Http;
use server::hub::Inbound;
use server::job_queue::Settings;
use server::{Hub, Server};
use shared_proto::{
    AgentId, AgentMessage, INSTRUCTION_VERSION, InstructionRejectReason, JobId, JobInstruction,
    JobKind, VciId, Vin,
};
use tokio::sync::mpsc::Receiver;

use crate::agent_register::{AgentEnv, fingerprint, run, text};
use crate::jobs::{add_user, login, password, post_to};
use crate::redeem::World;
use crate::serving::Running;

const LIMIT: Duration = Duration::from_secs(30);
const JOB_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f";

/// A world with a registered agent, a server whose hub the test holds and the agent's surroundings.
struct Fixture {
    world: World,
    server: Option<Running>,
    hub: Hub,
    /// The test's side of the inbox; `None` when the delivery queue holds it.
    inbox: Option<Receiver<Inbound>>,
    env: AgentEnv,
    agent_id: String,
}

impl Fixture {
    async fn new() -> Fixture {
        Fixture::with_queue(false).await
    }

    /// With `queue` the server's delivery queue is on and the test takes no inbox.
    async fn with_queue(queue: bool) -> Fixture {
        Fixture::build(queue, false).await
    }

    /// With the delivery queue on, and `alice` a local user with the role worker, who can sign in
    /// and create jobs.
    async fn with_local_user() -> Fixture {
        Fixture::build(true, true).await
    }

    async fn build(queue: bool, local_user: bool) -> Fixture {
        let world = World::new().await;
        if local_user {
            let hash = server::local_user::Hasher::new()
                .hash(password().to_owned())
                .await
                .unwrap();
            add_user(&world.pool, "alice", &["worker"], &hash).await;
        } else {
            world.add_operator("alice").await;
        }
        let http = Http {
            port: 0,
            shutdown_grace_secs: 2,
        };
        let server = Server::bind(&http, world.pg.pool(), world.key.clone())
            .await
            .unwrap();
        let hub = server.hub();
        let (server, inbox) = if queue {
            let settings = Settings {
                period: Duration::from_millis(50),
                lease: LIMIT,
                passes: None,
            };
            (server.with_job_queue(settings), None)
        } else {
            (server, Some(hub.inbox().expect("the inbox is taken once")))
        };
        let server = Running::spawn(server);
        let env = AgentEnv::new();
        let fingerprint = fingerprint(&env).await;
        let request =
            server::registration::IssueRequest::new("alice", "Bench PC", &fingerprint, None)
                .unwrap();
        let issued = server::registration::issue(&world.pool, &request)
            .await
            .unwrap();
        let args: Vec<String> = [
            "register",
            "--mode",
            "user",
            "--server",
            &format!("http://127.0.0.1:{}", server.addr.port()),
            "--server-key",
            &issued.server_key_id,
        ]
        .map(str::to_owned)
        .to_vec();
        let output = run(env.command(&args), issued.code.as_str().to_owned()).await;
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let agent_id = world.sql("SELECT id::text FROM agents").await;
        Fixture {
            world,
            server: Some(server),
            hub,
            inbox,
            env,
            agent_id,
        }
    }

    /// Starts `ngr-agent connect`, its operational log going to a file for a failure's message.
    fn start_agent(&self) -> Agent {
        let log = self.env.home.path().join("connect.log");
        let stderr = std::fs::File::create(&log).unwrap();
        let args: Vec<String> = ["connect", "--mode", "user"].map(str::to_owned).to_vec();
        let child = self
            .env
            .command(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        Agent {
            child: Some(child),
            log,
        }
    }

    fn is_connected(&self) -> bool {
        self.hub
            .connected()
            .iter()
            .any(|(agent, _)| agent.0 == self.agent_id)
    }

    async fn wait_connected(&self, mut agent: Option<&mut Agent>, connected: bool) {
        let deadline = Instant::now() + LIMIT;
        while self.is_connected() != connected {
            let log = agent.as_ref().map(|agent| agent.log_text());
            assert!(
                Instant::now() < deadline,
                "connected should become {connected}; the agent says:\n{log:?}"
            );
            if let Some(agent) = agent.as_deref_mut()
                && let Some(status) = agent.child.as_mut().unwrap().try_wait().unwrap()
            {
                panic!("the agent exited ({status}):\n{}", agent.log_text());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A job command for this agent, signed by the server's instruction key; `forge` flips a byte
    /// of the envelope after signing.
    async fn signed_job(&self, forge: bool) -> String {
        STANDARD.encode(self.envelope(JOB_ID, forge).await)
    }

    /// The envelope of the instruction of job `job_id`; `forge` flips its last byte.
    async fn envelope(&self, job_id: &str, forge: bool) -> Vec<u8> {
        let instruction = JobInstruction {
            version: INSTRUCTION_VERSION,
            job_id: JobId(job_id.to_owned()),
            ownership_generation: 1,
            agent_id: AgentId(self.agent_id.clone()),
            operator_id: "operator-1".to_owned(),
            kind: JobKind::RunSequence,
            vci: VciId("vci-a".to_owned()),
            vin: Some(Vin("VIN-UNDER-TEST".to_owned())),
            document_hashes: vec![],
            artifact_hash: None,
            plan_hash: None,
            issued_at: 1_000,
            start_deadline: 2_000,
        };
        let mut envelope = self.world.key.seal(&instruction).await.unwrap();
        if forge {
            *envelope.last_mut().unwrap() ^= 1;
        }
        envelope
    }

    /// A job row for this agent holding `envelope`, signed under the server's active key.
    async fn add_job(&self, id: &str, order: i64, envelope: &[u8]) {
        sqlx::query(
            "INSERT INTO jobs (tenant_id, id, kind, agent_id, operator_id, vci_id, \
                 confirmation_level, start_deadline, signed_job, signing_key_id, created_at, \
                 idempotency_key) \
             SELECT a.tenant_id, $2::uuid, 'runSequence', a.id, o.id, 'vci-a', 'notifyOnly', \
                 now() + interval '1 hour', $4, $5, now() - interval '1 hour' + $3 * interval '1 second', \
                 $2 \
             FROM agents a JOIN operators o ON o.tenant_id = a.tenant_id WHERE a.id = $1::uuid",
        )
        .bind(&self.agent_id)
        .bind(id)
        .bind(order)
        .bind(envelope)
        .bind(self.world.key.key_id())
        .execute(&self.world.pool)
        .await
        .unwrap();
    }

    /// Waits until the job's row reads `expected`: state, failure code, detail, delivered.
    async fn until_job(&self, id: &str, expected: &str) {
        let deadline = Instant::now() + LIMIT;
        loop {
            let now: String = sqlx::query_scalar(
                "SELECT concat_ws('|', state, coalesce(failure_code, ''), \
                     coalesce(failure_detail, ''), (delivered_at IS NOT NULL)::text) \
                 FROM jobs WHERE id = $1::uuid",
            )
            .bind(id)
            .fetch_one(&self.world.pool)
            .await
            .unwrap();
            if now == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the job reads {now}, not {expected}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn next(&mut self) -> Inbound {
        tokio::time::timeout(
            LIMIT,
            self.inbox
                .as_mut()
                .expect("the test holds the inbox")
                .recv(),
        )
        .await
        .expect("the agent answers")
        .expect("the inbox stays open")
    }
}

/// The `connect` process; it is killed when dropped, so a failed test leaves none behind.
struct Agent {
    child: Option<Child>,
    log: std::path::PathBuf,
}

impl Agent {
    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Asks the agent to stop (SIGINT on Unix, a kill elsewhere) and waits for it to exit. The exit
    /// status is `None` where the stop is a kill.
    async fn stop(&mut self) -> Option<i32> {
        let child = self.child.as_mut().unwrap();
        #[cfg(unix)]
        {
            // SAFETY: `kill` takes a process ID and a signal number and touches no memory.
            let result = unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
            assert_eq!(result, 0);
        }
        #[cfg(not(unix))]
        child.kill().unwrap();
        let deadline = Instant::now() + LIMIT;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "the agent did not stop:\n{}",
                self.log_text()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        self.child = None;
        if cfg!(unix) {
            Some(status.code().expect("exited, not killed"))
        } else {
            None
        }
    }

    fn running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| child.try_wait().unwrap().is_none())
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The agent registers, connects, acknowledges a signed command and rejects a forged one by its
/// own gate, and a stop signal ends it with exit 0 and a connection the hub sees leave.
#[tokio::test(flavor = "multi_thread")]
async fn a_registered_agent_connects_answers_commands_and_stops_on_a_signal() {
    let mut f = Fixture::new().await;
    let mut agent = f.start_agent();
    f.wait_connected(Some(&mut agent), true).await;

    let good = f.signed_job(false).await;
    let sent = f
        .hub
        .send_job_command(&AgentId(f.agent_id.clone()), good)
        .unwrap();
    let answer = f.next().await;
    assert_eq!(answer.agent.0, f.agent_id);
    assert_eq!(answer.connection, sent.connection);
    match answer.message {
        AgentMessage::CommandAck { command_id } => assert_eq!(command_id, sent.command_id),
        other => panic!("commandAck expected, got {other:?}"),
    }

    let forged = f.signed_job(true).await;
    let sent = f
        .hub
        .send_job_command(&AgentId(f.agent_id.clone()), forged)
        .unwrap();
    match f.next().await.message {
        AgentMessage::CommandRejected { command_id, reason } => {
            assert_eq!(command_id, sent.command_id);
            assert_eq!(reason, InstructionRejectReason::BadSignature);
        }
        other => panic!("commandRejected expected, got {other:?}"),
    }
    // The agent is still connected after a refusal.
    assert!(f.is_connected());

    let status = agent.stop().await;
    if cfg!(unix) {
        assert_eq!(status, Some(0), "{}", agent.log_text());
    }
    f.wait_connected(None, false).await;
    f.server.take().unwrap().finish().await;
    f.world.finish().await;
}

/// A stop of the server closes the connection; the agent stays up, retries, and stops on the
/// signal.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_of_the_server_closes_the_connection_and_the_agent_keeps_running() {
    let mut f = Fixture::new().await;
    let mut agent = f.start_agent();
    f.wait_connected(Some(&mut agent), true).await;

    f.server.take().unwrap().finish().await;
    assert!(!f.is_connected(), "the server's stop ended the connection");
    // The session logs its first failed attempt and retries; it does not exit. The server's port
    // is free after the stop, so a parallel test may bind it and answer the agent with a refusal
    // (a warning of its own): either line, written after the stop, shows the retry.
    let stopped_at = agent.log_text().len();
    let retried = |log: &str| {
        log.get(stopped_at..).is_some_and(|after| {
            after.contains("control channel attempt failed")
                || after.contains("control channel refused")
        })
    };
    let deadline = Instant::now() + LIMIT;
    while !retried(&agent.log_text()) {
        assert!(Instant::now() < deadline, "no retry:\n{}", agent.log_text());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(agent.running(), "the agent exited:\n{}", agent.log_text());
    assert!(!f.is_connected());

    let status = agent.stop().await;
    if cfg!(unix) {
        assert_eq!(status, Some(0), "{}", agent.log_text());
    }
    f.world.finish().await;
}

/// A stored envelope with one flipped byte is delivered by the queue; the agent's own gate refuses
/// it with the bad-signature reason, which ends the job as failed with the server's code for a
/// rejection, and the agent's next job is delivered and acknowledged (design 5.3, ADR-476).
#[tokio::test(flavor = "multi_thread")]
async fn a_forged_stored_command_is_rejected_by_the_agent_and_ends_the_job_and_the_next_is_delivered()
 {
    const NEXT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d50";
    let mut f = Fixture::with_queue(true).await;
    let forged = f.envelope(JOB_ID, true).await;
    f.add_job(JOB_ID, 1, &forged).await;
    let good = f.envelope(NEXT_ID, false).await;
    f.add_job(NEXT_ID, 2, &good).await;
    let mut agent = f.start_agent();
    f.wait_connected(Some(&mut agent), true).await;
    f.until_job(NEXT_ID, "received|||true").await;
    f.until_job(JOB_ID, "failed|instructionRejected|badSignature|false")
        .await;
    agent.stop().await;
    f.server.take().unwrap().finish().await;
    f.world.finish().await;
}

/// A job an operator creates through `POST /api/v1/jobs` reaches the real agent through the
/// queue: the agent's own gate accepts the stored envelope and acknowledges it, and the delivery
/// is recorded (design 5.3, 12; ADR-476, ADR-477).
#[tokio::test(flavor = "multi_thread")]
async fn a_job_created_through_the_route_is_accepted_and_acknowledged_by_the_agent() {
    let mut f = Fixture::with_local_user().await;
    let mut agent = f.start_agent();
    f.wait_connected(Some(&mut agent), true).await;
    let addr = f.server.as_ref().unwrap().addr;
    let cookie = login(addr, "alice").await;
    let body = serde_json::json!({
        "kind": "acquire",
        "agentId": f.agent_id,
        "vciId": "vci-a",
        "irDigests": ["aa".repeat(32)],
    })
    .to_string();
    let reply = post_to(addr, &cookie, "e2", &body).await;
    assert_eq!(reply.status, 202, "{} {}", reply.head, reply.body);
    let id = crate::redeem::json(&reply)["jobId"]
        .as_str()
        .unwrap()
        .to_owned();
    f.until_job(&id, "received|||true").await;
    assert!(f.is_connected(), "{}", agent.log_text());
    agent.stop().await;
    f.server.take().unwrap().finish().await;
    f.world.finish().await;
}
