# ADR-419: The J2534 Worker's Tester-Present Is Serialized With Client Primitives

**Date:** 2026-11-04
**Status:** Accepted
**Affects:** `j2534-0404-service` (`events.rs` `wait_for_bus_clear`, `dispatch_due_tester_present`,
`bind_frame` step 3, `push_open_tp_discard`; `service.rs` `ResidualTesterPresentDiscard`;
`rpc_primitive.rs` `ResolvedTesterPresent`), `docs/j2534-0404-architecture.md` "Tester-Present
Dispatch" and "Response attribution"; ADR-060, ADR-083, ADR-088, ADR-093, ADR-095, ADR-099,
ADR-100, ADR-102, ADR-137, ADR-418

## Context

ADR-418 has the J2534 worker send a job's tester-present and depends on two things the worker did
not do: serialize that send with the client's primitives as ISO 22900-2:2022 8.1.3 Table 1
requires, and report a missing or negative answer. This ADR settles the first (the second is the
error event in item 6).

Before this ADR the tester-present was gated only by the `CP_P3Func`/`CP_P3Phys` gap of ADR-060,
which does not apply after a send that expected a response. It was dispatched from inside a client
primitive's response wait (ADR-083, ADR-095). A client send-receive did not wait for an outstanding
tester-present. The discard window for a tester-present's answer ended at the send plus
`CP_P2Max` and a response-pending chain did not extend it (ADR-088, ADR-099, ADR-137), while
ADR-102 extends a send-receive's window. With a client request and a tester-present both
outstanding, a `7E 00` could therefore be bound to the client's primitive (the two-tier binding of
ADR-100) and the tester-present was left without an answer; a long `7F 3E 78` chain could not be
told from the end of the exchange.

Table 1 (8.1.3), paraphrased: after a physical request that expects a response, a physical
request, and a functional request that expects a response, wait for the previous one to complete
(a positive answer, or a negative one other than response pending), with no further timing; a
functional request that expects no response may go at once, except that after a physical cyclic
request the tester waits for its first answer (the table's note). After a functional request that
expects a response, everything waits for its completion. After a request that expects no response
the timing rows apply (`CP_P3Phys`, `CP_P3Func`), which ADR-060 already implements. 8.1.3's first
paragraph asks the protocol handler to serialize across logical links, and 8.1.4 describes links
sharing a bus.

8.1.3's fourth paragraph also allows a tester-present to go at once, even while a request is
extended by a pending-response chain, so long as that has no undesirable effect. This design holds
the physical tester-present for the whole chain anyway: the server's session timer is stopped while
it has a pending response (ISO 14229-2 (2021) 9.5), so holding cannot lapse the session, and a
tester-present sent into the chain would have its answer compete with the chain's frames and could
not be attributed safely.

## Decision

1. **One rule per shared channel.** `bus_hold(prev, next)` is a pure function over the exchange
   outstanding on the shared channel (the poll task's channel, not one logical link) and the
   send that asks to go:
   - `prev` is functional: every `next` waits.
   - `prev` is physical: a physical `next` waits; a functional `next` that expects an answer
     waits; a functional `next` that expects none goes at once, except while `prev` is an
     IS-CYCLIC client primitive that has had no response yet.
   - A previous exchange that expects no answer is not an outstanding exchange: the `CP_P3Phys`/
     `CP_P3Func` rows of ADR-060 govern what follows it, unchanged, and remain CAN-only.
   Addressing classes come from the resolved `can_functional`; an unresolved one (K-line, J1850
   style protocols) counts as physical, so the hold applies on every protocol family.
2. **One gate.** Every diagnostic request the worker writes on the channel (client send-receive,
   the `CoptStartcomm` request and the arm's first send, the K-line wakeup/StartCommunication
   exchange of `CoptStartcomm` (after the pre-init top-up), the `CoptStopcomm` message, the
   re-arm's send, and every tester-present dispatch) goes through `wait_for_bus_clear`: first the
   hold, then the gap wait of ADR-060. Not gated, since they are not requests that Table 1
   serializes: the SAE J1939 address-claim frames (broadcast claims, J1939-81) and the TP2.0
   connection and keep-alive frames (transport layer, no diagnostic response to wait for).
   A client primitive, `CoptStartcomm`/`CoptStopcomm` and a re-arm wait there, polling RX in the
   usual chunks (cancellable where they were; a hard channel error ends the wait). A tester-present
   dispatch, and any dispatch made from inside an inline wait, never waits there: a held send is
   deferred and stays due, with its `last_fired` unstamped, so mode 0 resumes at the first free
   pass and mode 1 re-idles from the answer's bus activity.
3. **Where "outstanding" lives.** The client side needs no stored state: it is the poll task's own
   inline response wait, which passes its kind (functional, IS-CYCLIC before the first response)
   into `dispatch_due_tester_present` (and the ISO-TP send loops pass a segmented send in flight as
   a physical exchange). Every exit of the wait ends the hold: a count or window end, the first
   positive response of an IS-CYCLIC primitive (it detaches to receive only), a cancel or a hard
   error. The tester-present side is derived from the open-discard entries of ADR-137: an entry made
   by a send that expected an answer holds the channel; one made by a send that expected none is
   the identity window for the send's own echo (ADR-099) and never holds.
4. **The window ends at the final answer.** A holding entry freezes, at the send, the response
   handling of the send (`CP_RCByteOffset`, `CP_RC78Handling`, `CP_P2Star`, the completion
   ceiling, and the message's service identifier as the request SID). For a physical send the
   entry ends at the first content frame that step 3 of ADR-100 binds to it and that matches the
   expected positive answer, or the expected negative answer with a code other than 0x78. A 0x78
   (when `CP_RC78Handling` is set; otherwise it is a final negative) reloads the window to the
   `CP_P2Star` span from that frame, limited by the completion ceiling anchored at the first
   pending answer, as ADR-102 does for a send-receive. For a functional send the reload only
   extends: it never shortens a window another ECU's pending answer has already extended. There is no 0x21/0x23 re-request for a
   tester-present. For a functional send the hold lasts until the window ends (several ECUs may
   answer); any ECU's 0x78 reloads it, and a positive answer only records that one came. A send
   that expected no answer has the fixed window of ADR-099. This replaces the fixed
   send-plus-`CP_P2Max` end of ADR-088 for a send that expects an answer.
5. **Settling happens at step 3 and is written back with the poll pass.** `bind_frame` only
   records what it saw (pending, positive, negative) per entry, in frame order; the end of the
   poll pass applies the records to the live entries under the `logical_links` lock, keyed by the
   entry's signature. A `7F 3E 78` and a `7E 00` in one batch are therefore applied as a reload
   and then a final answer, neither is delivered to the client, and no error event is raised. A
   client primitive cannot register while a physical tester-present is outstanding (item 2), so
   no with-answer registrant competes for the frames at step 2.
6. **The error event.** A holding entry that ends without a positive answer raises
   `PduErrEvtTesterPresentError` on the link, once per such send, and sending continues: a
   physical send's final negative answer, or the end of the window of a physical send (the last
   reload included) or of a functional send that got no positive answer. A per-tick sweep removes
   an expired holding entry (it is not pruned at `until`, so the hold lasts to the sweep) and
   raises the event. The sweep removes an entry only after an exhaustive RX drain at or after its
   `until` on every channel serving the link (the drain watermark of ADR-101 Decision E, as the
   cyclic reap uses it), and the read-side snapshot keeps a holding entry past `until` until then,
   so an answer sent in time but read one poll late still settles the send. It is raised only while the link's tester-present is still armed: after a
   disarm (`CP_TesterPresentHandling` back to 0, `CoptStopcomm`) the entry stays and still holds
   the channel until the answer or the expiry, but a missing answer is not reported.
7. **Entry lifetime.** The dedup signature of an entry adds whether it holds, and a new holding
   send replaces a holding entry of the same signature. Disconnect and Destroy clear the entries,
   which releases the hold; `CoptStopcomm` and a disarm leave them, as ADR-137 says.
8. **Frozen at the send.** A `CoptUpdateparam` of the expected answers, `CP_P2Star` or
   `CP_RCByteOffset` while a send is outstanding does not change how that send settles; the next
   send reads the live values. A `CoptUpdateparam` that changes the tester-present's wire
   behaviour re-arms with a send that waits for the outstanding one (not cancellable, as the
   re-arm's gap wait already was).

## Consequences

- A physical tester-present can be delayed by a long exchange, a response-pending chain included
  (the standard accepts this for physical requests; the session timer of the ECU is stopped while it
  has a pending response, ISO 14229-2 (2021) 9.5). A client request can wait up to the
  tester-present's `CP_P2Max`/`CP_P2Star` span for its answer. Mode 0 becomes "at the first free
  instant of each period" and no longer strictly periodic when the bus is busy.
- ADR-093's send stamp (`no_response_required` is `!expects_response`) is right once the hold
  waits for the answer, which closes the stamp item that ADR-100's out-of-scope Stage 2 listed. The
  other Stage 2 item (parking the inline holds) stays open.
- A disarm or CoptStopcomm after a tester-present send reports no missing answer for that send.
- The scope is the shared channel. Table 1 and 8.1.4 allow parallel requests to different ECUs
  that are not in a functional group; this implementation holds the whole channel and so is
  stricter on a CAN bus with several ECUs. K-line and J1850 style protocols have no resolved
  functional class here, so a functional request there counts as physical and its hold is longer
  than the table requires. Both are recorded as follow-ups in the worker-crates backlog.
- The agent's keep-alive (ADR-418) can rely on the worker for items 5 and 6 of that ADR. The
  D-PDU worker is not changed; whether a vendor library does the same is left to the hardware
  checks (ADR-418 item 7).
