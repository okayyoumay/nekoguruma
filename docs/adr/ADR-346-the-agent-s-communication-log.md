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
   combinations with their document hashes and conversion identities, since the chosen one and
   its document are known only after identification, ADR-325 item 7, and follow as an event),
   with the `vci_connected`
   event and a `link_opened` event per link (one per target, which may share a physical channel
   and be reopened, design 8.2.4, 8.8.2), gives
   everything needed to read it. There is one entry for each request the agent sends or refuses
   before sending, from any origin; system events share the timeline. Each entry says, in
   vehicle-communication terms, whether the request reached the bus, what happened, and the
   response time beside the limits in force; it names the service and the negative response code
   beside their bytes, and keeps the internal names in a separate `detail` field. Secrets
   (SecurityAccess seeds and keys, Authentication data) are logged as their length only, since a
   key with few possible values could be recovered from any digest of it (a seed also records
   whether it is all zero, which shows a level already unlocked, ISO 14229-1:2026 9.4); bulk
   TransferData is logged as length and hash.
2. **Where the evidence does not decide, the entry says so.** Every judgement rests on what the
   agent can observe, and a question the evidence leaves open is `unknown`, `undetermined` or
   "not judged", never a guess. In particular:
   - a difference is taken only between times of one clock and epoch, and a limit is passed only
     beyond the link's timing margin; the agent's clock starts a new epoch when a log is
     reopened, since its monotonic clock starts over with the agent;
   - a time is judged against a limit only from the VCI's transmit-done time (or a loopback
     echo's, which the VCI takes at the transmit's end) to a VCI time of the same clock. A
     response start or a receive timeout is judged both ways. A response's end includes its own
     transfer, so it is judged one way only: within the limit it proves the response on time,
     and it proves it late only for a response sent as one frame or one UART message, past the
     limit plus the margin plus the longest transfer of that length at the link's speed (a
     multi-frame response's end includes flow control the log does not see, so it never proves
     it late). A single-frame response on CAN usually has only its end, since
     start-of-message indications come only for a first frame or a UART first byte (ISO
     22900-2:2022 8.1.6.3, 8.1.6.5). Agent times, which include the RPC and worker queuing, are
     reported but never decide lateness or an early timeout;
   - on a link whose VCI absorbs pendings, a limit that depends on the unseen pendings is bounded
     from the transmit and judged only where the bound decides;
   - an extra error code is used only when it names the case, since part of the codes are the
     vendor's (ISO 22900-2:2022 D.4.2).
3. **Response pending: logged as seen, never assumed absent.** The API keeps handling 0x78
   (`CP_RC78Handling` 1); a worker that sees a pending passes it on, bound to the primitive. The
   entry lists the pendings seen and says whether the link can show them at all
   (`pending_visibility`), so an empty list on a link whose VCI absorbs them is not read as
   "none". P2* restarts on each pending and the completion limit runs from the first, as the
   API's handling does (ISO 22900-2:2022 Annex I.1.2, Figure I.4); each forwarded pending is
   judged against the limit before it, and one past it makes the exchange `system` whatever
   follows, since the ECU was late and the VCI did not end its wait.
4. **"Reached the bus": transmit indications on, and a three-valued answer with its evidence.**
   The link turns transmit indications on (`CP_TransmitIndEnable` 1, ISO 22900-2:2022 B.5.1); a
   library that refuses it opens the link without, and `link_opened` says so. `sent` is
   `confirmed`, `partial`, `not_sent` or `unknown`, with the evidence that set it. An unbound
   indication counts for an exchange only by its request identifier (SAE J2534-1 v04.04 clause
   8.6), while no other exchange on the physical channel is in flight with that identifier and the
   API sends no tester present of its own on it, since the J2534 worker gives a TxDone to every
   link on the channel (ADR-151), and no ended exchange on that identifier could still own it
   (one whose send is unknown and that nothing definite ended, within its limit in force).
   Any answer of the ECU, a pending or a first frame included, confirms the send; a flow-control
   frame shows only that the first frame went out (`partial`). `not_sent` needs a definite refusal
   or a transmit error; a failed start of the primitive is `unknown`.
   The link turns start-of-message indications on as well (`CP_StartMsgIndEnable`), since they
   carry the start time of a multi-frame response; an unbound one counts for an exchange only
   by its response identifier, under the same exclusivity, since the J2534 worker gives it to
   every link on the channel too.
5. **Error detail reaches the agent; the link hides nothing else.** The worker's error events
   carry their extra error information, and the host tells its own deadline from the VCI ending
   the exchange. A receive timeout is an outcome of its exchange; only other error events, on
   any link of the same physical channel, count as link errors. The link sets
   `CP_RC21Handling` and `CP_RC23Handling` to 0, so the API repeats no request unseen on a busy
   answer, and the agent never sets the suppress-positive-response transmit flag (ISO
   22900-2:2022 Annex I.2.1), with which the API would end a silent exchange without a timeout
   and hide an unexpected positive response. A primitive past the agent's deadline holds its
   link (8.4.17.2), so the agent cancels it before the link's next request, and at job end after
   the longest transfer of the largest response its definition allows.
6. **Classification on the communication axis, from one rule.** Each exchange is `vehicle`,
   `system`, `undetermined` or `none`, by the first matching row of design 8.11's table; the rows
   are derived from one rule, which the design states before the table:
   - four hidden causes can make the system look like the vehicle, each excluded only by named
     evidence: the request never reached the bus (excluded by a confirmed or partial send, or an
     answer in the exchange); the worker dropped a response that did not match, or the address
     or filter is wrong (excluded by an answer in the exchange, or an earlier answer matched the
     same way, by the same response identifier and the same expected response patterns with
     what they echo of the request, such as a data identifier, since the last link error on the physical channel and the target's last reset or
     session change); a frame lost in the transport or the VCI's receive path (excluded only by
     no link error during the exchange, and never for a missing flow-control frame); and a
     pending the VCI absorbed (excluded on a link that forwards pendings, or by timing within P2
     judged on one clock; a pending is never presumed absent);
   - one presumption is accepted, and only one: the VCI's own timer and its handling of pendings
     as configured (each pending restarts its wait) are presumed right where they cannot be
     judged (so a timeout at P2 shows that no pending came), since otherwise no silence could be
     placed on a link without transmit times or on one whose VCI absorbs pendings; an entry
     that rests on it says so;
   - an exchange is `vehicle` when every reading the evidence leaves is a fault of the ECU or
     the vehicle network; `system` when every reading names a failed part of the system, or the evidence shows one (a refusal
     by the agent or the worker for its own reasons, a worker or RPC failure, lost events, a
     VCI fault of its own, a timer judged wrong), or the system gave up on or missed an exchange
     that ended well; `none` when every reading is fault-free; otherwise `undetermined`. Faults
     seen together take the most severe, a link error the exchange's own link reported
     during it included; alternative readings never combine.

   What this gives, in particular:
   - an agent deadline is `system` only when a reading of the VCI's device clock at the
     deadline, less its round trip, shows the VCI's own limits had passed after a confirmed send
     with no response under way, and even then only until the primitive's end settles it (item
     7); on J2534 no such reading exists (ADR-120);
   - a response the ECU began and did not finish, and a missing flow-control frame, are
     `undetermined`; only a flow-control wait past its limit that the extra error information
     names, with no link error during the exchange, is `vehicle`;
   - a lost VCI keeps the `sent` that the evidence before the loss gave it;
   - a negative response is the ECU's answer: `vehicle` only for the codes that name the ECU's
     or the vehicle's own state (ISO 14229-1:2026 Annex A.1: busy, conditions not correct and its
     ranges, programming state); `none` for a code the definition declares definitive for an
     identification or guard read (ADR-327); any of these after a possible pending is
     `undetermined` (below); `undetermined` for every other code, an invalid key
     (0x35) included, since the key service, the variant's secret and the seed are not told
     apart, and for those that point at the definition, the session, access or sequence, an
     attempt counter another tester may have used up, secured data transmission,
     authentication, and codes with no known meaning;
   - no response is `vehicle` only with the send confirmed (or an answer of the ECU in the
     exchange), an earlier matched answer, and no link error during the exchange;
   - a timeout before the limit in force, or after it where no unseen pending can explain the
     delay, is `system`; a final response after it that the VCI still took is `system` too, since
     its timer is judged wrong beside the ECU's lateness, and one the worker delivers after the
     VCI ended the exchange is `vehicle`;
   - on a link whose VCI absorbs pendings, a timeout or response later than P2, or one whose
     timing cannot be judged, with no pending seen, is a possible pending: a no-response or any
     final response there that is not late is `undetermined`;
   - a suppressed positive response, only for a service with a sub-function as the definition
     declares it (ISO 14229-1:2026 7.7.3), is fault-free when none comes after a confirmed send,
     with an earlier matched answer, no link error and no pending seen or possible, `vehicle` when one comes anyway, not late, on
     a link that forwards pendings with none seen, fault-free after a forwarded pending (the ECU
     then owes its final response), and `undetermined` on a link whose VCI absorbs pendings,
     where a pending may precede it even within P2;
   - a request that collects every answer takes the most severe of its answers' classifications
     and of any link error or possible pending during its window (another link's error counts
     as `undetermined`), each answer judged on its own against the window, and silence is no fault only for the
     functional presence request with its send confirmed, no link error in its window and no
     possible pending (ADR-327 item 2); on a guard's request a second ECU is `vehicle` or the most severe of its
     answers (design 8.9.1), and so is a request meant for one ECU that gets more than one answer,
     unless the previous exchange with the same match went unanswered, whose late answer the
     extra one may be;
   - a lost VCI is `undetermined` until its clock, after it reconnects, shows that it kept its
     power (only then `system`; the reading is compared within a VCI-wide clock margin plus a
     recorded drift bound over the gap), since a VCI powered from the vehicle's connector drops off when
     the vehicle's supply drops. A clock that restarted does not decide it, nor does a
     reconnected VCI with another serial number or a gap that spans an agent restart, and on
     J2534, whose frame clock has a vendor-defined origin (ADR-120), the loss stays
     `undetermined` unless a hardware profile (design 13.5) states how that VCI's clock behaves
     across a power loss.
7. **One settlement rule for evidence after the entry.** The agent reads the link's items
   while it waits and when it closes the link. Evidence about an exchange that comes after its
   entry is a settling entry naming the exchanges it settles; it stands on its own (it repeats
   the link, origin and request), is classified from every item known at that point, and the
   latest entry referring to an exchange gives its standing classification. The earlier entry
   is never rewritten. A settling entry carries the facts of the settling entries before it (a
   give-up, a kept power), keeps any system-side fault an earlier entry observed (it takes the
   most severe of that fault and its own classification), and a cancellation by the agent settles
   nothing. The settling entries
   are:
   - the end of a primitive the agent stopped waiting for (`primitive_ended`), with every item
     that came after the entry, classified as if the agent had waited, except that an end within the
     limit in force is `system` whatever its outcome, since the job gave up on an exchange that
     ended in time; an end by a lost VCI or a worker exit
     takes that row;
   - the settling of a lost VCI (`vci_loss_settled`), for every exchange the loss ended, each
     with its own classification, with the
     clock readings, the measured gap, the tolerance and each fit test, so a reader can redo it;
   - a response the worker delivers after the exchange ended (`late_response`): started within
     the limit less the margin it is `system`, since the system missed an answer in time; after
     the limit plus the margin `vehicle`, or its outcome's classification if that is more
     severe; otherwise `undetermined`; it belongs to the ended exchange it
     matches that has no response yet, however long ago it ended, and one that two ended
     exchanges could own is unattached;
   - a transmit indication, loopback echo, start-of-message indication or forwarded pending that
     comes after the entry (`late_item`); a late start-of-message indication turns an exchange
     that ended without a response into `response_incomplete`.

   The J2534 worker discards a frame no primitive expects (ADR-100), so on such links a late
   response does not reach the log; capturing it would need a receive-only primitive beside the
   exchanges, which is not decided here.
8. **Times: raw, tagged, aligned per clock.** The entry keeps agent times (monotonic and wall)
   and VCI times as raw values tagged with their clock (`device` or `worker`, ADR-120) and epoch.
   The response latency is the VCI's response start less its transmit done (ISO 14229-2:2021
   clause 9), which includes the vehicle network and any gateway. VCI times are aligned per clock
   by least delay, unwrapped from 32 bits (ISO 22900-2:2022 8.1.6.1), with a new epoch for a
   reset, a backward step or a reconnect whose clock did not carry on.
9. **Per message, raw CAN deferred.** The log records messages (`layer: "message"`) with a
   `frames` field reserved. Design 8.11 lists what it cannot place: the tester-present requests the API sends itself (the
   log records that the setting is on and any error it reports), transport-level faults
   (ISO 15765-2:2024 9.6.5, 9.7, 9.8) beyond what the error information names, CAN-level causes, a
   response the worker drops as not matching, a late answer that matches the next request, and the
   timing inside absorbed pendings, and the real end of a primitive the agent cancelled before the
   link's next request.
10. **The log does not steer the job.** A failure to write it stops the log and marks the job's
    result as having an incomplete communication log; a log reopened without a clean close is
    marked incomplete too (a torn last line is dropped, and unflushed lines may be missing
    without one), and the reopen records a new agent clock reading. It never ends or changes the job, and the journal stays
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
  - the link to set `CP_TransmitIndEnable` and `CP_StartMsgIndEnable`, and `CP_RC21Handling` and
    `CP_RC23Handling` to 0;
  - the agent to cancel a primitive past its deadline before the link's next request, and at job
    end after the longest transfer of the largest response its definition allows;
  - the worker to report why it refused a request (invalid parameters, a full queue, a request
    the protocol cannot carry) in a form the agent can tell apart;
  - the worker interface's error events to carry their extra error information
    (`vci-service-interface`, both workers, `docs/rpc-api-guide.md`);
  - the host to tell its own deadline from a VCI-ended exchange without a response, and to read
    the VCI's clock (`GetTimestamp`) at that deadline on a D-PDU link;
  - each link to record its timing margin, and each VCI connection its clock margin and drift bound, from a
    configured default or a hardware profile (design 13.5), since no API reports a VCI's timer
    spread or drift;
  - the log's writer to log secrets as length only (with a seed's all-zero flag) and bulk data
    as length and hash, and to mark a log reopened without a clean close incomplete and write a
    new agent clock reading;
  - the agent to read the link's unbound items (transmit indications, link errors) and those of
    earlier primitives, instead of dropping them, and to write the settling entries from them;
  - `sim-vci` to give transmit indications, so a test can show a confirmed send with no response;
  - `docs/rpc-api-guide.md` to state that a worker passes on a pending it sees, bound to the
    primitive.
- **Links without confirmation.** A J2534 link on a protocol other than `ISO15765` (SAE J2534-1
  v04.04 clause 8.6), and a library
  that refuses the indication, confirm only requests that got an answer, unless the link has
  loopback on. On those links an ECU
  that stops answering stays `undetermined`, and the entry says why.
- **Fewer exchanges judged on links without transmit times.** A library that refuses the
  transmit indication and gives no transmit-done time leaves response times not judged, so on a
  link whose VCI absorbs pendings its positive responses after a send are `undetermined`;
  `link_opened` says that response times are not judged there.
- **The real end is not always kept.** A primitive past the agent's deadline is cancelled before
  the link's next request, so its real end is known only when it ends before that request or
  within the job-end wait; otherwise its entry keeps the classification it had.
- **No pending counts on most links.** On a link whose VCI absorbs pendings, the log shows the
  response window but not how many pendings came.
- **Personal data.** The log holds the VIN and ECU data, so it is local and protected like the
  journal. Its upload and retention are decided with the server's storage of it.
