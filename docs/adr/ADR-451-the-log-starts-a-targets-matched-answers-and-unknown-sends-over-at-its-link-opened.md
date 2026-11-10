# ADR-451: The Log Starts a Target's Matched Answers and Unknown Sends Over at Its `link_opened`

**Date:** 2026-11-10
**Status:** Accepted
**Affects:** `agent` (`comm_log.rs` `CommLog::link_opened`; `comm_log/exchange.rs`
`LinkState::reopened`; `host.rs` `WorkerHost::reopen`; `runner.rs` `close_job_link`), design 8.11
"Matched answer", `sent_evidence`; `crates/agent/docs/ngr-agent.md`; ADR-448 Consequences;
ADR-393

## Context

Design 8.11 counts a matched answer, and lets an ended exchange own a late transmit indication,
only on the same opened link. `LinkState::reopened` implements both ends, but nothing in
production called it, while a run writes `link_opened` again after a lost VCI or a worker's end
(ADR-393, ADR-394), and an identification run writes one for every part: each presence part, the
VIN read and each ECU read group opens its own link, and the same target (a functional address,
a read endpoint) recurs. So an answer from an earlier opening supported `vehicle`, which is not
conservative, and an unknown send from an earlier opening kept every later unbound transmit
indication `unattached` or took one as its own. ADR-448 made the call safe.

## Decision

1. `CommLog::link_opened` calls `LinkState::reopened` on the state of the target its event names,
   after its line is written. `vci_connected`, `worker_restart` and the host do not: each
   reconnect and restart the run logs is followed by a `link_opened`. The state is found by the
   event's own endpoint key (link, transmit and receive identifier), which is the key the host's
   exchanges carry.
2. A target's first `link_opened` finds no state and changes nothing, so there is no flag: a
   state begins with the target's first exchange, and the host writes `link_opened` before it
   sends anything.
3. Only what `reopened` resets is reset: the matched answers, with the epoch they count in, and
   the set of unknown sends. Owed requests, kept records, the window and the counts `answered`
   and `answered_since_error` stay, and no line is written for an earlier exchange.
4. The caller writes what the old link left (a stray primitive's end, the drained unbound items,
   the tester-present errors) before `link_opened`, so each is placed against the old opening.
   `WorkerHost::reopen` and, in an identification run, `close_job_link` already do. The order
   decides the owner: a transmit indication of the old link placed before the reopening is the
   late item of the old opening's unknown send, and placed after it, it is no one's.
5. An answer placed late with an exchange from before the reopening settles it as before and
   counts in the opening it was sent on: for the window's exchanges of that opening, not for
   later ones and not in the live count.
6. What owed requests and kept records own across a reopening stays undecided (ADR-448 decision
   4).
7. The reset applies to every `link_opened`, identification runs included: it means a new
   opening of that target on every run, not only after ADR-393's reopening. The code does not
   tell the two apart, and design 8.11 asks for the start-over for both.

The maintainer decided on 2026-11-10: the call sits in `link_opened` (not `vci_connected`, not
the host), after the callers have written the old link's items, resets only what `reopened`
resets, leaves owed and kept as they are, and applies to identification runs.

## Consequences

- No log format change. A match counted before a target's latest `link_opened` no longer counts
  after it, so a silent request whose only matched answer predates it is `undetermined` (H2
  open) where it was `vehicle`, and a request sent with its positive response suppressed, silent
  and judged at its timeout, is `undetermined` where it was `none`. A transmit indication read
  unbound after a reopening attaches to the exchange it belongs to where an unknown send from
  before kept it `unattached`. These hold in ADR-393 runs and in identification runs.
- The `none` to `undetermined` change reaches restarts. Take a session change answered with a
  refusal, the VCI lost (ADR-393), the link opened again, and then a silent
  DiagnosticSessionControl or ECUReset of the same match sent with its positive response
  suppressed (for example `10 83`, or `11 81`) and judged at its timeout. It was `none`, which
  ends the owed requests. It now has no matched answer, so it is `undetermined`, and the log
  takes it for an unsure restart: the matched answers end, the owed requests stay standing (design
  8.11 "Matched answer", "Owed request"). A later answer such an owed request could own then
  counts in `owed_matching` and is `undetermined`.
- An exchange the lost VCI ended with its send unknown stays unknown and owed. An indication of
  the new link no longer confirms it.
- Open, with the opened-link marker on owed requests: an unbound start after a reopening stays
  `unattached`; a `primitive_ended` written after a reopening for an exchange from before it
  would count its answer in the new opening (the host does not write that order); an unplaced
  link error after a reopening can name exchanges of the old opening through the link's last
  read.
- A state keyed by a role is not found until `link_opened` names the role; the host binds no
  role yet.
- With the call in place, the module-wide `expect(dead_code)` on `comm_log::exchange` no longer
  covers it; it is replaced by expectations on the items that still have no production use.
