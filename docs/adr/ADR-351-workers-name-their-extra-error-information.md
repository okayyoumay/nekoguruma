# ADR-351: Workers Name Their Extra Error Information, and the Agent Does Not Interpret It

**Date:** 2026-10-24
**Status:** Accepted
**Affects:** `vci-service-interface` (`service.proto`: `ErrorData.extra_error_info_name`),
`iso22900-service` (error event items, the MDF read at startup), `j2534-0404-service` (error
event items), `ngr-library-resolver` (`iso22900::mdf`), `agent` (`host.rs` `events_seen`,
`comm_log::exchange::Detail`), `iso22900-mock`, `docs/rpc-api-guide.md`; design 8.11, ADR-346
item 6, ADR-350 item 1

## Context

ADR-350 item 1 gave an error event its extra error information as the library's code
(`ErrorData.extra_error_info`, ISO 22900-2:2022 D.4.2), passed on without interpretation, and
left the agent to use a code "only when it knows the library's code". Design 8.11 then gave a
few error cases a more precise outcome than `link_error` when the extra information names them:
a frame error that names the response's frame (`response_malformed`), a receive error that names
a missing acknowledge (`not_sent` by a transmit error), a flow-control wait past its limit
(`no_flow_control`, `vehicle`).

A D-PDU library's codes are its own. The library's module description file (MDF, ISO
22900-2:2022 Annex F, F.4) lists each code with an ID, a short name and a description, so a
reader can learn a code's name, but nothing in the MDF says which code stands for which of
those cases: the names and descriptions are the vendor's words. SAE J2534-1 v04.04 has no such
code at all. The J2534 worker can still tell several of the cases apart itself: when it runs
ISO 15765-2 in software (`can_channel_mode = "software-isotp"`, ADR-046), it sees every frame and
timer, and on a native ISO 15765 channel it sees the receive status bits and the start-of-message
indication.

The agent therefore had no way to map a D-PDU code to a case, and no code to map on J2534.

## Decision

1. **The worker names the information; the interface carries a name beside the code.**
   `ErrorData` gains `optional string extra_error_info_name`. The interface defines no codes and
   no names: each worker gives the name, from what only it knows.
   - The D-PDU worker reads the `ERROR_CODE` elements of the MDF the root file entry names
     (Annex F, F.4) when it starts, after the design 7.2 checks and while the checked files are
     held, and gives each error item the code's `SHORT_NAME` beside the code. An MDF that is
     missing or cannot be read leaves every code without a name and does not stop the worker
     (it is logged). An element without one usable `ID` and one non-empty `SHORT_NAME` is
     skipped, and an ID two elements name differently has no name, so a wrong name is never
     given.
   - The J2534 worker gives a name, with no code, for a case it tells apart itself. Where an
     example identifier of ISO 22900-2:2022 Table D.8 fits the case, the name is that
     identifier. Until it tells a case apart it gives none.
2. **The agent does not interpret the extra error information.** An exchange entry's `detail`
   gives the code (`extra_error_info`, a number) and the name (`extra_error_info_name`) of the
   event `detail` names, as the worker gave them. Neither changes the entry's outcome, `sent` or
   classification: an error event is `link_error` whatever its extra information. This holds
   for both workers, so the agent needs no table of any library's or worker's codes.
3. **The cases that rested on the extra information leave design 8.11.** `response_malformed`,
   `no_flow_control` and `not_sent` by a transmit error had no other source, so they are no
   longer outcomes; a frame error, a protocol error and a transmit or receive error are
   `link_error`, named in `detail`. A request the protocol cannot carry stays `not_sent` through
   the worker's refusal reason (ADR-350 item 2). `response_incomplete` stays, since a
   start-of-message indication without a response can show it.

## Consequences

- A developer reading the log sees the vendor's or the worker's name for an error, and the code,
  but the log's classification of such an exchange stays coarse (`undetermined` for most link
  errors), as for a link error without extra information.
- The MDF is read by the worker that loads the library, so the name always matches the library
  the worker loaded; the agent no longer reads the MDF.
- A D-PDU code with no name in the MDF reaches the log as its number only. A name with no code
  comes only from the J2534 worker.
- `ErrorData`'s tag 3 is new; a peer built before it ignores the name.
- The J2534 worker's names for the cases it can tell apart are follow-up work: in software ISO
  15765-2, a flow control that does not come, a wait past its limit, an overflow, a consecutive
  frame that does not come or is out of sequence, and a malformed frame; on a native channel,
  a padding error the receive status reports and a response whose start was indicated but
  which did not complete.
