//! The report of a hardware run (design 13.5 "Reports"): the date and time, host and OS, worker
//! ABI, the VCI's vendor, model, firmware and library version, the target class and the result
//! of each check, written as a TOML file in a directory only the running user can read.
//!
//! - [`Report::new`] starts a report for a profile; a check adds its results with
//!   [`Report::record`] and, for each device it opened, [`Report::add_device`].
//! - [`Report::write`] writes it to [`reports_dir`], the `nekoguruma/hw-reports` directory of the
//!   user's data directory; [`Report::write_to`] takes the directory, for tests.
//! - VINs and serial numbers are masked ([`Masker`]) unless the profile sets
//!   `report_full_identifiers`.
//!
//! Directories are created with mode 0700 and files with mode 0600 on Unix, in the call that
//! creates them, so there is no moment when another user can read them. On Windows the files take
//! the ACLs of the user's profile directory, which only that user and administrators read by
//! default; no ACL is set here. A directory inside this repository's work tree is refused.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use worker_host::abi::Abi;

use crate::Refusal;
use crate::device::{ReportedModule, ReportedVersion, attribute_values, coded_version};
use crate::profile::{Api, Profile, Setup, Vci};

/// The report directory below the user's data directory.
const SUBDIR: [&str; 2] = ["nekoguruma", "hw-reports"];

/// Why a report cannot be written.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("the user's data directory is unknown (no home directory in the environment)")]
    NoDataDir,
    #[error("{0} is this repository; reports are never written to it")]
    InsideRepository(PathBuf),
    #[error("{path} is readable by others (mode {mode:o}); only its owner may read it")]
    TooOpen { path: PathBuf, mode: u32 },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot encode the report: {0}")]
    Encode(String),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ReportError + '_ {
    move |source| ReportError::Io {
        path: path.to_owned(),
        source,
    }
}

/// How a check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Passed,
    Failed,
    /// The profile does not permit the check; it did not run.
    Refused,
}

/// The result of one check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub outcome: Outcome,
    /// What the check says about the outcome; empty for a pass without remarks.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// What the opened device reported, already masked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceInfo {
    pub abi: String,
    /// For the D-PDU API the vendor name `GetVersion` reports; for J2534, or when the library
    /// leaves it empty, the library the profile names.
    pub vendor: String,
    /// For the D-PDU API the hardware name `GetVersion` reports; for J2534, or when the library
    /// leaves it empty, the module as the worker reports it (`vendor_module_name`).
    pub model: String,
    pub firmware_version: String,
    pub library_version: String,
}

/// Replaces VINs and serial numbers with a mask. A masked value is `****` and, for a value of at
/// least 8 characters, its last 4; a shorter value is fully masked, so that a short serial number
/// does not lose most of itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Masker {
    full: bool,
    /// Values the profile names that are serial numbers, wherever they occur.
    known: Vec<String>,
}

impl Masker {
    /// A masker that leaves everything as it is.
    pub fn unmasked() -> Self {
        Masker {
            full: true,
            known: Vec::new(),
        }
    }

    /// The masker for `profile`: masks unless the profile asks for full identifiers, and treats
    /// each D-PDU API `module` (a device name or serial number) as a serial number.
    pub fn for_profile(profile: &Profile) -> Self {
        let vcis: Vec<&Vci> = match &profile.setup {
            Setup::Link { vci, .. } => vec![vci],
            Setup::Loopback(ends) => ends.iter().filter_map(|e| e.vci.as_ref()).collect(),
        };
        Self::with_known(
            profile.report_full_identifiers,
            vcis.into_iter()
                .filter(|v| v.api == Api::Iso22900 && !v.module.trim().is_empty())
                .map(|v| v.module.trim().to_owned())
                .collect(),
        )
    }

    /// A masker for these known serial numbers, longest first, so that masking a shorter one
    /// that occurs inside a longer one cannot leave the rest of the longer one readable.
    fn with_known(full: bool, mut known: Vec<String>) -> Self {
        known.sort_by_key(|serial| std::cmp::Reverse(serial.len()));
        Masker { full, known }
    }

    /// The mask of one value.
    pub fn mask_value(value: &str) -> String {
        let chars: Vec<char> = value.chars().collect();
        if chars.len() < 8 {
            "****".to_owned()
        } else {
            format!(
                "****{}",
                chars[chars.len() - 4..].iter().collect::<String>()
            )
        }
    }

    /// Free text with every serial-like token and every known serial number masked. A
    /// serial-like token is a run of letters, digits, `-` and `_` that holds at least 8 letters
    /// and digits, one of them a digit: it covers every VIN (17 letters and digits, whether or
    /// not the check digit holds) and serial numbers such as `SN-1234567890`. A hexadecimal
    /// literal (`0x` and hexadecimal digits only), or a run of them joined by `-` or `_`, is a
    /// code, not a serial number, and is kept.
    pub fn text(&self, text: &str) -> String {
        self.mask(text, true)
    }

    /// A name the profile gives, such as a library or a configured J2534 label: only the known
    /// serial numbers and VIN-shaped tokens (17 letters and digits, one of them a digit) in it
    /// are masked, so that a library named like `op20pt32` stays readable.
    pub fn name(&self, name: &str) -> String {
        self.mask(name, false)
    }

    fn mask(&self, text: &str, serial_like: bool) -> String {
        if self.full {
            return text.to_owned();
        }
        let mut text = text.to_owned();
        for serial in &self.known {
            text = text.replace(serial.as_str(), &Self::mask_value(serial));
        }
        let is_part =
            |c: char| c.is_ascii_alphanumeric() || (serial_like && (c == '-' || c == '_'));
        let mut out = String::with_capacity(text.len());
        let mut token = String::new();
        let flush = |token: &mut String, out: &mut String| {
            if Self::is_identifier(token, serial_like) {
                out.push_str(&Self::mask_value(token));
            } else {
                out.push_str(token);
            }
            token.clear();
        };
        for c in text.chars() {
            if is_part(c) {
                token.push(c);
            } else {
                flush(&mut token, &mut out);
                out.push(c);
            }
        }
        flush(&mut token, &mut out);
        out
    }

    /// Whether `token` is a VIN or, with `serial_like`, looks like a serial number.
    fn is_identifier(token: &str, serial_like: bool) -> bool {
        let alphanumeric = token.bytes().filter(u8::is_ascii_alphanumeric).count();
        if !token.bytes().any(|b| b.is_ascii_digit()) {
            return false;
        }
        if !serial_like {
            return alphanumeric == 17 && alphanumeric == token.len();
        }
        // A run of hexadecimal literals, such as `0x7E0-0x7E8`, is codes, not a serial number.
        let hex_literals = token.split(['-', '_']).all(|part| {
            part.strip_prefix("0x")
                .or_else(|| part.strip_prefix("0X"))
                .is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit())
                })
        });
        alphanumeric >= 8 && !hex_literals
    }

    /// A module's `vendor_module_name`. For the D-PDU API a list of `attribute='value'` pairs
    /// has each value masked whose attribute name contains "serial" (any case), and text that is
    /// no such list is a device name or a serial number and is masked whole; the result is then
    /// masked as free text. A J2534 name is the label of a configured entry, not a serial
    /// number, and is masked as a [name](Self::name).
    pub fn module_name(&self, api: Api, name: &str) -> String {
        if self.full {
            return name.to_owned();
        }
        let masked = match api {
            Api::J2534V0404 => return self.name(name),
            Api::Iso22900 if attribute_values(name).is_empty() => {
                if name.trim().is_empty() {
                    String::new()
                } else {
                    Self::mask_value(name.trim())
                }
            }
            Api::Iso22900 => self.mask_serial_attributes(name),
        };
        self.text(&masked)
    }

    fn mask_serial_attributes(&self, vendor: &str) -> String {
        let mut out = String::new();
        let mut rest = vendor;
        while let Some(equals) = rest.find('=') {
            let (before, after) = rest.split_at(equals + 1);
            let name = before[..before.len() - 1]
                .trim_end()
                .rsplit(|c: char| c.is_whitespace() || c == ',' || c == ';')
                .next()
                .unwrap_or("");
            let trimmed = after.trim_start();
            out.push_str(before);
            let Some(quoted) = trimmed.strip_prefix('\'') else {
                rest = trimmed;
                continue;
            };
            let Some(end) = quoted.find('\'') else {
                // An unterminated value: mask the rest if it belongs to a serial attribute.
                if name.to_ascii_lowercase().contains("serial") {
                    out.push('\'');
                    out.push_str(&Self::mask_value(quoted));
                    return out;
                }
                rest = trimmed;
                break;
            };
            out.push('\'');
            if name.to_ascii_lowercase().contains("serial") {
                out.push_str(&Self::mask_value(&quoted[..end]));
            } else {
                out.push_str(&quoted[..end]);
            }
            out.push('\'');
            rest = &quoted[end + 1..];
        }
        out.push_str(rest);
        out
    }
}

/// The report of one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Start of the run, UTC, as RFC 3339.
    pub started_at: String,
    pub host: String,
    pub os: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    pub class: String,
    /// Whether VINs and serial numbers are in full (the profile asked for it).
    pub identifiers_in_full: bool,
    /// One entry per opened device (a T0 loopback may open two); empty when no check opened one.
    #[serde(rename = "device", skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceInfo>,
    #[serde(rename = "check")]
    pub checks: Vec<CheckResult>,
    #[serde(skip)]
    masker: Masker,
}

impl Report {
    /// Starts the report of a run with `profile`, now.
    pub fn new(profile: &Profile) -> Self {
        Self::with_host(profile, &host_name(), os_version().as_deref())
    }

    /// [`Report::new`] with the host name and OS description given. Both are masked like any
    /// other text, since a host name can carry a VIN or a serial number.
    fn with_host(profile: &Profile, host: &str, os_version: Option<&str>) -> Self {
        let masker = Masker::for_profile(profile);
        Report {
            started_at: rfc3339(SystemTime::now()),
            host: masker.text(host),
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            os_version: os_version.map(|v| masker.text(v)),
            class: format!("{:?}", profile.class),
            identifiers_in_full: profile.report_full_identifiers,
            devices: Vec::new(),
            checks: Vec::new(),
            masker,
        }
    }

    /// The report of a run whose profile could not be read: the class is `unknown` and every
    /// identifier is masked, since no profile asks for them in full.
    pub fn without_profile() -> Self {
        let masker = Masker {
            full: false,
            known: Vec::new(),
        };
        Report {
            started_at: rfc3339(SystemTime::now()),
            host: masker.text(&host_name()),
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            os_version: os_version().map(|v| masker.text(&v)),
            class: "unknown".to_owned(),
            identifiers_in_full: false,
            devices: Vec::new(),
            checks: Vec::new(),
            masker,
        }
    }

    /// The masker the report applies, for a check that writes its own free text.
    pub fn masker(&self) -> &Masker {
        &self.masker
    }

    /// Adds a check's result. The message is masked.
    pub fn record(&mut self, name: &str, outcome: Outcome, message: &str) {
        self.checks.push(CheckResult {
            name: name.to_owned(),
            outcome,
            message: self.masker.text(message),
        });
    }

    /// Adds the result of a check the profile does not permit.
    pub fn record_refusal(&mut self, name: &str, refusal: &Refusal) {
        self.record(name, Outcome::Refused, &refusal.to_string());
    }

    /// Adds what an opened device reported: the ABI of the worker, the library the profile
    /// names as the vendor's, the module and the versions. D-PDU API versions are coded numbers
    /// and are written as `major.minor.revision`; J2534 reports them as strings (`hw_name` is the
    /// firmware, `fw_name` the library).
    pub fn add_device(
        &mut self,
        vci: &Vci,
        abi: Abi,
        module: &ReportedModule,
        version: &ReportedVersion,
    ) {
        let (firmware, library) = match vci.api {
            Api::Iso22900 => (
                coded_version(version.fw_version),
                coded_version(version.pdu_api_sw_version),
            ),
            Api::J2534V0404 => (version.hw_name.clone(), version.fw_name.clone()),
        };
        self.devices.push(DeviceInfo {
            abi: abi.name().to_owned(),
            vendor: match vci.api {
                Api::Iso22900 if !version.vendor_name.trim().is_empty() => {
                    self.masker.text(&version.vendor_name)
                }
                _ => self.masker.name(&vci.library),
            },
            model: match vci.api {
                Api::Iso22900 if !version.hw_name.trim().is_empty() => {
                    self.masker.text(&version.hw_name)
                }
                _ => self.masker.module_name(vci.api, &module.name),
            },
            firmware_version: self.masker.text(&firmware),
            library_version: self.masker.text(&library),
        });
    }

    /// The report as TOML.
    pub fn to_toml(&self) -> Result<String, ReportError> {
        toml::to_string_pretty(self).map_err(|e| ReportError::Encode(e.to_string()))
    }

    /// Writes the report to [`reports_dir`] and returns the file's path.
    pub fn write(&self) -> Result<PathBuf, ReportError> {
        self.write_to(&reports_dir()?)
    }

    /// Writes the report to a new file in `dir`, which is created (mode 0700 on Unix) with its
    /// missing parents if it does not exist. Refuses a `dir` inside this repository's work tree (the
    /// one this crate was built from), and on Unix
    /// an existing `dir` that others can read. The file name is the run's start and the process
    /// id, with a counter if that name is taken; no file is overwritten.
    pub fn write_to(&self, dir: &Path) -> Result<PathBuf, ReportError> {
        let text = self.to_toml()?;
        let dir = resolve(dir)?;
        if let Some(repo) = repository_root(Path::new(env!("CARGO_MANIFEST_DIR")))
            && dir.starts_with(&repo)
        {
            return Err(ReportError::InsideRepository(repo));
        }
        create_private_dir(&dir)?;
        let stem = format!(
            "{}-{}",
            self.started_at.replace([':', '-'], ""),
            std::process::id()
        );
        for attempt in 0u32.. {
            let name = if attempt == 0 {
                format!("{stem}.toml")
            } else {
                format!("{stem}-{attempt}.toml")
            };
            let path = dir.join(name);
            match create_private_file(&path) {
                Ok(mut file) => {
                    file.write_all(text.as_bytes()).map_err(io(&path))?;
                    return Ok(path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io(&path)(e)),
            }
        }
        unreachable!("the counter does not run out")
    }
}

/// `<user data directory>/nekoguruma/hw-reports`: `$XDG_DATA_HOME` or `~/.local/share` on Linux
/// and other Unix, `~/Library/Application Support` on macOS, `%LOCALAPPDATA%` on Windows.
pub fn reports_dir() -> Result<PathBuf, ReportError> {
    let mut dir = data_dir(|name| std::env::var_os(name)).ok_or(ReportError::NoDataDir)?;
    dir.extend(SUBDIR);
    Ok(dir)
}

fn data_dir(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let absolute = |name: &str| var(name).map(PathBuf::from).filter(|p| p.is_absolute());
    if cfg!(windows) {
        absolute("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        absolute("HOME").map(|home| home.join("Library").join("Application Support"))
    } else {
        absolute("XDG_DATA_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".local").join("share")))
    }
}

fn create_private_dir(dir: &Path) -> Result<(), ReportError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(io(dir))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir)
            .map_err(io(dir))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(ReportError::TooOpen {
                path: dir.to_owned(),
                mode: mode & 0o777,
            });
        }
    }
    Ok(())
}

fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

fn host_name() -> String {
    ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The OS's own description of itself where it has one (`PRETTY_NAME` of `os-release`).
fn os_version() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim().trim_matches('"').to_owned())
}

/// `time` as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (proleptic Gregorian calendar).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let mut out = String::new();
    write!(
        out,
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
    .expect("writing to a String");
    out
}

/// A run's report that is written when the guard goes out of scope, so that a check which
/// fails by panic (an assertion, an `unwrap`) still leaves its report. The check ends with
/// [`RunGuard::pass`] or [`RunGuard::finish`]; a guard dropped while the thread panics records
/// the check as failed, and one dropped without a result records that as a failure too.
///
/// When the thread is not already panicking, dropping the guard panics, so the test fails, if
/// the check recorded anything but a pass or if its report cannot be written. During an unwind it
/// never panics again: a report that cannot be written is then described on stderr.
pub struct RunGuard {
    report: Report,
    check: String,
    dir: Option<PathBuf>,
}

impl RunGuard {
    /// Starts the report of `check` for `profile`; it is written to [`reports_dir`] on drop.
    pub fn start(profile: &Profile, check: &str) -> Self {
        Self::new(Report::new(profile), check, None)
    }

    /// Like [`RunGuard::start`], writing to `dir`.
    pub fn start_in(profile: &Profile, check: &str, dir: PathBuf) -> Self {
        Self::new(Report::new(profile), check, Some(dir))
    }

    /// [`RunGuard::start`] for a run whose profile could not be read
    /// ([`Report::without_profile`]).
    pub fn start_without_profile(check: &str) -> Self {
        Self::new(Report::without_profile(), check, None)
    }

    fn new(report: Report, check: &str, dir: Option<PathBuf>) -> Self {
        RunGuard {
            report,
            check: check.to_owned(),
            dir,
        }
    }

    /// The report, for `add_device` and other entries.
    pub fn report(&mut self) -> &mut Report {
        &mut self.report
    }

    /// Records the check's result.
    pub fn finish(&mut self, outcome: Outcome, message: &str) {
        self.report.record(&self.check.clone(), outcome, message);
    }

    pub fn pass(&mut self) {
        self.finish(Outcome::Passed, "");
    }

    fn recorded(&self) -> bool {
        self.report.checks.iter().any(|c| c.name == self.check)
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // A check that panics after recording a pass (in a later guard's drop, say) failed.
            let check = self.check.clone();
            self.report
                .checks
                .retain(|c| c.name != check || c.outcome != Outcome::Passed);
            if !self.recorded() {
                self.finish(Outcome::Failed, "the check panicked");
            }
        } else if !self.recorded() {
            self.finish(
                Outcome::Failed,
                "the check ended without recording a result",
            );
        }
        let written = match &self.dir {
            Some(dir) => self.report.write_to(dir),
            None => self.report.write(),
        };
        match written {
            Ok(path) => {
                eprintln!("hardware report: {}", path.display());
                // A check that recorded anything but a pass fails as a test too, also when it
                // ended without panicking.
                if !std::thread::panicking()
                    && let Some(result) = self
                        .report
                        .checks
                        .iter()
                        .find(|c| c.name == self.check && c.outcome != Outcome::Passed)
                {
                    panic!("{}: {:?}: {}", result.name, result.outcome, result.message);
                }
            }
            // Unwinding already fails the check; a second panic would abort the test process.
            Err(error) if std::thread::panicking() => {
                eprintln!("cannot write the hardware report: {error}");
            }
            // Every run leaves a report (design 13.5), so a check whose report is lost fails.
            Err(error) => panic!("cannot write the hardware report: {error}"),
        }
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;

/// `dir` as an absolute path with symlinks resolved, before anything is created: the nearest
/// existing ancestor is canonicalized and the components below it are appended. A `..` among
/// those components is refused, since it could lead out of the place that was checked.
fn resolve(dir: &Path) -> Result<PathBuf, ReportError> {
    let absolute = std::path::absolute(dir).map_err(io(dir))?;
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
            // A trailing `..` or the root: nothing to append, and `exists` failed.
            return Err(io(dir)(std::io::ErrorKind::InvalidInput.into()));
        };
        missing.push(name);
        existing = parent;
    }
    let mut resolved = existing.canonicalize().map_err(io(existing))?;
    for name in missing.into_iter().rev() {
        resolved.push(name);
    }
    if absolute.strip_prefix(existing).is_ok_and(|rest| {
        rest.components()
            .any(|c| c == std::path::Component::ParentDir)
    }) {
        return Err(io(dir)(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the path has `..` below its existing part",
        )));
    }
    // The repository root is compared in its canonical form too.
    Ok(resolved)
}

/// The root of the Git work tree that holds `crate_dir`, if it still exists. Only this
/// repository is refused: a user data directory inside some other work tree (a home directory
/// kept in Git, for one) is the user's choice.
fn repository_root(crate_dir: &Path) -> Option<PathBuf> {
    crate_dir
        .ancestors()
        .find(|a| a.join(".git").exists())
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.to_owned()))
}
