//! Signer check tests: the PE parser and the HRESULT mapping on synthetic data (every platform),
//! and the check itself against real files (Windows).

use super::*;

/// Builds PE headers: `directories` entries, the security entry with `security_size`.
fn pe(plus: bool, directories: u32, security_size: u32, optional_size: Option<u16>) -> Vec<u8> {
    let pe_offset = 0x80usize;
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
    assert_eq!(security_directory_size(&far), Err(PeError::Truncated));
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
fn hresults_map_to_failure_classes() {
    let table: [(u32, SignatureFailure); 18] = [
        (0x8009_6010, SignatureFailure::Tampered),
        (0x800B_0109, SignatureFailure::UntrustedSigner),
        (0x800B_010D, SignatureFailure::UntrustedSigner),
        (0x800B_0112, SignatureFailure::UntrustedSigner),
        (0x800B_010A, SignatureFailure::UntrustedSigner),
        (0x800B_0106, SignatureFailure::UntrustedSigner),
        (0x800B_0110, SignatureFailure::UntrustedSigner),
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
    };
    let text = both.to_string();
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.lines().nth(1).unwrap().contains("boom"));
    let only_io = PreloadError {
        findings: Vec::new(),
        io: Some(io::Error::other("boom")),
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
        Remove-Item -Path ('Cert:\CurrentUser\' + $s + '\' + $c.Thumbprint) -Force -ErrorAction SilentlyContinue
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
        let header = fs::read(path).unwrap();
        let header = &header[..header.len().min(HEADER_WINDOW as usize)];
        security_directory_size(header).is_ok_and(|size| size != 0)
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
            Err(SignerError::Invalid { failure, hresult }) => {
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
            Err(SignerError::Invalid { failure, hresult }) => {
                assert_eq!(failure, SignatureFailure::Tampered, "{hresult:#010X}");
            }
            other => panic!("expected a tampered file, got {other:?}"),
        }
    }

    #[test]
    fn microsoft_signed_image_is_trusted() {
        let candidates = ["ntoskrnl.exe", "ci.dll", "explorer.exe"];
        let system32 = Path::new(r"C:\Windows\System32");
        let Some(path) = candidates
            .iter()
            .map(|name| system32.join(name))
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
