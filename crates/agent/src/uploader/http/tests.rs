use std::io::{self, Cursor, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;

use super::*;
use crate::uploader::fake::{
    FakeDial, Mode, Seen, ServerEnd, answer, json, read_request, scripted, scripted_tls,
};

const HOST: &str = "127.0.0.1:9000";

fn request<'a>(method: Method, target: &'a str, body: Option<&str>) -> JsonRequest<'a> {
    JsonRequest {
        method,
        target,
        host: HOST,
        bearer: "tok-123",
        body: body.map(|b| b.as_bytes().to_vec()),
        nonce: shared_crypto::random_32().unwrap(),
        reads: reads_body,
    }
}

async fn post(client: &Client) -> Result<Answer, Error> {
    client
        .json(9000, &request(Method::POST, "/x", Some("{}")))
        .await
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A pin for a request that never reaches a server.
fn any_pin() -> CertPin {
    CertPin::of_certificate(b"no certificate")
}

#[tokio::test(start_paused = true)]
async fn the_bearer_goes_to_the_json_routes_and_never_to_a_put() {
    let (dial, seen) = scripted(json(200, "{}"));
    let client = Client::with_dial(dial.clone());
    let got = client
        .json(9000, &request(Method::GET, "/api/v1/agent/uploads/x", None))
        .await
        .unwrap();
    assert_eq!(got.status, 200);
    assert_eq!(got.body, AnswerBody::Bytes(b"{}".to_vec()));
    let (object_dial, object_seen, pin) = scripted_tls(json(200, "{}"));
    let put = Client::with_dial(object_dial)
        .put(
            9000,
            &pin,
            "/obj?sig=1",
            HOST,
            3,
            Box::new(Cursor::new(b"abc".to_vec())),
        )
        .await
        .unwrap();
    assert_eq!(put.status, 200);
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0].request_line(),
        "GET /api/v1/agent/uploads/x HTTP/1.1"
    );
    assert_eq!(
        seen[0].header("authorization").as_deref(),
        Some("Bearer tok-123")
    );
    assert_eq!(seen[0].header("host").as_deref(), Some(HOST));
    assert_eq!(seen[0].header("connection").as_deref(), Some("close"));
    let object_seen = object_seen.lock().unwrap();
    assert_eq!(object_seen[0].request_line(), "PUT /obj?sig=1 HTTP/1.1");
    assert_eq!(object_seen[0].header("authorization"), None);
    assert_eq!(object_seen[0].header("host").as_deref(), Some(HOST));
    assert_eq!(
        object_seen[0].header("content-length").as_deref(),
        Some("3")
    );
    assert_eq!(object_seen[0].body, b"abc");
}

#[tokio::test(start_paused = true)]
async fn a_redirect_is_returned_and_not_followed() {
    let (dial, _) = scripted(answer(
        302,
        "Location: http://127.0.0.1:9999/elsewhere\r\n",
        "moved",
    ));
    let client = Client::with_dial(dial.clone());
    let got = post(&client).await.unwrap();
    assert_eq!(got.status, 302);
    assert_eq!(got.body, AnswerBody::Unread);
    assert_eq!(dial.count(), 1, "no second connection");
    assert_eq!(*dial.dialled.lock().unwrap(), vec![9000]);
}

#[tokio::test(start_paused = true)]
async fn a_head_over_the_limit_is_refused() {
    let filler = "X-Pad: ".to_owned() + &"a".repeat(1000) + "\r\n";
    let (dial, _) = scripted(answer(200, &filler.repeat(20), "{}"));
    let client = Client::with_dial(dial);
    assert_eq!(post(&client).await.unwrap_err(), Error::Failed);
    // Too many header lines are refused as well.
    let lines = "X: 1\r\n".repeat(MAX_HEADERS + 1);
    let (dial, _) = scripted(answer(200, &lines, "{}"));
    assert_eq!(
        post(&Client::with_dial(dial)).await.unwrap_err(),
        Error::Failed
    );
}

/// The body is read for 200, 201, 202 and 409 only: for any other status the server declares a
/// body it never sends, and the answer comes at once.
#[tokio::test(start_paused = true)]
async fn a_body_is_read_for_four_statuses_only() {
    for status in [200u16, 201, 202, 409, 204, 400, 401, 404, 500, 503] {
        let (dial, _) = scripted(json(status, "{\"a\":1}"));
        let got = post(&Client::with_dial(dial)).await.unwrap();
        let want = if reads_body(status) {
            AnswerBody::Bytes(b"{\"a\":1}".to_vec())
        } else {
            AnswerBody::Unread
        };
        assert_eq!((status, got.status, got.body), (status, status, want));
    }
    // The unread body is never waited for.
    let dial = FakeDial::new(4096, |_, mut server| {
        tokio::spawn(async move {
            let _ = read_request(&mut server).await;
            let _ = server
                .write_all(b"HTTP/1.1 500 X\r\nContent-Length: 10\r\n\r\n")
                .await;
            std::future::pending::<()>().await;
        });
    });
    let started = Instant::now();
    let got = post(&Client::with_dial(dial)).await.unwrap();
    assert_eq!((got.status, got.body), (500, AnswerBody::Unread));
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn an_answer_body_over_the_bound_is_not_kept() {
    let big = "a".repeat(MAX_ANSWER + 1);
    // Declared.
    let (dial, _) = scripted(json(200, &big));
    let got = post(&Client::with_dial(dial)).await.unwrap();
    assert_eq!(got.body, AnswerBody::TooLarge);
    // Arriving without a declared length.
    let raw = format!("HTTP/1.1 200 X\r\nConnection: close\r\n\r\n{big}").into_bytes();
    let (dial, _) = scripted(raw);
    let got = post(&Client::with_dial(dial)).await.unwrap();
    assert_eq!(got.body, AnswerBody::TooLarge);
    // At the bound is kept.
    let exact = "a".repeat(MAX_ANSWER);
    let (dial, _) = scripted(json(200, &exact));
    let got = post(&Client::with_dial(dial)).await.unwrap();
    assert_eq!(got.body, AnswerBody::Bytes(exact.into_bytes()));
}

#[tokio::test(start_paused = true)]
async fn retry_after_is_carried_when_it_is_one_text_value() {
    let (dial, _) = scripted(answer(503, "Retry-After: 7\r\n", ""));
    let got = post(&Client::with_dial(dial)).await.unwrap();
    assert_eq!(got.retry_after.as_deref(), Some("7"));
    let (dial, _) = scripted(answer(503, "Retry-After: 7\r\nRetry-After: 8\r\n", ""));
    let got = post(&Client::with_dial(dial)).await.unwrap();
    assert_eq!(got.retry_after, None);
}

/// The server reads the request body in whatever pieces it comes in and records their sizes.
fn sizing_server(
    sizes: Arc<Mutex<Vec<usize>>>,
    received: Arc<Mutex<Vec<u8>>>,
) -> (Arc<FakeDial>, CertPin) {
    FakeDial::tls(1024, move |_, mut server| {
        let (sizes, received) = (sizes.clone(), received.clone());
        tokio::spawn(async move {
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                server.read_exact(&mut byte).await.unwrap();
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let mut buf = vec![0u8; 8192];
            while received.lock().unwrap().len() < length {
                let n = server.read(&mut buf).await.unwrap();
                assert!(n > 0, "the body ended early");
                sizes.lock().unwrap().push(n);
                received.lock().unwrap().extend_from_slice(&buf[..n]);
            }
            let _ = server.write_all(&answer(200, "", "")).await;
            let _ = server.shutdown().await;
        });
    })
}

use std::sync::{Arc, Mutex};

/// A reader of `length` bytes of a pattern that records how much it was asked for at once and
/// how much it has produced.
struct Pattern {
    left: u64,
    produced: Arc<AtomicU64>,
    biggest_read: Arc<AtomicU64>,
}

impl Read for Pattern {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        self.biggest_read
            .fetch_max(buf.len() as u64, Ordering::SeqCst);
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = ((self.produced.load(Ordering::SeqCst) + i as u64) % 251) as u8;
        }
        self.left -= n as u64;
        self.produced.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }
}

fn pattern(length: u64) -> (Box<Pattern>, Arc<AtomicU64>, Arc<AtomicU64>) {
    let produced = Arc::new(AtomicU64::new(0));
    let biggest_read = Arc::new(AtomicU64::new(0));
    (
        Box::new(Pattern {
            left: length,
            produced: produced.clone(),
            biggest_read: biggest_read.clone(),
        }),
        produced,
        biggest_read,
    )
}

#[tokio::test(start_paused = true)]
async fn a_part_larger_than_a_chunk_is_streamed_whole_and_in_pieces() {
    let length = (3 * CHUNK + 12345) as u64;
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::new(Mutex::new(Vec::new()));
    let (dial, pin) = sizing_server(sizes.clone(), received.clone());
    let client = Client::with_dial(dial);
    let (source, produced, biggest) = pattern(length);
    let got = client
        .put(9000, &pin, "/p", HOST, length, source)
        .await
        .unwrap();
    assert_eq!(got.status, 200);
    let received = received.lock().unwrap();
    assert_eq!(received.len() as u64, length);
    assert!(
        received
            .iter()
            .enumerate()
            .all(|(i, b)| *b == (i % 251) as u8)
    );
    assert_eq!(produced.load(Ordering::SeqCst), length);
    // The reader was never asked for more than a chunk, and the socket saw many pieces.
    assert!(biggest.load(Ordering::SeqCst) <= CHUNK as u64);
    assert!(sizes.lock().unwrap().len() > 3);
}

/// Memory does not depend on the part's size: a server that stops reading holds the reader back
/// after a few chunks, however long the part is.
/// On the real clock: a paused clock does not advance while a blocking reader is in flight.
#[tokio::test]
async fn the_reader_is_held_back_by_a_server_that_does_not_read() {
    let length = 1024 * 1024 * 1024u64;
    // This server reads the head only (`read_request` would wait for the whole body).
    let (dial, pin) = FakeDial::tls(1024, |_, mut server| {
        tokio::spawn(async move {
            let mut head = [0u8; 256];
            let _ = server.read(&mut head).await;
            std::future::pending::<()>().await;
        });
    });
    let client = Client::with_dial(dial);
    let (source, produced, _) = pattern(length);
    let put = client.put(9000, &pin, "/p", HOST, length, source);
    tokio::select! {
        _ = put => panic!("the put cannot finish"),
        () = tokio::time::sleep(Duration::from_millis(300)) => {}
    }
    let produced = produced.load(Ordering::SeqCst);
    assert!(produced > 0);
    assert!(produced <= 1024 * 1024, "{produced} bytes were read ahead");
}

#[tokio::test(start_paused = true)]
async fn a_source_shorter_than_declared_fails_the_put() {
    let (dial, pin) = FakeDial::tls(1024, |_, mut server| {
        tokio::spawn(async move {
            let mut sink = vec![0u8; 4096];
            while server.read(&mut sink).await.is_ok_and(|n| n > 0) {}
        });
    });
    let client = Client::with_dial(dial);
    let got = client
        .put(
            9000,
            &pin,
            "/p",
            HOST,
            100,
            Box::new(Cursor::new(vec![1u8; 10])),
        )
        .await;
    assert!(got.is_err(), "{got:?}");
}

#[tokio::test(start_paused = true)]
async fn an_empty_part_is_sent_with_a_zero_length() {
    let (dial, seen, pin) = scripted_tls(answer(200, "", ""));
    let client = Client::with_dial(dial);
    let got = client
        .put(9000, &pin, "/p", HOST, 0, Box::new(Cursor::new(Vec::new())))
        .await
        .unwrap();
    assert_eq!(got.status, 200);
    assert_eq!(
        seen.lock().unwrap()[0].header("content-length").as_deref(),
        Some("0")
    );
}

#[tokio::test(start_paused = true)]
async fn the_connect_is_bounded_at_five_seconds_and_a_refusal_is_not_sent() {
    let (dial, _) = scripted(json(200, "{}"));
    let client = Client::with_dial(dial.clone());
    dial.set(Mode::Hang);
    let started = Instant::now();
    assert_eq!(post(&client).await.unwrap_err(), Error::ConnectTimeout);
    assert_eq!(started.elapsed(), CONNECT_TIMEOUT);
    dial.set(Mode::Refuse);
    assert_eq!(post(&client).await.unwrap_err(), Error::NotSent);
    // A PUT is bounded the same way.
    dial.set(Mode::Hang);
    let started = Instant::now();
    let put = client
        .put(
            9000,
            &any_pin(),
            "/p",
            HOST,
            1,
            Box::new(Cursor::new(vec![0u8])),
        )
        .await;
    assert_eq!(put.unwrap_err(), Error::ConnectTimeout);
    assert_eq!(started.elapsed(), CONNECT_TIMEOUT);
}

/// A server that reads the request and then says nothing: no byte moves in either direction.
fn silent_after_request() -> Arc<FakeDial> {
    FakeDial::new(64 * 1024, |_, mut server| {
        tokio::spawn(async move {
            let _ = read_request(&mut server).await;
            std::future::pending::<()>().await;
        });
    })
}

/// [`silent_after_request`] over TLS.
fn silent_after_request_tls() -> (Arc<FakeDial>, CertPin) {
    FakeDial::tls(64 * 1024, |_, mut server| {
        tokio::spawn(async move {
            let _ = read_request(&mut server).await;
            std::future::pending::<()>().await;
        });
    })
}

#[tokio::test(start_paused = true)]
async fn thirty_seconds_of_silence_ends_a_request_and_twenty_nine_do_not() {
    let client = Client::with_dial(silent_after_request());
    let started = Instant::now();
    assert_eq!(post(&client).await.unwrap_err(), Error::NoProgress);
    assert_eq!(started.elapsed(), NO_PROGRESS);
    // A PUT has no total but the same clock.
    let (dial, pin) = silent_after_request_tls();
    let started = Instant::now();
    let put = Client::with_dial(dial)
        .put(9000, &pin, "/p", HOST, 1, Box::new(Cursor::new(vec![0u8])))
        .await;
    assert_eq!(put.unwrap_err(), Error::NoProgress);
    assert_eq!(started.elapsed(), NO_PROGRESS);

    // An answer that comes after 29 seconds is taken.
    let late = FakeDial::new(4096, |_, mut server| {
        tokio::spawn(async move {
            let _ = read_request(&mut server).await;
            tokio::time::sleep(secs(29)).await;
            let _ = server.write_all(&json(200, "{}")).await;
        });
    });
    assert_eq!(post(&Client::with_dial(late)).await.unwrap().status, 200);
}

#[tokio::test(start_paused = true)]
async fn the_clock_runs_from_the_dial_when_nothing_is_ever_sent_back() {
    // The server completes the handshake and then never reads a byte: the request cannot be
    // written past the pipe's buffer.
    let (dial, pin) = FakeDial::tls(4096, |_, server| {
        tokio::spawn(async move {
            let _server = server;
            std::future::pending::<()>().await;
        });
    });
    let client = Client::with_dial(dial);
    let started = Instant::now();
    let put = client
        .put(
            9000,
            &pin,
            "/p",
            HOST,
            100_000,
            Box::new(Cursor::new(vec![0u8; 100_000])),
        )
        .await;
    assert_eq!(put.unwrap_err(), Error::NoProgress);
    assert_eq!(started.elapsed(), NO_PROGRESS);
}

/// What a server that answers a byte at a time, `gap` apart, does with a connection.
fn trickle(gap: Duration, reply: Vec<u8>) -> impl Fn(u16, ServerEnd) + Send + Sync + 'static {
    move |_, mut server| {
        let reply = reply.clone();
        tokio::spawn(async move {
            let _ = read_request(&mut server).await;
            for byte in reply {
                tokio::time::sleep(gap).await;
                if server.write_all(&[byte]).await.is_err() {
                    return;
                }
            }
            let _ = server.shutdown().await;
        });
    }
}

/// The server answers a byte at a time, `gap` apart.
fn trickling(gap: Duration, reply: Vec<u8>) -> Arc<FakeDial> {
    FakeDial::new(4096, trickle(gap, reply))
}

/// [`trickling`] over TLS.
fn trickling_tls(gap: Duration, reply: Vec<u8>) -> (Arc<FakeDial>, CertPin) {
    FakeDial::tls(4096, trickle(gap, reply))
}

/// 60 seconds is the JSON routes' total; a PUT that keeps moving has none.
#[tokio::test(start_paused = true)]
async fn the_total_is_for_the_json_routes_and_a_moving_put_has_none() {
    let reply = answer(200, "", "ok");
    assert!(reply.len() > 40);
    let started = Instant::now();
    let client = Client::with_dial(trickling(secs(10), reply.clone()));
    assert_eq!(post(&client).await.unwrap_err(), Error::Timeout);
    assert_eq!(started.elapsed(), JSON_TOTAL);

    let started = Instant::now();
    let (dial, pin) = trickling_tls(secs(10), reply.clone());
    let got = client_put(&Client::with_dial(dial), &pin).await;
    assert_eq!(got.unwrap().status, 200);
    assert!(
        started.elapsed() > JSON_TOTAL * 3,
        "{:?}",
        started.elapsed()
    );

    // A trickle slower than the no-progress limit is still abandoned.
    let (dial, pin) = trickling_tls(secs(31), reply);
    let got = client_put(&Client::with_dial(dial), &pin).await;
    assert_eq!(got.unwrap_err(), Error::NoProgress);
}

async fn client_put(client: &Client, pin: &CertPin) -> Result<Answer, Error> {
    client
        .put(9000, pin, "/p", HOST, 1, Box::new(Cursor::new(vec![7u8])))
        .await
}

#[tokio::test(start_paused = true)]
async fn the_json_total_does_not_end_a_request_that_finishes_inside_it() {
    // 59 seconds of 5-second gaps would stall the total; 5 bytes at 10 seconds is 50 seconds.
    let reply = b"HTTP/1.1 204 X\r\n\r\n".to_vec();
    let client = Client::with_dial(trickling(secs(3), reply));
    let started = Instant::now();
    assert_eq!(post(&client).await.unwrap().status, 204);
    assert!(started.elapsed() < JSON_TOTAL);
}

#[tokio::test(start_paused = true)]
async fn a_cut_body_is_reported_as_cut_and_garbage_is_a_failure() {
    let dial = FakeDial::new(4096, |_, mut server| {
        tokio::spawn(async move {
            let _ = read_request(&mut server).await;
            let _ = server
                .write_all(b"HTTP/1.1 200 X\r\nContent-Length: 50\r\n\r\n{\"a\"")
                .await;
            let _ = server.shutdown().await;
        });
    });
    // The head arrived and the body did not: the answer says so, for the caller to treat as
    // not proved.
    let cut = post(&Client::with_dial(dial)).await.unwrap();
    assert_eq!((cut.status, cut.body), (200, AnswerBody::Cut));
    let (dial, _) = scripted(b"not http at all\r\n\r\n".to_vec());
    assert_eq!(
        post(&Client::with_dial(dial)).await.unwrap_err(),
        Error::Failed
    );
}

/// The real socket path, once: a loopback listener, the production dial. The PUT's real socket
/// path, over TLS, is `tls::the_loopback_client_puts_over_a_real_socket`.
#[tokio::test]
async fn a_request_over_a_real_loopback_socket() {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let seen = read_request(&mut socket).await.unwrap();
        socket
            .write_all(&json(200, "{\"state\":\"open\"}"))
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
        seen
    });
    let host = format!("127.0.0.1:{port}");
    let got = Client::loopback()
        .json(
            port,
            &JsonRequest {
                method: Method::POST,
                target: "/api/v1/agent/uploads",
                host: &host,
                bearer: "t",
                body: Some(b"{}".to_vec()),
                nonce: shared_crypto::random_32().unwrap(),
                reads: reads_body,
            },
        )
        .await
        .unwrap();
    assert_eq!(got.status, 200);
    let seen = server.await.unwrap();
    assert!(
        seen.request_line()
            .starts_with("POST /api/v1/agent/uploads HTTP/1.1")
    );
    assert!(seen.header("authorization").as_deref() == Some("Bearer t"));
}

/// A PUT is judged by its status alone: its body is neither waited for nor read.
#[tokio::test(start_paused = true)]
async fn a_puts_answer_body_is_never_read() {
    // A 200 whose declared body never comes.
    let (dial, pin) = FakeDial::tls(4096, |_, mut server| {
        tokio::spawn(async move {
            let _ = read_request(&mut server).await;
            let _ = server
                .write_all(b"HTTP/1.1 200 X\r\nContent-Length: 10\r\n\r\n12")
                .await;
            std::future::pending::<()>().await;
        });
    });
    let started = Instant::now();
    let got = client_put(&Client::with_dial(dial), &pin).await.unwrap();
    assert_eq!((got.status, got.body), (200, AnswerBody::Unread));
    assert_eq!(started.elapsed(), Duration::ZERO);
}

/// The reader does not start before the connection is up, and stops when the put is dropped.
#[tokio::test]
async fn the_reader_starts_after_the_connect_and_stops_with_the_put() {
    let (source, produced, _) = pattern(1024 * 1024 * 1024);
    let (dial, _) = scripted(json(200, "{}"));
    dial.set(Mode::Refuse);
    let client = Client::with_dial(dial.clone());
    assert_eq!(
        client
            .put(9000, &any_pin(), "/p", HOST, 1024 * 1024 * 1024, source)
            .await
            .unwrap_err(),
        Error::NotSent
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        produced.load(Ordering::SeqCst),
        0,
        "no read before a connection"
    );

    // A server that reads the head only: the reader fills its queue, then the put is dropped.
    let (dial, pin) = FakeDial::tls(1024, |_, mut server| {
        tokio::spawn(async move {
            let mut head = [0u8; 256];
            let _ = server.read(&mut head).await;
            std::future::pending::<()>().await;
        });
    });
    let client = Client::with_dial(dial);
    let (source, produced, _) = pattern(1024 * 1024 * 1024);
    let put = client.put(9000, &pin, "/p", HOST, 1024 * 1024 * 1024, source);
    tokio::select! {
        _ = put => panic!("the put cannot finish"),
        () = tokio::time::sleep(Duration::from_millis(300)) => {}
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after_drop = produced.load(Ordering::SeqCst);
    assert!(after_drop > 0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        produced.load(Ordering::SeqCst),
        after_drop,
        "the reader kept reading"
    );
}

/// A server that takes its input slowly but steadily keeps a PUT alive: the clock counts bytes
/// written, not only bytes read. The limit is shortened so that the test is short.
#[tokio::test]
async fn a_put_the_server_reads_slowly_is_kept_alive_by_the_bytes_it_takes() {
    let limit = Duration::from_millis(400);
    let length = 6 * 1024usize;
    let slow = |per_read: Duration| {
        FakeDial::tls_throttled(256, per_read, |_, mut server| {
            // The TLS layer hands the request over whole; the pipe under it yields 128 bytes per
            // `per_read`, and each of those moves bytes on the socket.
            tokio::spawn(async move {
                let _ = read_request(&mut server).await;
                let _ = server.write_all(&answer(200, "", "")).await;
                let _ = server.shutdown().await;
            });
        })
    };
    let put = |(dial, pin): (Arc<FakeDial>, CertPin)| async move {
        Client::with_dial(dial)
            .with_no_progress(limit)
            .put(
                9000,
                &pin,
                "/p",
                HOST,
                length as u64,
                Box::new(Cursor::new(vec![3u8; length])),
            )
            .await
    };
    // 6 KiB at 128 bytes per 100 ms is about 5 s, well past the 400 ms limit, and every read
    // moves bytes.
    let started = std::time::Instant::now();
    let got = put(slow(Duration::from_millis(100))).await.unwrap();
    assert_eq!(got.status, 200);
    assert!(started.elapsed() > limit * 3, "{:?}", started.elapsed());
    // The same server reading once per limit-and-a-half gives no progress in between.
    let got = put(slow(Duration::from_millis(700))).await;
    assert_eq!(got.unwrap_err(), Error::NoProgress);
}

/// A writer that fails after `ok` bytes, as a full disk does.
struct Failing {
    ok: usize,
}

impl Write for Failing {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.ok == 0 {
            return Err(io::ErrorKind::StorageFull.into());
        }
        let n = buf.len().min(self.ok);
        self.ok -= n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn a_get_streams_the_body_and_sends_no_authorization_and_no_range() {
    let body = vec![7u8; 150_000];
    let mut reply = format!(
        "HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    reply.extend_from_slice(&body);
    let (dial, seen, pin) = {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let (dial, pin) = FakeDial::tls(64 * 1024, move |_, mut server| {
            let (reply, log) = (reply.clone(), log.clone());
            tokio::spawn(async move {
                if let Some(request) = read_request(&mut server).await {
                    log.lock().unwrap().push(request);
                }
                let _ = server.write_all(&reply).await;
                let _ = server.shutdown().await;
            });
        });
        (dial, seen, pin)
    };
    let got = Client::with_dial(dial)
        .get(9000, &pin, "/obj?sig=1", HOST, 1 << 20, Vec::new())
        .await
        .unwrap();
    let GetBody::Complete { length, writer } = got.body else {
        panic!("the body was not read");
    };
    assert_eq!((got.status, length), (200, 150_000));
    assert_eq!(writer, body);
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].request_line(), "GET /obj?sig=1 HTTP/1.1");
    assert_eq!(seen[0].header("authorization"), None);
    assert_eq!(seen[0].header("range"), None);
    assert_eq!(seen[0].header("host").as_deref(), Some(HOST));
    assert_eq!(seen[0].header("connection").as_deref(), Some("close"));
}

#[tokio::test(start_paused = true)]
async fn a_get_that_is_not_a_complete_200_reads_no_body() {
    let get = |reply: Vec<u8>, max: u64| async move {
        let (dial, _, pin) = scripted_tls(reply);
        Client::with_dial(dial)
            .get(9000, &pin, "/o", HOST, max, Vec::new())
            .await
            .unwrap()
    };
    let got = get(answer(302, "Location: http://127.0.0.1:1/\r\n", "x"), 10).await;
    assert!(matches!((got.status, got.body), (302, GetBody::Unread)));
    let got = get(answer(404, "Retry-After: 5\r\n", "x"), 10).await;
    assert_eq!(got.retry_after.as_deref(), Some("5"));
    assert!(matches!(got.body, GetBody::Unread));
    let got = get(answer(200, "", "0123456789ab"), 10).await;
    assert!(matches!((got.status, got.body), (200, GetBody::BadLength)));
    let got = get(answer(200, "", "0123456789"), 10).await;
    assert!(matches!(got.body, GetBody::Complete { length: 10, .. }));
    // An empty body is a length of 0, which is canonical.
    let got = get(answer(200, "", ""), 10).await;
    assert!(matches!(got.body, GetBody::Complete { length: 0, .. }));
}

#[tokio::test(start_paused = true)]
async fn a_get_with_a_failing_writer_is_a_write_error() {
    let (dial, _, pin) = scripted_tls(answer(200, "", &"x".repeat(100_000)));
    let error = Client::with_dial(dial)
        .get(9000, &pin, "/o", HOST, 1 << 20, Failing { ok: 70_000 })
        .await
        .map(|_| ())
        .unwrap_err();
    assert_eq!(error, Error::Write(io::ErrorKind::StorageFull));
}

#[tokio::test(start_paused = true)]
async fn a_get_refused_at_connect_sends_nothing() {
    let (dial, _) = scripted(Vec::new());
    dial.set(Mode::Refuse);
    let error = Client::with_dial(dial)
        .get(9000, &any_pin(), "/o", HOST, 10, Vec::new())
        .await
        .map(|_| ())
        .unwrap_err();
    assert_eq!(error, Error::NotSent);
}

/// The TLS dial (ADR-505): the server's end of each pipe is a TLS server.
mod tls {
    use std::sync::atomic::AtomicUsize;

    use ngr_pinned_tls::test_server::{Acceptor, Identity, Key};

    use super::*;

    const TARGET: &str = "/obj?sig=secret";

    /// A fake dial whose server accepts TLS with `acceptor`, reads one request and answers it with
    /// `reply`. What it saw is kept: the requests read, and how many handshakes it accepted.
    struct Server {
        dial: Arc<FakeDial>,
        requests: Arc<Mutex<Vec<Seen>>>,
        accepted: Arc<AtomicUsize>,
    }

    fn serving(acceptor: Acceptor, reply: Vec<u8>) -> Server {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let (seen, count) = (requests.clone(), accepted.clone());
        let dial = FakeDial::new(64 * 1024, move |_, server| {
            let (acceptor, reply) = (acceptor.clone(), reply.clone());
            let (seen, count) = (seen.clone(), count.clone());
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(server).await else {
                    return;
                };
                count.fetch_add(1, Ordering::SeqCst);
                if let Some(request) = read_request(&mut stream).await {
                    seen.lock().unwrap().push(request);
                    let _ = stream.write_all(&reply).await;
                    let _ = stream.shutdown().await;
                }
            });
        });
        Server {
            dial,
            requests,
            accepted,
        }
    }

    impl Server {
        /// Lets the server's task run to the end of what it will do.
        async fn settle(&self) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        /// The server completed no handshake and read no request.
        fn assert_untouched(&self) {
            assert_eq!(self.accepted.load(Ordering::SeqCst), 0);
            assert!(self.requests.lock().unwrap().is_empty());
        }
    }

    fn pin_of(identity: &Identity) -> CertPin {
        CertPin::of_certificate(identity.certificate())
    }

    async fn put(client: &Client, pin: &CertPin) -> Result<Answer, Error> {
        client
            .put(
                9000,
                pin,
                TARGET,
                HOST,
                3,
                Box::new(Cursor::new(b"abc".to_vec())),
            )
            .await
    }

    async fn get(client: &Client, pin: &CertPin) -> Result<Got<Vec<u8>>, Error> {
        client
            .get(9000, pin, TARGET, HOST, 1 << 20, Vec::new())
            .await
    }

    #[tokio::test(start_paused = true)]
    async fn the_right_certificate_carries_a_put_and_a_get() {
        let identity = Identity::generate(Key::P256);
        let pin = pin_of(&identity);
        let server = serving(Acceptor::new(&identity), answer(200, "", "hello"));
        let client = Client::with_dial(server.dial.clone());
        let put = put(&client, &pin).await.unwrap();
        assert_eq!(put.status, 200);
        let got = get(&client, &pin).await.unwrap();
        assert_eq!(got.status, 200);
        let GetBody::Complete { length, writer } = got.body else {
            panic!("the body was not read: {:?}", got.body);
        };
        assert_eq!((length, writer.as_slice()), (5, b"hello".as_slice()));
        let seen = server.requests.lock().unwrap();
        assert_eq!(seen[0].request_line(), format!("PUT {TARGET} HTTP/1.1"));
        assert_eq!(seen[0].body, b"abc");
        assert_eq!(seen[1].request_line(), format!("GET {TARGET} HTTP/1.1"));
    }

    #[tokio::test(start_paused = true)]
    async fn another_certificate_is_refused_and_no_request_byte_is_sent() {
        let served = Identity::generate(Key::P256);
        let other = pin_of(&Identity::generate(Key::P256));
        let server = serving(Acceptor::new(&served), answer(200, "", ""));
        let client = Client::with_dial(server.dial.clone());
        assert_eq!(
            put(&client, &other).await.unwrap_err(),
            Error::Tls(Refused::Certificate)
        );
        assert_eq!(
            get(&client, &other).await.unwrap_err(),
            Error::Tls(Refused::Certificate)
        );
        server.settle().await;
        server.assert_untouched();
    }

    #[tokio::test(start_paused = true)]
    async fn the_right_certificate_without_its_key_is_refused_and_no_request_byte_is_sent() {
        let real = Identity::generate(Key::P256);
        let other_key = Identity::generate(Key::P256);
        let impostor = Acceptor::presenting(&[real.certificate()], &other_key);
        let server = serving(impostor, answer(200, "", ""));
        let client = Client::with_dial(server.dial.clone());
        // The pin is right, so the signature is what refuses.
        assert_eq!(
            put(&client, &pin_of(&real)).await.unwrap_err(),
            Error::Tls(Refused::Signature)
        );
        server.settle().await;
        server.assert_untouched();
    }

    #[tokio::test(start_paused = true)]
    async fn a_plain_http_server_is_refused_and_never_sees_the_request() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let seen = received.clone();
        let dial = FakeDial::new(64 * 1024, move |_, mut server| {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                while let Ok(n) = server.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    seen.lock().unwrap().extend_from_slice(&buf[..n]);
                    let _ = server.write_all(&answer(400, "", "")).await;
                }
            });
        });
        let client = Client::with_dial(dial);
        let pin = pin_of(&Identity::generate(Key::P256));
        assert_eq!(
            put(&client, &pin).await.unwrap_err(),
            Error::Tls(Refused::Handshake(io::ErrorKind::InvalidData))
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
        let received = received.lock().unwrap();
        assert!(!received.is_empty(), "the server read the hello");
        let text = String::from_utf8_lossy(&received);
        assert!(
            !text.contains("/obj") && !text.contains("sig=secret") && !text.contains("PUT"),
            "the request reached a plain server"
        );
    }

    /// A TLS server that writes `reply` and drops the connection, with no TLS close_notify (what
    /// the object server may do).
    fn closing_without_close_notify(reply: Vec<u8>) -> (Arc<FakeDial>, CertPin) {
        FakeDial::tls(64 * 1024, move |_, mut server| {
            let reply = reply.clone();
            tokio::spawn(async move {
                let _ = read_request(&mut server).await;
                let _ = server.write_all(&reply).await;
                let _ = server.flush().await;
                // Dropped here: the pipe closes, nothing says the stream ended.
            });
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_close_without_close_notify_after_a_complete_body_is_a_success() {
        let (dial, pin) = closing_without_close_notify(answer(200, "", "hello"));
        let client = Client::with_dial(dial);
        let got = get(&client, &pin).await.unwrap();
        let GetBody::Complete { length, writer } = got.body else {
            panic!("the body was not read: {:?}", got.body);
        };
        assert_eq!(
            (got.status, length, writer.as_slice()),
            (200, 5, b"hello".as_slice())
        );
        // A PUT is judged by its status, and a complete answer is one.
        let put = put(&client, &pin).await.unwrap();
        assert_eq!(put.status, 200);
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_cut_short_or_ended_by_the_close_alone_is_never_a_success() {
        // The head declares ten bytes and three arrive before the close.
        let (dial, pin) = closing_without_close_notify(
            b"HTTP/1.1 200 X\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc".to_vec(),
        );
        let error = get(&Client::with_dial(dial), &pin).await.unwrap_err();
        assert_eq!(error, Error::Failed);
        // No length: the body would end with the connection, and a close with no close_notify
        // cannot tell the end from a cut. The GET never reads it as a complete body.
        let (dial, pin) = closing_without_close_notify(
            b"HTTP/1.1 200 X\r\nConnection: close\r\n\r\nabc".to_vec(),
        );
        let got = get(&Client::with_dial(dial), &pin).await;
        assert!(
            !matches!(
                got,
                Ok(Got {
                    body: GetBody::Complete { .. },
                    ..
                })
            ),
            "{got:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_close_during_the_handshake_is_a_refusal() {
        let dial = FakeDial::new(64 * 1024, |_, mut server| {
            tokio::spawn(async move {
                let mut hello = vec![0u8; 4096];
                let _ = server.read(&mut hello).await;
                // Dropping the server's end closes the pipe.
            });
        });
        let client = Client::with_dial(dial);
        let pin = pin_of(&Identity::generate(Key::P256));
        assert_eq!(
            put(&client, &pin).await.unwrap_err(),
            Error::Tls(Refused::Handshake(io::ErrorKind::UnexpectedEof))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_silent_after_the_connect_ends_at_the_connect_bound() {
        let dial = FakeDial::new(64 * 1024, |_, server| {
            tokio::spawn(async move {
                let _held = server;
                std::future::pending::<()>().await;
            });
        });
        let client = Client::with_dial(dial);
        let pin = pin_of(&Identity::generate(Key::P256));
        let started = Instant::now();
        assert_eq!(put(&client, &pin).await.unwrap_err(), Error::ConnectTimeout);
        assert_eq!(started.elapsed(), CONNECT_TIMEOUT);
        let started = Instant::now();
        assert_eq!(get(&client, &pin).await.unwrap_err(), Error::ConnectTimeout);
        assert_eq!(started.elapsed(), CONNECT_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stall_after_the_handshake_ends_a_put_by_the_no_progress_clock() {
        let identity = Identity::generate(Key::P256);
        let acceptor = Acceptor::new(&identity);
        let dial = FakeDial::new(64 * 1024, move |_, server| {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(server).await else {
                    return;
                };
                let _ = read_request(&mut stream).await;
                std::future::pending::<()>().await;
            });
        });
        let client = Client::with_dial(dial);
        let started = Instant::now();
        assert_eq!(
            put(&client, &pin_of(&identity)).await.unwrap_err(),
            Error::NoProgress
        );
        assert_eq!(started.elapsed(), NO_PROGRESS);
    }

    /// The same dial over a real loopback socket: the pin and the stream are the production ones.
    #[tokio::test]
    async fn the_loopback_client_puts_over_a_real_socket() {
        let identity = Identity::generate(Key::P256);
        let acceptor = Acceptor::new(&identity);
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(socket).await.unwrap();
            let seen = read_request(&mut stream).await.unwrap();
            stream.write_all(&answer(200, "", "")).await.unwrap();
            stream.shutdown().await.unwrap();
            seen
        });
        let body = vec![9u8; 200 * 1024];
        let put = Client::loopback()
            .put(
                port,
                &pin_of(&identity),
                TARGET,
                HOST,
                body.len() as u64,
                Box::new(Cursor::new(body.clone())),
            )
            .await
            .unwrap();
        assert_eq!(put.status, 200);
        let seen = served.await.unwrap();
        assert_eq!(seen.request_line(), format!("PUT {TARGET} HTTP/1.1"));
        assert_eq!(seen.body, body);
        assert_eq!(seen.header("authorization"), None);
    }
}
