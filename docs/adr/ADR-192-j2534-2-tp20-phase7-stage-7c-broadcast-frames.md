# ADR-192: SAE J2534-2 TP2.0 (Phase 7, Stage 7c — Broadcast Frames and Periodic Re-Trigger)

**Date:** 2026-08-24
**Status:** Accepted (Decision item 2's in-flight-reservation/terminator-race mechanism superseded by ADR-193)
**Affects:**
- `j2534-0404-service/src/service/service_params.rs`
- `j2534-0404-service/src/service/comparam_id.rs`
- `j2534-0404-service/src/service/comparam_support.rs`
- `j2534-0404-service/src/service/comparam_defaults.rs`
- `j2534-0404-service/src/service/rpc_primitive.rs`
- `j2534-0404-service/src/service/rpc_link.rs`
- `j2534-0404-service/src/service/rpc_misc.rs`
- `j2534-0404-service/src/service/events_tp20_connection.rs`
- `j2534-0404-service/src/service/events_rx_routing.rs`
- `j2534-0404-service/src/service/events.rs`
- `j2534-0404-service/src/service.rs`
- `j2534-0404-service/src/error.rs`
- `j2534-0404-mock/src/lib.rs`
- `j2534-0404-service/docs/implementation-notes.md`
- `docs/j2534-2-support-plan.md`
- ADR-093 (Status line, partial annotation), ADR-188 (Status line, `Stage 7c` fulfilled)

## Context

Per ADR-188's "Staged delivery" section (§5), Stage 7c is the last unshipped TP2.0 sub-stage:
SAE J2534-2 clause 19's broadcast-send capability and the `PassThruStartPeriodicMsg`
re-trigger path built on top of it. Phase 0 (ADR-152) already added the native constant
`TX_FLAG_TP2_0_BROADCAST_MSG` (bit 16) to `j2534-0404-sys`'s bindings for all 5 target
triples — no header/bindgen work is needed here. ADR-188 §5 flagged, as a precondition for
choosing this stage's mechanism, whether the frozen client-facing `TxFlagBit` proto enum
already carries a value for this bit.

Clause 19.3.2.2 (paraphrased): when `PassThruWriteMsgs` is called with the native TxFlag bit
`TP2_0_BROADCAST_MSG` set and the message's `Data[0]` holds a valid broadcast address
(`0xF0`-`0xFF`), the device sends the message five times at the `TP2_0_T_BR_INT`-configured
interval, alternating the last two data bytes between `0xAA`/`0x55` each transmission; an
invalid `Data[0]` returns `ERR_INVALID_MSG`. This sits alongside (not in place of) the
existing three-way `PassThruWriteMsgs` routing rule already normalized by Stage 7a/7b
(connection-bound send / raw single-frame send / `ERR_NO_CONNECTION_ESTABLISHED`).

Clause 19.3.2.3 (paraphrased): `PassThruStartPeriodicMsg`, given the same broadcast message
data, the same TxFlag, and a periodic rate, immediately sends the five-times
`T_BR_INT`-spaced burst and then continues at the periodic rate thereafter, still alternating
the last two bytes each send.

This stage's design questions were resolved by a `design-advisor` consult, grounded first by
this session's own direct investigation of the proto, the ComParam ID space, and the relevant
prior ADRs (below), then handed to the advisor as a distilled brief rather than raw
exploration.

### Investigation findings (verified directly, not delegated)

- `vci-service-interface/src/proto/service.proto`'s `TxFlagBit` enum carries exactly the 6
  values ISO 22900-2:2022 D.2.1/Table D.4 defines; no J2534-2-only value exists for this bit.
  ADR-188's precondition resolves: no.
- The same message's `tx_flag_raw` alternative (`ComPrimitiveCtrlData`'s `tx_flag` oneof) is
  explicitly documented, and its consumer (`rpc_primitive.rs::compute_j2534_tx_flags`) is
  implemented, as the ISO 22900-2 D.2.1 Table D.4 4-byte layout — the identical byte array
  `iso22900-service` passes byte-for-byte to the native D-PDU API (ADR-116). It decodes only
  the two ISO-defined bit positions that layout actually assigns; hijacking a reserved position
  for a J2534-2-only bit with no ISO equivalent would silently diverge the two services'
  interpretation of one shared proto field — not a legitimate escape hatch.
- `ComPrimitiveCtrlData` (the send-control message on the COP-execution/`CoptSendrecv` path)
  has no generic/unused byte-blob field the way `DataItem.bytearray_data` exists on the
  `IoCtl` path (ADR-178's own escape hatch for a heterogeneous/byte-blob-shaped capability) —
  its fields are `time`/`num_send_cycles`/`num_receive_cycles`/`temp_param_update`/the
  `tx_flag` oneof/`expected_response_array`, none free.
- ADR-178 (the governing proto-freeze ADR) gives a forward-looking rule for representing a new
  J2534-2 capability without growing `service.proto` again: a scalar or native
  id/value-pair-list-shaped capability becomes a ComParam (its own `CP_AnalogSampleRate`,
  `0x80C4`, is the established mint precedent); a byte-blob-shaped structure becomes
  `DataItem.bytearray_data`; a name-shaped qualifier extends the string-typed name-resolution
  grammar. TP2.0 broadcast is a per-send-call decision on the COP-execution path, not a
  connect-time-resolved scalar and not `IoCtl`-routed — it doesn't fit either shape cleanly on
  its face. ADR-178's own Consequences section names this exact gap: "some unshipped future
  phases (TP2.0's connection-oriented lifecycle in particular...) may need genuinely new
  client-visible RPC-level semantics... Those get their own `design-advisor` consult when their
  time comes." This is that consult.
- The closest existing "ComParam implies a native per-message TxFlags bit" precedent
  (`ComParamSet::sw_can_tx_flags`, `CP_SwCan_HighVoltage` → `TX_FLAG_SW_CAN_HV_TX`, ADR-164
  Decision 2) does not transfer directly: every existing ComParam-derived TxFlags bit
  (`sci_tx_flags`, `sw_can_tx_flags`, `msg_priority_tx_flags`) is documented as an
  objective, **link-invariant** fact, true for every send on that link until explicitly
  changed. TP2.0 broadcast is not link-invariant — the same connection routinely sends
  ordinary connection-bound messages and occasional broadcast messages.
- A second, more load-bearing precedent pulls against literally calling native
  `PassThruStartPeriodicMsg` operationally: ADR-083 originally used it for tester-present's
  hardware-autonomous mode 0, then recorded its concrete cost (no per-tick visibility, no
  callback, no per-tick timestamp readback — full bidirectional gap coordination unachievable).
  ADR-093 then superseded that entirely: both tester-present modes now dispatch from this
  service's own software poll loop instead, specifically to regain that coordination
  (`CP_P3Func`/`CP_P3Phys` gap timing, per-send `discard_until` windowing). ADR-093 also
  records, explicitly, that this was a removal of *this service's own usage* of the native
  periodic-message call — the `j2534-0404` safe-wrapper crate's `start_periodic_message`/
  `stop_periodic_message`/`PeriodicMessageId` API and the underlying FFI bindings were left
  untouched, since they mirror the native J2534 API surface rather than any one service's use
  of it. ADR-093 also records that ADR-010 (a fix for a `PassThruStartPeriodicMsg`
  handle leak across shared-channel disconnect/destroy) became "fully superseded" specifically
  because, with no periodic message ever started for tester-present, its fix target no longer
  existed — not because the underlying leak class is now structurally impossible.
- `TP2_0_T_BR_INT` (native `CONFIG_TP2_0_T_BR_INT`, `0x8044`, default 20 per Table 77) is not
  wired anywhere in this service today (confirmed by a repo-wide search — the constant exists
  only in the generated bindings). None of Table 77's ten TP2.0 timing parameters were wired by
  Stage 7a/7b; ADR-188 §4 deferred all ten as governing the device's autonomous connection
  state machine, out of that stage's scope. `TP2_0_T_BR_INT` is different: it is
  client-workflow-facing for exactly the feature this stage ships (the broadcast burst
  interval), not a connection-machine internal — it needs wiring here specifically. The other
  nine stay deferred exactly as ADR-188 §4 left them; that deferral is not superseded by this
  ADR.
- The next free service-level ComParam ids are `0x80D0`/`0x80D1` (the existing TP2.0 block
  runs `0x80C9`-`0x80CF`, the highest currently minted id anywhere in
  `service_params.rs` — `PARAM_TP20_PASSIVE_RX_ID`).

## Decision

**1. Broadcast addressing: a new, per-send-scoped ComParam, `CP_TP20BroadcastAddress`
(`0x80D0`), carrying the broadcast address itself (`0` = no broadcast / normal send;
`0xF0`-`0xFF` = send as a TP2.0 broadcast to that address; any other nonzero value rejected).**

`ComParamId::to_j2534_config_id` returns `None` for it (service-level only, matching
`PARAM_TP20_PASSIVE_IDENTIFIER`/`_RX_ID`'s existing shape) — it is read directly by the
`CoptSendrecv` TX-flags/addressing resolution path, never forwarded to a native `SET_CONFIG`
call (there is no corresponding native `SET_CONFIG` parameter for a per-message address in the
first place). `comparam_support.rs::is_tp20_param` gains this id as an allowed TP2.0-family
ComParam.

Per-send scoping reuses the existing ADR-067 `temp_param_update` mechanism unchanged: a client
stages the broadcast address into Working via `SetComParam`, then issues the broadcast
`CoptSendrecv` with `temp_param_update` set. `rpc_primitive.rs`'s existing single
ComParam-snapshot critical section (ADR-067 claim A) already binds Working instead of Active
for a temp-scoped `CoptSendrecv`, and the poll task already reverts hardware to the live Active
set afterward — no new per-send-scoping machinery is introduced; this ADR only adds one more
resolved field to the same binding. A client that wants every send on a TP2.0 CLL to be a
broadcast may instead stage it into Active — nothing here forbids that — but the documented,
recommended recipe is the temp-scoped one, since it is the shape that matches "this one send is
a broadcast" without leaving state that a later, unrelated send on the same CLL could pick up
by omission.

At COP-execution time, when the resolved `CP_TP20BroadcastAddress` is nonzero (and the link is
a genuine TP2.0 link — gated the same way `sw_can_tx_flags`/`msg_priority_tx_flags` gate their
own ComParam-derived bits, since `is_tp20_param`'s allowlist is per-family, not proof of the
live link's own protocol): the send path composes the native message as
`Data[0] = address` followed by the caller's payload (never the 4-byte TX-ID prefix a
connection-bound TP2.0 send composes, and never gated on the connection being in the
`Established` phase — a broadcast is not addressed to an established connection), and ORs
`TX_FLAG_TP2_0_BROADCAST_MSG` into the native TxFlags the same way `sw_can_tx_flags` ORs its
own bit in. An address outside `{0} ∪ [0xF0, 0xFF]` is rejected before any native call, mapped
to the same `PduErrInvalidParameters` shape `error.rs::pdu_error_for` already gives
`ERR_INVALID_MSG` — the RPC layer enforces clause 19.3.2.2's address-validity precondition
itself rather than relying on the native call's own rejection, since by the time the native
call sees it, `Data[0]` has already been unconditionally overwritten by the ComParam's address
value (there is no client-supplied `Data[0]` to separately validate in this design — the
conjunctive "flag set AND valid `Data[0]`" precondition the spec text states for the native API
is satisfied by construction here, not replicated as two independent client signals).

A broadcast `CoptSendrecv` with a nonzero `num_receive_cycles`, or a non-empty
`expected_response_array`, is rejected as an invalid request (`PduErrInvalidParameters`): no
response is attributable to a broadcast send under this service's per-connection response
routing, so a caller asking to wait for one is a request this design cannot honor, not a
degraded-but-honorable one.

**Repeat Messaging exclusion (Codex review fix, P2, PR #101, round 9):** SAE J2534-2 clause 14
Repeat Messaging (`PDU_IOCTL_START_REPEAT_MESSAGE`) is a genuinely separate feature from this
ADR's own broadcast/periodic-re-trigger mechanisms — it is a channel-level ioctl, not a
ComPrimitive, with its own independent TxFlags composition. `tx_header::build_tx_message`'s
TP2.0 arm is shared infrastructure, though: it composes `[address] ++ payload` broadcast framing
whenever the resolved `CP_TP20BroadcastAddress` is nonzero, regardless of which caller resolved
it, and `CP_TP20BroadcastAddress` can be staged into Active via a plain `SetComParam` +
`CoptUpdateparam`, entirely independent of ever issuing a broadcast `CoptSendrecv`. Since
`ioctl_start_repeat_message`'s own TxFlags composition has no broadcast awareness,
`PDU_IOCTL_START_REPEAT_MESSAGE` is rejected synchronously (`PduErrIdNotSupported`, before any
native call or state mutation) whenever a TP2.0 CLL's Active `CP_TP20BroadcastAddress` is
staged, rather than silently composing a frame the receiving side would misinterpret. This is a
scope exclusion, not a design gap this ADR intends to eventually close: Repeat Messaging and
per-send broadcast addressing were never meant to compose, and extending Repeat Messaging to
also support broadcast framing would be new feature work, out of scope here.

**2. Periodic re-trigger: native `PassThruStartPeriodicMsg`/`PassThruStopPeriodicMsg`,
reintroduced for this one use, mapped from a cyclic broadcast `CoptSendrecv`
(`num_send_cycles == -1`, `time` = the desired periodic rate, `CP_TP20BroadcastAddress`
nonzero).**

This is not a reopening or partial reversal of ADR-093's decision for tester-present, and does
not restore mode-0-style hardware-autonomous dispatch there. The two cases differ in what
"software dispatch" would even mean: ADR-093's tester-present move to software dispatch was
possible, and worth its accepted timing-accuracy costs, because a software poll loop can
reproduce a periodic tester-present send's wire behavior exactly. TP2.0's broadcast re-trigger
cannot be reproduced the same way: the native API's only broadcast-capable primitives are
`PassThruWriteMsgs` (always a full 5×`T_BR_INT` burst, never a single frame) and
`PassThruStartPeriodicMsg` (burst once at arm, then genuinely-periodic single sends
thereafter, alternating `0xAA`/`0x55` each send). A software cyclic loop calling
`PassThruWriteMsgs` once per configured period would re-emit the 5-frame burst on every tick
instead of a single alternating frame — observably wrong on the wire, not merely less
precisely timed. TP2.0 also has none of tester-present's ADR-093 coordination requirements to
regain (`CP_P3Func`/`CP_P3Phys` gap timing does not apply here; ADR-188 §4 already excludes
`CP_TesterPresentSendType`-class concepts from TP2.0's allowlist since clause 19.3.1's device
autonomously maintains the established connection). ADR-093 itself explicitly left the
`j2534-0404` wrapper's periodic-message API and FFI bindings untouched for exactly this kind of
future native-surface use; this ADR is that use.

**Governing rule (rewritten in place, design-advisor consult, Codex review round 3 — the
original text below enumerated three terminators found in the first implementation pass; three
further review rounds each found a cross-cutting mechanism this enumeration had missed, so this
item now states the rule those terminators are instances of, not just the list itself):** the
broadcast periodic is client-visibly a cyclic ComPrimitive (started by `CoptSendrecv`, holding a
`cop_handle`, reported through `GetStatus`), so every COP-level mechanism this service already
has — suspension, cancellation, CLL teardown, a hard channel error — must terminate or track it,
exactly as it would any other executing COP. Every queue-scoped or RX-scoped mechanism
(`PDU_IOCTL_CLEAR_TX_QUEUE`, `PDU_IOCTL_RESET`) leaves it alone, because a broadcast periodic
never enters the `tx_held`/queue representation those mechanisms are defined over — it bypasses
`tx_queue`/`dispatch_tx_item` entirely by design (this item's own opening paragraphs). The
mechanism is NOT device-autonomous-vs-software-dispatched (Repeat Messaging, ADR-165, is left
running through a suspension for exactly that reason, since clause 14 makes it a channel-level
IOCTL feature, never a ComPrimitive) — it is whether the mechanism operates on the COP model or
the queue model.

`rpc_link.rs`/`service.rs` track the started `PeriodicMessageId` against the owning COP/CLL
(`LogicalLinkState::tp20_broadcast_periodic`, the same shape `handle_start_comm`'s pre-ADR-093
tracking used). The full terminator set, each stopping (or tracking the failure to stop) the
native message and finalizing the COP:

- **`CoptCancel`** of that COP: `stop_periodic_message`, `PduCopstCancelled` on success; on a
  failed native stop, the tracking entry is restored onto the CLL and the RPC returns an error
  instead of a false `Cancelled`, so a client retry naturally re-attempts the stop (Codex review
  round 2).
- **`DisconnectComLogicalLink`/`DestroyComLogicalLink`** of the owning CLL: `stop_periodic_message`
  unconditionally attempted. When this CLL is the last one on its physical channel (the channel
  itself is about to close via `PassThruDisconnect`), a stop failure is logged and dropped — the
  imminent `PassThruDisconnect` takes the device-side message with it, the same reasoning
  `leaked_repeat_message_ids`' own non-shared case already uses. When a sibling CLL keeps the
  physical channel open, a stop failure instead pushes the `PeriodicMessageId` onto a new
  `SharedChannel::leaked_periodic_message_ids: Vec<(PeriodicMessageId, u64)>` (the paired `u64` is
  the round-5 periodic-clear epoch, Decision item 2 below) — mirroring
  `leaked_repeat_message_ids`' own precedent exactly (ADR-165 Decision 6), since this CLL's own
  tracking is about to disappear (this is Fix B for the exact leak class ADR-010 existed to close
  for tester-present: ADR-093's "ADR-010 is fully superseded" held only because tester-present
  never starts a periodic message anymore, not because the leak mechanism is structurally
  unreachable in this service; this ADR reintroduces a real periodic-message start, so the same
  discipline applies again, for this one call site).
- **A hard channel error** (`events::handle_channel_hard_error`): the physical channel is going
  offline with `api` not held here, so no native stop is attempted — the entry is taken from the
  dying link (gated on this being the link's primary channel, the one a broadcast periodic is
  always started against) and pushed onto the dead `SharedChannel` entry's
  `leaked_periodic_message_ids`, mirroring this same function's existing repeat-message drain
  exactly. The owning COP still receives `PduCopstCancelled` through this function's ordinary
  per-link COP-cancellation sweep, like every other COP on a link going hard-offline.
- **TX-dispatch suspension taking effect** — `PDU_IOCTL_SUSPEND_TX_QUEUE`, or
  `CP_SuspendQueueOnError` at its three authoritative transition sites (the batch-final
  reconciliation and the two receive-phase timeout hooks — NOT the eager mid-batch publish a
  later batch-final pass can still override): a live broadcast periodic is **stopped and
  finalized**, not merely blocked from a future re-arm. This item's own start-time synchronous
  rejection (Consequences, below) already treats suspension as fatal to a *new* broadcast
  periodic; leaving an *already-running* one going under the identical suspension sources was
  the incoherent middle state Codex review round 3 correctly flagged — `PDU_IOCTL_RESUME_TX_QUEUE`
  has nothing to resume afterward, the same as a `CoptCancel`-terminated COP. A stop failure here
  is retained on the CLL (mirroring `CoptCancel`'s own failure handling) rather than leak-tracked,
  since the CLL itself is not going away.
- **A sibling acquiring `LOCK_PHYSICAL_TX_QUEUE`**: rejected outright rather than terminating
  anything (Consequences, below) — `rpc_lock_resource`'s scan also covers a channel's
  `leaked_periodic_message_ids`, not just live links' own tracking, so an orphaned-but-still-live
  stream from a prior failed stop keeps blocking new grants exactly like a live one would.

`rpc_misc.rs`'s existing `CLEAR_PERIODIC_MSGS` IOCTL handler is channel-wide and will now stop a
live broadcast periodic message device-side as a side effect of a client's unrelated request to
clear periodic messages on that physical channel. The handler gains the same reconciliation it
already performs for tester-present's per-CLL state (clearing every affected CLL's armed state
and surfacing the outcome) applied to a broadcast-periodic COP: on success, the owning COP's
state is transitioned to finalized/stopped rather than left claiming a periodic message that no
longer exists device-side.

**Update (Codex review round 5, design-advisor consult): the reconciliation above is
epoch-gated, not unconditional.** A concurrent `StartComPrimitive`'s own native
`start_periodic_message` call can race this handler's native `clear_periodic_messages` call on
the shared `self.api` mutex — the two calls serialize against each other, but each call's own
`logical_links`/`shared_channels` bookkeeping happens afterward, independently, and can interleave
either way. An unconditional reconciliation scan could therefore wrongly finalize a REAL,
already-committed broadcast-periodic entry whose native start actually happened AFTER this native
clear returned — the message was never touched by the native clear, so finalizing it as "cleared"
leaves it transmitting device-side with nothing left tracking it. This is closed by a single
global counter, `J2534Service::periodic_clear_epoch` (an `AtomicU64`, `Relaxed` ordering — the
`self.api` mutex itself is what provides the happens-before ordering, not the atomic operation):
bumped by exactly one while still holding `self.api`, immediately after a successful
`clear_periodic_messages` call returns, and read the same way immediately after a successful
`start_periodic_message` call returns, stashed as a new `Tp20BroadcastPeriodic::started_epoch`
field on the committed entry (and paired onto `SharedChannel::leaked_periodic_message_ids`
entries the same way). Because both native calls serialize on `self.api`, comparing a committed
entry's `started_epoch` against a later clear's own post-increment value (`clear_generation`)
tells the reconciliation scan whether that entry's native start provably preceded this clear's
native call (`started_epoch < clear_generation` — the entry is finalized, since the device-side
message is already gone) or raced/followed it (`started_epoch >= clear_generation` — the entry is
left live and tracked, untouched by this clear). A `None` in-flight-start sentinel's ordering against this specific clear is
undecidable at scan time — its own native `start_periodic_message` call hasn't returned yet, so no
`started_epoch` exists to compare — so the scan defers the decision instead of finalizing: it
records `max(pending_clear_generation, clear_generation)` onto a new `Tp20BroadcastPeriodic::
pending_clear_generation` field (the `max`, rather than a plain overwrite, so a second racing clear
can't regress an already-recorded higher generation from a first clear) and leaves the entry
tracked. `finalize_or_orphan_broadcast_periodic_start` resolves the reservation's true fate once its
own native call completes and `started_epoch` becomes known: `started_epoch < pending_clear_
generation` means at least one recorded clear's native call ran after this start's own returned, so
the message is already dead device-side (taken and finalized, no per-id native stop needed, since
the channel-wide clear already did the device-side equivalent); `started_epoch >= pending_clear_
generation` (including the `pending_clear_generation == 0` case, i.e. no clear ever scanned it)
commits it live exactly as an ordinary reservation would. Accepted transient: between the scan
recording `pending_clear_generation` and the native call resolving, `GetStatus` briefly still
reports `Executing` for a COP whose message may already be cleared device-side — bounded by the
in-flight native call's own duration, not indefinite. This deferral is specific to
`CLEAR_PERIODIC_MSGS`; the other four terminators of a broadcast-periodic COP (`CoptCancel`,
suspension-termination, teardown, and a hard channel error) intentionally remain unconditional
sentinel-takers, since their termination isn't contingent on native-call ordering against a
channel-wide clear. The channel's own `leaked_periodic_message_ids` is `retain`ed the same way
(entries with an epoch before `clear_generation` are moot and dropped; entries at or after it are
genuine post-clear leaks and stay tracked) rather than cleared outright.

**Update (Codex review round 10, PR #101): two more races closed in this same terminator
machinery, both clause 19.3.2.2/19.3.2.3-adjacent bookkeeping bugs rather than new mechanism.**

*Fix A (P1):* the `cancelled_cops` exclusion this governing rule requires (a broadcast-periodic
COP must never be marked `cancelled_cops`, since it is not "queued" the way that set's other
members are) is applied at three sites — `PDU_IOCTL_CLEAR_TX_QUEUE`'s handler, `CoptStopcomm`'s
queued-primitive cancellation block, and `cancel_send_recv_cops_for_cll` (the J1939
claim-relinquishment sweep). Two of the three snapshotted `link.tp20_broadcast_periodic`'s
`cop_handle` from a strictly earlier, separate `logical_links` lock acquisition than the final
`cancelled_cops.extend` that actually consumes it, mirroring an already-fixed same-shaped race
this ADR's own `tier2_now` exclusion (ADR-100 Decision §2) closed for a different field. A
concurrent `StartComPrimitive` publishing a fresh broadcast-periodic reservation into
`link.tp20_broadcast_periodic` in that gap was invisible to the stale snapshot, so the newly
reserved COP was wrongly marked `cancelled_cops` — corrupting `GetStatus` for a message about to
start transmitting. Fixed by re-reading `link.tp20_broadcast_periodic` fresh, under the same lock
the final `extend` already holds, at both sites — matching `cancel_send_recv_cops_for_cll`'s own
shape, which never had a separate early snapshot to begin with.

*Fix B (P2):* `terminate_tp20_broadcast_periodic_for_suspension`'s failed-native-stop branch
(the "retained on the CLL" outcome the suspension bullet above describes) restored the failed
entry onto `link.tp20_broadcast_periodic` whenever that slot read as empty, without checking
whether `cll_handle` was still the same connect session `periodic` was captured against. A
concurrent disconnect (or disconnect+reconnect) racing this failure branch can make the slot read
as empty for an unrelated reason — the disconnect's own teardown, or a fresh reconnect's clean
slate — and restoring onto that mismatched session would wrongly graft an old channel's failed
transmitter onto a new one, with no guarantee anything ever notices it again if that CLL
disconnects a second time. Fixed by gating the restore on the captured `connect_generation` still
matching the live link's: on a match, behavior is unchanged (restore onto the CLL, mirroring
`CoptCancel`'s own restore-on-failure); on a mismatch (or the link being gone entirely), the
restore is skipped and the failed stop is leak-tracked against the captured `channel_key`'s
`SharedChannel::leaked_periodic_message_ids` instead — unconditionally, not gated on a sibling
ref count, mirroring `DisconnectComLogicalLink`'s own Fix B leak-tracking shape above — or, if
that physical channel has also fully closed by the time this runs, logged as an accepted-residual
double-fault with nothing left to track. `shared_channels` is acquired before `logical_links` in
this branch (ADR-080 lock ordering), uniformly across both outcomes even though the same-session
path doesn't need it.

*Fix B follow-up (edge-case-hunter, PR #101 round 10 verification):* `ChannelKey` (the
protocol/baud/flags/pin tuple) has no uniqueness-over-time guarantee — a channel close followed by
an unrelated fresh `ConnectComLogicalLink` at the same params installs a brand-new `SharedChannel`
at the identical key, so the mismatch branch's leak-track could otherwise land on a channel that
was never actually running the failed message. Gated the leak-track on the captured `channel_id`
still matching the live `SharedChannel`'s own `channel_id` — cheap (no new lock or parameter), and
closes the practical case, since this crate's mock (and most real adapters) hand out a fresh id on
each connect. Accepted residual, not airtight: `next_connect_generation`'s own doc comment
(`service.rs`) already notes a `channel_id` can coincidentally repeat on a shared channel, so a
real adapter that reissues the same numeric handle after a close+reopen could still slip through
this gate. A fully airtight check would compare `SharedChannel::occupancy_epoch` (ADR-161) the way
`ioctl_reset`'s Phase 1 does, but that requires capturing it at the same time as
`periodic`/`channel_id`/`connect_generation` — i.e. acquiring `shared_channels` before
`logical_links` at all 4 call sites, a larger lock-ordering restructuring judged out of scope for
this fix; tracked in `j2534-0404-service/docs/implementation-notes.md`'s backlog.

**Update (Codex review round 11, PR #101): two findings about the same underlying gate, fixed
together by extracting it into one shared helper.** *Finding 1 (P2):* Fix B's `same_session`
gate above compared only `connect_generation`, but a plain disconnect with no reconnect leaves
`connect_generation` unchanged (`rpc_link.rs`'s disconnect path only clears
`connected`/`channel_id`) — so after a plain disconnect the gate still read as a session match
and wrongly restored the failed entry onto the now-disconnected link. *Finding 2 (P2):* the
identical unprotected hazard existed in `rpc_cancel_com_primitive`'s `CoptCancel` handling of a
TP2.0 broadcast-periodic COP, which restored a failed stop's entry onto the CLL unconditionally
— no session/generation/connected check at all — because it never received Fix B's protection in
the first place. Both are now closed by extracting the restore-vs-leak-track decision into one
shared method, `restore_or_leak_track_broadcast_periodic`, called by both
`terminate_tp20_broadcast_periodic_for_suspension` and `CoptCancel`'s failure branch: the
`same_session` check now additionally requires `link.connected` (mirroring
`reserve_tp20_broadcast_periodic`'s own identical generation-plus-connected gate), and
`CoptCancel` now captures `connect_generation`/`channel_key` in the same `logical_links` critical
section it already captures `channel_id`/`periodic` from, passing them through to the shared
helper exactly as the suspension path does. Consolidating the decision into a single
implementation is deliberate: finding 2 is exactly the failure mode of fixing one of two
structurally identical branches in isolation, so a future change to this gate can no longer drift
between the two call sites.

**Update (Codex review round 12, P2, PR #101): a later race in the same finalization path,
between the `logical_links` commit and this method's own status emission.**
`finalize_or_orphan_broadcast_periodic_start`'s `Live` resolution commits the real entry into
`logical_links` (replacing the `None`-sentinel in-flight reservation) under that lock, then
releases it before reporting `PduCopstExecuting`. Once the sentinel is gone, a concurrent
`CLEAR_PERIODIC_MSGS`/suspension-termination/`CoptCancel` can see the freshly-committed entry,
take it, stop it natively, remove the COP from `primitives`, and emit its own terminal status —
all before the deferred `Executing` report runs, which would then surface `Executing` after a
terminal event, violating status monotonicity. Fixed by routing that report through a new
sibling helper, `events::emit_nonterminal_if_live` (`events_event_senders.rs`), which mirrors
`emit_terminal_if_live`'s existing "hold the `primitives` lock across the liveness check and the
send" technique but never removes the entry — a nonterminal status must never finalize a COP. The
gate is `primitives` containment alone, matching every other terminal-finalization site in this
crate: round 10's Fix A already excludes a broadcast-periodic COP's own `cop_handle` from ever
being marked `cancelled_cops` while still genuinely live and un-finalized, so no additional
`cancelled_cops`/`terminal_cops` check is needed here.

**Update (Codex review round 13, P2, PR #101): the reclaimed-slot branch of
`restore_or_leak_track_broadcast_periodic` now leak-tracks instead of dropping the failed stop
with no tracking left anywhere.** Round 11's shared-helper extraction (above) left one branch
unfixed: when `same_session` holds but `link.tp20_broadcast_periodic` was already reclaimed by a
fresh reservation before the failure could be recorded, the helper only logged a `warn!` and
dropped the old message — untracked in `tp20_broadcast_periodic` (reclaimed) and untracked in
`leaked_periodic_message_ids` (only the session-mismatch branch leak-tracked), so
`LOCK_PHYSICAL_TX_QUEUE`'s active-transmission scan (which checks both places) could never detect
or retry stopping it. This was an accepted residual up through round 10 because, at the time,
`shared_channels` was not yet locked anywhere in this code path; round 11's extraction into
`restore_or_leak_track_broadcast_periodic` made that justification stale without revisiting the
reclaimed-slot branch itself — `chans` (the `shared_channels` guard) is acquired for the helper's
entire body, before `same_session` is even computed. Fixed by restructuring the helper so both
"can't restore" cases — session mismatch and same-session-with-slot-reclaimed — fall through to
the same leak-tracking logic (still gated on the live `SharedChannel::channel_id` matching the
captured one, per round 10's Fix B follow-up above, which now also protects this path). The
double-fault `warn!` (channel also gone/replaced) now carries a `slot_reclaimed` field so the two
causes stay distinguishable in logs. Closes the "same-generation/slot-reclaimed double-fault"
residual previously tracked in `j2534-0404-service/docs/implementation-notes.md`'s backlog for
both call sites (`terminate_tp20_broadcast_periodic_for_suspension` and `CoptCancel`, unified by
round 11).

**Update (Codex review round 14, P2, PR #101): `CoptCancel`'s failed-native-stop branch now
treats `ERR_INVALID_MSG_ID` as already-cancelled instead of a genuine failure.** A channel-wide
clear (e.g. `CLEAR_PERIODIC_MSGS`) that natively clears this exact message's slot device-side
after `CoptCancel` has already taken the tracking entry off the link (`take()`) but before its own
`stop_periodic_message` call runs makes a conforming adapter return `ERR_INVALID_MSG_ID` — an
authoritative "this id no longer exists device-side" signal, not a transient failure. Before this
fix, `CoptCancel` treated every `stop_periodic_message` error identically: restore-or-leak-track
via the round-11 shared helper, then return a gRPC error, leaving the COP `Executing` forever and
turning any cancel retry into an unbounded loop against a message that will never again succeed a
real stop, since it is genuinely already gone. Fixed by checking for `ERR_INVALID_MSG_ID`
specifically and, on a match, skipping the restore/leak-track/error path entirely and falling
through to the ordinary `Cancelled` success path (removed from `primitives`, `PduCopstCancelled`
reported) instead — completing the same "an authoritative already-gone signal from a periodic/
repeat-message stop is success, not failure" pattern already established for
`retry_leaked_periodic_message_stops`, `retry_leaked_repeat_message_stops`, and the
`PDU_IOCTL_STOP_REPEAT_MESSAGE` handler (all `rpc_misc.rs`), rather than inventing a fourth,
divergent shape for the same situation. A genuine (non-`ERR_INVALID_MSG_ID`) native failure keeps
the round-11/round-13 restore-or-leak-track-then-error behavior unchanged.

**Update (Codex review round 15, P2, PR #101): `terminate_tp20_broadcast_periodic_for_suspension`
gets the identical `ERR_INVALID_MSG_ID` fix round 14 gave `CoptCancel`.** The same race applies
here: a `CLEAR_PERIODIC_MSGS` that wins `self.api`'s lock after this function's own caller has
already taken the tracking entry off the link, but before this function's own native
`stop_periodic_message` call runs, has already cleared the message device-side, which a
conforming adapter surfaces as `ERR_INVALID_MSG_ID` here too. Before this fix, this function
treated every `stop_periodic_message` error identically — restore-or-leak-track via the round-11
shared helper — which for this authoritative-already-gone code wrongly perpetuated tracking for a
message that no longer exists, leaving the COP reported `Executing` and blocking later periodic
starts until an explicit cancellation cleaned it up. Fixed the same way as round 14: on
`ERR_INVALID_MSG_ID`, skip the restore/leak-track path entirely and fall through to this
function's own ordinary success finalization (`PduCopstFinished`, via
`events::emit_terminal_if_live`) instead — the fourth call site of this exact pattern in this
crate, alongside round 14's `CoptCancel` fix and `retry_leaked_periodic_message_stops`/
`retry_leaked_repeat_message_stops`. A genuine (non-`ERR_INVALID_MSG_ID`) native failure keeps the
round-11/round-13 restore-or-leak-track behavior unchanged.

`PDU_IOCTL_CLEAR_TX_QUEUE` and `PDU_IOCTL_RESET` are deliberately left unable to affect a
broadcast periodic at all, per the governing rule above: `CLEAR_TX_QUEUE`'s own existing
executing-vs-queued distinction (it already excludes `executing_cop`/detached-tier-2 registrants)
already puts an executing COP like this one out of its reach, and `RESET`'s own scope
(`rx_buf`/held TX queue/filters/`tx_suspended_by_ioctl`) never included repeat-message or
tester-present periodic state either — the broadcast periodic follows the identical precedent,
not a new exception.

**3. `TP2_0_T_BR_INT` is wired as a new ComParam, `CP_TP20BroadcastInterval` (`0x80D1`),
routed to the native `CONFIG_TP2_0_T_BR_INT` (`0x8044`) `SET_CONFIG` parameter, default 20
(Table 77's own default).** `ComParamId::to_j2534_config_id` returns
`Some(j2534_0404::CONFIG_TP2_0_T_BR_INT)` for it, so it follows the ordinary generic
ComParam-to-`SET_CONFIG` pipeline rather than the passive-params' deliberately-bypassed shape.
`comparam_defaults.rs` seeds it to 20 for TP2.0; `comparam_support.rs` adds it to
`is_tp20_param`'s allowlist. **Update (Codex review round 8 Finding 2, PR #101):** this param
is deliberately classified per-channel, hardware-resident, but PACING-class — like the existing
`CP_Cs` (`PARAM_N_CS`) → `CONFIG_J1939_BRDCST_MIN_DELAY` precedent (`comparam_id.rs`, J1939
broadcast-timing translation) — never `PDU_PC_BUSTYPE`, even though it is per-channel and
hardware-resident the same way a BUSTYPE param is: it governs protocol TIMING (how often a
burst repeats), not bus configuration (baud rate, sample point, termination) the way
`DATA_RATE`/`BIT_SAMPLE_POINT` do. Codex review round 8 suggested reclassifying it as
`PDU_PC_BUSTYPE` (adding it to `comparam_support.rs`'s `BUSTYPE_UNUM32` array) so
`LOCK_PHYSICAL_COM_PARAMS` would gate it the same way; this was rejected, not adopted, because
`BUSTYPE_UNUM32` membership also drives `bustype_params_differ` — the guard that rejects a
`temp_param_update` staging a BUSTYPE-class Working/Active difference (ISO 22900-2
§9.4.16.2.1's own NOTE on the BUSTYPE class's `temp_param_update` prohibition). Adding this
param there would make that guard reject this ADR's own already-shipped, already-tested
`temp_param_update=1` per-burst override of `CP_TP20BroadcastInterval` (Decision item 2's Fix
5 / round 8's continuously-held-`api`-guard update below) — the reclassification Codex
suggested would silently break the feature this ADR exists to ship, not merely tighten its
locking. `comparam_support.rs`'s `com_param_class` already correctly reports `PduPcSpecified`
for this param (it is neither in `BUSTYPE_UNUM32` nor `TESTER_PRESENT_CLASS_PARAMS`) — no
change needed there; `comparam_support.rs` gained three regression-fence unit tests (round 8)
pinning this decision so a future well-intentioned "fix" does not silently re-attempt it.

**Accepted residual (round 8, design-advisor consult):** because `CP_TP20BroadcastInterval` is
deliberately not `PDU_PC_BUSTYPE`, `LOCK_PHYSICAL_COM_PARAMS` never gates it at all — on a
shared physical channel, a sibling CLL's own `CoptUpdateparam` can move the standing
`CONFIG_TP2_0_T_BR_INT` hardware value between this CLL's own broadcast bursts, and a
`Plain`-bound (non-`temp_param_update`) broadcast-periodic start uses whatever the standing
value happens to be at that moment, which may reflect a sibling's own promoted Active rather
than this CLL's. This is not a new gap this ADR introduces: it is the same behavior every other
non-BUSTYPE, `SET_CONFIG`-forwarded ComParam already has under ADR-110's "non-conflicting
params are still pushed to hardware even while `LOCK_PHYSICAL_COM_PARAMS` is held by another
CLL" design — `CP_TP20BroadcastInterval` simply joins that existing class rather than getting a
bespoke exception. A client that needs this specific burst's own interval isolated from a
sibling's concurrent updates has the existing `temp_param_update=1` per-burst override
(Decision item 2's Fix 5) available for exactly that purpose.

**Update (Codex review round 18, P2, PR #101): the `temp_param_update=1` per-burst override this
residual points to as the isolation mechanism was itself bugged for exactly the sibling-CLL
scenario it exists to isolate against.** Every `temp_param_update` bracket's revert
(`events::revert_hardware_to_live_active_locked`) reads THIS CLL's own `LogicalLinkState::active`
snapshot, strips `PDU_PC_BUSTYPE`-class keys (ADR-110), and pushes the remainder back to
hardware. Because `CP_TP20BroadcastInterval` is channel-wide/hardware-resident but deliberately
NOT `PDU_PC_BUSTYPE` (this Decision item), the BUSTYPE strip never excluded it — and `active` is
tracked PER-CLL, not per-channel. On two CLLs sharing one physical channel with different
`CP_TP20BroadcastInterval` values in their own respective `active`, CLL A's own temp-bound
bracket reverted to CLL A's own (possibly stale) `active` copy, clobbering the channel's real,
currently-live value — which could be CLL B's own already-promoted value — with CLL A's stale
one. This is the opposite defect from the round-8 residual above: that residual describes a
`Plain`-bound start correctly reading whatever the channel's real standing value is (a sibling
CAN legitimately move it, and a `Plain` start correctly observes that); this bug had a
`temp_param_update`-bound bracket's own REVERT incorrectly overwriting that real standing value
with a stale per-CLL copy instead of restoring it.

Fixed by capturing the channel's actual pre-bracket HARDWARE value (`GET_CONFIG`, native units)
for every channel-wide, non-`PDU_PC_BUSTYPE` key at apply time — while `api` is already held
continuously for the whole apply → ... → revert bracket (the same continuous-guard fences
Decision item 2's Fix 5/round 8 update and the round-17 update above already established) — and
restoring that captured value on revert instead of this CLL's own `active`. Correct per ADR-067's
revert-target rule (a live read at revert time, not a call-time-bound snapshot) because the
capture itself happens under the same continuously-held `api` guard the bracket already uses, so
nothing can interleave between the capture and the bracket's own temp apply — the capture is not
a stale snapshot the way a `StartComPrimitive`-call-time Working bind would be, it is a live
read taken as late as possible before the bracket's own hardware mutation. A new
`comparam_support::CHANNEL_WIDE_UNUM32` const (currently containing only
`CP_TP20BroadcastInterval`, disjoint from `BUSTYPE_UNUM32` by a regression-fence test) names this
classification explicitly, with a companion `strip_captured_channel_wide_keys` mirroring
`strip_bustype_keys`'s shape (see the correction paragraph below: this strips a key from the
`active` push only when a captured pair actually exists for it, not unconditionally);
`events::capture_channel_wide_hardware_locked` performs the
capture; `events::revert_hardware_to_live_active`/`_locked` gained a required
`channel_wide_restore: &[(u32, u32)]` parameter threaded through from every bracket's own capture
to every one of its own revert call sites (`handle_send_recv`'s continuous bracket, `rpc_
primitive.rs`'s TP2.0 broadcast-periodic-start continuous bracket, and `handle_start_comm`'s
split-bracket callers via a new `events::apply_params_to_hardware_capturing` helper that locks
`api` once, captures, then applies). The soft-ISO-TP arm of `handle_send_recv` passes `&[]`
(ADR-046 gating means software ISO-TP never coexists with TP2.0 on one channel, so
`CP_TP20BroadcastInterval` is never in play there).

**Residual not closed by this fix, accepted (reworded -- edge-case-hunter adversarial review, PR
#101, minor Finding 8: the original wording named specific racing mechanisms that cannot actually
interleave the way described):** `handle_start_comm`'s split-bracket callers cannot hold `api`
continuously from their own capture/apply all the way to their own (possibly much later, after
further native I/O) revert call sites, unlike the two continuous-bracket sites above -- narrower
than the pre-fix bug (the captured value is at least the correct value as of shortly before the
bracket started, not this CLL's own arbitrarily-stale `active`), but not airtight against a
change to the channel-wide value landing in that specific gap.

In practice, of the two mechanisms the original wording named as able to land in that gap: a
sibling CLL's own `CoptUpdateparam` on the same physical channel cannot actually do so at all --
`handle_start_comm` and `CoptUpdateparam`'s own handler (`handle_update_param`) are both
dispatched exclusively as `TxItem`s processed by that channel's single poll task
(`spawn_channel_poll_task`/`poll_channel_events`, `service.rs`), which `.await`s each dispatched
item to completion before pulling the next one off the queue -- so a `CoptUpdateparam` queued for
this same channel is already fully serialized behind `handle_start_comm`'s own entire run, not
racing any gap within it. A `Plain`- or `Temp`-bound broadcast-periodic start via
`rpc_start_com_primitive`, by contrast, runs directly in the RPC handler rather than through the
poll task (ADR-192 Decision item 2), so it genuinely can execute concurrently with an in-flight
`handle_start_comm` on the same channel -- but its own apply -> native-start -> revert bracket
holds `api` continuously for that whole sequence and always captures-then-restores whatever value
is live at the moment it runs, symmetrically with what `handle_start_comm`'s own split bracket
does. An interleave between the two is therefore self-correcting, not value-clobbering: each side
restores exactly what it itself captured, so no bracket can leave hardware on a value neither side
ever intended -- only the transient in-between value briefly differs from either side's own
final target, which is the same class of momentary intermediate state ADR-110's own amendment
already accepts for its analogous `handle_update_param` critical section. This residual is
therefore theoretical, not reachable as a value-corrupting race given today's design (flagged only
in case the single-poll-task-per-channel structure or the broadcast-periodic-start bracket's
continuously-held `api` guard changes in the future); closing the underlying non-continuous-guard
gap in `handle_start_comm` itself would still require restructuring it to hold `api` continuously
across its own much larger init sequence, judged out of scope for this fix regardless.

**Explicitly out of scope (unchanged by this fix):** two CLLs with genuinely different `active`
copies of `CP_TP20BroadcastInterval` and NO `temp_param_update` bracket involved by either one
remains plain last-writer-wins on the shared physical channel — identical to every other
non-`PDU_PC_BUSTYPE` param under ADR-110, and identical to the round-8 residual above. This fix
only changes what a bracket's own REVERT restores; it does not add any new gating, locking, or
cross-CLL coordination for an ordinary (non-temp) `CoptUpdateparam`/`Plain`-bound broadcast
periodic start.

## Consequences

- **A broadcast periodic re-trigger start rejects synchronously
  (`Code::FailedPrecondition`) rather than queuing when this CLL's TX
  dispatch is currently suspended** (`PDU_IOCTL_SUSPEND_TX_QUEUE`, a sibling
  CLL's held `LOCK_PHYSICAL_TX_QUEUE`, or `CP_SuspendQueueOnError` — ADR-123/
  ADR-147's `tx_suspended()`). This is a deliberate asymmetry from every
  ordinary transmitting ComPrimitive, which is accepted and queued in
  `tx_held`, then dispatched once the suspension clears (ISO 22900-2
  §9.4.13.3 use case 1): a native `PassThruStartPeriodicMsg` call has no
  `tx_held`-shaped "accept now, dispatch later" representation to defer
  into, and the alternative — starting it anyway — would put real broadcast
  traffic on the wire while a sibling CLL believes it holds exclusive
  transmit privilege over the shared physical resource, defeating
  `LOCK_PHYSICAL_TX_QUEUE`'s entire purpose. A client hitting this rejects
  and retries once the suspension clears; a single-shot broadcast burst
  (`num_send_cycles == 1`) is unaffected, since it dispatches through the
  ordinary `tx_queue`/`dispatch_tx_item` pipeline unchanged and is already
  correctly siphoned/held by the existing mechanism. **Update (Codex review
  round 3, design-advisor consult):** suspension taking effect on an
  ALREADY-RUNNING broadcast periodic now stops and finalizes it too (Decision
  item 2's governing rule) — rejecting only new starts while leaving a live
  one running under the identical suspension sources was an incoherent
  middle state. A client must treat a suspension as fatal to the beacon and
  re-arm (a fresh `CoptSendrecv`) after the suspension clears, not expect an
  automatic resume; `PDU_IOCTL_RESUME_TX_QUEUE` has nothing left to resume
  for this COP once suspension has terminated it, the same as any other
  `CoptCancel`-terminated COP.
- **Reintroduces the ADR-010 leak class for this one call site**, deliberately, as the
  necessary cost of using the only native primitive capable of the required wire behavior (see
  Decision item 2). `rpc_link.rs`'s teardown paths must cover it at every CLL-destroying and
  COP-cancelling site; test coverage for the shared-channel case specifically (mirroring
  ADR-010's own original scenario) is part of this stage's own test plan, not a residual left to
  a future ADR. **Update (Codex review round 3):** a shared-channel teardown whose native stop
  itself fails is no longer a bare log-and-drop — the `PeriodicMessageId` is preserved in a new
  `SharedChannel::leaked_periodic_message_ids`, mirroring `leaked_repeat_message_ids`'s own
  precedent exactly (ADR-165 Decision 6), so it keeps blocking a `LOCK_PHYSICAL_TX_QUEUE` grant on
  that physical resource and is retried/pruned the same way a leaked repeat-message slot is,
  until either the retry succeeds or the channel itself finally closes (`ref_count == 0`
  backstop, dropped with a debug log like every other leaked-id backstop in this codebase).
  Accepted residual: a stop that keeps failing (not just races once) leaves a genuinely
  transmitting stream on the shared resource until some later retry succeeds or the channel
  closes — the same bounded, same-process, device-authoritative residual `leaked_repeat_message_ids`
  already accepts for its own case.
- **ADR-093's Status line gains a partial annotation**: its "ADR-010 fully superseded" claim
  was correct for tester-present specifically (no periodic message is ever started there); it
  does not extend to this ADR's own, separate reintroduction of periodic-message usage for TP2.0
  broadcast re-trigger.
- **`CP_TP20BroadcastAddress` is this service's first per-send-scoped (not link-invariant)
  ComParam-derived TxFlags bit.** Every prior instance (`sci_tx_flags`/`sw_can_tx_flags`/
  `msg_priority_tx_flags`) resolves an objective, link-wide fact; this one resolves a
  per-message client intent, by design, via the existing `temp_param_update` mechanism rather
  than a new one. A future protocol needing the same shape (a per-send, not per-link, ComParam-
  expressed flag) has this as its precedent instead of re-deriving the `temp_param_update`
  route from scratch.
- **No per-cycle events for a broadcast periodic COP.** The native
  `PassThruStartPeriodicMsg`/`PassThruStopPeriodicMsg` pair gives this service the same
  zero-per-tick-visibility ADR-083 originally documented for tester-present mode 0: a single
  confirmation at start, nothing per actual transmission. Accepted for the same reason ADR-083
  accepted it originally — no cheaper alternative exists that reproduces the wire behavior (see
  Decision item 2) — and narrower in scope here, since TP2.0 has no `CP_P3Func`/`CP_P3Phys`
  gap-coordination requirement this loses relative to a software-dispatched alternative.
- **No live re-arm.** Unlike ADR-093's tester-present (which gained live re-arm as a deliberate
  benefit of software dispatch), a broadcast periodic COP started under this ADR cannot be
  live-reconfigured — a rate or address change requires cancelling and re-starting it. This is a
  direct consequence of using the native hardware-autonomous primitive rather than a software
  poll loop, not an oversight.
- **`num_send_cycles` values other than `-1` (infinite) or the single-shot broadcast burst are
  not supported for a broadcast `CoptSendrecv`.** A finite repeat count greater than the fixed
  five-frame burst has no native primitive to implement it against (only "burst once" and
  "burst once then repeat forever at a rate" exist) and is rejected as an invalid request.
- **The native adapter's own `TimeInterval` band (SAE J2534-1's 5-65535 ms) is enforced by the
  device, not synthesized here** — the same external-spec-knowledge reasoning ADR-083 already
  used for not hardcoding a range check; a rejected value surfaces through the native call's own
  failure.
- **RX-side loopback classification**: a broadcast frame's TX-side echo does not match either
  of Stage 7a's two established connection-routing tiers (`tp20_rx_id`/`tp20_tx_id`) — it must
  be classified and dropped explicitly rather than falling through to a sibling CLL whose own
  `tx_id` happens to coincide with the broadcast frame's address prefix, which is the same leak
  class ADR-188's Fixes N/P already closed twice for the connection-bound case. **Update (Codex
  review round 2, PR #101):** the first implementation classified this per-CLL inside
  `events_rx_routing.rs::UniqueRespIdKey::matched`, reinterpreting the frame's leading bytes as a
  4-byte `can_id` — which only ran when `route_frame` had a `can_id` to hand it at all, requiring
  the frame to be `>= 4` bytes. Since a broadcast's legal composed size is `3..=8` bytes, the
  minimum-size (3-byte) case always produced no extractable `can_id`, hit `route_frame`'s
  too-short-frame fallback (deliver unconditionally, no routing), and leaked to every sibling CLL
  — the classifier was dead code for exactly the case most likely to occur. Fixed by classifying
  and dropping the echo directly from the raw frame's first data byte in `events.rs::poll_rx_inner`,
  before per-CLL routing runs at all (mirroring the existing `CONNECTION_ESTABLISHED`/`_LOST`
  early-drop), independent of frame length; the now-unreachable per-entry classifier in
  `events_rx_routing.rs` was removed. Test coverage for this specific scenario (a broadcast burst
  alongside an active sibling connection sharing the physical channel), including the exact
  3-byte minimum-size case, is part of this stage's test plan.
- **`CP_TP20BroadcastInterval` is the only one of Table 77's ten TP2.0 timing parameters wired
  by any TP2.0 stage so far.** The other nine remain exactly as ADR-188 §4 deferred them
  (governing the device's autonomous connection-establishment machine, out of every TP2.0
  stage's scope to date) — this ADR does not revisit or narrow that deferral.
- **Mock scope**: `j2534-0404-mock` needs `Data[0]`-range validation (`ERR_INVALID_MSG` for a
  value outside `0xF0`-`0xFF` reaching the native call — defense in depth alongside this
  service's own pre-native-call rejection) and working `PassThruStartPeriodicMsg`/
  `PassThruStopPeriodicMsg` support for the broadcast case specifically (verify the mock's
  existing periodic-message support, unused since ADR-093 removed tester-present's own usage of
  it, was not itself gutted or bit-rotted in that change).
- **Update (Codex review round 4, PR #101): five further closures of the periodic-start
  mechanism's own edge cases, all in `rpc_start_com_primitive`'s `is_broadcast_send &&
  num_send_cycles == -1` branch.**
  - **Fix 1 (P1):** a native start that succeeds AFTER this cop_handle's own `None`-sentinel
    reservation was already taken/cleared by a concurrent `CoptCancel`/suspension-termination/
    `CLEAR_PERIODIC_MSGS`/teardown (the race the `None`-sentinel reservation's own doc comment
    already flagged as an accepted residual, round 3) previously left the just-started native
    message permanently orphaned — correctly not resurrecting a possibly-already-cancelled COP's
    tracking, but also never stopping the message it raced ahead of. Closed with a best-effort
    `stop_periodic_message` in that case, issued after releasing `logical_links` (this mechanism's
    existing lock-ordering discipline), mirroring `CoptCancel`'s own best-effort native-stop-
    failure handling. A stop failure here is a rare double-fault with no channel-scoped
    leak-tracking available at this point (adding one is out of scope) — the residual this closes
    is narrower, not eliminated; see `j2534-0404-service/docs/implementation-notes.md`'s extended
    accepted-residual entry.
  - **Fix 2 (P1) + Fix 3 (P1):** the suspension check, the already-active check, and the
    reservation write were three separate `logical_links` critical sections; merged into one
    (`reserve_tp20_broadcast_periodic`), closing a TOCTOU window between them. The same critical
    section now also re-verifies `connect_generation`/`connected` against the `LinkView` snapshot
    captured earlier in the call (mirroring this function's own earlier ADR-086 `connect_generation`
    re-check) and reads `channel_id`/`hw_protocol_id` from the LIVE `LogicalLinkState`, never the
    stale snapshot — closing the gap where a disconnect (and possibly reconnect onto a different
    physical channel) landing between the snapshot and the reservation could have let a stale
    `channel_id` reach the native call.
  - **Fix 4 (P2):** the raw out-of-range `CP_TP20BroadcastAddress` rejection (Decision item 1) was
    enforced only on `CoptSendrecv`'s own send path; `CoptStartcomm`'s and `CoptStopcomm`'s own
    optional-message paths reach the identical `resolve_send_recv_tx` pipeline but were missing
    the same guard, letting a garbage nonzero value silently fold to "no broadcast"
    (`ComParamSet::tp20_broadcast_address`'s own by-design fallback) and send as an ordinary
    connection-bound frame. Factored into a shared `validate_tp20_broadcast_address_range` helper,
    applied at all three call sites.
  - **Fix 5 (P2):** a `temp_param_update`-bound broadcast periodic start bypassed
    `handle_send_recv`'s "temp-apply Working to hardware for one TX, then revert to live Active"
    bracket (ISO 22900-2 §9.4.3) entirely, since it never goes through that dispatch path
    (Decision item 2's own point). A client staging a hardware-backed TP2.0 physical ComParam
    (`CP_TP20BroadcastInterval` — the one TP2.0-family param actually routed to native
    `SET_CONFIG`, Decision item 3) into Working alongside `temp_param_update=1`, expecting it
    applied for just this one start the same as any other physical param under ADR-067, had it
    silently never pushed. The native periodic-message start is itself exactly ONE
    service-initiated action (its own effects continuing autonomously device-side afterward
    doesn't change that), so the identical apply-then-revert bracket is now run around it too —
    not a new "revert at COP-end" design. **Update (Codex review round 8 Finding 1, PR #101):**
    the first implementation of this bracket still acquired and released `self.api` separately
    for each of the apply, the native `start_periodic_message` call, and the revert — three
    independent `self.api.lock().await` acquisitions with real gaps between them, letting a
    concurrent operation on the same physical channel (a sibling CLL's own temp-bound broadcast
    start, or a queued `CoptUpdateparam`) interleave and corrupt which hardware value the burst
    actually transmits with. Fixed by acquiring `self.api` ONCE, before the apply step, and
    holding it continuously through the apply, the native call (and the round-5 periodic-clear-
    epoch read immediately after it, which already had to stay under the same guard), and the
    revert, on every exit path (apply failure, native-start failure, native-start success) —
    dropping it only afterward, before touching `self.logical_links` for its own `last_error`
    read, calling `rollback_tp20_broadcast_periodic_reservation`, returning an `Err`, or calling
    `finalize_or_orphan_broadcast_periodic_start` (which itself separately re-acquires both
    `self.logical_links` and `self.api`). This is safe under the ADR-110 amendment's own
    lock-ordering invariant ("Lock-grant/apply serialization", Finding 2: `api` outer,
    `logical_links` inner whenever both are held together) — reading `logical_links` (for the
    revert's own live-Active lookup) while `api` is already held is the sanctioned order, not the
    forbidden reverse one; `handle_update_param` (`events.rs`) is the established precedent for
    this exact nesting. `events.rs::apply_params_to_hardware_locked` (already existing, for
    `handle_update_param`'s own analogous bracket) was made `pub(super)` and reused directly, and
    a new `revert_hardware_to_live_active_locked` was added as the `_locked` counterpart of the
    existing `revert_hardware_to_live_active` wrapper (mirroring the existing
    `apply_params_to_hardware`/`apply_params_to_hardware_locked` pairing) — the revert still
    performs a LIVE read of `logical_links` at each revert point, never a value pre-fetched before
    the bracket, preserving `revert_hardware_to_live_active`'s own existing ADR-067 contract (a
    sibling CLL's own `CoptUpdateparam` can legitimately promote a different CLL's Active during
    this window, and the revert must observe that, not a stale snapshot). No literal concurrent
    interleaving repro was attempted for the fix's own regression test, for the same reason the
    ADR-110 amendment accepted for its analogous `handle_update_param` fix: this crate's
    single-threaded `current_thread` test runtime has no production yield hook to force the
    interleaving deterministically; the regression test instead pins the bracket's steady-state
    log-adjacency shape (`tests/grpc_mock/tp20.rs`).
- **`terminate_tp20_broadcast_periodic_for_suspension`'s and `CLEAR_PERIODIC_MSGS`'s own
  broadcast-periodic COP finalization (Decision item 2's suspension terminator and
  `CLEAR_PERIODIC_MSGS` cell) now route through the crate's existing `emit_terminal_if_live`
  helper (`edge-case-hunter` follow-up, PR #101) instead of a hand-rolled
  remove-then-`send_cop_status`.** The hand-rolled version dropped the `primitives` guard before
  the `send_cop_status` `.await`, reopening the exact A2-23 race that helper exists to close (a
  concurrent `CancelComPrimitive`/`GetStatus` landing in that gap sees a miss on both
  `primitives` and `terminal_cops`, and wrongly reports the cop_handle as never having existed),
  and never drained this cop_handle's own stale `cancelled_cops` mark the way
  `emit_terminal_if_live` also does — a `CancelComPrimitive` racing a suspension-termination
  could leave a mark nothing would ever drain, since a broadcast-periodic COP never reaches the
  ordinary `tx_queue`/`dispatch_tx_item` path that mark is meant for. `emit_terminal_if_live` was
  widened from `pub(super)` (visible only within `events` and its submodules) to
  `pub(in crate::service)` (matching `send_cop_status`'s own existing visibility) so `rpc_misc.rs`
  can call it directly. No dedicated regression test for the race itself: constructing the true
  concurrent interleaving is infeasible with this crate's established test techniques, the same
  narrow-window-race limitation `rollback_stop_comm_pending_tests` already documents for an
  analogous case — the fix is a mechanical routing through an already-tested shared helper, not a
  new mechanism needing its own coverage.
- **Update (Codex review round 17, P1, PR #101): the same split-bracket clobber race Fix 5's round
  8 update closed for the TP2.0 broadcast-periodic-start path also existed on `handle_send_recv`
  itself** — the general single-shot/cyclic `CoptSendrecv` dispatch path every hardware-transport
  send (TP2.0 or otherwise) goes through. Before this fix, a `ParamBinding::Temp` cycle applied its
  bound Working snapshot via `apply_params_to_hardware` (which locks-then-releases `ctx.api`
  internally), then — much later, after `wait_for_p3_gap`'s own wait/poll loop — reacquired
  `ctx.api` separately for the actual native transmit, with a real gap in between where a sibling
  CLL's own temp-bound bracket on the same physical channel (e.g. another CLL's own
  `CoptSendrecv`, or a TP2.0 broadcast-periodic start via the Fix 5/round-8 bracket above) could
  apply, use, and revert its own value — leaving this cycle's transmit using the sibling's
  reverted Active value instead of its own bound Working value. This became wire-visible once
  `CP_TP20BroadcastInterval`/`CONFIG_TP2_0_T_BR_INT` was wired to hardware (Decision item 3,
  above).
  - **Scope: `isotp_tx.is_none()` cycles only** (every hardware-transport protocol, including
    TP2.0 connection-mode and broadcast framing) — not every `ParamBinding::Temp` send
    unconditionally, and not narrowed to `tp20_is_broadcast` alone. Design-advisor consult:
    unconditional-for-every-Temp-send is infeasible because `wait_for_p3_gap`'s own wait loop
    polls RX internally (locks `ctx.api`) and the software-ISO-TP driver (`isotp_send`,
    `isotp_tx.is_some()`, ADR-046) does per-frame writes and FlowControl waits that each lock
    `ctx.api` internally too — holding one continuous guard across those would self-deadlock (the
    same task re-locking the same mutex it is already holding) or need a much larger refactor of
    the soft-ISO-TP driver. `tp20_is_broadcast`-only scoping would be too narrow: the real hazard
    is the ADR-067 Working/Active contract for ANY hardware-mapped temp param under
    `isotp_tx.is_none()`, not specifically broadcast framing — a TP2.0 connection-mode temp-bound
    send has the identical race. `isotp_tx.is_none()` is the correct scope because every such
    cycle's entire native transmit is one bounded `write_messages` call, so holding `ctx.api`
    continuously across apply → write → revert for this arm is cheap and safe.
  - **Fix shape:** `wait_for_p3_gap` and the pre-transmit staleness/J1939-claim-drift/TP2.0-
    connection-drift recheck (both already existed; formerly run AFTER the apply) now both run
    BEFORE it instead — P3 is a MINIMUM inter-request gap, so transmitting later than its minimum
    remains spec-conformant. A cycle this gate rejects (`Cancelled`/`HardError` from the gap wait,
    or a stale/drifted result from the recheck) now never applies anything to hardware at all, so
    no revert is owed either — an `applied` flag tracks whether the apply actually ran, since
    ADR-067's revert obligation only attaches once it did; before this fix the apply ran
    unconditionally, ahead of this whole gate, so every rejection still paid for an apply-then-
    revert round trip that used nothing. For `isotp_tx.is_none()`, `ctx.api` is acquired ONCE and
    held continuously across `apply_params_to_hardware_locked` → (only if the gate cleared the
    cycle to transmit and the apply itself succeeded) the native write, via a new
    `transmit_request_locked` helper mirroring the `apply_params_to_hardware`/
    `apply_params_to_hardware_locked` naming pairing → `revert_hardware_to_live_active_locked` →
    drop the guard — exactly mirroring the round-8 bracket shape above. `transmit_request`'s own
    post-hoc `last_bus_activity` bookkeeping (ADR-083) is replicated manually, outside the held
    guard, since this arm bypasses `transmit_request` itself. A receive-only cycle
    (`send_cycles_remaining == 0`, ADR-059) still goes through the same single-guard
    apply/(skip write)/revert bracket, unconditionally, same as before this fix — only the gate
    (gap wait + drift recheck) is skipped for it, since there is nothing to gap-wait or
    drift-check when nothing will transmit.
  - **`isotp_tx.is_some()` (software ISO-TP) residual, accepted and deliberate:** this arm keeps
    the OLD split apply → `isotp_send` → revert shape completely unchanged, each step
    independently locking `ctx.api`, per the infeasibility above. There is no constructible
    instance of this race today for this arm: software ISO-TP and TP2.0 never coexist on the same
    channel (ADR-046's own CAN dual-channel-mode gating), so a software-ISO-TP cycle's split
    bracket can never actually race a TP2.0 sibling's own temp-bound bracket on the same physical
    channel. This residual is recorded here rather than silently left unmentioned; closing it
    fully would require a much larger refactor of the soft-ISO-TP driver's own internal locking,
    which is out of scope for this fix.
  - **Test coverage:** `tests/grpc_mock/pin_selection.rs`'s pre-existing
    `iso9141_ps_temp_param_update_sendrecv_reverts_tidle_to_active`/`..._applies_tidle_com_param`
    regression-pin the ordinary (non-racing) single-shot `handle_send_recv` apply/revert path and
    continue to pass unchanged after this reordering, confirming it did not change externally
    observable behavior for that case. Two new direct unit tests
    (`events_handle_send_recv_api_fence_tests.rs`) drive `handle_send_recv` directly against a
    hand-built `ChannelPollCtx`, using this crate's established "test task holds `ctx.api` while
    the code under test runs as a spawned task on the `current_thread` runtime" fence technique
    (mirroring `rpc_primitive.rs::tp20_broadcast_periodic_api_fence_tests` and the round-16
    hard-error fence tests): `temp_bound_cycle_makes_no_progress_while_a_sibling_holds_the_fence`
    pins the simpler "no partial progress while a sibling already holds `ctx.api`" property, but
    that alone does not discriminate this fix from the pre-fix split-bracket shape (the old code
    also blocked entirely on `ctx.api` for its own apply step, so it passes the same test
    unchanged — edge-case-hunter finding, round 18). The property that actually discriminates the
    fix is proven by `temp_bound_cycle_excludes_a_racing_sibling_from_the_apply_write_gap`: with a
    racing sibling task deterministically queued BEHIND the cycle's own first `ctx.api`
    acquisition (parked on a contended `tokio::sync::Mutex`, which grants its permit to
    already-queued waiters in FIFO order), that sibling's own distinguishable native `SET_CONFIG`
    call can never land between this cycle's own apply and revert batches — only strictly after
    both — proving the whole apply → native write → revert bracket is provably inseparable from a
    single `ctx.api` acquisition, so nothing else touching `ctx.api` (a sibling's own bracket
    included) can ever interpose inside it. Verified to discriminate: temporarily simulating the
    old split-bracket's release-and-reacquire gap (a drop-then-relock of the `api` guard between
    the apply and the native write) makes this second test fail — the sibling's marker lands
    between the apply and revert entries instead of after both — while the first test still
    passes unchanged under the same simulated gap, confirming only the second test actually pins
    the fix.
