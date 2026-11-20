# ADR-495: The Uploader Classes Every Answer, Passes Over What the Server Refuses and Abandons Only a Source It Proved Broken

**Date:** 2026-11-20
**Status:** Accepted
**Affects:** design 5.2, 5.4; `crates/agent` (`uploader::{http, part_url, routes}`, the uploader's loop, `upload_ledger`, the seams of `control::{exchange, dial, backoff}`), `crates/agent/docs/ngr-agent.md`; amends ADR-493 items 6, 11 and 17, details its items 10, 12 and 13

## Context

ADR-493 gives the data channel's four routes and the agent's ledger, and says what each route
answers, but not what the agent does with each answer. Items 6 and 17 tell the agent to begin
again after a `failed` completion and to repeat the status while it answers 503, both without a
limit; item 11 lets an entry leave the ledger only on `finished`; and its Consequences leave open
what clears an entry the server refuses for good, or whose source is gone, while one object at a
time leaves every entry behind it waiting. Item 12's no-progress rule and item 13's host rule
also need a form the code can apply byte by byte. The maintainer decided these on 2026-11-20,
from a case table of every answer of every route.

Two of ADR-493's rules turn out to deadlock or never end as written. When the object server has
lost an upload, the status answers the same 503 as a store outage, and only a completion writes
the row `failed`, so a status repeated without end never sees the change. And a completion that
keeps finding a digest mismatch would send the whole object again and again, up to 4 GiB a time.

## Decision

### The client (built with this ADR)

1. **Every answer is one of four outcomes for the loop.** A typed result (begun, a status, a part
   URL, a completion), or a failure classed as:
   - **a new exchange**: a 401 from any of the four routes; the request is repeated once with the
     new token;
   - **retry** (after the back-off): no answer, a cut, no progress, a 3xx (never followed), 408,
     429, 502, 503, 504, a 409 on a part URL (the next attempt's begin re-reads the row), any
     failed PUT, and a part URL that breaks the host rule;
   - **pass over**: begin, status, part URL or completion answering 400, 404, 413, 500, begin's
     409, any other 4xx, or an answer the route does not promise (an upload ID that is not a
     canonical UUID, a part count other than the one the agent computes from the length and
     the part size, or above 10000, `partsHeld` above the count). A 500 is a pass-over because
     it can belong to one entry alone (a stored instruction that does not decode).
   A completion 409 with no `state` is a pass-over too.
2. **No progress is judged per connection.** 30 seconds without a byte read or written, counted
   from the dial, drops the connection; a connect takes at most 5 seconds; a JSON route takes at
   most 60 seconds in all, so a server cannot hold the task with an answer trickled a byte at a
   time. A PUT that keeps moving has no total (design 5.4); its answer body is never read.
3. **The host rule is a byte prefix.** A part URL must begin with the registration origin's
   scheme and host exactly, then `/`, or `:` and a port from 1 to 65535 written without leading
   zeros and then `/`; it holds no userinfo, `#` or non-graphic byte and at most 4096 bytes. The
   rest is sent unchanged as the request target, the authority as `Host`, and no token goes with
   it. The dial is to the IPv4 loopback address only, so no URL can reach another host.
4. **The uploader runs its own exchange through the control channel's code.** The exchange and
   the dial take their dependencies as seams and log through a small trait, so the control
   channel keeps its behaviour and its log lines (ADR-493 item 10: neither channel waits on the
   other).

### The loop (built with the uploader's loop)

5. **Every attempt on an entry starts at begin**, after the source check, and ends in a delivery
   (`finished` removes the entry), a retry, a pass-over or an abandon. A retry in the same run and
   a restart take the same path. After any attempt that stored no part and got no `finished`,
   the next request waits the back-off, so no answer can make the loop spin.
6. **A refusal is passed over in memory.** The entries behind it are tried first, and it is tried
   again on every pass, so a repaired or upgraded server receives it with no agent restart.
   Nothing durable changes, and a refusal never removes an entry.
7. **An entry is abandoned only when the agent proved its source broken** (this amends ADR-493
   item 11): the source file itself is missing, shorter than the entry's length, or its first
   `length` bytes hash otherwise. `UploadLedger::abandon` removes the entry as `finished` does,
   with a stored result's file; a communication log's file is never touched. Any other error
   reading the source is a retry.
8. **The source is verified once per run**, when the entry is first taken, and again after a
   `failed` completion. A change that keeps the length and comes during the transfer is sent and
   caught by the server's read-back.
9. **A `failed` completion begins again once** (this amends ADR-493 item 6): after the source is
   verified again, a new begin; a second `failed` in a row sets the entry aside until a restart.
10. **A 503 status on an `open` row is followed by one completion in the same attempt**, whose
    answer decides (this amends ADR-493 item 17): a lost upload becomes `failed`, and with parts
    merely missing the row stays `open`, so the call is harmless during a real outage.
11. **Refusals and abandons are warned once per entry and class** (its number, kind, class and
    status, never a URL, token or digest) and counted in the uploader's status. They are not
    events of the job (ADR-493 item 14).

## Consequences

- An entry the server refuses for good costs one begin per pass, at most one a minute at the
  back-off's cap, and one exchange per 300 seconds, for as long as it stays in the ledger.
- An abandon is seen in the agent's log only; telling the operator is later work.
- No route gives up an `open` row, so after an abandon, or after a `finished` row, an enqueue of
  the same job, generation and kind with other bytes is refused by begin for good; the server
  needs such a route.
- A part-URL 409 is retried rather than passed over: the row has moved on (completing, finished
  or failed), and the next begin reads where.
- The JSON routes' 60-second total is wider than the server's 20-second completion wait, which
  answers 202 before it.
