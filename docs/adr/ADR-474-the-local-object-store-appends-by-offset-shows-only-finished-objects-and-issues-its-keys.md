# ADR-474: The Local Object Store Appends by Offset, Shows Only Finished Objects and Issues Its Keys

**Date:** 2026-11-13
**Status:** Accepted
**Affects:** design 13; `crates/server` (`storage`, `config`, `lib`, the directory checks shared with `instruction_key`), `crates/server/docs/ngr-server.md`, `db/README.md`; extends ADR-458's start order

## Context

Design 13 puts object storage behind a trait, S3-compatible on the standard profile and local
files on the local profile. Job results, communication logs, dataset chunks, artifacts, IR
documents and extension packages will be stored there, and the tables that name them already hold
a `storage_key` apart from their content digest (`db/migrations/0001_init.sql`). A transfer that
is cut must resume without resending what the store holds, so the store needs a contract for
partial objects: how a writer learns where to continue, when an object becomes readable, and who
names it. Design 14.3's direct upload to S3 through pre-signed URLs is a standard-profile path;
how it relates to this contract is left to the data channel work.

## Decision

1. **Append by offset.** A writer begins an object, then appends bytes at an offset that must
   equal the length the store holds; any other offset is refused with that length, and nothing is
   written. The stored length is the staging file's length after it is synced, read by one
   function, with no second record of it: every acknowledged byte was synced before the answer,
   and a tail left by a cut is a prefix of the chunk being sent, which a resume continues from.
2. **Only finished objects are visible.** Finishing names the whole object's length and SHA-256;
   the store checks both and moves the object from staging to its finished place (a rename, then
   both directories synced). A mismatch leaves it unfinished. A finished object cannot be
   appended to or finished again; reading returns finished objects only.
3. **The store issues the key.** Beginning returns a random version 4 UUID in canonical lowercase
   text, which the caller records as its `storage_key`; any other key form is refused before the
   file system is touched. Content addressing, tenant separation and retention stay with the
   rows that name the object.
4. **The local store.** A required setting names an absolute directory, created owner-only if
   its parent exists; a directory others can write is refused by the same check the instruction
   key's directory uses. One process holds the directory, by an exclusive lock on a file in it.
   An append past the configured size limit (4 GiB by default) is refused whole.
5. **A failed sync makes that object answer an error to everything but its deletion until the
   process restarts**, as the agent's journal does, since its durable length is unknown.

## Consequences

- A resume may start inside a chunk; the caller sends from the offset the store reports.
- A tail that a power loss kept in the size without its data is caught by the hash at finish, and
  the only way out is to delete and upload again.
- Unfinished objects that nobody finishes stay until they are deleted; they can be listed.
- The store keeps no digest after finishing; the caller records it.
- The S3 implementation and design 14.3's direct upload must be fitted to this trait or the trait
  extended; S3 multipart's minimum part size does not take appends at any offset directly.
