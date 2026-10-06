//! gRPC client for a running worker (7.4).
//!
//! The worker's listener accepts only bearer tokens signed with the per-instance key the agent
//! provisioned over stdin (ADR-221). [`BearerAuth`] mints a fresh token from that key for every
//! call, so a long-lived client never presents an expired token. The key is held only by the agent
//! and the worker it was provisioned to; it never travels over the gRPC socket. The agent is
//! both the key holder and the only client, so it mints its own tokens (ADR-231).

use std::time::{Duration, SystemTime};

use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};
use vci_service_interface::token;
use vci_service_interface::vci_service_client::VciServiceClient;

use crate::service::WorkerProcess;

/// Token subject the agent presents; the listener does not interpret it.
pub const TOKEN_SUBJECT: &str = "ngr-agent";

/// D-PDU API client for one worker, authenticated with [`BearerAuth`].
pub type WorkerClient = VciServiceClient<InterceptedService<Channel, BearerAuth>>;

/// Adds `authorization: Bearer <token>` to every request, minted from the worker's auth key.
#[derive(Clone)]
pub struct BearerAuth {
    key: [u8; 32],
    subject: String,
}

impl BearerAuth {
    pub fn new(key: [u8; 32], subject: impl Into<String>) -> Self {
        Self {
            key,
            subject: subject.into(),
        }
    }
}

impl std::fmt::Debug for BearerAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key.
        f.debug_struct("BearerAuth")
            .field("subject", &self.subject)
            .finish_non_exhaustive()
    }
}

impl Interceptor for BearerAuth {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        let (token, _exp) = token::mint(&self.key, &self.subject, SystemTime::now());
        let value = MetadataValue::try_from(format!("Bearer {token}"))
            .map_err(|_| tonic::Status::internal("bearer token is not a valid header value"))?;
        request.metadata_mut().insert("authorization", value);
        Ok(request)
    }
}

/// Connection settings for [`connect`].
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// Timeout for establishing the connection to each endpoint.
    pub connect_timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("worker reported no gRPC endpoint")]
    NoEndpoint,
    #[error("invalid gRPC endpoint {endpoint:?}: {source}")]
    InvalidEndpoint {
        endpoint: String,
        #[source]
        source: tonic::transport::Error,
    },
    #[error("cannot connect to any worker endpoint (last tried {endpoint:?}): {source}")]
    Unreachable {
        endpoint: String,
        #[source]
        source: tonic::transport::Error,
    },
}

/// Connects to the first reachable endpoint in `endpoints` (as reported by `get_status`, e.g.
/// `127.0.0.1:54321` or `[::1]:54321`) and authenticates every call with tokens minted from
/// `key`. A malformed or unreachable entry is skipped; if none connects, the error of the last
/// entry is returned.
pub async fn connect(
    endpoints: &[String],
    key: [u8; 32],
    options: &ConnectOptions,
) -> Result<WorkerClient, ConnectError> {
    let mut last_error = ConnectError::NoEndpoint;
    for endpoint in endpoints {
        let channel = match Endpoint::from_shared(format!("http://{endpoint}")) {
            Ok(channel) => channel.connect_timeout(options.connect_timeout),
            Err(source) => {
                last_error = ConnectError::InvalidEndpoint {
                    endpoint: endpoint.clone(),
                    source,
                };
                continue;
            }
        };
        match channel.connect().await {
            Ok(channel) => {
                return Ok(VciServiceClient::with_interceptor(
                    channel,
                    BearerAuth::new(key, TOKEN_SUBJECT),
                ));
            }
            Err(source) => {
                last_error = ConnectError::Unreachable {
                    endpoint: endpoint.clone(),
                    source,
                }
            }
        }
    }
    Err(last_error)
}

impl WorkerProcess {
    /// Connects an authenticated D-PDU API client to this worker's gRPC listener.
    pub async fn connect(&self, options: &ConnectOptions) -> Result<WorkerClient, ConnectError> {
        connect(self.endpoints(), *self.auth_key(), options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [3u8; 32];

    #[test]
    fn interceptor_adds_a_token_the_listener_accepts() {
        let mut auth = BearerAuth::new(KEY, TOKEN_SUBJECT);
        let request = auth.call(tonic::Request::new(())).unwrap();
        let header = request
            .metadata()
            .get("authorization")
            .expect("authorization header")
            .to_str()
            .unwrap();
        let token = header.strip_prefix("Bearer ").expect("Bearer scheme");
        token::verify(&KEY, token, SystemTime::now()).unwrap();
        assert!(token::verify(&[4u8; 32], token, SystemTime::now()).is_err());
    }

    #[test]
    fn debug_output_omits_the_key() {
        let printed = format!("{:?}", BearerAuth::new(KEY, "x"));
        assert!(!printed.contains("key"), "{printed}");
    }

    #[tokio::test]
    async fn connect_without_endpoints_fails() {
        assert!(matches!(
            connect(&[], KEY, &ConnectOptions::default()).await,
            Err(ConnectError::NoEndpoint)
        ));
    }

    #[tokio::test]
    async fn connect_skips_a_malformed_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoints = [
            "not a valid authority".to_owned(),
            listener.local_addr().unwrap().to_string(),
        ];
        connect(&endpoints, KEY, &ConnectOptions::default())
            .await
            .expect("the second, reachable endpoint should be used");
        assert!(matches!(
            connect(&endpoints[..1], KEY, &ConnectOptions::default()).await,
            Err(ConnectError::InvalidEndpoint { .. })
        ));
    }

    #[tokio::test]
    async fn connect_reports_an_unreachable_endpoint() {
        // Bind and drop a listener so the port is very likely closed.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let endpoints = [format!("127.0.0.1:{port}")];
        assert!(matches!(
            connect(&endpoints, KEY, &ConnectOptions::default()).await,
            Err(ConnectError::Unreachable { .. })
        ));
    }
}
