//! The agent's HTTP/1 client for its calls to the server (design 6.4, 13.3, ADR-470; the pinned
//! origin is ADR-456's). `client.rs` is the worker client; this is the server's.
//!
//! One call is one connection: the agent opens a TCP stream to `127.0.0.1:<port>` itself and runs
//! hyper's HTTP/1 client connection over it. There is no pool, no name resolution, no redirect
//! handling (a 3xx is an answer like any other, and the caller decides) and no retry. The request
//! carries `Host`, `Connection: close`, `Content-Type` and the body's length, and no `Origin`.
//!
//! What the caller needs to judge the answer is in [`Reply`]: the status, `Retry-After`, whether
//! the Content-Type is `application/json`, and, for the one status the caller asks for, the body
//! (at most [`MAX_BODY`] bytes). The body of any other status is never read. A failure says
//! whether anything was sent ([`PostError::NotSent`], [`PostError::ConnectTimeout`]) or whether
//! the outcome is unknown ([`PostError::Unknown`]): the caller's retry rule depends on it.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, RETRY_AFTER};
use hyper::{HeaderMap, Method};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

/// How long the connect may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the exchange may take in all, from the connection's start to the last byte of the
/// body.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest answer head accepted, in bytes.
pub const MAX_HEAD: usize = 16 * 1024;
/// The most header lines an answer may have.
pub const MAX_HEADERS: usize = 64;
/// The largest body read, in bytes.
pub const MAX_BODY: usize = 4096;

/// The time limits of one call.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub connect: Duration,
    pub total: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            total: TOTAL_TIMEOUT,
        }
    }
}

/// A POST to `127.0.0.1:<port>`.
#[derive(Debug)]
pub struct PostRequest<'a> {
    pub port: u16,
    /// The `Host` header's value.
    pub host: &'a str,
    pub path: &'a str,
    /// The JSON body.
    pub body: Vec<u8>,
}

/// What came back.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    /// The single `Retry-After` value, when the answer has exactly one that is text.
    pub retry_after: Option<String>,
    /// Exactly one Content-Type, whose media type is `application/json` (parameters allowed).
    pub json_content_type: bool,
    /// The body, read only when the status is the one the caller asked for.
    pub body: Option<ReplyBody>,
}

/// A body that was asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum ReplyBody {
    Bytes(Vec<u8>),
    /// More than [`MAX_BODY`] bytes: declared or arrived. The bytes are not kept.
    TooLarge,
}

/// Why no answer came back.
#[derive(Debug, PartialEq, Eq)]
pub enum PostError {
    /// The connection was refused or the address is unreachable: nothing was sent.
    NotSent(std::io::ErrorKind),
    /// The connect did not finish in time: nothing was sent.
    ConnectTimeout,
    /// The request may have been sent and no complete answer came: a time out, a reset, a head
    /// over the limits, an answer that is not HTTP, or a body that is cut or late.
    Unknown,
}

/// Aborts the connection's task when the call ends, however it ends.
struct Driver(JoinHandle<()>);

impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Sends `request` and returns the answer. The body is read only when the status is
/// `body_status`.
pub async fn post(
    request: &PostRequest<'_>,
    body_status: u16,
    timeouts: Timeouts,
) -> Result<Reply, PostError> {
    // The address is a literal: no name is resolved.
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, request.port));
    let stream = match tokio::time::timeout(timeouts.connect, TcpStream::connect(addr)).await {
        Err(_) => return Err(PostError::ConnectTimeout),
        Ok(Err(error)) => return Err(PostError::NotSent(error.kind())),
        Ok(Ok(stream)) => stream,
    };
    tokio::time::timeout(timeouts.total, exchange(stream, request, body_status))
        .await
        .unwrap_or(Err(PostError::Unknown))
}

async fn exchange(
    stream: TcpStream,
    request: &PostRequest<'_>,
    body_status: u16,
) -> Result<Reply, PostError> {
    let (mut sender, connection) = http1::Builder::new()
        .max_buf_size(MAX_HEAD)
        .max_headers(MAX_HEADERS)
        .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
        .await
        .map_err(|_| PostError::Unknown)?;
    let _driver = Driver(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let http_request = hyper::Request::builder()
        .method(Method::POST)
        .uri(request.path)
        .header(HOST, request.host)
        .header(CONNECTION, "close")
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(request.body.clone())))
        .map_err(|_| PostError::Unknown)?;
    let response = sender
        .send_request(http_request)
        .await
        .map_err(|_| PostError::Unknown)?;
    let status = response.status().as_u16();
    let headers = response.headers();
    let retry_after = single_text(headers, &RETRY_AFTER);
    let json_content_type = is_json(headers);
    let body = if status == body_status {
        Some(read_body(response).await?)
    } else {
        None
    };
    Ok(Reply {
        status,
        retry_after,
        json_content_type,
        body,
    })
}

async fn read_body(
    response: hyper::Response<hyper::body::Incoming>,
) -> Result<ReplyBody, PostError> {
    let declared =
        single_text(response.headers(), &CONTENT_LENGTH).and_then(|text| text.parse::<u64>().ok());
    if declared.is_some_and(|len| len > MAX_BODY as u64) {
        return Ok(ReplyBody::TooLarge);
    }
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| PostError::Unknown)?;
        if let Ok(data) = frame.into_data() {
            if bytes.len() + data.len() > MAX_BODY {
                return Ok(ReplyBody::TooLarge);
            }
            bytes.extend_from_slice(&data);
        }
    }
    Ok(ReplyBody::Bytes(bytes))
}

/// The header's value when the answer has exactly one and it is text.
fn single_text(headers: &HeaderMap, name: &hyper::header::HeaderName) -> Option<String> {
    let mut values = headers.get_all(name).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return None;
    };
    value.to_str().ok().map(str::to_owned)
}

fn is_json(headers: &HeaderMap) -> bool {
    single_text(headers, &CONTENT_TYPE)
        .and_then(|v| v.split(';').next().map(|e| e.trim().to_owned()))
        .is_some_and(|essence| essence.eq_ignore_ascii_case("application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A listener that answers its first connection with `behaviour`, given the stream after the
    /// whole request (head and body) was read. The body is read too: closing a socket with
    /// unread input makes Windows reset the connection, and the client would see the reset
    /// instead of the answer.
    fn stub(behaviour: impl FnOnce(std::net::TcpStream) + Send + 'static) -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while !seen.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    return;
                }
                seen.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&seen).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            if stream.read_exact(&mut body).is_err() {
                return;
            }
            behaviour(stream);
        });
        port
    }

    fn request(port: u16) -> PostRequest<'static> {
        PostRequest {
            port,
            host: "127.0.0.1",
            path: "/x",
            body: b"{}".to_vec(),
        }
    }

    fn run(port: u16, body_status: u16, timeouts: Timeouts) -> Result<Reply, PostError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(post(&request(port), body_status, timeouts))
    }

    #[test]
    fn a_201_has_its_body_read_and_its_type_checked() {
        let port = stub(|mut s| {
            s.write_all(
                b"HTTP/1.1 201 Created\r\nContent-Type: application/json; charset=utf-8\r\n\
                  Content-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
            .unwrap();
        });
        let reply = run(port, 201, Timeouts::default()).unwrap();
        assert_eq!(reply.status, 201);
        assert!(reply.json_content_type);
        assert_eq!(reply.body, Some(ReplyBody::Bytes(b"{}".to_vec())));
    }

    /// The body of another status is not read: the answer's head alone decides.
    #[test]
    fn the_body_of_another_status_is_not_read() {
        let port = stub(|mut s| {
            // Declares a body and never sends it.
            s.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 17\r\nContent-Length: 50\r\n\r\n",
            )
            .unwrap();
            std::thread::sleep(Duration::from_secs(2));
        });
        let reply = run(port, 201, Timeouts::default()).unwrap();
        assert_eq!(reply.status, 429);
        assert_eq!(reply.retry_after.as_deref(), Some("17"));
        assert!(reply.body.is_none());
    }

    #[test]
    fn a_redirect_is_returned_not_followed() {
        let port = stub(|mut s| {
            s.write_all(
                b"HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:1/\r\n\
                  Content-Length: 0\r\n\r\n",
            )
            .unwrap();
        });
        let reply = run(port, 201, Timeouts::default()).unwrap();
        assert_eq!(reply.status, 307);
    }

    #[test]
    fn a_body_over_the_limit_is_too_large_when_declared_and_when_it_arrives() {
        let declared = stub(|mut s| {
            s.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 5000\r\n\r\n")
                .unwrap();
            std::thread::sleep(Duration::from_secs(1));
        });
        let reply = run(declared, 201, Timeouts::default()).unwrap();
        assert_eq!(reply.body, Some(ReplyBody::TooLarge));
        let streamed = stub(|mut s| {
            s.write_all(b"HTTP/1.1 201 Created\r\nConnection: close\r\n\r\n")
                .unwrap();
            let _ = s.write_all(&vec![b'a'; MAX_BODY + 1]);
        });
        let reply = run(streamed, 201, Timeouts::default()).unwrap();
        assert_eq!(reply.body, Some(ReplyBody::TooLarge));
    }

    #[test]
    fn a_connection_that_nothing_listens_on_is_not_sent() {
        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.local_addr().unwrap().port()
        };
        assert!(matches!(
            run(port, 201, Timeouts::default()),
            Err(PostError::NotSent(_))
        ));
    }

    #[test]
    fn an_answer_that_does_not_come_in_time_is_unknown() {
        let port = stub(|_stream| std::thread::sleep(Duration::from_secs(3)));
        let timeouts = Timeouts {
            connect: Duration::from_secs(5),
            total: Duration::from_millis(300),
        };
        assert_eq!(run(port, 201, timeouts).unwrap_err(), PostError::Unknown);
    }

    #[test]
    fn a_cut_body_a_reset_a_big_head_and_a_non_http_answer_are_unknown() {
        let cut = stub(|mut s| {
            s.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 10\r\n\r\nabc")
                .unwrap();
        });
        assert_eq!(
            run(cut, 201, Timeouts::default()).unwrap_err(),
            PostError::Unknown
        );
        let reset = stub(drop);
        assert_eq!(
            run(reset, 201, Timeouts::default()).unwrap_err(),
            PostError::Unknown
        );
        let big = stub(|mut s| {
            let mut head = b"HTTP/1.1 403 Forbidden\r\nX-Pad: ".to_vec();
            head.extend(std::iter::repeat_n(b'a', MAX_HEAD + 100));
            head.extend_from_slice(b"\r\n\r\n");
            let _ = s.write_all(&head);
        });
        assert_eq!(
            run(big, 201, Timeouts::default()).unwrap_err(),
            PostError::Unknown
        );
        let many = stub(|mut s| {
            let mut head = b"HTTP/1.1 403 Forbidden\r\n".to_vec();
            for i in 0..=MAX_HEADERS {
                head.extend_from_slice(format!("X-{i}: a\r\n").as_bytes());
            }
            head.extend_from_slice(b"\r\n");
            let _ = s.write_all(&head);
        });
        assert_eq!(
            run(many, 201, Timeouts::default()).unwrap_err(),
            PostError::Unknown
        );
        let not_http = stub(|mut s| {
            let _ = s.write_all(b"SSH-2.0-OpenSSH_9.6\r\n\r\n");
        });
        assert_eq!(
            run(not_http, 201, Timeouts::default()).unwrap_err(),
            PostError::Unknown
        );
    }
}
