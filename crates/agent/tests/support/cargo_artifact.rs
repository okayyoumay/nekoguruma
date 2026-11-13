//! Build outputs of another package for the tests that need them, found by asking Cargo, shared
//! through `#[path]` (`ngr_agent_binary.rs`, `crates/j2534-0404-service/tests/common/artifacts.rs`).
//!
//! A test that starts `ngr-agent` or loads the `sim-vci` cdylib needs a file that belongs to a
//! package other than the one under test, and which a run of that package alone does not
//! necessarily build or rebuild. The test therefore runs Cargo itself (`env!("CARGO")`, the
//! Cargo that built the test): `cargo build --locked --message-format=json --manifest-path
//! <workspace>/Cargo.toml -p <package>` with `--bin <name>` or `--lib`, and takes the path from
//! the `compiler-artifact` message of that package's target. No target-directory layout is
//! assumed, and the file is current: Cargo rebuilds it when its sources changed and does nothing
//! when they did not.
//!
//! The nested build is kept on the test's own configuration so that it reuses the test's build
//! instead of making a second one:
//!
//! - Profile: `--release` when the test was built without debug assertions (the dev profile has
//!   them, the release profile does not). A custom profile (`cargo test --profile <name>`) is not
//!   told apart from the dev or release profile by this rule.
//! - Target: `--target <triple>` when the test executable lies below a target-triple directory of
//!   the target directory (`<target dir>/<triple>/<profile>/...`). The target directory's root is
//!   the parent of `CARGO_TARGET_TMPDIR`, which every layout keeps at the root, and the triple is
//!   the first directory below it with three or more `-`-separated parts (a profile name is not).
//! - Features: none are passed. `sim-vci`'s dev-dependency entry in
//!   `crates/j2534-0404-service/Cargo.toml` asks for none and the other packages are built with
//!   their default features, which is what the workspace build uses; a dev-dependency that asks
//!   for features needs the same `--features` here.
//!
//! The nested Cargo inherits the test's environment unchanged: `RUSTFLAGS` (so a
//! `-D warnings` run builds with the same flags and the same fingerprints), `CARGO_TARGET_DIR`
//! (so it is the same target directory), `RUSTUP_TOOLCHAIN` (so a `cargo +<toolchain>` run builds
//! with that toolchain), nextest's variables and the rest. Its working directory is the
//! workspace root, where the outer Cargo was started. A test process that starts while another
//! holds the build directory waits on Cargo's lock; it neither fails nor races.
//!
//! The result is cached for the life of the test process, so a test binary runs Cargo once per
//! artifact. Under nextest every test is its own process and runs it once.
//!
//! Only integration tests may include this file: it reads `CARGO_TARGET_TMPDIR`, which Cargo
//! sets for them alone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// What to take from a package's build. A package that includes this file uses one of the two.
// Not `expect`: the unit tests in `j2534-0404-service/tests/cargo_artifact.rs` use both kinds, and
// an `expect` there would be unfulfilled.
#[allow(
    dead_code,
    reason = "each including test package but the unit tests builds only one kind"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Artifact<'a> {
    /// The executable of the binary target `name`.
    Bin(&'a str),
    /// The package's library, a cdylib.
    Cdylib,
}

/// The workspace's root directory, from the including package's `crates/<package>`.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The path of `artifact` of `package`, built by Cargo now or found up to date. Panics with the
/// command and Cargo's stderr when the build fails or reports no such artifact.
pub fn build_artifact(package: &str, artifact: Artifact) -> PathBuf {
    static CACHE: Mutex<BTreeMap<String, PathBuf>> = Mutex::new(BTreeMap::new());
    let key = format!("{package} {artifact:?}");
    // The lock is held across the build: a second caller for the same artifact waits for it.
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(path) = cache.get(&key) {
        return path.clone();
    }
    let exe = std::env::current_exe().expect("test executable path");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .and_then(|root| cargo_target(&exe, root));
    let manifest = workspace_root().join("Cargo.toml");
    let args = cargo_args(
        package,
        artifact,
        !cfg!(debug_assertions),
        target.as_deref(),
        &manifest,
    );
    let cargo = env!("CARGO");
    let command = format!("{cargo} {}", args.join(" "));
    let output = Command::new(cargo)
        .args(&args)
        .current_dir(workspace_root())
        .output()
        .unwrap_or_else(|error| panic!("could not run `{command}`: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        panic!("`{command}` failed ({}):\n{stderr}", output.status);
    }
    let path = parse_artifact(&stdout, package, artifact)
        .unwrap_or_else(|error| panic!("`{command}`: {error}\n{stderr}"));
    cache.insert(key, path.clone());
    path
}

/// The arguments of the nested Cargo.
pub fn cargo_args(
    package: &str,
    artifact: Artifact,
    release: bool,
    target: Option<&str>,
    manifest: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "build",
        "--locked",
        "--message-format=json",
        "--manifest-path",
    ]
    .map(String::from)
    .into();
    args.push(manifest.display().to_string());
    args.extend(["-p".to_owned(), package.to_owned()]);
    match artifact {
        Artifact::Bin(name) => args.extend(["--bin".to_owned(), name.to_owned()]),
        Artifact::Cdylib => args.push("--lib".to_owned()),
    }
    if release {
        args.push("--release".to_owned());
    }
    if let Some(target) = target {
        args.extend(["--target".to_owned(), target.to_owned()]);
    }
    args
}

/// The target triple the test executable `exe` was built for, when it lies below a
/// `<triple>` directory of the target directory `root`; `None` for a host build, which has the
/// profile directory there instead, and for an executable outside `root`.
pub fn cargo_target(exe: &Path, root: &Path) -> Option<String> {
    let first = exe.strip_prefix(root).ok()?.components().next()?;
    let name = first.as_os_str().to_string_lossy();
    // `<triple>/<profile>/...` has more below the first directory; a bare file is not a triple.
    (name.split('-').count() >= 3
        && exe
            .strip_prefix(root.join(first))
            .ok()?
            .components()
            .count()
            > 1)
    .then(|| name.into_owned())
}

/// The path of `artifact` of `package` in the JSON messages of `cargo build
/// --message-format=json` (`stdout`, one message per line), or why there is none: the build did
/// not finish, failed, or reported no matching `compiler-artifact`. Of several matches the last
/// counts.
pub fn parse_artifact(stdout: &str, package: &str, artifact: Artifact) -> Result<PathBuf, String> {
    let mut finished = None;
    let mut found = None;
    for message in stdout.lines().filter_map(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .filter(|message| message.is_object())
    }) {
        match message["reason"].as_str() {
            Some("build-finished") => finished = message["success"].as_bool(),
            Some("compiler-artifact") if is_of(&message, package, artifact) => {
                found = artifact_path(&message, artifact).or(found);
            }
            _ => {}
        }
    }
    match (finished, found) {
        (None, _) => Err("Cargo reported no build-finished message".to_owned()),
        (Some(false), _) => Err("Cargo reported that the build failed".to_owned()),
        (Some(true), None) => Err(format!(
            "Cargo built nothing that matches {artifact:?} of package {package}"
        )),
        (Some(true), Some(path)) => Ok(path),
    }
}

/// Whether a `compiler-artifact` message is the target `artifact` of the package in the
/// directory `package` (a package's manifest sits in the directory of its name).
fn is_of(message: &serde_json::Value, package: &str, artifact: Artifact) -> bool {
    let in_package = message["manifest_path"]
        .as_str()
        .and_then(|path| Path::new(path).parent()?.file_name())
        .is_some_and(|dir| dir == package);
    let target = &message["target"];
    let kind = |wanted: &str| {
        target["kind"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|kind| kind == wanted))
    };
    let name = target["name"].as_str().unwrap_or_default();
    in_package
        && match artifact {
            Artifact::Bin(bin) => kind("bin") && name == bin,
            Artifact::Cdylib => kind("cdylib"),
        }
}

fn artifact_path(message: &serde_json::Value, artifact: Artifact) -> Option<PathBuf> {
    match artifact {
        Artifact::Bin(_) => message["executable"].as_str().map(PathBuf::from),
        Artifact::Cdylib => message["filenames"]
            .as_array()?
            .iter()
            .filter_map(|file| file.as_str())
            .find(|file| {
                Path::new(file).file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(std::env::consts::DLL_PREFIX)
                        && name.ends_with(std::env::consts::DLL_SUFFIX)
                })
            })
            .map(PathBuf::from),
    }
}
