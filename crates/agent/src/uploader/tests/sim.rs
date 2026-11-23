//! A scripted data channel server for the loop's tests: the exchange and the four routes with
//! the rows' states as the server keeps them, and a PUT endpoint of its own port that speaks TLS
//! under a certificate of its own, whose pin the part-URL answers carry (ADR-505). A test queues
//! canned answers per route, may tap each request, and may change who answers on the object port
//! ([`Peer`]). It serves over the in-memory dial and, for the one real-socket test, over TCP.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ngr_pinned_tls::CertPin;
use ngr_pinned_tls::test_server::{Acceptor, Identity, Key};
use sha2::{Digest as _, Sha256};
use shared_crypto::{PublicKey, Signer as _, SigningKey};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

use crate::answer_proof::TestProver;
use crate::control::exchange::{CHALLENGE_PATH, SESSION_PATH};
use crate::uploader::fake::{
    FakeDial, Proof, Seen, Served, ServerEnd, accept_with, answer, json, proved_json, read_request,
};
use crate::uploader::routes::BEGIN_PATH;

/// The port of the API routes and the port of the object PUTs, over the in-memory dial.
pub(super) const API_PORT: u16 = 8080;
pub(super) const OBJECT_PORT: u16 = 9000;

/// Who answers on the object port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Peer {
    /// The object server: the certificate the part-URL answers carry the pin of.
    Real,
    /// Another process that took the port: TLS under a certificate of its own.
    Another,
    /// A process that has the object server's certificate (it is public) and not its key.
    WithoutKey,
    /// A process that speaks plain HTTP and answers 200 to whatever it first reads.
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Route {
    Challenge,
    Session,
    Begin,
    Status,
    Part,
    Put,
    Complete,
}

/// What the server does with a request.
pub(super) enum Reply {
    /// These bytes as they are.
    Bytes(Vec<u8>),
    /// An answer proved (or not) when it is served, from the request it answers.
    Json(Served),
    /// Reads the request and never answers.
    Hang,
    /// Reads the request and closes without an answer.
    Cut,
    /// Reads the request, waits for the notification, then answers 200 with no body (a PUT held
    /// until the test lets it end).
    Held(Arc<tokio::sync::Notify>),
    /// No override: the server's own answer (a placeholder in a queue).
    Default,
}

/// A JSON answer, proved as the server proves one.
pub(super) fn status(code: u16, body: &str) -> Reply {
    Reply::Json(Served::new(code, body))
}

/// A JSON answer with no proof.
pub(super) fn unproved(code: u16, body: &str) -> Reply {
    Reply::Json(Served::new(code, body).unproved())
}

/// A JSON answer whose proof is wrong in the way `proof` says.
pub(super) fn tampered(code: u16, body: &str, proof: Proof) -> Reply {
    Reply::Json(Served::new(code, body).with(proof))
}

pub(super) fn problem(code: u16, state: Option<&str>) -> Reply {
    match state {
        Some(state) => status(code, &format!(r#"{{"title":"x","state":"{state}"}}"#)),
        None => status(code, r#"{"title":"x"}"#),
    }
}

/// One request the server saw.
#[derive(Debug, Clone)]
pub(super) struct Logged {
    pub(super) route: Route,
    pub(super) port: u16,
    pub(super) at: Instant,
    pub(super) bearer: Option<String>,
    /// The request's `Ngr-Nonce`.
    pub(super) nonce: Option<String>,
    /// Begin: `job/kind`; part and PUT: the part number; otherwise empty.
    pub(super) detail: String,
    pub(super) body_len: usize,
}

struct Row {
    job: String,
    generation: u64,
    kind: String,
    id: String,
    state: &'static str,
    length: u64,
    part_bytes: u64,
    part_count: u64,
    sha: String,
    parts: BTreeMap<u64, Vec<u8>>,
}

struct State {
    prover: TestProver,
    rows: Vec<Row>,
    next_row: u64,
    tokens: u32,
    /// Tokens below this number are no longer good.
    min_valid: u32,
    canned: HashMap<Route, VecDeque<Reply>>,
    log: Vec<Logged>,
    part_bytes: u64,
    object_port: u16,
    /// The object server's certificate (DER) and its pin; the answers of the part-URL route
    /// carry the pin.
    certificate: Vec<u8>,
    acceptor: Acceptor,
    pin: CertPin,
    peer: Peer,
    /// Replace the certificate right after the next part-URL answer, so that the URL just given
    /// is for a certificate the object server no longer has.
    rotate_after_part_url: bool,
    /// Every byte a [`Peer::Plain`] port read.
    plain_read: Vec<u8>,
}

type Tap = Arc<dyn Fn(&Logged) + Send + Sync>;

pub(super) struct Sim {
    state: Mutex<State>,
    tap: Mutex<Option<Tap>>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn token_text(n: u32) -> String {
    STANDARD.encode([n as u8; 32])
}

fn token_number(text: &str) -> Option<u32> {
    let bytes = STANDARD.decode(text).ok()?;
    (bytes.len() == 32).then(|| u32::from(bytes[0]))
}

impl Sim {
    /// A server at `origin` that signs its session answers with a key of its own, for the agent
    /// `agent_id`; the registration record must name [`Sim::instruction_key`].
    pub(super) fn new(part_bytes: u64, object_port: u16, origin: &str, agent_id: &str) -> Arc<Sim> {
        let key = Arc::new(SigningKey::generate().unwrap());
        let identity = Identity::generate(Key::P256);
        Arc::new(Sim {
            state: Mutex::new(State {
                prover: TestProver::new(key, origin, agent_id),
                rows: Vec::new(),
                next_row: 1,
                tokens: 0,
                min_valid: 1,
                canned: HashMap::new(),
                log: Vec::new(),
                part_bytes,
                object_port,
                certificate: identity.certificate().to_vec(),
                acceptor: Acceptor::new(&identity),
                pin: CertPin::of_certificate(identity.certificate()),
                peer: Peer::Real,
                rotate_after_part_url: false,
                plain_read: Vec::new(),
            }),
            tap: Mutex::new(None),
        })
    }

    /// The public half of the key the server signs with: the record's instruction key.
    pub(super) fn instruction_key(&self) -> PublicKey {
        self.state.lock().unwrap().prover.key.public_key()
    }

    /// Who answers on the object port from now on.
    pub(super) fn set_peer(&self, peer: Peer) {
        self.state.lock().unwrap().peer = peer;
    }

    /// The object server gets a new certificate right after it answers the next part URL.
    pub(super) fn regenerate_certificate_after_next_part_url(&self) {
        self.state.lock().unwrap().rotate_after_part_url = true;
    }

    /// Every byte a [`Peer::Plain`] object port read.
    pub(super) fn plain_read(&self) -> Vec<u8> {
        self.state.lock().unwrap().plain_read.clone()
    }

    /// The acceptor the object port uses now, or `None` for a peer that is not TLS.
    fn object_acceptor(&self) -> Option<Acceptor> {
        let state = self.state.lock().unwrap();
        match state.peer {
            Peer::Real => Some(state.acceptor.clone()),
            Peer::Another => Some(Acceptor::new(&Identity::generate(Key::P256))),
            Peer::WithoutKey => Some(Acceptor::presenting(
                &[&state.certificate],
                &Identity::generate(Key::P256),
            )),
            Peer::Plain => None,
        }
    }

    /// The next request of `route` is answered with `reply` instead of the default.
    pub(super) fn queue(&self, route: Route, reply: Reply) {
        self.state
            .lock()
            .unwrap()
            .canned
            .entry(route)
            .or_default()
            .push_back(reply);
    }

    /// The request of `route` after the next `skip` ones is answered with `reply`.
    pub(super) fn queue_after(&self, route: Route, skip: usize, reply: Reply) {
        for _ in 0..skip {
            self.queue(route, Reply::Default);
        }
        self.queue(route, reply);
    }

    /// Calls `tap` with every request, before it is answered.
    pub(super) fn tap(&self, tap: impl Fn(&Logged) + Send + Sync + 'static) {
        *self.tap.lock().unwrap() = Some(Arc::new(tap));
    }

    pub(super) fn log(&self) -> Vec<Logged> {
        self.state.lock().unwrap().log.clone()
    }

    pub(super) fn count(&self, route: Route) -> usize {
        self.state
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|l| l.route == route)
            .count()
    }

    /// The `job/kind` of every begin, in order.
    pub(super) fn begins(&self) -> Vec<String> {
        self.of(Route::Begin)
            .into_iter()
            .map(|l| l.detail)
            .collect()
    }

    pub(super) fn of(&self, route: Route) -> Vec<Logged> {
        self.log()
            .into_iter()
            .filter(|l| l.route == route)
            .collect()
    }

    /// The part numbers of every PUT, in order.
    pub(super) fn puts(&self) -> Vec<u64> {
        self.of(Route::Put)
            .into_iter()
            .map(|l| l.detail.parse().unwrap())
            .collect()
    }

    /// The state of the row of `job` and `kind`, when there is one.
    pub(super) fn row_state(&self, job: &str, kind: &str) -> Option<&'static str> {
        let state = self.state.lock().unwrap();
        state
            .rows
            .iter()
            .find(|r| r.job == job && r.kind == kind)
            .map(|r| r.state)
    }

    /// The bytes the row holds, when every part is there.
    pub(super) fn object(&self, job: &str, kind: &str) -> Option<Vec<u8>> {
        let state = self.state.lock().unwrap();
        let row = state.rows.iter().find(|r| r.job == job && r.kind == kind)?;
        (row.parts.len() as u64 == row.part_count)
            .then(|| row.parts.values().flatten().copied().collect())
    }

    /// A row as an earlier run left it: `held` parts of `data` stored, in `state`.
    pub(super) fn preload(
        &self,
        job: &str,
        kind: &str,
        data: &[u8],
        state: &'static str,
        held: usize,
    ) {
        let mut st = self.state.lock().unwrap();
        let part_bytes = st.part_bytes;
        let count = (data.len() as u64).div_ceil(part_bytes).max(1);
        let mut parts = BTreeMap::new();
        for n in 1..=held as u64 {
            let from = ((n - 1) * part_bytes) as usize;
            let to = data.len().min((n * part_bytes) as usize);
            parts.insert(n, data[from..to].to_vec());
        }
        let id = format!("3f2a9c1e-5b7d-4e08-9a41-{:012x}", st.next_row);
        st.next_row += 1;
        st.rows.push(Row {
            job: job.to_owned(),
            generation: 1,
            kind: kind.to_owned(),
            id,
            state,
            length: data.len() as u64,
            part_bytes,
            part_count: count,
            sha: hex(&Sha256::digest(data)),
            parts,
        });
    }

    /// Answers one request read from `port`.
    pub(super) fn handle(&self, port: u16, seen: &Seen) -> Reply {
        let line = seen.request_line().to_owned();
        let mut words = line.split(' ');
        let (method, target) = (words.next().unwrap(), words.next().unwrap());
        let path = target.split('?').next().unwrap();
        let route = {
            let state = self.state.lock().unwrap();
            if port == state.object_port && method == "PUT" {
                Route::Put
            } else if path == CHALLENGE_PATH {
                Route::Challenge
            } else if path == SESSION_PATH {
                Route::Session
            } else if path == BEGIN_PATH {
                Route::Begin
            } else if path.ends_with("/complete") {
                Route::Complete
            } else if path.contains("/parts/") {
                Route::Part
            } else {
                Route::Status
            }
        };
        let last = path.rsplit('/').next().unwrap_or_default();
        let detail = match route {
            Route::Begin => {
                let body: serde_json::Value = serde_json::from_slice(&seen.body).unwrap();
                format!(
                    "{}/{}",
                    body["jobId"].as_str().unwrap(),
                    body["kind"].as_str().unwrap()
                )
            }
            Route::Part | Route::Put => last.to_owned(),
            _ => String::new(),
        };
        let logged = Logged {
            route,
            port,
            at: Instant::now(),
            bearer: seen
                .header("authorization")
                .map(|v| v.trim_start_matches("Bearer ").to_owned()),
            nonce: seen.header("ngr-nonce"),
            detail,
            body_len: seen.body.len(),
        };
        let canned = {
            let mut state = self.state.lock().unwrap();
            state.log.push(logged.clone());
            state.canned.get_mut(&route).and_then(VecDeque::pop_front)
        };
        let tap = self.tap.lock().unwrap().clone();
        if let Some(tap) = tap {
            tap(&logged);
        }
        let reply = match canned.filter(|r| !matches!(r, Reply::Default)) {
            Some(reply) => reply,
            None => self.default(route, path, seen, &logged),
        };
        match reply {
            Reply::Json(served) => {
                Reply::Bytes(served.bytes(&self.state.lock().unwrap().prover, seen))
            }
            other => other,
        }
    }

    fn default(&self, route: Route, path: &str, seen: &Seen, logged: &Logged) -> Reply {
        let mut state = self.state.lock().unwrap();
        match route {
            Route::Challenge => Reply::Bytes(json(
                200,
                &format!(r#"{{"challenge":"{}"}}"#, STANDARD.encode([7u8; 32])),
            )),
            Route::Session => {
                state.tokens += 1;
                let token = token_text(state.tokens);
                Reply::Bytes(proved_json(
                    &state.prover,
                    seen,
                    200,
                    &format!(r#"{{"token":"{token}","expiresInSecs":300}}"#),
                ))
            }
            Route::Put => {
                let id = path.split('/').nth(2).unwrap_or_default().to_owned();
                let n: u64 = logged.detail.parse().unwrap();
                match state
                    .rows
                    .iter_mut()
                    .find(|r| r.id == id && r.state == "open")
                {
                    Some(row) => {
                        row.parts.insert(n, seen.body.clone());
                        Reply::Bytes(answer(200, "", ""))
                    }
                    None => Reply::Bytes(answer(403, "", "")),
                }
            }
            _ => {
                let good = logged
                    .bearer
                    .as_deref()
                    .and_then(token_number)
                    .is_some_and(|n| n >= state.min_valid && n <= state.tokens);
                if !good {
                    return unproved(401, "{}");
                }
                api(&mut state, route, path, seen)
            }
        }
    }
}

/// Gives the object server a new certificate.
fn rotate(state: &mut State) {
    let identity = Identity::generate(Key::P256);
    state.certificate = identity.certificate().to_vec();
    state.acceptor = Acceptor::new(&identity);
    state.pin = CertPin::of_certificate(identity.certificate());
}

fn api(state: &mut State, route: Route, path: &str, seen: &Seen) -> Reply {
    let object_port = state.object_port;
    let pin = state.pin;
    if route == Route::Begin {
        let body: serde_json::Value = serde_json::from_slice(&seen.body).unwrap();
        let (job, kind) = (
            body["jobId"].as_str().unwrap().to_owned(),
            body["kind"].as_str().unwrap().to_owned(),
        );
        let generation = body["generation"].as_u64().unwrap();
        let existing = state
            .rows
            .iter()
            .position(|r| r.job == job && r.generation == generation && r.kind == kind);
        let (code, row) = match existing {
            Some(at) if state.rows[at].state != "failed" => (200, &state.rows[at]),
            other => {
                if let Some(at) = other {
                    state.rows.remove(at);
                }
                let length = body["sizeBytes"].as_u64().unwrap();
                let part_bytes = state.part_bytes;
                let id = format!("3f2a9c1e-5b7d-4e08-9a41-{:012x}", state.next_row);
                state.next_row += 1;
                state.rows.push(Row {
                    job,
                    generation,
                    kind,
                    id,
                    state: "open",
                    length,
                    part_bytes,
                    part_count: length.div_ceil(part_bytes).max(1),
                    sha: body["sha256"].as_str().unwrap().to_owned(),
                    parts: BTreeMap::new(),
                });
                (201, state.rows.last().unwrap())
            }
        };
        return status(
            code,
            &format!(
                r#"{{"uploadId":"{}","partBytes":{},"partCount":{},"state":"{}"}}"#,
                row.id, row.part_bytes, row.part_count, row.state
            ),
        );
    }
    let id = path
        .strip_prefix(&format!("{BEGIN_PATH}/"))
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default()
        .to_owned();
    let Some(row) = state.rows.iter_mut().find(|r| r.id == id) else {
        return problem(404, None);
    };
    match route {
        Route::Status => {
            let held = if row.state == "open" {
                // The parts from 1 the store holds.
                let mut n = 0;
                while row.parts.contains_key(&(n + 1)) {
                    n += 1;
                }
                format!(r#","partsHeld":{n}"#)
            } else {
                String::new()
            };
            status(
                200,
                &format!(
                    r#"{{"state":"{}","partBytes":{},"partCount":{}{held}}}"#,
                    row.state, row.part_bytes, row.part_count
                ),
            )
        }
        Route::Part => {
            if row.state != "open" {
                return problem(409, Some(row.state));
            }
            let n = path.rsplit('/').next().unwrap();
            let answer = status(
                200,
                &format!(
                    r#"{{"url":"https://127.0.0.1:{object_port}/obj/{}/{n}?sig=s","certificatePin":"{pin}","expiresInSecs":60}}"#,
                    row.id
                ),
            );
            if state.rotate_after_part_url {
                state.rotate_after_part_url = false;
                rotate(state);
            }
            answer
        }
        Route::Complete => match row.state {
            "finished" => status(200, r#"{"state":"finished"}"#),
            "completing" => {
                row.state = "finished";
                status(200, r#"{"state":"finished"}"#)
            }
            "failed" => problem(409, Some("failed")),
            _ if row.parts.len() as u64 != row.part_count => problem(409, Some("open")),
            _ => {
                let all: Vec<u8> = row.parts.values().flatten().copied().collect();
                if hex(&Sha256::digest(&all)) == row.sha && all.len() as u64 == row.length {
                    row.state = "finished";
                    status(200, r#"{"state":"finished"}"#)
                } else {
                    row.state = "failed";
                    problem(409, Some("failed"))
                }
            }
        },
        _ => unreachable!("begin and the others are handled above"),
    }
}

/// The in-memory dial the server answers on: the object port is TLS (or whatever [`Peer`] is
/// there), the API port is plain.
pub(super) fn dial(sim: &Arc<Sim>) -> Arc<FakeDial> {
    let sim = sim.clone();
    FakeDial::new(64 * 1024, move |port, stream| {
        let sim = sim.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            if port == sim.state.lock().unwrap().object_port {
                match sim.object_acceptor() {
                    Some(acceptor) => match acceptor.accept(stream).await {
                        Ok(tls) => stream = Box::new(tls),
                        Err(_) => return,
                    },
                    None => {
                        // A plain server answers the first bytes it reads, as one that does not
                        // wait for the end of a request it cannot parse would.
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        sim.state
                            .lock()
                            .unwrap()
                            .plain_read
                            .extend_from_slice(&buf[..n]);
                        let _ = stream.write_all(&answer(200, "", "")).await;
                        let _ = stream.shutdown().await;
                        return;
                    }
                }
            }
            let Some(seen) = read_request(&mut stream).await else {
                return;
            };
            play(sim.handle(port, &seen), &mut stream).await;
        });
    })
}

/// Plays a server's answer on the connection the request came on.
async fn play<S: AsyncRead + AsyncWrite + Unpin>(reply: Reply, stream: &mut S) {
    match reply {
        Reply::Bytes(bytes) => {
            let _ = stream.write_all(&bytes).await;
            let _ = stream.shutdown().await;
        }
        Reply::Held(gate) => {
            gate.notified().await;
            let _ = stream.write_all(&answer(200, "", "")).await;
            let _ = stream.shutdown().await;
        }
        Reply::Hang => {
            let mut byte = [0u8; 1];
            while matches!(stream.read(&mut byte).await, Ok(n) if n > 0) {}
        }
        // `Json` is resolved to bytes before it gets here; `Cut` closes at once.
        Reply::Json(_) | Reply::Cut | Reply::Default => {}
    }
}

/// Serves `sim` on a real listener: one request per connection, read whole, answered, closed.
pub(super) async fn serve_tcp(listener: tokio::net::TcpListener, port: u16, sim: Arc<Sim>) {
    let tls = {
        let sim = sim.clone();
        let object_port = sim.state.lock().unwrap().object_port;
        move || {
            (port == object_port)
                .then(|| sim.object_acceptor())
                .flatten()
        }
    };
    serve_with_tls(listener, tls, move |seen| sim.handle(port, seen)).await;
}

/// Accepts connections on a real listener until the task is dropped and answers each request, read
/// whole, as `handle` says.
pub(super) async fn serve_with(
    listener: tokio::net::TcpListener,
    handle: impl Fn(&Seen) -> Reply + Send + Sync + 'static,
) {
    serve_with_tls(listener, || None, handle).await;
}

/// [`serve_with`] with a TLS handshake first on the connections `tls` gives an acceptor for.
async fn serve_with_tls(
    listener: tokio::net::TcpListener,
    tls: impl Fn() -> Option<Acceptor> + Send + Sync + 'static,
    handle: impl Fn(&Seen) -> Reply + Send + Sync + 'static,
) {
    accept_with(listener, tls, move |seen, mut stream: ServerEnd| {
        let reply = handle(&seen);
        async move { play(reply, &mut stream).await }
    })
    .await;
}
