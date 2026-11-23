//! The loop against a scripted server (ADR-495): the case table's rows for the loop, on a paused
//! clock over the in-memory dial, and one transfer over real loopback sockets. The client's own
//! rows (B, S, U, P, C) are the tests of `routes` and `http`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use shared_crypto::SigningKey;
use shared_proto::JobId;

use super::*;
use crate::control::exchange::{CHALLENGE_PATH, SESSION_PATH};
use crate::upload_ledger::{Probe, Step, digest_file};
use crate::uploader::fake::{FakeDial, Mode, Proof, Seen, answer, json};
use sim::{
    Logged, Peer, Reply, Route, Sim, problem, serve_tcp, serve_with, status as reply, tampered,
    unproved,
};

mod sim;

const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const DATA: &[u8] = b"0123456789";

fn jk(n: u8) -> JobKey {
    JobKey {
        job_id: JobId(format!("job-{n}")),
        generation: 1,
    }
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// The ledger, the scripted server and the running loop.
struct Rig {
    dir: tempfile::TempDir,
    sim: Arc<Sim>,
    dial: Arc<FakeDial>,
    key: Arc<SigningKey>,
    record: Record,
    /// `None` only while a restart has the ledger closed.
    ledger: Option<SharedLedger>,
    handle: Option<Handle>,
}

impl Rig {
    fn new() -> Rig {
        Rig::with_probe(Box::new(|_| Ok(())))
    }

    fn with_probe(probe: Probe) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let ledger = UploadLedger::open_with(dir.path(), AGENT_ID, probe).unwrap();
        let origin = format!("http://127.0.0.1:{}", sim::API_PORT);
        let sim = Sim::new(4, sim::OBJECT_PORT, &origin, AGENT_ID);
        let dial = sim::dial(&sim);
        let key = Arc::new(SigningKey::generate().unwrap());
        let record = Record {
            agent_id: AGENT_ID.to_owned(),
            owner_id: "0194f0a2-7c3e-7d11-8a52-000000000001".to_owned(),
            preset: "personal".to_owned(),
            allow_remote: false,
            server_origin: origin,
            instruction_key_pem: sim.instruction_key().to_spki_pem(),
            instruction_key_id: String::new(),
            agent_key_fingerprint: key.public_key().fingerprint_hex(),
        };
        Rig {
            dir,
            sim,
            dial,
            key,
            record,
            ledger: Some(SharedLedger::new(ledger)),
            handle: None,
        }
    }

    fn ledger(&self) -> &SharedLedger {
        self.ledger.as_ref().unwrap()
    }

    fn handle(&self) -> &Handle {
        self.handle.as_ref().unwrap()
    }

    fn seams(&self) -> Seams {
        Seams {
            client: Client::with_dial(self.dial.clone()),
            dial: self.dial.clone(),
            signer: self.key.clone(),
            random: shared_crypto::random_32,
        }
    }

    fn start(&mut self) {
        let handle = start_with(
            &self.record,
            &self.key.public_key(),
            self.ledger().clone(),
            self.seams(),
        )
        .unwrap();
        self.handle = Some(handle);
    }

    /// Stops the loop, closes the ledger and opens it again from its file, then starts again.
    async fn restart(&mut self) {
        self.stop().await;
        drop(self.ledger.take());
        let ledger = UploadLedger::open_in(self.dir.path(), AGENT_ID).unwrap();
        self.ledger = Some(SharedLedger::new(ledger));
        self.start();
    }

    async fn stop(&mut self) {
        let handle = self.handle.take().unwrap();
        handle.stop();
        tokio::time::timeout(secs(5), handle.stopped())
            .await
            .expect("the loop ends at a stop");
    }

    fn counters(&self) -> Status {
        self.handle().status().borrow().clone()
    }

    /// Waits until the counters satisfy `done`.
    async fn until(&self, done: impl Fn(&Status) -> bool) {
        let mut status = self.handle().status();
        tokio::time::timeout(secs(3600), status.wait_for(|s| done(s)))
            .await
            .expect("the counters reach the state in an hour")
            .unwrap();
    }

    /// Waits until the server has seen `n` requests of `route`.
    async fn until_seen(&self, route: Route, n: usize) {
        for _ in 0..100_000 {
            if self.sim.count(route) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the server did not see {n} requests of {route:?}");
    }

    /// Lets the loop run on for ten minutes.
    async fn settle(&self) {
        tokio::time::sleep(secs(600)).await;
    }

    async fn pending(&self) -> Vec<u64> {
        self.ledger()
            .pending()
            .await
            .iter()
            .map(|p| p.entry.seq)
            .collect()
    }

    async fn result(&self, n: u8, bytes: &[u8]) -> u64 {
        self.ledger()
            .enqueue_result(jk(n), bytes.to_vec())
            .await
            .unwrap()
    }

    fn comm_path(&self, n: u8) -> PathBuf {
        self.dir.path().join(format!("comm-{n}.log"))
    }

    /// A communication log of `bytes` in a file of its own.
    async fn comm(&self, n: u8, bytes: &[u8]) -> u64 {
        let path = self.comm_path(n);
        fs::write(&path, bytes).unwrap();
        self.comm_entry(n, &path).await
    }

    async fn comm_entry(&self, n: u8, path: &Path) -> u64 {
        let digest = digest_file(path).unwrap();
        self.ledger()
            .enqueue_comm_log(jk(n), path.to_owned(), digest)
            .await
            .unwrap()
    }

    fn object_file(&self, seq: u64) -> PathBuf {
        self.dir
            .path()
            .join(format!("{AGENT_ID}.objects"))
            .join(seq.to_string())
    }
}

fn routes_of(sim: &Sim) -> Vec<Route> {
    sim.log().iter().map(|l| l.route).collect()
}

fn gap(a: &Logged, b: &Logged) -> Duration {
    b.at.duration_since(a.at)
}

/// A probe that fails the steps whose flag is set.
fn failing(steps: &[(Step, &Arc<AtomicBool>)]) -> Probe {
    let steps: Vec<(Step, Arc<AtomicBool>)> =
        steps.iter().map(|(s, f)| (*s, (*f).clone())).collect();
    Box::new(move |at| {
        if steps
            .iter()
            .any(|(step, armed)| *step == at && armed.load(Ordering::SeqCst))
        {
            Err(io::Error::other("injected"))
        } else {
            Ok(())
        }
    })
}

fn flag(on: bool) -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(on))
}

// ---------------------------------------------------------------------------------------------
// The loop: L1 to L9

// L1
#[tokio::test(start_paused = true)]
async fn an_empty_ledger_sends_nothing_and_waits_for_an_enqueue_or_a_stop() {
    let mut rig = Rig::new();
    rig.start();
    rig.settle().await;
    assert_eq!(rig.dial.count(), 0, "no exchange, no request");
    rig.stop().await;
    assert_eq!(rig.dial.count(), 0);
    assert!(rig.handle.is_none());
}

// L2, the attempt's order (ADR-495 item 5), T5 first half
#[tokio::test(start_paused = true)]
async fn an_enqueue_while_idle_wakes_the_loop_and_the_exchange_comes_only_now() {
    let mut rig = Rig::new();
    rig.start();
    rig.settle().await;
    assert_eq!(rig.dial.count(), 0);
    let seq = rig.result(1, DATA).await;
    rig.until(|s| s.delivered == 1).await;
    use Route::*;
    assert_eq!(
        routes_of(&rig.sim),
        [
            Challenge, Session, Begin, Status, Part, Put, Part, Put, Part, Put, Complete
        ]
    );
    let sizes: Vec<usize> = rig.sim.of(Put).iter().map(|l| l.body_len).collect();
    assert_eq!(sizes, [4, 4, 2]);
    assert_eq!(rig.sim.object("job-1", "result").as_deref(), Some(DATA));
    assert_eq!(rig.sim.row_state("job-1", "result"), Some("finished"));
    assert!(rig.pending().await.is_empty(), "entry {seq} was removed");
    assert!(
        !rig.object_file(seq).exists(),
        "the stored object went with it"
    );
}

// L3, T2 (the bearer goes to the four routes and never to a part URL)
#[tokio::test(start_paused = true)]
async fn results_go_before_logs_each_by_number_and_the_bearer_stays_off_the_puts() {
    let mut rig = Rig::new();
    rig.comm(1, DATA).await;
    rig.result(2, DATA).await;
    rig.comm(3, DATA).await;
    rig.result(4, DATA).await;
    rig.start();
    rig.until(|s| s.delivered == 4).await;
    assert_eq!(
        rig.sim.begins(),
        [
            "job-2/result",
            "job-4/result",
            "job-1/commLog",
            "job-3/commLog"
        ]
    );
    assert_eq!(
        rig.sim.count(Route::Session),
        1,
        "one token served them all"
    );
    for logged in rig.sim.log() {
        let carries = matches!(
            logged.route,
            Route::Begin | Route::Status | Route::Part | Route::Complete
        );
        assert_eq!(logged.bearer.is_some(), carries, "{:?}", logged.route);
        if logged.route == Route::Put {
            assert_eq!(logged.port, sim::OBJECT_PORT);
        }
    }
}

// L3
#[tokio::test(start_paused = true)]
async fn no_second_connection_is_opened_while_one_request_is_in_flight() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.result(2, DATA).await;
    rig.sim.queue(Route::Begin, Reply::Hang);
    rig.start();
    rig.until_seen(Route::Begin, 1).await;
    let dialled = rig.dial.count();
    tokio::time::sleep(secs(20)).await;
    assert_eq!(rig.dial.count(), dialled, "the begin is still in flight");
    assert_eq!(rig.sim.count(Route::Begin), 1);
}

// L4
#[tokio::test(start_paused = true)]
async fn a_result_enqueued_during_a_log_waits_for_the_log_to_end() {
    let mut rig = Rig::new();
    rig.comm(1, DATA).await;
    let ledger = rig.ledger().clone();
    let once = Arc::new(AtomicBool::new(false));
    rig.sim.tap(move |logged| {
        if logged.route == Route::Put && !once.swap(true, Ordering::SeqCst) {
            let ledger = ledger.clone();
            tokio::spawn(async move {
                ledger.enqueue_result(jk(2), DATA.to_vec()).await.unwrap();
            });
        }
    });
    rig.start();
    rig.until(|s| s.delivered == 2).await;
    assert_eq!(rig.sim.begins(), ["job-1/commLog", "job-2/result"]);
    let routes = routes_of(&rig.sim);
    let first_complete = routes.iter().position(|r| *r == Route::Complete).unwrap();
    let second_begin = routes
        .iter()
        .enumerate()
        .filter(|(_, r)| **r == Route::Begin)
        .nth(1)
        .unwrap()
        .0;
    assert!(
        first_complete < second_begin,
        "the log ran to its end first"
    );
}

// L5, refusals (ADR-495 item 6)
#[tokio::test(start_paused = true)]
async fn when_every_entry_was_passed_over_the_pass_starts_again_from_the_first_after_the_wait() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.result(2, DATA).await;
    rig.sim.queue(Route::Begin, problem(400, None));
    rig.sim.queue(Route::Begin, problem(404, None));
    rig.start();
    rig.until(|s| s.delivered == 2).await;
    assert_eq!(
        rig.sim.begins(),
        [
            "job-1/result",
            "job-2/result",
            "job-1/result",
            "job-2/result"
        ]
    );
    let begins = rig.sim.of(Route::Begin);
    assert_eq!(gap(&begins[0], &begins[1]), secs(1));
    assert_eq!(gap(&begins[1], &begins[2]), secs(2));
    assert_eq!(rig.counters().passed_over, 2);
}

// Refusals: never removed, tried on every pass, a repaired server takes the entry with no restart
#[tokio::test(start_paused = true)]
async fn a_refused_entry_stays_and_is_tried_every_pass_until_the_server_takes_it() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    for code in [400, 404, 413] {
        rig.sim.queue(Route::Begin, problem(code, None));
    }
    rig.start();
    rig.until(|s| s.passed_over == 3).await;
    assert_eq!(rig.pending().await.len(), 1, "a refusal removes nothing");
    rig.until(|s| s.delivered == 1).await;
    let begins = rig.sim.of(Route::Begin);
    assert_eq!(begins.len(), 4);
    let gaps: Vec<Duration> = begins.windows(2).map(|w| gap(&w[0], &w[1])).collect();
    assert_eq!(gaps, [secs(1), secs(2), secs(4)]);
}

// L6
#[tokio::test(start_paused = true)]
async fn a_stop_at_any_await_drops_the_request_and_nothing_more_is_sent() {
    // The exchange, the begin, a part's PUT and the back-off's wait.
    for case in 0..4 {
        let mut rig = Rig::new();
        rig.result(1, DATA).await;
        let reached = match case {
            0 => {
                rig.sim.queue(Route::Challenge, Reply::Hang);
                Route::Challenge
            }
            1 => {
                rig.sim.queue(Route::Begin, Reply::Hang);
                Route::Begin
            }
            2 => {
                rig.sim.queue(Route::Put, Reply::Hang);
                Route::Put
            }
            _ => {
                rig.sim.queue(Route::Begin, problem(503, None));
                Route::Begin
            }
        };
        rig.start();
        rig.until_seen(reached, 1).await;
        // Let the 503 be taken and the wait begin.
        tokio::time::sleep(Duration::from_millis(100)).await;
        rig.stop().await;
        let seen = rig.sim.log().len();
        let dialled = rig.dial.count();
        tokio::time::sleep(secs(600)).await;
        assert_eq!(rig.sim.log().len(), seen, "case {case}");
        assert_eq!(rig.dial.count(), dialled, "case {case}");
        assert_eq!(rig.pending().await, [1], "case {case}: nothing was removed");
    }
}

// L6
#[tokio::test(start_paused = true)]
async fn a_stop_during_a_ledger_write_waits_for_the_write() {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let tx = Mutex::new(tx);
    let armed = flag(false);
    let on = armed.clone();
    let mut rig = Rig::with_probe(Box::new(move |step| {
        if step == Step::BeforeRename && on.load(Ordering::SeqCst) {
            let _ = tx.lock().unwrap().send(());
            std::thread::sleep(Duration::from_millis(300));
        }
        Ok(())
    }));
    rig.result(1, DATA).await;
    armed.store(true, Ordering::SeqCst);
    rig.start();
    tokio::task::spawn_blocking(move || rx.recv().unwrap())
        .await
        .unwrap();
    // The removal is running on its blocking task.
    let status = rig.handle().status();
    rig.stop().await;
    armed.store(false, Ordering::SeqCst);
    // Read before the ledger is touched again: the task awaited the write, and counted it.
    assert_eq!(status.borrow().delivered, 1);
}

// L7
#[tokio::test(start_paused = true)]
async fn a_poisoned_ledger_is_reopened_and_an_entry_whose_append_landed_is_uploaded() {
    let armed = flag(true);
    let mut rig = Rig::with_probe(failing(&[(Step::AfterSync, &armed)]));
    rig.start();
    // The append lands, its sync reports a failure, and the ledger is poisoned.
    assert!(
        rig.ledger()
            .enqueue_result(jk(1), DATA.to_vec())
            .await
            .is_err()
    );
    armed.store(false, Ordering::SeqCst);
    // The failed enqueue woke the loop, which reopened the ledger and found the entry.
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.sim.object("job-1", "result").as_deref(), Some(DATA));
    assert!(rig.pending().await.is_empty());
}

// L7, L8 (Poisoned)
#[tokio::test(start_paused = true)]
async fn a_reopen_that_fails_keeps_uploading_from_memory_and_the_removal_is_retried_after_it() {
    let append = flag(false);
    let reopen = flag(false);
    let mut rig = Rig::with_probe(failing(&[
        (Step::AfterSync, &append),
        (Step::AfterRename, &reopen),
    ]));
    rig.result(1, DATA).await;
    // A second entry lands on disk and poisons the ledger, which then cannot be reopened.
    append.store(true, Ordering::SeqCst);
    assert!(
        rig.ledger()
            .enqueue_result(jk(2), DATA.to_vec())
            .await
            .is_err()
    );
    append.store(false, Ordering::SeqCst);
    reopen.store(true, Ordering::SeqCst);
    rig.start();
    // A is uploaded from memory; its removal is refused (Poisoned): passed over, not spun on.
    rig.until(|s| s.passed_over >= 1).await;
    rig.settle().await;
    assert_eq!(rig.counters().delivered, 0);
    assert_eq!(rig.sim.row_state("job-1", "result"), Some("finished"));
    assert_eq!(rig.sim.row_state("job-2", "result"), None);
    let begins = rig.sim.count(Route::Begin);
    // The removal keeps failing, so the delay is not reset and doubles: about 16 begins in
    // ten minutes (1, 2, 4, ... 60 seconds), not one a second.
    assert!(begins <= 20, "paced by the back-off, {begins} begins");
    assert_eq!(rig.sim.puts().len(), 3, "no byte sent again");
    // The disk recovers: the reopen succeeds, A's begin answers finished, B is found and sent.
    reopen.store(false, Ordering::SeqCst);
    rig.until(|s| s.delivered == 2).await;
    assert_eq!(rig.sim.object("job-2", "result").as_deref(), Some(DATA));
    assert_eq!(rig.sim.puts().len(), 6);
    assert!(rig.pending().await.is_empty());
}

// L7: an idle loop on a poisoned ledger keeps trying to reopen it
#[tokio::test(start_paused = true)]
async fn an_idle_loop_keeps_reopening_a_poisoned_ledger_and_finds_the_entry_that_landed() {
    let append = flag(false);
    let reopen = flag(false);
    let reopens = Arc::new(AtomicUsize::new(0));
    let (fail_append, fail_reopen, counted) = (append.clone(), reopen.clone(), reopens.clone());
    let mut rig = Rig::with_probe(Box::new(move |step| {
        if step == Step::AfterSync && fail_append.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected"));
        }
        if step == Step::AfterRename && fail_reopen.load(Ordering::SeqCst) {
            counted.fetch_add(1, Ordering::SeqCst);
            return Err(io::Error::other("injected"));
        }
        Ok(())
    }));
    rig.start();
    append.store(true, Ordering::SeqCst);
    reopen.store(true, Ordering::SeqCst);
    // The append lands, its sync fails: poisoned, the entry on disk only.
    assert!(
        rig.ledger()
            .enqueue_result(jk(1), DATA.to_vec())
            .await
            .is_err()
    );
    append.store(false, Ordering::SeqCst);
    // Storage stays bad for 30 seconds with no further enqueue: the reopens go on, paced by
    // the back-off (waits of 1, 2, 4, 8, 16 seconds), not in a spin.
    tokio::time::sleep(secs(30)).await;
    let tried = reopens.load(Ordering::SeqCst);
    assert!((3..=8).contains(&tried), "{tried} reopens in 30 s");
    assert_eq!(rig.dial.count(), 0);
    reopen.store(false, Ordering::SeqCst);
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.sim.object("job-1", "result").as_deref(), Some(DATA));
}

// L7
#[tokio::test(start_paused = true)]
async fn a_ledger_file_that_does_not_read_back_ends_the_task() {
    let armed = flag(true);
    let mut rig = Rig::with_probe(failing(&[(Step::AfterSync, &armed)]));
    assert!(
        rig.ledger()
            .enqueue_result(jk(1), DATA.to_vec())
            .await
            .is_err()
    );
    fs::write(rig.dir.path().join(AGENT_ID), b"not a ledger").unwrap();
    rig.start();
    tokio::time::timeout(secs(5), rig.handle().stopped())
        .await
        .expect("the task ends by itself");
    assert_eq!(rig.handle().ended(), Some(Ended::LedgerUnusable));
    assert_eq!(rig.dial.count(), 0);
}

// L8
#[tokio::test(start_paused = true)]
async fn a_removal_that_fails_is_passed_over_and_the_next_begin_answers_finished() {
    let armed = flag(false);
    let mut rig = Rig::with_probe(failing(&[(Step::BeforeRename, &armed)]));
    rig.result(1, DATA).await;
    armed.store(true, Ordering::SeqCst);
    rig.start();
    rig.until(|s| s.passed_over >= 1).await;
    assert_eq!(rig.pending().await, [1], "the entry is still pending");
    assert_eq!(rig.counters().delivered, 0);
    armed.store(false, Ordering::SeqCst);
    rig.until(|s| s.delivered == 1).await;
    assert!(rig.pending().await.is_empty());
    assert_eq!(rig.sim.puts().len(), 3, "no byte sent for the removal");
    assert_eq!(rig.sim.count(Route::Complete), 1);
    assert_eq!(rig.sim.count(Route::Status), 1);
    assert_eq!(rig.sim.begins().len(), 2);
}

// L8
#[tokio::test(start_paused = true)]
async fn a_removal_that_failed_after_the_entry_left_memory_needs_nothing_more() {
    let armed = flag(false);
    let mut rig = Rig::with_probe(failing(&[(Step::AfterRename, &armed)]));
    rig.result(1, DATA).await;
    armed.store(true, Ordering::SeqCst);
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert!(rig.pending().await.is_empty());
    assert_eq!(rig.counters().passed_over, 0);
    // The ledger is poisoned; the next wake reopens it and goes on.
    armed.store(false, Ordering::SeqCst);
    // An enqueue on the poisoned ledger fails and wakes the loop, which reopens it.
    let mut enqueued = false;
    for _ in 0..50 {
        let tried = rig.ledger().enqueue_result(jk(2), DATA.to_vec()).await;
        if tried.is_ok() {
            enqueued = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(enqueued, "the loop reopened the ledger");
    rig.until(|s| s.delivered == 2).await;
}

// L9
#[tokio::test(start_paused = true)]
async fn the_same_key_enqueued_during_its_transfer_gets_the_ledgers_answer_and_changes_nothing() {
    let mut rig = Rig::new();
    let seq = rig.result(1, DATA).await;
    let ledger = rig.ledger().clone();
    let answers = Arc::new(Mutex::new(Vec::new()));
    let seen = answers.clone();
    let once = Arc::new(AtomicBool::new(false));
    rig.sim.tap(move |logged| {
        if logged.route == Route::Put && !once.swap(true, Ordering::SeqCst) {
            let (ledger, seen) = (ledger.clone(), seen.clone());
            tokio::spawn(async move {
                let same = ledger.enqueue_result(jk(1), DATA.to_vec()).await;
                let other = ledger.enqueue_result(jk(1), b"other".to_vec()).await;
                seen.lock().unwrap().push((same.ok(), other.is_err()));
            });
        }
    });
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(*answers.lock().unwrap(), [(Some(seq), true)]);
    assert_eq!(rig.sim.begins().len(), 1);
    assert_eq!(rig.sim.object("job-1", "result").as_deref(), Some(DATA));
}

// ---------------------------------------------------------------------------------------------
// The token: T1 to T6

// T1
#[tokio::test(start_paused = true)]
async fn a_failed_exchange_waits_the_delay_a_longer_retry_after_or_the_cap() {
    // The route that fails, what it answers, and the wait before the next challenge.
    let cases: [(Route, Vec<u8>, u64); 3] = [
        (Route::Session, answer(500, "", "{}"), 1),
        (
            Route::Challenge,
            answer(429, "Retry-After: 30\r\n", "{}"),
            30,
        ),
        (Route::Session, answer(403, "", "{}"), 60),
    ];
    for (route, canned, wait) in cases {
        let mut rig = Rig::new();
        rig.result(1, DATA).await;
        rig.sim.queue(route, Reply::Bytes(canned));
        rig.start();
        rig.until(|s| s.delivered == 1).await;
        let challenges = rig.sim.of(Route::Challenge);
        assert_eq!(challenges.len(), 2, "{route:?}");
        assert_eq!(gap(&challenges[0], &challenges[1]), secs(wait), "{route:?}");
    }
}

// T3
#[tokio::test(start_paused = true)]
async fn a_401_drops_the_token_exchanges_at_once_and_repeats_the_request_once() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Begin, problem(401, None));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.sim.count(Route::Session), 2);
    let begins = rig.sim.of(Route::Begin);
    assert_eq!(begins.len(), 2);
    assert_eq!(gap(&begins[0], &begins[1]), secs(0), "no wait");
    assert_ne!(begins[0].bearer, begins[1].bearer, "the new token");
}

// T3
#[tokio::test(start_paused = true)]
async fn a_second_401_in_a_row_is_a_retry() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Begin, problem(401, None));
    rig.sim.queue(Route::Begin, problem(401, None));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.counters().retries, 1);
    let begins = rig.sim.of(Route::Begin);
    assert_eq!(begins.len(), 3);
    assert_eq!(gap(&begins[0], &begins[1]), secs(0));
    assert_eq!(gap(&begins[1], &begins[2]), secs(1), "the retry waits");
    assert_eq!(rig.sim.count(Route::Session), 3, "the token was dropped");
}

// T4
#[tokio::test(start_paused = true)]
async fn an_exchange_that_fails_after_a_401_is_handled_as_a_failed_exchange() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Begin, problem(401, None));
    // The first session is the server's own; the second one, after the 401, fails.
    rig.sim
        .queue_after(Route::Session, 1, Reply::Bytes(answer(500, "", "{}")));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.sim.count(Route::Session), 3);
    let challenges = rig.sim.of(Route::Challenge);
    assert_eq!(challenges.len(), 3);
    assert_eq!(gap(&challenges[1], &challenges[2]), secs(1), "the delay");
    assert_eq!(rig.counters().retries, 1);
}

// T5, T6
#[tokio::test(start_paused = true)]
async fn the_token_is_dropped_when_nothing_is_pending_and_never_judged_by_age() {
    let mut rig = Rig::new();
    rig.start();
    rig.result(1, DATA).await;
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.sim.count(Route::Session), 1);
    rig.settle().await;
    rig.result(2, DATA).await;
    rig.until(|s| s.delivered == 2).await;
    assert_eq!(
        rig.sim.count(Route::Session),
        2,
        "a new exchange for new work"
    );

    // Over six minutes of retries (past the token's 300 seconds) one token serves.
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    for _ in 0..12 {
        rig.sim.queue(Route::Begin, problem(503, None));
    }
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    let begins = rig.sim.of(Route::Begin);
    assert!(gap(&begins[0], begins.last().unwrap()) > secs(300));
    assert_eq!(rig.sim.count(Route::Session), 1);
}

// ---------------------------------------------------------------------------------------------
// The back-off

#[tokio::test(start_paused = true)]
async fn a_stored_part_and_a_finished_answer_bring_the_delay_back_to_one_second() {
    // Two 503s double the delay to 4 seconds. The third attempt stores the parts (a PUT answered
    // 200 resets the delay) and its completion is refused with a 503; the fourth attempt, which
    // stores nothing, owes a wait of 1 second and not 4.
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Begin, problem(503, None));
    rig.sim.queue(Route::Begin, problem(503, None));
    rig.sim.queue(Route::Complete, problem(503, None));
    rig.sim.queue(Route::Complete, problem(503, None));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    let begins = rig.sim.of(Route::Begin);
    assert_eq!(gap(&begins[0], &begins[1]), secs(1));
    assert_eq!(gap(&begins[1], &begins[2]), secs(2));
    assert_eq!(gap(&begins[2], &begins[3]), secs(0), "a part was stored");
    assert_eq!(gap(&begins[3], &begins[4]), secs(1), "reset by the PUT");

    // A `finished` answer resets it too: A's begin answers finished after two waits, and B's
    // first refusal then waits 1 second.
    let mut rig = Rig::new();
    rig.sim.preload("job-1", "result", DATA, "finished", 3);
    rig.result(1, DATA).await;
    rig.result(2, DATA).await;
    rig.sim.queue(Route::Begin, problem(503, None));
    rig.sim.queue(Route::Begin, problem(503, None));
    rig.sim.queue(Route::Begin, Reply::Default);
    rig.sim.queue(Route::Begin, problem(503, None));
    rig.start();
    rig.until(|s| s.delivered == 2).await;
    let begins = rig.sim.of(Route::Begin);
    assert_eq!(begins.len(), 5);
    assert_eq!(begins[3].detail, "job-2/result");
    assert_eq!(gap(&begins[3], &begins[4]), secs(1));
    assert_eq!(rig.sim.puts().len(), 3, "A's bytes were never sent");
}

#[tokio::test(start_paused = true)]
async fn a_202_repeats_the_completion_after_the_waits() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    for _ in 0..2 {
        rig.sim
            .queue(Route::Complete, reply(202, r#"{"state":"completing"}"#));
    }
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    let complete = rig.sim.of(Route::Complete);
    assert_eq!(complete.len(), 3);
    assert_eq!(gap(&complete[0], &complete[1]), secs(1));
    assert_eq!(gap(&complete[1], &complete[2]), secs(2));
    assert_eq!(rig.counters().retries, 0);
}

// ---------------------------------------------------------------------------------------------
// The source: V1 to V8

// V2, V3, R5
#[tokio::test(start_paused = true)]
async fn a_source_proved_broken_is_abandoned_with_nothing_sent_and_a_log_file_is_left_alone() {
    let mut rig = Rig::new();
    // 1: the file is gone; 2: shorter; 3: same length, other bytes; 4: a stored result that
    // changed on disk.
    rig.comm(1, DATA).await;
    rig.comm(2, DATA).await;
    rig.comm(3, DATA).await;
    let stored = rig.result(4, DATA).await;
    fs::remove_file(rig.comm_path(1)).unwrap();
    fs::write(rig.comm_path(2), b"0123").unwrap();
    fs::write(rig.comm_path(3), b"012345678X").unwrap();
    fs::write(rig.object_file(stored), b"012345678X").unwrap();
    rig.start();
    rig.until(|s| s.abandoned == 4).await;
    assert!(rig.sim.log().is_empty(), "no exchange, no begin, no PUT");
    assert!(rig.pending().await.is_empty());
    assert_eq!(fs::read(rig.comm_path(2)).unwrap(), b"0123");
    assert_eq!(fs::read(rig.comm_path(3)).unwrap(), b"012345678X");
    assert!(!rig.object_file(stored).exists(), "the stored object goes");
    // After a restart the entries are gone and the files intact.
    rig.restart().await;
    rig.settle().await;
    assert!(rig.pending().await.is_empty());
    assert!(rig.comm_path(2).exists() && rig.comm_path(3).exists());
    assert_eq!(rig.counters(), Status::default());
}

// V4
#[tokio::test(start_paused = true)]
async fn another_failure_to_read_the_source_is_a_retry_and_never_an_abandon() {
    let mut rig = Rig::new();
    let sub = rig.dir.path().join("volume");
    fs::create_dir(&sub).unwrap();
    let path = sub.join("comm.log");
    fs::write(&path, DATA).unwrap();
    rig.comm_entry(1, &path).await;

    // The directory is gone (a volume that is away): not a missing file.
    fs::remove_dir_all(&sub).unwrap();
    rig.start();
    // Paced by the back-off (1, 2, 4 ... 60 seconds): about 16 retries in ten minutes, and no
    // spin on a check that makes no request.
    rig.settle().await;
    let retries = rig.counters().retries;
    assert!((8..=20).contains(&retries), "{retries} retries in 600 s");
    // The path is a directory now: opening or reading it fails, which proves nothing.
    fs::create_dir_all(&path).unwrap();
    rig.until(|s| s.retries >= retries + 2).await;
    assert_eq!(rig.counters().abandoned, 0);
    assert!(
        rig.sim.log().is_empty(),
        "no begin for an unverified source"
    );
    // The file comes back as it was: the entry is delivered.
    fs::remove_dir_all(&path).unwrap();
    fs::write(&path, DATA).unwrap();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.counters().abandoned, 0);
}

// V5
#[tokio::test(start_paused = true)]
async fn a_file_that_runs_short_while_a_part_streams_drops_the_put_and_is_checked_again() {
    let mut rig = Rig::new();
    rig.comm(1, DATA).await;
    let path = rig.comm_path(1);
    rig.sim.tap(move |logged| {
        if logged.route == Route::Begin {
            // After the check, before the parts: the file loses its tail.
            fs::write(&path, b"01234").unwrap();
        }
    });
    rig.start();
    rig.until(|s| s.abandoned == 1).await;
    // Part 1 (4 bytes) went; the PUT of part 2 ended after one byte of its four, and the server
    // never saw it whole.
    assert_eq!(rig.sim.puts(), [1]);
    assert_eq!(rig.sim.count(Route::Complete), 0);
    assert_eq!(rig.counters().retries, 1);
    assert!(rig.pending().await.is_empty());
    assert_eq!(
        rig.sim.begins().len(),
        1,
        "the second look abandons before a begin"
    );
}

// V6, C4 (the re-check after a failed completion)
#[tokio::test(start_paused = true)]
async fn bytes_changed_after_the_check_are_sent_and_the_failed_completion_leads_to_an_abandon() {
    let mut rig = Rig::new();
    rig.comm(1, DATA).await;
    let path = rig.comm_path(1);
    rig.sim.tap(move |logged| {
        if logged.route == Route::Begin {
            fs::write(&path, b"012345678X").unwrap();
        }
    });
    rig.start();
    rig.until(|s| s.abandoned == 1).await;
    assert_eq!(rig.sim.puts(), [1, 2, 3], "the changed bytes were sent");
    assert_eq!(rig.sim.row_state("job-1", "commLog"), Some("failed"));
    assert_eq!(rig.sim.begins().len(), 1);
    assert_eq!(rig.counters().set_aside, 0);
    assert_eq!(rig.counters().retries, 1);
}

// V7
#[tokio::test(start_paused = true)]
async fn only_the_first_length_bytes_of_a_file_that_grew_are_read() {
    let mut rig = Rig::new();
    rig.comm(1, DATA).await;
    let path = rig.comm_path(1);
    rig.sim.tap(move |logged| {
        if logged.route == Route::Begin {
            let mut grown = DATA.to_vec();
            grown.extend_from_slice(b" and more written since");
            fs::write(&path, grown).unwrap();
        }
    });
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.sim.object("job-1", "commLog").as_deref(), Some(DATA));
}

// V8
#[tokio::test(start_paused = true)]
async fn an_empty_object_is_one_empty_part() {
    let mut rig = Rig::new();
    rig.result(1, b"").await;
    rig.comm(2, b"").await;
    rig.start();
    rig.until(|s| s.delivered == 2).await;
    assert_eq!(rig.sim.puts(), [1, 1]);
    assert!(rig.sim.of(Route::Put).iter().all(|l| l.body_len == 0));
    assert_eq!(rig.sim.count(Route::Part), 2);
}

// ---------------------------------------------------------------------------------------------
// The completion and the status: C3, C4, C5, S6, U3

// C4, R4
#[tokio::test(start_paused = true)]
async fn a_failed_completion_begins_again_once_and_a_second_in_a_row_sets_the_entry_aside() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Complete, problem(409, Some("failed")));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.sim.begins().len(), 2, "one more begin");
    assert_eq!(rig.counters().retries, 1);
    assert_eq!(rig.counters().set_aside, 0);

    // Twice in a row: set aside until a restart, and nothing more is sent for it.
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Complete, problem(409, Some("failed")));
    rig.sim.queue(Route::Complete, problem(409, Some("failed")));
    rig.start();
    rig.until(|s| s.set_aside == 1).await;
    let seen = rig.sim.log().len();
    rig.settle().await;
    assert_eq!(rig.sim.log().len(), seen, "idle");
    assert_eq!(rig.pending().await, [1]);
    // A restart tries it again (R4).
    rig.restart().await;
    rig.until(|s| s.delivered == 1).await;
    assert!(rig.pending().await.is_empty());
}

// C3
#[tokio::test(start_paused = true)]
async fn an_open_completion_is_a_retry_and_a_second_with_no_part_stored_between_is_passed_over() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Complete, problem(409, Some("open")));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.counters().retries, 1);
    assert_eq!(rig.counters().passed_over, 0);

    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Complete, problem(409, Some("open")));
    rig.sim.queue(Route::Complete, problem(409, Some("open")));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.counters().retries, 1);
    assert_eq!(rig.counters().passed_over, 1);
}

// C5
#[tokio::test(start_paused = true)]
async fn a_409_with_no_state_is_passed_over() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Complete, problem(409, None));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.counters().passed_over, 1);
    assert_eq!(rig.counters().retries, 0);
}

// S6
#[tokio::test(start_paused = true)]
async fn a_503_status_on_an_open_row_is_followed_by_one_completion_in_the_same_attempt() {
    // A lost upload: the completion writes the row failed, and the next begin makes a new one.
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Status, problem(503, None));
    rig.sim.queue(Route::Complete, problem(409, Some("failed")));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    let status = rig.sim.of(Route::Status);
    let complete = rig.sim.of(Route::Complete);
    assert_eq!(gap(&status[0], &complete[0]), secs(0), "the same attempt");
    assert_eq!(rig.sim.begins().len(), 2);
    assert_eq!(
        rig.sim.puts(),
        [1, 2, 3],
        "sent once, by the second attempt"
    );

    // An outage with parts merely missing: the row stays open and the next status resends.
    let mut rig = Rig::new();
    rig.sim.preload("job-1", "result", DATA, "open", 1);
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Status, problem(503, None));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.sim.puts(), [2, 3]);
    assert_eq!(rig.counters().retries, 1);

    // The row was complete all along: the completion finishes it with no byte sent.
    let mut rig = Rig::new();
    rig.sim.preload("job-1", "result", DATA, "open", 3);
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Status, problem(503, None));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert!(rig.sim.puts().is_empty());
    assert_eq!(rig.counters().retries, 0);
}

// U3
#[tokio::test(start_paused = true)]
async fn a_part_url_409_is_a_retry_and_the_next_begin_reads_the_row() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Part, problem(409, Some("completing")));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.counters().retries, 1);
    assert_eq!(rig.sim.begins().len(), 2);
    assert_eq!(rig.sim.puts(), [1, 2, 3]);
}

// ---------------------------------------------------------------------------------------------
// The object port is TLS under the pin of the answer (ADR-505)

// O1
#[tokio::test(start_paused = true)]
async fn a_part_goes_to_an_https_url_with_the_pin_of_its_answer() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.sim.puts(), [1, 2, 3]);
    assert_eq!(rig.sim.object("job-1", "result"), Some(DATA.to_vec()));
    assert_eq!(rig.counters().retries, 0);
}

// O2
#[tokio::test(start_paused = true)]
async fn a_peer_that_is_not_the_object_server_gets_no_part_and_the_entry_stays() {
    for peer in [Peer::Another, Peer::WithoutKey, Peer::Plain] {
        let mut rig = Rig::new();
        let seq = rig.result(1, DATA).await;
        rig.sim.set_peer(peer);
        rig.start();
        rig.until(|s| s.retries >= 1).await;
        assert!(rig.sim.puts().is_empty(), "{peer:?}: a part reached it");
        assert_eq!(rig.pending().await, [seq], "{peer:?}");
        assert!(rig.object_file(seq).exists(), "{peer:?}");
        let read = rig.sim.plain_read();
        assert!(
            !read.windows(DATA.len()).any(|w| w == DATA) && !read.windows(3).any(|w| w == b"PUT"),
            "{peer:?}: the peer read the request"
        );
        // The object server is back: the same entry is delivered.
        rig.sim.set_peer(Peer::Real);
        rig.until(|s| s.delivered == 1).await;
        assert_eq!(rig.sim.object("job-1", "result"), Some(DATA.to_vec()));
        assert!(rig.pending().await.is_empty());
    }
}

// O3
#[tokio::test(start_paused = true)]
async fn a_port_taken_after_the_stack_ended_gets_no_part_and_the_entry_stays() {
    let mut rig = Rig::new();
    let seq = rig.result(1, DATA).await;
    // The first part is stored; then the stack is gone and another process answers on the object
    // port, while the agent still holds the pin of the object server's certificate.
    let sim = rig.sim.clone();
    let taken = flag(false);
    rig.sim.tap(move |logged| {
        if logged.route == Route::Put && !taken.swap(true, Ordering::SeqCst) {
            sim.set_peer(Peer::Another);
        }
    });
    rig.start();
    rig.until(|s| s.retries >= 1).await;
    assert_eq!(rig.sim.puts(), [1], "no part reached the taker");
    assert_eq!(rig.pending().await, [seq]);
    assert!(rig.object_file(seq).exists());
    assert_eq!(rig.sim.count(Route::Complete), 0);
    // The object server is back: the entry goes on from the part the status gives.
    rig.sim.set_peer(Peer::Real);
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.sim.object("job-1", "result"), Some(DATA.to_vec()));
}

// O4
#[tokio::test(start_paused = true)]
async fn a_certificate_regenerated_while_the_agent_holds_an_old_url_is_refused_and_the_next_answer_works()
 {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    // The first part URL names the pin of the certificate the object server has just lost.
    rig.sim.regenerate_certificate_after_next_part_url();
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.counters().retries, 1);
    assert_eq!(rig.sim.puts(), [1, 2, 3], "no part before the new pin");
    assert_eq!(rig.sim.object("job-1", "result"), Some(DATA.to_vec()));
}

// ---------------------------------------------------------------------------------------------
// Restarts: R1 to R3

// R1
#[tokio::test(start_paused = true)]
async fn a_restart_during_a_put_resumes_after_the_parts_the_status_reports() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    // Parts 1 and 2 are stored by the server; the third PUT is never answered.
    rig.sim.queue_after(Route::Put, 2, Reply::Hang);
    rig.start();
    rig.until_seen(Route::Put, 3).await;
    rig.restart().await;
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(
        rig.sim.puts(),
        [1, 2, 3, 3],
        "after the restart only part 3"
    );
    assert_eq!(rig.sim.begins().len(), 2);
    assert_eq!(rig.sim.object("job-1", "result").as_deref(), Some(DATA));
}

// R2
#[tokio::test(start_paused = true)]
async fn a_restart_after_finished_and_before_the_removal_delivers_with_no_byte_sent() {
    let mut rig = Rig::new();
    rig.sim.preload("job-1", "result", DATA, "finished", 3);
    rig.result(1, DATA).await;
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert!(rig.sim.puts().is_empty());
    assert_eq!(rig.sim.count(Route::Status), 0);
    assert_eq!(rig.sim.count(Route::Complete), 0);
}

// R3
#[tokio::test(start_paused = true)]
async fn a_restart_during_the_completion_completes_directly() {
    let mut rig = Rig::new();
    rig.sim.preload("job-1", "result", DATA, "completing", 3);
    rig.result(1, DATA).await;
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert!(rig.sim.puts().is_empty());
    assert_eq!(rig.sim.count(Route::Status), 0);
    assert_eq!(rig.sim.count(Route::Complete), 1);
}

// ---------------------------------------------------------------------------------------------
// Warnings (ADR-495 item 11)

#[tokio::test(start_paused = true)]
async fn a_refusal_and_an_abandon_are_warned_once_per_entry_and_class_and_counted() {
    let rig = Rig::new();
    rig.result(1, DATA).await;
    rig.comm(2, DATA).await;
    let (_stop, stop_rx) = watch::channel(false);
    let (status_tx, _status_rx) = watch::channel(Status::default());
    let mut worker = Worker::new(
        &rig.record,
        &rig.key.public_key(),
        rig.ledger().clone(),
        rig.seams(),
        Stop(stop_rx),
        status_tx,
    )
    .unwrap();
    let pending = rig.ledger().pending().await;
    let (first, second) = (&pending[0], &pending[1]);
    // One warning per entry and class; the rest are debug lines.
    assert!(worker.warn(first, "refused", Some(404)));
    assert!(!worker.warn(first, "refused", Some(500)));
    assert!(!worker.warn(first, "refused", None));
    assert!(worker.warn(first, "unpromised", Some(404)));
    assert!(worker.warn(second, "refused", Some(404)));
    assert!(worker.warn(second, "source-missing", None));

    // The counters are the loop's: three refusals and an abandon.
    let mut rig = rig;
    fs::remove_file(rig.comm_path(2)).unwrap();
    for _ in 0..3 {
        rig.sim.queue(Route::Begin, problem(404, None));
    }
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    let status = rig.counters();
    assert_eq!((status.passed_over, status.abandoned), (3, 1));
}

// Abandon whose write fails
#[tokio::test(start_paused = true)]
async fn an_abandon_whose_write_fails_is_retried_at_the_back_offs_pace() {
    let armed = flag(false);
    let mut rig = Rig::with_probe(failing(&[(Step::BeforeRename, &armed)]));
    rig.comm(1, DATA).await;
    fs::remove_file(rig.comm_path(1)).unwrap();
    armed.store(true, Ordering::SeqCst);
    rig.start();
    rig.settle().await;
    let passed = rig.counters().passed_over;
    assert!((8..=20).contains(&passed), "{passed} passes in 600 s");
    assert_eq!(rig.counters().abandoned, 0);
    assert_eq!(rig.pending().await, [1], "the entry stays");
    armed.store(false, Ordering::SeqCst);
    rig.until(|s| s.abandoned == 1).await;
    assert!(rig.pending().await.is_empty());
    assert!(rig.sim.log().is_empty());
}

// A failed exchange after a stored part keeps its Retry-After
#[tokio::test(start_paused = true)]
async fn a_failed_exchange_after_a_stored_part_still_owes_its_retry_after() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    // Part 1 is stored; the part URL of part 2 answers 401, and the exchange that follows is
    // told to wait 30 seconds.
    rig.sim.queue_after(Route::Part, 1, problem(401, None));
    rig.sim.queue_after(
        Route::Challenge,
        1,
        Reply::Bytes(answer(429, "Retry-After: 30\r\n", "{}")),
    );
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    let challenges = rig.sim.of(Route::Challenge);
    assert_eq!(challenges.len(), 3);
    assert!(gap(&challenges[1], &challenges[2]) >= secs(30));
}

// C3 (ADR-495): two `open` completions in a row pass the entry over, whatever was stored
#[tokio::test(start_paused = true)]
async fn two_open_completions_in_a_row_pass_the_entry_over_even_when_parts_were_sent() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    // The server answers every PUT 200 and stores nothing: the status holds no part each time.
    for _ in 0..6 {
        rig.sim.queue(Route::Put, Reply::Bytes(answer(200, "", "")));
    }
    rig.sim.queue(Route::Complete, problem(409, Some("open")));
    rig.sim.queue(Route::Complete, problem(409, Some("open")));
    rig.start();
    rig.until(|s| s.passed_over == 1).await;
    assert_eq!(rig.counters().retries, 1);
    assert_eq!(
        rig.sim.puts().len(),
        6,
        "bounded: two sends, not an endless resend"
    );
    // The pass starts again afterwards and the server finally takes it.
    rig.until(|s| s.delivered == 1).await;
}

// Status answers finished, completing, failed
#[tokio::test(start_paused = true)]
async fn a_status_answering_finished_completing_or_failed_is_acted_on() {
    let body = |state: &str| format!(r#"{{"state":"{state}","partBytes":4,"partCount":3}}"#);
    // finished: delivered with no byte sent and no completion.
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Status, reply(200, &body("finished")));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert!(rig.sim.puts().is_empty());
    assert_eq!(rig.sim.count(Route::Complete), 0);

    // completing: the completion directly, no part URL first.
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim
        .queue(Route::Status, reply(200, &body("completing")));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    use Route::*;
    assert_eq!(
        &routes_of(&rig.sim)[..5],
        [Challenge, Session, Begin, Status, Complete]
    );

    // failed: begin again once; a second in a row sets the entry aside.
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Status, reply(200, &body("failed")));
    rig.start();
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.sim.begins().len(), 2);
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue(Route::Status, reply(200, &body("failed")));
    rig.sim.queue(Route::Status, reply(200, &body("failed")));
    rig.start();
    rig.until(|s| s.set_aside == 1).await;
    assert!(rig.sim.puts().is_empty());
    assert_eq!(rig.pending().await, [1]);
}

// A Corrupt reopen racing a stop is a stop
#[tokio::test(start_paused = true)]
async fn a_corrupt_reopen_that_races_a_stop_does_not_end_the_task_as_unusable() {
    let armed = flag(true);
    let mut rig = Rig::with_probe(failing(&[(Step::AfterSync, &armed)]));
    assert!(
        rig.ledger()
            .enqueue_result(jk(1), DATA.to_vec())
            .await
            .is_err()
    );
    fs::write(rig.dir.path().join(AGENT_ID), b"not a ledger").unwrap();
    // Hold the ledger so that the loop's reopen waits, then stop before it can answer.
    let shared = rig.ledger().clone();
    let held = shared.inner.ledger.lock().await;
    rig.start();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    rig.handle().stop();
    drop(held);
    tokio::time::timeout(secs(5), rig.handle().stopped())
        .await
        .expect("the task ends");
    assert_eq!(rig.handle().ended(), None);
}

// ---------------------------------------------------------------------------------------------
// Wiring

#[tokio::test(start_paused = true)]
async fn a_key_other_than_the_records_is_refused_and_a_dropped_handle_stops_the_loop() {
    let rig = Rig::new();
    let other = SigningKey::generate().unwrap().public_key();
    let refused = start_with(&rig.record, &other, rig.ledger().clone(), rig.seams());
    assert!(matches!(refused, Err(ControlError::KeyMismatch)));

    let mut rig = rig;
    rig.dial.set(Mode::Refuse);
    rig.result(1, DATA).await;
    rig.start();
    rig.until(|s| s.retries >= 1).await;
    let handle = rig.handle.take().unwrap();
    let mut done = handle.done.clone();
    drop(handle);
    tokio::time::timeout(secs(5), done.wait_for(|d| *d))
        .await
        .expect("a dropped handle stops the task")
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// Real loopback

/// The ledger and the registration of an agent whose server is on a real loopback port.
struct Real {
    dir: tempfile::TempDir,
    ledger: SharedLedger,
    key: Arc<SigningKey>,
    record: Record,
}

impl Real {
    /// An agent that registered with `instruction_key` at the server on `api_port`.
    fn new(instruction_key: &shared_crypto::PublicKey, api_port: u16) -> Real {
        let dir = tempfile::tempdir().unwrap();
        let ledger = SharedLedger::new(UploadLedger::open_in(dir.path(), AGENT_ID).unwrap());
        let key = Arc::new(SigningKey::generate().unwrap());
        let record = Record {
            agent_id: AGENT_ID.to_owned(),
            owner_id: "0194f0a2-7c3e-7d11-8a52-000000000001".to_owned(),
            preset: "personal".to_owned(),
            allow_remote: false,
            server_origin: format!("http://127.0.0.1:{api_port}"),
            instruction_key_pem: instruction_key.to_spki_pem(),
            instruction_key_id: String::new(),
            agent_key_fingerprint: key.public_key().fingerprint_hex(),
        };
        Real {
            dir,
            ledger,
            key,
            record,
        }
    }

    /// Starts the loop over real sockets.
    fn start(&self) -> Handle {
        let seams = Seams {
            client: Client::loopback(),
            dial: Arc::new(LoopbackPorts),
            signer: self.key.clone(),
            random: shared_crypto::random_32,
        };
        start_with(
            &self.record,
            &self.key.public_key(),
            self.ledger.clone(),
            seams,
        )
        .unwrap()
    }

    /// Waits until the loop has counted `n` retries (a state it stays in while it waits).
    async fn until_retries(handle: &Handle, n: u64) {
        let mut status = handle.status();
        tokio::time::timeout(secs(30), status.wait_for(|s| s.retries >= n))
            .await
            .expect("the loop counts the retry")
            .unwrap();
    }
}

#[tokio::test]
async fn a_transfer_over_loopback_sockets_is_cut_in_the_middle_and_resumed_after_the_status() {
    let api = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let objects = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let (api_port, object_port) = (
        api.local_addr().unwrap().port(),
        objects.local_addr().unwrap().port(),
    );
    let sim = Sim::new(
        1000,
        object_port,
        &format!("http://127.0.0.1:{api_port}"),
        AGENT_ID,
    );
    tokio::spawn(serve_tcp(api, api_port, sim.clone()));
    tokio::spawn(serve_tcp(objects, object_port, sim.clone()));
    // The third PUT is cut: the server closes with no answer.
    sim.queue_after(Route::Put, 2, Reply::Cut);

    let real = Real::new(&sim.instruction_key(), api_port);
    let data: Vec<u8> = (0..3500u32).map(|n| (n % 251) as u8).collect();
    real.ledger
        .enqueue_result(jk(1), data.clone())
        .await
        .unwrap();
    let handle = real.start();
    let ledger = real.ledger.clone();
    let mut status = handle.status();
    tokio::time::timeout(secs(30), status.wait_for(|s| s.delivered == 1))
        .await
        .expect("delivered over the sockets")
        .unwrap();
    assert_eq!(
        sim.puts(),
        [1, 2, 3, 3, 4],
        "resumed at the part the status gave"
    );
    assert_eq!(sim.object("job-1", "result"), Some(data));
    assert!(ledger.pending().await.is_empty());
}

// ---------------------------------------------------------------------------------------------
// The server's proof of the answers (ADR-504 item 3): R2 to R11

const UPLOAD_ID: &str = "3f2a9c1e-5b7d-4e08-9a41-0c6d2b8f7a13";

fn begin_body(state: &str) -> String {
    format!(r#"{{"uploadId":"{UPLOAD_ID}","partBytes":4,"partCount":3,"state":"{state}"}}"#)
}

/// A pin as a forger's answer might carry one.
const FORGED_PIN: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";

/// A part URL that names the server's object port, as a forger's answer might.
fn part_body(port: u16) -> String {
    format!(
        r#"{{"url":"https://127.0.0.1:{port}/obj/{UPLOAD_ID}/1?sig=s","certificatePin":"{FORGED_PIN}","expiresInSecs":60}}"#
    )
}

fn status_body(state: &str) -> String {
    format!(r#"{{"state":"{state}","partBytes":4,"partCount":3}}"#)
}

fn state_body(state: &str) -> String {
    format!(r#"{{"state":"{state}"}}"#)
}

fn problem_body(state: &str) -> String {
    format!(r#"{{"title":"x","state":"{state}"}}"#)
}

/// The requests of an attempt that got as far as `route`'s first one.
fn up_to(route: Route) -> Vec<Route> {
    use Route::*;
    let all = [
        Challenge, Session, Begin, Status, Part, Put, Part, Put, Part, Put, Complete,
    ];
    let end = match route {
        Begin => 3,
        Status => 4,
        Part => 5,
        _ => all.len(),
    };
    all[..end].to_vec()
}

/// Every answer a route gives as a result, with the first request of that route that the loop
/// makes (the part URL's is part 1, whose PUT is the next request if it is believed).
fn results() -> Vec<(&'static str, Route, u16, String)> {
    vec![
        (
            "begin 200 finished",
            Route::Begin,
            200,
            begin_body("finished"),
        ),
        ("begin 200 open", Route::Begin, 200, begin_body("open")),
        ("begin 201 open", Route::Begin, 201, begin_body("open")),
        ("part url", Route::Part, 200, part_body(sim::OBJECT_PORT)),
        (
            "status finished",
            Route::Status,
            200,
            status_body("finished"),
        ),
        ("status failed", Route::Status, 200, status_body("failed")),
        ("complete 200", Route::Complete, 200, state_body("finished")),
        (
            "complete 202",
            Route::Complete,
            202,
            state_body("completing"),
        ),
        (
            "complete 409 open",
            Route::Complete,
            409,
            problem_body("open"),
        ),
        (
            "complete 409 failed",
            Route::Complete,
            409,
            problem_body("failed"),
        ),
    ]
}

// R2 to R6
#[tokio::test(start_paused = true)]
async fn an_unproven_result_is_a_retry_at_the_cap_that_drops_the_token() {
    for (label, route, code, body) in results() {
        for proof in Proof::UNPROVED {
            let what = format!("{label} {proof:?}");
            let mut rig = Rig::new();
            let seq = rig.result(1, DATA).await;
            rig.sim.queue(route, tampered(code, &body, proof));
            rig.start();
            rig.until(|s| s.retries == 1).await;
            // Nothing the forger said was acted on: not a request, not a removal.
            assert_eq!(routes_of(&rig.sim), up_to(route), "{what}");
            assert!(
                !rig.dial.dialled.lock().unwrap().contains(&sim::OBJECT_PORT)
                    || route == Route::Complete,
                "{what}: no connection to an object port"
            );
            assert_eq!(rig.pending().await, [seq], "{what}");
            assert!(rig.object_file(seq).exists(), "{what}");
            let counters = rig.counters();
            assert_eq!(
                (counters.delivered, counters.passed_over, counters.set_aside),
                (0, 0, 0),
                "{what}"
            );
            // The next attempt exchanges a new token after the cap, and the real server's
            // answers then finish the entry.
            rig.until(|s| s.delivered == 1).await;
            let challenges = rig.sim.of(Route::Challenge);
            assert_eq!(challenges.len(), 2, "{what}");
            assert_eq!(gap(&challenges[0], &challenges[1]), secs(60), "{what}");
            assert_eq!(
                rig.sim.count(Route::Session),
                2,
                "{what}: the token was dropped"
            );
        }
    }
}

// R4
#[tokio::test(start_paused = true)]
async fn an_unproven_part_url_opens_no_connection_to_any_object_port() {
    for proof in Proof::UNPROVED {
        let mut rig = Rig::new();
        rig.result(1, DATA).await;
        rig.sim.queue(
            Route::Part,
            tampered(200, &part_body(sim::OBJECT_PORT), proof),
        );
        rig.start();
        rig.until(|s| s.retries == 1).await;
        assert!(rig.sim.of(Route::Put).is_empty(), "{proof:?}");
        let dialled = rig.dial.dialled.lock().unwrap().clone();
        assert!(
            dialled.iter().all(|port| *port == sim::API_PORT),
            "{proof:?}: {dialled:?}"
        );
    }
}

// R5: an unproven status sends no part and a proved one after it is believed again.
// R2: a forger that keeps saying finished delivers nothing, whatever it repeats
#[tokio::test(start_paused = true)]
async fn a_forged_finished_removes_nothing() {
    for route in [Route::Begin, Route::Status, Route::Complete] {
        let (code, body) = match route {
            Route::Begin => (200, begin_body("finished")),
            Route::Status => (200, status_body("finished")),
            _ => (200, state_body("finished")),
        };
        let mut rig = Rig::new();
        let seq = rig.result(1, DATA).await;
        for _ in 0..5 {
            rig.sim.queue(route, unproved(code, &body));
        }
        rig.start();
        rig.until(|s| s.retries == 5).await;
        assert_eq!(rig.counters().delivered, 0, "{route:?}");
        rig.stop().await;
        assert_eq!(rig.pending().await, [seq], "{route:?}");
        assert!(rig.object_file(seq).exists(), "{route:?}");
        let challenges = rig.sim.of(Route::Challenge);
        assert_eq!(challenges.len(), 5, "{route:?}: one exchange per attempt");
        for pair in challenges.windows(2) {
            assert_eq!(gap(&pair[0], &pair[1]), secs(60), "{route:?}: the cap");
        }
    }
}

// R7
#[tokio::test(start_paused = true)]
async fn two_forged_failed_completions_set_nothing_aside() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    for _ in 0..2 {
        rig.sim
            .queue(Route::Complete, unproved(409, &problem_body("failed")));
    }
    rig.start();
    rig.until(|s| s.retries == 2).await;
    let counters = rig.counters();
    assert_eq!((counters.set_aside, counters.passed_over), (0, 0));
    assert_eq!(
        rig.sim.begins().len(),
        2,
        "one begin per attempt, none to start over"
    );
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.counters().set_aside, 0);
    assert_eq!(rig.sim.puts(), [1, 2, 3], "the parts were sent once");
}

// R7, the status route's failed
#[tokio::test(start_paused = true)]
async fn two_forged_failed_statuses_set_nothing_aside_and_begin_nothing_again() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    for _ in 0..2 {
        rig.sim
            .queue(Route::Status, unproved(200, &status_body("failed")));
    }
    rig.start();
    rig.until(|s| s.retries == 2).await;
    let counters = rig.counters();
    assert_eq!((counters.set_aside, counters.passed_over), (0, 0));
    // A believed failed status begins again at once and reads the source for it; here each
    // attempt begins once and waits the cap.
    assert_eq!(rig.sim.begins().len(), 2, "one begin per attempt");
    assert!(rig.sim.puts().is_empty(), "no part was sent");
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.counters().set_aside, 0);
    assert_eq!(rig.sim.begins().len(), 3, "one begin per attempt");
    assert_eq!(rig.sim.puts(), [1, 2, 3], "the parts were sent once");
}

// R8
#[tokio::test(start_paused = true)]
async fn forged_open_completions_pass_nothing_over_and_forged_202s_repeat_nothing() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    for _ in 0..2 {
        rig.sim
            .queue(Route::Complete, unproved(409, &problem_body("open")));
    }
    rig.start();
    rig.until(|s| s.retries == 2).await;
    assert_eq!(rig.counters().passed_over, 0);
    rig.until(|s| s.delivered == 1).await;
    assert_eq!(rig.counters().passed_over, 0);

    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim
        .queue(Route::Complete, unproved(202, &state_body("completing")));
    rig.start();
    rig.until(|s| s.retries == 1).await;
    assert_eq!(rig.sim.count(Route::Complete), 1, "no repeat");
}

// R9
#[tokio::test(start_paused = true)]
async fn an_impostors_401_buys_one_exchange_and_no_result() {
    for proof in [
        Proof::Valid,
        Proof::Missing,
        Proof::NotCanonical,
        Proof::TwoValues,
    ] {
        let mut rig = Rig::new();
        rig.result(1, DATA).await;
        for _ in 0..3 {
            rig.sim.queue(
                Route::Begin,
                Reply::Json(crate::uploader::fake::Served::new(401, "{}").with(proof)),
            );
        }
        rig.start();
        rig.until(|s| s.retries == 1).await;
        use Route::*;
        assert_eq!(
            routes_of(&rig.sim),
            [Challenge, Session, Begin, Challenge, Session, Begin],
            "{proof:?}: one exchange at once, one repeat, then a retry"
        );
        let begins = rig.sim.of(Begin);
        assert_eq!(gap(&begins[0], &begins[1]), secs(0), "{proof:?}");
        assert!(begins[0].nonce.is_some() && begins[1].nonce.is_some());
        assert_ne!(
            begins[0].nonce, begins[1].nonce,
            "a new nonce for the repeat"
        );
        assert_eq!(rig.counters().delivered, 0);
    }
}

// R10
#[tokio::test(start_paused = true)]
async fn a_begin_409_is_classed_by_its_status_whatever_proof_it_carries() {
    for proof in [Proof::Valid, Proof::Missing, Proof::NotCanonical] {
        let mut rig = Rig::new();
        rig.result(1, DATA).await;
        rig.sim.queue(
            Route::Begin,
            Reply::Json(crate::uploader::fake::Served::new(409, "{}").with(proof)),
        );
        rig.start();
        rig.until(|s| s.delivered == 1).await;
        assert_eq!(rig.counters().passed_over, 1, "{proof:?}");
        assert_eq!(rig.counters().retries, 0, "{proof:?}");
    }
}

// R11
#[tokio::test(start_paused = true)]
async fn an_unproved_503_status_then_a_forged_completion_gives_one_completion_and_unproven() {
    let mut rig = Rig::new();
    let seq = rig.result(1, DATA).await;
    rig.sim.queue(Route::Status, problem(503, None));
    rig.sim
        .queue(Route::Complete, unproved(409, &problem_body("failed")));
    rig.start();
    rig.until(|s| s.retries == 1).await;
    use Route::*;
    assert_eq!(
        routes_of(&rig.sim),
        [Challenge, Session, Begin, Status, Complete]
    );
    assert_eq!(rig.pending().await, [seq]);
    assert_eq!(rig.counters().set_aside, 0);
}

// The wait is owed even when parts were stored before the forged answer.
#[tokio::test(start_paused = true)]
async fn an_unproven_answer_after_stored_parts_still_waits_the_cap_and_then_recovers() {
    let mut rig = Rig::new();
    rig.result(1, DATA).await;
    rig.sim.queue_after(
        Route::Part,
        1,
        tampered(200, &part_body(sim::OBJECT_PORT), Proof::OtherKey),
    );
    rig.start();
    rig.until(|s| s.retries == 1).await;
    assert_eq!(rig.sim.puts(), [1], "part 1 was stored before");
    rig.until(|s| s.delivered == 1).await;
    let challenges = rig.sim.of(Route::Challenge);
    assert_eq!(gap(&challenges[0], &challenges[1]), secs(60));
    assert_eq!(rig.sim.object("job-1", "result").as_deref(), Some(DATA));
}

// ---------------------------------------------------------------------------------------------
// Impostors on real sockets: I2, I2r

/// What a process that holds the server's port and is not the server says: a challenge and a
/// token with no proof, and for anything else that the upload is done.
fn lies(request: &Seen, own_put_port: u16) -> Reply {
    let line = request.request_line();
    let body = if line.contains(CHALLENGE_PATH) {
        format!(
            r#"{{"challenge":"{}"}}"#,
            STANDARD.encode(shared_crypto::random_32().unwrap())
        )
    } else if line.contains(SESSION_PATH) {
        format!(
            r#"{{"token":"{}","expiresInSecs":300}}"#,
            STANDARD.encode(shared_crypto::random_32().unwrap())
        )
    } else if line.contains("/parts/") {
        part_body(own_put_port)
    } else {
        begin_body("finished")
    };
    Reply::Bytes(json(200, &body))
}

/// What an impostor answers: `lies` to everything but a part URL, which `part_answer` answers.
/// It records what it was asked.
fn impostor_answers(
    asked: Arc<Mutex<Vec<Seen>>>,
    own_put_port: u16,
    part_answer: fn(u16) -> Reply,
) -> impl Fn(&Seen) -> Reply + Send + Sync + 'static {
    move |request| {
        asked.lock().unwrap().push(request.clone());
        if request.request_line().contains("/parts/") {
            part_answer(own_put_port)
        } else {
            lies(request, own_put_port)
        }
    }
}

/// Serves `lies` on `listener` and records what it was asked.
fn impostor(
    listener: tokio::net::TcpListener,
    own_put_port: u16,
    part_answer: fn(u16) -> Reply,
) -> (Arc<Mutex<Vec<Seen>>>, tokio::task::JoinHandle<()>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let task = tokio::spawn(serve_with(
        listener,
        impostor_answers(asked.clone(), own_put_port, part_answer),
    ));
    (asked, task)
}

fn request_lines(asked: &Mutex<Vec<Seen>>) -> Vec<String> {
    asked
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.request_line().to_owned())
        .collect()
}

// I2
#[tokio::test]
async fn an_impostor_holding_the_api_port_from_the_start_gets_one_exchange_and_no_begin() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (asked, _task) = impostor(listener, 1, |port| {
        Reply::Bytes(json(200, &part_body(port)))
    });
    // The agent registered with the real server's key, which the impostor does not hold.
    let real = Real::new(&SigningKey::generate().unwrap().public_key(), port);
    real.ledger
        .enqueue_result(jk(1), DATA.to_vec())
        .await
        .unwrap();
    let handle = real.start();
    // The exchange fails and the loop waits the cap before it dials again: the retry's count is
    // a state it stays in.
    Real::until_retries(&handle, 1).await;
    let lines = request_lines(&asked);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains(CHALLENGE_PATH) && lines[1].contains(SESSION_PATH));
    assert_eq!(handle.status().borrow().delivered, 0);
    handle.stop();
    handle.stopped().await;
}

/// The agent's registration and the real server over sockets, with the first part's PUT held; then
/// the server's API port starts to be answered by an impostor, the PUT is let go, and the agent's next request
/// is the impostor's to answer. Returns once the loop has counted its retry.
struct Takeover {
    real: Real,
    handle: Handle,
    sim: Arc<Sim>,
    asked: Arc<Mutex<Vec<Seen>>>,
    /// The impostor's own PUT port, which nothing accepts on.
    own_put: std::net::TcpListener,
}

async fn takeover(part_answer: fn(u16) -> Reply) -> Takeover {
    let api = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let objects = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let (api_port, object_port) = (
        api.local_addr().unwrap().port(),
        objects.local_addr().unwrap().port(),
    );
    let sim = Sim::new(
        1000,
        object_port,
        &format!("http://127.0.0.1:{api_port}"),
        AGENT_ID,
    );
    // One listener serves the API port throughout: the real server's answers until the takeover,
    // the impostor's after it, so that no port is released and bound again.
    let own_put = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    own_put.set_nonblocking(true).unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let taken = Arc::new(AtomicBool::new(false));
    let impostor = impostor_answers(
        asked.clone(),
        own_put.local_addr().unwrap().port(),
        part_answer,
    );
    let (server, switch) = (sim.clone(), taken.clone());
    tokio::spawn(serve_with(api, move |request| {
        if switch.load(Ordering::SeqCst) {
            impostor(request)
        } else {
            server.handle(api_port, request)
        }
    }));
    tokio::spawn(serve_tcp(objects, object_port, sim.clone()));
    let gate = Arc::new(tokio::sync::Notify::new());
    sim.queue(Route::Put, Reply::Held(gate.clone()));

    let real = Real::new(&sim.instruction_key(), api_port);
    let data: Vec<u8> = (0..3500u32).map(|n| (n % 251) as u8).collect();
    real.ledger.enqueue_result(jk(1), data).await.unwrap();
    let handle = real.start();
    // The first PUT reached the server: it is the next request that the impostor will get.
    for _ in 0..3000 {
        if sim.count(Route::Put) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        sim.count(Route::Put),
        1,
        "the first part's PUT is in flight"
    );

    // Another process now answers on the server's port.
    taken.store(true, Ordering::SeqCst);
    gate.notify_one();
    Real::until_retries(&handle, 1).await;
    Takeover {
        real,
        handle,
        sim,
        asked,
        own_put,
    }
}

impl Takeover {
    async fn end(self) {
        self.handle.stop();
        self.handle.stopped().await;
    }
}

// I2r
#[tokio::test]
async fn an_impostor_that_takes_the_port_over_cannot_send_the_agents_bytes_anywhere() {
    let t = takeover(|port| Reply::Bytes(json(200, &part_body(port)))).await;
    let lines = request_lines(&t.asked);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("/parts/2"), "{lines:?}");
    let old_bearer = t.sim.of(Route::Part).last().unwrap().bearer.clone();
    assert_eq!(
        t.asked.lock().unwrap()[0]
            .header("authorization")
            .map(|v| v.trim_start_matches("Bearer ").to_owned()),
        old_bearer,
        "the impostor met the token of the real server"
    );
    assert!(
        matches!(
            t.own_put.accept().map(drop),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
        ),
        "no connection to the impostor's PUT port"
    );
    assert_eq!(t.real.ledger.pending().await.len(), 1);
    let file = t
        .real
        .dir
        .path()
        .join(format!("{AGENT_ID}.objects"))
        .join("1");
    assert!(file.exists());
    assert_eq!(t.handle.status().borrow().delivered, 0);
    t.end().await;
}

// I2r, the second answer
#[tokio::test]
async fn an_impostors_401_after_the_takeover_buys_one_exchange_and_no_further_data_request() {
    let t = takeover(|_| Reply::Bytes(answer(401, "", "{}"))).await;
    let lines = request_lines(&t.asked);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[0].contains("/parts/2"), "{lines:?}");
    assert!(lines[1].contains(CHALLENGE_PATH) && lines[2].contains(SESSION_PATH));
    assert_eq!(t.real.ledger.pending().await.len(), 1);
    assert_eq!(t.handle.status().borrow().delivered, 0);
    t.end().await;
}
