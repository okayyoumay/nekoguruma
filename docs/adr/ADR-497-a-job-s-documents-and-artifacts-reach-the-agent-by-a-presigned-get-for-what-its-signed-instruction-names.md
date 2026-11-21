# ADR-497: A Job's Documents and Artifacts Reach the Agent by a Presigned GET for What Its Signed Instruction Names

**Date:** 2026-11-21
**Status:** Accepted
**Affects:** design 4.1, 5.2, 8.2.6, 13.3, 14.3; `crates/server` (`downloads`, `storage`, the token and agent-row judgment shared with `uploads`), `db/migrations/` and `db/README.md`, `api/openapi.yaml`, `crates/agent` (`fetch`, the agent half, a later change); extends ADR-492 and ADR-493, relies on ADR-454 and ADR-461

## Context

Design 14.3 gives the object store a download leg: the agent fetches an artifact through a URL
the server issues. ADR-492 built the store with presigned part URLs for writing only, and
ADR-493's data channel carries uploads only. The maintainer decided on 2026-11-20 that a job's
IR documents reach the agent by this leg too, rather than by a control-channel message (too
small) or a route that streams them through the server (a second path beside the artifacts').
The signed job instruction names each document by its SHA-256 and the artifact by its hash
(ADR-454, ADR-461), and that signature is the only integrity anchor: the server is not
authenticated to the agent on the local profile (ADR-493 Consequences, design 13.3), so what a
route answers is a hint the agent checks. Nothing stores IR documents yet: a later route does. The
maintainer decided the choices below on 2026-11-21, from a case table of the route's and the
agent's answers.

## Decision

### The server

1. **One more session-token route, `POST /api/v1/agent/downloads`.** The body names the job, the
   ownership generation, the kind (`document` or `artifact`) and the SHA-256; it is a POST so
   that the digest is in no path and the answer, a bearer URL, is never cached. The answer is a
   presigned GET URL and its expiry, as the part URL's answer is. The route judges the token and
   the agent's row as the upload routes do, through one shared judgment (ADR-493 items 2 and 16).
2. **What the job names is what its signed instruction names.** The route decodes the instruction
   the server stored for the job, not the digest columns beside it: a document must be in its
   document hashes, an artifact must be its artifact hash. The generation must equal the
   instruction's; the job's state plays no part, so a write job the server expired can still
   fetch its artifact while the agent restarts it (ADR-487 Consequences).
3. **Every refusal the caller could tell apart is one 404**: an unknown job, another agent's or
   tenant's, another generation, a digest the job does not name, or one no stored object
   answers.
4. **Objects keep the store's own keys.** A content-addressed object is found through the
   existing `ir_documents` and `artifacts` tables, by tenant and digest; the bucket key stays the
   UUID the store issued. A key made of the digest was rejected: a bad second upload to the same
   key would fail its read-back and the store would delete the good object with it (ADR-492
   item 5). A row exists only for an object the store has read back; the route only reads. A
   migration makes each digest 64 lowercase hex digits and each storage key unique within its table.
5. **The URL lasts at most 60 seconds.** This is the store's own request expiry, and it is cut
   to the second, so the URL lapses 59 to 60 seconds after its issue. It is asked for when it is
   about to be used. The route asks the store nothing, so a lost object shows as the agent's
   GET failing.

### The agent (built with the agent half)

6. **The fetch follows the instruction.** The agent fetches what a delivered instruction names
   before the run's first step, with its own token exchange, through the uploader's client and the
   part URL's host rule (ADR-495 items 1 to 3).
7. **The bytes are checked when they are used.** A download is hashed while it streams to a
   temporary file and renamed into a directory named by digest only when it matches; a file
   there is hashed again at every load against the verified instruction's hash and deleted when it
   no longer matches. No interface of the agent returns bytes it has not hashed against that
   hash. The cache is shared by digest across jobs.
8. **Bounds**: a document is at most 16 MiB, an artifact at most 4 GiB. A second digest mismatch
   in a row fails the fetch until a restart; any other failure is classed as ADR-495 item 1
   classes an upload's, and the caller bounds the retries.

## Consequences

- A revoked agent keeps a URL it was given for at most 60 seconds (ADR-492's accepted limit).
  The expiry bounds when a GET may start, not how long it runs: against the bundled object server
  a transfer begun before the expiry completes after it (a test records this).
- A port holder on the local profile can deny, stall or waste disk up to the bound, but cannot
  substitute content.
- There is no minimum transfer rate: a slow download that keeps moving is bounded only by the caller, which cancels or bounds its attempts.
- A cut download starts again from the first byte; identification plans' documents are not
  served; the cache is never evicted. Each is later work.
- A cached object survives a re-registration of the agent.
- Until the route that stores IR documents exists, the tables this route reads are seeded only by
  tests; that route must insert a row only after the store read the object back.
