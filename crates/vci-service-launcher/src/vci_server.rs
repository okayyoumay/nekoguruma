use std::{
    future::Future,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
};

use tracing::{info, warn};

use tokio_stream::StreamExt;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::codegen::http;
use tonic::transport::Server;
use vci_service_interface::vci_service_server::VciServiceServer;

use crate::{
    BoxError,
    jsonrpc::{dispatch::JsonRpcHandler, server::JsonRpcContext},
};

pub type VciServerHandle = tokio::task::JoinHandle<Result<(), tonic::transport::Error>>;

/// Holds the per-instance bearer-token signing key pushed by the manager's
/// `set_auth_key` JSON-RPC call (see ADR-221). `Default` starts with no key
/// installed, so the listener fails closed (`UNAUTHENTICATED`) until the
/// manager provisions one -- the listener never serves an unauthenticated
/// RPC once this ships.
#[derive(Clone)]
pub struct AuthKey(std::sync::Arc<tokio::sync::RwLock<Option<[u8; 32]>>>);

impl Default for AuthKey {
    fn default() -> Self {
        Self(std::sync::Arc::new(tokio::sync::RwLock::new(None)))
    }
}

impl AuthKey {
    /// Installs (replacing any previous) the signing key. Re-sending a key
    /// invalidates every token minted against the old one immediately.
    pub async fn set(&self, key: [u8; 32]) {
        *self.0.write().await = Some(key);
    }

    /// Tonic interceptor entry point: verifies the bearer token on `req`'s
    /// `authorization` metadata against the currently installed key.
    ///
    /// This can't be `async` (tonic's `Interceptor` trait is sync), and this
    /// runs inline in the async request path, so it uses `try_read` rather
    /// than blocking the executor: a `set()` call in progress (rare -- once
    /// at startup, occasionally on key rotation) makes this treat the
    /// request the same as "no key installed" (fail closed) rather than
    /// blocking on `blocking_read`. Losing one racing request to a
    /// transient fail-closed response is an acceptable, safe failure mode.
    pub fn verify_request(
        &self,
        req: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        let unauthenticated = || tonic::Status::unauthenticated("authentication required");

        let key = match self.0.try_read() {
            Ok(guard) => *guard,
            Err(_) => None,
        };
        let key = key.ok_or_else(unauthenticated)?;

        // HTTP auth-scheme names are case-insensitive (RFC 7235 SS2.1) --
        // `bearer <token>`/`BEARER <token>` are just as legal as
        // `Bearer <token>`, so the scheme is matched with ASCII case
        // folding rather than requiring the exact `"Bearer "` spelling.
        // The credentials grammar (RFC 7235 SS2.1: `1*SP`) also permits more
        // than one space between the scheme and the token -- trim any
        // remaining leading spaces off the token rather than assuming
        // exactly one separator space.
        let token = req
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .and_then(|(scheme, rest)| {
                scheme
                    .eq_ignore_ascii_case("bearer")
                    .then(|| rest.trim_start_matches(' '))
            })
            .ok_or_else(unauthenticated)?;

        // Every verification failure reason maps to the same generic
        // message -- do not leak which specific check failed.
        crate::token::verify(&key, token, std::time::SystemTime::now())
            .map_err(|_| unauthenticated())?;

        Ok(req)
    }
}

pub struct VciServerContext {
    handle: VciServerHandle,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Loopback addresses the gRPC server is listening on: IPv4 and IPv6 on
    /// the same port when the host supports both families, or a single entry
    /// on an IPv4-only / IPv6-only host (ADR-052). Never empty.
    endpoints: Vec<SocketAddr>,
    auth_key: AuthKey,
}

impl JsonRpcContext for VciServerContext {
    async fn shutdown(self) {
        info!("shutting down gRPC server");
        let _ = self.shutdown_tx.send(true);
        let _ = self.handle.await;
    }
}

impl JsonRpcHandler for VciServerContext {
    async fn get_status(&self, _params: Option<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "endpoints": self.endpoints,
        })
    }

    async fn set_auth_key(&self, key: [u8; 32]) {
        self.auth_key.set(key).await;
    }
}

/// Identity of a running service instance, used to look up configuration.
pub struct ServiceIdentity {
    /// API family name: `"iso22900"` or `"j2534-0404"`.
    pub api_name: &'static str,
    /// The library (device) name passed on the command line.
    pub library_name: String,
    /// Process architecture string for config lookup.
    /// `Some("x86_x64")` / `Some("x86")` on Windows; `None` elsewhere.
    pub arch: Option<&'static str>,
}

/// Returns the current process architecture key used in config lookups.
/// Only meaningful on Windows; returns `None` on other platforms.
#[cfg(all(windows, target_arch = "x86_64"))]
pub fn current_arch() -> Option<&'static str> {
    Some("x86_x64")
}

#[cfg(all(windows, target_arch = "x86"))]
pub fn current_arch() -> Option<&'static str> {
    Some("x86")
}

#[cfg(not(windows))]
pub fn current_arch() -> Option<&'static str> {
    None
}

pub trait VciServer:
    vci_service_interface::vci_service_server::VciService + Sized + Send + Sync + 'static
{
    type StartupConfig: Send + 'static;
    fn get_startup_config(
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self::StartupConfig, BoxError>;
    /// Returns the identity of this service instance for configuration lookup.
    fn service_identity(config: &Self::StartupConfig) -> ServiceIdentity;
    fn new(
        args: Self::StartupConfig,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> impl Future<Output = Result<Self, BoxError>> + Send + 'static;
    fn get_requested_port(&self) -> Option<u16>;
}

/// `true` when a bind error means the address family (or its loopback
/// address) simply does not exist on this host — an IPv4-only or IPv6-only
/// system — as opposed to a real error like the port already being in use.
fn is_family_unavailable(err: &std::io::Error) -> bool {
    if matches!(
        err.kind(),
        std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
    ) {
        return true;
    }
    // EAFNOSUPPORT is not covered by a stable `ErrorKind` on all platforms.
    #[cfg(unix)]
    const AF_NOT_SUPPORTED: i32 = 97; // EAFNOSUPPORT
    #[cfg(windows)]
    const AF_NOT_SUPPORTED: i32 = 10047; // WSAEAFNOSUPPORT
    err.raw_os_error() == Some(AF_NOT_SUPPORTED)
}

/// Binds the gRPC loopback listeners: `127.0.0.1` and `[::1]` on the same
/// port when the host supports both address families, or whichever single
/// family exists on an IPv4-only / IPv6-only host (ADR-052). A missing
/// family is skipped with a warning; any other bind error (e.g. the
/// requested port being in use) still fails startup, and so does a host
/// with no usable loopback family at all.
async fn bind_loopback_listeners(
    port_to_use: u16,
) -> Result<Vec<tokio::net::TcpListener>, BoxError> {
    let mut listeners = Vec::new();

    let v6_port;
    let mut v4_family_err = None;
    match tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port_to_use))).await
    {
        Ok(listener) => {
            v6_port = listener.local_addr()?.port();
            listeners.push(listener);
        }
        Err(err) if is_family_unavailable(&err) => {
            warn!(%err, "IPv4 loopback unavailable; not listening on 127.0.0.1");
            v6_port = port_to_use;
            v4_family_err = Some(err);
        }
        Err(err) => return Err(err.into()),
    }

    match tokio::net::TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, v6_port))).await {
        Ok(listener) => listeners.push(listener),
        Err(err) if is_family_unavailable(&err) => {
            if let Some(v4_err) = v4_family_err {
                return Err(format!(
                    "no usable loopback address family (IPv4: {v4_err}; IPv6: {err})"
                )
                .into());
            }
            warn!(%err, "IPv6 loopback unavailable; not listening on [::1]");
        }
        Err(err) => return Err(err.into()),
    }

    Ok(listeners)
}

/// Create and start a gRPC server from an already-parsed startup config.
pub(crate) async fn spawn_vci_context<S>(
    startup_config: S::StartupConfig,
) -> Result<VciServerContext, BoxError>
where
    S: VciServer,
{
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let service = S::new(startup_config, shutdown_rx.clone()).await?;
    let port_to_use = service.get_requested_port().unwrap_or(0);

    let listeners = bind_loopback_listeners(port_to_use).await?;
    let endpoints = listeners
        .iter()
        .map(|listener| listener.local_addr())
        .collect::<Result<Vec<_>, _>>()?;

    info!(?endpoints, "gRPC server listening");

    let auth_key = AuthKey::default();
    let interceptor_key = auth_key.clone();

    let handle = tokio::spawn(async move {
        let mut streams = listeners.into_iter().map(TcpListenerStream::new);
        let incoming: std::pin::Pin<
            Box<dyn tokio_stream::Stream<Item = std::io::Result<tokio::net::TcpStream>> + Send>,
        > = match (streams.next(), streams.next()) {
            (Some(first), Some(second)) => Box::pin(first.merge(second)),
            (Some(first), None) => Box::pin(first),
            _ => unreachable!("bind_loopback_listeners returns at least one listener"),
        };
        let shutdown = async move {
            let _ = shutdown_rx.clone().changed().await;
        };

        let cors_layer = tower_http::cors::CorsLayer::new()
            .allow_origin(tower_http::cors::Any)
            .allow_methods([http::Method::POST, http::Method::OPTIONS])
            .allow_headers([
                http::header::AUTHORIZATION,
                http::header::CONTENT_TYPE,
                http::HeaderName::from_static("x-grpc-web"),
                http::HeaderName::from_static("x-user-agent"),
                http::HeaderName::from_static("grpc-timeout"),
            ])
            .expose_headers([
                http::HeaderName::from_static("grpc-status"),
                http::HeaderName::from_static("grpc-message"),
                http::HeaderName::from_static("grpc-status-details-bin"),
            ]);

        // Debug-only escape hatch for direct dev runs of a service binary
        // without a parent process,
        // mirroring ADR-073's existing debug-only environment-variable
        // pattern. Compiled out of release
        // builds entirely, so it can never ship in a production binary.
        #[cfg(debug_assertions)]
        let insecure_no_auth = std::env::var_os("VCI_SERVICE_INSECURE_NO_AUTH").is_some();
        #[cfg(not(debug_assertions))]
        let insecure_no_auth = false;

        let result = if insecure_no_auth {
            warn!(
                "VCI_SERVICE_INSECURE_NO_AUTH set: gRPC listener auth interceptor disabled (debug builds only)"
            );
            Server::builder()
                .accept_http1(true)
                .layer(cors_layer)
                .layer(tonic_web::GrpcWebLayer::new())
                .add_service(VciServiceServer::new(service))
                .serve_with_incoming_shutdown(incoming, shutdown)
                .await
        } else {
            Server::builder()
                .accept_http1(true)
                .layer(cors_layer)
                .layer(tonic_web::GrpcWebLayer::new())
                .add_service(VciServiceServer::with_interceptor(service, move |req| {
                    interceptor_key.verify_request(req)
                }))
                .serve_with_incoming_shutdown(incoming, shutdown)
                .await
        };

        if let Err(ref err) = result {
            warn!(%err, "gRPC server exited with error");
        } else {
            info!("gRPC server shut down");
        }
        result
    });

    Ok(VciServerContext {
        handle,
        shutdown_tx,
        endpoints,
        auth_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The interceptor entry point itself, not just `token::verify`'s pure
    /// format/HMAC logic -- confirms every rejection reason (no key
    /// installed, missing header, malformed `Bearer` prefix, wrong key)
    /// surfaces through `AuthKey::verify_request` as the same generic
    /// `UNAUTHENTICATED` status, and that a genuinely valid token is
    /// accepted through the real enforcement path.
    mod verify_request_tests {
        use super::*;

        #[tokio::test]
        async fn rejects_when_no_key_installed() {
            let auth = AuthKey::default();
            let req = tonic::Request::new(());
            let err = auth
                .verify_request(req)
                .expect_err("no key installed should be rejected");
            assert_eq!(err.code(), tonic::Code::Unauthenticated);
        }

        #[tokio::test]
        async fn rejects_missing_authorization_header() {
            let auth = AuthKey::default();
            auth.set([1u8; 32]).await;
            let req = tonic::Request::new(());
            let err = auth
                .verify_request(req)
                .expect_err("a request with no authorization header should be rejected");
            assert_eq!(err.code(), tonic::Code::Unauthenticated);
        }

        #[tokio::test]
        async fn rejects_malformed_bearer_prefix() {
            let auth = AuthKey::default();
            auth.set([1u8; 32]).await;
            let mut req = tonic::Request::new(());
            req.metadata_mut()
                .insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
            let err = auth
                .verify_request(req)
                .expect_err("a non-Bearer scheme should be rejected");
            assert_eq!(err.code(), tonic::Code::Unauthenticated);
        }

        #[tokio::test]
        async fn rejects_a_token_minted_under_a_different_key() {
            let auth = AuthKey::default();
            auth.set([1u8; 32]).await;
            let (token, _exp) =
                crate::token::mint(&[9u8; 32], "test-client", std::time::SystemTime::now());
            let mut req = tonic::Request::new(());
            req.metadata_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
            let err = auth
                .verify_request(req)
                .expect_err("a token minted under a different key should be rejected");
            assert_eq!(err.code(), tonic::Code::Unauthenticated);
        }

        #[tokio::test]
        async fn accepts_a_valid_token_for_the_installed_key() {
            let key = [7u8; 32];
            let auth = AuthKey::default();
            auth.set(key).await;
            let (token, _exp) =
                crate::token::mint(&key, "test-client", std::time::SystemTime::now());
            let mut req = tonic::Request::new(());
            req.metadata_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
            assert!(auth.verify_request(req).is_ok());
        }

        /// HTTP auth-scheme names are case-insensitive (RFC 7235 SS2.1) --
        /// `bearer`/`BEARER`/`Bearer` are all equally legal.
        #[tokio::test]
        async fn accepts_bearer_scheme_regardless_of_case() {
            let key = [7u8; 32];
            let auth = AuthKey::default();
            auth.set(key).await;
            let (token, _exp) =
                crate::token::mint(&key, "test-client", std::time::SystemTime::now());
            for scheme in ["bearer", "BEARER", "Bearer", "BeArEr"] {
                let mut req = tonic::Request::new(());
                req.metadata_mut().insert(
                    "authorization",
                    format!("{scheme} {token}").parse().unwrap(),
                );
                assert!(
                    auth.verify_request(req).is_ok(),
                    "scheme {scheme:?} should be accepted"
                );
            }
        }

        /// The credentials grammar (RFC 7235 SS2.1: `1*SP`) permits more
        /// than one space between the scheme and the token.
        #[tokio::test]
        async fn accepts_multiple_spaces_between_scheme_and_token() {
            let key = [7u8; 32];
            let auth = AuthKey::default();
            auth.set(key).await;
            let (token, _exp) =
                crate::token::mint(&key, "test-client", std::time::SystemTime::now());
            let mut req = tonic::Request::new(());
            req.metadata_mut().insert(
                "authorization",
                format!("Bearer   {token}").parse().unwrap(),
            );
            assert!(auth.verify_request(req).is_ok());
        }
    }

    /// On any host — dual-stack, IPv4-only, or IPv6-only — at least one
    /// loopback listener binds, and every bound listener shares one port.
    #[tokio::test]
    async fn binds_at_least_one_loopback_listener_on_a_shared_port() {
        let listeners = bind_loopback_listeners(0)
            .await
            .expect("some loopback family should be available");
        assert!(!listeners.is_empty());

        let ports: Vec<u16> = listeners
            .iter()
            .map(|l| {
                l.local_addr()
                    .expect("listener should have an address")
                    .port()
            })
            .collect();
        assert!(
            ports.iter().all(|&p| p == ports[0]),
            "ports differ: {ports:?}"
        );
    }

    /// A real bind error (the requested port is already taken) must still
    /// fail startup — only a missing address family is tolerated.
    #[tokio::test]
    async fn requested_port_conflict_still_fails() {
        // Occupy a concrete port on whichever family is available here.
        let occupied = bind_loopback_listeners(0)
            .await
            .expect("some loopback family should be available");
        let port = occupied[0]
            .local_addr()
            .expect("listener should have an address")
            .port();

        let result = bind_loopback_listeners(port).await;
        assert!(result.is_err(), "binding an occupied port should fail");
    }
}
