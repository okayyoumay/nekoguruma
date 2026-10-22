# ADR-345: The Agent Builds Its Source Table From the Job's Document

**Date:** 2026-10-22
**Status:** Accepted
**Affects:** `agent` (`inputs.rs`: `ServiceSources::from_document`, `TableError::Document`;
`runner.rs`: `check_job`, `JobError::Sources`, the journal setup's table; `journaling.rs`:
`JournalSetup::sources`), design 8.9; ADR-245, ADR-336, ADR-338

## Context

A program names each identity value and each precondition by a `Source` (ADR-245): an input the
agent supplies, or `Source::EcuService { service_id, field_id }`, a field of a declared service's
response. The agent resolves an `EcuService` source through a `ServiceSources` table, a list of
single-DID ReadDataByIdentifier requests with the response field's offset, length and encoding.
ADR-245 left the table to the caller ("stands in until the declaration part has a decoder"): every
caller that journals a job, a restart included, builds one by hand, so it repeats what the job's
document already declares, and a table that differs from the document reads something else than
the procedure names.

The document holds every service and field a source can name, and the converter emits only
sources the table can read (ADR-336 item 1; `csv-tables.md` 4.13 and 4.14): a generated single-DID
read whose only response field is the value, fixed-length ASCII text or a big-endian unsigned
integer of whole bytes, the VIN a 17-byte text. Documents can come from other front ends and
from hand-built tests, which the converter's checks do not cover.

Open: whether the document's table replaces the caller's or merges with it, when a source the
table cannot read is found, and what is read for a field that carries a conversion.

## Decision

1. **A job with a document builds its table from the document.** `ServiceSources::from_document`
   takes each distinct `EcuService` source the program names (the identity sources and the
   precondition sources, default and programming session) and finds the declared service and
   field. The request is the service's request as the runtime encodes it. The field's offset and
   length are its byte position after the response's service identifier. Its encoding is ASCII
   for a text field and unsigned big-endian for an unsigned integer. The value read is the field's
   bytes as the ECU sends them, without the field's conversion, since a precondition range
   compares the value as read (`csv-tables.md` 4.14) and an identity check compares bytes or text
   (ADR-229).
2. **The document's table replaces the caller's.** For a job with a document, on a first run and
   on every resume, the runner uses the table built from it and ignores the one in its journal
   setup, so the first run and a restart read the same fields, the ones the procedure names. A job
   without a document (a hand-built program) keeps the caller's table. The two are never merged: a
   merged table would let a caller's entry answer for a source the document does not declare.
3. **The job check refuses a source the table cannot read, before anything is sent.** For every
   job with a document, journaled or not, `check_job` builds the table. It ends the job in
   `JobError::Sources` when a source names a service or field the document does not declare, or a
   shape the table cannot read:
   - a request other than a single-DID ReadDataByIdentifier, or a response that does not echo it;
   - a field that is not byte-aligned, not fixed-length, not the end of the response, or of
     another type than ASCII text or an unsigned big-endian integer of at most 7 bytes (the
     agent holds it as a signed 64-bit integer, as `csv-tables.md` 4.13 says);
   - a field whose kind does not fit what reads it: the VIN a 17-byte ASCII text, a precondition
     an unsigned integer.

   A document that the agent cannot read in full is refused when the job is checked, not halfway
   through a restart after a write.

## Alternatives rejected

- **Merge the document's table with the caller's.** A caller's entry could then stand for a
  source the document does not declare, which is what binding the table to the document is for.
- **Build the table only for journaled jobs, or check a source only when it is read.** A source
  is read on a journaled path, often a restart after an erase. A document the agent cannot read
  would then be found where the job can no longer go back, and the refusal would depend on the
  path a run takes.
- **Refuse a field that carries a conversion.** The converter accepts such a field (`csv-tables.md`
  4.14 compares the value as read), and the table reads the raw value the range and the identity
  checks compare, so a refusal would reject definitions the converter produces.
- **Read variable-length or terminated ASCII.** The agent's VIN and version checks compare whole
  fields; the example ECU's terminated DIDs are handled by giving it fixed-length ones, a
  backlog item of its own.

## Consequences

- A caller that journals a job with a document no longer builds a table. Tests that pair a
  document with a hand-built program declare the services the program names.
- A job whose document names an unreadable source fails at job check even if it would never read
  the source on its path, such as a read-only job that never restarts. The converter never emits
  one, so only another front end or a hand-built document meets this.
- ADR-245's stand-in table is the source of truth only for a job without a document.
