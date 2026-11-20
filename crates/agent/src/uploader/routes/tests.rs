use std::io::Cursor;

use super::*;
use crate::uploader::fake::{answer, json, scripted};

const ID: &str = "3f2a9c1e-5b7d-4e08-9a41-0c6d2b8f7a13";
const JOB: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const SHA: &str = "ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00";
const MIB5: u64 = 5 * 1024 * 1024;

fn with_reply(
    reply: Vec<u8>,
) -> (
    Routes,
    std::sync::Arc<std::sync::Mutex<Vec<crate::uploader::fake::Seen>>>,
) {
    let (dial, seen) = scripted(reply);
    let origin = ServerOrigin::parse("http://127.0.0.1:8080").unwrap();
    (Routes::new(Client::with_dial(dial), origin), seen)
}

fn id() -> UploadId {
    UploadId::parse(ID).unwrap()
}

fn declared(size_bytes: u64) -> Declaration<'static> {
    Declaration {
        job_id: JOB,
        generation: 2,
        kind: Kind::CommLog,
        size_bytes,
        sha256: SHA,
    }
}

fn upload_body(id: &str, part_bytes: u64, part_count: u64, state: &str) -> String {
    format!(
        r#"{{"uploadId":"{id}","partBytes":{part_bytes},"partCount":{part_count},"state":"{state}"}}"#
    )
}

fn failure(class: Class, why: &'static str, status: Option<u16>) -> Failure {
    Failure {
        class,
        why,
        status,
        retry_after_secs: None,
    }
}

/// Every status a route has no result for, and the class the loop gets (the same for all four).
fn unpromised_statuses() -> Vec<(u16, Class)> {
    vec![
        (401, Class::Reauthenticate),
        (301, Class::Retry),
        (302, Class::Retry),
        (307, Class::Retry),
        (308, Class::Retry),
        (408, Class::Retry),
        (429, Class::Retry),
        (502, Class::Retry),
        (503, Class::Retry),
        (504, Class::Retry),
        (400, Class::PassOver),
        (403, Class::PassOver),
        (404, Class::PassOver),
        (405, Class::PassOver),
        (410, Class::PassOver),
        (413, Class::PassOver),
        (418, Class::PassOver),
        (422, Class::PassOver),
        (500, Class::PassOver),
        (501, Class::PassOver),
        (505, Class::PassOver),
        (204, Class::PassOver),
    ]
}

#[tokio::test(start_paused = true)]
async fn begin_sends_the_declaration_with_the_bearer_and_reads_a_new_row() {
    let (routes, seen) = with_reply(json(201, &upload_body(ID, MIB5, 3, "open")));
    let begun = routes.begin("tok", &declared(MIB5 * 2 + 1)).await.unwrap();
    assert_eq!(
        begun,
        Begun {
            id: id(),
            part_bytes: MIB5,
            part_count: 3,
            state: State::Open
        }
    );
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0].request_line(),
        "POST /api/v1/agent/uploads HTTP/1.1"
    );
    assert_eq!(
        seen[0].header("authorization").as_deref(),
        Some("Bearer tok")
    );
    assert_eq!(seen[0].header("host").as_deref(), Some("127.0.0.1:8080"));
    let body: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "jobId": JOB, "generation": 2, "kind": "commLog",
            "sizeBytes": MIB5 * 2 + 1, "sha256": SHA,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn begin_reads_the_row_of_the_same_declaration_in_any_state_but_failed() {
    for (state, want) in [
        ("open", State::Open),
        ("completing", State::Completing),
        ("finished", State::Finished),
    ] {
        let (routes, _) = with_reply(json(200, &upload_body(ID, MIB5, 1, state)));
        assert_eq!(routes.begin("t", &declared(10)).await.unwrap().state, want);
    }
    for (status, state) in [
        (200, "failed"),
        (200, "other"),
        (201, "finished"),
        (201, "completing"),
    ] {
        let (routes, _) = with_reply(json(status, &upload_body(ID, MIB5, 1, state)));
        assert_eq!(
            routes.begin("t", &declared(10)).await.unwrap_err(),
            failure(Class::PassOver, "answer", Some(status)),
            "{status} {state}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn begin_judges_the_count_against_its_own_length() {
    // The count the agent computes: max(1, ceil(length / partBytes)).
    for (length, part_bytes, count, fine) in [
        (0, MIB5, 1, true),
        (0, MIB5, 0, false),
        (MIB5, MIB5, 1, true),
        (MIB5 + 1, MIB5, 2, true),
        (MIB5 + 1, MIB5, 1, false),
        (MIB5 + 1, MIB5, 3, false),
        (10, 0, 1, false),
        // The most parts: 10000 of one byte.
        (10_000, 1, 10_000, true),
        (10_001, 1, 10_001, false),
        (u64::MAX, 1, u64::MAX, false),
    ] {
        let (routes, _) = with_reply(json(201, &upload_body(ID, part_bytes, count, "open")));
        let got = routes.begin("t", &declared(length)).await;
        assert_eq!(got.is_ok(), fine, "{length} {part_bytes} {count}: {got:?}");
        if !fine {
            assert_eq!(
                got.unwrap_err(),
                failure(Class::PassOver, "answer", Some(201))
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn begin_refuses_an_upload_id_that_is_not_a_canonical_uuid() {
    for bad in [
        "3F2A9C1E-5B7D-4E08-9A41-0C6D2B8F7A13",
        "3f2a9c1e5b7d4e089a410c6d2b8f7a13",
        "3f2a9c1e-5b7d-4e08-9a41-0c6d2b8f7a1",
        "../x",
        "3f2a9c1e-5b7d-4e08-9a41-0c6d2b8f7a1g",
        "",
        " 3f2a9c1e-5b7d-4e08-9a41-0c6d2b8f7a13",
    ] {
        let (routes, _) = with_reply(json(201, &upload_body(bad, MIB5, 1, "open")));
        assert!(routes.begin("t", &declared(10)).await.is_err(), "{bad:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn begin_refuses_bodies_the_route_does_not_promise() {
    let big = format!(r#"{{"pad":"{}"}}"#, "a".repeat(64 * 1024));
    for body in [
        "".to_owned(),
        "not json".to_owned(),
        "[]".to_owned(),
        "null".to_owned(),
        "{}".to_owned(),
        // An extra member, a missing one, a wrong type.
        format!(r#"{{"uploadId":"{ID}","partBytes":1,"partCount":1,"state":"open","x":1}}"#),
        format!(r#"{{"uploadId":"{ID}","partBytes":1,"partCount":1}}"#),
        format!(r#"{{"uploadId":"{ID}","partBytes":"1","partCount":1,"state":"open"}}"#),
        format!(r#"{{"uploadId":"{ID}","partBytes":-1,"partCount":1,"state":"open"}}"#),
        format!(r#"{{"uploadId":"{ID}","partBytes":1.5,"partCount":1,"state":"open"}}"#),
        r#"{"uploadId":5,"partBytes":1,"partCount":1,"state":"open"}"#.to_owned(),
        big,
    ] {
        let (routes, _) = with_reply(json(201, &body));
        assert_eq!(
            routes.begin("t", &declared(1)).await.unwrap_err(),
            failure(Class::PassOver, "answer", Some(201)),
            "{:?}",
            &body[..body.len().min(60)]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn begin_classifies_every_other_status() {
    let mut table = unpromised_statuses();
    table.push((409, Class::PassOver));
    for (status, class) in table {
        let (routes, _) = with_reply(json(status, r#"{"title":"x"}"#));
        let got = routes.begin("t", &declared(1)).await.unwrap_err();
        assert_eq!(got.class, class, "{status}");
        assert_eq!(got.status, Some(status));
    }
}

#[tokio::test(start_paused = true)]
async fn retry_after_is_carried_for_429_and_503_when_usable() {
    for (status, header, want) in [
        (429, "Retry-After: 12\r\n", Some(12)),
        (503, "Retry-After: 60\r\n", Some(60)),
        (503, "Retry-After: 61\r\n", None),
        (503, "Retry-After: 0\r\n", None),
        (503, "Retry-After: soon\r\n", None),
        (503, "", None),
        (502, "Retry-After: 5\r\n", None),
    ] {
        let (routes, _) = with_reply(answer(status, header, ""));
        let got = routes.begin("t", &declared(1)).await.unwrap_err();
        assert_eq!(
            (got.class, got.retry_after_secs),
            (Class::Retry, want),
            "{status} {header}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn part_url_asks_by_path_and_applies_the_host_rule() {
    let url = r#"http://127.0.0.1:9000/bucket/key?X-Amz-Signature=abc"#;
    let (routes, seen) = with_reply(json(
        200,
        &format!(r#"{{"url":"{url}","expiresInSecs":300}}"#),
    ));
    let got = routes.part_url("tok", &id(), 3).await.unwrap();
    assert_eq!(got.expires_in_secs(), 300);
    assert_eq!(got.target().port(), 9000);
    assert_eq!(got.target().host(), "127.0.0.1:9000");
    assert_eq!(got.target().target(), "/bucket/key?X-Amz-Signature=abc");
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0].request_line(),
        format!("POST /api/v1/agent/uploads/{ID}/parts/3 HTTP/1.1")
    );
    assert_eq!(
        seen[0].header("authorization").as_deref(),
        Some("Bearer tok")
    );
    assert_eq!(seen[0].header("content-length").as_deref(), Some("0"));
}

#[tokio::test(start_paused = true)]
async fn a_part_url_that_breaks_the_rule_is_a_retry_and_other_bodies_are_unpromised() {
    for url in [
        "http://127.0.0.10:9000/x",
        "https://127.0.0.1:9000/x",
        "http://evil.example/x",
    ] {
        let (routes, _) = with_reply(json(
            200,
            &format!(r#"{{"url":"{url}","expiresInSecs":300}}"#),
        ));
        assert_eq!(
            routes.part_url("t", &id(), 1).await.unwrap_err(),
            failure(Class::Retry, "part-url-host", Some(200)),
            "{url}"
        );
    }
    for body in [
        "{}",
        "nope",
        r#"{"url":"http://127.0.0.1:9000/x"}"#,
        r#"{"url":"http://127.0.0.1:9000/x","expiresInSecs":0}"#,
        r#"{"url":"http://127.0.0.1:9000/x","expiresInSecs":300,"extra":1}"#,
        r#"{"url":5,"expiresInSecs":300}"#,
    ] {
        let (routes, _) = with_reply(json(200, body));
        assert_eq!(
            routes.part_url("t", &id(), 1).await.unwrap_err(),
            failure(Class::PassOver, "answer", Some(200)),
            "{body}"
        );
    }
    // A 201 is not what the route answers.
    let (routes, _) = with_reply(json(
        201,
        r#"{"url":"http://127.0.0.1:9000/x","expiresInSecs":300}"#,
    ));
    assert_eq!(
        routes.part_url("t", &id(), 1).await.unwrap_err().class,
        Class::PassOver
    );
    for (status, class) in unpromised_statuses()
        .into_iter()
        .chain([(409, Class::Retry)])
    {
        let (routes, _) = with_reply(json(status, "{}"));
        assert_eq!(
            routes.part_url("t", &id(), 1).await.unwrap_err().class,
            class,
            "{status}"
        );
    }
}

fn status_body(state: &str, part_bytes: u64, part_count: u64, held: Option<u64>) -> String {
    let held = held
        .map(|h| format!(r#","partsHeld":{h}"#))
        .unwrap_or_default();
    format!(r#"{{"state":"{state}","partBytes":{part_bytes},"partCount":{part_count}{held}}}"#)
}

#[tokio::test(start_paused = true)]
async fn status_reads_the_parts_held_while_open() {
    let (routes, seen) = with_reply(json(200, &status_body("open", MIB5, 3, Some(2))));
    let got = routes.status("tok", &id(), MIB5 * 3).await.unwrap();
    assert_eq!(
        got,
        Status {
            state: State::Open,
            part_bytes: MIB5,
            part_count: 3,
            parts_held: Some(2)
        }
    );
    {
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].request_line(),
            format!("GET /api/v1/agent/uploads/{ID} HTTP/1.1")
        );
        assert_eq!(
            seen[0].header("authorization").as_deref(),
            Some("Bearer tok")
        );
        assert!(seen[0].body.is_empty());
    }
    for (state, want) in [
        ("completing", State::Completing),
        ("finished", State::Finished),
        ("failed", State::Failed),
    ] {
        let (routes, _) = with_reply(json(200, &status_body(state, MIB5, 1, None)));
        let got = routes.status("t", &id(), 5).await.unwrap();
        assert_eq!((got.state, got.parts_held), (want, None));
    }
    // Held equal to the count is fine.
    let (routes, _) = with_reply(json(200, &status_body("open", MIB5, 3, Some(3))));
    assert_eq!(
        routes
            .status("t", &id(), MIB5 * 3)
            .await
            .unwrap()
            .parts_held,
        Some(3)
    );
}

#[tokio::test(start_paused = true)]
async fn status_refuses_what_the_route_does_not_promise() {
    let table: Vec<(String, u64)> = vec![
        // partsHeld above the count.
        (status_body("open", MIB5, 3, Some(4)), MIB5 * 3),
        // open without partsHeld; partsHeld without open.
        (status_body("open", MIB5, 3, None), MIB5 * 3),
        (status_body("finished", MIB5, 3, Some(1)), MIB5 * 3),
        // A count that is not the agent's own.
        (status_body("open", MIB5, 2, Some(1)), MIB5 * 3),
        (status_body("open", MIB5, 3, Some(1)), 1),
        (status_body("open", 0, 1, Some(0)), 0),
        (status_body("open", 1, 10_001, Some(0)), 10_001),
        (status_body("weird", MIB5, 1, None), 1),
        ("{}".to_owned(), 1),
        ("garbage".to_owned(), 1),
        (
            r#"{"state":"finished","partBytes":1,"partCount":1,"x":0}"#.to_owned(),
            1,
        ),
        (format!(r#"{{"pad":"{}"}}"#, "a".repeat(70_000)), 1),
    ];
    for (body, length) in table {
        let (routes, _) = with_reply(json(200, &body));
        assert_eq!(
            routes.status("t", &id(), length).await.unwrap_err(),
            failure(Class::PassOver, "answer", Some(200)),
            "{:?}",
            &body[..body.len().min(80)]
        );
    }
    for (status, class) in unpromised_statuses()
        .into_iter()
        .chain([(409, Class::PassOver)])
    {
        let (routes, _) = with_reply(json(status, "{}"));
        assert_eq!(
            routes.status("t", &id(), 1).await.unwrap_err().class,
            class,
            "{status}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn complete_answers_are_typed() {
    let problem = |state: &str| {
        format!(r#"{{"type":"about:blank","title":"x","status":409,"state":"{state}"}}"#)
    };
    let table: Vec<(u16, String, Completion)> = vec![
        (
            200,
            r#"{"state":"finished"}"#.to_owned(),
            Completion::Finished,
        ),
        (
            202,
            r#"{"state":"completing"}"#.to_owned(),
            Completion::Completing,
        ),
        (409, problem("open"), Completion::Open),
        (409, problem("failed"), Completion::Failed),
        (409, problem("finished"), Completion::Unstated),
        (409, problem("other"), Completion::Unstated),
        (409, r#"{"title":"x"}"#.to_owned(), Completion::Unstated),
        (409, "not json".to_owned(), Completion::Unstated),
        (409, String::new(), Completion::Unstated),
        (409, r#"{"state":5}"#.to_owned(), Completion::Unstated),
    ];
    for (status, body, want) in table {
        let (routes, seen) = with_reply(json(status, &body));
        assert_eq!(
            routes.complete("tok", &id()).await,
            Ok(want),
            "{status} {body}"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].request_line(),
            format!("POST /api/v1/agent/uploads/{ID}/complete HTTP/1.1")
        );
        assert_eq!(
            seen[0].header("authorization").as_deref(),
            Some("Bearer tok")
        );
    }
}

#[tokio::test(start_paused = true)]
async fn complete_refuses_what_the_route_does_not_promise() {
    for (status, body) in [
        (200, r#"{"state":"completing"}"#),
        (200, r#"{"state":"open"}"#),
        (200, "{}"),
        (200, r#"{"state":"finished","x":1}"#),
        (200, "nope"),
        (202, r#"{"state":"finished"}"#),
        (202, ""),
    ] {
        let (routes, _) = with_reply(json(status, body));
        assert_eq!(
            routes.complete("t", &id()).await.unwrap_err(),
            failure(Class::PassOver, "answer", Some(status)),
            "{status} {body}"
        );
    }
    // A 409 over the bound has no state to read.
    let (routes, _) = with_reply(json(409, &format!(r#"{{"pad":"{}"}}"#, "a".repeat(70_000))));
    assert_eq!(
        routes.complete("t", &id()).await.unwrap_err().class,
        Class::PassOver
    );
    for (status, class) in unpromised_statuses() {
        let (routes, _) = with_reply(json(status, "{}"));
        assert_eq!(
            routes.complete("t", &id()).await.unwrap_err().class,
            class,
            "{status}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_request_with_no_answer_is_a_retry() {
    let (dial, _) = scripted(Vec::new());
    dial.set(crate::uploader::fake::Mode::Refuse);
    let origin = ServerOrigin::parse("http://127.0.0.1:8080").unwrap();
    let routes = Routes::new(Client::with_dial(dial.clone()), origin);
    assert_eq!(
        routes.complete("t", &id()).await.unwrap_err(),
        failure(Class::Retry, "not-sent", None)
    );
    dial.set(crate::uploader::fake::Mode::Hang);
    assert_eq!(
        routes.status("t", &id(), 1).await.unwrap_err(),
        failure(Class::Retry, "connect-timeout", None)
    );
    // Garbage on the wire.
    let (routes2, _) = with_reply(b"junk\r\n\r\n".to_vec());
    assert_eq!(
        routes2.begin("t", &declared(1)).await.unwrap_err(),
        failure(Class::Retry, "transport", None)
    );
}

#[tokio::test(start_paused = true)]
async fn the_put_carries_no_token_and_any_failure_is_a_retry() {
    let origin = ServerOrigin::parse("http://127.0.0.1:8080").unwrap();
    let url = PartUrl {
        target: part_url::check(&origin, "http://127.0.0.1:9000/b/k?sig=1").unwrap(),
        expires_in_secs: 300,
    };
    let (routes, seen) = with_reply(answer(200, "ETag: \"x\"\r\n", ""));
    routes
        .put_part(&url, 4, Box::new(Cursor::new(b"abcd".to_vec())))
        .await
        .unwrap();
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].request_line(), "PUT /b/k?sig=1 HTTP/1.1");
        assert_eq!(seen[0].header("authorization"), None);
        assert_eq!(seen[0].header("host").as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(seen[0].body, b"abcd");
    }
    for status in [301, 400, 401, 403, 404, 409, 500, 503] {
        let (routes, _) = with_reply(answer(status, "", ""));
        assert_eq!(
            routes
                .put_part(&url, 1, Box::new(Cursor::new(vec![1])))
                .await
                .unwrap_err(),
            failure(Class::Retry, "put-refused", Some(status))
        );
    }
}

/// Nothing that holds a token, a signed URL or an answer body shows it in `{:?}`.
#[tokio::test(start_paused = true)]
async fn debug_output_shows_no_token_url_query_or_body() {
    let origin = ServerOrigin::parse("http://127.0.0.1:8080").unwrap();
    let target = part_url::check(
        &origin,
        "http://127.0.0.1:9000/bucket/key?X-Amz-Signature=hunter2",
    )
    .unwrap();
    let url = PartUrl {
        target,
        expires_in_secs: 300,
    };
    let request = crate::uploader::http::JsonRequest {
        method: Method::POST,
        target: "/api/v1/agent/uploads",
        host: "127.0.0.1:8080",
        bearer: "tok-hunter2",
        body: Some(b"body-hunter2".to_vec()),
    };
    let answer = crate::uploader::http::Answer {
        status: 200,
        retry_after: Some("7".to_owned()),
        body: crate::uploader::http::AnswerBody::Bytes(b"body-hunter2".to_vec()),
    };
    for text in [
        format!("{url:?}"),
        format!("{:?}", url.target()),
        format!("{request:?}"),
        format!("{answer:?}"),
        format!("{:?}", crate::uploader::http::AnswerBody::Unread),
    ] {
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("X-Amz"), "{text}");
        assert!(!text.contains("bucket"), "{text}");
    }
    assert!(format!("{request:?}").contains("body_len"));
}
