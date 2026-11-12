//! `GET /api/v1/agent/control` over real TCP (ADR-466, ADR-468): the refusals at the upgrade, the
//! connection's life (`hello`, the heartbeat echo, the idle close, the close codes), replacement
//! and the stop. The client is `tokio-tungstenite` over a stream the test opens. Tokens, keys and
//! nonces are made at run time and no assertion message prints one: values are named by what they
//! are.
//!
//! The clock moves only in jumps (`pause`, `advance`, `resume` back to back) made while the
//! server is at rest, on a current-thread runtime: a paused clock would turn every socket and
//! pool wait into an instant timeout.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use shared_crypto::random_32;
use shared_proto::ServerMessage;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Request, Response};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{WebSocketStream, client_async};

use crate::redeem::{dead, instruction_key, json};
use crate::serving::{Reply, exchange};
use crate::session::Up;

const PATH: &str = "/api/v1/agent/control";
const SUBPROTOCOL: &str = "ngr-control.v1";
const HEARTBEAT: &str = r#"{"type":"heartbeat"}"#;

type Client = WebSocketStream<TcpStream>;

// ---------------------------------------------------------------------------------------------
// Raw requests, for the refusals

fn raw(host: &str, version: &str, headers: &[String]) -> String {
    let mut text = format!("GET {PATH} {version}\r\n{host}\r\n");
    for header in headers {
        text.push_str(header);
        text.push_str("\r\n");
    }
    text.push_str("\r\n");
    text
}

/// The headers of a well-formed upgrade request with the subprotocol, answered once and closed.
fn upgrade_headers() -> Vec<String> {
    let key = STANDARD.encode(&random_32().unwrap()[..16]);
    vec![
        "Connection: close, Upgrade".to_owned(),
        "Upgrade: websocket".to_owned(),
        "Sec-WebSocket-Version: 13".to_owned(),
        format!("Sec-WebSocket-Key: {key}"),
        format!("Sec-WebSocket-Protocol: {SUBPROTOCOL}"),
    ]
}

fn without(mut headers: Vec<String>, name: &str) -> Vec<String> {
    headers.retain(|h| {
        !h.to_ascii_lowercase()
            .starts_with(&name.to_ascii_lowercase())
    });
    headers
}

fn bearer(token: &str) -> String {
    format!("Authorization: Bearer {token}")
}

fn assert_problem(r: &Reply, status: u16, what: &str) {
    assert_eq!(r.status, status, "{what}: {}", r.head);
    assert_eq!(
        r.header("content-type"),
        Some("application/problem+json"),
        "{what}"
    );
}

fn random_token() -> String {
    STANDARD.encode(random_32().unwrap())
}

/// A jump of the clock: the clock is paused only for it, with no request in flight.
async fn jump(secs: u64) {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(secs)).await;
    tokio::time::resume();
}

// ---------------------------------------------------------------------------------------------
// The client

fn request(addr: SocketAddr, authorization: Option<&str>, protocols: &[&str]) -> Request<()> {
    let mut request = format!("ws://127.0.0.1:{}{PATH}", addr.port())
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    if let Some(value) = authorization {
        headers.insert("authorization", HeaderValue::from_str(value).unwrap());
    }
    for protocol in protocols {
        headers.append(
            "sec-websocket-protocol",
            HeaderValue::from_str(protocol).unwrap(),
        );
    }
    request
}

async fn handshake(
    addr: SocketAddr,
    request: Request<()>,
) -> Result<(Client, Response<Option<Vec<u8>>>), tungstenite::Error> {
    let stream = TcpStream::connect(addr).await.unwrap();
    client_async(request, stream).await
}

/// Opens a connection with `token` and reads its `hello`.
async fn open(addr: SocketAddr, token: &str) -> Client {
    let request = request(addr, Some(&format!("Bearer {token}")), &[SUBPROTOCOL]);
    let (mut client, response) = handshake(addr, request)
        .await
        .unwrap_or_else(|error| panic!("the upgrade was refused: {error}"));
    assert_eq!(response.status(), 101);
    hello(&mut client).await;
    client
}

/// The next frame, within 10 seconds of real time.
async fn frame(client: &mut Client) -> Option<Result<Message, tungstenite::Error>> {
    tokio::time::timeout(Duration::from_secs(10), client.next())
        .await
        .expect("a frame or the end within 10 seconds")
}

/// The next text frame; pings and pongs are skipped.
async fn text(client: &mut Client) -> String {
    loop {
        match frame(client).await.expect("the stream ended") {
            Ok(Message::Text(text)) => return text.as_str().to_owned(),
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            other => panic!("not a text frame: {other:?}"),
        }
    }
}

async fn hello(client: &mut Client) {
    let message = ServerMessage::decode(&text(client).await).unwrap();
    assert_eq!(message, ServerMessage::Hello { heartbeat_secs: 20 });
}

/// Sends a text and reads the echo of a heartbeat.
async fn heartbeat_with(client: &mut Client, body: String) {
    client.send(Message::Text(body.into())).await.unwrap();
    let echo = ServerMessage::decode(&text(client).await).unwrap();
    assert_eq!(echo, ServerMessage::Heartbeat);
}

async fn heartbeat(client: &mut Client) {
    heartbeat_with(client, HEARTBEAT.to_owned()).await;
}

/// Reads until the connection ends and returns the code of the Close frame it carried.
async fn close_code(client: &mut Client) -> u16 {
    let mut code = None;
    loop {
        match frame(client).await {
            Some(Ok(Message::Close(Some(close)))) => code = Some(u16::from(close.code)),
            Some(Ok(Message::Close(None))) => panic!("a Close frame with no code"),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => break,
        }
    }
    code.expect("a Close frame before the end")
}

async fn token(u: &Up) -> String {
    let r = u.valid().await;
    assert_eq!(r.status, 200, "{}", r.head);
    json(&r)["token"].as_str().unwrap().to_owned()
}

/// A heartbeat padded with white space to exactly `len` bytes, still one valid message.
fn padded_heartbeat(len: usize) -> String {
    format!("{HEARTBEAT}{}", " ".repeat(len - HEARTBEAT.len()))
}

// ---------------------------------------------------------------------------------------------
// Refusals at the upgrade

#[tokio::test]
async fn no_valid_token_is_401_with_one_body_whatever_else_the_request_holds() {
    let s = dead(instruction_key()).await;
    let unknown = random_token();
    let cases: Vec<(&str, Vec<String>)> = vec![
        ("nothing at all", vec!["Connection: close".to_owned()]),
        ("no authorization", upgrade_headers()),
        (
            "another scheme",
            [
                upgrade_headers(),
                vec![format!("Authorization: Basic {unknown}")],
            ]
            .concat(),
        ),
        (
            "a bare scheme",
            [upgrade_headers(), vec!["Authorization: Bearer".to_owned()]].concat(),
        ),
        (
            "two headers",
            [
                upgrade_headers(),
                vec![bearer(&unknown), bearer(&random_token())],
            ]
            .concat(),
        ),
        (
            "43 characters",
            [upgrade_headers(), vec![bearer(&unknown[..43])]].concat(),
        ),
        (
            "an unknown token",
            [upgrade_headers(), vec![bearer(&unknown)]].concat(),
        ),
        (
            "an unknown token, lower case scheme",
            [
                upgrade_headers(),
                vec![format!("Authorization: bearer {unknown}")],
            ]
            .concat(),
        ),
        (
            "an unknown token and nothing else valid",
            vec!["Connection: close".to_owned(), bearer(&unknown)],
        ),
        (
            "no subprotocol and no key",
            [
                without(
                    without(upgrade_headers(), "Sec-WebSocket-Protocol"),
                    "Sec-WebSocket-Key",
                ),
                vec![bearer(&unknown)],
            ]
            .concat(),
        ),
    ];
    let mut first: Option<String> = None;
    for (name, headers) in &cases {
        let r = exchange(s.addr, &raw(&s.host(), "HTTP/1.1", headers)).await;
        assert_problem(&r, 401, name);
        assert_eq!(r.header("www-authenticate"), Some("Bearer"), "{name}");
        match &first {
            None => first = Some(r.body.clone()),
            Some(body) => assert_eq!(&r.body, body, "{name}: the body differs"),
        }
    }
    s.finish().await;
}

#[tokio::test]
async fn the_guard_and_the_method_answer_before_the_token_is_looked_at() {
    let s = dead(instruction_key()).await;
    let port = s.addr.port();
    let moved = raw(
        &format!("Host: localhost:{port}"),
        "HTTP/1.1",
        &upgrade_headers(),
    );
    assert_eq!(exchange(s.addr, &moved).await.status, 307);
    let foreign = [
        upgrade_headers(),
        vec!["Origin: http://evil.example".to_owned()],
    ]
    .concat();
    let r = exchange(s.addr, &raw(&s.host(), "HTTP/1.1", &foreign)).await;
    assert_eq!(r.status, 403, "{}", r.head);

    for method in ["POST", "PUT", "DELETE"] {
        let text = format!(
            "{method} {PATH} HTTP/1.1\r\n{}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            s.host()
        );
        let r = exchange(s.addr, &text).await;
        assert_eq!(r.status, 405, "{method}: {}", r.head);
        assert_eq!(r.header("allow"), Some("GET"), "{method}");
        assert_eq!(
            r.header("content-type"),
            Some("application/problem+json"),
            "{method}"
        );
        assert_eq!(r.header("www-authenticate"), None, "{method}");
    }
    // HEAD is refused by the same handler, before the token.
    let text = format!(
        "HEAD {PATH} HTTP/1.1\r\n{}\r\nConnection: close\r\n\r\n",
        s.host()
    );
    let r = exchange(s.addr, &text).await;
    assert_eq!(r.status, 405, "{}", r.head);
    assert_eq!(r.header("allow"), Some("GET"));
    assert_eq!(r.header("content-type"), Some("application/problem+json"));
    assert_eq!(r.header("www-authenticate"), None);
    s.finish().await;
}

#[tokio::test]
async fn an_expired_token_and_a_token_sent_twice_are_401() {
    let u = Up::new().await;
    let t = token(&u).await;
    let host = u.s.host();
    let twice = [upgrade_headers(), vec![bearer(&t), bearer(&t)]].concat();
    let r = exchange(u.s.addr, &raw(&host, "HTTP/1.1", &twice)).await;
    assert_problem(&r, 401, "two identical headers");

    // Live now, and 301 seconds on, past its 300.
    let once = [upgrade_headers(), vec![bearer(&t)]].concat();
    drop(open(u.s.addr, &t).await);
    jump(301).await;
    let r = exchange(u.s.addr, &raw(&host, "HTTP/1.1", &once)).await;
    assert_problem(&r, 401, "an expired token");
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    u.finish().await;
}

#[tokio::test]
async fn a_valid_token_without_a_proper_upgrade_is_400_and_stays_usable() {
    let u = Up::new().await;
    let t = token(&u).await;
    let host = u.s.host();
    let with_protocol = |value: &str| {
        let mut headers = without(upgrade_headers(), "Sec-WebSocket-Protocol");
        headers.push(format!("Sec-WebSocket-Protocol: {value}"));
        headers.push(bearer(&t));
        headers
    };
    let plain = vec!["Connection: close".to_owned(), bearer(&t)];
    let no_protocol = [
        without(upgrade_headers(), "Sec-WebSocket-Protocol"),
        vec![bearer(&t)],
    ]
    .concat();
    let version_12 = [
        without(upgrade_headers(), "Sec-WebSocket-Version"),
        vec!["Sec-WebSocket-Version: 12".to_owned(), bearer(&t)],
    ]
    .concat();
    let no_key = [
        without(upgrade_headers(), "Sec-WebSocket-Key"),
        vec![bearer(&t)],
    ]
    .concat();
    let ok = [upgrade_headers(), vec![bearer(&t)]].concat();
    let cases: Vec<(&str, &str, Vec<String>)> = vec![
        ("a plain GET", "HTTP/1.1", plain),
        ("no subprotocol", "HTTP/1.1", no_protocol),
        ("another case", "HTTP/1.1", with_protocol("NGR-Control.V1")),
        (
            "another version",
            "HTTP/1.1",
            with_protocol("ngr-control.v2"),
        ),
        ("quoted", "HTTP/1.1", with_protocol("\"ngr-control.v1\"")),
        ("empty", "HTTP/1.1", with_protocol("")),
        ("version 12", "HTTP/1.1", version_12),
        ("no key", "HTTP/1.1", no_key),
        ("HTTP/1.0", "HTTP/1.0", ok),
    ];
    for (name, version, headers) in &cases {
        let r = exchange(u.s.addr, &raw(&host, version, headers)).await;
        assert_problem(&r, 400, name);
        assert_eq!(r.header("www-authenticate"), None, "{name}");
    }
    // None of them used the token up.
    drop(open(u.s.addr, &t).await);
    u.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The connection

#[tokio::test]
async fn the_subprotocol_is_taken_from_a_list_and_from_two_headers_and_hello_comes_first() {
    let u = Up::new().await;
    let t = token(&u).await;
    let addr = u.s.addr;
    let offers: [(&str, Vec<&str>, String); 3] = [
        (
            "a list",
            vec!["other, ngr-control.v1"],
            format!("Bearer {t}"),
        ),
        (
            "the name twice",
            vec![SUBPROTOCOL, SUBPROTOCOL],
            format!("Bearer {t}"),
        ),
        // The scheme's case does not matter.
        (
            "lower case scheme",
            vec![SUBPROTOCOL],
            format!("bearer {t}"),
        ),
    ];
    for (name, protocols, authorization) in &offers {
        let request = request(addr, Some(authorization), protocols);
        let (mut client, response) = handshake(addr, request)
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(response.status(), 101, "{name}");
        assert_eq!(
            response.headers().get("sec-websocket-protocol"),
            Some(&HeaderValue::from_static(SUBPROTOCOL)),
            "{name}"
        );
        assert!(
            response.headers().get("sec-websocket-extensions").is_none(),
            "{name}"
        );
        hello(&mut client).await;
    }

    // Two headers, the first of another protocol. The client library judges only the first
    // header of the answer, so this one is written by hand.
    let mut headers = without(
        without(upgrade_headers(), "Sec-WebSocket-Protocol"),
        "Connection",
    );
    headers.extend([
        "Connection: Upgrade".to_owned(),
        "Sec-WebSocket-Protocol: other".to_owned(),
        format!("Sec-WebSocket-Protocol: {SUBPROTOCOL}"),
        bearer(&t),
    ]);
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let text = raw(&u.s.host(), "HTTP/1.1", &headers);
    stream.write_all(text.as_bytes()).await.unwrap();
    let head = read_head(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("sec-websocket-protocol: ngr-control.v1\r\n"),
        "{head}"
    );
    // The first frame is an unmasked text frame short enough for a one byte length.
    let mut prefix = [0u8; 2];
    read_exactly(&mut stream, &mut prefix).await;
    assert_eq!(prefix[0], 0x81);
    let mut payload = vec![0u8; usize::from(prefix[1] & 0x7f)];
    read_exactly(&mut stream, &mut payload).await;
    let hello = ServerMessage::decode(std::str::from_utf8(&payload).unwrap()).unwrap();
    assert_eq!(hello, ServerMessage::Hello { heartbeat_secs: 20 });
    drop(stream);
    u.finish().await;
}

/// Reads a response head, up to its blank line, within 10 seconds.
async fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let read = async {
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .expect("the response head within 10 seconds");
    String::from_utf8_lossy(&head).into_owned()
}

async fn read_exactly(stream: &mut TcpStream, buf: &mut [u8]) {
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(buf))
        .await
        .expect("bytes within 10 seconds")
        .unwrap();
}

#[tokio::test]
async fn a_connection_outlives_the_token_that_opened_it() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    // Seven heartbeats 50 s apart take the clock past the token's 300 s; the token is judged
    // at the upgrade only, so the connection stays open and is still echoed.
    for _ in 0..7 {
        jump(50).await;
        heartbeat(&mut client).await;
    }
    // The token itself has expired: a new upgrade with it is refused.
    let r = exchange(
        u.s.addr,
        &raw(
            &u.s.host(),
            "HTTP/1.1",
            &[upgrade_headers(), vec![bearer(&t)]].concat(),
        ),
    )
    .await;
    assert_eq!(r.status, 401, "{}", r.head);
    heartbeat(&mut client).await;
    u.finish().await;
}

#[tokio::test]
async fn the_heartbeat_is_echoed_and_three_intervals_of_silence_close_with_4003() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    // `hello` was read, so the server is at rest and its idle clock runs from its write.
    jump(50).await;
    heartbeat(&mut client).await;
    jump(50).await;
    // A ping counts as a frame: the echo above moved the deadline to 110 s, and this moves it
    // again. The pong shows that the server has handled it.
    client
        .send(Message::Ping(vec![1, 2, 3].into()))
        .await
        .unwrap();
    loop {
        match frame(&mut client).await.expect("the stream ended") {
            Ok(Message::Pong(_)) => break,
            Ok(Message::Ping(_)) => {}
            other => panic!("expected a pong: {other:?}"),
        }
    }
    // 150 s in: without the ping the deadline (110 s) has passed and the connection is gone.
    jump(50).await;
    heartbeat(&mut client).await;
    jump(61).await;
    assert_eq!(close_code(&mut client).await, 4003);
    u.finish().await;
}

#[tokio::test]
async fn a_binary_frame_closes_with_1003() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    client
        .send(Message::Binary(vec![0u8, 1, 2].into()))
        .await
        .unwrap();
    assert_eq!(close_code(&mut client).await, 1003);
    u.finish().await;
}

#[tokio::test]
async fn a_message_over_128_kib_closes_with_1009_and_one_of_exactly_128_kib_is_read() {
    let u = Up::new().await;
    let t = token(&u).await;
    let limit = 128 * 1024;

    let mut client = open(u.s.addr, &t).await;
    heartbeat_with(&mut client, padded_heartbeat(limit)).await;
    heartbeat(&mut client).await;

    // The stack refuses the frame on its header; its Close arrives while the server lingers.
    let mut client = open(u.s.addr, &t).await;
    client
        .send(Message::Text(padded_heartbeat(limit + 1).into()))
        .await
        .unwrap();
    assert_eq!(close_code(&mut client).await, 1009);
    u.finish().await;
}

#[tokio::test]
async fn each_malformed_text_closes_with_1002() {
    let u = Up::new().await;
    let t = token(&u).await;
    let cases = [
        ("not json", "nope"),
        ("an unknown type", r#"{"type":"nope"}"#),
        (
            "a message of the other direction",
            r#"{"type":"hello","heartbeatSecs":20}"#,
        ),
        ("an unknown property", r#"{"type":"heartbeat","extra":1}"#),
        ("a missing property", r#"{"type":"commandAck"}"#),
        (
            "a value out of range",
            r#"{"type":"agentEvent","seq":1,"kind":"progress","jobId":"j","progressPermille":1001,"phase":"x"}"#,
        ),
    ];
    for (name, body) in cases {
        let mut client = open(u.s.addr, &t).await;
        client.send(Message::Text(body.into())).await.unwrap();
        assert_eq!(close_code(&mut client).await, 1002, "{name}");
    }
    u.finish().await;
}

/// Writes one masked text frame whose payload is not UTF-8, which the stack refuses on read.
async fn send_invalid_utf8(client: &mut Client) {
    use tokio::io::AsyncWriteExt;
    // FIN and text, masked with a zero key so the payload goes as written.
    let frame = [0x81, 0x82, 0, 0, 0, 0, 0xff, 0xfe];
    client.get_mut().write_all(&frame).await.unwrap();
}

#[tokio::test]
async fn a_text_frame_that_is_not_utf8_closes_with_1002() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    send_invalid_utf8(&mut client).await;
    assert_eq!(close_code(&mut client).await, 1002);
    u.finish().await;
}

#[tokio::test]
async fn a_stop_ends_the_wait_after_a_read_error() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    send_invalid_utf8(&mut client).await;
    // The Close has been written, so the session now waits without reading.
    match frame(&mut client).await {
        Some(Ok(Message::Close(Some(close)))) => assert_eq!(u16::from(close.code), 1002),
        other => panic!("not a Close frame: {other:?}"),
    }
    let Up { w, mut s, .. } = u;
    let started = Instant::now();
    s.signal_stop();
    s.joined().await;
    // The wait is 5 seconds and the grace period 2: the stop ends the wait well before either.
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    drop(client);
    w.finish().await;
}

#[tokio::test]
async fn valid_outcomes_and_events_are_read_and_the_connection_stays_open() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    for body in [
        r#"{"type":"commandAck","commandId":"c-1"}"#,
        r#"{"type":"commandRejected","commandId":"c-2","reason":"malformed"}"#,
        r#"{"type":"agentEvent","seq":1,"kind":"progress","jobId":"j","progressPermille":5,"phase":"x"}"#,
    ] {
        client.send(Message::Text(body.into())).await.unwrap();
        heartbeat(&mut client).await;
    }
    u.finish().await;
}

#[tokio::test]
async fn a_close_from_the_agent_is_answered_and_ends_the_session() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    client.send(Message::Close(None)).await.unwrap();
    let mut answered = false;
    loop {
        match frame(&mut client).await {
            Some(Ok(Message::Close(_))) => answered = true,
            Some(Ok(_)) => {}
            Some(Err(_)) | None => break,
        }
    }
    assert!(answered, "the server's reply to the Close frame");
    u.finish().await;
}

#[tokio::test]
async fn a_newer_connection_of_the_agent_replaces_the_older_with_4001() {
    let u = Up::new().await;
    // One token for both: a token is not bound to a connection.
    let t = token(&u).await;
    let mut old = open(u.s.addr, &t).await;
    let mut new = open(u.s.addr, &t).await;
    assert_eq!(close_code(&mut old).await, 4001);
    heartbeat(&mut new).await;
    u.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The stop

#[tokio::test]
async fn a_stop_closes_an_open_connection_with_1001_within_the_grace_period() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    let Up { w, mut s, .. } = u;
    let started = Instant::now();
    s.signal_stop();
    let (code, ()) = tokio::join!(close_code(&mut client), s.joined());
    assert_eq!(code, 1001);
    // The grace period is 2 seconds: a session that was aborted would have taken all of it.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    w.finish().await;
}

#[tokio::test]
async fn a_peer_that_never_answers_is_aborted_at_the_end_of_the_grace_period() {
    let u = Up::new().await;
    let t = token(&u).await;
    // Never read again, so the Close frame is never answered.
    let client = open(u.s.addr, &t).await;
    let Up { w, mut s, .. } = u;
    let started = Instant::now();
    s.signal_stop();
    s.joined().await;
    let waited = started.elapsed();
    // The grace period is 2 seconds and the session would wait 5 for the peer.
    assert!(
        waited >= Duration::from_secs(2) && waited < Duration::from_millis(4500),
        "{waited:?}"
    );
    drop(client);
    w.finish().await;
}

#[tokio::test]
async fn a_second_stop_signal_drops_a_connection_at_once() {
    let u = Up::new().await;
    let t = token(&u).await;
    let client = open(u.s.addr, &t).await;
    let Up { w, mut s, .. } = u;
    s.signal_stop();
    let started = Instant::now();
    let _ = s.force.take().unwrap().send(());
    s.joined().await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    drop(client);
    w.finish().await;
}
