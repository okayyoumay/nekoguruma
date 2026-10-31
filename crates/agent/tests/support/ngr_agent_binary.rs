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
//! The lookup therefore checks the binary against the files it is compiled from: those the
//! compiler's dep-info files list for the binary (`deps/ngr_agent-<hash>.d`) and for the library
//! of the `agent` package and of every workspace crate it depends on (its `[dependencies]`,
//! target-specific ones and `[build-dependencies]`, followed through the crates' manifests; not
//! its dev-dependencies, which the binary does not link), with each crate's build script. A
//! library's dep-info is the one whose rules name its `.rlib`, so a file only a test build
//! compiles (a `#[cfg(test)]` module) or another binary's source is not counted; of several
//! builds of one library, the newest is read. A listed file newer than the binary is one cargo
//! would rebuild it for, so building the binary clears the failure. A test build writes no
//! top-level dep-info file for the binary (`target/debug/ngr-agent.d`, which only `cargo build`
//! writes), and the binary's own one lists none of the library's files, hence the libraries'.

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
    let root = workspace_root();
    let deps = exe.parent().expect("the deps directory");
    let files = sources(&root, deps, "agent");
    assert!(
        !files.is_empty(),
        "no dep-info file in {} lists the agent's sources: {rebuild}",
        deps.display()
    );
    let stale = newer_than(&files, built);
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

/// The workspace's root directory, from the including package's `crates/<package>`.
fn workspace_root() -> PathBuf {
    normalized(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

/// `path` with its `.` and `..` components resolved without the file system. Not
/// `canonicalize`, whose Windows form (`\\?\`) takes no `/` in a path joined to it.
fn normalized(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
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

/// The files the binary `ngr-agent` and the libraries of `crates/<package>` under `root` and of
/// the workspace crates it depends on were compiled from, by the newest dep-info of each in
/// `deps`, with each crate's `build.rs`. A relative path is taken from `root`, where cargo runs the
/// compiler for workspace members.
fn sources(root: &Path, deps: &Path, package: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut add = |listed: Vec<PathBuf>| {
        files.extend(listed.into_iter().map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        }));
    };
    add(newest_dep_info(deps, "ngr_agent", None));
    for dir in crates(root, package) {
        let build = dir.join("build.rs");
        if build.is_file() {
            add(vec![build]);
        }
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().replace('-', "_"))
            .unwrap_or_default();
        add(newest_dep_info(deps, &name, Some("rlib")));
    }
    files
}

/// The sources the newest `deps/<name>-<hash>.d` lists. With `artifact`, only a dep-info whose
/// rules name `lib<name>-<hash>.<artifact>` counts: a library's, not a test build's of it.
fn newest_dep_info(deps: &Path, name: &str, artifact: Option<&str>) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(deps) else {
        return Vec::new();
    };
    let mut newest: Option<(SystemTime, String)> = None;
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(hash) = file
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('-'))
            .and_then(|rest| rest.strip_suffix(".d"))
        else {
            continue;
        };
        if !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(artifact) = artifact {
            let target = format!("lib{name}-{hash}.{artifact}:");
            if !text.lines().any(|line| line.contains(&target)) {
                continue;
            }
        }
        let Some(time) = modified(&entry.path()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(best, _)| time > *best) {
            newest = Some((time, text));
        }
    }
    newest
        .map(|(_, text)| listed_sources(&text))
        .unwrap_or_default()
}

/// The sources a dep-info file lists: the compiler writes an empty rule (`path:`) for each, after
/// the rules that name its outputs. A Windows path's drive letter is followed by no space.
fn listed_sources(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.starts_with('#') && !line.contains(": "))
        .filter_map(|line| line.strip_suffix(':'))
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(path.replace("\\ ", " ")))
        .collect()
}

/// The directories of `crates/<package>` and of every workspace crate its manifest's linked
/// dependencies reach. A dependency is a workspace crate when it names a `path`, or when
/// `crates/<name>` holds a manifest (the workspace's dependency table names each member at that
/// path).
fn crates(root: &Path, package: &str) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::new();
    let mut pending = vec![root.join("crates").join(package)];
    while let Some(dir) = pending.pop() {
        let dir = normalized(&dir);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn root() -> PathBuf {
        workspace_root()
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
    }

    #[test]
    fn the_listed_sources_are_the_empty_rules() {
        let text = "/t/deps/agent-0a.d: crates/agent/src/lib.rs crates/agent/src/a.rs\n\n\
                    /t/deps/libagent-0a.rlib: crates/agent/src/lib.rs crates/agent/src/a.rs\n\n\
                    crates/agent/src/lib.rs:\ncrates/agent/src/a.rs:\n\n\
                    # env-dep:CARGO_PKG_VERSION=0.1.0\n";
        assert_eq!(
            listed_sources(text),
            [
                PathBuf::from("crates/agent/src/lib.rs"),
                PathBuf::from("crates/agent/src/a.rs")
            ]
        );
        assert_eq!(
            listed_sources("D:\\t\\deps\\agent-0a.d: D:\\w\\a.rs\n\nD:\\w\\a.rs:\n"),
            [PathBuf::from("D:\\w\\a.rs")]
        );
    }

    #[test]
    fn a_library_is_read_from_its_rlibs_dep_info_not_a_test_builds() {
        let dir = std::env::temp_dir().join(format!("ngr-agent-dep-info-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        std::fs::write(
            dir.join("agent-0a.d"),
            "/t/deps/libagent-0a.rlib: src/lib.rs\n\nsrc/lib.rs:\n",
        )
        .expect("the library's dep-info");
        // A test build of the library, written later, lists its test-only module too.
        std::fs::write(
            dir.join("agent-1b.d"),
            "/t/deps/agent-1b: src/lib.rs src/tests.rs\n\nsrc/lib.rs:\nsrc/tests.rs:\n",
        )
        .expect("the test build's dep-info");
        let later = modified(&dir.join("agent-0a.d")).expect("a time") + Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(dir.join("agent-1b.d"))
            .and_then(|file| file.set_modified(later))
            .expect("the test build's dep-info is the newer");
        assert_eq!(
            newest_dep_info(&dir, "agent", Some("rlib")),
            [PathBuf::from("src/lib.rs")]
        );
        assert_eq!(
            newest_dep_info(&dir, "agent", None).len(),
            2,
            "without an artifact the newest counts"
        );
        // Another crate whose name starts with this one's is not read.
        assert!(newest_dep_info(&dir, "agen", Some("rlib")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_binarys_sources_are_found_in_this_build() {
        let exe = std::env::current_exe().expect("test executable path");
        let files = sources(&root(), exe.parent().unwrap(), "agent");
        assert!(
            files
                .iter()
                .any(|file| file.ends_with("crates/agent/src/main.rs"))
        );
        assert!(
            files
                .iter()
                .any(|file| file.ends_with("crates/agent/src/comm_log.rs"))
        );
        assert!(
            files
                .iter()
                .any(|file| file.ends_with("crates/diag-ir/build.rs"))
        );
        // A module only the library's own tests compile.
        assert!(
            !files
                .iter()
                .any(|file| file.ends_with("identification/run_tests.rs"))
        );
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
