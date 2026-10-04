# ADR-129: `START_MSG_FILTER`/`STOP_MSG_FILTER`/`CLEAR_MSG_FILTER` Accept Pre-Connect Configuration

**Date:** 2026-07-24
**Status:** Accepted; amends ADR-082
**Affects:** `j2534-0404-service/src/service.rs`, `j2534-0404-service/src/service/rpc_misc.rs`, `j2534-0404-service/src/service/rpc_link.rs`, `docs/adr/ADR-082-client-filters-imply-sole-channel-ownership.md`

## Context

Conformance-audit finding A2-7 (`j2534-0404-service/docs/iso22900-2-conformance-audit.md`):
`ioctl_start_msg_filter` (`rpc_misc.rs`) unconditionally rejected
`PDU_IOCTL_START_MSG_FILTER` with `PDU_ERR_CLL_CONNECTED`'s sibling code,
`CLL_NOT_CONNECTED`, whenever the calling ComLogicalLink (CLL) had not yet
been connected (`channel_id.is_none()`). ISO 22900-2:2009(E) contradicts
this on two counts:

- §9.5.13's Table 54 (the return-value table for this IOCTL) lists no
  `CLL_NOT_CONNECTED`-equivalent code at all — only
  `PDU_ERR_PDUAPI_NOT_CONSTRUCTED`, `PDU_ERR_INVALID_HANDLE`,
  `PDU_ERR_COMM_PC_TO_VCI_FAILED`, `PDU_ERR_INVALID_PARAMETERS`, and
  `PDU_ERR_FCT_FAILED`.
- §9.4.11.2 d) (`PDUConnect`'s own behaviour spec) says explicitly
  that the connect step enables the ComLogicalLink's filters, taking their
  configuration from the URID table, except where the client application has
  already configured filters before `PDUConnect` through the
  `PDU_START_MSG_FILTER`, `PDU_CLEAR_MSG_FILTER` or `PDU_STOP_MSG_FILTER`
  IOCTLs.

So all three IOCTLs are meant to be usable before `PDUConnect`, with
`START_MSG_FILTER`'s filters becoming active once the CLL reaches
`PDU_CLLST_ONLINE`. The pre-existing implementation rejected the pre-connect
case outright instead.

This intersects a load-bearing invariant from ADR-082: a CLL's
`client_filters` being non-empty implies that CLL is the *sole* owner of its
physical channel (`ref_count == 1`), which lets disconnect/destroy skip
building an orphan-filter-tracking mechanism (a CLL with filters is always
the last one out, so its teardown always fully disconnects the channel
anyway). ADR-082's own text calls out that relaxing any of the checks it
depends on requires revisiting it — and one of those checks is exactly
`ioctl_start_msg_filter`'s pre-existing shared-channel rejection, the thing
this fix has to relocate for the pre-connect case, since there is no
`channel_key` yet to check a `ref_count` against.

## Decision

**Data model:** `LogicalLinkState` gains `pending_client_filters:
HashMap<u32, vci_service_interface::IoFilter>` (`service.rs`), storing raw,
not-yet-installed filter definitions keyed by client-supplied `FilterNumber`.
This is a separate field from `client_filters` (installed, hardware-backed
`MessageFilterId`s), not a variant folded into it: `client_filters.is_empty()`
is read at several ADR-082-load-bearing sites (the join guards in
`rpc_connect_com_logical_link`/`ensure_uudt_companion_channel`, and the
teardown loops in `rpc_disconnect_com_logical_link`/
`rpc_destroy_com_logical_link`/`ioctl_reset`) that all mean "actually
installed on hardware" — folding pending entries in would silently change
every one of those. Invariant: `pending_client_filters` is non-empty only
while `channel_id` is `None`; `ConnectComLogicalLink` always drains it (into
`client_filters` on success, or leaves it untouched if the connect itself is
rejected — see below). `PDU_IOCTL_RESET` deliberately does not touch it,
matching this service's existing policy that `RESET` only reconciles
hardware-reflecting state (`client_filters`, `working`/`working_unique_resp_id_table`
are similarly left alone).

**`ioctl_start_msg_filter`:** the ISO15765 rejection (ADR-038:
`FLOW_CONTROL_FILTER` is the only valid type there, and `PDU_IO_FILTER_DATA`
has no flow-control-message field to build one from) and the
duplicate/already-installed-or-pending `FilterNumber` check now both run
unconditionally, before branching on connection state — `hw_protocol_id` is
fixed at `CreateComLogicalLink` and never becomes ISO15765 later, so nothing
is lost by checking it early. Request-shape validation (recognized
`filter_type`, well-formed mask/pattern via `PassThruMessage::new`) is
likewise hoisted to run unconditionally: `PassThruMessage::new`'s validation
depends only on payload length, not on `TxFlags`, so a representative
`flags = 0` check is valid regardless of which `TxFlags` variant the
connected path will actually install under. This matters because
`PDUConnect`'s own return-value table has no `INVALID_PARAMETERS` code, only
`FCT_FAILED` — deferring shape validation to connect time would strand a
malformed request behind the wrong error code family. When `channel_id` is
`None` after all of the above, the (now fully validated) filters are stored
verbatim in `pending_client_filters` and the call returns `Ok(())` with no
native calls and no `shared_channels`/lock-holder checks (there is no
channel to race over yet). The connected path is otherwise unchanged
behaviorally; its native-install loop is factored out into a new shared
`rpc_link::install_client_message_filters` so the connect-time install path
(below) cannot drift from it.

**`ioctl_stop_msg_filter`/`ioctl_clear_msg_filter`:** when `channel_id` is
`None`, `client_filters` is necessarily empty (nothing has been installed
yet), so `STOP` removes the given `FilterNumber` from
`pending_client_filters` directly (same "unknown FilterNumber" error as
today if it's not there either) and `CLEAR` empties
`pending_client_filters` wholesale. Neither makes a native call or checks
the physical-lock holder.

**`rpc_connect_com_logical_link`:** the existing join guard (reject joining
an already-shared channel when another CLL on it has non-empty
`client_filters`) gets a symmetric addition: also reject the join when
*this* connecting CLL itself has non-empty `pending_client_filters`. Without
this, honoring those filters would mean installing them on a channel this
CLL does not solely own (breaking ADR-082), and silently dropping them
instead would contradict §9.4.11.2 d)'s explicit client-override intent
with no error at all. Both directions now report the same
`PDU_ERR_FCT_FAILED` (Table 19-legal for `PDUConnect`, unlike
`CLL_NOT_CONNECTED` would have been for `START_MSG_FILTER`); pending filters
are left intact on rejection so the client can `CLEAR_MSG_FILTER` and retry,
or connect a channel that isn't already shared.

This guarantees a CLL only ever installs pre-connect filters when creating a
*new* physical channel, so the install (`install_client_message_filters`,
best-effort per ADR-048's existing precedent for connect-time filter
installation — a native failure rolls back this call's own installs and
disconnects the just-created channel, but does not retry or defer) runs
between `connect_new_physical_channel` and `spawn_new_shared_channel`, while
still holding `shared_channels`.

`finalize_connected_link` is refactored to take that same `shared_channels`
guard by reference from the caller (`chans: &mut HashMap<ChannelKey,
SharedChannel>`) instead of re-acquiring it internally, plus the
newly-installed filter ids to fold into `client_filters` (and
`pending_client_filters` to clear) in the same `logical_links` critical
section that publishes `channel_id`/`connected`. This closes a concurrency
hole in the naive version of this design: with `shared_channels` dropped
and re-acquired between channel creation and `finalize_connected_link`,
another CLL's join-guard check could run in the gap and see a channel with
this CLL's filters already on hardware but `client_filters`/`channel_id`
still unpublished — joining a filtered channel undetected. Keeping
`shared_channels` locked continuously from before channel creation through
the `client_filters` publish closes that window. It also means a
concurrent-destroy rollback (ADR-012) inside `finalize_connected_link` is
guaranteed to see this fresh channel's `ref_count` at exactly 1 (nothing
else could have joined while `shared_channels` was held), so the existing
`PassThruDisconnect`-on-zero-ref_count path tears down any just-installed
filters along with the channel — no separate cleanup needed for that case.

## Alternatives Considered

1. **Fold pending state into `client_filters` as an `Option`/enum-valued
   entry.** Rejected: every existing `is_empty()`/iteration site means
   "installed on hardware," and this would silently change all of them
   (see ADR-082's own list of load-bearing sites).
2. **Install pending filters after `finalize_connected_link`, re-checking
   `ref_count` at that point (mirroring `ioctl_start_msg_filter`'s own
   shape).** Rejected: a joiner could slip into the window between
   `finalize_connected_link` publishing `channel_id` and this later
   re-check, forcing an already-finalized connect to be unwound (full
   disconnect machinery) instead of a plain "don't create the
   `SharedChannel` entry yet" rollback.
3. **A `SharedChannel`-level `client_filtered` marker to close the guard
   window without extending `finalize_connected_link`'s lock scope.**
   Workable fallback, but dual bookkeeping (a flag plus the existing scan)
   invites the two drifting apart; not needed since the guard's existing
   `shared_channels` hold already covers it once continuous.
4. **Silently drop pending filters when a connect would require sharing.**
   Rejected: changes observable RX/TX filtering with no error signal,
   directly contradicting §9.4.11.2 d)'s client-override language.
5. **Warn-and-continue (rather than fail) on a native install failure at
   connect time.** Rejected for this specific path even though ADR-048 uses
   that posture for UniqueRespIdTable-derived `FLOW_CONTROL_FILTER`s: a
   missing client `PDU_FLT_BLOCK` here would silently widen the traffic the
   client explicitly asked to restrict, which is a worse failure mode than
   failing the connect with `FCT_FAILED`.

## Consequences

- ADR-082's load-bearing check set grows from three to four: its join
  guards' "reject if the other side has filters" reasoning now also covers
  "reject if *this* side has pending filters." This amends ADR-082's
  Consequences section with a cross-reference to this ADR rather than
  superseding it — the original three checks are unchanged, a fourth is
  added alongside them.
- **Accepted residual:** filter *definitions* still do not survive
  `DisconnectComLogicalLink` in the sense that matters to a client —
  `rpc_disconnect_com_logical_link` drains and stops `client_filters`, but
  does not itself touch `pending_client_filters` at all (that field is
  simply always empty by the time a disconnect runs: `ConnectComLogicalLink`
  unconditionally drains it before `connected` ever becomes `true`, and
  nothing else can repopulate it while connected). ISO 22900-2 §9.5.13's own
  text ties filter deletion only to `PDUDestroyComLogicalLink`, though, so a
  client that pre-configures filters, connects, disconnects, and reconnects
  the same `cll_handle` must re-issue `START_MSG_FILTER` after every
  reconnect rather than having the original definitions persist and
  reinstall automatically. Not implemented here — tracked as a backlog item
  in `j2534-0404-service/docs/implementation-notes.md`.
- The ISO15765 branch of §9.4.11.2 d)'s "client filters override the URID
  table" clause is structurally moot in this adapter: `ioctl_start_msg_filter`
  rejects every `PDU_FLT` filter type on an ISO15765 `hw_protocol_id`
  (ADR-038), so a client can never have a non-empty `pending_client_filters`
  on a CLL that will connect as ISO15765. `install_point_to_point_fc_filters`
  (ADR-048) therefore needs no conditional skip for this case.
- The `finalize_connected_link` refactor incidentally narrows ADR-012's
  destroyed-mid-connect rollback window slightly (the caller now holds
  `shared_channels` from strictly earlier), a side effect rather than a
  goal of this change.
- Spec-edition caveat (per this repo's standing policy): the clause numbers
  cited above are ISO 22900-2:2009(E). A 2022 revision of §9.4.11.2/§9.5.13
  is possible and has not been checked against this decision.

## Amendment (Codex review, PR #139): serialize pending-filter IOCTLs against Connect's snapshot-install-finalize window

The original PR left a race the design above did not account for.
`rpc_connect_com_logical_link` snapshots `pending_client_filters` under
`logical_links` alone, then releases that lock while it installs the
snapshot onto hardware (an `api` call) and again before
`finalize_connected_link` re-acquires `logical_links` to fold the result
into `client_filters` and clear `pending_client_filters` — all while
`shared_channels` stays continuously held across the whole span, per this
ADR's Decision section. `ioctl_start_msg_filter`/`_stop_msg_filter`/
`_clear_msg_filter`'s not-yet-connected branches, however, only ever
acquired `logical_links`, never `shared_channels` — so a call to any of
them could land in the gap between the snapshot and the fold: a `STOP`/
`CLEAR` there reports success on a `FilterNumber` that the stale snapshot
still installs onto hardware moments later, and a `START` there reports
success on a `FilterNumber` that `finalize_connected_link`'s unconditional
`pending_client_filters.clear()` then discards before it ever reaches
hardware — in both cases, a client-visible success report that the
adapter's actual state contradicts.

**Fix:** all three IOCTLs now acquire `shared_channels` before reading
`channel_id`, and hold it through whichever branch actually runs — the
not-yet-connected branch's `pending_client_filters` read+mutate for
`ioctl_start_msg_filter`, or the connected branch's existing hardware
install and `client_filters` write (unchanged in scope, just acquired one
step earlier); `ioctl_stop_msg_filter`/`ioctl_clear_msg_filter` drop it
immediately once connected, since their connected-path bodies never needed
it and still don't. Because `rpc_connect_com_logical_link` holds this same
lock continuously from before its own snapshot through
`finalize_connected_link`, the two can now never interleave: whichever side
acquires `shared_channels` first runs its entire pending-filter
read-decide-mutate sequence to completion (and, for a filter IOCTL that
lost the race, re-reads `channel_id` fresh under the lock it just acquired,
so it correctly falls onto the connected-path behavior if the connect
finished in the meantime, rather than trusting a stale pre-lock read).

No change was needed to `finalize_connected_link` itself, or to the
connect-time install path — both already serialize correctly against each
other; the gap was solely between them and the three filter IOCTLs. This
closes the finding without weakening the ADR-082 sole-ownership guarantee
already established above.
