//! The server (design 3.2, 12, 13): a library, with the binary `ngr-server` in `main.rs`
//! (`crates/server/docs/ngr-server.md`).
//!
//! This build serves the local deployment profile: it listens on 127.0.0.1 only, behind the
//! request guard of design 13.3 (ADR-456).

pub mod config;
pub mod health;
pub mod local_guard;
pub mod problem;

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::get;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tower::Layer;

pub use config::{Config, ConfigError, Profile};

/// How long a connection may take to deliver a complete request head. A client that opens a
/// connection and sends nothing, or a head one byte at a time, is dropped after this; it also
/// bounds what such a client can hold during a graceful stop (ADR-456).
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// State the routes and the guard share.
#[derive(Debug, Clone, Copy)]
pub struct AppState {
    /// The port the listener is bound to; it decides the pinned origin.
    pub port: u16,
}

/// The server's routes behind the local profile's request guard (design 13, 13.3).
///
/// The guard wraps the whole route table as a service (axum's pattern for middleware that runs
/// before routing), so a request it rejects never reaches the router: no method-router answer
/// (405 with `Allow`) and no fallback is seen for it.
pub fn router(state: AppState) -> Router {
    let routes = Router::new()
        .route("/api/v1/health", get(health::health))
        .fallback(|| async { problem::Problem::new(StatusCode::NOT_FOUND, "Not found") });
    // The standard profile supplies its own ingress layer here (design 13).
    let guarded = middleware::from_fn_with_state(state.port, local_guard::guard).layer(routes);
    Router::new().fallback_service(guarded)
}

/// Why `Server::start` failed.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("cannot listen on 127.0.0.1:{port}: {source}")]
    Bind { port: u16, source: std::io::Error },
}

/// Why `Server::run` failed.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("the listener failed: {0}")]
    Serve(std::io::Error),
}

/// A bound server that is not serving yet.
pub struct Server {
    listener: TcpListener,
    router: Router,
    grace: Duration,
}

impl Server {
    /// Binds `127.0.0.1:<port>` (the address is fixed in code) and derives the pinned origin from
    /// the listener's real port, so a configured port 0 works in tests.
    pub async fn start(config: &Config) -> Result<Server, StartError> {
        let port = config.http.port;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|source| StartError::Bind { port, source })?;
        let bound = listener
            .local_addr()
            .map_err(|source| StartError::Bind { port, source })?
            .port();
        Ok(Server {
            listener,
            router: router(AppState { port: bound }),
            grace: Duration::from_secs(config.http.shutdown_grace_secs),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .expect("a bound listener has an address")
    }

    /// The pinned origin this server answers on.
    pub fn pinned_origin(&self) -> String {
        local_guard::pinned_origin(self.local_addr().port())
    }

    /// Serves HTTP/1 until `shutdown` resolves; then stops accepting and waits for requests in
    /// flight, at most the grace period or until `immediate` resolves (a second stop signal),
    /// and drops what is left.
    ///
    /// Only HTTP/1 is served, whatever features other crates in the build enable: the
    /// connections are driven by hyper's HTTP/1 builder, not by `axum::serve`.
    pub async fn run(
        self,
        shutdown: impl Future<Output = ()> + Send,
        immediate: impl Future<Output = ()> + Send,
    ) -> Result<(), RunError> {
        let Server {
            listener,
            router,
            grace,
        } = self;
        let service = TowerToHyperService::new(router);
        let graceful = GracefulShutdown::new();
        // Dropping the set aborts the connections that are still open.
        let mut connections: JoinSet<()> = JoinSet::new();
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let mut builder = http1::Builder::new();
                        builder
                            .timer(TokioTimer::new())
                            .header_read_timeout(HEADER_READ_TIMEOUT);
                        // The WebSocket route will need `with_upgrades()` and its own handling
                        // of a graceful stop.
                        let connection =
                            builder.serve_connection(TokioIo::new(stream), service.clone());
                        let connection = graceful.watch(connection);
                        connections.spawn(async move {
                            if let Err(error) = connection.await {
                                tracing::debug!(%error, "connection ended with an error");
                            }
                        });
                    }
                    Err(error) => {
                        // Typically descriptor exhaustion; back off instead of spinning.
                        tracing::warn!(%error, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        drop(listener);
        tokio::select! {
            () = graceful.shutdown() => {}
            () = tokio::time::sleep(grace) => {
                tracing::warn!(
                    grace_secs = grace.as_secs(),
                    "requests still in flight at the end of the grace period; stopping"
                );
            }
            () = immediate => {
                tracing::warn!("second stop signal; stopping at once");
            }
        }
        Ok(())
    }
}
