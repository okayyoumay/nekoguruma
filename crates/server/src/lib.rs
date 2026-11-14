//! The server (design 3.2, 12, 13): a library, with the binary `ngr-server` in `main.rs`
//! (`crates/server/docs/ngr-server.md`).
//!
//! This build serves the local deployment profile: it listens on 127.0.0.1 only, behind the
//! request guard of design 13.3 (ADR-456). It connects to PostgreSQL and applies the embedded
//! migrations before it binds, so a listening port means a migrated schema (ADR-458). It then
//! reads or creates the job instruction key, so a listening port also means a usable key
//! (ADR-462), and opens the object store (ADR-474), whose directory it locks for the process's
//! life. Local users sign in with a password and keep a cookie session (ADR-469).

pub mod config;
pub mod control;
pub mod db;
pub(crate) mod fs_guard;
pub mod health;
pub mod hub;
pub mod instruction_key;
pub mod job_queue;
pub mod local_guard;
pub mod local_user;
pub mod operator_session;
pub mod problem;
pub mod register;
pub mod registration;
pub(crate) mod revocation;
pub mod session;
pub mod storage;

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::{any, get, post};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use sqlx::PgPool;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tower::Layer;

pub use config::{Config, ConfigError, Profile};
pub use hub::{Delivery, Hub, Inbound, SendError};
pub use instruction_key::InstructionKey;
pub use local_user::Hasher;
pub use operator_session::OperatorSessions;
pub use session::Sessions;
pub use storage::{LocalStore, ObjectStore, StoreError};

/// How long a connection may take to deliver a complete request head. A client that opens a
/// connection and sends nothing, or a head one byte at a time, is dropped after this; it also
/// bounds what such a client can hold during a graceful stop (ADR-456).
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// State the routes and the guard share.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The port the listener is bound to; it decides the pinned origin.
    pub port: u16,
    /// The database pool.
    pub pool: PgPool,
    /// The key that signs job instructions (ADR-462).
    pub instruction_key: Arc<InstructionKey>,
    /// The session tokens the agent session route issues; the control channel route reads them.
    pub sessions: Sessions,
    /// The connections of the control channel.
    pub hub: Hub,
    /// The operators' login sessions (design 13.3, ADR-469); its `Debug` shows the count only.
    pub operator_sessions: OperatorSessions,
    /// The password hashing, one at a time (ADR-469).
    pub hasher: Hasher,
}

/// The server's routes behind the local profile's request guard (design 13, 13.3).
///
/// The guard wraps the whole route table as a service (axum's pattern for middleware that runs
/// before routing), so a request it rejects never reaches the router: no method-router answer
/// (405 with `Allow`) and no fallback is seen for it.
pub fn router(state: AppState) -> Router {
    let health = Router::new()
        .route("/api/v1/health", get(health::health))
        .with_state(state.pool.clone());
    // Login, logout and the whoami route; the session layer is the one every operator route
    // mounts with `route_layer`, so that an unknown path and a wrong method are answered first.
    let auth = operator_session::OperatorAuth::new(
        state.pool.clone(),
        state.port,
        state.operator_sessions,
        state.hasher,
    );
    // The layer is mounted on the method router (`MethodRouter::route_layer`), not on the
    // router: only the former leaves the 405 for a wrong method outside the layer.
    let signed_in = Router::new().route(
        operator_session::ME_PATH,
        get(operator_session::me).route_layer(middleware::from_fn_with_state(
            auth.clone(),
            operator_session::require_session,
        )),
    );
    let operator = Router::new()
        .route(operator_session::LOGIN_PATH, post(operator_session::login))
        .route(
            operator_session::LOGOUT_PATH,
            post(operator_session::logout),
        )
        .merge(signed_in)
        .with_state(auth);
    // The redemption route keeps its own limiter and log gates: one set per router.
    let register = Router::new()
        .route(register::PATH, post(register::register))
        .with_state(register::Registrar::new(
            state.pool.clone(),
            state.instruction_key,
        ));
    // The challenge store, the two limiters and the log gates of the proof exchange are per
    // router too; the token table is the state's, shared with the control channel route.
    let exchange = Router::new()
        .route(session::CHALLENGE_PATH, post(session::challenge))
        .route(session::SESSION_PATH, post(session::session))
        .with_state(session::Exchange::new(
            state.pool.clone(),
            state.port,
            state.sessions.clone(),
        ));
    // The control channel judges the token the exchange issues, and hands its sockets to the hub.
    // It takes every method, so that its 405 is a problem document like its other refusals.
    let control = Router::new()
        .route(control::PATH, any(control::upgrade))
        .with_state(control::Control::new(state.pool, state.sessions, state.hub));
    let routes = health
        .merge(operator)
        .merge(register)
        .merge(exchange)
        .merge(control)
        .fallback(|| async { problem::Problem::new(StatusCode::NOT_FOUND, "Not found") });
    // The standard profile supplies its own ingress layer here (design 13).
    let guarded = middleware::from_fn_with_state(state.port, local_guard::guard).layer(routes);
    Router::new().fallback_service(guarded)
}

/// Why `Server::start` failed.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Database(#[from] db::DbError),
    #[error(transparent)]
    InstructionKey(#[from] instruction_key::KeyError),
    #[error(transparent)]
    Storage(#[from] storage::StoreError),
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
    pool: PgPool,
    sessions: Sessions,
    hasher: Hasher,
    hub: Hub,
    grace: Duration,
    /// The object store `start` opened; `bind` leaves it empty (tests). Held so that its
    /// directory stays locked until `run` returns.
    object_store: Option<Arc<LocalStore>>,
    /// The delivery queue's settings; `None` leaves the queue off (`bind`, which tests use).
    job_queue: Option<job_queue::Settings>,
}

impl Server {
    /// Connects to the database (within the connect timeout), applies the migrations (within
    /// [`db::MIGRATION_TIMEOUT`]), makes the instruction key ready ([`InstructionKey::open`]),
    /// opens the object store ([`LocalStore::open`]), then binds `127.0.0.1:<port>`: nothing
    /// listens before the schema is current (ADR-458), before the key is usable (ADR-462) or
    /// before the store's directory is usable and locked (ADR-474). Any failure ends the start; a
    /// failed migration run is never retried on its pool (the advisory lock is per session and
    /// counts how often it was taken, so a retry could leave it held), and neither is the key or
    /// the store.
    pub async fn start(config: &Config) -> Result<Server, StartError> {
        let pool = db::connect(&config.database).await?;
        if let Err(error) = db::migrate(&pool).await {
            db::close(&pool).await;
            return Err(error.into());
        }
        let key = match InstructionKey::open(&config.instruction_key, &pool).await {
            Ok(key) => Arc::new(key),
            Err(error) => {
                db::close(&pool).await;
                return Err(error.into());
            }
        };
        let store = match LocalStore::open(&config.storage) {
            Ok(store) => Arc::new(store),
            Err(error) => {
                db::close(&pool).await;
                return Err(error.into());
            }
        };
        let mut server = Server::bind(&config.http, pool, key).await?;
        server.object_store = Some(store);
        server.job_queue = Some(job_queue::Settings::default());
        Ok(server)
    }

    /// Whether the delivery queue is on. Tests only.
    #[doc(hidden)]
    pub fn job_queue_enabled(&self) -> bool {
        self.job_queue.is_some()
    }

    /// The object store `start` opened (`None` on a server made by `bind`).
    pub fn object_store(&self) -> Option<Arc<dyn ObjectStore>> {
        self.object_store
            .as_ref()
            .map(|store| Arc::clone(store) as Arc<dyn ObjectStore>)
    }

    /// Binds `127.0.0.1:<port>` (the address is fixed in code) and derives the pinned origin from
    /// the listener's real port, so a configured port 0 works in tests. The pool is taken as it
    /// is, without migrating it. Tests only: a deployment starts with [`Server::start`].
    #[doc(hidden)]
    pub async fn bind(
        http: &config::Http,
        pool: PgPool,
        instruction_key: Arc<InstructionKey>,
    ) -> Result<Server, StartError> {
        let port = http.port;
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => listener,
            Err(source) => {
                db::close(&pool).await;
                return Err(StartError::Bind { port, source });
            }
        };
        let bound = listener
            .local_addr()
            .map_err(|source| StartError::Bind { port, source })?
            .port();
        let sessions = Sessions::new();
        let hub = Hub::new();
        let hasher = Hasher::new();
        Ok(Server {
            listener,
            router: router(AppState {
                port: bound,
                pool: pool.clone(),
                instruction_key,
                sessions: sessions.clone(),
                hub: hub.clone(),
                operator_sessions: OperatorSessions::new(),
                hasher: hasher.clone(),
            }),
            pool,
            sessions,
            hasher,
            hub,
            grace: Duration::from_secs(http.shutdown_grace_secs),
            object_store: None,
            job_queue: None,
        })
    }

    /// Turns the delivery queue on with `settings` (ADR-476); `start` does so with the defaults.
    /// The queue takes the hub's inbox when the server runs. Tests only.
    #[doc(hidden)]
    pub fn with_job_queue(mut self, settings: job_queue::Settings) -> Server {
        self.job_queue = Some(settings);
        self
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .expect("a bound listener has an address")
    }

    /// The hub of the control channel: the way to send a job command to a connected agent and to
    /// take what the agents send (ADR-468 item 12).
    pub fn hub(&self) -> Hub {
        self.hub.clone()
    }

    /// The session tokens this server issues, which the control channel route accepts.
    pub fn sessions(&self) -> Sessions {
        self.sessions.clone()
    }

    /// The server's password hasher. Tests only: holding its permit pins a login between its row
    /// read and its hash.
    #[doc(hidden)]
    pub fn hasher(&self) -> Hasher {
        self.hasher.clone()
    }

    /// The pinned origin this server answers on.
    pub fn pinned_origin(&self) -> String {
        local_guard::pinned_origin(self.local_addr().port())
    }

    /// Serves HTTP/1 until `shutdown` resolves; then stops accepting, closes every control
    /// channel connection with 1001, and waits for requests in flight and for those closes, at
    /// most the grace period or until `immediate` resolves (a second stop signal). What is left
    /// is aborted, the connections before the sockets, and the database pool is closed last.
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
            pool,
            sessions,
            hasher: _,
            hub,
            grace,
            object_store: _object_store,
            job_queue,
        } = self;
        // The revocation pass is one of the hub's tasks, so that the stop ends it before the pool
        // closes (ADR-468 item 11).
        hub.adopt(revocation::run(hub.clone(), sessions, hub.stopping(), {
            let pool = pool.clone();
            move |ids: Vec<String>| {
                let pool = pool.clone();
                async move { db::live_agents(&pool, &ids).await }
            }
        }));
        // The delivery queue is another of them, and the only consumer of the hub's inbox
        // (ADR-476).
        if let Some(settings) = job_queue {
            match hub.inbox() {
                Some(inbox) => hub.adopt(job_queue::run(
                    hub.clone(),
                    pool.clone(),
                    inbox,
                    hub.stopping(),
                    settings,
                )),
                None => tracing::warn!("the hub's inbox is taken; no job is delivered"),
            }
        }
        let service = TowerToHyperService::new(router);
        // hyper-util's graceful watch has no implementation for a connection that allows
        // upgrades, so each connection task watches this signal itself (ADR-468).
        let (stop, _) = watch::channel(false);
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
                        let connection = builder
                            .serve_connection(TokioIo::new(stream), service.clone())
                            .with_upgrades();
                        connections.spawn(serve_connection(connection, stop.subscribe()));
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
        // Requests in flight finish and idle connections close; every control channel session
        // is told to close with 1001. A connection that upgraded has left `connections`: its
        // socket is the hub's.
        stop.send_replace(true);
        let mut sockets = hub.begin_stop();
        tokio::select! {
            () = async {
                while connections.join_next().await.is_some() {}
                while sockets.join_next().await.is_some() {}
            } => {}
            () = tokio::time::sleep(grace) => {
                tracing::warn!(
                    grace_secs = grace.as_secs(),
                    "requests or control connections still open at the end of the grace \
                     period; stopping"
                );
            }
            () = immediate => {
                tracing::warn!("second stop signal; stopping at once");
            }
        }
        // What is still open at the end of the grace period (or of a second signal) is aborted
        // before the pool closes, so that none holds a pooled connection that `close` would wait
        // for. The connections come first: one that is about to upgrade then fails to, instead
        // of handing the hub a socket.
        connections.shutdown().await;
        sockets.shutdown().await;
        db::close(&pool).await;
        Ok(())
    }
}

/// A connection of the accept loop: HTTP/1 that may hand its socket over at an upgrade.
type Upgradeable = http1::UpgradeableConnection<TokioIo<TcpStream>, TowerToHyperService<Router>>;

/// Serves one connection until it ends, asking it to finish its request and close when `stop`
/// turns true.
async fn serve_connection(connection: Upgradeable, mut stop: watch::Receiver<bool>) {
    let mut connection = std::pin::pin!(connection);
    let mut stopped = false;
    let result = loop {
        tokio::select! {
            result = connection.as_mut() => break result,
            // `Err` means the sender is gone, which is a stop as well.
            _ = async { let _ = stop.wait_for(|stop| *stop).await; }, if !stopped => {
                stopped = true;
                connection.as_mut().graceful_shutdown();
            }
        }
    };
    if let Err(error) = result {
        tracing::debug!(%error, "connection ended with an error");
    }
}
