//! The download route's rows: the request, the strict answer, the classes (A3 to A7 of the
//! agent's case table) and the server's proof of the answers that decide (ADR-504 item 3, F1 to
//! F5), over the in-memory dial on a paused clock.

use ngr_pinned_tls::CertPin;
use tokio::io::AsyncWriteExt;

use super::*;
use crate::answer_proof::Verifier;
use crate::uploader::fake::{
    FakeDial, Mode, Proof, Served, ServerEnd, proving, read_request, scripted_served, scripted_with,
};
use crate::uploader::routes::Class;

const API: u16 = 8080;
const JOB: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn origin() -> ServerOrigin {
    ServerOrigin::parse(&format!("http://127.0.0.1:{API}")).unwrap()
}

fn want() -> Want<'static> {
    Want {
        job_id: JOB,
        generation: 3,
        kind: Kind::Document,
        sha256: SHA,
    }
}

/// A pin as a proved answer carries one.
const PIN: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";

/// A download answer for `url`.
fn answer_for(url: &str) -> String {
    serde_json::json!({"url": url, "certificatePin": PIN, "expiresInSecs": 60}).to_string()
}

fn good_url() -> String {
    answer_for("https://127.0.0.1:9000/bucket/key?X-Amz-Signature=s")
}

const AGENT: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";

fn downloads(dial: std::sync::Arc<FakeDial>, verifier: std::sync::Arc<Verifier>) -> Downloads {
    Downloads::new(
        Client::with_dial(dial),
        origin(),
        verifier,
        shared_crypto::random_32,
    )
}

/// The route's answer to a request that a server answered with `reply`, proved as it says.
async fn ask(reply: Served) -> Result<PartTarget, Failure> {
    let (prover, verifier) = proving(&format!("http://127.0.0.1:{API}"), AGENT);
    let (dial, _) = scripted_served(prover, reply);
    downloads(dial, verifier).url("tok", &want()).await
}

#[tokio::test(start_paused = true)]
async fn the_request_is_a_post_with_the_bearer_and_the_four_members() {
    let (prover, verifier) = proving(&format!("http://127.0.0.1:{API}"), AGENT);
    let (dial, seen) = scripted_served(prover, Served::new(200, &good_url()));
    let route = downloads(dial.clone(), verifier);
    let target = route.url("tok-1", &want()).await.unwrap();
    assert_eq!((target.port(), target.host()), (9000, "127.0.0.1:9000"));
    assert_eq!(target.target(), "/bucket/key?X-Amz-Signature=s");
    assert_eq!(target.pin(), &CertPin::from_hex(PIN).unwrap());
    assert_eq!(*dial.dialled.lock().unwrap(), vec![API]);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].request_line(), format!("POST {PATH} HTTP/1.1"));
        assert_eq!(
            seen[0].header("authorization").as_deref(),
            Some("Bearer tok-1")
        );
        let body: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"jobId": JOB, "generation": 3, "kind": "document", "sha256": SHA})
        );
    }
    let artifact = Want {
        kind: Kind::Artifact,
        ..want()
    };
    route.url("t", &artifact).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&seen.lock().unwrap()[1].body).unwrap();
    assert_eq!(body["kind"], "artifact");
}

/// A5: the refusals the same request will meet again end the fetch.
#[tokio::test(start_paused = true)]
async fn refusals_are_passed_over() {
    for status in [400u16, 404, 413, 500, 403, 418] {
        let failure = ask(Served::new(status, r#"{"title":"x"}"#))
            .await
            .unwrap_err();
        assert_eq!(
            (status, failure.class, failure.status),
            (status, Class::PassOver, Some(status))
        );
    }
}

/// A5: an answer the route does not promise.
#[tokio::test(start_paused = true)]
async fn an_answer_that_is_not_promised_is_passed_over() {
    let url = r#""url":"https://127.0.0.1:9000/x""#;
    let pin = format!(r#""certificatePin":"{PIN}""#);
    let mut cases = vec![
        format!(r#"{{{url},{pin},"expiresInSecs":60,"extra":1}}"#),
        format!(r#"{{{pin},"expiresInSecs":60}}"#),
        format!(r#"{{{url},{pin}}}"#),
        format!(r#"{{{url},{pin},"expiresInSecs":0}}"#),
        format!(r#"{{"url":5,{pin},"expiresInSecs":60}}"#),
        "not json".to_owned(),
        String::new(),
        // The pin: missing, 63 and 65 digits, uppercase, not hexadecimal, padded, not text.
        format!(r#"{{{url},"expiresInSecs":60}}"#),
    ];
    for wrong in [
        PIN[1..].to_owned(),
        format!("{PIN}0"),
        PIN.to_uppercase(),
        PIN.replacen('a', "g", 1),
        format!(" {PIN}"),
        String::new(),
    ] {
        cases.push(format!(
            r#"{{{url},"certificatePin":"{wrong}","expiresInSecs":60}}"#
        ));
    }
    cases.push(format!(
        r#"{{{url},"certificatePin":5,"expiresInSecs":60}}"#
    ));
    for body in cases {
        let failure = ask(Served::new(200, &body)).await.unwrap_err();
        assert_eq!(
            (failure.class, failure.status),
            (Class::PassOver, Some(200)),
            "{}",
            body.len()
        );
    }
    // 201 is not a status of this route.
    let failure = ask(Served::new(201, &good_url())).await.unwrap_err();
    assert_eq!(failure.class, Class::PassOver);
}

/// A4: a 401 is the class that exchanges a new token.
#[tokio::test(start_paused = true)]
async fn a_401_asks_for_a_new_token() {
    let failure = ask(Served::new(401, "{}")).await.unwrap_err();
    assert_eq!(failure.class, Class::Reauthenticate);
}

/// A6: the statuses that are retried, and the usable `Retry-After`.
#[tokio::test(start_paused = true)]
async fn unavailable_statuses_are_retried_and_retry_after_is_kept() {
    for status in [408u16, 429, 502, 503, 504, 302, 307] {
        let failure = ask(Served::new(status, "{}")).await.unwrap_err();
        assert_eq!((status, failure.class), (status, Class::Retry), "{status}");
    }
    for (header, want) in [
        ("Retry-After: 7\r\n", Some(7)),
        ("Retry-After: 0\r\n", None),
        ("Retry-After: 61\r\n", None),
        ("", None),
    ] {
        let failure = ask(Served::new(503, "{}").with_headers(header))
            .await
            .unwrap_err();
        assert_eq!(failure.retry_after_secs, want, "{header}");
    }
}

/// A6: no answer, a cut and no progress are retries; nothing about them is final.
#[tokio::test(start_paused = true)]
async fn a_dead_server_is_retried() {
    let (_, verifier) = proving(&format!("http://127.0.0.1:{API}"), AGENT);
    let (dial, _) = scripted_with(|_| Vec::new());
    dial.set(Mode::Refuse);
    let failure = downloads(dial.clone(), verifier.clone())
        .url("t", &want())
        .await
        .unwrap_err();
    assert_eq!((failure.class, failure.why), (Class::Retry, "not-sent"));
    dial.set(Mode::Hang);
    let failure = downloads(dial, verifier.clone())
        .url("t", &want())
        .await
        .unwrap_err();
    assert_eq!(failure.class, Class::Retry);

    let silent = FakeDial::new(4096, |_, mut server: ServerEnd| {
        tokio::spawn(async move {
            let _ = read_request(&mut server).await;
            let _ = server.write_all(b"HTTP/1.1 200 X\r\nContent-Le").await;
            let _ = server.shutdown().await;
        });
    });
    let failure = downloads(silent, verifier)
        .url("t", &want())
        .await
        .unwrap_err();
    assert_eq!((failure.class, failure.why), (Class::Retry, "transport"));
}

/// A7: a URL that breaks the host rule is a retry, whatever the breach.
#[tokio::test(start_paused = true)]
async fn a_url_that_breaks_the_host_rule_is_a_retry() {
    let long = format!("https://127.0.0.1:9000/{}", "a".repeat(5000));
    for url in [
        "http://127.0.0.1:9000/x",
        "http://127.0.0.1/x",
        "HTTPS://127.0.0.1:9000/x",
        "https://127.0.0.2:9000/x",
        "https://user@127.0.0.1:9000/x",
        "https://127.0.0.1.example:9000/x",
        "https://127.0.0.1:9000/x#frag",
        "https://127.0.0.1:09000/x",
        "https://127.0.0.1:70000/x",
        "https://127.0.0.1:9000",
        long.as_str(),
    ] {
        let body = answer_for(url);
        let failure = ask(Served::new(200, &body)).await.unwrap_err();
        assert_eq!(
            (failure.class, failure.why),
            (Class::Retry, "download-url-host"),
            "{}",
            url.len()
        );
    }
}

/// The statuses the fetch ends on when the server proved them.
const REFUSALS: [u16; 7] = [400, 404, 413, 500, 403, 418, 501];

/// F3: a proved refusal is a refusal, the same for each status that ends the fetch.
#[tokio::test(start_paused = true)]
async fn a_proved_refusal_ends_the_fetch() {
    for status in REFUSALS {
        let failure = ask(Served::new(status, r#"{"title":"x"}"#))
            .await
            .unwrap_err();
        assert_eq!(
            (failure.class, failure.status),
            (Class::PassOver, Some(status))
        );
    }
}

/// F2 and F4: an answer that is not proved is never acted on, whatever it says: the 200 is not
/// a URL and a refusal is not one that ends the fetch.
#[tokio::test(start_paused = true)]
async fn an_answer_that_is_not_proved_is_unproven_whatever_its_status() {
    for proof in Proof::UNPROVED {
        for status in std::iter::once(200).chain(REFUSALS) {
            let body = if status == 200 {
                good_url()
            } else {
                r#"{"title":"x"}"#.to_owned()
            };
            let failure = ask(Served::new(status, &body).with(proof))
                .await
                .unwrap_err();
            assert_eq!(
                (failure.class, failure.why, failure.status),
                (Class::Unproven, "unproven", Some(status)),
                "{proof:?} {status}"
            );
        }
    }
}

/// F5: a 401 and the statuses that are retried are classed by status, whatever proof they carry.
#[tokio::test(start_paused = true)]
async fn statuses_that_are_not_results_are_not_checked() {
    for proof in [
        Proof::Valid,
        Proof::Missing,
        Proof::NotCanonical,
        Proof::TwoValues,
    ] {
        for (status, class) in [
            (401, Class::Reauthenticate),
            (302, Class::Retry),
            (408, Class::Retry),
            (429, Class::Retry),
            (502, Class::Retry),
            (503, Class::Retry),
            (504, Class::Retry),
        ] {
            let failure = ask(Served::new(status, "{}").with(proof))
                .await
                .unwrap_err();
            assert_eq!((failure.class, failure.status), (class, Some(status)));
        }
    }
}

/// The nonce is fresh for every request, and the proof of one request does not prove the answer
/// to another.
#[tokio::test(start_paused = true)]
async fn each_request_carries_its_own_nonce() {
    let (prover, verifier) = proving(&format!("http://127.0.0.1:{API}"), AGENT);
    let (dial, seen) = scripted_served(prover, Served::new(200, &good_url()));
    let route = downloads(dial, verifier);
    route.url("t", &want()).await.unwrap();
    route.url("t", &want()).await.unwrap();
    let seen = seen.lock().unwrap();
    let nonces: Vec<String> = seen
        .iter()
        .map(|request| request.header("ngr-nonce").expect("a nonce"))
        .collect();
    assert_ne!(nonces[0], nonces[1]);
    assert!(
        nonces
            .iter()
            .all(|n| crate::answer_proof::canonical_base64::<32>(n).is_some())
    );
}

/// A server that signs for the nonce of an earlier request (a replay) is not proved.
#[tokio::test(start_paused = true)]
async fn an_answer_replayed_from_an_earlier_request_is_unproven() {
    let (prover, verifier) = proving(&format!("http://127.0.0.1:{API}"), AGENT);
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
    let first = recorded.clone();
    let (dial, _) = scripted_with(move |request| {
        let mut held = first.lock().unwrap();
        held.get_or_insert_with(|| Served::new(200, &good_url()).bytes(&prover, request))
            .clone()
    });
    let route = downloads(dial, verifier);
    route.url("t", &want()).await.unwrap();
    let failure = route.url("t", &want()).await.unwrap_err();
    assert_eq!(failure.class, Class::Unproven);
}
