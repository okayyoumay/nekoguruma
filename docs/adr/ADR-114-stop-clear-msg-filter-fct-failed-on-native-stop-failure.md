# ADR-114: STOP_MSG_FILTER/CLEAR_MSG_FILTER Report PDU_ERR_FCT_FAILED on a Native Stop Failure

**Date:** 2026-07-23
**Status:** Accepted, superseding ADR-079 (items 12-13's silent-success error reporting only;
the retry-tracking behavior itself is unchanged)
**Affects:**
- `j2534-0404-service/src/service/rpc_misc.rs`
- `j2534-0404-mock/src/lib.rs`

## Context

ISO 22900-2:2009(E) Table 55 (line 3579) and Table 56 (line 3606) — in
`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md` — both define
`PDU_ERR_FCT_FAILED` ("Command failed") as a valid return for
`PDU_IOCTL_STOP_MSG_FILTER`/`PDU_IOCTL_CLEAR_MSG_FILTER`. ADR-079 items 12/13 had a native
`PassThruStopMsgFilter` failure logged and silently treated as success (`Ok(())`) so the filter
stayed tracked/retriable — this is a real, confirmed spec deviation, flagged "Revisit" by
`j2534-0404-service/docs/iso22900-2-conformance-audit.md` item B21 (originally A1-5, P1
severity — a client cannot distinguish "filter actually stopped" from "stop silently failed").

This rests on the ISO 22900-2:2009 text only. A 2022 copy was added to `vehicle-comm-specs`
after this decision (2026-07-29); the 2009→2022 revision of Table 55/56 has not yet been
checked against it — see the tracked backlog item in
`j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog.

## Decision

Keep ADR-079's best-effort iteration and retry-tracking (a failed stop's filter id(s) stay in
`client_filters` so the client can see/retry them — the hardware filter may still be active)
— that rationale is sound and unchanged.

What changes: the RPC now returns `PDU_ERR_FCT_FAILED` (via `map_native_error_as`,
`Code::Internal`) when any underlying native stop call fails, with a message naming the
affected `FilterNumber`(s), instead of silently returning `Ok(())`.

- `ioctl_stop_msg_filter` still attempts every `MessageFilterId` under the requested
  `FilterNumber` (no short-circuiting) and keeps the failed ids tracked under
  `client_filters` exactly as before. It additionally captures the first native
  `j2534_0404::Error` encountered; if any id failed, it returns
  `map_native_error_as(..., PduError::PduErrFctFailed, last_error)`, with `last_error` read
  from `LogicalLinkState::last_error` inside the same final `logical_links` lock acquisition
  that performs the bookkeeping update (matching the stale-snapshot-race contract documented
  at `j2534-0404-service/src/error.rs:168-179`).
- `ioctl_clear_msg_filter` still attempts every filter id across every tracked `FilterNumber`
  (no short-circuiting) and keeps failed `FilterNumber`s (with just their failed ids) tracked
  in `client_filters`, removing only fully-succeeded ones — unchanged. It additionally
  captures the first native error and, if any `FilterNumber` failed, returns
  `map_native_error_as` naming the sorted list of failed `FilterNumber`s, with `last_error`
  read from the same final lock acquisition as the bookkeeping update.

Internal/teardown callers of `stop_message_filter` are **unaffected** — Table 55/56 govern
only these two client-facing IOCTLs, not:
- `DestroyComLogicalLink` teardown (ADR-082),
- `remove_point_to_point_fc_filters` FLOW_CONTROL_FILTER cleanup (ADR-039/005;
  `sync_channel_fc_pass_all_filter`, also unaffected at the time this ADR was
  written, has since been removed entirely by ADR-122),
- `PDU_IOCTL_RESET`'s own filter cleanup (ADR-079 item 1).

All three stay best-effort-only, unchanged.

`PDU_IOCTL_CLEAR_MSG_FILTER`'s own scope is also unchanged: it does not touch
`unique_resp_filter_ids` (the API-owned pass-all baseline filter is not a "message filter
from the ComLogicalLink" a client installed, consistent with Figure 27's note that the D-PDU
API itself configures at least one PASS filter per CLL).

The mock (`j2534-0404-mock/src/lib.rs`) gained a blanket per-call error-injection override,
`__mock_set_stop_filter_error`, mirroring `__mock_set_fast_init_error`'s existing pattern
(`Option<c_long>` state field, `code == STATUS_NOERROR` clears it, cleared by
`__mock_reset`), checked in `PassThruStopMsgFilter` after the existing bad-`channel_id` check
but before the success counter increments or the filter is removed — so a forced failure
never simulates a partial success and never increments `__mock_get_stop_filter_count`.

## Consequences

- A client that previously observed unconditional `PDU_STATUS_NOERROR` from
  `PDU_IOCTL_STOP_MSG_FILTER`/`PDU_IOCTL_CLEAR_MSG_FILTER` now sees `Code::Internal` /
  `PDU_ERR_FCT_FAILED` on a genuine native failure; retrying the same IOCTL is the documented
  recovery path, since state is preserved for exactly that.
- **Caveat:** this decision rests on the ISO 22900-2:2009(E) text only. A 2022 copy is now
  available in `vehicle-comm-specs` (added 2026-07-29, after this decision) — the 2009→2022
  revision of Table 55/56 has not yet been checked against it; see the tracked backlog item
  in `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog.
- **Latent gap, explicitly out of scope for this fix** (recorded in
  `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog): Table 55 (line
  3587) explicitly reserves `PDU_ERR_INVALID_PARAMETERS` for "the Filter Number is invalid",
  but `ioctl_stop_msg_filter`'s `client_filters.get(&filter_number)` lookup failure currently
  returns `unknown_handle_status` → `PDU_ERR_INVALID_HANDLE`, which Table 55 reserves for an
  invalid CLL handle, not an invalid `FilterNumber`. Not fixed here.
- **Tests:** `tests/grpc_mock/pdu_ioctl.rs`'s
  `stop_msg_filter_reports_fct_failed_and_keeps_filter_tracked_on_native_stop_failure` and
  `clear_msg_filter_reports_fct_failed_and_keeps_filters_tracked_on_native_stop_failure`
  exercise the new `PDU_ERR_FCT_FAILED` reporting end to end against the mock (via
  `__mock_set_stop_filter_error`), including the retry-after-clearing-the-override recovery
  path. Pre-existing success-path tests
  (`clear_msg_filter_stops_all_client_filters_and_empties_the_map`, disconnect/destroy/reset
  filter-cleanup tests) are unaffected — the mock's default behavior (no error injected) is
  unchanged.

## Alternatives considered

- **Short-circuiting the stop loop on first failure.** Rejected — loses best-effort coverage
  of the remaining filters; a later id in the same `FilterNumber`/`CLEAR_MSG_FILTER` sweep
  might still stop successfully and should not be skipped just because an earlier one failed.
- **Dropping failed ids from `client_filters` on error.** Rejected — regresses ADR-079's
  hardware-filter-leak retry rationale: the client would lose the ability to see and retry a
  filter whose hardware state is unknown/possibly still active.
- **A generic "N of M failed" message instead of naming `FilterNumber`s.** Rejected — discards
  already-collected actionable detail; naming the specific `FilterNumber`(s) that failed costs
  nothing extra and is more useful for client-side diagnostics/retry logic.
