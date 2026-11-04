# ADR-416: A Timed-Out Start Leaves a Tagged Stray That the Host Finds Again by Its Tag

**Date:** 2026-11-04
**Status:** Accepted
**Affects:** `agent` host (`host.rs` `WorkerHost::send_recv_async`, `settle_at_end`, `Stray`,
`resolve_tag`), communication log (`comm_log.rs` `primitive_ended`, `comm_log/exchange.rs`
`settle_primitive_ended`); design 8.11 "Settlement", `primitive_ended`; ADR-204; ADR-358;
ADR-367; ADR-372; ADR-386; ADR-389; ADR-393; ADR-394; ADR-397

## Context

A `StartComPrimitive` the host stops waiting for (`HostError::WorkerUnresponsive`) may have
started a primitive all the same: the worker answers late, or never, while the primitive runs.
The host knows no handle for it, so it records no stray primitive. The next request does not
cancel it first, the link stays held by a primitive nobody settles, and its items are read as
items of no primitive. ADR-389 recorded the gap in its Consequences and left it open. The same
holds for a start the worker answers with success and no handle.

Both workers echo the `cop_tag` of a start on every event item of the primitive, as soon as the
primitive has a handle (ADR-204). Every item that carries a handle carries the tag, so the first
item the host reads after the start names the handle, whatever kind of item it is.

## Decision

The change is in the agent's host alone; no worker changes (design principle 6: the workers
already absorb their APIs' differences into one tagged event stream).

1. **Every start carries a tag.** The host gives each start an 8-byte big-endian counter value,
   unique within the host. The counter is per `WorkerHost` and is never reset: a reopened link
   keeps counting.
2. **A start that got no handle leaves a tagged stray.** A start the worker did not answer
   (`WorkerUnresponsive("StartComPrimitive")`) or answered with success and no handle (the
   `Setup` error) leaves a stray that has its tag and no handle. It has the deadline of its
   exchange and `cancel_failed` false. After the exchange is logged, the stray's `seq` and
   `exchange` are the exchange's entry, so a worker's exit names it and its end can settle it.
   The stray carries its progress (the items read for its end) from one wait to the next, so
   items read before a failed cancel reach the eventual entry.
3. **One function finds the handle.** The host reads the link's event stream until an item
   carrying the tag names the handle. Both waits use it (ADR-397: shared by extraction):
   - **Before the next request**, bounded by one unary call's time. A handle found: the existing
     cancel-first path runs. None found: the request is refused with `HostError::LinkHeld` (the
     entry stays `not_sent`, cause `link_held`, `system`), the stray stays tag-only, and the next
     request waits again.
   - **At job end** (`settle_at_end`), bounded as ADR-358 bounds the wait: the stray's deadline
     or now, whichever is later, plus the job-end wait. A handle found: the wait goes on as it
     did for a stray with a handle, from that read. None found: nothing is cancelled and nothing
     is written for the exchange, since its end is unknown; teardown proceeds.
4. **The exchange of the start is settled by the primitive's end.** The log keeps an exchange
   whose end is `unknown` with the cause `worker_not_answering` for its `primitive_ended`
   entry, as it keeps one the agent's deadline ended. The `system` fault that exchange was
   classified with stands: the entry takes the more severe of it and the classification of the
   end read, and restates `sent` from all the evidence.
5. **Hand-offs between the two waits.** A lost-events notice does not end the wait for the tag,
   since every later item of the primitive carries it; after a handle is known the earlier rule
   applies. Items read before the tagged one, results and errors of other primitives or of
   none, are written as unattached at once when no handle is found, and are the entry's reads
   in read order when one is.

Decisions on the edges:

- (a) If the item that gives the handle is the primitive's end (finished or cancelled status, or
  a lost VCI bound to it), no stray remains and no cancel is sent; its `primitive_ended` is
  written at once with that end.
- (b) A lost-events notice does not end the tag wait (item 5).
- (c) If the event stream ends or fails during the wait before a request, the stray is dropped
  (its end cannot be read, as `worker_gone` drops it), its reads are written as unattached, and
  the request goes on to its start, whose own failure is judged as ADR-389 judges it. It is
  neither `LinkHeld` nor `EventStreamEnded` for the wait.
- (d) The start answered with no handle is treated as the timeout (a tag-only stray).
- (e) The job-end bound is ADR-358's.
- (f) A lost VCI before a handle is known, before a request or at job end, drops the stray and
  writes nothing for the exchange; the request then proceeds, and the start is refused after
  the loss as before.
- (g) Items of other primitives read while no handle is known go to the log as unattached at
  once; once a handle is known they are the settling entry's reads (ADR-386).
- (h) The stray carries its progress across waits (item 2).
- (i) The refused request's entry is as ADR-389 gives it; no new cause or field.

## Consequences

- The repeat decision is unchanged (ADR-389 item 1, ADR-393 item 4): a timed-out start is still
  a request that may have reached the ECU.
- A hung worker's reopen waits up to ADR-358's bound longer, since the job-end wait now also
  runs for a stray whose handle is not yet known; a worker whose process has exited is found
  first and drops the stray without the wait (ADR-394).
- A library that gives no executing status can send its first item of the primitive after the
  unary wait, so one request may be refused twice before the stray is found.
- A primitive whose handle is never seen is never cancelled by the host; the link stays held
  until the job ends, by the choice of a refusal over a guess.
- This closes the last Consequences bullet of ADR-389.
