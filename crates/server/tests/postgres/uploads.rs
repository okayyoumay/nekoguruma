//! The data channel's four routes (ADR-493 items 1 to 8, 15 to 23): begin, the part URL, the
//! status and the completion, with the `job_objects` rows they write. PostgreSQL throughout; the object
//! store is the real pinned RustFS where the store's answers matter (`NGR_TEST_RUSTFS_BIN`, as
//! `storage.rs` has it) and a wrapper of the `MultipartStore` that scripts failures, counts calls
//! and holds a call at a barrier where they do not. Tokens and keys are made at run time; no
//! assertion message prints one.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use server::Server;
use server::config::Http;
use server::object_server::ObjectServer;
use server::storage::{
    GetUrl, MultipartStore, ObjectBody, ObjectKey, Part, PartUrl, S3Store, S3Tuning, StoreError,
    StoreFuture, UnfinishedUploads, Upload, UploadId,
};
use server::uploads::{BEGIN_PATH, Backend, MAX_SIZE, PART_BYTES, Pause};
use sha2::{Digest, Sha256};
use shared_proto::{AgentId, INSTRUCTION_VERSION, JobId, JobInstruction, JobKind, VciId};
use tokio::io::AsyncReadExt;
use tokio::sync::{Barrier, Notify, Semaphore};

use crate::control::{nonce_line, token};
use crate::redeem::{Agent, bytes_of, capture, json};
use crate::serving::{Reply, Running, exchange};
use crate::session::{Up, audience_of};
use crate::storage::{MIB, content, put, sha256};
use crate::support::{Answered, assert_unproved};

const SMALL: u64 = 512 * 1024;

fn digest() -> String {
    "ab".repeat(32)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random version 4 UUID in canonical text.
pub(crate) fn v4() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).unwrap();
    b[6] = b[6] & 0x0f | 0x40;
    b[8] = b[8] & 0x3f | 0x80;
    let h = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

// ---------------------------------------------------------------------------------------------
// The store wrapper

#[derive(Clone, Copy)]
pub(crate) enum Fail {
    Io,
    Random,
    NotFound,
    AlreadyFinished,
    // The store's answers to `complete`.
    NoParts,
    MissingPart,
    PartTooSmall,
    PartsLengthMismatch,
    PartsChanged,
    ObjectTooLarge,
    HashMismatch,
    LengthMismatch,
    Refused,
}

/// A call held inside the fake until the test lets it go: `entered` says it arrived.
struct Hold {
    entered: Notify,
    gate: Semaphore,
}

impl Hold {
    fn new() -> Arc<Hold> {
        Arc::new(Hold {
            entered: Notify::new(),
            gate: Semaphore::new(0),
        })
    }

    async fn wait(&self) {
        self.entered.notify_one();
        self.gate.acquire().await.unwrap().forget();
    }

    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(20), self.entered.notified())
            .await
            .expect("the call reaches the store");
    }

    fn release(&self) {
        self.gate.add_permits(1000);
    }
}

impl Fail {
    fn error(self) -> StoreError {
        match self {
            Fail::Io => StoreError::Io {
                op: "test",
                kind: io::ErrorKind::Other,
            },
            Fail::Random => StoreError::Random,
            Fail::NotFound => StoreError::NotFound,
            Fail::AlreadyFinished => StoreError::AlreadyFinished,
            Fail::NoParts => StoreError::NoParts,
            Fail::MissingPart => StoreError::MissingPart { part: 2 },
            Fail::PartTooSmall => StoreError::PartTooSmall { part: 1 },
            Fail::PartsLengthMismatch => StoreError::PartsLengthMismatch { stored: 1 },
            Fail::PartsChanged => StoreError::PartsChanged,
            Fail::ObjectTooLarge => StoreError::ObjectTooLarge,
            Fail::HashMismatch => StoreError::HashMismatch,
            Fail::LengthMismatch => StoreError::LengthMismatch { stored: 1 },
            Fail::Refused => StoreError::Refused { op: "test" },
        }
    }
}

/// A store that forwards to the real one when it has one, and otherwise issues uploads of its own
/// that hold no parts (and no URL: `presign_part` of such an upload is refused).
#[derive(Default)]
pub(crate) struct Wrap {
    inner: Option<Arc<S3Store>>,
    begins: AtomicUsize,
    pub(crate) presigns: AtomicUsize,
    lists: AtomicUsize,
    deletes: AtomicUsize,
    /// The calls of `complete`, apart from `calls()` so that its shape stays.
    completes: AtomicUsize,
    begin_fail: Mutex<Option<Fail>>,
    /// The calls of `put_part`.
    puts: AtomicUsize,
    put_fail: Mutex<Option<Fail>>,
    /// The key the next fake `begin` (one with no real store behind it) issues, once.
    pub(crate) begin_key: Mutex<Option<ObjectKey>>,
    list_fail: Mutex<Option<Fail>>,
    delete_fail: Mutex<Option<Fail>>,
    pub(crate) presign_fail: Mutex<Option<Fail>>,
    complete_fail: Mutex<Option<Fail>>,
    /// Makes `complete` panic, as a fault in the task would.
    complete_panics: std::sync::atomic::AtomicBool,
    /// Holds every `complete` before it answers.
    complete_hold: Mutex<Option<Arc<Hold>>>,
    /// Holds every `list_parts` before it lists.
    list_hold: Mutex<Option<Arc<Hold>>>,
    /// Awaited by every `begin` after the upload exists and before it returns.
    begin_barrier: Mutex<Option<Arc<Barrier>>>,
    issued: Mutex<Vec<String>>,
    deleted: Mutex<Vec<String>>,
}

impl Wrap {
    /// The calls of `put_part`.
    pub(crate) fn puts(&self) -> usize {
        self.puts.load(Ordering::SeqCst)
    }

    /// The calls of `complete`.
    pub(crate) fn completes(&self) -> usize {
        self.completes.load(Ordering::SeqCst)
    }

    /// The keys `begin` issued, in order.
    pub(crate) fn issued(&self) -> Vec<String> {
        self.issued.lock().unwrap().clone()
    }

    /// The keys `delete` was called with, in order.
    pub(crate) fn deleted(&self) -> Vec<String> {
        self.deleted.lock().unwrap().clone()
    }

    /// Makes the next `begin`s wait for `parties` of them, after their upload exists.
    pub(crate) fn hold_begins(&self, parties: usize) -> Arc<Barrier> {
        let barrier = Arc::new(Barrier::new(parties));
        *self.begin_barrier.lock().unwrap() = Some(Arc::clone(&barrier));
        barrier
    }

    pub(crate) fn fail_begin(&self, fail: Option<Fail>) {
        Wrap::set(&self.begin_fail, fail);
    }

    pub(crate) fn fail_put(&self, fail: Option<Fail>) {
        Wrap::set(&self.put_fail, fail);
    }

    pub(crate) fn fail_complete(&self, fail: Option<Fail>) {
        Wrap::set(&self.complete_fail, fail);
    }

    pub(crate) fn fail_delete(&self, fail: Option<Fail>) {
        Wrap::set(&self.delete_fail, fail);
    }

    pub(crate) fn calls(&self) -> [usize; 4] {
        [
            self.begins.load(Ordering::SeqCst),
            self.presigns.load(Ordering::SeqCst),
            self.lists.load(Ordering::SeqCst),
            self.deletes.load(Ordering::SeqCst),
        ]
    }

    pub(crate) fn set(slot: &Mutex<Option<Fail>>, fail: Option<Fail>) {
        *slot.lock().unwrap() = fail;
    }

    fn fail(slot: &Mutex<Option<Fail>>) -> Option<StoreError> {
        slot.lock().unwrap().map(Fail::error)
    }
}

impl MultipartStore for Wrap {
    fn begin(&self) -> StoreFuture<'_, Upload> {
        Box::pin(async move {
            self.begins.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = Wrap::fail(&self.begin_fail) {
                return Err(error);
            }
            let upload = match &self.inner {
                Some(inner) => inner.begin().await?,
                None => Upload {
                    key: self
                        .begin_key
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap_or_else(|| ObjectKey::parse(&v4()).unwrap()),
                    id: UploadId::parse(&format!("fake-{}", v4())).unwrap(),
                },
            };
            self.issued.lock().unwrap().push(upload.key.to_string());
            let barrier = self.begin_barrier.lock().unwrap().clone();
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
            Ok(upload)
        })
    }

    fn presign_part(&self, upload: &Upload, part: u16) -> Result<PartUrl, StoreError> {
        self.presigns.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = Wrap::fail(&self.presign_fail) {
            return Err(error);
        }
        match &self.inner {
            Some(inner) => inner.presign_part(upload, part),
            None => Err(StoreError::InvalidKey),
        }
    }

    fn put_part<'a>(
        &'a self,
        upload: &'a Upload,
        part: u16,
        bytes: Vec<u8>,
    ) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.puts.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = Wrap::fail(&self.put_fail) {
                return Err(error);
            }
            match &self.inner {
                Some(inner) => inner.put_part(upload, part, bytes).await,
                None => Ok(()),
            }
        })
    }

    fn presign_get(&self, key: &ObjectKey) -> Result<GetUrl, StoreError> {
        self.presigns.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = Wrap::fail(&self.presign_fail) {
            return Err(error);
        }
        match &self.inner {
            Some(inner) => inner.presign_get(key),
            None => Err(StoreError::InvalidKey),
        }
    }

    fn list_parts<'a>(&'a self, upload: &'a Upload) -> StoreFuture<'a, Vec<Part>> {
        Box::pin(async move {
            self.lists.fetch_add(1, Ordering::SeqCst);
            let hold = self.list_hold.lock().unwrap().clone();
            if let Some(hold) = hold {
                hold.wait().await;
            }
            if let Some(error) = Wrap::fail(&self.list_fail) {
                return Err(error);
            }
            match &self.inner {
                Some(inner) => inner.list_parts(upload).await,
                None => Ok(Vec::new()),
            }
        })
    }

    fn complete<'a>(
        &'a self,
        upload: &'a Upload,
        sha256: [u8; 32],
        length: u64,
    ) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.completes.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.complete_panics.load(Ordering::SeqCst),
                "the fake's completion panics"
            );
            let hold = self.complete_hold.lock().unwrap().clone();
            if let Some(hold) = hold {
                hold.wait().await;
            }
            if let Some(error) = Wrap::fail(&self.complete_fail) {
                return Err(error);
            }
            match &self.inner {
                Some(inner) => inner.complete(upload, sha256, length).await,
                None => Ok(()),
            }
        })
    }

    fn get<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ObjectBody> {
        match &self.inner {
            Some(inner) => inner.get(key),
            None => Box::pin(async { Err(StoreError::Unavailable) }),
        }
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.deletes.fetch_add(1, Ordering::SeqCst);
            self.deleted.lock().unwrap().push(key.to_string());
            if let Some(error) = Wrap::fail(&self.delete_fail) {
                return Err(error);
            }
            match &self.inner {
                Some(inner) => inner.delete(key).await,
                None => Ok(()),
            }
        })
    }

    fn list_unfinished(&self) -> StoreFuture<'_, UnfinishedUploads> {
        match &self.inner {
            Some(inner) => inner.list_unfinished(),
            None => Box::pin(async {
                Ok(UnfinishedUploads {
                    uploads: Vec::new(),
                    skipped: 0,
                })
            }),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The fixture

pub(crate) struct Fx {
    pub(crate) u: Up,
    pub(crate) wrap: Arc<Wrap>,
    pub(crate) real: Option<(crate::object_server_rustfs::Real, ObjectServer)>,
    /// The agent's session token.
    pub(crate) token: String,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum With {
    /// No store at all.
    Nothing,
    /// The wrapper with no real store behind it.
    Fake,
    /// The wrapper over the real object server.
    Real,
}

impl Fx {
    pub(crate) async fn new(with: With) -> Fx {
        Fx::sized(with, None).await
    }

    async fn sized(with: With, part_bytes: Option<u64>) -> Fx {
        Fx::build(with, part_bytes, None, None).await
    }

    /// A fixture whose completion requests wait `wait` for the task before they answer 202.
    async fn waiting(with: With, wait: Duration) -> Fx {
        Fx::build(with, None, Some(wait), None).await
    }

    /// A fixture whose next completion request is held by `pause` before it starts a task.
    async fn paused(with: With, pause: Arc<Pause>) -> Fx {
        Fx::build(with, None, None, Some(pause)).await
    }

    async fn build(
        with: With,
        part_bytes: Option<u64>,
        wait: Option<Duration>,
        pause: Option<Arc<Pause>>,
    ) -> Fx {
        Fx::tuned(with, |mut backend| {
            if let Some(bytes) = part_bytes {
                backend = backend.with_part_bytes(bytes);
            }
            if let Some(wait) = wait {
                backend = backend.with_complete_wait(wait);
            }
            if let Some(pause) = pause {
                backend = backend.with_pause(pause);
            }
            backend
        })
        .await
    }

    /// A fixture whose backend `tune` has changed.
    pub(crate) async fn tuned(with: With, tune: impl FnOnce(Backend) -> Backend) -> Fx {
        Fx::assemble(with, tune, false).await.0
    }

    /// A fixture whose instruction key stops signing while the returned flag is set.
    pub(crate) async fn flaky(with: With) -> (Fx, Arc<std::sync::atomic::AtomicBool>) {
        let (fx, fail) = Fx::assemble(with, |backend| backend, true).await;
        (fx, fail.expect("a flaky fixture has its flag"))
    }

    async fn assemble(
        with: With,
        tune: impl FnOnce(Backend) -> Backend,
        flaky: bool,
    ) -> (Fx, Option<Arc<std::sync::atomic::AtomicBool>>) {
        let mut real = None;
        let mut wrap = Wrap::default();
        if with == With::Real {
            let mut r = crate::object_server_rustfs::Real::new();
            let server = r.start().await;
            let store = tokio::time::timeout(
                Duration::from_secs(60),
                S3Store::open(
                    server.port(),
                    server.credentials(),
                    "nekoguruma",
                    S3Tuning::default(),
                ),
            )
            .await
            .expect("open must not hang")
            .unwrap();
            wrap.inner = Some(Arc::new(store));
            real = Some((r, server));
        }
        let wrap = Arc::new(wrap);
        let store = (with != With::Nothing).then(|| Arc::clone(&wrap) as Arc<dyn MultipartStore>);
        let backend = tune(Backend::new(store));
        let (u, fail) = if flaky {
            let (u, fail) = Up::flaky_with(backend).await;
            (u, Some(fail))
        } else {
            (Up::with_backend(2, None, None, backend).await, None)
        };
        let token = token(&u).await;
        (
            Fx {
                u,
                wrap,
                real,
                token,
            },
            fail,
        )
    }

    pub(crate) async fn finish(self) {
        self.u.finish().await;
        if let Some((_real, server)) = self.real {
            server.stop(std::future::pending()).await;
        }
    }

    fn addr(&self) -> std::net::SocketAddr {
        self.u.s.addr
    }

    pub(crate) async fn send(
        &self,
        method: &str,
        target: &str,
        token: Option<&str>,
        content_type: Option<&str>,
        body: &str,
    ) -> Reply {
        exchange(
            self.addr(),
            &self.request_text(method, target, token, content_type, body),
        )
        .await
    }

    fn request_text(
        &self,
        method: &str,
        target: &str,
        token: Option<&str>,
        content_type: Option<&str>,
        body: &str,
    ) -> String {
        self.request_text_with(method, target, token, content_type, &[], body)
    }

    /// Like `request_text`, with the header lines `extra` after the `Host` line.
    fn request_text_with(
        &self,
        method: &str,
        target: &str,
        token: Option<&str>,
        content_type: Option<&str>,
        extra: &[String],
        body: &str,
    ) -> String {
        let mut raw = format!(
            "{method} {target} HTTP/1.1\r\n{}\r\nConnection: close\r\n",
            self.u.s.host()
        );
        for line in extra {
            raw.push_str(&format!("{line}\r\n"));
        }
        if let Some(token) = token {
            raw.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if let Some(content_type) = content_type {
            raw.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        raw.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        raw
    }

    /// Like [`Fx::send`], with the header lines `extra`.
    pub(crate) async fn send_with(
        &self,
        method: &str,
        target: &str,
        token: Option<&str>,
        content_type: Option<&str>,
        extra: &[String],
        body: &str,
    ) -> Reply {
        exchange(
            self.addr(),
            &self.request_text_with(method, target, token, content_type, extra, body),
        )
        .await
    }

    async fn begin_as(&self, token: Option<&str>, body: &str) -> Reply {
        self.send(
            "POST",
            "/api/v1/agent/uploads",
            token,
            Some("application/json"),
            body,
        )
        .await
    }

    pub(crate) async fn begin(&self, body: &str) -> Reply {
        self.begin_as(Some(&self.token), body).await
    }

    async fn part_as(&self, token: Option<&str>, id: &str, part: &str) -> Reply {
        self.send(
            "POST",
            &format!("/api/v1/agent/uploads/{id}/parts/{part}"),
            token,
            None,
            "",
        )
        .await
    }

    pub(crate) async fn part(&self, id: &str, part: &str) -> Reply {
        self.part_as(Some(&self.token), id, part).await
    }

    async fn status_as(&self, token: Option<&str>, id: &str) -> Reply {
        self.send(
            "GET",
            &format!("/api/v1/agent/uploads/{id}"),
            token,
            None,
            "",
        )
        .await
    }

    async fn status(&self, id: &str) -> Reply {
        self.status_as(Some(&self.token), id).await
    }

    async fn complete_as(&self, token: Option<&str>, id: &str) -> Reply {
        self.send(
            "POST",
            &format!("/api/v1/agent/uploads/{id}/complete"),
            token,
            None,
            "",
        )
        .await
    }

    pub(crate) async fn complete(&self, id: &str) -> Reply {
        self.complete_as(Some(&self.token), id).await
    }

    async fn state(&self, id: &str) -> String {
        self.sql(&format!("SELECT state FROM job_objects WHERE id = '{id}'"))
            .await
    }

    /// The object the row names, as the store holds it.
    async fn stored(&self, id: &str) -> Result<Vec<u8>, StoreError> {
        let key = self
            .sql(&format!(
                "SELECT storage_key FROM job_objects WHERE id = '{id}'"
            ))
            .await;
        let body = self.wrap.get(&ObjectKey::parse(&key).unwrap()).await?;
        let mut out = Vec::new();
        let mut reader = body.reader;
        reader.read_to_end(&mut out).await.unwrap();
        Ok(out)
    }

    /// Waits until the row is in `state`.
    async fn until(&self, id: &str, state: &str) {
        for _ in 0..500 {
            if self.state(id).await == state {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the row did not become {state}");
    }

    /// A job of agent `agent` with the ownership generation `generation` in its signed
    /// instruction.
    async fn job_for(&self, agent: &str, generation: u64) -> String {
        self.job_naming(agent, generation, Vec::new(), None, None)
            .await
    }

    /// A job whose signed instruction names the given documents (ascending), artifact and plan.
    pub(crate) async fn job_naming(
        &self,
        agent: &str,
        generation: u64,
        document_hashes: Vec<[u8; 32]>,
        artifact_hash: Option<[u8; 32]>,
        plan_hash: Option<[u8; 32]>,
    ) -> String {
        let id: String = sqlx::query_scalar("SELECT gen_random_uuid()::text")
            .fetch_one(&self.u.w.pool)
            .await
            .unwrap();
        let instruction = JobInstruction {
            version: INSTRUCTION_VERSION,
            job_id: JobId(id.clone()),
            ownership_generation: generation,
            agent_id: AgentId(agent.to_owned()),
            operator_id: "operator".to_owned(),
            kind: JobKind::Acquire,
            vci: VciId("vci".to_owned()),
            vin: None,
            document_hashes,
            artifact_hash,
            plan_hash,
            issued_at: 1,
            start_deadline: 2,
        };
        let sealed = self.u.w.key.seal(&instruction).await.unwrap();
        self.insert_job(&id, agent, &sealed).await;
        id
    }

    pub(crate) async fn insert_job(&self, id: &str, agent: &str, signed: &[u8]) {
        sqlx::query(
            "INSERT INTO jobs (tenant_id, id, kind, agent_id, operator_id, vci_id, \
                 confirmation_level, start_deadline, signed_job, signing_key_id, idempotency_key) \
             SELECT a.tenant_id, $1::uuid, 'acquire', a.id, o.id, 'vci', 'notifyOnly', \
                 now() + interval '1 hour', $3, $4, gen_random_uuid()::text \
             FROM agents a JOIN LATERAL (SELECT id FROM operators WHERE tenant_id = a.tenant_id \
                 ORDER BY created_at, id LIMIT 1) o ON true WHERE a.id = $2::uuid",
        )
        .bind(id)
        .bind(agent)
        .bind(signed)
        .bind(self.u.w.key.key_id())
        .execute(&self.u.w.pool)
        .await
        .unwrap();
    }

    pub(crate) async fn job(&self) -> String {
        self.job_for(&self.u.id, 1).await
    }

    /// A row written by hand, for the cases the routes do not reach by themselves.
    async fn row(
        &self,
        job: &str,
        generation: i64,
        kind: &str,
        size: i64,
        state: &str,
        part_bytes: i64,
    ) -> String {
        sqlx::query_scalar(
            "INSERT INTO job_objects (tenant_id, id, job_id, generation, kind, agent_id, \
                 storage_key, upload_id, part_bytes, size_bytes, digest, state, finished_at) \
             SELECT j.tenant_id, gen_random_uuid(), j.id, $2, $3, j.agent_id, \
                 gen_random_uuid()::text, 'fake-upload-id', $6, $4, $7, $5, \
                 CASE WHEN $5 = 'finished' THEN now() END \
             FROM jobs j WHERE j.id = $1::uuid RETURNING id::text",
        )
        .bind(job)
        .bind(generation)
        .bind(kind)
        .bind(size)
        .bind(state)
        .bind(part_bytes)
        .bind(digest())
        .fetch_one(&self.u.w.pool)
        .await
        .unwrap()
    }

    pub(crate) async fn sql(&self, sql: &str) -> String {
        self.u.w.sql(sql).await
    }

    /// The token of another agent of the same tenant, with its ID.
    pub(crate) async fn other_agent(&self) -> (String, String) {
        let agent = Agent::new();
        let id = self.u.w.register(&agent).await;
        let r = self.u.exchange_as(&agent.key, &id).await;
        assert_eq!(r.status, 200, "{}", r.head);
        (id, json(&r)["token"].as_str().unwrap().to_owned())
    }
}

pub(crate) fn decl(job: &str, generation: u64, kind: &str, size: u64, sha: &str) -> String {
    format!(
        r#"{{"jobId":"{job}","generation":{generation},"kind":"{kind}","sizeBytes":{size},"sha256":"{sha}"}}"#
    )
}

fn result_of(job: &str, size: u64) -> String {
    decl(job, 1, "result", size, &digest())
}

/// The count of leading parts the status reports.
async fn held(fx: &Fx, id: &str) -> u64 {
    let r = fx.status(id).await;
    assert_eq!(r.status, 200, "{}", r.head);
    json(&r)["partsHeld"].as_u64().unwrap()
}

/// Asks for the URL of part `number` and PUTs `len` bytes to it.
async fn send(fx: &Fx, id: &str, number: u16, len: usize) {
    let r = fx.part(id, &number.to_string()).await;
    assert_eq!(r.status, 200, "part {number}: {}", r.head);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let v = json(&r);
    assert_eq!(v["expiresInSecs"], 300);
    let url = v["url"].as_str().unwrap().to_owned();
    assert!(url.starts_with("http://127.0.0.1:"), "the URL's host");
    let (status, etag) = put(&url, content(number as u8, len)).await;
    assert_eq!(status, 200, "PUT part {number}");
    assert!(etag.is_some());
}

/// The SHA-256 of `bytes` as the begin body writes it.
pub(crate) fn sha_of(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// A row begun on a new job over the route, with the declared length and digest of `bytes`.
pub(crate) async fn begin_for(fx: &Fx, bytes: &[u8]) -> String {
    let job = fx.job().await;
    let r = fx
        .begin(&decl(&job, 1, "result", bytes.len() as u64, &sha_of(bytes)))
        .await;
    assert_eq!(r.status, 201, "{}", r.head);
    upload_id(&r)
}

pub(crate) fn problem(r: &Reply, status: u16, what: &str) {
    assert_eq!(r.status, status, "{what}: {}", r.head);
    assert_eq!(
        r.header("content-type"),
        Some("application/problem+json"),
        "{what}"
    );
}

fn upload_id(r: &Reply) -> String {
    json(r)["uploadId"].as_str().unwrap().to_owned()
}

// ---------------------------------------------------------------------------------------------
// Order, shape and the token

/// The shape is judged before the token (ADR-493 item 16): a request with no token and a wrong
/// body is a 400, not a 401, and a valid body with no token is a 401.
#[tokio::test]
async fn the_shape_is_judged_before_the_token() {
    let fx = Fx::new(With::Nothing).await;
    let job = v4();
    let good = result_of(&job, 10);
    assert_eq!(fx.begin_as(None, &good).await.status, 401);
    let upper = job.to_uppercase();
    let bad_shapes = [
        "".to_owned(),
        "[]".to_owned(),
        "null".to_owned(),
        "{}".to_owned(),
        format!("{good}x"),
        decl(&upper, 1, "result", 1, &digest()),
        decl("not-a-uuid", 1, "result", 1, &digest()),
        decl(&job, 1, "Result", 1, &digest()),
        decl(&job, 1, "other", 1, &digest()),
        decl(&job, 1, "result", 1, &digest().to_uppercase()),
        decl(&job, 1, "result", 1, &"ab".repeat(31)),
        decl(&job, 1, "result", 1, &"ab".repeat(33)),
        decl(&job, 1, "result", 1, &"zz".repeat(32)),
        // Numbers: negative, fractional, exponent form, strings, null.
        good.replace("\"sizeBytes\":10", "\"sizeBytes\":-1"),
        good.replace("\"sizeBytes\":10", "\"sizeBytes\":1.5"),
        good.replace("\"sizeBytes\":10", "\"sizeBytes\":1e3"),
        good.replace("\"sizeBytes\":10", "\"sizeBytes\":\"10\""),
        good.replace("\"sizeBytes\":10", "\"sizeBytes\":null"),
        good.replace("\"generation\":1", "\"generation\":-1"),
        good.replace("\"generation\":1", "\"generation\":1.0"),
        good.replace("\"generation\":1", "\"generation\":9223372036854775808"),
        good.replace("\"generation\":1", "\"generation\":\"1\""),
        // A property twice, an unknown one, one missing.
        good.replace("}", ",\"kind\":\"result\"}"),
        good.replace("}", ",\"extra\":1}"),
        good.replace(",\"kind\":\"result\"", ""),
        good.replace("\"jobId\":", "\"jobid\":"),
    ];
    for body in &bad_shapes {
        let r = fx.begin_as(None, body).await;
        problem(&r, 400, body);
        let with_token = fx.begin(body).await;
        assert_eq!(with_token.status, 400, "{body}");
    }
    // A content type that is not JSON, or none, is a 400 too; JSON with a parameter passes.
    for content_type in [Some("text/plain"), None, Some("application/jsonx")] {
        let r = fx
            .send("POST", "/api/v1/agent/uploads", None, content_type, &good)
            .await;
        assert_eq!(r.status, 400, "{content_type:?}");
    }
    let r = fx
        .send(
            "POST",
            "/api/v1/agent/uploads",
            None,
            Some("application/json; charset=utf-8"),
            &good,
        )
        .await;
    assert_eq!(r.status, 401);
    fx.finish().await;
}

/// A length over 4 GiB is a 413 before the token and before any lookup, after the shape
/// (ADR-493 item 3): a bad kind with a huge length is a 400.
#[tokio::test]
async fn a_length_over_4_gib_is_413_after_the_shape_and_before_the_token() {
    let fx = Fx::new(With::Nothing).await;
    let job = v4();
    for size in [
        "4294967297",
        "18446744073709551615",
        "18446744073709551616",
        "99999999999999999999999999",
        "1e30",
    ] {
        let body = result_of(&job, 0).replace("\"sizeBytes\":0", &format!("\"sizeBytes\":{size}"));
        let r = fx.begin_as(None, &body).await;
        problem(&r, 413, size);
        let r = fx.begin(&body).await;
        assert_eq!(r.status, 413, "{size}");
        // The shape first.
        let bad = body.replace("\"result\"", "\"nothing\"");
        assert_eq!(fx.begin_as(None, &bad).await.status, 400, "{size}");
    }
    // Exactly 4 GiB is not over: it reaches the token.
    let r = fx.begin_as(None, &result_of(&job, 4_294_967_296)).await;
    assert_eq!(r.status, 401);
    assert_eq!(fx.sql("SELECT count(*) FROM job_objects").await, "0");
    fx.finish().await;
}

#[tokio::test]
async fn a_body_of_4096_bytes_is_read_and_one_more_is_413_on_every_route() {
    let fx = Fx::new(With::Nothing).await;
    // 4096 bytes of a body that is not the shape: read whole, then judged.
    let r = fx.begin(&"x".repeat(4096)).await;
    assert_eq!(r.status, 400, "{}", r.head);
    let r = fx.begin(&"x".repeat(4097)).await;
    problem(&r, 413, "begin");
    let id = v4();
    let r = fx
        .send(
            "POST",
            &format!("/api/v1/agent/uploads/{id}/parts/1"),
            Some(&fx.token),
            None,
            &"x".repeat(4097),
        )
        .await;
    problem(&r, 413, "part");
    let r = fx
        .send(
            "GET",
            &format!("/api/v1/agent/uploads/{id}"),
            Some(&fx.token),
            None,
            &"x".repeat(4097),
        )
        .await;
    problem(&r, 413, "status");
    // The part and status routes take no body at all.
    let r = fx
        .send(
            "POST",
            &format!("/api/v1/agent/uploads/{id}/parts/1"),
            Some(&fx.token),
            None,
            "x",
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.head);
    fx.finish().await;
}

#[tokio::test]
async fn a_body_that_does_not_arrive_within_five_seconds_is_408() {
    let fx = Fx::new(With::Nothing).await;
    for (method, target) in [
        ("POST", "/api/v1/agent/uploads".to_owned()),
        ("POST", format!("/api/v1/agent/uploads/{}/parts/1", v4())),
        ("GET", format!("/api/v1/agent/uploads/{}", v4())),
        ("POST", format!("/api/v1/agent/uploads/{}/complete", v4())),
    ] {
        let raw = format!(
            "{method} {target} HTTP/1.1\r\n{}\r\nContent-Type: application/json\r\n\
             Content-Length: 10\r\n\r\n",
            fx.u.s.host()
        );
        let started = std::time::Instant::now();
        let r = exchange(fx.addr(), &raw).await;
        problem(&r, 408, &target);
        assert!(started.elapsed() >= Duration::from_secs(5));
    }
    fx.finish().await;
}

/// Every refusal of the token is the same 401, byte for byte, on the three routes.
#[tokio::test]
async fn no_live_token_is_one_401_on_every_route() {
    let fx = Fx::new(With::Nothing).await;
    let job = fx.job().await;
    let id = v4();
    let live = token(&fx.u).await;
    // A token that decodes to 32 bytes but was never issued.
    let never = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode([7u8; 32])
    };
    let mut seen = Vec::new();
    for token in [None, Some("x"), Some(never.as_str()), Some("")] {
        for r in [
            fx.begin_as(token, &result_of(&job, 1)).await,
            fx.part_as(token, &id, "1").await,
            fx.status_as(token, &id).await,
        ] {
            problem(&r, 401, "no token");
            assert_eq!(r.header("www-authenticate"), Some("Bearer"));
            seen.push(bytes_of(&r));
        }
    }
    assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
    // A token of a live session works: the route is reached (404 for an upload nobody has).
    assert_eq!(fx.status_as(Some(&live), &id).await.status, 404);
    fx.finish().await;
}

/// A revoked or missing agent row is the same 401, and it drops the agent's tokens at once
/// (ADR-493 item 2); a request that is malformed drops none.
#[tokio::test]
async fn a_revoked_agent_is_401_and_loses_its_tokens() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx.row(&job, 1, "result", 10, "open", 5 * 1024 * 1024).await;
    let revoke = format!(
        "UPDATE agents SET revoked_at = now() WHERE id = '{}'",
        fx.u.id
    );
    let restore = format!(
        "UPDATE agents SET revoked_at = NULL WHERE id = '{}'",
        fx.u.id
    );
    let t1 = token(&fx.u).await;
    let t2 = token(&fx.u).await;
    let t3 = token(&fx.u).await;
    let t4 = token(&fx.u).await;
    sqlx::query(sqlx::AssertSqlSafe(revoke.clone()))
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    // A malformed request is answered before the row is read: it drops nothing. With the row
    // live again, t1 still reaches the routes.
    assert_eq!(fx.begin_as(Some(&t1), "{}").await.status, 400);
    assert_eq!(fx.part_as(Some(&t1), &id, "0").await.status, 400);
    assert_eq!(fx.part_as(Some(&t1), &id, "1%FF").await.status, 400);
    let target = format!("/api/v1/agent/uploads/{id}/complete");
    let body = fx.send("POST", &target, Some(&t1), None, "x").await;
    assert_eq!(body.status, 400);
    sqlx::query(sqlx::AssertSqlSafe(restore.clone()))
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    assert_eq!(fx.status_as(Some(&t1), &id).await.status, 200);
    assert_eq!(fx.part_as(Some(&t1), &id, "9").await.status, 400);
    let unknown_token = fx.status_as(Some("x"), &id).await;
    sqlx::query(sqlx::AssertSqlSafe(revoke.clone()))
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    // Each route sees the revoked row.
    problem(
        &fx.begin_as(Some(&t1), &result_of(&job, 10)).await,
        401,
        "begin",
    );
    problem(&fx.part_as(Some(&t2), &id, "1").await, 401, "part");
    let revoked = fx.status_as(Some(&t3), &id).await;
    problem(&revoked, 401, "status");
    problem(&fx.complete_as(Some(&t4), &id).await, 401, "complete");
    // The same bytes as a token that is unknown.
    assert_eq!(bytes_of(&revoked), bytes_of(&unknown_token));
    // The row is live again, and every token of the agent is gone, also the ones not used.
    sqlx::query(sqlx::AssertSqlSafe(restore))
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    for (name, t) in [
        ("t1", &t1),
        ("t2", &t2),
        ("t3", &t3),
        ("t4", &t4),
        ("main", &fx.token),
    ] {
        problem(&fx.status_as(Some(t), &id).await, 401, name);
    }
    // A new exchange works again.
    let fresh = token(&fx.u).await;
    assert_eq!(fx.status_as(Some(&fresh), &id).await.status, 200);
    // An agent with no row at all.
    for sql in [
        "DELETE FROM job_objects".to_owned(),
        "DELETE FROM jobs".to_owned(),
        "DELETE FROM agent_allowed_operators".to_owned(),
        "DELETE FROM agent_registration_codes".to_owned(),
        format!("DELETE FROM agents WHERE id = '{}'", fx.u.id),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&fx.u.w.pool)
            .await
            .unwrap();
    }
    problem(&fx.status_as(Some(&fresh), &id).await, 401, "no row");
    fx.finish().await;
}

/// The tenant comes from the agent's row: another tenant's agent cannot name this one's job or
/// upload, and the answer for it is the unknown job's.
#[tokio::test]
async fn another_agents_job_or_upload_is_404_like_an_unknown_one() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let (other_id, other) = fx.other_agent().await;
    // A second tenant's agent.
    fx.u.w.add_operator("bob").await;
    let outsider = Agent::new();
    let (_, r) = fx.u.w.redeem("bob", &outsider).await;
    assert_eq!(r.status, 201, "{}", r.head);
    let outsider_id = json(&r)["agentId"].as_str().unwrap().to_owned();
    let r = fx.u.exchange_as(&outsider.key, &outsider_id).await;
    let outsider_token = json(&r)["token"].as_str().unwrap().to_owned();
    let unknown = v4();
    for token in [&other, &outsider_token] {
        let begin = fx.begin_as(Some(token), &result_of(&job, 10)).await;
        problem(&begin, 404, "begin");
        let unknown_job = fx.begin_as(Some(token), &result_of(&unknown, 10)).await;
        assert_eq!(bytes_of(&begin), bytes_of(&unknown_job));
        let part = fx.part_as(Some(token), &id, "1").await;
        problem(&part, 404, "part");
        assert_eq!(
            bytes_of(&part),
            bytes_of(&fx.part_as(Some(token), &unknown, "1").await)
        );
        let complete = fx.complete_as(Some(token), &id).await;
        problem(&complete, 404, "complete");
        assert_eq!(
            bytes_of(&complete),
            bytes_of(&fx.complete_as(Some(token), &unknown).await)
        );
        let status = fx.status_as(Some(token), &id).await;
        problem(&status, 404, "status");
        assert_eq!(
            bytes_of(&status),
            bytes_of(&fx.status_as(Some(token), &unknown).await)
        );
    }
    // The owner sees its own, and the other agent's own job is its own.
    assert_eq!(fx.status(&id).await.status, 200);
    let mine = fx.job_for(&other_id, 1).await;
    assert_eq!(fx.begin(&result_of(&mine, 10)).await.status, 404);
    assert_eq!(
        fx.begin_as(Some(&other), &result_of(&mine, 10))
            .await
            .status,
        201
    );
    assert_eq!(fx.wrap.calls()[0], 1);
    fx.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Begin

#[tokio::test]
async fn begin_makes_a_row_and_answers_the_same_row_again() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let body = result_of(&job, 5 * MIB as u64 + 1);
    let first = fx.begin(&body).await;
    assert_eq!(first.status, 201, "{}", first.head);
    assert_eq!(first.header("content-type"), Some("application/json"));
    assert_eq!(first.header("cache-control"), Some("no-store"));
    let v = json(&first);
    let id = upload_id(&first);
    assert_eq!(v["partBytes"], PART_BYTES);
    assert_eq!(v["partCount"], 2);
    assert_eq!(v["state"], "open");
    assert_eq!(v.as_object().unwrap().len(), 4);
    // The row holds what the store issued, and the answer none of it.
    let issued = fx.wrap.issued.lock().unwrap().clone();
    assert_eq!(issued.len(), 1);
    assert_eq!(
        fx.sql(&format!(
            "SELECT storage_key || '|' || generation || '|' || kind || '|' || agent_id || '|' || \
                part_bytes || '|' || size_bytes || '|' || digest || '|' || state || '|' || \
                (finished_at IS NULL) FROM job_objects WHERE id = '{id}'"
        ))
        .await,
        format!(
            "{}|1|result|{}|{}|{}|{}|open|true",
            issued[0],
            fx.u.id,
            PART_BYTES,
            5 * MIB + 1,
            digest()
        )
    );
    assert!(!first.body.contains(&issued[0]) && !first.body.contains("fake-"));

    // The same declaration answers the same row, and the store is not asked again.
    let again = fx.begin(&body).await;
    assert_eq!(again.status, 200, "{}", again.head);
    assert_eq!(json(&again), v);
    assert_eq!(fx.wrap.calls(), [1, 0, 0, 0]);
    // Another length or digest is a conflict, and nothing changes.
    let other_size = fx.begin(&result_of(&job, 5 * MIB as u64)).await;
    problem(&other_size, 409, "length");
    let other_digest = fx
        .begin(&decl(
            &job,
            1,
            "result",
            5 * MIB as u64 + 1,
            &"cd".repeat(32),
        ))
        .await;
    problem(&other_digest, 409, "digest");
    assert_eq!(fx.wrap.calls()[0], 1);
    // The other kind is its own upload.
    let log = fx.begin(&decl(&job, 1, "commLog", 3, &digest())).await;
    assert_eq!(log.status, 201, "{}", log.head);
    assert_ne!(upload_id(&log), id);
    assert_eq!(fx.sql("SELECT count(*) FROM job_objects").await, "2");
    fx.finish().await;
}

#[tokio::test]
async fn a_failed_row_is_ignored_and_a_new_begin_makes_a_new_one() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let failed = fx
        .row(&job, 1, "result", 10, "failed", PART_BYTES as i64)
        .await;
    // Another declaration than the failed row's, and the same one, are both a new upload.
    for size in [11, 10] {
        let r = fx.begin(&result_of(&job, size)).await;
        assert_eq!(r.status, 201, "{}", r.head);
        let id = upload_id(&r);
        assert_ne!(id, failed);
        // It is the live one now.
        assert_eq!(fx.begin(&result_of(&job, size)).await.status, 200);
        sqlx::query("UPDATE job_objects SET state = 'failed' WHERE id = $1::uuid")
            .bind(&id)
            .execute(&fx.u.w.pool)
            .await
            .unwrap();
    }
    assert_eq!(fx.sql("SELECT count(*) FROM job_objects").await, "3");
    fx.finish().await;
}

/// The job: any state, the generation of the stored instruction (ADR-493 item 15), a decodable
/// instruction, and a row of another agent.
#[tokio::test]
async fn begin_judges_the_job_and_its_generation() {
    let fx = Fx::new(With::Fake).await;
    // Unknown.
    problem(&fx.begin(&result_of(&v4(), 10)).await, 404, "unknown job");
    // The generation must be the instruction's.
    let job = fx.job_for(&fx.u.id, 3).await;
    for generation in [0, 1, 2, 4] {
        let r = fx
            .begin(&decl(&job, generation, "result", 10, &digest()))
            .await;
        problem(&r, 404, &format!("generation {generation}"));
    }
    assert_eq!(fx.sql("SELECT count(*) FROM job_objects").await, "0");
    assert_eq!(fx.wrap.calls()[0], 0);
    let r = fx.begin(&decl(&job, 3, "result", 10, &digest())).await;
    assert_eq!(r.status, 201, "{}", r.head);
    // Any job state: a finished job still takes an upload.
    sqlx::query("UPDATE jobs SET state = 'succeeded', finished_at = now() WHERE id = $1::uuid")
        .bind(&job)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    let r = fx.begin(&decl(&job, 3, "commLog", 10, &digest())).await;
    assert_eq!(r.status, 201, "{}", r.head);
    // An instruction that does not decode is the server's fault.
    let broken = v4();
    fx.insert_job(&broken, &fx.u.id, b"not an envelope").await;
    problem(&fx.begin(&result_of(&broken, 10)).await, 500, "undecodable");
    assert_eq!(fx.sql("SELECT count(*) FROM job_objects").await, "2");
    // A live row of another agent for the same job, generation and kind is a conflict.
    let (other_id, _) = fx.other_agent().await;
    let shared = fx.job().await;
    fx.row(&shared, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    sqlx::query("UPDATE job_objects SET agent_id = $1::uuid WHERE job_id = $2::uuid")
        .bind(&other_id)
        .bind(&shared)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    problem(&fx.begin(&result_of(&shared, 10)).await, 409, "foreign row");
    fx.finish().await;
}

#[tokio::test]
async fn the_declared_length_decides_the_part_count() {
    let fx = Fx::new(With::Fake).await;
    let cases: [(u64, u64); 6] = [
        (0, 1),
        (1, 1),
        (PART_BYTES, 1),
        (PART_BYTES + 1, 2),
        (4 * 1024 * MIB as u64, 820),
        (4 * 1024 * MIB as u64 - 1, 820),
    ];
    for (size, count) in cases {
        let job = fx.job().await;
        let r = fx.begin(&result_of(&job, size)).await;
        assert_eq!(r.status, 201, "{size}: {}", r.head);
        assert_eq!(json(&r)["partCount"], count, "{size}");
        // The row keeps the part size, whatever the setting is later.
        assert_eq!(
            fx.sql(&format!(
                "SELECT part_bytes FROM job_objects WHERE id = '{}'",
                upload_id(&r)
            ))
            .await,
            PART_BYTES.to_string()
        );
    }
    fx.finish().await;
}

/// Without a store begin answers an existing row and refuses a new one (503); a store that fails
/// is a 503, one without randomness a 500, and no row is written.
#[tokio::test]
async fn begin_answers_what_the_store_does_and_writes_nothing_on_failure() {
    let fx = Fx::new(With::Nothing).await;
    let job = fx.job().await;
    problem(&fx.begin(&result_of(&job, 10)).await, 503, "no store");
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let r = fx.begin(&result_of(&job, 10)).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(upload_id(&r), id);
    fx.finish().await;

    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    Wrap::set(&fx.wrap.begin_fail, Some(Fail::Io));
    problem(&fx.begin(&result_of(&job, 10)).await, 503, "store fails");
    Wrap::set(&fx.wrap.begin_fail, Some(Fail::Random));
    problem(&fx.begin(&result_of(&job, 10)).await, 500, "random");
    assert_eq!(fx.sql("SELECT count(*) FROM job_objects").await, "0");
    Wrap::set(&fx.wrap.begin_fail, None);
    assert_eq!(fx.begin(&result_of(&job, 10)).await.status, 201);
    fx.finish().await;
}

/// A row that cannot be written leaves no upload behind: the key is deleted (ADR-493 item 8).
#[tokio::test]
async fn an_insert_that_fails_deletes_the_upload_it_began() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    fx.u.w
        .pg
        .execute("ALTER TABLE job_objects ADD CONSTRAINT refuse CHECK (false) NOT VALID")
        .await;
    let r = fx.begin(&result_of(&job, 10)).await;
    problem(&r, 503, "insert fails");
    assert!(!r.body.to_lowercase().contains("constraint"), "{}", r.body);
    assert_eq!(fx.wrap.calls(), [1, 0, 0, 1]);
    assert_eq!(
        *fx.wrap.deleted.lock().unwrap(),
        *fx.wrap.issued.lock().unwrap()
    );
    fx.finish().await;
}

/// Two begins of one declaration meet at the store, and both insert: one row stays, the loser
/// answers the winner's row, and the upload it began is deleted from the object server (C1 to C3).
#[tokio::test]
async fn two_racing_begins_leave_one_row_and_the_losers_upload_is_aborted() {
    let fx = Fx::new(With::Real).await;
    let job = fx.job().await;
    *fx.wrap.begin_barrier.lock().unwrap() = Some(Arc::new(Barrier::new(2)));
    let body = result_of(&job, 10);
    let (a, b) = tokio::join!(fx.begin(&body), fx.begin(&body));
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [200, 201], "{} / {}", a.head, b.head);
    assert_eq!(upload_id(&a), upload_id(&b));
    assert_eq!(fx.sql("SELECT count(*) FROM job_objects").await, "1");
    let key = fx.sql("SELECT storage_key FROM job_objects").await;
    let issued = fx.wrap.issued.lock().unwrap().clone();
    assert_eq!(issued.len(), 2);
    let deleted = fx.wrap.deleted.lock().unwrap().clone();
    assert_eq!(deleted.len(), 1);
    assert_ne!(deleted[0], key);
    assert!(issued.contains(&deleted[0]) && issued.contains(&key));
    // The object server holds the winner's upload only.
    let listed = fx.wrap.list_unfinished().await.unwrap();
    assert_eq!(
        listed
            .uploads
            .iter()
            .map(|u| u.key.to_string())
            .collect::<Vec<_>>(),
        [key]
    );
    fx.finish().await;
}

/// The loser of a race for another declaration is a conflict; a failed abort is a warning and not
/// another answer (C3, C5).
#[tokio::test]
async fn a_racing_begin_with_another_declaration_is_409_and_a_failed_abort_only_warns() {
    let (log, _guard) = capture();
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    Wrap::set(&fx.wrap.delete_fail, Some(Fail::Io));
    *fx.wrap.begin_barrier.lock().unwrap() = Some(Arc::new(Barrier::new(2)));
    let (small, big) = (result_of(&job, 10), result_of(&job, 11));
    let (a, b) = tokio::join!(fx.begin(&small), fx.begin(&big));
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [201, 409], "{} / {}", a.head, b.head);
    assert_eq!(fx.sql("SELECT count(*) FROM job_objects").await, "1");
    assert_eq!(fx.wrap.calls()[3], 1);
    let text = log.text();
    assert!(text.contains("could not be aborted"), "{text}");
    let key = fx.sql("SELECT storage_key FROM job_objects").await;
    for issued in fx.wrap.issued.lock().unwrap().iter() {
        assert!(!text.contains(issued.as_str()), "a key is in the log");
    }
    assert!(!text.contains(&key));
    fx.finish().await;
}

/// No request holds a pooled connection while it waits for the store: more requests than the
/// pool has connections are all inside `begin` at once.
#[tokio::test]
async fn no_pooled_connection_is_held_across_a_store_call() {
    let fx = Fx::new(With::Fake).await;
    // The test pool's default is ten connections.
    const N: usize = 12;
    *fx.wrap.begin_barrier.lock().unwrap() = Some(Arc::new(Barrier::new(N)));
    let mut bodies = Vec::new();
    for _ in 0..N {
        bodies.push(result_of(&fx.job().await, 10));
    }
    let addr = fx.addr();
    let tasks: Vec<_> = bodies
        .iter()
        .map(|body| {
            let raw = fx.request_text(
                "POST",
                "/api/v1/agent/uploads",
                Some(&fx.token),
                Some("application/json"),
                body,
            );
            tokio::spawn(async move { exchange(addr, &raw).await.status })
        })
        .collect();
    for task in tasks {
        let status = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("every request reaches the store at once")
            .unwrap();
        assert_eq!(status, 201);
    }
    fx.finish().await;
}

#[tokio::test]
async fn the_database_being_down_is_503() {
    let mut fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    fx.u.w.pg.stop();
    problem(&fx.begin(&result_of(&job, 10)).await, 503, "begin");
    problem(&fx.part(&id, "1").await, 503, "part");
    problem(&fx.status(&id).await, 503, "status");
    problem(&fx.complete(&id).await, 503, "complete");
    // A request that asks for a proof is refused the same way, unproved (ADR-504).
    for call in calls(&result_of(&job, 10), &id) {
        let (_, r) = fx.nonced(&call, &[]).await;
        problem(&r, 503, call.name);
        assert_unproved(&r, call.name);
    }
    fx.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The part URL and the status

#[tokio::test]
async fn the_part_number_is_judged_before_the_token_and_the_upload_after_it() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(
            &job,
            1,
            "result",
            3 * 1024 * 1024 * 1024,
            "open",
            PART_BYTES as i64,
        )
        .await;
    // 614.4 parts: 615.
    for part in [
        "0", "00", "01", "+1", "-1", "1.0", "1e0", " 1", "1 ", "0x1", "10001", "99999", "abc", "٣",
    ] {
        let target = part.replace(' ', "%20").replace('٣', "%D9%A3");
        for token in [None, Some(fx.token.as_str())] {
            let r = fx.part_as(token, &id, &target).await;
            problem(&r, 400, part);
        }
    }
    // A good number with no token is a 401, whatever the upload.
    problem(&fx.part_as(None, "garbage", "1").await, 401, "no token");
    // After the token the upload: not a UUID, unknown and another's are a 404.
    for upload in [
        "garbage",
        "%FF",
        "0192F3A4-5B6C-7D8E-8F90-123456789ABC",
        &v4(),
    ] {
        problem(&fx.part_as(Some(&fx.token), upload, "1").await, 404, upload);
    }
    problem(&fx.status_as(Some(&fx.token), "%FF").await, 404, "status");
    // A part past the row's count is a 400, the last one is allowed (or refused by the store).
    let before = fx.wrap.calls()[1];
    assert_eq!(fx.part(&id, "615").await.status, 503);
    // The last part reached the store (which the fake refuses).
    assert_eq!(fx.wrap.calls()[1], before + 1);
    assert_eq!(fx.part(&id, "616").await.status, 400);
    // The part that is in range is not in a state to be asked for, either way: 400 comes first.
    let done = fx
        .row(
            &fx.job().await,
            1,
            "result",
            10,
            "completing",
            PART_BYTES as i64,
        )
        .await;
    assert_eq!(fx.part(&done, "2").await.status, 400);
    fx.finish().await;
}

/// A row that is not `open` gets no URL: 409 carrying its state, and the store is not asked
/// (ADR-493 item 4, item 18).
#[tokio::test]
async fn a_part_url_for_a_row_that_is_not_open_is_409_with_its_state() {
    let fx = Fx::new(With::Real).await;
    for state in ["completing", "finished", "failed"] {
        let job = fx.job().await;
        let id = fx
            .row(&job, 1, "result", 10, state, PART_BYTES as i64)
            .await;
        let r = fx.part(&id, "1").await;
        problem(&r, 409, state);
        assert_eq!(json(&r)["state"], state);
        assert!(!r.body.contains("http"), "{}", r.body);
        // The status of such a row has no `partsHeld` and asks the store nothing.
        let s = fx.status(&id).await;
        assert_eq!(s.status, 200, "{}", s.head);
        let v = json(&s);
        assert_eq!(v["state"], state);
        assert_eq!(v["partBytes"], PART_BYTES);
        assert_eq!(v["partCount"], 1);
        assert!(v.get("partsHeld").is_none(), "{v}");
        // HEAD answers the same without a body.
        let head = fx
            .send(
                "HEAD",
                &format!("/api/v1/agent/uploads/{id}"),
                Some(&fx.token),
                None,
                "",
            )
            .await;
        assert_eq!(head.status, 200);
        assert_eq!(head.body, "");
    }
    assert_eq!(fx.wrap.calls(), [0, 0, 0, 0]);
    fx.finish().await;
}

#[tokio::test]
async fn a_part_url_and_a_status_need_the_store_and_say_so_when_it_fails() {
    let fx = Fx::new(With::Nothing).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    problem(&fx.part(&id, "1").await, 503, "no store, part");
    problem(&fx.status(&id).await, 503, "no store, status");
    // A closed row needs none.
    let closed = fx
        .row(
            &fx.job().await,
            1,
            "result",
            10,
            "failed",
            PART_BYTES as i64,
        )
        .await;
    assert_eq!(fx.status(&closed).await.status, 200);
    fx.finish().await;

    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    // The store refuses the URL: 503; it has no randomness: 500; the row's key does not parse: 500.
    Wrap::set(&fx.wrap.presign_fail, Some(Fail::Io));
    problem(&fx.part(&id, "1").await, 503, "presign fails");
    Wrap::set(&fx.wrap.presign_fail, Some(Fail::Random));
    problem(&fx.part(&id, "1").await, 500, "presign random");
    Wrap::set(&fx.wrap.presign_fail, None);
    sqlx::query("UPDATE job_objects SET storage_key = repeat('x', 36) WHERE id = $1::uuid")
        .bind(&id)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    problem(&fx.part(&id, "1").await, 500, "unparsable key");
    problem(&fx.status(&id).await, 500, "unparsable key, status");
    // A store that has lost the upload, or fails, is a 503 and the row is not written.
    sqlx::query("UPDATE job_objects SET storage_key = gen_random_uuid()::text WHERE id = $1::uuid")
        .bind(&id)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    let before = fx
        .sql(&format!(
            "SELECT xmin::text || state FROM job_objects WHERE id = '{id}'"
        ))
        .await;
    for fail in [Fail::NotFound, Fail::AlreadyFinished, Fail::Io] {
        Wrap::set(&fx.wrap.list_fail, Some(fail));
        problem(&fx.status(&id).await, 503, "list fails");
    }
    Wrap::set(&fx.wrap.list_fail, Some(Fail::Random));
    problem(&fx.status(&id).await, 500, "list random");
    assert_eq!(
        fx.sql(&format!(
            "SELECT xmin::text || state FROM job_objects WHERE id = '{id}'"
        ))
        .await,
        before
    );
    Wrap::set(&fx.wrap.list_fail, None);
    // An upload with no parts holds none.
    let v = json(&fx.status(&id).await);
    assert_eq!(v["partsHeld"], 0);
    fx.finish().await;
}

/// The three routes against the real object server with small parts: a URL for each part, a PUT
/// to it stored, and the status counting the leading parts of the expected size only.
#[tokio::test]
async fn a_put_to_a_part_url_is_stored_and_the_status_counts_the_leading_parts() {
    let fx = Fx::sized(With::Real, Some(SMALL)).await;
    let job = fx.job().await;
    let size = 3 * SMALL + 10;
    let r = fx.begin(&result_of(&job, size)).await;
    assert_eq!(r.status, 201, "{}", r.head);
    let id = upload_id(&r);
    assert_eq!(json(&r)["partBytes"], SMALL);
    assert_eq!(json(&r)["partCount"], 4);
    assert_eq!(held(&fx, &id).await, 0);

    // Part 1, then 3 (a gap at 2), then a part of another size at 2.
    send(&fx, &id, 1, SMALL as usize).await;
    assert_eq!(held(&fx, &id).await, 1);
    send(&fx, &id, 3, SMALL as usize).await;
    assert_eq!(
        held(&fx, &id).await,
        1,
        "a part beyond a gap is not counted"
    );
    send(&fx, &id, 2, SMALL as usize - 1).await;
    assert_eq!(
        held(&fx, &id).await,
        1,
        "a part of another size is not counted"
    );
    // Sending it again, as asked for again, replaces it.
    send(&fx, &id, 2, SMALL as usize).await;
    assert_eq!(held(&fx, &id).await, 3);
    // The last part has the remainder as its size; a whole part there is another size.
    send(&fx, &id, 4, SMALL as usize).await;
    assert_eq!(held(&fx, &id).await, 3);
    send(&fx, &id, 4, 10).await;
    assert_eq!(held(&fx, &id).await, 4);
    // Past the count is a 400.
    problem(&fx.part(&id, "5").await, 400, "past the count");
    assert_eq!(fx.wrap.calls()[1], 6);
    fx.finish().await;
}

/// The size of an object that is a multiple of the part size, and an empty one (T4, T5).
#[tokio::test]
async fn the_last_part_of_an_exact_multiple_is_a_whole_part_and_an_empty_object_is_one_part() {
    let fx = Fx::sized(With::Real, Some(SMALL)).await;
    let job = fx.job().await;
    let r = fx.begin(&result_of(&job, 2 * SMALL)).await;
    assert_eq!(json(&r)["partCount"], 2);
    let id = upload_id(&r);
    for n in 1..=2u16 {
        let r = fx.part(&id, &n.to_string()).await;
        let url = json(&r)["url"].as_str().unwrap().to_owned();
        assert_eq!(put(&url, content(n as u8, SMALL as usize)).await.0, 200);
        assert_eq!(json(&fx.status(&id).await)["partsHeld"], n);
    }
    let job = fx.job().await;
    let r = fx.begin(&result_of(&job, 0)).await;
    assert_eq!(json(&r)["partCount"], 1);
    let id = upload_id(&r);
    assert_eq!(json(&fx.status(&id).await)["partsHeld"], 0);
    let url = json(&fx.part(&id, "1").await)["url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(put(&url, Vec::new()).await.0, 200);
    // The pinned object server lists a part of no bytes, so the empty object's only part counts.
    assert_eq!(held(&fx, &id).await, 1);
    fx.finish().await;
}

/// The one test with the real part size: 5 MiB, a part of it PUT and counted.
#[tokio::test]
async fn the_part_size_of_the_server_is_5_mib() {
    let fx = Fx::new(With::Real).await;
    let job = fx.job().await;
    let r = fx.begin(&result_of(&job, 5 * MIB as u64 + 3)).await;
    assert_eq!(json(&r)["partBytes"], 5 * MIB as u64);
    assert_eq!(json(&r)["partCount"], 2);
    let id = upload_id(&r);
    for (n, len) in [(1u16, 5 * MIB), (2, 3)] {
        let url = json(&fx.part(&id, &n.to_string()).await)["url"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(put(&url, content(n as u8, len)).await.0, 200);
    }
    assert_eq!(json(&fx.status(&id).await)["partsHeld"], 2);
    fx.finish().await;
}

#[tokio::test]
async fn the_routes_take_their_methods_only() {
    let fx = Fx::new(With::Nothing).await;
    let id = v4();
    let cases = [
        ("GET", "/api/v1/agent/uploads".to_owned(), "POST"),
        ("PUT", "/api/v1/agent/uploads".to_owned(), "POST"),
        ("GET", format!("/api/v1/agent/uploads/{id}/parts/1"), "POST"),
        (
            "HEAD",
            format!("/api/v1/agent/uploads/{id}/parts/1"),
            "POST",
        ),
        ("POST", format!("/api/v1/agent/uploads/{id}"), "GET,HEAD"),
        ("DELETE", format!("/api/v1/agent/uploads/{id}"), "GET,HEAD"),
        (
            "GET",
            format!("/api/v1/agent/uploads/{id}/complete"),
            "POST",
        ),
        (
            "PUT",
            format!("/api/v1/agent/uploads/{id}/complete"),
            "POST",
        ),
        (
            "HEAD",
            format!("/api/v1/agent/uploads/{id}/complete"),
            "POST",
        ),
    ];
    for (method, target, allow) in cases {
        let r = fx.send(method, &target, Some(&fx.token), None, "").await;
        assert_eq!(r.status, 405, "{method} {target}: {}", r.head);
        assert_eq!(r.header("allow"), Some(allow), "{method} {target}");
    }
    // The completion is served: an upload nobody has is the route's own 404, not the fallback's.
    let r = fx.complete(&id).await;
    problem(&r, 404, "unknown upload");
    assert!(r.body.contains("No such upload"), "{}", r.body);
    fx.finish().await;
}

/// Nothing a request or an answer carries is in the log: the token, the digest, the job, the
/// object key, the upload ID, the URL and its signature, the database's text.
#[tokio::test]
async fn the_log_holds_no_token_digest_key_upload_id_url_or_database_text() {
    let (log, _guard) = capture();
    let fx = Fx::sized(With::Real, Some(SMALL)).await;
    let job = fx.job().await;
    let sha = "9f".repeat(32);
    let r = fx.begin(&decl(&job, 1, "result", SMALL + 5, &sha)).await;
    assert_eq!(r.status, 201, "{}", r.head);
    let id = upload_id(&r);
    let r = fx.part(&id, "1").await;
    let url = json(&r)["url"].as_str().unwrap().to_owned();
    assert_eq!(put(&url, content(1, SMALL as usize)).await.0, 200);
    assert_eq!(fx.status(&id).await.status, 200);
    // Failures of every kind the routes log: the store, the database, a lost race's abort.
    Wrap::set(&fx.wrap.list_fail, Some(Fail::Io));
    assert_eq!(fx.status(&id).await.status, 503);
    Wrap::set(&fx.wrap.list_fail, None);
    // A completion the store fails, and a part problem it reports.
    Wrap::set(&fx.wrap.complete_fail, Some(Fail::Io));
    assert_eq!(fx.complete(&id).await.status, 503);
    Wrap::set(&fx.wrap.complete_fail, None);
    assert_eq!(fx.complete(&id).await.status, 409);
    Wrap::set(&fx.wrap.delete_fail, Some(Fail::Io));
    *fx.wrap.begin_barrier.lock().unwrap() = Some(Arc::new(Barrier::new(2)));
    let job2 = fx.job().await;
    let body = decl(&job2, 1, "result", 7, &sha);
    let (a, b) = tokio::join!(fx.begin(&body), fx.begin(&body));
    assert_eq!(a.status.min(b.status), 200);
    *fx.wrap.begin_barrier.lock().unwrap() = None;
    fx.u.w
        .pg
        .execute("ALTER TABLE job_objects ADD CONSTRAINT refuse CHECK (false) NOT VALID")
        .await;
    let job3 = fx.job().await;
    assert_eq!(
        fx.begin(&decl(&job3, 1, "result", 7, &sha)).await.status,
        503
    );

    let upload_ids: Vec<String> = sqlx::query_scalar("SELECT upload_id FROM job_objects")
        .fetch_all(&fx.u.w.pool)
        .await
        .unwrap();
    let keys: Vec<String> = sqlx::query_scalar("SELECT storage_key FROM job_objects")
        .fetch_all(&fx.u.w.pool)
        .await
        .unwrap();
    let mut secrets = vec![
        fx.token.clone(),
        sha.clone(),
        job.clone(),
        job2,
        job3,
        url.clone(),
        id.clone(),
    ];
    secrets.extend(upload_ids);
    secrets.extend(keys);
    secrets.extend(fx.wrap.issued.lock().unwrap().iter().cloned());
    let query = url.split_once('?').unwrap().1.to_owned();
    secrets.push(query);
    let text = log.text();
    assert!(
        text.contains("upload route"),
        "the failures are logged: {text}"
    );
    for secret in &secrets {
        assert!(!text.contains(secret.as_str()), "a secret is in the log");
    }
    for word in ["X-Amz", "sqlx", "constraint", "violates", "refuse"] {
        assert!(!text.contains(word), "{word} is in the log: {text}");
    }
    fx.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Complete

/// The object the parts `(number, length)` hold, as `send` PUTs them.
fn object_of(parts: &[(u16, usize)]) -> Vec<u8> {
    parts
        .iter()
        .flat_map(|(n, len)| content(*n as u8, *len))
        .collect()
}

/// Case R1 (the one-part and the empty object) and the finished row: 200 `finished`, the row
/// `finished` with its time, and the object on the object server byte for byte what was PUT.
#[tokio::test]
async fn a_completed_one_part_or_empty_object_is_finished_and_byte_identical() {
    let fx = Fx::new(With::Real).await;
    for len in [1000usize, 0] {
        let bytes = content(1, len);
        let id = begin_for(&fx, &bytes).await;
        send(&fx, &id, 1, len).await;
        let r = fx.complete(&id).await;
        assert_eq!(r.status, 200, "{}", r.head);
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(r.header("cache-control"), Some("no-store"));
        assert_eq!(json(&r), serde_json::json!({"state": "finished"}));
        assert_eq!(fx.state(&id).await, "finished");
        assert_eq!(
            fx.sql(&format!(
                "SELECT (finished_at IS NOT NULL)::text FROM job_objects WHERE id = '{id}'"
            ))
            .await,
            "true"
        );
        assert_eq!(fx.stored(&id).await.unwrap(), bytes);
        // The status of a finished row has no `partsHeld`.
        assert!(json(&fx.status(&id).await).get("partsHeld").is_none());
    }
    // A repeat on a `finished` row is 200 and asks the store nothing.
    let id = fx
        .sql("SELECT id FROM job_objects ORDER BY created_at LIMIT 1")
        .await;
    let before = fx.wrap.completes.load(Ordering::SeqCst);
    assert_eq!(fx.complete(&id).await.status, 200);
    assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), before);
    fx.finish().await;
}

/// Case R1 with the real part size: 5 MiB and a few bytes are two parts.
#[tokio::test]
async fn a_completed_object_of_two_parts_is_finished_and_byte_identical() {
    let fx = Fx::new(With::Real).await;
    let parts = [(1u16, 5 * MIB), (2, 3)];
    let bytes = object_of(&parts);
    let id = begin_for(&fx, &bytes).await;
    for (n, len) in parts {
        send(&fx, &id, n, len).await;
    }
    let r = fx.complete(&id).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(fx.state(&id).await, "finished");
    assert!(fx.stored(&id).await.unwrap() == bytes);
    fx.finish().await;
}

/// Case S: a digest that is not the object's makes the row `failed` (409 with that state), the
/// store has deleted the object, and a new begin of the declaration is allowed.
#[tokio::test]
async fn a_digest_mismatch_fails_the_row_and_a_new_begin_is_allowed() {
    let fx = Fx::new(With::Real).await;
    let job = fx.job().await;
    let r = fx.begin(&result_of(&job, 1000)).await;
    let id = upload_id(&r);
    send(&fx, &id, 1, 1000).await;
    let r = fx.complete(&id).await;
    problem(&r, 409, "digest mismatch");
    assert_eq!(json(&r)["state"], "failed");
    assert_eq!(fx.state(&id).await, "failed");
    assert!(matches!(fx.stored(&id).await, Err(StoreError::NotFound)));
    // The route deleted nothing itself: the store did.
    assert_eq!(fx.wrap.calls()[3], 0);
    // The same answer again, from the row, and a new upload for the declaration.
    let again = fx.complete(&id).await;
    problem(&again, 409, "failed row");
    assert_eq!(json(&again)["state"], "failed");
    let new = fx.begin(&result_of(&job, 1000)).await;
    assert_eq!(new.status, 201, "{}", new.head);
    assert_ne!(upload_id(&new), id);
    fx.finish().await;
}

/// Case S: a part problem returns the row to `open` with the parts untouched, and the agent can
/// send them and complete again.
#[tokio::test]
async fn a_part_problem_returns_the_row_to_open_and_a_later_completion_finishes_it() {
    let fx = Fx::sized(With::Real, Some(SMALL)).await;
    let bytes = content(1, 1000);
    let id = begin_for(&fx, &bytes).await;
    // No part at all.
    let r = fx.complete(&id).await;
    problem(&r, 409, "no parts");
    assert_eq!(json(&r)["state"], "open");
    assert_eq!(fx.state(&id).await, "open");
    assert_eq!(held(&fx, &id).await, 0);
    send(&fx, &id, 1, 1000).await;
    let r = fx.complete(&id).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(fx.state(&id).await, "finished");
    assert_eq!(fx.stored(&id).await.unwrap(), bytes);
    fx.finish().await;
}

/// Cases S and D: every answer of the store and the row it leaves, and the repeat of a row left
/// `open` or `completing`.
#[tokio::test]
async fn the_stores_answers_decide_the_row_and_the_status() {
    let fx = Fx::new(With::Fake).await;
    let cases = [
        (Fail::NoParts, 409, "open"),
        (Fail::MissingPart, 409, "open"),
        (Fail::PartTooSmall, 409, "open"),
        (Fail::PartsLengthMismatch, 409, "open"),
        (Fail::PartsChanged, 409, "open"),
        (Fail::ObjectTooLarge, 409, "open"),
        (Fail::HashMismatch, 409, "failed"),
        (Fail::LengthMismatch, 409, "failed"),
        (Fail::NotFound, 409, "failed"),
        (Fail::Io, 503, "completing"),
        (Fail::Refused, 503, "completing"),
        (Fail::AlreadyFinished, 503, "completing"),
        (Fail::Random, 500, "completing"),
    ];
    for (fail, status, row_state) in cases {
        let job = fx.job().await;
        let id = fx
            .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
            .await;
        Wrap::set(&fx.wrap.complete_fail, Some(fail));
        let r = fx.complete(&id).await;
        problem(&r, status, row_state);
        if status == 409 {
            assert_eq!(json(&r)["state"], row_state);
        } else {
            assert!(json(&r).get("state").is_none());
        }
        assert_eq!(fx.state(&id).await, row_state);
        // A repeat with the store well: a row that is `open` or `completing` is completed, a
        // `failed` one is answered as it is.
        Wrap::set(&fx.wrap.complete_fail, None);
        let before = fx.wrap.completes.load(Ordering::SeqCst);
        let r = fx.complete(&id).await;
        if row_state == "failed" {
            problem(&r, 409, "failed repeat");
            assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), before);
        } else {
            assert_eq!(r.status, 200, "{}", r.head);
            assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), before + 1);
            assert_eq!(fx.state(&id).await, "finished");
        }
    }
    fx.finish().await;
}

/// Case C1: two requests at once run one completion, and both are answered 200.
#[tokio::test]
async fn two_concurrent_completions_run_one_store_completion() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let hold = Hold::new();
    *fx.wrap.complete_hold.lock().unwrap() = Some(Arc::clone(&hold));
    let first = fx.complete(&id);
    let second = async {
        hold.entered().await;
        // The first is inside the store; this one joins it.
        let second = fx.complete(&id);
        let release = async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            hold.release();
        };
        tokio::join!(second, release).0
    };
    let (a, b) = tokio::join!(first, second);
    assert_eq!((a.status, b.status), (200, 200), "{} / {}", a.head, b.head);
    assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), 1);
    assert_eq!(fx.state(&id).await, "finished");
    fx.finish().await;
}

/// Case R2: a request that finds `completing` and no task (as after a crash) verifies the object
/// and finishes the row.
#[tokio::test]
async fn a_completing_row_with_no_task_is_verified_and_finished() {
    let fx = Fx::new(With::Real).await;
    let bytes = content(1, 1000);
    let id = begin_for(&fx, &bytes).await;
    send(&fx, &id, 1, 1000).await;
    sqlx::query("UPDATE job_objects SET state = 'completing' WHERE id = $1::uuid")
        .bind(&id)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    let r = fx.complete(&id).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(fx.state(&id).await, "finished");
    assert_eq!(fx.stored(&id).await.unwrap(), bytes);
    // A completion that was cut after the store's completed it: the upload is gone, the object
    // is there, and the repeat reads it back again.
    let bytes = content(1, 10);
    let id = begin_for(&fx, &bytes).await;
    send(&fx, &id, 1, 10).await;
    let upload = Upload {
        key: ObjectKey::parse(
            &fx.sql(&format!(
                "SELECT storage_key FROM job_objects WHERE id = '{id}'"
            ))
            .await,
        )
        .unwrap(),
        id: UploadId::parse(
            &fx.sql(&format!(
                "SELECT upload_id FROM job_objects WHERE id = '{id}'"
            ))
            .await,
        )
        .unwrap(),
    };
    fx.wrap.complete(&upload, sha256(&bytes), 10).await.unwrap();
    sqlx::query("UPDATE job_objects SET state = 'completing' WHERE id = $1::uuid")
        .bind(&id)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    assert_eq!(fx.complete(&id).await.status, 200);
    assert_eq!(fx.state(&id).await, "finished");
    fx.finish().await;
}

/// Case R3: a `completing` row whose upload and object are both gone (a stop between a
/// mismatch's delete and the row's update) is made `failed`, and a new begin is allowed.
#[tokio::test]
async fn a_completing_row_with_nothing_left_is_failed_and_a_new_begin_is_allowed() {
    let fx = Fx::new(With::Real).await;
    let job = fx.job().await;
    let r = fx.begin(&result_of(&job, 10)).await;
    let id = upload_id(&r);
    // The upload is gone and so is the object: delete it as the store's own mismatch would.
    let key = ObjectKey::parse(
        &fx.sql(&format!(
            "SELECT storage_key FROM job_objects WHERE id = '{id}'"
        ))
        .await,
    )
    .unwrap();
    fx.wrap.delete(&key).await.unwrap();
    sqlx::query("UPDATE job_objects SET state = 'completing' WHERE id = $1::uuid")
        .bind(&id)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    let r = fx.complete(&id).await;
    problem(&r, 409, "nothing left");
    assert_eq!(json(&r)["state"], "failed");
    assert_eq!(fx.state(&id).await, "failed");
    assert_eq!(fx.begin(&result_of(&job, 10)).await.status, 201);
    fx.finish().await;
}

/// Case T: a store that holds past the wait is a 202 while the task goes on; the repeat after
/// the release is 200 and the store completed once.
#[tokio::test]
async fn a_slow_store_is_202_the_task_goes_on_and_a_repeat_is_200() {
    let fx = Fx::waiting(With::Fake, Duration::from_millis(300)).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let hold = Hold::new();
    *fx.wrap.complete_hold.lock().unwrap() = Some(Arc::clone(&hold));
    let r = fx.complete(&id).await;
    assert_eq!(r.status, 202, "{}", r.head);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(json(&r), serde_json::json!({"state": "completing"}));
    assert_eq!(fx.state(&id).await, "completing");
    // A repeat joins the task that still runs: another 202, and no second store call.
    assert_eq!(fx.complete(&id).await.status, 202);
    assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), 1);
    hold.release();
    fx.until(&id, "finished").await;
    assert_eq!(fx.complete(&id).await.status, 200);
    assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), 1);
    fx.finish().await;
}

/// Case T: a request that goes away does not stop the task, which finishes the row.
#[tokio::test]
async fn a_client_that_leaves_does_not_stop_the_completion() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let hold = Hold::new();
    *fx.wrap.complete_hold.lock().unwrap() = Some(Arc::clone(&hold));
    let waiting = tokio::time::timeout(Duration::from_millis(500), fx.complete(&id)).await;
    assert!(waiting.is_err(), "the request is still waiting");
    hold.entered().await;
    hold.release();
    fx.until(&id, "finished").await;
    fx.finish().await;
}

/// Case C2: a status that finds the upload gone because a completion just finished it answers
/// the row as it is now.
#[tokio::test]
async fn a_status_racing_a_completion_answers_the_finished_row() {
    let fx = Fx::new(With::Real).await;
    let bytes = content(1, 1000);
    let id = begin_for(&fx, &bytes).await;
    send(&fx, &id, 1, 1000).await;
    let hold = Hold::new();
    *fx.wrap.list_hold.lock().unwrap() = Some(Arc::clone(&hold));
    let status = fx.status(&id);
    let race = async {
        // The status has read the `open` row and is at the store's listing.
        hold.entered().await;
        *fx.wrap.list_hold.lock().unwrap() = None;
        let r = fx.complete(&id).await;
        assert_eq!(r.status, 200, "{}", r.head);
        hold.release();
    };
    let (status, ()) = tokio::join!(status, race);
    assert_eq!(status.status, 200, "{}", status.head);
    let v = json(&status);
    assert_eq!(v["state"], "finished");
    assert_eq!(v["partBytes"], PART_BYTES);
    assert_eq!(v["partCount"], 1);
    assert!(v.get("partsHeld").is_none(), "{v}");
    fx.finish().await;
}

/// Case Q3: the stop ends a running task at once (the waiter is answered 503), the row stays
/// `completing`, and a request to a server started on the same database finishes it.
#[tokio::test]
async fn the_stop_ends_a_running_completion_and_a_later_request_finishes_it() {
    let mut fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let hold = Hold::new();
    *fx.wrap.complete_hold.lock().unwrap() = Some(Arc::clone(&hold));
    let addr = fx.addr();
    let raw = fx.request_text(
        "POST",
        &format!("/api/v1/agent/uploads/{id}/complete"),
        Some(&fx.token),
        None,
        "",
    );
    let pending = tokio::spawn(async move { exchange(addr, &raw).await });
    hold.entered().await;
    assert_eq!(fx.state(&id).await, "completing");
    fx.u.s.begin_stop().await;
    let r = tokio::time::timeout(Duration::from_secs(10), pending)
        .await
        .expect("the waiter is answered when the stop begins")
        .unwrap();
    problem(&r, 503, "the stop");
    fx.u.s.finish().await;
    // The server is gone; the row and the fake are read without it.
    assert_eq!(
        fx.u.w
            .sql(&format!("SELECT state FROM job_objects WHERE id = '{id}'"))
            .await,
        "completing"
    );
    assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), 1);

    // A server on the same database resumes it on the next request.
    *fx.wrap.complete_hold.lock().unwrap() = None;
    let http = Http {
        port: 0,
        shutdown_grace_secs: 2,
    };
    let backend = Backend::new(Some(Arc::clone(&fx.wrap) as Arc<dyn MultipartStore>));
    let server = Server::bind_with_store(&http, fx.u.w.pg.pool(), fx.u.w.key.clone(), backend)
        .await
        .unwrap();
    fx.u.s = Running::spawn(server);
    let fresh = token(&fx.u).await;
    let r = fx.complete_as(Some(&fresh), &id).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(fx.state(&id).await, "finished");
    assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), 2);
    fx.finish().await;
}

/// Case C3: a request reads the row before the task wrote `finished` and starts its own task
/// after the first one left the registry: its compare-and-set changes no row, it re-reads
/// `finished` and answers 200 with no second store call.
#[tokio::test]
async fn a_request_that_starts_a_task_after_the_first_finished_answers_the_state() {
    let pause = Pause::new();
    let fx = Fx::paused(With::Fake, Arc::clone(&pause)).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let addr = fx.addr();
    let raw = fx.request_text(
        "POST",
        &format!("/api/v1/agent/uploads/{id}/complete"),
        Some(&fx.token),
        None,
        "",
    );
    // The first request has read the `open` row and waits before it starts a task.
    let held = tokio::spawn(async move { exchange(addr, &raw).await });
    tokio::time::timeout(Duration::from_secs(20), pause.entered())
        .await
        .expect("the request is held");
    // Another request runs the whole completion, which leaves the registry before it answers.
    let r = fx.complete(&id).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), 1);
    pause.release();
    let r = held.await.unwrap();
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(json(&r), serde_json::json!({"state": "finished"}));
    assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), 1);
    fx.finish().await;
}

/// Case T: a task that panics answers its waiters 503 and leaves the registry, so the repeat
/// starts a new task, which finds the row `completing` and finishes it.
#[tokio::test]
async fn a_task_that_panics_is_503_and_the_repeat_starts_a_new_one() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    fx.wrap.complete_panics.store(true, Ordering::SeqCst);
    problem(&fx.complete(&id).await, 503, "the task panicked");
    assert_eq!(fx.state(&id).await, "completing");
    fx.wrap.complete_panics.store(false, Ordering::SeqCst);
    let r = fx.complete(&id).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(fx.state(&id).await, "finished");
    fx.finish().await;
}

/// Case D: no pooled connection is held while a completion waits in the store: more held
/// completions than the pool has connections, and the database is still served.
#[tokio::test]
async fn no_pooled_connection_is_held_while_a_completion_waits_for_the_store() {
    let fx = Fx::new(With::Fake).await;
    const N: usize = 12;
    let hold = Hold::new();
    *fx.wrap.complete_hold.lock().unwrap() = Some(Arc::clone(&hold));
    let addr = fx.addr();
    let mut tasks = Vec::new();
    for _ in 0..N {
        let job = fx.job().await;
        let id = fx
            .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
            .await;
        let raw = fx.request_text(
            "POST",
            &format!("/api/v1/agent/uploads/{id}/complete"),
            Some(&fx.token),
            None,
            "",
        );
        tasks.push(tokio::spawn(
            async move { exchange(addr, &raw).await.status },
        ));
    }
    for _ in 0..1000 {
        if fx.wrap.completes.load(Ordering::SeqCst) == N {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        fx.wrap.completes.load(Ordering::SeqCst),
        N,
        "all are in the store"
    );
    // The server's sessions are idle, and a request that needs the database is served.
    assert_eq!(
        fx.sql(
            "SELECT count(*) FROM pg_stat_activity WHERE application_name = 'ngr-server' \
             AND state <> 'idle'"
        )
        .await,
        "0"
    );
    let job = fx.job().await;
    assert_eq!(fx.begin(&result_of(&job, 1)).await.status, 201);
    hold.release();
    for task in tasks {
        assert_eq!(task.await.unwrap(), 200);
    }
    fx.finish().await;
}

/// Case O1: the order of the judgment. The body before the token, the token before the upload,
/// a foreign upload is the unknown one's 404, and then the store and the row.
#[tokio::test]
async fn complete_judges_the_body_then_the_token_then_the_upload_then_the_store() {
    let fx = Fx::new(With::Nothing).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let target = format!("/api/v1/agent/uploads/{id}/complete");
    // The body: a byte of it is a 400, more than 4096 a 413, both with no token.
    for token in [None, Some(fx.token.as_str())] {
        let r = fx.send("POST", &target, token, None, "x").await;
        problem(&r, 400, "a body");
        assert!(r.body.contains("The body must be empty"), "{}", r.body);
        problem(
            &fx.send("POST", &target, token, None, &"x".repeat(4097))
                .await,
            413,
            "a long body",
        );
    }
    // The token, then the upload.
    let unknown = fx.complete_as(None, &v4()).await;
    problem(&unknown, 401, "no token");
    assert_eq!(unknown.header("www-authenticate"), Some("Bearer"));
    let (_, other) = fx.other_agent().await;
    let foreign = fx.complete_as(Some(&other), &id).await;
    problem(&foreign, 404, "foreign upload");
    assert_eq!(
        bytes_of(&foreign),
        bytes_of(&fx.complete_as(Some(&other), &v4()).await)
    );
    problem(
        &fx.complete_as(Some(&fx.token), "garbage").await,
        404,
        "not a UUID",
    );
    problem(
        &fx.complete_as(Some(&fx.token), "%FF").await,
        404,
        "not UTF-8",
    );
    // A row that needs the store finds none.
    problem(&fx.complete(&id).await, 503, "no store");
    assert_eq!(fx.state(&id).await, "open");
    // A `finished` or `failed` row is answered with no store.
    let done = fx
        .row(
            &fx.job().await,
            1,
            "result",
            10,
            "finished",
            PART_BYTES as i64,
        )
        .await;
    assert_eq!(fx.complete(&done).await.status, 200);
    let failed = fx
        .row(
            &fx.job().await,
            1,
            "result",
            10,
            "failed",
            PART_BYTES as i64,
        )
        .await;
    let r = fx.complete(&failed).await;
    problem(&r, 409, "failed");
    assert_eq!(json(&r)["state"], "failed");
    fx.finish().await;

    // A row that does not parse is the server's fault, and nothing is written.
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    sqlx::query("UPDATE job_objects SET storage_key = repeat('x', 36) WHERE id = $1::uuid")
        .bind(&id)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    problem(&fx.complete(&id).await, 500, "unparsable key");
    assert_eq!(fx.state(&id).await, "open");
    assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), 0);
    fx.finish().await;
}

/// Case D: a database that fails during the completion is a 503 and a later request repeats.
#[tokio::test]
async fn a_database_that_fails_in_the_completion_is_503_and_the_repeat_finishes() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    // The write of the result fails after the store completed: the row stays `completing`.
    fx.u.w
        .pg
        .execute(
            "CREATE FUNCTION refuse_finish() RETURNS trigger AS $$ BEGIN \
             IF NEW.state = 'finished' THEN RAISE EXCEPTION 'no'; END IF; RETURN NEW; END; $$ \
             LANGUAGE plpgsql",
        )
        .await;
    fx.u.w
        .pg
        .execute(
            "CREATE TRIGGER refuse_finish BEFORE UPDATE ON job_objects \
             FOR EACH ROW EXECUTE FUNCTION refuse_finish()",
        )
        .await;
    problem(&fx.complete(&id).await, 503, "the final write fails");
    assert_eq!(fx.state(&id).await, "completing");
    assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), 1);
    fx.u.w
        .pg
        .execute("DROP TRIGGER refuse_finish ON job_objects")
        .await;
    // The repeat verifies the object again and finishes the row.
    assert_eq!(fx.complete(&id).await.status, 200);
    assert_eq!(fx.state(&id).await, "finished");
    assert_eq!(fx.wrap.completes.load(Ordering::SeqCst), 2);
    fx.finish().await;
}

/// A failure of the completion is logged in fixed words only: no upload ID, key, digest or
/// database text.
#[tokio::test]
async fn the_log_of_a_failed_completion_holds_no_upload_id_key_digest_or_database_text() {
    let (log, _guard) = capture();
    let mut secrets = Vec::new();
    // The store fails.
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    Wrap::set(&fx.wrap.complete_fail, Some(Fail::Io));
    problem(&fx.complete(&id).await, 503, "store");
    secrets.push(id);
    secrets.push(job);
    secrets.push(fx.token.clone());
    secrets.push(digest());
    secrets.extend(
        sqlx::query_scalar::<_, String>("SELECT storage_key FROM job_objects")
            .fetch_all(&fx.u.w.pool)
            .await
            .unwrap(),
    );
    fx.finish().await;
    // The database fails (a gate of its own on this server's routes).
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&job, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    fx.u.w
        .pg
        .execute("ALTER TABLE job_objects ADD CONSTRAINT refuse CHECK (false) NOT VALID")
        .await;
    problem(&fx.complete(&id).await, 503, "database");
    secrets.push(id);
    secrets.push(job);
    secrets.extend(
        sqlx::query_scalar::<_, String>("SELECT storage_key FROM job_objects")
            .fetch_all(&fx.u.w.pool)
            .await
            .unwrap(),
    );
    fx.finish().await;
    let text = log.text();
    assert!(
        text.contains("the object store failed to complete an upload"),
        "the store's failure is logged: {text}"
    );
    assert!(
        text.contains("the database is unavailable") || text.contains("a statement failed"),
        "the database's failure is logged: {text}"
    );
    for secret in &secrets {
        assert!(!text.contains(secret.as_str()), "a secret is in the log");
    }
    for word in ["sqlx", "constraint", "violates", "refuse"] {
        assert!(!text.contains(word), "{word} is in the log: {text}");
    }
}

/// The migration's own rules (ADR-493 item 7): each CHECK, the partial unique index and the
/// foreign keys, which have no cascade.
#[tokio::test]
async fn the_table_refuses_what_the_rules_forbid() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let insert = |set: &'static str| {
        let pool = fx.u.w.pool.clone();
        let job = job.clone();
        async move {
            // A valid row, with one column replaced by `set` (`column = expression`).
            let (column, expression) = set.split_once('=').unwrap_or(("id", "id"));
            let columns = [
                ("generation", "1"),
                ("kind", "'result'"),
                ("storage_key", "gen_random_uuid()::text"),
                ("upload_id", "'u'"),
                ("part_bytes", "5242880"),
                ("size_bytes", "10"),
                ("digest", "repeat('a', 64)"),
                ("state", "'open'"),
                ("finished_at", "NULL"),
            ];
            let values: Vec<String> = columns
                .iter()
                .map(|(c, v)| {
                    if *c == column.trim() {
                        expression.to_owned()
                    } else {
                        (*v).to_owned()
                    }
                })
                .collect();
            let sql = format!(
                "INSERT INTO job_objects (tenant_id, id, job_id, agent_id, generation, kind, \
                     storage_key, upload_id, part_bytes, size_bytes, digest, state, finished_at) \
                 SELECT tenant_id, gen_random_uuid(), id, agent_id, {} FROM jobs WHERE id = '{job}'",
                values.join(", ")
            );
            sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&pool).await
        }
    };
    for bad in [
        "generation = -1",
        "kind = 'log'",
        "storage_key = 'short'",
        "upload_id = ''",
        "upload_id = repeat('x', 1025)",
        "part_bytes = 0",
        "size_bytes = -1",
        "size_bytes = 4294967297",
        "digest = repeat('A', 64)",
        "digest = repeat('a', 63)",
        "state = 'done'",
        "state = 'finished'",
        "finished_at = now()",
    ] {
        assert!(insert(bad).await.is_err(), "{bad}");
    }
    for good in [
        "size_bytes = 0",
        "size_bytes = 4294967296",
        "kind = 'commLog'",
        "state = 'failed'",
    ] {
        sqlx::query("DELETE FROM job_objects")
            .execute(&fx.u.w.pool)
            .await
            .unwrap();
        assert!(insert(good).await.is_ok(), "{good}");
    }
    // One live row per job, generation and kind: a second `open` or `completing` is refused, a
    // failed one is not counted.
    sqlx::query("DELETE FROM job_objects")
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    assert!(insert("state = 'open'").await.is_ok());
    assert!(insert("state = 'completing'").await.is_err());
    assert!(insert("generation = 2").await.is_ok());
    assert!(insert("state = 'failed'").await.is_ok());
    assert!(insert("state = 'failed'").await.is_ok());
    // The key of an object is one row's.
    sqlx::query("UPDATE job_objects SET storage_key = repeat('a', 36)")
        .execute(&fx.u.w.pool)
        .await
        .expect_err("one key for several rows");
    // Foreign keys, with no cascade: no row without its job, and no job, agent or tenant deleted
    // under a row.
    sqlx::query("UPDATE job_objects SET job_id = gen_random_uuid()")
        .execute(&fx.u.w.pool)
        .await
        .expect_err("a row names a job that exists");
    for sql in [
        "DELETE FROM jobs",
        "DELETE FROM agents",
        "DELETE FROM tenants",
    ] {
        assert!(
            sqlx::query(sql).execute(&fx.u.w.pool).await.is_err(),
            "{sql}"
        );
    }
    fx.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The answer proof (ADR-504)

/// One request of a data route, as the agent writes it.
pub(crate) struct Call {
    pub(crate) name: &'static str,
    pub(crate) method: &'static str,
    pub(crate) target: String,
    pub(crate) content_type: Option<&'static str>,
    pub(crate) body: String,
}

/// A valid request to each of the four upload routes: begin with `declaration`, the part URL of
/// part 1, the status and the completion of upload `id`.
pub(crate) fn calls(declaration: &str, id: &str) -> Vec<Call> {
    vec![
        Call {
            name: "begin",
            method: "POST",
            target: BEGIN_PATH.to_owned(),
            content_type: Some("application/json"),
            body: declaration.to_owned(),
        },
        Call {
            name: "part",
            method: "POST",
            target: format!("{BEGIN_PATH}/{id}/parts/1"),
            content_type: None,
            body: String::new(),
        },
        Call {
            name: "status",
            method: "GET",
            target: format!("{BEGIN_PATH}/{id}"),
            content_type: None,
            body: String::new(),
        },
        Call {
            name: "complete",
            method: "POST",
            target: format!("{BEGIN_PATH}/{id}/complete"),
            content_type: None,
            body: String::new(),
        },
    ]
}

fn fresh() -> [u8; 32] {
    shared_crypto::random_32().unwrap()
}

impl Fx {
    /// `call` with the agent's token and the header lines `extra`.
    pub(crate) async fn call(&self, call: &Call, token: Option<&str>, extra: &[String]) -> Reply {
        self.send_with(
            call.method,
            &call.target,
            token,
            call.content_type,
            extra,
            &call.body,
        )
        .await
    }

    /// `call` with the agent's token and a fresh nonce, which is returned.
    pub(crate) async fn nonced(&self, call: &Call, extra: &[String]) -> ([u8; 32], Reply) {
        let nonce = fresh();
        let lines = [vec![nonce_line(&nonce)], extra.to_vec()].concat();
        (nonce, self.call(call, Some(&self.token), &lines).await)
    }

    /// Asserts that `reply` is the answer of `status` to the request of `method`, `target` and
    /// `request_body` made with `nonce`, proved as the agent checks it.
    pub(crate) fn assert_proved_for(
        &self,
        nonce: [u8; 32],
        method: &str,
        target: &str,
        request_body: &str,
        status: u16,
        reply: &Reply,
    ) {
        let public = self.u.w.key.public_key();
        let audience = audience_of(self.addr().port());
        Answered {
            public: &public,
            audience: &audience,
            agent_id: &self.u.id,
            nonce,
            method,
            target,
            request_body: request_body.as_bytes(),
        }
        .verify(status, reply);
    }

    /// Sends `call` with a fresh nonce and asserts the answer is `status`, proved.
    pub(crate) async fn proved(&self, call: &Call, status: u16) -> Reply {
        let (nonce, r) = self.nonced(call, &[]).await;
        self.assert_proved_for(nonce, call.method, &call.target, &call.body, status, &r);
        r
    }
}

#[tokio::test]
async fn an_answer_without_a_nonce_carries_no_proof() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&fx.job().await, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let mut all = calls(&result_of(&job, 10), &id);
    // Answers of several statuses: the created row, the same row again, a 404 and the errors.
    all.extend(calls(&result_of(&job, 10), &v4()));
    all.extend(calls(&result_of(&v4(), 10), &id));
    let mut statuses = Vec::new();
    for call in &all {
        let r = fx.call(call, Some(&fx.token), &[]).await;
        assert_unproved(&r, call.name);
        statuses.push(r.status);
    }
    for want in [200, 201, 404, 503] {
        assert!(statuses.contains(&want), "{want} not in {statuses:?}");
    }
    fx.finish().await;
}

#[tokio::test]
async fn no_answer_before_the_token_carries_a_proof() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&fx.job().await, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let good = result_of(&job, 10);
    let line = [nonce_line(&fresh())];
    let json_type = Some("application/json");
    // The body step and the shape, whatever the token.
    let begin = |body: &str| ("POST", BEGIN_PATH.to_owned(), json_type, body.to_owned());
    let over_length = decl(&job, 1, "result", MAX_SIZE + 1, &digest());
    let mut cases = vec![
        ("shape", 400, begin("{}")),
        ("length", 413, begin(&over_length)),
        (
            "body size",
            413,
            begin(&format!("{good}{}", " ".repeat(4096))),
        ),
    ];
    for call in calls(&good, &id).into_iter().skip(1) {
        cases.push((
            call.name,
            400,
            (call.method, call.target, None, "x".to_owned()),
        ));
    }
    cases.push((
        "part number",
        400,
        (
            "POST",
            format!("{BEGIN_PATH}/{id}/parts/0"),
            None,
            String::new(),
        ),
    ));
    for (name, status, (method, target, content_type, body)) in &cases {
        for token in [None, Some(fx.token.as_str())] {
            let r = fx
                .send_with(method, target, token, *content_type, &line, body)
                .await;
            assert_eq!(r.status, *status, "{name}: {}", r.head);
            assert_unproved(&r, name);
        }
    }
    // The token, then the agent's row.
    for call in calls(&good, &id) {
        let r = fx.call(&call, None, &line).await;
        problem(&r, 401, call.name);
        assert_unproved(&r, call.name);
        let r = fx.call(&call, Some("x"), &line).await;
        problem(&r, 401, call.name);
        assert_unproved(&r, call.name);
    }
    sqlx::query("UPDATE agents SET revoked_at = now() WHERE id = $1::uuid")
        .bind(&fx.u.id)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    for call in calls(&good, &id) {
        let r = fx.call(&call, Some(&fx.token), &line).await;
        problem(&r, 401, call.name);
        assert_unproved(&r, call.name);
    }
    fx.finish().await;
}

#[tokio::test]
async fn a_body_that_does_not_arrive_is_an_unproved_408() {
    let fx = Fx::new(With::Nothing).await;
    let line = nonce_line(&fresh());
    let requests: Vec<String> = calls(&result_of(&v4(), 1), &v4())
        .iter()
        .map(|call| {
            format!(
                "{} {} HTTP/1.1\r\n{}\r\n{line}\r\nContent-Type: application/json\r\n\
                 Content-Length: 10\r\n\r\n",
                call.method,
                call.target,
                fx.u.s.host()
            )
        })
        .collect();
    let replies =
        futures_util::future::join_all(requests.iter().map(|raw| exchange(fx.addr(), raw))).await;
    for r in &replies {
        problem(r, 408, "no body");
        assert_unproved(r, "no body");
    }
    fx.finish().await;
}

/// What a guard refusal and a wrong method answer with a nonce: no proof, since no route ran.
#[tokio::test]
async fn the_guard_and_the_method_answer_unproved() {
    let fx = Fx::new(With::Fake).await;
    let port = fx.addr().port();
    let line = nonce_line(&fresh());
    for call in calls(&result_of(&v4(), 1), &v4()) {
        let host = |host: &str| {
            format!(
                "{} {} HTTP/1.1\r\nHost: {host}\r\n{line}\r\nConnection: close\r\n\
                 Content-Length: 0\r\n\r\n",
                call.method, call.target
            )
        };
        let r = exchange(fx.addr(), &host("example.invalid")).await;
        assert_eq!(r.status, 421, "{}", call.name);
        assert_unproved(&r, call.name);
        let r = exchange(fx.addr(), &host(&format!("localhost:{port}"))).await;
        assert_eq!(r.status, 307, "{}", call.name);
        assert_unproved(&r, call.name);
        let origin = ["Origin: http://example.invalid".to_owned(), line.clone()];
        let r = fx.call(&call, Some(&fx.token), &origin).await;
        assert_eq!(r.status, 403, "{}", call.name);
        assert_unproved(&r, call.name);
        // An absolute-form target is the guard's 400.
        let absolute = format!(
            "{} http://127.0.0.1:{port}{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{line}\r\n\
             Connection: close\r\nContent-Length: 0\r\n\r\n",
            call.method, call.target
        );
        let r = exchange(fx.addr(), &absolute).await;
        assert_eq!(r.status, 400, "{}", call.name);
        assert_unproved(&r, call.name);
        // Another method than the route's.
        let other = if call.method == "GET" { "PUT" } else { "GET" };
        let r = fx
            .send_with(
                other,
                &call.target,
                Some(&fx.token),
                None,
                std::slice::from_ref(&line),
                "",
            )
            .await;
        assert_eq!(r.status, 405, "{}", call.name);
        assert_unproved(&r, call.name);
    }
    fx.finish().await;
}

/// Nonces that are not the canonical base64 of 32 bytes, and two of them.
pub(crate) fn bad_nonce_lines() -> Vec<(&'static str, Vec<String>)> {
    use base64::Engine;
    let name = shared_proto::NONCE_HEADER;
    let engine = base64::engine::general_purpose::STANDARD;
    let nonce = fresh();
    let good = engine.encode(nonce);
    // The last data character of 32 bytes carries two padding bits, clear in the canonical text.
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let last = alphabet
        .iter()
        .position(|c| *c == good.as_bytes()[42])
        .unwrap();
    let loose = format!("{}{}=", &good[..42], alphabet[last | 1] as char);
    vec![
        ("not base64", vec![format!("{name}: !!not base64!!")]),
        (
            "31 bytes",
            vec![format!("{name}: {}", engine.encode(&nonce[..31]))],
        ),
        (
            "33 bytes",
            vec![format!(
                "{name}: {}",
                engine.encode([&nonce[..], &[0]].concat())
            )],
        ),
        ("non-canonical", vec![format!("{name}: {loose}")]),
        (
            "unpadded",
            vec![format!("{name}: {}", good.trim_end_matches('='))],
        ),
        ("empty", vec![format!("{name}:")]),
        ("non-ASCII", vec![format!("{name}: \u{e9}")]),
        (
            "two values",
            vec![format!("{name}: {good}"), format!("{name}: {good}")],
        ),
    ]
}

#[tokio::test]
async fn a_malformed_nonce_is_400_before_the_token() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&fx.job().await, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let all = calls(&result_of(&job, 10), &id);
    let rows_before = fx.sql("SELECT count(*) FROM job_objects").await;
    for (name, lines) in bad_nonce_lines() {
        for call in &all {
            for token in [None, Some(fx.token.as_str())] {
                let r = fx.call(call, token, &lines).await;
                problem(&r, 400, &format!("{} {name}", call.name));
                assert!(r.body.contains("Invalid Ngr-Nonce header"), "{}", r.body);
                assert_unproved(&r, name);
            }
        }
    }
    // Nothing was consumed: no row, and the token still works.
    assert_eq!(
        fx.sql("SELECT count(*) FROM job_objects").await,
        rows_before
    );
    assert_eq!(fx.status(&id).await.status, 200);
    fx.finish().await;
}

#[tokio::test]
async fn the_routes_own_refusals_come_before_the_nonce() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = v4();
    let (_, lines) = bad_nonce_lines().swap_remove(0);
    let json_type = Some("application/json");
    let shape = fx
        .send_with("POST", BEGIN_PATH, None, json_type, &lines, "{}")
        .await;
    problem(&shape, 400, "begin shape");
    assert!(
        shape.body.contains("not the expected JSON"),
        "{}",
        shape.body
    );
    let over = decl(&job, 1, "result", MAX_SIZE + 1, &digest());
    let r = fx
        .send_with("POST", BEGIN_PATH, None, json_type, &lines, &over)
        .await;
    problem(&r, 413, "begin length");
    let target = format!("{BEGIN_PATH}/{id}/parts/0");
    let r = fx.send_with("POST", &target, None, None, &lines, "").await;
    problem(&r, 400, "part number");
    assert!(r.body.contains("The part must be a number"), "{}", r.body);
    for (method, target) in [
        ("GET", format!("{BEGIN_PATH}/{id}")),
        ("POST", format!("{BEGIN_PATH}/{id}/complete")),
        ("POST", format!("{BEGIN_PATH}/{id}/parts/1")),
    ] {
        let r = fx.send_with(method, &target, None, None, &lines, "x").await;
        problem(&r, 400, &target);
        assert!(r.body.contains("The body must be empty"), "{}", r.body);
    }
    fx.finish().await;
}

/// A target that the transcript cannot take is a 400 before the token when a nonce is sent, and
/// the same request without a nonce is answered as before. A raw byte over 0x7F in a request
/// target is refused by this server's own parser or by the route's shape step; either is a 400.
#[tokio::test]
async fn a_target_the_transcript_cannot_take_is_400() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let id = fx
        .row(&fx.job().await, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let line = [nonce_line(&fresh())];
    let long_query = format!("?q={}", "a".repeat(2100));
    let all = calls(&result_of(&job, 10), &id);
    for call in &all {
        let long = Call {
            target: format!("{}{long_query}", call.target),
            body: call.body.clone(),
            ..*call
        };
        let high_query = Call {
            target: format!("{}?q=\u{e9}", call.target),
            body: call.body.clone(),
            ..*call
        };
        for bad in [&long, &high_query] {
            for token in [None, Some(fx.token.as_str())] {
                let r = fx.call(bad, token, &line).await;
                assert_eq!(r.status, 400, "{}: {}", call.name, r.head);
                assert_unproved(&r, call.name);
            }
            // D11: no nonce, no token: the route judges the token as before.
            let r = fx.call(bad, None, &[]).await;
            assert_eq!(r.status, 401, "{} without a nonce: {}", call.name, r.head);
        }
    }
    // A byte over 0x7F in the upload ID of the three routes that have one.
    for call in all.iter().skip(1) {
        let high = Call {
            target: call.target.replace(&id, "\u{e9}"),
            body: String::new(),
            ..*call
        };
        let r = fx.call(&high, Some(&fx.token), &line).await;
        assert_eq!(r.status, 400, "{}: {}", call.name, r.head);
        assert_unproved(&r, call.name);
        // Without a nonce it names no upload: the route's own 404.
        let r = fx.call(&high, Some(&fx.token), &[]).await;
        assert_eq!(r.status, 404, "{} without a nonce: {}", call.name, r.head);
    }
    fx.finish().await;
}

/// The request of route `name` among [`calls`].
fn call_of(name: &str, declaration: &str, id: &str) -> Call {
    calls(declaration, id)
        .into_iter()
        .find(|call| call.name == name)
        .expect("a route of that name")
}

#[tokio::test]
async fn a_begin_answer_is_proved() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let begin = call_of("begin", &result_of(&job, 10), "");
    let created = fx.proved(&begin, 201).await;
    assert_eq!(created.header("cache-control"), Some("no-store"));
    assert_eq!(upload_id(&created).len(), 36);
    // The same declaration again is the same row, proved to the new nonce.
    let again = fx.proved(&begin, 200).await;
    assert_eq!(upload_id(&again), upload_id(&created));
    fx.finish().await;
}

#[tokio::test]
async fn a_part_url_answer_is_proved() {
    let fx = Fx::new(With::Real).await;
    let id = begin_for(&fx, b"some bytes").await;
    let r = fx.proved(&call_of("part", "", &id), 200).await;
    assert!(
        json(&r)["url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
    fx.finish().await;
}

#[tokio::test]
async fn a_status_answer_is_proved() {
    let fx = Fx::new(With::Fake).await;
    for state in ["open", "completing", "finished", "failed"] {
        let id = fx
            .row(&fx.job().await, 1, "result", 10, state, PART_BYTES as i64)
            .await;
        let r = fx.proved(&call_of("status", "", &id), 200).await;
        assert_eq!(json(&r)["state"], state);
    }
    fx.finish().await;
}

#[tokio::test]
async fn a_completion_answer_is_proved() {
    let fx = Fx::waiting(With::Fake, Duration::from_millis(300)).await;
    let open = || async {
        fx.row(&fx.job().await, 1, "result", 10, "open", PART_BYTES as i64)
            .await
    };
    // 200: the object matches.
    let id = open().await;
    let r = fx.proved(&call_of("complete", "", &id), 200).await;
    assert_eq!(json(&r)["state"], "finished");
    // 409 with the row open again, and 409 with the row failed.
    for (fail, state) in [(Fail::MissingPart, "open"), (Fail::HashMismatch, "failed")] {
        let id = open().await;
        Wrap::set(&fx.wrap.complete_fail, Some(fail));
        let r = fx.proved(&call_of("complete", "", &id), 409).await;
        assert_eq!(json(&r)["state"], state);
    }
    Wrap::set(&fx.wrap.complete_fail, None);
    // 202: the task is still at the store when the wait ends.
    let id = open().await;
    let hold = Hold::new();
    *fx.wrap.complete_hold.lock().unwrap() = Some(Arc::clone(&hold));
    let r = fx.proved(&call_of("complete", "", &id), 202).await;
    assert_eq!(json(&r)["state"], "completing");
    hold.release();
    fx.until(&id, "finished").await;
    fx.finish().await;
}

/// Every answer after the token and the row are judged is proved, whatever its status.
#[tokio::test]
async fn every_answer_after_the_token_and_row_is_proved() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let declaration = result_of(&job, 10);
    let open = fx
        .row(&fx.job().await, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let finished = fx
        .row(
            &fx.job().await,
            1,
            "result",
            10,
            "finished",
            PART_BYTES as i64,
        )
        .await;
    let unparsable = fx
        .row(&fx.job().await, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    sqlx::query("UPDATE job_objects SET storage_key = repeat('x', 36) WHERE id = $1::uuid")
        .bind(&unparsable)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    let garbage = v4();
    fx.insert_job(&garbage, &fx.u.id, b"not an envelope").await;

    // Begin: no such job, a repeat with another declaration, an instruction that does not decode,
    // and the store failing.
    let begin = |declaration: &str| call_of("begin", declaration, "");
    fx.proved(&begin(&result_of(&v4(), 10)), 404).await;
    fx.proved(&begin(&declaration), 201).await;
    fx.proved(&begin(&result_of(&job, 11)), 409).await;
    fx.proved(&begin(&result_of(&garbage, 10)), 500).await;
    let other = fx.job().await;
    fx.wrap.fail_begin(Some(Fail::Io));
    fx.proved(&begin(&result_of(&other, 10)), 503).await;
    fx.wrap.fail_begin(None);

    // The part URL.
    let part = |id: &str| call_of("part", "", id);
    fx.proved(&part(&v4()), 404).await;
    fx.proved(&part(&finished), 409).await;
    fx.proved(&part(&unparsable), 500).await;
    fx.proved(&part(&open), 503).await;
    let beyond = Call {
        target: format!("{BEGIN_PATH}/{open}/parts/2"),
        ..part(&open)
    };
    fx.proved(&beyond, 400).await;

    // The status.
    let status = |id: &str| call_of("status", "", id);
    fx.proved(&status(&v4()), 404).await;
    Wrap::set(&fx.wrap.list_fail, Some(Fail::Io));
    fx.proved(&status(&open), 503).await;
    Wrap::set(&fx.wrap.list_fail, None);

    // The completion.
    let complete = |id: &str| call_of("complete", "", id);
    fx.proved(&complete(&v4()), 404).await;
    Wrap::set(&fx.wrap.complete_fail, Some(Fail::Io));
    fx.proved(&complete(&open), 503).await;
    Wrap::set(&fx.wrap.complete_fail, None);
    fx.finish().await;

    // No object store.
    let fx = Fx::new(With::Nothing).await;
    let open = fx
        .row(&fx.job().await, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    fx.proved(&call_of("part", "", &open), 503).await;
    fx.proved(&call_of("status", "", &open), 503).await;
    fx.proved(&call_of("complete", "", &open), 503).await;
    fx.proved(&call_of("begin", &result_of(&fx.job().await, 10), ""), 503)
        .await;
    fx.finish().await;
}

#[tokio::test]
async fn a_signing_failure_is_an_unproved_503() {
    let (logs, _guard) = capture();
    let (fx, fail) = Fx::flaky(With::Fake).await;
    let job = fx.job().await;
    let open = fx
        .row(&fx.job().await, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    let begin_call = |job: &str| call_of("begin", &result_of(job, 10), "");
    let begin = begin_call(&job);
    fail.store(true, Ordering::SeqCst);
    let count = format!("SELECT count(*) FROM job_objects WHERE job_id = '{job}'");
    for name in ["begin", "part", "status", "complete"] {
        let call = match name {
            "begin" => begin_call(&job),
            other => call_of(other, "", &open),
        };
        let (_, r) = fx.nonced(&call, &[]).await;
        problem(&r, 503, call.name);
        assert!(
            r.body.contains("The answer could not be proved"),
            "{}",
            r.body
        );
        assert_unproved(&r, call.name);
    }
    // The row begin made is there: the answer was lost, not the work.
    assert_eq!(fx.sql(&count).await, "1");
    fail.store(false, Ordering::SeqCst);
    fx.proved(&begin, 200).await;
    assert_eq!(fx.sql(&count).await, "1");
    // The completion ran although its answer could not be proved; the repeat is a proved 200.
    fx.proved(&call_of("complete", "", &open), 200).await;
    let text = logs.text();
    assert!(text.contains("could not be proved"), "{text}");
    assert!(
        !text.contains("/api/v1"),
        "the log names the target: {text}"
    );
    assert!(!text.contains(&open), "the log names the upload: {text}");
    fx.finish().await;
}

#[tokio::test]
async fn the_signed_target_is_the_bytes_the_agent_sends() {
    let fx = Fx::new(With::Fake).await;
    let job = fx.job().await;
    let declaration = result_of(&job, 10);
    let open = fx
        .row(&fx.job().await, 1, "result", 10, "open", PART_BYTES as i64)
        .await;
    // The first round finds the row open (the fake store has no URL to give: 503) and finishes it,
    // so the second finds it finished (409).
    for (suffix, begin_status, part_status) in [("", 201, 503), ("?a=1", 200, 409)] {
        for (name, status) in [
            ("begin", begin_status),
            ("part", part_status),
            ("status", 200),
            ("complete", 200),
        ] {
            let mut call = call_of(name, &declaration, &open);
            call.target.push_str(suffix);
            let (nonce, r) = fx.nonced(&call, &[]).await;
            fx.assert_proved_for(nonce, call.method, &call.target, &call.body, status, &r);
            // The proof is for these bytes only: the target without its query, or with another
            // one, does not verify it.
            if !suffix.is_empty() {
                let bare = call.target.trim_end_matches(suffix);
                let wrong = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    fx.assert_proved_for(nonce, call.method, bare, &call.body, status, &r);
                }));
                assert!(wrong.is_err(), "{name}: the proof ignores the query");
            }
        }
    }
    fx.finish().await;
}
