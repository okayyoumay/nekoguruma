# ADR-470: The Agent Registers in a Fixed Order That Spends the Code Last

**Date:** 2026-11-12
**Status:** Accepted
**Affects:** design 6.7; `crates/agent` (`registration`, the server HTTP client, `ngr-agent register`), `crates/agent/docs/ngr-agent.md`; `api/openapi.yaml` (`AgentRegistration`); `docs/glossary.md`; details ADR-463 items 13, 16, 17 and 18

## Context

ADR-463 fixes the agent's registration: the command and its flags, the code's input, the record
and its write, the refusal to overwrite without `--replace`, and exit codes 5 (already
registered) and 6 (server key mismatch) on top of the identity command's 0 to 4. It leaves open
the order of the steps, what each answer of the server means for the exit code, when `--replace`
takes effect, how the lock waits, the client's limits, what a terminal on standard input does,
whether the record keeps the signed `allowRemote`, and what stands for mode 0600 on Windows.

Two facts constrain the answers. A redemption spends the code, and with the same key revokes that
key's earlier agent row in the same transaction (ADR-463 item 14), so anything the agent can
still fail after sending must be checked first. And an honest server that is given the wrong
`--server-key` refuses with its uniform 403, because the proof covers the server key ID: an
exit 6 needs an answerer that returns a 201 with another key or a forged signature.

## Decision

1. **A fixed order, the code spent last.** The command line; the agent key (ADR-460's
   create-once routine); the code from standard input or `--code-file`; the registration lock;
   the existing-record check; the temporary file; one request; the answer's checks; the write.
   The first step that fails decides the exit code, and every step that can fail without the
   server is done before the request. The key comes before the lock, so a build without a key
   store exits 1 without touching the state directory; ADR-463 item 16's lock "for the whole
   command" is read as from the record check to the rename. A key that this run had to create
   stops the command before anything is sent (exit 1), since no code can have been issued for
   it yet.
2. **`--replace` acts only at the rename.** It lets the record check pass; the old record is
   replaced only by the final rename, after the answer has passed every check, so no failure
   destroys a working record. The server-side revocation of item 14 still happens whenever the
   server committed.
3. **The lock is tried once.** `<state>/registration/registration.lock`, exclusive, held to the
   end and never deleted; a lock that is held is exit 3 ("another registration is running"), and
   that run has sent nothing.
4. **One request over a stream the agent opens.** hyper's HTTP/1 client connection over a TCP
   stream to `127.0.0.1:<port>`, with no pool, no name resolution, no retry and no redirect
   followed; `Host` is the pinned authority and no Origin is sent. Limits: 5 seconds to connect,
   30 seconds in all, a head of at most 16 KiB and 64 fields, a body of at most 4096 bytes. The
   origin rule is `Audience::canonicalise` followed by an exact comparison with `http://127.0.0.1`
   or `http://127.0.0.1:<port>`, so port 80 and the form without a port, which is how the server
   pins port 80 (ADR-456), are accepted.
5. **The answer maps to an exit code.** 201 goes on to the checks; 403, 429 and every other 4xx,
   3xx, 1xx and 2xx are exit 1 (a refusal: retrying with the same code is pointless, and a
   429 comes only to a request the server would refuse); 408, 5xx, a transport failure and a cut
   or late answer are exit 3, with a text that says whether anything was sent. A 201 is read
   strictly (the documented fields and no other, `additionalProperties: false`); a key whose
   fingerprint differs from `--server-key`, and a response signature that does not verify, are
   exit 6, which now means "the answer is not authentic"; any other defect is exit 1.
6. **The record.** One snake_case JSON object: the agent ID, the owner ID, the preset,
   `allow_remote`, the server origin, the instruction key's PEM and ID, and the agent key's
   fingerprint. `allow_remote` is added to ADR-463 item 16's list because the server signs it
   and, for the `shared` preset, the preset does not determine it. The mode is not stored: a
   new mode means registering again (design 6.7). It is written to a temporary file created
   before the request, synced, renamed over the final name, and the directory synced; on Windows
   the profile's inherited permissions stand for mode 0600, as for the key store (ADR-460).
7. **Nothing the operator typed or the server said is echoed.** A terminal on standard input is
   refused with exit 2, as for passwords (ADR-469); no argument value, no code in any form and no
   server text other than the status and `Retry-After` is printed or logged.

## Consequences

- A mistyped `--server-key` reads as a refusal (exit 1), not as exit 6; exit 6 is tested against
  a stand-in answerer.
- A registration the server committed but the agent could not verify or store needs a new code,
  and with the same key has already revoked the earlier agent row.
- Exit 3 covers "nothing was sent", "the outcome is unknown" and "registered but not stored";
  the text tells them apart.
- A crash after the rename leaves a record that nobody printed; the next run exits 5.
- A key reset must take the registration lock as well, and the control channel session must
  decide whether `register --replace` may run while it is connected.
