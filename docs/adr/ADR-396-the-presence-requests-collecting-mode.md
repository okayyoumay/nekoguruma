# ADR-396: The Presence Request's Collecting Mode, Its Halving Trigger and Its Loss Rule

**Date:** 2026-10-31
**Status:** Accepted
**Affects:** `agent` (`link`, `policy`, `host`, `comm_log`, `runner`, `identification`),
`crates/agent/docs/ngr-agent.md`, design 8.8.2 and 8.11; ADR-235 item 8, ADR-327 item 2,
ADR-392, ADR-393, ADR-394, ADR-395

## Context

ADR-327 item 2 has an identification job send one functional presence request per bus and
functional identifier before any other request, collecting every answer, and ADR-395 decides the
plan format, the presence group, the window (the longest `CP_P2Max` of the group) and that a
receive table the VCI refuses is halved. The agent had none of the machinery: its link addresses
one ECU, its host ends a primitive at the first answer, and TesterPresent is refused to every job.
This ADR records how the presence request is sent and what it does when a part of it fails. The
identification job kind and its CLI, the classification rows for `answers` and the J2534 worker's
handling of a refused table entry are separate steps.

## Decision

1. **A functional link.** `LinkConfig` can address functionally (`functional`: the functional
   identifier and a receive table of physical request and response pairs). Such a link sets
   `CP_RequestAddrMode` to 2 and `CP_CanFuncReqId` to the functional identifier, and gives the
   response identifier table one entry per pair, numbered from 1 in table order; both ComParams
   are ones the link sets itself, so a document cannot declare them. The table and the
   identifier are validated like the physical ones (11 bits, no repeated pair, the functional
   identifier not one of the table's). The request goes to the functional identifier, so
   the link's `tx_id` is that identifier and its `rx_id` is 0. The send-receive ceiling allows
   one `CP_P2Max` window per pair, since the API restarts the window for each answer (ADR-395
   item 5). `link_opened` records the functional identifier and the table (design 8.11).
2. **Collecting mode in the host.** `WorkerHost::presence` is a send-receive whose primitive has
   `num_receive_cycles` of -2 and whose progress collects every result bound to the primitive
   that is not an indication, in order, until the primitive's finished status. The response
   identifier of a result is the CAN identifier in its header, else the table entry its
   unique-response number names, else the only pair of a one-pair table; a result with none of
   these is not presence. Every collected result on a table identifier within the window marks
   presence for every endpoint with that bus and response identifier: a positive answer, a
   negative one (`7F 3E xx`), a pending (`7F 3E 78`) that no final answer on its identifier
   followed, and a result that does not answer the request (a wrong echo). Presence drops nothing
   (ADR-327 item 2), so counting every result of a declared identifier is harmless where counting
   fewer could hide an ECU. A pending followed by its final answer is one answer. An answer after
   the window has ended is not presence: nothing reads it, and where a later primitive on the
   same link reads it, the existing path logs it as unattached.
3. **Only `presence` sends TesterPresent.** `policy::check_presence_request` accepts exactly
   `3E 00` on UDS, whatever the link's permission, and the host's `presence` is its only caller;
   `check_request` is unchanged, so `3E` stays refused to every other path and job kind on a
   read-only link, and on the simulator permission it is allowed as before. This realizes the
   exception ADR-327 makes to ADR-235 item 8, which keeps the rest of the item.
4. **`answers` in the exchange entry.** The presence exchange is one entry with its collected
   results as `answers` items (response identifier, bytes, VCI and agent times, outcome); its
   `outcome` is that of its first answer, `no_response` when none. An item is `undetermined`
   and says that the answer rows do not classify it yet, and its response time is not computed:
   the classification rows of design 8.11 and their time placement (ADR-361) are a separate
   step. The entry's `rejected` list stays empty, the rejected results being among `answers`.
   Its `target` is the functional identifier and a response identifier of 0, which no history
   row keys an answer on.
5. **Halving trigger.** The agent halves the receive table (ADR-395 item 6) when the worker
   answered with a refusal at `SetUniqueRespIdTable` or `ConnectComLogicalLink`: a status the
   worker gave that is not a lost path and not a loss of the connection, and whose link was closed again. A deadline that passed
   at either step, a failed connection and a link refused as lost are never a refusal. The halves
   go to the front of the group's queue in declaration order, the first half the larger, each
   with a link and a request of its own, one link open at a time; a single pair still refused
   fails the job in the worker's error. A link that fails to open for any other reason fails the
   job as a run does, except on a lost path (item 6).
6. **Loss rule.** When the path to the VCI is lost during a part (the VCI is lost, the worker
   exits, the event stream ends, the worker does not answer) or while its link opens, the link
   is opened again by ADR-393 and ADR-394 under the run's bounds (`JobLimits::reopen_poll`,
   `vci_return_wait`, `max_link_reopens`, counted over the whole run), and the part's request is
   sent again, as many times as `JobLimits::max_primitive_repeats` allows (ADR-392), with the
   origin `repeat`. A VCI lost while the link opens (the worker's refusal naming the loss,
   ADR-294) is a lost path too. Before each link opens the run checks for a cancel and for
   guards marked by a link that did not close (ADR-258): a refusal whose link did not close
   is not halved, and no further link opens. A worker that exited between two parts is logged
   (`worker_exit`, `worker_restart`) as ADR-394 has it. TesterPresent with sub-function zero changes nothing on the ECU, so a
   resend needs no idempotency check. Lost events, which end the primitive but not the link,
   and a start refused for a reason that passes, are resent on the same link within the same
   bound. The answers received before the failure count and are not discarded by the resend. A
   part whose bounds run out ends the presence phase: the report keeps what was heard and sets
   `presence_interrupted`; a cancel ends the run as cancelled; any other failure of the request
   fails the job.
7. **The run.** `run_identification` takes the per-VCI lock only (`JobGuards::take_vci_only`),
   sends the presence requests of the plan's groups one link at a time, and is the only writer
   of the report's presence section: the answers as bus, response identifier and bytes, the
   number of presence links opened, the refused tables and the interrupted flag. A bus with no
   presence group gets no request and no link, and the report names it. The report serializes to
   JSON (hashes and bytes as lowercase hex). The runner's open, log, host, settle and close shell,
   and the loop that opens a link again within the bounds, are shared with `run_program`, which
   behaves as before.
8. **Not decided here.** The J2534 worker today logs and skips a refused table entry instead of
   failing the connect, so a halving against `sim-vci` waits for that change in the worker, which
   is the next step; the halving is tested here against a scripted worker. The communication log's
   header still names the job kind a procedure, until the identification job kind exists.

## Consequences

- The report's presence section tells the next step (the VIN read and the ECU reads) which
  endpoints answered, for ordering and not for dropping.
- A receive table the J2534 worker accepts partly cannot be halved yet; the agent sees no refusal.
- The `answers` items carry no classification of their own, so a presence exchange with answers
  is classified by the last row of design 8.11 until the answer rows exist.
- `presence` returns the answers read with the failure that ended the window, so a resend never
  loses them; the host's other primitives are unchanged.
