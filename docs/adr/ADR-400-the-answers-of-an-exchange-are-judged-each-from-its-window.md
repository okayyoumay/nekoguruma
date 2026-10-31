# ADR-400: The Answers of an Exchange Are Judged Each From Its Own Window

**Date:** 2026-10-31
**Status:** Accepted
**Affects:** `agent` (`host`, `comm_log` `exchange`, `timing`, `comm_log`), `sim-ecu`
(`Fault::RepeatResponse`), `sim-vci`, `j2534-0404-service` tests; design 8.11 "Exchange entry"
(`answers`, `timing`) and "Classification"; ADR-327 item 2, ADR-361, ADR-376, ADR-396 item 4

## Context

ADR-396 item 4 lists the answers of the functional presence request as `answers` items, each
`undetermined`, with no response time, and leaves the classification rows of design 8.11 for a
later step. The rows (an entry's classification from its answers, the limit in force of a
window that restarts at each answer, and a request meant for one ECU that got more than one
answer) need three decisions the design table leaves open: which point each answer's window runs
from, what the entry's own `timing` then means, and how the entry marks such an exchange.

The host also had a latent fault: in collecting mode `Progress::on_event` overwrote the
response and its times on every answer, while the entry took the first answer's end.

## Decision

1. **Each answer is judged from the end of the answer before it.** A window starts at the
   transmit and restarts at the end of each final answer, whichever identifier it came on, as
   the receiving side's window does for a request that collects every answer (ISO 14229-2:2021
   10.2.1; ISO 22900-2:2022 8.2.6.2.2). A response pending extends the limit in force, and it is
   kept per response identifier (ISO 14229-2:2021 10.2.3): an identifier's final answer is
   judged from its own pendings since its previous final answer, with the limit they put in
   force, and another ECU's final answer does not clear them. The chain of pendings starts at the
   end of the latest final answer, of any identifier, read before its first pending (the window
   that answer restarted); later answers of other identifiers leave the chain whole. The host names the identifier of
   each pending (`Evidence.pending_ids`; one it cannot name counts for every identifier).
   `timing::Times` gains `window_from`, the point the limit in force runs from (`None` is the
   transmit; a point whose place is unknown leaves the answer `not_judged` as `other_clock`).
   The response time and the interval from the latest pending stay measured from the transmit,
   so every answer's `response_ms` is comparable. The first answer's judgement is identical to
   the single-ECU request's. A pending that is the last of its identifier is listed as an
   answer and judged as a request that got a pending and no response, its own pending the last
   of its list, by the one conversion the classification and the log's timing share
   (`Answer::as_exchange`), so a late one is `system` as for a single exchange.
2. **A collecting entry's own `timing` judges the window's end.** With answers, no receive
   timeout comes (ISO 22900-2:2022 8.2.6.2.2): the window ends with the primitive's finished
   status, whose item time is judged like a receive timeout when it is on the clock the answers
   are, against the limit in force from the last answer, or from the pendings of the identifiers
   that never got a final answer. On the J2534 worker the finished item is on the worker clock,
   so there the end is not judged at its limit on one clock, and an answered presence entry is at
   least `undetermined` by the third reading of design 8.11's `answers` row (ADR-376). With no
   answer a receive timeout ends the window as for any request. The entry has no response time:
   each answer's response time and window are in its `answers` item.
3. **The entry is marked only inside the agent.** `Evidence.collecting` tells the rows a
   collecting entry from a single one; the log has no key for it, and a reader tells it by its
   target (the functional identifier) and its origin. The answer list is the raw
   `exchange::Answer`; `ExchangeEntry::build` is the only place that classifies an answer, by
   `Evidence::as_answer` (the exchange that answer alone would be: its end and timing, its own
   pendings, and nothing that belongs to the whole exchange), which `Kept::answered` shares.
4. **The rows.** (A) A collecting entry whose window the VCI ended `timeout_early` or
   `timeout_late` is `system`. (B) A collecting entry with answers, or with none, takes the most
   severe of its answers' classifications; a window end not judged at its limit on one clock, a
   pending the VCI absorbed, and a link error on another link of the channel each make it at
   least `undetermined`; an own-link error is the `link_error` rows' as for any exchange.
   Silence is `none` only for a confirmed send, a forwarding link, an end judged at its limit
   and no link error. The reason says how many answers came on how many identifiers, the answer
   that set the classification and each open reading. (C) A request meant for one ECU that got
   more than one answer is `undetermined` when each extra answer (every one but the first) can
   be given its own owed request out of those its own link history says it could belong to (an
   owed request takes one answer at most), else `vehicle` (a second responder) or the most
   severe of its answers if that is `system` or `undetermined`; its entry's outcome stays the
   first answer's. Each
   answer is classified against its own link history. A collecting entry's answers carry the
   owed requests they could belong to in their own verdicts, and a request meant for one ECU
   weighs them against its extra answers once (`owed_tail`), so that an extra answer no
   remaining owed request can own still shows a second responder.
5. **A request meant for one ECU lists its answers when it got several.** The host keeps every
   answering result, the first stays the primitive's response and its times, and the entry lists
   all of them from the second on. The J2534 worker binds one answer per receive cycle
   (ADR-100 decision 5), so row C is reachable there only through a D-PDU API worker; it is
   covered by host and log unit tests with scripted events.
6. **Collecting mode takes no response times.** The entry's transmit done comes from the first
   answer that carries one, placed before that answer's end.
7. **`sim-ecu` `Fault::RepeatResponse { interval_ms }`** sends the next final response a second
   time, `interval_ms` after the first, so the simulator can answer twice on one identifier;
   `sim-vci` queues the copy after the response.

## Consequences

- The presence entry and its answers carry a classification, a response time, a clock and an
  epoch; the last row of design 8.11 no longer stands in for an answers list.
- A settling entry that re-judges a collecting exchange (`primitive_ended`, a late item) still
  judges its timing from the transmit and keeps the answers' timing it was given; judging it
  from the last answer there is a separate step.
- A pending the host lists as an answer (the last of its identifier, with no final answer after
  it) is classified as a request that got a pending and no response, so it is `undetermined`.
- The primitive's finished item is the one new VCI time a collecting entry reads
  (`VciTimes.finished`, not written to the log).
- A stray primitive's answers after the agent's deadline are not carried into its
  `primitive_ended` entry.
- On the J2534 worker the functional presence primitive gives the log no transmit time, and an
  identification job is given no timing margin, so its answers are placed on their clock and
  `undetermined`, with no response time (`no_transmit_time`, `no_margin`); the rows judge them
  once either is supplied.
