# ADR-353: Event Items Name the Clock of Their Time

**Date:** 2026-10-25
**Status:** Accepted
**Affects:** `vci-service-interface` (`service.proto`: `EventItem.timestamp_clock`,
`TimestampClock`), `j2534-0404-service` (`ReceivedFrame.clock`, every `EventItem` it builds),
`iso22900-service` (event items), `agent` (`host.rs` `unattached_items`, `comm_log`'s
`unattached` event), `docs/rpc-api-guide.md`, `docs/j2534-0404-architecture.md`; design 8.11
"On one clock", design principle 6, ADR-120

## Context

Design 8.11 takes a difference only between two VCI times on the same clock and epoch, and
tags each VCI time `device` (the VCI's own clock) or `worker` (a worker's synthetic clock,
ADR-120). Which clock an item's time is on depends on where the time came from:

- a D-PDU library times every item itself, on one time base per module (ISO 22900-2:2022
  8.1.6.1);
- a J2534 library times only the messages it reads (`PASSTHRU_MSG.Timestamp`, SAE J2534-1
  v04.04 clause 8.2); `j2534-0404-service` times its error, status and information items, and
  the frames it makes up itself (the K-line fast-init response), on its synthetic clock
  (`module_timestamp_us()`, ADR-120).

`EventItem.timestamp` carried the time alone. To tag it, the agent would have had to know which
worker sent the item and that worker's timestamp sources, which design principle 6 forbids: the
agent does not branch on the worker, and the worker that knows a difference absorbs it. The
first version of the communication log's `unattached` events did tag the clock by the job's API
in the agent; that was removed, and the events gave `vci_us` with no clock.

## Decision

1. **Each event item names its clock.** `EventItem` gains `TimestampClock timestamp_clock = 11`:
   `TIMESTAMP_CLOCK_DEVICE` (1) for the VCI's own clock as the library reports it,
   `TIMESTAMP_CLOCK_WORKER` (2) for the worker's synthetic clock, and
   `TIMESTAMP_CLOCK_UNSPECIFIED` (0) when the worker does not say. Zero is also what a worker
   built before the field sends, so an older worker reads as one that does not say.
2. **The worker that made the time sets it.** `iso22900-service` gives `DEVICE` on every item.
   `j2534-0404-service` carries the clock on each buffered frame (`ReceivedFrame.clock`):
   `DEVICE` for a frame it read, a reassembled or concatenated message included (its time is the
   library's), and `WORKER` for the fast-init response; every error, status and information item
   is `WORKER`.
3. **The agent takes the clock from the item alone.** An unknown or unspecified value gives no
   clock, and a time with no clock is compared with no other. The `unattached` event writes the
   `clock` beside `vci_us` when the item names one.

## Consequences

- The agent can compare VCI times of items from any worker without knowing which worker it is,
  as the settlement of design 8.11 ("Settlement", "Timing judged") will need when it aligns VCI
  times per clock.
- The agent writes no exchange VCI times yet (`times.vci`, the pendings' `vci_us`); when it
  does, they take their clock from their items the same way.
- Times outside event items still carry no clock: `GetTimestamp`'s `TimestampResponse`,
  `GetStatus`, and `ErrorEventData` in an RPC's error detail. The agent uses none of them today.
  One the agent compares with an item's time will need the same field rather than a guess from
  the worker.
- A frame `j2534-0404-service` adds later that it makes up itself must set `WORKER`;
  `ReceivedFrame.clock` has no default, so the compiler asks for it at each construction.
