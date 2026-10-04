# ADR-001: ModuleDisconnect Force-Cleanup Semantics

**Date:** 2026-06-28  
**Status:** Accepted  
**Affects:** `j2534-0404-service/src/service/rpc_module.rs`

## Context

The original `rpc_module_disconnect` implementation delegated to `close_device()`,
which returned `Status::failed_precondition` when any logical links still existed.
This caused `ModuleDisconnect` to fail with a gRPC error whenever the caller had
not already destroyed every CLL — a common scenario in teardown flows.

## Decision

`ModuleDisconnect` now performs a **forced, ordered cleanup** of all resources:

1. Cancel all queued COPs for every CLL, emitting `PduCopstCancelled` events so
   that subscribers learn their primitives will not execute.
2. Emit `PduCllstOffline` for every currently-connected CLL.
3. Clear the `logical_links` map (drops all `LogicalLinkState` values).
4. Drain `shared_channels`, calling `PassThruDisconnect` for each physical channel
   (stops poll tasks via the `poll_cancel` oneshot).
5. Call `PassThruClose` to release the device.
6. Terminate all gRPC event subscriptions.

## Rationale

ISO 22900-2:2022 §9.3.3 states that `PDU_ModuleDisconnect` must release all
resources associated with the module. Forcing the caller to manually destroy every
CLL before disconnecting makes the API unusable in error-path teardown and is not
required by the standard.

The cleanup sequence matches the existing `spawn_shutdown_task` logic (used on
process exit), ensuring consistent resource release behaviour.

## Consequences

- Callers no longer need to explicitly destroy all CLLs before calling
  `ModuleDisconnect`.
- All active COPs receive `PduCopstCancelled` — subscribers must handle this event
  during teardown.
- The original `close_device()` helper is retained; it is still called internally
  during normal single-CLL-to-disconnect flows where no force-cleanup is needed.
