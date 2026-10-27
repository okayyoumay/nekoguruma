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
   one the VCI ended. A late response still settles only an exchange the VCI ended. When the
   host cancels such a primitive before the link's next request, its end is never read, so the
   log drops what it kept (`CommLog::release_kept`).
2. **The host reads the primitive's end as its exchange would have.** The stray primitive
   remembers:
   - its request;
   - the answer it waits for;
   - its entry's `seq`, only when the agent's deadline ended the wait (a receive timeout read
     before the deadline makes no `vci_did_not_end` entry, so nothing is settled);
   - what the wait had read of its end before the deadline: a final response with its times,
     and the error events.

   At job end the host feeds the primitive's own items, and a lost VCI, through a `Progress`
   for that answer that starts from what the wait had read. The first end it reaches is the
   primitive's end: a response, a receive timeout, a failure, a lost VCI or the finished
   status. The end, with its VCI times and the pendings it passed on, goes to
   `CommLog::primitive_ended`, with every link error read at job end beside the one that
   ended it, so the classification takes the most severe. A lost-events notice gives the
   `unknown` end (`events_lost`), and the primitive, which may still run, is then cancelled;
   an event stream that fails or ends gives the `unknown` end (`events_ended`). When the job-end wait passes, or an earlier cancel failed, the host
   cancels the primitive and records the agent's cancel only when the cancel succeeds; a
   handle the worker no longer knows, or a cancel that fails, leaves the end unknown and writes
   nothing.
3. **The entry gives the end as an exchange entry gives its end.**
   - The log builds the exchange's evidence again with that end and with the new pendings
     after the kept ones.
   - The end's VCI times are placed nearest their clock's latest time, since the host reads
     them at job end.
   - Timing is judged against the kept transmit and limits, with the new pendings' intervals.
     A final response ends the exchange as an owed request. The exchange is never counted as an
     owed request its own answer could belong to.
   - A transmit or start-of-message indication of no primitive read at job end is the
     exchange's when no other exchange on the link could own it, as during the exchange: it
     confirms the send, or shows the response began. The latest start is the response's start
     time when the response spans frames and no pending came after it, as for an exchange.
   - A receive timeout after a response began gives `response_incomplete`, as for an
     exchange.
   - The entry carries `refers_to`, `link`, `sent`, `sent_evidence`, `outcome`, `answers`,
     `pending`, `timing`, the end's VCI times (`vci`), `detail`, `text` and `classification`,
     and `exchanges` with the exchange's origin, request, classification and reason.
4. **The classification.**
   - The end takes the exchange rows' classification.
   - A final response is `system` whatever its time: within the limit in force the job gave up
     on an exchange that ended in time, and after it the late row is `system` too, so a time
     that cannot be judged changes nothing.
   - A receive timeout at the limit (`timeout_at_limit`) is `system` for the same reason.
   - Any other end keeps its row: a lost VCI, a failure of the system, a link error, a timeout
     early or late.
   - The `vci_did_not_end` entry's `system` row rests on the VCI's own clock past its limits
     with no response under way. An end that shows no response under way at that reading (a
     link error, a lost VCI, a timeout) does not refute it, so `system` stands; a final response
     is `system` anyway. A start-of-message read at job end from at or before that reading, or
     one whose order with it is unknown, refutes it, and the end takes its own row.
5. **The agent's cancel settles nothing.** The entry has `cancelled_by_agent` true and no
   `outcome` or `timing`. The exchange keeps its classification.

## Consequences

- An end read before the link's next request, during the next exchange, is still written
  `unattached`; that path is left to its own backlog item.
- The host takes no read time for the end's items at job end, so a pending's agent time is
  the time the host wrote the entry.
- The rejected results of the primitive and a rejected result that ends it
  (`positive_unusable`, ADR-359) are not carried into the entry.
- A start-of-message indication the primitive gives at job end is not taken as its response's
  start, as for a bound indication during an exchange.
