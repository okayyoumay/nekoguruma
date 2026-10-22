# ADR-346: The Agent's Communication Log

**Date:** 2026-10-22
**Status:** Accepted
**Affects:** design 8.11 (new); `agent` (`WorkerHost::send_recv` / `send_recv_async` and the
link's event handling in `host.rs`, `link.rs`, the job runner); `sim-vci` (transmit
indications); ADR-235 item 3, ADR-236, ADR-289, ADR-294

## Context

When a diagnostic run fails, nobody can tell today whether the vehicle or the diagnostic system
caused it. The existing records do not cover the exchanges:

- Acquired data stores ECU responses (design 4.3, ADR-236), and ADR-236 leaves bus-frame logging
  out of scope.
- The audit log records the executed instructions (design 8.2.3).

Every request the agent sends goes through `WorkerHost::send_recv_async` (`host.rs`). That is one
send-receive primitive on the worker's link, waited for under one agent deadline, with no retry
by the agent. Its outcomes are the host errors: no response, a failed primitive with the
worker's error event, a lost VCI (while pending, ADR-289, or before the send, ADR-294), lost
events, an unresponsive worker or stream, an RPC failure, and the agent's own refusals before
sending.

The person who places a fault knows vehicle communication and the vehicle, but not this system.
What the worker interface gives for that:

- **Response pending.** The link sets `CP_RC78Handling` to 1, so the API waits through 0x78 itself
  and reports only the final response, or a receive timeout (ISO 22900-2:2022 Annex I.1.2,
  Figure I.4). A native D-PDU library gives no item for an absorbed pending.
  `j2534-0404-service` handles 0x78 itself and passes those frames on (ADR-018 item 2). The
  agent skips them as not final (ADR-235 item 3).
- **Transmit confirmation.** It depends on the API:
  - J2534 v04.04 gives a TxDone indication only on ISO 15765 (clauses 8.6 and 8.7.1), and a
    loopback echo when loopback is on; the J2534 worker forwards both without a primitive handle,
    and the agent drops every item without one.
  - A D-PDU result item may carry a transmit-done and a start-of-message time, behind its
    timestamp flags (ISO 22900-2:2022 10.1.4.11.4, D.2.4; ADR-143). A request that gets no
    response has no result item to carry one.
- **Clocks.** VCI timestamps are 32-bit microseconds on the device's own time base, which wraps
  after about 71.6 minutes (ISO 22900-2:2022 8.1.6.1). On a J2534 link the frame timestamps are
  the device clock, while `GetTimestamp` and the error and status times come from the worker's
  synthetic clock (ADR-120), so one link shows two clocks.

Open: what one entry holds, how a pending, a transmit and a time are reported when the API does
not give them, how an exchange is classified, and what a per-message log cannot place.

## Decision

1. **One log per job, an entry per exchange, in the reader's terms.**
   - The agent writes a communication log for every job and ownership generation, as JSON Lines
     beside the job's journal and protected like it (design 5.5).
   - **Header.** It lists the job, the vehicle, each target's link with the ComParams in force,
     the VCI's version data and the software versions, so the log needs no other document.
   - **Entries.** There is one exchange entry for each request the agent sends or refuses before
     sending, from any origin (the procedure, identification, restart reads, an access target's
     keep-alive). System events (worker, VCI, link, job, identity values, clock epochs, late
     responses) are entries on the same timeline.
   - **Wording.** Each entry says, in vehicle-communication terms, whether the request reached
     the bus, what happened, and the measured response time beside the P2 and P2* limits in
     force. It names the service and the negative response code beside their bytes. The internal
     names go in a separate `detail` field.
   - Design 8.11 defines the fields and gives example logs.
2. **Response pending: logged as seen, never assumed absent.**
   - The API keeps handling 0x78 (`CP_RC78Handling` 1). The entry lists each pending the agent
     sees, with its time.
   - It also says whether the link can show them at all (`pending_visibility`: `forwarded` or
     `absorbed_by_vci`), so an empty list on a link whose VCI absorbs pendings is not read as
     "none".
   - A response window is derived from the elapsed time against P2, P2* and the completion limit:
     within P2, beyond P2 with absorbed pendings (marked as an inference), or no response within
     each limit.
   - A worker that sees a pending passes it on, bound to the primitive.
3. **"Reached the bus" is three-valued, with its evidence.**
   - `sent` is `confirmed`, `not_sent` or `unknown`, and `sent_evidence` names its source, in
     this order:
     1. a transmit indication on the link while the exchange runs (TxDone, loopback, or a D-PDU
        result item flagged as a transmit), bound to the primitive or not;
     2. a transmit-done time on the response;
     3. any response, which confirms the send without a time;
     4. a refusal before sending (the agent's own, or the worker's after a lost VCI, ADR-294),
        a failed StartComPrimitive or a transmit error, which give `not_sent`.

     Anything else is `unknown`.
   - Matching an unbound indication to the exchange by time is sound only while one primitive
     runs on the link, which the host holds to.
   - The header names each link's transmit-confirmation sources, so "unknown on a link that
     confirms its requests" reads differently from "unknown on a link that never does".
4. **Classification on the communication axis.**
   - Each exchange is `vehicle`, `system`, `undetermined` or `none` (completed without fault),
     with its reason.
   - **Negative responses.** A negative response is the ECU's own answer: `vehicle`. The
     exception is a code that says the ECU does not accept the request as the definition built
     it (0x11, 0x12, 0x13, 0x7E, 0x7F, and 0x14 and 0x31, which may mean either), which is
     `undetermined`. The entry's `nrc_kind` records which.
   - **No response.** A request with a confirmed send, on a link that answered before and
     reported no error meanwhile, is `vehicle`. It is `undetermined` on the link's first
     exchange (a wrong address or response filter is not excluded), after a link error, or
     without a confirmation, since the VCI's receive path can lose a response that was on the bus.
   - **Transmit errors.** A transmit error is `undetermined`; the entry's count of earlier answers
     on the link tells a network that went away from one never reached.
   - **System side.** These are `system`: a lost VCI, a worker that exited or does not answer,
     lost events, an RPC failure, the agent's refusals, and an agent deadline that passed while
     the VCI, having confirmed the send, never ended the exchange.
   - A response that arrives after the exchange ended is written when the agent next reads the
     link's events, as a `late_response` entry linked to the exchange, `vehicle`. The earlier
     entry is not rewritten.
5. **Times: raw, tagged, aligned per clock.**
   - **Agent times.** Each entry has the agent's monotonic and wall-clock times at the start of
     the primitive, after it was started, at the transmit confirmation and at the end.
   - **VCI times.** It also has the VCI times as raw values, each tagged with its clock (`device`
     or `worker`).
   - **ECU latency.** It is the response start less the transmit done, the points the client's
     P2 timing is measured between (ISO 14229-2:2021 clause 9). When that is missing, the
     response end less the transmit done is used, and failing that the agent's times; the entry
     names which.
   - **Alignment.** VCI times are aligned with the agent's clock per clock, by the least delay
     seen between a VCI time and the agent receiving it. The 32-bit values are unwrapped, and a
     reset, a reconnect or a backward step opens a new epoch, which the log records.
6. **Per message, raw CAN deferred.**
   - The log records messages (`layer: "message"`). A `frames` field is reserved.
   - It cannot place these faults:
     - ISO-TP flow control and pacing (ISO 15765-2:2024 9.6.5, 9.7, 9.8), except where the
       error's extra information names them;
     - CAN-level causes;
     - a response on an unexpected identifier;
     - the timing inside a chain of pendings the VCI absorbed.
   - A raw CAN channel for those is not decided here (see the alternatives below).
7. **The log does not steer the job.**
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
- **Align the VCI clock with one `GetTimestamp` reading at link open.** On a J2534 link that call
  reads the worker's synthetic clock, not the frames' (ADR-120). Elsewhere a single sample has
  whatever delay it happened to get.
- **Classify every negative response as vehicle side.** A code that says the request does not fit
  the ECU (wrong length, unsupported service or sub-function) points at the definition as often
  as at the vehicle.
- **Classify a confirmed request with no response as vehicle side always.** The VCI's response
  filters and buffers can drop a response that was on the bus. Without an earlier answer on the
  link, the log cannot exclude that.
- **A raw CAN channel now.** Both workers run UDS through the API's own ISO-TP, and a second
  channel on the same pins conflicts with the diagnostic one on most devices. Whether a device can
  run both is a hardware-profile question (design 13.5), so the channel waits for a device that
  shows it.

## Consequences

- **Implementation work.** The implementation, a backlog item of its own, has to:
  - have the agent read unbound transmit indications and drain the link's events at primitive
    boundaries;
  - give `sim-vci` transmit indications, so a test can show a confirmed send with no response;
  - document the worker property on pendings and unbound indications in `docs/rpc-api-guide.md`.
- **J2534 ISO 15765 links.** These confirm sends. A native D-PDU link confirms only a request that
  got a response, until loopback (`CP_Loopback`) is checked on real hardware.
- **No pending counts on most links.** On a link whose VCI absorbs pendings, the log shows the
  response window but not how many pendings came.
- **Personal data.** The log holds the VIN and ECU data, so it is local and protected like the
  journal. Its upload and retention follow when the server receives it.
