//! The exchange's seams used by a second caller with its own dial, port and log texts: the
//! uploader's (ADR-493 item 10), and what the exchange requires of the server's answer proof
//! (ADR-504). The control channel's own behaviour is `control/tests.rs`.

use std::sync::{Arc, Mutex};

use shared_crypto::SigningKey;
use tokio::io::AsyncWriteExt;

use super::*;
use crate::answer_proof::TestProver;
use crate::control::dial::AtPort;
use crate::uploader::fake::{FakeDial, Seen, answer, json, proved_json, read_request};

const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const ORIGIN: &str = "http://127.0.0.1:9000";

/// A log with words of its own, which records every call.
#[derive(Default)]
struct Uploader {
    lines: Vec<String>,
}

impl Log for Uploader {
    fn failure(&mut self, class: &'static str, status: Option<u16>) {
        self.lines
            .push(format!("uploader failure {class} {status:?}"));
    }

    fn refusal(&mut self, class: &'static str, status: u16) {
        self.lines
            .push(format!("uploader refusal {class} {status}"));
    }

    fn unproven(&mut self, class: &'static str, status: u16) {
        self.lines
            .push(format!("uploader unproven {class} {status}"));
    }

    fn accepted(&mut self) {
        self.lines.push("uploader accepted".to_owned());
    }

    fn stale_challenge(&mut self) {
        self.lines.push("uploader stale".to_owned());
    }
}

fn value() -> String {
    STANDARD.encode(shared_crypto::random_32().unwrap())
}

/// The requests a server read, with the port each came to.
type Served = Vec<(u16, Seen)>;

/// A server that answers the requests in order, with the answer `reply(path, n, request)`
/// gives, and records them.
fn server(
    reply: impl Fn(&str, usize, &Seen) -> Vec<u8> + Send + Sync + 'static,
) -> (Arc<FakeDial>, Arc<Mutex<Served>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let reply = Arc::new(reply);
    let dial = FakeDial::new(64 * 1024, move |port, mut stream| {
        let (log, reply) = (log.clone(), reply.clone());
        tokio::spawn(async move {
            let Some(request) = read_request(&mut stream).await else {
                return;
            };
            let path = request.request_line().split(' ').nth(1).unwrap().to_owned();
            let answered = {
                let mut log = log.lock().unwrap();
                log.push((port, request.clone()));
                reply(&path, log.len() - 1, &request)
            };
            let _ = stream.write_all(&answered).await;
            let _ = stream.shutdown().await;
        });
    });
    (dial, seen)
}

/// The server's key, which the agent's record names as the instruction key.
fn prover() -> TestProver {
    TestProver::new(Arc::new(SigningKey::generate().unwrap()), ORIGIN, AGENT_ID)
}

async fn run(dial: &Arc<FakeDial>, prover: &TestProver, log: &mut Uploader) -> Result<Token, Fail> {
    let at = AtPort {
        dial: &**dial,
        port: 9000,
    };
    let signer = SigningKey::generate().unwrap();
    let verifier = Verifier {
        key: shared_crypto::Signer::public_key(&*prover.key),
        audience: prover.audience.clone(),
        agent_id: AGENT_ID.to_owned(),
    };
    token(
        &Seams {
            dial: &at,
            signer: &signer,
            random: shared_crypto::random_32,
            verifier: &verifier,
            host: "127.0.0.1:9000",
        },
        log,
    )
    .await
}

fn token_json(token: &str) -> String {
    format!(r#"{{"token":"{token}","expiresInSecs":300}}"#)
}

/// A server that answers the challenge as it should and the session request with
/// `session(prover, request)`.
fn session_server(
    prover: &Arc<TestProver>,
    session: impl Fn(&TestProver, &Seen) -> Vec<u8> + Send + Sync + 'static,
) -> (Arc<FakeDial>, Arc<Mutex<Served>>) {
    let prover = prover.clone();
    server(move |path, _, request| match path {
        CHALLENGE_PATH => json(200, &format!(r#"{{"challenge":"{}"}}"#, value())),
        SESSION_PATH => session(&prover, request),
        other => panic!("unexpected path {other}"),
    })
}

#[tokio::test(start_paused = true)]
async fn a_second_caller_gets_a_token_at_its_own_port_and_logs_in_its_own_words() {
    let token_value = value();
    let issued = token_value.clone();
    let prover = Arc::new(prover());
    let (dial, seen) = session_server(&prover, move |prover, request| {
        proved_json(prover, request, 200, &token_json(&issued))
    });
    let mut log = Uploader::default();
    let got = run(&dial, &prover, &mut log).await.unwrap();
    assert_eq!(got.as_str(), token_value);
    assert_eq!(log.lines, ["uploader accepted"]);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        seen.iter().all(|(port, _)| *port == 9000),
        "the dial's port is used"
    );
    let session: serde_json::Value = serde_json::from_slice(&seen[1].1.body).unwrap();
    assert_eq!(session["agentId"], AGENT_ID);
    assert_eq!(seen[1].1.header("host").as_deref(), Some("127.0.0.1:9000"));
}

#[tokio::test(start_paused = true)]
async fn a_second_caller_hears_a_stale_challenge_a_failure_and_a_refusal() {
    let prover = prover();
    // A 401 once: a new challenge, then a refusal.
    let (dial, _) = server(|path, n, _| match (path, n) {
        (CHALLENGE_PATH, _) => json(200, &format!(r#"{{"challenge":"{}"}}"#, value())),
        (SESSION_PATH, 1) => json(401, "{}"),
        _ => json(403, "{}"),
    });
    let mut log = Uploader::default();
    assert_eq!(run(&dial, &prover, &mut log).await.unwrap_err(), Fail::Cap);
    assert_eq!(
        log.lines,
        ["uploader stale", "uploader refusal session-refused 403"]
    );

    // A challenge answer that is not a challenge.
    let (dial, _) = server(|_, _, _| json(200, r#"{"challenge":"short"}"#));
    let mut log = Uploader::default();
    assert_eq!(run(&dial, &prover, &mut log).await.unwrap_err(), Fail::Ord);
    assert_eq!(log.lines, ["uploader failure challenge-answer Some(200)"]);

    // No server at all.
    let (dial, _) = server(|_, _, _| Vec::new());
    dial.set(crate::uploader::fake::Mode::Refuse);
    let mut log = Uploader::default();
    assert_eq!(run(&dial, &prover, &mut log).await.unwrap_err(), Fail::Ord);
    assert_eq!(log.lines, ["uploader failure challenge-transport None"]);
}

// ---------------------------------------------------------------------------------------------
// The server's answer proof (ADR-504)

/// A proved token is the token, and the request it was proved for is what the agent sent: the
/// session body's nonce, no `Ngr-Nonce` header (E1, E6); the challenge request carries none
/// either (E5).
#[tokio::test(start_paused = true)]
async fn a_proved_session_200_gives_the_token() {
    let token_value = value();
    let issued = token_value.clone();
    let prover = Arc::new(prover());
    let (dial, seen) = session_server(&prover, move |prover, request| {
        proved_json(prover, request, 200, &token_json(&issued))
    });
    let mut log = Uploader::default();
    let got = run(&dial, &prover, &mut log).await.unwrap();
    assert_eq!(got.as_str(), token_value);
    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|(_, s)| s.header("ngr-nonce").is_none()));
}

/// Every way the session 200 can fail to be proved gives no token, waits the cap, logs one
/// `session-unproven` line and never reads the body: its token is not the one the agent then
/// holds (E2).
#[tokio::test(start_paused = true)]
async fn an_unproven_session_200_is_no_token_and_waits_the_cap() {
    type Make = fn(&TestProver, &Seen) -> Vec<u8>;
    let cases: Vec<(&str, Make)> = vec![
        ("missing", |_, _| json(200, &token_json(&value()))),
        ("another key", |_, request| {
            proved_json(&prover(), request, 200, &token_json(&value()))
        }),
        ("another nonce", |p, request| {
            let text = token_json(&value());
            let proof = p.proof(
                shared_crypto::random_32().unwrap(),
                "POST",
                SESSION_PATH,
                &request.body,
                200,
                text.as_bytes(),
            );
            answer(
                200,
                &format!("Content-Type: application/json\r\nNgr-Server-Proof: {proof}\r\n"),
                &text,
            )
        }),
        ("body altered", |p, request| {
            let proof = p.proof_of_request(
                "POST",
                SESSION_PATH,
                None,
                &request.body,
                200,
                token_json(&value()).as_bytes(),
            );
            answer(
                200,
                &format!("Content-Type: application/json\r\nNgr-Server-Proof: {proof}\r\n"),
                &token_json(&value()),
            )
        }),
        ("status altered", |p, request| {
            let text = token_json(&value());
            let proof = p.proof_of_request(
                "POST",
                SESSION_PATH,
                None,
                &request.body,
                202,
                text.as_bytes(),
            );
            answer(
                200,
                &format!("Content-Type: application/json\r\nNgr-Server-Proof: {proof}\r\n"),
                &text,
            )
        }),
        ("another request's proof", |p, request| {
            let text = token_json(&value());
            let proof = p.proof_of_request(
                "GET",
                "/api/v1/agent/control",
                Some(&request_nonce(request)),
                &[],
                200,
                text.as_bytes(),
            );
            answer(
                200,
                &format!("Content-Type: application/json\r\nNgr-Server-Proof: {proof}\r\n"),
                &text,
            )
        }),
        ("two valid values", |p, request| {
            let text = token_json(&value());
            let proof = p.proof_of_request(
                "POST",
                SESSION_PATH,
                None,
                &request.body,
                200,
                text.as_bytes(),
            );
            answer(
                200,
                &format!(
                    "Content-Type: application/json\r\nNgr-Server-Proof: {proof}\r\nNgr-Server-Proof: {proof}\r\n"
                ),
                &text,
            )
        }),
        ("not canonical base64", |p, request| {
            let text = token_json(&value());
            let proof = p.proof_of_request(
                "POST",
                SESSION_PATH,
                None,
                &request.body,
                200,
                text.as_bytes(),
            );
            let loose = proof.trim_end_matches('=');
            answer(
                200,
                &format!("Content-Type: application/json\r\nNgr-Server-Proof: {loose}\r\n"),
                &text,
            )
        }),
        ("an answer over the limit", |p, request| {
            let text = format!(r#"{{"pad":"{}"}}"#, "a".repeat(70_000));
            proved_json(p, request, 200, &text)
        }),
    ];
    for (name, make) in cases {
        let prover = Arc::new(prover());
        let (dial, seen) = session_server(&prover, make);
        let mut log = Uploader::default();
        assert_eq!(
            run(&dial, &prover, &mut log).await.unwrap_err(),
            Fail::Cap,
            "{name}"
        );
        assert_eq!(
            log.lines,
            ["uploader unproven session-unproven 200"],
            "{name}"
        );
        assert_eq!(seen.lock().unwrap().len(), 2, "{name}: nothing follows");
    }
}

/// The nonce a session request's body carries, as text.
fn request_nonce(request: &Seen) -> String {
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    body["nonce"].as_str().unwrap().to_owned()
}

/// A session answer that is wrong in shape is the ordinary failure when it is proved (E3), and
/// one that is not a result is classed by its status, proved or not (E4).
#[tokio::test(start_paused = true)]
async fn only_a_200_needs_the_proof_and_a_proved_200_of_the_wrong_shape_is_ordinary() {
    let prover = Arc::new(prover());
    let (dial, _) = session_server(&prover, |p, request| {
        proved_json(p, request, 200, r#"{"token":"short","expiresInSecs":1}"#)
    });
    let mut log = Uploader::default();
    assert_eq!(run(&dial, &prover, &mut log).await.unwrap_err(), Fail::Ord);
    assert_eq!(log.lines, ["uploader failure session-answer Some(200)"]);

    for (status, how, line) in [
        (403u16, Fail::Cap, "uploader refusal session-refused 403"),
        (
            400,
            Fail::Ord,
            "uploader failure session-bad-request Some(400)",
        ),
        (500, Fail::Ord, "uploader failure session-answer Some(500)"),
    ] {
        // Once with a garbage proof header and once with none: neither is looked at.
        for extra in ["Ngr-Server-Proof: !!\r\n", ""] {
            let (dial, _) = session_server(&prover, move |_, _| {
                answer(
                    status,
                    &format!("Content-Type: application/json\r\n{extra}"),
                    "{}",
                )
            });
            let mut log = Uploader::default();
            assert_eq!(run(&dial, &prover, &mut log).await.unwrap_err(), how);
            assert_eq!(log.lines, [line]);
        }
    }
}
