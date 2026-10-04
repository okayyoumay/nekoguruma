# ISO 22900-2 Protocol Names, BUSTYPE, and PINTYPE to J2534-1 (DEC2004) Mapping

This document captures how `j2534-0404-service` maps protocol names from ISO 22900-2 standard specifications to SAE J2534-1 (DEC2004) protocol IDs, following the **Standard protocol naming guidelines** defined in ISO 22900-2.

## Scope

- Source references:
  - ISO 22900-2:2022-06 — Primary reference for standard protocol names, bustype/physical
    layer definitions, and naming guidelines. No converted/markdown copy is currently
    available in this workspace; only the 2009(E) edition is (see below). Values here were
    mapped against a locally-held 2022 PDF at the time this document was written, not
    reproducible from what's checked out today — re-verify against the 2009(E) text (or the
    actual 2022 PDF, if obtained) before trusting an edition-sensitive value.
  - J2534-1 v05.00 — J2534 protocol constants and IDs. No converted/markdown copy is
    currently available in this workspace; only DEC2004/v04.04 is (see below).
  - `J2534_1_200412.md` (J2534-1 DEC2004) — sibling `vehicle-comm-specs` repository, at
    `j2534-1-0404/J2534_1_200412 - Recommended Practice for Pass-Thru Vehicle Programming.md`
- This mapping enables protocol names and bustype (physical layer) names to be used in service requests alongside numeric protocol IDs.
- Both ISO 22900-2 standard names and J2534 naming conventions are supported with case-insensitive matching per the ISO standard guidelines.
- **Key Difference:** ISO 22900-2 introduces the concept of **bustype** (physical layer) as distinct from protocol (transport layer), while J2534-1 combines these. The service maps both concepts to J2534-1 protocol IDs where applicable.

## Protocol Mapping Reference

| J2534 ID | J2534 Name | ISO 22900-2 Standard Names | ISO Standard | J2534 Aliases | Hex |
|---:|---|---|---|---|---:|
| 0x01 | `J1850VPW` | J1850VPW | SAE J1850 VPW | `vpw`, `j1850_vpw`, `j1850-vpw` | `0x01` |
| 0x02 | `J1850PWM` | J1850PWM | SAE J1850 PWM | `pwm`, `j1850_pwm`, `j1850-pwm` | `0x02` |
| 0x03 | `ISO9141` | ISO9141, ISO9141-2, KWP | ISO 9141-2 (Keyword Protocol 1) | `iso_9141`, `iso-9141-2`, `kwp`, `kwp1` | `0x03` |
| 0x04 | `ISO14230` | ISO14230, ISO14230-1, KWP2000 | ISO 14230-1 (Keyword Protocol 2000) | `iso_14230`, `kwp2000`, `kwp_2000`, `kwp-2000`, `kwp2` | `0x04` |
| 0x05 | `CAN` | CAN, ISO11898, ISO11898-1 | ISO 11898-1 (Controller Area Network) | `iso11898`, `iso-11898`, `iso_11898`, `iso11898-1`, `iso-11898-1`, `iso_11898_1`, `can_iso`, `can-iso`, `iso_11898_raw` | `0x05` |
| 0x06 | `ISO15765` | ISO15765, ISO15765-2, ISO-TP | ISO 15765-2 (Diagnostics on CAN) | `iso_15765`, `iso-15765`, `iso15765-2`, `iso-15765-2`, `iso_15765_2`, `iso15765-4`, `iso-15765-4`, `iso_15765_4`, `iso-tp`, `isotp`, `iso_tp` | `0x06` |
| 0x07 | `SCI_A_ENGINE` | SCI_A_ENGINE | Serial Communications Interface A (Engine) | `sci_a`, `sciaengine`, `scia_engine` | `0x07` |
| 0x08 | `SCI_A_TRANS` | SCI_A_TRANS | Serial Communications Interface A (Transmission) | `sci_a_transmission`, `sciatrans`, `scia_trans`, `scia_transmission` | `0x08` |
| 0x09 | `SCI_B_ENGINE` | SCI_B_ENGINE | Serial Communications Interface B (Engine) | `sci_b`, `scibengine`, `scib_engine` | `0x09` |
| 0x0A | `SCI_B_TRANS` | SCI_B_TRANS | Serial Communications Interface B (Transmission) | `sci_b_transmission`, `scibtrans`, `scib_trans`, `scib_transmission` | `0x0A` |
| 0x0B | `SCI_MODE` | SCI_MODE | SCI Mode Select | `sci`, `sci_mode_select` | `0x0B` |

## ISO 22900-2:2022 Annex B.1.5 Short Names (Supported)

The implementation now accepts ISO 22900-2 Annex B.1.5 short names (application layer + `_on_` + transport layer) for all unambiguous mappings to J2534-1 (DEC2004) protocol IDs.

| ISO 22900-2 short name | Mapped J2534 ID | Mapped J2534 Name |
|---|---:|---|
| `ISO_14230_3_on_ISO_14230_2` | `0x04` | `ISO14230` |
| `ISO_14230_3_on_ISO_15765_2` | `0x06` | `ISO15765` |
| `ISO_15765_3_on_ISO_15765_2` | `0x06` | `ISO15765` |
| `ISO_14229_3_on_ISO_15765_2` | `0x06` | `ISO15765` |
| `SAE_J2190_on_ISO_14230_2` | `0x04` | `ISO14230` |
| `SAE_J2190_on_ISO_9141_2` | `0x03` | `ISO9141` |
| `SAE_J2190_on_ISO_15765_2` | `0x06` | `ISO15765` |
| `SAE_J2190_on_SAE_J1850_VPW` | `0x01` | `J1850VPW` |
| `SAE_J2190_on_SAE_J1850_PWM` | `0x02` | `J1850PWM` |
| `ISO_15031_5_on_ISO_9141_2` | `0x03` | `ISO9141` |
| `ISO_15031_5_on_SAE_J1850_VPW` | `0x01` | `J1850VPW` |
| `ISO_15031_5_on_ISO_15765_4` | `0x06` | `ISO15765` |
| `ISO_15031_5_on_SAE_J1850_PWM` | `0x02` | `J1850PWM` |
| `ISO_15031_5_on_ISO_14230_4` | `0x04` | `ISO14230` |
| `ISO_11898_RAW` | `0x05` | `CAN` |
| `ISO_11783_12_on_ISO_11783_5` | `0x05` | `CAN` |
| `ISO_14229_3_on_ISO_15765_2_with_ISO_11783_5` | `0x06` | `ISO15765` |
| `SAE_J2610_on_SAE_J2610_SCI` | `0x0B` | `SCI_MODE` |
| `ISO_14229_3` (standalone, not `_on_ISO_15765_2`-qualified) | `0x06` | `ISO15765` |
| `ISO_15765_3` (standalone, not `_on_ISO_15765_2`-qualified) | `0x06` | `ISO15765` |
| `ISO_15031_5_on_ISO_9141_2_and_ISO_14230_4` (combined K-line) | `0x03` | `ISO9141` |
| `SAE_J2190_on_ISO_9141_2_and_ISO_14230_2` (combined K-line) | `0x03` | `ISO9141` |
| `SAE_J2190_on_SAE_J1850` (bus-agnostic VPW/PWM, `J1850VPW` is only the initial connect candidate — ADR-070) | `0x01` | `J1850VPW` |
| `ISO_15031_5_on_SAE_J1850` (bus-agnostic VPW/PWM, `J1850VPW` is only the initial connect candidate — ADR-070) | `0x01` | `J1850VPW` |

Short names from Annex B.1.5 that require protocols not provided by J2534-1 (DEC2004) constants in this crate (for example SAE J1939/J1708/DoIP), or that are ambiguous many-to-one combinations, are intentionally not mapped. The last 6 rows above are the `ChannelProtocol` variants ADR-069 added to back the resource table (see below); `ISO_14229_3`/`ISO_15765_3` map identically to their `_on_ISO_15765_2`-qualified counterparts since ISO_14229_3 supersedes ISO_15765_3 and offers the same feature set.

## ISO 22900-2:2022 Annex B.2 BUSTYPE / Physical Layer Mapping

ISO 22900-2 defines **physical layer** (bustype) as a distinct layer from the transport/application layers. Per Annex B.2, physical layer names are used in MDF files as short names for BUSTYPE elements.

**Note:** the table below is `GetObjectId(OBJT_BUSTYPE, ...)`'s **legacy fallback** mapping — resolved only when a shortname matches no row's `bus_type_name` in the resource table (see "ISO 22900-2 Resource Table" below, ADR-069); a table-canonical `bus_type_name` (e.g. `"ISO_11898_2_DWCAN"`, `"SAE_J1850_VPW"`) now resolves through that table first (A1-2 fix), returning its opaque `bus_type_id` instead of the J2534 ID below. It is also the same legacy fallback `GetResourceIds`'s `bus_type_name` selector uses when a name matches no row's `bus_type_name` in the resource table. The resource table's own bus type IDs (`0x0301`-`0x0308`) are a separate, opaque namespace — not the J2534 IDs this table resolves to.

The service maps ISO 22900-2 physical layer short names to underlying J2534-1 protocol IDs where applicable:

| Physical Layer Short Name | J2534 ID | Description | Notes |
|---|---:|---|---|
| `ISO_11898_2_DWCAN` | 0x05 | DW-CAN (Differential Wiring CAN) | Standard CAN bus |
| `ISO_11898_3_DWFTCAN` | 0x05 | DW-FTCAN (Differential Wiring CAN FD) | CAN with FD extensions |
| `ISO_11992_1_DWCAN` | 0x05 | Truck CAN (Heavy-duty vehicles) | ISO 11992 over CAN |
| `SAE_J1850_VPW` | 0x01 | SAE J1850 VPW | Variable Pulse Width |
| `SAE_J1850_PWM` | 0x02 | SAE J1850 PWM | Pulse Width Modulation |
| `SAE_J2610_UART` / `SAE_J2610_SCI` | 0x0B | Chrysler SCI | Serial Communications Interface |
| `ISO_9141_2_UART` | — | K-Line (ISO 9141) | Physical layer only; requires protocol context |
| `ISO_14230_1_UART` | — | K-Line (ISO 14230) | Physical layer only; requires protocol context |

**Unmapped Physical Layers** (no J2534 DEC2004 equivalent):
- `SAE_J1939_11_DWCAN` — Heavy-duty CAN with J1939 application layer
- `SAE_J1708_UART` — Heavy-duty UART
- `SAE_J2411_SWCAN` — Single-wire CAN

### BUSTYPE Resolution Behavior

- **GetObjectId(OBJT_BUSTYPE, shortname)** resolves through the resources
  table's `bus_type_name` first (A1-2 fix, mirroring `OBJT_RESOURCE`'s
  ADR-069 behavior) — a table-canonical name (e.g. `"ISO_11898_2_DWCAN"`,
  `"SAE_J1850_VPW"`, `"ISO_9141_2_UART"`, `"SAE_J1850"`) returns that row's
  opaque `bus_type_id` (`0x0301`-`0x0308`), not a J2534 protocol ID
- Only when no table row's `bus_type_name` matches does it fall back to the
  legacy physical-layer-name-to-protocol-ID mapping below
- For K-Line physical layers (`ISO_9141_2_UART`, `ISO_14230_1_UART`), the
  legacy fallback returns `None` as a pure physical layer mapping
  - Further fallback: resolves generic protocol names for backward
    compatibility
- **Case-insensitive matching** at every step, per ISO 22900-2 guidelines
- A shortname matching neither a table `bus_type_name`, a legacy bustype
  name, nor a legacy protocol name is rejected with
  `PDU_ERR_INVALID_PARAMETERS` (ADR-078) — a bare number (e.g. `"5"`) no
  longer resolves unless it is itself a recognized name

### Example BUSTYPE Resolution

- `GetObjectId(OBJT_BUSTYPE, "ISO_11898_2_DWCAN")` → `0x0301` (table-canonical `bus_type_name`, resolved through the resource table — A1-2 fix; previously returned `0x05`/CAN via the legacy fallback below)
- `GetObjectId(OBJT_BUSTYPE, "SAE_J1850_VPW")` → `0x0306` (table-canonical `bus_type_name` — A1-2 fix; previously returned `0x01`/J1850VPW via the legacy fallback below)
- `GetObjectId(OBJT_BUSTYPE, "ISO9141")` → `0x03` (backward compatibility fallback — `"ISO9141"` is not itself a table `bus_type_name`, so this is unchanged)

## ISO 22900-2:2022 PINTYPE Handling (Annex B.2/B.5)

ISO 22900-2 defines standard pin type short names (for example `HI`, `LOW`, `K`, `L`, `TX`, `RX`, `PLUS`, `MINUS`, `SINGLE`, `IGN`, `PROGV`) used in MDF `PINTYPE` objects. J2534-1 DEC2004 does not define these short names and instead uses numeric connector pin handling (for example pin numbers in programming-voltage related calls).

To bridge this mismatch, `j2534-0404-service` resolves ISO pin type short names to stable logical IDs:

| ISO pin type short name | Logical ID |
|---|---:|
| `HI` | 2000 |
| `LOW` | 2001 |
| `K` | 2002 |
| `L` | 2003 |
| `TX` | 2004 |
| `RX` | 2005 |
| `PLUS` | 2006 |
| `MINUS` | 2007 |
| `SINGLE` | 2008 |
| `IGN`, `IGNITION_CLAMP` | 2009 |
| `PROGV` | 2010 |

Behavior:
- `GetObjectId(OBJT_PINTYPE, shortname)` resolves ISO pin type short names (case-insensitive)
- An unrecognized name — including a bare number that is not itself one of these short names — is rejected with `PDU_ERR_INVALID_PARAMETERS` (ADR-078)

## ISO 22900-2 Resource Table (`GetResourceIds` / `CreateComLogicalLink`, ADR-069)

`GetResourceIds` and `CreateComLogicalLink` resolve through a static table
(`resources.rs::RESOURCE_TABLE`) keyed by two **opaque** namespaces, disjoint
from every J2534/`ChannelProtocol` value above — MDF-style handles a caller
must discover via `GetResourceIds` and pass back to `CreateComLogicalLink`
verbatim, not values ISO 22900-2 or J2534 standardize:

- **Resource IDs:** `0x0201..=0x0225` (37 rows) — one per ISO 22900-2 resource
  (protocol × bus type × typed DLC pins).
- **Bus type IDs:** `0x0301..=0x0308` (8 rows).

Each row binds a `protocol_name` (canonical ISO 22900-2 short name), an
optional `config_name` (SCI configuration only, `SAE_J2610_SCI` rows), a
`ChannelProtocol` (identity/ComParam defaults), an optional
`hw_protocol_override` (the actual connect protocol, when it diverges from
`ChannelProtocol::j2534_protocol_id()`), and a bus type (id, name, typed DLC
pins). Several rows are **alias rows** sharing one `ChannelProtocol` under a
distinct `resource_id`/`protocol_name` (e.g. `ISO_OBD_on_K_Line` and
`ISO_15031_5_on_ISO_9141_2_and_ISO_14230_4`). `SAE_J2610_SCI` expands to
**four** resource IDs (`SCI_A_ENGINE`/`SCI_A_TRANS`/`SCI_B_ENGINE`/
`SCI_B_TRANS`), since J2534 selects that configuration via the connect-time
`ProtocolID` argument, not a post-connect ComParam.
`SAE_J2610_on_SAE_J2610_SCI` expands the same way (spec correction) but
keeps one shared `ChannelProtocol` across all four rows — `hw_protocol_override`
carries the actual per-row connect protocol instead, selected by DLC pin
wiring exactly like the bare `SAE_J2610_SCI` rows. `SCI_MODE` is therefore
no longer used as a connect protocol by any table row (only by a legacy
direct-value create with no matching resource row). The combined K-line bus
(`ISO_9141_2_UART_and_ISO_14230_1_UART`) fixes one concrete connect protocol
(`ISO9141`) with no probing — J2534-1 cannot auto-detect between two
protocol IDs at connect time, so K-line 5-baud-vs-fast-init auto-detection
remains a documented limitation. The combined J1850 bus, **renamed
`SAE_J1850`** (dropping the never-released `SAE_J1850_VPW_and_SAE_J1850_PWM`
name), is the exception: `J1850VPW` is only the initial connect candidate,
and `ConnectComLogicalLink` runs an active-probe sequence that resolves the
actual VPW/PWM flavor and reconnects on a PWM win (see ADR-070 for the probe
mechanism). The result is cached per module -- but **only when the probe was
conclusive** (verification-pass fix): an inconclusive probe (e.g. the
passive-only J2190 resource on a quiet bus) still resolves that one CLL to
the VPW fallback, but leaves the cache empty so a later CLL that can
actively probe (the OBD-capable resource) still gets the chance to. A PWM
win also merges only the *flavor-dependent* subset of the PWM ComParam
preset into the CLL's Working set (`DATA_RATE` and friends -- effectively
auto-managed on this bus), not the full preset, so a client's own
`SetComParam` staged on any other key before `ConnectComLogicalLink`
survives instead of being clobbered.

The full 37-row table, and the 8-row bus type table with typed DLC pins, are
maintained in `docs/j2534-0404-architecture.md` §6 (reproduced from
`resources.rs`) and in ADR-069, rather than a third copy here.

## Runtime Behavior in j2534-0404-service

### CreateComLogicalLink
- Accepts protocol specification via three sources:
  1. `ResourceId(u32)`
  2. `ResourceName(string)`
  3. `RscData { protocol: ProtocolId(u32) | ProtocolName(string) }`
- Resolution order (ADR-069): resource table lookup by ID/name (case-insensitive
  against `protocol_name`/`config_name`) → `map_protocol_name()` → numeric
  fallback → error. A numeric string re-enters the table lookup first, so it
  still prefers a table hit over the raw `ChannelProtocol::from_raw`
  interpretation.
- **Ambiguous table names are rejected, not silently resolved to the first
  match.** `"SAE_J2610_SCI"` matches all four SCI configuration rows
  (`0x0222`-`0x0225`), which differ only by `config_name`/`ChannelProtocol` —
  a name matching multiple rows that differ in `ChannelProtocol` **or**
  `hw_protocol_override` (spec correction: this also now covers
  `"SAE_J2610_on_SAE_J2610_SCI"`'s four rows, `0x021E`-`0x0221`, which share
  one `ChannelProtocol` but differ in hardware override) fails with
  `invalid_argument` listing the available `config_name`s/resource IDs
  (e.g. `SCI_A_ENGINE (resource_id 0x0222), ...`) instead of picking one.
  A name matching multiple rows that all share the *same* `ChannelProtocol`
  *and* `hw_protocol_override` (alias rows) is unaffected — any of them
  resolves identically. A specific `config_name` (e.g. `"SCI_B_TRANS"`) or
  `protocol_name` still resolves unambiguously, as does every other table
  name.
- **Pin-driven narrowing (spec correction, `RscData` only):** when
  `dlc_pin_data` is supplied alongside an otherwise-ambiguous
  `protocol_name`, candidates are first narrowed by typed pin (a row must
  contain the requested pin number, matching its declared type when one is
  also given); narrowing to exactly one row resolves the request regardless
  of differing `ChannelProtocol`/`hw_protocol_override`, narrowing to zero
  rows is `invalid_argument`. `ResourceName` (a bare string) cannot carry
  pins, so this narrowing is unavailable there.
- Legacy raw (`1..=0x0B`) and extended (`0x0100+`) `ChannelProtocol` values
  not present in the table remain valid inputs via `ChannelProtocol::from_raw`
  — this is a pure fallback, unaffected by the table's addition.
- When a table row matches, its own canonical `bus_type_name`/`protocol_name`
  (not whatever `RscData` fields the caller separately supplied) drive the
  initial Working ComParam defaults (`comparam_defaults::bustype_default_params`/
  `protocol_default_params`) — so a bare numeric `resource_id` now gets the
  correct defaults, which it could not before this table existed.
- Example: `ResourceId(0x0206)` resolves to the `ISO_15765_2` row (`ChannelProtocol::ISO15765`)
- Example: `ResourceName("ISO_OBD_on_K_Line")` resolves to resource `0x0213`'s `ChannelProtocol`
- Example: `ResourceName("SAE_J2610_SCI")` fails with `invalid_argument` (ambiguous across 4 configurations)
- Example: `ResourceName("SCI_B_TRANS")` resolves unambiguously to resource `0x0225`'s `ChannelProtocol`
- Example: `RscData{protocol_name: "SAE_J2610_SCI", dlc_pin_data: [(14,TX),(7,RX)]}` narrows to resource `0x0223` (`SCI_A_TRANS`)
- Example: `RscData{protocol_name: "SAE_J2610_on_SAE_J2610_SCI", dlc_pin_data: []}` fails with `invalid_argument` (ambiguous, no pins to narrow with)
- Example: `ResourceId(0x06)` (legacy, not a table resource ID) resolves via `ChannelProtocol::from_raw` to `0x06`
- Example: `ResourceName("iso15765")` resolves via `map_protocol_name()` (not a table name) to `ChannelProtocol::ISO15765` (`0x06`)
- **Verification-pass tightening: pin narrowing applies even when the name
  already resolves to exactly one row, not only when ambiguous.** Any
  supplied `dlc_pin_data` is checked against the resolved row's own
  `dlc_pins` regardless of how many candidates the bare name matched; a pin
  naming a number the row doesn't have narrows the (already unique)
  candidate to zero and is rejected, rather than being silently ignored.
  Example: `RscData{protocol_name: "ISO_15765_2", dlc_pin_data: [(7, -)]}`
  (pin `7`, any type) now fails with `invalid_argument` — resource `0x0206`'s
  only pins are `6`/`HI` and `14`/`LOW`, so pin `7` matches nothing on that
  row — where before this tightening the pin data would have been ignored
  and the link created successfully. This is intentional: pin data
  contradicting the resolved row's actual wiring is a caller error worth
  surfacing, not silently discarding.

### GetResourceStatus (ADR-069)
- `resource_id` input accepts either a table resource ID (`0x0201..=0x0225`)
  or a legacy raw/extended `ChannelProtocol` value: `id` is looked up via
  the resource table first (matching a link's `protocol` against the row's
  `ChannelProtocol`); if no row matches, falls back to
  `ChannelProtocol::from_raw(id)` (the pre-ADR-069 behavior).
- This mirrors `CreateComLogicalLink`'s own table-first/`from_raw`-fallback
  resolution, so a resource ID round-tripped from `GetResourceIds` (e.g.
  `0x0206`) correctly reports that resource's active status instead of
  always reporting "not active" (a table ID and a raw `ChannelProtocol`
  value are different numeric namespaces, so comparing one against the
  other directly never matches).
- **`resource_name` also resolves through the table first**
  (`names::find_table_rows_by_name`, matched case-insensitively against
  `protocol_name`/`config_name`), falling back to the legacy
  `map_protocol_name`/`map_object_type_name` mapping when no table row
  matches at all — so a table-only name (e.g. `"ISO_OBD_on_K_Line"`, which
  `map_protocol_name` never recognized) now resolves correctly here too,
  not just in `CreateComLogicalLink`.
- **Unlike `CreateComLogicalLink`, an ambiguous name is not rejected here.**
  `GetResourceStatus` is a filter, not a creation request: `"SAE_J2610_SCI"`
  matches a link using *any* of the four SCI configurations' `ChannelProtocol`s
  — reporting active for a link using any one of them is the
  least-surprising behavior for a status query, so no `config_name` is
  required to disambiguate here (contrast the `CreateComLogicalLink` section
  above).
- **Verification-pass amendment: an active-link match also checks
  `hw_protocol_override`, not just `ChannelProtocol`.** The four
  `SAE_J2610_on_SAE_J2610_SCI` rows (`0x021E`-`0x0221`) share one
  `ChannelProtocol` (`0x0160`) but each override a distinct hardware
  protocol ID, which becomes the connected link's own `hw_protocol_id`.
  `GetResourceStatus` requires a matched candidate's `hw_protocol_override`
  (when it has one) to equal the link's `hw_protocol_id` before counting it
  as active — a candidate with no override (e.g. the J1850 auto-detect
  rows) still matches on `ChannelProtocol` alone, unaffected. Without this
  check, connecting resource `0x0221` (`SCI_B_TRANS`) would falsely report
  `0x021E` (`SCI_A_ENGINE`) as active too, and name echoes would always pick
  `0x021E` regardless of which configuration was actually connected.
- **Response `resource_id` echo** (one entry per request, so there is
  always a single value to pick):
  - A `resource_id` query always echoes exactly the ID the caller passed.
  - A `resource_name` matching exactly one table row echoes *that row's own*
    resource ID — even if its `ChannelProtocol` is shared with an alias row
    (e.g. `"ISO_OBD_on_K_Line"` echoes `0x0213`, not its alias `0x0212`,
    which would otherwise win a plain protocol-based lookup).
  - A `resource_name` matching several table rows directly (`"SAE_J2610_SCI"`
    across its four configurations, or `"SAE_J2610_on_SAE_J2610_SCI"` across
    its four `hw_protocol_override` variants) echoes whichever matched row
    has an active link (matched by `ChannelProtocol` **and**
    `hw_protocol_override`, per the amendment above), else the first row in
    table order (`0x0222`, `SCI_A_ENGINE`, for `"SAE_J2610_SCI"`; `0x021E`
    for `"SAE_J2610_on_SAE_J2610_SCI"`).
  - A `resource_name` resolved only via the legacy `map_protocol_name` (no
    direct table-name match) echoes the resources-table ID for that
    protocol (**tie-break: the first row in table order**, for a protocol
    shared by several rows, e.g. `"ISO15765"` → `ChannelProtocol::ISO15765`
    → resource `0x0206`, the only row carrying it), or the raw
    `ChannelProtocol` value when the protocol has no table row at all (a
    legacy-only extended protocol — the pre-ADR-069 echo, unchanged).
  - A `resource_name` resolving to nothing echoes `0`.
- Example: `GetResourceStatus(resource_id=0x0206)` reports active (resource_id
  `0x0206`) when a CLL connected via that (or any table-equivalent) resource
  is online.
- Example: `GetResourceStatus(resource_name="ISO_OBD_on_K_Line")` reports
  active (resource_id `0x0213`) when a CLL was created via resource `0x0213`
  or its alias `0x0212`.
- Example: `GetResourceStatus(resource_name="ISO_15765_2")` reports active
  (resource_id `0x0206`) when a CLL connected via resource `0x0206` is online.
- Example: after `CreateComLogicalLink(ResourceId(0x0221))` connects
  (`SCI_B_TRANS`), `GetResourceStatus(resource_id=0x021E)` (`SCI_A_ENGINE`,
  same `ChannelProtocol`) reports **not** active; `GetResourceStatus(resource_id=0x0221)`
  and `GetResourceStatus(resource_name="SAE_J2610_on_SAE_J2610_SCI")` both
  report active/echo `0x0221`.

### GetConflictingResources (ADR-106)

**Superseded design note:** prior to ADR-106 this RPC scanned currently-
connected CLLs for a matching protocol, mirroring `GetResourceStatus`'s
input resolution (ADR-069). ISO 22900-2 §9.4.26 actually defines a
**static** resource-table query (pin/controller conflicts), computable
before any CLL exists and explicitly treating same-protocol channel sharing
as spec-legal, not a conflict — see ADR-106 and
`j2534-0404-service/docs/iso22900-2-conformance-audit.md` finding A1-1 for
the full defect writeup. Live-connection state (`hw_protocol_id`, active
links) is no longer consulted by this RPC at all.

- `resource_id` resolves via a **direct table lookup only**
  (`resources::find_by_resource_id`) — no `ChannelProtocol::from_raw`
  fallback. An unmapped legacy ID (no table row) yields an **empty conflict
  list, not an error**, since no pin/bus metadata exists to compute a
  static conflict from.
- `resource_name` resolves via **`names::find_table_rows_by_name` only** —
  no legacy `map_protocol_name` fallback. An unrecognized name likewise
  yields an empty conflict list, not an error. Same as `GetResourceStatus`,
  an ambiguous name is not rejected: it matches every row it resolves to
  (e.g. `"SAE_J2610_SCI"` matches all four SCI-configuration rows), and
  their conflicts are unioned and deduplicated by `resource_id`.
- **Conflict predicate** (`resources::rows_conflict`): two table rows
  conflict iff they share at least one DLC pin *number* (ignoring pin type)
  **or** share the same `bus_type_id` (used as a physical-controller proxy —
  needed for the `SAE_J2610_UART` SCI rows, whose four wirings can have
  disjoint pins yet still share one controller), and are **not** the same
  electrical configuration on one controller (identical `bus_type_id` **and**
  identical `dlc_pins`) — the latter case is multiple CLLs legally sharing
  one physical channel (e.g. the ten `ISO_11898_2_DWCAN`-family rows), which
  ISO 22900-2 explicitly allows and must not be reported as a conflict. See
  ADR-106's "Accepted residual" for this proxy's scope.
- **Response `resource_id` echo**: one entry per resource-table row that
  conflicts with the queried row(s), echoing that conflicting row's own
  `resource_id` directly (table order) — no protocol/hw-override tie-break
  needed, since the static scan always resolves to a concrete row.
- `input_module_list` must be present with at least one entry, and every
  entry's `module_handle` must equal the single supported module handle
  (`require_module_handle`); an absent list is rejected
  (`PDU_ERR_INVALID_PARAMETERS`), an empty list returns an empty conflict
  list, and an unrecognized `module_handle` is rejected
  (`PDU_ERR_INVALID_HANDLE`).
- Example: `GetConflictingResources(resource_id=0x0206)` (`ISO_15765_2`,
  pins 6/14) reports no conflict against the other nine
  `ISO_11898_2_DWCAN`-family rows (same bus type, same pins — legal
  sharing), but does conflict with any row on a different bus that also
  claims pin 6 or 14.
- Example: `GetConflictingResources(resource_id=0x0221)` (`SCI_B_TRANS`,
  the `SAE_J2610_on_SAE_J2610_SCI` hardware-override row) reports the
  K-line rows sharing its `dlc_pins` (pin 15) *plus* every other
  `SAE_J2610_UART` row (same shared SCI controller via `bus_type_id`, even
  where pins are disjoint) other than its identically-wired `SAE_J2610_SCI`
  sibling (`0x0225`, legal sharing) — unlike the pre-ADR-106 behavior, none
  of this depends on whether any CLL is connected.

### GetObjectId(OBJT_PROTOCOL, shortname)
- Resolves through the resources table's `protocol_name`/`config_name`
  first (A1-2 fix, ADR-069) via `find_protocol_for_name` — a table-canonical
  name returns that row's `ChannelProtocol` value, not a raw legacy alias.
  Unlike `OBJT_RESOURCE`'s `find_table_row_by_name` (which must pick one
  specific hardware configuration and so treats differing
  `hw_protocol_override`s as ambiguous), `find_protocol_for_name` only cares
  about protocol identity: rows sharing one `ChannelProtocol` resolve
  unambiguously regardless of `hw_protocol_override` (e.g. the four
  `SAE_J2610_on_SAE_J2610_SCI` configurations), while rows spanning
  different `ChannelProtocol`s (e.g. `SAE_J2610_SCI`'s four distinct SCI
  configs) are still rejected as ambiguous
- Only when no table row matches does it fall back to the legacy
  shortname-to-J2534-ID mapping, which accepts both ISO and J2534 naming
  conventions
- Returns numeric protocol ID
- Example: `GetObjectId(OBJT_PROTOCOL, "kwp2000")` returns `0x04` (legacy alias fallback; `"kwp2000"` is not itself a table `protocol_name`)
- Example: `GetObjectId(OBJT_PROTOCOL, "CAN")` returns `0x05` (case-insensitive; legacy alias fallback)
- Example: `GetObjectId(OBJT_PROTOCOL, "SAE_J1850_VPW")` returns `0x01` (table-canonical `protocol_name`, resolved through the resource table — A1-2 fix; previously failed outright since `"SAE_J1850_VPW"` has no legacy `map_protocol_name` alias)
- Example: `GetObjectId(OBJT_PROTOCOL, "SAE_J2610_on_SAE_J2610_SCI")` returns `0x0160` — its 4 table rows share one `ChannelProtocol` and differ only in `hw_protocol_override`, so `find_protocol_for_name` resolves it unambiguously (unlike `GetObjectId(OBJT_RESOURCE, ...)`, which rejects the same name as ambiguous since it must pick one specific hardware configuration)

### GetObjectId(OBJT_BUSTYPE, shortname)
- Resolves through the resources table's `bus_type_name` first (A1-2 fix,
  mirroring `OBJT_RESOURCE`'s ADR-069 behavior) — a table-canonical name
  returns that row's opaque `bus_type_id`, not a J2534 protocol ID
- Only when no table row's `bus_type_name` matches does it fall back to
  resolving ISO 22900-2 Annex B.2 physical layer names to J2534 protocol IDs
- Falls back further to protocol name mapping for backward compatibility
- A shortname matching neither a table `bus_type_name`, a legacy bustype
  name, nor a legacy protocol name is rejected with
  `PDU_ERR_INVALID_PARAMETERS` (ADR-078) — a bare number is no longer
  accepted unless it is itself a recognized name
- Example: `GetObjectId(OBJT_BUSTYPE, "ISO_11898_2_DWCAN")` returns `0x0301` (table-canonical `bus_type_name` — A1-2 fix; previously returned `0x05`/CAN via the legacy fallback)
- Example: `GetObjectId(OBJT_BUSTYPE, "SAE_J1850_VPW")` returns `0x0306` (table-canonical `bus_type_name` — A1-2 fix; previously returned `0x01`/J1850VPW via the legacy fallback)
- Example: `GetObjectId(OBJT_BUSTYPE, "ISO9141")` returns `0x03` (backward compatibility fallback; `"ISO9141"` is not itself a table `bus_type_name`, so this is unchanged)

### GetObjectId(OBJT_PINTYPE, shortname)
- Resolves ISO 22900-2 pin type short names to stable logical IDs
- Accepts names case-insensitively (`HI`, `hi`, `Hi` all resolve equally)
- An unrecognized name — including a bare number that is not itself one of these short names — is rejected with `PDU_ERR_INVALID_PARAMETERS` (ADR-078)
- Example: `GetObjectId(OBJT_PINTYPE, "HI")` returns `2000`
- Example: `GetObjectId(OBJT_PINTYPE, "ignition_clamp")` returns `2009`
- Example: `GetObjectId(OBJT_PINTYPE, "2010")` fails with `PDU_ERR_INVALID_PARAMETERS` (ADR-078; `"2010"` is not itself a recognized pin type name)

### GetObjectId(OBJT_RESOURCE, shortname) (ADR-069)
- Resolves through the resources table first (`names::find_table_row_by_name`),
  the same resolution `CreateComLogicalLink`'s resource-name field and
  `GetResourceStatus`'s response echo use — **not** the legacy
  `map_protocol_name` mapping alone (which is the pre-ADR-069 behavior this
  RPC used to have, and which returned `0` for a table-only name and a raw
  J2534/`ChannelProtocol` value instead of a resource ID for every other
  name):
  - A name matching exactly one table row (`protocol_name` or
    `config_name`, case-insensitive) returns that row's resource ID —
    including a table-only name with no legacy alias at all (e.g.
    `"ISO_OBD_on_K_Line"` → `0x0213`), and a name that also happens to be a
    recognized legacy alias (e.g. `"ISO_15765_2"` → `0x0206`, not the raw
    J2534 protocol value `6`).
  - A name matching several table rows that differ in `ChannelProtocol`
    **or** `hw_protocol_override` (`"SAE_J2610_SCI"`, across its four
    configurations, or `"SAE_J2610_on_SAE_J2610_SCI"`, across its four
    hardware overrides — spec correction) is rejected with
    `invalid_argument` naming the available `config_name`s/resource IDs —
    the same rejection `CreateComLogicalLink` uses for the identical
    ambiguity (`GetObjectId` has no pin data to narrow with, unlike
    `CreateComLogicalLink`'s `RscData` path). A name matching several rows
    that all share **one** `ChannelProtocol` *and* `hw_protocol_override`
    (true alias rows) is unaffected and resolves like a unique match.
  - A name with no direct table match, but resolvable via the legacy
    `map_protocol_name` (e.g. `"ISO15765"`), returns the resources-table ID
    for that protocol (`0x0206`) rather than the raw protocol value, unless
    no table row carries that protocol at all (a legacy-only extended
    protocol), in which case the raw value is returned unchanged.
  - An unrecognized name is rejected with `PDU_ERR_INVALID_PARAMETERS`
    (**ADR-078**, superseding the numeric-parse-then-`0` fallback this ADR
    originally documented).
- Example: `GetObjectId(OBJT_RESOURCE, "ISO_OBD_on_K_Line")` returns `0x0213`
- Example: `GetObjectId(OBJT_RESOURCE, "ISO_15765_2")` returns `0x0206`
- Example: `GetObjectId(OBJT_RESOURCE, "ISO15765")` (legacy alias) returns `0x0206`
- Example: `GetObjectId(OBJT_RESOURCE, "SCI_B_TRANS")` returns `0x0225`
- Example: `GetObjectId(OBJT_RESOURCE, "SAE_J2610_SCI")` fails with `invalid_argument` (ambiguous across 4 configurations)
- Example: `GetObjectId(OBJT_RESOURCE, "SAE_J2610_on_SAE_J2610_SCI")` fails with `invalid_argument` (ambiguous across 4 hardware overrides, no pin data to narrow with)
- Example: `GetObjectId(OBJT_RESOURCE, "not_a_resource")` fails with `PDU_ERR_INVALID_PARAMETERS` (ADR-078)

### GetResourceIds (ISO 22900-2 PDUGetResourceIds alignment, ADR-069)
- `GetResourceIdsRequest.resource_data` is required and used as selector input
- Selector fields supported:
  - `protocol` (name/id)
  - `bus_type` (name/id)
  - `dlc_pin_data[].dlc_pin_type` (name/id)
  - `dlc_pin_data[].dlc_pin_number` (numeric)
- Table-driven AND-filtering: every supplied selector independently narrows
  the candidate resource-table rows; the result is their intersection, in
  table order. **The *returned* `resource_id_array` values changed** (they
  are now opaque `0x0201..=0x0225` table IDs, not raw J2534 protocol IDs, per
  ADR-069) — but `ResourceData` has no `resource_id` *selector* field; the
  input selectors are `protocol` and `bus_type` (below).
  - `protocol` (id/name): **`protocol_id` (the numeric form) takes a
    `ChannelProtocol`/J2534 protocol value — 1-0x0B native, or 0x0100+
    extended — never an opaque resource ID.** It is matched against a row's
    `ChannelProtocol` value (id) or `protocol_name`/`config_name` (name)
    first; if no row matches at all, falls back to the legacy interpretation
    (a raw/extended `ChannelProtocol`'s `j2534_protocol_id()`, or
    `map_protocol_name()` then that same fallback for names) — so a legacy
    caller (e.g. `protocol_id = 6`, `protocol_name = "ISO15765"`) still
    resolves, now to whichever table row(s) share that underlying J2534
    protocol. Passing an opaque resource ID (e.g. `0x0206`) as `protocol_id`
    matches nothing — the two numeric namespaces are disjoint by
    construction; there is no "look up this resource ID" selector on
    `GetResourceIds` (that is what `CreateComLogicalLink`'s `resource_id`
    field is for).
  - `bus_type` (id/name): matched against a row's `bus_type_id`/`bus_type_name`
    first; falls back to `map_bustype_name()`'s legacy J2534-ID interpretation
    if no row matches by name/id at all -- **the legacy fallback compares a
    row's *fixed effective* connect protocol (`hw_protocol_override` when the
    row has one, else `ChannelProtocol::j2534_protocol_id()`), and never
    matches a row on the `SAE_J1850` auto-detect bus at all (verification-pass
    fix, P2):** those rows (`0x021A`/`0x021C`/`0x021D`) have no single fixed
    connect protocol -- `j2534_protocol_id()` for them is only the VPW
    *initial probe candidate* (ADR-070) -- so a legacy alias like
    `"j1850_vpw"` must never resolve to them; they remain reachable only via
    their own bus name (`"SAE_J1850"`), `bus_type_id` (`0x0307`), a
    `protocol` selector, or a resource ID. This also fixed a related
    inconsistency for the `SAE_J2610_on_SAE_J2610_SCI` rows
    (`0x021E`-`0x0221`): comparing their shared `ChannelProtocol`'s own
    `j2534_protocol_id()` (`SCI_MODE`, no longer used as a connect protocol by
    any row) meant a legacy native SCI hardware id (e.g. `SCI_A_ENGINE`)
    matched only the bare `SAE_J2610_SCI` row identifying as that id
    (`0x0222`), not the `_on_` row that actually connects with it via
    `hw_protocol_override` (`0x021E`) -- both now match
  - **pin filtering is row-based (spec correction), not the previous
    protocol-family/pin-type combo check:** each requested `PinData` narrows
    candidates directly against every remaining row's own typed `dlc_pins`
    (`(pin_number, pin_type_id)` pairs) --
    - `dlc_pin_number > 0`: the row must contain that pin number; if
      `dlc_pin_type` is also given, it must equal *that specific pin's*
      declared type (not just be typewise compatible with the row's
      protocol family)
    - `dlc_pin_number == 0` with a `dlc_pin_type`: matches any row having a
      pin of that type at all, regardless of number
    - neither present: no constraint from this `PinData` entry
  - an unrecognized `dlc_pin_type` name is `invalid_argument`; a recognized
    type/number combination that matches no row simply narrows to an empty
    result, not an error -- this replaces the old separate "contradictory
    pair → empty result" pre-check, which is now just the general case
  - the previous "protocol and bus type both given but conflict → empty
    result" special case no longer exists as a distinct rule — it is
    subsumed by ordinary AND-filtering (a genuinely contradictory combination
    intersects to the empty set on its own)
- Typed DLC pin conventions (per bus type/configuration, from the resource
  table; pin type IDs are the `map_pintype_name` 2000-range logical IDs:
  `HI`=2000, `LOW`=2001, `K`=2002, `L`=2003, `TX`=2004, `RX`=2005,
  `PLUS`=2006, `MINUS`=2007):
  - `ISO_11898_2_DWCAN`: `(6,HI)`, `(14,LOW)`
  - `ISO_14230_1_UART` / `ISO_9141_2_UART` / `ISO_9141_2_UART_and_ISO_14230_1_UART`: `(7,K)`, `(15,L)`
  - `SAE_J1850_VPW`: `(2,PLUS)`
  - `SAE_J1850_PWM` / `SAE_J1850` (renamed from `SAE_J1850_VPW_and_SAE_J1850_PWM`, ADR-070): `(2,PLUS)`, `(10,MINUS)`
  - `SAE_J2610_UART` (no single bus-wide pin set -- wiring selects the
    configuration): `SCI_A_ENGINE` `(6,TX)`,`(7,RX)`; `SCI_A_TRANS`
    `(14,TX)`,`(7,RX)`; `SCI_B_ENGINE` `(12,TX)`,`(7,RX)`; `SCI_B_TRANS`
    `(9,TX)`,`(15,RX)`
  - Example: `by_pin(6, "HI")` matches only the CAN rows; `by_pin(6, "TX")`
    matches only the two `SCI_A_ENGINE` rows (`0x021E`, `0x0222`) -- pin 6
    is shared across buses but typed differently on each
- Handle behavior:
  - accepts module handle `1` (default) and `0xFFFFFFFF` (`PDU_HANDLE_UNDEF` semantics)
- Invalid selector names return `invalid_argument` (input validation per ISO behavior)

## ISO 22900-2 Standard Protocol Naming Guidelines

Per ISO 22900-2 specification, the following naming conventions apply. The
canonical name, ISO standard, J2534 ID, and full set of recognized aliases
for each protocol are listed once in the
[Protocol Mapping Reference](#protocol-mapping-reference) table above; this
section states the general matching rules only, not a repeated per-protocol
listing (the earlier version of this document repeated the alias lists in
three places, which had drifted out of sync with each other and with the
actual implementation in `names.rs`).

- **Case Sensitivity**: **Case-insensitive** matching for all protocol names (per ISO 22900-2 guidelines)
- **Format Variants**: Full standard identifiers with ISO parts are accepted
  - Hyphenated: `ISO11898-1`, `ISO15765-2`, `ISO9141-2`, `ISO14230-1`
  - Underscored: `ISO_11898_1`, `ISO_15765_2`, `ISO_9141_2`, `ISO_14230_1`
  - Concatenated: `ISO11898`, `ISO15765`, `ISO9141`, `ISO14230`
  - ISO15765 additionally accepts `-4`/`_4` part-number variants (`ISO15765-4`, `ISO_15765_4`) alongside the `-2`/`_2` variants, since D-PDU clients may reference either the transport-protocol part (ISO 15765-2) or the OBD-II-over-CAN part (ISO 15765-4)
- **Canonical Priority**: When no explicit variant is provided, canonical form (e.g., `CAN`, `ISO15765`) takes precedence
- **Hex Protocol IDs**: `0x01`–`0x0B` for direct J2534 protocol ID specification

## Numeric Fallback

Protocol specifications that are not recognized as valid names fall back to numeric parsing:

- `"5"` or `"0x05"` → `0x05` (CAN)
- `"6"` or `"0x06"` → `0x06` (ISO15765)
- `"invalid"` → Error: unrecognized protocol

## Future Extensions

This mapping is designed to be extended with additional protocols as needed:

- Additional J2534-1 protocol IDs (0x0C, 0x0D, etc.) if supported by underlying library
- Additional naming conventions from evolving ISO 22900 standards
- Regional or vendor-specific protocol aliases

Additions should maintain:
- Case-insensitive matching consistency
- Clear documentation of semantic equivalence
- No breaking changes to existing protocol resolution paths
