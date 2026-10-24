# The operational log's file output

`ngr-oplog` writes an operational log (design 12.2) to a file: one JSON object per line, in a
file rotated by size and readable only by the process's account and administrators (ADR-348
item 5). The agent and the workers install it as their file output. Each caller chooses:

- its filters;
- its process name;
- where the file goes;
- what to do when the file cannot be opened privately.

## Use

```rust
use ngr_oplog::{FileSettings, JsonLayer, RotatingFile};
use tracing_subscriber::layer::SubscriberExt;

let file = RotatingFile::open(FileSettings::new("logs/agent.log"))?;
let subscriber = tracing_subscriber::registry().with(JsonLayer::new("agent", file));
```

When `RotatingFile::open` fails (`OpenError`), the caller logs to stderr with a warning
(ADR-348 item 5). It fails when:

- the directory cannot be created;
- the file cannot be opened;
- the file cannot be made private, or another account owns it;
- the path is not a regular file. The open itself does not follow a symbolic link (Unix) or a
  reparse point (Windows), so one put in place after the check is refused too, and a FIFO does
  not block it.

## Entries

Each event is one line:

| Key | Value |
|---|---|
| `timestamp` | UTC, RFC 3339 with microseconds, from the device clock (not trusted, ADR-348 item 7) |
| `level` | `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE` |
| `target` | the event's `tracing` target, usually its module path |
| `process` | the name the caller gives: `agent`, `server`, or a worker's API and number, never a configured library or VCI name |
| `message` | the event's message |
| `fields` | the event's other fields, an object; left out when there are none |
| `external` | the event's `external` field, kept apart from `fields`; left out when absent |
| `spans` | the spans the event is in, root first, each with its `name`, its `fields` and its `external` field, the last two left out when empty; left out outside every span |

The keys come in this order.

A field recorded as an integer, a float or a boolean is written as a JSON number or boolean.
A NaN or an infinite float is written as text. Every other field is written as a string: a
`%` field as its `Display` text, a `?` field as its `Debug` text.

A span's fields are those it was created with and any recorded on it later; a span field named
`message` is one of its `fields`. A span object's keys are sorted.

JSON escapes every control character in a string, so a value never breaks the line. The
`external` field's text is escaped by its caller as well (`vci_service_config::External`,
`crates/agent/docs/operational-log.md`).

## Rotation

`FileSettings` gives three things:

- the path, for example `logs/agent.log`;
- the size limit, `max_bytes`, 10 MiB by default;
- the number of rotated files kept, `max_files`, 5 by default and at most 100 (a larger value
  counts as 100, since each rotation renames every rotated file while the log waits).

Before an entry that would take a non-empty file past the limit, the file is rotated:

1. the oldest rotated file, `agent.5.log`, is deleted;
2. each other rotated file moves one number up;
3. the file becomes `agent.1.log`;
4. a new empty file is started.

An entry is never split. An entry larger than the limit goes whole into an empty file. With
`max_files` at 0 the file is emptied and started again. One process's log therefore takes at
most about `(max_files + 1) × max_bytes`, about 60 MiB by default.

A reopened file is appended to, and its length counts toward the limit. A file whose last line
has no end (a crash in the middle of an entry) is ended with a line break first, and so is an
entry whose write failed part of the way, so the next entry always starts a line of its own.
When even that line break cannot be written, it is written before the next entry, which is
refused while it still cannot be.

A rotation never stops the log:
- each step is tried even when an earlier one failed, for example a rotated file another process
  holds open, or one that cannot be deleted;
- a current file that cannot be renamed is emptied instead, through the open handle (on Windows
  the file is opened to write rather than to append, so it can be, and each entry is written at
  its end). Where the OS refuses that too, it keeps growing, by the limit each time it is
  reached;
- when the new file cannot be opened, the renamed one is moved back and emptied. Where that fails
  too, entries go on to the renamed file: no later rotation moves or deletes it, each rotation
  tries again to open a new file and otherwise empties it, so the log stays bounded;
- the entry that started the rotation is written in every case.

## Permissions

| | Unix | Windows |
|---|---|---|
| A directory the writer creates (each missing one, its parents included) | `0700` | an access list, protected from inheritance, allowing only the owner, SYSTEM and the Administrators group, inherited by what is created in it; a directory that cannot be given it is removed again |
| The file | `0600`; an existing file with wider access is narrowed when it is opened | the same access list, set each time the file is opened |
| An existing file another account owns | refused (narrowing it would leave its owner access); root may own it | refused, unless the process's account, SYSTEM or the Administrators group owns it |
| An existing directory | left as it is: the installation may have prepared it, or a configured path may name a shared one | left as it is |

A rotated file keeps its permissions, since it is the same file renamed.

On Windows the owner is read and the access list set through the handle the writer opened, never
by the path, so a path another account changes in between cannot redirect either.

## Text output

`TextLayer` writes each entry as one line of text, for stderr (design 12.2: stderr stays
human-readable):

```text
2026-10-24T06:40:07.123456Z  WARN agent::runner: job{generation=0 job_id="run-1" external="bench"}: could not close the link pc=7
```

A line holds, in this order:
- the timestamp, the level and the target;
- the spans, root first, each with its fields;
- the message, with its line breaks and control characters escaped;
- the event's fields, string values quoted and escaped.

`ExternalText` chooses how an `external` field is written:
- `Full` writes the text itself, for an output that stays on the device (a terminal);
- `LengthOnly` writes `<n bytes>`, for an output the OS may forward (a stderr that is not a
  terminal, `journald`, `eventlog`; ADR-348 item 6).

## One file per process

`Slot::claim(dir, stem)` gives a process a file of its own (ADR-348 item 5). It claims the
smallest number `n` whose lock file `{stem}.{n}.lock` it can lock exclusively, and the file is
`{stem}.{n}.log`. The OS releases the lock when the process ends, so no two running processes
share a file, and the numbers stay bounded by how many run at once.
