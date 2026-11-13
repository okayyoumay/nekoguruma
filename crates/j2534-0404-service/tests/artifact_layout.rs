//! The lookup of build outputs the end-to-end tests share
//! (`crates/agent/tests/support/build_layout.rs`) finds a file whatever layout cargo gives its
//! target directory: `<profile>/deps`, Rust 1.100's `<profile>/build/<package>/<hash>/out`, a
//! cross build's `<triple>/<profile>`, and a deeper layout neither has used.

#[path = "common/artifacts.rs"]
mod artifacts;

#[path = "../../agent/tests/support/build_layout.rs"]
mod build_layout;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use build_layout::{find_artifact, target_scope};

/// A fake target directory (with the `CACHEDIR.TAG` cargo writes at its root) in a temporary
/// directory.
fn target(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ngr-layout-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    std::fs::write(dir.join("CACHEDIR.TAG"), "").expect("the tag");
    dir
}

fn touch(path: &Path, age_secs: u64) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
    std::fs::write(path, "").expect("a file");
    let time = SystemTime::now() - Duration::from_secs(age_secs);
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(time))
        .expect("the modification time");
}

#[test]
fn the_scope_is_the_profile_directory_or_a_cross_builds_below_its_triple() {
    let root = target("scope");
    assert_eq!(
        target_scope(&root.join("debug/deps/e2e-0a")),
        root.join("debug")
    );
    assert_eq!(
        target_scope(&root.join("debug/build/pkg/0a/out/e2e-0a")),
        root.join("debug")
    );
    assert_eq!(
        target_scope(&root.join("x86_64-pc-windows-gnullvm/debug/deps/e2e-0a")),
        root.join("x86_64-pc-windows-gnullvm/debug")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[should_panic(expected = "not in a cargo target directory")]
fn an_executable_outside_a_target_directory_is_refused() {
    let root = std::env::temp_dir().join("ngr-layout-none/a/b/c/e2e");
    target_scope(&root);
}

#[test]
fn a_file_is_found_in_the_older_layout() {
    let root = target("older");
    let exe = root.join("debug/deps/e2e-0a");
    touch(&root.join("debug/deps/libx.so"), 0);
    assert_eq!(
        find_artifact(&exe, "libx.so"),
        Some(root.join("debug/deps/libx.so"))
    );
    assert_eq!(find_artifact(&exe, "liby.so"), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_file_is_found_in_the_newer_layout_and_the_newest_wins() {
    let root = target("newer");
    let exe = root.join("debug/build/j2534-0404-service/0a/out/e2e-0a");
    touch(&root.join("debug/build/x/1b/out/libx.so"), 100);
    touch(&root.join("debug/build/x/2c/out/libx.so"), 10);
    assert_eq!(
        find_artifact(&exe, "libx.so"),
        Some(root.join("debug/build/x/2c/out/libx.so"))
    );
    // An uplifted copy in the profile directory counts too, and a release build's does not.
    touch(&root.join("debug/libx.so"), 1);
    touch(&root.join("release/libx.so"), 0);
    assert_eq!(
        find_artifact(&exe, "libx.so"),
        Some(root.join("debug/libx.so"))
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_file_is_found_in_a_layout_not_seen_before() {
    let root = target("unknown");
    let exe = root.join("debug/v9/units/j2534/0a/bin/e2e-0a");
    touch(&root.join("debug/v9/units/x/1b/lib/native/libx.so"), 0);
    assert_eq!(
        find_artifact(&exe, "libx.so"),
        Some(root.join("debug/v9/units/x/1b/lib/native/libx.so"))
    );
    // The incremental-compilation and fingerprint directories are not searched.
    touch(&root.join("debug/incremental/libz.so"), 0);
    touch(&root.join("debug/.fingerprint/libz.so"), 0);
    assert_eq!(find_artifact(&exe, "libz.so"), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_simulators_library_is_found_for_this_build() {
    let path = artifacts::sim_vci_path();
    assert!(path.is_file(), "{}", path.display());
}
