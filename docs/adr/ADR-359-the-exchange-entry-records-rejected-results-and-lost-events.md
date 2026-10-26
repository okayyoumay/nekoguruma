# ADR-359: The Exchange Entry Records Rejected Results and Lost Events

**Date:** 2026-10-26
**Status:** Accepted
**Affects:** `agent` host (`host.rs` `Progress::on_event`, `RejectedRead`, `is_indication`) and
communication log (`comm_log/exchange.rs` `Rejected`, `RejectedItem`, `EventsLost`, the exchange
entry), `sim-ecu` (`Fault::TruncateResponse`), `crates/agent/docs/ngr-agent.md`,
`crates/sim-ecu/docs/simulated-ecu.md`, `crates/sim-vci/docs/simulated-vci.md`; design 8.11
("Exchange entry")

## Context

The host takes a result bound to the exchange's primitive as the answer only when it answers the
request. A result answers it when it is a positive response that begins as the request's does, or
a complete negative response to its SID. This is stricter than the worker's match, which also
accepts a response shorter than its pattern. A result that did not answer was dropped, so the
log never showed that the ECU had sent something.

Lost events ended the exchange as `unknown` (`EventsLost`). The entry did not say when the loss
was read.

The maintainer decided where the entry records both, and what it says of lost events.

## Decision

1. **The exchange entry gains a `rejected` list.** It has one item per result bound to the
   primitive that the host did not take as its answer. Each item gives:
   - the response identifier: the header's when the worker split one off, else the link's;
   - the bytes, redacted as a response's are;
   - the VCI time with its clock, and the agent time the host read it;
   - the reason:
     - `wrong_echo`: not negative, and not beginning as the request's positive response does;
     - `pattern_mismatch`: a negative response that is not a complete one to the request's SID.
2. **Indications are not results of the ECU.** A result whose RxFlag marks a transmit or
   start-of-message indication, a transmitted message or a break is not listed (ISO 22900-2:2022
   D.2.2).
3. **A rejected result counts for nothing else.** It counts for neither the classification nor
   `link_history`'s `answered`. It is evidence for a reader, not an answer of the ECU's.
4. **The exchange entry gains `events_lost`.** When the worker reports lost events, it records
   the agent time the host read the report and how many such reports it read. The host stops
   waiting at the first report, so there is one. `LostEventItemNotification` carries no count,
   so the interface does not change. The outcome stays `unknown`.
5. **The simulator can send a response that does not answer.** `sim-ecu`'s
   `Fault::TruncateResponse { len }` cuts the next positive response to its first `len` bytes.
   The J2534 worker passes the cut response on as the primitive's result, and the host rejects
   it.

## Consequences

- A read the ECU answered wrongly is no longer logged as silence alone: its entry shows what came.
- The entry lists rejected results only. The `answers` list, for a request that collects every
  answer, keeps its own meaning.
- A rejected result does not change how the exchange is classified. Whether one should, for
  example as evidence that the request reached the ECU, is left open.
