//! Signer check tests: the PE parser and the HRESULT mapping on synthetic data (every platform),
//! and the check itself against real files (Windows).

use super::*;

/// Builds PE headers: `directories` entries, the security entry with `security_size`.
fn pe(plus: bool, directories: u32, security_size: u32, optional_size: Option<u16>) -> Vec<u8> {
    pe_at(0x80, plus, directories, security_size, optional_size)
}

/// As [`pe`], with the PE signature at `pe_offset` (the gap is zeros).
fn pe_at(
    pe_offset: usize,
    plus: bool,
    directories: u32,
    security_size: u32,
    optional_size: Option<u16>,
) -> Vec<u8> {
    let (magic, dirs_at) = if plus {
        (0x20Bu16, 112usize)
    } else {
        (0x10B, 96)
    };
    let full = dirs_at + 16 * 8;
    let mut h = vec![0u8; pe_offset + 24 + full];
    h[..2].copy_from_slice(b"MZ");
    h[0x3C..0x40].copy_from_slice(&(pe_offset as u32).to_le_bytes());
    h[pe_offset..pe_offset + 4].copy_from_slice(b"PE\0\0");
    let size = optional_size.unwrap_or(full as u16);
    h[pe_offset + 20..pe_offset + 22].copy_from_slice(&size.to_le_bytes());
    let opt = pe_offset + 24;
    h[opt..opt + 2].copy_from_slice(&magic.to_le_bytes());
    h[opt + dirs_at - 4..opt + dirs_at].copy_from_slice(&directories.to_le_bytes());
    let entry = opt + dirs_at + SECURITY_DIRECTORY * DATA_DIRECTORY_SIZE;
    h[entry..entry + 4].copy_from_slice(&0x1000u32.to_le_bytes());
    h[entry + 4..entry + 8].copy_from_slice(&security_size.to_le_bytes());
    h
}

#[test]
fn security_directory_is_read_in_pe32_and_pe32_plus() {
    for plus in [false, true] {
        assert_eq!(
            security_directory_size(&pe(plus, 16, 0x1A00, None)),
            Ok(0x1A00)
        );
        assert_eq!(security_directory_size(&pe(plus, 5, 0x20, None)), Ok(0x20));
    }
}

#[test]
fn zero_size_means_unsigned() {
    for plus in [false, true] {
        assert_eq!(security_directory_size(&pe(plus, 16, 0, None)), Ok(0));
    }
}

#[test]
fn fewer_than_five_directories_means_unsigned() {
    // The bytes at the entry are non-zero, but the header declares no such entry.
    for plus in [false, true] {
        assert_eq!(security_directory_size(&pe(plus, 4, 0x1A00, None)), Ok(0));
        assert_eq!(security_directory_size(&pe(plus, 0, 0x1A00, None)), Ok(0));
    }
}

#[test]
fn entry_outside_the_declared_optional_header_means_unsigned() {
    let h = pe(false, 16, 0x1A00, Some(96 + 4 * 8));
    assert_eq!(security_directory_size(&h), Ok(0));
}

#[test]
fn bad_headers_are_errors() {
    let good = pe(false, 16, 8, None);
    assert_eq!(security_directory_size(&[]), Err(PeError::Truncated));
    assert_eq!(security_directory_size(b"M"), Err(PeError::Truncated));
    let mut bad_mz = good.clone();
    bad_mz[0] = b'Z';
    assert_eq!(security_directory_size(&bad_mz), Err(PeError::NotPe));
    let mut bad_pe = good.clone();
    bad_pe[0x80] = b'X';
    assert_eq!(security_directory_size(&bad_pe), Err(PeError::NotPe));
    let mut bad_magic = good.clone();
    bad_magic[0x80 + 24] = 0x07;
    assert_eq!(security_directory_size(&bad_magic), Err(PeError::BadMagic));
    let mut far = good.clone();
    far[0x3C..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(security_directory_size(&far), Err(PeError::TooFar));
    let mut beyond = good.clone();
    beyond[0x3C..0x40].copy_from_slice(&MAX_PE_OFFSET.to_le_bytes());
    assert_eq!(security_directory_size(&beyond), Err(PeError::Truncated));
    for cut in [
        0x40,
        0x80,
        0x84,
        0x80 + 24,
        0x80 + 24 + 96,
        0x80 + 24 + 96 + 32 + 7,
    ] {
        assert_eq!(
            security_directory_size(&good[..cut]),
            Err(PeError::Truncated),
            "cut at {cut:#x}"
        );
    }
}

#[test]
fn pe32_plus_optional_header_too_short_for_the_entry_means_unsigned() {
    // PE32+ holds the entry at 112 + 32; a declared size of 112 + 32 + 4 cuts it in half.
    let h = pe(true, 16, 0x1A00, Some(112 + 4 * 8 + 4));
    assert_eq!(security_directory_size(&h), Ok(0));
    // Exactly large enough for the entry: read.
    let h = pe(true, 16, 0x1A00, Some(112 + 5 * 8));
    assert_eq!(security_directory_size(&h), Ok(0x1A00));
    // The optional header is cut off in the file itself.
    let h = pe(true, 16, 0x1A00, None);
    assert_eq!(
        security_directory_size(&h[..0x80 + 24 + 112 + 5 * 8 - 1]),
        Err(PeError::Truncated)
    );
}

#[test]
fn nt_headers_are_parsed_from_a_slice_of_their_own() {
    let h = pe(false, 16, 0x40, None);
    assert_eq!(pe_offset(&h), Ok(0x80));
    assert_eq!(security_directory_size_at(&h[0x80..]), Ok(0x40));
    assert_eq!(pe_offset(&h[..0x3F]), Err(PeError::Truncated));
    assert_eq!(pe_offset(b"ZM"), Err(PeError::NotPe));
    assert_eq!(security_directory_size_at(&h[0x81..]), Err(PeError::NotPe));
}

#[test]
fn headers_beyond_64_kib_are_read_from_a_reader() {
    use std::io::Cursor;
    for plus in [false, true] {
        let at = 200 * 1024;
        let mut file = vec![0u8; at];
        let image = pe_at(at, plus, 16, 0x2400, None);
        file.splice(..0x40, image[..0x40].iter().copied());
        file.extend_from_slice(&image[at..]);
        assert_eq!(
            embedded_signature_size(&mut Cursor::new(&file)).unwrap(),
            0x2400
        );
        // A pointer past the end of the file, and a file shorter than the DOS header, are unsigned.
        let cut = &file[..at + 10];
        assert_eq!(embedded_signature_size(&mut Cursor::new(cut)).unwrap(), 0);
        assert_eq!(
            embedded_signature_size(&mut Cursor::new(&file[..8])).unwrap(),
            0
        );
        // A pointer past the bound is not followed.
        let mut far = file.clone();
        far[0x3C..0x40].copy_from_slice(&(MAX_PE_OFFSET + 1).to_le_bytes());
        assert_eq!(embedded_signature_size(&mut Cursor::new(&far)).unwrap(), 0);
    }
}

#[test]
fn hresults_map_to_failure_classes() {
    let table: [(u32, SignatureFailure); 21] = [
        (0x8009_6010, SignatureFailure::Tampered),
        (0x800B_0109, SignatureFailure::UntrustedSigner),
        (0x800B_010D, SignatureFailure::UntrustedSigner),
        (0x800B_0112, SignatureFailure::UntrustedSigner),
        (0x800B_010A, SignatureFailure::UntrustedSigner),
        (0x800B_0106, SignatureFailure::UntrustedSigner),
        (0x800B_0110, SignatureFailure::UntrustedSigner),
        (0x800B_010C, SignatureFailure::UntrustedSigner),
        (0x800B_010E, SignatureFailure::UntrustedSigner),
        (0x800B_0111, SignatureFailure::UntrustedSigner),
        (0x800B_0004, SignatureFailure::UntrustedSigner),
        (0x800B_0101, SignatureFailure::Expired),
        (0x8009_6005, SignatureFailure::Expired),
        (0x800B_0100, SignatureFailure::Malformed),
        (0x800B_0003, SignatureFailure::Malformed),
        (0x8009_6011, SignatureFailure::Malformed),
        (0x8009_6004, SignatureFailure::Malformed),
        (0x8009_6002, SignatureFailure::Malformed),
        (0x8007_0005, SignatureFailure::Other),
        (0x800B_0102, SignatureFailure::Other),
        (0x8000_4005, SignatureFailure::Other),
    ];
    for (code, expected) in table {
        assert_eq!(classify(code as i32), expected, "{code:#010X}");
    }
}

#[test]
fn finding_and_error_show_the_hresult_in_hex() {
    let reason = Reason::InvalidSignature {
        failure: SignatureFailure::Tampered,
        hresult: TRUST_E_BAD_DIGEST,
    };
    assert!(reason.to_string().contains("0x80096010"), "{reason}");
    let error = SignerError::Invalid {
        failure: SignatureFailure::Expired,
        hresult: CERT_E_EXPIRED,
        file: tempfile::tempfile().unwrap(),
    };
    assert!(error.to_string().contains("0x800B0101"), "{error}");
}

#[test]
fn preload_error_lists_findings_then_the_io_error() {
    let finding = Finding {
        path: PathBuf::from("lib"),
        role: Role::Library,
        reason: Reason::NullDacl,
    };
    let both = PreloadError {
        findings: vec![finding.clone()],
        io: Some(io::Error::other("boom")),
        signer: None,
        held: None,
    };
    let text = both.to_string();
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.lines().nth(1).unwrap().contains("boom"));
    let only_io = PreloadError {
        findings: Vec::new(),
        io: Some(io::Error::other("boom")),
        signer: None,
        held: None,
    };
    assert_eq!(only_io.to_string().lines().count(), 1);
}

#[cfg(unix)]
mod unix {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let lib = dir.join("lib.so");
        fs::write(&lib, b"x").unwrap();
        fs::set_permissions(&lib, fs::Permissions::from_mode(0o644)).unwrap();
        (tmp, lib)
    }

    #[test]
    fn signer_is_not_applicable_off_windows() {
        let (_tmp, lib) = fixture();
        assert!(matches!(check_signer(&lib), Ok(Signer::NotApplicable)));
        // Not even a missing file is looked at.
        assert!(matches!(
            check_signer(Path::new("/nonexistent/lib.so")),
            Ok(Signer::NotApplicable)
        ));
    }

    #[test]
    fn clean_library_passes_check_library() {
        let (_tmp, lib) = fixture();
        let policy = Policy::system().trusting_current_user();
        let signer = check_library(&lib, &[], &policy).unwrap();
        assert!(signer.file().is_none());
    }

    #[test]
    fn check_library_reports_writability_findings() {
        let (_tmp, lib) = fixture();
        fs::set_permissions(&lib, fs::Permissions::from_mode(0o666)).unwrap();
        let policy = Policy::system().trusting_current_user();
        let e = check_library(&lib, &[], &policy).unwrap_err();
        assert_eq!(e.findings.len(), 1);
        assert_eq!(e.findings[0].role, Role::Library);
        assert!(e.io.is_none());
        // The signer verdict is not thrown away with the finding.
        assert!(matches!(e.signer, Some(Signer::NotApplicable)));
        assert!(e.held.is_none());
    }
}

#[cfg(windows)]
mod windows {
    use std::{fs, process::Command};

    use super::*;

    /// A test that cannot run here fails on CI and says so elsewhere.
    fn unavailable(why: &str) {
        assert!(std::env::var_os("CI").is_none(), "{why}");
        eprintln!("skipped: {why}");
    }

    fn policy() -> Policy {
        Policy::system().trusting_current_user()
    }

    /// A copy of the test executable as an unsigned PE image named `fixture.dll`.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let dll = dir.join("fixture.dll");
        fs::copy(std::env::current_exe().unwrap(), &dll).unwrap();
        (tmp, dll)
    }

    /// Signs `target` with a throw-away self-signed code-signing certificate (SHA-256, no time
    /// stamp) and removes the certificate again. Err carries the reason it could not.
    fn sign_self_signed(target: &Path) -> Result<(), String> {
        const SCRIPT: &str = r"
$ErrorActionPreference = 'Stop'
$c = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=ngr-library-resolver test signer' -CertStoreLocation Cert:\CurrentUser\My
try {
    # The status is UnknownError (untrusted root); the file is signed all the same.
    Set-AuthenticodeSignature -FilePath $env:NGR_SIGN_TARGET -Certificate $c -HashAlgorithm SHA256 | Out-Null
} finally {
    foreach ($s in 'My', 'CA', 'Root') {
        Remove-Item -Path ('Cert:\CurrentUser\' + $s + '\' + $c.Thumbprint) -DeleteKey -Force -ErrorAction SilentlyContinue
    }
}
";
        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                SCRIPT,
            ])
            .env("NGR_SIGN_TARGET", target)
            .output()
            .map_err(|e| format!("cannot run powershell.exe: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "signing script failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    fn has_signature(path: &Path) -> bool {
        let mut file = fs::File::open(path).unwrap();
        embedded_signature_size(&mut file).unwrap() != 0
    }

    #[test]
    fn unsigned_image_is_unsigned_and_passes_check_library() {
        let (_tmp, dll) = fixture();
        assert!(matches!(check_signer(&dll), Ok(Signer::Unsigned { .. })));
        let signer = check_library(&dll, &[], &policy()).unwrap();
        assert!(signer.file().is_some());
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let (_tmp, dll) = fixture();
        let missing = dll.with_file_name("missing.dll");
        assert!(matches!(check_signer(&missing), Err(SignerError::Io(_))));
        let e = check_library(&missing, &[], &policy()).unwrap_err();
        assert!(e.io.is_some());
    }

    #[test]
    fn held_file_cannot_be_overwritten() {
        let (_tmp, dll) = fixture();
        let signer = check_signer(&dll).unwrap();
        assert!(fs::OpenOptions::new().write(true).open(&dll).is_err());
        assert!(fs::remove_file(&dll).is_err());
        drop(signer);
        assert!(fs::remove_file(&dll).is_ok());
    }

    #[test]
    fn self_signed_signature_is_an_untrusted_signer() {
        let (_tmp, dll) = fixture();
        if let Err(why) = sign_self_signed(&dll) {
            return unavailable(&why);
        }
        assert!(has_signature(&dll), "the fixture was not signed");
        match check_signer(&dll) {
            Err(SignerError::Invalid {
                failure, hresult, ..
            }) => {
                assert_eq!(
                    failure,
                    SignatureFailure::UntrustedSigner,
                    "{hresult:#010X}"
                );
            }
            other => panic!("expected an untrusted signer, got {other:?}"),
        }
        let e = check_library(&dll, &[], &policy()).unwrap_err();
        assert_eq!(e.findings.len(), 1, "{e}");
        assert_eq!(e.findings[0].role, Role::Library);
        assert!(matches!(
            e.findings[0].reason,
            Reason::InvalidSignature {
                failure: SignatureFailure::UntrustedSigner,
                ..
            }
        ));
        // No verdict, but the open file travels with the error.
        assert!(e.signer.is_none());
        assert!(e.held.is_some());
    }

    #[test]
    fn writability_and_signature_findings_are_reported_together() {
        let (_tmp, dll) = fixture();
        if let Err(why) = sign_self_signed(&dll) {
            return unavailable(&why);
        }
        // Everyone in BUILTIN\Users may write the library.
        let status = Command::new("icacls")
            .arg(&dll)
            .arg("/grant")
            .arg("*S-1-5-32-545:(W)")
            .status()
            .unwrap();
        assert!(status.success());
        let e = check_library(&dll, &[], &policy()).unwrap_err();
        assert!(e.findings.len() >= 2, "{e}");
        assert!(
            e.findings.iter().any(|f| f.role == Role::Library
                && matches!(f.reason, Reason::WritableByRegularUsers(_))),
            "{e}"
        );
        assert!(
            matches!(
                e.findings.last().unwrap().reason,
                Reason::InvalidSignature { .. }
            ),
            "{e}"
        );
        assert!(e.held.is_some());
    }

    /// `LoadLibraryExW` and `FreeLibrary`, which windows-sys only offers behind a feature this
    /// crate does not enable; declared the way the wintrust helpers are.
    #[cfg_attr(
        target_arch = "x86",
        link(
            name = "kernel32.dll",
            kind = "raw-dylib",
            modifiers = "+verbatim",
            import_name_type = "undecorated"
        )
    )]
    #[cfg_attr(
        not(target_arch = "x86"),
        link(name = "kernel32.dll", kind = "raw-dylib", modifiers = "+verbatim")
    )]
    unsafe extern "system" {
        fn LoadLibraryExW(
            name: *const u16,
            file: windows_sys::Win32::Foundation::HANDLE,
            flags: u32,
        ) -> windows_sys::Win32::Foundation::HMODULE;
        fn FreeLibrary(module: windows_sys::Win32::Foundation::HMODULE) -> i32;
    }

    #[test]
    fn library_loads_while_the_verdict_holds_the_file() {
        // A real load (no flags) of a copy of a system DLL: the held handle denies write
        // sharing, and the loader must still be able to map the file. The copy is of
        // version.dll, a small DLL whose load has no side effects here. Loading the unsigned test-executable copy
        // would run the wrong entry point, and the data-file flags would not map the image the
        // way the real load does.
        use std::os::windows::ffi::OsStrExt;
        let source = Path::new(r"C:\Windows\System32\version.dll");
        if !source.is_file() {
            return unavailable("version.dll not found");
        }
        let tmp = tempfile::tempdir().unwrap();
        let copy = fs::canonicalize(tmp.path()).unwrap().join("version.dll");
        fs::copy(source, &copy).unwrap();
        let signer = check_library(&copy, &[], &policy()).unwrap();
        assert!(signer.file().is_some());
        let wide: Vec<u16> = copy.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: `wide` is a NUL-terminated path; the module is freed below.
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 0) };
        let error = io::Error::last_os_error();
        assert!(!module.is_null(), "LoadLibraryExW failed: {error}");
        // SAFETY: `module` came from the successful call above and is freed once.
        assert_ne!(unsafe { FreeLibrary(module) }, 0);
        drop(signer);
    }

    #[test]
    fn changed_byte_in_a_signed_image_is_tampered() {
        let (_tmp, dll) = fixture();
        // The middle of the unsigned image is inside the hashed region; the signature is
        // appended after it.
        let middle = fs::metadata(&dll).unwrap().len() / 2;
        if let Err(why) = sign_self_signed(&dll) {
            return unavailable(&why);
        }
        assert!(has_signature(&dll), "the fixture was not signed");
        let mut bytes = fs::read(&dll).unwrap();
        bytes[middle as usize] ^= 0xFF;
        fs::write(&dll, bytes).unwrap();
        match check_signer(&dll) {
            Err(SignerError::Invalid {
                failure, hresult, ..
            }) => {
                assert_eq!(failure, SignatureFailure::Tampered, "{hresult:#010X}");
            }
            other => panic!("expected a tampered file, got {other:?}"),
        }
    }

    #[test]
    fn microsoft_signed_image_is_trusted() {
        let candidates = [
            r"C:\Windows\System32\ntoskrnl.exe",
            r"C:\Windows\System32\ci.dll",
            r"C:\Windows\explorer.exe",
        ];
        let Some(path) = candidates
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file() && has_signature(p))
        else {
            return unavailable("no embedded-signed system image found");
        };
        match check_signer(&path) {
            Ok(Signer::Trusted {
                subject,
                thumbprint_sha256,
                ..
            }) => {
                assert!(subject.contains("Microsoft"), "{subject}");
                assert_eq!(thumbprint_sha256.len(), 64, "{thumbprint_sha256}");
                assert!(thumbprint_sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            }
            other => panic!(
                "expected a trusted signer for {}, got {other:?}",
                path.display()
            ),
        }
    }

    #[test]
    fn catalog_signed_image_is_unsigned() {
        let kernel32 = Path::new(r"C:\Windows\System32\kernel32.dll");
        if has_signature(kernel32) {
            return unavailable("kernel32.dll carries an embedded signature on this system");
        }
        assert!(matches!(
            check_signer(kernel32),
            Ok(Signer::Unsigned { .. })
        ));
    }
}
