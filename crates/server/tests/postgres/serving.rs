//! The guard and the health route over real TCP against a running server on an ephemeral port.
//! The requests are written by hand so that the test controls every header.
//!
//! Most tests serve over a pool that points at no database (health then answers 503); the ones
//! that need the database up start a throwaway PostgreSQL.

use std::time::Duration;

use std::sync::Arc;

use server::config::{Config, Database, Http, InstructionKeyConfig, Profile};
use server::{InstructionKey, Server, db};
use shared_crypto::SigningKey;
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::support::{TestPostgres, free_port};

/// A key made in memory, for the tests that do not start from the database.
fn test_key() -> Arc<InstructionKey> {
    Arc::new(InstructionKey::from_signer(Arc::new(
        SigningKey::generate().unwrap(),
    )))
}

pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) head: String,
    pub(crate) body: String,
}

impl Reply {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then_some(v.trim())
        })
    }
}

/// Sends `raw` (a full request head ending in a blank line, plus any body) and reads the answer
/// until the server closes the connection.
pub(crate) async fn exchange(addr: std::net::SocketAddr, raw: &str) -> Reply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut buf))
        .await
        .expect("the server closes the connection after the answer")
        .ok();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"));
    Reply {
        status,
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

fn request(method: &str, target: &str, headers: &[String]) -> String {
    let mut raw = format!("{method} {target} HTTP/1.1\r\n");
    for h in headers {
        raw.push_str(h);
        raw.push_str("\r\n");
    }
    raw.push_str("Connection: close\r\nContent-Length: 0\r\n\r\n");
    raw
}

pub(crate) struct Running {
    pub(crate) addr: std::net::SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    force: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    /// Serves over a pool that points at a port nothing listens on.
    async fn start() -> Running {
        Running::start_with_grace(2).await
    }

    async fn start_with_grace(grace_secs: u64) -> Running {
        let options = PgConnectOptions::new_without_pgpass()
            .host("127.0.0.1")
            .port(free_port())
            .ssl_mode(PgSslMode::Disable);
        let pool = db::pool_lazy(options, Duration::from_secs(1));
        Running::serve(
            Http {
                port: 0,
                shutdown_grace_secs: grace_secs,
            },
            pool,
        )
        .await
    }

    /// Starts as a deployment does: connect, migrate, bind.
    async fn start_with_database(pg: &TestPostgres, grace_secs: u64) -> Running {
        let mut database = Database::new(pg.url());
        database.connect_timeout_secs = 5;
        let config = Config {
            profile: Profile::Local,
            http: Http {
                port: 0,
                shutdown_grace_secs: grace_secs,
            },
            database,
            instruction_key: InstructionKeyConfig {
                path: pg.key_path(),
            },
        };
        let server = Server::start(&config).await.unwrap();
        Running::spawn(server)
    }

    async fn serve(http: Http, pool: sqlx::PgPool) -> Running {
        Running::spawn(Server::bind(&http, pool, test_key()).await.unwrap())
    }

    pub(crate) fn spawn(server: Server) -> Running {
        let addr = server.local_addr();
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (force, force_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            server
                .run(
                    async {
                        let _ = stop_rx.await;
                    },
                    async {
                        let _ = force_rx.await;
                    },
                )
                .await
                .unwrap();
        });
        Running {
            addr,
            stop: Some(stop),
            force: Some(force),
            task,
        }
    }

    pub(crate) fn host(&self) -> String {
        format!("Host: 127.0.0.1:{}", self.addr.port())
    }

    /// Begins the graceful stop and returns once the listener no longer accepts.
    async fn begin_stop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(self.addr).await.is_ok() {
            assert!(std::time::Instant::now() < deadline, "still accepting");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub(crate) async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.joined().await;
    }

    async fn joined(self) {
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("run returns after the shutdown")
            .unwrap();
    }
}

#[tokio::test]
async fn the_listener_is_on_loopback_and_health_is_200() {
    let pg = TestPostgres::start();
    let s = Running::start_with_database(&pg, 2).await;
    assert_eq!(s.addr.ip().to_string(), "127.0.0.1");
    let r = exchange(s.addr, &request("GET", "/api/v1/health", &[s.host()])).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.body.trim(), r#"{"status":"ok"}"#);
    s.finish().await;
}

#[tokio::test]
async fn health_is_503_when_the_database_is_down_and_200_when_it_is_up() {
    let mut pg = TestPostgres::start();
    let s = Running::start_with_database(&pg, 2).await;
    let ask = || request("GET", "/api/v1/health", &[s.host()]);
    assert_eq!(exchange(s.addr, &ask()).await.status, 200);

    pg.stop();
    let started = std::time::Instant::now();
    let r = exchange(s.addr, &ask()).await;
    assert_eq!(r.status, 503, "{}", r.head);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(r.header("content-type"), Some("application/problem+json"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert!(r.body.contains("\"status\":503"), "{}", r.body);
    assert!(
        !r.body.contains("127.0.0.1"),
        "the body names the database: {}",
        r.body
    );
    s.finish().await;
}

#[tokio::test]
async fn health_is_503_within_its_timeout_for_a_database_that_never_was_there() {
    let s = Running::start().await;
    let started = std::time::Instant::now();
    let r = exchange(s.addr, &request("GET", "/api/v1/health", &[s.host()])).await;
    assert_eq!(r.status, 503, "{}", r.head);
    assert!(started.elapsed() < server::health::DATABASE_TIMEOUT + Duration::from_secs(2));
    s.finish().await;
}

#[tokio::test]
async fn another_host_is_421_also_on_an_unknown_path() {
    let s = Running::start().await;
    for target in ["/api/v1/health", "/no/such/path"] {
        let r = exchange(
            s.addr,
            &request("GET", target, &["Host: attacker.example".to_owned()]),
        )
        .await;
        assert_eq!(r.status, 421, "{target}");
        assert_eq!(r.header("content-type"), Some("application/problem+json"));
        assert!(!r.body.contains("attacker"), "the body repeats the host");
    }
    let r = exchange(
        s.addr,
        &request("GET", "/api/v1/health", &["Host: [::1]:80".to_owned()]),
    )
    .await;
    assert_eq!(r.status, 421);
    s.finish().await;
}

#[tokio::test]
async fn an_unknown_path_with_the_right_host_is_404() {
    let s = Running::start().await;
    let r = exchange(s.addr, &request("GET", "/no/such/path", &[s.host()])).await;
    assert_eq!(r.status, 404);
    s.finish().await;
}

#[tokio::test]
async fn the_alias_is_redirected_with_307_for_every_method() {
    let s = Running::start().await;
    let port = s.addr.port();
    for method in ["GET", "POST", "DELETE"] {
        let r = exchange(
            s.addr,
            &request(
                method,
                "/api/v1/jobs?a=1",
                &[format!("Host: localhost:{port}")],
            ),
        )
        .await;
        assert_eq!(r.status, 307, "{method}");
        assert_eq!(
            r.header("location"),
            Some(format!("http://127.0.0.1:{port}/api/v1/jobs?a=1").as_str())
        );
        assert_eq!(r.header("cache-control"), Some("no-store"));
    }
    let r = exchange(
        s.addr,
        &request(
            "GET",
            "//other.example/x",
            &[format!("Host: localhost:{port}")],
        ),
    )
    .await;
    assert_eq!(
        r.header("location"),
        Some(format!("http://127.0.0.1:{port}//other.example/x").as_str())
    );
    s.finish().await;
}

#[tokio::test]
async fn a_foreign_origin_is_403_and_the_pinned_origin_passes() {
    let pg = TestPostgres::start();
    let s = Running::start_with_database(&pg, 2).await;
    let port = s.addr.port();
    for origin in [
        "null".to_owned(),
        "http://evil.example".to_owned(),
        format!("http://localhost:{port}"),
    ] {
        let r = exchange(
            s.addr,
            &request(
                "GET",
                "/api/v1/health",
                &[s.host(), format!("Origin: {origin}")],
            ),
        )
        .await;
        assert_eq!(r.status, 403, "{origin}");
    }
    let r = exchange(
        s.addr,
        &request(
            "GET",
            "/api/v1/health",
            &[s.host(), format!("Origin: http://127.0.0.1:{port}")],
        ),
    )
    .await;
    assert_eq!(r.status, 200);
    // A WebSocket handshake from another site is refused before any upgrade.
    let r = exchange(
        s.addr,
        &request(
            "GET",
            "/ws/v1",
            &[
                s.host(),
                "Upgrade: websocket".to_owned(),
                "Connection: Upgrade".to_owned(),
                "Origin: http://evil.example".to_owned(),
            ],
        ),
    )
    .await;
    assert_eq!(r.status, 403);
    s.finish().await;
}

#[tokio::test]
async fn a_request_without_a_host_header_is_400() {
    let s = Running::start().await;
    let r = exchange(s.addr, &request("GET", "/api/v1/health", &[])).await;
    assert_eq!(r.status, 400);
    let twice = [s.host(), s.host()];
    let r = exchange(s.addr, &request("GET", "/api/v1/health", &twice)).await;
    assert_eq!(r.status, 400);
    s.finish().await;
}

#[tokio::test]
async fn an_absolute_form_target_is_400() {
    let s = Running::start().await;
    let target = format!("http://127.0.0.1:{}/api/v1/health", s.addr.port());
    let r = exchange(s.addr, &request("GET", &target, &[s.host()])).await;
    assert_eq!(r.status, 400, "{}", r.head);
    s.finish().await;
}

#[tokio::test]
async fn a_cleartext_http2_preface_is_refused_as_http1() {
    // Only HTTP/1 is served, also when another crate in the build enables HTTP/2 in hyper: a
    // client that speaks HTTP/2 from the first byte gets an HTTP/1.1 error status or a closed
    // connection, never a SETTINGS frame (which would start with a zero byte).
    let s = Running::start().await;
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut buf))
        .await
        .expect("the connection ends")
        .ok();
    let text = String::from_utf8_lossy(&buf);
    assert!(
        buf.is_empty() || text.starts_with("HTTP/1.1 4") || text.starts_with("HTTP/1.1 5"),
        "{buf:?}"
    );
    assert_ne!(buf.first(), Some(&0), "an HTTP/2 frame: {buf:?}");
    s.finish().await;
}

#[tokio::test]
async fn a_rejected_request_never_sees_the_routers_answer() {
    // The guard runs before routing: a wrong method on a known path is 405 with `Allow` for a
    // right Host, but a wrong Host gets 421 and nothing of the method router.
    let s = Running::start().await;
    let r = exchange(
        s.addr,
        &request(
            "POST",
            "/api/v1/health",
            &["Host: attacker.example".to_owned()],
        ),
    )
    .await;
    assert_eq!(r.status, 421);
    assert_eq!(r.header("allow"), None, "{}", r.head);
    let r = exchange(s.addr, &request("POST", "/api/v1/health", &[s.host()])).await;
    assert_eq!(r.status, 405);
    assert!(r.header("allow").is_some(), "{}", r.head);
    s.finish().await;
}

#[tokio::test]
async fn a_slow_request_head_is_dropped_after_the_header_read_timeout() {
    let s = Running::start_with_grace(30).await;
    let mut idle = TcpStream::connect(s.addr).await.unwrap();
    idle.write_all(b"GET /api/v1/health HTTP/1.1\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(20), idle.read_to_end(&mut buf))
        .await
        .expect("the server drops the connection")
        .ok();
    let waited = started.elapsed();
    assert!(
        waited >= server::HEADER_READ_TIMEOUT.mul_f32(0.5)
            && waited < server::HEADER_READ_TIMEOUT * 3,
        "{waited:?}"
    );
    s.finish().await;
}

#[tokio::test]
async fn a_request_in_flight_finishes_during_the_grace_period() {
    let pg = TestPostgres::start();
    let mut s = Running::start_with_database(&pg, 30).await;
    let mut partial = TcpStream::connect(s.addr).await.unwrap();
    partial
        .write_all(format!("GET /api/v1/health HTTP/1.1\r\n{}\r\n", s.host()).as_bytes())
        .await
        .unwrap();
    // The server accepts in order, so once a later connection has been answered, `partial` has
    // been accepted too and the stop cannot reset it from the backlog.
    let r = exchange(s.addr, &request("GET", "/api/v1/health", &[s.host()])).await;
    assert_eq!(r.status, 200);
    // The stop has begun once the listener refuses; the request head is still incomplete.
    s.begin_stop().await;
    partial
        .write_all(b"Connection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), partial.read_to_end(&mut buf))
        .await
        .expect("the answer arrives")
        .ok();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    s.joined().await;
}

#[tokio::test]
async fn a_request_that_never_completes_does_not_outlast_the_grace_period() {
    let s = Running::start().await;
    let mut idle = TcpStream::connect(s.addr).await.unwrap();
    idle.write_all(b"GET /api/v1/health HTTP/1.1\r\n")
        .await
        .unwrap();
    let started = std::time::Instant::now();
    s.finish().await;
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_second_stop_signal_ends_the_wait_at_once() {
    let mut s = Running::start_with_grace(30).await;
    let mut idle = TcpStream::connect(s.addr).await.unwrap();
    idle.write_all(b"GET /api/v1/health HTTP/1.1\r\n")
        .await
        .unwrap();
    s.begin_stop().await;
    let started = std::time::Instant::now();
    let _ = s.force.take().unwrap().send(());
    s.joined().await;
    assert!(
        started.elapsed() < server::HEADER_READ_TIMEOUT,
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn start_fails_when_the_port_is_taken() {
    let s = Running::start().await;
    let options = PgConnectOptions::new_without_pgpass()
        .host("127.0.0.1")
        .port(free_port())
        .ssl_mode(PgSslMode::Disable);
    let http = Http {
        port: s.addr.port(),
        shutdown_grace_secs: 2,
    };
    let pool = db::pool_lazy(options, Duration::from_secs(1));
    assert!(Server::bind(&http, pool, test_key()).await.is_err());
    s.finish().await;
}
