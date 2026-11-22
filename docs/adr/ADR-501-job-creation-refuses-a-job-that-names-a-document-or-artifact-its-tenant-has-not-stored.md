# ADR-501: Job Creation Refuses a Job That Names a Document or Artifact Its Tenant Has Not Stored

**Date:** 2026-11-22
**Status:** Accepted
**Affects:** design 5.1; `crates/server` (`jobs`), `crates/server/docs/ngr-server.md`, `api/openapi.yaml`; replaces ADR-477 item 2's digests taken as named and ADR-500's consequence that job creation takes them

## Context

ADR-477 item 2 takes a job's IR digests and its artifact digest as the request names them. Since
ADR-500 the server has a route that stores a document, and the download route (ADR-497) finds a
job's objects through the `ir_documents` and `artifacts` rows of the job's tenant. A job that
names a digest with no row is still created and delivered; its agent's fetch then gets the
download route's 404, so the job never starts, while it holds the vehicle's lock (ADR-480) until
the lock expires. The maintainer decided the check on 2026-11-21.

## Decision

1. **`POST /api/v1/jobs` checks, at creation only, that what the job names is stored for the
   caller's tenant.** Every named IR digest needs a row in `ir_documents`, and the artifact
   digest, when there is one, a row in `artifacts`, both under the caller's tenant. A missing
   document is 422 with the code `DOCUMENT_NOT_STORED`, a missing artifact 422 with the code
   `ARTIFACT_NOT_STORED`; the problem names no digest. When both are missing the documents answer:
   the request names them first and every job names at least one.
2. **The check sits after the permission and the deadline's bounds and before the active
   instruction key** (ADR-477 item 7's order, ADR-487). A caller without the permission learns
   nothing of the tenant's documents; an operator who named something unstored hears so even while
   the server's key is not active, since storing it is what the operator can do. A refusal
   writes nothing and leaves the idempotency key unbound, as the deadline's 422 does: the same key
   creates the job once the object is stored.
3. **A replay answers before the check** (ADR-477 item 5): a replayed job was created once, and
   what it named is not judged again.
4. **The check is a read in the creating transaction and takes no row lock.** Nothing deletes a
   row of `ir_documents` or `artifacts`, so a row seen is a row the job's fetch finds. A digest is
   also not judged against the job's kind, and an artifact row counts whether or not it is
   published.
5. **Jobs created before the check are left as they are**; a fetch of what such a job names finds
   the download route's 404, as before.

## Consequences

- A job can only be created for documents someone stored first, so a client stores each
  document (ADR-500) before it creates the job that names it. No route stores an artifact yet, so
  a job that names an artifact can be created only for one whose row was written otherwise.
- The check holds only while nothing deletes those rows. Whatever later deletes one (an eviction,
  an erasure for a tenant) must lock it against a concurrent creation or refuse to delete a row
  that an unfinished job names.
