# ADR-422: A Failed Device Close Keeps the Worker's Slot

**Date:** 2026-11-05
**Status:** Accepted
**Affects:** `j2534-0404-service` (`rpc_module_disconnect`), `j2534-0404-mock` (`__mock_fail_closes`),
`sim-vci` (`fail_closes`, `clear_close_failures`), `crates/j2534-0404-service/tests/agent_marked_guards.rs`,
`docs/j2534-0404-architecture.md`; ADR-107 addendum (h), ADR-132, ADR-258, ADR-393 item 2

## Context

`ModuleDisconnect` cleared the worker's device slot before it called `PassThruClose`, so a close
that failed still left the worker with no open device (ADR-107 addendum (h)). The agent then had
no way to close the device again: its second close of a lost path (ADR-393 item 2) found nothing
open and succeeded, which confirmed a close that had failed twice. ADR-258's mark on the job's
guards, which keeps the VCI lock while the worker may still hold the device, therefore had no
end-to-end case, and a library that kept the device open after a failed close was out of the
worker's reach for the rest of its life.

## Decision

1. **A failed close keeps the slot.** `rpc_module_disconnect` still tears down the links and
   channels and bumps the device epoch before it closes, but it clears the slot only when the
   close ended the device's open state. Any other error keeps the slot, and the RPC returns the
   status it mapped before.
2. **Two answers count as closed, also on the first close.** A close answered with an invalid
   device ID or with the device not connected counts as closed: the library no longer has the
   device (SAE J2534-1 6.10.1 and 7.2.2; ISO 22900-2:2022 9.4.30). The worker clears the slot,
   logs a warning and returns success. This keeps ADR-393 item 2's confirmed close of a lost
   device: a library that releases the device on the first close yet reports the loss from it is
   confirmed by that first close.
3. **`ModuleListChg` follows the slot.** The worker sends the event only when the slot is empty
   after the attempt: after a close that cleared it, and when nothing was open. Since ADR-132 the
   module's row in `GetModuleIds` derives from the slot, so a kept slot leaves the row, and the
   subscribers' view of it, unchanged. Subscriptions end only when the disconnect succeeds.
4. **A kept slot is used again.** The open paths reuse it as before (`ModuleConnect` for the same
   handle opens no second device; another handle is refused with `PDU_ERR_RESOURCE_BUSY`, and a
   module marked not available is still refused), a later `ModuleDisconnect` closes it again, and
   the worker's shutdown closes it as before. `sim-vci` models the failure with `fail_closes`
   (`ERR_FAILED`, the device stays open), and the mock library with `__mock_fail_closes`.

## Consequences

- A failed close costs the agent one more `ModuleDisconnect`, which its retry of a lost path
  already makes (ADR-393 item 2). A close that fails twice stays unconfirmed and ADR-258's mark
  holds the guards until the worker is reaped; `agent_marked_guards.rs` shows that end to end,
  and the retried close that confirms.
- ADR-107 addendum (h)'s residual that a failed close abandons the device narrows: the channels,
  links and their messages are still gone after a failed close, but the device itself stays
  reachable by the next close.
- The close's retry is the agent's; the worker does not retry on its own.
