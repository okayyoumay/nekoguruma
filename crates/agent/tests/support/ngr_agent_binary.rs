//! The `ngr-agent` binary for the end-to-end tests of the worker packages, shared through
//! `#[path]`: `crates/j2534-0404-service/tests/ngr_agent_end_to_end.rs`,
//! `crates/iso22900-service/tests/ngr_agent_dpdu_end_to_end.rs` and
//! `crates/iso22900-service/tests/ngr_agent_definition_end_to_end.rs`.
//!
//! `ngr-agent` belongs to another package, so cargo builds it only when the run builds the
//! `agent` package's binaries (as `cargo test --workspace` and CI do), and a run of the worker
//! package alone, or one that selects its tests with `--test`, uses whatever binary an earlier
//! build left. That binary can predate the sources the test was built from, and a test then fails
//! on what the older agent does rather than on the change.
//!
//! The lookup therefore checks the binary against the sources it is built from: the Rust files
//! under `src` and the build script of the `agent` package and of every workspace crate it
//! depends on (its `[dependencies]`, target-specific ones and `[build-dependencies]`, followed
//! through the crates' manifests; not its dev-dependencies, which the binary does not link). A
//! file newer than the binary is one cargo would rebuild it for, so building the binary clears
//! the failure. A test build writes no dep-info file for the binary that lists the library's
//! sources, so the crates are found from the manifests instead.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The `ngr-agent` binary next to the test's `deps` directory, after checking that it is built and
/// newer than its sources. `package` is the worker package under test, named in the failure
/// message.
pub fn ngr_agent_path(package: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("the test executable is in a deps directory");
    let path = dir.join(format!("ngr-agent{}", std::env::consts::EXE_SUFFIX));
    // Selecting tests with `--test` builds no binary of a package whose tests it leaves out, so
    // `-p agent` beside a `--test` of the worker package does not build it either.
    let rebuild = format!(
        "build it first (cargo build -p agent --bin ngr-agent), or test the whole workspace \
         (cargo test --workspace; a run of -p {package} alone does not rebuild it)"
    );
    assert!(
        path.is_file(),
        "{} should be built: {rebuild}",
        path.display()
    );
    let built = modified(&path).expect("the binary's modification time");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let stale = newer_than(&sources(&root, "agent"), built);
    assert!(
        stale.is_empty(),
        "{} is older than its sources ({}): {rebuild}",
        path.display(),
        stale
            .iter()
            .take(3)
            .map(|source| source.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    path
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The files of `files` modified after `built`.
fn newer_than(files: &[PathBuf], built: SystemTime) -> Vec<PathBuf> {
    files
        .iter()
        .filter(|file| modified(file).is_some_and(|time| time > built))
        .cloned()
        .collect()
}

/// The source files of `crates/<package>` under `root` and of the workspace crates it depends on:
/// each crate's `build.rs` and the `.rs` files under its `src`.
fn sources(root: &Path, package: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in crates(root, package) {
        let build = dir.join("build.rs");
        if build.is_file() {
            files.push(build);
        }
        rust_files(&dir.join("src"), &mut files);
    }
    files
}

/// The directories of `crates/<package>` and of every workspace crate its manifest's linked
/// dependencies reach. A dependency is a workspace crate when it names a `path`, or when
/// `crates/<name>` holds a manifest (the workspace's dependency table names each member at that
/// path).
fn crates(root: &Path, package: &str) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::new();
    let mut pending = vec![root.join("crates").join(package)];
    while let Some(dir) = pending.pop() {
        let Ok(dir) = dir.canonicalize() else {
            continue;
        };
        if !found.insert(dir.clone()) {
            continue;
        }
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
        for (name, path) in linked_dependencies(&manifest) {
            let candidate = match path {
                Some(path) => dir.join(path),
                None => root.join("crates").join(name),
            };
            if candidate.join("Cargo.toml").is_file() {
                pending.push(candidate);
            }
        }
    }
    found
}

/// The dependencies a manifest's `[dependencies]`, `[build-dependencies]` and target-specific
/// dependency tables declare, each with its `path` if it gives one. Dev-dependencies are left
/// out. Reads one dependency per line, the form this workspace's manifests use.
fn linked_dependencies(manifest: &str) -> Vec<(String, Option<String>)> {
    let mut linked = false;
    let mut found = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            let table = line.trim_matches(|c| c == '[' || c == ']');
            linked = table == "dependencies"
                || table == "build-dependencies"
                || (table.starts_with("target.")
                    && (table.ends_with(".dependencies")
                        || table.ends_with(".build-dependencies")));
            continue;
        }
        if !linked || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let name = key.trim().split('.').next().unwrap_or_default().trim();
        if name.is_empty() {
            continue;
        }
        let path = value.split_once("path").and_then(|(_, rest)| {
            let rest = rest.trim_start().strip_prefix('=')?.trim_start();
            let rest = rest.strip_prefix('"')?;
            Some(rest.split('"').next()?.to_owned())
        });
        found.push((name.to_owned(), path));
    }
    found
}

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn linked_dependencies_leave_out_dev_dependencies() {
        let manifest = "[package]\nname = \"a\"\n\n[dependencies]\nb.workspace = true\n\
                        c = { path = \"../c\" }\nserde = \"1\"\n# d = \"1\"\n\n\
                        [target.'cfg(unix)'.dependencies]\ne = { workspace = true }\n\n\
                        [build-dependencies]\nf.workspace = true\n\n[dev-dependencies]\n\
                        g.workspace = true\n";
        assert_eq!(
            linked_dependencies(manifest),
            [
                ("b".to_owned(), None),
                ("c".to_owned(), Some("../c".to_owned())),
                ("serde".to_owned(), None),
                ("e".to_owned(), None),
                ("f".to_owned(), None),
            ]
        );
    }

    #[test]
    fn the_agents_crates_are_its_linked_workspace_dependencies() {
        let names: BTreeSet<String> = crates(&root(), "agent")
            .iter()
            .map(|dir| dir.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        for name in ["agent", "diag-ir", "worker-host", "vci-service-interface"] {
            assert!(names.contains(name), "{name} in {names:?}");
        }
        // Dev-dependencies of the agent only.
        for name in ["sim-ecu", "diag-frontend"] {
            assert!(!names.contains(name), "{name} in {names:?}");
        }
        let files = sources(&root(), "agent");
        assert!(files.iter().any(|file| file.ends_with("agent/src/main.rs")));
        assert!(files.iter().any(|file| file.ends_with("diag-ir/build.rs")));
    }

    #[test]
    fn a_source_newer_than_the_binary_is_out_of_date() {
        let dir = std::env::temp_dir().join(format!("ngr-agent-binary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        let source = dir.join("a.rs");
        std::fs::write(&source, "").expect("a source file");
        let time = modified(&source).expect("its modification time");
        let files = [source.clone(), dir.join("gone.rs")];
        assert!(newer_than(&files, time).is_empty());
        let earlier = time - std::time::Duration::from_secs(1);
        assert_eq!(newer_than(&files, earlier), [source]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
