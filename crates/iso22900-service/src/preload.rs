//! The design 7.2 pre-load checks on the resolved library, and what the operating mode makes of
//! a failed one (ADR-281 Decision item 2).

use std::fs::File;

use ngr_library_resolver::{Policy, Signer, iso22900::Resolved};
use tonic::Status;
use tracing::{error, info, warn};
use vci_service_launcher::mode::OperatingMode;

/// What the checks keep open until the library is mapped, so the file they checked is the file
/// that is loaded (ADR-278).
#[derive(Debug)]
pub struct CheckedLibrary {
    _signer: Option<Signer>,
    _held: Option<File>,
}

/// Runs the writability and signer checks on `resolved`'s library and the files that named it.
///
/// A library that passes is reported with its signer. One that fails is refused in device mode,
/// with a client message that names no path (ADR-089) and the findings in the log; in user mode
/// the findings are logged as a warning and the library is loaded.
pub fn check_library(resolved: &Resolved, mode: OperatingMode) -> Result<CheckedLibrary, Status> {
    let library = &resolved.implementation.library_file;
    let naming_files = resolved.naming_files();
    match ngr_library_resolver::check_library(library, &naming_files, &Policy::system()) {
        Ok(signer) => {
            match &signer {
                Signer::Trusted {
                    subject,
                    thumbprint_sha256,
                    ..
                } => info!(
                    library = %library.display(),
                    signer = %subject,
                    thumbprint_sha256 = %thumbprint_sha256,
                    "library passed the pre-load checks"
                ),
                Signer::Unsigned { .. } | Signer::NotApplicable => info!(
                    library = %library.display(),
                    "library passed the pre-load checks"
                ),
            }
            Ok(CheckedLibrary {
                _signer: Some(signer),
                _held: None,
            })
        }
        Err(failed) => match mode {
            OperatingMode::Device => {
                error!(
                    library = %library.display(),
                    findings = %failed,
                    "refusing the library in device mode: it failed the pre-load checks"
                );
                Err(Status::failed_precondition(
                    "the D-PDU API library failed the pre-load checks and is not loaded in device mode",
                ))
            }
            OperatingMode::User => {
                warn!(
                    library = %library.display(),
                    findings = %failed,
                    "loading the library in user mode although it failed the pre-load checks"
                );
                Ok(CheckedLibrary {
                    _signer: failed.signer,
                    _held: failed.held,
                })
            }
        },
    }
}
