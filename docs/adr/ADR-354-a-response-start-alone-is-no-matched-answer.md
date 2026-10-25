# ADR-354: A Response Start Alone Is No Matched Answer

**Date:** 2026-10-25
**Status:** Accepted
**Affects:** design 8.11 ("Answer of the ECU", "Matched answer", "Classification" H2, the
`response_incomplete` row, "Settlement" `late_item`); `agent` (`comm_log/exchange.rs`:
`Evidence::bound_answer`, `LinkState::record`, `open_readings`, `classify`),
`crates/agent/docs/ngr-agent.md`; ADR-346 item 6

## Context

Design 8.11 counts the first frame of a response, which the agent sees as a start-of-message
indication, as an answer of the ECU. ADR-346 item 6 lets an answer in the exchange exclude H2
(the worker dropped a response that did not match, or the address or filter is wrong), and an
answer to a request matched the same way exclude it for the later exchanges.

A first frame shows that the ECU answered, but not that the worker would take the response:

- on J2534, `j2534-0404-service` discards a content frame that matches no registrant (ADR-100),
  while the start-of-message indication of that same response still reaches the agent, unbound
  (SAE J2534-1 v04.04 A.4);
- a D-PDU library drops a response that misses the expected-response array of its primitive.

A definition whose response pattern is wrong is exactly H2. With the first frame counted, its
exchange ended as `response_incomplete` without naming H2, and every later exchange matched the
same way lost H2 from its readings. A pending differs: the worker passed it on as an answer to
this request, so it shows that the worker matches the ECU's responses to it.

## Decision

1. **A response start alone is no matched answer.** An exchange with a start indication and no
   final response or pending (no answer the worker bound to it, `Evidence::bound_answer`) is not
   counted in the matched answers of its key. It still counts in `answered` and
   `answered_since_error`, as evidence that the ECU was answering.
2. **It does not exclude H2.** H2 is excluded by a final response or a pending in the exchange,
   or by a matched answer; a start alone leaves it open. `response_incomplete`'s reason names H2
   beside a frame lost in the network or the VCI's receive path and an ECU that stopped, unless a
   pending in the exchange or a matched answer excludes it, as every row follows from the one
   rule of design 8.11.
3. **The rows of a pending seen need a pending.** A `no_response` exchange with a start alone
   (the agent's own deadline passed while the response may still be arriving) is not classified
   by the rows that rest on a pending seen in the exchange. For every other row the start is
   still an answer of the ECU: a request whose positive response is suppressed does not take the
   rows for no answer of the ECU.
4. **Unchanged.** A start still confirms the send (`sent` `confirmed`, `sent_evidence`
   `answer`, ADR-346 item 4) and still makes an exchange the VCI ended `response_incomplete`.
   `link_history.this_exchange` still says whether the ECU answered in the exchange, a start
   included.

## Consequences

- ADR-346 item 6's exclusion of H2 by an answer in the exchange, and the matched answer it
  counts, no longer take a response start alone.
- A wrong response pattern now shows as H2 on its own exchange and on the later ones matched the
  same way, until a final response or a pending is matched.
- An ECU that only ever starts its responses (every response lost after its first frame) leaves
  its later exchanges without a matched answer: their `no_response` rows stay `undetermined`
  where a matched answer would have made them `vehicle`. This is the price of not hiding H2.
- A late start-of-message indication a settling entry places (design 8.11 "Settlement") is an
  answer under the same rule: it confirms the send and adds no matched answer.
