# Vehicle Definition Tables (CSV -> Vehicle Definition)

Corresponds to design document 8.8.2. Defines the CSV tables of a vehicle definition in the
proprietary format, and how `diag-frontend` converts them. A vehicle definition states which
vehicle model a job's vehicle is and which ECUs that model carries (ADR-326). Its identification
values and matching rules feed the identification job (ADR-327), and its topology lists the ECU
entries. It refers to ECU definitions by the identifier each declares (`definition.csv`,
`csv-tables.md` 4.15, ADR-363) and never copies them.

An example is in `crates/diag-frontend/examples/example-vehicle/`. It names the example ECU of
`crates/diag-frontend/examples/example-ecu/`.

---

## 1. Definition Directory

One vehicle definition is one directory of the fixed tables below. The dialect, the value syntax,
the `note` column every table takes and the error codes are those of `csv-tables.md` (sections
2, 3 and 7); a `.csv` file of any other name is `CSV_UNKNOWN_FILE`, and other files are not read.

| File | Required | Section | Converts to |
|---|---|---|---|
| `vehicle.csv` | yes | 2.1 | `VehicleDefinition.id` |
| `identification.csv` | no | 2.2 | `VehicleDefinition.identification` |
| `vin_rules.csv` | no | 2.3 | `VehicleDefinition.vin_rules` |
| `field_rules.csv` | no | 2.4 | `VehicleDefinition.field_rules` |
| `entries.csv` | yes | 2.5 | `VehicleDefinition.entries` |
| `entry_definitions.csv` | yes | 2.5 | `Entry.definitions` |

`tables::vehicle::read_vehicle_directory` (or `read_vehicle_files`) reads the tables, and
`tables::vehicle::convert_vehicle` converts them with the ECU definitions the vehicle definition
names. Each of those is given as the documents it converted to, which carry its identifier. The
result is the in-memory `VehicleDefinition`; the signed form the server distributes is not
defined yet.

## 2. Tables

### 2.1 `vehicle.csv`

Exactly one row (`CSV_MISSING_COLUMN` with none, `CSV_DUPLICATE` for a second).

| Column | Type | Required | Meaning |
|---|---|---|---|
| `id` | name | yes | The vehicle definition's identifier, which a job and its data record (ADR-326 item 2) |

### 2.2 `identification.csv`

The values of the four identification components (ADR-326 item 2), one row per value. A
component may have several values (a definition that covers several model years) or none. A
component and value given twice is `CSV_DUPLICATE`.

| Column | Type | Required | Meaning |
|---|---|---|---|
| `component` | `oem`, `vehicle_model`, `model_year` or `vehicle_type` | yes | The component |
| `value` | text | yes | One of its values |

### 2.3 `vin_rules.csv`

A VIN matching rule: when positions `start` to `end` of the vehicle's VIN equal `expected`, the
vehicle's `component` is `value`. The VIN is read from the rule's source (ADR-327 item 5).

| Column | Type | Required | Meaning |
|---|---|---|---|
| `component` | as in 2.2 | yes | The component the rule establishes |
| `value` | text | yes | The value it establishes, one `identification.csv` gives for that component (`CSV_UNKNOWN_REFERENCE` otherwise) |
| `start`, `end` | integer, 1 to 17, `end` no less than `start` | yes | The VIN positions compared, from 1, inclusive |
| `expected` | text | yes | Exactly one character per position, each a digit or a capital letter other than I, O and Q, as a well-formed VIN has (ADR-261 item 3; `CSV_BAD_VALUE` otherwise) |
| `definition`, `variant`, `service`, `field` | source | yes | Where the VIN is read (2.6); the field is text (`ascii` or `utf8`) |

### 2.4 `field_rules.csv`

A read-field matching rule: when the source's field matches `expected`, the vehicle's
`component` is `value`. The field is compared as a variant pattern compares (`csv-tables.md`
4.2, ADR-306, ADR-327 item 4).

| Column | Type | Required | Meaning |
|---|---|---|---|
| `component`, `value` | as in 2.3 | yes | The component and the value the rule establishes |
| `definition`, `variant`, `service`, `field` | source | yes | The field compared (2.6) |
| `expected` | bytes | yes | The value it matches, written as a variant pattern's `expected` |

### 2.5 `entries.csv` and `entry_definitions.csv`

The topology (ADR-326 item 3). Each entry of `entries.csv` is a role, and its rows in
`entry_definitions.csv` are the ECU definitions that may fill it, in rising `order`: the
alternatives of an ODX ECU group. A definition needs at least one entry
(`CSV_MISSING_COLUMN`), and each entry needs at least one alternative (`CSV_MISSING_COLUMN` on
the entry).

`entries.csv`:

| Column | Type | Required | Meaning |
|---|---|---|---|
| `role` | name | yes | The role, unique in the definition (`CSV_DUPLICATE`), such as `engine` |
| `optional` | `yes` or `no` | no, default `no` | Whether the entry may be not fitted |
| `gateway` | name | no | The role of the entry the ECU is reached through, an entry of the same definition (`CSV_UNKNOWN_REFERENCE`); the relation has no cycle (`CSV_BAD_VALUE` on each entry that reaches itself) |

`entry_definitions.csv`:

| Column | Type | Required | Meaning |
|---|---|---|---|
| `role` | name | yes | An entry of `entries.csv` (`CSV_UNKNOWN_REFERENCE`) |
| `order` | integer, 1 to 65535 | yes | The alternative's place, unique per role (`CSV_DUPLICATE`) |
| `definition` | name | yes | An ECU definition's identifier, once per role (`CSV_DUPLICATE`) |

### 2.6 Sources

A rule's source names a service and a field of one variant of an ECU definition, as that
definition's tables name them, since service and field identifiers are local to a document
(ADR-318):

- `definition`: the identifier of one of the vehicle's own ECU definitions, one an entry names in
  `entry_definitions.csv` (ADR-326 item 2), and of exactly one of the ECU definitions the vehicle
  definition is converted with, all of whose documents carry it (ADR-363 item 2). Anything else
  is `CSV_UNKNOWN_REFERENCE`.
- `variant`: a variant of that definition, by its `variants.csv` name.
- `service`: a service of that variant, by its service name, generated ones included
  (`vin.read`). It must be in the read-only set of the application layer the variant's protocols
  name (ADR-327 item 1; for UDS, ReadDataByIdentifier and ReadDTCInformation), and its request
  needs no value (no field other than constants, as for an identity source, `csv-tables.md`
  4.13), or it is `CSV_BAD_VALUE`.
- `field`: a field at the top of that service's positive response (`CSV_UNKNOWN_REFERENCE`
  otherwise) that is not shaped: not a repeat, a selection or a group (`CSV_BAD_VALUE`).

An ECU definition's identifier in `entry_definitions.csv` is checked the same way.

## 3. Stages

As for ECU definitions (ADR-298), the checks run in stages, and each lists all of its errors:

1. each file on its own: the file set, the dialect, the header and every cell (sections 1 and 2);
2. the rules between the vehicle definition's own tables: one identifier, values stated once,
   rules naming stated values, roles, gateways and alternatives (2.1 to 2.5);
3. the names the definition gives the ECU definitions it is converted with (2.6).

A repeat (a table given twice, a value, role, order or alternative given again) names the line
that first gave it.

A later stage runs only when the earlier ones found no error.

## 4. Example

`examples/example-vehicle/` is a fictional model, `exampleVehicle`, with one entry, `engine`,
filled by the example ECU (`exampleEcu`):

- its OEM is established by a VIN rule on the first three positions, read with the example ECU's
  `vin.read`;
- its model year 2027 is established by the second software generation, a field rule on
  `softwareVersion.read` of `exampleEcuV2`.
