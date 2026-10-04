# ADR-122: Remove the Zero-Mask Pass-All `FLOW_CONTROL_FILTER` Fallback on ISO15765 Channels

**Date:** 2026-07-23
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (`SharedChannel`, `LogicalLinkState`),
             `j2534-0404-service/src/service/rpc_link.rs` (`promote_unique_resp_id_table`,
             `reinstall_iso15765_channel_filters_after_clear`, removes
             `sync_channel_fc_pass_all_filter` and `install_pass_all_fc_filter`),
             `j2534-0404-service/src/service/rpc_misc.rs` (`CLEAR_MSG_FILTERS`),
             `j2534-0404-service/tests/grpc_mock/flow_control_filters.rs`

**Amends:** ADR-039 (removes the fallback its "Decision" section introduced;
its point-to-point filter-derivation logic is otherwise unchanged and remains
authoritative), ADR-048 (this ADR extends its "no unconditional pass-all"
decision from `ConnectComLogicalLink` specifically to the two remaining
lifecycle points — `CoptUpdateparam` promotion and `CLEAR_MSG_FILTERS` — so
the fallback is now gone everywhere, not just at connect time)

## Context

ADR-039 introduced a zero-mask/zero-pattern/zero-flow-control-message
`FLOW_CONTROL_FILTER` as a pass-all fallback for ISO15765 channels: any CLL
sharing a channel that has not (yet) configured a full `CP_CanRespUSDTId` /
`CP_CanPhysReqId` address pair via `SetUniqueRespIdTable` is covered by this
wide-open filter instead of a precise point-to-point one. ADR-039 admitted
this fallback is not spec-conformant: SAE J2534-1 v04.04 (DEC2004) requires
a `FLOW_CONTROL_FILTER`'s mask to be 4 or 5 bytes of `$FF` and to be
point-to-point, matching exactly one CAN ID pair (`pPatternMsg` the ECU's
response ID, `pFlowControlMsg` the tester's physical request ID) — see
`j2534-1-0404/J2534_1_200412 - Recommended Practice for Pass-Thru Vehicle
Programming.md` lines 816-818. A zero mask matches every CAN ID, violating
this outright, and per line 780's uniqueness rule a real point-to-point
filter's IDs must not collide with any other filter's IDs on the channel —
a constraint a wide-open zero-mask filter cannot even participate in
meaningfully.

The service's own conformance audit (`j2534-0404-service/docs/iso22900-2-conformance-audit.md`,
item B4) flagged this as a "Revisit": since ADR-039 itself admits the
non-conformance, should the fallback instead simply not deliver until a CLL
is addressed?

ADR-039's own "Alternatives Considered" #3 — "drop the pass-all fallback
entirely once any `SetUniqueRespIdTable` call is made" — was explicitly
rejected at the time: it "would starve any other CLL sharing the channel
that has not yet configured its own table." ADR-048 later adopted exactly
this alternative, but scoped narrowly to `ConnectComLogicalLink`: a CLL that
connects with an empty table gets no filter of its own at connect, full
stop. ADR-048's own consequences section names the tradeoff explicitly: "A
deployment that never calls `SetUniqueRespIdTable` on an ISO15765 CLL
... now receives nothing from that CLL's own filters — this is a behaviour
change ... accepted."

That left the fallback alive at the two remaining lifecycle points where a
CLL's table changes after connect: `promote_unique_resp_id_table` (run on
`CoptUpdateparam` execution, ADR-068) and
`reinstall_iso15765_channel_filters_after_clear` (run on `CLEAR_MSG_FILTERS`).
Both still installed the zero-mask fallback whenever any CLL sharing the
channel lacked its own point-to-point filter — meaning the fallback's
presence for a given unaddressed CLL depended entirely on whether some
*sibling* CLL happened to trigger a table promotion or a filter clear after
that CLL connected. ADR-048 §"Deliberately not addressed" already documents
one edge of this asymmetry (a stale pass-all filter surviving a new CLL's
connect because `ConnectComLogicalLink` never resyncs it).

The fallback is also unreliable in practice regardless of this
inconsistency: it is only effective against a lenient pass-thru DLL. A
conformant one may reject a zero-mask `FLOW_CONTROL_FILTER` outright per the
spec text above (ADR-039 §Context already notes this), and the service's
own install call treats that failure as best-effort — logged and skipped,
never surfaced to the caller (`rpc_link.rs`, `warn!(...)` on
`install_pass_all_fc_filter` failure). So the fallback's protection was
already conditional on hardware leniency, not something a caller could rely
on.

## Decision

The zero-mask pass-all `FLOW_CONTROL_FILTER` fallback is removed entirely,
from every lifecycle point, not just `ConnectComLogicalLink`:

- `sync_channel_fc_pass_all_filter` and `install_pass_all_fc_filter`
  (`rpc_link.rs`) are deleted.
- `promote_unique_resp_id_table` no longer calls the fallback-sync helper
  after reconciling point-to-point filters; it only installs/removes
  point-to-point filters for the CLL whose table changed (ADR-039's per-entry
  derivation, unchanged) and re-syncs the dual-channel-mode UUDT companion
  channel (ADR-046), unchanged.
- `reinstall_iso15765_channel_filters_after_clear` (run on
  `CLEAR_MSG_FILTERS`) still rebuilds point-to-point filters for every CLL
  sharing the channel from its `active_unique_resp_id_table`, but no longer
  installs a fallback for any CLL left uncovered.
- `SharedChannel::fc_pass_all_filter_id` is removed — there is nothing left
  to track.

An ISO15765 CLL now receives, via its own hardware filter, only what its own
`UniqueRespIdTable` addresses — deterministically, at every point in its
lifecycle (connect, `CoptUpdateparam`, `CLEAR_MSG_FILTERS` alike), matching
the spec's own default: nothing is delivered unless an appropriate
`FLOW_CONTROL_FILTER` matches it (spec line 778), and after `CLEAR_MSG_FILTERS`
the queue accepts nothing further until a `PASS_FILTER` or `FLOW_CONTROL_FILTER`
has been added again (spec line 1388).

**`events.rs`'s `route_frame` is explicitly out of scope and unchanged.**
`route_frame` treats a CLL with an empty `unique_resp_id_table` as "deliver
everything the channel forwards" — this is a software-routing-layer fan-out
policy over whatever hardware actually forwards, not a hardware filter, and
it remains load-bearing for non-ISO15765 protocols (K-line/J1850/raw CAN,
which legitimately use a wide-open `PASS_FILTER` at connect per ADR-048's own
scope note) and for ADR-048's documented shared-channel semantics (a CLL
with no filter of its own still observes frames that another CLL's
point-to-point filter lets through). Narrowing `route_frame` to also require
addressing would break those and was not part of this decision — this ADR is
scoped to the hardware `FLOW_CONTROL_FILTER` fallback only.

No new reject/error path was introduced: `ConnectComLogicalLink` and
`SetUniqueRespIdTable` still never fail due to missing or incomplete
addressing, consistent with every other filter-install failure in this
service being best-effort (ADR-008).

## Consequences

- An ISO15765 CLL that never configures a `UniqueRespIdTable` entry with a
  full address pair — whether it never calls `SetUniqueRespIdTable` at all,
  or clears a previously-configured table — now has no `FLOW_CONTROL_FILTER`
  of its own at any point, not just at connect (ADR-048's tradeoff, now
  applied uniformly). It still observes whatever traffic another CLL sharing
  the same physical channel lets through via that CLL's own point-to-point
  filter(s), through the unchanged `route_frame` broadcast-to-empty-table
  policy — but nothing further.
- The stale-pass-all-filter edge case ADR-048 left open ("Deliberately not
  addressed") is eliminated: there is no fallback filter left to go stale.
- `j2534-0404-service/tests/grpc_mock/flow_control_filters.rs`'s
  `iso15765_set_unique_resp_id_table_installs_point_to_point_flow_control_filter`
  now asserts zero filters remain after a table is cleared and promoted
  (previously asserted the zero-mask fallback reappeared). A new test,
  `iso15765_shared_channel_with_unaddressed_sibling_never_shows_zero_mask_fallback`,
  covers the previously-untested multi-CLL case directly: one CLL addressed
  and promoted, a sibling joining the same channel unaddressed, and a further
  promotion on the first CLL — asserting a zero-mask filter never appears at
  any step and the unaddressed sibling never gets a filter of its own.
- ISO15765 channels are now unconditionally spec-conformant with respect to
  `FLOW_CONTROL_FILTER` shape (mask always `$FF`, always point-to-point) —
  there is no code path left that can install anything else. Conformance
  audit item B4 is resolved.
