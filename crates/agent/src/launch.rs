//! Launching the worker for a VCI (design 7.3, 7.4).
//!
//! The library behind a VCI name is resolved for each worker build through `ngr-library-resolver`
//! (7.1.1), as that build resolves it: the build's registry view on Windows, the registration
//! definition on Linux, and in a debug build the `NGR_J2534_DEFINITION_DIR` definition directory
//! on both (ADR-282). The file's header picks the ABI, so the file whose header selects the build
//! is the file that build loads (ADR-281 Decision item 1). The service is then started with the
//! name and the device's operating mode (ADR-281 Decision item 2; 7.2 gives the mode its meaning
//! for a failed library check) and resolves the name again itself. ADR-240 records the selection
//! rule.

use std::path::{Path, PathBuf};

use j2534_0404_registry::RegistryViewMode;
use ngr_library_resolver::j2534_0404::ResolveError;
use worker_host::abi::{self, Abi, AbiError};
use worker_host::client::{ConnectError, ConnectOptions, WorkerClient};
use worker_host::service::{
    LaunchOptions, OperatingMode, ServiceError, ServiceKind, WorkerLayout, WorkerProcess,
};

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("cannot resolve the library of VCI {vci:?}: {}", reasons.join("; "))]
    Resolve { vci: String, reasons: Vec<String> },
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Connect(#[from] ConnectError),
}

/// How one worker build resolves a library name: the registry view it reads on Windows, and the
/// ABI it is built for (`None`: any, there is one build per host on Linux).
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

// No j2534-0404 worker is built for any other Windows architecture.
#[cfg(all(windows, not(any(target_arch = "x86_64", target_arch = "x86"))))]
compile_error!("no j2534-0404 worker build is defined for this Windows architecture");

#[cfg(not(windows))]
const RESOLUTIONS: &[Resolution] = &[Resolution {
    view: RegistryViewMode::Native,
    abi: None,
}];

/// A running worker and a client connected to it.
pub struct LaunchedWorker {
    pub abi: Abi,
    pub process: WorkerProcess,
    pub client: WorkerClient,
}

/// Launches the j2534-0404 worker that matches the ABI of `vci`'s library in the device's
/// operating `mode` and connects a client to it.
///
/// `options.long_size` is filled with the ABI's default (7.1.2) when it is `None`; the `LongSize`
/// of the registration definition the lookup resolves (7.1.1) is not used.
pub async fn launch_j2534_worker(
    vci: &str,
    mode: OperatingMode,
    workers: &WorkerLayout,
    options: &LaunchOptions,
) -> Result<LaunchedWorker, LaunchError> {
    let (_, abi) = resolve(vci, RESOLUTIONS, lookup, abi::detect_file)?;
    let binary = workers.find(ServiceKind::J2534V0404, abi)?;
    let options = LaunchOptions {
        long_size: options.long_size.or(Some(abi.default_long_size())),
        ..options.clone()
    };
    let process =
        WorkerProcess::launch(&binary, ServiceKind::J2534V0404, vci, mode, &options).await?;
    let client = process.connect(&ConnectOptions::default()).await?;
    Ok(LaunchedWorker {
        abi,
        process,
        client,
    })
}

/// What one worker build would load for `vci`.
fn lookup(vci: &str, resolution: &Resolution) -> Result<PathBuf, ResolveError> {
    #[cfg(windows)]
    let resolved = ngr_library_resolver::j2534_0404::resolve_on_registry(vci, resolution.view)?;
    #[cfg(not(windows))]
    let resolved = {
        let _ = resolution;
        ngr_library_resolver::j2534_0404::resolve(vci)?
    };
    Ok(resolved.definition.library)
}

/// The first resolution whose library is built for that resolution's ABI, with the library's
/// path and ABI.
fn resolve(
    vci: &str,
    resolutions: &[Resolution],
    lookup: impl Fn(&str, &Resolution) -> Result<PathBuf, ResolveError>,
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
        view: RegistryViewMode::Native,
        abi: Some(Abi::WinX64),
    };
    const X86: Resolution = Resolution {
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

    /// Resolves to the path listed for the build's ABI, or not at all.
    fn lookup_in(
        paths: &'static [(Option<Abi>, &'static str)],
    ) -> impl Fn(&str, &Resolution) -> Result<PathBuf, ResolveError> {
        move |vci, resolution| {
            paths
                .iter()
                .find(|(abi, _)| *abi == resolution.abi)
                .map(|(_, path)| PathBuf::from(path))
                .ok_or_else(|| ResolveError::NotFound {
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
    }

    #[test]
    fn takes_the_build_that_finds_a_library_of_its_own_abi() {
        let found = resolve(
            "v",
            &[X64, X86],
            lookup_in(&[(Some(Abi::WinX86), "x86.dll")]),
            detect,
        );
        assert_eq!(found.unwrap(), (PathBuf::from("x86.dll"), Abi::WinX86));
    }

    #[test]
    fn prefers_the_first_build_when_both_match() {
        let paths = &[
            (Some(Abi::WinX86), "x86.dll"),
            (Some(Abi::WinX64), "x64.dll"),
        ];
        let found = resolve("v", &[X64, X86], lookup_in(paths), detect);
        assert_eq!(found.unwrap(), (PathBuf::from("x64.dll"), Abi::WinX64));
    }

    #[test]
    fn skips_a_build_whose_library_has_another_abi() {
        // Both builds would load the 32-bit library: only the x86 build can.
        let paths = &[
            (Some(Abi::WinX86), "x86.dll"),
            (Some(Abi::WinX64), "x86.dll"),
        ];
        let found = resolve("v", &[X64, X86], lookup_in(paths), detect);
        assert_eq!(found.unwrap(), (PathBuf::from("x86.dll"), Abi::WinX86));
    }

    #[test]
    fn any_abi_matches_a_build_without_one() {
        let any = Resolution { abi: None, ..X86 };
        let found = resolve("v", &[any], lookup_in(&[(None, "x64.dll")]), detect);
        assert_eq!(found.unwrap(), (PathBuf::from("x64.dll"), Abi::WinX64));
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
