# ADR-348: Operational Logs

**Date:** 2026-10-23
**Status:** Accepted
**Affects:** design 12.2 (new), 8.7, 8.11 (the `procedure_log` event), 16.2; `agent` (logging setup,
worker output), `vci-service-launcher` and `vci-service-config` (logging output, rotation),
`worker-host` (worker stderr), `server`; ADR-024, ADR-033, ADR-089, ADR-285, ADR-346

## Context

NGR keeps two records that are already designed: the audit log (design 8.2.3, ADR-229), which
records what was done, by whom and on which vehicle, and must not be lost; and the agent's
communication log (design 8.11, ADR-346, ADR-347), which records each exchange with the vehicle
so that a failed run can be placed on the vehicle side or the system side. Neither is meant for
finding out why NGR itself misbehaved: a worker that failed to start, a configuration file that
was refused, a lost server connection, a library that returned something unexpected.

That third record, the operational log, has a design only for the workers. They log through
`tracing`, configured in `config.toml` by `level` (an `EnvFilter` directive, default `info`) and
`output` (`stderr`, `null`, `eventlog`, `journald` or an appended file), looked up per API,
library and architecture (ADR-024, ADR-033), and checked before use in device mode (ADR-285).
The rest is open:

- the agent emits `tracing` events but installs no subscriber, so they are discarded; its
  messages to the user go to stderr directly;
- each worker's stderr is inherited by the agent, so a worker's default output ends wherever the
  agent's stderr goes, unmarked;
- a file output grows without limit;
- nothing says what may not be written to an operational log, or how a log on a device reaches
  anyone who could read it;
- the server has no logging at all yet.

Comparable systems were surveyed before this decision: edge-device agents (AWS IoT Greengrass,
Azure IoT Edge), an over-the-air update client (Mender), in-vehicle logging (AUTOSAR DLT) and the
SOVD standard (ISO 17978-2:2026 use case 12, ISO 17978-3:2026 7.21). They agree on the shape:
operational logs are kept apart from audit and communication records; entries are structured
with a timestamp, a severity, a source and a message; the device keeps them in size-bounded,
rotated files; and a server fetches them on request or receives them when something failed,
rather than having them streamed all the time. SOVD likewise keeps a component's logs separate
from its communication logs and its faults, and aims the logs at software experts away from the
workshop.

## Decision

1. **Three records, kept apart.** The operational log is for the developers and maintainers of
   NGR. It is not the audit log and not the communication log: it is never the evidence of what a
   job did, an entry lost to rotation is acceptable, and nothing reads it to decide what to do.
   An event that belongs in the audit log or the communication log is written there; the
   operational log may mention it (a job id, an exchange's sequence number) but does not copy
   its content.
2. **One logging library and one set of levels.** The agent, every worker and the server log
   through `tracing` with its five levels, `error`, `warn`, `info`, `debug` and `trace`; the
   default is `info`. A process that cannot continue logs at `error` before it exits; there is no
   separate fatal level. Where SOVD severities are needed (item 10), `error`, `warn` and `info`
   map to the same names and `debug` and `trace` both map to `debug`.
3. **The agent sets up logging the way a worker does.** At startup the agent installs a
   subscriber configured from the same configuration file and the same `level` and `output`
   fields as the workers, in a table of the agent's own, checked as ADR-285 checks the workers'
   (in device mode a file that fails the check is not read and logging starts with its
   defaults). The default output is stderr when the agent runs from a terminal and a rotated file
   in a per-OS log directory when it runs as an OS service (design 6.5): `/var/log/nekoguruma/`
   on Linux and `%ProgramData%\nekoguruma\logs\` on Windows. Every entry logged while a job runs
   carries the job id and ownership generation (the journal's key) and the VCI name in a span,
   so it can be lined up with the job's journal and communication log.
4. **The agent captures its workers' output.** The agent reads each worker's stderr line by line and
   logs each line in its own operational log, the line in the entry's `external` field (item 6), at
   `info` unless the line carries a level the agent can read, with the worker's library name and ABI
   as fields. A worker configured with an output of its own (a file, `journald`, `eventlog`) keeps
   it, and its entries then stay at that destination: they are not in the agent's log, so neither a
   fetch nor a failed job's result (item 8) carries them. The default stderr output is the one that
   is collected; another output is a choice for local investigation on the device. A line longer
   than 8 KiB is cut to 8 KiB and marked as cut, so one misbehaving library cannot fill an entry
   without bound.
5. **Files are JSON Lines, rotated by size, with a cap.** A file output, in the agent and the
   workers alike, writes one JSON object per line with at least the timestamp (UTC, RFC 3339 with
   microseconds), the level, the `tracing` target, the process (`agent`, the worker's API and
   library, or `server`), the message and the span fields. stderr stays human-readable text. A
   file is rotated when it reaches its size limit, and the oldest rotated file is deleted once
   the count limit is reached. The limits are two new settings beside `output`: a default of
   10 MiB per file and 5 rotated files, so one log takes at most about 60 MiB.
6. **What an operational log never holds.** No VIN or other vehicle identity read from an ECU, no
   key, token, credential, seed or key value, OEM authorization material, signing material or bearer
   token, no personal data, and no request or response payload of a diagnostic exchange (the
   communication log holds those, under its own protection). Text NGR does not write itself cannot
   be checked against this list: an error description a vendor library returns (which may echo a
   diagnostic payload), a line of a worker's stderr, and a file path (which may name a user's home
   directory; ADR-089 keeps full paths out of gRPC replies and in the log). Such text goes in an
   entry's `external` field, apart from the message, with control characters and line breaks escaped
   so that one entry stays one line and cannot imitate another; item 8 says when it may leave the
   device. Text a procedure builds at run time can hold any of these, so it never reaches the
   operational log: a `diag.log` message (Starlark subset 3.1) is written to the job's communication
   log as a `procedure_log` event with its level, and a `diag.fail` detail stays in the job's
   journal and result; the operational log records at most that a procedure logged or failed, with
   the level or the code. A value the agent reads from the vehicle, such as an identity that did not
   match, is likewise logged as the fact, not the value. Until every existing `tracing` call has
   been checked against this item, the agent installs no subscriber (item 3).
7. **Device timestamps are not authoritative.** Entries carry the device's clock. Unlike the
   audit log (design 16.3), the operational log does not need a trusted time; when the server
   receives entries (item 8) it records its receive time beside them.
8. **The server fetches logs; it does not receive a stream.** The server can ask an agent for its
   operational log entries in a time range at or above a level, through an enumerated instruction
   (design 16.3); the agent answers from its local files, at most 4 MiB per request, over the data
   channel's resumable transfer (design 5.2). When a job fails, the agent sends the entries logged
   in that job's span at `info` and above, at most 1 MiB, with the job's result. Fetching is limited
   to administrators. Entries leave the device without their `external` fields, which are replaced
   by their length, unless an administrator's fetch asks for them; such a fetch is held to design
   16.2's rule for troubleshooting logs (sent only on an explicit action of the device's user, with
   the scope shown), and nothing sent automatically carries them. The server keeps fetched entries
   apart from the audit log, for a retention period that is an operator setting. An agent with no
   server, or offline, keeps its entries only in its rotated files.
9. **The agent's level can be raised for a while.** An enumerated instruction can set the agent's
   level filter for a bounded period, at most 24 hours, after which the configured level returns.
   It cannot change the output or any path, and only administrators can issue it, as for item 8.
   A worker's level changes only through its configuration file and a restart.
10. **The server logs JSON to stdout.** The server writes JSON Lines entries to stdout for the
    platform to collect (design 14); a local deployment (design 13), which runs as an OS service,
    writes the rotated file of item 5 instead. Its entries for a job carry the same job id as the
    agent's, so the two can be joined. Exporting to OpenTelemetry is left to a later decision. When
    NGR exposes SOVD (design 8.7), a component's `logs` resource is served from these entries, with
    the severity mapping of item 2 and a context type of NGR's own.

## Consequences

- The communication log gains a `procedure_log` event, so a procedure's own messages stay
  under that log's protection and sending rules (design 8.11, 16.2) beside the exchanges they
  explain.
- Opaque text from vendor libraries, workers and file paths stays on the device unless the
  device's user releases it, so a failed job's attached entries may lack the vendor's own words;
  an administrator asks for them through the explicit path.
- The agent's `tracing` events stop being discarded, and a worker's stderr in a service
  deployment lands in a file someone can read.
- A device holds at most about 60 MiB of operational log per process with the default limits;
  an incident older than the rotation window cannot be investigated from the device.
- The workers' file output changes from text to JSON Lines, which a person reads less easily
  than before; stderr keeps the text form for that.
- The agent gains two enumerated instructions (fetch the log, raise the level), each a new entry
  point the server can use; both are limited to reading the log and to the level filter.
- `crates/vci-service-config/docs/logging-config.md` documents the new settings, the JSON Lines
  format and the agent's table once they are implemented.
- Rejected: streaming every entry to the server (the surveyed systems keep it optional, and it
  costs bandwidth on the low-bandwidth links of design 5.4 for records that are rarely read);
  writing the operational log into the audit log (it would put rotation-tolerant debugging text
  under the audit log's retention and tamper-resistance duties); per-file time rotation as the
  only limit (a busy device could still fill its disk within a day).
