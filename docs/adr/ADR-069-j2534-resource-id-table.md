# ADR-069: ISO 22900-2 Resource Table for GetResourceIds / CreateComLogicalLink

**Date:** 2026-07-08
**Status:** Accepted (`GetConflictingResources`-specific portions superseded by ADR-106 — see the note near the end of this document; extended, not superseded, by ADR-156's `_PS`/`_CHx` resolution path for J2534-2 Pin Selection/Additional Channels, which runs alongside the static table lookup this ADR defines)
**Affects:**
- `j2534-0404-service/src/service/resources.rs` (new)
- `j2534-0404-service/src/service/protocol.rs`
- `j2534-0404-service/src/service/names.rs`
- `j2534-0404-service/src/service/rpc_link.rs`
- `j2534-0404-service/src/service/comparam_defaults.rs`
- `j2534-0404-service/src/service.rs`
- `j2534-0404-service/tests/grpc_mock/resources.rs` (new)
- `j2534-0404-service/tests/grpc_mock/lifecycle.rs`

## Context

`GetResourceIds` and `CreateComLogicalLink` previously exposed raw J2534
protocol IDs (`1..=0x0B`, the same values `ChannelProtocol`'s native range
uses, ADR-023) as "resource IDs": `GetResourceIds` returned whichever native
IDs matched the caller's filter, and `CreateComLogicalLink(resource_id)`
interpreted that number directly as a `ChannelProtocol` raw value.

This flat `resource_id == protocol_id` scheme cannot represent ISO 22900-2's
actual resource concept — a resource is a **(protocol × bus type × DLC pin
list)** tuple, not a bare protocol ID — and breaks down in three concrete
ways this service now needs to support:

1. **Combined bus types.** ISO 22900-2 defines bus types such as
   `ISO_9141_2_UART_and_ISO_14230_1_UART` (a K-line connector that could carry
   either ISO 9141-2 or ISO 14230-1 traffic) and
   `SAE_J1850_VPW_and_SAE_J1850_PWM`. J2534-1's `PassThruConnect` has no
   "either" protocol ID — a channel is opened against exactly one concrete
   protocol ID — so a combined-bus-type resource still needs a single,
   specific `ChannelProtocol` chosen at `CreateComLogicalLink` time. A flat
   `resource_id == protocol_id` scheme has no field to carry "and this is the
   bus type it lives on" alongside that choice.
2. **Alias protocols.** ISO 22900-2 gives distinct short names to resources
   that are behaviorally identical at the J2534 level — e.g.
   `ISO_OBD_on_K_Line` and `ISO_15031_5_on_ISO_9141_2_and_ISO_14230_4` both
   describe the same combined-K-line OBD channel. These need distinct,
   independently filterable resource IDs even though they resolve to the same
   `ChannelProtocol`, which a scheme keyed 1:1 on protocol ID cannot express.
3. **Per-configuration SCI resources.** ISO 22900-2's `SAE_J2610_SCI` bus type
   carries a `configuration` attribute (`SCI_A_ENGINE`/`SCI_A_TRANS`/
   `SCI_B_ENGINE`/`SCI_B_TRANS`) that is not deferrable to a post-connect
   ComParam: J2534 selects SCI-A vs. SCI-B and Engine vs. Transmission via the
   `ProtocolID` argument of `PassThruConnect` itself (four distinct native IDs,
   `0x07`-`0x0A`). One ISO 22900-2 resource concept therefore has to expand
   into four resource IDs, one per configuration, each carrying its own
   connect protocol.

## Decision

Add a static resource table (`resources.rs`) in a dedicated **opaque**
numeric namespace, disjoint from every `ChannelProtocol` value:

- **Resource IDs:** `0x0201..=0x0225` (37 rows; spec correction expanded the
  `SAE_J2610_on_SAE_J2610_SCI` resource from one row to four, adding 3).
- **Bus type IDs:** `0x0301..=0x0308` (8 rows).

These are MDF-style opaque handles — like the arbitrary `id` attribute a real
MDF file's `<RESOURCE>` element uses purely for cross-referencing within that
one document — not values ISO 22900-2 or J2534 standardize. Callers must
discover them via `GetResourceIds` and pass them back to
`CreateComLogicalLink` verbatim; the numeric value itself carries no meaning
outside a table lookup. Legacy raw (`1..=0x0B`) and extended (`0x0100+`)
`ChannelProtocol` values remain valid `CreateComLogicalLink` inputs as a
fallback for callers built against the pre-table scheme (see "Legacy
fallback" below) — this ADR is purely additive to `CreateComLogicalLink`'s
accepted inputs.

Each resource row binds:

| Field | Meaning |
|---|---|
| `resource_id` | The opaque `0x0200`-namespace handle. |
| `protocol_name` | Canonical ISO 22900-2 short name (case preserved as the standard writes it, e.g. `on`/`and` lowercase; all lookups against it are case-insensitive). |
| `config_name` | `Some("SCI_A_ENGINE"\|"SCI_A_TRANS"\|"SCI_B_ENGINE"\|"SCI_B_TRANS")` for the four `SAE_J2610_SCI` rows, `None` everywhere else. |
| `protocol` | The `ChannelProtocol` used for identity/ComParam defaults (ADR-023's abstraction — `j2534_protocol_id()` gives the *default* `PassThruConnect` argument, overridden per `hw_protocol_override` below when set). |
| `hw_protocol_override` | Spec correction (pin-typing amendment): `Some(native J2534 protocol id)` when this row's actual connect protocol diverges from `protocol.j2534_protocol_id()` — used only by the four `SAE_J2610_on_SAE_J2610_SCI` rows (see below); `None` (the common case) means connect with `protocol.j2534_protocol_id()` as before. |
| `bus_type_id` / `bus_type_name` | The ISO 22900-2 BUSTYPE this resource lives on, from the 8-row bus type table below. |
| `dlc_pins` | Spec correction: **typed** `(pin_number, pin_type_id)` pairs (pin type IDs are the existing `map_pintype_name` 2000-range logical IDs — `HI`=2000, `LOW`=2001, `K`=2002, `L`=2003, `TX`=2004, `RX`=2005, `PLUS`=2006, `MINUS`=2007). Conventions: CAN/ISO15765 `[(6,HI),(14,LOW)]`, K-line `[(7,K),(15,L)]`, PWM/SAE_J1850 `[(2,PLUS),(10,MINUS)]`, VPW `[(2,PLUS)]`; SAE_J2610_UART has no bus-wide pin set — each SCI configuration has its own pins (below), since the wiring itself selects the configuration. |

**Alias rows** intentionally share one `ChannelProtocol` under a distinct
`resource_id`/`protocol_name` — e.g. `0x0209 ISO_OBD_on_ISO_15765_4` and
`0x0205 ISO_15031_5_on_ISO_15765_4` both carry
`ChannelProtocol::ISO_15031_5_ON_ISO_15765_4`; `0x0213 ISO_OBD_on_K_Line` and
`0x0212 ISO_15031_5_on_ISO_9141_2_and_ISO_14230_4` both carry
`ChannelProtocol::ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4`; `0x021D
ISO_OBD_on_SAE_J1850` and `0x021C ISO_15031_5_on_SAE_J1850` both carry
`ChannelProtocol::ISO_15031_5_ON_SAE_J1850`. This is intentional duplication,
matching distinct ISO 22900-2 short names that happen to describe the same
channel — not a modeling bug.

**`SAE_J2610_SCI` expands to four resource IDs** (renumbered `0x0222`-`0x0225`
by the pin-typing amendment, formerly `0x021F`-`0x0222`), one per
`config_name`, each bound to the matching native `ChannelProtocol`
(`SCI_A_ENGINE`/`SCI_A_TRANS`/`SCI_B_ENGINE`/`SCI_B_TRANS` = `0x07`-`0x0A`) —
the only way to represent "configuration selected at connect time, not by
ComParam" in this table.

**`SAE_J2610_on_SAE_J2610_SCI` also expands to four resource IDs**
(`0x021E`-`0x0221`, pin-typing amendment): the DLC pin wiring that names an
ISO 22900-2 `SAE_J2610_UART` configuration (per `SAE_J2610_SCI`'s spec pins
below) applies here too, but all four rows share one identity-level
`ChannelProtocol` (`SAE_J2610_ON_SAE_J2610_SCI`, `0x0160`) rather than the
per-configuration native ones `SAE_J2610_SCI` uses — ComParam defaults and
the `TX_FLAG_SCI_MODE` quirk are identical regardless of which SCI
configuration is actually wired. `hw_protocol_override` (`SCI_A_ENGINE`/
`_A_TRANS`/`_B_ENGINE`/`_B_TRANS`) is what actually reaches
`PassThruConnect` for these four rows; `SCI_MODE` (`0x0B`) is therefore no
longer used as a connect protocol by any table row — only by a legacy
direct-`ChannelProtocol`-value create with no matching resource row (see
`protocol.rs`'s updated `SAE_J2610_ON_SAE_J2610_SCI` doc comment).

**Pin-driven configuration selection.** Because the four
`SAE_J2610_on_SAE_J2610_SCI` rows share one `ChannelProtocol`, the prior
ambiguity rule ("all matches share one `ChannelProtocol`" ⇒ pick any) would
have silently resolved an unqualified `"SAE_J2610_on_SAE_J2610_SCI"` to
`SCI_A_ENGINE` (the first in table order) — wrong whenever the caller meant
a different wiring. The rule is amended: rows are now ambiguous when they
differ in `ChannelProtocol` **or** `hw_protocol_override`. When
`CreateComLogicalLink`'s `RscData` supplies `dlc_pin_data` alongside an
otherwise-ambiguous `protocol_name`/`resource_name`, the candidate rows are
first narrowed by typed pin — a row matches a requested `PinData` when it
contains that `dlc_pin_number` and (if a pin type is also given) that pin's
type equals the row's declared type for it; a pin type with no usable
number matches any row having a pin of that type at all. Narrowing to
exactly one row resolves the request regardless of remaining
`ChannelProtocol`/`hw_protocol_override` differences; narrowing to zero rows
is `invalid_argument` ("pin set matches no configuration"); no pin data (or
pins that don't narrow to one row) falls through to the original
ambiguity check. `GetResourceIds`'s pin selectors use the same row-based
matching, replacing the old two-part `protocol_supports_pintype`/
`pin_type_supports_dlc_pin` combo validation entirely.
`ResourceName` (a bare string) cannot carry pin data, so only the `RscData`
path gains this narrowing. **This pin narrowing is unconditional whenever
`dlc_pin_data` is non-empty — not only when the name is otherwise
ambiguous.** A name that already resolves to exactly one row (e.g.
`"ISO_15765_2"` → `0x0206`, pins `6`/`HI` and `14`/`LOW`) still has any
supplied `dlc_pin_data` checked against that row: a pin naming a number the
row doesn't have (e.g. pin `7`) narrows the single candidate to zero and is
rejected the same `invalid_argument` way, rather than being silently
ignored as it would be pre-table (see Consequences).

**Combined bus types fix one connect protocol, chosen at table-authoring
time, not per-request** — with one exception. The combined K-line bus
(`ISO_9141_2_UART_and_ISO_14230_1_UART`, bus `0x0304`) resources connect
`ISO9141` (`0x03`) unconditionally; K-line fast-init-vs-5-baud selection
remains a documented limitation, per ADR-017. The combined J1850 bus,
**renamed `SAE_J1850`** (bus `0x0307`, spec correction; the original name
`SAE_J1850_VPW_and_SAE_J1850_PWM` was never released and has no legacy
alias), is the exception: `J1850VPW` (`0x01`) is only the *initial
candidate* `ConnectComLogicalLink` connects with — an active-probe sequence
then determines whether the bus is actually wired for VPW or PWM and
reconnects if it finds PWM, overriding `LogicalLinkState::hw_protocol_id`
accordingly. **ADR-070 supersedes this ADR's original limitation text**,
which asserted VPW/PWM auto-detection was not implementable over J2534-1;
see ADR-070 for the probe mechanism, the ADR-017 principle it follows for
the still-unimplemented K-line fast-init-vs-5-baud case, and why resource
identity here is unaffected (detection is a hardware-layer decision, not a
naming one).

**6 new `ChannelProtocol` variants** were added to represent rows with no
prior extended-protocol equivalent (values chosen as the next free slot in
their existing value block, per ADR-023's encoding):

| Variant | Value | Underlying J2534 ID |
|---|---|---|
| `ISO_14229_3` | `0x0106` | ISO15765 (`0x06`) |
| `ISO_15765_3` | `0x0107` | ISO15765 (`0x06`) |
| `ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4` | `0x0122` | ISO9141 (`0x03`) |
| `SAE_J2190_ON_ISO_9141_2_AND_ISO_14230_2` | `0x0123` | ISO9141 (`0x03`) |
| `SAE_J2190_ON_SAE_J1850` | `0x0152` | J1850VPW (`0x01`), initial candidate -- see ADR-070 |
| `ISO_15031_5_ON_SAE_J1850` | `0x0153` | J1850VPW (`0x01`), initial candidate -- see ADR-070 |

`ISO_14229_3`/`ISO_15765_3` are used standalone (not qualified
`_ON_ISO_15765_2`) because ISO_14229_3 supersedes ISO_15765_3 while
providing the same feature set — both therefore map to the same
underlying ISO15765 channel as their `_ON_ISO_15765_2`-qualified
counterparts, per ADR-023.

**`GetResourceIds` does table-driven AND-filtering:** each supplied selector
(`protocol` id/name, `bus_type` id/name, pin type, pin number) independently
narrows the candidate row set; the final result is their intersection, in
table order. Every id/name selector tries a table match first
(`row.protocol.value()`/`row.protocol_name`/`row.config_name` for protocol,
`row.bus_type_id`/`row.bus_type_name` for bus type) and only falls back to
the pre-table interpretation (a raw/legacy `ChannelProtocol` value via
`j2534_protocol_id()`, or `map_bustype_name`'s J2534 ID) when nothing in the
table matches by name/value at all — so a caller still filtering by a bare
legacy protocol ID or name (e.g. `protocol_id = 6`, `protocol_name =
"ISO15765"`) keeps working, now resolving to the table row(s) that share that
underlying J2534 protocol. The previous "protocol and bus type both given but
disagree → empty result" special case is subsumed by ordinary AND-filtering
across rows (a genuinely contradictory combination now simply intersects to
the empty set on its own).

**The `bus_type` legacy hw-id fallback compares a row's *fixed effective*
connect protocol, not its `ChannelProtocol`'s hardware id directly, and
never matches the `SAE_J1850` auto-detect bus at all (verification-pass
fix, P2).** A `bus_type_id`/`bus_type_name` that matches no table row
directly falls back to a J2534 hardware protocol ID (via a numeric parse or
`map_bustype_name`) and previously matched every row whose
`protocol.j2534_protocol_id()` equalled it. That is wrong for two families
of rows introduced since this fallback was written: the `SAE_J1850`
auto-detect rows (`0x021A`/`0x021C`/`0x021D`, ADR-070) have no *fixed*
connect protocol at all — `j2534_protocol_id()` for their `ChannelProtocol`s
is only the VPW *initial probe candidate* — so a legacy alias like
`"j1850_vpw"` (→ `J1850VPW`) must never match them; and the four
`SAE_J2610_on_SAE_J2610_SCI` rows (`0x021E`-`0x0221`, the pin-typing
amendment) connect with `hw_protocol_override`, not their shared
`ChannelProtocol`'s own `j2534_protocol_id()` (`SCI_MODE`, which is no
longer used as a connect protocol by any table row at all). The fallback
now compares against `legacy_bustype_hw_id(row)`
(`names.rs`): `None` (never matches) for a row on the `SAE_J1850` bus;
otherwise `Some(hw_protocol_override.unwrap_or(protocol.j2534_protocol_id()))`
— so a legacy SCI hardware id (e.g. `SCI_A_ENGINE`) now correctly matches
*both* the bare `SAE_J2610_SCI` row that natively identifies as that id
(`0x0222`) and the `SAE_J2610_on_SAE_J2610_SCI` row that overrides its
connect protocol to it (`0x021E`), since both actually issue
`PassThruConnect` with it; a legacy `SCI_MODE` id now correctly matches
none, since nothing actually connects with it anymore. The `SAE_J1850`
auto-detect bus remains reachable only via its own bus name
(`"SAE_J1850"`), `bus_type_id` (`0x0307`), a `protocol` selector, or a
resource ID — never a legacy hw-id bus alias/numeric id, which by
construction cannot express "the auto-detecting bus." The `protocol`
selector's own legacy hw-id fallback (`candidates_matching_protocol`) needs
no equivalent fix: it only reaches its hw-id branch after an *exact*
`ChannelProtocol` match already failed, and the `SAE_J1850` rows'
`ChannelProtocol`s (`0x0152`/`0x0153`) are never equal to the native
`J1850VPW`/`J1850PWM` values a legacy protocol alias resolves to, so the
leak this fix addresses is specific to `bus_type`.

**`ResourceData.protocol_id` takes a `ChannelProtocol`/J2534 protocol value,
never a resource ID.** `GetResourceIds`'s `protocol` selector's numeric form
(`protocol_id`) is matched against `row.protocol.value()` (falling back to
`row.protocol.j2534_protocol_id()`) — i.e. it is the same numeric space as
`CreateComLogicalLink`'s legacy raw/extended `ChannelProtocol` input, *not*
the opaque `0x0200` resource-ID namespace this ADR introduces. Passing a
resource ID (e.g. `0x0206`) as `protocol_id` matches nothing, since the two
namespaces are disjoint by construction — this is intentional, not a gap:
selecting *by* resource ID is what the `resource_id`/`resource_name` fields
of `CreateComLogicalLink` are for, not a `GetResourceIds` selector (there is
no "give me the row for this resource ID" filter — the whole point of
`GetResourceIds` is to discover resource IDs from a protocol/bus-type/pin
description, not the reverse).

**`CreateComLogicalLink` resolves through the table first:** a `resource_id`
is looked up in the table before falling back to
`ChannelProtocol::from_raw`; a `resource_name`/`protocol_name` is matched
case-insensitively against `protocol_name`/`config_name` before falling back
to `map_protocol_name` and then numeric parsing (which itself re-enters the
table lookup, so a numeric string still prefers a table hit). When a row
matches, its canonical `bus_type_name`/`protocol_name` — not whatever ad hoc
`RscData.bus_type_name`/`protocol_name` fields the caller separately supplied
— drive the initial Working ComParam defaults
(`comparam_defaults::bustype_default_params`/`protocol_default_params`), so a
bare numeric `resource_id` now gets correct defaults, which the pre-table
code could not do (it needed a separate matching `RscData` name field to find
any defaults at all). The legacy path (no row matched) is unchanged.
**`comparam_defaults.rs` has an entry for every one of the 37 rows'
`bus_type_name` and (with five documented exceptions) `protocol_name`,
verified exhaustively by a unit test (below), not assembled ad hoc row by
row.** Three separate PR-review passes each caught one more row missing a
`protocol_default_params` entry before this exhaustive check existed
(0x0212/0x0213/0x0214/0x021C/0x021D/0x021F-0x0222 in the first pass; 0x021A
in the second; 0x0202/0x0203/0x0207 in the third) — each gap meant that
resource silently got only its bus-type defaults (e.g. no `CP_P2Max`), not
the full protocol-layer default a name-based `CreateComLogicalLink` call for
the spec-identical name already got. `resources::tests::every_resource_row_has_
protocol_and_bustype_defaults_or_is_allowlisted` now iterates the whole
table and fails if any row's `bus_type_name` doesn't resolve via
`bustype_default_params`, or its `protocol_name` doesn't resolve via
`protocol_default_params` *and* isn't in the explicit
`comparam_defaults::PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS` allowlist —
closing this class of gap: a future resource row with neither a preset nor
an allowlist entry now fails this test instead of silently shipping with an
incomplete Working set.

Every alias/per-configuration/combined/standalone protocol name added
alongside the table shares its closest existing preset's values rather than
duplicating them: `iso_9141_2_uart_and_iso_14230_1_uart`,
`sae_j1850_vpw_and_sae_j1850_pwm` (bustype); `iso_14229_3`,
`iso_14229_3_on_iso_15765_2`, `iso_15765_3` (all three spec-identical to
`iso_15765_3_on_iso_15765_2` — ISO_14229_3 supersedes ISO_15765_3 while
providing the same feature set); `iso_15031_5_on_iso_9141_2_and_iso_14230_4`,
`sae_j2190_on_iso_9141_2_and_iso_14230_2`, `sae_j2190_on_sae_j1850`,
`iso_15031_5_on_sae_j1850`, `sae_j2610_sci` (protocol).

**The five allowlisted `protocol_name`s** (`ISO_15765_2`, `ISO_9141_2`,
`ISO_14230_4`, `SAE_J1850_PWM`, `SAE_J1850_VPW` — resources 0x0206, 0x0210,
0x020C, 0x0216, 0x0219) are each a *native* `ChannelProtocol` (no
application-layer overlay), and their resource-table name is actually an
ISO 22900-2 BUSTYPE name already covered by `bustype_default_params`
(`iso_9141_2_uart`, `iso_14230_1_uart`, `sae_j1850_pwm`, `sae_j1850_vpw`),
not one of the 21 ISO 22900-3 Table B protocol-name entries
`protocol_default_params` implements — so they legitimately have no
protocol-layer defaults beyond their bus-type defaults. `ISO_11898_RAW`
(resource 0x0201) is the one native-protocol-shaped exception: it *is* a
genuine Table B entry (`iso_11898_raw`, with its own P2/P3 timing) and is
therefore not allowlisted.

**Correction (2026-07-28, PR #7, design-advisor consult on a Codex finding):**
the paragraph above misattributed this boundary to "ISO 22900-3" (no such
document exists in this workspace) and claimed the five allowlisted names are
absent from "Table B" — four of them (`ISO_9141_2`, `ISO_14230_4`,
`SAE_J1850_VPW`, `SAE_J1850_PWM`) actually appear as column headers in ISO
22900-2:2009(E) Annex B's Tables B.10/B.19. The Decision (these five stay
allowlisted, no protocol-layer overlay) is unchanged and still correct — only
the stated reason was wrong. The real reason: those transport-named B.10/B.19
columns denote the `ISO_15031_5` OBD application stack keyed by its transport
layer (Table B.2's `ISO_15031_5`-on-transport rows), not the bare, native
protocol itself — `ISO_15031_5` has no column of its own precisely because its
values vary per transport and live in these columns instead (confirmed by the
`ISO_9141_2` column's own content: it defaults tester-present to *enabled*
with the OBD service $01 PID $00 keep-alive message, which would be nonsensical
seeded onto a bare, application-layer-free K-line channel that Table B.1
defines as behaviorally identical to J2534's raw `ProtocolID`). So the bare
native protocols correctly get bus-type defaults only, same as this paragraph
already concluded — and the same reasoning pre-empts the identical question
for bare `ISO_9141_2` (0x0210) and `ISO_14230_4` (0x020C), whose columns even
default tester-present handling *on*, making the bare-vs-stack distinction
more consequential there than for J1850. **Verified against the 2022 edition
(2026-08-07, `iso22900-2-conformance-audit.md` A2-16):** the 2022 text's own
Table B.10 carries an explanatory note absent from 2009, describing these
same five columns as originally presented as one grouped, OBD-emissions-labeled
set — confirming, not merely restating, the reading above. No change to this
Decision.

**An ambiguous `resource_name`/`protocol_name` is rejected, not silently
resolved to the first table match.** `"SAE_J2610_SCI"` matches all four SCI
configuration rows, which share one `protocol_name` but differ by
`config_name` and `ChannelProtocol` — `find_table_row_by_name` returns
`Status::invalid_argument` (naming the available `config_name`s/resource
IDs) whenever a name matches rows with more than one distinct
`ChannelProtocol`, rather than picking the first in table order. A name
matching multiple rows that all share the *same* `ChannelProtocol` (true
alias rows) is unaffected, since any of them is behaviorally identical.

**Superseded note (2026-07-21, ADR-106):** the `GetConflictingResources`
portions of the next several paragraphs (its live-link resolution,
`resolve_protocols_for_name_filter`, echo/tie-break logic, and
`hw_protocol_override` live-link matching) describe pre-ADR-106 behavior
and no longer reflect the current implementation — `GetConflictingResources`
is now a static resource-table scan with no live-link involvement at all;
see ADR-106. `GetResourceStatus`'s own description below is unaffected and
still accurate.

**`GetResourceStatus`/`GetConflictingResources` accept table resource IDs
and names too.** Both RPCs' `resource_id` input is resolved through
`resources::find_by_resource_id` first (comparing the matched row's
`ChannelProtocol` against each link's `protocol`), falling back to the
legacy `ChannelProtocol::from_raw` interpretation — the same two-step
resolution `CreateComLogicalLink` uses — so a resource ID round-tripped from
`GetResourceIds` (e.g. `0x0206`) correctly reports an active link's status or
a conflict, not silently "not active"/"no conflict" from comparing the
opaque ID directly against a raw `ChannelProtocol` value. A `resource_name`
input resolves the same table-first way (`names::find_table_rows_by_name`
for `GetResourceStatus`'s echo, below; `names::resolve_protocols_for_name_filter`
for the active/conflict predicate and for `GetConflictingResources`, which
has no per-query row to echo), falling back to the legacy
`map_protocol_name`/`map_object_type_name` mapping only when no table row
matches at all — this is what makes a table-only name (e.g.
`"ISO_OBD_on_K_Line"`) work here too, not just in `CreateComLogicalLink`.
Unlike `CreateComLogicalLink`, these two RPCs treat a name matching several
`ChannelProtocol`s as "match any of them," not an error: since they are
filters, not creation requests, reporting a link active when it uses *any*
of `"SAE_J2610_SCI"`'s four configurations is the least-surprising behavior,
with no configuration to disambiguate to.

**The response `resource_id` these two RPCs echo is also table-aware, with a
table-order tie-break for a `ChannelProtocol` shared by several rows —
disambiguated further by `hw_protocol_override` when a matched candidate has
one.** `GetResourceStatus` responds with exactly one entry per query, so
there is always a single ID to choose: a `resource_id` query echoes the ID
the caller passed, unchanged; a `resource_name` matching exactly one table
row echoes *that row's own* resource ID, even when its `ChannelProtocol` is
shared with an alias row (`"ISO_OBD_on_K_Line"` echoes `0x0213`, never its
alias `0x0212` — a naive protocol-based lookup would otherwise pick whichever
of the two sorts first); a name matching several table rows directly (e.g.
`"SAE_J2610_SCI"`, or `"SAE_J2610_on_SAE_J2610_SCI"`, which also matches
four rows) echoes whichever matched row has an active link, else the first
row in table order (`0x0222` for `"SAE_J2610_SCI"`); a name resolved only
via legacy `map_protocol_name` (no direct table-name match, e.g.
`"ISO15765"`) echoes `resources::find_resource_id_for_protocol`'s
table-order-tie-break ID for that protocol (`0x0206`, the only row carrying
`ChannelProtocol::ISO15765`), or the raw `ChannelProtocol` value when the
protocol has no table row at all (a legacy-only extended protocol, matching
the pre-ADR-069 echo unchanged). `GetConflictingResources` has one entry per
matching *link*, not per query, so there is no per-query row to prefer: each
entry echoes `find_resource_id_for_protocol_and_hw`'s table-order-tie-break
ID for that link's own `ChannelProtocol` **and** `hw_protocol_id` (or the raw
value, same fallback).

**Matching an active link against a resolved candidate also checks
`hw_protocol_override`, not just `ChannelProtocol` (verification-pass
amendment).** The four `SAE_J2610_on_SAE_J2610_SCI` rows (`0x021E`-`0x0221`)
share one `ChannelProtocol` (`0x0160`) but each override a distinct hardware
protocol ID (`SCI_A_ENGINE`/`_A_TRANS`/`_B_ENGINE`/`_B_TRANS`), which becomes
the connected link's own `LogicalLinkState::hw_protocol_id` at
`CreateComLogicalLink` time. Matching on `ChannelProtocol` alone would make
`GetResourceStatus(ResourceId=0x021E)` falsely report active whenever *any*
of the other three configurations (e.g. `0x0221`) is connected, and would
make `GetConflictingResources`/a `resource_name` echo always pick the first
matching row (`0x021E`) in table order regardless of which configuration is
actually in use. Both RPCs now additionally require
`row.hw_protocol_override.is_none_or(|hw| hw == link.hw_protocol_id)` before
counting a candidate as matching an active link — `is_none_or` so rows with
no override (e.g. the J1850 auto-detect rows, whose hardware protocol ID
varies by connect-time detection rather than a fixed table value) are
unaffected and keep matching on `protocol` alone, exactly as before this
amendment. `resources::find_resource_id_for_protocol_and_hw` is the
link-aware counterpart of `find_resource_id_for_protocol` used for the
`GetConflictingResources`/legacy-name echo paths that need this check but
have no candidate `ResourceDef` row of their own to compare against (only a
link's `protocol`/`hw_protocol_id`).

**`GetObjectId(OBJT_RESOURCE, shortname)` resolves through the table the
same way, reusing `find_table_row_by_name`/`find_resource_id_for_protocol`
directly rather than new logic.** This RPC is a name→ID lookup, not a
filter (unlike `GetResourceStatus`/`GetConflictingResources`) and not a
creation request (unlike `CreateComLogicalLink`), but its semantics land
squarely with `CreateComLogicalLink`'s: a unique table-name match returns
that row's resource ID (`"ISO_OBD_on_K_Line"` → `0x0213`,
`"SCI_B_TRANS"` → `0x0225`); an ambiguous name (`"SAE_J2610_SCI"`, or now
`"SAE_J2610_on_SAE_J2610_SCI"` too, since `GetObjectId` has no pin data to
narrow with) is rejected with `invalid_argument` naming the available
configurations/resource IDs, identical to `CreateComLogicalLink`'s own
rejection (there is no "match any" reading available for a single-value
lookup); a legacy-only name resolves via `find_resource_id_for_protocol`
(`"ISO15765"` → `0x0206`) or the raw value when the protocol has no table
row; an unrecognized name keeps the pre-existing numeric-fallback/`0`
behavior (**superseded by ADR-078**, which rejects an unrecognized name with
`PDU_ERR_INVALID_PARAMETERS` instead). Before this fix, `OBJT_RESOURCE` resolved only through the legacy
`map_protocol_name` (via a since-removed `map_object_type_name` wrapper),
so it could not return a resource ID for a table-only name at all (`0`) and
returned the raw protocol value, not a resource ID, for every other name —
found and fixed in a fourth PR-review pass, after the first three had
already covered `GetResourceIds`/`CreateComLogicalLink`/
`GetResourceStatus`/`GetConflictingResources`.

**`GetObjectId(OBJT_PROTOCOL, shortname)`/`GetObjectId(OBJT_BUSTYPE, shortname)` resolve
through the table first too (conformance-audit finding A1-2, 2026-07-22).**
`OBJT_PROTOCOL` uses a dedicated helper, `find_protocol_for_name` — **not**
`find_table_row_by_name` (the initial version of this fix reused
`find_table_row_by_name` directly, but a Codex review on PR #115 caught a
regression: the four `"SAE_J2610_on_SAE_J2610_SCI"` rows share one
`ChannelProtocol` and differ only in `hw_protocol_override`, which
`find_table_row_by_name` also treats as an ambiguity axis since
`OBJT_RESOURCE`/`CreateComLogicalLink` must pick one specific hardware
configuration; a protocol-identity query has no such need, so that name —
previously resolvable via `map_protocol_name` — briefly stopped resolving
at all). `find_protocol_for_name` only rejects a name as ambiguous when its
matching rows differ in `ChannelProtocol` itself: a unique-protocol table
`protocol_name`/`config_name` match returns that row's `ChannelProtocol`
value (regardless of `hw_protocol_override` divergence, e.g. the SCI
`_on_` family); a match spanning genuinely distinct `ChannelProtocol`s
(e.g. bare `"SAE_J2610_SCI"`) is rejected with `invalid_argument`, same as
`OBJT_RESOURCE`; an unrecognized name falls through to the pre-existing
`map_protocol_name`/numeric-parse chain, keeping `OBJT_PROTOCOL`'s own
`not_found` carve-out for that case unchanged. `OBJT_BUSTYPE` cannot reuse
`find_table_row_by_name` either (it matches `protocol_name`/`config_name`,
not `bus_type_name`), so it uses a new helper, `find_bustype_id_by_name` — a
plain case-insensitive first-match over the table by `bus_type_name`,
returning that row's `bus_type_id`. Unlike `protocol_name` (which can span
several distinct `ChannelProtocol`s per name, e.g. the SCI configurations,
hence `find_protocol_for_name`'s ambiguity handling), `bus_type_name` is 1:1
with `bus_type_id` for every row by construction — a regression guard
(`resources.rs::every_bus_type_name_maps_to_exactly_one_bus_type_id`) sweeps
the whole table to keep that invariant honest, since nothing else would
catch a future row silently breaking it. All three arms fall back to their
pre-existing `map_protocol_name`/`map_bustype_name` legacy alias resolution
unchanged when no table row matches. Before this fix, neither arm consulted
the table at all: a table-canonical name like `"ISO_11898_2_DWCAN"` or
`"SAE_J1850_VPW"` silently returned the wrong numeric ID (a J2534 hardware
ID instead of the table's own opaque `bus_type_id`), and several other
canonical bus type names (`"ISO_9141_2_UART"`, `"ISO_14230_1_UART"`,
`"SAE_J1850"`, `"SAE_J2610_UART"`) failed outright with
`PDU_ERR_INVALID_PARAMETERS` — breaking the spec's `GetObjectId ->
GetResourceIds` discovery round trip for most ISO 22900-2 canonical names
(Annex B.1/B.2). See
`j2534-0404-service/docs/iso22900-2-conformance-audit.md`'s A1-2 entry for
the full finding.

### Full resource table

| Resource ID | Protocol Name (Config) | `ChannelProtocol` | Bus Type ID | Bus Type Name |
|---|---|---|---|---|
| `0x0201` | `ISO_11898_RAW` | `CAN` | `0x0301` | `ISO_11898_2_DWCAN` |
| `0x0202` | `ISO_14229_3` | `ISO_14229_3` | `0x0301` | `ISO_11898_2_DWCAN` |
| `0x0203` | `ISO_14229_3_on_ISO_15765_2` | `ISO_14229_3_ON_ISO_15765_2` | `0x0301` | `ISO_11898_2_DWCAN` |
| `0x0204` | `ISO_14230_3_on_ISO_15765_2` | `ISO_14230_3_ON_ISO_15765_2` | `0x0301` | `ISO_11898_2_DWCAN` |
| `0x0205` | `ISO_15031_5_on_ISO_15765_4` | `ISO_15031_5_ON_ISO_15765_4` | `0x0301` | `ISO_11898_2_DWCAN` |
| `0x0206` | `ISO_15765_2` | `ISO15765` | `0x0301` | `ISO_11898_2_DWCAN` |
| `0x0207` | `ISO_15765_3` | `ISO_15765_3` | `0x0301` | `ISO_11898_2_DWCAN` |
| `0x0208` | `ISO_15765_3_on_ISO_15765_2` | `ISO_15765_3_ON_ISO_15765_2` | `0x0301` | `ISO_11898_2_DWCAN` |
| `0x0209` | `ISO_OBD_on_ISO_15765_4` (alias of `0x0205`) | `ISO_15031_5_ON_ISO_15765_4` | `0x0301` | `ISO_11898_2_DWCAN` |
| `0x020A` | `SAE_J2190_on_ISO_15765_2` | `SAE_J2190_ON_ISO_15765_2` | `0x0301` | `ISO_11898_2_DWCAN` |
| `0x020B` | `ISO_14230_3_on_ISO_14230_2` | `ISO_14230_3_ON_ISO_14230_2` | `0x0302` | `ISO_14230_1_UART` |
| `0x020C` | `ISO_14230_4` | `ISO14230` | `0x0302` | `ISO_14230_1_UART` |
| `0x020D` | `ISO_15031_5_on_ISO_14230_4` | `ISO_15031_5_ON_ISO_14230_4` | `0x0302` | `ISO_14230_1_UART` |
| `0x020E` | `SAE_J2190_on_ISO_14230_2` | `SAE_J2190_ON_ISO_14230_2` | `0x0302` | `ISO_14230_1_UART` |
| `0x020F` | `ISO_15031_5_on_ISO_9141_2` | `ISO_15031_5_ON_ISO_9141_2` | `0x0303` | `ISO_9141_2_UART` |
| `0x0210` | `ISO_9141_2` | `ISO9141` | `0x0303` | `ISO_9141_2_UART` |
| `0x0211` | `SAE_J2190_on_ISO_9141_2` | `SAE_J2190_ON_ISO_9141_2` | `0x0303` | `ISO_9141_2_UART` |
| `0x0212` | `ISO_15031_5_on_ISO_9141_2_and_ISO_14230_4` | `ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4` | `0x0304` | `ISO_9141_2_UART_and_ISO_14230_1_UART` |
| `0x0213` | `ISO_OBD_on_K_Line` (alias of `0x0212`) | `ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4` | `0x0304` | `ISO_9141_2_UART_and_ISO_14230_1_UART` |
| `0x0214` | `SAE_J2190_on_ISO_9141_2_and_ISO_14230_2` | `SAE_J2190_ON_ISO_9141_2_AND_ISO_14230_2` | `0x0304` | `ISO_9141_2_UART_and_ISO_14230_1_UART` |
| `0x0215` | `ISO_15031_5_on_SAE_J1850_PWM` | `ISO_15031_5_ON_SAE_J1850_PWM` | `0x0305` | `SAE_J1850_PWM` |
| `0x0216` | `SAE_J1850_PWM` | `J1850PWM` | `0x0305` | `SAE_J1850_PWM` |
| `0x0217` | `SAE_J2190_on_SAE_J1850_PWM` | `SAE_J2190_ON_SAE_J1850_PWM` | `0x0305` | `SAE_J1850_PWM` |
| `0x0218` | `ISO_15031_5_on_SAE_J1850_VPW` | `ISO_15031_5_ON_SAE_J1850_VPW` | `0x0306` | `SAE_J1850_VPW` |
| `0x0219` | `SAE_J1850_VPW` | `J1850VPW` | `0x0306` | `SAE_J1850_VPW` |
| `0x021A` | `SAE_J2190_on_SAE_J1850` | `SAE_J2190_ON_SAE_J1850` | `0x0307` | `SAE_J1850` (moved from `0x0306`, see ADR-070) |
| `0x021B` | `SAE_J2190_on_SAE_J1850_VPW` | `SAE_J2190_ON_SAE_J1850_VPW` | `0x0306` | `SAE_J1850_VPW` |
| `0x021C` | `ISO_15031_5_on_SAE_J1850` | `ISO_15031_5_ON_SAE_J1850` | `0x0307` | `SAE_J1850` |
| `0x021D` | `ISO_OBD_on_SAE_J1850` (alias of `0x021C`) | `ISO_15031_5_ON_SAE_J1850` | `0x0307` | `SAE_J1850` |
| `0x021E` | `SAE_J2610_on_SAE_J2610_SCI` (hw override `SCI_A_ENGINE`) | `SAE_J2610_ON_SAE_J2610_SCI` | `0x0308` | `SAE_J2610_UART` |
| `0x021F` | `SAE_J2610_on_SAE_J2610_SCI` (hw override `SCI_A_TRANS`) | `SAE_J2610_ON_SAE_J2610_SCI` | `0x0308` | `SAE_J2610_UART` |
| `0x0220` | `SAE_J2610_on_SAE_J2610_SCI` (hw override `SCI_B_ENGINE`) | `SAE_J2610_ON_SAE_J2610_SCI` | `0x0308` | `SAE_J2610_UART` |
| `0x0221` | `SAE_J2610_on_SAE_J2610_SCI` (hw override `SCI_B_TRANS`) | `SAE_J2610_ON_SAE_J2610_SCI` | `0x0308` | `SAE_J2610_UART` |
| `0x0222` | `SAE_J2610_SCI` (`SCI_A_ENGINE`) | `SCI_A_ENGINE` | `0x0308` | `SAE_J2610_UART` |
| `0x0223` | `SAE_J2610_SCI` (`SCI_A_TRANS`) | `SCI_A_TRANS` | `0x0308` | `SAE_J2610_UART` |
| `0x0224` | `SAE_J2610_SCI` (`SCI_B_ENGINE`) | `SCI_B_ENGINE` | `0x0308` | `SAE_J2610_UART` |
| `0x0225` | `SAE_J2610_SCI` (`SCI_B_TRANS`) | `SCI_B_TRANS` | `0x0308` | `SAE_J2610_UART` |

`0x021E`-`0x0221`'s "hw override" is `ResourceDef::hw_protocol_override`
(pin-typing amendment): the column that actually reaches `PassThruConnect`,
diverging from the shared `SAE_J2610_ON_SAE_J2610_SCI` identity in the
`ChannelProtocol` column. Every other row's `hw_protocol_override` is `None`.

Per-configuration DLC pins for `SAE_J2610_UART` (both the `_on_` and bare
rows use identical pins per configuration): `SCI_A_ENGINE`
`[(6,TX),(7,RX)]`, `SCI_A_TRANS` `[(14,TX),(7,RX)]`, `SCI_B_ENGINE`
`[(12,TX),(7,RX)]`, `SCI_B_TRANS` `[(9,TX),(15,RX)]`.

Bus types (typed `dlc_pins` in brackets, pin-typing amendment):
`0x0301 ISO_11898_2_DWCAN [(6,HI),(14,LOW)]`,
`0x0302 ISO_14230_1_UART [(7,K),(15,L)]`,
`0x0303 ISO_9141_2_UART [(7,K),(15,L)]`, `0x0304
ISO_9141_2_UART_and_ISO_14230_1_UART [(7,K),(15,L)]`,
`0x0305 SAE_J1850_PWM [(2,PLUS),(10,MINUS)]`,
`0x0306 SAE_J1850_VPW [(2,PLUS)]`,
`0x0307 SAE_J1850 [(2,PLUS),(10,MINUS)]` (renamed from
`SAE_J1850_VPW_and_SAE_J1850_PWM`, ADR-070), `0x0308 SAE_J2610_UART`
(per-configuration pins, see above — no single bus-wide pin set).

This table is the authoritative source; see `resources.rs::RESOURCE_TABLE`
for the executable definition and `docs/j2534-0404-architecture.md` §6 for
the same table reproduced in the architecture reference.

## Consequences

- **`GetResourceIds` output values changed.** A client matching, say,
  `protocol_name = "ISO15765"` now receives resource ID `0x0206`, not the raw
  J2534 protocol ID `0x06` it received before this ADR. Any caller that
  treated the returned value as a raw J2534/`ChannelProtocol` ID directly
  (rather than as an opaque handle to hand back to `CreateComLogicalLink`)
  must be updated. `tests/grpc_mock/lifecycle.rs` was updated for this.
- **`CreateComLogicalLink` is backward compatible.** Legacy raw
  (`1..=0x0B`) and extended (`0x0100+`) `ChannelProtocol` values keep working
  exactly as before (`ChannelProtocol::from_raw` fallback) — only the
  `GetResourceIds` *output* changed, not what `CreateComLogicalLink` accepts.
  `tests/grpc_mock/lifecycle.rs` and `tests/grpc_mock/resources.rs` both
  assert this fallback still creates a link successfully.
- **ComParam defaults are more correct for numeric/table resource IDs.**
  Before this ADR, `CreateComLogicalLink(ResourceId(n))` (or any `RscData`
  without an explicit `bus_type_name`/`protocol_name`) got zero ComParam
  defaults, since the lookup needed a name field the caller had no reason to
  also supply when passing a numeric ID. Resolving through the table now
  supplies the row's own canonical names to the same lookup, so numeric
  resource IDs get the same defaults a name-based `CreateComLogicalLink` call
  would — for every row that has a `protocol_default_params` entry (see the
  five-name allowlist in the Decision section above for the rows that
  legitimately don't). Three PR-review passes after this ADR's initial
  landing each caught a batch of rows missing that entry, one batch at a
  time, before the exhaustive sweep test existed: the combined-bus-type,
  bus-agnostic VPW/PWM, and per-configuration SCI rows
  (0x0212/0x0213/0x0214/0x021C/0x021D/0x021F-0x0222 in the first pass,
  0x021A in the second), then the ISO_14229_3/standalone-ISO_15765_3 rows
  (0x0202/0x0203/0x0207 in the third) — each affected row resolved to only
  its bus-type defaults (missing e.g. `CP_P2Max`) despite the table row
  matching correctly. `resources::tests::every_resource_row_has_protocol_and_
  bustype_defaults_or_is_allowlisted` (added in the third pass) now makes
  this a compile-time-adjacent regression gate instead of a "wait for the
  next PR review" one.
- **Superseded note (2026-07-21, ADR-106):** the `GetConflictingResources`
  portions of the next three bullets describe pre-ADR-106 live-link-based
  behavior; `GetConflictingResources` is now a static resource-table scan
  (see ADR-106). The `GetResourceStatus` portions remain accurate.
- **`GetResourceStatus`/`GetConflictingResources` resolve table resource IDs
  and names too**, via the same table-first/legacy-fallback resolution as
  `CreateComLogicalLink` (also caught and fixed after this ADR's initial
  landing, across three PR-review passes) — a resource ID or name fed back
  from `GetResourceIds` reports accurate status/conflicts, not always "not
  active"/"no conflict" from comparing the opaque ID against a raw
  `ChannelProtocol` value or from the legacy name mapping never recognizing
  a table-only name. Unlike `CreateComLogicalLink`, an ambiguous name here
  matches any of its several `ChannelProtocol`s rather than erroring, since
  these two RPCs are filters, not creation requests.
- **The response `resource_id` these two RPCs echo is table-aware too** (a
  third PR-review pass, after the second had already fixed the lookup/match
  side): echoing a raw `ChannelProtocol`/legacy value for a `resource_name`
  query left the caller unable to correlate the result back to
  `GetResourceIds` output (e.g. `resource_name="ISO_15765_2"` reported
  `resource_id=6`, not `0x0206`; a table-only name reported `0`). Fixed with
  the table-order tie-break described in the Decision section above --
  `GetResourceStatus` prefers a direct table-name match's own row (falling
  back to an active-link preference for an ambiguous name, then to
  `find_resource_id_for_protocol`'s tie-break for a legacy-only name),
  `GetConflictingResources` uses `find_resource_id_for_protocol` per
  matching link.
- **`GetResourceStatus`/`GetConflictingResources` also key on
  `hw_protocol_override`, not just `ChannelProtocol` (verification-pass
  amendment).** The four `SAE_J2610_on_SAE_J2610_SCI` rows share one
  `ChannelProtocol` but override distinct hardware protocol IDs, which
  become the connected link's own `hw_protocol_id` — matching on
  `ChannelProtocol` alone made `GetResourceStatus(ResourceId=0x021E)` falsely
  report active whenever a *different* configuration (e.g. `0x0221`) was
  connected, and made the conflict/name echo always pick the first matching
  row (`0x021E`) in table order regardless of which configuration was
  actually in use. Both RPCs now additionally require a matched candidate's
  `hw_protocol_override` (when it has one) to equal the link's own
  `hw_protocol_id`; rows/protocols with no override are unaffected. See the
  Decision section's amended paragraph above and
  `resources::find_resource_id_for_protocol_and_hw`.
- **Pin narrowing can turn a previously-unambiguous `CreateComLogicalLink`
  name into a rejected request.** Before this fix, `dlc_pin_data` was only
  consulted once a name matched more than one row; a name resolving to
  exactly one row connected regardless of what pins (if any) the caller also
  supplied. Pin narrowing is now applied whenever `dlc_pin_data` is
  non-empty, even for a name that already resolves uniquely — e.g.
  `CreateComLogicalLink(protocol_name="ISO_15765_2", dlc_pin_data=[pin 7])`
  now returns `invalid_argument` (resource `0x0206`'s only pins are `6`/`HI`
  and `14`/`LOW`, so pin `7` narrows the single candidate to zero), where it
  previously ignored the pin data and connected. This is intentional: pin
  data the caller supplied but that contradicts the resolved row's actual
  wiring is a caller error worth surfacing, not silently discarding.
- **An ambiguous `CreateComLogicalLink` resource name is now rejected.**
  `"SAE_J2610_SCI"` previously resolved silently to the first matching row
  in table order (`SCI_A_ENGINE`) — a second PR-review pass caught this and
  changed `find_table_row_by_name` to return `invalid_argument` (naming the
  available configurations/resource IDs) whenever a name matches rows with
  more than one distinct `ChannelProtocol`. A specific `config_name` (e.g.
  `"SCI_B_TRANS"`) still resolves unambiguously, as does every other table
  name. The pin-typing amendment widens the ambiguity test to
  `ChannelProtocol` **or** `hw_protocol_override` differing, since
  `"SAE_J2610_on_SAE_J2610_SCI"`'s four rows share one `ChannelProtocol` but
  would otherwise silently resolve to `SCI_A_ENGINE` the same way; typed
  `dlc_pin_data` on the `RscData` path can narrow such a name to one row
  without erroring.
- **Pin-typing amendment (spec correction): resource IDs `0x0201`-`0x021D`
  are unchanged; `SAE_J2610_on_SAE_J2610_SCI` expands from one row (`0x021E`)
  to four (`0x021E`-`0x0221`, one per SCI configuration via
  `hw_protocol_override`), and `SAE_J2610_SCI`'s four configuration rows are
  renumbered `0x0222`-`0x0225` (previously `0x021F`-`0x0222`) to make room —
  37 rows total, up from 34.** `dlc_pins` changed type from a bare pin-number
  list to typed `(pin_number, pin_type_id)` pairs, and `GetResourceIds`'s pin
  filtering became row-based (matching a row's own declared pin/type pairs)
  instead of the previous two-part generic protocol-family/pin-type check —
  see the Decision section's "Pin-driven configuration selection" for the
  full semantics. Any caller that hard-coded the old `0x021F`-`0x0222` SCI
  IDs must be updated; `GetResourceIds` is the only supported way to
  discover them.
- **`GetObjectId(OBJT_RESOURCE, shortname)` resolves through the table too**
  (a fourth PR-review pass) — it previously used only the legacy
  `map_protocol_name` mapping (via a now-removed `map_object_type_name`
  wrapper, since nothing else called it), so a table-only name like
  `"ISO_OBD_on_K_Line"` returned `0` and every other name returned a raw
  protocol value instead of a resource ID. Now reuses
  `find_table_row_by_name`/`find_resource_id_for_protocol` directly (no new
  resolution logic), landing on `CreateComLogicalLink`'s semantics: unique
  match → that row's ID, ambiguous match → `invalid_argument`, legacy-only
  name → the table ID for that protocol, unrecognized → unchanged (**superseded
  by ADR-078**: an unrecognized name is now rejected with
  `PDU_ERR_INVALID_PARAMETERS`, for `OBJT_RESOURCE` and every other
  `GetObjectId` object type except `OBJT_PROTOCOL`).
- **`GetObjectId(OBJT_PROTOCOL, shortname)`/`GetObjectId(OBJT_BUSTYPE, shortname)`
  resolve through the table first too (conformance-audit finding A1-2,
  2026-07-22)** — previously neither arm consulted the table at all, so a
  table-canonical name either returned the wrong numeric ID (a J2534
  hardware ID instead of the table's own opaque `bus_type_id`, for
  `OBJT_BUSTYPE`) or failed outright. `OBJT_PROTOCOL` uses a new
  `find_protocol_for_name` helper — deliberately **not**
  `find_table_row_by_name` (Codex-review fix, PR #115: reusing it directly
  regressed `"SAE_J2610_on_SAE_J2610_SCI"`, since it also treats differing
  `hw_protocol_override`s as ambiguous, which is irrelevant to a
  protocol-identity query); `find_protocol_for_name` only rejects a name
  whose matching rows differ in `ChannelProtocol` itself. `OBJT_BUSTYPE`
  uses a new `find_bustype_id_by_name` helper (`bus_type_name` is 1:1 with
  `bus_type_id` by construction, so no ambiguity handling is needed, unlike
  `protocol_name`). All three arms fall back to the pre-existing legacy
  alias maps unchanged when no table row matches. See the Decision
  section's paragraph above for the full before/after behavior.
- **VPW/PWM auto-detection is now implemented for the renamed `SAE_J1850`
  bus, superseding this ADR's original limitation text — see ADR-070** for
  the probe mechanism. K-line 5-baud-vs-fast-init selection remains
  unimplemented, consistent with ADR-017: the combined K-line bus still
  encodes one fixed connect protocol with no probe.
- **`GetResourceIds`'s `bus_type` legacy hw-id fallback no longer leaks the
  `SAE_J1850` auto-detect rows into a fixed-flavor legacy alias
  (verification-pass fix, P2).** A legacy bus alias like `"j1850_vpw"` (→
  `map_bustype_name` → `J1850VPW`) previously matched every row whose
  `ChannelProtocol::j2534_protocol_id()` equalled `J1850VPW` — which
  included `0x021A`/`0x021C`/`0x021D` (ADR-070's auto-detect rows), since
  `j2534_protocol_id()` for their `ChannelProtocol`s is only the VPW
  *initial probe candidate*, not a fixed connect protocol. A caller filtering
  for the genuinely fixed-VPW bus therefore got handed auto-detecting rows
  too. Fixed by excluding the `SAE_J1850` bus from this fallback entirely
  (see the Decision section's new paragraph on `legacy_bustype_hw_id`), which
  also corrected an analogous inconsistency for the SCI
  `hw_protocol_override` rows: the fallback now compares a row's actual
  fixed connect protocol (`hw_protocol_override` when set), not its
  `ChannelProtocol`'s own hardware id, so a legacy SCI hardware id now finds
  every row that actually connects with it.
- `docs/j2534-0404-architecture.md`,
  `j2534-0404-service/docs/protocol-mapping.md`, and
  `j2534-0404-service/docs/implementation-notes.md` are updated alongside this
  ADR to describe the table instead of the flat protocol-ID scheme.

See ADR-023 (`ChannelProtocol` abstraction this table's `protocol` field
reuses unchanged) and ADR-017 (J2534-1 protocol scope — the principle behind
combined-bus-type resources fixing one connect protocol rather than
auto-detecting).

- **`GetConflictingResources`'s live-link-based resolution, echo, and
  `hw_protocol_override` matching described above is superseded by ADR-106
  (2026-07-21).** ISO 22900-2 §9.4.26 defines `GetConflictingResources` as a
  static resource-table query, independent of live connection state; the
  live-CLL scan this ADR originally described (and the
  `resolve_protocols_for_name_filter`/`find_resource_id_for_protocol_and_hw`
  helpers it used) is removed. `GetResourceStatus` is unaffected and
  continues to work exactly as described in this ADR. See ADR-106 for the
  replacement design.
- **Extended (not superseded) by ADR-156 (2026-07-31, J2534-2 Pin
  Selection/Additional Channels, Phase 2).** `names.rs::retain_rows_matching_pin`
  (this ADR's typed-pin narrowing, used to disambiguate the four
  `SAE_J2610_SCI` `hw_protocol_override` rows) is reused, not replaced, as
  the resolution entry point for a caller-supplied `dlc_pin_data` that
  selects a `_PS` protocol variant instead of matching an existing row's
  fixed pins. A new, ADR-156-owned resolution path (arithmetic `_CHx`
  mapping, `_PS` hardware-ID + `pin_select` computation) runs alongside this
  table's static lookup; no `RESOURCE_TABLE` row changes, and every existing
  resource resolves exactly as this ADR describes when no pin/channel-index
  qualifier is supplied.
