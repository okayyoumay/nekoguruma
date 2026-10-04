use std::io;

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::{
    BoxError,
    jsonrpc::{
        codec::{MessageCodec, OneLinerCodec},
        dispatch::{JsonRpcHandler, MessageHandling, handle_message},
    },
};

pub trait JsonRpcContext: JsonRpcHandler {
    fn shutdown(self) -> impl std::future::Future<Output = ()> + Send + 'static;
}

pub struct ServerState<S: JsonRpcContext> {
    json_rpc_context: Option<Result<S, BoxError>>,
}

pub struct ServerContext<S: JsonRpcContext> {
    pub state: Mutex<ServerState<S>>,
    pub library_name: Option<String>,
}

impl<S: JsonRpcContext> ServerContext<S> {
    pub fn new(running: Result<S, BoxError>, library_name: Option<String>) -> Self {
        if let Err(ref err) = running {
            warn!(%err, "VCI service failed to start");
        }
        Self {
            state: Mutex::new(ServerState {
                json_rpc_context: Some(running),
            }),
            library_name,
        }
    }

    pub async fn shutdown_for_process_exit(&self) {
        let mut state = self.state.lock().await;
        if let Some(Ok(json_rpc_context)) = state.json_rpc_context.take() {
            json_rpc_context.shutdown().await;
        }
    }

    pub async fn handle_message(&self, body: &[u8]) -> MessageHandling {
        let state = self.state.lock().await;
        handle_message(&state.json_rpc_context, body, self.library_name.as_deref()).await
    }

    pub async fn run_stdio_server(&self) -> Result<(), io::Error> {
        info!("JSON-RPC stdio server started");
        let codec = OneLinerCodec;

        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();

        loop {
            let message = codec.read_message(&mut stdin).await;
            let message = match message {
                Ok(message) => message,
                Err(err) => {
                    if err.kind() != io::ErrorKind::UnexpectedEof {
                        warn!(%err, "error reading JSON-RPC message from stdin");
                    }
                    self.shutdown_for_process_exit().await;
                    break;
                }
            };

            let handling = self.handle_message(&message).await;

            if let Some(response) = handling.response {
                let bytes = response.to_string().into_bytes();
                if codec.write_message(&mut stdout, &bytes).await.is_err() {
                    self.shutdown_for_process_exit().await;
                    break;
                }
            }

            if handling.exit_after_response {
                info!("JSON-RPC stop requested, shutting down");
                self.shutdown_for_process_exit().await;
                break;
            }
        }

        debug!("JSON-RPC stdio server stopped");
        Ok(())
    }
}
