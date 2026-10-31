//! The `ngr-agent` binary for the end-to-end tests of the worker packages, shared through
//! `#[path]`: `crates/j2534-0404-service/tests/ngr_agent_end_to_end.rs`,
//! `crates/iso22900-service/tests/ngr_agent_dpdu_end_to_end.rs` and
//! `crates/iso22900-service/tests/ngr_agent_definition_end_to_end.rs`.
//!
//! `ngr-agent` belongs to another package, so cargo builds it only when the run builds the
//! `agent` package's binaries (as `cargo test --workspace` and CI do), and a run of the worker
//! package alone, or one that selects its tests with `--test`, uses whatever binary an earlier
//! build left. That binary can predate the sources the test
//! was built from, and a test then fails on what the older agent does rather than on the change.
//! The lookup therefore checks the binary against its sources, the way cargo itself decides
//! whether to rebuild it: the dep-info file cargo writes beside it (`ngr-agent.d`) lists every
//! source file it was built from, and a listed file that is newer than the binary, or no longer
//! exists, means the binary is out of date.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The `ngr-agent` binary next to the test's `deps` directory, after checking that it is built and
/// current. `package` is the worker package under test, named in the failure message.
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
    let dep_info = dir.join("ngr-agent.d");
    let text = std::fs::read_to_string(&dep_info).unwrap_or_else(|error| {
        panic!(
            "{} cannot be read ({error}), so {} cannot be checked against its sources: {rebuild}",
            dep_info.display(),
            path.display()
        )
    });
    let built = modified(&path).expect("the binary's modification time");
    let stale = out_of_date(&text, built);
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

/// The sources listed in the dep-info `text` that are newer than `built`, or missing. A relative
/// path is taken from the workspace root.
fn out_of_date(text: &str, built: SystemTime) -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    sources(text)
        .into_iter()
        .map(|source| {
            if source.is_absolute() {
                source
            } else {
                root.join(source)
            }
        })
        .filter(|source| modified(source).is_none_or(|time| time > built))
        .collect()
}

/// The prerequisites of the first rule of a dep-info file: the text after the target's `": "`,
/// split at spaces, where `"\ "` is a space inside a path. The target may hold a drive letter's
/// colon, which no space follows.
fn sources(text: &str) -> Vec<PathBuf> {
    let line = text.lines().next().unwrap_or_default();
    let Some((_, prerequisites)) = line.split_once(": ") else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut chars = prerequisites.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                current.push(' ');
                chars.next();
            }
            ' ' => {
                if !current.is_empty() {
                    paths.push(PathBuf::from(std::mem::take(&mut current)));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        paths.push(PathBuf::from(current));
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sources_are_the_first_rules_prerequisites() {
        let text = "/t/debug/ngr-agent: /w/a.rs /w/with\\ space.rs  /w/b.rs\n\n/w/a.rs:\n";
        assert_eq!(
            sources(text),
            [
                PathBuf::from("/w/a.rs"),
                PathBuf::from("/w/with space.rs"),
                PathBuf::from("/w/b.rs")
            ]
        );
        // A Windows target's drive letter is not the rule's colon.
        assert_eq!(
            sources("D:\\t\\debug\\ngr-agent.exe: D:\\w\\a.rs D:\\w\\b.rs"),
            [PathBuf::from("D:\\w\\a.rs"), PathBuf::from("D:\\w\\b.rs")]
        );
        assert!(sources("").is_empty());
    }

    #[test]
    fn a_newer_or_missing_source_is_out_of_date() {
        let dir = std::env::temp_dir().join(format!("ngr-agent-binary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        let source = dir.join("a.rs");
        std::fs::write(&source, "").expect("a source file");
        let time = modified(&source).expect("its modification time");
        let missing = dir.join("gone.rs");
        let escaped = |path: &Path| path.display().to_string().replace(' ', "\\ ");
        let text = format!("bin: {} {}", escaped(&source), escaped(&missing));
        assert_eq!(out_of_date(&text, time), std::slice::from_ref(&missing));
        let earlier = time - std::time::Duration::from_secs(1);
        assert_eq!(out_of_date(&text, earlier), [source, missing]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
