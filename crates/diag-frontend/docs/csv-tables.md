# CSV Table Format (CSV -> IR Declaration Part)

Corresponds to design document 8.4. Defines the CSV tables of the proprietary vehicle-knowledge
format (CSV tables + Starlark) and how `diag-frontend` converts them into the IR declaration part
(`crates/diag-ir/schema/ir.fbs`). ADR-295 records the decisions behind the format. The procedure
part is written in Starlark (`starlark-subset.md`).

An example definition of one ECU is in `crates/diag-frontend/examples/example-ecu/`. Every table
below has rows there, and the examples in this document are taken from it.

---

## 1. Definition Directory

One ECU is defined by one directory. Its tables are the CSV files below, each with a fixed name.
A table that is absent is empty; so is a file of zero bytes, and a file with only a header row.
File names match exactly, case included; any other file whose name ends in `.csv`, in any case,
is rejected (`CSV_UNKNOWN_FILE`). Files that are not `.csv`, such as the Starlark procedure files and a
`README`, are not tables and are ignored by the table conversion. A symbolic link is read as the
file it points to; an entry named like a `.csv` or `.star` file that is not a regular file (a
directory, a link to nothing) is rejected (`CSV_UNKNOWN_FILE`).

| File | Required | Section | IR target |
|---|---|---|---|
| `variants.csv` | yes | 4.1 | `EcuVariant` (one `EcuDocument` per row) |
| `variant_patterns.csv` | no | 4.2 | `EcuVariant.patterns` |
| `protocols.csv` | yes | 4.3 | `EcuVariant.protocols` |
| `comparams.csv` | no | 4.3 | `Protocol.params` |
| `texts.csv` | no | 4.4 | text ids (`text_id`, `message_id`) |
| `units.csv` | no | 4.4 | `unit_id` |
| `conversions.csv` | no | 4.5 | `Compu` |
| `conversion_scales.csv` | no | 4.5 | `Compu` arrays |
| `services.csv` | no | 4.6 | `Service` |
| `dids.csv` | no | 4.7 | generated `Service`s (ReadDataByIdentifier, WriteDataByIdentifier) |
| `routines.csv` | no | 4.8 | generated `Service`s (RoutineControl) |
| `nrcs.csv` | no | 4.9 | `Service.negatives` |
| `layouts.csv` | no | 4.10 | `Layout` |
| `fields.csv` | no | 5 | `Field`, `Request.prefix`, `PositiveResponse.prefix` |
| `repeats.csv` | no | 5.4 | `Repeat` |
| `select_cases.csv` | no | 5.4 | `Select`, `SelectCase` |
| `dtcs.csv` | no | 4.11 | `DtcDefinition` |
| `flash_sessions.csv` | no | 4.12 | `FlashSession` |
| `flash_segments.csv` | no | 4.12 | `FlashSegment` |

The table conversion produces the declaration part of each variant's documents (design 8.2.6),
with `source_format` set to `csv-starlark` (ADR-254). Each `.star` file directly in the
directory is one procedure (`starlark-subset.md` 2.1), and the documents are packaged by ADR-307:

- With procedure files, there is one document per variant and procedure: the variant's whole
  declaration part and, in `EcuDocument.procedure`, the procedure's program lowered for that
  variant, as postcard bytes. Every procedure goes with every variant, and is lowered once per
  variant against its declarations (ADR-311). The documents come in `variants.csv` row order,
  and within a variant in byte order of the procedure file names.
- Without procedure files, there is one document per variant, with no procedure.

A procedure file is any file whose name ends in `.star`, in any case. In `diag-frontend`,
`convert::lower_procedures` checks the procedure files of a definition that `tables::resolve`
accepted and lowers each for every variant, listing every file's errors
together with the warnings of the files that lowered (a file that is not UTF-8 is
`STAR_SYNTAX_ERROR`, `starlark-subset.md` 2.1), and
`convert::to_ir` converts a definition that `tables::resolve` accepted (section 7) with those
procedures. It returns the documents, each named by its variant and its procedure's file name,
with the catalogs of 4.4. Monitor sets (`EcuVariant.monitor_sets`) and `CompuBytecode` have no
table in this version: the converter emits no monitor set, and a conversion kind other than those
in 4.5 is rejected.

---

## 2. CSV Dialect

- The record format of RFC 4180 section 2: comma-separated, fields that contain a comma, a
  double quote or a line break are enclosed in double quotes, and a double quote inside them is
  doubled. As an extension to that section, which ends records with CRLF, a record may also end
  with LF alone. As that section's grammar has it, no cell holds an ASCII control character
  (a tab, NUL or DEL among them), except line breaks inside a quoted cell (`CSV_SYNTAX`).
- UTF-8. A byte order mark at the start of a file is skipped, so files saved by spreadsheet
  programs are accepted. Any other invalid UTF-8 is rejected.
- The first record is the header. Columns are identified by their header name, not their
  position, so their order is free. A header name that the table does not define is rejected
  (`CSV_UNKNOWN_COLUMN`), as is a header name that appears twice.
- An optional column may be left out of the header; every row then has it empty. A required
  column must be in the header (`CSV_MISSING_COLUMN`) and non-empty in every row.
- Every table accepts a `note` column, which the conversion ignores.
- An empty line, and a row whose cells are all empty, are skipped before anything else is
  checked. Any other row with a different number of cells than the header is rejected.
- Cells are taken as written: leading and trailing spaces are part of the value, and a value
  that does not parse is rejected (`CSV_BAD_VALUE`).

## 3. Values

| Type | Syntax | Example |
|---|---|---|
| integer | Decimal, with `-` allowed where the column is signed, or hexadecimal with a lowercase `0x` prefix, which is an unsigned magnitude (no sign; `0xFFFFFFFFFFFFFFFF` in a signed column is out of range, not -1). No `+`. Each column states its range | `500000`, `0x7E0` |
| float | Decimal, with an optional fraction and exponent; integers are accepted. A `.` has a digit on both sides (`0.5`, not `.5` or `5.`); hexadecimal is not accepted. The value must be finite in IEEE 754 double precision: one that overflows to infinity, and `inf` or `nan`, is rejected (`CSV_BAD_VALUE`) | `0.1`, `1e-3` |
| bytes | Hexadecimal digit pairs, upper or lower case, with optional single spaces between pairs; no `0x` prefix | `F190`, `31 2E` |
| boolean | `yes` or `no` | `yes` |
| name | `[A-Za-z_][A-Za-z0-9_]*` | `softwareVersion` |
| key | A name, or names joined by `.` | `session.default` |
| list | Values joined by `;`, without spaces. `;` is reserved: no element may contain it, and there is no escape. An empty element (`a;;b`, a leading or trailing `;`) is rejected. In a list of names or keywords (`variants`, `part_numbers`, `sw_versions`, `steps`) an element given twice is rejected too; a list of numbers (`numerator_coeffs`, `denominator_coeffs`) may repeat values | `1.0.0;1.0.1` |
| text | Any string | `Default session` |
| service name | A `name`, or a DID or routine name followed by `.read`, `.write`, `.start`, `.stop` or `.results`, naming a generated service (4.7, 4.8) | `vin.read` |

An empty optional cell takes the column's default.

**Names and references.** Rows refer to each other by name, never by IR id. A name is unique
within its table (variants, services, DIDs, routines, layouts, conversions, flash sessions) or its
container (fields, 5.1), across the whole definition, whichever variants a row belongs to: a
variant that needs a different layout for the same DID defines a second DID row with another
name. A reference to a name that does not exist is rejected (`CSV_UNKNOWN_REFERENCE`); a
duplicate name is rejected (`CSV_DUPLICATE`).

**IR ids.** Ids that only connect parts of one document (`Field.id`, `Layout.id`, `Service.id`)
are assigned by the converter from 1, over the whole definition, so the same row has the same id
in every variant's document:

- fields in `fields.csv` row order, counting only rows that become IR fields (a constant, 5.3,
  takes no id), then the marker fields of `repeats.csv` (5.4) in its row order;
- layouts in `layouts.csv` row order;
- services in `services.csv` row order, then the DID services in `dids.csv` row order (read before
  write), then the routine services in `routines.csv` row order (start, stop, results). Only
  services that are generated take an id: a DID without a write service takes one id, not two.

Ids that something outside the document refers to are written in the tables: variant ids
(`variants.csv`), text and unit ids (resolved on the server, design 9.2) and flash session ids
(`diag.flash_transfer(session_id)` in Starlark).

**Variant scope.** Tables marked "scoped" in section 4 have an optional `variants` column: empty
means every variant, otherwise a list of variant names. A variant's document contains the scoped
rows that name it or name no variant, together with the layouts, conversions, texts, units and
fields those rows reach. Two rows of one table that would put two definitions of the same thing
into one variant's document (the same DID number, routine identifier, DTC code, flash session id
or protocol name) are rejected. A row that names a scoped row of another table (a ComParam its
protocol, an NRC its service, a flash segment its session, a variant pattern its service) must
belong only to variants that the named row also belongs to; otherwise the name does not exist
in some variant's document and the row is rejected (`CSV_UNKNOWN_REFERENCE`).

---

## 4. Tables

### 4.1 `variants.csv`

| Column | Type | Required | Meaning |
|---|---|---|---|
| `id` | integer, 1 to 2^32 - 1 | yes | `EcuVariant.id`, unique |
| `name` | name | yes | `EcuVariant.short_name` |
| `part_numbers` | list of text | no | `EcuVariant.part_numbers` |
| `sw_versions` | list of text | no | `EcuVariant.sw_versions` |

At least one row is required.

### 4.2 `variant_patterns.csv`

Variant identification (design 8.9.1), one `VariantPattern` per row, in row order.

| Column | Type | Required | Meaning |
|---|---|---|---|
| `variant` | name | yes | The variant the pattern identifies |
| `service` | service name | yes | A service in that variant's document, including generated ones (`softwareVersion.read`) |
| `field` | name | yes | A scalar field at the top of that service's positive response: not a constant (5.3), not shaped and not inside a nested layout (5.4), not a `float` and without a numeric conversion |
| `expected` | bytes | yes | `VariantPattern.expected`, compared with the field's decoded value as below |
| `pattern` | integer, 1 to 65535 | no | `VariantPattern.pattern`, default 1: the rows of one variant with the same number form one pattern |

A variant's rows are grouped into patterns by `pattern` (ADR-315, after ISO 22901-1 (2008)
7.4.3). A pattern matches when every one of its rows matches, a variant is identified when any of
its patterns matches, and a variant without rows is never identified. The variants are tried in
rising order of their names, and the first that matches is selected, so patterns of different
variants should not overlap; prefix matching makes `31 2E 32` (text `1.2`) also match `1.20`. A row whose service gave no positive response does not match. Without the
column, all of a variant's rows form one pattern.

Each row compares `expected` with the value the field decodes to, not with the bytes on the wire
(ADR-306):

- A `bytes` field matches when `expected` is a prefix of its bytes. An `ascii`, `utf8` or
  `unicode2` field matches when `expected` is a prefix of its text in UTF-8, so `31 2E` matches a
  version `1.` followed by anything, whatever the field's encoding. An `ascii` byte of 0x80 or
  above counts as that code point, so `FF` on the wire is written `C3 BF`. A terminated field's
  terminator is not part of its text, so a prefix cannot tell `1.2` from `1.20`; a fixed-length
  field padded after its value can, with the padding written into `expected`.
- A `uint` or `int` field matches when `expected`, one to eight bytes read as a big-endian number,
  equals its value; an `int`'s `expected` is two's complement of its own width (`FE` is -2). A
  little-endian field's `expected` is written big-endian too: `12 34` matches a field sent as
  `34 12`.
- A `bcd` field's `expected` is written as its digits, two per byte, most significant first, each
  0 to 9 (`12 34` for 1234), at most eight bytes. The converter writes the number they spell into
  the IR.
- A field with a text table compares its internal value, not the text.
- A `float` field, or a field whose conversion is not `identity` or `text-table`, decodes to a
  float that no byte string matches exactly, and is rejected (`CSV_BAD_VALUE`).
- A pattern that could never match is rejected too (`CSV_BAD_VALUE`): an integer's `expected`
  longer than eight bytes, a value a fixed-width `uint`, `int` or `bcd` field cannot hold (leading
  zero bytes or digits are fine), an `expected` longer than a fixed-length `bytes` field, and a
  text field's `expected` that is not UTF-8.

### 4.3 `protocols.csv` (scoped) and `comparams.csv` (scoped)

`protocols.csv`, one `Protocol` per row. Every variant needs at least one.

| Column | Type | Required | Meaning |
|---|---|---|---|
| `name` | text | yes | `Protocol.short_name`, such as `ISO15765` |
| `baud_rate` | integer, 1 to 2^32 - 1 | yes | |
| `tx_id` | integer, 0x000 to 0x7FF | yes | Physical request identifier |
| `rx_id` | integer, 0x000 to 0x7FF | yes | Physical response identifier |
| `functional_id` | integer, 0x000 to 0x7FF | no | Default 0 (none) |
| `extended_addressing` | boolean | no | Default `no` |
| `application_layer` | text | no | `Protocol.application_layer`, the application layer the protocol carries, default `UDS`; its profile's negative-response shape and pending codes go into `Protocol.negative_shape` |

The identifiers are 11-bit CAN identifiers. A 29-bit identifier also needs the ComParams that
select the identifier format, which this version does not define, so a value above 0x7FF is
rejected (`CSV_BAD_VALUE`); the agent's link accepts only 11-bit identifiers for the same reason
(ADR-235 item 4).

`application_layer` selects the application-layer profile the variant's procedures are lowered
through (ADR-309 item 3, ADR-314). It must name a profile that ships with the frontend, `UDS`
being the only one, or it is rejected (`CSV_UNKNOWN_REFERENCE`). Every protocol of one variant
must name the same application layer, since a variant's procedures are one program per variant
whose requests belong to one layer; a protocol whose layer differs from that of the variant's
first protocol is rejected (`CSV_DUPLICATE`). An ECU reachable over two application layers is
declared as two variants.

`comparams.csv`, one `ComParam` per row, appended to the named protocol's `params` in row order.
The agent sets them on the job's link as written (ADR-324). It reads `CP_P2Max`, `CP_P2Star`
and `CP_RC78CompletionTimeout` as the link's timings, in microseconds as ISO 22900-2 defines
them. It refuses a ComParam with a `unit`, since it cannot convert units (give the value in
the ComParam's own unit), and a ComParam the link sets from the protocol row itself (such as
`CP_Baudrate` or the CAN identifiers); `crates/agent/docs/ngr-agent.md` lists the rest.

| Column | Type | Required | Meaning |
|---|---|---|---|
| `protocol` | text | yes | A protocol in the same variant's document |
| `name` | text | yes | The COMPARAM short name, held standard-neutral (design 8.5); unique per protocol and variant |
| `value` | signed integer, 64 bits | yes | |
| `unit` | key | no | A `units.csv` key |

### 4.4 `texts.csv` and `units.csv`

Text and unit ids are resolved on the server (design 9.2), so they are ids of the server's
message and unit tables, shared by every definition, not numbers local to one directory. The
tables give each id a key that other tables refer to, and its default text or symbol. Besides the
documents, the conversion outputs these pairs as a text catalog (id, default text) and a unit
catalog (id, symbol), sorted by id; an import that finds an id already registered with a
different text or symbol is a conflict for the server to report, not something the converter can
see.

`texts.csv`:

| Column | Type | Required | Meaning |
|---|---|---|---|
| `key` | key | yes | Unique |
| `id` | integer, 1 to 2^32 - 1 | yes | Unique; 0 means "no text" in the IR |
| `text` | text | yes | Default-language text |

`units.csv`: the same with an optional `symbol` (text) in place of `text`; 0 means "no unit".

### 4.5 `conversions.csv` and `conversion_scales.csv`

A conversion is named in `conversions.csv` and referenced from a field's `conversion` column. A
field without one uses `CompuIdentity`. The kinds that need arrays take one `conversion_scales.csv`
row per array entry, in row order, which is the IR array order. That order matters for
`text-table`, where the first matching entry wins.

`conversions.csv`:

| Column | Type | Used by | Meaning |
|---|---|---|---|
| `name` | name | all | Required, unique |
| `kind` | `identity`, `linear`, `scale-linear`, `text-table`, `interpolation`, `rational` | all | Required |
| `numerator` | float | `linear` | `CompuLinear.numerator`, default 1 |
| `denominator` | float, not 0 | `linear` | Default 1 |
| `offset` | float | `linear` | Default 0 |
| `numerator_coeffs` | list of float | `rational` | Required for `rational`, at least one; ordered by ascending power, the first being the constant term |
| `denominator_coeffs` | list of float | `rational` | Required for `rational`, at least one, in the same order, and not all zero |
| `default_text` | key | `text-table` | `CompuTextTable.default_text_id`; empty is 0 |

`conversion_scales.csv`:

| Column | Type | Used by | Meaning |
|---|---|---|---|
| `conversion` | name | all | Required |
| `lower` | integer (`text-table`) or float (`scale-linear`) | `scale-linear`, `text-table` | Required there |
| `upper` | as `lower` | `scale-linear`, `text-table` | Default: the same as `lower`; `lower` <= `upper` |
| `numerator`, `denominator`, `offset` | float | `scale-linear` | Defaults 1, 1, 0; `denominator` not 0 |
| `text` | key | `text-table` | Required there |
| `internal`, `physical` | float | `interpolation` | Required there; `internal` strictly ascending |

A cell that the conversion's kind does not use must be empty, and a kind without arrays takes no
`conversion_scales.csv` rows. `scale-linear` and `interpolation` need at least one row (two for
`interpolation`). `text-table` limits are signed 64-bit integers, compared with the internal
value; a `text-table` on a `float` field is rejected (`ir.fbs`, `TextEntry`). A range of either
kind includes both its limits (ADR-297). The ranges of a `scale-linear` conversion must not
overlap, so every internal value has at most one result; overlapping rows are rejected
(`CSV_BAD_VALUE`). (A `text-table` may overlap, since its first matching row wins.)

A `rational` conversion gives physical = (n0 + n1·x + n2·x² + …) / (d0 + d1·x + d2·x² + …) for
the internal value x, with the coefficients in the order of the two lists. A denominator that
evaluates to zero for a decoded value makes that field's decoding fail, like any other value the
decoder cannot convert.

What a decoded field yields: `identity` gives the internal value; `linear`, `scale-linear`,
`interpolation` and `rational` give the physical value as a float. A `text-table` gives the
internal value as an integer; its texts are resolved on the server by `text_id` (design 9.2), so
the agent and a Starlark procedure never see the text, only the value it stands for.

A conversion applies to a numeric value, so the field types it may be used with are:

| Kind | Field types |
|---|---|
| `identity` | all |
| `linear`, `scale-linear`, `interpolation`, `rational` | `uint`, `int`, `float`, `bcd` |
| `text-table` | `uint`, `int`, `bcd` |

Any other combination, such as a `linear` conversion on an `ascii` field, is rejected
(`CSV_BAD_VALUE`).

### 4.6 `services.csv` (scoped)

One `Service` per row, for any service the DID and routine tables do not generate.

| Column | Type | Required | Meaning |
|---|---|---|---|
| `name` | name | yes | `Service.short_name`; must not contain `.` (reserved for generated services) |
| `sid` | integer, 0x00 to 0xBF, not 0x3F | yes | Request SID; the positive response SID is `sid` + 0x40 |
| `session` | integer, 0x00 to 0xFF | no | `required_session`, default 0 (none) |
| `security` | integer, 0x00 to 0xFF | no | `required_security_level`, default 0 (none) |
| `p2_ms` | integer, 1 to 2^32 - 1 | no | Default 50 |
| `p2_star_ms` | integer, 1 to 2^32 - 1 | no | Default 5000 |

The request is the fields of container `request:<name>` and the positive response those of
`response:<name>` (section 5). Either may have no fields.

### 4.7 `dids.csv` (scoped)

A data identifier and its data record. The record's fields are container `did:<name>`. Each row
generates up to two services:

| Service | Request | Positive response |
|---|---|---|
| `<name>.read` | SID 0x22, constant `did` (2 bytes) | SID 0x62, constant `did`, then the record |
| `<name>.write` | SID 0x2E, constant `did`, then the record | SID 0x6E, constant `did` |

`did` is a generated constant (5.3), so no field of a `did:` container may be named `did`
(`CSV_DUPLICATE`).

The record's fields are the same IR fields (same ids) in both services, with positions counted
from the byte after the DID (5.2).

| Column | Type | Required | Meaning |
|---|---|---|---|
| `name` | name | yes | |
| `did` | integer, 0x0000 to 0xFFFF | yes | |
| `read` | boolean | no | Generate `<name>.read`, default `yes` |
| `write` | boolean | no | Generate `<name>.write`, default `no` |
| `read_session`, `read_security` | integer, 0x00 to 0xFF | no | The read service's requirements, default 0 |
| `write_session`, `write_security` | integer, 0x00 to 0xFF | no | The write service's requirements, default 0 |
| `p2_ms`, `p2_star_ms` | as in 4.6 | no | Both services' timings |

A row with neither service, or a DID with no fields, is rejected.

### 4.8 `routines.csv` (scoped)

A routine identifier with the sub-functions it supports. Each sub-function generates a service
`<name>.start`, `<name>.stop` or `<name>.results` (RoutineControl sub-functions 0x01, 0x02, 0x03):
a SID 0x31 request whose generated constants are `sub` (the sub-function, 1 byte) and `rid` (the
identifier, 2 bytes), and a SID 0x71 response with the same constants. No field of a `routine:`
container may be named `sub` or `rid` (`CSV_DUPLICATE`). Their fields are containers
`routine:<name>:<start|stop|results>:<request|response>`, with positions counted from the byte
after the identifier.

| Column | Type | Required | Meaning |
|---|---|---|---|
| `name` | name | yes | |
| `rid` | integer, 0x0000 to 0xFFFF | yes | |
| `start`, `stop`, `results` | boolean | no | Defaults `yes`, `no`, `no`; at least one `yes` |
| `session`, `security` | integer, 0x00 to 0xFF | no | All of the routine's services, default 0 |
| `p2_ms`, `p2_star_ms` | as in 4.6 | no | |

### 4.9 `nrcs.csv` (scoped)

Negative responses a service can return, one `NegativeResponse` per row.

| Column | Type | Required | Meaning |
|---|---|---|---|
| `service` | service name | no | Empty: every service in the variant's document |
| `nrc` | integer, 0x01 to 0xFF | yes | |
| `text` | key | no | `message_id`, default 0 |
| `retryable` | boolean | no | Default `no` |

Two rows of one variant with the same `service` (or both empty) and the same `nrc` are rejected
(`CSV_DUPLICATE`). A row for one service replaces an empty-`service` row with the same NRC for
that service. Each
service's negatives are in row order: its own rows first, then the remaining variant-wide rows.

### 4.10 `layouts.csv`

A nested layout (ADR-242) that a `repeat`, `select` or `group` field, or a DTC, refers to. Its
fields are container `layout:<name>`.

| Column | Type | Required | Meaning |
|---|---|---|---|
| `name` | name | yes | `Layout.short_name` |
| `byte_size` | integer, 0 to 2^32 - 1 | no | `Layout.byte_size`, default 0 (the extent of the decoded fields) |

A layout is in a variant's `EcuVariant.layouts` when a `group`, `repeat` or `select` field in
that variant's document refers to it, since `layouts` holds only their targets. A layout that
only DTCs name is not emitted there; its fields are copied into the DTC definitions (4.11).

### 4.11 `dtcs.csv` (scoped)

One `DtcDefinition` per row.

| Column | Type | Required | Meaning |
|---|---|---|---|
| `code` | integer, 0x000000 to 0xFFFFFF | yes | The 3-byte DTC |
| `display` | text | yes | `display_code` |
| `text` | key | no | `message_id`, default 0 |
| `status` | layout name | yes | Its fields become `status_fields` |
| `snapshot` | layout name | no | Its fields become `snapshot_fields` |
| `extended` | layout name | no | Its fields become `extended_fields` |

Many DTCs share one status layout. The layouts' fields are copied into the definition with
their ids (a field id names the definition, not the occurrence; `ir.fbs`, `Field.id`). A layout
that a DTC names must have no shaped field, since `DtcDefinition` has no layouts of its own; its
fields are checked against its `byte_size` and the size itself is not emitted. The fields of a
status layout must lie within one byte (bits 0 to 7 of byte 0). Where snapshot and extended
fields start in a message is the declaration-part decoder's rule, not this format's: it decodes
each list from a starting byte its caller states (`diag_ir::decode::decode_dtc_fields`).

### 4.12 `flash_sessions.csv` (scoped) and `flash_segments.csv`

`flash_sessions.csv`:

| Column | Type | Required | Meaning |
|---|---|---|---|
| `id` | integer, 1 to 2^32 - 1 | yes | `FlashSession.id`, the number Starlark's `diag.flash_transfer` takes |
| `name` | name | yes | |
| `steps` | list of `enter-programming-session`, `security-access`, `erase-memory`, `request-download`, `transfer-data`, `transfer-exit`, `check-memory`, `ecu-reset` | yes | `FlashSession.steps`, in the order written; no step may appear twice |

The deprecated `FlashSession` fields are left at their schema defaults. The recovery plan and the
preconditions belong to the procedure part (ADR-245) and have no table here.

`flash_segments.csv`, appended to the named session's `segments` in row order. It has no
`variants` column: a segment belongs to exactly the variants of its session.

| Column | Type | Required | Meaning |
|---|---|---|---|
| `session` | name | yes | A flash session |
| `name` | name | yes | Unique per session |
| `start_address`, `artifact_offset` | integer, 0 to 2^64 - 1 | yes | |
| `length` | integer, 1 to 2^64 - 1 | yes | `start_address + length` and `artifact_offset + length` must not exceed 2^64 |
| `block_size` | integer, 1 to 2^32 - 1 | yes | |
| `checksum` | bytes | no | Default empty |

Every flash session needs at least one segment, and the address ranges of one session's segments
must not overlap (`CSV_DUPLICATE`).

---

## 5. Fields

### 5.1 Containers

Every field row in `fields.csv` belongs to one container, named in its `container` column:

| Container | Fields of | Position 0 is |
|---|---|---|
| `request:<service>` | a `services.csv` request | the byte after the SID |
| `response:<service>` | a `services.csv` positive response | the byte after the SID |
| `did:<did>` | a DID's data record | the byte after the DID |
| `routine:<routine>:<sub>:request` | a routine sub-function's request (`<sub>` is `start`, `stop` or `results`) | the byte after the routine identifier |
| `routine:<routine>:<sub>:response` | its positive response | the byte after the routine identifier |
| `layout:<layout>` | a nested layout | the layout's first byte |

Row order within a container is decode order (`ir.fbs`, `Field`). Rows of different containers
may be interleaved. A container whose owner is not defined is rejected, as is a routine container
for a sub-function the routine does not have.

### 5.2 Columns

| Column | Type | Required | Meaning |
|---|---|---|---|
| `container` | as in 5.1 | yes | |
| `name` | name | yes | `Field.short_name`, unique in the container; the key of the field in a Starlark response's `fields` (subset spec 3.4) |
| `byte` | integer, 0 to 2^29 - 1 | no | Byte position from the container's position 0. Empty: `AfterPrevious`. The resulting IR `bit_offset` (below) must also fit in 32 bits, so the largest `byte` depends on the container |
| `bit` | integer, 0 to 7 | no | Bit position within that byte, counted from the least significant bit; default 0 |
| `type` | `uint`, `int`, `float`, `ascii`, `bytes`, `bcd`, `utf8`, `unicode2` | yes, unless `shape` is set | `Field.kind` |
| `bits` | integer, 1 to 2^32 - 1 | see `length` | |
| `length` | `fixed`, `field`, `leading`, `terminated` | no | `Field.length_kind`, default `fixed` |
| `min_bytes`, `max_bytes` | integer, 0 to 2^32 - 1 | no | `terminated` only; defaults 0; `max_bytes` 0 means no maximum |
| `terminator` | `end`, `zero`, `ff` | no | `terminated` only: `EndOfPdu` (default), `Zero`, `HexFF` |
| `byte_order` | `big`, `little` | no | Default `big` |
| `conversion` | name | no | A `conversions.csv` name; empty: `CompuIdentity`. It converts decoded values; a procedure supplies request parameters as internal values (5.3) |
| `unit` | key | no | A `units.csv` key |
| `const` | integer or bytes | no | A constant (5.3) |
| `shape` | `group`, `repeat`, `select` | no | A nested layout (5.4) |
| `layout` | layout name | for `group` and `repeat` | `Group.layout_id`, `Repeat.layout_id` |
| `ref` | field name | for `length` `field`, a `repeat` counted by a field, and `select` | The field holding the length, the count or the key: `length_field_id`, `count_field_id` or `key_field_id` |

`bits` is the value's width for `fixed` and the length prefix's width for `leading`, and must be
empty for `field` and `terminated`. A `fixed` field of type `ascii`, `bytes`, `bcd`, `utf8` or
`unicode2` gives its width in bits too (a 17-character VIN is 136). The widths each type accepts
are (any other is rejected, `CSV_BAD_VALUE`):

| Type | `fixed` width |
|---|---|
| `uint`, `int` | 1 to 64 |
| `float` | 32 or 64, byte-aligned |
| `bcd` | a multiple of 4, up to 64 |
| `bytes`, `ascii`, `utf8` | a multiple of 8, byte-aligned |
| `unicode2` | a multiple of 16, byte-aligned |

A `leading` length prefix is 1 to 32 bits wide and may start at any bit; the value after it starts
at the next byte edge. Only `bytes`, `ascii`, `utf8` and `unicode2` take a `leading` or
`terminated` length, and a `unicode2` field's `min_bytes` and `max_bytes` are even (`ir.fbs`,
`LengthKind`; `CSV_IR_RULE`).

`ref` never names a constant (it has no IR field) or a shaped field. The field a `length` `field`
or a `repeat` counted by a field refers to is a `fixed` `uint`, `int` or `bcd` with an `identity`
or `linear` conversion; its physical value is the length in bits (`ir.fbs`, `LengthKind`) or the
item count, and a value that is negative or not a whole number fails decoding. The key of a
`select` is a scalar field of any type but `float`.

**Positions.** A row with `byte` set is `Absolute`: its IR `bit_offset` is
`(base + byte) * 8 + bit`, where `base` is the number of bytes before the container's position 0
in the message (1 for `request:` and `response:`, 3 for `did:`, 4 for `routine:`, 0 for
`layout:`). A value above 2^32 - 1 is rejected (`CSV_BAD_VALUE`). A row without `byte` is `AfterPrevious` with `bit_offset` = `bit`.

**References.** `ref` names a field earlier in the same container; inside a repeated item it
means the same item's occurrence (`ir.fbs`, `Field.length_field_id`). The IR also allows a field
of an enclosing layout, but a layout can be reached from several places, so a name in a
`layout:` row could not be tied to one field; a `ref` to a field outside the row's container is
therefore not accepted (`CSV_UNSUPPORTED`).

### 5.3 Constants

A field with `const` is a fixed value the request carries or the response must carry: a
sub-function, a record identifier echoed in the response. Constants are accepted in `request:`,
`response:`, `did:` and `routine:` containers; a constant in a `layout:` container has no prefix
to go to and is rejected (`CSV_UNSUPPORTED`). Constants must be the first fields of
their container, start at byte 0 (a first constant without `byte` starts there), follow each other without gaps, be `fixed`, byte-aligned
(`bit` 0, `bits` a multiple of 8) and of type `uint` (`const` an integer that fits `bits`) or
`bytes` (`const` exactly `bits` / 8 bytes), have no conversion, unit or shape, and keep the
default `byte_order` `big`, since constants are written big-endian; a constant with `byte_order`
`little` is rejected (`CSV_BAD_VALUE`). A constant anywhere else is rejected
(`CSV_UNSUPPORTED`).

The converter writes the constants, big-endian, into `Request.prefix` or
`PositiveResponse.prefix`, after the generated constants of a DID or routine, and does not emit
them as IR fields. Constants of a `did:` record go into the prefix of both the read response and
the write request. The positions of the other fields are unaffected, since they count from the
container's position 0 either way. The Starlark lowering builds a request's operand bytes as
`Request.prefix` followed by the encoded parameters, since `ServiceRequest` sends the SID followed
by its operand (ADR-235 item 2). `RoutineControl` already sends the sub-function and the routine
identifier before its operand, so for a routine service the lowering leaves out the three
generated prefix bytes and starts the operand with any declared constants that follow them.
Request parameters are encoded from internal values (`diag_ir::encode::encode_request`, ADR-308:
every field needs a value that fits it as given): a field's conversion applies only when a
response is decoded, so a procedure never needs the inverse of a text table or another
conversion that has none. Whether a received response's prefix matches is checked by the
declaration-part decoder, not by the host, which checks only the response SID and, for some
services, the identifier it echoes (ADR-235 item 3). The generic send-receive is the exception:
the agent's host takes only an answer that starts with the declared positive response's SID and
prefix (ADR-322 item 3).

Constants also identify the service a Starlark call means (design 8.4, subset spec 3.1). For
`diag.request(sid, params)` the frontend takes the services of the variant with that SID whose
every request constant appears in `params` with the same value and whose other request fields
are exactly the other keys of `params`, no more and no fewer, so every parameter has a value;
exactly one service must match. The generated DID services
have one constant named `did`, so `diag.request(0x22, {"did": 0xF190})` selects `vin.read` in the
example. `diag.routine(rid, sub, params)` considers only the services `routines.csv` generates,
and selects the one whose `rid` and `sub` constants have those values, with the same rule for the
other keys of `params`; `diag.request` never selects a generated routine service. The IR keeps
the constants only as prefix bytes, so the conversion hands the lowering an index of each
variant's services with their request constants by name and value; a call that selects no
service, or more than one, is rejected (`STAR_UNKNOWN_SERVICE`, ADR-313).
`diag.security_access` needs no declared service: the host builds its requests and reads the
seed from the response itself (subset spec 3.2). Under ADR-309 the seed and key requests come from the application-layer profile instead.
`diag.read_dtc(mask)` is lowered through the profile (ADR-314): its request template selects the
variant's one service whose SID and request prefix are the template's constants at their widths
(on UDS, SID 0x19 and one 1-byte constant 0x02) and whose other request fields take the
template's arguments in order, each a `fixed` `uint` of the template's width with no conversion
(on UDS one 8-bit field, so that every mask from 0x00 to 0xFF is a value of it), without relying
on the names a definition gives them. The request is built at ingestion and sent to that service.
A variant without exactly one such service rejects every `diag.read_dtc` call
(`STAR_UNKNOWN_SERVICE`), and one whose selected response does not fit the profile's result map
is `STAR_NOT_IN_PROFILE` (subset spec 3.4): on UDS, an 8-bit `uint` availability mask, then a
repeat until the end of the message of a layout of a 24-bit `uint` code and an 8-bit `uint`
status, none with a conversion. Service selection happens at ingestion, so a `params` value for a
key that names a constant must be a constant expression; a value known only at run time is
rejected (`STAR_NON_CONSTANT_ARGUMENT`). Since a call names
every request field of the service it means, two services of one variant with the same SID could
both match one call only when their request fields have the same names and no name is a constant
in both with different values; such a pair can never be told apart and is rejected. Constants
are compared by value: a `uint` constant by its integer, whatever its width, and a `bytes`
constant by its bytes; an integer never equals bytes, since a parameter is one or the other. Services
whose request field names differ, or that hold one name as constants of different values, are
always distinguishable.

### 5.4 Nested Layouts

A field with `shape` is a nested layout (ADR-242). `type`, `bits`, `length` and its columns,
`conversion` and `unit` must be empty, and the field must start at a byte edge. The `ir.fbs` rules
for each shape apply. The IR also allows some shapes in a request, but this version defines no
value a procedure could supply for them, so a shaped field in a request is rejected
(`CSV_UNSUPPORTED`); see 5.5.

- `group`: one occurrence of `layout`.
- `repeat`: `layout` repeated; its count is a row of `repeats.csv` for the field.
- `select`: one layout chosen by the key in `ref`, which must not be a `float` field. A key of
  bytes or a string is compared for equality as a big-endian integer, so it is at most 8 bytes
  (fixed at most 64 bits wide, or `terminated` with a `max_bytes` of 1 to 8; ADR-242,
  `CSV_IR_RULE`) and each of its cases is one value. Its cases are rows of `select_cases.csv`
  for the field, and `layout` stays empty.

A Starlark response's `fields` (subset spec 3.4) holds one value per name, so in this version its
keys are the scalar fields at the top of the response: those of the `response:` container, the
DID record or the routine response, not fields inside a `group`, `repeat` or `select`, which may
occur several times or not at all. Reading a nested field, or a shaped field itself, from
`fields` is rejected at ingestion; a procedure reaches that data through `raw`. The nested
layouts still serve the declaration-part decoder and DTC decoding.

`repeats.csv`, exactly one row per `repeat` field:

| Column | Type | Required | Meaning |
|---|---|---|---|
| `container`, `field` | as in `fields.csv` | yes | The `repeat` field |
| `count` | `fixed`, `field`, `marker`, `end` | yes | `RepeatCount`: `Fixed`, `FromField` (the field in `ref`), `UntilMarker`, `UntilEndOfPdu` |
| `items` | integer, 1 to 2^32 - 1 | `fixed` only | `fixed_count` |
| `item_bytes` | integer, 1 to 2^32 - 1 | `fixed` only | `item_byte_size` |
| `marker_bits` | integer, 8 to 64, a multiple of 8 | `marker` only | The marker is an unsigned big-endian integer of this width at the item's start |
| `marker_lower`, `marker_upper` | integer, 0 to 2^`marker_bits` - 1 | `marker` only | The internal marker values that end the repetition; `marker_upper` defaults to `marker_lower`. A 64-bit value is stored bit for bit in the IR's `int64` |

The converter builds the `Repeat.marker` field (`Absolute`, offset 0, `fixed`, `uint`,
`marker_bits` wide) and names it `<field>.marker`. A byte or string marker is compared as its
bytes read as a big-endian integer (`ir.fbs`, `Repeat`), so this one form covers both.

`select_cases.csv`, one row per case, in row order:

| Column | Type | Required | Meaning |
|---|---|---|---|
| `container`, `field` | as in `fields.csv` | yes | The `select` field |
| `case` | name | yes | `SelectCase.short_name`, unique per field |
| `lower` | integer | no | Inclusive, compared with the key's internal value (ADR-242); empty together with `upper` makes the row the default case. For an `int` key, -2^63 to 2^63 - 1; for a `uint`, `bcd`, `bytes`, `ascii`, `utf8` or `unicode2` key, 0 to 2^64 - 1, stored bit for bit in the IR's `int64` (as for byte and string keys, `ir.fbs` `Select`). A `select` keyed by a `float` field is rejected (`CSV_IR_RULE`; `ir.fbs` `Select`, ADR-242) |
| `upper` | integer | no | Default: the same as `lower`; same range as `lower` |
| `layout` | layout name | no | Empty: the key is valid but carries no data |

At most one default row per field (`has_default`, `default_layout_id`). Overlapping cases are
rejected (`CSV_DUPLICATE`). A limit outside the key's width (`0x100` for an 8-bit key) is
rejected, and so is a range of a non-`int` key that crosses 2^63, whose limits would not be in
order once stored in `int64`; the same applies to `marker_lower` and `marker_upper`.

### 5.5 Request Values

A procedure supplies each non-constant request field as one value, which the Starlark lowering
encodes at the field's position. The fields of a container that is sent (`request:`, a routine's
`request` and the record of a DID with a write service) are limited to what has one encoding:

| Field | Value | Encoding |
|---|---|---|
| `fixed` `uint`, `int`, `bcd` | `int` | `bits` wide, in `byte_order`; BCD as one decimal digit per 4 bits |
| `fixed` `float` | `float` | IEEE 754, `bits` 32 or 64 |
| `fixed` `bytes`, `ascii`, `utf8`, `unicode2` | `bytes` or `str` | Exactly `bits` / 8 bytes |
| `leading` | `bytes` or `str` | The length prefix, then the value |
| `terminated` with `terminator` `end` | `bytes` or `str` | The value as it is; the field must be the container's last |

`str` values are encoded as UTF-8 (`ascii` accepts only ASCII) or, for `unicode2`, as 2-byte
characters in `byte_order`. A value that does not fit its type is rejected: a character above
U+FFFF for `unicode2`, a byte above 0x7F for `ascii`, bytes that are not valid UTF-8 for `utf8`,
and a negative value for `bcd`. A `bcd` value has its most significant digit first in big-endian
order; `little` reverses the bytes. Any other field in such a container (a `field` length, a `zero` or
`ff` terminator, a shape) is rejected (`CSV_UNSUPPORTED`).

The encoded request is the concatenation of its fields in row order, so every byte of it comes
from exactly one field. In a container that is sent, every field is byte-aligned (`bit` 0) and
whole bytes wide (`bits` a multiple of 8), the constants come first (5.3), and every other field
starts exactly where the previous one ends: either without `byte`, or with the `byte` at which
the previous field ends. A field that would leave a gap or overlap the previous one is rejected
(`CSV_UNSUPPORTED`). A constant value that does not fit
its field is rejected at ingestion; a run-time value that does not fit is an error at run time.

---

## 6. Conversion Rules

- **Every IR rule is checked.** The constraints `ir.fbs` states (field order, which shapes may
  appear where, length kinds per type, marker and key types, `min_bytes` <= `max_bytes`, layout
  bounds) are checked at conversion and reported like any other table error (`CSV_IR_RULE`), and
  so are the limits ADR-242 places on nested layouts: a layout that reaches itself through
  `group`, `repeat` or `select` fields (a cycle) is rejected, once per cycle at its first layout
  in `layouts.csv`, as is a field of a request or response that nests more than 16 layouts
  deep. A field of a layout with a `byte_size`, at a set `byte` and of a fixed width, must end
  within it, and a shaped field there must not start past it; such a field of the layout of a
  `fixed` repeat must end within `item_bytes`.
  Nothing is left for the agent to discover at run time (design 8.3).
- **Determinism.** The same directory always converts to the same documents, byte for byte: ids
  follow row order (section 3) and arrays follow row order.
- **`source_digest`.** The lowercase hexadecimal SHA-256 over the table files of section 1 that
  are present and the procedure file the document carries, if any, all directly in the
  definition directory and nothing else. The names of the `.csv` and `.star` files are limited
  to the ASCII characters `A-Z`, `a-z`, `0-9`, `_`, `-` and `.`, so the name bytes are the same
  on every platform; a `.csv` or `.star` file with any other name is rejected
  (`CSV_UNKNOWN_FILE`). Other files are not read and their names are not checked. The files are taken in byte order of their names; each contributes its
  name, a zero byte, its length as a big-endian 64-bit integer and its contents. The digest covers
  the bytes as stored, so a change of line ends alone changes it, and with it the document.
- **`schema_version`** is the IR schema version the converter writes.

## 7. Error Reporting

As for Starlark (subset spec 5), all errors are listed, not only the first. Each names the file
and a code, and where they exist the row (`line`: the line on which the record starts, counting
the header as line 1) and the column (`column`: its header name). An error about a whole file
(`CSV_UNKNOWN_FILE`, a missing required file) has neither; one about the header has no `line`
beyond 1; invalid UTF-8 or a CSV syntax error gives the line where it occurs and no column. A
location that does not exist is left out of the report. Errors are listed sorted by file name,
line and column: a missing line or column sorts before any, and columns sort by their header
names. A file that breaks the record format or is not UTF-8 is not read past the error, since
its records cannot be told apart from there on; its other errors are not reported. Every row is
checked, whether or not anything refers to it; an error is reported once, at the row that has
it, and not again where another row refers to that row. The checks run in stages: first each file
on its own (the file set, the record format, the header, each cell, and names defined twice in
one table); then the rules that compare rows with each other or with other tables (references,
variant scoping, a key defined twice in one variant's document); then the rules of fields (5.2
to 5.5, the conversion and type pairs of 4.5, the services of 4.6, the status layouts of 4.11
and the layout rules of section 6), which follow the references the stage before resolved. A stage runs only when the
earlier ones find no error, since a rule that follows a row with an error can be checked neither
way; a definition with errors of an earlier stage therefore lists only those (ADR-298).

A rule in this document that names no code uses the closest one below: a missing row that a rule
requires (an empty `variants.csv`, a variant without a protocol, a DID with no service or no
fields, a routine with no sub-function, a `repeat` without its `repeats.csv` row) is
`CSV_MISSING_COLUMN`; two rows that conflict (overlapping cases, a second default case, a second
`repeats.csv` row, services no call can tell apart) are `CSV_DUPLICATE`; a rule `ir.fbs` states
(such as a `text-table` on a `float` field) is `CSV_IR_RULE`; any other value that breaks a rule
is `CSV_BAD_VALUE`:

| Code | Meaning |
|---|---|
| `CSV_SYNTAX` | The file does not follow the record format of section 2 (RFC 4180 section 2, with LF line ends allowed) or is not UTF-8, or a row has the wrong number of cells |
| `CSV_UNKNOWN_FILE` | A `.csv` file that is not a table of section 1, a file name outside the allowed characters (section 6), or a required table that is missing |
| `CSV_UNKNOWN_COLUMN` | A header name the table does not define, or a duplicate header name |
| `CSV_MISSING_COLUMN` | A required column is missing from the header, a required cell is empty, or a row that a rule requires is missing |
| `CSV_BAD_VALUE` | A value does not parse, is out of range, or is set in a column its row's kind does not use |
| `CSV_UNKNOWN_REFERENCE` | A name, key or container that is not defined |
| `CSV_DUPLICATE` | A name, id or key defined twice, or two definitions of one thing in one variant (section 3) |
| `CSV_UNSUPPORTED` | A construct this version does not accept, such as a constant that is not leading |
| `CSV_IR_RULE` | A rule of `ir.fbs` is violated (section 6) |

```json
{
  "status": "rejected",
  "errors": [
    {
      "code": "CSV_UNKNOWN_REFERENCE",
      "file": "fields.csv",
      "line": 29,
      "column": "conversion",
      "hint": "No conversion named sessionNames in conversions.csv"
    }
  ]
}
```

---

## 8. Example

The example directory defines a fictional ECU with two variants. Its DIDs, DTC read and routines
are those `sim-ecu` answers, except the supply voltage and last reset reason DIDs, which `sim-ecu`
does not have:

- `variants.csv` and `variant_patterns.csv`: the variants are told apart by the first two
  characters of the software version (DID 0xF189).
- `dids.csv`: the session (a text table), the spare part number and software version
  (`terminated` ASCII to the end of the message), the VIN (writable outside the default session, behind security access),
  the flash state (three fields at fixed positions), the power cycle count and, in the second variant only, a supply
  voltage with a linear conversion and a unit; and a last reset reason whose details are a
  `select` on the reason (`select_cases.csv`): no data, the `watchdogReset` layout, or a range of
  reasons without data; then, in both variants, a battery voltage (the supply voltage's 0.1 V
  linear conversion) and a boot time (the 10 ms linear conversion), which the end-to-end
  procedure reads through their conversions.
- `services.csv`: three session controls told apart by their `sub` constant, and
  ReadDTCInformation by status mask, whose response repeats the `dtcRecord` layout to the end of
  the message.
- `routines.csv`: eraseMemory, behind security access, and checkProgrammingDependencies, with its
  result as a text table; both start only, in the programming session.
- `dtcs.csv`: two DTCs sharing the `dtcStatus` layout, whose eight one-bit fields are the bits
  of the DTC status byte (ISO 14229-1 (2026) Annex D), under names of the example's own.
- `flash_sessions.csv` and `flash_segments.csv`: one flash session with one segment.

A row of `fields.csv` from it, the VIN in its data record:

```csv
container,name,byte,bit,type,bits,length,min_bytes,terminator,conversion,unit,const,shape,layout,ref
did:vin,vin,0,,ascii,136,,,,,,,,,
```

`vin.read` therefore sends `22 F1 90` and decodes `vin` from byte 3 of the response
(`bit_offset` 24), after the SID and the echoed DID. The positive response's `prefix` is the
echoed DID alone (`F1 90`); the SID is held separately in `PositiveResponse.sid`.
