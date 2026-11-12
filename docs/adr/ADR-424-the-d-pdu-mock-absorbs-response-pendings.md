# ADR-424: The D-PDU Mock Absorbs Response Pendings

**Date:** 2026-11-05
**Status:** Accepted
**Affects:** `crates/iso22900-mock` (`PDUSetComParam`, `PDUGetComParam`, `PDUConnect`,
`PDUStartComPrimitive`, `receive_events`), `crates/iso22900-mock/docs/testing-guide.md`,
`crates/iso22900-service/tests/agent_suppressed_send_receive.rs`; ADR-102, ADR-299, ADR-423

## Context

ADR-299 had the mock pass each response pending message (negative response code 0x78) to the
client as a result item, because the mock kept no ComParams. A D-PDU API library that keeps to
ISO 22900-2:2022 does not: with `CP_RC78Handling` on it handles the pending itself (8.4.15.6,
Annex B.5.1, Table 9, Figures I.4 and I.5), so the client sees the response, or a receive
timeout. The agent sets these ComParams on a UDS link, and the worker's pending-visibility
rules (ADR-355) assume a library that absorbs. The mock could not stand in for one, so the
silence that follows an absorbed pending, and the log entry that classifies it, could not be
tested end to end.

## Decision

1. **Four ComParams, two buffers.** `CP_RC78Handling`, `CP_P2Star`, `CP_RC78CompletionTimeout`
   and `CP_RCByteOffset` have a working buffer, written by `PDUSetComParam`, and an active
   buffer. `PDUConnect` and the start of a `PDU_COPT_UPDATEPARAM` copy working to active
   (8.4.15.6), on every UPDATEPARAM whether or not an ECU is attached. A ComPrimitive takes
   the active buffer as it is when `PDUStartComPrimitive` is called. `PDUGetComParam` reads the
   working buffer, in the class B.5.1 gives (timing for P2*, error handling for the others).
   The `TempParamUpdate` flag (one ComPrimitive on the working buffer) is not supported.
2. **Handling is off at the start.** B.5.1's default for the UDS protocols is 2, which a
   library has. The mock starts at 0, so ADR-299's behaviour (items 4 and 5: pendings as result
   items, the timeout right after the last message) stays until a client sets the handling and
   activates it. P2* starts at 5 050 000 us and the completion timeout at 25 000 000 us, the
   B.5.1 values for the UDS protocols. A wrong data type, a handling above 2, a byte offset of
   0 or a P2* above B.5.1's range (655 350 000 us) is refused; B.5.1 gives the completion
   timeout's maximum as protocol specific, so any value is kept.
3. **Absorption.** With the handling on, a message that is a response pending message (a 0x7F
   byte first, the request's service id second, 0x78 at the response code offset) is not a
   result item. Each one reloads the receive timer to P2*. Handling 1 also ends the chain at the
   completion timeout counted from the first pending (Figure I.4); handling 2 has no limit
   (Figure I.5). When a message is due after the timer ran out, or the ECU sends nothing more,
   the receive timeout is the item, at the instant the timer ran out, and the later message is
   not delivered. A final message that is itself `7F SID 78` with no pending list (the
   simulated ECU's negative-response fault with code 0x78) is absorbed the same way, so the
   timeout follows after P2* if nothing comes after it.
4. **Response code offset.** The mock counts the offset from 0 with the 0x7F at index 0, as the
   agent sets it (2 for `7F SID code`); 0 is refused, as B.5.1 lists it as invalid, and
   0xFFFFFFFF is the last byte.
5. **Completion timeout 0 is no ceiling.** B.5.1 does not say what 0 means. The mock reads it as
   the repository does elsewhere (ADR-102, the J2534 worker, the agent's `link.rs`): handling 1
   with a completion timeout of 0 then behaves as handling 2 for the limit.
6. **Tester-present unchanged.** The answer to a tester-present that is a pending is judged as
   ADR-423 has it; the response pending ComParams do not touch it.

## Consequences

- ADR-299 Decision items 2, 4 and 5 hold as they were only while the handling is off.
- A client that sets the handling on and activates it gets a mock that behaves as a library:
  `agent_suppressed_send_receive` shows the silence after a pending lasting P2* and the log
  entry `undetermined` with no pending listed.
- `PDUCancelComPrimitive` stays a no-op, so a cancelled primitive still runs its schedule.
- The mock's offset reading of B.5.1's byte offset is the agent's; a library reading the
  standard's wording differently is not checked here.
