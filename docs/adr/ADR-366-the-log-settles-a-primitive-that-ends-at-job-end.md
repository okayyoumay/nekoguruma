# ADR-366: The Log Settles a Primitive That Ends at Job End

**Date:** 2026-10-27
**Status:** Accepted
**Affects:** `agent` host (`WorkerHost::settle_at_end`, `log_primitive_end`, `Stray`) and
communication log (`CommLog::primitive_ended`, `PrimitiveEnded`, `PrimitiveEndedEntry`,
`exchange::settle_primitive_ended`); design 8.11 ("Settlement", `primitive_ended`); ADR-346,
ADR-358, ADR-365

## Context

When the agent's own deadline ends a wait, the exchange is logged `vci_did_not_end`, and its
primitive may still be running. At job end `WorkerHost::settle_at_end` waits for that primitive
for the job-end wait (ADR-358), and cancels it if it has not ended. Until now the host dropped
whatever the primitive gave in that time.

Design 8.11 has the primitive's end settle the exchange in a `primitive_ended` entry: it is
classified by the exchange rows as if the agent had waited, except that an end within the limit
in force is `system`. ADR-365 gave settling entries their form and kept evidence, but kept only
the exchanges the VCI ended.

## Decision

1. **The log also keeps an exchange the agent's deadline ended**, with what ADR-365 keeps for
   one the VCI ended. A late response still settles only an exchange the VCI ended.
2. **The host reads the primitive's end as its exchange would have.** The stray primitive
   remembers:
   - its request;
   - the answer it waits for;
   - its entry's `seq`.

   At job end the host feeds the primitive's own items, and a lost VCI, through a fresh
   `Progress` for that answer. The first end it reaches is the primitive's end: a response, a
   receive timeout, a failure, a lost VCI or the finished status. The end, with its VCI times
   and the pendings it passed on, goes to `CommLog::primitive_ended`. When the job-end wait
   passes, the event stream ends, or an earlier cancel failed, the host records the agent's
   cancel instead.
3. **The entry gives the end as an exchange entry gives its end.**
   - The log builds the exchange's evidence again with that end and with the new pendings
     after the kept ones.
   - The end's VCI times are placed nearest their clock's latest time, since the host reads
     them at job end.
   - Timing is judged against the kept transmit and limits. A final response ends the exchange
     as an owed request.
   - The entry carries `refers_to`, `link`, `sent`, `sent_evidence`, `outcome`, `pending`,
     `timing`, `detail`, `text` and `classification`, and `exchanges` with the exchange's
     origin, request, classification and reason.
4. **The classification.**
   - The end takes the exchange rows' classification.
   - A window of `within_p2`, `within_p2_star` or `timeout_at_limit` makes it `system`: the job
     gave up on an exchange that ended in time.
   - A lost VCI or a failure of the system keeps its own row.
   - The `vci_did_not_end` entry's classification is not carried over. Its `system` row
     (ADR-364's single-frame bound) inferred that no response was under way, and the
     primitive's end replaces that inference.
5. **The agent's cancel settles nothing.** The entry has `cancelled_by_agent` true and no
   `outcome` or `timing`. The exchange keeps its classification.

## Consequences

- An end read before the link's next request, during the next exchange, is still written
  `unattached`; that path is left to its own backlog item.
- The host takes no read time for the end's items at job end, so a pending's agent time is
  the time the host wrote the entry.
- A start-of-message indication the primitive gives at job end is not taken as its response's
  start, as for a bound indication during an exchange.
