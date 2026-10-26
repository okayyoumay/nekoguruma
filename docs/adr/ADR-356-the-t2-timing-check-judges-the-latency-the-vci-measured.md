# ADR-356: The T2 Timing Check Judges the Latency the VCI Measured

**Date:** 2026-10-26
**Status:** Accepted
**Affects:** `ngr-hw-checks` (`guard/handle.rs` `GuardedHandle::send_receive_timed`, the link's
indications, `profile.rs` `Vci::timing_margin_us`, `checks/t2_timing.rs`),
`crates/j2534-0404-service/tests/t2_timing_check.rs`,
`crates/ngr-hw-checks/docs/hardware-profile.md`; design 13.5 ("Hardware profile"), design 8.11
("Timing margin", "Timing judged", "On one clock"); ADR-275, ADR-333, ADR-353

## Context

`hw_t2_timing` sends the default session request and a few TesterPresent requests to a bench
ECU. Until now it reported only the agent's round trip of each request. Design 8.11 "Timing
judged" never lets an agent time decide lateness: it starts before the RPC and includes the
worker and the event path. So the check could not say whether the ECU answered within the P2 it
reported.

The guarded handle kept no event times. Its links also did not turn on the transmit or
start-of-message indications (`CP_TransmitIndEnable`, `CP_StartMsgIndEnable`, ISO 22900-2 (2022)
B.5.1), although the agent's link does and the guard already admits both.

The maintainer decided three things:

- the hardware profile gains an optional `timing_margin_us` in its `[vci]` table, the key the
  agent is to read when it takes a link's timing margin from a profile;
- without that key the check records the latency and judges nothing;
- times are compared only on one clock (ADR-353), and a pair on different clocks, or with no
  clock, is reported unjudged.

## Decision

1. **The handle gives the VCI times of an exchange.**
   - On T1 to T3, `open_link` turns both indications on, as the agent's link does. A library
     that refuses one, or does not know it, leaves the link without it. A raw T0 link has no
     exchanges to time, so it turns neither on.
   - `send_receive_timed` returns the final response with three times:
     - the transmit done;
     - the response's start;
     - the response's end.
   - Each time keeps the clock its item names; an item that names none gives a time compared
     with no other.
   - The times come from the link's event items, read the same way for every worker (design
     principle 6):
     - a transmit or start-of-message indication bound to the primitive;
     - an unbound one whose header carries the profile's request identifier (for a transmit) or
       response identifier (for a start);
     - the final response's own item, which is the end. It may also carry its request's
       transmit and its own start (ISO 22900-2 (2022) 10.1.4.11.4), and those then take
       precedence.
   - An indication is never taken as the response.
   - The handle sends one request at a time on a link. So among the unbound indications, it
     keeps the latest one before the response; one after the response belongs to another
     exchange.
   - `send_receive` keeps its signature and drops the times.
2. **The check judges by design 8.11 "Timing judged".**
   - Each request's latency runs from the transmit done to the response's start (judged both
     ways), or, without a start on that clock, to its end (judged one way only).
   - Both times must be on one clock, and the later must not read earlier. The 32-bit values
     are not unwrapped, so a wrap or a reset gives no latency rather than a wrong one.
   - The limit is the P2 the ECU reported in its session response, plus the profile's margin.
     The same P2 applies to the TesterPresent requests, since they are sent in that session.
   - A response end past the limit proves the response late only when the response fits one
     frame under every addressing format (six bytes at most), and only by more than one frame's
     transfer at the link's baud rate (an upper bound of 160 bits per classic CAN frame).
   - Without the margin key, the latency is recorded and nothing is judged.
3. **A late response is recorded, not a failure.** The check measures the ECU, as it already
   does for P2 values that differ from `sim-ecu`'s. Each request's latency, `measured_by` (the
   design 8.11 name) and verdict go in the report's message, beside the agent's round trips,
   which stay reported only.
4. **The check keeps its own copy of the indication rules.** It matches the indications by the
   RxFlag bits the agent's communication log reads: a transmit indication, and a start of
   message that is neither a transmit indication nor a transmitted message. The handle's
   outcome rules already mirror the agent host's in the same way. Making the log's private
   matchers public would tie the two crates for three bit tests.

## Consequences

- On a real VCI the check now says whether a bench ECU answered within its own P2. This holds
  once the profile gives a margin, the library gives a transmit time, and the two times share a
  clock.
- On J2534, the transmit indication and the response are both on the device clock, so their
  difference is measured. `sim-vci` gives no start of message for a single frame, so CI
  measures to the response's end.
- The D-PDU mock gives no timestamp flags, so on `iso22900-service` the shape of the D-PDU
  items is covered by the handle's unit tests, not end to end.
- Every guarded link on T1 to T3 now has the indications on. The handle's other readers ignore
  them: an indication is never a response, and the raw frame reader serves only T0.
- The key is the one the agent will read. Its timing margin, still `source` `none` in
  `link_opened`, comes from a profile once the agent takes it from there.
