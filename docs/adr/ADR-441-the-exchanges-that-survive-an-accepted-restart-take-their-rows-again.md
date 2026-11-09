# ADR-441: The Exchanges That Survive an Accepted Restart Take Their Rows Again

**Date:** 2026-11-09
**Status:** Accepted (Consequences' bullet on the restart's uncounted matched answers closed by decision 7)
**Affects:** `agent` (`comm_log/exchange.rs` `LinkState::record`, `LinkState::reset_after`,
`LinkState::refresh_kept_beyond_window`, `LinkState::refresh_window_survivors`,
`LinkState::refresh_row_owners`, `LinkState::free_owner_answer`), design 8.11 "Settlement"; ADR-368, ADR-435 decision 6,
ADR-439 decisions 1 and 3 and Consequences

## Context

An accepted ECUReset or session change ends the owed requests sent before the earliest request
its answer could belong to (`earliest`, ADR-346 item 6, ADR-369). `LinkState::record` (in time)
and `LinkState::reset_after` (read late) drop those requests' rows and kept records. An owed
exchange between `earliest` and the accepting exchange survives, but a row it holds only
because a dropped owner could own its final answer there (ADR-435 decision 6) stayed, in the
window and beyond it. ADR-439 refreshed the kept exchanges beyond the window only after the
accepting exchange, and the window's reclassification only reaches the exchanges after the
settling one, so the survivors were visited by neither. The stale row made the survivor an owner
of the later exchanges' answers and counted among a late response's owners, against the rule
that rows follow owners as they are now (ADR-435 decision 6, ADR-439 decision 1).

## Decision

1. **A survivor is visited with the exchanges after the accepting one.** Both paths take the
   rows of every owed exchange with `earliest <= seq < r` again from the owners that are left,
   where `r` is the accepting exchange (the entry in `record`, the late-accepted exchange in
   `reset_after`). The visit is the one ADR-439 decision 2 describes, from the same helper
   (`LinkState::refresh_row_owners`, the loop body of `refresh_kept_beyond_window` taken out so
   both passes share it): an answer no owner can own confirms the send, a final response placed
   alone releases the exchange with its record (a non-collecting exchange with a final response,
   on a physical link or on a table link, which is owed on its own identifier alone
   (`Evidence::owed_ids`), whatever its owners), otherwise the rows lose the identifiers they
   rested on. Rows that need no owner stay: an identifier with a pending open (ADR-436), the
   whole table of a rejected end (ADR-437), and a row an owner that is left (at or after
   `earliest`) could still own.
2. **Two passes, in `seq` order.** `refresh_kept_beyond_window` takes a range of `seq` and is
   called for `earliest..r`; `refresh_window_survivors` walks the window for owed exchanges in
   the same range. A kept exchange is read from its record; an owed exchange with no kept record
   (a non-collecting exchange with a final response whose answer had an owner is owed but not
   kept) is read from its window entry, its `sent` and matched flag as `reclassify_one`'s old
   entry gives them. The exchanges beyond the window come before the window's, which come before
   `r`, so the visit keeps ADR-439 decision 1's order and a chain (a survivor whose row rests
   on an earlier survivor's dropped row) loses its rows in one visit. An unkept exchange beyond
   the window has no evidence left and is not visited (Consequences).
3. **Rows only; no counting, no reclassification.** The survivors are visited with
   `Place::BeyondRows`. Their matched answers stay uncounted, since the accepting exchange
   clears the matched answers (`matched.clear()`, the epoch shift of `reset_after`) and an
   answer from before it counts for nothing after it. Their classification is not derived
   again (ADR-368, ADR-439 decision 3): a survivor may have been answered by a dropped owner
   before the restart, and the restart proves only that no owner before `earliest` answers after
   `r`. The window entry of a survivor stands as written.
4. **Order within each path.** In `record` the passes run after the owed rows and records
   before `earliest` are dropped and before the window is cleared; no exchange after `r` is
   logged yet, so the answers they free are discarded. In `reset_after` they run before the
   existing visit of the kept exchanges after `r`, and the answers they free go to the same
   worklist (`freed_answers`) the exchanges after `r` consult (`frees_owner`), so an exchange
   logged after `r` that took a survivor as an owner through a dropped row loses it too.
5. **Why dropping the row is right.** A late answer to a survivor `S` on an identifier can arrive
   after `r` only if `S` was sent after the restart, so the acceptance was an earlier owner's;
   then no owner before `earliest`, which was sent before the restart, can have answered `S`,
   and `S`'s recorded final answer there was its own. If `S` was sent before the restart,
   nothing to `S` arrives after `r`. Either way the row `S` held only through a dropped owner
   is moot. A survivor that an owner left in the table could own keeps the row.
6. **A corner stays.** The row of `r` itself is not refreshed. If `r`'s row on an identifier
   (from its step-3 owners, ADR-435) rests on a survivor's row that the pass dropped, it stays:
   that needs the survivor's final answer on the identifier to be negative and the dropped owner
   to share the survivor's service but not its sub-function, so the dropped owner is not an
   owner of `r`. If `r` was answered by the survivor's request, `r` was sent after the
   restart and its own answer may still come; refreshing `r` from the live rows would drop a row
   that is right.
7. **A kept exchange after `r` counts its matched answer in the new epoch.** An owed exchange
   beyond the window with a `seq` after `r` was sent after the restart, so its answer belongs to
   the new epoch (`reset_after` shifts its `clears.matched` with the window's later exchanges)
   and, unlike a survivor's, can count; the recount from the window's `matched_at` alone cannot
   find it. `reset_after` therefore adds two kinds of answer to what the window's later
   exchanges (`answered_same_match`) and the link's matched count (`matched`) take from the
   window:
   - an exchange the visit releases (`Place::BeyondReleased`: rows and record as `BeyondRows`)
     counts its matched answer when it is placed alone or loses its last row, also when it
     counted at its record, since it is no longer kept and no later visit can count it;
   - an exchange that stays kept counts when its record notes a matched answer
     (`Kept::matched`) and no link error its own entry saw came after the answer (the record's
     note ignores that, since `record` keeps the answer's earlier epoch for it). A late link
     error read later ends a kept record's answer as it ends a window exchange's: `error_after`
     clears `Kept::matched` of a record after the error's place whose stored answer time is not
     shown after the error (ADR-385), through the step it takes for the window's exchanges.
     The recount does not test that time again, since it would then also drop the record for
     exchanges before the error, which the error does not end it for; one that did not count
     yet is counted by the settlement that follows (`Place::Beyond`), once, since that sets
     `Kept::matched`.

   Both kinds are counted on the unusable path too (a positive response the agent cannot use,
   `accepting` false): nothing is released then, so only the kept records are read, and a
   later settlement counts the rest.

   A released record is gone before the kept ones are read, so none is counted twice. The
   survivors (`seq` before `r`) never count: their epoch is before the restart. This closes the
   gap ADR-439 decision 3 recorded; it changes no other decision.

## Consequences

- ADR-439 decision 1's "later than the settled exchange" no longer holds for an accepted
  restart, and decision 3's sentence that the exchanges beyond the window lose the owners
  before `earliest` after an accepted restart extends to the survivors; ADR-439's Consequences
  bullet that an accepting restart still drops owners without refreshing the surviving
  exchanges is closed. ADR-368 stands for
  the window's reclassification: the window's exchanges before an accepted restart are
  refreshed rows-only before they leave the window. ADR-435 decision 6 holds for the survivors
  too.
- An owed exchange with no kept record that has left the window keeps a stale row (the case
  ADR-439's last Consequences bullet describes), also when a restart is what let its owner go;
  it stays open as a backlog item.
- The matched answers of the survivors stay uncounted (decision 3), as they are from before the
  restart.
