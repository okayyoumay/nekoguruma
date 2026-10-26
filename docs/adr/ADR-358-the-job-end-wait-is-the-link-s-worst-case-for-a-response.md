# ADR-358: The Job-End Wait Is the Link's Worst Case for a Response

**Date:** 2026-10-26
**Status:** Accepted
**Affects:** `agent` link (`link.rs` `LinkConfig::job_end_wait`), host (`host.rs`
`WorkerHost::settle_at_end`, `ends_primitive`), runner (the job's end, before the link
closes), `crates/agent/docs/ngr-agent.md`; design 8.11 ("Settlement")

## Context

When the agent's own deadline ends its wait, the primitive may still be running. Before the
link's next request, the host cancels it. Design 8.11 also has the agent, at job end, keep the
link open until the primitive ends or for the longest transfer of the largest response its
definition allows, then cancel it. Two things blocked this:

- the IR declares no largest response for a definition;
- a response's transfer depends on the flow control and on the ECU's frame spacing (ISO
  15765-2:2024 9.6.5, 9.8.1).

So until now the agent closed the link under the running primitive.

The maintainer decided the bound.

## Decision

1. **The bound is the link's worst case for a response already under way, not a definition's
   largest response.** `LinkConfig::job_end_wait` is the sum of two parts:
   - one wait for a consecutive frame (`CP_Cr`);
   - the transfer of the longest classic ISO-TP message, 4095 bytes (586 frames). Each frame
     takes at most 160 bits at the link's baud rate, plus the separation time the tester's flow
     control requests (`CP_STmin`).

   The sum is capped at 10 s.
2. **Unset ComParams take fixed values.** A `CP_Cr` the link leaves to the library counts as
   1 s, the timeout ISO 15765-2:2024 9.8.1 gives the receiver's wait for the next consecutive
   frame. An unset `CP_STmin` counts as none. On a 500 kbit/s link with neither set, the wait
   is about 1.19 s.
3. **No limit in force is added.** The agent's deadline already lies past every limit in force.
4. **The host waits, then cancels.** `WorkerHost::settle_at_end` runs before the link closes.
   - The wait ends early when the primitive's own finished or cancelled status arrives, or on a
     lost VCI, whose primitives the worker cancels (ADR-289).
   - When the wait passes, or the event stream fails or ends so that the end is unknown, the
     host cancels the primitive by its handle.
   - A cancel answered with an invalid handle, or a lost VCI, counts as done (as before the
     next request). Any other failure is logged and does not change the job's result.
5. **What the primitive gives meanwhile is read but not logged yet.** Its `primitive_ended`
   entry belongs to the settlement of the communication log.

## Consequences

- A job whose last exchange outlasted the agent's deadline ends up to the job-end wait later.
- The link is no longer closed under a running primitive, so a library that queues the close
  behind it no longer holds the VCI.
- An ECU that sends its frames more slowly than `CP_STmin` can outlast the wait. The cap also
  cuts off a slow link: below about 100 kbit/s, 586 frames take most of the 10 s. Either way
  the primitive is cancelled, and its end stays unknown.
- The settlement P1 can now write `primitive_ended` for an end read within the job-end wait.
