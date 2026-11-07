# ADR-430: The D-PDU API's Tester-Present Indication Carries No Header, and the Worker Logs One That Does

**Date:** 2026-11-07
**Status:** Accepted
**Affects:** `crates/iso22900-service` (`events.rs` `note_unbound_header`, `rpc.rs` `CopTagMap`),
`crates/iso22900-mock` (`send_tester_present`), `crates/iso22900-service/tests/ngr_agent_dpdu_unbound_indications.rs`,
`crates/iso22900-service/tests/t2_indication_unbound.rs`, `crates/iso22900-service/docs/implementation-notes.md`,
`crates/iso22900-mock/docs/testing-guide.md`, `crates/agent/docs/ngr-agent.md`, design 8.11
`sent_evidence`; ADR-099, ADR-356, ADR-423, ADR-427, ADR-428

## Context

Design 8.11 lets the agent take an unbound transmit indication that names the exchange's request
identifier as the exchange's send evidence only if the API sends no tester present of its own on
that identifier. The agent relies on this (`crates/agent/docs/ngr-agent.md`, the link's
"design 8.11's other conditions"): a tester present sent just before a request would otherwise
leave an indication that stands in for the request's. On the J2534 worker the condition is made
true by discarding the tester present's own indications (ADR-099). The D-PDU worker forwards
every item the library queues.

With the mock's unbound control (ADR-427) the mock's own tester present (ADR-423) raised its
transmit indication unbound with the request identifier in a header. The agent took it as a VIN
read's transmit done, some 46 ms before the read was sent.

What ISO 22900-2:2022 says decides whether a library does the same. An item that does not come
from a ComPrimitive has `hCop` equal to PDU_HANDLE_UNDEF (10.1.4.11). Extra info header bytes
come only on the items of a ComPrimitive whose TxFlag sets ENABLE_EXTRA_INFO (10.1.4.11.4,
D.2.1). So the API's tester-present transmit indication, if the library raises it at all (B.5.1,
`CP_TransmitIndEnable`, does not say whether an API-generated message counts), has no
ComPrimitive and no header. A request's own transmit indication is bound to its ComPrimitive,
and the agent prefers a bound one; its unbound path needs the identifier from a header.

Table 1 of 8.1.3 makes a physical request wait for the final answer of the previous physical one
(a response pending excluded). Its paragraph on sending a request at once also lets a library
send a tester present during a response pending when that has no undesirable effect, so a D-PDU
library may put one between a request and its response.

## Decision

1. **On D-PDU the condition holds by the standard's structure.** A conforming library gives the
   API's tester-present indication no header, so the agent's unbound path never matches it.
   The worker filters nothing and the agent is unchanged.
2. **The worker logs a sign of a non-conforming library.** When it converts an event item
   (`to_proto_event_item`, for the stream and `GetEventItem`) that is a result item with `hCop`
   undefined and a non-empty extra info header, it logs a warning naming the link, the kind of
   item (transmit indication, start of message, other, from the RxFlag), the header's
   identifier when it has four bytes (else its length), and that a conforming library gives such
   an item no header. It warns on the first such item per link and kind, counts the rest, and
   logs one summary line with the count (when above one) when the link is destroyed or the
   module disconnected. The counts live in `CopTagMap`, under the lock of the tag bookkeeping,
   so no lock order is added. Items are forwarded unchanged. A bound item with a header
   (ADR-428's result headers) is not warned about.
3. **The mock's tester present raises its transmit indication with no header.** When the link's
   active `CP_TransmitIndEnable` is on and the active tester-present address mode is 0, the
   mock queues an unbound indication with `pExtraInfo` null, in every binding mode. The
   request's own indications keep ADR-427's binding control.
4. **The agent is unchanged.**

## Consequences

- On a non-conforming library that raises the tester present's indication unbound with a header
  and gives the request's own indication unbound too, the agent can take the tester present's
  as the transmit done. The agent takes the first match in read order (`comm_log.rs`), so an
  earlier-queued tester present's item wins, which gives an earlier transmit done and a longer
  response time; it never shortens `response_ms`. This is the residual ADR-356 accepts, and the worker's
  warning shows it. Reconsider if a hardware run (design 13.5) shows such items.
- The agent's end-to-end tests cover both bindings: the VIN read after a session control keeps
  its own transmit done, whether the mock queues the request's indications unbound or bound.
- ADR-427's statement that a library following Table 1 lands the tester present before a request
  or after its response holds only for such a library; one using the paragraph above may send it
  during a response pending. That does not matter here, since its item has no header either way.
  The J2534 worker holds its tester present for the whole pending chain (ADR-419).
- The counts are summed up only after a successful native destroy or disconnect, never on the
  worker's exit, and a failed destroy leaves them, so a reused link handle gets no first warning.
  This is the pattern of the COP tags (ADR-205).

## Alternatives considered

- **A worker-side discard (rejected).** The worker would keep a per-link "tester present runs"
  state learned in event order: the request identifier from `SetUniqueRespIdTable`'s first
  `CP_CanPhysReqId`; started at a STARTCOMM FINISHED confirmed by `PDUGetStatus`; the handling
  and address mode read back by `PDUGetComParam` at that point and at an UPDATEPARAM's
  EXECUTING or FINISHED; cleared at STOPCOMM, Disconnect or Destroy. While it runs, an unbound
  TX_INDICATION whose four-byte header equals the request identifier would be dropped.
  Rejected: it changes nothing on a conforming library; on a non-conforming one it also drops
  the client's own unbound indication; it adds state and library calls; and it needs hardware
  checks to be trusted. A later switch can start from this shape.
