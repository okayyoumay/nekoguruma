# ADR-434: A Deadline-Ended Collecting Entry With Answers Takes Its Answers' Classifications

**Date:** 2026-11-08
**Status:** Accepted
**Affects:** `agent` (`comm_log/exchange.rs` `rows`, `silence_open`, `collecting_row`;
`comm_log.rs` tests); design 8.11 "Exchange entry" `vci_did_not_end` rows and `answers`,
`crates/agent/docs/ngr-agent.md`; ADR-400 decision 4(B), ADR-426

## Context

ADR-400 decision 4(B) says a collecting entry with answers takes the most severe of its answers'
classifications, at least `undetermined` while a reading stays open. The agent's own deadline can
end a collecting window before the VCI does (`vci_did_not_end`), with answers already read. The
`rows()` order put the generic `vci_did_not_end` row ahead of the collecting row, so such an entry
was `undetermined` whatever its answers said: an answer whose own pending came late in its window
(`system`) was hidden, and the reason did not say how many answers came. Only a collecting entry
past the VCI's own limits (the `system` `vci_did_not_end` row) escaped it.

## Decision

1. A `vci_did_not_end` entry of a collecting request with at least one answer, and with its VCI
   limits not shown past, takes the collecting row (`collecting_row`): the most severe of the
   answers' classifications, at least `undetermined`, with the same reason as any collecting entry
   (answer count, identifiers, the answer that set it, the readings left open).
2. The row that puts the VCI's limits past by its own clock stays first and `system`, so such an
   entry keeps its reason and an answer count is not added to it. The merged-window late-pending
   row stays before both.
3. The deadline is one of the open readings (`silence_open`): the agent stopped waiting before
   the VCI ended the window, so whether the VCI would have ended it at its limit is not shown. It
   replaces the "end not judged on one clock" phrase for this end and keeps the absorbed-pending
   phrase (H4).
4. A deadline-ended collecting entry with no answers keeps the generic `vci_did_not_end` row,
   `undetermined`; so does a request that is not collecting. A collecting `vci_did_not_end` entry
   whose window the VCI ended is not reachable, and the VCI-ended rows are unchanged.
5. A link error of the exchange's own link still takes the most severe of this row and the
   `link_error` rows; one on another link of the channel is named (H3) and makes the entry at
   least `undetermined`, as in ADR-400.

## Consequences

- Design 8.11 splits the generic `vci_did_not_end` row and the `answers` row names the window the
  agent's deadline ended.
- A collecting entry's classification can be `system` or `vehicle` for an answer while the
  deadline is open; a settling entry (`primitive_ended`) still reclassifies it as before.
- An existing test that pinned the generic reason for an in-time answer now pins the collecting
  row's, and a fixture answer that came late against P2 is `system` by its own verdict.
