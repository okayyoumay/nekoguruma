# ADR-364: The Log Proves a Single-Frame Response Late by Its End

**Date:** 2026-10-26
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log/timing.rs` `judge`, `Limits`, `Times`;
`comm_log.rs` `link_opened` and `CommLog::exchange_with`; the late-pending row in
`comm_log/exchange.rs`), `link.rs` (`LinkConfig::single_frame`, `CAN_FRAME_BITS`,
`CAN_SINGLE_FRAME_BYTES`); design 8.11 ("Timing judged", "Limit in force", `timing`); ADR-346,
ADR-358, ADR-362

## Context

A received message's VCI time is its end (ISO 22900-2:2022 8.1.6.3), and a start-of-message
indication comes only with a multi-frame response's first frame (8.1.6.5). Many responses on
CAN therefore reach the log with an end and no start. ADR-362 item 4 had the log judge such an
end one way only: an end within the limit proves the response on time, and one past it proves
nothing, since the end includes the response's own transfer.

Design 8.11 goes further for a response sent as one frame. Its transfer is short and bounded,
so an end past the limit plus the margin plus that bound shows that the response began after
the limit. A forwarded pending is the same case: it is always one short message, and its time
is its end. To use either, the log needs to know two things: which responses went as one frame,
and how long one frame can take on the link. Neither is in a ComParam the log reads.

## Decision

1. **One frame, on UDS on CAN, is a response of at most six bytes.** ISO 15765-2:2024 9.6.2.1
   lets a classic CAN single frame carry seven bytes with normal addressing and six with
   extended or mixed addressing. The log takes six, so the rule holds whatever addressing the
   link uses. A seven-byte response is not judged by its end. A response pending (`7F`, the
   SID, `78`) is three bytes, so it is always one frame.
2. **One frame takes at most 160 bits at the link's baud rate.** This is the bound ADR-358
   takes for a classic CAN frame with stuffing, now named `CAN_FRAME_BITS` and shared by the
   job-end wait and the log. `LinkConfig::single_frame` gives the length and the transfer; on
   any other protocol it gives none, and on such a link the log proves nothing by an end. A
   UART protocol's bound, which design 8.11 builds from the baud rate and the inter-byte limit,
   is defined when the agent first opens a link on one.
3. **A single-frame end proves lateness by the limit's latest bound.** The end must be past the
   latest limit in force (the expected one on a link that forwards pendings), plus the margin,
   plus the frame's transfer. `window_by_ms` is the end less the transfer less that limit: the
   least by which the response began late. Between the expected limit and that point the end
   stays `not_judged` with `end_after_limit`.
4. **Each forwarded pending is judged by the limit in force before it.** That limit comes from
   the pendings before it, and on a link whose VCI absorbs pendings it is the latest bound
   (ADR-362 item 3). A pending past it, by the rule of item 3, makes the window `late`. The
   window names the pending in `late_pending` (its place in `pending`, from 0), and this decides
   the window whatever follows: a response, a timeout or a loss. The first late pending is the
   one named.
5. **The late-pending row classifies.** Any outcome after such a pending is `system`, at the
   row's place in design 8.11's table (after the `not_sent` rows). The reason names the
   pending, by how much it was late, and the outcome that followed. The entry's text does not
   call the final response late, since the window is the pending's.

## Consequences

- ADR-362 item 4 is narrowed: an end past the limit still proves nothing, except under item 3
  above.
- The rule of item 1 leaves a seven-byte single frame unjudged by its end. Taking the link's
  addressing format from its ComParams would judge it too, but the agent opens no link with
  another format today, and the gain is one byte.
- The 160-bit bound is for classic CAN. A CAN FD link would need its own bound, defined with
  that link.
- `vci_did_not_end`'s `system` row, which compares a device clock reading with the limits,
  can use the same single-frame transfer for its "no response was under way" test.
