# ADR-493: The Data Channel Is Four Session-Token Routes to Presigned Multipart Uploads, Driven by the Agent's Upload Ledger

**Date:** 2026-11-19
**Status:** Accepted (items 15 to 18 added with the begin, part URL and status routes; item 6's mapping corrected and items 19 to 23 added with the complete route; item 17 extended by item 23; item 11's record of delivery corrected and items 24 to 28 added with the upload ledger; items 6 and 17 amended and item 11 extended by ADR-495; a download route added by ADR-497; item 10's ending of either task detailed by ADR-499; their routes' answers are proved, ADR-504; item 13's scheme is `https` and the part-URL answer carries the certificate pin, ADR-505; item 22 revised by item 29 with the completion pass)
**Affects:** design 5.2, 5.4, 13, 13.3, 14.3, 14.4; `crates/server` (the upload routes, a migration for `job_objects`, `db/README.md`), `crates/agent` (the uploader, the ledger, `connect`, a debug-only enqueue command), `crates/shared-proto` if the route bodies are shared, `api/openapi.yaml` (the four operations, marked planned until served), `docs/glossary.md`; uses ADR-492's `MultipartStore` and ADR-466's session token; fixes the acknowledgement ADR-455 left

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
   is another agent's, is a 404; a length over 4 GiB is a 413. The shape is judged first (400), then the length (413), both before the job is looked up. Begin is idempotent per job,
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
   (`NoParts`, `MissingPart`, `PartTooSmall`, `PartsLengthMismatch`, `PartsChanged`,
   `ObjectTooLarge`) returns the row to `open` and answers 409 (the upload and its parts are
   untouched, so the agent reads the status and sends again); `HashMismatch` and
   `LengthMismatch` (the object is already deleted) make the row `failed` and answer 409, and the agent begins
   again, which the partial unique index allows; `NotFound` (the upload and the object are both
   gone, as when the server stopped after a mismatch's delete and before the row was written)
   also makes the row `failed` and answers 409, so a row can never stay `completing` with
   nothing left to complete; any other error leaves the row `completing` and answers 503, to
   be repeated. The mapping of the store's errors to these answers is confirmed against
   ADR-492 item 10 by item 19, which also puts `ObjectTooLarge` among the part problems.

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
    SHA-256) until it is delivered (items 24 to 28: delivery is recorded by the entry's
    removal). It holds no progress: after a restart the agent begins
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

### Decisions taken with the begin, part URL and status routes

15. **Begin compares the declared generation with the job's stored instruction.** The server
    decodes the `signed_job` it stored for the job (the envelope's payload; it wrote the
    envelope itself, so the signature is not checked again) and compares its ownership
    generation with the one begin declares. A difference is a 404, the answer for a job that
    does not exist or is another agent's, so a caller learns nothing of a job it does not own;
    an instruction that does not decode is a 500. The job's state plays no part (item 3: begin
    is allowed before the terminal event arrives, and after the job has ended). A row that is
    not `failed` for the same job, generation and kind but belongs to another agent is a 409,
    as a different declaration is.
16. **The shape and the length are judged before the token, and the token before every
    lookup.** The order is the guard, the method (405), the body (413, 408, 400), the shape
    (400: the JSON object of begin, the part number, an empty body where there is none), begin's
    length (413), the token (401), the agent's row (401 with the agent's tokens dropped, 503,
    500), the job or the upload (404), the request against the row (400 for a part past the
    count, then 409) and the object store (503, 500). A request that is malformed is therefore
    answered without a lookup and drops no token, and a body is never read on behalf of an
    unauthenticated caller beyond the 4 KiB bound. The begin body is read by a visitor of the
    server's own: a property twice, an unknown or a null one is a 400, and a number is an
    integer written as one. The JSON reader turns an integer of 2^64 or more into a float, so
    for `sizeBytes` a value written with a fraction or an exponent is judged as that float: an
    integral value of 2^64 or more (`18446744073709551616`, `1e30`) is a length over the limit and
    a 413, and any other (`1.5`, `5e9`, a negative number) is a 400, as is a number beyond the
    range of a double (`1e400`, an integer of over 308 digits), which the reader refuses whole.
    `generation` is an integer from 0 to 9223372036854775807 (the column's range), else a 400. The part number is canonical decimal text from 1 to 10000, taken as text
    from the path, and an upload ID that is not a canonical UUID is a 404 after the token.
17. **A status that finds the store without the upload answers 503 and writes nothing.** An
    `open` row whose upload `list_parts` reports as not found or already finished is a state the
    completion decides (item 6), not the status: the status answers 503, leaves the row as it
    is, and the agent repeats it (item 23 refines this: the status reads the row once more before
    it answers 503). A part URL for a row that is not `open` is never issued and
    asks the store nothing; the status of such a row has no `partsHeld` and asks the store
    nothing either.
18. **Settings and details.** The part size is a constant of the code, 5 MiB, with an override
    that only tests use; a build-time check keeps the largest object within 10000 parts and
    its last part between one byte and a whole part. The part count is `max(1, ceil(length /
    part size))` and the last part holds `length - (count - 1) * part size` bytes, so an empty
    object is one empty part (which the pinned object server lists with size 0, and the status
    counts). The 409 of the part URL carries the row's state as a `state` member of the problem
    document. `job_objects` has foreign keys to the tenant, the agent and the job (with no
    cascade: nothing that has objects is deleted under them), a unique `storage_key`, a CHECK for
    each column's range and for `finished_at` being set exactly when the state is `finished`;
    this route writes the state `open` only. The loser of two racing begins deletes the upload
    it began (a failed delete is a warning and does not change the answer) and answers the
    winner's row, or a 409 for another declaration. No database connection is held while the
    store is called, and nothing from a request that could identify an upload, a URL, a
    key or a digest, and no database error text, is logged.

### Decisions taken with the complete route

19. **The store's answer decides the row, and every outcome keeps two invariants: a row is
    `finished` only for an object that was read back, and a row never stays `completing` with
    nothing left to complete.** Confirmed against ADR-492 item 10: success makes the row
    `finished` (with `finished_at`) and answers 200; `NoParts`, `MissingPart`, `PartTooSmall`,
    `PartsLengthMismatch`, `PartsChanged` and `ObjectTooLarge` are part problems (the store
    checks the parts, and for `ObjectTooLarge` the sizes, before it completes anything, so the
    upload is untouched) and return the row to `open`, 409; `HashMismatch`, `LengthMismatch` and
    `NotFound` make it `failed`, 409 (the store has deleted what was left, so the route deletes
    nothing); a failed delete, `Refused`, `Server`, `Io` and any other error write nothing and
    answer 503 (`Random` 500, as in the other routes), leaving the row `completing`; a database
    failure answers 503 or 500 and writes nothing more. Both 409 answers carry the row's
    state as a `state` member of the problem document, as the part URL's does, so the agent
    tells "send the parts again" from "begin again"; a `failed` row asked to complete answers the
    same 409. A `finished` row is 200 with no store call.
20. **One completion task per upload, owned by the hub, with the compare-and-set inside it.** The
    handler only reads. The task is registered in a registry in the server process (tenant and
    upload to the receiver of a `watch` of the outcome, get-or-insert under one lock), is
    adopted by the hub, and is wrapped so that the stop ends it at once. It makes the
    compare-and-set from `open` to `completing` in a transaction of its own; when no row
    changed it reads the state: `completing` is verified, any other state is the outcome and
    no store call is made. It then calls the store with no transaction open and makes one write
    conditional on `completing`. A request that arrives while the task runs joins it, so two
    concurrent requests run one store completion. The registry is per process, as the store's
    key lock is (ADR-492); with several server processes on one database the compare-and-set
    keeps the row right, and the store's repeat safety keeps the object right.
21. **The wait is 20 seconds, a constant, and a waiter that times out gets 202 with `state:
    completing`.** The code has a test-only override of the wait. A request that goes away does
    not stop the task. A task that ends with no outcome (it panicked, or the stop ended it)
    closes the channel, and the waiters answer 503: the row is as the task left it.
22. **A task the stop ended is resumed by the next request, not at start.** The row stays
    `completing`, which the next completion request verifies (item 6). No sweep runs at start
    or periodically, so a `completing` row that no request repeats stays so. (Revised by item
    29: a periodic pass resumes such a row.)
23. **A status that finds the upload gone reads the row once more.** `list_parts` answering
    `NotFound` or `AlreadyFinished` for an `open` row is the sign of a completion that moved the
    row on between the status's read and its listing. The status re-reads the row once: a row
    that is no longer `open` is answered with its state and no `partsHeld`; one that still is
    `open` is the 503 of item 17, with nothing written.

### Decisions taken with the upload ledger

Item 11 left open where a `result` object's bytes wait for their upload: a ledger frame holds at
most 1 MiB (the journal's frame bound), and a run's final state can be larger.

24. **A result waits as a file of its own; a communication log stays where it is.** The agent
    writes a result's bytes to `<state>/uploads/<agent-id>.objects/<n>`, owner-only and synced,
    where `n` is the entry's number, before it appends the entry, and deletes that file only
    after the entry's removal is durable. A `commLog` entry names the log file by its absolute
    path and covers its first `length` bytes, so a later run appending to the log does not change
    what was enqueued; the ledger never opens, changes or deletes that file. The digest is taken
    before the enqueue, outside the ledger (item 11).
25. **Numbers are never reused, and delivery is recorded by removal.** Each entry has a number,
    one above the highest ever given. An entry leaves the ledger by a rewrite of the whole file
    without it (a temporary file, synced, renamed over the ledger, the directory synced), so the
    ledger records what is still to be delivered and not, as item 11 had it, that an object was
    delivered. The header keeps the highest number given and how many entries the rewrite
    wrote: the entries a rewrite wrote are increasing, and the entries appended after it
    continue from the highest number one by one, which lets the open tell an append a crash
    tore (cut off) from damage (refused, never repaired). A removal of a number already
    removed answers success and writes nothing, so a repeat after an unclear failure is safe;
    a number above the highest given is refused and writes nothing.
26. **An enqueue repeated for the same object is answered with the entry it made.** The same
    job, generation and kind with the same length and digest returns the pending entry's number
    and writes nothing; with another length or digest it is refused as a conflict, as begin
    answers 200 and 409 (item 3). A key whose entry was already removed is enqueued anew; the
    server decides what it already has.
27. **Failures keep the bytes.** A failed append never deletes the result's file: the frame may
    have landed, and the ledger refuses writes until it is opened again, which reads the entry
    back or finds the file unnamed. A result's file is deleted by the removal of its entry, once
    the rewrite is durable (item 24), and otherwise only by an open, which deletes the files no
    entry names after it has made the ledger file and its directory durable, and never when it
    finds the ledger corrupt; a delete that fails at the removal is left to the next open. A failed rewrite before the rename leaves the entry pending and the ledger usable.
    Only the first open creates a missing file: a reopen (after a failed write) that finds the
    file gone refuses it as corrupt, so the results its entries name are not swept and their
    numbers are not given again.
28. **Bounds.** The ledger file is at most 1 MiB; a result is at most 64 MiB; the results waiting
    together hold at most 1 GiB; a communication log is at most 4 GiB, the server's bound on an
    object. A result or log over its bound is refused as too large, and an enqueue that would
    pass the file's or the results' total bound is refused as full; nothing is dropped to make
    room.

### Decisions taken with the completion pass

Item 22 left a `completing` row that no request repeats as it was: the agent that completed it
may have gone, or have given up, and the row then holds its job's slot for good.

29. **A periodic pass of the hub's tasks takes the `completing` rows that no request repeats.**
    One pass runs as the server starts and then every 60 seconds (a constant, with a test-only
    override); a tick that is missed gives one pass, not a burst. It runs only when the server
    has an object store. A pass reads, in one short read-only transaction, the 100 oldest
    `completing` rows of all tenants, by `created_at`, and then takes them one at a time: it
    starts the completion of item 20 for the row, or joins the one that runs, through the same
    registry the routes use (a server's own, shared by its routes and its pass, so a pass and a
    request for one upload run one store completion), and waits for its outcome. The outcomes
    are item 19's: `finished`, `failed` (which frees the job's slot for a new begin), `open`
    when the parts have a problem, and a row that stays `completing` on a store or database
    error until the next pass. There is no grace period: a `completing` row whose task is not
    in this process is taken at once, since the registry stops a second task here, and across
    processes the compare-and-set and the store's repeat safety keep the row and the object
    right (item 20); a row a request is completing in another process costs one extra, harmless
    store call. **A task the pass starts only resumes a row that is still `completing`:** it
    never makes the compare-and-set from `open`, because a request may have returned the row to
    `open` (a part problem) after the pass read it, and the row is then the agent's again. When
    the row is `open` at the task's start the task ends with no outcome, so a request that
    joined it answers 503 and repeats, which starts a normal task; when it is `finished` or
    `failed` the task's outcome is that state. **The pass completes the rows of agents revoked
    since:** the upload was declared and its completion requested while the agent was
    authorized, and the object is still verified against the declared digest; revocation stops
    what the agent does next (item 2), it does not discard data already handed over. A row
    whose upload ID, key or digest does not parse is skipped and left as it is, with a warning
    that names nothing; the pass writes nothing for it. A failed read warns with the SQLSTATE
    only and writes nothing; the next pass retries. The completion's warnings have a log gate
    for each reason, so one reason's flood does not hide another's. The stop ends the pass in
    its wait and in its work, as it ends the revocation pass (ADR-468 item 11). A task that ends
    without an outcome for another reason (a panic, or the row case above) leaves its row to the
    next pass, and the pass goes on with the next row, so one row that always panics does not
    keep the others from their pass.

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
- A `finished` write that fails after the store verified the object leaves the row `completing`,
  and the repeat reads the whole object back again. A `completing` row nobody repeats is never
  finished or failed (item 22).
- Not verified: that a part URL for the bundled object server passes the host rule on every
  platform; the URL's 300 seconds on a slow link where a part of 5 MiB takes longer to send;
  the length of one begin-to-finished cycle under the 20-second completion bound for a large
  object (the read-back runs at the object server's speed).

- The channel carries uploads only: documents and artifacts reach the agent by another leg,
  and the store has no presigned GET for it (ADR-497 adds that leg as a fifth route). Uploads have no bandwidth limit. Abandoned uploads
  are not cleaned up, and a part URL does not bind the length of what is PUT (ADR-492).
- The part-URL host rule is the registration origin's host, which fits the bundled object
  server and not an external one, and the agent's source bytes can change between hashing and
  the last part (a log is appended by later runs): both are decided with the work that builds
  them. How an operator's explicit upload of a log (ADR-455 item 1) reaches the agent is likewise
  left to that work; whether begin checks the generation against the signed instruction is item 15.
- A pending result takes its size twice on the device until it is delivered: in the job's own
  records and in the ledger's copy (item 24). The bounds of item 28 are first values, set without
  a measured result size.
- An entry the server refuses for good, or whose source is gone, stays in the ledger, and with
  one object at a time it stands in front of the entries behind it: what clears it is decided
  with the uploader. A missing source is found by the uploader, not by the open.
- A ledger file removed from outside the agent between two runs is not told from a first start:
  the next open creates an empty ledger and deletes the stored results as files no entry names,
  so their uploads are lost (item 27 guards only the reopen within a run).
- A retention of the communication log that deletes files must first ask the ledger, since an
  entry names the file and nothing local is deleted before the server's acknowledgement
  (ADR-455 item 2).
