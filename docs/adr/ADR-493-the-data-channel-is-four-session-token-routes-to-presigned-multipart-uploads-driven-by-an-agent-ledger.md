# ADR-493: The Data Channel Is Four Session-Token Routes to Presigned Multipart Uploads, Driven by the Agent's Upload Ledger

**Date:** 2026-11-19
**Status:** Accepted
**Affects:** design 5.2, 5.4, 13, 13.3, 14.3, 14.4; `crates/server` (the upload routes, a migration for `job_objects`, `db/README.md`), `crates/agent` (the uploader, the ledger, `connect`, a debug-only enqueue command), `crates/shared-proto` if the route bodies are shared, `api/openapi.yaml` (the four operations, added when they are served), `docs/glossary.md`; uses ADR-492's `MultipartStore` and ADR-466's session token; fixes the acknowledgement ADR-455 left

## Context

Design 5.2 names a data channel (HTTPS, resumable chunked transfer) apart from the control
channel, and 5.4 asks for a timeout judged by no progress and for metadata before bulk data.
ADR-455 has the agent upload a job's communication log as a must-deliver transfer and delete
nothing locally before the server acknowledges it. ADR-466 and ADR-459 item 3 reserved the
session token for "long polling and the data channel". ADR-492 gave the server a part-based
object store and left its caller to the data channel. Nothing yet fixed the routes, the rows
that name an uploaded object, who checks the agent and when, what the agent keeps so that a
restart resumes, or when an object counts as delivered. Facts of the code that constrain it:

- A session token lives 300 seconds, multi-use, in memory (ADR-466 item 2, `session.rs`).
- The revocation pass (every 30 seconds, ADR-466 item 5) reads only the agents that are
  connected to the control channel. An agent that is revoked while it uploads, with no
  connection, is not found by it, and its token stays valid until it expires.
- The agent's HTTP client for the server (`server_http.rs`) is POST-only, one call per
  connection, bounded at 30 seconds and a 4 KiB answer; it connects to a loopback literal.
- The agent's durable files (journal, outbox, received-jobs ledger) are frames of length,
  CRC-32 and payload, one file per agent ID, with a torn last frame cut off at the next open.
- The part URLs of ADR-492 are bearers for 300 seconds and do not bind a length.

The maintainer approved the design below on 2026-11-19, all nineteen of its open choices as
proposed. It is a specification: none of it is implemented when this record is written.

## Decision

### Routes

1. **Four HTTPS routes under `/api/v1/agent/uploads`**, on the same listener and behind the same
   request guard as the agent's other routes (design 13.3). Every request carries the session
   token as a bearer header, obtained as the control channel's is (design 6.4, ADR-466 items 1
   and 2). A body is at most 4 KiB, read within 5 seconds, a strict JSON object, a larger body
   is a 413, and a wrong shape is a 400 before any lookup (as ADR-466 item 1). Errors are `application/problem+json`.
   The routes are: `POST /uploads` (begin), `POST /uploads/{uploadId}/parts/{part}` (one part
   URL), `GET /uploads/{uploadId}` (status) and `POST /uploads/{uploadId}/complete`.
2. **Every request judges the token and then reads the agent's row.** The token is looked up
   (missing or expired: 401 with `WWW-Authenticate: Bearer`), then one read of `agents` gives
   `tenant_id` and `revoked_at`; a revoked or missing row is a 401, the same answer as an
   expired token, and the server drops that agent's tokens (`Sessions::revoke_agent`). The
   data routes therefore do not rely on the revocation pass, which covers connected agents only
   (design 14.4). A failed read is a 503. A 401 never says which of the reasons it was. An
   upload that belongs to another agent or tenant is a 404.
3. **Begin** takes `{jobId, generation, kind, sizeBytes, sha256}`: the job (a UUID), the
   ownership generation, `result` or `commLog`, the length the agent declares (0 to 4 GiB,
   4294967296 bytes) and the SHA-256 of the object as 64 lowercase hex digits. These fields are
   the object's metadata: there is no free-form metadata object. A job that does not exist, or
   is another agent's, is a 404; a length over 4 GiB is a 413. Begin is idempotent per job,
   generation and kind: when a row that is not `failed` exists for them and declares the same
   length and digest, begin answers that row (200); one that declares a different length or
   digest is a 409. Otherwise the server calls `MultipartStore::begin` and then inserts the row
   (below) and answers 201. The part size is a server setting, 5 MiB (the store's minimum
   part), returned by begin and kept in the row, so a later change of the setting does not
   reinterpret an open upload. The part count is the length divided by the part size, rounded
   up, and at least 1 (an empty object is one empty part, ADR-492 Consequences). The answer is
   `{uploadId, partBytes, partCount, state}`, where `uploadId` is the row's ID: the store's
   upload ID and key never leave the server. Begin is allowed before the job's terminal event
   has reached the server, since the agent's events and its uploads run on separate paths.
4. **Part URL**: `POST /uploads/{uploadId}/parts/{part}` with an empty body, for a part from 1
   to the row's part count (else 400), on a row that is `open` (else 409 with the row's state).
   It answers `{url, expiresInSecs}` with a presigned PUT URL from `presign_part` (valid 300
   seconds, ADR-492 item 7). One URL per request, so a URL is requested when it is about to be
   used; asking again for a part already held is allowed and gives another URL.
5. **Status**: `GET /uploads/{uploadId}` answers `{state, partBytes, partCount, partsHeld}`.
   `partsHeld` is present while the row is `open` and is the count of leading parts, from
   part 1, that the store lists with the expected size (`partBytes` for each, the remainder
   for the last), counted up to the first part that is missing or has another size. The agent
   resumes from part `partsHeld + 1`. A part beyond a gap is not counted and is sent again.
6. **Complete**: `POST /uploads/{uploadId}/complete` with an empty body. The server moves the
   row from `open` to `completing` by a compare-and-set and commits that before it calls the
   store; the store's `complete` (ADR-492 item 5: it checks the parts, completes the upload,
   reads the object back and compares length and SHA-256 with the row's) runs in a task of its
   own that outlives the request. The row becomes `finished` only after the verification
   succeeded. The request waits for the task up to 20 seconds and answers 200 `{state:
   "finished"}` when it is done, else 202 `{state: "completing"}`, and the agent repeats the
   request. A request that finds the row `completing` waits on the task in this process, or
   starts it again when there is none (a crash left it; the store's completion is safe to
   repeat). A request on a `finished` row answers 200. Failures of the store: a part problem
   (`NoParts`, `MissingPart`, `PartTooSmall`, `PartsLengthMismatch`, `PartsChanged`) returns
   the row to `open` and answers 409 (the upload and its parts are untouched, so the agent
   reads the status and sends again); `HashMismatch`, `LengthMismatch` and `ObjectTooLarge`
   (the object is already deleted) make the row `failed` and answer 409, and the agent begins
   again, which the partial unique index allows; any other error leaves the row `completing`
   and answers 503, to be repeated. The mapping of the store's errors to these answers is the
   server work's to confirm; it is not verified.

### The row

7. **`job_objects`** (a new migration, `db/README.md`): `tenant_id`, `id` (UUID, the route's
   `uploadId`), `job_id`, `generation`, `kind` (`result` or `commLog`), `agent_id`,
   `storage_key`, `upload_id` (the store's, a bearer, never answered or logged), `part_bytes`,
   `size_bytes`, `digest` (hex SHA-256), `state` (`open`, `completing`, `finished` or
   `failed`), `created_at` and `finished_at`. The key is `(tenant_id, id)`. A partial unique
   index on `(tenant_id, job_id, generation, kind)` where `state` is not `failed` holds the
   idempotency of begin: two racing begins insert one row, and the loser aborts the upload it
   started (`MultipartStore::delete`) and answers the winner's row.
8. **Write order.** Begin calls the store and then inserts; a crash between them leaves an
   unfinished upload no row names (the backlog's abandoned-upload cleanup). Completion commits
   `completing` before it calls the store, so a crash after the store's completion leaves a
   `completing` row whose object a repeated request verifies; `finished` is written last. No
   row is `finished` for an object that was not read back.
9. **Kinds.** `result` is the run's final VM state as JSON, the document `ngr-agent run` prints
   (design 8.2); `commLog` is the job's communication log file (design 8.11, ADR-455), the
   same bytes as on the device. The time series of a monitoring job is M4 work and is not a
   kind here. The retention, access and VIN deletion of the objects follow the job result's
   (design 4.3, ADR-455 item 3); the rows carry no retention of their own yet.

### The agent

10. **The uploader is a second task of `connect`**, beside the control session, with its own
    token exchange (challenge, then session, design 6.4), so the control channel's token and
    connection are not touched by it and neither waits on the other. It exchanges only while
    its ledger holds work, and one exchange serves 300 seconds. The agent hands it work by an
    in-process enqueue; there is no control message for it. A debug-only command, absent from a
    release build, enqueues an object so that tests can drive the uploader (design 13.4).
11. **The upload ledger** is one owner-only file per agent ID under `<state>/uploads/`, in the
    journal's frame format with a magic, a version and a header frame like the outbox's. It
    holds, for each object, what to upload (job, generation, kind, where the bytes are, length,
    SHA-256) and that it was delivered. It holds no progress: after a restart the agent begins
    again (idempotent) and asks for the status, and the server's answer is the truth about
    which parts exist. An entry leaves the ledger only when the server has answered that the
    object is `finished`; that answer is the acknowledgement ADR-455 item 2 asks for, and
    nothing local is deleted before it. The agent hashes the object before it enqueues, since
    begin declares the length and digest.
12. **The order and the pace.** One object at a time and one part at a time, each part one
    request to the URL: `result` objects first, then `commLog`, the oldest first within each.
    A transfer that makes no progress for 30 seconds (no byte moved) is abandoned and tried
    again after a back-off that starts at 1 second and doubles to 60 seconds as the control
    channel's does (ADR-471), and goes back to 1 second when a part or a completion advances.
    A 401 is answered by a new exchange; a 409 on completion by a new status.
13. **The part URL's host rule.** The agent PUTs only to a URL whose scheme and host equal those
    of the origin it registered with (any port, since the local profile's object server has its
    own), follows no redirect and resolves no name (the host is a literal). A URL that breaks
    the rule is a failure of that attempt, not sent to. The client is new: it is not
    `server_http`'s POST client, whose answer bound (4 KiB) and POST-only shape do not fit a
    part's body or the GET of the status.
14. **Job state is independent of uploads.** A job ends, and is reported ended, whether or not
    its objects were delivered; a failed or slow upload never changes a job's state and is not
    an event of the job.

## Consequences

- A revoked agent that is offline cannot get a new URL or complete an upload (item 2); the
  URLs it already holds lapse within 300 seconds of their issue (ADR-492's accepted limit),
  since revocation does not reach the object server.
- An uploading agent costs one more proof exchange (a challenge and a session) per 300
  seconds. The challenge cap of ADR-467 (60 a minute) is shared with the control channel; many
  uploading agents on one server share it.
- The agent that registers again gets a new agent ID and a new ledger: what its old ledger held
  is not uploaded. The cost of an agent key lost is its undelivered objects.
- Each route reads the database once or twice and a part URL costs no store call, so a large
  upload costs about one database read per part; the status and completion calls list parts.
- Integrity is end to end only: nothing checks a part when it is stored, and a part URL does not
  bind a length (ADR-492), so a holder can push more bytes than declared until completion.
- The route's token judgment differs from the control channel's, which judges it at the upgrade
  only (ADR-466): here it is judged on every request, because a request can come after the
  connection that would have been closed.
- The server is not authenticated to the agent on the local profile (ADR-466 Consequences). The
  part-URL answer decides where the agent sends the job's bytes, so that gap must be closed
  before real data is enqueued.
- Not verified: that a part URL for the bundled object server passes the host rule on every
  platform; the URL's 300 seconds on a slow link where a part of 5 MiB takes longer to send;
  the length of one begin-to-finished cycle under the 20-second completion bound for a large
  object (the read-back runs at the object server's speed).

Open items, kept in the backlog: the download leg (documents and artifacts to the agent through
presigned GET, which the store lacks), a bandwidth limit for uploads (design 5.4, 15.3),
abandoned-upload cleanup (a period of 7 days is proposed), a bound on the length of what is PUT
(ADR-492), the server's authentication on the local profile, a probe of URL expiry on slow
links, the part-URL host rule for an external object store (the standard deployment names
another host), the stability of the agent's source bytes between hashing and the last part
(the log is appended by later runs), whether begin checks the generation against the signed
instruction, and how an operator's explicit upload of a log (ADR-455 item 1) reaches the agent.
