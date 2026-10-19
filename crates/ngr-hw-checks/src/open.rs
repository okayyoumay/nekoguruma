//! Opening the profile's device through a worker (design 13.5, ADR-275 item 4): starts the
//! worker for the profile's VCI, reads what it reports, runs the module match of [`device`]
//! and returns the opened device only when the match holds.
//!
//! The steps, in order, with no link created by any of them:
//!
//! 1. Start the worker for the profile's library as the agent starts it
//!    (`agent::launch::launch_worker`).
//! 2. `GetModuleIds`, and for J2534 the `modules` entries of the worker's configuration, then
//!    [`select_module`]: a wrong, absent or ambiguous module is refused here, before anything is
//!    opened.
//! 3. `ModuleConnect` on the selected module, `GetVersion`, then [`check_versions`].
//!
//! A refusal at any step disconnects the module if it was connected and stops the worker, so a
//! refused device is left as found. A check creates its links only on the returned
//! [`OpenedDevice`].
//!
//! The J2534 worker lists every configured `modules` entry until it can be started with a label
//! (ADR-281 Decision item 3), so [`select_module`]'s rule that the profile's entry is the only
//! configured one applies in full.

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
    #[error("cannot read the worker's `modules` configuration: {0}")]
    Config(String),
    #[error("the worker reports the selected module without a handle")]
    NoHandle,
    /// The device is not the profile's. No link was created.
    #[error("the attached device is not the profile's: {0}")]
    Refused(#[from] MatchRefusal),
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

    /// Disconnects the module and stops the worker. Dropping the device kills the worker without
    /// disconnecting the module.
    pub async fn close(mut self) -> Result<Stopped, OpenError> {
        disconnect(&mut self.client, self.module_handle).await;
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
    match match_device(vci, abi, &mut client).await {
        Ok((module_handle, module, version)) => Ok(OpenedDevice {
            abi,
            process,
            client,
            module_handle,
            module,
            version,
        }),
        Err(error) => {
            // The refusal is the result; a worker that does not stop is killed on drop.
            let _ = process.stop(STOP_GRACE).await;
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
) -> Result<(ModuleHandle, ReportedModule, ReportedVersion), OpenError> {
    let reported = client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .map_err(|status| rpc("GetModuleIds", status))?
        .into_inner()
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
    let at = select_module(vci, &modules, &configured)?;
    let module_handle = reported[at].module_handle.ok_or(OpenError::NoHandle)?;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(module_handle),
        })
        .await
        .map_err(|status| rpc("ModuleConnect", status))?;
    let version = async {
        let data = client
            .get_version(GetVersionRequest {
                module_handle: Some(module_handle),
            })
            .await
            .map_err(|status| rpc("GetVersion", status))?
            .into_inner()
            .version_data
            .unwrap_or_default();
        let version = ReportedVersion {
            hw_name: data.hw_name,
            fw_name: data.fw_name,
            fw_version: data.fw_version,
            pdu_api_sw_version: data.pdu_api_sw_version,
        };
        check_versions(vci, &version)?;
        Ok::<_, OpenError>(version)
    }
    .await;
    match version {
        Ok(version) => Ok((module_handle, modules[at].clone(), version)),
        Err(error) => {
            disconnect(client, module_handle).await;
            Err(error)
        }
    }
}

fn rpc(call: &'static str, status: impl std::fmt::Display) -> OpenError {
    OpenError::Rpc {
        call,
        message: status.to_string(),
    }
}

/// Disconnects `module_handle`, ignoring a failure: it is the cleanup of a device that is being
/// let go.
async fn disconnect(client: &mut WorkerClient, module_handle: ModuleHandle) {
    let _ = client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(module_handle),
        })
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
