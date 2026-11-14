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
use server::hub::{INBOX_CAPACITY, Inbound};
use server::{Delivery, SendError};
use shared_crypto::random_32;
use shared_proto::{AgentId, AgentMessage, ServerMessage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Request, Response};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{WebSocketStream, client_async};

use crate::redeem::{Agent, capture, dead, instruction_key, json};
use crate::serving::{Reply, exchange};
use crate::session::Up;

const PATH: &str = "/api/v1/agent/control";
const SUBPROTOCOL: &str = "ngr-control.v1";
const HEARTBEAT: &str = r#"{"type":"heartbeat"}"#;

pub(crate) type Client = WebSocketStream<TcpStream>;

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
pub(crate) async fn open(addr: SocketAddr, token: &str) -> Client {
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
pub(crate) async fn text(client: &mut Client) -> String {
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

pub(crate) async fn heartbeat(client: &mut Client) {
    heartbeat_with(client, HEARTBEAT.to_owned()).await;
}

/// Reads until the connection ends and returns the code of the Close frame it carried.
pub(crate) async fn close_code(client: &mut Client) -> u16 {
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

pub(crate) async fn token(u: &Up) -> String {
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
    // A9: none of them stamped the row, and none used the token up.
    assert_eq!(
        last_seen(&u, &u.id).await,
        "",
        "a refused upgrade writes nothing"
    );
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
            r#"{"type":"agentEvent","kind":"progress","jobId":"j","progressPermille":1001,"phase":"x"}"#,
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
        r#"{"type":"agentEvent","kind":"progress","jobId":"j","progressPermille":5,"phase":"x"}"#,
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

// ---------------------------------------------------------------------------------------------
// The row, the stamp and the pass (ADR-468 item 11). Case ids (A1, B3, C4, ...) are those of the
// design notes of the hub.

/// The agent's `last_seen_at` as text, empty while it is NULL.
async fn last_seen(u: &Up, id: &str) -> String {
    u.w.sql(&format!(
        "SELECT last_seen_at FROM agents WHERE id = '{id}'"
    ))
    .await
}

/// Polls (real time, at most 3 seconds) until the row's `last_seen_at` is later than `than`.
async fn later_than(u: &Up, id: &str, than: &str) {
    let sql = format!("SELECT last_seen_at > '{than}'::timestamptz FROM agents WHERE id = '{id}'");
    for _ in 0..120 {
        if u.w.sql(&sql).await == "true" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("last_seen_at did not move past {than}");
}

/// Holds an exclusive lock on the agents table until dropped.
async fn lock_agents(u: &Up) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut tx = u.w.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE agents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

pub(crate) async fn revoke_row(u: &Up, id: &str) {
    sqlx::query("UPDATE agents SET revoked_at = now() WHERE id = $1::uuid")
        .bind(id)
        .execute(&u.w.pool)
        .await
        .unwrap();
}

/// Removes the agent's row. The rows that refer to it are not the test's concern, so the
/// foreign keys are not enforced for this one transaction.
async fn delete_row(u: &Up, id: &str) {
    let mut tx = u.w.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM agents WHERE id = $1::uuid")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// Registers a second agent and gives its token.
pub(crate) async fn second_agent(u: &Up) -> (String, String) {
    let agent = Agent::new();
    let id = u.w.register(&agent).await;
    let r = u.exchange_as(&agent.key, &id).await;
    assert_eq!(r.status, 200, "{}", r.head);
    (id, json(&r)["token"].as_str().unwrap().to_owned())
}

/// The upgrade only, with no `hello` read.
async fn connect(addr: SocketAddr, token: &str) -> Client {
    let request = request(addr, Some(&format!("Bearer {token}")), &[SUBPROTOCOL]);
    let (client, response) = handshake(addr, request)
        .await
        .unwrap_or_else(|error| panic!("the upgrade was refused: {error}"));
    assert_eq!(response.status(), 101);
    client
}

/// The code of the first frame, which must be a Close, and then reads to the end.
async fn first_frame_close_code(client: &mut Client) -> u16 {
    let code = match frame(client).await {
        Some(Ok(Message::Close(Some(close)))) => u16::from(close.code),
        other => panic!("the first frame is not a Close: {other:?}"),
    };
    while let Some(Ok(_)) = frame(client).await {}
    code
}

pub(crate) fn agent_id(u: &Up) -> AgentId {
    AgentId(u.id.clone())
}

fn count(log: &crate::redeem::Captured, needle: &str) -> usize {
    log.text().lines().filter(|l| l.contains(needle)).count()
}

pub(crate) async fn quiet_for(client: &mut Client, millis: u64) {
    assert!(
        tokio::time::timeout(Duration::from_millis(millis), client.next())
            .await
            .is_err(),
        "a frame arrived where none is due"
    );
}

/// How many statements of the server match `like` and are running now (not idle in a
/// transaction), as seen from the database.
async fn running_statements(u: &Up, like: &str) -> u32 {
    u.w.pg
        .scalar(&format!(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE pid <> pg_backend_pid() AND state = 'active' AND query LIKE '{like}'"
        ))
        .await
        .parse()
        .unwrap()
}

/// Polls (real time, at most 5 seconds) until a statement matching `like` is running (`want`) or
/// no longer is.
async fn wait_for_statement(u: &Up, like: &str, want: bool) {
    for _ in 0..200 {
        if (running_statements(u, like).await > 0) == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no statement like {like}: running = {want} within 5 seconds");
}

/// What the revocation pass's read looks like in the database.
const PASS_READ: &str = "%ANY($1::uuid[]) AND revoked_at IS NULL%";
/// What the connect stamp looks like in the database.
const STAMP: &str = "UPDATE agents SET last_seen_at%";

// A1, B3, B4
#[tokio::test]
async fn the_connect_stamp_is_set_when_hello_is_read_and_heartbeats_do_not_move_it() {
    let u = Up::new().await;
    let t = token(&u).await;
    assert_eq!(last_seen(&u, &u.id).await, "", "NULL before");
    let mut client = open(u.s.addr, &t).await;
    let connected = last_seen(&u, &u.id).await;
    assert_ne!(connected, "", "set once hello is read");
    for _ in 0..3 {
        heartbeat(&mut client).await;
    }
    client
        .send(Message::Text(
            r#"{"type":"agentEvent","kind":"progress","jobId":"j","progressPermille":5,"phase":"x"}"#.into(),
        ))
        .await
        .unwrap();
    heartbeat(&mut client).await;
    assert_eq!(
        last_seen(&u, &u.id).await,
        connected,
        "equal after heartbeats and an event"
    );

    // B3: the agent closes; the server's reply is read, then the row is written.
    client.send(Message::Close(None)).await.unwrap();
    let mut answered = false;
    while let Some(Ok(message)) = frame(&mut client).await {
        answered |= matches!(message, Message::Close(_));
    }
    assert!(answered);
    later_than(&u, &u.id, &connected).await;

    // B4: a transport that is gone is written at once.
    let t = token(&u).await;
    let client = open(u.s.addr, &t).await;
    let before = last_seen(&u, &u.id).await;
    drop(client);
    later_than(&u, &u.id, &before).await;
    u.finish().await;
}

// A2
#[tokio::test]
async fn a_revoked_row_at_connect_closes_with_4002_first_and_displaces_nobody() {
    let (log, _guard) = capture();
    let u = Up::new().await;
    let t = token(&u).await;
    let mut earlier = open(u.s.addr, &t).await;
    let stamped = last_seen(&u, &u.id).await;
    revoke_row(&u, &u.id).await;

    let mut refused = connect(u.s.addr, &t).await;
    assert_eq!(first_frame_close_code(&mut refused).await, 4002);
    // The stamp was committed, whatever the row said.
    later_than(&u, &u.id, &stamped).await;
    let after = last_seen(&u, &u.id).await;
    // The earlier connection was not replaced: it is still echoed (the pass is 30 s away).
    heartbeat(&mut earlier).await;
    // The tokens are gone.
    let r = exchange(
        u.s.addr,
        &raw(
            &u.s.host(),
            "HTTP/1.1",
            &[upgrade_headers(), vec![bearer(&t)]].concat(),
        ),
    )
    .await;
    assert_problem(&r, 401, "the token of a revoked agent");
    assert_eq!(
        count(&log, "control channel refused: the agent is revoked"),
        1
    );
    assert_eq!(
        last_seen(&u, &u.id).await,
        after,
        "no disconnect write for the refused one"
    );
    u.finish().await;
}

// A3
#[tokio::test]
async fn a_missing_row_at_connect_closes_with_4002_first_and_stamps_nothing() {
    let u = Up::new().await;
    let t = token(&u).await;
    delete_row(&u, &u.id).await;
    let mut refused = connect(u.s.addr, &t).await;
    assert_eq!(first_frame_close_code(&mut refused).await, 4002);
    let r = exchange(
        u.s.addr,
        &raw(
            &u.s.host(),
            "HTTP/1.1",
            &[upgrade_headers(), vec![bearer(&t)]].concat(),
        ),
    )
    .await;
    assert_problem(&r, 401, "the token of a missing agent");
    assert_eq!(
        u.w.sql(&format!(
            "SELECT count(*) FROM agents WHERE id = '{}'",
            u.id
        ))
        .await,
        "0"
    );
    u.finish().await;
}

// A4, B12
#[tokio::test]
async fn a_failed_stamp_is_one_gated_warning_each_way_and_the_connection_goes_on() {
    let (log, _guard) = capture();
    let u = Up::new().await;
    let t = token(&u).await;
    u.w.pg
        .execute("ALTER TABLE agents RENAME COLUMN last_seen_at TO last_seen_x")
        .await;
    let mut first = open(u.s.addr, &t).await;
    heartbeat(&mut first).await;
    let mut second = open(u.s.addr, &t).await;
    heartbeat(&mut second).await;
    assert_eq!(close_code(&mut first).await, 4001);
    assert_eq!(
        count(&log, "was not written at connect"),
        1,
        "one line for two failures"
    );
    second.send(Message::Close(None)).await.unwrap();
    while let Some(Ok(_)) = frame(&mut second).await {}
    // The drain went on after the failed write: the task ended.
    for _ in 0..120 {
        if count(&log, "control channel ended") == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(count(&log, "control channel ended"), 2);
    assert_eq!(count(&log, "was not written at disconnect"), 1);
    u.w.pg
        .execute("ALTER TABLE agents RENAME COLUMN last_seen_x TO last_seen_at")
        .await;
    assert_eq!(last_seen(&u, &u.id).await, "", "the row is unchanged");
    u.finish().await;
}

// A5 (real time)
#[tokio::test]
async fn a_database_that_does_not_answer_delays_hello_by_about_five_seconds_and_writes_nothing() {
    let u = Up::new().await;
    let t = token(&u).await;
    let lock = lock_agents(&u).await;
    let started = Instant::now();
    let mut client = connect(u.s.addr, &t).await;
    hello(&mut client).await;
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(4) && waited < Duration::from_secs(9),
        "{waited:?}"
    );
    heartbeat(&mut client).await;
    drop(lock);
    assert_eq!(
        last_seen(&u, &u.id).await,
        "",
        "the stamp that timed out left nothing"
    );
    u.finish().await;
}

// A6, B5
#[tokio::test]
async fn a_second_connection_stamps_again_and_the_replaced_one_writes_nothing() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut old = open(u.s.addr, &t).await;
    let first = last_seen(&u, &u.id).await;
    let mut new = open(u.s.addr, &t).await;
    let second = last_seen(&u, &u.id).await;
    later_than(&u, &u.id, &first).await;
    assert_eq!(close_code(&mut old).await, 4001);
    // The marker: the new connection is served after the old one ended.
    heartbeat(&mut new).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        last_seen(&u, &u.id).await,
        second,
        "B5: the successor's connect value"
    );

    // A stamp never moves back.
    u.w.pg
        .execute("UPDATE agents SET last_seen_at = now() + interval '1 hour'")
        .await;
    let future = last_seen(&u, &u.id).await;
    let mut third = open(u.s.addr, &t).await;
    heartbeat(&mut third).await;
    assert_eq!(last_seen(&u, &u.id).await, future);
    u.finish().await;
}

// A7
#[tokio::test]
async fn a_stop_during_the_stamp_is_not_raced_and_the_connection_is_closed_with_1001_first() {
    // A grace period well beyond the stop's own probe (on Windows `begin_stop` returns only after
    // a refused connect, about 2 seconds): the test does not time the stop, and the session must
    // not be aborted before the lock is released.
    let u = Up::with_grace(10).await;
    let t = token(&u).await;
    let lock = lock_agents(&u).await;
    let mut client = connect(u.s.addr, &t).await;
    // The marker: the session is adopted and its stamp waits on the lock, so the stop below
    // cannot come before the adoption (which would drop the session unrun: A8).
    wait_for_statement(&u, STAMP, true).await;
    let Up { w, mut s, id, .. } = u;
    // Returns once the listener is closed, which is after the hub has begun its stop.
    s.begin_stop().await;
    drop(lock);
    let (code, ()) = tokio::join!(first_frame_close_code(&mut client), s.joined());
    assert_eq!(code, 1001, "no hello before it");
    let stamped = w
        .sql(&format!(
            "SELECT last_seen_at IS NOT NULL FROM agents WHERE id = '{id}'"
        ))
        .await;
    assert_eq!(stamped, "true", "the connect write is kept");
    w.finish().await;
}

// B1
#[tokio::test]
async fn the_close_is_written_before_the_disconnect_stamp_and_the_stamp_follows_the_lock() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    let connected = last_seen(&u, &u.id).await;
    let lock = lock_agents(&u).await;
    client
        .send(Message::Binary(vec![1u8].into()))
        .await
        .unwrap();
    // The Close arrives while the stamp is still blocked.
    match frame(&mut client).await {
        Some(Ok(Message::Close(Some(close)))) => assert_eq!(u16::from(close.code), 1003),
        other => panic!("not a Close frame: {other:?}"),
    }
    drop(lock);
    while let Some(Ok(_)) = frame(&mut client).await {}
    later_than(&u, &u.id, &connected).await;
    u.finish().await;
}

// B8
#[tokio::test]
async fn a_stop_writes_the_disconnect_before_the_database_closes() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    let connected = last_seen(&u, &u.id).await;
    let Up { w, mut s, id, .. } = u;
    s.signal_stop();
    let (code, ()) = tokio::join!(close_code(&mut client), s.joined());
    assert_eq!(code, 1001);
    // Read at once, not polled: `run` returned, so the write was made before it.
    let later = w
        .sql(&format!(
            "SELECT last_seen_at > '{connected}'::timestamptz FROM agents WHERE id = '{id}'"
        ))
        .await;
    assert_eq!(later, "true");
    w.finish().await;
}

// B9 (real time)
#[tokio::test]
async fn a_connection_aborted_at_the_end_of_the_grace_period_writes_nothing() {
    let u = Up::new().await;
    let t = token(&u).await;
    let client = open(u.s.addr, &t).await;
    let connected = last_seen(&u, &u.id).await;
    let lock = lock_agents(&u).await;
    // The transport goes; the server's disconnect stamp now waits on the lock.
    drop(client);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let Up { w, mut s, id, .. } = u;
    s.signal_stop();
    s.joined().await;
    drop(lock);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let now = w
        .sql(&format!(
            "SELECT last_seen_at FROM agents WHERE id = '{id}'"
        ))
        .await;
    assert_eq!(now, connected);
    w.finish().await;
}

// C3 and the cadence
#[tokio::test]
async fn the_pass_leaves_live_agents_alone_and_runs_again_after_the_next_period() {
    let (log, _guard) = capture();
    let u = Up::new().await;
    let (_other_id, other_token) = second_agent(&u).await;
    let t = token(&u).await;
    let mut one = open(u.s.addr, &t).await;
    let mut two = open(u.s.addr, &other_token).await;
    // The pass's read is held on a lock until the clock has jumped, then let go: once it is
    // seen to have finished, nobody may have been closed.
    let lock = lock_agents(&u).await;
    jump(31).await;
    wait_for_statement(&u, PASS_READ, true).await;
    drop(lock);
    wait_for_statement(&u, PASS_READ, false).await;
    heartbeat(&mut one).await;
    heartbeat(&mut two).await;
    assert_eq!(count(&log, "its control channel is closed"), 0);
    revoke_row(&u, &u.id).await;
    jump(31).await;
    assert_eq!(close_code(&mut one).await, 4002);
    heartbeat(&mut two).await;
    u.finish().await;
}

// C4
#[tokio::test]
async fn a_row_revoked_by_sql_is_closed_by_the_pass_with_its_tokens_dropped() {
    let (log, _guard) = capture();
    let u = Up::new().await;
    let (_other_id, other_token) = second_agent(&u).await;
    let t = token(&u).await;
    let mut one = open(u.s.addr, &t).await;
    let mut two = open(u.s.addr, &other_token).await;
    let connected = last_seen(&u, &u.id).await;
    revoke_row(&u, &u.id).await;
    jump(31).await;
    assert_eq!(close_code(&mut one).await, 4002);
    heartbeat(&mut two).await;
    let r = exchange(
        u.s.addr,
        &raw(
            &u.s.host(),
            "HTTP/1.1",
            &[upgrade_headers(), vec![bearer(&t)]].concat(),
        ),
    )
    .await;
    assert_problem(&r, 401, "the token of the revoked agent");
    let lines: Vec<String> = log
        .text()
        .lines()
        .filter(|l| l.contains("its control channel is closed"))
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 1, "one warn line");
    assert!(lines[0].contains(&u.id), "the line names the agent");
    // The closed connection took the disconnect path, which stamps.
    later_than(&u, &u.id, &connected).await;
    u.finish().await;
}

// C5
#[tokio::test]
async fn a_missing_row_is_closed_by_the_pass_like_a_revoked_one() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut one = open(u.s.addr, &t).await;
    delete_row(&u, &u.id).await;
    jump(31).await;
    assert_eq!(close_code(&mut one).await, 4002);
    u.finish().await;
}

// C6
#[tokio::test]
async fn a_failed_read_of_the_pass_closes_nobody_and_the_next_period_is_normal() {
    let (log, _guard) = capture();
    let u = Up::new().await;
    let t = token(&u).await;
    let mut one = open(u.s.addr, &t).await;
    u.w.pg
        .execute("ALTER TABLE agents RENAME COLUMN revoked_at TO revoked_x")
        .await;
    jump(31).await;
    heartbeat(&mut one).await;
    for _ in 0..120 {
        if count(&log, "could not read the agents") == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(count(&log, "could not read the agents"), 1);
    // The read has finished and failed: the agent is still open and its token still works (a
    // valid token without an upgrade is 400, a dropped one is 401).
    heartbeat(&mut one).await;
    let r = exchange(
        u.s.addr,
        &raw(
            &u.s.host(),
            "HTTP/1.1",
            &["Connection: close".to_owned(), bearer(&t)],
        ),
    )
    .await;
    assert_problem(&r, 400, "the token after a failed read");
    u.w.pg
        .execute("ALTER TABLE agents RENAME COLUMN revoked_x TO revoked_at")
        .await;
    revoke_row(&u, &u.id).await;
    jump(31).await;
    assert_eq!(close_code(&mut one).await, 4002);
    u.finish().await;
}

// D1, D2, D5, D6
#[tokio::test]
async fn commands_reach_the_agent_in_order_and_the_size_and_presence_errors_are_returned() {
    let u = Up::new().await;
    let t = token(&u).await;
    let agent = agent_id(&u);
    assert_eq!(
        u.hub.send_job_command(&agent, "x".to_owned()),
        Err(SendError::NotConnected),
        "D2: nobody connected"
    );
    let mut client = open(u.s.addr, &t).await;
    // D1: the frame's own length counts, judged with the widest command ID the counter can make.
    let overhead = ServerMessage::JobCommand {
        command_id: format!("c-{}", u64::MAX),
        signed_job: Some(String::new()),
    }
    .encode()
    .unwrap()
    .len();
    let limit = 128 * 1024;
    // `c-1` went to the call with nobody connected and `c-2` to this refusal.
    assert_eq!(
        u.hub
            .send_job_command(&agent, "x".repeat(limit - overhead + 1)),
        Err(SendError::TooLarge)
    );
    let exact = "x".repeat(limit - overhead);
    let delivery: Delivery = u.hub.send_job_command(&agent, exact.clone()).unwrap();
    assert_eq!(delivery.command_id, "c-3");
    let third = u.hub.send_job_command(&agent, "tail".to_owned()).unwrap();
    assert_eq!(third.command_id, "c-4");
    let message = ServerMessage::decode(&text(&mut client).await).unwrap();
    assert_eq!(
        message,
        ServerMessage::JobCommand {
            command_id: "c-3".to_owned(),
            signed_job: Some(exact)
        }
    );
    let message = ServerMessage::decode(&text(&mut client).await).unwrap();
    assert_eq!(
        message,
        ServerMessage::JobCommand {
            command_id: "c-4".to_owned(),
            signed_job: Some("tail".to_owned())
        }
    );
    // Written within the write limit, the connection is still served.
    heartbeat(&mut client).await;
    // The agent goes; the entry goes with it.
    client.send(Message::Close(None)).await.unwrap();
    while let Some(Ok(_)) = frame(&mut client).await {}
    for _ in 0..120 {
        if u.hub.connected().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        u.hub.send_job_command(&agent, "x".to_owned()),
        Err(SendError::NotConnected)
    );
    u.finish().await;
}

async fn next_inbound(inbox: &mut tokio::sync::mpsc::Receiver<Inbound>) -> Inbound {
    tokio::time::timeout(Duration::from_secs(10), inbox.recv())
        .await
        .expect("a message within 10 seconds")
        .expect("the inbox is open")
}

/// A numbered event (a `progress` event has no number, ADR-478); `0` is sent as 1, since a
/// number is at least 1.
pub(crate) fn numbered_event(seq: usize) -> Message {
    let seq = seq.max(1);
    Message::Text(
        format!(
            r#"{{"type":"agentEvent","seq":{seq},"kind":"jobState","jobId":"j","state":"running"}}"#
        )
        .into(),
    )
}

// E5
#[tokio::test]
async fn the_inbox_gives_outcomes_and_events_in_read_order_with_the_agent_and_connection() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut inbox = u.hub.inbox().expect("the first call");
    let mut client = open(u.s.addr, &t).await;
    let agent = agent_id(&u);
    let one = u.hub.send_job_command(&agent, "a".to_owned()).unwrap();
    let two = u.hub.send_job_command(&agent, "b".to_owned()).unwrap();
    assert_eq!(one.connection, two.connection);
    let first = ServerMessage::decode(&text(&mut client).await).unwrap();
    assert!(
        matches!(first, ServerMessage::JobCommand { ref command_id, .. } if *command_id == one.command_id)
    );
    let second = ServerMessage::decode(&text(&mut client).await).unwrap();
    assert!(
        matches!(second, ServerMessage::JobCommand { ref command_id, .. } if *command_id == two.command_id)
    );

    let bodies = [
        format!(
            r#"{{"type":"commandAck","commandId":"{}"}}"#,
            one.command_id
        ),
        r#"{"type":"agentEvent","seq":1,"kind":"jobState","jobId":"j","state":"received"}"#
            .to_owned(),
        format!(
            r#"{{"type":"commandRejected","commandId":"{}","reason":"malformed"}}"#,
            two.command_id
        ),
        r#"{"type":"reconcile","openJobs":[]}"#.to_owned(),
    ];
    for body in &bodies {
        client
            .send(Message::Text(body.clone().into()))
            .await
            .unwrap();
        // A heartbeat between messages is echoed and never reaches the inbox.
        heartbeat(&mut client).await;
    }
    let got = [
        next_inbound(&mut inbox).await,
        next_inbound(&mut inbox).await,
        next_inbound(&mut inbox).await,
        next_inbound(&mut inbox).await,
    ];
    for inbound in &got {
        assert_eq!(inbound.agent, agent);
        assert_eq!(inbound.connection, one.connection);
    }
    assert!(
        matches!(&got[0].message, AgentMessage::CommandAck { command_id } if *command_id == one.command_id)
    );
    assert!(matches!(&got[1].message, AgentMessage::AgentEvent(event) if event.seq == Some(1)));
    assert!(
        matches!(&got[2].message, AgentMessage::CommandRejected { command_id, .. } if *command_id == two.command_id)
    );
    assert!(matches!(&got[3].message, AgentMessage::Reconcile { .. }));
    assert!(inbox.try_recv().is_err(), "no heartbeat arrives");
    u.finish().await;
}

// E2, E4
#[tokio::test]
async fn messages_nobody_takes_are_dropped_without_waiting_and_an_inbox_taken_later_is_used() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    for seq in 0..300 {
        client.send(numbered_event(seq)).await.unwrap();
    }
    heartbeat(&mut client).await;
    let mut inbox = u.hub.inbox().expect("the first call");
    client.send(numbered_event(1000)).await.unwrap();
    heartbeat(&mut client).await;
    let got = next_inbound(&mut inbox).await;
    assert!(matches!(got.message, AgentMessage::AgentEvent(event) if event.seq == Some(1000)));
    assert!(
        inbox.try_recv().is_err(),
        "the 300 earlier ones were not kept"
    );
    u.finish().await;
}

/// Takes the inbox and sends events until the inbox is full and the task holds one more.
async fn fill_inbox_only(u: &Up, client: &mut Client) -> tokio::sync::mpsc::Receiver<Inbound> {
    let inbox = u.hub.inbox().expect("the first call");
    for seq in 1..=INBOX_CAPACITY + 1 {
        client.send(numbered_event(seq)).await.unwrap();
    }
    for _ in 0..200 {
        if inbox.len() == INBOX_CAPACITY {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(inbox.len(), INBOX_CAPACITY, "the inbox is full");
    inbox
}

/// As [`fill_inbox_only`], then proves that nothing more is read: a heartbeat sent now is not
/// echoed.
async fn fill_inbox(u: &Up, client: &mut Client) -> tokio::sync::mpsc::Receiver<Inbound> {
    let inbox = fill_inbox_only(u, client).await;
    client.send(Message::Text(HEARTBEAT.into())).await.unwrap();
    quiet_for(client, 300).await;
    inbox
}

// E6, E3
#[tokio::test]
async fn a_full_inbox_holds_the_task_without_idle_time_and_a_slot_or_a_dropped_receiver_frees_it() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    let mut inbox = fill_inbox(&u, &mut client).await;
    // The wait is not idle time: more than three intervals pass, and the connection lives.
    jump(61).await;
    quiet_for(&mut client, 200).await;
    // A slot frees: the held message is handed over, the queued heartbeat is read and echoed.
    let first = next_inbound(&mut inbox).await;
    assert!(matches!(first.message, AgentMessage::AgentEvent(event) if event.seq == Some(1)));
    let echo = ServerMessage::decode(&text(&mut client).await).unwrap();
    assert_eq!(echo, ServerMessage::Heartbeat);
    // All 257 arrive in order: the held one is the last.
    let mut last = 0;
    for _ in 0..INBOX_CAPACITY {
        match next_inbound(&mut inbox).await.message {
            AgentMessage::AgentEvent(event) => last = event.seq.unwrap(),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(last, u64::try_from(INBOX_CAPACITY + 1).unwrap());

    // E3: a task that waits wakes when the receiver is dropped, and drops what it holds.
    let mut client2 = open(u.s.addr, &token(&u).await).await;
    drop(inbox);
    for seq in 0..5 {
        client2.send(numbered_event(seq)).await.unwrap();
    }
    heartbeat(&mut client2).await;
    u.finish().await;
}

// E7
#[tokio::test]
async fn a_close_instruction_ends_a_task_that_waits_on_a_full_inbox() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    let _inbox = fill_inbox(&u, &mut client).await;
    assert!(u.hub.close_agent(&agent_id(&u), 4002));
    assert_eq!(close_code(&mut client).await, 4002);
    u.finish().await;
}

// E8
#[tokio::test]
async fn a_stop_ends_a_task_that_waits_on_a_full_inbox_within_the_grace_period() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    let _inbox = fill_inbox(&u, &mut client).await;
    let Up { w, mut s, .. } = u;
    let started = Instant::now();
    s.signal_stop();
    let (code, ()) = tokio::join!(close_code(&mut client), s.joined());
    assert_eq!(code, 1001);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    w.finish().await;
}

// D10
#[tokio::test]
async fn a_command_written_to_a_silent_agent_does_not_restart_the_idle_clock() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    jump(40).await;
    u.hub
        .send_job_command(&agent_id(&u), "j".to_owned())
        .unwrap();
    let message = ServerMessage::decode(&text(&mut client).await).unwrap();
    assert!(matches!(message, ServerMessage::JobCommand { .. }));
    // 65 s after hello and 25 after the command: the command did not move the 60 s deadline.
    jump(25).await;
    assert_eq!(close_code(&mut client).await, 4003);
    u.finish().await;
}

// E6 (the idle clock restarts on hand-over; the queue is not drained while the task waits)
#[tokio::test]
async fn a_hand_over_restarts_the_idle_clock_and_the_queue_waits_for_the_release() {
    let u = Up::new().await;
    let t = token(&u).await;
    let mut client = open(u.s.addr, &t).await;
    // No heartbeat is queued behind the events, so the hand-over is the only thing that can
    // restart the idle clock.
    let mut inbox = fill_inbox_only(&u, &mut client).await;
    jump(61).await;
    quiet_for(&mut client, 200).await;
    u.hub
        .send_job_command(&agent_id(&u), "j".to_owned())
        .unwrap();
    quiet_for(&mut client, 300).await;
    // The slot frees: the frame arrives only now, and the connection is not idle although the
    // old deadline has passed.
    let _ = next_inbound(&mut inbox).await;
    let message = ServerMessage::decode(&text(&mut client).await).unwrap();
    assert!(matches!(message, ServerMessage::JobCommand { .. }));
    // Under 60 seconds after the hand-over: open. Past it: 4003.
    jump(40).await;
    quiet_for(&mut client, 200).await;
    jump(30).await;
    assert_eq!(close_code(&mut client).await, 4003);
    u.finish().await;
}

// D13 (a close instruction)
#[tokio::test]
async fn a_frame_queued_before_a_close_instruction_is_never_written() {
    let u = Up::new().await;
    let t = token(&u).await;
    let agent = agent_id(&u);
    // The test does not yield between the two calls, so the task finds both ready; it takes
    // either at random unless the frame's arm looks at the instruction first.
    for round in 0..16 {
        let mut client = open(u.s.addr, &t).await;
        u.hub.send_job_command(&agent, format!("j{round}")).unwrap();
        assert!(u.hub.close_agent(&agent, 4002));
        assert_eq!(
            first_frame_close_code(&mut client).await,
            4002,
            "round {round}"
        );
    }
    u.finish().await;
}

// D13 (the stop)
#[tokio::test]
async fn a_frame_queued_before_the_stop_is_never_written() {
    let mut u = Up::new().await;
    let mut agents = vec![(agent_id(&u), token(&u).await)];
    for _ in 0..7 {
        let (id, token) = second_agent(&u).await;
        agents.push((AgentId(id), token));
    }
    let mut clients = Vec::new();
    for (_, token) in &agents {
        clients.push(open(u.s.addr, token).await);
    }
    // The stop is signalled and the commands are queued with no yield between, so each task
    // finds its frame and the stop ready together. This relies on tokio's current-thread run
    // queue being FIFO: the run task wakes before the connection tasks, so `begin_stop` happens
    // first.
    u.s.signal_stop();
    for (agent, _) in &agents {
        u.hub.send_job_command(agent, "j".to_owned()).unwrap();
    }
    for client in &mut clients {
        assert_eq!(first_frame_close_code(client).await, 1001);
    }
    u.finish().await;
}
