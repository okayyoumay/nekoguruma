# ADR-390: The Dry Run Answers the Profile's Security Access Requests

**Date:** 2026-10-30
**Status:** Accepted
**Affects:** `diag-ir` dry run (`dry_run.rs` `dry_run`, `ScriptedHost::service_request`,
`Script::seed`, `PROFILE_SEED_REQUEST`, `PROFILE_KEY_REQUEST`);
`crates/diag-frontend/docs/starlark-subset.md` (3.2, 6); design 12.1; ADR-328 items 2 to 5

## Context

ADR-328 item 2 resolves every request of a dry run to a declared service and ends the run with a
host error for one that no service matches: a definition that sends what it does not declare is
what the validation is for. The lowering of `diag.security_access` (ADR-309 item 4, ADR-314)
sends its seed and send-key requests through the application-layer profile's templates, not
through declared services, so the variant needs no SecurityAccess service. A valid procedure
that unlocks such a variant therefore ended its dry run with a host error at the seed request.

The profile's requests are `ServiceRequest` instructions, as a `diag.request` is. The dry run
cannot tell the profile's request from a definition's own undeclared request by its bytes.
Answering every undeclared SecurityAccess request would hide the second, and marking the
profile's requests in the IR would change the format, its golden files and the fuzz seeds for
one call.

## Decision

1. **The profile's requests are found by their place.** Before the run, the dry run marks, for
   each `SecurityAccess` instruction, the nearest `ServiceRequest` before it as the seed request
   and the nearest after it as the send-key request, each only when no other instruction that
   reaches the host (a request, a wait, a log, a failure, an HMI or record step), and no call or
   return, lies between them. That is the shape the lowering emits.
2. **Only a marked request that no declaration matches is answered by the profile.** A seed
   request is answered with the profile's positive seed response, and a send-key request with
   its positive response, each echoing the request's SID and sub-function as UDS answers them. A marked request that resolves to a declared
   service is answered through that service as before, and an unmarked undeclared request,
   or a marked one that two declared services match equally well, still ends the run with a host
   error (ADR-328 item 2).
3. **The seed comes from the script, else a fixed one.** `Script::seed` holds whole seed
   responses, negative ones included, used in order as the other lists are. Without one the
   seed is four fixed non-zero bytes: a zero seed means the ECU is already unlocked. The key
   still comes from the script. The send-key answer is always built. A request that resolves to a declared service takes its
   answers from that service's scripted responses, as before, not from `Script::seed`.
4. **The report names them as the profile's.** Each answer is a request event with a name no
   declared service can have and no decoded value, since no declaration describes it.

The maintainer chose finding the requests by their place, over answering every undeclared
SecurityAccess request or marking them in the IR, and a built seed the script can replace
(2026-10-30).

## Consequences

- A procedure that unlocks a variant through the profile dry-runs to its end, so a conversion
  that dry-runs every procedure does not refuse it.
- The dry run depends on the lowering's expansion. A lowering that placed another host request
  between the steps would leave its requests unmarked, and the dry run would end with a host
  error, which shows the change rather than hiding it.
- The profile is UDS's. Another application layer's profile needs its answers here too.
- A definition's own undeclared SecurityAccess request placed right before or after a
  `SecurityAccess` step would be answered as the profile's. Only the lowering emits that step,
  so a definition cannot place one there.
