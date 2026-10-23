# ADR-347: The Communication Log's File and Writer

**Date:** 2026-10-22
**Status:** Accepted
**Affects:** design 8.11 ("Scope and form"); `agent` (`comm_log`, the job runner's entry points
and their `JobRun` result, `Job::with_comm_log`, `JobError::CommLogMismatch`, the journal's
writer lock and file stem, `ngr-agent run --comm-log`); ADR-346 item 10

## Context

ADR-346 decides what the agent's communication log says and that it never steers the job. It
leaves open how the log is stored and written:

- where the file lives;
- how a later run of the same job finds and continues it;
- what a clean close is, so that a reopen can tell an interrupted log;
- how the "communication log incomplete" mark reaches the job's result, when the file it would
  go in is what failed;
- what the one function every logged byte passes through does with secrets and bulk data.

The runner's entry points `run_program`, `run_program_journaled` and
`resume_program_journaled` return the job's result with its guards, and nothing else.
`run_program` has no job key and no directory, and its callers are developer tools and tests. A
journaled job has both, but a first run creates its journal only once the link is open, and a
resume opens its journal before the link.

## Decision

1. **One file per job and ownership generation, beside the journal.** The log is
   `{dir}/{id}.g{generation}.commlog.jsonl`, the job id checked by the same rule as the journal's
   file name. It has a writer lock of its own, the sidecar `{id}.g{generation}.commlog.lock`, taken
   the way the journal takes its own (ADR-255): the journal's lock exists on a first run only
   once the link is open, which is too late for the log. Every run of the job and generation (a
   first run, a plain start on an existing journal, a resume) opens the file if it exists and
   appends to it; a new generation starts a new file.
2. **The setup comes from the journal, or is asked for.** The journaled entry points log under
   their journal's directory and key, with no parameter of their own. `run_program` logs only
   when the job carries a setup (`Job::with_comm_log`); `ngr-agent run --comm-log <dir>` gives
   it one with a job id of its own. A journaled job whose setup names another directory or key
   is refused before anything opens (`JobError::CommLogMismatch`), so a job never has two logs.
   The job's thread opens the log after a resume's journal and its early refusals, and before
   the link, so a link that fails to open is logged. A job refused before its thread starts has
   no log, as it has no journal.
3. **The result carries the log's status.** The three entry points return a `JobRun`: the job's
   result, its guards and a `CommLogStatus` (no log, complete, or incomplete with the reason: the
   log could not be opened, another run holding it included, a write or a sync failed, or the log
   was reopened without a clean close). The result itself is the same whatever the log's status. The status cannot be
   only in the file, since the file is what fails, and the guards are about locks.
4. **A log that ends in `job_end` was closed cleanly.** The first line is the header (`seq` 0).
   Each run writes `job_start`, and its last line is `job_end` with the job's outcome. A reopen
   drops a torn last line (everything after the last line break), goes on with the `seq` after
   the last complete line, and writes `agent_clock`, a new reading of the agent's clocks that
   starts a new agent epoch (design 8.11). `job_start` names the run as the runner knows it
   when it opens the log: a first run, a resume, or a resume that found no journal; a plain start
   on an existing journal, which the restart classification decides later, is logged as a resume. When the file ended in a torn line or its last complete
   line is not `job_end`, the reopen marks `agent_clock` incomplete (on a file that held only a
   torn line, the new header's clock) and the run's status is incomplete, even if the rest of
   the run writes cleanly. No sidecar marks the close, since one could disagree with the file.
5. **One write per line, synced at the edges, poisoned after a failure.** Each line is
   serialized and written with one write call from the job's thread, without a buffer the
   agent holds and without a sync per line: the write costs microseconds against the
   milliseconds of an exchange, and a writer task would only add a failure path to report back.
   The opening lines and `job_end` are synced. After a failed write the log takes no more
   lines (as the journal is poisoned after a failed commit) and writes no `job_end`, so the file
   reads as interrupted. Lines since the last write-back can be lost on a power loss, which
   design 8.11 accepts.
6. **One redaction function, the only way bytes reach the log.** Every message the log will
   carry is built by one function from its direction and bytes, and an entry holds what it
   returns, never raw bytes. It keeps the service, the sub-function and the block counter in
   clear and replaces the rest:
   - a SecurityAccess key request (an even sub-function, ISO 14229-1:2026 9.4) and its seed
     response (an odd one) by their length, the seed with whether it is all zero;
   - an Authentication request or response by its length;
   - a TransferData request, and a TransferData response that carries data (an upload,
     ISO 14229-1:2026 14.4), by length and SHA-256.

   Everything else, negative responses included, is logged in clear.

## Consequences

- Every caller of the three entry points takes a `JobRun` instead of a pair.
- A developer run through `run_program` without a setup writes no log; every job the agent runs
  for the server goes through a journaled entry point and so has one.
- The exchange entries, `link_opened`, `vci_connected` and the settling entries write through
  the same log, which the runner gives the worker host.
- The log has the journal's protection: today the same directory, and the journal's encryption
  once that is built (design 5.5).
