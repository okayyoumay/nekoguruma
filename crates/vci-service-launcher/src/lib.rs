/// Re-exported from the standalone `vci-service-config` crate (see ADR-033)
/// so existing `vci_service_launcher::config::*` call sites keep working.
pub use vci_service_config as config;
pub mod jsonrpc;
mod logging;
/// The bearer-token format lives with the gRPC interface so that clients can
/// mint tokens without depending on this crate.
pub use vci_service_interface::token;
pub mod vci_server;

pub type BoxError = Box<dyn std::error::Error>;

pub async fn start_service<S, Args, E>(args: Args) -> Result<(), BoxError>
where
    S: vci_server::VciServer + Send + 'static,
    Args: IntoIterator<Item = E>,
    E: Into<String>,
{
    // Collect args so they can be inspected before starting the service.
    let args: Vec<String> = args.into_iter().map(Into::into).collect();

    // Parse startup config to derive the service identity used for config lookup.
    // If parsing fails we still initialize logging with defaults so that any
    // subsequent error messages are captured, then propagate the failure through
    // the JSON-RPC layer so callers can query status even when startup failed.
    let (startup_config_result, logging_cfg, library_name) =
        match S::get_startup_config(args.iter().map(String::as_str)) {
            Ok(cfg) => {
                let identity = S::service_identity(&cfg);
                let logging = config::find_logging_config(
                    identity.api_name,
                    identity.arch,
                    &identity.library_name,
                );
                let name = identity.library_name.clone();
                (Ok(cfg), logging, Some(name))
            }
            Err(e) => (Err(e), config::LoggingConfig::default(), None),
        };

    logging::init_logging(logging_cfg);

    // Spawn the gRPC service, passing any startup error through to the JSON-RPC
    // context so callers can still query status even if the service failed to start.
    let vci_context = match startup_config_result {
        Ok(cfg) => vci_server::spawn_vci_context::<S>(cfg).await,
        Err(e) => Err(e),
    };

    jsonrpc::server::ServerContext::new(vci_context, library_name)
        .run_stdio_server()
        .await?;
    Ok(())
}
