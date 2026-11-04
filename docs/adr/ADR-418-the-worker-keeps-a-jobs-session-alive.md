# ADR-418: The Worker Keeps a Job's Session Alive, the Agent Decides When

**Date:** 2026-11-04
**Status:** Accepted
**Affects:** `agent` (`link.rs` `LinkConfig`, link open, `host.rs`, the communication log's
`Origin::KeepAlive`), `j2534-0404-service` (tester-present dispatch, `handle_start_comm`),
`iso22900-service` (passes through to the vendor library), `ngr-hw-checks`; design 8.2.5, 8.10,
8.11 and 13.5; `docs/j2534-0404-architecture.md` "Tester-Present Dispatch"; ADR-417 items 1, 4, 5,
6, 8 and 9; ADR-325 item 11; ADR-264 item 4; ADR-088, ADR-093, ADR-099, ADR-137

## Context

ADR-417 decided that a job's non-default session on its own link is kept alive, and that the
agent sends the `3E 00` itself from a task of its own, sharing the link with the VM's requests
one exchange at a time. The maintainer revised the mechanism the same day (2026-11-04): the
worker sends the tester-present on the job's link, using the cyclic tester-present ComParams of
ISO 22900-2:2022 Annex B (`CP_TesterPresentHandling`, `CP_TesterPresentMessage`,
`CP_TesterPresentReqRsp`, `CP_TesterPresentExpPosResp`, `CP_TesterPresentExpNegResp`,
`CP_TesterPresentAddrMode`, `CP_TesterPresentSendType`, `CP_TesterPresentTime`), and the agent
only decides when, from its record of the session. The reasons are that the worker sees every
primitive on the link and so can serialize its sends with them, which a second sender on the
agent's side could only approximate, and that the ComParams are the API's own way to ask for a
cyclic tester-present on both worker kinds.

What the J2534 worker does today, which this decision depends on (`crates/j2534-0404-service/src/service/events.rs`):

- It does not implement ISO 22900-2:2022 8.1.3 Table 1 in either direction. Its tester-present is
  gated only by the P3 gap, and the gap is skipped when the previous send expected a response
  (`wait_for_p3_gap`); the dispatch also runs from inside a client's response wait; and a client
  send-receive does not wait for an outstanding tester-present.
- Its discard window for a tester-present's answer is fixed at the send plus `CP_P2Max` and is not
  extended by a response-pending chain (ADR-088, ADR-099, ADR-137). With both a client request and
  a tester-present outstanding, a `7E 00` can be bound to the client's ComPrimitive (the two-tier
  binding of ADR-100, which superseded part of ADR-088), and the tester-present then has no answer.
- `PduErrEvtTesterPresentError` is raised only when the write fails, never for a missing or a
  negative answer.
- `CoptStartcomm` accepts the optional request (ADR-111, ISO 22900-2:2022 8.2.6.3.2): on a CAN or
  J1850 style link `handle_start_comm` sends it and, when its `NumReceiveCycles` is not zero, runs
  the same bounded receive phase a send-receive runs and delivers the response as the primitive's
  result. A K-line link runs its initialisation sequence instead and carries no request.
  After the request it arms the tester-present (ADR-084), whose first send follows at once, and the
  link moves to COMM_STARTED.

ISO 22900-2:2022 8.2.6.3.2 runs the cyclic tester-present only while the link is in COMM_STARTED,
which only a successful `CoptStartcomm` reaches (the `CP_TesterPresentHandling` description says
the same); 8.2.6.3.3 makes `CoptStopcomm` cancel the link's queued and running primitives. The
ISO 22900-2:2022 event error table defines `PduErrEvtTesterPresentError` for a tester-present
that gets no answer or a negative one, and the module keeps sending afterwards.

## Decision

Design principle 6 holds: the agent does not branch on the worker kind. Both workers get the same
ComParams and the same primitives; each absorbs its API's differences. ADR-417 items 2, 3 and 7
stand. This ADR replaces ADR-417 item 1 (who sends), item 4 (the sharing), item 5 (the window),
item 6's trigger and item 8's last sentence, and amends item 9.

1. **The worker sends, the agent decides when.** On a link of the job that is not an access
   target's, the tester-present is the worker's cyclic one, on the job's own link. The agent sets
   no timer and sends no `3E 00` of its own there. The scope of ADR-417 item 1 (each link of the
   job that is not an access target's, one record per link) is unchanged; an access target keeps
   ADR-325 item 11 and its loss rule (item 12), whose probe may be a session read rather than a
   TesterPresent.
2. **ComParams set at link open.** When the keep-alive is on, the agent sets on the link:
   message `3E 00`; `CP_TesterPresentReqRsp` 1 (a response is expected); expected positive
   response `7E 00`; expected negative response `7F 3E`; `CP_TesterPresentAddrMode` 0 (physical);
   `CP_TesterPresentSendType` 0 (send every period, whatever else the bus carries);
   `CP_TesterPresentTime`
   the `LinkConfig` period; and `CP_TesterPresentHandling` 1. `LinkConfig` carries the on/off
   setting and the period as ADR-417 item 2 says (on, 2 s by default, a period that is not
   positive or is below 200 ms refused). With the keep-alive off the agent sets nothing and the
   behaviour of ADR-264 item 4 holds for the link.
   The period is fixed rather than counted from the last exchange on the link, which replaces
   that rule of ADR-417 item 4 (the maintainer's decision, 2026-11-04): send type 1 waits for
   the bus to be idle for the period, and the bus is shared, so another ECU's or another
   logical link's traffic on it can hold this link's tester-present off while its own link is
   idle, and the session lapses (the J2534 worker's idle clock is per shared channel, see
   `docs/j2534-0404-architecture.md` "Tester-Present Dispatch"). A tester-present sent while
   the link's own traffic would already have kept the session costs one frame per period.
3. **Tester-present runs only in COMM_STARTED, so the first session control starts it.** A link
   opens ONLINE and stays so until the first non-default session. The agent sends the link's
   first DiagnosticSessionControl request into a non-default session as the request of a
   `CoptStartcomm` primitive (a StartComm with a request behaves like a send-receive, 8.2.6.3.2),
   which puts the link into COMM_STARTED and starts the tester-present. Every earlier request,
   and every request on a link whose keep-alive is off, goes as an ordinary send-receive as
   today. If the answer to that request leaves no non-default session in the record (item 4),
   the agent switches the handling off as item 4 says for a return to the default session.
4. **On and off by UpdateParam.** The agent keeps the per-link session record of ADR-417 item 3
   (unchanged). When a positive answer enters a non-default session on a link that is already in
   COMM_STARTED it switches `CP_TesterPresentHandling` to 1 by `CoptUpdateparam`; when a
   default-session answer or an accepted ECUReset ends the session it switches it to 0 the same
   way. It never uses `CoptStopcomm`, which would cancel the primitives queued or running on the
   link (8.2.6.3.3). The record starts as the default session whenever a link opens, so a
   restart's teardown (ADR-264) is not disturbed.
5. **Serialization is the worker's.** The worker sends its tester-present and the client's
   primitives one at a time as ISO 22900-2:2022 8.1.3 Table 1 requires: a physical request that
   waits for a response waits for the previous physical request's final response, a
   response-pending chain included. A tester-present answer is therefore attributable, and the
   window in which it is expected ends at its final response under the protocol's timing, not at a
   fixed send plus `CP_P2Max`. This replaces ADR-417 items 4 and 5: the agent keeps no
   exchange-at-a-time lock for the keep-alive, uses no `LinkHeld`-style refusal for it, and
   applies no send-receive ceiling to it.
6. **The error event is the loss.** The worker reports a missing or a negative answer to its
   tester-present as `PduErrEvtTesterPresentError` and keeps sending. The agent counts the first
   such event on the link as the session lost, and ADR-417 item 6 applies unchanged: the loss is
   latched (a later good answer does not restore it), reported at the VM's next host call
   (a request not yet sent is refused as never sent, a `Wait` ends at its next poll, a server round
   trip finishes first), the run ends in the host error without ADR-392's in-run repeat and without
   ADR-393's continuation, and a loss found after the VM's last call is not reported. A
   tester-present error event is the link error of the step in progress only in that sense; it is
   not an ordinary link error that fails a primitive in flight.
7. **The D-PDU worker leaves it to the library.** `iso22900-service` passes the ComParams,
   `CoptStartcomm` and `CoptUpdateparam` through. Whether a vendor library reports a negative or
   missing answer (the standard's wording is a "should") and honours Table 1 is not assumed; the
   real-hardware checks decide it (design 13.5, ADR-275).
8. **The workers' cyclic tester-present is used.** ADR-417 item 8's statement that it stays off is
   reversed for a job's own link. The VM still may not send TesterPresent, and a read-only job
   still never has a non-default session on its own link (ADR-417 item 8, unchanged).
9. **The log shows the switching.** The agent writes an entry with `origin: keep_alive` when it
   switches the tester-present on or off (the `CoptStartcomm` that starts it is the VM's own
   session control and logs as that request), and one entry for each tester-present error event.
   There is no entry per send, since the agent does not see each one. Design 8.11 gives the shape.

## Consequences

- The agent loses its keep-alive task, the exchange-at-a-time sharing and the `LinkHeld` interplay
  ADR-417 needed; it gains the open-time ComParams, the StartComm choice for the first non-default
  session control, the on/off updates and the event mapping.
- ADR-417's one entry every 2 s no longer exists. The log can no longer show a tester-present that
  was sent but whose answer was never seen, beyond the error event; the sends are visible only on
  a bus trace.
- The loss detection is the worker's: the window and a long response-pending chain are handled by
  the worker's own response handling, so a long exchange no longer needs special treatment in the
  agent. The residual of ADR-417's Consequences stands: a session the ECU dropped by itself while
  tester-present was still answered is not detected.
- The J2534 worker must be fixed first, because the design depends on what it does not do today:
  it must serialize its tester-present with client primitives per Table 1 and end its discard window
  at the final response, and it must raise the error event for a missing or negative answer. The
  `CoptStartcomm` request needs no change on a CAN link.
- The tester-present's first send follows the `CoptStartcomm` at once (ADR-084); under item 5 it
  waits for the session control's final response, so it is not mistaken for that answer.
- With a D-PDU library the behaviour is the vendor's until a real-hardware check shows it; a
  library that reports no error leaves a lost session undetected there, as ADR-417's residual does.
- Design 8.2.5, 8.10, 8.11 and 13.5 and ADR-417's status are updated with this ADR.
