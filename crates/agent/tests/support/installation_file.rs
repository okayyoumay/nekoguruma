//! The agent's installation file `agent.toml` for the tests that start `ngr-agent` (design 6.7,
//! ADR-494), shared through `#[path]`: `crates/agent/tests/installation.rs`, `identity.rs`,
//! `register.rs` and `connect.rs`, and the server's `crates/server/tests/postgres/agent_register.rs`
//! and `agent_connect.rs`.
//!
//! A debug `ngr-agent` reads the file where `NGR_AGENT_INSTALLATION_FILE` points and then trusts
//! the user it runs as to own it, so a test writes the file in a temporary directory it owns.
//! A release `ngr-agent` ignores the variable and reads `/etc/nekoguruma/agent.toml`
//! (`%ProgramData%\nekoguruma\agent.toml`), which a test machine does not have: every command
//! that needs the file exits 1 there, so the tests that go on past it run in debug builds only.

// Not `expect`: each including test package uses a different part of this file, and an `expect`
// would be unfulfilled in the ones that use all of it.
#![allow(
    dead_code,
    reason = "each including test package uses a different part of the helper"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The variable that moves the file in a debug build.
pub const VAR: &str = "NGR_AGENT_INSTALLATION_FILE";

/// A P-256 public key in SPKI PEM form, the distribution root of the fixtures.
pub const ROOT_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAExLXdhtCUjjaq8kjf/fUG8XOSJd4E\n\
dc8H7X7vfRj2bz8YsntyWK78zZpqNeKMW11GF6o1CoOFXy10hm6T5evbaw==\n\
-----END PUBLIC KEY-----\n";

/// A valid file's text: `mode` (`device` or `user`) and `server_url`, with the fixture root key.
pub fn text(mode: &str, server_url: &str) -> String {
    format!(
        "mode = \"{mode}\"\nserver_url = \"{server_url}\"\n\
         distribution_root_pem = \"\"\"\n{ROOT_PEM}\"\"\"\n"
    )
}

/// Writes `contents` as `agent.toml` in `dir`, readable by everyone and writable by its owner
/// alone, and returns the path. The file is written aside and renamed into place, so a process
/// that reads it while a test writes it again never sees half of it.
pub fn write(dir: &Path, contents: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let path = dir.join("agent.toml");
    let aside = dir.join(format!(
        "agent.toml.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&aside, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    std::fs::rename(&aside, &path).unwrap();
    path
}

/// Points `command` at the installation file `path`.
pub fn point(command: &mut Command, path: &Path) {
    command.env(VAR, path);
}

/// Writes a valid file for `mode` and `server_url` in `dir` and points `command` at it.
pub fn install(command: &mut Command, dir: &Path, mode: &str, server_url: &str) -> PathBuf {
    let path = write(dir, &text(mode, server_url));
    point(command, &path);
    path
}
