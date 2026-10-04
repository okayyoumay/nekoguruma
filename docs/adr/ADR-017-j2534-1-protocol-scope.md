# ADR-017: J2534-1 Protocol Scope — Features Specific to Unsupported Protocols Are Not Implemented

**Date:** 2026-06-28  
**Status:** Superseded by ADR-152  
**Affects:**
- `j2534-0404-service/src/service.rs` (ComParam constants and allowlists)
- `j2534-0404-service/src/service/comparam_support.rs` (protocol allowlists)
- `j2534-0404-service/src/service/names.rs` (ComParam name mappings)

## Context

SAE J2534-1 (DEC 2004) defines the following physical/transport protocols:

| Protocol ID | Name           |
|-------------|----------------|
| 0x01        | J1850PWM       |
| 0x02        | J1850VPW       |
| 0x03        | ISO9141        |
| 0x04        | ISO14230 (KWP) |
| 0x05        | CAN            |
| 0x06        | ISO15765       |
| 0x07        | SCI_A_ENGINE   |
| 0x08        | SCI_A_TRANS    |
| 0x09        | SCI_B_ENGINE   |
| 0x0A        | SCI_B_TRANS    |

Protocols such as SWCAN (Single Wire CAN) and GM_UART are defined in J2534-2
(an optional extension standard), not in J2534-1.  This service is an implementation
of the J2534-1 API only (`j2534-0404` = SAE J2534-1 version 04.04).

Some ISO 22900-2 D-PDU ComParams exist exclusively to support functionality
that requires a non-J2534-1 protocol.  Examples:

- **CP_ChangeSpeedCtrl / CP_ChangeSpeedMessage / CP_ChangeSpeedRate /
  CP_ChangeSpeedResCtrl / CP_ChangeSpeedTxDelay** — SWCAN baud-rate negotiation
  sequence: send a speed-change message at the current baud rate, wait for ECU
  acknowledgement, then switch the adapter to the new baud rate.  This sequence
  is only meaningful for SWCAN, which is not a J2534-1 protocol.

## Decision

**Protocol features that require a protocol not in J2534-1's scope are not
implemented.  `SetComParam` rejects the corresponding ComParam IDs with
`Status::invalid_argument`.**

The first application of this rule was the CP_ChangeSpeed* group (commit
preceding this ADR): these five ComParam IDs were removed from the
`comparam_support` allowlists for all protocol families so that any call to
`SetComParam` with these IDs returns `invalid_argument` immediately.

**ComParam constant definitions (`PARAM_CHANGE_SPEED_*` etc.) and
`GetObjectId` name mappings are intentionally retained.**  The numeric ID
values and their canonical names are part of the service's ComParam namespace
contract and may be referenced by callers for identification or documentation
purposes (e.g. diagnostic tools that query `GetObjectId("CP_ChangeSpeedCtrl")`
to discover the param ID).  Only the operational behaviour (storage in
`ComParamSet`, forwarding to hardware, `apply_params_to_hardware`) is absent.

**Note on unknown protocol IDs:** The `comparam_support::is_param_allowed`
function uses a catch-all `true` return for protocol IDs it does not recognise.
This allows the service to accept `SetComParam` calls on channels opened with
a custom or future protocol ID without rejecting every param.  This is
forward-compatibility behaviour and does not constitute support for any
specific non-J2534-1 protocol.

## Consequences

- Callers that set CP_ChangeSpeed* ComParams receive `invalid_argument` instead
  of a silent no-op.  This makes unsupported behaviour explicit and detectable.
- The rule provides a clear principle for future decisions: if a proposed feature
  requires a protocol not in the J2534-1 list above, it is out of scope and
  should be rejected at the `SetComParam` / `StartComPrimitive` boundary rather
  than silently stored.
- Protocols in J2534-2 (SWCAN, GM_UART, …) remain unimplemented.  A future
  revision to `j2534-0404-service` that targets J2534-2 would revisit this ADR.
- **2026-07-29:** that future revision is now planned — see
  [docs/j2534-2-support-plan.md](../j2534-2-support-plan.md).
- **2026-07-29 (Phase 0):** superseded by
  [ADR-152](ADR-152-j2534-2-foundational-decisions.md), which resolves the
  plan's cross-cutting design questions and begins J2534-2 support. This
  ADR's protocol-scope restriction no longer applies going forward, but the
  history above (why `CP_ChangeSpeed*` was rejected, and the general
  principle) remains an accurate record of the decision as it stood.
