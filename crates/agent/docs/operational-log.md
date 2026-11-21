# What the agent's `tracing` calls log

The agent's `tracing` events are its operational log (design 12.2). ADR-348 item 6 lists what an
operational log never holds. This file turns that list into rules for every `tracing` call in
`crates/agent` and in the workspace crates whose events reach the agent's subscriber. Today those
crates are `worker-host`, `ngr-library-resolver`, `j2534-0404-registry`, `vci-service-config`,
`vci-service-interface`, `diag-ir` and `shared-proto`; `cargo tree -p agent -e normal` lists them.
A review of a new or changed `tracing` call in any of them checks it against these rules.

## Never logged

- **Values read from the vehicle.**
  - Covers a VIN or other identity, a field's bytes, a software version and an ECU's answer.
  - Log the fact instead: what was read, its kind (`IdentityKind`), a length, and an NRC.
  - A response is logged as `answer_len` and, when it is negative, its `nrc`.
- **A diagnostic exchange's payload.**
  - Covers a request's or a response's bytes, a TransferData block, and a RequestDownload
    response. The communication log holds those (design 8.11).
- **Key and credential material.** Covers a seed or key, a token, OEM authorization material and
  signing material.
- **Text a procedure builds at run time.**
  - A `diag.log` message goes to the communication log only; the operational log records its
    level.
  - A `diag.fail` detail stays in the journal and the job's result; the operational log records
    its code.
  - A run-time value a VM error prints (an index, a byte, a shift amount, a wait) is left out
    (`vm_error_text`).

## Logged only in `external`

Some text NGR cannot check against the list above. It goes in the entry's `external` field,
escaped with `vci_service_config::External` (or with `?`, `Debug`, in `j2534-0404-registry`, which
does not link `vci-service-config`), and never in the message or another field:

- text NGR does not write itself:
  - a worker's gRPC status message;
  - a vendor library's error text;
  - a file or directory path, which may name a user's home;
  - a parse error that may quote the file it read;
- names that may identify a person or a device:
  - a VCI name, a J2534 registry device name, a D-PDU API module's short name or a `pname`;
  - an operator, a host or user name, an IP address.

An entry has at most one `external` field. When it carries several of these, they are written
into one string with labels, for example `file {path}, entry {name:?}`. `External` escapes:

- a backslash and line breaks;
- every other control character;
- the Unicode line and paragraph separators.

So the field cannot end the entry or imitate another one. An output that may be forwarded off the
device writes the field as its length only (ADR-348 item 6); that is the subscriber's work, not
the call site's.

## Errors

An error's `Display` text can carry any of the above. For example, `HostError::Rpc` prints the
worker's status message, and `HostError::TransferRefused` prints the response's bytes. A call
therefore logs an error through `oplog::LogSafe`, which `HostError`, `JobError` and `GuardError`
implement:

```rust
tracing::warn!(error = %error.log_text(), external = error.log_external(), "what failed");
```

`log_text` keeps NGR's own words, codes, positions and counts. `log_external` returns the
error's foreign text for `external`, or `None`, which records no field.

Some errors may still be logged with `%error`, because their text is NGR's own and carries none
of the above:

- `std::io::Error`: the OS's message and code, never a path;
- `JournalError`;
- `OutboxError`;
- `CommLogIncomplete`.

A new error type that is logged needs a `LogSafe` impl, unless its text is likewise NGR's own
throughout.

## Entries with a rule of their own

- **A refused job instruction** (`agent::instruction::open`, ADR-461 Decision item 10): the
  entry holds the coarse reason, the envelope's length and, for an unknown key, the fingerprint
  the envelope offered (hex of a hash). It never holds a job ID, agent ID, operator, VIN or VCI
  taken from the payload: an instruction that failed verification is not the job's own word, and
  one that verified for another agent is that agent's business.
  `instruction::open` writes one entry per refusal, for a caller that opens one envelope. The
  control channel's gate does not use that entry: it calls the variant without it and bounds what
  a connection writes itself, the first refusal per reason at `warn`, later ones at `debug`, and
  at the end of the connection one `info` line with the count per reason, if any. Both write the
  entry through one function, so its fields are the same. A `signedJob` that is missing or is not
  a string has the same entry (reason `Malformed`) with an envelope length of 0; one that is
  text but not base64 has it with the length of that text. No `commandId` is logged.

- **A job refused for its operator** (`agent::control::gate`, ADR-483): a verified instruction
  whose operator is not the registered owner, on a `personal` or `personalRemote` preset. The
  entry holds the job ID (the instruction verified, so the ID is the server's own word) and the
  failure code `operatorNotAllowed`; it never holds the operator ID, the owner ID or a
  `commandId`. The gate bounds it as it bounds the refused-instruction entry: the first per
  connection at `warn`, later ones at `debug`, and the end-of-connection `info` line of refused
  instructions carries the count as `operator_not_allowed`. The warning and the count are refusals decided, not refusals recorded: when the outbox is full the command stays unacknowledged, and its redelivery is decided and counted again.

- **A job past its start deadline** (`agent::control::gate`, ADR-486): a verified instruction
  whose operator is allowed and whose deadline is at or before the later of the device clock and
  the highest issue time. The entry holds the job ID and the state `expired`; it never holds a
  time (the deadline, the issue time or the clock), the operator ID or a `commandId`. The first
  per connection is a `warn`, later ones are `debug`, and the end-of-connection `info` line of
  refused instructions carries the count as `expired`. When the highest issue time and not the
  device clock decided it, one further `warn` per connection says so (the device clock is behind
  it), with the job ID only. Like the operator entry, it is a refusal decided, not recorded: the count and the warning come before the mark's write and the event's append, so when the mark cannot be written or the outbox is full the command stays unacknowledged and the end-of-connection line still counts it, and its redelivery is decided and counted again (the operator refusal behaves the same).

- **The issue-time mark on the control channel** (`agent::control`, ADR-486): a mark file that
  cannot be written warns once per `hello` cycle with the class `mark-write` and the
  `MarkError`'s own text (the kind of the I/O error; no path and no value), and is `debug` after
  that; the connection then closes with 1011 and the command is not acknowledged. The file's open
  is not logged: a corrupt one ends `ngr-agent connect` with its message and exit 1.

- **The received-jobs ledger on the control channel** (`agent::control`, `agent::received_jobs`,
  ADR-489): the entries hold a job ID and an ownership generation of a verified instruction, and
  nothing else of it: never a `commandId`, an operator ID, an owner ID, a VIN, a time or a byte of
  the envelope.
  - A **duplicate** (the job recorded under the same generation) is one `debug` entry per
    command, "a duplicate job command", with the job ID and the generation. The entry and the
    count are of duplicates decided, as the expiry's are (ADR-486): they come before the mark's
    write and the answer, so a command left unanswered by a mark failure or a stop is counted,
    and its redelivery is decided and counted again. A command for an
    **earlier generation** than the recorded one is a `warn` the first time on a connection and
    `debug` after that. Both are acknowledged; the end-of-connection `info` line of refused
    instructions carries the counts as `duplicate` and `earlier_generation`.
  - A record that cannot be written warns once per `hello` cycle with the class `ledger-write`
    (or `ledger-poisoned` for a record refused because an earlier write failed) and the
    `LedgerError`'s own text (the kind of the I/O error; no path, no job), and is `debug` after
    that; the connection then closes with 1011 and the command is not acknowledged. An
    intake that fails after the record was made warns once per `hello` cycle with the class
    `intake`, closes with 1011 and leaves the record.
  - A ledger that cannot be opened again before a connection warns once per `hello` cycle with
    the class `ledger-reopen`, and `debug` after that; a reopen that finds it corrupt is an
    `error` entry, "the session ends", and `ngr-agent connect` exits 1. A reopen that succeeds
    is one `info` entry. An open that cuts a torn last frame off is one `warn` with the byte
    count cut and nothing of the content. The file's open at the start is otherwise not logged:
    a corrupt one ends `ngr-agent connect` with its message and exit 1.

- **The control channel session** (`agent::control`, ADR-471): the entries hold a failure
  class, an HTTP status, a close code and the heartbeat interval. They never hold the token, the
  challenge, the nonce or the proof, an HTTP body, a message's text, a `commandId`, or the reason
  text of a Close the server sent. A failure class warns once until the next `hello` and is logged
  at `debug` after that; a refusal (403, a 429 on the session route, Close 4002) warns once until
  the server issues a session token again.
  `tungstenite` and `tokio-tungstenite` log through the `log` crate, at `trace` and `debug`, the
  upgrade request with its `Authorization` header, each message's text and a close's reason. The
  agent installs its subscriber with `set_global_default`, which installs no `log` bridge, so none
  of it reaches the operational log. tracing-subscriber's `init()` or `try_init()` would install
  one (its default `tracing-log` feature is on), so switching to them needs a filter that keeps
  those targets out first.

- **The outbox on the control channel** (`agent::control`, ADR-478): the entries hold a failure
  class (`outbox-full`, `outbox-poisoned`, `outbox-write`, `outbox-event`, `event-ack`) and the
  `OutboxError`'s own text; never an event's text, a job ID or a `commandId`. A failed
  `received` append warns once per class until the next `hello` and is `debug` after that. A
  failed rewrite of the outbox file logs the kind of the I/O error, as `Outbox` does. A poisoned
  outbox is opened again before each connection attempt: a success is `info`; a failure other
  than a corrupt file warns once per `hello` cycle with the class `outbox-reopen` and is `debug`
  after that, and the reopen is tried again before the next attempt; a corrupt file is an `error`
  line and ends the session.

- **The installation file** (`agent::installation`, ADR-494), read by `run`, `identify`,
  `identity`, `register` and `connect` after the log starts:
  - A valid file is one `info` entry, "installation file read", with the mode, the profile, the
    startup method and the root key's fingerprint (hex of a hash) as fields. The path and the
    server's origin are in its `external` field, as `file {path} server {origin}`, and only
    there: the origin is the operator's own text and the path may name a user's directory. The
    root key's PEM is never logged.
  - A file that cannot be used is one `error` entry, "the installation file cannot be used",
    with the error's own text as the `error` field (a fixed sentence that names a key and the
    rule it broke, a line number for a syntax error, or an I/O error kind; never a value from the
    file, since `toml`'s own error text, which quotes the line, is not used) and, in `external`,
    `file {path}` and, for a refusal by the writability check, the findings, which name paths.
    The same fixed sentence is printed on standard error by the command.

- **The fetcher** (`agent::fetch`, ADR-497): a fetch that gives up is written once at `warn` with the object's kind (`document` or `artifact`), the class (`retry`, `refused`, `local`, `mismatch`, `mismatch-twice`, `not-named`) and the HTTP status where there is one; each retry is written at `debug` with the same words and the attempt's number; a cached object that failed its check and was deleted is written once per kind at `warn`. Never a URL, a token, a digest, a path, a job ID or a body. The fetcher's token exchange writes the control channel's exchange entries under its own names (`fetch exchange attempt failed`, `fetch exchange refused`). The route and the client write nothing; the cache writes one `warn` when a sweep of its temporary directory cannot remove something. The exchange's lines are the uploader's own code with a different prefix (`crate::data_token`).
- **The uploader** (`agent::uploader`, ADR-495 item 11): a refusal, an abandon, a set-aside entry and a removal that fails are warned once per entry and class; the entry holds the entry's number, its kind (`result` or `commLog`), the class and the HTTP status where there is one, never a URL, a token, a digest, a path or a job ID. Retries are written at `debug` only. The uploader's token exchange writes the control channel's exchange entries under its own names. A ledger that cannot be reopened is written once at `warn`, and one that does not read back once at `error`; nothing is written after a stop. `ngr-agent connect` adds its own lines (`agent::connect`): `info` "uploader stopped" when a stop ended the task, `warn` "the uploader did not stop in time; the agent exits without it" when it did not end within 10 seconds, and, when the task ended on its own, one `error` "the uploader ended; nothing is uploaded until the agent restarts, and the control channel session goes on" with the reason (`LedgerUnusable`, or none for a panic); the session's lines are unchanged and `connect`'s exit status stays the session's.

## A crate not yet checked

A crate that starts to reach the agent's subscriber before its calls have been checked against
these rules is excluded by the subscriber's safety filter (ADR-348 item 9). The filter runs after
the level filter, and neither the configuration nor a level raise changes it. Its targets come out
of the filter once the crate is checked. The list is `UNCHECKED_TARGETS` in `src/logging.rs`;
it is empty, since every crate the agent links is checked.
