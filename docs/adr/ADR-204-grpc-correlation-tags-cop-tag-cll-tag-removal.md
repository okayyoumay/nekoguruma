# ADR-204: gRPC Event-Correlation Tags — Add `cop_tag` (opaque bytes), Remove `cll_tag`, `api_tag` Stays Internal

**Date:** 2026-08-31
**Status:** Accepted
**Affects:** `vci-service-interface` (proto, generated bindings), `iso22900-service` (rpc_primitive,
rpc_link, service/events), `j2534-0404-service` (rpc_primitive, rpc_link, service/events,
docs/implementation-notes.md), `iso22900-mock`, `j2534-0404-mock`, `docs/rpc-api-guide.md`. Does
**not** affect `j2534-0500-service` (every RPC there is an unimplemented stub that ignores its
request; see Consequences). ADR-178 (applies its exception carve-out), ADR-196/198/200
(each annotated — see each ADR's own Status line)

## Context

ISO 22900-2 defines three application-supplied "tags" — opaque values the D-PDU API never
interprets, only stores and echoes back — as an optional correlation aid for a native, in-process
client: `pAPITag` (§8.4.2.4, set at `PDUConstruct`, distinguishing which loaded D-PDU API library
instance is calling back), `pCllTag` (§8.4.9.4, set at `PDUCreateComLogicalLink`, echoed in the
event callback per §8.4.22.3 whenever the CLL handle is defined), and `pCoPTag` (§8.4.17.4, set at
`PDUStartComPrimitive`, returned in `PDU_EVENT_ITEM` unless the COP handle is undefined). Annex E
(informative) frames all three as a same-process pointer-dereference optimization that avoids an
application-side lookup keyed by handle, and states plainly that the tags are not strictly
necessary for correlation, since callbacks and result items always carry the corresponding
handles.

This workspace's shared gRPC surface (`vci-service-interface/src/proto/service.proto`), consumed
both by `iso22900-service` (a direct D-PDU API passthrough, where the tags map onto real native
parameters) and by `j2534-0404-service`/`j2534-0500-service` (J2534 adapters, whose native API has
no equivalent tag concept at all), exposed only a fragment of this mechanism going into this
decision: `CreateComLogicalLinkRequest.cll_tag` (`service.proto:700`, optional uint64) was accepted
and passed through to the native `pCllTag` parameter in `iso22900-service`
(`rpc_link.rs:30-36`, `iso22900/lib.rs:652-674`), but the corresponding native echo
(`EventNotification.cll_tag`, computed in `iso22900/src/events.rs:38-43`) was never read by
`iso22900-service/src/service/events.rs`, and no `EventItem`/`EventNotification` field existed to
carry it to a gRPC client at all. `j2534-0404-service/src/service/rpc_link.rs` never read
`request.cll_tag` in the first place, since J2534 has no native tag to forward it to. `cop_tag` had
no request field whatsoever; `iso22900-service` hardcoded the native parameter to `0`
(`rpc_primitive.rs:68`). `api_tag` had (and has) no client-facing exposure at all — it is used
purely internally, as the dispatch key for the native-callback trampoline
(`iso22900::ApiTagContext`), an architecture that is always exactly one loaded D-PDU library
instance per `iso22900-service` process, which is the only ambiguity `pAPITag` exists to resolve.

`j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog (P2, citing ADR-196/198/200,
which had each explicitly scoped `cll_tag` out of their own RawMode/ChecksumMode work as "a
different mechanism... tracked in its own existing backlog entry") flagged this half-plumbed
`cll_tag` gap and deferred its resolution to "a design pass" — this ADR is that design pass, for
both `cll_tag` and (since the same event-correlation question covers it) `cop_tag`.

`vci-service-interface/src/proto/service.proto` is under ADR-178's freeze: `service.proto` "does
not grow again for a new protocol capability," with exactly three sanctioned generic channels for
future client-facing capabilities (a ComParam; a hand-packed blob inside the pre-existing
`DataItem.bytearray_data`/`IOBytearray` carrier, reachable only from `IoCtl`; a compound-name
grammar extension of an existing `string` field) — plus an explicit carve-out for "genuinely new
client-visible RPC-level semantics" that none of the three channels can express, routed to their
own `design-advisor` consult rather than pre-decided by ADR-178 itself. ADR-178's own Decision also
established a directly relevant precedent by *removing* a proto field outright
(`ResourceData.channel_index`) rather than only ever growing the schema, accepting the resulting
wire-compatibility hazard for the reason recorded in its Consequences: `vci-service-manager` spawns
every service and every client from one colocated build, so no independently-versioned client
exists that could still carry a schema a field removal would silently break; the accepted trigger
for revisiting that acceptance is the day an independently-released client package is introduced.

Separately, this workspace's own async architecture creates a correlation hazard native,
single-process D-PDU clients are less exposed to. `register_event_callback`
(`iso22900/src/lib.rs:916`) is registered once per `(module, cll)` at CLL-connect time, independent
of any specific `StartComPrimitive` call, so the callback is already live before a given COP
starts. `start_com_primitive` (`iso22900/src/lib.rs:799-827`) mints the COP handle and (depending on
vendor-DLL threading, which the spec's §8.4.17.2 queue-then-execute state machine does not
constrain) may already be producing events for that COP before the native call returns to the Rust
caller — and even once it returns, `rpc_start_com_primitive` still has to unwind through several
async layers (mutex release, response construction, tonic serialization, HTTP/2 frame
transmission) before a gRPC client ever sees `StartComPrimitiveResponse`, while
`run_event_subscription_task` (`iso22900-service/src/service/events.rs:137`) runs as an
independent, always-live task pushing matching events onto the already-open, separate
`SubscribeEvent` HTTP/2 stream the instant a notification arrives — nothing synchronizes "unary
response flushed" ahead of "stream push flushed" across two independent HTTP/2 streams.
`j2534-0404-service` is even more deterministically async here: `rpc_start_com_primitive`
(`rpc_primitive.rs:2728`) enqueues the COP and returns while a separately-spawned poll task
(`spawn_channel_poll_task`/`poll_channel_events`, ADR-021) dequeues, executes it, and emits its
events — by this repo's own design, not vendor-dependent behavior. Since the CoP Queue legitimately
holds multiple concurrently-outstanding COPs per CLL (spec-sanctioned pipelining, not an edge
case), a client that pipelines several `StartComPrimitive` calls on one CLL has no way, from
`cop_handle` alone, to attribute an event that arrives before its own matching response resolves to
one of its own pending calls — until that response resolves, at which point gRPC's own per-call
request/response correlation always closes the ambiguity. A documented client-side pattern
("buffer events by `cop_handle` until the matching Start response arrives, then reconcile") is
therefore *sufficient* for correctness on its own.

Sufficiency for correctness is not the same question as reasonable implementation cost across an
open set of independent gRPC client implementations, in any language, that this multi-client
ecosystem must support. The buffering pattern requires every client to build and keep correct: two
collections (known handles; not-yet-attributed events) reconciled across two execution contexts
(the task awaiting the unary response, the task consuming the event stream) — a small
concurrency-correct producer/consumer join that this repository's own ADRs (e.g. ADR-021, ADR-140)
treat as worth a single, carefully reviewed, server-side implementation, not something to leave to
be re-derived once per client; plus an orphan-eviction policy for a buffered event whose matching
Start call later fails client-side despite server-side success. The race window is narrow and
environment-dependent (fast local mocks rarely expose it), so a client that gets the join wrong
does not fail loudly in development — it fails intermittently in the field, the least discoverable
defect shape available. A tag the client chooses before the call and reads back verbatim off every
event removes this ordering dependency entirely: correct behavior becomes the straightforward
implementation rather than something requiring careful design to get right per client.

## Decision

**1. Add `cop_tag` as an opaque `bytes` field, via ADR-178's own "genuinely new RPC-level
semantics" carve-out.** `StartComPrimitiveRequest` gains `optional bytes cop_tag = 5` (next free
tag); `EventItem` gains `optional bytes cop_tag = 9` (fields 1-8 are taken). This is judged to
qualify for ADR-178's carve-out, not an erosion of the freeze, on a three-part test any future
invocation of the same carve-out should also be held to: (a) the capability is not a J2534-2
protocol capability at all — it is generic RPC-level event correlation, orthogonal to every
protocol ADR-178 was written to bound; (b) all three of ADR-178's sanctioned channels are
structurally incapable of carrying it (a ComParam is CLL-scoped, connect-time-staged state with no
per-call, per-event presence; `IOBytearray` is reachable only from `IoCtl` RPCs, not from
`StartComPrimitive`/`EventItem`; the name-grammar channel has no applicable string field on either
message); (c) declining the capability exports a nontrivial, correctness-relevant burden (the
concurrency join above) to every independent client implementation indefinitely, for a defect shape
that surfaces as field-only intermittent failures rather than a missed convenience.

`bytes` rather than `uint64`: `uint64` would only fully discharge the burden in (c) for clients
whose natural request identity already fits in eight bytes (a counter, a native pointer); a client
whose natural identity is a UUID, a string key, or a small struct would still need its own
"counter → real identity" side-table to use a `uint64` tag — a milder, but real, residue of the
exact bookkeeping this field exists to eliminate. `bytes` lets any client embed its identity
directly, with no side-table regardless of its shape. This also fixes a portability defect an
initial `uint64` design (with `cll_tag`-style pointer-width validation, plus native
`pCoPTag`-forwarding "for trace fidelity") would have introduced: a validity check tied to pointer
width would make wire acceptance target-dependent, rejecting an ordinary 64-bit tag value on
`i686-pc-windows-gnullvm`/`armv5te-unknown-linux-gnueabi` (both first-class targets in this
workspace's bindings matrix) that a 64-bit target would accept. `EventItem.cop_tag`'s authoritative
source is service-side, per-COP state on all three backends uniformly — never a native
`pCoPTag` round-trip — so this dependency is avoided entirely: `iso22900-service` continues passing
native `pCoPTag` as null (`rpc_primitive.rs:68`'s existing behavior, unchanged), and the echo comes
from the same per-COP tracking structure on every backend, including the two J2534 adapters, which
already synthesize `EventItem`s from their own COP bookkeeping (ADR-021) and gain nothing from a
native forwarding path that does not exist for them anyway.

`bytes` rather than `google.protobuf.Any`: `Any`'s type-URL-plus-serialized-message shape solves a
self-description problem this call site does not have — the tag's only reader is the client that
wrote it, which already knows its own encoding. ADR-178 already rejected the structurally similar
`ParamVendorSpecificStruct` (`type_url`/`size_of_entry`/`count_of_entry`/`bytes value`) for a
different call site on exactly this reasoning. `Any` would additionally pull a well-known-types
import into a frozen proto file and add a type-URL string to every echoed event for no client
benefit. Plain `bytes` is this repository's own established idiom for a schema-free opaque value
(`DataItem.bytearray_data`/`IOBytearray`, in ADR-178's own words, "a generic opaque-bytes carrier").

`cop_tag` is echoed on `EventItem` verbatim, iff `cop_handle` is present on that event and a tag
was supplied at `StartComPrimitive` — mirroring `PDU_EVENT_ITEM.pCoPTag`'s own
ignore-if-handle-undefined rule. A tag is never required; its absence leaves `EventItem.cop_tag`
absent. `StartComPrimitive` enforces a documented maximum tag size, rejected with
`invalid_argument` when exceeded: because the tag is echoed on *every* event for a given COP
(periodic/cyclic COPs emit many over their lifetime), an unbounded tag is a per-event amplification
hazard `uint64` could never pose. The exact cap (generous enough for a UUID or a compact key) is an
implementation-brief detail; the cap itself, and its enforcement point, are normative.

**2. Delete `cll_tag`.** `CreateComLogicalLinkRequest.cll_tag` (`service.proto:700`) is removed
outright, with `reserved 5; reserved "cll_tag";` left in its place. No `cll_tag`-equivalent field is
added to any event message. This reopens and reverses this ADR's own initial framing (an earlier
round of this design's review considered keeping `cll_tag` as an inert, documented-as-such input,
which this Decision supersedes): keeping a field with zero observable client-visible effect,
alongside a newly-introduced, fully-functional `cop_tag`, would misrepresent the shape of this
workspace's tag family to any reader of the schema. Three facts justify deletion over a
documentation-only fix: (a) `cll_tag` is a complete end-to-end no-op today for any gRPC
client — the native `pCllTag` value it sets is never read by any of this workspace's own
event-forwarding code, on any backend, so setting it changes nothing a client can ever observe; (b)
unlike `cop_tag`, no correlation race exists for CLL-scoped events to justify one: `cll_handle` is
always already known to the client before it can trigger any action capable of producing a
CLL-scoped event (`CreateComLogicalLink`'s own response delivers the handle; event-callback
registration only happens afterward, at Connect, `iso22900/src/lib.rs:916`), so handle-based
correlation for CLL was always trivially sufficient, with no "server mints an identifier the client
doesn't know yet" window of the kind that motivates `cop_tag`; (c) the wire-compatibility hazard a
field deletion raises is the identical hazard class ADR-178's own Decision already weighed and
accepted for `channel_index` — same colocated single-build deployment fact
(`vci-service-manager` spawns every service and client together), same "no independently-versioned
client exists to break" reasoning, same re-evaluation trigger — and is strictly milder here, since a
stale client's silently-dropped `channel_index` could misroute traffic onto the wrong physical
channel, while a stale client's silently-dropped `cll_tag` changes nothing at all (it was already a
no-op). This Decision restates, rather than reinvents, ADR-178's own accepted reasoning and its
re-evaluation trigger — it does not lower the bar for future field removals beyond this specific,
already-no-op case.

**3. `api_tag` needs no change.** It remains purely internal (`iso22900::ApiTagContext`), with no
client-facing exposure on any proto message, since this architecture's one-D-PDU-library-instance-
per-process invariant means the ambiguity `pAPITag` exists to resolve never arises across the gRPC
boundary.

### Alternatives rejected

- **Handle-only correlation, no `cop_tag` (this ADR's own first-pass recommendation).** Sufficient
  for correctness (gRPC's own per-call response correlation always resolves the ambiguity
  eventually), but exports a real concurrency join to every independent client implementation
  indefinitely, with a defect shape (field-intermittent, development-invisible) worse than the cost
  of the proto exception. Reversed once this cost was weighed explicitly against the freeze-exception
  cost, rather than only against strict necessity.
- **`cop_tag` as `uint64`.** Re-imports a client-side identity side-table for any client whose
  natural identifier isn't 8 bytes; a pointer-width-validated variant (mirroring `cll_tag`'s
  existing check) would also make wire acceptance depend on the serving `iso22900-service`
  binary's target architecture.
- **`cop_tag` as `google.protobuf.Any`.** Solves a self-description problem this call site does not
  have (disqualified by the same reasoning ADR-178 already applied to `ParamVendorSpecificStruct`);
  adds import and per-event wire overhead for no client benefit.
- **Keep `cll_tag` as an inert input, documented as a no-op.** Leaves permanent, misleading
  API-surface debt once a fully-functional `cop_tag` exists alongside it; the field genuinely does
  nothing a client can observe, so there is nothing a "clarified comment" preserves that deletion
  loses.
- **Add a `cll_tag` echo alongside `cop_tag`'s, for symmetry.** No race exists for CLL to justify
  the same freeze exception; would be scope creep against the qualification test in Decision item 1.
- **Route `cop_tag` through one of ADR-178's three sanctioned channels instead of its carve-out.**
  Established as structurally impossible in Decision item 1(b); none of the three channels reach
  the event-response path at all.

## Consequences

- `service.proto` gains two `bytes` fields (`StartComPrimitiveRequest.cop_tag = 5`,
  `EventItem.cop_tag = 9`) and loses one (`CreateComLogicalLinkRequest.cll_tag`, tag 5 and the name
  reserved). Regeneration via `cargo build -p vci-service-interface --features vendored-protoc`
  lands in the same PR as the schema edit, per `vci-service-interface/docs/implementation-notes.md`'s
  existing workflow.
- **This is the first invocation of ADR-178's "genuinely new RPC-level semantics" carve-out.** The
  three-part qualification test in Decision item 1 is the bar any future invocation of the same
  carve-out should be held to, so a later consult cites a precedent, not a vibe. ADR-178's own
  default policy (no proto growth for a *protocol* capability) is otherwise unchanged and is not
  narrowed by this ADR.
- `cop_tag` must be forwarded, stored, and echoed uniformly by `iso22900-service` and
  `j2534-0404-service` — the only two backends that implement `StartComPrimitive`/`EventItem` today.
  `j2534-0500-service` needs no changes at all: every RPC there is an unimplemented stub that
  ignores its request, so it already compiles unchanged against the new proto fields and carries no
  `cop_tag`/`cll_tag` reference anywhere; it picks up the real mechanism automatically once (if
  ever) it grows a real implementation, at no cost to this ADR. `j2534-0404-service` stores the tag
  alongside its existing per-COP tracking state (ADR-021's `primitives` map) with cleanup tied to
  existing COP-removal paths, so no new leak is introduced there. `iso22900-service`'s equivalent
  map (`Iso22900Service.cop_tags`, keyed by `(module, cll, cop)`, since this backend has no
  pre-existing per-COP tracking structure to fold the tag into) requires its own explicit
  reconciliation and bounded-lifetime discipline: every `StartComPrimitive` call reconciles the map
  slot for its own `(module, cll, cop)` key unconditionally (inserting the supplied tag, or removing
  any stale entry when no tag is supplied) so a reused handle can never echo a prior occupant's tag;
  the insert/removal happens while still holding the native-API lock the call already acquired, so
  it happens-before any concurrent `GetEventItem`/`SubscribeEvent` consumer that must acquire the
  same lock first (closing a race an initial implementation pass left open, caught by
  `edge-case-hunter`); and CLL-destroy/module-disconnect cascades sweep any lingering entries for
  their scope, the same way existing subscription cleanup already does, bounding a leak to at most
  the owning CLL's/module's lifetime instead of the life of the process. `iso22900-service` never
  forwards the tag to native `pCoPTag`; that parameter stays null exactly as it is today.
- The previously-identified "multi-orphan ambiguity" residual (concurrent `StartComPrimitive` calls
  on one CLL, some failing client-side after server-side success) dissolves: an orphaned event
  still carries the client's own token, so no separate recovery discipline needs documenting.
- `docs/rpc-api-guide.md` gains tag-based correlation as the recommended pattern for `cop_tag`
  (handle-based correlation via `cop_handle` remains valid and is `cll_tag`'s and `api_tag`'s only
  applicable pattern going forward), the documented `cop_tag` size cap, and a note that
  `CreateComLogicalLinkRequest` no longer accepts a CLL-scoped tag.
- `j2534-0404-service/docs/implementation-notes.md`'s P2 backlog entry deferring the `cll_tag`
  design pass is deleted outright (this ADR is that design pass); the file's line-3061 analogy
  ("the same place `cll_tag`/`cll_create_flag` already sit") is corrected to name `cll_create_flag`
  only, since `cll_tag` no longer exists. ADR-200's Consequences line ("`cll_tag` remains tracked in
  its own, separate backlog entry — unaffected by this ADR") is updated to cite this ADR instead of
  the now-deleted backlog entry. No equivalent backlog entry is added to
  `iso22900-service/docs/implementation-notes.md`: this ADR is the durable record for both services.
- A race-shaped regression test (a mock backend emits a COP status/result event before the client
  consumes the matching `StartComPrimitiveResponse`; the test asserts the echoed `cop_tag` still
  matches) is added alongside the implementation, since this is exactly the failure mode a
  handle-only design would have made undetectable in fast local testing.
- **Accepted residual (implementation-time finding, `iso22900-service`): the `cop_tags` eviction
  path (Decision item 1's "evict on the observed terminal `PDU_COPST_FINISHED`/`PDU_COPST_CANCELLED`
  status event") is implemented but not covered by an automated test.** `iso22900-mock`'s
  `PDUGetEventItem` only ever synthesizes `PDU_IT_RESULT` items, never `PDU_IT_STATUS`, so the
  terminal-status branch in `iso22900-service/src/service/events.rs`'s `to_proto_event_item` is
  unreachable through the existing black-box mock without first extending it to emit status events
  — a mock-fidelity gap that predates this ADR, not introduced by it. Verified correct by direct
  code reading (the eviction runs after the tag is cloned for the echo, keyed off the same terminal
  status comparison used elsewhere in this function). Tracked as a `iso22900-service` backlog item
  (`docs/implementation-notes.md`) rather than blocking this ADR's implementation on extending the
  mock's status-event fidelity, which is a separate, pre-existing concern.
- **Accepted residual, restated from ADR-178's own precedent, not newly invented here:** deleting
  `CreateComLogicalLinkRequest.cll_tag` relies on `vci-service-manager` spawning every service and
  client from one colocated build, so no independently-versioned client exists today that could
  still send the removed field and observe a behavior change (there is none to observe — the field
  was already inert). If an independently-released client package is ever introduced for any of
  these services, this reasoning — and ADR-178's own equivalent acceptance for `channel_index` —
  needs re-evaluating before that release ships, not after.
