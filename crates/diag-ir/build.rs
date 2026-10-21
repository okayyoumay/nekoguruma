//! Regenerates the IR's FlatBuffers bindings from `schema/ir.fbs` when the `flatc` feature is on
//! (`schema/README.md`). Without it the build uses the committed `src/generated/ir_generated.rs`
//! and needs no `flatc`. `vendored-flatc` builds the pinned `flatc` from source (cmake and a C++
//! compiler); plain `flatc` runs the one on `PATH`, or the one `FLATC` names.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    generate();
}

#[cfg(not(feature = "flatc"))]
fn generate() {}

#[cfg(feature = "flatc")]
fn generate() {
    use std::path::PathBuf;
    use std::process::Command;

    // The generated code calls the `flatbuffers` runtime, so the compiler must be the release the
    // workspace pins (`schema/README.md`).
    const FLATC_VERSION: &str = "25.12.19";

    println!("cargo:rerun-if-changed=schema/ir.fbs");
    println!("cargo:rerun-if-env-changed=FLATC");

    #[cfg(feature = "vendored-flatc")]
    let flatc = PathBuf::from(flatc_fork::flatc());
    #[cfg(not(feature = "vendored-flatc"))]
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
