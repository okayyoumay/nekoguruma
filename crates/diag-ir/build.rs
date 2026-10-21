//! Regenerates the IR's FlatBuffers bindings from `schema/ir.fbs` when the `flatc` feature is on
//! (`schema/README.md`). Without it the build uses the committed `src/generated/ir_generated.rs`
//! and needs no `flatc`. `download-flatc` fetches the pinned `flatc` release binary with `curl` and
//! checks its SHA-256; plain `flatc` runs the one on `PATH`, or the one `FLATC` names.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    generate();
}

#[cfg(not(feature = "flatc"))]
fn generate() {}

// The generated code calls the `flatbuffers` runtime, so the compiler must be the release the
// workspace pins (`schema/README.md`).
#[cfg(feature = "flatc")]
const FLATC_VERSION: &str = "25.12.19";

#[cfg(feature = "flatc")]
fn generate() {
    use std::path::PathBuf;
    use std::process::Command;

    println!("cargo:rerun-if-changed=schema/ir.fbs");
    println!("cargo:rerun-if-env-changed=FLATC");

    #[cfg(feature = "download-flatc")]
    let flatc = download::flatc();
    #[cfg(not(feature = "download-flatc"))]
    let flatc = std::env::var_os("FLATC").map_or_else(|| PathBuf::from("flatc"), PathBuf::from);

    let version = Command::new(&flatc)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", flatc.display()));
    let version = String::from_utf8_lossy(&version.stdout);
    assert!(
        version.trim() == format!("flatc version {FLATC_VERSION}"),
        "{} reports `{}`; the bindings need flatc {FLATC_VERSION}",
        flatc.display(),
        version.trim()
    );

    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let status = Command::new(&flatc)
        .current_dir(&dir)
        .args([
            "--rust",
            "--gen-object-api",
            "-o",
            "src/generated",
            "schema/ir.fbs",
        ])
        .status()
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", flatc.display()));
    assert!(status.success(), "flatc failed: {status}");
}

#[cfg(feature = "download-flatc")]
mod download {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use sha2::{Digest, Sha256};

    /// The release asset for the platform the build script runs on, its SHA-256, and the
    /// executable's name inside it. The flatbuffers release ships no other platforms; elsewhere
    /// use the `flatc` feature with a compiler of the same version.
    const ASSET: Option<(&str, &str, &str)> =
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            // The g++ build is statically linked, so it runs on any x86_64 Linux.
            Some((
                "Linux.flatc.binary.g++-13.zip",
                "9f87066dc5dfa7fe02090b55bab5f3e55df03e32c9b0cdf229004ade7d091039",
                "flatc",
            ))
        } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some((
                "Windows.flatc.binary.zip",
                "fff9445c9db907227bc64b54cc98743084c4949282aa4e576cff6a955724ddc8",
                "flatc.exe",
            ))
        } else if cfg!(target_os = "macos") {
            // A universal binary (x86_64 and arm64).
            Some((
                "Mac.flatc.binary.zip",
                "9340a5f9900b95e34ccadcb06bceec91180cc8b83098d5e966ed6d8d590cbba2",
                "flatc",
            ))
        } else {
            None
        };

    /// Downloads, checks and unpacks the release binary into `OUT_DIR`, once per build directory.
    pub fn flatc() -> PathBuf {
        let Some((asset, sha256, name)) = ASSET else {
            panic!(
                "flatc {} has no release binary for this platform; use the `flatc` feature with \
                 FLATC naming a flatc {0}",
                super::FLATC_VERSION
            )
        };
        // A directory per version, so a version change never reuses an older binary.
        let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo"))
            .join(format!("flatc-{}", super::FLATC_VERSION));
        fs::create_dir_all(&out).expect("create the flatc directory");
        let flatc = out.join(name);
        if flatc.exists() {
            return flatc;
        }

        let archive = out.join(asset);
        let url = format!(
            "https://github.com/google/flatbuffers/releases/download/v{}/{asset}",
            super::FLATC_VERSION
        );
        // curl honours the proxy and trust settings of the build environment.
        let status = Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--output",
            ])
            .arg(&archive)
            .arg(&url)
            .status()
            .unwrap_or_else(|error| panic!("cannot run curl to fetch {url}: {error}"));
        assert!(status.success(), "curl failed to fetch {url}: {status}");

        let bytes = fs::read(&archive).expect("read the downloaded archive");
        let digest: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(
            digest == sha256,
            "{url} has SHA-256 {digest}, expected {sha256}"
        );

        unpack(&bytes, name, &flatc);
        fs::remove_file(&archive).expect("remove the downloaded archive");
        flatc
    }

    /// Writes the archive's `name` entry to `path` and marks it executable.
    fn unpack(archive: &[u8], name: &str, path: &Path) {
        let mut zip =
            zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("read the flatc archive");
        let mut entry = zip.by_name(name).expect("the archive holds flatc");
        // Unpack beside the target and rename, so an interrupted build leaves no partial binary
        // that the next run would take as complete.
        let partial = path.with_extension("partial");
        let mut file = fs::File::create(&partial).expect("create the flatc binary");
        std::io::copy(&mut entry, &mut file).expect("unpack the flatc binary");
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))
                .expect("make flatc executable");
        }
        fs::rename(&partial, path).expect("move the flatc binary into place");
    }
}
