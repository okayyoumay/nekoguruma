# ADR-367: The Log Settles a Primitive That Ends Before the Next Request

**Date:** 2026-10-27
**Status:** Accepted
**Affects:** `agent` host (`WorkerHost::send_recv_async`, `WorkerHost::settle_cancelled`,
`Settling`, `Stray::progress`) and communication log (`CommLog::primitive_ended`); design 8.11
("Settlement", `primitive_ended`); ADR-346, ADR-358, ADR-366

## Context

When the agent's own deadline ends a wait, the primitive may still run, and the host cancels it
before the link's next request (design 8.11 "Settlement", ADR-346 item 5). ADR-366 settles such
a primitive at job end. Before the next request, the host released what the log kept for the
exchange at the cancel (ADR-366 item 1), as if the primitive's end could never be read then.

But a primitive that ends between the deadline and the next request leaves its end on the
link's event stream, and nothing reads the stream between exchanges. The next exchange's wait
read those items. They were bound to another primitive, so the log wrote them `unattached`
with `primitive` `earlier`. A start-of-message among them could even make the next exchange
`response_incomplete`. Design 8.11 says the real end is known when the primitive ends before
that request, and that a cancellation before it is recorded too.

## Decision

1. **The cancelled primitive's end is read during the next exchange.** After the cancel, the
   host keeps the stray primitive's wait (`Settling`), from what its exchange's wait had read
   (`Stray::progress`, shared with the job end). It does so only when the exchange's entry was
   written. The next exchange's wait gives it the primitive's own items until its end is read.
   It also gets lost events and a lost VCI of no primitive, which end the next exchange as
   well. An indication of no primitive stays with the exchange in flight, which may own it
   (design 8.11 "Link settings for the log"). This differs from the job end, where no exchange
   is in flight.
2. **The end is the one the job end would give** (ADR-366 item 2 and the classification of
   `exchange::settle_primitive_ended`):
   - its finished status, a lost VCI or lost events end it as its exchange's wait would have;
   - its cancelled status ends it with the end read before it (a final response or a receive
     timeout), as a wait that passes before the job end's cancel does. With none, it is the
     agent's cancel when the worker carried that cancel out; otherwise the status ends it as
     its exchange's wait would have read it, since the agent's cancel did not give it;
   - when the next exchange's wait ends first:
     - an event stream that failed or ended gives the `unknown` end;
     - else the end read so far settles it;
     - else, when the worker carried out the cancel, it is the agent's cancel;
     - a primitive that had already ended (its handle invalid), or that a lost VCI ended, with
       no end read, writes nothing: its end is unknown, and the exchange keeps its
       classification.
3. **When the entry is written.** It is written when the next exchange's wait is over, before
   that exchange's entry. A request whose primitive does not start writes it at once.
   Afterwards the log releases what it kept for the exchange. Later items of the primitive
   are logged `unattached` with `primitive` `earlier`, as before. So are its results and
   errors read during the wait when no end of it is written, or only the agent's cancel, so
   none is lost.
4. **A handle given again.** When the library gives the next primitive the cancelled one's
   handle, the items cannot be told apart. The cancelled primitive is then settled at once,
   with what was read before its deadline, and its items are taken as the new primitive's.

## Consequences

- ADR-366 item 1's release at the cancel, and its Consequences' note that an end read before
  the next request is written `unattached`, are superseded.
- An agent's cancel before the next request is now recorded (`cancelled_by_agent`), as design
  8.11 lists it. Like a cancel at job end, it settles nothing.
- The end is read only within the next exchange's wait. An end that arrives after that wait,
  or after the cancelled status, is still written `unattached`.
- An end that is written carries what the job end's entry carries. As there (ADR-366
  Consequences), its rejected results and a start-of-message bound to it are not carried, and
  its pendings get the time the host wrote the entry as their agent time.
