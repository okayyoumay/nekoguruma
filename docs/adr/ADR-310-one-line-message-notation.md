# ADR-310: A One-Line Message Notation for the Vehicle-Knowledge Format

**Date:** 2026-10-16
**Status:** Accepted
**Affects:** `diag-frontend` (CSV table reading), `crates/diag-frontend/docs/csv-tables.md`
sections 1, 4.10 and 5, `crates/diag-frontend/examples/example-ecu/`, ADR-295 items 1 and 5, design 8.4

## Context

The proprietary vehicle-knowledge format describes each message field as one row of
`fields.csv` (ADR-295, `csv-tables.md` 5), with nested shapes in `repeats.csv`,
`select_cases.csv` and `layouts.csv`. A row carries the field's container, byte and bit position,
type, width, length kind, conversion, unit, constant and shape in separate columns. That is exact
and maps one to one onto the IR, but a reader cannot see a message at a glance: the fields of one
request or response are spread over rows, mostly empty columns separate the values that matter,
positions are numbers the reader must add up, and a `repeat` or `select` is split across two or
three files.

There is no widely used compact text notation for diagnostic requests and responses. The industry
describes them in ODX, usually edited in GUI tools or converted from spreadsheet
specifications; open-source UDS libraries use short type codes per data identifier (Python
`struct` strings in udsoncan) or code objects; CAN signal databases (DBC) write one signal per
line with a scale, an offset and a unit; and generic binary description languages such as Kaitai
Struct describe sequences, repetitions and keyed choices but nothing diagnostic. None can be
adopted as is.

## Decision

1. **One line per message.** A definition directory describes its message shapes in one text
   file, `messages.txt`, which replaces `fields.csv`, `repeats.csv`, `select_cases.csv` and
   `layouts.csv`. Each line names one container and lists its fields left to right in the order
   they appear on the wire. The other tables (services, DIDs, routines, conversions, units, texts,
   DTCs, flash, protocols, variants) stay CSV: they are lists of independent records, which a
   table shows well.

2. **The meaning does not change.** The notation is another surface for the same model: every
   rule of `csv-tables.md` 5 (containers and their position 0, positions, constants, nested
   shapes, request values) and every IR rule applies unchanged, and a definition converts to the
   same IR whichever surface it was written in. This supersedes ADR-295 item 5 (one field table
   for every layout) and the four field tables among item 1's files. The model stays ODX's: a message is a sequence of
   parameters, each a constant, a value with a data type and a conversion, or a nested structure,
   so the ODX frontend of M7 maps onto the same terms.

3. **Lines.** The file is UTF-8. `#` starts a comment; blank lines are ignored; a line ending in
   `\` continues on the next. Each statement is a container head, a colon and the field list:

   | Head | Container (`csv-tables.md` 5.1) |
   |---|---|
   | `request <service>` | `request:<service>` |
   | `response <service>` | `response:<service>` |
   | `did <did>` | `did:<did>` |
   | `routine <routine>.<sub> request` | `routine:<routine>:<sub>:request` |
   | `routine <routine>.<sub> response` | `routine:<routine>:<sub>:response` |
   | `layout <layout> [size=<bytes>]` | `layout:<layout>` and its `byte_size` |

   A container is written once; its owner (service, DID, routine) is still defined in its CSV
   table, which keeps the variant scope.

4. **Fields.** Fields are separated by spaces; a field contains no space outside brackets. Each is
   `[@position] name:type[length][*conversion]["unit"][=constant]`:
   - position: `@3` is byte 3 and `@0.5` byte 0, bit 5 (`Absolute`); without it the field
     follows the previous one (`AfterPrevious`). The byte counts from the container's position 0,
     as the `byte` column does;
   - type: `u<bits>`, `i<bits>`, `f32`, `f64`, `bcd<bits>`, `bytes`, `ascii`, `utf8`, `ucs2`, with
     a `le` suffix for little-endian (big-endian is the default);
   - length, for the byte and string types: `[17]` a fixed size in bytes; `[len]` the length held
     by the earlier field `len` (`FromField`, its value in bits as `csv-tables.md` 5.2 defines);
     `[lead8]` a leading length prefix of that many bits; `[..end]`, `[..zero]` or `[..ff]` a
     terminated value, optionally with its byte limits as `[1..16 zero]` (a maximum left out, as in `[1.. end]`, means none);
   - `*name` a conversion of `conversions.csv`, `"key"` a unit of `units.csv` (quoted, as DBC writes units, so it never reads as a length);
   - `=value` a constant: an integer for `u` types, hex digit pairs for `bytes`.

   Nested shapes are written as types: `group(layout)`; `repeat(layout, count)` with count `end`,
   a field name (`FromField`), `4x3` (four items of three bytes), or `until u8 0xF0..0xFF` (a
   marker); and `select(key){case=value:layout ...}`, where a case value may be a range `lo..hi`,
   the layout may be left out for a key that carries no data, and `default:layout` is the default
   case.

5. **Errors.** The frontend reports a notation error with the file, line and column, under the
   staged checking of ADR-298: a line's syntax first, then the references and the rules that span
   containers.

6. **Example.** Some containers of the example ECU:

   ```
   request  defaultSession:    sub:u8=0x01
   response defaultSession:    sub:u8=0x01 p2ServerMax:u16"millisecond" \
                               p2StarServerMax:u16*tenMilliseconds"millisecond"
   request  readDtcByStatusMask:  sub:u8=0x02 statusMask:u8
   response readDtcByStatusMask:  sub:u8=0x02 availabilityMask:u8 records:repeat(dtcRecord, end)
   layout dtcRecord size=4:    dtc:u24 status:u8
   layout dtcStatus size=1:    @0.0 failedNow:u1 @0.1 failedThisCycle:u1 @0.2 pending:u1 @0.3 confirmed:u1
   did vin:                    vin:ascii[17]
   did softwareVersion:        softwareVersion:ascii[1.. end]
   did supplyVoltage:          supplyVoltage:u16*supplyVoltage"volt"
   did lastResetReason:        reason:u8 details:select(reason){powerOn=0x01 watchdog=0x02:watchdogReset software=0x03..0x0F}
   ```

## Consequences

- One message reads as one line in wire order, with constants, types, conversions and units next
  to the field they belong to; positions are written only where a field does not follow the
  previous one.
- `diag-frontend` gains a parser for the notation, with its own size and nesting limits like the
  Starlark pre-scan (design 8.4), and the example ECU and `csv-tables.md` 5 are rewritten in it.
  Until then the CSV field tables remain the format; the format is not released, so they are
  replaced rather than kept as a second surface.
- The notation is a format of this project: authors learn it, and an ODX tool cannot read it.
  Keeping ODX's model under it, and borrowing familiar spellings (short type codes, a conversion
  and a unit next to the value), limits that cost.
- The same notation can describe the services of an application-layer profile (ADR-309).
