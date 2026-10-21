# ADR-309: Application-Layer Profiles for Protocol-Neutral Diagnostic Calls

**Date:** 2026-10-16
**Status:** Accepted (items 2 and 5 given their form by ADR-314; item 3's one program per variant lowered per combination of target variants, each call against its target's declarations, by ADR-325 item 6; item 4's per-layer read-only lists and the end of `ReadDtc`'s request building given their form in the agent by ADR-342)
**Affects:** `diag-frontend` (Starlark lowering, CSV tables), `diag-ir` (`Op`, `DiagHost`, declaration
part schema), `agent` (`src/host.rs`, `src/link.rs`, `src/policy.rs`),
`crates/diag-frontend/docs/starlark-subset.md`, `crates/diag-frontend/docs/csv-tables.md`, ADR-235
items 2, 3 and 8, design 8.2.4 and 8.4

## Context

The Starlark procedure API (ADR-254, `starlark-subset.md` 3) names its calls by intent:
`diag.read_dtc`, `diag.routine`, `diag.security_access`, `diag.flash_transfer`. Today each of them
means one UDS request:

- the IR has a primitive per call (`ReadDtc`, `RoutineControl`, `SecurityAccess`, `FlashTransfer`),
  and the agent's host builds the UDS request bytes for it (ADR-235 item 2, ADR-250);
- the host accepts a response only in UDS form: a positive response is the request's service
  identifier plus 0x40, and a negative one is `7F`, the service identifier and a code, with 0x78
  as response pending (ADR-235 item 3);
- the release agent allows only two UDS read services, checked against the program and against the
  first byte of every request (ADR-235 item 8);
- interrupted write jobs are recovered from the `RoutineControl` and `FlashTransfer` primitives and
  from UDS reset and session queries (design 8.2.5, ADR-245, ADR-250);
- `diag.request` takes a one-byte service identifier described as a UDS one.

Older ECUs speak other application layers over the transports the workers already support, KWP2000
on K-line or CAN among them. Their services follow the same request, positive response, negative
response pattern, but the same intent maps to different services, sub-functions, identifier widths
and response layouts: reading DTCs or running a routine is a different request, and a routine's
results can need a second service. Procedure authors want to keep writing `diag.read_dtc` and
`diag.routine` for such ECUs, because those names say what the procedure does, where a
`diag.request` with a protocol-specific service identifier does not.

The question is where the per-protocol behaviour of each call lives. If the agent's host grew a
branch per protocol for building requests and reading responses, every new application layer would
be a core change redistributed to every agent, and protocol differences would leak below the
frontend, against the design invariant that format differences stop at L3 (design 8.2) and the rule
that the subset is widened by updating the frontend (design 8.4).

## Decision

1. **Calls stay protocol-neutral.** `diag.read_dtc`, `diag.routine` and `diag.security_access` keep
   their names and arguments as statements of intent. A procedure does not change when the ECU's
   application layer does. `diag.request` keeps addressing a declared service by its service
   identifier and constants (`csv-tables.md` 5.3), which are those of the ECU's own application
   layer.

2. **Application-layer profiles.** The frontend holds one profile per supported application layer,
   starting with UDS. A profile is data, not code in the agent. For each call it gives:
   - the service or sequence of services the call expands to, with their constants (service
     identifier, sub-function) and the request fields the call's arguments fill, including their
     widths;
   - how the response of each service maps to the call's result fields, under names shared by every
     profile (for example the DTC list and status of `read_dtc`), so a procedure reads the same keys
     whatever the application layer;
   - the rules a sequence needs that are not one request, such as the key request that follows a
     seed;
   - the shape of a negative response and the codes that mean response pending, within the limits of
     item 5.

   Profiles ship with the frontend. Adding one, or correcting one, is a frontend update (design 8.4,
   9.2), apart from the agent-side items 4 and 6 name.

3. **The variant names its application layer.** Each protocol a variant declares names the
   application layer used over it, and every protocol of one variant must name the same application
   layer, or the definition is rejected at ingestion. A variant's procedures are packaged as one
   program per variant (ADR-307), and their lowered request bytes belong to one application layer,
   so a variant whose protocols mixed layers would have no single program for both; an ECU reachable
   over two application layers is declared as two variants. The frontend lowers every call of the
   variant's procedures through that layer's profile into the variant's declared services. A call whose intent the
   profile does not define, or a result field the profile does not map, is an ingestion error; a
   procedure never reaches the agent with a call its ECU cannot serve.

4. **Reads and routines become a generic send-receive.** `ReadDtc` is replaced by one generic
   send-receive: the complete request bytes, built by lowering, plus a reference to the declared
   service whose response it expects. The host matches the answer against that service's declared
   positive response (its leading bytes and the constant fields it echoes) and against the
   negative-response shape the IR declaration carries for the link's protocol, and it builds no
   request bytes for it. `SecurityAccess` stays, since turning a seed into a key goes through the
   server, and takes its seed and key requests from the profile. On UDS, `diag.routine` keeps
   lowering to `RoutineControl`, which recovery relies on (item 6); on any other application layer
   it lowers to the generic send-receive. The release agent's read-only rule (ADR-235 item 8) stays
   a check in the agent, at both of its checkpoints: its allowlist becomes one list of read-only
   service identifiers per application layer, kept in agent code, and the generic send-receive is
   checked against the list of the link's application layer. A new application layer therefore
   needs an agent update for its read-only list until the policy layer of design 5.5, 5.6, 6 and 8.9
   replaces the allowlist; the IR never declares its own services read-only, since the agent must
   not take that from the program it is guarding against.

5. **Negative and pending responses stay within what the workers support.** Response pending is
   absorbed in the worker (ADR-235 item 3) through the D-PDU response-code ComParams, and
   `j2534-0404-service` recognises a pending response only as a `7F` marker, the request's service
   identifier and a code at the following byte, with the codes 0x78, 0x21 and 0x23. A profile must
   use that shape and a subset of those codes, and the frontend rejects one that does not. An application layer outside
   it needs a worker extension, which is a core update (design 9.7), and a further decision.

6. **Flash transfer and recovery stay UDS-only.** `diag.flash_transfer`, the `FlashTransfer`
   primitive with its host-built TransferData requests and block counter (ADR-250), the recovery
   boundaries the recovery model finds from `RoutineControl` and `FlashTransfer`, and the reset and
   session queries a restart sends (design 8.2.5, ADR-245) are unchanged and remain UDS. A procedure
   of a variant whose application layer is not UDS that calls `diag.flash_transfer`, or that needs a
   recovery declaration, is rejected at ingestion. Extending reprogramming and its recovery to
   another application layer is a further decision.

7. **Scope.** Profiles cover application layers that follow the request, positive response, negative
   response pattern. A protocol without it (block-oriented manufacturer protocols with per-byte
   handshakes) is not a profile: its framing and timing belong to the worker's L1 (design 8.1, 9.7),
   and exposing its services would need a further IR decision.

8. **Relation to earlier ADRs.** This replaces ADR-235 item 2's request building for `ReadDtc`,
   item 3's fixed UDS positive and negative response patterns for the generic send-receive, and item
   8's single UDS allowlist (by the per-application-layer lists of item 4; its two checkpoints stay).
   Item 3's handling of unsolicited frames, late answers, failures and lost events is unchanged.
   ADR-245 and ADR-250 are unchanged (item 6).

## Consequences

- The UDS profile reproduces today's behaviour, so the lowering of `diag.read_dtc`,
  `diag.routine` and `diag.security_access` that M2 still needs goes through it from the start
  rather than being written against the UDS-specific request building and reworked later.
- The IR declaration part gains the per-protocol negative-response shape, and the protocol
  declaration gains its application layer. The agent then reads its matching rules from the program
  instead of from code, which is also what filling the link from the IR (ADR-235 item 4) needs.
- Adding an application layer is a frontend update for its requests and responses, but an agent
  update for its read-only list (item 4), and a worker update if its negative or pending responses
  differ from the supported shape (item 5).
- A KWP2000 profile needs the KWP2000 application-layer standard, which the project does not hold
  yet, and running it end to end also needs a non-CAN link in the agent (K-line addressing in the
  protocol declaration, ADR-235 item 4) and a simulated ECU that speaks it. Until then UDS is the
  only profile.
- The result fields shared across profiles become part of the procedure API (`starlark-subset.md`
  3.4). A field one application layer cannot provide is absent for it, and a procedure that reads it
  is rejected at ingestion for that variant.
- Each profile is tested the way the IR is: a known call lowered through the profile gives the
  expected request bytes and decoded result fields (design 12.1), and runs through a worker against
  a simulated ECU, since the pending handling of item 5 lives in the worker.
