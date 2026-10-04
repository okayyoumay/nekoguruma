# ADR-178: Freeze the J2534-2 D-PDU Proto Interface — Revert Additions Since ADR-105, Route via Existing Generic Mechanisms

**Date:** 2026-08-14
**Status:** Accepted
**Affects:** `vci-service-interface` (proto), `j2534-0404-service` (resources, names, rpc_link,
rpc_misc, rpc_primitive, comparam_support, service_params), `j2534-0404-mock`,
`docs/j2534-2-support-plan.md`, ADR-156, ADR-165, ADR-177 (partial supersession — see each
ADR's own Status line)

## Context

Every J2534-2 phase shipped since the rich-error-model baseline (`8a76116`, "Remove
GetLastError RPC; adopt rich error model", ADR-105) that needed to carry new client-facing
data has done so by growing `vci-service-interface/src/proto/service.proto`: Additional
Channels added `ResourceData.channel_index` (clause 7, ADR-156 Decision 3); Repeat Messaging
added `message IORepeatMessageSetup` + `DataItem.repeat_message_setup` (clause 14, ADR-165);
Device Configuration added `message IODeviceConfigEntry`/`IODeviceConfigList` +
`DataItem.device_config_list` (clause 18, ADR-176); Analog Inputs added
`CreateComLogicalLinkRequest.analog_sample_rate` (clause 10, ADR-177). By direction, all four
additions are reverted and the client-facing capability each carried is re-expressed through
mechanisms that already existed in the proto file at `8a76116` — no new proto message or field
is introduced anywhere in this ADR's own Decision. The same constraint governs every remaining
and future J2534-2 phase from this point forward: `service.proto` does not grow again for a
new protocol capability.

A survey of the current `DataItem` oneof against its `8a76116` shape (10 pre-existing variants,
tags 1-10) found `bytearray_data` (`IOBytearray { bytes data = 1 }`, tag 3) already present as
a generic opaque-bytes carrier reachable from any `IoCtl` RPC. `CreateComLogicalLinkRequest`
and `ResourceData`, by contrast, had no equivalent generic carrier at `8a76116` — their one
raw-bytes field, `cll_create_flag_raw`, already carries a distinct, real ISO 22900-2 native
meaning (`pCllCreateFlag`'s checksum-mode/raw-mode bits) unrelated to resource selection, so
repurposing it would conflate two unrelated concerns rather than genuinely reuse a generic
mechanism. The ComParam surface (`SetComParam`/`GetComParam`, `comparam_id.rs`) is the other
side's actual generic, proto-schema-stable extension point: a new ComParam id is a Rust-side
numeric-id-plus-allowlist-entry addition, never a `.proto` change, matching this same
proto-freeze constraint by construction.

This ADR's mechanism decisions for `channel_index` and `analog_sample_rate` were reviewed by a
`design-advisor` consult (the ordering-conflict risk between "resolved during
`CreateComLogicalLink`" vs. "ComParam RPCs operate on an already-created CLL" made this a
genuine protocol-interpretation/connect-sequencing question, not a mechanical swap). A second
`design-advisor` consult (Codex review, PR #67) corrected this ADR's own initial premise for
`channel_index` — see the Decision below and the "Correction" note in Consequences.

## Decision

**Additional Channels (`channel_index`): removed as a proto field; re-expressed through the
pre-existing direct-`_CHx`-id-naming route, plus a compound-name grammar in the existing
`string`-typed resource fields for the one family that route alone cannot fully cover.**

This ADR's first-drafted premise — that the pre-existing direct-id route (naming a raw
`_CH5`-style native id directly via `ResourceData.protocol_id`, already present at `8a76116`)
"already decomposes into the identical `(base, chx_id, index)` tuple the `channel_index` field
produced" — held for six of the seven in-scope clause-7 protocol families (J1850VPW, J1850PWM,
ISO9141, ISO14230, CAN, ISO15765), each of which has exactly one native hardware id per base
protocol, so a bare `_CHx` numeric id is fully self-describing. It did **not** hold for the
seventh, **SAE J2610 (Chrysler SCI)**: this family has four distinct native hardware ids
(`SCI_A_ENGINE`/`SCI_A_TRANS`/`SCI_B_ENGINE`/`SCI_B_TRANS`) that all collapse onto one shared
`_CHx` numeric block (`resources::chx_block_base`), matching the spec's own Table 1
consolidation note for this family (the same collapse `_PS` already has for SCI). A bare numeric
`_CHx` id therefore cannot express *which* SCI variant a caller means — `chx_base_protocol_id`,
the inverse mapping, always decomposes to the `SCI_A_ENGINE` representative, no matter what was
actually intended. The original `channel_index` field route avoided this because the caller
supplied the exact SCI variant *separately*, via the resource selector (`resource_row`/
`protocol`), independent of the (always-ambiguous) numeric `_CHx` arithmetic used only for the
native `PassThruConnect` call — a distinction this ADR's first draft missed, having reasoned
about `_CHx` resolution as if every in-scope family had the CAN-shaped one-native-id-per-base
property the majority of them do.

**Fix**: `names.rs::resolve_channel_selection` regains an index-driven resolution arm (a
`requested_index: Option<u32>` parameter, functionally the field route's own logic, minus the
proto field) alongside the still-primary directly-named-`_CHx`-id route. It is fed by a new
compound-name grammar, accepted only in the two existing `string`-typed input routes
(`CreateComLogicalLinkRequest.ResourceName`, and `RscData.protocol_name` inside the `RscData`
variant) — never in the numeric `ResourceId`/`protocol_id` routes, which stay purely numeric,
unchanged: `"<resolvable-name>_CH<n>"` (case-insensitive, `n` decimal `1..=128`), tried **only
after** whole-string resolution against the existing table/alias/numeric routes fails. A plain
`"SCI_B_TRANS"` still resolves exactly as it always has; `"SCI_B_TRANS_CH3"` is new — the head
resolves through the *same* existing name-resolution call, which preserves the caller's exact
SCI variant selection (independent of the numeric `_CHx` id, exactly like the removed field
route did), and the trailing digits supply the requested Additional-Channel index. Every guard
the field route enforced (clause-5 opt-in gating, mutual exclusion with Pin Selection and with a
directly-named `_PS` id, double-qualification rejection when a caller supplies both a compound
name and a directly-named `_CHx` id, and the `1..=128` range/family-scope check) is reinstated
on this route, adapted to the new `Option<u32>` shape (no proto3 zero-sentinel exists anymore).

`GetResourceIds`/`ChannelKey`/`check_chx_capacity` semantics are unaffected, since resolution
still happens entirely at `CreateComLogicalLink` time either way — nothing about *when*
resolution happens changes, only which of the (now three) input routes a caller uses. The
underlying `resources::chx_protocol_id`/`chx_base_protocol_id` arithmetic is untouched.

**Repeat Messaging and Device Configuration: re-expressed as hand-packed binary payloads inside
the pre-existing `DataItem.bytearray_data` (`IOBytearray`) carrier**, decoded by a dedicated
serialize/deserialize helper pair per feature. The surrounding IOCTL dispatch
(`io_ctrl_command_id`) already disambiguates which decoder applies to a given payload — no
self-describing type tag inside the bytes is needed, which is why the heavier,
already-present-but-unused `ParamVendorSpecificStruct` (`type_url`/`size_of_entry`/
`count_of_entry`/`bytes value`, base.proto — spec-defined ISO 22900-2 Structfield shape,
currently rejected for every ComParam id by `comparam_support.rs`) is not the right tool here:
it solves a self-description problem this call site doesn't have, while introducing a routing
mismatch of its own (it hangs off the ComParam RPC path, not `DataItem`/`IoCtl`).

Byte layouts (little-endian throughout; both documented as an explicit table in
`docs/rpc-api-guide.md`, since real clients must now hand-implement this packing rather than
use generated protobuf bindings — see Consequences):

- **Device Configuration** (`SET`'s input; `GET`'s input and output — one shape serves both
  directions, mirroring how the removed proto message did): `u32 entry_count`, then
  `entry_count` × `{u32 parameter_id, u32 value}`.
- **Repeat Messaging** (`START`'s input only — `START`'s returned `MsgId` and `QUERY`'s
  returned status both already fit the pre-existing `unum32_value`, tag 1, unchanged): `u32
  time_interval`, `u32 condition`, then three length-prefixed byte spans (`u32 len` + that many
  bytes each) for `repeat_msg_data`/`mask_data`/`pattern_data`, then `u32 tx_flag_bits_count`
  followed by that many `u32`s (each a `TxFlagBit` enum's raw wire value).

**Analog Inputs (`analog_sample_rate`): removed as a request field; re-expressed as a new
ComParam, `CP_AnalogSampleRate` (id `0x80C4`, the next free service-level id after `0x80C3`),
staged via `SetComParam` and resolved at `ConnectComLogicalLink` time — mirroring the
already-established CAN FD precedent (ADR-158) of inferring a connect-time hardware behavior
from a staged Working ComParam rather than a request-time field.** `CP_AnalogSampleRate` is
explicitly documented as project-invented with no ISO 22900-2 source — the same "first mint"
question ADR-170 (UART Echo Byte's `UEB_T*` parameters) and ADR-176/177 (Device
Configuration's/Analog Inputs' own remaining native-only parameters) each deliberately
deferred, now resolved for this one value specifically because ADR-177 already established
that `SAMPLE_RATE` cannot be left at its native default the way those other parameters can (a
zero default disables the acquisition subsystem entirely — a broken feature, not merely an
unconfigurable one), so once its dedicated proto field is removed, a ComParam is the only
remaining schema-stable channel for it.

Mechanics:
- `CreateComLogicalLink` resolution for an `ANALOG_IN_x` resource is unchanged — each of the 32
  rows already names a fully specific native id, with none of `_CHx`'s "which id" ambiguity, so
  there is no equivalent ordering conflict to resolve here.
- `comparam_support.rs`'s `ANALOG_IN` family allowlist (currently empty by design, per ADR-177)
  gains exactly one allow-arm: `CP_AnalogSampleRate` only, for this family only. Every other
  ComParam and every other family's treatment of this one are unchanged.
- `to_j2534_config_id` returns `None` for `CP_AnalogSampleRate` (service-level only, the same
  shape `PARAM_CANFD_BAUDRATE` already has) — it is never forwarded through the generic
  per-protocol `SET_CONFIG` path; `rpc_connect_com_logical_link`'s existing Working-set snapshot
  reads it directly (replacing the removed request-field read), and
  `connect_new_physical_channel`'s existing `SET_CONFIG(CONFIG_SAMPLE_RATE)` step, unchanged in
  every other respect, applies the snapshotted value.
- **Required-nonzero validation moves from `CreateComLogicalLink` time to `ConnectComLogicalLink`
  time**: an analog CLL with an unset/zero staged rate now fails at Connect, not Create — the
  same deferral shape ADR-158 already normalized for FD-mode detection. The inverse
  rejection (this ComParam set on a non-analog link) is enforced structurally by the allowlist
  itself at `SetComParam` time, needing no separate check.
- **Join-mismatch check moves from comparing live per-CLL state to comparing a recorded
  applied rate on `SharedChannel`.** The existing join-time rejection (a second CLL joining an
  already-open `ANALOG_IN_x` channel at a different rate) used to compare an immutable per-link
  field; a staged ComParam is re-stageable at any time after connect, so comparing two live
  Working values again would let an owner's post-connect re-stage (with no reconnect) silently
  desync the check from what is actually running on the hardware. Instead,
  `connect_new_physical_channel` records the rate it actually applied on a new `SharedChannel`
  field at the moment `SET_CONFIG` succeeds, under the same `logical_links` critical section
  the existing snapshot already uses (one snapshot, one source of truth — no second independent
  Working read to race a concurrent `SetComParam`), and the join check compares against that
  recorded value instead.
- **`CoptUpdateparam` on an already-connected analog link rejects outright** any attempt to
  re-stage `CP_AnalogSampleRate` away from the value actually applied to the connected channel,
  mirroring `handle_update_param`'s existing "reject rather than silently ignore" pattern for
  an FD-mode-affecting ComParam change on an already-connected link
  (`rpc_primitive.rs`'s `fd_mode_staged(&params) != resources::is_fd_protocol_id(hw_protocol_id)`
  guard) — silently ignoring the re-stage would let `GetComParam` report a Working value the
  hardware was never actually reconfigured to match.

**Forward-looking rule for every remaining/future J2534-2 phase** (`docs/j2534-2-support-plan.md`
§5's per-phase execution checklist is amended accordingly): a scalar or a native
id/value-pair-list-shaped (`SCONFIG_LIST`-shaped) capability is represented as a ComParam,
minting a fresh id when genuinely native-only, following this ADR's naming/id-range convention;
a heterogeneous or byte-blob-shaped native structure is represented via
`DataItem.bytearray_data` with a documented internal packing format added to
`docs/rpc-api-guide.md`; a name-shaped qualifier that an existing numeric id cannot
self-describingly carry (the Additional Channels/SCI case above) extends the existing
`string`-typed name-resolution grammar (`ResourceName`/`protocol_name`) with a documented
compound form, rather than adding a new field to carry it separately. `service.proto` itself
does not grow for a new protocol capability.

## Consequences

- **Lost typed schema for Repeat Messaging and Device Configuration.** Their payloads are now a
  hand-packed binary blob rather than a `.proto` message — a real client must hand-implement the
  documented byte layout instead of using generated protobuf bindings. This is the accepted cost
  of never touching the interface again for these two features; it is not free, and is recorded
  here deliberately rather than left implicit.
- **The Additional Channels compound-name grammar is a client-facing string convention with no
  compiler-checked schema of its own** — a real client selecting an SCI variant on an Additional
  Channel must hand-construct `"<name>_CH<n>"` correctly (case-insensitive, exact existing
  canonical name as the head) rather than setting two typed fields the way `channel_index` let
  it. The same "documented convention, not generated bindings" cost shape as the byte-layout
  decisions above, scoped narrowly to the one family that needs it.
- **`CP_AnalogSampleRate` establishes this codebase's precedent for minting a genuinely
  native-only, no-ISO-source ComParam** — the "first-of-its-kind mint" question ADR-170/176/177
  each raised and deferred is now resolved with a concrete id-range convention (next free
  service-level id after the existing max) any future phase facing the same question should
  follow, rather than re-opening the question from scratch.
- **Additional Channels clients using bare `channel_index` today must switch to a directly-named
  `_CHx` id, or (for SAE J2610 SCI specifically) the new compound-name grammar** — a real, if
  mechanical, client-facing change for anyone who had adopted the now-removed field. Unlike the
  other six in-scope families, an SCI client must use the compound-name grammar specifically
  (`"SCI_B_TRANS_CH3"`-shaped), not the bare numeric `_CHx` id, since only the compound-name
  route can still specify which of the four SCI variants it means.
- **Correction (Codex review, PR #67): this ADR's Decision originally claimed the Additional
  Channels revert was a pure deletion with an already-identical replacement tuple for every
  in-scope family.** That was true for six of the seven families but false for SAE J2610 SCI
  (see the Decision section above for the corrected mechanism) — a real regression that shipped
  in this PR's initial commits and was caught by review before merge, not a hypothetical. The
  compound-name grammar closes it; see the Decision section's `resolve_channel_selection`
  description for the reinstated guards.
- **Accepted residual (Codex review, PR #67): no wire-level guard against a stale client still
  sending the removed `channel_index` field.** Deleting proto tag 6 outright, rather than
  retaining it as a `[deprecated = true]` tombstone that the server explicitly rejects, means a
  client still built against the pre-revert schema that sends `channel_index` nonzero has that
  field silently discarded by prost (unknown-field bytes are dropped, not surfaced) — the
  request then resolves as an ordinary base-channel connect instead of failing, which could
  route diagnostic traffic onto the wrong physical channel rather than erroring loudly. Weighed
  against `vci-service-interface/docs/implementation-notes.md`'s own stated field-tag
  backward-compatibility policy and declined as inapplicable *for this specific hazard, in this
  specific deployment*: `vci-service-manager` spawns every service and its clients from one
  colocated build (`vci-service-manager/src/main.rs`), so no independently-versioned client that
  could still carry the pre-revert schema exists today, and this ADR's own governing mandate is
  an exact revert to `service.proto`'s shape at `8a76116` — which had no `channel_index` field to
  guard at all — not a gradual, tombstoned deprecation. If an independently-released client
  package is ever introduced for this service, this hazard needs re-evaluating before that
  release ships, not after.
- **Analog Inputs' connect-time behavior changes observably**: a malformed connect attempt (no
  rate staged) now fails at `ConnectComLogicalLink` instead of `CreateComLogicalLink` — a
  client polling `CreateComLogicalLink`'s own success as "the resource is valid" no longer gets
  that signal for a missing rate specifically; it must call `Connect` to find out.
- **Not addressed by this ADR**: some unshipped future phases (TP2.0's connection-oriented
  lifecycle in particular; possibly GM UART's bus-mastership handshake or Ethernet_NDIS) may
  need genuinely new client-visible RPC-level semantics — a new stream, a new handle concept —
  that neither ComParam staging nor `DataItem.bytearray_data` can express without inventing a
  new RPC method (a different kind of interface change this ADR's own Decision doesn't cover
  either way, since it only addresses *messages and fields*, not *RPC methods*). Those get their
  own `design-advisor` consult when their time comes; this ADR closes the four already-shipped
  cases and sets the default policy for the common (scalar / id-value-list / byte-blob) shapes,
  it does not pre-solve every future one.
