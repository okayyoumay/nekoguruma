# ADR-352: TX-Side Frames Route by the Unique Response Entry's Physical Request ID

**Date:** 2026-10-24
**Status:** Accepted
**Affects:** `j2534-0404-service` (`events_rx_routing.rs`: `UniqueRespIdKey`, `route_frame`,
`build_cll_rx_entries`), `sim-vci` (TxDone indications), `docs/j2534-0404-architecture.md`,
`docs/rpc-api-guide.md`; design 8.11 "Link settings for the log", ADR-098, ADR-100, ADR-151,
ADR-346 item 4

## Context

On an ISO 15765 channel the J2534 device queues a TxDone indication when a message has gone out,
and a loopback echo when loopback is on. Both carry the CAN ID the tester sent the message to,
not the ECU's response ID (SAE J2534-1 v04.04 clauses 8.6 and 8.7.1). ADR-098 forwards them as
`ResultData`, and ADR-151 lets `CP_TransmitIndEnable` decide per link whether a TxDone reaches
the client.

Before any of that, the worker routes each frame to the links on the channel (ADR-100): a link
whose unique response ID table is empty takes every frame, and a link with a table takes a frame
only when its CAN ID matches one of the table's response IDs (`CP_CanRespUSDTId`,
`CP_CanRespUUDTId`) or another receive-side key. A TxDone or a loopback echo never carries a
response ID, so on a CAN link with a table it matched nothing and was dropped. The agent's link
always has a table, so the transmit indication it turns on for the communication log (design
8.11, ADR-346 item 4) never reached it. TP2.0 already routes its own echo by the connection's
transmit ID (`tp20_tx_id`); CAN had no counterpart.

## Decision

1. **A TX-side frame on CAN routes by the table entry's `CP_CanPhysReqId`.** A frame is TX-side
   when its `RxStatus` has `TX_MSG_TYPE` or `TX_INDICATION` set (ADR-099's definition). Such a
   frame whose CAN ID equals an entry's `CP_CanPhysReqId` is routed to that entry's link, with
   that entry's unique response identifier. ADR-151's per-link gate and ADR-098's delivery rules
   then apply as before.
2. **Only a TX-side frame matches the request ID.** A received frame on a request ID is another
   tester's request, never this link's response, so it still routes only by the response keys.
   A TX-side frame whose CAN ID is no entry's request ID is not routed to that link.
3. **`CP_CanFuncReqId` is not a key.** It is a channel ComParam, not a unique response table
   ComParam, so a TxDone of a functional request still reaches only links without a table.
4. **`sim-vci` gives a TxDone indication for each message it sends**, ahead of any response to
   it, with the CAN ID as its data, and no filter applies to it (clause 8.6). A send that fails
   gives none. This lets tests show a transmit indication end to end.

## Consequences

- The agent's link receives the TxDone of each request it sends. The agent logs it
  `unattached` until the log attributes unbound items to exchanges (design 8.11).
- Every link whose table names the same physical request ID gets the frame, so two links that
  share a request ID both see each other's TxDone. Design 8.11's attribution conditions already
  allow for an indication more than one exchange could own.
- A client that reads `PassThruReadMsgs` from `sim-vci` directly now reads a TxDone before each
  response, as on a real ISO 15765 device; the tests that do so read it first.
- A TxDone of a functional request does not reach a link with a table. No caller sends
  functional requests on such a link yet.
