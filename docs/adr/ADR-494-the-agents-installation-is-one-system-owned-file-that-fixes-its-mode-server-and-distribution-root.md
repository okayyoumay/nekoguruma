# ADR-494: The Agent's Installation Is One System-Owned File That Fixes Its Mode, Server and Distribution Root

**Date:** 2026-11-19
**Status:** Accepted
**Affects:** design 6.2, 6.5, 6.7, 7.2, 11.1, 13; `crates/agent` (`installation`, `ngr-agent run`, `identify`, `identity`, `register` and `connect`), `crates/vci-service-config` (`agent_installation_file()`), `crates/agent/docs/ngr-agent.md`; annotates ADR-228, ADR-285, ADR-460, ADR-463, ADR-470, ADR-471

## Context

Design 6.2 fixes the operating mode per device at installation, and 6.7 lists what the installer
decides: the mode, the startup method, the deployment profile, the server URL and the distribution
key root. Nothing stored any of it. The mode came from `--mode` on the command line, which any
caller can set to either value, and the server's origin from `register --server`, which any caller
can point at another server. `connect` took its origin from the registration record alone, so a
record written for one server stayed in use after the installation moved to another. The
distribution key root of 11.1 was described as embedded in the agent, which no build did.

The agent already has a pattern for a file an administrator owns: the worker's configuration file
sits at a location fixed at build time, with a debug-only override (ADR-228, ADR-073), and the
launcher checks it with the writability rules of design 7.2 before reading it (ADR-285). That
check is lenient in user mode, where the findings are a warning, because a configuration file only
sets logging and per-library details. What the installation decides is policy the agent acts on:
which mode it runs in decides whether a library that fails the pre-load check is refused, and the
origin decides who receives the registration code and the agent's proof.

## Decision

1. **One file, `agent.toml`, beside `config.toml`.** It lives in the fixed system directory:
   `/etc/nekoguruma/agent.toml` on Linux, `%ProgramData%\nekoguruma\agent.toml` on Windows. Its
   location is `vci_service_config::agent_installation_file()`: the build-time
   `NGR_AGENT_INSTALLATION_FILE` (default `nekoguruma/agent.toml`, relative to the fixed system
   directory or absolute), and in debug builds only a non-empty runtime variable of the same name
   (the pattern of ADR-228 and ADR-073). No `config-root-*` feature moves it. Release builds read
   no environment variable.
2. **The file is checked as a system file in both modes.** The agent runs the design 7.2
   writability check ([`Policy::system`], ADR-270) on it before it reads a byte, and a finding is
   a refusal in user mode as well as in device mode: a user-mode account cannot change what its own
   agent installed. A debug build that has the location override also trusts the user it runs as
   to own the file, so tests can use files they own; nothing else trusts any user. A symbolic link
   is followed under the check's own rule (a link owned by a trusted owner, to a file that passes,
   is accepted). The file is read as bytes, at most 64 KiB, checked as UTF-8 (a leading byte order
   mark is accepted) and parsed to a TOML table that the agent validates by hand, so no message
   repeats a value from the file and `toml`'s error text, which quotes the offending line, is never
   shown.
3. **The keys.** All are strings; an unknown key is refused.
   - `mode`: `device` or `user`, required.
   - `server_url`: required; the pinned origin of design 13.3 in the form `register --server`
     accepts (`ServerOrigin::parse`), kept in its canonical text.
   - `distribution_root_pem`: required; one P-256 public key in SPKI PEM form (the strictness of
     `PublicKey::from_spki_pem`, line endings CRLF or LF). It is parsed and the fingerprint logged;
     nothing verifies against it before M6, and the agent holds only the parsed key.
   - `profile`: `local` (the default) or `standard`; `standard` is refused until it exists.
   - `startup`: `explicit` or `auto`; the default is `auto` for a device and `explicit` for a user;
     `explicit` with `device` is refused.
   - `lock_dir`: an optional absolute path, validated only.

   The first problem found decides, in a fixed order: the syntax, an unknown key, then `mode`,
   `server_url`, `distribution_root_pem`, `profile`, `startup` and `lock_dir`.
4. **Three commands read it, once, early (item 7 adds two).** `ngr-agent identity`, `register` and `connect` read the
   file in `main`, after the operational log starts and after every usage check (a terminal on
   standard input for `register` included), and before the key, a lock or a connection. `--mode`
   stays required and must equal the file's mode; a difference is exit 1. A file that is absent,
   refused, not UTF-8, larger than the limit, not valid, incomplete or invalid is exit 1. A read
   error is exit 3, except that a refusal of access to a file that passed the check is exit 1: the
   account the agent runs as cannot read it, and a restart does not change that.
5. **The installed origin is the authority.** `register` takes the server from the file;
   `--server` is optional and must canonicalise to the installed origin, else exit 1 before the key.
   `connect` compares the registration record's origin with the installed one byte for byte inside
   `control::open_in`, after the record is read and before the outbox, the issue-time mark, the
   received-jobs ledger or the key store: a difference is exit 1 with the advice to register again.
   Both locks are taken for the comparison and released with it.
6. **What is logged.** A valid file logs the mode, the profile, the startup method and the root
   key's fingerprint; the file's path and the origin go in the entry's `external` field only
   (ADR-348 item 6). A refusal logs its findings, which name paths, in `external` only; the
   standard error message is fixed wording.

7. **`run` and `identify` read the installed mode too.** They read the file at the same place as
   the three commands of item 4 (after the log starts and every usage check, before a lock, the
   communication log directory, the state directory or a worker), once, and use its mode only: no
   server or key check. `--mode` stays required and must equal the file's mode, a difference is
   exit 1, an absent, refused or invalid file is exit 1 and any other read error exit 3, with the
   same messages as the three commands, so a device with no installed
   mode starts no worker and a caller cannot run a job in the other mode than the installation
   chose.

## Consequences

- A device is installed by writing one root-owned file. The directory `/etc/nekoguruma` must also
  be owned by a system principal: a server that runs as another account and owns that directory
  makes the file refused, so the server's configuration examples keep its files under `/etc/nekoguruma-server` and `/var/lib/nekoguruma-server`.
- Because a release agent has no override, a release test machine without a file at the fixed
  location exits 1 from every command that reads it; tests that go on past the file run in debug builds, and
  one release test shows that the override is ignored.
- Moving an installation to another server needs a new `register --replace`: `connect` refuses the
  old record, which is what ADR-463's pinned out-of-band server key ID already assumed.
- `distribution_root_pem`, `profile`, `startup` and `lock_dir` are validated and recorded; the
  extension-package check (M6), the standard profile, the startup artifacts and the lock area use
  them later. A key a later version adds is refused by an older agent as an unknown key.
- A release `run` on a test machine stops at the missing installation file before it checks the
  program, so its up-front refusal of a write and of TesterPresent is shown by a release-only test
  of the job check it makes (`check_job` with the build's ceiling) rather than through the binary.
