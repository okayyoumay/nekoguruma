# ADR-459: The Agent Key Is ECDSA P-256 and Its Connection Proof Is Clock-Free

**Date:** 2026-11-11
**Status:** Accepted (Decision item 4's create-once rule and state directory given their form by ADR-460; item 5's rename done by ADR-463)
**Affects:** design 6.2, 6.4, 6.7, 12, 14.4, 16.3; `crates/shared-crypto`; `.github/dependabot.yml`; ADR-248 (the auto-merged group); the agent's key stores and the registration and connection flows built on this record

## Context

Design 6.4 gives each agent a key pair that is registered with the server and kept in the
operating system's credential store, and 14.4 names the agent key as the possible client
certificate. Nothing in the repository fixed the algorithm, the encodings, what "signs its
connection" means, or where the key lives for each mode and platform. M3 needs these before
registration and the control channel can be built.

Three facts shape the choices. The local deployment profile has no TLS (design 13.3), so a
client certificate cannot be the mechanism that identifies the agent there. The device clock is
untrusted (design 16.3), so a proof that depends on a timestamp would be trusted only as far as
that clock. And the same primitive will sign job instructions (design 11.1), so the choice
binds more than the agent's connection.

## Decision

1. **Signature.** ECDSA over P-256 with SHA-256, through the RustCrypto `p256` crate, wrapped
   in `shared-crypto`'s own types (`SigningKey`, `PublicKey`, `Signature`, a `Signer` trait) so
   that replacing the primitive touches one crate. P-256 is what TPMs and HSMs support
   generally; Ed25519 is not, and the agent key is meant to move into a TPM (item 7). The
   `p256` crates have not had an independent audit. That is accepted, mitigated by
   known-answer tests in `shared-crypto` (the P-256 vectors of RFC 6979, appendix A.2.5) and by the one-crate
   wrapper; `ring` is the fallback if the risk is judged too high. The same primitive will sign
   job instructions (design 11.1).
2. **Encodings.** The public key is an SPKI DER document, which names its algorithm, stored as
   bytes and carried as PEM in the API. A signature is the fixed 64-byte r followed by s, signed
   in low-S form and accepted only in low-S form, so a signature has one valid encoding. The
   fingerprint is the SHA-256 of the canonical SPKI DER; a non-canonical SPKI is refused so the
   stored bytes and the fingerprint cannot disagree.
3. **Connection proof.** "The agent signs its connection" means a clock-free challenge proof.
   The agent signs a transcript of a domain-separation tag (one tag per signed purpose, applied by `shared-crypto`'s tagged sign and verify), a protocol version, the audience
   (the server's origin string), the agent ID, a 32-byte server challenge and a 32-byte agent
   nonce, with length-prefixed variable fields and no timestamp. The server issues each
   challenge once. `shared-crypto` provides building the transcript, signing and verifying. The
   tag names the purpose, so a proof signature never verifies as a job instruction signature
   under another tag, and the reverse. The wire transport (an HTTP proof exchange that yields
   a short-lived session token for the WebSocket upgrade, long polling and the data channel)
   belongs to the control channel work. M3 authenticates the agent only: the server is
   authenticated by TLS on the standard profile and by loopback on the local profile. Client
   certificates (mTLS, design 14.4) stay an optional addition and are not M3's mechanism,
   because the local profile has no TLS.
4. **Key stores.** The agent reaches its key through a signer over a named-secret store, one
   per platform and mode, with no plain-file fallback in release builds:
   - Windows, both modes: a DPAPI blob in a file, in the user's scope. Device mode runs as a
     virtual service account (`NT SERVICE\<name>`), whose user scope is its own. Machine-scope
     DPAPI is not used: it would let any local user decrypt the blob or plant a key of their own.
     A DPAPI-protected blob is not a plain file in the sense of the ban.
   - Linux user mode: the Secret Service, through `zbus`. A locked collection reads as
     unavailable, never as absent.
   - Linux device mode: a systemd encrypted credential that the administrator provisions. This
     needs systemd 250 or later. Older hosts need a method of their own, which is to be designed
     later.
   Outcomes: a store with no key creates one, under an exclusive lock file (as ADR-255 locks the
   journal); a store that is unavailable is an error the connect loop retries; a key that
   cannot be decrypted is an error that asks for re-registration, and is never overwritten.
   Keys are created only by the commands that need them (the identity command, later
   registration and connect); `ngr-agent run` and `identify` work without a store. A debug-only
   file store and an in-memory store exist for tests.
5. **Mode name.** The name is `device`, as in the design and the command line. The database and
   the Web API description still say `machine`; the registration work renames them.
6. **Dependabot.** `p256`, and later `zbus`, are kept out of the `cargo-minor-patch` group
   that ADR-248 merges by itself. Their updates get their own pull requests for the maintainer.
   Only direct dependencies can be named: Dependabot's Cargo version updates change the
   manifest's own entries. The signing path's transitive crates (`ecdsa`, `elliptic-curve`,
   `digest`, `crypto-bigint`, `sec1`, `der`, `spki` and others) can therefore still move as
   side effects of the lock inside the auto-merged group. That is accepted, because
   `shared-crypto`'s known-answer tests run on every such pull request.
7. **TPM.** A non-exportable key in a TPM is M6 work (design 16.3). Moving to it replaces the
   key, so the agent registers again.

## Consequences

- A key in a store is only as protected as the account that owns it: any process of that
  account can ask the store to sign. The TPM step removes the export of the key, not that.
- A host with systemd older than 250 has no device-mode store until the later design exists.
- The local profile does not authenticate the server to the agent beyond loopback: a local
  process that takes the port receives the agent's events, but it cannot issue jobs, since
  those are signed (design 11.1).
- A new signed purpose in `shared-crypto` must pick its own tag and never reuse the proof's.
- The signature crate `p256` has a different update path from the rest; the crates under it do not (item 6).
