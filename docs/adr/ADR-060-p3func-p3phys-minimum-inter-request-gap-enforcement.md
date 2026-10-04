# ADR-060: CP_P3Func/CP_P3Phys Minimum Inter-Request Gap Enforcement

**Date:** 2026-07-04
**Status:** Accepted (the RC21/23 request-time implementation slip this ADR's own "Out of
             scope" section left uncovered — an explicit `CP_RC2xRequestTime = 0` coerced to
             a hardcoded 25 ms instead of `CP_P3Min` — is fixed by ADR-125; this ADR's own
             "RC21/23 re-requests aren't gap-*tracked*" decision is unchanged)
**Affects:** `j2534-0404-service/src/service.rs` (`SharedChannel`, `TxItem::SendRecv`,
             `TxGapState`, `ComParamSet::p3_func_gap_ms`/`p3_phys_gap_ms`),
             `j2534-0404-service/src/service/rpc_link.rs` (`spawn_new_shared_channel`),
             `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`),
             `j2534-0404-service/src/service/events.rs` (`handle_send_recv`,
             `wait_for_p3_gap`, `spawn_channel_poll_task`, `poll_channel_events`,
             `dispatch_tx_item`)

## Context

ADR-056 made `CP_P3Func`/`CP_P3Phys` reachable via `SetComParam`/`GetComParam`
on CAN/ISO15765 channels but left them stored-only — no inter-request timing
was actually enforced. The caller specified the intended semantics directly,
summarised here in our own words:

- `CP_P3Func`: before a functional-address request goes out, wait at least
  this long after the previous functional-address request on the shared bus,
  if either of the two (the request about to be sent, or the previous one)
  expects no response (`receiveCycle = 0`).
- `CP_P3Phys`: before a physical-address request goes out, wait at least this
  long after the previous physical-address request on the shared bus, if that
  previous request expected no response (`receiveCycle = 0`).

So the gap is enforced for `CP_P3Func` when *either* the upcoming functional
send *or* the previous one requires no response. `CP_P3Phys` has only the
second condition: the gap is enforced only when the *previous* physical send
required no response; there is no symmetric "upcoming" condition.
`NumReceiveCycles == 0` is exactly ADR-058's
"no response required" (not ADR-053's superseded "wait for one" reading),
which is what made this condition well-defined to implement at all.

This asymmetry is not arbitrary: a physical (point-to-point) request that
itself waits for a response naturally paces the bus regardless of what
preceded it, so only the previous send's behavior needs checking. A
functional (broadcast) request is different — even one that will wait for
responses doesn't retroactively protect against having been sent too soon
after the last broadcast, and one that won't wait for a response either
leaves nothing to pace the *next* message, so either side of the pair needs
checking.

Both ComParams are µs-denominated like every other D-PDU timing ComParam
(`CP_P2Max`, the RC-family timeouts), but unlike those, `0`/absent is a
legitimate "no gap required" value here — several presets seed it as `0`
intentionally (ADR-056) — not a "use some fallback default" signal.

## Decision

### Per-shared-channel gap state, not per-CLL

The gap is a property of the physical bus, not of any one `ComLogicalLink`
(the semantics above refer to the previous send *on the shared bus*). Two new `Arc<Mutex<Option<TxGapState>>>` values
(`last_func_tx`, `last_phys_tx`) are created once per physical channel in
`spawn_new_shared_channel`, alongside the existing `executing_cop` (the
established pattern for shared per-channel state), and moved into
`spawn_channel_poll_task`/`poll_channel_events` → threaded through
`dispatch_tx_item` → `handle_send_recv`. Unlike `executing_cop`, nothing
reads these back through `SharedChannel` itself (no `GetStatus`-style
accessor exists for them), so they are owned solely by the poll task rather
than also stored on the `SharedChannel` struct.

```rust
pub(super) struct TxGapState {
    pub(super) at: tokio::time::Instant,
    pub(super) no_response_required: bool,
}
```

Each entry records both *when* the last send of that addressing happened and
*whether* it required a response — the second field is what the trigger
condition above actually checks.

### Addressing resolved once, threaded through `TxItem`

`can_addressing`/`tx_header::resolve_can_addressing` was already computed in
`rpc_primitive.rs` at RPC time (ADR-050/054/055) but never threaded past
that point. `TxItem::SendRecv` gains `can_functional: Option<bool>` —
`Some(true)` functionally addressed, `Some(false)` physically addressed,
`None` when gap enforcement does not apply (non-CAN-family protocol, or
addressing could not be resolved):

```rust
let can_functional = link.protocol.is_can_family()
    .then(|| can_addressing.map(|a| a.functional))
    .flatten();
```

Gating on `is_can_family()` explicitly (rather than relying on
`resolve_can_addressing` happening to return `None` for non-CAN protocols)
keeps the CAN-only scope of these two ComParams as visible here as it is in
`is_can_param`'s allowlist (ADR-056) — CP_P3Func/P3Phys have no meaning for
KWP, which resolves the same D-PDU names to the native `P3_MIN` hardware
timer instead. `can_functional` is carried forward into the `TxItem` a
cyclic `CoptSendrecv`'s continuation rebuilds each cycle (ADR-053), so a
cyclic send's later cycles are gap-checked too.

### The wait itself: `wait_for_p3_gap`

Inserted in `handle_send_recv` right before the write, only on cycles that
actually transmit (`send_cycles_remaining != 0`, ADR-059) and only once
`temp_param_update`'s hardware apply has already succeeded:

```rust
let Some(functional) = can_functional else { return P3GapOutcome::Ready; };
let gap_ms = /* Active CP_P3Func or CP_P3Phys, µs → ms, this CLL's own value */;
if gap_ms == 0 { return P3GapOutcome::Ready; }
let last_tx = if functional { last_func_tx } else { last_phys_tx };
let this_requires_no_response = functional && num_receive_cycles == 0;
let deadline = match *last_tx.lock().await {
    Some(prev) if prev.no_response_required || this_requires_no_response =>
        Some(prev.at + Duration::from_millis(gap_ms as u64)),
    _ => None,
};
```

`this_requires_no_response` is gated on `functional` — this is the one place
the asymmetry from the Context section actually has to be encoded in code,
and a first draft of this ADR's implementation got it wrong (applied the
"upcoming" check to both buckets uniformly), caught only by the
`phys_gap_not_enforced_when_previous_phys_send_required_a_response` test
(see Consequences).

When a deadline applies, the wait polls RX and checks cancellation in
`POLL_INTERVAL_MS`-sized chunks, mirroring `handle_delay`/ADR-003, so a
multi-hundred-millisecond gap never starves RX intake for other CLLs sharing
the channel. `Cancelled`/`HardError` outcomes feed into the same
`channel_lost`/`cancelled` flags `handle_send_recv` already had, reusing its
existing status-emission logic rather than duplicating it.

On a successful write, the relevant bucket (`last_func_tx` or
`last_phys_tx`, by this send's own `can_functional`) is updated with the new
`TxGapState { at: now, no_response_required: num_receive_cycles == 0 }` —
this is what the *next* send in that bucket will check.

### Out of scope

RC21/23 pending-response re-requests (ADR-018) re-transmit the original
request from inside `wait_for_expected_response` mid-receive-phase. These
re-transmissions are not gap-checked or recorded here — there was no
original request's addressing context threaded that deep, and re-requests
are inherently responses to an ECU that's already mid-conversation with a
specific request, making the "no response required" condition largely moot
for them in practice. Left as a known limitation rather than expanding this
ADR's scope.

## Consequences

- A functionally- or physically-addressed `CoptSendrecv` on CAN/ISO15765 may
  now be delayed before its `PassThruWriteMsgs` (or software-ISO-TP send)
  when `CP_P3Func`/`CP_P3Phys` is non-zero and the trigger condition holds.
  Every existing preset seeds these at either `0` (no behavior change) or
  `150_000` µs (`iso_15765_3_on_iso_15765_2`, ADR-056) — a real change for
  clients using that preset with fire-and-forget (`NumReceiveCycles == 0`)
  sends.
- `tests/grpc_mock/p3_gap.rs` (new): gap enforced when the previous physical
  send required no response; not enforced when it did; gap enforced when
  the *upcoming* functional send requires no response even though the
  *previous* one didn't (the asymmetry's positive case); not enforced when
  both functional sends require a response; and gap state is scoped to the
  shared physical channel, not to one CLL (two CLLs on the same
  protocol+baud channel gap-block each other). All five were verified to
  fail against the pre-fix code and pass against the fix. Writing the
  cross-CLL test surfaced (and required accounting for, not fixing) an
  existing, separately-documented behavior: a CLL that joins an
  already-open shared channel keeps a default (zero) Active ComParam set
  until it issues `CoptUpdateparam` — this is not new, but this ADR's test
  is the first to depend on it for a *service-level* (not hardware) param.
- RC21/23 re-request retransmissions are not gap-tracked (see "Out of
  scope") — a known, documented limitation, not a defect being deferred
  silently.
- ADR-056's "no inter-request minimum-gap enforcement exists" consequence
  note is amended to point here.
- ADR-125 fixes a separate implementation slip this ADR's "Out of scope"
  never covered: `RcHandlingConfig::from_params` was coercing an explicit
  `CP_RC21RequestTime`/`CP_RC23RequestTime = 0` to a hardcoded 25 ms instead
  of the spec-mandated `Max(CP_P3Min, CP_RC2xRequestTime)` (ISO 22900-2
  Annex I.1.4.3). This ADR's own decision — RC21/23 re-requests are not
  gap-*tracked* against `CP_P3Func`/`CP_P3Phys` state — is unaffected.
