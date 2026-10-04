# ADR-156: J2534-2 Pin Selection & Additional Channels — Connect-Time Resource Qualifiers (Phase 2)

**Date:** 2026-07-31
**Status:** Accepted (partially supersedes ADR-023's "Physical channel sharing" bullet — `ChannelKey`'s shape — and adds a resolution path alongside ADR-069's static table lookup; neither ADR's core abstraction is replaced. Phase 2a's implementation of this Decision left `hw_protocol_id`'s handling in ~30 pre-existing call sites incorrect for a `_PS` link; corrected by ADR-157, which this ADR's Decision 2 and Corrections remain otherwise unaffected by. Decision 2's `ChannelKey` shape (the 3-tuple this ADR widened ADR-023's original 2-tuple to) is widened again, to a 4-tuple adding the CAN FD effective data-phase rate, by ADR-158's "Correction (Codex review, PR #30)" — this ADR's own Decision 2 rationale for keying on connect-time-fixed channel identity remains otherwise unaffected)
**Affects:**
- `docs/adr/ADR-023-channel-protocol-abstraction.md` (Status line annotated)
- `docs/adr/ADR-069-j2534-resource-id-table.md` (Status line annotated)
- `docs/j2534-2-support-plan.md` (Phase 2 progress)
- `j2534-0404-service/src/service/{resources.rs,names.rs,rpc_link.rs,protocol.rs,discovery.rs,service.rs}`
- `vci-service-interface/src/proto/service.proto` (Phase 2b only — new `ResourceData.channel_index` field)
- `j2534-0404-mock/src/lib.rs` (Phase 2b's `_CHx` connect/capacity simulation)
- `j2534-0404/src/lib.rs` (Phase 2b's `_CHx`/`DEVICE_INFO_*_SUPPORTED` re-exports)
- `docs/rpc-api-guide.md`, `docs/glossary.md`, `docs/j2534-0404-architecture.md` (Phase 2b)

## Context

`docs/j2534-2-support-plan.md`'s Phase 2 brings up SAE J2534-2 (DEC2020) clauses 6
(Pin Selection) and 7 (Additional Channels). Both are cross-cutting: clause 6's
`_PS` protocol ID variants (e.g. `PROTOCOL_CAN_PS`) leave physical DLC pins
unassigned at connect until the caller sets them via `SET_CONFIG`
(`CONFIG_J1962_PINS`/`CONFIG_J1939_PINS`/`CONFIG_J1708_PINS`, already in the
header from Phase 0), with `ERR_PIN_INVALID` gating I/O until then; clause 7's
`_CH1`..`_CH128` variants let up to 128 same-protocol channels run
simultaneously on vendor-specific connectors, for J2534-1 and J2534-2
protocols alike.

The plan's own Phase 2 roadmap row flags this explicitly as a genuine open
design question, not a mechanical application of an existing pattern: the
existing `hw_protocol_override` field on `ResourceDef` (the SAE J2610 SCI
precedent, ADR-069) is a **static**, per-row override baked in at
table-definition time — one dedicated row per fixed wiring configuration. It
has no mechanism for a caller to select a value dynamically at connect time,
and 128 static resource-table rows per protocol (one per `_CHx` channel) was
already flagged in the plan as not a workable extension of that pattern.

This ADR was produced via a `design-advisor` consultation (grounded in a
`code-scout` fact-gathering pass, then independently verified against the
actual code before being accepted here) per the plan's explicit escalation
note and this repo's cost-policy rule 3 (protocol interpretation,
ADR-worthy architecture choice).

## Decision

### 1. Neither feature gets a new `ChannelProtocol` variant, resource-table row, or ComParam

Per §5 step 3's carve-out (confirmed correct against clauses 6.1/7 directly):
`_PS`/`_CHx` change *where* the same D-PDU application protocol runs (which
pins, which vendor-connector channel index), not *what* protocol it is. A new
`ChannelProtocol` variant per `_PS`/`_CHx` combination would multiply the
ComParam-support tables (ADR-027/028) 129× for no semantic gain. A dedicated
resource-table row per `_CHx` index (128 per protocol) was already rejected
in the plan for the same reason ADR-069's static-table design doesn't fit an
enumerable-but-caller-selected value. No SCONFIG parameter here has an
ISO 22900-2 ComParam equivalent (ADR-027/028 step 5's gate for a table entry
is unmet) — `CONFIG_J1962_PINS` is invoked purely internally, never exposed
as a ComParam.

### 2. Pin Selection rides the existing typed-pin resource-resolution mechanism, resolved and applied entirely inside `ConnectComLogicalLink`

ISO 22900-2 already models a caller's desired DLC pins as resource identity,
not as a ComParam: `CreateComLogicalLink`'s `ResourceData.dlc_pin_data`
(`vci-service-interface/src/proto/service.proto`, a `repeated PinData`) is
the same typed-pin mechanism `names.rs::retain_rows_matching_pin` already
uses to disambiguate an otherwise-ambiguous resource name (e.g. the four
`SAE_J2610_SCI` configurations). This is exactly clause 6.3.3.2's contract
shape: pins are bound once, at channel setup, immutable until teardown, one
`SET_CONFIG` call, `ERR_CHANNEL_IN_USE` on a second attempt.

Phase 2a's resolution extension: when `dlc_pin_data` is supplied and differs
from the matched row's default `dlc_pins`, the connect path resolves to that
row's `_PS` hardware protocol variant (a new small `ps_protocol_id(base) ->
Option<u32>` lookup, since `_PS` IDs are not contiguous with their base IDs
the way `_CHx` IDs are with each other) instead of the base native protocol,
and computes a `pin_select: u32` bitmask (`0x0000PPSS` — `PP` the HI/K/TX/PLUS
pin, `SS` the LOW/L/RX/MINUS pin, `SS = 0` for single-wire) from the supplied
`PinData` list. When `dlc_pin_data` is absent or exactly matches the row's
default pins, resolution is unchanged (base protocol, no `_PS`) — clause
6.3.3.2 itself specifies this as the base protocol's backward-compatible
auto-connect-to-default-pins behavior.

`rpc_connect_com_logical_link` (`rpc_link.rs`), which already defers the
native `PassThruConnect` call to this RPC (not `CreateComLogicalLink`),
performs the full sequence atomically inside the existing hardware-failure
rollback (disconnect-on-any-subsequent-failure) it already has for ComParam
application and filter installation: `PassThruConnect(_PS variant)` ->
`PassThruIoctl(SET_CONFIG, CONFIG_J1962_PINS = pin_select)` -> the rest of
the existing connect sequence. **The client never observes an
unpinned-but-connected channel** — clause 6.3.2.3–6's `ERR_PIN_INVALID`
gating window is entirely internal to this one RPC call and needs no
client-visible state modeling. A native failure at either the connect or the
pin-`SET_CONFIG` step maps through the existing Phase 0 error mappings
(`ERR_PIN_IN_USE`, `ERR_NOT_SUPPORTED`, etc.) and fails the RPC.

**Channel identity must widen to keep same-pins vs. different-pins links
correctly shared or separated:** `ChannelKey` (`service.rs`, currently
`(u32, u32)` = `(j2534_protocol_id, baud_rate)`) becomes `(u32, u32, u32)` =
`(hw_protocol_id, baud_rate, pin_select)`, with `pin_select = 0` for every
non-`_PS` link (preserving today's sharing behavior exactly for every
existing protocol). Two `_PS` links on the *same* pins still share one
physical channel (a joiner skips the pins-`SET_CONFIG`, since the channel
already has it applied); two `_PS` links on *different* pins get distinct
physical channels — required by clause 6.3.2.1, which explicitly allows
multiple simultaneous `_PS` opens differing only in pin assignment.
`LogicalLinkState` gains a `pin_select: Option<u32>` field (`None` for
non-`_PS` links) alongside the existing `hw_protocol_id`.

**Scope-narrowed for this phase:** only the J1962 connector (the seven
protocols clause 6.3.1's Table 1 lists: `J1850VPW`, `J1850PWM`, `ISO9141`,
`ISO14230`, `CAN`, `ISO15765`, and their `_PS` forms) is implemented.
`J1939_PINS`/`J1708_PINS` (the J1939-13/J1708 connectors) are deferred — see
Consequences.

### 3. Additional Channels: a new `channel_index` resource field, arithmetic hardware-ID mapping

`ResourceData` gains one new optional field, `channel_index: uint32` (0 or
absent = the base channel; 1..=128 selects `_CH1`..`_CH128`). A channel index
is hardware-resource identity needed *before* connect, not protocol
behavior — it doesn't fit a ComParam (settable/changeable post-create) any
better than pin selection does, and the honest ISO 22900-2 answer (one MDF
resource ID per physical channel) is the same 128-static-rows-per-protocol
non-starter §5 step 3 already rejected. `(existing resource row,
channel_index)` is the adapter-private equivalent of that per-resource
enumeration, kept out of `GetResourceIds` advertising (ADR-152 Decision 1's
static-advertising rule is unaffected: nothing new is advertised, a caller
must already know which index it wants).

Every `_CH1`..`_CH128` family confirmed (CAN, J1850VPW, ISO15765; the same
generation pattern applies to every other protocol Phase 0 added) is a
contiguous 128-value block (e.g. `PROTOCOL_CAN_CH1 = 0x9000` ..
`PROTOCOL_CAN_CH128 = 0x907F`), so the hardware ID is computed arithmetically
— `chx_protocol_id(base, idx) = chx_base(base) + idx - 1` — rather than via a
128-entry lookup table per protocol. `channel_index > 0` combined with a
non-default `dlc_pin_data` is rejected `invalid_argument`: clause 7's `_CHx`
channels live on vendor connectors, never on J1962/J1939/J1708 pins, so the
two selectors are mutually exclusive. `_CHx` needs no `ChannelKey` change —
the hardware ID itself already differs per index, so two different indices
naturally get distinct physical channels under the existing 2-of-3-field
sharing key.

### 4. Gating and Discovery consumption

Both features are gated on the connecting module's `pname` carrying the
`"J2534-2:"` prefix (clause 5), reusing `service/discovery.rs`'s existing
`is_j2534_2_opted_in` helper (ADR-153) — a module not opted in is rejected
before any `_PS`/`_CHx` resolution is attempted, consistent with clause 5's
"J2534-1-only behavior must be assumed" default. This phase is also the
first real consumer of ADR-153's previously-uncalled discovery cache: for
`_PS`, `discovery_device_info` is queried for the relevant
`DEVICE_INFO_<protocol>_PS_J1962` parameter (caller's pin bitmask as input)
before connect, refining the native connect error with a clean rejection
when Discovery already knows the pin is invalid; for `_CHx`,
`discovery_protocol_info`'s `PROTOCOL_INFO_MAX_AD_ACTIVE_CHANNELS`-style
capacity data is consulted where cached, falling back to the connect-time
native error (`ERR_RESOURCE_IN_USE`/`ERR_NOT_SUPPORTED`) otherwise — matching
ADR-153 Decision 1's "Discovery refines, native error is the fallback"
design exactly as anticipated there.

### 5. Sub-staged delivery (ADR-155)

Delivered as two independently-mergeable PRs, each leaving `main`
self-consistent: **Phase 2a (Pin Selection)** first — service-only
(resolution extension, `ChannelKey`/`LogicalLinkState` widening, the
internal connect-sequence change, Discovery wiring, mock `_PS` state
machine), no proto change. **Phase 2b (Additional Channels)** second — the
`ResourceData.channel_index` proto field (regenerated bindings,
`docs/rpc-api-guide.md` update), arithmetic mapping, mock, and tests, built
independently of 2a's `ChannelKey` change (2b needs no further `ChannelKey`
widening). No same-change consistency duty spans the two PRs: the
`ChannelKey` widening lands wholly within 2a. 2a is ordered first because
later `_PS`-dependent phases (SWCAN, J1939, TP2.0, …) need the mechanism;
nothing before those needs `_CHx`.

## Corrections (found during Phase 2a implementation)

**Decision 2 scoping — Pin Selection does not extend to a `ProtocolName` that
matches a `resources` table row by canonical ISO 22900-2 name.** Decision 2's
text describes pin-selection resolution generically ("the matched row's
default `dlc_pins`"), which reads as applying uniformly to every resolution
path. In practice, `names.rs::parse_protocol_id_from_resource` only layers
Pin Selection onto the `RscData::ProtocolId` route and the `ProtocolName`
route when the name does **not** match any `resources` table row (the
`map_protocol_name` legacy-alias fallback, e.g. `"can"`/`"iso15765"`). A
`ProtocolName` that matches a table row (e.g. the canonical `"ISO_15765_2"`)
keeps `find_table_row_by_name`'s pre-existing all-or-nothing pin-narrowing
contract (ADR-106/ADR-069) completely unchanged — `dlc_pin_data` there is
identifying a specific, fixed resource configuration, not requesting a
dynamic clause 6 pin choice. This was required to preserve an existing
regression test (`create_com_logical_link_rejects_pin_narrowing_a_unique_name_to_zero_rows`),
which pins the pre-Phase-2 contract that a canonical resource name's
`dlc_pin_data` must match that resource's own wiring or be rejected outright.
Reconciling the two would mean either breaking that fast, synchronous
Create-time validation for canonical-name requests, or fabricating a
per-protocol legal-pin-set table that SAE J2534-2 itself does not define (see
the next correction) — both worse than the scoping split. A caller wanting
genuine clause 6 Pin Selection uses `protocol_id` or a generic alias
`protocol_name` instead of a canonical ISO 22900-2 resource name.

**Decision 2's `pin_select` computation — SAE J2534-2 clause 6.3.3.2's Table 3
defines protocol-independent syntactic constraints on `CONFIG_J1962_PINS`
that must be validated at the service layer, distinct from per-protocol pin
*legality* (which the spec leaves to native J2534-1 hardware validation via
`ERR_PIN_INVALID` at the real `SET_CONFIG` call, and which this service does
not attempt to replicate in a static table).** Table 3 constrains a J1962
`PP`/`SS` byte to the numeric range 0x00–0x10, excludes pins 4, 5, and 16 from
that range regardless of protocol (16 sits at the top of the numeric range
yet is still excluded), and requires `PP != SS` when both are supplied (with
the sole exception being the `0x0000` "no selection performed" sentinel,
which `compute_pin_select` never produces since a wildcard/zero pin number is
already rejected earlier in the same function). `names.rs::compute_pin_select`
enforces all three; it deliberately does not check whether a given pin number
makes physical sense for the specific protocol being connected (e.g. whether
pin 7 is sensible for `CAN`) — clause 6.3.3.2 explicitly punts that
determination to native hardware, surfaced as `ERR_PIN_INVALID`/
`ERR_NOT_SUPPORTED` at connect/`SET_CONFIG` time, not a static enumerable
table this service could invent.

**Decision 2's default-pins comparison — matching the request against a
row's default wiring must compare each pin's typed role, not just which
pin numbers were supplied (found by Codex review, PR #28).**
`resolve_pin_selection`'s original "does `dlc_pin_data` match this
protocol's default pins" check compared only the *set* of requested pin
numbers against the default set, discarding each pin's `HI`/`LOW`-style
type entirely. Two consequences: a caller requesting the same pin numbers
as the default but with roles swapped (e.g. CAN's default 6=`HI`/14=`LOW`
requested as 6=`LOW`/14=`HI`) was silently treated as "default, no Pin
Selection" and connected with the *original* (unswapped) polarity instead
of resolving to `CAN_PS` with the caller's actually-requested wiring; and
malformed `dlc_pin_data` with duplicate or extra entries whose
deduplicated numbers happened to equal the default set bypassed
`compute_pin_select`'s own validation entirely, since the number-set match
short-circuited before that validation ever ran. Fixed by
`dlc_pin_data_matches_defaults`, which requires an exact pin count match,
no duplicate requested numbers, every requested number present in the
default set, and — for a pin the caller explicitly typed — that type
matching the default's type for that specific pin number (an untyped
requested pin is allowed to match any default type, preserving clause
6.3.3.2's auto-connect-to-default-pins behavior for a caller that supplies
bare pin numbers).

**`compute_pin_select`'s single-pin branch skipped type resolution entirely
(found by Codex review, PR #28 — the 5th gap surfaced in this pipeline).**
The single-supplied-pin case (`SS = 0`, a single-wire protocol per clause
6.3.1's J1850VPW) packed `dlc_pin_number` directly into the primary `PP`
byte without ever calling `resolve_pin_type_id`. Two consequences: an
unrecognized `dlc_pin_type_name` on the lone pin was silently accepted
rather than rejected, since the validating call was never made at all; and
a pin the caller explicitly typed secondary (LOW/L/RX/MINUS) was packed as
if it were primary, misrepresenting the caller's request with no
correcting mechanism downstream (the later `SET_CONFIG` call carries only
the packed bitmask, not the original per-pin type). Fixed by resolving the
lone pin's type the same way the two-pin branch already does: an
unrecognized type name propagates as `Err`; an untyped pin, or one
resolving to `PIN_TYPE_PRIMARY`, packs unchanged; anything else is
rejected.

**Correction to the above (found by Codex review, PR #28, a subsequent
round): the fix's first version rejected only a type resolving to
`PIN_TYPE_SECONDARY`, a block-list that still let a recognized
non-communication role (`IGN`/`IGNITION_CLAMP`, `SINGLE`, `PROGV`) or an
arbitrary numeric type id pass through and be packed as primary — none of
these are "secondary" by the block-list's own definition, but none are a
legitimate primary role either.** Corrected to an allow-list: reject
anything that isn't untyped or `PIN_TYPE_PRIMARY`, rather than accepting
anything that isn't `PIN_TYPE_SECONDARY`.

**`resolve_pin_selection` never normalized a caller-supplied raw `_PS`
hardware protocol id before doing default-pins/`ps_protocol_id` lookups
(found by Codex review, PR #28, the round after Codex's clean approval of
commit `99d32f4b`).** A caller can name a `_PS` hardware protocol id
directly via `protocol_id` (e.g. `PROTOCOL_CAN_PS`) instead of requesting
non-default pins on the base id; `resolve_protocol_id`'s table fallback
(`ChannelProtocol::from_raw`) preserves that raw id unchanged, since no
`resources` table row exists for a `_PS` id (Decision 1). The original
`resolve_pin_selection` computed `hw_protocol_id` straight from that raw
value with no normalization, so `resources::default_dlc_pins_for_hw_protocol`
and `resources::ps_protocol_id` — both keyed by base J2534-1 ids — were
called with a `_PS` id neither recognizes. Two failure modes resulted: an
empty `dlc_pin_data` (the common case — a caller expecting a `_PS` id to
"just work" like any other protocol id) silently returned `Ok(None)`,
connecting a `_PS` channel with no `SET_CONFIG(CONFIG_J1962_PINS)` ever
issued and its DLC pins permanently unassigned; a non-empty `dlc_pin_data`
instead fell all the way to the `ps_protocol_id` lookup returning `None`,
incorrectly rejecting the request as "no `_PS` variant in scope" for a
protocol that plainly has one. Fixed by normalizing the raw hardware id via
`resources::base_protocol_id` (ADR-157's exact-inverse helper) before any
lookup, and tracking whether normalization actually changed the value
(`already_ps`): an empty `dlc_pin_data` now rejects outright when
`already_ps` (a `_PS` channel this service could never assign pins to is
worse than rejecting it at `CreateComLogicalLink`), and the "matches
defaults, no Pin Selection" short-circuit is skipped entirely when
`already_ps` — a caller who named the `_PS` id directly always gets a real
`pin_select` computed and a real `SET_CONFIG` call, even when its supplied
pins happen to equal the base protocol's usual defaults, since the `_PS`
hardware channel itself starts pin-unassigned regardless.

**Correction to the above (found by `edge-case-hunter`, PR #28, same round):
the fix only covered the `RscData::ProtocolId`/`ProtocolName` routes; the
bare `Resource::ResourceId`/`ResourceName` variants have no `dlc_pin_data`
field at all and never called `resolve_pin_selection`, so a raw `_PS` id
numerically supplied through either still reproduced the original bug (plus
bypassed the opt-in gate and this ADR's `hw_protocol_id` normalization,
since neither lives outside `resolve_pin_selection`).** Fixed by calling
`resolve_pin_selection(protocol, row, &[], j2534_2_opted_in)` from both
arms of `parse_protocol_id_from_resource` — an empty pin slice, since
neither variant has pins to supply — reusing the same `already_ps`
rejection rather than new logic. See ADR-157's own Correction/accepted
residual entries for the full detail, including a narrower SCI
variant-fidelity residual this same fix surfaced.

**Further correction (design-advisor, PR #28, same review round): naming a
`_PS` id directly also left `LogicalLinkState.protocol` itself holding the
raw `_PS` value (`ChannelProtocol::from_raw`), violating the invariant
every Plane B consumer of `link.protocol` relies on and reopening ADR-157's
entire normalization concern at a fourth independent site.**
`parse_protocol_id_from_resource` now normalizes `protocol` to its base
identity at its single return point. See ADR-157's Correction on this
(same round) for the full detail, including why the fix belongs at this
one source point rather than another consumption-site sweep.

**Correction to the above (design-advisor, PR #28, a later round): the
"always compute a real `pin_select` for a directly-named `_PS` id, even
when its pins equal the base defaults" decision (the paragraph beginning
"Fixed by normalizing the raw hardware id..." above) gave one physical
wiring two different `ChannelKey`s.** `ChannelKey = (hw_protocol_id,
baud_rate, pin_select)`: a directly-named `PROTOCOL_CAN_PS` request with
CAN's own default pins (6/HI, 14/LOW) got `(PROTOCOL_CAN_PS, baud,
0x0000060E)`, while an ordinary default-pins `CAN` connect got `(CAN,
baud, 0)` — two keys for the same electrical configuration. Physical
resource sharing (`SharedChannel`) and `same_physical_resource`'s
pin-selection-aware lock-conflict check (this ADR's own earlier
propagation, ADR-157) both key off `ChannelKey`/`hw_protocol_id`+`pin_select`
equality, so the two CLLs opened separate physical channels and a physical
ComParam/TX lock held by either did not protect the other (Codex review, PR
#28). SAE J2534-2 clause 6.3.3.2 also does not allow this coexistence in
the first place — a base-protocol channel already claims its pins at
`PassThruConnect`, and a later `_PS` `SET_CONFIG(CONFIG_J1962_PINS)`
requesting an already-claimed pin is documented there as a rejection
(pin-in-use), not a legal parallel channel. Fixed by removing the
`!already_ps` guard on the "matches defaults, no Pin Selection"
short-circuit: a directly-named `_PS` id whose supplied pins structurally
match the base protocol's own defaults now canonicalizes to `Ok(None)` --
the same ordinary base-identity connect a plain default-pins request gets,
sharing its `ChannelKey` and therefore its physical-resource lock scope.
The empty-`dlc_pin_data` rejection (nothing supplied to compare against
defaults) is unaffected — orthogonal to this correction, since it is about
a request with no pins at all, not one whose pins happen to already match
the defaults. The J2534-2 clause-5 opt-in gate now runs *before* this
short-circuit for a directly-named `_PS` id specifically (a new
`already_ps && !j2534_2_opted_in` check ahead of the defaults comparison),
so opting out of J2534-2 still rejects a `_PS`-named request even when its
pins would otherwise canonicalize — naming a `_PS` id at all is using
J2534-2 vocabulary, independent of what its pins turn out to be worth.
`already_ps` with no known default pins cannot arise in practice (every
`_PS`-scoped hardware id has a matching `default_dlc_pins_for_hw_protocol`
entry, kept in sync with `ps_protocol_id` by construction, and no
`resources` table row's `hw_protocol_override` is ever a `_PS` id, so
`already_ps` only arises via the table-less `from_raw` fallback); if a
future `_PS`-scoped addition ever broke that invariant, resolution falls
through to the non-default-pins path rather than silently canonicalizing
against the wrong (missing) defaults. **Accepted residual:** a J2534
adapter that somehow supports `CAN_PS` but not plain `CAN` is not a
configuration either J2534-1 or J2534-2's protocol model describes, so no
attempt is made to special-case it.

## Consequences

- `ADR-023`'s "Physical channel sharing is preserved: `ChannelKey =
  (j2534_protocol_id, baud_rate)`" bullet is superseded by Decision 2's
  3-tuple shape as of Phase 2a; every other part of ADR-023 (the
  `ChannelProtocol` newtype, its encoding, call-site discipline) is
  unaffected and remains in force.
- `ADR-069`'s static resource-table resolution flow gains a second path
  (Decision 2/3's `_PS`/`_CHx` resolution) that runs alongside, not instead
  of, the existing table lookup — a resource with no pin/channel-index
  qualifier resolves exactly as before. The table itself (`RESOURCE_TABLE`,
  its 37 rows) is unmodified by this ADR.
- **Accepted residual, deferred:** `J1939_PINS`/`J1708_PINS` (the J1939-13
  and J1708 connectors) are out of scope for Phase 2 — ISO 22900-2's
  `PinData` has no connector discriminator (pin numbers are
  connector-relative), so representing "pin 3 on the J1939-13 connector"
  distinctly from "pin 3 on J1962" needs a connector qualifier this ADR does
  not resolve. Tracked for whichever of Phases 5 (J1939) or 11 (J1708)
  reaches its own §4.4 ISO 22900-2 mapping research.
- **Accepted residual:** non-J2534-1 `_PS` protocols (SWCAN, J1939, TP2.0,
  GM UART, UART Echo Byte, Honda DIAG-H, J1708) are not implemented by
  Phase 2a — only the seven clause-6.3.1 Table 1 base protocols are. Each
  later phase that adds one of those protocols reuses this same mechanism
  (the `ps_protocol_id`/`pin_select`/`ChannelKey` machinery), not a new one.
- **Accepted residual:** `GetResourceIds` never enumerates vendor `_CHx`
  channels — a caller must already know which index it wants (static
  advertising, ADR-152 Decision 1). `CONFIG_ACTIVE_CHANNELS` (a
  device-wide diagnostic SCONFIG parameter, distinct from per-channel
  selection) is not surfaced by this phase; likely never warrants it.
- No client-observable behavior changes for any existing (non-`_PS`,
  non-`_CHx`) resource — `pin_select = 0`/no `channel_index` reproduces
  today's exact resolution and connect sequence.
- **Accepted residual, deferred:** Decision 4's Discovery pre-check
  (`discovery_device_info` queried for `DEVICE_INFO_<protocol>_PS_J1962`
  before a `_PS` connect) was not implemented by Phase 2a. `discovery_device
  _info`'s existing wrapper (ADR-153) hardcodes its `SPARAM` input value to
  `0` and caches by `(module_handle, parameter)` only — neither can carry
  the caller's actual pin bitmask as input, so wiring this in as originally
  described would require widening the Discovery cache key first, a change
  beyond a straightforward call-site addition. `ConnectComLogicalLink` falls
  through to the native connect + native error mapping in every case today,
  which is ADR-153 Decision 1's own documented fallback path, so this is not
  a functional gap for the mock's current behavior — only a missed
  optimization (a less-clean native error instead of a synchronous
  pre-check rejection). Tracked here for whichever future work wires it in.
- **Accepted residual, minor:** no mock error-injection hook exists for
  `IOCTL_SET_CONFIG` (unlike `PassThruReadMsgs`/`WriteMsgs`/`FastInit`/stop-
  filter/prog-voltage, which all have one), so the pin-`SET_CONFIG`-fails
  rollback path in `connect_new_physical_channel` is untested. This mirrors
  a pre-existing gap: the sibling `apply_j2534_params`/`install_pass_all_
  filter` failure-rollback branches in the same function are equally
  untested today, for the same reason. Phase 2a adds a third untested
  instance of an existing pattern rather than a new one; fixing all three
  together (one shared error-injection mechanism) is better scoped as its
  own follow-up than folded into this ADR.
- **Accepted residual, minor:** a `dlc_pin_data` entry with `dlc_pin_number
  == 0` (a type-only wildcard — legal in the pre-existing `retain_rows_
  matching_pin` narrowing convention) can never equal a real protocol's
  default pin-number set, so on the `ProtocolId`/generic-alias-`ProtocolName`
  route it now always reads as "requesting non-default pins" and is rejected
  by `compute_pin_select`'s concrete-pin-number requirement. Before Phase
  2a, `dlc_pin_data` on this route was never inspected at all, so this
  caller-visible input was previously silently ignored rather than
  rejected. This is treated as the intended, arguably better behavior (a
  caller supplying a wildcard pin on this route now gets clear feedback
  instead of having its input silently dropped) rather than a defect, but
  is recorded here since Decision 2's original text didn't discuss it.

## Corrections (found during Phase 2b design review)

**Decision 4 correction (Phase 2b design review): `_CHx` capacity discovery
comes from `GET_DEVICE_INFO`, not `GET_PROTOCOL_INFO`.** Decision 4 cited
`PROTOCOL_INFO_MAX_AD_ACTIVE_CHANNELS`-style data for `_CHx` capacity; that
parameter (clause 25.3.2.3 Table 114) belongs to the Analog Input feature
(clause 10) — its applicability is limited to the analog-input protocol
entry and its value mirrors the analog `ACTIVE_CHANNELS` bitmap — and says
nothing about clause 7. The actual clause-7 discovery signal (§7.1, via
clause 25.3.2.2 Table 111) is the per-family `DEVICE_INFO_<PROTOCOL>_
SUPPORTED` parameter, whose packed value's `QQ` byte (bits 16-23) carries
the count of available `_CHx` ids (the adjacent `RR`/`SS` bytes, bits 8-15
and 0-7, carry the `_PS` channel count and base-id support flag
respectively — a first Phase 2b implementation pass extracted bits 8-15
instead, a genuine byte-position bug caught by Codex review, PR #29, that a
matching mis-packing in the mock had concealed from testing). Because this
parameter takes no input
value, ADR-153's existing `discovery_device_info(module_handle, parameter)`
cache serves it unmodified — unlike the `_PS` pin precheck deferred above —
so Phase 2b implements the precheck: a `channel_index` above the cached
count is rejected synchronously (a count is read as licensing contiguous
indices 1 through that count, the only set a count can express); with no
cached value, the native connect error remains the fallback per ADR-153
Decision 1. Implemented as `J2534Service::check_chx_capacity`
(`service/discovery.rs`), called from `rpc_connect_com_logical_link`'s
brand-new-physical-channel path, immediately before the native
`PassThruConnect`.

**Decision 3 addendum (Phase 2b design review): `_CHx` reuses ADR-157's
single normalization funnel and inherits its Plane B sweep; direct `_CHx`
naming is canonicalized at resolution.** `resources::base_protocol_id` gains
a range-based tier (`chx_base_protocol_id`: the seven in-scope contiguous
128-id blocks map to their base id; `PROTOCOL_J2610_CHx` collapses to the
same representative SCI id as `PROTOCOL_J2610_PS`, same accepted residual),
so every ADR-157 Plane B site and every `link.protocol` consumer (via
`parse_protocol_id_from_resource`'s source-point normalization) handles
`_CHx` without a new sweep. Planes A and C keep the raw `_CHx` id (clause 7
requires message `ProtocolID` to equal the connect-time id; no `ChannelKey`
change, as already decided). A caller naming a `_CHx` id directly via
`protocol_id` (or a bare `ResourceId`/`ResourceName`) is decomposed to (base
id, index) at resolution (`names::resolve_channel_selection`) and proceeds
identically to the `channel_index` route — accepted, since the id is
self-describing, unlike a bare `_PS` id's missing pins. Unlike `_PS`, no
`_CHx` index canonicalizes to the base id: clause 7 defines no
base-equivalent index (vendor-connector mapping is opaque) and assigns
base/`_PS`/`_CHx` hardware overlap to the native layer's resource-conflict
error, so distinct ids correctly keep distinct `ChannelKey`s. Mutual
exclusion extends to the direct route: a `_CHx`-named id with non-default
`dlc_pin_data`, or with an explicit `channel_index` (even an agreeing one),
is rejected `invalid_argument`. Any id in the full clause-24 `_CHx` region —
including the eleven out-of-scope family blocks — triggers the clause-5
opt-in gate before resolution; out-of-scope blocks then reject cleanly
rather than passing through as unknown extended ids. `LogicalLinkState`'s
`pin_select_base_hw_protocol_id` generalizes to a qualifier-agnostic
base-override field (`base_hw_protocol_override`) gated on its own
presence, populated by both the `_PS` and `_CHx` routes. The
`pin_select`-conditioned gates ADR-157 added (software-ISO-TP/dual-channel
skips with their paired point-to-point-filter condition, J1850 flavor
write-back and cache bypass) widen to "any qualifier present"; `_CHx` links
are hardware-ISO-TP only and get no companion channel, mirroring the `_PS`
residuals. A `_CHx` link occupies its base resource in `GetResourceStatus`
(ADR-157 item 6's consistency rule); a literal `_CHx`-id query matches only
its exact index, and a literal query in the J2610 `_CHx` block matches any
SCI variant at that same index (extending the existing
`PROTOCOL_J2610_PS` literal-query broadening). Per-index occupancy
granularity for base-resource queries is an accepted residual.

**Correction (Codex review, PR #29): `channel_index` does NOT share Decision
2's canonical-`ProtocolName` scoping exclusion.** The first Phase 2b
implementation pass mirrored Decision 2's correction above too literally,
dropping `channel_index` (returning no Additional Channels resolution at
all) whenever `RscData::ProtocolName` matched a `resources` table row by
canonical ISO 22900-2 name (e.g. `"ISO_15765_2"`) — silently connecting the
base channel instead of the requested `_CHx` index, bypassing the clause-5
opt-in/mutual-exclusion/index-range checks entirely. Decision 2's exclusion
has a specific structural justification that does not carry over:
`dlc_pin_data` is itself an input to `find_table_rows_by_name`'s own
row-matching (the "all-or-nothing pin-narrowing contract" it preserves), so
once a canonical name resolves to a specific row, `dlc_pin_data` has already
done its job and re-running it through `resolve_pin_selection` would be a
category error. `channel_index` is never passed to `find_table_rows_by_name`
at all — it plays no role in canonical-name resolution — so there is no
equivalent "already did its job" reason to drop it. Fixed by still calling
`names::resolve_channel_selection` with the row-matched route's resolved
`protocol`/`row` for a canonical-name request, leaving only `pin_selection`
(never `channel_selection`) forced to `None` on this route.

**Correction (edge-case-hunter, PR #29, post-approval pass): the clause 6/7
mutual-exclusion check keyed on Pin Selection's *resolved outcome*, not on
whether clause-6 vocabulary was used, so a directly-named `_PS` id whose
`dlc_pin_data` equals the base defaults — which canonicalizes to `Ok(None)`
per the PR #28 correction above — combined with a nonzero `channel_index`
was silently accepted and connected as a `_CHx` channel.** The caller named
two irreconcilable connect identities (a clause-6 `_PS` ProtocolID and a
clause-7 channel; a native connect takes exactly one ProtocolID), and the
service silently chose the clause-7 reading — the same silent-disagreement
trap the double-qualification rejection above already refuses for the
*agreeing* case, and a violation of the PR #28 principle that naming a `_PS`
id is clause-6 vocabulary use regardless of what its pins canonicalize to.
Fixed by widening `resolve_channel_selection`'s mutual-exclusion rejection
to also trigger on a `_PS`-shaped raw id, detected via a new direct
`resources::is_ps_protocol_id` predicate (a finite match over the seven
in-scope `_PS` constants, now also used for `resolve_pin_selection`'s own
`already_ps` in place of the earlier "normalization changed the id and it
isn't `_CHx`" inference, which had already needed one compensating term when
the normalization funnel's domain grew and would silently misclassify again
on the next growth). Deliberately **not** rejected: default-matching
`dlc_pin_data` alongside a `channel_index` on a base-id or canonical-name
request — supplying pin data is ISO 22900-2's generic typed-pin resource
field, not clause-6 vocabulary (clause 6.3.3.2 defines default-matching pins
as an ordinary no-selection connect, and pins also serve pre-existing row
disambiguation, e.g. the SCI configuration rows, which must remain
combinable with `channel_index`). Structural note: `resolve_pin_selection`'s
canonicalized `Ok(None)` intentionally erases the vocabulary signal (that
erasure is what unifies `ChannelKey`/lock scope); any consumer needing "was
clause-6 vocabulary used" must use `is_ps_protocol_id` on the raw id, never
infer it from a `None` resolution outcome — audited: the only such consumer
was this mutual-exclusion check (`resolve_pin_selection`'s own opt-in gate
runs before canonicalization, `LogicalLinkState` population treats `None` as
its intended outcome, and the source-point `protocol` normalization is keyed
on the raw id's shape already, not on `pin_selection`; the bare
`ResourceId`/`ResourceName` routes cannot reach canonicalization at all,
since an empty pin slice on a `_PS` id rejects before it ever runs).

**Accepted residual (Phase 2b, `_CHx` × SAE_J1850 flavor auto-detect,
ADR-070/ADR-157): a `channel_index`-qualified J1850 link's flavor probe
still only knows how to apply `_PS`'s J1962-pin `SET_CONFIG`, never a
vendor-connector index, to its candidate channels.** The module-wide
`j1850_bus_flavor` cache bypass (ADR-156/157 Bug B) widens to any qualifier
present (so a `_CHx` link's probe result never pollutes, or is satisfied
by, the shared cache — a distinct physical resource, same reasoning as
`_PS`), and the post-probe write-back correctly threads the detected
flavor back through `resources::chx_protocol_id` (mirroring the existing
`ps_protocol_id` write-back) so `hw_protocol_id`/`base_hw_protocol_override`
stay the connect-time `_CHx` variant. The probe *itself*
(`probe_sae_j1850_flavor`) is unchanged for `_CHx`: clause 7's
vendor-connector routing is opaque to this service, unlike clause 6's DLC
pins, so there is no equivalent "probe on the caller's selected connector"
mechanism to build — a `_CHx`-qualified J1850 link's probe exercises the
connect candidate's default pins, same as an unqualified probe would.
Tracked here rather than built out now, mirroring this ADR's own
"Software-ISO-TP × Pin Selection interaction remains unimplemented"
residual's precedent.
