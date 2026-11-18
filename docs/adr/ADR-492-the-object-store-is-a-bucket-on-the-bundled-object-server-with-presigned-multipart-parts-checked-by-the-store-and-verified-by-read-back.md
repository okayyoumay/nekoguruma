# ADR-492: The Object Store Is a Bucket on the Bundled Object Server, With Presigned Multipart Parts, Checked by the Store and Verified by Read-Back

**Date:** 2026-11-18
**Status:** Accepted
**Affects:** design 13, 13.3, 14.3; `crates/server` (`storage`, `storage/s3`, `config`, `lib`, `bin/ngr-fake-object-server`, `tests/postgres/`), `crates/server/docs/ngr-server.md`, `db/README.md`, `docs/glossary.md`, `README.md`; supersedes in part ADR-474; details ADR-491

## Context

ADR-474 gave the server an object store of local files with a resumable append by offset, and
ADR-491 added the bundled RustFS, an S3-compatible server, as the server's child, with the S3
store left as later work. Design 14.3 wants uploads of large data to go straight to storage by
presigned URL and to resume as S3 multipart uploads. The append-by-offset contract cannot be
written over S3 (a part is not an append), and nothing yet used the local store. The maintainer
decided on 2026-11-18 to make the S3 store the one the server uses, on the bundled RustFS, with
the contract below. RustFS 1.0.6 was probed for what the design relies on (the facts are in
Consequences).

## Decision

1. **A part-based trait, `MultipartStore`.** `begin()` issues the key (a version 4 UUID, as
   ADR-474) and starts a multipart upload, and returns `Upload { key, id }`; the caller's row
   keeps both, and the store keeps nothing. `presign_part(upload, part)` is synchronous and does no
   I/O. `list_parts` returns number and size (no entity tag leaves the store). `complete`,
   `get`, `delete` and `list_unfinished` are as the next items say. `ObjectStore` and `LocalStore`
   (ADR-474) stay compiled and tested, and the server no longer uses them; ADR-474's items 1, 4
   and 5 now describe the test store only. The object server's data directory still uses the
   local store's directory-locking code.
2. **The server opens the store after the object server is ready** (ADR-491's step 5, then this
   one). It checks the bucket with HeadBucket and creates it when absent (a 409 that says the
   bucket is ours counts as present), retrying a failed connection or a 5xx every 100 ms within
   the 30 seconds of the ready budget, and giving up at once if the child ends meanwhile; a
   refusal (401, 403) or an unexpected 4xx ends the open. A failure stops the child, closes the
   pool and exits 1, as ADR-491's other failures. ADR-491's signed readiness check is this
   HeadBucket. The subcommands never open the store.
3. **Bucket and configuration.** `object_server.bucket`, default `nekoguruma`, is 3 to 63 of
   `[a-z0-9-]`, not starting or ending with a hyphen, and not `health`, `minio` or `rustfs`. The
   `[storage]` table is removed: an existing file that has one is refused as an unknown key, and
   the migration is to delete it. ADR-491 item 1's rule that `data_dir` may not be `storage.dir`
   goes with the table; the rule on `credentials_dir` stays. The size limit of an object, 4 GiB,
   is a constant of the code.
4. **Bundled RustFS only.** The store reaches `127.0.0.1:<port>` over HTTP, path style, region
   `us-east-1`, with the credentials of ADR-491. An external S3 service over HTTPS is not
   supported yet.
5. **`complete` is checked by the store.** Under the key's lock the store lists the parts (paged
   by 1000) and checks them itself: at least one, numbered consecutively from 1, each but the last
   at least 5 MiB, the sizes adding up to the length the caller names, at most 4 GiB and at most
   10000 parts. Only then does it send CompleteMultipartUpload, with the entity tags of the
   listing in part order, and then it reads the object back with GetObject, streaming its SHA-256
   and counting its bytes, and compares both with what the caller declared (the end-to-end check
   of design 14.3). A mismatch deletes the object, and `HashMismatch` or `LengthMismatch` is
   answered only once nothing is left; a failed delete answers its own error, since the caller
   must not conclude the object is gone. A failed read-back (a cut, 30 seconds of silence)
   deletes nothing. A retry (the answer lost, the read-back cut) finds the upload gone and the
   object present and reads the object back again, so completing is safe to repeat. The store
   does not trust the object server's own part checks (its minimum size, the order, the ETags),
   because the contract must not depend on one implementation, and the caller's length and digest
   are checked by the store whatever the server did.
6. **`delete` and the key lock.** Completing and deleting a key take a per-key lock in the
   process (the pattern of ADR-474's `lock_key`); other calls take none. Delete lists the uploads
   whose key starts with the key, aborts those whose key is exactly it, repeats while it found any
   (at most 3 rounds, since a writer may start another upload meanwhile), then deletes the object.
   Deleting what is not there is `Ok`.
7. **Part URLs are valid 300 seconds**, a constant. The store's own requests are presigned for
   60 seconds. A control request is bounded by 30 seconds; a body being read by 30 seconds without
   a byte and no total bound; an XML answer by 4 MiB and an error body by 64 KiB.
8. **The HTTP rules.** rusty-s3 (0.11, RustCrypto signing and XML only) builds and signs every
   request and parses the answers; the requests are sent by hyper's HTTP/1 client, one request on
   one connection (`connection: close`), with no pool, no retry, no redirect and no HTTP/2. A
   request with a body always sends `content-length`, `0` included. ListMultipartUploads, which
   rusty-s3 has no action for, is signed by hand with its `signing::sign` and parsed with
   instant-xml; its query is sorted by name, which the signature requires.
9. **Nothing sensitive is formatted.** A presigned URL carries its signature in its query (its
   `Debug` would show it), the upload ID is a bearer for the upload, and the object server's
   messages repeat keys: no URL, key text, upload ID or server message goes into an error or a
   log line. An error is a fixed message, an operation name, an HTTP status or an `ErrorKind`.
   `PartUrl` and `UploadId` have a redacting `Debug` and no `Display`.
10. **Errors.** `InvalidPartNumber`, `InvalidUploadId`, `NoParts`, `MissingPart { part }`,
    `PartTooSmall { part }`, `PartsChanged` (a listing that is not ascending or does not advance,
    or the object server's complaint about a part), `ObjectTooLarge`, `LengthMismatch { stored }`,
    `HashMismatch`, `AlreadyFinished` and `NotFound` (a gone upload, the first when the object
    exists), `Refused { op }`, `Server { op, status }` and `Io { op, kind }`.
11. **Tests.** The store's units run against a scripted loopback listener; the real RustFS tests
    use a plain HTTP client to PUT parts to the presigned URLs. The fake object server has a
    steering key, `s3_status`, that makes it answer the store's requests with a chosen status.

## Consequences

Probed on RustFS 1.0.6 (2026-11-18), and what the code relies on: a presigned UploadPart PUT is
answered 200 with a quoted ETag; ListParts pages with a truncation flag and a continuation marker;
ListMultipartUploads with no prefix lists the uploads of every key, with a prefix it filters;
CompleteMultipartUpload accepts XML-escaped quoted ETags, refuses two small parts (400), accepts
one empty part (an object of length 0) and, called again on a completed upload, answers 200 with
the same ETag; an upload that is gone (aborted, completed, or never there) is a 404 for ListParts
and Abort; GET and HEAD of a missing key are 404 and DELETE of a missing key is 204; an expired
URL, an edited part number or another Host is a 403; an anonymous GET is a 403; HeadBucket of a
missing bucket is 404 and CreateBucket with an empty body and `content-length: 0` is 200; and
RustFS's log at its default level holds no signature.

Known limits, accepted:

- **A part URL does not bind a length.** Whoever holds a URL can PUT any number of bytes to its
  part within the 300 seconds; the completion's checks (the sizes of the listed parts, the
  read-back's length and digest) are what catch it, and the 4 GiB limit is not enforced before
  the bytes are stored. A URL that binds `Content-Length` is a backlog item.
- **A revoked agent's URLs stay usable** for up to 5 minutes from their issue; revocation does
  not reach the object server.
- **An unverified completed object can outlive a crash.** If the server dies between the
  completion and the read-back, the object exists with no verification; a retry of `complete`
  verifies it, but nothing retries by itself.
- **`get` verifies nothing.** It streams what the object server holds, bounded by the idle
  timeout; only `complete` compares a digest.
- **The key lock is in this process only.** Two servers on one object server (not a deployment
  this build allows, since the data directory is locked) would not exclude each other.
- **Deleting nothing is `Ok`**, so a caller cannot tell a delete that removed an object from one
  of a key that never existed.
- **Reads are not capped:** each `get` or read-back holds a connection to the object server for
  as long as it makes progress.
- No connection is reused, so each request pays a loopback connect; the loopback cost is small
  and the rule keeps the failure handling simple.

Follow-up work, kept in the backlog: an external S3 service over HTTPS (endpoint, TLS,
credentials, and a crypto provider that builds for the Windows worker targets and passes
ADR-283's DLL check), part URLs that bind `Content-Length`, a cap on concurrent read-backs, the
key lock across several server instances, abandoned-upload cleanup and the data channel's use of
the store.
