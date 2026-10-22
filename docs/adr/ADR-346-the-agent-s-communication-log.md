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
    v04.04 gives one only on its `ISO15765` protocol (SAE J2534-1 v04.04 clauses 8.6, 8.7.1),
    and the worker forwards it without a
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

Design 8.11 holds the fields, the terms, the ordered classification table and example logs. The
decisions behind them:

1. **One log per job, an entry per exchange, in the reader's terms.** The agent writes a
   communication log for every job and ownership generation, as JSON Lines beside the job's
   journal and protected like it (design 5.5). A header at job start (listing the candidate
   combinations with their document hashes, since the chosen one and its document are known only
   after identification, ADR-325 item 7, and follow as an event), with the `vci_connected`
   event and a `link_opened` event per link (a job may open several in turn, design 8.8.2), gives
   everything needed to read it. There is one entry for each request the agent sends or refuses
   before sending, from any origin; system events share the timeline. Each entry says, in
   vehicle-communication terms, whether the request reached the bus, what happened, and the
   response time beside the limits in force; it names the service and the negative response code
   beside their bytes, and keeps the internal names in a separate `detail` field. Secrets
   (SecurityAccess seeds and keys, Authentication data) are logged as their length only, since a
   key with few possible values could be recovered from any digest of it; bulk TransferData is
   logged as length and hash.
2. **Where the evidence does not decide, the entry says so.** Every judgement rests on what the
   agent can observe, and a question the evidence leaves open is `unknown`, `undetermined` or
   "not judged", never a guess. In particular:
   - a difference is taken only between times of one clock and epoch, and a limit is passed only
     beyond the link's timing margin;
   - a response's lateness is judged only from its start (or the end of a single-frame response)
     on one clock with the transmit;
   - on a link whose VCI absorbs pendings, a limit that depends on the unseen pendings is bounded
     from the transmit and judged only where the bound decides;
   - an extra error code is used only when it names the case, since part of the codes are the
     vendor's (ISO 22900-2:2022 D.4.2).
3. **Response pending: logged as seen, never assumed absent.** The API keeps handling 0x78
   (`CP_RC78Handling` 1); a worker that sees a pending passes it on, bound to the primitive. The
   entry lists the pendings seen and says whether the link can show them at all
   (`pending_visibility`), so an empty list on a link whose VCI absorbs them is not read as
   "none". P2* restarts on each pending and the completion limit runs from the first, as the
   API's handling does (ISO 22900-2:2022 Annex I.1.2, Figure I.4).
4. **"Reached the bus": transmit indications on, and a three-valued answer with its evidence.**
   The link turns transmit indications on (`CP_TransmitIndEnable` 1, ISO 22900-2:2022 B.5.1); a
   library that refuses it opens the link without, and `link_opened` says so. `sent` is
   `confirmed`, `partial`, `not_sent` or `unknown`, with the evidence that set it. An unbound
   indication counts for an exchange only by its request identifier (SAE J2534-1 v04.04 clause
   8.6), while no other exchange on the physical channel is in flight with that identifier and the
   API sends no tester present of its own on it, since the J2534 worker gives a TxDone to every
   link on the channel (ADR-151). Any answer of the ECU, a pending or a first frame included,
   confirms the send. `not_sent` needs a definite refusal or a transmit error; a failed start of
   the primitive is `unknown`.
5. **Error detail reaches the agent.** The worker's error events carry their extra error
   information, and the host tells its own deadline from the VCI ending the exchange. A receive
   timeout is an outcome of its exchange; only other error events count as link errors.
6. **Classification on the communication axis.** Each exchange is `vehicle`, `system`,
   `undetermined` or `none`, by the first matching row of design 8.11's table; system-side
   failures come first, and a last row makes any other case `undetermined`. The principles:
   - a refusal by the agent, a worker or RPC failure, lost events, and a VCI fault of its own are
     `system`; an agent deadline is `system` only when the VCI's own limits had passed after a
     confirmed send, since a long transfer can outlast the deadline;
   - a negative response is the ECU's answer: `vehicle` for its own state, `undetermined` when the
     code points at the definition, the session or access state, or the sequence, and `system`
     for a refused key, which the server's key service computed (design 8.10);
   - no response is `vehicle` only with the send confirmed (or an answer of the ECU in the
     exchange) on a link that has an answer of the ECU since its last link error and reported
     none during the exchange; otherwise `undetermined`, since a wrong address, a response filter
     or the VCI's receive path is not excluded;
   - a timeout before the limit in force is `system`; a final response after it is `vehicle`;
   - a suppressed positive response, only for a service with a sub-function as the definition
     declares it (ISO 14229-1:2026 7.7.3), is fault-free when none comes and no pending was seen
     or inferred, and `vehicle` when one comes anyway; after a pending the ECU owes its final
     response;
   - a request that collects every answer takes the most severe of its answers' classifications,
     each answer judged on its own against the window, and silence is no fault only for the
     functional presence request (ADR-327 item 2); on a guard's request a second ECU is `vehicle`
     (design 8.9.1); a request meant for one ECU that gets more than
     one answer is `vehicle`;
   - a lost VCI is `undetermined` until its clock, after it reconnects, shows that it kept its
     power (only then `system`), since a VCI powered from the vehicle's connector drops off when
     the vehicle's supply drops. A clock that restarted does not decide it (on J2534 its origin is
     the vendor's).
7. **Responses after their exchange.** The agent reads the link's items while it waits and when
   it closes the link, so a response that the worker delivers after its exchange is a
   `late_response` entry referring to that exchange, judged on its own against that exchange's
   limits. The earlier entry is never rewritten. The J2534 worker discards a frame no primitive
   expects (ADR-100), so on such links a late response does not reach the log; capturing it would
   need a receive-only primitive beside the exchanges, which is not decided here.
8. **Times: raw, tagged, aligned per clock.** The entry keeps agent times (monotonic and wall)
   and VCI times as raw values tagged with their clock (`device` or `worker`, ADR-120) and epoch.
   The response latency is the VCI's response start less its transmit done (ISO 14229-2:2021
   clause 9), which includes the vehicle network and any gateway. VCI times are aligned per clock
   by least delay, unwrapped from 32 bits (ISO 22900-2:2022 8.1.6.1), with a new epoch for a
   reset, a backward step or a reconnect whose clock did not carry on.
9. **Per message, raw CAN deferred.** The log records messages (`layer: "message"`) with a
   `frames` field reserved. Design 8.11 lists what it cannot place: transport-level faults
   (ISO 15765-2:2024 9.6.5, 9.7, 9.8) beyond what the error information names, CAN-level causes, a
   response the worker drops as not matching, a late answer that matches the next request, and the
   timing inside absorbed pendings.
10. **The log does not steer the job.** A failure to write it stops the log and marks the job's
    result as having an incomplete communication log; a torn last line after a power loss is
    dropped and marks it incomplete too. It never ends or changes the job, and the journal stays
    the only record a resume reads.

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
  and it counts as evidence only where the link has loopback on.
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
  - each link to record its timing margin, from a configured default or a hardware profile
    (design 13.5), since no API reports a VCI's timer spread;
  - the log's writer to log secrets as length only and bulk data as length and hash, and to
    recover after a
    torn last line;
  - the agent to read the link's unbound items (transmit indications, link errors) and those of
    earlier primitives, instead of dropping them;
  - `sim-vci` to give transmit indications, so a test can show a confirmed send with no response;
  - `docs/rpc-api-guide.md` to state that a worker passes on a pending it sees, bound to the
    primitive.
- **Links without confirmation.** A J2534 link on a protocol other than `ISO15765` (SAE J2534-1
  v04.04 clause 8.6), and a library
  that refuses the indication, confirm only requests that got an answer, unless the link has
  loopback on. On those links an ECU
  that stops answering stays `undetermined`, and the entry says why.
- **No pending counts on most links.** On a link whose VCI absorbs pendings, the log shows the
  response window but not how many pendings came.
- **Personal data.** The log holds the VIN and ECU data, so it is local and protected like the
  journal. Its upload and retention are decided with the server's storage of it.
