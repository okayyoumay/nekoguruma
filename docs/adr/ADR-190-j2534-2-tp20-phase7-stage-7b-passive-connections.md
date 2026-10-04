# ADR-190: SAE J2534-2 TP2.0 (Phase 7, Stage 7b — Passive Connections)

**Date:** 2026-08-24
**Status:** Accepted (its own "no client-visible establish/loss event" residual resolved, by
             decision, by ADR-191)
**Affects:**
- `j2534-0404-service/src/service/service_params.rs`
- `j2534-0404-service/src/service/comparam_support.rs`
- `j2534-0404-service/src/service/comparam_id.rs`
- `j2534-0404-service/src/service/events.rs`
- `j2534-0404-service/src/service/events_tp20_connection.rs`
- `j2534-0404-service/src/service/rpc_link.rs`
- `j2534-0404-service/src/service.rs`
- `j2534-0404-mock/src/lib.rs`
- `j2534-0404-service/docs/implementation-notes.md`
- `docs/j2534-2-support-plan.md`
- `docs/j2534-0404-architecture.md`

## Context

[ADR-188](ADR-188-j2534-2-tp20-phase7-stage-7a-active-connections.md) (Stage
7a, Active Connections) explicitly deferred this stage's own design:
"Stage 7b's passive-connection client-surface design is explicitly deferred
and unresolved by this ADR — do not treat its two ComParams' mere existence
in the header as authorization to wire them without a fresh design-advisor
round." This ADR is that round.

SAE J2534-2 clause 19.3.1 requires the interface to support exactly one
inbound-accepting ("passive") connection, individually configurable and
disableable, counting as one of the shared four-connection total (not a
fifth — the same correction ADR-188 already made for the active-connection
count). Table 77 defines two `SET_CONFIG` parameters governing it, both
defaulting to 0 (disabled): `TP2_0_IDENTIFER` (0x0 or 0x200-0x2EF, the CAN ID
the adapter uses to recognize an inbound connection request addressed to it)
and `TP2_0_RXIDPASSIVE` (0x0 or 0x300-0x7FF, the CAN ID that becomes the
passive connection's own RX-ID once established). Clause 19.3.3.1 describes
the mechanism: setting `TP2_0_IDENTIFER` to a valid nonzero value implicitly
installs a PASS filter (cleared when set back to 0); once both parameters
hold valid nonzero values and an inbound connection request arrives from the
network, the adapter accepts it automatically, device-side, with **no
application-initiated `REQUEST_CONNECTION` call at all** — unlike an active
connection, where the application always issues that IOCTL itself. A full
interface (all four slots occupied) or an unset `TP2_0_RXIDPASSIVE` causes
the network-side request to be rejected with reason `0xD8`, the same reason
byte Stage 7a already maps to `PduErrResourceBusy`; this network-side
rejection never reaches the application as an indication.

The resulting acceptance is signaled via the exact same `CONNECTION_
ESTABLISHED`/`CONNECTION_LOST` RxStatus-bit-16/17 indications (Table 81)
active connections already use — `Data[0..3]` is the receiving address,
`RX-ID-P` for a passive connection instead of `RX-ID-A`, with an otherwise
identical shape. `TEARDOWN_CONNECTION` (clause 19.3.3.3, Table 79) is
generic over "the CAN address this connection receives on" and Table 81
explicitly names `RX-ID-P` in `CONNECTION_LOST` too — teardown of an
established passive connection is spec-available, unlike `REQUEST_
CONNECTION`, which has no passive-side equivalent at all.

The central design question: since `TP2_0_IDENTIFER`/`TP2_0_RXIDPASSIVE` are
per-physical-channel `SET_CONFIG` parameters (not a per-request payload like
active `REQUEST_CONNECTION`'s), and clause 19.3.1 allows exactly one passive
slot per interface, not per-CLL — how does the frozen D-PDU CLL/COP model
(one CLL = one connection, established via `PDU_COPT_STARTCOMM`, per Stage
7a's own Decision, with no new IOCTL or proto surface per
[ADR-178](ADR-178-freeze-j2534-2-dpdu-interface.md)'s freeze) represent
"listen passively for an unsolicited inbound connection," given the
established connection is entirely unsolicited from the application's own
perspective?

## Decision

**One CLL arms the interface's single passive slot via its own
`CoptStartcomm`, which completes successfully immediately once armed
("arm-and-complete") — it does NOT wait for the first inbound connection.**
Establishment happens asynchronously afterward, through the existing RX
indication path, against a routing-map entry that (unlike an active
connection's) stays registered across repeated accept/lose cycles rather
than being removed once resolved.

**Arm-and-complete is forced by this codebase's own concurrency
architecture, not a stylistic choice.** Each physical channel has exactly
one poll task that processes every queued `TxItem` — including a TP2.0
connection request's entire bounded wait — to completion before dequeuing
the next (`events_tp20_connection.rs`'s own module doc). Stage 7a's
active-connection wait tolerates this because it is bounded to ~2s,
spec-derived (`CP_TP20T_E × (CP_TP20MNTC + 1)`). A passive listen has no
spec-derived bound at all — the peer may connect seconds, hours, or never —
so a `CoptStartcomm` that waited for the first inbound connection would
starve every sibling CLL's sends and `CoptStartcomm`s on the same physical
channel indefinitely: e.g. CLL A arms passive and waits; sibling CLL B
(already `Established` on an active connection) queues a `CoptSendrecv` —
it never dispatches until A's peer connects, if ever. This alone rules out
a waiting model regardless of any cancellation mechanism layered on top.

### 1. ComParam surface

Two new **minted** ComParams (no ISO 22900-2 source, the `CP_TP20*`
Stage-7a naming/id-range convention): `CP_TP20PassiveIdentifier` (`0x80CE`)
and `CP_TP20PassiveRxId` (`0x80CF`) — the next-free ids after Stage 7a's
`PARAM_TP20_APPLICATION_TYPE` (`0x80CD`). Both `to_j2534_config_id → None`
(service-level-only), **deliberately not routed through the generic
ComParam→`SET_CONFIG` pipeline** despite the native `CONFIG_TP2_0_IDENTIFER`/
`_RXIDPASSIVE` (`0x804E`/`0x804F`) constants already existing: if they were,
ordinary ComParam application (connect-time, or a `CoptUpdateparam` on an
already-active link) would arm or re-arm the device-side listener outside
this ADR's arm/disarm lifecycle and outside its exclusivity gate below.
Instead, `handle_start_comm`'s new passive arm issues the two native
`SET_CONFIG` calls itself, exactly when arming.

Selection at `CoptStartcomm` time: either passive ComParam staged routes to
the new passive arm; both must then be staged, in-range (`0x200-0x2EF` /
`0x300-0x7FF` — a staged 0 is meaningless, since not staging them at all
already means "don't arm passive"), and staging any of Stage 7a's five
active-connection ComParams (`CP_TP20ChannelSetupCanId` etc.) alongside a
passive one is rejected as ambiguous intent. Validated service-side at
`CoptStartcomm` time (mirroring Stage 7a's own `resolve_tp20_connection_
params` shape), with native `ERR_INVALID_IOCTL_VALUE` as the backstop the
same way Stage 7a's own params rely on it. Stage 7a's Fix Q dispatch-time
`comm_started` recheck applies to this arm identically (a second concurrent
`CoptStartcomm` racing the first must still be rejected, not overwrite it).

### 2. Exclusivity

New field `SharedChannel::tp20_passive: Option<Tp20PassiveSlot>`
(`Tp20PassiveSlot { cll_handle: u32, connect_generation: u64, identifier:
u16, rx_id_passive: u16 }`), beside the existing `tp20_connections` map,
under the same `shared_channels`-outermost lock discipline (ADR-080).
Arming requires `tp20_passive.is_none()`; a second CLL's own passive-arm
attempt on the same channel while one is already armed is rejected with the
existing `PduErrResourceBusy` shape — the same shape the 0xD8 reason byte
already maps to for a network-side rejection, keeping "the passive slot is
a resource, and it is busy" one consistent error semantics regardless of
which side detected the conflict.

**[ADR-189](ADR-189-j2534-2-gm-uart-phase8.md)'s `become_master_in_flight`
reservation-flag pattern does not transfer here, and copying it would be
the wrong shape, not merely an unnecessary one.** That flag guards a bounded
~2s in-flight native call and *rejects a sibling's channel-join* for that
window. Here, the exclusive state is held for the listener's entire armed
lifetime (unbounded), and clause 19.3.1 explicitly requires active
connections to keep working alongside the passive one (four total, not a
choice between them) — sibling joins and active `CoptStartcomm`s must
continue succeeding while a CLL holds the passive slot armed. A persistent
owner-token `Option`, not an in-flight `AtomicBool` gating joins, is the
correct shape.

Enforcement is per-`SharedChannel` (per physical channel); since no
`TP2_0_CHx` exists yet, this is equivalent to per-interface today. Revisit
if `TP2_0_CHx` Additional Channels are ever built (deferred, matching every
other phase's own `_CHx` deferral precedent).

### 3. Routing, establishment, and re-listen

Arming happens under one `shared_channels` critical section (mirroring
`run_tp20_connection_request`'s own shape):

1. Check `tp20_passive.is_none()`.
2. Check `tp20_rx_id_unavailable_for(rx_id_passive, ...)` — an abandoned
   quarantine entry or a pending sibling active request on the same
   `rx_id_passive` blocks arming, rejected with the existing `RxIdInUse`
   shape.
3. **Additionally scan live CLLs on the channel for one whose own
   `tp20_connection` claims `Requested`/`Established` on `rx_id_passive`,
   and reject if found.** An active connection's own routing-map entry is
   removed once it establishes, so step 2's map check alone cannot see an
   already-established occupant of that RX-ID — without this scan, CLL A
   active-established on RX-ID 0x350 would not stop CLL B from arming
   passive with `TP2_0_RXIDPASSIVE = 0x350`, creating a duplicate-RX-ID
   routing ambiguity nothing else in this mechanism prevents.
4. Issue the two native `SET_CONFIG` calls.
5. Insert a **persistent** `Tp20ConnEntry { passive: true, abandoned: false,
   expect_stale_lost: false, .. }` (new `passive: bool` field, `false`
   everywhere else) keyed on `rx_id_passive` — deliberately never removed
   once inserted while the CLL holds the slot armed, breaking the existing
   "entry removed once resolved" invariant for passive entries only
   (adopting instead the same retain-for-session shape
   `SharedChannel::j1939_claims` already uses,
   `events_tp20_connection.rs`'s own module doc already contrasts this
   against).
6. Set `LogicalLinkState::tp20_connection = Some(Tp20Connection {
   requested_rx_id: rx_id_passive, established_tx_id: None, phase:
   Listening, passive: true })` (new `Tp20ConnectionPhase::Listening`
   variant; new `passive: bool` field on `Tp20Connection`).
7. Complete `CoptStartcomm` successfully.

`deliver_tp20_connection_indication` gains a passive delivery arm, checked
after the existing abandoned/stale-swallow checks (a quarantined passive
entry drains through the existing abandonment machinery unchanged,
including the late-`Established` re-teardown path — see §4). For a
non-abandoned `passive` entry: verify owner CLL liveness and
`connect_generation`, then write `LogicalLinkState` directly (the same
direct-write shape `reconcile_live_established_cll`'s own no-live-wait
write-back already uses, since no wait loop reads a passive entry's own
outcome the way `run_tp20_connection_request` does for an active one):

- `Established(tx_id)` → phase `Established`, record `established_tx_id`.
- `Lost(reason)` → phase back to **`Listening`** (not a terminal `Lost`),
  clear `established_tx_id`. Per clause 19.3.3.1, with both `SET_CONFIG`
  params still valid and a slot free, the device auto-accepts the *next*
  inbound request too and will emit a fresh `Established` indication for
  it — the persistent entry (step 5) is what lets that next indication
  route correctly. A terminal `Lost` phase, or removing the entry on loss,
  would strand every subsequent accept in the round-22 no-match reconcile
  path (ADR-188 Consequences) instead.

Do **not** run Fix AA's `reconcile_live_established_cll`/`expect_stale_lost`
logic at arm time. That mechanism exists because a successful native
`REQUEST_CONNECTION` is itself proof (per clause 19.3.3.2) that the device
no longer considers the target RX-ID occupied — the passive arm issues no
such call, so no equivalent proof exists, and flipping a sibling's belief
without it would be unfounded.

Once `Established`, TX/RX for a passive connection reuse Stage 7a's
mechanism verbatim — the same `Tp20Connection` fields drive `tx_header.rs`
(Fix W's magnitude-based `TX_EXTENDED_ID` rule holds unchanged: TX-ID-P
arrives as a 4-byte value and may exceed the 11-bit CAN ID range) and
`events_rx_routing`'s `tp20_rx_id` matching. A passive connection's client
surface is therefore identical to an active connection's own, once
established.

### 4. Disarm / teardown

`handle_stop_comm`, plus both existing `DestroyComLogicalLink`/
`DisconnectComLogicalLink` cleanup sites (`rpc_link.rs`), gain a passive
branch, gated on the CLL owning `tp20_passive` (matching `cll_handle` and
`connect_generation`):

1. Best-effort `SET_CONFIG(TP2_0_IDENTIFER = 0)`, then `SET_CONFIG(TP2_0_
   RXIDPASSIVE = 0)` — **in this order, first**, before anything else.
   Clearing native config before touching service-side state stops the
   device from accepting any further inbound connections; reversing the
   order would open a window where the device accepts a fresh, now-orphan
   connection between service-side teardown and the native config actually
   clearing.
2. If the entry's phase is `Established`, best-effort `TEARDOWN_CONNECTION`
   keyed on `rx_id_passive` (spec-available per Table 79/81, unlike active
   `REQUEST_CONNECTION`). If a real device rejects this as active-only
   despite the spec's generic wording, the slot self-heals via clause
   19.3.1's own mandatory maintenance timeout — the same accepted-residual
   class ADR-188 already documents for a leaked active connection slot.
3. **Unconditionally** mark the persistent entry `abandoned = true` in
   place — never gated on step 2's own result. This directly reuses ADR-188
   rounds 19-21's own conclusion (Consequences): an accept or a spontaneous
   device-side loss can race a disarm exactly as it can race an active
   connection's own teardown, so no synchronous call result may safely
   decide whether quarantining is skippable.
4. Clear `SharedChannel::tp20_passive`.

A delayed `Established` indication landing on the now-abandoned entry
re-tears-down and holds the quarantine until the follow-up `Lost` drains —
the existing Fix O/S/L machinery (ADR-188 Consequences) already covers this
exactly, with no passive-specific extension needed.

**Correction (Codex review finding, P1, PR #99; design-advisor consult):**
step 3's "unconditional, never released except by an indication" rule is
correct for a disarm that already reached `Established` (step 2 issues a
real `TEARDOWN_CONNECTION`, whose eventual confirmation — or clause
19.3.1's own maintenance-timeout loss — releases the quarantine the
ordinary way). It is WRONG for a disarm that is still `Listening` (never
accepted a connection): step 2 issues no native call at all in that case,
so nothing will *ever* deliver a follow-up indication to release the
quarantine — permanently leaking `rx_id_passive` on the shared physical
channel for as long as it stays open, in the ordinary (not merely raced)
case of arming and later disarming without anything ever connecting.

Two naive fixes were considered and rejected as unsafe: releasing the
entry immediately on `phase == Listening` reopens the exact
raced-accept misattribution `disarm_races_an_inbound_accept_and_leaves_
the_entry_quarantined` already proves is real (an autonomous device-side
accept can land, with its `CONNECTION_ESTABLISHED` indication still
undrained, microseconds before a disarm observes a stale `Listening`
phase); and issuing an unconditional best-effort `TEARDOWN_CONNECTION`
on every `Listening` disarm and gating release on *that* call's own
synchronous result is equally unsafe — a synchronous failure cannot
distinguish "nothing was ever accepted" from "an accept-then-
spontaneous-loss cycle already completed device-side, with BOTH a
still-queued `Established` and a subsequent `Lost` racing the disarm" —
exactly the class of synchronous-result-gating ADR-188 rounds 19-21
already proved unsafe for the identical reason (Consequences).

The resolution rests on an asymmetry between the two phases: for an
active connection's `Requested` phase, a real native call
(`REQUEST_CONNECTION`) is always already in flight, so there is no sound
place to stop waiting. For a `Listening` passive slot, once the native
config-clear itself succeeds, clause 19.3.3.1's own accept condition
(both `TP2_0_IDENTIFER`/`TP2_0_RXIDPASSIVE` must be nonzero) means the
device can never generate a **new** indication for this listener again —
whatever will ever arrive (a raced accept's `Established`, or a full
`Established`+`Lost` pair) is already sitting in the native RX queue, and
J2534-1 clause 7.2.5's in-order-read guarantee (the same ordering proof
ADR-188's own Fix AA residual already rests on) means one exhaustive
drain past the disarm moment is guaranteed to flush all of it.

This is precisely the soundness condition this codebase's own ADR-101
Decision §E "drain watermark" mechanism (`ctx.drain_watermarks`,
`is_cyclic_reap_sound`) already encodes and has proven correct for an
unrelated reap (a cyclic COP registrant's own deadline expiry): a stale
watermark only ever *defers* a decision, never wrongly *permits* one.
Reused here rather than re-derived:

- `best_effort_disarm_tp20_passive_native_config` returns `config_cleared:
  bool` (`true` if either `SET_CONFIG` call succeeded — either alone
  already kills the both-nonzero accept condition; both configs are still
  cleared regardless).
- `Tp20ConnEntry` gains `idle_release_at: Option<tokio::time::Instant>`.
  `quarantine_tp20_passive_slot_on_disarm` gains `was_established: bool,
  config_cleared: bool` parameters; when `!was_established &&
  config_cleared`, stamps `idle_release_at = Some(now +
  TP20_PASSIVE_IDLE_RELEASE_GRACE)` (a new named const, 500 ms — margin
  for real device-side latency between an accept event and its indication
  becoming queue-readable; the mock queues synchronously, so mock-driven
  test correctness rests on the drain condition alone, not the grace
  period). Otherwise `None` — the pre-existing indefinite-quarantine
  behavior, which stays correct when the config-clear itself failed (the
  device may genuinely still be able to auto-accept on this RX-ID).
- A new poll-task sweep, called from `run_due_tick_duties` alongside
  `reap_expired_cyclic_registrants` (same watermark-read-before-lock
  pattern, ADR-080-safe for the identical reason), removes every
  `tp20_connections` entry that is `abandoned && passive &&
  idle_release_at.is_some_and(|dl| watermark >= dl)`. The release
  predicate is extracted as a pure, sync, directly-unit-testable function,
  matching this file's own `is_cyclic_deadline_expired`/`is_cyclic_reap_
  sound` precedent.
- The abandoned-branch `Established` re-teardown arm (`deliver_tp20_
  connection_indication`) clears `idle_release_at` back to `None` when it
  keeps the entry — after a re-teardown, release must again wait for the
  follow-up `Lost` (the existing round-14/15 rule), not the drain barrier.
  The `Lost` arm removes the entry regardless, as today.
- **Companion fix, a latent gap this new release makes dangerous rather
  than merely theoretical:** the passive delivery arm's own
  still-current-CLL recheck can silently drop an indication on mismatch —
  a `Destroy`/`Disconnect` disarm (which holds `shared_channels` across
  its whole sequence) can land between that delivery's initial snapshot
  and its recheck. A dropped `Established` would mean the marker-clear
  above never runs, and the sweep could then release an RX-ID whose
  native connection is genuinely established. Fixed by retrying the
  delivery once against a fresh snapshot on a recheck mismatch — the
  fresh pass correctly routes to the abandoned branch (re-teardown,
  marker clear, hold).

Accepted residuals (documented, not solved): a device whose own latency
between a pre-disarm accept and that indication becoming queue-readable
exceeds the 500ms grace reopens a narrow misattribution window — the
same conformance-dependence class Fix AA's own residual already accepts
(ADR-188 Consequences), unverifiable against real (unavailable)
hardware; both config-clear calls failing degrades to the pre-existing
indefinite quarantine, which is the *correct* behavior in that case, not
a residual; and a continuously RX-backlogged physical channel defers
release indefinitely, the same benign "a stale read only defers" property
ADR-101 §E already accepts elsewhere. The early-release regression this
mechanism guards against is timing-infeasible to construct end-to-end
(the same class of gap this file's other disarm-race tests already
accept) — proven instead by unit tests against the extracted release
predicate and the marker-clear logic directly.

**Correction (Codex review finding, P1, PR #99, round 2):** the companion
fix above (the delivery arm's own exact-entry recheck) closed the window
between the passive arm's *initial* snapshot and its recheck, but left a
second window open: between that recheck (which acquired and released
`shared_channels`) and the subsequent `logical_links` write that actually
applies the `Established`/`Lost` outcome. `handle_stop_comm`'s and
`rpc_link.rs`'s own disarm sequences acquire `shared_channels` FIRST and
hold it across their own `logical_links` read (the read that decides
`was_established`) — so a disarm landing in that second window could
observe the connection as still `Listening`, skip `TEARDOWN_CONNECTION`,
`take()` the CLL's `tp20_connection`, and schedule a bounded release, all
before the delivery arm's own `logical_links` write ran. That write then
silently no-ops (`link.tp20_connection` already `None`), leaving the
routing entry scheduled for release while the native connection is
genuinely established with nothing left tracking it. Fixed by acquiring
`shared_channels` FIRST in the delivery arm too, and holding it across
both the recheck and the `logical_links` write — mirroring every disarm
site's own outermost-lock discipline exactly. The two flows are now
mutually exclusive via the same mutex: a disarm can only run either fully
before the recheck (in which case `still_current` already catches it and
the companion fix's retry handles it) or fully after the delivery arm
releases `shared_channels`, never interleaved in between. This is a pure
lock-discipline fix, not a timing mitigation — the race it closes is not
re-testable at the integration level (constructing it would require
literally reverting the fix), so it is verified by the lock-ordering
argument above plus the full existing passive-arm test suite (establish/
data-exchange, re-listen-after-loss, disarm-races-an-inbound-accept, the
new bounded-release test) continuing to pass unchanged.

**Correction (Codex review finding, P2, PR #99, round 2):** the passive
delivery arm's `Lost` outcome re-enters `Listening` but did not stop any
repeat-message slot the CLL had running against the connection just lost.
Unlike an ordinary ComPrimitive, `PDU_IOCTL_STOP_REPEAT_MESSAGE` is not
tied to `comm_started`, so a slot left running keeps autonomously
retransmitting the now-stale peer TX-ID both while the listener sits idle
in `Listening` and after a new peer establishes with a different TX-ID.
Fixed by stopping the CLL's own repeat slots (`stop_repeat_slots_for_cll`/
`push_leaked_repeat_slots`, the same best-effort pair `reconcile_live_
established_cll` already uses for an active TP2.0 connection's own
spontaneous loss) whenever the `Lost` outcome's connection was genuinely
`Established` beforehand.

**Correction (Codex review finding, P2, PR #99, round 3):** round 2's fix
above covers the device-side spontaneous-loss path; `handle_stop_comm`'s
own client-driven disarm sequence (`### 4. Disarm / teardown`) had the
identical gap on the OTHER path. Its active-connection arm only stops this
CLL's own repeat slots when `tp20_teardown.is_some()`, which the
active/passive split (`!c.passive` on that filter) deliberately excludes a
passive connection from — but the passive-disarm arm never called
`stop_repeat_slots_for_cll` either, so `CoptStopcomm` on an `Established`
passive connection with a live repeat slot left the adapter autonomously
retransmitting the stale peer TX-ID straight through the disarm. Fixed the
same way: `stop_repeat_slots_for_cll`/`push_leaked_repeat_slots`, called
before the native config-clear/teardown when `was_established`, mirroring
both the active arm's own stop-before-teardown ordering and round 2's
identical fix on the `Lost`-outcome path. `rpc_link.rs`'s
`DestroyComLogicalLink`/`DisconnectComLogicalLink` sites needed no
equivalent change — both already stop every one of a CLL's repeat slots
unconditionally, ahead of and independent of any TP2.0-specific branch, so
neither was ever susceptible to this gap.

**Correction (Codex review finding, P2, PR #99, round 4):** the two-call
native arm sequence (§1's "issue the two native `SET_CONFIG` calls",
`TP2_0_IDENTIFER` first, `TP2_0_RXIDPASSIVE` second) is unsafe whenever a
PRIOR disarm's own best-effort clear
(`best_effort_disarm_tp20_passive_native_config`) only partially
succeeded, leaving a stale nonzero `TP2_0_RXIDPASSIVE` on the device (that
helper always attempts both clears unconditionally, so either can fail
independently of the other) — the exclusivity check (`slot_busy`) only
inspects this SERVICE's own tracked state, never the device's actual
native config, so it cannot detect this. Writing the new nonzero
identifier first then briefly pairs it with that stale nonzero rx_id,
satisfying clause 19.3.3.1's both-nonzero accept condition for an rx_id
with no registered `Tp20ConnEntry` (this arm's own entry, keyed on the NEW
rx_id, is not inserted until after both native calls return) — an
untracked native connection. The mirror-image residual (a prior
identifier clear failing, rx_id clear succeeding) is symmetric.

Fixed by a 3-call sequence instead of 2: (a) force `TP2_0_IDENTIFER` to 0
FIRST, unconditionally, regardless of whatever value the device may
already hold — this alone already guarantees the device cannot
auto-accept, since one side of the both-nonzero condition is now
known-zero, neutralizing a stale residual on EITHER param; (b) write the
new `TP2_0_RXIDPASSIVE` next, still safe since identifier is 0; (c) write
the new `TP2_0_IDENTIFER` LAST — the single call that actually enables the
listener, and by this point `TP2_0_RXIDPASSIVE` already holds its correct
new value from (b), so no window ever pairs a stale value with a live
nonzero counterpart. A failure at (a) aborts immediately (nothing else
attempted); a failure at (b) or (c) leaves the device at
`TP2_0_IDENTIFER = 0` (disabled, never a mismatched pair), with a
best-effort rollback of whatever (b) already wrote on a (c) failure,
mirroring this mechanism's own "always attempt cleanup, never leave
known-bad state uncleaned" convention.

### 5. No new proto surface

Arming reuses the frozen `CoptStartcomm` plus two staged ComParams;
disarming reuses `CoptStopcomm`. No new IOCTL, no new `DataItem` variant,
no new proto message — [ADR-178](ADR-178-freeze-j2534-2-dpdu-interface.md)'s
freeze holds, extending Stage 7a's own precedent that TP2.0's lifecycle
verbs already exist in the D-PDU object model.

### 6. Cancellation

`CancelComPrimitive` needs no new contract: arm-and-complete means there is
never a pending passive `CoptStartcomm` for it to target. "Stop listening"
is `CoptStopcomm`'s disarm sequence (§4). The rejected waiting-model
alternative (§ above) was priced against this codebase's real
cancellation precedent, not dismissed unconsidered: SAE J1939's own claim
loop (`run_j1939_claim_loop`, `events_j1939_claim.rs`, ADR-179/180
Decisions 21/24) checks `cancelled_cops`/`stop_comm_pending` per iteration
and *does* support mid-wait cancellation — but that mechanism only works
because the claim loop's own wait is bounded per-candidate; it would not by
itself solve the passive listener's channel-starvation problem above, which
is about the shared poll task never reaching the next queued `TxItem` at
all, not about whether a wait can be individually cancelled. Stage 7a's own
pre-existing no-mid-wait-cancellation residual on the *active* connection
arm (`events_tp20_connection.rs`, backlogged) is unaffected by this ADR.

## Consequences

- **Implementation correction (Codex review finding, PR #99):**
  `arm_tp20_passive_listener`'s step 4 originally chained its two native
  `SET_CONFIG` calls with no rollback — if `CONFIG_TP2_0_IDENTIFER`
  succeeded but `CONFIG_TP2_0_RXIDPASSIVE` then failed, the adapter was
  left with a real, nonzero `TP2_0_IDENTIFER` and no recorded
  `Tp20ConnEntry`/`tp20_passive` state (since the arm itself failed), so no
  future disarm site would ever clear it — a native/service state
  divergence with no cleanup path. Clause 19.3.3.1's own accept condition
  requires BOTH params nonzero, so this alone could not cause a spurious
  accept, but it is still latent bad state. Fixed by best-effort rolling
  `CONFIG_TP2_0_IDENTIFER` back to 0 immediately if the second call fails,
  mirroring `best_effort_disarm_tp20_passive_native_config`'s own
  unconditional-clear convention; a failed rollback itself is logged and
  accepted as a residual, the same class ADR-188's own leaked-native-slot
  residual already documents for a failed best-effort call elsewhere in
  this mechanism. **Superseded in shape, not in substance, by round 4's
  Correction (Decision §4 above):** the round-4 fix reorders the arm
  sequence (`TP2_0_IDENTIFER=0` defensively first, then the new
  `TP2_0_RXIDPASSIVE`, then the new `TP2_0_IDENTIFER` last), so it is now
  `TP2_0_RXIDPASSIVE` that gets rolled back to 0 if the FINAL
  `TP2_0_IDENTIFER` call fails — the mirror-image of what this bullet
  originally described — but the underlying rollback convention and its
  accepted-residual reasoning are unchanged.
- **No client-visible establish/loss event for a passive connection.** The
  client observes establishment only indirectly — a send attempt that
  previously rejected with the no-connection error now succeeds, or the
  peer's first data telegram arrives. This is the same shape as ADR-188's
  already-accepted "no client-visible loss event" residual for active
  connections. **Resolved, by explicit decision rather than further
  deferral, by ADR-191:** these indication frames are state-machine input,
  not vehicle content, so tagging them into the content-delivery stream
  would be a category error, and no ISO 22900-2 `RxFlag` bit exists for
  them either — applies identically to both active and passive connections.
  Synthesizing a fake RX message to signal this instead was considered and
  rejected — inventing client-facing data the wire contract never actually
  defines would be worse than the documented residual.
- **Teardown-of-passive spec ambiguity** (§4 step 2): whether a real
  adapter actually honors `TEARDOWN_CONNECTION` against an established
  passive connection is unverified against real hardware (VW/Audi-specific,
  unavailable to this workspace — the same standing caveat ADR-188 and
  `docs/j2534-2-support-plan.md` §8 already carry for every TP2.0-adjacent
  claim). Degrades to the pre-existing maintenance-timeout self-heal
  residual class if a device rejects it, not a new failure mode.
- **Per-channel exclusivity assumes per-channel equals per-interface**
  (§2) — true today (no `TP2_0_CHx`), revisit if Additional Channels are
  ever added for this protocol.
- **ADR-188 needs no Status-line change.** Its own Stage 7b deferral is
  fulfilled by this ADR, not superseded or revised — Stage 7a's Decision
  and Consequences remain accurate and in force unchanged. `docs/j2534-2-
  support-plan.md` and `j2534-0404-service/docs/implementation-notes.md`'s
  own "deferred to Stage 7b" notes are updated in this same PR to record
  the fulfillment.
- **Real hardware unavailability**: VW/Audi-specific, same standing caveat
  as Stage 7a and every other TP2.0-adjacent claim in this workspace —
  correctness rests on mock fidelity and careful spec re-reading.
- Tests: mock-backed integration coverage for arm/establish/data-exchange
  on a passive connection; a second CLL's passive-arm attempt rejected
  while one is already armed; an active-connection RX-ID collision blocking
  a passive arm (and the reverse: a passive entry blocking an active
  proposal for the same RX-ID, already covered by the existing `tp20_rx_
  id_unavailable_for` check); a loss-then-re-listen-then-re-establish cycle
  proving the persistent entry survives a `Lost` outcome; and a disarm
  racing an in-flight accept, proving the unconditional-quarantine path
  drains correctly. `j2534-0404-mock` gains a per-channel passive-config
  store (`SET_CONFIG` range validation → `ERR_INVALID_IOCTL_VALUE`), a test
  hook injecting an inbound connection request (silently dropped
  network-side, no indication reaches the application, when the interface
  is full or `TP2_0_RXIDPASSIVE == 0`, per clause 19.3.3.1 — observable via
  a mock-side counter for test assertions only), and `TEARDOWN_CONNECTION`
  accepting `RX-ID-P` as a valid key.
- **Implementation correction (edge-case-hunter finding, pre-merge review):**
  the first implementation pass added §4's disarm sequence to
  `handle_stop_comm` but left `rpc_link.rs`'s two `DestroyComLogicalLink`/
  `DisconnectComLogicalLink` cleanup sites without it — completed
  afterward, mirroring `handle_stop_comm`'s exact shape. That completion's
  own first version left `rpc_link.rs`'s **pre-existing** active-connection
  teardown filter (`Tp20ConnectionPhase::Established`, with no `!c.passive`
  exclusion — `handle_stop_comm`'s own equivalent filter already excludes
  it) unchanged, so an `Established` passive connection matched BOTH the
  active-teardown block and the new passive-disarm block at both call
  sites, issuing `IOCTL_TEARDOWN_CONNECTION` twice (the second call always
  failing once the mock's slot is already removed by the first) and
  quarantining `rx_id_passive` via the active connection's own quarantine
  helper (`quarantine_tp20_connection_for_orphaned_write_back`, which
  stamps the resulting `Tp20ConnEntry` `passive: false` — incorrect
  attribution, though nothing currently reads `passive` on an `abandoned`
  entry, so no observed misbehavior traced to it). Fixed by adding
  `!c.passive` to both active-block filters, matching `handle_stop_comm`'s
  own guard exactly. **Accepted test-coverage gap:** no `grpc_mock` test
  can distinguish the buggy (double-call) code from the fixed (single-call)
  code by observable client behavior alone — `j2534-0404-mock`'s own
  `IOCTL_TEARDOWN_CONNECTION` handler is forgiving enough (the second,
  redundant call's failure is only logged, and the abandoned-entry release
  path in `deliver_tp20_connection_indication` does not consult `passive`)
  that a re-arm attempt on the freed `rx_id_passive` eventually succeeds
  either way, once the real teardown's own delayed `CONNECTION_LOST`
  indication drains. `destroy_com_logical_link_disarms_the_passive_
  listener` (`tests/grpc_mock/tp20.rs`) still establishes the connection
  before destroying (rather than merely arming it) specifically so it
  exercises both call sites' own `Established` branch at all, but its own
  pass/fail is not proof of single- vs. double-call correctness — that
  rests on code review (the fix's shape is identical to `handle_stop_comm`'s
  already-tested guard) plus this note. A `j2534-0404-mock` call-count
  backdoor for `IOCTL_TEARDOWN_CONNECTION` would close this gap if ever
  needed for a future finding in this same area.
