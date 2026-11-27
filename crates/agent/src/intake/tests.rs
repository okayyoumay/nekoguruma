//! The intake's cases (ADR-511): the order of the checks, the refusals and what each reports, the
//! wait for an unavailable server, the queue, the deadline, the stop and the outcome record's
//! recovery. A job that runs on a worker is the integration tests' of `j2534-0404-service`.
//!
//! Time is the paused tokio clock, which the test clock follows, so a minute's wait costs
//! nothing. The jobs end before a link opens (no worker is built for these tests), so each case
//! looks at the events the intake appended to the outbox, the files it left and what it asked of
//! its seams.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use diag_ir::Op;
use shared_crypto::{Signer, SigningKey, instruction as envelope};
use shared_proto::{AgentId, INSTRUCTION_VERSION, JobInstruction, VciId};
use tokio::time::Instant;

use super::*;
use crate::comm_log::CommLogStatus;
use crate::document::tests::{empty_program, vin_document};
use crate::identification::{NoPlans, PlanSourceError};
use crate::instruction;
use crate::launch::Api;
use crate::outbox::Outbox;
use crate::runner::JobError;
use crate::upload_ledger::UploadLedger;
use crate::vci_shell::{VciRoute, WorkerEnd};

const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
/// The device clock at the start of a test.
const NOW: u64 = 1_000_000;

fn job_id(n: u32) -> String {
    format!("0194f0a2-7c3e-7d11-8a52-{n:012x}")
}

/// A clock that follows the paused tokio clock.
struct TokioClock {
    start: Instant,
}

impl Clock for TokioClock {
    fn now_unix(&self) -> u64 {
        NOW + self.start.elapsed().as_secs()
    }
}

/// A clock that moves with real time from `NOW`, for a test whose blocking lock wait runs in real
/// time.
struct RealClock {
    start: std::time::Instant,
}

impl Clock for RealClock {
    fn now_unix(&self) -> u64 {
        NOW + self.start.elapsed().as_secs()
    }
}

fn program(code: Vec<Op>) -> diag_ir::Program {
    let mut program = empty_program();
    program.code = code;
    program
}

/// A document whose program sends nothing.
fn reading_document() -> Vec<u8> {
    vin_document(Some(&program(vec![Op::PushI64(1)])))
}

/// A document whose program sends WriteDataByIdentifier.
fn writing_document() -> Vec<u8> {
    vin_document(Some(&program(vec![Op::ServiceRequest { service: 0x2E }])))
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    JobDocument::new(bytes.to_vec())
        .expect("the test document loads")
        .sha256()
}

/// What an instruction of a test says.
struct Spec {
    job: u32,
    generation: u64,
    kind: JobKind,
    documents: Vec<[u8; 32]>,
    plan: Option<[u8; 32]>,
    vci: &'static str,
    /// Seconds after the start of the test.
    deadline_in: u64,
}

impl Spec {
    fn new(job: u32, kind: JobKind, documents: &[&[u8]]) -> Spec {
        let mut hashes: Vec<[u8; 32]> = documents.iter().map(|bytes| digest(bytes)).collect();
        hashes.sort_unstable();
        Spec {
            job,
            generation: 1,
            kind,
            documents: hashes,
            plan: None,
            vci: "vci-a",
            deadline_in: 600,
        }
    }

    fn sign(&self) -> VerifiedInstruction {
        let key = SigningKey::generate().unwrap();
        let body = JobInstruction {
            version: INSTRUCTION_VERSION,
            job_id: JobId(job_id(self.job)),
            ownership_generation: self.generation,
            agent_id: AgentId(AGENT_ID.to_owned()),
            operator_id: "operator".to_owned(),
            kind: self.kind,
            vci: VciId(self.vci.to_owned()),
            vin: None,
            document_hashes: self.documents.clone(),
            artifact_hash: None,
            plan_hash: self.plan,
            issued_at: 1,
            start_deadline: NOW + self.deadline_in,
        };
        let blob = envelope::seal(&key, &body.encode().unwrap()).unwrap();
        instruction::open(&blob, &key.public_key(), AGENT_ID).unwrap()
    }
}

/// How a scripted `fetch_all` answers.
#[derive(Clone, Copy)]
enum Answer {
    Ok,
    /// The server does not answer.
    Retry,
    /// A refusal the server proved.
    Refused,
    /// The agent's own fault.
    Local,
    /// Two downloads in a row did not match.
    MismatchTwice,
    /// Waits until the test calls [`Objects::release`], then answers.
    Held,
}

/// The object source of a test: scripted answers, the documents it can read, and what was asked.
struct FakeObjects {
    /// The answers of the next calls; an empty script answers `Ok`.
    script: StdMutex<VecDeque<Answer>>,
    /// The answer once the script is empty.
    fallback: StdMutex<Answer>,
    documents: StdMutex<HashMap<[u8; 32], Vec<u8>>>,
    /// Another document's bytes, given for every read, as a source that lies would.
    lie: StdMutex<Option<Vec<u8>>>,
    /// Cached documents the instruction does not name, which `fetch_all` lists among its objects.
    unrelated: StdMutex<Vec<PathBuf>>,
    /// The job ID of each `fetch_all`, in order.
    fetched: StdMutex<Vec<String>>,
    /// The digests asked of `read_document`.
    read: StdMutex<Vec<[u8; 32]>>,
    gate: tokio::sync::Semaphore,
}

impl FakeObjects {
    fn new() -> FakeObjects {
        FakeObjects {
            script: StdMutex::default(),
            fallback: StdMutex::new(Answer::Ok),
            documents: StdMutex::default(),
            lie: StdMutex::default(),
            unrelated: StdMutex::default(),
            fetched: StdMutex::default(),
            read: StdMutex::default(),
            gate: tokio::sync::Semaphore::new(0),
        }
    }

    fn has(&self, bytes: &[u8]) {
        self.documents
            .lock()
            .unwrap()
            .insert(digest(bytes), bytes.to_vec());
    }

    fn then(&self, answer: Answer) {
        self.script.lock().unwrap().push_back(answer);
    }

    fn always(&self, answer: Answer) {
        *self.fallback.lock().unwrap() = answer;
    }

    fn release(&self) {
        self.gate.add_permits(1);
    }

    fn fetches(&self) -> Vec<String> {
        self.fetched.lock().unwrap().clone()
    }
}

impl Objects for FakeObjects {
    fn fetch_all<'a>(
        &'a self,
        instruction: &'a VerifiedInstruction,
        _attempts: u32,
    ) -> ObjectFuture<'a, Vec<Fetched>> {
        self.fetched
            .lock()
            .unwrap()
            .push(instruction.instruction().job_id.0.clone());
        let answer = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(*self.fallback.lock().unwrap());
        Box::pin(async move {
            if let Answer::Held = answer {
                self.gate.acquire().await.unwrap().forget();
            }
            match answer {
                Answer::Ok | Answer::Held => {
                    let mut objects: Vec<Fetched> = instruction
                        .instruction()
                        .document_hashes
                        .iter()
                        .map(|_| Fetched {
                            kind: ObjectKind::Document,
                            path: PathBuf::from("named"),
                            length: 1,
                        })
                        .collect();
                    objects.extend(self.unrelated.lock().unwrap().iter().map(|path| Fetched {
                        kind: ObjectKind::Document,
                        path: path.clone(),
                        length: 1,
                    }));
                    Ok(objects)
                }
                Answer::Retry => Err(FetchError::Retry {
                    why: "scripted",
                    status: None,
                    retry_after_secs: None,
                }),
                Answer::Refused => Err(FetchError::Refused {
                    why: "scripted",
                    status: Some(404),
                }),
                Answer::Local => Err(FetchError::Local { why: "scripted" }),
                Answer::MismatchTwice => Err(FetchError::MismatchTwice),
            }
        })
    }

    fn read_document<'a>(
        &'a self,
        _instruction: &'a VerifiedInstruction,
        digest: &'a [u8; 32],
        _attempts: u32,
    ) -> ObjectFuture<'a, Vec<u8>> {
        self.read.lock().unwrap().push(*digest);
        let bytes = self
            .lie
            .lock()
            .unwrap()
            .clone()
            .or_else(|| self.documents.lock().unwrap().get(digest).cloned());
        Box::pin(std::future::ready(
            bytes.ok_or(FetchError::Local { why: "not cached" }),
        ))
    }
}

/// The VCIs of a test: one name is found, its workers directory holds no worker, so a job that
/// gets this far ends in a launch that fails.
struct FakeVcis {
    dir: PathBuf,
    found: AtomicUsize,
}

impl VciLookup for FakeVcis {
    fn find(&self, vci: &str) -> Result<VciRoute, VciShellError> {
        self.found.fetch_add(1, Ordering::SeqCst);
        if vci == "vci-unknown" {
            return Err(VciShellError::UnknownVci {
                vci: vci.to_owned(),
            });
        }
        Ok(VciRoute {
            api: Api::J2534,
            workers: self.dir.join("no-workers"),
            locks: self.dir.join("locks"),
        })
    }
}

/// A plan source that holds one plan under the hash given and answers every other hash with the
/// bytes it has, as a source that cannot be trusted does.
struct FakePlans(Vec<u8>);

impl PlanSource for FakePlans {
    fn plan(&self, _plan_hash: &[u8; 32]) -> Result<Vec<u8>, PlanSourceError> {
        Ok(self.0.clone())
    }
}

struct Rig {
    dir: tempfile::TempDir,
    objects: Arc<FakeObjects>,
    vcis: Arc<FakeVcis>,
    outbox: SharedOutbox,
    uploads: SharedLedger,
    marks: IssueMark,
    clock: Arc<dyn Clock>,
    outcomes: OutcomeRecord,
    intake: Arc<JobIntake>,
    handle: Handle,
    /// How many ends the outcome record held unreported at each write the outbox made.
    owed_at_append: Arc<StdMutex<Vec<usize>>>,
}

impl Rig {
    fn new() -> Rig {
        Rig::with(
            Arc::new(NoPlans),
            Arc::new(TokioClock {
                start: Instant::now(),
            }),
        )
    }

    /// A rig on the device's real clock, for a test whose blocking lock wait runs in real time.
    fn new_real_clock() -> Rig {
        Rig::with(
            Arc::new(NoPlans),
            Arc::new(RealClock {
                start: std::time::Instant::now(),
            }),
        )
    }

    fn with(plans: Arc<dyn PlanSource + Send + Sync>, clock: Arc<dyn Clock>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let registration = dir.path().join("registration");
        std::fs::create_dir_all(&registration).unwrap();
        let outcomes = OutcomeRecord::open_in(&registration, AGENT_ID).unwrap();
        let outbox_dir = dir.path().join("outbox");
        std::fs::create_dir_all(&outbox_dir).unwrap();
        // The outbox looks at the record each time it is about to write an event.
        let owed_at_append = Arc::new(StdMutex::new(Vec::new()));
        let probe: crate::outbox::Probe = {
            let (outcomes, seen) = (outcomes.clone(), owed_at_append.clone());
            Box::new(move |step| {
                if step == crate::outbox::Step::BeforeWrite {
                    seen.lock().unwrap().push(outcomes.owed().len());
                }
                Ok(())
            })
        };
        let outbox = SharedOutbox::new(Outbox::open_with(&outbox_dir, AGENT_ID, probe).unwrap());
        let uploads_dir = dir.path().join("uploads");
        std::fs::create_dir_all(&uploads_dir).unwrap();
        let uploads = SharedLedger::new(UploadLedger::open_in(&uploads_dir, AGENT_ID).unwrap());
        let marks = IssueMark::open_in(&registration, AGENT_ID).unwrap();
        let objects = Arc::new(FakeObjects::new());
        let vcis = Arc::new(FakeVcis {
            dir: dir.path().to_owned(),
            found: AtomicUsize::new(0),
        });
        let journal = dir.path().join("journal");
        let (intake, handle) = start(Parts {
            objects: objects.clone(),
            plans,
            vcis: vcis.clone(),
            mode: OperatingMode::User,
            journal_dir: Arc::new(move || Ok(journal.clone())),
            outbox: outbox.clone(),
            uploads: uploads.clone(),
            marks: marks.clone(),
            clock: clock.clone(),
            outcomes: outcomes.clone(),
        });
        Rig {
            dir,
            objects,
            vcis,
            outbox,
            uploads,
            marks,
            clock,
            outcomes,
            intake,
            handle,
            owed_at_append,
        }
    }

    /// A worker over the rig's stores, which the test drives itself, and its stop.
    fn worker(&self) -> (Worker, watch::Sender<bool>) {
        let (stop_tx, stop_rx) = watch::channel(false);
        let worker = Worker {
            parts: Parts {
                objects: self.objects.clone(),
                plans: Arc::new(NoPlans),
                vcis: self.vcis.clone(),
                mode: OperatingMode::User,
                journal_dir: Arc::new(|| Ok(PathBuf::from("journal"))),
                outbox: self.outbox.clone(),
                uploads: self.uploads.clone(),
                marks: self.marks.clone(),
                clock: self.clock.clone(),
                outcomes: self.outcomes.clone(),
            },
            queue: Arc::new(Queue::default()),
            stop: Stop(stop_rx),
            appended: StdMutex::default(),
            warned: StdMutex::default(),
            in_memory: StdMutex::default(),
        };
        (worker, stop_tx)
    }

    /// What the channel does for a job: the intake takes it, then hears that `received` is in.
    async fn deliver(&self, spec: &Spec) {
        let verified = spec.sign();
        self.intake.take(verified).await.unwrap();
        self.intake.received(&job_id(spec.job), spec.generation);
    }

    /// The job's events so far as (state, failure code).
    async fn events(&self, job: u32) -> Vec<(String, Option<String>)> {
        let wanted = job_id(job);
        self.outbox
            .texts_for_test()
            .await
            .iter()
            .filter_map(|text| {
                let value: serde_json::Value = serde_json::from_str(text).unwrap();
                let event = &value;
                (event["jobId"] == wanted.as_str()).then(|| {
                    (
                        event["state"].as_str().unwrap().to_owned(),
                        event["failureCode"].as_str().map(str::to_owned),
                    )
                })
            })
            .collect()
    }

    /// Waits until the job has a terminal event and returns it.
    async fn ended(&self, job: u32) -> (String, Option<String>) {
        for _ in 0..20_000 {
            if let Some(event) = self.events(job).await.pop()
                && event.0 != "running"
            {
                return event;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "job {job} did not end; events: {:?}",
            self.events(job).await
        );
    }

    fn locks_taken(&self) -> bool {
        std::fs::read_dir(self.dir.path().join("locks")).is_ok_and(|mut dir| dir.next().is_some())
    }
}

fn state(name: &str, code: Option<&str>) -> (String, Option<String>) {
    (name.to_owned(), code.map(str::to_owned))
}

// ---------------------------------------------------------------- the shape, before any fetch

/// A kind that is not identification names exactly one document: none, or two, is refused before
/// anything is fetched.
#[tokio::test(start_paused = true)]
async fn a_program_instruction_naming_other_than_one_document_is_refused_before_any_fetch() {
    let rig = Rig::new();
    let read = reading_document();
    let other = vin_document(Some(&program(vec![Op::PushI64(2)])));
    let mut none = Spec::new(1, JobKind::Acquire, &[]);
    none.documents.clear();
    rig.deliver(&none).await;
    rig.deliver(&Spec::new(2, JobKind::Acquire, &[&read, &other]))
        .await;
    for job in [1, 2] {
        assert_eq!(
            rig.ended(job).await,
            state("failed", Some("instructionRejected"))
        );
    }
    assert!(rig.objects.fetches().is_empty());
    assert_eq!(rig.vcis.found.load(Ordering::SeqCst), 0);
}

/// Without a plan hash, or with a plan source that has no plan (the production agent's), an
/// identification instruction is refused before anything is fetched.
#[tokio::test(start_paused = true)]
async fn an_identification_instruction_without_a_plan_is_refused_before_any_fetch() {
    let rig = Rig::new();
    let read = reading_document();
    let mut spec = Spec::new(1, JobKind::Identify, &[&read]);
    rig.deliver(&spec).await;
    spec.job = 2;
    spec.plan = Some([7; 32]);
    rig.deliver(&spec).await;
    for job in [1, 2] {
        assert_eq!(
            rig.ended(job).await,
            state("failed", Some("instructionRejected"))
        );
    }
    assert!(rig.objects.fetches().is_empty());
}

/// A plan whose SHA-256 is not the instruction's plan hash is refused before the VCI is looked
/// up, so no lock is taken and no worker launched.
#[tokio::test(start_paused = true)]
async fn a_plan_that_hashes_differently_from_its_plan_hash_is_refused_before_any_link() {
    let read = reading_document();
    let rig = Rig::with(
        Arc::new(FakePlans(b"not the plan the hash names".to_vec())),
        Arc::new(TokioClock {
            start: Instant::now(),
        }),
    );
    rig.objects.has(&read);
    let mut spec = Spec::new(1, JobKind::Identify, &[&read]);
    spec.plan = Some([9; 32]);
    rig.deliver(&spec).await;
    assert_eq!(
        rig.ended(1).await,
        state("failed", Some("instructionRejected"))
    );
    assert_eq!(rig.vcis.found.load(Ordering::SeqCst), 0);
    assert!(!rig.locks_taken());
}

// ---------------------------------------------------------------- the fetch

/// An object the server refused with a proof ends the job at once, and nothing opens.
#[tokio::test(start_paused = true)]
async fn a_refused_object_ends_the_job_without_a_run() {
    let rig = Rig::new();
    let read = reading_document();
    rig.objects.has(&read);
    rig.objects.then(Answer::Refused);
    rig.deliver(&Spec::new(1, JobKind::Acquire, &[&read])).await;
    assert_eq!(
        rig.ended(1).await,
        state("failed", Some("instructionRejected"))
    );
    assert_eq!(rig.objects.fetches().len(), 1);
    assert_eq!(rig.vcis.found.load(Ordering::SeqCst), 0);
}

/// A fetch that runs out of attempts is the server unavailable: the intake waits at most a minute
/// at a time and fetches again, until the start deadline passes, and the job then ends `expired`
/// and never starts.
#[tokio::test(start_paused = true)]
async fn an_unavailable_server_is_waited_for_until_the_start_deadline() {
    for answer in [Answer::Retry, Answer::MismatchTwice] {
        let rig = Rig::new();
        let read = reading_document();
        rig.objects.has(&read);
        rig.objects.always(answer);
        let mut spec = Spec::new(1, JobKind::Acquire, &[&read]);
        spec.deadline_in = 200;
        let begun = Instant::now();
        rig.deliver(&spec).await;
        assert_eq!(rig.ended(1).await, state("expired", None));
        let waited = begun.elapsed();
        assert!(
            waited >= Duration::from_secs(190) && waited < Duration::from_secs(260),
            "{waited:?}"
        );
        // Rounds a minute apart at most: more than two, fewer than the polls of a busy loop.
        let rounds = rig.objects.fetches().len();
        assert!((3..=5).contains(&rounds), "{rounds} rounds");
        assert_eq!(rig.vcis.found.load(Ordering::SeqCst), 0);
        assert_eq!(rig.events(1).await, vec![state("expired", None)]);
    }
}

/// A server that answers on a later round gives the job its objects: it then goes on.
#[tokio::test(start_paused = true)]
async fn a_server_that_comes_back_before_the_deadline_lets_the_job_go_on() {
    let rig = Rig::new();
    let read = reading_document();
    rig.objects.has(&read);
    rig.objects.then(Answer::Retry);
    rig.objects.then(Answer::Retry);
    rig.deliver(&Spec::new(1, JobKind::Acquire, &[&read])).await;
    // The job reaches the VCI lookup, whose worker is not there.
    assert_eq!(
        rig.ended(1).await,
        state("preconditionFailed", Some("linkNotOpened"))
    );
    assert_eq!(rig.objects.fetches().len(), 3);
}

/// The agent's own fault in the cache is not the server's unavailability: it ends the job at once.
#[tokio::test(start_paused = true)]
async fn a_local_fetch_failure_is_an_internal_precondition_failure() {
    let rig = Rig::new();
    let read = reading_document();
    rig.objects.then(Answer::Local);
    rig.deliver(&Spec::new(1, JobKind::Acquire, &[&read])).await;
    assert_eq!(
        rig.ended(1).await,
        state("preconditionFailed", Some("internal"))
    );
    assert_eq!(rig.objects.fetches().len(), 1);
}

// ---------------------------------------------------------------- the documents held

/// A document the cache holds and the instruction does not name is not used, however it is
/// listed with the fetched objects: the job takes its program from the named document only.
#[tokio::test(start_paused = true)]
async fn an_unrelated_cached_document_is_not_used() {
    let rig = Rig::new();
    let named = reading_document();
    let unrelated = writing_document();
    rig.objects.has(&named);
    rig.objects.has(&unrelated);
    rig.objects
        .unrelated
        .lock()
        .unwrap()
        .push(PathBuf::from("other"));
    // Were the unrelated document taken for the job, it would be refused as a write.
    rig.deliver(&Spec::new(1, JobKind::Acquire, &[&named]))
        .await;
    assert_eq!(
        rig.ended(1).await,
        state("preconditionFailed", Some("linkNotOpened"))
    );
    assert_eq!(*rig.objects.read.lock().unwrap(), vec![digest(&named)]);
}

/// Bytes that are not the document the instruction names, though the source says they are, are
/// not run: the held set is not the named set.
#[tokio::test(start_paused = true)]
async fn documents_held_that_are_not_the_named_set_end_the_job_as_internal() {
    let rig = Rig::new();
    let named = reading_document();
    *rig.objects.lie.lock().unwrap() = Some(vin_document(Some(&program(vec![Op::PushI64(5)]))));
    rig.deliver(&Spec::new(1, JobKind::Acquire, &[&named]))
        .await;
    assert_eq!(
        rig.ended(1).await,
        state("preconditionFailed", Some("internal"))
    );
    assert_eq!(rig.vcis.found.load(Ordering::SeqCst), 0);
}

/// A document that does not load is a refusal of the job's data, before any link.
#[tokio::test(start_paused = true)]
async fn a_document_that_does_not_load_is_job_refused() {
    let rig = Rig::new();
    let named = reading_document();
    *rig.objects.lie.lock().unwrap() = Some(b"not a document".to_vec());
    rig.deliver(&Spec::new(1, JobKind::Acquire, &[&named]))
        .await;
    assert_eq!(
        rig.ended(1).await,
        state("preconditionFailed", Some("jobRefused"))
    );
    assert_eq!(rig.vcis.found.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------- the kind against the document

/// A job of a kind that only reads, whose document writes, is a rejected instruction before the
/// VCI is looked up; the same document in a read-only job's kind passes the check.
#[tokio::test(start_paused = true)]
async fn a_read_only_kind_naming_a_writing_document_is_refused() {
    let rig = Rig::new();
    let writing = writing_document();
    rig.objects.has(&writing);
    for (job, kind) in [
        (1, JobKind::Acquire),
        (2, JobKind::MonitorSession),
        (3, JobKind::MonitorCapture),
    ] {
        rig.deliver(&Spec::new(job, kind, &[&writing])).await;
        assert_eq!(
            rig.ended(job).await,
            state("failed", Some("instructionRejected")),
            "{kind:?}"
        );
    }
    assert_eq!(rig.vcis.found.load(Ordering::SeqCst), 0);
}

/// A read-only program in a read-only kind is not refused for writing: it reaches the VCI.
#[tokio::test(start_paused = true)]
async fn a_read_only_document_in_a_read_only_kind_reaches_the_vci() {
    let rig = Rig::new();
    let reading = reading_document();
    rig.objects.has(&reading);
    rig.deliver(&Spec::new(1, JobKind::Acquire, &[&reading]))
        .await;
    rig.ended(1).await;
    assert_eq!(rig.vcis.found.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------- the VCI

/// A VCI no definition names, and a worker that does not launch, are `linkNotOpened`; the locks
/// the shell took are released again.
#[tokio::test(start_paused = true)]
async fn a_vci_that_is_unknown_or_whose_worker_does_not_launch_is_link_not_opened() {
    let rig = Rig::new();
    let reading = reading_document();
    rig.objects.has(&reading);
    let mut unknown = Spec::new(1, JobKind::Acquire, &[&reading]);
    unknown.vci = "vci-unknown";
    rig.deliver(&unknown).await;
    rig.deliver(&Spec::new(2, JobKind::Acquire, &[&reading]))
        .await;
    for job in [1, 2] {
        assert_eq!(
            rig.ended(job).await,
            state("preconditionFailed", Some("linkNotOpened"))
        );
    }
    assert_eq!(rig.vcis.found.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------- the wait for the locks

/// Another job holds the VCI's lock (taken here, as a run of another process would hold it).
fn hold_vci(rig: &Rig) -> crate::guards::JobGuards {
    crate::guards::JobGuards::take_vci_only(
        &crate::guards::GuardSetup {
            dir: rig.dir.path().join("locks"),
            vci: "vci-a".to_owned(),
        },
        Duration::from_millis(10),
        &AtomicBool::new(false),
    )
    .expect("the test takes the VCI's lock")
}

/// A job that waits for the VCI's lock when its start deadline passes ends `expired`: the wait is
/// cancelled and no worker is launched (the workers directory does not exist, which a launch
/// would have reported as `linkNotOpened`).
#[tokio::test(flavor = "multi_thread")]
async fn the_start_deadline_ends_the_wait_for_the_locks_as_expired() {
    let rig = Rig::new_real_clock();
    let reading = reading_document();
    rig.objects.has(&reading);
    let held = hold_vci(&rig);
    let mut spec = Spec::new(1, JobKind::Acquire, &[&reading]);
    spec.deadline_in = 3;
    rig.deliver(&spec).await;
    assert_eq!(rig.ended(1).await, state("expired", None));
    assert_eq!(rig.events(1).await, vec![state("expired", None)]);
    drop(held);
}

/// A stop while a job waits for the VCI's lock cancels the wait: the task ends within a bound,
/// nothing terminal is reported, and the lock is free for the next job.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_ends_the_wait_for_the_locks_and_reports_nothing() {
    let rig = Rig::new_real_clock();
    let reading = reading_document();
    rig.objects.has(&reading);
    let held = hold_vci(&rig);
    rig.deliver(&Spec::new(1, JobKind::Acquire, &[&reading]))
        .await;
    // The job is at the lock when the fetch it made is over.
    while rig.objects.fetches().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.handle.stop();
    tokio::time::timeout(Duration::from_secs(10), rig.handle.stopped())
        .await
        .expect("the intake ends within the bound");
    assert!(rig.events(1).await.is_empty());
    assert!(rig.outcomes.owed().is_empty());
    drop(held);
    drop(hold_vci(&rig));
}

// ---------------------------------------------------------------- the queue

/// Jobs are served one at a time, in the order they were taken: the second is not fetched while
/// the first is in progress, and a job whose `received` is not yet appended is not served at all.
#[tokio::test(start_paused = true)]
async fn jobs_are_served_one_at_a_time_in_the_order_they_were_taken() {
    let rig = Rig::new();
    let reading = reading_document();
    rig.objects.has(&reading);
    rig.objects.then(Answer::Held);
    let (a, b, c) = (
        Spec::new(1, JobKind::Acquire, &[&reading]),
        Spec::new(2, JobKind::Acquire, &[&reading]),
        Spec::new(3, JobKind::Acquire, &[&reading]),
    );
    rig.deliver(&a).await;
    rig.deliver(&b).await;
    // The channel has not yet appended the `received` event of the third.
    rig.intake.take(c.sign()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        rig.objects.fetches(),
        vec![job_id(1)],
        "only the first is served"
    );
    rig.objects.release();
    rig.ended(1).await;
    rig.ended(2).await;
    assert_eq!(rig.objects.fetches(), vec![job_id(1), job_id(2)]);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        rig.objects.fetches().len(),
        2,
        "the third waits for its `received`"
    );
    rig.intake.received(&job_id(3), 1);
    rig.ended(3).await;
    assert_eq!(rig.objects.fetches(), vec![job_id(1), job_id(2), job_id(3)]);
}

/// A job whose start deadline passes while it waits behind another ends `expired`, without a
/// fetch, and the one behind it is served.
#[tokio::test(start_paused = true)]
async fn a_job_whose_deadline_passes_while_queued_ends_expired() {
    let rig = Rig::new();
    let reading = reading_document();
    rig.objects.has(&reading);
    rig.objects.then(Answer::Held);
    let first = Spec::new(1, JobKind::Acquire, &[&reading]);
    let mut late = Spec::new(2, JobKind::Acquire, &[&reading]);
    late.deadline_in = 30;
    let last = Spec::new(3, JobKind::Acquire, &[&reading]);
    rig.deliver(&first).await;
    rig.deliver(&late).await;
    rig.deliver(&last).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    rig.objects.release();
    assert_eq!(rig.ended(2).await, state("expired", None));
    rig.ended(3).await;
    assert_eq!(rig.objects.fetches(), vec![job_id(1), job_id(3)]);
}

// ---------------------------------------------------------------- the stop

/// A stop while a fetch is in flight ends the task within a bound, reports nothing terminal and
/// records no end; what was taken afterwards is refused.
#[tokio::test(start_paused = true)]
async fn a_stop_leaves_the_job_open_and_closes_the_intake() {
    let rig = Rig::new();
    let reading = reading_document();
    rig.objects.has(&reading);
    rig.objects.always(Answer::Held);
    rig.deliver(&Spec::new(1, JobKind::Acquire, &[&reading]))
        .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(rig.objects.fetches().len(), 1);
    rig.handle.stop();
    tokio::time::timeout(Duration::from_secs(10), rig.handle.stopped())
        .await
        .expect("the intake ends within the bound");
    assert!(
        rig.events(1).await.is_empty(),
        "nothing terminal while the agent stops"
    );
    assert!(rig.outcomes.owed().is_empty());
    assert!(
        rig.intake
            .take(Spec::new(2, JobKind::Acquire, &[&reading]).sign())
            .await
            .is_err(),
        "a take after the task ended is an error"
    );
}

// ---------------------------------------------------------------- the end, and its record

/// An end that is recorded and reported is recorded as both; its `reported` follows the append.
#[tokio::test(start_paused = true)]
async fn an_end_is_recorded_before_it_is_reported_and_marked_reported_after() {
    let rig = Rig::new();
    let reading = reading_document();
    rig.objects.then(Answer::Refused);
    let spec = Spec::new(1, JobKind::Acquire, &[&reading]);
    rig.deliver(&spec).await;
    rig.ended(1).await;
    // The `reported` record is written after the append the test just saw.
    let key = job_key(&spec.sign());
    for _ in 0..100 {
        if rig
            .outcomes
            .standing(&key)
            .is_some_and(|(_, reported)| reported)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (ended, reported) = rig.outcomes.standing(&key).unwrap();
    assert!(reported);
    assert!(matches!(
        ended,
        JobState::Failed {
            code: FailureCode::InstructionRejected,
            ..
        }
    ));
    assert!(rig.outcomes.owed().is_empty());
}

/// A crash between the `ended` record and the append leaves the end owed: the next intake appends
/// it once and marks it reported, and a later start appends nothing more.
#[tokio::test(start_paused = true)]
async fn an_end_that_was_recorded_and_not_reported_is_appended_at_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let registration = dir.path().join("registration");
    std::fs::create_dir_all(&registration).unwrap();
    let key = JobKey {
        job_id: JobId(job_id(7)),
        generation: 2,
    };
    {
        let record = OutcomeRecord::open_in(&registration, AGENT_ID).unwrap();
        record.ended(&key, &JobState::Expired).unwrap();
    }
    for start_number in 0..2 {
        let outbox_dir = dir.path().join("outbox");
        std::fs::create_dir_all(&outbox_dir).unwrap();
        let outbox = SharedOutbox::new(Outbox::open_in(&outbox_dir, AGENT_ID).unwrap());
        let uploads_dir = dir.path().join("uploads");
        std::fs::create_dir_all(&uploads_dir).unwrap();
        let outcomes = OutcomeRecord::open_in(&registration, AGENT_ID).unwrap();
        let (_intake, handle) = start(Parts {
            objects: Arc::new(FakeObjects::new()),
            plans: Arc::new(NoPlans),
            vcis: Arc::new(FakeVcis {
                dir: dir.path().to_owned(),
                found: AtomicUsize::new(0),
            }),
            mode: OperatingMode::User,
            journal_dir: Arc::new(|| Ok(PathBuf::from("journal"))),
            outbox: outbox.clone(),
            uploads: SharedLedger::new(UploadLedger::open_in(&uploads_dir, AGENT_ID).unwrap()),
            marks: IssueMark::open_in(&registration, AGENT_ID).unwrap(),
            clock: Arc::new(TokioClock {
                start: Instant::now(),
            }),
            outcomes: outcomes.clone(),
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        let texts = outbox.texts_for_test().await;
        let expired = texts.iter().filter(|text| text.contains("expired")).count();
        assert_eq!(expired, 1, "start {start_number}: {texts:?}");
        assert!(outcomes.owed().is_empty());
        handle.stop();
        handle.stopped().await;
        drop((outbox, outcomes));
        // Let the stores release their locks before the next start.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ---------------------------------------------------------------- the run's end, watched

/// What a fake run ends with: an `Ok` result of a run that completed.
fn finished() -> Result<ShellRun<()>, VciShellError> {
    Ok(ShellRun {
        result: Ok(()),
        comm_log: CommLogStatus::None,
        worker_end: WorkerEnd::Exited,
    })
}

/// What a test hands `supervise` besides the run: its flags, its first-step channel and the
/// deadline `in_secs` from the start of the test.
fn watch_of(
    rig: &Rig,
    in_secs: u64,
) -> (
    Watched,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    mpsc::UnboundedSender<()>,
) {
    let (lock_cancel, job_cancel) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let (first_step, started) = mpsc::unbounded_channel();
    (
        Watched {
            lock_cancel: lock_cancel.clone(),
            job_cancel: job_cancel.clone(),
            started,
            deadline: rig.clock.now_unix() + in_secs,
        },
        lock_cancel,
        job_cancel,
        first_step,
    )
}

async fn supervise_unit(
    worker: &Worker,
    key: &JobKey,
    watched: Watched,
    run: impl Future<Output = Result<ShellRun<()>, VciShellError>> + Send + 'static,
) -> Ending {
    worker
        .supervise(
            key,
            run,
            watched,
            || JournalStanding::Clean,
            |(): &()| Ok("{\"final\":true}".to_owned()),
        )
        .await
}

/// `running` is appended when the run's first step begins and the terminal event after it, in that
/// order; the result is enqueued before the end.
#[tokio::test(start_paused = true)]
async fn running_is_reported_before_the_terminal_event() {
    let rig = Rig::new();
    let (worker, _stop) = rig.worker();
    let key = JobKey {
        job_id: JobId(job_id(1)),
        generation: 1,
    };
    let (watched, _lock, _job, first_step) = watch_of(&rig, 600);
    let run = async move {
        first_step.send(()).unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        finished()
    };
    let ending = supervise_unit(&worker, &key, watched, run).await;
    assert_eq!(rig.events(1).await, vec![state("running", None)]);
    worker.finish(&key, ending).await;
    assert_eq!(
        rig.events(1).await,
        vec![state("running", None), state("completed", None)]
    );
    let entries = rig.uploads.entries_for_test().await;
    assert_eq!(
        entries
            .iter()
            .map(|entry| (entry.key.clone(), entry.kind))
            .collect::<Vec<_>>(),
        vec![(key, crate::upload_ledger::Kind::Result)]
    );
}

/// A stop does not drop the run: it cancels the job and the run is awaited, so a run that ends
/// `Ok` after the stop was requested (while its worker stops) is reported `completed`, recorded,
/// and its result enqueued; a run that ends any other way is reported as nothing terminal.
#[tokio::test(start_paused = true)]
async fn a_run_that_ends_ok_after_a_stop_is_completed_recorded_and_its_result_enqueued() {
    let rig = Rig::new();
    let (worker, stop) = rig.worker();
    let key = JobKey {
        job_id: JobId(job_id(1)),
        generation: 1,
    };
    let (watched, _lock, job_cancel, first_step) = watch_of(&rig, 600);
    let seen = job_cancel.clone();
    let run = async move {
        first_step.send(()).unwrap();
        while !seen.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The worker stops, which takes a moment, and the run ends `Ok` all the same.
        tokio::time::sleep(Duration::from_secs(3)).await;
        finished()
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        stop.send_replace(true);
    });
    let ending = supervise_unit(&worker, &key, watched, run).await;
    assert!(
        job_cancel.load(Ordering::SeqCst),
        "the stop cancelled the job"
    );
    assert!(matches!(
        ending,
        Ending::Ended {
            state: JobState::Completed,
            result: Some(_)
        }
    ));
    worker.finish(&key, ending).await;
    assert_eq!(
        rig.events(1).await,
        vec![state("running", None), state("completed", None)]
    );
    assert_eq!(
        rig.outcomes.standing(&key),
        Some((JobState::Completed, true))
    );
    assert_eq!(rig.uploads.entries_for_test().await.len(), 1);
}

/// A run that ends in an error after the stop reports nothing terminal, and one that does not
/// end within the bound is dropped and left open.
#[tokio::test(start_paused = true)]
async fn a_stop_leaves_a_run_that_fails_or_does_not_end_open() {
    let rig = Rig::new();
    let (worker, stop) = rig.worker();
    let key = JobKey {
        job_id: JobId(job_id(1)),
        generation: 1,
    };
    stop.send_replace(true);
    let (watched, ..) = watch_of(&rig, 600);
    let run = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok(ShellRun {
            result: Err(JobError::Cancelled),
            comm_log: CommLogStatus::None,
            worker_end: WorkerEnd::Exited,
        })
    };
    let ending = supervise_unit(&worker, &key, watched, run).await;
    worker.finish(&key, ending).await;
    assert!(rig.events(1).await.is_empty());

    let begun = Instant::now();
    let (watched, ..) = watch_of(&rig, 600);
    let ending = supervise_unit(&worker, &key, watched, std::future::pending()).await;
    assert!(matches!(ending, Ending::Open));
    assert!(begun.elapsed() <= RUN_STOP_BOUND + Duration::from_secs(1));
    worker.finish(&key, ending).await;
    assert!(rig.events(1).await.is_empty());
    assert!(rig.outcomes.owed().is_empty());
}

/// The start deadline ends a job that still waits for its locks, by cancelling the shell, and
/// only that: a job whose first step has begun runs on past the deadline, and neither flag the
/// run sees is set by it (an identification run takes the job's cancel as its own).
#[tokio::test(start_paused = true)]
async fn the_deadline_ends_only_a_job_that_has_not_begun() {
    let rig = Rig::new();
    let (worker, _stop) = rig.worker();
    let key = JobKey {
        job_id: JobId(job_id(1)),
        generation: 1,
    };
    // Not begun: the lock wait is cancelled at the deadline and the job is expired.
    let (watched, lock_cancel, job_cancel, _first_step) = watch_of(&rig, 5);
    let seen = lock_cancel.clone();
    let run = async move {
        while !seen.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(VciShellError::Cancelled)
    };
    let ending = supervise_unit(&worker, &key, watched, run).await;
    assert!(matches!(
        ending,
        Ending::Ended {
            state: JobState::Expired,
            ..
        }
    ));
    assert!(
        !job_cancel.load(Ordering::SeqCst),
        "the job's own cancel is not the deadline's"
    );

    // Begun: the job goes on past the deadline and ends as the run ends.
    let (watched, lock_cancel, job_cancel, first_step) = watch_of(&rig, 3);
    let (lock_seen, job_seen) = (lock_cancel.clone(), job_cancel.clone());
    let run = async move {
        first_step.send(()).unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!lock_seen.load(Ordering::SeqCst) && !job_seen.load(Ordering::SeqCst));
        finished()
    };
    let ending = supervise_unit(&worker, &key, watched, run).await;
    assert!(matches!(
        ending,
        Ending::Ended {
            state: JobState::Completed,
            ..
        }
    ));
}

/// The end is synced in the outcome record before its event is appended to the outbox: the
/// outbox's write hook finds it recorded and not yet reported.
#[tokio::test(start_paused = true)]
async fn an_end_is_recorded_before_the_outbox_takes_it() {
    let rig = Rig::new();
    let reading = reading_document();
    rig.objects.then(Answer::Refused);
    rig.deliver(&Spec::new(1, JobKind::Acquire, &[&reading]))
        .await;
    rig.ended(1).await;
    assert_eq!(*rig.owed_at_append.lock().unwrap(), vec![1]);
}

/// An end the intake appended in this start is not appended again when its `reported` record
/// failed: the record is only tried once more.
#[tokio::test(start_paused = true)]
async fn an_owed_end_is_appended_once_per_start() {
    let rig = Rig::new();
    rig.handle.stop();
    rig.handle.stopped().await;
    let (worker, _stop) = rig.worker();
    let key = JobKey {
        job_id: JobId(job_id(1)),
        generation: 1,
    };
    rig.outcomes.ended(&key, &JobState::Expired).unwrap();
    // This start appended it already (its `reported` write is what failed).
    worker
        .appended
        .lock()
        .unwrap()
        .insert((key.job_id.0.clone(), 1));
    worker.recover().await;
    assert!(rig.events(1).await.is_empty(), "not appended again");
    assert!(rig.outcomes.owed().is_empty(), "the record is made good");
    // Another job's owed end, not appended in this start, is appended once however often it is
    // looked at.
    let other = JobKey {
        job_id: JobId(job_id(2)),
        generation: 1,
    };
    rig.outcomes.ended(&other, &JobState::Expired).unwrap();
    worker.recover().await;
    worker.recover().await;
    assert_eq!(rig.events(2).await.len(), 1);
}
