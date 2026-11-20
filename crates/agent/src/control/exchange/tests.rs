//! The exchange's seams used by a second caller with its own dial, port and log texts: the
//! uploader's (ADR-493 item 10). The control channel's own behaviour is `control/tests.rs`.

use std::sync::{Arc, Mutex};

use shared_crypto::SigningKey;
use tokio::io::AsyncWriteExt;

use super::*;
use crate::control::dial::AtPort;
use crate::uploader::fake::{FakeDial, Seen, json, read_request};

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

/// A server that answers the requests in order, with the answer `reply(path, n)` gives, and
/// records them.
fn server(
    reply: impl Fn(&str, usize) -> Vec<u8> + Send + Sync + 'static,
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
            let n = {
                let mut log = log.lock().unwrap();
                log.push((port, request));
                log.len() - 1
            };
            let _ = stream.write_all(&reply(&path, n)).await;
            let _ = stream.shutdown().await;
        });
    });
    (dial, seen)
}

async fn run(dial: &Arc<FakeDial>, log: &mut Uploader) -> Result<Token, Fail> {
    let at = AtPort {
        dial: &**dial,
        port: 9000,
    };
    let signer = SigningKey::generate().unwrap();
    let audience = Audience::parse_canonical(ORIGIN).unwrap();
    token(
        &Seams {
            dial: &at,
            signer: &signer,
            random: shared_crypto::random_32,
            agent_id: AGENT_ID,
            audience: &audience,
            host: "127.0.0.1:9000",
        },
        log,
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn a_second_caller_gets_a_token_at_its_own_port_and_logs_in_its_own_words() {
    let token_value = value();
    let issued = token_value.clone();
    let (dial, seen) = server(move |path, _| match path {
        CHALLENGE_PATH => json(200, &format!(r#"{{"challenge":"{}"}}"#, value())),
        SESSION_PATH => json(
            200,
            &format!(r#"{{"token":"{issued}","expiresInSecs":300}}"#),
        ),
        other => panic!("unexpected path {other}"),
    });
    let mut log = Uploader::default();
    let got = run(&dial, &mut log).await.unwrap();
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
    // A 401 once: a new challenge, then a refusal.
    let (dial, _) = server(|path, n| match (path, n) {
        (CHALLENGE_PATH, _) => json(200, &format!(r#"{{"challenge":"{}"}}"#, value())),
        (SESSION_PATH, 1) => json(401, "{}"),
        _ => json(403, "{}"),
    });
    let mut log = Uploader::default();
    assert_eq!(run(&dial, &mut log).await.unwrap_err(), Fail::Cap);
    assert_eq!(
        log.lines,
        ["uploader stale", "uploader refusal session-refused 403"]
    );

    // A challenge answer that is not a challenge.
    let (dial, _) = server(|_, _| json(200, r#"{"challenge":"short"}"#));
    let mut log = Uploader::default();
    assert_eq!(run(&dial, &mut log).await.unwrap_err(), Fail::Ord);
    assert_eq!(log.lines, ["uploader failure challenge-answer Some(200)"]);

    // No server at all.
    let (dial, _) = server(|_, _| Vec::new());
    dial.set(crate::uploader::fake::Mode::Refuse);
    let mut log = Uploader::default();
    assert_eq!(run(&dial, &mut log).await.unwrap_err(), Fail::Ord);
    assert_eq!(log.lines, ["uploader failure challenge-transport None"]);
}
