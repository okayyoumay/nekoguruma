//! Launching the worker for a VCI (design 7.3, 7.4).
//!
//! A VCI is a J2534 v04.04 library or a D-PDU API implementation ([`Api`]). The library behind
//! its name is resolved for each worker build through `ngr-library-resolver` (7.1.1), as that
//! build resolves it: for J2534 the build's registry view on Windows, the registration definition
//! on Linux, and in a debug build the `NGR_J2534_DEFINITION_DIR` definition directory on both;
//! for the D-PDU API the root description file the build's registry view names on Windows, the
//! fixed root file on Linux, and in a debug build the `NGR_PDU_API_ROOT_FILE` file on both
//! (ADR-282). The file's header picks the ABI, so the file whose header selects the build is the
//! file that build loads (ADR-281 Decision item 1). The service (`j2534-0404-service` or
//! `iso22900-service`) is then started with the name and the device's operating mode (ADR-281
//! Decision item 2; 7.2 gives the mode its meaning for a failed library check) and resolves the
//! name again itself. ADR-240 records the selection rule; it applies to both APIs alike.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use j2534_0404_registry::RegistryViewMode;
use ngr_library_resolver::{RegistryView, iso22900, j2534_0404};
use worker_host::abi::{self, Abi, AbiError};
use worker_host::client::{ConnectError, ConnectOptions, WorkerClient};
use worker_host::service::{
    LaunchOptions, OperatingMode, ServiceError, ServiceKind, WorkerLayout, WorkerProcess,
};

use crate::client::{ExitHow, WorkerExit, WorkerSource, WorkerStatus};
use crate::host::HostError;

/// The API a VCI's library implements, which decides the worker service that loads it. The
/// names are those of a VCI profile's `standard.kind` (`schemas/vci-profile.schema.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    /// J2534 v04.04, loaded by `j2534-0404-service`.
    J2534,
    /// The D-PDU API, loaded by `iso22900-service`.
    Dpdu,
}

impl Api {
    fn service(self) -> ServiceKind {
        match self {
            Api::J2534 => ServiceKind::J2534V0404,
            Api::Dpdu => ServiceKind::Iso22900,
        }
    }
}

impl fmt::Display for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Api::J2534 => "j2534",
            Api::Dpdu => "dpdu",
        })
    }
}

impl FromStr for Api {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "j2534" => Ok(Api::J2534),
            "dpdu" => Ok(Api::Dpdu),
            _ => Err(format!("unknown API {text:?} (expected j2534 or dpdu)")),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("cannot resolve the library of VCI {vci:?}: {}", reasons.join("; "))]
    Resolve { vci: String, reasons: Vec<String> },
    /// The ABI's `unsigned long` is 32 bits wide, so it cannot be 8 bytes (7.1.2).
    #[error("VCI {vci:?}: LongSize {long_size} is not possible for {}", abi.name())]
    UnsupportedLongSize {
        vci: String,
        abi: Abi,
        long_size: u8,
    },
    /// The caller named a worker ABI this agent build cannot run (7.3), so no resolution is tried.
    #[error("VCI {vci:?}: this agent build cannot run a {} worker", abi.name())]
    AbiNotRunnable { vci: String, abi: Abi },
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Connect(#[from] ConnectError),
}

/// How one worker build resolves a library name: the registry view it reads on Windows, and the
/// ABI it is built for (`None`: any, there is one build per host on Linux). The same builds and
/// order serve both APIs.
#[derive(Debug, Clone, Copy)]
struct Resolution {
    #[cfg_attr(
        not(windows),
        expect(dead_code, reason = "only Windows reads a registry view")
    )]
    view: RegistryViewMode,
    abi: Option<Abi>,
}

/// The worker builds the agent can choose from, preferred first. A 64-bit Windows agent reads
/// both registry views explicitly (7.1) and prefers the native x64 build when a VCI has a
/// library in both.
#[cfg(all(windows, target_arch = "x86_64"))]
const RESOLUTIONS: &[Resolution] = &[
    Resolution {
        view: RegistryViewMode::W64,
        abi: Some(Abi::WinX64),
    },
    Resolution {
        view: RegistryViewMode::W32,
        abi: Some(Abi::WinX86),
    },
];

#[cfg(all(windows, target_arch = "x86"))]
const RESOLUTIONS: &[Resolution] = &[Resolution {
    view: RegistryViewMode::Native,
    abi: Some(Abi::WinX86),
}];

// No worker is built for any other Windows architecture.
#[cfg(all(windows, not(any(target_arch = "x86_64", target_arch = "x86"))))]
compile_error!("no worker build is defined for this Windows architecture");

#[cfg(not(windows))]
const RESOLUTIONS: &[Resolution] = &[Resolution {
    view: RegistryViewMode::Native,
    abi: None,
}];

/// The ABIs a worker build without an ABI of its own (`Resolution::abi` is `None`: one build per
/// host, the library's header decides) can serve on this host (7.3 "Bundled workers"): an x86_64
/// Linux host the x64 and x86 workers, an aarch64 one the arm64 and armhf workers, and a 32-bit
/// one its own. No other host (macOS, a Windows host, whose builds name their ABI) serves any.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const HOST_ABIS: &[Abi] = &[Abi::LinuxX86_64, Abi::LinuxX86];
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const HOST_ABIS: &[Abi] = &[Abi::LinuxArm64, Abi::LinuxArmhf];
#[cfg(all(target_os = "linux", target_arch = "x86"))]
const HOST_ABIS: &[Abi] = &[Abi::LinuxX86];
#[cfg(all(target_os = "linux", target_arch = "arm"))]
const HOST_ABIS: &[Abi] = &[Abi::LinuxArmhf];
#[cfg(not(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "x86",
        target_arch = "arm"
    )
)))]
const HOST_ABIS: &[Abi] = &[];

/// The resolutions that may serve a caller who names the worker `abi`: those built for it. A
/// resolution without an ABI stays only when `abi` is one of `served` (the ABIs such a build can
/// serve, [`HOST_ABIS`]), and then requires exactly `abi`, so a library of another ABI is refused
/// instead of launched. Empty when this agent build cannot run `abi`.
fn restrict(resolutions: &[Resolution], abi: Abi, served: &[Abi]) -> Vec<Resolution> {
    resolutions
        .iter()
        .filter(|r| match r.abi {
            Some(built) => built == abi,
            None => served.contains(&abi),
        })
        .map(|r| Resolution {
            abi: Some(abi),
            ..*r
        })
        .collect()
}

/// Whether this agent build can run a worker built for `abi` (7.3): on Windows the ABIs its
/// registry views serve, on Linux the ABIs of the host table (whether the worker binary is
/// installed is found when it is launched). A hardware profile checks it when it is loaded.
pub fn host_runs(abi: Abi) -> bool {
    !restrict(RESOLUTIONS, abi, HOST_ABIS).is_empty()
}

/// What one worker build would load for a VCI: the library, and the `LongSize` its J2534
/// registration definition declares (7.1.1; a registry entry and a D-PDU API implementation
/// declare none).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Library {
    path: PathBuf,
    long_size: Option<u8>,
}

/// What it takes to start a job's worker again (ADR-394): the binary the first launch chose and
/// the arguments it was started with.
#[derive(Debug, Clone)]
pub struct LaunchRecipe {
    pub binary: PathBuf,
    pub kind: ServiceKind,
    /// The VCI's library name, as the worker's startup argument gives it.
    pub vci: String,
    pub mode: OperatingMode,
    pub options: LaunchOptions,
    pub connect: ConnectOptions,
}

impl LaunchRecipe {
    /// Starts a worker by the recipe and connects a client to it. The process is killed when the
    /// connection fails.
    pub async fn launch(&self) -> Result<(WorkerProcess, WorkerClient), LaunchError> {
        let process =
            WorkerProcess::launch(&self.binary, self.kind, &self.vci, self.mode, &self.options)
                .await?;
        let client = process.connect(&self.connect).await?;
        Ok((process, client))
    }
}

/// A running worker and a client connected to it.
pub struct LaunchedWorker {
    pub abi: Abi,
    /// The library the lookup resolved and the worker loads (7.1.1).
    pub library: PathBuf,
    /// The `unsigned long` width the worker was started with (7.1.2): for a J2534 worker the
    /// override, else the registration definition's `LongSize`, else the ABI's default; for a
    /// D-PDU API worker, which takes none, the ABI's default.
    pub long_size: u8,
    pub process: WorkerProcess,
    pub client: WorkerClient,
    /// How the worker was started, to start it again ([`RelaunchingWorker`]).
    pub recipe: LaunchRecipe,
}

/// Launches the worker of `api` that matches the ABI of `vci`'s library in the device's
/// operating `mode` and connects a client to it.
///
/// A J2534 worker's `long_size` is `options.long_size`, else the `LongSize` of the registration
/// definition the lookup resolves (7.1.1), else the ABI's default (7.1.2). An 8-byte width for an
/// ABI whose `unsigned long` is 32 bits wide fails the launch. A D-PDU API worker takes no
/// `long_size`.
///
/// `abi` restricts the launch to the worker build for that ABI (`None`: the agent's own
/// preference, the first build whose registry view lists a library of its ABI). The named ABI is
/// never replaced by another: an agent build that cannot run it fails with
/// [`LaunchError::AbiNotRunnable`], a VCI with no library of it (or, outside Windows, a library of
/// another ABI) with [`LaunchError::Resolve`].
pub async fn launch_worker(
    api: Api,
    vci: &str,
    mode: OperatingMode,
    workers: &WorkerLayout,
    options: &LaunchOptions,
    abi: Option<Abi>,
) -> Result<LaunchedWorker, LaunchError> {
    let named;
    let resolutions: &[Resolution] = match abi {
        None => RESOLUTIONS,
        Some(abi) => {
            named = restrict(RESOLUTIONS, abi, HOST_ABIS);
            if named.is_empty() {
                return Err(LaunchError::AbiNotRunnable {
                    vci: vci.to_owned(),
                    abi,
                });
            }
            &named
        }
    };
    let (library, abi, long_size, options) = match api {
        Api::J2534 => {
            let (library, abi) = resolve(vci, resolutions, lookup_j2534, abi::detect_file)?;
            let long_size = worker_long_size(vci, abi, options.long_size, &library)?;
            let options = LaunchOptions {
                long_size: Some(long_size),
                ..options.clone()
            };
            (library.path, abi, long_size, options)
        }
        Api::Dpdu => {
            let (library, abi) = resolve(vci, resolutions, lookup_dpdu, abi::detect_file)?;
            let options = LaunchOptions {
                long_size: None,
                ..options.clone()
            };
            (library.path, abi, abi.default_long_size(), options)
        }
    };
    let binary = workers.find(api.service(), abi)?;
    let recipe = LaunchRecipe {
        binary,
        kind: api.service(),
        vci: vci.to_owned(),
        mode,
        options,
        connect: ConnectOptions::default(),
    };
    let (process, client) = recipe.launch().await?;
    Ok(LaunchedWorker {
        abi,
        library,
        long_size,
        process,
        client,
        recipe,
    })
}

/// The current worker process of a [`RelaunchingWorker`], shared with whoever stops it when the
/// job is over.
pub type WorkerSlot = Arc<Mutex<Option<WorkerProcess>>>;

/// The [`WorkerSource`] of a job that owns its worker's process (design 8.2.5, ADR-394): it tells
/// whether the process has exited, ends one that does not answer, and starts a new worker by the
/// launch recipe when the current one is gone, so the run can open its link on it.
///
/// The process stays in the [`WorkerSlot`], which the caller keeps a clone of ([`Self::slot`]) and
/// stops the process of when the run is over: the slot holds the current worker, a new one
/// replacing a gone one. The source blocks on the runtime for a launch, so the run calls it from
/// its blocking thread, as it calls the host.
pub struct RelaunchingWorker {
    recipe: LaunchRecipe,
    handle: tokio::runtime::Handle,
    slot: WorkerSlot,
    client: WorkerClient,
    /// The current process's ID, which the process no longer gives once it was reaped.
    pid: Option<u32>,
    /// The run found the process exited, or ended it ([`WorkerSource::status`],
    /// [`WorkerSource::end`]), and no worker was started since: the next client is a new worker's.
    /// A process the run was never told about is not started again, so the log tells every exit.
    gone: bool,
}

/// How long [`WorkerSource::end`] waits for a worker it killed to be reaped.
const END_GRACE: Duration = Duration::from_secs(5);

impl RelaunchingWorker {
    /// Takes `process` started by `recipe`, with its `client`. Must be called inside the runtime
    /// the job runs on.
    pub fn new(recipe: LaunchRecipe, process: WorkerProcess, client: WorkerClient) -> Self {
        Self {
            recipe,
            handle: tokio::runtime::Handle::current(),
            pid: process.id(),
            slot: Arc::new(Mutex::new(Some(process))),
            client,
            gone: false,
        }
    }

    /// The slot with the current worker's process.
    pub fn slot(&self) -> WorkerSlot {
        Arc::clone(&self.slot)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<WorkerProcess>> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl WorkerSource for RelaunchingWorker {
    type Client = WorkerClient;

    fn client(&mut self) -> Result<WorkerClient, HostError> {
        // Only an exit the run was told about is followed by a new worker. Until then the client
        // is the current worker's, whose calls fail as a lost path if its process is gone; the
        // first client of a run, asked for in async context, never launches.
        if !self.gone {
            return Ok(self.client.clone());
        }
        match self.handle.block_on(self.recipe.launch()) {
            Ok((process, client)) => {
                self.pid = process.id();
                // The gone process is dropped here, already reaped.
                *self.lock() = Some(process);
                self.client = client.clone();
                self.gone = false;
                Ok(client)
            }
            Err(error) => Err(HostError::WorkerStart {
                spawn_failed: matches!(error, LaunchError::Service(ServiceError::Spawn(_))),
                reason: error.to_string(),
            }),
        }
    }

    fn status(&mut self) -> WorkerStatus {
        let pid = self.pid;
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        let Some(process) = slot.as_mut() else {
            return WorkerStatus::Unknown;
        };
        match process.try_wait() {
            Ok(None) => WorkerStatus::Alive,
            Ok(Some(status)) => {
                self.gone = true;
                WorkerStatus::Exited(WorkerExit {
                    pid,
                    how: ExitHow::of(status),
                })
            }
            Err(_) => WorkerStatus::Unknown,
        }
    }

    fn end(&mut self) -> Result<WorkerExit, HostError> {
        let pid = self.pid;
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        let Some(process) = slot.as_mut() else {
            return Err(HostError::Setup("the source holds no worker process"));
        };
        // A worker that exited on its own is reported with its own end.
        if let Ok(Some(status)) = process.try_wait() {
            self.gone = true;
            return Ok(WorkerExit {
                pid,
                how: ExitHow::of(status),
            });
        }
        let killed = self
            .handle
            .block_on(tokio::time::timeout(END_GRACE, process.kill()));
        if !matches!(killed, Ok(Ok(()))) {
            return Err(HostError::Setup("the worker could not be ended"));
        }
        self.gone = true;
        Ok(WorkerExit {
            pid,
            how: ExitHow::KilledByAgent,
        })
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }
}

/// The width a J2534 worker for `library` is launched with: `requested` (the caller's override),
/// else the `LongSize` of the registration definition the lookup resolved, else the ABI's default.
fn worker_long_size(
    vci: &str,
    abi: Abi,
    requested: Option<u8>,
    library: &Library,
) -> Result<u8, LaunchError> {
    long_size(vci, abi, requested.or(library.long_size))
}

/// The width to launch `abi`'s worker with: `declared`, else the ABI's default (7.1.2). A
/// declared 8 is refused where `unsigned long` is 32 bits wide.
fn long_size(vci: &str, abi: Abi, declared: Option<u8>) -> Result<u8, LaunchError> {
    match declared {
        None => Ok(abi.default_long_size()),
        Some(4) => Ok(4),
        Some(long_size) if long_size == abi.default_long_size() => Ok(long_size),
        Some(long_size) => Err(LaunchError::UnsupportedLongSize {
            vci: vci.to_owned(),
            abi,
            long_size,
        }),
    }
}

/// What one J2534 worker build would load for `vci`.
fn lookup_j2534(vci: &str, resolution: &Resolution) -> Result<Library, j2534_0404::ResolveError> {
    #[cfg(windows)]
    let resolved = j2534_0404::resolve_on_registry(vci, resolution.view)?;
    #[cfg(not(windows))]
    let resolved = {
        let _ = resolution;
        j2534_0404::resolve(vci)?
    };
    Ok(Library {
        path: resolved.definition.library,
        long_size: resolved.definition.long_size,
    })
}

/// What one D-PDU API worker build would load for `vci`: the library of the root description
/// file's entry named `vci`, the root file being the one the build's registry view names.
fn lookup_dpdu(vci: &str, resolution: &Resolution) -> Result<Library, iso22900::ResolveError> {
    let resolved = iso22900::resolve_in_view(vci, registry_view(resolution))?;
    Ok(Library {
        path: resolved.implementation.library_file,
        long_size: None,
    })
}

/// The D-PDU API resolver's name for the registry view `resolution` reads. Outside Windows the
/// view is ignored.
fn registry_view(resolution: &Resolution) -> RegistryView {
    #[cfg(all(windows, target_arch = "x86_64"))]
    match resolution.view {
        RegistryViewMode::W64 => return RegistryView::Wow64_64,
        RegistryViewMode::W32 => return RegistryView::Wow64_32,
        _ => {}
    }
    let _ = resolution;
    RegistryView::native()
}

/// The first resolution whose library is built for that resolution's ABI, with the library and
/// its ABI.
fn resolve<E: fmt::Display>(
    vci: &str,
    resolutions: &[Resolution],
    lookup: impl Fn(&str, &Resolution) -> Result<Library, E>,
    detect: impl Fn(&Path) -> Result<Abi, AbiError>,
) -> Result<(Library, Abi), LaunchError> {
    let mut reasons = Vec::new();
    for resolution in resolutions {
        let build = resolution.abi.map_or("worker", Abi::name);
        let library = match lookup(vci, resolution) {
            Ok(library) => library,
            Err(error) => {
                reasons.push(format!("{build}: {error}"));
                continue;
            }
        };
        match detect(&library.path) {
            Ok(abi) if resolution.abi.is_none_or(|expected| expected == abi) => {
                return Ok((library, abi));
            }
            Ok(abi) => reasons.push(format!(
                "{build}: {} is built for {}",
                library.path.display(),
                abi.name()
            )),
            Err(error) => reasons.push(format!("{build}: {}: {error}", library.path.display())),
        }
    }
    Err(LaunchError::Resolve {
        vci: vci.to_owned(),
        reasons,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const X64: Resolution = Resolution {
        view: RegistryViewMode::Native,
        abi: Some(Abi::WinX64),
    };
    const X86: Resolution = Resolution {
        view: RegistryViewMode::Native,
        abi: Some(Abi::WinX86),
    };
    const LINUX_X64_HOST: &[Abi] = &[Abi::LinuxX86_64, Abi::LinuxX86];

    /// `x64.dll` and `x86.dll` are built for their names; anything else is unreadable.
    fn detect(path: &Path) -> Result<Abi, AbiError> {
        match path.to_str() {
            Some("x64.dll") => Ok(Abi::WinX64),
            Some("x86.dll") => Ok(Abi::WinX86),
            _ => Err(AbiError::UnknownFormat),
        }
    }

    fn library(path: &str) -> Library {
        Library {
            path: PathBuf::from(path),
            long_size: None,
        }
    }

    /// Resolves to the path listed for the build's ABI, or not at all.
    fn lookup_in(
        paths: &'static [(Option<Abi>, &'static str)],
    ) -> impl Fn(&str, &Resolution) -> Result<Library, j2534_0404::ResolveError> {
        move |vci, resolution| {
            paths
                .iter()
                .find(|(abi, _)| *abi == resolution.abi)
                .map(|(_, path)| library(path))
                .ok_or_else(|| j2534_0404::ResolveError::NotFound {
                    name: vci.to_owned(),
                    skipped_invalid: 0,
                })
        }
    }

    /// Each build must read what that worker build reads: for the x86 build, the 32-bit view a
    /// WOW64 process sees as its own.
    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn windows_x64_agent_reads_both_builds_explicitly() {
        let table: Vec<_> = RESOLUTIONS.iter().map(|r| (r.view, r.abi)).collect();
        assert_eq!(
            table,
            [
                (RegistryViewMode::W64, Some(Abi::WinX64)),
                (RegistryViewMode::W32, Some(Abi::WinX86)),
            ]
        );
        // The D-PDU API lookup reads the same view for each build.
        let views: Vec<_> = RESOLUTIONS.iter().map(registry_view).collect();
        assert_eq!(views, [RegistryView::Wow64_64, RegistryView::Wow64_32]);
    }

    #[test]
    fn takes_the_build_that_finds_a_library_of_its_own_abi() {
        let found = resolve(
            "v",
            &[X64, X86],
            lookup_in(&[(Some(Abi::WinX86), "x86.dll")]),
            detect,
        );
        assert_eq!(found.unwrap(), (library("x86.dll"), Abi::WinX86));
    }

    #[test]
    fn prefers_the_first_build_when_both_match() {
        let paths = &[
            (Some(Abi::WinX86), "x86.dll"),
            (Some(Abi::WinX64), "x64.dll"),
        ];
        let found = resolve("v", &[X64, X86], lookup_in(paths), detect);
        assert_eq!(found.unwrap(), (library("x64.dll"), Abi::WinX64));
    }

    #[test]
    fn skips_a_build_whose_library_has_another_abi() {
        // Both builds would load the 32-bit library: only the x86 build can.
        let paths = &[
            (Some(Abi::WinX86), "x86.dll"),
            (Some(Abi::WinX64), "x86.dll"),
        ];
        let found = resolve("v", &[X64, X86], lookup_in(paths), detect);
        assert_eq!(found.unwrap(), (library("x86.dll"), Abi::WinX86));
    }

    #[test]
    fn any_abi_matches_a_build_without_one() {
        let any = Resolution { abi: None, ..X86 };
        let found = resolve("v", &[any], lookup_in(&[(None, "x64.dll")]), detect);
        assert_eq!(found.unwrap(), (library("x64.dll"), Abi::WinX64));
    }

    #[test]
    fn a_named_abi_keeps_only_its_own_build() {
        let both = [X64, X86];
        let only = |abi| {
            restrict(&both, abi, &[])
                .iter()
                .map(|r| r.abi)
                .collect::<Vec<_>>()
        };
        assert_eq!(only(Abi::WinX86), [Some(Abi::WinX86)]);
        assert_eq!(only(Abi::WinX64), [Some(Abi::WinX64)]);
        // A Windows host has no Linux build, and a 32-bit one no x64 build.
        assert!(only(Abi::LinuxX86_64).is_empty());
        assert!(restrict(&[X86], Abi::WinX64, &[]).is_empty());
    }

    #[test]
    fn a_named_abi_reaches_the_x86_build_though_the_x64_one_would_win() {
        let paths = &[
            (Some(Abi::WinX86), "x86.dll"),
            (Some(Abi::WinX64), "x64.dll"),
        ];
        let both = [X64, X86];
        let picked = |abi| {
            resolve("v", &restrict(&both, abi, &[]), lookup_in(paths), detect).map(|(_, abi)| abi)
        };
        assert_eq!(picked(Abi::WinX86).unwrap(), Abi::WinX86);
        assert_eq!(picked(Abi::WinX64).unwrap(), Abi::WinX64);
    }

    #[test]
    fn a_named_abi_without_a_library_of_it_does_not_fall_back() {
        let both = [X64, X86];
        let error = resolve(
            "v",
            &restrict(&both, Abi::WinX86, &[]),
            lookup_in(&[(Some(Abi::WinX64), "x64.dll")]),
            detect,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("win-x86"), "{error}");
        assert!(!error.contains("x64.dll"), "{error}");
    }

    #[test]
    fn on_a_host_with_one_build_the_named_abi_must_match_the_library() {
        let any = Resolution { abi: None, ..X86 };
        // An x86_64 Linux host: a Windows ABI is not runnable.
        assert!(restrict(&[any], Abi::WinX64, LINUX_X64_HOST).is_empty());
        let kept = restrict(&[any], Abi::LinuxX86_64, LINUX_X64_HOST);
        assert_eq!(kept.len(), 1);
        // The library is the win-x64 one, which is not the named ABI: refused.
        let paths = &[(Some(Abi::LinuxX86_64), "x64.dll")];
        let error = resolve("v", &kept, lookup_in(paths), detect).unwrap_err();
        assert!(error.to_string().contains("built for win-x64"), "{error}");
        // The named ABI equal to the detected one is launched.
        let kept = restrict(&[any], Abi::WinX64, &[Abi::WinX64]);
        let paths = &[(Some(Abi::WinX64), "x64.dll")];
        assert_eq!(
            resolve("v", &kept, lookup_in(paths), detect).unwrap().1,
            Abi::WinX64
        );
    }

    #[test]
    fn a_resolution_without_a_build_serves_the_host_table_only() {
        let any = Resolution { abi: None, ..X86 };
        let kept = |served: &[Abi], abi| restrict(&[any], abi, served).len();
        // 7.3: an x86_64 Linux host bundles x64 and x86, an aarch64 one arm64 and armhf.
        assert_eq!(kept(LINUX_X64_HOST, Abi::LinuxX86), 1);
        assert_eq!(kept(LINUX_X64_HOST, Abi::LinuxArm64), 0);
        let arm = [Abi::LinuxArm64, Abi::LinuxArmhf];
        assert_eq!(kept(&arm, Abi::LinuxArmhf), 1);
        assert_eq!(kept(&arm, Abi::LinuxX86_64), 0);
        // A host with no Linux table (macOS) serves none.
        assert_eq!(kept(&[], Abi::LinuxX86_64), 0);
    }

    #[test]
    fn this_agent_build_runs_the_abis_of_its_host_table() {
        let runs: Vec<Abi> = Abi::ALL.into_iter().filter(|&a| host_runs(a)).collect();
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(runs, [Abi::LinuxX86, Abi::LinuxX86_64]);
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            assert_eq!(runs, [Abi::LinuxArm64, Abi::LinuxArmhf]);
        } else if cfg!(all(windows, target_arch = "x86_64")) {
            assert_eq!(runs, [Abi::WinX86, Abi::WinX64]);
        }
    }

    /// The x86 build of a 64-bit Windows agent is reachable by name, and reads the 32-bit view.
    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn windows_x64_agent_restricts_to_the_x86_build_and_its_view() {
        let kept = restrict(RESOLUTIONS, Abi::WinX86, HOST_ABIS);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].view, RegistryViewMode::W32);
        assert_eq!(kept[0].abi, Some(Abi::WinX86));
    }

    #[test]
    fn a_declared_long_size_overrides_the_abi_default() {
        assert_eq!(long_size("v", Abi::LinuxX86_64, None).unwrap(), 8);
        assert_eq!(long_size("v", Abi::LinuxX86_64, Some(4)).unwrap(), 4);
        assert_eq!(long_size("v", Abi::LinuxX86_64, Some(8)).unwrap(), 8);
        assert_eq!(long_size("v", Abi::LinuxArmhf, None).unwrap(), 4);
        assert_eq!(long_size("v", Abi::LinuxArmhf, Some(4)).unwrap(), 4);
    }

    #[test]
    fn the_worker_width_is_the_override_then_the_definitions_then_the_abi_default() {
        let library = |long_size| Library {
            path: PathBuf::from("lib"),
            long_size,
        };
        let width = |abi, requested, declared| {
            worker_long_size("v", abi, requested, &library(declared)).unwrap()
        };
        // The definition's LongSize, else the ABI's default.
        assert_eq!(width(Abi::LinuxX86_64, None, Some(4)), 4);
        assert_eq!(width(Abi::LinuxX86_64, None, Some(8)), 8);
        assert_eq!(width(Abi::LinuxX86_64, None, None), 8);
        assert_eq!(width(Abi::LinuxArmhf, None, None), 4);
        // An override of the caller wins over the definition.
        assert_eq!(width(Abi::LinuxX86_64, Some(8), Some(4)), 8);
        // A width the ABI cannot have is refused, from either source.
        assert!(worker_long_size("v", Abi::LinuxX86, None, &library(Some(8))).is_err());
        assert!(worker_long_size("v", Abi::LinuxX86, Some(8), &library(None)).is_err());
    }

    #[test]
    fn eight_bytes_is_refused_where_unsigned_long_is_32_bits() {
        for abi in [Abi::LinuxX86, Abi::LinuxArmhf, Abi::WinX86, Abi::WinX64] {
            let error = long_size("v", abi, Some(8)).unwrap_err();
            assert!(
                matches!(error, LaunchError::UnsupportedLongSize { long_size: 8, .. }),
                "{abi:?}: {error}"
            );
        }
    }

    #[test]
    fn reports_every_build_when_none_matches() {
        let paths = &[
            (Some(Abi::WinX86), "x64.dll"),
            (Some(Abi::WinX64), "bad.dll"),
        ];
        let error = resolve("v", &[X64, X86], lookup_in(paths), detect)
            .unwrap_err()
            .to_string();
        assert!(error.contains("win-x64: bad.dll"), "{error}");
        assert!(
            error.contains("win-x86: x64.dll is built for win-x64"),
            "{error}"
        );
    }
}
