//! A dial that hands the client one end of an in-memory pipe, and the helpers that play the
//! server's end, so a request runs against an in-process server on a paused clock.

use std::io;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use crate::control::dial::{Dialled, PortDial, Stream};

/// What the dial does with a call.
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Connect,
    Refuse,
    /// The dial never finishes.
    Hang,
}

type Handler = dyn Fn(u16, DuplexStream) + Send + Sync;

/// The fake dial: each call makes a pipe, gives the server's end to the handler and returns the
/// client's.
pub(crate) struct FakeDial {
    handler: Box<Handler>,
    pub(crate) mode: Mutex<Mode>,
    /// The ports dialled, in order.
    pub(crate) dialled: Mutex<Vec<u16>>,
    /// The pipe's buffer, in bytes per direction.
    capacity: usize,
}

impl FakeDial {
    pub(crate) fn new(
        capacity: usize,
        handler: impl Fn(u16, DuplexStream) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            handler: Box::new(handler),
            mode: Mutex::new(Mode::Connect),
            dialled: Mutex::new(Vec::new()),
            capacity,
        })
    }

    pub(crate) fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }

    pub(crate) fn count(&self) -> usize {
        self.dialled.lock().unwrap().len()
    }
}

impl PortDial for FakeDial {
    fn dial_port(&self, port: u16) -> Dialled<'_> {
        self.dialled.lock().unwrap().push(port);
        let mode = *self.mode.lock().unwrap();
        let (client, server) = tokio::io::duplex(self.capacity);
        if matches!(mode, Mode::Connect) {
            (self.handler)(port, server);
        }
        Box::pin(async move {
            match mode {
                Mode::Connect => Ok(Box::new(client) as Stream),
                Mode::Refuse => Err(io::ErrorKind::ConnectionRefused.into()),
                Mode::Hang => std::future::pending::<io::Result<Stream>>().await,
            }
        })
    }
}

/// One request as the server read it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Seen {
    pub(crate) head: String,
    pub(crate) body: Vec<u8>,
}

impl Seen {
    pub(crate) fn request_line(&self) -> &str {
        self.head.lines().next().unwrap_or_default()
    }

    /// The value of a header, by lowercase name.
    pub(crate) fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
}

/// Reads the head of a request, and its body when the head has a length.
pub(crate) async fn read_request(server: &mut DuplexStream) -> Option<Seen> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        if server.read(&mut byte).await.ok()? == 0 {
            return None;
        }
        raw.push(byte[0]);
    }
    let mut seen = Seen {
        head: String::from_utf8_lossy(&raw).into_owned(),
        body: Vec::new(),
    };
    let length = seen
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    seen.body = vec![0u8; length];
    server.read_exact(&mut seen.body).await.ok()?;
    Some(seen)
}

/// A complete answer with `body` and a declared length.
pub(crate) fn answer(status: u16, extra_headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} X\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A JSON answer.
pub(crate) fn json(status: u16, body: &str) -> Vec<u8> {
    answer(status, "Content-Type: application/json\r\n", body)
}

/// A dial whose server answers every request with `reply` after reading it whole, and records
/// what it read.
pub(crate) fn scripted(reply: Vec<u8>) -> (Arc<FakeDial>, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let dial = FakeDial::new(64 * 1024, move |_, mut server| {
        let reply = reply.clone();
        let log = log.clone();
        tokio::spawn(async move {
            if let Some(request) = read_request(&mut server).await {
                log.lock().unwrap().push(request);
            }
            let _ = server.write_all(&reply).await;
            let _ = server.shutdown().await;
        });
    });
    (dial, seen)
}
