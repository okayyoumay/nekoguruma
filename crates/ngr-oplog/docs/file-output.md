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
- the file cannot be made private;
- the path is not a regular file (a symbolic link is refused).

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

A span's fields are those it was created with and any recorded on it later.

JSON escapes every control character in a string, so a value never breaks the line. The
`external` field's text is escaped by its caller as well (`vci_service_config::External`,
`crates/agent/docs/operational-log.md`).

## Rotation

`FileSettings` gives three things:

- the path, for example `logs/agent.log`;
- the size limit, `max_bytes`, 10 MiB by default;
- the number of rotated files kept, `max_files`, 5 by default.

Before an entry that would take a non-empty file past the limit, the file is rotated:

1. the oldest rotated file, `agent.5.log`, is deleted;
2. each other rotated file moves one number up;
3. the file becomes `agent.1.log`;
4. a new empty file is started.

An entry is never split. An entry larger than the limit goes whole into an empty file. With
`max_files` at 0 the file is deleted and started again. One process's log therefore takes at
most about `(max_files + 1) × max_bytes`, about 60 MiB by default.

A reopened file is appended to, and its length counts toward the limit.

## Permissions

| | Unix | Windows |
|---|---|---|
| A directory the writer creates (each missing one, its parents included) | `0700` | an access list, protected from inheritance, allowing only the owner and the Administrators group, inherited by what is created in it |
| The file | `0600`; an existing file with wider access is narrowed when it is opened | the same access list, set each time the file is opened |
| An existing directory | left as it is: the installation may have prepared it, or a configured path may name a shared one | left as it is |

A rotated file keeps its permissions, since it is the same file renamed.
