# ADR-153: J2534-2 Discovery Mechanism (Phase 1)

**Date:** 2026-07-30
**Status:** Accepted
**Affects:**
- `j2534-0404/src/lib.rs`, `j2534-0404/src/discovery.rs` (new)
- `j2534-0404-service/src/service.rs`, `j2534-0404-service/src/service/discovery.rs` (new)
- `j2534-0404-mock/src/lib.rs`
- `docs/j2534-2-support-plan.md` (Phase 1 progress)
- `docs/adr/ADR-152-j2534-2-foundational-decisions.md` (Decision 2's deferred client-exposure question, answered here for this phase)

## Context

`docs/j2534-2-support-plan.md`'s Phase 1 brings up SAE J2534-2 (DEC2020)
clause 25's Discovery Mechanism: the `GET_DEVICE_INFO`/`GET_PROTOCOL_INFO`
`PassThruIoctl` calls that let an application query a device's capabilities
parameter-by-parameter via the `SPARAM`/`SPARAM_LIST` structures (already
added to `j2534-0404-sys`'s header in Phase 0, ADR-152).

ADR-152 Decision 2 already settled that these two IOCTLs are consumed
*internally* by `j2534-0404-service` rather than exposed as a new
client-visible RPC, "cached at device-open time" to support Decision 1's
connect-time capability enforcement — but left two things open for this
phase to decide: the exact shape of that internal cache, and whether to
revisit the internal-only default and expose Discovery data to D-PDU
clients after all (ADR-152: "the default until then is internal-only").

This ADR makes those two calls, plus a related mock-fidelity decision.

## Decision

### 1. Cache shape: lazy, per-parameter, epoch-tagged — not an eager blanket fetch

`j2534-0404-service::J2534Service` already has two precedents for a
device-derived, self-invalidating cache: `resolved_can_channel_mode` and
`j1850_bus_flavor` (ADR-107 addendum (h)). Both are populated lazily, on
first use, and stamped with the service's `device_epoch` counter — a cached
entry is used only if its stored epoch still matches the live one; a
stale-epoch entry is silently treated as a miss and re-probed, with no
active invalidation needed on device close/reopen.

The Discovery cache follows the same shape: two new fields,
`discovery_device_info: Arc<Mutex<HashMap<(u32, u32), (u64,
DiscoveryResult)>>>` keyed by `(module_handle, parameter)` and
`discovery_protocol_info: Arc<Mutex<HashMap<(u32, u32, u32), (u64,
DiscoveryResult)>>>` keyed by `(module_handle, protocol_id, parameter)`,
populated one `SPARAM` at a time, on first request for that key.
`module_handle` is part of both keys, not just the epoch — see the
"Correction" below this section for why an epoch-only key is unsound.

**Rejected alternative:** eagerly querying every known `GET_DEVICE_INFO`
parameter the instant a J2534-2-opted-in module opens. `j2534_v0404.h`
currently defines roughly 85 `DEVICE_INFO_*` parameters; blanket-querying
all of them today would mean re-exporting most of that list from
`j2534-0404` for zero actual consumer — no later phase's need is known yet,
and roughly a third of that list (`SHORT_TO_GND_J1962`, `PGM_VOLTAGE_J1962`,
and every `*_PS_J1962`/`*_PS_J1939`/`*_PS_J1708` parameter, per clause
25.3.2.2's Table 111) requires a caller-chosen pin bitmask as an *input*,
so it cannot be blanket-queried at all — that family is Pin Selection's own
job (Phase 2). The lazy, per-parameter shape generalizes an already-proven
pattern instead of inventing a new one, and "cached at device-open time"
(ADR-152 Decision 2) is satisfied by scoping cache entries to
`device_epoch` exactly like the two existing precedents — not by
pre-fetching every parameter the moment the device opens.

**Correction (edge-case-hunter finding on this same phase's PR):** the
epoch stamped onto a freshly-inserted entry must be read *after* the native
`PassThruIoctl` call, not at function entry — `ensure_open_device_for` can
itself perform the device's first `PassThruOpen` and bump `device_epoch` as
a side effect, so an entry inserted under the pre-open epoch would be
immediately stale on the very next lookup, defeating the cache on exactly
the "device just opened, first Discovery query" sequence Decision 4
earmarks for a future phase's actual use case. Fixed to re-read the epoch
right before the `.insert()` call, mirroring `probe_can_channel_mode`'s and
`autodetect_sae_j1850_flavor`'s existing re-read-after-every-`.await`
discipline — the entry-time read is still used for the cache-*hit* check
(a stale value there only costs one extra redundant native call, never an
incorrect answer).

**Correction (Codex review on PR #25):** the original cache keys —
`parameter` alone for device-info, `(protocol_id, parameter)` for
protocol-info — omitted `module_handle` entirely, relying on `device_epoch`
to distinguish sessions. That is unsound: `device_epoch` only tracks *that*
an open/close transition happened, not *which* module is the one currently
open. A query for module A's capabilities, issued while module B happens to
be the module actually open (both under the same, unchanged epoch), would
hit an entry module B's own earlier query had cached — silently returning
B's device's capabilities mislabeled as A's, and entirely bypassing
`ensure_open_device_for`'s resource-busy rejection, which only runs on a
miss. Fixed by adding `module_handle` to both keys (`(module_handle,
parameter)` and `(module_handle, protocol_id, parameter)` respectively) —
`docs/j2534-2-support-plan.md`'s design questions don't cover this since
ADR-107's single-open-device model was assumed sufficient protection; it
protects against two modules being open *simultaneously*, not against a
cache entry outliving the module identity it was queried under.
`device_info_cache_does_not_leak_across_modules`/
`protocol_info_cache_does_not_leak_across_modules`
(`service/discovery.rs`) pin this directly: query module 1, then query
module 2 for the same parameter while module 1 is still open, and assert
the second call rejects with `PDU_ERR_RESOURCE_BUSY` rather than returning
module 1's cached answer.

Locking: a lookup either (a) locks the map alone, finds a fresh entry,
returns — no other lock touched — or (b) on a miss, drops that lock, calls
`ensure_open_device_for` (which returns the held `device_id` guard,
per that function's own TOCTOU-safety contract shared by every other
module-scoped IOCTL helper in this crate), locks `self.api` nested inside
it to issue the native `PassThruIoctl` call, drops `api`'s lock, then
re-locks the map — still under the `device_id` guard — to insert and
returns. **Correction (edge-case-hunter finding on this same phase's PR):**
an earlier draft of this ADR claimed the discovery maps are "never held
while acquiring another lock" and that this "sidesteps the lock-ordering
hierarchy entirely" — both overstated. `device_id` (via that guard) *is*
held across the map's insert-time lock acquisition on the miss path. This
is not a new edge to audit, though: `device_id` is already this crate's
documented outermost lock (ADR-107 addendum) — "never acquired while
holding any other service lock" — which is a one-way rule about what may
be held *before* acquiring `device_id`, not a promise that nothing else is
ever acquired *while holding* it (every other module-scoped IOCTL helper,
e.g. `ioctl_read_vbatt`, already holds it across its own `api` call the
same way). The only claim this ADR can actually make is narrower: `api`
and the discovery maps are never ordered against *each other* — `api`'s
lock is always independently acquired-and-released, never held across a
discovery-map acquisition or vice versa — so no new edge exists between
those two specifically. A benign, serialized-not-parallel redundancy is
still possible: since the miss path doesn't re-check the map after
acquiring the `device_id` guard, a second caller that also missed and is
now waiting on that same guard (rather than blocked on a true data race)
still issues its own native call once it acquires the guard, even though
the first caller's insert already landed. Wasteful, not incorrect — clause
25.3.2.1 defines device/protocol capabilities as static, so both native
answers are the same value.

**Correction (Codex review, round 5 on PR #25):** the cache-*hit* check's
`device_epoch` read originally happened *before* `.lock().await` on the map.
Since `.lock().await` is a genuine suspension point, a concurrent
`ModuleDisconnect` could bump the live epoch while this task was waiting for
the lock — and the stale, pre-await epoch value captured earlier would still
compare equal to an entry cached under that same stale epoch, serving it as
a "hit" for a device that had since closed, bypassing
`ensure_open_device_for` entirely. Fixed by reading `device_epoch` after the
lock is acquired, with no `.await` between the read and the comparison, so
nothing can invalidate the entry in the window this check actually
evaluates it. (The insert-time epoch read, added by the earlier
"Correction" above, was already correctly placed after every relevant
`.await` — this fix is specific to the hit-check read, which had no
corresponding earlier bug because it was believed instantaneous relative to
the epoch it read, an assumption `.lock().await`'s suspension possibility
invalidates.)

### 2. Query gating: only for a `pname` opted into J2534-2 (clause 5)

A new `is_j2534_2_opted_in(pname: Option<&CStr>) -> bool` helper
(`service/discovery.rs`) checks for the literal `"J2534-2:"` prefix (clause
5's feature-enablement convention, already noted as resolved plumbing in
`docs/j2534-2-support-plan.md` §4.1). A module whose `pname` doesn't carry
this prefix is never queried — clause 5 says a non-prefixed `pname` means
J2534-1-only behavior must be assumed, so the device may not even implement
these IOCTLs. This is narrower than §4.1's still-open secondary question
("how should a non-`"J2534-2:"`-prefixed, non-`NULL` `pname` otherwise be
treated") — that remains unresolved; this ADR only decides whether the
Discovery IOCTLs get issued.

### 3. Client exposure: stays internal-only for this phase

ADR-152 left exposing Discovery data to D-PDU clients (a new `PDU_IOCTL_*`
pair) as an open option for Phase 1. This phase keeps the internal-only
default: no proto/RPC surface change. Two reasons drove this over adding a
new `PDU_IOCTL_*` pair now:

- No concrete D-PDU client need exists yet — ISO 22900-2's object model has
  no obvious "raw discovery parameter" verb, and inventing one now would be
  guessing at a payload shape ADR-152 explicitly flagged as premature
  ("the exact `DataItem`/proto payload shape ... is Phase 1/14 design
  work, not resolved here" — resolving it without a real caller would just
  move the guess from ADR-152 into this ADR).
- It is not needed for test coverage either: `j2534-0404-service` already
  has a convention (`rpc_misc.rs`'s
  `ioctl_set_event_queue_properties_atomic_trim_tests`, `rpc_link.rs`'s
  CAN-mode tests) of building a full `J2534Service` backed by the real
  `j2534-0404-mock` cdylib inside an inline `#[cfg(test)]` module and
  calling private/`pub(super)` methods directly. The new
  `discovery_device_info`/`discovery_protocol_info` methods are tested this
  way, exercising the real `PassThruOpen` → `PassThruIoctl(GET_DEVICE_INFO)`
  round-trip against the mock without any gRPC surface.

A future phase may revisit this if a concrete client need for raw discovery
data (as opposed to a phase-specific D-PDU projection, e.g. Phase 14's own
`GET_DEVICE_CONFIG` IOCTL) actually emerges.

### 4. No connect-time enforcement wiring yet

Every currently-connectable D-PDU resource is a J2534-1 protocol, which
this service has always fully supported — there is no capability decision
for `ConnectComLogicalLink` to make with Discovery data yet, and wiring a
branch in now would be untestable dead code (no fixture exists for "device
lacks capability X" since no J2534-2-gated resource exists). This is
deferred to whichever future phase (2+) first advertises a J2534-2-gated
resource: that phase calls the methods this ADR adds at its own enforcement
point.

### 5. Mock fidelity

Per ADR-152 Decision 4's Phase-1 addition, `j2534-0404-mock` implements
`IOCTL_GET_DEVICE_INFO`/`IOCTL_GET_PROTOCOL_INFO` deliberately rather than
falling through to the existing default `ERR_INVALID_IOCTL_ID` arm (which
already covers every *other* not-yet-implemented J2534-2 IOCTL ID, so no
change was needed there):

- `GET_DEVICE_INFO`: reports `Supported=1` with a clause-25.3.2.2-shaped
  `0xPPQQRRSS` value (`QQ=RR=0`, since the mock implements neither
  Additional Channels nor Pin Selection yet; `SS=1`) for exactly the 10
  base J2534-1 protocols' `*_SUPPORTED`/`*_SIMULTANEOUS` parameters, and
  `Supported=0` for every other known parameter (nothing beyond J2534-1 is
  implemented in the mock yet) — including the entire pin-bitmask-input
  family, which the mock declines regardless of the caller-supplied pin.
- `GET_PROTOCOL_INFO`: returns `ERR_INVALID_PROTOCOL_ID` for any protocol ID
  outside the 10 known native J2534-1 IDs; for a known protocol, reports a
  small, real subset of parameters (`MAX_RX_BUFFER_SIZE` using the existing
  `PASSTHRU_MSG.Data` size of 4128, `MAX_PASS_FILTER`/`MAX_BLOCK_FILTER`,
  `CAN_11_29_IDS_SUPPORTED` gated to the CAN/ISO15765 pair) and
  `Supported=0` for the rest.

## Consequences

- `j2534-0404` gains general-purpose `get_device_info`/`get_protocol_info`
  primitives (mirroring `get_config`/`set_config`'s `SCONFIG_LIST`
  marshaling, but over `SPARAM_LIST`, with a `Supported` flag alongside
  `Value`) — usable by any future phase without further FFI-layer work.
- The Discovery cache exists and is exercised by real tests, but has no
  consumer yet; a future phase's design work is expected to reference
  `docs/j2534-2-support-plan.md`'s Phase 1 progress note and this ADR
  rather than rediscovering the cache from scratch.
- `docs/j2534-2-support-plan.md`'s design question §4.3 already resolved
  the "no new RPC" question generally (ADR-152); this ADR is the concrete
  instance of that resolution for Discovery specifically, and the reasoning
  in Decision 3 above is the citable justification the next design-question
  cross-reference should point to if a future phase reconsiders exposing
  Discovery data directly.
- **Accepted residual:** because client exposure stays internal-only,
  nothing outside this service can query Discovery directly yet (e.g. for
  diagnostics/tooling) — acceptable since no such caller has asked for it.
- **Accepted residual (edge-case-hunter finding):** `discovery_device_info`/
  `discovery_protocol_info` index `self.modules[(module_handle - 1) as
  usize]` with no range check, relying entirely on the documented "caller
  already ran `require_module_handle`" contract — the exact same pattern
  `ensure_open_device_inner`'s `requested` parameter already uses
  (`service.rs`), and equally unchecked there. An out-of-range
  `module_handle` (in practice, only `0` — everything else is guarded
  earlier — since these are internal `pub(super)` methods with no RPC caller
  yet) panics instead of returning a clean `Status`. Accepted because it
  matches this crate's existing convention for every other internal
  module-scoped helper rather than introducing a new, inconsistent
  validation style; the two RPC handlers a future phase adds when wiring
  these methods into a client-visible path (per Decision 4) will call
  `require_module_handle` first, same as every other module-scoped RPC in
  this crate already does.
