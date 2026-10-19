//! Opening the profile's device through a worker (design 13.5, ADR-275 item 4): starts the
//! worker for the profile's VCI, reads what it reports, runs the module match of [`device`]
//! and returns the opened device only when the match holds.
//!
//! The steps, in order, with no link created by any of them:
//!
//! 1. Start the worker for the profile's library as the agent starts it
//!    (`agent::launch::launch_worker`).
//! 2. `GetModuleIds`, and for J2534 the `modules` entries of the harness's own view of the
//!    worker's configuration, then [`select_module`]: a wrong, absent or ambiguous module, or a
//!    J2534 worker that reports more than one, is refused here, before anything is opened.
//! 3. `ModuleConnect` on the selected module, `GetVersion`, then [`check_versions`].
//!
//! Every worker request is bounded by the launch options' `request_timeout`; a request that
//! times out is an error. A refusal or an error at any step disconnects the module, when the
//! step is `ModuleConnect` or later (a failed `ModuleConnect` can leave the device opened), and
//! stops the worker, which is killed if it does not exit. A hung or failed disconnect does not
//! keep the worker from being stopped. A check creates its links only on the returned
//! [`OpenedDevice`].
//!
//! The J2534 worker lists every configured `modules` entry until it can be started with a label
//! and `pname` (ADR-281 Decision item 3), so [`select_module`]'s rule that the profile's entry is
//! the only configured one applies in full, and the worker must report one module.

use std::future::Future;
use std::time::Duration;

use agent::launch::{Api, LaunchError, launch_worker};
use vci_service_interface::{
    GetModuleIdsRequest, GetVersionRequest, ModuleConnectRequest, ModuleDisconnectRequest,
    ModuleHandle,
};
use worker_host::abi::Abi;
use worker_host::client::WorkerClient;
use worker_host::service::{
    LaunchOptions, OperatingMode, ServiceError, Stopped, WorkerLayout, WorkerProcess,
};

use crate::device::{
    ConfiguredModule, MatchRefusal, ReportedModule, ReportedVersion, check_versions, select_module,
};
use crate::profile::{self, Vci};

/// How long a worker gets to exit after its stop request before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Why the profile's device could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("cannot start the worker: {0}")]
    Launch(#[from] LaunchError),
    /// A worker request failed; `call` names it.
    #[error("{call} failed: {message}")]
    Rpc { call: &'static str, message: String },
    /// A worker request got no answer within the request timeout.
    #[error("{call} got no answer within {timeout:?}")]
    Timeout {
        call: &'static str,
        timeout: Duration,
    },
    #[error("cannot read the worker's `modules` configuration: {0}")]
    Config(String),
    #[error("the worker reports the selected module without a handle")]
    NoHandle,
    /// `GetVersion` answered without version data, so the versions cannot be compared.
    #[error("the worker answered `GetVersion` without version data")]
    NoVersion,
    /// The device is not the profile's. No link was created. `stopped` is how the worker ended
    /// afterwards, `None` when stopping it failed.
    #[error("the attached device is not the profile's: {refusal}")]
    Refused {
        refusal: MatchRefusal,
        stopped: Option<Stopped>,
    },
    #[error("cannot stop the worker: {0}")]
    Stop(#[from] ServiceError),
}

/// The profile's device, opened by a worker and matched with the profile. A check creates its
/// links on it; creating them is not part of opening it.
pub struct OpenedDevice {
    abi: Abi,
    process: WorkerProcess,
    client: WorkerClient,
    module_handle: ModuleHandle,
    module: ReportedModule,
    version: ReportedVersion,
    timeout: Duration,
}

impl OpenedDevice {
    /// The ABI of the worker that opened the device.
    pub fn abi(&self) -> Abi {
        self.abi
    }

    pub fn module_handle(&self) -> ModuleHandle {
        self.module_handle
    }

    /// The module as the worker reported it.
    pub fn module(&self) -> &ReportedModule {
        &self.module
    }

    /// The versions the device reported, which equal the profile's.
    pub fn version(&self) -> &ReportedVersion {
        &self.version
    }

    /// A client for the worker, which shares its connection. The guarded handle
    /// ([`crate::guard::GuardedHandle`]) is the only user a check gets; the client itself is not
    /// handed to a check (design 13.5 "Guarded VCI access").
    pub(crate) fn client(&self) -> WorkerClient {
        self.client.clone()
    }

    /// Disconnects the module and stops the worker. Dropping the device kills the worker without
    /// disconnecting the module.
    pub async fn close(mut self) -> Result<Stopped, OpenError> {
        disconnect(&mut self.client, self.module_handle, self.timeout).await;
        Ok(self.process.stop(STOP_GRACE).await?)
    }
}

/// Opens the device `vci` names: starts its worker from `workers` in `mode`, matches what the
/// worker reports with the profile and returns the opened device. On a refusal or an error the
/// module is disconnected if it was connected and the worker is stopped. No link is created.
pub async fn open_device(
    vci: &Vci,
    workers: &WorkerLayout,
    mode: OperatingMode,
    options: &LaunchOptions,
) -> Result<OpenedDevice, OpenError> {
    let api = match vci.api {
        profile::Api::J2534V0404 => Api::J2534,
        profile::Api::Iso22900 => Api::Dpdu,
    };
    let worker = launch_worker(api, &vci.library, mode, workers, options).await?;
    let (abi, process, mut client) = (worker.abi, worker.process, worker.client);
    let timeout = options.request_timeout;
    match match_device(vci, abi, &mut client, timeout).await {
        Ok((module_handle, module, version)) => Ok(OpenedDevice {
            abi,
            process,
            client,
            module_handle,
            module,
            version,
            timeout,
        }),
        Err(mut error) => {
            // The error is the result; a worker that does not stop is killed on drop.
            let ended = process.stop(STOP_GRACE).await.ok();
            if let OpenError::Refused { stopped, .. } = &mut error {
                *stopped = ended;
            }
            Err(error)
        }
    }
}

/// Steps 2 and 3 of the module: the match before and after opening. A connected module that
/// fails the version check is disconnected before this returns.
async fn match_device(
    vci: &Vci,
    abi: Abi,
    client: &mut WorkerClient,
    timeout: Duration,
) -> Result<(ModuleHandle, ReportedModule, ReportedVersion), OpenError> {
    let reported = bounded(
        timeout,
        "GetModuleIds",
        client.get_module_ids(GetModuleIdsRequest {}),
    )
    .await?
    .module_id_list
    .map(|list| list.module_data)
    .unwrap_or_default();
    let modules: Vec<ReportedModule> = reported
        .iter()
        .map(|data| ReportedModule {
            name: data.vendor_module_name.clone(),
            additional_info: data.vendor_additional_info.clone(),
        })
        .collect();
    let configured = match vci.api {
        profile::Api::J2534V0404 => configured_modules(abi, &vci.library)?,
        profile::Api::Iso22900 => Vec::new(),
    };
    let at = select_module(vci, &modules, &configured).map_err(|refusal| OpenError::Refused {
        refusal,
        stopped: None,
    })?;
    let module_handle = reported[at].module_handle.ok_or(OpenError::NoHandle)?;

    let connected = bounded(
        timeout,
        "ModuleConnect",
        client.module_connect(ModuleConnectRequest {
            module_handle: Some(module_handle),
        }),
    )
    .await;
    if let Err(error) = connected {
        // The device can be open although the call failed or timed out; disconnecting a module
        // that is not connected changes nothing.
        disconnect(client, module_handle, timeout).await;
        return Err(error);
    }
    let version = async {
        let data = bounded(
            timeout,
            "GetVersion",
            client.get_version(GetVersionRequest {
                module_handle: Some(module_handle),
            }),
        )
        .await?
        .version_data
        .ok_or(OpenError::NoVersion)?;
        let version = ReportedVersion {
            vendor_name: data.vendor_name,
            hw_name: data.hw_name,
            fw_name: data.fw_name,
            fw_version: data.fw_version,
            pdu_api_sw_version: data.pdu_api_sw_version,
        };
        check_versions(vci, &version).map_err(|refusal| OpenError::Refused {
            refusal,
            stopped: None,
        })?;
        Ok::<_, OpenError>(version)
    }
    .await;
    match version {
        Ok(version) => Ok((module_handle, modules[at].clone(), version)),
        Err(error) => {
            disconnect(client, module_handle, timeout).await;
            Err(error)
        }
    }
}

/// Calls a unary worker request, failing when it errors or gets no answer within `timeout`.
async fn bounded<T>(
    timeout: Duration,
    call: &'static str,
    request: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
) -> Result<T, OpenError> {
    match tokio::time::timeout(timeout, request).await {
        Err(_) => Err(OpenError::Timeout { call, timeout }),
        Ok(Err(status)) => Err(OpenError::Rpc {
            call,
            message: status.to_string(),
        }),
        Ok(Ok(response)) => Ok(response.into_inner()),
    }
}

/// Disconnects `module_handle` within `timeout`, ignoring a failure: it is the cleanup of a
/// device that is being let go, and the worker is stopped next either way.
async fn disconnect(client: &mut WorkerClient, module_handle: ModuleHandle, timeout: Duration) {
    let _ = bounded(
        timeout,
        "ModuleDisconnect",
        client.module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(module_handle),
        }),
    )
    .await;
}

/// The `modules` entries of the J2534 worker configuration for `library`, read from the file the
/// worker reads (`vci-service-config`): the worker of `abi` looks them up under its own
/// architecture key on Windows. An absent `modules` key is the worker's single default entry,
/// which has no `pname`, so nothing is configured.
fn configured_modules(abi: Abi, library: &str) -> Result<Vec<ConfiguredModule>, OpenError> {
    let arch = match abi {
        Abi::WinX86 => Some("x86"),
        Abi::WinX64 => Some("x86_x64"),
        _ => None,
    };
    let entries = vci_service_config::find_modules("j2534-0404", arch, library)
        .map_err(|error| OpenError::Config(error.to_string()))?
        .unwrap_or_default();
    Ok(entries
        .into_iter()
        .map(|entry| ConfiguredModule {
            label: entry.label,
            pname: entry.pname,
        })
        .collect())
}
