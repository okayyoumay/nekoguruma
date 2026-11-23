# ADR-505: The Object Server Speaks TLS Only, With a Certificate the Server Creates and the Agent Learns From a Proved Answer

**Date:** 2026-11-23
**Status:** Accepted (items 9 to 14 added with the verifier and the agent's dial)
**Affects:** design 13, 13.3, 14.3, 14.4; `crates/server` (`object_server`, `storage/s3`), `crates/agent`, `api/openapi.yaml`, CLAUDE.md "Building and testing"; annotates ADR-491, ADR-492, ADR-493, ADR-497, ADR-504

## Context

ADR-504 item 5 rests the server's answer proof on the server's own listener being unshared. The
bundled object server (RustFS 1.0.6, ADR-491) is a separate process whose listener options are not
ours. A probe against the real binary showed:

- On Linux a second socket of the same account with `SO_REUSEPORT` binds RustFS's
  `127.0.0.1:<port>`. Another account is refused, since Linux shares a port-reuse address only
  within one effective user.
- On Windows a second socket with `SO_REUSEADDR` binds it, because RustFS opts its listener into
  address sharing, which under Windows' rules probably opens it to other accounts too.
- RustFS has no setting that turns either off.

A process beside the object server on that port, or one that takes the port after the object
server is gone, can receive an agent's part bytes and the part's presigned URL (a 300-second bearer
that also lets the holder overwrite the part), a download's presigned URL (60 seconds), and the
server's own store traffic, which is plain HTTP over presigned URLs too: document bodies,
read-backs, and replayable signed get, delete and abort URLs. Integrity is already detected (the
completion's read-back against the SHA-256 declared on a proved route; the instruction's hash on
downloads). Confidentiality and availability are not. The window after the whole stack has died is
the same exposure.

## Decision

1. **Nothing rests on the object server's listener options.** The object server speaks TLS only.
   The server starts RustFS with `RUSTFS_TLS_PATH` naming a directory that holds a P-256 private
   key and a self-signed certificate for `127.0.0.1`, which the server creates (the file names
   RustFS reads are `rustfs_cert.pem` and `rustfs_key.pem`). The files are owner-only under
   ADR-462's file rules, beside the credential files of ADR-491 item 3. There is no rotation: as
   for the credentials, deleting the files and restarting makes new ones.
2. **The store client and the readiness probe pin the certificate.** The server's store client and
   its readiness probe connect with TLS and accept only that certificate, by its pin. The object
   server itself refuses plain HTTP. Presigned URLs are `https`.
3. **The pin is the SHA-256 of the leaf certificate (DER).** The verifier compares the pin and
   still checks the handshake signature with the certificate's key: a verifier that skipped the
   signature would accept anyone who presents the public certificate.
4. **The agent learns the pin only from a proved answer (ADR-504).** The part-URL 200 and the
   download 200 carry the pin in the proved body, in every such answer, since the port holder can
   change between requests. The agent dials the object server directly with TLS (not through a
   server proxy) and refuses a plain object URL. Workers are untouched (design principle 6).
5. **The TLS stack is rustls with the aws-lc-rs provider, and rcgen makes the certificate.**
   aws-lc-rs is rustls' default provider, is maintained by AWS, has a FIPS-validated mode and
   supports RSA, so the standard profile's external S3 endpoint can use the same stack. Its C code
   is compiled even by `cargo check`, so the local Windows type check needs the pinned llvm-mingw
   compilers on `PATH` (`scripts/fetch-llvm-mingw.sh`, pinned in `scripts/llvm-mingw-pin.txt`, the
   release CI's `worker-windows` job uses); CLAUDE.md says so.
6. **No real data is enqueued by the agent until its side is built.** A result or a log, in a build
   that is not a test, is enqueued on neither OS until the agent side of this record is built.
7. **Hardening upstream.** The maintainer reports the Windows address reuse to the RustFS project
   as hardening; nothing here depends on it.
8. **Built in steps**, each a change of its own: the server's TLS and its store client; the pin in
   the routes and `api/openapi.yaml`; the agent's TLS with tests against an impostor.

This record annotates ADR-491 (items 2, 3, 6 and 8, and the residual about the port), ADR-492
(item 4), ADR-493 (item 13), ADR-497 (item 6) and ADR-504 (Consequences). ADR-463 item 13 is about
the server's own origin and is unchanged.

### Decisions taken with the verifier and the agent's dial

9. **The verifier judges the leaf only.** A certificate is accepted when its DER's SHA-256 equals
   the pin. Its validity dates and names are not checked (the certificate is self-signed for the
   loopback address, has no issuer to trust, and is named by the pin), and certificates after the
   leaf carry no authority: a chain whose leaf is the pinned one is accepted whatever follows it,
   and one whose leaf is another is refused whatever follows it. The handshake signature is still
   checked with the leaf's key (item 3).
10. **TLS 1.3 only, with no session resumption.** The client offers no other version, so the
    handshake signature is always checked in the one place, and a connection is judged against its
    own pin in a full handshake, never by a session another pin made. No ALPN is offered and the
    server name is the loopback address, for which no SNI is sent.
11. **The handshake is inside the connect bound.** The agent's dial and the handshake share the
    5-second connect limit of ADR-493 item 12; a peer that accepts the connection and says nothing
    ends as the connect timeout. The no-progress clock counts the socket's bytes, so TLS sits above
    the byte counting and above the dial seam (the loopback literal stays where it is). No byte of
    the request is written before the server is accepted.
12. **A refused handshake is an ordinary retry.** It is not ADR-504's unproven class and has no
    cap: no session token goes to the object port, so a peer that refuses or fakes the handshake
    learns nothing that a retry would give away further. The words are fixed (certificate,
    signature, handshake) and name nothing the peer sent.
13. **The pin is compared with `==` and written as 64 lowercase hexadecimal digits.** It is a
    public value, so a constant-time comparison protects nothing. Any other text (63 or 65 digits,
    uppercase, a space, a non-ASCII character) is refused rather than normalized, so one pin has
    one spelling in the proved answers.
14. **The verifier is its own crate, `ngr-pinned-tls`.** It is the one crate whose non-test code
    depends on rustls, and it serves both the server's store client and probe and the agent, so the
    two sides cannot disagree on what a pin accepts. Its `test-server` feature makes self-signed test certificates and a TLS
    acceptor, including an impostor that presents a certificate it holds no key for, so that a
    caller's tests need no TLS crate of their own.

### Alternatives rejected

- Providers other than aws-lc-rs: ring (a C build too, no release since March 2025, otherwise
  acceptable); rustls-graviola (pure Rust, but only x86_64 and aarch64 with CPU extensions that
  not every target has, and no 32-bit); rustls-rustcrypto (its own README says it is not for
  production, and it is a pre-release); rustls-pure (released a month ago, one maintainer, one
  dependent).
- A digest in the proved answer: integrity is already detected.
- Per-object encryption: it changes the stored format and leaves the server's own hop and the
  bearer URLs exposed.
- Pre-binding the port or handing RustFS a socket: the binary has no inherited-socket support, and
  an exclusive pre-bind blocks RustFS itself.
- Proxying object traffic through the server's own listener: the proxy-to-RustFS hop is the same
  shared address; only TLS on that hop would fix it.
- A patched or other object server: our own builds of a large binary for four platforms.
- No bundled object server on Windows: a second, Windows-only data path. It is the last resort if
  RustFS's TLS failed on Windows; a test against the real binary shows it serves over TLS on Linux
  and on Windows.

## Consequences

- A process that shares the object server's port, or takes it later, can only deny service. The
  window after the whole stack has died closes with the agent side.
- The agent and the server gain a TLS stack and a C build.
- The sharing probe becomes a test that a sharer receives nothing, in the server's step.
- Whoever can read the key file already had what the credential files give (ADR-462).
- ADR-504 item 5 stays as it is for the server's own listener.
