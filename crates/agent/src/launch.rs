//! Launching the worker for a VCI (design 7.3, 7.4).
//!
//! The library behind a VCI name is resolved the way each worker build resolves it on its side
//! (`j2534_0404_registry`: a `library_path` entry in `config.toml` under the build's
//! architecture key or at api level, else the build's registry view), so the file whose header
//! picks the ABI is the file that build loads. The service is then started with the name only
//! and resolves it again itself (7.2). ADR-240 records the selection rule.

use std::path::{Path, PathBuf};

use j2534_0404_registry::{RegistryError, RegistryViewMode};
use worker_host::abi::{self, Abi, AbiError};
use worker_host::client::{ConnectError, ConnectOptions, WorkerClient};
use worker_host::service::{LaunchOptions, ServiceError, ServiceKind, WorkerLayout, WorkerProcess};

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("cannot resolve the library of VCI {vci:?}: {}", reasons.join("; "))]
    Resolve { vci: String, reasons: Vec<String> },
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Connect(#[from] ConnectError),
}

/// How one worker build resolves a library name: the `config.toml` architecture key it passes
/// (`vci_service_launcher::vci_server::current_arch`), the registry view it reads, and the ABI
/// it is built for (`None`: any, there is one build per host on Linux).
#[derive(Debug, Clone, Copy)]
struct Resolution {
    arch: Option<&'static str>,
    view: RegistryViewMode,
    abi: Option<Abi>,
}

/// The worker builds the agent can choose from, preferred first. A 64-bit Windows agent reads
/// both registry views explicitly (7.1) and prefers the native x64 build when a VCI has a
/// library in both.
#[cfg(all(windows, target_arch = "x86_64"))]
const RESOLUTIONS: &[Resolution] = &[
    Resolution {
        arch: Some("x86_x64"),
        view: RegistryViewMode::W64,
        abi: Some(Abi::WinX64),
    },
    Resolution {
        arch: Some("x86"),
        view: RegistryViewMode::W32,
        abi: Some(Abi::WinX86),
    },
];

#[cfg(all(windows, target_arch = "x86"))]
const RESOLUTIONS: &[Resolution] = &[Resolution {
    arch: Some("x86"),
    view: RegistryViewMode::Native,
    abi: Some(Abi::WinX86),
}];

// `vci_service_launcher::vci_server::current_arch` knows no other Windows architecture.
#[cfg(all(windows, not(any(target_arch = "x86_64", target_arch = "x86"))))]
compile_error!("no j2534-0404 worker build is defined for this Windows architecture");

#[cfg(not(windows))]
const RESOLUTIONS: &[Resolution] = &[Resolution {
    arch: None,
    view: RegistryViewMode::Native,
    abi: None,
}];

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
/// registration definition that could override it (7.1.1) is not read.
pub async fn launch_j2534_worker(
    vci: &str,
    workers: &WorkerLayout,
    options: &LaunchOptions,
) -> Result<LaunchedWorker, LaunchError> {
    let (_, abi) = resolve(vci, RESOLUTIONS, lookup, abi::detect_file)?;
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

/// What one worker build would load for `vci`.
fn lookup(vci: &str, resolution: &Resolution) -> Result<PathBuf, RegistryError> {
    if let Some(path) = j2534_0404_registry::find_library_path(resolution.arch, vci) {
        return Ok(path);
    }
    Ok(j2534_0404_registry::find_j2534_device_on_registry(vci, resolution.view)?.library_path)
}

/// The first resolution whose library is built for that resolution's ABI, with the library's
/// path and ABI.
fn resolve(
    vci: &str,
    resolutions: &[Resolution],
    lookup: impl Fn(&str, &Resolution) -> Result<PathBuf, RegistryError>,
    detect: impl Fn(&Path) -> Result<Abi, AbiError>,
) -> Result<(PathBuf, Abi), LaunchError> {
    let mut reasons = Vec::new();
    for resolution in resolutions {
        let build = resolution.abi.map_or("worker", Abi::name);
        let path = match lookup(vci, resolution) {
            Ok(path) => path,
            Err(error) => {
                reasons.push(format!("{build}: {error}"));
                continue;
            }
        };
        match detect(&path) {
            Ok(abi) if resolution.abi.is_none_or(|expected| expected == abi) => {
                return Ok((path, abi));
            }
            Ok(abi) => reasons.push(format!(
                "{build}: {} is built for {}",
                path.display(),
                abi.name()
            )),
            Err(error) => reasons.push(format!("{build}: {}: {error}", path.display())),
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
        arch: Some("x86_x64"),
        view: RegistryViewMode::Native,
        abi: Some(Abi::WinX64),
    };
    const X86: Resolution = Resolution {
        arch: Some("x86"),
        view: RegistryViewMode::Native,
        abi: Some(Abi::WinX86),
    };

    /// `x64.dll` and `x86.dll` are built for their names; anything else is unreadable.
    fn detect(path: &Path) -> Result<Abi, AbiError> {
        match path.to_str() {
            Some("x64.dll") => Ok(Abi::WinX64),
            Some("x86.dll") => Ok(Abi::WinX86),
            _ => Err(AbiError::UnknownFormat),
        }
    }

    /// Resolves to `paths[arch]`, or not at all.
    fn lookup_in(
        paths: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str, &Resolution) -> Result<PathBuf, RegistryError> {
        move |vci, resolution| {
            paths
                .iter()
                .find(|(arch, _)| Some(*arch) == resolution.arch)
                .map(|(_, path)| PathBuf::from(path))
                .ok_or_else(|| RegistryError::NotFound(vci.to_owned()))
        }
    }

    /// Each build must read what that worker build reads: its `current_arch` key and, for the
    /// x86 build, the 32-bit view a WOW64 process sees as its own.
    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn windows_x64_agent_reads_both_builds_explicitly() {
        let table: Vec<_> = RESOLUTIONS
            .iter()
            .map(|r| (r.arch, r.view, r.abi))
            .collect();
        assert_eq!(
            table,
            [
                (Some("x86_x64"), RegistryViewMode::W64, Some(Abi::WinX64)),
                (Some("x86"), RegistryViewMode::W32, Some(Abi::WinX86)),
            ]
        );
    }

    #[test]
    fn takes_the_build_that_finds_a_library_of_its_own_abi() {
        let found = resolve("v", &[X64, X86], lookup_in(&[("x86", "x86.dll")]), detect);
        assert_eq!(found.unwrap(), (PathBuf::from("x86.dll"), Abi::WinX86));
    }

    #[test]
    fn prefers_the_first_build_when_both_match() {
        let paths = &[("x86", "x86.dll"), ("x86_x64", "x64.dll")];
        let found = resolve("v", &[X64, X86], lookup_in(paths), detect);
        assert_eq!(found.unwrap(), (PathBuf::from("x64.dll"), Abi::WinX64));
    }

    #[test]
    fn skips_a_build_whose_library_has_another_abi() {
        // Both builds would load the 32-bit library: only the x86 build can.
        let paths = &[("x86", "x86.dll"), ("x86_x64", "x86.dll")];
        let found = resolve("v", &[X64, X86], lookup_in(paths), detect);
        assert_eq!(found.unwrap(), (PathBuf::from("x86.dll"), Abi::WinX86));
    }

    #[test]
    fn any_abi_matches_a_build_without_one() {
        let any = Resolution { abi: None, ..X86 };
        let found = resolve("v", &[any], lookup_in(&[("x86", "x64.dll")]), detect);
        assert_eq!(found.unwrap(), (PathBuf::from("x64.dll"), Abi::WinX64));
    }

    #[test]
    fn reports_every_build_when_none_matches() {
        let paths = &[("x86", "x64.dll"), ("x86_x64", "bad.dll")];
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
