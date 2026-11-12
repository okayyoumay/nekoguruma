# ADR-420: The Agent's Keep-Alive Mechanics

**Date:** 2026-11-05
**Status:** Accepted
**Affects:** `agent` (`link.rs` `LinkConfig` and the link open, `keep_alive.rs`, `host.rs`
`WorkerHost`, `client.rs` `WorkerApi`, the communication log's `keep_alive` event and
`link_opened`), `diag-ir` (`link::SET_BY_LINK`), `ngr-hw-checks` (`GuardedClient`); design 8.2.5 and
8.11; ADR-264 item 4, ADR-324 item 3, ADR-389, ADR-392, ADR-393, ADR-408, ADR-417 items 2, 3, 6 and
7, ADR-418, ADR-419

## Context

ADR-418 has the worker send a job's tester-present and the agent decide when, and leaves the
agent's mechanics open: which request goes as the `CoptStartcomm`, how the agent knows the link
reached COMM_STARTED when that request got no positive answer, who writes the session record, how
the handling is switched, what a failed switch means, and what the log shows. The maintainer
decided three points on 2026-11-04:

- (a) a tester-present error event is the session lost only while the session record holds a
  non-default session; with the record in the default session it is logged only;
- (b) after a `CoptStartcomm` whose request got no positive answer, the agent asks the link's status
  rather than assuming either way;
- (c) a failed switch-on of the handling is reported like a loss; a failed switch-off is logged only.

ISO 22900-2:2022 8.2.6.3.2 starts the cyclic tester-present only in COMM_STARTED, and the J2534
worker enters that state whatever answer the `CoptStartcomm`'s request got (`events.rs`
`handle_start_comm`) and refuses a second `CoptStartcomm` on a started link. A StartComm carrying a
request is judged as a send-receive is (8.2.6.3.2; ADR-418 item 3). `GetStatus` of a logical link is
supported by both workers.

## Decision

Design principle 6 holds: the agent sets the same ComParams and sends the same primitives to both
worker kinds and does not branch on which it is.

1. **One record per link, one writer per value.** The host holds a `KeepAlive` record
   (`keep_alive.rs`) of four values, each written in one place, and starts it over whenever a link
   opens (`WorkerHost::new`) or is opened again (`WorkerHost::reopen`, ADR-393):

   | Value | Starts as | Written by |
   |---|---|---|
   | session (sub-function of the ECU's session) | default | `after_exchange` only (item 4) |
   | link state (ONLINE, COMM_STARTED, unknown) | ONLINE | the StartComm's end (item 3), then its status |
   | handling believed on | on when the keep-alive is on | the link open, and a switch that succeeded (item 5) |
   | a switch-on failed | no | the failed switch (item 6) |

   `LinkConfig.keep_alive` is `Option<Duration>`: `Some` (2 s by default) from `LinkConfig::iso15765`
   and `declared`, `None` for off. `validate` refuses a period below 200 ms, which includes zero
   (ADR-417 item 2), and one that does not fit the worker's microsecond ComParam. A functional link
   has none: its requests are presence requests and the tester-present ComParams address one ECU.

2. **ComParams at open.** With the keep-alive on, the link open sets, after its own and the
   declared ComParams, the tester-present ComParams of ADR-418 item 2, under the class
   `PDU_PC_TESTER_PRESENT` (the D-PDU API takes the concrete class): message `3E 00`, expected
   positive response `7E 00` and expected negative response `7F 3E` as byte fields, the others as
   numbers (`ReqRsp` 1, `AddrMode` 0, `SendType` 0, `Time` the period in microseconds), and the
   `Handling` 1 last, so that it switches on only a complete set. `CP_RC78Handling` and
   `CP_RCByteOffset` stay among the link's own ComParams, where ADR-419 depends on them. A library
   that does not know one of these ComParams or refuses it is treated as for an indication
   ComParam: the link opens without the keep-alive (the handling is set to 0 on a best-effort
   basis), `link_opened` lists them as attempted with `accepted: false` (the handling with the 0 set afterwards, the value in force), and the host
   behaves as if the keep-alive were off for that link. The names are in `diag_ir::link::SET_BY_LINK`,
   so a document cannot override them (ADR-324 item 3); ADR-406 item 7's test keeps the two lists
   equal. `link_opened` names the byte fields by a hexadecimal `value_hex` in place of `value`;
   `CP_TesterPresentHandling` is listed with the library's default only when the keep-alive is off.

3. **The StartComm, and the link's status.** A request goes as a `CoptStartcomm` when the
   keep-alive is on, the link is ONLINE, and the request is a DiagnosticSessionControl whose
   sub-function, with the suppress-positive-response bit masked, is not the default session. Every
   other request goes as a `CoptSendrecv`, and the host never sends a `CoptStopcomm` (it would
   cancel the link's queued and running primitives, 8.2.6.3.3). Everything else about the request
   is the send-receive's: its tag, expected responses, one receive cycle, deadline, stray
   handling and progress, so its answer is judged as a send-receive's (positive, negative, no
   response, a 0x78 chain, the silence of a suppressed positive response by ADR-408), and
   ADR-389's `send_state` and ADR-392's repeat rules are unchanged (the refusal check keys on the
   `StartComPrimitive` RPC, the same for both). The exchange entry is the same, with
   `detail.cop_type` `startcomm`.

   The link state after it: a positive answer, or the silence of a suppressed positive response,
   makes it COMM_STARTED; any other end that reached the worker makes it unknown (decision b), and
   the host asks `GetStatus` of the logical link: COMM_STARTED makes it started, ONLINE makes it
   ONLINE again (the next such request is a StartComm again), and an answer that fails or names
   another state leaves it unknown, asked again after the next exchange. It is not asked while a primitive the host stopped waiting for is unsettled (a StartComm the agent's deadline ended is a stray the next request cancels): the status of a link that primitive may still change says nothing, so the question waits for the exchange that settles it. A refusal before the
   start (`SendState::NotSent`) leaves the state as it was.

4. **The session record.** `after_exchange(request, result)`, at the end of `exchange`, is the
   only writer. Every request passes through `exchange`, and so through it (send-receive,
   generic send-receive, flash transfer, the runner's and the restart's own reads; the presence
   request does too, but its functional link has the keep-alive off, so nothing changes). A DiagnosticSessionControl
   answered positively, or by the silence of a suppressed positive response, records the
   sub-function (suppress bit masked; 1 is the default); an ECUReset of a sub-function that resets the ECU (01 to 03, ISO 14229-1:2026 9.3) answered
   the same way records the default session; the rapid-power-shutdown sub-functions (04, 05) do
   not reset it and change nothing, a tester-present left running in the default session being
   harmless; a negative answer, a failure or a positive answer of another service
   changes nothing (ADR-417 item 3, ADR-408). A restart's teardown on a fresh link therefore
   leaves the record in the default session and switches nothing.

5. **Switching the handling.** `switch_handling(on)` sets `CP_TesterPresentHandling` to 1 or 0 on the
   link and applies it with a `CoptUpdateparam` primitive that carries no data, and does not wait
   for its finished status: the link runs its primitives in order (ISO 22900-2:2022 8.4.17), so it
   is applied before the next one starts. It is called only by `after_exchange`, and only on a
   started link: when the record goes from the default to a non-default session and the handling is
   off, it switches on; when the record returns to the default session and the handling is on, it
   switches off; and when the link has just become started (by the StartComm or its status) with the
   record in the default session, it switches off, since the StartComm left the handling on
   (ADR-418 item 3). A StartComm answered positively into a non-default session switches nothing,
   the handling being on from the open. The handling changes only when a switch succeeded, or when a switch-off failed in a way
   that leaves it unknown whether the update reached the worker (its start timed out or failed
   without a refusal): the handling is then believed off, since a later switch-on is harmless if
   it was on, and a handling believed on would never be switched on again. A refusal that names
   its reason proves the update did not start, and the handling stays believed on.

   The status can also come late: when it says ONLINE for a link whose record already holds a
   non-default session (the start got no answer, its status could not be read, and a session
   control went as a send-receive meanwhile), no tester-present runs for the session and no switch
   can start one. The agent treats that as the failure of a switch-on (item 6): the next request
   is refused and a failed `keep_alive` entry is written. Sending the next session control as a
   StartComm instead would depend on the job sending one. When it says COMM_STARTED with the
   handling still on, the tester-present the start left on keeps the session, and the log shows
   the `on` entry with `means` `start_comm`.

6. **A failed switch.** A failed switch-on leaves the session unkept, which is the loss ADR-418
   item 6 reports; a failed switch-off is only logged, as the handling stays on and the worker
   keeps sending a harmless tester-present. The request that triggered a failed switch-on returns
   its own result unchanged. In the first part, with no latch, the host keeps the failure and the
   next request of the host fails with `HostError::KeepAliveFailed` before anything is sent (not
   sent, no in-run repeat; the entry has its own refusal cause, `keep_alive_failed`). The failed
   switch-on and the latch of item 8 share one report point: the check at the start of `send`,
   `wait` and `runtime_input` (`WorkerHost::check_session`), which returns the lost session
   first, else the failed switch-on, so a `Wait` or a runtime read ends in the failed switch-on
   as a request is refused by it. They keep their own errors and refusal causes
   (`HostError::KeepAliveFailed`, cause `keep_alive_failed`, classified `system`, since the
   agent could not switch the handling; `HostError::SessionLost`, cause `session_lost`,
   `undetermined`, since the vehicle may have dropped the session): both are `NotSent` and
   `Other`, so `repeat_decision` ends the run for both, with no repeat and no reopen. The
   presence request does not pass the check; its functional link has no keep-alive.

7. **The log.** Each switch, on or off, writes a `keep_alive` event beside the exchange entries
   (design 8.11): `origin` `keep_alive`, `target`, `link`, `action` (`on` or `off`), `means`
   (`start_comm` when the session control that started communication switched it on, `update_param`
   for a switch by `CoptUpdateparam`), `result` (`ok`, or `failed` with its `detail`) and the
   agent's time only, since the VCI gives a switch no time. The StartComm itself is the VM's own
   exchange and is logged as such. A failed switch is logged whichever way it was meant.

8. **Noticing a lost session (implemented).** The link's event stream is read only inside a
   send-receive's wait and at the job's end, so a tester-present error event raised during a
   `Wait`, a server round trip or between steps waits for the next send-receive. The second part
   gives every reader of the stream one observer, drains the stream without blocking at the start
   of each request, `Wait` poll and runtime read, and latches a tester-present error event as the
   session lost only while the record holds a non-default session (decision a); with the record in
   the default session the event is logged and the run goes on. The latch is reported at the VM's
   next host call (ADR-418 item 6), at the check it shares with a failed switch-on (item 6
   above); the other items the drain reads keep their ADR-372 and ADR-386 placement.

   The mechanism, as built:

   - `KeepAlive::observe` is the one observer and the only writer of the latch. The send-receive
     wait, the tag wait (`resolve_tag`), the job-end settlement (`settle_at_end`) and the drain
     call it for every notification before they hand it on. A tester-present error is told by its
     event code, not by whether it names a primitive, since the J2534 worker's first
     tester-present of a switch-on runs inside the `CoptUpdateparam` primitive and its error may
     carry that handle; it is never taken as the result of the primitive it names or of the
     exchange that read it: an error of any handle, the exchange's own included (the J2534 worker
     raises the first one with the StartComm's handle before its finished status), stays an
     unbound item and never ends the exchange, so a StartComm's positive answer or the silence of
     a suppressed one is not turned into a failure. A lost VCI that ends the link (one of no
     primitive or of the primitive waited for, ADR-294) makes the errors after it unlatched until
     the link opens again (ADR-393), and one the drain read before the report point is what the
     run reports: the next request is refused after the loss and the host opens the link again,
     which ends a latch read before it. The loss, not the lapsed session, is what the run
     reports, as it is for the same items read inside an exchange.
   - The drain (`WorkerHost::drain_link_items`) reads the stream without waiting at the start of
     `send`, `wait` and `runtime_input` (one poll of the read per notification; a read that
     would wait is dropped, which `tonic::Streaming::message` allows without loss). It does not
     run while a stray primitive is unsettled: the stray's items belong to its settlement, and the
     settling wait observes. Results and errors of no primitive or of an earlier one wait in
     `pending_unbound` and go in front of the next exchange's own unbound items, so their
     placement is the one the exchange would have given them; what no exchange takes goes to the
     log at job end. A lost VCI of no primitive is kept, and the next request is refused as
     `VciLost { refused_after_loss: true }` without being sent, as the worker would refuse it
     (ADR-294); it enters the exchange entry as the lost VCI's error. A stream that ended is
     dropped, so the next request starts its primitive and fails on it as it does without the
     drain; one that failed with a status is kept, and the next request is refused with it before
     anything is sent, since no answer can be read from it: `HostError::StreamFailedBeforeSend`
     wraps the failure (`Rpc` of `SubscribeEvent`), is `NotSent`, takes the resend class of the
     failure (`Unavailable` and `Unknown` are a lost path, so the host opens the link again and
     sends the request, whatever its idempotency; any other status ends the run, the marker
     withdrawn), and is logged as a refusal, cause `stream_failed` (`system`), with the status
     in `detail.grpc_status`. A failure seen after a start stays the inner error, which may have
     reached the ECU. What the drain read that no exchange
     took (results, indications, an earlier primitive's timeout, as well as link errors) is
     placed by the same code as an exchange's unbound items (ADR-397) when the session is
     refused, at reopen and at job end, with no exchange in flight; the drain also runs once
     at job end, so an error queued after the VM's last call is logged.
   - Lost events. A lost-events notice read outside an exchange (by the drain, a tag wait or the
     job-end settlement) while the record holds a non-default session and the handling is on is
     the session lost, latched and reported like a tester-present error: the error may have been
     among the events lost (the maintainer's decision, 2026-11-05). The log gets a `keep_alive`
     entry with `action` `events_lost` and `session_lost` `true`. Outside that state it changes
     no outcome (a notice inside an exchange ends it as lost events as before) and is written to
     the operational log only.
   - The latch is reported by `check_session` at the start of `send` (the request is refused
     unsent, `NotSent`, `Other`, so `repeat_decision` ends the run and the request marker is
     withdrawn as not sent), of `wait` (a `Wait` ends at its next poll) and of `runtime_input`.
     A server round trip is not interrupted, and nothing checks after the VM's last call:
     `settle_at_end` still writes the entry of an error it reads. An error read during a step's
     exchange is latched all the same: the exchange's own result stands and the next call is
     refused. `reopen` starts the record over, the latch included.
   - The log: a `keep_alive` entry with `action` `error`, written for every tester-present
     error the observer read, latched or not (`session_lost`), with the event's name and its extra
     error information (ADR-351), `link_error` `tester_present`, the agent's time and no VCI time
     of its own. Its `refers_to` names the entries that placed the item's VCI time: the exchange
     entry that took it and the `unattached` or `late_item` lines the log wrote for it, so the
     time is placed once; with one such line per error, each entry refers to the line that
     placed its own error. The entry is written once those exist, not when the error is read:
     after the exchange entry, after the lines the drain's items get, or at job end. The refused
     request's entry has the cause `session_lost`, `caused_by` the error entry, and the
     classification `undetermined` (design 8.11).

## Consequences

- `WorkerApi` gains `GetStatus`, which the hardware harness's guarded client passes through its
  guard (the guard already decides it).
- A link whose library refuses the tester-present ComParams opens without the keep-alive; a
  session on it lapses as ADR-264 item 4 describes, and the real-hardware check of ADR-418 item 7
  shows whether a library does.
- With the keep-alive on, the link open sets eight more ComParams, and the first
  non-default session control of a job's link goes as a `CoptStartcomm`. Every test that counted
  the open's calls or the primitives' types sees them.
- The handling is believed, not read: a switch the worker accepted but did not apply goes
  unseen. The error event of the second part is what shows a session that lapsed.
- The J2534 worker's tester-present starts at the StartComm whatever answer it got, so for a short
  while after a refused session change the ECU receives a tester-present in its default session
  before the switch-off; the ECU answers it positively.
- ADR-418 items 3, 6 and 9 are amended: item 3 by the status question and the switch-off rule of
  item 5, item 6 by the rule that a tester-present error is the loss only while the record holds a
  non-default session (decision a), and item 9 by the `keep_alive` event's shape (item 7).
