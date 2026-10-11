//! The signer check of 7.2 (ADR-278): when a Windows library carries an embedded Authenticode
//! signature, the signature must verify against the operating system's trust store.
//!
//! "Carries a signature" means the PE header's security data directory is not empty; catalog
//! signatures are not looked at. Only then is the file handed to `WinVerifyTrust` (generic
//! Authenticode policy, no user interface, revocation checking explicitly off, no network). A failure is one
//! [`Reason::InvalidSignature`] finding; the check takes no operating mode, and the caller decides
//! what the finding means (device mode refuses, user mode warns). On other platforms the check
//! reports [`Signer::NotApplicable`].
//!
//! The check gives no protection against someone who can write the file: they can strip the
//! signature, and the file is then `Unsigned`, which passes. That is why the writability check
//! (ADR-270), not this one, is the gate against planting a library.
//!
//! The mapping from an HRESULT to a [`SignatureFailure`] and the PE header parser are plain
//! functions over data, compiled and tested on every platform; only [`imp`]'s calls into the
//! operating system are Windows-only.
#![cfg_attr(
    all(not(windows), not(test)),
    expect(
        dead_code,
        reason = "only the Windows check uses the parser and the mapping"
    )
)]

use std::{
    fmt,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use crate::writability::{Finding, Policy, Reason, Role, check_writability_with};

/// Why a present signature was not accepted. A coarse class; the raw HRESULT travels with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureFailure {
    /// The file no longer matches the digest in its signature.
    Tampered,
    /// The signer's chain does not end at a root the system trusts, or the certificate may not be
    /// used for code signing, or the signer is explicitly distrusted. Also a revoked certificate,
    /// which the check does not look for itself but a machine policy may still report.
    UntrustedSigner,
    /// A certificate or time stamp is outside its validity (a time-stamped signature outlives the
    /// certificate's expiry; one without a time stamp does not).
    Expired,
    /// The signature structure is unusable.
    Malformed,
    /// Any other result.
    Other,
}

impl fmt::Display for SignatureFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SignatureFailure::Tampered => "file does not match its signature",
            SignatureFailure::UntrustedSigner => "signer is not trusted",
            SignatureFailure::Expired => "certificate or time stamp expired",
            SignatureFailure::Malformed => "malformed signature",
            SignatureFailure::Other => "signature not accepted",
        })
    }
}

// HRESULTs of the trust provider (winerror.h names), kept here as plain numbers so the mapping
// compiles everywhere.
const fn hr(code: u32) -> i32 {
    code as i32
}
/// Reported (E_FAIL, class `Other`) when the provider says yes but the leaf certificate, its
/// name (an empty subject included) or its SHA-256 property cannot be read. The signature
/// verified, so it is not a malformed signature; the identity is simply unavailable.
const NO_SIGNER_DATA: i32 = 0x8000_4005_u32 as i32;
const TRUST_E_BAD_DIGEST: i32 = hr(0x8009_6010);
const CERT_E_UNTRUSTEDROOT: i32 = hr(0x800B_0109);
const CERT_E_UNTRUSTEDTESTROOT: i32 = hr(0x800B_010D);
const CERT_E_UNTRUSTEDCA: i32 = hr(0x800B_0112);
const CERT_E_CHAINING: i32 = hr(0x800B_010A);
const CERT_E_PURPOSE: i32 = hr(0x800B_0106);
const CERT_E_WRONG_USAGE: i32 = hr(0x800B_0110);
const CERT_E_REVOKED: i32 = hr(0x800B_010C);
const CERT_E_REVOCATION_FAILURE: i32 = hr(0x800B_010E);
const TRUST_E_EXPLICIT_DISTRUST: i32 = hr(0x800B_0111);
const TRUST_E_SUBJECT_NOT_TRUSTED: i32 = hr(0x800B_0004);
const CERT_E_EXPIRED: i32 = hr(0x800B_0101);
const TRUST_E_TIME_STAMP: i32 = hr(0x8009_6005);
const TRUST_E_NOSIGNATURE: i32 = hr(0x800B_0100);
const TRUST_E_SUBJECT_FORM_UNKNOWN: i32 = hr(0x800B_0003);
const TRUST_E_MALFORMED_SIGNATURE: i32 = hr(0x8009_6011);
const TRUST_E_CERT_SIGNATURE: i32 = hr(0x8009_6004);
const TRUST_E_NO_SIGNER_CERT: i32 = hr(0x8009_6002);

/// Classifies a non-success result of the trust provider.
fn classify(hresult: i32) -> SignatureFailure {
    match hresult {
        TRUST_E_BAD_DIGEST => SignatureFailure::Tampered,
        CERT_E_UNTRUSTEDROOT
        | CERT_E_UNTRUSTEDTESTROOT
        | CERT_E_UNTRUSTEDCA
        | CERT_E_CHAINING
        | CERT_E_PURPOSE
        | CERT_E_WRONG_USAGE
        | CERT_E_REVOKED
        | CERT_E_REVOCATION_FAILURE
        | TRUST_E_EXPLICIT_DISTRUST
        | TRUST_E_SUBJECT_NOT_TRUSTED => SignatureFailure::UntrustedSigner,
        CERT_E_EXPIRED | TRUST_E_TIME_STAMP => SignatureFailure::Expired,
        TRUST_E_NOSIGNATURE
        | TRUST_E_SUBJECT_FORM_UNKNOWN
        | TRUST_E_MALFORMED_SIGNATURE
        | TRUST_E_CERT_SIGNATURE
        | TRUST_E_NO_SIGNER_CERT => SignatureFailure::Malformed,
        _ => SignatureFailure::Other,
    }
}

/// The verdict of [`check_signer`]. A file the check opened is held in the verdict: it was opened
/// with read sharing only, so keeping it open until the library is mapped stops anyone from
/// replacing the file in between (ADR-278 item 7).
#[derive(Debug)]
pub enum Signer {
    /// No signature check on this platform.
    NotApplicable,
    /// The file carries no embedded signature (catalog signatures are not looked at).
    Unsigned {
        /// The open file.
        file: File,
    },
    /// The embedded signature verified against the system trust store.
    Trusted {
        /// The signer certificate's simple display name.
        subject: String,
        /// SHA-256 of the signer certificate's encoded bytes, 64 upper-case hex digits.
        thumbprint_sha256: String,
        /// The open file.
        file: File,
    },
}

impl Signer {
    /// The open file held for the load, if the check opened one.
    pub fn file(&self) -> Option<&File> {
        match self {
            Signer::NotApplicable => None,
            Signer::Unsigned { file } | Signer::Trusted { file, .. } => Some(file),
        }
    }
}

/// Why the signer check gave no verdict, or a failing one.
#[derive(Debug, thiserror::Error)]
pub enum SignerError {
    /// A signature is present and was not accepted.
    #[error("{failure} (HRESULT {hresult:#010X})")]
    Invalid {
        /// The class of the failure.
        failure: SignatureFailure,
        /// The raw result of the trust provider.
        hresult: i32,
        /// The open file, held as in [`Signer`]: a caller that decides to load the library anyway
        /// (user mode) keeps it open until the library is mapped.
        file: File,
    },
    /// The file could not be opened or read.
    #[error("cannot read the library: {0}")]
    Io(#[from] io::Error),
}

/// Checks the embedded signature of `library`, if it has one.
///
/// Windows: opens the file with read sharing only. A file with an empty security data directory
/// (or one that is not a PE image, which cannot be told from unsigned) is [`Signer::Unsigned`];
/// any other is verified, and a non-success result is [`SignerError::Invalid`]. Elsewhere:
/// [`Signer::NotApplicable`].
pub fn check_signer(library: &Path) -> Result<Signer, SignerError> {
    imp::check_signer(library)
}

/// The library failed a pre-load check, or could not be checked.
#[derive(Debug, thiserror::Error)]
#[error("{}{}", crate::writability::list(findings), io_line(io, findings.is_empty()))]
pub struct PreloadError {
    /// Every finding of the writability and signer checks, in that order.
    pub findings: Vec<Finding>,
    /// The signer check could not read the file. Distinct from a finding: nothing is known
    /// about the signature.
    pub io: Option<io::Error>,
    /// The signer verdict, when the check ran without an I/O error and found no invalid
    /// signature (so it is set whenever the findings are writability findings only). It holds the
    /// open file. User mode, which warns about findings and loads anyway, should load with this
    /// verdict kept alive until the library is mapped, so the file it reports on is the file it
    /// loads. Device mode refuses and drops it.
    pub signer: Option<Signer>,
    /// The open file when the signature was invalid (an [`Reason::InvalidSignature`] finding):
    /// there is no [`Signer`] then. The same use as in `signer`.
    pub held: Option<File>,
}

fn io_line(io: &Option<io::Error>, first: bool) -> String {
    match io {
        Some(e) => format!(
            "{}cannot read the library: {e}",
            if first { "" } else { "\n" }
        ),
        None => String::new(),
    }
}

/// The single pre-load entry point of 7.2 (ADR-228 item 4, ADR-278 item 6): runs the writability
/// check ([`check_writability_with`]) and the signer check and returns every finding together.
/// On success the verdict holds the open file (see [`Signer`]).
///
/// A failure does not discard what the signer check learned: the error carries the verdict
/// ([`PreloadError::signer`]) or, for an invalid signature, the open file ([`PreloadError::held`]),
/// so a caller that warns and loads (user mode) loads the file that was checked.
pub fn check_library(
    library: &Path,
    naming_files: &[PathBuf],
    policy: &Policy,
) -> Result<Signer, PreloadError> {
    let mut findings = match check_writability_with(library, naming_files, policy) {
        Ok(()) => Vec::new(),
        Err(e) => e.findings,
    };
    let mut io = None;
    let mut held = None;
    let signer = match check_signer(library) {
        Ok(s) => Some(s),
        Err(SignerError::Invalid {
            failure,
            hresult,
            file,
        }) => {
            findings.push(Finding {
                path: library.to_owned(),
                role: Role::Library,
                reason: Reason::InvalidSignature { failure, hresult },
            });
            held = Some(file);
            None
        }
        Err(SignerError::Io(e)) => {
            io = Some(e);
            None
        }
    };
    match signer {
        Some(s) if findings.is_empty() => Ok(s),
        signer => Err(PreloadError {
            findings,
            io,
            signer,
            held,
        }),
    }
}

/// Why a header cannot be read as a PE image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeError {
    /// No `MZ` or no `PE\0\0` signature.
    NotPe,
    /// `e_lfanew` points further into the file than a real image puts it.
    TooFar,
    /// The header is cut off before a field it needs.
    Truncated,
    /// The optional header is neither PE32 nor PE32+.
    BadMagic,
}

/// Index of the security (certificate table) entry in the data directories.
const SECURITY_DIRECTORY: usize = 4;
const DATA_DIRECTORY_SIZE: usize = 8;

fn bytes(h: &[u8], offset: usize, len: usize) -> Result<&[u8], PeError> {
    offset
        .checked_add(len)
        .and_then(|end| h.get(offset..end))
        .ok_or(PeError::Truncated)
}

fn u16_at(h: &[u8], offset: usize) -> Result<u16, PeError> {
    Ok(u16::from_le_bytes(
        bytes(h, offset, 2)?.try_into().expect("two bytes"),
    ))
}

fn u32_at(h: &[u8], offset: usize) -> Result<u32, PeError> {
    Ok(u32::from_le_bytes(
        bytes(h, offset, 4)?.try_into().expect("four bytes"),
    ))
}

/// Size of the DOS header; `e_lfanew` is its last field (at 0x3C).
const DOS_HEADER_SIZE: usize = 0x40;
/// Largest `e_lfanew` accepted. Real images put the PE headers right behind a small DOS stub;
/// the loader takes any 32-bit value, but a header megabytes into the file is not an image
/// worth treating as signed, and the bound keeps the second read short.
const MAX_PE_OFFSET: u32 = 16 * 1024 * 1024;
/// How much is read at `e_lfanew`: the signature, the COFF header and an optional header with
/// room for all sixteen data directories (PE32+: 112 + 16 * 8 bytes) with a margin.
const NT_WINDOW: u64 = 4 + 20 + 512;

/// The offset of the PE signature from the DOS header at the start of `h`.
fn pe_offset(h: &[u8]) -> Result<u32, PeError> {
    if bytes(h, 0, 2)? != b"MZ" {
        return Err(PeError::NotPe);
    }
    let offset = u32_at(h, 0x3C)?;
    if offset > MAX_PE_OFFSET {
        return Err(PeError::TooFar);
    }
    Ok(offset)
}

/// The size field of the security data directory, from the NT headers: `h` starts at the PE
/// signature. A directory the optional header does not hold (fewer entries than five, or a
/// header too short for the entry) counts as empty, as the loader would see it.
fn security_directory_size_at(h: &[u8]) -> Result<u32, PeError> {
    if bytes(h, 0, 4)? != b"PE\0\0" {
        return Err(PeError::NotPe);
    }
    // COFF header (20 bytes) follows the signature; its SizeOfOptionalHeader is at +16.
    let optional_size = usize::from(u16_at(h, 4 + 16)?);
    let optional = 4 + 20;
    // The data directories start at 96 (PE32) or 112 (PE32+); the entry count precedes them.
    let directories = match u16_at(h, optional)? {
        0x10B => optional + 96,
        0x20B => optional + 112,
        _ => return Err(PeError::BadMagic),
    };
    let count = u32_at(h, directories - 4)? as usize;
    let entry = directories + SECURITY_DIRECTORY * DATA_DIRECTORY_SIZE;
    let within_header = entry + DATA_DIRECTORY_SIZE <= optional + optional_size;
    if count <= SECURITY_DIRECTORY || !within_header {
        return Ok(0);
    }
    u32_at(h, entry + 4)
}

/// The size field of the security data directory of an image held in one slice (the DOS header
/// and the NT headers it points to both inside `h`). Only the tests read whole images this way.
#[cfg(test)]
fn security_directory_size(h: &[u8]) -> Result<u32, PeError> {
    let at = pe_offset(h)? as usize;
    security_directory_size_at(h.get(at..).ok_or(PeError::Truncated)?)
}

/// The size of the security data directory of the image in `r`, 0 when there is none to look at.
///
/// Reads the DOS header, then seeks to `e_lfanew` and reads the NT headers there, so headers
/// beyond any fixed prefix of the file are found. A file that is not a PE image, whose
/// `e_lfanew` is out of bounds or past the end of the file, or whose headers are cut off, has no
/// signature to verify and gives 0. Only a read failure is an error. The position afterwards is
/// unspecified.
fn embedded_signature_size<R: Read + Seek>(r: &mut R) -> io::Result<u32> {
    let mut dos = Vec::new();
    r.by_ref()
        .take(DOS_HEADER_SIZE as u64)
        .read_to_end(&mut dos)?;
    let Ok(at) = pe_offset(&dos) else {
        return Ok(0);
    };
    r.seek(SeekFrom::Start(u64::from(at)))?;
    let mut nt = Vec::new();
    r.by_ref().take(NT_WINDOW).read_to_end(&mut nt)?;
    Ok(security_directory_size_at(&nt).unwrap_or(0))
}

#[cfg(windows)]
mod imp {
    use std::{
        ffi::c_void,
        fs::{File, OpenOptions},
        io::Seek,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
        path::Path,
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE},
        Security::{
            Cryptography::{
                CERT_NAME_SIMPLE_DISPLAY_TYPE, CERT_SHA256_HASH_PROP_ID,
                CertGetCertificateContextProperty, CertGetNameStringW,
            },
            WinTrust::{
                CRYPT_PROVIDER_SGNR, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA,
                WINTRUST_DATA_0, WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE,
                WTD_REVOCATION_CHECK_NONE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
                WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvCertFromChain, WinVerifyTrust,
            },
        },
        Storage::FileSystem::FILE_SHARE_READ,
    };

    use super::{NO_SIGNER_DATA, Signer, SignerError, classify, embedded_signature_size};

    // windows-sys binds these two wintrust.dll helpers only behind its Catalog and Sip features,
    // so they are declared here the way it declares the others (raw-dylib, no import library
    // needed).
    #[cfg_attr(
        target_arch = "x86",
        link(
            name = "wintrust.dll",
            kind = "raw-dylib",
            modifiers = "+verbatim",
            import_name_type = "undecorated"
        )
    )]
    #[cfg_attr(
        not(target_arch = "x86"),
        link(name = "wintrust.dll", kind = "raw-dylib", modifiers = "+verbatim")
    )]
    unsafe extern "system" {
        // The provider data is only passed back to wintrust, so it stays opaque here (its
        // windows-sys type needs two more feature sets).
        fn WTHelperProvDataFromStateData(hstatedata: HANDLE) -> *mut c_void;
        fn WTHelperGetProvSignerFromChain(
            pprovdata: *mut c_void,
            idxsigner: u32,
            fcountersigner: windows_sys::core::BOOL,
            idxcountersigner: u32,
        ) -> *mut CRYPT_PROVIDER_SGNR;
    }

    const S_OK: i32 = 0;

    pub(super) fn check_signer(library: &Path) -> Result<Signer, SignerError> {
        // Read sharing only: nobody can open the file for writing, rename or delete it while the
        // handle is open (GENERIC_READ is the default access of `read(true)`).
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(library)?;
        // Not a PE image, or one cut off before its directories: no signature to verify.
        let signed = embedded_signature_size(&mut file)? != 0;
        file.rewind()?;
        if !signed {
            return Ok(Signer::Unsigned { file });
        }
        match verify(&file, library) {
            Ok((subject, thumbprint_sha256)) => Ok(Signer::Trusted {
                subject,
                thumbprint_sha256,
                file,
            }),
            Err(hresult) => Err(SignerError::Invalid {
                failure: classify(hresult),
                hresult,
                file,
            }),
        }
    }

    /// Runs the trust provider over the open file; returns the signer's subject and thumbprint,
    /// or the HRESULT that was not success.
    fn verify(file: &File, path: &Path) -> Result<(String, String), i32> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut file_info = WINTRUST_FILE_INFO {
            cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: wide.as_ptr(),
            hFile: file.as_raw_handle(),
            pgKnownSubject: ptr::null_mut(),
        };
        let mut data = WINTRUST_DATA {
            cbStruct: size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            fdwRevocationChecks: WTD_REVOKE_NONE,
            dwUnionChoice: WTD_CHOICE_FILE,
            Anonymous: WINTRUST_DATA_0 {
                pFile: &mut file_info,
            },
            dwStateAction: WTD_STATEACTION_VERIFY,
            // The machine-wide Software Publishing policy can switch revocation checking on for
            // every caller; the explicit no-revocation flag keeps it off.
            dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_NONE,
            ..Default::default()
        };
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;

        // SAFETY: `data` is a fully initialised WINTRUST_DATA whose union points at `file_info`,
        // which points at `wide` (NUL-terminated) and at a handle `file` keeps open; all three
        // outlive both calls. INVALID_HANDLE_VALUE as the window means "no user interface".
        let status = unsafe {
            WinVerifyTrust(
                INVALID_HANDLE_VALUE,
                &mut action,
                (&raw mut data).cast::<c_void>(),
            )
        };
        let result = if status == S_OK {
            // SAFETY: the VERIFY call succeeded, so `data.hWVTStateData` is a live state that
            // is closed only below.
            unsafe { signer_identity(&data) }
        } else {
            Err(status)
        };
        // Release the state the VERIFY call allocated, whatever its result.
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        // SAFETY: as above; the state is not used after this call.
        unsafe {
            WinVerifyTrust(
                INVALID_HANDLE_VALUE,
                &mut action,
                (&raw mut data).cast::<c_void>(),
            );
        }
        result
    }

    /// Reads the first signer's leaf certificate out of the verified state.
    ///
    /// # Safety
    ///
    /// `data` must hold the state of a successful VERIFY call that has not been closed.
    unsafe fn signer_identity(data: &WINTRUST_DATA) -> Result<(String, String), i32> {
        // SAFETY: the caller guarantees the state is live; every returned pointer is checked for
        // null and points into that state, which stays valid until it is closed.
        unsafe {
            let provider = WTHelperProvDataFromStateData(data.hWVTStateData);
            if provider.is_null() {
                return Err(NO_SIGNER_DATA);
            }
            let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
            if signer.is_null() {
                return Err(NO_SIGNER_DATA);
            }
            // Index 0 of the signer's chain is the leaf (the signing certificate).
            let leaf = WTHelperGetProvCertFromChain(signer, 0);
            if leaf.is_null() || (*leaf).pCert.is_null() {
                return Err(NO_SIGNER_DATA);
            }
            let cert = (*leaf).pCert;

            let chars = CertGetNameStringW(
                cert,
                CERT_NAME_SIMPLE_DISPLAY_TYPE,
                0,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            // An empty name still counts its terminating NUL, so 1 means no name.
            if chars <= 1 {
                return Err(NO_SIGNER_DATA);
            }
            let mut name = vec![0u16; chars as usize];
            let written = CertGetNameStringW(
                cert,
                CERT_NAME_SIMPLE_DISPLAY_TYPE,
                0,
                ptr::null(),
                name.as_mut_ptr(),
                chars,
            );
            if written <= 1 {
                return Err(NO_SIGNER_DATA);
            }
            // The count includes the terminating NUL.
            name.truncate((written as usize).saturating_sub(1));
            let subject = String::from_utf16_lossy(&name);

            let mut hash = [0u8; 32];
            let mut size = hash.len() as u32;
            let ok = CertGetCertificateContextProperty(
                cert,
                CERT_SHA256_HASH_PROP_ID,
                hash.as_mut_ptr().cast::<c_void>(),
                &mut size,
            );
            if ok == 0 || size as usize != hash.len() {
                return Err(NO_SIGNER_DATA);
            }
            let thumbprint = hash.iter().map(|b| format!("{b:02X}")).collect();
            Ok((subject, thumbprint))
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    use super::{Signer, SignerError};

    pub(super) fn check_signer(_library: &Path) -> Result<Signer, SignerError> {
        Ok(Signer::NotApplicable)
    }
}

#[cfg(test)]
mod tests;
