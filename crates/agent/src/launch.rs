//! Launching the worker for a VCI (design 7.3, 7.4).
//!
//! The library behind a VCI name is resolved with the resolver the j2534-0404 service itself
//! uses (`j2534_0404_registry::resolve_library_path`), so the file whose header picks the ABI
//! is the file the service will load. The service is then started with the name only and
//! resolves it again on its side (7.2).

use std::path::PathBuf;

use worker_host::abi::{self, Abi, AbiError};
use worker_host::client::{ConnectError, ConnectOptions, WorkerClient};
use worker_host::service::{LaunchOptions, ServiceError, ServiceKind, WorkerLayout, WorkerProcess};

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("cannot resolve the library of VCI {vci:?}: {source}")]
    Resolve {
        vci: String,
        #[source]
        source: j2534_0404_registry::RegistryError,
    },
    #[error("cannot determine the ABI of {}: {source}", path.display())]
    Abi {
        path: PathBuf,
        #[source]
        source: AbiError,
    },
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Connect(#[from] ConnectError),
}

/// A running worker and a client connected to it.
pub struct LaunchedWorker {
    pub abi: Abi,
    pub process: WorkerProcess,
    pub client: WorkerClient,
}

/// Launches the j2534-0404 worker that matches the ABI of `vci`'s library and connects a client
/// to it.
///
/// `options.long_size` is filled with the ABI's default (7.1.2) when it is `None`; the Linux
/// registration definition that could override it (7.1.1) is not read yet.
pub async fn launch_j2534_worker(
    vci: &str,
    workers: &WorkerLayout,
    options: &LaunchOptions,
) -> Result<LaunchedWorker, LaunchError> {
    // `None`: the agent does not know the worker's architecture before reading the header, so
    // only the api-level `library_path` entry and the registry apply here.
    let path = j2534_0404_registry::resolve_library_path(None, vci).map_err(|source| {
        LaunchError::Resolve {
            vci: vci.to_owned(),
            source,
        }
    })?;
    let abi = abi::detect_file(&path).map_err(|source| LaunchError::Abi {
        path: path.clone(),
        source,
    })?;
    let binary = workers.find(ServiceKind::J2534V0404, abi)?;
    let options = LaunchOptions {
        long_size: options.long_size.or(Some(abi.default_long_size())),
        ..options.clone()
    };
    let process = WorkerProcess::launch(&binary, ServiceKind::J2534V0404, vci, &options).await?;
    let client = process.connect(&ConnectOptions::default()).await?;
    Ok(LaunchedWorker {
        abi,
        process,
        client,
    })
}
