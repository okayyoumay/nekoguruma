# ADR-507: The Windows Key Is a DPAPI Blob File in the Account's State Directory, Created Once and Tied to a Persistent Profile

**Date:** 2026-11-23
**Status:** Accepted
**Affects:** design 6.4; `crates/agent` (`identity::blob_store`, `identity::dpapi`, the store selection in `identity`, `state::root_from`, `guards::open_read_only`, `tests/windows_service_account.rs`), `crates/agent/docs/ngr-agent.md`, `.github/workflows/ci.yml` (`core-windows`), `CLAUDE.md` ("Building and testing"); details ADR-459 item 4 and ADR-460

## Context

ADR-459 item 4 names a DPAPI-protected blob file in the account's own scope as the Windows key
store, in both modes, and leaves its form open. ADR-460 fixes the directory (the `identity`
sub-directory of the state root, `%LOCALAPPDATA%\nekoguruma` of the account in both modes), the
interface (read answers absent only on a positive answer; create never replaces) and the
create-once routine, and records a hazard it leaves to this work: a temporary profile or an
unmounted home reads as an absent key and would make a new one, which a profile reset then
discards.

The device mode runs as the virtual service account `NT SERVICE\<service name>`, so the store
must work for an account that has no interactive logon. Whether such an account has a loaded
profile, a `LOCALAPPDATA` and a DPAPI master key is a fact about Windows that the repository
could not assume, so a gated test measures it (Decision item 11). Nothing Windows-specific runs
on the development machines the code is written on; the policy is therefore plain `std` behind two
traits, tested everywhere with fakes, and only the operating-system calls are Windows-only.

Probe results (core-windows, runner image: to be filled in from the first CI run of
`windows_service_account`): whether `NT SERVICE\<name>` has `LOCALAPPDATA`; what `whoami /user`
and `set` print; the codes DPAPI returns for a changed blob, for a different entropy and for
another account's blob.

The maintainer approved the choices below on 2026-11-23.

## Decision

1. **The file.** `<state root>\identity\<name>.dpapi`: the 8 bytes `NGRDPAPI`, one format byte
   (1), then the DPAPI blob to the end of the file. At most 16 KiB are read, as for the systemd
   credential (ADR-498). The folder is ADR-460's and is not resolved by the store: the agent's
   `state` module resolves the root (for both modes `%LOCALAPPDATA%\nekoguruma`, or the debug
   `NGR_AGENT_STATE_DIR`) and hands it over. The store sets no ACL; the file inherits the
   profile's.
2. **DPAPI's form.** `CryptProtectData` and `CryptUnprotectData` with `CRYPTPROTECT_UI_FORBIDDEN`
   and no other flag (never `CRYPTPROTECT_LOCAL_MACHINE`, so the blob belongs to the account), no
   description and no prompt structure. The optional entropy is the bytes
   `nekoguruma-secret-store-v1`, a zero byte and the secret's name: a domain separator, not a
   secret, so a blob copied under another name, or made by another program, does not open.
3. **Creation.** `put_new` checks the profile (item 6), protects the secret, then creates the
   file with `journal::create_linked`: written and synced under `<name>.dpapi.tmp`, hard-linked
   to the final name without replacing, the temporary removed. A crash leaves at most the
   temporary, which reads as absent and is removed by the next create. When two creators race
   outside the lock, one link wins and the other gets "already exists" or "unavailable";
   `load_or_create` reads back and adopts what the store holds. Hard links are required of the
   file system (the journal's rule, `journal_dir`).
4. **Reading** opens the file without following a reparse point and judges the handle
   (`guards::open_read_only`, the lock files' rule): a link, a directory or any other I/O error is
   unavailable. A file longer than the cap, shorter than the header and a blob, with a wrong
   magic or an unknown format byte (a plain PKCS#8 document, an empty file) is undecryptable.
5. **Classification of the operating system's refusal.** `CryptUnprotectData` failing with
   `ERROR_INVALID_DATA` or `NTE_BAD_KEY_STATE` is undecryptable; any other code is unavailable. The
   list is closed and starts with these two, so an unknown failure is retried and never reported
   as a lost key; a code the CI measures to mean "cannot open" is added to it. Error sources are
   fixed text, an `io::ErrorKind`, or fixed text with the code in hexadecimal; no new
   `StoreErrorKind` and the exit codes 3 and 4 are unchanged.
6. **The profile rule.** `GetProfileType` is read when it matters: a missing file is absent only
   if the root's parent directory exists and the profile is local or roaming; with the parent
   missing (a drive or profile not mounted), a temporary or mandatory profile, or a profile type
   that cannot be read, a missing file is unavailable. `put_new` requires a persistent profile.
   The rule gates the absent answer and creation, never a key that is present: a blob that is
   there is read without the check.
7. **Zeroization.** The decrypted secret is copied once into a buffer of exactly its size that is
   wiped when dropped, and a guard wipes the buffer DPAPI allocated before `LocalFree` frees it.
8. **Layout.** `identity::blob_store` (compiled on Windows and in tests) holds the store, the
   format, the classification and the profile rule behind two traits, `Protect` and
   `ProfileProbe`; `identity::dpapi` (Windows only) holds the operating-system calls, each
   `unsafe` call on the smallest item. `windows-sys` is the agent's only new dependency, for
   Windows only (it is already locked for other crates).
9. **Selection.** A release build on Windows selects this store in both modes and reads no
   `NGR_` variable. A debug build selects it only with `NGR_AGENT_KEY_STORE=dpapi`, so a test
   never writes into the account's profile by accident; the value on another platform is no
   store. The release choice is one function that debug test builds compile too, so the tests
   check it on every platform.
10. **Tests.** The store's cases run on every platform against a fake protector and a fake
    profile probe; the real DPAPI, the binary's second-process and racing-process cases, a changed
    or copied blob and a symbolic link run in `core-windows`. A real temporary profile is not
    tested.
11. **The gated test.** `tests/windows_service_account.rs`, active only with
    `NGR_TEST_WINDOWS_SERVICE_ACCOUNT=1` and then failing rather than skipping, creates a
    demand-start service under `NT SERVICE\<name>` that records `set`, `whoami /user` and the
    debug `ngr-agent identity --mode device`, starts it twice (the key is created, then found),
    reads the service's blob as the CI account (undecryptable) and prints what it found. A
    `core-windows` step runs it after the main runs, on pull requests too.

## Consequences

- A reset or loss of the account's profile loses the key: the next start reads an undecryptable
  or absent key. A blob that cannot be opened is exit 4 and asks for registering again; a blob that
  is gone on a persistent profile is a new key.
- The key's integrity rests on the profile's ACL: the account, an administrator and anything that
  can act as them can read the file, replace it or plant a key. Machine-scope DPAPI would be
  worse (any local user could open or plant a blob), and is not used.
- A temporary or mandatory profile is never given a key, but a real one is untested; the check
  rests on `GetProfileType` and the injected-flag tests.
- Hard links are required of the state directory's file system.
- The creators that race outside the lock share one temporary name, so a creator may link a file
  another is still filling and be told "created" for a secret that is not its own. What protects
  `load_or_create` is its creation lock (`<root>\identity\<name>.lock`), not its read-back: without
  the lock it could read a half-written file back and fail as undecryptable (exit 4). Every
  caller of the store's create step therefore holds that lock, and so must any other user of the
  format.
- A device-mode command run from another account's shell (an administrator's) makes an
  unrelated key in that account's profile, which the service never reads.
- The agent's logger makes the state root and its log directory (with any missing parent)
  before the store is read, unless logging is off or fails, so the rule that a missing parent of
  the root is unavailable guards only those runs and the tests; the profile check is what
  protects a running agent.
