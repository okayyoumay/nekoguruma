# ADR-421: A Functional Request's TxDone Reaches the Link That Sent It

**Date:** 2026-11-05
**Status:** Accepted
**Affects:** `j2534-0404-service` (`LogicalLinkState::open_func_tx`, `handle_send_recv`,
`CllRxEntry::func_tx_can_id`, `build_cll_rx_entries`, `route_frame_with_role`,
`DisconnectComLogicalLink`), `docs/j2534-0404-architecture.md`; design 8.11 "Link settings for the
log"; ADR-099, ADR-100, ADR-352 item 3, ADR-400, ADR-419

## Context

ADR-352 routes the TxDone indication of a request to the link whose unique response table names
the request's physical ID, and leaves a functional request out (item 3): `CP_CanFuncReqId` is a
channel ComParam, not a table key, so the TxDone of a functional request reached only links
without a table. The agent's link always has a table. An identification run's presence request is
functional (design 8.8.2), so its log entry had no transmit time on the J2534 worker, and its
answers stayed `undetermined` with no response time even when the run was given a timing margin
(`no_transmit_time`, ADR-400).

The agent already turns a TxDone that no registrant took (an unbound TX-side item, which ADR-100
step 1 never binds) into the exchange's transmit time, by the CAN ID in its header: the presence
exchange's target is the functional ID. Only the worker's routing was missing.

## Decision

1. **A per-send, per-link marker.** `LogicalLinkState` holds `open_func_tx`, the CAN ID of the
   functional request this link has sent and is waiting on: the first four bytes of the sent
   data, the prefix ADR-099's `tx_can_id` reads. `handle_send_recv` sets it after the write of a
   functional send-receive succeeds (`can_functional == Some(true)`, a nonzero
   `NumReceiveCycles`), and clears it at every exit of that cycle's
   `wait_for_expected_response` and on `DisconnectComLogicalLink`. It is a write under
   `logical_links`, as ADR-419's writebacks are, and set before the wait's first read.
   `build_cll_rx_entries` copies it into the pass's `CllRxEntry`.
2. **Routing.** After the table scan finds no entry, `route_frame_with_role` routes a TX-side
   frame whose CAN ID equals the entry's marker to the link, with unique response identifier 0.
   A table row that matches keeps its own identifier. `bind_frame` leaves the frame unbound, as
   any TX-side frame, so it reaches the log as a plain indication and the agent attributes it
   to the exchange as before. This is how the presence primitive's exchange "binds" the
   indication: by the agent's existing attribution, not by a registrant.
3. **Exclusions.** Tester-present is not marked (ADR-099 keeps its own indication discarded),
   nor are StartComm and StopComm sends, nor a functional send with `NumReceiveCycles` 0, which
   has no receive phase (ADR-352 item 3 stands for that send). Software ISO 15765-2 and raw CAN
   give no TxDone (ADR-352 item 5).
4. **No `CP_CanFuncReqFormat` or width gate.** ADR-352 item 2 withholds the request-ID key from
   an extended-addressed entry because several entries behind one CAN ID differ only by the
   address extension. The marker belongs to one send of one link, so no other entry competes
   for the frame, and no gate is needed.
5. **The agent is unchanged.** Its attribution by the header CAN ID, and the decision that the
   run is given margins as a job is, make the answers' response times measured and judged.

## Consequences

- A presence answer is measured from the request's TxDone on the device clock and judged against
  the run's margin: within `CP_P2Max` it is `none`, a chain of pendings that ends in the final
  answer is judged by the P2* rules, and a repeated answer is judged from the previous one's
  end (ADR-400 item 1). The presence entry stays `undetermined`, because its window ends on the
  worker's clock (ADR-400 item 2).
- A TxDone read after the entry was written goes to the entry's late item, which restates the
  sent evidence; the answers keep their timing.
- A TxDone that is lost leaves nothing to attribute: the marker ends with the wait, and the
  entry has no transmit time, as before.
- Residuals. While the link has an open tester-present window on the same functional ID (arm c
  of ADR-099), that discard takes the client's TxDone; the agent's tester-present is physical
  and its presence link arms none. A sibling's tester-present TxDone on the functional ID that
  is enqueued after the P3Func gap but read inside the marker is taken for the request's, so
  the transmit time can be early by up to that gap.
- Another link with a table and the same functional ID that sends later does not overlap: ADR-419
  serializes the sends, and the marker of the later send takes the later TxDone.
- ADR-352 item 3 is superseded for a functional send with a receive phase.
