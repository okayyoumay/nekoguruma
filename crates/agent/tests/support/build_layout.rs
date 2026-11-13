//! Finds build outputs under a Cargo target directory without assuming where Cargo puts them,
//! shared through `#[path]` by the tests that need another package's artifact or dep-info file
//! (`ngr_agent_binary.rs`, `crates/j2534-0404-service/tests/common/artifacts.rs`).
//!
//! Cargo has changed this layout (Rust 1.100 moved each unit's outputs from `<profile>/deps` to
//! `<profile>/build/<package>/<hash>/out`), so nothing here names a subdirectory. The lookup
//! starts from the running test executable: the target directory's root is the nearest ancestor
//! holding the `CACHEDIR.TAG` file Cargo writes there, the search scope is the profile directory
//! below it (`target/debug`, or `target/<triple>/debug` for a cross build), and a file is found
//! by name anywhere under the scope, to a bounded depth, skipping directories that hold no
//! outputs of interest. Of several matches the newest is taken (a library can have a build per
//! hash in any layout).
//!
//! The rules are unit-tested once, in `crates/j2534-0404-service/tests/artifact_layout.rs`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How many ancestors of the executable are searched for the target directory's root.
const ROOT_DEPTH: usize = 8;
/// How deep below the scope a file is looked for.
const SEARCH_DEPTH: usize = 8;
/// Directories under the scope that hold no outputs to look for (and many files).
const SKIPPED: [&str; 3] = ["incremental", ".fingerprint", "tmp"];

/// The directory a build's outputs are searched in: the child of the target directory's root
/// that holds `exe`, and below a target-triple directory (`x86_64-unknown-linux-gnu`) its child
/// on the way to `exe`. Panics, naming `exe`, when no ancestor within a few levels is a target
/// directory root.
pub fn target_scope(exe: &Path) -> PathBuf {
    let root = exe
        .ancestors()
        .skip(1)
        .take(ROOT_DEPTH)
        .find(|dir| dir.join("CACHEDIR.TAG").is_file())
        .unwrap_or_else(|| {
            panic!(
                "no CACHEDIR.TAG within {ROOT_DEPTH} directories above the test executable {}: \
                 it is not in a cargo target directory",
                exe.display()
            )
        });
    let mut parts = exe
        .strip_prefix(root)
        .expect("the root is an ancestor of the executable")
        .components();
    let mut scope = root.to_path_buf();
    let child = parts.next().expect("the executable is below the root");
    scope.push(child);
    if is_target_triple(&child.as_os_str().to_string_lossy())
        && let Some(profile) = parts.next().filter(|_| parts.clone().next().is_some())
    {
        scope.push(profile);
    }
    scope
}

/// Whether a directory name is a target triple (`arch-vendor-os[-env]`) rather than a profile.
fn is_target_triple(name: &str) -> bool {
    name.split('-').count() >= 3
}

/// Every file under `scope` whose file name `wanted` accepts.
pub fn files_in(scope: &Path, wanted: &dyn Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(scope, SEARCH_DEPTH, wanted, &mut found);
    found
}

fn walk(dir: &Path, depth: usize, wanted: &dyn Fn(&str) -> bool, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if is_dir {
            if depth > 1 && !SKIPPED.contains(&name.as_str()) {
                walk(&entry.path(), depth - 1, wanted, found);
            }
        } else if wanted(&name) && entry.path().is_file() {
            found.push(entry.path());
        }
    }
}

/// The most recently modified of `files`.
pub fn newest(files: Vec<PathBuf>) -> Option<PathBuf> {
    files
        .into_iter()
        .filter_map(|path| {
            let time = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            Some((time, path))
        })
        .max_by_key(|(time, _): &(SystemTime, PathBuf)| *time)
        .map(|(_, path)| path)
}

/// The newest file called `file` under the scope of the test executable `exe`.
pub fn find_artifact(exe: &Path, file: &str) -> Option<PathBuf> {
    newest(files_in(&target_scope(exe), &|name| name == file))
}
