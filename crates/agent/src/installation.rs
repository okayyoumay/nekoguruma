//! The agent's installation file `agent.toml` (design 6.2, 6.7, ADR-494): what the installation
//! decides for this device, read by `ngr-agent run`, `identify`, `identity`, `register` and
//! `connect` once, after the operational log starts and before the key, a lock, a worker or a
//! connection (`run` and `identify` use only its mode).
//!
//! The file lives beside the worker configuration file under the fixed system directory
//! ([`vci_service_config::agent_installation_file`]) and passes the design 7.2 writability check
//! ([`Policy::system`]) in both operating modes: a file, a link or a directory above it that a
//! regular user can change is refused, so a user-mode account cannot change what its own agent
//! installed. Only a debug build with the runtime location override also trusts the current user
//! as owner, so tests can use files they own.
//!
//! The keys, all strings:
//!
//! | Key | Rule |
//! |---|---|
//! | `mode` | required: `device` or `user` |
//! | `server_url` | required: the pinned origin of [`ServerOrigin::parse`] |
//! | `distribution_root_pem` | required: one P-256 public key in SPKI PEM form, parsed only and its fingerprint logged |
//! | `profile` | optional: `local` (the default); `standard` is refused until it exists |
//! | `startup` | optional: `explicit` or `auto`; `auto` for a device, `explicit` for a user; `explicit` with `device` is refused |
//! | `lock_dir` | optional: an absolute path, validated only |
//!
//! An unknown key is refused. The first problem found decides, in this order: the syntax, an
//! unknown key, then `mode`, `server_url`, `distribution_root_pem`, `profile`, `startup` and
//! `lock_dir`.
//!
//! The file is read as bytes, at most [`MAX_SIZE`], checked as UTF-8 (a leading byte order mark
//! is accepted) and parsed to a [`toml::Table`], which is validated by hand: no message repeats
//! a value from the file, and `toml`'s own error text, which quotes the line, is never used.
//! The exit status of every error is [`InstallationError::exit_code`]: 1 for a file the agent
//! cannot use or open, 3 for another read error, which a retry may clear.

use std::io::Read;
use std::path::{Path, PathBuf};

use ngr_library_resolver::Policy;
use shared_crypto::PublicKey;
use worker_host::service::OperatingMode;

use crate::registration::origin::ServerOrigin;

/// The largest file the agent reads: an installation file is a few hundred bytes.
pub const MAX_SIZE: u64 = 64 * 1024;

/// The keys the file may hold.
const KEYS: [&str; 6] = [
    "mode",
    "server_url",
    "distribution_root_pem",
    "profile",
    "startup",
    "lock_dir",
];

/// The longest key name a message repeats.
const MAX_KEY_NAME: usize = 32;

/// The deployment profile (design 13.3). Only the local profile exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Local,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Local => "local",
        }
    }
}

/// How the agent is started (design 6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Startup {
    /// The user starts it.
    Explicit,
    /// The service manager starts it at boot or login.
    Auto,
}

impl Startup {
    pub fn as_str(self) -> &'static str {
        match self {
            Startup::Explicit => "explicit",
            Startup::Auto => "auto",
        }
    }
}

/// A valid installation file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    mode: OperatingMode,
    server: ServerOrigin,
    distribution_root: PublicKey,
    profile: Profile,
    startup: Startup,
    lock_dir: Option<PathBuf>,
}

impl Installation {
    /// The device's operating mode.
    pub fn mode(&self) -> OperatingMode {
        self.mode
    }

    /// The server's origin: the one `register` registers with and `connect` connects to.
    pub fn server(&self) -> &ServerOrigin {
        &self.server
    }

    /// The distribution root key: parsed only, nothing verifies against it yet.
    pub fn distribution_root(&self) -> &PublicKey {
        &self.distribution_root
    }

    pub fn profile(&self) -> Profile {
        self.profile
    }

    pub fn startup(&self) -> Startup {
        self.startup
    }

    /// The configured lock directory: validated, not used yet.
    pub fn lock_dir(&self) -> Option<&Path> {
        self.lock_dir.as_deref()
    }

    /// Logs what the file decided, without the origin, which goes in `external` with the path
    /// (the origin is the operator's own text, ADR-348 item 6).
    fn log(&self, path: &Path) {
        tracing::info!(
            mode = %self.mode,
            profile = self.profile.as_str(),
            startup = self.startup.as_str(),
            distribution_root_fingerprint = %self.distribution_root.fingerprint_hex(),
            external = %crate::oplog::External(format!(
                "file {} server {}",
                path.display(),
                self.server.as_str()
            )),
            "installation file read"
        );
    }
}

/// Why the installation file cannot be used. The text is NGR's own: it names a key and the rule
/// it broke, never a value from the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstallationError {
    /// There is no file.
    #[error("there is no installation file")]
    Absent,
    /// The file, a link on the way or a directory above it fails the writability check; the
    /// findings name paths and go in the log's `external` field only.
    #[error("the installation file fails the writability check of design 7.2")]
    Refused { findings: String },
    /// The agent's account is not allowed to read it.
    #[error("this account cannot read the installation file")]
    AccessDenied,
    /// The file cannot be read for another reason, which a retry may clear.
    #[error("the installation file cannot be read: {0:?}")]
    Unreadable(std::io::ErrorKind),
    #[error("the installation file is larger than {MAX_SIZE} bytes")]
    TooLarge,
    #[error("the installation file is not UTF-8")]
    NotUtf8,
    /// A TOML syntax error, at a line of the file (none when the parser gave no position).
    #[error("the installation file is not valid TOML{}", line_text(*.line))]
    Syntax { line: Option<usize> },
    /// A key the file does not define; named only when it is a short word of lower-case
    /// letters, digits, `_` and `-`.
    #[error("the installation file has an unknown key{}", name_text(.name))]
    UnknownKey { name: Option<String> },
    #[error("the installation file lacks {key}")]
    Missing { key: &'static str },
    #[error("the installation file's {key} {rule}")]
    Invalid {
        key: &'static str,
        rule: &'static str,
    },
}

fn line_text(line: Option<usize>) -> String {
    line.map_or_else(String::new, |line| format!(" (line {line})"))
}

fn name_text(name: &Option<String>) -> String {
    name.as_ref()
        .map_or_else(String::new, |name| format!(": {name}"))
}

impl InstallationError {
    /// The exit status: 3 for a read error other than a refusal of access, which a retry may
    /// clear; 1 for everything else (a file that is absent, refused, unreadable by this account,
    /// not valid or incomplete), which no restart fixes.
    pub fn exit_code(&self) -> u8 {
        match self {
            InstallationError::Unreadable(_) => 3,
            _ => 1,
        }
    }

    /// The text the log's `external` field carries: the path and, for a refusal, the findings.
    fn external(&self, path: &Path) -> String {
        match self {
            InstallationError::Refused { findings } => {
                format!("file {}: {findings}", path.display())
            }
            _ => format!("file {}", path.display()),
        }
    }
}

/// The file's location and the owners it trusts: [`vci_service_config::agent_installation_file`]
/// and the system owners, and in a debug build with the location override also the current user.
fn location() -> (PathBuf, Policy) {
    let policy = Policy::system();
    #[cfg(debug_assertions)]
    let policy = if vci_service_config::agent_installation_file_override().is_some() {
        policy.trusting_current_user()
    } else {
        policy
    };
    (vci_service_config::agent_installation_file(), policy)
}

/// Reads the installation file of this build, and logs what it decided (or why it cannot be
/// used). Call once, after the operational log starts.
pub fn read_installed() -> Result<Installation, InstallationError> {
    let (path, policy) = location();
    match load(&path, &policy) {
        Ok(installation) => {
            installation.log(&path);
            Ok(installation)
        }
        Err(error) => {
            tracing::error!(
                error = %error,
                external = %crate::oplog::External(error.external(&path)),
                "the installation file cannot be used"
            );
            Err(error)
        }
    }
}

/// Reads and validates the installation file at `path` under `policy`.
pub fn load(path: &Path, policy: &Policy) -> Result<Installation, InstallationError> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(InstallationError::Absent);
        }
        _ => {}
    }
    ngr_library_resolver::check_files_with(&[path.to_owned()], policy).map_err(|error| {
        InstallationError::Refused {
            findings: error.to_string(),
        }
    })?;
    let bytes = read_bytes(path)?;
    parse(&bytes)
}

/// The file's bytes, at most [`MAX_SIZE`]. A file removed after the check is absent.
fn read_bytes(path: &Path) -> Result<Vec<u8>, InstallationError> {
    let read_error = |error: std::io::Error| match error.kind() {
        std::io::ErrorKind::NotFound => InstallationError::Absent,
        std::io::ErrorKind::PermissionDenied => InstallationError::AccessDenied,
        kind => InstallationError::Unreadable(kind),
    };
    let mut file = std::fs::File::open(path).map_err(read_error)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if bytes.len() as u64 > MAX_SIZE {
        return Err(InstallationError::TooLarge);
    }
    Ok(bytes)
}

/// Validates the file's `bytes`.
pub fn parse(bytes: &[u8]) -> Result<Installation, InstallationError> {
    if bytes.len() as u64 > MAX_SIZE {
        return Err(InstallationError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| InstallationError::NotUtf8)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let table: toml::Table =
        text.parse()
            .map_err(|error: toml::de::Error| InstallationError::Syntax {
                line: error.span().map(|span| {
                    let end = span.start.min(text.len());
                    text.as_bytes()[..end]
                        .iter()
                        .filter(|&&b| b == b'\n')
                        .count()
                        + 1
                }),
            })?;
    if let Some(key) = table.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return Err(InstallationError::UnknownKey {
            name: printable_key(key),
        });
    }
    let mode = parse_mode(&table)?;
    let server = parse_server(&table)?;
    let distribution_root = parse_root(&table)?;
    let profile = parse_profile(&table)?;
    let startup = parse_startup(&table, mode)?;
    let lock_dir = parse_lock_dir(&table)?;
    Ok(Installation {
        mode,
        server,
        distribution_root,
        profile,
        startup,
        lock_dir,
    })
}

/// A key name a message may repeat.
fn printable_key(key: &str) -> Option<String> {
    (!key.is_empty()
        && key.len() <= MAX_KEY_NAME
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'))
    .then(|| key.to_owned())
}

/// The key's string value; `Ok(None)` when absent, and an error when it is not a string.
fn string<'a>(
    table: &'a toml::Table,
    key: &'static str,
) -> Result<Option<&'a str>, InstallationError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(InstallationError::Invalid {
            key,
            rule: "must be a string",
        }),
    }
}

fn required<'a>(table: &'a toml::Table, key: &'static str) -> Result<&'a str, InstallationError> {
    string(table, key)?.ok_or(InstallationError::Missing { key })
}

fn parse_mode(table: &toml::Table) -> Result<OperatingMode, InstallationError> {
    match required(table, "mode")? {
        "device" => Ok(OperatingMode::Device),
        "user" => Ok(OperatingMode::User),
        _ => Err(InstallationError::Invalid {
            key: "mode",
            rule: "must be \"device\" or \"user\"",
        }),
    }
}

fn parse_server(table: &toml::Table) -> Result<ServerOrigin, InstallationError> {
    ServerOrigin::parse(required(table, "server_url")?).map_err(|_| InstallationError::Invalid {
        key: "server_url",
        rule: "must be http://127.0.0.1 with an optional port, and nothing else",
    })
}

fn parse_root(table: &toml::Table) -> Result<PublicKey, InstallationError> {
    // Line endings may be CRLF (a file saved on Windows); the PEM itself is read as one block.
    let pem = required(table, "distribution_root_pem")?.replace("\r\n", "\n");
    PublicKey::from_spki_pem(&pem).map_err(|_| InstallationError::Invalid {
        key: "distribution_root_pem",
        rule: "must be one P-256 public key in SPKI PEM form",
    })
}

fn parse_profile(table: &toml::Table) -> Result<Profile, InstallationError> {
    match string(table, "profile")? {
        None | Some("local") => Ok(Profile::Local),
        Some("standard") => Err(InstallationError::Invalid {
            key: "profile",
            rule: "standard is not supported yet",
        }),
        Some(_) => Err(InstallationError::Invalid {
            key: "profile",
            rule: "must be \"local\" or \"standard\"",
        }),
    }
}

fn parse_startup(table: &toml::Table, mode: OperatingMode) -> Result<Startup, InstallationError> {
    match (string(table, "startup")?, mode) {
        (None, OperatingMode::Device) | (Some("auto"), _) => Ok(Startup::Auto),
        (None, OperatingMode::User) | (Some("explicit"), OperatingMode::User) => {
            Ok(Startup::Explicit)
        }
        (Some("explicit"), OperatingMode::Device) => Err(InstallationError::Invalid {
            key: "startup",
            rule: "must not be \"explicit\" in device mode",
        }),
        (Some(_), _) => Err(InstallationError::Invalid {
            key: "startup",
            rule: "must be \"explicit\" or \"auto\"",
        }),
    }
}

fn parse_lock_dir(table: &toml::Table) -> Result<Option<PathBuf>, InstallationError> {
    let Some(dir) = string(table, "lock_dir")? else {
        return Ok(None);
    };
    let path = Path::new(dir);
    if dir.is_empty() || dir.contains('\0') || !path.is_absolute() {
        return Err(InstallationError::Invalid {
            key: "lock_dir",
            rule: "must be an absolute path",
        });
    }
    Ok(Some(path.to_owned()))
}

#[cfg(test)]
mod tests;
