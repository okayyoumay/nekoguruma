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
- `CommLogIncomplete`.

A new error type that is logged needs a `LogSafe` impl, unless its text is likewise NGR's own
throughout.

## A crate not yet checked

A crate that starts to reach the agent's subscriber before its calls have been checked against
these rules is excluded by the subscriber's safety filter (ADR-348 item 9). The filter runs after
the level filter, and neither the configuration nor a level raise changes it. Its targets come out
of the filter once the crate is checked.
