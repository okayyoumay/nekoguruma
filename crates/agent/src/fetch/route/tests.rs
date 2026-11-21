//! The download route's rows: the request, the strict answer and the classes (A3 to A7 of the
//! agent's case table), over the in-memory dial on a paused clock.

use tokio::io::{AsyncWriteExt, DuplexStream};

use super::*;
use crate::uploader::fake::{FakeDial, Mode, answer, json, read_request, scripted};

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

fn good_url() -> String {
    r#"{"url":"http://127.0.0.1:9000/bucket/key?X-Amz-Signature=s","expiresInSecs":60}"#.to_owned()
}

async fn ask(reply: Vec<u8>) -> Result<PartTarget, Failure> {
    let (dial, _) = scripted(reply);
    Downloads::new(Client::with_dial(dial), origin())
        .url("tok", &want())
        .await
}

#[tokio::test(start_paused = true)]
async fn the_request_is_a_post_with_the_bearer_and_the_four_members() {
    let (dial, seen) = scripted(json(200, &good_url()));
    let target = Downloads::new(Client::with_dial(dial.clone()), origin())
        .url("tok-1", &want())
        .await
        .unwrap();
    assert_eq!((target.port(), target.host()), (9000, "127.0.0.1:9000"));
    assert_eq!(target.target(), "/bucket/key?X-Amz-Signature=s");
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
    Downloads::new(Client::with_dial(dial), origin())
        .url("t", &artifact)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&seen.lock().unwrap()[1].body).unwrap();
    assert_eq!(body["kind"], "artifact");
}

/// A5: the refusals the same request will meet again end the fetch.
#[tokio::test(start_paused = true)]
async fn refusals_are_passed_over() {
    for status in [400u16, 404, 413, 500, 403, 418] {
        let failure = ask(json(status, r#"{"title":"x"}"#)).await.unwrap_err();
        assert_eq!(
            (status, failure.class, failure.status),
            (status, Class::PassOver, Some(status))
        );
    }
}

/// A5: an answer the route does not promise.
#[tokio::test(start_paused = true)]
async fn an_answer_that_is_not_promised_is_passed_over() {
    let big = format!(
        r#"{{"url":"http://127.0.0.1:9000/{}","expiresInSecs":60}}"#,
        "a".repeat(70_000)
    );
    let cases = [
        r#"{"url":"http://127.0.0.1:9000/x","expiresInSecs":60,"extra":1}"#.to_owned(),
        r#"{"expiresInSecs":60}"#.to_owned(),
        r#"{"url":"http://127.0.0.1:9000/x"}"#.to_owned(),
        r#"{"url":"http://127.0.0.1:9000/x","expiresInSecs":0}"#.to_owned(),
        r#"{"url":5,"expiresInSecs":60}"#.to_owned(),
        "not json".to_owned(),
        String::new(),
        big,
    ];
    for body in cases {
        let failure = ask(json(200, &body)).await.unwrap_err();
        assert_eq!(
            (failure.class, failure.status),
            (Class::PassOver, Some(200)),
            "{}",
            body.len()
        );
    }
    // 201 is not a status of this route.
    let failure = ask(json(201, &good_url())).await.unwrap_err();
    assert_eq!(failure.class, Class::PassOver);
}

/// A4: a 401 is the class that exchanges a new token.
#[tokio::test(start_paused = true)]
async fn a_401_asks_for_a_new_token() {
    let failure = ask(json(401, "{}")).await.unwrap_err();
    assert_eq!(failure.class, Class::Reauthenticate);
}

/// A6: the statuses that are retried, and the usable `Retry-After`.
#[tokio::test(start_paused = true)]
async fn unavailable_statuses_are_retried_and_retry_after_is_kept() {
    for status in [408u16, 429, 502, 503, 504, 302, 307] {
        let failure = ask(json(status, "{}")).await.unwrap_err();
        assert_eq!((status, failure.class), (status, Class::Retry), "{status}");
    }
    for (header, want) in [
        ("Retry-After: 7\r\n", Some(7)),
        ("Retry-After: 0\r\n", None),
        ("Retry-After: 61\r\n", None),
        ("", None),
    ] {
        let failure = ask(answer(503, header, "{}")).await.unwrap_err();
        assert_eq!(failure.retry_after_secs, want, "{header}");
    }
}

/// A6: no answer, a cut and no progress are retries; nothing about them is final.
#[tokio::test(start_paused = true)]
async fn a_dead_server_is_retried() {
    let (dial, _) = scripted(Vec::new());
    dial.set(Mode::Refuse);
    let failure = Downloads::new(Client::with_dial(dial.clone()), origin())
        .url("t", &want())
        .await
        .unwrap_err();
    assert_eq!((failure.class, failure.why), (Class::Retry, "not-sent"));
    dial.set(Mode::Hang);
    let failure = Downloads::new(Client::with_dial(dial), origin())
        .url("t", &want())
        .await
        .unwrap_err();
    assert_eq!(failure.class, Class::Retry);

    let silent = FakeDial::new(4096, |_, mut server: DuplexStream| {
        tokio::spawn(async move {
            let _ = read_request(&mut server).await;
            let _ = server.write_all(b"HTTP/1.1 200 X\r\nContent-Le").await;
            let _ = server.shutdown().await;
        });
    });
    let failure = Downloads::new(Client::with_dial(silent), origin())
        .url("t", &want())
        .await
        .unwrap_err();
    assert_eq!((failure.class, failure.why), (Class::Retry, "transport"));
}

/// A7: a URL that breaks the host rule is a retry, whatever the breach.
#[tokio::test(start_paused = true)]
async fn a_url_that_breaks_the_host_rule_is_a_retry() {
    let long = format!("http://127.0.0.1:9000/{}", "a".repeat(5000));
    for url in [
        "https://127.0.0.1:9000/x",
        "http://127.0.0.2:9000/x",
        "http://user@127.0.0.1:9000/x",
        "http://127.0.0.1.example:9000/x",
        "http://127.0.0.1:9000/x#frag",
        "http://127.0.0.1:09000/x",
        "http://127.0.0.1:70000/x",
        "http://127.0.0.1:9000",
        long.as_str(),
    ] {
        let body = serde_json::json!({"url": url, "expiresInSecs": 60}).to_string();
        let failure = ask(json(200, &body)).await.unwrap_err();
        assert_eq!(
            (failure.class, failure.why),
            (Class::Retry, "download-url-host"),
            "{}",
            url.len()
        );
    }
}
