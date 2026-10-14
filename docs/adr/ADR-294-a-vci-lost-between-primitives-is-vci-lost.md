# ADR-294: A VCI Lost between Two Primitives Is Reported as `HostError::VciLost`

**Date:** 2026-10-14
**Status:** Accepted
**Affects:** `agent` (`src/host.rs`), `j2534-0404-service` (`src/service/events.rs`,
`tests/agent_restart_end_to_end.rs`), `docs/system-architecture.md` 8.2.4; ADR-105, ADR-289,
ADR-292

## Context

ADR-289 ends the pending primitive with `HostError::VciLost` when the worker reports the VCI
lost. When the device is lost between two primitives, no primitive is pending: the worker's
receive poll meets the lost device, reports `PDU_ERR_EVT_LOST_COMM_TO_VCI` on the link and takes
the link offline. The next request then fails as a call on an offline link, which the agent
reported as `HostError::Rpc` with `FailedPrecondition`. The job still ended as an interrupted
step, but its error did not name the lost VCI. ADR-289 (Consequences) left this open, assuming
the worker would first have to say why a link is offline.

The worker already says it. It keeps each link's last error event and attaches it to a refused
call's status as `ErrorDetail.error_event_data` (ADR-105). A `StartComPrimitive` on a link the
worker took offline after a lost VCI is refused as `FailedPrecondition` with
`PDU_ERR_EVT_LOST_COMM_TO_VCI` attached.

## Decision

1. **The host reports such a refusal as `HostError::VciLost`.** A `StartComPrimitive` that fails
   with `FailedPrecondition` and carries `PDU_ERR_EVT_LOST_COMM_TO_VCI` as its error event is the
   lost VCI. The request was not sent, since the link was offline.
2. **Every other failure is reported as before.** A refusal with another error event or none,
   another status code, and a failure of any other call stay `HostError::Rpc` (or what they
   were).
3. **The worker records the loss with the offline transition.** It already reported the loss in
   the status it returns, but recorded the link's error event only after it had taken the link
   offline and released the link table, so a primitive started in between was refused without
   it. The hard-error handler now records `PDU_ERR_EVT_LOST_COMM_TO_VCI` as the link's last
   error in the same critical section that takes the link offline. Every refusal on the offline
   link carries the loss.
4. **The job's handling does not change.** A host error ends the run as an interrupted step, as
   ADR-289 item 4 says.

## Consequences

- A VCI disconnect between two primitives now ends the run in `HostError::VciLost`, the same error
  as a disconnect during one. The restart end-to-end tests assert it.
- A crashed simulated VCI (ADR-292) ends the run the same way, since the worker treats its failing
  read as a lost device and records the same event. ADR-292's consequence that a crash shows as
  `HostError::Rpc` no longer holds.
- The mapping depends on the worker attaching the link's last error event. A worker that did not
  would leave the error as `HostError::Rpc`, as before; the job's handling is the same either
  way.
- A worker unit test parks the hard-error handler after it took the link offline and before it
  sends the event, and checks that the link already carries the loss.
