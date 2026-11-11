//! `ngr-server --config <file>`: the server (`crates/server/docs/ngr-server.md`).
//! Exit codes: 0 after a graceful stop, 1 for a start or run failure, 2 for a usage error.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use tokio::sync::oneshot;

use server::{Config, Server};

const USAGE: &str = "usage: ngr-server --config <file>\n";

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<PathBuf, String> {
    let mut args = args.into_iter();
    let mut config = None;
    while let Some(arg) = args.next() {
        if arg == "--config" {
            let value = args.next().ok_or("--config needs a file")?;
            if config.replace(PathBuf::from(value)).is_some() {
                return Err("--config given twice".into());
            }
        } else {
            return Err(format!("unknown argument {}", arg.to_string_lossy()));
        }
    }
    config.ok_or_else(|| "--config is required".into())
}

#[tokio::main]
async fn main() -> ExitCode {
    let path = match parse_args(std::env::args_os().skip(1)) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("ngr-server: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match serve(&path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            tracing::error!(%message, "ngr-server failed");
            ExitCode::from(1)
        }
    }
}

async fn serve(path: &std::path::Path) -> Result<(), String> {
    let config = Config::load(path).map_err(|e| e.to_string())?;
    let server = Server::start(&config).await.map_err(|e| e.to_string())?;
    // The handlers are in place before anyone is told the server listens, so a stop signal sent
    // on that cue is never lost.
    let (first, second) = stop_signals()?;
    tracing::info!(origin = %server.pinned_origin(), "listening");
    server
        .run(
            async {
                let _ = first.await;
            },
            async {
                let _ = second.await;
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("stopped");
    Ok(())
}

/// Installs the stop-signal handlers (SIGTERM and SIGINT on Unix, Ctrl-C and Ctrl-Break on
/// Windows) and returns two receivers: the first stop signal, which begins the graceful stop, and
/// a second one, which ends the wait for requests in flight at once.
fn stop_signals() -> Result<(oneshot::Receiver<()>, oneshot::Receiver<()>), String> {
    let (first_tx, first_rx) = oneshot::channel();
    let (second_tx, second_rx) = oneshot::channel();
    let mut signals = Signals::install().map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        signals.next().await;
        let _ = first_tx.send(());
        signals.next().await;
        let _ = second_tx.send(());
    });
    Ok((first_rx, second_rx))
}

#[cfg(unix)]
struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    fn install() -> std::io::Result<Signals> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Signals {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
        })
    }

    async fn next(&mut self) {
        tokio::select! { _ = self.term.recv() => {}, _ = self.int.recv() => {} }
    }
}

#[cfg(windows)]
struct Signals {
    c: tokio::signal::windows::CtrlC,
    b: tokio::signal::windows::CtrlBreak,
}

#[cfg(windows)]
impl Signals {
    fn install() -> std::io::Result<Signals> {
        Ok(Signals {
            c: tokio::signal::windows::ctrl_c()?,
            b: tokio::signal::windows::ctrl_break()?,
        })
    }

    async fn next(&mut self) {
        tokio::select! { _ = self.c.recv() => {}, _ = self.b.recv() => {} }
    }
}
