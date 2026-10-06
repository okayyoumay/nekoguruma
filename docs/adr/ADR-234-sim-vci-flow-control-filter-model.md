# ADR-234: sim-vci Flow-Control Filter Model

**Date:** 2026-10-06
**Status:** Accepted
**Affects:** `sim-vci` (`src/lib.rs`: `PassThruStartMsgFilter`, `PassThruStopMsgFilter`, `PassThruWriteMsgs`, `PassThruReadMsgs`, `PassThruIoctl`), `crates/sim-vci/docs/simulated-vci.md`

## Context

`sim-vci` stands in for a vendor J2534 v04.04 library in CI, so `j2534-0404-service` can be tested against real exports without hardware (design 13.4). On ISO 15765 channels, SAE J2534-1 7.2.9 and Appendix A make flow-control filters the gate for both directions:

- a message is received only through a filter whose pattern ID is its source;
- a segmented message is sent only when a filter's flow-control ID is its destination.

The simulator does not model segmentation itself. A request or response of any length moves as one message, with no flow-control frames on a bus. So it has to decide which of these rules it enforces and how. If the simulator is more lenient than a device, a service bug slips through CI. If it is stricter, or strict in a different way, correct code fails.

## Decision

1. **Only flow-control filters, validated as clause 7.2.9 and Appendix A describe.** `PASS_FILTER` and `BLOCK_FILTER` are refused on ISO 15765. The filter messages must agree:
   - same protocol, size and TxFlags;
   - a mask covering the whole CAN ID;
   - 11-bit IDs with normal addressing, the only kind the channel uses;
   - IDs unique across the channel's filters in either role, except that one filter may use the same ID for both;
   - at most ten filters per channel.
2. **Filters judge a response when it appears on the bus.** A response is queued with its delay. Its filter decision is made once, against the filters in place at the moment it appears. This is checked before anything reads the queue or changes the filters. A device filters what it receives, so a filter started during the delay lets the response in, and one stopped before it keeps it out. A response kept out stays lost.
3. **Segmented traffic needs the partner filter, in both directions.** The simulator counts a message as segmented when its payload exceeds a classic CAN single frame (7 bytes with normal addressing).
   - **Receive:** a segmented response is received only through a filter whose pattern ID is the response ID and whose flow-control ID is the responder's request ID, because that is where the device's flow control would go.
   - **Send:** a segmented request is sent only when a filter's flow-control ID is the request ID and its pattern ID is the responder's response ID, because that is where the responder's flow control would arrive.
   - A filter with the same pattern and flow-control ID handles single frames only.
4. **Functional requests are never segmented.** A request to the functional ID `7DF` that does not fit a single frame is refused with `ERR_NO_FLOW_CONTROL`, whatever the filters. This matches ADR-055 on the service side.
5. **Unsimulated addresses fall back to the general rule.** For a request ID no simulated responder listens on, the responder's response ID is unknown. A segmented send then needs only a filter whose flow-control ID is the request ID and whose pattern ID differs from it.
   - The simulator does not invent a partner relation it cannot check.
   - The service's filter set-up for such an address is still exercised.
   - Nothing is delivered from that address, so the receive side needs no fallback.
6. **IOCTLs that change the gate are real.** `CLEAR_MSG_FILTERS` removes the channel's filters, and `CLEAR_RX_BUFFER` drops the responses already received. The service clears filters and installs the same IDs again, which a no-op would turn into `ERR_NOT_UNIQUE`.

## Consequences

- A service that forgets the flow-control filter, or installs it with the wrong partner, receives nothing from the simulated ECU. The end-to-end test in `j2534-0404-service` therefore fails without the unique response ID table that makes the service install the filter.
- Because segmentation is not simulated, separation time, block size and flow-control frames cannot be tested against `sim-vci`. The filter rules are the only part of ISO 15765 flow control it checks.
- The fallback in item 5 accepts filter set-ups for unsimulated addresses that a real device would only reject in the sense that no flow control would ever arrive. A future simulated responder on another ID needs its pair added where the simulator maps request and response IDs.
