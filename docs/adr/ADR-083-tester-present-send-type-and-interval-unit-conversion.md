# ADR-083: CP_TesterPresentSendType Software Idle Mode, and CP_TesterPresentTime Unit Conversion

**Date:** 2026-07-12
**Status:** Accepted (superseded in part by ADR-093 — mode-0 dispatch mechanism and its P3/discard
limitations; see the strikethrough sections below)
**Affects:** `j2534-0404-service` service, service_params, comparam_id, rpc_primitive, events, rpc_link, rpc_misc

## Context

`CP_TesterPresentSendType` (`PARAM_TESTER_PRESENT_SEND_TYPE`, D-PDU 0x8008) has two ISO 22900-2
values:

- `0`: send on a periodic interval defined by `CP_TesterPresentTime`.
- `1`: send only after the bus has stayed quiet for `CP_TesterPresentTime`; any bus activity
  restarts that idle timer.

Only mode 0 existed. `resolve_tester_present` and the `CoptStartcomm` TX-branch handler
(`events.rs`) always called `PassThruStartPeriodicMsg`, regardless of what
`CP_TesterPresentSendType` was set to. The ComParam itself was fully wired (get/set, validation,
per-protocol defaults) but never read by the dispatch path. This is not a hypothetical gap:
`iso_14230_3_on_iso_15765_2` — KWP2000 running over an ISO15765-2/CAN transport —
defaults `CP_TesterPresentSendType` to `1`, so a real preset was silently getting mode-0 behavior.

J2534 v04.04 has no native primitive for "send once N since last bus activity, reset on any
activity" — `PassThruStartPeriodicMsg` is fixed-interval-only. Mode 1 has to be driven by this
service.

Separately, `CP_TesterPresentTime` — like every other ISO 22900-2 `CP_*` timing ComParam — is
µs-resolution, but `PassThruStartPeriodicMsg`'s `TimeInterval` argument is native J2534 ms
resolution. The stored value (`PARAM_TESTER_PRESENT_INTERVAL_MS`, default `2_000_000` across every
preset — evidently meant as 2,000,000 µs = 2 s) was being forwarded to `TimeInterval` unconverted,
turning a "2 second" default into a ~33-minute periodic interval. ADR-072 established the
`us_to_ms`/`us_to_half_ms` conversion pattern for exactly this kind of native-resolution mismatch,
but it was never applied here because this param is service-level (0x8002), not a native J2534
`SET_CONFIG` ID that flows through `to_j2534_config_value`.

A third, related gap: ADR-060's `CP_P3Func`/`CP_P3Phys` software gap enforcement
(`wait_for_p3_gap`) is wired into exactly one call site, `handle_send_recv`'s `CoptSendrecv` path.
The mode-0 periodic tester-present start (`events.rs`, inside `CoptStartcomm` handling) never
consults or updates that gap state, even though the J2534 v04.04 spec for
`PassThruStartPeriodicMsg` requires periodic messages to respect the bus idle timing
parameters (P3Min being the example given). On K-line this is free — native hardware `P1_MAX`/`P3_MIN` applies to
every adapter-issued transmit. On ISO15765 there is no native `P3_MIN` `SET_CONFIG` path
(confirmed: `ComParamId::to_j2534_config_id`'s ISO15765 allowlist has no `P3_MIN`); `CP_P3Func`/
`CP_P3Phys` is this service's own software substitute, and periodic sends were invisible to it in
both directions.

## Decision

### CP_TesterPresentTime: convert µs → ms at resolve time

`resolve_tester_present` (`rpc_primitive.rs`) converts the stored µs value to ms with the existing
`us_to_ms` round-to-nearest helper (`comparam_id.rs`, ADR-072), the same policy applied to every
other µs-denominated timing param. `PARAM_TESTER_PRESENT_INTERVAL_MS`/`tester_present_interval_ms()`
are renamed to `PARAM_TESTER_PRESENT_INTERVAL_US`/`tester_present_interval_us()` — the old names
actively misstated the stored unit, which is the proximate cause of the bug.
`ResolvedTesterPresent.interval_ms` keeps its name; post-conversion it genuinely is milliseconds.

Three outcomes after conversion, handled distinctly rather than uniformly:

- Raw stored `0` → tester-present disabled. This check happens *before* conversion so the existing
  "no TP configured" sentinel is untouched.
- Non-zero raw value that rounds to `0` ms (sub-500µs) → synchronous `StartComPrimitive`
  `INVALID_ARGUMENT` at resolve time, consistent with ADR-067's resolve-time-failure precedent.
  This is the one broken value the service itself can produce, and letting it fall through would
  silently collapse a configured keep-alive into "disabled," which is exactly the harder-to-notice
  failure.
- Non-zero result that is out of the adapter's own accepted `TimeInterval` band → not hardcoded or
  synchronously rejected here (SAE J2534-1's 5–65535 ms range is external spec knowledge, not
  present in this repo's stripped header, and the actual adapter is the authority on it). Left to
  `PassThruStartPeriodicMsg`'s own failure, surfaced through the existing
  `PduErrEvtTesterPresentError` path.

### CP_TesterPresentSendType: mode 1 driven off the existing 10 ms poll tick, outside the TxItem queue

`LogicalLinkState.tester_present_periodic_id: Option<PeriodicMessageId>` becomes:

```rust
enum TesterPresentState {
    None,
    Periodic(PeriodicMessageId),          // SendType=0, hardware-autonomous
    Idle { resolved: ResolvedTesterPresent, interval: Duration, armed_at: Instant }, // SendType=1
}
```

`ResolvedTesterPresent` gains a `send_type` field (`tester_present_send_type()` accessor mirroring
the existing `tester_present_interval_ms()`/`_us()` pattern). The `CoptStartcomm` TX-branch handler
branches on it: `send_type == 0` keeps the existing `start_periodic_message` call and stores
`Periodic(id)`; `send_type == 1` arms the idle clock and stores `Idle{..}` — no hardware call, no
`PeriodicMessageId`.

Idle detection:

- **Clock scope: per shared physical channel**, not per CLL — the spec measures "the bus has been
  idle," and ADR-060 already treats P3-family bus-idle timing as a shared-channel property. A new
  `Arc<Mutex<Instant>>` (`last_bus_activity`) is created once per physical channel alongside
  ADR-060's `last_func_tx`/`last_phys_tx`, and threaded into `ChannelPollCtx`.
- **Reset on genuinely external TX and RX only — never on arming, and never on this subsystem's own
  idle-mode sends.** `last_bus_activity` is stamped by real wire events this CLL did not itself
  schedule: `poll_rx` (any received frame) and `transmit_request` for an ordinary `CoptSendrecv`
  write, an RC21/23 re-request, or a mode-0 periodic start. Arming a CLL's `Idle` state
  (`CoptStartcomm` itself) does not stamp it either — arming has no `PassThruConnect`, no frame on
  the wire, and is a purely local state transition, not "bus activity." **An idle-mode CLL's own
  one-shot send is excluded from this shared clock** (`transmit_request`'s `count_as_bus_activity`
  parameter, `false` only at the idle-mode call site): a caught-in-review bug (see Consequences) had
  it included, which let one CLL's own keep-alive permanently resynchronize and starve a sibling CLL
  with the same interval on the same channel — two independent ECU sessions do not share an S3-timer
  budget just because they share a physical bus, so one CLL's tester-present to its own ECU must
  never count as "activity" that defers another CLL's unrelated keep-alive.
- **`poll_rx`'s `last_bus_activity` stamp ignores `CONFIG_LOOPBACK` TX echoes.** When loopback is
  enabled, `PassThruWriteMsgs` echoes the transmitted frame back through the RX queue with
  `RxStatus`'s `RX_TX_MSG_TYPE` bit set. An idle-mode CLL's own send is excluded from the shared
  clock at the TX side (`count_as_bus_activity = false`, above) specifically to prevent it from
  starving a same-interval sibling — but `poll_rx_inner`'s RX-side stamp originally fired
  unconditionally for any non-empty read, including that same send's own loopback echo, silently
  reintroducing the exact starvation the TX-side exclusion was built to prevent (caught in review).
  The stamp now only fires when at least one frame in the batch is not a TX echo. `RX_TX_MSG_TYPE`
  is defined locally in `events.rs` rather than sourced from `j2534-0404-sys`'s generated bindings:
  bindgen's `allowlist_var` regex matches `RX_FLAG_.*` but not this bare name, and widening that
  regex to regenerate bindings for all five pre-committed targets is out of scope for one bit value
  the J2534 v04.04 header already defines.
- **A failed idle-mode send backs off for a full interval, the same as a successful one.** The
  `Err` arm of `dispatch_due_idle_tester_present`'s send used to leave `last_fired` unchanged,
  so a persistently-failing send (e.g. a recurring hardware error) stayed "due" and was retried on
  every ~10 ms poll tick instead of waiting out `CP_TesterPresentTime` again — flooding
  `PassThruWriteMsgs` calls and `PduErrEvtTesterPresentError` events (caught in review). The `Err`
  arm now stamps `last_fired` under the same `channel_id`/`armed_at` re-validation guard the
  success arm already uses.
- **Software ISO-TP stamps per frame written, not post-hoc on the overall send result.** A
  multi-frame `isotp_send` (FirstFrame, then a FlowControl wait, then ConsecutiveFrame blocks) can
  put a FirstFrame — and possibly some ConsecutiveFrames — on the wire and still return `Err` later
  (no FlowControl within `CP_Bs`, a mid-block write failure, cancellation): that's still real bus
  occupancy a sibling CLL's idle detection must see, even though the send as a whole failed. Rather
  than gating on `transmit_request`'s overall `Result::is_ok()` (which a partial-then-failed send
  never reaches), `write_can_frame` — the sole chokepoint every ISO-TP frame write already passes
  through, for the single-frame, FirstFrame, and each ConsecutiveFrame case alike — takes
  `count_as_bus_activity` itself and stamps `last_bus_activity` immediately after each individual
  successful write, before any later step in the same send has a chance to fail (caught in review;
  `transmit_request`'s own post-hoc stamp remains, now a harmless duplicate on full success, and
  stays the sole stamp for the non-ISO-TP hardware-transport path, which has no partial-write
  scenario since it's a single atomic `PassThruWriteMsgs`).
- **Per-CLL arm instant and per-CLL fire instant, not a shared-clock mutation.** `TesterPresentState::Idle`
  carries `armed_at: Instant` (set once at arm time) and `last_fired: Option<Instant>` (`None` until
  this CLL's own first successful send, then advanced on every subsequent one). The fire decision
  uses `now.duration_since(last_bus_activity.max(last_fired.unwrap_or(armed_at))) >= interval` — each
  CLL's idle window starts counting from the later of "the last *external* bus event" and "when this
  CLL last fired (or armed, before its first fire)." Because a CLL's own sends no longer touch the
  shared clock, this anchor stays CLL-local: two same-interval siblings due at the same instant both
  fire (P3-gap-spaced, not one deferred a full interval to the other) and both keep re-arriving at
  their own due instant every interval after that, independent of each other. Genuine external
  traffic (another CLL's `CoptSendrecv`, a client write, an RX frame, a mode-0 periodic start) still
  legitimately defers every mode-1 CLL on the channel equally, since that traffic is real bus
  occupancy none of them scheduled.
- **Fire decision: per CLL.** On the poll loop's existing tick, each CLL whose stored state is
  `Idle{..}` evaluates the test above; if elapsed, the service builds a one-shot send from the
  already-resolved `data`/`tx_flags`/`isotp_framing` and dispatches it through `transmit_request`
  (`count_as_bus_activity = false`) — the same path a normal software TX takes otherwise, so it still
  participates in `wait_for_p3_gap` and updates `last_func_tx`/`last_phys_tx` like any other send;
  only the *shared idle-detection* clock is excluded. This is dispatched directly from the poll loop,
  not enqueued as a `TxItem`: it has no `cop_handle`, emits no COP status event, and must not interact
  with per-CLL FIFO ordering, `tx_held`, or `tx_suspended` (ADR-081) — the same reasoning that already
  keeps the mode-0 periodic start outside the `TxItem` queue. The snapshot of due CLLs is taken and
  released under `logical_links.lock()` before any `.await`; because the send itself happens after
  the lock is dropped, the dispatcher re-validates — immediately before sending — that the CLL still
  exists, is still connected on this same physical channel, and is still the *same arm* (`armed_at`
  unchanged from the snapshot) before using the pre-wait `protocol_id`/`tx_flags`/`data`, so a
  concurrent `DisconnectComLogicalLink`/`DestroyComLogicalLink`/`CoptStopcomm`/reconnect-and-rearm
  landing in that window is not raced past. On a successful send, `last_fired` is written back under
  the same arm-identity guard, from the post-send instant (not the pre-wait snapshot time), so it
  reflects real on-wire timing even after a `wait_for_p3_gap` delay.
- Idle-mode teardown is a `TesterPresentState::Idle → None` transition (no hardware resource to
  release), replacing the mode-0-only `stop_periodic_message` call at the four existing teardown
  sites (`CoptStopcomm`, disconnect, destroy, and the `CLEAR_PERIODIC_MSGS` IOCTL).
- **Driven from every long-running per-tick poll loop on the channel, not just the outer select
  loop — with one deliberate exception.** (**ADR-094/091 note:** the outer select loop itself later
  turned out to have its own starvation gap under sustained TX queue pressure (ADR-094), and a
  follow-up audit found several more holds this bullet's "two long-running sub-loops" framing missed
  entirely — `isotp_send`'s FC-wait/STmin loops, the RC21/23 retry wait, and the parked/`tx_held`
  drain paths — see ADR-095 for the full enumeration.) `dispatch_due_idle_tester_present` was
  originally called only from `poll_channel_events`'s outer `tokio::select!` sleep arm, which only
  runs *between* `TxItem`s — but `handle_delay` (`CoptDelay`, a client-controlled duration) and
  `wait_for_expected_response` (`CoptSendrecv`'s response phase, unbounded for
  `NumReceiveCycles == -1`/IS-CYCLIC) each run their own internal poll loop that can hold the poll
  task for an arbitrarily long time without returning to the outer loop — during which a mode-1
  sibling CLL sharing the same physical channel could never fire, however long its own interval,
  defeating the idle-triggered keepalive contract for that sibling (caught by Codex review). Both
  now also call `dispatch_due_idle_tester_present` on every non-terminal iteration, alongside their
  existing per-tick `poll_rx` call. `wait_for_p3_gap` is **deliberately excluded** from this same
  treatment: `dispatch_due_idle_tester_present` itself calls `wait_for_p3_gap` for each due CLL's
  own send, so adding a call to it *inside* `wait_for_p3_gap`'s own loop would close a mutual
  `async fn` recursion cycle — Rust rejects the resulting infinitely-sized future outright (it does
  not compile without boxing the future, which this design does not do). The omission is safe
  without a recursion guard because `wait_for_p3_gap`'s wait is bounded by a P3-family
  inter-message gap (`CP_P3Func`/`CP_P3Phys`), which is a bus-pacing timing anchored to a *past* TX
  and therefore necessarily much shorter than a session-keepalive interval (`CP_TesterPresentTime`,
  seconds-scale by default) — a CLL blocked behind another CLL's P3 gap slips its keepalive by at
  most that gap's residual duration, the same order of magnitude as the poll-tick jitter already
  accepted below. A client that configures `CP_P3Func`/`CP_P3Phys` larger than its own
  `CP_TesterPresentTime` is a self-contradictory configuration this design does not attempt to
  cover.
- Detection accuracy is bounded by the poll tick (`POLL_INTERVAL_MS` = 10 ms) plus, when the poll
  task is inside a `wait_for_p3_gap` wait for some other CLL's send, that gap's residual duration —
  negligible against keep-alive intervals in the hundreds-of-ms-to-seconds range, but not exact.
- **Never RX-polls unprobed from inside `wait_for_expected_response`'s match-sensitive context.**
  Driving dispatch from `wait_for_expected_response` (the bullet above) introduced a second bug,
  caught by a follow-up Codex review: `poll_rx` and `poll_rx_and_check_match` both wrap the same
  sole RX-drain point, `poll_rx_inner`, differing only in whether a `MatchProbe` is threaded through
  to attribute a drained frame to a specific COP's expected response. `dispatch_due_idle_tester_present`'s
  own per-item `wait_for_p3_gap` call drains RX via plain `poll_rx` (no probe) inside its wait loop —
  fine at the outer-select and `handle_delay` call sites, where there is no COP-specific match state
  to protect, but not from inside `wait_for_expected_response`: a due, `CP_P3Func`/`CP_P3Phys`-gated
  idle send entering `wait_for_p3_gap`'s wait there could drain the very ECU response the active
  `CoptSendrecv` is waiting for, consume it as unattributed background RX, and leave that COP to
  report a spurious RX timeout despite the response having arrived. `wait_for_p3_gap` gained a
  `defer_if_blocked: bool` parameter: when `true` and the gap would require an actual wait
  (`now < deadline`), it returns a new `P3GapOutcome::Deferred` immediately instead of entering its
  sleep/`poll_rx` loop — no RX drain happens at all. `dispatch_due_idle_tester_present` threads a
  `defer_gap_wait: bool` down to this call, set `true` only at the `wait_for_expected_response` call
  site and `false` at the outer-select and `handle_delay` sites (which keep waiting out the gap with
  their own unprobed `poll_rx`, as before — no correctness issue there since neither has a COP-specific
  match state in flight). A deferred send is simply left `Idle` and re-evaluated on
  `wait_for_expected_response`'s next iteration; since the P3-gap deadline is a fixed instant already
  anchored to a past TX, a few more `POLL_INTERVAL_MS` ticks — all handled by
  `wait_for_expected_response`'s own `poll_rx_and_check_match`, which continues to be the sole RX
  drain for the whole response phase — clear it without ever needing dispatch's own poll. This also
  means a deferred gap-wait does not consume any of the active COP's `CP_P2Max` response window,
  unlike an actual wait would have. `Deferred` is unreachable at the two `defer_if_blocked = false`
  call sites (`handle_send_recv`'s pre-write gate, `handle_start_comm`'s mode-0 periodic-start gate)
  and is asserted so with `unreachable!`.

`last_bus_activity` is a separate clock from ADR-060's `TxGapState`, not a repurposing of it:
`TxGapState` is TX-only and semantically "gap since a no-response send"; idle detection needs
TX+RX bus activity. The idle-mode send *updates* `TxGapState` on its way out so later
`CoptSendrecv` gap-checks see it, but *reads* `last_bus_activity` to decide when to fire.

### ~~Mode-0 periodic vs. CP_P3Func/CP_P3Phys: gate the start, document the rest as a known limitation~~

**Superseded by ADR-093 (2026-07-16): mode 0 no longer uses `PassThruStartPeriodicMsg` at all — it
is now dispatched by this service's own software poll loop, exactly like mode 1, and is therefore
fully `CP_P3Func`/`CP_P3Phys`-coordinated on every send, not just the first one.** The section below
is kept as the historical record of the limitation that decision existed to work around.

~~Once `PassThruStartPeriodicMsg` returns, the service has no visibility into individual periodic
ticks — no callback, no per-tick timestamp readback. Full bidirectional gap coordination is not
achievable through this API. The fix is deliberately partial:~~

- The *initial* `start_periodic_message` call is gated the same way a `CoptSendrecv` write is —
  `wait_for_p3_gap`-equivalent logic runs before it, and a successful start stamps `last_func_tx`/
  `last_phys_tx` — treating "start periodic" as one TX event, which is the one periodic-related
  event the service actually controls and observes.
- **This gate is non-cancellable**, unlike `CoptSendrecv`'s use of the same gap-waiting mechanism.
  By the time `handle_start_comm` reaches this gate, `run_protocol_init` has already put a real ECU
  handshake on the wire — the COP is past the point of no return the same way it already was,
  pre-ADR-083, for every step between a successful init and `CoptStartcomm` completion (baud
  write-back, hardware revert, periodic start). `wait_for_p3_gap`'s cancellation-checking exists for
  `CoptSendrecv`, where cancelling *before* an unsent write is safe; that precondition doesn't hold
  here. The gate still enforces the P3 timing delay and still surfaces a genuine `HardError` (lost
  comms), but a `CancelComPrimitive` arriving during the wait does not abort it — the COP proceeds
  to `PduCopstFinished` and `tester_present_state` is armed regardless, matching the invariant that
  once the ECU believes a session has started, the service must not leave it without a keep-alive.
- Everything after the first tick is an accepted, documented limitation, not a further fix.
  Approximating later ticks by re-stamping `last_func_tx`/`last_phys_tx` on a synthetic
  `interval_ms` cadence was considered and rejected: the DLL's real tick phase drifts and is never
  reported, so a fabricated clock would make later `CoptSendrecv` gap-waits check against a fiction
  that is neither correct nor conservative — a worse outcome than not enforcing anything. This
  follows the precedent ADR-060 already set for RC21/23 re-requests (§Out of scope): document the
  API's limitation rather than ship an enforcement that only looks real.
- This limitation is specific to mode 0 (hardware-autonomous). Mode-1 sends have no such gap: they
  are dispatched through `transmit_request` like any other software TX and are therefore fully
  P3-gap-coordinated in both directions by construction.

## Consequences

- `CP_TesterPresentTime` now actually reaches the adapter at the resolution the client configured
  it in; every preset's `2_000_000` default now means 2 seconds, not ~33 minutes. Any client
  currently compensating for the old (broken) behavior by pre-multiplying its configured interval
  will need to stop doing so.
- `CP_TesterPresentSendType=1` now has real effect. `iso_14230_3_on_iso_15765_2` and the K-line KWP
  presets that default to it get idle-triggered tester-present instead of silently falling back to
  periodic.
- `TesterPresentState` replaces `Option<PeriodicMessageId>` on `LogicalLinkState`; all three
  teardown call sites (`rpc_link.rs` ×2, `rpc_misc.rs`, `events.rs`'s `CoptStopcomm` handler) must
  match on it instead of `Option::take`.
- A new per-shared-channel `last_bus_activity` clock is threaded through `ChannelPollCtx` alongside
  ADR-060's `TxGapState` buckets — one more piece of shared, mutex-guarded per-channel state on the
  established stamp-and-release leaf pattern; it does not change the ADR-080 lock hierarchy.
- ~~Mode-0 periodic tester-present on ISO15765 still cannot be fully P3Func/P3Phys-coordinated after
  its first tick — a structural J2534 API limitation, not a defect being deferred silently. A
  client that needs strict P3-family pacing against its own periodic tester-present on ISO15765
  should prefer `CP_TesterPresentSendType=1`, which does not have this gap.~~ **Superseded by
  ADR-093: mode 0 is now software-dispatched and fully P3-coordinated on every send.**
- The mode-0 P3-gate is a non-cancellable wait: a `CancelComPrimitive` racing `CoptStartcomm` after
  ECU init has already succeeded no longer aborts the COP once it reaches this point — it always
  completes to `PduCopstFinished` with tester-present armed, consistent with the pre-existing
  (pre-ADR-083) invariant that nothing after a successful `run_protocol_init` was cancellable. This
  was a design gap in an earlier draft of this ADR, caught in review, not a change to prior
  behavior for `CoptStartcomm`. (Still true post-ADR-093 — the unified branch keeps this gate.)
- `TesterPresentState::Idle` carries its own `armed_at`; arming a CLL never mutates the shared
  `last_bus_activity` clock. A CLL's idle window is measured from `max(last_bus_activity, armed_at)`
  so that one CLL's `CoptStartcomm` cannot silently extend a sibling CLL's already-counting idle
  timer on a shared physical channel. Also caught in review, not a change to prior behavior.
- `dispatch_due_idle_tester_present` re-validates, immediately before sending, that a snapshotted CLL
  still exists, is still connected on the same physical channel, and is still the same arm generation
  (`armed_at` unchanged) — not just "still present" or "still `Idle`" — so a `DisconnectComLogicalLink`/
  `DestroyComLogicalLink`/`CoptStopcomm`/reconnect-and-rearm racing the send does not result in a
  stale frame going out for a torn-down CLL or onto the wrong channel. `PARAM_TESTER_PRESENT_INTERVAL_US`'s
  mode-0 counterpart re-check uses the analogous `channel_id`-match condition, not bare existence.
- **N same-interval mode-1 CLLs sharing a physical channel now emit N keep-alives per interval
  (P3-gap-spaced), not one.** An earlier draft let a CLL's own idle-mode send re-stamp the shared
  `last_bus_activity` clock, which (via `max(last_bus_activity, armed_at)`) permanently
  resynchronized every same-interval sibling's due instant to the winner's — and since `HashMap`
  iteration order is stable within one process run, the same CLL won every batch, starving every
  sibling of tester-present indefinitely (caught by Codex review, not by the original design or by
  `edge-case-hunter`). Each mode-1 CLL now tracks its own `last_fired` and is excluded from the
  shared clock, so redundant channel-wide traffic increases (by design — each ECU session needs its
  own keep-alive) but no CLL can be starved by a sibling's successful sends. External bus activity
  (RX, other CLLs' `CoptSendrecv`, mode-0 periodic starts) still defers every mode-1 CLL equally.
- **The idle-mode send itself is validate-then-await-then-write; a teardown landing in the residual
  window still emits one final keepalive frame — an accepted limitation, not a further fix.**
  `dispatch_due_idle_tester_present`'s `still_due` re-check drops `logical_links` before
  `transmit_request`'s own `ctx.api.lock().await`; a `DisconnectComLogicalLink`/
  `DestroyComLogicalLink`/`CoptStopcomm`/`CLEAR_PERIODIC_MSGS` clearing `tester_present_state` in
  that residual gap does not stop the write that's already committed — only the post-send
  `last_fired` update is guarded against it (caught by Codex review). Closing this window to zero
  would mean holding `logical_links` across the hardware write (freezing RX fan-out and every
  sibling CLL's own dispatch across FFI I/O, for one keepalive frame) or nesting a second lock under
  the poll task, reopening the ADR-080 hierarchy audit for no proportionate benefit — the same
  disproportionate-cost reasoning already applied to the mode-0 P3-gate limitation above. The blast
  radius is bounded and inert: idle-mode teardown is state-only (no hardware resource to leak, unlike
  an orphaned `PassThruStartPeriodicMsg`), the frame's addressing was baked into `framed_data` at arm
  time so it cannot be misrouted regardless of which sibling CLL keeps the physical channel open, and
  the `armed_at` arm-identity guard rejects a reused handle — so at most one extra, correctly-addressed
  keepalive frame reaches the wire slightly after the client disarmed or tore down that CLL.
- **K-line init traffic (5-baud/fast-init) now stamps `last_bus_activity` too.** `run_protocol_init`
  puts real traffic on the shared physical channel but calls the J2534 init IOCTLs directly rather
  than going through `transmit_request`/`write_can_frame`, so none of the existing stamping paths
  saw it — a mode-1 sibling CLL sharing that channel could fire its own keepalive immediately after
  another CLL's init instead of being deferred by that real activity (caught by Codex review). A
  successful init now stamps `last_bus_activity` before `CoptStartcomm` proceeds to its own
  tester-present setup, regardless of which `CP_TesterPresentSendType` this particular CLL uses —
  the gap was about the init's own traffic being invisible to *other* CLLs, not about this CLL's own
  mode.
- **`CP_TesterPresentSendType` is validated at resolve time; only `0` or `1` is accepted.**
  `SetComParam` does not range-check this value, so a misconfigured value like `2` previously fell
  through the `send_type == 0` check straight into the mode-1 arm and silently armed the software
  idle sender instead of being rejected (caught by Codex review) — a typo would have changed wire
  behavior instead of failing loudly. `resolve_tester_present` now rejects any other value the same
  way it already rejects a `CP_TesterPresentTime` that rounds to 0 ms: a synchronous
  `StartComPrimitive INVALID_ARGUMENT`, per ADR-067's resolve-time-failure precedent.
- **Step 3's channel-identity re-check and its write-back happen under one lock acquisition, not
  two.** An earlier draft checked `still_on_this_channel` and dropped `logical_links` before
  re-acquiring it to write `comm_started`/`tester_present_state` — a narrow but real gap, since
  tokio's cooperative scheduler can force a yield at any `.await`, including an uncontended
  `Mutex::lock`, letting a `DisconnectComLogicalLink` land between the check and the write (caught
  by Codex review). Unlike the two other teardown-race gaps this ADR documents as accepted
  limitations (the mode-0 pre-hardware-call gate, and `dispatch_due_idle_tester_present`'s own
  send), this one has no hardware I/O between the check and the write — both only touch
  `logical_links` — so folding them into a single critical section closes the window entirely
  rather than merely narrowing it. The teardown/orphan-cleanup path (stopping an already-armed
  mode-0 `Periodic(id)`, emitting `PduCopstCancelled`) still runs outside the lock, as it must for
  its own `.await`s.
- **Update (ADR-086):** the `still_on_this_channel` guard introduced here (mode-0 gate and Step 3's
  write-back; the mode-1 gate ADR-084 later added reuses the same pattern) checked `channel_id`
  equality alone, which is insufficient on a *shared* physical channel: a disconnect-then-reconnect
  of the same `cll_handle` rejoins the same channel with the identical `ChannelId`. ADR-086 extends
  all three of `handle_start_comm`'s guard sites (and `handle_stop_comm`'s two, from ADR-085's
  round-7 amendment) with an additional `connect_generation` check that closes that gap.
