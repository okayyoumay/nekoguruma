# ADR-429: A Collecting Exchange Ends With Its First Final Answer and Is Owed Once per Identifier With an Open Pending

**Date:** 2026-11-07
**Status:** Accepted (item 3's renewal at `primitive_ended` and release exception also apply to an exchange left owed that the VCI ended, by its entry or by a receive timeout its `primitive_ended` read, ADR-431; decision 2's fallback on the link's identifier superseded by ADR-432, and the Consequences' deferred bullets on identifier 0 and on `Kept::answered` closed; decision 3's whole settlement superseded by ADR-433, which settles per identifier; decision 2's "only an open pending owes an identifier once an answer exists" superseded by ADR-435 on a link with a receive table; decision 3's late final answer, for an exchange a pending is open on, settles it whatever its end, ADR-436)
**Affects:** `agent` (`comm_log/exchange.rs` `collecting_end`, `Evidence::open_pending_ids`,
`LinkEffect`, `LinkState::record`, the owner lookups; `comm_log.rs` `primitive_ended`,
`late_item`; `host.rs`); design 8.11 "Owed request", `answers`; ADR-396 item 4, ADR-400,
ADR-425 decision 5, ADR-426

## Context

`collecting_end` (ADR-396 item 4) gave a collecting request the end of its first answer. A
pending the window did not follow with a final answer is such an answer, so a lone pending made
the end a negative one. `Evidence::final_response()` reads the end, so the exchange then counted
as having a final response, was not owed, and the ECU's later final answer on that identifier was
`unattached`. That contradicts ADR-400, which reads a pending as no final response, and it left
ADR-426's late-item judgement without an owner for such an exchange.

An exchange is owed under the response identifier of its link, and a functional link's is 0
(`LinkConfig::functional`). A late item carries the ECU's own identifier (for example 0x7E8), so
even an exchange that stayed owed could never own it.

## Decision

1. **The end is the first standing answer that is not a pending, else no response.**
   `collecting_end` takes the request's expected answers to tell a pending. The host's end and
   the log's restatement at `primitive_ended` use the one function. A lone pending stays in the
   entry's `pending`, `pending_ids` and `answers`.
2. **A collecting exchange is owed once per response identifier whose last standing answer is a
   pending** (`Evidence::open_pending_ids`; `LinkState::record` pushes one owed row for each). A
   final answer of another identifier does not end it. With no such identifier it is owed as
   before: on its link's identifier, or not at all when a final response is placed with it.
3. **The first late final answer on any of those identifiers settles the exchange**, removing all
   its rows. Settling per identifier (the exchange staying owed on the others) is not decided
   here. The owner lookups count distinct exchanges, since two rows can share one kept exchange.
   At `primitive_ended` the exchange is settled only when no identifier is left open; when it
   stays owed, its rows are set again from the answers read after the deadline and its kept
   evidence is replaced with them, so a later late item is judged against them. One condition
   (a final response and no open identifier) is shared by `record`, `primitive_ended` and the
   answer a settling entry adds (`settled_answer`, which a late response always settles);
   `record` and the answers freed for later exchanges also require that no owed request could
   own the answer (`placed_alone`), a part `primitive_ended` does not apply, as before. The
   host's release of the kept record after a `primitive_ended` (ADR-367 item 3) does not apply
   to an exchange left owed per an open identifier, and a renewed exchange keeps the send and
   the transmit that end read.
4. **Amendments.** ADR-396 item 4's outcome for a lone pending is `no_response`, not the
   pending's negative result. ADR-425 decision 5's "a lone pending read after the deadline is a
   negative end" becomes no response. ADR-426's open bullets (a lone pending counted as a final
   response, so no late item had an owner) are closed. A late start or pending is judged on the
   identifier it carries, not on the link's, so a functional link's judgement no longer reads
   identifier 0. The window a late response, start or pending is judged from (`late_window`)
   takes the identifier of the item. An exchange in flight that collects awaits answers on any
   identifier, so an unbound answer read during a presence resend could be the resend's own and
   settles no earlier exchange.

## Consequences

- The entry's outcome is `no_response` where it said `negative` (0x78) for a lone pending; the
  pending is still listed in `answers` and `pending`. A mixed window (a pending on one
  identifier, a final answer on another) ends with the final answer and is owed on the pending's
  identifier.
- A presence resend's answer on an identifier that is open on an earlier exchange now has an
  owner, so it is `undetermined` as a possible late final answer (ADR-400 row C). This is
  intended.
- Deferred, kept as backlog items: settling per identifier; a pending whose identifier is
  unnamed (identifier 0) and an exchange with no answer, which stay owed on identifier 0 only,
  so a later final answer on a named identifier is not attached; `Kept::answered` restating the
  answers of a settled exchange (closed: `Kept::answered` of a collecting exchange restates the late
  final answer through the same generic fold as decision 4 of ADR-425, keyed as the host keys an
  answer, so an unnamed final answer folds an unnamed pending; it extends that fold in one
  case, a final answer that names no identifier also folds the pending of the one identifier
  the exchange is owed on, and none when several are open. The window's end stays judged by
  the kept exchange's own timing, or as at the limit when a late start was judged so (design
  8.11 `late_item`); only the appended answer is taken as within the limit).
