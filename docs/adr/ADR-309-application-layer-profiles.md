# ADR-309: Application-Layer Profiles for Protocol-Neutral Diagnostic Calls

**Date:** 2026-10-16
**Status:** Accepted
**Affects:** `diag-frontend` (Starlark lowering, CSV tables), `diag-ir` (`Op`, `DiagHost`, declaration
part schema), `agent` (`src/host.rs`, `src/link.rs`), `crates/diag-frontend/docs/starlark-subset.md`,
`crates/diag-frontend/docs/csv-tables.md`, ADR-235 items 2 and 3, design 8.2.4 and 8.4

## Context

The Starlark procedure API (ADR-254, `starlark-subset.md` 3) names its calls by intent:
`diag.read_dtc`, `diag.routine`, `diag.security_access`, `diag.flash_transfer`. Today each of them
means one UDS request:

- the IR has a primitive per call (`ReadDtc`, `RoutineControl`, `SecurityAccess`, `FlashTransfer`),
  and the agent's host builds the UDS request bytes for it (ADR-235 item 2): ReadDTCInformation
  with sub-function 0x02, RoutineControl with a 2-byte identifier, the TransferData block counter;
- the host accepts a response only in UDS form: a positive response is the request's service
  identifier plus 0x40, and a negative one is `7F`, the service identifier and a code, with 0x78
  as response pending (ADR-235 item 3);
- `diag.request` takes a one-byte service identifier described as a UDS one.

Older ECUs speak other application layers over the transports the workers already support, KWP2000
on K-line or CAN among them. Their services follow the same request, positive response, negative
response pattern, but the same intent maps to different services, sub-functions, identifier widths
and response layouts: reading DTCs or running a routine is a different request, and a routine's
results can need a second service. Procedure authors want to keep writing `diag.read_dtc` and
`diag.routine` for such ECUs, because those names say what the procedure does, where a
`diag.request` with a protocol-specific service identifier does not.

The question is where the per-protocol behaviour of each call lives. If the agent's host grew a
branch per protocol, every new application layer would be a core change redistributed to every
agent, and protocol differences would leak below the frontend, against the design invariant that
format differences stop at L3 (design 8.2) and the rule that the subset is widened by updating the
frontend (design 8.4).

## Decision

1. **Calls stay protocol-neutral.** `diag.read_dtc`, `diag.routine`, `diag.security_access` and
   `diag.flash_transfer` keep their names and arguments as statements of intent. A procedure does
   not change when the ECU's application layer does. `diag.request` keeps addressing a declared
   service by its service identifier and constants (`csv-tables.md` 5.3), which are those of the
   ECU's own application layer.

2. **Application-layer profiles.** The frontend holds one profile per supported application layer,
   starting with UDS (ISO 14229-1). A profile is data, not code in the agent. For each call it gives:
   - the service or sequence of services the call expands to, with their constants (service
     identifier, sub-function) and the request fields the call's arguments fill, including their
     widths;
   - how the response of each service maps to the call's result fields, under names shared by every
     profile (for example the DTC list and status of `read_dtc`), so a procedure reads the same keys
     whatever the application layer;
   - the rules the sequence needs that are not one request: the key request that follows a seed,
     the block counter of a transfer;
   - the shape of a negative response (the marker byte, where the rejected service identifier and the
     code sit) and the codes that mean response pending.

   Profiles ship with the frontend. Adding one, or correcting one, is a frontend update (design 8.4,
   9.2), with no agent redistribution as long as the IR schema does not change.

3. **The variant names its application layer.** Each protocol a variant declares names the
   application layer used over it, and the frontend lowers every call of that variant's procedures
   through that layer's profile into the variant's declared services. A call whose intent the
   profile does not define, or a result field the profile does not map, is an ingestion error; a
   procedure never reaches the agent with a call its ECU cannot serve.

4. **The IR and the agent carry no application layer.** The diagnostic primitives that reach the ECU
   become one generic send-receive: the complete request bytes, built by lowering, plus a reference
   to the declared service whose response it expects. The host matches the answer against that
   service's declared positive response (its leading bytes and the constant fields it echoes) and
   against the negative-response shape and response-pending codes the IR declaration carries for the
   link's protocol, and it configures the worker's response-pending handling from the same
   declaration. The host builds no request bytes of its own. `ReadDtc` and `RoutineControl` are
   removed in favour of the generic send-receive, which the pre-release schema rule allows
   (ADR-281 item 4). `SecurityAccess` stays, since turning a seed into a key goes through the server,
   and `FlashTransfer` stays for its resume semantics; both take the bytes and counter rule the
   profile gives them instead of building UDS requests.

5. **Scope.** Profiles cover application layers that follow the request, positive response, negative
   response pattern. A protocol without it (block-oriented manufacturer protocols with per-byte
   handshakes) is not a profile: its framing and timing belong to the worker's L1 (design 8.1, 9.7),
   and exposing its services would need a further IR decision.

6. **Relation to ADR-235.** This replaces ADR-235 item 2's UDS request building in the host and item
   3's fixed UDS response patterns. Item 3's handling of unsolicited frames, late answers, failures
   and lost events is unchanged.

## Consequences

- The UDS profile reproduces today's behaviour, so the lowering of `diag.read_dtc`,
  `diag.routine` and `diag.security_access` that M2 still needs goes through it from the start
  rather than being written against the UDS-specific primitives and reworked later.
- The IR declaration part gains the per-protocol negative-response shape and response-pending codes,
  and the protocol declaration gains its application layer. The agent then reads its matching rules
  from the program instead of from code, which is also what filling the link from the IR (ADR-235
  item 4) needs.
- A KWP2000 profile needs the KWP2000 application-layer standard, which the project does not hold
  yet, and running it end to end also needs a non-CAN link in the agent (K-line addressing in the
  protocol declaration, ADR-235 item 4) and a simulated ECU that speaks it. Until then UDS is the
  only profile.
- The result fields shared across profiles become part of the procedure API (`starlark-subset.md`
  3.4). A field one application layer cannot provide is absent for it, and a procedure that reads it
  is rejected at ingestion for that variant.
- Each profile is tested the way the IR is: a known call lowered through the profile gives the
  expected request bytes and decoded result fields (design 12.1).
