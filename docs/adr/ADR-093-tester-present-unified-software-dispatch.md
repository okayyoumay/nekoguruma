# ADR-093: Tester-Present Unified Software Dispatch (Mode 0 Drops `PassThruStartPeriodicMsg`), and `CP_TesterPresentReqRsp`-Aware P3 Gap Classification

**Date:** 2026-07-16
**Status:** Accepted (the unified window-scoped discard this ADR gives mode 0 is extended by ADR-099,
2026-07-17, to also cover SOM/TX_DONE/loopback indication frames, for both modes; this ADR's
"ADR-010 fully superseded" claim is narrowed to tester-present specifically by ADR-192, which
reintroduces native periodic-message usage, and its ADR-010-class leak-cleanup discipline, for a
different feature)
**Affects:** `j2534-0404-service` service, rpc_primitive, events, rpc_link, rpc_misc (supersedes ADR-083's
mode-0 dispatch mechanism, extends ADR-084's re-arm contract to mode 0, supersedes ADR-088's mode-0
discard carve-out, supersedes ADR-010)

## Context

ADR-083 gave `CP_TesterPresentSendType = 0` ("periodic," fixed-interval) and `= 1`
("idle-triggered") their ISO 22900-2-mandated distinct behaviors, but only mode 1 was made
software-driven — mode 0 kept using the native `PassThruStartPeriodicMsg`/`PassThruStopPeriodicMsg`
J2534 primitives, since ADR-083's era believed hardware-exact interval timing was worth the
resulting blind spot. That blind spot was real and is documented across three ADRs:

- ADR-083 itself: once `PassThruStartPeriodicMsg` returns, this service has **zero per-tick
  visibility** into the running periodic message — no callback, no per-tick timestamp — so
  `CP_P3Func`/`CP_P3Phys` (ADR-060) could only ever gate the *initial* start, never a single tick
  after it. Documented as a permanent, unfixable J2534 API limitation.
- ADR-084: mode 0 was explicitly excluded from the immediate-first-send contract and from
  `handle_update_param`'s live re-arm — "an already-running `Periodic` is never
  stopped/restarted/re-intervaled by this hook" — because the hardware primitive gave no way to
  re-arm it short of stop-then-restart, which this service never attempted.
- ADR-088: `CP_TesterPresentReqRsp = 1`'s response-discard mechanism could only be window-scoped
  (`CP_P2Max` after the actual send) for mode 1, where every send is visible; mode 0 kept the
  original armed-wide, unwindowed discard as "the closest achievable reading... it is mode 0's only
  available tightening," explicitly because there was no send instant to window against.

Separately, a real bug survived all three ADRs: `send_idle_tester_present_once`'s post-send
`TxGapState.no_response_required` was hardcoded to `true`, and every tester-present
`wait_for_p3_gap` call site hardcoded `num_receive_cycles = 0`, regardless of
`CP_TesterPresentReqRsp`. Per ADR-060, that flag means "this transmission does not elicit a
protocol-level response" — when `CP_TesterPresentReqRsp == 1` the ECU *does* reply (ADR-088 just
discards it), so the flag should be `false` in that case. This was found, confirmed, and
deliberately deferred during ADR-088's own design-advisor review (tracked in
`j2534-0404-service/docs/implementation-notes.md`'s backlog).

The domain expert who has driven this subsystem's design throughout ADR-083/084/088 gave an
explicit instruction (not inferred): eliminate `PassThruStartPeriodicMsg`-based tester-present
transmission entirely, and dispatch **all** tester-present sending — both modes — from this
service's own software poll loop, the way mode 1 already works. `design-advisor` confirmed
`start_periodic_message`/`stop_periodic_message` are used, in this codebase, only for
tester-present mode 0 — no client-facing feature depends on these native calls surviving.

## Decision

### One armed state, discriminated by the mode already stored on it

`TesterPresentState::Periodic { id, target_can_ids }` and `::Idle { resolved, interval, armed_at,
last_fired, framed_data, discard_until }` collapse into a single variant:

```rust
enum TesterPresentState {
    None,
    Armed {
        resolved: rpc_primitive::ResolvedTesterPresent,
        interval: Duration,
        armed_at: tokio::time::Instant,
        last_fired: Option<tokio::time::Instant>,
        framed_data: Vec<u8>,
        discard_until: Option<tokio::time::Instant>,
    },
}
```

`resolved.send_type` (already stored, already validated to `{0, 1}` at resolve time) is the mode
discriminator — no new field or sibling variant is needed, and nothing is duplicated between two
shapes that must otherwise be kept in sync. `TesterPresentTargetCanIds` (ADR-088 amendment) is no
longer frozen redundantly onto the state enum itself; both modes now read `resolved.target_can_ids`
uniformly, since `resolved` is only ever replaced at an actual arm/re-arm for either mode now.
`TesterPresentToken` collapses to `None | Armed(Instant)`, keyed on `armed_at` — already a unique
per-arm identity for both modes.

### `handle_start_comm`'s two branches become one

Mode 0's branch (P3-gate → `start_periodic_message` → store `Periodic`) is deleted outright,
including its `Ok`/`Err` handling and the orphan-cleanup block that existed for a lost
periodic-id write-back race (nothing hardware-side can be orphaned once no periodic message is
ever started). Both `send_type == 0` and `send_type == 1` now run what was mode 1's branch:
non-cancellable `wait_for_p3_gap`, `still_on_this_channel` re-validation, an immediate synchronous
first send via `transmit_request` (`count_as_bus_activity = false`), and `TesterPresentState::Armed`
construction with `armed_at = last_fired = fired_at`. **Mode 0 now gets ADR-084's immediate-first-send
contract too** — the same keep-alive-at-session-start guarantee mode 1 already had.

### One dispatcher, one mode-aware due-check formula

`dispatch_due_idle_tester_present` → `dispatch_due_tester_present`; `send_idle_tester_present_once`
→ `send_tester_present_once`; `DueIdleTesterPresent` → `DueTesterPresent`. A single new helper
replaces the mode-1-only idle-since-bus-activity check:

```rust
fn tester_present_due_reference(
    send_type: u32,
    armed_at: Instant,
    last_fired: Option<Instant>,
    last_activity: Instant,
) -> Instant {
    let anchor = last_fired.unwrap_or(armed_at);
    if send_type == 1 { last_activity.max(anchor) } else { anchor }
}
```

Mode 1 keeps its existing idle-reset-on-bus-activity semantics unchanged. Mode 0 fires strictly
every `interval` since its own last fire/arm and **never consults `last_bus_activity`** — nothing
else can defer it, matching ISO 22900-2's "periodic" semantics (unconditional cadence) as distinct
from "idle-triggered." One function serves both, rather than two structurally parallel ones,
because the ~200 lines of race-guard discipline this function already carries (`armed_at`-identity
re-validation, `connect_generation` checks, fresh-`last_activity` reads, pre-send `CP_P2Max`
snapshotting — each the product of a separate Codex review round across ADR-083/086/088) would
otherwise have to be duplicated and independently hardened a second time for one line of
mode-dependent logic.

Mode 0's own send no longer stamps the shared `last_bus_activity` clock either (previously
unconditional on a successful periodic start) — the same ADR-083 rationale already applied to mode
1 ("one CLL's keepalive to its own ECU is not activity that should defer a sibling's idle timer")
extends naturally now that mode 0 is dispatched through the identical `transmit_request` call.

### `CP_TesterPresentReqRsp`-aware P3 classification, fixed once in the shared path

`ResolvedTesterPresent` gains `expects_response: bool` (`active.tester_present_req_rsp() == 1`),
resolved alongside `target_can_ids`/`p2_max_ms` and excluded from `same_wire_behavior` for the same
reason those two are (RX/gap-timing classification, not wire content). `send_tester_present_once`'s
post-send `TxGapState` stamp becomes `no_response_required: !expects_response`; every tester-present
`wait_for_p3_gap` call site's hardcoded `num_receive_cycles = 0` becomes
`if expects_response { 1 } else { 0 }`. Sourced per site exactly like `p2_max_ms`/`connect_generation`
already are in this same function:

- **Arm/re-arm sites** (the unified `handle_start_comm` branch, `handle_update_param`'s re-arm)
  read `resolved.expects_response` directly — avoiding the `temp_param_update` Working-vs-Active
  trap ADR-088 already worked around for `p2_max_ms`.
- **`dispatch_due_tester_present`'s per-tick sends** capture `l.active.tester_present_req_rsp() ==
  1` twice: once in the initial due-snapshot critical section (needed as the `wait_for_p3_gap`
  argument, which runs before the `still_due` recheck can), and again, freshly re-read, in the
  `still_due` pre-send critical section (used for the send's own `TxGapState` stamp) — extending
  the existing tuple that already threads `connect_generation`/`p2_max_ms` through this same
  two-stage capture. A `CP_TesterPresentReqRsp` flip racing one gap wait misclassifies at most that
  one wait — bounded, and not a new class of exposure this codebase doesn't already accept
  elsewhere in this same function for `p2_max_ms`.

This fully closes the P2 backlog item ADR-088's design-advisor review deferred
(`j2534-0404-service/docs/implementation-notes.md`), including the mode-0 half ADR-083 had written
off — mode-0 sends are now per-send visible, so there is no longer a "periodic path's own hardcoded
limitation" to reconcile it against.

### Mode 0 re-arm — a deliberate behavior change, not a side effect

`handle_update_param`'s re-arm gate previously excluded `Periodic` outright and required
`resolved.send_type == 1`. Both restrictions are removed: a `CoptUpdateparam` that changes
tester-present-relevant ComParams (`same_wire_behavior`, comparing `data`/`interval_ms`/`tx_flags`/
`isotp_framing`/`send_type`/`can_functional`) now re-arms mode 0 exactly the way it already re-arms
mode 1. This is the change that makes deleting `Periodic`'s frozen `target_can_ids` sound — without
it, mode 0 would have no live path to ever update a stale physical target, the same class of gap
ADR-088's amendment spent three review rounds closing for mode 1. It also satisfies ISO 22900-2
NOTE 19 ("any change to a tester-present ComParam resends immediately," ADR-088's own reading of it)
uniformly across both modes for the first time. This extension had a carve-out that wasn't caught
until a later review round — see "`TesterPresentState::Cleared`" below.

### `TesterPresentState::Cleared` — the re-arm extension's `CLEAR_PERIODIC_MSGS` interaction
(Codex PR review finding, same PR)

`TesterPresentState::None` conflated two meanings: "never configured" and "explicitly cleared via
`PDU_IOCTL_CLEAR_PERIODIC_MSGS`." `handle_update_param`'s re-arm gate treats `token == None` plus a
non-empty configured payload as ADR-084's legitimate "first enable" case — but that is
indistinguishable from "client explicitly cleared an already-armed tester-present, and some later
*unrelated* `CoptUpdateparam` (e.g. promoting `CP_Loopback`) resurrects it," since both produce
`token == None` with a non-empty payload still sitting in Active. This was already latent for mode 1
(which could already reach this gate before this ADR) but was never recorded as an accepted residual
anywhere, and became a flat, client-visible regression for mode 0 specifically: mode 0's
`PassThruStartPeriodicMsg` predecessor made a clear durable at the hardware level with no live
re-arm path at all, so extending re-arm to mode 0 (above) newly exposed it there.

Fixed with a third `TesterPresentState` variant carrying the resolved value that was on the wire at
clear time: `Cleared { resolved: ResolvedTesterPresent, cleared_at: Instant }`. `CLEAR_PERIODIC_MSGS`
transitions an `Armed{..}` CLL to `Cleared` (not `None`) with a clone of its `resolved`; the re-arm
snapshot feeds that `resolved` into `old_resolved` exactly as it already does for `Armed`, so
`same_wire_behavior` correctly reports "unchanged" for an unrelated promotion (blocking
resurrection) while still correctly reporting "changed" for a promotion that actually reconfigures
tester-present's wire content — reconfiguring TP after a clear is itself a legitimate re-enable, per
NOTE 19, and must not be blocked. `TesterPresentToken` gains `Cleared(Instant)`, keyed on
`cleared_at`, since the gate's post-`.await` staleness recheck must distinguish "still the same
clear" from "got re-armed and re-cleared again during my `.await`s." A fresh `CoptStartcomm` (or
session teardown) still unconditionally overwrites `Cleared` the same as it already overwrote
`None`/`Armed` — clearing does not block the always-legitimate re-enable paths.

**Deliberate asymmetry, not a residual to close:** while a CLL is `Cleared`, a `CoptUpdateparam` that
resolves tester-present to *disabled* (empty payload or zero interval) leaves the state `Cleared`
unchanged rather than reverting to `None` — the comparison baseline stays "what was on the wire at
clear time," so a later promotion back to the exact cleared config still correctly doesn't
resurrect. Coherent with the armed-side contract (never silently disarm, never resend on unchanged
wire behavior); not special-cased away.

### Mode 0's tester-present discard is now window-scoped, like mode 1

`build_cll_rx_entries`'s `TesterPresentState::Periodic`-specific armed-wide-discard arm is deleted;
both modes now go through what was the `Idle`-only path — `discard_until`-windowed, reading
`resolved.target_can_ids` directly. ADR-088's mode-0 carve-out ("the armed-wide prefix match remains
the closest achievable reading... it is mode 0's only available tightening") no longer applies:
mode 0 has a real send instant to window against now, the same as mode 1.

### Dead native code removed; wrapper/FFI layer untouched

Removed from `j2534-0404-service`: the `start_periodic_message` call and mode-0 branch in
`handle_start_comm`, the orphan-cleanup block, `handle_stop_comm`'s `stop_periodic_message` call and
its `Periodic`-only teardown branch (collapsed to a single `Armed{..} => None` transition — no
hardware call, ordering invariant from ADR-085 unchanged, only the hardware call disappears), and
both `stop_periodic_message` call sites in `rpc_link.rs` (`DestroyComLogicalLink`/
`DisconnectComLogicalLink`) along with their periodic-id extraction plumbing. The `j2534-0404`
safe-wrapper crate's `start_periodic_message`/`stop_periodic_message`/`PeriodicMessageId` API and
all `-sys`/FFI bindings are untouched — they mirror the native J2534 API surface, not this service's
usage of it. `rpc_misc.rs`'s `CLEAR_PERIODIC_MSGS` IOCTL handler's `clear_periodic_messages` native
call is unchanged (it remains a generic, client-facing IOCTL that simply now has nothing
hardware-side to ever clear for this service's own tester-present messages) — but its per-CLL disarm
loop is **not** left untouched: see "`TesterPresentState::Cleared`" below, where it was corrected to
transition an armed CLL to a new `Cleared` state (carrying its `resolved` value forward) instead of
bare `None`.

### Supersedes ADR-010 entirely

ADR-010 ("Stop Tester-Present Periodic Message on Shared-Channel Disconnect/Destroy") existed
solely to fix a leak of `PassThruStartPeriodicMsg` handles across a shared-channel
disconnect/destroy. With no periodic message ever started, that entire mechanism — and the bug it
fixed — no longer exists. Teardown for tester-present at both call sites is now a plain state
clear, unconditionally, regardless of `ref_count`.

## Consequences

- **Accepted residual — `dispatch_due_tester_present`'s `expects_response` two-read window
  (edge-case-hunter finding; design-advisor-confirmed no cheap fix for the residual itself,
  2026-07-16; narrowed by a same-day Codex PR-review fix — see below).** Both the due-snapshot's
  `expects_response` (used only for the `wait_for_p3_gap` call's `num_receive_cycles` argument,
  deciding *whether to wait now*) and the `still_due` recheck's independently, freshly re-read
  `l.active.tester_present_req_rsp()` (used for the actual post-send `TxGapState` stamp) now read
  **live** Active state — Codex's review caught that the due-snapshot originally read the *frozen*
  `resolved.expects_response` instead, and because `expects_response` is deliberately excluded from
  `same_wire_behavior` (so an RX-classification-only `CoptUpdateparam` doesn't trigger a spurious
  resend), that frozen copy was never refreshed by a `CP_TesterPresentReqRsp`-only change —
  staleness was unbounded (until some *unrelated* field change happened to trigger a re-arm), not
  the bounded single-wait window originally analyzed here. Fixed to read `link.active` at
  due-snapshot time too, matching `still_due`'s pattern. What remains, now correctly bounded to a
  single wait as originally intended: the two reads can still observe different values if a
  `CoptUpdateparam` changes `CP_TesterPresentReqRsp` in between them — including during
  `wait_for_p3_gap`'s own wait, which is the largest part of that window (up to the full configured
  gap, not an instant). This narrower residual is not a new class of exposure this diff introduces:
  `wait_for_p3_gap`
  snapshots its gap deadline once before entering its wait loop for every call site, including
  `handle_send_recv`'s pre-existing one — a `CoptUpdateparam` changing any gap-relevant ComParam
  mid-wait already isn't re-derived anywhere in this mechanism (ADR-060). Closing it here would mean
  restructuring `wait_for_p3_gap` to re-evaluate its deadline inside the wait loop for every call site,
  and even then the send's own `ctx.api.lock().await` leaves an irreducible residual instant — judged
  disproportionate to the actual blast radius: at most one `CP_P3Func`/`CP_P3Phys` gap-wait skipped or
  added unnecessarily around one tester-present send, immediately following a client's own live
  `CP_TesterPresentReqRsp` reconfiguration, and self-healing on the very next send (the `still_due`
  fresh read correctly stamps `TxGapState` for whichever value was actually live at send-commit time).
  Same severity class as ADR-088's "one extra frame" and ADR-060's RC21/23 out-of-scope note. Both
  single-read alternatives considered and rejected: reusing the due-snapshot for the stamp would widen
  the blast radius (misclassifying the *next* send's gap too) and violate the read-fresh-at-send-commit
  discipline `p2_max_ms`/`connect_generation` already establish in this same function (ADR-053/067);
  moving the fresh read before `wait_for_p3_gap` would defeat `still_due`'s entire purpose, which exists
  specifically to revalidate *after* the wait's `.await` completes.
- **Timing accuracy tradeoff, accepted and precedented.** Mode 0's interval accuracy is now bounded
  by `POLL_INTERVAL_MS` (10 ms) plus `wait_for_p3_gap` residual, identical to mode 1's
  already-documented bound (ADR-083). Each cycle's period is `interval + jitter`, never shorter —
  drift only ever lengthens the gap between sends, the fail-safe direction for a keep-alive. This
  buys full `CP_P3Func`/`CP_P3Phys` coordination (closing ADR-083's "permanent API limitation"),
  per-send `discard_until` windowing (closing ADR-088's mode-0 carve-out), and every
  `connect_generation`/`armed_at`-identity guard mode 1 already had — in exchange for two honest
  costs, stated plainly rather than glossed over:
  - A hardware-autonomous keep-alive survived a busy/stalled service process; software dispatch
    does not — mode-0 sends now depend on the poll task and the shared `api` mutex being live,
    same as mode 1 always did. **Update (ADR-094):** a Codex review finding on this PR caught that
    the poll loop's outer `select!` had its own starvation gap under sustained TX queue pressure
    (a restarting-every-iteration `sleep` arm that a continuously-ready `tx_rx.recv()` could starve
    indefinitely) — mode 0 had just lost its hardware-given immunity to exactly this class of
    problem. Fixed by ADR-094 (a persistent tick deadline, not tester-present-specific); mode 0
    regains its starvation immunity as a result, this time for the same underlying reason mode 1 and
    RX polling now have it too.
  - ADR-083's "the adapter is the authority on SAE J2534-1's 5–65535 ms `TimeInterval` band"
    enforcement is gone — a sub-10 ms interval now silently floors at the poll tick instead of
    `PassThruStartPeriodicMsg` failing loudly. No synthetic range check is added to replace it (the
    same external-spec-knowledge reasoning ADR-083 already used to justify not hardcoding one).
  - `PduErrEvtTesterPresentError` no longer fires for a bad `TimeInterval` at `CoptStartcomm` (there
    is no longer a periodic-start call that could reject one); failures now surface per-send, same
    as mode 1 always has.
- **Mode 0 can now be re-armed live**, a deliberate, previously-impossible capability (see Decision)
  — a `CoptUpdateparam` that changes `CP_TesterPresentTime`/message content/addressing for a mode-0
  CLL now takes effect immediately rather than only on the next `CoptStopcomm`/`CoptStartcomm`
  cycle.
- **ADR-083's "Mode-0 periodic vs. CP_P3Func/CP_P3Phys" section and its related Consequences bullets
  are superseded by this ADR** — struck through there, pointing here. ADR-084's mode-0 exclusion
  ("mode 0 is out of scope everywhere, including here") is superseded by this ADR's re-arm decision
  above. ADR-088's mode-0 armed-wide-discard carve-out is superseded by this ADR's window-scoping
  decision above. **ADR-010 is fully superseded** — its fix target no longer exists.
- Test suite: `tester_present_send_type.rs` and `tester_present_reqrsp.rs` mode-0 tests were
  rewritten to assert software `PassThruWriteMsgs` sends (`written_count`) instead of
  `start_periodic_count`, and to prove the new re-arm/window-scoping behavior instead of the old
  "mode 0 never re-arms" premise. `p3_gap.rs` gained the `CP_TesterPresentReqRsp`-aware
  `no_response_required` regression pair. All changes verified with explicit
  fail-without/pass-with proof per check (reverting the P3 classification fix, the re-arm gate
  change, and the discard-windowing change independently each reproduced exactly one test failure).
- One additional test (`send_type_1_no_periodic_start_and_fires_one_shot_after_idle`) joined the
  known-flaky-under-parallel-load tests of the time — same timing-window category as the two
  entries listed then (its later fix is cause 3 in `j2534-0404-service/docs/implementation-notes.md`'s
  "Test-suite reliability: past flaky-test root causes" section), reproducible failure only under full-suite contention, passes reliably
  in isolation.
