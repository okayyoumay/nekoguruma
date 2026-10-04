# ADR-127: GetResourceStatus Surfaces Lock Bits and Creation-Time "In Use" Status

**Date:** 2026-07-24
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`rpc_get_resource_status`)

## Context

Conformance-audit finding A2-3 (`j2534-0404-service/docs/iso22900-2-conformance-audit.md`)
identified two defects in `GetResourceStatus`'s `resource_status` bitfield, neither
previously covered by a decision record:

1. **Bits 2/3 (Transmit Queue Lock Status / Physical ComParam Lock Status) were never
   set.** Table D.1 (ISO 22900-2:2009(E), spec lines 6352-6359) defines bit 2 as "1 = ...
   locked by a CLL" for the TX queue and bit 3 identically for physical ComParams; §9.4.13.4
   (line 1818) and §9.4.14.2 c) (line 1861) both point back to `PDUGetResourceStatus` as the
   mechanism a client uses to observe a lock change. ADR-123 (finding A2-15) already added
   `LogicalLinkState::held_lock_mask`, `LOCK_PHYSICAL_COM_PARAMS`, `LOCK_PHYSICAL_TX_QUEUE`,
   `find_physical_lock_holder`, and `same_physical_resource` for `LockResource`/
   `UnlockResource` conformance, but `rpc_get_resource_status` never reads
   `held_lock_mask` at all.
2. **Bit 0 (Usage Status) required `link.connected`.** §9.4.9.2 c) (spec line 1574) marks a
   resource "in use" in the resource table at `PDUCreateComLogicalLink` time, step (c) of
   that function's behaviour — before `PDUConnect` is ever called. The existing
   `active_candidate` match (`rpc_link.rs:589-601`) requires `link.connected`, so a created
   but not-yet-connected CLL incorrectly reports "not in use."

Two design questions are spec-silent and decided here:

- What counts as "the same physical resource" for this bitfield: `ChannelProtocol` equality
  (`active_candidate`'s existing match) or `hw_protocol_id` equality (ADR-123's lock-conflict
  machinery)?
- Whether a lock held by a CLL on whose behalf the query itself might be understood to be
  made still sets the bit (the RPC has no "querying CLL" concept at all — it takes a module
  handle and a resource, not a CLL handle).

## Decision

- **Resource identity: `hw_protocol_id` equality**, not `ChannelProtocol` equality, mirroring
  `same_physical_resource`'s comparison (shared by `find_physical_lock_holder`, ADR-123).
  Each resolved `(ChannelProtocol, hw_protocol_override)` candidate's `hw_protocol_id` is
  derived exactly the way `CreateComLogicalLink` derives a new CLL's own
  (`hw_protocol_override.unwrap_or_else(|| can_channel_mode.hw_protocol_id(protocol))`,
  `rpc_link.rs:935-937`), so the comparison uses the identical value a real CLL created for
  this resource would carry. This is deliberately different from `active_candidate`'s existing
  `ChannelProtocol`-equality gate: in software-ISO-TP mode an ISO15765-family CLL and a raw-CAN
  CLL share one physical CAN channel under two different `ChannelProtocol` values (ADR-046) —
  only `hw_protocol_id` equality correctly reports one CLL's held lock when the other's
  resource is queried. `active_candidate` itself is left unchanged, since its own purpose (the
  `resource_id` echo tie-break for an ambiguous `ResourceName` query, preferring the actually
  *connected* configuration) is unrelated to A2-3 and still wants exactly what it already
  computes.
- **Any holder counts, including a CLL that happens to belong to whichever context is
  querying.** Table D.1's bit text ("locked by a CLL") describes only that a lock exists on
  the resource, not who holds it, and `GetResourceStatus` has no per-call CLL identity to
  exclude even if the spec called for it.
- **Bit 0 ("in use")** is true whenever any `LogicalLinkState` with a matching
  `hw_protocol_id` exists for the currently-open module (ADR-107's single-open-device model,
  the same gate `active_candidate` already uses), regardless of `connected` — i.e. from
  creation onward, per §9.4.9.2 c).
- **Bits 2/3** OR `held_lock_mask` across every matching link and test
  `LOCK_PHYSICAL_TX_QUEUE`/`LOCK_PHYSICAL_COM_PARAMS` respectively. Table D.2's `LockMask` bit
  positions (bit 0 = ComParams, bit 1 = TX queue) and Table D.1's `resource_status` bit
  positions (bit 2 = TX queue, bit 3 = ComParams) are two distinct bit-encoded fields; this is
  a value remap between them, not a shared bit layout.
- **Bit 1 (Availability Status)** is unchanged (always 0) — not part of A2-3's finding.
- **One `logical_links` lock acquisition, not two.** `active_candidate`, the in-use flag, and
  the OR'd lock mask are all computed under the single existing lock guard (still gated on
  `open_module_handle == Some(module_handle.module_handle)`), instead of adding a second,
  redundant lock/unlock cycle for the new computation.
- **`resource_status` is scoped to the SAME single resource the response's `resource_id`
  echoes, never to the full ambiguous candidate set (edge-case-hunter finding, closed before
  merge).** An ambiguous `ResourceName` query (e.g. `"SAE_J2610_SCI"`) resolves to several
  `(ChannelProtocol, hw_protocol_override)` candidates in `resolved`, but the response is one
  `resource_id`/`resource_status` pair, and the pre-existing (unchanged) `resource_id` echo
  logic already narrows an ambiguous match down to exactly one row (preferring
  `active_candidate`'s row, else the first table row). The first implementation of this
  decision computed `in_use`/`held_lock_mask` against every candidate's `hw_protocol_id`
  unioned together — this let the echoed `resource_id` and `resource_status` describe two
  *different* rows when one ambiguous-group member had a CLL and a different member was the
  one actually echoed, e.g. echoing 0x0222 (first table row, nothing connected) with
  `resource_status` bit 0 set because 0x0225 (a different row in the same group) had an
  unconnected CLL. Corrected by deriving a single `status_candidate` using the *exact same*
  row-selection rule the `resource_id` echo already uses (mirrored, not shared code — the
  echo logic itself is untouched to avoid risking its own tested behavior), and matching
  `in_use`/`held_lock_mask` against only that one candidate's `hw_protocol_id`.

## Accepted Residual

`GetResourceStatus`'s notion of "the same physical resource" for these bits — like
`active_candidate`'s pre-existing `ChannelProtocol` match, which this ADR's `hw_protocol_id`
match is scoped no more finely than — is **protocol-level, not channel/baud-rate-level**. Two
connected CLLs sharing one `hw_protocol_id` at two different baud rates are two distinct
`ChannelKey`s/`SharedChannel`s (and `LockResource`/`find_physical_lock_holder`/
`same_physical_resource` correctly treat them as non-conflicting, since those compare
`channel_key` once both sides are connected, falling back to `hw_protocol_id` only pre-connect)
— but `GetResourceStatus`'s bits 0/2/3 cannot make that distinction, because its own request
(`ModuleAndResourceId`, a resource ID or name) carries no baud rate/channel parameter to
disambiguate against in the first place. This is not a new gap introduced here: bit 0 already
had this exact coarseness before this ADR (`active_candidate` never compared baud rate
either); this ADR's lock bits simply inherit the same, structurally-forced granularity rather
than introducing a finer-grained inconsistency. Fixing it would require widening this RPC's
request shape, which is out of scope for A2-3.

## Alternatives Considered

1. **Match by `ChannelProtocol` equality**, reusing `active_candidate`'s existing gate as-is
   for the new computation. Rejected: misses the ADR-046 software-ISO-TP cross-protocol
   sibling case that ADR-123's lock machinery already accounts for; would make
   `GetResourceStatus` and `LockResource`/`UnlockResource` disagree about what "the same
   physical resource" means for the same underlying hardware.
2. **Exclude a lock held by the querying context's own CLL.** Rejected: `GetResourceStatus`
   takes a module + resource, never a CLL handle, so there is no "self" to exclude, and the
   spec text describes lock existence, not attribution.

## Consequences

- `GetResourceStatus` now reports bit 0 for a CLL that has been created but never connected —
  a client-visible change from always-0 in that state.
- `GetResourceStatus` now reports bits 2/3 whenever any CLL sharing the physical resource
  holds the corresponding `LockResource` bit (via ADR-123's `held_lock_mask`) — previously
  always 0.
- Test coverage: `j2534-0404-service/tests/grpc_mock/resources.rs` — new regression tests for
  (a) a created-but-unconnected CLL reporting "in use", and (b) `LockResource`'s TX-queue and
  ComParam bits surfacing through a sibling's `GetResourceStatus` query.
- **Accepted residual, unchanged by this fix:** the §9.4.13.3 use case 4 / §9.4.14.2 c)
  `PDU_IT_INFO` lock-status-change callback remains unimplemented (an existing accepted
  residual from ADR-123) — a client must still poll `GetResourceStatus` to observe a lock
  change rather than being pushed one; this ADR makes that poll return the correct value; it
  does not add the push notification.
