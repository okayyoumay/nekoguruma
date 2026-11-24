//! The server (design 3.2, 12, 13): a library, with the binary `ngr-server` in `main.rs`
//! (`crates/server/docs/ngr-server.md`).
//!
//! This build serves the local deployment profile: it listens on 127.0.0.1 only, behind the
//! request guard of design 13.3 (ADR-456). It connects to PostgreSQL and applies the embedded
//! migrations before it binds, so a listening port means a migrated schema (ADR-458). It then
//! reads or creates the job instruction key, so a listening port also means a usable key
//! (ADR-462), starts the bundled object server as its child and waits until it is ready
//! (ADR-491), and opens the object store, a bucket on that server (ADR-492), creating the bucket
//! if it is absent. Local users sign in with a password and keep a cookie session (ADR-469).

pub(crate) mod agent_caller;
pub(crate) mod agent_events;
pub(crate) mod answer_proof;
pub(crate) mod audit;
pub mod config;
pub mod control;
pub mod db;
pub mod documents;
pub mod downloads;
pub(crate) mod fs_guard;
pub mod health;
pub mod hub;
pub mod instruction_key;
pub mod job_expiry;
pub mod job_queue;
pub(crate) mod job_state;
pub mod jobs;
pub mod local_guard;
pub mod local_user;
pub mod object_server;
pub mod operator_session;
pub mod problem;
pub mod register;
pub mod registration;
pub(crate) mod revocation;
pub mod session;
pub mod storage;
pub mod uploads;

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{Method, StatusCode};
use axum::middleware;
use axum::routing::{MethodRouter, any, get, post, put};
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
pub use object_server::ObjectServer;
pub use operator_session::OperatorSessions;
pub use session::Sessions;
pub use storage::{LocalStore, MultipartStore, ObjectStore, S3Store, StoreError};

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
    /// The object store and part size of the data channel's routes (ADR-493); the routes are
    /// built from it, so it is set before the router is.
    pub uploads: uploads::Backend,
}

/// A route the server serves: the HTTP method and the path, as a client sends them. The control
/// channel's route takes every method (its refusals are problem documents, 405 included) and is
/// listed with `GET`, the method of the upgrade.
pub type ServedRoute = (Method, &'static str);

/// The routes as they are mounted, so that the list of what is served is the router's own and
/// cannot drift from it. `api/openapi.yaml` is checked against it (`tests/postgres/api_spec.rs`).
struct Mounts(Vec<ServedRoute>);

impl Mounts {
    /// Mounts `handler` at `path` and records the route. Every route of [`router`] goes through
    /// here; a test in `tests/postgres/api_spec.rs` refuses any other
    /// call of the router's own mounting method in this file.
    fn mount<S: Clone + Send + Sync + 'static>(
        &mut self,
        router: Router<S>,
        method: Method,
        path: &'static str,
        handler: MethodRouter<S>,
    ) -> Router<S> {
        self.0.push((method, path));
        router.route(path, handler)
    }
}

/// The server's routes behind the local profile's request guard (design 13, 13.3).
///
/// The guard wraps the whole route table as a service (axum's pattern for middleware that runs
/// before routing), so a request it rejects never reaches the router: no method-router answer
/// (405 with `Allow`) and no fallback is seen for it.
pub fn router(state: AppState) -> Router {
    router_with_routes(state).0
}

/// [`router`] and the list of the routes it serves, in the order they are mounted. The guard is
/// the same; tests use the list to check what is served against `api/openapi.yaml`.
#[doc(hidden)]
pub fn router_with_routes(state: AppState) -> (Router, Vec<ServedRoute>) {
    let (routes, served) = routes_unguarded(state.clone());
    // The standard profile supplies its own ingress layer here (design 13).
    let guarded = middleware::from_fn_with_state(state.port, local_guard::guard).layer(routes);
    (Router::new().fallback_service(guarded), served)
}

/// The route table WITHOUT the request guard, with the list of its routes. Tests only: it must
/// never be served, since the guard's Host and Origin rules (design 13.3) are what keep other
/// local users and web pages from reaching these routes.
#[doc(hidden)]
pub fn routes_unguarded(state: AppState) -> (Router, Vec<ServedRoute>) {
    let mut mounts = Mounts(Vec::new());
    // The answer proof's key, audience and log gate are built once and shared by the routes that
    // prove (ADR-504).
    let prover = Arc::new(answer_proof::Prover::new(
        Arc::clone(&state.instruction_key),
        state.port,
    ));
    let health = mounts
        .mount(
            Router::new(),
            Method::GET,
            "/api/v1/health",
            get(health::health),
        )
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
    let signed_in = mounts.mount(
        Router::new(),
        Method::GET,
        operator_session::ME_PATH,
        get(operator_session::me).route_layer(middleware::from_fn_with_state(
            auth.clone(),
            operator_session::require_session,
        )),
    );
    // The job route takes the same layer and has its own state.
    let jobs = mounts
        .mount(
            Router::new(),
            Method::POST,
            jobs::PATH,
            post(jobs::create).route_layer(middleware::from_fn_with_state(
                auth.clone(),
                operator_session::require_session,
            )),
        )
        .with_state(jobs::Jobs::new(
            state.pool.clone(),
            Arc::clone(&state.instruction_key),
        ));
    // The document store takes the operator session's layer and the object store the data
    // channel's routes use; its state holds its own slots and log gates (ADR-500).
    let documents = mounts
        .mount(
            Router::new(),
            Method::PUT,
            documents::PATH,
            put(documents::store).route_layer(middleware::from_fn_with_state(
                auth.clone(),
                operator_session::require_session,
            )),
        )
        .with_state(documents::Documents::new(
            state.pool.clone(),
            &state.uploads,
        ));
    let operator = mounts.mount(
        Router::new(),
        Method::POST,
        operator_session::LOGIN_PATH,
        post(operator_session::login),
    );
    let operator = mounts
        .mount(
            operator,
            Method::POST,
            operator_session::LOGOUT_PATH,
            post(operator_session::logout),
        )
        .merge(signed_in)
        .with_state(auth);
    // The data channel's routes judge the session token and read the agent's row on every
    // request (ADR-493 item 2).
    let uploads = mounts.mount(
        Router::new(),
        Method::POST,
        uploads::BEGIN_PATH,
        post(uploads::begin),
    );
    let uploads = mounts.mount(
        uploads,
        Method::POST,
        uploads::PART_PATH,
        post(uploads::part_url),
    );
    let uploads = mounts.mount(
        uploads,
        Method::GET,
        uploads::STATUS_PATH,
        get(uploads::status),
    );
    let uploads = mounts.mount(
        uploads,
        Method::POST,
        uploads::COMPLETE_PATH,
        post(uploads::complete),
    );
    // The download URL route reads the same rows and signs with the same store, so it takes the
    // upload routes' state.
    let uploads = mounts
        .mount(uploads, Method::POST, downloads::PATH, post(downloads::url))
        .with_state(uploads::Uploads::new(
            state.pool.clone(),
            state.sessions.clone(),
            state.uploads,
            state.hub.clone(),
            Arc::clone(&prover),
        ));
    // The redemption route keeps its own limiter and log gates: one set per router.
    let register = mounts
        .mount(
            Router::new(),
            Method::POST,
            register::PATH,
            post(register::register),
        )
        .with_state(register::Registrar::new(
            state.pool.clone(),
            state.instruction_key,
        ));
    // The challenge store, the two limiters and the log gates of the proof exchange are per
    // router too; the token table is the state's, shared with the control channel route.
    let exchange = mounts.mount(
        Router::new(),
        Method::POST,
        session::CHALLENGE_PATH,
        post(session::challenge),
    );
    let exchange = mounts
        .mount(
            exchange,
            Method::POST,
            session::SESSION_PATH,
            post(session::session),
        )
        .with_state(session::Exchange::new(
            state.pool.clone(),
            Arc::clone(&prover),
            state.sessions.clone(),
        ));
    // The control channel judges the token the exchange issues, and hands its sockets to the hub.
    // It takes every method, so that its 405 is a problem document like its other refusals.
    let control = mounts
        .mount(
            Router::new(),
            Method::GET,
            control::PATH,
            any(control::upgrade),
        )
        .with_state(control::Control::new(
            state.pool,
            state.sessions,
            state.hub,
            prover,
        ));
    let routes = health
        .merge(operator)
        .merge(jobs)
        .merge(documents)
        .merge(uploads)
        .merge(register)
        .merge(exchange)
        .merge(control)
        .fallback(|| async { problem::Problem::new(StatusCode::NOT_FOUND, "Not found") });
    (routes, mounts.0)
}

/// The server's listener: bound to `127.0.0.1` itself, never the wildcard address, with no
/// address-sharing option set by this code (no `SO_REUSEPORT`, and `SO_REUSEADDR` only where the
/// runtime's own bind sets it, which on Unix does not let a second socket bind a listening
/// address). The answer proof rests on no other process being able to receive connections to this
/// address while the server listens (ADR-504 item 5); the probe
/// `the_listener_shares_its_address_with_nobody` checks it.
async fn loopback_listener(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await
}

/// Why `Server::start` failed.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Database(#[from] db::DbError),
    #[error(transparent)]
    InstructionKey(#[from] instruction_key::KeyError),
    #[error(transparent)]
    ObjectServer(#[from] object_server::ObjectServerError),
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
    /// The object server ended while the server ran and no stop signal had been received. The
    /// server stopped as for a signal first; the service manager restarts it (ADR-491).
    #[error("the object server ended while the server ran: {0}")]
    ObjectServerExited(String),
}

/// A bound server that is not serving yet.
pub struct Server {
    listener: TcpListener,
    router: Router,
    pool: PgPool,
    sessions: Sessions,
    operator_sessions: OperatorSessions,
    hasher: Hasher,
    hub: Hub,
    grace: Duration,
    /// The object store `start` opened on the object server; `bind` leaves it empty (tests), and
    /// so does a `Config` with no `[object_server]`.
    object_store: Option<Arc<S3Store>>,
    /// The object server `start` started (ADR-491); `bind` leaves it empty (tests). `run` ends
    /// it after the connections and before the pool.
    object_server: Option<ObjectServer>,
    /// The delivery queue's settings; `None` leaves the queue off (`bind`, which tests use).
    job_queue: Option<job_queue::Settings>,
    /// The job expiry task's settings; `None` leaves it off (`bind`, which tests use).
    job_expiry: Option<job_expiry::Settings>,
    /// The data channel's backend, which the router was built from: the completion pass takes its
    /// store, period and registry.
    uploads: uploads::Backend,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").finish_non_exhaustive()
    }
}

impl Server {
    /// Connects to the database (within the connect timeout), applies the migrations (within
    /// [`db::MIGRATION_TIMEOUT`]), makes the instruction key ready ([`InstructionKey::open`]),
    /// starts the object server when `[object_server]` is configured ([`ObjectServer::start`],
    /// ADR-491), opens the object store on it ([`S3Store::open`], ADR-492: the bucket is checked
    /// and created if absent, and the wait ends if the child ends), then binds
    /// `127.0.0.1:<port>`: nothing listens before the schema is current (ADR-458), before the
    /// key is usable (ADR-462), before the object server answers its readiness probe (ADR-491) or
    /// before the bucket is ready (ADR-492). Any failure ends the start, with the pool closed and
    /// the object server stopped; a failed migration run is never retried on its pool (the
    /// advisory lock is per session and counts how often it was taken, so a retry could leave it
    /// held), and neither is the key, the object server or the store.
    pub async fn start(config: &Config) -> Result<Server, StartError> {
        Server::start_tuned(
            config,
            object_server::Tuning::default(),
            storage::S3Tuning::default(),
        )
        .await
    }

    /// [`Server::start`] with the object server's and the object store's bounds set. Tests only.
    #[doc(hidden)]
    pub async fn start_tuned(
        config: &Config,
        tuning: object_server::Tuning,
        store_tuning: storage::S3Tuning,
    ) -> Result<Server, StartError> {
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
        let (object_server, store) = match &config.object_server {
            Some(object) => {
                let mut object_server =
                    match ObjectServer::start(object, config.http.port, &tuning).await {
                        Ok(object_server) => object_server,
                        Err(error) => {
                            db::close(&pool).await;
                            return Err(error.into());
                        }
                    };
                // The store opens while the child lives: a child that ends meanwhile ends the
                // start at once instead of after the store's retries.
                let open = S3Store::open(
                    object_server.port(),
                    object_server.credentials(),
                    object_server.certificate_pin(),
                    &object.bucket,
                    store_tuning,
                );
                let opened = tokio::select! {
                    biased;
                    ended = object_server.exited() => Err(StartError::ObjectServer(
                        object_server::ObjectServerError::ExitedBeforeReady(ended),
                    )),
                    opened = open => opened.map_err(StartError::from),
                };
                match opened {
                    Ok(store) => (Some(object_server), Some(Arc::new(store))),
                    Err(error) => {
                        object_server.stop(std::future::pending()).await;
                        db::close(&pool).await;
                        return Err(error);
                    }
                }
            }
            None => (None, None),
        };
        let backend = uploads::Backend::new(
            store
                .as_ref()
                .map(|store| Arc::clone(store) as Arc<dyn MultipartStore>),
        );
        let mut server = match Server::bind_with_store(&config.http, pool, key, backend).await {
            Ok(server) => server,
            Err(error) => {
                // `bind` closed the pool.
                stop_object_server(object_server).await;
                return Err(error);
            }
        };
        server.object_server = object_server;
        server.object_store = store;
        server.job_queue = Some(job_queue::Settings::default());
        server.job_expiry = Some(job_expiry::Settings::default());
        Ok(server)
    }

    /// Whether the delivery queue is on. Tests only.
    #[doc(hidden)]
    pub fn job_queue_enabled(&self) -> bool {
        self.job_queue.is_some()
    }

    /// Whether the job expiry task is on (ADR-487). Tests only.
    #[doc(hidden)]
    pub fn job_expiry_enabled(&self) -> bool {
        self.job_expiry.is_some()
    }

    /// The object server `start` started (`None` on a server made by `bind`, and when no
    /// `[object_server]` is configured). Tests only.
    #[doc(hidden)]
    pub fn object_server(&self) -> Option<&ObjectServer> {
        self.object_server.as_ref()
    }

    /// The object store `start` opened (`None` on a server made by `bind`, and when no
    /// `[object_server]` is configured).
    pub fn object_store(&self) -> Option<Arc<dyn MultipartStore>> {
        self.object_store
            .as_ref()
            .map(|store| Arc::clone(store) as Arc<dyn MultipartStore>)
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
        Server::bind_with_store(http, pool, instruction_key, uploads::Backend::default()).await
    }

    /// [`Server::bind`] with the object store and part size the data channel's routes use. They
    /// are part of the router's state, so they are given here, before the router is built; a store
    /// set afterwards would not reach the routes. Tests only: `start` calls it with the store it
    /// opened.
    #[doc(hidden)]
    pub async fn bind_with_store(
        http: &config::Http,
        pool: PgPool,
        instruction_key: Arc<InstructionKey>,
        uploads: uploads::Backend,
    ) -> Result<Server, StartError> {
        let port = http.port;
        let listener = match loopback_listener(port).await {
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
        let operator_sessions = OperatorSessions::new();
        Ok(Server {
            listener,
            router: router(AppState {
                port: bound,
                pool: pool.clone(),
                instruction_key,
                sessions: sessions.clone(),
                hub: hub.clone(),
                operator_sessions: operator_sessions.clone(),
                hasher: hasher.clone(),
                uploads: uploads.clone(),
            }),
            pool,
            sessions,
            operator_sessions,
            hasher,
            hub,
            grace: Duration::from_secs(http.shutdown_grace_secs),
            object_store: None,
            object_server: None,
            job_queue: None,
            job_expiry: None,
            uploads,
        })
    }

    /// Turns the delivery queue on with `settings` (ADR-476); `start` does so with the defaults.
    /// The queue takes the hub's inbox when the server runs. Tests only.
    #[doc(hidden)]
    pub fn with_job_queue(mut self, settings: job_queue::Settings) -> Server {
        self.job_queue = Some(settings);
        self
    }

    /// Turns the job expiry task on with `settings` (ADR-487); `start` does so with the defaults.
    /// Tests only.
    #[doc(hidden)]
    pub fn with_job_expiry(mut self, settings: job_expiry::Settings) -> Server {
        self.job_expiry = Some(settings);
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

    /// The operators' login sessions. Tests only: a test that needs a session for an operator the
    /// login route does not serve (login looks in the local tenant only) makes it in this table.
    #[doc(hidden)]
    pub fn operator_sessions(&self) -> OperatorSessions {
        self.operator_sessions.clone()
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
            operator_sessions: _,
            hasher: _,
            hub,
            grace,
            object_store: _object_store,
            mut object_server,
            job_queue,
            job_expiry,
            uploads,
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
        // The completion pass is another: it finishes the uploads that no request repeats
        // (ADR-493 item 29), through the registry the routes use, and has nothing to do without
        // a store.
        if uploads.shared_store().is_some() {
            hub.adopt(uploads::run_completion_pass(
                pool.clone(),
                hub.clone(),
                uploads,
            ));
        }
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
        // The job expiry task is a third (ADR-487): it too ends before the pool closes.
        if let Some(settings) = job_expiry {
            hub.adopt(job_expiry::run(pool.clone(), hub.stopping(), settings));
        }
        let service = TowerToHyperService::new(router);
        // hyper-util's graceful watch has no implementation for a connection that allows
        // upgrades, so each connection task watches this signal itself (ADR-468).
        let (stop, _) = watch::channel(false);
        // Dropping the set aborts the connections that are still open.
        let mut connections: JoinSet<()> = JoinSet::new();
        let mut shutdown = std::pin::pin!(shutdown);
        let mut immediate = std::pin::pin!(immediate);
        let mut second_signal = false;
        // How the object server ended before a stop signal, if it did.
        let mut object_ended: Option<String> = None;
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown => break,
                ended = async {
                    match object_server.as_mut() {
                        Some(object) => object.exited().await,
                        None => std::future::pending().await,
                    }
                } => {
                    tracing::error!(%ended, "the object server ended; stopping");
                    object_ended = Some(ended);
                    break;
                },
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
            () = &mut immediate => {
                tracing::warn!("second stop signal; stopping at once");
                second_signal = true;
            }
        }
        // What is still open at the end of the grace period (or of a second signal) is aborted
        // before the pool closes, so that none holds a pooled connection that `close` would wait
        // for. The connections come first: one that is about to upgrade then fails to, instead
        // of handing the hub a socket.
        connections.shutdown().await;
        sockets.shutdown().await;
        // The object server goes after what could still use it and before the pool closes.
        if let Some(object) = object_server.take() {
            if second_signal {
                object.stop(std::future::ready(())).await;
            } else {
                object.stop(immediate.as_mut()).await;
            }
        }
        db::close(&pool).await;
        match object_ended {
            Some(ended) => Err(RunError::ObjectServerExited(ended)),
            None => Ok(()),
        }
    }
}

/// Stops the object server of a start that failed after it started it.
async fn stop_object_server(object: Option<ObjectServer>) {
    if let Some(object) = object {
        object.stop(std::future::pending()).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpSocket;

    /// ADR-504 item 5: while the server's listener is up, no other socket can take its address.
    /// A second bind of `127.0.0.1:<port>` with address reuse (and on Unix with port reuse)
    /// fails; a wildcard bind fails on Linux, and where one succeeds every connection to
    /// `127.0.0.1` still reaches the server's listener.
    #[tokio::test]
    async fn the_listener_shares_its_address_with_nobody() {
        let listener = loopback_listener(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(addr.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));

        // Another bind of the same address, with address reuse.
        let second = TcpSocket::new_v4().unwrap();
        second.set_reuseaddr(true).unwrap();
        assert!(second.bind(addr).is_err(), "a reuse bind took the address");
        #[cfg(unix)]
        {
            let second = TcpSocket::new_v4().unwrap();
            second.set_reuseport(true).unwrap();
            assert!(second.bind(addr).is_err(), "a port reuse bind took it");
        }

        // A wildcard socket on the same port. Linux refuses the bind while the loopback listener
        // is up, with or without address reuse. Elsewhere (Windows) it may bind, and then every
        // connection to `127.0.0.1` must still reach the server's listener.
        for reuse in [false, true] {
            let wildcard = TcpSocket::new_v4().unwrap();
            wildcard.set_reuseaddr(reuse).unwrap();
            let bound = wildcard.bind((Ipv4Addr::UNSPECIFIED, addr.port()).into());
            if cfg!(target_os = "linux") {
                assert!(
                    bound.is_err(),
                    "a wildcard bind took the port (reuse {reuse})"
                );
                eprintln!("wildcard bind refused (reuse {reuse}): the Linux variant");
                continue;
            }
            if bound.is_err() {
                eprintln!("wildcard bind refused (reuse {reuse}): nothing to connect to");
                continue;
            }
            eprintln!("wildcard bind taken (reuse {reuse}): checking the connections");
            let wildcard = wildcard.listen(16).unwrap();
            let mut clients = Vec::new();
            for _ in 0..16 {
                clients.push(TcpStream::connect(addr).await.unwrap());
            }
            for i in 0..16 {
                let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap_or_else(|_| {
                        panic!("connection {i} did not reach the server (reuse {reuse})")
                    });
                accepted.unwrap();
            }
            drop(wildcard);
        }
    }
}
