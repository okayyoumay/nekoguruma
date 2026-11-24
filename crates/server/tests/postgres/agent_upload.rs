//! The agent's data channel against a real local-profile server and the bundled RustFS (design 5.2,
//! 13.4, 14.3, ADR-493, ADR-495): `ngr-agent debug enqueue` puts a result and a communication log
//! into the upload ledger, `ngr-agent connect` uploads them through the four routes, the test
//! kills the agent while the log is half stored and starts it again, and the objects the store
//! holds are byte-identical to the sources.
//!
//! The first test places its cut with the server's store, not with a sleep: the store wrapper
//! refuses the URL of every part from the third on (503, which the agent retries after its
//! back-off) until the test lets it go. The state that holds then is fixed: parts 1 and 2 of the
//! log are stored, part 3 was asked for and never given, and the agent's request for it has been
//! seen. The test kills the agent in that state, lifts the refusal and starts the agent again.
//!
//! The second test cuts inside a part's PUT. The object server sits behind a TCP relay on
//! 127.0.0.1 that copies bytes both ways untouched, so the TLS session, the certificate pin and the
//! URL's signature are the real ones; the server's store is opened on the relay's port, so every
//! presigned URL names the relay. When the store issues the URL of the log's second part the relay
//! is armed, and once it has taken more than half a part's bytes on one connection since then it
//! stops forwarding them, says so, and holds the connection. That state is fixed: part 1 is
//! stored, part 2's body is half sent and cannot end, part 3 was never asked for. The test kills the
//! agent there, drops the held connection, and starts the agent again.
//!
//! "Not sent again" is read from the URLs the server issued. The agent PUTs a part only to a URL
//! it was given for that part (design 5.2, ADR-493 item 13), so a part the server never issued a
//! second URL for was not sent a second time; the tests also check, through the store's own
//! listing, which parts existed before the kill. A part whose PUT was cut does not exist in a
//! multipart upload (the object server keeps a part only once its PUT ends), so the listing shows
//! the earlier parts and not the cut one, and the second run issues the cut part's URL again.
//!
//! A release agent has no plain-file key store and no debug command, so this module is compiled
//! in debug builds only. The test needs `NGR_TEST_RUSTFS_BIN` (`scripts/fetch-rustfs.sh`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use server::Server;
use server::config::Http;
use server::storage::{
    GetUrl, MultipartStore, ObjectBody, ObjectKey, Part, PartUrl, S3Store, S3Tuning, StoreError,
    StoreFuture, UnfinishedUploads, Upload, UploadId,
};
use server::uploads::{Backend, PART_BYTES};
use shared_proto::{AgentId, INSTRUCTION_VERSION, JobId, JobInstruction, JobKind, VciId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use crate::agent_connect::Agent;
use crate::agent_register::{AgentEnv, fingerprint, run, text};
use crate::redeem::World;
use crate::serving::Running;
use crate::storage::{content, read_all, sha256};
use crate::uploads::{hex, v4};

const LIMIT: Duration = Duration::from_secs(60);

/// A TCP relay in front of the object server that copies both ways untouched, and that, once
/// armed, stops forwarding a connection's bytes to the server after it has taken `cut_after` of
/// them since the arming (and holds the connection) until it is released.
struct Relay {
    port: u16,
    shared: Arc<RelayShared>,
    accept: tokio::task::JoinHandle<()>,
}

struct RelayShared {
    /// Counts the armings; a connection counts its bytes from the arming it last saw.
    epoch: AtomicU64,
    /// Cutting is on until [`Relay::release`].
    enabled: AtomicBool,
    cut_after: u64,
    /// Notified (a permit is kept) when a connection was cut.
    cut: Notify,
    /// Notified to end the held connection.
    release: Notify,
}

/// How a client-to-server copy ended.
enum Flow {
    Eof,
    Cut,
}

impl Relay {
    async fn start(target: u16, cut_after: u64) -> Relay {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(RelayShared {
            epoch: AtomicU64::new(0),
            enabled: AtomicBool::new(true),
            cut_after,
            cut: Notify::new(),
            release: Notify::new(),
        });
        let accept = tokio::spawn({
            let shared = Arc::clone(&shared);
            async move {
                loop {
                    let Ok((client, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(relay_connection(client, target, Arc::clone(&shared)));
                }
            }
        });
        Relay {
            port,
            shared,
            accept,
        }
    }

    /// Resolves once a connection was cut.
    async fn cut(&self) {
        tokio::time::timeout(LIMIT, self.shared.cut.notified())
            .await
            .expect("a connection is cut");
    }

    /// Ends the held connection and stops cutting.
    fn release(&self) {
        self.shared.enabled.store(false, Ordering::SeqCst);
        self.shared.release.notify_waiters();
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl RelayShared {
    fn arm(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }
}

async fn relay_connection(client: TcpStream, target: u16, shared: Arc<RelayShared>) {
    let Ok(upstream) = TcpStream::connect(("127.0.0.1", target)).await else {
        return;
    };
    let (mut client_read, mut client_write) = client.into_split();
    let (mut upstream_read, mut upstream_write) = upstream.into_split();
    let up = async {
        let mut buf = vec![0u8; 16 * 1024];
        let (mut seen, mut counted) = (0u64, 0u64);
        loop {
            let n = client_read.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                let _ = upstream_write.shutdown().await;
                return Flow::Eof;
            }
            let epoch = shared.epoch.load(Ordering::SeqCst);
            if epoch != seen {
                (seen, counted) = (epoch, 0);
            }
            if epoch > 0 && shared.enabled.load(Ordering::SeqCst) {
                counted += n as u64;
                if counted > shared.cut_after {
                    // This chunk and every later one stay here: the object server never sees
                    // the end of the body.
                    let released = shared.release.notified();
                    tokio::pin!(released);
                    released.as_mut().enable();
                    shared.cut.notify_one();
                    released.await;
                    return Flow::Cut;
                }
            }
            if upstream_write.write_all(&buf[..n]).await.is_err() {
                return Flow::Eof;
            }
        }
    };
    let down = async {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = upstream_read.read(&mut buf).await.unwrap_or(0);
            if n == 0 || client_write.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    };
    tokio::pin!(down);
    tokio::select! {
        flow = up => {
            if matches!(flow, Flow::Eof) {
                down.await;
            }
        }
        () = &mut down => {}
    }
}

/// The store the server uses: the real one, which records every part URL it issues and refuses
/// the URL of any part from `refuse_from` on (0 refuses none).
struct Gate {
    inner: Arc<S3Store>,
    refuse_from: AtomicU16,
    /// `(object key, part)` of every `presign_part`, refused or not, in order.
    asked: Mutex<Vec<(String, u16)>>,
    /// How many of them were refused.
    refused: Mutex<Vec<(String, u16)>>,
    /// Armed when the URL of the log's second part is issued (the result has one part only).
    relay: Option<Arc<RelayShared>>,
}

impl Gate {
    fn asked_for(&self, key: &str) -> Vec<u16> {
        let asked = self.asked.lock().unwrap();
        asked
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, part)| *part)
            .collect()
    }

    fn refused_for(&self, key: &str) -> usize {
        self.refused
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k == key)
            .count()
    }
}

impl MultipartStore for Gate {
    fn begin(&self) -> StoreFuture<'_, Upload> {
        self.inner.begin()
    }

    fn presign_part(&self, upload: &Upload, part: u16) -> Result<PartUrl, StoreError> {
        let entry = (upload.key.to_string(), part);
        self.asked.lock().unwrap().push(entry.clone());
        if part == 2
            && let Some(relay) = &self.relay
        {
            relay.arm();
        }
        let from = self.refuse_from.load(Ordering::SeqCst);
        if from != 0 && part >= from {
            self.refused.lock().unwrap().push(entry);
            return Err(StoreError::Unavailable);
        }
        self.inner.presign_part(upload, part)
    }

    fn put_part<'a>(
        &'a self,
        upload: &'a Upload,
        part: u16,
        bytes: Vec<u8>,
    ) -> StoreFuture<'a, ()> {
        self.inner.put_part(upload, part, bytes)
    }

    fn presign_get(&self, key: &ObjectKey) -> Result<GetUrl, StoreError> {
        self.inner.presign_get(key)
    }

    fn list_parts<'a>(&'a self, upload: &'a Upload) -> StoreFuture<'a, Vec<Part>> {
        self.inner.list_parts(upload)
    }

    fn complete<'a>(
        &'a self,
        upload: &'a Upload,
        sha256: [u8; 32],
        length: u64,
    ) -> StoreFuture<'a, ()> {
        self.inner.complete(upload, sha256, length)
    }

    fn get<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ObjectBody> {
        self.inner.get(key)
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ()> {
        self.inner.delete(key)
    }

    fn list_unfinished(&self) -> StoreFuture<'_, UnfinishedUploads> {
        self.inner.list_unfinished()
    }
}

/// A registered agent, a server over the real object server, and one job of the agent's owner.
struct Fixture {
    world: World,
    server: Running,
    real: server::object_server::ObjectServer,
    /// Owns the object server's data directory, which must outlive it.
    _object: crate::object_server_rustfs::Real,
    store: Arc<S3Store>,
    gate: Arc<Gate>,
    /// The relay the store is opened on, in the test that cuts inside a part.
    relay: Option<Relay>,
    env: AgentEnv,
    agent_id: String,
    job: String,
}

impl Fixture {
    async fn new() -> Fixture {
        Fixture::build(false).await
    }

    /// A fixture whose store reaches the object server through a [`Relay`] that cuts at half a part.
    async fn through_relay() -> Fixture {
        Fixture::build(true).await
    }

    async fn build(relayed: bool) -> Fixture {
        let world = World::new().await;
        world.add_operator("alice").await;
        let mut object = crate::object_server_rustfs::Real::new();
        let real = object.start().await;
        let relay = if relayed {
            Some(Relay::start(real.port(), PART_BYTES / 2).await)
        } else {
            None
        };
        let store = Arc::new(
            tokio::time::timeout(
                Duration::from_secs(60),
                S3Store::open(
                    relay.as_ref().map_or(real.port(), |relay| relay.port),
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
        let gate = Arc::new(Gate {
            inner: Arc::clone(&store),
            refuse_from: AtomicU16::new(0),
            asked: Mutex::default(),
            refused: Mutex::default(),
            relay: relay.as_ref().map(|relay| Arc::clone(&relay.shared)),
        });
        let backend = Backend::new(Some(Arc::clone(&gate) as Arc<dyn MultipartStore>));
        let http = Http {
            port: 0,
            shutdown_grace_secs: 2,
        };
        let server = Server::bind_with_store(&http, world.pg.pool(), world.key.clone(), backend)
            .await
            .unwrap();
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
        let mut fx = Fixture {
            world,
            server,
            real,
            _object: object,
            store,
            gate,
            relay,
            env,
            agent_id,
            job: String::new(),
        };
        fx.job = fx.seed_job().await;
        fx
    }

    async fn finish(self) {
        self.server.finish().await;
        self.real.stop(std::future::pending()).await;
        self.world.finish().await;
    }

    /// A job of the agent's owner at ownership generation 1, with its signed instruction, so that
    /// begin's judgment of the job and its generation passes.
    async fn seed_job(&self) -> String {
        let owner: String =
            sqlx::query_scalar("SELECT owner_id::text FROM agents WHERE id = $1::uuid")
                .bind(&self.agent_id)
                .fetch_one(&self.world.pool)
                .await
                .unwrap();
        let id = v4();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let instruction = JobInstruction {
            version: INSTRUCTION_VERSION,
            job_id: JobId(id.clone()),
            ownership_generation: 1,
            agent_id: AgentId(self.agent_id.clone()),
            operator_id: owner.clone(),
            kind: JobKind::RunSequence,
            vci: VciId("vci-a".to_owned()),
            vin: None,
            document_hashes: vec![],
            artifact_hash: None,
            plan_hash: None,
            issued_at: now,
            start_deadline: now + 3_600,
        };
        let envelope = self.world.key.seal(&instruction).await.unwrap();
        sqlx::query(
            "INSERT INTO jobs (tenant_id, id, kind, agent_id, operator_id, vci_id, \
                 confirmation_level, start_deadline, signed_job, signing_key_id, \
                 idempotency_key) \
             SELECT a.tenant_id, $2::uuid, 'runSequence', a.id, o.id, 'vci-a', 'notifyOnly', \
                 now() + interval '1 hour', $3, $4, $2 \
             FROM agents a JOIN operators o ON o.tenant_id = a.tenant_id \
             WHERE a.id = $1::uuid AND o.id = $5::uuid",
        )
        .bind(&self.agent_id)
        .bind(&id)
        .bind(&envelope)
        .bind(self.world.key.key_id())
        .bind(&owner)
        .execute(&self.world.pool)
        .await
        .unwrap();
        id
    }

    /// `ngr-agent debug enqueue` of `file` as the job's object of `kind`; returns the entry's
    /// number. It needs the ledger, so no `connect` of this agent may be running.
    async fn enqueue(&self, kind: &str, file: &Path) -> u64 {
        let args: Vec<String> = [
            "debug",
            "enqueue",
            "--mode",
            "user",
            "--job",
            &self.job,
            "--generation",
            "1",
            "--kind",
            kind,
            "--file",
            file.to_str().unwrap(),
        ]
        .map(str::to_owned)
        .to_vec();
        let output = run(self.env.command(&args), String::new()).await;
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let printed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        printed["seq"].as_u64().unwrap()
    }

    /// Starts `ngr-agent connect`, its operational log going to a file for a failure's message.
    fn start_agent(&self, name: &str) -> Agent {
        let log = self.env.home.path().join(name);
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

    fn ledger_file(&self) -> PathBuf {
        self.env.state.path().join("uploads").join(&self.agent_id)
    }

    fn ledger_len(&self) -> u64 {
        std::fs::metadata(self.ledger_file())
            .map(|meta| meta.len())
            .unwrap_or(0)
    }

    /// `(state, storage_key, upload_id)` of the job's row of `kind`, once it has one.
    async fn row(&self, kind: &str) -> Option<(String, String, String)> {
        sqlx::query_as(
            "SELECT state, storage_key, upload_id FROM job_objects \
             WHERE job_id = $1::uuid AND kind = $2",
        )
        .bind(&self.job)
        .bind(kind)
        .fetch_optional(&self.world.pool)
        .await
        .unwrap()
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

    async fn upload_of(&self, kind: &str) -> (Upload, ObjectKey) {
        let (_, key, id) = self.row(kind).await.expect("the row exists");
        let key = ObjectKey::parse(&key).unwrap();
        let upload = Upload {
            key: key.clone(),
            id: UploadId::parse(&id).unwrap(),
        };
        (upload, key)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_agent_resumes_a_half_stored_log_and_sends_no_stored_part_again() {
    let fx = Fixture::new().await;
    let result = content(1, 3_000);
    // Three parts: two whole ones and a rest.
    let log = content(2, 2 * PART_BYTES as usize + 123_456);
    let result_file = fx.env.home.path().join("result.json");
    let log_file = fx.env.home.path().join("comm.log");
    std::fs::write(&result_file, &result).unwrap();
    std::fs::write(&log_file, &log).unwrap();
    assert_eq!(fx.enqueue("result", &result_file).await, 1);
    assert_eq!(fx.enqueue("commLog", &log_file).await, 2);

    // The store gives no URL for part 3 until the test says so.
    fx.gate.refuse_from.store(3, Ordering::SeqCst);
    let mut agent = fx.start_agent("connect-1.log");
    // The log's row exists, and the agent has asked for the URL of its third part (which it did
    // not get): parts 1 and 2 were sent, since the agent sends one part at a time in order. The
    // result goes first, so its row is finished by then.
    fx.until(
        "the log's third part to be asked for",
        &mut agent,
        || async {
            match fx.row("commLog").await {
                Some((_, key, _)) => fx.gate.refused_for(&key) >= 1,
                None => false,
            }
        },
    )
    .await;
    let (log_upload, log_key) = fx.upload_of("commLog").await;
    let (state, ..) = fx.row("commLog").await.unwrap();
    assert_eq!(state, "open", "the log is not finished");
    let (result_state, ..) = fx.row("result").await.unwrap();
    assert_eq!(result_state, "finished", "the result goes first");
    let stored = fx.store.list_parts(&log_upload).await.unwrap();
    let held: Vec<(u16, u64)> = stored.iter().map(|p| (p.number, p.size)).collect();
    assert_eq!(
        held,
        [(1, PART_BYTES), (2, PART_BYTES)],
        "parts 1 and 2 are stored"
    );

    // The cut: SIGKILL, with nothing more sent or written after it.
    agent.kill();
    let ledger_len_at_kill = fx.ledger_len();
    let asked_before = fx.gate.asked_for(log_key.as_str());
    // The first run asked for parts 1 and 2 (it may have asked again after a failed attempt) and
    // for part 3, which it was refused.
    assert!(
        asked_before.contains(&1) && asked_before.contains(&2),
        "{asked_before:?}"
    );
    assert!(
        asked_before.iter().all(|part| *part <= 3),
        "{asked_before:?}"
    );
    assert_eq!(
        fx.store.list_parts(&log_upload).await.unwrap().len(),
        2,
        "nothing was stored after the observation"
    );

    // The second run.
    fx.gate.refuse_from.store(0, Ordering::SeqCst);
    let mut agent = fx.start_agent("connect-2.log");
    fx.until("both rows to be finished", &mut agent, || async {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT state FROM job_objects WHERE job_id = $1::uuid ORDER BY kind",
        )
        .bind(&fx.job)
        .fetch_all(&fx.world.pool)
        .await
        .unwrap();
        rows == ["finished", "finished"]
    })
    .await;
    // The row is finished before the agent has read the answer and removed the entry: the ledger
    // file is rewritten without the entry, which makes it shorter than it was at the kill (one
    // entry and the header).
    fx.until("the ledger to be emptied", &mut agent, || async {
        fx.ledger_len() < ledger_len_at_kill
    })
    .await;
    let status = agent.stop().await;
    if cfg!(unix) {
        assert_eq!(status, Some(0), "{}", agent.log_text());
    }

    // Not sent again: the requests made after the restart are for the third part only, so no URL
    // was asked for a part the first run had stored.
    let asked = fx.gate.asked_for(log_key.as_str());
    assert_eq!(asked[..asked_before.len()], asked_before[..]);
    let after = &asked[asked_before.len()..];
    assert!(
        !after.is_empty() && after.iter().all(|part| *part == 3),
        "the second run asked for {after:?}"
    );
    // One row for each object, none failed, so the second run continued the first one's upload.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM job_objects")
        .fetch_one(&fx.world.pool)
        .await
        .unwrap();
    assert_eq!(rows, 2);

    // Both objects are complete and byte-identical in the store.
    for (kind, bytes) in [("result", &result), ("commLog", &log)] {
        let (_, key) = fx.upload_of(kind).await;
        let stored = read_all(&fx.store, &key).await.unwrap();
        assert_eq!(stored.len(), bytes.len(), "{kind}");
        assert_eq!(hex(&sha256(&stored)), hex(&sha256(bytes)), "{kind}");
        assert!(stored == *bytes, "{kind} differs");
        let (size, digest): (i64, String) = sqlx::query_as(
            "SELECT size_bytes, digest FROM job_objects WHERE job_id = $1::uuid AND kind = $2",
        )
        .bind(&fx.job)
        .bind(kind)
        .fetch_one(&fx.world.pool)
        .await
        .unwrap();
        assert_eq!((size as usize, digest), (bytes.len(), hex(&sha256(bytes))));
    }

    // The ledger is empty: the same objects enqueue as new entries, numbered after the two the
    // ledger gave before (a number is never reused, and an entry still pending would have been
    // answered with its own number). The result's stored file is gone as well.
    let objects = fx
        .env
        .state
        .path()
        .join("uploads")
        .join(format!("{}.objects", fx.agent_id));
    let left: Vec<_> = std::fs::read_dir(&objects)
        .map(|entries| entries.map(|e| e.unwrap().file_name()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "stored files left: {left:?}");
    assert_eq!(fx.enqueue("result", &result_file).await, 3);
    assert_eq!(fx.enqueue("commLog", &log_file).await, 4);
    fx.finish().await;
}

/// The kill falls inside a part's PUT: the log's second part is half sent when the relay holds its
/// connection. After the restart that part is sent whole, the stored part before it is not sent
/// again, and the object is byte-identical to the source.
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_agent_resumes_a_part_cut_inside_its_put_and_sends_the_stored_part_no_more() {
    let fx = Fixture::through_relay().await;
    let relay = fx.relay.as_ref().expect("the fixture has a relay");
    let log = content(3, 2 * PART_BYTES as usize + 123_456);
    let log_file = fx.env.home.path().join("comm.log");
    std::fs::write(&log_file, &log).unwrap();
    assert_eq!(fx.enqueue("commLog", &log_file).await, 1);

    let mut agent = fx.start_agent("connect-1.log");
    // The relay holds a connection that took half of part 2's body. The agent sends one part at
    // a time in order, so part 1 was stored before part 2's URL was issued.
    relay.cut().await;
    let (log_upload, log_key) = fx.upload_of("commLog").await;
    let (state, ..) = fx.row("commLog").await.unwrap();
    assert_eq!(state, "open", "the log is not finished");
    let held: Vec<(u16, u64)> = fx
        .store
        .list_parts(&log_upload)
        .await
        .unwrap()
        .iter()
        .map(|p| (p.number, p.size))
        .collect();
    assert_eq!(
        held,
        [(1, PART_BYTES)],
        "part 1 is stored and the part cut inside its PUT is not"
    );
    assert_eq!(
        fx.gate.asked_for(log_key.as_str()),
        [1, 2],
        "the first run asked for parts 1 and 2 only"
    );

    // The cut: SIGKILL while the body is half sent, then the held connection goes.
    agent.kill();
    relay.release();
    let ledger_len_at_kill = fx.ledger_len();
    assert_eq!(fx.store.list_parts(&log_upload).await.unwrap().len(), 1);

    // The second run.
    let mut agent = fx.start_agent("connect-2.log");
    fx.until("the log to be finished", &mut agent, || async {
        matches!(fx.row("commLog").await, Some((state, ..)) if state == "finished")
    })
    .await;
    fx.until("the ledger to be emptied", &mut agent, || async {
        fx.ledger_len() < ledger_len_at_kill
    })
    .await;
    let status = agent.stop().await;
    if cfg!(unix) {
        assert_eq!(status, Some(0), "{}", agent.log_text());
    }

    // The cut part's URL was issued again and is followed by the last part's; part 1's was not.
    let asked = fx.gate.asked_for(log_key.as_str());
    assert_eq!(asked, [1, 2, 2, 3], "the URLs the server issued");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM job_objects")
        .fetch_one(&fx.world.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "the second run continued the first one's upload");
    let stored = read_all(&fx.store, &log_key).await.unwrap();
    assert_eq!(stored.len(), log.len());
    assert_eq!(hex(&sha256(&stored)), hex(&sha256(&log)));
    assert!(stored == log, "the object differs from the source");
    fx.finish().await;
}
