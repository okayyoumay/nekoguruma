# ADR-005: Pass-All Hardware Message Filter Strategy

**Date:** 2026-06-28  
**Status:** Superseded by ADR-038  
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`rpc_connect_com_logical_link`),
             `j2534-0404/src/lib.rs` (exports)

## Context

J2534 adapters silently discard all received frames unless at least one message
filter is installed via `PassThruStartMsgFilter`.  The service was calling
`PassThruConnect` but never installing any filter, so no frames were ever
delivered to `poll_rx`.  On a CAN bus with any traffic this caused 100% frame
loss, making all diagnostic communication impossible.

ISO 22900-2 handles filtering at the service layer via `UniqueRespIdTable`
and `ExpectedResponseData` mask/pattern matching.  The J2534 hardware filter is
a lower-level mechanism: filtering in hardware reduces bus load on the USB/serial
link between the PC and the adapter.

## Decision

> **Superseded by ADR-038:** installing both a `PASS_FILTER` and a
> `FLOW_CONTROL_FILTER` on ISO15765 channels (as described below) violates
> the J2534 v04.04 spec, which permits only `FLOW_CONTROL_FILTER` on ISO15765
> channels. See ADR-038 for the corrected per-protocol filter type selection.
> The rest of this ADR (pass-all strategy for non-ISO15765 channels; filter
> IDs not stored) still stands.

When `ConnectComLogicalLink` creates a **new** physical J2534 channel, the
service immediately installs a **pass-all PASS_FILTER** (mask = 0, pattern = 0,
4 bytes) before the poll task starts.  This ensures all received frames reach
the service layer.

For **ISO15765** channels a second **FLOW_CONTROL_FILTER** (mask = 0,
pattern = 0, flow-control frame = 0) is also installed.  This allows the J2534
adapter to generate ISO15765 flow-control frames autonomously in response to
multi-frame transmissions from ECUs, which is required for correct ISO15765
operation.  A failure to install the FC filter is logged but does not abort the
connection (some adapters handle FC internally without an explicit filter).

The filter IDs returned by `PassThruStartMsgFilter` are not stored because
`CLEAR_MSG_FILTERS` (exposed via the IoCtl RPC) allows the caller to clear all
filters at once when finer-grained control is needed.

## Alternatives Considered

1. **Per-address hardware filters from UniqueRespIdTable** — Install a specific
   filter for each ECU address in the table.  This reduces load on the USB link
   but requires keeping filters in sync with every `SetUniqueRespIdTable` call,
   and fails gracefully when the table has more entries than the adapter's filter
   limit (typically 10–20 filters).

2. **No hardware filter / rely on adapter default** — Some adapters allow frames
   through without a filter when LOOPBACK is enabled; others do not.  Relying on
   adapter-specific behaviour would make the service non-portable.

3. **Address-aware FC filter from addressing ComParams** — Build an FC filter
   from `CP_CanPhysReqId` / `CP_CanRespUSDTId`.  More efficient for ISO15765,
   but requires tracking param changes and re-installing the filter on every
   `CoptUpdateparam`, increasing complexity significantly.

## Consequences

- All received frames are delivered to the service layer regardless of source
  address.  Service-level filtering (UniqueRespIdTable, expected-response
  mask/pattern) handles routing to the correct CLL.
- `CLEAR_MSG_FILTERS` now also removes the pass-all filter; callers must
  re-connect (or re-issue `ConnectComLogicalLink`) to restore reception.
- Joining CLLs on an already-connected channel share the existing filter set;
  they do not install additional filters.
