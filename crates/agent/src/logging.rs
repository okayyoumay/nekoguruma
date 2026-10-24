//! The agent's operational log (design 12.2, ADR-348 items 3, 6 and 9): its `tracing`
//! subscriber, with fixed defaults until the agent reads logging settings of its own.
//!
//! - level `info`, and after it a safety filter that drops the targets of any workspace crate
//!   whose `tracing` calls have not been checked against `docs/operational-log.md` (none today);
//!   neither a configuration nor a level raise changes that filter (ADR-348 item 9);
//! - stderr as text, each `external` field written as its length only when stderr is not a
//!   terminal, since the OS may forward it (ADR-348 item 6);
//! - and the rotated JSON Lines file (`ngr_oplog`) in the user's state directory, one per
//!   running agent ([`ngr_oplog::Slot`]); when it cannot be opened the agent logs to stderr alone,
//!   with a warning, and that stderr, the fallback of ADR-348 item 5, writes `external` fields as
//!   their length only even on a terminal.
//!
//! Each job runs in a [`job_span`] with its job id, ownership generation and VCI name.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use ngr_oplog::{ExternalText, FileSettings, JsonLayer, RotatingFile, Slot, TextLayer};
use tracing::Subscriber;
use tracing_subscriber::filter::{FilterFn, LevelFilter};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;

use crate::journal::JobKey;
use crate::oplog::External;

/// The `tracing` targets the safety filter drops: those of a workspace crate whose calls reach
/// the agent's subscriber but have not been checked against `docs/operational-log.md` yet. A
/// crate is removed from the list once it is checked. Every crate the agent links is checked.
const UNCHECKED_TARGETS: &[&str] = &[];

/// The process name in the file's entries (design 12.2).
const PROCESS: &str = "agent";

/// The installed operational log. Dropping it releases the file's slot; the agent keeps it for
/// its lifetime.
#[derive(Debug)]
pub struct Logging {
    /// The file the log is written to, or why there is none.
    pub file: Result<PathBuf, NoFile>,
    _slot: Option<Slot>,
}

/// Why the agent has no log file: NGR's own words, and the path concerned, which may name a
/// user's directory and so goes in the warning's `external` field (ADR-348 item 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoFile {
    pub why: String,
    pub path: Option<PathBuf>,
}

/// Installs the agent's subscriber as the process's global default. Entries go to stderr and,
/// unless it cannot be opened, to the rotated file in [`default_dir`]. Call once, at startup.
pub fn install() -> Logging {
    let (logging, json, external) = setup(default_dir(), std::io::stderr().is_terminal());
    let installed =
        tracing::subscriber::set_global_default(subscriber(std::io::stderr, external, json))
            .is_ok();
    if installed && let Err(no_file) = &logging.file {
        warn_no_file(no_file);
    }
    logging
}

/// The file's output and how stderr writes `external` fields, for the state directory `dir`.
/// Without the file, stderr is the fallback ADR-348 item 5 names, which item 6 treats as one
/// the OS may forward, so it writes `external` as its length only even on a terminal.
fn setup(
    dir: Result<PathBuf, NoFile>,
    stderr_is_terminal: bool,
) -> (Logging, Option<JsonLayer>, ExternalText) {
    match dir.and_then(|dir| open_file(&dir)) {
        Ok((slot, file)) => (
            Logging {
                file: Ok(slot.path().to_owned()),
                _slot: Some(slot),
            },
            Some(JsonLayer::new(PROCESS, file)),
            if stderr_is_terminal {
                ExternalText::Full
            } else {
                ExternalText::LengthOnly
            },
        ),
        Err(no_file) => (
            Logging {
                file: Err(no_file),
                _slot: None,
            },
            None,
            ExternalText::LengthOnly,
        ),
    }
}

/// Logs that the file could not be opened.
fn warn_no_file(no_file: &NoFile) {
    tracing::warn!(
        error = %no_file.why,
        external = no_file.path.as_ref().map(|path| tracing::field::display(External(path.display()))),
        "the operational log file could not be opened; logging to stderr only"
    );
}

/// The subscriber: the level filter, then the safety filter, then the text output on `stderr`
/// and, when given, the file's JSON output.
pub fn subscriber<W>(
    stderr: W,
    external: ExternalText,
    file: Option<JsonLayer>,
) -> impl Subscriber + for<'a> LookupSpan<'a> + Send + Sync
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::registry()
        .with(LevelFilter::INFO)
        .with(FilterFn::new(|metadata| !is_unchecked(metadata.target())))
        .with(TextLayer::new(stderr, external))
        .with(file)
}

/// Whether `target` belongs to a crate the safety filter drops.
fn is_unchecked(target: &str) -> bool {
    in_crates(UNCHECKED_TARGETS, target)
}

/// Whether `target` is one of `crates` or a module of one: `foo` and `foo::bar` for `foo`, not
/// `foo_bar`.
fn in_crates(crates: &[&str], target: &str) -> bool {
    crates.iter().any(|crate_name| {
        target == *crate_name
            || target
                .strip_prefix(crate_name)
                .is_some_and(|rest| rest.starts_with("::"))
    })
}

/// The span a job runs in: its job id and ownership generation, and the VCI name, which may
/// identify a device, in its `external` field (ADR-348 items 3 and 6).
pub fn job_span(key: &JobKey, vci: &str) -> tracing::Span {
    tracing::info_span!(
        "job",
        job_id = %key.job_id.0,
        generation = key.generation,
        external = %External(vci),
    )
}

/// Claims the agent's slot in `dir` and opens its file.
fn open_file(dir: &Path) -> Result<(Slot, RotatingFile), NoFile> {
    let slot = Slot::claim(dir, PROCESS).map_err(|error| NoFile {
        why: format!("no slot in the log directory: {}", io_kind(&error)),
        path: Some(dir.to_owned()),
    })?;
    let file = RotatingFile::open(FileSettings::new(slot.path())).map_err(|error| NoFile {
        why: match &error {
            ngr_oplog::OpenError::Dir(io) => format!("the log directory: {}", io_kind(io)),
            ngr_oplog::OpenError::File(io) => format!("the log file: {}", io_kind(io)),
        },
        path: Some(slot.path().to_owned()),
    })?;
    Ok((slot, file))
}

/// An I/O error as its kind and OS code, which hold no path or other outside text.
fn io_kind(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => format!("{:?} (OS error {code})", error.kind()),
        None => format!("{:?}", error.kind()),
    }
}

/// The directory of the agent's log when run by a user (design 12.2): the user's state
/// directory, `$XDG_STATE_HOME/nekoguruma/logs` (an absolute `XDG_STATE_HOME`, else
/// `~/.local/state`) on Unix, `%LOCALAPPDATA%\nekoguruma\logs` on Windows.
pub fn default_dir() -> Result<PathBuf, NoFile> {
    state_dir(|name| std::env::var_os(name))
        .map(|dir| dir.join("nekoguruma").join("logs"))
        .map_err(|why| NoFile { why, path: None })
}

/// The user's state directory from the environment `var` reads.
fn state_dir(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<PathBuf, String> {
    let absolute = |name: &str| {
        var(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    if cfg!(windows) {
        absolute("LOCALAPPDATA").ok_or_else(|| "LOCALAPPDATA is not set".to_owned())
    } else {
        absolute("XDG_STATE_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".local").join("state")))
            .ok_or_else(|| "neither XDG_STATE_HOME nor HOME is set".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A writer that keeps what is written, standing in for stderr.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn key() -> JobKey {
        JobKey {
            job_id: shared_proto::JobId("run-1-2-3".to_owned()),
            generation: 4,
        }
    }

    /// A `warn!` of the runner inside a job reaches stderr and the file, each with the job's
    /// span fields, the VCI name in the span's `external` field; a debug entry reaches neither.
    #[test]
    fn a_runner_warning_reaches_stderr_and_the_file_with_the_job_s_span() {
        let temp = tempfile::tempdir().unwrap();
        let (slot, file) = open_file(&temp.path().join("logs")).unwrap();
        let stderr = Captured::default();
        let subscriber = subscriber(
            stderr.clone(),
            ExternalText::Full,
            Some(JsonLayer::new(PROCESS, file)),
        );
        tracing::subscriber::with_default(subscriber, || {
            let _job = job_span(&key(), "bench VCI").entered();
            tracing::warn!(target: "agent::runner", pc = 7_u32, "could not close the link");
            tracing::debug!(target: "agent::runner", "not at the default level");
        });
        let text = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.contains(
                r#"WARN agent::runner: job{generation=4 job_id="run-1-2-3" external="bench VCI"}: could not close the link pc=7"#
            ),
            "{text}"
        );
        let json = std::fs::read_to_string(slot.path()).unwrap();
        let entries: Vec<serde_json::Value> = json
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(entries.len(), 1, "{json}");
        let entry = &entries[0];
        assert_eq!(entry["process"], "agent");
        assert_eq!(entry["target"], "agent::runner");
        assert_eq!(entry["level"], "WARN");
        assert_eq!(entry["fields"], serde_json::json!({"pc": 7}));
        assert_eq!(
            entry["spans"],
            serde_json::json!([{
                "name": "job",
                "fields": {"job_id": "run-1-2-3", "generation": 4},
                "external": "bench VCI",
            }])
        );
        assert_eq!(slot.path(), temp.path().join("logs").join("agent.0.log"));
    }

    /// Without the file, stderr is the fallback, which writes `external` as its length even on a
    /// terminal; the warning says why in NGR's words and puts the path in `external`.
    #[test]
    fn without_the_file_stderr_warns_and_writes_external_as_its_length() {
        let temp = tempfile::tempdir().unwrap();
        // A file where the log directory should be.
        let dir = temp.path().join("logs");
        std::fs::write(&dir, b"").unwrap();
        let (logging, json, external) = setup(Ok(dir.clone()), true);
        assert!(json.is_none());
        assert_eq!(external, ExternalText::LengthOnly);
        let no_file = logging.file.unwrap_err();
        assert_eq!(no_file.path.as_deref(), Some(dir.as_path()));
        let stderr = Captured::default();
        tracing::subscriber::with_default(subscriber(stderr.clone(), external, None), || {
            warn_no_file(&no_file)
        });
        let text = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains("logging to stderr only error=\"no slot in the log directory: "),
            "{text}"
        );
        assert!(!text.contains(temp.path().to_str().unwrap()), "{text}");
        let length = dir.display().to_string().len();
        assert!(
            text.ends_with(&format!("external=<{length} bytes>\n")),
            "{text}"
        );
        // With no state directory at all there is no path to name.
        let no_dir = NoFile {
            why: "neither XDG_STATE_HOME nor HOME is set".to_owned(),
            path: None,
        };
        let (logging, _, _) = setup(Err(no_dir), true);
        assert_eq!(logging.file.unwrap_err().path, None);
        // With the file, a terminal's stderr writes `external` in full.
        let (logging, json, external) = setup(Ok(temp.path().join("ok")), true);
        assert!(logging.file.is_ok() && json.is_some());
        assert_eq!(external, ExternalText::Full);
    }

    /// A stderr that is not a terminal writes `external` fields as their length only.
    #[test]
    fn a_forwarded_stderr_writes_external_as_its_length() {
        let stderr = Captured::default();
        let subscriber = subscriber(stderr.clone(), ExternalText::LengthOnly, None);
        tracing::subscriber::with_default(subscriber, || {
            let _job = job_span(&key(), "bench VCI").entered();
            tracing::warn!(external = "vendor text", "a worker said something");
        });
        let text = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
        assert!(
            !text.contains("bench") && !text.contains("vendor"),
            "{text}"
        );
        assert!(text.contains("external=<9 bytes>}"), "{text}");
        assert!(text.ends_with("external=<11 bytes>\n"), "{text}");
    }

    /// The safety filter matches a crate's own targets, not another crate whose name starts
    /// the same.
    #[test]
    fn the_safety_filter_matches_whole_crate_targets() {
        assert!(in_crates(&["foo"], "foo"));
        assert!(in_crates(&["foo"], "foo::bar"));
        assert!(!in_crates(&["foo"], "foo_bar"));
        assert!(!in_crates(&["foo"], "foobar::x"));
        // Every crate the agent links is checked today.
        assert!(UNCHECKED_TARGETS.is_empty());
        assert!(!is_unchecked("agent::runner"));
    }

    /// The state directory comes from an absolute `XDG_STATE_HOME`, else `HOME` (Unix), or
    /// `LOCALAPPDATA` (Windows).
    #[test]
    fn the_state_directory_comes_from_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| std::ffi::OsString::from(value))
            }
        };
        if cfg!(windows) {
            assert_eq!(
                state_dir(env(&[("LOCALAPPDATA", r"C:\Users\a\AppData\Local")])).unwrap(),
                PathBuf::from(r"C:\Users\a\AppData\Local")
            );
            assert!(state_dir(env(&[])).is_err());
        } else {
            assert_eq!(
                state_dir(env(&[("XDG_STATE_HOME", "/x/state"), ("HOME", "/home/a")])).unwrap(),
                PathBuf::from("/x/state")
            );
            assert_eq!(
                state_dir(env(&[("XDG_STATE_HOME", "relative"), ("HOME", "/home/a")])).unwrap(),
                PathBuf::from("/home/a/.local/state")
            );
            assert!(state_dir(env(&[])).is_err());
        }
    }
}
