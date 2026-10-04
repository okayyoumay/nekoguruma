use base64::Engine as _;
use serde_json::Value;
use tracing::{debug, warn};

use crate::BoxError;

use super::errors::{JsonRpcError, success_response};

#[derive(Debug)]
pub struct DispatchOutcome {
    pub response: Value,
    pub exit_after_response: bool,
}

#[derive(Debug)]
pub struct MessageHandling {
    pub response: Option<Value>,
    pub exit_after_response: bool,
}

#[derive(Debug)]
pub struct RequestEnvelope<'a> {
    pub id: Option<&'a Value>,
    pub method: &'a str,
    pub params: Option<&'a Value>,
}

pub fn parse_request_envelope(request: &Value) -> Result<RequestEnvelope<'_>, JsonRpcError> {
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(JsonRpcError::invalid_request());
    }

    let method = request
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(JsonRpcError::invalid_request)?;

    Ok(RequestEnvelope {
        id: request.get("id"),
        method,
        params: request.get("params"),
    })
}

pub trait JsonRpcHandler {
    fn get_status(&self, params: Option<Value>) -> impl Future<Output = Value> + Send + '_;

    /// Installs (replacing any previous) the bearer-token signing key used
    /// by the shared gRPC listener's auth interceptor (see ADR-221). This is
    /// both the initial provisioning call and the revocation mechanism -- a
    /// new key invalidates every token minted against the old one.
    fn set_auth_key(&self, key: [u8; 32]) -> impl Future<Output = ()> + Send + '_;
}

/// Dispatch a JSON-RPC request and return the result or error.
pub async fn dispatch<C: JsonRpcHandler>(
    ctx: &Option<Result<C, BoxError>>,
    method: &str,
    params: Option<&Value>,
    library_name: Option<&str>,
) -> Result<DispatchOutcome, JsonRpcError> {
    match method {
        "ping" => Ok(DispatchOutcome {
            response: serde_json::json!({ "message": "pong" }),
            exit_after_response: false,
        }),
        "get_status" => match ctx {
            Some(Ok(json_rpc_context)) => {
                let service_status = json_rpc_context.get_status(params.cloned()).await;
                let endpoints = service_status
                    .get("endpoints")
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(DispatchOutcome {
                    response: serde_json::json!({
                        "running": true,
                        "library_name": library_name,
                        "endpoints": endpoints,
                    }),
                    exit_after_response: false,
                })
            }
            Some(Err(startup_error)) => {
                debug!(%startup_error, "get_status: service is not running due to startup error");
                Ok(DispatchOutcome {
                    response: serde_json::json!({
                        "running": false,
                        "library_name": library_name,
                        "endpoints": Value::Null,
                        "error": startup_error.to_string(),
                    }),
                    exit_after_response: false,
                })
            }
            None => Ok(DispatchOutcome {
                response: serde_json::json!({
                    "running": false,
                    "library_name": library_name,
                    "endpoints": Value::Null,
                }),
                exit_after_response: false,
            }),
        },
        "set_auth_key" => {
            let key_b64 = params
                .and_then(|p| p.get("key"))
                .and_then(Value::as_str)
                .ok_or_else(|| JsonRpcError::invalid_params("missing 'key' parameter"))?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(key_b64)
                .map_err(|_| JsonRpcError::invalid_params("'key' must be valid base64"))?;
            let key: [u8; 32] = decoded.try_into().map_err(|_| {
                JsonRpcError::invalid_params("'key' must decode to exactly 32 bytes")
            })?;

            match ctx {
                Some(Ok(json_rpc_context)) => {
                    json_rpc_context.set_auth_key(key).await;
                    Ok(DispatchOutcome {
                        response: serde_json::json!({}),
                        exit_after_response: false,
                    })
                }
                // `set_auth_key` is the manager's first request to a
                // freshly spawned child (before its first `get_status`
                // poll) -- surface the actual startup failure here, the
                // same detail `get_status`'s own `Some(Err(_))` arm above
                // reports, rather than a generic message that would
                // otherwise be the caller's only signal for what's usually
                // a real, previously-diagnosable startup error (bad
                // library path, misconfigured vendor tables, etc.).
                Some(Err(startup_error)) => {
                    debug!(%startup_error, "set_auth_key: service is not running due to startup error");
                    Err(JsonRpcError::server_error(format!(
                        "service startup failed: {startup_error}"
                    )))
                }
                None => Err(JsonRpcError::server_error(
                    "no running service to authenticate",
                )),
            }
        }
        "stop" => {
            debug!("stop requested via JSON-RPC");
            let stopped = matches!(ctx, Some(Ok(_)));
            Ok(DispatchOutcome {
                response: serde_json::json!({ "stopped": stopped }),
                exit_after_response: true,
            })
        }
        _ => {
            warn!(method, "unknown JSON-RPC method");
            Err(JsonRpcError::method_not_found())
        }
    }
}

/// Handle an incoming message and return response (or None for notifications).
pub async fn handle_message<C: JsonRpcHandler>(
    ctx: &Option<Result<C, BoxError>>,
    body: &[u8],
    library_name: Option<&str>,
) -> MessageHandling {
    let request: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(err) => {
            warn!(%err, "failed to parse JSON-RPC message");
            return MessageHandling {
                response: Some(JsonRpcError::parse_error().to_response(None)),
                exit_after_response: false,
            };
        }
    };

    let envelope = match parse_request_envelope(&request) {
        Ok(envelope) => envelope,
        Err(err) => {
            return MessageHandling {
                response: Some(err.to_response(request.get("id"))),
                exit_after_response: false,
            };
        }
    };

    if envelope.id.is_none() {
        let exit_after_response = dispatch(ctx, envelope.method, envelope.params, library_name)
            .await
            .map(|outcome| outcome.exit_after_response)
            .unwrap_or(false);
        return MessageHandling {
            response: None,
            exit_after_response,
        };
    }

    let (response, exit_after_response) =
        match dispatch(ctx, envelope.method, envelope.params, library_name).await {
            Ok(outcome) => (
                success_response(envelope.id, outcome.response),
                outcome.exit_after_response,
            ),
            Err(err) => (err.to_response(envelope.id), false),
        };

    MessageHandling {
        response: Some(response),
        exit_after_response,
    }
}
