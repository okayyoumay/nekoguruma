# ADR-460: The Agent Key Is Created Once Under a Bounded Lock in a Per-Mode State Directory

**Date:** 2026-11-11
**Status:** Accepted
**Affects:** design 6.2, 6.4; `crates/agent` (`state`, `identity`, the `ngr-agent identity` command); ADR-459 (Decision item 4); ADR-255 (the lock's waiting rule differs); the journal-location, Windows, Linux user-mode and Linux device-mode key store work

## Context

ADR-459 Decision item 4 names the outcomes of the agent's key store: a store with no key creates
one under an exclusive lock file (as ADR-255 locks the journal), an unavailable store is retried,
and an undecryptable key is never overwritten. It does not say which file is locked, whether a
second creator waits, what the secret is called, where design 6.2's "user area" and "device area"
are, or what the command that creates the key prints.

ADR-255 tries the lock once, because contention for a journal means a duplicate job. For the key,
the second process of a race is not an error: it should find the first one's key. The Secret
Service cannot create a secret only if it is absent, so the lock is what makes creation correct
there; a file-backed store can create atomically (a hard link into place, as `Journal::create`
does), but it is covered by the same rule.

## Decision

1. **State roots per mode** (`agent::state`), with sub-directories created readable by the
   account only and, on Unix, refused when writable by group or others:
   - user mode, Unix: an absolute `$XDG_STATE_HOME/nekoguruma`, else `~/.local/state/nekoguruma`;
     Windows: `%LOCALAPPDATA%\nekoguruma`. The agent creates it.
   - device mode, Unix: `/var/lib/nekoguruma`, built in. The installation creates it for the
     service account (systemd's `StateDirectory=`, 0700); the agent creates only sub-directories,
     and a missing root is an error. The maintainer decided this path.
   - device mode, Windows: the service account's own `%LOCALAPPDATA%\nekoguruma`, which the agent
     creates. The maintainer decided this over a directory under `%ProgramData%` that the
     installation would create with a service-account-only ACL: it needs no ACL code, cannot be
     pre-created by a regular user, and sits beside the DPAPI master key the blob depends on. It
     assumes a `NT SERVICE\<name>` process has a loaded profile and `LOCALAPPDATA`, which the
     Windows store work checks first.
   - A debug build takes an absolute `NGR_AGENT_STATE_DIR` in place of the root for both modes;
     a release build does not read it. The operational log's directory is unchanged.
   - `identity/` holds the lock and the debug store; `journal/` is for the journal item.
2. **Name and value.** The secret is `nekoguruma-agent-key.<mode>.v1`: the mode and a format
   version, no host or account name, so the name means the same on every device. The value is the
   PKCS#8 DER of the key. A later version reads the earlier names before it creates one.
3. **Store interface.** `get` and `put_new` only. There is no replace and no delete, so no path
   of the agent can overwrite or lose a key. `get` answers Absent only when the store positively
   holds nothing; anything else that stops a read is Unavailable (or Undecryptable).
4. **Create once.** Read; only on Absent take an exclusive OS lock on
   `<state>/identity/<name>.lock`, polling every 20 ms for at most 10 seconds; read again;
   generate; `put_new`; read back and use what was read. The lock file is never deleted. A holder
   stuck past the wait gives the waiter an Unavailable error (retryable). A read-back that
   decodes to a different key is adopted, with a warning.
5. **A present key needs neither the lock nor a writable state directory.** Unavailable and
   unusable keys never reach the lock.
6. **The plain-file store** exists in debug builds only and is selected only when
   `NGR_AGENT_KEY_STORE=file` and `NGR_AGENT_STATE_DIR` are both set, so a test that sets only the
   state directory (the journal's) never gets a plain key store. A release build has no store
   until the platform stores land.
7. **`ngr-agent identity --mode <device|user>`** prints one JSON line (`public_key_pem`,
   `fingerprint`, `created`). Exit 0 key found or created; 1 no store for the build or mode, the
   store cannot create (an administrator provisions), randomness unavailable or wrong debug
   variables; 2 command line; 3 unavailable (store, lock or state directory; retry); 4 key
   present but unusable (re-register after an explicit reset).
   `run` and `identify` never touch the store.

## Consequences

- The lock covers only processes that resolve the same state directory. Two processes of one
  account with different `XDG_STATE_HOME` and one Secret Service use two lock files; the Secret
  Service store must read more than one item under the name as Undecryptable, never pick one.
- A key reset must take the same lock. A key removed outside the agent reads as Absent, so the
  next start creates a new key, which the server refuses until it is registered again.
- File-backed stores need hard links in their directory. A creator that crashes after creating
  and before printing leaves a key nobody reported as created (accepted).
- A creator stuck inside its store makes every other start report the store unavailable after 10
  seconds, instead of hanging.
- On Windows a temporary profile or an unmounted home reads as Absent and would create a new
  key; the Windows store work must account for it.
- Registration and connect call the create-once routine and never `put_new`.
