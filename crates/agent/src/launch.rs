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

use j2534_0404_registry::RegistryViewMode;
use ngr_library_resolver::iso22900::mdf::ErrorCodes;
use ngr_library_resolver::{RegistryView, iso22900, j2534_0404};
use vci_service_config::External;
use worker_host::abi::{self, Abi, AbiError};
use worker_host::client::{ConnectError, ConnectOptions, WorkerClient};
use worker_host::service::{
    LaunchOptions, OperatingMode, ServiceError, ServiceKind, WorkerLayout, WorkerProcess,
};

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

/// What one worker build would load for a VCI: the library, the `LongSize` its J2534
/// registration definition declares (7.1.1; a registry entry and a D-PDU API implementation
/// declare none), and the module description file a D-PDU API implementation names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Library {
    path: PathBuf,
    long_size: Option<u8>,
    module_description_file: Option<PathBuf>,
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
    /// The names of the extra error information the worker's error events carry: the error
    /// codes of the MDF the D-PDU API implementation names (ISO 22900-2:2022 Annex F, F.4).
    /// Empty for a J2534 worker, whose events carry none, and when the entry names no MDF or
    /// the MDF cannot be read, which is logged and does not fail the launch.
    pub error_codes: ErrorCodes,
    pub process: WorkerProcess,
    pub client: WorkerClient,
}

/// Launches the worker of `api` that matches the ABI of `vci`'s library in the device's
/// operating `mode` and connects a client to it.
///
/// A J2534 worker's `long_size` is `options.long_size`, else the `LongSize` of the registration
/// definition the lookup resolves (7.1.1), else the ABI's default (7.1.2). An 8-byte width for an
/// ABI whose `unsigned long` is 32 bits wide fails the launch. A D-PDU API worker takes no
/// `long_size`.
pub async fn launch_worker(
    api: Api,
    vci: &str,
    mode: OperatingMode,
    workers: &WorkerLayout,
    options: &LaunchOptions,
) -> Result<LaunchedWorker, LaunchError> {
    let (library, abi, long_size, options, error_codes) = match api {
        Api::J2534 => {
            let (library, abi) = resolve(vci, RESOLUTIONS, lookup_j2534, abi::detect_file)?;
            let long_size = worker_long_size(vci, abi, options.long_size, &library)?;
            let options = LaunchOptions {
                long_size: Some(long_size),
                ..options.clone()
            };
            (library.path, abi, long_size, options, ErrorCodes::default())
        }
        Api::Dpdu => {
            let (library, abi) = resolve(vci, RESOLUTIONS, lookup_dpdu, abi::detect_file)?;
            let options = LaunchOptions {
                long_size: None,
                ..options.clone()
            };
            let error_codes = library
                .module_description_file
                .as_deref()
                .map(error_codes)
                .unwrap_or_default();
            (
                library.path,
                abi,
                abi.default_long_size(),
                options,
                error_codes,
            )
        }
    };
    let binary = workers.find(api.service(), abi)?;
    let process = WorkerProcess::launch(&binary, api.service(), vci, mode, &options).await?;
    let client = process.connect(&ConnectOptions::default()).await?;
    Ok(LaunchedWorker {
        abi,
        library,
        long_size,
        error_codes,
        process,
        client,
    })
}

/// The error codes the MDF at `path` names, or none when it cannot be read: the names are for
/// the log's readers, so a launch does not fail for them.
fn error_codes(path: &Path) -> ErrorCodes {
    iso22900::mdf::read_error_codes(path).unwrap_or_else(|error| {
        // The error quotes the vendor's path, which NGR does not write (ADR-348 item 6).
        tracing::warn!(
            external = %External(error),
            "the module description file's error codes cannot be read; extra error \
             information is logged by its code"
        );
        ErrorCodes::default()
    })
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
        module_description_file: None,
    })
}

/// What one D-PDU API worker build would load for `vci`: the library of the root description
/// file's entry named `vci`, the root file being the one the build's registry view names.
fn lookup_dpdu(vci: &str, resolution: &Resolution) -> Result<Library, iso22900::ResolveError> {
    let resolved = iso22900::resolve_in_view(vci, registry_view(resolution))?;
    Ok(Library {
        path: resolved.implementation.library_file,
        long_size: None,
        module_description_file: resolved.implementation.module_description_file,
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
            module_description_file: None,
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
            module_description_file: None,
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
