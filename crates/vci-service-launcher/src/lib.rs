/// Re-exported from the standalone `vci-service-config` crate (see ADR-033)
/// so existing `vci_service_launcher::config::*` call sites keep working.
pub use vci_service_config as config;
mod config_check;
pub use config_check::ConfigFile;
pub mod jsonrpc;
mod logging;
/// The operating mode in the startup argument (ADR-281 Decision item 2) is spelled in the
/// gRPC interface crate, which the agent shares.
pub use vci_service_interface::mode;
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
    //
    // The configuration file is checked (design 7.2) before any setting is read
    // from it, since its logging settings name a file the worker opens: a file
    // device mode refuses leaves logging at its defaults (ADR-285).
    let (startup, logging_cfg, library_name) =
        match S::get_startup_config(args.iter().map(String::as_str)) {
            Ok(cfg) => {
                let identity = S::service_identity(&cfg);
                let outcome = config_check::check();
                let logging = outcome.logging_config(identity.mode, || {
                    config::find_logging_config(
                        identity.api_name,
                        identity.arch,
                        &identity.library_name,
                    )
                });
                let name = identity.library_name.clone();
                (Ok((cfg, outcome, identity.mode)), logging, Some(name))
            }
            Err(e) => (Err(e), config::LoggingConfig::default(), None),
        };

    logging::init_logging(logging_cfg);

    // Spawn the gRPC service, passing any startup error through to the JSON-RPC
    // context so callers can still query status even if the service failed to start.
    let vci_context = match startup {
        Ok((cfg, outcome, mode)) => match outcome.verdict(mode) {
            Ok(config_file) => vci_server::spawn_vci_context::<S>(cfg, config_file).await,
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };

    jsonrpc::server::ServerContext::new(vci_context, library_name)
        .run_stdio_server()
        .await?;
    Ok(())
}
