# ADR-106: Static Table Scan Replaces Live-Connection Scan for GetConflictingResources

**Date:** 2026-07-21
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_link.rs`, `j2534-0404-service/src/service/resources.rs`, `j2534-0404-service/src/service/names.rs`

## Context

`GetConflictingResources` reported currently-connected same-protocol
`ComLogicalLink`s as conflicts: it scanned every live link and matched its
`(protocol, hw_protocol_override)` against the queried resource. ISO
22900-2:2009(E) §9.4.26.1/§9.4.26.2, and the worked example in Annex G.1.4,
define `GetConflictingResources` as a static MDF/CDF resource-table query
(pin/controller conflicts) that is computable *before* any `ComLogicalLink`
exists at all, and that explicitly treats several CLLs sharing one physical
channel with the same protocol/baud as spec-legal, not a conflict. The
live-connection scan therefore misreported ordinary channel sharing as a
resource conflict, and could not be called pre-CLL as the spec intends. The
full defect writeup is finding A1-1 in
`j2534-0404-service/docs/iso22900-2-conformance-audit.md`.

## Decision

Conflicts are now computed purely from the static `RESOURCE_TABLE`
(`resources::resource_table()`), with no live connection state consulted.
Two distinct rows `A`/`B` conflict iff they share at least one DLC pin
*number* (`dlc_pins`'s `.0`, ignoring the logical pin type in `.1` — the
same physical pin can carry a different logical pin type across
configurations, e.g. pin 6 is `PIN_HI` on the CAN rows but `PIN_TX` on
`SCI_A_ENGINE`, and that is still a real wiring conflict) *or* they sit on
the same physical controller — using `bus_type_id` as the controller-group
proxy, per Annex G.1.4's own worked example, which flags two routes as
conflicting solely because they share one CAN controller, even with
disjoint pins — and are *not* the same electrical configuration on that
controller (identical `bus_type_id` *and* `dlc_pins`) — that case is exactly
the spec-legal channel sharing ISO 22900-2 allows (e.g. the ten
`ISO_11898_2_DWCAN`-family rows, 0x0201-0x020A, all of which share
`PINS_ISO_11898_2_DWCAN`, or the two `SAE_J2610_UART` naming schemes for one
SCI config, e.g. 0x0220 vs 0x0224), so it must not be reported as a
conflict. This predicate lives in `resources::rows_conflict`.

No explicit controller-grouping field was added to `ResourceDef`;
`bus_type_id` is reused as the controller-group proxy instead (`same_bus` in
`rows_conflict`). A pin-overlap-only predicate was the original design, but
a Codex review pass on PR #109 found it insufficient: the eight
`SAE_J2610_UART` (SCI) rows model one physical SCI transceiver whose wiring
selects among four configurations (A_ENGINE/A_TRANS/B_ENGINE/B_TRANS), and
`B_TRANS`'s pins ((9, TX), (15, RX)) share no pin number with the other
three configs, so pin overlap alone missed that it cannot be used
simultaneously with them. `bus_type_id` fixes this because every row pair
sharing one controller in this table also shares one `bus_type_id`, and
because grouping by `bus_type_id` is a superset of the old pin-overlap-only
predicate: of the table's 8 `bus_type_id` groups (`resources.rs`'s
`RESOURCE_TABLE`), only `BUSTYPE_SAE_J2610_UART` (the 8 SCI rows) has more
than one distinct `dlc_pins` value among its rows -- every other group's
rows all share one identical `dlc_pins` list, so `same_bus` is a no-op there
(already exempted by `same_config`), adding no false positives and closing
no other gap. Independently re-verified twice (a design-advisor pass and a
separate edge-case-hunter pass, both walking the live table row-by-row) with
no discrepancy found.

For `GetConflictingResources`'s `resource` selector: a `resource_id`
resolves via `resources::find_by_resource_id`; a `resource_name` resolves
via `find_table_rows_by_name` (a name can legitimately match several rows,
e.g. `"SAE_J2610_SCI"`). The queried row(s)' conflicts are unioned and
deduplicated by resource ID, emitted in table order. An unmapped legacy
`resource_id` (a raw/extended `ChannelProtocol` value with no table row,
still valid for `CreateComLogicalLink` per ADR-069) or a `resource_name`
matching no row both yield an empty conflict list rather than an error or a
protocol-only fallback match — the latter would reintroduce the same
"protocol sharing is not a conflict" bug this ADR fixes. The `resource`
oneof being entirely unset is rejected as `PDU_ERR_INVALID_PARAMETERS`,
matching §9.4.26.2 a)'s parameter-validation requirement.

`input_module_list` is validated the same way: entirely absent is
`PDU_ERR_INVALID_PARAMETERS` (the `pInputModuleList` NULL-pointer-equivalent
case); present but empty is a valid call with nothing to check against, so
it returns an empty result; each entry present is validated against the
single supported module handle via the existing `require_module_handle`
(rejecting with `PDU_ERR_INVALID_HANDLE`), failing closed on the first bad
entry.

The prior live-CLL-based scan (`active_link_handles`-driven) is removed
entirely, not kept alongside the static scan — the two semantics are
mutually exclusive per the spec, and `GetResourceStatus` remains the correct
RPC for live in-use/lock state.

## Consequences

- `GetConflictingResources` now matches Annex G.1.4's worked example and is
  callable before any CLL exists, as ISO 22900-2 intends.
- Same-protocol channel sharing (several CLLs on one physical channel) is no
  longer misreported as a resource conflict.
- **Externally-observable behavior change.** Any caller relying on the old
  live-connection-based semantics (conflicts only appearing once a CLL is
  connected, echoing that CLL's own resolved resource ID) now sees a fixed,
  connection-independent result set instead; `GetResourceStatus` is
  unaffected and remains the RPC for live active/in-use state.
- **Accepted residual (revised — see the `same_bus` addition above, PR #109
  Codex review):** the `bus_type_id`-as-controller-group proxy assumes at
  most one physical controller per `bus_type_id` in this table, and treats
  SCI-A and SCI-B as one shared controller (conservative, matching this
  adapter's own single-`SAE_J2610_UART`-channel SCI model — see
  `resources.rs`'s module doc comment and `protocol.rs`'s "SAE J2610 SCI
  channel" singular framing). If a future device needs SCI-A and SCI-B to
  run concurrently (i.e. two independent physical SCI controllers under one
  `bus_type_id`), or if any future resource-table row otherwise shares a
  `bus_type_id` with a row on a genuinely different physical controller,
  this proxy over-reports a conflict, and `ResourceDef` will need an
  explicit `controller_group` field at that point. Not needed now — no such
  row exists in the table today.
- This decision follows ADR-069's resource-table resolution style
  (`find_by_resource_id` / `find_table_rows_by_name`) but does not supersede
  it.
- **Reviewed and rejected residual (Codex review, PR #109):** a review pass
  suggested erroring instead of returning an empty conflict list when
  neither selector resolves, on the premise that legacy raw
  `ChannelProtocol` values are a `CreateComLogicalLink`-only input. That
  premise does not hold in this codebase: `GetResourceStatus`
  (`rpc_link.rs`, unchanged by this ADR) resolves an unmatched
  `resource_id`/`resource_name` the same way (`ChannelProtocol::from_raw`
  fallback / legacy name mapping) and reports "not active" rather than
  erroring — this dual-namespace convention is ADR-069, not scoped to link
  creation. `ChannelProtocol::from_raw` is also total over `u32` by design
  (`protocol.rs`, "without validation"), so there is no principled
  "invalid ID" boundary to enforce, and erroring only in
  `GetConflictingResources` would make the same selector valid in one
  resource RPC and rejected in the adjacent one. Kept as decided above.
