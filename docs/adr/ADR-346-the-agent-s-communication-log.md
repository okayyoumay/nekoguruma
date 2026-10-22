# ADR-346: The Agent's Communication Log

**Date:** 2026-10-22
**Status:** Accepted
**Affects:** design 8.11 (new); `agent` (`WorkerHost::send_recv` / `send_recv_async` and the
link's event handling in `host.rs`, the ComParams `link.rs` sets, the job runner); the worker
interface's error events (`vci-service-interface`, `docs/rpc-api-guide.md`); `sim-vci`
(transmit indications); ADR-235 item 3, ADR-236, ADR-289, ADR-294

## Context

When a diagnostic run fails, nobody can tell today whether the vehicle or the diagnostic system
caused it. The existing records do not cover the exchanges:

- Acquired data stores ECU responses (design 4.3, ADR-236), and ADR-236 leaves bus-frame logging
  out of scope.
- The audit log records the executed instructions (design 8.2.3).

Every request the agent sends goes through `WorkerHost::send_recv_async` (`host.rs`). That is one
send-receive primitive on the worker's link, waited for under one agent deadline, with no retry
by the agent. Its outcomes are the host errors:

- no response;
- a failed primitive with the worker's error event;
- a lost VCI, while pending (ADR-289) or before the send (ADR-294);
- lost events;
- an unresponsive worker or event stream, or an RPC failure;
- the agent's own refusals before sending.

When the agent's deadline passes, the host returns the same no-response error as for a VCI's receive timeout, without cancelling the primitive.

The person who places a fault knows vehicle communication and the vehicle, but not this system.
What the worker interface gives today:

- **Response pending.** The link sets `CP_RC78Handling` to 1, so the API waits through 0x78 itself
  and reports only the final response or a receive timeout (ISO 22900-2:2022 Annex I.1.2,
  Figure I.4). A native D-PDU library gives no item for a pending it absorbs. `j2534-0404-service`
  handles 0x78 itself and passes those frames on (ADR-018 item 2); the agent skips them as not
  final (ADR-235 item 3).
- **Transmit confirmation.** It is off by default.
  - A D-PDU link gives a transmit indication only when `CP_TransmitIndEnable` is on (an optional
    ComParam, ISO 22900-2:2022 B.5.1). A result item may also carry a transmit-done time behind
    its timestamp flags (10.1.4.11.4, D.2.4; ADR-143), but a request that gets no response has no
    result item to carry it.
  - The J2534 worker passes a TxDone indication on only under the same ComParam (ADR-151). J2534
    v04.04 gives one only on ISO 15765 (clauses 8.6, 8.7.1), and the worker forwards it without a
    primitive handle.
  - The agent sets neither, and drops every item without a handle, link errors included,
    except a lost VCI, which ends the primitive (ADR-289). `sim-vci` gives no indication.
- **Error detail.** An error event reaches the agent as the bare event. Its extra error
  information (ISO 22900-2:2022 D.4.2) travels only with a failed RPC's error detail. That
  information tells a receive timeout with no response from a missing flow control or a missing
  consecutive frame.
- **Clocks.** VCI timestamps are 32-bit microseconds on the device's own time base, which wraps
  after about 71.6 minutes (ISO 22900-2:2022 8.1.6.1). On a J2534 link the frame timestamps are
  the device clock, while `GetTimestamp` and the error and status times come from the worker's
  synthetic clock (ADR-120), so one link shows two clocks.

Open: what one entry holds; how a pending, a transmit, an error's detail and a time are reported
when the interface does not give them; how an exchange is classified; and what a per-message log
cannot place.

## Decision

1. **One log per job, an entry per exchange, in the reader's terms.**
   - The agent writes a communication log for every job and ownership generation, as JSON Lines
     beside the job's journal and protected like it (design 5.5).
   - **Header.** It lists the job, the vehicle, each target's link with the ComParams in force,
     the VCI's version data and the software versions, so the log needs no other document.
   - **Exchange entries.** There is one for each request the agent sends or refuses before
     sending, with its origin: the procedure, a repeat after a resume (design 8.2.5),
     identification, restart reads, an access program, or an access target's keep-alive.
   - **System events.** Worker, VCI, link and job events, the identity values read, clock epochs,
     late responses, and events the log cannot attach to an exchange are entries on the same
     timeline.
   - **Wording.** Each entry says, in vehicle-communication terms:
     - whether the request reached the bus;
     - what happened;
     - the measured response time beside the P2 and P2* limits in force.

     It names the service and the negative response code beside their bytes, and keeps the
     internal names in a separate `detail` field.
   - Design 8.11 defines the fields and gives example logs.
2. **Response pending: logged as seen, never assumed absent.**
   - The API keeps handling 0x78 (`CP_RC78Handling` 1).
   - The entry lists each pending the agent sees, with its time. A worker that sees a pending
     passes it on, bound to the primitive.
   - The entry says whether the link can show pendings at all (`pending_visibility`: `forwarded`
     or `absorbed_by_vci`), so an empty list on a link whose VCI absorbs them is not read as
     "none".
   - A response window is derived from the elapsed time against P2, P2* and the completion limit:
     within P2, beyond P2 with absorbed pendings (marked as an inference), or no response within
     each limit.
   - A pending is an answer from the ECU: it confirms the send and counts in the link's history.
3. **"Reached the bus": transmit indications on, and a three-valued answer with its evidence.**
   - **Setting.** The link turns transmit indications on (`CP_TransmitIndEnable` 1). If the
     library refuses the ComParam, the link opens without it, and the header records that the
     link has no transmit confirmation.
   - **Values.** `sent` is `confirmed`, `partial` (the first frame of a multi-frame request went
     out and the ECU sent no flow control), `not_sent` or `unknown`. `sent_evidence` names the source,
     in this order:
     1. a transmit indication for the exchange, bound to its primitive, or unbound on the link
        while it is the only primitive running there;
     2. a transmit-done time on the response;
     3. any answer, a pending included, which confirms the send without a time.
   - **Not sent.** `not_sent` needs a definite refusal: the agent's own, the worker's refusal
     after a lost VCI (ADR-294), a transmit error event or a missing acknowledge. A refusal that an earlier response
     caused (a RequestDownload answer without the block length) names that exchange.
   - **Unknown.** An RPC that failed or timed out while starting the primitive, and every case
     without evidence, give `unknown`.
   - **Confirmation sources.** The header names each link's sources, so "unknown on a link that
     confirms its requests" reads differently from "unknown on a link that has no confirmation".
4. **Error detail reaches the agent.** The worker's error event carries its extra error
   information, by name. A receive timeout is then told apart:
   - a missing flow control: the request was not completely sent;
   - a missing consecutive frame: the ECU began a response and did not finish it;
   - no response at all.

   Without that information, the entry says which of these it cannot tell. The host also tells
   its own deadline passing from the VCI ending the exchange (`vci_did_not_end`).
5. **Classification on the communication axis.** Each exchange is `vehicle`, `system`,
   `undetermined` or `none` (completed without fault), with its reason. Design 8.11's table lists
   the rules for exchange entries in order, and the first that matches applies. The system-side
   failures come first, so a suppressed response or an answer never hides one.
   - **Negative responses.** A negative response is the ECU's answer, and `nrc_kind` sorts it:
     - the ECU's own state (busy, conditions, programming state): `vehicle`;
     - a request that does not fit the ECU as the definition built it (0x11, 0x12, 0x13, and
       0x14 and 0x31, which may mean either): `undetermined`;
     - not supported in the active session (0x7E, 0x7F: a lapsed session, or a definition that
       places the service in the wrong session): `undetermined`;
     - out of sequence (0x24, 0x73, which a repeat after a resume can cause): `undetermined`;
     - a key the ECU refused (0x35, 0x36): `system`, since the key comes from the server's key
       service (design 8.10).
   - **Unusable positive responses.** A positive response the agent cannot use (a wrong block
     echo, a missing length field), and the refusal of a later request it causes, are
     `undetermined`.
   - **No response.** A confirmed request with no response, an unfinished response, or a
     multi-frame request whose flow control never came, on a link that answered since its last
     error and reported no error during the exchange, is `vehicle`. It is `undetermined` in
     three cases:
     - when the link has not answered since its last error, its first exchanges included, since
       a wrong address or response filter is not excluded;
     - during a link error;
     - without a confirmation, since the VCI's receive path can lose a response that was on the
       bus.
   - **Suppressed responses.** A request with the positive-response suppression bit whose send is
     confirmed and that the VCI ended without a response is `none`. The host still ends such a
     request as having no response today; changing that is not part of this decision.
   - **Deadline.** An agent deadline that passed while the VCI never ended the exchange is
     `system`, sent or not, since the VCI must end its own primitive within its limits. This rule comes before the no-response rules.
   - **Transmit and receive errors.** A transmit error or a missing acknowledge is `not_sent` and
     `undetermined`. A response that broke the transport's rules (a wrong sequence number, wrong
     padding) is `response_malformed` with the send confirmed, `undetermined`, since the ECU and
     the VCI's receive path are not told apart. Other receive errors count as link errors. The entry's count of earlier answers on the link tells a network that went
     away from one never reached.
   - **Lost VCI.** A lost VCI is `undetermined`. A VCI powered from the vehicle's connector drops
     off USB or WLAN when the vehicle's supply drops, so the loss's kind does not show the side,
     and the agent knows no supply range of the VCI. The entry carries the extra error
     information and the last supply reading with its age. When the VCI reconnects in the job, the
     event says whether its time base carried on (it kept its power: the loss was the PC's
     connection, `system`) or restarted from zero (ISO 22900-2:2022 8.1.6.1: it was powered off,
     still `undetermined`), and names the loss.
   - **Other system failures.** A worker that exited or does not answer, lost events, an RPC
     failure and the agent's refusals are `system`.
6. **Responses that come after their exchange.**
   - The agent reads the link's events while it waits and when it closes the link. Items of an
     earlier primitive, or of none, are written as entries of their own.
   - A response to an earlier exchange is a `late_response` entry linked to that exchange. It is
     classified on its own, not by the exchange rules, by its VCI time against the limits in
     force:
     - after the limit: `vehicle` ("answered N ms after the limit");
     - in time, after the exchange ended for a system cause (lost events, the agent's deadline):
       that cause's `system`, as an answer the system missed.
   - The earlier entry is not rewritten.
   - Matching an unbound indication to the exchange by time holds only while one primitive runs
     on the link. Until the host cancels a primitive whose deadline passed, an unbound item after
     such a deadline is written unattached.
7. **Times: raw, tagged, aligned per clock.**
   - **Agent times.** Each entry has the agent's monotonic and wall-clock times at the start of
     the primitive, after it was started, at the transmit confirmation and at the end.
   - **VCI times.** It has the VCI times as raw values, each tagged with its clock: `device` for
     the VCI's own, `worker` for a worker's synthetic clock.
   - **Response latency.** Measured at the VCI, it is the response start less the transmit done,
     the points the client's P2 timing is measured between (ISO 14229-2:2021 clause 9). It
     includes the vehicle network and any gateway, not only the ECU. When that is missing, the
     response end less the transmit done is used, then the agent's times, and the entry names
     which. A difference is taken only between times of one clock.
   - **Alignment.** VCI times are aligned with the agent's clock per clock, by the least delay
     seen between a VCI time and the agent receiving it. The 32-bit values are unwrapped, and a
     reset, a reconnect or a backward step opens a new epoch, which the log records.
8. **Per message, raw CAN deferred.**
   - The log records messages (`layer: "message"`), and a `frames` field is reserved.
   - It cannot place these faults:
     - ISO-TP flow control and pacing (ISO 15765-2:2024 9.6.5, 9.7, 9.8), beyond what the error's
       extra information names;
     - CAN-level causes;
     - a response on an unexpected identifier;
     - the timing inside a chain of pendings the VCI absorbed.
   - A raw CAN channel for those is not decided here (alternatives below).
9. **The log does not steer the job.**
   - A failure to write it stops the log and marks the job's result as having an incomplete
     communication log. It never ends or changes the job.
   - The journal stays the only record a resume reads.

## Alternatives rejected

- **Handle 0x78 in the agent (`CP_RC78Handling` 0) to see every pending.** With handling off the
  primitive ends on the pending, and the final response has to be caught by a receive-only
  primitive (ISO 22900-2:2022 Annex I.1.2). That primitive has no P2-type timer, so the agent
  would rebuild the P2* and completion timing over the gRPC stream. A final response that follows
  the pending quickly can fall between the two primitives, and libraries differ most in exactly
  that case. It would also change ADR-235 item 3, which deserves its own decision if wanted.
- **Require every worker to report pendings.** A conforming native library reports none while it
  handles 0x78, so the requirement could not be met.
- **Leave transmit indications off and rely on responses and loopback.** Every request without a
  response would then be `unknown`, so an ECU that stops answering could never be told from a
  request that never left the VCI. Loopback echoes the whole message and adds receive traffic,
  and it stays a fallback for a library that refuses the indication.
- **Align the VCI clock with one `GetTimestamp` reading at link open.** On a J2534 link that call
  reads the worker's synthetic clock, not the frames' (ADR-120). Elsewhere a single sample has
  whatever delay it happened to get.
- **Classify every negative response as vehicle side.** Some codes point at the definition, the
  session or the system as often as at the vehicle: a request that does not fit the ECU, a
  service the active session does not allow, or a refused key.
- **Classify a confirmed request with no response as vehicle side always.** The VCI's response
  filters and buffers can drop a response that was on the bus. Without an earlier answer on the
  link, the log cannot exclude that.
- **A raw CAN channel now.** Both workers run UDS through the API's own ISO-TP, and a second
  channel on the same pins conflicts with the diagnostic one on most devices. Whether a device can
  run both is a hardware-profile question (design 13.5), so the channel waits for a device that
  shows it.

## Consequences

- **Implementation work.** The implementation, a backlog item of its own, needs:
  - the link to set `CP_TransmitIndEnable`;
  - the worker interface's error events to carry their extra error information
    (`vci-service-interface`, both workers, `docs/rpc-api-guide.md`);
  - the host to tell its own deadline from a VCI-ended exchange without a response;
  - the agent to read the link's unbound items (transmit indications, link errors) and those of
    earlier primitives, instead of dropping them;
  - `sim-vci` to give transmit indications, so a test can show a confirmed send with no response;
  - `docs/rpc-api-guide.md` to state that a worker passes on a pending it sees, bound to the
    primitive.
- **Links without confirmation.** A J2534 link on a protocol other than ISO 15765, and a library
  that refuses the indication, confirm only requests that got an answer. On those links an ECU
  that stops answering stays `undetermined`, and the entry says why.
- **No pending counts on most links.** On a link whose VCI absorbs pendings, the log shows the
  response window but not how many pendings came.
- **Personal data.** The log holds the VIN and ECU data, so it is local and protected like the
  journal. Its upload and retention are decided with the server's storage of it.
