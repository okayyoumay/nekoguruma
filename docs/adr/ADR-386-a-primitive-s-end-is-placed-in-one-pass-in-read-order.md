# ADR-386: A Primitive's End Is Placed in One Pass, in Read Order

**Date:** 2026-10-30
**Status:** Accepted
**Affects:** `agent` host (`host.rs` `WorkerHost::settle_at_end`, `Progress`, `Settling::read`,
`log_primitive_end`), communication log (`comm_log.rs` `PrimitiveEnded`, `EndRead`, `EndItem`,
`CommLog::primitive_ended`, `CommLog::late_link_errors`, `Inner::late_link_error`,
`Inner::refresh_loss`; `comm_log/exchange.rs` `LinkState::error_in`); design 8.11 "Order",
"Link error", `primitive_ended`, `late_item`, "Settlement"; ADR-358 item 5; ADR-366 item 3;
ADR-367; ADR-372 Consequences; ADR-373 item 2

## Context

While the agent waits at job end for a primitive it stopped waiting for (ADR-366), and before
the link's next request (ADR-367), the host reads that primitive's items and the link's other
items. Three gaps came from how that end reached the log:

- At job end the host dropped link errors of no primitive and of earlier primitives, so they
  never reached the log (ADR-372 Consequences).
- The log placed the end's times when it wrote the entry, nearest each clock's latest time,
  placed indications more than once, and replaced the exchange's own VCI times with the end's,
  so the exchange lost the span a later error is placed against.
- A `vci_loss` record was not rewritten when a later settling entry reclassified an exchange it
  lists.

A first attempt to pass the job end's link errors placed its times piecemeal, and review found
one ordering case after another: errors before or after the end, times placed twice, ties in
one microsecond, and which exchange stands in for the exchange in flight.

## Decision

1. **The host hands one list in read order.** For each primitive's end, the host passes every
   item it read after the exchange's entry: the primitive's own (pendings, its response with
   the times it carries, a receive timeout, its errors, a transmit indication) and items of no
   primitive or of an earlier one. Each item comes with the agent's clocks at its read, and its
   place in the list is its read ordinal. The job end and the next request's wait build the
   list the same way. Status items of other primitives are still skipped.

2. **The log places each item once, in that order.** Under one lock, `primitive_ended` places
   every item at its read, in list order, within the exchange's reach (its own items came after
   the exchange began). The times a response carries are anchored before its end. Items that
   share a microsecond keep read order; VCI order is not consulted (the maintainer's choice,
   2026-10-30). Epochs the pass opens are written before any entry. Nothing is placed twice, and
   nothing at the end is placed nearest a clock's latest time.

3. **The exchange keeps its own times.** The entry's VCI times are the exchange's own, overlaid
   with the end's receive end, timeout and error. A transmit done or start from the end replaces
   the exchange's only where it is the better reading.

4. **Order decides "before the answer".** A link error the exchange holds counts as before its
   answer only when its read ordinal is below the answer's (the maintainer's choice,
   2026-10-30). This replaces the rule that an error in no known order ended the answer.

5. **Every link error read meanwhile goes to `late_link_error`, after the entry.** In read
   order, each is its own `late_item` (the maintainer's choice, 2026-10-30):
   - one of no primitive is the stray exchange's own, as the exchange in flight would take it,
     when it is at or after that exchange's earliest placed start on its clock and epoch, or
     cannot be set against any of its starts; its before-the-answer flag follows item 4;
   - otherwise, and for one of an earlier primitive, it is placed by its time (ADR-372);
   - an error bound to the primitive with no end written (a cancel) is the exchange's own;
   - one that settles nothing is `unattached`.

   A job end with no entry to write (no exchange for the primitive, an invalid handle, or a
   failed cancel) places its link errors the same way, with no owner.

6. **A loss record stays current.** When any settling entry (`primitive_ended`, `late_response`,
   `late_item`, `late_link_error`) changes the classification of an exchange a `vci_loss`
   record lists, the record is rewritten and synced after it (extends ADR-373 item 2). The
   maintainer chose one helper for every writer (2026-10-30).

The order of lines is: the epochs, `primitive_ended`, its `late_item` and `unattached` lines,
then `vci_loss`.

## Consequences

- No epoch opens from placing an end's times at write time or twice, and an exchange that a
  primitive's end settles keeps the span later errors are placed against.
- A link error read at job end reaches the log, and can raise the stray's exchange after its
  `primitive_ended`; the loss record follows.
- An error of an earlier primitive read at job end is placed against the settled exchanges, not
  taken by an exchange in flight, since none is.
- Indications read before a cancelled end, and indications of an earlier primitive, are still
  not logged (backlog).
- At job end the "written since the last read" rule (ADR-372 item 4) names no exchange, so an
  error of an earlier primitive that cannot be placed there is `unattached`.
