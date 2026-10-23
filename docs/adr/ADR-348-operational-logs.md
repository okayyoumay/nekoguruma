# ADR-348: Operational Logs

**Date:** 2026-10-23
**Status:** Accepted
**Affects:** design 12.2 (new), 8.7, 8.11 (the `procedure_log` event), 16.2; `agent` (logging setup,
worker output), `vci-service-launcher` and `vci-service-config` (logging output, rotation),
`worker-host` (worker stderr), `server`, design 6.8; ADR-024, ADR-033, ADR-089, ADR-285, ADR-346,
ADR-347

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

Comparable systems were surveyed before this decision: edge-device agents (AWS IoT Greengrass, Azure
IoT Edge), an over-the-air update client (Mender) and the SOVD standard (ISO 17978-2:2026 use case
12, ISO 17978-3:2026 7.21, with communication logs in 7.22). They agree on the shape: operational
logs are kept apart from audit and communication records; entries are structured with a timestamp, a
severity, a source and a message; the device keeps them in size-bounded, rotated files; and a server
fetches them on request or receives them when something failed, rather than having them streamed all
the time. SOVD likewise keeps a component's logs separate from its communication logs and its
faults, and aims the logs at software experts away from the workshop.

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
3. **The agent sets up logging the way a worker does, and always keeps a file.** At startup the
   agent installs a subscriber configured from the same configuration file and the same `level` and
   `output` fields as the workers, in a table of the agent's own, checked as ADR-285 checks the
   workers' (in device mode a file that fails the check is not read and logging starts with its
   defaults). The default output is stderr when the agent runs from a terminal and the rotated file
   alone when it runs as an OS service (design 6.5). Whatever `output` names, the agent also writes
   its entries to its rotated file of item 5, which is what item 8 collects, so a stderr, `journald`
   or `eventlog` output does not leave the server with nothing to fetch. The file's directory is a
   setting. Its default for an agent run as an OS service is `/var/log/nekoguruma/` on Linux and
   `%ProgramData%\nekoguruma\logs\` on Windows, which the installation creates owned by the service
   account; for an agent run from a terminal it is the user's own state directory,
   `$XDG_STATE_HOME/nekoguruma/logs/` (by default `~/.local/state/nekoguruma/logs/`) on Linux and
   `%LOCALAPPDATA%\nekoguruma\logs\` on Windows, which the agent creates. Only `null` turns the file
   off; when it is off or cannot be opened, the agent says so on its other output, and a fetch or a
   failed job's result reports that the agent keeps no log rather than returning nothing. Every
   entry logged while a job runs carries the job id and ownership generation (the journal's key) and
   the VCI name in a span (the VCI name in the span's `external` field, item 6), so it can be lined
   up with the job's journal and communication log.
4. **The agent captures its workers' output.** The agent reads each worker's stderr line by line and
   logs each line in its own operational log, the line in the entry's `external` field (item 6), at
   `info` unless the line carries a level the agent can read, with the worker's API, ABI and worker
   number (item 5) as fields and its library name, which a user or vendor configures and may name a
   device, in `external` as well. A worker configured with an output of its own (a file, `journald`,
   `eventlog`) keeps it, and its entries then stay at that destination: they are not in the agent's
   log, so neither a fetch nor a failed job's result (item 8) carries them. The default stderr
   output is the one that is collected; another output is a choice for local investigation on the
   device. A line longer than 8 KiB is cut to 8 KiB and marked as cut: the reader keeps at most 8
   KiB of a line and reads and discards the rest up to the next line break or the end of the stream,
   so one misbehaving library that never ends a line can neither fill an entry nor the agent's
   memory without bound.
5. **Files are JSON Lines, one per process, private, rotated by size, with a cap.** A file output,
   in the agent and the workers alike, writes one JSON object per line with at least the timestamp
   (UTC, RFC 3339 with microseconds), the level, the `tracing` target, the process (`agent`,
   `server`, or a worker's API and worker number, never its configured library or VCI name, which go
   in `external`), the message and the span fields. stderr stays human-readable text. Every process
   writes a file of its own: a worker adds a stem naming its API and its worker number to the
   configured path. The agent gives each worker it launches a number unique among the workers it
   runs at that time (the smallest one free) on its command line, so workers that share a
   library-level or root setting, a J2534 library's several devices and the per-VCI D-PDU workers of
   one library (design 8.8.1) alike, never write or rotate the same file; a worker started without
   one uses its process id. A log file and the directory NGR creates for it are readable and
   writable only by the account the process runs as and by administrators (mode 0600 for files and
   0700 for directories on Unix, an access list granting only that account, SYSTEM and
   Administrators on Windows), since entries can carry `external` text (item 6); an existing file
   with wider access is narrowed when opened, or, where that fails, the output falls back to stderr
   with a warning (a stderr that item 6 treats as forwardable). A file is rotated when it reaches
   its size limit, and the oldest rotated file is deleted once the count limit is reached. The
   limits are two new settings beside `output`: a default of 10 MiB per file and 5 rotated files, so
   one process's log takes at most about 60 MiB.
6. **What an operational log never holds.** No VIN or other vehicle identity read from an ECU, no
   key, token, credential, seed or key value, OEM authorization material, signing material or bearer
   token, no personal data, and no request or response payload of a diagnostic exchange (the
   communication log holds those, under its own protection). Text NGR does not write itself cannot
   be checked against this list: an error description a vendor library returns (which may echo a
   diagnostic payload), a line of a worker's stderr, and a file path (which may name a user's home
   directory; ADR-089 keeps full paths out of gRPC replies and in the log). Such text goes in an
   entry's `external` field, apart from the message, with control characters and line breaks escaped
   so that one entry stays one line and cannot imitate another; item 8 says when it may leave the
   device. An output that the OS may forward off the device writes an `external` field as its length
   only, like the server's copy: `journald`, `eventlog`, and stderr whenever it is not a terminal
   (an OS service's stderr goes to the service manager, which may forward it), the fallback of item
   5 included. The full text is kept only in the local file, on a terminal's stderr and on a
   worker's stderr, which the agent reads through a private pipe (item 4). Names that may identify a
   person or a device go there too, wherever they appear, span fields included: the operator, a
   device or host name, an OS user name and an IP address (design 16.2), and a name a user or vendor
   gives a VCI (a VCI name, a J2534 `pname` or label). Text a procedure builds at run time can hold
   any of these, so it never reaches the operational log: a `diag.log` message (Starlark subset 3.1)
   is written to the job's communication log as a `procedure_log` event with its level (a job whose
   procedure uses `diag.log`, which its IR shows when the job starts, always has a communication
   log, even without a journal and without its caller asking for one, so the message is never
   dropped: when such a `run_program` job carries no setup, the agent gives it one, in its
   communication-log directory, a setting that defaults to `commlog/` in the agent's local storage
   beside its journals (design 5.5), with a job id of its own as `ngr-agent run --comm-log` does;
   this extends ADR-347 Decision item 2; the communication log's rule that secret bytes are kept as
   their length only, design 8.11, holds for these messages too: ingestion rejects a `diag.log`
   message or `diag.fail` detail built from a value read from a SecurityAccess or Authentication
   response, so no such bytes reach either), and a `diag.fail` detail stays in the job's journal and
   result; the operational log records at most that a procedure logged or failed, with the level or
   the code. A value the agent reads from the vehicle, such as an identity that did not match, is
   likewise logged as the fact, not the value. Until every existing `tracing` call has been checked
   against this item, the agent installs no subscriber (item 3).
7. **Device timestamps are not authoritative.** Entries carry the device's clock. Unlike the
   audit log (design 16.3), the operational log does not need a trusted time; when the server
   receives entries (item 8) it records its receive time beside them.
8. **The server fetches logs; it does not receive a stream.** The server can ask an agent for its
   operational log entries in a time range at or above a level, through an enumerated instruction
   (design 16.3); the agent answers from its local files in time order, at most 4 MiB per request,
   with a cursor naming the next entry when more match, which the next request passes to continue (a
   cursor whose entry has been rotated away since is answered from the oldest entry left, and the
   reply says entries were lost), over the data channel's resumable transfer (design 5.2). When a
   job fails, the agent sends the entries logged in that job's span at `info` and above, at most 1
   MiB, with the job's result; when they do not fit, entries are taken in this order until the cap
   is reached: the entry that recorded the job's failure, then `error` entries newest first, then
   `warn` entries newest first, then `info` entries newest first; the attachment says how many
   entries it left out at each level, and an entry larger than the space left is skipped rather than
   cut. The attachment goes only to the server that issued the job and is stored with that job; its
   job id and ownership generation are that server's own keys, not identifiers of a person or
   vehicle, and with `external` fields removed it adds nothing about the vehicle or any person to
   what the server already holds for the job, so it needs no anonymization beyond that removal
   (design 16.2). Fetching is limited to administrators. Entries leave the device without their
   `external` fields, span fields included, which are replaced by their length, unless an
   administrator's fetch asks for them; such a fetch is held to design 16.2's rule for
   troubleshooting logs (sent only on an explicit action of the device's user, with the scope
   shown), and nothing sent automatically carries them. The agent itself obtains that action: it
   holds the fetch and waits for a user of the device to approve it there, never through the server.
   The approval is given with a command of the agent's own run on the device (for example `ngr-agent
   logs approve`), which reaches the running agent over a local-only channel (a Unix-domain socket
   or a named pipe that accepts only the device's local users, never a network listener) whether or
   not the server is connected, lists the pending requests with their time range, level and the fact
   that vendor and worker text and names are included, and approves or declines one of them; an
   agent run from a terminal may also ask on that terminal. The offline listener of design 5.7.1 is
   not used for this. The approval names that one request and covers it and its continuations only;
   when the device's user declines, or does not answer within 10 minutes, the agent answers without
   `external` fields and the reply says that approval was not given. The server's or the
   administrator's request is never taken as the approval. The server keeps fetched entries apart
   from the audit log, for a retention period that is an operator setting. An agent with no server,
   or offline, keeps its entries only in its rotated files.
9. **The agent's level can be raised for a while.** An enumerated instruction can set the agent's
   level filter for a bounded period, at most 24 hours, after which the configured level returns.
   The raise is one level applied to every target and laid over the configured filter: each
   configured directive keeps its own level when that is more verbose, so a raise never makes any
   target quieter (a configured `info,j2534_0404=trace` raised to `debug` logs `debug` everywhere
   and still `trace` for that target). It cannot change the output or any path, and only
   administrators can issue it, as for item 8. A worker's level changes only through its
   configuration file and a restart.
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
