//! The writability check of 7.2 (ADR-270): before a vendor library is loaded, neither it nor
//! anything that decides which file is loaded may be changeable by a regular user.
//!
//! Checked are the library, its directory, every ancestor directory up to the root, each naming
//! file (the definition file that named the library) and the ancestors of each. Every entry on
//! the way, including symbolic links, must be owned by a trusted owner ([`Policy`]); entries
//! must not grant write access to anyone else. The per-entry rules are pure functions over
//! plain data (`unix.rs`, `windows.rs`); a walker feeds them what the operating system reports.
//! The caller decides what a finding means: device mode refuses the library, user mode warns.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    fmt, io,
    path::{Component, MAIN_SEPARATOR_STR, Path, PathBuf},
};

mod unix;
mod windows;

/// Maximum number of symbolic links followed while resolving one path.
const MAX_LINK_HOPS: usize = 40;

/// What a checked path is for. It selects the rule applied to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The vendor library itself.
    Library,
    /// The directory that directly contains the library: nobody else may add entries to it
    /// (an add-only directory is not enough).
    LibraryDirectory,
    /// A further ancestor directory, a container: others may add entries but not remove or
    /// replace what is in it (a sticky directory on Unix, add-only rights on Windows).
    Directory,
    /// A file that names the library (a definition file).
    NamingFile,
    /// A symbolic link or other reparse point met while resolving a path.
    Link,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::Library => "library",
            Role::LibraryDirectory => "library directory",
            Role::Directory => "directory",
            Role::NamingFile => "naming file",
            Role::Link => "link",
        })
    }
}

/// Why a path failed the check.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Reason {
    /// The owner is not in the [`Policy`] (the detail is the uid or SID).
    #[error("owned by {0}, who is not trusted")]
    UnprivilegedOwner(String),
    /// Someone outside the policy can write (the detail is the mode bits or the ACE).
    #[error("writable by regular users ({0})")]
    WritableByRegularUsers(String),
    /// The Windows DACL is absent, which grants everyone full access.
    #[error("no DACL, so everyone has full access")]
    NullDacl,
    /// The path is relative, so what it names depends on the working directory.
    #[error("the path is not absolute")]
    NotAbsolute,
    /// The path does not end at a regular file.
    #[error("not a regular file")]
    NotARegularFile,
    /// Resolving the path followed more links than the limit.
    #[error("too many symbolic links")]
    TooManyLinks,
    /// The entry could not be inspected.
    #[error("cannot be inspected: {0}")]
    Io(String),
}

/// One failed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The path that failed.
    pub path: PathBuf,
    /// Its role in the check.
    pub role: Role,
    /// Why it failed.
    pub reason: Reason,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.path.display(),
            self.role,
            self.reason
        )
    }
}

/// The library is not safe to load: every path that failed, not only the first.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", list(findings))]
pub struct WritabilityError {
    /// The failed paths, never empty.
    pub findings: Vec<Finding>,
}

fn list(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(Finding::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The owners trusted to hold write access (7.2).
#[derive(Debug, Clone)]
pub struct Policy {
    /// Trusted owner uids.
    #[cfg(unix)]
    uids: Vec<u32>,
    /// Trusted owner SIDs in text form.
    #[cfg(windows)]
    sids: Vec<String>,
}

impl Policy {
    /// The system owners: uid 0 on Unix; SYSTEM, BUILTIN\Administrators and TrustedInstaller on
    /// Windows.
    pub fn system() -> Policy {
        Policy {
            #[cfg(unix)]
            uids: vec![0],
            #[cfg(windows)]
            sids: windows::SYSTEM_SIDS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }

    /// Also trusts the user the process runs as (the effective uid, or the user of the process
    /// token). For tests and debug builds, where fixtures are owned by that user.
    ///
    /// # Panics
    ///
    /// On Windows, if the process token cannot be read.
    #[cfg(any(test, debug_assertions))]
    pub fn trusting_current_user(mut self) -> Policy {
        #[cfg(unix)]
        {
            // SAFETY: geteuid has no preconditions and cannot fail.
            self.uids.push(unsafe { libc::geteuid() });
        }
        #[cfg(windows)]
        {
            self.sids
                .push(windows::current_user_sid().expect("read the process token user"));
        }
        self
    }
}

/// Checks `library` and `naming_files` against [`Policy::system`].
pub fn check_writability(library: &Path, naming_files: &[PathBuf]) -> Result<(), WritabilityError> {
    check_writability_with(library, naming_files, &Policy::system())
}

/// Checks the library, its directory, all ancestors, and each naming file with its ancestors.
/// All findings are collected.
pub fn check_writability_with(
    library: &Path,
    naming_files: &[PathBuf],
    policy: &Policy,
) -> Result<(), WritabilityError> {
    #[cfg(unix)]
    let probe = unix::UnixProbe {
        uids: policy.uids.clone(),
    };
    #[cfg(windows)]
    let probe = windows::WindowsProbe {
        sids: policy.sids.clone(),
    };
    #[cfg(not(any(unix, windows)))]
    let probe = {
        let _ = policy;
        NoProbe
    };
    check_with_probe(&probe, library, naming_files)
}

/// What an entry is, without following a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Link,
    Dir,
    File,
    Other,
}

struct Inspected {
    kind: Kind,
    /// Reasons the entry fails the rule for the role (the role is `Link` for a link).
    reasons: Vec<Reason>,
}

/// Reads an entry's kind, owner and permissions and applies the rule.
trait Probe {
    fn inspect(&self, path: &Path, role: Role) -> io::Result<Inspected>;
}

#[cfg(not(any(unix, windows)))]
struct NoProbe;

#[cfg(not(any(unix, windows)))]
impl Probe for NoProbe {
    fn inspect(&self, _path: &Path, _role: Role) -> io::Result<Inspected> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no writability check on this platform",
        ))
    }
}

fn check_with_probe(
    probe: &dyn Probe,
    library: &Path,
    naming_files: &[PathBuf],
) -> Result<(), WritabilityError> {
    let mut walker = Walker {
        probe,
        cache: HashMap::new(),
        findings: Vec::new(),
    };
    walker.walk(library, Role::Library, Role::LibraryDirectory);
    for file in naming_files {
        walker.walk(file, Role::NamingFile, Role::Directory);
    }
    let mut findings = walker.findings;
    // A directory checked both as a library directory and as a plain container keeps the
    // stricter role's findings.
    let strict: HashSet<PathBuf> = findings
        .iter()
        .filter(|f| f.role == Role::LibraryDirectory)
        .map(|f| f.path.clone())
        .collect();
    findings.retain(|f| f.role != Role::Directory || !strict.contains(&f.path));
    if findings.is_empty() {
        Ok(())
    } else {
        Err(WritabilityError { findings })
    }
}

/// One element of a path still to be walked.
enum Item {
    Prefix(OsString),
    Root,
    Parent,
    Name(OsString),
}

fn items(path: &Path) -> VecDeque<Item> {
    path.components()
        .filter_map(|c| match c {
            Component::Prefix(p) => Some(Item::Prefix(p.as_os_str().to_owned())),
            Component::RootDir => Some(Item::Root),
            Component::CurDir => None,
            Component::ParentDir => Some(Item::Parent),
            Component::Normal(n) => Some(Item::Name(n.to_owned())),
        })
        .collect()
}

struct Walker<'a> {
    probe: &'a dyn Probe,
    cache: HashMap<(PathBuf, Role), io::Result<Inspected>>,
    findings: Vec<Finding>,
}

impl Walker<'_> {
    fn report(&mut self, path: &Path, role: Role, reason: Reason) {
        let finding = Finding {
            path: path.to_owned(),
            role,
            reason,
        };
        // The same entry reached by two walks is reported once.
        if !self.findings.contains(&finding) {
            self.findings.push(finding);
        }
    }

    /// Inspects an entry once per (path, role) and reports its reasons the first time.
    /// Returns its kind, or `None` if it could not be inspected.
    fn inspect(&mut self, path: &Path, role: Role) -> Option<Kind> {
        let key = (path.to_owned(), role);
        if !self.cache.contains_key(&key) {
            let result = self.probe.inspect(path, role);
            let fresh: Vec<(Role, Reason)> = match &result {
                Ok(i) => {
                    let r = if i.kind == Kind::Link {
                        Role::Link
                    } else {
                        role
                    };
                    i.reasons.iter().map(|x| (r, x.clone())).collect()
                }
                Err(e) => vec![(role, Reason::Io(e.to_string()))],
            };
            for (r, reason) in fresh {
                self.report(path, r, reason);
            }
            self.cache.insert(key.clone(), result);
        }
        self.cache[&key].as_ref().ok().map(|i| i.kind)
    }

    fn walk(&mut self, path: &Path, leaf_role: Role, dir_role: Role) {
        if !path.is_absolute() {
            self.report(path, leaf_role, Reason::NotAbsolute);
            return;
        }
        let mut pending = items(path);
        let mut current = PathBuf::new();
        let mut hops = 0usize;
        while let Some(item) = pending.pop_front() {
            // The directory whose next entry is the leaf is the leaf's own directory.
            let next_is_leaf = pending.len() == 1 && matches!(pending[0], Item::Name(_));
            let container = if next_is_leaf {
                dir_role
            } else {
                Role::Directory
            };
            match item {
                Item::Prefix(p) => current = PathBuf::from(p),
                Item::Root => {
                    let prefix = match current.components().next() {
                        Some(Component::Prefix(p)) => Some(p.as_os_str().to_owned()),
                        _ => None,
                    };
                    current = prefix.map(PathBuf::from).unwrap_or_default();
                    current.push(MAIN_SEPARATOR_STR);
                    if self.inspect(&current, container).is_none() {
                        return;
                    }
                }
                Item::Parent => {
                    // `current` holds no links, so its parent is the lexical one, already checked.
                    current.pop();
                }
                Item::Name(name) => {
                    let candidate = current.join(name);
                    let last = pending.is_empty();
                    let Some(kind) =
                        self.inspect(&candidate, if last { leaf_role } else { container })
                    else {
                        return;
                    };
                    match kind {
                        Kind::Link => {
                            hops += 1;
                            if hops > MAX_LINK_HOPS {
                                self.report(&candidate, Role::Link, Reason::TooManyLinks);
                                return;
                            }
                            let target = match std::fs::read_link(&candidate) {
                                Ok(t) => t,
                                Err(e) => {
                                    self.report(&candidate, Role::Link, Reason::Io(e.to_string()));
                                    return;
                                }
                            };
                            // A relative target starts in the link's directory (`current`).
                            let mut expanded = items(&target);
                            expanded.append(&mut pending);
                            pending = expanded;
                        }
                        Kind::Dir if !last => current = candidate,
                        Kind::File | Kind::Other | Kind::Dir if last => {
                            if kind != Kind::File {
                                self.report(&candidate, leaf_role, Reason::NotARegularFile);
                            }
                            return;
                        }
                        _ => {
                            self.report(
                                &candidate,
                                container,
                                Reason::Io("not a directory".to_owned()),
                            );
                            return;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
