# ADR-355: A Link Says Whether It Forwards Every Pending

**Date:** 2026-10-26
**Status:** Accepted
**Affects:** `vci-service-interface` (`service.proto`: `ComLogicalLinkResponse.pending_forwarding`,
`PendingForwarding`), `j2534-0404-service` and `iso22900-service` (`CreateComLogicalLink`),
`agent` (`link.rs` `Link::forwards_pendings`, `host.rs` `pending_visibility`, `comm_log`'s
`link_opened`), `docs/rpc-api-guide.md`, `docs/j2534-0404-architecture.md`,
`crates/iso22900-service/docs/architecture.md`, `crates/agent/docs/ngr-agent.md`; design 8.11
("Header" `link_opened`, `pending`, H4), design principle 6, ADR-346 item 3

## Context

Design 8.11 excludes H4 (the VCI absorbed a response pending) on a link that forwards pendings,
and gives each exchange a `pending_visibility` of `forwarded` or `absorbed_by_vci`; ADR-346
item 3 has the log say whether a link can show its pendings at all. Whether it can depends on
the worker and its library:

- `j2534-0404-service` runs the response-pending handling itself and passes each pending on,
  bound to its primitive;
- under `iso22900-service` the D-PDU library handles a pending itself (`CP_RC78Handling`,
  ISO 22900-2:2022 B.5) and need not deliver it to the client.

The agent did not know which, so `link_opened` wrote `forwards_pendings` `null` and every
exchange was `absorbed_by_vci`: H4 stayed open on every link, and a silence or an answer with
no judged time stayed `undetermined`. Telling the case from the worker's identity is what design
principle 6 forbids; the worker that knows the difference has to say it, as event items say
their clock (ADR-353).

## Decision

1. **The worker says it when the link is created.** `ComLogicalLinkResponse` gains
   `PendingForwarding pending_forwarding = 2`: `PENDING_FORWARDING_ALL` (1) when the worker
   passes on every pending the link sees, `PENDING_FORWARDING_NOT_ALL` (2) when the library may
   handle one without delivering it, and `PENDING_FORWARDING_UNSPECIFIED` (0) when the worker
   does not say. Zero is also what a worker built before the field sends.
2. **Each worker answers for itself.** `j2534-0404-service` answers `ALL` for every link;
   `iso22900-service` answers `NOT_ALL` for every link.
3. **The agent takes it from the field alone.** The link keeps it (`Link::forwards_pendings`:
   `Some(true)`, `Some(false)`, or `None` for an unspecified or unknown value), a reopened link
   takes the value its new `CreateComLogicalLink` answer gives, and `link_opened` records it as
   `forwards_pendings` (`true`, `false`, or `null`).
4. **Only a forwarding link is `forwarded`.** An exchange has `pending_visibility` `forwarded`
   when the worker said `ALL`, and `absorbed_by_vci` otherwise, an unknown value included, since
   a pending the agent cannot rule out stays possible (H4). A link on which a profile declared
   0x21 or 0x23 a pending code stays `absorbed_by_vci` even when its worker forwards 0x78
   (design 8.11 "Link settings for the log", ADR-309 item 5); the agent supports 0x78 alone as a
   pending code today (`send_receive`), so no such link exists yet, and one that comes must keep
   this rule.
5. **The host records the pendings it is shown.** On a forwarding link an empty `pending` list
   reads as "no pending came", so the host keeps every pending bound to the primitive for the
   exchange's list (its VCI time and clock as the worker gave them, ADR-353, and the agent's time
   when the host read it). Without this a pending the worker passed on would be dropped and the
   log would presume it absent, which design 8.11 forbids.

## Consequences

- On a J2534 link, H4 is excluded and a positive response with its send confirmed and its time
  not judged is no longer `undetermined`; a no-response is judged by the readings still open
  (H1 to H3). A D-PDU link keeps H4 open as before.
- The log's classification now differs between the two workers for the same ECU behaviour,
  because the evidence differs, not because the agent tells the workers apart.
- A D-PDU library that does deliver every pending gives no way to say so yet: a VCI profile
  override (design 9.3) is a separate item.
- A worker restart reopens the link, so a different worker binary behind the same job takes
  effect from its own answer.
