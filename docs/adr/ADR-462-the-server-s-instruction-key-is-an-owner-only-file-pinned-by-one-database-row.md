# ADR-462: The Server's Instruction Key Is an Owner-Only File Pinned by One Database Row

**Date:** 2026-11-12
**Status:** Accepted
**Affects:** design 11.1, 13.3, 16.3; `crates/server` (`instruction_key.rs`, `config.rs`, `lib.rs`), `db/migrations/0003_instruction_keys.sql`, `db/README.md`, `crates/server/docs/ngr-server.md`, `docs/glossary.md`; extends ADR-456 (Decision item 7) and ADR-458 (start order); fulfils ADR-461 Decision item 14; scopes ADR-459 Decision item 4

## Context

ADR-461 gives the job instruction its signed form and the agent its gate, and leaves the signer
unbuilt: nothing on the server holds the private key. Design 11.1 says the agent trusts the key it
received at registration, and design 16.3 wants the key kept away from the server's data and
eventually in an HSM or a signing service. The key has to exist once per deployment: two servers
of one deployment, or a restart that finds another key, must not sign under two identities,
because every registered agent pins one key.

## Decision

1. **Location.** The private key is a PKCS#8 file at an absolute path named by a required
   `[instruction_key]` table (`path`) in the server's configuration. There is no default path, no
   search and no environment variable (ADR-456 Decision item 7); the table holds a path, never key
   material, so ADR-456's rule for secrets in the file needs no addition. It is read once, at
   start. Rejected: a PostgreSQL row (every reader, dump and backup of the database would become
   a job issuer, and anyone who can write a row could forge `jobs.signed_job`; design 16.3 moves
   keys away from the server's data); an operating-system credential store (no release store
   exists yet, and a service has no session bus); a built-in state directory (it contradicts
   ADR-456 Decision item 7, and `/var/lib/nekoguruma` is the agent's device root, ADR-460).
2. **Protection is permissions.** On Unix the server creates the file with mode 0600 and refuses
   a path that is not a regular file, has any group or other permission bit, or whose directory
   is writable by group or others; the checks run on the opened file. This is the level of the
   database password in the same configuration file (ADR-458 Decision item 2). The reader is the
   server's account alone. A personal installation where a user-mode agent runs under that same
   account is accepted (design 6.6). On Windows the file is a plain file protected by the ACL of
   its directory from installation, and the code checks no ACL; wrapping it with DPAPI is
   follow-up work. ADR-459 Decision item 4's rule against a plain key file is scoped to the agent
   key, whose store is per account and per mode; it does not apply to this key.
3. **Who creates it.** The server, and only when the file is absent and the database records no
   active key. The directory must already exist.
4. **One key per deployment.** A new table `instruction_keys` (`key_id` primary key, `public_key`
   the SPKI DER, `created_at`, `retired_at`) with a partial unique index that allows one row whose
   `retired_at` is null. It has no tenant ID: the key is per deployment (ADR-461 Decision item 7),
   the first exception to the tenant rule in `db/README.md`. Retired rows are kept as history:
   they explain old `jobs.signing_key_id` values and leave room for the overlap set that ADR-461
   Decision item 7 mentions.
5. **Create once without a lock file.** The creator writes the PKCS#8 bytes to a temporary file
   of a unique name in the key's directory (mode 0600, synced), hard-links it to the final name,
   removes the temporary file and syncs the directory. A link that finds the name taken means
   another creator won, and its key is loaded instead. The server then inserts the public key
   (`ON CONFLICT DO NOTHING`), reads the active row back and compares it with the loaded key; the
   row is the arbiter between servers that use different files. The agent's create-by-link helper
   is crate-private and uses a fixed temporary name, which races, so the server has its own. A
   server that finds the file absent while a row exists reads the file once more before it
   refuses, since a creator links the file before it records the key.
6. **Outcomes at start** (order: configuration, database connection, migrations, key, bind; a
   listening port also means a usable key, and nothing is retried in the process, as in ADR-458
   Decision item 4):

   | File | Active row | Result |
   |---|---|---|
   | absent | none | created, recorded, logged as created |
   | valid | same key | start |
   | valid | none | recorded, start |
   | absent | present | exit 1 |
   | unreadable, not a P-256 PKCS#8 key, or too open | any | exit 1; the file is never overwritten or regenerated |
   | valid | another key | exit 1 |

   An error is fixed text plus the `io::ErrorKind` of a failed file operation: no key bytes and
   no file contents.
7. **Signer.** `server::InstructionKey` wraps an `Arc<dyn shared_crypto::Signer + Send + Sync>`
   (ADR-459 Decision item 1) and is built from one with `InstructionKey::from_signer`, which tests
   and a later signing service use. `seal(&JobInstruction)` is `async` now, because a signing
   service is network I/O and the job queue's call sites then never change.
   `shared_crypto::instruction::seal` asks the signer for its public key on every call, so a
   signer that reaches a module or a network must cache it.
8. **Interface.** `AppState.instruction_key` is an `Arc<InstructionKey>` with `public_key()`,
   `key_id()` (64 lowercase hex digits, the fingerprint, the value of `jobs.signing_key_id`) and
   `seal()`. Registration, a later item, returns `public_key().to_spki_pem()`; the Web API change
   belongs to it.
9. **Exposure.** One info log line with the key ID at every start. There is no endpoint and no
   command for it in this change.
10. **Rotation and reset.** Rotation stays re-registration (design 11.1). A reset is a documented
    manual procedure in `crates/server/docs/ngr-server.md`: stop the server, set `retired_at` on
    the active row, remove the file, start. The key file is in neither a database dump nor the
    export of design 13.3: losing it costs re-registration of the agents, not data.
11. **Handling.** Key bytes live in wiped buffers and in `shared_crypto::SigningKey`, whose
    `Debug` output is redacted; `InstructionKey`'s shows only the key ID.

## Consequences

- A file moved between hosts without its database row, or a database restored over a lost file,
  stops the server: the operator restores the file or follows the reset procedure.
- A server whose files differ in two directories (two hosts, one database) is refused at the
  second start, not at the first job.
- A reset leaves jobs that were signed under the retired key: the job queue must end or refuse
  them (backlog), since agents registered afterwards pin the new key.
- Registration over unauthenticated loopback is trust on first use (design 14.4): a process that
  holds the port during registration can hand the agent its own key. The server-authentication
  work closes this.
- The Windows key file stays unwrapped until DPAPI wrapping lands.
- File systems without hard links cannot hold the key file; the start fails with the link error.
