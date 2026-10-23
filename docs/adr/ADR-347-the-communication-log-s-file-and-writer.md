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
4. **A log that runs whole from its header to a `job_end` was closed cleanly.** The first line
   is the header (`seq` 0). Each run writes `job_start`, and its last line is `job_end` with the
   job's outcome. A log was closed cleanly when it starts with its header, its lines number 0,
   1, ... with nothing blank or damaged between them, and its last line is a complete `job_end`:
   a power loss can keep a synced `job_end` and lose a line before it, so the last line alone
   does not show a clean close. A reopen drops a torn last line (everything after the last line
   break), goes on with the `seq` after the largest the file holds, so numbering never goes
   back, and writes `agent_clock`, a new reading of the agent's clocks that starts a new agent
   epoch (design 8.11). When the close was not clean, the reopen marks `agent_clock` incomplete
   and the run's status is incomplete, even if the rest of the run writes cleanly. A file that
   does not start with a header gets one after whatever it holds, marked incomplete, and so does
   a resume that finds no log, since the job ran before. A file whose `seq` or epoch has no
   successor is not opened, and a log that reaches the last `seq` stops rather than wrap. A new
   file's directory is synced, as the journal's is, so its name survives a power loss.
   `job_start` names the run as the runner knows it when it opens the log: a first run, a resume,
   or a resume that found no journal; a plain start on an existing journal, which the restart
   classification decides later, is logged as a resume. No sidecar marks the close, since one
   could disagree with the file.
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
   returns, never raw bytes. It keeps the service and the sub-function in clear and replaces the
   rest:
   - SecurityAccess (ISO 14229-1:2026 9.4): a key request (a key level: even from 0x02 to 0x42,
     or the ISO 26021-2 key 0x60) and a seed response (odd from 0x01 to 0x41, or 0x5F) by their
     length, the seed with whether it is all zero. A request or response with any other
     sub-function (the supplier-specific range 0x61 to 0x7E and the reserved values, 9.4.2.2)
     has no pairing the standard defines, so its data is its length in both directions;
   - Authentication (9.6): the data by its length; a response keeps its return value (9.6.6),
     which says how the ECU judged the step and holds no secret;
   - TransferData (14.4): the data of a request, and of a response that carries data (an
     upload), by length and SHA-256, the block counter in clear.

   Everything else, negative responses included, is logged in clear.

## Consequences

- Every caller of the three entry points takes a `JobRun` instead of a pair.
- A developer run through `run_program` without a setup writes no log; every job the agent runs
  for the server goes through a journaled entry point and so has one.
- The exchange entries, `link_opened`, `vci_connected` and the settling entries write through
  the same log, which the runner gives the worker host.
- A reopen reads the whole file to check it, which grows with the log.
- A job whose setup names its journal's directory spelled another way (a `..`, a link) is not
  refused: the directories are compared as the file system resolves them.
- The log has the journal's protection: today the same directory, and the journal's encryption
  once that is built (design 5.5).
