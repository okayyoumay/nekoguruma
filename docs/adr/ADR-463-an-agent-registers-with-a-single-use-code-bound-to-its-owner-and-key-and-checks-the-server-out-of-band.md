# ADR-463: An Agent Registers With a Single-Use Code Bound to Its Owner and Key, and Checks the Server Out of Band

**Date:** 2026-11-12
**Status:** Accepted (items 5 and 14 detailed by ADR-465; item 13's transport rule applied to the audience by ADR-466; item 5's limiter rule not applied to passwords, ADR-469)
**Affects:** design 6.4, 6.6, 6.7, 11.1, 13.3, 14.4; `api/openapi.yaml` (`/v1/agents/register`, `AgentRegistration`, `Agent.mode`); `db/migrations/0004_agent_mode_device.sql`, `db/README.md`; `crates/shared-crypto` (`registration`); `crates/shared-proto` (`AgentMode`); `docs/glossary.md`; the server's registration code, `registration-code` subcommand (`main.rs` usage, `crates/server/docs/ngr-server.md` "Running"; ADR-456 item 7 stays, since `--config` stays) and route, and the agent's `register` command (later work); relies on ADR-458 (the command never migrates); annotates ADR-459 (Decision item 5), ADR-461 (item 11) and ADR-462 (item 8 and the first-use consequence)

## Context

ADR-459 to ADR-462 built the pieces registration needs: the agent key and its store, the job
instruction and its gate, and the server's instruction key. Nothing joins them. Looking at what
registration has to do showed that the backlog item and ADR-462 had understated it:

- **Server authentication does not close trust on first use.** ADR-462 says the server-authentication
  work closes the first-use risk of registration over unauthenticated loopback. It cannot: a
  process that holds the port during registration answers it, the agent pins that process's
  instruction key (ADR-461 item 7), and every later check authenticates the port holder.
  Registration needs an anchor of its own that does not travel through the port.
- **A Web UI is not out of band on the local profile.** The browser reaches the server through the
  same port as the agent. A command that reads the database is out of band.
- **The connection proof cannot be reused.** `ProofTranscript::new` refuses an empty agent ID, and
  no ID exists before registration.
- **The rename is wider than the backlog said.** `shared_proto::AgentMode` still had `Machine`,
  beside the database check and the Web API enumeration.
- **Nothing is seeded.** No tenant or operator row exists, and `audit_log` has no partition, so a
  registration cannot assume either.
- **An unsigned answer can be rewritten.** The code is bound to the agent key, but a process that
  holds the port can replay the agent's request to the real server within the code's life and
  answer the agent with the real instruction key and a rewritten `ownerId`, `preset` or
  `allowRemote`. The key check alone does not catch it, since the key is the real one.
- **The agent has no HTTP client.** hyper-util's legacy client is already in its dependency tree
  through tonic.

## Decision

1. **Issuance is an administrator command.** `ngr-server registration-code --config <file> --owner
   <subject> --name <display name> --agent-key <fingerprint hex> [--ttl-minutes N]` writes to the
   database directly. It never migrates, and it refuses unless `instruction_keys` has an active
   row. It prints the code, the expiry and the server key ID. Whoever can read the configuration is
   the Administrator (design 6.8). Rejected: a Web API route (it needs sessions, so it is a
   backlog item for the standard profile); a bootstrap code printed at the server's first start.
2. **The code names its owner.** The owner is required at issuance. The server writes a new column
   `agents.owner_id`, keeps `registered_by` for a logged-in issuer (null for the command), and
   inserts the owner into `agent_allowed_operators`. The agent's tenant is the owner's.
   Production issuance needs an `operators` row, which arrives with local users; tests insert one.
3. **Listing agents is not part of registration.** `GET /v1/agents` is never served without a
   session (design 13.3). It is built on the local-users session and filtered by
   `agent_allowed_operators`, and waits for local users.
4. **Code format.** 128 random bits as 26 Crockford base32 characters with a version prefix
   (`ngr1-`), copied and pasted. The canonical form is the prefix in lower case and the digits in
   upper case (the first digit is 0 to 7, since 26 digits hold 130 bits). One parser,
   `shared_crypto::registration::RegistrationCode::parse`, serves both sides: it trims ASCII
   whitespace at both ends, reads prefix and digits without regard to case, reads `I` and `L` as
   `1` and `O` as `0` (Crockford), and refuses a separator inside the code and `U`. The code's
   hash is the SHA-256 of the canonical text, so every typed variant of one code hashes alike.
   Short typed codes belong with the tray UI and the device flow.
5. **Expiry and limits.** Ten minutes by default, 1 to 60 at issuance, judged by the database's
   clock. Single use is one conditional update in the transaction that inserts the agent. The
   route has an in-memory limit on refusals as a whole (10 per minute, then 429) and a 4 KiB body
   limit. Every refusal is one uniform response; the reason goes only to the server's log,
   rate-bounded like the health check's log line. On the local profile entropy, not an attempt
   count, stops guessing (design 14.4). Only 403 refusals count toward the limit. A request that
   redeems successfully (valid code, valid proof) succeeds even while the limit is exceeded: the
   limit only slows guessing, and 429 and 403 are both failures, so the limit gives a guesser no
   oracle.
6. **Storage.** A table `agent_registration_codes` keyed by the SHA-256 of the code (no slow hash:
   the input is random). Columns: tenant, owner, display name, the bound agent key's ID, the ID of
   the instruction key it was issued under (which must still be active at redemption), the times
   of creation, expiry, use and revocation, and the agent ID once used. The rows are kept as the
   record of the registration; no `audit_log` row is written.
7. **Request.** `{code, publicKey (SPKI PEM), mode, proof}`. `capabilities` is not part of the
   request: the agent reports it after it connects. There are no host or account hints; the display name comes
   from issuance.
8. **Proof of possession.** A signature under a new domain tag in a new `shared-crypto` module,
   `registration`, beside `proof`, through `sign_tagged` and `verify_tagged`. The transcript is
   length-prefixed like the proof's: a version, the SHA-256 of the code, the agent key's
   fingerprint, the mode and the server key ID. It has no audience and no server challenge: the
   single-use code is the freshness. Tests cover the tag against the connection proof's and the
   instruction's, in both directions. The signature travels as standard padded base64 of its 64 bytes. The server key ID in the
   transcript is the 32-byte fingerprint, and the mode is a closed enum in `shared-crypto`
   (`shared-crypto` may not depend on `shared-proto`). `verify_registration` refuses a transcript
   built for another key than the one it is given.
9. **Agent ID.** The database's `gen_random_uuid()`, never client-supplied, returned as canonical
   lowercase text; the agent checks the form. (PostgreSQL 13 or later; CI runs 16 and 17.)
10. **Response.** 201 with the new schema `AgentRegistration {agentId, ownerId, preset,
    allowRemote, instructionKey (SPKI PEM), instructionKeyId, signature}`; the route has
    `security: []`, and `signature` is item 21's. The preset is `personal` in M3, in both modes
    (design 6.7); a preset chosen at issuance is later work.
11. **Server authenticity.** The operator gives the agent the server key ID (`--server-key <64
    hex>`, required for an `http://` server). The agent first compares the returned key's
    fingerprint with it, then verifies the response signature (item 21) against that key. It
    stores nothing when either check fails.
12. **The code is bound to the agent key.** At issuance, by the fingerprint that `ngr-agent
    identity` prints; required on the local profile. A port holder that reads the code cannot
    register a key of its own.
13. **Transport.** The M3 agent accepts exactly `http://127.0.0.1:<port>` (ADR-456's pinned
    origin) and treats a redirect as an error; `https` is refused until the standard profile
    picks its TLS. The client is hyper and hyper-util, HTTP/1 only, with no reqwest. `--server` is
    a flag in M3.
14. **A key that registers again.** A registration with the same key revokes that key's earlier
    unrevoked rows in the same transaction, and a partial unique index on `public_key WHERE
    revoked_at IS NULL` backs it. A different key after a reset leaves the old row until
    revocation exists.
15. **A lost response is not replayed.** The agent needs a new code; item 14 cleans up the orphan
    row.
16. **Stored result.** One record, `<state>/registration/registration.v1.json`: the agent ID, the
    owner ID, the preset, the server origin, the instruction key's PEM and ID, and the fingerprint
    of the agent key; the response signature is verified before the record is written and is not
    stored. Mode 0600, written to a temporary file, synced and renamed, under an
    exclusive lock held for the whole command. ADR-461 item 11 reads: the PEM is a field of this
    record.
17. **Overwrite.** The command refuses when a record exists unless `--replace` is given.
18. **Agent command.** `ngr-agent register --mode <device|user> --server <origin> --server-key
    <hex> [--replace]`, one JSON line on standard output. The code comes from standard input or
    `--code-file`, never an argument (argument visibility is a Linux fact; standard input is right
    on both platforms). Exit codes are the identity command's 0 to 4, plus 5 (already registered)
    and 6 (server key mismatch). A release agent has no key store, so `register` exits 1 there.
19. **Rename.** `machine` becomes `device`: migration 0004 drops the mode check, updates the rows
    and adds the new check; `Agent.mode` in `api/openapi.yaml`; `shared_proto::AgentMode::Device`.
    The column `machine_hint` and the vendor profile's `machineModeSupported` keep their names;
    they describe a host and a library, not the agent's mode. No deployed database holds an
    `agents` row, since no route writes one.
20. **End-to-end test.** A database test in `crates/server/tests/postgres/` runs the `ngr-agent`
    binary (`crates/agent/tests/support/ngr_agent_binary.rs`) with `NGR_AGENT_KEY_STORE=file` and
    `NGR_AGENT_STATE_DIR` set per child process.

21. **The server signs the response.** A second transcript in `shared_crypto::registration`,
    under a new tag (`nekoguruma agent registration response`, distinct from the request tag, the
    connection proof's and the instruction's): the code's SHA-256, the agent key's fingerprint,
    `agentId`, `ownerId`, `preset` and `allowRemote`, length-prefixed with an internal version.
    The server signs it with its instruction key and returns it as `signature` (standard padded
    base64 of 64 bytes) in `AgentRegistration`. The agent verifies it against the instruction
    key it received, after the fingerprint check of item 11, and stores nothing when it fails.
    This answers the replay in the Context: a port holder cannot change what the server assigned
    without breaking the signature, and cannot sign for the real key.
22. **A 400 is decided on shape alone.** Before and without any database lookup: the code's
    prefix, length or alphabet, an unparsable PEM, bad base64, a signature that is not 64 bytes,
    a mode outside the enum, an unknown field.

## Consequences

- Registration has its own out-of-band anchor, the server key ID. ADR-462's statement that the
  server-authentication work closes first use is answered here: that work protects the connection
  after registration, not registration.
- The agent needs a minimal HTTP client and a registration record, both later work; this record
  and its pull request carry the specification, the rename and the signing module only.
- A reset of the server's instruction key stays re-registration: agents registered afterwards
  pin the new key, and a code issued under the retired key no longer redeems (item 6).
- A key that is registered again, then lost, leaves orphan rows until revocation exists (item 14).
- A code printed in a terminal is visible to whoever can read that terminal; the binding to the
  agent key (item 12) limits what that is worth.
- The mode is chosen by the agent in its request, not bound at issuance. A later rule that reads
  `agents.mode` for an unattended device must bind the mode at issuance first.
- A registration that redeems during a flood of refusals still succeeds (item 5), so the limit
  slows an attacker without locking the operator out.
- The job queue must not deliver to a revoked agent; the revocation work owns that rule.
